//! This crate's rig really goes through the shared tmux rig.
//!
//! The rule itself (kill, then unlink what kill-server leaves behind, from
//! Drop so a panicking test tears down too) is stated and tested once, in
//! cyclops-testrig. What is worth re-checking here is the wiring: dropping
//! the guard the cyclopsd tests use must take the server and its socket
//! file with it, so a helper added here later cannot quietly grow a second
//! teardown path.

use crate::common;

use std::path::PathBuf;
use std::process::Command;

use common::{tmux_available, TmuxGuard};

#[test]
fn dropping_the_guard_kills_the_server_and_removes_its_socket() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let guard = TmuxGuard::new("teardown");
    let socket = guard.socket().to_string();
    guard.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "teardown",
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ]);

    let path: PathBuf = guard
        .socket_path()
        .expect("live server reports its socket path");
    assert!(
        path.exists(),
        "socket {path:?} missing while the server is up"
    );

    drop(guard);

    // Isolation is unchanged: the server is gone.
    let alive = Command::new("tmux")
        .args(["-L", &socket, "list-sessions"])
        .output()
        .expect("run tmux");
    assert!(
        !alive.status.success(),
        "server survived teardown: {}",
        String::from_utf8_lossy(&alive.stdout)
    );

    // And so is the socket file it used to leave behind.
    assert!(!path.exists(), "teardown left the socket file {path:?}");
}
