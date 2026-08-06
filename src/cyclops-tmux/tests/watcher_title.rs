//! Title changes reach the row and emit PaneChanged, both via select-pane -T
//! from outside and via an OSC escape printed inside the pane. This is the
//! subscription path working end to end (MEASURED on tmux 3.6a).

mod common;

use common::{await_event, TestServer};
use cyclops_tmux::{PaneEvent, PaneField, SessionWatcher};

#[tokio::test]
async fn title_changes_update_row_and_emit_pane_changed() {
    let Some(srv) = TestServer::new("title") else {
        return;
    };
    srv.new_session("ti");

    let w = SessionWatcher::connect(srv.config("ti"))
        .await
        .expect("connect");
    let pid_at_bootstrap = w.pane("%0").expect("pane %0").pane_pid;
    assert!(pid_at_bootstrap > 0);
    let mut rx = w.subscribe();

    // From outside the pane.
    srv.tmux_ok(&["select-pane", "-t", "%0", "-T", "TITLE_OUTSIDE"]);
    let ev = await_event(&mut rx, "PaneChanged(title=TITLE_OUTSIDE)", |e| {
        matches!(e, PaneEvent::PaneChanged { changed, row, .. }
            if changed.contains(&PaneField::Title) && row.title == "TITLE_OUTSIDE")
    })
    .await;
    let PaneEvent::PaneChanged { id, .. } = ev else {
        unreachable!()
    };
    assert_eq!(id, "%0");
    assert_eq!(w.pane("%0").unwrap().title, "TITLE_OUTSIDE");

    // From inside the pane, via OSC 2 (the path agent CLIs use, F5/F6).
    srv.tmux_ok(&[
        "send-keys",
        "-t",
        "%0",
        "printf '\\033]2;TITLE_OSC\\007'",
        "Enter",
    ]);
    await_event(&mut rx, "PaneChanged(title=TITLE_OSC)", |e| {
        matches!(e, PaneEvent::PaneChanged { changed, row, .. }
            if changed.contains(&PaneField::Title) && row.title == "TITLE_OSC")
    })
    .await;
    let row = w.pane("%0").unwrap();
    assert_eq!(row.title, "TITLE_OSC");
    // The subscription push carries pane_pid as its last field; a garbled
    // parse would have shifted or zeroed it.
    assert_eq!(row.pane_pid, pid_at_bootstrap);

    w.shutdown().await;
}
