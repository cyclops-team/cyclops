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
mod attention_resolution;
mod chrome;
mod composer_recovery;
pub mod config;
mod deadlock;
mod delivery;
mod fusion;
mod history;
mod hook_lifecycle;
pub mod identity;
mod livesession;
pub mod mailbox;
mod messaging;
mod notification_adapter;
mod registry;
mod selftest;
mod server;
mod sessionid;
mod sessionstore;
pub(crate) mod turnkey;
mod workspace_ui;
// Removed when daemon startup reads the workspace identity.
#[allow(dead_code)]
mod workspaceid;

pub use config::Config;
/// The exact bytes a delivery puts in a composer. Public because it IS
/// the payload contract: the hook acknowledgement matcher compares a
/// vendor's reported prompt against these bytes, so anything that needs
/// to reason about a delivery's text has to build it the same way.
pub use delivery::render_payload;
#[doc(hidden)]
pub use delivery::{prove_composer_seam, ComposerSeamProof};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cyclops_ledger::LedgerWriter;
use cyclops_manifest::Manifest;
use cyclops_proto::{
    AdminNotifyParams, AgentState, Detection, Event, Kind, LedgerLine, MessageId,
    MessagesSnapshotResult, MsgSendParams, NotificationAttemptId, NotificationRouteEvidenceId,
    NotificationWithdrawResult, ProcessInstanceId, RecipientKey, SessionIdentityBinding,
    SessionInstanceId, StateReportParams, TmuxPaneId, TmuxSessionId, WireError, WorkspaceId,
};
use cyclops_state::{RepairSummary, StateRoot};
use cyclops_tmux::{
    pane_session_id, ControlConfig, PaneEvent, PaneField, PaneRow, SessionWatcher, TmuxError,
    TmuxVersion,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Recompute a pane this long after its last output activity settles.
const OUTPUT_SETTLE: Duration = Duration::from_millis(300);
/// Watcher reconnect backoff bounds (reconnects only, never state polls).
const RECONNECT_MIN: Duration = Duration::from_millis(200);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// Maximum wall time a pane-label request lends to authoritative discovery
/// after its normal cache lookup misses.
const NAME_RECONCILE_TIMEOUT: Duration = Duration::from_secs(1);
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

/// Test seam: force and optionally pause a watcher reconcile requested by
/// `pane.label`. Always None in production.
pub(crate) type NameReconcilePause = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// One pane inside one append-only daemon session slot.
///
/// tmux pane ids are unique only inside a tmux server. Two watched servers
/// may both have `%1`, so every boot-scoped runtime cache uses the slot as
/// part of the key. The slot index is stable for the daemon's lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PaneKey {
    pub(crate) session_idx: usize,
    pub(crate) pane_id: String,
}

impl PaneKey {
    pub(crate) fn new(session_idx: usize, pane_id: &str) -> Self {
        Self {
            session_idx,
            pane_id: pane_id.to_string(),
        }
    }
}

/// Shared daemon state. Everything the socket server and the fusion engine
/// need lives here behind one Arc.
pub(crate) struct Inner {
    pub(crate) cfg: Config,
    /// Validated descriptor anchoring every session-ledger open in this daemon.
    pub(crate) state_root: Arc<StateRoot>,
    /// One boot-wide aggregate recorded on each session's boot fact.
    pub(crate) state_repair: RepairSummary,
    /// Durable identity of this state root. Loaded before any mailbox or
    /// session identity is opened.
    pub(crate) workspace_id: WorkspaceId,
    /// Durable live-session assignments. Persistence and adoption happen
    /// while this lock is held; the lock is never held across an await.
    pub(crate) session_identities: StdMutex<sessionstore::SessionIdentities>,
    pub(crate) mailbox: Option<Arc<mailbox::MailboxService>>,
    /// Boot-scoped reconciliation state for barriers derived from the
    /// canonical workspace projection.
    pub(crate) composer_recovery: StdMutex<composer_recovery::RecoveryCoordinator>,
    /// Serializes route state, directory replacement, and authenticated reads.
    pub(crate) mailbox_publication: StdMutex<()>,
    /// Serializes unread badge derivations and tmux writes across concurrent sync requests.
    ///
    /// (1) Global scope is deliberate because badge writes are rare and short,
    /// avoiding an unbounded per-pane lock map.
    /// (2) `sync_pane_unread` acquires this gate with no std guard held.
    /// (3) Code executed under it must not recurse into `sync_pane_unread`.
    pub(crate) unread_projection_gate: tokio::sync::Mutex<()>,
    /// Recipient keys whose tmux unread projection is dirty. One daemon-owned
    /// worker drains this set, so a burst allocates neither one task nor one
    /// queue entry per message.
    unread_projection_pending: StdMutex<HashSet<RecipientKey>>,
    unread_projection_wake: Notify,
    unread_projection_stopping: AtomicBool,
    unread_projection_pause: StdMutex<Option<Arc<UnreadProjectionTestPause>>>,
    #[cfg(test)]
    mailbox_publish_pause: StdMutex<Option<MailboxPublishPause>>,
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
    /// Serializes the check, open, and publish transaction for a runtime
    /// session registration with a followed rename. The protected section
    /// never awaits.
    pub(crate) session_registration: StdMutex<()>,
    /// Push stream for events.subscribe connections.
    pub(crate) events: broadcast::Sender<Event>,
    /// Cached fusion verdict per exact watched pane route.
    pub(crate) detections: StdMutex<HashMap<PaneKey, DetEntry>>,
    /// Pane-local causal route observation generation for notification
    /// reproof. Synthetic reconciliation reads this map without advancing it.
    pub(crate) route_evidence_generations: StdMutex<HashMap<PaneKey, u64>>,
    /// One observation transaction per pane. Capture and cache commit stay in
    /// order, so an older slow capture cannot overwrite a newer verdict or
    /// mutate lifecycle candidates after the newer observation settled them.
    pub(crate) pane_recomputes: StdMutex<HashMap<PaneKey, Arc<tokio::sync::Mutex<()>>>>,
    /// One event-driven lifecycle settlement task per exact pane route.
    /// Candidate replacements wake the existing task instead of spawning
    /// another sleeper for the same pane.
    pub(crate) lifecycle_rechecks: StdMutex<HashMap<PaneKey, fusion::LifecycleRecheckTask>>,
    /// Adoption registry: which pane wears which label, what manifest is
    /// pinned to it, and the tmux chrome it wore before cyclops arrived.
    /// Explicit adoption via pane.label (v1 keeper), durable across
    /// restarts (src/cyclopsd/src/registry.rs).
    pub(crate) registry: StdMutex<registry::Registry>,
    /// Active theme for the pane border chrome, re-stat'ed on the state
    /// change that is about to repaint (cyclops-theme's hot reload rule:
    /// the stat rides an event, no timer exists).
    pub(crate) theme: StdMutex<cyclops_theme::ThemeWatch>,
    /// Latest hook sensor reading per exact watched pane route, plus
    /// the aging state that keeps a stale edge from pinning fused state.
    pub(crate) hook_readings: StdMutex<HashMap<PaneKey, fusion::HookEntry>>,
    /// Lifecycle hook reports that need a later watcher observation before
    /// they can change runtime state or verify a delivery.
    pub(crate) hook_lifecycle: StdMutex<hook_lifecycle::Store>,
    /// Authenticated turn ENDS, kept apart from the runtime hook reading
    /// above and never evicted by it.
    ///
    /// The two answer different questions. A hook reading is an ephemeral
    /// opinion about what the pane is doing NOW, and fusion is right to
    /// age it out or drop it when the rules contradict it three times
    /// running. A turn end is a fact about a turn that happened, and the
    /// composer hold consumes it possibly seconds later. Storing one in
    /// the other let the runtime eviction destroy lifecycle evidence: a
    /// `Stop` recorded while the vendor was still painting its working
    /// row disagreed with the rules, hit the disagreement limit, and was
    /// erased before the hold it belonged to could read it.
    pub(crate) turn_ends: StdMutex<turnkey::Ends>,
    /// argv-basename cache for manifest binding, per (route, pane pid).
    /// Filled lazily when process-name binding misses; entries die with
    /// the pane. Only a basename that actually bound a manifest is ever
    /// stored, so a miss means "not settled yet" rather than "no agent".
    /// See [`fusion::argv_bound_manifest`] for the exec race that rule
    /// exists to survive.
    pub(crate) argv_cache: StdMutex<HashMap<(PaneKey, identity::ProcId), String>>,
    /// Delivery pipeline state.
    pub(crate) engine: delivery::Engine,
    /// Hook report dedupe state.
    pub(crate) ack_state: ack::AckState,
    /// Per-pane hook edge record behind hooks_verified, hooks.verify, and
    /// the one-time missing-hook notification.
    pub(crate) hook_liveness: selftest::HookLiveness,
    /// Test-only injection pause, see [`InjectPause`].
    pub(crate) inject_pause: StdMutex<Option<InjectPause>>,
    /// Test-only forced naming reconcile and pause, see [`NameReconcilePause`].
    pub(crate) name_reconcile_pause: StdMutex<Option<NameReconcilePause>>,
    /// Test-only: make the `--clear` chrome restore fail the way tmux
    /// refusing a command would. See [`Daemon::fail_chrome_restore`].
    pub(crate) fail_chrome_restore: AtomicBool,
    /// Test-only: make the next final pre-write process observation
    /// unavailable after the admitting capture has completed.
    pub(crate) fail_next_final_binding_observation: AtomicBool,
    /// Test-only: fail at the synchronous on_write boundary before record_writing for a specific attempt.
    pub(crate) fail_pre_record_writing: StdMutex<Option<NotificationAttemptId>>,
    /// Last-active workspace/tab for the terminal workspace UI.
    pub(crate) workspace_ui: StdMutex<workspace_ui::WorkspaceUiState>,
    /// Self-shutdown request sent only after a successful daemon.shutdown
    /// response has reached its authenticated client.
    pub(crate) shutdown_request: watch::Sender<bool>,
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

#[cfg(test)]
struct SessionRenamePause {
    entered: std::sync::mpsc::Sender<()>,
    release: Arc<std::sync::Barrier>,
}

pub(crate) struct SessionSlot {
    /// Mutable so a followed session rename (`PaneEvent::SessionRenamed`,
    /// `handle_pane_event`) can update it in place: `session_index` then
    /// keeps resolving this same slot under the new name instead of a
    /// `watch_session` for the new name opening a second slot + watcher for
    /// one tmux session. Go through [`SessionSlot::name`] and
    /// [`SessionSlot::rename`] rather than the field directly.
    name: StdMutex<String>,
    /// A rename can make a live runtime slot collide with a configured slot
    /// that was still waiting for that name. Indices are durable for the
    /// daemon's lifetime, so the losing slot cannot be removed from
    /// `Inner::sessions`; it becomes an alias of the live canonical slot
    /// instead. Historical ledger reads may still address this slot by index,
    /// while every live traversal skips it.
    ///
    /// `usize::MAX` means canonical. Any other value is the canonical slot's
    /// append-only index.
    alias_of: AtomicUsize,
    /// Wakes an already-published session task when this slot loses a rename
    /// collision. The atomic above is the state; this channel is only the
    /// edge that makes a blocked task observe it immediately.
    alias_changed: watch::Sender<Option<usize>>,
    #[cfg(test)]
    rename_pause: StdMutex<Option<SessionRenamePause>>,
    pub(crate) link: StdMutex<SessionLink>,
    /// Append-only session ledger at $CYCLOPS_HOME/ledger/<session>.ndjson,
    /// opened once when the slot is created (`boot`, `watch_session`) and
    /// held open for the slot's life. A followed rename does NOT reopen it:
    /// appends keep landing in the file the watcher was attached under when
    /// it started, because the OS handle this holds is keyed by inode, not
    /// by the path or by `name` above. `<new-name>.ndjson` is only opened
    /// if a later boot or runtime `session.watch` registers that name as a
    /// fresh slot. This is deliberate: the alternative (closing
    /// this handle and opening a second file mid-session) would split one
    /// session's record across two files with no line in either saying so.
    pub(crate) ledger: Arc<LedgerWriter>,
    /// Pane table and exact root generations retained across a detach.
    pub(crate) last_panes: StdMutex<HashMap<String, ObservedPane>>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ObservedPane {
    pub(crate) row: PaneRow,
    pub(crate) root: Option<identity::ProcId>,
}

impl ObservedPane {
    fn capture(row: PaneRow) -> ObservedPane {
        let root = identity::ProcId::of(row.pane_pid);
        ObservedPane { row, root }
    }
}

fn live_pane_roots_are_proven(rows: &[ObservedPane]) -> bool {
    rows.iter().all(|pane| pane.row.dead || pane.root.is_some())
}

impl SessionSlot {
    /// A freshly attached slot: no link yet, no pane history yet. `boot`
    /// and `watch_session` both build slots this way so the two paths a
    /// session can join the daemon by stay in lockstep.
    pub(crate) fn new(name: String, ledger: Arc<LedgerWriter>) -> Self {
        let (alias_changed, _) = watch::channel(None);
        SessionSlot {
            name: StdMutex::new(name),
            alias_of: AtomicUsize::new(usize::MAX),
            alias_changed,
            #[cfg(test)]
            rename_pause: StdMutex::new(None),
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

    fn alias_of(&self) -> Option<usize> {
        let idx = self.alias_of.load(Ordering::Acquire);
        (idx != usize::MAX).then_some(idx)
    }

    fn is_canonical(&self) -> bool {
        self.alias_of().is_none()
    }

    fn journal_file_name(&self) -> Option<String> {
        self.ledger
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    /// Retire this slot behind `canonical_idx` without invalidating its
    /// historical index. Only the session-registration transaction calls
    /// this. A live loser is woken so its task tears down its duplicate
    /// watcher instead of waiting for another tmux event.
    fn retire_as_alias(&self, canonical_idx: usize) -> bool {
        let retired = self
            .alias_of
            .compare_exchange(
                usize::MAX,
                canonical_idx,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if retired {
            self.alias_changed.send_replace(Some(canonical_idx));
        }
        retired
    }

    fn retarget_alias(&self, prior_idx: usize, canonical_idx: usize) -> bool {
        self.alias_of
            .compare_exchange(
                prior_idx,
                canonical_idx,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Current observed rows while attached, otherwise the retained rows.
    fn mailbox_route(&self) -> Option<MailboxSessionRoute> {
        let (identity, panes, attached) = {
            let link = self.link.lock().expect("session link lock");
            (
                link.identity.clone(),
                link.mailbox_panes.values().cloned().collect::<Vec<_>>(),
                link.attached,
            )
        };
        let instance_id = identity?.session_instance_id();
        let rows = if attached {
            panes
        } else {
            self.last_panes
                .lock()
                .expect("last panes lock")
                .values()
                .cloned()
                .collect()
        };
        Some(MailboxSessionRoute {
            session_idx: usize::MAX,
            instance_id,
            attached,
            panes: rows,
        })
    }
}

#[derive(Default)]
pub(crate) struct SessionLink {
    pub(crate) attached: bool,
    pub(crate) watcher: Option<Arc<SessionWatcher>>,
    /// Published only after the live key has been observed and persisted.
    /// Kept while detached so a last-known pane retains its mailbox.
    pub(crate) identity: Option<SessionIdentityBinding>,
    mailbox_panes: HashMap<String, ObservedPane>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ComposerProjection {
    pub(crate) state: cyclops_proto::ComposerState,
    pub(crate) proof: cyclops_proto::ComposerProof,
    pub(crate) notification_attempt: Option<cyclops_proto::NotificationAttemptId>,
    /// Stable, content-free code explaining why ownership is unresolved.
    pub(crate) reason: Option<&'static str>,
    /// Number of active durable barriers considered for this composer.
    pub(crate) candidate_count: u32,
    /// Exact process and manifest observation behind an owned projection.
    /// Ambiguous projections never carry a binding that another surface could
    /// mistake for authorization.
    pub(crate) binding: Option<fusion::Binding>,
}

impl Default for ComposerProjection {
    fn default() -> Self {
        Self {
            state: cyclops_proto::ComposerState::ComposerAmbiguous,
            proof: cyclops_proto::ComposerProof::Unprovable,
            notification_attempt: None,
            reason: Some("composer_not_observed"),
            candidate_count: 0,
            binding: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct DetEntry {
    pub(crate) detection: Detection,
    /// Complete process and manifest observation that produced this cached
    /// readiness verdict. A partial identity cannot authorize a later reopen.
    pub(crate) binding: Option<fusion::Binding>,
    /// Manifest id bound at the last recompute.
    pub(crate) manifest: Option<String>,
    /// The foreground process-group leader this verdict describes, when it
    /// could be proven. A pane id names a place, not an occupant: an agent
    /// can exit and another start at the same shell prompt, inheriting the
    /// pane id, the root pid and the manifest. Without this, a retained
    /// verdict is attributed to whoever is there now.
    ///
    /// None means nobody proved it, which never matches a proven binding.
    pub(crate) occupant: Option<i32>,
    /// The admitted AGENT identity behind this verdict, resolved by the
    /// recompute that produced it.
    ///
    /// Published with the stamp so readers do not each go and inspect
    /// processes: `status` walks every pane under one lock, and doing
    /// identity work there put a process spawn per pane on the hot path
    /// and held the detection lock across all of them.
    pub(crate) agent: Option<identity::ProcId>,
    /// Whether the pane was in a mode at the last recompute. Kept so a
    /// hold change can re-stamp the cached verdict without going back to
    /// tmux for a fact that has not moved.
    pub(crate) in_mode: bool,
    /// Whether this exact occupant has produced a fresh, agreeing,
    /// non-quota screen observation this boot. It gates the one durable
    /// quota-reset scan and survives title-only redraws that carry no new
    /// screen evidence.
    pub(crate) quota_screen_clear: bool,
    /// What this pane's composer has proven since text was last seen
    /// staged in it. Carried across recomputes, dropped with the
    /// occupant: a hold is about one agent's composer, not the place.
    pub(crate) hold: cyclops_proto::ComposerHold,
    /// The turn that hold is waiting on, when the vendor can name one.
    /// Structural and daemon-internal, which is why it sits beside the
    /// hold rather than inside it.
    pub(crate) turn: Option<turnkey::TurnKey>,
    /// Which delivery attempt owns the barrier, content-free.
    ///
    /// A pane binding is not enough, because evidence outlives the
    /// delivery it belongs to. A resolved delivery stays in the hook ACK
    /// registry, so a late upgrade for delivery A can arrive after A's
    /// turn ended and B claimed the composer. Without an owner, A's edge
    /// would settle B's barrier: promoting, releasing or clearing a
    /// hold that belongs to a payload A never wrote.
    pub(crate) hold_owner: Option<String>,
    /// Content-free result of joining the current composer observation with
    /// the exact active notification barrier. Composer bytes never enter the
    /// cache or a status response.
    pub(crate) composer: ComposerProjection,
    /// Runtime certainty is separate from runtime state. A provisional
    /// authenticated start reports Working immediately without claiming that
    /// the vendor has visually accepted it yet.
    pub(crate) working_confirmed: bool,
    /// When the fused STATE last changed, not when it was last computed.
    /// A recompute that lands on the same state keeps this, which is what
    /// lets `status` say "working for 13m" instead of "working since the
    /// last event". The roster's elapsed column reads it.
    pub(crate) since: std::time::Instant,
}

/// A ledger line the daemon itself is authoring, not relaying for an agent.
/// `seq`, `boot_id`, and `ts` are placeholders here on purpose: the ledger
/// writer fills all three in at append time (`cyclops-ledger`), so a value
/// set here would just be discarded. `from` is always "cyclopsd"; every
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

    /// How many append-only session slots exist (configured plus any added
    /// since boot). Retired aliases remain in this count because callers that
    /// walk `0..session_count()` use the result as stable ledger indices.
    pub(crate) fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions lock").len()
    }

    /// The index of a watched session by name, if it is already watched.
    pub(crate) fn session_index(&self, name: &str) -> Option<usize> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .iter()
            .enumerate()
            .find_map(|(idx, slot)| (slot.is_canonical() && slot.name() == name).then_some(idx))
    }

    /// A cloned snapshot of every session slot, for iteration. Lock the
    /// sessions vector, clone every `Arc`, release: the lock never has to
    /// span the loop body that follows.
    pub(crate) fn session_slots(&self) -> Vec<Arc<SessionSlot>> {
        self.sessions.lock().expect("sessions lock").clone()
    }

    /// Canonical live slots with their append-only indices. A retired alias
    /// remains available through `session(idx)` and `session_slots()` for
    /// historical ledger reads, but must never take part in routing, status,
    /// mailbox publication, or another watcher attachment.
    pub(crate) fn active_session_slots(&self) -> Vec<(usize, Arc<SessionSlot>)> {
        self.sessions
            .lock()
            .expect("sessions lock")
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_canonical())
            .map(|(idx, slot)| (idx, Arc::clone(slot)))
            .collect()
    }

    /// Emit a fused-state change: one kind=state ledger line on the
    /// session's ledger plus a "state" event. Gate/state lines carry rule
    /// ids and causes, never raw screen captures (secrets rule).
    ///
    /// Takes the session's slot index directly rather than resolving it
    /// from a name here. A name resolved through `watcher.session()` can be
    /// ahead of this daemon's own `SessionSlot::rename` because the watcher
    /// updates its name live, at notification time, while the matching
    /// slot rename only lands when this process gets around to handling
    /// the `PaneEvent::SessionRenamed` that follows it. Therefore a caller
    /// recomputing a pane on an event that predates the rename must not
    /// re-derive the index from the (already new) name at emit time, or
    /// `session_index` misses during that window and the append silently
    /// drops (seq `None`). Every caller already carries a stable
    /// `session_idx` from where it entered the session (`session_task`'s
    /// `idx`, `handle.session_idx`, `resolve_recipient`'s return, ...),
    /// which is append-only-stable for the daemon's life; passing that
    /// through closes the window instead of reopening it here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_state(
        &self,
        session_idx: usize,
        pane_id: &str,
        det: &Detection,
        prior: Option<AgentState>,
        cause: &str,
        source: (Option<crate::identity::ProcId>, &str),
        working_confirmed: bool,
    ) {
        let target = self
            .label_for_route(session_idx, pane_id)
            .unwrap_or_else(|| pane_id.to_string());
        let recipient = self.recipient_key(session_idx, pane_id);
        let seq = self.append_line(
            session_idx,
            daemon_line(
                Kind::State,
                self.mint_event_id(),
                json!({
                    "pane_id": pane_id,
                    "session_idx": session_idx,
                    "target": target,
                    "recipient": recipient,
                    "state": det.state,
                    "prior": prior,
                    "disagreement": det.disagreement,
                    "decided_by": det.decided_by,
                    "cause": cause,
                    "working_confirmed": working_confirmed,
                }),
            ),
        );
        // The binding that produced this verdict travels with it. A pane
        // id is reusable, so a consumer asking "is this event about my
        // delivery" cannot answer from the id alone, and looking the row
        // up later answers about whoever holds the pane by then rather
        // than about whoever produced the event.
        let (source_agent, source_manifest) = source;
        let observed_at_ms = unix_ms();
        self.emit(
            "state",
            json!({
                "target": target,
                "recipient": recipient,
                "pane_id": pane_id,
                "session_idx": session_idx,
                "state": det.state,
                "prior": prior,
                "disagreement": det.disagreement,
                "decided_by": det.decided_by,
                "source_pid": source_agent.map(|a| a.pid),
                "source_birth": source_agent.map(|a| a.birth),
                "source_manifest": source_manifest,
                "observed_at_ms": observed_at_ms,
                "working_confirmed": working_confirmed,
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

    /// Cached fused state for one exact pane route; Unknown when never computed.
    pub(crate) fn cached_state(&self, session_idx: usize, pane_id: &str) -> AgentState {
        self.detections
            .lock()
            .expect("detections lock")
            .get(&PaneKey::new(session_idx, pane_id))
            .map(|e| e.detection.state)
            .unwrap_or(AgentState::Unknown)
    }

    /// Current durable identity for this pane's route evidence.
    pub(crate) fn route_evidence_id(
        &self,
        session_idx: usize,
        pane_id: &str,
    ) -> NotificationRouteEvidenceId {
        let generation = self
            .route_evidence_generations
            .lock()
            .expect("route evidence generations lock")
            .get(&PaneKey::new(session_idx, pane_id))
            .copied()
            .unwrap_or(0);
        NotificationRouteEvidenceId {
            boot_id: self.boot_id.clone(),
            generation,
        }
    }

    /// Advance one real watcher, process, adoption, or readiness observation.
    pub(crate) fn advance_route_evidence(
        &self,
        session_idx: usize,
        pane_id: &str,
    ) -> NotificationRouteEvidenceId {
        let generation = {
            let mut generations = self
                .route_evidence_generations
                .lock()
                .expect("route evidence generations lock");
            let generation = generations
                .entry(PaneKey::new(session_idx, pane_id))
                .or_insert(0);
            *generation = generation.saturating_add(1);
            *generation
        };
        NotificationRouteEvidenceId {
            boot_id: self.boot_id.clone(),
            generation,
        }
    }

    /// Durable recipient for a pane inside one exact daemon session slot.
    pub(crate) fn recipient_key(&self, session_idx: usize, pane_id: &str) -> Option<RecipientKey> {
        let slot = self.session(session_idx)?;
        let instance_id = slot
            .link
            .lock()
            .expect("session link lock")
            .identity
            .as_ref()?
            .session_instance_id();
        Some(RecipientKey::agent(
            self.workspace_id,
            instance_id,
            pane_id.parse().ok()?,
        ))
    }

    /// Adoption for one current route, including its pane-root generation.
    pub(crate) fn adoption_for_route(
        &self,
        session_idx: usize,
        pane_id: &str,
    ) -> Option<registry::Adoption> {
        let recipient = self.recipient_key(session_idx, pane_id)?;
        let slot = self.session(session_idx)?;
        let (attached, watcher) = {
            let link = slot.link.lock().expect("session link lock");
            (link.attached, link.watcher.as_ref().map(Arc::clone))
        };
        let root = if attached {
            let row = watcher?.pane(pane_id)?;
            identity::ProcId::of(row.pane_pid)?
        } else {
            slot.last_panes
                .lock()
                .expect("last panes lock")
                .get(pane_id)?
                .root?
        };
        let root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
        self.registry
            .lock()
            .expect("registry lock")
            .for_route(recipient, root)
            .cloned()
    }

    /// Adoption for an exact route or a same-server pane transfer.
    ///
    /// An exact adoption wins even when it has no manifest pin. A physical
    /// fallback is valid only while two durable sessions describe the same
    /// tmux server generation. Pane IDs and pane roots alone are not identities
    /// across a server restart.
    pub(crate) fn adoption_for_observed_route(
        &self,
        recipient: RecipientKey,
        pane_id: &str,
        pane_root: ProcessInstanceId,
    ) -> Option<registry::Adoption> {
        let physical = {
            let registry = self.registry.lock().expect("registry lock");
            if let Some(exact) = registry.for_route(recipient, pane_root) {
                return Some(exact.clone());
            }
            registry.for_physical_pane(pane_id, pane_root)?.clone()
        };
        let current_session = recipient.session_instance_id()?;
        let physical_session = physical.recipient?.session_instance_id()?;
        self.session_identities
            .lock()
            .expect("session identities lock")
            .same_tmux_server_generation(current_session, physical_session)
            .then_some(physical)
    }

    /// Exact adopted label for a route, independent of duplicate tmux pane ids.
    pub(crate) fn label_for_route(&self, session_idx: usize, pane_id: &str) -> Option<String> {
        self.adoption_for_route(session_idx, pane_id)
            .map(|adoption| adoption.label)
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
        if !slot.is_canonical() {
            return None;
        }
        let link = slot.link.lock().expect("session link lock");
        link.watcher.as_ref().map(Arc::clone)
    }

    /// Resolve a recipient/target name: label first, then pane id. Only
    /// panes that currently exist resolve.
    pub(crate) fn resolve_recipient(&self, name: &str) -> Option<(usize, String)> {
        let exact = self
            .registry
            .lock()
            .expect("registry lock")
            .for_label(name)
            .and_then(|adoption| adoption.recipient);
        if let Some(recipient) = exact {
            let session_instance_id = recipient.session_instance_id()?;
            let pane_id = recipient.pane_id()?.to_string();
            for (idx, slot) in self.active_session_slots() {
                let watcher = {
                    let link = slot.link.lock().expect("session link lock");
                    if link
                        .identity
                        .as_ref()
                        .map(SessionIdentityBinding::session_instance_id)
                        != Some(session_instance_id)
                    {
                        continue;
                    }
                    link.watcher.as_ref().map(Arc::clone)
                };
                if let Some(row) = watcher.and_then(|watcher| watcher.pane(&pane_id)) {
                    let addressable = identity::ProcId::of(row.pane_pid)
                        .and_then(|root| ProcessInstanceId::new(root.pid, root.birth).ok())
                        .is_some_and(|pane_root| {
                            self.registry
                                .lock()
                                .expect("registry lock")
                                .for_route(recipient, pane_root)
                                .is_some()
                        });
                    if addressable {
                        return Some((idx, row.pane_id));
                    }
                }
            }
            return None;
        }
        let wanted = name;
        let mut found = None;
        for (idx, slot) in self.active_session_slots() {
            let watcher = {
                let link = slot.link.lock().expect("session link lock");
                link.watcher.as_ref().map(Arc::clone)
            };
            if let Some(w) = watcher {
                if let Some(row) = w.pane(wanted) {
                    if found.replace((idx, row.pane_id)).is_some() {
                        return None;
                    }
                }
            }
        }
        found
    }

    /// Resolve a name against the last-known pane tables of DETACHED
    /// sessions. Hook reports do not need the tmux connection: a report for
    /// a pane that existed at detach must still match ACKs, or every detach
    /// blinds tier 1 (the m1 soak's duplicate-delivery failure).
    pub(crate) fn resolve_recipient_last_known(&self, name: &str) -> Option<(usize, PaneRow)> {
        let exact = self
            .registry
            .lock()
            .expect("registry lock")
            .for_label(name)
            .and_then(|adoption| adoption.recipient);
        if let Some(recipient) = exact {
            let session_instance_id = recipient.session_instance_id()?;
            let pane_id = recipient.pane_id()?.to_string();
            for (idx, slot) in self.active_session_slots() {
                let link = slot.link.lock().expect("session link lock");
                if link.attached
                    || link
                        .identity
                        .as_ref()
                        .map(SessionIdentityBinding::session_instance_id)
                        != Some(session_instance_id)
                {
                    continue;
                }
                drop(link);
                let pane = slot
                    .last_panes
                    .lock()
                    .expect("last panes lock")
                    .get(&pane_id)
                    .cloned();
                if let Some(pane) = pane {
                    let addressable = pane
                        .root
                        .and_then(|root| ProcessInstanceId::new(root.pid, root.birth).ok())
                        .is_some_and(|pane_root| {
                            self.registry
                                .lock()
                                .expect("registry lock")
                                .for_route(recipient, pane_root)
                                .is_some()
                        });
                    if addressable {
                        return Some((idx, pane.row));
                    }
                }
            }
            return None;
        }
        let wanted = name;
        let mut found = None;
        for (idx, slot) in self.active_session_slots() {
            if slot.link.lock().expect("session link lock").attached {
                continue; // live table is authoritative while attached
            }
            let last = slot.last_panes.lock().expect("last panes lock");
            if let Some(pane) = last.get(wanted) {
                if found.replace((idx, pane.row.clone())).is_some() {
                    return None;
                }
            }
        }
        found
    }
}

struct MailboxRouteOverride<'a> {
    session_idx: usize,
    instance_id: SessionInstanceId,
    rows: &'a [ObservedPane],
}

struct MailboxSessionRoute {
    session_idx: usize,
    instance_id: SessionInstanceId,
    attached: bool,
    panes: Vec<ObservedPane>,
}

struct MailboxPaneChoice {
    attached: bool,
    pane: Option<(usize, SessionInstanceId, ObservedPane)>,
}

#[cfg(test)]
struct MailboxPublishPause {
    entered: std::sync::mpsc::Sender<()>,
    release: Arc<std::sync::Barrier>,
}

/// Snapshot mailbox routing without nesting daemon locks.
///
/// Lock order is snapshots only: sessions, then each session link or pane
/// table, then registry labels, then the mailbox directory. Every guard is
/// dropped before the next lock is taken, and no guard crosses an await.
fn mailbox_routes(
    inner: &Inner,
    proposed: Option<&MailboxRouteOverride<'_>>,
) -> Vec<MailboxSessionRoute> {
    inner
        .active_session_slots()
        .into_iter()
        .filter_map(|(idx, slot)| {
            proposed
                .filter(|route| route.session_idx == idx)
                .map(|route| MailboxSessionRoute {
                    session_idx: idx,
                    instance_id: route.instance_id,
                    attached: true,
                    panes: route.rows.to_vec(),
                })
                .or_else(|| {
                    slot.mailbox_route().map(|mut route| {
                        route.session_idx = idx;
                        route
                    })
                })
        })
        .collect()
}

/// Resolve each exact recipient once. Live routes outrank retained routes;
/// equal-rank conflicts for the same durable recipient are omitted because
/// neither root generation is provable.
fn mailbox_panes(
    inner: &Inner,
    proposed: Option<&MailboxRouteOverride<'_>>,
) -> Vec<(usize, SessionInstanceId, ObservedPane)> {
    let mut selected: BTreeMap<RecipientKey, MailboxPaneChoice> = BTreeMap::new();
    for route in mailbox_routes(inner, proposed) {
        for pane in route.panes {
            let Ok(pane_id) = pane.row.pane_id.parse() else {
                continue;
            };
            let recipient = RecipientKey::agent(inner.workspace_id, route.instance_id, pane_id);
            match selected.entry(recipient) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(MailboxPaneChoice {
                        attached: route.attached,
                        pane: Some((route.session_idx, route.instance_id, pane)),
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let selected = entry.get_mut();
                    if route.attached && !selected.attached {
                        selected.attached = true;
                        selected.pane = Some((route.session_idx, route.instance_id, pane));
                    } else if route.attached == selected.attached
                        && !selected
                            .pane
                            .as_ref()
                            .is_some_and(|(_, instance_id, existing)| {
                                *instance_id == route.instance_id && existing.root == pane.root
                            })
                    {
                        selected.pane = None;
                    }
                }
            }
        }
    }
    selected
        .into_values()
        .filter_map(|selected| selected.pane)
        .collect()
}

fn replace_mailbox_directory_unlocked(
    inner: &Inner,
    proposed: Option<&MailboxRouteOverride<'_>>,
) -> Result<(), String> {
    let Some(service) = inner.mailbox.as_ref() else {
        return Ok(());
    };
    let panes = mailbox_panes(inner, proposed);
    #[cfg(test)]
    let pause = inner
        .mailbox_publish_pause
        .lock()
        .expect("mailbox publish pause lock")
        .take();
    #[cfg(test)]
    if let Some(pause) = pause {
        pause.entered.send(()).expect("publish pause receiver");
        pause.release.wait();
    }
    let mut agents = Vec::new();
    for (_, session_instance_id, pane) in panes {
        let row = &pane.row;
        let pane_id: TmuxPaneId = row
            .pane_id
            .parse()
            .map_err(|error| format!("invalid pane id {}: {error}", row.pane_id))?;
        let key = RecipientKey::agent(inner.workspace_id, session_instance_id, pane_id);
        let Some(root) = pane.root else {
            continue;
        };
        let Ok(pane_root) = ProcessInstanceId::new(root.pid, root.birth) else {
            continue;
        };
        let label = inner
            .registry
            .lock()
            .expect("registry lock")
            .for_route(key, pane_root)
            .map(|adoption| adoption.label.clone());
        let Some(label) = label else {
            continue;
        };
        agents.push(mailbox::MailboxIdentity { key, label });
    }
    let directory = mailbox::MailboxDirectory::new(inner.workspace_id, agents)
        .map_err(|error| error.to_string())?;
    service
        .replace_directory(directory)
        .map_err(|error| error.to_string())
}

fn publish_mailbox_transition(
    inner: &Inner,
    proposed: &MailboxRouteOverride<'_>,
    commit: impl FnOnce(),
) -> Result<(), String> {
    let _publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    replace_mailbox_directory_unlocked(inner, Some(proposed))?;
    commit();
    Ok(())
}

fn refresh_mailbox_directory_unlocked(inner: &Inner) -> bool {
    if let Err(error) = replace_mailbox_directory_unlocked(inner, None) {
        error!(error = %error, "cannot refresh mailbox directory");
        let Some(service) = inner.mailbox.as_ref() else {
            return false;
        };
        let empty = mailbox::MailboxDirectory::new(
            inner.workspace_id,
            std::iter::empty::<mailbox::MailboxIdentity>(),
        )
        .expect("an empty directory is valid");
        if let Err(clear_error) = service.replace_directory(empty) {
            error!(error = %clear_error, "cannot close stale mailbox directory");
        }
        return false;
    }
    true
}

fn refresh_mailbox_directory(inner: &Inner) -> bool {
    let _publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    refresh_mailbox_directory_unlocked(inner)
}

fn update_mailbox_route(inner: &Inner, update: impl FnOnce()) -> bool {
    let _publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    update();
    let published = refresh_mailbox_directory_unlocked(inner);
    if published {
        inner.emit("messages.route_changed", json!({}), None);
    }
    published
}

fn refresh_mailbox_and_schedule(inner: &Arc<Inner>) {
    if refresh_mailbox_directory(inner) {
        messaging::schedule_available(inner);
    }
}

/// Resolve a pane origin to its current durable key.
///
/// The captured root generation must match the selected pane route. A reused
/// pane id or numeric PID therefore refuses the previous session's mailbox.
#[cfg(test)]
pub(crate) fn mailbox_recipient_for_origin(
    inner: &Inner,
    pane_id: TmuxPaneId,
    pane_root: identity::ProcId,
) -> Option<RecipientKey> {
    let pane_text = pane_id.to_string();
    let mut matches = mailbox_panes(inner, None)
        .into_iter()
        .filter(move |(_, _, pane)| pane.row.pane_id == pane_text)
        .filter_map(move |(_, session_instance_id, pane)| {
            if pane.root != Some(pane_root) {
                return None;
            }
            let recipient = RecipientKey::agent(inner.workspace_id, session_instance_id, pane_id);
            let root = ProcessInstanceId::new(pane_root.pid, pane_root.birth).ok()?;
            inner
                .registry
                .lock()
                .expect("registry lock")
                .for_route(recipient, root)
                .map(|_| recipient)
        });
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

async fn observe_session_identity(
    inner: &Inner,
    watcher: &SessionWatcher,
) -> Result<SessionIdentityBinding, String> {
    let session_id: TmuxSessionId = watcher
        .session_id()
        .ok_or_else(|| "tmux session id is unavailable".to_string())?
        .parse()
        .map_err(|error| format!("invalid tmux session id: {error}"))?;
    let live_key =
        livesession::observe_watched(&watcher.client(), session_id, inner.workspace_id, |pid| {
            identity::ProcId::of(pid).map(|process| process.birth)
        })
        .await
        .map_err(|error| error.to_string())?;
    let instance_id = inner
        .session_identities
        .lock()
        .map_err(|_| "session identity lock is poisoned".to_string())?
        .resolve(&inner.state_root, &live_key, || {
            SessionInstanceId::from_uuid(uuid::Uuid::new_v4()).expect("non-nil UUID")
        })
        .map_err(|error| error.to_string())?;
    Ok(SessionIdentityBinding::new(live_key, instance_id))
}

/// A booted daemon. Dropping it does not stop the tasks; call
/// [`Daemon::shutdown`] for a clean exit (detach watchers, remove socket).
pub struct Daemon {
    inner: Arc<Inner>,
    stop: watch::Sender<bool>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    unread_projection_task: StdMutex<Option<JoinHandle<()>>>,
    socket_cleanup: StdMutex<Option<cyclops_state::BoundSocketCleanup>>,
}

/// Test-only coordination at the exact stale-projection boundary.
///
/// It is always compiled because integration tests exercise the public daemon
/// type as a dependency. Production never arms it, so the hot path performs
/// only one uncontended `None` check.
#[doc(hidden)]
pub struct UnreadProjectionTestPause {
    derived: Notify,
    release: Notify,
}

impl UnreadProjectionTestPause {
    #[doc(hidden)]
    pub async fn wait_until_derived(&self) {
        self.derived.notified().await;
    }

    #[doc(hidden)]
    pub fn release(&self) {
        self.release.notify_one();
    }
}

impl Daemon {
    /// Path of the Unix socket this daemon serves.
    pub fn socket_path(&self) -> PathBuf {
        self.inner.cfg.home.join(cyclops_proto::SOCK_NAME)
    }

    /// Wait until an authenticated client asks this exact daemon to stop.
    pub async fn shutdown_requested(&self) {
        let mut request = self.inner.shutdown_request.subscribe();
        if *request.borrow() {
            return;
        }
        let _ = request.changed().await;
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
        // Close worker creation before any task handle is drained. A delivery
        // settling during shutdown may try to schedule the next mailbox item;
        // the latch turns that into a durable pending item instead of an
        // untracked task that outlives shutdown.
        self.inner.engine.begin_stopping();
        self.inner
            .unread_projection_stopping
            .store(true, Ordering::Release);
        self.inner
            .unread_projection_pending
            .lock()
            .expect("unread projection pending lock")
            .clear();
        self.inner.unread_projection_wake.notify_one();
        // Wait for a registration that crossed the stopping edge to finish.
        // Later session.watch calls acquire this lock, observe stopping, and
        // refuse before opening a ledger or publishing a task.
        {
            let _registration = self
                .inner
                .session_registration
                .lock()
                .expect("session registration lock");
        }
        let descendants_stopped = tokio::time::timeout(
            SHUTDOWN_GRACE,
            self.inner.engine.wait_for_descendant_tasks(),
        )
        .await
        .is_ok();
        // The unread worker owns optional tmux chrome writes outside delivery
        // supervision. Join or cancel it before restoring the user's chrome,
        // otherwise a late badge write can repaint Cyclops after shutdown.
        let unread_projection_task = self
            .unread_projection_task
            .lock()
            .expect("unread projection task lock")
            .take();
        if let Some(mut task) = unread_projection_task {
            if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        if descendants_stopped {
            restore_all_chrome(&self.inner).await;
        } else {
            warn!("descendant tasks exceeded shutdown grace; skipping chrome restore");
        }
        let _ = self.stop.send(true);
        let mut tasks: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.tasks.lock().expect("tasks lock"));
        // Tasks spawned after boot (watch_session's session_task) shut down
        // exactly like the ones boot spawned: same stop signal, same
        // grace-then-abort below.
        tasks.extend(std::mem::take(
            &mut *self.inner.extra_tasks.lock().expect("extra tasks lock"),
        ));
        tasks.extend(fusion::take_lifecycle_recheck_tasks(&self.inner));
        for mut task in tasks {
            if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        let mut workers = self.inner.engine.take_legacy_worker_tasks();
        workers.extend(self.inner.engine.take_notification_worker_tasks());
        for worker in &workers {
            worker.abort();
        }
        for mut worker in workers {
            if tokio::time::timeout(SHUTDOWN_GRACE, &mut worker)
                .await
                .is_err()
            {
                worker.abort();
                let _ = worker.await;
            }
        }
        if tokio::time::timeout(
            SHUTDOWN_GRACE,
            self.inner.engine.wait_for_descendant_tasks(),
        )
        .await
        .is_err()
        {
            warn!("descendant tasks exceeded shutdown grace; sealing journals");
        }
        if let Some(mailbox) = &self.inner.mailbox {
            if let Err(error) = mailbox.seal() {
                error!(%error, "cannot seal workspace journal during shutdown");
            }
        }
        for slot in self.inner.session_slots() {
            if let Err(error) = slot.ledger.seal() {
                error!(session = %slot.name(), %error, "cannot seal session journal during shutdown");
            }
        }
        if let Some(cleanup) = self
            .socket_cleanup
            .lock()
            .expect("socket cleanup lock")
            .take()
        {
            let _ = cleanup.remove();
        }
        info!("cyclopsd stopped");
    }

    /// Subscribe to the daemon event stream (msg, messages.changed,
    /// delivery-state, gate, admin-notify, state, session). Same stream
    /// events.subscribe serves.
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

    /// In-process workspace message send with an already-resolved sender.
    /// The socket path resolves the sender from peer credentials first.
    pub async fn msg_send(&self, from: &str, params: MsgSendParams) -> Result<Value, WireError> {
        if params.wait.is_some() {
            return Err(WireError {
                code: "notification_unavailable".to_string(),
                message: "send wait is not supported for mailbox notifications".to_string(),
                data: None,
            });
        }
        if params.reply_to.is_some() && (params.fyi || params.supersedes.is_some()) {
            return Err(WireError {
                code: "bad_request".to_string(),
                message: "a reply cannot be an announcement or supersede another message"
                    .to_string(),
                data: None,
            });
        }
        let service = self.inner.mailbox.as_ref().ok_or_else(|| WireError {
            code: "mailbox_unavailable".to_string(),
            message: "durable workspace identity is not connected".to_string(),
            data: None,
        })?;
        let sender = service
            .identity_for_address(from)
            .map_err(server::mailbox_service_error)?;
        let result = messaging::send(&self.inner, service, sender, params)
            .await
            .map_err(server::mailbox_service_error)?;
        serde_json::to_value(result).map_err(|error| WireError {
            code: "internal".to_string(),
            message: error.to_string(),
            data: None,
        })
    }

    /// Test seam for proving mailbox acceptance does not wait on tmux chrome.
    ///
    /// Socket authentication and unread rendering have separate coverage.
    #[doc(hidden)]
    pub async fn hold_unread_projection_for_test(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.unread_projection_gate.lock().await
    }

    /// Number of coalesced recipient keys awaiting an unread projection.
    ///
    /// This exposes queue cardinality, never message content. Tests use it to
    /// prove a burst creates one bounded dirty key and that a fact arriving
    /// behind an in-flight tmux write is not dropped.
    #[doc(hidden)]
    pub fn pending_unread_projection_count_for_test(&self) -> usize {
        self.inner
            .unread_projection_pending
            .lock()
            .expect("unread projection pending lock")
            .len()
    }

    /// Pause the next unread projection after it derives the durable count but
    /// before tmux receives that value.
    #[doc(hidden)]
    pub fn pause_next_unread_projection_for_test(&self) -> Arc<UnreadProjectionTestPause> {
        let pause = Arc::new(UnreadProjectionTestPause {
            derived: Notify::new(),
            release: Notify::new(),
        });
        let replaced = self
            .inner
            .unread_projection_pause
            .lock()
            .expect("unread projection pause lock")
            .replace(Arc::clone(&pause));
        assert!(
            replaced.is_none(),
            "an unread projection pause is already armed"
        );
        pause
    }

    /// Test seam for an exact mailbox claim while a delivery worker is paused.
    ///
    /// Sender authentication has separate socket and process-tree coverage.
    #[doc(hidden)]
    pub fn claim_message_for_test(
        &self,
        claimant: &str,
        message_id: &str,
    ) -> Result<(), WireError> {
        let service = self.inner.mailbox.as_ref().ok_or_else(|| WireError {
            code: "mailbox_unavailable".to_string(),
            message: "durable workspace identity is not connected".to_string(),
            data: None,
        })?;
        let claimant = service
            .identity_for_address(claimant)
            .map_err(server::mailbox_service_error)?;
        let message_id = MessageId::new(message_id).map_err(|error| WireError {
            code: "bad_request".to_string(),
            message: error.to_string(),
            data: None,
        })?;
        messaging::claim(&self.inner, service, claimant.key, message_id)
            .map_err(server::mailbox_service_error)?;
        Ok(())
    }

    /// Test seam for a body-free mailbox snapshot with an already-resolved
    /// caller.
    ///
    /// Socket authentication has separate process-tree coverage. Integration
    /// rigs run from whichever shell starts `cargo test`; if that shell name
    /// happens to match the fixture manifest, its socket peer has no unique
    /// mailbox identity. Delivery tests use this seam for the same reason
    /// they use [`Daemon::msg_send`] and [`Daemon::claim_message_for_test`].
    #[doc(hidden)]
    pub fn messages_snapshot_for_test(
        &self,
        caller: &str,
        recent_settled: u32,
    ) -> Result<MessagesSnapshotResult, WireError> {
        let service = self.inner.mailbox.as_ref().ok_or_else(|| WireError {
            code: "mailbox_unavailable".to_string(),
            message: "durable workspace identity is not connected".to_string(),
            data: None,
        })?;
        let caller = service
            .identity_for_address(caller)
            .map_err(server::mailbox_service_error)?;
        service
            .messages_snapshot(caller.key, recent_settled)
            .map_err(server::mailbox_service_error)
    }

    /// Test seam for an exact operator withdrawal while a delivery worker is
    /// paused.
    ///
    /// Socket authorization has separate coverage. Delivery fixtures can run
    /// beneath a shell that matches multiple manifest candidates, leaving its
    /// peer credential without one mailbox identity.
    #[doc(hidden)]
    pub fn withdraw_notification_for_test(
        &self,
        operator: &str,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationWithdrawResult, WireError> {
        let service = self.inner.mailbox.as_ref().ok_or_else(|| WireError {
            code: "mailbox_unavailable".to_string(),
            message: "durable workspace identity is not connected".to_string(),
            data: None,
        })?;
        let operator = service
            .identity_for_address(operator)
            .map_err(server::mailbox_service_error)?;
        if !operator.key.is_admin() {
            return Err(WireError {
                code: "denied".to_string(),
                message: "this operation requires the workspace administrator".to_string(),
                data: None,
            });
        }
        messaging::withdraw_notification(&self.inner, service, operator.key, recipient, attempt_id)
            .map_err(server::mailbox_service_error)
    }

    /// Legacy in-process delivery seam used by transport tests and embedders.
    ///
    /// This bypasses the durable mailbox contract. Its optional composed
    /// wait is an occupant-pinned pane-state heuristic, not proof that a
    /// specific message or task completed.
    pub async fn deliver_payload(
        &self,
        from: &str,
        params: MsgSendParams,
    ) -> Result<Value, WireError> {
        delivery::msg_send(&self.inner, from, params).await
    }

    /// In-process agent.state.report with a pre-trusted origin, mirroring
    /// [`Daemon::msg_send`]'s design: embedders and tests call this
    /// directly. The SOCKET path instead pins every report to the
    /// reporting pane via peer credentials and denies everything else,
    /// because hook reports are liveness and ACK evidence.
    pub async fn report_state(&self, params: StateReportParams) -> Result<Value, WireError> {
        // The in-process path is pre-trusted, so it states the origin the
        // socket path would have derived: the named pane, as it stands
        // right now. A caller that names nothing cannot be placed.
        let Some(name) = params.agent.clone() else {
            return Err(WireError {
                code: "denied".to_string(),
                message: "an in-process report must name the pane it speaks for".to_string(),
                data: None,
            });
        };
        let (session_idx, pane_id, pane_root, agent, manifest) =
            match self.inner.resolve_recipient(&name) {
                Some((idx, pane_id)) => {
                    let row = self.inner.watcher_of(idx).and_then(|w| w.pane(&pane_id));
                    // Same domain the socket path derives and the ACK
                    // check re-derives: the agent instance proven from the
                    // process tree.
                    let admitted = row
                        .as_ref()
                        .and_then(|r| fusion::admitted_vendor(&self.inner, idx, r));
                    let pane_root = row.as_ref().and_then(|r| identity::ProcId::of(r.pane_pid));
                    let agent = admitted.as_ref().map(|(_, proc)| *proc);
                    let manifest = admitted.map(|(m, _)| m.agent.id.clone());
                    (idx, pane_id, pane_root, agent, manifest)
                }
                // Detached, so the live table cannot place the pane. The
                // last-known row can, and the socket path already derives
                // origins from it during an outage. Dropping to a zero pid
                // here instead refused every hook that fired inside the one
                // window this whole path exists to cover.
                None => match self.inner.resolve_recipient_last_known(&name) {
                    Some((idx, row)) => {
                        let admitted = fusion::admitted_vendor(&self.inner, idx, &row);
                        let pane_root = identity::ProcId::of(row.pane_pid);
                        let agent = admitted.as_ref().map(|(_, proc)| *proc);
                        let manifest = admitted.map(|(m, _)| m.agent.id.clone());
                        (idx, row.pane_id.clone(), pane_root, agent, manifest)
                    }
                    None => (usize::MAX, name.clone(), None, None, None),
                },
            };
        // An origin the daemon could not place names no agent, and a
        // report about nobody is refused downstream rather than guessed
        // at here.
        let Some(agent) = agent else {
            return Err(WireError {
                code: "denied".to_string(),
                message: "this pane's agent could not be identified; a hook report has to \
                          name a process"
                    .to_string(),
                data: None,
            });
        };
        let Some(pane_root) = pane_root else {
            return Err(WireError {
                code: "denied".to_string(),
                message: "this pane's root process could not be identified".to_string(),
                data: None,
            });
        };
        let Some(recipient_key) = self.inner.recipient_key(session_idx, &pane_id) else {
            return Err(WireError {
                code: "denied".to_string(),
                message: "this pane has no durable session identity".to_string(),
                data: None,
            });
        };
        let origin = server::ReportOrigin {
            recipient: name,
            pane_id,
            session_idx,
            recipient_key,
            pane_root,
            agent,
            manifest,
        };
        ack::handle_report(&self.inner, params, origin).await
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

    /// Test-only seam: pause a terminal mutation at a named phase so an
    /// integration test can move the pane or stop the daemon at a precise
    /// boundary. Not part of the public API surface.
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

    /// Clear test-only injection pause hook.
    #[doc(hidden)]
    pub fn clear_inject_pause(&self) {
        *self.inner.inject_pause.lock().expect("inject pause lock") = None;
    }

    /// Test-only seam: force a pane-name fallback and pause it before it
    /// reconciles one watched session. Not part of the public API surface.
    #[doc(hidden)]
    pub fn set_name_reconcile_pause<F>(&self, f: F)
    where
        F: Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        *self
            .inner
            .name_reconcile_pause
            .lock()
            .expect("name reconcile pause lock") = Some(Arc::new(f));
    }

    /// Clear the test-only pane-name reconcile pause hook.
    #[doc(hidden)]
    pub fn clear_name_reconcile_pause(&self) {
        *self
            .inner
            .name_reconcile_pause
            .lock()
            .expect("name reconcile pause lock") = None;
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

    /// Test-only seam: fail the next OS binding observation at the exact
    /// terminal write boundary. The one-shot models a real process-table
    /// observation failure without replacing the authenticated pane process.
    #[doc(hidden)]
    pub fn fail_next_final_binding_observation(&self) {
        self.inner
            .fail_next_final_binding_observation
            .store(true, Ordering::SeqCst);
    }

    /// Test-only seam: fail the workspace journal append during recovery for one exact attempt.
    #[doc(hidden)]
    pub fn fail_notification_recovery_append(&self, attempt_id: NotificationAttemptId) {
        if let Some(service) = self.inner.mailbox.as_ref() {
            service.inject_notification_recovery_append_failure(attempt_id);
        }
    }

    /// Test seam: inspect exact in-flight job owned by a mailbox notification worker
    /// under the queue mutex boundary.
    #[doc(hidden)]
    pub fn mailbox_worker_current_for_test(
        &self,
        recipient_label: &str,
    ) -> Option<(String, Option<NotificationAttemptId>)> {
        let service = self.inner.mailbox.as_ref()?;
        let id = service.identity_for_address(recipient_label).ok()?;
        self.inner.engine.mailbox_worker_current_for_test(id.key)
    }

    /// Test seam: inspect exact in-flight job owned by a legacy worker
    /// under the queue mutex boundary.
    #[doc(hidden)]
    pub fn legacy_worker_current_for_test(
        &self,
        session_idx: usize,
        pane_id: &str,
    ) -> Option<String> {
        let key = PaneKey::new(session_idx, pane_id);
        self.inner.engine.legacy_worker_current_for_test(&key)
    }

    /// Test seam: inspect composer hold and owner for a pane.
    #[doc(hidden)]
    pub fn composer_hold_for_test(
        &self,
        session_idx: usize,
        pane_id: &str,
    ) -> Option<(cyclops_proto::ComposerHold, Option<String>)> {
        let detections = self.inner.detections.lock().expect("detections lock");
        let entry = detections.get(&PaneKey::new(session_idx, pane_id))?;
        Some((entry.hold, entry.hold_owner.clone()))
    }

    /// Test-only seam: panic at the synchronous on_write boundary before record_writing for the specified attempt.
    #[doc(hidden)]
    pub fn fail_pre_record_writing_for_attempt(&self, attempt: NotificationAttemptId) {
        *self.inner.fail_pre_record_writing.lock().unwrap() = Some(attempt);
    }

    /// Test-only seam: read the current armed fail_pre_record_writing target attempt.
    #[doc(hidden)]
    pub fn fail_pre_record_writing_target_for_test(&self) -> Option<NotificationAttemptId> {
        *self.inner.fail_pre_record_writing.lock().unwrap()
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
    // 1. Resolve. A pane can be real before the structural notification's
    //    debounce updates the watcher table, and a lost hint can leave that
    //    table stale. Naming is an explicit request about current tmux state,
    //    so a raw pane-id cache miss earns one authoritative owner lookup and
    //    one reconcile of that session alone, under one wall-clock bound.
    let forced_test_reconcile = target.parse::<TmuxPaneId>().is_ok()
        && inner
            .name_reconcile_pause
            .lock()
            .expect("name reconcile pause lock")
            .is_some();
    let mut resolved = if forced_test_reconcile {
        None
    } else {
        inner.resolve_recipient(target)
    };
    if resolved.is_none() && target.parse::<TmuxPaneId>().is_ok() {
        match tokio::time::timeout(
            NAME_RECONCILE_TIMEOUT,
            reconcile_raw_pane_for_naming(inner, target),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                debug!(%error, pane_id = target, "cannot refresh pane table for naming");
            }
            Err(_) => {
                debug!(pane_id = target, "pane naming reconcile timed out");
            }
        }
        resolved = inner.resolve_recipient(target);
    }
    let Some((session_idx, pane_id)) = resolved else {
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
    let watcher = inner.watcher_of(session_idx);
    let route = watcher
        .as_ref()
        .and_then(|watcher| adoption_route(inner, watcher, session_idx, &pane_id));

    // 2. Validate the label. The rule and its wording are
    //    cyclops_proto::label, so every surface refuses the same names
    //    with the same sentence.
    if let Some(l) = &label {
        if let Some(why) = cyclops_proto::label::refusal(l) {
            return Err(bad_request(why));
        }
        let Some((recipient, _, _)) = route.as_ref() else {
            return Err(WireError {
                code: "no_such_target".to_string(),
                message: format!("target {target:?} changed during adoption"),
                data: None,
            });
        };
        let holder = conflicting_label_holder(inner, l, *recipient);
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
            .get(&PaneKey::new(session_idx, &pane_id))
            .and_then(|e| e.manifest.clone())),
    }))
}

/// Resolve one raw tmux pane id to its stable owner and refresh only that
/// session. Every lock below protects a short clone/read and is dropped
/// before the optional test pause or watcher reconcile awaits.
async fn reconcile_raw_pane_for_naming(inner: &Arc<Inner>, target: &str) -> Result<(), TmuxError> {
    let Some(session_id) = pane_session_id(
        target,
        inner.cfg.tmux_socket.as_deref(),
        inner.cfg.tmux_config.as_deref(),
    )
    .await?
    else {
        return Ok(());
    };
    let watcher = inner
        .active_session_slots()
        .into_iter()
        .filter_map(|(_, slot)| {
            slot.link
                .lock()
                .expect("session link lock")
                .watcher
                .as_ref()
                .map(Arc::clone)
        })
        .find(|watcher| watcher.session_id() == Some(session_id.as_str()));
    let Some(watcher) = watcher else {
        return Ok(());
    };
    let pause = inner
        .name_reconcile_pause
        .lock()
        .expect("name reconcile pause lock")
        .clone();
    if let Some(pause) = pause {
        pause(watcher.session()).await;
    }
    watcher.reconcile_now().await
}

/// Why a name cannot be claimed: who wears it now, where, and the way
/// out. "already taken" alone once had an operator staring at an empty
/// roster and a refused name at the same time, with nothing to act on.
fn label_taken_words(inner: &Inner, label: &str, holder: &registry::Adoption) -> String {
    let session_idx = inner.session_index(&holder.session).or_else(|| {
        let instance_id = holder.recipient?.session_instance_id()?;
        inner
            .active_session_slots()
            .into_iter()
            .find_map(|(idx, slot)| {
                slot.link
                    .lock()
                    .expect("session link lock")
                    .identity
                    .as_ref()
                    .map(|identity| identity.session_instance_id())
                    .is_some_and(|candidate| candidate == instance_id)
                    .then_some(idx)
            })
    });
    let attached = session_idx
        .and_then(|idx| inner.session(idx))
        .map(|slot| slot.link.lock().expect("session link lock").attached)
        .unwrap_or(false);
    if attached {
        format!(
            "label {label:?} is already taken by {pane} in session {session} ({state}). \
             Free it with: cyclops name {pane} --clear, or pick another name.",
            pane = holder.pane_id,
            session = holder.session,
            state = cyclops_proto::state_words(
                session_idx
                    .map(|idx| inner.cached_state(idx, &holder.pane_id))
                    .unwrap_or(AgentState::Unknown)
            ),
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

fn conflicting_label_holder(
    inner: &Inner,
    label: &str,
    recipient: RecipientKey,
) -> Option<registry::Adoption> {
    inner
        .registry
        .lock()
        .expect("registry lock")
        .for_label(label)
        .filter(|holder| holder.recipient != Some(recipient))
        .cloned()
}

/// Persist one adoption while the caller owns `mailbox_publication`.
///
/// The earlier label check gives a fast refusal. This check is authoritative:
/// two concurrent calls may both pass the earlier check, but only one may
/// mutate the registry and publish the mailbox directory.
fn commit_adoption_under_publication(
    inner: &Inner,
    adoption: registry::Adoption,
    window: registry::WindowChrome,
) -> Result<(), WireError> {
    let recipient = adoption
        .recipient
        .expect("new adoptions carry an exact recipient");
    if let Some(holder) = conflicting_label_holder(inner, &adoption.label, recipient) {
        return Err(bad_request(label_taken_words(
            inner,
            &adoption.label,
            &holder,
        )));
    }
    if let Err(error) = inner
        .registry
        .lock()
        .expect("registry lock")
        .adopt(adoption, window)
    {
        return Err(WireError {
            code: "internal".to_string(),
            message: format!("cannot record the adoption: {error}"),
            data: None,
        });
    }
    if !refresh_mailbox_directory_unlocked(inner) {
        return Err(WireError {
            code: "internal".to_string(),
            message: "the name was recorded but its mailbox route could not be published"
                .to_string(),
            data: None,
        });
    }
    Ok(())
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
fn adoption_route(
    inner: &Inner,
    watcher: &Arc<SessionWatcher>,
    session_idx: usize,
    pane_id: &str,
) -> Option<(RecipientKey, ProcessInstanceId, PaneRow)> {
    let slot = inner.session(session_idx)?;
    let session_instance_id = {
        let link = slot.link.lock().expect("session link lock");
        let current = link.watcher.as_ref()?;
        if !link.attached || !Arc::ptr_eq(current, watcher) {
            return None;
        }
        link.identity.as_ref()?.session_instance_id()
    };
    let row = watcher.pane(pane_id)?;
    let pane: TmuxPaneId = pane_id.parse().ok()?;
    let root = identity::ProcId::of(row.pane_pid)?;
    let pane_root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
    Some((
        RecipientKey::agent(inner.workspace_id, session_instance_id, pane),
        pane_root,
        row,
    ))
}

async fn adopt_pane(
    inner: &Arc<Inner>,
    watcher: Option<&Arc<SessionWatcher>>,
    session_idx: usize,
    target: &str,
    pane_id: &str,
    label: &str,
    manifest: Option<&str>,
) -> Result<(), WireError> {
    let Some(watcher) = watcher else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("no such target {target:?}"),
            data: None,
        });
    };
    let Some((recipient, pane_root, row)) = adoption_route(inner, watcher, session_idx, pane_id)
    else {
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
        match reg.for_route(recipient, pane_root) {
            Some(adoption) => (
                Some(adoption.border_format.clone()),
                reg.window(&row.window_id)
                    .map(|window| window.border_status.clone()),
            ),
            None => (None, None),
        }
    };
    let read = match known_format.is_none() || known_status.is_none() {
        true => match chrome::snapshot(&watcher.client(), pane_id, &row.window_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!(pane = %pane_id, error = %e, "cannot read pane chrome; adopting without it");
                chrome::Snapshot::none()
            }
        },
        false => chrome::Snapshot::none(),
    };
    let publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    let Some((current_recipient, current_root, current_row)) =
        adoption_route(inner, watcher, session_idx, pane_id)
    else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("target {target:?} changed during adoption"),
            data: None,
        });
    };
    if current_recipient != recipient
        || current_root != pane_root
        || current_row.window_id != row.window_id
    {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("target {target:?} changed during adoption"),
            data: None,
        });
    }
    // 2. Write the registry.
    let session = inner
        .session(session_idx)
        .expect("session_idx valid: caller resolved it")
        .name();
    let adoption = registry::Adoption {
        session: session.clone(),
        pane_id: pane_id.to_string(),
        label: label.to_string(),
        recipient: Some(recipient),
        pane_root: Some(pane_root),
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
    commit_adoption_under_publication(inner, adoption, window)?;
    drop(publication);
    // 3. Paint.
    paint_chrome(inner, session_idx, pane_id).await;
    // 4. Re-read.
    let route_evidence = inner.advance_route_evidence(session_idx, pane_id);
    fusion::recompute_pane_for_route_evidence(
        inner,
        session_idx,
        watcher,
        pane_id,
        false,
        "pane_labeled",
        &route_evidence,
    )
    .await;
    messaging::schedule_route_evidence(inner, session_idx, pane_id, &route_evidence);
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
    // A best-effort unread write may still be in flight after msg.send has
    // returned. Clear owns the final chrome write, so order it after that
    // projection and keep later projections from observing the adoption
    // until its registry entry is gone.
    let _unread_projection = inner.unread_projection_gate.lock().await;
    let route = watcher.and_then(|watcher| adoption_route(inner, watcher, session_idx, pane_id));
    let Some((recipient, pane_root, _)) = route else {
        return Ok(());
    };
    // 1. Look, do not commit.
    let pending = inner
        .registry
        .lock()
        .expect("registry lock")
        .pending_clear(recipient, pane_root);
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
    let publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    let cleared = inner
        .registry
        .lock()
        .expect("registry lock")
        .clear(recipient, pane_root)
        .map_err(|e| WireError {
            code: "internal".to_string(),
            message: format!("cannot record the change: {e}"),
            data: None,
        })?;
    if cleared.is_none() {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("target {pane_id:?} changed during clear"),
            data: None,
        });
    }
    if !refresh_mailbox_directory_unlocked(inner) {
        return Err(WireError {
            code: "internal".to_string(),
            message: "the name was cleared but its mailbox route could not be republished"
                .to_string(),
            data: None,
        });
    }
    drop(publication);
    messaging::schedule_available(inner);
    // 5. Re-read.
    if let Some(w) = watcher {
        let route_evidence = inner.advance_route_evidence(session_idx, pane_id);
        fusion::recompute_pane_for_route_evidence(
            inner,
            session_idx,
            w,
            pane_id,
            false,
            "pane_unlabeled",
            &route_evidence,
        )
        .await;
        messaging::schedule_route_evidence(inner, session_idx, pane_id, &route_evidence);
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
    for (idx, slot) in inner.active_session_slots() {
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
    session_idx: usize,
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
        let state = inner.cached_state(session_idx, &a.pane_id);
        let unread = inner
            .mailbox
            .as_ref()
            .and_then(|m| {
                let recipient = a.recipient?;
                m.pending_count(recipient).ok()
            })
            .unwrap_or(0);
        if let Err(e) = chrome::apply(
            &watcher.client(),
            inner.cfg.chrome,
            &a.pane_id,
            &a.window_id,
            &a.label,
            state,
            &theme,
            unread,
        )
        .await
        {
            warn!(pane = %a.pane_id, error = %e, "cannot write pane chrome");
        }
    }
}

/// Update the @cyclops_unread option on an adopted pane.
pub(crate) async fn sync_pane_unread(inner: &Arc<Inner>, pane_id: &str) {
    let _gate = inner.unread_projection_gate.lock().await;
    sync_pane_unread_with_gate(inner, pane_id).await;
}

/// Mark one recipient's unread chrome dirty without waiting on tmux.
///
/// The pending set coalesces any number of facts for the same recipient. The
/// daemon-owned worker re-derives the authoritative count after each dirty
/// edge, so a fact committed while an older tmux write is blocked cannot be
/// lost behind that stale write.
pub(crate) fn schedule_recipient_unread(inner: &Arc<Inner>, recipient: RecipientKey) {
    if inner.unread_projection_stopping.load(Ordering::Acquire) {
        return;
    }
    let inserted = {
        let mut pending = inner
            .unread_projection_pending
            .lock()
            .expect("unread projection pending lock");
        if inner.unread_projection_stopping.load(Ordering::Acquire) {
            return;
        }
        pending.insert(recipient)
    };
    if inserted {
        inner.unread_projection_wake.notify_one();
    }
}

/// Drain unread projections until shutdown. The set is checked before each
/// wait so a notify racing the wait is retained as either a set entry or a
/// Notify permit.
async fn unread_projection_task(inner: Arc<Inner>) {
    loop {
        if inner.unread_projection_stopping.load(Ordering::Acquire) {
            return;
        }
        let recipients = std::mem::take(
            &mut *inner
                .unread_projection_pending
                .lock()
                .expect("unread projection pending lock"),
        );
        if recipients.is_empty() {
            inner.unread_projection_wake.notified().await;
            continue;
        }
        for recipient in recipients {
            if inner.unread_projection_stopping.load(Ordering::Acquire) {
                return;
            }
            sync_recipient_unread(&inner, recipient).await;
        }
    }
}

/// Derive and paint one unread count while the caller owns the projection
/// gate. Keeping the tmux work here makes the blocking and best-effort entry
/// points share one authoritative projection read.
async fn sync_pane_unread_with_gate(inner: &Arc<Inner>, pane_id: &str) {
    let adoptions = inner
        .registry
        .lock()
        .expect("registry lock")
        .exact_adoptions();
    let Some(adoption) = adoptions.into_iter().find(|a| a.pane_id == pane_id) else {
        return;
    };
    let Some(session_idx) = inner
        .active_session_slots()
        .into_iter()
        .find_map(|(idx, slot)| {
            let link = slot.link.lock().expect("session link lock");
            let watcher = link.watcher.as_ref()?;
            let snapshot = watcher.snapshot();
            snapshot.iter().any(|r| r.pane_id == pane_id).then_some(idx)
        })
    else {
        return;
    };
    let Some(watcher) = inner.watcher_of(session_idx) else {
        return;
    };
    let unread = inner
        .mailbox
        .as_ref()
        .and_then(|m| {
            let recipient = adoption.recipient?;
            m.pending_count(recipient).ok()
        })
        .unwrap_or(0);
    let pause = inner
        .unread_projection_pause
        .lock()
        .expect("unread projection pause lock")
        .take();
    if let Some(pause) = pause {
        pause.derived.notify_one();
        pause.release.notified().await;
    }
    if let Err(e) =
        chrome::update_unread(&watcher.client(), inner.cfg.chrome, pane_id, unread).await
    {
        warn!(pane = %pane_id, error = %e, "cannot update pane unread option");
    }
}

/// Update the @cyclops_unread option for a recipient key.
pub(crate) async fn sync_recipient_unread(
    inner: &Arc<Inner>,
    recipient: cyclops_proto::RecipientKey,
) {
    if let Some(pane_id) = recipient.pane_id() {
        sync_pane_unread(inner, &pane_id.to_string()).await;
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
    let Some(adoption) = inner.adoption_for_route(session_idx, pane_id) else {
        return;
    };
    paint_adoptions(
        inner,
        session_idx,
        &watcher,
        std::slice::from_ref(&adoption),
    )
    .await;
}

/// Repaint the state half of an adopted pane's border. Called from the one
/// place a fused state change is recorded
/// (fusion::recompute_pane_with_evidence), so a border can never disagree
/// with the row `cyclops list` prints.
pub(crate) async fn repaint_chrome(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &SessionWatcher,
    pane_id: &str,
) {
    let Some(adoption) = inner.adoption_for_route(session_idx, pane_id) else {
        return;
    };
    let theme = inner.theme_now();
    let state = inner.cached_state(session_idx, pane_id);
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
        paint_adoptions(inner, idx, &watcher, &adopted).await;
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
            "state_permission_repair": {
                "directories": inner.state_repair.directories,
                "regular_files": inner.state_repair.regular_files,
                "live_socket_preserved": inner.state_repair.live_socket_preserved,
            },
        }),
    )
}

fn require_bound_socket_in_state_root(
    repair: &RepairSummary,
    state_root: &StateRoot,
) -> anyhow::Result<()> {
    if !repair.live_socket_preserved {
        anyhow::bail!(
            "bound socket is not inside the validated state root {}",
            state_root.path().display()
        );
    }
    if !state_root.path_matches_held_root()? {
        anyhow::bail!(
            "state root path changed during socket bind {}",
            state_root.path().display()
        );
    }
    Ok(())
}

/// Boot the daemon, secure its state, and spawn its watcher and server tasks.
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
                // Through tmux 3.6a there is no way to see bracketed paste
                // degradation up front, so deliveries gate on post-paste
                // composer verification instead.
                info!("bracket_paste_flag unavailable; deliveries will gate on post-paste composer verification");
            }
            v.raw
        }
        None => {
            warn!("tmux -V failed; session watchers will keep retrying");
            "unavailable".to_string()
        }
    };

    // Every session-ledger open is anchored to this validated state root.
    let state_root = Arc::new(
        StateRoot::open_or_create(&cfg.home)
            .map_err(|error| anyhow::anyhow!("open state root {}: {error}", cfg.home.display()))?,
    );
    let bound_socket = server::bind_socket(&state_root).await?;
    let repair = state_root
        .repair_descendant_permissions(Some(OsStr::new(cyclops_proto::SOCK_NAME)))
        .map_err(|error| anyhow::anyhow!("repair state root {}: {error}", cfg.home.display()))?;
    require_bound_socket_in_state_root(&repair, &state_root)?;
    info!(
        directories = repair.directories,
        regular_files = repair.regular_files,
        live_socket_preserved = repair.live_socket_preserved,
        "state permissions repaired"
    );
    let workspace_id = workspaceid::load_or_create(&state_root)
        .map_err(|error| anyhow::anyhow!("load workspace identity: {error}"))?;
    let session_identities = sessionstore::SessionIdentities::open(&state_root)
        .map_err(|error| anyhow::anyhow!("load session identities: {error}"))?;
    let message_journal = PathBuf::from("workspaces")
        .join(workspace_id.to_string())
        .join("messages.ndjson");
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let mut message_store =
        mailbox::MessageStore::open(&state_root, &message_journal, workspace_id, &boot_id)
            .map_err(|error| anyhow::anyhow!("open workspace message journal: {error}"))?;
    let recovered_notifications = message_store
        .recover_notifications_after_restart()
        .map_err(|error| anyhow::anyhow!("recover workspace notifications: {error}"))?;
    if !recovered_notifications.is_empty() {
        info!(
            notifications = recovered_notifications.len(),
            "closed ambiguous workspace notifications after restart"
        );
    }
    let mailbox_directory = mailbox::MailboxDirectory::new(
        workspace_id,
        std::iter::empty::<mailbox::MailboxIdentity>(),
    )
    .map_err(|error| anyhow::anyhow!("open mailbox directory: {error}"))?;
    let (manifests, manifest_dir) = load_manifests(&cfg);
    let mut sessions = Vec::with_capacity(cfg.sessions.len());
    let mut replay_roots: Vec<(usize, String, Vec<LedgerLine>)> = Vec::new();
    let engine = delivery::Engine::new();
    for (idx, name) in cfg.sessions.iter().enumerate() {
        let descendant = PathBuf::from("ledger").join(format!("{name}.ndjson"));
        let ledger = LedgerWriter::open(&state_root, &descendant, &boot_id).map_err(|error| {
            anyhow::anyhow!(
                "open ledger {}: {error}",
                state_root.path().join(&descendant).display()
            )
        })?;
        // The roots and every rename-linked journal discovered from them
        // feed both id preload and restart-limbo settlement below.
        match ledger.read_after(0) {
            Ok(lines) => {
                replay_roots.push((idx, format!("{name}.ndjson"), lines));
            }
            Err(e) => warn!(session = %name, error = %e, "ledger replay for id preload failed"),
        }
        sessions.push(Arc::new(SessionSlot::new(name.clone(), Arc::new(ledger))));
    }
    let replay = history::session_journal_replay(&state_root, replay_roots);
    // This is the first Engine use after construction, so every historical
    // id is reserved before any request can mint one. Files stay separate for
    // history; restart recovery receives one descendant-first stream per root.
    for (_, lines) in &replay.files {
        engine.preload_ids(lines);
    }
    let replayed = replay.recovery;
    // Adoptions from the previous run. Nothing is trusted onto a pane
    // yet; each session prunes its own entries against the live pane
    // table when it attaches (registry::restore_session).
    let (mut adoptions, warnings) = registry::Registry::load(Arc::clone(&state_root));
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
            let removed = adoptions.in_session(&session);
            let mut gone = Vec::new();
            for adoption in &removed {
                match composer_recovery::pane_root_gone(adoption) {
                    Ok(true) => gone.extend(adoption.recipient),
                    Ok(false) => {}
                    Err(reason) => anyhow::bail!(
                        "cannot prove physical pane loss for {}: {reason}",
                        adoption.pane_id
                    ),
                }
            }
            composer_recovery::retire_gone_in_store(&mut message_store, gone.iter().copied())
                .map_err(|error| anyhow::anyhow!("retire barriers for missing session: {error}"))?;
            release_gone_recipients(&mut adoptions, gone);
        }
    }
    let recovered_barrier_ids: Vec<_> = message_store
        .projection()
        .active_notification_barriers()
        .into_iter()
        .map(|record| record.attempt_id)
        .collect();
    let mailbox = Arc::new(mailbox::MailboxService::new_with_events(
        mailbox_directory,
        message_store,
        events.clone(),
    ));
    let mut theme = cyclops_theme::ThemeWatch::new(&cfg.home);
    for w in theme.take_warnings() {
        warn!("theme: {w}");
    }

    // Created before Inner so the receiver can live on it: a session
    // watched after boot (watch_session) hands its session_task the same
    // receiver every configured session got. boot keeps the sender.
    let (stop, stop_rx) = watch::channel(false);
    let (shutdown_request, _) = watch::channel(false);
    let inner = Arc::new(Inner {
        cfg,
        state_root,
        state_repair: repair,
        workspace_id,
        session_identities: StdMutex::new(session_identities),
        mailbox: Some(mailbox),
        composer_recovery: StdMutex::new(composer_recovery::RecoveryCoordinator::new(
            recovered_barrier_ids,
        )),
        mailbox_publication: StdMutex::new(()),
        unread_projection_gate: tokio::sync::Mutex::new(()),
        unread_projection_pending: StdMutex::new(HashSet::new()),
        unread_projection_wake: Notify::new(),
        unread_projection_stopping: AtomicBool::new(false),
        unread_projection_pause: StdMutex::new(None),
        #[cfg(test)]
        mailbox_publish_pause: StdMutex::new(None),
        boot_id,
        started,
        tmux_version,
        manifests,
        manifest_dir,
        sessions: StdMutex::new(sessions),
        session_registration: StdMutex::new(()),
        events,
        detections: StdMutex::new(HashMap::new()),
        route_evidence_generations: StdMutex::new(HashMap::new()),
        pane_recomputes: StdMutex::new(HashMap::new()),
        lifecycle_rechecks: StdMutex::new(HashMap::new()),
        registry: StdMutex::new(adoptions),
        theme: StdMutex::new(theme),
        hook_readings: StdMutex::new(HashMap::new()),
        hook_lifecycle: StdMutex::new(hook_lifecycle::Store::new()),
        turn_ends: StdMutex::new(turnkey::Ends::new()),
        argv_cache: StdMutex::new(HashMap::new()),
        engine,
        ack_state: ack::AckState::new(),
        hook_liveness: selftest::HookLiveness::new(),
        inject_pause: StdMutex::new(None),
        name_reconcile_pause: StdMutex::new(None),
        fail_chrome_restore: AtomicBool::new(false),
        fail_next_final_binding_observation: AtomicBool::new(false),
        fail_pre_record_writing: StdMutex::new(None),
        workspace_ui: StdMutex::new(workspace_ui::WorkspaceUiState::default()),
        shutdown_request,
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

    // Any delivery the previous run left unresolved gets a named ending now.
    delivery::close_limbo(&inner, &replayed);
    drop(replayed);
    messaging::schedule_unclaimed_reminders(&inner);

    let mut tasks = Vec::new();
    for idx in 0..inner.session_count() {
        tasks.push(tokio::spawn(session_task(
            Arc::clone(&inner),
            idx,
            inner.stop.clone(),
        )));
    }
    let (listener, socket_cleanup) = bound_socket.into_parts();
    tasks.push(tokio::spawn(server::accept_loop(
        Arc::clone(&inner),
        listener,
        inner.stop.clone(),
    )));
    let unread_projection_task = tokio::spawn(unread_projection_task(Arc::clone(&inner)));
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
        unread_projection_task: StdMutex::new(Some(unread_projection_task)),
        socket_cleanup: StdMutex::new(Some(socket_cleanup)),
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
    let _registration = inner
        .session_registration
        .lock()
        .expect("session registration lock");
    if inner.engine.is_stopping() {
        return Err(WireError {
            code: "daemon_stopping".to_string(),
            message: "daemon is stopping; session.watch refused".to_string(),
            data: None,
        });
    }
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
        .active_session_slots()
        .into_iter()
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
        rename_session_slot_locked(inner, idx, name.to_string(), None);
        return Ok((idx, false));
    }
    let descendant = PathBuf::from("ledger").join(format!("{name}.ndjson"));
    let path = inner.state_root.path().join(&descendant);
    let ledger =
        LedgerWriter::open(&inner.state_root, &descendant, &inner.boot_id).map_err(|e| {
            WireError {
                code: "internal".to_string(),
                message: format!("open ledger {}: {e}", path.display()),
                data: None,
            }
        })?;
    // Same id-preload boot does: message ids stay unique across restarts
    // and across every session this daemon has ever watched.
    match ledger.read_after(0) {
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

/// Release only adoptions whose physical pane loss was proven.
///
/// A missing session can also mean its pane moved to another session. Those
/// adoptions retain the pinned manifest needed for composer recovery until a
/// server-wide observation proves the pane itself is gone.
fn release_gone_recipients(
    reg: &mut registry::Registry,
    recipients: impl IntoIterator<Item = RecipientKey>,
) {
    for recipient in recipients {
        let Some(adoption) = reg.for_recipient(recipient).cloned() else {
            continue;
        };
        let Some(pane_root) = adoption.pane_root else {
            continue;
        };
        reg.forget(recipient, pane_root);
        info!(
            session = %adoption.session,
            pane = %adoption.pane_id,
            label = %adoption.label,
            "released a label whose physical pane is gone"
        );
    }
}

async fn reconnect_delay(stop: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = stop.changed() => true,
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
        if let Some(canonical_idx) = slot.alias_of() {
            info!(
                alias_idx = idx,
                canonical_idx, "retired session-slot task stopped"
            );
            return;
        }
        // Re-read on every attempt, not cached once outside the loop: a
        // rename followed while attached (`handle_pane_event`'s
        // `SessionRenamed` arm) updates this slot in place, and if the
        // connection later drops for real, the reattach below must target
        // the name tmux actually calls this session now, not the name this
        // task started with.
        let name = slot.name();
        // tmux needs a pathname, but creation and cleanup stay anchored to
        // the held state root.
        let mut ccfg = ControlConfig::attach(&name)
            .with_state_buffer_spool(Arc::clone(&inner.state_root), "spool");
        if let Some(sock) = &inner.cfg.tmux_socket {
            ccfg = ccfg.on_socket(sock.clone());
        }
        if let Some(f) = &inner.cfg.tmux_config {
            ccfg = ccfg.with_config_file(f.clone());
        }
        match SessionWatcher::connect(ccfg).await {
            Ok(watcher) => {
                let watcher = Arc::new(watcher);
                if slot.alias_of().is_some() {
                    watcher.shutdown().await;
                    return;
                }
                let binding = match observe_session_identity(&inner, &watcher).await {
                    Ok(binding) => binding,
                    Err(error) => {
                        warn!(session = %name, error = %error, "cannot establish durable session identity");
                        watcher.shutdown().await;
                        if reconnect_delay(&mut stop, backoff).await {
                            return;
                        }
                        backoff = (backoff * 2).min(RECONNECT_MAX);
                        continue;
                    }
                };
                if slot.alias_of().is_some() {
                    watcher.shutdown().await;
                    return;
                }
                if !run_session(&inner, idx, &watcher, binding, stop.clone()).await {
                    watcher.shutdown().await;
                    if slot.alias_of().is_some() {
                        return;
                    }
                    if reconnect_delay(&mut stop, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                    continue;
                }
                announced_missing = false;
                backoff = RECONNECT_MIN;
                // Keep the root generations captured while each pane was
                // authoritative. A detached numeric PID must never be
                // observed again as a new process.
                update_mailbox_route(&inner, || {
                    let mut link = slot.link.lock().expect("session link lock");
                    let panes = std::mem::take(&mut link.mailbox_panes);
                    link.attached = false;
                    link.watcher = None;
                    drop(link);
                    *slot.last_panes.lock().expect("last panes lock") = panes;
                });
                messaging::schedule_available(&inner);
                session_lifecycle(&inner, idx, false);
                // Detached panes have no live sensors, so their runtime
                // verdict goes: serving a stale one would let a reader
                // treat a frame nobody can see as current.
                //
                // The composer barrier does NOT go with it. It is not a
                // sensor reading, it is what this daemon knows about a
                // composer it wrote into, and the outage is exactly when
                // that matters: a delivery can be staged, the pane can
                // detach before its receipt, and dropping the barrier
                // would let the first clean frame after reattach admit
                // the next paste on top of it. So the hold, the binding
                // it belongs to and the turn it waits on are kept, and
                // everything observational is cleared. The binding is
                // what makes that safe: the next recompute carries the
                // hold only if the same admitted agent and manifest are
                // proven again, and a replacement clears it.
                {
                    let mut det = inner.detections.lock().expect("detections lock");
                    for row in watcher.snapshot() {
                        if let Some(entry) = det.get_mut(&PaneKey::new(idx, &row.pane_id)) {
                            entry.detection.state = AgentState::Unknown;
                            entry.detection.readings.clear();
                            entry.detection.decided_by = "detached".into();
                            entry.detection.stale = true;
                            entry.detection.write_ready = false;
                            entry.detection.write_block = Some("session_detached".into());
                        }
                    }
                }
                watcher.shutdown().await;
                if slot.alias_of().is_some() {
                    return;
                }
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
                // A missing session does not prove its panes died. tmux can
                // move a pane into another session while preserving its id,
                // root process and composer. Release only roots whose loss is
                // independently proven.
                let stale = !inner
                    .registry
                    .lock()
                    .expect("registry lock")
                    .in_session(&name)
                    .is_empty();
                if stale || !announced_missing {
                    let missing = session_missing(&inner, &name).await;
                    if stale && missing {
                        let adoptions = inner
                            .registry
                            .lock()
                            .expect("registry lock")
                            .in_session(&name);
                        let mut gone = Vec::new();
                        for adoption in &adoptions {
                            match crate::composer_recovery::pane_root_gone(adoption) {
                                Ok(false) => {}
                                Ok(true) => {
                                    inner
                                        .hook_liveness
                                        .close(&PaneKey::new(idx, &adoption.pane_id));
                                    if let Some(recipient) = adoption.recipient {
                                        match crate::composer_recovery::retire_gone_recipient(
                                            &inner, recipient,
                                        ) {
                                            Ok(()) => gone.push(recipient),
                                            Err(reason) => {
                                                warn!(session = %name, %reason, "cannot retire composer barrier for missing session");
                                            }
                                        }
                                    }
                                }
                                Err(reason) => {
                                    warn!(session = %name, pane = %adoption.pane_id, %reason, "cannot prove physical pane loss for missing session");
                                }
                            }
                        }
                        if !gone.is_empty() {
                            let mut reg = inner.registry.lock().expect("registry lock");
                            release_gone_recipients(&mut reg, gone);
                        }
                    }
                    if missing {
                        update_mailbox_route(&inner, || {
                            slot.last_panes.lock().expect("last panes lock").clear();
                            let mut link = slot.link.lock().expect("session link lock");
                            link.identity = None;
                            link.mailbox_panes.clear();
                        });
                        messaging::schedule_available(&inner);
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
        if reconnect_delay(&mut stop, backoff).await {
            return;
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
/// open. It uses the same file and handle; see `SessionSlot::ledger`'s doc
/// comment. The record then explains, the next time anyone reads it, why
/// the rest of this file's lines describe a session under a different
/// name than the file itself.
///
/// Idempotent: a no-op when the slot already carries `new_name`. Both
/// callers rely on that: the ordered `SessionRenamed` event and
/// `run_session`'s lagged-receiver recovery path, which cannot tell
/// whether a `SessionRenamed` it missed was already applied by the time it
/// notices the drift.
///
/// `config.toml`'s `sessions` list is deliberately untouched: this mirrors
/// [`watch_session`], which also never rewrites it (a session watched at
/// runtime is not durable across a restart by design, and neither is a
/// rename of one. A restart re-reads `sessions` and watches the OLD name
/// again, same as it always has).
fn rename_session_slot(inner: &Arc<Inner>, idx: usize, new_name: String) -> bool {
    rename_session_slot_with_identity(inner, idx, new_name, None)
}

fn rename_session_slot_with_identity(
    inner: &Arc<Inner>,
    idx: usize,
    new_name: String,
    observed_instance_id: Option<SessionInstanceId>,
) -> bool {
    #[cfg(test)]
    if let Some(slot) = inner.session(idx) {
        let pause = slot
            .rename_pause
            .lock()
            .expect("session rename pause lock")
            .take();
        if let Some(pause) = pause {
            pause.entered.send(()).expect("rename pause receiver");
            pause.release.wait();
        }
    }
    let _registration = inner
        .session_registration
        .lock()
        .expect("session registration lock");
    // Shutdown crosses this same lock before restoring chrome. A rename that
    // lands after that barrier would split the slot name from the registry
    // name while restore is reading both, leaving Cyclops chrome behind.
    if inner.engine.is_stopping() {
        return true;
    }
    rename_session_slot_locked(inner, idx, new_name, observed_instance_id)
}

fn rename_session_slot_locked(
    inner: &Arc<Inner>,
    idx: usize,
    new_name: String,
    observed_instance_id: Option<SessionInstanceId>,
) -> bool {
    let Some(slot) = inner.session(idx) else {
        return false;
    };
    if !slot.is_canonical() {
        return false;
    }
    let old_name = slot.name();
    if old_name == new_name {
        return true;
    }

    // A runtime-created session can be watched under its temporary name and
    // then renamed onto a configured name whose slot is still detached. The
    // source slot owns the live watcher and therefore wins. The other slot
    // cannot be removed because its index may already be held by a task or a
    // historical cursor; retire it as an alias instead. If its task won the
    // registration race and attached first, matching durable session identity
    // proves both watchers follow the same tmux session and wakes the loser so
    // it tears its duplicate connection down.
    let source_instance_id = observed_instance_id.or_else(|| {
        slot.link
            .lock()
            .expect("session link lock")
            .identity
            .as_ref()
            .map(SessionIdentityBinding::session_instance_id)
    });
    let collision = inner
        .active_session_slots()
        .into_iter()
        .find(|(other_idx, other)| *other_idx != idx && other.name() == new_name);
    let retired = if let Some((other_idx, other)) = collision {
        let (target_is_live, target_instance_id) = {
            let link = other.link.lock().expect("session link lock");
            (
                link.attached || link.watcher.is_some(),
                link.identity
                    .as_ref()
                    .map(SessionIdentityBinding::session_instance_id),
            )
        };
        let same_live_identity = matches!(
            (source_instance_id, target_instance_id),
            (Some(source), Some(target)) if source == target
        );
        if target_is_live && !same_live_identity {
            error!(
                source_idx = idx,
                target_idx = other_idx,
                old_name = %old_name,
                new_name = %new_name,
                "refusing to merge live session slots without matching durable identity"
            );
            return true;
        }
        let Some(canonical_journal) = slot.journal_file_name() else {
            error!(
                source_idx = idx,
                target_idx = other_idx,
                "rename collision source has no journal file name"
            );
            return false;
        };
        if inner
            .append_line(
                other_idx,
                daemon_line(
                    Kind::System,
                    inner.mint_event_id(),
                    json!({
                        "event": "session_slot_aliased",
                        "session": &new_name,
                        "canonical_session_idx": idx,
                        "canonical_journal": canonical_journal,
                    }),
                ),
            )
            .is_none()
        {
            error!(
                source_idx = idx,
                target_idx = other_idx,
                "rename collision history link could not be recorded"
            );
            return false;
        }
        if !other.retire_as_alias(idx) {
            error!(
                source_idx = idx,
                target_idx = other_idx,
                new_name = %new_name,
                "rename collision target changed before it could be retired"
            );
            return true;
        }
        for alias in inner.session_slots() {
            alias.retarget_alias(other_idx, idx);
        }
        debug_assert!(inner.session(idx).is_some_and(|slot| slot.is_canonical()));
        debug_assert!(inner.session_slots().iter().all(|slot| {
            slot.alias_of().is_none_or(|canonical_idx| {
                inner
                    .session(canonical_idx)
                    .is_some_and(|canonical| canonical.is_canonical())
            })
        }));
        Some((other_idx, other, target_is_live))
    } else {
        None
    };

    let replaced = slot
        .rename(new_name.clone())
        .expect("the old and new session names were checked above");
    debug_assert_eq!(replaced, old_name);
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
    refresh_mailbox_and_schedule(inner);
    info!(old_name = %old_name, new_name = %new_name, "session renamed; daemon slot now follows tmux");
    if let Some((alias_idx, alias, was_live)) = retired {
        info!(
            canonical_idx = idx,
            alias_idx,
            session = %new_name,
            was_live,
            "retired a duplicate session slot after a rename collision"
        );
        debug_assert_eq!(alias.alias_of(), Some(idx));
    }
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
    true
}

/// Reconcile daemon-owned pane routes after its watcher receiver lagged.
///
/// The watcher updates its own table before broadcasting an edge. If this
/// receiver drops that edge, asking the watcher to reconcile again produces
/// no diff. Compare the daemon's last published roots with the authoritative
/// watcher snapshot so a missed process replacement still crosses the same
/// checked transition as a live `PanePid` event.
#[derive(Debug, Default, PartialEq, Eq)]
struct PaneRouteReconcile {
    changed_panes: Vec<String>,
    route_changed: bool,
}

fn reconcile_missed_pane_routes(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
) -> Result<PaneRouteReconcile, &'static str> {
    let slot = inner
        .session(session_idx)
        .ok_or("lag_reconcile_session_missing")?;
    let previous = {
        let link = slot.link.lock().expect("session link lock");
        let current = link
            .watcher
            .as_ref()
            .ok_or("lag_reconcile_watcher_missing")?;
        if !link.attached || !Arc::ptr_eq(current, watcher) {
            return Err("lag_reconcile_watcher_changed");
        }
        link.mailbox_panes.clone()
    };
    let rows: Vec<ObservedPane> = watcher
        .snapshot()
        .into_iter()
        .map(ObservedPane::capture)
        .collect();
    let current: HashMap<_, _> = rows
        .iter()
        .cloned()
        .map(|pane| (pane.row.pane_id.clone(), pane))
        .collect();

    if previous
        .keys()
        .any(|pane_id| !current.contains_key(pane_id))
    {
        return Err("lag_reconcile_pane_removed");
    }
    let mut outcome = PaneRouteReconcile::default();
    for pane in &rows {
        match previous.get(&pane.row.pane_id) {
            None => {
                outcome.changed_panes.push(pane.row.pane_id.clone());
                outcome.route_changed = true;
            }
            Some(old) if old != pane => {
                outcome.changed_panes.push(pane.row.pane_id.clone());
                outcome.route_changed |= old.root != pane.root;
                let generation_changed = replace_pane_process(
                    inner,
                    session_idx,
                    watcher,
                    &pane.row.pane_id,
                    &pane.row,
                )?;
                debug_assert_eq!(generation_changed, old.root != pane.root);
            }
            Some(_) => {}
        }
    }
    if outcome.changed_panes.is_empty() {
        return Ok(outcome);
    }
    if current
        .keys()
        .any(|pane_id| !previous.contains_key(pane_id))
        && !update_mailbox_route(inner, || {
            slot.link.lock().expect("session link lock").mailbox_panes = current;
        })
    {
        return Err("lag_reconcile_directory_publish_failed");
    }
    Ok(outcome)
}

/// Pump one attached watcher until it disconnects or the daemon stops.
async fn run_session(
    inner: &Arc<Inner>,
    idx: usize,
    watcher: &Arc<SessionWatcher>,
    binding: SessionIdentityBinding,
    mut stop: watch::Receiver<bool>,
) -> bool {
    let Some(slot) = inner.session(idx) else {
        return false;
    };
    let mut alias_changed = slot.alias_changed.subscribe();
    if !slot.is_canonical() {
        return false;
    }
    let mut rx = watcher.subscribe();
    // A rename can land after SessionWatcher::connect returns but before
    // this receiver exists. Broadcast channels do not replay that event;
    // synchronize from the watcher's live name once after subscribing so
    // the daemon slot and registry cannot remain on the connect-time name
    // forever. If the event lands after subscribe, the ordinary match arm
    // below is idempotent with this check.
    let live_name = watcher.session();
    if inner.session(idx).is_some_and(|s| s.name() != live_name) {
        rename_session_slot_with_identity(
            inner,
            idx,
            live_name,
            Some(binding.session_instance_id()),
        );
    }
    if !slot.is_canonical() || slot.name() != watcher.session() {
        return false;
    }
    let rows: Vec<ObservedPane> = watcher
        .snapshot()
        .into_iter()
        .map(ObservedPane::capture)
        .collect();
    if !live_pane_roots_are_proven(&rows) {
        warn!(session = %watcher.session(), "cannot attach until every live pane process generation is proven");
        return false;
    }
    let kept = match reconcile_adoption_records(inner, idx, watcher, binding.session_instance_id())
        .await
    {
        Ok(kept) => kept,
        Err(error) => {
            warn!(session = %watcher.session(), %error, "cannot reconcile recovered composer barriers");
            return false;
        }
    };
    let route = MailboxRouteOverride {
        session_idx: idx,
        instance_id: binding.session_instance_id(),
        rows: &rows,
    };
    for pane in &rows {
        inner
            .hook_liveness
            .open(&PaneKey::new(idx, &pane.row.pane_id));
    }
    // A detached slot may have been retired as an alias while this task was
    // observing process generations or reconciling adoption records. Share
    // the registration barrier with rename collision handling so the losing
    // task can never publish a second live route after retirement.
    let _registration = inner
        .session_registration
        .lock()
        .expect("session registration lock");
    if !slot.is_canonical() {
        return false;
    }
    if let Err(error) = publish_mailbox_transition(inner, &route, || {
        let mut link = slot.link.lock().expect("session link lock");
        link.identity = Some(binding);
        link.attached = true;
        link.watcher = Some(Arc::clone(watcher));
        link.mailbox_panes = rows
            .iter()
            .cloned()
            .map(|pane| (pane.row.pane_id.clone(), pane))
            .collect();
    }) {
        warn!(session = %watcher.session(), error = %error, "cannot publish mailbox directory");
        return false;
    }
    drop(_registration);
    info!(session = %slot.name(), "attached to tmux session");
    session_lifecycle(inner, idx, true);
    // Bootstrap: the watcher's table is already authoritative; evaluate
    // every pane once so status answers immediately. Adoptions are
    // reconciled against that table first, so the very first recompute
    // already knows which panes are named and which manifest is pinned.
    reconcile_adoptions(inner, idx, watcher, &kept).await;
    for row in watcher.snapshot() {
        let route_evidence = inner.advance_route_evidence(idx, &row.pane_id);
        fusion::recompute_pane_for_route_evidence(
            inner,
            idx,
            watcher,
            &row.pane_id,
            false,
            "bootstrap",
            &route_evidence,
        )
        .await;
        messaging::schedule_route_evidence(inner, idx, &row.pane_id, &route_evidence);
    }
    // Per-pane debounce kickers for output activity.
    let mut debounce: HashMap<String, watch::Sender<u64>> = HashMap::new();
    loop {
        tokio::select! {
            biased;
            changed = alias_changed.changed() => {
                if changed.is_err() || !slot.is_canonical() {
                    return true;
                }
            }
            _ = stop.changed() => return true,
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if handle_pane_event(inner, idx, watcher, &mut debounce, ev).await {
                        return true;
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
                        && !rename_session_slot(inner, idx, live_name)
                    {
                        return true;
                    }
                    warn!(session = %watcher.session(), missed, "event stream lagged; reconciling");
                    if watcher.reconcile_now().await.is_err() {
                        return true;
                    }
                    if let Err(reason) = reconcile_missed_pane_routes(inner, idx, watcher) {
                        warn!(session = %watcher.session(), %reason, "cannot reconcile lagged pane routes safely");
                        return true;
                    }
                    for row in watcher.snapshot() {
                        let route_evidence = inner.advance_route_evidence(idx, &row.pane_id);
                        fusion::recompute_pane_for_route_evidence(
                            inner,
                            idx,
                            watcher,
                            &row.pane_id,
                            false,
                            "lag_reconcile",
                            &route_evidence,
                        ).await;
                        messaging::schedule_route_evidence(
                            inner,
                            idx,
                            &row.pane_id,
                            &route_evidence,
                        );
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return true,
            }
        }
    }
}

/// Paint the adoptions that survived attach-time registry reconciliation.
/// Directory publication happens first so mailbox routing and pane chrome
/// become visible in the same attach cycle.
async fn reconcile_adoptions(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    kept: &[registry::Adoption],
) {
    paint_adoptions(inner, session_idx, watcher, kept).await;
}

async fn reconcile_adoption_records(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    session_instance_id: SessionInstanceId,
) -> Result<Vec<registry::Adoption>, &'static str> {
    let observed: Vec<ObservedPane> = watcher
        .snapshot()
        .into_iter()
        .map(ObservedPane::capture)
        .collect();
    let live: Vec<(String, ProcessInstanceId)> = observed
        .iter()
        .filter_map(|pane| {
            let root = pane.root?;
            let pane_root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
            Some((pane.row.pane_id.clone(), pane_root))
        })
        .collect();
    let mut existing = inner
        .registry
        .lock()
        .expect("registry lock")
        .in_session(&watcher.session());
    rebind_same_session_adoptions(
        inner,
        session_idx,
        session_instance_id,
        &observed,
        &existing,
    )?;
    existing = inner
        .registry
        .lock()
        .expect("registry lock")
        .in_session(&watcher.session());
    let removed = stale_adoption_recipients(&existing, session_instance_id, &live);
    let mut transferred = HashSet::new();
    for recipient in removed {
        let adoption = existing
            .iter()
            .find(|adoption| adoption.recipient == Some(recipient))
            .ok_or("composer_recovery_pane_root_unproven")?;
        if crate::composer_recovery::physical_pane_gone(watcher, adoption).await? {
            crate::composer_recovery::retire_gone_recipient(inner, recipient)?;
        } else {
            transferred.insert(recipient);
        }
    }
    let mut reg = inner.registry.lock().expect("registry lock");
    match reg.restore_session_preserving(
        &watcher.session(),
        session_instance_id,
        &live,
        &transferred,
    ) {
        Ok(kept) => Ok(kept),
        Err(e) => {
            error!(session = %watcher.session(), error = %e, "cannot rewrite the registry; keeping it in memory only");
            Ok(reg.in_session(&watcher.session()))
        }
    }
}

/// Preserve logical pane names when Cyclops reconnects after a same-session
/// process replacement.
///
/// A new tmux server or session has a different `SessionInstanceId` and never
/// enters this path. Exact old roots move to the watcher snapshot as one
/// durable compare-and-swap. Process-bound trust is retired before any
/// replacement route can become addressable. Only the label, manifest pin,
/// and saved chrome move.
fn rebind_same_session_adoptions(
    inner: &Inner,
    session_idx: usize,
    session_instance_id: SessionInstanceId,
    observed: &[ObservedPane],
    existing: &[registry::Adoption],
) -> Result<(), &'static str> {
    let replacements: Vec<_> = existing
        .iter()
        .filter_map(|adoption| {
            let recipient = adoption.recipient?;
            if recipient.session_instance_id() != Some(session_instance_id) {
                return None;
            }
            let old_root = adoption.pane_root?;
            let pane = observed.iter().find(|pane| {
                pane.row.pane_id == adoption.pane_id
                    && pane.root.is_some_and(|root| {
                        root.pid != old_root.pid() || root.birth != old_root.birth()
                    })
            })?;
            let root = pane.root?;
            let new_root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
            Some((recipient, old_root, new_root, pane.row.clone()))
        })
        .collect();
    if replacements.is_empty() {
        return Ok(());
    }

    let _publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    let slot = inner
        .session(session_idx)
        .ok_or("adoption_rebind_session_missing")?;
    for (recipient, old_root, new_root, row) in replacements {
        let replacement_binding = fusion::admitted_binding(inner, session_idx, &row)
            .as_ref()
            .and_then(|binding| crate::composer_recovery::observed_binding(recipient, binding));
        let mut registry = inner.registry.lock().expect("registry lock");
        let Some(current_root) = registry
            .for_recipient(recipient)
            .and_then(|adoption| adoption.pane_root)
        else {
            return Err("adoption_rebind_route_changed");
        };
        if current_root != old_root && current_root != new_root {
            return Err("adoption_rebind_route_changed");
        }
        if inner.mailbox.is_some() {
            crate::composer_recovery::retire_replaced_recipient(
                inner,
                recipient,
                replacement_binding,
            )?;
        }
        if current_root == old_root {
            match registry.rebind_process(recipient, old_root, new_root) {
                Ok(true) => {}
                Ok(false) => return Err("adoption_rebind_route_changed"),
                Err(error) => {
                    error!(recipient = %recipient, error = %error, "cannot persist adoption process replacement");
                    return Err("adoption_rebind_write_failed");
                }
            }
        }
        drop(registry);
        let pane_id = recipient
            .pane_id()
            .ok_or("adoption_rebind_recipient_invalid")?;
        retire_pane_process_trust(inner, session_idx, &pane_id.to_string());
    }
    {
        let mut last = slot.last_panes.lock().expect("last panes lock");
        for pane in observed {
            last.insert(pane.row.pane_id.clone(), pane.clone());
        }
    }
    Ok(())
}

/// Remove current facts authenticated by the process generation that exited.
///
/// Logical pane identity survives a respawn. Detection, lifecycle hooks, and
/// cached process evidence do not. Exact hook-liveness history remains keyed by
/// its process generation until the physical pane closes, so a delayed
/// diagnostic cannot confuse a replacement with the submitted occupant. Both
/// live events and reconnect reconciliation use this operation before
/// publishing the replacement route.
fn retire_pane_process_trust(inner: &Inner, session_idx: usize, pane_id: &str) {
    let pane = PaneKey::new(session_idx, pane_id);
    inner
        .detections
        .lock()
        .expect("detections lock")
        .remove(&pane);
    turnkey::PaneEnds::forget(&mut inner.turn_ends.lock().expect("turn ends lock"), &pane);
    inner
        .hook_readings
        .lock()
        .expect("hook readings lock")
        .remove(&pane);
    fusion::cancel_lifecycle_recheck(inner, &pane);
    inner
        .argv_cache
        .lock()
        .expect("argv cache lock")
        .retain(|(cached, _), _| cached != &pane);
}

/// Exact routes disproved by one authoritative session snapshot.
fn stale_adoption_recipients(
    existing: &[registry::Adoption],
    session_instance_id: SessionInstanceId,
    live: &[(String, ProcessInstanceId)],
) -> Vec<RecipientKey> {
    existing
        .iter()
        .filter(|adoption| {
            adoption.recipient.is_some_and(|recipient| {
                recipient.session_instance_id() != Some(session_instance_id)
            }) || !live.iter().any(|(pane_id, pane_root)| {
                *pane_id == adoption.pane_id && adoption.pane_root == Some(*pane_root)
            })
        })
        .filter_map(|adoption| adoption.recipient)
        .collect()
}

/// An adopted pane moved to another window (tmux `join-pane`,
/// `break-pane`). Take the border text off the window it left and put it
/// on the window it joined.
///
/// Border text is a window setting with no pane scope, so it does
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
    let Some((recipient, pane_root, _)) = adoption_route(inner, watcher, session_idx, pane_id)
    else {
        return;
    };
    // Nothing to do for a pane nobody named, or one already recorded in
    // the window it is now in.
    match inner
        .registry
        .lock()
        .expect("registry lock")
        .for_route(recipient, pane_root)
    {
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
        recipient,
        pane_root,
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

/// Apply one authoritative pane-root replacement without carrying trust from
/// the former process generation.
///
/// The durable recipient and its human label belong to the live tmux pane, so
/// they survive `respawn-pane` inside the same session instance. Composer
/// barriers, sensor state, hook liveness, and lifecycle edges belong to the
/// process that exited and are retired before the new route is published.
fn validated_replacement_generation(
    authoritative: &ObservedPane,
    replacement: &ObservedPane,
) -> Result<ProcessInstanceId, &'static str> {
    if authoritative.root != replacement.root {
        return Err("pane_process_event_stale");
    }
    replacement
        .root
        .and_then(|root| ProcessInstanceId::new(root.pid, root.birth).ok())
        .ok_or("pane_process_generation_unproven")
}

fn replace_pane_process(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    pane_id: &str,
    row: &PaneRow,
) -> Result<bool, &'static str> {
    let _publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    let authoritative = watcher
        .pane(pane_id)
        .map(ObservedPane::capture)
        .ok_or("pane_process_route_missing")?;
    let event_observation = ObservedPane::capture(row.clone());
    let replacement_root = validated_replacement_generation(&authoritative, &event_observation)?;
    let replacement = authoritative;
    let slot = inner
        .session(session_idx)
        .ok_or("pane_process_session_missing")?;
    let (recipient, previous) = {
        let link = slot.link.lock().expect("session link lock");
        let current = link
            .watcher
            .as_ref()
            .ok_or("pane_process_watcher_missing")?;
        if !link.attached || !Arc::ptr_eq(current, watcher) {
            return Err("pane_process_watcher_changed");
        }
        let instance = link
            .identity
            .as_ref()
            .ok_or("pane_process_session_identity_missing")?
            .session_instance_id();
        let pane = pane_id
            .parse::<TmuxPaneId>()
            .map_err(|_| "pane_process_id_invalid")?;
        let previous = link
            .mailbox_panes
            .get(pane_id)
            .cloned()
            .ok_or("pane_process_previous_route_missing")?;
        (
            RecipientKey::agent(inner.workspace_id, instance, pane),
            previous,
        )
    };
    if previous.root == replacement.root {
        if previous.row != replacement.row {
            let mut link = slot.link.lock().expect("session link lock");
            let current = link
                .watcher
                .as_ref()
                .ok_or("pane_process_watcher_missing")?;
            if !link.attached || !Arc::ptr_eq(current, watcher) {
                return Err("pane_process_watcher_changed");
            }
            let stored = link
                .mailbox_panes
                .get(pane_id)
                .ok_or("pane_process_previous_route_missing")?;
            if stored.root != replacement.root {
                return Err("pane_process_route_changed");
            }
            link.mailbox_panes.insert(pane_id.to_string(), replacement);
        }
        return Ok(false);
    }

    let previous_root = previous
        .root
        .and_then(|root| ProcessInstanceId::new(root.pid, root.birth).ok());
    if previous.root.is_some() != previous_root.is_some() {
        return Err("pane_process_generation_invalid");
    } else {
        let new_root = replacement_root;
        let replacement_binding = fusion::admitted_binding(inner, session_idx, &replacement.row)
            .as_ref()
            .and_then(|binding| crate::composer_recovery::observed_binding(recipient, binding));
        let mut registry = inner.registry.lock().expect("registry lock");
        let adopted_root = registry
            .for_recipient(recipient)
            .and_then(|adoption| adoption.pane_root);
        match adopted_root {
            None => {}
            Some(current) if current == new_root || previous_root == Some(current) => {
                if inner.mailbox.is_some() {
                    crate::composer_recovery::retire_replaced_recipient(
                        inner,
                        recipient,
                        replacement_binding,
                    )?;
                }
                if current != new_root {
                    match registry.rebind_process(recipient, current, new_root) {
                        Ok(true) => {}
                        Ok(false) => return Err("pane_process_adoption_changed"),
                        Err(error) => {
                            error!(pane = %pane_id, error = %error, "cannot persist pane process replacement");
                            return Err("pane_process_adoption_write_failed");
                        }
                    }
                }
            }
            Some(_) => return Err("pane_process_adoption_changed"),
        }
    }

    retire_pane_process_trust(inner, session_idx, pane_id);

    {
        let mut link = slot.link.lock().expect("session link lock");
        link.mailbox_panes.insert(pane_id.to_string(), replacement);
    }
    if !refresh_mailbox_directory_unlocked(inner) {
        return Err("pane_process_directory_publish_failed");
    }
    inner.emit("messages.route_changed", json!({}), None);
    Ok(true)
}

/// Apply one watcher event. Returns true when the connection is over.
async fn handle_pane_event(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    debounce: &mut HashMap<String, watch::Sender<u64>>,
    ev: PaneEvent,
) -> bool {
    match ev {
        PaneEvent::PaneAdded(row) => {
            inner
                .hook_liveness
                .open(&PaneKey::new(session_idx, &row.pane_id));
            update_mailbox_route(inner, || {
                let Some(slot) = inner.session(session_idx) else {
                    return;
                };
                slot.link
                    .lock()
                    .expect("session link lock")
                    .mailbox_panes
                    .insert(row.pane_id.clone(), ObservedPane::capture(row.clone()));
            });
            let route_evidence = inner.advance_route_evidence(session_idx, &row.pane_id);
            fusion::recompute_pane_for_route_evidence(
                inner,
                session_idx,
                watcher,
                &row.pane_id,
                false,
                "pane_added",
                &route_evidence,
            )
            .await;
            messaging::schedule_route_evidence(inner, session_idx, &row.pane_id, &route_evidence);
            false
        }
        PaneEvent::PaneRemoved(id) => {
            let recipient = inner.session(session_idx).and_then(|slot| {
                let link = slot.link.lock().expect("session link lock");
                let current = link.watcher.as_ref()?;
                if !Arc::ptr_eq(current, watcher) {
                    return None;
                }
                let pane = id.parse().ok()?;
                Some(RecipientKey::agent(
                    inner.workspace_id,
                    link.identity.as_ref()?.session_instance_id(),
                    pane,
                ))
            });
            let adoptions = inner.registry.lock().expect("registry lock").in_pane(&id);
            let expected = recipient
                .and_then(|recipient| {
                    adoptions
                        .iter()
                        .find(|adoption| adoption.recipient == Some(recipient))
                })
                .or_else(|| (adoptions.len() == 1).then(|| &adoptions[0]));
            let physical_gone = match expected {
                Some(adoption) => {
                    crate::composer_recovery::physical_pane_gone(watcher, adoption).await
                }
                None => {
                    crate::composer_recovery::physical_pane_gone_with_expected(watcher, &id, None)
                        .await
                }
            };
            let physical_gone = match physical_gone {
                Ok(gone) => gone,
                Err(reason) => {
                    warn!(pane = %id, %reason, "cannot prove physical pane loss before pane cleanup");
                    return true;
                }
            };
            if physical_gone {
                let gone_recipients: HashSet<_> = recipient
                    .into_iter()
                    .chain(expected.and_then(|adoption| adoption.recipient))
                    .collect();
                for recipient in gone_recipients {
                    if let Err(reason) =
                        crate::composer_recovery::retire_gone_recipient(inner, recipient)
                    {
                        warn!(pane = %id, %reason, "cannot retire composer barrier before pane cleanup");
                        return true;
                    }
                }
            }
            update_mailbox_route(inner, || {
                let Some(slot) = inner.session(session_idx) else {
                    return;
                };
                slot.link
                    .lock()
                    .expect("session link lock")
                    .mailbox_panes
                    .remove(&id);
            });
            debounce.remove(&id);
            if physical_gone {
                let pane = PaneKey::new(session_idx, &id);
                inner
                    .detections
                    .lock()
                    .expect("detections lock")
                    .remove(&pane);
                inner
                    .route_evidence_generations
                    .lock()
                    .expect("route evidence generations lock")
                    .remove(&pane);
                // Stored turn ends die only with the physical pane. A
                // session transfer keeps the exact end and composer hold
                // under its source route until the destination recompute
                // restores the durable barrier.
                turnkey::PaneEnds::forget(
                    &mut inner.turn_ends.lock().expect("turn ends lock"),
                    &pane,
                );
                // Adoption, hook history and argv identity also belong to
                // this exact route. Clear them only after the same proof.
                // A duplicate pane id in another watched tmux server must
                // retain its independent state and adoption.
                let route = recipient
                    .and_then(|recipient| {
                        adoptions
                            .iter()
                            .find(|adoption| adoption.recipient == Some(recipient))
                    })
                    .or(expected)
                    .and_then(|adoption| Some((adoption.recipient?, adoption.pane_root?)));
                let freed = route.and_then(|(recipient, pane_root)| {
                    let mut registry = inner.registry.lock().expect("registry lock");
                    registry
                        .forget(recipient, pane_root)
                        .and_then(|(adoption, freed)| freed.map(|window| (adoption, window)))
                });
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
                    .remove(&pane);
                fusion::cancel_lifecycle_recheck(inner, &pane);
                inner
                    .argv_cache
                    .lock()
                    .expect("argv cache lock")
                    .retain(|(cached, _), _| cached != &pane);
                inner.hook_liveness.close(&pane);
            }
            messaging::schedule_available(inner);
            // The source session's last transition for this pane. The pane
            // may still exist under another session after a route transfer.
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
                    "session_idx": session_idx,
                    "pane_id": id,
                    "physical_gone": physical_gone,
                }),
                None,
            );
            false
        }
        PaneEvent::PaneChanged { id, changed, row } => {
            // `pane_pid` is only a numeric id. Reconcile the exact process
            // generation on every pane change so rapid PID reuse cannot
            // preserve a predecessor's durable route when the integer is
            // unchanged.
            let occupant_changed =
                match replace_pane_process(inner, session_idx, watcher, &id, &row) {
                    Ok(changed) => changed,
                    Err("pane_process_event_stale") => return false,
                    Err(reason) => {
                        warn!(pane = %id, %reason, "cannot replace pane process safely");
                        return true;
                    }
                };
            // Size and focus changes do not move agent state. Title, death,
            // mode, foreground command, and occupant generation do.
            let relevant = changed.iter().any(|f| {
                matches!(
                    f,
                    PaneField::Title
                        | PaneField::Dead
                        | PaneField::InMode
                        | PaneField::CurrentCommand
                        | PaneField::PanePid
                )
            });
            let size_changed = changed.iter().any(|field| matches!(field, PaneField::Size));
            let route_changed = relevant || occupant_changed;
            let route_evidence = if route_changed {
                inner.advance_route_evidence(session_idx, &id)
            } else {
                inner.route_evidence_id(session_idx, &id)
            };
            if route_changed {
                fusion::recompute_pane_for_route_evidence(
                    inner,
                    session_idx,
                    watcher,
                    &id,
                    false,
                    "pane_changed",
                    &route_evidence,
                )
                .await;
                messaging::schedule_route_evidence(inner, session_idx, &id, &route_evidence);
            }
            if size_changed {
                messaging::schedule_pane_size_changed(inner, session_idx, &id, &route_evidence);
            }
            // A move does not touch agent state, but it does move half the
            // chrome: the pane carries its own options and the window's
            // border text does not follow it.
            if changed.iter().any(|f| matches!(f, PaneField::WindowId)) {
                move_chrome(inner, session_idx, watcher, &id).await;
            }
            false
        }
        PaneEvent::OutputActivity { pane_id, ts } => {
            let Some(row) = watcher.pane(&pane_id) else {
                return false;
            };
            match replace_pane_process(inner, session_idx, watcher, &pane_id, &row) {
                Ok(true) => {}
                Ok(false) | Err("pane_process_event_stale") => {}
                Err(reason) => {
                    warn!(pane = %pane_id, %reason, "cannot reconcile output with the pane process safely");
                    return true;
                }
            }
            kick_debounce(inner, session_idx, watcher, debounce, pane_id, ts);
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
        // emits after the rename gets processed. See `emit_state`'s doc
        // comment for what breaks if that ordering did not hold.
        PaneEvent::SessionRenamed { name } => !rename_session_slot(inner, session_idx, name),
        PaneEvent::Reconciled => {
            let outcome = match reconcile_missed_pane_routes(inner, session_idx, watcher) {
                Ok(outcome) => outcome,
                Err(reason) => {
                    warn!(session = %watcher.session(), %reason, "cannot publish an authoritative pane reconciliation");
                    return true;
                }
            };
            if outcome.changed_panes.is_empty() {
                return false;
            }
            for pane_id in outcome.changed_panes {
                let route_evidence = inner.advance_route_evidence(session_idx, &pane_id);
                fusion::recompute_pane_for_route_evidence(
                    inner,
                    session_idx,
                    watcher,
                    &pane_id,
                    false,
                    "pane_reconciled",
                    &route_evidence,
                )
                .await;
                messaging::schedule_route_evidence(inner, session_idx, &pane_id, &route_evidence);
            }
            false
        }
        PaneEvent::Disconnected => true,
    }
}

/// Feed a pane's debounce task, spawning it on first activity. An existing
/// task keeps only the newest output evidence timestamp for the pane.
fn kick_debounce(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &Arc<SessionWatcher>,
    debounce: &mut HashMap<String, watch::Sender<u64>>,
    pane_id: String,
    evidence_ms: u64,
) {
    if let Some(tx) = debounce.get(&pane_id) {
        if update_output_evidence(tx, evidence_ms) {
            return;
        }
    }
    let (tx, rx) = watch::channel(evidence_ms);
    debounce.insert(pane_id.clone(), tx);
    inner.engine.spawn_descendant_task(debounce_task(
        rx,
        Arc::clone(inner),
        session_idx,
        Arc::clone(watcher),
        pane_id,
    ));
}

fn update_output_evidence(tx: &watch::Sender<u64>, evidence_ms: u64) -> bool {
    if tx.receiver_count() == 0 {
        return false;
    }
    // A watch channel has no full state. The settle task always sees the
    // newest causal timestamp even when it falls behind an output burst.
    tx.send_modify(|current| *current = (*current).max(evidence_ms));
    true
}

/// Output settle debounce: a reset timer, not an interval. The sleep only
/// exists between the first kick and the settle; each further kick pushes
/// the deadline out. With no output, this task is parked in recv.
///
/// `session_idx` is captured once at spawn, not re-derived from
/// `watcher.session()` on each fire: this task runs off its own timer,
/// entirely outside the watcher's ordered event channel, so a rename that
/// races it would hit the exact `session_index` miss `emit_state`'s doc
/// comment describes. The captured idx cannot go stale mid-task because one
/// debounce task lives only as long as one attach, and `session_idx` is
/// append-only-stable for the daemon's whole life regardless of how many
/// times the slot it names gets renamed.
async fn debounce_task(
    mut rx: watch::Receiver<u64>,
    inner: Arc<Inner>,
    session_idx: usize,
    watcher: Arc<SessionWatcher>,
    pane_id: String,
) {
    loop {
        let Some(evidence_ms) = settled_output_evidence(&mut rx).await else {
            return;
        };
        let route_evidence = inner.advance_route_evidence(session_idx, &pane_id);
        fusion::recompute_pane_from_output(
            &inner,
            session_idx,
            &watcher,
            &pane_id,
            evidence_ms,
            &route_evidence,
        )
        .await;
        messaging::schedule_route_evidence(&inner, session_idx, &pane_id, &route_evidence);
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Wait for one quiet output window and return the newest causal timestamp.
async fn settled_output_evidence(rx: &mut watch::Receiver<u64>) -> Option<u64> {
    let mut evidence_ms = *rx.borrow_and_update();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(OUTPUT_SETTLE) => return Some(evidence_ms),
            changed = rx.changed() => {
                if changed.is_err() {
                    return None;
                }
                evidence_ms = evidence_ms.max(*rx.borrow_and_update());
                // Another burst of output: restart the settle window.
            }
        }
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

    #[test]
    fn output_debounce_retains_the_newest_causal_timestamp() {
        let (tx, mut rx) = watch::channel(10);

        assert!(update_output_evidence(&tx, 30));
        assert!(update_output_evidence(&tx, 20));
        assert!(update_output_evidence(&tx, 40));
        assert_eq!(*rx.borrow_and_update(), 40);

        drop(rx);
        assert!(!update_output_evidence(&tx, 50));
    }

    #[tokio::test(start_paused = true)]
    async fn output_debounce_resets_and_returns_the_newest_source_edge() {
        let (tx, mut rx) = watch::channel(10);
        let settled = tokio::spawn(async move { settled_output_evidence(&mut rx).await });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(update_output_evidence(&tx, 30));
        tokio::task::yield_now().await;
        tokio::time::advance(OUTPUT_SETTLE - Duration::from_millis(1)).await;
        assert!(!settled.is_finished());
        assert!(update_output_evidence(&tx, 20));
        assert!(update_output_evidence(&tx, 40));
        tokio::task::yield_now().await;
        tokio::time::advance(OUTPUT_SETTLE).await;

        assert_eq!(settled.await.unwrap(), Some(40));
    }

    #[test]
    fn boot_rejects_a_socket_outside_the_held_state_root() {
        let repair = RepairSummary::default();
        let home = cyclops_proto::scratch::scratch_dir("cyc-boot-socket-root");
        let _ = std::fs::remove_dir_all(&home);
        let state_root = StateRoot::open_or_create(&home).unwrap();

        let error = require_bound_socket_in_state_root(&repair, &state_root).unwrap_err();

        assert!(error
            .to_string()
            .contains("not inside the validated state root"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_fact_replays_the_upgraded_install_repair_summary() {
        use std::os::unix::fs::PermissionsExt as _;

        let tag = "cyc-repair-boot-fact";
        let home = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&home);
        let mut inner = bare_inner(tag);
        let legacy = home.join("legacy");
        let legacy_file = legacy.join("state.json");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(&legacy_file, b"legacy\n").unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::set_permissions(&legacy_file, std::fs::Permissions::from_mode(0o666)).unwrap();

        let repair = inner
            .state_root
            .repair_descendant_permissions(None)
            .unwrap();
        Arc::get_mut(&mut inner).unwrap().state_repair = repair;
        let ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/main.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        Arc::get_mut(&mut inner)
            .unwrap()
            .sessions
            .get_mut()
            .unwrap()
            .push(Arc::new(SessionSlot::new("main".into(), Arc::new(ledger))));
        inner.append_line(0, boot_fact_line(&inner, &[], "main"));

        let lines = inner.session(0).unwrap().ledger.read_after(0).unwrap();
        let fact = lines
            .iter()
            .filter_map(|line| line.data.as_ref())
            .find(|data| data["event"] == "boot")
            .unwrap();
        assert_eq!(fact["state_permission_repair"]["directories"], 2);
        assert_eq!(fact["state_permission_repair"]["regular_files"], 2);
        assert_eq!(
            fact["state_permission_repair"]["live_socket_preserved"],
            false
        );
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&legacy).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&legacy_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Minimal `Inner` with no sessions, manifests, or tmux. This is enough
    /// to exercise slot bookkeeping without a live daemon or a tmux server.
    /// Mirrors `server::tests::bare_inner`, kept separate because that one
    /// is private to its own module.
    fn bare_inner(tag: &str) -> Arc<Inner> {
        let home = cyclops_proto::scratch::scratch_dir(tag);
        let state_root = Arc::new(StateRoot::open_or_create(&home).unwrap());
        let (registry, _) = registry::Registry::load(Arc::clone(&state_root));
        let workspace_id = workspaceid::load_or_create(&state_root).unwrap();
        let session_identities = sessionstore::SessionIdentities::open(&state_root).unwrap();
        Arc::new(Inner {
            cfg: Config::defaults(&home),
            state_root,
            state_repair: RepairSummary::default(),
            workspace_id,
            session_identities: StdMutex::new(session_identities),
            mailbox: None,
            composer_recovery: StdMutex::new(composer_recovery::RecoveryCoordinator::default()),
            mailbox_publication: StdMutex::new(()),
            unread_projection_gate: tokio::sync::Mutex::new(()),
            unread_projection_pending: StdMutex::new(HashSet::new()),
            unread_projection_wake: Notify::new(),
            unread_projection_stopping: AtomicBool::new(false),
            unread_projection_pause: StdMutex::new(None),
            mailbox_publish_pause: StdMutex::new(None),
            boot_id: "b-test".into(),
            started: Instant::now(),
            tmux_version: "3.6a".into(),
            manifests: BTreeMap::new(),
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            session_registration: StdMutex::new(()),
            events: broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::new()),
            route_evidence_generations: StdMutex::new(HashMap::new()),
            pane_recomputes: StdMutex::new(HashMap::new()),
            lifecycle_rechecks: StdMutex::new(HashMap::new()),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
            hook_lifecycle: StdMutex::new(hook_lifecycle::Store::new()),
            turn_ends: StdMutex::new(turnkey::Ends::new()),
            argv_cache: StdMutex::new(HashMap::new()),
            engine: delivery::Engine::new(),
            ack_state: ack::AckState::new(),
            hook_liveness: selftest::HookLiveness::new(),
            inject_pause: StdMutex::new(None),
            name_reconcile_pause: StdMutex::new(None),
            fail_chrome_restore: AtomicBool::new(false),
            fail_next_final_binding_observation: AtomicBool::new(false),
            fail_pre_record_writing: StdMutex::new(None),
            workspace_ui: StdMutex::new(workspace_ui::WorkspaceUiState::default()),
            shutdown_request: watch::channel(false).0,
            stop: watch::channel(false).1,
            extra_tasks: StdMutex::new(Vec::new()),
        })
    }

    fn enable_mailbox(inner: &mut Arc<Inner>) {
        let directory = mailbox::MailboxDirectory::new(
            inner.workspace_id,
            std::iter::empty::<mailbox::MailboxIdentity>(),
        )
        .unwrap();
        let store = mailbox::MessageStore::open(
            &inner.state_root,
            Path::new("workspaces/test/messages.ndjson"),
            inner.workspace_id,
            &inner.boot_id,
        )
        .unwrap();
        let events = inner.events.clone();
        Arc::get_mut(inner).unwrap().mailbox = Some(Arc::new(
            mailbox::MailboxService::new_with_events(directory, store, events),
        ));
    }

    fn test_pane(pane_id: &str, pane_pid: i32) -> PaneRow {
        PaneRow {
            pane_id: pane_id.into(),
            window_id: "@1".into(),
            window_name: "test".into(),
            title: String::new(),
            dead: false,
            in_mode: false,
            current_command: "test".into(),
            width: 80,
            height: 24,
            active: true,
            pane_pid,
        }
    }

    #[test]
    fn an_attached_live_pane_requires_an_exact_process_generation() {
        let mut live = ObservedPane {
            row: test_pane("%0", 41),
            root: Some(identity::ProcId { pid: 41, birth: 7 }),
        };
        assert!(live_pane_roots_are_proven(std::slice::from_ref(&live)));

        live.root = None;
        assert!(!live_pane_roots_are_proven(std::slice::from_ref(&live)));

        live.row.dead = true;
        assert!(live_pane_roots_are_proven(std::slice::from_ref(&live)));
    }

    fn test_live_key(
        workspace_id: WorkspaceId,
        server_pid: i32,
        server_birth: u64,
        session_id: &str,
    ) -> cyclops_proto::LiveSessionKey {
        cyclops_proto::LiveSessionKey::new(
            workspace_id,
            cyclops_proto::OsBootId::new("boot-test").unwrap(),
            cyclops_proto::ProcessInstanceId::new(server_pid, server_birth).unwrap(),
            session_id.parse().unwrap(),
        )
    }

    fn persist_test_binding(
        inner: &Inner,
        live_key: cyclops_proto::LiveSessionKey,
    ) -> SessionIdentityBinding {
        let instance_id = inner
            .session_identities
            .lock()
            .unwrap()
            .resolve(&inner.state_root, &live_key, || {
                SessionInstanceId::from_uuid(uuid::Uuid::new_v4()).unwrap()
            })
            .unwrap();
        SessionIdentityBinding::new(live_key, instance_id)
    }

    fn add_detached_route(
        inner: &Inner,
        name: &str,
        row: PaneRow,
        binding: SessionIdentityBinding,
    ) -> Arc<SessionSlot> {
        let descendant = PathBuf::from("ledger").join(format!("{name}.ndjson"));
        let ledger = LedgerWriter::open(&inner.state_root, &descendant, &inner.boot_id).unwrap();
        let slot = Arc::new(SessionSlot::new(name.into(), Arc::new(ledger)));
        let pane = ObservedPane::capture(row);
        slot.last_panes
            .lock()
            .unwrap()
            .insert(pane.row.pane_id.clone(), pane);
        slot.link.lock().unwrap().identity = Some(binding);
        inner.sessions.lock().unwrap().push(Arc::clone(&slot));
        slot
    }

    fn set_test_label(inner: &Inner, session: &str, pane: TmuxPaneId, pane_pid: i32, label: &str) {
        let session_instance_id = inner
            .session_index(session)
            .and_then(|idx| inner.session(idx))
            .and_then(|slot| {
                slot.link
                    .lock()
                    .unwrap()
                    .identity
                    .as_ref()
                    .map(SessionIdentityBinding::session_instance_id)
            })
            .unwrap();
        let root = identity::ProcId::of(pane_pid).unwrap();
        inner
            .registry
            .lock()
            .unwrap()
            .adopt(
                registry::Adoption {
                    session: session.into(),
                    pane_id: pane.to_string(),
                    label: label.into(),
                    recipient: Some(RecipientKey::agent(
                        inner.workspace_id,
                        session_instance_id,
                        pane,
                    )),
                    pane_root: Some(ProcessInstanceId::new(root.pid, root.birth).unwrap()),
                    manifest: None,
                    pane_pid,
                    window_id: "@1".into(),
                    border_format: None,
                },
                registry::WindowChrome {
                    session: session.into(),
                    window_id: "@1".into(),
                    border_status: None,
                },
            )
            .unwrap();
        refresh_mailbox_directory(inner);
    }

    fn send_test_message(
        service: &mailbox::MailboxService,
        address: String,
    ) -> Result<mailbox::AcceptResult, mailbox::MailboxServiceError> {
        service.send(
            service.admin(),
            mailbox::MailboxSend {
                addresses: vec![address],
                recipient_keys: None,
                subject: "Test".into(),
                body: String::new(),
                fyi: false,
                client_key: None,
                supersedes: None,
            },
        )
    }

    #[test]
    fn a_reused_pane_id_retires_the_old_exact_session_route() {
        let workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(1)).unwrap();
        let old_session = SessionInstanceId::from_uuid(uuid::Uuid::from_u128(2)).unwrap();
        let new_session = SessionInstanceId::from_uuid(uuid::Uuid::from_u128(3)).unwrap();
        let pane: TmuxPaneId = "%1".parse().unwrap();
        let old_root = ProcessInstanceId::new(41, 141).unwrap();
        let new_root = ProcessInstanceId::new(42, 142).unwrap();
        let old_recipient = RecipientKey::agent(workspace_id, old_session, pane);
        let existing = vec![registry::Adoption {
            session: "main".into(),
            pane_id: pane.to_string(),
            label: "reviewer".into(),
            recipient: Some(old_recipient),
            pane_root: Some(old_root),
            manifest: Some("codex".into()),
            pane_pid: old_root.pid(),
            window_id: "@1".into(),
            border_format: None,
        }];

        assert_eq!(
            stale_adoption_recipients(&existing, new_session, &[(pane.to_string(), new_root)]),
            vec![old_recipient]
        );
    }

    #[tokio::test]
    async fn daemon_restart_reopens_one_workspace_mailbox() {
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-workspace-restart-{}",
            uuid::Uuid::new_v4()
        ));
        let first = boot(Config::defaults(&home)).await.unwrap();
        let workspace_id = first.inner.workspace_id;
        let service = first.inner.mailbox.as_ref().unwrap();
        let accepted = service
            .send(
                service.admin(),
                mailbox::MailboxSend {
                    addresses: vec!["admin".into()],
                    recipient_keys: None,
                    subject: "Restart".into(),
                    body: "Persisted".into(),
                    fyi: false,
                    client_key: Some("restart-test".into()),
                    supersedes: None,
                },
            )
            .unwrap();
        let message_id = accepted.message_id.clone();
        first.shutdown().await;
        drop(first);

        let second = boot(Config::defaults(&home)).await.unwrap();
        assert_eq!(second.inner.workspace_id, workspace_id);
        let inbox = second
            .inner
            .mailbox
            .as_ref()
            .unwrap()
            .list(RecipientKey::admin(workspace_id), None, None)
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].entry.message_id, message_id);
        second.shutdown().await;
        drop(second);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn two_boots_settle_a_rename_linked_legacy_journal_once() {
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-linked-boot-replay-{}",
            uuid::Uuid::new_v4()
        ));
        let state_root = StateRoot::open_or_create(&home).unwrap();
        let configured =
            LedgerWriter::open(&state_root, Path::new("ledger/research.ndjson"), "old-boot")
                .unwrap();
        let linked =
            LedgerWriter::open(&state_root, Path::new("ledger/runtime.ndjson"), "old-boot")
                .unwrap();

        let legacy_id = "m-abcdef";
        let skewed_id = "m-123456";
        let assert_terminal_history =
            |configured_lines: Vec<LedgerLine>, linked_lines: Vec<LedgerLine>| {
                // The replay traversal presents a family descendants-first
                // and configured-root-last. Wall clocks in the older file
                // may be ahead; causal family order still decides.
                let history = history::merge_files(&[linked_lines, configured_lines], None);
                let records = history
                    .iter()
                    .filter(|line| line.id == legacy_id)
                    .collect::<Vec<_>>();
                assert_eq!(
                    records.len(),
                    1,
                    "linked history must expose one message after recovery"
                );
                assert_eq!(
                    records[0].deliveries[0].state,
                    cyclops_proto::DeliveryState::AttentionRequired,
                    "the terminal root fact must dominate the linked submitted copy"
                );
                let gating = history
                    .iter()
                    .filter(|line| line.id == skewed_id)
                    .collect::<Vec<_>>();
                assert_eq!(gating.len(), 1, "the skewed chain remains one message");
                assert_eq!(
                    gating[0].deliveries[0].state,
                    cyclops_proto::DeliveryState::RetryQueued,
                    "the root retry fact must follow the future-dated linked gating fact"
                );
            };
        let mut message = daemon_line(Kind::Msg, legacy_id.into(), json!({"hosted": ["%0"]}));
        message.from = "admin".into();
        message.to = vec!["%0".into()];
        message.subject = Some("linked before restart".into());
        message.body = Some("body".into());
        message.deliveries = vec![cyclops_proto::Delivery {
            to: "%0".into(),
            state: cyclops_proto::DeliveryState::Submitted,
            verified_by: None,
            attempts: 1,
            ts: unix_ms(),
            cause: None,
        }];
        linked.append(message).unwrap();
        let future_ms = unix_ms().saturating_add(86_400_000);
        let mut gating = daemon_line(Kind::Msg, skewed_id.into(), json!({"hosted": ["%1"]}));
        gating.ts = future_ms;
        gating.from = "admin".into();
        gating.to = vec!["%1".into()];
        gating.subject = Some("future-dated before restart".into());
        gating.body = Some("body".into());
        gating.deliveries = vec![cyclops_proto::Delivery {
            to: "%1".into(),
            state: cyclops_proto::DeliveryState::Gating,
            verified_by: None,
            attempts: 1,
            ts: future_ms,
            cause: None,
        }];
        linked.append(gating).unwrap();
        configured
            .append(daemon_line(
                Kind::System,
                "e-alias".into(),
                json!({
                    "event": "session_slot_aliased",
                    "session": "research",
                    "canonical_session_idx": 1,
                    "canonical_journal": "runtime.ndjson",
                }),
            ))
            .unwrap();
        drop(linked);
        drop(configured);
        drop(state_root);

        let mut cfg = Config::defaults(&home);
        cfg.sessions = vec!["research".into()];
        let daemon = boot(cfg.clone()).await.unwrap();
        assert_eq!(
            daemon
                .inner
                .engine
                .mint_msg_id_from(&[legacy_id, skewed_id, "m-fedcba"]),
            "m-fedcba",
            "an id visible through linked history must be rejected by the real mint path"
        );

        let lines = daemon
            .inner
            .session(0)
            .unwrap()
            .ledger
            .read_after(0)
            .unwrap();
        let settlements = lines
            .iter()
            .filter(|line| {
                line.id == legacy_id
                    && line.kind == Kind::State
                    && line.data.as_ref().is_some_and(|data| {
                        data["to_state"] == "attention_required"
                            && data["cause"] == "daemon_restart"
                    })
            })
            .count();
        assert_eq!(settlements, 1, "linked in-flight chain must settle once");
        let retries = lines
            .iter()
            .filter(|line| {
                line.id == skewed_id
                    && line.kind == Kind::State
                    && line.data.as_ref().is_some_and(|data| {
                        data["to_state"] == "retry_queued" && data["cause"] == "daemon_restart"
                    })
            })
            .count();
        assert_eq!(retries, 1, "linked gating chain must requeue once");
        let linked_lines = cyclops_ledger::read_after(
            &daemon.inner.state_root,
            Path::new("ledger/runtime.ndjson"),
            0,
        )
        .unwrap();
        assert_terminal_history(lines, linked_lines);

        daemon.shutdown().await;
        drop(daemon);

        let daemon = boot(cfg).await.unwrap();
        let configured_lines = daemon
            .inner
            .session(0)
            .unwrap()
            .ledger
            .read_after(0)
            .unwrap();
        let settlements = configured_lines
            .iter()
            .filter(|line| {
                line.id == legacy_id
                    && line.kind == Kind::State
                    && line.data.as_ref().is_some_and(|data| {
                        data["to_state"] == "attention_required"
                            && data["cause"] == "daemon_restart"
                    })
            })
            .count();
        assert_eq!(
            settlements, 1,
            "a linked chain already settled into its root must not settle again"
        );
        let retries = configured_lines
            .iter()
            .filter(|line| {
                line.id == skewed_id
                    && line.kind == Kind::State
                    && line.data.as_ref().is_some_and(|data| {
                        data["to_state"] == "retry_queued" && data["cause"] == "daemon_restart"
                    })
            })
            .count();
        assert_eq!(
            retries, 1,
            "future timestamps must not make a linked gating fact requeue twice"
        );
        let linked_lines = cyclops_ledger::read_after(
            &daemon.inner.state_root,
            Path::new("ledger/runtime.ndjson"),
            0,
        )
        .unwrap();
        assert_terminal_history(configured_lines, linked_lines);

        daemon.shutdown().await;
        drop(daemon);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn session_and_label_renames_keep_durable_routing() {
        let mut inner = bare_inner("cyc-mailbox-renames");
        enable_mailbox(&mut inner);
        let pane: TmuxPaneId = "%1".parse().unwrap();
        let root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let expected = RecipientKey::agent(inner.workspace_id, binding.session_instance_id(), pane);
        let slot = add_detached_route(
            &inner,
            "before",
            test_pane(&pane.to_string(), root.pid),
            binding,
        );
        set_test_label(&inner, "before", pane, root.pid, "reviewer");

        rename_session_slot(&inner, 0, "after".into());
        assert_eq!(slot.name(), "after");
        assert_eq!(
            inner
                .mailbox
                .as_ref()
                .unwrap()
                .agent_for_pane(pane)
                .unwrap()
                .unwrap()
                .key,
            expected
        );

        set_test_label(&inner, "after", pane, root.pid, "driver");
        let service = inner.mailbox.as_ref().unwrap();
        assert!(send_test_message(service, "reviewer".into()).is_err());
        send_test_message(service, "driver".into()).unwrap();

        inner
            .registry
            .lock()
            .unwrap()
            .clear(
                expected,
                ProcessInstanceId::new(root.pid, root.birth).unwrap(),
            )
            .unwrap();
        refresh_mailbox_directory(&inner);
        assert!(send_test_message(service, "driver".into()).is_err());
        assert!(send_test_message(service, pane.to_string()).is_err());
    }

    #[test]
    fn two_prevalidated_adoptions_cannot_publish_one_label_or_clear_the_directory() {
        let mut inner = bare_inner("cyc-atomic-pane-label");
        enable_mailbox(&mut inner);
        let root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let pane_a: TmuxPaneId = "%1".parse().unwrap();
        let pane_b: TmuxPaneId = "%2".parse().unwrap();
        let binding_a =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let binding_b =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 901, 2000, "$1"));
        let recipient_a =
            RecipientKey::agent(inner.workspace_id, binding_a.session_instance_id(), pane_a);
        let recipient_b =
            RecipientKey::agent(inner.workspace_id, binding_b.session_instance_id(), pane_b);
        add_detached_route(&inner, "a", test_pane("%1", root.pid), binding_a);
        add_detached_route(&inner, "b", test_pane("%2", root.pid), binding_b);
        let pane_root = ProcessInstanceId::new(root.pid, root.birth).unwrap();
        let adoption =
            |session: &str, pane: TmuxPaneId, recipient: RecipientKey| registry::Adoption {
                session: session.into(),
                pane_id: pane.to_string(),
                label: "worker".into(),
                recipient: Some(recipient),
                pane_root: Some(pane_root),
                manifest: None,
                pane_pid: root.pid,
                window_id: format!("@{}", pane.number()),
                border_format: None,
            };
        let window = |session: &str, pane: TmuxPaneId| registry::WindowChrome {
            session: session.into(),
            window_id: format!("@{}", pane.number()),
            border_status: None,
        };

        let _publication = inner.mailbox_publication.lock().unwrap();
        commit_adoption_under_publication(
            &inner,
            adoption("a", pane_a, recipient_a),
            window("a", pane_a),
        )
        .unwrap();
        let refused = commit_adoption_under_publication(
            &inner,
            adoption("b", pane_b, recipient_b),
            window("b", pane_b),
        )
        .unwrap_err();
        assert_eq!(refused.code, "bad_request");
        assert!(refused.message.contains("already taken"));
        drop(_publication);

        let adoptions = inner.registry.lock().unwrap().exact_adoptions();
        assert_eq!(adoptions.len(), 1);
        assert_eq!(adoptions[0].recipient, Some(recipient_a));
        let service = inner.mailbox.as_ref().unwrap();
        assert_eq!(
            service.agent_for_pane(pane_a).unwrap().unwrap().key,
            recipient_a
        );
        assert!(service.agent_for_pane(pane_b).unwrap().is_none());
        send_test_message(service, "worker".into()).unwrap();
    }

    #[test]
    fn mailbox_directory_and_broadcast_include_only_adopted_panes() {
        let mut inner = bare_inner("cyc-mailbox-adopted-only");
        enable_mailbox(&mut inner);
        let root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let adopted: TmuxPaneId = "%1".parse().unwrap();
        let unadopted: TmuxPaneId = "%2".parse().unwrap();
        let slot = add_detached_route(
            &inner,
            "main",
            test_pane(&adopted.to_string(), root.pid),
            binding,
        );
        let observed = ObservedPane::capture(test_pane(&unadopted.to_string(), root.pid));
        slot.last_panes
            .lock()
            .unwrap()
            .insert(observed.row.pane_id.clone(), observed);
        set_test_label(&inner, "main", adopted, root.pid, "reviewer");

        let service = inner.mailbox.as_ref().unwrap();
        let broadcast = send_test_message(service, "*".into()).unwrap();
        assert_eq!(broadcast.recipient_keys.len(), 1);
        assert_eq!(
            broadcast.recipient_keys[0].pane_id(),
            Some(adopted),
            "broadcast escaped the adopted roster"
        );
        assert!(service.agent_for_pane(unadopted).unwrap().is_none());
        assert!(send_test_message(service, unadopted.to_string()).is_err());
    }

    #[test]
    fn exact_adoptions_with_the_same_pane_id_coexist_in_broadcasts() {
        let mut inner = bare_inner("cyc-mailbox-same-pane-id");
        enable_mailbox(&mut inner);
        let root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let pane: TmuxPaneId = "%1".parse().unwrap();
        let first =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let first_key = RecipientKey::agent(inner.workspace_id, first.session_instance_id(), pane);
        add_detached_route(
            &inner,
            "first",
            test_pane(&pane.to_string(), root.pid),
            first,
        );
        set_test_label(&inner, "first", pane, root.pid, "reviewer");

        let second =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 901, 2000, "$1"));
        let second_key =
            RecipientKey::agent(inner.workspace_id, second.session_instance_id(), pane);
        add_detached_route(
            &inner,
            "second",
            test_pane(&pane.to_string(), root.pid),
            second,
        );
        set_test_label(&inner, "second", pane, root.pid, "implementer");

        let service = inner.mailbox.as_ref().unwrap();
        let broadcast = send_test_message(service, "*".into()).unwrap();
        assert_eq!(
            broadcast
                .recipient_keys
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from([first_key, second_key])
        );
    }

    #[test]
    fn replacement_route_does_not_inherit_a_stale_manifest_pin() {
        let mut inner = bare_inner("cyc-stale-manifest-pin");
        let auto = Manifest::parse(
            r#"
[agent]
id = "auto"
display_name = "Auto"
process_names = ["test"]
"#,
            Path::new("auto.toml"),
        )
        .unwrap();
        let pinned = Manifest::parse(
            r#"
[agent]
id = "pinned"
display_name = "Pinned"
process_names = ["never"]
"#,
            Path::new("pinned.toml"),
        )
        .unwrap();
        Arc::get_mut(&mut inner)
            .unwrap()
            .manifests
            .extend([("auto".into(), auto), ("pinned".into(), pinned)]);
        let pane: TmuxPaneId = "%1".parse().unwrap();
        let root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let old_binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let old_recipient =
            RecipientKey::agent(inner.workspace_id, old_binding.session_instance_id(), pane);
        add_detached_route(
            &inner,
            "old",
            test_pane(&pane.to_string(), root.pid),
            old_binding,
        );
        inner
            .registry
            .lock()
            .unwrap()
            .adopt(
                registry::Adoption {
                    session: "old".into(),
                    pane_id: pane.to_string(),
                    label: "reviewer".into(),
                    recipient: Some(old_recipient),
                    pane_root: Some(ProcessInstanceId::new(root.pid, root.birth).unwrap()),
                    manifest: Some("pinned".into()),
                    pane_pid: root.pid,
                    window_id: "@1".into(),
                    border_format: None,
                },
                registry::WindowChrome {
                    session: "old".into(),
                    window_id: "@1".into(),
                    border_status: None,
                },
            )
            .unwrap();

        let new_binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 901, 2000, "$1"));
        let new_recipient =
            RecipientKey::agent(inner.workspace_id, new_binding.session_instance_id(), pane);
        let new_slot = add_detached_route(
            &inner,
            "new",
            test_pane(&pane.to_string(), root.pid),
            new_binding,
        );
        new_slot.link.lock().unwrap().attached = true;
        let row = new_slot.last_panes.lock().unwrap()[&pane.to_string()]
            .row
            .clone();

        assert_eq!(
            fusion::bind_manifest_for(&inner, 1, &row).map(|manifest| manifest.agent.id.as_str()),
            Some("auto")
        );
        let pane_root = ProcessInstanceId::new(root.pid, root.birth).unwrap();
        assert!(
            inner
                .adoption_for_observed_route(new_recipient, &row.pane_id, pane_root)
                .is_none(),
            "a different tmux server generation is not the same physical route"
        );

        let moved_binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$2"));
        let moved_recipient = RecipientKey::agent(
            inner.workspace_id,
            moved_binding.session_instance_id(),
            pane,
        );
        let moved_slot = add_detached_route(
            &inner,
            "moved",
            test_pane(&pane.to_string(), root.pid),
            moved_binding,
        );
        moved_slot.link.lock().unwrap().attached = true;
        let moved_row = moved_slot.last_panes.lock().unwrap()[&pane.to_string()]
            .row
            .clone();
        assert_eq!(
            fusion::bind_manifest_for(&inner, 2, &moved_row)
                .map(|manifest| manifest.agent.id.as_str()),
            Some("pinned"),
            "a pane transfer within one tmux server keeps its explicit pin"
        );
        assert_eq!(
            inner
                .adoption_for_observed_route(moved_recipient, &moved_row.pane_id, pane_root)
                .and_then(|adoption| adoption.manifest),
            Some("pinned".into())
        );
    }

    #[test]
    fn concurrent_attach_publications_keep_both_routes() {
        let mut inner = bare_inner("cyc-mailbox-concurrent-attach");
        enable_mailbox(&mut inner);
        let root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let binding_a =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let binding_b =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$2"));
        let slot_a = add_detached_route(&inner, "a", test_pane("%1", root.pid), binding_a.clone());
        let slot_b = add_detached_route(&inner, "b", test_pane("%2", root.pid), binding_b.clone());
        set_test_label(&inner, "a", "%1".parse().unwrap(), root.pid, "first");
        set_test_label(&inner, "b", "%2".parse().unwrap(), root.pid, "second");
        slot_a.link.lock().unwrap().identity = None;
        slot_b.link.lock().unwrap().identity = None;

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let release = Arc::new(std::sync::Barrier::new(2));
        *inner.mailbox_publish_pause.lock().unwrap() = Some(MailboxPublishPause {
            entered: entered_tx,
            release: Arc::clone(&release),
        });
        let rows_a: Vec<_> = slot_a
            .last_panes
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let rows_b: Vec<_> = slot_b
            .last_panes
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let inner_a = Arc::clone(&inner);
            let slot_a = Arc::clone(&slot_a);
            let attach_a = scope.spawn(move || {
                let route = MailboxRouteOverride {
                    session_idx: 0,
                    instance_id: binding_a.session_instance_id(),
                    rows: &rows_a,
                };
                publish_mailbox_transition(&inner_a, &route, || {
                    slot_a.link.lock().unwrap().identity = Some(binding_a);
                })
                .unwrap();
            });
            entered_rx.recv().unwrap();

            let inner_b = Arc::clone(&inner);
            let slot_b = Arc::clone(&slot_b);
            let attach_b = scope.spawn(move || {
                let route = MailboxRouteOverride {
                    session_idx: 1,
                    instance_id: binding_b.session_instance_id(),
                    rows: &rows_b,
                };
                publish_mailbox_transition(&inner_b, &route, || {
                    slot_b.link.lock().unwrap().identity = Some(binding_b);
                })
                .unwrap();
                done_tx.send(()).unwrap();
            });
            let overlapped = done_rx.recv_timeout(Duration::from_millis(500)).is_ok();
            release.wait();
            attach_a.join().unwrap();
            attach_b.join().unwrap();
            assert!(!overlapped, "mailbox publications overlapped");
        });

        let service = inner.mailbox.as_ref().unwrap();
        assert!(service
            .agent_for_pane("%1".parse().unwrap())
            .unwrap()
            .is_some());
        assert!(service
            .agent_for_pane("%2".parse().unwrap())
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_paused_snapshot_cannot_resurrect_a_removed_route() {
        let mut inner = bare_inner("cyc-mailbox-stale-removal");
        enable_mailbox(&mut inner);
        let root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let slot = add_detached_route(&inner, "a", test_pane("%1", root.pid), binding);
        refresh_mailbox_directory(&inner);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let release = Arc::new(std::sync::Barrier::new(2));
        *inner.mailbox_publish_pause.lock().unwrap() = Some(MailboxPublishPause {
            entered: entered_tx,
            release: Arc::clone(&release),
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let stale_inner = Arc::clone(&inner);
            let stale = scope.spawn(move || refresh_mailbox_directory(&stale_inner));
            entered_rx.recv().unwrap();
            slot.last_panes.lock().unwrap().clear();

            let current_inner = Arc::clone(&inner);
            let current = scope.spawn(move || {
                refresh_mailbox_directory(&current_inner);
                done_tx.send(()).unwrap();
            });
            let overlapped = done_rx.recv_timeout(Duration::from_millis(500)).is_ok();
            release.wait();
            stale.join().unwrap();
            current.join().unwrap();
            assert!(!overlapped, "mailbox publications overlapped");
        });

        assert!(inner
            .mailbox
            .as_ref()
            .unwrap()
            .agent_for_pane("%1".parse().unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn attached_replacement_wins_a_reused_pane_id_without_hiding_old_mail() {
        let mut inner = bare_inner("cyc-mailbox-reused-pane-id");
        enable_mailbox(&mut inner);
        let pane: TmuxPaneId = "%1".parse().unwrap();
        let live_root = identity::ProcId::of(std::process::id() as i32).unwrap();
        let stale_root = identity::ProcId {
            pid: live_root.pid,
            birth: live_root.birth.checked_sub(1).unwrap_or(1),
        };
        let old_binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1000, "$1"));
        let old_key =
            RecipientKey::agent(inner.workspace_id, old_binding.session_instance_id(), pane);
        let old_slot =
            add_detached_route(&inner, "old", test_pane("%1", live_root.pid), old_binding);
        set_test_label(&inner, "old", pane, live_root.pid, "old-worker");
        refresh_mailbox_directory(&inner);
        send_test_message(inner.mailbox.as_ref().unwrap(), "old-worker".into()).unwrap();
        old_slot
            .last_panes
            .lock()
            .unwrap()
            .get_mut("%1")
            .unwrap()
            .root = Some(stale_root);
        refresh_mailbox_directory(&inner);

        let new_binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 901, 2000, "$1"));
        let new_key =
            RecipientKey::agent(inner.workspace_id, new_binding.session_instance_id(), pane);
        let new_slot =
            add_detached_route(&inner, "new", test_pane("%1", live_root.pid), new_binding);
        let panes = std::mem::take(&mut *new_slot.last_panes.lock().unwrap());
        {
            let mut link = new_slot.link.lock().unwrap();
            link.attached = true;
            link.mailbox_panes = panes;
        }
        refresh_mailbox_directory(&inner);

        let service = inner.mailbox.as_ref().unwrap();
        set_test_label(&inner, "new", pane, live_root.pid, "reviewer");
        let routed = service.agent_for_pane(pane).unwrap().unwrap();
        assert_eq!(routed.key, new_key);
        assert_eq!(routed.label, "reviewer");
        assert_eq!(
            send_test_message(service, "reviewer".into())
                .unwrap()
                .recipients,
            vec!["reviewer"]
        );
        assert_eq!(
            mailbox_recipient_for_origin(&inner, pane, live_root),
            Some(new_key)
        );
        assert_eq!(mailbox_recipient_for_origin(&inner, pane, stale_root), None);
        assert_eq!(service.list(old_key, None, None).unwrap().len(), 1);
        assert_eq!(service.list(new_key, None, None).unwrap().len(), 1);
    }

    #[test]
    fn composite_cursor_survives_rename_and_rejects_a_reused_name() {
        let inner = bare_inner("cyc-history-cursor-rename");
        let old_ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/before.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        let old = Arc::new(SessionSlot::new("before".into(), Arc::new(old_ledger)));
        for id in ["m-first", "m-second"] {
            let mut line = daemon_line(Kind::Msg, id.into(), Value::Null);
            line.from = "admin".into();
            line.to = vec!["agent".into()];
            old.ledger.append(line).unwrap();
        }
        let side_ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/side.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        inner.sessions.lock().unwrap().extend([
            Arc::clone(&old),
            Arc::new(SessionSlot::new("side".into(), Arc::new(side_ledger))),
        ]);
        let params = cyclops_proto::HistoryParams {
            with: None,
            from: None,
            to: None,
            limit: 1,
            cursor: None,
        };

        let first =
            history::msg_history(&inner, params.clone(), Some(String::new()), None).unwrap();
        assert_eq!(first["lines"][0]["id"], "m-first");
        let cursor = first["next_cursor2"].as_str().unwrap().to_string();
        old.rename("after".into());

        let second = history::msg_history(&inner, params.clone(), Some(cursor), None).unwrap();
        assert_eq!(second["lines"][0]["id"], "m-second");
        let cursor = second["next_cursor2"].as_str().unwrap().to_string();

        let replacement_ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/replacement.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        let replacement = Arc::new(SessionSlot::new(
            "before".into(),
            Arc::new(replacement_ledger),
        ));
        let mut line = daemon_line(Kind::Msg, "m-replacement".into(), Value::Null);
        line.from = "admin".into();
        line.to = vec!["agent".into()];
        replacement.ledger.append(line).unwrap();
        inner.sessions.lock().unwrap().push(replacement);

        let stale = history::msg_history(&inner, params, Some(cursor), None).unwrap_err();
        assert_eq!(stale.code, "bad_request");
    }

    async fn wait_for_session_binding(
        slot: &Arc<SessionSlot>,
        previous: Option<SessionInstanceId>,
    ) -> SessionIdentityBinding {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let binding = slot
                    .link
                    .lock()
                    .unwrap()
                    .identity
                    .clone()
                    .filter(|binding| {
                        previous.is_none_or(|id| binding.session_instance_id() != id)
                    });
                if let Some(binding) = binding {
                    return binding;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("session identity was not published")
    }

    fn assert_current_mailbox_route(
        inner: &Inner,
        slot: &SessionSlot,
        binding: &SessionIdentityBinding,
    ) {
        let row = slot
            .link
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .unwrap()
            .snapshot()[0]
            .clone();
        let pane: TmuxPaneId = row.pane_id.parse().unwrap();
        let root = identity::ProcId::of(row.pane_pid).unwrap();
        inner
            .registry
            .lock()
            .unwrap()
            .adopt(
                registry::Adoption {
                    session: slot.name(),
                    pane_id: row.pane_id,
                    label: "worker".into(),
                    recipient: Some(RecipientKey::agent(
                        inner.workspace_id,
                        binding.session_instance_id(),
                        pane,
                    )),
                    pane_root: Some(ProcessInstanceId::new(root.pid, root.birth).unwrap()),
                    manifest: None,
                    pane_pid: row.pane_pid,
                    window_id: row.window_id.clone(),
                    border_format: None,
                },
                registry::WindowChrome {
                    session: slot.name(),
                    window_id: row.window_id,
                    border_status: None,
                },
            )
            .unwrap();
        refresh_mailbox_directory(inner);
        let routed = inner
            .mailbox
            .as_ref()
            .unwrap()
            .agent_for_pane(pane)
            .unwrap()
            .unwrap();
        assert_eq!(
            routed.key,
            RecipientKey::agent(inner.workspace_id, binding.session_instance_id(), pane)
        );
    }

    #[test]
    fn process_replacement_retires_current_trust_but_keeps_exact_hook_history() {
        let inner = bare_inner("cyc-process-trust-retirement");
        let pane = PaneKey::new(0, "%1");
        let agent = identity::ProcId::of(std::process::id() as i32).unwrap();
        let reading = cyclops_proto::SensorReading {
            sensor: cyclops_proto::Sensor::Hook,
            state: AgentState::Working,
            rule: "turn_start".into(),
            ts: 1,
        };
        inner.detections.lock().unwrap().insert(
            pane.clone(),
            DetEntry {
                detection: Detection {
                    state: AgentState::Working,
                    readings: vec![reading.clone()],
                    disagreement: false,
                    decided_by: "hook".into(),
                    unknown_reason: None,
                    stale: false,
                    write_ready: false,
                    write_block: Some("agent_working".into()),
                    composer_semantic: None,
                },
                binding: None,
                manifest: Some("test".into()),
                occupant: Some(agent.pid),
                agent: Some(agent),
                in_mode: false,
                quota_screen_clear: false,
                hold: cyclops_proto::ComposerHold::Staged,
                turn: Some(turnkey::TurnKey::for_test(&["turn-1"])),
                hold_owner: Some("m-old".into()),
                composer: ComposerProjection::default(),
                working_confirmed: true,
                since: Instant::now(),
            },
        );
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            fusion::HookEntry::bound(agent, Some("test".into()), reading),
        );
        let turn = turnkey::TurnKey::for_test(&["turn-1"]);
        turnkey::PaneEnds::record(
            &mut inner.turn_ends.lock().unwrap(),
            &pane,
            agent,
            "test",
            turn.clone(),
        );
        inner
            .argv_cache
            .lock()
            .unwrap()
            .insert((pane.clone(), agent), "test-agent".into());
        inner.hook_liveness.open(&pane);
        let _ = inner
            .hook_liveness
            .bind_diagnostic(&pane, "Stop", 1, agent, "test");
        let reserved = inner
            .hook_liveness
            .binding(&pane, agent, "reserved-manifest")
            .expect("live reserved binding");
        assert!(inner.hook_liveness.reserve_f1_if_no_edges(&reserved));

        retire_pane_process_trust(&inner, 0, "%1");

        assert!(!inner.detections.lock().unwrap().contains_key(&pane));
        assert!(!inner.hook_readings.lock().unwrap().contains_key(&pane));
        assert!(!inner.turn_ends.lock().unwrap().contains_key(&pane));
        assert!(!inner
            .argv_cache
            .lock()
            .unwrap()
            .keys()
            .any(|(cached, _)| cached == &pane));
        assert!(inner.hook_liveness.seen_any(&pane, agent, "test"));
        assert!(
            !inner.hook_liveness.reserve_f1_if_no_edges(&reserved),
            "process replacement cannot reset the old generation's one-shot"
        );
        let replacement = identity::ProcId {
            pid: agent.pid + 1,
            birth: agent.birth + 1,
        };
        assert!(!inner.hook_liveness.seen_any(&pane, replacement, "test"));
        let replacement_binding = inner
            .hook_liveness
            .binding(&pane, replacement, "test")
            .expect("replacement shares the live pane lifetime");
        assert!(
            inner
                .hook_liveness
                .reserve_f1_if_no_edges(&replacement_binding),
            "replacement process gets an independent one-shot"
        );
    }

    #[test]
    fn a_rootless_process_observation_cannot_replace_a_route() {
        let row = test_pane("%1", i32::MAX);
        let authoritative = ObservedPane {
            row: row.clone(),
            root: None,
        };
        let replacement = ObservedPane { row, root: None };

        assert_eq!(
            validated_replacement_generation(&authoritative, &replacement),
            Err("pane_process_generation_unproven")
        );
    }

    #[test]
    fn stale_reconnect_rebind_cannot_retire_the_current_occupants_barrier() {
        use cyclops_proto::{
            NotificationBinding, NotificationManifestId, NotificationState, NotificationTransport,
            DOORBELL_FORMAT_COMPACT_CLAIM,
        };

        let mut inner = bare_inner("cyc-stale-rebind-barrier");
        enable_mailbox(&mut inner);
        let pane: TmuxPaneId = "%1".parse().unwrap();
        let old_process = identity::ProcId::of(std::process::id() as i32).unwrap();
        let binding =
            persist_test_binding(&inner, test_live_key(inner.workspace_id, 900, 1_000, "$1"));
        add_detached_route(
            &inner,
            "main",
            test_pane("%1", old_process.pid),
            binding.clone(),
        );
        set_test_label(&inner, "main", pane, old_process.pid, "worker");
        let recipient =
            RecipientKey::agent(inner.workspace_id, binding.session_instance_id(), pane);
        let existing = inner.registry.lock().unwrap().in_session("main");
        let old_root = ProcessInstanceId::new(old_process.pid, old_process.birth).unwrap();
        let current_root =
            ProcessInstanceId::new(old_process.pid, old_process.birth.checked_add(1).unwrap())
                .unwrap();
        assert!(inner
            .registry
            .lock()
            .unwrap()
            .rebind_process(recipient, old_root, current_root)
            .unwrap());

        let service = inner.mailbox.as_ref().unwrap();
        let accepted = send_test_message(service, "worker".into()).unwrap();
        let queued = service
            .prepare_oldest_notification(recipient)
            .unwrap()
            .unwrap();
        let durable_binding = NotificationBinding {
            recipient,
            pane_root: Some(current_root),
            leader: Some(current_root),
            agent: current_root,
            manifest: NotificationManifestId::new("test").unwrap(),
        };
        let store = service.store_handle();
        {
            let mut store = store.lock().unwrap();
            store
                .advance_notification(
                    accepted.message_id.clone(),
                    recipient,
                    queued.attempt_id,
                    NotificationState::Gating,
                    None,
                    None,
                )
                .unwrap();
            store
                .advance_notification_with_transport(
                    accepted.message_id,
                    recipient,
                    queued.attempt_id,
                    NotificationState::Writing,
                    durable_binding,
                    NotificationTransport::Doorbell,
                    Some(DOORBELL_FORMAT_COMPACT_CLAIM),
                )
                .unwrap();
        }
        assert_eq!(service.active_notification_barriers().unwrap().len(), 1);

        let mut replacement = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let observed = vec![ObservedPane::capture(test_pane(
            "%1",
            replacement.id() as i32,
        ))];
        let error = rebind_same_session_adoptions(
            &inner,
            0,
            binding.session_instance_id(),
            &observed,
            &existing,
        )
        .unwrap_err();

        assert_eq!(error, "adoption_rebind_route_changed");
        assert_eq!(service.active_notification_barriers().unwrap().len(), 1);
        assert!(inner
            .registry
            .lock()
            .unwrap()
            .for_route(recipient, current_root)
            .is_some());
        replacement.kill().unwrap();
        replacement.wait().unwrap();
    }

    #[tokio::test]
    async fn an_unadopted_same_command_respawn_stays_attached() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let tmux = cyclops_testrig::TmuxServer::new("unadopted-respawn-route");
        tmux.run_ok(&["new-session", "-d", "-s", "main", "sleep 60"]);
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-unadopted-respawn-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cfg = Config::defaults(&home);
        cfg.sessions.push("main".into());
        cfg.tmux_socket = Some(tmux.socket().to_string());
        cfg.tmux_config = Some(PathBuf::from("/dev/null"));
        let daemon = boot(cfg).await.unwrap();
        let slot = daemon.inner.session(0).unwrap();
        wait_for_session_binding(&slot, None).await;
        let watcher = slot.link.lock().unwrap().watcher.clone().unwrap();
        let first = watcher.snapshot()[0].clone();

        tmux.run_ok(&["respawn-pane", "-k", "-t", &first.pane_id, "sleep 60"]);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = watcher.pane(&first.pane_id).unwrap();
                let replacement_is_published = {
                    let link = slot.link.lock().unwrap();
                    row.pane_pid != first.pane_pid
                        && link.attached
                        && link
                            .mailbox_panes
                            .get(&first.pane_id)
                            .is_some_and(|pane| pane.row.pane_pid == row.pane_pid)
                };
                if replacement_is_published {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("unadopted replacement did not remain attached");

        let pane: TmuxPaneId = first.pane_id.parse().unwrap();
        assert!(daemon
            .inner
            .mailbox
            .as_ref()
            .unwrap()
            .agent_for_pane(pane)
            .unwrap()
            .is_none());
        daemon.shutdown().await;
        drop(daemon);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn respawn_rebinds_the_durable_name_and_mailbox_process_generation() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let tmux = cyclops_testrig::TmuxServer::new("mailbox-respawn-route");
        tmux.run_ok(&["new-session", "-d", "-s", "main", "sleep 60"]);
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-mailbox-respawn-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cfg = Config::defaults(&home);
        cfg.sessions.push("main".into());
        cfg.tmux_socket = Some(tmux.socket().to_string());
        cfg.tmux_config = Some(PathBuf::from("/dev/null"));
        let daemon = boot(cfg).await.unwrap();
        let slot = daemon.inner.session(0).unwrap();
        let binding = wait_for_session_binding(&slot, None).await;
        let watcher = slot
            .link
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .map(Arc::clone)
            .unwrap();
        let first = watcher.snapshot()[0].clone();
        let pane: TmuxPaneId = first.pane_id.parse().unwrap();
        let first_root = identity::ProcId::of(first.pane_pid).unwrap();
        let recipient = RecipientKey::agent(
            daemon.inner.workspace_id,
            binding.session_instance_id(),
            pane,
        );
        daemon
            .label_pane(&first.pane_id, Some("worker".into()), None)
            .await
            .unwrap();
        assert_eq!(
            daemon
                .inner
                .mailbox
                .as_ref()
                .unwrap()
                .agent_for_pane(pane)
                .unwrap()
                .unwrap()
                .label,
            "worker"
        );

        tmux.run_ok(&["respawn-pane", "-k", "-t", &first.pane_id, "sleep 60"]);
        let replacement = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = watcher.pane(&first.pane_id).unwrap();
                let route = slot
                    .link
                    .lock()
                    .unwrap()
                    .mailbox_panes
                    .get(&first.pane_id)
                    .cloned();
                let root = identity::ProcId::of(row.pane_pid);
                let registry_matches = root
                    .and_then(|root| ProcessInstanceId::new(root.pid, root.birth).ok())
                    .is_some_and(|root| {
                        daemon
                            .inner
                            .registry
                            .lock()
                            .unwrap()
                            .for_route(recipient, root)
                            .is_some()
                    });
                if row.pane_pid != first.pane_pid
                    && route.as_ref().and_then(|pane| pane.root) == root
                    && registry_matches
                {
                    break (row, root.unwrap());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("replacement route was not published");

        assert_eq!(
            mailbox_recipient_for_origin(&daemon.inner, pane, replacement.1),
            Some(recipient)
        );
        assert_eq!(
            mailbox_recipient_for_origin(&daemon.inner, pane, first_root),
            None
        );
        let identity = daemon
            .inner
            .mailbox
            .as_ref()
            .unwrap()
            .agent_for_pane(pane)
            .unwrap()
            .unwrap();
        assert_eq!(identity.key, recipient);
        assert_eq!(identity.label, "worker");
        let (reopened, warnings) = registry::Registry::load(Arc::clone(&daemon.inner.state_root));
        assert!(warnings.is_empty(), "{warnings:?}");
        let replacement_root =
            ProcessInstanceId::new(replacement.1.pid, replacement.1.birth).unwrap();
        assert_eq!(
            reopened
                .for_route(recipient, replacement_root)
                .map(|adoption| adoption.label.as_str()),
            Some("worker")
        );

        daemon.shutdown().await;
        drop(daemon);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn lag_reconciliation_repairs_a_missed_pane_root_edge() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let tmux = cyclops_testrig::TmuxServer::new("mailbox-lagged-respawn");
        tmux.run_ok(&["new-session", "-d", "-s", "main", "sleep 60"]);
        let home =
            cyclops_proto::scratch::scratch_dir(&format!("cyc-mbox-lag-{}", uuid::Uuid::new_v4()));
        let mut cfg = Config::defaults(&home);
        cfg.sessions.push("main".into());
        cfg.tmux_socket = Some(tmux.socket().to_string());
        cfg.tmux_config = Some(PathBuf::from("/dev/null"));
        let daemon = boot(cfg).await.unwrap();
        let slot = daemon.inner.session(0).unwrap();
        let binding = wait_for_session_binding(&slot, None).await;
        let watcher = slot.link.lock().unwrap().watcher.clone().unwrap();
        let first = watcher.snapshot()[0].clone();
        let pane: TmuxPaneId = first.pane_id.parse().unwrap();
        let recipient = RecipientKey::agent(
            daemon.inner.workspace_id,
            binding.session_instance_id(),
            pane,
        );
        daemon
            .label_pane(&first.pane_id, Some("worker".into()), None)
            .await
            .unwrap();
        let old_process = identity::ProcId::of(first.pane_pid).unwrap();
        let old_root = ProcessInstanceId::new(old_process.pid, old_process.birth).unwrap();

        tmux.run_ok(&["respawn-pane", "-k", "-t", &first.pane_id, "sleep 60"]);
        let replacement = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = watcher.pane(&first.pane_id).unwrap();
                let Some(root) = identity::ProcId::of(row.pane_pid) else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                };
                let new_root = ProcessInstanceId::new(root.pid, root.birth).unwrap();
                if new_root != old_root
                    && daemon
                        .inner
                        .registry
                        .lock()
                        .unwrap()
                        .for_route(recipient, new_root)
                        .is_some()
                {
                    break (row, new_root);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("initial replacement was not observed");

        let route_before_stale = slot
            .link
            .lock()
            .unwrap()
            .mailbox_panes
            .get(&first.pane_id)
            .cloned()
            .unwrap();
        assert_eq!(
            replace_pane_process(&daemon.inner, 0, &watcher, &first.pane_id, &first),
            Err("pane_process_event_stale")
        );
        let route_after_stale = slot
            .link
            .lock()
            .unwrap()
            .mailbox_panes
            .get(&first.pane_id)
            .cloned()
            .unwrap();
        assert_eq!(route_after_stale.root, route_before_stale.root);
        assert_eq!(route_after_stale.row, route_before_stale.row);
        assert!(daemon
            .inner
            .registry
            .lock()
            .unwrap()
            .for_route(recipient, replacement.1)
            .is_some());

        assert!(daemon
            .inner
            .registry
            .lock()
            .unwrap()
            .rebind_process(recipient, replacement.1, old_root)
            .unwrap());
        slot.link.lock().unwrap().mailbox_panes.insert(
            first.pane_id.clone(),
            ObservedPane {
                row: first.clone(),
                root: Some(old_process),
            },
        );

        reconcile_missed_pane_routes(&daemon.inner, 0, &watcher).unwrap();

        assert!(daemon
            .inner
            .registry
            .lock()
            .unwrap()
            .for_route(recipient, replacement.1)
            .is_some());
        assert_eq!(
            slot.link
                .lock()
                .unwrap()
                .mailbox_panes
                .get(&first.pane_id)
                .and_then(|pane| pane.root),
            Some(identity::ProcId {
                pid: replacement.1.pid(),
                birth: replacement.1.birth(),
            })
        );
        assert_eq!(
            daemon
                .inner
                .mailbox
                .as_ref()
                .unwrap()
                .agent_for_pane(pane)
                .unwrap()
                .unwrap()
                .label,
            "worker"
        );

        let mut events = daemon.subscribe_events();
        while events.try_recv().is_ok() {}
        assert_eq!(
            reconcile_missed_pane_routes(&daemon.inner, 0, &watcher).unwrap(),
            PaneRouteReconcile::default()
        );
        while let Ok(event) = events.try_recv() {
            assert_ne!(event.event, "messages.route_changed");
        }

        tmux.run_ok(&[
            "select-pane",
            "-t",
            &first.pane_id,
            "-T",
            "metadata-only-change",
        ]);
        watcher.reconcile_now().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let title = slot
                    .link
                    .lock()
                    .unwrap()
                    .mailbox_panes
                    .get(&first.pane_id)
                    .map(|pane| pane.row.title.clone());
                if title.as_deref() == Some("metadata-only-change") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("metadata-only reconcile was not stored");
        while let Ok(event) = events.try_recv() {
            assert_ne!(event.event, "messages.route_changed");
        }
        assert!(daemon
            .inner
            .registry
            .lock()
            .unwrap()
            .for_route(recipient, replacement.1)
            .is_some());

        daemon.shutdown().await;
        drop(daemon);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn reconnect_rebinds_a_respawn_missed_while_the_daemon_was_down() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let tmux = cyclops_testrig::TmuxServer::new("mailbox-offline-respawn");
        tmux.run_ok(&["new-session", "-d", "-s", "main", "sleep 60"]);
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-mbox-offline-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cfg = Config::defaults(&home);
        cfg.sessions.push("main".into());
        cfg.tmux_socket = Some(tmux.socket().to_string());
        cfg.tmux_config = Some(PathBuf::from("/dev/null"));

        let first_daemon = boot(cfg.clone()).await.unwrap();
        let first_slot = first_daemon.inner.session(0).unwrap();
        let first_binding = wait_for_session_binding(&first_slot, None).await;
        let first_row = first_slot
            .link
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .unwrap()
            .snapshot()[0]
            .clone();
        let pane: TmuxPaneId = first_row.pane_id.parse().unwrap();
        let recipient = RecipientKey::agent(
            first_daemon.inner.workspace_id,
            first_binding.session_instance_id(),
            pane,
        );
        let first_root = identity::ProcId::of(first_row.pane_pid).unwrap();
        first_daemon
            .label_pane(&first_row.pane_id, Some("worker".into()), None)
            .await
            .unwrap();
        first_daemon.shutdown().await;
        drop(first_daemon);

        tmux.run_ok(&["respawn-pane", "-k", "-t", &first_row.pane_id, "sleep 60"]);
        let second_daemon = boot(cfg).await.unwrap();
        let second_slot = second_daemon.inner.session(0).unwrap();
        let second_binding = wait_for_session_binding(&second_slot, None).await;
        assert_eq!(
            second_binding.session_instance_id(),
            first_binding.session_instance_id(),
            "the same live tmux session keeps its durable session identity"
        );

        let replacement = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let route = second_daemon
                    .inner
                    .mailbox
                    .as_ref()
                    .unwrap()
                    .agent_for_pane(pane)
                    .unwrap();
                let row = second_slot
                    .link
                    .lock()
                    .unwrap()
                    .watcher
                    .as_ref()
                    .unwrap()
                    .pane(&first_row.pane_id)
                    .unwrap();
                let root = identity::ProcId::of(row.pane_pid).unwrap();
                if row.pane_pid != first_row.pane_pid
                    && route.as_ref().is_some_and(|identity| {
                        identity.key == recipient && identity.label == "worker"
                    })
                {
                    break root;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("offline replacement route was not rebound on attach");

        assert_eq!(
            mailbox_recipient_for_origin(&second_daemon.inner, pane, replacement),
            Some(recipient)
        );
        assert_eq!(
            mailbox_recipient_for_origin(&second_daemon.inner, pane, first_root),
            None
        );
        let (reopened, warnings) =
            registry::Registry::load(Arc::clone(&second_daemon.inner.state_root));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            reopened
                .for_route(
                    recipient,
                    ProcessInstanceId::new(replacement.pid, replacement.birth).unwrap(),
                )
                .map(|adoption| adoption.label.as_str()),
            Some("worker")
        );

        second_daemon.shutdown().await;
        drop(second_daemon);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn live_rename_and_replacement_preserve_identity_boundaries() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let tmux = cyclops_testrig::TmuxServer::new("durable-session-wiring");
        tmux.run_ok(&["new-session", "-d", "-s", "before"]);
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-durable-session-{}",
            uuid::Uuid::new_v4()
        ));
        let mut cfg = Config::defaults(&home);
        cfg.sessions.push("before".into());
        cfg.tmux_socket = Some(tmux.socket().to_string());
        cfg.tmux_config = Some(PathBuf::from("/dev/null"));
        let daemon = boot(cfg).await.unwrap();
        let slot = daemon.inner.session(0).unwrap();
        let first = wait_for_session_binding(&slot, None).await;
        assert!(
            slot.link.lock().unwrap().attached,
            "watcher detached during bootstrap"
        );
        assert_current_mailbox_route(&daemon.inner, &slot, &first);
        let reopened = sessionstore::SessionIdentities::open(&daemon.inner.state_root).unwrap();
        assert_eq!(
            reopened.instance_of(first.live_session_key()),
            Some(first.session_instance_id())
        );

        tmux.run_ok(&["rename-session", "-t", "=before", "after"]);
        let renamed = tokio::time::timeout(Duration::from_secs(10), async {
            while slot.name() != "after" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if renamed.is_err() {
            let (attached, watched_name) = {
                let link = slot.link.lock().unwrap();
                (
                    link.attached,
                    link.watcher.as_ref().map(|watcher| watcher.session()),
                )
            };
            panic!(
                "session rename was not applied: slot={}, attached={attached}, watcher={watched_name:?}",
                slot.name()
            );
        }
        assert_eq!(
            slot.link
                .lock()
                .unwrap()
                .identity
                .as_ref()
                .unwrap()
                .session_instance_id(),
            first.session_instance_id()
        );

        drop(tmux);
        let tmux = cyclops_testrig::TmuxServer::new("durable-session-wiring");
        tmux.run_ok(&["new-session", "-d", "-s", "after"]);
        let second = wait_for_session_binding(&slot, Some(first.session_instance_id())).await;
        let second_pane: TmuxPaneId = slot
            .link
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .unwrap()
            .snapshot()[0]
            .pane_id
            .parse()
            .unwrap();
        assert!(daemon
            .inner
            .mailbox
            .as_ref()
            .unwrap()
            .agent_for_pane(second_pane)
            .unwrap()
            .is_none());
        assert_current_mailbox_route(&daemon.inner, &slot, &second);
        assert_ne!(
            second.live_session_key().tmux_server(),
            first.live_session_key().tmux_server()
        );

        tmux.run_ok(&["new-session", "-d", "-s", "anchor"]);
        tmux.run_ok(&["kill-session", "-t", "=after"]);
        tmux.run_ok(&["new-session", "-d", "-s", "after"]);
        let third = wait_for_session_binding(&slot, Some(second.session_instance_id())).await;
        let third_pane: TmuxPaneId = slot
            .link
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .unwrap()
            .snapshot()[0]
            .pane_id
            .parse()
            .unwrap();
        assert!(daemon
            .inner
            .mailbox
            .as_ref()
            .unwrap()
            .agent_for_pane(third_pane)
            .unwrap()
            .is_none());
        assert_current_mailbox_route(&daemon.inner, &slot, &third);
        assert_ne!(
            third.live_session_key().tmux_session_id(),
            second.live_session_key().tmux_session_id()
        );
        daemon.shutdown().await;
        drop(daemon);
        std::fs::remove_dir_all(home).unwrap();
    }

    /// A rename that lands on the daemon's own slot (`rename_session_slot`,
    /// the same call `handle_pane_event`'s `SessionRenamed` arm makes) must
    /// make `session_index` resolve the new name to the same slot. No
    /// tmux and no watcher needed to prove that bookkeeping. That is what
    /// lets a later `session.watch` for the new name dedup instead of
    /// opening a second slot and watcher for the one tmux session, which is
    /// the duplicate-watcher bug this feature exists to prevent.
    #[tokio::test]
    async fn a_renamed_slot_is_found_under_its_new_name_and_watch_session_dedups() {
        let inner = bare_inner("cyc-rename-unit");
        let dir = cyclops_proto::scratch::scratch_dir("cyc-rename-unit-ledger");
        let _ = std::fs::remove_dir_all(&dir);
        let ledger_path = dir.join("ledger/old-name.ndjson");
        let state_root = StateRoot::open_or_create(&dir).expect("state root opens");
        let ledger = cyclops_ledger::LedgerWriter::open(
            &state_root,
            Path::new("ledger/old-name.ndjson"),
            &inner.boot_id,
        )
        .expect("ledger opens");
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
        // slot already had open, still named after the old name on disk
        // (SessionSlot::ledger's doc comment), never a new file for the
        // new name.
        assert!(ledger_path.exists());
        assert!(!dir.join("ledger/new-name.ndjson").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stopping_refuses_a_new_session_without_publishing_state() {
        let inner = bare_inner("cyc-watch-after-stop");
        inner.engine.begin_stopping();

        let error = watch_session(&inner, "late-session")
            .await
            .expect_err("a stopped daemon must not register a session");

        assert_eq!(error.code, "daemon_stopping");
        assert_eq!(inner.session_count(), 0);
        assert!(inner
            .extra_tasks
            .lock()
            .expect("extra tasks lock")
            .is_empty());
        assert!(!inner
            .state_root
            .path()
            .join("ledger/late-session.ndjson")
            .exists());
    }

    #[test]
    fn stopping_refuses_a_followed_session_rename() {
        let inner = bare_inner("cyc-rename-after-stop");
        let dir = cyclops_proto::scratch::scratch_dir("cyc-rename-after-stop-ledger");
        let _ = std::fs::remove_dir_all(&dir);
        let state_root = StateRoot::open_or_create(&dir).expect("state root opens");
        let ledger = cyclops_ledger::LedgerWriter::open(
            &state_root,
            Path::new("ledger/original.ndjson"),
            &inner.boot_id,
        )
        .expect("ledger opens");
        let idx = {
            let mut sessions = inner.sessions.lock().expect("sessions lock");
            sessions.push(Arc::new(SessionSlot::new(
                "original".to_string(),
                Arc::new(ledger),
            )));
            sessions.len() - 1
        };

        inner.engine.begin_stopping();
        rename_session_slot(&inner, idx, "too-late".to_string());

        assert_eq!(inner.session_index("original"), Some(idx));
        assert_eq!(inner.session_index("too-late"), None);

        std::fs::remove_dir_all(dir).unwrap();
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
        let ledger = cyclops_ledger::LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/old-name.ndjson"),
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

    /// A terminal workspace is initially watched under its temporary tmux
    /// name, then renamed to the configured workspace name. If that configured
    /// slot was still detached, the rename used to leave two live watchers for
    /// one tmux session: status rendered the session twice and raw pane-id
    /// resolution failed as ambiguous. The live source slot must become the
    /// sole canonical owner while the configured slot remains readable only as
    /// a historical alias.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_runtime_session_renamed_onto_a_configured_slot_has_one_live_owner() {
        if !cyclops_testrig::tmux_available() {
            eprintln!("skipping: tmux not on PATH");
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("rename-configured-collision");
        server.run_ok(&["new-session", "-d", "-s", "yahirh", "/bin/sh"]);

        let mut inner = bare_inner("cyc-rename-configured-collision");
        let home = inner.cfg.home.clone();
        let (stop_tx, stop_rx) = watch::channel(false);
        {
            let mutable = Arc::get_mut(&mut inner).expect("bare inner is unique");
            mutable.cfg.tmux_socket = Some(server.socket().to_string());
            mutable.cfg.tmux_config = Some(PathBuf::from("/dev/null"));
            mutable.shutdown_request = stop_tx.clone();
            mutable.stop = stop_rx;
        }

        // The rename pause below lets this configured slot's real session task
        // attach before the runtime watcher handles the edge.
        let configured_ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/research.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        let configured = Arc::new(SessionSlot::new(
            "research".into(),
            Arc::new(configured_ledger),
        ));
        let configured_idx = {
            let mut sessions = inner.sessions.lock().unwrap();
            sessions.push(Arc::clone(&configured));
            sessions.len() - 1
        };
        let (runtime_idx, added) = watch_session(&inner, "yahirh")
            .await
            .expect("runtime session is watched");
        assert!(added);
        let runtime = inner.session(runtime_idx).unwrap();
        let runtime_binding = wait_for_session_binding(&runtime, None).await;
        assert!(runtime.link.lock().unwrap().attached);

        let (rename_entered_tx, rename_entered_rx) = std::sync::mpsc::channel();
        let rename_release = Arc::new(std::sync::Barrier::new(2));
        *runtime.rename_pause.lock().unwrap() = Some(SessionRenamePause {
            entered: rename_entered_tx,
            release: Arc::clone(&rename_release),
        });
        server.run_ok(&["rename-session", "-t", "=yahirh", "research"]);
        rename_entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("runtime rename handler reaches the registration race");
        let configured_task = tokio::spawn(session_task(
            Arc::clone(&inner),
            configured_idx,
            inner.stop.clone(),
        ));
        let configured_binding = wait_for_session_binding(&configured, None).await;
        assert!(configured.link.lock().unwrap().attached);
        assert_eq!(
            configured_binding.session_instance_id(),
            runtime_binding.session_instance_id(),
            "both live watchers must be proven to follow the same tmux session"
        );
        rename_release.wait();

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if configured.alias_of() == Some(runtime_idx) && runtime.name() == "research" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("rename collision is coalesced");

        tokio::time::timeout(Duration::from_secs(5), configured_task)
            .await
            .expect("active retired task exits")
            .expect("retired task does not panic");
        {
            let link = configured.link.lock().unwrap();
            assert!(!link.attached);
            assert!(link.watcher.is_none());
        }

        assert_eq!(inner.session_count(), 2, "both ledger indices remain");
        assert!(inner.session(configured_idx).is_some());
        assert_eq!(inner.session_index("research"), Some(runtime_idx));
        let active = inner.active_session_slots();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, runtime_idx);

        let live_identities: Vec<_> = active
            .iter()
            .filter_map(|(idx, slot)| {
                let link = slot.link.lock().unwrap();
                (link.attached && link.watcher.is_some())
                    .then(|| (*idx, link.identity.as_ref().unwrap().session_instance_id()))
            })
            .collect();
        assert_eq!(
            live_identities,
            vec![(runtime_idx, runtime_binding.session_instance_id())]
        );
        let mailbox = mailbox_routes(&inner, None);
        assert_eq!(mailbox.len(), 1);
        assert_eq!(mailbox[0].session_idx, runtime_idx);
        assert_eq!(
            mailbox[0].instance_id,
            runtime_binding.session_instance_id()
        );

        let status = server::status_result(&inner, false);
        assert_eq!(status.sessions.len(), 1);
        assert_eq!(status.sessions[0].name, "research");
        assert_eq!(status.sessions[0].panes.len(), 1);
        let pane_id = status.sessions[0].panes[0].pane_id.clone();
        assert_eq!(
            inner.resolve_recipient(&pane_id),
            Some((runtime_idx, pane_id.clone()))
        );
        let labeled = label_pane(&inner, &pane_id, Some("gemini-research".into()), None)
            .await
            .expect("a raw pane id remains labelable");
        assert_eq!(labeled["label"], "gemini-research");

        let alias_facts = configured.ledger.read_after(0).unwrap();
        assert!(alias_facts.iter().any(|line| {
            line.data
                .as_ref()
                .is_some_and(|data| data["event"] == "session_slot_aliased")
        }));
        let alias_line_count = alias_facts.len();
        delivery::admin_notify(
            &inner,
            cyclops_proto::NotifyLevel::Fyi,
            "canonical-only notification",
            "alias ledgers are historical",
            None,
            None,
            delivery::About::default(),
        );
        assert_eq!(
            configured.ledger.read_after(0).unwrap().len(),
            alias_line_count,
            "global live notifications must not extend a retired ledger"
        );
        assert!(runtime
            .ledger
            .read_after(0)
            .unwrap()
            .iter()
            .any(|line| { line.subject.as_deref() == Some("canonical-only notification") }));

        let _ = stop_tx.send(true);
        let tasks = std::mem::take(&mut *inner.extra_tasks.lock().unwrap());
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("runtime session task stops")
                .expect("runtime session task does not panic");
        }
        drop(runtime);
        drop(configured);
        drop(inner);
        let _ = std::fs::remove_dir_all(home);
    }

    /// The opposite registration order is equally important: the runtime
    /// rename can retire the configured slot before its task ever publishes.
    /// Starting that task afterwards must observe the alias and exit without
    /// attaching a second watcher or route.
    #[tokio::test]
    async fn a_rename_that_wins_registration_prevents_the_configured_slot_from_publishing() {
        if !cyclops_testrig::tmux_available() {
            eprintln!("skipping: tmux not on PATH");
            return;
        }
        let server = cyclops_testrig::TmuxServer::new("rename-wins-registration");
        server.run_ok(&["new-session", "-d", "-s", "yahirh", "/bin/sh"]);

        let mut inner = bare_inner("cyc-rename-wins-registration");
        let home = inner.cfg.home.clone();
        let (stop_tx, stop_rx) = watch::channel(false);
        {
            let mutable = Arc::get_mut(&mut inner).expect("bare inner is unique");
            mutable.cfg.tmux_socket = Some(server.socket().to_string());
            mutable.cfg.tmux_config = Some(PathBuf::from("/dev/null"));
            mutable.shutdown_request = stop_tx.clone();
            mutable.stop = stop_rx;
        }

        let configured_ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/research.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        let configured = Arc::new(SessionSlot::new(
            "research".into(),
            Arc::new(configured_ledger),
        ));
        let configured_idx = {
            let mut sessions = inner.sessions.lock().unwrap();
            sessions.push(Arc::clone(&configured));
            sessions.len() - 1
        };

        let (runtime_idx, added) = watch_session(&inner, "yahirh")
            .await
            .expect("runtime session is watched");
        assert!(added);
        let runtime = inner.session(runtime_idx).unwrap();
        wait_for_session_binding(&runtime, None).await;

        server.run_ok(&["rename-session", "-t", "=yahirh", "research"]);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if configured.alias_of() == Some(runtime_idx) && runtime.name() == "research" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("rename retires the unpublished configured slot");

        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::spawn(session_task(
                Arc::clone(&inner),
                configured_idx,
                inner.stop.clone(),
            )),
        )
        .await
        .expect("retired configured task exits")
        .expect("retired configured task does not panic");
        {
            let link = configured.link.lock().unwrap();
            assert!(!link.attached);
            assert!(link.watcher.is_none());
            assert!(link.identity.is_none());
        }
        assert_eq!(inner.session_index("research"), Some(runtime_idx));
        assert_eq!(inner.active_session_slots().len(), 1);
        let pane_id = runtime
            .link
            .lock()
            .unwrap()
            .mailbox_panes
            .keys()
            .next()
            .expect("runtime watcher published one pane")
            .clone();
        assert_eq!(
            inner.resolve_recipient(&pane_id),
            Some((runtime_idx, pane_id))
        );

        let _ = stop_tx.send(true);
        let tasks = std::mem::take(&mut *inner.extra_tasks.lock().unwrap());
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("runtime session task stops")
                .expect("runtime session task does not panic");
        }
        drop(runtime);
        drop(configured);
        drop(inner);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_configured_alias_keeps_the_runtime_journal_in_history_after_restart() {
        let tag = "cyc-session-alias-history";
        let inner = bare_inner(tag);
        let home = inner.cfg.home.clone();
        let configured_ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/research.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        let runtime_ledger = LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/yahirh.ndjson"),
            &inner.boot_id,
        )
        .unwrap();
        let configured = Arc::new(SessionSlot::new(
            "research".into(),
            Arc::new(configured_ledger),
        ));
        let runtime = Arc::new(SessionSlot::new("yahirh".into(), Arc::new(runtime_ledger)));
        inner
            .sessions
            .lock()
            .unwrap()
            .extend([Arc::clone(&configured), Arc::clone(&runtime)]);

        let mut before = daemon_line(Kind::Msg, "m-1-before-rename".into(), Value::Null);
        before.from = "admin".into();
        before.to = vec!["reviewer".into()];
        runtime.ledger.append(before).unwrap();
        rename_session_slot_locked(&inner, 1, "research".into(), None);
        std::thread::sleep(Duration::from_millis(2));
        let mut after = daemon_line(Kind::Msg, "m-2-after-rename".into(), Value::Null);
        after.from = "admin".into();
        after.to = vec!["reviewer".into()];
        runtime.ledger.append(after).unwrap();

        // A restart recreates only the configured slot. Its durable alias fact
        // must keep the runtime-created journal in the history source set.
        *inner.sessions.lock().unwrap() = vec![Arc::clone(&configured)];
        let history = history::msg_history(
            &inner,
            cyclops_proto::HistoryParams {
                with: None,
                from: None,
                to: None,
                limit: 10,
                cursor: None,
            },
            None,
            None,
        )
        .unwrap();
        let ids = history["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|line| line["id"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            ["m-1-before-rename", "m-2-after-rename"],
            "both sides of the rename must survive exactly once and in order: {history}"
        );

        drop(runtime);
        drop(configured);
        drop(inner);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn repeated_session_collisions_keep_every_alias_one_hop_from_the_owner() {
        let tag = "cyc-session-alias-flat";
        let inner = bare_inner(tag);
        let home = inner.cfg.home.clone();
        let add_slot = |name: &str| {
            let ledger = LedgerWriter::open(
                &inner.state_root,
                &PathBuf::from("ledger").join(format!("{name}.ndjson")),
                &inner.boot_id,
            )
            .unwrap();
            let slot = Arc::new(SessionSlot::new(name.into(), Arc::new(ledger)));
            inner.sessions.lock().unwrap().push(Arc::clone(&slot));
            slot
        };

        let configured = add_slot("research");
        let first = add_slot("first");
        rename_session_slot_locked(&inner, 1, "research".into(), None);
        assert_eq!(configured.alias_of(), Some(1));

        let second = add_slot("second");
        rename_session_slot_locked(&inner, 2, "research".into(), None);

        assert_eq!(first.alias_of(), Some(2));
        assert_eq!(configured.alias_of(), Some(2));
        assert!(second.is_canonical());
        assert_eq!(inner.active_session_slots().len(), 1);

        drop(second);
        drop(first);
        drop(configured);
        drop(inner);
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
        let state_root = StateRoot::open_or_create(&dir).expect("state root opens");
        let ledger = cyclops_ledger::LedgerWriter::open(
            &state_root,
            Path::new("ledger/same-name.ndjson"),
            &inner.boot_id,
        )
        .expect("ledger opens");
        let idx = {
            let mut sessions = inner.sessions.lock().expect("sessions lock");
            sessions.push(Arc::new(SessionSlot::new(
                "same-name".to_string(),
                Arc::new(ledger),
            )));
            sessions.len() - 1
        };

        let before = inner
            .session(idx)
            .unwrap()
            .ledger
            .read_after(0)
            .expect("ledger reads");
        rename_session_slot(&inner, idx, "same-name".to_string());
        let after = inner
            .session(idx)
            .unwrap()
            .ledger
            .read_after(0)
            .expect("ledger reads");
        assert_eq!(
            before.len(),
            after.len(),
            "a same-name rename appends nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
