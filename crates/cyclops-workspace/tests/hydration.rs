//! Hydration correctness on an isolated tmux rig (workspace Step 3).

use std::time::Duration;

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::{ControlClient, ControlConfig};
use cyclops_workspace::{snapshot_from_bundle, PaneRuntime};

struct Rig {
    server: TmuxServer,
}

impl Rig {
    fn new(tag: &str) -> Option<Self> {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return None;
        }
        Some(Self {
            server: TmuxServer::new(tag),
        })
    }

    fn config(&self, session: &str) -> ControlConfig {
        ControlConfig::attach(session)
            .on_socket(self.server.socket())
            .with_config_file("/dev/null")
    }

    fn session(&self, name: &str) {
        self.server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
    }
}

fn norm_row(s: &str) -> String {
    s.trim().to_string()
}

fn rows_match_grid_and_capture(grid: &[String], capture: &str) {
    let cap_lines: Vec<String> = capture.lines().map(norm_row).collect();
    let grid_lines: Vec<String> = grid.iter().map(|l| norm_row(l)).collect();
    assert_eq!(
        grid_lines.len(),
        cap_lines.len(),
        "row count mismatch grid vs capture"
    );
    for (i, (g, c)) in grid_lines.iter().zip(cap_lines.iter()).enumerate() {
        assert_eq!(g, c, "row {i} differs: grid={g:?} capture={c:?}");
    }
}

#[tokio::test]
async fn hydrated_grid_matches_plain_capture() {
    let Some(rig) = Rig::new("hydrate") else {
        return;
    };
    rig.session("hyd");

    let (client, _notif) = ControlClient::spawn(rig.config("hyd"))
        .await
        .expect("attach");
    client.set_window_size_latest().await.expect("window-size");
    client.set_client_size(80, 24).await.expect("client size");

    rig.server.run_ok(&[
        "send-keys",
        "-t",
        "%0",
        r"printf '\033[H\033[2JHYDRATE_MARK\n'",
        "Enter",
    ]);
    rig.server.wait_screen("%0", "HYDRATE_MARK");

    let bundle = client.hydrate_pane("%0").await.expect("hydrate bundle");
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.hydrate(&snapshot_from_bundle(&bundle));

    let cap = rig.server.capture("%0");
    rows_match_grid_and_capture(&runtime.grid().row_texts(), &cap);

    client.shutdown().await;
}

#[tokio::test]
async fn resize_then_rehydrate_still_matches_capture() {
    let Some(rig) = Rig::new("hydr-resize") else {
        return;
    };
    rig.session("hrsz");

    let (client, _notif) = ControlClient::spawn(rig.config("hrsz"))
        .await
        .expect("attach");
    client.set_window_size_latest().await.expect("window-size");

    rig.server.run_ok(&[
        "send-keys",
        "-t",
        "%0",
        r"printf '\033[H\033[2JRESIZE_OK\n'",
        "Enter",
    ]);
    rig.server.wait_screen("%0", "RESIZE_OK");

    rig.server
        .run_ok(&["resize-pane", "-t", "%0", "-x", "60", "-y", "20"]);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let bundle = client.hydrate_pane("%0").await.expect("hydrate");
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.hydrate(&snapshot_from_bundle(&bundle));

    rows_match_grid_and_capture(&runtime.grid().row_texts(), &rig.server.capture("%0"));

    client.shutdown().await;
}

#[tokio::test]
async fn mid_stream_rehydrate_matches_capture() {
    let Some(rig) = Rig::new("hydr-mid") else {
        return;
    };
    rig.session("hmid");

    let (client, _notif) = ControlClient::spawn(rig.config("hmid"))
        .await
        .expect("attach");

    rig.server.run_ok(&[
        "send-keys",
        "-t",
        "%0",
        r"printf '\033[H\033[2J'; yes MID_STREAM | head -n 20",
        "Enter",
    ]);
    rig.server.wait_screen("%0", "MID_STREAM");

    let bundle = client.hydrate_pane("%0").await.expect("mid hydrate");
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.rehydrate(&snapshot_from_bundle(&bundle));

    let cap = rig.server.capture("%0");
    assert!(cap.contains("MID_STREAM"));
    assert!(
        runtime
            .grid()
            .row_texts()
            .iter()
            .any(|l| l.contains("MID_STREAM")),
        "rehydrated grid missing mid-stream content"
    );

    client.shutdown().await;
}
