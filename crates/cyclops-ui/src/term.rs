//! The terminal backend: raw mode, alternate screen, frame writes.
//!
//! Hand-rolled on termios plus ANSI, no TUI dependency: the offline build
//! environment carries none, and the surface the stream needs is small.
//! Frames are drawn as one buffered write from the home position with
//! per-line clears; the screen is never cleared wholesale after entry, so
//! nothing can blink. Autowrap is off while the UI runs: long lines clip
//! at the edge instead of reflowing rows.

use std::io::Write;
use std::mem::MaybeUninit;

/// Raw-mode guard. Restores the terminal on drop, unwind included.
pub struct Term {
    orig: libc::termios,
}

impl Term {
    /// Enter raw mode and the alternate screen. Fails when stdin is not a
    /// terminal.
    pub fn enter() -> std::io::Result<Term> {
        let orig = unsafe {
            let mut t = MaybeUninit::<libc::termios>::uninit();
            if libc::tcgetattr(libc::STDIN_FILENO, t.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            t.assume_init()
        };
        let mut raw = orig;
        // No echo, no line buffering, no signal keys (Ctrl-C is a key the
        // app answers so the guard always restores), no flow-control
        // freeze. Reads block for one byte.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Alternate screen, hidden cursor, autowrap off; then mouse
        // press+wheel reporting in the SGR encoding (1006, the one whose
        // coordinates are not capped at 223, and the one every terminal
        // this decade speaks). A terminal that knows neither ignores both
        // and the UI is keyboard-only, exactly as before.
        print!("\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?1000;1006h");
        let _ = std::io::stdout().flush();
        Ok(Term { orig })
    }

    /// Terminal size as (width, height) in cells, with a classic fallback.
    pub fn size() -> (usize, usize) {
        unsafe {
            let mut ws = MaybeUninit::<libc::winsize>::uninit();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, ws.as_mut_ptr()) == 0 {
                let ws = ws.assume_init();
                if ws.ws_col > 0 && ws.ws_row > 0 {
                    return (ws.ws_col as usize, ws.ws_row as usize);
                }
            }
        }
        (80, 24)
    }

    /// Draw one frame: home the cursor, write every row with a reset in
    /// front (a clipped colored line cannot bleed) and a clear behind
    /// (shorter lines leave no ghosts). One write, one flush.
    pub fn draw(&mut self, rows: &[String]) {
        let mut out = String::with_capacity(rows.len() * 64 + 8);
        out.push_str("\x1b[H");
        for (i, row) in rows.iter().enumerate() {
            out.push_str("\x1b[0m");
            out.push_str(row);
            out.push_str("\x1b[K");
            if i + 1 < rows.len() {
                out.push_str("\r\n");
            }
        }
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // Mouse reporting off first: a shell fed mouse escapes is the one
        // way this UI could outlive itself. Then leave the alternate
        // screen, restore wrap and cursor, then the saved termios. Errors
        // are moot: the process is on its way out.
        print!("\x1b[?1000;1006l\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
        }
    }
}
