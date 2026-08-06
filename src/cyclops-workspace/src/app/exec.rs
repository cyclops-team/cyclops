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

use super::App;
use crate::action::{Action, Insertion, TabDestination};
use crate::daemon;
use crate::decoration::{self, DecorationSnapshot};
use crate::dialog::Dialog;
use crate::naming;

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
        Action::ScrollPane { pane_id, lines } => {
            // Local scrollback only; this never reaches tmux.
            if let Some(rt) = app.runtimes.get_mut(&pane_id) {
                rt.scroll(lines);
            }
            Ok(Outcome::default())
        }
        Action::RequestNamePane { pane_id } => {
            let buffer = app
                .decoration
                .pane(&pane_id)
                .and_then(|decoration| decoration.label.clone())
                .unwrap_or_default();
            app.dialog = Some(Dialog::NamePane {
                pane_id,
                buffer,
                error: None,
            });
            Ok(Outcome::default())
        }
        Action::NamePane { pane_id, label } => name_pane(app, pane_id, label),

        Action::RequestNewTab => {
            app.dialog = Some(Dialog::NewTab {
                buffer: String::new(),
            });
            Ok(Outcome::default())
        }
        Action::NewTab { name } => new_tab(app, client, name).await,
        Action::SelectTab { window_id } => select_tab(app, client, window_id).await,
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
            app.dialog = Some(Dialog::RenameTab {
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
        Action::CloseTab { window_id } => close_tab(app, client, window_id).await,
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
            app.dialog = Some(Dialog::RenameWorkspace {
                buffer: session.clone(),
                session,
            });
            Ok(Outcome::default())
        }
        Action::RenameWorkspace { session, name } => {
            rename_workspace(app, client, session, name).await
        }
        Action::RequestCloseWorkspace { session } => {
            app.dialog = Some(Dialog::ConfirmCloseWorkspace { session });
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

        Action::ToggleEventPanel => {
            app.event_stream_open = !app.event_stream_open;
            super::resize_client(app, client).await;
            Ok(Outcome::default())
        }
        Action::ShowKeybinds => {
            app.dialog = Some(Dialog::Keybinds {
                scroll: 0,
                rows: app.router.help(),
            });
            Ok(Outcome::default())
        }
        Action::Detach => Ok(Outcome {
            detach: true,
            ..Outcome::default()
        }),
    }
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
            app.dialog = Some(Dialog::confirm_close(pane_id));
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
            app.dialog = Some(Dialog::ConfirmCloseTab { window_id });
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
    if let Some(snapshot) = decoration::fetch_decoration(&app.home) {
        app.decoration = snapshot;
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
    let persist = if app.prefs.folder_tracked.contains(&session_id) {
        false
    } else {
        app.prefs.folder_tracked.push(session_id);
        true
    };
    Ok(Outcome {
        reconcile: true,
        persist,
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cyclops_testrig::{tmux_available, TmuxServer};
    use cyclops_tmux::{ControlClient, ControlConfig, SplitDirection};

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
        }
    }

    fn test_app(model: WorkspaceModel, home: std::path::PathBuf) -> App {
        App {
            model,
            runtimes: RuntimeRegistry::default(),
            router: Router::new(default_bindings()),
            paint: Paint::for_test(),
            dialog: None,
            link_state: LinkState::Live,
            paused_panes: HashSet::new(),
            reconnect_attempt: 0,
            hit_map: HitMap::default(),
            menu: MenuState::None,
            hover: None,
            selection: SelectionState::default(),
            drag: None,
            decoration: DecorationSnapshot::default(),
            prefs: WorkspacePrefs::default(),
            expanded_workspaces: HashSet::new(),
            expanded_for: None,
            watched_sessions: HashSet::new(),
            event_stream_open: false,
            record: cyclops_ui::Record::new(),
            intake: cyclops_ui::Intake::new(),
            cursor_style: None,
            term_size: (80, 24),
            declared_client_size: None,
            needs_reconcile: false,
            needs_hydrate: false,
            paste_seq: 0,
            home,
            folder_probe_at: None,
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

        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&folder);
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
