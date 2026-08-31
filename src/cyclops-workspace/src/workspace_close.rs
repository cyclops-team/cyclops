//! Pure workspace-close policy between confirmation and the tmux adapter.
//!
//! A confirmation carries the stable tmux session id it was opened for, not
//! the session name the operator happened to see. This module checks that
//! identity against the last authoritative model, chooses an exact fallback
//! when the visible workspace is closing, and returns one adapter effect. It
//! performs no IO and never removes a workspace from the model.

use crate::model::WorkspaceModel;
use crate::resilience::LinkState;

/// One confirmed request to close a workspace identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub session_id: String,
}

/// The exact session represented by the model at decision time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub session_id: String,
    pub name: String,
}

/// One exact close operation for the tmux adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub target: Route,
    /// Present only when closing the currently visible workspace. The adapter
    /// selects this stable identity before it closes `target`.
    pub fallback_session_id: Option<String>,
}

/// Why a known request cannot be attempted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Reconnecting,
    ServerGone,
    Refreshing,
}

/// Pure close-policy answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(Effect),
    /// The confirmed identity is no longer represented by current state.
    Refresh,
    Refused(Refusal),
}

/// Decide one confirmed workspace close from application-owned state.
pub fn decide(
    intent: Intent,
    model: &WorkspaceModel,
    link: LinkState,
    model_current: bool,
) -> Decision {
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
    let Some(target) = model
        .workspaces
        .iter()
        .find(|workspace| workspace.session_id == intent.session_id)
    else {
        return Decision::Refresh;
    };

    let fallback_session_id = if target.session_id == active.session_id {
        model
            .workspaces
            .iter()
            .find(|workspace| workspace.session_id != target.session_id)
            .map(|workspace| workspace.session_id.clone())
    } else {
        None
    };

    Decision::Run(Effect {
        target: Route {
            session_id: target.session_id.clone(),
            name: target.name.clone(),
        },
        fallback_session_id,
    })
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
                    name: "shown".into(),
                    tab_count: 1,
                    window_ids: vec!["@1".into()],
                },
                WorkspaceRow {
                    session_id: "$2".into(),
                    name: "other".into(),
                    tab_count: 1,
                    window_ids: vec!["@2".into()],
                },
            ],
            active_workspace: 0,
            session: SessionModel {
                session: "shown".into(),
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
    fn active_close_uses_stable_target_and_fallback_ids() {
        assert_eq!(
            decide(
                Intent {
                    session_id: "$1".into(),
                },
                &model(),
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect {
                target: Route {
                    session_id: "$1".into(),
                    name: "shown".into(),
                },
                fallback_session_id: Some("$2".into()),
            })
        );
    }

    #[test]
    fn background_and_only_workspace_closes_need_no_fallback_transition() {
        assert_eq!(
            decide(
                Intent {
                    session_id: "$2".into(),
                },
                &model(),
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect {
                target: Route {
                    session_id: "$2".into(),
                    name: "other".into(),
                },
                fallback_session_id: None,
            })
        );

        let mut only = model();
        only.workspaces.truncate(1);
        assert_eq!(
            decide(
                Intent {
                    session_id: "$1".into(),
                },
                &only,
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect {
                target: Route {
                    session_id: "$1".into(),
                    name: "shown".into(),
                },
                fallback_session_id: None,
            })
        );
    }

    #[test]
    fn names_may_change_without_changing_the_confirmed_identity() {
        let mut renamed = model();
        renamed.workspaces[0].name = "renamed".into();
        renamed.session.session = "renamed".into();

        assert_eq!(
            decide(
                Intent {
                    session_id: "$1".into(),
                },
                &renamed,
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect {
                target: Route {
                    session_id: "$1".into(),
                    name: "renamed".into(),
                },
                fallback_session_id: Some("$2".into()),
            })
        );
    }

    #[test]
    fn stale_identity_or_hybrid_active_state_refreshes_without_an_effect() {
        assert_eq!(
            decide(
                Intent {
                    session_id: "$9".into(),
                },
                &model(),
                LinkState::Live,
                true,
            ),
            Decision::Refresh
        );

        let mut hybrid = model();
        hybrid.session.session = "replacement".into();
        assert_eq!(
            decide(
                Intent {
                    session_id: "$1".into(),
                },
                &hybrid,
                LinkState::Live,
                true,
            ),
            Decision::Refresh
        );
    }

    #[test]
    fn link_and_reconciliation_refusals_stay_distinct() {
        let intent = || Intent {
            session_id: "$1".into(),
        };
        assert_eq!(
            decide(
                intent(),
                &model(),
                LinkState::Reconnecting { attempt: 1 },
                true,
            ),
            Decision::Refused(Refusal::Reconnecting)
        );
        assert_eq!(
            decide(intent(), &model(), LinkState::ServerGone, true),
            Decision::Refused(Refusal::ServerGone)
        );
        assert_eq!(
            decide(intent(), &model(), LinkState::Live, false),
            Decision::Refused(Refusal::Refreshing)
        );
    }
}
