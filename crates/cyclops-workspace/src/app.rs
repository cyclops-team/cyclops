//! Workspace application state and event loop.
//!
//! The loop is event-armed: every message arms one render debounce
//! (`RECONCILE_DEBOUNCE`) if none is pending — arming never pushes an
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
    self, Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use cyclops_tmux::{quote_arg, ControlClient, ControlConfig, Notification};
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
use crate::model::{visible_pane_ids, RuntimeRegistry, WorkspaceModel};
use crate::persist::{self, load_prefs, set_last_active, WorkspacePrefs};
use crate::render::{
    paint_dialog, paint_event_panel, paint_menu, paint_sidebar, paint_tab_bar, paint_window,
};
use crate::resilience::{self, LinkState};
use crate::selection::{self, SelectionState};
use crate::sync::{fetch_workspace_model, hydrate_visible_tab};
use crate::term_guard::TermGuard;
use crate::theme::Paint;

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(30);
const TAB_BAR_HEIGHT: u16 = 1;
const EVENT_PANEL_WIDTH: u16 = 40;

enum AppMsg {
    Input(KeyEvent),
    Mouse(MouseEvent),
    Output {
        pane: String,
        bytes: Vec<u8>,
    },
    Redraw,
    Resized(u16, u16),
    Reconcile,
    SessionSwitched(String),
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
    selection: SelectionState,
    drag: Option<DragState>,
    decoration: DecorationSnapshot,
    prefs: WorkspacePrefs,
    event_panel_open: bool,
    event_lines: Vec<String>,
    term_size: (u16, u16),
    needs_reconcile: bool,
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
        *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
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
    let boot_canvas = chrome_areas_for(
        Rect::new(0, 0, term_size.0, term_size.1),
        prefs.sidebar_visible,
        prefs.sidebar_width.max(8),
        false,
    )
    .canvas;
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
        selection: SelectionState::default(),
        drag: None,
        decoration: decoration::fetch_decoration(&home),
        prefs: prefs.clone(),
        event_panel_open: false,
        event_lines: Vec::new(),
        term_size,
        needs_reconcile: false,
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
        if let Some(deadline) = next_deadline {
            tokio::select! {
                biased;
                msg = rx.recv() => {
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
                _ = sleep_until(deadline) => {
                    if debounce.is_some_and(|d| d <= deadline) {
                        debounce = None;
                        if app.needs_reconcile {
                            app.needs_reconcile = false;
                            if let Err(e) = reconcile(&mut app, &client).await {
                                log_err(&app.home, &e);
                            }
                        }
                        let _ = draw(&mut terminal, &mut app);
                    }
                    if reconnect_deadline.is_some_and(|d| d <= deadline) {
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
        } else {
            let Some(msg) = rx.recv().await else { break };
            if !handle_app_msg(
                Some(msg),
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
        while let Some(n) = rx.recv().await {
            match n {
                Notification::Output { pane, data }
                | Notification::ExtendedOutput { pane, data, .. } => {
                    let _ = tx.send(AppMsg::Output { pane, bytes: data });
                }
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
                Notification::SessionChanged { name, .. }
                | Notification::SessionRenamed { name } => {
                    let _ = tx.send(AppMsg::SessionSwitched(name));
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
    match ControlClient::spawn(cfg.clone()).await {
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

impl App {
    fn is_visible_pane(&self, pane: &str) -> bool {
        visible_pane_ids(self.model.active_tab())
            .iter()
            .any(|id| id == pane)
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

enum DetachOutcome {
    Detached,
    Continue,
}

async fn resize_client(app: &App, client: &ControlClient) {
    let (w, h) = app.term_size;
    let canvas = app.chrome(Rect::new(0, 0, w, h)).canvas;
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
            resize_client(app, client).await;
            arm(debounce);
        }
        AppMsg::Reconcile => {
            app.needs_reconcile = true;
            arm(debounce);
        }
        AppMsg::SessionSwitched(name) => {
            app.model.session.session = name;
            app.needs_reconcile = true;
            arm(debounce);
        }
        AppMsg::LayoutChanged {
            window,
            layout,
            flags,
        } => {
            if apply_layout_change(app, &window, &layout, flags.as_deref()) {
                if app.model.active_tab().window_id == window {
                    if let Err(e) =
                        hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await
                    {
                        log_err(&app.home, &e);
                    }
                }
            } else {
                app.needs_reconcile = true;
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
        AppMsg::Output { pane, bytes } => {
            if app.is_visible_pane(&pane) {
                if let Some(rt) = app.runtimes.get_mut(&pane) {
                    rt.feed(&bytes);
                }
            }
            arm(debounce);
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
                if let Err(e) =
                    hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await
                {
                    log_err(&app.home, &e);
                }
            }
            arm(debounce);
        }
        AppMsg::Mouse(mouse) => {
            if let Err(e) = handle_mouse(app, client, mouse, detached).await {
                log_err(&app.home, &e);
            }
            arm(debounce);
        }
        AppMsg::Input(key) => {
            if app.link_state == LinkState::ServerGone {
                return true;
            }
            if app.menu.is_open() {
                // Any key dismisses an open menu and is consumed by it.
                app.menu.close();
                arm(debounce);
                return true;
            }
            if key.code == crossterm::event::KeyCode::Esc {
                app.selection.cancel_drag();
                app.drag = None;
            }
            match handle_key(app, client, key).await {
                Ok(DetachOutcome::Detached) => *detached = true,
                Ok(DetachOutcome::Continue) => arm(debounce),
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
        MouseEventKind::Down(MouseButton::Right) => {
            if let Some(HitTarget::PaneBody { pane_id }) = app.hit_map.hit(col, row).cloned() {
                focus_pane(app, client, &pane_id).await?;
                app.menu = MenuState::ContextMenu {
                    pane_id,
                    at: (col, row),
                };
            } else {
                app.menu.close();
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = app.hit_map.hit(col, row).cloned() else {
                app.menu.close();
                app.selection.clear();
                return Ok(());
            };
            match target {
                HitTarget::MenuItem { action } => {
                    app.menu.close();
                    if action == BindingAction::Detach {
                        *detached = true;
                        return Ok(());
                    }
                    dispatch_action(app, client, action).await?;
                }
                HitTarget::PaneBody { pane_id } => {
                    app.menu.close();
                    let target = HitTarget::PaneBody {
                        pane_id: pane_id.clone(),
                    };
                    let clicks = app.selection.register_click(&target, col, row);
                    if let Some(geom) = app.hit_map.pane_geometry(&pane_id) {
                        if let Some(cell) = crate::input::mouse::HitMap::cell_at(geom, col, row) {
                            match clicks {
                                2 => {
                                    if let Some(rt) = app.runtimes.get(&pane_id) {
                                        app.selection.set_word(pane_id.clone(), cell, &rt.grid());
                                    }
                                    copy_active_selection(app);
                                }
                                3 => {
                                    app.selection.set_line(pane_id.clone(), cell.row, geom.cols);
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
                    app.menu.close();
                    intent::execute(client, Intent::SplitRight, &pane_id).await?;
                    app.needs_reconcile = true;
                }
                HitTarget::PaneSplitDown { pane_id } => {
                    app.menu.close();
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
                HitTarget::Tab { index } => {
                    app.menu.close();
                    app.selection.clear();
                    // Down starts a possible reorder drag; a below-threshold
                    // release selects the tab instead.
                    app.drag = Some(DragState::on_down(DragTarget::Tab { index }, col, row));
                }
                HitTarget::NewTabButton => {
                    app.menu.close();
                    new_tab(app, client).await?;
                }
                HitTarget::SidebarRow { index } => {
                    app.menu.close();
                    if let Some(ws) = app.model.workspaces.get(index) {
                        intent::execute(client, Intent::SwitchWorkspace(ws.name.clone()), "")
                            .await?;
                        app.needs_reconcile = true;
                    }
                }
                HitTarget::AttentionIndicator { pane_id } => {
                    app.menu.close();
                    focus_pane(app, client, &pane_id).await?;
                }
                HitTarget::AppMenu => {
                    app.menu = if app.menu == MenuState::AppMenu {
                        MenuState::None
                    } else {
                        MenuState::AppMenu
                    };
                }
                HitTarget::PaneBorder { .. } => {}
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.drag.is_some() {
                if let Some(drag) = app.drag.as_mut() {
                    drag.on_move(col, row);
                }
                apply_live_divider(app, client).await?;
            } else if let Some(anchor) = app.selection.anchor_pane().map(str::to_string) {
                if let Some(geom) = app.hit_map.pane_geometry(&anchor) {
                    if let Some(cell) = crate::input::mouse::HitMap::cell_at(geom, col, row) {
                        app.selection.drag_to(&anchor, cell);
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(mut drag) = app.drag.take() {
                match drag.on_up() {
                    Some(DragTarget::Tab { index }) => {
                        commit_tab_drop(app, client, index, col, row).await?;
                    }
                    // Divider drags applied live during motion.
                    Some(_) => {}
                    None => {
                        if let DragTarget::Tab { index } = drag.target {
                            select_tab(app, client, index).await?;
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
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(drag) = app.drag.as_mut() else {
        return Ok(());
    };
    if !drag.is_active() {
        return Ok(());
    }
    let DragTarget::Divider { pane_id, dir } = drag.target.clone() else {
        return Ok(());
    };
    let delta = match dir {
        SplitDir::Horizontal => drag.current.0 as i32 - drag.last_applied.0 as i32,
        SplitDir::Vertical => drag.current.1 as i32 - drag.last_applied.1 as i32,
    };
    if delta == 0 {
        return Ok(());
    }
    drag.last_applied = drag.current;
    intent::resize_divider(client, &pane_id, dir, delta).await
}

/// Commit a tab drag: drop on another tab reorders, drop on a sidebar row
/// moves the window to that workspace.
async fn commit_tab_drop(
    app: &mut App,
    client: &ControlClient,
    index: usize,
    col: u16,
    row: u16,
) -> Result<(), cyclops_tmux::TmuxError> {
    let Some(src) = app
        .model
        .session
        .tabs
        .get(index)
        .map(|t| t.window_id.clone())
    else {
        return Ok(());
    };
    match app.hit_map.hit(col, row).cloned() {
        Some(HitTarget::Tab { index: j }) if j != index => {
            if let Some(dst) = app.model.session.tabs.get(j).map(|t| t.window_id.clone()) {
                client
                    .command(&format!(
                        "swap-window -s {} -t {}",
                        quote_arg(&src),
                        quote_arg(&dst)
                    ))
                    .await?;
                app.needs_reconcile = true;
            }
        }
        Some(HitTarget::SidebarRow { index: w }) => {
            if let Some(ws) = app.model.workspaces.get(w) {
                client
                    .command(&format!(
                        "move-window -s {} -t {}",
                        quote_arg(&src),
                        quote_arg(&format!("{}:", ws.name))
                    ))
                    .await?;
                app.needs_reconcile = true;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn new_tab(app: &mut App, client: &ControlClient) -> Result<(), cyclops_tmux::TmuxError> {
    let pane = app.model.active_tab().active_pane.clone();
    let cwd = client
        .display(&pane, "#{pane_current_path}")
        .await
        .map(|p| p.trim().to_string())
        .ok();
    intent::execute_new_tab(client, cwd.as_deref()).await?;
    app.needs_reconcile = true;
    Ok(())
}

async fn reconcile(app: &mut App, client: &ControlClient) -> Result<(), cyclops_tmux::TmuxError> {
    let active_window = app.model.active_tab().window_id.clone();
    let session = app.model.session.session.clone();
    let model = fetch_workspace_model(&session, app.socket.as_deref())?;
    let active_tab = model
        .session
        .tabs
        .iter()
        .position(|t| t.window_id == active_window)
        .unwrap_or(model.session.active_tab);
    app.model = model;
    app.model.session.active_tab = active_tab;
    app.model.sidebar_visible = app.prefs.sidebar_visible;
    app.decoration = decoration::fetch_decoration(&app.home);
    app.refresh_event_lines();
    app.persist_active();
    hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await
}

async fn handle_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
) -> Result<DetachOutcome, cyclops_tmux::TmuxError> {
    if let Some(dialog) = app.dialog.clone() {
        return handle_dialog_key(app, client, key, dialog).await;
    }
    match app.router.route(key) {
        RouterResult::PrefixArmed => Ok(DetachOutcome::Continue),
        RouterResult::Consumed => Ok(DetachOutcome::Continue),
        RouterResult::Action(BindingAction::Detach) => Ok(DetachOutcome::Detached),
        RouterResult::Action(action) => {
            dispatch_action(app, client, action).await?;
            Ok(DetachOutcome::Continue)
        }
        RouterResult::PassThrough(key) => {
            let pane = app.model.active_tab().active_pane.clone();
            let encoded = encode_send_keys(&key);
            for key_arg in &encoded {
                client.send_keys(&pane, &[key_arg.as_str()]).await?;
            }
            Ok(DetachOutcome::Continue)
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
            let home = cyclops_proto::cyclops_home();
            if pane_has_agent(&home, &pane) {
                app.dialog = Some(Dialog::confirm_close(&pane));
                return Ok(());
            }
            intent::execute(client, Intent::ClosePane, &pane).await?;
            app.needs_reconcile = true;
        }
        BindingAction::NewTab => {
            new_tab(app, client).await?;
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
            app.dialog = Some(Dialog::RenameTab {
                buffer: String::new(),
            });
            return Ok(());
        }
        BindingAction::NewWorkspace => {
            app.dialog = Some(Dialog::NewWorkspace {
                buffer: String::new(),
            });
            return Ok(());
        }
        BindingAction::RenameWorkspace => {
            app.dialog = Some(Dialog::RenameWorkspace {
                buffer: String::new(),
            });
            return Ok(());
        }
        BindingAction::CloseWorkspace => {
            app.dialog = Some(Dialog::ConfirmCloseWorkspace);
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
        other => {
            intent::execute(client, Intent::from(other), &pane).await?;
            app.needs_reconcile = true;
        }
    }
    Ok(())
}

async fn handle_dialog_key(
    app: &mut App,
    client: &ControlClient,
    key: KeyEvent,
    dialog: Dialog,
) -> Result<DetachOutcome, cyclops_tmux::TmuxError> {
    use crossterm::event::KeyCode;
    if key.code == KeyCode::Esc {
        app.dialog = None;
        app.selection.cancel_drag();
        app.drag = None;
        return Ok(DetachOutcome::Continue);
    }
    match dialog {
        Dialog::ConfirmClosePane { pane_id } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.dialog = None;
                intent::execute(client, Intent::ClosePane, &pane_id).await?;
                app.needs_reconcile = true;
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.dialog = None;
            }
            _ => {}
        },
        Dialog::RenameTab { mut buffer } => match key.code {
            KeyCode::Esc => app.dialog = None,
            KeyCode::Enter => {
                app.dialog = None;
                if !buffer.is_empty() {
                    intent::execute_rename(client, &buffer).await?;
                    app.needs_reconcile = true;
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                app.dialog = Some(Dialog::RenameTab { buffer });
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                app.dialog = Some(Dialog::RenameTab { buffer });
            }
            _ => {}
        },
        Dialog::NewWorkspace { mut buffer } => match key.code {
            KeyCode::Esc => app.dialog = None,
            KeyCode::Enter => {
                app.dialog = None;
                if !buffer.is_empty() {
                    let path = std::path::PathBuf::from(&buffer);
                    intent::execute_new_workspace(client, &path).await?;
                    app.needs_reconcile = true;
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                app.dialog = Some(Dialog::NewWorkspace { buffer });
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                app.dialog = Some(Dialog::NewWorkspace { buffer });
            }
            _ => {}
        },
        Dialog::RenameWorkspace { mut buffer } => match key.code {
            KeyCode::Esc => app.dialog = None,
            KeyCode::Enter => {
                app.dialog = None;
                if !buffer.is_empty() {
                    intent::execute_rename_workspace(client, &buffer).await?;
                    app.needs_reconcile = true;
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                app.dialog = Some(Dialog::RenameWorkspace { buffer });
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                app.dialog = Some(Dialog::RenameWorkspace { buffer });
            }
            _ => {}
        },
        Dialog::ConfirmCloseWorkspace => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.dialog = None;
                intent::execute_close_workspace(client).await?;
                app.needs_reconcile = true;
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                app.dialog = None;
            }
            _ => {}
        },
    }
    Ok(DetachOutcome::Continue)
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
            );
            if let Some(dialog) = &app.dialog {
                paint_dialog(dialog, f.area(), f.buffer_mut(), &app.paint);
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
