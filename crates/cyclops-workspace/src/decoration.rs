#![allow(dead_code)] // exact-state helpers remain available to diagnostic surfaces

//! Agent decoration from the daemon: badges, attention rollup, event stream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cyclops_proto::{
    attention::{Attention, AttentionItem},
    state_words, AgentState, PaneStatus, StatusParams, StatusResult,
};

use crate::model::TabModel;

/// Per-pane decoration fetched from cyclopsd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDecoration {
    pub pane_id: String,
    pub window_id: String,
    pub label: Option<String>,
    pub manifest: Option<String>,
    pub manifest_display_name: Option<String>,
    pub state: AgentState,
    pub needs_attention: bool,
}

/// The deliberately small state vocabulary used by primary workspace chrome.
/// Diagnostics retain the exact [`AgentState`], including `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimaryStatus {
    pub glyph: &'static str,
    pub word: &'static str,
    /// Exact state whose semantic color represents this simplified status.
    pub color_state: AgentState,
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

    /// Sidebar display name: explicit label → manifest display name → id.
    pub fn sidebar_name(dec: &PaneDecoration) -> &str {
        if let Some(label) = &dec.label {
            return label;
        }
        dec.manifest_display_name
            .as_deref()
            .or(dec.manifest.as_deref())
            .unwrap_or("agent")
    }

    /// A durable ordering key when possible. Named agents are globally
    /// unique; an unnamed detected agent falls back to its live pane id.
    pub fn agent_order_key(dec: &PaneDecoration) -> String {
        dec.label
            .as_ref()
            .map(|label| format!("name:{label}"))
            .unwrap_or_else(|| format!("pane:{}", dec.pane_id))
    }

    /// Primary UI status. Unknown stays absent, while diagnostics continue
    /// to expose it. A staged composer is unavailable for another prompt, so
    /// the compact UI groups it with working rather than falsely calling it
    /// idle.
    pub fn primary_status(dec: &PaneDecoration) -> Option<PrimaryStatus> {
        if dec.needs_attention || dec.state.is_blocked() {
            return Some(PrimaryStatus {
                glyph: "⚠",
                word: "needs attention",
                color_state: dec.state,
            });
        }
        match dec.state {
            AgentState::Unknown => None,
            AgentState::Idle => Some(PrimaryStatus {
                glyph: "○",
                word: "idle",
                color_state: AgentState::Idle,
            }),
            AgentState::IdleWithInput | AgentState::Working => Some(PrimaryStatus {
                glyph: "●",
                word: "working",
                color_state: AgentState::Working,
            }),
            AgentState::Dead => Some(PrimaryStatus {
                glyph: "✕",
                word: "dead",
                color_state: AgentState::Dead,
            }),
            _ => unreachable!("blocked states return above"),
        }
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

    /// Named or detected agents owned by these tabs, ordered so the sidebar
    /// does not jump around when daemon hash-map insertion order changes.
    /// Window ids, unlike session names, stay stable across a workspace
    /// rename and are global within the target tmux server.
    pub fn agent_rows_for_tabs(&self, tabs: &[TabModel]) -> Vec<&PaneDecoration> {
        let window_ids: Vec<String> = tabs.iter().map(|tab| tab.window_id.clone()).collect();
        self.agent_rows_for_window_ids(&window_ids, &[])
    }

    /// Named and detected agents linked into these windows, in the user's
    /// sidebar order with deterministic name/id fallback for new rows.
    pub fn agent_rows_for_window_ids(
        &self,
        window_ids: &[String],
        order: &[String],
    ) -> Vec<&PaneDecoration> {
        let mut rows: Vec<_> = self
            .panes
            .values()
            .filter(|pane| {
                window_ids.contains(&pane.window_id)
                    && (pane.label.is_some() || pane.manifest.is_some())
            })
            .collect();
        let order_positions: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect();
        let row_positions: HashMap<&str, usize> = rows
            .iter()
            .filter_map(|pane| {
                order_positions
                    .get(Self::agent_order_key(pane).as_str())
                    .copied()
                    .map(|position| (pane.pane_id.as_str(), position))
            })
            .collect();
        rows.sort_by(|left, right| {
            row_positions
                .get(left.pane_id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(
                    &row_positions
                        .get(right.pane_id.as_str())
                        .copied()
                        .unwrap_or(usize::MAX),
                )
                .then_with(|| Self::sidebar_name(left).cmp(Self::sidebar_name(right)))
                .then_with(|| left.pane_id.cmp(&right.pane_id))
        });
        rows
    }
}

/// Fetch decoration from cyclopsd on reconcile. Attention is consumed, never
/// recomputed here.
pub fn fetch_decoration(home: &Path) -> DecorationSnapshot {
    crate::daemon::status(
        home,
        StatusParams {
            open_deliveries: true,
        },
    )
    .map(|status| {
        let display_names = manifest_display_names(home);
        snapshot_from_status(&status, &display_names)
    })
    .unwrap_or_default()
}

fn snapshot_from_status(
    status: &StatusResult,
    display_names: &HashMap<String, String>,
) -> DecorationSnapshot {
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
            panes.insert(
                pane.pane_id.clone(),
                pane_decoration(pane, needs_attention, display_names),
            );
        }
    }
    DecorationSnapshot {
        online: true,
        panes,
        attention,
    }
}

fn pane_decoration(
    pane: &PaneStatus,
    needs_attention: bool,
    display_names: &HashMap<String, String>,
) -> PaneDecoration {
    PaneDecoration {
        pane_id: pane.pane_id.clone(),
        window_id: pane.window_id.clone(),
        label: pane.agent.clone(),
        manifest: pane.manifest.clone(),
        manifest_display_name: pane
            .manifest
            .as_ref()
            .and_then(|id| display_names.get(id).cloned()),
        state: pane.state,
        needs_attention,
    }
}

/// Read only manifest identity metadata. Invalid files are already reported
/// by the daemon and cannot be allowed to break primary workspace chrome.
fn manifest_display_names(home: &Path) -> HashMap<String, String> {
    let Some(dir) = manifest_dir(home) else {
        return HashMap::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| text.parse::<toml::Table>().ok())
        .filter_map(|table| {
            let agent = table.get("agent")?.as_table()?;
            Some((
                agent.get("id")?.as_str()?.to_string(),
                agent.get("display_name")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Match the daemon's manifest-directory precedence without importing its
/// IO-owning crate: explicit config, seeded home, then a development checkout.
fn manifest_dir(home: &Path) -> Option<PathBuf> {
    let configured = std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            table
                .get("manifest_dir")
                .and_then(toml::Value::as_str)
                .map(PathBuf::from)
        });
    configured
        .or_else(|| {
            home.join("manifests")
                .is_dir()
                .then(|| home.join("manifests"))
        })
        .or_else(|| {
            PathBuf::from("manifests")
                .is_dir()
                .then(|| PathBuf::from("manifests"))
        })
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
            &HashMap::new(),
        );
        assert_eq!(DecorationSnapshot::sidebar_name(&dec), "reviewer");
    }

    #[test]
    fn unnamed_detection_uses_the_manifest_display_name() {
        let mut raw = pane("%0", "@0", None, AgentState::Working);
        raw.manifest = Some("claude".into());
        let names = HashMap::from([("claude".into(), "Claude Code".into())]);
        let dec = pane_decoration(&raw, false, &names);
        assert_eq!(DecorationSnapshot::sidebar_name(&dec), "Claude Code");
    }

    #[test]
    fn primary_status_hides_unknown_and_simplifies_attention() {
        let unknown = pane_decoration(
            &pane("%0", "@0", Some("planner"), AgentState::Unknown),
            false,
            &HashMap::new(),
        );
        assert_eq!(DecorationSnapshot::primary_status(&unknown), None);

        let blocked = pane_decoration(
            &pane("%1", "@0", Some("reviewer"), AgentState::BlockedPermission),
            true,
            &HashMap::new(),
        );
        assert_eq!(
            DecorationSnapshot::primary_status(&blocked),
            Some(PrimaryStatus {
                glyph: "⚠",
                word: "needs attention",
                color_state: AgentState::BlockedPermission,
            })
        );
    }

    #[test]
    fn configured_manifest_directory_drives_display_names() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-manifest-display");
        let _ = std::fs::remove_dir_all(&home);
        let custom = home.join("custom-manifests");
        std::fs::create_dir_all(&custom).expect("custom manifest dir");
        std::fs::write(
            home.join("config.toml"),
            format!("manifest_dir = {:?}\n", custom.to_string_lossy()),
        )
        .expect("config");
        std::fs::write(
            custom.join("custom.toml"),
            "[agent]\nid = \"custom\"\ndisplay_name = \"Custom Agent\"\n",
        )
        .expect("manifest");

        assert_eq!(
            manifest_display_names(&home)
                .get("custom")
                .map(String::as_str),
            Some("Custom Agent")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn sidebar_membership_follows_stable_window_ids() {
        let snap = snapshot_from_status(
            &status_with(vec![
                pane("%0", "@7", Some("reviewer"), AgentState::Idle),
                pane("%1", "@8", Some("other"), AgentState::Working),
            ]),
            &HashMap::new(),
        );
        let node = crate::layout::parse_layout("0000,80x24,0,0,0").unwrap();
        let tab = TabModel {
            window_id: "@7".into(),
            name: "1".into(),
            layout: crate::layout::resolve_layout(&node, &[]).unwrap(),
            active_pane: "%0".into(),
            zoomed: false,
        };

        let rows = snap.agent_rows_for_tabs(&[tab]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label.as_deref(), Some("reviewer"));
    }

    #[test]
    fn attention_rollup_is_presence_not_recomputed() {
        let status = status_with(vec![
            pane("%0", "@0", Some("a"), AgentState::BlockedPermission),
            pane("%1", "@0", Some("b"), AgentState::Idle),
        ]);
        let snap = snapshot_from_status(&status, &HashMap::new());
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
