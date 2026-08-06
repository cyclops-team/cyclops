//! Hydration bundle adapter tests (workspace Step 3).

mod common;

use common::TestServer;
use cyclops_tmux::ControlClient;

#[tokio::test]
async fn hydrate_pane_returns_metadata_and_captures() {
    let Some(srv) = TestServer::new("hydr-bundle") else {
        return;
    };
    srv.new_session("hb");

    let (client, _notif) = ControlClient::spawn(srv.config("hb")).await.expect("spawn");

    client
        .set_window_size_smallest("@0")
        .await
        .expect("smallest");
    client.set_client_size(120, 30).await.expect("client size");

    srv.tmux_ok(&["send-keys", "-t", "%0", "echo BUNDLE_OK", "Enter"]);
    common::eventually("bundle text", || {
        srv.tmux(&["capture-pane", "-p", "-t", "%0"])
            .stdout
            .windows(9)
            .any(|w| w == b"BUNDLE_OK")
    })
    .await;

    let bundle = client.hydrate_pane("%0").await.expect("bundle");
    assert!(bundle.cols > 0 && bundle.rows > 0);
    assert!(!bundle.visible_escaped.is_empty());
    let visible = String::from_utf8_lossy(&bundle.visible_escaped);
    assert!(
        visible.contains("BUNDLE_OK"),
        "visible capture missing content: {visible}"
    );

    client.shutdown().await;
}
