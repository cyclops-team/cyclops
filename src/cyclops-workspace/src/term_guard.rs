//! Terminal guard: raw mode, alternate screen, panic-safe restore.

use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

/// The terminal's own default foreground and background, learned once at
/// startup by asking the terminal (`capture_default_palette`).
///
/// `Some(None)` means the query ran and the terminal did not answer (no
/// tty, or a terminal that does not report its colors); an unset cell means
/// capture never ran, which is the case under test. Both resolve to "not
/// known" and fall back to the OSC 110/111 reset request.
static ORIGINAL_PALETTE: OnceLock<Option<(Rgb, Rgb)>> = OnceLock::new();

fn original_palette() -> Option<(Rgb, Rgb)> {
    ORIGINAL_PALETTE.get().copied().flatten()
}

/// Ask the terminal for its own default foreground and background and
/// remember them, so exit and focus-loss can hand back the exact colors
/// the terminal started with.
///
/// This is the fix for a terminal that honors an OSC 10/11 *set* but
/// ignores the OSC 110/111 *reset* — Apple Terminal is the common one. On
/// such a terminal the workspace's themed ground would otherwise stay on
/// the shell after cyclops leaves, because the only reset it was ever sent
/// is the one that terminal drops on the floor. Handing back the captured
/// colors is a plain OSC set, which every terminal that showed the theme
/// ground in the first place obeys.
///
/// Runs once, and must run before the input reader thread starts: the
/// terminal answers the query as terminal input, so whoever reads stdin
/// first reads the answer. A terminal that never answers costs one bounded
/// wait here and nothing afterward.
pub fn capture_default_palette() {
    let _ = ORIGINAL_PALETTE.set(query_default_palette());
}

/// Restore stdin's termios on drop, so no early return can leave the
/// terminal in the transient raw mode the query needs.
struct TermiosRestore(libc::termios);

impl Drop for TermiosRestore {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

fn query_default_palette() -> Option<(Rgb, Rgb)> {
    // Only a real terminal answers, and only over stdin.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return None;
    }
    let orig = unsafe {
        let mut t = MaybeUninit::<libc::termios>::uninit();
        if libc::tcgetattr(libc::STDIN_FILENO, t.as_mut_ptr()) != 0 {
            return None;
        }
        t.assume_init()
    };
    let _restore = TermiosRestore(orig);
    let mut raw = orig;
    // Non-canonical, no echo, timed reads: the reply has no newline, so a
    // cooked read would block until Enter, and echo would print the reply.
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 1; // 100ms per read
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    // Foreground (OSC 10) and background (OSC 11), then a primary
    // device-attributes request (DA1, `ESC [ c`) as a fence. Every terminal
    // answers DA1, and its answer cannot precede the color answers it was
    // sent after, so the read stops the instant DA1 lands rather than
    // guessing a timeout and risking a real keystroke typed at launch.
    {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c");
        let _ = out.flush();
    }
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut buf: Vec<u8> = Vec::with_capacity(128);
    let mut chunk = [0u8; 256];
    while Instant::now() < deadline {
        let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
            if da1_answered(&buf) {
                break;
            }
        } else if n == 0 {
            // VTIME elapsed with nothing. A terminal that answers has begun
            // by now; an empty first read means it never will.
            if buf.is_empty() {
                break;
            }
        } else {
            break; // read error
        }
    }
    match parse_osc_colors(&buf) {
        (Some(fg), Some(bg)) => Some((fg, bg)),
        _ => None,
    }
}

/// Whether a DA1 reply (`ESC [ ... c`) is present: a CSI closed by `c`. The
/// color replies are OSC sequences closed by BEL or ST, so the only `c`
/// that closes a CSI here is DA1's.
fn da1_answered(buf: &[u8]) -> bool {
    let Some(start) = find_subslice(buf, b"\x1b[") else {
        return false;
    };
    buf[start + 2..].contains(&b'c')
}

fn parse_osc_colors(buf: &[u8]) -> (Option<Rgb>, Option<Rgb>) {
    (
        find_osc_color(buf, b"\x1b]10;"),
        find_osc_color(buf, b"\x1b]11;"),
    )
}

fn find_osc_color(buf: &[u8], prefix: &[u8]) -> Option<Rgb> {
    let pos = find_subslice(buf, prefix)?;
    let rest = &buf[pos + prefix.len()..];
    // OSC terminator: BEL, or ESC of the two-byte ST.
    let end = rest.iter().position(|&b| b == 0x07 || b == 0x1b)?;
    parse_color_payload(&rest[..end])
}

fn parse_color_payload(payload: &[u8]) -> Option<Rgb> {
    let s = std::str::from_utf8(payload).ok()?.trim();
    if let Some(hex) = s.strip_prefix("rgb:") {
        let mut it = hex.split('/');
        let r = scale_component(it.next()?)?;
        let g = scale_component(it.next()?)?;
        let b = scale_component(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            return Some((
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ));
        }
    }
    None
}

/// One `rgb:` component, 1..=4 hex digits, scaled to 8 bits. `xterm`
/// reports 16-bit (`3a3a`); a shorter width is scaled across its own range
/// so `f` is full intensity, not near-black.
fn scale_component(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(s, 16).ok()?;
    let max = (1u32 << (4 * s.len())) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Hand the terminal's default foreground and background back.
///
/// When the terminal told us its defaults at startup
/// (`capture_default_palette`), set exactly those back: Apple Terminal
/// honors an OSC 10/11 set but ignores the OSC 110/111 reset, so a bare
/// reset leaves the workspace's ground on the shell. Writing the captured
/// colors is the reset it obeys, and it is correct on every terminal that
/// answered the query. Otherwise fall back to asking the terminal to reset
/// to its own defaults (OSC 110/111).
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

    /// The reset the operator's shell actually gets. When the terminal
    /// reported its own defaults at startup, hand exactly those back with an
    /// OSC set — the reset Apple Terminal obeys — instead of the OSC 110/111
    /// request it silently drops, which is what left the theme ground on the
    /// shell after a detach.
    #[test]
    fn a_known_default_is_handed_back_by_setting_it_not_by_asking_for_a_reset() {
        let mut out: Vec<u8> = Vec::new();
        write_reset(&mut out, Some(((0x3a, 0x2b, 0x26), (0x00, 0x00, 0x00))));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]10;#3a2b26\x1b\\\x1b]11;#000000\x1b\\",
            "a captured default is set back, never left to an ignored OSC 110/111"
        );
    }

    /// With no captured default — a terminal that does not answer the query,
    /// or no tty at all — fall back to asking the terminal to reset itself.
    #[test]
    fn an_unknown_default_falls_back_to_the_reset_request() {
        let mut out: Vec<u8> = Vec::new();
        write_reset(&mut out, None);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b]110\x1b\\\x1b]111\x1b\\",
            "no captured default means ask the terminal to reset to its own"
        );
    }

    /// A terminal's OSC 10/11 replies, ST- and BEL-terminated, are read out
    /// of the raw byte stream and 16-bit `rgb:` channels are downscaled to
    /// eight bits. The trailing DA1 reply is the fence the reader stops on.
    #[test]
    fn a_terminals_color_reply_is_parsed_and_downscaled() {
        let buf = b"\x1b]10;rgb:3a3a/2b2b/2626\x1b\\\x1b]11;rgb:0000/0000/0000\x07\x1b[?62;c";
        let (fg, bg) = parse_osc_colors(buf);
        assert_eq!(fg, Some((0x3a, 0x2b, 0x26)));
        assert_eq!(bg, Some((0x00, 0x00, 0x00)));
        assert!(da1_answered(buf), "the DA1 reply is the read fence");
    }

    /// Neither the `#rrggbb` form nor a short `rgb:` component is near-black:
    /// each width scales across its own range, so `f` is full intensity.
    #[test]
    fn color_payloads_parse_hash_and_short_forms() {
        assert_eq!(parse_color_payload(b"#ff8800"), Some((0xff, 0x88, 0x00)));
        assert_eq!(parse_color_payload(b"rgb:f/0/8"), Some((255, 0, 136)));
        assert_eq!(parse_color_payload(b"rubbish"), None);
    }

    /// A DA1 reply that has not arrived yet does not falsely fence the read:
    /// the color replies alone carry no CSI closed by `c`.
    #[test]
    fn da1_is_not_reported_before_it_arrives() {
        assert!(!da1_answered(b"\x1b]11;rgb:0000/0000/0000\x1b\\"));
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
