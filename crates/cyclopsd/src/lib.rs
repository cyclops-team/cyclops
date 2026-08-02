//! cyclopsd: the Cyclops daemon.
//!
//! M0 landed the read-only shadow scope: watch configured tmux sessions
//! over control mode, fuse manifest rules over pane titles and screens
//! into an AgentState per pane, and serve the NDJSON socket API. M1 adds
//! the ledger (one crash-safe NDJSON file per watched session), the
//! delivery pipeline (msg.send end to end, docs/DELIVERY.md), the hook
//! sensor via agent.state.report, admin.notify, and pane labeling.
//!
//! Zero-polling contract: nothing here re-queries state on a clock. The
//! only timers are the per-pane output settle debounce, the watcher
//! reconnect backoff, and the per-delivery one-shots inside the delivery
//! pipeline (verify re-reads, ACK windows, decline spacing). No interval
//! ever re-queries state.
//!
//! The crate is a library so integration tests boot the daemon in-process;
//! main.rs is a thin wrapper adding signals and logging.

mod ack;
pub mod config;
mod delivery;
mod fusion;
pub mod identity;
mod server;

pub use config::Config;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cyclops_ledger::LedgerWriter;
use cyclops_manifest::Manifest;
use cyclops_proto::{
    AdminNotifyParams, AgentState, Detection, Event, Kind, LedgerLine, MsgSendParams,
    SensorReading, StateReportParams, WireError,
};
use cyclops_tmux::{ControlConfig, PaneEvent, PaneField, SessionWatcher, TmuxVersion};
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
    /// One slot per configured session, fixed at boot.
    pub(crate) sessions: Vec<SessionSlot>,
    /// Push stream for events.subscribe connections.
    pub(crate) events: broadcast::Sender<Event>,
    /// Cached fusion verdict per pane id.
    pub(crate) detections: StdMutex<HashMap<String, DetEntry>>,
    /// Adoption registry: pane id -> cyclops label. Explicit labeling via
    /// pane.label (v1 keeper: explicit pane adoption).
    pub(crate) labels: StdMutex<HashMap<String, String>>,
    /// Latest hook sensor reading per pane id (agent.state.report).
    pub(crate) hook_readings: StdMutex<HashMap<String, SensorReading>>,
    /// Delivery pipeline state.
    pub(crate) engine: delivery::Engine,
    /// Hook report dedupe state.
    pub(crate) ack_state: ack::AckState,
}

pub(crate) struct SessionSlot {
    pub(crate) name: String,
    pub(crate) link: StdMutex<SessionLink>,
    /// Append-only session ledger at $CYCLOPS_HOME/ledger/<session>.ndjson.
    pub(crate) ledger: Arc<LedgerWriter>,
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
}

impl Inner {
    /// Emit a fused-state change: one kind=state ledger line on the
    /// session's ledger plus a "state" event. Gate/state lines carry rule
    /// ids and causes, never raw screen captures (secrets rule).
    pub(crate) fn emit_state(
        &self,
        session: &str,
        pane_id: &str,
        det: &Detection,
        prior: Option<AgentState>,
        cause: &str,
    ) {
        let target = self
            .label_of(pane_id)
            .unwrap_or_else(|| pane_id.to_string());
        let session_idx = self.sessions.iter().position(|s| s.name == session);
        let seq = session_idx.and_then(|idx| {
            self.append_line(
                idx,
                LedgerLine {
                    seq: 0,
                    boot_id: String::new(),
                    id: self.mint_event_id(),
                    ts: 0,
                    kind: Kind::State,
                    from: "cyclopsd".to_string(),
                    to: Vec::new(),
                    subject: None,
                    body: None,
                    reply_to: None,
                    deliveries: Vec::new(),
                    data: Some(json!({
                        "pane_id": pane_id,
                        "target": target,
                        "state": det.state,
                        "prior": prior,
                        "disagreement": det.disagreement,
                        "decided_by": det.decided_by,
                        "cause": cause,
                    })),
                },
            )
        });
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
        let slot = self.sessions.get(session_idx)?;
        match slot.ledger.append(line) {
            Ok(l) => Some(l.seq),
            Err(e) => {
                error!(session = %slot.name, error = %e, "ledger append failed");
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
        self.labels
            .lock()
            .expect("labels lock")
            .get(pane_id)
            .cloned()
    }

    /// Live watcher for a session slot, if attached.
    pub(crate) fn watcher_of(&self, session_idx: usize) -> Option<Arc<SessionWatcher>> {
        let slot = self.sessions.get(session_idx)?;
        let link = slot.link.lock().expect("session link lock");
        link.watcher.as_ref().map(Arc::clone)
    }

    /// Resolve a recipient/target name: label first, then pane id. Only
    /// panes that currently exist resolve.
    pub(crate) fn resolve_recipient(&self, name: &str) -> Option<(usize, String)> {
        let by_label: Option<String> = {
            let labels = self.labels.lock().expect("labels lock");
            labels
                .iter()
                .find(|(_, l)| l.as_str() == name)
                .map(|(pane, _)| pane.clone())
        };
        let wanted = by_label.as_deref().unwrap_or(name);
        for (idx, slot) in self.sessions.iter().enumerate() {
            let watcher = {
                let link = slot.link.lock().expect("session link lock");
                link.watcher.as_ref().map(Arc::clone)
            };
            if let Some(w) = watcher {
                if let Some(row) = w.pane(wanted) {
                    return Some((idx, row.pane_id));
                }
            }
        }
        None
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

    /// Clean shutdown: signal every task, let session tasks detach their
    /// control clients, then remove the socket file. Delivery workers are
    /// aborted; queued deliveries stay recorded in the ledger.
    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
        let tasks: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.tasks.lock().expect("tasks lock"));
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

    /// In-process msg.send with an already-resolved sender. The socket
    /// path resolves the sender from peer credentials first; embedders and
    /// tests supply it directly.
    pub async fn msg_send(&self, from: &str, params: MsgSendParams) -> Result<Value, WireError> {
        delivery::msg_send(&self.inner, from, params).await
    }

    /// In-process agent.state.report (the hook receiver posts this).
    pub async fn report_state(&self, params: StateReportParams) -> Result<Value, WireError> {
        ack::handle_report(&self.inner, params).await
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
        );
        Ok(json!({"notified": true, "seq": seq}))
    }

    /// Label (adopt) or unlabel a pane. `target` is a pane id or an
    /// existing label; `label: None` clears.
    pub async fn label_pane(
        &self,
        target: &str,
        label: Option<String>,
    ) -> Result<Value, WireError> {
        label_pane(&self.inner, target, label)
    }
}

/// Set or clear a pane label. Labels are the adoption registry: they name
/// senders, resolve recipients, and define the "*" broadcast domain.
pub(crate) fn label_pane(
    inner: &Arc<Inner>,
    target: &str,
    label: Option<String>,
) -> Result<Value, WireError> {
    let Some((session_idx, pane_id)) = inner.resolve_recipient(target) else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("no such target {target:?}"),
        });
    };
    let label = label.filter(|l| !l.is_empty());
    if let Some(l) = &label {
        if l == "*" || l == "admin" || l.starts_with('%') {
            return Err(WireError {
                code: "bad_request".to_string(),
                message: format!("label {l:?} is reserved"),
            });
        }
        let labels = inner.labels.lock().expect("labels lock");
        if labels
            .iter()
            .any(|(p, existing)| existing == l && *p != pane_id)
        {
            return Err(WireError {
                code: "bad_request".to_string(),
                message: format!("label {l:?} is already taken"),
            });
        }
    }
    {
        let mut labels = inner.labels.lock().expect("labels lock");
        match &label {
            Some(l) => {
                labels.insert(pane_id.clone(), l.clone());
            }
            None => {
                labels.remove(&pane_id);
            }
        }
    }
    let seq = inner.append_line(
        session_idx,
        LedgerLine {
            seq: 0,
            boot_id: String::new(),
            id: inner.mint_event_id(),
            ts: 0,
            kind: Kind::System,
            from: "cyclopsd".to_string(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(json!({"event": "pane_labeled", "pane_id": pane_id, "label": label})),
        },
    );
    inner.emit(
        "session",
        json!({"name": inner.sessions[session_idx].name, "pane_labeled": pane_id, "label": label}),
        seq,
    );
    Ok(json!({"target": target, "pane_id": pane_id, "label": label}))
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

    let manifests = load_manifests(&cfg);

    // One crash-safe ledger per watched session. A daemon that cannot
    // record must not deliver, so open failures fail the boot.
    let ledger_dir = cfg.home.join("ledger");
    let mut sessions = Vec::with_capacity(cfg.sessions.len());
    let engine = delivery::Engine::new();
    for name in &cfg.sessions {
        let path = ledger_dir.join(format!("{name}.ndjson"));
        let ledger = LedgerWriter::open(&path, &boot_id)
            .map_err(|e| anyhow::anyhow!("open ledger {}: {e}", path.display()))?;
        // Message ids stay unique per ledger across restarts.
        match cyclops_ledger::read_after(&path, 0) {
            Ok(lines) => engine.preload_ids(&lines),
            Err(e) => warn!(session = %name, error = %e, "ledger replay for id preload failed"),
        }
        sessions.push(SessionSlot {
            name: name.clone(),
            link: StdMutex::new(SessionLink::default()),
            ledger: Arc::new(ledger),
        });
    }
    let (events, _) = broadcast::channel(1024);
    let inner = Arc::new(Inner {
        cfg,
        boot_id,
        started,
        tmux_version,
        manifests,
        sessions,
        events,
        detections: StdMutex::new(HashMap::new()),
        labels: StdMutex::new(HashMap::new()),
        hook_readings: StdMutex::new(HashMap::new()),
        engine,
        ack_state: ack::AckState::new(),
    });

    // Boot fact on every session ledger: which daemon run, which tmux,
    // which manifest set.
    let manifest_ids: Vec<String> = inner.manifests.keys().cloned().collect();
    for idx in 0..inner.sessions.len() {
        inner.append_line(
            idx,
            LedgerLine {
                seq: 0,
                boot_id: String::new(),
                id: inner.mint_event_id(),
                ts: 0,
                kind: Kind::System,
                from: "cyclopsd".to_string(),
                to: Vec::new(),
                subject: None,
                body: None,
                reply_to: None,
                deliveries: Vec::new(),
                data: Some(json!({
                    "event": "boot",
                    "tmux_version": inner.tmux_version,
                    "manifests": manifest_ids,
                    "session": inner.sessions[idx].name,
                })),
            },
        );
    }

    let listener = server::bind_socket(&inner.cfg.home).await?;
    let (stop, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    for idx in 0..inner.sessions.len() {
        tasks.push(tokio::spawn(session_task(
            Arc::clone(&inner),
            idx,
            stop_rx.clone(),
        )));
    }
    tasks.push(tokio::spawn(server::accept_loop(
        Arc::clone(&inner),
        listener,
        stop_rx,
    )));
    info!(
        boot_id = %inner.boot_id,
        sessions = inner.sessions.len(),
        manifests = inner.manifests.len(),
        "cyclopsd booted"
    );
    Ok(Daemon {
        inner,
        stop,
        tasks: StdMutex::new(tasks),
    })
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

/// Load detection manifests. Failure is loud but not fatal: a shadow
/// daemon with zero manifests still watches panes and answers status,
/// every pane just reads unknown.
fn load_manifests(cfg: &Config) -> BTreeMap<String, Manifest> {
    let Some(dir) = cfg.manifest_dir() else {
        warn!("no manifest directory found; every pane will read unknown");
        return BTreeMap::new();
    };
    match cyclops_manifest::load_dir(&dir) {
        Ok(map) => {
            info!(dir = %dir.display(), count = map.len(), "manifests loaded");
            map.into_iter().collect()
        }
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "manifest load failed; continuing with none");
            BTreeMap::new()
        }
    }
}

/// Own one configured session for the daemon's lifetime: attach, pump
/// events, reattach with backoff when the connection dies or the session
/// does not exist yet.
async fn session_task(inner: Arc<Inner>, idx: usize, mut stop: watch::Receiver<bool>) {
    let name = inner.sessions[idx].name.clone();
    let mut backoff = RECONNECT_MIN;
    let mut announced_missing = false;
    loop {
        if *stop.borrow() {
            return;
        }
        let mut ccfg = ControlConfig::attach(&name);
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
                    let mut link = inner.sessions[idx].link.lock().expect("session link lock");
                    link.attached = true;
                    link.watcher = Some(Arc::clone(&watcher));
                }
                info!(session = %name, "attached to tmux session");
                session_lifecycle(&inner, idx, true);
                run_session(&inner, &watcher, stop.clone()).await;
                {
                    let mut link = inner.sessions[idx].link.lock().expect("session link lock");
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
                warn!(session = %name, "tmux connection lost; reattaching");
            }
            Err(e) => {
                if !announced_missing {
                    warn!(session = %name, error = %e, "cannot attach; retrying with backoff");
                    announced_missing = true;
                } else {
                    debug!(session = %name, error = %e, "attach retry failed");
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
    let name = inner.sessions[idx].name.clone();
    let seq = inner.append_line(
        idx,
        LedgerLine {
            seq: 0,
            boot_id: String::new(),
            id: inner.mint_event_id(),
            ts: 0,
            kind: Kind::System,
            from: "cyclopsd".to_string(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(json!({
                "event": if attached { "attach" } else { "detach" },
                "session": name,
            })),
        },
    );
    inner.emit("session", json!({"name": name, "attached": attached}), seq);
}

/// Pump one attached watcher until it disconnects or the daemon stops.
async fn run_session(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    mut stop: watch::Receiver<bool>,
) {
    let mut rx = watcher.subscribe();
    // Bootstrap: the watcher's table is already authoritative; evaluate
    // every pane once so status answers immediately.
    for row in watcher.snapshot() {
        fusion::recompute_pane(inner, watcher, &row.pane_id, false, "bootstrap").await;
    }
    // Per-pane debounce kickers for output activity.
    let mut debounce: HashMap<String, mpsc::Sender<()>> = HashMap::new();
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if handle_pane_event(inner, watcher, &mut debounce, ev).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    // Missed hints degrade freshness, never correctness:
                    // reconcile and re-evaluate everything (level-triggered
                    // core, ADR revision 1).
                    warn!(session = %watcher.session(), missed, "event stream lagged; reconciling");
                    if watcher.reconcile_now().await.is_err() {
                        return;
                    }
                    for row in watcher.snapshot() {
                        fusion::recompute_pane(inner, watcher, &row.pane_id, false, "lag_reconcile").await;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

/// Apply one watcher event. Returns true when the connection is over.
async fn handle_pane_event(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    debounce: &mut HashMap<String, mpsc::Sender<()>>,
    ev: PaneEvent,
) -> bool {
    match ev {
        PaneEvent::PaneAdded(row) => {
            fusion::recompute_pane(inner, watcher, &row.pane_id, false, "pane_added").await;
            false
        }
        PaneEvent::PaneRemoved(id) => {
            debounce.remove(&id);
            inner
                .detections
                .lock()
                .expect("detections lock")
                .remove(&id);
            // Adoption ends with the pane; hook history dies with it too.
            inner.labels.lock().expect("labels lock").remove(&id);
            inner
                .hook_readings
                .lock()
                .expect("hook readings lock")
                .remove(&id);
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
                fusion::recompute_pane(inner, watcher, &id, false, "pane_changed").await;
            }
            false
        }
        PaneEvent::OutputActivity { pane_id, .. } => {
            kick_debounce(inner, watcher, debounce, pane_id);
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
        PaneEvent::Disconnected => true,
    }
}

/// Feed a pane's debounce task, spawning it on first activity. A full
/// channel means a recompute is already pending; nothing to do.
fn kick_debounce(
    inner: &Arc<Inner>,
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
        Arc::clone(watcher),
        pane_id,
    ));
}

/// Output settle debounce: a reset timer, not an interval. The sleep only
/// exists between the first kick and the settle; each further kick pushes
/// the deadline out. With no output, this task is parked in recv.
async fn debounce_task(
    mut rx: mpsc::Receiver<()>,
    inner: Arc<Inner>,
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
        fusion::recompute_pane(&inner, &watcher, &pane_id, false, "output_settled").await;
    }
}

/// Unix time in milliseconds.
pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
