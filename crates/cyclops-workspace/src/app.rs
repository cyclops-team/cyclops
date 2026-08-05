//! Workspace application state and event loop.
//!
//! The loop is event-armed: every visible change arms one render debounce
//! (`RENDER_DEBOUNCE`) if none is pending — arming never pushes an
//! already-armed deadline back, so a stream of events cannot starve
//! rendering. Full model reconciliation (subprocess tmux listing) is
//! deferred onto that same deadline and coalesced through
//! `needs_reconcile`; cheap structural notifications (`%layout-change`,
//! `%window-pane-changed`, `%session-changed`) apply to the model directly
//! without a full fetch.

#![allow(clippy::too_many_arguments)]

use std::collections::HashSet;
use std::io;

use crossterm::event::{
    self, Event, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use cyclops_tmux::{quote_arg, session_target, ControlClient, ControlConfig, Notification};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Duration, Instant};

use crate::bindings::{load_bindings, BindingAction};
use crate::config::load_tmux_config;
use crate::copy;
use crate::daemon::pane_has_agent;
use crate::decoration::{self, DecorationSnapshot};
use crate::dialog::Dialog;
use crate::drag::{DragState, DragTarget};
use crate::input::encode_send_keys;
use crate::input::mouse::{HitMap, HitTarget, MenuState};
use crate::input::router::{Router, RouterResult};
use crate::intent::{self, Intent};
use crate::layout::SplitDir;
use crate::model::{pane_is_visible, RuntimeRegistry, WorkspaceModel};
use crate::persist::{self, load_prefs, set_last_active, WorkspacePrefs};
use crate::render::{
    paint_dialog, paint_event_stream, paint_menu, paint_sidebar, paint_tab_bar, paint_window,
};
use crate::resilience::{self, LinkState};
use crate::selection::{self, SelectionState};
use crate::sync::{fetch_workspace_model, hydrate_visible_tab};
use crate::term_guard::TermGuard;
use crate::theme::Paint;

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
const TAB_BAR_HEIGHT: u16 = 1;
const EVENT_STREAM_WIDTH: u16 = 40;
const SIDEBAR_MIN_WIDTH: u16 = 22;
const SIDEBAR_MAX_WIDTH: u16 = 42;

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
}

struct App {
    model: WorkspaceModel,
    runtimes: RuntimeRegistry,
    router: Router,
    paint: Paint,
    socket: Option<String>,
    dialog: Option<Dialog>,
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
    event_stream_open: bool,
    event_lines: Vec<String>,
    term_size: (u16, u16),
    /// Last size successfully declared by this control client. Avoids a
    /// resize notification loop when expanded pane gutters are already at
    /// their target geometry.
    declared_client_size: Option<(u16, u16)>,
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

/// Chrome rectangles for one frame.
struct ChromeAreas {
    sidebar: Option<Rect>,
    panel: Option<Rect>,
    tab_bar: Rect,
    canvas: Rect,
}

fn chrome_areas_for(
    area: Rect,
    sidebar_visible: bool,
    sidebar_width: u16,
    panel_open: bool,
) -> ChromeAreas {
    let mut main = area;
    let sidebar = if sidebar_visible && main.width > 4 {
        let w = clamp_sidebar_width(sidebar_width, main.width);
        let s = Rect::new(main.x, main.y, w, main.height);
        main = Rect::new(main.x + w, main.y, main.width - w, main.height);
        Some(s)
    } else {
        None
    };
    let panel = if panel_open && main.width > EVENT_STREAM_WIDTH + 4 {
        let p = Rect::new(
            main.x + main.width - EVENT_STREAM_WIDTH,
            main.y,
            EVENT_STREAM_WIDTH,
            main.height,
        );
        main = Rect::new(main.x, main.y, main.width - EVENT_STREAM_WIDTH, main.height);
        Some(p)
    } else {
        None
    };
    let bar_h = TAB_BAR_HEIGHT.min(main.height);
    let tab_bar = Rect::new(main.x, main.y, main.width, bar_h);
    let canvas = Rect::new(
        main.x,
        main.y + bar_h,
        main.width,
        main.height.saturating_sub(bar_h),
    );
    ChromeAreas {
        sidebar,
        panel,
        tab_bar,
        canvas,
    }
}

fn clamp_sidebar_width(requested: u16, terminal_width: u16) -> u16 {
    let max = SIDEBAR_MAX_WIDTH.min(terminal_width / 2).max(1);
    let min = SIDEBAR_MIN_WIDTH.min(max);
    requested.clamp(min, max)
}

fn sidebar_width_for_column(column: u16, terminal_width: u16) -> u16 {
    clamp_sidebar_width(column.saturating_add(1), terminal_width)
}

fn sidebar_width_on_cancel(drag: &DragState, terminal_width: u16) -> Option<u16> {
    matches!(&drag.target, DragTarget::Sidebar)
        .then(|| sidebar_width_for_column(drag.start.0, terminal_width))
}

fn toggle_workspace_expanded(expanded: &mut HashSet<String>, session_id: String) -> bool {
    if expanded.remove(&session_id) {
        false
    } else {
        expanded.insert(session_id);
        true
    }
}

fn escape_cancels_visual_state(
    code: crossterm::event::KeyCode,
    selection_active: bool,
    drag_active: bool,
) -> bool {
    code == crossterm::event::KeyCode::Esc && (selection_active || drag_active)
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

    if let Err(e) = client.set_window_size_latest().await {
        eprintln!("{e}");
    }

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

    let guard = match TermGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };

    let term_size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut model = match fetch_workspace_model(&session, socket) {
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
                if let Err(error) =
                    intent::execute(&client, Intent::SelectTabId(window_id.clone()), "").await
                {
                    log_err(&home, &error);
                } else {
                    model.session.active_tab = index;
                }
            }
        }
    }

    // Declare terminal cells only after the split topology is known. tmux
    // gets pane content cells; two-cell separator bands remain UI chrome.
    let chrome_canvas = chrome_areas_for(
        Rect::new(0, 0, term_size.0, term_size.1),
        prefs.sidebar_visible,
        prefs.sidebar_width.max(SIDEBAR_MIN_WIDTH),
        false,
    )
    .canvas;
    let boot_size = crate::render::tmux_client_size(chrome_canvas, model.active_tab());
    let mut declared_client_size = None;
    if boot_size.0 >= 10 && boot_size.1 >= 3 {
        match client.set_client_size(boot_size.0, boot_size.1).await {
            Ok(()) => {
                declared_client_size = Some(boot_size);
                // The resize can rebalance leaf dimensions. Re-list before
                // hydration rather than replaying captures into stale slots.
                if let Ok(resized) = fetch_workspace_model(&session, socket) {
                    model = resized;
                    apply_workspace_order(&mut model, &prefs.workspace_order);
                }
            }
            Err(error) => log_err(&home, &error),
        }
    }
    let mut runtimes = RuntimeRegistry::default();
    if let Err(e) = hydrate_visible_tab(&client, model.active_tab(), &mut runtimes).await {
        drop(guard);
        eprintln!("{e}");
        client.shutdown().await;
        return 1;
    }

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
        socket: socket_name,
        dialog: None,
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
        event_stream_open: false,
        event_lines: Vec::new(),
        term_size,
        declared_client_size,
        needs_reconcile: false,
        needs_hydrate: false,
        paste_seq: 0,
        home,
        folder_probe_at: None,
    };
    app.model.sidebar_visible = prefs.sidebar_visible;
    // Bare `cyclops` can boot a session config.toml never mentions, so the
    // very first frame is already a frame the daemon may not be watching
    // for. Ask before drawing it.
    ensure_sessions_watched(&mut app);
    app.decoration = decoration::fetch_decoration(&app.home).unwrap_or_default();
    app.refresh_event_lines();

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
                    &tx,
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
                        if let Err(e) =
                            hydrate_visible_tab(&client, app.model.active_tab(), &mut app.runtimes)
                                .await
                        {
                            log_err(&app.home, &e);
                        }
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

/// Event-driven daemon decoration updates. The subscription itself never
/// polls; each pushed state/label/delivery event triggers one bounded status
/// snapshot on this dedicated thread, away from the input loop.
fn spawn_decoration_forwarder(home: std::path::PathBuf, tx: mpsc::UnboundedSender<AppMsg>) {
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        let socket = home.join(cyclops_proto::SOCK_NAME);
        let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) else {
            return;
        };
        let mut reader = std::io::BufReader::new(stream);
        let mut hello = String::new();
        if reader
            .read_line(&mut hello)
            .ok()
            .filter(|read| *read > 0)
            .is_none()
        {
            return;
        }
        if reader
            .get_mut()
            .write_all(b"{\"id\":1,\"method\":\"events.subscribe\",\"params\":{}}\n")
            .is_err()
        {
            return;
        }
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("event").is_none() {
                continue;
            }
            // A refused or timed-out status call is doubt about this
            // instant, not news about the roster; the subscription is still
            // up, so the next event asks again. Dropping it keeps the last
            // known decoration on screen instead of un-naming every agent
            // for a frame. A daemon that is really gone ends the read loop
            // below, which is the one place "offline" is reported.
            let Some(snapshot) = decoration::fetch_decoration(&home) else {
                continue;
            };
            if tx.send(AppMsg::DecorationChanged(snapshot)).is_err() {
                return;
            }
        }
        let _ = tx.send(AppMsg::DecorationChanged(DecorationSnapshot::default()));
    });
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
            let _ = client.set_window_size_latest().await;
            app.declared_client_size = None;
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

    fn sidebar_width(&self) -> u16 {
        self.prefs.sidebar_width.max(SIDEBAR_MIN_WIDTH)
    }

    fn chrome(&self, area: Rect) -> ChromeAreas {
        chrome_areas_for(
            area,
            self.model.sidebar_visible,
            self.sidebar_width(),
            self.event_stream_open,
        )
    }

    fn refresh_event_lines(&mut self) {
        self.event_lines = self
            .decoration
            .attention
            .items()
            .into_iter()
            .map(|item| format!("{item:?}"))
            .collect();
    }

    fn persist_active(&self) {
        let tab = self.model.active_tab();
        set_last_active(&self.home, &self.model.session.session, &tab.window_id);
    }

    /// Whether this motion arrives on, or departs from, the sidebar's
    /// create button. Both edges have to reach the renderer: one lights the
    /// button, the other puts it out, and a filter that only let the
    /// arrival through would leave it lit wherever the mouse went next.
    fn motion_touches_new_workspace_button(&self, col: u16, row: u16) -> bool {
        let on_button = |col: u16, row: u16| {
            matches!(
                self.hit_map.hit(col, row),
                Some(HitTarget::NewWorkspaceButton)
            )
        };
        on_button(col, row) || self.hover.is_some_and(|(col, row)| on_button(col, row))
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
    _tx: &mpsc::UnboundedSender<AppMsg>,
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
                    if migrate_order_entry(&mut app.prefs.workspace_order, &old_name, &name) {
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
            app.paused_panes.remove(&pane);
            if app.is_visible_pane(&pane) {
                // Rehydrate: paused output was dropped, continuity is gone.
                app.runtimes.retain_visible(&[]);
                app.needs_hydrate = true;
            }
            arm(debounce);
        }
        AppMsg::DecorationChanged(snapshot) => {
            // A daemon that went away forgot every session it was asked to
            // watch: those live in memory, not in config.toml. Dropping the
            // record here is what makes the next reconcile ask again.
            if !snapshot.online {
                app.watched_sessions.clear();
            }
            if migrate_agent_order_entries(&mut app.prefs.agent_order, &app.decoration, &snapshot) {
                if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
                    log_err(&app.home, &error);
                }
            }
            app.decoration = snapshot;
            app.refresh_event_lines();
            arm(debounce);
        }
        AppMsg::Mouse(mouse) => {
            // Bare motion only matters while a menu or dialog shows hover
            // highlights — or over the sidebar's create button, the one
            // piece of resting chrome that answers the mouse. Everywhere
            // else it must not wake the renderer.
            if matches!(mouse.kind, MouseEventKind::Moved)
                && !app.menu.is_open()
                && app.dialog.is_none()
                && !app.motion_touches_new_workspace_button(mouse.column, mouse.row)
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
                if append_dialog_text(app.dialog.as_mut(), &text) {
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
            if escape_cancels_visual_state(
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
        if let Some(width) = sidebar_width_on_cancel(&drag, app.term_size.0) {
            // Sidebar motion is only visual until mouse-up, so Escape can
            // restore the start without a compensating tmux resize.
            app.prefs.sidebar_width = width;
        }
    }
}

/// Focus a pane, switching to the tab that owns it when needed. Sidebar
/// agent rows span every tab in the active workspace, so selecting only the
/// pane would otherwise leave the UI on a different window.
async fn focus_pane(
    app: &mut App,
    client: &ControlClient,
    pane_id: &str,
) -> Result<(), cyclops_tmux::TmuxError> {
    let target = app
        .model
        .session
        .tabs
        .iter()
        .position(|tab| crate::layout::layout_contains_pane(&tab.layout, pane_id));
    let prior_tab = app.model.session.active_tab;
    let prior_pane = app.model.active_tab().active_pane.clone();
    let target_window = target
        .filter(|index| *index != prior_tab)
        .and_then(|index| app.model.session.tabs.get(index))
        .map(|tab| tab.window_id.as_str());

    intent::execute_focus_pane(client, target_window, pane_id).await?;
    let Some(index) = target else {
        // The daemon or hit map can briefly be ahead of a tmux reconcile.
        // Never attach that stale pane id to the wrong tab in local state.
        app.needs_reconcile = true;
        return Ok(());
    };

    app.model.session.active_tab = index;
    let zoomed = app.model.session.tabs[index].zoomed;
    app.model.session.tabs[index].active_pane = pane_id.to_string();
    if index != prior_tab || (zoomed && prior_pane != pane_id) {
        resize_client(app, client).await;
        hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await?;
        app.needs_hydrate = false;
        app.persist_active();
    }
    Ok(())
}

/// Focus a sidebar agent even when its expanded parent is a background
/// workspace. The switch and pane selection are sent in order; reconciliation
/// then replaces the active session model with authoritative tmux state.
async fn focus_sidebar_agent(
    app: &mut App,
    client: &ControlClient,
    workspace_id: &str,
    pane_id: &str,
) -> Result<(), cyclops_tmux::TmuxError> {
    let active_id = app
        .model
        .workspaces
        .get(app.model.active_workspace)
        .map(|workspace| workspace.session_id.as_str());
    if active_id == Some(workspace_id) {
        if app.model.active_tab().active_pane == pane_id {
            return Ok(());
        }
        return focus_pane(app, client, pane_id).await;
    }
    let Some(session) = app
        .model
        .workspaces
        .iter()
        .find(|workspace| workspace.session_id == workspace_id)
        .map(|workspace| workspace.name.clone())
    else {
        app.needs_reconcile = true;
        return Ok(());
    };
    let Some(window_id) = app
        .decoration
        .pane(pane_id)
        .map(|decoration| decoration.window_id.clone())
    else {
        // A decoration event can invalidate a rendered hit region before the
        // next frame. Do not select a same-shaped pane in the wrong window.
        app.needs_reconcile = true;
        return Ok(());
    };
    intent::execute(client, Intent::SwitchWorkspace(session), "").await?;
    intent::execute_focus_pane(client, Some(&window_id), pane_id).await?;
    app.needs_reconcile = true;
    Ok(())
}

/// Select a tab by model index: tell tmux, mirror locally, hydrate.
async fn select_tab(
    app: &mut App,
    client: &ControlClient,
    index: usize,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(window_id) = app
        .model
        .session
        .tabs
        .get(index)
        .map(|t| t.window_id.clone())
    else {
        return Ok(());
    };
    intent::execute(client, Intent::SelectTabId(window_id), "").await?;
    app.model.session.active_tab = index;
    resize_client(app, client).await;
    hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await?;
    app.needs_hydrate = false;
    app.persist_active();
    Ok(())
}

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
            let max_scroll = keybind_scroll_limit(app);
            if let Some(Dialog::Keybinds { scroll, .. }) = app.dialog.as_mut() {
                let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    -3
                } else {
                    3
                };
                *scroll = move_keybind_scroll(*scroll, delta, max_scroll);
            }
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
            if let Some(HitTarget::PaneBody { pane_id }) = app.hit_map.hit(col, row).cloned() {
                let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    -3
                } else {
                    3
                };
                if let Some(rt) = app.runtimes.get_mut(&pane_id) {
                    rt.scroll(delta);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => match app.hit_map.hit(col, row).cloned() {
            Some(HitTarget::PaneBody { pane_id } | HitTarget::PaneFrame { pane_id }) => {
                focus_pane(app, client, &pane_id).await?;
                app.open_menu(MenuState::ContextMenu {
                    pane_id,
                    at: (col, row),
                });
            }
            Some(HitTarget::Tab { window_id }) => {
                app.open_menu(MenuState::TabMenu {
                    window_id,
                    at: (col, row),
                });
            }
            Some(HitTarget::SidebarRow { session, .. }) => {
                app.open_menu(MenuState::WorkspaceMenu {
                    session,
                    at: (col, row),
                });
            }
            Some(HitTarget::SidebarAgent {
                workspace_id,
                pane_id,
                ..
            }) => {
                focus_sidebar_agent(app, client, &workspace_id, &pane_id).await?;
                app.open_menu(MenuState::ContextMenu {
                    pane_id,
                    at: (col, row),
                });
            }
            _ => app.close_menu(),
        },
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = app.hit_map.hit(col, row).cloned() else {
                app.close_menu();
                app.selection.clear();
                return Ok(());
            };
            match target {
                HitTarget::MenuItem { action } => {
                    let menu = std::mem::replace(&mut app.menu, MenuState::None);
                    app.hover = None;
                    app.hit_map.clear_menu_items();
                    if action == BindingAction::Detach {
                        *detached = true;
                        return Ok(());
                    }
                    menu_action(app, client, menu, action).await?;
                }
                HitTarget::PaneBody { pane_id } => {
                    app.close_menu();
                    let target = HitTarget::PaneBody {
                        pane_id: pane_id.clone(),
                    };
                    let clicks = app.selection.register_click(&target, col, row);
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
                    if app.model.active_tab().active_pane != pane_id {
                        focus_pane(app, client, &pane_id).await?;
                    }
                }
                HitTarget::PaneFrame { pane_id } => {
                    app.close_menu();
                    app.selection.clear();
                    if app.model.active_tab().active_pane != pane_id {
                        focus_pane(app, client, &pane_id).await?;
                    }
                }
                HitTarget::PaneSplitRight { pane_id } => {
                    app.close_menu();
                    intent::execute(client, Intent::SplitRight, &pane_id).await?;
                    app.needs_reconcile = true;
                }
                HitTarget::PaneSplitDown { pane_id } => {
                    app.close_menu();
                    intent::execute(client, Intent::SplitDown, &pane_id).await?;
                    app.needs_reconcile = true;
                }
                HitTarget::Divider { pane_id, dir } => {
                    app.selection.clear();
                    app.drag = Some(DragState::on_down(
                        DragTarget::Divider { pane_id, dir },
                        col,
                        row,
                    ));
                }
                HitTarget::Tab { window_id } => {
                    app.close_menu();
                    app.selection.clear();
                    // Down starts a possible reorder drag; a below-threshold
                    // release selects the tab instead.
                    app.drag = Some(DragState::on_down(DragTarget::Tab { window_id }, col, row));
                }
                HitTarget::NewTabButton => {
                    app.close_menu();
                    app.dialog = Some(Dialog::NewTab {
                        buffer: String::new(),
                    });
                }
                HitTarget::SidebarRow {
                    session_id,
                    session,
                } => {
                    app.close_menu();
                    app.selection.clear();
                    app.drag = Some(DragState::on_down(
                        DragTarget::Workspace {
                            session_id,
                            session,
                        },
                        col,
                        row,
                    ));
                }
                HitTarget::SidebarDisclosure { session_id } => {
                    toggle_workspace_expanded(&mut app.expanded_workspaces, session_id);
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
                            workspace_id,
                            pane_id,
                            order_key,
                        },
                        col,
                        row,
                    ));
                }
                HitTarget::SidebarDivider => {
                    app.close_menu();
                    app.selection.clear();
                    app.drag = Some(DragState::on_down(DragTarget::Sidebar, col, row));
                }
                HitTarget::AttentionIndicator { pane_id } => {
                    app.close_menu();
                    focus_pane(app, client, &pane_id).await?;
                }
                HitTarget::AppMenu => {
                    if app.menu == MenuState::AppMenu {
                        app.close_menu();
                    } else {
                        app.open_menu(MenuState::AppMenu);
                    }
                }
                HitTarget::NewWorkspaceButton => {
                    app.close_menu();
                    new_workspace_here(app, client).await?;
                }
                HitTarget::DialogConfirm | HitTarget::DialogCancel => {}
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
                    app.prefs.sidebar_width = sidebar_width_for_column(col, app.term_size.0);
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
                app.prefs.sidebar_width = sidebar_width_for_column(col, app.term_size.0);
                if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
                    log_err(&app.home, &error);
                }
                resize_client(app, client).await;
            }
            apply_live_divider(app, client).await?;
            if let Some(drag) = app.drag.take() {
                match drag.on_up() {
                    Some(DragTarget::Tab { window_id }) => {
                        commit_tab_drop(app, client, &window_id, col, row).await?;
                    }
                    Some(DragTarget::Workspace {
                        session_id,
                        session: _,
                    }) => commit_workspace_drop(app, &session_id, col, row),
                    Some(DragTarget::Agent {
                        workspace_id,
                        pane_id: _,
                        order_key,
                    }) => commit_agent_drop(app, &workspace_id, &order_key, col, row),
                    Some(DragTarget::Sidebar) => {}
                    // Divider drags applied live during motion.
                    Some(_) => {}
                    None => match drag.target {
                        DragTarget::Tab { window_id } => {
                            if let Some(index) = app
                                .model
                                .session
                                .tabs
                                .iter()
                                .position(|tab| tab.window_id == window_id)
                            {
                                select_tab(app, client, index).await?;
                            }
                        }
                        DragTarget::Workspace { session, .. } => {
                            intent::execute(client, Intent::SwitchWorkspace(session), "").await?;
                            app.needs_reconcile = true;
                        }
                        DragTarget::Agent {
                            workspace_id,
                            pane_id,
                            ..
                        } => {
                            focus_sidebar_agent(app, client, &workspace_id, &pane_id).await?;
                        }
                        DragTarget::Divider { .. } | DragTarget::Sidebar => {}
                    },
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

/// Move one item to the index occupied by another. Downward drags land after
/// the target and upward drags before it, matching the direction of motion.
fn move_to_index(items: &mut Vec<String>, source: &str, target: &str) -> bool {
    let Some(source_index) = items.iter().position(|item| item == source) else {
        return false;
    };
    let Some(target_index) = items.iter().position(|item| item == target) else {
        return false;
    };
    if source_index == target_index {
        return false;
    }
    let item = items.remove(source_index);
    items.insert(target_index.min(items.len()), item);
    true
}

/// Keep a persisted row in place when its identity-bearing label changes.
/// If a stale copy of the new key already exists, remove the old entry rather
/// than creating a duplicate.
fn migrate_order_entry(order: &mut Vec<String>, old: &str, new: &str) -> bool {
    if old == new {
        return false;
    }
    let Some(old_index) = order.iter().position(|entry| entry == old) else {
        return false;
    };
    if order.iter().any(|entry| entry == new) {
        order.remove(old_index);
    } else {
        order[old_index] = new.to_string();
    }
    true
}

/// Preserve a pane's position when a daemon event reports an external rename
/// or clear. Pane ids bind the before/after snapshots without guessing from
/// display text.
fn migrate_agent_order_entries(
    order: &mut Vec<String>,
    previous: &DecorationSnapshot,
    next: &DecorationSnapshot,
) -> bool {
    let mut changed = false;
    for pane in next.panes.values() {
        let Some(old_pane) = previous.pane(&pane.pane_id) else {
            continue;
        };
        let old_key = DecorationSnapshot::agent_order_key(old_pane);
        let new_key = DecorationSnapshot::agent_order_key(pane);
        changed |= migrate_order_entry(order, &old_key, &new_key);
    }
    changed
}

fn commit_workspace_drop(app: &mut App, source_id: &str, col: u16, row: u16) {
    let Some(HitTarget::SidebarRow {
        session_id: target_id,
        ..
    }) = app.hit_map.hit(col, row).cloned()
    else {
        return;
    };
    let active_id = app
        .model
        .workspaces
        .get(app.model.active_workspace)
        .map(|workspace| workspace.session_id.clone());
    let Some(source_index) = app
        .model
        .workspaces
        .iter()
        .position(|workspace| workspace.session_id == source_id)
    else {
        return;
    };
    let Some(target_index) = app
        .model
        .workspaces
        .iter()
        .position(|workspace| workspace.session_id == target_id)
    else {
        return;
    };
    if source_index == target_index {
        return;
    }
    let workspace = app.model.workspaces.remove(source_index);
    app.model
        .workspaces
        .insert(target_index.min(app.model.workspaces.len()), workspace);
    app.model.active_workspace = active_id
        .and_then(|id| {
            app.model
                .workspaces
                .iter()
                .position(|workspace| workspace.session_id == id)
        })
        .unwrap_or(0);
    app.prefs.workspace_order = app
        .model
        .workspaces
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect();
    if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
        log_err(&app.home, &error);
    }
}

fn commit_agent_drop(app: &mut App, workspace_id: &str, source_key: &str, col: u16, row: u16) {
    let Some(HitTarget::SidebarAgent {
        workspace_id: target_workspace,
        order_key: target_key,
        ..
    }) = app.hit_map.hit(col, row).cloned()
    else {
        return;
    };
    if target_workspace != workspace_id {
        return;
    }
    let Some(workspace) = app
        .model
        .workspaces
        .iter()
        .find(|workspace| workspace.session_id == workspace_id)
    else {
        return;
    };
    let mut local: Vec<String> = app
        .decoration
        .agent_rows_for_window_ids(&workspace.window_ids, &app.prefs.agent_order)
        .into_iter()
        .map(DecorationSnapshot::agent_order_key)
        .collect();
    if !move_to_index(&mut local, source_key, &target_key) {
        return;
    }
    app.prefs
        .agent_order
        .retain(|key| !local.iter().any(|local_key| local_key == key));
    app.prefs.agent_order.extend(local);
    if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
        log_err(&app.home, &error);
    }
}

/// Apply divider motion since the last applied step as resize-pane calls.
/// tmux's `%layout-change` answers reconcile the model — the drag itself
/// never writes geometry.
async fn apply_live_divider(
    app: &mut App,
    client: &ControlClient,
) -> Result<bool, cyclops_tmux::TmuxError> {
    let Some(drag) = app.drag.as_mut() else {
        return Ok(false);
    };
    if !drag.is_active() {
        return Ok(false);
    }
    let DragTarget::Divider { pane_id, dir } = drag.target.clone() else {
        return Ok(false);
    };
    let delta = match dir {
        SplitDir::Horizontal => drag.current.0 as i32 - drag.last_applied.0 as i32,
        SplitDir::Vertical => drag.current.1 as i32 - drag.last_applied.1 as i32,
    };
    if delta == 0 {
        return Ok(false);
    }
    intent::resize_divider(client, &pane_id, dir, delta).await?;
    drag.last_applied = drag.current;
    Ok(true)
}

/// Commit a tab drag: drop on another tab reorders, drop on a sidebar row
/// moves the window to that workspace.
async fn commit_tab_drop(
    app: &mut App,
    client: &ControlClient,
    window_id: &str,
    col: u16,
    row: u16,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(src) = app
        .model
        .session
        .tabs
        .iter()
        .find(|tab| tab.window_id == window_id)
        .map(|tab| tab.window_id.clone())
    else {
        return Ok(());
    };
    match app.hit_map.hit(col, row).cloned() {
        Some(HitTarget::Tab { window_id: dst }) if dst != src => {
            client
                .command(&format!(
                    "swap-window -s {} -t {}",
                    quote_arg(&src),
                    quote_arg(&dst)
                ))
                .await?;
            app.needs_reconcile = true;
        }
        Some(HitTarget::SidebarRow { session, .. }) => {
            client
                .command(&format!(
                    "move-window -s {} -t {}",
                    quote_arg(&src),
                    quote_arg(&format!("{}:", session_target(&session)))
                ))
                .await?;
            app.needs_reconcile = true;
        }
        _ => {}
    }
    Ok(())
}

/// Create a tab in the current pane's directory; a non-empty `name`
/// renames the fresh window.
async fn new_tab(
    app: &mut App,
    client: &ControlClient,
    name: Option<&str>,
) -> Result<(), cyclops_tmux::TmuxError> {
    let pane = app.model.active_tab().active_pane.clone();
    let cwd = client
        .display(&pane, "#{pane_current_path}")
        .await
        .map(|p| p.trim().to_string())
        .ok();
    let default_name = next_numeric_tab_name(&app.model.session.tabs);
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&default_name);
    intent::execute_new_tab(client, cwd.as_deref(), Some(name)).await?;
    app.needs_reconcile = true;
    Ok(())
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

/// Create a workspace named after the focused pane's directory and switch
/// to it — no prompt, the folder is the name.
async fn new_workspace_here(
    app: &mut App,
    client: &ControlClient,
) -> Result<(), cyclops_tmux::TmuxError> {
    let pane = app.model.active_tab().active_pane.clone();
    let cwd = client
        .display(&pane, "#{pane_current_path}")
        .await
        .map(|p| p.trim().to_string())
        .unwrap_or_default();
    let folder = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    } else {
        std::path::PathBuf::from(cwd)
    };
    let taken: Vec<String> = app
        .model
        .workspaces
        .iter()
        .map(|w| w.name.clone())
        .collect();
    let created = intent::execute_new_workspace(client, &folder, &taken).await?;
    if !app.prefs.folder_tracked.contains(&created.session_id) {
        app.prefs.folder_tracked.push(created.session_id);
        if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
            log_err(&app.home, &error);
        }
    }
    app.needs_reconcile = true;
    Ok(())
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
    let Some(next) = intent::folder_rename(&current_name, cwd.trim(), &taken) else {
        return Ok(());
    };

    intent::execute_rename_workspace(client, &current_name, &next).await?;

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
    if migrate_order_entry(&mut app.prefs.workspace_order, &current_name, &next) {
        if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
            log_err(&app.home, &error);
        }
    }
    app.needs_reconcile = true;
    Ok(())
}

/// Close a pane: straight away when it hosts no agent, else via confirm.
async fn close_pane_flow(
    app: &mut App,
    client: &ControlClient,
    pane_id: String,
) -> Result<(), cyclops_tmux::TmuxError> {
    if pane_has_agent(&app.home, &pane_id) {
        app.dialog = Some(Dialog::confirm_close(pane_id));
    } else {
        intent::execute(client, Intent::ClosePane, &pane_id).await?;
        app.needs_reconcile = true;
    }
    Ok(())
}

fn open_name_pane(app: &mut App, pane_id: String) {
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
}

/// Close a tab: straight away when it hosts no agent, else via confirm.
async fn close_tab_flow(
    app: &mut App,
    client: &ControlClient,
    window_id: String,
) -> Result<(), cyclops_tmux::TmuxError> {
    if app.model.session.tabs.len() == 1 && app.model.session.tabs[0].window_id == window_id {
        app.dialog = Some(Dialog::ConfirmCloseWorkspace {
            session: app.model.session.session.clone(),
        });
        return Ok(());
    }
    let has_agent = app
        .model
        .session
        .tabs
        .iter()
        .find(|t| t.window_id == window_id)
        .map(|t| crate::layout::pane_ids_in_layout(&t.layout))
        .into_iter()
        .flatten()
        .any(|p| pane_has_agent(&app.home, &p));
    if has_agent {
        app.dialog = Some(Dialog::ConfirmCloseTab { window_id });
    } else {
        intent::execute_close_tab(client, &window_id).await?;
        app.needs_reconcile = true;
    }
    Ok(())
}

/// Run one menu item against the pane, tab, or workspace that opened it.
async fn menu_action(
    app: &mut App,
    client: &ControlClient,
    menu: MenuState,
    action: BindingAction,
) -> Result<(), cyclops_tmux::TmuxError> {
    match (menu, action) {
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::NamePane) => {
            open_name_pane(app, pane_id);
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SplitRight) => {
            intent::execute(client, Intent::SplitRight, &pane_id).await?;
            app.needs_reconcile = true;
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::SplitDown) => {
            intent::execute(client, Intent::SplitDown, &pane_id).await?;
            app.needs_reconcile = true;
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::ZoomPane) => {
            intent::execute(client, Intent::ZoomPane, &pane_id).await?;
            app.needs_reconcile = true;
        }
        (MenuState::ContextMenu { pane_id, .. }, BindingAction::ClosePane) => {
            close_pane_flow(app, client, pane_id).await?;
        }
        (MenuState::TabMenu { window_id, .. }, BindingAction::RenameTab) => {
            if let Some(tab) = app
                .model
                .session
                .tabs
                .iter()
                .find(|tab| tab.window_id == window_id)
            {
                app.dialog = Some(Dialog::RenameTab {
                    window_id: tab.window_id.clone(),
                    buffer: tab.name.clone(),
                });
            }
        }
        (MenuState::TabMenu { window_id, .. }, BindingAction::CloseTab) => {
            if app
                .model
                .session
                .tabs
                .iter()
                .any(|tab| tab.window_id == window_id)
            {
                close_tab_flow(app, client, window_id).await?;
            }
        }
        (MenuState::WorkspaceMenu { session, .. }, BindingAction::RenameWorkspace) => {
            if let Some(ws) = app
                .model
                .workspaces
                .iter()
                .find(|workspace| workspace.name == session)
            {
                app.dialog = Some(Dialog::RenameWorkspace {
                    session: ws.name.clone(),
                    buffer: ws.name.clone(),
                });
            }
        }
        (MenuState::WorkspaceMenu { session, .. }, BindingAction::CloseWorkspace) => {
            if let Some(ws) = app
                .model
                .workspaces
                .iter()
                .find(|workspace| workspace.name == session)
            {
                app.dialog = Some(Dialog::ConfirmCloseWorkspace {
                    session: ws.name.clone(),
                });
            }
        }
        // The menus' "New tab" is a mouse affordance, so it opens the
        // naming modal; the keyboard binding stays instant.
        (_, BindingAction::NewTab) => {
            app.dialog = Some(Dialog::NewTab {
                buffer: String::new(),
            });
        }
        (MenuState::AppMenu, action) => dispatch_action(app, client, action).await?,
        (MenuState::None, _) => {}
        (menu, action) => {
            return Err(cyclops_tmux::TmuxError::Protocol(format!(
                "menu action {action:?} is not valid for {menu:?}"
            )));
        }
    }
    Ok(())
}

/// Apply the open dialog's action (Enter or its confirm button).
async fn dialog_confirm(
    app: &mut App,
    client: &ControlClient,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(dialog) = app.dialog.clone() else {
        return Ok(());
    };
    if let Dialog::NamePane {
        pane_id, buffer, ..
    } = &dialog
    {
        let label = buffer.trim();
        let previous_order_key = app
            .decoration
            .pane(pane_id)
            .map(DecorationSnapshot::agent_order_key);
        let result = crate::daemon::label_pane(&app.home, pane_id, label);
        if let Err(error) = result {
            if let Some(Dialog::NamePane {
                error: shown_error, ..
            }) = app.dialog.as_mut()
            {
                *shown_error = Some(error);
            }
            return Ok(());
        }
        let next_order_key = format!("name:{label}");
        if previous_order_key.is_some_and(|previous| {
            migrate_order_entry(&mut app.prefs.agent_order, &previous, &next_order_key)
        }) {
            if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
                log_err(&app.home, &error);
            }
        }
        app.dialog = None;
        app.hover = None;
        if let Some(snapshot) = decoration::fetch_decoration(&app.home) {
            app.decoration = snapshot;
        }
        app.refresh_event_lines();
        return Ok(());
    }
    app.dialog = None;
    app.hover = None;
    match dialog {
        Dialog::ConfirmClosePane { pane_id } => {
            intent::execute(client, Intent::ClosePane, &pane_id).await?;
            app.needs_reconcile = true;
        }
        Dialog::NewTab { buffer } => {
            new_tab(app, client, Some(buffer.trim())).await?;
        }
        Dialog::NamePane { .. } => unreachable!("pane naming returns above"),
        Dialog::RenameTab { window_id, buffer } => {
            if !buffer.trim().is_empty() {
                intent::execute_rename_tab(client, &window_id, buffer.trim()).await?;
                app.needs_reconcile = true;
            }
        }
        Dialog::ConfirmCloseTab { window_id } => {
            intent::execute_close_tab(client, &window_id).await?;
            app.needs_reconcile = true;
        }
        Dialog::RenameWorkspace { session, buffer } => {
            if !buffer.trim().is_empty() {
                let name = buffer.trim();
                intent::execute_rename_workspace(client, &session, name).await?;
                let mut prefs_changed =
                    migrate_order_entry(&mut app.prefs.workspace_order, &session, name);
                // An explicit rename means the user owns the name now — a
                // folder-following workspace must never be renamed out from
                // under them again.
                if let Some(session_id) = app
                    .model
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.name == session)
                    .map(|workspace| workspace.session_id.clone())
                {
                    let before = app.prefs.folder_tracked.len();
                    app.prefs.folder_tracked.retain(|id| id != &session_id);
                    prefs_changed |= app.prefs.folder_tracked.len() != before;
                }
                if prefs_changed {
                    if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
                        log_err(&app.home, &error);
                    }
                }
                // The model addresses the active session BY NAME; skip this
                // and the reconcile that follows queries the old name.
                if app.model.session.session == session {
                    app.model.session.session = name.to_string();
                }
                app.needs_reconcile = true;
            }
        }
        Dialog::ConfirmCloseWorkspace { session } => {
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
            intent::execute_close_workspace(client, &session, fallback.as_deref()).await?;
            let previous_order_len = app.prefs.workspace_order.len();
            app.prefs.workspace_order.retain(|name| name != &session);
            let previous_tracked_len = app.prefs.folder_tracked.len();
            if let Some(session_id) = closed_session_id {
                app.prefs.folder_tracked.retain(|id| id != &session_id);
            }
            if app.prefs.workspace_order.len() != previous_order_len
                || app.prefs.folder_tracked.len() != previous_tracked_len
            {
                if let Err(error) = persist::save_prefs(&app.home, &app.prefs) {
                    log_err(&app.home, &error);
                }
            }
            app.needs_reconcile = true;
        }
        Dialog::Keybinds { .. } => {}
    }
    Ok(())
}

fn dialog_cancel(app: &mut App) {
    app.dialog = None;
    app.hover = None;
}

/// The editable buffer of an input dialog, if this dialog has one.
fn dialog_buffer_mut(dialog: &mut Dialog) -> Option<&mut String> {
    match dialog {
        Dialog::NewTab { buffer }
        | Dialog::NamePane { buffer, .. }
        | Dialog::RenameTab { buffer, .. }
        | Dialog::RenameWorkspace { buffer, .. } => Some(buffer),
        _ => None,
    }
}

/// Add printable pasted text to an input dialog. Line controls belong to a
/// pane paste, never to a tmux tab or session name.
fn append_dialog_text(dialog: Option<&mut Dialog>, text: &str) -> bool {
    let Some(dialog) = dialog else {
        return false;
    };
    if let Dialog::NamePane { error, .. } = dialog {
        *error = None;
    }
    let Some(buffer) = dialog_buffer_mut(dialog) else {
        return false;
    };
    let before = buffer.len();
    buffer.extend(text.chars().filter(|ch| !ch.is_control()));
    buffer.len() != before
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogKeyAction {
    Confirm,
    Cancel,
    Backspace,
    Append(char),
    Scroll(i16),
    ScrollStart,
    ScrollEnd,
    Ignore,
}

/// Resolve dialog keys without mutating application state. Every modal
/// confirms on Enter and cancels on Escape, so one key means the same thing
/// in every dialog. The read-only keybinds sheet has nothing to confirm, so
/// Enter dismisses it.
fn dialog_key_action(dialog: &Dialog, key: &KeyEvent) -> DialogKeyAction {
    use crossterm::event::KeyCode;

    if matches!(dialog, Dialog::Keybinds { .. }) {
        return match key.code {
            KeyCode::Esc | KeyCode::Enter => DialogKeyAction::Cancel,
            KeyCode::Up => DialogKeyAction::Scroll(-1),
            KeyCode::Down => DialogKeyAction::Scroll(1),
            KeyCode::PageUp => DialogKeyAction::Scroll(-8),
            KeyCode::PageDown => DialogKeyAction::Scroll(8),
            KeyCode::Home => DialogKeyAction::ScrollStart,
            KeyCode::End => DialogKeyAction::ScrollEnd,
            _ => DialogKeyAction::Ignore,
        };
    }
    let text_key = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Esc => DialogKeyAction::Cancel,
        KeyCode::Enter => DialogKeyAction::Confirm,
        KeyCode::Backspace if dialog.has_input() => DialogKeyAction::Backspace,
        KeyCode::Char(c) if dialog.has_input() && text_key => DialogKeyAction::Append(c),
        _ => DialogKeyAction::Ignore,
    }
}

fn keybind_scroll_limit(app: &App) -> u16 {
    let row_count = match app.dialog.as_ref() {
        Some(Dialog::Keybinds { rows, .. }) => rows.len(),
        _ => return 0,
    };
    crate::render::keybind_max_scroll(row_count, Rect::new(0, 0, app.term_size.0, app.term_size.1))
}

fn move_keybind_scroll(current: u16, delta: i16, max: u16) -> u16 {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs()).min(max)
    } else {
        current.saturating_add(delta as u16).min(max)
    }
}

async fn reconcile(app: &mut App, client: &ControlClient) -> Result<(), cyclops_tmux::TmuxError> {
    let session = app.model.session.session.clone();
    let mut model = fetch_workspace_model(&session, app.socket.as_deref())?;
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
    app.refresh_event_lines();
    app.persist_active();
    resize_client(app, client).await;
    hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await?;
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
        RouterResult::Action(BindingAction::Detach) => Ok(InputOutcome::Detached),
        RouterResult::Action(action) => {
            dispatch_action(app, client, action).await?;
            Ok(InputOutcome::Redraw)
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

async fn dispatch_action(
    app: &mut App,
    client: &ControlClient,
    action: BindingAction,
) -> Result<(), cyclops_tmux::TmuxError> {
    let pane = app.model.active_tab().active_pane.clone();
    match action {
        BindingAction::ClosePane => {
            close_pane_flow(app, client, pane).await?;
        }
        BindingAction::NamePane => {
            open_name_pane(app, pane);
            return Ok(());
        }
        BindingAction::NewTab => {
            new_tab(app, client, None).await?;
        }
        BindingAction::NextTab | BindingAction::PrevTab => {
            let len = app.model.session.tabs.len();
            if len > 0 {
                let cur = app.model.session.active_tab as isize;
                let delta = if action == BindingAction::NextTab {
                    1
                } else {
                    -1
                };
                let next = (cur + delta).rem_euclid(len as isize) as usize;
                select_tab(app, client, next).await?;
            }
        }
        BindingAction::SelectTab(n) => {
            select_tab(app, client, n.saturating_sub(1)).await?;
        }
        BindingAction::RenameTab => {
            let tab = app.model.active_tab();
            app.dialog = Some(Dialog::RenameTab {
                window_id: tab.window_id.clone(),
                buffer: tab.name.clone(),
            });
            return Ok(());
        }
        BindingAction::CloseTab => {
            let window_id = app.model.active_tab().window_id.clone();
            close_tab_flow(app, client, window_id).await?;
        }
        BindingAction::NewWorkspace => {
            new_workspace_here(app, client).await?;
        }
        BindingAction::RenameWorkspace => {
            let session = app.model.session.session.clone();
            app.dialog = Some(Dialog::RenameWorkspace {
                buffer: session.clone(),
                session,
            });
            return Ok(());
        }
        BindingAction::CloseWorkspace => {
            app.dialog = Some(Dialog::ConfirmCloseWorkspace {
                session: app.model.session.session.clone(),
            });
            return Ok(());
        }
        BindingAction::ToggleEventPanel => {
            app.event_stream_open = !app.event_stream_open;
            resize_client(app, client).await;
            return Ok(());
        }
        BindingAction::ShowKeybinds => {
            app.dialog = Some(Dialog::Keybinds {
                scroll: 0,
                rows: app.router.help(),
            });
            return Ok(());
        }
        BindingAction::NextWorkspace => {
            let active = app.model.active_workspace;
            let workspaces = app.model.workspaces.clone();
            intent::execute_switch_workspace_by_delta(client, &workspaces, active, 1).await?;
            app.needs_reconcile = true;
        }
        BindingAction::PrevWorkspace => {
            let active = app.model.active_workspace;
            let workspaces = app.model.workspaces.clone();
            intent::execute_switch_workspace_by_delta(client, &workspaces, active, -1).await?;
            app.needs_reconcile = true;
        }
        BindingAction::FocusLeft
        | BindingAction::FocusRight
        | BindingAction::FocusUp
        | BindingAction::FocusDown
        | BindingAction::SplitRight
        | BindingAction::SplitDown
        | BindingAction::ZoomPane => {
            let intent = match action {
                BindingAction::FocusLeft => Intent::FocusLeft,
                BindingAction::FocusRight => Intent::FocusRight,
                BindingAction::FocusUp => Intent::FocusUp,
                BindingAction::FocusDown => Intent::FocusDown,
                BindingAction::SplitRight => Intent::SplitRight,
                BindingAction::SplitDown => Intent::SplitDown,
                BindingAction::ZoomPane => Intent::ZoomPane,
                _ => unreachable!("the outer arm names every intent action"),
            };
            intent::execute(client, intent, &pane).await?;
            app.needs_reconcile = true;
        }
        BindingAction::Detach => unreachable!("detach is handled before dispatch"),
    }
    Ok(())
}

async fn handle_dialog_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
) -> Result<InputOutcome, cyclops_tmux::TmuxError> {
    let Some(dialog) = app.dialog.as_ref() else {
        return Ok(InputOutcome::NoRedraw);
    };
    let action = dialog_key_action(dialog, &key);
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
            if let Some(buffer) = app.dialog.as_mut().and_then(dialog_buffer_mut) {
                buffer.pop();
            }
            if let Some(Dialog::NamePane { error, .. }) = app.dialog.as_mut() {
                *error = None;
            }
        }
        DialogKeyAction::Append(c) => {
            let mut encoded = [0; 4];
            append_dialog_text(app.dialog.as_mut(), c.encode_utf8(&mut encoded));
        }
        DialogKeyAction::Scroll(delta) => {
            if let Some(Dialog::Keybinds { scroll, .. }) = app.dialog.as_mut() {
                *scroll = move_keybind_scroll(*scroll, delta, max_scroll);
            }
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
    terminal
        .draw(|f| {
            let areas = app.chrome(f.area());
            if let Some(panel) = areas.panel {
                paint_event_stream(&app.event_lines, panel, f.buffer_mut(), &app.paint);
            }
            if let Some(sidebar) = areas.sidebar {
                paint_sidebar(
                    &app.model.workspaces,
                    app.model.active_workspace,
                    &app.model.active_tab().active_pane,
                    &app.expanded_workspaces,
                    &app.prefs.agent_order,
                    sidebar,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    &app.decoration,
                    app.hover,
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
                if let Some(pos) = cursor {
                    f.set_cursor_position(pos);
                }
            }
        })
        .map(|_| ())
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

    #[test]
    fn help_exits_zero_message() {
        assert_eq!(print_help_and_exit(), 0);
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

    #[test]
    fn automatic_tab_names_advance_numerically() {
        let node = crate::layout::parse_layout("0000,10x3,0,0,0").unwrap();
        let layout = crate::layout::resolve_layout(&node, &[]).unwrap();
        let tab = |name: &str| crate::model::TabModel {
            window_id: format!("@{name}"),
            name: name.into(),
            layout: layout.clone(),
            active_pane: "%0".into(),
            zoomed: false,
        };

        assert_eq!(next_numeric_tab_name(&[tab("1")]), "2");
        assert_eq!(next_numeric_tab_name(&[tab("1"), tab("notes")]), "2");
        assert_eq!(next_numeric_tab_name(&[tab("1"), tab("4")]), "5");
        assert_eq!(next_numeric_tab_name(&[tab("zsh")]), "2");
    }

    #[test]
    fn sidebar_resize_is_bounded_by_readability_and_half_the_terminal() {
        assert_eq!(clamp_sidebar_width(1, 200), SIDEBAR_MIN_WIDTH);
        assert_eq!(clamp_sidebar_width(100, 200), SIDEBAR_MAX_WIDTH);
        assert_eq!(clamp_sidebar_width(100, 50), 25);
        assert_eq!(sidebar_width_for_column(30, 50), 25);
    }

    #[test]
    fn workspace_disclosure_click_toggles_both_directions() {
        let mut expanded = HashSet::new();
        assert!(toggle_workspace_expanded(&mut expanded, "$0".into()));
        assert!(expanded.contains("$0"));
        assert!(!toggle_workspace_expanded(&mut expanded, "$0".into()));
        assert!(!expanded.contains("$0"));
    }

    #[test]
    fn sidebar_drag_order_moves_in_the_direction_of_the_drop() {
        let mut down = vec!["a".into(), "b".into(), "c".into()];
        assert!(move_to_index(&mut down, "a", "c"));
        assert_eq!(down, vec!["b", "c", "a"]);

        let mut up = vec!["a".into(), "b".into(), "c".into()];
        assert!(move_to_index(&mut up, "c", "a"));
        assert_eq!(up, vec!["c", "a", "b"]);
    }

    #[test]
    fn renamed_sidebar_entries_keep_their_persisted_position() {
        let mut order = vec!["alpha".into(), "beta".into(), "gamma".into()];
        assert!(migrate_order_entry(&mut order, "beta", "renamed"));
        assert_eq!(order, vec!["alpha", "renamed", "gamma"]);

        let mut stale = vec!["old".into(), "new".into()];
        assert!(migrate_order_entry(&mut stale, "old", "new"));
        assert_eq!(stale, vec!["new"]);
    }

    #[test]
    fn external_agent_rename_keeps_its_persisted_position() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let snapshot = |label: &str| {
            let mut snapshot = DecorationSnapshot::default();
            snapshot.panes.insert(
                "%7".into(),
                PaneDecoration {
                    pane_id: "%7".into(),
                    window_id: "@2".into(),
                    label: Some(label.into()),
                    manifest: Some("claude".into()),
                    manifest_display_name: Some("Claude Code".into()),
                    state: AgentState::Idle,
                    needs_attention: false,
                },
            );
            snapshot
        };
        let mut order = vec!["name:reviewer".into(), "name:implementer".into()];
        assert!(migrate_agent_order_entries(
            &mut order,
            &snapshot("reviewer"),
            &snapshot("auditor")
        ));
        assert_eq!(order, vec!["name:auditor", "name:implementer"]);
    }

    #[test]
    fn cancelling_a_sidebar_drag_restores_its_starting_width() {
        let mut drag = DragState::on_down(DragTarget::Sidebar, 27, 5);
        drag.on_move(38, 5);
        assert_eq!(sidebar_width_on_cancel(&drag, 100), Some(28));

        let tab = DragState::on_down(
            DragTarget::Tab {
                window_id: "@0".into(),
            },
            27,
            5,
        );
        assert_eq!(sidebar_width_on_cancel(&tab, 100), None);
    }

    #[test]
    fn escape_is_consumed_when_it_cancels_a_chrome_operation() {
        use crossterm::event::KeyCode;

        assert!(escape_cancels_visual_state(KeyCode::Esc, true, false));
        assert!(escape_cancels_visual_state(KeyCode::Esc, false, true));
        assert!(!escape_cancels_visual_state(KeyCode::Esc, false, false));
        assert!(!escape_cancels_visual_state(KeyCode::Char('x'), true, true));
    }

    #[test]
    fn keybind_scroll_moves_immediately_after_end_and_never_overshoots() {
        assert_eq!(move_keybind_scroll(4, 20, 10), 10);
        assert_eq!(move_keybind_scroll(10, -1, 10), 9);
        assert_eq!(move_keybind_scroll(0, -8, 10), 0);
    }

    #[test]
    fn enter_confirms_a_destructive_dialog() {
        let dialog = Dialog::ConfirmCloseTab {
            window_id: "@1".into(),
        };
        let enter = KeyEvent::new(crossterm::event::KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(dialog_key_action(&dialog, &enter), DialogKeyAction::Confirm);
    }

    #[test]
    fn enter_submits_an_input_dialog() {
        let dialog = Dialog::NewTab {
            buffer: "review".into(),
        };
        let enter = KeyEvent::new(crossterm::event::KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(dialog_key_action(&dialog, &enter), DialogKeyAction::Confirm);
    }

    #[test]
    fn modified_characters_do_not_leak_into_dialog_text() {
        let dialog = Dialog::NewTab {
            buffer: String::new(),
        };
        let control_c = KeyEvent::new(crossterm::event::KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            dialog_key_action(&dialog, &control_c),
            DialogKeyAction::Ignore
        );
    }

    #[test]
    fn dialog_paste_keeps_text_and_drops_line_controls() {
        let mut dialog = Dialog::NewTab {
            buffer: "review".into(),
        };
        assert!(append_dialog_text(Some(&mut dialog), "-api\n\t"));
        assert_eq!(
            dialog,
            Dialog::NewTab {
                buffer: "review-api".into()
            }
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
        let row = |id: &str, name: &str, active: bool| crate::model::WorkspaceRow {
            session_id: id.into(),
            name: name.into(),
            tab_count: 1,
            active,
            window_ids: vec!["@1".into()],
        };
        let mut model = WorkspaceModel {
            workspaces: vec![row("$0", "alpha", false), row("$1", "beta", true)],
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
            active: false,
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

    #[test]
    fn chrome_canvas_excludes_sidebar_and_tab_bar() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), true, 22, false);
        assert_eq!(areas.sidebar, Some(Rect::new(0, 0, 22, 50)));
        assert_eq!(areas.tab_bar, Rect::new(22, 0, 178, 1));
        assert_eq!(areas.canvas, Rect::new(22, 1, 178, 49));
        assert_eq!(areas.panel, None);
    }

    #[test]
    fn chrome_canvas_shrinks_for_event_stream() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), true, 22, true);
        assert_eq!(areas.panel, Some(Rect::new(160, 0, 40, 50)));
        assert_eq!(areas.canvas, Rect::new(22, 1, 138, 49));
    }
}
