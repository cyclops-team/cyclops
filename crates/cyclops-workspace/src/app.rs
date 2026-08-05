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
    paint_dialog, paint_event_panel, paint_menu, paint_sidebar, paint_tab_bar, paint_window,
};
use crate::resilience::{self, LinkState};
use crate::selection::{self, SelectionState};
use crate::sync::{fetch_workspace_model, hydrate_visible_tab};
use crate::term_guard::TermGuard;
use crate::theme::Paint;

/// At most one frame per 8 ms (~120 Hz). The timer exists only after an
/// event; idle workspaces still have no wakeups.
const RENDER_DEBOUNCE: Duration = Duration::from_millis(8);
const TAB_BAR_HEIGHT: u16 = 1;
const EVENT_PANEL_WIDTH: u16 = 40;

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
    event_panel_open: bool,
    event_lines: Vec<String>,
    term_size: (u16, u16),
    needs_reconcile: bool,
    /// A structural notification changed visible pane dimensions. Hydration
    /// waits for the render deadline so resize bursts collapse to one set of
    /// captures instead of blocking the input path for every intermediate.
    needs_hydrate: bool,
    paste_seq: u64,
    home: std::path::PathBuf,
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
        let w = sidebar_width.clamp(8, main.width / 2);
        let s = Rect::new(main.x, main.y, w, main.height);
        main = Rect::new(main.x + w, main.y, main.width - w, main.height);
        Some(s)
    } else {
        None
    };
    let panel = if panel_open && main.width > EVENT_PANEL_WIDTH + 4 {
        let p = Rect::new(
            main.x + main.width - EVENT_PANEL_WIDTH,
            main.y,
            EVENT_PANEL_WIDTH,
            main.height,
        );
        main = Rect::new(main.x, main.y, main.width - EVENT_PANEL_WIDTH, main.height);
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
        ControlConfig::new_session(&session)
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

    // Declare the pane canvas to tmux before hydrating, so the grids the
    // captures describe are the grids we render.
    let term_size = crossterm::terminal::size().unwrap_or((80, 24));
    let boot_canvas = crate::render::pane_canvas(
        chrome_areas_for(
            Rect::new(0, 0, term_size.0, term_size.1),
            prefs.sidebar_visible,
            prefs.sidebar_width.max(8),
            false,
        )
        .canvas,
    );
    if boot_canvas.width >= 10 && boot_canvas.height >= 3 {
        if let Err(e) = client
            .set_client_size(boot_canvas.width, boot_canvas.height)
            .await
        {
            log_err(&home, &e);
        }
    }

    let model = match fetch_workspace_model(&session, socket) {
        Ok(m) => m,
        Err(e) => {
            drop(guard);
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };
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
        decoration: decoration::fetch_decoration(&home),
        prefs: prefs.clone(),
        event_panel_open: false,
        event_lines: Vec::new(),
        term_size,
        needs_reconcile: false,
        needs_hydrate: false,
        paste_seq: 0,
        home,
    };
    app.model.sidebar_visible = prefs.sidebar_visible;
    if let persist::ReopenTarget::LastActive { window_id, .. } = &reopen {
        if let Some(idx) = app
            .model
            .session
            .tabs
            .iter()
            .position(|t| t.window_id == *window_id)
        {
            app.model.session.active_tab = idx;
            let _ = hydrate_visible_tab(&client, app.model.active_tab(), &mut app.runtimes).await;
        }
    }
    app.refresh_event_lines();

    let mut debounce: Option<Instant> = None;
    let mut reconnect_deadline: Option<Instant> = None;
    let mut detached = false;
    let _ = draw(&mut terminal, &mut app);
    while !detached {
        let next_deadline = match (debounce, reconnect_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
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
        self.prefs.sidebar_width.max(8)
    }

    fn chrome(&self, area: Rect) -> ChromeAreas {
        chrome_areas_for(
            area,
            self.model.sidebar_visible,
            self.sidebar_width(),
            self.event_panel_open,
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

async fn resize_client(app: &App, client: &ControlClient) {
    let (w, h) = app.term_size;
    // The margin around the pane canvas is chrome, not pane cells; tmux
    // gets the inset size so painted grids stay 1:1.
    let canvas = crate::render::pane_canvas(app.chrome(Rect::new(0, 0, w, h)).canvas);
    if canvas.width >= 10 && canvas.height >= 3 {
        if let Err(e) = client.set_client_size(canvas.width, canvas.height).await {
            log_err(&app.home, &e);
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
        AppMsg::Mouse(mouse) => {
            // Bare motion only matters while a menu or dialog shows hover
            // highlights; everywhere else it must not wake the renderer.
            if matches!(mouse.kind, MouseEventKind::Moved)
                && !app.menu.is_open()
                && app.dialog.is_none()
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
            let cleared_visual_state = key.code == crossterm::event::KeyCode::Esc
                && (app.selection.active.is_some() || app.drag.is_some());
            if key.code == crossterm::event::KeyCode::Esc {
                app.selection.cancel_drag();
                app.drag = None;
            }
            match handle_key(app, client, key).await {
                Ok(InputOutcome::Detached) => *detached = true,
                Ok(InputOutcome::Redraw) => arm(debounce),
                Ok(InputOutcome::NoRedraw) if cleared_visual_state => arm(debounce),
                Ok(InputOutcome::NoRedraw) => {}
                Err(e) => log_err(&app.home, &e),
            }
        }
    }
    true
}

/// Focus a pane: tell tmux, and mirror the reply into the model so the
/// highlight moves this frame (the `%window-pane-changed` notification
/// confirms it a moment later).
async fn focus_pane(
    app: &mut App,
    client: &ControlClient,
    pane_id: &str,
) -> Result<(), cyclops_tmux::TmuxError> {
    client
        .command(&format!("select-pane -t {}", quote_arg(pane_id)))
        .await?;
    let idx = app.model.session.active_tab;
    if let Some(tab) = app.model.session.tabs.get_mut(idx) {
        tab.active_pane = pane_id.to_string();
    }
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
            Some(HitTarget::PaneBody { pane_id }) => {
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
            Some(HitTarget::SidebarRow { session }) => {
                app.open_menu(MenuState::WorkspaceMenu {
                    session,
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
                                    if let Some(rt) = app.runtimes.get_mut(&pane_id) {
                                        app.selection.set_word(pane_id.clone(), cell, &rt.grid());
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
                HitTarget::SidebarRow { session } => {
                    app.close_menu();
                    intent::execute(client, Intent::SwitchWorkspace(session), "").await?;
                    app.needs_reconcile = true;
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
                HitTarget::DialogConfirm | HitTarget::DialogCancel => {}
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.drag.is_some() {
                if let Some(drag) = app.drag.as_mut() {
                    drag.on_move(col, row);
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
            apply_live_divider(app, client).await?;
            if let Some(drag) = app.drag.take() {
                match drag.on_up() {
                    Some(DragTarget::Tab { window_id }) => {
                        commit_tab_drop(app, client, &window_id, col, row).await?;
                    }
                    // Divider drags applied live during motion.
                    Some(_) => {}
                    None => {
                        if let DragTarget::Tab { window_id } = drag.target {
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
                    }
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
        Some(HitTarget::SidebarRow { session }) => {
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
    intent::execute_new_tab(client, cwd.as_deref(), name.filter(|name| !name.is_empty())).await?;
    app.needs_reconcile = true;
    Ok(())
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
    intent::execute_new_workspace(client, &folder, &taken).await?;
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

/// Apply the open dialog's action (Enter, `y`, or its confirm button).
async fn dialog_confirm(
    app: &mut App,
    client: &ControlClient,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(dialog) = app.dialog.take() else {
        return Ok(());
    };
    app.hover = None;
    match dialog {
        Dialog::ConfirmClosePane { pane_id } => {
            intent::execute(client, Intent::ClosePane, &pane_id).await?;
            app.needs_reconcile = true;
        }
        Dialog::NewTab { buffer } => {
            new_tab(app, client, Some(buffer.trim())).await?;
        }
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
                intent::execute_rename_workspace(client, &session, buffer.trim()).await?;
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
            intent::execute_close_workspace(client, &session, fallback.as_deref()).await?;
            app.needs_reconcile = true;
        }
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
        | Dialog::RenameTab { buffer, .. }
        | Dialog::RenameWorkspace { buffer, .. } => Some(buffer),
        _ => None,
    }
}

/// Add printable pasted text to an input dialog. Line controls belong to a
/// pane paste, never to a tmux tab or session name.
fn append_dialog_text(dialog: Option<&mut Dialog>, text: &str) -> bool {
    let Some(buffer) = dialog.and_then(dialog_buffer_mut) else {
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
    Ignore,
}

/// Resolve dialog keys without mutating application state. Input dialogs
/// submit on Enter; destructive `[y/N]` confirms keep No as the Enter
/// default, so an accidental return key can never delete anything.
fn dialog_key_action(dialog: &Dialog, key: &KeyEvent) -> DialogKeyAction {
    use crossterm::event::KeyCode;

    let text_key = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Esc => DialogKeyAction::Cancel,
        KeyCode::Enter if dialog.has_input() => DialogKeyAction::Confirm,
        KeyCode::Enter => DialogKeyAction::Cancel,
        KeyCode::Backspace if dialog.has_input() => DialogKeyAction::Backspace,
        KeyCode::Char(c) if dialog.has_input() && text_key => DialogKeyAction::Append(c),
        KeyCode::Char('y' | 'Y') if text_key => DialogKeyAction::Confirm,
        KeyCode::Char('n' | 'N') if text_key => DialogKeyAction::Cancel,
        _ => DialogKeyAction::Ignore,
    }
}

async fn reconcile(app: &mut App, client: &ControlClient) -> Result<(), cyclops_tmux::TmuxError> {
    let session = app.model.session.session.clone();
    let model = fetch_workspace_model(&session, app.socket.as_deref())?;
    install_reconciled_model(&mut app.model, model, app.prefs.sidebar_visible);
    app.decoration = decoration::fetch_decoration(&app.home);
    app.refresh_event_lines();
    app.persist_active();
    hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await?;
    app.needs_hydrate = false;
    Ok(())
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
            for key_arg in &encoded {
                client.send_keys(&pane, &[key_arg.as_str()]).await?;
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
            app.event_panel_open = !app.event_panel_open;
            resize_client(app, client).await;
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
    match action {
        DialogKeyAction::Cancel => {
            dialog_cancel(app);
            if key.code == crossterm::event::KeyCode::Esc {
                app.selection.cancel_drag();
                app.drag = None;
            }
        }
        DialogKeyAction::Confirm => dialog_confirm(app, client).await?,
        DialogKeyAction::Backspace => {
            if let Some(buffer) = app.dialog.as_mut().and_then(dialog_buffer_mut) {
                buffer.pop();
            }
        }
        DialogKeyAction::Append(c) => {
            let mut encoded = [0; 4];
            append_dialog_text(app.dialog.as_mut(), c.encode_utf8(&mut encoded));
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
                paint_event_panel(&app.event_lines, panel, f.buffer_mut(), &app.paint);
            }
            if let Some(sidebar) = areas.sidebar {
                paint_sidebar(
                    &app.model.workspaces,
                    app.model.active_workspace,
                    sidebar,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                    &app.decoration,
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
                &mut app.runtimes,
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
    fn enter_never_confirms_a_destructive_dialog() {
        let dialog = Dialog::ConfirmCloseTab {
            window_id: "@1".into(),
        };
        let enter = KeyEvent::new(crossterm::event::KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(dialog_key_action(&dialog, &enter), DialogKeyAction::Cancel);
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
    fn chrome_canvas_excludes_sidebar_and_tab_bar() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), true, 20, false);
        assert_eq!(areas.sidebar, Some(Rect::new(0, 0, 20, 50)));
        assert_eq!(areas.tab_bar, Rect::new(20, 0, 180, 1));
        assert_eq!(areas.canvas, Rect::new(20, 1, 180, 49));
        assert_eq!(areas.panel, None);
    }

    #[test]
    fn chrome_canvas_shrinks_for_event_panel() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), true, 20, true);
        assert_eq!(areas.panel, Some(Rect::new(160, 0, 40, 50)));
        assert_eq!(areas.canvas, Rect::new(20, 1, 140, 49));
    }
}
