//! The shell home of the teardown rule, `tests/e2e/lib/lib.sh`, against the
//! same contract this crate holds Rust to.
//!
//! bash cannot call the Rust rule, so the rule has two homes and each one
//! has to be proven on its own. The two drifted exactly the way separate
//! homes do: Rust grew the already-gone fallback and the shell copy did
//! not, so a demo whose tmux server died before the EXIT trap ran left
//! `/tmp/tmux-<uid>/cyc-demo-<pid>` behind on every run (MEASURED).

use std::path::PathBuf;
use std::process::{Command, Output};

use cyclops_testrig::{tmux_available, TmuxServer};

/// The shell home. This crate sits at `<root>/tests/testrig`.
fn lib_sh() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../e2e/lib/lib.sh")
        .canonicalize()
        .expect("tests/e2e/lib/lib.sh")
}

/// Call one function from `tests/e2e/lib/lib.sh`, the way
/// `tests/e2e/m1_soak.py` does: source the file, then run the named
/// function with its arguments.
fn sh(func: &str, arg: &str) -> Output {
    Command::new("bash")
        .args([
            "-c",
            ". \"$1\"; shift; \"$@\"",
            "bash",
            lib_sh().to_str().expect("utf8 path"),
            func,
            arg,
        ])
        .env_remove("TMUX")
        .output()
        .expect("run bash")
}

/// Start a server on a fresh rig and report (rig, socket name, socket file).
fn started(tag: &str) -> (TmuxServer, String, PathBuf) {
    let srv = TmuxServer::new(tag);
    srv.run_ok(&["new-session", "-d", "-s", "shell", "/bin/sh"]);
    let socket = srv.socket().to_string();
    let path = srv.socket_path().expect("live server reports its socket");
    (srv, socket, path)
}

/// A live server on `socket`? Asked without the rig and without lib.sh, so
/// the answer does not depend on either code under test.
fn server_is_up(socket: &str) -> bool {
    Command::new("tmux")
        .args(["-L", socket, "list-sessions"])
        .env_remove("TMUX")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn the_shell_teardown_kills_a_live_server_and_removes_its_socket() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let (_srv, socket, path) = started("libsh");
    let out = sh("cyc_tmux_teardown", &socket);
    assert!(out.status.success(), "teardown must never fail its caller");
    assert!(!server_is_up(&socket), "server survived teardown");
    assert!(!path.exists(), "teardown left the socket file {path:?}");
}

/// The case that kept leaking. The server is gone before teardown runs, so
/// there is nothing left to ask for the socket path, and the file outlives
/// it. This is what a demo hits when its daemon or its server dies early.
#[test]
fn a_server_that_is_already_gone_still_loses_its_socket() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let (_srv, socket, path) = started("libsh-orphan");
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .env_remove("TMUX")
        .output();
    assert!(
        path.exists(),
        "precondition: a killed server leaves its socket at {path:?}"
    );

    let out = sh("cyc_tmux_teardown", &socket);

    assert!(out.status.success(), "teardown must never fail its caller");
    assert!(!server_is_up(&socket), "teardown left a server behind");
    assert!(
        !path.exists(),
        "teardown skipped the unlink because no server was left to ask: {path:?}"
    );
}

/// A demo that dies before it starts its server still runs the trap. The
/// probe session teardown opens to read the path is its own to clean up.
#[test]
fn tearing_down_a_socket_that_never_had_a_server_leaves_nothing() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    // A live server on another name reports the directory tmux derives for
    // this uid, which is where the unused name's file would land.
    let (_srv, _, live) = started("libsh-dir");
    let dir = live.parent().expect("socket dir").to_path_buf();
    let socket = format!("cyc-libsh-unused-{}", std::process::id());
    let path = dir.join(&socket);
    assert!(!path.exists(), "precondition: {path:?} must not exist yet");

    let out = sh("cyc_tmux_teardown", &socket);

    assert!(out.status.success(), "teardown must never fail its caller");
    assert!(!server_is_up(&socket), "teardown left a server behind");
    assert!(
        !path.exists(),
        "teardown left its own probe's socket {path:?}"
    );
}
