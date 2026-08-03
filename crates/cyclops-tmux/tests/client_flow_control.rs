//! The pause-after client flag is accepted on the installed tmux
//! (validation amendment a), and new-session control mode works.

mod common;

use common::TestServer;
use cyclops_tmux::{ControlClient, ControlConfig};

#[tokio::test]
async fn pause_after_flag_is_accepted() {
    let Some(srv) = TestServer::new("flowctl") else {
        return;
    };
    // NewSession mode: the control client creates the session itself.
    let cfg = ControlConfig::new_session("fc")
        .on_socket(srv.sock().to_string())
        .with_config_file("/dev/null");
    let (client, _notif) = ControlClient::spawn(cfg).await.expect("spawn");

    // spawn already set the flag as its handshake; setting it again must
    // succeed as a plain command reply, proving the server accepts it.
    client
        .command("refresh-client -f pause-after=300")
        .await
        .expect("pause-after accepted");

    // The session exists on the isolated server.
    let out = client
        .command("display-message -p '#{session_name}'")
        .await
        .expect("display");
    assert_eq!(out, vec!["fc".to_string()]);

    client.shutdown().await;
}
