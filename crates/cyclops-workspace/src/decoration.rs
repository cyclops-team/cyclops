#![allow(dead_code)] // step 11–13 APIs exercised by tests and upcoming UI wiring

//! Agent decoration from the daemon: badges, attention rollup, event panel.

use std::collections::HashMap;
use std::path::Path;

use cyclops_proto::{
    attention::{Attention, AttentionItem},
    state_words, AgentState, PaneStatus, StatusResult, PROTOCOL_VERSION,
};

/// Per-pane decoration fetched from cyclopsd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDecoration {
    pub pane_id: String,
    pub window_id: String,
    pub label: Option<String>,
    pub manifest: Option<String>,
    pub state: AgentState,
    pub needs_attention: bool,
}

/// Snapshot of daemon decoration for the workspace render pass.
#[derive(Debug, Clone, Default)]
pub struct DecorationSnapshot {
    pub panes: HashMap<String, PaneDecoration>,
    pub attention: Attention,
    /// True when the daemon answered the last status query.
    pub online: bool,
}

impl DecorationSnapshot {
    pub fn pane(&self, pane_id: &str) -> Option<&PaneDecoration> {
        self.panes.get(pane_id)
    }

    pub fn tab_needs_attention(&self, window_id: &str) -> bool {
        self.panes
            .values()
            .any(|p| p.window_id == window_id && p.needs_attention)
    }

    pub fn workspace_needs_attention(&self) -> bool {
        self.attention.count() > 0
    }

    /// Sidebar display name: label → detected name → "agent".
    pub fn sidebar_name(dec: &PaneDecoration) -> String {
        if let Some(label) = &dec.label {
            return label.clone();
        }
        dec.manifest.clone().unwrap_or_else(|| "agent".into())
    }

    pub fn state_badge(state: AgentState) -> String {
        state_words(state)
    }

    /// Named agent rows for the expanded sidebar list.
    pub fn named_agent_rows(&self) -> Vec<&PaneDecoration> {
        self.panes
            .values()
            .filter(|p| p.label.is_some() || p.manifest.is_some())
            .collect()
    }
}

/// Fetch decoration from cyclopsd on reconcile. Attention is consumed, never
/// recomputed here.
pub fn fetch_decoration(_home: &Path) -> DecorationSnapshot {
    let sock = cyclops_proto::socket_path();
    if !sock.exists() {
        return DecorationSnapshot::default();
    }
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        return DecorationSnapshot::default();
    };
    use std::io::{BufRead, Write};
    let req = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status\",\"params\":{{\"protocol_version\":{PROTOCOL_VERSION},\"open_deliveries\":true}}}}\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return DecorationSnapshot::default();
    }
    let mut line = String::new();
    let mut reader = std::io::BufReader::new(stream);
    if reader.read_line(&mut line).is_err() {
        return DecorationSnapshot::default();
    }
    let Ok(resp) = serde_json::from_str::<serde_json::Value>(&line) else {
        return DecorationSnapshot::default();
    };
    let Some(result) = resp.get("result") else {
        return DecorationSnapshot::default();
    };
    let Ok(status) = serde_json::from_value::<StatusResult>(result.clone()) else {
        return DecorationSnapshot::default();
    };
    snapshot_from_status(&status)
}

fn snapshot_from_status(status: &StatusResult) -> DecorationSnapshot {
    let attention = Attention::from_status(status);
    let attention_panes: HashMap<String, AgentState> = attention
        .items()
        .into_iter()
        .filter_map(|item| match item {
            AttentionItem::Agent { pane_id, state, .. } => Some((pane_id, state)),
            _ => None,
        })
        .collect();
    let mut panes = HashMap::new();
    for session in &status.sessions {
        for pane in &session.panes {
            let needs_attention = attention_panes.contains_key(&pane.pane_id);
            panes.insert(pane.pane_id.clone(), pane_decoration(pane, needs_attention));
        }
    }
    DecorationSnapshot {
        online: true,
        panes,
        attention,
    }
}

fn pane_decoration(pane: &PaneStatus, needs_attention: bool) -> PaneDecoration {
    PaneDecoration {
        pane_id: pane.pane_id.clone(),
        window_id: pane.window_id.clone(),
        label: pane.agent.clone(),
        manifest: pane.manifest.clone(),
        state: pane.state,
        needs_attention,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{SessionStatus, StatusResult};

    fn pane(id: &str, win: &str, agent: Option<&str>, state: AgentState) -> PaneStatus {
        PaneStatus {
            pane_id: id.into(),
            window_id: win.into(),
            window_name: "main".into(),
            agent: agent.map(str::to_string),
            manifest: agent.map(|_| "claude".into()),
            title: String::new(),
            current_command: String::new(),
            dead: false,
            in_mode: false,
            width: 80,
            height: 24,
            state,
            state_ms: None,
            hooks_verified: None,
        }
    }

    fn status_with(panes: Vec<PaneStatus>) -> StatusResult {
        StatusResult {
            daemon_version: "0.1.0".into(),
            proto: 1,
            boot_id: "b".into(),
            uptime_ms: 1,
            tmux_version: "3.6".into(),
            sessions: vec![SessionStatus {
                name: "main".into(),
                attached: true,
                panes,
            }],
            open_deliveries: vec![],
            manifests: None,
            pid: None,
        }
    }

    #[test]
    fn naming_priority_label_over_manifest() {
        let dec = pane_decoration(
            &pane("%0", "@0", Some("reviewer"), AgentState::Working),
            false,
        );
        assert_eq!(DecorationSnapshot::sidebar_name(&dec), "reviewer");
    }

    #[test]
    fn attention_rollup_is_presence_not_recomputed() {
        let status = status_with(vec![
            pane("%0", "@0", Some("a"), AgentState::BlockedPermission),
            pane("%1", "@0", Some("b"), AgentState::Idle),
        ]);
        let snap = snapshot_from_status(&status);
        assert!(snap.tab_needs_attention("@0"));
        assert!(snap.pane("%0").unwrap().needs_attention);
        assert!(!snap.pane("%1").unwrap().needs_attention);
    }

    #[test]
    fn offline_snapshot_is_empty() {
        let snap = DecorationSnapshot::default();
        assert!(!snap.online);
    }

    #[test]
    fn state_badge_has_glyph_and_word() {
        let badge = DecorationSnapshot::state_badge(AgentState::Working);
        assert!(badge.contains('●'));
        assert!(badge.contains("working"));
    }
}
