//! Workspace application state and event loop.

use std::io;

use crossterm::event::{self, Event, KeyEvent};
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
use crate::input::encode_send_keys;
use crate::input::router::{Router, RouterResult};
use crate::model::{RuntimeRegistry, SessionModel};
use crate::render::{paint_dialog, paint_tab_bar, paint_window};
use crate::sync::{fetch_session_model, hydrate_visible_tab};
use crate::term_guard::TermGuard;
use crate::theme::Paint;

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(30);
const TAB_BAR_HEIGHT: u16 = 1;

enum AppMsg {
    Input(KeyEvent),
    Output { pane: String, bytes: Vec<u8> },
    Redraw,
    Reconcile,
}

struct App {
    model: SessionModel,
    runtimes: RuntimeRegistry,
    router: Router,
    paint: Paint,
    socket: Option<String>,
    dialog: Option<Dialog>,
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
    let (client, mut notif_rx) = match ControlClient::spawn(cfg).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    if let Err(e) = client.set_window_size_latest().await {
        eprintln!("{e}");
    }

    let model = match fetch_session_model(&session, socket) {
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
            Ok(Event::Resize(_, _)) => {
                let _ = input_tx.send(AppMsg::Redraw);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    });

    let out_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(n) = notif_rx.recv().await {
            match n {
                Notification::Output { pane, data }
                | Notification::ExtendedOutput { pane, data, .. } => {
                    let _ = out_tx.send(AppMsg::Output { pane, bytes: data });
                }
                Notification::LayoutChange { .. }
                | Notification::WindowAdd { .. }
                | Notification::WindowClose { .. }
                | Notification::WindowRenamed { .. }
                | Notification::WindowPaneChanged { .. } => {
                    let _ = out_tx.send(AppMsg::Reconcile);
                }
                Notification::Exit { .. } => break,
                _ => {}
            }
        }
    });

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
    };

    let mut debounce: Option<Instant> = None;
    let mut detached = false;
    let _ = draw(&mut terminal, &app);
    while !detached {
        if let Some(deadline) = debounce {
            tokio::select! {
                biased;
                msg = rx.recv() => {
                    if !handle_app_msg(msg, &mut app, &client, &mut debounce, &mut detached).await {
                        break;
                    }
                }
                _ = sleep_until(deadline) => {
                    debounce = None;
                    let _ = draw(&mut terminal, &app);
                }
            }
        } else {
            let Some(msg) = rx.recv().await else { break };
            if !handle_app_msg(Some(msg), &mut app, &client, &mut debounce, &mut detached).await {
                break;
            }
        }
    }

    drop(terminal);
    drop(guard);
    client.shutdown().await;
    if detached {
        eprintln!("{}", copy::DETACHED);
    }
    0
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
    client: &ControlClient,
    debounce: &mut Option<Instant>,
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
        AppMsg::Input(key) => {
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

async fn reconcile(app: &mut App, client: &ControlClient) -> Result<(), cyclops_tmux::TmuxError> {
    let active_window = app.model.active_tab().window_id.clone();
    let model = fetch_session_model(&app.model.session, app.socket.as_deref())?;
    let active_tab = model
        .tabs
        .iter()
        .position(|t| t.window_id == active_window)
        .unwrap_or(model.active_tab);
    app.model = model;
    app.model.active_tab = active_tab;
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
    if action == BindingAction::ClosePane {
        let home = cyclops_proto::cyclops_home();
        if pane_has_agent(&home, &pane) {
            app.dialog = Some(Dialog::confirm_close(&pane));
            return Ok(());
        }
    }
    if action == BindingAction::RenameTab {
        app.dialog = Some(Dialog::RenameTab { buffer: String::new() });
        return Ok(());
    }
    intent::execute(client, Intent::from(action), &pane).await
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
    }
    Ok(DetachOutcome::Continue)
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &App) -> io::Result<()> {
    terminal
        .draw(|f| {
            let area = f.area();
            let tab_area = Rect::new(area.x, area.y, area.width, TAB_BAR_HEIGHT);
            let canvas = Rect::new(
                area.x,
                area.y + TAB_BAR_HEIGHT,
                area.width,
                area.height.saturating_sub(TAB_BAR_HEIGHT),
            );
            paint_tab_bar(
                &app.model.tabs,
                app.model.active_tab,
                tab_area,
                f.buffer_mut(),
                &app.paint,
            );
            let tab = app.model.active_tab();
            let _ = tab.index;
            let _ = tab.zoomed;
            paint_window(tab, &app.runtimes, canvas, f.buffer_mut(), &app.paint);
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
