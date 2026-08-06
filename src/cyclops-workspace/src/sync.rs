//! Reconcile workspace model from tmux.

use cyclops_tmux::{ControlClient, SnapshotWindow, TmuxError, WorkspaceSnapshot};

use crate::layout::{parse_layout, resolve_layout};
use crate::model::{
    visible_pane_dims, RuntimeRegistry, SessionModel, TabModel, WorkspaceModel, WorkspaceRow,
};
use crate::runtime::{snapshot_from_bundle, PaneRuntime};

/// Build the full workspace model from tmux with one
/// [`ControlClient::workspace_snapshot`] round trip — two control-mode
/// commands over the connection that already exists, regardless of session
/// or window count (see that method's own doc for why two is enough).
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
    let active_workspace = workspaces
        .iter()
        .position(|w| w.name == active_session)
        .unwrap_or(0);
    let session = session_model_from_snapshot(&snapshot, active_session)?;
    Ok(WorkspaceModel {
        workspaces,
        active_workspace,
        session,
        sidebar_visible: true,
    })
}

/// Pick the named session's tabs out of an already-fetched snapshot. Kept
/// separate from [`fetch_workspace_model`] so that function pays for exactly
/// one snapshot, not two.
fn session_model_from_snapshot(
    snapshot: &WorkspaceSnapshot,
    session_name: &str,
) -> Result<SessionModel, TmuxError> {
    let session = snapshot
        .sessions
        .iter()
        .find(|s| s.name == session_name)
        .ok_or_else(|| {
            TmuxError::Protocol(format!("session not present in snapshot: {session_name}"))
        })?;
    let mut tabs = Vec::with_capacity(session.windows.len());
    let mut active_tab = 0;
    for (i, window) in session.windows.iter().enumerate() {
        if window.active {
            active_tab = i;
        }
        tabs.push(build_tab(window)?);
    }
    Ok(SessionModel {
        session: session_name.to_string(),
        tabs,
        active_tab,
    })
}

fn build_tab(window: &SnapshotWindow) -> Result<TabModel, TmuxError> {
    let known: Vec<String> = window.panes.iter().map(|p| p.id.clone()).collect();
    let layout_node = parse_layout(&window.layout)
        .map_err(|e| TmuxError::Protocol(format!("layout parse: {e}")))?;
    let active_pane = window
        .panes
        .iter()
        .find(|p| p.active)
        .map(|p| p.id.clone())
        .or_else(|| window.panes.first().map(|p| p.id.clone()))
        .unwrap_or_else(|| "%0".to_string());
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
    })
}

/// Hydrate runtimes for every pane on the visible tab, concurrently. A
/// runtime whose grid no longer matches the pane's layout size is
/// rehydrated fresh — feeding bytes into a stale-sized grid scrambles rows
/// (the recovery rule from the hydration design: never trust continuity
/// across a resize).
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
    let dims = visible_pane_dims(tab);
    let pane_ids: Vec<String> = dims.iter().map(|(id, _, _)| id.clone()).collect();
    registry.retain_visible(&pane_ids);

    let stale: Vec<(String, u16, u16)> = dims
        .into_iter()
        .filter(|(pane_id, cols, rows)| {
            !registry
                .get(pane_id)
                .is_some_and(|rt| rt.size() == (*cols, *rows))
        })
        .collect();
    if stale.is_empty() {
        return;
    }

    let stale_ids: Vec<&str> = stale.iter().map(|(id, _, _)| id.as_str()).collect();
    let results = client.hydrate_panes(&stale_ids).await;
    for ((pane_id, _, _), result) in stale.into_iter().zip(results) {
        if let Ok(bundle) = result {
            let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
            runtime.hydrate(&snapshot_from_bundle(&bundle));
            registry.insert(pane_id, runtime);
        }
        // Err: this pane's slot fails alone (see the doc comment above) —
        // no runtime is inserted, and it paints blank until a later
        // hydrate succeeds.
    }
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
            deltas[0], 2,
            "fetch_workspace_model must cost exactly workspace_snapshot's two commands"
        );

        client.shutdown().await;
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
}
