//! A watcher should follow a rename of its OWN session (resolved by tmux's
//! stable `$id`, F37) instead of tearing down the way it does today for a
//! session that stops resolving (`watcher_zombie_session.rs`). This file
//! covers the two halves: a rename of the watched session itself keeps
//! events flowing under the new name with no gap, and a rename of a
//! DIFFERENT session — including the survivor a zombie client gets
//! switched onto — must never resurrect a dead watcher. That second case
//! is the revival guard the id check exists for; it is distinct from, and
//! must not weaken, the teardown `watcher_zombie_session.rs` already
//! proves.

mod common;

use common::{await_event, TestServer};
use cyclops_tmux::{PaneEvent, SessionWatcher};

#[tokio::test]
async fn a_renamed_session_keeps_flowing_under_the_new_name() {
    let Some(srv) = TestServer::new("rename-follow") else {
        return;
    };
    srv.new_session("before");

    let w = SessionWatcher::connect(srv.config("before"))
        .await
        .expect("connect");
    assert!(
        w.session_id().is_some(),
        "the connect-time id probe must resolve on a live session"
    );
    let mut rx = w.subscribe();

    srv.tmux_ok(&["rename-session", "-t", "=before", "after"]);

    let renamed = await_event(&mut rx, "SessionRenamed", |e| {
        matches!(e, PaneEvent::SessionRenamed { .. })
    })
    .await;
    let PaneEvent::SessionRenamed { name } = renamed else {
        unreachable!()
    };
    assert_eq!(name, "after");
    assert_eq!(
        w.session(),
        "after",
        "the watcher's own idea of its session name must follow the rename"
    );

    // Prove the internal target actually moved, not just the getter: a
    // split issued against the NEW name only reaches this watcher's
    // `list-panes -s -t` if that is what it is targeting now.
    srv.tmux_ok(&["split-window", "-t", "after", "/bin/sh"]);
    let added = await_event(&mut rx, "PaneAdded", |e| {
        matches!(e, PaneEvent::PaneAdded(_))
    })
    .await;
    let PaneEvent::PaneAdded(row) = added else {
        unreachable!()
    };
    assert_eq!(row.pane_id, "%1");
    assert_eq!(w.snapshot().len(), 2);

    w.shutdown().await;
}

#[tokio::test]
async fn renaming_a_different_session_does_not_move_this_watchers_target() {
    let Some(srv) = TestServer::new("rename-foreign") else {
        return;
    };
    srv.new_session("mine");
    srv.new_session("theirs");

    let w = SessionWatcher::connect(srv.config("mine"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    srv.tmux_ok(&["rename-session", "-t", "=theirs", "theirs-renamed"]);

    // The rename of "theirs" produces no SessionRenamed for this watcher.
    // Prove it the same way `watcher_events.rs` proves a positive: drive a
    // change this watcher DOES own and confirm that arrives first, with no
    // SessionRenamed ahead of it.
    srv.tmux_ok(&["split-window", "-t", "mine", "/bin/sh"]);
    let ev = await_event(&mut rx, "PaneAdded", |e| {
        matches!(e, PaneEvent::PaneAdded(_))
    })
    .await;
    assert!(
        matches!(ev, PaneEvent::PaneAdded(_)),
        "a foreign rename must not have queued a SessionRenamed ahead of this watcher's own events"
    );
    assert_eq!(
        w.session(),
        "mine",
        "a rename of a different session must not move this watcher's target"
    );

    w.shutdown().await;
}

/// The revival guard: a zombie client (`watcher_zombie_session.rs`'s
/// scenario, `detach-on-destroy off` switching the control client onto a
/// survivor instead of detaching it) must still tear down via
/// `PaneEvent::Disconnected` even when the survivor it landed on gets
/// renamed while the zombie is attached to it. The id check is what keeps
/// this from reading as "my session was renamed" and reviving the dead
/// watcher under the survivor's new name instead of disconnecting.
#[tokio::test]
async fn a_renamed_survivor_does_not_revive_a_zombie_watcher() {
    let Some(srv) = TestServer::new("rename-zombie") else {
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
    // The now-zombie client is attached to "survivor". Rename it before it
    // gets a chance to speak: if the id check were missing or wrong, this
    // is the rename that would incorrectly read as "my session, followed".
    srv.tmux_ok(&["rename-session", "-t", "=survivor", "survivor-renamed"]);
    srv.tmux_ok(&["send-keys", "-t", "survivor-renamed", "echo hi", "Enter"]);

    await_event(&mut rx, "Disconnected", |e| {
        matches!(e, PaneEvent::Disconnected)
    })
    .await;

    assert_eq!(
        w.session(),
        "dies",
        "a dead watcher must never adopt a survivor's name, renamed or not"
    );

    w.shutdown().await;
}
