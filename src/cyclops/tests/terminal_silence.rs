//! What the CLI writes to a real terminal, with a broken theme file in
//! place.
//!
//! These need a pty, not a pipe: the theme is read only when stdout is a
//! terminal, so a piped run cannot see the warning either test is about.
//!
//! - `cyclops hook` must write nothing at all, on stdout or stderr. It
//!   runs inside a vendor CLI's hook budget, and anything it prints lands
//!   in that CLI's output.
//! - `cyclops ui` must say what is wrong with the theme once. It loads the
//!   theme itself; a second load in the CLI's dispatch said it twice.
//! - A one-shot command must say it in one line. The warning quotes a TOML
//!   parse error, and toml's own Display is a diagnostic block.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// A theme file that cannot parse, so loading it warns.
const BROKEN_THEME: &str = "[surface\n";

/// Scratch home with a themes directory holding a broken default theme.
/// Under the relocatable scratch root (F24), not the OS temp dir.
fn home_with_a_broken_theme(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("themes")).expect("create scratch home");
    std::fs::write(dir.join("themes/dark.toml"), BROKEN_THEME).expect("write broken theme");
    dir
}

/// ptsname returns a pointer into a static buffer, so only one thread may
/// be between posix_openpt and the open of the slave at a time.
static PTY: Mutex<()> = Mutex::new(());

/// One pty: (master fd, slave file). Allocated with posix_openpt rather
/// than openpty, which lives in libutil on some systems and would add a
/// link flag to a test-only dependency.
fn pty() -> (i32, File) {
    let _held = PTY.lock().expect("pty lock");
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        master >= 0,
        "posix_openpt: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(unsafe { libc::grantpt(master) }, 0, "grantpt");
    assert_eq!(unsafe { libc::unlockpt(master) }, 0, "unlockpt");
    let name = unsafe {
        let p = libc::ptsname(master);
        assert!(!p.is_null(), "ptsname");
        CStr::from_ptr(p).to_string_lossy().into_owned()
    };
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
        .expect("open pty slave");
    // Non-blocking: "the child wrote nothing" must read as an empty
    // result, not as a read that hangs on a pty this process holds open.
    unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) };
    (master, slave)
}

/// Everything written to the terminal so far.
fn drain(master: i32) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The CLI with `home` as CYCLOPS_HOME and the pty on all three streams,
/// which is what makes it look like an interactive terminal.
fn spawn_on_a_terminal(home: &Path, args: &[&str], slave: &File) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env_remove("NO_COLOR")
        .env_remove("CYCLOPS_THEME")
        .args(args)
        .stdin(slave.try_clone().expect("dup pty for stdin"))
        .stdout(slave.try_clone().expect("dup pty for stdout"))
        .stderr(slave.try_clone().expect("dup pty for stderr"))
        .spawn()
        .expect("run cyclops")
}

#[test]
fn hook_writes_nothing_to_a_terminal_even_with_a_broken_theme() {
    let home = home_with_a_broken_theme("hooksilence");
    // No daemon is listening either, so the hook fails at the step it can:
    // the contract is silence and exit 0 regardless. stdin is the pty and
    // stays open, so the payload read hits its own budget rather than EOF,
    // which is also the shape a wedged vendor hook has.
    let (master, slave) = pty();
    let mut child = spawn_on_a_terminal(&home, &["hook", "Stop", "--agent", "reviewer"], &slave);
    let status = child.wait().expect("hook exits");
    let wrote = drain(master);
    unsafe { libc::close(master) };

    assert!(status.success(), "hook must exit 0, got {status}");
    assert!(
        wrote.is_empty(),
        "hook wrote to the vendor CLI's terminal: {wrote:?}"
    );
    // The failure went where it belongs instead.
    let log = std::fs::read_to_string(home.join("hook-errors.log")).unwrap_or_default();
    assert!(
        log.contains("hook Stop"),
        "the failure belongs in hook-errors.log, got {log:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// The broken-theme warning as a one-shot command prints it: one line,
/// one sentence, and the file named once.
///
/// It used to print a seven-line block. `theme: ` prefixed a message that
/// opened with the word "theme" again, then toml's five-line diagnostic
/// with its caret diagram, and the tail of the sentence the diagnostic was
/// quoted inside ("Using built-in colors.") landed alone on the last line.
///
/// `cyclops theme` is the command under test because it needs no daemon,
/// so what reaches the terminal is the theme warning and the listing.
#[test]
fn a_one_shot_command_says_a_broken_theme_in_one_line() {
    let home = home_with_a_broken_theme("oneshottheme");
    std::fs::write(
        home.join("themes/light.toml"),
        "[surface]\ndim = \"#6e6e6e\"\n",
    )
    .expect("write a theme that loads");
    let (master, slave) = pty();
    let mut child = spawn_on_a_terminal(&home, &["theme"], &slave);
    let status = child.wait().expect("theme exits");
    let wrote = drain(master);
    unsafe { libc::close(master) };
    assert!(status.success(), "{status}");

    let said: Vec<&str> = wrote
        .lines()
        .map(str::trim_end)
        .filter(|l| l.contains("theme:"))
        .collect();
    assert_eq!(said.len(), 1, "the warning is not one line: {wrote:?}");
    let line = said[0];
    // What is wrong, where, and what is being painted instead. The path is
    // in there once: the prefix and the message both naming it read
    // "theme: theme /path/...".
    assert!(line.contains("isn't valid TOML"), "{line:?}");
    assert!(line.contains("line 1, column 9"), "{line:?}");
    assert!(line.contains("built-in colors"), "{line:?}");
    assert!(!line.contains("theme: theme "), "stutters: {line:?}");
    assert_eq!(
        line.matches("dark.toml").count(),
        1,
        "the file is named more than once: {line:?}"
    );
    // The diagnostic block is gone, caret diagram and all.
    for noise in ["TOML parse error", "  |", "^"] {
        assert!(!wrote.contains(noise), "{noise:?} survived: {wrote:?}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn ui_reports_a_broken_theme_once() {
    let home = home_with_a_broken_theme("uitheme");
    // A socket nothing answers on: the UI starts, says what it has to say
    // about the theme before taking the screen, and then sits on a silent
    // connection until it is killed. The warning is what is measured.
    let sock = std::os::unix::net::UnixListener::bind(home.join("sock")).expect("bind sock");
    let (master, slave) = pty();
    let mut child = spawn_on_a_terminal(&home, &["ui"], &slave);
    std::thread::sleep(Duration::from_millis(1500));
    let _ = child.kill();
    let _ = child.wait();
    let wrote = drain(master);
    unsafe { libc::close(master) };
    drop(sock);

    let said = wrote.matches("theme:").count();
    assert_eq!(
        said, 1,
        "the theme warning must be said once, not {said} times: {wrote:?}"
    );
    assert!(
        wrote.contains("isn't valid TOML"),
        "the warning should name what is wrong: {wrote:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
