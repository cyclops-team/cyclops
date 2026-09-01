//! Pure workspace-rename policy between dialog confirmation and tmux.
//!
//! A rename carries the stable tmux session id selected when the dialog
//! opened. The current model supplies the display name and the identity to
//! reconcile afterward; tmux remains the authority for whether the rename
//! actually lands.

use crate::model::WorkspaceModel;
use crate::resilience::LinkState;

/// One confirmed request to rename a workspace identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub session_id: String,
    pub name: String,
}

/// The exact session represented by the model at decision time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub session_id: String,
    pub name: String,
}

/// One exact rename operation for the tmux adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub target: Route,
    pub name: String,
    /// The session that must remain visible after the host snapshot settles.
    pub reconcile_session_id: String,
}

/// Why a known request cannot be attempted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Reconnecting,
    ServerGone,
    Refreshing,
}

/// Pure rename-policy answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(Effect),
    /// The confirmed identity is no longer represented by current state.
    Refresh,
    Refused(Refusal),
}

/// Decide one confirmed workspace rename from application-owned state.
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

    Decision::Run(Effect {
        target: Route {
            session_id: target.session_id.clone(),
            name: target.name.clone(),
        },
        name: intent.name,
        reconcile_session_id: active.session_id.clone(),
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
    fn rename_uses_the_confirmed_target_and_active_session_ids() {
        assert_eq!(
            decide(
                Intent {
                    session_id: "$2".into(),
                    name: "review".into(),
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
                name: "review".into(),
                reconcile_session_id: "$1".into(),
            })
        );
    }

    #[test]
    fn a_name_change_does_not_change_the_confirmed_identity() {
        let mut renamed = model();
        renamed.workspaces[1].name = "moved".into();

        assert_eq!(
            decide(
                Intent {
                    session_id: "$2".into(),
                    name: "review".into(),
                },
                &renamed,
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect {
                target: Route {
                    session_id: "$2".into(),
                    name: "moved".into(),
                },
                name: "review".into(),
                reconcile_session_id: "$1".into(),
            })
        );
    }

    #[test]
    fn stale_or_hybrid_models_refresh_without_an_effect() {
        assert_eq!(
            decide(
                Intent {
                    session_id: "$9".into(),
                    name: "review".into(),
                },
                &model(),
                LinkState::Live,
                true,
            ),
            Decision::Refresh
        );

        let mut hybrid = model();
        hybrid.session.session = "replacement".into();
        assert!(matches!(
            decide(
                Intent {
                    session_id: "$1".into(),
                    name: "review".into(),
                },
                &hybrid,
                LinkState::Live,
                true,
            ),
            Decision::Refresh
        ));
    }

    #[test]
    fn connection_and_refresh_state_refuse_before_io() {
        let intent = || Intent {
            session_id: "$1".into(),
            name: "review".into(),
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
