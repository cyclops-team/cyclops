//! Pure split policy between input routing and the tmux adapter.
//!
//! Input names the pane and where the new pane should appear. This module
//! decides whether that request is still legal against the last authoritative
//! workspace model and returns the exact route the adapter may mutate. It does
//! no IO and never updates the model; tmux events or a later snapshot settle
//! the new layout.

use crate::layout::layout_contains_pane;
use crate::model::WorkspaceModel;
use crate::resilience::LinkState;

/// Where the operator asked the new pane to appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Right,
    Down,
}

/// One semantic split request from any input device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub source_pane_id: String,
    pub placement: Placement,
}

/// The exact host route a split is allowed to mutate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub session_id: String,
    pub window_id: String,
    pub pane_id: String,
}

/// One concrete split effect for the tmux adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    pub route: Route,
    pub placement: Placement,
}

/// Why a known route cannot be attempted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Reconnecting,
    ServerGone,
    Refreshing,
}

/// Pure split-policy answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(Effect),
    /// The source no longer belongs to the authoritative visible route.
    Refresh,
    Refused(Refusal),
}

/// Decide one split intent from state already held by the application.
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

    let Some(workspace) = model.workspaces.get(model.active_workspace) else {
        return Decision::Refresh;
    };
    if workspace.name != model.session.session {
        return Decision::Refresh;
    }

    let tab = model.session.active_tab();
    if !layout_contains_pane(&tab.layout, &intent.source_pane_id) {
        return Decision::Refresh;
    }

    Decision::Run(Effect {
        route: Route {
            session_id: workspace.session_id.clone(),
            window_id: tab.window_id.clone(),
            pane_id: intent.source_pane_id,
        },
        placement: intent.placement,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::layout::{ResolvedLayout, SplitDir};
    use crate::model::{SessionModel, TabModel, WorkspaceRow};

    fn leaf(pane_id: &str, x: u16) -> ResolvedLayout {
        ResolvedLayout::Leaf {
            pane_id: pane_id.to_string(),
            x,
            y: 0,
            width: 40,
            height: 20,
        }
    }

    fn model() -> WorkspaceModel {
        WorkspaceModel {
            workspaces: vec![WorkspaceRow {
                session_id: "$1".into(),
                name: "shown".into(),
                tab_count: 1,
                window_ids: vec!["@1".into()],
            }],
            active_workspace: 0,
            session: SessionModel {
                session: "shown".into(),
                tabs: vec![TabModel {
                    window_id: "@1".into(),
                    name: "one".into(),
                    layout: ResolvedLayout::Split {
                        dir: SplitDir::Horizontal,
                        x: 0,
                        y: 0,
                        width: 81,
                        height: 20,
                        children: vec![leaf("%1", 0), leaf("%2", 41)],
                    },
                    active_pane: "%1".into(),
                    zoomed: false,
                    minimized: HashMap::new(),
                }],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        }
    }

    #[test]
    fn a_current_source_produces_one_exact_effect_for_each_placement() {
        for placement in [Placement::Right, Placement::Down] {
            assert_eq!(
                decide(
                    Intent {
                        source_pane_id: "%1".into(),
                        placement,
                    },
                    &model(),
                    LinkState::Live,
                    true,
                ),
                Decision::Run(Effect {
                    route: Route {
                        session_id: "$1".into(),
                        window_id: "@1".into(),
                        pane_id: "%1".into(),
                    },
                    placement,
                })
            );
        }
    }

    #[test]
    fn stale_source_or_hybrid_session_state_refreshes_without_an_effect() {
        let missing = decide(
            Intent {
                source_pane_id: "%9".into(),
                placement: Placement::Right,
            },
            &model(),
            LinkState::Live,
            true,
        );
        assert_eq!(missing, Decision::Refresh);

        let mut hybrid = model();
        hybrid.session.session = "replacement".into();
        assert_eq!(
            decide(
                Intent {
                    source_pane_id: "%1".into(),
                    placement: Placement::Down,
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
            source_pane_id: "%1".into(),
            placement: Placement::Right,
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
