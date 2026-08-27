//! Structural changes (split, kill) reach the table via events, no polling.

mod common;

use common::{await_event, TestServer};
use cyclops_tmux::{PaneEvent, SessionWatcher};

#[tokio::test]
async fn split_and_kill_are_reflected_via_events() {
    let Some(srv) = TestServer::new("events") else {
        return;
    };
    srv.new_session("ev");

    let w = SessionWatcher::connect(srv.config("ev"))
        .await
        .expect("connect");
    assert_eq!(w.snapshot().len(), 1);
    let mut rx = w.subscribe();

    srv.tmux_ok(&["split-window", "-t", "ev", "/bin/sh"]);
    let added = await_event(&mut rx, "PaneAdded", |e| {
        matches!(e, PaneEvent::PaneAdded(_))
    })
    .await;
    let PaneEvent::PaneAdded(row) = added else {
        unreachable!()
    };
    assert_eq!(row.pane_id, "%1");
    assert!(w.pane("%1").is_some(), "table tracks the new pane");
    assert_eq!(w.snapshot().len(), 2);

    srv.tmux_ok(&["kill-pane", "-t", "%1"]);
    let removed = await_event(
        &mut rx,
        "PaneRemoved",
        |e| matches!(e, PaneEvent::PaneRemoved(id) if id == "%1"),
    )
    .await;
    let PaneEvent::PaneRemoved(id) = removed else {
        unreachable!()
    };
    assert_eq!(id, "%1");
    assert!(w.pane("%1").is_none(), "table dropped the killed pane");
    assert_eq!(w.snapshot().len(), 1);

    w.shutdown().await;
}

#[tokio::test]
async fn session_removal_does_not_report_a_server_wide_moved_pane_as_gone() {
    let Some(srv) = TestServer::new("events-cross-session") else {
        return;
    };
    srv.new_session("source");
    srv.tmux_ok(&["split-window", "-t", "source", "/bin/sh"]);
    srv.new_session("destination");

    let w = SessionWatcher::connect(srv.config("source"))
        .await
        .expect("connect");
    let moved = w
        .snapshot()
        .into_iter()
        .find(|row| !row.active)
        .expect("source has a movable pane");
    let mut rx = w.subscribe();

    srv.tmux_ok(&["join-pane", "-d", "-s", &moved.pane_id, "-t", "destination"]);
    await_event(
        &mut rx,
        "moved PaneRemoved",
        |event| matches!(event, PaneEvent::PaneRemoved(id) if id == &moved.pane_id),
    )
    .await;

    assert_eq!(
        w.client()
            .server_pane_pid(&moved.pane_id)
            .await
            .expect("server-wide pane lookup"),
        Some(moved.pane_pid),
        "the source route disappeared but the physical pane survived"
    );

    srv.tmux_ok(&["kill-pane", "-t", &moved.pane_id]);
    assert_eq!(
        w.client()
            .server_pane_pid(&moved.pane_id)
            .await
            .expect("server-wide pane lookup after kill"),
        None
    );
    w.shutdown().await;
}

/// Gate 1: a client detaching and reattaching must not duplicate live rows.
///
/// A reattach makes tmux resend the session's structural picture, and the
/// watcher reconciles against `list-panes` when it does. If reconciliation
/// appended instead of merging, every reattach would grow the table by a
/// whole session while every pane id stayed valid, so the duplicates would
/// look like real panes to everything downstream: the roster, delivery
/// routing, and the FIFO all key on that table.
#[tokio::test]
async fn detach_and_reattach_do_not_duplicate_live_rows() {
    let Some(srv) = TestServer::new("events-reattach") else {
        return;
    };
    srv.new_session("reattach");
    srv.tmux_ok(&["split-window", "-t", "reattach", "/bin/sh"]);

    let w = SessionWatcher::connect(srv.config("reattach"))
        .await
        .expect("connect");
    common::eventually("the watcher sees both panes", || w.snapshot().len() == 2).await;
    let before: Vec<String> = sorted_pane_ids(&w);

    // A ControlClient IS a real tmux client, so spawn/shutdown is a genuine
    // attach/detach pair rather than a simulation of one.
    for round in 0..2 {
        let (client, _notif) = cyclops_tmux::ControlClient::spawn(srv.config("reattach"))
            .await
            .unwrap_or_else(|error| panic!("round {round} attach: {error}"));
        client.shutdown().await;
    }

    common::eventually("the table settles after reattach", || {
        w.snapshot().len() == 2
    })
    .await;

    let after = sorted_pane_ids(&w);
    assert_eq!(
        after, before,
        "detach and reattach changed the live pane set"
    );
    let mut unique = after.clone();
    unique.dedup();
    assert_eq!(unique, after, "a pane id appears twice in the live table");
    for id in &after {
        assert!(w.pane(id).is_some(), "{id} is listed but not addressable");
    }

    w.shutdown().await;
}

fn sorted_pane_ids(w: &SessionWatcher) -> Vec<String> {
    let mut ids: Vec<String> = w.snapshot().iter().map(|r| r.pane_id.clone()).collect();
    ids.sort();
    ids
}
