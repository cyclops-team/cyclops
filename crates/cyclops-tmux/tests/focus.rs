//! focus_pane against an isolated tmux server: the jump really moves the
//! active window and pane, and failures are named errors.

mod common;

use common::TestServer;
use cyclops_tmux::{focus_pane, TmuxError};

/// Pane id of the active pane, asked of the isolated server directly.
fn active_pane(srv: &TestServer, session: &str) -> String {
    let out = srv.tmux(&[
        "display-message",
        "-p",
        "-t",
        session,
        "#{window_index} #{pane_id}",
    ]);
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn focus_jumps_across_windows_and_panes() {
    let Some(srv) = TestServer::new("focus") else {
        return;
    };
    srv.new_session("focus");
    // A second window and a split in the first: three panes, two windows.
    srv.tmux_ok(&["new-window", "-t", "focus:", "/bin/sh"]);
    srv.tmux_ok(&["split-window", "-t", "focus:0", "/bin/sh"]);
    let out = srv.tmux(&[
        "list-panes",
        "-s",
        "-t",
        "focus",
        "-F",
        "#{window_index} #{pane_id}",
    ]);
    assert!(out.status.success());
    let panes: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let (w, p) = l.split_once(' ').expect("window and pane");
            (w.to_string(), p.to_string())
        })
        .collect();
    assert_eq!(panes.len(), 3, "panes: {panes:?}");

    // Jump to every pane in turn; the active window and pane must follow.
    for (window, pane) in &panes {
        focus_pane(Some(srv.sock()), Some("/dev/null".as_ref()), pane).expect("focus");
        assert_eq!(active_pane(&srv, "focus"), format!("{window} {pane}"));
    }
}

#[test]
fn focus_unknown_pane_is_a_command_error() {
    let Some(srv) = TestServer::new("focusgone") else {
        return;
    };
    srv.new_session("focusgone");
    match focus_pane(Some(srv.sock()), Some("/dev/null".as_ref()), "%999") {
        Err(TmuxError::Command(text)) => assert!(!text.is_empty()),
        other => panic!("expected a command error, got {other:?}"),
    }
}
