//! The isolated tmux server every Cyclops test runs against.
//!
//! One rule, one file. The rule was previously copied into every harness
//! that needed a tmux server, and three review rounds in a row fixed the
//! copy a reviewer had named while the next copy kept leaking: the suite
//! stayed green and still left a dead socket file per run. Nothing outside
//! this crate starts or kills a tmux server, so "does teardown clean up?"
//! is answered by reading [`TmuxServer`] and nothing else.
//!
//! The rule in full, in the order it has to happen:
//!
//! 1. Address the server by a unique `-L` name (`cyc-<tag>-<pid>`), never
//!    the user's default server. The pid keeps concurrent test binaries
//!    off each other's server.
//! 2. Every call carries `-f /dev/null` so the user's tmux config cannot
//!    change behavior, `-u` so tmux does not sanitize tabs and non-ASCII
//!    to `_` for a non-UTF-8 client (F14), and an unset `TMUX` so a test
//!    run from inside tmux does not look nested.
//! 3. Teardown asks tmux for `#{socket_path}` rather than recomputing it:
//!    tmux derives that path from `TMUX_TMPDIR` and the uid by a rule that
//!    has moved between versions. Only a server can answer, so when none
//!    is left teardown gives tmux a throwaway session on the same `-L`
//!    name, which is the same file, and asks that.
//! 4. Teardown kills the server, then unlinks that path. `kill-server`
//!    stops a server and unlinks nothing (MEASURED), and a server that
//!    exits on its own leaves the file too.
//! 5. Teardown lives in `Drop`, so it also runs when a test panics. A
//!    straight-line kill at the end of a test body leaks a live server,
//!    not just a file, the first time an assertion fails.
//!
//! It is its own crate because the sites that need it span both crates and
//! kinds: integration tests in `cyclops-tmux` and `cyclopsd`, plus a unit
//! test inside the `cyclopsd` library. A dev-dependency reaches all three;
//! a `#[cfg(test)]` module in any one crate reaches none of the others.
//!
//! What it does not own: anything that ships. `publish = false`, no
//! dependencies, and nothing in the product links it. It also does not own
//! the shell half of the same rule, which bash cannot call: that is
//! `demos/lib.sh`, held to this contract by `tests/shell_teardown.rs`. And
//! it owns no fixture, session shape or wait helper beyond the server
//! itself; each test crate builds its own on top.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Is there a tmux binary to test against? Tests skip cleanly without one.
pub fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// An isolated tmux server, killed and unlinked on drop.
///
/// Constructing one only reserves the socket name; tmux starts the server
/// on the first command that needs one. Dropping one that never started a
/// server leaves nothing behind: teardown's step 3 opens a throwaway
/// session on the name and step 4 takes it straight back out.
pub struct TmuxServer {
    socket: String,
}

impl TmuxServer {
    /// Reserve `cyc-<tag>-<pid>` on the tmux socket directory.
    pub fn new(tag: &str) -> TmuxServer {
        TmuxServer {
            socket: format!("cyc-{tag}-{}", std::process::id()),
        }
    }

    /// The `-L` name, for code that has to name the same server itself
    /// (a daemon config, a helper thread driving the pane).
    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// A tmux command already pointed at this server. Callers append their
    /// own arguments; nobody re-states the isolation flags.
    pub fn cmd(&self) -> Command {
        let mut c = Command::new("tmux");
        c.args(["-u", "-L", &self.socket, "-f", "/dev/null"])
            .env_remove("TMUX");
        c
    }

    /// Run a tmux command and hand back whatever it did.
    pub fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("run tmux")
    }

    /// Same, but the command must succeed.
    pub fn run_ok(&self, args: &[&str]) {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Visible screen of one pane, plain text.
    pub fn capture(&self, target: &str) -> String {
        let out = self.run(&["capture-pane", "-p", "-t", target]);
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Bounded wait for text to render in a pane. Test-side only: the
    /// product never polls, but a test driving a real TUI has no edge to
    /// await for "the shell finished drawing".
    pub fn wait_screen(&self, target: &str, needle: &str) {
        let t = Instant::now();
        while !self.capture(target).contains(needle) {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "{needle:?} never rendered in {target}: {}",
                self.capture(target)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Where this server's socket file lives, asked of the server itself.
    /// None when no server is up.
    pub fn socket_path(&self) -> Option<PathBuf> {
        let out = self
            .cmd()
            .args(["display-message", "-p", "#{socket_path}"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!path.is_empty()).then(|| PathBuf::from(path))
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        // Step 3. A live server answers directly.
        let mut socket_path = self.socket_path();
        if socket_path.is_none() {
            // Nothing left to ask does NOT mean nothing left to remove.
            // A server that exits on its own leaves its socket behind, and
            // that is the case that kept leaking: the daemon under test
            // starts the server, the daemon stops, the file stays. One
            // throwaway session brings a server back on the same name, and
            // therefore on the same file, so tmux can report the path; the
            // kill below takes it straight out again. `start-server` alone
            // does not work: tmux 3.6a exits a server with no sessions
            // before the next command reaches it (MEASURED).
            let _ = self
                .cmd()
                .args(["new-session", "-d", "-s", "teardown-probe", "/bin/sh"])
                .output();
            socket_path = self.socket_path();
        }
        // Step 4: kill, then unlink what kill-server leaves behind.
        let _ = self.cmd().arg("kill-server").output();
        if let Some(path) = socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
}
