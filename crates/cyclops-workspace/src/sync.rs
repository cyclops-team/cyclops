//! Reconcile workspace model from tmux.

use cyclops_tmux::{
    list_panes, list_sessions, list_window_memberships, list_windows, ControlClient, TmuxError,
    WindowPaneRow, WindowRow,
};

use crate::layout::{parse_layout, resolve_layout};
use crate::model::{
    visible_pane_dims, RuntimeRegistry, SessionModel, TabModel, WorkspaceModel, WorkspaceRow,
};
use crate::runtime::{snapshot_from_bundle, PaneRuntime};

/// Build the full workspace model from tmux.
pub fn fetch_workspace_model(
    active_session: &str,
    socket: Option<&str>,
) -> Result<WorkspaceModel, TmuxError> {
    let sessions = list_sessions(socket)?;
    let memberships = list_window_memberships(socket)?;
    let workspaces: Vec<WorkspaceRow> = sessions
        .iter()
        .map(|s| WorkspaceRow {
            session_id: s.id.clone(),
            name: s.name.clone(),
            tab_count: s.tab_count,
            active: s.name == active_session,
            window_ids: memberships
                .iter()
                .filter(|membership| membership.session_id == s.id)
                .map(|membership| membership.window_id.clone())
                .collect(),
        })
        .collect();
    let active_workspace = workspaces
        .iter()
        .position(|w| w.name == active_session)
        .unwrap_or(0);
    let session = fetch_session_model(active_session, socket)?;
    Ok(WorkspaceModel {
        workspaces,
        active_workspace,
        session,
        sidebar_visible: true,
    })
}

/// Build the session model from tmux list commands.
pub fn fetch_session_model(session: &str, socket: Option<&str>) -> Result<SessionModel, TmuxError> {
    let windows = list_windows(session, socket)?;
    let mut tabs = Vec::with_capacity(windows.len());
    let mut active_tab = 0;
    for (i, win) in windows.iter().enumerate() {
        if win.active {
            active_tab = i;
        }
        let panes = list_panes(&win.id, socket)?;
        tabs.push(build_tab(win, &panes)?);
    }
    Ok(SessionModel {
        session: session.to_string(),
        tabs,
        active_tab,
    })
}

fn build_tab(window: &WindowRow, panes: &[WindowPaneRow]) -> Result<TabModel, TmuxError> {
    let known: Vec<String> = panes.iter().map(|p| p.id.clone()).collect();
    let layout_node = parse_layout(&window.layout)
        .map_err(|e| TmuxError::Protocol(format!("layout parse: {e}")))?;
    let active_pane = panes
        .iter()
        .find(|p| p.active)
        .map(|p| p.id.clone())
        .or_else(|| panes.first().map(|p| p.id.clone()))
        .unwrap_or_else(|| "%0".to_string());
    let layout = match resolve_layout(&layout_node, &known) {
        Some(layout) => layout,
        None => {
            // Layout leaves name a pane the listing does not know — a race
            // between the two reads. Render the first pane alone; the next
            // %layout-change reconciles.
            let root = layout_node.rect();
            let pane = panes
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

/// Hydrate runtimes for every pane on the visible tab. A runtime whose grid
/// no longer matches the pane's layout size is rehydrated fresh — feeding
/// bytes into a stale-sized grid scrambles rows (the recovery rule from the
/// hydration design: never trust continuity across a resize).
pub async fn hydrate_visible_tab(
    client: &ControlClient,
    tab: &TabModel,
    registry: &mut RuntimeRegistry,
) -> Result<(), TmuxError> {
    let dims = visible_pane_dims(tab);
    let pane_ids: Vec<String> = dims.iter().map(|(id, _, _)| id.clone()).collect();
    registry.retain_visible(&pane_ids);
    for (pane_id, cols, rows) in dims {
        let fresh = registry
            .get(&pane_id)
            .is_some_and(|rt| rt.size() == (cols, rows));
        if fresh {
            continue;
        }
        let bundle = client.hydrate_pane(&pane_id).await?;
        let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
        runtime.hydrate(&snapshot_from_bundle(&bundle));
        registry.insert(pane_id, runtime);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use cyclops_testrig::{tmux_available, TmuxServer};
    use cyclops_tmux::ControlClient;

    use super::*;
    use crate::model::RuntimeRegistry;

    #[tokio::test]
    async fn foreign_session_appears_in_workspace_list() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("ws-list");
        server.run_ok(&["new-session", "-d", "-s", "alpha", "/bin/sh"]);
        server.run_ok(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
        let model = fetch_workspace_model("alpha", Some(server.socket())).expect("model");
        assert_eq!(model.workspaces.len(), 2);
        assert!(model.workspaces.iter().any(|w| w.name == "beta"));
        assert!(
            model
                .workspaces
                .iter()
                .all(|workspace| workspace.window_ids.len() == 1),
            "one all-windows query should populate every sidebar workspace"
        );
    }

    #[tokio::test]
    async fn tab_switch_rehydrates_panes_to_match_capture() {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return;
        }
        let server = TmuxServer::new("tab-hydrate");
        let sock = server.socket().to_string();
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

        let cfg = cyclops_tmux::ControlConfig::attach("tabs")
            .on_socket(sock)
            .with_config_file("/dev/null");
        let (client, _notif) = ControlClient::spawn(cfg).await.expect("attach");

        let model = fetch_session_model("tabs", Some(server.socket())).expect("model");
        assert_eq!(model.tabs.len(), 2);

        let mut runtimes = RuntimeRegistry::default();
        hydrate_visible_tab(&client, model.active_tab(), &mut runtimes)
            .await
            .expect("hydrate first tab");

        client.command("select-window -t :1").await.expect("switch");
        let model = fetch_session_model("tabs", Some(server.socket())).expect("model");
        let second = model.tabs.get(1).expect("second tab");
        hydrate_visible_tab(&client, second, &mut runtimes)
            .await
            .expect("hydrate second tab");

        let pane = &second.active_pane;
        let capture = client.capture_pane(pane).await.expect("capture");
        assert!(
            capture.contains("TAB_TWO_MARKER"),
            "hydrated grid should match capture after tab switch; capture={capture:?}"
        );

        client.shutdown().await;
    }
}
