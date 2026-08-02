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
    assert_eq!(w.pane("%0").unwrap().title, "TITLE_OSC");

    w.shutdown().await;
}
