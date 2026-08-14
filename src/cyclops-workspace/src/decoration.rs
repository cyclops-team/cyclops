//! Agent decoration from the daemon: badges, attention rollup, event stream.

use std::collections::HashMap;
use std::path::Path;

use cyclops_proto::{
    attention::{Attention, AttentionItem},
    AgentState, PaneStatus, StatusParams, StatusResult,
};

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
    /// Whether this status is the attention state. Two renderers used to
    /// re-derive this by string-matching `glyph`, each a different way;
    /// this field gives them the one answer `primary_status` already knows.
    pub attention: bool,
}

/// Snapshot of daemon decoration for the workspace render pass.
#[derive(Debug, Clone, Default)]
pub struct DecorationSnapshot {
    pub panes: HashMap<String, PaneDecoration>,
    /// The daemon's attention register.
    ///
    /// Nothing in the workspace reads it any more. Its per-pane half is
    /// already folded into `panes[..].needs_attention` by `apply`, which is
    /// what the tab strip and the agent rows mark from. Its delivery half
    /// had exactly one surface, the sidebar header's rollup dot, and that
    /// dot could not go out: a delivery in `attention_required` leaves only
    /// through `queued`, and nothing in the product writes that transition.
    ///
    /// Kept on the struct because the register itself is correct and the
    /// admin inbox is the surface that should read it. What is NOT kept is
    /// asking the daemon to compute the delivery half every poll: see
    /// `open_deliveries` at the fetch below.
    #[allow(dead_code)]
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
    /// to expose it. A staged composer is unavailable for another prompt but
    /// is not a running turn, so the primary UI presents it as idle while the
    /// exact state remains available to diagnostics and delivery.
    ///
    /// `needs_attention` is the only attention predicate: it is copied
    /// straight from the daemon's authoritative `cyclops_proto::attention`
    /// result (see `snapshot_from_status`) and never recomputed here. A
    /// second, local guess — this used to also fire on `state.is_blocked()`
    /// — can only ever paper over a disagreement between that register and
    /// the UI; it must never manufacture attention the daemon didn't raise.
    /// A blocked state the register left unflagged is therefore not an
    /// attention item: it groups with `working` below as an "occupied, not
    /// free" fallback, colored by the pane's exact state rather than the
    /// generic `Working` the shared glyph implies.
    pub fn primary_status(dec: &PaneDecoration) -> Option<PrimaryStatus> {
        if dec.needs_attention {
            return Some(PrimaryStatus {
                glyph: "⚠",
                word: "needs attention",
                color_state: dec.state,
                attention: true,
            });
        }
        match dec.state {
            AgentState::Unknown => None,
            AgentState::Idle => Some(PrimaryStatus {
                glyph: "○",
                word: "idle",
                color_state: AgentState::Idle,
                attention: false,
            }),
            AgentState::IdleWithInput => Some(PrimaryStatus {
                glyph: "○",
                word: "idle",
                color_state: AgentState::IdleWithInput,
                attention: false,
            }),
            AgentState::Dead => Some(PrimaryStatus {
                glyph: "✕",
                word: "dead",
                color_state: AgentState::Dead,
                attention: false,
            }),
            // Occupied, not free: real work and a block the register has not
            // flagged both read ● on a compact surface. Whether a block needs
            // a human is the register's call alone (`needs_attention` above);
            // the exact state only picks the color token, through the one
            // owner of "is this blocked".
            state => Some(PrimaryStatus {
                glyph: "●",
                word: "working",
                color_state: if state.is_blocked() {
                    state
                } else {
                    AgentState::Working
                },
                attention: false,
            }),
        }
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
///
/// `None` is a failed ASK, not an answer of "no agents", and the difference
/// matters: every sidebar row and pane label is filtered on a pane carrying
/// a label or a manifest, so rendering an empty snapshot un-names every
/// agent on screen. `app::spawn_decoration_forwarder` coalesces a burst of
/// daemon events (a split or a border drag pushes several) into one fetch,
/// but that one fetch can still be refused; a caller with a previous
/// snapshot should keep it rather than blank the roster for a frame.
pub fn fetch_decoration(home: &Path) -> Option<DecorationSnapshot> {
    crate::daemon::status(
        home,
        StatusParams {
            // Off. The only thing that read the answer was a rollup dot
            // that could never clear, and computing it is not cheap: the
            // daemon folds every ledger file from byte zero with no window,
            // on every poll. The admin inbox turns this back on when it has
            // somewhere to show the result.
            open_deliveries: false,
        },
    )
    .map(|status| snapshot_from_status(&status))
    .ok()
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
        // Daemon identity data straight off the wire: the daemon already
        // loaded the manifest at boot, so a client renders this instead of
        // rediscovering it by re-parsing manifest TOML off disk. None from
        // a daemon that predates the field, or a pane with no bound
        // manifest; `sidebar_name` falls back to the bare manifest id.
        manifest_display_name: pane.manifest_display_name.clone(),
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
            manifest_display_name: None,
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

    /// Sidebar naming precedence, all three levels, exactly as
    /// `sidebar_name` ranks them. The display name comes straight off the
    /// status answer set on the raw `PaneStatus` here — never from a
    /// manifest file on disk, which is the scan this field replaced (see
    /// the deleted `manifest_display_names`/`manifest_dir` functions).
    #[test]
    fn sidebar_name_prefers_label_then_manifest_display_name_then_manifest_id() {
        // A label wins even when the daemon also sent a display name.
        let mut labeled = pane("%0", "@0", Some("reviewer"), AgentState::Working);
        labeled.manifest_display_name = Some("Claude Code".into());
        assert_eq!(
            DecorationSnapshot::sidebar_name(&pane_decoration(&labeled, false)),
            "reviewer"
        );

        // No label: the manifest's display name from the answer wins over
        // its bare id.
        let mut named = pane("%1", "@0", None, AgentState::Working);
        named.manifest = Some("claude".into());
        named.manifest_display_name = Some("Claude Code".into());
        assert_eq!(
            DecorationSnapshot::sidebar_name(&pane_decoration(&named, false)),
            "Claude Code"
        );

        // No label and no display name — an old daemon that predates the
        // field, or a pane whose bound manifest carried none. Cosmetic
        // miss, not an error: falls back to the bare manifest id.
        let mut bare = pane("%2", "@0", None, AgentState::Working);
        bare.manifest = Some("claude".into());
        assert_eq!(
            DecorationSnapshot::sidebar_name(&pane_decoration(&bare, false)),
            "claude"
        );
    }

    #[test]
    fn primary_status_hides_unknown_and_simplifies_attention() {
        let unknown = pane_decoration(
            &pane("%0", "@0", Some("planner"), AgentState::Unknown),
            false,
        );
        assert_eq!(DecorationSnapshot::primary_status(&unknown), None);

        let blocked = pane_decoration(
            &pane("%1", "@0", Some("reviewer"), AgentState::BlockedPermission),
            true,
        );
        assert_eq!(
            DecorationSnapshot::primary_status(&blocked),
            Some(PrimaryStatus {
                glyph: "⚠",
                word: "needs attention",
                color_state: AgentState::BlockedPermission,
                attention: true,
            })
        );
    }

    /// Sidebar rows and pane chrome share this primary status. Composer
    /// input is visually idle because no turn is running, but the underlying
    /// state remains unsafe to inject into.
    #[test]
    fn composer_input_is_presented_as_idle_without_becoming_injectable() {
        let input = pane_decoration(
            &pane("%0", "@0", Some("reviewer"), AgentState::IdleWithInput),
            false,
        );

        assert!(!input.state.safe_to_inject());
        assert_eq!(
            DecorationSnapshot::primary_status(&input),
            Some(PrimaryStatus {
                glyph: "○",
                word: "idle",
                color_state: AgentState::IdleWithInput,
                attention: false,
            })
        );
    }

    /// The daemon's attention register is the only source of an attention
    /// item (rule: never recompute attention in decoration). A blocked pane
    /// the register did not flag — the daemon hasn't raised it yet, or the
    /// two disagree — must not be relabeled into attention by a second,
    /// local guess; it renders as its own occupied state instead.
    #[test]
    fn blocked_without_an_attention_item_is_not_shown_as_attention() {
        // Every state, filtered by the owner's own predicate: the blocked
        // states are enumerated nowhere here, so a future blocked state
        // joins this test by itself.
        let every_state = [
            AgentState::Unknown,
            AgentState::Idle,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
            AgentState::Dead,
        ];
        for state in every_state.into_iter().filter(|s| s.is_blocked()) {
            let dec = pane_decoration(&pane("%0", "@0", Some("reviewer"), state), false);
            assert_eq!(
                DecorationSnapshot::primary_status(&dec),
                Some(PrimaryStatus {
                    glyph: "●",
                    word: "working",
                    color_state: state,
                    attention: false,
                }),
                "{state} without needs_attention must not read as needs attention"
            );
        }
    }

    #[test]
    fn sidebar_membership_follows_stable_window_ids() {
        let snap = snapshot_from_status(&status_with(vec![
            pane("%0", "@7", Some("reviewer"), AgentState::Idle),
            pane("%1", "@8", Some("other"), AgentState::Working),
        ]));

        let rows = snap.agent_rows_for_window_ids(&["@7".to_string()], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label.as_deref(), Some("reviewer"));
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

    /// A fetch that cannot reach the daemon must be distinguishable from a
    /// daemon reporting no agents, or a caller holding a good roster has no
    /// way to keep it.
    #[test]
    fn a_failed_fetch_is_no_answer_not_an_empty_roster() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-decoration-unreachable");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("home");
        // Nothing is bound at <home>/sock, so the status call cannot answer.
        assert!(
            fetch_decoration(&home).is_none(),
            "an unreachable daemon must not read as an empty roster"
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
