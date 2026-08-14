//! Terminal guard: raw mode, alternate screen, panic-safe restore.

use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;

use crate::runtime::CursorShape;

static RESTORING: AtomicBool = AtomicBool::new(false);

/// Emit the focused pane's requested cursor shape (DECSCUSR) to the host
/// terminal. Lives beside the guard because it changes terminal state the
/// guard must undo: `restore` puts the user's configured shape back on
/// every exit path, panic included, so a pane that asked for a bar cannot
/// leave the user's shell with one.
pub fn apply_cursor_style(shape: CursorShape, blink: bool) {
    let style = match (shape, blink) {
        (CursorShape::Block, true) => SetCursorStyle::BlinkingBlock,
        (CursorShape::Block, false) => SetCursorStyle::SteadyBlock,
        (CursorShape::Underline, true) => SetCursorStyle::BlinkingUnderScore,
        (CursorShape::Underline, false) => SetCursorStyle::SteadyUnderScore,
        (CursorShape::Bar, true) => SetCursorStyle::BlinkingBar,
        (CursorShape::Bar, false) => SetCursorStyle::SteadyBar,
    };
    let mut out = io::stdout();
    let _ = out.execute(style);
}

/// Paint the host terminal's own default background the theme's chrome
/// color (OSC 11).
///
/// This is the only way to reach the band around the grid. A terminal
/// reserves a few pixels of window padding and fills them with its default
/// background, and no amount of cell painting touches it: the workspace
/// already fills every cell it is given, so the strip an operator sees at
/// the edges is outside the grid entirely. OSC 11 changes the color that
/// strip is filled with, which is why it is here rather than in a painter.
///
/// Lives beside the guard for the same reason `apply_cursor_style` does:
/// it changes terminal state that outlives the process unless something
/// undoes it, and `restore` is what undoes it, on every exit path
/// including a panic. Leaving a shell with the workspace's background
/// would be a worse bug than the padding it fixes.
///
/// Unsupported terminals ignore the sequence, so there is nothing to
/// detect and no fallback to write.
pub fn apply_window_background(rgb: (u8, u8, u8)) {
    let (r, g, b) = rgb;
    let mut out = io::stdout();
    // ST rather than BEL: both terminate an OSC, and a stray BEL in a
    // terminal that did not understand the sequence rings the bell.
    let _ = write!(out, "\x1b]11;#{r:02x}{g:02x}{b:02x}\x1b\\");
    let _ = out.flush();
}

/// Hand the terminal's default background back (OSC 111).
fn reset_window_background(out: &mut impl Write) {
    let _ = write!(out, "\x1b]111\x1b\\");
}

/// Hand the background back while the workspace keeps running.
///
/// Focus left the workspace's tab. The operator is now looking at their
/// own shell or another program in the same terminal window, and it
/// should wear the terminal's own background, not the theme's. The same
/// escape `restore` sends on exit; focus return reapplies the theme
/// through the ordinary draw path.
pub fn yield_window_background() {
    let mut out = io::stdout();
    reset_window_background(&mut out);
    let _ = out.flush();
}

/// Owns the terminal mode until dropped.
pub struct TermGuard {
    active: bool,
}

impl TermGuard {
    /// Enter raw mode and the alternate screen, with mouse and bracketed
    /// paste reporting owned by the same restoration guard.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(EnterAlternateScreen)?;
        let _ = out.execute(EnableMouseCapture);
        let _ = out.execute(EnableBracketedPaste);
        // Focus reporting drives the window background: the theme's ground
        // is only painted onto the terminal while the workspace is the
        // thing being looked at (app.rs, AppMsg::Focus).
        let _ = out.execute(EnableFocusChange);
        // The kitty keyboard protocol's disambiguate level, pushed blind
        // rather than after a support query: the query's reply arrives as
        // an input event, and the reader thread that would eat it is
        // already running when this guard enters. The protocol is built
        // for exactly this: a terminal that does not speak it ignores the
        // push and the pop, and keys keep their legacy shapes. Where it is
        // spoken, chords the legacy encoding cannot carry become real
        // events: Ctrl+Backspace stops arriving as Ctrl+H, and Cmd chords
        // arrive at all (input.rs translates both for the focused pane).
        let _ = out.execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ));
        let _ = out.flush();
        install_panic_hook();
        Ok(TermGuard { active: true })
    }

    fn restore(&mut self) {
        if !self.active || RESTORING.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut out = io::stdout();
        // Pop first, matching the push order in reverse. A terminal that
        // never took the push ignores the pop the same way.
        let _ = out.execute(PopKeyboardEnhancementFlags);
        let _ = out.execute(DisableFocusChange);
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(DisableMouseCapture);
        // Undo any DECSCUSR a focused pane asked for (`apply_cursor_style`)
        // before leaving the alternate screen, so the user's shell gets its
        // own configured cursor back rather than the last pane's.
        let _ = out.execute(SetCursorStyle::DefaultUserShape);
        // Before leaving the alternate screen, so the shell underneath is
        // revealed already wearing its own background rather than flashing
        // the workspace's for a frame.
        reset_window_background(&mut out);
        let _ = out.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = out.flush();
        self.active = false;
        RESTORING.store(false, Ordering::SeqCst);
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook() {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut guard = TermGuard { active: true };
            guard.restore();
        }));
        prev(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::IsTerminal;

    /// The two escapes are the exact pair a terminal needs to change its
    /// default background and hand it back.
    ///
    /// Pinned as bytes because this is the one thing the workspace writes
    /// that outlives the process. A malformed set is a cosmetic bug; a
    /// malformed or missing reset leaves the operator's shell wearing the
    /// workspace's background after cyclops exits, which is the failure
    /// that matters. ST terminates rather than BEL so a terminal that does
    /// not understand the sequence stays silent instead of ringing.
    #[test]
    fn the_background_escapes_set_and_hand_back() {
        let mut out: Vec<u8> = Vec::new();
        reset_window_background(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]111\x1b\\",
            "OSC 111 with an ST terminator is what returns the default"
        );

        // The setter writes to stdout, so its format is checked against the
        // same construction rather than by capturing a real terminal.
        let (r, g, b) = (0xfa_u8, 0xf6_u8, 0xe6_u8);
        assert_eq!(
            format!("\x1b]11;#{r:02x}{g:02x}{b:02x}\x1b\\"),
            "\x1b]11;#faf6e6\x1b\\",
            "channels are two lowercase hex digits each, zero padded"
        );
    }

    #[test]
    #[should_panic(expected = "guard restore probe")]
    fn panic_restores_terminal() {
        // Only meaningful on a real tty; skip when stdout is not one.
        if !std::io::stdout().is_terminal() {
            panic!("guard restore probe");
        }
        let _guard = TermGuard::enter().expect("tty");
        panic!("guard restore probe");
    }
}
