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
mod chrome;
pub mod config;
mod delivery;
mod fusion;
mod history;
pub mod identity;
mod registry;
mod selftest;
mod server;

pub use config::Config;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
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
    /// One slot per configured session, fixed at boot.
    pub(crate) sessions: Vec<SessionSlot>,
    /// Push stream for events.subscribe connections.
    pub(crate) events: broadcast::Sender<Event>,
    /// Cached fusion verdict per pane id.
    pub(crate) detections: StdMutex<HashMap<String, DetEntry>>,
    /// Adoption registry: which pane wears which label, what manifest is
    /// pinned to it, and the tmux chrome it wore before cyclops arrived.
    /// Explicit adoption via pane.label (v1 keeper), durable across
    /// restarts (crates/cyclopsd/src/registry.rs).
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
    /// the pane.
    pub(crate) argv_cache: StdMutex<HashMap<(String, i32), Option<String>>>,
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
}

pub(crate) struct SessionSlot {
    pub(crate) name: String,
    pub(crate) link: StdMutex<SessionLink>,
    /// Append-only session ledger at $CYCLOPS_HOME/ledger/<session>.ndjson.
    pub(crate) ledger: Arc<LedgerWriter>,
    /// Pane table as of the last detach. Hook reports arriving while the
    /// control connection is down resolve against this (a report does not
    /// need the tmux connection); the live table wins whenever attached.
    pub(crate) last_panes: StdMutex<HashMap<String, PaneRow>>,
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
        let slot = self.sessions.get(session_idx)?;
        let link = slot.link.lock().expect("session link lock");
        link.watcher.as_ref().map(Arc::clone)
    }

    /// Resolve a recipient/target name: label first, then pane id. Only
    /// panes that currently exist resolve.
    pub(crate) fn resolve_recipient(&self, name: &str) -> Option<(usize, String)> {
        let wanted = self.label_target(name);
        for (idx, slot) in self.sessions.iter().enumerate() {
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
        for (idx, slot) in self.sessions.iter().enumerate() {
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
    let session = inner.sessions[session_idx].name.clone();
    let label = label.filter(|l| !l.is_empty());

    // 2. Validate the label.
    if let Some(l) = &label {
        if l == "*" || l == "admin" || l.starts_with('%') {
            return Err(bad_request(format!("label {l:?} is reserved")));
        }
        // A control character cannot survive onto a tmux command line
        // (quote_arg strips newlines), so the border would wear a
        // different name than the ledger. One name per pane, everywhere.
        if l.chars().any(char::is_control) {
            return Err(bad_request(format!(
                "label {l:?} has a control character in it"
            )));
        }
        if inner
            .registry
            .lock()
            .expect("registry lock")
            .label_taken_by_other(l, &pane_id)
        {
            return Err(bad_request(format!("label {l:?} is already taken")));
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
        None => unadopt_pane(inner, watcher.as_ref(), &pane_id).await?,
    }

    // 5. Record.
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
            data: Some(json!({
                "event": "pane_labeled",
                "pane_id": pane_id,
                "label": label,
                "manifest": manifest,
            })),
        },
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
    }))
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
    let session = inner.sessions[session_idx].name.clone();
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
        fusion::recompute_pane(inner, w, pane_id, false, "pane_labeled").await;
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
        fusion::recompute_pane(inner, w, pane_id, false, "pane_unlabeled").await;
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
    for (idx, slot) in inner.sessions.iter().enumerate() {
        let Some(watcher) = inner.watcher_of(idx) else {
            continue;
        };
        let adoptions = inner
            .registry
            .lock()
            .expect("registry lock")
            .in_session(&slot.name);
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
    for idx in 0..inner.sessions.len() {
        let Some(watcher) = inner.watcher_of(idx) else {
            continue;
        };
        let adopted = inner
            .registry
            .lock()
            .expect("registry lock")
            .in_session(watcher.session());
        paint_adoptions(inner, &watcher, &adopted).await;
    }
    inner.emit("theme", json!({"name": name}), None);
    name
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
        sessions.push(SessionSlot {
            name: name.clone(),
            link: StdMutex::new(SessionLink::default()),
            ledger: Arc::new(ledger),
            last_panes: StdMutex::new(HashMap::new()),
        });
    }
    // Adoptions from the previous run. Nothing is trusted onto a pane
    // yet; each session prunes its own entries against the live pane
    // table when it attaches (registry::restore_session).
    let (adoptions, warnings) = registry::Registry::load(&cfg.home);
    for w in warnings {
        warn!("registry: {w}");
    }
    let mut theme = cyclops_theme::ThemeWatch::new(&cfg.home);
    for w in theme.take_warnings() {
        warn!("theme: {w}");
    }

    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let inner = Arc::new(Inner {
        cfg,
        boot_id,
        started,
        tmux_version,
        manifests,
        sessions,
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

    // Restart-limbo closure: any delivery the previous run left in a
    // non-resolved state gets a named ending now (GOALS: limbo is a bug).
    delivery::close_limbo(&inner, &replayed);
    drop(replayed);

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
                    let mut link = inner.sessions[idx].link.lock().expect("session link lock");
                    link.attached = true;
                    link.watcher = Some(Arc::clone(&watcher));
                }
                info!(session = %name, "attached to tmux session");
                session_lifecycle(&inner, idx, true);
                run_session(&inner, &watcher, stop.clone()).await;
                // Freeze the pane table as of this detach: hook reports
                // arriving during the outage resolve against it.
                {
                    let mut last = inner.sessions[idx]
                        .last_panes
                        .lock()
                        .expect("last panes lock");
                    *last = watcher
                        .snapshot()
                        .into_iter()
                        .map(|r| (r.pane_id.clone(), r))
                        .collect();
                }
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
    // every pane once so status answers immediately. Adoptions are
    // reconciled against that table first, so the very first recompute
    // already knows which panes are named and which manifest is pinned.
    reconcile_adoptions(inner, watcher).await;
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
        match reg.restore_session(watcher.session(), &live) {
            Ok(kept) => kept,
            Err(e) => {
                error!(session = %watcher.session(), error = %e, "cannot rewrite the registry; keeping it in memory only");
                reg.in_session(watcher.session())
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
async fn move_chrome(inner: &Arc<Inner>, watcher: &Arc<SessionWatcher>, pane_id: &str) {
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
    let session_idx = inner
        .sessions
        .iter()
        .position(|s| s.name == watcher.session());
    if let Some(idx) = session_idx {
        paint_chrome(inner, idx, pane_id).await;
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
            inner.emit(
                "pane-removed",
                json!({
                    "ts": unix_ms(),
                    "session": watcher.session(),
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
                fusion::recompute_pane(inner, watcher, &id, false, "pane_changed").await;
            }
            // A move does not touch agent state, but it does move half the
            // chrome: the pane carries its own options and the window's
            // border text does not follow it.
            if changed.iter().any(|f| matches!(f, PaneField::WindowId)) {
                move_chrome(inner, watcher, &id).await;
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
