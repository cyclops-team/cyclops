//! pane_in_mode and pane_dead flips reach the table via events.

mod common;

use common::{await_event, TestServer};
use cyclops_tmux::{PaneEvent, PaneField, SessionWatcher};
use std::time::Duration;

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
    // Take away the coincidence this test used to pass on. tmux's automatic
    // rename fires when the command exits, and that %window-renamed forced
    // the reconcile that noticed the death: a race the watcher never
    // arbitrated, which 3.6a won by 23ms and next-3.8 lost by 13ms (F25).
    // With automatic-rename off, nothing else moves when the pane dies, so
    // what is under test is cyclops's own dead edge and not tmux's timing.
    srv.tmux_ok(&["set-option", "-g", "automatic-rename", "off"]);

    let w = SessionWatcher::connect(srv.config("dp"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    // Long enough to be seen alive first, short enough to die during the
    // test.
    srv.tmux_ok(&["split-window", "-t", "dp", "sleep 0.5"]);
    await_event(
        &mut rx,
        "PaneAdded(%1)",
        |e| matches!(e, PaneEvent::PaneAdded(r) if r.pane_id == "%1"),
    )
    .await;

    await_event(&mut rx, "PaneChanged(dead=true)", |e| {
        matches!(e, PaneEvent::PaneChanged { id, changed, row }
            if id == "%1" && changed.contains(&PaneField::Dead) && row.dead)
    })
    .await;
    // The corpse stays in the table. next-3.8 stops reporting #{pane_pid}
    // for a dead pane, and a row dropped for that reads as a removal.
    assert!(w.pane("%1").unwrap().dead);

    w.shutdown().await;
}

#[tokio::test]
async fn respawn_emits_the_replacement_process_generation() {
    let Some(srv) = TestServer::new("respawn-pid") else {
        return;
    };
    srv.new_session("rp");
    // Isolate the process-generation edge. A pending automatic rename can
    // otherwise join the respawn event on newer tmux builds.
    srv.tmux_ok(&["set-option", "-g", "automatic-rename", "off"]);
    srv.tmux_ok(&["respawn-pane", "-k", "-t", "%0", "sleep 60"]);

    let w = SessionWatcher::connect(srv.config("rp"))
        .await
        .expect("connect");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if w.pane("%0")
                .is_some_and(|row| row.current_command == "sleep")
            {
                break;
            }
            w.reconcile_now().await.expect("refresh initial command");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial sleep process did not become authoritative");
    let first = w.pane("%0").expect("pane %0").pane_pid;
    let mut rx = w.subscribe();

    srv.tmux_ok(&["respawn-pane", "-k", "-t", "%0", "sleep 60"]);
    let event = await_event(&mut rx, "PaneChanged(pane_pid)", |event| {
        matches!(event, PaneEvent::PaneChanged { id, changed, row }
            if id == "%0"
                && changed.contains(&PaneField::PanePid)
                && row.pane_pid > 0
                && row.pane_pid != first)
    })
    .await;
    let PaneEvent::PaneChanged { changed, row, .. } = event else {
        unreachable!()
    };
    assert_eq!(
        changed,
        vec![PaneField::PanePid],
        "same-command respawn must be a process-generation edge only"
    );
    assert_eq!(w.pane("%0").expect("pane %0").pane_pid, row.pane_pid);

    w.shutdown().await;
}
