//! cyclopsd: the Cyclops shadow daemon (M0, read-only).
//!
//! M0 scope: watch configured tmux sessions over control mode, fuse
//! manifest rules over pane titles and screens into an AgentState per
//! pane, and serve the NDJSON socket API (ping, status, pane.read,
//! events.subscribe). No delivery, no ledger writes yet.
//!
//! Zero-polling contract: nothing here re-queries state on a clock. The
//! only timers are the per-pane output settle debounce (a reset timer that
//! exists only while output is arriving) and the watcher reconnect backoff
//! (a retry sleep after a connection dies, not a state poll).
//!
//! The crate is a library so integration tests boot the daemon in-process;
//! main.rs is a thin wrapper adding signals and logging.

pub mod config;
mod fusion;
mod peercred;
mod server;

pub use config::Config;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cyclops_manifest::Manifest;
use cyclops_proto::{AgentState, Detection, Event};
use cyclops_tmux::{ControlConfig, PaneEvent, PaneField, SessionWatcher, TmuxVersion};
use serde_json::json;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

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
}

pub(crate) struct SessionSlot {
    pub(crate) name: String,
    pub(crate) link: StdMutex<SessionLink>,
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
    /// Emit a fused-state change to subscribers. Target equals the pane id
    /// until the adoption registry provides labels (M1).
    pub(crate) fn emit_state(
        &self,
        pane_id: &str,
        state: AgentState,
        prior: Option<AgentState>,
        disagreement: bool,
    ) {
        let ev = Event {
            event: "state".to_string(),
            data: json!({
                "target": pane_id,
                "pane_id": pane_id,
                "state": state,
                "prior": prior,
                "disagreement": disagreement,
            }),
            seq: None,
        };
        // No subscribers is normal; the send result is irrelevant.
        let _ = self.events.send(ev);
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
    /// control clients, then remove the socket file.
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
        let _ = std::fs::remove_file(self.socket_path());
        info!("cyclopsd stopped");
    }
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
    let sessions = cfg
        .sessions
        .iter()
        .map(|name| SessionSlot {
            name: name.clone(),
            link: StdMutex::new(SessionLink::default()),
        })
        .collect();
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
    });

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
        "cyclopsd booted (M0 shadow mode, read-only)"
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
                run_session(&inner, &watcher, stop.clone()).await;
                {
                    let mut link = inner.sessions[idx].link.lock().expect("session link lock");
                    link.attached = false;
                    link.watcher = None;
                }
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
