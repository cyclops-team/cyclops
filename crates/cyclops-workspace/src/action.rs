//! One target-bearing workspace action vocabulary, and pure routing from
//! every input device to it.
//!
//! Today the same user gesture is resolved independently by keyboard
//! dispatch (`app::dispatch_action`), menu dispatch (`app::menu_action`),
//! mouse handling (`app::handle_mouse`), and dialog confirmation
//! (`app::dialog_confirm`) — each one re-deriving its own target from
//! whatever context it happens to have, and each one deciding separately
//! whether to reconcile, persist, or redraw. [`Action`] is the value all
//! four should resolve *to*; the functions below are the resolution step
//! only. None of them mutate anything, perform IO, or call tmux — they are
//! plain functions from "what did the user do, and what's the minimal
//! context needed to name a stable target" to `Option<Action>`.
//!
//! Every variant that names a pane, tab, or workspace carries the stable
//! tmux identity (`%pane`, `@window`, session name/id) it was resolved
//! against at routing time, never a list position. A list position (a menu
//! row, a keybinding's digit, a sidebar row's on-screen order) is exactly
//! the kind of thing that goes stale between the keystroke and the moment
//! an executor would act on it; resolving it here, once, against the
//! current model, is what makes that impossible.
//!
//! Confirmation is deliberately *not* part of this vocabulary. Whether
//! closing a pane or tab needs a "this may kill a live agent" confirm
//! depends on the daemon's label state, which is IO — routing cannot see
//! it and must not guess. So keyboard and menu routing always resolve
//! straight to the terminal action ([`Action::ClosePane`],
//! [`Action::CloseTab`]); a later executor is free to gate that action
//! behind a confirmation dialog when it discovers the pane hosts an agent,
//! and the dialog's own confirmation resolves to the exact same action
//! value. [`Action::CloseWorkspace`] is the one case the current UI always
//! confirms unconditionally, so keyboard and menu routing for it resolve to
//! [`Action::RequestCloseWorkspace`] (open the confirm dialog) rather than
//! the terminal action.
//!
//! # Device sources → routing functions
//!
//! | Device source | Routing function |
//! |---|---|
//! | Keyboard chord, resolved to a `BindingAction` | [`route_binding`] |
//! | Menu item clicked while a `MenuState` is open | [`route_menu_item`] |
//! | Enter, or a dialog's Confirm button | [`route_dialog_confirm`] |
//! | Mouse button pressed on a painted `HitTarget` | [`route_mouse_click`] |
//! | Mouse wheel over a painted `HitTarget` | [`route_mouse_scroll`] |
//! | Divider drag motion (coalesced per render deadline) | [`resolve_pane_resize`] |
//! | Drag released without crossing the reorder threshold | [`route_drag_click`] |
//! | Tab drag released onto another hit target | [`resolve_tab_drop`] |
//! | Sidebar-agent drag released onto another row | [`resolve_agent_drop`] |
//!
//! Workspace-row drag release is the one exception to "every device routes
//! through this module": it does not resolve against whichever hit target
//! happens to sit under the release point. A live insertion rule tracks the
//! pointer while the drag is active (`render::paint_sidebar`), and the drop
//! has to land exactly where that rule pointed — including "before the
//! first row" and "after the last," neither of which is any row's own hit
//! target. `app::resolve_workspace_slot_drop` answers that by re-running
//! the same slot math the rule painted with
//! ([`crate::drag::slot_for_row`], [`crate::drag::insertion_for_slot`])
//! against the release point, rather than adding a second, possibly
//! disagreeing, resolution here.
//!
//! `HitTarget::MenuItem` is deliberately absent from [`route_mouse_click`]:
//! it needs the open `MenuState` to mean anything, so a caller must route it
//! through [`route_menu_item`] instead. Opening or closing a menu, a
//! disclosure triangle, or a confirmation dialog is not itself in this
//! vocabulary — those are transient overlay state, the same category as
//! "hover moved," not a workspace action. Text selection and the sidebar
//! resize handle are excluded for the same reason: they never reach tmux or
//! persisted preferences through more than one device, so there is nothing
//! for one vocabulary to unify.
//!
//! `Action` deliberately does not cover the exact pixel/half-row geometry
//! that decides *where* a sidebar drop lands relative to its target row.
//! [`resolve_insertion`] mirrors the index-shift rule (dragging down
//! inserts after the target, dragging up inserts before it) restated over
//! stable ids, and still answers that question for
//! [`resolve_agent_drop`]'s target-row drops. Workspace-row dragging was
//! refined exactly as anticipated: [`crate::drag::insertion_for_slot`]
//! answers the same question from the live rule's own previewed slot
//! instead of a dropped-on row, without changing
//! [`Action::ReorderWorkspace`], [`Action::ReorderAgent`], or [`Insertion`]
//! themselves.

use crossterm::event::MouseButton;
use cyclops_tmux::{PaneDirection, SplitDirection};

use crate::bindings::BindingAction;
use crate::dialog::Dialog;
use crate::drag::DragTarget;
use crate::input::mouse::{HitTarget, MenuState};
use crate::layout::SplitDir;
use crate::model::{TabModel, WorkspaceRow};

/// One target-bearing workspace action. Every device resolves to this
/// vocabulary; only [`Action`] values reach execution (added by a later
/// task), never raw key codes, menu rows, or hit targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // -- Pane --
    /// Split `pane_id`; `direction` matches tmux's `-h`/`-v` split flags.
    Split {
        pane_id: String,
        direction: SplitDirection,
    },
    /// Focus a specific, already-known pane.
    FocusPane {
        pane_id: String,
    },
    /// Move focus to whichever pane tmux considers the current pane's
    /// neighbour in `direction`. Unlike [`Action::FocusPane`] this carries
    /// no target: tmux resolves the neighbour from live layout at execution
    /// time, the same way `select-pane -L/-R/-U/-D` always has.
    FocusDirection(PaneDirection),
    /// Close one pane. Whether this needs a confirmation first is an
    /// execution-time question (it depends on daemon-held agent state, not
    /// on anything routing can see); routing always resolves the same
    /// terminal action regardless of which device produced it.
    ClosePane {
        pane_id: String,
    },
    ZoomPane {
        pane_id: String,
    },
    /// Push `pane_id`'s edge `cells` cells in `direction`, mirroring the
    /// tmux `resize-pane` flag `direction` implies.
    ResizePane {
        pane_id: String,
        direction: PaneDirection,
        cells: u32,
    },
    /// Scroll a pane's local scrollback. This never reaches tmux — the
    /// runtime's own Alacritty display offset moves — but it is still a
    /// stable-target, device-reachable action worth naming once.
    ScrollPane {
        pane_id: String,
        lines: i32,
    },
    /// Open the pane-naming dialog for `pane_id`. The dialog itself
    /// supplies the current label from decoration state at open time;
    /// routing only needs to resolve which pane.
    RequestNamePane {
        pane_id: String,
    },
    /// Assign (or, with a blank label, clear) a pane's Cyclops identity.
    NamePane {
        pane_id: String,
        label: String,
    },

    // -- Tab --
    /// Open the new-tab naming dialog. Carries no target: a new tab is not
    /// scoped to any existing window.
    RequestNewTab,
    /// Create a tab. `name` is `None` for an automatic numeric name.
    NewTab {
        name: Option<String>,
    },
    /// Select a tab by tmux window id — never by index, so a tab opened or
    /// closed between resolution and use cannot shift what this targets.
    SelectTab {
        window_id: String,
    },
    RequestRenameTab {
        window_id: String,
    },
    RenameTab {
        window_id: String,
        name: String,
    },
    /// Close one tab. Closing the last tab in a session is, in this UI,
    /// closing the workspace instead; that decision is pure (it only reads
    /// the current tab count) so routing already makes it — see
    /// [`resolve_close_tab`].
    CloseTab {
        window_id: String,
    },
    /// Move a tab: swap it with another tab in the same session, or move it
    /// into another session entirely.
    MoveTab {
        window_id: String,
        destination: TabDestination,
    },

    // -- Workspace --
    /// Create a workspace named after the active pane's current directory.
    /// Carries no target: the source directory is resolved from the
    /// currently focused pane at execution time, the same as today.
    NewWorkspace,
    /// Switch the attached client to a workspace (tmux session), addressed
    /// by name — the same exact-match target `cyclops-tmux` already uses
    /// for `rename-session`/`kill-session`.
    SelectWorkspace {
        session: String,
    },
    RequestRenameWorkspace {
        session: String,
    },
    RenameWorkspace {
        session: String,
        name: String,
    },
    /// Open the close-workspace confirmation. Unlike pane/tab closing, this
    /// UI confirms every workspace close unconditionally, so keyboard and
    /// menu routing resolve here rather than to [`Action::CloseWorkspace`]
    /// directly.
    RequestCloseWorkspace {
        session: String,
    },
    CloseWorkspace {
        session: String,
    },
    /// Reorder one sidebar workspace row. `session_id` is the stable tmux
    /// session id, not the (renameable) session name, so a reorder started
    /// before a folder-following rename lands on the right row regardless.
    ReorderWorkspace {
        session_id: String,
        insertion: Insertion,
    },
    /// Reorder one sidebar agent row within a workspace. `order_key` is the
    /// same durable key `DecorationSnapshot::agent_order_key` produces
    /// (`name:<label>`, or `pane:<id>` for an unnamed agent).
    ReorderAgent {
        workspace_id: String,
        order_key: String,
        insertion: Insertion,
    },

    // -- App --
    ToggleEventPanel,
    ShowKeybinds,
    Detach,
}

/// Where a moved tab lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabDestination {
    /// Exchange positions with another tab in the same session — tmux has
    /// no "insert at index" for a window that already exists, so a reorder
    /// is always expressed as a swap.
    SwapWithTab(String),
    /// Move into another session (workspace), by name.
    ToWorkspace(String),
}

/// Where a reordered row lands, relative to another row's stable key.
/// `Before` the first row and `After` the last row are ordinary values of
/// this type, not special cases — see [`resolve_insertion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Insertion {
    Before(String),
    After(String),
}

/// Wheel direction over a hit target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Lines per wheel notch. Matches the constant `handle_mouse` already uses
/// for both pane scroll and the keybinds sheet.
const SCROLL_LINES: i32 = 3;

/// The minimal live-model context keyboard and menu routing need to resolve
/// a stable target: which tab/pane is current, and the full tab/workspace
/// lists to resolve a relative ("next", "by number") reference against.
/// Borrowed, read-only, and built fresh by the caller for each event — routing
/// never retains this past one call, so nothing here can go stale.
pub struct RouteContext<'a> {
    pub tabs: &'a [TabModel],
    pub active_tab: usize,
    pub active_pane: &'a str,
    /// The active workspace's tmux session name.
    pub session: &'a str,
    pub workspaces: &'a [WorkspaceRow],
    pub active_workspace: usize,
}

/// Route one resolved keyboard action. Used directly for keyboard input,
/// and by [`route_menu_item`] for the app menu, whose items are themselves
/// `BindingAction`s.
pub fn route_binding(action: BindingAction, ctx: &RouteContext) -> Option<Action> {
    match action {
        BindingAction::Detach => Some(Action::Detach),
        BindingAction::NextTab => resolve_adjacent_tab(ctx.tabs, ctx.active_tab, 1)
            .map(|window_id| Action::SelectTab { window_id }),
        BindingAction::PrevTab => resolve_adjacent_tab(ctx.tabs, ctx.active_tab, -1)
            .map(|window_id| Action::SelectTab { window_id }),
        BindingAction::SelectTab(n) => {
            ctx.tabs
                .get(n.saturating_sub(1))
                .map(|tab| Action::SelectTab {
                    window_id: tab.window_id.clone(),
                })
        }
        BindingAction::NewTab => Some(Action::NewTab { name: None }),
        BindingAction::FocusLeft => Some(Action::FocusDirection(PaneDirection::Left)),
        BindingAction::FocusRight => Some(Action::FocusDirection(PaneDirection::Right)),
        BindingAction::FocusUp => Some(Action::FocusDirection(PaneDirection::Up)),
        BindingAction::FocusDown => Some(Action::FocusDirection(PaneDirection::Down)),
        BindingAction::SplitRight => Some(Action::Split {
            pane_id: ctx.active_pane.to_string(),
            direction: SplitDirection::Horizontal,
        }),
        BindingAction::SplitDown => Some(Action::Split {
            pane_id: ctx.active_pane.to_string(),
            direction: SplitDirection::Vertical,
        }),
        BindingAction::ClosePane => Some(Action::ClosePane {
            pane_id: ctx.active_pane.to_string(),
        }),
        BindingAction::ZoomPane => Some(Action::ZoomPane {
            pane_id: ctx.active_pane.to_string(),
        }),
        BindingAction::NamePane => Some(Action::RequestNamePane {
            pane_id: ctx.active_pane.to_string(),
        }),
        BindingAction::RenameTab => {
            ctx.tabs
                .get(ctx.active_tab)
                .map(|tab| Action::RequestRenameTab {
                    window_id: tab.window_id.clone(),
                })
        }
        BindingAction::CloseTab => {
            let window_id = ctx.tabs.get(ctx.active_tab)?.window_id.clone();
            Some(resolve_close_tab(&window_id, ctx.tabs, ctx.session))
        }
        BindingAction::NextWorkspace => {
            resolve_adjacent_workspace(ctx.workspaces, ctx.active_workspace, 1)
                .map(|session| Action::SelectWorkspace { session })
        }
        BindingAction::PrevWorkspace => {
            resolve_adjacent_workspace(ctx.workspaces, ctx.active_workspace, -1)
                .map(|session| Action::SelectWorkspace { session })
        }
        BindingAction::NewWorkspace => Some(Action::NewWorkspace),
        BindingAction::RenameWorkspace => Some(Action::RequestRenameWorkspace {
            session: ctx.session.to_string(),
        }),
        BindingAction::CloseWorkspace => Some(Action::RequestCloseWorkspace {
            session: ctx.session.to_string(),
        }),
        BindingAction::ToggleEventPanel => Some(Action::ToggleEventPanel),
        BindingAction::ShowKeybinds => Some(Action::ShowKeybinds),
    }
}

/// Route one menu item, given the menu state that opened it. `ContextMenu`,
/// `TabMenu`, and `WorkspaceMenu` carry the pane/window/session the menu was
/// opened on — the item resolves against THAT target, never whichever one
/// happens to be active, so a right-click on a background tab still renames
/// the tab that was clicked. `AppMenu` items are plain `BindingAction`s, so
/// they resolve through [`route_binding`] exactly like the matching key
/// would, except the `NewTab` item: every menu (unlike the keyboard) opens
/// the naming dialog rather than creating a tab immediately — see the
/// module docs.
pub fn route_menu_item(
    menu: &MenuState,
    action: BindingAction,
    ctx: &RouteContext,
) -> Option<Action> {
    match (menu, action) {
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::NamePane) => {
            Some(Action::RequestNamePane {
                pane_id: pane_id.clone(),
            })
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SplitRight) => {
            Some(Action::Split {
                pane_id: pane_id.clone(),
                direction: SplitDirection::Horizontal,
            })
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SplitDown) => Some(Action::Split {
            pane_id: pane_id.clone(),
            direction: SplitDirection::Vertical,
        }),
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::ZoomPane) => {
            Some(Action::ZoomPane {
                pane_id: pane_id.clone(),
            })
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::ClosePane) => {
            Some(Action::ClosePane {
                pane_id: pane_id.clone(),
            })
        }
        (MenuState::TabMenu { window_id, .. }, BindingAction::RenameTab) => ctx
            .tabs
            .iter()
            .any(|tab| &tab.window_id == window_id)
            .then(|| Action::RequestRenameTab {
                window_id: window_id.clone(),
            }),
        (MenuState::TabMenu { window_id, .. }, BindingAction::CloseTab) => ctx
            .tabs
            .iter()
            .any(|tab| &tab.window_id == window_id)
            .then(|| resolve_close_tab(window_id, ctx.tabs, ctx.session)),
        (MenuState::WorkspaceMenu { session, .. }, BindingAction::RenameWorkspace) => ctx
            .workspaces
            .iter()
            .any(|workspace| &workspace.name == session)
            .then(|| Action::RequestRenameWorkspace {
                session: session.clone(),
            }),
        (MenuState::WorkspaceMenu { session, .. }, BindingAction::CloseWorkspace) => ctx
            .workspaces
            .iter()
            .any(|workspace| &workspace.name == session)
            .then(|| Action::RequestCloseWorkspace {
                session: session.clone(),
            }),
        // Every menu's "New tab" opens the naming dialog; only the keyboard
        // binding creates one immediately (checked above this arm so it
        // still wins for `AppMenu`, matching current precedence).
        (_, BindingAction::NewTab) => Some(Action::RequestNewTab),
        (MenuState::AppMenu, action) => route_binding(action, ctx),
        _ => None,
    }
}

/// Route a dialog's confirmation (Enter, or its Confirm button). Every
/// dialog carries its own target, so this needs no live-model context at
/// all — the answer is a pure function of which dialog is open and what its
/// buffer holds.
pub fn route_dialog_confirm(dialog: &Dialog) -> Option<Action> {
    match dialog {
        Dialog::ConfirmClosePane { pane_id } => Some(Action::ClosePane {
            pane_id: pane_id.clone(),
        }),
        Dialog::NewTab { buffer } => Some(Action::NewTab {
            name: non_empty(buffer),
        }),
        Dialog::NamePane {
            pane_id, buffer, ..
        } => Some(Action::NamePane {
            pane_id: pane_id.clone(),
            label: buffer.trim().to_string(),
        }),
        Dialog::RenameTab { window_id, buffer } => {
            non_empty(buffer).map(|name| Action::RenameTab {
                window_id: window_id.clone(),
                name,
            })
        }
        Dialog::ConfirmCloseTab { window_id } => Some(Action::CloseTab {
            window_id: window_id.clone(),
        }),
        Dialog::RenameWorkspace { session, buffer } => {
            non_empty(buffer).map(|name| Action::RenameWorkspace {
                session: session.clone(),
                name,
            })
        }
        Dialog::ConfirmCloseWorkspace { session } => Some(Action::CloseWorkspace {
            session: session.clone(),
        }),
        // Read-only: nothing to confirm. Enter just dismisses it.
        Dialog::Keybinds { .. } => None,
    }
}

/// `None` for blank/whitespace-only input. Every text-collecting dialog but
/// `NamePane` uses this before turning its buffer into a mutation — a blank
/// rename or new-tab name is "no action," not "rename to empty string."
/// `NamePane` does not call this: see [`route_dialog_confirm`].
fn non_empty(buffer: &str) -> Option<String> {
    let trimmed = buffer.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Route an immediate mouse click (no drag involved) on a painted hit
/// target. `HitTarget::MenuItem` is absent on purpose — see the module
/// docs. Every other hit target that starts a drag on `Down` (tabs,
/// sidebar rows, sidebar agents, dividers, the sidebar splitter) resolves
/// no action here; its click-without-drag fallback is
/// [`route_drag_click`], and its drag-release commit is
/// [`resolve_tab_drop`], `app::resolve_workspace_slot_drop`, or
/// [`resolve_agent_drop`].
pub fn route_mouse_click(target: &HitTarget, button: MouseButton) -> Option<Action> {
    match (target, button) {
        (
            HitTarget::PaneBody { pane_id } | HitTarget::PaneFrame { pane_id },
            MouseButton::Left | MouseButton::Right,
        ) => Some(Action::FocusPane {
            pane_id: pane_id.clone(),
        }),
        (HitTarget::PaneSplitRight { pane_id }, MouseButton::Left) => Some(Action::Split {
            pane_id: pane_id.clone(),
            direction: SplitDirection::Horizontal,
        }),
        (HitTarget::PaneSplitDown { pane_id }, MouseButton::Left) => Some(Action::Split {
            pane_id: pane_id.clone(),
            direction: SplitDirection::Vertical,
        }),
        (HitTarget::NewTabButton, MouseButton::Left) => Some(Action::RequestNewTab),
        (HitTarget::NewWorkspaceButton, MouseButton::Left) => Some(Action::NewWorkspace),
        (HitTarget::AttentionIndicator { pane_id }, MouseButton::Left) => Some(Action::FocusPane {
            pane_id: pane_id.clone(),
        }),
        // Left-down on a sidebar agent starts a reorder drag instead; only
        // the right-click (which never drags) focuses immediately, matching
        // `handle_mouse` today.
        (HitTarget::SidebarAgent { pane_id, .. }, MouseButton::Right) => Some(Action::FocusPane {
            pane_id: pane_id.clone(),
        }),
        _ => None,
    }
}

/// Route a wheel event over a painted hit target. Only a pane body
/// scrolls; every other target ignores the wheel today.
pub fn route_mouse_scroll(target: &HitTarget, direction: ScrollDirection) -> Option<Action> {
    let HitTarget::PaneBody { pane_id } = target else {
        return None;
    };
    let lines = match direction {
        ScrollDirection::Up => -SCROLL_LINES,
        ScrollDirection::Down => SCROLL_LINES,
    };
    Some(Action::ScrollPane {
        pane_id: pane_id.clone(),
        lines,
    })
}

/// Route a drag release that never crossed its target's move threshold —
/// i.e. a click that happened to land on a draggable hit target. Each
/// target resolves to the same action a plain click on it would, using the
/// value the drag already carries (never a re-derived index).
pub fn route_drag_click(target: &DragTarget) -> Option<Action> {
    match target {
        DragTarget::Tab { window_id } => Some(Action::SelectTab {
            window_id: window_id.clone(),
        }),
        DragTarget::Workspace { session, .. } => Some(Action::SelectWorkspace {
            session: session.clone(),
        }),
        DragTarget::Agent { pane_id, .. } => Some(Action::FocusPane {
            pane_id: pane_id.clone(),
        }),
        DragTarget::Divider { .. } | DragTarget::Sidebar => None,
    }
}

/// Resolve a divider drag's coalesced motion into one resize step.
/// `axis` is the split's orientation (which the divider itself carries);
/// the signed `delta` (screen cells moved since the last applied step)
/// picks which of that axis's two tmux resize directions applies, mirroring
/// the flag `intent::resize_divider` selects today.
pub fn resolve_pane_resize(pane_id: &str, axis: SplitDir, delta: i32) -> Option<Action> {
    if delta == 0 {
        return None;
    }
    let (direction, cells) = match (axis, delta.is_positive()) {
        (SplitDir::Horizontal, true) => (PaneDirection::Right, delta),
        (SplitDir::Horizontal, false) => (PaneDirection::Left, -delta),
        (SplitDir::Vertical, true) => (PaneDirection::Down, delta),
        (SplitDir::Vertical, false) => (PaneDirection::Up, -delta),
    };
    Some(Action::ResizePane {
        pane_id: pane_id.to_string(),
        direction,
        cells: cells.unsigned_abs(),
    })
}

/// Resolve a tab drag released on another hit target: dropping on another
/// tab reorders (swaps) within the session, dropping on a sidebar row moves
/// it to that workspace.
pub fn resolve_tab_drop(window_id: &str, drop: &HitTarget) -> Option<Action> {
    match drop {
        HitTarget::Tab { window_id: dst } if dst != window_id => Some(Action::MoveTab {
            window_id: window_id.to_string(),
            destination: TabDestination::SwapWithTab(dst.clone()),
        }),
        HitTarget::SidebarRow { session, .. } => Some(Action::MoveTab {
            window_id: window_id.to_string(),
            destination: TabDestination::ToWorkspace(session.clone()),
        }),
        _ => None,
    }
}

/// Resolve a sidebar-agent drag released on another agent row in the same
/// workspace. `order` is that workspace's current agent order, top to
/// bottom, by [`crate::decoration::DecorationSnapshot::agent_order_key`].
pub fn resolve_agent_drop(
    workspace_id: &str,
    source_key: &str,
    drop: &HitTarget,
    order: &[String],
) -> Option<Action> {
    let HitTarget::SidebarAgent {
        workspace_id: target_workspace,
        order_key: target_key,
        ..
    } = drop
    else {
        return None;
    };
    if target_workspace != workspace_id {
        return None;
    }
    let insertion = resolve_insertion(order, source_key, target_key)?;
    Some(Action::ReorderAgent {
        workspace_id: workspace_id.to_string(),
        order_key: source_key.to_string(),
        insertion,
    })
}

/// Resolve a drop into a stable insertion, from the row order alone.
/// `None` when `source` and `target` are the same row or either is not in
/// `order` (a stale hit against a row that has since disappeared).
///
/// This mirrors the current remove-then-insert-at-index behavior — moving a
/// row down lands it after the target, moving it up lands it before —
/// restated over stable ids instead of positions, so it naturally covers
/// "before the first row" (any downward-dragged source ends up `Before` a
/// target at position 0 is impossible; only an upward drag reaches it, and
/// upward always yields `Before`) and "after the last row" (symmetric, via
/// `After`) without a special case for either edge.
pub fn resolve_insertion(order: &[String], source: &str, target: &str) -> Option<Insertion> {
    if source == target {
        return None;
    }
    let source_index = order.iter().position(|id| id == source)?;
    let target_index = order.iter().position(|id| id == target)?;
    Some(if source_index < target_index {
        Insertion::After(target.to_string())
    } else {
        Insertion::Before(target.to_string())
    })
}

/// The tab at `active + delta` positions, wrapping — the same cyclic rule
/// `dispatch_action`'s `NextTab`/`PrevTab` arm and
/// `execute_switch_workspace_by_delta` both use today.
fn resolve_adjacent_tab(tabs: &[TabModel], active: usize, delta: isize) -> Option<String> {
    if tabs.is_empty() {
        return None;
    }
    let len = tabs.len() as isize;
    let next = (active as isize + delta).rem_euclid(len) as usize;
    tabs.get(next).map(|tab| tab.window_id.clone())
}

/// The workspace at `active + delta` positions, wrapping.
fn resolve_adjacent_workspace(
    workspaces: &[WorkspaceRow],
    active: usize,
    delta: isize,
) -> Option<String> {
    if workspaces.is_empty() {
        return None;
    }
    let len = workspaces.len() as isize;
    let next = (active as isize + delta).rem_euclid(len) as usize;
    workspaces.get(next).map(|workspace| workspace.name.clone())
}

/// Closing a session's only tab closes the workspace instead. This only
/// reads the current tab count, so — unlike the has-agent confirmation
/// check for either close — routing can make this decision itself.
fn resolve_close_tab(window_id: &str, tabs: &[TabModel], session: &str) -> Action {
    if tabs.len() == 1 && tabs[0].window_id == window_id {
        Action::RequestCloseWorkspace {
            session: session.to_string(),
        }
    } else {
        Action::CloseTab {
            window_id: window_id.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ResolvedLayout;

    fn tab(window_id: &str) -> TabModel {
        TabModel {
            window_id: window_id.to_string(),
            name: window_id.trim_start_matches('@').to_string(),
            layout: ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
        }
    }

    fn workspace(session_id: &str, name: &str) -> WorkspaceRow {
        WorkspaceRow {
            session_id: session_id.to_string(),
            name: name.to_string(),
            tab_count: 1,
            active: false,
            window_ids: Vec::new(),
        }
    }

    fn ctx<'a>(
        tabs: &'a [TabModel],
        active_tab: usize,
        active_pane: &'a str,
        session: &'a str,
        workspaces: &'a [WorkspaceRow],
        active_workspace: usize,
    ) -> RouteContext<'a> {
        RouteContext {
            tabs,
            active_tab,
            active_pane,
            session,
            workspaces,
            active_workspace,
        }
    }

    // -- Cross-device equivalence: the same logical operation must resolve
    // to the same `Action` value regardless of which device produced it. --

    #[test]
    fn split_right_agrees_across_keyboard_menu_and_mouse() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%3", "main", &workspaces, 0);

        let from_keyboard = route_binding(BindingAction::SplitRight, &c);
        let from_mouse = route_mouse_click(
            &HitTarget::PaneSplitRight {
                pane_id: "%3".into(),
            },
            MouseButton::Left,
        );
        let from_menu = route_menu_item(
            &MenuState::ContextMenu {
                pane_id: "%3".into(),
                at: (0, 0),
            },
            BindingAction::SplitRight,
            &c,
        );

        let expected = Some(Action::Split {
            pane_id: "%3".into(),
            direction: SplitDirection::Horizontal,
        });
        assert_eq!(from_keyboard, expected);
        assert_eq!(from_mouse, expected);
        assert_eq!(from_menu, expected);
    }

    #[test]
    fn split_down_agrees_across_keyboard_menu_and_mouse() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%3", "main", &workspaces, 0);

        let from_keyboard = route_binding(BindingAction::SplitDown, &c);
        let from_mouse = route_mouse_click(
            &HitTarget::PaneSplitDown {
                pane_id: "%3".into(),
            },
            MouseButton::Left,
        );
        let from_menu = route_menu_item(
            &MenuState::ContextMenu {
                pane_id: "%3".into(),
                at: (0, 0),
            },
            BindingAction::SplitDown,
            &c,
        );

        let expected = Some(Action::Split {
            pane_id: "%3".into(),
            direction: SplitDirection::Vertical,
        });
        assert_eq!(from_keyboard, expected);
        assert_eq!(from_mouse, expected);
        assert_eq!(from_menu, expected);
    }

    #[test]
    fn close_pane_agrees_across_keyboard_menu_and_confirm_dialog() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%9", "main", &workspaces, 0);

        let from_keyboard = route_binding(BindingAction::ClosePane, &c);
        let from_menu = route_menu_item(
            &MenuState::ContextMenu {
                pane_id: "%9".into(),
                at: (0, 0),
            },
            BindingAction::ClosePane,
            &c,
        );
        let from_dialog = route_dialog_confirm(&Dialog::ConfirmClosePane {
            pane_id: "%9".into(),
        });

        let expected = Some(Action::ClosePane {
            pane_id: "%9".into(),
        });
        assert_eq!(from_keyboard, expected);
        assert_eq!(from_menu, expected);
        assert_eq!(from_dialog, expected);
    }

    #[test]
    fn close_tab_agrees_across_keyboard_menu_and_confirm_dialog() {
        let tabs = [tab("@1"), tab("@2")];
        let workspaces = [workspace("$1", "main")];
        // Active tab is @2 for the keyboard case, matching the TabMenu's
        // explicit target so all three devices name the same window.
        let c = ctx(&tabs, 1, "%0", "main", &workspaces, 0);

        let from_keyboard = route_binding(BindingAction::CloseTab, &c);
        let from_menu = route_menu_item(
            &MenuState::TabMenu {
                window_id: "@2".into(),
                at: (0, 0),
            },
            BindingAction::CloseTab,
            &c,
        );
        let from_dialog = route_dialog_confirm(&Dialog::ConfirmCloseTab {
            window_id: "@2".into(),
        });

        let expected = Some(Action::CloseTab {
            window_id: "@2".into(),
        });
        assert_eq!(from_keyboard, expected);
        assert_eq!(from_menu, expected);
        assert_eq!(from_dialog, expected);
    }

    #[test]
    fn new_workspace_agrees_across_keyboard_menu_and_mouse() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);

        let from_keyboard = route_binding(BindingAction::NewWorkspace, &c);
        let from_menu = route_menu_item(&MenuState::AppMenu, BindingAction::NewWorkspace, &c);
        let from_mouse = route_mouse_click(&HitTarget::NewWorkspaceButton, MouseButton::Left);

        assert_eq!(from_keyboard, Some(Action::NewWorkspace));
        assert_eq!(from_menu, Some(Action::NewWorkspace));
        assert_eq!(from_mouse, Some(Action::NewWorkspace));
    }

    #[test]
    fn detach_agrees_across_keyboard_and_app_menu() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);

        assert_eq!(
            route_binding(BindingAction::Detach, &c),
            Some(Action::Detach)
        );
        assert_eq!(
            route_menu_item(&MenuState::AppMenu, BindingAction::Detach, &c),
            Some(Action::Detach)
        );
    }

    #[test]
    fn focus_pane_agrees_across_pane_click_attention_indicator_and_agent_right_click() {
        let from_pane_body = route_mouse_click(
            &HitTarget::PaneBody {
                pane_id: "%5".into(),
            },
            MouseButton::Left,
        );
        let from_attention = route_mouse_click(
            &HitTarget::AttentionIndicator {
                pane_id: "%5".into(),
            },
            MouseButton::Left,
        );
        let from_agent_right_click = route_mouse_click(
            &HitTarget::SidebarAgent {
                workspace_id: "$1".into(),
                pane_id: "%5".into(),
                order_key: "pane:%5".into(),
            },
            MouseButton::Right,
        );

        let expected = Some(Action::FocusPane {
            pane_id: "%5".into(),
        });
        assert_eq!(from_pane_body, expected);
        assert_eq!(from_attention, expected);
        assert_eq!(from_agent_right_click, expected);
    }

    #[test]
    fn select_tab_agrees_across_keybinding_and_a_click_that_never_drags() {
        let tabs = [tab("@1"), tab("@2"), tab("@3")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);

        // Ctrl+B 3 and a plain (never-dragged) click on tab @3 must target
        // the same window.
        let from_keyboard = route_binding(BindingAction::SelectTab(3), &c);
        let from_click = route_drag_click(&DragTarget::Tab {
            window_id: "@3".into(),
        });

        let expected = Some(Action::SelectTab {
            window_id: "@3".into(),
        });
        assert_eq!(from_keyboard, expected);
        assert_eq!(from_click, expected);
    }

    // -- Never a stale index into a target. --

    #[test]
    fn select_tab_by_number_re_resolves_against_the_current_tabs_every_call() {
        let before = [tab("@1"), tab("@2"), tab("@3")];
        let workspaces = [workspace("$1", "main")];
        let c_before = ctx(&before, 0, "%0", "main", &workspaces, 0);
        let resolved_before = route_binding(BindingAction::SelectTab(2), &c_before);
        assert_eq!(
            resolved_before,
            Some(Action::SelectTab {
                window_id: "@2".into()
            })
        );

        // @1 closed between the two calls; the same digit binding now
        // means a different window. Nothing in `Action` could have carried
        // a stale "index 1" forward — there is no index in the type at
        // all — so a fresh resolution is the only way this changes.
        let after = [tab("@2"), tab("@3")];
        let c_after = ctx(&after, 0, "%0", "main", &workspaces, 0);
        let resolved_after = route_binding(BindingAction::SelectTab(2), &c_after);
        assert_eq!(
            resolved_after,
            Some(Action::SelectTab {
                window_id: "@3".into()
            })
        );
        assert_ne!(resolved_before, resolved_after);
    }

    #[test]
    fn next_tab_wraps_and_never_carries_the_wrap_arithmetic_into_the_action() {
        let tabs = [tab("@1"), tab("@2")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 1, "%0", "main", &workspaces, 0);
        assert_eq!(
            route_binding(BindingAction::NextTab, &c),
            Some(Action::SelectTab {
                window_id: "@1".into()
            })
        );
    }

    #[test]
    fn select_tab_out_of_range_resolves_nothing() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);
        assert_eq!(route_binding(BindingAction::SelectTab(9), &c), None);
    }

    // -- Close-tab / close-workspace branching. --

    #[test]
    fn closing_a_sole_tab_requests_the_workspace_close_instead() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "solo")];
        let c = ctx(&tabs, 0, "%0", "solo", &workspaces, 0);
        assert_eq!(
            route_binding(BindingAction::CloseTab, &c),
            Some(Action::RequestCloseWorkspace {
                session: "solo".into()
            })
        );
    }

    #[test]
    fn closing_one_of_several_tabs_closes_the_tab() {
        let tabs = [tab("@1"), tab("@2")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);
        assert_eq!(
            route_binding(BindingAction::CloseTab, &c),
            Some(Action::CloseTab {
                window_id: "@1".into()
            })
        );
    }

    #[test]
    fn close_workspace_always_requests_confirmation() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);
        assert_eq!(
            route_binding(BindingAction::CloseWorkspace, &c),
            Some(Action::RequestCloseWorkspace {
                session: "main".into()
            })
        );
        assert_eq!(
            route_dialog_confirm(&Dialog::ConfirmCloseWorkspace {
                session: "main".into()
            }),
            Some(Action::CloseWorkspace {
                session: "main".into()
            })
        );
    }

    #[test]
    fn tab_menu_action_against_a_since_closed_tab_resolves_nothing() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);
        assert_eq!(
            route_menu_item(
                &MenuState::TabMenu {
                    window_id: "@stale".into(),
                    at: (0, 0),
                },
                BindingAction::CloseTab,
                &c,
            ),
            None
        );
    }

    // -- Documented device disagreement: keyboard bypasses the naming
    // dialog that every other device opens. Not fixed here — see the C1
    // report. --

    #[test]
    fn new_tab_disagreement_keyboard_direct_vs_every_other_device_requests_a_name() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);

        assert_eq!(
            route_binding(BindingAction::NewTab, &c),
            Some(Action::NewTab { name: None })
        );
        assert_eq!(
            route_menu_item(&MenuState::AppMenu, BindingAction::NewTab, &c),
            Some(Action::RequestNewTab)
        );
        assert_eq!(
            route_mouse_click(&HitTarget::NewTabButton, MouseButton::Left),
            Some(Action::RequestNewTab)
        );
    }

    // -- Dialog confirmation. --

    #[test]
    fn blank_rename_buffers_resolve_nothing() {
        assert_eq!(
            route_dialog_confirm(&Dialog::RenameTab {
                window_id: "@1".into(),
                buffer: "   ".into(),
            }),
            None
        );
        assert_eq!(
            route_dialog_confirm(&Dialog::RenameWorkspace {
                session: "main".into(),
                buffer: "".into(),
            }),
            None
        );
    }

    #[test]
    fn non_blank_rename_buffers_trim_and_resolve() {
        assert_eq!(
            route_dialog_confirm(&Dialog::RenameTab {
                window_id: "@1".into(),
                buffer: "  review  ".into(),
            }),
            Some(Action::RenameTab {
                window_id: "@1".into(),
                name: "review".into(),
            })
        );
    }

    #[test]
    fn blank_name_pane_buffer_still_resolves_and_clears_the_label() {
        assert_eq!(
            route_dialog_confirm(&Dialog::NamePane {
                pane_id: "%2".into(),
                buffer: "  ".into(),
                error: None,
            }),
            Some(Action::NamePane {
                pane_id: "%2".into(),
                label: String::new(),
            })
        );
    }

    #[test]
    fn blank_new_tab_buffer_still_resolves_with_no_name() {
        assert_eq!(
            route_dialog_confirm(&Dialog::NewTab {
                buffer: "  ".into()
            }),
            Some(Action::NewTab { name: None })
        );
        assert_eq!(
            route_dialog_confirm(&Dialog::NewTab {
                buffer: " scratch ".into(),
            }),
            Some(Action::NewTab {
                name: Some("scratch".into())
            })
        );
    }

    #[test]
    fn keybinds_dialog_confirms_to_no_action() {
        assert_eq!(
            route_dialog_confirm(&Dialog::Keybinds {
                scroll: 0,
                rows: Vec::new(),
            }),
            None
        );
    }

    // -- Reorder insertion: first, middle, last slots. --

    #[test]
    fn dropping_on_the_first_row_inserts_before_it() {
        let order = vec!["$a".to_string(), "$b".to_string(), "$c".to_string()];
        assert_eq!(
            resolve_insertion(&order, "$c", "$a"),
            Some(Insertion::Before("$a".into()))
        );
    }

    #[test]
    fn dropping_on_a_middle_row_from_below_inserts_before_it() {
        let order = vec![
            "$a".to_string(),
            "$b".to_string(),
            "$c".to_string(),
            "$d".to_string(),
        ];
        assert_eq!(
            resolve_insertion(&order, "$d", "$b"),
            Some(Insertion::Before("$b".into()))
        );
    }

    #[test]
    fn dropping_on_a_middle_row_from_above_inserts_after_it() {
        let order = vec![
            "$a".to_string(),
            "$b".to_string(),
            "$c".to_string(),
            "$d".to_string(),
        ];
        assert_eq!(
            resolve_insertion(&order, "$a", "$c"),
            Some(Insertion::After("$c".into()))
        );
    }

    #[test]
    fn dropping_on_the_last_row_inserts_after_it() {
        let order = vec!["$a".to_string(), "$b".to_string(), "$c".to_string()];
        assert_eq!(
            resolve_insertion(&order, "$a", "$c"),
            Some(Insertion::After("$c".into()))
        );
    }

    #[test]
    fn dropping_a_row_on_itself_resolves_nothing() {
        let order = vec!["$a".to_string(), "$b".to_string()];
        assert_eq!(resolve_insertion(&order, "$a", "$a"), None);
    }

    #[test]
    fn dropping_on_a_row_absent_from_the_order_resolves_nothing() {
        let order = vec!["$a".to_string(), "$b".to_string()];
        assert_eq!(resolve_insertion(&order, "$a", "$missing"), None);
        assert_eq!(resolve_insertion(&order, "$missing", "$a"), None);
    }

    // Workspace-row drag release no longer resolves through this module —
    // see `app::resolve_workspace_slot_drop` and its own tests, plus the
    // slot math in `drag::tests`.

    #[test]
    fn resolve_agent_drop_requires_the_same_workspace() {
        let order = vec!["pane:%1".to_string(), "pane:%2".to_string()];
        let drop_same_workspace = HitTarget::SidebarAgent {
            workspace_id: "$1".into(),
            pane_id: "%2".into(),
            order_key: "pane:%2".into(),
        };
        assert_eq!(
            resolve_agent_drop("$1", "pane:%1", &drop_same_workspace, &order),
            Some(Action::ReorderAgent {
                workspace_id: "$1".into(),
                order_key: "pane:%1".into(),
                insertion: Insertion::After("pane:%2".into()),
            })
        );

        let drop_other_workspace = HitTarget::SidebarAgent {
            workspace_id: "$2".into(),
            pane_id: "%2".into(),
            order_key: "pane:%2".into(),
        };
        assert_eq!(
            resolve_agent_drop("$1", "pane:%1", &drop_other_workspace, &order),
            None
        );
    }

    #[test]
    fn resolve_tab_drop_onto_another_tab_swaps() {
        let drop = HitTarget::Tab {
            window_id: "@2".into(),
        };
        assert_eq!(
            resolve_tab_drop("@1", &drop),
            Some(Action::MoveTab {
                window_id: "@1".into(),
                destination: TabDestination::SwapWithTab("@2".into()),
            })
        );
    }

    #[test]
    fn resolve_tab_drop_onto_itself_resolves_nothing() {
        let drop = HitTarget::Tab {
            window_id: "@1".into(),
        };
        assert_eq!(resolve_tab_drop("@1", &drop), None);
    }

    #[test]
    fn resolve_tab_drop_onto_a_sidebar_row_moves_workspaces() {
        let drop = HitTarget::SidebarRow {
            session_id: "$2".into(),
            session: "beta".into(),
        };
        assert_eq!(
            resolve_tab_drop("@1", &drop),
            Some(Action::MoveTab {
                window_id: "@1".into(),
                destination: TabDestination::ToWorkspace("beta".into()),
            })
        );
    }

    // -- Drag release without crossing the threshold. --

    #[test]
    fn drag_click_resolves_per_target_kind() {
        assert_eq!(
            route_drag_click(&DragTarget::Tab {
                window_id: "@1".into()
            }),
            Some(Action::SelectTab {
                window_id: "@1".into()
            })
        );
        assert_eq!(
            route_drag_click(&DragTarget::Workspace {
                session_id: "$1".into(),
                session: "main".into(),
            }),
            Some(Action::SelectWorkspace {
                session: "main".into()
            })
        );
        assert_eq!(
            route_drag_click(&DragTarget::Agent {
                workspace_id: "$1".into(),
                pane_id: "%1".into(),
                order_key: "pane:%1".into(),
            }),
            Some(Action::FocusPane {
                pane_id: "%1".into()
            })
        );
        assert_eq!(
            route_drag_click(&DragTarget::Divider {
                pane_id: "%1".into(),
                dir: SplitDir::Horizontal,
            }),
            None
        );
        assert_eq!(route_drag_click(&DragTarget::Sidebar), None);
    }

    // -- Divider resize resolution. --

    #[test]
    fn divider_resize_maps_axis_and_sign_to_pane_direction() {
        assert_eq!(
            resolve_pane_resize("%1", SplitDir::Horizontal, 4),
            Some(Action::ResizePane {
                pane_id: "%1".into(),
                direction: PaneDirection::Right,
                cells: 4,
            })
        );
        assert_eq!(
            resolve_pane_resize("%1", SplitDir::Horizontal, -4),
            Some(Action::ResizePane {
                pane_id: "%1".into(),
                direction: PaneDirection::Left,
                cells: 4,
            })
        );
        assert_eq!(
            resolve_pane_resize("%1", SplitDir::Vertical, 2),
            Some(Action::ResizePane {
                pane_id: "%1".into(),
                direction: PaneDirection::Down,
                cells: 2,
            })
        );
        assert_eq!(
            resolve_pane_resize("%1", SplitDir::Vertical, -2),
            Some(Action::ResizePane {
                pane_id: "%1".into(),
                direction: PaneDirection::Up,
                cells: 2,
            })
        );
    }

    #[test]
    fn divider_resize_with_no_motion_resolves_nothing() {
        assert_eq!(resolve_pane_resize("%1", SplitDir::Horizontal, 0), None);
    }

    // -- Scroll. --

    #[test]
    fn wheel_over_a_pane_body_scrolls_it() {
        assert_eq!(
            route_mouse_scroll(
                &HitTarget::PaneBody {
                    pane_id: "%1".into()
                },
                ScrollDirection::Up,
            ),
            Some(Action::ScrollPane {
                pane_id: "%1".into(),
                lines: -3,
            })
        );
        assert_eq!(
            route_mouse_scroll(
                &HitTarget::PaneBody {
                    pane_id: "%1".into()
                },
                ScrollDirection::Down,
            ),
            Some(Action::ScrollPane {
                pane_id: "%1".into(),
                lines: 3,
            })
        );
    }

    #[test]
    fn wheel_over_a_non_pane_target_resolves_nothing() {
        assert_eq!(
            route_mouse_scroll(&HitTarget::NewTabButton, ScrollDirection::Up),
            None
        );
    }

    #[test]
    fn next_workspace_agrees_with_a_click_on_that_row_without_a_drag() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "alpha"), workspace("$2", "beta")];
        let c = ctx(&tabs, 0, "%0", "alpha", &workspaces, 0);

        let from_keyboard = route_binding(BindingAction::NextWorkspace, &c);
        let from_click = route_drag_click(&DragTarget::Workspace {
            session_id: "$2".into(),
            session: "beta".into(),
        });

        let expected = Some(Action::SelectWorkspace {
            session: "beta".into(),
        });
        assert_eq!(from_keyboard, expected);
        assert_eq!(from_click, expected);
    }
}
