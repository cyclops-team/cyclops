//! cyclopsd: the Cyclops daemon. One process, one socket, one record.
//!
//! ## What it owns
//!
//! - The watched sessions: one control-mode connection each, through
//!   `cyclops-tmux`, and the pane table it reconciles (`watcher.rs` is
//!   that crate's; the session slots and the reconnect loop are here).
//! - The verdict about what each pane is doing (`fusion.rs`), fusing
//!   manifest rules over title and screen with hook edges (`ack.rs`).
//! - The record: one crash-safe NDJSON ledger per watched session, and
//!   the read side over it (`history.rs`: `msg.history`, `msg.thread`).
//! - Delivery (`delivery.rs`, docs/development/DELIVERY.md): the gate, the paste,
//!   the ACK tiers, quota parking, `agent.wait`, restart-limbo closure.
//! - Who is who: sender identity from socket peer credentials
//!   (`identity.rs`) and the durable adoption roster
//!   (`registry.rs`).
//! - The one thing cyclops draws itself, the pane border
//!   (`chrome.rs`), and hook liveness plus the self-test
//!   (`selftest.rs`).
//! - The socket (`server.rs`): every method in docs/reference/PROTOCOL.md.
//!
//! ## What it does not own
//!
//! - Talking to tmux. Every invocation goes through `cyclops-tmux`; see
//!   that crate's header for the rule and its one live exception, which
//!   is `probe_tmux` in this file.
//! - What a rule means. Detection rules are `cyclops-manifest` data, and
//!   a new agent CLI is a TOML file, never a change here.
//! - What needs a human. That rule has one home,
//!   `cyclops_proto::attention`; this crate answers `status` with the same
//!   predicate and decides nothing extra.
//! - Rendering. Nothing here formats for a human except the border, and
//!   the border's colors are `cyclops-theme` tokens.
//!
//! ## Zero polling
//!
//! Nothing here re-queries state on a clock. Every timer is a one-shot
//! attached to one thing that already happened: the per-pane output settle
//! debounce, the watcher reconnect backoff, the delivery pipeline's verify
//! re-reads, ACK windows and decline spacing, the gate's single
//! wedged-hold ping, and the deadlines a caller asked for
//! (`receipt_block_ms`, `agent.wait`'s `timeout_ms`). No interval exists.
//!
//! The crate is a library so integration tests boot the daemon in-process;
//! main.rs is a thin wrapper adding signals and logging.

mod ack;
mod chrome;
pub mod config;
mod delivery;
mod fusion;
mod history;
pub mod identity;
mod registry;
mod selftest;
mod server;
mod workspace_ui;

pub use config::Config;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cyclops_ledger::LedgerWriter;
use cyclops_manifest::Manifest;
use cyclops_proto::{
    AdminNotifyParams, AgentState, Detection, Event, Kind, LedgerLine, MsgSendParams,
    StateReportParams, WireError,
};
use cyclops_tmux::{
    ControlConfig, PaneEvent, PaneField, PaneRow, SessionWatcher, TmuxError, TmuxVersion,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Recompute a pane this long after its last output activity settles.
const OUTPUT_SETTLE: Duration = Duration::from_millis(300);
/// Watcher reconnect backoff bounds (reconnects only, never state polls).
const RECONNECT_MIN: Duration = Duration::from_millis(200);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// How long shutdown waits for tasks before aborting them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Event broadcast capacity. Doubles as every subscriber's buffer: a
/// briefly-stalled client must survive soak-rate events (the m1 soak
/// measured drops at ~2.5s of stall on the old 1024), while a truly wedged
/// client still lags out and is dropped by the server.
const EVENT_BUFFER: usize = 8192;

/// Test seam: an async pause awaited inside the delivery injection path at
/// a named phase ("pre_paste", "pre_submit"), installed via
/// [`Daemon::set_inject_pause`]. Always None in production.
pub(crate) type InjectPause = Arc<
    dyn Fn(&'static str) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Shared daemon state. Everything the socket server and the fusion engine
/// need lives here behind one Arc.
pub(crate) struct Inner {
    pub(crate) cfg: Config,
    pub(crate) boot_id: String,
    pub(crate) started: Instant,
    /// Raw `tmux -V` text, "unavailable" when the probe failed.
    pub(crate) tmux_version: String,
    /// Keyed and iterated by agent id, so process-name binding is
    /// deterministic.
    pub(crate) manifests: BTreeMap<String, Manifest>,
    /// The directory the manifests above were read from, resolved once at
    /// boot. None when no directory was found at all. Kept because the
    /// resolution consults the working directory (see
    /// [`Config::manifest_dir`]) and a later reader has no way to redo it:
    /// `status` reports this path to whoever has to fix an unknown pane.
    pub(crate) manifest_dir: Option<PathBuf>,
    /// One slot per watched session: every session named in `sessions` at
    /// boot, plus any [`watch_session`] has added since.
    ///
    /// Append-only, and that append-only-ness is the whole reason this is
    /// safe: a `session_idx` handed to a spawned task, an in-flight
    /// request, or a delivery handle stays valid for the daemon's life,
    /// because no element is ever removed or reordered. Go through
    /// [`Inner::session`], [`Inner::session_count`], [`Inner::session_index`],
    /// or [`Inner::session_slots`] rather than touching the `Vec` directly,
    /// and never hold this lock across an `.await` or while taking another
    /// lock: every read here is a snapshot (clone of the `Arc`s, or of the
    /// whole `Vec`) taken and released before any work happens.
    pub(crate) sessions: StdMutex<Vec<Arc<SessionSlot>>>,
    /// Push stream for events.subscribe connections.
    pub(crate) events: broadcast::Sender<Event>,
    /// Cached fusion verdict per pane id.
    pub(crate) detections: StdMutex<HashMap<String, DetEntry>>,
    /// Adoption registry: which pane wears which label, what manifest is
    /// pinned to it, and the tmux chrome it wore before cyclops arrived.
    /// Explicit adoption via pane.label (v1 keeper), durable across
    /// restarts (src/cyclopsd/src/registry.rs).
    pub(crate) registry: StdMutex<registry::Registry>,
    /// Active theme for the pane border chrome, re-stat'ed on the state
    /// change that is about to repaint (cyclops-theme's hot reload rule:
    /// the stat rides an event, no timer exists).
    pub(crate) theme: StdMutex<cyclops_theme::ThemeWatch>,
    /// Latest hook sensor reading per pane id (agent.state.report), plus
    /// the aging state that keeps a stale edge from pinning fused state.
    pub(crate) hook_readings: StdMutex<HashMap<String, fusion::HookEntry>>,
    /// argv-basename cache for manifest binding, per (pane id, pane pid).
    /// Filled lazily when comm-name binding misses (F21); entries die with
    /// the pane. Only a basename that actually bound a manifest is ever
    /// stored, so a miss means "not settled yet" rather than "no agent" —
    /// see [`fusion::argv_bound_manifest`] for the exec race that rule
    /// exists to survive.
    pub(crate) argv_cache: StdMutex<HashMap<(String, i32), String>>,
    /// Delivery pipeline state.
    pub(crate) engine: delivery::Engine,
    /// Hook report dedupe state.
    pub(crate) ack_state: ack::AckState,
    /// Per-pane hook edge record behind hooks_verified, hooks.verify, and
    /// the F1 downgrade notification (amendment c).
    pub(crate) hook_liveness: selftest::HookLiveness,
    /// Test-only injection pause, see [`InjectPause`].
    pub(crate) inject_pause: StdMutex<Option<InjectPause>>,
    /// Test-only: make the `--clear` chrome restore fail the way tmux
    /// refusing a command would. See [`Daemon::fail_chrome_restore`].
    pub(crate) fail_chrome_restore: AtomicBool,
    /// Last-active workspace/tab for the terminal workspace UI.
    pub(crate) workspace_ui: StdMutex<workspace_ui::WorkspaceUiState>,
    /// The shutdown signal, readable so a session watched after boot
    /// ([`watch_session`]) can hand its `session_task` the same receiver
    /// every configured session got.
    pub(crate) stop: watch::Receiver<bool>,
    /// Handles for tasks spawned after boot (currently: `session_task` for
    /// a dynamically watched session). [`Daemon::shutdown`] joins these
    /// alongside `Daemon.tasks` so a session added at runtime shuts down
    /// exactly as cleanly as one that was configured.
    pub(crate) extra_tasks: StdMutex<Vec<JoinHandle<()>>>,
}

pub(crate) struct SessionSlot {
    /// Mutable so a followed session rename (`PaneEvent::SessionRenamed`,
    /// `handle_pane_event`) can update it in place: `session_index` then
    /// keeps resolving this same slot under the new name instead of a
    /// `watch_session` for the new name opening a second slot + watcher for
    /// one tmux session. Go through [`SessionSlot::name`] and
    /// [`SessionSlot::rename`] rather than the field directly.
    name: StdMutex<String>,
    pub(crate) link: StdMutex<SessionLink>,
    /// Append-only session ledger at $CYCLOPS_HOME/ledger/<session>.ndjson,
    /// opened once when the slot is created (`boot`, `watch_session`) and
    /// held open for the slot's life. A followed rename does NOT reopen it:
    /// appends keep landing in the file the watcher was attached under when
    /// it started, because the OS handle this holds is keyed by inode, not
    /// by the path or by `name` above. `<new-name>.ndjson` is only opened
    /// if a later boot or runtime `session.watch` registers that name as a
    /// fresh slot — a deliberate minimal choice: the alternative (closing
    /// this handle and opening a second file mid-session) would split one
    /// session's record across two files with no line in either saying so.
    pub(crate) ledger: Arc<LedgerWriter>,
    /// Pane table as of the last detach. Hook reports arriving while the
    /// control connection is down resolve against this (a report does not
    /// need the tmux connection); the live table wins whenever attached.
    pub(crate) last_panes: StdMutex<HashMap<String, PaneRow>>,
}

impl SessionSlot {
    /// A freshly attached slot: no link yet, no pane history yet. `boot`
    /// and `watch_session` both build slots this way so the two paths a
    /// session can join the daemon by stay in lockstep.
    pub(crate) fn new(name: String, ledger: Arc<LedgerWriter>) -> Self {
        SessionSlot {
            name: StdMutex::new(name),
            link: StdMutex::new(SessionLink::default()),
            ledger,
            last_panes: StdMutex::new(HashMap::new()),
        }
    }

    /// Current session name. `session_index` and every display surface
    /// (`status`, `history`) read this rather than the tmux `$id`: cyclops
    /// names sessions to humans, and the id is watcher-internal machinery
    /// for telling one rename apart from another (see
    /// `cyclops_tmux::SessionWatcher::session_id`).
    pub(crate) fn name(&self) -> String {
        self.name.lock().expect("session name lock").clone()
    }

    /// Rename in place, in response to a followed `%session-renamed`. The
    /// ledger, the last-known pane table, and every adoption stay attached
    /// to this same slot; only the lookup key changes. Returns the prior
    /// name exactly once so two concurrent observations of one rename do
    /// not append two rename facts.
    fn rename(&self, new_name: String) -> Option<String> {
        let mut name = self.name.lock().expect("session name lock");
        if *name == new_name {
            return None;
        }
        Some(std::mem::replace(&mut *name, new_name))
    }
}

#[derive(Default)]
pub(crate) struct SessionLink {
    pub(crate) attached: bool,
    pub(crate) watcher: Option<Arc<SessionWatcher>>,
}

pub(crate) struct DetEntry {
    pub(crate) detection: Detection,
    /// Manifest id bound at the last recompute.
    pub(crate) manifest: Option<String>,
    /// When the fused STATE last changed, not when it was last computed.
    /// A recompute that lands on the same state keeps this, which is what
    /// lets `status` say "working for 13m" instead of "working since the
    /// last event". The roster's elapsed column reads it.
    pub(crate) since: std::time::Instant,
}

/// A ledger line the daemon itself is authoring, not relaying for an agent.
/// `seq`, `boot_id`, and `ts` are placeholders here on purpose: the ledger
/// writer fills all three in at append time (`cyclops-ledger`), so a value
/// set here would just be discarded. `from` is always "cyclopsd" — every
/// caller of this constructor is the daemon reporting on itself (state
/// changes, boot/attach/detach/rename facts, gate/delivery-state lines,
/// self-test results), never a message on an agent's behalf. Callers that
/// need `to`, `subject`, `body`, or `deliveries` set those on the result.
pub(crate) fn daemon_line(kind: Kind, id: String, data: Value) -> LedgerLine {
    LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id,
        ts: 0,
        kind,
        from: "cyclopsd".to_string(),
        to: Vec::new(),
        subject: None,
        body: None,
        reply_to: None,
        deliveries: Vec::new(),
        data: Some(data),
    }
}

impl Inner {
    /// The slot at `idx`, if one exists. Locks, clones the `Arc`, releases:
    /// safe to call right before an `.await`.
    pub(crate) fn session(&self, idx: usize) -> Option<Arc<SessionSlot>> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .get(idx)
            .cloned()
    }

    /// How many sessions are watched right now (configured plus any added
    /// since boot).
    pub(crate) fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions lock").len()
    }

    /// The index of a watched session by name, if it is already watched.
    pub(crate) fn session_index(&self, name: &str) -> Option<usize> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .iter()
            .position(|s| s.name() == name)
    }

    /// A cloned snapshot of every session slot, for iteration. Lock the
    /// sessions vector, clone every `Arc`, release: the lock never has to
    /// span the loop body that follows.
    pub(crate) fn session_slots(&self) -> Vec<Arc<SessionSlot>> {
        self.sessions.lock().expect("sessions lock").clone()
    }

    /// Emit a fused-state change: one kind=state ledger line on the
    /// session's ledger plus a "state" event. Gate/state lines carry rule
    /// ids and causes, never raw screen captures (secrets rule).
    ///
    /// Takes the session's slot index directly rather than resolving it
    /// from a name here. A name resolved through `watcher.session()` can be
    /// ahead of this daemon's own `SessionSlot::rename` — the watcher
    /// updates its name live, at notification time, while the matching
    /// slot rename only lands when this process gets around to handling
    /// the `PaneEvent::SessionRenamed` that follows it — so a caller
    /// recomputing a pane on an event that predates the rename must not
    /// re-derive the index from the (already new) name at emit time, or
    /// `session_index` misses during that window and the append silently
    /// drops (seq `None`). Every caller already carries a stable
    /// `session_idx` from where it entered the session (`session_task`'s
    /// `idx`, `handle.session_idx`, `resolve_recipient`'s return, ...),
    /// which is append-only-stable for the daemon's life; passing that
    /// through closes the window instead of reopening it here.
    pub(crate) fn emit_state(
        &self,
        session_idx: usize,
        pane_id: &str,
        det: &Detection,
        prior: Option<AgentState>,
        cause: &str,
    ) {
        let target = self
            .label_of(pane_id)
            .unwrap_or_else(|| pane_id.to_string());
        let seq = self.append_line(
            session_idx,
            daemon_line(
                Kind::State,
                self.mint_event_id(),
                json!({
                    "pane_id": pane_id,
                    "target": target,
                    "state": det.state,
                    "prior": prior,
                    "disagreement": det.disagreement,
                    "decided_by": det.decided_by,
                    "cause": cause,
                }),
            ),
        );
        self.emit(
            "state",
            json!({
                "target": target,
                "pane_id": pane_id,
                "state": det.state,
                "prior": prior,
                "disagreement": det.disagreement,
                "decided_by": det.decided_by,
            }),
            seq,
        );
    }

    /// Broadcast one event. No subscribers is normal.
    pub(crate) fn emit(&self, event: &str, data: Value, seq: Option<u64>) {
        let _ = self.events.send(Event {
            event: event.to_string(),
            data,
            seq,
        });
    }

    /// Append one line to a session ledger. Append failures are loud but
    /// never take the daemon down mid-delivery.
    pub(crate) fn append_line(&self, session_idx: usize, line: LedgerLine) -> Option<u64> {
        let slot = self.session(session_idx)?;
        match slot.ledger.append(line) {
            Ok(l) => Some(l.seq),
            Err(e) => {
                error!(session = %slot.name(), error = %e, "ledger append failed");
                None
            }
        }
    }

    /// Short id for standalone ledger events (system/state lines that are
    /// not tied to a message).
    pub(crate) fn mint_event_id(&self) -> String {
        format!("e-{}", &uuid::Uuid::new_v4().simple().to_string()[..6])
    }

    /// Cached fused state for a pane; Unknown when never computed.
    pub(crate) fn cached_state(&self, pane_id: &str) -> AgentState {
        self.detections
            .lock()
            .expect("detections lock")
            .get(pane_id)
            .map(|e| e.detection.state)
            .unwrap_or(AgentState::Unknown)
    }

    /// Label of a pane, if adopted.
    pub(crate) fn label_of(&self, pane_id: &str) -> Option<String> {
        self.registry
            .lock()
            .expect("registry lock")
            .label_of(pane_id)
    }

    /// pane id -> label for every adopted pane. The surfaces that render
    /// or resolve the whole roster take this snapshot rather than holding
    /// the registry lock across their own work.
    pub(crate) fn labels(&self) -> HashMap<String, String> {
        self.registry.lock().expect("registry lock").labels()
    }

    /// The theme for the next chrome write.
    ///
    /// The stat rides the edge that is about to repaint, which is
    /// cyclops-theme's reload contract: an edit to the active theme, or a
    /// `cyclops theme <name>` that moved the config key, reaches the
    /// borders on the next state change, and no timer exists.
    ///
    /// Warnings are taken, not read: a refusal to reload a half-written
    /// file has to be logged once per edit, and reading them without
    /// draining logged the same line on every repaint after it.
    pub(crate) fn theme_now(&self) -> cyclops_theme::Theme {
        let mut watch = self.theme.lock().expect("theme lock");
        watch.refresh();
        for w in watch.take_warnings() {
            warn!("theme: {w}");
        }
        watch.theme().clone()
    }

    /// Live watcher for a session slot, if attached.
    pub(crate) fn watcher_of(&self, session_idx: usize) -> Option<Arc<SessionWatcher>> {
        let slot = self.session(session_idx)?;
        let link = slot.link.lock().expect("session link lock");
        link.watcher.as_ref().map(Arc::clone)
    }

    /// Resolve a recipient/target name: label first, then pane id. Only
    /// panes that currently exist resolve.
    pub(crate) fn resolve_recipient(&self, name: &str) -> Option<(usize, String)> {
        let wanted = self.label_target(name);
        for (idx, slot) in self.session_slots().into_iter().enumerate() {
            let watcher = {
                let link = slot.link.lock().expect("session link lock");
                link.watcher.as_ref().map(Arc::clone)
            };
            if let Some(w) = watcher {
                if let Some(row) = w.pane(&wanted) {
                    return Some((idx, row.pane_id));
                }
            }
        }
        None
    }

    /// Resolve a name against the last-known pane tables of DETACHED
    /// sessions. Hook reports do not need the tmux connection: a report for
    /// a pane that existed at detach must still match ACKs, or every detach
    /// blinds tier 1 (the m1 soak's duplicate-delivery failure).
    pub(crate) fn resolve_recipient_last_known(&self, name: &str) -> Option<(usize, PaneRow)> {
        let wanted = self.label_target(name);
        for (idx, slot) in self.session_slots().into_iter().enumerate() {
            if slot.link.lock().expect("session link lock").attached {
                continue; // live table is authoritative while attached
            }
            let last = slot.last_panes.lock().expect("last panes lock");
            if let Some(row) = last.get(&wanted) {
                return Some((idx, row.clone()));
            }
        }
        None
    }

    /// Pane id a label points at, or the name itself when unlabeled.
    fn label_target(&self, name: &str) -> String {
        self.registry
            .lock()
            .expect("registry lock")
            .pane_for_label(name)
            .unwrap_or_else(|| name.to_string())
    }
}

/// A booted daemon. Dropping it does not stop the tasks; call
/// [`Daemon::shutdown`] for a clean exit (detach watchers, remove socket).
pub struct Daemon {
    inner: Arc<Inner>,
    stop: watch::Sender<bool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
}

impl Daemon {
    /// Path of the Unix socket this daemon serves.
    pub fn socket_path(&self) -> PathBuf {
        self.inner.cfg.home.join(cyclops_proto::SOCK_NAME)
    }

    /// Clean shutdown: put every adopted pane's border back, signal every
    /// task, let session tasks detach their control clients, then remove
    /// the socket file. Delivery workers are aborted; queued deliveries
    /// stay recorded in the ledger.
    ///
    /// The chrome restore comes first because it needs the control
    /// connections the stop signal is about to close. The adoptions
    /// themselves stay in the registry: a restart re-adopts and repaints.
    pub async fn shutdown(&self) {
        restore_all_chrome(&self.inner).await;
        let _ = self.stop.send(true);
        let mut tasks: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.tasks.lock().expect("tasks lock"));
        // Tasks spawned after boot (watch_session's session_task) shut down
        // exactly like the ones boot spawned: same stop signal, same
        // grace-then-abort below.
        tasks.extend(std::mem::take(
            &mut *self.inner.extra_tasks.lock().expect("extra tasks lock"),
        ));
        for mut task in tasks {
            if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
        let workers: Vec<JoinHandle<()>> = std::mem::take(
            &mut *self
                .inner
                .engine
                .worker_tasks
                .lock()
                .expect("worker tasks lock"),
        );
        for w in workers {
            w.abort();
        }
        let _ = std::fs::remove_file(self.socket_path());
        info!("cyclopsd stopped");
    }

    /// Subscribe to the daemon event stream (msg, delivery-state, gate,
    /// admin-notify, state, session). Same stream events.subscribe serves.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// In-process `status`, byte for byte the answer the socket serves.
    ///
    /// The eye's whole register comes from this one answer, so a test that
    /// drives the daemon and then hand-builds a snapshot proves nothing
    /// about what a reader sees. `open_deliveries` is the delivery half of
    /// the count (`cyclops_proto::attention`); any caller drawing the eye
    /// asks for it.
    pub fn status(&self, open_deliveries: bool) -> cyclops_proto::StatusResult {
        server::status_result(&self.inner, open_deliveries)
    }

    /// In-process msg.send with an already-resolved sender. The socket
    /// path resolves the sender from peer credentials first; embedders and
    /// tests supply it directly.
    pub async fn msg_send(&self, from: &str, params: MsgSendParams) -> Result<Value, WireError> {
        delivery::msg_send(&self.inner, from, params).await
    }

    /// In-process agent.state.report with a pre-trusted origin, mirroring
    /// [`Daemon::msg_send`]'s design: embedders and tests call this
    /// directly. The SOCKET path instead pins every report to the
    /// reporting pane via peer credentials and denies everything else,
    /// because hook reports are liveness and ACK evidence.
    pub async fn report_state(&self, params: StateReportParams) -> Result<Value, WireError> {
        ack::handle_report(&self.inner, params).await
    }

    /// In-process hooks.verify: hook liveness for one target pane.
    pub async fn hooks_verify(
        &self,
        params: cyclops_proto::HooksVerifyParams,
    ) -> Result<Value, WireError> {
        selftest::verify(&self.inner, params).await
    }

    /// In-process hooks.selftest: one fyi marker through the delivery
    /// pipeline, reporting whether the ACK hook fired with it.
    pub async fn hooks_selftest(
        &self,
        params: cyclops_proto::HooksSelftestParams,
    ) -> Result<Value, WireError> {
        selftest::selftest(&self.inner, params).await
    }

    /// In-process admin.notify.
    pub async fn admin_notify(&self, params: AdminNotifyParams) -> Result<Value, WireError> {
        let seq = delivery::admin_notify(
            &self.inner,
            params.level,
            &params.subject,
            &params.body,
            None,
            None,
            delivery::About::default(),
        );
        Ok(json!({"notified": true, "seq": seq}))
    }

    /// Test-only seam: pause the delivery injection path at a named phase
    /// ("pre_paste", "pre_submit"), between the gate's admit and the
    /// occupant re-check, so integration tests can change the pane
    /// occupant deterministically. Not part of the public API surface.
    #[doc(hidden)]
    pub fn set_inject_pause<F>(&self, f: F)
    where
        F: Fn(&'static str) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        *self.inner.inject_pause.lock().expect("inject pause lock") = Some(Arc::new(f));
    }

    /// Test-only seam: from here on, the chrome restore behind `--clear`
    /// fails as tmux refusing the command would. Not part of the public API
    /// surface.
    ///
    /// It exists because the branch it reaches cannot be reached any other
    /// way on demand. The real failure is tmux refusing a `set-option`, and
    /// the only ways to cause that (kill the pane, kill the server) also
    /// destroy the tmux state every assertion in such a test has to read
    /// afterwards to prove the settings survived.
    #[doc(hidden)]
    pub fn fail_chrome_restore(&self, on: bool) {
        self.inner.fail_chrome_restore.store(on, Ordering::SeqCst);
    }

    /// Adopt a pane under a label, or un-adopt it. `target` is a pane id or
    /// an existing label; `label: None` clears.
    pub async fn label_pane(
        &self,
        target: &str,
        label: Option<String>,
        manifest: Option<String>,
    ) -> Result<Value, WireError> {
        label_pane(&self.inner, target, label, manifest).await
    }
}

/// Adopt a pane under a label, or un-adopt it. Labels are the adoption
/// registry: they name senders, resolve recipients, and define the "*"
/// broadcast domain.
///
/// This is the half both verbs share: what has to be true before anything
/// changes, and what has to be recorded once it has. The steps are ordered
/// because each one can only be undone before the next has happened:
///
/// 1. Resolve the target to a live pane. Nothing is written for a name
///    that points at no pane.
/// 2. Validate the label. Reserved names and duplicates are refused here,
///    while nothing has changed yet.
/// 3. Validate an explicit `--manifest` against the loaded set, so a typo
///    is an error instead of a pane that silently detects nothing.
/// 4. Hand the pane to [`adopt_pane`] or [`unadopt_pane`], which own the
///    registry write and the chrome between them.
/// 5. Append the system ledger line and emit the event.
pub(crate) async fn label_pane(
    inner: &Arc<Inner>,
    target: &str,
    label: Option<String>,
    manifest: Option<String>,
) -> Result<Value, WireError> {
    // 1. Resolve.
    let Some((session_idx, pane_id)) = inner.resolve_recipient(target) else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("no such target {target:?}"),
            data: None,
        });
    };
    let session = inner
        .session(session_idx)
        .expect("session_idx valid: resolve_recipient just returned it")
        .name();
    let label = label.filter(|l| !l.is_empty());

    // 2. Validate the label. The rule and its wording are
    //    cyclops_proto::label, so every surface refuses the same names
    //    with the same sentence.
    if let Some(l) = &label {
        if let Some(why) = cyclops_proto::label::refusal(l) {
            return Err(bad_request(why));
        }
        let holder = {
            let reg = inner.registry.lock().expect("registry lock");
            if reg.label_taken_by_other(l, &pane_id) {
                let pane = reg.pane_for_label(l).expect("taken means a holder exists");
                reg.get(&pane).cloned()
            } else {
                None
            }
        };
        if let Some(holder) = holder {
            return Err(bad_request(label_taken_words(inner, l, &holder)));
        }
    }

    // 3. Validate the manifest pin.
    if let Some(m) = &manifest {
        if !inner.manifests.contains_key(m) {
            let known: Vec<&str> = inner.manifests.keys().map(String::as_str).collect();
            return Err(bad_request(if known.is_empty() {
                format!("no manifest {m:?}; this daemon loaded none at all")
            } else {
                format!("no manifest {m:?}; loaded: {}", known.join(", "))
            }));
        }
    }

    // 4. Adopt or un-adopt.
    let watcher = inner.watcher_of(session_idx);
    match &label {
        Some(l) => {
            adopt_pane(
                inner,
                watcher.as_ref(),
                session_idx,
                target,
                &pane_id,
                l,
                manifest.as_deref(),
            )
            .await?
        }
        None => unadopt_pane(inner, watcher.as_ref(), session_idx, &pane_id).await?,
    }

    // 5. Record.
    let seq = inner.append_line(
        session_idx,
        daemon_line(
            Kind::System,
            inner.mint_event_id(),
            json!({
                "event": "pane_labeled",
                "pane_id": pane_id,
                "label": label,
                "manifest": manifest,
            }),
        ),
    );
    inner.emit(
        "session",
        json!({"name": session, "pane_labeled": pane_id, "label": label}),
        seq,
    );
    Ok(json!({
        "target": target,
        "pane_id": pane_id,
        "label": label,
        "manifest": manifest,
        // What actually binds the pane now, which is not the same as the
        // pin above: the pin is what the caller asked for and is usually
        // absent, and adoption re-reads the pane (adopt_pane step 4) so
        // this is the verdict a delivery would gate on. Null means nothing
        // binds it, and a named pane nothing binds can receive no message.
        // Additive: an older client ignores the field.
        "detects_as": label.as_ref().and_then(|_| inner
            .detections
            .lock()
            .expect("detections lock")
            .get(&pane_id)
            .and_then(|e| e.manifest.clone())),
    }))
}

/// Why a name cannot be claimed: who wears it now, where, and the way
/// out. "already taken" alone once had an operator staring at an empty
/// roster and a refused name at the same time, with nothing to act on.
fn label_taken_words(inner: &Inner, label: &str, holder: &registry::Adoption) -> String {
    let attached = inner
        .session_index(&holder.session)
        .and_then(|idx| inner.session(idx))
        .map(|slot| slot.link.lock().expect("session link lock").attached)
        .unwrap_or(false);
    if attached {
        format!(
            "label {label:?} is already taken by {pane} in session {session} ({state}). \
             Free it with: cyclops name {pane} --clear, or pick another name.",
            pane = holder.pane_id,
            session = holder.session,
            state = cyclops_proto::state_words(inner.cached_state(&holder.pane_id)),
        )
    } else {
        format!(
            "label {label:?} is already taken by {pane} in session {session}, which cyclops \
             is not attached to right now. Pick another name, or once cyclops is watching \
             that session again (opening its workspace re-attaches it), clear it: \
             cyclops name {pane} --clear.",
            pane = holder.pane_id,
            session = holder.session,
        )
    }
}

/// Put one pane on the roster under `label` and paint the border that says
/// so.
///
/// The order is the crash story: the registry is the durable fact and the
/// border is decoration, so a crash between them leaves a named pane
/// wearing stale decoration rather than decoration nobody can take off.
///
/// 1. Read what tmux looked like before cyclops, and only the half that is
///    not already on file: re-reading a pane cyclops already painted would
///    record cyclops's own border as the thing to restore.
/// 2. Write the registry.
/// 3. Paint, from the state already on file so the border is never blank.
///    A tmux failure is logged and does not fail the verb: the pane is
///    adopted either way, and decoration is not the record.
/// 4. Re-read the pane, which is what makes an explicit `--manifest` take
///    effect now instead of at the next unrelated event; it repaints again
///    if the pin changed the verdict.
async fn adopt_pane(
    inner: &Arc<Inner>,
    watcher: Option<&Arc<SessionWatcher>>,
    session_idx: usize,
    target: &str,
    pane_id: &str,
    label: &str,
    manifest: Option<&str>,
) -> Result<(), WireError> {
    let Some(row) = watcher.and_then(|w| w.pane(pane_id)) else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("no such target {target:?}"),
            data: None,
        });
    };
    // 1. Read, ONCE.
    //
    // The two halves are decided separately because they belong to
    // different things. The pane's format is already recorded if this pane
    // was adopted before, and the window's status is already recorded if
    // any adopted pane is already in this window. Either can be known
    // while the other is not: renaming a pane that has since moved windows
    // is exactly that case.
    let (known_format, known_status) = {
        let reg = inner.registry.lock().expect("registry lock");
        (
            reg.get(pane_id).map(|a| a.border_format.clone()),
            reg.window(&row.window_id).map(|w| w.border_status.clone()),
        )
    };
    let read = match watcher {
        Some(w) if known_format.is_none() || known_status.is_none() => {
            match chrome::snapshot(&w.client(), pane_id, &row.window_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(pane = %pane_id, error = %e, "cannot read pane chrome; adopting without it");
                    chrome::Snapshot::none()
                }
            }
        }
        _ => chrome::Snapshot::none(),
    };
    // 2. Write the registry.
    let session = inner
        .session(session_idx)
        .expect("session_idx valid: caller resolved it")
        .name();
    let adoption = registry::Adoption {
        session: session.clone(),
        pane_id: pane_id.to_string(),
        label: label.to_string(),
        manifest: manifest.map(str::to_string),
        pane_pid: row.pane_pid,
        window_id: row.window_id.clone(),
        border_format: known_format.unwrap_or(read.border_format),
    };
    let window = registry::WindowChrome {
        session,
        window_id: row.window_id.clone(),
        border_status: known_status.unwrap_or(read.border_status),
    };
    if let Err(e) = inner
        .registry
        .lock()
        .expect("registry lock")
        .adopt(adoption, window)
    {
        return Err(WireError {
            code: "internal".to_string(),
            message: format!("cannot record the adoption: {e}"),
            data: None,
        });
    }
    // 3. Paint.
    paint_chrome(inner, session_idx, pane_id).await;
    // 4. Re-read.
    if let Some(w) = watcher {
        fusion::recompute_pane(inner, session_idx, w, pane_id, false, "pane_labeled").await;
    }
    Ok(())
}

/// Take a pane off the roster and give tmux its border back.
///
/// The order IS the correctness here. The registry entry is the only copy
/// on the machine of the pane's own `pane-border-format` and its window's
/// own `pane-border-status`: tmux has been overwritten and the user wrote
/// those values down nowhere else. Forgetting the entry first and
/// restoring afterwards means one tmux failure destroys both at once, with
/// the pane still wearing cyclops's decoration and nothing left that knows
/// what was under it. Worse, the next `name` re-snapshots the pane and
/// records CYCLOPS's format as the thing to restore, so the user gets
/// cyclops's border back believing it is theirs.
///
/// So the snapshot is the last thing to go:
///
/// 1. Read what the clear would hand back, WITHOUT committing it.
/// 2. Restore the border from that, while the entry still exists. The
///    window's own border status comes back only when this was the last
///    adopted pane in that window.
/// 3. A failed restore stops here. The entry stays exactly as it is, so a
///    later `--clear` restores the same values, and the pane stays named,
///    so no re-adoption ever re-snapshots cyclops's own decoration.
/// 4. Forget the entry, now that what it carried is back on tmux.
/// 5. Re-read the pane. The pin went with the name, so detection goes back
///    to working the manifest out from the process, and that is an event
///    rather than something to notice later.
async fn unadopt_pane(
    inner: &Arc<Inner>,
    watcher: Option<&Arc<SessionWatcher>>,
    session_idx: usize,
    pane_id: &str,
) -> Result<(), WireError> {
    // 1. Look, do not commit.
    let pending = inner
        .registry
        .lock()
        .expect("registry lock")
        .pending_clear(pane_id);
    // 2. Restore, with the snapshot still on file.
    if let (Some((adoption, freed)), Some(w)) = (&pending, watcher) {
        if let Err(e) = restore_for_clear(inner, w, adoption, freed.as_ref()).await {
            // 3. Keep the name rather than the decoration: see above.
            warn!(pane = %pane_id, error = %e, "cannot restore pane chrome; the name is kept so the snapshot survives");
            return Err(WireError {
                code: "chrome_not_restored".to_string(),
                message: chrome_not_restored(adoption, freed.as_ref(), &e),
                data: None,
            });
        }
    }
    // 4. Forget it. A write that fails here has already put the border
    //    back, and the entry it could not drop still holds the ORIGINAL
    //    values, so the retry restores the same thing twice and nothing is
    //    poisoned.
    inner
        .registry
        .lock()
        .expect("registry lock")
        .clear(pane_id)
        .map_err(|e| WireError {
            code: "internal".to_string(),
            message: format!("cannot record the change: {e}"),
            data: None,
        })?;
    // 5. Re-read.
    if let Some(w) = watcher {
        fusion::recompute_pane(inner, session_idx, w, pane_id, false, "pane_unlabeled").await;
    }
    Ok(())
}

/// The chrome restore `--clear` runs, with the test seam in front of it.
/// Production installs no seam and this is exactly [`chrome::restore`].
async fn restore_for_clear(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    adoption: &registry::Adoption,
    freed: Option<&registry::WindowChrome>,
) -> Result<(), TmuxError> {
    if inner.fail_chrome_restore.load(Ordering::SeqCst) {
        return Err(TmuxError::Command("forced by the test seam".to_string()));
    }
    chrome::restore(&watcher.client(), inner.cfg.chrome, adoption, freed).await
}

/// What a failed `--clear` leaves behind, and how to finish it.
///
/// Three things the user has to be told, because the pane looks unchanged
/// from the outside: the clear did not happen, the decoration on screen is
/// still cyclops's, and the name was kept on purpose rather than by
/// accident. Naming the tmux options makes the state checkable by hand;
/// naming the command makes it fixable without one.
fn chrome_not_restored(
    adoption: &registry::Adoption,
    freed: Option<&registry::WindowChrome>,
    cause: &TmuxError,
) -> String {
    let window = match freed {
        Some(w) => format!(
            ", and window {} still wears cyclops's pane-border-status",
            w.window_id
        ),
        None => String::new(),
    };
    format!(
        "couldn't put {pane}'s tmux border back: {cause}. {pane} is still named \"{label}\" and still wears cyclops's pane-border-format{window}. Your own settings live only in that name's record, so the name is kept until they are back on tmux. Retry with: cyclops name {pane} --clear",
        pane = adoption.pane_id,
        label = adoption.label,
    )
}

/// Put every adopted pane's border back the way cyclops found it, without
/// touching the registry: the panes stay adopted, they just stop wearing
/// cyclops's decoration while no daemon is running to keep it true.
///
/// A window's border status is put back once, on the first of its adopted
/// panes this loop reaches; the rest only need their own pane options
/// removed. (`--clear` gets there the other way round, on the last pane
/// out, because there the window keeps its text while a named pane is
/// still in it.)
async fn restore_all_chrome(inner: &Arc<Inner>) {
    for (idx, slot) in inner.session_slots().into_iter().enumerate() {
        let Some(watcher) = inner.watcher_of(idx) else {
            continue;
        };
        let adoptions = inner
            .registry
            .lock()
            .expect("registry lock")
            .in_session(&slot.name());
        let mut restored_windows: Vec<String> = Vec::new();
        for adoption in adoptions {
            let window_snapshot = if restored_windows.contains(&adoption.window_id) {
                None
            } else {
                restored_windows.push(adoption.window_id.clone());
                inner
                    .registry
                    .lock()
                    .expect("registry lock")
                    .window(&adoption.window_id)
                    .cloned()
            };
            if let Err(e) = chrome::restore(
                &watcher.client(),
                inner.cfg.chrome,
                &adoption,
                window_snapshot.as_ref(),
            )
            .await
            {
                warn!(pane = %adoption.pane_id, error = %e, "cannot restore pane chrome at shutdown");
            }
        }
    }
}

fn bad_request(message: String) -> WireError {
    WireError {
        code: "bad_request".to_string(),
        message,
        data: None,
    }
}

/// Paint these adopted panes' borders, through this session's watcher, in
/// the theme that is active right now.
///
/// The one place a border is painted. Four of the eight write edges land
/// here (adoption, a session attach, a window move, a theme switch), and
/// before this existed each carried its own copy of the same four steps:
/// read the theme, read the pane's cached state, apply, warn. Three copies
/// meant three answers to "which panes get repainted, with what", and
/// three wordings of the same failure in the log.
///
/// Silent on an empty set, which is what a session with nothing adopted
/// gives. `chrome = "off"` is chrome.rs's answer, not this function's.
async fn paint_adoptions(
    inner: &Arc<Inner>,
    watcher: &SessionWatcher,
    adoptions: &[registry::Adoption],
) {
    if adoptions.is_empty() {
        return;
    }
    // One theme for the whole set: a file edited between two panes would
    // otherwise leave one window wearing two palettes.
    let theme = inner.theme_now();
    for a in adoptions {
        let state = inner.cached_state(&a.pane_id);
        if let Err(e) = chrome::apply(
            &watcher.client(),
            inner.cfg.chrome,
            &a.pane_id,
            &a.window_id,
            &a.label,
            state,
            &theme,
        )
        .await
        {
            warn!(pane = %a.pane_id, error = %e, "cannot write pane chrome");
        }
    }
}

/// Paint one adopted pane's border with its current label and state.
///
/// [`paint_adoptions`] with a set of one. Silent when the pane is not
/// adopted, and when the session is detached: there is no client to write
/// through, and the re-attach repaints everything anyway.
pub(crate) async fn paint_chrome(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) {
    let Some(watcher) = inner.watcher_of(session_idx) else {
        return;
    };
    let Some(adoption) = inner
        .registry
        .lock()
        .expect("registry lock")
        .get(pane_id)
        .cloned()
    else {
        return;
    };
    paint_adoptions(inner, &watcher, std::slice::from_ref(&adoption)).await;
}

/// Repaint the state half of an adopted pane's border. Called from the one
/// place a fused state change is recorded (fusion::recompute_pane), so a
/// border can never disagree with the row `cyclops list` prints.
pub(crate) async fn repaint_chrome(inner: &Arc<Inner>, watcher: &SessionWatcher, pane_id: &str) {
    let Some(adoption) = inner
        .registry
        .lock()
        .expect("registry lock")
        .get(pane_id)
        .cloned()
    else {
        return;
    };
    let theme = inner.theme_now();
    let state = inner.cached_state(pane_id);
    if let Err(e) = chrome::repaint(
        &watcher.client(),
        inner.cfg.chrome,
        pane_id,
        &adoption.label,
        state,
        &theme,
    )
    .await
    {
        warn!(pane = %pane_id, error = %e, "cannot repaint pane chrome");
    }
}

/// Re-read the theme selection and put it on every border that is up.
///
/// `cyclops theme <name>` writes the config key and calls this. Without
/// the call the switch still lands, on the next fused state change, which
/// on a calm rig is not a time anyone can name; borders in last week's
/// theme next to a stream in this week's is the visible half of that.
///
/// Returns the name now active, for the CLI to print, and emits `theme`
/// so a running `cyclops ui` wakes and re-reads the selection itself.
/// The event carries no colors: every surface resolves its own, and one
/// that took a palette off the wire could show a theme no file holds.
pub(crate) async fn reload_theme(inner: &Arc<Inner>) -> String {
    // This call IS the reload: it re-stats the selection and drains
    // whatever the engine refused, so the warning is logged once rather
    // than once per pane. The paints below read the same watch, which by
    // then has nothing new to find.
    let name = inner.theme_now().name().to_string();
    for idx in 0..inner.session_count() {
        let Some(watcher) = inner.watcher_of(idx) else {
            continue;
        };
        let adopted = inner
            .registry
            .lock()
            .expect("registry lock")
            .in_session(&watcher.session());
        paint_adoptions(inner, &watcher, &adopted).await;
    }
    inner.emit("theme", json!({"name": name}), None);
    name
}

/// The `event: "boot"` system line every watched session's ledger gets:
/// which daemon run, which tmux, which manifest set. `boot` appends one of
/// these to every configured session; [`watch_session`] appends the same
/// line to a session that joins afterwards.
fn boot_fact_line(inner: &Inner, manifest_ids: &[String], session: &str) -> LedgerLine {
    daemon_line(
        Kind::System,
        inner.mint_event_id(),
        json!({
            "event": "boot",
            "tmux_version": inner.tmux_version,
            "manifests": manifest_ids,
            "session": session,
        }),
    )
}

/// Boot the daemon: probe tmux, load manifests, bind the socket, spawn one
/// watcher task per configured session plus the accept loop.
pub async fn boot(cfg: Config) -> anyhow::Result<Daemon> {
    let boot_id = uuid::Uuid::new_v4().to_string();
    let started = Instant::now();

    let tmux_version = match probe_tmux().await {
        Some(v) => {
            info!(
                tmux = %v.raw,
                bracket_paste_flag = v.has_bracket_paste_flag(),
                "tmux probed"
            );
            if !v.has_bracket_paste_flag() {
                // Amendment b: through 3.6a there is no way to see bracketed
                // paste degrade up front, so M1 deliveries gate on post-paste
                // composer verification instead.
                info!("bracket_paste_flag unavailable; deliveries will gate on post-paste composer verification (amendment b)");
            }
            v.raw
        }
        None => {
            warn!("tmux -V failed; session watchers will keep retrying");
            "unavailable".to_string()
        }
    };

    let (manifests, manifest_dir) = load_manifests(&cfg);

    // One crash-safe ledger per watched session. A daemon that cannot
    // record must not deliver, so open failures fail the boot.
    let ledger_dir = cfg.home.join("ledger");
    let mut sessions = Vec::with_capacity(cfg.sessions.len());
    let mut replayed: Vec<(usize, Vec<LedgerLine>)> = Vec::new();
    let engine = delivery::Engine::new();
    for (idx, name) in cfg.sessions.iter().enumerate() {
        let path = ledger_dir.join(format!("{name}.ndjson"));
        let ledger = LedgerWriter::open(&path, &boot_id)
            .map_err(|e| anyhow::anyhow!("open ledger {}: {e}", path.display()))?;
        // Message ids stay unique per ledger across restarts; the replayed
        // lines also feed the restart-limbo scan below.
        match cyclops_ledger::read_after(&path, 0) {
            Ok(lines) => {
                engine.preload_ids(&lines);
                replayed.push((idx, lines));
            }
            Err(e) => warn!(session = %name, error = %e, "ledger replay for id preload failed"),
        }
        sessions.push(Arc::new(SessionSlot::new(name.clone(), Arc::new(ledger))));
    }
    // Adoptions from the previous run. Nothing is trusted onto a pane
    // yet; each session prunes its own entries against the live pane
    // table when it attaches (registry::restore_session).
    let (mut adoptions, warnings) = registry::Registry::load(&cfg.home);
    for w in warnings {
        warn!("registry: {w}");
    }
    // Entries from sessions this run does NOT watch (watched at runtime by
    // a previous daemon, dropped by the restart) are re-verified here or
    // never: only an attach prunes, and only watched sessions attach. Ask
    // tmux once; a session that is gone took every pane in it along, and
    // an entry kept past this point still frees the moment its pane
    // proves dead (restore_session on a later session.watch).
    for session in adoptions.sessions() {
        if cfg.sessions.contains(&session) {
            continue;
        }
        if tmux_session_missing(&cfg, &session).await {
            release_gone_session(&mut adoptions, &session);
        }
    }
    let mut theme = cyclops_theme::ThemeWatch::new(&cfg.home);
    for w in theme.take_warnings() {
        warn!("theme: {w}");
    }

    // Created before Inner so the receiver can live on it: a session
    // watched after boot (watch_session) hands its session_task the same
    // receiver every configured session got. boot keeps the sender.
    let (stop, stop_rx) = watch::channel(false);
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let inner = Arc::new(Inner {
        cfg,
        boot_id,
        started,
        tmux_version,
        manifests,
        manifest_dir,
        sessions: StdMutex::new(sessions),
        events,
        detections: StdMutex::new(HashMap::new()),
        registry: StdMutex::new(adoptions),
        theme: StdMutex::new(theme),
        hook_readings: StdMutex::new(HashMap::new()),
        argv_cache: StdMutex::new(HashMap::new()),
        engine,
        ack_state: ack::AckState::new(),
        hook_liveness: selftest::HookLiveness::new(),
        inject_pause: StdMutex::new(None),
        fail_chrome_restore: AtomicBool::new(false),
        workspace_ui: StdMutex::new(workspace_ui::WorkspaceUiState::default()),
        stop: stop_rx,
        extra_tasks: StdMutex::new(Vec::new()),
    });

    // Boot fact on every session ledger: which daemon run, which tmux,
    // which manifest set. watch_session appends the same line to a
    // session that joins afterwards (boot_fact_line).
    let manifest_ids: Vec<String> = inner.manifests.keys().cloned().collect();
    for idx in 0..inner.session_count() {
        let name = inner.session(idx).expect("just counted it").name();
        inner.append_line(idx, boot_fact_line(&inner, &manifest_ids, &name));
    }

    // A daemon with no manifests boots clean, watches panes, and can
    // deliver nothing. The warn! below reaches whoever is tailing stderr,
    // which after `cyclopsd &` is nobody, so the same sentence also goes on
    // the record: it lands in `cyclops ui` and replays out of the ledger.
    // `cyclops status` reads the same fact off the status answer and
    // explains the unknown panes it produces.
    //
    // Fyi, not ActionRequired, and the level is load-bearing. A ping is a
    // POINTER at something in the attention register, and this names
    // nothing there: no pane is blocked and no delivery is open, because
    // nothing has been tried yet. An action-required ping naming no item is
    // admitted to the calm view forever (`cyclops_ui::App::admits`), which
    // would put "⚠ action required" under a closed eye on every frame until
    // the daemon is restarted. That contradiction is the thing M3 banned.
    if manifest_ids.is_empty() {
        let words = no_manifests_warning(inner.manifest_dir.as_deref());
        warn!("{words}");
        delivery::admin_notify(
            &inner,
            cyclops_proto::NotifyLevel::Fyi,
            "no detection manifests",
            &words,
            None,
            None,
            delivery::About::default(),
        );
    }

    // Restart-limbo closure: any delivery the previous run left in a
    // non-resolved state gets a named ending now (GOALS: limbo is a bug).
    delivery::close_limbo(&inner, &replayed);
    drop(replayed);

    let listener = server::bind_socket(&inner.cfg.home).await?;
    let mut tasks = Vec::new();
    for idx in 0..inner.session_count() {
        tasks.push(tokio::spawn(session_task(
            Arc::clone(&inner),
            idx,
            inner.stop.clone(),
        )));
    }
    tasks.push(tokio::spawn(server::accept_loop(
        Arc::clone(&inner),
        listener,
        inner.stop.clone(),
    )));
    info!(
        boot_id = %inner.boot_id,
        sessions = inner.session_count(),
        manifests = inner.manifests.len(),
        build = env!("CYCLOPS_BUILD_REF"),
        "cyclopsd booted"
    );
    Ok(Daemon {
        inner,
        stop,
        tasks: StdMutex::new(tasks),
    })
}

/// Start watching a tmux session the daemon was not booted with.
///
/// `sessions` in `config.toml` is what the daemon watches AT BOOT; this is
/// how a session created afterwards (the terminal workspace UI creates
/// tmux sessions at runtime) joins that set. It does not rewrite
/// `config.toml`: a restart watches the configured list again, not
/// whatever a client added here in between.
///
/// Idempotent: watching an already-watched session neither opens a second
/// ledger nor spawns a second task, it just hands back the existing index
/// with `added: false`. Otherwise the ledger is opened exactly the way
/// `boot` opens one (same [`LedgerWriter::open`] with the boot id, same
/// id-preload so message ids stay unique across the whole daemon, not just
/// within one session), and a failure to open it is a [`WireError`] rather
/// than a silent no-watch: a daemon that cannot record must not watch,
/// same rule `boot` follows when it fails the whole boot.
pub(crate) async fn watch_session(
    inner: &Arc<Inner>,
    name: &str,
) -> Result<(usize, bool), WireError> {
    if let Some(idx) = inner.session_index(name) {
        return Ok((idx, false));
    }
    // The watcher applies its own rename before the matching PaneEvent
    // reaches this daemon. A session.watch RPC can land in that ordered
    // channel's small hand-off window: the slot still has the old name,
    // but its live watcher already targets `name`. Fold the slot forward
    // here and dedup against it instead of opening a second ledger and
    // watcher for the same tmux session.
    if let Some(idx) = inner
        .session_slots()
        .iter()
        .enumerate()
        .find_map(|(idx, slot)| {
            let watcher = slot
                .link
                .lock()
                .expect("session link lock")
                .watcher
                .as_ref()
                .map(Arc::clone);
            watcher
                .is_some_and(|watcher| watcher.session() == name)
                .then_some(idx)
        })
    {
        rename_session_slot(inner, idx, name.to_string());
        return Ok((idx, false));
    }
    let path = inner.cfg.home.join("ledger").join(format!("{name}.ndjson"));
    let ledger = LedgerWriter::open(&path, &inner.boot_id).map_err(|e| WireError {
        code: "internal".to_string(),
        message: format!("open ledger {}: {e}", path.display()),
        data: None,
    })?;
    // Same id-preload boot does: message ids stay unique across restarts
    // and across every session this daemon has ever watched.
    match cyclops_ledger::read_after(&path, 0) {
        Ok(lines) => inner.engine.preload_ids(&lines),
        Err(e) => warn!(session = %name, error = %e, "ledger replay for id preload failed"),
    }
    let slot = Arc::new(SessionSlot::new(name.to_string(), Arc::new(ledger)));
    // Push, then drop the lock before doing anything else: the locking
    // rule this field's doc comment states is never taking another lock,
    // or awaiting, while holding the sessions lock.
    let idx = {
        let mut sessions = inner.sessions.lock().expect("sessions lock");
        sessions.push(slot);
        sessions.len() - 1
    };
    // The same boot-fact line `boot` appends to every configured session.
    let manifest_ids: Vec<String> = inner.manifests.keys().cloned().collect();
    inner.append_line(idx, boot_fact_line(inner, &manifest_ids, name));
    let handle = tokio::spawn(session_task(Arc::clone(inner), idx, inner.stop.clone()));
    inner
        .extra_tasks
        .lock()
        .expect("extra tasks lock")
        .push(handle);
    Ok((idx, true))
}

/// One `tmux -V`, run once at boot. None when tmux is absent or broken.
async fn probe_tmux() -> Option<TmuxVersion> {
    let out = tokio::process::Command::new("tmux")
        .arg("-V")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let v = TmuxVersion::parse(&text);
    if v.raw.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Load detection manifests, and report the directory they came from.
///
/// Failure is loud but not fatal: a daemon with zero manifests still
/// watches panes and answers status. It just cannot tell what is running in
/// any of them, so every pane reads unknown and every delivery to one ends
/// in attention_required. That is a broken install, not a quiet mode, and
/// [`boot`] says so on the record as well as in the log.
fn load_manifests(cfg: &Config) -> (BTreeMap<String, Manifest>, Option<PathBuf>) {
    let Some(dir) = cfg.manifest_dir() else {
        return (BTreeMap::new(), None);
    };
    match cyclops_manifest::load_dir(&dir) {
        Ok(map) => {
            info!(dir = %dir.display(), count = map.len(), "manifests loaded");
            (map.into_iter().collect(), Some(dir))
        }
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "manifest load failed; continuing with none");
            (BTreeMap::new(), Some(dir))
        }
    }
}

/// What to tell a person when the daemon booted with no manifests.
///
/// Same words in the log and in the notification, because they are the same
/// fact reaching two readers. The directory is named when there is one: an
/// empty directory and no directory at all are fixed differently.
pub(crate) fn no_manifests_warning(dir: Option<&Path>) -> String {
    let where_ = match dir {
        Some(d) => format!("{} holds no readable manifests", d.display()),
        None => "there is no manifest directory".to_string(),
    };
    format!(
        "{where_}, so every pane reads unknown and no message can be delivered. Install the shipped set with: cyclops start, then restart cyclopsd."
    )
}

/// Does this session not exist on the server the daemon is configured for?
///
/// Answers false on any tmux trouble, because the point is to soften one
/// specific log line and a probe that cannot tell should not be the reason
/// a real failure goes unreported. Runs off the reactor: it shells out to
/// tmux, which blocks.
async fn session_missing(inner: &Arc<Inner>, session: &str) -> bool {
    tmux_session_missing(&inner.cfg, session).await
}

/// The same question asked of the config alone, so `boot` can ask it
/// before `Inner` exists. True only when tmux positively says the session
/// is not there; an error keeps the answer at false, because "could not
/// ask" must never release anybody's label.
async fn tmux_session_missing(cfg: &Config, session: &str) -> bool {
    let server = cyclops_tmux::layout::Server {
        socket: cfg.tmux_socket.clone(),
        config_file: cfg.tmux_config.clone(),
    };
    let session = session.to_string();
    tokio::task::spawn_blocking(move || {
        matches!(
            cyclops_tmux::layout::session_exists(&server, &session),
            Ok(false)
        )
    })
    .await
    .unwrap_or(false)
}

/// Release every adoption recorded for a session tmux says is gone.
///
/// The panes died with the session, so each label is free again, and
/// there is no chrome to put back: the windows died too. `forget`, not
/// `clear`, so the entry goes even when the registry file cannot be
/// rewritten; a dead pane must not keep its name claimed.
fn release_gone_session(reg: &mut registry::Registry, session: &str) {
    for a in reg.in_session(session) {
        reg.forget(&a.pane_id);
        info!(session = %session, pane = %a.pane_id, label = %a.label, "released a label whose session is gone");
    }
}

/// Own one configured session for the daemon's lifetime: attach, pump
/// events, reattach with backoff when the connection dies or the session
/// does not exist yet.
async fn session_task(inner: Arc<Inner>, idx: usize, mut stop: watch::Receiver<bool>) {
    // One snapshot of the Arc for the task's whole life: idx is append-only
    // stable, so this never needs to re-consult the sessions lock.
    let slot = inner
        .session(idx)
        .expect("session_idx valid: append-only, never removed");
    let mut backoff = RECONNECT_MIN;
    let mut announced_missing = false;
    loop {
        if *stop.borrow() {
            return;
        }
        // Re-read on every attempt, not cached once outside the loop: a
        // rename followed while attached (`handle_pane_event`'s
        // `SessionRenamed` arm) updates this slot in place, and if the
        // connection later drops for real, the reattach below must target
        // the name tmux actually calls this session now, not the name this
        // task started with.
        let name = slot.name();
        // Payload spool files stay under the 0700 cyclops home, never the
        // shared system temp dir.
        let mut ccfg =
            ControlConfig::attach(&name).with_buffer_spool_dir(inner.cfg.home.join("spool"));
        if let Some(sock) = &inner.cfg.tmux_socket {
            ccfg = ccfg.on_socket(sock.clone());
        }
        if let Some(f) = &inner.cfg.tmux_config {
            ccfg = ccfg.with_config_file(f.clone());
        }
        match SessionWatcher::connect(ccfg).await {
            Ok(watcher) => {
                announced_missing = false;
                backoff = RECONNECT_MIN;
                let watcher = Arc::new(watcher);
                {
                    let mut link = slot.link.lock().expect("session link lock");
                    link.attached = true;
                    link.watcher = Some(Arc::clone(&watcher));
                }
                info!(session = %name, "attached to tmux session");
                session_lifecycle(&inner, idx, true);
                run_session(&inner, idx, &watcher, stop.clone()).await;
                // Freeze the pane table as of this detach: hook reports
                // arriving during the outage resolve against it.
                {
                    let mut last = slot.last_panes.lock().expect("last panes lock");
                    *last = watcher
                        .snapshot()
                        .into_iter()
                        .map(|r| (r.pane_id.clone(), r))
                        .collect();
                }
                {
                    let mut link = slot.link.lock().expect("session link lock");
                    link.attached = false;
                    link.watcher = None;
                }
                session_lifecycle(&inner, idx, false);
                // Detached panes have no live sensors; drop their cached
                // verdicts instead of serving stale state.
                {
                    let mut det = inner.detections.lock().expect("detections lock");
                    for row in watcher.snapshot() {
                        det.remove(&row.pane_id);
                    }
                }
                watcher.shutdown().await;
                if *stop.borrow() {
                    return;
                }
                // Re-read: a rename during this attach already moved the
                // slot on, and the reattach log line should say the name
                // that is about to be dialed, not the one this attempt
                // started with.
                warn!(session = %slot.name(), "tmux connection lost; reattaching");
            }
            Err(e) => {
                if announced_missing {
                    debug!(session = %name, error = %e, "attach retry failed");
                }
                // Adoptions recorded for a session that does not exist
                // name panes that died with it. The reattach reconcile
                // can never release them, because a session that never
                // comes back never reattaches; without this, its labels
                // stay claimed forever while no roster shows the holder.
                let stale = !inner
                    .registry
                    .lock()
                    .expect("registry lock")
                    .in_session(&name)
                    .is_empty();
                if stale || !announced_missing {
                    let missing = session_missing(&inner, &name).await;
                    if stale && missing {
                        let mut reg = inner.registry.lock().expect("registry lock");
                        release_gone_session(&mut reg, &name);
                    }
                    if !announced_missing {
                        // Two different situations reach this arm, and only
                        // one of them is trouble. A session that does not
                        // exist yet is the ordinary case for a daemon started
                        // before `cyclops start`, and it clears itself the
                        // moment the session appears. Logging that at WARN as
                        // "cannot attach" reads like a dead end, and an
                        // operator who stops there never finds out that the
                        // retry two seconds later succeeded.
                        if missing {
                            info!(session = %name, "waiting for session; cyclops start creates it");
                        } else {
                            warn!(session = %name, error = %e, "cannot attach; retrying with backoff");
                        }
                        announced_missing = true;
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// Record and broadcast a session attach/detach: a system ledger line
/// plus a "session" event (gate holds wake on it after a reattach).
fn session_lifecycle(inner: &Arc<Inner>, idx: usize, attached: bool) {
    let name = inner
        .session(idx)
        .expect("session_idx valid: append-only, never removed")
        .name();
    let seq = inner.append_line(
        idx,
        daemon_line(
            Kind::System,
            inner.mint_event_id(),
            json!({
                "event": if attached { "attach" } else { "detach" },
                "session": name,
            }),
        ),
    );
    inner.emit("session", json!({"name": name, "attached": attached}), seq);
}

/// Apply a followed rename to this daemon's own slot: `SessionSlot::rename`
/// so `session_index(new_name)` starts hitting it, which is what a later
/// `session.watch` for the new name needs to dedup instead of opening a
/// second slot and watcher for the one tmux session. Records the moment
/// with one `kind=system` ledger line on the ledger this slot already has
/// open — no new file, same handle, see `SessionSlot::ledger`'s doc
/// comment — so the record explains, the next time anyone reads it, why
/// the rest of this file's lines describe a session under a different
/// name than the file itself.
///
/// Idempotent: a no-op when the slot already carries `new_name`. Both
/// callers rely on that — the ordered `SessionRenamed` event, and
/// `run_session`'s lagged-receiver recovery path, which cannot tell
/// whether a `SessionRenamed` it missed was already applied by the time it
/// notices the drift.
///
/// `config.toml`'s `sessions` list is deliberately untouched: this mirrors
/// [`watch_session`], which also never rewrites it (a session watched at
/// runtime is not durable across a restart by design, and neither is a
/// rename of one — a restart re-reads `sessions` and watches the OLD name
/// again, same as it always has).
fn rename_session_slot(inner: &Arc<Inner>, idx: usize, new_name: String) {
    let Some(slot) = inner.session(idx) else {
        return;
    };
    let Some(old_name) = slot.rename(new_name.clone()) else {
        return;
    };
    // Adoptions and their chrome snapshots are session-scoped even though
    // their tmux pane/window ids do not change on a rename. Move those
    // durable facts with the slot or every in_session(new_name) path
    // (theme repaint, reattach, shutdown restore) loses the named panes.
    if let Err(e) = inner
        .registry
        .lock()
        .expect("registry lock")
        .rename_session(&old_name, &new_name)
    {
        error!(old_name = %old_name, new_name = %new_name, error = %e, "cannot persist session rename in adoption registry");
    }
    info!(old_name = %old_name, new_name = %new_name, "session renamed; daemon slot now follows tmux");
    inner.append_line(
        idx,
        daemon_line(
            Kind::System,
            inner.mint_event_id(),
            json!({
                "event": "renamed",
                "old_name": old_name,
                "new_name": new_name,
            }),
        ),
    );
}

/// Pump one attached watcher until it disconnects or the daemon stops.
async fn run_session(
    inner: &Arc<Inner>,
    idx: usize,
    watcher: &Arc<SessionWatcher>,
    mut stop: watch::Receiver<bool>,
) {
    let mut rx = watcher.subscribe();
    // A rename can land after SessionWatcher::connect returns but before
    // this receiver exists. Broadcast channels do not replay that event;
    // synchronize from the watcher's live name once after subscribing so
    // the daemon slot and registry cannot remain on the connect-time name
    // forever. If the event lands after subscribe, the ordinary match arm
    // below is idempotent with this check.
    let live_name = watcher.session();
    if inner.session(idx).is_some_and(|s| s.name() != live_name) {
        rename_session_slot(inner, idx, live_name);
    }
    // Bootstrap: the watcher's table is already authoritative; evaluate
    // every pane once so status answers immediately. Adoptions are
    // reconciled against that table first, so the very first recompute
    // already knows which panes are named and which manifest is pinned.
    reconcile_adoptions(inner, watcher).await;
    for row in watcher.snapshot() {
        fusion::recompute_pane(inner, idx, watcher, &row.pane_id, false, "bootstrap").await;
    }
    // Per-pane debounce kickers for output activity.
    let mut debounce: HashMap<String, mpsc::Sender<()>> = HashMap::new();
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if handle_pane_event(inner, idx, watcher, &mut debounce, ev).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    // Missed hints degrade freshness, never correctness:
                    // reconcile and re-evaluate everything (level-triggered
                    // core, ADR revision 1). A rename notification could be
                    // among what was missed; a lagged receiver has no way to
                    // replay it, so bring the slot's name back in step with
                    // the watcher's own (already-live) idea of it before
                    // reconciling panes, or a `SessionRenamed` this receiver
                    // never saw leaves the slot answering to a name tmux
                    // dropped.
                    let live_name = watcher.session();
                    if inner
                        .session(idx)
                        .is_some_and(|s| s.name() != live_name)
                    {
                        rename_session_slot(inner, idx, live_name);
                    }
                    warn!(session = %watcher.session(), missed, "event stream lagged; reconciling");
                    if watcher.reconcile_now().await.is_err() {
                        return;
                    }
                    for row in watcher.snapshot() {
                        fusion::recompute_pane(inner, idx, watcher, &row.pane_id, false, "lag_reconcile").await;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

/// Bring the registry back in step with a session that just attached, and
/// repaint what survives.
///
/// This runs on every attach, not only at boot: a reattach after tmux went
/// away can find a completely different set of panes, and a label pointing
/// at a pane id that now belongs to somebody else is how a message reaches
/// the wrong terminal.
async fn reconcile_adoptions(inner: &Arc<Inner>, watcher: &Arc<SessionWatcher>) {
    let live: Vec<(String, i32)> = watcher
        .snapshot()
        .into_iter()
        .map(|r| (r.pane_id, r.pane_pid))
        .collect();
    let kept = {
        let mut reg = inner.registry.lock().expect("registry lock");
        match reg.restore_session(&watcher.session(), &live) {
            Ok(kept) => kept,
            Err(e) => {
                error!(session = %watcher.session(), error = %e, "cannot rewrite the registry; keeping it in memory only");
                reg.in_session(&watcher.session())
            }
        }
    };
    paint_adoptions(inner, watcher, &kept).await;
}

/// An adopted pane moved to another window (tmux `join-pane`,
/// `break-pane`). Take the border text off the window it left and put it
/// on the window it joined.
///
/// Border text is a window setting with no pane scope (F27), so it does
/// not travel with the pane. Without this, the source window keeps showing
/// border text with nothing named left in it, and the destination shows
/// none at all. The pane's own options need no work: they moved with it.
///
/// The name and everything under it are untouched. This is chrome only.
///
/// Takes `session_idx` from the caller rather than resolving it from
/// `watcher.session()` here, for the same reason `emit_state` does: this
/// runs mid-processing of one `PaneEvent` off the watcher's ordered
/// channel, and that live name can already be ahead of a `SessionRenamed`
/// still queued behind the event this call is handling.
async fn move_chrome(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    pane_id: &str,
) {
    let Some(row) = watcher.pane(pane_id) else {
        return;
    };
    // Nothing to do for a pane nobody named, or one already recorded in
    // the window it is now in.
    match inner.registry.lock().expect("registry lock").get(pane_id) {
        Some(a) if a.window_id != row.window_id => {}
        _ => return,
    }
    // Read the destination's border setting before anything writes to it,
    // and only when this is a window the registry has not already
    // snapshotted; a window already holding an adopted pane has one.
    let destination_known = inner
        .registry
        .lock()
        .expect("registry lock")
        .window(&row.window_id)
        .is_some();
    let destination_status = if destination_known {
        None
    } else {
        match chrome::snapshot(&watcher.client(), pane_id, &row.window_id).await {
            Ok(s) => s.border_status,
            Err(e) => {
                warn!(pane = %pane_id, error = %e, "cannot read the destination window's border setting");
                None
            }
        }
    };
    let freed = match inner.registry.lock().expect("registry lock").move_window(
        pane_id,
        &row.window_id,
        destination_status,
    ) {
        Ok(freed) => freed,
        Err(e) => {
            error!(pane = %pane_id, error = %e, "cannot record the pane move");
            return;
        }
    };
    if let Some(source) = freed {
        if let Err(e) = chrome::restore_window(
            &watcher.client(),
            inner.cfg.chrome,
            &source.window_id,
            source.border_status.as_deref(),
        )
        .await
        {
            warn!(window = %source.window_id, error = %e, "cannot restore the window an adopted pane left");
        }
    }
    paint_chrome(inner, session_idx, pane_id).await;
}

/// Apply one watcher event. Returns true when the connection is over.
async fn handle_pane_event(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    debounce: &mut HashMap<String, mpsc::Sender<()>>,
    ev: PaneEvent,
) -> bool {
    match ev {
        PaneEvent::PaneAdded(row) => {
            fusion::recompute_pane(
                inner,
                session_idx,
                watcher,
                &row.pane_id,
                false,
                "pane_added",
            )
            .await;
            false
        }
        PaneEvent::PaneRemoved(id) => {
            debounce.remove(&id);
            inner
                .detections
                .lock()
                .expect("detections lock")
                .remove(&id);
            // Adoption ends with the pane; hook history and the argv
            // binding cache die with it too. There is no chrome to put
            // back: the pane that wore it is gone, and its window only
            // keeps the border on while some other adopted pane is left.
            let freed = inner
                .registry
                .lock()
                .expect("registry lock")
                .forget(&id)
                .and_then(|(a, freed)| freed.map(|f| (a, f)));
            if let Some((adoption, window)) = freed {
                if let Err(e) = chrome::restore_window(
                    &watcher.client(),
                    inner.cfg.chrome,
                    &adoption.window_id,
                    window.border_status.as_deref(),
                )
                .await
                {
                    warn!(window = %adoption.window_id, error = %e, "cannot restore window border after the last adopted pane closed");
                }
            }
            inner
                .hook_readings
                .lock()
                .expect("hook readings lock")
                .remove(&id);
            inner
                .argv_cache
                .lock()
                .expect("argv cache lock")
                .retain(|(pane, _), _| pane != &id);
            inner.hook_liveness.forget(&id);
            // The pane's last transition, and the only one a subscriber
            // can hear: every other event names a pane that still exists.
            // A client counting what needs a human takes its roster from
            // one `status` answer at startup and moves it on events after
            // that (cyclops_proto::attention), so without this edge a pane
            // that blocked and then closed stays counted for the life of
            // the client. Additive: an older client renders it and moves
            // on, and a client watching an older daemon never hears it.
            // The slot this pane belonged to, addressed by idx rather than
            // by re-deriving it from `watcher.session()`: same rationale as
            // `emit_state`, this is diagnostic payload for one event off
            // the ordered channel and must not show a name a queued
            // `SessionRenamed` has not reached yet.
            let session = inner
                .session(session_idx)
                .expect("session_idx valid: append-only, never removed")
                .name();
            inner.emit(
                "pane-removed",
                json!({
                    "ts": unix_ms(),
                    "session": session,
                    "pane_id": id,
                }),
                None,
            );
            false
        }
        PaneEvent::PaneChanged { id, changed, .. } => {
            // Size and focus changes do not move agent state; title, death,
            // mode, and foreground command do.
            let relevant = changed.iter().any(|f| {
                matches!(
                    f,
                    PaneField::Title
                        | PaneField::Dead
                        | PaneField::InMode
                        | PaneField::CurrentCommand
                )
            });
            if relevant {
                fusion::recompute_pane(inner, session_idx, watcher, &id, false, "pane_changed")
                    .await;
            }
            // A move does not touch agent state, but it does move half the
            // chrome: the pane carries its own options and the window's
            // border text does not follow it.
            if changed.iter().any(|f| matches!(f, PaneField::WindowId)) {
                move_chrome(inner, session_idx, watcher, &id).await;
            }
            false
        }
        PaneEvent::OutputActivity { pane_id, .. } => {
            kick_debounce(inner, session_idx, watcher, debounce, pane_id);
            false
        }
        PaneEvent::Paused { pane_id } => {
            debug!(pane = %pane_id, "flow control paused pane");
            false
        }
        PaneEvent::Resumed { pane_id } => {
            debug!(pane = %pane_id, "flow control resumed pane");
            false
        }
        // The watcher already followed the rename (its own internal target
        // and `SessionWatcher::session()` reflect `name` before this event
        // was even sent, see `cyclops_tmux::handle_session_renamed`); this
        // is the daemon's turn. Applying it here, on the same ordered
        // channel every other event in this match travels, is what
        // guarantees the slot is renamed before any event the watcher
        // emits AFTER the rename gets processed — see `emit_state`'s doc
        // comment for what breaks if that ordering did not hold.
        PaneEvent::SessionRenamed { name } => {
            rename_session_slot(inner, session_idx, name);
            false
        }
        PaneEvent::Disconnected => true,
    }
}

/// Feed a pane's debounce task, spawning it on first activity. A full
/// channel means a recompute is already pending; nothing to do.
fn kick_debounce(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    debounce: &mut HashMap<String, mpsc::Sender<()>>,
    pane_id: String,
) {
    if let Some(tx) = debounce.get(&pane_id) {
        match tx.try_send(()) {
            Ok(()) => return,
            Err(mpsc::error::TrySendError::Full(())) => return,
            // Task ended (should not happen while the sender lives, but a
            // panic inside it would); fall through and respawn.
            Err(mpsc::error::TrySendError::Closed(())) => {}
        }
    }
    let (tx, rx) = mpsc::channel(8);
    let _ = tx.try_send(());
    debounce.insert(pane_id.clone(), tx);
    tokio::spawn(debounce_task(
        rx,
        Arc::clone(inner),
        session_idx,
        Arc::clone(watcher),
        pane_id,
    ));
}

/// Output settle debounce: a reset timer, not an interval. The sleep only
/// exists between the first kick and the settle; each further kick pushes
/// the deadline out. With no output, this task is parked in recv.
///
/// `session_idx` is captured once at spawn, not re-derived from
/// `watcher.session()` on each fire: this task runs off its own timer,
/// entirely outside the watcher's ordered event channel, so a rename that
/// races it would hit the exact `session_index` miss `emit_state`'s doc
/// comment describes. The captured idx cannot go stale mid-task — one
/// debounce task lives only as long as one attach, and `session_idx` is
/// append-only-stable for the daemon's whole life regardless of how many
/// times the slot it names gets renamed.
async fn debounce_task(
    mut rx: mpsc::Receiver<()>,
    inner: Arc<Inner>,
    session_idx: usize,
    watcher: Arc<SessionWatcher>,
    pane_id: String,
) {
    while rx.recv().await.is_some() {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(OUTPUT_SETTLE) => break,
                more = rx.recv() => {
                    if more.is_none() {
                        return;
                    }
                    // Another burst of output: restart the settle window.
                }
            }
        }
        fusion::recompute_pane(
            &inner,
            session_idx,
            &watcher,
            &pane_id,
            false,
            "output_settled",
        )
        .await;
    }
}

/// Unix time in milliseconds.
pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `Inner` with no sessions, no manifests, no tmux — enough to
    /// exercise slot bookkeeping without a live daemon or a tmux server.
    /// Mirrors `server::tests::bare_inner`, kept separate because that one
    /// is private to its own module.
    fn bare_inner(tag: &str) -> Arc<Inner> {
        let home = cyclops_proto::scratch::scratch_dir(tag);
        let (registry, _) = registry::Registry::load(&home);
        Arc::new(Inner {
            cfg: Config::defaults(&home),
            boot_id: "b-test".into(),
            started: Instant::now(),
            tmux_version: "3.6a".into(),
            manifests: BTreeMap::new(),
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            events: broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::new()),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
            argv_cache: StdMutex::new(HashMap::new()),
            engine: delivery::Engine::new(),
            ack_state: ack::AckState::new(),
            hook_liveness: selftest::HookLiveness::new(),
            inject_pause: StdMutex::new(None),
            fail_chrome_restore: AtomicBool::new(false),
            workspace_ui: StdMutex::new(workspace_ui::WorkspaceUiState::default()),
            stop: watch::channel(false).1,
            extra_tasks: StdMutex::new(Vec::new()),
        })
    }

    /// A rename that lands on the daemon's own slot (`rename_session_slot`,
    /// the same call `handle_pane_event`'s `SessionRenamed` arm makes) must
    /// make `session_index` resolve the NEW name to the SAME slot — no
    /// tmux and no watcher needed to prove that bookkeeping. That is what
    /// lets a later `session.watch` for the new name dedup instead of
    /// opening a second slot and watcher for the one tmux session, which is
    /// the duplicate-watcher bug this feature exists to prevent.
    #[tokio::test]
    async fn a_renamed_slot_is_found_under_its_new_name_and_watch_session_dedups() {
        let inner = bare_inner("cyc-rename-unit");
        let dir = cyclops_proto::scratch::scratch_dir("cyc-rename-unit-ledger");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ledger")).expect("scratch ledger dir");
        let ledger_path = dir.join("ledger/old-name.ndjson");
        let ledger =
            cyclops_ledger::LedgerWriter::open(&ledger_path, &inner.boot_id).expect("ledger opens");
        let idx = {
            let mut sessions = inner.sessions.lock().expect("sessions lock");
            sessions.push(Arc::new(SessionSlot::new(
                "old-name".to_string(),
                Arc::new(ledger),
            )));
            sessions.len() - 1
        };

        assert_eq!(inner.session_index("old-name"), Some(idx));
        assert_eq!(inner.session_index("new-name"), None);

        rename_session_slot(&inner, idx, "new-name".to_string());

        assert_eq!(
            inner.session_index("old-name"),
            None,
            "the old name must no longer resolve"
        );
        assert_eq!(
            inner.session_index("new-name"),
            Some(idx),
            "the new name must hit the SAME slot"
        );

        // The dedup a later session.watch RPC depends on: watch_session
        // must return the existing slot, not open a second one.
        let (watched_idx, added) = watch_session(&inner, "new-name")
            .await
            .expect("watch_session on an already-watched name");
        assert_eq!(watched_idx, idx);
        assert!(
            !added,
            "watch_session must not spawn a second watcher for the renamed session"
        );

        // The ledger append the rename itself makes landed on the file the
        // slot already had open, still named after the OLD name on disk
        // (SessionSlot::ledger's doc comment) — never a new file for the
        // new name.
        assert!(ledger_path.exists());
        assert!(!dir.join("ledger/new-name.ndjson").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The watcher updates its own name before the daemon consumes the
    /// matching PaneEvent. Put a live watcher in exactly that hand-off
    /// window and prove session.watch folds the existing slot forward
    /// instead of creating a duplicate.
    #[tokio::test]
    async fn watch_session_dedups_when_the_live_watcher_is_ahead_of_its_slot() {
        if !cyclops_testrig::tmux_available() {
            eprintln!("skipping: tmux not on PATH");
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("rename-watch-race");
        server.run_ok(&["new-session", "-d", "-s", "old-name", "/bin/sh"]);
        let watcher = Arc::new(
            SessionWatcher::connect(
                ControlConfig::attach("old-name")
                    .on_socket(server.socket().to_string())
                    .with_config_file("/dev/null"),
            )
            .await
            .expect("watcher connects"),
        );
        let mut events = watcher.subscribe();

        let inner = bare_inner("cyc-rename-watch-race");
        let home = inner.cfg.home.clone();
        std::fs::create_dir_all(home.join("ledger")).expect("scratch ledger dir");
        let ledger = cyclops_ledger::LedgerWriter::open(
            &home.join("ledger/old-name.ndjson"),
            &inner.boot_id,
        )
        .expect("ledger opens");
        let slot = Arc::new(SessionSlot::new("old-name".to_string(), Arc::new(ledger)));
        {
            let mut link = slot.link.lock().expect("session link lock");
            link.attached = true;
            link.watcher = Some(Arc::clone(&watcher));
        }
        inner
            .sessions
            .lock()
            .expect("sessions lock")
            .push(Arc::clone(&slot));

        server.run_ok(&["rename-session", "-t", "=old-name", "new-name"]);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    events.recv().await,
                    Ok(PaneEvent::SessionRenamed { ref name }) if name == "new-name"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("watcher reports rename");
        assert_eq!(
            slot.name(),
            "old-name",
            "the test must stop before the daemon applies the event"
        );

        let (idx, added) = watch_session(&inner, "new-name")
            .await
            .expect("watch_session dedups");
        assert_eq!(idx, 0);
        assert!(!added);
        assert_eq!(inner.session_count(), 1);
        assert_eq!(slot.name(), "new-name");

        watcher.shutdown().await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// Renaming to the name the slot already carries (the lagged-receiver
    /// recovery path in `run_session` calls this speculatively, since it
    /// cannot tell whether a `SessionRenamed` it missed was already
    /// applied) must not append a spurious ledger line.
    #[tokio::test]
    async fn renaming_a_slot_to_its_own_name_is_a_no_op() {
        let inner = bare_inner("cyc-rename-noop-unit");
        let dir = cyclops_proto::scratch::scratch_dir("cyc-rename-noop-unit-ledger");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ledger")).expect("scratch ledger dir");
        let ledger_path = dir.join("ledger/same-name.ndjson");
        let ledger =
            cyclops_ledger::LedgerWriter::open(&ledger_path, &inner.boot_id).expect("ledger opens");
        let idx = {
            let mut sessions = inner.sessions.lock().expect("sessions lock");
            sessions.push(Arc::new(SessionSlot::new(
                "same-name".to_string(),
                Arc::new(ledger),
            )));
            sessions.len() - 1
        };

        let before = cyclops_ledger::read_after(&ledger_path, 0).expect("ledger reads");
        rename_session_slot(&inner, idx, "same-name".to_string());
        let after = cyclops_ledger::read_after(&ledger_path, 0).expect("ledger reads");
        assert_eq!(
            before.len(),
            after.len(),
            "a same-name rename appends nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
