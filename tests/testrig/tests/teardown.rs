//! The teardown contract, both halves and both exits.
//!
//! `kill-server` stops a tmux server and unlinks nothing, and a server that
//! exits on its own leaves the file too (both MEASURED). Every earlier
//! version of this rule was fixed for the normal exit only, so the panic
//! path is tested here as a first-class case: a failing test used to leave
//! a LIVE server behind, not just a file.

use std::path::PathBuf;
use std::process::Command;

use cyclops_testrig::{tmux_available, TmuxServer};

/// Start a server on a fresh rig and report (rig, socket name, socket file).
fn started(tag: &str) -> (TmuxServer, String, PathBuf) {
    let srv = TmuxServer::new(tag);
    srv.run_ok(&[
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
    let socket = srv.socket().to_string();
    let path = srv
        .socket_path()
        .expect("live server reports its socket path");
    assert!(
        path.exists(),
        "socket {path:?} missing while the server is up"
    );
    (srv, socket, path)
}

/// A live server on `socket`? Asked without the rig, so the answer does
/// not depend on the code under test.
fn server_is_up(socket: &str) -> bool {
    Command::new("tmux")
        .args(["-L", socket, "list-sessions"])
        .env_remove("TMUX")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn dropping_the_server_kills_it_and_removes_its_socket() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let (srv, socket, path) = started("teardown");
    drop(srv);
    assert!(!server_is_up(&socket), "server survived teardown");
    assert!(!path.exists(), "teardown left the socket file {path:?}");
}

/// The shape that survived three rounds of fixes: the server under test is
/// started by something else (a daemon), that something stops, and the
/// socket file outlives it. Teardown has nothing live to ask for the path
/// and used to give up and leave the file.
#[test]
fn a_server_that_is_already_gone_still_loses_its_socket() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let (srv, socket, path) = started("teardown-orphan");
    // Stop the server behind the rig's back, the way a daemon shutting
    // down does. kill-server unlinks nothing, so the file stays.
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .env_remove("TMUX")
        .output();
    assert!(
        path.exists(),
        "precondition: a killed server leaves its socket at {path:?}"
    );

    drop(srv);

    assert!(!server_is_up(&socket), "teardown left a server behind");
    assert!(
        !path.exists(),
        "teardown skipped the unlink because no server was left to ask: {path:?}"
    );
}

#[test]
fn a_panicking_test_tears_down_too() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let (srv, socket, path) = started("teardown-panic");
    // The rig is owned by the closure, so unwinding is what drops it.
    // The panic message says so: it is printed by the default hook during
    // a passing run and should not read like a real failure.
    let caught = std::panic::catch_unwind(move || {
        let _owned_by_the_unwind = srv;
        panic!("expected panic: proving tmux teardown runs on unwind");
    });
    assert!(caught.is_err(), "the closure must have panicked");

    assert!(
        !server_is_up(&socket),
        "a panicking test left a LIVE tmux server on {socket}"
    );
    assert!(!path.exists(), "a panicking test left the socket {path:?}");
}

#[test]
fn simulated_server_loss_reuses_the_owned_address_and_still_tears_down() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let (server, socket, path) = started("teardown-simulated-loss");

    server.simulate_server_loss();
    assert!(
        !server_is_up(&socket),
        "the simulated loss left the old server live"
    );

    server.run_ok(&["new-session", "-d", "-s", "replacement", "/bin/sh"]);
    let replacement_path = server
        .socket_path()
        .expect("replacement reports the owned socket path");
    assert_eq!(replacement_path, path);
    drop(server);

    assert!(!server_is_up(&socket), "replacement survived teardown");
    assert!(
        !replacement_path.exists(),
        "replacement teardown left the socket file {replacement_path:?}"
    );
}

#[test]
fn an_explicit_restart_reuses_one_owned_address_and_still_tears_down() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let (srv, socket, old_path) = started("teardown-restart");

    let replacement = srv.restart();
    assert_eq!(replacement.socket(), socket);
    assert!(!server_is_up(&socket), "restart left the old server live");
    assert!(!old_path.exists(), "restart left the old socket file");

    replacement.run_ok(&["new-session", "-d", "-s", "replacement", "/bin/sh"]);
    let replacement_path = replacement
        .socket_path()
        .expect("replacement reports the same owned socket path");
    assert_eq!(replacement_path, old_path);
    drop(replacement);

    assert!(!server_is_up(&socket), "replacement survived teardown");
    assert!(
        !replacement_path.exists(),
        "replacement teardown left the socket file {replacement_path:?}"
    );
}
