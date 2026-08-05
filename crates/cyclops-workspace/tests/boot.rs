//! From-scratch boot: no tmux server at all → bare `cyclops` creates the
//! default session and renders its single pane.

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::{ControlClient, ControlConfig};
use cyclops_workspace::runtime::PaneRuntime;
use cyclops_workspace::snapshot_from_bundle;

#[tokio::test]
async fn create_mode_boots_server_session_and_pane() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    // A rig socket with no command run yet has no server behind it — this
    // is the empty-machine case bare `cyclops` must survive.
    let server = TmuxServer::new("scratch-boot");
    let cfg = ControlConfig::new_session("main")
        .on_socket(server.socket().to_string())
        .with_config_file("/dev/null");
    let (client, _notif) = ControlClient::spawn(cfg).await.expect("boot from scratch");

    let sessions = cyclops_tmux::list_sessions(Some(server.socket())).expect("server is up");
    assert_eq!(sessions.len(), 1, "one fresh session");
    assert_eq!(sessions[0].name, "main");
    assert_eq!(sessions[0].tab_count, 1, "one window");

    // The fresh pane hydrates like any other.
    let pane = cyclops_tmux::active_pane("main", Some(server.socket())).expect("active pane");
    let bundle = client.hydrate_pane(&pane).await.expect("hydrate");
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.hydrate(&snapshot_from_bundle(&bundle));
    assert_eq!(runtime.size(), (bundle.cols, bundle.rows));

    client.shutdown().await;
}
