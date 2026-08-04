//! Workspace application state and event loop.

use std::collections::HashSet;
use std::io;

use crossterm::event::{self, Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use cyclops_tmux::{list_sessions, ControlClient, ControlConfig, Notification};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Duration, Instant};

use crate::config::load_tmux_config;
use crate::daemon::pane_has_agent;
use crate::dialog::Dialog;
use crate::intent::{self, Intent};
use crate::bindings::{load_bindings, BindingAction};
use crate::copy;
use crate::input::mouse::{HitMap, HitTarget, MenuState};
use crate::input::encode_send_keys;
use crate::input::router::{Router, RouterResult};
use crate::model::{RuntimeRegistry, WorkspaceModel};
use crate::render::{paint_dialog, paint_sidebar, paint_tab_bar, paint_window};
use crate::resilience::{self, LinkState};
use crate::sync::{fetch_workspace_model, hydrate_visible_tab};
use crate::term_guard::TermGuard;
use crate::theme::Paint;

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(30);
const TAB_BAR_HEIGHT: u16 = 1;
const SIDEBAR_WIDTH: u16 = 20;

enum AppMsg {
    Input(KeyEvent),
    Mouse(MouseEvent),
    Output { pane: String, bytes: Vec<u8> },
    Redraw,
    Reconcile,
    LinkLost,
    PanePaused { pane: String },
    PaneContinued { pane: String },
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
}

/// Run the workspace on a tty. Returns the process exit code.
pub async fn run_async() -> i32 {
    let tmux_cfg = load_tmux_config(&cyclops_proto::cyclops_home());
    let socket_name = tmux_cfg.socket.clone();
    let socket = socket_name.as_deref();
    let sessions = match list_sessions(socket) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{}", copy::NO_TMUX_SERVER);
            return 0;
        }
    };
    let Some(session) = sessions
        .iter()
        .find(|s| s.attached)
        .or(sessions.first())
        .map(|s| s.name.clone())
    else {
        eprintln!("{}", copy::NO_TMUX_SERVER);
        return 0;
    };

    let mut cfg = ControlConfig::attach(&session);
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

    let model = match fetch_workspace_model(&session, socket) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };
    let mut runtimes = RuntimeRegistry::default();
    if let Err(e) = hydrate_visible_tab(&client, model.active_tab(), &mut runtimes).await {
        eprintln!("{e}");
        client.shutdown().await;
        return 1;
    }

    let bindings = load_bindings(&cyclops_proto::cyclops_home());
    let (tx, mut rx) = mpsc::unbounded_channel::<AppMsg>();

    let input_tx = tx.clone();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(k)) => {
                if input_tx.send(AppMsg::Input(k)).is_err() {
                    break;
                }
            }
            Ok(Event::Mouse(m)) => {
                if input_tx.send(AppMsg::Mouse(m)).is_err() {
                    break;
                }
            }
            Ok(Event::Resize(_, _)) => {
                let _ = input_tx.send(AppMsg::Redraw);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    spawn_notif_forwarder(notif_rx, tx.clone());

    let guard = match TermGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };

    let size = crossterm::terminal::size().unwrap_or((80, 24));
    if size.0 >= 20 && size.1 >= 8 {
        if let Err(e) = client.set_client_size(size.0, size.1).await {
            eprintln!("{e}");
        }
    }

    let mut terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            drop(guard);
            client.shutdown().await;
            return 1;
        }
    };

    let mut app = App {
        model,
        runtimes,
        router: Router::new(bindings),
        paint: Paint::detect(),
        socket: socket_name,
        dialog: None,
        link_state: LinkState::Live,
        paused_panes: HashSet::new(),
        reconnect_attempt: 0,
        hit_map: HitMap::default(),
        menu: MenuState::None,
    };

    let mut debounce: Option<Instant> = None;
    let mut reconnect_deadline: Option<Instant> = None;
    let mut detached = false;
    let _ = draw(&mut terminal, &mut app);
    while !detached {
        if let Some(deadline) = debounce.or(reconnect_deadline) {
            tokio::select! {
                biased;
                msg = rx.recv() => {
                    if !handle_app_msg(
                        msg,
                        &mut app,
                        &mut client,
                        &control_cfg,
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
                    if debounce == Some(deadline) {
                        debounce = None;
                        let _ = draw(&mut terminal, &mut app);
                    } else if reconnect_deadline == Some(deadline) {
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
                &control_cfg,
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
                Notification::LayoutChange { .. }
                | Notification::WindowAdd { .. }
                | Notification::WindowClose { .. }
                | Notification::WindowRenamed { .. }
                | Notification::WindowPaneChanged { .. }
                | Notification::SessionsChanged
                | Notification::SessionRenamed { .. } => {
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

fn schedule_reconnect(
    app: &mut App,
    reconnect_deadline: &mut Option<Instant>,
) {
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
        let tab = self.model.active_tab();
        crate::layout::pane_ids_in_layout(&tab.layout)
            .iter()
            .any(|id| id == pane)
    }
}

enum DetachOutcome {
    Detached,
    Continue,
}

/// Handle one app message. Returns false when the channel closed.
async fn handle_app_msg(
    msg: Option<AppMsg>,
    app: &mut App,
    client: &mut ControlClient,
    control_cfg: &ControlConfig,
    _tx: &mpsc::UnboundedSender<AppMsg>,
    debounce: &mut Option<Instant>,
    reconnect_deadline: &mut Option<Instant>,
    detached: &mut bool,
) -> bool {
    let Some(msg) = msg else {
        return false;
    };
    match msg {
        AppMsg::Redraw => *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE),
        AppMsg::Reconcile => {
            if let Err(e) = reconcile(app, client).await {
                eprintln!("{e}");
            }
            *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
        }
        AppMsg::Output { pane, bytes } => {
            if app.is_visible_pane(&pane) {
                if let Some(rt) = app.runtimes.get_mut(&pane) {
                    rt.feed(&bytes);
                }
            }
            *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
        }
        AppMsg::LinkLost => {
            app.reconnect_attempt = 0;
            schedule_reconnect(app, reconnect_deadline);
            *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
        }
        AppMsg::PanePaused { pane } => {
            app.paused_panes.insert(pane);
            *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
        }
        AppMsg::PaneContinued { pane } => {
            app.paused_panes.remove(&pane);
            if app.is_visible_pane(&pane) {
                if let Err(e) = hydrate_visible_tab(client, app.model.active_tab(), &mut app.runtimes).await {
                    eprintln!("{e}");
                }
            }
            *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
        }
        AppMsg::Mouse(mouse) => {
            if let Err(e) = handle_mouse(app, client, mouse).await {
                eprintln!("{e}");
            }
            *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
        }
        AppMsg::Input(key) => {
            if app.link_state == LinkState::ServerGone {
                return true;
            }
            app.menu.close();
            match handle_key(app, client, key).await {
                Ok(DetachOutcome::Detached) => *detached = true,
                Ok(DetachOutcome::Continue) => {
                    *debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
                }
                Err(e) => eprintln!("{e}"),
            }
        }
    }
    true
}

async fn handle_mouse(
    app: &mut App,
    client: &ControlClient,
    mouse: MouseEvent,
) -> Result<(), cyclops_tmux::TmuxError> {
    let col = mouse.column;
    let row = mouse.row;
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
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
                app.menu = MenuState::ContextMenu { pane_id };
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) = app.hit_map.hit(col, row).cloned() else {
                app.menu.close();
                return Ok(());
            };
            match target {
                HitTarget::PaneBody { pane_id } => {
                    app.menu.close();
                    client
                        .command(&format!(
                            "select-pane -t {}",
                            cyclops_tmux::quote_arg(&pane_id)
                        ))
                        .await?;
                    reconcile(app, client).await?;
                }
                HitTarget::PaneSplitRight { pane_id } => {
                    app.menu.close();
                    intent::execute(client, Intent::SplitRight, &pane_id).await?;
                    reconcile(app, client).await?;
                }
                HitTarget::PaneSplitDown { pane_id } => {
                    app.menu.close();
                    intent::execute(client, Intent::SplitDown, &pane_id).await?;
                    reconcile(app, client).await?;
                }
                HitTarget::Tab { index } => {
                    app.menu.close();
                    intent::execute(client, Intent::SelectTab(index + 1), "").await?;
                    reconcile(app, client).await?;
                }
                HitTarget::SidebarRow { index } => {
                    app.menu.close();
                    if let Some(ws) = app.model.workspaces.get(index) {
                        intent::execute(
                            client,
                            Intent::SwitchWorkspace(ws.name.clone()),
                            "",
                        )
                        .await?;
                        reconcile(app, client).await?;
                    }
                }
                HitTarget::AppMenu => {
                    app.menu = if app.menu == MenuState::AppMenu {
                        MenuState::None
                    } else {
                        MenuState::AppMenu
                    };
                }
            }
        }
        _ => {}
    }
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
            reconcile(app, client).await?;
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
        BindingAction::NextWorkspace => {
            let active = app.model.active_workspace;
            let workspaces = app.model.workspaces.clone();
            intent::execute_switch_workspace_by_delta(client, &workspaces, active, 1).await?;
        }
        BindingAction::PrevWorkspace => {
            let active = app.model.active_workspace;
            let workspaces = app.model.workspaces.clone();
            intent::execute_switch_workspace_by_delta(client, &workspaces, active, -1).await?;
        }
        other => {
            intent::execute(client, Intent::from(other), &pane).await?;
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
    match dialog {
        Dialog::ConfirmClosePane { pane_id } => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.dialog = None;
                intent::execute(client, Intent::ClosePane, &pane_id).await?;
                reconcile(app, client).await?;
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
                    reconcile(app, client).await?;
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
                    reconcile(app, client).await?;
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
                    reconcile(app, client).await?;
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
                reconcile(app, client).await?;
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
            let area = f.area();
            let (sidebar_area, main_area) = if app.model.sidebar_visible {
                let chunks = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Horizontal)
                    .constraints([
                        ratatui::layout::Constraint::Length(SIDEBAR_WIDTH),
                        ratatui::layout::Constraint::Min(0),
                    ])
                    .split(area);
                (Some(chunks[0]), chunks[1])
            } else {
                (None, area)
            };
            if let Some(sidebar) = sidebar_area {
                paint_sidebar(
                    &app.model.workspaces,
                    app.model.active_workspace,
                    sidebar,
                    f.buffer_mut(),
                    &app.paint,
                    &mut app.hit_map,
                );
            }
            let tab_area = Rect::new(main_area.x, main_area.y, main_area.width, TAB_BAR_HEIGHT);
            let canvas = Rect::new(
                main_area.x,
                main_area.y + TAB_BAR_HEIGHT,
                main_area.width,
                main_area.height.saturating_sub(TAB_BAR_HEIGHT),
            );
            paint_tab_bar(
                &app.model.session.tabs,
                app.model.session.active_tab,
                tab_area,
                f.buffer_mut(),
                &app.paint,
                &mut app.hit_map,
            );
            let tab = app.model.active_tab();
            paint_window(
                tab,
                &app.runtimes,
                canvas,
                f.buffer_mut(),
                &app.paint,
                app.link_state,
                &app.paused_panes,
                &mut app.hit_map,
            );
            if let Some(dialog) = &app.dialog {
                paint_dialog(dialog, f.area(), f.buffer_mut(), &app.paint);
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
}
