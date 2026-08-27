//! The workspace's tmux geometry contract on an isolated rig: the canvas
//! size a control client declares is the size tmux lays panes out for,
//! held through chrome-change redeclarations and against a second attached
//! client (F48). Sizes mirror a 200x50 terminal with the default 22-column
//! sidebar; the canvas math itself is unit-tested in `render::canvas`.

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

    fn pane_heights(&self, session: &str) -> Vec<u16> {
        let out = self
            .server
            .run(&["list-panes", "-t", session, "-F", "#{pane_height}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().parse().expect("pane height"))
            .collect()
    }
}

/// Canvas cells always equal laid-out window cells, from attach through
/// every chrome change the workspace resizes for.
///
/// The workspace sizes windows with `resize-window` under `window-size
/// manual`, not by declaring a client size: a declaration is a vote, and
/// under any voting policy some other client can outvote it and reshape
/// every pane in the session (F76).
#[tokio::test]
async fn window_tracks_every_canvas_change() {
    let Some(rig) = Rig::new("geometry") else {
        return;
    };
    rig.session("geo");

    let (client, _notif) = ControlClient::spawn(rig.config("geo"))
        .await
        .expect("attach");
    client
        .capture_prior_window_size("@0")
        .await
        .expect("capture");
    client.pin_window_size_manual("@0").await.expect("pin");

    // Attach: 200x50 terminal, 22-column sidebar, tab bar, one-cell margin.
    client.resize_window("@0", 176, 47).await.expect("size");
    assert_eq!(rig.window_size("geo"), (176, 47));
    assert_eq!(rig.pane_widths("geo"), vec![176]);

    // A split spends one tmux separator column; pane totals stay inside
    // the declared width.
    rig.server.run_ok(&["split-window", "-h", "-t", "geo"]);
    assert_eq!(rig.window_size("geo").0, 176);
    assert_eq!(rig.pane_widths("geo").iter().sum::<u16>(), 175);

    // A wider canvas, the shape a sidebar collapse produces. The numbers
    // here are driven directly rather than derived from a chrome split,
    // because this test pins the tmux side of the contract; the split
    // itself is pinned by the chrome tests in app.rs.
    client
        .resize_window("@0", 198, 47)
        .await
        .expect("sidebar hidden");
    assert_eq!(rig.window_size("geo"), (198, 47));
    assert_eq!(rig.pane_widths("geo").iter().sum::<u16>(), 197);
    client
        .resize_window("@0", 176, 47)
        .await
        .expect("sidebar shown");
    assert_eq!(rig.window_size("geo"), (176, 47));

    // Sidebar dragged to 30 columns.
    client
        .resize_window("@0", 168, 47)
        .await
        .expect("sidebar drag");
    assert_eq!(rig.window_size("geo"), (168, 47));

    // Terminal resized to 220x55.
    client
        .resize_window("@0", 196, 52)
        .await
        .expect("terminal resize");
    assert_eq!(rig.window_size("geo"), (196, 52));
    assert_eq!(rig.pane_widths("geo").iter().sum::<u16>(), 195);

    client.shutdown().await;
}

/// The regression this exists for, and Admin measured: no other client can
/// move a window this workspace owns, whichever direction it would move it.
///
/// Under every voting policy the session's size is decided by whoever
/// attached, so a 62x21 terminal opened next to a 176x47 workspace took the
/// window, and every agent pane in it, down with it (F76). A window a
/// workspace owns is on `manual`, which has no vote to lose.
#[tokio::test]
async fn no_other_client_can_move_an_owned_window() {
    let Some(rig) = Rig::new("geometry-2c") else {
        return;
    };
    rig.session("geo2");

    let (workspace, _n1) = ControlClient::spawn(rig.config("geo2"))
        .await
        .expect("attach");
    workspace
        .capture_prior_window_size("@0")
        .await
        .expect("capture");
    workspace.pin_window_size_manual("@0").await.expect("pin");
    workspace.resize_window("@0", 176, 47).await.expect("size");
    assert_eq!(rig.window_size("geo2"), (176, 47));

    // A second client attaches and declares, both larger and smaller. Under
    // `latest` the first would have taken the window; under `smallest` the
    // second would. Neither does.
    let (viewer, _n2) = ControlClient::spawn(rig.config("geo2"))
        .await
        .expect("attach");
    viewer.set_client_size(240, 58).await.expect("declare");
    assert_eq!(
        rig.window_size("geo2"),
        (176, 47),
        "a larger viewer moved it"
    );
    viewer.set_client_size(60, 20).await.expect("redeclare");
    assert_eq!(
        rig.window_size("geo2"),
        (176, 47),
        "a smaller viewer collapsed the owner's session"
    );
    assert_eq!(rig.pane_widths("geo2"), vec![176]);

    // And the viewer leaving changes nothing either, because it never had
    // a say.
    viewer.shutdown().await;
    assert_eq!(rig.window_size("geo2"), (176, 47));

    workspace.shutdown().await;
}

/// When a small owner (e.g. 152x45) collapses a stacked pane to 1 row due
/// to window resize compression (without deliberate user minimization),
/// followers must never resize the shared window. When the owner exits,
/// the successor owner (e.g. 271x61) takes over sizing authority, recomputes
/// the canvas, and uncrushes the compressed pane so no active pane is
/// accidentally trapped at 1 row.
#[tokio::test]
async fn successor_owner_recomputes_full_canvas_and_uncrushes_compressed_panes() {
    let Some(rig) = Rig::new("geometry-uncrush") else {
        return;
    };
    rig.session("geo3");
    // Split vertically so we have two stacked panes in @0
    rig.server.run_ok(&["split-window", "-v", "-t", "geo3"]);

    // Predecessor Owner A attaches, captures prior window size, pins manual, and claims driver
    let (owner, _n1) = ControlClient::spawn(rig.config("geo3"))
        .await
        .expect("owner attach");
    owner
        .capture_prior_window_size("@0")
        .await
        .expect("capture");
    owner.pin_window_size_manual("@0").await.expect("pin");

    // Owner A sizes window down to 229x20, compressing the bottom pane to 1 row
    owner
        .resize_window("@0", 229, 20)
        .await
        .expect("owner size");
    rig.server.run_ok(&["resize-pane", "-t", "%1", "-y", "1"]);
    assert_eq!(rig.window_size("geo3"), (229, 20));
    assert_eq!(
        rig.pane_heights("geo3"),
        vec![18, 1],
        "bottom pane is compressed down to 1 row"
    );

    // Follower B attaches with a large 271x61 terminal (declaring full 267x58 canvas)
    let (follower, _n2) = ControlClient::spawn(rig.config("geo3"))
        .await
        .expect("follower attach");
    follower
        .set_client_size(267, 58)
        .await
        .expect("follower declare");

    // Invariant: Follower must NOT resize the shared window while owner is live
    assert_eq!(
        rig.window_size("geo3"),
        (229, 20),
        "follower must never resize the shared window while owner is live"
    );
    assert_eq!(rig.pane_heights("geo3"), vec![18, 1]);

    // Owner A drops / exits
    owner.shutdown().await;

    // Follower B adopts @0, takes over driver, and expands window to full 267x58 canvas
    follower
        .resize_window("@0", 267, 58)
        .await
        .expect("successor resize");
    assert_eq!(
        rig.window_size("geo3"),
        (267, 58),
        "successor owner must consume its own full 271x61 client dimensions rather than stopping at 229x58"
    );

    // When successor recomputes layout, the compressed pane (without deliberate minimization provenance)
    // must NOT remain trapped at 1 row!
    let heights = rig.pane_heights("geo3");
    assert!(
        heights[1] >= 5,
        "compressed stacked pane must be uncrushed upon authority transfer, got heights: {heights:?}"
    );

    follower.shutdown().await;
}

/// A deliberately minimized pane records provenance in the tmux pane option.
/// When authority transfers to a successor owner, the pane remains minimized
/// at 1 row, and its restore provenance is preserved.
#[tokio::test]
async fn deliberate_minimization_provenance_survives_authority_transfer() {
    let Some(rig) = Rig::new("geometry-min-prov") else {
        return;
    };
    rig.session("geo4");
    rig.server.run_ok(&["split-window", "-v", "-t", "geo4"]);

    let (owner, _n1) = ControlClient::spawn(rig.config("geo4"))
        .await
        .expect("owner attach");
    owner
        .capture_prior_window_size("@0")
        .await
        .expect("capture");
    owner.pin_window_size_manual("@0").await.expect("pin");
    owner.resize_window("@0", 176, 47).await.expect("size");

    // Deliberately minimize pane %1, recording pre-minimize height (e.g. 23) in pane option
    rig.server.run_ok(&[
        "set-option",
        "-p",
        "-t",
        "%1",
        "@cyclops_pane_minimized",
        "23",
    ]);
    rig.server.run_ok(&["resize-pane", "-t", "%1", "-y", "1"]);
    assert_eq!(rig.pane_heights("geo4"), vec![45, 1]);

    // Owner exits
    owner.shutdown().await;

    // Follower takes over and resizes window to larger canvas
    let (successor, _n2) = ControlClient::spawn(rig.config("geo4"))
        .await
        .expect("successor attach");
    successor
        .resize_window("@0", 240, 58)
        .await
        .expect("successor resize");

    // Provenance is preserved on the pane
    let out = rig
        .server
        .run(&["show-options", "-p", "-t", "%1", "@cyclops_pane_minimized"]);
    let prov = String::from_utf8_lossy(&out.stdout);
    assert!(
        prov.contains("23"),
        "minimization provenance must be retained: {prov}"
    );

    successor.shutdown().await;
}
