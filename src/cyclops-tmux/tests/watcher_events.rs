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
