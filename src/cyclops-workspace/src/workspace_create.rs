//! Workspace creation policy between one semantic request and tmux.
//!
//! Creation starts from the last authoritative workspace snapshot, asks the
//! adapter for the focused pane's directory, then produces one exact session
//! name and sidebar placement. The application executor performs the tmux
//! operations, while this module owns the decisions and the preference update
//! earned by a confirmed new session id.

use std::path::PathBuf;

use crate::model::WorkspaceModel;
use crate::naming;
use crate::persist::WorkspacePrefs;
use crate::resilience::LinkState;

/// The exact pane whose directory is allowed to seed a new workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub pane_id: String,
    taken_names: Vec<String>,
    order: Vec<String>,
    insert_at: usize,
}

/// One exact tmux creation request plus its eventual presentation order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub name: String,
    pub folder: PathBuf,
    order: Vec<String>,
}

/// Why a known creation request cannot begin yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Reconnecting,
    ServerGone,
    Refreshing,
}

/// Pure answer for the first step of workspace creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Probe(Probe),
    /// The active route is not represented by one coherent snapshot.
    Refresh,
    Refused(Refusal),
}

/// Resolve the one pane-directory probe from current application state.
pub fn decide(model: &WorkspaceModel, link: LinkState, model_current: bool) -> Decision {
    match link {
        LinkState::Live => {}
        LinkState::Reconnecting { .. } => return Decision::Refused(Refusal::Reconnecting),
        LinkState::ServerGone => return Decision::Refused(Refusal::ServerGone),
    }
    if !model_current {
        return Decision::Refused(Refusal::Refreshing);
    }

    let Some(active) = model.workspaces.get(model.active_workspace) else {
        return Decision::Refresh;
    };
    if active.name != model.session.session {
        return Decision::Refresh;
    }
    let pane_id = model.active_tab().active_pane.clone();
    if pane_id.is_empty() {
        return Decision::Refresh;
    }

    let order = model
        .workspaces
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect();
    Decision::Probe(Probe {
        pane_id,
        taken_names: model
            .workspaces
            .iter()
            .map(|workspace| workspace.name.clone())
            .collect(),
        order,
        insert_at: model.active_workspace.saturating_add(1),
    })
}

/// Turn one resolved creation folder into a complete creation effect.
///
/// The executor owns the external read and the exact-empty fallback rule. An
/// unreadable observation never reaches this policy: it must stop and
/// reconcile before any session is created.
pub fn prepare(probe: Probe, folder: PathBuf) -> Effect {
    let base = naming::session_name_from_folder(&folder);
    let name = naming::unique_session_name(&base, &probe.taken_names);
    let mut order = probe.order;
    order.insert(probe.insert_at.min(order.len()), name.clone());
    Effect {
        name,
        folder,
        order,
    }
}

/// Record the preference facts earned by a confirmed new session identity.
pub fn settle(effect: &Effect, session_id: &str, prefs: &mut WorkspacePrefs) {
    if !prefs.folder_tracked.iter().any(|id| id == session_id) {
        prefs.folder_tracked.push(session_id.to_string());
    }
    prefs.workspace_order.clone_from(&effect.order);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::layout::ResolvedLayout;
    use crate::model::{SessionModel, TabModel, WorkspaceRow};

    fn model() -> WorkspaceModel {
        WorkspaceModel {
            workspaces: vec![
                WorkspaceRow {
                    session_id: "$1".into(),
                    name: "alpha".into(),
                    tab_count: 1,
                    window_ids: vec!["@1".into()],
                },
                WorkspaceRow {
                    session_id: "$2".into(),
                    name: "project".into(),
                    tab_count: 1,
                    window_ids: vec!["@2".into()],
                },
            ],
            active_workspace: 0,
            session: SessionModel {
                session: "alpha".into(),
                tabs: vec![TabModel {
                    window_id: "@1".into(),
                    name: "one".into(),
                    layout: ResolvedLayout::Leaf {
                        pane_id: "%1".into(),
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    active_pane: "%1".into(),
                    zoomed: false,
                    minimized: HashMap::new(),
                    minimization_provenance: HashMap::new(),
                }],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        }
    }

    #[test]
    fn current_state_produces_one_exact_folder_probe() {
        let Decision::Probe(probe) = decide(&model(), LinkState::Live, true) else {
            panic!("current state must produce a probe");
        };
        assert_eq!(probe.pane_id, "%1");
    }

    #[test]
    fn preparation_owns_naming_uniqueness_and_sidebar_placement() {
        let Decision::Probe(probe) = decide(&model(), LinkState::Live, true) else {
            panic!("current state must produce a probe");
        };
        let effect = prepare(probe, PathBuf::from("/work/project"));

        assert_eq!(effect.name, "project-2");
        assert_eq!(effect.folder, PathBuf::from("/work/project"));
        assert_eq!(effect.order, vec!["alpha", "project-2", "project"]);
    }

    #[test]
    fn a_nonempty_folder_preserves_whitespace_exactly() {
        let Decision::Probe(probe) = decide(&model(), LinkState::Live, true) else {
            panic!("current state must produce a probe");
        };
        let folder = PathBuf::from(" /work/folder with space \n");
        let effect = prepare(probe, folder.clone());

        assert_eq!(effect.folder, folder);
    }

    #[test]
    fn settlement_tracks_the_confirmed_identity_once() {
        let Decision::Probe(probe) = decide(&model(), LinkState::Live, true) else {
            panic!("current state must produce a probe");
        };
        let effect = prepare(probe, PathBuf::from("/work/fresh"));
        let mut prefs = WorkspacePrefs {
            folder_tracked: vec!["$3".into()],
            ..WorkspacePrefs::default()
        };

        settle(&effect, "$3", &mut prefs);

        assert_eq!(prefs.folder_tracked, vec!["$3"]);
        assert_eq!(prefs.workspace_order, vec!["alpha", "fresh", "project"]);
    }

    #[test]
    fn stale_hybrid_or_disconnected_state_never_requests_io() {
        let mut hybrid = model();
        hybrid.session.session = "other".into();
        assert_eq!(decide(&hybrid, LinkState::Live, true), Decision::Refresh);
        assert_eq!(
            decide(&model(), LinkState::Live, false),
            Decision::Refused(Refusal::Refreshing)
        );
        assert_eq!(
            decide(&model(), LinkState::Reconnecting { attempt: 2 }, true),
            Decision::Refused(Refusal::Reconnecting)
        );
        assert_eq!(
            decide(&model(), LinkState::ServerGone, true),
            Decision::Refused(Refusal::ServerGone)
        );
    }

    #[test]
    fn bounded_executor_source_guard_checks_only_listed_creation_boundaries() {
        // This is deliberately a syntactic guard, not a runtime proof of the
        // lifecycle. The behavior tests above and in `app::exec` exercise the
        // actual tmux effects; this merely catches the listed policy leaking
        // back into the executor during a later edit.
        let source = include_str!("app/exec.rs");
        let body = source
            .split("async fn new_workspace")
            .nth(1)
            .expect("new-workspace executor")
            .split("/// Decide and perform one confirmed workspace rename")
            .next()
            .expect("bounded new-workspace executor");

        for leaked_policy in [
            "model.workspaces",
            "active_workspace",
            "session_name_from_folder",
            "unique_session_name",
            "folder_tracked",
            "workspace_order",
            "switch_to_session(&effect.name)",
        ] {
            assert!(
                !body.contains(leaked_policy),
                "workspace creation policy leaked back through {leaked_policy}"
            );
        }
        for owned_operation in ["::decide(", "::prepare(", "::settle("] {
            assert!(
                body.contains(owned_operation),
                "executor stopped delegating creation through {owned_operation}"
            );
        }
        assert!(
            body.contains("switch_to_session(&session_id)"),
            "the created stable identity must be the switch target"
        );
        assert!(
            body.contains("pane_current_path(&probe.pane_id)"),
            "the executor must preserve exact-pane presence before using a folder fallback"
        );
        assert!(
            body.contains("current_session_id().await"),
            "the bounded executor must retain its uncertain-switch reconciliation read"
        );
    }
}
