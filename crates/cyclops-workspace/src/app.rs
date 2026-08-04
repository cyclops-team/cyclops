//! Workspace application state and event loop.

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use cyclops_tmux::{active_pane, list_sessions, ControlClient, ControlConfig, Notification};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Block, Borders};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Duration, Instant};

use crate::copy;
use crate::input::encode_send_keys;
use crate::render::paint_pane;
use crate::runtime::{snapshot_from_bundle, PaneRuntime};
use crate::term_guard::TermGuard;
use crate::theme::Paint;

const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(30);

enum AppMsg {
    Input(KeyEvent),
    Output { pane: String, bytes: Vec<u8> },
    Redraw,
}

struct App {
    session: String,
    pane_id: String,
    runtime: PaneRuntime,
    paint: Paint,
    prefix_armed: bool,
}

/// Run the workspace on a tty. Returns the process exit code.
pub async fn run_async() -> i32 {
    let sessions = match list_sessions(None) {
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

    let pane_id = match active_pane(&session, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let cfg = ControlConfig::attach(&session);
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

    let bundle = match client.hydrate_pane(&pane_id).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            client.shutdown().await;
            return 1;
        }
    };
    let mut runtime = PaneRuntime::new(bundle.cols, bundle.rows);
    runtime.hydrate(&snapshot_from_bundle(&bundle));

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
    if let Err(e) = client.set_client_size(size.0, size.1).await {
        eprintln!("{e}");
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
        session,
        pane_id,
        runtime,
        paint: Paint::detect(),
        prefix_armed: false,
    };

    let mut debounce: Option<Instant> = None;
    let mut detached = false;
    let _ = draw(&mut terminal, &app);
    while !detached {
        let deadline = debounce;
        tokio::select! {
            biased;
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    AppMsg::Redraw => debounce = Some(Instant::now() + RECONCILE_DEBOUNCE),
                    AppMsg::Output { pane, bytes } if pane == app.pane_id => {
                        app.runtime.feed(&bytes);
                        debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
                    }
                    AppMsg::Output { .. } => {}
                    AppMsg::Input(key) => {
                        if handle_key(&mut app, &client, key).await {
                            detached = true;
                        } else {
                            debounce = Some(Instant::now() + RECONCILE_DEBOUNCE);
                        }
                    }
                }
            }
            _ = sleep_until(deadline.unwrap()), if deadline.is_some() => {
                debounce = None;
                let _ = draw(&mut terminal, &app);
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

async fn handle_key(app: &mut App, client: &ControlClient, key: KeyEvent) -> bool {
    if app.prefix_armed {
        app.prefix_armed = false;
        if matches!(key.code, KeyCode::Char('d')) {
            return true;
        }
    }
    if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.prefix_armed = true;
        return false;
    }
    let encoded = encode_send_keys(&key);
    for key_arg in &encoded {
        let _ = client.send_keys(&app.pane_id, &[key_arg.as_str()]).await;
    }
    false
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &App) -> io::Result<()> {
    terminal
        .draw(|f| {
            let area = f.area();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(crate::theme::pane_border_focused(&app.paint))
                .title(format!(" {} · {} ", app.session, app.pane_id));
            let inner = block.inner(area);
            f.render_widget(block, area);
            let grid = app.runtime.grid();
            paint_pane(grid.grid, inner, f.buffer_mut(), &app.paint);
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
