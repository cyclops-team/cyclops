//! Reconcile workspace model from tmux.

use std::collections::HashMap;

use cyclops_tmux::{list_panes, list_windows, ControlClient, TmuxError, WindowPaneRow, WindowRow};

use crate::layout::{pane_ids_in_layout, parse_layout, resolve_layout};
use crate::model::{RuntimeRegistry, SessionModel, TabModel};
use crate::runtime::{snapshot_from_bundle, PaneRuntime};

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
    let index_to_id: HashMap<usize, String> =
        panes.iter().map(|p| (p.index, p.id.clone())).collect();
    let layout_node = parse_layout(&window.layout)
        .map_err(|e| TmuxError::Protocol(format!("layout parse: {e}")))?;
    let active_pane = panes
        .iter()
        .find(|p| p.active)
        .map(|p| p.id.clone())
        .or_else(|| panes.first().map(|p| p.id.clone()))
        .unwrap_or_else(|| "%0".to_string());
    let layout = match resolve_layout(&layout_node, &index_to_id) {
        Some(layout) => layout,
        None => {
            // tmux layout leaf numbers can disagree with #{pane_index} on
            // some builds; fall back to one leaf per listed pane.
            let pane = panes
                .first()
                .ok_or_else(|| TmuxError::Protocol(format!("no panes in window {}", window.id)))?;
            crate::layout::ResolvedLayout::Leaf {
                pane_id: pane.id.clone(),
                width: 80,
                height: 24,
            }
        }
    };
    Ok(TabModel {
        window_id: window.id.clone(),
        index: window.index,
        name: window.name.clone(),
        layout,
        active_pane,
        zoomed: window.zoomed,
    })
}

/// Hydrate runtimes for every pane on the visible tab.
pub async fn hydrate_visible_tab(
    client: &ControlClient,
    tab: &TabModel,
    registry: &mut RuntimeRegistry,
) -> Result<(), TmuxError> {
    let pane_ids = pane_ids_in_layout(&tab.layout);
    registry.retain_visible(&pane_ids);
    for pane_id in pane_ids {
        if registry.get(&pane_id).is_some() {
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
