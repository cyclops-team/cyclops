//! This crate's harness really goes through the shared rig.
//!
//! The rule itself (kill, then unlink what kill-server leaves behind, from
//! Drop so a panicking test tears down too) is stated and tested once, in
//! cyclops-testrig. What is worth re-checking here is the wiring: dropping
//! a `TestServer` must take the server and its socket file with it, so a
//! helper added to this harness later cannot quietly grow a second
//! teardown path.

mod common;

use std::path::PathBuf;
use std::process::Command;

use common::TestServer;

#[test]
fn dropping_the_harness_kills_the_server_and_removes_its_socket() {
    let Some(srv) = TestServer::new("teardown") else {
        return;
    };
    srv.new_session("teardown");

    let path: PathBuf = srv
        .socket_path()
        .expect("live server reports its socket path");
    assert!(
        path.exists(),
        "socket {path:?} missing while the server is up"
    );

    let sock = srv.sock().to_string();
    drop(srv);

    // Isolation is unchanged: the server is gone.
    let alive = Command::new("tmux")
        .args(["-L", &sock, "list-sessions"])
        .env_remove("TMUX")
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
