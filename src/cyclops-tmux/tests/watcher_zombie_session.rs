//! Killing the watched session does not always disconnect this client: on
//! the live system tmux switched it to a surviving session instead of
//! detaching it. `detach-on-destroy off` (tmux's own name for that choice)
//! reproduces the same switch-instead-of-detach behavior on an isolated
//! server; this file sets it explicitly rather than assume the default
//! matches whatever the live server has configured. The zombie watcher
//! then keeps receiving the survivor's notifications, and every reconcile
//! it drives against the dead session name fails the same way forever
//! unless the watcher probes for the session's death itself and tears
//! down.

mod common;

use common::{await_event, TestServer};
use cyclops_tmux::{PaneEvent, SessionWatcher};

#[tokio::test]
async fn zombie_watcher_disconnects_after_its_session_dies() {
    let Some(srv) = TestServer::new("zombie") else {
        return;
    };
    srv.new_session("dies");
    srv.new_session("survivor");
    // Forces the switch-instead-of-detach behavior this test is about:
    // tmux's own default (confirmed on this build) detaches the client
    // instead, which is a different, already-handled case.
    srv.tmux_ok(&["set-option", "-g", "detach-on-destroy", "off"]);

    let w = SessionWatcher::connect(srv.config("dies"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    srv.tmux_ok(&["kill-session", "-t", "=dies"]);

    // The now-zombie client is attached to "survivor"; output there is
    // exactly what used to arm a hint-reconcile against the dead session
    // name forever, with nothing ever noticing. It must instead surface
    // here as a bounded Disconnected.
    srv.tmux_ok(&["send-keys", "-t", "survivor", "echo hi", "Enter"]);

    await_event(&mut rx, "Disconnected", |e| {
        matches!(e, PaneEvent::Disconnected)
    })
    .await;

    w.shutdown().await;
}

/// The other reconcile-error path: an explicit doubt via `reconcile_now`
/// rather than a hint's debounced one. Its own reply is whatever the
/// failed `list-panes` said (a command error, not `Disconnected` — the
/// probe runs after the ack, not in place of it), and the broadcast
/// confirms the same teardown follows it.
#[tokio::test]
async fn zombie_watcher_disconnects_after_an_explicit_reconcile_too() {
    let Some(srv) = TestServer::new("zombie-explicit") else {
        return;
    };
    srv.new_session("dies");
    srv.new_session("survivor");
    srv.tmux_ok(&["set-option", "-g", "detach-on-destroy", "off"]);

    let w = SessionWatcher::connect(srv.config("dies"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    srv.tmux_ok(&["kill-session", "-t", "=dies"]);

    let res = w.reconcile_now().await;
    assert!(
        res.is_err(),
        "list-panes against the dead session name must fail: {res:?}"
    );

    await_event(&mut rx, "Disconnected", |e| {
        matches!(e, PaneEvent::Disconnected)
    })
    .await;

    w.shutdown().await;
}
