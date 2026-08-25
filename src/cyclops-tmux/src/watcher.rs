//! Zero-polling reconciling pane table for one session.
//!
//! Level-triggered core (ADR-001 revision 1): control notifications are
//! hints that mark state dirty; truth comes from authoritative queries
//! (`list-panes`). Missed events cost freshness, never correctness. There
//! are no interval timers anywhere: with no events, nothing runs.
//!
//! Primary per-pane change signal: `refresh-client -B` subscriptions,
//! MEASURED working in control mode on tmux 3.6a. tmux pushes the expanded
//! format whenever pane_title, pane_dead, pane_in_mode, or
//! pane_current_command changes, including title changes made via OSC from
//! inside the pane. Structural changes (splits, kills, window renames)
//! arrive as notification hints and trigger a debounced full reconcile.
//!
//! Death is the one field that signal cannot carry, so it gets its own
//! subscription: see [`DEAD_SUB`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::control::{ControlClient, ControlConfig};
use crate::error::TmuxError;
use crate::notify::Notification;
use crate::quote::quote_arg;

/// Coalescing window between a change hint and the reconcile query it
/// triggers. Event-triggered one-shot, not an interval: it only exists
/// while a hint is pending.
const RECONCILE_DEBOUNCE: Duration = Duration::from_millis(30);

/// Per-pane floor between OutputActivity events. %output arrives per pty
/// write; consumers want "the pane is alive", not the firehose.
const OUTPUT_RATE_LIMIT: Duration = Duration::from_millis(100);

/// Subscription name prefix; the pane id digits are appended ("cyp3").
const SUB_PREFIX: &str = "cyp";

/// Per-pane subscription format. Title first because it is the only
/// free-text field; the fixed fields are parsed from the right.
const SUB_FORMAT: &str =
    "#{pane_title}\t#{pane_dead}\t#{pane_in_mode}\t#{pane_current_command}\t#{pane_pid}";

/// Session-wide dead-pane subscription: the edge a pane's death produces.
/// Shares the prefix and cannot collide with a per-pane name, which is
/// always digits after "cyp".
///
/// A death has no notification of its own, and it cannot arrive on the
/// per-pane subscription either, by construction: tmux sets pane_dead when
/// the pane's pty fd closes, and the closed fd is also what makes tmux skip
/// that pane's per-pane subscription. MEASURED on 3.6a and next-3.8: zero
/// pushes over four-plus ticks after the death, while list-panes reports
/// dead=1. So `#{pane_dead}` in [`SUB_FORMAT`] can report a live pane and
/// never the flip. Before this subscription the watcher learned of a death
/// only when an unrelated event happened to force a reconcile at the same
/// moment, which 3.6a won and next-3.8 lost, leaving a corpse reading as a
/// live agent for the rest of the session (F25).
///
/// The all-panes form (`%*`) has no fd gate: tmux keeps expanding it for a
/// dead pane, MEASURED pushing the 0-to-1 flip on the tick after the death
/// on both versions.
const DEAD_SUB: &str = "cypdead";

/// Format for [`DEAD_SUB`]. One field, so tmux pushes on the flip and on
/// nothing else; the pane id travels in the notification header.
const DEAD_FORMAT: &str = "#{pane_dead}";

/// Snapshot format for list-panes. pane_id and window_id lead (no tabs
/// possible in ids), the seven fixed fields trail; window_name and title
/// sit in the middle and are split on the first tab between them. A tab
/// inside a window name would shift the title; documented limitation.
const PANE_FORMAT: &str = "#{pane_id}\t#{window_id}\t#{window_name}\t#{pane_title}\t#{pane_dead}\t#{pane_in_mode}\t#{pane_current_command}\t#{pane_width}\t#{pane_height}\t#{pane_active}\t#{pane_pid}";

/// One pane as the watcher knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    /// tmux pane id, e.g. "%3". Stable for the pane's lifetime.
    pub pane_id: String,
    /// Window id, e.g. "@1".
    pub window_id: String,
    /// Window name.
    pub window_name: String,
    /// Pane title (OSC 0/2, or set via select-pane -T).
    pub title: String,
    /// Process exited (visible with remain-on-exit).
    pub dead: bool,
    /// Pane is in a mode (copy mode and friends). Injection gate input.
    pub in_mode: bool,
    /// Foreground command name, e.g. "zsh".
    pub current_command: String,
    /// Grid width in cells.
    pub width: u32,
    /// Grid height in cells.
    pub height: u32,
    /// Active pane of its window.
    pub active: bool,
    /// Pid of the pane's root process (what tmux spawned into the pane).
    /// Daemon-internal: sender identity walks socket-peer ancestry to this
    /// pid; it never enters PaneStatus. -1 when tmux reports no process:
    /// next-3.8 stops reporting `#{pane_pid}` for a dead pane while 3.6a
    /// keeps returning the stale pid, so a dead pane's pid is one of the
    /// two depending on the tmux underneath (MEASURED, both). A change
    /// (respawn-pane) emits [`PaneField::PanePid`] so consumers can retire
    /// the former process generation before trusting the replacement.
    pub pane_pid: i32,
}

impl PaneRow {
    /// Bridge to the wire-protocol shape. The daemon supplies what tmux
    /// cannot know: the cyclops label, manifest id, and fused agent state.
    pub fn to_status(
        &self,
        agent: Option<String>,
        manifest: Option<String>,
        state: cyclops_proto::AgentState,
    ) -> cyclops_proto::PaneStatus {
        cyclops_proto::PaneStatus {
            pane_id: self.pane_id.clone(),
            window_id: self.window_id.clone(),
            window_name: self.window_name.clone(),
            agent,
            manifest,
            title: self.title.clone(),
            current_command: self.current_command.clone(),
            dead: self.dead,
            in_mode: self.in_mode,
            width: self.width,
            height: self.height,
            state,
            // Readiness is fusion's stamp, and the adapter has no fusion:
            // refused here, filled in by the daemon alongside the fields
            // below. Absent evidence is not permission.
            write_ready: false,
            write_block: None,
            composer: cyclops_proto::ComposerState::ComposerAmbiguous,
            composer_proof: cyclops_proto::ComposerProof::Unprovable,
            notification_attempt: None,
            composer_reason: None,
            composer_candidates: 0,
            notification_state: None,
            message_state: None,
            next_action: None,
            // Elapsed-in-state, hook liveness, and the manifest's display
            // name are daemon knowledge; the daemon fills all three in.
            state_ms: None,
            working_confirmed: None,
            hooks_verified: None,
            manifest_display_name: None,
        }
    }
}

/// Which fields of a pane changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneField {
    /// Pane moved to another window.
    WindowId,
    /// Window name changed.
    WindowName,
    /// Pane title changed.
    Title,
    /// pane_dead flipped.
    Dead,
    /// pane_in_mode flipped.
    InMode,
    /// Foreground command changed.
    CurrentCommand,
    /// The process tmux spawned into the pane changed.
    PanePid,
    /// Width or height changed.
    Size,
    /// Active flag flipped.
    Active,
}

/// High-level watcher events on the broadcast channel.
#[derive(Debug, Clone)]
pub enum PaneEvent {
    /// A pane appeared.
    PaneAdded(PaneRow),
    /// A pane disappeared. Carries the pane id.
    PaneRemoved(String),
    /// One or more fields of a tracked pane changed.
    PaneChanged {
        /// Pane id.
        id: String,
        /// Which fields changed.
        changed: Vec<PaneField>,
        /// The row after the change.
        row: PaneRow,
    },
    /// The pane produced output. Rate limited to one event per pane per
    /// [`OUTPUT_RATE_LIMIT`].
    OutputActivity {
        /// Pane id.
        pane_id: String,
        /// Unix ms when the event was emitted.
        ts: u64,
    },
    /// Flow control paused the pane (already auto-resumed by the client).
    Paused {
        /// Pane id.
        pane_id: String,
    },
    /// Flow control resumed the pane.
    Resumed {
        /// Pane id.
        pane_id: String,
    },
    /// This watcher's own session was renamed. The stable `$id` either
    /// matched a notification or resolved the new name after a failed
    /// snapshot. The internal target already reflects `name` when sent.
    SessionRenamed {
        /// The session's new name.
        name: String,
    },
    /// An authoritative pane-table reconciliation completed. Consumers that
    /// track process generations separately from tmux fields use this edge to
    /// compare their bindings even when every visible [`PaneRow`] field stayed
    /// the same.
    Reconciled,
    /// The control connection died. The table is frozen at its last state;
    /// the owner reconnects by building a new watcher.
    Disconnected,
}

/// Owns a control client and maintains the reconciling pane table for one
/// session.
pub struct SessionWatcher {
    client: Arc<ControlClient>,
    table: Arc<StdMutex<HashMap<String, PaneRow>>>,
    events: broadcast::Sender<PaneEvent>,
    reconcile_tx: mpsc::Sender<oneshot::Sender<Result<(), TmuxError>>>,
    task: StdMutex<Option<JoinHandle<()>>>,
    /// Session name this watcher currently targets (`list-panes -s -t` and
    /// every other session-scoped command). Shared with the event loop
    /// (`LoopCtx::session_shared`) so a followed rename updates both at
    /// once, synchronously with the `SessionRenamed` event that reports it.
    session: Arc<StdMutex<String>>,
    /// This watcher's own stable tmux `$id`, resolved once at connect
    /// ([`resolve_session_id`]). Renaming a session never changes its id
    /// (F37), which is what tells a rename of THIS session apart from a
    /// rename of some other one that happens to collide on name during a
    /// swap. None when the probe failed; every `%session-renamed` then
    /// falls back to a bare reconcile hint for this connection's life.
    session_id: Option<String>,
}

impl SessionWatcher {
    /// Spawn the control client, take the bootstrap snapshot, subscribe to
    /// per-pane formats, and start the event loop.
    pub async fn connect(cfg: ControlConfig) -> Result<SessionWatcher, TmuxError> {
        let session = cfg.session.clone();
        let (client, notif_rx) = ControlClient::spawn(cfg).await?;
        let client = Arc::new(client);
        // Resolved before the bootstrap reconcile so a rename notification
        // arriving during bootstrap can already be matched; a failed probe
        // degrades to "never follow a rename on this connection" rather
        // than blocking connect.
        let session_id = resolve_session_id(&client, &session).await;
        let table: Arc<StdMutex<HashMap<String, PaneRow>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(256);
        let session_shared = Arc::new(StdMutex::new(session.clone()));

        let mut ctx = LoopCtx {
            client: Arc::clone(&client),
            session: session.clone(),
            session_shared: Arc::clone(&session_shared),
            session_id: session_id.clone(),
            table: Arc::clone(&table),
            events: events.clone(),
            last_output: HashMap::new(),
        };

        // Bootstrap: full snapshot before any subscriber exists, so the
        // events it produces vanish and the table starts authoritative.
        if let Err(e) = reconcile_current_session(&mut ctx).await {
            client.shutdown().await;
            return Err(e);
        }

        // Arm the death edge once. It covers panes that do not exist yet:
        // tmux expands an all-panes format for whatever the session holds
        // at each evaluation, so nothing resubscribes on a split. Not
        // fatal when it fails: the rest of the watcher is unaffected and
        // deaths go back to being noticed by the next reconcile, whenever
        // that is (F25).
        if let Err(e) = subscribe_dead(&client).await {
            warn!(error = %e, "dead-pane subscription failed; pane_dead flips will be late or missed");
        }

        let (reconcile_tx, reconcile_rx) = mpsc::channel(16);
        let task = tokio::spawn(watch_loop(ctx, notif_rx, reconcile_rx));

        Ok(SessionWatcher {
            client,
            table,
            events,
            reconcile_tx,
            task: StdMutex::new(Some(task)),
            session: session_shared,
            session_id,
        })
    }

    /// Current session name this watcher covers. Starts as the connect-time
    /// name. A rename notification or stable-id recovery updates it before
    /// emitting [`PaneEvent::SessionRenamed`]. A caller still addresses its
    /// daemon slot by stable index because the event can remain in flight.
    pub fn session(&self) -> String {
        self.session.lock().expect("session name lock").clone()
    }

    /// This watcher's own tmux `$id`, if the connect-time probe resolved
    /// one. The daemon binds durable session identity to this immutable id;
    /// [`PaneEvent::SessionRenamed`] carries display-name changes.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The underlying control client, for typed commands (capture, paste,
    /// send-keys). Shares the connection the watcher listens on.
    pub fn client(&self) -> Arc<ControlClient> {
        Arc::clone(&self.client)
    }

    /// Current pane table, sorted by pane id for stable output.
    pub fn snapshot(&self) -> Vec<PaneRow> {
        let mut rows: Vec<PaneRow> = self
            .table
            .lock()
            .expect("table lock")
            .values()
            .cloned()
            .collect();
        rows.sort_by_key(|r| pane_id_ordinal(&r.pane_id));
        rows
    }

    /// One pane by id.
    pub fn pane(&self, pane_id: &str) -> Option<PaneRow> {
        self.table.lock().expect("table lock").get(pane_id).cloned()
    }

    /// Subscribe to the event stream. Late subscribers get events from now
    /// on; the table itself is always available via [`Self::snapshot`].
    pub fn subscribe(&self) -> broadcast::Receiver<PaneEvent> {
        self.events.subscribe()
    }

    /// Reconcile against authoritative state right now (explicit doubt).
    /// Runs on the event loop so it serializes with hint-driven reconciles;
    /// returns once the table reflects the query result.
    ///
    /// `Err(Timeout)` means the query reply was late, nothing more: the
    /// table kept its prior state and the connection is still up. Only
    /// `Err(Disconnected)` means the watcher is dead and needs rebuilding.
    pub async fn reconcile_now(&self) -> Result<(), TmuxError> {
        let (tx, rx) = oneshot::channel();
        self.reconcile_tx
            .send(tx)
            .await
            .map_err(|_| TmuxError::Disconnected)?;
        rx.await.map_err(|_| TmuxError::Disconnected)?
    }

    /// Stop the loop and shut the control client down.
    pub async fn shutdown(&self) {
        if let Some(t) = self.task.lock().expect("task lock").take() {
            t.abort();
        }
        self.client.shutdown().await;
    }
}

/// State owned by the event loop.
struct LoopCtx {
    client: Arc<ControlClient>,
    /// Working copy of the session name, read by every session-scoped
    /// command (`list_panes`, `session_gone`). No lock: only this task's
    /// own synchronous notification handling ever writes it.
    session: String,
    /// Mirror of `session` for [`SessionWatcher::session`]'s callers.
    /// Updated in the same synchronous step as `session` on a followed
    /// rename, before the `SessionRenamed` event that announces it is sent.
    session_shared: Arc<StdMutex<String>>,
    /// This watcher's own tmux `$id`, resolved once at connect. See the
    /// field of the same name on [`SessionWatcher`] for what it is for.
    session_id: Option<String>,
    table: Arc<StdMutex<HashMap<String, PaneRow>>>,
    events: broadcast::Sender<PaneEvent>,
    /// Per-pane instant of the last OutputActivity emission.
    last_output: HashMap<String, Instant>,
}

/// What a handled notification asks the loop to do next.
enum Action {
    /// Nothing further.
    None,
    /// Arm (or keep) the debounced reconcile.
    Hint,
    /// The connection is over.
    Disconnect,
}

async fn watch_loop(
    mut ctx: LoopCtx,
    mut notif_rx: mpsc::UnboundedReceiver<Notification>,
    mut reconcile_rx: mpsc::Receiver<oneshot::Sender<Result<(), TmuxError>>>,
) {
    // One-shot deadline armed by hints. No hint, no timer: zero polling.
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            n = notif_rx.recv() => match n {
                Some(n) => match handle_notification(&mut ctx, n) {
                    Action::None => {}
                    Action::Hint => {
                        if deadline.is_none() {
                            deadline = Some(tokio::time::Instant::now() + RECONCILE_DEBOUNCE);
                        }
                    }
                    Action::Disconnect => {
                        let _ = ctx.events.send(PaneEvent::Disconnected);
                        break;
                    }
                },
                None => {
                    let _ = ctx.events.send(PaneEvent::Disconnected);
                    break;
                }
            },
            req = reconcile_rx.recv() => match req {
                Some(ack) => {
                    let res = reconcile_current_session(&mut ctx).await;
                    let disconnected = matches!(res, Err(TmuxError::Disconnected));
                    let failed = res.is_err();
                    let _ = ack.send(res);
                    deadline = None;
                    // A non-Disconnected failure here may be the session
                    // itself having been destroyed out from under a client
                    // tmux switched to a survivor instead of dropping (see
                    // the hint-deadline arm below); the probe is what tells
                    // the two apart.
                    let target = ctx.session_id.as_deref().unwrap_or(&ctx.session);
                    if disconnected || (failed && session_gone(&ctx.client, target).await) {
                        let _ = ctx.events.send(PaneEvent::Disconnected);
                        break;
                    }
                }
                None => break, // watcher dropped
            },
            _ = tokio::time::sleep_until(
                deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if deadline.is_some() => {
                deadline = None;
                match reconcile_current_session(&mut ctx).await {
                    Ok(()) => {}
                    Err(TmuxError::Disconnected) => {
                        let _ = ctx.events.send(PaneEvent::Disconnected);
                        break;
                    }
                    Err(e) => {
                        // Killing the watched session does not always
                        // disconnect this client: tmux can switch it to a
                        // surviving session instead of detaching it, and
                        // every reconcile against the dead name then fails
                        // the same way forever without this probe ever
                        // telling the loop to stop.
                        let target = ctx.session_id.as_deref().unwrap_or(&ctx.session);
                        if session_gone(&ctx.client, target).await {
                            let _ = ctx.events.send(PaneEvent::Disconnected);
                            break;
                        }
                        warn!(error = %e, "hint-driven reconcile failed");
                    }
                }
            }
        }
    }
}

fn handle_notification(ctx: &mut LoopCtx, n: Notification) -> Action {
    match n {
        Notification::Output { pane, .. } | Notification::ExtendedOutput { pane, .. } => {
            output_activity(ctx, pane)
        }
        // Ordered before the per-pane arm: DEAD_SUB shares the prefix.
        Notification::SubscriptionChanged {
            name, pane, value, ..
        } if name == DEAD_SUB => match pane {
            Some(pane) => dead_edge(ctx, &pane, &value),
            None => Action::Hint,
        },
        Notification::SubscriptionChanged {
            name, pane, value, ..
        } if name.starts_with(SUB_PREFIX) => match pane {
            Some(pane) => apply_sub_value(ctx, &pane, &value),
            None => Action::Hint,
        },
        Notification::SubscriptionChanged { name, .. } => {
            debug!(%name, "ignoring foreign subscription");
            Action::None
        }
        Notification::Pause { pane } => {
            let _ = ctx.events.send(PaneEvent::Paused { pane_id: pane });
            Action::None
        }
        Notification::Continue { pane } => {
            let _ = ctx.events.send(PaneEvent::Resumed { pane_id: pane });
            Action::None
        }
        Notification::Exit { reason } => {
            debug!(?reason, "control connection exiting");
            Action::Disconnect
        }
        Notification::SessionRenamed { session, name } => {
            handle_session_renamed(ctx, session, name)
        }
        // Structural hints: something about the pane population or window
        // metadata changed. Reconcile finds out what.
        Notification::WindowAdd { .. }
        | Notification::WindowClose { .. }
        | Notification::WindowRenamed { .. }
        | Notification::UnlinkedWindowAdd { .. }
        | Notification::UnlinkedWindowClose { .. }
        | Notification::UnlinkedWindowRenamed { .. }
        | Notification::LayoutChange { .. }
        | Notification::WindowPaneChanged { .. }
        | Notification::PaneModeChanged { .. }
        | Notification::SessionsChanged
        | Notification::SessionChanged { .. } => Action::Hint,
        Notification::ClientDetached { .. }
        | Notification::ClientSessionChanged { .. }
        | Notification::Other(_) => Action::None,
    }
}

/// A `%session-renamed`. Followed only when it names THIS watcher's own
/// session: the id resolved at connect (`ctx.session_id`) matches the
/// notification's. tmux identifies every renamed session by stable id,
/// including background ones (F37), which is exactly what makes that
/// comparison meaningful — a bare name compare could not tell "our session
/// was renamed" from "some other session picked up a name that reminds
/// tmux of ours for one notification".
///
/// A match updates `ctx.session` (what `list_panes`/`session_gone` target)
/// and its shared mirror in the same synchronous step, then sends
/// [`PaneEvent::SessionRenamed`] on the same ordered channel every other
/// `PaneEvent` travels — the daemon's own rename of its session slot relies
/// on seeing this event before any later event from this loop, and nothing
/// here awaits between the two writes and the send, so no other code runs
/// in between.
///
/// A non-match (different session, or no id resolved — older tmux, or the
/// connect-time probe failed) is left exactly as it was before this
/// feature: a bare reconcile hint. This is the guard the name-swap edge and
/// the zombie-teardown path (`watcher_zombie_session.rs`) both depend on —
/// a rename of a DIFFERENT session, including the survivor a zombie client
/// got switched to, must never make a dead watcher's session name follow
/// along.
fn handle_session_renamed(ctx: &mut LoopCtx, session: Option<String>, name: String) -> Action {
    let is_own_session = matches!(
        (&ctx.session_id, &session),
        (Some(id), Some(renamed)) if id == renamed
    );
    if !is_own_session {
        return Action::Hint;
    }
    follow_session_name(ctx, name);
    Action::None
}

fn follow_session_name(ctx: &mut LoopCtx, name: String) -> bool {
    if ctx.session == name {
        return false;
    }
    ctx.session = name.clone();
    *ctx.session_shared.lock().expect("session name lock") = name.clone();
    let _ = ctx.events.send(PaneEvent::SessionRenamed { name });
    true
}

/// Snapshot this watcher's stable session id and refresh its display name.
/// A mutable name is only a fallback when the connect-time id probe failed.
async fn reconcile_current_session(ctx: &mut LoopCtx) -> Result<(), TmuxError> {
    let Some(session_id) = ctx.session_id.clone() else {
        return reconcile(ctx).await;
    };
    if let Some(name) = recover_session_name(ctx, &session_id).await {
        follow_session_name(ctx, name);
    }
    reconcile_target(ctx, &session_id).await
}

async fn recover_session_name(ctx: &LoopCtx, session_id: &str) -> Option<String> {
    let target = format!("{session_id}:");
    let cmd = format!(
        "display-message -p -t {} {}",
        quote_arg(&target),
        quote_arg("#{session_name}")
    );
    let Ok(mut lines) = ctx.client.command(&cmd).await else {
        return None;
    };
    if lines.len() != 1 || lines[0].is_empty() {
        return None;
    }
    Some(lines.remove(0))
}

/// Emit OutputActivity, rate limited per pane. Output from a pane the table
/// does not know is a staleness signal: reconcile.
fn output_activity(ctx: &mut LoopCtx, pane: String) -> Action {
    let known = ctx.table.lock().expect("table lock").contains_key(&pane);
    let now = Instant::now();
    let emit = match ctx.last_output.get(&pane) {
        Some(prev) => now.duration_since(*prev) >= OUTPUT_RATE_LIMIT,
        None => true,
    };
    if emit {
        ctx.last_output.insert(pane.clone(), now);
        let _ = ctx.events.send(PaneEvent::OutputActivity {
            pane_id: pane,
            ts: unix_ms(),
        });
    }
    if known {
        Action::None
    } else {
        Action::Hint
    }
}

/// Handle a push from [`DEAD_SUB`]: hint when it disagrees with the table,
/// stay silent when it does not.
///
/// A hint rather than a direct write: a death moves more of the row than
/// this one field (next-3.8 drops pane_pid along with it), and reconcile is
/// the single place that builds a whole row out of tmux's answer. The
/// disagreement check is what keeps the subscription free: every pane's
/// first push repeats the flag the bootstrap snapshot already recorded, so
/// a session of live panes arms no reconcile at all.
fn dead_edge(ctx: &LoopCtx, pane: &str, value: &str) -> Action {
    let dead = match value {
        "0" => false,
        "1" => true,
        _ => {
            warn!(%pane, %value, "malformed dead subscription value");
            return Action::Hint;
        }
    };
    match ctx.table.lock().expect("table lock").get(pane) {
        Some(row) if row.dead == dead => Action::None,
        // Stale flag, or a pane the table has never seen. Either way the
        // table is behind tmux.
        _ => Action::Hint,
    }
}

/// Apply a pushed subscription value to the table. The value is tmux's own
/// format expansion, so it is authoritative for the fields it carries; an
/// unknown pane means the table is stale and needs a reconcile.
fn apply_sub_value(ctx: &mut LoopCtx, pane: &str, value: &str) -> Action {
    // Fields in reverse: pane_pid, current_command, in_mode, dead, then
    // title (which may itself contain tabs and so is taken as the
    // remainder).
    let mut it = value.rsplitn(5, '\t');
    let (Some(pid), Some(cmd), Some(in_mode), Some(dead), Some(title)) =
        (it.next(), it.next(), it.next(), it.next(), it.next())
    else {
        // F50: every observed occurrence is a clean-prefix truncation tmux
        // itself sent short (tmux 3.7b, trigger not identified); the Hint
        // below already self-heals via reconcile, so this is diagnostic
        // noise, not a correctness signal.
        debug!(%pane, %value, "malformed subscription value");
        return Action::Hint;
    };
    let Some(pid) = parse_pane_pid(pid) else {
        warn!(%pane, %value, "malformed pane_pid in subscription value");
        return Action::Hint;
    };
    let mut table = ctx.table.lock().expect("table lock");
    let Some(row) = table.get_mut(pane) else {
        return Action::Hint;
    };
    let mut changed = Vec::new();
    if row.title != title {
        row.title = title.to_string();
        changed.push(PaneField::Title);
    }
    let dead = dead == "1";
    if row.dead != dead {
        row.dead = dead;
        changed.push(PaneField::Dead);
    }
    let in_mode = in_mode == "1";
    if row.in_mode != in_mode {
        row.in_mode = in_mode;
        changed.push(PaneField::InMode);
    }
    if row.current_command != cmd {
        row.current_command = cmd.to_string();
        changed.push(PaneField::CurrentCommand);
    }
    if row.pane_pid != pid {
        row.pane_pid = pid;
        changed.push(PaneField::PanePid);
    }
    if changed.is_empty() {
        return Action::None;
    }
    let row = row.clone();
    drop(table);
    let _ = ctx.events.send(PaneEvent::PaneChanged {
        id: pane.to_string(),
        changed,
        row,
    });
    Action::None
}

/// Whether `session` is gone for good, as far as a reconcile failure can be
/// blamed on it.
///
/// A reconcile error that is not [`TmuxError::Disconnected`] is ambiguous by
/// itself: it may be the session having been destroyed, or something
/// unrelated and transient. `has-session` resolves it — a `%error` reply
/// means tmux no longer has a session by that name, which reconcile's own
/// `list-panes` cannot distinguish from any other command failure. The probe
/// finding the connection itself gone is folded in here too, so the caller
/// has one question to ask instead of two. Anything else (the session still
/// exists, or the probe failed for an unrelated reason such as a timeout) is
/// not proof of anything and must not tear the watcher down on a guess.
async fn session_gone(client: &ControlClient, session: &str) -> bool {
    match client
        .command(&format!("has-session -t {}", quote_arg(session)))
        .await
    {
        Ok(_) => false,
        Err(TmuxError::Disconnected) | Err(TmuxError::Command(_)) => true,
        Err(_) => false,
    }
}

/// Resolve this session's stable tmux `$id` right after attach, the same
/// format [`crate::snapshot`] already reads (`#{session_id}`). Renaming a
/// session never changes its id (F37); this is what later lets a followed
/// rename be told apart from a rename of some other session. `=` is the
/// same exact-match rule [`crate::cmd::session_target`] documents
/// everywhere else in this crate — without it a session named as a prefix
/// of another could resolve the wrong id.
///
/// The target carries a trailing `:` (F53, MEASURED): `display-message -t`
/// needs to resolve down to a pane to evaluate anything, and a bare
/// `=session` with no window/pane part resolves to nothing on tmux 3.7b —
/// empty output, exit 0, no error — where plain `session` (no `=`) falls
/// back to the current window and works. `=session:` names the session's
/// window list the same way [`ControlClient::move_window_to_session`]'s
/// target does, which gives tmux a window to fall back to and restores the
/// exact-match safety at the same time.
///
/// None on any failure (tmux gone, session vanished between spawn and this
/// call): the watcher still works, `%session-renamed` just falls back to a
/// bare reconcile hint for this connection's whole life, same as before
/// this feature existed.
async fn resolve_session_id(client: &ControlClient, session: &str) -> Option<String> {
    let cmd = format!(
        "display-message -p -t {} {}",
        quote_arg(&format!("{}:", crate::cmd::session_target(session))),
        quote_arg("#{session_id}")
    );
    match client.command(&cmd).await {
        Ok(mut lines) if !lines.is_empty() => Some(lines.remove(0)),
        Ok(_) => {
            warn!(%session, "session id probe returned no output; rename-follow disabled for this watcher");
            None
        }
        Err(e) => {
            warn!(%session, error = %e, "cannot resolve session id; rename-follow disabled for this watcher");
            None
        }
    }
}

/// Query authoritative state and fold it into the table: diff, emit events,
/// keep subscriptions in step with the pane population.
async fn reconcile(ctx: &mut LoopCtx) -> Result<(), TmuxError> {
    let session = ctx.session.clone();
    reconcile_target(ctx, &session).await
}

async fn reconcile_target(ctx: &mut LoopCtx, session: &str) -> Result<(), TmuxError> {
    let fresh = list_panes(&ctx.client, session).await?;

    let mut added: Vec<PaneRow> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut updated: Vec<(String, Vec<PaneField>, PaneRow)> = Vec::new();
    {
        let mut table = ctx.table.lock().expect("table lock");
        let fresh_map: HashMap<String, PaneRow> =
            fresh.into_iter().map(|r| (r.pane_id.clone(), r)).collect();
        for id in table.keys() {
            if !fresh_map.contains_key(id) {
                removed.push(id.clone());
            }
        }
        for (id, new_row) in &fresh_map {
            match table.get(id) {
                None => added.push(new_row.clone()),
                Some(old) => {
                    let diff = diff_fields(old, new_row);
                    if !diff.is_empty() {
                        updated.push((id.clone(), diff, new_row.clone()));
                    }
                }
            }
        }
        *table = fresh_map;
    }

    for id in &removed {
        ctx.last_output.remove(id);
        let _ = ctx.events.send(PaneEvent::PaneRemoved(id.clone()));
        // Bare name removes the subscription (MEASURED on 3.6a). Best
        // effort: a vanished pane's subscription just stops reporting.
        if let Err(e) = ctx
            .client
            .command(&format!("refresh-client -B {}", quote_arg(&sub_name(id))))
            .await
        {
            debug!(pane = %id, error = %e, "unsubscribe failed");
        }
    }
    for row in added {
        if let Err(e) = subscribe_pane(&ctx.client, &row.pane_id).await {
            // The pane may have died between snapshot and subscribe; the
            // next hint reconciles it away.
            warn!(pane = %row.pane_id, error = %e, "pane subscription failed");
        }
        let _ = ctx.events.send(PaneEvent::PaneAdded(row));
    }
    for (id, changed, row) in updated {
        let _ = ctx.events.send(PaneEvent::PaneChanged { id, changed, row });
    }
    let _ = ctx.events.send(PaneEvent::Reconciled);
    Ok(())
}

fn diff_fields(old: &PaneRow, new: &PaneRow) -> Vec<PaneField> {
    let mut d = Vec::new();
    if old.window_id != new.window_id {
        d.push(PaneField::WindowId);
    }
    if old.window_name != new.window_name {
        d.push(PaneField::WindowName);
    }
    if old.title != new.title {
        d.push(PaneField::Title);
    }
    if old.dead != new.dead {
        d.push(PaneField::Dead);
    }
    if old.in_mode != new.in_mode {
        d.push(PaneField::InMode);
    }
    if old.current_command != new.current_command {
        d.push(PaneField::CurrentCommand);
    }
    if old.pane_pid != new.pane_pid {
        d.push(PaneField::PanePid);
    }
    if old.width != new.width || old.height != new.height {
        d.push(PaneField::Size);
    }
    if old.active != new.active {
        d.push(PaneField::Active);
    }
    d
}

/// Full authoritative snapshot via list-panes.
async fn list_panes(client: &ControlClient, session: &str) -> Result<Vec<PaneRow>, TmuxError> {
    let cmd = format!(
        "list-panes -s -t {} -F {}",
        quote_arg(session),
        quote_arg(PANE_FORMAT)
    );
    let out = client.command(&cmd).await?;
    let mut rows = Vec::with_capacity(out.len());
    for line in &out {
        match parse_pane_row(line) {
            Some(r) => rows.push(r),
            None => warn!(%line, "unparseable list-panes row"),
        }
    }
    Ok(rows)
}

/// Parse one PANE_FORMAT row. Ids from the left, fixed fields from the
/// right, window_name/title split on the first tab between them.
fn parse_pane_row(line: &str) -> Option<PaneRow> {
    let mut left = line.splitn(3, '\t');
    let pane_id = left.next()?;
    let window_id = left.next()?;
    let rest = left.next()?;
    let mut right = rest.rsplitn(8, '\t');
    let pane_pid = right.next()?;
    let active = right.next()?;
    let height = right.next()?;
    let width = right.next()?;
    let current_command = right.next()?;
    let in_mode = right.next()?;
    let dead = right.next()?;
    let middle = right.next()?;
    let (window_name, title) = middle.split_once('\t')?;
    Some(PaneRow {
        pane_id: pane_id.to_string(),
        window_id: window_id.to_string(),
        window_name: window_name.to_string(),
        title: title.to_string(),
        dead: dead == "1",
        in_mode: in_mode == "1",
        current_command: current_command.to_string(),
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        active: active == "1",
        pane_pid: parse_pane_pid(pane_pid)?,
    })
}

/// Parse a `#{pane_pid}` field. Empty means the pane has no process, which
/// is -1, not a parse failure.
///
/// next-3.8 reports nothing there for a dead pane; 3.6a reports the stale
/// pid (MEASURED, both). Rejecting the empty field would drop the whole row
/// from the snapshot on next-3.8, and the row is the record that the pane
/// is dead: reconcile would read the death as a removal and delete the pane
/// the death edge just asked it to look at.
fn parse_pane_pid(field: &str) -> Option<i32> {
    if field.is_empty() {
        return Some(-1);
    }
    field.parse().ok()
}

fn sub_name(pane_id: &str) -> String {
    format!("{SUB_PREFIX}{}", pane_id.trim_start_matches('%'))
}

async fn subscribe_pane(client: &ControlClient, pane_id: &str) -> Result<(), TmuxError> {
    let arg = format!("{}:{}:{}", sub_name(pane_id), pane_id, SUB_FORMAT);
    client
        .command(&format!("refresh-client -B {}", quote_arg(&arg)))
        .await
        .map(|_| ())
}

/// Subscribe to pane_dead for every pane in the session at once (`%*`).
async fn subscribe_dead(client: &ControlClient) -> Result<(), TmuxError> {
    let arg = format!("{DEAD_SUB}:%*:{DEAD_FORMAT}");
    client
        .command(&format!("refresh-client -B {}", quote_arg(&arg)))
        .await
        .map(|_| ())
}

/// Numeric ordinal of a pane id for stable sorting; unparseable ids sort
/// last in lexical order.
fn pane_id_ordinal(pane_id: &str) -> (u64, String) {
    match pane_id.trim_start_matches('%').parse::<u64>() {
        Ok(n) => (n, String::new()),
        Err(_) => (u64::MAX, pane_id.to_string()),
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn stale_session_context(
        server: &cyclops_testrig::TmuxServer,
        session: &str,
    ) -> (LoopCtx, broadcast::Receiver<PaneEvent>) {
        let cfg = ControlConfig::attach(session)
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _notifications) = ControlClient::spawn(cfg).await.unwrap();
        let client = Arc::new(client);
        let session_id = resolve_session_id(&client, session).await;
        let (events, receiver) = broadcast::channel(16);
        (
            LoopCtx {
                client,
                session: session.to_string(),
                session_shared: Arc::new(StdMutex::new(session.to_string())),
                session_id,
                table: Arc::new(StdMutex::new(HashMap::new())),
                events,
                last_output: HashMap::new(),
            },
            receiver,
        )
    }

    #[tokio::test]
    async fn a_failed_snapshot_recovers_the_same_session_by_id() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("watcher-early-rename");
        server.run_ok(&["new-session", "-d", "-s", "before"]);
        let (mut ctx, mut events) = stale_session_context(&server, "before").await;
        assert!(ctx.session_id.is_some());

        server.run_ok(&["rename-session", "-t", "=before", "after"]);
        reconcile_current_session(&mut ctx).await.unwrap();

        assert_eq!(ctx.session, "after");
        assert_eq!(ctx.session_shared.lock().unwrap().as_str(), "after");
        assert!(matches!(
            events.try_recv(),
            Ok(PaneEvent::SessionRenamed { name }) if name == "after"
        ));
        while events.try_recv().is_ok() {}
        let session_id = ctx.session_id.clone().unwrap();
        let _ = handle_session_renamed(&mut ctx, Some(session_id), "after".into());
        assert!(events.try_recv().is_err());
        ctx.client.shutdown().await;
    }

    #[tokio::test]
    async fn a_reused_name_cannot_redirect_the_authoritative_snapshot() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("watcher-reused-name");
        server.run_ok(&["new-session", "-d", "-s", "before"]);
        server.run_ok(&["new-session", "-d", "-s", "other"]);
        let pane_id = |session: &str| {
            let out = server.run(&["list-panes", "-t", session, "-F", "#{pane_id}"]);
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let original = pane_id("=before");
        let replacement = pane_id("=other");
        let (mut ctx, _events) = stale_session_context(&server, "before").await;
        reconcile_current_session(&mut ctx).await.unwrap();

        server.run_ok(&["rename-session", "-t", "=before", "after"]);
        server.run_ok(&["rename-session", "-t", "=other", "before"]);
        reconcile_current_session(&mut ctx).await.unwrap();

        {
            let table = ctx.table.lock().unwrap();
            assert!(table.contains_key(&original));
            assert!(!table.contains_key(&replacement));
        }
        assert_eq!(ctx.session, "after");
        ctx.client.shutdown().await;
    }

    #[tokio::test]
    async fn a_deleted_session_id_cannot_recover_as_a_survivor() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("watcher-deleted-session");
        server.run_ok(&["new-session", "-d", "-s", "dies"]);
        server.run_ok(&["new-session", "-d", "-s", "survivor"]);
        server.run_ok(&["set-option", "-g", "detach-on-destroy", "off"]);
        let (mut ctx, mut events) = stale_session_context(&server, "dies").await;

        server.run_ok(&["kill-session", "-t", "=dies"]);
        assert!(reconcile_current_session(&mut ctx).await.is_err());

        assert_eq!(ctx.session, "dies");
        assert!(events.try_recv().is_err());
        ctx.client.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_pid_edge_emits_even_when_every_other_field_is_stable() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("watcher-sub-pid");
        server.run_ok(&["new-session", "-d", "-s", "pid-edge"]);
        let (mut ctx, mut events) = stale_session_context(&server, "pid-edge").await;
        reconcile_current_session(&mut ctx).await.unwrap();
        while events.try_recv().is_ok() {}
        let row = ctx
            .table
            .lock()
            .unwrap()
            .values()
            .next()
            .cloned()
            .expect("bootstrap pane");
        let replacement_pid = row.pane_pid + 10_000;
        let value = format!(
            "{}\t{}\t{}\t{}\t{}",
            row.title,
            u8::from(row.dead),
            u8::from(row.in_mode),
            row.current_command,
            replacement_pid
        );

        assert!(matches!(
            apply_sub_value(&mut ctx, &row.pane_id, &value),
            Action::None
        ));
        let event = events.try_recv().expect("pid edge event");
        assert!(matches!(
            event,
            PaneEvent::PaneChanged { id, changed, row: changed_row }
                if id == row.pane_id
                    && changed == vec![PaneField::PanePid]
                    && changed_row.pane_pid == replacement_pid
        ));
        ctx.client.shutdown().await;
    }

    #[tokio::test]
    async fn an_unchanged_authoritative_snapshot_still_emits_a_reconcile_edge() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("watcher-reconcile-edge");
        server.run_ok(&["new-session", "-d", "-s", "stable"]);
        let (mut ctx, mut events) = stale_session_context(&server, "stable").await;
        reconcile_current_session(&mut ctx).await.unwrap();
        while events.try_recv().is_ok() {}

        reconcile_current_session(&mut ctx).await.unwrap();

        assert!(matches!(events.try_recv(), Ok(PaneEvent::Reconciled)));
        assert!(events.try_recv().is_err());
        ctx.client.shutdown().await;
    }

    #[test]
    fn pane_row_parses_and_survives_tabbed_title() {
        let line = "%3\t@1\tmain\tsome title\t0\t1\tzsh\t120\t30\t1\t4242";
        let r = parse_pane_row(line).unwrap();
        assert_eq!(r.pane_id, "%3");
        assert_eq!(r.window_id, "@1");
        assert_eq!(r.window_name, "main");
        assert_eq!(r.title, "some title");
        assert!(!r.dead);
        assert!(r.in_mode);
        assert_eq!(r.current_command, "zsh");
        assert_eq!((r.width, r.height), (120, 30));
        assert!(r.active);
        assert_eq!(r.pane_pid, 4242);

        // A tab inside the title stays inside the title.
        let line = "%0\t@0\tw\ttab\there\t1\t0\tcat\t80\t24\t0\t99";
        let r = parse_pane_row(line).unwrap();
        assert_eq!(r.title, "tab\there");
        assert!(r.dead);
        assert_eq!(r.pane_pid, 99);

        assert!(parse_pane_row("garbage").is_none());
        assert!(parse_pane_row("%0\t@0\tw\tt\t1\t0\tcat\t80\tNaN\t0\t99").is_none());
        assert!(parse_pane_row("%0\t@0\tw\tt\t1\t0\tcat\t80\t24\t0\tNaN").is_none());
    }

    #[test]
    fn dead_pane_row_survives_an_empty_pid() {
        // What next-3.8 sends for a pane whose process has exited. The row
        // has to parse or the death reads as a removal (F25).
        let line = "%1\t@0\tw\tt\t1\t0\tsleep\t120\t14\t0\t";
        let r = parse_pane_row(line).unwrap();
        assert!(r.dead);
        assert_eq!(r.pane_pid, -1);
    }

    #[test]
    fn sub_names_track_pane_ids() {
        assert_eq!(sub_name("%0"), "cyp0");
        assert_eq!(sub_name("%17"), "cyp17");
    }

    #[test]
    fn diff_reports_each_field_once() {
        let a = parse_pane_row("%1\t@0\tw\tt\t0\t0\tzsh\t80\t24\t1\t7").unwrap();
        let mut b = a.clone();
        assert!(diff_fields(&a, &b).is_empty());
        b.title = "other".into();
        b.height = 12;
        b.in_mode = true;
        let d = diff_fields(&a, &b);
        assert_eq!(
            d,
            vec![PaneField::Title, PaneField::InMode, PaneField::Size]
        );

        let mut c = a.clone();
        c.pane_pid = 8;
        assert_eq!(diff_fields(&a, &c), vec![PaneField::PanePid]);
    }
}
