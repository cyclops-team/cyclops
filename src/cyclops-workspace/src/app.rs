//! Workspace application state and event loop.
//!
//! The loop is event-armed: every visible change arms one render debounce
//! (`RENDER_DEBOUNCE`) if none is pending — arming never pushes an
//! already-armed deadline back, so a stream of events cannot starve
//! rendering. Full model reconciliation (one control-mode workspace
//! snapshot; see `crate::sync::fetch_workspace_model`) is deferred onto that
//! same deadline and coalesced through `needs_reconcile`; cheap structural
//! notifications (`%layout-change`, `%window-pane-changed`,
//! `%session-changed`) apply to the model directly without a full fetch.
//! Daemon decoration events get the identical treatment on their own
//! dedicated thread: `spawn_decoration_forwarder` arms one debounce per
//! burst instead of fetching status once per pushed event, and its
//! subscription outlives the daemon (`run_decoration_forwarder`
//! reconnects on a bounded backoff and resyncs on every connect).
//!
//! This module owns boot, the event queue, render scheduling, reconnect
//! orchestration, and top-level `App` state. It does not own tmux command
//! strings (`cyclops-tmux` and `action::route_*`), device-event decoding
//! (`input`), dialog text-editing (`dialog`), or frame composition
//! (`render`) — it calls into those and reacts to their results.

#![allow(clippy::too_many_arguments)]

use std::collections::HashSet;
use std::io;

use crossterm::event::{
    self, Event, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use cyclops_tmux::{ControlClient, ControlConfig, Notification};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Duration, Instant};

use crate::action;
use crate::bindings::load_bindings;
use crate::config::load_tmux_config;
use crate::copy;
use crate::decoration::{self, DecorationSnapshot};
use crate::dialog::{self, Dialog, DialogKeyAction};
use crate::drag::{DragState, DragTarget};
use crate::input::encode_send_keys;
use crate::input::mouse::{HitMap, HitTarget, MenuState};
use crate::input::router::{Router, RouterResult};
use crate::layout::SplitDir;
use crate::model::{pane_is_visible, RuntimeRegistry, TabModel, WorkspaceModel};
use crate::naming;
use crate::persist::{self, load_prefs, set_last_active, SidebarTab, WorkspacePrefs};
use crate::render::{paint_dialog, paint_menu, paint_sidebar, paint_tab_bar, paint_window};
use crate::resilience::{self, LinkState};
use crate::selection::{self, SelectionState};
use crate::sync::{fetch_workspace_model, hydrate_visible_tab};
use crate::term_guard::TermGuard;
use crate::theme::Paint;

mod exec;

/// At most one frame per 8 ms (~120 Hz). The timer exists only after an
/// event; idle workspaces still have no wakeups.
const RENDER_DEBOUNCE: Duration = Duration::from_millis(8);
/// How long after pane output a folder-following workspace re-reads its
/// pane's directory.
///
/// It has to be a DELAY, not a render-time check. There is no tmux
/// notification for a `cd`, so the only signal is the pane output the
/// command produces — and the first of that output is the shell echoing the
/// line the user typed, which arrives before the shell has run `chdir`.
/// MEASURED: probing on that edge reads the old path every time, and a pane
/// that then sits idle produces no further output, so the name never catches
/// up. Waiting lets the shell land first. One armed probe at a time, never
/// pushed back, so a noisy pane costs one tmux round trip per interval and
/// an idle workspace costs no wakeups at all.
const FOLDER_PROBE_DELAY: Duration = Duration::from_millis(600);
/// How long a burst of daemon decoration events (state, label, delivery
/// changes) is allowed to coalesce before `spawn_decoration_forwarder`
/// issues one status fetch. Same rule as `RENDER_DEBOUNCE`/`arm()`: armed
/// once by the first event in a burst, fired once, never pushed back by a
/// later event in the same burst. A split or a border drag can push several
/// events through cyclopsd at once; without this, each one used to cost its
/// own blocking status round trip even though only the last result before
/// the next paint was ever shown.
const DECORATION_DEBOUNCE: Duration = Duration::from_millis(30);

enum AppMsg {
    Input(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    OutputBatch(Vec<(String, Vec<u8>)>),
    Redraw,
    Resized(u16, u16),
    Reconcile,
    SessionSwitched {
        session: String,
        name: String,
    },
    SessionRenamed {
        session: Option<String>,
        name: String,
    },
    LayoutChanged {
        window: String,
        layout: String,
        flags: Option<String>,
    },
    ActivePaneChanged {
        window: String,
        pane: String,
    },
    LinkLost,
    PanePaused {
        pane: String,
    },
    PaneContinued {
        pane: String,
    },
    DecorationChanged(DecorationSnapshot),
    /// The daemon subscription (re)connected. The handler resyncs what an
    /// outage loses on both sides: a restarted daemon forgot every watch
    /// ask (those live in its memory, not config.toml), and any state that
    /// changed while nothing was subscribed produced no event, so only a
    /// full refetch can replace it.
    DaemonReconnected,
    /// One daemon event, already normalized onto the shared `cyclops
    /// watch` stream vocabulary (E2) by [`spawn_decoration_forwarder`]'s
    /// reader thread. Feeds `App::record` and nothing else — decoration's
    /// own coalesced refresh is a separate message (`DecorationChanged`)
    /// on the same connection, so this must never gate or delay it.
    StreamEntry(Box<cyclops_ui::Entry>),
    /// The daemon switched themes (`cyclops theme <name>`), forwarded by
    /// [`spawn_decoration_forwarder`]'s reader thread. Wake-only, like
    /// cyclops-ui's `UiMsg::ThemeChanged`: the handler arms the render
    /// debounce and the reload itself runs on that deadline, before draw.
    ThemeChanged,
}

struct App {
    model: WorkspaceModel,
    runtimes: RuntimeRegistry,
    router: Router,
    paint: Paint,
    dialog: Option<Dialog>,
    /// The theme that was live when the theme picker opened; `Some` for
    /// exactly as long as the picker is. Selection moves preview the
    /// highlighted theme straight into `paint.theme`
    /// (`exec::preview_selected_theme`); closing without applying puts
    /// this back, and applying drops it, the previewed paint being the
    /// theme the daemon confirms. While set, [`refresh_theme_watch`]
    /// holds the ThemeWatch off so a reload cannot overwrite the preview.
    theme_restore: Option<cyclops_theme::Theme>,
    link_state: LinkState,
    paused_panes: HashSet<String>,
    reconnect_attempt: usize,
    hit_map: HitMap,
    menu: MenuState,
    /// Mouse cell, tracked while a menu or dialog is open so its rows can
    /// paint a hover highlight.
    hover: Option<(u16, u16)>,
    selection: SelectionState,
    drag: Option<DragState>,
    decoration: DecorationSnapshot,
    prefs: WorkspacePrefs,
    /// Stable session ids whose agent children are visible in the sidebar.
    expanded_workspaces: HashSet<String>,
    /// The session id the sidebar last auto-expanded for. Only a CHANGE
    /// opens a row; see [`expand_active_workspace`].
    expanded_for: Option<String>,
    /// Session ids cyclopsd has already been asked to watch. Keyed by id,
    /// not name, so a folder-following rename never asks twice; see
    /// [`crate::daemon::watch_session`].
    watched_sessions: HashSet<String>,
    /// Which tab the sidebar is showing. Seeded from prefs at boot and
    /// written back on every change, so a workspace reopens on the tab it
    /// was left on.
    sidebar_tab: SidebarTab,
    /// The shared `cyclops watch` stream model (E2): the same ordered,
    /// identity-stable [`cyclops_ui::Entry`] rows that surface renders,
    /// fed by [`spawn_decoration_forwarder`]'s `events.subscribe`
    /// connection so the Stream tab never opens a second one.
    ///
    /// Fed whether or not the Stream tab is showing. The record is cheap
    /// (no IO per event, and internally ring-capped) and stopping the feed
    /// while the sidebar sits on Sessions would show an empty history the
    /// moment the tab is selected mid-session; keeping it warm costs
    /// nothing a stopped feed would otherwise save.
    ///
    /// Built and fed through [`crate::event_record`]: the same replayed
    /// ledger tail, status seed, and live ordering `cyclops watch` runs,
    /// so the Stream tab and the CLI show one history rather than two that
    /// agree only on formatting.
    record: cyclops_ui::Record,
    /// Startup ordering and seq dedup for `record`
    /// ([`cyclops_ui::Intake`]): live entries reaching the app before
    /// [`crate::event_record::boot`] lands its backfill buffer here, and
    /// ledger-backed duplicates arriving live during startup are dropped
    /// by seq instead of shown twice.
    intake: cyclops_ui::Intake,
    /// The (shape, blink) last emitted to the host terminal
    /// ([`crate::term_guard::apply_cursor_style`]), so a frame whose
    /// focused-pane cursor did not change costs no terminal write. `None`
    /// until the first visible cursor is drawn.
    cursor_style: Option<(crate::runtime::CursorShape, bool)>,
    term_size: (u16, u16),
    /// Last size successfully declared by this control client. Avoids a
    /// resize notification loop when expanded pane gutters are already at
    /// their target geometry.
    declared_client_size: Option<(u16, u16)>,
    /// Window ids already pinned to `window-size smallest`
    /// ([`pin_window_sizes`]), so reconciliation asks tmux once per window
    /// rather than once per snapshot. Cleared on reconnect: a restarted
    /// server can reuse ids for windows that were never pinned.
    pinned_windows: HashSet<String>,
    needs_reconcile: bool,
    /// A structural notification changed visible pane dimensions. Hydration
    /// waits for the render deadline so resize bursts collapse to one set of
    /// captures instead of blocking the input path for every intermediate.
    needs_hydrate: bool,
    paste_seq: u64,
    home: std::path::PathBuf,
    /// When the next folder probe is due. `None` means none is armed; see
    /// [`arm_folder_probe`].
    folder_probe_at: Option<Instant>,
}

fn toggle_workspace_expanded(expanded: &mut HashSet<String>, session_id: String) -> bool {
    if expanded.remove(&session_id) {
        false
    } else {
        expanded.insert(session_id);
        true
    }
}

/// Arm the render debounce if none is pending. Never pushes an armed
/// deadline back — that would let a busy event stream starve rendering.
fn arm(debounce: &mut Option<Instant>) {
    if debounce.is_none() {
        *debounce = Some(Instant::now() + RENDER_DEBOUNCE);
    }
}

enum Wake {
    Message(Option<AppMsg>),
    Deadline,
}

/// Wait for a message or the next armed deadline. The explicit due check is
/// what makes the render guarantee real: a permanently ready message queue
/// cannot keep winning a biased select after the deadline has passed.
async fn next_wake(rx: &mut mpsc::UnboundedReceiver<AppMsg>, deadline: Option<Instant>) -> Wake {
    if deadline.is_some_and(|at| at <= Instant::now()) {
        return Wake::Deadline;
    }
    let Some(deadline) = deadline else {
        return Wake::Message(rx.recv().await);
    };
    tokio::select! {
        biased;
        msg = rx.recv() => Wake::Message(msg),
        _ = sleep_until(deadline) => Wake::Deadline,
    }
}

/// What the boot path does with the reopen decision: attach to `.0`, or
/// create it first (`new-session -A`) because the server has nothing.
fn boot_target(reopen: &persist::ReopenTarget) -> (String, bool) {
    match reopen {
        persist::ReopenTarget::LastActive { session, .. }
        | persist::ReopenTarget::DefaultWorkspace(session)
        | persist::ReopenTarget::First(session) => (session.clone(), false),
        persist::ReopenTarget::OfferCreate => (copy::DEFAULT_SESSION_NAME.to_string(), true),
    }
}

/// Run the workspace on a tty. Returns the process exit code.
pub async fn run_async() -> i32 {
    let home = cyclops_proto::cyclops_home();
    let prefs = load_prefs(&home);
    let tmux_cfg = load_tmux_config(&home);
    let socket_name = tmux_cfg.socket.clone();
    let socket = socket_name.as_deref();
    // No server yet is the same as a server with no sessions: boot one.
    let sessions = cyclops_tmux::list_sessions(socket).unwrap_or_default();
    let session_names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
    let last_active = persist::get_last_active(&home);
    let reopen = persist::reopen_fallback(
        &session_names,
        last_active.as_ref(),
        None,
        &prefs.workspace_order,
    );
    // Creating inherits this process's cwd: `cyclops` in a project folder
    // opens a shell pane there, no preset or manual tmux required.
    let (session, create) = boot_target(&reopen);
    let mut cfg = if create {
        ControlConfig::new_session(&session).with_initial_window_name("1")
    } else {
        ControlConfig::attach(&session)
    };
    if let Some(ref sock) = socket_name {
        cfg = cfg.on_socket(sock.clone());
    }
    if let Some(path) = tmux_cfg.config_file {
        cfg = cfg.with_config_file(path);
    }
    // Pasted text is transient but can still be sensitive. Keep its 0600
    // spool files under Cyclops' private home instead of a shared temp root.
    cfg = cfg.with_buffer_spool_dir(home.join("spool"));
    let control_cfg = cfg.clone();
    let (mut client, notif_rx) = match ControlClient::spawn(cfg).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let bindings = load_bindings(&home);
    let (tx, mut rx) = mpsc::unbounded_channel::<AppMsg>();

    let input_tx = tx.clone();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(k)) => {
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                if input_tx.send(AppMsg::Input(k)).is_err() {
                    break;
                }
            }
            Ok(Event::Mouse(m)) => {
                if input_tx.send(AppMsg::Mouse(m)).is_err() {
                    break;
                }
            }
            Ok(Event::Paste(text)) => {
                if input_tx.send(AppMsg::Paste(text)).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(w, h)) => {
                let _ = input_tx.send(AppMsg::Resized(w, h));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    spawn_notif_forwarder(notif_rx, tx.clone());
    spawn_decoration_forwarder(home.clone(), tx.clone());

    // Theme detection prints warnings; do it before the alternate screen
    // swallows them.
    let paint = Paint::detect();
    // Theme hot reload: a stat re-checked on the render deadline, riding
    // events that already wake the loop (never a timer; tests/guards.rs).
    // Only meaningful while color is on. A theme file edited by hand
    // repaints on the next event; the daemon's "theme" event covers
    // `cyclops theme <name>` promptly, and that trade is accepted.
    let mut theme_watch = if paint.colors_enabled() {
        Some(cyclops_theme::ThemeWatch::new(&home))
    } else {
        None
    };

    let guard = match TermGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };

    let term_size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut model = match fetch_workspace_model(&client, &session).await {
        Ok(m) => m,
        Err(e) => {
            drop(guard);
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };
    apply_workspace_order(&mut model, &prefs.workspace_order);
    // Last-active is a tmux selection, not just local paint state. Select it
    // before sizing and hydration so bytes and geometry come from the same
    // window the user sees.
    if let persist::ReopenTarget::LastActive { window_id, .. } = &reopen {
        if let Some(index) = model
            .session
            .tabs
            .iter()
            .position(|tab| tab.window_id == *window_id)
        {
            if index != model.session.active_tab {
                if let Err(error) = client.select_window(window_id).await {
                    log_err(&home, &error);
                } else {
                    model.session.active_tab = index;
                }
            }
        }
    }

    // Pin the sizing policy before declaring the canvas, so the declaration
    // below is already the binding vote (see `set_window_size_smallest`).
    let mut pinned_windows = HashSet::new();
    pin_window_sizes(&client, &model.session.tabs, &mut pinned_windows, &home).await;

    // Declare terminal cells only after the split topology is known. tmux
    // gets pane content cells; two-cell separator bands remain UI chrome.
    // The model carries the persisted sidebar visibility BEFORE the
    // declaration is computed, because the first frame paints from the
    // model: a workspace quit collapsed would otherwise be declared one
    // sidebar wide and painted another, and the first reconcile would
    // fight the declaration. `chrome_for` is the one geometry both read.
    model.sidebar_visible = prefs.sidebar_visible;
    let chrome_canvas =
        chrome_for(Rect::new(0, 0, term_size.0, term_size.1), &model, &prefs).canvas;
    let boot_size = crate::render::tmux_client_size(chrome_canvas, model.active_tab());
    let mut declared_client_size = None;
    if boot_size.0 >= 10 && boot_size.1 >= 3 {
        match client.set_client_size(boot_size.0, boot_size.1).await {
            Ok(()) => {
                declared_client_size = Some(boot_size);
                // The resize can rebalance leaf dimensions. Re-list before
                // hydration rather than replaying captures into stale slots.
                if let Ok(resized) = fetch_workspace_model(&client, &session).await {
                    // A fresh snapshot knows nothing about UI-owned
                    // preferences; re-carry visibility the same way
                    // `install_reconciled_model` does for every later one.
                    install_reconciled_model(&mut model, resized, prefs.sidebar_visible);
                    apply_workspace_order(&mut model, &prefs.workspace_order);
                }
            }
            Err(error) => log_err(&home, &error),
        }
    }
    let mut runtimes = RuntimeRegistry::default();
    hydrate_visible_tab(&client, model.active_tab(), &mut runtimes).await;

    let mut terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
        Ok(t) => t,
        Err(e) => {
            drop(guard);
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };

    let expanded_workspaces = model
        .workspaces
        .get(model.active_workspace)
        .map(|workspace| HashSet::from([workspace.session_id.clone()]))
        .unwrap_or_default();
    let mut app = App {
        model,
        runtimes,
        router: Router::new(bindings),
        paint,
        dialog: None,
        theme_restore: None,
        link_state: LinkState::Live,
        paused_panes: HashSet::new(),
        reconnect_attempt: 0,
        hit_map: HitMap::default(),
        menu: MenuState::None,
        hover: None,
        selection: SelectionState::default(),
        drag: None,
        // Nothing to fall back to on the first frame: no answer here is
        // genuinely "nothing known yet", which is what the default says.
        decoration: decoration::fetch_decoration(&home).unwrap_or_default(),
        prefs: prefs.clone(),
        expanded_workspaces,
        expanded_for: None,
        watched_sessions: HashSet::new(),
        sidebar_tab: prefs.sidebar_tab,
        record: cyclops_ui::Record::new(),
        intake: cyclops_ui::Intake::new(),
        cursor_style: None,
        term_size,
        declared_client_size,
        pinned_windows,
        needs_reconcile: false,
        needs_hydrate: false,
        paste_seq: 0,
        home,
        folder_probe_at: None,
    };
    // Bare `cyclops` can boot a session config.toml never mentions, so the
    // very first frame is already a frame the daemon may not be watching
    // for. Ask before drawing it.
    ensure_sessions_watched(&mut app);
    app.decoration = decoration::fetch_decoration(&app.home).unwrap_or_default();
    // The subscription at the top of this function is already queuing live
    // entries on the app channel; boot's backfill-then-seed lands before
    // the loop below drains them, which is exactly the order the intake
    // contract wants (crate::event_record's doc).
    crate::event_record::boot(&mut app.record, &mut app.intake, &app.home);

    let mut debounce: Option<Instant> = None;
    let mut reconnect_deadline: Option<Instant> = None;
    let mut detached = false;
    let _ = draw(&mut terminal, &mut app);
    while !detached {
        let next_deadline = [debounce, reconnect_deadline, app.folder_probe_at]
            .into_iter()
            .flatten()
            .min();
        match next_wake(&mut rx, next_deadline).await {
            Wake::Message(msg) => {
                if !handle_app_msg(
                    msg,
                    &mut app,
                    &mut client,
                    &mut debounce,
                    &mut reconnect_deadline,
                    &mut detached,
                )
                .await
                {
                    break;
                }
            }
            Wake::Deadline => {
                let now = Instant::now();
                if debounce.is_some_and(|deadline| deadline <= now) {
                    debounce = None;
                    let resize_applied = match apply_live_divider(&mut app, &client).await {
                        Ok(applied) => applied,
                        Err(e) => {
                            log_err(&app.home, &e);
                            false
                        }
                    };
                    if app.needs_reconcile {
                        app.needs_reconcile = false;
                        if let Err(e) = reconcile(&mut app, &client).await {
                            log_err(&app.home, &e);
                        }
                    } else if app.needs_hydrate && !resize_applied {
                        app.needs_hydrate = false;
                        hydrate_visible_tab(&client, app.model.active_tab(), &mut app.runtimes)
                            .await;
                    }
                    // A theme edit, or a `cyclops theme <name>`, applies on
                    // this render. A reload the engine refused leaves the
                    // colors alone and hands back a line for workspace.log:
                    // stderr is under the alternate screen here.
                    if let Some(watch) = theme_watch.as_mut() {
                        refresh_theme_watch(&mut app, watch);
                    }
                    let _ = draw(&mut terminal, &mut app);
                }
                if app.folder_probe_at.is_some_and(|due| due <= now) {
                    app.folder_probe_at = None;
                    if let Err(e) = follow_workspace_folder(&mut app, &client).await {
                        log_err(&app.home, &e);
                    }
                }
                if reconnect_deadline.is_some_and(|deadline| deadline <= now) {
                    reconnect_deadline = None;
                    let _ = handle_reconnect(
                        &mut app,
                        &mut client,
                        &control_cfg,
                        &tx,
                        &mut reconnect_deadline,
                    )
                    .await;
                    let _ = draw(&mut terminal, &mut app);
                }
            }
        }
    }

    drop(terminal);
    drop(guard);
    client.shutdown().await;
    if detached {
        eprintln!("{}", copy::DETACHED);
    } else if app.link_state == LinkState::ServerGone {
        eprintln!("{}", copy::SERVER_GONE_OFFER);
    }
    0
}

/// The render-deadline half of theme hot reload: re-stat the selection,
/// adopt a change into the live paint, log what a refused reload had to
/// say. Skipped whole while the theme picker is open (`App::theme_restore`
/// is `Some`): the preview owns the paint until the picker closes, and a
/// stamp the watch never polled is still a pending change, so the first
/// refresh after the close adopts whatever happened while browsing. That
/// is how the watch resumes ownership unconditionally.
fn refresh_theme_watch(app: &mut App, watch: &mut cyclops_theme::ThemeWatch) {
    if app.theme_restore.is_some() {
        return;
    }
    if watch.refresh() {
        app.paint.theme = watch.theme().clone();
    }
    for warning in watch.take_warnings() {
        log_err(&app.home, &format!("theme: {warning}"));
    }
}

fn spawn_notif_forwarder(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Notification>,
    tx: mpsc::UnboundedSender<AppMsg>,
) {
    tokio::spawn(async move {
        let mut pending = None;
        loop {
            let notification = match pending.take() {
                Some(notification) => notification,
                None => match rx.recv().await {
                    Some(notification) => notification,
                    None => break,
                },
            };
            let notification = match notification {
                Notification::Output { pane, data }
                | Notification::ExtendedOutput { pane, data, .. } => {
                    let mut output = Vec::new();
                    push_output(&mut output, pane, data);
                    while let Ok(next) = rx.try_recv() {
                        match next {
                            Notification::Output { pane, data }
                            | Notification::ExtendedOutput { pane, data, .. } => {
                                push_output(&mut output, pane, data)
                            }
                            other => {
                                pending = Some(other);
                                break;
                            }
                        }
                    }
                    let _ = tx.send(AppMsg::OutputBatch(output));
                    continue;
                }
                other => other,
            };
            match notification {
                Notification::LayoutChange { window, rest } => {
                    let mut fields = rest.split_whitespace();
                    let layout = fields.next().unwrap_or("").to_string();
                    // rest is "layout visible-layout flags"; the flags field
                    // carries the zoom marker.
                    let flags = fields.nth(1).map(str::to_string);
                    let _ = tx.send(AppMsg::LayoutChanged {
                        window,
                        layout,
                        flags,
                    });
                }
                Notification::WindowPaneChanged { window, pane } => {
                    let _ = tx.send(AppMsg::ActivePaneChanged { window, pane });
                }
                Notification::SessionChanged { session, name } => {
                    let _ = tx.send(AppMsg::SessionSwitched { session, name });
                }
                Notification::SessionRenamed { session, name } => {
                    let _ = tx.send(AppMsg::SessionRenamed { session, name });
                }
                Notification::WindowAdd { .. }
                | Notification::WindowClose { .. }
                | Notification::WindowRenamed { .. }
                | Notification::SessionsChanged => {
                    let _ = tx.send(AppMsg::Reconcile);
                }
                Notification::Pause { pane } => {
                    let _ = tx.send(AppMsg::PanePaused { pane });
                }
                Notification::Continue { pane } => {
                    let _ = tx.send(AppMsg::PaneContinued { pane });
                }
                Notification::Exit { .. } => {
                    let _ = tx.send(AppMsg::LinkLost);
                    break;
                }
                _ => {}
            }
        }
    });
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What the blocking reader thread below hands to the coalescing loop: one
/// signal per subscription line that carried an `"event"` field, or one
/// signal when the connection ends.
pub enum DecorationSignal {
    Event,
    Closed,
}

/// Why [`coalesce_decoration_signals`] returned: the daemon connection
/// ended (the caller reports offline), or the refresh sink is gone (the
/// app itself is shutting down, nothing left to tell).
#[derive(Debug, PartialEq, Eq)]
pub enum CoalesceEnd {
    Closed,
    SinkGone,
}

/// The decoration burst rule, runnable on its own: a burst of signals
/// coalesces into ONE `refresh` call on a deadline armed by the burst's
/// FIRST signal — later signals never push it back (rule 9's arm-once
/// shape, `recv_timeout` instead of `tokio::select!` because this loop
/// deliberately lives on a blocking thread). `refresh` returns false when
/// its sink is gone, which ends the loop the way a lost app channel ends
/// the production forwarder.
///
/// Public so the performance contract can drive a measured burst through
/// the real loop (src/cyclops-workspace/tests/perf_contract.rs) instead
/// of a re-implementation that could drift; production behavior is
/// unchanged, `spawn_decoration_forwarder` calls exactly this.
pub fn coalesce_decoration_signals(
    sig_rx: std::sync::mpsc::Receiver<DecorationSignal>,
    debounce: Duration,
    mut refresh: impl FnMut() -> bool,
) -> CoalesceEnd {
    // One-shot deadline armed by the first signal in a burst; `None`
    // means idle, exactly like `debounce` in the main loop.
    let mut deadline: Option<std::time::Instant> = None;
    loop {
        let signal = match deadline {
            None => sig_rx.recv().ok(),
            Some(at) => {
                let wait = at.saturating_duration_since(std::time::Instant::now());
                match sig_rx.recv_timeout(wait) {
                    Ok(signal) => Some(signal),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        deadline = None;
                        if !refresh() {
                            return CoalesceEnd::SinkGone;
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
                }
            }
        };
        match signal {
            Some(DecorationSignal::Event) => {
                // Arm once per burst; a signal that arrives while
                // already armed must never push the deadline back.
                if deadline.is_none() {
                    deadline = Some(std::time::Instant::now() + debounce);
                }
            }
            Some(DecorationSignal::Closed) | None => return CoalesceEnd::Closed,
        }
    }
}

/// Event-driven daemon decoration updates, and (E2) the shared `cyclops
/// watch` stream feed. The subscription itself never polls. A split or a
/// border drag pushes several state/label/delivery events through
/// cyclopsd at once; a dedicated reader thread turns each subscription
/// line into one [`DecorationSignal`] on a plain channel, and
/// [`subscribe_decoration_once`] coalesces a burst of them into ONE status
/// fetch instead of one per event — the same arm-once, never-push-back
/// rule `arm()` and `RENDER_DEBOUNCE` use for rendering
/// (`DECORATION_DEBOUNCE`), just built on
/// `std::sync::mpsc::Receiver::recv_timeout` instead of `tokio::select!`
/// because this connection is deliberately blocking IO on its own thread,
/// away from the input loop.
///
/// That same reader thread also normalizes each event line into a
/// [`cyclops_ui::Entry`] and sends it straight to the app as
/// [`AppMsg::StreamEntry`], bypassing the coalescing above entirely.
/// Feeding the record is in-memory normalization with no IO of its own —
/// unlike a status fetch, there is nothing to debounce — so folding it
/// into the burst logic would only add latency the decoration path does
/// not have today. This is the ONE connection both concerns share; see
/// the module doc and `docs/development/INVARIANTS.md` rule 9.
///
/// The subscription outlives the daemon: [`run_decoration_forwarder`]
/// reconnects on a bounded backoff, so a daemon restart or a boot-order
/// race costs an outage, never the thread.
fn spawn_decoration_forwarder(home: std::path::PathBuf, tx: mpsc::UnboundedSender<AppMsg>) {
    std::thread::spawn(move || {
        run_decoration_forwarder(&home, &tx, resilience::reconnect_delay);
    });
}

/// Why one subscription connection ended, deciding what the reconnect
/// loop does next.
#[derive(Debug, PartialEq, Eq)]
enum SubscribeEnd {
    /// Nothing answered on the socket, or the handshake failed.
    ConnectFailed,
    /// A live subscription ended: the daemon went away mid-run.
    Closed,
    /// The app channel is gone: the workspace itself is shutting down.
    SinkGone,
}

/// The forwarder's whole life: subscribe, forward until the connection
/// ends, reconnect on a bounded backoff, repeat. Unlike the tmux link
/// there is no give-up state: the workspace works without its daemon and
/// the daemon restarts routinely (upgrades, crashes), so the subscription
/// must still be waiting whenever it returns; a dead thread here is a
/// status indicator that only moves on structural reconciles. The backoff
/// sleeps are reconnects only, never state polls (the same bound the
/// daemon's own watcher states); while connected the loop rides
/// subscription events alone, so rule 9 holds.
///
/// Offline is reported once per outage, and only after the chain has
/// failed [`resilience::RECONNECT_CAP`] times: an outage shorter than the
/// chain keeps the last decoration on screen instead of un-naming every
/// agent for a restart the user never noticed (doubt vs. news, the
/// refresh closure's rule). The empty snapshot paints the sidebar's
/// "cyclopsd offline" line and drops the watch record so recovery
/// re-asks; one workspace.log line says the same. `delay_for` is injected
/// so tests can run the chain in milliseconds; production passes
/// [`resilience::reconnect_delay`], which clamps at its last entry.
fn run_decoration_forwarder(
    home: &std::path::Path,
    tx: &mpsc::UnboundedSender<AppMsg>,
    delay_for: impl Fn(usize) -> std::time::Duration,
) {
    let mut attempt = 0usize;
    let mut reported_offline = false;
    loop {
        match subscribe_decoration_once(home, tx) {
            SubscribeEnd::SinkGone => return,
            // A connection existed, so this outage starts its own chain.
            SubscribeEnd::Closed => {
                attempt = 0;
                reported_offline = false;
            }
            SubscribeEnd::ConnectFailed => attempt = attempt.saturating_add(1),
        }
        if !resilience::may_retry(attempt) && !reported_offline {
            reported_offline = true;
            if tx
                .send(AppMsg::DecorationChanged(DecorationSnapshot::default()))
                .is_err()
            {
                return;
            }
            log_err(
                home,
                &format!(
                    "cyclopsd is not answering after {} reconnect attempts; \
                     agent decoration is offline until it returns (still retrying)",
                    resilience::RECONNECT_CAP
                ),
            );
        }
        // The workspace shutting down closes the channel; a retry loop
        // with nobody left to tell stops instead of probing a dead socket.
        if tx.is_closed() {
            return;
        }
        // `attempt` counts completed failures; the sleep indexes the retry
        // about to run, so a cold boot's chain starts at RECONNECT_ATTEMPT_1
        // exactly like a chain that follows a live close.
        std::thread::sleep(delay_for(attempt.saturating_sub(1)));
    }
}

/// One subscription connection, end to end: the Hello-first handshake,
/// the `events.subscribe` request, a resync ask, then the reader thread
/// and the coalescing loop until the connection or the app ends. See
/// [`spawn_decoration_forwarder`] for what the reader and the coalescer
/// each carry.
fn subscribe_decoration_once(
    home: &std::path::Path,
    tx: &mpsc::UnboundedSender<AppMsg>,
) -> SubscribeEnd {
    use std::io::{BufRead, Write};
    let socket = home.join(cyclops_proto::SOCK_NAME);
    let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return SubscribeEnd::ConnectFailed;
    };
    let mut reader = std::io::BufReader::new(stream);
    let mut hello = String::new();
    if reader
        .read_line(&mut hello)
        .ok()
        .filter(|read| *read > 0)
        .is_none()
    {
        return SubscribeEnd::ConnectFailed;
    }
    if reader
        .get_mut()
        .write_all(b"{\"id\":1,\"method\":\"events.subscribe\",\"params\":{}}\n")
        .is_err()
    {
        return SubscribeEnd::ConnectFailed;
    }
    // Subscribed. Ask the app to resync before any event arrives on this
    // connection: everything that changed while nothing was subscribed
    // produced no event, so without this a state flip during the outage
    // stays on screen as stale.
    if tx.send(AppMsg::DaemonReconnected).is_err() {
        return SubscribeEnd::SinkGone;
    }

    let (sig_tx, sig_rx) = std::sync::mpsc::channel::<DecorationSignal>();
    let stream_tx = tx.clone();
    std::thread::spawn(move || loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = sig_tx.send(DecorationSignal::Closed);
                return;
            }
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("event").is_none() {
            continue;
        }
        // E2: normalize this same line onto the shared stream model
        // before the decoration signal below. Cheap (no IO) and
        // per-event on purpose — it must never wait for or extend the
        // coalescing deadline that follows. An unreadable event still
        // reaches the decoration signal; only the record feed skips
        // it (`Entry::from_event` never rejects a KNOWN event, but a
        // daemon ahead of this build can still send a shape that
        // fails to deserialize at all).
        if let Ok(ev) = serde_json::from_value::<cyclops_proto::Event>(value) {
            // A theme reload is not a fact about the record; the CLI
            // stream drops it the same way (`cyclops_ui`'s own
            // subscribe loop). It still becomes a wake-only
            // `ThemeChanged`: the decoration signal below only
            // repaints when a status fetch lands, and a theme switch
            // must repaint either way. Every other vocabulary, known
            // or not, still becomes an entry (unknown kinds render as
            // `Other` rather than being dropped).
            if ev.event != "theme" {
                let entry = cyclops_ui::Entry::from_event(&ev, now_ms());
                if stream_tx
                    .send(AppMsg::StreamEntry(Box::new(entry)))
                    .is_err()
                {
                    return;
                }
            } else if stream_tx.send(AppMsg::ThemeChanged).is_err() {
                return;
            }
        }
        if sig_tx.send(DecorationSignal::Event).is_err() {
            return;
        }
    });

    let end = coalesce_decoration_signals(sig_rx, DECORATION_DEBOUNCE, || {
        // A refused or timed-out status call is doubt about this
        // instant, not news about the roster: the subscription is
        // still up, so the next burst asks again. Dropping it keeps
        // the last known decoration on screen instead of un-naming
        // every agent for a frame. A daemon that is really gone ends
        // the read loop above; whether that becomes "offline" on
        // screen is the reconnect chain's call in
        // [`run_decoration_forwarder`], the one place it is reported.
        match decoration::fetch_decoration(home) {
            Some(snapshot) => tx.send(AppMsg::DecorationChanged(snapshot)).is_ok(),
            None => true,
        }
    });
    match end {
        CoalesceEnd::Closed => SubscribeEnd::Closed,
        CoalesceEnd::SinkGone => SubscribeEnd::SinkGone,
    }
}

/// Coalesce adjacent control-mode output per pane before it reaches the app
/// queue. Pane order is independent; byte order within each pane is not.
fn push_output(output: &mut Vec<(String, Vec<u8>)>, pane: String, bytes: Vec<u8>) {
    if let Some((_, pending)) = output.iter_mut().find(|(id, _)| *id == pane) {
        pending.extend(bytes);
    } else {
        output.push((pane, bytes));
    }
}

fn schedule_reconnect(app: &mut App, reconnect_deadline: &mut Option<Instant>) {
    if !resilience::may_retry(app.reconnect_attempt) {
        app.link_state = LinkState::ServerGone;
        return;
    }
    app.link_state = LinkState::Reconnecting {
        attempt: app.reconnect_attempt,
    };
    let delay = resilience::reconnect_delay(app.reconnect_attempt);
    *reconnect_deadline = Some(Instant::now() + delay);
}

async fn handle_reconnect(
    app: &mut App,
    client: &mut ControlClient,
    cfg: &ControlConfig,
    tx: &mpsc::UnboundedSender<AppMsg>,
    reconnect_deadline: &mut Option<Instant>,
) -> Result<(), cyclops_tmux::TmuxError> {
    client.shutdown().await;
    let cfg = reconnect_config(cfg, &app.model.session.session);
    match ControlClient::spawn(cfg).await {
        Ok((new_client, rx)) => {
            *client = new_client;
            spawn_notif_forwarder(rx, tx.clone());
            app.declared_client_size = None;
            app.pinned_windows.clear();
            resize_client(app, client).await;
            reconcile(app, client).await?;
            app.link_state = LinkState::Live;
            app.reconnect_attempt = 0;
        }
        Err(_) => {
            app.reconnect_attempt += 1;
            if resilience::may_retry(app.reconnect_attempt) {
                schedule_reconnect(app, reconnect_deadline);
            } else {
                app.link_state = LinkState::ServerGone;
                let _ = tx.send(AppMsg::Redraw);
            }
        }
    }
    Ok(())
}

/// The chrome split one frame of `model` under `prefs` composes to.
///
/// The one place chrome geometry is derived, called by boot (to declare the
/// tmux client size) and by every frame (to paint). Both must read the same
/// inputs or the declaration and the paint disagree by the sidebar's width
/// — the collapsed-at-boot case, and the bug class of 626ec09.
fn chrome_for(
    area: Rect,
    model: &WorkspaceModel,
    prefs: &WorkspacePrefs,
) -> crate::render::ChromeAreas {
    crate::render::chrome_areas_for(
        area,
        model.sidebar_visible,
        prefs.sidebar_width.max(crate::render::SIDEBAR_MIN_WIDTH),
        crate::render::tab_bar_visible(model.session.tabs.len()),
    )
}

/// A workspace can switch or rename sessions after boot. Reconnection must
/// follow the model's current target, never the name captured at startup.
fn reconnect_config(base: &ControlConfig, session: &str) -> ControlConfig {
    let mut cfg = base.clone();
    cfg.session = session.to_string();
    cfg
}

impl App {
    fn is_visible_pane(&self, pane: &str) -> bool {
        pane_is_visible(self.model.active_tab(), pane)
    }

    fn chrome(&self, area: Rect) -> crate::render::ChromeAreas {
        chrome_for(area, &self.model, &self.prefs)
    }

    fn persist_active(&self) {
        let tab = self.model.active_tab();
        set_last_active(&self.home, &self.model.session.session, &tab.window_id);
    }

    fn open_menu(&mut self, menu: MenuState) {
        self.menu = menu;
        self.hover = None;
        self.hit_map.clear_menu_items();
    }

    fn close_menu(&mut self) {
        self.menu.close();
        self.hover = None;
        self.hit_map.clear_menu_items();
    }
}

/// Append one error line to `<home>/workspace.log`. The alternate screen
/// owns stderr while the workspace runs; printing there corrupts the frame.
fn log_err(home: &std::path::Path, err: &dyn std::fmt::Display) {
    use std::io::Write;
    let path = home.join("workspace.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{} {err}", chrono_stamp());
    }
}

fn chrono_stamp() -> String {
    // Seconds since the epoch; enough to correlate with daemon logs.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("[{now}]")
}

enum InputOutcome {
    Detached,
    Redraw,
    NoRedraw,
}

async fn resize_client(app: &mut App, client: &ControlClient) {
    let (w, h) = app.term_size;
    let size = crate::render::tmux_client_size(
        app.chrome(Rect::new(0, 0, w, h)).canvas,
        app.model.active_tab(),
    );
    if size.0 < 10 || size.1 < 3 || app.declared_client_size == Some(size) {
        return;
    }
    match client.set_client_size(size.0, size.1).await {
        Ok(()) => app.declared_client_size = Some(size),
        Err(error) => log_err(&app.home, &error),
    }
}

/// The tab windows not yet pinned to the sizing policy, in tab order.
fn unpinned_windows<'a>(tabs: &'a [TabModel], pinned: &HashSet<String>) -> Vec<&'a str> {
    tabs.iter()
        .filter(|tab| !pinned.contains(&tab.window_id))
        .map(|tab| tab.window_id.as_str())
        .collect()
}

/// Pin `window-size smallest` on every window of the displayed session,
/// once per window id. Without the pin, tmux's default `latest` policy lets
/// any other attached client out-size this one, laying panes out wider than
/// the painted canvas (F48). A window that fails stays unpinned, so the
/// next reconcile retries it.
async fn pin_window_sizes(
    client: &ControlClient,
    tabs: &[TabModel],
    pinned: &mut HashSet<String>,
    home: &std::path::Path,
) {
    for window_id in unpinned_windows(tabs, pinned) {
        match client.set_window_size_smallest(window_id).await {
            Ok(()) => {
                pinned.insert(window_id.to_string());
            }
            Err(error) => log_err(home, &error),
        }
    }
}

/// Apply a `%layout-change` notification directly. Returns false when the
/// model cannot absorb it and a full reconcile is needed.
fn apply_layout_change(app: &mut App, window: &str, layout: &str, flags: Option<&str>) -> bool {
    let Ok(node) = crate::layout::parse_layout(layout) else {
        return false;
    };
    let Some(resolved) = crate::layout::resolve_layout(&node, &[]) else {
        return false;
    };
    let Some(zoomed) = flags.map(|f| f.contains('Z')) else {
        // Older tmux without the flags field: zoom state is unknowable
        // from the notification alone.
        return false;
    };
    let Some(tab) = app
        .model
        .session
        .tabs
        .iter_mut()
        .find(|t| t.window_id == window)
    else {
        return false;
    };
    tab.layout = resolved;
    tab.zoomed = zoomed;
    let ids = crate::layout::pane_ids_in_layout(&tab.layout);
    if !ids.iter().any(|id| id == &tab.active_pane) {
        if let Some(first) = ids.first() {
            tab.active_pane = first.clone();
        }
    }
    true
}

/// Whether a rename notification belongs to this control client's session.
/// A missing id is the legacy attached-session-only notification shape.
fn rename_targets_active_session(active: Option<&str>, renamed: Option<&str>) -> bool {
    renamed.is_none_or(|id| Some(id) == active)
}

/// Handle one app message. Returns false when the channel closed.
async fn handle_app_msg(
    msg: Option<AppMsg>,
    app: &mut App,
    client: &mut ControlClient,
    debounce: &mut Option<Instant>,
    reconnect_deadline: &mut Option<Instant>,
    detached: &mut bool,
) -> bool {
    let Some(msg) = msg else {
        return false;
    };
    match msg {
        AppMsg::Redraw => arm(debounce),
        AppMsg::Resized(w, h) => {
            app.term_size = (w, h);
            app.hit_map.clear();
            resize_client(app, client).await;
            arm(debounce);
        }
        AppMsg::Reconcile => {
            app.needs_reconcile = true;
            app.hit_map.clear();
            arm(debounce);
        }
        AppMsg::SessionSwitched { session, name } => {
            if let Some(index) = app
                .model
                .workspaces
                .iter()
                .position(|workspace| workspace.session_id == session)
            {
                app.model.active_workspace = index;
            }
            app.model.session.session = name;
            app.needs_reconcile = true;
            app.hit_map.clear();
            arm(debounce);
        }
        AppMsg::SessionRenamed { session, name } => {
            let renamed_index = session
                .as_deref()
                .and_then(|session_id| {
                    app.model
                        .workspaces
                        .iter()
                        .position(|workspace| workspace.session_id == session_id)
                })
                .or_else(|| session.is_none().then_some(app.model.active_workspace));
            if let Some(index) = renamed_index {
                if let Some(workspace) = app.model.workspaces.get_mut(index) {
                    let old_name = std::mem::replace(&mut workspace.name, name.clone());
                    if persist::migrate_order_entry(
                        &mut app.prefs.workspace_order,
                        &old_name,
                        &name,
                    ) {
                        if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
                            log_err(&app.home, &error);
                        }
                    }
                }
            }
            let active_session = app
                .model
                .workspaces
                .get(app.model.active_workspace)
                .map(|workspace| workspace.session_id.as_str());
            // The legacy one-field notification was documented for the
            // attached session. Current tmux identifies every renamed
            // session, including background ones, by stable id (F37).
            if rename_targets_active_session(active_session, session.as_deref()) {
                app.model.session.session = name;
            }
            app.needs_reconcile = true;
            app.hit_map.clear();
            arm(debounce);
        }
        AppMsg::LayoutChanged {
            window,
            layout,
            flags,
        } => {
            if apply_layout_change(app, &window, &layout, flags.as_deref()) {
                if app.model.active_tab().window_id == window {
                    resize_client(app, client).await;
                    app.needs_hydrate = true;
                    app.hit_map.clear();
                }
            } else {
                app.needs_reconcile = true;
                app.hit_map.clear();
            }
            arm(debounce);
        }
        AppMsg::ActivePaneChanged { window, pane } => {
            let known = app
                .model
                .session
                .tabs
                .iter_mut()
                .find(|t| t.window_id == window)
                .map(|t| t.active_pane = pane)
                .is_some();
            if !known {
                app.needs_reconcile = true;
            }
            arm(debounce);
        }
        AppMsg::OutputBatch(output) => {
            let mut changed = false;
            for (pane, bytes) in output {
                if app.is_visible_pane(&pane) {
                    if let Some(rt) = app.runtimes.get_mut(&pane) {
                        rt.feed(&bytes);
                        changed = true;
                    }
                }
            }
            if changed {
                arm(debounce);
                arm_folder_probe(app);
            }
        }
        AppMsg::LinkLost => {
            app.reconnect_attempt = 0;
            schedule_reconnect(app, reconnect_deadline);
            arm(debounce);
        }
        AppMsg::PanePaused { pane } => {
            app.paused_panes.insert(pane);
            arm(debounce);
        }
        AppMsg::PaneContinued { pane } => {
            if app.paused_panes.remove(&pane) {
                if app.is_visible_pane(&pane) {
                    // Rehydrate: paused output was dropped, continuity is gone.
                    app.runtimes.retain_visible(&[]);
                    app.needs_hydrate = true;
                }
                arm(debounce);
            }
        }
        AppMsg::DecorationChanged(snapshot) => {
            apply_decoration_snapshot(app, snapshot);
            arm(debounce);
        }
        AppMsg::DaemonReconnected => {
            resync_daemon_state(app);
            arm(debounce);
        }
        // E2: per-event, not coalesced with the decoration burst above —
        // feeding the record is cheap and in-memory, so it runs on every
        // event rather than waiting for that debounce (see
        // `spawn_decoration_forwarder`'s doc). `Record::live` also moves
        // the record's own attention register by the one rule in
        // `cyclops_proto::attention`, the same rule `app.decoration`'s
        // register answers to; neither side recomputes it. The intake
        // between here and the record drops ledger-backed entries the
        // boot-time tail already replayed (crate::event_record).
        AppMsg::StreamEntry(entry) => {
            crate::event_record::live(&mut app.record, &mut app.intake, *entry);
            arm(debounce);
        }
        // Wake-only: the reload itself runs on the render deadline this
        // arms (the theme_watch refresh in `run_async`).
        AppMsg::ThemeChanged => arm(debounce),
        AppMsg::Mouse(mouse) => {
            // Bare motion only matters while a menu or dialog shows hover
            // highlights — or over the sidebar's create button, the one
            // piece of resting chrome that answers the mouse. Everywhere
            // else it must not wake the renderer.
            if matches!(mouse.kind, MouseEventKind::Moved)
                && !app.menu.is_open()
                && app.dialog.is_none()
                && !crate::input::mouse::motion_touches_new_workspace_button(
                    &app.hit_map,
                    app.hover,
                    mouse.column,
                    mouse.row,
                )
            {
                return true;
            }
            if matches!(mouse.kind, MouseEventKind::Moved)
                && app.hover == Some((mouse.column, mouse.row))
            {
                return true;
            }
            if let Err(e) = handle_mouse(app, client, mouse, detached).await {
                log_err(&app.home, &e);
            }
            arm(debounce);
        }
        AppMsg::Paste(text) => {
            if app.link_state == LinkState::ServerGone {
                return true;
            }
            if app.menu.is_open() {
                app.close_menu();
                arm(debounce);
                return true;
            }
            if app.dialog.is_some() {
                if dialog::append_dialog_text(app.dialog.as_mut(), &text) {
                    arm(debounce);
                }
                return true;
            }
            if let Err(e) = paste_into_focused_pane(app, client, text.as_bytes()).await {
                log_err(&app.home, &e);
            }
        }
        AppMsg::Input(key) => {
            if app.link_state == LinkState::ServerGone {
                return true;
            }
            if app.menu.is_open() {
                // Any key dismisses an open menu and is consumed by it.
                app.close_menu();
                arm(debounce);
                return true;
            }
            if crate::input::escape_cancels_visual_state(
                key.code,
                app.selection.active.is_some(),
                app.drag.is_some(),
            ) {
                app.selection.cancel_drag();
                cancel_drag(app);
                // Escape belongs to the chrome operation it just cancelled;
                // do not leak it into the child TUI as a second action.
                arm(debounce);
                return true;
            }
            match handle_key(app, client, key).await {
                Ok(InputOutcome::Detached) => *detached = true,
                Ok(InputOutcome::Redraw) => arm(debounce),
                Ok(InputOutcome::NoRedraw) => {}
                Err(e) => log_err(&app.home, &e),
            }
        }
    }
    true
}

fn cancel_drag(app: &mut App) {
    if let Some(drag) = app.drag.take() {
        if let Some(width) = crate::render::sidebar_width_on_cancel(&drag, app.term_size.0) {
            // Sidebar motion is only visual until mouse-up, so Escape can
            // restore the start without a compensating tmux resize.
            app.prefs.sidebar_width = width;
        }
    }
}

/// The minimal live-model context [`action::route_binding`] and
/// [`action::route_menu_item`] need, built fresh for each event so nothing
/// here can go stale.
fn route_context(app: &App) -> action::RouteContext<'_> {
    action::RouteContext {
        tabs: &app.model.session.tabs,
        active_tab: app.model.session.active_tab,
        active_pane: &app.model.active_tab().active_pane,
        session: &app.model.session.session,
        workspaces: &app.model.workspaces,
        active_workspace: app.model.active_workspace,
    }
}

/// Apply the executor's outcome. `redraw` is not one of its fields: every
/// call site below already arms the render debounce unconditionally after
/// dispatching a resolved action (the same thing keyboard, mouse, and dialog
/// handling did before this task), so a redundant flag would just be a
/// field nobody reads. `detach` is read at each call site directly, since
/// the two callers that can reach it (`handle_key`, and the menu-item
/// branch of `handle_mouse`) each have their own way of ending the loop.
fn apply_outcome(app: &mut App, outcome: exec::Outcome) {
    if outcome.persist {
        if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
            log_err(&app.home, &error);
        }
    }
    if outcome.reconcile {
        app.needs_reconcile = true;
    }
}

/// Handle one resolved mouse event. This owns hit-testing, drag-state
/// mechanics, selection, and menu/dialog overlay handling only; every
/// workspace mutation resolves an [`Action`] (via `action`'s routing
/// functions) and runs through [`exec::execute`] — see the module docs.
async fn handle_mouse(
    app: &mut App,
    client: &ControlClient,
    mouse: MouseEvent,
    detached: &mut bool,
) -> Result<(), cyclops_tmux::TmuxError> {
    let col = mouse.column;
    let row = mouse.row;
    // Hover feeds the highlight on menu rows and dialog buttons; the
    // arm-side filter drops motion when neither is open.
    if matches!(mouse.kind, MouseEventKind::Moved) {
        app.hover = Some((col, row));
        return Ok(());
    }
    // An open dialog owns the mouse: its buttons respond, nothing else.
    if app.dialog.is_some() {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            let max_scroll = keybind_scroll_limit(app);
            match app.dialog.as_mut() {
                Some(Dialog::Keybinds { scroll, .. }) => {
                    let delta = if up { -3 } else { 3 };
                    *scroll = dialog::move_keybind_scroll(*scroll, delta, max_scroll);
                }
                // A picker notch moves the selection one row, not a
                // viewport three: the list is what the wheel is over.
                Some(Dialog::Themes {
                    selected, names, ..
                }) => {
                    let delta = if up { -1 } else { 1 };
                    *selected = dialog::move_theme_selection(*selected, delta, names.len());
                }
                _ => {}
            }
            // Same preview the arrow keys make: the row the wheel lands
            // on goes live for the next render.
            exec::preview_selected_theme(app);
            return Ok(());
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            match app.hit_map.hit(col, row) {
                Some(HitTarget::DialogConfirm) => dialog_confirm(app, client).await?,
                Some(HitTarget::DialogCancel) => dialog_cancel(app),
                _ => {}
            }
        }
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if app.menu.is_open() || app.selection.is_dragging() {
                return Ok(());
            }
            if let Some(target) = app.hit_map.hit(col, row).cloned() {
                let direction = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    action::ScrollDirection::Up
                } else {
                    action::ScrollDirection::Down
                };
                if let Some(action) = action::route_mouse_scroll(&target, direction) {
                    let outcome = exec::execute(app, client, action).await?;
                    apply_outcome(app, outcome);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            let Some(target) = app.hit_map.hit(col, row).cloned() else {
                app.close_menu();
                return Ok(());
            };
            match &target {
                HitTarget::PaneBody { pane_id }
                | HitTarget::PaneFrame { pane_id }
                | HitTarget::PaneGrip { pane_id } => {
                    if let Some(action) = action::route_mouse_click(&target, MouseButton::Right) {
                        let outcome = exec::execute(app, client, action).await?;
                        apply_outcome(app, outcome);
                    }
                    app.open_menu(MenuState::ContextMenu {
                        pane_id: pane_id.clone(),
                        at: (col, row),
                    });
                }
                HitTarget::Tab { window_id } => {
                    app.open_menu(MenuState::TabMenu {
                        window_id: window_id.clone(),
                        at: (col, row),
                    });
                }
                HitTarget::SidebarRow { session, .. } => {
                    app.open_menu(MenuState::WorkspaceMenu {
                        session: session.clone(),
                        at: (col, row),
                    });
                }
                HitTarget::SidebarAgent { pane_id, .. } => {
                    if let Some(action) = action::route_mouse_click(&target, MouseButton::Right) {
                        let outcome = exec::execute(app, client, action).await?;
                        apply_outcome(app, outcome);
                    }
                    app.open_menu(MenuState::ContextMenu {
                        pane_id: pane_id.clone(),
                        at: (col, row),
                    });
                }
                _ => app.close_menu(),
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = app.hit_map.hit(col, row).cloned() else {
                app.close_menu();
                app.selection.clear();
                return Ok(());
            };
            match &target {
                HitTarget::MenuItem { action } => {
                    let action = *action;
                    let menu = std::mem::replace(&mut app.menu, MenuState::None);
                    app.hover = None;
                    app.hit_map.clear_menu_items();
                    let resolved = {
                        let ctx = route_context(app);
                        crate::action::route_menu_item(&menu, action, &ctx)
                    };
                    if let Some(resolved) = resolved {
                        let outcome = exec::execute(app, client, resolved).await?;
                        apply_outcome(app, outcome);
                        if outcome.detach {
                            *detached = true;
                        }
                    }
                    return Ok(());
                }
                HitTarget::PaneBody { pane_id } => {
                    app.close_menu();
                    let pane_id = pane_id.clone();
                    let hit = HitTarget::PaneBody {
                        pane_id: pane_id.clone(),
                    };
                    let clicks = app.selection.register_click(&hit, col, row);
                    if let Some(geom) = app.hit_map.pane_geometry(&pane_id) {
                        if let Some(cell) = crate::input::mouse::HitMap::cell_at(geom, col, row) {
                            match clicks {
                                2 => {
                                    if let Some(rt) = app.runtimes.get(&pane_id) {
                                        let row_text = rt.row_text(cell.row);
                                        app.selection.set_word(pane_id.clone(), cell, &row_text);
                                    }
                                    copy_active_selection(app);
                                }
                                3 => {
                                    app.selection.set_line(
                                        pane_id.clone(),
                                        cell.row,
                                        geom.inner.width,
                                    );
                                    copy_active_selection(app);
                                }
                                _ => app.selection.press(pane_id.clone(), cell),
                            }
                        }
                    }
                }
                HitTarget::PaneFrame { .. } => {
                    // A frame click focuses, nothing more; the swap pickup
                    // lives on the corner grip, so the seam cells a frame
                    // shares with a sibling stay resize handles.
                    app.close_menu();
                    app.selection.clear();
                }
                HitTarget::PaneGrip { pane_id } => {
                    app.close_menu();
                    app.selection.clear();
                    // Down on the grip starts a possible swap drag; a
                    // below-threshold release focuses the pane, same as a
                    // frame click. The body is untouched; dragging there
                    // is text selection.
                    app.drag = Some(DragState::on_down(
                        DragTarget::Pane {
                            pane_id: pane_id.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::PaneSplitRight { .. } | HitTarget::PaneSplitDown { .. } => {
                    app.close_menu();
                }
                HitTarget::Divider { pane_id, dir } => {
                    app.selection.clear();
                    app.drag = Some(DragState::on_down(
                        DragTarget::Divider {
                            pane_id: pane_id.clone(),
                            dir: *dir,
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::Tab { window_id } => {
                    app.close_menu();
                    app.selection.clear();
                    // Down starts a possible reorder drag; a below-threshold
                    // release selects the tab instead.
                    app.drag = Some(DragState::on_down(
                        DragTarget::Tab {
                            window_id: window_id.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::NewTabButton
                | HitTarget::NewWorkspaceButton
                | HitTarget::SidebarTab { .. }
                | HitTarget::AttentionIndicator { .. } => {
                    app.close_menu();
                }
                HitTarget::SidebarRow {
                    session_id,
                    session,
                } => {
                    app.close_menu();
                    app.selection.clear();
                    app.drag = Some(DragState::on_down(
                        DragTarget::Workspace {
                            session_id: session_id.clone(),
                            session: session.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::SidebarDisclosure { session_id } => {
                    toggle_workspace_expanded(&mut app.expanded_workspaces, session_id.clone());
                    return Ok(());
                }
                HitTarget::SidebarAgent {
                    workspace_id,
                    pane_id,
                    order_key,
                } => {
                    app.close_menu();
                    app.selection.clear();
                    app.drag = Some(DragState::on_down(
                        DragTarget::Agent {
                            workspace_id: workspace_id.clone(),
                            pane_id: pane_id.clone(),
                            order_key: order_key.clone(),
                        },
                        col,
                        row,
                    ));
                    return Ok(());
                }
                HitTarget::SidebarDivider => {
                    app.close_menu();
                    app.selection.clear();
                    app.drag = Some(DragState::on_down(DragTarget::Sidebar, col, row));
                    return Ok(());
                }
                HitTarget::AppMenu => {
                    if app.menu == MenuState::AppMenu {
                        app.close_menu();
                    } else {
                        app.open_menu(MenuState::AppMenu);
                    }
                    return Ok(());
                }
                HitTarget::DialogConfirm | HitTarget::DialogCancel => return Ok(()),
            }
            if let Some(action) = action::route_mouse_click(&target, MouseButton::Left) {
                let outcome = exec::execute(app, client, action).await?;
                apply_outcome(app, outcome);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.drag.is_some() {
                if let Some(drag) = app.drag.as_mut() {
                    drag.on_move(col, row);
                }
                if app.drag.as_ref().is_some_and(|drag| {
                    drag.is_active() && matches!(&drag.target, DragTarget::Sidebar)
                }) {
                    app.prefs.sidebar_width =
                        crate::render::sidebar_width_for_column(col, app.term_size.0);
                }
            } else if let Some(anchor) = app.selection.anchor_pane().map(str::to_string) {
                if let Some(geom) = app.hit_map.pane_geometry(&anchor) {
                    if let Some(cell) = crate::input::mouse::HitMap::cell_at(geom, col, row) {
                        app.selection.drag_to(&anchor, cell);
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(drag) = app.drag.as_mut() {
                drag.on_move(col, row);
            }
            let sidebar_drag = app.drag.as_ref().is_some_and(|drag| {
                drag.is_active() && matches!(&drag.target, DragTarget::Sidebar)
            });
            if sidebar_drag {
                app.prefs.sidebar_width =
                    crate::render::sidebar_width_for_column(col, app.term_size.0);
                if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
                    log_err(&app.home, &error);
                }
                resize_client(app, client).await;
            }
            apply_live_divider(app, client).await?;
            if let Some(drag) = app.drag.take() {
                let crossed_threshold = drag.on_up().is_some();
                if crossed_threshold {
                    commit_drag_drop(app, client, &drag.target, col, row).await?;
                } else if let Some(action) = action::route_drag_click(&drag.target) {
                    let outcome = exec::execute(app, client, action).await?;
                    apply_outcome(app, outcome);
                }
            } else if app.selection.is_dragging() {
                if let Some(sel) = app.selection.finish_drag() {
                    if let Some(rt) = app.runtimes.get_mut(&sel.pane_id) {
                        if let Some(text) = selection::SelectionState::extract(rt, &sel) {
                            selection::copy_to_clipboard(&text);
                        }
                    }
                }
            } else {
                let _ = app.selection.finish_drag();
            }
        }
        _ => {}
    }
    Ok(())
}

/// Commit a drag that crossed its move threshold: resolve the drop hit
/// target under the release point through `action`'s routing, and execute
/// whatever it resolves to. Only [`DragTarget::Pane`], `Tab`, `Workspace`,
/// and `Agent` resolve here: a divider applies live during motion
/// ([`apply_live_divider`]) and a sidebar-width drag already applied and
/// persisted above.
///
/// `Workspace` is the one variant that does NOT resolve through a dropped-
/// on hit target: [`resolve_workspace_slot_drop`] recomputes the exact
/// slot [`crate::drag::slot_for_row`] would preview at the release point,
/// so the drop always matches the live insertion rule the user watched
/// track their pointer — never a second, possibly disagreeing, resolution.
async fn commit_drag_drop(
    app: &mut App,
    client: &ControlClient,
    picked_up: &DragTarget,
    col: u16,
    row: u16,
) -> Result<(), cyclops_tmux::TmuxError> {
    let drop = app.hit_map.hit(col, row).cloned();
    let action = match picked_up {
        DragTarget::Pane { pane_id } => {
            drop.and_then(|drop| action::resolve_pane_drop(pane_id, &drop))
        }
        DragTarget::Tab { window_id } => {
            drop.and_then(|drop| action::resolve_tab_drop(window_id, &drop))
        }
        DragTarget::Workspace { session_id, .. } => {
            let sidebar = app
                .chrome(Rect::new(0, 0, app.term_size.0, app.term_size.1))
                .sidebar;
            resolve_workspace_slot_drop(&app.hit_map, sidebar, session_id, col, row)
        }
        DragTarget::Agent {
            workspace_id,
            order_key,
            ..
        } => {
            let order = agent_order_for_workspace(app, workspace_id);
            drop.and_then(|drop| action::resolve_agent_drop(workspace_id, order_key, &drop, &order))
        }
        DragTarget::Divider { .. } | DragTarget::Sidebar => None,
    };
    let Some(action) = action else {
        return Ok(());
    };
    let outcome = exec::execute(app, client, action).await?;
    apply_outcome(app, outcome);
    Ok(())
}

/// Resolve a workspace-row drag release into the exact [`Action`] its live
/// preview showed. `None` — leave the order exactly as it was, dispatch
/// nothing — covers every case that is not a real move:
///
/// - the release point is outside the sidebar entirely (no rule was
///   showing there to honor);
/// - the sidebar is not visible at all (`sidebar` is `None`);
/// - the previewed slot is one of the two boundaries touching the dragged
///   row's own position (dropping it back where it started); or
/// - the dragged workspace has vanished from the model mid-drag (closed by
///   another client) — a stale drop, not a move.
///
/// This is a pure function of the last painted frame's hit rects and the
/// release point — no tmux call, nothing async — so both the rule's
/// destination and this function answer the identical question the
/// identical way; see [`crate::drag::slot_for_row`] and
/// [`crate::drag::insertion_for_slot`].
fn resolve_workspace_slot_drop(
    hit_map: &crate::input::mouse::HitMap,
    sidebar: Option<Rect>,
    session_id: &str,
    col: u16,
    row: u16,
) -> Option<action::Action> {
    let sidebar = sidebar?;
    if !sidebar.contains(ratatui::layout::Position::from((col, row))) {
        return None;
    }
    let blocks = hit_map.workspace_blocks();
    let slot = crate::drag::slot_for_row(&blocks, row);
    let insertion = crate::drag::insertion_for_slot(&blocks, session_id, slot)?;
    Some(action::Action::ReorderWorkspace {
        session_id: session_id.to_string(),
        insertion,
    })
}

/// The sidebar agent order for one workspace, keyed the same way
/// [`crate::decoration::DecorationSnapshot::agent_order_key`] does — the
/// context [`action::resolve_agent_drop`] needs to turn a drop into a
/// stable-id insertion.
fn agent_order_for_workspace(app: &App, workspace_id: &str) -> Vec<String> {
    app.model
        .workspaces
        .iter()
        .find(|workspace| workspace.session_id == workspace_id)
        .map(|workspace| {
            app.decoration
                .agent_rows_for_window_ids(&workspace.window_ids, &app.prefs.agent_order)
                .into_iter()
                .map(DecorationSnapshot::agent_order_key)
                .collect()
        })
        .unwrap_or_default()
}

fn copy_active_selection(app: &mut App) {
    let Some(sel) = app.selection.active.clone() else {
        return;
    };
    if let Some(rt) = app.runtimes.get_mut(&sel.pane_id) {
        if let Some(text) = selection::SelectionState::extract(rt, &sel) {
            selection::copy_to_clipboard(&text);
        }
    }
}

/// The divider drag's pending motion since the last applied step, if any:
/// `(pane_id, axis, signed delta)`. A read-only helper so the borrow it
/// takes on `app.drag` ends before [`apply_live_divider`] needs `&mut App`
/// to run the executor.
fn pending_divider_resize(app: &App) -> Option<(String, SplitDir, i32)> {
    let drag = app.drag.as_ref()?;
    if !drag.is_active() {
        return None;
    }
    let DragTarget::Divider { pane_id, dir } = drag.target.clone() else {
        return None;
    };
    let delta = match dir {
        SplitDir::Horizontal => drag.current.0 as i32 - drag.last_applied.0 as i32,
        SplitDir::Vertical => drag.current.1 as i32 - drag.last_applied.1 as i32,
    };
    Some((pane_id, dir, delta))
}

/// Apply divider motion since the last applied step as a resize-pane call.
/// tmux's `%layout-change` answers reconcile the model — the drag itself
/// never writes geometry.
async fn apply_live_divider(
    app: &mut App,
    client: &ControlClient,
) -> Result<bool, cyclops_tmux::TmuxError> {
    let Some((pane_id, axis, delta)) = pending_divider_resize(app) else {
        return Ok(false);
    };
    let Some(action) = action::resolve_pane_resize(&pane_id, axis, delta) else {
        return Ok(false);
    };
    exec::execute(app, client, action).await?;
    if let Some(drag) = app.drag.as_mut() {
        drag.last_applied = drag.current;
    }
    Ok(true)
}

/// Arm one delayed folder probe, if the active workspace follows its folder
/// and none is armed already.
///
/// Never pushes an armed probe back — the same rule [`arm`] follows for the
/// render deadline, and for the same reason: a pane producing a steady
/// stream of output would otherwise postpone the probe forever. Workspaces
/// that do not follow a folder arm nothing, so they add no wakeups.
fn arm_folder_probe(app: &mut App) {
    if app.folder_probe_at.is_some() {
        return;
    }
    let follows = app
        .model
        .workspaces
        .get(app.model.active_workspace)
        .is_some_and(|workspace| app.prefs.folder_tracked.contains(&workspace.session_id));
    if follows {
        app.folder_probe_at = Some(Instant::now() + FOLDER_PROBE_DELAY);
    }
}

/// Keep a folder-following workspace's name on its pane's directory.
///
/// A `cd` produces no tmux notification, so there is nothing to subscribe
/// to; what a `cd` DOES produce is pane output, and pane output is what
/// arms the render deadline. Riding that deadline means the check happens
/// exactly when the screen changed and never on a clock, rate-limited so a
/// noisy pane cannot turn every frame into a tmux round trip.
async fn follow_workspace_folder(
    app: &mut App,
    client: &ControlClient,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(workspace) = app.model.workspaces.get(app.model.active_workspace) else {
        return Ok(());
    };
    if !app.prefs.folder_tracked.contains(&workspace.session_id) {
        return Ok(());
    }
    let session_id = workspace.session_id.clone();
    let current_name = workspace.name.clone();

    let pane = app.model.active_tab().active_pane.clone();
    let Ok(cwd) = client.display(&pane, "#{pane_current_path}").await else {
        // A transient tmux failure here must never be fatal — the next
        // probe tries again.
        return Ok(());
    };

    let taken: Vec<String> = app
        .model
        .workspaces
        .iter()
        .filter(|w| w.session_id != session_id)
        .map(|w| w.name.clone())
        .collect();
    let Some(next) = naming::folder_rename(&current_name, cwd.trim(), &taken) else {
        return Ok(());
    };

    client.rename_session(&current_name, &next).await?;

    // The model addresses the active session BY NAME; skip this and the
    // next reconcile queries a session that no longer exists. The sidebar
    // row carries that same name, and the reconcile that refreshes it is a
    // deadline away — leaving the stale name there would let the next probe
    // rename a target tmux no longer knows.
    if app.model.session.session == current_name {
        app.model.session.session = next.clone();
    }
    if let Some(row) = app.model.workspaces.get_mut(app.model.active_workspace) {
        row.name = next.clone();
    }
    if persist::migrate_order_entry(&mut app.prefs.workspace_order, &current_name, &next) {
        if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
            log_err(&app.home, &error);
        }
    }
    app.needs_reconcile = true;
    Ok(())
}

/// Apply the open dialog's action (Enter or its confirm button).
/// Route the open dialog's confirmation to a terminal [`Action`] and run it
/// through the executor. A dialog whose buffer resolves to nothing (a blank
/// rename, or the read-only Keybinds sheet) still dismisses on Enter — it
/// just does so here instead of inside an executor arm, since there is no
/// action to hand it.
async fn dialog_confirm(
    app: &mut App,
    client: &ControlClient,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(dialog) = app.dialog.clone() else {
        return Ok(());
    };
    let Some(action) = action::route_dialog_confirm(&dialog) else {
        // Dismissing without an action is a cancel: same close path, so a
        // theme picker with nothing to apply also restores its paint.
        dialog_cancel(app);
        return Ok(());
    };
    let outcome = exec::execute(app, client, action).await?;
    apply_outcome(app, outcome);
    Ok(())
}

fn dialog_cancel(app: &mut App) {
    // Close-without-apply: the theme picker previews over the live paint,
    // so the theme that was live when it opened goes back. `None` for
    // every other dialog, and for an applied picker (the apply drops it).
    if let Some(theme) = app.theme_restore.take() {
        app.paint.theme = theme;
    }
    app.dialog = None;
    app.hover = None;
}

/// Paste one outer-terminal bracketed paste into the focused pane in two
/// tmux commands, regardless of payload length. On failure after load, the
/// server-global buffer is removed best-effort so pasted text cannot linger.
async fn paste_into_focused_pane(
    app: &mut App,
    client: &ControlClient,
    bytes: &[u8],
) -> Result<(), cyclops_tmux::TmuxError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let pane_id = app.model.active_tab().active_pane.clone();
    let buffer = format!("cyclops-workspace-{}-{}", std::process::id(), app.paste_seq);
    app.paste_seq = app.paste_seq.checked_add(1).ok_or_else(|| {
        cyclops_tmux::TmuxError::Protocol("workspace paste sequence overflow".into())
    })?;
    client.load_buffer(&buffer, bytes).await?;
    if let Err(error) = client.paste_buffer(&buffer, &pane_id, true, true).await {
        let _ = client.delete_buffer(&buffer).await;
        return Err(error);
    }
    Ok(())
}

fn keybind_scroll_limit(app: &App) -> u16 {
    let row_count = match app.dialog.as_ref() {
        Some(Dialog::Keybinds { rows, .. }) => rows.len(),
        _ => return 0,
    };
    crate::render::keybind_max_scroll(row_count, Rect::new(0, 0, app.term_size.0, app.term_size.1))
}

/// Install one decoration snapshot: the offline bookkeeping and the
/// agent-order migration every arrival path shares (the forwarder's
/// coalesced refresh, its offline report, and the reconnect resync).
fn apply_decoration_snapshot(app: &mut App, snapshot: DecorationSnapshot) {
    // A daemon that went away forgot every session it was asked to
    // watch: those live in memory, not in config.toml. Dropping the
    // record here is what makes the next reconcile ask again.
    if !snapshot.online {
        app.watched_sessions.clear();
    }
    if persist::migrate_agent_order_entries(&mut app.prefs.agent_order, &app.decoration, &snapshot)
    {
        if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
            log_err(&app.home, &error);
        }
    }
    app.decoration = snapshot;
}

/// The daemon subscription (re)connected: pull back what the outage lost
/// on both sides, without waiting for a structural reconcile. The watch
/// record is forgotten first because a restarted daemon has no pane table
/// for UI-created sessions until each is re-asked (asking for a session
/// it already watches returns the existing slot, so a reconnect that was
/// only a socket blip costs nothing); the asks land before the fetch so
/// the snapshot already covers them. The fetch then replaces the whole
/// snapshot, because a state that flipped while nothing was subscribed
/// produced no event and would otherwise stay on screen as stale.
fn resync_daemon_state(app: &mut App) {
    app.watched_sessions.clear();
    ensure_sessions_watched(app);
    if let Some(snapshot) = decoration::fetch_decoration(&app.home) {
        apply_decoration_snapshot(app, snapshot);
    }
}

async fn reconcile(app: &mut App, client: &ControlClient) -> Result<(), cyclops_tmux::TmuxError> {
    let session = app.model.session.session.clone();
    let mut model = fetch_workspace_model(client, &session).await?;
    apply_workspace_order(&mut model, &app.prefs.workspace_order);
    install_reconciled_model(&mut app.model, model, app.prefs.sidebar_visible);
    expand_active_workspace(
        &app.model.workspaces,
        app.model.active_workspace,
        &mut app.expanded_for,
        &mut app.expanded_workspaces,
    );
    // Before the snapshot, not after: a session the daemon starts watching
    // now is one this same reconcile can already show agents for.
    ensure_sessions_watched(app);
    // Keep what the last answer said when this one does not arrive: a
    // reconcile that cannot reach the daemon knows nothing new about the
    // roster, and blanking it would un-name every agent on screen.
    if let Some(snapshot) = decoration::fetch_decoration(&app.home) {
        app.decoration = snapshot;
    }
    app.persist_active();
    // New windows arrive through this snapshot (new tab, session switch,
    // external new-window); pin them before sizing so no displayed window
    // ever lays out under another client's authority.
    pin_window_sizes(
        client,
        &app.model.session.tabs,
        &mut app.pinned_windows,
        &app.home,
    )
    .await;
    resize_client(app, client).await;
    hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await;
    app.needs_hydrate = false;
    Ok(())
}

/// Make sure cyclopsd is watching every workspace on screen.
///
/// The sidebar's whole agent hierarchy comes from the daemon's pane table,
/// and the daemon only builds one for sessions it watches. Workspaces this
/// UI creates are not in config.toml, so without this their rows would show
/// nothing no matter what was running in them.
///
/// One ask per session id, ever. See [`crate::daemon::watch_session`] for
/// why a rename must not produce a second ask.
fn ensure_sessions_watched(app: &mut App) {
    for workspace in &app.model.workspaces {
        if app.watched_sessions.contains(&workspace.session_id) {
            continue;
        }
        match crate::daemon::watch_session(&app.home, &workspace.name) {
            Ok(()) => {
                app.watched_sessions.insert(workspace.session_id.clone());
            }
            // A daemon that is down is a normal state for this UI: the rest
            // of the workspace works without it. Leaving the id unrecorded
            // is what makes the next reconcile try again.
            Err(error) => log_err(&app.home, &error),
        }
    }
}

/// Open the active workspace in the sidebar when it becomes the active one.
///
/// Agent rows are children of an expanded workspace, so a workspace you just
/// created or switched to would otherwise hide the agents it was opened for.
/// Only the CHANGE opens it: re-expanding on every reconcile would leave the
/// disclosure triangle unable to close the row you are actually looking at.
fn expand_active_workspace(
    workspaces: &[crate::model::WorkspaceRow],
    active: usize,
    expanded_for: &mut Option<String>,
    expanded: &mut HashSet<String>,
) {
    let Some(row) = workspaces.get(active) else {
        return;
    };
    if expanded_for.as_deref() == Some(row.session_id.as_str()) {
        return;
    }
    *expanded_for = Some(row.session_id.clone());
    expanded.insert(row.session_id.clone());
}

/// Apply the persisted visual order without changing tmux session identity.
/// Rows absent from the preference append in tmux's deterministic order.
fn apply_workspace_order(model: &mut WorkspaceModel, order: &[String]) {
    let active_id = model
        .workspaces
        .get(model.active_workspace)
        .map(|workspace| workspace.session_id.clone());
    let mut remaining = std::mem::take(&mut model.workspaces);
    let mut ordered = Vec::with_capacity(remaining.len());
    for name in order {
        if let Some(index) = remaining
            .iter()
            .position(|workspace| &workspace.name == name)
        {
            ordered.push(remaining.remove(index));
        }
    }
    ordered.extend(remaining);
    model.workspaces = ordered;
    model.active_workspace = active_id
        .and_then(|id| {
            model
                .workspaces
                .iter()
                .position(|workspace| workspace.session_id == id)
        })
        .or_else(|| {
            model
                .workspaces
                .iter()
                .position(|workspace| workspace.name == model.session.session)
        })
        .unwrap_or(0);
}

/// Replace local structure with one authoritative tmux snapshot while
/// retaining only UI-owned preferences. In particular, the snapshot's
/// active window wins: preserving the old window when it still exists
/// would leave a newly created or externally selected tab invisible.
fn install_reconciled_model(
    current: &mut WorkspaceModel,
    mut fresh: WorkspaceModel,
    sidebar_visible: bool,
) {
    fresh.sidebar_visible = sidebar_visible;
    *current = fresh;
}

/// Route one key to an [`Action`] (via [`action::route_binding`]) and run
/// it through the executor. Key passthrough to the focused pane is NOT an
/// action and stays on its own fast path below, unconfirmed and untouched.
async fn handle_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
) -> Result<InputOutcome, cyclops_tmux::TmuxError> {
    if app.dialog.is_some() {
        return handle_dialog_key(app, client, key).await;
    }
    match app.router.route(key) {
        RouterResult::PrefixArmed => Ok(InputOutcome::NoRedraw),
        RouterResult::Consumed => Ok(InputOutcome::NoRedraw),
        RouterResult::Action(binding) => {
            let resolved = {
                let ctx = route_context(app);
                // Shift on the chord upgrades directional focus to a pane
                // swap (Ctrl+B Shift+Arrow). Only a prefix chord can carry
                // the upgrade: the router matches prefix chords by key
                // code alone, so Shift is unread information there, while
                // a direct chord matched its modifiers exactly and an
                // explicit shift+arrow focus binding must stay focus.
                let prefix_shift = key.modifiers.contains(KeyModifiers::SHIFT)
                    && matches!(
                        app.router.chord(binding),
                        Some(crate::bindings::BindingChord::Prefix(_))
                    );
                if prefix_shift {
                    action::route_binding_shifted(binding, &ctx)
                } else {
                    action::route_binding(binding, &ctx)
                }
            };
            let Some(resolved) = resolved else {
                return Ok(InputOutcome::NoRedraw);
            };
            let outcome = exec::execute(app, client, resolved).await?;
            apply_outcome(app, outcome);
            Ok(if outcome.detach {
                InputOutcome::Detached
            } else {
                InputOutcome::Redraw
            })
        }
        RouterResult::PassThrough(key) => {
            let pane = app.model.active_tab().active_pane.clone();
            let encoded = encode_send_keys(&key);
            if !encoded.is_empty() {
                let keys: Vec<&str> = encoded.iter().map(String::as_str).collect();
                client.send_keys_unconfirmed(&pane, &keys).await?;
            }
            Ok(InputOutcome::NoRedraw)
        }
    }
}

async fn handle_dialog_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
) -> Result<InputOutcome, cyclops_tmux::TmuxError> {
    let Some(dialog) = app.dialog.as_ref() else {
        return Ok(InputOutcome::NoRedraw);
    };
    let action = dialog::dialog_key_action(dialog, &key);
    let max_scroll = keybind_scroll_limit(app);
    match action {
        DialogKeyAction::Cancel => {
            dialog_cancel(app);
            if key.code == crossterm::event::KeyCode::Esc {
                app.selection.cancel_drag();
                cancel_drag(app);
            }
        }
        DialogKeyAction::Confirm => dialog_confirm(app, client).await?,
        DialogKeyAction::Backspace => {
            if let Some(buffer) = app.dialog.as_mut().and_then(dialog::dialog_buffer_mut) {
                buffer.pop();
            }
            if let Some(Dialog::NamePane { error, .. }) = app.dialog.as_mut() {
                *error = None;
            }
        }
        DialogKeyAction::Append(c) => {
            let mut encoded = [0; 4];
            dialog::append_dialog_text(app.dialog.as_mut(), c.encode_utf8(&mut encoded));
        }
        DialogKeyAction::Scroll(delta) => {
            match app.dialog.as_mut() {
                Some(Dialog::Keybinds { scroll, .. }) => {
                    *scroll = dialog::move_keybind_scroll(*scroll, delta, max_scroll);
                }
                Some(Dialog::Themes {
                    selected, names, ..
                }) => {
                    *selected = dialog::move_theme_selection(*selected, delta, names.len());
                }
                _ => {}
            }
            // The row the arrows land on goes live for the next render
            // (a no-op for every other dialog).
            exec::preview_selected_theme(app);
        }
        DialogKeyAction::ScrollStart => {
            if let Some(Dialog::Keybinds { scroll, .. }) = app.dialog.as_mut() {
                *scroll = 0;
            }
        }
        DialogKeyAction::ScrollEnd => {
            if let Some(Dialog::Keybinds { scroll, .. }) = app.dialog.as_mut() {
                *scroll = max_scroll;
            }
        }
        DialogKeyAction::Ignore => {}
    }
    Ok(if action == DialogKeyAction::Ignore {
        InputOutcome::NoRedraw
    } else {
        InputOutcome::Redraw
    })
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    app.hit_map.clear();
    let mut shown_cursor: Option<crate::render::HostCursor> = None;
    terminal
        .draw(|f| {
            let areas = app.chrome(f.area());
            if let Some(sidebar) = areas.sidebar {
                paint_sidebar(
                    &app.model.workspaces,
                    app.model.active_workspace,
                    &app.model.active_tab().active_pane,
                    &app.expanded_workspaces,
                    &app.prefs.agent_order,
                    app.sidebar_tab,
                    &app.record,
                    sidebar,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    &app.decoration,
                    app.hover,
                    app.drag.as_ref(),
                );
            }
            paint_tab_bar(
                &app.model.session.tabs,
                app.model.session.active_tab,
                areas.tab_bar,
                f.buffer_mut(),
                &app.paint,
                &mut app.hit_map,
                &app.decoration,
            );
            let tab = app.model.active_tab();
            let mut ctx = crate::render::WindowPaintCtx {
                link: app.link_state,
                paused: &app.paused_panes,
                hits: &mut app.hit_map,
                decoration: &app.decoration,
                selection: app.selection.active.as_ref(),
                drag: app.drag.as_ref(),
                cursor: None,
            };
            paint_window(
                tab,
                &app.runtimes,
                areas.canvas,
                f.buffer_mut(),
                &app.paint,
                &mut ctx,
            );
            let cursor = ctx.cursor;
            // Menus paint after panes so their hit regions shadow them.
            paint_menu(
                &app.menu,
                f.area(),
                f.buffer_mut(),
                &app.paint,
                &mut app.hit_map,
                app.hover,
            );
            if let Some(dialog) = &app.dialog {
                paint_dialog(
                    dialog,
                    f.area(),
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    app.hover,
                );
            } else if !app.menu.is_open() {
                if let Some(hc) = cursor {
                    f.set_cursor_position((hc.x, hc.y));
                    shown_cursor = Some(hc);
                }
            }
        })
        .map(|_| ())?;
    // DECSCUSR is terminal state, not frame content: Ratatui diffs cells
    // and knows nothing of cursor shape, so the pane's requested shape is
    // emitted here, once per change rather than once per frame. While the
    // cursor is hidden its shape cannot show, so nothing is emitted and
    // the last emission stands until a visible cursor differs.
    if let Some(hc) = shown_cursor {
        let style = (hc.shape, hc.blink);
        if app.cursor_style != Some(style) {
            crate::term_guard::apply_cursor_style(hc.shape, hc.blink);
            app.cursor_style = Some(style);
        }
    }
    Ok(())
}

/// Sync entry: run the async workspace loop.
pub fn run() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("can't start workspace runtime: {e}");
            return 1;
        }
    };
    rt.block_on(run_async())
}

/// Non-tty entry: print help hint and exit 0.
pub fn print_help_and_exit() -> i32 {
    println!("{}", copy::HELP_HINT);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `App` for tests that only need model/prefs/drag state, not
    /// a live pane runtime — mirrors `exec::tests::test_app`, which is
    /// private to that module and so cannot be reused directly here.
    fn test_app(model: WorkspaceModel, home: std::path::PathBuf) -> App {
        App {
            model,
            runtimes: RuntimeRegistry::default(),
            router: Router::new(crate::bindings::default_bindings()),
            paint: Paint::for_test(),
            dialog: None,
            theme_restore: None,
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
            sidebar_tab: SidebarTab::default(),
            record: cyclops_ui::Record::new(),
            intake: cyclops_ui::Intake::new(),
            cursor_style: None,
            term_size: (40, 12),
            declared_client_size: None,
            pinned_windows: HashSet::new(),
            needs_reconcile: false,
            needs_hydrate: false,
            paste_seq: 0,
            home,
            folder_probe_at: None,
        }
    }

    /// A one-pane model for tests that need an `App` but never reach
    /// tmux, so the ids are inert.
    fn one_pane_model() -> WorkspaceModel {
        let tab = crate::model::TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
        };
        WorkspaceModel {
            workspaces: vec![crate::model::WorkspaceRow {
                session_id: "$0".into(),
                name: "s".into(),
                tab_count: 1,
                window_ids: vec!["@0".into()],
            }],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "s".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
        }
    }

    #[test]
    fn help_exits_zero_message() {
        assert_eq!(print_help_and_exit(), 0);
    }

    #[test]
    fn windows_pin_once_and_a_failed_pin_stays_eligible() {
        let tab = |id: &str| crate::model::TabModel {
            window_id: id.into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
        };
        let tabs = vec![tab("@0"), tab("@1")];
        let mut pinned = HashSet::new();
        assert_eq!(unpinned_windows(&tabs, &pinned), vec!["@0", "@1"]);
        // Only a recorded pin drops out; a window whose pin failed is not
        // recorded and stays eligible for the next reconcile.
        pinned.insert("@0".to_string());
        assert_eq!(unpinned_windows(&tabs, &pinned), vec!["@1"]);
        pinned.insert("@1".to_string());
        assert!(unpinned_windows(&tabs, &pinned).is_empty());
    }

    #[test]
    fn empty_server_boots_a_fresh_default_session() {
        let (session, create) = boot_target(&persist::ReopenTarget::OfferCreate);
        assert_eq!(session, copy::DEFAULT_SESSION_NAME);
        assert!(create, "nothing to attach to must create, not exit");
    }

    #[test]
    fn existing_session_attaches_without_creating() {
        let reopen = persist::ReopenTarget::First("proj".into());
        assert_eq!(boot_target(&reopen), ("proj".into(), false));
    }

    #[test]
    fn arm_never_pushes_a_pending_deadline_back() {
        let mut debounce = None;
        arm(&mut debounce);
        let first = debounce.expect("armed");
        // A later event must not extend the pending deadline — that shape
        // starves rendering under a steady event stream.
        arm(&mut debounce);
        assert_eq!(debounce, Some(first));
    }

    #[tokio::test]
    async fn due_render_deadline_beats_a_ready_message_queue() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(AppMsg::Redraw).expect("queue stays open");
        let due = Instant::now() - Duration::from_millis(1);

        assert!(matches!(
            next_wake(&mut rx, Some(due)).await,
            Wake::Deadline
        ));
    }

    /// L1: a burst of daemon events must collapse to exactly one status
    /// fetch, the same coalescing guarantee `arm()`/`RENDER_DEBOUNCE` give
    /// rendering. A fake cyclopsd accepts the forwarder's persistent
    /// `events.subscribe` connection, pushes five event lines back to back
    /// with no delay (a split or a border drag's burst), then counts every
    /// LATER connection — each one is exactly one coalesced status fetch.
    #[test]
    fn a_burst_of_decoration_events_produces_one_refresh() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let home = cyclops_proto::scratch::scratch_dir("workspace-decoration-burst");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener =
            UnixListener::bind(home.join(cyclops_proto::SOCK_NAME)).expect("bind fake daemon");

        let fetches = Arc::new(AtomicUsize::new(0));
        let fetches_for_server = fetches.clone();
        std::thread::spawn(move || {
            // 1. The persistent subscribe connection: hello, read the
            //    subscribe request, then a burst of five event lines with
            //    no delay between them.
            let (stream, _) = listener.accept().expect("subscribe connection");
            let mut reader = std::io::BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("subscribe request");
            for _ in 0..5 {
                let _ = reader
                    .get_mut()
                    .write_all(b"{\"event\":{\"kind\":\"state\"}}\n");
            }

            // 2. Every later connection is one coalesced status fetch:
            //    answer it and count it.
            for stream in listener.incoming().flatten() {
                fetches_for_server.fetch_add(1, Ordering::SeqCst);
                let mut reader = std::io::BufReader::new(stream);
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
                        "sessions": [],
                    },
                });
                let mut out = serde_json::to_vec(&body).expect("encode status");
                out.push(b'\n');
                let _ = reader.get_mut().write_all(&out);
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_decoration_forwarder(home.clone(), tx);

        // 3. Wait for the coalesced refresh to land, then confirm it stays
        //    at exactly one — bounded polling in test code only (rule 9's
        //    documented exception), never in the forwarder itself.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while fetches.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(DECORATION_DEBOUNCE * 5);
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "a burst of five daemon events must collapse to exactly one status fetch"
        );

        let mut changed = 0;
        while let Ok(msg) = rx.try_recv() {
            if matches!(msg, AppMsg::DecorationChanged(_)) {
                changed += 1;
            }
        }
        assert_eq!(
            changed, 1,
            "exactly one DecorationChanged should reach the app per coalesced burst"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A daemon "theme" event must reach the app as the wake-only
    /// `ThemeChanged`, never as a stream entry: the record mirrors
    /// `cyclops watch`, which drops theme switches the same way.
    #[test]
    fn a_theme_event_wakes_the_app_without_entering_the_record() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixListener;

        let home = cyclops_proto::scratch::scratch_dir("workspace-theme-event");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener =
            UnixListener::bind(home.join(cyclops_proto::SOCK_NAME)).expect("bind fake daemon");

        std::thread::spawn(move || {
            // The persistent subscribe connection: hello, read the
            // subscribe request, then one theme event. Later connections
            // (the coalesced status fetch) are dropped unanswered; the
            // refresh closure treats that as doubt and sends nothing.
            let (stream, _) = listener.accept().expect("subscribe connection");
            let mut reader = std::io::BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("subscribe request");
            let _ = reader
                .get_mut()
                .write_all(b"{\"event\":\"theme\",\"data\":{\"name\":\"aurora\"}}\n");
            for stream in listener.incoming().flatten() {
                drop(stream);
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_decoration_forwarder(home.clone(), tx);

        // Bounded polling in test code only (rule 9's documented
        // exception), never in the forwarder itself.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut woke = 0;
        let mut entries = 0;
        while woke == 0 && std::time::Instant::now() < deadline {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    AppMsg::ThemeChanged => woke += 1,
                    AppMsg::StreamEntry(_) => entries += 1,
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(woke, 1, "a theme event must wake the app exactly once");
        assert_eq!(entries, 0, "a theme event must never become a stream entry");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A fake cyclopsd for subscription-lifecycle tests: serves any
    /// number of sequential connections on the home socket, unlike the
    /// single-connection fakes above. `events.subscribe` connections are
    /// held open ([`FakeDaemon::push_event`] writes to them); `status`
    /// answers with one working agent in pane %0 of session "s";
    /// `session.watch` answers ok and records the asked name. Dropping
    /// the handle is a daemon death: the socket file goes away and every
    /// held subscription closes.
    struct FakeDaemon {
        home: std::path::PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        subs: std::sync::Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>>,
        watches: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeDaemon {
        fn spawn(home: &std::path::Path) -> FakeDaemon {
            use std::io::{BufRead, Write};
            use std::os::unix::net::UnixListener;
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};

            let socket = home.join(cyclops_proto::SOCK_NAME);
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).expect("bind fake daemon");
            // Nonblocking accept so a stopped daemon's thread can notice
            // the stop flag; bounded test-side polling only (rule 9's
            // documented exception).
            listener.set_nonblocking(true).expect("nonblocking accept");
            let stop = Arc::new(AtomicBool::new(false));
            let subs = Arc::new(Mutex::new(Vec::new()));
            let watches = Arc::new(Mutex::new(Vec::new()));
            let stop_bg = Arc::clone(&stop);
            let subs_bg = Arc::clone(&subs);
            let watches_bg = Arc::clone(&watches);
            std::thread::spawn(move || {
                while !stop_bg.load(Ordering::SeqCst) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                    };
                    // BSD-derived accepts can inherit nonblocking; every
                    // connection below is plain blocking line IO.
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    let mut reader = std::io::BufReader::new(stream);
                    if reader
                        .get_mut()
                        .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                        .is_err()
                    {
                        continue;
                    }
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    match request["method"].as_str() {
                        Some("events.subscribe") => {
                            let mut stream = reader.into_inner();
                            let _ = stream.write_all(b"{\"id\":1,\"result\":{}}\n");
                            subs_bg.lock().expect("subs").push(stream);
                        }
                        Some("session.watch") => {
                            if let Some(name) = request["params"]["session"].as_str() {
                                watches_bg.lock().expect("watches").push(name.to_string());
                            }
                            let _ = reader.get_mut().write_all(b"{\"id\":1,\"result\":{}}\n");
                        }
                        Some("status") => {
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
                                            "pane_id": "%0",
                                            "window_id": "@0",
                                            "window_name": "1",
                                            "agent": "reviewer",
                                            "title": "",
                                            "current_command": "sh",
                                            "dead": false,
                                            "in_mode": false,
                                            "width": 80,
                                            "height": 24,
                                            "state": "working",
                                        }],
                                    }],
                                },
                            });
                            let mut out = serde_json::to_vec(&body).expect("encode status");
                            out.push(b'\n');
                            let _ = reader.get_mut().write_all(&out);
                        }
                        _ => {}
                    }
                }
            });
            FakeDaemon {
                home: home.to_path_buf(),
                stop,
                subs,
                watches,
            }
        }

        fn push_event(&self, line: &[u8]) {
            use std::io::Write;
            for stream in self.subs.lock().expect("subs").iter_mut() {
                let _ = stream.write_all(line);
            }
        }

        fn watched(&self) -> Vec<String> {
            self.watches.lock().expect("watches").clone()
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;
            self.stop.store(true, Ordering::SeqCst);
            // What a daemon death looks like from outside: connects start
            // failing and the live subscriptions EOF.
            let _ = std::fs::remove_file(self.home.join(cyclops_proto::SOCK_NAME));
            self.subs.lock().expect("subs").clear();
        }
    }

    /// Bounded wait for one matching app message. Test-side polling only
    /// (rule 9's documented exception); the forwarder itself stays
    /// event-armed.
    fn wait_for_msg(
        rx: &mut mpsc::UnboundedReceiver<AppMsg>,
        what: &str,
        mut matching: impl FnMut(&AppMsg) -> bool,
    ) -> AppMsg {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(msg) if matching(&msg) => return msg,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("gave up waiting for {what}");
    }

    /// The regression the reconnect loop exists for: the one-shot
    /// forwarder died silently on a boot-order race or a daemon restart,
    /// and decoration then refreshed only inside structural reconciles
    /// (the stale status indicator the operator alt-tabbed to fix). The
    /// subscription must survive both a daemon that is not up yet and a
    /// daemon that dies and returns, and events on the new connection
    /// must still become refreshes.
    #[test]
    fn the_decoration_subscription_survives_boot_races_and_daemon_restarts() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-dec-restart");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let home_bg = home.clone();
        // Millisecond backoff so the whole lifecycle runs in test time;
        // production wires resilience::reconnect_delay.
        std::thread::spawn(move || {
            run_decoration_forwarder(&home_bg, &tx, |_| Duration::from_millis(5));
        });

        // Boot race: nothing is listening yet. The forwarder must still
        // be trying, not dead, when the daemon finally binds.
        std::thread::sleep(Duration::from_millis(40));
        let daemon = FakeDaemon::spawn(&home);
        wait_for_msg(&mut rx, "the resync ask after the boot race", |msg| {
            matches!(msg, AppMsg::DaemonReconnected)
        });

        // Restart: the live connection drops; before the fix the thread
        // ended here, permanently.
        drop(daemon);
        let daemon = FakeDaemon::spawn(&home);
        wait_for_msg(&mut rx, "the resync ask after the restart", |msg| {
            matches!(msg, AppMsg::DaemonReconnected)
        });

        // A state event on the new subscription still becomes a coalesced
        // online refresh. The push can race the daemon registering the
        // fresh subscription, so it is re-sent on a bounded schedule;
        // one landing is proof.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut refreshed = false;
        'push: while std::time::Instant::now() < deadline {
            daemon.push_event(b"{\"event\":{\"kind\":\"state\"}}\n");
            let pause = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < pause {
                match rx.try_recv() {
                    Ok(AppMsg::DecorationChanged(snapshot)) if snapshot.online => {
                        refreshed = true;
                        break 'push;
                    }
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        }
        assert!(
            refreshed,
            "a state event after the restart must still produce a fresh online refresh"
        );

        drop(daemon);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A daemon that stays gone is reported once per outage: after the
    /// backoff chain fails `RECONNECT_CAP` times the app gets one empty
    /// offline snapshot (the sidebar's "cyclopsd offline" line) and
    /// workspace.log says why, while the loop keeps retrying quietly. A
    /// daemon that finally answers is resynced like any reconnect, so
    /// the report is a state, not a surrender.
    #[test]
    fn a_daemon_that_stays_gone_is_reported_offline_once_then_recovered() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-dec-offline");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let home_bg = home.clone();
        std::thread::spawn(move || {
            run_decoration_forwarder(&home_bg, &tx, |_| Duration::from_millis(2));
        });

        let offline = wait_for_msg(&mut rx, "the offline report", |msg| {
            matches!(msg, AppMsg::DecorationChanged(_))
        });
        let AppMsg::DecorationChanged(snapshot) = offline else {
            unreachable!("wait_for_msg matched DecorationChanged");
        };
        assert!(!snapshot.online, "the report is the empty offline snapshot");
        let log_deadline = std::time::Instant::now() + Duration::from_secs(2);
        let log_path = home.join("workspace.log");
        while std::time::Instant::now() < log_deadline
            && !std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .contains("cyclopsd is not answering")
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .contains("cyclopsd is not answering"),
            "the outage must leave one honest line in workspace.log"
        );

        // Once per outage: many more failed chain turns add nothing.
        std::thread::sleep(Duration::from_millis(60));
        while let Ok(msg) = rx.try_recv() {
            assert!(
                !matches!(msg, AppMsg::DecorationChanged(_)),
                "offline must be reported once, not once per retry"
            );
        }

        // The chain never gave up: a daemon that appears now is found.
        let daemon = FakeDaemon::spawn(&home);
        wait_for_msg(&mut rx, "the resync ask after recovery", |msg| {
            matches!(msg, AppMsg::DaemonReconnected)
        });

        drop(daemon);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// What the resync ask does when it lands: the stale watch record is
    /// dropped and re-asked (a restarted daemon has no watch table), and
    /// the snapshot is replaced whole, so a state that flipped while
    /// nothing was subscribed cannot stay on screen as its old word.
    #[test]
    fn a_reconnect_resync_reasks_watches_and_replaces_the_snapshot() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-dec-resync");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let daemon = FakeDaemon::spawn(&home);

        let mut app = test_app(one_pane_model(), home.clone());
        // Watched under the daemon that died, and showing its last word.
        app.watched_sessions.insert("$0".into());
        app.decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        app.decoration.panes.insert(
            "%0".into(),
            crate::decoration::PaneDecoration {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: None,
                manifest_display_name: None,
                state: cyclops_proto::AgentState::Idle,
                needs_attention: false,
            },
        );

        resync_daemon_state(&mut app);

        assert_eq!(
            daemon.watched(),
            vec!["s".to_string()],
            "the on-screen workspace must be re-asked"
        );
        assert!(
            app.watched_sessions.contains("$0"),
            "the fresh ask is recorded again"
        );
        assert!(app.decoration.online);
        assert_eq!(
            app.decoration.pane("%0").expect("the resynced agent").state,
            cyclops_proto::AgentState::Working,
            "the state change the outage swallowed must be on screen"
        );

        drop(daemon);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Esc (or the Cancel button: both land in `dialog_cancel`) puts back
    /// the paint that was live when the theme picker opened.
    #[test]
    fn closing_the_picker_without_applying_restores_the_original_paint() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-picker-restore");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("themes")).expect("themes dir");
        std::fs::write(
            home.join("themes/solar.toml"),
            "name = \"solar\"\n[surface]\ndim = \"#222222\"\n",
        )
        .expect("write theme");
        let mut app = test_app(one_pane_model(), home.clone());
        let dim = cyclops_theme::tokens::SURFACE_DIM;
        let original = app.paint.theme.resolve(dim).rgb;
        // What ShowThemes does: keep the live paint beside the picker.
        app.theme_restore = Some(app.paint.theme.clone());
        app.dialog = Some(Dialog::Themes {
            names: vec!["solar".into()],
            selected: 0,
            active: None,
            notice: None,
        });
        exec::preview_selected_theme(&mut app);
        assert_ne!(app.paint.theme.resolve(dim).rgb, original, "previewed");

        dialog_cancel(&mut app);

        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            original,
            "cancel restores the paint the picker opened over"
        );
        assert!(app.dialog.is_none());
        assert!(
            app.theme_restore.is_none(),
            "the watch owns the paint again"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The interleaving trap: the ThemeWatch refresh runs on the render
    /// deadline, BEFORE draw, so a theme change landing while the picker
    /// previews would repaint over the preview. While the picker is open
    /// the refresh is skipped whole; the stamps it never polled are still
    /// pending changes, so the first refresh after close adopts them and
    /// the watch owns the paint again.
    #[test]
    fn a_watch_refresh_never_overwrites_an_open_preview() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-preview-watch");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("themes")).expect("themes dir");
        std::fs::write(
            home.join("themes/dark.toml"),
            "[surface]\ndim = \"#111111\"\n",
        )
        .expect("write dark");
        std::fs::write(
            home.join("themes/solar.toml"),
            "[surface]\ndim = \"#222222\"\n",
        )
        .expect("write solar");
        std::fs::write(home.join("config.toml"), "theme = \"dark\"\n").expect("config");
        let mut watch = cyclops_theme::ThemeWatch::with_env(None, &home);
        let mut app = test_app(one_pane_model(), home.clone());
        app.paint.theme = watch.theme().clone();
        let dim = cyclops_theme::tokens::SURFACE_DIM;

        app.theme_restore = Some(app.paint.theme.clone());
        app.dialog = Some(Dialog::Themes {
            names: vec!["dark".into(), "solar".into()],
            selected: 1,
            active: Some(0),
            notice: None,
        });
        exec::preview_selected_theme(&mut app);
        assert_eq!(app.paint.theme.resolve(dim).rgb, (0x22, 0x22, 0x22));

        // The watched file moves mid-browse (longer, so the stamp moves
        // regardless of mtime granularity). The deadline refresh must not
        // repaint.
        std::fs::write(
            home.join("themes/dark.toml"),
            "name = \"dark\"\n[surface]\ndim = \"#333333\"\n",
        )
        .expect("edit dark");
        refresh_theme_watch(&mut app, &mut watch);
        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            (0x22, 0x22, 0x22),
            "the preview survives the refresh"
        );

        // Close without applying: the original comes back first, then the
        // watch resumes ownership and adopts the edit it was held off from.
        dialog_cancel(&mut app);
        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            (0x11, 0x11, 0x11),
            "Esc restores what was live at open"
        );
        refresh_theme_watch(&mut app, &mut watch);
        assert_eq!(
            app.paint.theme.resolve(dim).rgb,
            (0x33, 0x33, 0x33),
            "the first refresh after close adopts what happened while browsing"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn adjacent_output_batches_preserve_each_panes_byte_order() {
        let mut output = Vec::new();
        push_output(&mut output, "%0".into(), b"ab".to_vec());
        push_output(&mut output, "%1".into(), b"x".to_vec());
        push_output(&mut output, "%0".into(), b"cd".to_vec());

        assert_eq!(
            output,
            vec![
                ("%0".to_string(), b"abcd".to_vec()),
                ("%1".to_string(), b"x".to_vec())
            ]
        );
    }

    #[test]
    fn background_session_rename_does_not_switch_the_attached_session() {
        assert!(!rename_targets_active_session(Some("$0"), Some("$1")));
        assert!(rename_targets_active_session(Some("$0"), Some("$0")));
        assert!(rename_targets_active_session(Some("$0"), None));
    }

    #[test]
    fn reconnect_uses_the_workspace_selected_after_boot() {
        let boot = ControlConfig::attach("alpha").on_socket("test-socket");
        let reconnect = reconnect_config(&boot, "beta");

        assert_eq!(reconnect.session, "beta");
        assert_eq!(reconnect.socket_name, boot.socket_name);
    }

    /// Boot's declared size and the first painted frame read one geometry
    /// from one pair of inputs, so a workspace quit collapsed reopens
    /// collapsed with no disagreement between what tmux was told and what
    /// is on screen. The open case is measured too: without it the
    /// equality below would hold even if visibility were ignored.
    #[test]
    fn a_collapsed_sidebar_declares_the_geometry_the_first_frame_paints() {
        let area = Rect::new(0, 0, 200, 50);
        let prefs = WorkspacePrefs {
            sidebar_visible: false,
            ..WorkspacePrefs::default()
        };

        // What `run_async` does before it declares: the persisted
        // visibility lands on the model, then boot sizes from the model.
        let mut model = one_pane_model();
        model.sidebar_visible = prefs.sidebar_visible;
        let boot = chrome_for(area, &model, &prefs);
        let declared = crate::render::tmux_client_size(boot.canvas, model.active_tab());

        let mut app = test_app(
            model,
            cyclops_proto::scratch::scratch_dir("app-boot-collapsed"),
        );
        app.prefs = prefs;
        let painted = app.chrome(area);

        assert_eq!(painted.sidebar, None, "prefs said collapsed");
        assert_eq!(painted.canvas, boot.canvas);
        assert_eq!(
            crate::render::tmux_client_size(painted.canvas, app.model.active_tab()),
            declared,
            "the first frame must paint the canvas boot declared"
        );

        app.model.sidebar_visible = true;
        app.prefs.sidebar_visible = true;
        let open = app.chrome(area);
        assert_eq!(open.sidebar, Some(Rect::new(0, 0, 22, 50)));
        assert_ne!(
            crate::render::tmux_client_size(open.canvas, app.model.active_tab()),
            declared,
            "an open sidebar really is a different declaration"
        );
    }

    #[test]
    fn workspace_disclosure_click_toggles_both_directions() {
        let mut expanded = HashSet::new();
        assert!(toggle_workspace_expanded(&mut expanded, "$0".into()));
        assert!(expanded.contains("$0"));
        assert!(!toggle_workspace_expanded(&mut expanded, "$0".into()));
        assert!(!expanded.contains("$0"));
    }

    // -- Cancelling a workspace-row drag (Escape, or any other
    // `cancel_drag` path) must leave the model order and prefs exactly as
    // they were: `cancel_drag` only ever restores the sidebar WIDTH for a
    // `DragTarget::Sidebar` drag, and a `Workspace` drag never touched the
    // model to begin with — nothing here is undone because nothing was
    // ever applied while the drag was live. --
    #[test]
    fn cancelling_a_workspace_reorder_drag_leaves_order_and_prefs_untouched() {
        let row = |id: &str, name: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: name.into(),
            tab_count: 1,
            window_ids: Vec::new(),
        };
        let tab = crate::model::TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
        };
        let model = WorkspaceModel {
            workspaces: vec![row("$a", "a"), row("$b", "b"), row("$c", "c")],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "a".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
        };
        let mut app = test_app(
            model,
            cyclops_proto::scratch::scratch_dir("cancel-workspace-drag"),
        );
        let orders_before = (
            app.model
                .workspaces
                .iter()
                .map(|w| w.session_id.clone())
                .collect::<Vec<_>>(),
            app.prefs.workspace_order.clone(),
        );
        let mut drag = DragState::on_down(
            DragTarget::Workspace {
                session_id: "$c".into(),
                session: "c".into(),
            },
            5,
            5,
        );
        drag.on_move(5, 3);
        assert!(drag.is_active(), "past the 1-cell sidebar row threshold");
        app.drag = Some(drag);

        cancel_drag(&mut app);

        assert!(app.drag.is_none(), "cancel always clears the drag");
        assert_eq!(
            (
                app.model
                    .workspaces
                    .iter()
                    .map(|w| w.session_id.clone())
                    .collect::<Vec<_>>(),
                app.prefs.workspace_order.clone(),
            ),
            orders_before,
            "cancelling must not reorder the model or touch prefs"
        );
    }

    #[test]
    fn reconciled_model_follows_tmuxs_new_active_window() {
        let tab = |id: &str, pane: &str| crate::model::TabModel {
            window_id: id.into(),
            name: id.into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: pane.into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: pane.into(),
            zoomed: false,
        };
        let mut current = WorkspaceModel {
            workspaces: vec![],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "alpha".into(),
                tabs: vec![tab("@1", "%1")],
                active_tab: 0,
            },
            sidebar_visible: true,
        };
        let fresh = WorkspaceModel {
            workspaces: vec![],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "alpha".into(),
                tabs: vec![tab("@1", "%1"), tab("@2", "%2")],
                active_tab: 1,
            },
            sidebar_visible: true,
        };

        install_reconciled_model(&mut current, fresh, false);

        assert_eq!(current.active_tab().window_id, "@2");
        assert!(!current.sidebar_visible);
    }

    #[test]
    fn persisted_workspace_order_keeps_the_active_identity() {
        let tab = crate::model::TabModel {
            window_id: "@1".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%1".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%1".into(),
            zoomed: false,
        };
        let row = |id: &str, name: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: name.into(),
            tab_count: 1,
            window_ids: vec!["@1".into()],
        };
        let mut model = WorkspaceModel {
            workspaces: vec![row("$0", "alpha"), row("$1", "beta")],
            active_workspace: 1,
            session: crate::model::SessionModel {
                session: "beta".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
        };

        apply_workspace_order(&mut model, &["beta".into(), "alpha".into()]);
        assert_eq!(model.workspaces[0].name, "beta");
        assert_eq!(model.active_workspace, 0);
        assert_eq!(model.workspaces[model.active_workspace].session_id, "$1");
    }

    #[test]
    fn switching_workspaces_opens_the_new_one_but_respects_a_manual_collapse() {
        let row = |id: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: id.into(),
            tab_count: 1,
            window_ids: vec!["@1".into()],
        };
        let rows = vec![row("$0"), row("$1")];
        let mut expanded_for = None;
        let mut expanded = HashSet::new();

        expand_active_workspace(&rows, 0, &mut expanded_for, &mut expanded);
        assert!(expanded.contains("$0"), "the active workspace opens");

        // Collapsing the row you are looking at has to stick, so a reconcile
        // that does not change the active workspace must not reopen it.
        toggle_workspace_expanded(&mut expanded, "$0".into());
        expand_active_workspace(&rows, 0, &mut expanded_for, &mut expanded);
        assert!(!expanded.contains("$0"), "a manual collapse survives");

        // Switching is a change, and the workspace you switch to has to show
        // its agents without a second click.
        expand_active_workspace(&rows, 1, &mut expanded_for, &mut expanded);
        assert!(expanded.contains("$1"), "the workspace switched to opens");
    }

    // -- `resolve_workspace_slot_drop`: the drop must match exactly what
    // the live rule would have previewed at the same point, computed the
    // same way (`crate::drag::slot_for_row` + `insertion_for_slot`), never
    // re-derived from whatever hit target happens to sit under the
    // release. --

    fn sidebar_rows(rows: &[(&str, &str, u16)]) -> HitMap {
        let mut hits = HitMap::default();
        for (session_id, session, y) in rows {
            hits.push(
                Rect::new(2, *y, 18, 1),
                HitTarget::SidebarRow {
                    session_id: session_id.to_string(),
                    session: session.to_string(),
                },
            );
        }
        hits
    }

    #[test]
    fn a_valid_drop_resolves_the_previewed_insertion() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4), ("$c", "c", 5)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        // $c, picked up and released on $a's row, previews "before $a" —
        // the same slot a pointer at row 3 would have shown all along.
        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$c", 5, 3),
            Some(action::Action::ReorderWorkspace {
                session_id: "$c".into(),
                insertion: action::Insertion::Before("$a".into()),
            })
        );
    }

    #[test]
    fn a_release_outside_the_sidebar_resolves_nothing() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        // Column 30 is past the sidebar's right edge: released over the
        // pane canvas, not the sidebar, even though the row lines up with
        // a workspace row.
        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$a", 30, 3),
            None
        );
        // No sidebar painted at all (hidden).
        assert_eq!(resolve_workspace_slot_drop(&hits, None, "$a", 5, 3), None);
    }

    #[test]
    fn a_drop_that_does_not_move_the_row_resolves_nothing() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        // Releasing $a back on its own row previews "before $a" — its own
        // position, not a move.
        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$a", 5, 3),
            None
        );
    }

    #[test]
    fn a_stale_drop_against_a_vanished_workspace_resolves_nothing() {
        let hits = sidebar_rows(&[("$a", "a", 3), ("$b", "b", 4)]);
        let sidebar = Some(Rect::new(0, 0, 22, 10));

        assert_eq!(
            resolve_workspace_slot_drop(&hits, sidebar, "$gone", 5, 3),
            None
        );
    }

    // -- End to end through `commit_drag_drop`: a valid drop dispatches
    // exactly one `ReorderWorkspace` and persists exactly once. Executor
    // tests (`app::exec::tests`) already cover what the dispatched action
    // itself does; this proves the resolution-to-dispatch wiring. --

    #[tokio::test]
    async fn a_valid_workspace_drop_dispatches_one_reorder_and_persists_once() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-workspace-drop");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");

        let home = cyclops_proto::scratch::scratch_dir("app-workspace-drop-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");

        let row = |id: &str, name: &str| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: name.into(),
            tab_count: 1,
            window_ids: Vec::new(),
        };
        let tab = crate::model::TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: crate::layout::ResolvedLayout::Leaf {
                pane_id: "%0".into(),
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
            active_pane: "%0".into(),
            zoomed: false,
        };
        let model = WorkspaceModel {
            workspaces: vec![row("$a", "a"), row("$b", "b"), row("$c", "c")],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "a".into(),
                tabs: vec![tab],
                active_tab: 0,
            },
            sidebar_visible: true,
        };
        let mut app = test_app(model, home.clone());

        // Fixture rects standing in for "the last frame actually painted" —
        // narrower than proving `paint_sidebar` itself (that's render.rs's
        // job), just enough to exercise `commit_drag_drop`'s wiring.
        let sidebar = app
            .chrome(Rect::new(0, 0, app.term_size.0, app.term_size.1))
            .sidebar
            .expect("sidebar visible");
        app.hit_map.push(
            Rect::new(sidebar.x + 2, 3, 10, 1),
            HitTarget::SidebarRow {
                session_id: "$a".into(),
                session: "a".into(),
            },
        );
        app.hit_map.push(
            Rect::new(sidebar.x + 2, 4, 10, 1),
            HitTarget::SidebarRow {
                session_id: "$b".into(),
                session: "b".into(),
            },
        );
        app.hit_map.push(
            Rect::new(sidebar.x + 2, 5, 10, 1),
            HitTarget::SidebarRow {
                session_id: "$c".into(),
                session: "c".into(),
            },
        );
        assert!(
            sidebar.contains(ratatui::layout::Position::from((sidebar.x + 2, 3))),
            "fixture rows must sit inside the real sidebar rect"
        );

        // $c is picked up and released on $a's row.
        commit_drag_drop(
            &mut app,
            &client,
            &DragTarget::Workspace {
                session_id: "$c".into(),
                session: "c".into(),
            },
            sidebar.x + 2,
            3,
        )
        .await
        .expect("commit");

        assert_eq!(
            app.model
                .workspaces
                .iter()
                .map(|w| w.session_id.clone())
                .collect::<Vec<_>>(),
            vec!["$c".to_string(), "$a".to_string(), "$b".to_string()],
        );
        assert_eq!(
            app.prefs.workspace_order,
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
        let reloaded = crate::persist::load_prefs(&home);
        assert_eq!(
            reloaded.workspace_order, app.prefs.workspace_order,
            "the promised persist must have actually round-tripped"
        );

        let _ = std::fs::remove_dir_all(&home);
        client.shutdown().await;
    }

    // -- A pane-grip drag released on another pane dispatches one swap
    // through the executor. What the swap itself does to tmux is
    // `app::exec::tests`' job; this proves the resolution-to-dispatch
    // wiring against the painted hit rects. --

    #[tokio::test]
    async fn a_pane_drop_on_another_pane_dispatches_one_swap() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-pane-drop");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "s"]);
        let out = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_left}"]);
        let before: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut fields = line.split_whitespace();
                (
                    fields.next().unwrap_or_default().to_string(),
                    fields.next().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let (dragged, target) = (before[0].0.clone(), before[1].0.clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");

        let model = WorkspaceModel {
            workspaces: vec![crate::model::WorkspaceRow {
                session_id: "$0".into(),
                name: "s".into(),
                tab_count: 1,
                window_ids: vec!["@0".into()],
            }],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "s".into(),
                tabs: vec![crate::model::TabModel {
                    window_id: "@0".into(),
                    name: "1".into(),
                    layout: crate::layout::ResolvedLayout::Leaf {
                        pane_id: dragged.clone(),
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    active_pane: dragged.clone(),
                    zoomed: false,
                }],
                active_tab: 0,
            },
            sidebar_visible: true,
        };
        let mut app = test_app(
            model,
            cyclops_proto::scratch::scratch_dir("app-pane-drop-home"),
        );

        // Fixture rects standing in for the last painted frame: the dragged
        // pane on the left, the drop target's body on the right.
        app.hit_map.push(
            Rect::new(0, 2, 10, 5),
            HitTarget::PaneBody {
                pane_id: dragged.clone(),
            },
        );
        app.hit_map.push(
            Rect::new(12, 2, 10, 5),
            HitTarget::PaneBody {
                pane_id: target.clone(),
            },
        );

        commit_drag_drop(
            &mut app,
            &client,
            &DragTarget::Pane {
                pane_id: dragged.clone(),
            },
            13,
            3,
        )
        .await
        .expect("commit");

        assert!(app.needs_reconcile, "a swap is structural");
        let out = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_left}"]);
        let after = String::from_utf8_lossy(&out.stdout);
        assert!(
            after
                .lines()
                .any(|line| line == format!("{dragged} {}", before[1].1)),
            "the dragged pane must take the target's slot, got {after}"
        );
        assert!(
            after
                .lines()
                .any(|line| line == format!("{target} {}", before[0].1)),
            "the target pane must take the dragged pane's slot, got {after}"
        );
        client.shutdown().await;
    }

    // -- The corner grip owns the swap pickup; every other frame cell is a
    // focus click; the stacked seam resizes. The hit map comes from a real
    // `paint_window` pass so these tests press the exact cells a user
    // sees. --

    /// A stacked two-pane app (top over bottom, the bottom labeled like an
    /// adopted agent pane) whose hit map the real render pass painted at
    /// the 40x12 test terminal size: pane rects (1,1,38,4) and (1,7,38,3),
    /// seam rows 5 and 6 with the bottom pane's title strip on row 6, and
    /// corner grips at (39,5) and (39,10).
    fn stacked_app_with_painted_hits(top: &str, bottom: &str, home: std::path::PathBuf) -> App {
        use crate::decoration::PaneDecoration;

        let leaf = |pane_id: &str, y: u16, height: u16| crate::layout::ResolvedLayout::Leaf {
            pane_id: pane_id.into(),
            x: 0,
            y,
            width: 38,
            height,
        };
        let model = WorkspaceModel {
            workspaces: vec![crate::model::WorkspaceRow {
                session_id: "$0".into(),
                name: "s".into(),
                tab_count: 1,
                window_ids: vec!["@0".into()],
            }],
            active_workspace: 0,
            session: crate::model::SessionModel {
                session: "s".into(),
                tabs: vec![crate::model::TabModel {
                    window_id: "@0".into(),
                    name: "1".into(),
                    layout: crate::layout::ResolvedLayout::Split {
                        dir: SplitDir::Vertical,
                        x: 0,
                        y: 0,
                        width: 38,
                        height: 8,
                        children: vec![leaf(top, 0, 4), leaf(bottom, 5, 3)],
                    },
                    active_pane: bottom.into(),
                    zoomed: false,
                }],
                active_tab: 0,
            },
            sidebar_visible: true,
        };
        let mut app = test_app(model, home);
        app.decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        app.decoration.panes.insert(
            bottom.into(),
            PaneDecoration {
                pane_id: bottom.into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: None,
                manifest_display_name: None,
                state: cyclops_proto::AgentState::Idle,
                needs_attention: false,
            },
        );

        let tab = app.model.active_tab().clone();
        let backend = ratatui::backend::TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| {
            let mut ctx = crate::render::WindowPaintCtx {
                link: LinkState::Live,
                paused: &app.paused_panes,
                hits: &mut app.hit_map,
                decoration: &app.decoration,
                selection: None,
                drag: None,
                cursor: None,
            };
            paint_window(
                &tab,
                &app.runtimes,
                frame.area(),
                frame.buffer_mut(),
                &app.paint,
                &mut ctx,
            );
        })
        .unwrap();
        app
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    /// The drag-grip regression: a drag starting on the seam between a top
    /// and bottom pane must resize the panes and never swap them, even
    /// with the bottom pane labeled. This is the grab the operator
    /// reported as "just grabs and does not resize".
    #[tokio::test]
    async fn a_drag_from_the_stacked_seam_resizes_and_never_swaps() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-seam-resize");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let heights = |server: &TmuxServer| -> Vec<(String, u16)> {
            String::from_utf8_lossy(
                &server
                    .run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_height}"])
                    .stdout,
            )
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.to_string(), fields.next()?.parse().ok()?))
            })
            .collect()
        };
        let before = heights(&server);
        assert_eq!(before.len(), 2);
        let (top, bottom) = (before[0].0.clone(), before[1].0.clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let mut app = stacked_app_with_painted_hits(
            &top,
            &bottom,
            cyclops_proto::scratch::scratch_dir("app-seam-resize-home"),
        );
        let mut detached = false;

        // Row A of the seam, on the columns the title strip shadowed.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 5),
            &mut detached,
        )
        .await
        .expect("down");
        assert!(
            matches!(
                app.drag.as_ref().map(|drag| &drag.target),
                Some(DragTarget::Divider {
                    dir: SplitDir::Vertical,
                    ..
                })
            ),
            "the seam must pick up a resize, never a pane"
        );
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 20, 8),
            &mut detached,
        )
        .await
        .expect("drag");
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 20, 8),
            &mut detached,
        )
        .await
        .expect("up");

        assert!(app.drag.is_none());
        assert!(
            !app.needs_reconcile,
            "a resize applies live; a swap would have asked to reconcile"
        );
        let after = heights(&server);
        assert_eq!(
            after.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec![top.as_str(), bottom.as_str()],
            "no swap: both panes keep their slots"
        );
        assert_eq!(
            after[0].1,
            before[0].1 + 3,
            "the top pane must grow by the dragged rows"
        );
        client.shutdown().await;
    }

    /// The frame is not a drag handle anymore: left-down on a frame cell,
    /// including the labeled title strip that used to start the swap,
    /// focuses the pane and creates no drag, mirroring the right-click arm.
    #[tokio::test]
    async fn a_left_down_on_the_frame_focuses_without_picking_the_pane_up() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-frame-focus");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let out = server.run(&["list-panes", "-t", "s", "-F", "#{pane_id}"]);
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        let (top, bottom) = (ids[0].clone(), ids[1].clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let mut app = stacked_app_with_painted_hits(
            &top,
            &bottom,
            cyclops_proto::scratch::scratch_dir("app-frame-focus-home"),
        );
        let mut detached = false;

        // The labeled bottom pane's title strip: the old swap pickup.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 6),
            &mut detached,
        )
        .await
        .expect("down");
        assert!(app.drag.is_none(), "a frame cell must never start a drag");

        // The top pane's frame: focus follows the click.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), 20, 0),
            &mut detached,
        )
        .await
        .expect("down");
        assert!(app.drag.is_none());
        assert_eq!(app.model.active_tab().active_pane, top);
        let active = server.run(&["display-message", "-p", "-t", "s", "#{pane_id}"]);
        assert_eq!(String::from_utf8_lossy(&active.stdout).trim(), top);
        client.shutdown().await;
    }

    /// The one frame cell that still picks a pane up: a drag from the
    /// bottom-right corner grip dropped on another pane swaps the two,
    /// exactly as the frame drag used to.
    #[tokio::test]
    async fn a_grip_drag_dropped_on_another_pane_swaps_them() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, ControlConfig};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("app-grip-swap");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let positions = |server: &TmuxServer| -> Vec<(String, String)> {
            String::from_utf8_lossy(
                &server
                    .run(&["list-panes", "-t", "s", "-F", "#{pane_id} #{pane_top}"])
                    .stdout,
            )
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.to_string(), fields.next()?.to_string()))
            })
            .collect()
        };
        let before = positions(&server);
        let (top, bottom) = (before[0].0.clone(), before[1].0.clone());
        let cfg = ControlConfig::attach("s")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let mut app = stacked_app_with_painted_hits(
            &top,
            &bottom,
            cyclops_proto::scratch::scratch_dir("app-grip-swap-home"),
        );
        let mut detached = false;

        // Down on the bottom pane's grip cell, drop on the top pane's body.
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Down(MouseButton::Left), 39, 10),
            &mut detached,
        )
        .await
        .expect("down");
        assert!(
            matches!(
                app.drag.as_ref().map(|drag| &drag.target),
                Some(DragTarget::Pane { pane_id }) if pane_id == &bottom
            ),
            "the grip must pick its pane up for a swap"
        );
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 20, 3),
            &mut detached,
        )
        .await
        .expect("drag");
        handle_mouse(
            &mut app,
            &client,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 20, 3),
            &mut detached,
        )
        .await
        .expect("up");

        assert!(app.needs_reconcile, "a swap is structural");
        let after = positions(&server);
        assert!(
            after
                .iter()
                .any(|(id, at)| id == &bottom && at == &before[0].1),
            "the dragged pane must take the top slot, got {after:?}"
        );
        assert!(
            after
                .iter()
                .any(|(id, at)| id == &top && at == &before[1].1),
            "the top pane must take the dragged pane's slot, got {after:?}"
        );
        client.shutdown().await;
    }
}
