//! The one action executor.
//!
//! Every device resolves an [`Action`](crate::action::Action) (see
//! `crate::action`'s routing functions); [`execute`] is the only place that
//! turns one into a mutation. It validates the stable target against
//! current state, calls the typed `cyclops-tmux` operation or a daemon/
//! persistence helper, and returns an [`Outcome`] the caller applies —
//! never a scattered `app.needs_reconcile = true` or an inline
//! `persist::save_prefs` at the call site.
//!
//! Two rules this module holds itself to:
//!
//! 1. **The model updates only from reconciliation.** Like `intent.rs`
//!    before it, nothing here inserts, removes, or reorders tmux-owned
//!    structure (tabs, panes, workspaces) directly — that always comes back
//!    through a structural notification. The one exception is sidebar
//!    *presentation* order ([`Action::ReorderWorkspace`],
//!    [`Action::ReorderAgent`]): tmux has no concept of session or agent
//!    ordering, so there is nothing for reconciliation to ever tell us and
//!    this is the only place that order lives.
//! 2. **Transient UI state is this module's to own.** Dialogs, hover, and
//!    the pane runtimes' local scroll offset are updated directly; that is
//!    the "confirmation flows stay execution-time" rule — see
//!    [`close_pane`] and [`close_tab`] for the shape of it.

use std::path::{Path, PathBuf};

use cyclops_tmux::{ControlClient, TmuxError};
use uuid::Uuid;

use super::App;
use crate::action::{Action, Insertion, TabDestination};
use crate::copy;
use crate::daemon;
use crate::decoration::{self, DecorationSnapshot};
use crate::dialog::{
    Composed, Dialog, SettingsSection, SoundPicker, SoundRow, ThemePicker, ViewSwitches,
};
use crate::naming;
use crate::persist::SidebarTab;

/// What the caller must do after one action executed. Every field defaults
/// to false ("nothing beyond what already happened"); an arm sets exactly
/// the ones it earned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Outcome {
    /// A structural tmux change is in flight; the next render deadline
    /// should replace the model from a fresh snapshot rather than trust
    /// local state.
    pub(super) reconcile: bool,
    /// `app.prefs` changed and belongs on disk.
    pub(super) persist: bool,
    /// The user asked to detach; the event loop should exit.
    pub(super) detach: bool,
}

impl Outcome {
    fn reconcile() -> Self {
        Outcome {
            reconcile: true,
            ..Outcome::default()
        }
    }
}

/// Execute one resolved action: validate its target, call the adapter or a
/// daemon/persistence helper, and report what changed.
pub(super) async fn execute(
    app: &mut App,
    client: &ControlClient,
    action: Action,
) -> Result<Outcome, TmuxError> {
    match action {
        Action::Split { pane_id, direction } => {
            client.split_window(&pane_id, direction).await?;
            Ok(Outcome::reconcile())
        }
        Action::FocusPane { pane_id } => focus_pane(app, client, &pane_id).await,
        Action::FocusDirection(direction) => {
            client.select_pane_toward(direction).await?;
            Ok(Outcome::reconcile())
        }
        Action::SwapPaneDirection(direction) => {
            // At an edge with no neighbour tmux answers with an error,
            // which the caller logs like any other failed command.
            client.swap_pane_toward(direction).await?;
            Ok(Outcome::reconcile())
        }
        Action::SwapPanes {
            pane_id,
            other_pane_id,
        } => {
            // The dragged pane rides in `-t`: tmux focuses `-t` after a
            // swap, so the pane the user just dropped ends up focused in
            // its new slot, the same way a frame click focuses it.
            client.swap_pane(&other_pane_id, &pane_id).await?;
            Ok(Outcome::reconcile())
        }
        Action::SwapPaneToward { pane_id, direction } => {
            // Focus first: tmux's neighbour mnemonics resolve against the
            // current pane only, and a swap leaves the acted-on pane
            // focused anyway, so the menu's clicked-pane swap ends in the
            // same place a keyboard swap of that pane would.
            client.select_pane(&pane_id).await?;
            client.swap_pane_toward(direction).await?;
            Ok(Outcome::reconcile())
        }
        Action::ClosePane { pane_id } => close_pane(app, client, pane_id).await,
        Action::ZoomPane { pane_id } => {
            client.toggle_pane_zoom(&pane_id).await?;
            Ok(Outcome::reconcile())
        }
        Action::ResizePane {
            pane_id,
            direction,
            cells,
        } => {
            // No reconcile: tmux's own `%layout-change` notification updates
            // the model, and asking for a full reconcile on every coalesced
            // drag step would make dragging feel like it lags the mouse.
            client.resize_pane(&pane_id, direction, cells).await?;
            Ok(Outcome::default())
        }
        Action::ScrollPane { pane_id, lines, at } => {
            scroll_pane(app, client, pane_id, lines, at).await
        }
        Action::RequestNamePane { pane_id } => {
            let buffer = app
                .decoration
                .pane(&pane_id)
                .and_then(|decoration| decoration.label.clone())
                .unwrap_or_default();
            app.open_dialog(Dialog::NamePane {
                pane_id,
                buffer,
                error: None,
            });
            Ok(Outcome::default())
        }
        Action::NamePane { pane_id, label } => name_pane(app, pane_id, label),

        Action::RequestRedraw => {
            // The renderer drains this before its next frame. Nothing else
            // is touched: not the daemon, not a pane, not the mailbox, not
            // the layout.
            app.repaint_requested = true;
            Ok(Outcome::default())
        }
        Action::RequestCompose => {
            // Prefilled with the focused pane's label when it has one,
            // because the agent you are looking at is the one you are
            // usually writing to. It is only a prefill: the name is part of
            // the text, so overtyping it addresses someone else.
            let buffer = app
                .decoration
                .pane(&app.model.active_tab().active_pane)
                .and_then(|d| d.label.clone())
                .map(|label| format!("@{label} "))
                .unwrap_or_else(|| "@".to_string());
            app.open_dialog(Dialog::Compose {
                buffer,
                status: None,
                send: crate::dialog::ComposeSendState::Ready,
            });
            Ok(Outcome::default())
        }
        Action::SendMessage { to, subject, body } => {
            send_message(app, to, subject, body);
            Ok(Outcome::default())
        }
        Action::InsertFileRef { reference } => insert_file_ref(app, client, reference).await,

        Action::RequestNewTab => {
            // Prefilled with the name the tab would get anyway, rather than
            // left blank for the same rule to apply invisibly downstream.
            // Enter alone still does the common thing, and now the operator
            // can see what that thing is and edit it instead of guessing.
            app.open_dialog(Dialog::NewTab {
                buffer: next_numeric_tab_name(&app.model.session.tabs),
            });
            Ok(Outcome::default())
        }
        Action::NewTab { name } => {
            // A tab appearing or the strip gaining a row changes what the
            // canvas owns.
            app.layout_changed();
            new_tab(app, client, name).await
        }
        Action::SelectTab { window_id } => {
            // A different tab is a different pane layout entirely.
            app.layout_changed();
            select_tab(app, client, window_id).await
        }
        Action::RequestRenameTab { window_id } => {
            let Some(tab) = app
                .model
                .session
                .tabs
                .iter()
                .find(|tab| tab.window_id == window_id)
            else {
                return Ok(Outcome::default());
            };
            app.open_dialog(Dialog::RenameTab {
                window_id: tab.window_id.clone(),
                buffer: tab.name.clone(),
            });
            Ok(Outcome::default())
        }
        Action::RenameTab { window_id, name } => {
            client.rename_window(&window_id, &name).await?;
            app.dialog = None;
            app.hover = None;
            Ok(Outcome::reconcile())
        }
        Action::CloseTab { window_id } => {
            app.layout_changed();
            close_tab(app, client, window_id).await
        }
        Action::MoveTab {
            window_id,
            destination,
        } => {
            match destination {
                TabDestination::SwapWithTab(dst) => {
                    client.swap_window(&window_id, &dst).await?;
                }
                TabDestination::ToWorkspace(session) => {
                    client.move_window_to_session(&window_id, &session).await?;
                }
            }
            Ok(Outcome::reconcile())
        }

        Action::NewWorkspace => new_workspace(app, client).await,
        Action::SelectWorkspace { session } => {
            client.switch_to_session(&session).await?;
            Ok(Outcome::reconcile())
        }
        Action::RequestRenameWorkspace { session } => {
            app.open_dialog(Dialog::RenameWorkspace {
                buffer: session.clone(),
                session,
            });
            Ok(Outcome::default())
        }
        Action::RenameWorkspace { session, name } => {
            rename_workspace(app, client, session, name).await
        }
        Action::RequestCloseWorkspace { session } => {
            app.open_dialog(Dialog::ConfirmCloseWorkspace { session });
            Ok(Outcome::default())
        }
        Action::CloseWorkspace { session } => close_workspace(app, client, session).await,
        Action::ReorderWorkspace {
            session_id,
            insertion,
        } => Ok(reorder_workspace(app, session_id, insertion)),
        Action::ReorderAgent {
            workspace_id,
            order_key,
            insertion,
        } => Ok(reorder_agent(app, workspace_id, order_key, insertion)),

        Action::ToggleSidebar => {
            app.model.sidebar_visible = !app.model.sidebar_visible;
            Ok(commit_sidebar_visibility(app, client).await)
        }
        Action::ToggleMessages => {
            app.model.messages_visible = !app.model.messages_visible;
            app.prefs.messages_visible = app.model.messages_visible;
            app.layout_changed();
            if app.model.messages_visible {
                app.messages_focused = true;
                super::request_messages_snapshot(app);
            } else {
                app.messages_focused = false;
            }
            super::resize_client(app, client).await;
            Ok(Outcome {
                persist: true,
                ..Outcome::default()
            })
        }
        Action::MessagesVerb(verb) => {
            // The click is dispatched as the key press its own label names,
            // through the one handler that implements the verb. A pointer
            // user therefore gets exactly the keyboard behaviour, including
            // its refusals, and there is no second copy to drift.
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let (code, modifiers) = match verb {
                cyclops_ui::ChatAction::Reply => (KeyCode::Char('r'), KeyModifiers::NONE),
                cyclops_ui::ChatAction::Announce => (KeyCode::Char('a'), KeyModifiers::NONE),
                cyclops_ui::ChatAction::Open => (KeyCode::Enter, KeyModifiers::NONE),
                cyclops_ui::ChatAction::Scope => (KeyCode::Char('s'), KeyModifiers::NONE),
                cyclops_ui::ChatAction::Sessions => (KeyCode::Char('t'), KeyModifiers::NONE),
                cyclops_ui::ChatAction::Retry => (KeyCode::Char('r'), KeyModifiers::CONTROL),
            };
            // Clicking a verb in the strip is also a statement about where
            // the operator is working, so the drawer takes focus first for
            // the verbs that read the selection.
            app.messages_focused = true;
            super::handle_messages_key(app, KeyEvent::new(code, modifiers)).await?;
            Ok(Outcome::default())
        }
        Action::ToggleTabBar => {
            // The strip's row moves between chrome and the declared grid
            // whole, so every flip re-declares the client size exactly the
            // way a sidebar collapse does. Nothing about the tab count
            // enters into it: the strip shows because the operator has not
            // said otherwise.
            app.prefs.tab_bar_visible = !app.prefs.tab_bar_visible;
            app.layout_changed();
            super::resize_client(app, client).await;
            sync_view_switches(app);
            Ok(Outcome {
                persist: true,
                ..Outcome::default()
            })
        }
        Action::ToggleFiles => {
            // Stored as the row count, so "off" is zero rows and "on" is
            // whatever it was last. There is no separate visibility flag to
            // fall out of step with the size.
            app.prefs.files_rows = if app.prefs.files_rows == 0 {
                crate::persist::WorkspacePrefs::default().files_rows
            } else {
                0
            };
            // No `resize_client`: the seam is inside the sidebar, so no
            // column changed hands and no pane reflows. The rows the panel
            // occupied still hold its glyphs, though, which is why the
            // repaint is requested here and not folded into the resize.
            app.layout_changed();
            sync_view_switches(app);
            Ok(Outcome {
                persist: true,
                ..Outcome::default()
            })
        }
        Action::ToggleMinimizePane { pane_id } => {
            // The height to go back to is the height it has right now, read
            // from the frame that is on screen. tmux is the authority on
            // pane geometry and the render follows it 1:1, so the painted
            // height IS the tmux height.
            match app.minimized.remove(&pane_id) {
                Some(rows) => {
                    client.resize_pane_height(&pane_id, rows).await?;
                }
                None => {
                    let Some(geometry) = app.hit_map.pane_geometry(&pane_id) else {
                        // Nothing painted it this frame, so there is no
                        // height to remember and nothing to collapse.
                        return Ok(Outcome::default());
                    };
                    let was = geometry.inner.height;
                    // A pane already at the floor has nothing to give, and
                    // recording that as its restore height would make the
                    // restore a no-op forever after.
                    if was <= crate::render::MINIMIZED_ROWS {
                        return Ok(Outcome::default());
                    }
                    app.minimized.insert(pane_id.clone(), was);
                    client
                        .resize_pane_height(&pane_id, crate::render::MINIMIZED_ROWS)
                        .await?;
                }
            }
            // tmux moved the layout, so the model has to be re-read before
            // the next frame paints panes at the old geometry.
            Ok(Outcome {
                reconcile: true,
                ..Outcome::default()
            })
        }
        Action::FocusFiles => {
            // Open what the cursor is going to live in. Focusing a panel
            // the operator cannot see reads as a chord that did nothing.
            let was_hidden = !app.model.sidebar_visible;
            app.model.sidebar_visible = true;
            if app.prefs.files_rows == 0 {
                app.prefs.files_rows = crate::persist::WorkspacePrefs::default().files_rows;
            }
            app.files_tree_mut().take_cursor();
            if was_hidden {
                // The panel took columns back from the canvas, so tmux has
                // to be told before the next frame paints panes at the old
                // width.
                return Ok(commit_sidebar_visibility(app, client).await);
            }
            Ok(Outcome {
                persist: true,
                ..Outcome::default()
            })
        }
        Action::ToggleMotion => {
            // No client-size redeclaration and no reflow: a fade is a
            // color, so turning it off changes what the next frame paints
            // and nothing about the grid. The live clock is not touched
            // here because it does not live on `App`; `draw` reads this
            // preference every frame and settles the clock when it goes
            // false, which also cancels whatever was mid-fade.
            app.prefs.motion = !app.prefs.motion;
            Ok(Outcome {
                persist: true,
                ..Outcome::default()
            })
        }
        Action::ToggleEventPanel => {
            // "Show me the stream", and pressed again, "take it away": the
            // stream goes off and the session list comes back, which is
            // what turning the old right-hand panel off did to this half
            // of the screen. Turning it off must never hide the sidebar
            // instead, or a keyboard-only operator lands on Stream with no
            // route back to Sessions and the choice persists (visibility
            // belongs to Ctrl+B b and the edge chevron, not to this). A
            // hidden sidebar opens on Stream.
            //
            // While the Stream tab is off (`persist::STREAM_TAB`) this
            // chord has nowhere to go, so it opens the sidebar and stops
            // rather than toggling to a tab the panel will not paint. That
            // is honest: the sidebar appears, which is half of what the
            // chord promises, and nothing lands the operator on a body with
            // no chip to leave it by.
            let tab = if crate::persist::STREAM_TAB {
                let show_stream =
                    !(app.model.sidebar_visible && app.sidebar_tab == SidebarTab::Stream);
                if show_stream {
                    SidebarTab::Stream
                } else {
                    SidebarTab::Sessions
                }
            } else {
                SidebarTab::default()
            };
            app.sidebar_tab = tab;
            app.prefs.sidebar_tab = tab;
            let was_visible = app.model.sidebar_visible;
            app.model.sidebar_visible = true;
            if was_visible {
                // Same columns, different body: tmux has nothing to be told.
                return Ok(Outcome {
                    persist: true,
                    ..Outcome::default()
                });
            }
            Ok(commit_sidebar_visibility(app, client).await)
        }
        Action::SelectSidebarTab { tab } => {
            // No resize: the sidebar keeps its columns, only its body
            // changes, so tmux has nothing to be told.
            //
            // Coerced, because a tab that is no longer offered must not be
            // reachable by any route. The chips that raise this action are
            // not painted for one, but the action is public.
            let tab = tab.available();
            app.sidebar_tab = tab;
            app.prefs.sidebar_tab = tab;
            Ok(Outcome {
                persist: true,
                ..Outcome::default()
            })
        }
        Action::ShowSettings { section } => {
            let (names, active) = theme_rows(&app.home);
            // What close-without-apply puts back: browsing previews into
            // `paint.theme` directly (see [`preview_selected_theme`]), so
            // the theme that is live right now rides beside the picker.
            app.theme_restore = Some(app.paint.theme.clone());
            app.open_dialog(Dialog::Settings {
                section,
                themes: ThemePicker {
                    selected: active.unwrap_or(0),
                    names,
                    active,
                    notice: None,
                },
                view: ViewSwitches::new(app.prefs.tab_bar_visible, app.prefs.files_rows > 0),
                sound: SoundPicker::new(
                    app.prefs.sound_notifs,
                    crate::sound::choices(&app.home),
                    &app.prefs.sound,
                ),
            });
            Ok(Outcome::default())
        }
        Action::ShowKeybinds => {
            // From the router's live map, not from documentation, so a
            // rebinding in config.toml is what the card teaches.
            app.open_dialog(Dialog::Keybinds {
                scroll: 0,
                rows: app.router.help(),
            });
            Ok(Outcome::default())
        }
        Action::ApplyTheme { name } => Ok(apply_theme(app, &name)),
        Action::ApplySoundSettings { on, cue } => {
            app.prefs.sound_notifs = on;
            if let Some(cue) = cue {
                app.prefs.sound = cue;
            }
            // Closing the card closes its theme section too: a theme
            // browsed but never applied goes back, exactly as Esc does.
            super::dialog_cancel(app);
            Ok(Outcome {
                persist: true,
                ..Outcome::default()
            })
        }
        Action::Detach => Ok(Outcome {
            detach: true,
            ..Outcome::default()
        }),
    }
}

/// Finish a sidebar show/hide: mirror the model's new visibility into
/// prefs, re-declare the tmux client size for the width the canvas just
/// gained or lost, and ask the caller to persist.
///
/// Every collapse and reopen reflows every pane, the same cost the old
/// panel toggle carried. Visibility is persisted rather than reset at
/// boot, so a workspace quit collapsed reopens collapsed.
async fn commit_sidebar_visibility(app: &mut App, client: &ControlClient) -> Outcome {
    app.prefs.sidebar_visible = app.model.sidebar_visible;
    app.layout_changed();
    super::resize_client(app, client).await;
    Outcome {
        persist: true,
        ..Outcome::default()
    }
}

/// What one wheel notch over a pane resolves to, decided by
/// [`decide_scroll`] from that pane's own terminal modes rather than any
/// global setting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScrollDecision {
    /// Forward one SGR mouse report, byte-exact, through tmux.
    ForwardSgr(String),
    /// Forward this many presses of the named arrow key through tmux.
    Arrows(&'static str, usize),
    /// Move this runtime's own scroll offset by this many lines.
    Local(i32),
}

/// Decide what one wheel notch over a pane should do — the same choice a
/// real terminal emulator makes for the program it hosts, restated as a
/// pure function so it needs no tmux client to test. A program that
/// enabled SGR mouse reporting gets the notch forwarded as one SGR event
/// (never scaled by `lines`'s magnitude — the far end applies its own
/// scroll amount per event, the same way this app does for its own local
/// path). A program on the alternate screen without mouse reporting gets
/// arrow-key presses instead, matching the xterm/tmux alternate-scroll
/// convention: its transcript lives inside the program, so there is
/// nothing in this runtime's own scrollback to move. Anything else — and
/// `at` being `None`, a stale hit or a pane this caller never found a
/// runtime for — falls back to moving this runtime's own scroll offset,
/// exactly as it always has.
fn decide_scroll(
    wants_sgr_mouse_wheel: bool,
    alt_screen: bool,
    lines: i32,
    at: Option<crate::runtime::CellPos>,
) -> ScrollDecision {
    let Some(cell) = at else {
        return ScrollDecision::Local(lines);
    };
    if wants_sgr_mouse_wheel {
        let button = if lines < 0 { 64 } else { 65 };
        let col = u32::from(cell.col) + 1;
        let row = u32::from(cell.row) + 1;
        return ScrollDecision::ForwardSgr(format!("\x1b[<{button};{col};{row}M"));
    }
    if alt_screen {
        let key = if lines < 0 { "Up" } else { "Down" };
        return ScrollDecision::Arrows(key, lines.unsigned_abs() as usize);
    }
    ScrollDecision::Local(lines)
}

/// Execute [`Action::ScrollPane`]: one wheel notch over a pane. This is the
/// terminal-emulation fix for Claude Code panes not scrolling — such a pane
/// runs on the alternate screen with its own mouse reporting on, so the
/// notch has to reach tmux (as an SGR report, or as arrows) instead of
/// moving a local scrollback the pane never shows.
async fn scroll_pane(
    app: &mut App,
    client: &ControlClient,
    pane_id: String,
    lines: i32,
    at: Option<crate::runtime::CellPos>,
) -> Result<Outcome, TmuxError> {
    let Some(rt) = app.runtimes.get(&pane_id) else {
        return Ok(Outcome::default());
    };
    let decision = decide_scroll(rt.wants_sgr_mouse_wheel(), rt.alt_screen(), lines, at);
    // Mid-drag over this pane the wheel means "grow the selection past
    // one screen", which only local scrollback can do. Forwarded wheels
    // would make the pane's program repaint under a selection still being
    // built, so those stay swallowed mid-drag, exactly as every wheel was
    // before selections could scroll.
    let dragging_here = app.selection.dragging_pane() == Some(pane_id.as_str());
    match decision {
        ScrollDecision::ForwardSgr(report) => {
            if !dragging_here {
                client
                    .send_keys_unconfirmed(&pane_id, &[report.as_str()])
                    .await?;
            }
        }
        ScrollDecision::Arrows(key, count) => {
            if !dragging_here {
                let keys = vec![key; count];
                client.send_keys_unconfirmed(&pane_id, &keys).await?;
            }
        }
        ScrollDecision::Local(lines) => {
            if let Some(rt) = app.runtimes.get_mut(&pane_id) {
                rt.scroll(lines);
                // The viewport moved under the still-held pointer, so the
                // selection's live end moves to the text now under it.
                // This is what makes scroll-while-dragging extend the
                // selection instead of sliding the highlight.
                if dragging_here {
                    if let Some(at) = at {
                        rt.extend_selection(at);
                    }
                }
            }
        }
    }
    Ok(Outcome::default())
}

/// Focus a pane. Sidebar agent rows span every workspace, not just the
/// active one, so this tries the active session first (cheap: no session
/// switch) and only falls back to the cross-workspace path when the pane
/// genuinely isn't on screen.
async fn focus_pane(
    app: &mut App,
    client: &ControlClient,
    pane_id: &str,
) -> Result<Outcome, TmuxError> {
    let target = app
        .model
        .session
        .tabs
        .iter()
        .position(|tab| crate::layout::layout_contains_pane(&tab.layout, pane_id));
    match target {
        Some(index) => focus_pane_in_session(app, client, pane_id, index).await,
        None => focus_pane_in_background_workspace(app, client, pane_id).await,
    }
}

/// `pane_id` is on tab `index` of the active session. Select it, switching
/// tabs first if needed, and hydrate synchronously on the input path the
/// same way this has always worked. L1 made `hydrate_visible_tab` itself
/// faster (every stale pane hydrates concurrently instead of serially)
/// rather than moving it off this path: deferring it to the render deadline
/// would be the background-effect system the recommendation says to add
/// only after measurement shows this path is still slow, not speculatively.
async fn focus_pane_in_session(
    app: &mut App,
    client: &ControlClient,
    pane_id: &str,
    index: usize,
) -> Result<Outcome, TmuxError> {
    // A click resolves against the frame it was aimed at, and a window can
    // close between that frame and this handler. A stale index is a spent
    // click, not a request.
    if index >= app.model.session.tabs.len() {
        return Ok(Outcome::default());
    }
    let prior_tab = app.model.session.active_tab;
    let prior_pane = app.model.active_tab().active_pane.clone();
    if index == prior_tab && prior_pane == pane_id {
        // Already focused: nothing to tell tmux.
        return Ok(Outcome::default());
    }
    if index != prior_tab {
        client
            .select_window(&app.model.session.tabs[index].window_id)
            .await?;
    }
    client.select_pane(pane_id).await?;
    app.model.session.active_tab = index;
    let zoomed = app.model.session.tabs[index].zoomed;
    app.model.session.tabs[index].active_pane = pane_id.to_string();
    if index != prior_tab || (zoomed && prior_pane != pane_id) {
        super::resize_client(app, client).await;
        crate::sync::hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await;
        app.needs_hydrate = false;
        app.persist_active();
    }
    Ok(Outcome::default())
}

/// `pane_id` is not on the active session — it's a sidebar agent row in a
/// background workspace. Switch to that workspace and select the pane's
/// window; reconciliation replaces the local model with tmux's own answer.
async fn focus_pane_in_background_workspace(
    app: &mut App,
    client: &ControlClient,
    pane_id: &str,
) -> Result<Outcome, TmuxError> {
    let Some(window_id) = app.decoration.pane(pane_id).map(|d| d.window_id.clone()) else {
        // Decoration doesn't know this pane — a stale hit, or a reconcile
        // that hasn't caught up yet. Ask for a fresh model rather than guess.
        return Ok(Outcome::reconcile());
    };
    let Some(session) = app
        .model
        .workspaces
        .iter()
        .find(|workspace| workspace.window_ids.iter().any(|id| id == &window_id))
        .map(|workspace| workspace.name.clone())
    else {
        return Ok(Outcome::reconcile());
    };
    client.switch_to_session(&session).await?;
    client.select_window(&window_id).await?;
    client.select_pane(pane_id).await?;
    Ok(Outcome::reconcile())
}

/// Close one pane: straight away when it hosts no agent, else via a confirm
/// dialog. The dialog's own Enter resolves to this SAME action, and by then
/// `app.dialog` is still showing exactly this confirm — that is what lets
/// this run the has-agent check only once per gesture instead of looping.
async fn close_pane(
    app: &mut App,
    client: &ControlClient,
    pane_id: String,
) -> Result<Outcome, TmuxError> {
    let already_confirmed = matches!(&app.dialog, Some(Dialog::ConfirmClosePane { pane_id: shown }) if *shown == pane_id);
    if !already_confirmed {
        if daemon::pane_has_agent(&app.home, &pane_id) {
            app.open_dialog(Dialog::confirm_close(pane_id));
            return Ok(Outcome::default());
        }
    } else {
        app.dialog = None;
        app.hover = None;
    }
    client.kill_pane(&pane_id).await?;
    Ok(Outcome::reconcile())
}

/// Close one tab: straight away when none of its panes host an agent, else
/// via a confirm dialog. Routing already turned "this is the session's only
/// tab" into [`Action::RequestCloseWorkspace`] instead, so this never has to
/// re-check that.
async fn close_tab(
    app: &mut App,
    client: &ControlClient,
    window_id: String,
) -> Result<Outcome, TmuxError> {
    let already_confirmed = matches!(&app.dialog, Some(Dialog::ConfirmCloseTab { window_id: shown }) if *shown == window_id);
    if !already_confirmed {
        let has_agent = app
            .model
            .session
            .tabs
            .iter()
            .find(|tab| tab.window_id == window_id)
            .map(|tab| crate::layout::pane_ids_in_layout(&tab.layout))
            .into_iter()
            .flatten()
            .any(|pane| daemon::pane_has_agent(&app.home, &pane));
        if has_agent {
            app.open_dialog(Dialog::ConfirmCloseTab { window_id });
            return Ok(Outcome::default());
        }
    } else {
        app.dialog = None;
        app.hover = None;
    }
    client.kill_window(&window_id).await?;
    Ok(Outcome::reconcile())
}

/// Assign (or, with a blank label, clear) a pane's Cyclops identity. Unlike
/// [`close_pane`]/[`close_tab`], failure here is not "ask again" — it is
/// something to show, so the dialog stays open with the daemon's reason
/// instead of closing.
fn name_pane(app: &mut App, pane_id: String, label: String) -> Result<Outcome, TmuxError> {
    let previous_order_key = app
        .decoration
        .pane(&pane_id)
        .map(DecorationSnapshot::agent_order_key);
    if let Err(error) = daemon::label_pane(&app.home, &pane_id, &label) {
        if let Some(Dialog::NamePane {
            error: shown_error, ..
        }) = app.dialog.as_mut()
        {
            *shown_error = Some(error);
        }
        return Ok(Outcome::default());
    }
    let next_order_key = format!("name:{label}");
    let persist = previous_order_key.is_some_and(|previous| {
        crate::persist::migrate_order_entry(&mut app.prefs.agent_order, &previous, &next_order_key)
    });
    app.dialog = None;
    app.hover = None;
    match decoration::fetch_decoration(&app.home) {
        Ok(snapshot) => app.decoration = snapshot,
        Err(error) => super::log_err(
            &app.home,
            &format!("decoration refresh after label failed: {error}"),
        ),
    }
    Ok(Outcome {
        persist,
        ..Outcome::default()
    })
}

/// Create a tab in the current pane's directory. `name` is `None` for an
/// automatic numeric name — matches the device disagreement documented on
/// [`Action::NewTab`]: the keyboard binding never opened a dialog to reach
/// here, so this only clears one if one was actually open.
async fn new_tab(
    app: &mut App,
    client: &ControlClient,
    name: Option<String>,
) -> Result<Outcome, TmuxError> {
    let dialog_open = matches!(app.dialog, Some(Dialog::NewTab { .. }));
    let pane = app.model.active_tab().active_pane.clone();
    let cwd = client
        .display(&pane, "#{pane_current_path}")
        .await
        .map(|p| p.trim().to_string())
        .ok();
    let default_name = next_numeric_tab_name(&app.model.session.tabs);
    let name = name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&default_name);
    client
        .new_window(Some(name), cwd.as_deref().map(Path::new))
        .await?;
    if dialog_open {
        app.dialog = None;
        app.hover = None;
    }
    Ok(Outcome::reconcile())
}

/// The next automatic tab label. Explicit numeric labels advance the
/// sequence; a legacy/custom-only session starts from its visible tab count
/// so its next tab still reads naturally as 2, 3, and so on.
fn next_numeric_tab_name(tabs: &[crate::model::TabModel]) -> String {
    let largest = tabs
        .iter()
        .filter_map(|tab| tab.name.parse::<u64>().ok())
        .max()
        .unwrap_or(tabs.len() as u64);
    largest
        .checked_add(1)
        .map(|next| next.to_string())
        .unwrap_or_else(|| (tabs.len().saturating_add(1)).to_string())
}

/// Select a tab by tmux window id — robust to a tab that closed between
/// resolution and here, which just resolves to nothing.
async fn select_tab(
    app: &mut App,
    client: &ControlClient,
    window_id: String,
) -> Result<Outcome, TmuxError> {
    let Some(index) = app
        .model
        .session
        .tabs
        .iter()
        .position(|tab| tab.window_id == window_id)
    else {
        return Ok(Outcome::default());
    };
    client.select_window(&window_id).await?;
    app.model.session.active_tab = index;
    super::resize_client(app, client).await;
    crate::sync::hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await;
    app.needs_hydrate = false;
    app.persist_active();
    Ok(Outcome::default())
}

/// Create a workspace named after the focused pane's directory and switch
/// to it — no prompt, the folder is the name.
async fn new_workspace(app: &mut App, client: &ControlClient) -> Result<Outcome, TmuxError> {
    let pane = app.model.active_tab().active_pane.clone();
    let cwd = client
        .display(&pane, "#{pane_current_path}")
        .await
        .map(|p| p.trim().to_string())
        .unwrap_or_default();
    let folder = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    } else {
        PathBuf::from(cwd)
    };
    let taken: Vec<String> = app
        .model
        .workspaces
        .iter()
        .map(|w| w.name.clone())
        .collect();
    let name = naming::unique_session_name(&naming::session_name_from_folder(&folder), &taken);
    let session_id = client.new_session(&name, &folder).await?;
    client.switch_to_session(&name).await?;
    if !app.prefs.folder_tracked.contains(&session_id) {
        app.prefs.folder_tracked.push(session_id);
    }
    // Land the new row directly under whichever workspace was active when
    // the operator asked for it, instead of wherever tmux's own session
    // ordering happens to put it (often above). Rewritten wholesale from
    // the model's current names — the same move `reorder_workspace` makes
    // for a drag — because sidebar order lives only in preferences, and
    // the reconcile this Outcome triggers re-applies that order over
    // whatever tmux just handed back.
    let mut order: Vec<String> = app
        .model
        .workspaces
        .iter()
        .map(|w| w.name.clone())
        .collect();
    let insert_at = app
        .model
        .workspaces
        .get(app.model.active_workspace)
        .map(|_| app.model.active_workspace + 1)
        .unwrap_or(order.len());
    order.insert(insert_at, name);
    app.prefs.workspace_order = order;
    Ok(Outcome {
        reconcile: true,
        persist: true,
        ..Outcome::default()
    })
}

/// Rename one workspace. An explicit rename means the human owns the name
/// now, so a folder-following workspace stops following.
async fn rename_workspace(
    app: &mut App,
    client: &ControlClient,
    session: String,
    name: String,
) -> Result<Outcome, TmuxError> {
    client.rename_session(&session, &name).await?;
    let mut persist =
        crate::persist::migrate_order_entry(&mut app.prefs.workspace_order, &session, &name);
    if let Some(session_id) = app
        .model
        .workspaces
        .iter()
        .find(|workspace| workspace.name == session)
        .map(|workspace| workspace.session_id.clone())
    {
        let before = app.prefs.folder_tracked.len();
        app.prefs.folder_tracked.retain(|id| id != &session_id);
        persist |= app.prefs.folder_tracked.len() != before;
    }
    // The model addresses the active session BY NAME; skip this and the
    // reconcile that follows queries the old name.
    if app.model.session.session == session {
        app.model.session.session = name;
    }
    app.dialog = None;
    app.hover = None;
    Ok(Outcome {
        reconcile: true,
        persist,
        ..Outcome::default()
    })
}

/// Close one workspace. Always reached through [`Action::RequestCloseWorkspace`]
/// first, so the dialog is unconditionally the one being answered here.
async fn close_workspace(
    app: &mut App,
    client: &ControlClient,
    session: String,
) -> Result<Outcome, TmuxError> {
    app.dialog = None;
    app.hover = None;
    let fallback = if session == app.model.session.session {
        app.model
            .workspaces
            .iter()
            .find(|workspace| workspace.name != session)
            .map(|workspace| workspace.name.clone())
    } else {
        None
    };
    let closed_session_id = app
        .model
        .workspaces
        .iter()
        .find(|workspace| workspace.name == session)
        .map(|workspace| workspace.session_id.clone());
    if let Some(fallback) = &fallback {
        client.switch_to_session(fallback).await?;
    }
    client.kill_session(&session).await?;
    let before_order = app.prefs.workspace_order.len();
    app.prefs.workspace_order.retain(|name| name != &session);
    let mut persist = app.prefs.workspace_order.len() != before_order;
    if let Some(session_id) = closed_session_id {
        let before_tracked = app.prefs.folder_tracked.len();
        app.prefs.folder_tracked.retain(|id| id != &session_id);
        persist |= app.prefs.folder_tracked.len() != before_tracked;
    }
    Ok(Outcome {
        reconcile: true,
        persist,
        ..Outcome::default()
    })
}

/// Reorder one sidebar workspace row. Purely local presentation order — tmux
/// has no session ordering concept, so nothing here waits for reconciliation.
fn reorder_workspace(app: &mut App, session_id: String, insertion: Insertion) -> Outcome {
    let mut order: Vec<String> = app
        .model
        .workspaces
        .iter()
        .map(|w| w.session_id.clone())
        .collect();
    if !apply_insertion(&mut order, &session_id, &insertion) {
        return Outcome::default();
    }
    let active_id = app
        .model
        .workspaces
        .get(app.model.active_workspace)
        .map(|w| w.session_id.clone());
    let mut remaining = std::mem::take(&mut app.model.workspaces);
    let mut ordered = Vec::with_capacity(remaining.len());
    for id in &order {
        if let Some(pos) = remaining.iter().position(|w| &w.session_id == id) {
            ordered.push(remaining.remove(pos));
        }
    }
    ordered.extend(remaining);
    app.model.workspaces = ordered;
    app.model.active_workspace = active_id
        .and_then(|id| app.model.workspaces.iter().position(|w| w.session_id == id))
        .unwrap_or(0);
    app.prefs.workspace_order = app
        .model
        .workspaces
        .iter()
        .map(|w| w.name.clone())
        .collect();
    Outcome {
        persist: true,
        ..Outcome::default()
    }
}

/// Reorder one sidebar agent row within a workspace. Same reasoning as
/// [`reorder_workspace`]: this order lives only in preferences.
fn reorder_agent(
    app: &mut App,
    workspace_id: String,
    order_key: String,
    insertion: Insertion,
) -> Outcome {
    let Some(window_ids) = app
        .model
        .workspaces
        .iter()
        .find(|w| w.session_id == workspace_id)
        .map(|w| w.window_ids.clone())
    else {
        return Outcome::default();
    };
    let mut local: Vec<String> = app
        .decoration
        .agent_rows_for_window_ids(&window_ids, &app.prefs.agent_order)
        .into_iter()
        .map(DecorationSnapshot::agent_order_key)
        .collect();
    if !apply_insertion(&mut local, &order_key, &insertion) {
        return Outcome::default();
    }
    app.prefs
        .agent_order
        .retain(|key| !local.iter().any(|local_key| local_key == key));
    app.prefs.agent_order.extend(local);
    Outcome {
        persist: true,
        ..Outcome::default()
    }
}

/// The rows the theme picker offers, sorted by name, and which one is
/// active. The CLI's listing rule is the contract (`entries` in
/// src/cyclops/src/theme.rs): a row is offered only for a file that loads
/// clean and paints anything, because a row that would repaint nothing is
/// a lie the reader only finds out later. The active row is the file
/// selection resolves (`cyclops_theme::active`), matched by path; a theme
/// chosen by path or by CYCLOPS_THEME marks no row, same as the CLI.
fn theme_rows(home: &Path) -> (Vec<String>, Option<usize>) {
    let active = cyclops_theme::active(home).path;
    let Some(dir) = cyclops_theme::themes_dir(home) else {
        return (Vec::new(), None);
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return (Vec::new(), None);
    };
    let mut rows: Vec<(String, PathBuf)> = read
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let (theme, _) = cyclops_theme::Theme::load(&path).ok()?;
            theme.paints_anything().then_some((name, path))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let active = rows
        .iter()
        .position(|(_, path)| active.as_deref() == Some(path.as_path()));
    (rows.into_iter().map(|(name, _)| name).collect(), active)
}

/// Mirror the surfaces' visibility into the settings card's view section
/// while it is open. Its rows flip the prefs at once and read them back
/// from here, so a chord that hides the tab bar while the card is up
/// moves the check too, and the card never says a surface is showing
/// that is not. A no-op with no card open.
pub(super) fn sync_view_switches(app: &mut App) {
    let (tab_bar, files) = (app.prefs.tab_bar_visible, app.prefs.files_rows > 0);
    if let Some(Dialog::Settings { view, .. }) = app.dialog.as_mut() {
        view.tab_bar = tab_bar;
        view.files = files;
    }
}

/// The cursor landed on a settings row, by arrows, wheel, or a click
/// (`clicked`). Landing is a preview in both sections and saves nothing:
/// Enter does that, Esc forgets it. A theme row goes live over the
/// workspace ([`preview_selected_theme`]) and its check follows the
/// cursor (the painter's rule). A sound row takes its group's check
/// ([`SoundPicker::check_selected`]) and plays once for the arrival
/// ([`SoundPicker::arrived_on`]) and on every click, so hearing it
/// again is one click. A no-op for every other dialog.
pub(super) fn settings_cursor_moved(app: &mut App, clicked: bool) {
    preview_selected_theme(app);
    let Some(Dialog::Settings {
        section: SettingsSection::Sound,
        sound,
        ..
    }) = app.dialog.as_mut()
    else {
        return;
    };
    sound.check_selected();
    let arrived = sound.arrived_on().is_some();
    let play = match sound.selected_row() {
        Some(SoundRow::Sound(name)) if arrived || clicked => Some(name.to_string()),
        _ => None,
    };
    if let Some(name) = play {
        crate::sound::play(&app.home, &name);
    }
}

/// Preview the theme under the picker's cursor: load its file and swap it
/// into the live paint for the next render. Nothing is written and no
/// daemon is nudged; this is transient UI state (module rule 2), and a
/// deliberate exception to [`apply_theme`]'s "the repaint is the
/// ThemeWatch's job": a preview is not an applied theme, so the watch
/// must not adopt it. `App::theme_restore` keeps what to put back, and
/// `refresh_theme_watch` (app.rs) holds the watch off while it is set.
///
/// Tolerant on purpose: a file that stopped loading or paints nothing
/// previews as nothing (the prior paint stays, the picker keeps working),
/// and load warnings are dropped for the reason [`save_theme_choice`]
/// gives: the ThemeWatch logs them once if this file is ever applied.
pub(super) fn preview_selected_theme(app: &mut App) {
    let Some(Dialog::Settings { themes, .. }) = app.dialog.as_ref() else {
        return;
    };
    let Some(name) = themes.names.get(themes.selected) else {
        return;
    };
    let Some(path) = cyclops_theme::path_for(name, &app.home) else {
        return;
    };
    let Ok((theme, _)) = cyclops_theme::Theme::load(&path) else {
        return;
    };
    if !theme.paints_anything() {
        return;
    }
    app.paint.theme = theme;
}

/// Switch to a theme by name: what `cyclops theme <name>` does, told the
/// way the picker tells it. The config write and daemon nudge are
/// [`save_theme_choice`] and [`daemon::theme_reload`]; nothing here
/// touches `app.paint`, because the repaint is the ThemeWatch's job on
/// the render deadline (the daemon's theme event, or failing that the
/// redraw this action already arms, wakes it). The preview has usually
/// painted the picked theme already; the confirming refresh makes it the
/// watch's own again.
fn apply_theme(app: &mut App, name: &str) -> Outcome {
    let (saved, notice) = match save_theme_choice(&app.home, name) {
        Err(refusal) => (false, Some(refusal)),
        Ok(want) => match daemon::theme_reload(&app.home) {
            // The daemon compares by the theme's own `name` key, which
            // the file stem the user picked can differ from.
            daemon::ThemeReload::Painting(Some(now)) if now == want => (true, None),
            daemon::ThemeReload::Painting(painting) => {
                (true, Some(copy::theme_not_live(painting.as_deref())))
            }
            daemon::ThemeReload::NoDaemon => (true, Some(copy::THEME_SAVED_NO_DAEMON.to_string())),
        },
    };
    let Some(text) = notice else {
        // Live everywhere the daemon paints; the preview already painted
        // it here. Dropping the kept paint hands ownership back to the
        // watch, whose next refresh confirms this same file.
        app.dialog = None;
        app.hover = None;
        app.theme_restore = None;
        return Outcome::default();
    };
    // The story stays in the open picker (the NamePane error shape);
    // Escape closes it. A written config moves the active marker even
    // when the daemon did not confirm, because the selection did switch.
    if let Some(Dialog::Settings { themes, .. }) = app.dialog.as_mut() {
        themes.notice = Some(text);
        if saved {
            themes.active = themes.names.iter().position(|n| n == name);
        }
    }
    Outcome::default()
}

/// Validate a picked theme and write the config key, mirroring steps 1-3
/// of the CLI's `set` (src/cyclops/src/theme.rs) in the order that keeps
/// a bad name from costing anything: resolve the name the way selection
/// will, refuse a file that will not load or sets nothing, then write.
/// Ok carries the theme's own `name` key, the name a nudged daemon
/// answers with. Load warnings are not surfaced here: the ThemeWatch
/// reloading this same file logs them once on the render deadline.
fn save_theme_choice(home: &Path, name: &str) -> Result<String, String> {
    let Some(path) = cyclops_theme::path_for(name, home) else {
        return Err(copy::THEMES_EMPTY.to_string());
    };
    let theme = match cyclops_theme::Theme::load(&path) {
        Ok((theme, _)) => theme,
        Err(e) => return Err(copy::theme_unusable(name, &e.to_string())),
    };
    if !theme.paints_anything() {
        return Err(copy::theme_sets_no_colors(name));
    }
    cyclops_theme::set_config_theme(home, name).map_err(|e| copy::theme_not_saved(&e))?;
    Ok(theme.name().to_string())
}

/// Move `source` to sit immediately before/after `insertion`'s target.
/// `false` (leaving `order` untouched) when `source` and the target are the
/// same row or either has vanished from `order` since the drop was resolved
/// — a stale drop, not a move.
fn apply_insertion(order: &mut Vec<String>, source: &str, insertion: &Insertion) -> bool {
    let (target, after) = match insertion {
        Insertion::Before(target) => (target.as_str(), false),
        Insertion::After(target) => (target.as_str(), true),
    };
    if source == target {
        return false;
    }
    if !order.iter().any(|id| id == source) || !order.iter().any(|id| id == target) {
        return false;
    }
    let source_index = order
        .iter()
        .position(|id| id == source)
        .expect("checked above");
    let item = order.remove(source_index);
    let mut target_index = order
        .iter()
        .position(|id| id == target)
        .expect("checked above");
    if after {
        target_index += 1;
    }
    order.insert(target_index.min(order.len()), item);
    true
}

/// Start a send and return immediately.
///
/// The daemon holds a send's answer for the acknowledgement window, which
/// is seconds, so doing this inline would freeze every pane in the
/// workspace while it waited. It runs on a thread of its own and posts the
/// receipt back as an [`AppMsg`]; the composer stays open showing that it
/// is in flight, and its send state keeps a second Enter from sending twice.
///
/// With no channel (a test App built without a loop) the send is simply not
/// started. Spawning a thread whose answer nothing can receive would be a
/// message on the record that the operator is never told about.
/// Type `@<reference> ` into the focused pane.
///
/// Typed, not submitted. The path lands beside whatever is already on that
/// line and the operator decides what to do with it, which is the whole
/// point: clicking a file in the tree is how you say "this one" while you
/// are still composing a sentence about it.
///
/// The trailing space is not decoration. Without it a second click
/// concatenates two paths into one nonexistent one, and that is the
/// failure the agent on the other end reports rather than the two files
/// that were meant.
async fn insert_file_ref(
    app: &mut App,
    client: &ControlClient,
    reference: String,
) -> Result<Outcome, cyclops_tmux::TmuxError> {
    // Control mode is line based: a literal carrying a line break ends the
    // command, and tmux reads the rest as another one. A path may legally
    // contain a newline on unix, so this is a real input rather than a
    // theoretical one, and the safe answer is to type nothing.
    if reference.contains('\n') || reference.contains('\r') {
        return Ok(Outcome::default());
    }
    let pane = app.model.active_tab().active_pane.clone();
    client
        .send_keys(&pane, &[&format!("@{reference} ")])
        .await?;
    // Named by label where the pane has one. The click happened in the
    // sidebar and the text landed somewhere else, so the notice is the
    // only thing that says where it went.
    let where_to = app
        .decoration
        .pane(&pane)
        .and_then(|d| d.label.clone())
        .unwrap_or_else(|| pane.clone());
    app.notice.show(
        crate::copy::file_sent(&reference, &where_to),
        tokio::time::Instant::now(),
    );
    Ok(Outcome::default())
}

fn send_message(app: &mut App, to: String, subject: String, body: String) {
    let Some(requests) = app.send_requests.clone() else {
        return;
    };
    let message = Composed { to, subject, body };
    let Some(attempt) = crate::dialog::begin_compose_send(app.dialog.as_mut(), message, || {
        format!("workspace-{}", Uuid::new_v4())
    }) else {
        return;
    };
    if let Some(Dialog::Compose { status, .. }) = app.dialog.as_mut() {
        let to = &attempt.message.to;
        *status = Some(crate::copy::compose_sending(to));
    }
    match requests.try_send(attempt) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(attempt)) => {
            super::finish_compose_send(
                app.dialog.as_mut(),
                attempt,
                daemon::SendOutcome::NotSent("another send is still in progress".into()),
            );
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(attempt)) => {
            super::finish_compose_send(
                app.dialog.as_mut(),
                attempt,
                daemon::SendOutcome::NotSent("the send worker stopped".into()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cyclops_testrig::{tmux_available, TmuxServer};
    use cyclops_tmux::{ControlClient, ControlConfig, PaneDirection, SplitDirection};

    use super::*;
    use crate::bindings::default_bindings;
    use crate::input::mouse::{HitMap, MenuState};
    use crate::input::router::Router;
    use crate::layout::ResolvedLayout;
    use crate::model::{RuntimeRegistry, SessionModel, TabModel, WorkspaceModel, WorkspaceRow};
    use crate::persist::WorkspacePrefs;
    use crate::resilience::LinkState;
    use crate::selection::SelectionState;
    use crate::theme::Paint;

    fn one_tab_model(
        session: &str,
        window_id: &str,
        pane_id: &str,
        session_id: &str,
    ) -> WorkspaceModel {
        let layout = ResolvedLayout::Leaf {
            pane_id: pane_id.to_string(),
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let tab = TabModel {
            window_id: window_id.to_string(),
            name: "1".to_string(),
            layout,
            active_pane: pane_id.to_string(),
            zoomed: false,
        };
        WorkspaceModel {
            workspaces: vec![WorkspaceRow {
                session_id: session_id.to_string(),
                name: session.to_string(),
                tab_count: 1,
                window_ids: vec![window_id.to_string()],
            }],
            active_workspace: 0,
            session: SessionModel {
                session: session.to_string(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
            messages_visible: false,
        }
    }

    fn test_app(model: WorkspaceModel, home: std::path::PathBuf) -> App {
        App {
            // No loop around this App, so nothing could receive a send's
            // answer. `send_message` declines to start one rather than put
            // a message on the record nobody will be told about.
            model,
            runtimes: RuntimeRegistry::default(),
            router: Router::new(default_bindings()),
            paint: Paint::for_test(),
            dialog_offset: (0, 0),
            files: crate::files::FileTree::new(),
            files_pinned: crate::files::FileTree::new(),
            files_view: crate::files::FilesView::default(),
            files_probe_at: None,
            files_root_pending: false,
            dialog: None,
            theme_restore: None,
            link_state: LinkState::Live,
            paused_panes: HashSet::new(),
            minimized: std::collections::HashMap::new(),
            window_palette: crate::app::HostPaletteState::Unknown,
            window_focused: true,
            select_all: crate::input::SelectAll::default(),
            reconnect_attempt: 0,
            needs_forced_hydrate: false,
            hit_map: HitMap::default(),
            menu: MenuState::None,
            hover: None,
            selection: SelectionState::default(),
            drag: None,
            notice: crate::notice::NoticeState::default(),
            decoration: DecorationSnapshot::default(),
            prefs: WorkspacePrefs::default(),
            expanded_workspaces: HashSet::new(),
            expanded_for: None,
            watched_sessions: HashSet::new(),
            sidebar_tab: SidebarTab::default(),
            record: cyclops_ui::Record::new(),
            messages_queue: cyclops_ui::HumanQueue::default(),
            messages_caller: None,
            messages_detail: None,
            messages_composer: cyclops_ui::ComposerState::default(),
            avatar_registry: cyclops_ui::AvatarRegistry::default(),
            intake: cyclops_ui::Intake::new(),
            stream_reconciling: false,
            cursor_style: None,
            term_size: (80, 24),
            declared_client_size: None,
            sizing: crate::app::WindowSizing::default(),
            needs_reconcile: false,
            needs_hydrate: false,
            paste_seq: 0,
            home,
            folder_probe_at: None,
            send_requests: None,
            stream_reconcile_requests: None,
            repaint_requested: false,
            repaint_resize_pending: false,
            repaint_resize_settle_at: None,
            messages_focused: false,
            messages_session_scoped: true,
            messages_gate: cyclops_ui::RefreshGate::new(),
            messages_refresh_error: None,
            messages_send_tx: None,
            messages_composer_revision: 0,
            messages_send_in_flight: None,
            messages_snapshot_tx: None,
            message_detail_tx: None,
            message_detail_in_flight: None,
            messages_reconcile_owed: None,
        }
    }

    async fn rig_client(server: &TmuxServer, session: &str) -> ControlClient {
        let cfg = ControlConfig::attach(session)
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        ControlClient::spawn(cfg).await.expect("attach").0
    }

    fn pane_ids(server: &TmuxServer, target: &str) -> Vec<String> {
        let out = server.run(&["list-panes", "-t", target, "-F", "#{pane_id}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn window_ids(server: &TmuxServer, target: &str) -> Vec<String> {
        let out = server.run(&["list-windows", "-t", target, "-F", "#{window_id}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn window_names(server: &TmuxServer, target: &str) -> Vec<String> {
        let out = server.run(&["list-windows", "-t", target, "-F", "#{window_name}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// `(pane_id, pane_left)` per pane, enough to prove a swap exchanged
    /// two side-by-side panes' slots.
    fn pane_positions(server: &TmuxServer, target: &str) -> Vec<(String, String)> {
        let out = server.run(&["list-panes", "-t", target, "-F", "#{pane_id} #{pane_left}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let mut fields = line.split_whitespace();
                (
                    fields.next().unwrap_or_default().to_string(),
                    fields.next().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    fn left_of(positions: &[(String, String)], pane: &str) -> String {
        positions
            .iter()
            .find(|(id, _)| id == pane)
            .map(|(_, left)| left.clone())
            .unwrap_or_else(|| panic!("pane {pane} missing from {positions:?}"))
    }

    fn scratch_home(tag: &str) -> std::path::PathBuf {
        let home = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        home
    }

    fn tab(window_id: &str, name: &str, pane_id: &str) -> TabModel {
        TabModel {
            window_id: window_id.to_string(),
            name: name.to_string(),
            layout: ResolvedLayout::Leaf {
                pane_id: pane_id.to_string(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: pane_id.to_string(),
            zoomed: false,
        }
    }

    /// A fake cyclopsd that answers exactly one `status` request with one
    /// session holding one adopted-agent pane. Mirrors the Hello-then-
    /// response pattern `crate::daemon`'s own tests use — `pane_has_agent`
    /// speaks that same protocol and has no other way to be made to answer
    /// "yes" without a real daemon.
    fn spawn_status_daemon_with_agent(home: &std::path::Path, pane_id: &str) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let socket = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
        let pane_id = pane_id.to_string();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream);
            let _ = reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n");
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let body = serde_json::json!({
                "id": 1,
                "result": {
                    "daemon_version": "0.1.0",
                    "proto": 1,
                    "boot_id": "b",
                    "uptime_ms": 0,
                    "tmux_version": "3.4",
                    "sessions": [{
                        "name": "s",
                        "attached": true,
                        "panes": [{
                            "pane_id": pane_id,
                            "window_id": "@0",
                            "window_name": "one",
                            "agent": "reviewer",
                            "title": "",
                            "current_command": "sh",
                            "dead": false,
                            "in_mode": false,
                            "width": 80,
                            "height": 24,
                            "state": "idle",
                        }],
                    }],
                },
            });
            let mut out = serde_json::to_vec(&body).expect("encode fake status");
            out.push(b'\n');
            let _ = reader.get_mut().write_all(&out);
        });
    }

    /// A fake cyclopsd that answers exactly one `theme.reload` request,
    /// naming `painting` as the theme it is now on. Same Hello-then-
    /// response shape as [`spawn_status_daemon_with_agent`].
    fn spawn_theme_reload_daemon(home: &std::path::Path, painting: &str) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let socket = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
        let painting = painting.to_string();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream);
            let _ = reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n");
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let request: serde_json::Value = serde_json::from_str(&line).expect("JSON request");
            // A wrong method gets no response at all, which the test sees
            // as a daemon that did not confirm.
            if request["method"] != "theme.reload" {
                return;
            }
            let body = serde_json::json!({"id": 1, "result": {"theme": painting}});
            let mut out = serde_json::to_vec(&body).expect("encode fake reload");
            out.push(b'\n');
            let _ = reader.get_mut().write_all(&out);
        });
    }

    fn write_theme(home: &std::path::Path, name: &str, body: &str) {
        let dir = home.join("themes");
        std::fs::create_dir_all(&dir).expect("mkdir themes");
        std::fs::write(dir.join(format!("{name}.toml")), body).expect("write theme");
    }

    // -- Pure: moved from app.rs's own test module with `next_numeric_tab_name`. --

    #[test]
    fn automatic_tab_names_advance_numerically() {
        assert_eq!(next_numeric_tab_name(&[tab("@1", "1", "%0")]), "2");
        assert_eq!(
            next_numeric_tab_name(&[tab("@1", "1", "%0"), tab("@2", "notes", "%1")]),
            "2"
        );
        assert_eq!(
            next_numeric_tab_name(&[tab("@1", "1", "%0"), tab("@2", "4", "%1")]),
            "5"
        );
        assert_eq!(next_numeric_tab_name(&[tab("@1", "zsh", "%0")]), "2");
    }

    // -- Scroll decision: SGR forward, arrow-key forward, local fallback. --

    #[test]
    fn sgr_mouse_wheel_forwards_one_event_with_one_based_coords() {
        let at = Some(crate::runtime::CellPos { col: 0, row: 0 });
        assert_eq!(
            decide_scroll(true, true, -3, at),
            ScrollDecision::ForwardSgr("\x1b[<64;1;1M".to_string()),
            "up forwards button 64"
        );
        assert_eq!(
            decide_scroll(true, true, 3, at),
            ScrollDecision::ForwardSgr("\x1b[<65;1;1M".to_string()),
            "down forwards button 65"
        );
    }

    #[test]
    fn alt_screen_without_sgr_mouse_forwards_arrow_keys() {
        let at = Some(crate::runtime::CellPos { col: 4, row: 2 });
        assert_eq!(
            decide_scroll(false, true, -3, at),
            ScrollDecision::Arrows("Up", 3)
        );
        assert_eq!(
            decide_scroll(false, true, 3, at),
            ScrollDecision::Arrows("Down", 3)
        );
    }

    #[test]
    fn plain_pane_scrolls_locally() {
        let at = Some(crate::runtime::CellPos { col: 4, row: 2 });
        assert_eq!(
            decide_scroll(false, false, -3, at),
            ScrollDecision::Local(-3)
        );
        assert_eq!(decide_scroll(false, false, 3, at), ScrollDecision::Local(3));
    }

    #[test]
    fn no_resolved_cell_always_falls_back_to_local_even_with_sgr_mouse_on() {
        assert_eq!(
            decide_scroll(true, true, -3, None),
            ScrollDecision::Local(-3)
        );
    }

    // -- An action that mutates structure reconciles. --

    #[tokio::test]
    async fn split_calls_tmux_and_reconciles() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-split");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let mut app = test_app(
            one_tab_model("s", "@0", &pane, "$0"),
            cyclops_proto::scratch::scratch_dir("exec-split-home"),
        );

        let outcome = execute(
            &mut app,
            &client,
            Action::Split {
                pane_id: pane.clone(),
                direction: SplitDirection::Horizontal,
            },
        )
        .await
        .expect("split executes");

        assert!(
            outcome.reconcile,
            "a structural change must ask to reconcile"
        );
        assert_eq!(pane_ids(&server, "s").len(), 2, "tmux actually split");
        client.shutdown().await;
    }

    // -- Pane swap: ids exchange slots while the layout shape stays put. --

    #[tokio::test]
    async fn swap_panes_exchanges_the_two_panes_positions() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-swap-panes");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "s"]);
        let before = pane_positions(&server, "s");
        assert_eq!(before.len(), 2);
        let (a, b) = (before[0].0.clone(), before[1].0.clone());
        let client = rig_client(&server, "s").await;
        let mut app = test_app(
            one_tab_model("s", "@0", &a, "$0"),
            cyclops_proto::scratch::scratch_dir("exec-swap-panes-home"),
        );

        let outcome = execute(
            &mut app,
            &client,
            Action::SwapPanes {
                pane_id: a.clone(),
                other_pane_id: b.clone(),
            },
        )
        .await
        .expect("swap executes");

        assert!(outcome.reconcile, "a swap is structural and must reconcile");
        let after = pane_positions(&server, "s");
        assert_eq!(left_of(&after, &a), left_of(&before, &b));
        assert_eq!(left_of(&after, &b), left_of(&before, &a));
        // The dragged pane (`pane_id`) ends up focused in its new slot.
        let active = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_active}"]);
        let active = String::from_utf8_lossy(&active.stdout);
        assert!(
            active.lines().any(|line| line == format!("{a} 1")),
            "the dragged pane must end focused, got {active}"
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn swap_pane_direction_swaps_with_the_tmux_resolved_neighbour() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-swap-direction");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "s"]);
        let before = pane_positions(&server, "s");
        let (left, right) = (before[0].0.clone(), before[1].0.clone());
        server.run_ok(&["select-pane", "-t", &left]);
        let client = rig_client(&server, "s").await;
        let mut app = test_app(
            one_tab_model("s", "@0", &left, "$0"),
            cyclops_proto::scratch::scratch_dir("exec-swap-direction-home"),
        );

        let outcome = execute(
            &mut app,
            &client,
            Action::SwapPaneDirection(PaneDirection::Right),
        )
        .await
        .expect("directional swap executes");

        assert!(outcome.reconcile);
        let after = pane_positions(&server, "s");
        assert_eq!(left_of(&after, &left), left_of(&before, &right));
        assert_eq!(left_of(&after, &right), left_of(&before, &left));
        // Focus follows the focused pane to its new slot.
        let active = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_active}"]);
        let active = String::from_utf8_lossy(&active.stdout);
        assert!(
            active.lines().any(|line| line == format!("{left} 1")),
            "the swapped pane must keep focus, got {active}"
        );
        client.shutdown().await;
    }

    // -- A dialog-opening action does not touch tmux. --

    #[tokio::test]
    async fn request_new_tab_opens_a_dialog_and_never_reaches_tmux() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-request-new-tab");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let mut app = test_app(
            one_tab_model("s", "@0", &pane, "$0"),
            cyclops_proto::scratch::scratch_dir("exec-request-new-tab-home"),
        );
        let windows_before = server.run(&["list-windows", "-t", "s", "-F", "#{window_id}"]);

        let outcome = execute(&mut app, &client, Action::RequestNewTab)
            .await
            .expect("request opens a dialog");

        assert!(
            !outcome.reconcile,
            "opening a dialog must not ask to reconcile"
        );
        assert!(!outcome.persist);
        assert!(
            matches!(app.dialog, Some(Dialog::NewTab { .. })),
            "the naming dialog should be open, got {:?}",
            app.dialog
        );
        let windows_after = server.run(&["list-windows", "-t", "s", "-F", "#{window_id}"]);
        assert_eq!(
            windows_before.stdout, windows_after.stdout,
            "no window may exist until the dialog is confirmed"
        );
        client.shutdown().await;
    }

    // -- The tab bar is the operator's own choice, and its row must track
    // the tmux-declared size. --

    /// Hide and show, through the executor: the preference flips, the flip
    /// survives a round trip through config.toml, and each flip re-declares
    /// the tmux client size by exactly the strip's row. Both the chrome
    /// split and the declaration go through `App::chrome`, so a drift
    /// between them would land here as a declared size that does not move
    /// with the bar. Tab count never enters into it: this workspace has one
    /// tab throughout and keeps its strip.
    /// The settings card's view rows flip their surface at once and
    /// stay open: the check follows the pref, whichever way the pref
    /// was flipped, so the card never says a surface is showing that
    /// is not.
    #[tokio::test]
    async fn a_view_row_flips_its_surface_and_the_card_reads_it_back() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-view-row");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-view-row-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        assert!(app.prefs.files_rows > 0, "the panel shows by default");

        execute(
            &mut app,
            &client,
            Action::ShowSettings {
                section: SettingsSection::View,
            },
        )
        .await
        .expect("open the card");
        let view = |app: &App| match app.dialog.as_ref() {
            Some(Dialog::Settings { view, .. }) => *view,
            other => panic!("the card is open: {other:?}"),
        };
        assert!(view(&app).files, "opens reading the panel as shown");

        if let Some(open) = app.dialog.as_mut() {
            crate::dialog::select_settings_row(open, 1);
        }
        let action = crate::action::route_dialog_confirm(app.dialog.as_ref().unwrap())
            .expect("Enter on the Files row names its switch");
        let outcome = execute(&mut app, &client, action)
            .await
            .expect("flip the panel");
        assert!(outcome.persist, "the new visibility belongs on disk");
        assert_eq!(app.prefs.files_rows, 0, "the panel is put away");
        assert!(!view(&app).files, "and the row's check went with it");
        assert_eq!(view(&app).selected, 1, "the cursor stayed on its row");

        // Flipped from anywhere else while the card is up, the check
        // still follows.
        execute(&mut app, &client, Action::ToggleFiles)
            .await
            .expect("flip it back by chord");
        assert!(app.prefs.files_rows > 0);
        assert!(view(&app).files, "the card reads the pref, not its memory");

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    /// The keybinds card opens on the router's live map, scrolled to the
    /// top: what the card teaches is what is bound, not what was written
    /// down.
    #[tokio::test]
    async fn show_keybinds_opens_its_own_card_on_the_active_map() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-show-keybinds");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-show-keybinds-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());

        let outcome = execute(&mut app, &client, Action::ShowKeybinds)
            .await
            .expect("open the card");
        assert!(!outcome.persist && !outcome.reconcile);
        match &app.dialog {
            Some(Dialog::Keybinds { scroll, rows }) => {
                assert_eq!(*scroll, 0, "opens at the top");
                assert_eq!(*rows, app.router.help(), "the active map, as rows");
                assert!(!rows.is_empty());
            }
            other => panic!("the keybinds card is open: {other:?}"),
        }
        assert!(
            app.theme_restore.is_none(),
            "no theme is previewed, so there is nothing to put back"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn toggling_the_tab_bar_persists_and_redeclares_the_client_size() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-toggle-tab-bar");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-toggle-tab-bar-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        let term = ratatui::layout::Rect::new(0, 0, app.term_size.0, app.term_size.1);

        // A fresh install shows the strip, one tab or ten: the `+` that
        // makes the second tab lives there.
        assert!(app.prefs.tab_bar_visible, "the default is shown");
        assert_eq!(app.model.session.tabs.len(), 1);
        crate::app::reconcile(&mut app, &client)
            .await
            .expect("initial reconcile");
        assert_eq!(app.chrome(term).tab_bar.height, 1, "a lone tab keeps it");
        let shown = app.declared_client_size.expect("declared with the bar");

        let outcome = execute(&mut app, &client, Action::ToggleTabBar)
            .await
            .expect("hide the strip");
        assert!(outcome.persist, "the new visibility belongs on disk");
        assert!(!app.prefs.tab_bar_visible);
        assert_eq!(app.chrome(term).tab_bar.height, 0);
        let hidden = app.declared_client_size.expect("declared without the bar");
        assert_eq!(hidden.0, shown.0);
        assert_eq!(
            hidden.1,
            shown.1 + 1,
            "the bar row moves between chrome and the declared grid, whole"
        );

        // What `apply_outcome` does for the real caller; the reload below
        // has to read a file that actually exists.
        crate::persist::save_prefs(&home, &app.prefs).expect("save prefs");
        assert!(
            !crate::persist::load_prefs(&home).tab_bar_visible,
            "a workspace quit with the strip hidden must reopen hidden"
        );

        // And back: the settings card's View row is the visible way here,
        // and it puts the row and the `+` back exactly where they were.
        let outcome = execute(&mut app, &client, Action::ToggleTabBar)
            .await
            .expect("show the strip");
        assert!(outcome.persist);
        assert!(app.prefs.tab_bar_visible);
        assert_eq!(app.chrome(term).tab_bar.height, 1);
        assert_eq!(app.declared_client_size, Some(shown));
        crate::persist::save_prefs(&home, &app.prefs).expect("save prefs");
        assert!(crate::persist::load_prefs(&home).tab_bar_visible);

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    // -- A reorder persists once on drop. --

    #[tokio::test]
    async fn workspace_reorder_moves_the_row_and_flags_a_persist() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-reorder");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = cyclops_proto::scratch::scratch_dir("exec-reorder-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let mut model = one_tab_model("s", "@0", &pane, "$0");
        model.workspaces.push(WorkspaceRow {
            session_id: "$1".into(),
            name: "other".into(),
            tab_count: 1,
            window_ids: Vec::new(),
        });
        let mut app = test_app(model, home.clone());

        let outcome = execute(
            &mut app,
            &client,
            Action::ReorderWorkspace {
                session_id: "$1".into(),
                insertion: Insertion::Before("$0".into()),
            },
        )
        .await
        .expect("reorder executes");

        assert!(outcome.persist, "moving a row must ask to persist");
        assert!(!outcome.reconcile, "reordering never touches tmux");
        assert_eq!(
            app.model
                .workspaces
                .iter()
                .map(|w| w.session_id.clone())
                .collect::<Vec<_>>(),
            vec!["$1".to_string(), "$0".to_string()],
        );
        assert_eq!(
            app.prefs.workspace_order,
            vec!["other".to_string(), "s".to_string()]
        );

        // The flag is a promise the caller keeps exactly once per drop; prove
        // the promised write actually round-trips.
        crate::persist::save_prefs(&home, &app.prefs).expect("persist once");
        let reloaded = crate::persist::load_prefs(&home);
        assert_eq!(reloaded.workspace_order, app.prefs.workspace_order);

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn workspace_reorder_onto_itself_is_a_no_op() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-reorder-noop");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let mut app = test_app(
            one_tab_model("s", "@0", &pane, "$0"),
            cyclops_proto::scratch::scratch_dir("exec-reorder-noop-home"),
        );

        let outcome = execute(
            &mut app,
            &client,
            Action::ReorderWorkspace {
                session_id: "$0".into(),
                insertion: Insertion::After("$0".into()),
            },
        )
        .await
        .expect("no-op reorder still executes cleanly");

        assert!(
            !outcome.persist,
            "a drop that changes nothing must not persist"
        );
        client.shutdown().await;
    }

    // -- Rename targeting a background tab: the dialog must show the
    // clicked tab's own name, never whichever tab happens to be active. --

    #[tokio::test]
    async fn request_rename_tab_targets_a_background_tab_by_id() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-rename-background");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let client = rig_client(&server, "s").await;
        let mut model = one_tab_model("s", "@1", "%1", "$0");
        // "@0" (name "one") is a background tab; "@1" (name "two") is active.
        model.session.tabs = vec![tab("@0", "one", "%0"), tab("@1", "two", "%1")];
        model.session.active_tab = 1;
        let mut app = test_app(model, scratch_home("exec-rename-background-home"));

        let outcome = execute(
            &mut app,
            &client,
            Action::RequestRenameTab {
                window_id: "@0".into(),
            },
        )
        .await
        .expect("request opens a dialog");

        assert!(!outcome.reconcile);
        match &app.dialog {
            Some(Dialog::RenameTab { window_id, buffer }) => {
                assert_eq!(window_id, "@0");
                assert_eq!(buffer, "one", "must show the clicked tab's own name");
            }
            other => panic!("expected a rename dialog, got {other:?}"),
        }
        client.shutdown().await;
    }

    // -- Close-tab-with-agent confirmation: gate, then re-entry after the
    // dialog answers the question. --

    #[tokio::test]
    async fn close_tab_with_an_agent_opens_a_confirm_dialog_then_closes_on_reentry() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-close-tab-agent");
        server.run_ok(&["new-session", "-d", "-s", "s", "-n", "one", "/bin/sh"]);
        server.run_ok(&["new-window", "-t", "s", "-n", "two", "/bin/sh"]);
        let ids = window_ids(&server, "s");
        let target_window = ids[0].clone();
        let pane = pane_ids(&server, &target_window)[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-close-tab-agent-home");
        spawn_status_daemon_with_agent(&home, &pane);

        let mut model = one_tab_model("s", &ids[1], "%unused", "$0");
        model.session.tabs = vec![
            tab(&target_window, "one", &pane),
            tab(&ids[1], "two", "%unused"),
        ];
        model.session.active_tab = 1;
        model.workspaces[0].window_ids = vec![target_window.clone(), ids[1].clone()];
        let mut app = test_app(model, home.clone());

        let outcome = execute(
            &mut app,
            &client,
            Action::CloseTab {
                window_id: target_window.clone(),
            },
        )
        .await
        .expect("gate check reaches the daemon");

        assert!(
            !outcome.reconcile,
            "must ask before closing a tab with an agent"
        );
        assert!(
            matches!(&app.dialog, Some(Dialog::ConfirmCloseTab { window_id }) if window_id == &target_window),
            "expected a confirm dialog, got {:?}",
            app.dialog
        );

        // Enter on the open dialog resolves to the SAME action and re-enters
        // the executor — this time it executes, because the dialog already
        // answered the question.
        let outcome = execute(
            &mut app,
            &client,
            Action::CloseTab {
                window_id: target_window.clone(),
            },
        )
        .await
        .expect("confirmed close executes");

        assert!(outcome.reconcile);
        assert!(app.dialog.is_none());
        assert_eq!(window_names(&server, "s"), vec!["two"]);

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    // -- New-workspace naming and uniqueness, end to end through the
    // executor (the pure policy itself is `naming`'s own tests). --

    #[tokio::test]
    async fn new_workspace_names_after_the_folder_and_deduplicates_against_open_workspaces() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-new-workspace");
        let folder = cyclops_proto::scratch::scratch_dir("cyclops-exec-new-workspace-folder");
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("folder");
        let folder_name = folder.file_name().unwrap().to_string_lossy().to_string();
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "host",
            "-c",
            folder.to_str().expect("UTF-8 scratch path"),
            "/bin/sh",
        ]);
        let pane = pane_ids(&server, "host")[0].clone();
        let client = rig_client(&server, "host").await;
        let mut model = one_tab_model("host", "@0", &pane, "$host");
        // A workspace already open under the folder's own name forces the
        // uniqueness suffix.
        model.workspaces.push(WorkspaceRow {
            session_id: "$existing".into(),
            name: folder_name.clone(),
            tab_count: 1,
            window_ids: Vec::new(),
        });
        let mut app = test_app(model, scratch_home("exec-new-workspace-home"));

        let outcome = execute(&mut app, &client, Action::NewWorkspace)
            .await
            .expect("create workspace");

        assert!(outcome.reconcile);
        assert!(
            outcome.persist,
            "a freshly created folder-following workspace must be tracked"
        );
        assert_eq!(app.prefs.folder_tracked.len(), 1);
        let sessions = server.run(&["list-sessions", "-F", "#{session_name}"]);
        let sessions = String::from_utf8_lossy(&sessions.stdout);
        let expected = format!("{folder_name}-2");
        assert!(
            sessions.lines().any(|name| name == expected),
            "expected a deduplicated name {expected:?}, got {sessions}"
        );
        // "host" was the active workspace (index 0) when the new one was
        // created, so the new name must be spliced directly below it — not
        // appended after "existing", which is where tmux's own session
        // ordering would otherwise have left it.
        assert_eq!(
            app.prefs.workspace_order,
            vec!["host".to_string(), expected, folder_name],
            "the new workspace must land directly under the workspace that was active, not wherever tmux's ordering puts it"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A sibling of the naming test above, focused purely on placement: the
    /// active workspace sits in the *middle* of a longer sidebar list, so
    /// the new row landing right after it (rather than at the front or the
    /// back, both of which a less careful splice could produce by accident)
    /// proves the splice targets whichever workspace is active.
    #[tokio::test]
    async fn new_workspace_splices_into_the_order_right_after_the_active_workspace() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-new-workspace-splice");
        let folder =
            cyclops_proto::scratch::scratch_dir("cyclops-exec-new-workspace-splice-folder");
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("folder");
        let folder_name = folder.file_name().unwrap().to_string_lossy().to_string();
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "host",
            "-c",
            folder.to_str().expect("UTF-8 scratch path"),
            "/bin/sh",
        ]);
        let pane = pane_ids(&server, "host")[0].clone();
        let client = rig_client(&server, "host").await;
        // "host" (the active tab's own session) sits first, with "alpha" and
        // "beta" trailing behind it — but the active row is moved to
        // "alpha", the middle of the three, so the splice target is neither
        // the first nor the last row in the list.
        let mut model = one_tab_model("host", "@0", &pane, "$host");
        model.workspaces.push(WorkspaceRow {
            session_id: "$alpha".into(),
            name: "alpha".into(),
            tab_count: 1,
            window_ids: Vec::new(),
        });
        model.workspaces.push(WorkspaceRow {
            session_id: "$beta".into(),
            name: "beta".into(),
            tab_count: 1,
            window_ids: Vec::new(),
        });
        model.active_workspace = 1;
        let mut app = test_app(model, scratch_home("exec-new-workspace-splice-home"));

        let outcome = execute(&mut app, &client, Action::NewWorkspace)
            .await
            .expect("create workspace");

        assert!(outcome.reconcile);
        assert!(outcome.persist, "the rewritten order must be saved");
        assert_eq!(
            app.prefs.workspace_order,
            vec![
                "host".to_string(),
                "alpha".to_string(),
                folder_name,
                "beta".to_string(),
            ],
            "the new row belongs directly after the active workspace (alpha), not at the front or back of the list"
        );

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&folder);
    }

    // -- The sidebar: collapse, reopen, and which tab the stream chord
    // lands on. Both toggles re-declare the tmux client size, so these
    // need a real client. --

    /// Collapse and reopen: the model flips, prefs mirror it, the mirrored
    /// value survives a round trip through config.toml, and each flip
    /// re-declares the tmux client size for the columns the canvas just
    /// gained or lost.
    #[tokio::test]
    async fn toggling_the_sidebar_persists_and_redeclares_the_client_size() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-toggle-sidebar");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-toggle-sidebar-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        assert!(app.model.sidebar_visible, "the default is open");

        let outcome = execute(&mut app, &client, Action::ToggleSidebar)
            .await
            .expect("collapse");
        assert!(outcome.persist, "the new visibility belongs on disk");
        assert!(!app.model.sidebar_visible);
        assert!(!app.prefs.sidebar_visible, "prefs mirror the model");
        let collapsed = app
            .declared_client_size
            .expect("collapsing re-declares the client size");

        // What `apply_outcome` does for the real caller; the reload below
        // has to read a file that actually exists.
        crate::persist::save_prefs(&home, &app.prefs).expect("save prefs");
        assert!(
            !crate::persist::load_prefs(&home).sidebar_visible,
            "a workspace quit collapsed must reopen collapsed"
        );

        let outcome = execute(&mut app, &client, Action::ToggleSidebar)
            .await
            .expect("reopen");
        assert!(outcome.persist);
        assert!(app.model.sidebar_visible);
        assert!(app.prefs.sidebar_visible);
        let reopened = app
            .declared_client_size
            .expect("reopening re-declares the client size");
        assert_ne!(
            reopened, collapsed,
            "the canvas has to give the sidebar's columns back"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn toggle_messages_persists_visibility_and_redecares_client_size() {
        use cyclops_testrig::{tmux_available, TmuxServer};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-toggle-messages");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-toggle-messages-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        assert!(!app.model.messages_visible, "the default is collapsed");

        let outcome = execute(&mut app, &client, Action::ToggleMessages)
            .await
            .expect("open messages drawer");
        assert!(outcome.persist, "the new visibility belongs on disk");
        assert!(app.model.messages_visible);
        assert!(app.prefs.messages_visible, "prefs mirror the model");
        let opened = app
            .declared_client_size
            .expect("opening messages drawer re-declares the client size");

        crate::persist::save_prefs(&home, &app.prefs).expect("save prefs");
        assert!(
            crate::persist::load_prefs(&home).messages_visible,
            "a workspace quit with messages open must reopen with messages open"
        );

        let outcome = execute(&mut app, &client, Action::ToggleMessages)
            .await
            .expect("collapse messages drawer");
        assert!(outcome.persist);
        assert!(!app.model.messages_visible);
        assert!(!app.prefs.messages_visible);
        let collapsed = app
            .declared_client_size
            .expect("collapsing messages drawer re-declares the client size");
        assert_ne!(
            opened, collapsed,
            "the canvas must resize when messages drawer collapses"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    /// The stream chord keeps its meaning through the merge: it shows the
    /// stream from wherever the sidebar is, and hides the sidebar when the
    /// stream is already what's showing. A plain tab click is the quiet
    /// case beside it: the tab persists and tmux is told nothing, because
    /// the sidebar kept its columns.
    #[tokio::test]
    async fn the_stream_chord_toggles_the_stream_and_never_strands_a_tab() {
        // The Stream tab is off while it is revised
        // (`persist::STREAM_TAB`). The chord has nowhere to toggle to, so
        // this pins behavior that comes back with the tab rather than
        // behavior the build currently has.
        if !crate::persist::STREAM_TAB {
            return;
        }
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-stream-tab");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-stream-tab-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        assert_eq!(app.sidebar_tab, SidebarTab::Sessions);

        // Open on Sessions: the sidebar stays open and swaps tabs.
        execute(&mut app, &client, Action::ToggleEventPanel)
            .await
            .expect("show the stream");
        assert!(app.model.sidebar_visible);
        assert_eq!(app.sidebar_tab, SidebarTab::Stream);
        assert_eq!(app.prefs.sidebar_tab, SidebarTab::Stream);

        // The stream is already what's showing: the chord takes it away
        // and the session list comes back. It must NOT hide the sidebar,
        // which would leave a keyboard-only operator on Stream with no
        // route back to Sessions, persisted across restarts.
        let declared_open = app.declared_client_size;
        execute(&mut app, &client, Action::ToggleEventPanel)
            .await
            .expect("take the stream away");
        assert!(app.model.sidebar_visible, "the sidebar never hides here");
        assert_eq!(app.sidebar_tab, SidebarTab::Sessions);
        assert_eq!(app.prefs.sidebar_tab, SidebarTab::Sessions);
        assert_eq!(
            app.declared_client_size, declared_open,
            "the sidebar keeps its columns, so tmux hears nothing"
        );
        execute(&mut app, &client, Action::ToggleEventPanel)
            .await
            .expect("show the stream again");
        assert!(app.model.sidebar_visible);
        assert_eq!(app.sidebar_tab, SidebarTab::Stream);

        // Collapsed and sitting on Sessions: one chord has to both open
        // the sidebar and land it on the stream.
        app.model.sidebar_visible = false;
        app.sidebar_tab = SidebarTab::Sessions;
        execute(&mut app, &client, Action::ToggleEventPanel)
            .await
            .expect("open onto the stream");
        assert!(app.model.sidebar_visible);
        assert_eq!(app.sidebar_tab, SidebarTab::Stream);

        // A header click back to Sessions: persisted, no resize.
        let declared = app.declared_client_size;
        let outcome = execute(
            &mut app,
            &client,
            Action::SelectSidebarTab {
                tab: SidebarTab::Sessions,
            },
        )
        .await
        .expect("select the sessions tab");
        assert!(outcome.persist);
        assert_eq!(app.sidebar_tab, SidebarTab::Sessions);
        assert_eq!(app.prefs.sidebar_tab, SidebarTab::Sessions);
        assert_eq!(
            app.declared_client_size, declared,
            "a tab switch costs the canvas no columns, so tmux hears nothing"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    // -- Settings: the theme listing filter, the two ends of an apply, and
    //    the sound switch. --

    /// The settings card open on its theme section, the shape every
    /// theme test below starts from.
    fn theme_settings(names: Vec<String>, selected: usize, active: Option<usize>) -> Dialog {
        Dialog::Settings {
            section: SettingsSection::Theme,
            themes: ThemePicker {
                names,
                selected,
                active,
                notice: None,
            },
            view: ViewSwitches::new(true, true),
            sound: SoundPicker::new(false, vec!["system".into()], "system"),
        }
    }

    #[tokio::test]
    async fn show_settings_offers_only_loadable_themes_and_marks_the_active_one() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-show-themes");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-show-themes-home");
        // One loadable theme, one broken file, one that parses but paints
        // nothing: the listing must offer only the first, the CLI's rule.
        write_theme(
            &home,
            "dark",
            "name = \"dark\"\n[surface]\ndim = \"#111111\"\n",
        );
        write_theme(&home, "broken", "[surface\n");
        write_theme(&home, "empty", "name = \"empty\"\n");
        std::fs::write(home.join("config.toml"), "theme = \"dark\"\n").expect("config");
        std::fs::create_dir_all(home.join("sounds")).expect("sounds dir");
        std::fs::write(home.join("sounds/chime.wav"), b"").expect("a sound");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());

        app.prefs.sound_notifs = true;
        app.prefs.sound = "chime".into();
        let outcome = execute(
            &mut app,
            &client,
            Action::ShowSettings {
                section: SettingsSection::Theme,
            },
        )
        .await
        .expect("open the card");

        assert!(!outcome.reconcile);
        assert!(!outcome.persist);
        match &app.dialog {
            Some(Dialog::Settings {
                section,
                themes,
                view,
                sound,
            }) => {
                assert_eq!(*section, SettingsSection::Theme, "opens on themes");
                assert_eq!(
                    *view,
                    ViewSwitches::new(true, true),
                    "reads the surfaces as shown, cursor on the first row"
                );
                assert_eq!(themes.names, vec!["dark".to_string()]);
                assert_eq!(themes.selected, 0, "the arrows start on the active row");
                assert_eq!(themes.active, Some(0));
                assert_eq!(themes.notice, None);
                assert_eq!(
                    *sound,
                    SoundPicker::new(true, vec!["chime".into(), "system".into()], "chime"),
                    "the saved switch, the installed sounds then the bell, the saved cue"
                );
            }
            other => panic!("expected the settings card, got {other:?}"),
        }
        assert!(
            app.theme_restore.is_some(),
            "the live paint rides beside the open picker for Esc"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn apply_theme_writes_the_key_and_closes_when_the_daemon_confirms() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-apply-theme");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-apply-theme-home");
        write_theme(
            &home,
            "solar",
            "name = \"solar\"\n[surface]\ndim = \"#222222\"\n",
        );
        // A config someone wrote: the apply must edit one line and keep
        // the rest, comments and order included.
        let before = "# my config\nsessions = [\"main\"]\ntheme = \"dark\"\nchrome = \"off\"\n";
        std::fs::write(home.join("config.toml"), before).expect("config");
        spawn_theme_reload_daemon(&home, "solar");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        app.theme_restore = Some(app.paint.theme.clone());
        app.open_dialog(theme_settings(
            vec!["dark".into(), "solar".into()],
            1,
            Some(0),
        ));

        let outcome = execute(
            &mut app,
            &client,
            Action::ApplyTheme {
                name: "solar".into(),
            },
        )
        .await
        .expect("apply executes");

        assert_eq!(outcome, Outcome::default(), "no reconcile, no persist");
        assert!(app.dialog.is_none(), "a confirmed switch closes the picker");
        assert!(
            app.theme_restore.is_none(),
            "an applied theme is the watch's to own again"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).expect("read config"),
            "# my config\nsessions = [\"main\"]\ntheme = \"solar\"\nchrome = \"off\"\n"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn apply_theme_without_a_daemon_saves_and_tells_the_next_command_story() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-apply-theme-down");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-apply-theme-down-home");
        write_theme(
            &home,
            "solar",
            "name = \"solar\"\n[surface]\ndim = \"#222222\"\n",
        );
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        app.theme_restore = Some(app.paint.theme.clone());
        app.open_dialog(theme_settings(
            vec!["dark".into(), "solar".into()],
            1,
            Some(0),
        ));

        execute(
            &mut app,
            &client,
            Action::ApplyTheme {
                name: "solar".into(),
            },
        )
        .await
        .expect("apply executes");

        // The config write happened; only the immediacy is missing, and
        // the open picker says so with the CLI's own story.
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).expect("read config"),
            "theme = \"solar\"\n"
        );
        match &app.dialog {
            Some(Dialog::Settings { themes, .. }) => {
                assert_eq!(
                    themes.notice.as_deref(),
                    Some(crate::copy::THEME_SAVED_NO_DAEMON)
                );
                assert_eq!(themes.active, Some(1), "the selection did switch");
            }
            other => panic!("expected the picker to stay open, got {other:?}"),
        }
        assert!(
            app.theme_restore.is_some(),
            "an open picker still owns the paint"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    /// Browsing previews: the highlighted row's theme becomes the live
    /// paint, and nothing touches the config until Enter.
    #[test]
    fn selection_preview_paints_without_writing_the_config() {
        let home = scratch_home("exec-preview-theme-home");
        write_theme(
            &home,
            "solar",
            "name = \"solar\"\n[surface]\ndim = \"#222222\"\n",
        );
        std::fs::write(home.join("config.toml"), "theme = \"dark\"\n").expect("config");
        let mut app = test_app(one_tab_model("s", "@0", "%0", "$0"), home.clone());
        app.theme_restore = Some(app.paint.theme.clone());
        app.open_dialog(theme_settings(
            vec!["dark".into(), "solar".into()],
            1,
            Some(0),
        ));

        preview_selected_theme(&mut app);

        assert_eq!(
            app.paint
                .theme
                .resolve(cyclops_theme::tokens::SURFACE_DIM)
                .rgb,
            (0x22, 0x22, 0x22),
            "the highlighted theme is the live paint"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).expect("read config"),
            "theme = \"dark\"\n",
            "browsing must not write the config"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A file that broke after the picker listed it previews as nothing:
    /// the paint on screen stays and the picker keeps working.
    #[test]
    fn a_broken_theme_under_the_cursor_keeps_the_prior_paint() {
        let home = scratch_home("exec-preview-broken-home");
        write_theme(
            &home,
            "solar",
            "name = \"solar\"\n[surface]\ndim = \"#222222\"\n",
        );
        write_theme(
            &home,
            "lunar",
            "name = \"lunar\"\n[surface]\ndim = \"#333333\"\n",
        );
        write_theme(&home, "broken", "[surface\n");
        let mut app = test_app(one_tab_model("s", "@0", "%0", "$0"), home.clone());
        app.theme_restore = Some(app.paint.theme.clone());
        // The listing only offers loadable rows, so "broken" being a row
        // means the file broke while the picker was open.
        app.open_dialog(theme_settings(
            vec!["broken".into(), "lunar".into(), "solar".into()],
            2,
            None,
        ));
        let select = |app: &mut App, row: usize| {
            if let Some(Dialog::Settings { themes, .. }) = app.dialog.as_mut() {
                themes.selected = row;
            }
            preview_selected_theme(app);
        };
        let dim = |app: &App| {
            app.paint
                .theme
                .resolve(cyclops_theme::tokens::SURFACE_DIM)
                .rgb
        };

        select(&mut app, 2);
        assert_eq!(dim(&app), (0x22, 0x22, 0x22), "solar previews");
        select(&mut app, 0);
        assert_eq!(
            dim(&app),
            (0x22, 0x22, 0x22),
            "a broken file previews as nothing"
        );
        select(&mut app, 1);
        assert_eq!(dim(&app), (0x33, 0x33, 0x33), "the picker is not wedged");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The sound switch: Enter on its row saves the preference, and the
    /// card closes the way Esc would, putting back a theme that was only
    /// browsed.
    #[tokio::test]
    async fn apply_sound_settings_saves_closes_and_restores_a_browsed_theme() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-set-sound");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-set-sound-home");
        write_theme(
            &home,
            "solar",
            "name = \"solar\"\n[surface]\ndim = \"#222222\"\n",
        );
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        let dim = cyclops_theme::tokens::SURFACE_DIM;
        let original = app.paint.theme.resolve(dim).rgb;
        app.theme_restore = Some(app.paint.theme.clone());
        app.open_dialog(Dialog::Settings {
            section: SettingsSection::Sound,
            themes: ThemePicker {
                names: vec!["solar".into()],
                selected: 0,
                active: None,
                notice: None,
            },
            view: ViewSwitches::new(true, true),
            sound: SoundPicker::new(false, vec!["system".into()], "system"),
        });
        preview_selected_theme(&mut app);
        assert_ne!(app.paint.theme.resolve(dim).rgb, original, "browsed");

        let outcome = execute(
            &mut app,
            &client,
            Action::ApplySoundSettings {
                on: true,
                cue: None,
            },
        )
        .await
        .expect("save the switch");

        assert!(outcome.persist, "the switch belongs on disk");
        assert!(app.prefs.sound_notifs);
        assert!(app.dialog.is_none(), "a saved switch closes the card");
        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            original,
            "a browsed theme was not applied, so it goes back"
        );
        assert!(app.theme_restore.is_none());

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    /// Landing on the switch's other row moves the check and nothing
    /// else: no pref changes, no file is written, the card stays open,
    /// and Esc forgets it. Enter is what saves the checks.
    #[tokio::test]
    async fn landing_on_a_sound_row_checks_it_without_saving() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-sound-landing");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-sound-landing-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        app.open_dialog(Dialog::Settings {
            section: SettingsSection::Sound,
            themes: ThemePicker {
                names: Vec::new(),
                selected: 0,
                active: None,
                notice: None,
            },
            view: ViewSwitches::new(true, true),
            sound: SoundPicker::new(false, vec!["system".into()], "system"),
        });

        if let Some(open) = app.dialog.as_mut() {
            crate::dialog::select_settings_row(open, 0);
        }
        settings_cursor_moved(&mut app, true);

        match &app.dialog {
            Some(Dialog::Settings { sound, .. }) => {
                assert!(sound.on && sound.is_checked(0), "the check moved");
            }
            other => panic!("the card stays open, got {other:?}"),
        }
        assert!(!app.prefs.sound_notifs, "landing saves nothing");
        assert!(!home.join("config.toml").exists(), "and writes nothing");

        super::super::dialog_cancel(&mut app);
        assert!(!app.prefs.sound_notifs, "Esc forgets the check");
        assert!(!home.join("config.toml").exists());

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    /// Both checks are saved by one Enter: the switch and the cue.
    #[tokio::test]
    async fn apply_sound_settings_saves_the_cue_and_closes_the_card() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("exec-set-sound-name");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let pane = pane_ids(&server, "s")[0].clone();
        let client = rig_client(&server, "s").await;
        let home = scratch_home("exec-set-sound-name-home");
        let mut app = test_app(one_tab_model("s", "@0", &pane, "$0"), home.clone());
        app.prefs.sound_notifs = true;
        app.open_dialog(Dialog::Settings {
            section: SettingsSection::Sound,
            themes: ThemePicker {
                names: Vec::new(),
                selected: 0,
                active: None,
                notice: None,
            },
            view: ViewSwitches::new(true, true),
            sound: SoundPicker::new(
                true,
                vec!["bow-ripple".into(), "system".into()],
                "bow-ripple",
            ),
        });

        let outcome = execute(
            &mut app,
            &client,
            Action::ApplySoundSettings {
                on: true,
                cue: Some("system".into()),
            },
        )
        .await
        .expect("save the cue");

        assert!(outcome.persist);
        assert_eq!(app.prefs.sound, "system");
        assert!(app.prefs.sound_notifs, "the switch is saved with it");
        assert!(app.dialog.is_none());

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    // -- Insertion ordering: matches `action::resolve_insertion`'s
    // direction rule (down inserts after, up inserts before). --

    #[test]
    fn apply_insertion_moves_down_after_and_up_before() {
        let mut down = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(apply_insertion(
            &mut down,
            "a",
            &Insertion::After("c".to_string())
        ));
        assert_eq!(down, vec!["b", "c", "a"]);

        let mut up = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(apply_insertion(
            &mut up,
            "c",
            &Insertion::Before("a".to_string())
        ));
        assert_eq!(up, vec!["c", "a", "b"]);
    }

    #[test]
    fn apply_insertion_rejects_a_stale_target() {
        let mut order = vec!["a".to_string(), "b".to_string()];
        assert!(!apply_insertion(
            &mut order,
            "a",
            &Insertion::Before("gone".to_string())
        ));
        assert_eq!(order, vec!["a", "b"]);
    }
}
