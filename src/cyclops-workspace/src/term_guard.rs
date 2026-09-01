//! Terminal guard: raw mode, alternate screen, panic-safe restore.

use std::io::{self, Read as _, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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

const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";

/// Present one Ratatui frame atomically on terminals that implement DEC
/// synchronized updates.
///
/// Ratatui writes a frame through several `write` calls and finishes it with
/// one `flush`. A fast stream of pane echo, such as ordinary typing, can let
/// a terminal paint between those writes even though Cyclops never asked for
/// a full repaint. Opening the synchronized update on the first byte and
/// closing it at the frame flush keeps the previous complete frame visible
/// until the next complete frame is ready. Unsupported terminals ignore both
/// markers.
pub(crate) struct SynchronizedWriter<W: Write> {
    inner: W,
    open: bool,
}

impl<W: Write> SynchronizedWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner, open: false }
    }

    fn close_frame(&mut self) -> io::Result<()> {
        if self.open {
            self.inner.write_all(END_SYNCHRONIZED_UPDATE)?;
            self.open = false;
        }
        self.inner.flush()
    }
}

impl<W: Write> Write for SynchronizedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if !self.open {
            self.inner.write_all(BEGIN_SYNCHRONIZED_UPDATE)?;
            self.open = true;
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.close_frame()
    }
}

impl<W: Write> Drop for SynchronizedWriter<W> {
    fn drop(&mut self) {
        let _ = self.close_frame();
    }
}

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

/// Paint the host terminal's own default foreground and background with
/// the theme's pane ink and chrome ground (OSC 10 and OSC 11).
///
/// This is the only way to reach the band around the grid. A terminal
/// reserves a few pixels of window padding and fills them with its default
/// background, and no amount of cell painting touches it: the workspace
/// already fills every cell it is given, so the strip an operator sees at
/// the edges is outside the grid entirely. OSC 11 changes the color that
/// strip is filled with. OSC 10 keeps unstyled host text readable on that
/// ground, including a shell revealed or opened while a light Cyclops theme
/// is active. That is why the pair lives here rather than in a painter.
///
/// Lives beside the guard for the same reason `apply_cursor_style` does:
/// it changes terminal state that outlives the process unless something
/// undoes it, and `restore` is what undoes it, on every exit path
/// including a panic. Leaving a shell with the workspace's palette would
/// be a worse bug than the padding it fixes.
///
/// Unsupported terminals ignore the sequences, so there is nothing to
/// detect and no fallback to write.
pub fn apply_window_palette(fg: (u8, u8, u8), bg: (u8, u8, u8)) {
    let mut out = io::stdout();
    write_window_palette(&mut out, fg, bg);
    let _ = out.flush();
}

fn write_window_palette(out: &mut impl Write, fg: (u8, u8, u8), bg: (u8, u8, u8)) {
    let (fr, fg, fb) = fg;
    let (br, bg, bb) = bg;
    // ST rather than BEL: both terminate an OSC, and a stray BEL in a
    // terminal that did not understand the sequence rings the bell.
    let _ = write!(
        out,
        "\x1b]10;#{fr:02x}{fg:02x}{fb:02x}\x1b\\\x1b]11;#{br:02x}{bg:02x}{bb:02x}\x1b\\"
    );
}

/// One terminal color as 8-bit RGB.
type Rgb = (u8, u8, u8);

/// The terminal's own default foreground and background when an operator
/// explicitly configured both values. An unset cell means there is no
/// override, so restoration uses the terminal's OSC 110/111 reset request.
static ORIGINAL_PALETTE: OnceLock<Option<(Rgb, Rgb)>> = OnceLock::new();

fn original_palette() -> Option<(Rgb, Rgb)> {
    ORIGINAL_PALETTE.get().copied().flatten()
}

/// Load an optional exact terminal palette from the operator's `[workspace]`
/// settings. The UI never writes these keys and never asks the terminal to
/// report them, so input still has exactly one owner.
///
/// Both values are required. A partial or malformed pair deliberately falls
/// back to OSC 110/111 rather than guessing the terminal's defaults.
pub fn configure_default_palette(home: &Path) {
    let _ = ORIGINAL_PALETTE.set(configured_default_palette(home));
}

fn configured_default_palette(home: &Path) -> Option<(Rgb, Rgb)> {
    let root = cyclops_state::StateRoot::open_existing(home).ok()??;
    let mut file = root.open_read(Path::new("config.toml")).ok()??;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    let workspace = table.get("workspace")?.as_table()?;
    let fg = workspace
        .get("terminal_default_fg")?
        .as_str()
        .and_then(parse_hex_color)?;
    let bg = workspace
        .get("terminal_default_bg")?
        .as_str()
        .and_then(parse_hex_color)?;
    Some((fg, bg))
}

fn parse_hex_color(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

/// Hand the terminal's default foreground and background back.
///
/// When the operator configured exact defaults, set those back with OSC
/// 10/11. This supports terminals that ignore OSC 110/111. Otherwise ask
/// the terminal to reset to its own defaults with OSC 110/111.
fn reset_window_palette(out: &mut impl Write) {
    write_reset(out, original_palette());
}

fn write_reset(out: &mut impl Write, original: Option<(Rgb, Rgb)>) {
    match original {
        Some((fg, bg)) => write_window_palette(out, fg, bg),
        None => {
            let _ = write!(out, "\x1b]110\x1b\\\x1b]111\x1b\\");
        }
    }
}

/// Hand the foreground and background back while the workspace keeps running.
///
/// Focus left the workspace's tab. The operator is now looking at their
/// own shell or another program in the same terminal window, and it
/// should wear the terminal's own defaults, not the theme's. The same
/// escape `restore` sends on exit; focus return reapplies the theme
/// through the ordinary draw path.
pub fn yield_window_palette() {
    let mut out = io::stdout();
    reset_window_palette(&mut out);
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
        // Focus reporting drives the host palette: the theme's ink and
        // ground are only handed to the terminal while the workspace is the
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
        // revealed already wearing its own foreground and background rather
        // than flashing the workspace's palette for a frame.
        reset_window_palette(&mut out);
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

    /// The four escapes are the exact pairs a terminal needs to change its
    /// default foreground/background and hand both back.
    ///
    /// Pinned as bytes because this is the one thing the workspace writes
    /// that outlives the process. A malformed set is a cosmetic bug; a
    /// malformed or missing reset leaves the operator's shell wearing the
    /// workspace's background after cyclops exits, which is the failure
    /// that matters. ST terminates rather than BEL so a terminal that does
    /// not understand the sequence stays silent instead of ringing.
    #[test]
    fn the_palette_escapes_set_and_hand_back() {
        let mut out: Vec<u8> = Vec::new();
        reset_window_palette(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]110\x1b\\\x1b]111\x1b\\",
            "OSC 110 and 111 with ST terminators return both defaults"
        );

        let mut out: Vec<u8> = Vec::new();
        write_window_palette(&mut out, (0x3a, 0x2b, 0x26), (0xfa, 0xf6, 0xe6));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]10;#3a2b26\x1b\\\x1b]11;#faf6e6\x1b\\",
            "foreground and background channels are lowercase, zero-padded hex"
        );
    }

    /// An operator-configured default is handed back with an OSC set, which
    /// supports terminals that ignore the OSC 110/111 reset request.
    #[test]
    fn a_known_default_is_handed_back_by_setting_it_not_by_asking_for_a_reset() {
        let mut out: Vec<u8> = Vec::new();
        write_reset(&mut out, Some(((0x3a, 0x2b, 0x26), (0x00, 0x00, 0x00))));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]10;#3a2b26\x1b\\\x1b]11;#000000\x1b\\",
            "an operator-provided default must be set back exactly"
        );
    }

    /// With no valid operator override, ask the terminal to reset itself.
    #[test]
    fn an_unknown_default_falls_back_to_the_reset_request() {
        let mut out: Vec<u8> = Vec::new();
        write_reset(&mut out, None);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]110\x1b\\\x1b]111\x1b\\",
            "no operator default means ask the terminal to reset to its own"
        );
    }

    /// The operator must supply one complete, strict `#rrggbb` pair. A
    /// partial or malformed configuration must not guess a terminal palette.
    #[test]
    fn configured_default_palette_requires_a_valid_pair() {
        use std::path::Path;

        let home = cyclops_proto::scratch::scratch_dir("workspace-terminal-default-palette");
        let _ = std::fs::remove_dir_all(&home);
        let root = cyclops_state::StateRoot::open_or_create(&home).expect("safe home");

        root.replace_file(
            Path::new("config.toml"),
            b"[workspace]\nterminal_default_fg = \"#3a2b26\"\nterminal_default_bg = \"#000000\"\n",
        )
        .expect("valid pair");
        assert_eq!(
            configured_default_palette(&home),
            Some(((0x3a, 0x2b, 0x26), (0x00, 0x00, 0x00)))
        );

        root.replace_file(
            Path::new("config.toml"),
            b"[workspace]\nterminal_default_fg = \"#3a2b2\"\nterminal_default_bg = \"#000000\"\n",
        )
        .expect("malformed pair");
        assert_eq!(configured_default_palette(&home), None);

        root.replace_file(
            Path::new("config.toml"),
            b"[workspace]\nterminal_default_fg = \"#3a2b26\"\n",
        )
        .expect("partial pair");
        assert_eq!(configured_default_palette(&home), None);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Terminal input has a single owner: the application event thread.
    /// This guard only writes terminal state and reads configuration files.
    #[test]
    fn the_terminal_guard_never_reads_terminal_input() {
        let source = include_str!("term_guard.rs");
        for forbidden in [
            ["std::io", "::stdin"].concat(),
            ["libc", "::read"].concat(),
            ["event", "::read()"].concat(),
        ] {
            assert!(
                !source.contains(&forbidden),
                "terminal palette restoration must not read input through {forbidden}"
            );
        }
    }

    #[test]
    fn each_flushed_frame_is_one_synchronized_terminal_update() {
        let mut out = SynchronizedWriter::new(Vec::new());

        out.write_all(b"first ").unwrap();
        out.write_all(b"frame").unwrap();
        out.flush().unwrap();
        out.write_all(b"second").unwrap();
        out.flush().unwrap();

        assert_eq!(
            out.inner, b"\x1b[?2026hfirst frame\x1b[?2026l\x1b[?2026hsecond\x1b[?2026l",
            "typing frames must never be exposed between Ratatui writes"
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
