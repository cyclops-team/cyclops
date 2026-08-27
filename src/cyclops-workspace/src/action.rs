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
//! | Keyboard chord with Shift held on a prefix chord | [`route_binding_shifted`] |
//! | Menu item clicked while a `MenuState` is open | [`route_menu_item`] |
//! | Enter, or a dialog's Confirm button | [`route_dialog_confirm`] |
//! | Mouse button pressed on a painted `HitTarget` | [`route_mouse_click`] |
//! | Mouse wheel over a painted `HitTarget` | [`route_mouse_scroll`] |
//! | Divider drag motion (coalesced per render deadline) | [`resolve_pane_resize`] |
//! | Drag released without crossing the reorder threshold | [`route_drag_click`] |
//! | Tab drag released onto another hit target | [`resolve_tab_drop`] |
//! | Pane-grip drag released onto another pane | [`resolve_pane_drop`] |
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
use crate::dialog::{Dialog, SettingsSection, ViewRow};
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
    /// Swap the focused pane with whichever pane tmux considers its
    /// neighbour in `direction`. Carries no target for the same reason as
    /// [`Action::FocusDirection`].
    SwapPaneDirection(PaneDirection),
    /// Exchange the positions of two specific panes: the drop half of a
    /// pane drag. tmux swaps the contents; each pane keeps its id and takes
    /// the other's slot, and reconciliation reads the new layout back like
    /// any other structural change.
    SwapPanes {
        pane_id: String,
        other_pane_id: String,
    },
    /// Swap a specific pane with its neighbour in `direction`: the pane
    /// context menu's swap, which acts on the clicked pane rather than the
    /// focused one. The executor focuses `pane_id` first, because tmux's
    /// neighbour mnemonics only resolve against the current pane, and a
    /// swap leaves the acted-on pane focused anyway.
    SwapPaneToward {
        pane_id: String,
        direction: PaneDirection,
    },
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
    /// Scroll one wheel notch over a pane. `lines` carries direction and
    /// today's local scroll amount (negative is up); `at` is the
    /// pane-relative cell the notch landed on, 0-based, or `None` when
    /// hit-testing couldn't resolve one. Execution decides from there
    /// whether that forwards to tmux as an SGR mouse report, forwards as
    /// arrow keys, or moves this runtime's own Alacritty display offset —
    /// see `app::exec`'s `decide_scroll`.
    ScrollPane {
        pane_id: String,
        lines: i32,
        at: Option<crate::runtime::CellPos>,
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

    // -- Messaging --
    /// Open the composer. Carries no target: the recipient is typed as
    /// part of the message, so nothing has to be resolved to open it.
    RequestCompose,
    /// Repaint the whole workspace surface. Owns no application state: it
    /// only tells the renderer its remembered frame is not what the user
    /// is looking at.
    RequestRedraw,
    /// Send one addressed message, exactly as `cyclops send` would.
    ///
    /// The send is slow by design (the daemon holds the answer for the
    /// acknowledgement window), so whoever performs this must do it off
    /// the draw loop and report back. The composer stays open until it
    /// does.
    SendMessage {
        to: String,
        subject: String,
        body: String,
    },
    /// Type a file's path into the focused pane, as `@path `.
    ///
    /// Typed, not sent: it lands in whatever the pane is running, next to
    /// whatever is already on that line, and the operator says what to do
    /// with it. Every coding agent in the roster reads `@path` as "this
    /// file", which is why the sigil is here rather than left to the
    /// reader to add.
    ///
    /// The reference is already root-relative
    /// ([`crate::files::FileTree::reference`]); nothing downstream
    /// re-derives it, so what was clicked is what arrives.
    InsertFileRef {
        reference: String,
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
    /// Collapse or reopen the sidebar. Visibility persists, so a
    /// workspace quit while collapsed reopens collapsed.
    ToggleSidebar,
    /// Collapse or reopen the right-edge Messages pane.
    ToggleMessages,
    /// One verb clicked in the Messages pane's action strip, dispatched through
    /// the same handler its key press reaches.
    MessagesVerb(cyclops_ui::ChatAction),
    /// Show or hide the tab strip. Visible is the default and hiding is an
    /// explicit choice, so the `+` that makes tabs is on screen from a
    /// fresh install; the choice persists like the sidebar's.
    ToggleTabBar,
    /// Open or close the sidebar's file panel.
    /// Collapse a pane to its own title bar, or put it back to the height
    /// it had. Session state: a minimized pane is not a preference, it is
    /// where you left the furniture this afternoon.
    ToggleMinimizePane {
        pane_id: String,
    },
    ToggleFiles,
    /// Put the keyboard in the file panel, opening the sidebar and the
    /// panel first if either is away. A chord that focused a panel the
    /// operator cannot see would look like it did nothing.
    FocusFiles,
    ToggleMotion,
    /// Show the event stream: open the sidebar on its Stream tab. When
    /// the stream is already what's showing, hide the sidebar instead —
    /// the same "toggle the stream surface off, canvas back" meaning the
    /// old right-hand slide-out gave this action its name for.
    ToggleEventPanel,
    /// Select one of the sidebar's tabs (Sessions or Stream). The choice
    /// persists like the sidebar's width and visibility.
    SelectSidebarTab {
        tab: crate::persist::SidebarTab,
    },
    /// Open the settings card on one of its sections. Carries no other
    /// target: the theme listing is read from the themes directory at
    /// execution time, so a file added or removed between the click and
    /// the open cannot show a stale row, and the sound switch and the
    /// view switches are read from prefs.
    ShowSettings {
        section: SettingsSection,
    },
    /// Open the keybinding reference, a card of its own, from the
    /// router's live map.
    ShowKeybinds,
    /// Switch to a theme by name, exactly what `cyclops theme <name>`
    /// does: write the config key and nudge the daemon, whose theme event
    /// repaints this workspace through the existing hot-reload watch.
    ApplyTheme {
        name: String,
    },
    /// Save what the sound section has checked, the switch
    /// (`[workspace] sound_notifs`) and the cue (`[workspace] sound`, an
    /// installed stem or `crate::sound::SYSTEM`; `None` keeps the saved
    /// one when nothing on offer is checked), and close the card, the
    /// way a confirmed Apply on the theme section does.
    ApplySoundSettings {
        on: bool,
        cue: Option<String>,
    },
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
        // A keyboard swap acts on the focused pane, so it carries no
        // target, exactly like the Shift upgrade of the focus chords.
        // No target to resolve: the recipient is typed into the composer,
        // not taken from whatever pane happens to be focused.
        BindingAction::Compose => Some(Action::RequestCompose),
        BindingAction::Redraw => Some(Action::RequestRedraw),
        BindingAction::ToggleFiles => Some(Action::ToggleFiles),
        BindingAction::FocusFiles => Some(Action::FocusFiles),
        BindingAction::SwapPaneLeft => Some(Action::SwapPaneDirection(PaneDirection::Left)),
        BindingAction::SwapPaneRight => Some(Action::SwapPaneDirection(PaneDirection::Right)),
        BindingAction::SwapPaneUp => Some(Action::SwapPaneDirection(PaneDirection::Up)),
        BindingAction::SwapPaneDown => Some(Action::SwapPaneDirection(PaneDirection::Down)),
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
        BindingAction::ToggleSidebar => Some(Action::ToggleSidebar),
        BindingAction::ToggleMessages => Some(Action::ToggleMessages),
        BindingAction::ToggleTabBar => Some(Action::ToggleTabBar),
        BindingAction::ToggleMotion => Some(Action::ToggleMotion),
        BindingAction::ToggleEventPanel => Some(Action::ToggleEventPanel),
        BindingAction::ShowKeybinds => Some(Action::ShowKeybinds),
        BindingAction::ShowSettings => Some(Action::ShowSettings {
            section: SettingsSection::Theme,
        }),
    }
}

/// Route one resolved keyboard action whose key arrived with Shift held.
/// Shift upgrades the four directional focus chords to pane swaps
/// (Ctrl+B Shift+Arrow by default); every other binding resolves exactly
/// as [`route_binding`] would. The prefix router matches chords by key
/// code alone, so keyboard dispatch re-reads the modifier from the
/// original key event and picks this entry point.
pub fn route_binding_shifted(action: BindingAction, ctx: &RouteContext) -> Option<Action> {
    match action {
        BindingAction::FocusLeft => Some(Action::SwapPaneDirection(PaneDirection::Left)),
        BindingAction::FocusRight => Some(Action::SwapPaneDirection(PaneDirection::Right)),
        BindingAction::FocusUp => Some(Action::SwapPaneDirection(PaneDirection::Up)),
        BindingAction::FocusDown => Some(Action::SwapPaneDirection(PaneDirection::Down)),
        other => route_binding(other, ctx),
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
        // The menu's swap acts on the pane the menu was opened on, per the
        // module rule: the item resolves against THAT target.
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SwapPaneLeft) => {
            Some(Action::SwapPaneToward {
                pane_id: pane_id.clone(),
                direction: PaneDirection::Left,
            })
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SwapPaneRight) => {
            Some(Action::SwapPaneToward {
                pane_id: pane_id.clone(),
                direction: PaneDirection::Right,
            })
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SwapPaneUp) => {
            Some(Action::SwapPaneToward {
                pane_id: pane_id.clone(),
                direction: PaneDirection::Up,
            })
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SwapPaneDown) => {
            Some(Action::SwapPaneToward {
                pane_id: pane_id.clone(),
                direction: PaneDirection::Down,
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
        // A send already in flight has nothing to confirm: Enter twice must
        // not put the same message on the record twice.
        Dialog::Compose { send, .. } if send.is_sending() => None,
        // Confirmation is handled by the dialog owner. It is not a terminal
        // action and must never fall through into another send.
        Dialog::Compose { send, .. } if send.is_confirming_abandon() => None,
        // None here is a line that is not addressed yet, not a refusal to
        // act. The caller re-reads the same buffer with `parse_compose` to
        // get the sentence saying what is missing; keeping the reason out
        // of the return type is what lets this stay a pure route.
        Dialog::Compose { buffer, .. } => {
            crate::dialog::parse_compose(buffer)
                .ok()
                .map(|c| Action::SendMessage {
                    to: c.to,
                    subject: c.subject,
                    body: c.body,
                })
        }
        Dialog::RenameWorkspace { session, buffer } => {
            non_empty(buffer).map(|name| Action::RenameWorkspace {
                session: session.clone(),
                name,
            })
        }
        Dialog::ConfirmCloseWorkspace { session } => Some(Action::CloseWorkspace {
            session: session.clone(),
        }),
        // Enter applies the row the arrows are on, in the section that is
        // showing. An empty theme listing has nothing to apply, so Enter
        // dismisses like the keybinds card.
        Dialog::Settings {
            section: SettingsSection::Theme,
            themes,
            ..
        } => themes
            .names
            .get(themes.selected)
            .map(|name| Action::ApplyTheme { name: name.clone() }),
        // The view section's Enter flips the surface under the cursor,
        // at once and without closing the card: these are the app menu's
        // old toggles, and a switch that waited for Apply would be the
        // only one in the workspace that did. The card stays because the
        // action leaves the dialog alone; `exec::sync_view_switches`
        // moves the check.
        Dialog::Settings {
            section: SettingsSection::View,
            view,
            ..
        } => match view.selected_row() {
            Some(ViewRow::TabBar) => Some(Action::ToggleTabBar),
            Some(ViewRow::Files) => Some(Action::ToggleFiles),
            None => None,
        },
        // The sound section's Enter reads the checks, not the cursor:
        // landing on rows moved them, and both groups are saved at once.
        Dialog::Settings {
            section: SettingsSection::Sound,
            sound,
            ..
        } => Some(Action::ApplySoundSettings {
            on: sound.on,
            cue: sound.checked_sound().map(String::from),
        }),
        // Read-only: nothing to confirm. Enter just dismisses the card.
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
/// docs. Every other hit target that starts a drag on `Down` (pane
/// grips, tabs, sidebar rows, sidebar agents, dividers, the sidebar
/// splitter) resolves no action here; its click-without-drag fallback is
/// [`route_drag_click`], and its drag-release commit is
/// [`resolve_tab_drop`], [`resolve_pane_drop`],
/// `app::resolve_workspace_slot_drop`, or [`resolve_agent_drop`].
pub fn route_mouse_click(target: &HitTarget, button: MouseButton) -> Option<Action> {
    match (target, button) {
        (
            HitTarget::PaneBody { pane_id } | HitTarget::PaneFrame { pane_id },
            MouseButton::Left | MouseButton::Right,
        )
        // Left-down on the grip starts a swap drag instead; only the
        // right-click (which never drags) focuses immediately.
        | (HitTarget::PaneGrip { pane_id }, MouseButton::Right) => Some(Action::FocusPane {
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
        // The same action Ctrl+B @ routes to. A control that opened a
        // different composer from the chord would be a second code path
        // for one feature.
        (HitTarget::ComposeButton, MouseButton::Left) => Some(Action::RequestCompose),
        (HitTarget::SidebarTab { tab }, MouseButton::Left) => {
            Some(Action::SelectSidebarTab { tab: *tab })
        }
        (HitTarget::NewWorkspaceButton, MouseButton::Left) => Some(Action::NewWorkspace),
        // The chevron is the mouse's half of Ctrl+B b, on the panel edge
        // and on the collapsed rail alike.
        (HitTarget::SidebarToggle, MouseButton::Left) => Some(Action::ToggleSidebar),
        (HitTarget::MessagesToggle, MouseButton::Left) => Some(Action::ToggleMessages),
        // The strip's words are the mouse's half of the Messages pane's keys. The
        // action they raise is dispatched through the same key handler, so
        // there is one implementation of each verb rather than two.
        (HitTarget::MessagesAction(action), MouseButton::Left) => {
            Some(Action::MessagesVerb(*action))
        }
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
/// scrolls; every other target ignores the wheel today. `at` is the
/// pane-relative cell the caller already resolved via
/// `HitMap::pane_geometry` and [`crate::input::mouse::HitMap::cell_at`] —
/// this function only routes, it does no geometry lookups of its own.
pub fn route_mouse_scroll(
    target: &HitTarget,
    direction: ScrollDirection,
    at: Option<crate::runtime::CellPos>,
) -> Option<Action> {
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
        at,
    })
}

/// Route a drag release that never crossed its target's move threshold —
/// i.e. a click that happened to land on a draggable hit target. Each
/// target resolves to the same action a plain click on it would, using the
/// value the drag already carries (never a re-derived index).
pub fn route_drag_click(target: &DragTarget) -> Option<Action> {
    match target {
        DragTarget::Pane { pane_id } => Some(Action::FocusPane {
            pane_id: pane_id.clone(),
        }),
        DragTarget::Tab { window_id } => Some(Action::SelectTab {
            window_id: window_id.clone(),
        }),
        DragTarget::Workspace { session, .. } => Some(Action::SelectWorkspace {
            session: session.clone(),
        }),
        DragTarget::Agent { pane_id, .. } => Some(Action::FocusPane {
            pane_id: pane_id.clone(),
        }),
        // A seam grabbed through a pane's own top border focuses that pane
        // when the press never became a drag: the title strip painted there
        // is a focus control, and pressing it must still do what pressing
        // it always did. A seam grabbed in the bare gutter carries no pane
        // and stays a no-op, as does a title bar: neither has anything to
        // select.
        DragTarget::Divider { focus_on_click, .. } => focus_on_click
            .clone()
            .map(|pane_id| Action::FocusPane { pane_id }),
        DragTarget::Sidebar
        | DragTarget::Messages
        | DragTarget::SidebarSplit
        | DragTarget::Dialog => None,
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

/// Resolve a pane drag released on another pane: dropping on a different
/// pane's body, frame, or grip swaps the two. Dropping a pane on itself,
/// or on anything that is not a pane, resolves nothing.
pub fn resolve_pane_drop(pane_id: &str, drop: &HitTarget) -> Option<Action> {
    match drop {
        HitTarget::PaneBody { pane_id: dst }
        | HitTarget::PaneFrame { pane_id: dst }
        | HitTarget::PaneGrip { pane_id: dst }
            if dst != pane_id =>
        {
            Some(Action::SwapPanes {
                pane_id: pane_id.to_string(),
                other_pane_id: dst.clone(),
            })
        }
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
    use crate::dialog::Dialog;
    use crate::layout::ResolvedLayout;

    /// The composer's Enter carries the recipient the operator typed, not
    /// one resolved from focus, and its body is the text as typed.
    #[test]
    fn a_composed_line_confirms_into_the_send_it_says() {
        let dialog = Dialog::Compose {
            buffer: "@reviewer gateway.rs:120 drops the burst path".into(),
            status: None,
            send: crate::dialog::ComposeSendState::Ready,
        };
        assert_eq!(
            route_dialog_confirm(&dialog),
            Some(Action::SendMessage {
                to: "reviewer".into(),
                subject: "gateway.rs:120 drops the burst path".into(),
                body: "gateway.rs:120 drops the burst path".into(),
            })
        );
    }

    /// Two guards, and each is a different mistake.
    ///
    /// A line with no recipient is unfinished, not a command to run; Enter
    /// has to leave the dialog open so the composer can say what is
    /// missing. A send already in flight has nothing to confirm, and a
    /// second Enter must not put the same message on the record twice.
    #[test]
    fn an_unaddressed_or_in_flight_composer_sends_nothing() {
        for buffer in ["", "@", "@reviewer", "no recipient here"] {
            let dialog = Dialog::Compose {
                buffer: buffer.into(),
                status: None,
                send: crate::dialog::ComposeSendState::Ready,
            };
            assert_eq!(route_dialog_confirm(&dialog), None, "{buffer:?} sent");
        }
        let in_flight = Dialog::Compose {
            buffer: "@reviewer ship it".into(),
            status: Some("sending to reviewer…".into()),
            send: crate::dialog::ComposeSendState::Sending(crate::dialog::ComposeAttempt {
                message: crate::dialog::parse_compose("@reviewer ship it").expect("message"),
                client_key: "key".into(),
            }),
        };
        assert_eq!(route_dialog_confirm(&in_flight), None);
    }

    /// Opening the composer resolves no target: the recipient is typed, so
    /// the chord works from any pane including one nothing is named in.
    #[test]
    fn the_compose_chord_needs_no_target() {
        let tabs = [tab("@0")];
        let spaces = [workspace("$0", "main")];
        assert_eq!(
            route_binding(
                BindingAction::Compose,
                &ctx(&tabs, 0, "%0", "$0", &spaces, 0)
            ),
            Some(Action::RequestCompose)
        );
    }

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

    // -- Sidebar toggles and tabs: chords, the app menu, and the tab
    // header's chips all resolve into the one vocabulary. --

    /// The old panel chord and menu item still resolve to
    /// [`Action::ToggleEventPanel`] — whose meaning is now "the sidebar,
    /// on the Stream tab" — and the new collapse chord resolves beside
    /// it, from keyboard and app menu alike.
    #[test]
    fn sidebar_toggles_route_from_keyboard_and_app_menu() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%3", "main", &workspaces, 0);

        assert_eq!(
            route_binding(BindingAction::ToggleSidebar, &c),
            Some(Action::ToggleSidebar)
        );
        assert_eq!(
            route_menu_item(&MenuState::AppMenu, BindingAction::ToggleSidebar, &c),
            Some(Action::ToggleSidebar)
        );
        assert_eq!(
            route_binding(BindingAction::ToggleEventPanel, &c),
            Some(Action::ToggleEventPanel)
        );
        assert_eq!(
            route_menu_item(&MenuState::AppMenu, BindingAction::ToggleEventPanel, &c),
            Some(Action::ToggleEventPanel)
        );
    }

    /// A click on a header chip selects that tab; the header answers no
    /// other button, so a right-click there opens nothing by accident.
    #[test]
    fn a_sidebar_tab_click_selects_that_tab() {
        use crate::persist::SidebarTab;

        let stream = HitTarget::SidebarTab {
            tab: SidebarTab::Stream,
        };
        assert_eq!(
            route_mouse_click(&stream, MouseButton::Left),
            Some(Action::SelectSidebarTab {
                tab: SidebarTab::Stream
            })
        );
        assert_eq!(
            route_mouse_click(
                &HitTarget::SidebarTab {
                    tab: SidebarTab::Sessions
                },
                MouseButton::Left
            ),
            Some(Action::SelectSidebarTab {
                tab: SidebarTab::Sessions
            })
        );
        assert_eq!(route_mouse_click(&stream, MouseButton::Right), None);
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

    /// The menu's swap carries the clicked pane and the keyboard's stays
    /// targetless: right-clicking a background pane must swap THAT pane,
    /// while a chord acts on whatever tmux considers current.
    #[test]
    fn context_menu_swap_targets_the_clicked_pane_keyboard_stays_targetless() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%3", "main", &workspaces, 0);

        let from_menu = route_menu_item(
            &MenuState::ContextMenu {
                pane_id: "%7".into(),
                at: (0, 0),
            },
            BindingAction::SwapPaneLeft,
            &c,
        );
        assert_eq!(
            from_menu,
            Some(Action::SwapPaneToward {
                pane_id: "%7".into(),
                direction: PaneDirection::Left,
            })
        );

        let from_keyboard = route_binding(BindingAction::SwapPaneDown, &c);
        assert_eq!(
            from_keyboard,
            Some(Action::SwapPaneDirection(PaneDirection::Down))
        );
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

    /// Nothing on the keybinds card is chosen, so its Enter has no
    /// action and the card closes the way an empty theme list's does.
    #[test]
    fn the_keybinds_card_confirms_to_no_action() {
        let dialog = Dialog::Keybinds {
            scroll: 3,
            rows: vec![crate::bindings::BindingHelp {
                keys: "Ctrl+B ?".into(),
                action: "Keybinds".into(),
            }],
        };
        assert_eq!(route_dialog_confirm(&dialog), None);
    }

    /// The view section's Enter is the app menu's old toggle: it names
    /// the surface under the cursor and nothing else, so the card can
    /// stay open while the surface flips.
    #[test]
    fn the_view_section_confirms_to_the_switch_under_the_cursor() {
        let mut dialog = settings(SettingsSection::View, Vec::new(), 0);
        assert_eq!(route_dialog_confirm(&dialog), Some(Action::ToggleTabBar));
        if let Dialog::Settings { view, .. } = &mut dialog {
            view.selected = 1;
        }
        assert_eq!(route_dialog_confirm(&dialog), Some(Action::ToggleFiles));
    }

    #[test]
    fn show_settings_agrees_across_keyboard_and_app_menu() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);

        assert_eq!(
            route_binding(BindingAction::ShowSettings, &c),
            Some(Action::ShowSettings {
                section: SettingsSection::Theme
            })
        );
        // The keybinding reference is its own card, and `Ctrl+B ?` opens
        // it directly.
        assert_eq!(
            route_binding(BindingAction::ShowKeybinds, &c),
            Some(Action::ShowKeybinds)
        );
        assert_eq!(
            route_menu_item(&MenuState::AppMenu, BindingAction::ShowSettings, &c),
            Some(Action::ShowSettings {
                section: SettingsSection::Theme
            })
        );
        // The menu's Keybinds row is the chord's twin: the same card.
        assert_eq!(
            route_menu_item(&MenuState::AppMenu, BindingAction::ShowKeybinds, &c),
            Some(Action::ShowKeybinds)
        );
    }

    fn settings(section: SettingsSection, names: Vec<String>, sound_selected: usize) -> Dialog {
        Dialog::Settings {
            section,
            themes: crate::dialog::ThemePicker {
                names,
                selected: 1,
                active: Some(0),
                notice: None,
            },
            view: crate::dialog::ViewSwitches::new(true, false),
            sound: crate::dialog::SoundPicker {
                selected: sound_selected,
                on: false,
                sounds: vec!["bow-ripple".into(), "system".into()],
                active_sound: Some(0),
                previewed: None,
            },
        }
    }

    /// Enter answers the section that is showing: the theme under the
    /// cursor on one, the sound section's checks on the other, each
    /// blind to the other's state.
    #[test]
    fn settings_confirms_the_selected_row_of_the_showing_section() {
        let names = vec!["dark".to_string(), "light".into(), "solar".into()];
        assert_eq!(
            route_dialog_confirm(&settings(SettingsSection::Theme, names.clone(), 0)),
            Some(Action::ApplyTheme {
                name: "light".into()
            })
        );
        // The sound section answers with its checks, wherever the cursor
        // is: the helper's picker has the switch off and the shipped cue
        // checked.
        for cursor in [0, 3, 4] {
            assert_eq!(
                route_dialog_confirm(&settings(SettingsSection::Sound, names.clone(), cursor)),
                Some(Action::ApplySoundSettings {
                    on: false,
                    cue: Some("bow-ripple".into())
                }),
                "cursor on row {cursor}"
            );
        }
        let mut checked = settings(SettingsSection::Sound, Vec::new(), 3);
        if let Dialog::Settings { sound, .. } = &mut checked {
            sound.selected = 0;
            sound.check_selected();
            sound.selected = 3;
            sound.check_selected();
        }
        assert_eq!(
            route_dialog_confirm(&checked),
            Some(Action::ApplySoundSettings {
                on: true,
                cue: Some("system".into())
            }),
            "what landing checked is what Enter saves"
        );
    }

    #[test]
    fn an_empty_theme_listing_confirms_to_no_action() {
        assert_eq!(
            route_dialog_confirm(&settings(SettingsSection::Theme, Vec::new(), 0)),
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

    // -- Pane swap: shifted focus chords and the pane drag's drop. --

    #[test]
    fn shifted_focus_chords_resolve_to_directional_swaps() {
        let tabs = [tab("@1")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);

        for (binding, direction) in [
            (BindingAction::FocusLeft, PaneDirection::Left),
            (BindingAction::FocusRight, PaneDirection::Right),
            (BindingAction::FocusUp, PaneDirection::Up),
            (BindingAction::FocusDown, PaneDirection::Down),
        ] {
            assert_eq!(
                route_binding_shifted(binding, &c),
                Some(Action::SwapPaneDirection(direction))
            );
        }
    }

    #[test]
    fn shifted_non_focus_chords_resolve_like_the_unshifted_binding() {
        let tabs = [tab("@1"), tab("@2")];
        let workspaces = [workspace("$1", "main")];
        let c = ctx(&tabs, 0, "%0", "main", &workspaces, 0);

        // Shift only means swap on the focus chords; a stray shift on any
        // other binding must not change its meaning.
        assert_eq!(
            route_binding_shifted(BindingAction::NextTab, &c),
            route_binding(BindingAction::NextTab, &c)
        );
        assert_eq!(
            route_binding_shifted(BindingAction::ClosePane, &c),
            route_binding(BindingAction::ClosePane, &c)
        );
    }

    #[test]
    fn resolve_pane_drop_onto_another_panes_body_frame_or_grip_swaps() {
        let expected = Some(Action::SwapPanes {
            pane_id: "%1".into(),
            other_pane_id: "%2".into(),
        });
        assert_eq!(
            resolve_pane_drop(
                "%1",
                &HitTarget::PaneBody {
                    pane_id: "%2".into()
                }
            ),
            expected
        );
        assert_eq!(
            resolve_pane_drop(
                "%1",
                &HitTarget::PaneFrame {
                    pane_id: "%2".into()
                }
            ),
            expected
        );
        // The drop target's own grip cell is part of its frame corner; a
        // release there must swap like any other frame cell always did.
        assert_eq!(
            resolve_pane_drop(
                "%1",
                &HitTarget::PaneGrip {
                    pane_id: "%2".into()
                }
            ),
            expected
        );
    }

    /// The frame focuses on either button now that the swap pickup lives
    /// on the grip; the grip itself resolves no click on left-down, since
    /// that down starts the swap drag and its below-threshold fallback is
    /// [`route_drag_click`]'s focus.
    #[test]
    fn a_frame_click_focuses_and_the_grip_left_down_resolves_nothing() {
        let frame = HitTarget::PaneFrame {
            pane_id: "%5".into(),
        };
        let grip = HitTarget::PaneGrip {
            pane_id: "%5".into(),
        };
        let expected = Some(Action::FocusPane {
            pane_id: "%5".into(),
        });
        assert_eq!(route_mouse_click(&frame, MouseButton::Left), expected);
        assert_eq!(route_mouse_click(&frame, MouseButton::Right), expected);
        assert_eq!(route_mouse_click(&grip, MouseButton::Right), expected);
        assert_eq!(route_mouse_click(&grip, MouseButton::Left), None);
        assert_eq!(
            route_drag_click(&DragTarget::Pane {
                pane_id: "%5".into()
            }),
            expected
        );
    }

    #[test]
    fn resolve_pane_drop_onto_itself_resolves_nothing() {
        assert_eq!(
            resolve_pane_drop(
                "%1",
                &HitTarget::PaneBody {
                    pane_id: "%1".into()
                }
            ),
            None
        );
        assert_eq!(
            resolve_pane_drop(
                "%1",
                &HitTarget::PaneFrame {
                    pane_id: "%1".into()
                }
            ),
            None
        );
        // A zoomed pane's grip has nowhere to go: a self-drop is inert.
        assert_eq!(
            resolve_pane_drop(
                "%1",
                &HitTarget::PaneGrip {
                    pane_id: "%1".into()
                }
            ),
            None
        );
    }

    #[test]
    fn resolve_pane_drop_onto_a_non_pane_target_resolves_nothing() {
        assert_eq!(
            resolve_pane_drop(
                "%1",
                &HitTarget::Tab {
                    window_id: "@1".into()
                }
            ),
            None
        );
        assert_eq!(resolve_pane_drop("%1", &HitTarget::NewTabButton), None);
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
            route_drag_click(&DragTarget::Pane {
                pane_id: "%1".into()
            }),
            Some(Action::FocusPane {
                pane_id: "%1".into()
            })
        );
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
        // A seam grabbed in the bare gutter: nothing was pressed, so a
        // release that never moved has nothing to focus.
        assert_eq!(
            route_drag_click(&DragTarget::Divider {
                pane_id: "%1".into(),
                dir: SplitDir::Horizontal,
                focus_on_click: None,
            }),
            None
        );
        // The same seam grabbed through a pane's own border. `%1` is the
        // resize target on the far side; `%2` is the border that was
        // pressed, and it is the one a click focuses.
        assert_eq!(
            route_drag_click(&DragTarget::Divider {
                pane_id: "%1".into(),
                dir: SplitDir::Vertical,
                focus_on_click: Some("%2".into()),
            }),
            Some(Action::FocusPane {
                pane_id: "%2".into()
            })
        );
        assert_eq!(route_drag_click(&DragTarget::Sidebar), None);
        assert_eq!(route_drag_click(&DragTarget::Dialog), None);
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
        let at = Some(crate::runtime::CellPos { col: 4, row: 2 });
        assert_eq!(
            route_mouse_scroll(
                &HitTarget::PaneBody {
                    pane_id: "%1".into()
                },
                ScrollDirection::Up,
                at,
            ),
            Some(Action::ScrollPane {
                pane_id: "%1".into(),
                lines: -3,
                at,
            })
        );
        assert_eq!(
            route_mouse_scroll(
                &HitTarget::PaneBody {
                    pane_id: "%1".into()
                },
                ScrollDirection::Down,
                at,
            ),
            Some(Action::ScrollPane {
                pane_id: "%1".into(),
                lines: 3,
                at,
            })
        );
    }

    #[test]
    fn wheel_over_a_non_pane_target_resolves_nothing() {
        assert_eq!(
            route_mouse_scroll(&HitTarget::NewTabButton, ScrollDirection::Up, None),
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
