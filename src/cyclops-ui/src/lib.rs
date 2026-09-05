//! cyclops-ui: the live stream behind `cyclops watch`.
//!
//! Three views share one terminal: the admin stream (only what is aimed at
//! the human plus states that need one), the firehose (everything), and
//! Messages (the durable mailbox and notification queue). Data comes from
//! daemon pushes plus whole snapshots; IO runs on separate tasks feeding
//! bounded priority lanes, so the event loop never blocks on the daemon or
//! leaves a keypress behind history.
//!
//! The terminal layer is hand-rolled (termios raw mode plus ANSI frames,
//! see term.rs): the build environment is offline and carries no TUI
//! crates. The rendering itself is pure (frame.rs), so the backend is a
//! thin seam.
//!
//! Zero polling: the subscription pushes, daemon backfill runs once, the eye
//! arms a single one-shot timer per transition. No intervals anywhere.
//!
//! ## What it owns beyond the stream
//!
//! [`grid`] is the product's one voice for a timestamp gutter, a state
//! cell, a delivery badge and a cause word. The CLI calls it rather than
//! holding a copy, because it held a copy once and the two drifted while
//! both were green.
//!
//! [`Record`] is the backend-neutral event-stream model: entry
//! normalization, backfill/live ordering ([`StreamProjectionState`]), resolution rows,
//! the calm/firehose decision, and stable row identity: everything a
//! renderer needs and nothing about how one paints. `cyclops watch`
//! (app.rs, frame.rs, entry.rs, plain.rs) is its first renderer; a
//! workspace sidebar's Stream tab reading the same daemon answer through the same
//! model is meant to be the second.
//!
//! ## What it does not own
//!
//! - What needs a human. The register and the rule are
//!   `cyclops_proto::attention`; this crate feeds it the two things it
//!   accepts and asks it for the count.
//! - Any color value. Every paint names a `cyclops-theme` token, and the
//!   state-to-group mapping is that crate's too.
//! - The daemon. It reads `events.subscribe`, one `status`, one bounded
//!   `events.backfill`, and whole
//!   `messages.snapshot` answers after content-free change edges. Focus is a
//!   target description handed to the launcher-provided terminal adapter.

#[cfg(feature = "presentation")]
mod action;
#[cfg(feature = "watch")]
pub mod action_io;
#[cfg(feature = "presentation")]
mod app;
#[cfg(feature = "presentation")]
pub mod avatar;
#[cfg(feature = "presentation")]
pub mod chat;
#[cfg(feature = "watch")]
mod data;
#[cfg(feature = "presentation")]
pub mod detail;
#[cfg(feature = "presentation")]
mod entry;
#[cfg(feature = "presentation")]
mod frame;
pub mod grid;
#[cfg(feature = "presentation")]
mod health;
#[cfg(feature = "watch")]
mod input;
#[cfg(feature = "presentation")]
mod key;
#[cfg(feature = "presentation")]
pub mod messages;
#[cfg(feature = "watch")]
mod plain;
#[cfg(feature = "presentation")]
mod projection;
#[cfg(feature = "presentation")]
pub mod queue;
#[cfg(feature = "presentation")]
mod stream;
#[cfg(feature = "watch")]
mod term;
mod terminal_size;
#[cfg(feature = "presentation")]
mod theme;

#[cfg(feature = "presentation")]
pub use action::{ActionOutcome, ActionRequest, RequestKind, RequestToken};
#[cfg(feature = "watch")]
pub use action_io::perform;
#[cfg(feature = "presentation")]
pub use app::{App, Command, Density, RosterRow, RowTarget, View};
#[cfg(feature = "presentation")]
pub use avatar::{Avatar, AvatarRegistry};
#[cfg(feature = "presentation")]
pub use chat::{
    chat_action_line, chat_action_lines, chat_action_strip, chat_action_strips, chat_actions,
    render_chat, render_chat_lines, wrap_words, ChatAction, ChatActionSpan, ChatActionStrip,
    ChatInk, ChatLine, ChatLineKind, ChatRenderContext, ChatSpan, ComposerMode, ComposerState,
    TimelineItem,
};
#[cfg(feature = "presentation")]
pub use cyclops_proto::{Attention, AttentionItem, Eye, PaneSnapshot};
#[cfg(feature = "watch")]
pub use data::{FocusPane, UiMsg};
#[cfg(feature = "presentation")]
pub use detail::{Action, Back, Check, Detail, Draft, Loaded, Request, Stage, ThreadEntry};
#[cfg(feature = "presentation")]
pub use frame::{build, messages_help};
#[cfg(feature = "presentation")]
pub use health::BuildHealth;
#[cfg(feature = "presentation")]
pub use key::Key;
#[cfg(feature = "presentation")]
pub use messages::{
    rows_from_snapshot, FollowRequest, Link, MessageFollower, RefreshGate, RefreshRequest,
};
#[cfg(feature = "presentation")]
pub use projection::{
    project_backfill, BackfillReport, StreamInput, StreamProjection, StreamProjectionState,
    StreamUpdate,
};
#[cfg(feature = "presentation")]
pub use queue::{
    Counts, Direction, FrozenTarget, HumanQueue, MailboxWord, QueueRow, QueueTarget, Scope,
    SessionFilter, Snapshot, WakeWord,
};
#[cfg(feature = "presentation")]
pub use stream::{
    EndpointFilter, Entry, EntryKind, Filter, MessageEndpoints, PingDelivery, Record, RosterSeed,
    StatusSeed,
};
#[cfg(feature = "presentation")]
pub use theme::Theme;

#[cfg(feature = "watch")]
use std::io::IsTerminal;
#[cfg(feature = "watch")]
use std::path::Path;

#[cfg(feature = "watch")]
use tokio::sync::mpsc;

/// The terminal's size in cells, or the classic 80x24 when there is none
/// to ask.
///
/// Public because two callers ask the same question: the stream sizes its
/// frame with it, and `cyclops start` sizes a new tmux session with it.
/// The ioctl is written once.
pub fn terminal_size() -> (usize, usize) {
    terminal_size::get()
}

/// How `cyclops watch` was asked to run.
#[cfg(feature = "watch")]
#[derive(Debug, Clone, Default)]
pub struct UiOptions {
    /// Line-oriented follow mode; also forced by a non-tty.
    pub plain: bool,
    /// Start in the firehose instead of the admin stream.
    pub firehose: bool,
    pub with: Option<EndpointFilter>,
    pub from: Option<EndpointFilter>,
    pub to: Option<EndpointFilter>,
    /// Ledger tail length for backfill.
    pub backfill: usize,
    /// Launcher-owned terminal focus effect. None keeps rows readable but
    /// reports that focus is unavailable when the user asks for it.
    pub focus: Option<FocusPane>,
}

#[cfg(feature = "watch")]
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
/// firehose toggle, filters, scrolling, the cheatsheet and pane focus all
/// read fine uncolored. GOALS lists the two as separate obligations.
#[cfg(feature = "watch")]
fn wants_plain(opts: &UiOptions, tty: bool) -> bool {
    opts.plain || !tty
}

/// Run the UI to completion. Returns the process exit code.
#[cfg(feature = "watch")]
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
#[cfg(feature = "watch")]
const EYE_TICK_MS: u64 = 120;

/// Largest number of queued messages folded into one frame. Keeps a flood
/// fluid (one render per batch) without starving key handling. The workspace
/// UI uses the same budget so one number defines interactive ingress.
#[cfg(feature = "watch")]
pub const INGRESS_BATCH: usize = 256;
#[cfg(feature = "watch")]
const BATCH: usize = INGRESS_BATCH;
/// One complete render batch. Producers backpressure after this instead of
/// growing memory while the terminal is slow.
#[cfg(feature = "watch")]
pub(crate) const EVENT_CAPACITY: usize = BATCH;
/// One result from each snapshot producer can be outstanding: startup,
/// queue refresh, and durable follow.
#[cfg(feature = "watch")]
pub(crate) const SNAPSHOT_CAPACITY: usize = 3;
/// The action worker is serial, so a second result cannot exist before the
/// first is consumed.
#[cfg(feature = "watch")]
pub(crate) const ACTION_CAPACITY: usize = 1;
/// Keys have their own lane and one render batch of headroom. The blocking
/// reader waits when it fills, preserving every key without letting data
/// traffic delay it.
#[cfg(feature = "watch")]
const INPUT_CAPACITY: usize = BATCH;
#[cfg(feature = "watch")]
const MESSAGE_GAP_NOTICE: &str = "message sequence gap detected; rebuilding from a whole snapshot";

#[cfg(feature = "watch")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Input,
    Action,
    Snapshot,
    Event,
}

#[cfg(feature = "watch")]
impl Lane {
    fn next(self) -> Self {
        match self {
            Lane::Input => Lane::Action,
            Lane::Action => Lane::Snapshot,
            Lane::Snapshot => Lane::Event,
            Lane::Event => Lane::Input,
        }
    }
}

#[cfg(feature = "watch")]
enum IngressWake {
    Message(Lane, UiMsg),
    Resize,
    Closed,
}

#[cfg(feature = "watch")]
async fn run_tui(opts: &UiOptions, home: &Path) -> i32 {
    let view = if opts.firehose {
        View::Firehose
    } else {
        View::Admin
    };
    let mut app = App::new(Theme::detect(), view, opts.filter());

    let (event_tx, mut event_rx) = mpsc::channel(EVENT_CAPACITY);
    let (snapshot_tx, mut snapshot_rx) = mpsc::channel(SNAPSHOT_CAPACITY);
    let (action_tx, mut action_rx) = mpsc::channel(ACTION_CAPACITY);
    let sinks = data::UiSinks {
        events: event_tx,
        snapshots: snapshot_tx,
        actions: action_tx,
    };
    let io = data::spawn_io(&sinks, home, opts.backfill, opts.focus.clone());
    let (key_tx, mut key_rx) = mpsc::channel(INPUT_CAPACITY);
    input::spawn_reader(key_tx);

    let mut term = match term::Term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("can't take over this terminal: {e}. Try cyclops watch --plain.");
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

    let mut stream_projection = StreamProjectionState::new();
    let mut message_follower = MessageFollower::default();
    let mut tick_armed = false;
    let mut key_open = true;
    draw(&mut term, &mut app);

    loop {
        // Wait for one message or a resize; then fold in whatever else is
        // already queued so a burst costs one frame.
        let first = match wait_next_ingress(
            &mut key_open,
            &mut key_rx,
            &mut action_rx,
            &mut snapshot_rx,
            &mut event_rx,
            &mut sigwinch,
        )
        .await
        {
            IngressWake::Message(lane, msg) => Some((lane, msg)),
            IngressWake::Resize => None,
            IngressWake::Closed => break,
        };
        let mut quit = false;
        if let Some((lane, first)) = first {
            let mut next_lane = lane.next();
            let mut queued = Some(first);
            let mut n = 0;
            while let Some(msg) = queued.take() {
                if handle(
                    &mut app,
                    &mut stream_projection,
                    &mut message_follower,
                    &mut tick_armed,
                    &io.focus,
                    msg,
                ) {
                    quit = true;
                    break;
                }
                if std::mem::take(&mut app.reconnect_owed) {
                    match io.reconnect.try_send(()) {
                        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                        Err(mpsc::error::TrySendError::Closed(())) => {
                            app.refresh.disconnected();
                            app.notice =
                                Some("can't reconnect: subscription worker stopped".into());
                        }
                    }
                }
                n += 1;
                if n >= BATCH {
                    break;
                }
                queued = try_next_ready(
                    &mut next_lane,
                    &mut key_rx,
                    &mut action_rx,
                    &mut snapshot_rx,
                    &mut event_rx,
                )
                .map(|(lane, msg)| {
                    next_lane = lane.next();
                    msg
                });
            }
        }
        if quit {
            break;
        }
        // One fetch per batch at most, and only when something said the
        // state moved. The gate refuses while a fetch is in flight, so a
        // burst of edges costs one follow-up rather than one read each.
        if let Some(request) = app.wants_messages() {
            if io.refresh.send(request).await.is_err() {
                break;
            }
        }
        if let Some(request) = message_follower.begin() {
            if io.follow.send(request).await.is_err() {
                break;
            }
        }
        // One detail read or action at a time, off the frame path. A
        // confirmed action goes first: the reader is waiting on it.
        // Both takers mark the request in flight and hand back the token
        // its answer must carry.
        let next = app.take_pending().or_else(|| app.take_detail_read());
        if let Some(sent) = next {
            match io.action.try_send(sent) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full((token, _))) => app.apply_action(
                    token,
                    crate::action::ActionOutcome::NotSent(
                        "another detail action is still queued".into(),
                    ),
                ),
                Err(mpsc::error::TrySendError::Closed((token, _))) => app.apply_action(
                    token,
                    crate::action::ActionOutcome::NotSent("action worker stopped".into()),
                ),
            }
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
            let tx = sinks.actions.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(EYE_TICK_MS)).await;
                let _ = tx.send(UiMsg::EyeTick).await;
            });
        }
        draw(&mut term, &mut app);
    }
    drop(term);
    0
}

/// Apply one message to the app. True means quit.
#[cfg(feature = "watch")]
fn handle(
    app: &mut App,
    stream_projection: &mut StreamProjectionState,
    message_follower: &mut MessageFollower,
    tick_armed: &mut bool,
    focus: &tokio::sync::mpsc::Sender<String>,
    msg: UiMsg,
) -> bool {
    match msg {
        UiMsg::Key(k) => match app.handle_key(k) {
            Some(Command::Quit) => return true,
            // The loop spawns it; `reconnect_owed` carries the request
            // out to where a task can be started.
            Some(Command::Reconnect) => {}
            Some(Command::Focus(pane)) => {
                // One serial worker owns tmux focus. Repeated clicks cannot
                // create an unbounded set of blocking tasks.
                match focus.try_send(pane) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        app.notice = Some("pane focus already in progress".into());
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        app.notice = Some("can't focus pane: focus worker stopped".into());
                    }
                }
            }
            None => {}
        },
        UiMsg::Entry(e) => {
            apply_stream_updates(app, stream_projection.apply(StreamInput::Live(*e)))
        }
        UiMsg::StreamProjection(projection) => {
            // The presentation projection owns the epoch reset, ordering, and
            // duplicate cursor. App owns its own roster and render state.
            app.clear_stream_projection();
            apply_stream_updates(app, stream_projection.replace(*projection));
        }
        // The acknowledgement closes the startup race: only now may the
        // snapshot socket open, because later changes cannot be missed.
        UiMsg::Subscribed => {
            app.conn_lost = false;
            app.refresh.connected();
            message_follower.connected();
        }
        UiMsg::ConnLost(why) => {
            app.conn_lost = true;
            app.refresh.disconnected();
            message_follower.disconnected();
            app.notice = Some(why);
        }
        UiMsg::BuildHealth(health) => app.build_health = Some(health),
        UiMsg::MessagesChanged(changed) => {
            message_follower.changed(&changed);
            if app.refresh.messages_changed(&changed) {
                app.notice = Some(MESSAGE_GAP_NOTICE.into());
            }
        }
        UiMsg::MessagesRouteChanged => app.refresh.mark_dirty(),
        UiMsg::Messages { request, snapshot } => {
            if let Some(lines) = app.apply_messages_response(request, &snapshot) {
                for e in lines {
                    app.live(e);
                }
                message_follower.baseline(&snapshot);
                if app.notice.as_deref() == Some(MESSAGE_GAP_NOTICE) {
                    app.notice = None;
                }
            }
        }
        // The last good snapshot stays on screen. Replacing it with
        // nothing would read as an empty mailbox.
        UiMsg::MessagesFailed { request, why } => {
            if app.refresh.finish_failure(request) {
                app.notice = Some(format!("messages unavailable: {why}"));
            }
        }
        UiMsg::MessagesFollow { request, page } => match message_follower.finish(request, &page) {
            Ok(entries) => {
                for entry in entries {
                    app.live(entry);
                }
            }
            Err(why) => app.notice = Some(format!("messages follow unavailable: {why}")),
        },
        UiMsg::MessagesFollowFailed { request, why } => {
            if message_follower.failed(request) {
                app.notice = Some(format!("messages follow unavailable: {why}"));
            }
        }
        UiMsg::ActionDone { token, outcome } => app.apply_action(token, *outcome),
        UiMsg::Notice(n) => app.notice = Some(n),
        UiMsg::EyeTick => *tick_armed = false,
        // Nothing to apply: waking the loop is the whole message, and the
        // reload runs before the frame it woke.
        UiMsg::ThemeChanged => {}
    }
    false
}

/// Apply pure stream updates to this renderer's own state. The projection
/// decides ordering and duplicate suppression; the application retains its
/// roster, selection, and render-specific consequences of those facts.
#[cfg(feature = "watch")]
fn apply_stream_updates(app: &mut App, updates: Vec<StreamUpdate>) {
    for update in updates {
        match update {
            StreamUpdate::Replay(entry) => app.replay(entry),
            StreamUpdate::Status(seed) => seed_status(app, *seed),
            StreamUpdate::Live(entry) => {
                app.live(entry);
            }
            StreamUpdate::Notice(text) => app.notice = Some(text),
        }
    }
}

/// Apply the startup reconciliation and ingest the lines it wrote for
/// items the replayed tail does not already carry.
#[cfg(feature = "watch")]
fn seed_status(app: &mut App, seed: stream::StatusSeed) {
    for e in app.seed_status(seed) {
        app.replay(e);
    }
}

#[cfg(feature = "watch")]
fn draw(term: &mut term::Term, app: &mut App) {
    let (w, h) = terminal_size();
    let rows = frame::build(app, w, h);
    term.draw(&rows);
}

#[cfg(feature = "watch")]
async fn recv_winch(sig: &mut Option<tokio::signal::unix::Signal>) -> bool {
    match sig {
        Some(s) => s.recv().await.is_some(),
        None => std::future::pending().await,
    }
}

/// Wait fairly across lanes. A closed stdin lane is disabled permanently,
/// so EOF cannot become an eventless redraw loop that starves daemon work.
#[cfg(feature = "watch")]
async fn wait_next_ingress(
    key_open: &mut bool,
    key_rx: &mut mpsc::Receiver<Key>,
    action_rx: &mut mpsc::Receiver<UiMsg>,
    snapshot_rx: &mut mpsc::Receiver<UiMsg>,
    event_rx: &mut mpsc::Receiver<UiMsg>,
    sigwinch: &mut Option<tokio::signal::unix::Signal>,
) -> IngressWake {
    loop {
        let wake = tokio::select! {
            key = key_rx.recv(), if *key_open => match key {
                Some(key) => IngressWake::Message(Lane::Input, UiMsg::Key(key)),
                None => {
                    *key_open = false;
                    continue;
                }
            },
            action = action_rx.recv() => match action {
                Some(msg) => IngressWake::Message(Lane::Action, msg),
                None => IngressWake::Closed,
            },
            snapshot = snapshot_rx.recv() => match snapshot {
                Some(msg) => IngressWake::Message(Lane::Snapshot, msg),
                None => IngressWake::Closed,
            },
            event = event_rx.recv() => match event {
                Some(msg) => IngressWake::Message(Lane::Event, msg),
                None => IngressWake::Closed,
            },
            resized = recv_winch(sigwinch) => if resized {
                IngressWake::Resize
            } else {
                IngressWake::Closed
            },
        };
        return wake;
    }
}

/// Drain ready work by rotating the first eligible lane after every item.
/// Input starts each run first, while every continuously ready lane is served
/// within four items.
#[cfg(feature = "watch")]
fn try_next_ready(
    start: &mut Lane,
    key_rx: &mut mpsc::Receiver<Key>,
    action_rx: &mut mpsc::Receiver<UiMsg>,
    snapshot_rx: &mut mpsc::Receiver<UiMsg>,
    event_rx: &mut mpsc::Receiver<UiMsg>,
) -> Option<(Lane, UiMsg)> {
    let mut lane = *start;
    for _ in 0..4 {
        let msg = match lane {
            Lane::Input => key_rx.try_recv().ok().map(UiMsg::Key),
            Lane::Action => action_rx.try_recv().ok(),
            Lane::Snapshot => snapshot_rx.try_recv().ok(),
            Lane::Event => event_rx.try_recv().ok(),
        };
        if let Some(msg) = msg {
            return Some((lane, msg));
        }
        lane = lane.next();
    }
    None
}

#[cfg(all(test, feature = "watch"))]
mod tests {
    use super::*;

    #[test]
    fn a_ready_key_precedes_a_full_event_lane() {
        let (key_tx, mut key_rx) = mpsc::channel(INPUT_CAPACITY);
        let (_action_tx, mut action_rx) = mpsc::channel(ACTION_CAPACITY);
        let (_snapshot_tx, mut snapshot_rx) = mpsc::channel(SNAPSHOT_CAPACITY);
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_CAPACITY);

        for _ in 0..EVENT_CAPACITY {
            event_tx.try_send(UiMsg::ThemeChanged).unwrap();
        }
        assert!(event_tx.try_send(UiMsg::ThemeChanged).is_err());
        key_tx.try_send(Key::Char('q')).unwrap();
        let mut start = Lane::Input;

        assert!(matches!(
            try_next_ready(
                &mut start,
                &mut key_rx,
                &mut action_rx,
                &mut snapshot_rx,
                &mut event_rx,
            ),
            Some((Lane::Input, UiMsg::Key(Key::Char('q'))))
        ));
        assert!(matches!(event_rx.try_recv(), Ok(UiMsg::ThemeChanged)));
    }

    #[test]
    fn ready_lanes_are_drained_in_a_bounded_rotation() {
        let (key_tx, mut key_rx) = mpsc::channel(1);
        let (action_tx, mut action_rx) = mpsc::channel(1);
        let (snapshot_tx, mut snapshot_rx) = mpsc::channel(1);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        key_tx.try_send(Key::Char('x')).unwrap();
        action_tx.try_send(UiMsg::EyeTick).unwrap();
        snapshot_tx.try_send(UiMsg::ThemeChanged).unwrap();
        event_tx.try_send(UiMsg::MessagesRouteChanged).unwrap();

        let mut start = Lane::Input;
        let mut seen = Vec::new();
        for _ in 0..4 {
            let (lane, _) = try_next_ready(
                &mut start,
                &mut key_rx,
                &mut action_rx,
                &mut snapshot_rx,
                &mut event_rx,
            )
            .unwrap();
            seen.push(lane);
            start = lane.next();
        }
        assert_eq!(
            seen,
            vec![Lane::Input, Lane::Action, Lane::Snapshot, Lane::Event]
        );
    }

    #[tokio::test]
    async fn closed_key_input_is_disabled_before_daemon_work_continues() {
        let (key_tx, mut key_rx) = mpsc::channel(1);
        drop(key_tx);
        let (_action_tx, mut action_rx) = mpsc::channel(1);
        let (_snapshot_tx, mut snapshot_rx) = mpsc::channel(1);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let producer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            event_tx.send(UiMsg::ThemeChanged).await.unwrap();
        });
        let mut key_open = true;
        let mut no_signal = None;

        let wake = wait_next_ingress(
            &mut key_open,
            &mut key_rx,
            &mut action_rx,
            &mut snapshot_rx,
            &mut event_rx,
            &mut no_signal,
        )
        .await;

        producer.await.unwrap();
        assert!(!key_open);
        assert!(matches!(
            wake,
            IngressWake::Message(Lane::Event, UiMsg::ThemeChanged)
        ));
    }

    /// GOALS lists the plain screen-reader mode and honoring NO_COLOR as
    /// two separate obligations. Treating them as one cost a user who
    /// expressed a COLOR preference the eye, the firehose toggle, filters,
    /// scrolling, the cheatsheet and pane focus. Screen mode answers to
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
                recipient: None,
                session_idx: 0,
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

    #[test]
    fn a_snapshot_failure_keeps_the_last_good_queue_and_posts_one_notice() {
        let mut app = App::new(Theme::none(), View::Messages, Filter::default());
        app.queue.replace(Snapshot {
            watermark: 17,
            rows: Vec::new(),
        });
        let mut intake = StreamProjectionState::new();
        let mut message_follower = MessageFollower::default();
        let mut tick_armed = false;
        let (tx, _rx) = mpsc::channel(4);

        assert!(!handle(
            &mut app,
            &mut intake,
            &mut message_follower,
            &mut tick_armed,
            &tx,
            UiMsg::Subscribed,
        ));
        let request = app.wants_messages().unwrap();
        assert!(!handle(
            &mut app,
            &mut intake,
            &mut message_follower,
            &mut tick_armed,
            &tx,
            UiMsg::MessagesFailed {
                request,
                why: "socket closed".into(),
            },
        ));

        assert_eq!(app.queue.watermark(), 17);
        assert_eq!(
            app.notice.as_deref(),
            Some("messages unavailable: socket closed")
        );
        assert_eq!(app.refresh.link(), crate::messages::Link::Lost);
        assert!(!app.refresh.is_fetching());
    }

    #[test]
    fn an_initial_connection_failure_is_visible_and_retryable() {
        let mut app = App::new(Theme::none(), View::Messages, Filter::default());
        let mut intake = StreamProjectionState::new();
        let mut message_follower = MessageFollower::default();
        let mut tick_armed = false;
        let (tx, _rx) = mpsc::channel(4);

        assert!(!handle(
            &mut app,
            &mut intake,
            &mut message_follower,
            &mut tick_armed,
            &tx,
            UiMsg::ConnLost("daemon socket unavailable".into()),
        ));

        assert_eq!(app.refresh.link(), crate::messages::Link::Lost);
        assert_eq!(app.notice.as_deref(), Some("daemon socket unavailable"));
        let frame = build(&mut app, 80, 24).join("\n");
        assert!(frame.contains("R reconnect"), "{frame}");
        assert!(frame.contains("daemon socket unavailable"), "{frame}");
        assert_eq!(app.handle_key(Key::Char('R')), Some(Command::Reconnect));
    }

    #[test]
    fn a_visible_stream_gap_requires_and_accepts_a_whole_snapshot_rebuild() {
        let mut app = App::new(Theme::none(), View::Messages, Filter::default());
        app.live(Entry {
            uid: 0,
            ts: 1,
            seq: None,
            id: Some("stale-before-gap".into()),
            kind: EntryKind::State {
                target: "reviewer".into(),
                recipient: None,
                session_idx: 0,
                pane_id: Some("%1".into()),
                state: cyclops_proto::AgentState::BlockedPermission,
            },
        });
        assert!(!app.is_empty());
        app.queue.replace(Snapshot {
            watermark: 17,
            rows: Vec::new(),
        });
        let mut intake = StreamProjectionState::new();
        let mut message_follower = MessageFollower::default();
        let mut tick_armed = false;
        let (focus, _focus_rx) = mpsc::channel(1);

        assert!(!handle(
            &mut app,
            &mut intake,
            &mut message_follower,
            &mut tick_armed,
            &focus,
            UiMsg::ConnLost("malformed event; live stream may have a gap".into()),
        ));
        assert_eq!(app.queue.watermark(), 17, "the last good model vanished");
        assert!(
            !app.refresh.may_mutate(),
            "stale state still allowed actions"
        );
        assert_eq!(app.handle_key(Key::Char('R')), Some(Command::Reconnect));

        assert!(!handle(
            &mut app,
            &mut intake,
            &mut message_follower,
            &mut tick_armed,
            &focus,
            UiMsg::StreamProjection(Box::new(StreamProjection {
                seed: Some(Box::new(StatusSeed::default())),
                entries: Vec::new(),
                max_seq: None,
                warning: None,
            })),
        ));
        assert!(
            app.is_empty(),
            "the stale stream projection survived reconnect"
        );
        assert!(
            intake.is_backfilled(),
            "the replacement projection did not land"
        );
        assert_eq!(
            app.queue.watermark(),
            17,
            "stream recovery replaced the independent mailbox snapshot"
        );

        assert!(!handle(
            &mut app,
            &mut intake,
            &mut message_follower,
            &mut tick_armed,
            &focus,
            UiMsg::Subscribed,
        ));
        assert!(
            !app.refresh.may_mutate(),
            "an ack was mistaken for rebuilt state"
        );
        let request = app.wants_messages().expect("reconnect owes one rebuild");
        let snapshot: cyclops_proto::MessagesSnapshotResult =
            serde_json::from_value(serde_json::json!({
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "workspace_seq": 23,
                "counts": {
                    "visible_messages": 0,
                    "returned_messages": 0,
                    "inbox_messages": 0,
                    "outbound_messages": 0,
                    "work_messages": 0,
                    "active_messages": 0,
                    "settled_messages": 0,
                    "pending_entries": 0,
                    "claimed_entries": 0,
                    "open_attention_entries": 0
                },
                "rows": []
            }))
            .unwrap();
        assert!(!handle(
            &mut app,
            &mut intake,
            &mut message_follower,
            &mut tick_armed,
            &focus,
            UiMsg::Messages {
                request,
                snapshot: Box::new(snapshot),
            },
        ));

        assert_eq!(app.queue.watermark(), 23);
        assert!(
            app.refresh.may_mutate(),
            "rebuilt state did not restore actions"
        );
    }
}
