//! pane_in_mode and pane_dead flips reach the table via events.

mod common;

use common::{await_event, TestServer};
use cyclops_tmux::{PaneEvent, PaneField, SessionWatcher};

#[tokio::test]
async fn copy_mode_flips_in_mode_and_back() {
    let Some(srv) = TestServer::new("copymode") else {
        return;
    };
    srv.new_session("cm");

    let w = SessionWatcher::connect(srv.config("cm"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    srv.tmux_ok(&["copy-mode", "-t", "%0"]);
    await_event(&mut rx, "PaneChanged(in_mode=true)", |e| {
        matches!(e, PaneEvent::PaneChanged { changed, row, .. }
            if changed.contains(&PaneField::InMode) && row.in_mode)
    })
    .await;
    assert!(w.pane("%0").unwrap().in_mode);

    srv.tmux_ok(&["send-keys", "-t", "%0", "-X", "cancel"]);
    await_event(&mut rx, "PaneChanged(in_mode=false)", |e| {
        matches!(e, PaneEvent::PaneChanged { changed, row, .. }
            if changed.contains(&PaneField::InMode) && !row.in_mode)
    })
    .await;
    assert!(!w.pane("%0").unwrap().in_mode);

    w.shutdown().await;
}

#[tokio::test]
async fn short_lived_command_flips_pane_dead_with_remain_on_exit() {
    let Some(srv) = TestServer::new("deadpane") else {
        return;
    };
    srv.new_session("dp");
    // Keep exited panes around so pane_dead is observable.
    srv.tmux_ok(&["set-option", "-g", "remain-on-exit", "on"]);

    let w = SessionWatcher::connect(srv.config("dp"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    // Long enough to be seen alive first, short enough to die during the
    // test. Death has no dedicated notification; the pane_dead flip arrives
    // through the per-pane subscription.
    srv.tmux_ok(&["split-window", "-t", "dp", "sleep 0.5"]);
    await_event(
        &mut rx,
        "PaneAdded(%1)",
        |e| matches!(e, PaneEvent::PaneAdded(r) if r.pane_id == "%1"),
    )
    .await;

    await_event(&mut rx, "PaneChanged(dead=true) or PaneAdded dead", |e| {
        matches!(e, PaneEvent::PaneChanged { id, changed, row }
            if id == "%1" && changed.contains(&PaneField::Dead) && row.dead)
    })
    .await;
    assert!(w.pane("%1").unwrap().dead);

    w.shutdown().await;
}
