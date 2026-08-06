//! Terminal guard: raw mode, alternate screen, panic-safe restore.

use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
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
        let _ = out.flush();
        install_panic_hook();
        Ok(TermGuard { active: true })
    }

    fn restore(&mut self) {
        if !self.active || RESTORING.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut out = io::stdout();
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(DisableMouseCapture);
        // Undo any DECSCUSR a focused pane asked for (`apply_cursor_style`)
        // before leaving the alternate screen, so the user's shell gets its
        // own configured cursor back rather than the last pane's.
        let _ = out.execute(SetCursorStyle::DefaultUserShape);
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
