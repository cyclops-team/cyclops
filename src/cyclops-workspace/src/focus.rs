//! Pure focus policy between input routing and the tmux adapter.
//!
//! A key, click, or menu gesture says what the operator meant. This module
//! decides whether that intent is still legal against the last authoritative
//! workspace model and, when it is, returns one exact route for the adapter to
//! perform. It does no IO and never changes the model. tmux notifications or a
//! later snapshot are the only things allowed to settle focus on screen.

use crate::decoration::DecorationSnapshot;
use crate::layout::layout_contains_pane;
use crate::model::WorkspaceModel;
use crate::resilience::LinkState;

/// A direction in the workspace interaction vocabulary, independent of tmux
/// command flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// What the operator asked focus to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Focus one stable pane id.
    Pane { pane_id: String },
    /// Move from the pane that was active when the chord was routed.
    Adjacent {
        from_pane_id: String,
        direction: Direction,
    },
}

/// The exact host route an effect is allowed to touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub session_id: String,
    pub window_id: String,
    pub pane_id: String,
}

/// One concrete operation for the tmux adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The pane is in the visible window; no host transition is needed first.
    Pane(Route),
    /// Select another window in the active session, then its exact pane.
    WindowPane(Route),
    /// Switch session and window, then select the exact pane.
    SessionWindowPane(Route),
    /// Ask tmux for the live neighbour of an exact source pane. The source is
    /// carried so the command cannot drift to whatever became current later.
    Adjacent { from: Route, direction: Direction },
}

/// Why focus cannot be attempted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Reconnecting,
    ServerGone,
    Refreshing,
}

/// Pure focus-policy answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The requested pane is already focused, or there cannot be a neighbour.
    NoOp,
    /// Perform this exact adapter effect, then reconcile from host state.
    Run(Effect),
    /// The gesture referenced stale or incomplete state. Refresh; do not guess.
    Refresh,
    /// The route is known but the control link cannot honestly perform it.
    Refused(Refusal),
}

/// Decide one focus intent from state already held by the application.
pub fn decide(
    intent: Intent,
    model: &WorkspaceModel,
    decoration: &DecorationSnapshot,
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

    match intent {
        Intent::Pane { pane_id } => decide_pane(&pane_id, model, decoration),
        Intent::Adjacent {
            from_pane_id,
            direction,
        } => decide_adjacent(&from_pane_id, direction, model),
    }
}

fn decide_pane(pane_id: &str, model: &WorkspaceModel, decoration: &DecorationSnapshot) -> Decision {
    let Some(active_workspace) = model.workspaces.get(model.active_workspace) else {
        return Decision::Refresh;
    };
    if active_workspace.name != model.session.session {
        return Decision::Refresh;
    }
    let current_tab = model.session.active_tab();
    if current_tab.active_pane == pane_id && layout_contains_pane(&current_tab.layout, pane_id) {
        return Decision::NoOp;
    }

    if let Some((index, tab)) = model
        .session
        .tabs
        .iter()
        .enumerate()
        .find(|(_, tab)| layout_contains_pane(&tab.layout, pane_id))
    {
        let route = Route {
            session_id: active_workspace.session_id.clone(),
            window_id: tab.window_id.clone(),
            pane_id: pane_id.to_string(),
        };
        return if index == model.session.active_tab {
            Decision::Run(Effect::Pane(route))
        } else {
            Decision::Run(Effect::WindowPane(route))
        };
    }

    // Background workspace panes are represented in the sidebar by daemon
    // decoration. Its window id is joined to the tmux-reconciled workspace
    // membership. Both halves must agree before focus crosses a session.
    let Some(window_id) = decoration.pane(pane_id).map(|pane| &pane.window_id) else {
        return Decision::Refresh;
    };
    let Some(workspace) = model
        .workspaces
        .iter()
        .find(|workspace| workspace.window_ids.iter().any(|id| id == window_id))
    else {
        return Decision::Refresh;
    };

    // A pane absent from the active session snapshot but claiming one of its
    // windows is stale evidence, not a cross-session route.
    if workspace.name == model.session.session {
        return Decision::Refresh;
    }

    Decision::Run(Effect::SessionWindowPane(Route {
        session_id: workspace.session_id.clone(),
        window_id: window_id.clone(),
        pane_id: pane_id.to_string(),
    }))
}

fn decide_adjacent(from_pane_id: &str, direction: Direction, model: &WorkspaceModel) -> Decision {
    let Some(active_workspace) = model.workspaces.get(model.active_workspace) else {
        return Decision::Refresh;
    };
    if active_workspace.name != model.session.session {
        return Decision::Refresh;
    }
    let tab = model.session.active_tab();
    if tab.active_pane != from_pane_id || !layout_contains_pane(&tab.layout, from_pane_id) {
        return Decision::Refresh;
    }

    // One pane has no legal directional destination. More complex neighbour
    // choice stays with tmux's live layout algorithm; duplicating it here
    // would create a second host authority.
    if crate::layout::pane_ids_in_layout(&tab.layout).len() < 2 {
        return Decision::NoOp;
    }

    Decision::Run(Effect::Adjacent {
        from: Route {
            session_id: active_workspace.session_id.clone(),
            window_id: tab.window_id.clone(),
            pane_id: from_pane_id.to_string(),
        },
        direction,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cyclops_proto::AgentState;

    use super::*;
    use crate::decoration::PaneDecoration;
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

    fn tab(window_id: &str, active_pane: &str, panes: &[&str]) -> TabModel {
        let layout = if panes.len() == 1 {
            leaf(panes[0], 0)
        } else {
            ResolvedLayout::Split {
                dir: SplitDir::Horizontal,
                x: 0,
                y: 0,
                width: 81,
                height: 20,
                children: panes
                    .iter()
                    .enumerate()
                    .map(|(index, pane)| leaf(pane, u16::try_from(index).unwrap() * 41))
                    .collect(),
            }
        };
        TabModel {
            window_id: window_id.to_string(),
            name: window_id.to_string(),
            layout,
            active_pane: active_pane.to_string(),
            zoomed: false,
            minimized: HashMap::new(),
        }
    }

    fn model() -> WorkspaceModel {
        WorkspaceModel {
            workspaces: vec![
                WorkspaceRow {
                    session_id: "$1".into(),
                    name: "shown".into(),
                    tab_count: 2,
                    window_ids: vec!["@1".into(), "@2".into()],
                },
                WorkspaceRow {
                    session_id: "$2".into(),
                    name: "background".into(),
                    tab_count: 1,
                    window_ids: vec!["@3".into()],
                },
            ],
            active_workspace: 0,
            session: SessionModel {
                session: "shown".into(),
                tabs: vec![tab("@1", "%1", &["%1", "%2"]), tab("@2", "%3", &["%3"])],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        }
    }

    fn decoration(pane_id: &str, window_id: &str) -> DecorationSnapshot {
        let mut snapshot = DecorationSnapshot::default();
        snapshot.panes.insert(
            pane_id.into(),
            PaneDecoration {
                pane_id: pane_id.into(),
                window_id: window_id.into(),
                label: Some("agent".into()),
                manifest: None,
                manifest_display_name: None,
                state: AgentState::Idle,
                needs_attention: false,
            },
        );
        snapshot
    }

    #[test]
    fn pane_focus_names_the_smallest_exact_host_transition() {
        let workspace = model();
        let none = DecorationSnapshot::default();
        assert_eq!(
            decide(
                Intent::Pane {
                    pane_id: "%2".into()
                },
                &workspace,
                &none,
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect::Pane(Route {
                session_id: "$1".into(),
                window_id: "@1".into(),
                pane_id: "%2".into(),
            }))
        );
        assert_eq!(
            decide(
                Intent::Pane {
                    pane_id: "%3".into()
                },
                &workspace,
                &none,
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect::WindowPane(Route {
                session_id: "$1".into(),
                window_id: "@2".into(),
                pane_id: "%3".into(),
            }))
        );
        assert_eq!(
            decide(
                Intent::Pane {
                    pane_id: "%9".into()
                },
                &workspace,
                &decoration("%9", "@3"),
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect::SessionWindowPane(Route {
                session_id: "$2".into(),
                window_id: "@3".into(),
                pane_id: "%9".into(),
            }))
        );
    }

    #[test]
    fn directional_focus_carries_the_source_and_rejects_stale_or_single_pane_state() {
        let workspace = model();
        assert_eq!(
            decide(
                Intent::Adjacent {
                    from_pane_id: "%1".into(),
                    direction: Direction::Right,
                },
                &workspace,
                &DecorationSnapshot::default(),
                LinkState::Live,
                true,
            ),
            Decision::Run(Effect::Adjacent {
                from: Route {
                    session_id: "$1".into(),
                    window_id: "@1".into(),
                    pane_id: "%1".into(),
                },
                direction: Direction::Right,
            })
        );
        assert_eq!(
            decide(
                Intent::Adjacent {
                    from_pane_id: "%2".into(),
                    direction: Direction::Right,
                },
                &workspace,
                &DecorationSnapshot::default(),
                LinkState::Live,
                true,
            ),
            Decision::Refresh,
            "a chord routed before another focus settled must not retarget"
        );

        let mut single = model();
        single.session.active_tab = 1;
        assert_eq!(
            decide(
                Intent::Adjacent {
                    from_pane_id: "%3".into(),
                    direction: Direction::Left,
                },
                &single,
                &DecorationSnapshot::default(),
                LinkState::Live,
                true,
            ),
            Decision::NoOp
        );
    }

    #[test]
    fn stale_routes_refresh_and_a_broken_link_refuses_without_an_effect() {
        let model = model();
        assert_eq!(
            decide(
                Intent::Pane {
                    pane_id: "%missing".into()
                },
                &model,
                &DecorationSnapshot::default(),
                LinkState::Live,
                true,
            ),
            Decision::Refresh
        );
        assert_eq!(
            decide(
                Intent::Pane {
                    pane_id: "%2".into()
                },
                &model,
                &DecorationSnapshot::default(),
                LinkState::Reconnecting { attempt: 1 },
                true,
            ),
            Decision::Refused(Refusal::Reconnecting)
        );
        assert_eq!(
            decide(
                Intent::Pane {
                    pane_id: "%2".into()
                },
                &model,
                &DecorationSnapshot::default(),
                LinkState::ServerGone,
                true,
            ),
            Decision::Refused(Refusal::ServerGone)
        );
        assert_eq!(
            decide(
                Intent::Pane {
                    pane_id: "%2".into()
                },
                &model,
                &DecorationSnapshot::default(),
                LinkState::Live,
                false,
            ),
            Decision::Refused(Refusal::Refreshing)
        );
    }
}
