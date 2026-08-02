//! Output activity events after send-keys into a shell pane. With the
//! pause-after flow-control flag set at attach, tmux delivers output as
//! %extended-output (MEASURED on 3.6a), so this also proves that path.

mod common;

use common::{await_event, TestServer};
use cyclops_tmux::{PaneEvent, SessionWatcher};

#[tokio::test]
async fn output_activity_arrives_after_send_keys() {
    let Some(srv) = TestServer::new("output") else {
        return;
    };
    srv.new_session("out");

    let w = SessionWatcher::connect(srv.config("out"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    srv.tmux_ok(&["send-keys", "-t", "%0", "echo CYOUT_MARK", "Enter"]);
    let ev = await_event(
        &mut rx,
        "OutputActivity(%0)",
        |e| matches!(e, PaneEvent::OutputActivity { pane_id, .. } if pane_id == "%0"),
    )
    .await;
    let PaneEvent::OutputActivity { ts, .. } = ev else {
        unreachable!()
    };
    assert!(ts > 0, "activity timestamp is populated");

    w.shutdown().await;
}
