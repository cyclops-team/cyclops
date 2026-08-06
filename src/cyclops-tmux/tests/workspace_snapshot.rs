//! Adapter-owned workspace snapshot tests (task D2).
//!
//! `ControlClient::workspace_snapshot` replaces the `W + 3` one-shot-process
//! fan-out `src/cyclops-workspace/src/sync.rs` performs today
//! (`list-sessions`, a membership query, `list-windows`, then one
//! `list-panes` per window) with two control-mode commands over a
//! connection that already exists. These tests prove correctness against
//! the rig's own tmux invocations — never against the function under test —
//! prove the command count does not grow with window count, and record
//! (never gate; see `src/cyclops-workspace/tests/baseline.rs`'s
//! rationale) the wall-clock gap against the old fan-out shape.

mod common;

use std::time::Instant;

use common::TestServer;
use cyclops_tmux::{
    list_panes, list_sessions, list_window_memberships, list_windows, ControlClient,
};

fn lines(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// One format expanded against one target, via the rig's own tmux
/// invocation (not the client under test).
fn field(srv: &TestServer, target: &str, format: &str) -> String {
    let out = srv.tmux(&["display-message", "-p", "-t", target, format]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Ground truth for one session: (window_id, pane_ids in index order) for
/// every window, in window-index order, read straight from tmux.
fn windows_and_panes(srv: &TestServer, session_id: &str) -> Vec<(String, Vec<String>)> {
    let out = srv.tmux(&["list-windows", "-t", session_id, "-F", "#{window_id}"]);
    lines(&out.stdout)
        .into_iter()
        .map(|window_id| {
            let out = srv.tmux(&["list-panes", "-t", &window_id, "-F", "#{pane_id}"]);
            (window_id.clone(), lines(&out.stdout))
        })
        .collect()
}

#[tokio::test]
async fn snapshot_matches_a_multi_session_multi_window_fixture_with_awkward_names() {
    let Some(srv) = TestServer::new("snap-fixture") else {
        return;
    };
    // A session with a plain name, a window renamed with an embedded space,
    // a split so the window has two panes, and a second window.
    srv.new_session("alpha");
    srv.tmux_ok(&["rename-window", "-t", "alpha:0", "my window one"]);
    srv.tmux_ok(&["split-window", "-h", "-t", "alpha:0", "/bin/sh"]);
    srv.tmux_ok(&["new-window", "-t", "alpha", "-n", "second win", "/bin/sh"]);
    // A second session whose own name has a space, to exercise the
    // `list-sessions` companion query's own free-text field.
    srv.new_session("beta session");
    srv.tmux_ok(&["split-window", "-v", "-t", "beta session:0", "/bin/sh"]);

    let (client, _notif) = ControlClient::spawn(srv.config("alpha"))
        .await
        .expect("attach");
    let snap = client.workspace_snapshot().await.expect("snapshot");

    let out = srv.tmux(&["list-sessions", "-F", "#{session_id}\t#{session_name}"]);
    let expected_sessions: Vec<(String, String)> = lines(&out.stdout)
        .iter()
        .map(|l| {
            let (id, name) = l.split_once('\t').expect("session_id\\tsession_name");
            (id.to_string(), name.to_string())
        })
        .collect();
    assert_eq!(
        snap.sessions.len(),
        expected_sessions.len(),
        "session count"
    );

    for (expected_id, expected_name) in &expected_sessions {
        let session = snap
            .sessions
            .iter()
            .find(|s| &s.id == expected_id)
            .unwrap_or_else(|| panic!("snapshot missing session {expected_id}"));
        assert_eq!(&session.name, expected_name, "name for {expected_id}");

        let expected_windows = windows_and_panes(&srv, expected_id);
        assert_eq!(
            session.windows.len(),
            expected_windows.len(),
            "window count for session {expected_name:?}"
        );
        for (window, (expected_window_id, expected_pane_ids)) in
            session.windows.iter().zip(&expected_windows)
        {
            assert_eq!(&window.id, expected_window_id, "window id order");
            let pane_ids: Vec<String> = window.panes.iter().map(|p| p.id.clone()).collect();
            assert_eq!(
                &pane_ids, expected_pane_ids,
                "panes for window {expected_window_id} ({:?})",
                window.name
            );
        }
    }

    // The space-bearing window and session names survive whole.
    let alpha = snap
        .sessions
        .iter()
        .find(|s| s.name == "alpha")
        .expect("alpha session");
    let alpha_window_names: Vec<&str> = alpha.windows.iter().map(|w| w.name.as_str()).collect();
    assert!(
        alpha_window_names.contains(&"my window one"),
        "space-bearing window name survived: {alpha_window_names:?}"
    );
    assert!(alpha_window_names.contains(&"second win"));
    let beta = snap
        .sessions
        .iter()
        .find(|s| s.name == "beta session")
        .expect("space-bearing session name survived");
    assert_eq!(beta.windows.len(), 1);
    assert_eq!(beta.windows[0].panes.len(), 2);

    // Active flags and pane dimensions at every level agree with a direct
    // read, not merely with each other.
    let active_window_id = field(&srv, "alpha", "#{window_id}");
    assert_eq!(
        alpha.windows.iter().filter(|w| w.active).count(),
        1,
        "exactly one active window per session"
    );
    let active_window = alpha
        .windows
        .iter()
        .find(|w| w.id == active_window_id)
        .expect("the active window tmux reports is in the snapshot");
    assert!(active_window.active);

    let raw_dims = field(&srv, "%0", "#{pane_width}\t#{pane_height}");
    let (expected_w, expected_h) = raw_dims
        .split_once('\t')
        .map(|(w, h)| (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap()))
        .expect("pane_width\\tpane_height");
    let pane0 = alpha
        .windows
        .iter()
        .flat_map(|w| &w.panes)
        .find(|p| p.id == "%0")
        .expect("%0 present in the snapshot");
    assert_eq!((pane0.width, pane0.height), (expected_w, expected_h));

    client.shutdown().await;
}

#[tokio::test]
async fn snapshot_command_count_does_not_scale_with_window_count() {
    let Some(srv) = TestServer::new("snap-count") else {
        return;
    };
    for &w in &[1usize, 4, 8] {
        let session = format!("cnt{w}");
        srv.tmux_ok(&[
            "new-session",
            "-d",
            "-s",
            &session,
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        for _ in 1..w {
            srv.tmux_ok(&["new-window", "-t", &session, "/bin/sh"]);
        }
    }
    let (client, _notif) = ControlClient::spawn(srv.config("cnt1"))
        .await
        .expect("attach");

    let mut deltas = Vec::new();
    for &w in &[1usize, 4, 8] {
        let before = client.commands_issued();
        let snap = client.workspace_snapshot().await.expect("snapshot");
        let after = client.commands_issued();

        let session_name = format!("cnt{w}");
        let session = snap
            .sessions
            .iter()
            .find(|s| s.name == session_name)
            .unwrap_or_else(|| panic!("snapshot missing session {session_name}"));
        assert_eq!(session.windows.len(), w, "expected {w} windows");

        deltas.push(after - before);
    }

    assert!(
        deltas.iter().all(|&d| d == deltas[0]),
        "workspace_snapshot must issue the same fixed number of commands \
         regardless of window count, got {deltas:?} for W=1,4,8"
    );
    assert_eq!(
        deltas[0], 2,
        "workspace_snapshot is documented as exactly two commands (list-panes -a, list-sessions)"
    );
    println!("workspace_snapshot commands issued per call for W=1,4,8: {deltas:?}");

    client.shutdown().await;
}

/// Timing PRINT only — recorded, never gated (see
/// `src/cyclops-workspace/tests/baseline.rs`'s rationale for why this
/// harness never asserts a wall-clock budget).
#[tokio::test]
async fn timing_fan_out_vs_snapshot_on_eight_windows() {
    let Some(srv) = TestServer::new("snap-timing") else {
        return;
    };
    let session = "timing";
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        session,
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ]);
    for _ in 1..8 {
        srv.tmux_ok(&["new-window", "-t", session, "/bin/sh"]);
    }
    let socket = Some(srv.sock());

    // 1. The pre-L1 shape fetch_workspace_model used to walk: one
    //    list-sessions, one membership query, one list-windows, then one
    //    list-panes PER WINDOW — each a fresh one-shot tmux process. Kept
    //    as the timing comparison point.
    let t = Instant::now();
    let _sessions = list_sessions(socket).expect("list_sessions");
    let _memberships = list_window_memberships(socket).expect("list_window_memberships");
    let windows = list_windows(session, socket).expect("list_windows");
    for win in &windows {
        list_panes(&win.id, socket).expect("list_panes");
    }
    let fan_out = t.elapsed();

    // 2. workspace_snapshot: two control-mode commands total, regardless of
    //    window count (see snapshot_command_count_does_not_scale_with_
    //    window_count above).
    let (client, _notif) = ControlClient::spawn(srv.config(session))
        .await
        .expect("attach");
    let t = Instant::now();
    let snap = client.workspace_snapshot().await.expect("snapshot");
    let snapshot_time = t.elapsed();
    assert_eq!(
        snap.sessions
            .iter()
            .find(|s| s.name == session)
            .expect("session present")
            .windows
            .len(),
        8
    );

    println!(
        "=== D2: fan-out (list-sessions+membership+list-windows+list-panes x8) vs workspace_snapshot, 8 windows ==="
    );
    println!(
        "fan_out={:.2}ms workspace_snapshot={:.2}ms",
        fan_out.as_secs_f64() * 1000.0,
        snapshot_time.as_secs_f64() * 1000.0
    );

    client.shutdown().await;
}
