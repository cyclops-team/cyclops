//! Reconcile workspace model from tmux.

use cyclops_tmux::{
    ControlClient, HydrationBundle, SnapshotSession, SnapshotWindow, TmuxError, WorkspaceSnapshot,
};

use crate::layout::{parse_layout, resolve_layout};
use crate::model::{
    visible_pane_dims, RuntimeRegistry, SessionModel, TabModel, WorkspaceModel, WorkspaceRow,
};
use crate::runtime::{snapshot_from_bundle, PaneRuntime};

/// Build the full workspace model from tmux with one
/// [`ControlClient::workspace_snapshot`] round trip: three control-mode
/// commands over the connection that already exists, regardless of session
/// or window count (see that method's own doc for why three is enough).
///
/// This replaces the `list-sessions` + all-window-membership query +
/// `list-windows` + one `list-panes` *per window* fan-out this function used
/// to run as separate one-shot tmux processes: `W + 3` processes for a
/// session with `W` windows, MEASURED in `tests/baseline.rs` at 10-15ms per
/// extra window. `tab_count` is `windows.len()`: every tmux session has at
/// least one window and every window at least one pane (verified in
/// `workspace_snapshot`'s own module doc), so the window count the snapshot
/// already carries is exactly what the old `#{session_windows}` field
/// reported.
pub async fn fetch_workspace_model(
    client: &ControlClient,
    active_session: &str,
) -> Result<WorkspaceModel, TmuxError> {
    let snapshot = client.workspace_snapshot().await?;
    workspace_model_from_snapshot(&snapshot, SessionSelector::Name(active_session))
}

/// [`fetch_workspace_model`] addressed by stable tmux session identity.
///
/// Use this when an adapter has already confirmed an exact session id. A
/// session rename between the initiating action and this snapshot then changes
/// only the name installed into the model; it cannot redirect or strand the
/// reconciliation.
pub async fn fetch_workspace_model_by_id(
    client: &ControlClient,
    active_session_id: &str,
) -> Result<WorkspaceModel, TmuxError> {
    let snapshot = client.workspace_snapshot().await?;
    workspace_model_from_snapshot(&snapshot, SessionSelector::Id(active_session_id))
}

#[derive(Clone, Copy)]
enum SessionSelector<'a> {
    Name(&'a str),
    Id(&'a str),
}

fn workspace_model_from_snapshot(
    snapshot: &WorkspaceSnapshot,
    selector: SessionSelector<'_>,
) -> Result<WorkspaceModel, TmuxError> {
    let active_workspace = snapshot
        .sessions
        .iter()
        .position(|session| match selector {
            SessionSelector::Name(name) => session.name == name,
            SessionSelector::Id(id) => session.id == id,
        })
        .ok_or_else(|| {
            let missing = match selector {
                SessionSelector::Name(name) => format!("name {name}"),
                SessionSelector::Id(id) => format!("id {id}"),
            };
            TmuxError::Protocol(format!("session not present in snapshot: {missing}"))
        })?;
    let workspaces: Vec<WorkspaceRow> = snapshot
        .sessions
        .iter()
        .map(|s| WorkspaceRow {
            session_id: s.id.clone(),
            name: s.name.clone(),
            tab_count: s.windows.len(),
            window_ids: s.windows.iter().map(|w| w.id.clone()).collect(),
        })
        .collect();
    let session = session_model_from_snapshot(&snapshot.sessions[active_workspace])?;
    Ok(WorkspaceModel {
        workspaces,
        active_workspace,
        session,
        sidebar_visible: true,
        messages_visible: false,
    })
}

/// Pick one resolved session's tabs out of an already-fetched snapshot. Kept
/// separate from [`fetch_workspace_model`] so that function pays for exactly
/// one snapshot, not two.
fn session_model_from_snapshot(session: &SnapshotSession) -> Result<SessionModel, TmuxError> {
    let mut tabs = Vec::with_capacity(session.windows.len());
    let mut active_tab = 0;
    for (i, window) in session.windows.iter().enumerate() {
        if window.active {
            active_tab = i;
        }
        tabs.push(build_tab(window)?);
    }
    Ok(SessionModel {
        session: session.name.clone(),
        tabs,
        active_tab,
    })
}

fn build_tab(window: &SnapshotWindow) -> Result<TabModel, TmuxError> {
    let known: Vec<String> = window.panes.iter().map(|p| p.id.clone()).collect();
    let mut minimized = std::collections::HashMap::new();
    let mut minimization_provenance = std::collections::HashMap::new();
    for pane in &window.panes {
        minimization_provenance.insert(pane.id.clone(), pane.minimization.clone());
        if let Some(was) = pane.minimization.original_height() {
            minimized.insert(pane.id.clone(), was);
        }
    }
    let layout_node = parse_layout(&window.layout)
        .map_err(|e| TmuxError::Protocol(format!("layout parse: {e}")))?;
    let active_pane = window
        .panes
        .iter()
        .find(|p| p.active)
        .map(|p| p.id.clone())
        .or_else(|| window.panes.first().map(|p| p.id.clone()))
        .unwrap_or_default();
    let layout = match resolve_layout(&layout_node, &known) {
        Some(layout) => layout,
        None => {
            // Layout leaves name a pane the snapshot's pane list does not —
            // the window's shape changed between the notification that
            // named it and this snapshot. Render the first pane alone; the
            // next %layout-change reconciles.
            let root = layout_node.rect();
            let pane = window
                .panes
                .first()
                .ok_or_else(|| TmuxError::Protocol(format!("no panes in window {}", window.id)))?;
            crate::layout::ResolvedLayout::Leaf {
                pane_id: pane.id.clone(),
                x: 0,
                y: 0,
                width: root.width,
                height: root.height,
            }
        }
    };
    Ok(TabModel {
        window_id: window.id.clone(),
        name: window.name.clone(),
        layout,
        active_pane,
        zoomed: window.zoomed,
        minimized,
        minimization_provenance,
    })
}

/// Hydrate runtimes for every pane on the visible tab, concurrently. A
/// runtime whose grid no longer matches the pane's layout size is
/// rehydrated, because feeding bytes into a stale-sized grid scrambles
/// rows (the recovery rule from the hydration design: never trust
/// continuity across a resize). Rehydration happens in place so the
/// runtime's accumulated scrollback survives; only a pane with no runtime
/// yet gets a fresh one.
///
/// Every stale pane's hydration round trip runs through one
/// [`ControlClient::hydrate_panes`] call instead of a serial loop, so total
/// latency tracks the slowest visible pane instead of their sum (MEASURED
/// serially in `tests/baseline.rs`). A pane's hydrate can fail alone — a
/// dead or just-closed pane — and that failure stays isolated to its own
/// slot: no runtime is inserted for it, so it paints blank
/// (`render::paint_pane_slot`'s existing fallback for "no runtime yet"),
/// exactly what a pane not yet hydrated for the first time already looks
/// like. One failed pane never blocks or fails the others, and there is
/// nothing left for a caller to react to per pane: every call site already
/// just logs and keeps going on error, so leaving a failed slot blank *is*
/// that same "log and move on" behavior, decided once per pane instead of
/// once for the whole batch.
pub async fn hydrate_visible_tab(
    client: &ControlClient,
    tab: &TabModel,
    registry: &mut RuntimeRegistry,
) {
    let _ = hydrate_tab(client, tab, registry, false).await;
}

/// [`hydrate_visible_tab`] with the size check overridden: every visible
/// pane rehydrates. For the paths where continuity was lost without a
/// size change — a daemon or control-mode reconnect misses `%output`
/// while the layout stands still, and the size-gated hydrate would leave
/// those panes stale (content and VT modes both) until something happened
/// to resize them. Not for the resize path, which is a hot loop and where
/// the size check is exactly right. A failed forced capture removes that
/// pane's existing runtime and returns the first error, so a continuity
/// barrier cannot acknowledge stale screen bytes as a complete repair.
pub async fn hydrate_visible_tab_forced(
    client: &ControlClient,
    tab: &TabModel,
    registry: &mut RuntimeRegistry,
) -> Result<(), TmuxError> {
    hydrate_tab(client, tab, registry, true).await
}

/// How much scrollback a runtime meeting its pane for the first time asks
/// tmux for. Below the engine's own 10k cap and above what fits any real
/// screen; the point is that the wheel finds the pane's transcript above
/// the attach moment instead of a wall.
const FIRST_SIGHT_HISTORY_LINES: u16 = 2000;

async fn hydrate_tab(
    client: &ControlClient,
    tab: &TabModel,
    registry: &mut RuntimeRegistry,
    force: bool,
) -> Result<(), TmuxError> {
    let dims = visible_pane_dims(tab);
    let pane_ids: Vec<String> = dims.iter().map(|(id, _, _)| id.clone()).collect();
    registry.retain_visible(&pane_ids);

    let stale: Vec<(String, u16, u16)> = dims
        .into_iter()
        .filter(|(pane_id, cols, rows)| {
            force
                || !registry
                    .get(pane_id)
                    .is_some_and(|rt| rt.size() == (*cols, *rows))
        })
        .collect();
    if stale.is_empty() {
        return Ok(());
    }

    // A pane without a runtime yet gets the deeper bundle: its runtime has
    // no local history to carry, and tmux's transcript is what puts lines
    // above the attach moment under the wheel. Panes that already have a
    // runtime keep the cheap bundle — their history rides across the
    // hydrate — so the resize path pays nothing new.
    let stale_ids: Vec<&str> = stale.iter().map(|(id, _, _)| id.as_str()).collect();
    let fresh: Vec<bool> = stale
        .iter()
        .map(|(id, _, _)| registry.get(id).is_none())
        .collect();
    let results = client
        .hydrate_panes_seeding(&stale_ids, &fresh, FIRST_SIGHT_HISTORY_LINES)
        .await;
    apply_hydration_results(stale, results, registry, force)
}

fn apply_hydration_results(
    stale: Vec<(String, u16, u16)>,
    results: Vec<Result<HydrationBundle, TmuxError>>,
    registry: &mut RuntimeRegistry,
    force: bool,
) -> Result<(), TmuxError> {
    let mut results = results.into_iter();
    let mut first_error = None;
    for (pane_id, _, _) in stale {
        match results.next() {
            Some(Ok(bundle)) => {
                let snapshot = snapshot_from_bundle(&bundle);
                // In place when the pane already has a runtime: hydrate
                // resets modes and the grid itself but carries scrollback
                // across, so a resize does not eat the history the wheel
                // scrolls through.
                match registry.get_mut(&pane_id) {
                    Some(runtime) => runtime.hydrate(&snapshot),
                    None => {
                        let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
                        runtime.hydrate(&snapshot);
                        registry.insert(pane_id, runtime);
                    }
                }
            }
            Some(Err(error)) => {
                if force {
                    registry.remove(&pane_id);
                }
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            None => {
                if force {
                    registry.remove(&pane_id);
                }
                if first_error.is_none() {
                    first_error = Some(TmuxError::Protocol(format!(
                        "hydrate returned no result for pane {pane_id}"
                    )));
                }
            }
        }
    }
    if results.next().is_some() && first_error.is_none() {
        first_error = Some(TmuxError::Protocol(
            "hydrate returned more pane results than requested".into(),
        ));
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use cyclops_testrig::{tmux_available, TmuxServer};
    use cyclops_tmux::{ControlClient, ControlConfig};

    use super::*;
    use crate::layout::{ResolvedLayout, SplitDir};

    fn config(server: &TmuxServer, session: &str) -> ControlConfig {
        ControlConfig::attach(session)
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null")
    }

    #[tokio::test]
    async fn foreign_session_appears_in_workspace_list() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("ws-list");
        server.run_ok(&["new-session", "-d", "-s", "alpha", "/bin/sh"]);
        server.run_ok(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
        let (client, _notif) = ControlClient::spawn(config(&server, "alpha"))
            .await
            .expect("attach");

        let model = fetch_workspace_model(&client, "alpha")
            .await
            .expect("model");
        assert_eq!(model.workspaces.len(), 2);
        assert!(model.workspaces.iter().any(|w| w.name == "beta"));
        assert!(
            model
                .workspaces
                .iter()
                .all(|workspace| workspace.window_ids.len() == 1),
            "one snapshot should populate every sidebar workspace"
        );

        client.shutdown().await;
    }

    #[tokio::test]
    async fn tab_switch_rehydrates_panes_to_match_capture() {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return;
        }
        let server = TmuxServer::new("tab-hydrate");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "tabs",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["new-window", "-t", "tabs", "-n", "second", "/bin/sh"]);
        server.run_ok(&[
            "send-keys",
            "-t",
            "tabs:1",
            r"printf 'TAB_TWO_MARKER\n'",
            "Enter",
        ]);

        let (client, _notif) = ControlClient::spawn(config(&server, "tabs"))
            .await
            .expect("attach");

        let model = fetch_workspace_model(&client, "tabs").await.expect("model");
        assert_eq!(model.session.tabs.len(), 2);

        let mut runtimes = RuntimeRegistry::default();
        hydrate_visible_tab(&client, model.session.active_tab(), &mut runtimes).await;

        client.command("select-window -t :1").await.expect("switch");
        let model = fetch_workspace_model(&client, "tabs").await.expect("model");
        let second = model.session.tabs.get(1).expect("second tab");
        hydrate_visible_tab(&client, second, &mut runtimes).await;

        let pane = &second.active_pane;
        let capture = client.capture_pane(pane).await.expect("capture");
        assert!(
            capture.contains("TAB_TWO_MARKER"),
            "hydrated grid should match capture after tab switch; capture={capture:?}"
        );

        client.shutdown().await;
    }

    /// L1: one reconcile's model fetch must issue a small, window-count-
    /// independent number of commands through the control client — proof
    /// this integration actually replaced the `W + 3` fan-out end to end,
    /// not just that `ControlClient::workspace_snapshot` itself can (D2
    /// already proved that in
    /// `src/cyclops-tmux/tests/workspace_snapshot.rs`).
    #[tokio::test]
    async fn fetch_workspace_model_issues_a_bounded_command_count() {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return;
        }
        let server = TmuxServer::new("ws-recon-count");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "base",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let (client, _notif) = ControlClient::spawn(config(&server, "base"))
            .await
            .expect("attach");

        let mut deltas = Vec::new();
        for &w in &[1usize, 4, 8] {
            let session = format!("rec{w}");
            server.run_ok(&[
                "new-session",
                "-d",
                "-s",
                &session,
                "-x",
                "80",
                "-y",
                "24",
                "/bin/sh",
            ]);
            for _ in 1..w {
                server.run_ok(&["new-window", "-t", &session, "/bin/sh"]);
            }

            let before = client.commands_issued();
            let model = fetch_workspace_model(&client, &session)
                .await
                .expect("model");
            let after = client.commands_issued();

            assert_eq!(model.session.tabs.len(), w, "expected {w} windows");
            deltas.push(after - before);
        }

        assert!(
            deltas.iter().all(|&d| d == deltas[0]),
            "command count must not grow with window count, got {deltas:?} for W=1,4,8"
        );
        assert_eq!(
            deltas[0], 3,
            "fetch_workspace_model must cost exactly workspace_snapshot's three commands"
        );

        client.shutdown().await;
    }

    #[test]
    fn a_failed_forced_hydrate_removes_the_stale_runtime_and_reports_incomplete() {
        let mut registry = RuntimeRegistry::default();
        registry.insert("%7".to_string(), PaneRuntime::new(80, 24));
        let result = apply_hydration_results(
            vec![("%7".to_string(), 80, 24)],
            vec![Err(TmuxError::Protocol("capture failed".into()))],
            &mut registry,
            true,
        );

        assert!(result.is_err(), "forced hydration reports incompleteness");
        assert!(
            registry.get("%7").is_none(),
            "failed forced capture cannot leave stale pane bytes visible"
        );
    }

    /// L1: a dead pane's hydrate failure must not stop the rest of the
    /// visible tab from hydrating.
    #[tokio::test]
    async fn a_failed_pane_slot_does_not_fail_visible_tab_hydration() {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return;
        }
        let server = TmuxServer::new("hyd-dead-slot");
        server.run_ok(&["new-session", "-d", "-s", "mix", "/bin/sh"]);
        let (client, _notif) = ControlClient::spawn(config(&server, "mix"))
            .await
            .expect("attach");

        // %0 is real; %99 never existed on this server, so its hydrate
        // fails on its own slot (see cyclops-tmux's hydrate_panes doc).
        let tab = TabModel {
            window_id: "@0".to_string(),
            name: "1".to_string(),
            layout: ResolvedLayout::Split {
                dir: SplitDir::Horizontal,
                x: 0,
                y: 0,
                width: 80,
                height: 24,
                children: vec![
                    ResolvedLayout::Leaf {
                        pane_id: "%0".to_string(),
                        x: 0,
                        y: 0,
                        width: 40,
                        height: 24,
                    },
                    ResolvedLayout::Leaf {
                        pane_id: "%99".to_string(),
                        x: 40,
                        y: 0,
                        width: 40,
                        height: 24,
                    },
                ],
            },
            active_pane: "%0".to_string(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };

        let mut registry = RuntimeRegistry::default();
        hydrate_visible_tab(&client, &tab, &mut registry).await;

        assert!(
            registry.get("%0").is_some(),
            "the live pane must still hydrate"
        );
        assert!(
            registry.get("%99").is_none(),
            "a dead pane's slot must fail alone: no runtime, but the live \
             pane's own hydration must not be blocked by it"
        );

        client.shutdown().await;
    }

    /// The resize path marks a pane's runtime stale and rehydrates it. The
    /// existing runtime must be hydrated in place, not rebuilt, so the
    /// scrollback it accumulated from live output still answers the wheel
    /// immediately after the resize.
    #[tokio::test]
    async fn a_resized_panes_rehydration_keeps_its_scrollback() {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return;
        }
        let server = TmuxServer::new("hyd-keep-scrollback");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "keep",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        let (client, _notif) = ControlClient::spawn(config(&server, "keep"))
            .await
            .expect("attach");

        let leaf = |width, height| TabModel {
            window_id: "@0".to_string(),
            name: "1".to_string(),
            layout: ResolvedLayout::Leaf {
                pane_id: "%0".to_string(),
                x: 0,
                y: 0,
                width,
                height,
            },
            active_pane: "%0".to_string(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };

        let mut registry = RuntimeRegistry::default();
        hydrate_visible_tab(&client, &leaf(80, 24), &mut registry).await;
        // Live output builds local scrollback, the way the %output feed does.
        let runtime = registry.get_mut("%0").expect("runtime");
        for i in 0..40 {
            runtime.feed(format!("history{i}\r\n").as_bytes());
        }

        // The pane resized: the layout reports new dims, the runtime is stale.
        hydrate_visible_tab(&client, &leaf(60, 20), &mut registry).await;

        let runtime = registry.get_mut("%0").expect("runtime");
        runtime.scroll(-5);
        assert!(
            !runtime.at_tail(),
            "the wheel must still reach scrollback right after a resize"
        );
        assert!(
            runtime.row_text(0).contains("history"),
            "the lines that scrolled off before the resize are still there"
        );

        client.shutdown().await;
    }

    /// The wall an operator could only fix by resizing: before history
    /// seeding, a runtime meeting its pane for the first time started with
    /// empty scrollback, so the wheel stopped dead at the attach moment
    /// even though tmux held the pane's whole transcript. A first-sight
    /// hydration must put that transcript above the screen.
    #[tokio::test]
    async fn a_fresh_runtime_can_scroll_into_the_panes_pre_attach_history() {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return;
        }
        let server = TmuxServer::new("hyd-seed-history");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "seed",
            "-x",
            "80",
            "-y",
            "10",
            "/bin/sh",
        ]);
        // Fill tmux's own history well past one screen BEFORE any client
        // attaches: this is the transcript the operator expects the wheel
        // to reach.
        server.run_ok(&[
            "send-keys",
            "-t",
            "seed",
            "for i in $(seq 1 60); do echo preattach$i; done",
            "Enter",
        ]);
        // The loop is fast, but it still has to run: wait until the last
        // line is on the pane before hydrating.
        for _ in 0..50 {
            let out = server.run(&["capture-pane", "-p", "-t", "seed"]);
            if String::from_utf8_lossy(&out.stdout).contains("preattach60") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let (client, _notif) = ControlClient::spawn(config(&server, "seed"))
            .await
            .expect("attach");
        let leaf = TabModel {
            window_id: "@0".to_string(),
            name: "1".to_string(),
            layout: ResolvedLayout::Leaf {
                pane_id: "%0".to_string(),
                x: 0,
                y: 0,
                width: 80,
                height: 10,
            },
            active_pane: "%0".to_string(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
            minimization_provenance: std::collections::HashMap::new(),
        };

        let mut registry = RuntimeRegistry::default();
        hydrate_visible_tab(&client, &leaf, &mut registry).await;

        let runtime = registry.get_mut("%0").expect("runtime");
        runtime.scroll(-30);
        assert!(
            !runtime.at_tail(),
            "the wheel must reach the pane's pre-attach transcript"
        );
        let shown: String = (0..10).map(|r| runtime.row_text(r)).collect();
        assert!(
            shown.contains("preattach"),
            "scrolled back, the viewport must show pre-attach lines: {shown:?}"
        );

        client.shutdown().await;
    }
}
