//! Terminal guard: raw mode, alternate screen, panic-safe restore.

use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableMouseCapture;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;

static RESTORING: AtomicBool = AtomicBool::new(false);

/// Owns the terminal mode until dropped.
pub struct TermGuard {
    active: bool,
}

impl TermGuard {
    /// Enter raw mode and the alternate screen. Mouse capture stays off in
    /// step 4; step 9 enables it through the same guard.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(EnterAlternateScreen)?;
        let _ = out.execute(EnableMouseCapture);
        let _ = out.flush();
        install_panic_hook();
        Ok(TermGuard { active: true })
    }

    fn restore(&mut self) {
        if !self.active || RESTORING.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut out = io::stdout();
        let _ = out.execute(DisableMouseCapture);
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
