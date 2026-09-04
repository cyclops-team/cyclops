//! The notification pipeline (docs/development/DELIVERY.md is the spec, and
//! the flow with its decision points drawn is docs/development/ARCHITECTURE.md).
//!
//! One worker per durable recipient; notifications to one mailbox are
//! strictly FIFO. Every transition is a content-free workspace journal fact
//! appended through `NotificationContext`. Failures queue or block durably;
//! they never drop.
//!
//! Three things live here that read like they belong elsewhere, and each
//! one is here because it needs the same handle the pipeline holds:
//! `admin_notify` (a ping usually points at a notification), `agent_wait`
//! (a pane-state wait pinned to the occupant present at its start), and
//! `About`, the item a ping names so a reader can stop showing it.
//!
//! What is NOT decided here: whether a pane is idle (`fusion.rs`), which
//! keys dismiss a modal (`cyclops-manifest` data), whether a finished
//! notification still needs a human (`cyclops_proto::attention`), and how
//! any of it is worded for a person (the CLI).
//!
//! Zero-polling shape: workers sleep on queue notifies and wake on watcher
//! or fusion events. Every timer is a one-shot tied to one notification: the
//! paste verification re-reads, the tier-1 ACK window, the screen-evidence
//! checkpoints, the decline-key spacing, the idle-ambiguous-composer settle
//! deadline, the gate's single wedged-hold ping, and the two deadlines a
//! caller asked for (`receipt_block_ms`, and `timeout_ms` on a wait). Nothing
//! runs on an interval.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_manifest::{strip_csi, AckEvidence, Manifest, UnstyledComposerProof};
use cyclops_proto::{
    AgentState, ComposerSemantic, ComposerState, DeliveryState, Detection, Event, Kind, LedgerLine,
    NotificationAttemptId, NotificationAttentionCause, NotificationBinding, NotificationManifestId,
    NotificationPreWriteCause, NotificationPreWriteObservation, NotificationState,
    NotificationTransport, NotificationVerifyFailureKind, NotificationVerifyOutcome, NotifyLevel,
    ProcessInstanceId, QuiesceResult, RecipientKey, StatusDiagnostic, VerifiedBy, WaitUntil,
    WireError,
};
use cyclops_tmux::{ControlClient, PaneEvent, PaneRow, SessionWatcher, TmuxError};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, warn};

use crate::messaging::{MessagingPreWriteBlock, MessagingPreWriteBlockOutcome};
use crate::notification_adapter::{
    ClaimedNotificationBarrier, NotificationAdapterError, NotificationContext, SubmitReservation,
};
use crate::{daemon_line, fusion, unix_ms, Inner, PaneKey};

pub(crate) mod gate;
pub(crate) mod inject;
pub(crate) mod terminal;
pub(crate) mod worker;

pub(crate) use gate::*;
pub use inject::*;
pub use terminal::*;
pub(crate) use worker::*;

/// Delivery gives up on evidence this long after submit (spec: neither ACK
/// tier within 5s goes to retry_queued).
pub(crate) const SCREEN_ACK_DEADLINE: Duration = Duration::from_secs(5);
/// One-shot screen-evidence checkpoints after submit. Events also wake the
/// waiter; these bound the captures per delivery.
pub(crate) const ACK_CHECKPOINTS_MS: [u64; 5] = [250, 750, 1500, 3000, 5000];
/// Post-paste and final-staging verification re-reads. A terminal renderer
/// can lag one frame behind a paste or expose a partial repaint between two
/// otherwise exact proofs. Offsets from the preceding write or reread, one
/// capture each; bounded per attempt.
pub(crate) const VERIFY_DELAYS_MS: [u64; 4] = [0, 120, 240, 480];
/// Bottom non-empty lines scanned for the staged verify pattern.
pub(crate) const VERIFY_REGION: usize = 15;
/// Bottom non-empty lines treated as the composer zone for the
/// marker-left-composer check.
pub(crate) const COMPOSER_WINDOW: usize = 6;
/// Spacing between manifest-defined modal decline keys.
pub(crate) const DECLINE_SPACING: Duration = Duration::from_millis(250);
/// Attempts to auto-dismiss one modal rule before treating it as
/// non-dismissable (hold plus admin notify). Never loop.
pub(crate) const MAX_DECLINES: u32 = 3;
/// Default and ceiling for agent.wait timeouts.
pub(crate) const WAIT_DEFAULT_MS: u64 = 60_000;
pub(crate) const WAIT_MAX_MS: u64 = 600_000;
/// Default and ceiling for `daemon.quiesce`: how long to wait for
/// deliveries already past the paste to resolve. Past-the-paste windows
/// are seconds by construction (the verify re-reads and ACK deadline
/// above), so a small bound covers the honest case and a caller cannot
/// wedge the pipeline with a huge one.
pub(crate) const QUIESCE_DEFAULT_MS: u64 = 5_000;
pub(crate) const QUIESCE_MAX_MS: u64 = 30_000;
/// How long a quiet quiesce holds the pipeline still waiting for the stop
/// that should follow. If none arrives (the caller died between the
/// answer and the signal), the pipeline un-holds itself rather than
/// freezing deliveries forever.
pub(crate) const QUIESCE_HOLD_FALLBACK_MS: u64 = 30_000;
/// Upgradeable delivered_unverified handles kept per pane for late hook
/// ACK upgrades.
pub(crate) const ACK_REGISTRY_CAP: usize = 32;
pub(crate) const NO_LONGER_CURRENT_BEFORE_WRITE: &str = "notification_no_longer_current";
pub(crate) const NOTIFICATION_RECORD_FAILED: &str = "notification_record_failed";
pub(crate) const CLAIMED_STAGED_SETTLEMENT_FAILED: &str = "claimed_staged_settlement_failed";

/// Delivery engine state. Lives in [`Inner`]; all behavior is free
/// functions taking the daemon state so nothing here holds locks across
/// awaits by construction.
pub(crate) struct Engine {
    /// Active mailbox workers and their tasks, keyed by durable recipient.
    pub(crate) notification_workers: StdMutex<HashMap<RecipientKey, NotificationWorker>>,
    /// Per-delivery unique tmux buffer names.
    pub(crate) buffer_seq: AtomicU64,
    /// Notifications awaiting or upgradeable by a hook ACK, per exact route.
    pub(crate) acks: StdMutex<HashMap<PaneKey, Vec<Arc<DeliveryHandle>>>>,
    /// Weak refs to every handle the pipeline has created, for the
    /// quiesce sweep. Pruned as it is read; the pipeline itself never
    /// looks here.
    pub(crate) open: StdMutex<Vec<std::sync::Weak<DeliveryHandle>>>,
    /// Active mailbox notification handles by durable attempt id.
    ///
    /// The workspace record is authoritative. This index only prevents
    /// two in-memory workers from driving the same queued attempt.
    pub(crate) notification_attempts:
        StdMutex<HashMap<NotificationAttemptId, std::sync::Weak<DeliveryHandle>>>,
    /// Set while a quiesce holds the pipeline still: workers finish the
    /// delivery they are on, start no new one, and nothing crosses the
    /// paste boundary (the gate's proceed re-checks it).
    pub(crate) paused: AtomicBool,
    /// Set before shutdown drains task handles. Once set, no worker registry
    /// may publish new work.
    pub(crate) stopping: AtomicBool,
    /// Every spawned task below a registered daemon root. Shutdown waits
    /// for this tree to become empty.
    pub(crate) descendant_tasks: Arc<DescendantTaskDrain>,
    /// Cancels descendant work at the same boundary that closes worker
    /// creation. The journal seal remains the final write barrier.
    pub(crate) descendant_stop: watch::Sender<bool>,
}

#[derive(Default)]
pub(crate) struct DescendantTaskDrain {
    pub(crate) active: AtomicUsize,
    pub(crate) empty: Notify,
}

pub(crate) struct DescendantTaskGuard {
    pub(crate) drain: Arc<DescendantTaskDrain>,
}

impl Drop for DescendantTaskGuard {
    fn drop(&mut self) {
        if self.drain.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drain.empty.notify_one();
        }
    }
}

impl DescendantTaskDrain {
    pub(crate) fn enter(self: &Arc<Self>) -> DescendantTaskGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        DescendantTaskGuard {
            drain: Arc::clone(self),
        }
    }

    pub(crate) async fn wait_empty(&self) {
        loop {
            let empty = self.empty.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            empty.await;
        }
    }
}

impl Engine {
    pub(crate) fn new() -> Engine {
        let (descendant_stop, _) = watch::channel(false);
        Engine {
            notification_workers: StdMutex::new(HashMap::new()),
            buffer_seq: AtomicU64::new(0),
            acks: StdMutex::new(HashMap::new()),
            open: StdMutex::new(Vec::new()),
            notification_attempts: StdMutex::new(HashMap::new()),
            paused: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            descendant_tasks: Arc::new(DescendantTaskDrain::default()),
            descendant_stop,
        }
    }

    /// Wrap work in the daemon's descendant lifetime.
    ///
    /// `None` means shutdown cancelled the work. Callers that need a result
    /// must treat that as an incomplete operation, never as success.
    pub(crate) fn track_descendant<F>(
        &self,
        future: F,
    ) -> impl std::future::Future<Output = Option<F::Output>> + Send + 'static
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let guard = self.descendant_tasks.enter();
        let mut stop = self.descendant_stop.subscribe();
        async move {
            let _guard = guard;
            if *stop.borrow() {
                return None;
            }
            tokio::select! {
                _ = stop.changed() => None,
                output = future => Some(output),
            }
        }
    }

    pub(crate) fn spawn_descendant_task<F>(&self, future: F) -> JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let tracked = self.track_descendant(future);
        tokio::spawn(async move {
            let _ = tracked.await;
        })
    }

    pub(crate) async fn wait_for_descendant_tasks(&self) {
        self.descendant_tasks.wait_empty().await;
    }

    pub(crate) fn begin_stopping(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.paused.store(true, Ordering::SeqCst);
        self.descendant_stop.send_replace(true);
        for entry in self
            .notification_workers
            .lock()
            .expect("notification workers lock")
            .values()
        {
            entry.worker.notify.notify_waiters();
        }
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Remember a handle for the quiesce sweep, dropping entries whose
    /// deliveries are gone.
    pub(crate) fn track(&self, handle: &Arc<DeliveryHandle>) {
        let mut open = self.open.lock().expect("open handles lock");
        open.retain(|w| w.strong_count() > 0);
        open.push(Arc::downgrade(handle));
    }

    /// Every delivery handle still alive.
    pub(crate) fn open_handles(&self) -> Vec<Arc<DeliveryHandle>> {
        let mut open = self.open.lock().expect("open handles lock");
        open.retain(|w| w.strong_count() > 0);
        open.iter().filter_map(std::sync::Weak::upgrade).collect()
    }

    pub(crate) fn cancel_notification(&self, attempt_id: NotificationAttemptId) {
        let handle = self
            .notification_attempts
            .lock()
            .expect("notification attempts lock")
            .get(&attempt_id)
            .and_then(std::sync::Weak::upgrade);
        if let Some(handle) = handle {
            handle.cancel.notify_one();
        }
    }

    pub(crate) fn notification_handle(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Option<Arc<DeliveryHandle>> {
        self.notification_attempts
            .lock()
            .expect("notification attempts lock")
            .get(&attempt_id)
            .and_then(std::sync::Weak::upgrade)
    }

    /// Retire this run only when the attempts index still names this handle.
    ///
    /// Evidence-driven reopening replaces the index with a fresh handle for
    /// the same durable attempt. The returning stale handle must not erase it.
    pub(crate) fn retire_notification_run(&self, handle: &Arc<DeliveryHandle>) -> bool {
        let attempt_id = handle.notification.attempt_id();
        let mut active = self
            .notification_attempts
            .lock()
            .expect("notification attempts lock");
        let owns_entry = active
            .get(&attempt_id)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, handle));
        if !owns_entry {
            return false;
        }
        active.remove(&attempt_id);
        true
    }

    pub(crate) fn replace_notification_run(
        &self,
        old: &Arc<DeliveryHandle>,
        new: &Arc<DeliveryHandle>,
    ) -> bool {
        let attempt_id = old.notification.attempt_id();
        let mut active = self
            .notification_attempts
            .lock()
            .expect("notification attempts lock");
        let owns_entry = active
            .get(&attempt_id)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, old));
        if owns_entry {
            active.insert(attempt_id, Arc::downgrade(new));
        }
        drop(active);
        if owns_entry {
            self.track(new);
        }
        owns_entry
    }

    /// Un-hold the pipeline and wake every worker.
    pub(crate) fn resume_workers(&self) {
        self.paused.store(false, Ordering::SeqCst);
        for entry in self
            .notification_workers
            .lock()
            .expect("notification workers lock")
            .values()
        {
            entry.worker.notify.notify_one();
        }
    }

    /// Enqueue while holding the worker registry lock. Retirement takes the
    /// same locks in the same order, so it cannot remove a worker between
    /// lookup and queue publication.
    pub(crate) fn enqueue_notification_worker<F>(
        &self,
        recipient: RecipientKey,
        handle: Arc<DeliveryHandle>,
        spawn: F,
    ) -> Result<Arc<Worker>, NotificationEnqueueRefusal>
    where
        F: FnOnce(Arc<Worker>) -> JoinHandle<()>,
    {
        let mut entries = self
            .notification_workers
            .lock()
            .expect("notification workers lock");
        if self.stopping.load(Ordering::SeqCst) {
            return Err(NotificationEnqueueRefusal::DaemonStopping);
        }
        if let Some(entry) = entries.get_mut(&recipient) {
            let worker = Arc::clone(&entry.worker);
            if entry.task.is_finished() {
                worker.set_fault("notification worker supervisor exited");
                return Err(NotificationEnqueueRefusal::WorkerSupervisorExited);
            }
            if worker.is_faulted() {
                return Err(NotificationEnqueueRefusal::WorkerFaulted);
            }
            worker.enqueue_back(handle);
            drop(entries);
            worker.notify.notify_one();
            return Ok(worker);
        }

        let worker = Arc::new(Worker::new());
        worker.enqueue_back(handle);
        let task = spawn(Arc::clone(&worker));
        let task_finished = task.is_finished();
        entries.insert(
            recipient,
            NotificationWorker {
                worker: Arc::clone(&worker),
                task,
            },
        );
        if task_finished {
            worker.set_fault("notification worker supervisor exited");
            return Err(NotificationEnqueueRefusal::WorkerSupervisorExited);
        }
        drop(entries);
        worker.notify.notify_one();
        Ok(worker)
    }

    /// Remove this exact worker only while its queue is still empty.
    /// True means the caller no longer owns the registry entry and must exit.
    pub(crate) fn retire_notification_worker(
        &self,
        recipient: RecipientKey,
        worker: &Arc<Worker>,
    ) -> bool {
        let mut entries = self
            .notification_workers
            .lock()
            .expect("notification workers lock");
        let Some(entry) = entries.get(&recipient) else {
            return true;
        };
        if !Arc::ptr_eq(&entry.worker, worker) {
            return true;
        }
        if !worker.is_idle() {
            return false;
        }
        entries.remove(&recipient);
        true
    }

    /// Whether this exact worker still owns the recipient registry entry.
    ///
    /// A notification loop removes its entry before its one legitimate clean
    /// return. The supervisor uses this exact pointer check to distinguish
    /// that retirement from a child that vanished while its FIFO was still
    /// published. A replacement worker for the same recipient is not this
    /// worker and must not keep the old supervisor alive.
    pub(crate) fn notification_worker_is_current(
        &self,
        recipient: RecipientKey,
        worker: &Arc<Worker>,
    ) -> bool {
        self.notification_workers
            .lock()
            .expect("notification workers lock")
            .get(&recipient)
            .is_some_and(|entry| Arc::ptr_eq(&entry.worker, worker))
    }

    pub(crate) fn take_notification_worker_tasks(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(
            &mut *self
                .notification_workers
                .lock()
                .expect("notification workers lock"),
        )
        .into_values()
        .map(|entry| entry.task)
        .collect()
    }

    /// Content-free faults for exact notification work that stopped in memory.
    pub(crate) fn notification_worker_diagnostics(&self) -> Vec<StatusDiagnostic> {
        self.notification_workers
            .lock()
            .expect("notification workers lock")
            .iter()
            .filter_map(|(recipient, entry)| {
                let state = entry.worker.state.lock().expect("worker state lock");
                let fault = state.fault.as_ref()?;
                let handle = state.current.as_ref().or_else(|| state.queue.front())?;
                let notification = &handle.notification;
                Some(StatusDiagnostic {
                    code: if fault == CLAIMED_STAGED_SETTLEMENT_FAILED {
                        "notification_settlement_storage_failed"
                    } else if fault.starts_with("notification pre-write block storage failed:") {
                        "notification_prewrite_storage_failed"
                    } else if fault.starts_with("notification recovery failed:") {
                        "notification_recovery_storage_failed"
                    } else {
                        "notification_worker_failed"
                    }
                    .into(),
                    message_id: notification.message_id().clone(),
                    notification_attempt: notification.attempt_id(),
                    recipient: *recipient,
                    recipient_label: handle.to.clone(),
                    pane_id: handle.pane_id.clone(),
                })
            })
            .collect()
    }

    /// Whether the exact recipient worker currently owns this attempt.
    ///
    /// The exact handle must be current or queued under a live, nonfaulted
    /// supervisor. A weak attempt-index entry alone never proves active work.
    /// Status uses this only to describe the next step; mutation paths recheck
    /// their own authority immediately before acting.
    pub(crate) fn notification_worker_owns(
        &self,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> bool {
        self.notification_worker_refusal(recipient, attempt_id)
            .is_none()
    }

    pub(crate) fn notification_worker_refusal(
        &self,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Option<NotificationEnqueueRefusal> {
        if self.is_stopping() {
            return Some(NotificationEnqueueRefusal::DaemonStopping);
        }
        let workers = self
            .notification_workers
            .lock()
            .expect("notification workers lock");
        let Some(entry) = workers.get(&recipient) else {
            return Some(NotificationEnqueueRefusal::AttemptUnowned);
        };
        if entry.task.is_finished() {
            return Some(NotificationEnqueueRefusal::WorkerSupervisorExited);
        }
        let state = entry.worker.state.lock().expect("worker state lock");
        if state.fault.is_some() {
            return Some(NotificationEnqueueRefusal::WorkerFaulted);
        }
        let current = state
            .current
            .as_ref()
            .is_some_and(|handle| handle.notification.attempt_id() == attempt_id);
        if current
            || state
                .queue
                .iter()
                .any(|handle| handle.notification.attempt_id() == attempt_id)
        {
            None
        } else {
            Some(NotificationEnqueueRefusal::AttemptUnowned)
        }
    }

    #[doc(hidden)]
    pub(crate) fn mailbox_worker_current_for_test(
        &self,
        recipient: RecipientKey,
    ) -> Option<(String, Option<NotificationAttemptId>)> {
        let workers = self
            .notification_workers
            .lock()
            .expect("notification workers lock");
        let entry = workers.get(&recipient)?;
        let state = entry.worker.state.lock().expect("worker state lock");
        let current = state.current.as_ref()?;
        Some((
            current.msg_id.clone(),
            Some(current.notification.attempt_id()),
        ))
    }
}

/// One recipient's notification attempt, shared between the worker, the
/// ACK matcher, and receipt waiters.
pub(crate) struct DeliveryHandle {
    pub(crate) msg_id: String,
    /// Recipient as addressed (label, or pane id when unlabeled).
    pub(crate) to: String,
    pub(crate) pane_id: String,
    pub(crate) session_idx: usize,
    /// The exact bytes selected at the write boundary. Empty until then.
    pub(crate) payload: StdMutex<String>,
    /// Durable one-shot notification facts emitted at this worker's real boundaries.
    pub(crate) notification: NotificationContext,
    /// Payload shape persisted at the notification write boundary.
    pub(crate) notification_transport: StdMutex<Option<cyclops_proto::NotificationTransport>>,
    pub(crate) state: StdMutex<HandleState>,
    pub(crate) state_tx: watch::Sender<DeliveryState>,
    /// Wakes the worker when the ACK matcher resolved this notification.
    pub(crate) ack: Notify,
    /// Wakes a held gate after the same claim fact withdraws its attempt.
    pub(crate) cancel: Notify,
    /// Working evidence at or after this notification's submit. Tier-2
    /// receipt evidence only; it does not correlate a turn to a message.
    pub(crate) working_seen: AtomicBool,
    /// The admitted AGENT identity the submit key reached, birth included
    /// so a reused pid is a different agent rather than an heir to this
    /// notification's trust.
    pub(crate) submitted_agent: StdMutex<Option<crate::identity::ProcId>>,
    /// When the submit key went out. A screen-lifecycle receipt carries
    /// this mark for diagnosis. Exact lifecycle release ignores it and
    /// matches the manifest-declared TurnKey instead.
    pub(crate) submitted_at_ms: std::sync::atomic::AtomicU64,
    /// The manifest bound to the pane when the submit key was sent. Paired
    /// with `submitted_agent` it identifies the agent instance and vendor
    /// rules that own receipt evidence. A transient foreground tool may
    /// change without transferring that ownership.
    pub(crate) submitted_manifest: StdMutex<Option<String>>,
    /// True once the synchronous write-boundary hook completed.
    pub(crate) write_boundary_crossed: AtomicBool,
    /// One automatic supervisor recovery is allowed for this exact run.
    pub(crate) worker_recoveries: AtomicU64,
    /// A readiness edge observed while this claimed-barrier run was active.
    /// Consumed only after the attempt index releases this handle.
    pub(crate) claimed_notification_rerun_requested: AtomicBool,
    /// This ordinary notification was admitted immediately before paste while
    /// the pane was visibly Working and its composer was positively clean or
    /// ghosted. The staged doorbell naturally reads as input afterward, so
    /// this one-attempt capability carries the pre-paste proof to the final
    /// exact-byte submit check. It is never restored or used by recovery.
    pub(crate) working_clean_submit_admitted: AtomicBool,
}

/// Evidence from a vendor hook that landed before the worker consumed it.
#[derive(Debug, Clone)]
pub(crate) struct PendingAck {
    pub(crate) edge_ms: u64,
    pub(crate) turn: Option<crate::turnkey::TurnKey>,
    pub(crate) evidence: PendingAckEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingAckEvidence {
    /// The hook itself proves receipt.
    Receipt,
    /// The hook only proves dispatch. A later visual Working observation
    /// must accept the same correlated turn.
    DispatchPending,
    DispatchAccepted,
}

pub(crate) struct HandleState {
    pub(crate) state: DeliveryState,
    pub(crate) attempts: u32,
    pub(crate) verified_by: Option<VerifiedBy>,
    pub(crate) cause: Option<String>,
    /// Normalized gate hold token for the in-flight head, if any.
    pub(crate) held_by: Option<String>,
    /// An acknowledgement that arrived before the notification reached the
    /// state that consumes one.
    ///
    /// Kept HERE, under the same lock as the state, because installing it
    /// and classifying the state are ONE decision. Read the state, see
    /// `Staged`, and install afterwards, and the worker can move to
    /// `Submitted` and consume in between: the record is then written
    /// after the only read of it, and a valid acknowledgement is lost.
    pub(crate) early_ack: Option<PendingAck>,
    /// Count of write-boundary refusals that wrote no pane bytes. Attempts
    /// remain append-only, so retry accounting subtracts this cumulative
    /// count rather than charging a refusal as transport work.
    pub(crate) regates: u32,
    /// Binding receives one immediate re-proof after an exact pane or
    /// readiness edge. Repeated refusal under unchanged evidence settles as
    /// a durable pre-write block.
    pub(crate) regate_reproof_used: bool,
    /// The barrier claim this notification currently holds. Set only when a
    /// claim was granted, and compared before any later settlement so a
    /// receipt cannot release a barrier this notification no longer owns.
    pub(crate) barrier: Option<String>,
}

impl DeliveryHandle {
    /// Does this hook prompt carry exactly the payload this notification
    /// wrote? The bytes stay inside the handle; see `prompt_matches` for
    /// why nothing weaker is accepted. Nothing matches before the write
    /// boundary selected the bytes.
    pub(crate) fn claims_prompt(&self, text: &str) -> bool {
        let payload = self.payload.lock().expect("payload lock");
        !payload.is_empty() && prompt_matches(text, &payload)
    }

    /// This attempt's claim on a pane's composer barrier: the durable
    /// attempt id, content-free and unique per attempt.
    pub(crate) fn barrier_owner(&self) -> String {
        self.notification.attempt_id().to_string()
    }

    /// Is this report from the process and rules the submit key reached?
    ///
    /// False before a submit has happened at all, which is the point: the
    /// ACK registry is deliberately populated earlier so a fast hook is
    /// not missed, and a notification that has not been submitted has no
    /// binding for a hook to match.
    pub(crate) fn submitted_binding_is(
        &self,
        agent: crate::identity::ProcId,
        manifest_id: &str,
    ) -> bool {
        let want = self.submitted_agent.lock().expect("submitted agent lock");
        if *want != Some(agent) {
            return false;
        }
        self.submitted_manifest
            .lock()
            .expect("submitted manifest lock")
            .as_deref()
            == Some(manifest_id)
    }

    pub(crate) fn for_notification(
        to: &str,
        pane_id: &str,
        session_idx: usize,
        notification: NotificationContext,
    ) -> Arc<Self> {
        let (state_tx, _) = watch::channel(DeliveryState::Queued);
        Arc::new(DeliveryHandle {
            msg_id: notification.message_id().to_string(),
            to: to.to_string(),
            pane_id: pane_id.to_string(),
            session_idx,
            payload: StdMutex::new(String::new()),
            notification,
            notification_transport: StdMutex::new(None),
            state: StdMutex::new(HandleState {
                state: DeliveryState::Queued,
                attempts: 0,
                verified_by: None,
                cause: None,
                held_by: None,
                early_ack: None,
                regates: 0,
                regate_reproof_used: false,
                barrier: None,
            }),
            state_tx,
            ack: Notify::new(),
            cancel: Notify::new(),
            working_seen: AtomicBool::new(false),
            submitted_agent: StdMutex::new(None),
            submitted_at_ms: std::sync::atomic::AtomicU64::new(0),
            submitted_manifest: StdMutex::new(None),
            write_boundary_crossed: AtomicBool::new(false),
            worker_recoveries: AtomicU64::new(0),
            claimed_notification_rerun_requested: AtomicBool::new(false),
            working_clean_submit_admitted: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub(crate) fn payload(&self) -> String {
        self.payload.lock().expect("payload lock").clone()
    }

    pub(crate) fn set_attempt_payload(
        &self,
        payload: String,
        transport: Option<cyclops_proto::NotificationTransport>,
    ) {
        *self.payload.lock().expect("payload lock") = payload;
        *self
            .notification_transport
            .lock()
            .expect("notification transport lock") = transport;
    }

    pub(crate) fn notification_transport(&self) -> Option<cyclops_proto::NotificationTransport> {
        *self
            .notification_transport
            .lock()
            .expect("notification transport lock")
    }

    pub(crate) fn set_working_clean_submit_admitted(&self, admitted: bool) {
        self.working_clean_submit_admitted
            .store(admitted, Ordering::SeqCst);
    }

    pub(crate) fn working_clean_submit_admitted(&self) -> bool {
        self.working_clean_submit_admitted.load(Ordering::SeqCst)
    }

    /// Resume a claimed `staged` doorbell from its durable record: the
    /// exact bytes the earlier run wrote, the barrier it owns, and the
    /// write boundary already crossed.
    pub(crate) fn restore_claimed_notification_barrier(&self, staged: String) {
        let attempt_id = self.notification.attempt_id();
        {
            let mut state = self.state.lock().expect("handle state lock");
            state.state = DeliveryState::Staged;
            state.barrier = Some(attempt_id.to_string());
        }
        self.state_tx.send_replace(DeliveryState::Staged);
        self.set_attempt_payload(staged, Some(NotificationTransport::Doorbell));
        self.write_boundary_crossed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn state(&self) -> DeliveryState {
        self.state.lock().expect("handle state lock").state
    }

    pub(crate) fn snapshot(
        &self,
    ) -> (
        DeliveryState,
        u32,
        Option<VerifiedBy>,
        Option<String>,
        Option<String>,
    ) {
        let st = self.state.lock().expect("handle state lock");
        (
            st.state,
            st.attempts,
            st.verified_by,
            st.cause.clone(),
            st.held_by.clone(),
        )
    }

    pub(crate) fn set_hold(&self, hold: Option<&str>) {
        self.state.lock().expect("handle state lock").held_by = hold.map(str::to_string);
    }
}

/// What a ping is ABOUT: the attention item a reader can go and clear.
///
/// A ping says a human is needed but is not itself a state, so nothing
/// ever transitions it and no register can hold it. Naming its item is
/// what lets a reader's stream tell a ping that still stands from one
/// whose subject already resolved (cyclops-ui `App::admits`). Additive on
/// the wire: a ping that names nothing omits both fields, and a reader
/// that predates them ignores them.
#[derive(Default)]
pub(crate) struct About {
    /// The pane whose blocked state raised the ping.
    pub(crate) pane_id: Option<String>,
    /// The recipient of the notification the ping is about. That
    /// notification's message id is the ping's own `msg_id`, so this pairs
    /// with `Some(msg_id)`.
    pub(crate) to: Option<String>,
}

impl About {
    /// A ping about a pane a human must unblock.
    pub(crate) fn pane(pane_id: &str) -> About {
        About {
            pane_id: Some(pane_id.to_string()),
            ..About::default()
        }
    }

    /// A ping about one notification to `to`. Pass the message id as
    /// `msg_id` or the ping names a recipient without saying which message.
    pub(crate) fn delivery(to: &str) -> About {
        About {
            to: Some(to.to_string()),
            ..About::default()
        }
    }
}

/// Write a kind=system admin notification line and broadcast the event.
/// `session_idx` scopes internal (notification-driven) pings to the
/// recipient's ledger; None (external admin.notify) writes to every active
/// canonical session ledger so any live single-session reader sees it.
pub(crate) fn admin_notify(
    inner: &Arc<Inner>,
    level: NotifyLevel,
    subject: &str,
    body: &str,
    msg_id: Option<&str>,
    session_idx: Option<usize>,
    about: About,
) -> Option<u64> {
    let id = msg_id
        .map(str::to_string)
        .unwrap_or_else(|| inner.mint_event_id());
    // The item the ping points at, written once and worn by both the
    // ledger line's data and the live event, so a reader replaying the
    // record and one on the push read the same ping.
    let mut about_fields = serde_json::Map::new();
    if let Some(pane_id) = &about.pane_id {
        about_fields.insert("pane_id".into(), json!(pane_id));
    }
    if let Some(to) = &about.to {
        about_fields.insert("to".into(), json!(to));
    }
    let with_about = |mut v: Value| {
        if let Value::Object(map) = &mut v {
            map.extend(about_fields.clone());
        }
        v
    };
    let line = LedgerLine {
        to: vec!["admin".to_string()],
        subject: Some(subject.to_string()),
        body: if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        },
        ..daemon_line(
            Kind::System,
            id.clone(),
            with_about(json!({"event": "admin_notify", "level": level})),
        )
    };
    let sessions: Vec<usize> = match session_idx {
        Some(i) => vec![i],
        None => inner
            .active_session_slots()
            .into_iter()
            .map(|(idx, _)| idx)
            .collect(),
    };
    let mut first_seq = None;
    for idx in sessions {
        let seq = inner.append_line(idx, line.clone());
        if first_seq.is_none() {
            first_seq = seq;
        }
    }
    inner.emit(
        "admin-notify",
        with_about(json!({"id": id, "level": level, "subject": subject, "body": body})),
        first_seq,
    );
    first_seq
}

// ---------------------------------------------------------------------------
// Quiesce
// ---------------------------------------------------------------------------

/// `daemon.quiesce`: hold the delivery pipeline still so a stop that
/// follows loses nothing.
///
/// Holds the workers (they finish the delivery they are on and start no
/// new one; the gate's proceed re-checks the hold so nothing crosses the
/// paste boundary), then waits out every delivery already past the paste.
/// Those windows are seconds by construction: the verify re-reads and the
/// acknowledgment deadline. On a healthy fleet this answers quickly.
///
/// Notifications that have not reached a pane do not block quiet: they are
/// durably queued and the next boot schedules them again. Quiet keeps the
/// pipeline held for the stop that should follow, with a bounded self-release in case the
/// caller died between the answer and the signal. Not-quiet releases the
/// hold immediately and names what is still moving.
pub(crate) async fn quiesce(inner: &Arc<Inner>, timeout_ms: Option<u64>) -> QuiesceResult {
    let bound = timeout_ms.unwrap_or(QUIESCE_DEFAULT_MS).min(QUIESCE_MAX_MS);
    let deadline = Instant::now() + Duration::from_millis(bound);
    inner.engine.paused.store(true, Ordering::SeqCst);
    loop {
        // Re-collected each pass: a worker that popped its job before the
        // hold landed can still carry one delivery past the paste, and
        // that one must be waited out too.
        let in_flight: Vec<Arc<DeliveryHandle>> = inner
            .engine
            .open_handles()
            .into_iter()
            .filter(|h| {
                let s = h.state();
                !receipt_resolved(s) && !receipt_is_queued(s)
            })
            .collect();
        if in_flight.is_empty() {
            let held = Arc::clone(inner);
            let mut stop = held.stop.clone();
            inner.engine.spawn_descendant_task(async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(QUIESCE_HOLD_FALLBACK_MS)) => {
                        if held.engine.paused.load(Ordering::SeqCst) {
                            warn!("quiesce hold expired with no stop; resuming deliveries");
                            held.engine.resume_workers();
                        }
                    }
                    _ = stop.changed() => {}
                }
            });
            return QuiesceResult {
                quiet: true,
                in_flight: Vec::new(),
            };
        }
        let mut timed_out = false;
        for handle in &in_flight {
            let mut rx = handle.state_tx.subscribe();
            if tokio::time::timeout_at(deadline, rx.wait_for(|s| receipt_resolved(*s)))
                .await
                .is_err()
            {
                timed_out = true;
                break;
            }
        }
        if timed_out {
            inner.engine.resume_workers();
            return QuiesceResult {
                quiet: false,
                in_flight: in_flight
                    .iter()
                    .filter(|h| !receipt_resolved(h.state()))
                    .map(|h| format!("{} -> {}", h.msg_id, h.to))
                    .collect(),
            };
        }
    }
}

pub(crate) fn wire_err(code: &str, msg: impl Into<String>) -> WireError {
    WireError {
        code: code.to_string(),
        message: msg.into(),
        data: None,
    }
}

/// The four states a delivery stops moving in. A receipt taken on any of
/// them is final; anything else is still in the pipeline.
pub(crate) fn receipt_resolved(s: DeliveryState) -> bool {
    matches!(
        s,
        DeliveryState::DeliveredVerified
            | DeliveryState::DeliveredUnverified
            | DeliveryState::AttentionRequired
            | DeliveryState::ParkedBlockedQuota
    )
}

/// Queued means nothing has been typed into the pane yet.
///
/// That covers the three states where the delivery is waiting on the
/// recipient rather than on cyclops: behind another message, held at the
/// gate for a turn to end or a human to stop typing, or waiting out a
/// bounded retry. From the sender's side those are one thing, and the
/// queue position is the honest detail.
///
/// Past the paste it is a different fact and it may not wear the same
/// word: the payload is in the pane, the composer verified it, the submit
/// key may already be sent. Reporting "queued" there tells the sender the
/// opposite of what happened.
pub(crate) fn receipt_is_queued(s: DeliveryState) -> bool {
    matches!(
        s,
        DeliveryState::Queued | DeliveryState::Gating | DeliveryState::RetryQueued
    )
}

/// Attach one durable queued attempt to its recipient's FIFO worker.
///
/// Recipient selection and oldest-pending policy belong to the coordinator.
pub(crate) fn enqueue_notification_attempt(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    display_recipient: &str,
    notification: NotificationContext,
    replace_existing: bool,
) -> Result<Arc<DeliveryHandle>, NotificationEnqueueRefusal> {
    let attempt_id = notification.attempt_id();
    let recipient = notification.recipient();
    let claimed_barrier = notification.claimed_notification_barrier();
    let mut active = inner
        .engine
        .notification_attempts
        .lock()
        .expect("notification attempts lock");
    active.retain(|_, handle| handle.strong_count() > 0);
    if !replace_existing {
        if let Some(handle) = active.get(&attempt_id).and_then(std::sync::Weak::upgrade) {
            drop(active);
            let refusal = inner
                .engine
                .notification_worker_refusal(recipient, attempt_id);
            if claimed_barrier.as_ref().is_ok_and(Option::is_some) {
                handle
                    .claimed_notification_rerun_requested
                    .store(true, Ordering::SeqCst);
            }
            return if let Some(refusal) = refusal {
                Err(refusal)
            } else {
                Ok(handle)
            };
        }
    }
    let claimed_barrier = match claimed_barrier {
        Ok(barrier) => barrier,
        Err(error) => {
            error!(message_id = %notification.message_id(), %error, "cannot classify notification enqueue");
            return Err(NotificationEnqueueRefusal::ClassificationUnavailable);
        }
    };
    let staged = if claimed_barrier.is_some() {
        let record = match notification.current_record() {
            Ok(record) => record,
            Err(error) => {
                error!(message_id = %notification.message_id(), %error, "cannot rebuild claimed staged notification");
                return Err(NotificationEnqueueRefusal::PayloadUnavailable);
            }
        };
        // Recovery owns the durable error result. An empty handle payload is
        // never written on this path; it lets the worker append one exact
        // attention cause for a missing message or unsupported format.
        Some(
            notification
                .message_line()
                .ok()
                .and_then(|message| expected_notification_payload(&record, &message))
                .unwrap_or_default(),
        )
    } else {
        None
    };
    let handle =
        DeliveryHandle::for_notification(display_recipient, pane_id, session_idx, notification);
    if let Some(staged) = staged {
        handle.restore_claimed_notification_barrier(staged);
    }
    active.insert(attempt_id, Arc::downgrade(&handle));
    drop(active);
    inner.engine.track(&handle);
    let task_inner = Arc::clone(inner);
    let worker =
        inner
            .engine
            .enqueue_notification_worker(recipient, Arc::clone(&handle), move |worker| {
                tokio::spawn(notification_worker_supervisor(
                    task_inner, recipient, worker,
                ))
            });
    if let Err(refusal) = worker {
        inner.engine.retire_notification_run(&handle);
        return Err(refusal);
    }
    Ok(handle)
}

/// Wait for a pane's fused state to satisfy `until`, pinned to the pane
/// occupant present at wait start. Event-driven off the fusion broadcast
/// and the session watcher stream; the deadline is the only timer.
///
/// Semantics (protocol spec): idle is fused Idle; blocked is any blocked_*
/// state; turn-ended is an observed Working followed by Idle or IdleWithInput. The
/// current confirmed Working phase or the next confirmed Working phase can
/// provide the first observation. A blocked state mid-sequence keeps waiting
/// rather than passing as turn-ended. This state sequence carries no turn or
/// message identity and says nothing about write readiness.
///
/// Pinning: (pane_id, pane_pid) recorded at start. The pane vanishing,
/// dying, or changing root pid resolves OccupantChanged, never a false
/// success. The watcher emits a PanePid edge for a root replacement, and
/// every other pane wake still rechecks the pin as a fail-closed defense
/// against a delayed or lagged event.
pub(crate) async fn wait_for_pane_state(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    until: WaitUntil,
    timeout: Duration,
) -> WaitEnd {
    let started = Instant::now();
    let deadline = started + timeout;
    let end = |outcome: WaitOutcome, state: AgentState| WaitEnd {
        outcome,
        state,
        waited_ms: started.elapsed().as_millis() as u64,
    };
    // Subscribe before the baseline refresh so the receiver owns the
    // current and future state sequence and stale state cannot replay
    // after the baseline.
    let mut ev_rx = inner.events.subscribe();
    let watcher = inner.watcher_of(session_idx);
    let mut pane_rx = watcher.as_ref().map(|w| w.subscribe());
    if let Some(watcher) = watcher.as_ref() {
        crate::observe_pane(
            inner,
            session_idx,
            watcher,
            pane_id,
            false,
            "agent_wait_baseline",
        )
        .await;
    }
    let mut state = inner.cached_state(session_idx, pane_id);
    // Pin the occupant, in two domains that are NOT interchangeable.
    //
    // `pane_pid` is the process tmux spawned, which for an interactive
    // pane is the shell and never changes for the pane's life. The
    // caller's pin is a delivery's admitted pid, which is the FOREGROUND
    // leader: the agent. Comparing one against the other is how a wait
    // that should have run its course returned occupant_changed the
    // instant it started, on every pane where an agent runs under a
    // shell.
    //
    // So the root pin guards the cheap continuous checks (a respawned
    // pane is an occupant change no matter what is in front), and the
    // foreground pin guards the answer: it is re-resolved before the wait
    // may report Reached, which is the one moment a stale identity would
    // be reported as success. Resolution failure counts as gone.
    let Some(row) = occupant_of(inner, session_idx, pane_id).filter(|r| !r.dead) else {
        return end(WaitOutcome::OccupantChanged, state);
    };
    let pinned_pid = row.pane_pid;
    let Some(pinned_fg) = fusion::foreground_pid_checked(row.pane_pid) else {
        return end(WaitOutcome::OccupantChanged, state);
    };
    // Re-proving the foreground costs a process spawn, so it runs on the
    // wakes that are rare (a pane edge, a reattach, a lagged stream, the
    // moment before success) and not on output, which arrives
    // continuously while an agent streams a turn. Output keeps the cheap
    // root check it always had, which also covers a delayed respawn edge.
    //
    // The pane row's command text is a WAKE, never the answer. It is not
    // an identity: the row can be a queued snapshot older than the pin,
    // and the same process reads "Python" here and "python3" there. Both
    // of those turned a live agent into a false occupant change. So a
    // pane edge re-reads the table and asks ps, and ps decides.
    let gone = |inner: &Arc<Inner>| {
        occupant_gone(inner, session_idx, pane_id, pinned_pid)
            || fusion::foreground_pid_checked(pinned_pid) != Some(pinned_fg)
    };
    let mut working_seen = state == AgentState::Working
        && fusion::cached_working_confirmed(inner, session_idx, pane_id);
    loop {
        if state == AgentState::Dead {
            return end(WaitOutcome::OccupantChanged, state);
        }
        let satisfied = match until {
            WaitUntil::Idle => state == AgentState::Idle,
            WaitUntil::Blocked => state.is_blocked(),
            WaitUntil::TurnEnded => {
                working_seen && matches!(state, AgentState::Idle | AgentState::IdleWithInput)
            }
        };
        if satisfied {
            // The state sequence does not identify a turn. Re-prove the
            // occupant before reporting that this pane reached the requested
            // state.
            if gone(inner) {
                return end(WaitOutcome::OccupantChanged, state);
            }
            return end(WaitOutcome::Reached, state);
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return end(WaitOutcome::Timeout, state),
            ev = ev_rx.recv() => match ev {
                Ok(e)
                    if e.event == "state"
                        && e.data["pane_id"] == pane_id
                        && e.data["session_idx"] == session_idx =>
                {
                    if let Ok(s) = serde_json::from_value::<AgentState>(e.data["state"].clone()) {
                        state = s;
                        if state == AgentState::Working && confirmed_working_state_event(&e) {
                            working_seen = true;
                        }
                    }
                }
                // Attach/detach truth for THIS session comes from
                // `inner.watcher_of(session_idx)`, resolved fresh here,
                // never from matching this event's own `data["name"]`
                // against a name captured at entry: a followed rename
                // changes the live name mid-wait, and a stale snapshot
                // then never matches an attach line again. See the doc
                // comment on `await_ack`'s (this function's sibling wait)
                // event arm for the full failure this avoids.
                Ok(e) if e.event == "session" => match inner.watcher_of(session_idx) {
                    Some(w) if pane_rx.is_none() => {
                        // Reattach: fresh stream, then re-verify the pin
                        // against the live table (the pane may have died
                        // or been replaced during the outage).
                        pane_rx = Some(w.subscribe());
                        if gone(inner) {
                            return end(WaitOutcome::OccupantChanged, state);
                        }
                    }
                    // Detached: fused state stops moving; keep waiting on
                    // the deadline and the reattach edge. Also the no-op
                    // case (already attached, this event was about a
                    // different session or already-applied edge).
                    None => pane_rx = None,
                    _ => {}
                },
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Reconcile on doubt: re-read the cache and the pin.
                    state = inner.cached_state(session_idx, pane_id);
                    if state == AgentState::Working
                        && fusion::cached_working_confirmed(inner, session_idx, pane_id)
                    {
                        working_seen = true;
                    }
                    if gone(inner) {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return end(WaitOutcome::Timeout, state)
                }
            },
            pe = recv_pane(&mut pane_rx) => match pe {
                Ok(PaneEvent::PaneRemoved(id)) if id == pane_id => {
                    return end(WaitOutcome::OccupantChanged, state);
                }
                Ok(PaneEvent::PaneChanged { id, row, .. }) if id == pane_id => {
                    if row.dead || gone(inner) {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Ok(PaneEvent::OutputActivity { pane_id: p, .. }) if p == pane_id => {
                    // Output is not identity, but it is another opportunity
                    // to reject a root replacement before reporting success.
                    if occupant_gone(inner, session_idx, pane_id, pinned_pid) {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Ok(PaneEvent::Disconnected) | Err(broadcast::error::RecvError::Closed) => {
                    pane_rx = None;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if gone(inner) {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Ok(_) => {}
            },
        }
    }
}

/// agent.wait entry for the socket server. Reached answers with the state
/// and waited_ms; timeout and occupant_changed answer as wire errors whose
/// data carries the same fields, so a caller always learns the state the
/// target was in.
pub(crate) async fn agent_wait(
    inner: &Arc<Inner>,
    params: cyclops_proto::AgentWaitParams,
) -> Result<Value, WireError> {
    let Some((session_idx, pane_id)) = inner.resolve_recipient(&params.target) else {
        return Err(wire_err(
            "no_such_target",
            format!("no such target {:?}", params.target),
        ));
    };
    let timeout = params
        .timeout_ms
        .unwrap_or(WAIT_DEFAULT_MS)
        .min(WAIT_MAX_MS);
    let until_word = until_word(params.until);
    let end = wait_for_pane_state(
        inner,
        session_idx,
        &pane_id,
        params.until,
        Duration::from_millis(timeout),
    )
    .await;
    // `outcome` is "reached" on success, and the same word the error code
    // carries otherwise.
    let data = json!({
        "target": params.target,
        "pane_id": pane_id,
        "until": params.until,
        "outcome": end.outcome,
        "state": end.state,
        "waited_ms": end.waited_ms,
    });
    match end.outcome {
        WaitOutcome::Reached => Ok(data),
        WaitOutcome::Timeout => Err(WireError {
            code: "timeout".to_string(),
            message: format!(
                "{} did not reach {until_word} within {timeout}ms; state was {}",
                params.target, end.state
            ),
            data: Some(data),
        }),
        WaitOutcome::OccupantChanged => Err(WireError {
            code: "occupant_changed".to_string(),
            message: format!(
                "the pane behind {} died or changed occupant while waiting",
                params.target
            ),
            data: Some(data),
        }),
    }
}

/// The wire word for an until mode, for human-facing error copy.
pub(crate) fn until_word(until: WaitUntil) -> &'static str {
    match until {
        WaitUntil::Idle => "idle",
        WaitUntil::TurnEnded => "turn ended",
        WaitUntil::Blocked => "blocked",
    }
}

#[cfg(test)]
mod tests;
