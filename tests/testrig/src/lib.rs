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
//! 1. Address the server by a unique `-L` name
//!    (`cyc-<tag>-<pid>-<sequence>`), never the user's default server. The
//!    pid separates test binaries and the sequence separates repeated rigs
//!    inside one executable.
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
//! 6. One external cleanup owner per test executable records only that
//!    executable's exact socket names. If the executable is terminated before
//!    `Drop`, closing its registration pipe makes the owner remove the still
//!    registered servers. The owner has its own process group, so cancellation
//!    of the fixture's group does not kill the cleanup it needs. Normal drops
//!    unregister after their own cleanup.
//!
//! It is its own crate because the sites that need it span both crates and
//! kinds: integration tests in `cyclops-tmux` and `cyclopsd`, plus a unit
//! test inside the `cyclopsd` library. A dev-dependency reaches all three;
//! a `#[cfg(test)]` module in any one crate reaches none of the others.
//!
//! What it does not own: anything that ships. `publish = false`, no
//! dependencies, and nothing in the product links it. It also does not own
//! the shell half of the same rule: external cleanup sources
//! `tests/e2e/lib/lib.sh`, held to this contract by
//! `tests/testrig/tests/shell_teardown.rs`. And
//! it owns no fixture, session shape or wait helper beyond the server
//! itself; each test crate builds its own on top.

use std::io::Write;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CLEANUP_OWNER_SCRIPT: &str = r#"
. "$1"
journal=
while IFS= read -r record; do
  case "$record" in
    +cyc-*|-cyc-*) journal="${journal}
${record}" ;;
    *) exit 2 ;;
  esac
done

printf '%s\n' "$journal" |
  awk 'length($0) > 1 { state[substr($0, 2)] = substr($0, 1, 1) }
       END { for (socket in state) if (state[socket] == "+") print socket }' |
  while IFS= read -r socket; do
    cyc_tmux_teardown "$socket"
  done
"#;

const SHELL_HELPERS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../e2e/lib/lib.sh");

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);
static CLEANUP_OWNER: OnceLock<Mutex<CleanupOwner>> = OnceLock::new();

struct CleanupOwner {
    input: ChildStdin,
    _process: Child,
}

impl CleanupOwner {
    fn spawn() -> Self {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", CLEANUP_OWNER_SCRIPT, "cyclops-cleanup", SHELL_HELPERS])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("TMUX");
        // A cancelled test runner can terminate its entire process group,
        // which is the path that bypasses Rust Drop. The owner must not share
        // that group or it dies before observing the closed registration pipe.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut process = command.spawn().expect("start exact tmux cleanup owner");
        let input = process
            .stdin
            .take()
            .expect("cleanup owner registration pipe");
        Self {
            input,
            _process: process,
        }
    }

    fn record(&mut self, operation: char, socket: &str) -> std::io::Result<()> {
        writeln!(self.input, "{operation}{socket}")?;
        self.input.flush()
    }
}

fn cleanup_owner() -> &'static Mutex<CleanupOwner> {
    CLEANUP_OWNER.get_or_init(|| Mutex::new(CleanupOwner::spawn()))
}

fn register(socket: &str) {
    cleanup_owner()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record('+', socket)
        .expect("register exact tmux cleanup target");
}

fn unregister(socket: &str) {
    let _ = cleanup_owner()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .record('-', socket);
}

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
    /// Reserve `cyc-<tag>-<pid>-<sequence>` on the tmux socket directory.
    pub fn new(tag: &str) -> TmuxServer {
        assert!(
            !tag.is_empty()
                && tag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "tmux test tag must contain only ASCII letters, digits, '-' or '_': {tag:?}"
        );
        let sequence = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
        let socket = format!("cyc-{tag}-{}-{sequence}", std::process::id());
        register(&socket);
        TmuxServer { socket }
    }

    /// Stop this server and retain its exact `-L` address for a replacement.
    ///
    /// Most fixtures should construct one server and let `Drop` clean it up.
    /// A server-incarnation test sometimes needs the daemon to observe that
    /// one exact tmux address disappeared and later returned. This operation
    /// keeps the external cleanup owner registered across that gap instead of
    /// allocating a different fixture address or exposing arbitrary sockets.
    pub fn restart(self) -> TmuxServer {
        let this = ManuallyDrop::new(self);
        cleanup_server(&this);
        TmuxServer {
            socket: this.socket.clone(),
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

    /// Make this isolated server unavailable while retaining its cleanup
    /// owner. A server-lifecycle test can then start the same address again;
    /// dropping this fixture still removes any socket the loss left behind.
    pub fn simulate_server_loss(&self) {
        self.run_ok(&["kill-server"]);
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
        cleanup_server(self);
        // Keep the external owner armed until normal cleanup is complete. If
        // the executable is killed between these two statements, duplicate
        // exact cleanup is harmless and a leak is not.
        unregister(&self.socket);
    }
}

fn cleanup_server(server: &TmuxServer) {
    // Step 3. A live server answers directly.
    let mut socket_path = server.socket_path();
    if socket_path.is_none() {
        // Nothing left to ask does NOT mean nothing left to remove. A server
        // that exits on its own leaves its socket behind. One throwaway
        // session recreates the same exact file so tmux can report its path.
        let _ = server
            .cmd()
            .args(["new-session", "-d", "-s", "teardown-probe", "/bin/sh"])
            .output();
        socket_path = server.socket_path();
    }
    // Step 4: kill, then unlink what kill-server leaves behind.
    let _ = server.cmd().arg("kill-server").output();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}
