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
