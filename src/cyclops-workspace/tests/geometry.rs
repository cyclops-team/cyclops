//! The workspace's tmux geometry contract on an isolated rig: the canvas
//! size a control client declares is the size tmux lays panes out for,
//! held through chrome-change redeclarations and against a second attached
//! client (F48). Sizes mirror a 200x50 terminal with the default 22-column
//! sidebar; the canvas math itself is unit-tested in `render::canvas`.

use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::{ControlClient, ControlConfig};

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

    /// One session, one window, no status line: with the status row out of
    /// the way, a declared client size maps 1:1 onto the window size.
    fn session(&self, name: &str) {
        self.server
            .run_ok(&["new-session", "-d", "-s", name, "/bin/sh"]);
        self.server.run_ok(&["set-option", "-g", "status", "off"]);
    }

    fn window_size(&self, session: &str) -> (u16, u16) {
        let out = self.server.run(&[
            "display-message",
            "-p",
            "-t",
            session,
            "#{window_width}x#{window_height}",
        ]);
        let text = String::from_utf8_lossy(&out.stdout);
        let (w, h) = text.trim().split_once('x').expect("WxH");
        (w.parse().expect("width"), h.parse().expect("height"))
    }

    fn pane_widths(&self, session: &str) -> Vec<u16> {
        let out = self
            .server
            .run(&["list-panes", "-t", session, "-F", "#{pane_width}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse().expect("pane width"))
            .collect()
    }

    /// Bounded wait for the window to settle on `expected`. Detach-driven
    /// recalculation lands after the leaving client's EOF, which has no
    /// reply to await.
    fn wait_window_size(&self, session: &str, expected: (u16, u16)) {
        let t = Instant::now();
        while self.window_size(session) != expected {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "window never settled on {expected:?}, still {:?}",
                self.window_size(session)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Declared canvas cells always equal laid-out window cells, from attach
/// through every chrome change the workspace redeclares for.
#[tokio::test]
async fn window_tracks_every_canvas_redeclaration() {
    let Some(rig) = Rig::new("geometry") else {
        return;
    };
    rig.session("geo");

    let (client, _notif) = ControlClient::spawn(rig.config("geo"))
        .await
        .expect("attach");
    client.set_window_size_smallest("@0").await.expect("pin");

    // Attach: 200x50 terminal, 22-column sidebar, tab bar, one-cell margin.
    client.set_client_size(176, 47).await.expect("declare");
    assert_eq!(rig.window_size("geo"), (176, 47));
    assert_eq!(rig.pane_widths("geo"), vec![176]);

    // A split spends one tmux separator column; pane totals stay inside
    // the declared width.
    rig.server.run_ok(&["split-window", "-h", "-t", "geo"]);
    assert_eq!(rig.window_size("geo").0, 176);
    assert_eq!(rig.pane_widths("geo").iter().sum::<u16>(), 175);

    // Sidebar collapsed: its 22 columns go back to the canvas, then it
    // reopens and takes them again.
    client
        .set_client_size(198, 47)
        .await
        .expect("sidebar hidden");
    assert_eq!(rig.window_size("geo"), (198, 47));
    assert_eq!(rig.pane_widths("geo").iter().sum::<u16>(), 197);
    client
        .set_client_size(176, 47)
        .await
        .expect("sidebar shown");
    assert_eq!(rig.window_size("geo"), (176, 47));

    // Sidebar dragged to 30 columns.
    client.set_client_size(168, 47).await.expect("sidebar drag");
    assert_eq!(rig.window_size("geo"), (168, 47));

    // Terminal resized to 220x55.
    client
        .set_client_size(196, 52)
        .await
        .expect("terminal resize");
    assert_eq!(rig.window_size("geo"), (196, 52));
    assert_eq!(rig.pane_widths("geo").iter().sum::<u16>(), 195);

    client.shutdown().await;
}

/// The regression the fix exists for: under the previous `latest` policy a
/// second attached client that declared a larger size took the window with
/// it, and panes laid out wider than the first client's painted canvas.
/// Under `smallest` the window never exceeds the workspace canvas; a
/// smaller viewer shrinks it, which the canvas absorbs as gutter.
#[tokio::test]
async fn a_second_larger_client_cannot_out_size_the_canvas() {
    let Some(rig) = Rig::new("geometry-2c") else {
        return;
    };
    rig.session("geo2");

    let (workspace, _n1) = ControlClient::spawn(rig.config("geo2"))
        .await
        .expect("attach");
    workspace.set_window_size_smallest("@0").await.expect("pin");
    workspace.set_client_size(176, 47).await.expect("declare");
    assert_eq!(rig.window_size("geo2"), (176, 47));

    // A bigger viewer attaches and declares: the window must not grow past
    // the workspace canvas.
    let (viewer, _n2) = ControlClient::spawn(rig.config("geo2"))
        .await
        .expect("attach");
    viewer.set_client_size(240, 58).await.expect("declare");
    assert_eq!(rig.window_size("geo2"), (176, 47));
    assert_eq!(rig.pane_widths("geo2"), vec![176]);

    // A smaller viewer wins instead: panes stay inside every canvas.
    viewer.set_client_size(150, 40).await.expect("redeclare");
    assert_eq!(rig.window_size("geo2"), (150, 40));

    // The viewer leaves; the workspace canvas is authoritative again.
    viewer.shutdown().await;
    rig.wait_window_size("geo2", (176, 47));

    workspace.shutdown().await;
}
