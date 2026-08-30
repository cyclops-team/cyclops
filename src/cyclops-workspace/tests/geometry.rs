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

/// An unproven 1-row pane (with `None` provenance, e.g. manual operator `tmux resize-pane -y 1`)
/// fails closed: the successor owner will NOT blindly guess or auto-uncrush unknown intent.
#[tokio::test]
async fn unknown_intent_one_row_pane_fails_closed_and_refuses_auto_uncrush() {
    let Some(rig) = Rig::new("geometry-unknown-refusal") else {
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

    // Owner A sizes window down to 229x20, compressing the bottom pane to 1 row manually
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

    // Follower B adopts @0 and expands window to full 267x58 canvas
    follower
        .resize_window("@0", 267, 58)
        .await
        .expect("successor resize");
    assert_eq!(
        rig.window_size("geo3"),
        (267, 58),
        "successor owner must consume its own full client dimensions"
    );

    // With None provenance, successor must NOT blindly uncrush or guess height.
    let snap = follower.workspace_snapshot().await.expect("snapshot");
    assert_eq!(
        snap.sessions[0].windows[0].panes[1].minimization,
        cyclops_tmux::PaneMinimizationProvenance::None,
        "unproven 1-row pane has None provenance"
    );

    follower.shutdown().await;
}

/// A deliberately minimized pane records versioned provenance in `@cyclops_pane_minimized_v1`.
/// When authority transfers to a successor owner and the window resizes, the pane remains
/// explicitly collapsed at 1 row (resisting tmux automatic reflow), and restoring it
/// recovers the exact captured pre-collapse height and clears the option.
#[tokio::test]
async fn deliberately_minimized_pane_remains_collapsed_and_restores_exact_captured_height() {
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

    // Deliberately minimize pane %1 with captured height 24 using cyclops-tmux API
    owner
        .set_pane_option("%1", cyclops_tmux::PANE_MINIMIZED_OPTION_V1, "v1:24")
        .await
        .expect("set option");
    owner
        .resize_pane_height("%1", 1)
        .await
        .expect("minimize pane");
    assert_eq!(rig.pane_heights("geo4"), vec![45, 1]);

    // Snapshot verifies parsed provenance
    let snapshot = owner.workspace_snapshot().await.expect("snapshot");
    let win = &snapshot.sessions[0].windows[0];
    assert_eq!(
        win.panes[1].minimization,
        cyclops_tmux::PaneMinimizationProvenance::Minimized {
            original_height: 24
        }
    );

    // Owner exits
    owner.shutdown().await;

    // Follower takes over and resizes window to larger canvas (267x58)
    let (successor, _n2) = ControlClient::spawn(rig.config("geo4"))
        .await
        .expect("successor attach");
    successor
        .resize_window("@0", 267, 58)
        .await
        .expect("successor resize");

    // Re-assert collapsed state to resist tmux automatic reflow
    successor
        .resize_pane_height("%1", 1)
        .await
        .expect("re-collapse");
    assert_eq!(
        rig.pane_heights("geo4"),
        vec![56, 1],
        "deliberately minimized pane must remain exactly collapsed at 1 row after window resize"
    );

    // Restore pane to exact captured height (24 rows) and unset option
    let snap2 = successor.workspace_snapshot().await.expect("snapshot 2");
    let prov = &snap2.sessions[0].windows[0].panes[1].minimization;
    if let cyclops_tmux::PaneMinimizationProvenance::Minimized { original_height } = prov {
        successor
            .resize_pane_height("%1", *original_height)
            .await
            .expect("restore height");
        successor
            .unset_pane_option("%1", cyclops_tmux::PANE_MINIMIZED_OPTION_V1)
            .await
            .expect("unset option");
    }

    assert_eq!(
        rig.pane_heights("geo4"),
        vec![33, 24],
        "restored pane must recover exact captured 24-row height"
    );

    let out = rig.server.run(&[
        "show-options",
        "-p",
        "-t",
        "%1",
        cyclops_tmux::PANE_MINIMIZED_OPTION_V1,
    ]);
    let prov_after = String::from_utf8_lossy(&out.stdout);
    assert!(
        prov_after.trim().is_empty() || prov_after.contains("unknown"),
        "provenance option must be cleared after restore: {prov_after}"
    );

    successor.shutdown().await;
}

/// Malformed minimization provenance fails closed: it is visible in the snapshot,
/// is never modified or deleted, and restore is refused without guessing.
#[tokio::test]
async fn malformed_minimization_provenance_fails_closed_and_refuses_modification() {
    let Some(rig) = Rig::new("geometry-malformed-prov") else {
        return;
    };
    rig.session("geo5");
    rig.server.run_ok(&["split-window", "-v", "-t", "geo5"]);

    let (client, _n1) = ControlClient::spawn(rig.config("geo5"))
        .await
        .expect("attach");

    // Inject malformed provenance value with embedded tab into pane option
    client
        .set_pane_option(
            "%1",
            cyclops_tmux::PANE_MINIMIZED_OPTION_V1,
            "corrupted\twith\ttabs",
        )
        .await
        .expect("set option");
    client.resize_pane_height("%1", 1).await.expect("shrink");

    // Snapshot verifies Malformed provenance is reported without field shifting
    let snapshot = client.workspace_snapshot().await.expect("snapshot");
    let win = &snapshot.sessions[0].windows[0];
    assert_eq!(
        win.panes[1].minimization,
        cyclops_tmux::PaneMinimizationProvenance::Malformed("corrupted\twith\ttabs".to_string()),
        "malformed option with tabs must be preserved as Malformed provenance"
    );

    // Fail closed: evidence must NOT be deleted or guessed
    let out = rig.server.run(&[
        "show-options",
        "-p",
        "-t",
        "%1",
        cyclops_tmux::PANE_MINIMIZED_OPTION_V1,
    ]);
    let prov_text = String::from_utf8_lossy(&out.stdout);
    assert!(
        prov_text.contains("corrupted") && prov_text.contains("tabs"),
        "corrupted evidence must be retained intact: {prov_text}"
    );

    client.shutdown().await;
}

/// Sizing authority and minimization provenance are strictly isolated across multiple sessions.
#[tokio::test]
async fn multi_session_and_duplicate_pane_id_isolation() {
    let Some(rig) = Rig::new("geometry-multi-session") else {
        return;
    };
    rig.session("sess_a");
    rig.session("sess_b");

    let (client_a, _na) = ControlClient::spawn(rig.config("sess_a"))
        .await
        .expect("attach a");
    let (client_b, _nb) = ControlClient::spawn(rig.config("sess_b"))
        .await
        .expect("attach b");

    client_a
        .capture_prior_window_size("@0")
        .await
        .expect("capture a");
    client_a.pin_window_size_manual("@0").await.expect("pin a");
    client_a.resize_window("@0", 160, 40).await.expect("size a");

    client_b
        .capture_prior_window_size("@1")
        .await
        .expect("capture b");
    client_b.pin_window_size_manual("@1").await.expect("pin b");
    client_b.resize_window("@1", 200, 50).await.expect("size b");

    assert_eq!(rig.window_size("sess_a"), (160, 40));
    assert_eq!(rig.window_size("sess_b"), (200, 50));

    // Minimization on sess_a does not affect sess_b
    client_a
        .set_pane_option("%0", cyclops_tmux::PANE_MINIMIZED_OPTION_V1, "v1:20")
        .await
        .expect("set option on sess_a");

    let snap = client_a.workspace_snapshot().await.expect("snapshot");
    let sess_a_snap = snap.sessions.iter().find(|s| s.name == "sess_a").unwrap();
    let sess_b_snap = snap.sessions.iter().find(|s| s.name == "sess_b").unwrap();

    assert_eq!(
        sess_a_snap.windows[0].panes[0].minimization,
        cyclops_tmux::PaneMinimizationProvenance::Minimized {
            original_height: 20
        }
    );
    assert_eq!(
        sess_b_snap.windows[0].panes[0].minimization,
        cyclops_tmux::PaneMinimizationProvenance::None
    );

    client_a.shutdown().await;
    client_b.shutdown().await;
}

/// Ordinary resize paths (sidebar toggle, messages surface toggle, terminal resize)
/// and authority takeover all re-collapse deliberate 1-row minimizations via shared recovery.
#[tokio::test]
async fn deliberate_minimized_pane_survives_ordinary_resizes_and_takeover_and_restores() {
    let Some(rig) = Rig::new("geometry-ordinary-resizes") else {
        return;
    };
    rig.session("geo6");
    rig.server.run_ok(&["split-window", "-v", "-t", "geo6"]);

    let (client, _notif) = ControlClient::spawn(rig.config("geo6"))
        .await
        .expect("attach");
    let identity = client.client_identity().await.expect("identity");
    client
        .claim_window_driver("geo6", &identity.marker())
        .await
        .expect("claim");
    client
        .capture_prior_window_size("@0")
        .await
        .expect("capture");
    client.pin_window_size_manual("@0").await.expect("pin");
    client.resize_window("@0", 176, 47).await.expect("size");

    // Initially %0 is 24 rows, %1 is 22 rows (plus 1 divider = 47)
    assert_eq!(rig.pane_heights("geo6"), vec![24, 22]);

    // Deliberately minimize pane %1 with original height 22
    client
        .set_pane_option("%1", cyclops_tmux::PANE_MINIMIZED_OPTION_V1, "v1:22")
        .await
        .expect("set option");
    client.resize_pane_height("%1", 1).await.expect("minimize");
    assert_eq!(rig.pane_heights("geo6"), vec![45, 1]);

    // 1. Ordinary resize path: Terminal size increases (e.g. sidebar collapsed: 198x47)
    // Tmux automatic reflow expands %1, but shared recovery re-collapses %1 to 1 row
    client
        .resize_window("@0", 198, 47)
        .await
        .expect("resize window");
    // Simulate tmux reflow by inflating pane height
    rig.server.run_ok(&["resize-pane", "-t", "%1", "-y", "6"]);
    assert_eq!(rig.pane_heights("geo6"), vec![40, 6], "tmux reflowed pane");

    let mut owned = cyclops_workspace::app::OwnedSession::default();
    owned.pinned.insert("@0".to_string());
    let mut owned_map = std::collections::BTreeMap::new();
    owned_map.insert("geo6".to_string(), owned);
    let sizing = cyclops_workspace::app::WindowSizing {
        identity: Some(identity.clone()),
        owned: owned_map,
        following: std::collections::BTreeSet::new(),
    };

    let temp_home = cyclops_proto::scratch::scratch_root();
    let modified =
        cyclops_workspace::app::recover_post_resize_geometry(&sizing, &client, &temp_home, None)
            .await
            .expect("recover");
    assert!(modified, "recovery must re-collapse reflowed pane");
    assert_eq!(
        rig.pane_heights("geo6"),
        vec![45, 1],
        "deliberately minimized pane must be re-collapsed to 1 row"
    );

    // 2. Ordinary resize path: Terminal size decreases (e.g. 168x47)
    client
        .resize_window("@0", 168, 47)
        .await
        .expect("resize window 2");
    rig.server.run_ok(&["resize-pane", "-t", "%1", "-y", "8"]);
    let modified2 =
        cyclops_workspace::app::recover_post_resize_geometry(&sizing, &client, &temp_home, None)
            .await
            .expect("recover 2");
    assert!(modified2);
    assert_eq!(rig.pane_heights("geo6"), vec![45, 1]);

    // 3. Restore: Pane recovers exact original height (22) and clears option
    client.resize_pane_height("%1", 22).await.expect("restore");
    client
        .unset_pane_option("%1", cyclops_tmux::PANE_MINIMIZED_OPTION_V1)
        .await
        .expect("unset");
    assert_eq!(rig.pane_heights("geo6"), vec![24, 22]);

    client.shutdown().await;
}

/// Byte-exact malformed minimization options (e.g. "v1:24\t" or " v1:24 ") fail closed:
/// they are never resized, never uncrushed, and evidence is never deleted.
#[tokio::test]
async fn byte_exact_malformed_provenance_rejects_trailing_tabs_and_spaces() {
    let Some(rig) = Rig::new("geometry-byte-exact-malformed") else {
        return;
    };
    rig.session("geo7");
    rig.server.run_ok(&["split-window", "-v", "-t", "geo7"]);

    let (client, _notif) = ControlClient::spawn(rig.config("geo7"))
        .await
        .expect("attach");
    let identity = client.client_identity().await.expect("identity");
    client
        .claim_window_driver("geo7", &identity.marker())
        .await
        .expect("claim");
    client
        .capture_prior_window_size("@0")
        .await
        .expect("capture");
    client.pin_window_size_manual("@0").await.expect("pin");
    client.resize_window("@0", 176, 47).await.expect("size");

    // Inject malformed option with literal trailing tab
    client
        .set_pane_option("%1", cyclops_tmux::PANE_MINIMIZED_OPTION_V1, "v1:24\t")
        .await
        .expect("set option");
    rig.server.run_ok(&["resize-pane", "-t", "%1", "-y", "1"]);

    let mut owned7 = cyclops_workspace::app::OwnedSession::default();
    owned7.pinned.insert("@0".to_string());
    let mut owned_map7 = std::collections::BTreeMap::new();
    owned_map7.insert("geo7".to_string(), owned7);
    let sizing = cyclops_workspace::app::WindowSizing {
        identity: Some(identity.clone()),
        owned: owned_map7,
        following: std::collections::BTreeSet::new(),
    };

    let temp_home = cyclops_proto::scratch::scratch_root();
    let modified =
        cyclops_workspace::app::recover_post_resize_geometry(&sizing, &client, &temp_home, None)
            .await
            .expect("recover");
    assert!(
        !modified,
        "malformed provenance must fail closed with 0 mutations"
    );

    // Option evidence must remain untouched
    let out = rig.server.run(&[
        "show-options",
        "-p",
        "-t",
        "%1",
        cyclops_tmux::PANE_MINIMIZED_OPTION_V1,
    ]);
    let prov_text = String::from_utf8_lossy(&out.stdout);
    assert!(
        prov_text.contains("v1:24"),
        "raw option evidence must be preserved: {prov_text}"
    );

    client.shutdown().await;
}
