//! cyclops-ui: the live stream behind `cyclops ui`.
//!
//! Two views of one record: the admin stream (only what is aimed at the
//! human plus the states that need one) and the firehose (everything),
//! one keypress apart. Data comes from the daemon's events.subscribe push
//! plus a one-time ledger tail for backfill; all IO lives on its own
//! tasks feeding one channel, so the event loop never blocks on the
//! daemon and a keypress never waits on IO.
//!
//! The terminal layer is hand-rolled (termios raw mode plus ANSI frames,
//! see term.rs): the build environment is offline and carries no TUI
//! crates. The rendering itself is pure (frame.rs), so the backend is a
//! thin seam.
//!
//! Zero polling: the subscription pushes, backfill reads once, the eye
//! arms a single one-shot timer per transition. No intervals anywhere.
//!
//! ## What it owns beyond the stream
//!
//! [`grid`] is the product's one voice for a timestamp gutter, a state
//! cell, a delivery badge and a cause word. The CLI calls it rather than
//! holding a copy, because it held a copy once and the two drifted while
//! both were green.
//!
//! ## What it does not own
//!
//! - What needs a human. The register and the rule are
//!   `cyclops_proto::attention`; this crate feeds it the two things it
//!   accepts and asks it for the count.
//! - Any color value. Every paint names a `cyclops-theme` token, and the
//!   state-to-group mapping is that crate's too.
//! - The daemon. It reads `events.subscribe` and one `status`, asks
//!   nothing else, and jumps focus through `cyclops_tmux::focus_pane`,
//!   which is the only tmux call anywhere near it.

mod app;
mod data;
mod entry;
mod frame;
pub mod grid;
mod input;
mod plain;
mod term;
mod theme;

pub use app::{App, Command, Density, RosterRow, RosterSeed, RowTarget, StatusSeed, View};
pub use cyclops_proto::{Attention, AttentionItem, Eye, PaneSnapshot};
pub use data::{read_backfill, Backfilled, Intake, UiMsg};
pub use entry::{Entry, EntryKind, Filter, PingDelivery};
pub use frame::build;
pub use input::Key;
pub use theme::Theme;

use std::io::IsTerminal;
use std::path::Path;

use tokio::sync::mpsc::unbounded_channel;

/// The terminal's size in cells, or the classic 80x24 when there is none
/// to ask.
///
/// Public because two callers ask the same question: the stream sizes its
/// frame with it, and `cyclops start` sizes a new tmux session with it.
/// The ioctl is written once.
pub fn terminal_size() -> (usize, usize) {
    term::Term::size()
}

/// How `cyclops ui` was asked to run.
#[derive(Debug, Clone, Default)]
pub struct UiOptions {
    /// Line-oriented follow mode; also forced by a non-tty.
    pub plain: bool,
    /// Start in the firehose instead of the admin stream.
    pub firehose: bool,
    pub with: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Ledger tail length for backfill.
    pub backfill: usize,
}

impl UiOptions {
    pub fn filter(&self) -> Filter {
        Filter {
            with: self.with.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
        }
    }
}

/// Which screen mode a run takes: `--plain` asks for the line-oriented
/// follow, and no terminal to take over forces it.
///
/// A color preference is deliberately not here. NO_COLOR reaches
/// `Theme::detect` instead, which turns the paint off and leaves the whole
/// UI standing: every state pairs a glyph with a word, so the eye, the
/// firehose toggle, filters, scrolling, the cheatsheet and the jump all
/// read fine uncolored. GOALS lists the two as separate obligations.
fn wants_plain(opts: &UiOptions, tty: bool) -> bool {
    opts.plain || !tty
}

/// Run the UI to completion. Returns the process exit code.
pub fn run(opts: UiOptions) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let plain = wants_plain(&opts, tty);
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("can't start the UI runtime: {e}");
            return 1;
        }
    };
    if plain {
        rt.block_on(plain::run(&opts, &home))
    } else {
        rt.block_on(run_tui(&opts, &home))
    }
}

/// How long the eye holds its intermediate frame: one tick, never a loop.
const EYE_TICK_MS: u64 = 120;

/// Largest number of queued messages folded into one frame. Keeps a flood
/// fluid (one render per batch) without starving key handling.
const BATCH: usize = 256;

async fn run_tui(opts: &UiOptions, home: &Path) -> i32 {
    let view = if opts.firehose {
        View::Firehose
    } else {
        View::Admin
    };
    let mut app = App::new(Theme::detect(), view, opts.filter());

    let (tx, mut rx) = unbounded_channel();
    data::spawn_io(&tx, home, opts.backfill);
    // Keys ride the same channel as data, decoded off-thread.
    let (key_tx, mut key_rx) = unbounded_channel();
    input::spawn_reader(key_tx);
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(k) = key_rx.recv().await {
                if tx.send(UiMsg::Key(k)).is_err() {
                    return;
                }
            }
        });
    }

    let mut term = match term::Term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("can't take over this terminal: {e}. Try cyclops ui --plain.");
            return 1;
        }
    };
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();

    // Theme hot reload: a stat riding the events that already woke the
    // loop (never a timer). Only meaningful while color is on.
    let mut theme_watch = if app.theme.color {
        Some(cyclops_theme::ThemeWatch::new(home))
    } else {
        None
    };

    let mut intake = Intake::new();
    let mut tick_armed = false;
    draw(&mut term, &mut app);

    loop {
        // Wait for one message or a resize; then fold in whatever else is
        // already queued so a burst costs one frame.
        let first = tokio::select! {
            m = rx.recv() => match m {
                Some(m) => Some(m),
                None => break,
            },
            _ = recv_winch(&mut sigwinch) => None,
        };
        let mut quit = false;
        if let Some(first) = first {
            let mut queued = Some(first);
            let mut n = 0;
            while let Some(msg) = queued.take() {
                if handle(&mut app, &mut intake, &mut tick_armed, &tx, msg) {
                    quit = true;
                    break;
                }
                n += 1;
                if n >= BATCH {
                    break;
                }
                queued = rx.try_recv().ok();
            }
        }
        if quit {
            break;
        }
        // A theme edit, or a `cyclops theme <name>`, applies on this
        // render. A reload the engine refused (a file half-written, a
        // token misspelled) leaves the colors alone and hands back one
        // line, which goes on the notice row: stderr would land in the
        // middle of the frame.
        if let Some(watch) = theme_watch.as_mut() {
            if watch.refresh() {
                app.theme.set_engine(watch.theme().clone());
            }
            if let Some(first) = watch.take_warnings().into_iter().next() {
                app.notice = Some(format!("theme: {first}"));
            }
        }
        // The eye advances one step per frame toward its target; a pending
        // second step arms exactly one delayed redraw.
        if app.tick_eye() && !tick_armed {
            tick_armed = true;
            let tx = tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(EYE_TICK_MS)).await;
                let _ = tx.send(UiMsg::EyeTick);
            });
        }
        draw(&mut term, &mut app);
    }
    drop(term);
    0
}

/// Apply one message to the app. True means quit.
fn handle(
    app: &mut App,
    intake: &mut Intake,
    tick_armed: &mut bool,
    tx: &tokio::sync::mpsc::UnboundedSender<UiMsg>,
    msg: UiMsg,
) -> bool {
    match msg {
        UiMsg::Key(k) => match app.handle_key(k) {
            Some(Command::Quit) => return true,
            Some(Command::Focus(pane)) => {
                // The jump runs off-loop; a slow tmux answer can never
                // hold a frame or a keypress.
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = cyclops_tmux::focus_pane(None, None, &pane) {
                        let _ = tx.send(UiMsg::Notice(format!("can't jump to {pane}: {e}")));
                    }
                });
            }
            None => {}
        },
        UiMsg::Entry(e) => {
            for e in intake.entry(*e) {
                app.live(e);
            }
        }
        UiMsg::Backfill { entries, max_seq } => {
            // The startup order, and it is the whole correctness story:
            // the replayed tail is history, the seed is the daemon's
            // answer about now, and the live entries that queued behind
            // them are newer than both.
            let landed = intake.backfill(entries, max_seq);
            for e in landed.replayed {
                app.replay(e);
            }
            if let Some(seed) = landed.seed {
                seed_status(app, *seed);
            }
            for e in landed.live {
                app.live(e);
            }
        }
        UiMsg::Status(seed) => {
            if let Some(seed) = intake.status(seed) {
                seed_status(app, *seed);
            }
        }
        UiMsg::ConnLost(_) => app.conn_lost = true,
        UiMsg::Notice(n) => app.notice = Some(n),
        UiMsg::EyeTick => *tick_armed = false,
        // Nothing to apply: waking the loop is the whole message, and the
        // reload runs before the frame it woke.
        UiMsg::ThemeChanged => {}
    }
    false
}

/// Apply the startup reconciliation and ingest the lines it wrote for
/// items the replayed tail does not already carry.
fn seed_status(app: &mut App, seed: app::StatusSeed) {
    for e in app.seed_status(seed) {
        app.replay(e);
    }
}

fn draw(term: &mut term::Term, app: &mut App) {
    let (w, h) = term::Term::size();
    let rows = frame::build(app, w, h);
    term.draw(&rows);
}

async fn recv_winch(sig: &mut Option<tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GOALS lists the plain screen-reader mode and honoring NO_COLOR as
    /// two separate obligations. Treating them as one cost a user who
    /// expressed a COLOR preference the eye, the firehose toggle, filters,
    /// scrolling, the cheatsheet and the jump. Screen mode answers to
    /// --plain and to having a terminal; nothing else.
    ///
    /// Run this with NO_COLOR set in the environment; the color half is
    /// pinned separately by `an_uncolored_frame_carries_everything`.
    #[test]
    fn no_color_is_a_color_preference_not_a_screen_mode() {
        let full = UiOptions::default();
        let asked = UiOptions {
            plain: true,
            ..UiOptions::default()
        };
        assert!(!wants_plain(&full, true), "NO_COLOR took the full UI");
        assert!(wants_plain(&asked, true), "--plain was ignored");
        assert!(wants_plain(&full, false), "a pipe must not take over a tty");
    }

    /// The UI is fully legible uncolored: every state pairs a glyph with a
    /// word, so an uncolored frame carries the same information and not
    /// one escape byte. This is why NO_COLOR costs nothing but paint.
    #[test]
    fn an_uncolored_frame_carries_everything() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        app.live(Entry {
            uid: 0,
            ts: 43_480_000,
            seq: None,
            id: Some("e-1".into()),
            kind: EntryKind::State {
                target: "reviewer".into(),
                pane_id: Some("%1".into()),
                state: cyclops_proto::AgentState::BlockedPermission,
            },
        });
        while app.tick_eye() {}
        let rows = build(&mut app, 80, 12);
        assert!(
            rows.iter().all(|r| !r.contains('\x1b')),
            "an uncolored frame emitted escape sequences"
        );
        assert!(rows[0].starts_with("◑ 1 cyclops"), "{:?}", rows[0]);
        assert!(rows[0].contains("1 needs attention"), "{:?}", rows[0]);
        assert!(rows[2].contains("⚠ blocked_permission"), "{:?}", rows[2]);
        assert!(rows.last().unwrap().contains("? keys"));
    }
}
