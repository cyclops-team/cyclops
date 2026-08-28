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

/// Send a shell command and wait for text that only the command's output can
/// produce. If the marker appears in the command itself, `wait_screen` can
/// observe the shell's echo before the command has run.
fn send_and_wait_for_output(rig: &Rig, target: &str, command: &str, marker: &str) {
    assert!(
        !command.contains(marker),
        "output marker {marker:?} must not appear in the echoed command"
    );
    rig.server
        .run_ok(&["send-keys", "-t", target, command, "Enter"]);
    rig.server.wait_screen(target, marker);
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
    client
        .set_window_size_smallest("@0")
        .await
        .expect("window-size");
    client.set_client_size(80, 24).await.expect("client size");

    send_and_wait_for_output(
        &rig,
        "%0",
        r"printf '\033[H\033[2JHYDRATE_''MARK\n'",
        "HYDRATE_MARK",
    );

    let bundle = client.hydrate_pane("%0").await.expect("hydrate bundle");
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.hydrate(&snapshot_from_bundle(&bundle));

    let cap = rig.server.capture("%0");
    rows_match_grid_and_capture(&runtime.snapshot().row_texts(), &cap);

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
    client
        .set_window_size_smallest("@0")
        .await
        .expect("window-size");

    send_and_wait_for_output(
        &rig,
        "%0",
        r"printf '\033[H\033[2JRESIZE_''OK\n'",
        "RESIZE_OK",
    );

    rig.server
        .run_ok(&["resize-pane", "-t", "%0", "-x", "60", "-y", "20"]);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let bundle = client.hydrate_pane("%0").await.expect("hydrate");
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.hydrate(&snapshot_from_bundle(&bundle));

    rows_match_grid_and_capture(&runtime.snapshot().row_texts(), &rig.server.capture("%0"));

    client.shutdown().await;
}

/// A full-screen agent TUI lives in the alternate screen. Hydration must
/// restore what the user is looking at, not the shell content tmux saved
/// when the TUI started.
#[tokio::test]
async fn hydrating_a_pane_in_the_alternate_screen_restores_what_the_user_sees() {
    let Some(rig) = Rig::new("hydr-alt") else {
        return;
    };
    rig.session("halt");

    let (client, _notif) = ControlClient::spawn(rig.config("halt"))
        .await
        .expect("attach");
    client
        .set_window_size_smallest("@0")
        .await
        .expect("window-size");
    client.set_client_size(80, 24).await.expect("client size");

    // Mark the primary screen, then enter the alternate screen and paint a
    // different marker there — the shape every shipped agent TUI has.
    send_and_wait_for_output(
        &rig,
        "%0",
        r"printf '\033[H\033[2JPRIMARY_''SHELL\n'",
        "PRIMARY_SHELL",
    );
    send_and_wait_for_output(
        &rig,
        "%0",
        r"printf '\033[?1049h\033[H\033[2JALT_TUI_''SCREEN\n'",
        "ALT_TUI_SCREEN",
    );

    let bundle = client.hydrate_pane("%0").await.expect("hydrate bundle");
    assert!(
        bundle.alternate_on,
        "tmux must report the pane as being on the alternate screen"
    );

    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.hydrate(&snapshot_from_bundle(&bundle));
    let rows = runtime.snapshot().row_texts();

    assert!(
        rows.iter().any(|l| l.contains("ALT_TUI_SCREEN")),
        "hydration must show the alternate screen the user is looking at, \
         got {rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("PRIMARY_SHELL")),
        "the saved primary screen must not be painted over the live TUI, \
         got {rows:?}"
    );

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

    send_and_wait_for_output(
        &rig,
        "%0",
        // Keep the rendered marker out of the echoed command. The wait must
        // observe program output, not text the shell is about to clear.
        r"printf '\033[H\033[2J'; yes MID_''STREAM | head -n 20",
        "MID_STREAM",
    );

    let bundle = client.hydrate_pane("%0").await.expect("mid hydrate");
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.rehydrate(&snapshot_from_bundle(&bundle));

    let cap = rig.server.capture("%0");
    assert!(cap.contains("MID_STREAM"));
    assert!(
        runtime
            .snapshot()
            .row_texts()
            .iter()
            .any(|l| l.contains("MID_STREAM")),
        "rehydrated grid missing mid-stream content"
    );

    client.shutdown().await;
}
