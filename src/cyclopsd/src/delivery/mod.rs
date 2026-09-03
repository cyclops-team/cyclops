//! The delivery pipeline (docs/development/DELIVERY.md is the spec, and the flow with
//! its decision points drawn is docs/development/ARCHITECTURE.md).
//!
//! One worker per target pane; deliveries to one recipient are strictly
//! FIFO. Direct-delivery transitions append to the session ledger. Mailbox
//! notification transitions append only to the workspace journal through
//! `NotificationContext`. Failures queue or park; they never drop (limbo is
//! a bug).
//!
//! Four things live here that read like they belong elsewhere, and each
//! one is here because it needs the same handle the pipeline holds:
//! `admin_notify` (a ping usually points at a delivery), `close_limbo`
//! (the retained restart sweep, entered only through `compatibility.rs`),
//! `agent_wait` and
//! `wait_pinned` (send-and-wait pins on the pid the submit went to), and
//! `About`, the item a ping names so a reader can stop showing it.
//!
//! What is NOT decided here: whether a pane is idle (`fusion.rs`), which
//! keys dismiss a modal (`cyclops-manifest` data), whether a finished
//! delivery still needs a human (`cyclops_proto::attention`), and how any
//! of it is worded for a person (the CLI).
//!
//! Zero-polling shape: workers sleep on queue notifies and wake on watcher
//! or fusion events. Every timer is a one-shot tied to one delivery: the
//! paste verification re-reads, the tier-1 ACK window, the screen-evidence
//! checkpoints, the decline-key spacing, the idle-ambiguous-composer settle
//! deadline, the gate's single wedged-hold ping, and the two deadlines a
//! caller asked for (`receipt_block_ms`, and `timeout_ms` on a wait). Nothing
//! runs on an interval.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_manifest::{
    mailbox_capability, strip_csi, AckEvidence, Manifest, UnstyledComposerProof,
};
use cyclops_proto::{
    AgentState, ComposerSemantic, ComposerState, Delivery, DeliveryReceipt, DeliveryState,
    Detection, Event, Kind, LedgerLine, MsgSendParams, MsgSendResult, NotificationAttemptId,
    NotificationAttentionCause, NotificationBinding, NotificationManifestId,
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

pub(crate) mod terminal;
pub(crate) use terminal::{
    clean_composer_proof, parse_reset_hint, prompt_matches, render_canonical_message_payload,
    sentinel_for, visible_clean_composer_proof,
};

#[cfg(test)]
use cyclops_proto::NotificationRouteEvidenceId;

/// Delivery gives up on evidence this long after submit (spec: neither ACK
/// tier within 5s goes to retry_queued).
const SCREEN_ACK_DEADLINE: Duration = Duration::from_secs(5);
/// One-shot screen-evidence checkpoints after submit. Events also wake the
/// waiter; these bound the captures per delivery.
const ACK_CHECKPOINTS_MS: [u64; 5] = [250, 750, 1500, 3000, 5000];
/// Post-paste and final-staging verification re-reads. A terminal renderer
/// can lag one frame behind a paste or expose a partial repaint between two
/// otherwise exact proofs. Offsets from the preceding write or reread, one
/// capture each; bounded per attempt.
const VERIFY_DELAYS_MS: [u64; 4] = [0, 120, 240, 480];
/// Bottom non-empty lines scanned for the staged verify pattern.
const VERIFY_REGION: usize = 15;
/// Bottom non-empty lines treated as the composer zone for the
/// marker-left-composer check.
const COMPOSER_WINDOW: usize = 6;
/// Spacing between manifest-defined modal decline keys.
const DECLINE_SPACING: Duration = Duration::from_millis(250);
/// Attempts to auto-dismiss one modal rule before treating it as
/// non-dismissable (hold plus admin notify). Never loop.
const MAX_DECLINES: u32 = 3;
/// Default and ceiling for agent.wait / send-and-wait timeouts.
const WAIT_DEFAULT_MS: u64 = 60_000;
const WAIT_MAX_MS: u64 = 600_000;
/// Default and ceiling for `daemon.quiesce`: how long to wait for
/// deliveries already past the paste to resolve. Past-the-paste windows
/// are seconds by construction (the verify re-reads and ACK deadline
/// above), so a small bound covers the honest case and a caller cannot
/// wedge the pipeline with a huge one.
const QUIESCE_DEFAULT_MS: u64 = 5_000;
const QUIESCE_MAX_MS: u64 = 30_000;
/// How long a quiet quiesce holds the pipeline still waiting for the stop
/// that should follow. If none arrives (the caller died between the
/// answer and the signal), the pipeline un-holds itself rather than
/// freezing deliveries forever.
const QUIESCE_HOLD_FALLBACK_MS: u64 = 30_000;
/// Upgradeable delivered_unverified handles kept per pane for late hook
/// ACK upgrades.
const ACK_REGISTRY_CAP: usize = 32;
const NO_LONGER_CURRENT_BEFORE_WRITE: &str = "notification_no_longer_current";
const NOTIFICATION_RECORD_FAILED: &str = "notification_record_failed";
const CLAIMED_STAGED_SETTLEMENT_FAILED: &str = "claimed_staged_settlement_failed";
#[derive(Debug, Clone)]
struct MailboxCapabilityProof {
    recipient: RecipientKey,
    agent: crate::identity::ProcId,
    manifest: String,
    file: PathBuf,
    expected_digest: [u8; 32],
}

struct AttemptPayload {
    bytes: String,
    transport: Option<NotificationTransport>,
    doorbell_format: Option<u32>,
    capability: Option<MailboxCapabilityProof>,
    capability_required: bool,
}

impl AttemptPayload {
    fn required_pane_width(&self) -> Option<u32> {
        None
    }
}

impl MailboxCapabilityProof {
    fn recheck(
        &self,
        recipient: RecipientKey,
        agent: crate::identity::ProcId,
        manifest: &str,
    ) -> bool {
        self.recipient == recipient
            && self.agent == agent
            && self.manifest == manifest
            && mailbox_capability::file_digest(&self.file) == Some(self.expected_digest)
    }
}

/// Recheck format-specific authority before considering terminal geometry.
///
/// Capability loss is an identity failure and must not be hidden by a narrow
/// pane. Both pre-write bookends use this exact ordering.
fn notification_prewrite_bookend(
    selected: &AttemptPayload,
    recipient: Option<RecipientKey>,
    binding: &fusion::Binding,
    pane_width: u32,
) -> Option<String> {
    if matches!(selected.transport, Some(NotificationTransport::Doorbell))
        && selected.capability_required
    {
        let current =
            recipient
                .zip(selected.capability.as_ref())
                .is_some_and(|(recipient, proof)| {
                    proof.recheck(recipient, binding.agent, &binding.manifest)
                });
        if !current {
            return Some("capability_changed".to_string());
        }
    }
    if selected
        .required_pane_width()
        .is_some_and(|required| pane_width < required)
    {
        return Some(format!("pane_too_narrow:{pane_width}"));
    }
    None
}

fn select_mailbox_capability(
    manifest: &Manifest,
    recipient: RecipientKey,
    agent: crate::identity::ProcId,
    manifest_id: &str,
) -> Option<MailboxCapabilityProof> {
    if manifest.agent.id != manifest_id {
        return None;
    }
    let declared = manifest.messaging.mailbox_capability_file.as_ref()?;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let file = mailbox_capability::resolve_path(declared, &home)?;
    let expected_digest = mailbox_capability::shipped_digest();
    if mailbox_capability::file_digest(&file) != Some(expected_digest) {
        return None;
    }
    Some(MailboxCapabilityProof {
        recipient,
        agent,
        manifest: manifest_id.to_string(),
        file,
        expected_digest,
    })
}

fn select_attempt_payload(
    handle: &DeliveryHandle,
    manifest: &Manifest,
    observed: Option<&fusion::Binding>,
    _pane_width: Option<u32>,
) -> Result<AttemptPayload, NotificationAdapterError> {
    let Some(notification) = &handle.notification else {
        return Ok(AttemptPayload {
            bytes: handle.payload(),
            transport: None,
            doorbell_format: None,
            capability: None,
            capability_required: false,
        });
    };
    let capability = observed.and_then(|binding| {
        select_mailbox_capability(
            manifest,
            notification.recipient(),
            binding.agent,
            &binding.manifest,
        )
    });
    let message = notification.message_line()?;
    let metadata = message.data.as_ref().and_then(|data| {
        serde_json::from_value::<cyclops_proto::MessageMetadata>(data.clone()).ok()
    });
    if let Some(metadata) = metadata {
        if let Some(summary) = metadata.summary {
            let bytes = cyclops_proto::render_doorbell_v4(
                &metadata.presentation.sender_label,
                &summary,
                notification.attempt_id(),
            );
            return Ok(AttemptPayload {
                bytes,
                transport: Some(NotificationTransport::Doorbell),
                doorbell_format: Some(cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM),
                capability: None,
                capability_required: false,
            });
        }
    }
    let reminder = notification.current_record()?.unclaimed_reminder_count > 0;
    if capability.is_some() || reminder {
        return Ok(AttemptPayload {
            bytes: cyclops_proto::render_doorbell_v3(notification.attempt_id()),
            transport: Some(NotificationTransport::Doorbell),
            doorbell_format: Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            capability,
            capability_required: true,
        });
    }

    Ok(AttemptPayload {
        bytes: render_canonical_message_payload(&message),
        transport: Some(NotificationTransport::DirectPayload),
        doorbell_format: None,
        capability: None,
        capability_required: false,
    })
}

/// Rebuild the exact payload selected at this notification's write boundary.
///
/// Delivery recovery and composer projection share this owner so a transport
/// format cannot be actionable in one path and unprovable in the other.
pub(crate) fn expected_notification_payload(
    record: &cyclops_proto::NotificationRecord,
    message: &LedgerLine,
) -> Option<String> {
    if message.id != record.message_id.as_str() {
        return None;
    }
    match (record.transport, record.doorbell_format) {
        (NotificationTransport::Doorbell, format) => match format {
            None => Some(cyclops_proto::render_legacy_doorbell(&record.message_id)),
            Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM) => {
                Some(cyclops_proto::render_doorbell_v1(&record.message_id))
            }
            Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_CLAIM) => Some(
                cyclops_proto::render_doorbell_v2(&record.message_id, record.attempt_id),
            ),
            Some(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM) => {
                Some(cyclops_proto::render_doorbell_v3(record.attempt_id))
            }
            Some(cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM) => message
                .data
                .as_ref()
                .and_then(|data| {
                    serde_json::from_value::<cyclops_proto::MessageMetadata>(data.clone()).ok()
                })
                .and_then(|metadata| {
                    metadata.summary.map(|summary| {
                        cyclops_proto::render_doorbell_v4(
                            &metadata.presentation.sender_label,
                            &summary,
                            record.attempt_id,
                        )
                    })
                }),
            Some(_) => None,
        },
        (NotificationTransport::DirectPayload, None) => {
            Some(render_canonical_message_payload(message))
        }
        (NotificationTransport::DirectPayload, Some(_)) => None,
    }
}

/// Delivery engine state. Lives in [`Inner`]; all behavior is free
/// functions taking the daemon state so nothing here holds locks across
/// awaits by construction.
pub(crate) struct Engine {
    /// Legacy direct-delivery workers, keyed by exact watched pane route.
    workers: StdMutex<HashMap<PaneKey, LegacyWorker>>,
    /// Active mailbox workers and their tasks, keyed by durable recipient.
    notification_workers: StdMutex<HashMap<RecipientKey, NotificationWorker>>,
    /// Message ids ever issued or seen in the ledgers (unique per ledger).
    issued: StdMutex<HashSet<String>>,
    /// Per-delivery unique tmux buffer names.
    buffer_seq: AtomicU64,
    /// Deliveries awaiting or upgradeable by a hook ACK, per exact route.
    acks: StdMutex<HashMap<PaneKey, Vec<Arc<DeliveryHandle>>>>,
    /// Weak refs to every handle the pipeline has created, for the
    /// quiesce sweep. Pruned as it is read; the pipeline itself never
    /// looks here.
    open: StdMutex<Vec<std::sync::Weak<DeliveryHandle>>>,
    /// Active mailbox notification handles by durable attempt id.
    ///
    /// The workspace record is authoritative. This index only prevents
    /// two in-memory workers from driving the same queued attempt.
    notification_attempts:
        StdMutex<HashMap<NotificationAttemptId, std::sync::Weak<DeliveryHandle>>>,
    /// Set while a quiesce holds the pipeline still: workers finish the
    /// delivery they are on, start no new one, and nothing crosses the
    /// paste boundary (the gate's proceed re-checks it).
    paused: AtomicBool,
    /// Set before shutdown drains task handles. Once set, no worker registry
    /// may publish new work.
    stopping: AtomicBool,
    /// Every spawned task below a registered daemon root. Shutdown waits
    /// for this tree to become empty.
    descendant_tasks: Arc<DescendantTaskDrain>,
    /// Cancels descendant work at the same boundary that closes worker
    /// creation. The journal seal remains the final write barrier.
    descendant_stop: watch::Sender<bool>,
}

#[derive(Default)]
struct DescendantTaskDrain {
    active: AtomicUsize,
    empty: Notify,
}

struct DescendantTaskGuard {
    drain: Arc<DescendantTaskDrain>,
}

impl Drop for DescendantTaskGuard {
    fn drop(&mut self) {
        if self.drain.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drain.empty.notify_one();
        }
    }
}

impl DescendantTaskDrain {
    fn enter(self: &Arc<Self>) -> DescendantTaskGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        DescendantTaskGuard {
            drain: Arc::clone(self),
        }
    }

    async fn wait_empty(&self) {
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
            workers: StdMutex::new(HashMap::new()),
            notification_workers: StdMutex::new(HashMap::new()),
            issued: StdMutex::new(HashSet::new()),
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
        for entry in self.workers.lock().expect("workers lock").values() {
            entry.worker.notify.notify_waiters();
        }
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
    fn track(&self, handle: &Arc<DeliveryHandle>) {
        let mut open = self.open.lock().expect("open handles lock");
        open.retain(|w| w.strong_count() > 0);
        open.push(Arc::downgrade(handle));
    }

    /// Every delivery handle still alive.
    fn open_handles(&self) -> Vec<Arc<DeliveryHandle>> {
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

    fn notification_handle(
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
    fn retire_notification_run(&self, handle: &Arc<DeliveryHandle>) -> bool {
        let Some(notification) = &handle.notification else {
            return false;
        };
        let attempt_id = notification.attempt_id();
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

    fn replace_notification_run(
        &self,
        old: &Arc<DeliveryHandle>,
        new: &Arc<DeliveryHandle>,
    ) -> bool {
        let Some(notification) = &old.notification else {
            return false;
        };
        let attempt_id = notification.attempt_id();
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

    /// Run one synchronous queue action while this exact legacy worker is
    /// published in the registry.
    ///
    /// Retirement takes the same registry lock before checking the worker
    /// queue. Keeping lookup, creation, and queue mutation under that lock
    /// means an idle retirement can never remove a worker between a producer
    /// finding it and publishing the next handle.
    fn with_legacy_worker<T, S, F>(&self, pane: PaneKey, spawn: S, action: F) -> Option<T>
    where
        S: FnOnce(Arc<Worker>) -> JoinHandle<()>,
        F: FnOnce(&Arc<Worker>) -> T,
    {
        let mut entries = self.workers.lock().expect("workers lock");
        if self.stopping.load(Ordering::SeqCst) {
            return None;
        }
        if !entries.contains_key(&pane) {
            let worker = Arc::new(Worker::new());
            let task = spawn(Arc::clone(&worker));
            entries.insert(pane.clone(), LegacyWorker { worker, task });
        }
        let worker = Arc::clone(&entries.get(&pane).expect("worker inserted above").worker);
        Some(action(&worker))
    }

    /// Remove this exact legacy worker only while its FIFO is still empty.
    fn retire_legacy_worker(&self, pane: &PaneKey, worker: &Arc<Worker>) -> bool {
        let mut entries = self.workers.lock().expect("workers lock");
        let Some(entry) = entries.get(pane) else {
            return true;
        };
        if !Arc::ptr_eq(&entry.worker, worker) {
            return true;
        }
        if !worker.is_idle() {
            return false;
        }
        entries.remove(pane);
        true
    }

    fn legacy_worker_is_current(&self, pane: &PaneKey, worker: &Arc<Worker>) -> bool {
        self.workers
            .lock()
            .expect("workers lock")
            .get(pane)
            .is_some_and(|entry| Arc::ptr_eq(&entry.worker, worker))
    }

    /// Un-hold the pipeline and wake every worker.
    fn resume_workers(&self) {
        self.paused.store(false, Ordering::SeqCst);
        for entry in self.workers.lock().expect("workers lock").values() {
            entry.worker.notify.notify_one();
        }
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
    fn enqueue_notification_worker<F>(
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
    fn retire_notification_worker(&self, recipient: RecipientKey, worker: &Arc<Worker>) -> bool {
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
    fn notification_worker_is_current(
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

    pub(crate) fn take_legacy_worker_tasks(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *self.workers.lock().expect("workers lock"))
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
                let notification = handle.notification.as_ref()?;
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

    fn notification_worker_refusal(
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
            .and_then(|handle| handle.notification.as_ref())
            .is_some_and(|notification| notification.attempt_id() == attempt_id);
        if current
            || state.queue.iter().any(|handle| {
                handle
                    .notification
                    .as_ref()
                    .is_some_and(|notification| notification.attempt_id() == attempt_id)
            })
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
        let notif_attempt = current.notification.as_ref().map(|n| n.attempt_id());
        Some((current.msg_id.clone(), notif_attempt))
    }

    #[doc(hidden)]
    pub(crate) fn legacy_worker_current_for_test(&self, key: &PaneKey) -> Option<String> {
        let workers = self.workers.lock().expect("workers lock");
        let entry = workers.get(key)?;
        let state = entry.worker.state.lock().expect("worker state lock");
        state.current.as_ref().map(|c| c.msg_id.clone())
    }

    /// Seed the issued-id set from a ledger so new ids stay unique per
    /// ledger across daemon restarts.
    pub(crate) fn preload_ids(&self, lines: &[LedgerLine]) {
        let mut issued = self.issued.lock().expect("issued lock");
        for l in lines {
            if matches!(l.kind, Kind::Msg | Kind::Fyi) {
                issued.insert(l.id.clone());
            }
        }
    }

    /// Mint a short unique message id, e.g. "m-3f9c2a".
    fn mint_msg_id(&self) -> String {
        self.mint_msg_id_with(|| format!("m-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]))
    }

    fn mint_msg_id_with(&self, mut candidate: impl FnMut() -> String) -> String {
        let mut issued = self.issued.lock().expect("issued lock");
        loop {
            let id = candidate();
            if issued.insert(id.clone()) {
                return id;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn mint_msg_id_from(&self, candidates: &[&str]) -> String {
        let mut candidates = candidates.iter();
        self.mint_msg_id_with(|| {
            candidates
                .next()
                .expect("test candidate sequence exhausted")
                .to_string()
        })
    }
}

struct NotificationWorker {
    worker: Arc<Worker>,
    task: JoinHandle<()>,
}

struct LegacyWorker {
    worker: Arc<Worker>,
    task: JoinHandle<()>,
}

/// Per-recipient FIFO worker. Notification workers sleep on `notify`; legacy
/// workers retire their registry entry once the FIFO becomes idle.
struct Worker {
    state: StdMutex<WorkerState>,
    notify: Notify,
    /// Set when quota parking hit this recipient; carries the reset hint.
    /// Cleared only by an operator recovery verb. Never auto-retried.
    parked: StdMutex<Option<String>>,
}

struct WorkerState {
    queue: VecDeque<Arc<DeliveryHandle>>,
    /// Strong ownership of the exact job removed from the FIFO.
    current: Option<Arc<DeliveryHandle>>,
    /// Visible reason the supervisor stopped restarting this worker.
    fault: Option<String>,
    /// Bounds failures that happen outside an exact current job.
    empty_restarts: u8,
}

impl Worker {
    fn new() -> Self {
        Self {
            state: StdMutex::new(WorkerState {
                queue: VecDeque::new(),
                current: None,
                fault: None,
                empty_restarts: 0,
            }),
            notify: Notify::new(),
            parked: StdMutex::new(None),
        }
    }

    fn enqueue_back(&self, handle: Arc<DeliveryHandle>) {
        self.state
            .lock()
            .expect("worker state lock")
            .queue
            .push_back(handle);
    }

    /// Return the exact in-flight job to the FIFO head without releasing
    /// ownership of it. Used when quiesce closes the gate after admission
    /// but before the pane write.
    fn requeue_current_front(&self, handle: &Arc<DeliveryHandle>) -> bool {
        let mut state = self.state.lock().expect("worker state lock");
        let owns = state
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, handle));
        if owns {
            let current = state.current.take().expect("current checked above");
            state.queue.push_front(current);
        }
        owns
    }

    fn drain_pending(&self) -> Vec<Arc<DeliveryHandle>> {
        self.state
            .lock()
            .expect("worker state lock")
            .queue
            .drain(..)
            .collect()
    }

    fn prepend(&self, handles: Vec<Arc<DeliveryHandle>>) {
        let mut state = self.state.lock().expect("worker state lock");
        for handle in handles.into_iter().rev() {
            state.queue.push_front(handle);
        }
    }

    /// Return the already-owned job after a supervisor restart, or take the FIFO head.
    fn current_or_next(&self) -> Option<Arc<DeliveryHandle>> {
        let mut state = self.state.lock().expect("worker state lock");
        if let Some(current) = &state.current {
            return Some(Arc::clone(current));
        }
        let next = state.queue.pop_front()?;
        state.current = Some(Arc::clone(&next));
        Some(next)
    }

    /// Release this exact in-flight job. False means ownership already moved,
    /// such as a quiesce handback or a supervisor replacing a failed run.
    fn finish(&self, handle: &Arc<DeliveryHandle>) -> bool {
        let mut state = self.state.lock().expect("worker state lock");
        let owns = state
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, handle));
        if owns {
            state.current = None;
            state.empty_restarts = 0;
        }
        owns
    }

    fn replace_current(&self, old: &Arc<DeliveryHandle>, new: Arc<DeliveryHandle>) -> bool {
        let mut state = self.state.lock().expect("worker state lock");
        let owns = state
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, old));
        if owns {
            state.current = Some(new);
        }
        owns
    }

    #[cfg(test)]
    fn current(&self) -> Option<Arc<DeliveryHandle>> {
        self.state
            .lock()
            .expect("worker state lock")
            .current
            .clone()
    }

    fn is_idle(&self) -> bool {
        let state = self.state.lock().expect("worker state lock");
        state.current.is_none() && state.queue.is_empty()
    }

    fn set_fault(&self, cause: impl Into<String>) {
        self.state.lock().expect("worker state lock").fault = Some(cause.into());
    }

    fn is_faulted(&self) -> bool {
        self.state
            .lock()
            .expect("worker state lock")
            .fault
            .is_some()
    }

    /// Deliveries ahead of `handle` from the sender's point of view.
    fn position_of(&self, handle: &Arc<DeliveryHandle>) -> u32 {
        let state = self.state.lock().expect("worker state lock");
        let busy = state.current.is_some() as u32;
        match state.queue.iter().position(|h| Arc::ptr_eq(h, handle)) {
            Some(i) => i as u32 + busy,
            // Not queued: it is the in-flight one (or already resolved).
            None => 0,
        }
    }
}

/// One recipient's delivery, shared between the worker, the ACK matcher,
/// and receipt waiters.
pub(crate) struct DeliveryHandle {
    pub(crate) msg_id: String,
    /// Recipient as addressed (label, or pane id when unlabeled).
    pub(crate) to: String,
    pub(crate) pane_id: String,
    pub(crate) session_idx: usize,
    /// Session files this delivery's state lines append to. Normally just
    /// the hosting session; an unresolvable recipient records into every
    /// session file that carries the msg line, so each file stays a
    /// complete stream.
    ledger_sessions: Vec<usize>,
    payload: StdMutex<String>,
    /// Durable one-shot notification facts emitted at this worker's real boundaries.
    notification: Option<NotificationContext>,
    /// Payload shape persisted at the notification write boundary.
    notification_transport: StdMutex<Option<cyclops_proto::NotificationTransport>>,
    state: StdMutex<HandleState>,
    state_tx: watch::Sender<DeliveryState>,
    /// Wakes the worker when the ACK matcher resolved this delivery.
    ack: Notify,
    /// Wakes a held gate after the same claim fact withdraws its attempt.
    cancel: Notify,
    /// Working evidence at or after this delivery's submit.
    ///
    /// The legacy composed wait uses this only to reject a working phase
    /// that predates the submit. It does not correlate a turn to a message.
    working_seen: AtomicBool,
    /// A receiver opened before the submit key and handed to a composed wait.
    /// It may contain older broadcasts, so the wait treats it only as a
    /// source of an exact post-submit Working fact. Its state sequence never
    /// becomes the wait's live state.
    post_submit_turn_events: StdMutex<Option<broadcast::Receiver<Event>>>,
    /// pane_pid of the occupant this delivery was submitted to, recorded
    /// right before the submit key. Send-and-wait pins its wait on THIS
    /// occupant, not whoever lives in the pane when the wait starts; an
    /// impostor that swaps in between must read occupant_changed, never a
    /// report about itself. 0 until a submit happened.
    submitted_pid: AtomicI32,
    /// The admitted AGENT identity the submit key reached, birth included
    /// so a reused pid is a different agent rather than an heir to this
    /// delivery's trust.
    submitted_agent: StdMutex<Option<crate::identity::ProcId>>,
    /// When the submit key went out. A screen-lifecycle receipt carries
    /// this mark for diagnosis. Exact lifecycle release ignores it and
    /// matches the manifest-declared TurnKey instead.
    submitted_at_ms: std::sync::atomic::AtomicU64,
    /// The manifest bound to the pane when the submit key was sent. Paired
    /// with `submitted_agent` it identifies the agent instance and vendor
    /// rules that own receipt evidence. A transient foreground tool may
    /// change without transferring that ownership.
    submitted_manifest: StdMutex<Option<String>>,
    /// True once the synchronous write-boundary hook completed.
    write_boundary_crossed: AtomicBool,
    /// One automatic supervisor recovery is allowed for this exact run.
    worker_recoveries: AtomicU64,
    /// A readiness edge observed while this claimed-barrier run was active.
    /// Consumed only after the attempt index releases this handle.
    claimed_notification_rerun_requested: AtomicBool,
    /// This ordinary notification was admitted immediately before paste while
    /// the pane was visibly Working and its composer was positively clean or
    /// ghosted. The staged doorbell naturally reads as input afterward, so
    /// this one-attempt capability carries the pre-paste proof to the final
    /// exact-byte submit check. It is never restored or used by recovery.
    working_clean_submit_admitted: AtomicBool,
}

/// Evidence from a vendor hook that landed before the worker consumed it.
#[derive(Debug, Clone)]
struct PendingAck {
    edge_ms: u64,
    turn: Option<crate::turnkey::TurnKey>,
    evidence: PendingAckEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAckEvidence {
    /// The hook itself proves receipt.
    Receipt,
    /// The hook only proves dispatch. A later visual Working observation
    /// must accept the same correlated turn.
    DispatchPending,
    DispatchAccepted,
}

struct HandleState {
    state: DeliveryState,
    attempts: u32,
    verified_by: Option<VerifiedBy>,
    cause: Option<String>,
    /// Human hint carried into receipts (quota reset, attention cause).
    note: Option<String>,
    /// Normalized gate hold token for the in-flight head, if any.
    held_by: Option<String>,
    /// An acknowledgement that arrived before the delivery reached the
    /// state that consumes one.
    ///
    /// Kept HERE, under the same lock as the state, because installing it
    /// and classifying the state are ONE decision. Read the state, see
    /// `Staged`, and install afterwards, and the worker can move to
    /// `Submitted` and consume in between: the record is then written
    /// after the only read of it, and a valid acknowledgement is lost.
    early_ack: Option<PendingAck>,
    /// Monotonic count of direct-delivery barrier claims, including
    /// refused ones. Mailbox notifications use their durable attempt id.
    /// Separate from `attempts` because a refused claim wrote nothing and
    /// must not cost transport budget.
    claims: u32,
    /// Count of write-boundary refusals that wrote no pane bytes. Attempts
    /// remain append-only, so retry accounting subtracts this cumulative
    /// count rather than charging a refusal as transport work.
    regates: u32,
    /// Binding and capability each receive one immediate re-proof after an
    /// exact pane or readiness edge. Repeated refusal under unchanged
    /// evidence settles as a durable pre-write block.
    regate_reproof_used: [bool; 2],
    /// The barrier claim this delivery currently holds. Set only when a
    /// claim was granted, and compared before any later settlement so a
    /// receipt cannot release a barrier this delivery no longer owns.
    barrier: Option<String>,
}

/// The pre-Enter event receiver travels with the exact delivery it observed.
/// It is consumed only as a fact source by the composed `send --wait` path;
/// the wait itself starts a fresh live receiver after its baseline.
pub(crate) struct SubmittedTurnEvidence {
    events: broadcast::Receiver<Event>,
    handle: Arc<DeliveryHandle>,
}

/// Identity facts a wait must preserve from a preceding delivery.
#[derive(Default)]
pub(crate) struct WaitPin {
    submitted_pid: Option<i32>,
    turn_evidence: Option<SubmittedTurnEvidence>,
}

impl DeliveryHandle {
    /// This attempt's claim on a pane's composer barrier.
    ///
    /// Content-free and unique per claim. Mailbox notifications use the
    /// durable attempt id; direct deliveries use message id plus claim.
    /// Does this hook prompt carry exactly the payload this delivery
    /// rendered? The bytes stay inside the handle; see `prompt_matches`
    /// for why nothing weaker is accepted.
    pub(crate) fn claims_prompt(&self, text: &str) -> bool {
        prompt_matches(text, &self.payload.lock().expect("payload lock"))
    }

    fn barrier_owner(&self) -> String {
        if let Some(notification) = &self.notification {
            return notification.attempt_id().to_string();
        }
        let mut st = self.state.lock().expect("handle state lock");
        st.claims += 1;
        format!("{}#{}", self.msg_id, st.claims)
    }

    /// Is this report from the process and rules the submit key reached?
    ///
    /// False before a submit has happened at all, which is the point: the
    /// ACK registry is deliberately populated earlier so a fast hook is
    /// not missed, and a delivery that has not been submitted has no
    /// binding for a hook to match.
    fn submitted_binding_is(&self, agent: crate::identity::ProcId, manifest_id: &str) -> bool {
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

    fn replace_post_submit_turn_events(&self, events: broadcast::Receiver<Event>) {
        *self
            .post_submit_turn_events
            .lock()
            .expect("post-submit turn events lock") = Some(events);
    }

    fn take_post_submit_turn_evidence(self: &Arc<Self>) -> Option<SubmittedTurnEvidence> {
        self.post_submit_turn_events
            .lock()
            .expect("post-submit turn events lock")
            .take()
            .map(|events| SubmittedTurnEvidence {
                events,
                handle: Arc::clone(self),
            })
    }

    fn new(
        msg_id: &str,
        to: &str,
        pane_id: &str,
        session_idx: usize,
        payload: String,
    ) -> Arc<Self> {
        Self::build(
            msg_id,
            to,
            pane_id,
            session_idx,
            vec![session_idx],
            payload,
            None,
        )
    }

    fn with_ledger_sessions(
        msg_id: &str,
        to: &str,
        pane_id: &str,
        session_idx: usize,
        ledger_sessions: Vec<usize>,
        payload: String,
    ) -> Arc<Self> {
        Self::build(
            msg_id,
            to,
            pane_id,
            session_idx,
            ledger_sessions,
            payload,
            None,
        )
    }

    fn for_notification(
        to: &str,
        pane_id: &str,
        session_idx: usize,
        doorbell: String,
        notification: NotificationContext,
    ) -> Arc<Self> {
        let msg_id = notification.message_id().to_string();
        Self::build(
            &msg_id,
            to,
            pane_id,
            session_idx,
            vec![session_idx],
            doorbell,
            Some(notification),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        msg_id: &str,
        to: &str,
        pane_id: &str,
        session_idx: usize,
        ledger_sessions: Vec<usize>,
        payload: String,
        notification: Option<NotificationContext>,
    ) -> Arc<Self> {
        let (state_tx, _) = watch::channel(DeliveryState::Queued);
        Arc::new(DeliveryHandle {
            msg_id: msg_id.to_string(),
            to: to.to_string(),
            pane_id: pane_id.to_string(),
            session_idx,
            ledger_sessions,
            payload: StdMutex::new(payload),
            notification,
            notification_transport: StdMutex::new(None),
            state: StdMutex::new(HandleState {
                state: DeliveryState::Queued,
                attempts: 0,
                verified_by: None,
                cause: None,
                note: None,
                held_by: None,
                early_ack: None,
                claims: 0,
                regates: 0,
                regate_reproof_used: [false; 2],
                barrier: None,
            }),
            state_tx,
            ack: Notify::new(),
            cancel: Notify::new(),
            working_seen: AtomicBool::new(false),
            post_submit_turn_events: StdMutex::new(None),
            submitted_pid: AtomicI32::new(0),
            submitted_agent: StdMutex::new(None),
            submitted_at_ms: std::sync::atomic::AtomicU64::new(0),
            submitted_manifest: StdMutex::new(None),
            write_boundary_crossed: AtomicBool::new(false),
            worker_recoveries: AtomicU64::new(0),
            claimed_notification_rerun_requested: AtomicBool::new(false),
            working_clean_submit_admitted: AtomicBool::new(false),
        })
    }

    fn payload(&self) -> String {
        self.payload.lock().expect("payload lock").clone()
    }

    fn set_attempt_payload(
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

    fn notification_transport(&self) -> Option<cyclops_proto::NotificationTransport> {
        *self
            .notification_transport
            .lock()
            .expect("notification transport lock")
    }

    fn set_working_clean_submit_admitted(&self, admitted: bool) {
        self.working_clean_submit_admitted
            .store(admitted, Ordering::SeqCst);
    }

    fn working_clean_submit_admitted(&self) -> bool {
        self.working_clean_submit_admitted.load(Ordering::SeqCst)
    }

    fn restore_claimed_notification_barrier(&self) {
        let attempt_id = self
            .notification
            .as_ref()
            .expect("staged recovery belongs to a notification")
            .attempt_id();
        {
            let mut state = self.state.lock().expect("handle state lock");
            state.state = DeliveryState::Staged;
            state.barrier = Some(attempt_id.to_string());
        }
        self.state_tx.send_replace(DeliveryState::Staged);
        *self
            .notification_transport
            .lock()
            .expect("notification transport lock") = Some(NotificationTransport::Doorbell);
        self.write_boundary_crossed.store(true, Ordering::SeqCst);
    }

    /// Direct sends own session delivery state. Mailbox notifications use
    /// only their durable workspace notification record.
    fn owns_session_delivery_state(&self) -> bool {
        self.notification.is_none()
    }

    pub(crate) fn state(&self) -> DeliveryState {
        self.state.lock().expect("handle state lock").state
    }

    fn snapshot(
        &self,
    ) -> (
        DeliveryState,
        u32,
        Option<VerifiedBy>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        let st = self.state.lock().expect("handle state lock");
        (
            st.state,
            st.attempts,
            st.verified_by,
            st.cause.clone(),
            st.note.clone(),
            st.held_by.clone(),
        )
    }

    fn set_hold(&self, hold: Option<&str>) {
        self.state.lock().expect("handle state lock").held_by = hold.map(str::to_string);
    }
}

/// One transition request for [`advance`].
struct Step<'a> {
    next: DeliveryState,
    cause: Option<&'a str>,
    verified_by: Option<VerifiedBy>,
    note: Option<String>,
    /// When the vendor edge that justifies this step was taken, for the
    /// steps that carry one. Passed through rather than looked up again:
    /// the stored hook slot is mutable, so a re-read can hand back a
    /// concurrent Stop instead of the ACK being acted on.
    turn_edge_ms: Option<u64>,
    /// The turn the vendor named in the payload that justifies this step.
    /// Carried for the same reason as the edge: the stored slot is
    /// mutable, and a re-read can hand back a different turn than the one
    /// being acted on.
    turn: Option<crate::turnkey::TurnKey>,
}

impl<'a> Step<'a> {
    fn to(next: DeliveryState) -> Step<'a> {
        Step {
            next,
            cause: None,
            verified_by: None,
            note: None,
            turn_edge_ms: None,
            turn: None,
        }
    }
    fn turn(mut self, turn: Option<crate::turnkey::TurnKey>) -> Step<'a> {
        self.turn = turn;
        self
    }
    fn turn_edge(mut self, ms: u64) -> Step<'a> {
        self.turn_edge_ms = Some(ms);
        self
    }
    fn cause(mut self, c: &'a str) -> Step<'a> {
        self.cause = Some(c);
        self
    }
    fn verified(mut self, v: VerifiedBy) -> Step<'a> {
        self.verified_by = Some(v);
        self
    }
    fn note(mut self, n: String) -> Step<'a> {
        self.note = Some(n);
        self
    }
}

/// Every transition the pipeline performs, checked as legal against
/// `DeliveryState::can_transition_to` by a unit test and a debug assertion
/// in [`advance`].
#[cfg(test)]
const PIPELINE_TRANSITIONS: &[(DeliveryState, DeliveryState)] = {
    use DeliveryState::*;
    &[
        (Queued, Gating),
        (Queued, AttentionRequired),
        (Queued, ParkedBlockedQuota),
        (Gating, Pasting),
        (Gating, RetryQueued),
        (Gating, AttentionRequired),
        (Gating, ParkedBlockedQuota),
        (Pasting, Staged),
        (Pasting, RetryQueued),
        (Pasting, AttentionRequired),
        (Staged, Submitted),
        (Staged, RetryQueued),
        (Staged, AttentionRequired),
        (Submitted, DeliveredVerified),
        (Submitted, DeliveredUnverified),
        (Submitted, RetryQueued),
        (Submitted, AttentionRequired),
        (DeliveredUnverified, DeliveredVerified),
        (RetryQueued, Gating),
        (RetryQueued, AttentionRequired),
    ]
};

/// Write one direct-delivery transition to the record: the `Kind::State`
/// line in every named session ledger, then the matching `delivery-state`
/// event. Mailbox notifications never call this function because their
/// workspace notification record is the sole durable authority.
/// One writer for both the running pipeline ([`advance`]) and the restart
/// closure ([`close_limbo`]): each used to build these by hand, and the
/// restart event had already lost three fields (`verified_by`, `attempts`,
/// `note`) the live one carried.
#[allow(clippy::too_many_arguments)]
fn emit_delivery_state(
    inner: &Arc<Inner>,
    sessions: &[usize],
    msg_id: &str,
    to: &str,
    recipient: Option<RecipientKey>,
    from: DeliveryState,
    next: DeliveryState,
    cause: Option<&str>,
    note: Option<&str>,
    record: &Delivery,
) -> Option<u64> {
    let line = LedgerLine {
        to: vec![to.to_string()],
        deliveries: vec![record.clone()],
        ..daemon_line(
            Kind::State,
            msg_id.to_string(),
            json!({
                "to": to,
                "recipient": recipient,
                "from": from,
                "to_state": next,
                "cause": cause,
            }),
        )
    };
    // Every session file carrying this delivery's msg line gets the state
    // line too; a per-session ledger is a complete stream on its own.
    let mut seq = None;
    for idx in sessions {
        let s = inner.append_line(*idx, line.clone());
        if seq.is_none() {
            seq = s;
        }
    }
    inner.emit(
        "delivery-state",
        json!({
            "id": msg_id,
            "to": to,
            "recipient": recipient,
            "from": from,
            "to_state": next,
            "cause": cause,
            "verified_by": record.verified_by,
            "attempts": record.attempts,
            "note": note,
        }),
        seq,
    );
    seq
}

/// Apply one transition if the delivery is still in an expected state.
/// Returns false when a concurrent actor (ACK matcher vs worker timeout)
/// already moved it; the caller treats that as "someone else resolved it".
/// Legal-transition checking is a debug assertion: an illegal move is a
/// programming bug, and the ledger records what actually happened.
fn advance(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    allowed_from: &[DeliveryState],
    step: Step<'_>,
) -> bool {
    let (from, record) = {
        let mut st = handle.state.lock().expect("handle state lock");
        if !allowed_from.contains(&st.state) {
            return false;
        }
        let from = st.state;
        debug_assert!(
            from.can_transition_to(step.next),
            "illegal delivery transition {from:?} -> {:?}",
            step.next
        );
        if !from.can_transition_to(step.next) {
            error!(
                id = %handle.msg_id,
                ?from,
                to_state = ?step.next,
                "refusing illegal delivery transition"
            );
            return false;
        }
        st.state = step.next;
        if let Some(v) = step.verified_by {
            st.verified_by = Some(v);
        }
        st.cause = step.cause.map(str::to_string);
        if let Some(n) = &step.note {
            st.note = Some(n.clone());
        }
        if step.next != DeliveryState::Gating {
            st.held_by = None;
        }
        (
            from,
            Delivery {
                to: handle.to.clone(),
                state: st.state,
                verified_by: st.verified_by,
                attempts: st.attempts,
                ts: unix_ms(),
                cause: st.cause.clone(),
            },
        )
    };
    if handle.owns_session_delivery_state() {
        emit_delivery_state(
            inner,
            &handle.ledger_sessions,
            &handle.msg_id,
            &handle.to,
            inner.recipient_key(handle.session_idx, &handle.pane_id),
            from,
            step.next,
            step.cause,
            step.note.as_deref(),
            &record,
        );
    }
    // send_replace, not send: watch::Sender::send drops the value when no
    // receiver exists, and receipt blocking subscribes late. A worker that
    // resolves before the subscribe must still leave the state readable, or
    // the receipt waits out its whole cap on an already-final delivery.
    handle.state_tx.send_replace(step.next);
    // A receipt is the first thing that PROVES the composer was consumed:
    // either the vendor acknowledged this message, or the marker left the
    // composer and a turn started. Send-keys returning Ok proves neither.
    // tmux accepting the key says nothing about what the vendor did with
    // it, and a swallowed Enter leaves the payload staged, which is the
    // staged-never-sent class this whole unit exists for.
    //
    // Only the FIRST resolution promotes. The unverified-to-verified
    // upgrade is the same consumption arriving twice, and re-marking it
    // would push the mark past a turn-end edge that has already arrived.
    let first_receipt = from == DeliveryState::Submitted
        && matches!(
            step.next,
            DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
        );
    // A late upgrade carries the correlated edge a screen-only receipt
    // did not have. If that receipt left the hold waiting, this is the
    // evidence it was waiting for, so it settles here too.
    let late_correlated =
        from == DeliveryState::DeliveredUnverified && step.next == DeliveryState::DeliveredVerified;
    if first_receipt || late_correlated {
        settle_hold_on_receipt(
            inner,
            handle,
            step.verified_by,
            step.turn_edge_ms,
            step.turn,
        );
    }
    true
}

/// Append a gate decision line and emit the matching event.
fn gate_line(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    action: &str,
    rule: Option<&str>,
    cause: Option<&str>,
) {
    let recipient = inner.recipient_key(handle.session_idx, &handle.pane_id);
    let line = LedgerLine {
        to: vec![handle.to.clone()],
        ..daemon_line(
            Kind::Gate,
            handle.msg_id.clone(),
            json!({
                "to": handle.to,
                "recipient": recipient,
                "action": action,
                "rule": rule,
                "cause": cause,
            }),
        )
    };
    let seq = inner.append_line(handle.session_idx, line);
    inner.emit(
        "gate",
        json!({
            "id": handle.msg_id,
            "to": handle.to,
            "recipient": recipient,
            "action": action,
            "rule": rule,
            "cause": cause,
        }),
        seq,
    );
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
    pane_id: Option<String>,
    /// The recipient of the delivery the ping is about. That delivery's
    /// id is the ping's own `msg_id`, so this pairs with `Some(msg_id)`.
    to: Option<String>,
    /// Every delivery a ping about SEVERAL of them names, when one ping
    /// covers a batch. The ping's own `msg_id` can only name one, and the
    /// restart closure ends a whole run's worth at once.
    deliveries: Vec<DeliveryRef>,
}

/// One delivery a batch ping points at, keyed the way the register keys
/// it: recipient plus message.
///
/// Named fields, not a pair: both are strings, they sit next to each
/// other, and a transposition would compile and then quietly point every
/// ping at nothing.
pub(crate) struct DeliveryRef {
    pub(crate) to: String,
    pub(crate) msg_id: String,
}

impl About {
    /// A ping about a pane a human must unblock.
    pub(crate) fn pane(pane_id: &str) -> About {
        About {
            pane_id: Some(pane_id.to_string()),
            ..About::default()
        }
    }

    /// A ping about one delivery to `to`. Pass the message id as `msg_id`
    /// or the ping names a recipient without saying which delivery.
    pub(crate) fn delivery(to: &str) -> About {
        About {
            to: Some(to.to_string()),
            ..About::default()
        }
    }

    /// A ping about many deliveries at once. A reader holds it against
    /// the register per item and shows it while ANY of them still stands,
    /// so one summary line cannot outlive the whole batch it summarizes.
    pub(crate) fn deliveries(deliveries: Vec<DeliveryRef>) -> About {
        About {
            deliveries,
            ..About::default()
        }
    }
}

/// Write a kind=system admin notification line and broadcast the event.
/// `session_idx` scopes internal (delivery-driven) notifications to the
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
    if !about.deliveries.is_empty() {
        // The batch form of the `to` field above, spelled the same way:
        // recipient plus message id, which is the register's key for the
        // delivery half. Additive and absent when the ping names one item
        // or none.
        about_fields.insert(
            "deliveries".into(),
            json!(about
                .deliveries
                .iter()
                .map(|d| json!({"to": d.to, "id": d.msg_id}))
                .collect::<Vec<_>>()),
        );
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
/// Deliveries that have not reached a pane do not block quiet: a restart
/// requeues them ([`close_limbo`]). Quiet keeps the pipeline held for the
/// stop that should follow, with a bounded self-release in case the
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

// ---------------------------------------------------------------------------
// Restart recovery
// ---------------------------------------------------------------------------

/// Resolve deliveries a previous daemon run left unresolved. This runs once
/// at boot over the replayed session ledgers. The pre-write boundary decides
/// each chain's fate, using the same boundary
/// the running pipeline retries by:
///
/// - Before the paste (queued, gating, retry_queued): nothing has touched
///   the pane, so the chain is requeued. The payload is rebuilt from the message
///   line, handle re-enqueued, and the delivery re-enters the gate as if
///   the restart were a long hold. One aggregated FYI names them.
/// - Past the paste: the outcome is unknowable from here, so the chain
///   closes as attention_required (cause: daemon_restart) and ONE
///   aggregated action-required admin.notify lists everything closed.
/// - A pre-paste chain whose recipient no longer maps to any pane (label
///   not adopted, session not watched this boot) has nothing to requeue
///   into and closes the same way.
///
/// A msg line's `hosted` list names the recipients whose chains live in
/// that file, so a chain recorded in another session's file is never
/// falsely closed here; a delivery that died before its first state line
/// still closes through its hosted msg record.
pub(crate) fn close_limbo(
    inner: &Arc<Inner>,
    replayed: &[(usize, Vec<LedgerLine>)],
    _boundary: &crate::compatibility::BoundaryToken,
) {
    /// What `render_payload` needs to rebuild a requeued delivery's bytes,
    /// straight off the msg line.
    struct Envelope {
        from: String,
        subject: String,
        body: String,
        fyi: bool,
    }
    struct Chain {
        state: DeliveryState,
        attempts: u32,
        owner: usize,
        owners: BTreeSet<usize>,
        rank: u64,
    }
    fn consider(
        chains: &mut HashMap<(String, String), Chain>,
        key: (String, String),
        state: DeliveryState,
        attempts: u32,
        owner: usize,
        rank: u64,
    ) {
        match chains.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Chain {
                    state,
                    attempts,
                    owner,
                    owners: BTreeSet::from([owner]),
                    rank,
                });
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let current = entry.get_mut();
                current.owners.insert(owner);
                // A terminal fact in the configured root closes unresolved
                // history in a linked journal. Otherwise the family's
                // descendant-first/root-last scan is its causal order; wall
                // clocks from separate journal files are not comparable.
                let terminal_wins = receipt_resolved(state) && !receipt_resolved(current.state);
                let same_class_is_newer = receipt_resolved(state)
                    == receipt_resolved(current.state)
                    && rank > current.rank;
                if terminal_wins || same_class_is_newer {
                    current.state = state;
                    current.attempts = attempts;
                    current.owner = owner;
                    current.rank = rank;
                }
            }
        }
    }
    let workspace_ids = match &inner.mailbox {
        Some(service) => match service.workspace_message_ids() {
            Ok(ids) => ids,
            Err(error) => {
                // Recovery must fail closed when ownership is unreadable.
                // Closing a compatibility copy here would create a second
                // terminal authority for a workspace-owned notification.
                error!(error = %error, "workspace ownership unavailable during limbo recovery");
                return;
            }
        },
        None => HashSet::new(),
    };
    let mut closed: Vec<String> = Vec::new();
    let mut requeued: Vec<String> = Vec::new();
    // The same closures as identities, so the one ping can name them and
    // a reader can hold it to the register (cyclops-ui `App::admits`).
    let mut named: Vec<DeliveryRef> = Vec::new();
    // Families are descendants-first and configured-root-last. Fold all of
    // them before acting so one (message, recipient) can mint at most one
    // recovery handle during this boot.
    let mut chains: HashMap<(String, String), Chain> = HashMap::new();
    let mut envelopes: HashMap<(String, usize), Envelope> = HashMap::new();
    let mut scan_order = 0_u64;
    for (idx, lines) in replayed {
        for line in lines {
            scan_order = scan_order.saturating_add(1);
            if !legacy_recovery_owns(&line.id, &workspace_ids) {
                continue;
            }
            match line.kind {
                Kind::Msg | Kind::Fyi => {
                    envelopes
                        .entry((line.id.clone(), *idx))
                        .or_insert(Envelope {
                            from: line.from.clone(),
                            subject: line.subject.clone().unwrap_or_default(),
                            body: line.body.clone().unwrap_or_default(),
                            fyi: matches!(line.kind, Kind::Fyi),
                        });
                    // `hosted` names the recipients whose chains live in
                    // this file. Ledgers from before the field existed were
                    // single-file: a msg line with no hosted list hosts
                    // every recipient it names, so those chains still get
                    // closed instead of dangling forever.
                    let hosted: Option<HashSet<&str>> = line
                        .data
                        .as_ref()
                        .and_then(|d| d.get("hosted"))
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).collect());
                    for d in &line.deliveries {
                        let is_hosted = hosted.as_ref().is_none_or(|h| h.contains(d.to.as_str()));
                        if is_hosted {
                            consider(
                                &mut chains,
                                (line.id.clone(), d.to.clone()),
                                d.state,
                                d.attempts,
                                *idx,
                                scan_order,
                            );
                        }
                    }
                }
                Kind::State => {
                    let Some(data) = &line.data else { continue };
                    let (Some(to), Ok(state)) = (
                        data["to"].as_str(),
                        serde_json::from_value::<DeliveryState>(data["to_state"].clone()),
                    ) else {
                        continue; // fused-state line, not a delivery line
                    };
                    let record = line.deliveries.first();
                    consider(
                        &mut chains,
                        (line.id.clone(), to.to_string()),
                        state,
                        record.map(|delivery| delivery.attempts).unwrap_or(0),
                        *idx,
                        scan_order,
                    );
                }
                _ => {}
            }
        }
    }
    let mut dangling: Vec<_> = chains
        .into_iter()
        .filter(|(_, chain)| !receipt_resolved(chain.state))
        .collect();
    dangling.sort_by(|a, b| a.0.cmp(&b.0));
    for ((id, to), chain) in dangling {
        if chain.owners.len() != 1 {
            error!(
                message_id = id,
                recipient = to,
                owners = ?chain.owners,
                "legacy restart chain has more than one configured owner; leaving it untouched"
            );
            continue;
        }
        let Chain {
            state,
            attempts,
            owner: idx,
            ..
        } = chain;
        if receipt_is_queued(state) {
            let target = envelopes
                .get(&(id.clone(), idx))
                .zip(requeue_target(inner, &to, idx));
            if let Some((env, (sess_idx, pane_id))) = target {
                let payload = render_payload(&id, &env.from, &env.subject, &env.body, env.fyi);
                // Gating cannot survive the run that was doing the
                // gating; the recorded step back is retry_queued, the
                // pre-paste retry state. Queued and retry_queued are
                // already accurate and re-enter silently.
                let requeue_state = if state == DeliveryState::Gating {
                    let record = Delivery {
                        to: to.clone(),
                        state: DeliveryState::RetryQueued,
                        verified_by: None,
                        attempts,
                        ts: unix_ms(),
                        cause: Some("daemon_restart".to_string()),
                    };
                    emit_delivery_state(
                        inner,
                        &[idx],
                        &id,
                        &to,
                        inner.recipient_key(sess_idx, &pane_id),
                        state,
                        DeliveryState::RetryQueued,
                        Some("daemon_restart"),
                        None,
                        &record,
                    );
                    DeliveryState::RetryQueued
                } else {
                    state
                };
                let handle = DeliveryHandle::with_ledger_sessions(
                    &id,
                    &to,
                    &pane_id,
                    sess_idx,
                    vec![idx],
                    payload,
                );
                {
                    let mut st = handle.state.lock().expect("handle state lock");
                    st.state = requeue_state;
                    st.attempts = attempts;
                }
                inner.engine.track(&handle);
                let Some(()) = with_worker(inner, sess_idx, &pane_id, |worker| {
                    worker.enqueue_back(handle);
                    worker.notify.notify_one();
                }) else {
                    continue;
                };
                requeued.push(format!("{id} -> {to}"));
                continue;
            }
            // No pane to requeue into: close below, like any other
            // chain the restart cannot carry forward.
        }
        // A post-write delivery cannot be requeued after restart. Its
        // mutable recipient label does not prove which pane may still
        // hold the payload, so the ambiguous outcome closes below.
        let record = Delivery {
            to: to.clone(),
            state: DeliveryState::AttentionRequired,
            verified_by: None,
            attempts,
            ts: unix_ms(),
            cause: Some("daemon_restart".to_string()),
        };
        emit_delivery_state(
            inner,
            &[idx],
            &id,
            &to,
            None,
            state,
            DeliveryState::AttentionRequired,
            Some("daemon_restart"),
            None,
            &record,
        );
        closed.push(format!("{id} -> {to}"));
        named.push(DeliveryRef {
            to: to.clone(),
            msg_id: id.clone(),
        });
    }
    if !requeued.is_empty() {
        requeued.sort();
        requeued.dedup();
        // Fyi, not action-required: these deliveries are being handled,
        // and a ping that claims a human is needed while naming nothing a
        // human can do contradicts the calm-view contract.
        admin_notify(
            inner,
            NotifyLevel::Fyi,
            "deliveries requeued after daemon restart",
            &format!(
                "nothing had reached a pane; requeued: {}",
                requeued.join(", ")
            ),
            None,
            None,
            About::default(),
        );
    }
    if closed.is_empty() {
        return;
    }
    closed.sort();
    closed.dedup();
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        "deliveries interrupted by daemon restart",
        &format!(
            "closed as attention_required (cause: daemon_restart): {}",
            closed.join(", ")
        ),
        None,
        None,
        // Every delivery this closed, by name. One ping over many is
        // still a ping about each of them: it claims a human is needed,
        // so a calm stream has to be able to ask the register whether any
        // of them still does. Naming nothing is what put a closed eye
        // over "action required" once the batch had been dealt with.
        About::deliveries(named),
    );
}

fn legacy_recovery_owns(message_id: &str, workspace_ids: &HashSet<String>) -> bool {
    !workspace_ids.contains(message_id)
}

/// Where a requeued delivery should go: the adopted pane for a label, in
/// the session the adoption names, provided that session is watched this
/// boot; or the name itself when it already is a pane id (such a chain
/// lives in the session file that hosted it, which is the session the
/// pane resolved into at send time). None means there is nothing to
/// requeue into and the chain closes instead.
fn requeue_target(inner: &Arc<Inner>, to: &str, hosted_idx: usize) -> Option<(usize, String)> {
    let adopted = {
        let reg = inner.registry.lock().expect("registry lock");
        reg.for_label(to)
            .map(|adoption| (adoption.session.clone(), adoption.pane_id.clone()))
    };
    if let Some((session, pane)) = adopted {
        return inner.session_index(&session).map(|idx| (idx, pane));
    }
    to.starts_with('%').then(|| (hosted_idx, to.to_string()))
}

// ---------------------------------------------------------------------------
// msg.send
// ---------------------------------------------------------------------------

/// Render the injected payload. The daemon builds the envelope; nothing in
/// the request body can forge it (sender identity is structural).
///
/// The legacy direct-payload reply line is dropped in two cases.
///
/// - An `fyi` expects no answer, and offering one invites a reply the
///   sender did not ask for.
/// - `admin` is a mailbox address, not a pane. The durable mailbox path
///   prints `cyclops reply <id>` after claim. This compatibility renderer
///   preserves its older payload shape and offers no pane-addressed hint.
pub fn render_payload(msg_id: &str, from: &str, subject: &str, body: &str, fyi: bool) -> String {
    let mut lines = vec![format!(
        "[cyclops {msg_id}] FROM: {from}  SUBJECT: {subject}"
    )];
    if !body.is_empty() {
        lines.push(body.to_string());
    }
    if !fyi && from != cyclops_proto::label::ADMIN {
        lines.push(format!(
            "Reply: cyclops send {from} --subject \"...\" --summary \"First sentence. Second sentence.\""
        ));
    }
    lines.push(sentinel_for(msg_id));
    lines.join("\n")
}

/// The msg.send entry: ledger the message, fan deliveries out to per-pane
/// workers, and build receipts per DELIVERY.md semantics (block on the
/// idle path up to receipt_block_ms, immediate queued/parked otherwise).
pub(crate) async fn msg_send(
    inner: &Arc<Inner>,
    from: &str,
    params: MsgSendParams,
    _boundary: &crate::compatibility::BoundaryToken,
) -> Result<Value, WireError> {
    if inner.session_count() == 0 {
        return Err(wire_err("no_such_target", "no sessions are watched"));
    }
    let typed = expand_recipients(inner, &params.to)?;

    // Resolve each recipient before writing the msg line, and canonicalize
    // the resolved ones to their ledger name: the pane's label, or the
    // pane id when unlabeled. "%1" and its label are the same recipient;
    // the record carries one name, so history filters match however the
    // sender addressed it. Unresolvable names stay as typed (the
    // attention_required record should show what was asked for).
    let mut resolved: Vec<(String, Option<(usize, String)>)> = Vec::new();
    let mut canonical_seen: HashSet<String> = HashSet::new();
    for n in &typed {
        let target = inner.resolve_recipient(n);
        let name = match &target {
            Some((session_idx, pane_id)) => inner
                .label_for_route(*session_idx, pane_id)
                .unwrap_or_else(|| pane_id.clone()),
            None => n.clone(),
        };
        if canonical_seen.insert(name.clone()) {
            resolved.push((name, target));
        }
    }
    let names: Vec<String> = resolved.iter().map(|(n, _)| n.clone()).collect();

    let msg_id = inner.engine.mint_msg_id();
    let payload = render_payload(&msg_id, from, &params.subject, &params.body, params.fyi);
    let now = unix_ms();
    let deliveries: Vec<Delivery> = names
        .iter()
        .map(|n| Delivery {
            to: n.clone(),
            state: DeliveryState::Queued,
            verified_by: None,
            attempts: 0,
            ts: now,
            cause: None,
        })
        .collect();
    let line = LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id: msg_id.clone(),
        ts: 0,
        kind: if params.fyi { Kind::Fyi } else { Kind::Msg },
        from: from.to_string(),
        to: names.clone(),
        subject: Some(params.subject.clone()),
        body: if params.body.is_empty() {
            None
        } else {
            Some(params.body.clone())
        },
        reply_to: params.reply_to.clone(),
        deliveries,
        data: None,
    };
    // One msg fact. With recipients across sessions the same line lands in
    // each involved per-session file; each file stays a complete stream.
    // Each copy names the recipients whose delivery chains that file hosts,
    // so a restart can tell a chain that belongs elsewhere from one that
    // died before its first state line.
    let mut involved: Vec<usize> = resolved
        .iter()
        .filter_map(|(_, r)| r.as_ref().map(|(idx, _)| *idx))
        .collect();
    involved.sort_unstable();
    involved.dedup();
    if involved.is_empty() {
        involved.push(0);
    }
    let mut first_seq = None;
    for idx in &involved {
        let mut copy = line.clone();
        let mut hosted: Vec<&str> = resolved
            .iter()
            .filter(|(_, r)| r.as_ref().is_some_and(|(i, _)| i == idx))
            .map(|(n, _)| n.as_str())
            .collect();
        // Unresolvable recipients record their chain in every involved
        // file; every file hosts them.
        hosted.extend(
            resolved
                .iter()
                .filter(|(_, r)| r.is_none())
                .map(|(n, _)| n.as_str()),
        );
        copy.data = Some(json!({ "hosted": hosted }));
        let seq = inner.append_line(*idx, copy);
        if first_seq.is_none() {
            first_seq = seq;
        }
    }
    let seq = first_seq.unwrap_or(0);
    // The session ledger owns the legacy payload. The push is a resting-row
    // edge shared by every subscriber, so it carries metadata only; authorized
    // body reads go through msg.history/msg.thread instead.
    inner.emit(
        "msg",
        json!({
            "id": msg_id,
            "from": from,
            "to": names,
            "subject": params.subject,
            "fyi": params.fyi,
            "reply_to": params.reply_to,
        }),
        first_seq,
    );

    // Fan out. Each delivery record advances independently.
    let mut handles: Vec<Arc<DeliveryHandle>> = Vec::with_capacity(resolved.len());
    let mut blocking: Vec<Arc<DeliveryHandle>> = Vec::new();
    for (name, target) in &resolved {
        match target {
            None => {
                // Gate step 1: unresolvable recipient needs a human. The
                // resolution line lands in every file that carries the msg
                // line, never a session the message does not involve.
                let handle = DeliveryHandle::with_ledger_sessions(
                    &msg_id,
                    name,
                    "",
                    involved[0],
                    involved.clone(),
                    String::new(),
                );
                advance(
                    inner,
                    &handle,
                    &[DeliveryState::Queued],
                    Step::to(DeliveryState::AttentionRequired)
                        .cause("no_such_pane")
                        .note(format!("no pane for {name:?}")),
                );
                admin_notify(
                    inner,
                    NotifyLevel::ActionRequired,
                    &format!("delivery to {name} needs attention"),
                    &format!("message {msg_id}: no such pane for {name:?}"),
                    Some(&msg_id),
                    None,
                    About::delivery(name),
                );
                handles.push(handle);
            }
            Some((session_idx, pane_id)) => {
                let handle =
                    DeliveryHandle::new(&msg_id, name, pane_id, *session_idx, payload.clone());
                inner.engine.track(&handle);
                crate::sync_pane_unread(inner, pane_id).await;
                let answers_now = gate_answers_now(inner, *session_idx, pane_id);
                let hold = (!answers_now)
                    .then(|| initial_hold(inner, *session_idx, pane_id).map(str::to_string))
                    .flatten();
                let Some((parked_hint, first_in_line)) =
                    with_worker(inner, *session_idx, pane_id, |worker| {
                        let parked_hint = worker.parked.lock().expect("parked lock").clone();
                        if parked_hint.is_some() {
                            return (parked_hint, false);
                        }
                        let first_in_line = worker.is_idle();
                        if first_in_line {
                            handle.set_hold(hold.as_deref());
                        }
                        worker.enqueue_back(Arc::clone(&handle));
                        worker.notify.notify_one();
                        (None, first_in_line)
                    })
                else {
                    advance(
                        inner,
                        &handle,
                        &[DeliveryState::Queued],
                        Step::to(DeliveryState::AttentionRequired).cause("daemon_stopping"),
                    );
                    handles.push(handle);
                    continue;
                };
                if let Some(hint) = parked_hint {
                    // Parked recipients never auto-retry; new sends park
                    // immediately with the reset hint.
                    advance(
                        inner,
                        &handle,
                        &[DeliveryState::Queued],
                        Step::to(DeliveryState::ParkedBlockedQuota)
                            .cause("blocked_quota")
                            .note(hint),
                    );
                } else {
                    if first_in_line && answers_now {
                        blocking.push(Arc::clone(&handle));
                    }
                }
                handles.push(handle);
            }
        }
    }

    // Receipts: block only where the verdict is coming, capped by
    // receipt_block_ms.
    let deadline = Instant::now() + Duration::from_millis(inner.cfg.receipt_block_ms);
    for handle in &blocking {
        let mut rx = handle.state_tx.subscribe();
        let _ = tokio::time::timeout_at(deadline, rx.wait_for(|s| receipt_resolved(*s))).await;
    }
    let receipts: Vec<DeliveryReceipt> = handles.iter().map(|h| receipt_of(inner, h)).collect();

    let result = MsgSendResult {
        msg_id: msg_id.clone(),
        seq,
        deliveries: receipts,
        inserted: Some(true),
    };
    let mut value = serde_json::to_value(result).expect("msg.send result serializes");

    // Send-and-wait composes agent.wait onto the same call: the wait
    // starts only AFTER the delivery reaches a resolved state
    // (DELIVERY.md), so `turn_ended` can never be satisfied by a turn that
    // predates the delivery. A delivery that ends anywhere but delivered
    // has no turn to watch; its entry reports the delivery state instead
    // of a fabricated wait result. Every entry carries the same
    // {outcome, state, waited_ms} shape agent.wait resolves with.
    if let Some(spec) = &params.wait {
        // Test seam after the initial receipt snapshot but before the
        // combined wait resolves each delivery and begins pane observation.
        // It lets tests order those two boundaries deterministically and is
        // a no-op in production.
        inject_pause(inner, "pre_wait").await;
        let timeout = spec.timeout_ms.unwrap_or(WAIT_DEFAULT_MS).min(WAIT_MAX_MS);
        let wait_deadline = Instant::now() + Duration::from_millis(timeout);
        let mut waits = Vec::new();
        for handle in handles.iter() {
            if handle.pane_id.is_empty() {
                // Every recipient reports (DELIVERY.md). A pane-less
                // recipient resolved at send time (attention_required);
                // there is no pane to watch, so the state is null and the
                // delivery field carries the resolution.
                waits.push(json!({
                    "to": handle.to,
                    "outcome": WaitOutcome::NotDelivered,
                    "state": Value::Null,
                    "waited_ms": 0,
                    "delivery": handle.state(),
                }));
                continue;
            }
            let started = Instant::now();
            let mut rx = handle.state_tx.subscribe();
            let resolved =
                tokio::time::timeout_at(wait_deadline, rx.wait_for(|s| receipt_resolved(*s)))
                    .await
                    .is_ok();
            let delivery_state = handle.state();
            let delivered = matches!(
                delivery_state,
                DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
            );
            if !delivered {
                // Resolved but not delivered: no turn to watch. Unresolved
                // by the deadline: the wait timed out waiting for the
                // delivery itself.
                waits.push(json!({
                    "to": handle.to,
                    "outcome": if resolved {
                        WaitOutcome::NotDelivered
                    } else {
                        WaitOutcome::Timeout
                    },
                    "state": inner.cached_state(handle.session_idx, &handle.pane_id),
                    "waited_ms": started.elapsed().as_millis() as u64,
                    "delivery": delivery_state,
                }));
                continue;
            }
            let remaining = wait_deadline.saturating_duration_since(Instant::now());
            // A working edge after submit is the legacy wait's time bound.
            // It does not identify which message or task the turn handled.
            let working_pre = handle.working_seen.load(Ordering::SeqCst);
            // Pin the wait on the occupant the delivery was SUBMITTED to,
            // not whoever lives in the pane now: an occupant swap between
            // submit and wait start must read occupant_changed instead of
            // answering for the impostor.
            let submitted = handle.submitted_pid.load(Ordering::SeqCst);
            let turn_evidence = handle.take_post_submit_turn_evidence();
            let end = wait_pinned(
                inner,
                handle.session_idx,
                &handle.pane_id,
                spec.until,
                remaining,
                working_pre,
                WaitPin {
                    submitted_pid: (submitted != 0).then_some(submitted),
                    turn_evidence,
                },
            )
            .await;
            waits.push(json!({
                "to": handle.to,
                "outcome": end.outcome,
                "state": end.state,
                "waited_ms": end.waited_ms,
                "delivery": delivery_state,
            }));
        }
        value["wait"] = Value::Array(waits);
    }
    Ok(value)
}

/// Will the gate reach a verdict on this pane without waiting on the agent
/// or the human? That is the question the receipt blocks on, and it is not
/// the same question as "is the pane idle".
///
/// Two shapes answer immediately and they end opposite ways. An idle pane
/// proceeds and the delivery resolves inside the block. A pane no manifest
/// binds is refused: the gate returns no_manifest before it ever looks at a
/// state (MEASURED at 7ms), because nothing can be typed into a pane cyclops
/// cannot read. Reporting that one "queued · 0 ahead" and exiting 0 told the
/// sender their message was on its way to a pane it could never reach.
///
/// Everything else holds: a working pane without positive composer proof, a
/// human mid-keystroke, a modal
/// waiting on a person. Those senders get their queue position now rather
/// than a 2.5s wait for a badge that is not coming, which is the property
/// docs/guides/send.md promises for a busy target.
///
/// The verdict itself stays the gate's: this only decides whether the
/// receipt is worth waiting for. A pane that binds a manifest between this
/// call and the gate simply takes the idle path, and the block is capped
/// either way.
fn gate_answers_now(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> bool {
    let cached = inner.cached_state(session_idx, pane_id);
    if matches!(
        cached,
        AgentState::Idle | AgentState::BlockedQuota | AgentState::Dead
    ) {
        return true;
    }
    let Some(watcher) = inner.watcher_of(session_idx) else {
        // Detached: the gate holds until the session comes back.
        return false;
    };
    // A pane that left the table between resolution and here is answered
    // by the gate's no_such_pane, which is just as immediate.
    match watcher.pane(pane_id) {
        Some(row) => row.dead || fusion::bind_manifest_for(inner, session_idx, &row).is_none(),
        None => true,
    }
}

/// Best synchronous estimate of the first gate hold. This is only a receipt
/// seed; the event-driven gate remains authoritative and replaces or clears
/// it as soon as it evaluates the pane. No capture or retry is introduced.
fn initial_hold(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> Option<&'static str> {
    let Some(watcher) = inner.watcher_of(session_idx) else {
        return Some("session_detached");
    };
    let Some(row) = watcher.pane(pane_id) else {
        return Some("unknown");
    };
    if row.in_mode {
        return Some("pane_in_mode");
    }
    match inner.cached_state(session_idx, pane_id) {
        AgentState::Working => Some("working"),
        AgentState::IdleWithInput => Some("idle_with_input"),
        AgentState::BlockedModal | AgentState::BlockedPermission => Some("blocked"),
        AgentState::Unknown => Some("unknown"),
        AgentState::Idle | AgentState::BlockedQuota | AgentState::Dead => None,
    }
}

fn wire_err(code: &str, msg: impl Into<String>) -> WireError {
    WireError {
        code: code.to_string(),
        message: msg.into(),
        data: None,
    }
}

/// The four states a delivery stops moving in. A receipt taken on any of
/// them is final; anything else is still in the pipeline.
fn receipt_resolved(s: DeliveryState) -> bool {
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
fn receipt_is_queued(s: DeliveryState) -> bool {
    matches!(
        s,
        DeliveryState::Queued | DeliveryState::Gating | DeliveryState::RetryQueued
    )
}

fn receipt_of(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) -> DeliveryReceipt {
    let (state, _, _, cause, note, held_by) = handle.snapshot();
    // The pane the delivery resolved to, so the caller can name it and
    // build the per-pane fix. Empty for a recipient that answered to no
    // pane, which is the one case there is nothing to name.
    let pane = (!handle.pane_id.is_empty()).then(|| handle.pane_id.clone());
    if receipt_resolved(state) {
        return DeliveryReceipt {
            to: handle.to.clone(),
            state,
            notification_state: None,
            quota_state: None,
            notification_settlement: None,
            pre_write_cause: None,
            wake_block: None,
            position: None,
            // The gate's machine cause travels as-is when the daemon had
            // no detail to add; wording it belongs to the surface showing
            // it (cyclops_ui::grid::cause_words), not here.
            note: note.or(cause),
            pane,
            held_by: None,
        };
    }
    if !receipt_is_queued(state) {
        return DeliveryReceipt {
            to: handle.to.clone(),
            state,
            notification_state: None,
            quota_state: None,
            notification_settlement: None,
            pre_write_cause: None,
            wake_block: None,
            position: None,
            note: None,
            pane,
            held_by: None,
        };
    }
    let position = inner
        .engine
        .workers
        .lock()
        .expect("workers lock")
        .get(&PaneKey::new(handle.session_idx, &handle.pane_id))
        .map(|entry| entry.worker.position_of(handle));
    // A prior job can finish between enqueue and this snapshot. If that
    // handoff makes this handle position zero, recover the current target
    // hold synchronously; followers never inherit the head's token.
    let fallback = (position == Some(0) && held_by.is_none())
        .then(|| initial_hold(inner, handle.session_idx, &handle.pane_id).map(str::to_string))
        .flatten();
    let held_by = held_by_for_position(position, held_by, fallback);
    DeliveryReceipt {
        to: handle.to.clone(),
        state: DeliveryState::Queued,
        notification_state: None,
        quota_state: None,
        notification_settlement: None,
        pre_write_cause: None,
        wake_block: None,
        position,
        note: None,
        pane,
        held_by,
    }
}

fn held_by_for_position(
    position: Option<u32>,
    held_by: Option<String>,
    fallback: Option<String>,
) -> Option<String> {
    (position == Some(0))
        .then(|| held_by.or(fallback))
        .flatten()
}

/// Expand the to-list: "*" means every labeled pane (explicit adoption is
/// the broadcast domain). Order is preserved, duplicates dropped.
fn expand_recipients(inner: &Arc<Inner>, to: &[String]) -> Result<Vec<String>, WireError> {
    let mut names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for t in to {
        if t == "*" {
            let mut labels: Vec<String> = inner.labels().into_values().collect();
            labels.sort();
            for l in labels {
                if seen.insert(l.clone()) {
                    names.push(l);
                }
            }
        } else if !t.is_empty() && seen.insert(t.clone()) {
            names.push(t.clone());
        }
    }
    if names.is_empty() {
        return Err(wire_err(
            "bad_request",
            "no recipients: give labels, pane ids, or \"*\" with labeled panes",
        ));
    }
    Ok(names)
}

/// Run one queue publication against the FIFO worker owning one pane.
fn with_worker<T, F>(inner: &Arc<Inner>, session_idx: usize, pane_id: &str, action: F) -> Option<T>
where
    F: FnOnce(&Arc<Worker>) -> T,
{
    let pane = PaneKey::new(session_idx, pane_id);
    let task_inner = Arc::clone(inner);
    let task_pane = pane.clone();
    inner.engine.with_legacy_worker(
        pane,
        move |worker| {
            tokio::spawn(worker_supervisor(
                task_inner,
                task_pane,
                Arc::clone(&worker),
            ))
        },
        action,
    )
}

/// Attach an already-queued mailbox notification to the pane's existing FIFO worker.
///
/// Recipient selection and oldest-pending policy belong to the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationEnqueueRefusal {
    DaemonStopping,
    WorkerFaulted,
    WorkerSupervisorExited,
    AttemptUnowned,
    ClassificationUnavailable,
    PayloadUnavailable,
}

#[allow(dead_code)]
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
    let doorbell = if claimed_barrier.is_some() {
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
        notification
            .message_line()
            .ok()
            .and_then(|message| expected_notification_payload(&record, &message))
            .unwrap_or_default()
    } else {
        cyclops_proto::render_doorbell_v3(notification.attempt_id())
    };
    let handle = DeliveryHandle::for_notification(
        display_recipient,
        pane_id,
        session_idx,
        doorbell,
        notification,
    );
    if claimed_barrier.is_some() {
        handle.restore_claimed_notification_barrier();
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

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Observe every worker-loop exit without waiting for another enqueue.
///
/// The loop child owns normal FIFO work. The supervisor owns its task and
/// classifies every unexpected exit against the exact current handle before
/// starting a new child. A clean task return is not proof of success: only
/// daemon stop, a visible worker fault, or exact notification-registry
/// retirement is expected. Claim cancellation drains into that exact
/// retirement. Quiesce parks the child and is not an exit. Dropping the
/// supervisor aborts its child through `DeliveryTask`.
async fn supervise_worker_task<S, R, E>(mut spawn: S, mut recover: R, mut expected_exit: E)
where
    S: FnMut() -> JoinHandle<()>,
    R: FnMut() -> bool,
    E: FnMut() -> bool,
{
    loop {
        let mut child = DeliveryTask(spawn());
        match child.wait().await {
            Ok(()) if expected_exit() => return,
            Ok(()) => {
                error!("delivery worker loop returned while it still owned its registry slot");
                if !recover() {
                    std::future::pending::<()>().await;
                }
            }
            Err(error) => {
                let exit = if error.is_cancelled() {
                    "cancelled"
                } else if error.is_panic() {
                    "panicked"
                } else {
                    "failed"
                };
                error!(%error, exit, "delivery worker loop failed");
                if expected_exit() {
                    return;
                }
                if !recover() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

fn recover_outer_worker(inner: &Arc<Inner>, worker: &Arc<Worker>) -> bool {
    if let Some(handle) = worker.current_or_next() {
        return recover_failed_job(inner, worker, &handle);
    }
    let mut state = worker.state.lock().expect("worker state lock");
    if state.empty_restarts == 0 {
        state.empty_restarts = 1;
        true
    } else {
        state.fault = Some("worker loop failed repeatedly without an owned job".into());
        false
    }
}

async fn worker_supervisor(inner: Arc<Inner>, pane: PaneKey, worker: Arc<Worker>) {
    supervise_worker_task(
        || {
            inner.engine.spawn_descendant_task(worker_loop(
                Arc::clone(&inner),
                pane.clone(),
                Arc::clone(&worker),
            ))
        },
        || recover_outer_worker(&inner, &worker),
        || {
            inner.engine.is_stopping()
                || worker.is_faulted()
                || !inner.engine.legacy_worker_is_current(&pane, &worker)
        },
    )
    .await;
}

async fn notification_worker_supervisor(
    inner: Arc<Inner>,
    recipient: RecipientKey,
    worker: Arc<Worker>,
) {
    supervise_worker_task(
        || {
            inner.engine.spawn_descendant_task(notification_worker_loop(
                Arc::clone(&inner),
                recipient,
                Arc::clone(&worker),
            ))
        },
        || recover_outer_worker(&inner, &worker),
        || {
            inner.engine.is_stopping()
                || worker.is_faulted()
                || !inner
                    .engine
                    .notification_worker_is_current(recipient, &worker)
        },
    )
    .await;
}

async fn worker_loop(inner: Arc<Inner>, pane: PaneKey, worker: Arc<Worker>) {
    loop {
        // A quiesce holds the pipeline still: finish nothing new until
        // resume_workers notifies. Jobs stay queued (pre-paste, safe
        // across the restart the quiesce is for).
        if inner.engine.paused.load(Ordering::SeqCst) {
            worker.notify.notified().await;
            continue;
        }
        let job = worker.current_or_next();
        match job {
            Some(handle) => {
                let parked_hint =
                    legacy_park_hint(&handle, worker.parked.lock().expect("parked lock").clone());
                if let Some(hint) = parked_hint {
                    // A job that raced in around the parking moment parks
                    // too. Workspace notifications bypass this legacy
                    // flag and let their own gate hold on live quota state.
                    advance(
                        &inner,
                        &handle,
                        &[DeliveryState::Queued],
                        Step::to(DeliveryState::ParkedBlockedQuota)
                            .cause("blocked_quota")
                            .note(hint),
                    );
                    worker.finish(&handle);
                    continue;
                }
                match supervised_process(&inner, &worker, &handle).await {
                    Ok(()) => {
                        worker.finish(&handle);
                    }
                    Err(error) => {
                        error!(id = %handle.msg_id, %error, "legacy delivery worker failed");
                        if !recover_failed_job(&inner, &worker, &handle) {
                            return;
                        }
                    }
                }
            }
            None => {
                if inner.engine.retire_legacy_worker(&pane, &worker) {
                    return;
                }
            }
        }
    }
}

async fn notification_worker_loop(inner: Arc<Inner>, recipient: RecipientKey, worker: Arc<Worker>) {
    loop {
        if inner.engine.paused.load(Ordering::SeqCst) {
            if inner.engine.retire_notification_worker(recipient, &worker) {
                return;
            }
            worker.notify.notified().await;
            continue;
        }
        let job = worker.current_or_next();
        let Some(handle) = job else {
            if inner.engine.retire_notification_worker(recipient, &worker) {
                return;
            }
            continue;
        };
        match supervised_process(&inner, &worker, &handle).await {
            Ok(()) => {
                if worker.is_faulted() {
                    return;
                }
                // Quiesce may have atomically returned this same handle to
                // the FIFO. In that case its attempt index remains active.
                if worker.finish(&handle) {
                    let retired = inner.engine.retire_notification_run(&handle);
                    if retired
                        && handle
                            .claimed_notification_rerun_requested
                            .swap(false, Ordering::SeqCst)
                    {
                        if let Some(notification) = &handle.notification {
                            if let Some(messaging) = inner.workspace_messaging() {
                                if let Err(error) =
                                    messaging.notification_head_changed(notification.recipient())
                                {
                                    error!(
                                        id = %handle.msg_id,
                                        %error,
                                        "cannot reschedule claimed notification after readiness edge"
                                    );
                                }
                            } else {
                                error!(
                                    id = %handle.msg_id,
                                    "cannot reschedule claimed notification without workspace messaging"
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => {
                error!(id = %handle.msg_id, %error, "notification delivery worker failed");
                if !recover_failed_job(&inner, &worker, &handle) {
                    return;
                }
            }
        }
    }
}

/// Abort the child if its owning worker is cancelled during shutdown.
struct DeliveryTask(JoinHandle<()>);

impl DeliveryTask {
    async fn wait(&mut self) -> Result<(), tokio::task::JoinError> {
        (&mut self.0).await
    }
}

impl Drop for DeliveryTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn supervised_process(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
) -> Result<(), tokio::task::JoinError> {
    let task_inner = Arc::clone(inner);
    let worker = Arc::clone(worker);
    let handle = Arc::clone(handle);
    let mut task = DeliveryTask(inner.engine.spawn_descendant_task(async move {
        process(&task_inner, &worker, &handle).await;
    }));
    task.wait().await
}

/// Classify one panicked job from its exact durable boundary.
///
/// True restarts the worker loop. False leaves a visible fault and stops it.
fn recover_failed_job(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    let recovery = handle.worker_recoveries.fetch_add(1, Ordering::SeqCst);
    if let Some(notification) = &handle.notification {
        let current = match notification.current_record() {
            Ok(current) => current,
            Err(error) => {
                worker.set_fault(format!("notification recovery failed: {error}"));
                return false;
            }
        };
        if current.attempt_id != notification.attempt_id()
            || current.execution_epoch() != notification.run_epoch()
        {
            // Another run now owns this attempt. The stale task has no fact left to write.
            worker.finish(handle);
            inner.engine.retire_notification_run(handle);
            return true;
        }
        match current.state {
            NotificationState::Queued | NotificationState::Gating if recovery == 0 => {
                let fresh = DeliveryHandle::for_notification(
                    &handle.to,
                    &handle.pane_id,
                    handle.session_idx,
                    cyclops_proto::render_doorbell_v3(notification.attempt_id()),
                    notification.clone(),
                );
                fresh.worker_recoveries.store(1, Ordering::SeqCst);
                if !worker.replace_current(handle, Arc::clone(&fresh))
                    || !inner.engine.replace_notification_run(handle, &fresh)
                {
                    worker.set_fault("notification recovery lost exact job ownership");
                    worker.finish(&fresh);
                    return false;
                }
                true
            }
            NotificationState::Queued | NotificationState::Gating => {
                let Some(messaging) = inner.workspace_messaging() else {
                    worker.set_fault("notification recovery has no workspace messaging Module");
                    return false;
                };
                let block = match messaging.record_worker_failed_prewrite(notification) {
                    Ok(MessagingPreWriteBlockOutcome::Recorded(block)) => Some(block),
                    Ok(MessagingPreWriteBlockOutcome::Obsolete) => None,
                    Err(error) => {
                        worker.set_fault(format!("notification recovery failed: {error}"));
                        return false;
                    }
                };
                if let Some(block) = block {
                    notify_notification_prewrite_blocked(inner, handle, &block);
                }
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
            NotificationState::Staged
                if recovery == 0
                    && notification
                        .claimed_notification_barrier()
                        .is_ok_and(|barrier| barrier.is_some()) =>
            {
                true
            }
            NotificationState::Writing
            | NotificationState::Staged
            | NotificationState::Submitting
            | NotificationState::Submitted => {
                if let Err(error) = notification
                    .record_attention(NotificationAttentionCause::TransportOutcomeUnknown)
                {
                    worker.set_fault(format!("notification recovery failed: {error}"));
                    return false;
                }
                unregister_ack(inner, handle);
                let _ = advance(
                    inner,
                    handle,
                    &[
                        DeliveryState::Pasting,
                        DeliveryState::Staged,
                        DeliveryState::Submitted,
                        DeliveryState::RetryQueued,
                    ],
                    Step::to(DeliveryState::AttentionRequired).cause("worker_failed_after_write"),
                );
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
            NotificationState::Notified => {
                unregister_ack(inner, handle);
                if current.transport == NotificationTransport::DirectPayload {
                    if let Err(error) = notification.record_delivered_direct() {
                        worker.set_fault(format!("direct settlement recovery failed: {error}"));
                        return false;
                    }
                    let recipient = notification.recipient();
                    if let Some(messaging) = inner.workspace_messaging() {
                        if let Err(error) = messaging.direct_delivery_settled(recipient) {
                            worker
                                .set_fault(format!("direct settlement scheduling failed: {error}"));
                            return false;
                        }
                    } else {
                        worker.set_fault(
                            "direct settlement scheduling failed: workspace messaging unavailable",
                        );
                        return false;
                    }
                }
                let _ = advance(
                    inner,
                    handle,
                    &[DeliveryState::Submitted],
                    Step::to(DeliveryState::DeliveredUnverified)
                        .cause("worker_recovered_notified")
                        .verified(VerifiedBy::Screen),
                );
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
            NotificationState::BlockedPreWrite
            | NotificationState::QuotaHeld
            | NotificationState::QuotaResetObserved
            | NotificationState::SubmittedUnverified
            | NotificationState::AttentionRequired
            | NotificationState::Withdrawn
            | NotificationState::WithdrawnAfterStaging
            | NotificationState::WithdrawnByOperator
            | NotificationState::Superseded => {
                worker.finish(handle);
                inner.engine.retire_notification_run(handle);
                true
            }
        }
    } else if !handle.write_boundary_crossed.load(Ordering::SeqCst) && recovery == 0 {
        let state = handle.state();
        if state != DeliveryState::Queued
            && state != DeliveryState::RetryQueued
            && !advance(
                inner,
                handle,
                &[DeliveryState::Gating, DeliveryState::Pasting],
                Step::to(DeliveryState::RetryQueued).cause("worker_recovered_before_write"),
            )
        {
            worker.set_fault("legacy pre-write recovery could not requeue its exact job");
            return false;
        }
        true
    } else {
        let cause = if handle.write_boundary_crossed.load(Ordering::SeqCst) {
            "worker_failed_after_write"
        } else {
            "worker_failed_before_write"
        };
        let moved = advance(
            inner,
            handle,
            &[
                DeliveryState::Queued,
                DeliveryState::Gating,
                DeliveryState::Pasting,
                DeliveryState::Staged,
                DeliveryState::Submitted,
                DeliveryState::RetryQueued,
            ],
            Step::to(DeliveryState::AttentionRequired).cause(cause),
        );
        if moved {
            notify_attention(inner, handle, cause);
        }
        worker.finish(handle);
        if !moved {
            worker.set_fault(format!("legacy recovery stopped in {:?}", handle.state()));
            return false;
        }
        true
    }
}

/// Persist one known pre-write block without releasing FIFO ownership on an
/// uncertain journal append.
///
/// A successful or stale append lets the worker retire normally. A storage
/// failure faults the worker, so its current handle remains ahead of every
/// later notification for this recipient.
async fn persist_notification_prewrite_block(
    inner: &Arc<Inner>,
    worker: &Worker,
    handle: &DeliveryHandle,
    cause: NotificationPreWriteCause,
    observation: Option<NotificationPreWriteObservation>,
) {
    let Some(notification) = &handle.notification else {
        return;
    };
    let route_evidence = inner.route_evidence_id(handle.session_idx, &handle.pane_id);
    // Test seam: an admitting edge can land between the gate's verdict and
    // this append. The reconcile below must catch it, never strand it.
    inject_pause(inner, "pre_prewrite_block").await;
    let Some(messaging) = inner.workspace_messaging() else {
        worker.set_fault("notification pre-write block has no workspace messaging Module");
        return;
    };
    let block = match messaging.record_notification_prewrite_block(
        notification,
        cause,
        observation,
        route_evidence,
        handle.session_idx,
        &handle.pane_id,
    ) {
        Ok(MessagingPreWriteBlockOutcome::Recorded(block)) => block,
        Ok(MessagingPreWriteBlockOutcome::Obsolete) => return,
        Err(error) => {
            error!(id = %notification.message_id(), %error, "notification pre-write block fact failed");
            worker.set_fault(format!(
                "notification pre-write block storage failed: {error}"
            ));
            return;
        }
    };
    notify_notification_prewrite_blocked(inner, handle, &block);
    // A positive route edge can race just ahead of this append and see only
    // Gating. Re-observe after the durable block exists so that edge is not
    // lost. The Module enforces the one-reopen limit and exact binding checks.
    if let Some(watcher) = inner.watcher_of(handle.session_idx) {
        let route_evidence = inner.route_evidence_id(handle.session_idx, &handle.pane_id);
        crate::observe_pane_for_route_evidence(
            inner,
            handle.session_idx,
            &watcher,
            &handle.pane_id,
            true,
            "prewrite_block_reconcile",
            &route_evidence,
        )
        .await;
        messaging.notification_prewrite_blocked(handle.session_idx, &handle.pane_id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegateAction {
    ImmediateReproof,
    Hold,
    BlockPreWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegateCause {
    BarrierHeld,
    BindingChanged,
    CapabilityChanged,
}

impl RegateCause {
    fn reproof_slot(self) -> Option<usize> {
        match self {
            Self::BarrierHeld => None,
            Self::BindingChanged => Some(0),
            Self::CapabilityChanged => Some(1),
        }
    }
}

/// A held composer waits for its owner to release it. Binding and capability
/// races receive one immediate re-proof per exact evidence generation.
fn regate_action(handle: &DeliveryHandle, cause: RegateCause) -> RegateAction {
    let mut state = handle.state.lock().expect("handle state lock");
    state.regates = state.regates.saturating_add(1);
    match cause {
        RegateCause::BarrierHeld => {
            if handle.notification.is_some() {
                RegateAction::Hold
            } else {
                RegateAction::BlockPreWrite
            }
        }
        RegateCause::BindingChanged | RegateCause::CapabilityChanged => {
            let slot = cause.reproof_slot().expect("reproof slot");
            if state.regate_reproof_used[slot] {
                RegateAction::BlockPreWrite
            } else {
                state.regate_reproof_used[slot] = true;
                RegateAction::ImmediateReproof
            }
        }
    }
}

fn reset_immediate_regates(handle: &DeliveryHandle) {
    handle
        .state
        .lock()
        .expect("handle state lock")
        .regate_reproof_used = [false; 2];
}

/// Result of checking the live route against its durable mailbox binding.
enum HandleRoute {
    Exact(Arc<SessionWatcher>),
    BindingChanged,
    BindingUnprovable { pane_pid: i32 },
    Unavailable,
}

/// Classify the live watcher without treating an identity mismatch as absence.
///
/// A pane replacement can reach the watcher before the ordered registry event.
/// That route is present but changed, so the pre-write barrier must record a
/// reprovable identity change instead of a permanent session-unavailable block.
fn handle_route(inner: &Inner, handle: &DeliveryHandle) -> HandleRoute {
    let Some(notification) = &handle.notification else {
        return inner
            .watcher_of(handle.session_idx)
            .map(HandleRoute::Exact)
            .unwrap_or(HandleRoute::Unavailable);
    };
    let recipient = notification.recipient();
    let Some(session_instance_id) = recipient.session_instance_id() else {
        return HandleRoute::Unavailable;
    };
    let Some(pane_id) = recipient.pane_id() else {
        return HandleRoute::Unavailable;
    };
    if pane_id.to_string() != handle.pane_id {
        return HandleRoute::BindingChanged;
    }
    let Some(slot) = inner.session(handle.session_idx) else {
        return HandleRoute::Unavailable;
    };
    let watcher = {
        let link = slot.link.lock().expect("session link lock");
        if !link.attached
            || link
                .identity
                .as_ref()
                .map(|identity| identity.session_instance_id())
                != Some(session_instance_id)
        {
            return HandleRoute::Unavailable;
        }
        link.watcher.as_ref().map(Arc::clone)
    };
    let Some(watcher) = watcher else {
        return HandleRoute::Unavailable;
    };
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return HandleRoute::Unavailable;
    };
    let Some(root) = crate::identity::ProcId::of(row.pane_pid) else {
        return HandleRoute::BindingUnprovable {
            pane_pid: row.pane_pid,
        };
    };
    let Ok(pane_root) = ProcessInstanceId::new(root.pid, root.birth) else {
        return HandleRoute::BindingUnprovable {
            pane_pid: row.pane_pid,
        };
    };
    let registry = inner.registry.lock().expect("registry lock");
    if registry.for_route(recipient, pane_root).is_some() {
        HandleRoute::Exact(watcher)
    } else if registry.for_recipient(recipient).is_some() {
        HandleRoute::BindingChanged
    } else {
        HandleRoute::BindingUnprovable {
            pane_pid: row.pane_pid,
        }
    }
}

/// Resolve only a watcher whose durable route binding is exact.
fn watcher_for_handle(inner: &Inner, handle: &DeliveryHandle) -> Option<Arc<SessionWatcher>> {
    match handle_route(inner, handle) {
        HandleRoute::Exact(watcher) => Some(watcher),
        HandleRoute::BindingChanged
        | HandleRoute::BindingUnprovable { .. }
        | HandleRoute::Unavailable => None,
    }
}

/// Resolve the route for a write that has not crossed the terminal boundary.
fn exact_prewrite_watcher(
    inner: &Inner,
    handle: &DeliveryHandle,
    manifest_id: &str,
) -> Result<Arc<SessionWatcher>, AttemptFailure> {
    match handle_route(inner, handle) {
        HandleRoute::Exact(watcher) => Ok(watcher),
        HandleRoute::BindingChanged => Err(AttemptFailure::pane_rebound_before_paste()),
        HandleRoute::BindingUnprovable { pane_pid } => {
            Err(AttemptFailure::binding_unprovable(Some(
                binding_unprovable_observation(inner, handle, pane_pid, manifest_id),
            )))
        }
        HandleRoute::Unavailable => Err(AttemptFailure::session_detached()),
    }
}

/// Drive one delivery through gate, inject, submit, ACK, bounded retry.
async fn process(inner: &Arc<Inner>, worker: &Arc<Worker>, handle: &Arc<DeliveryHandle>) {
    if let Some(notification) = &handle.notification {
        match notification.claimed_notification_barrier() {
            Ok(Some(barrier)) => {
                if let AttemptOutcome::Failed(failure) =
                    reconcile_recovered_claimed_notification_barrier(inner, handle, barrier).await
                {
                    inject_pause(inner, "post_claimed_notification_refusal").await;
                    if !fault_notification_worker(worker, &failure) {
                        let _ = fail_attempt(inner, worker, handle, &failure).await;
                    }
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                error!(id = %handle.msg_id, %error, "cannot classify staged claim recovery");
                return;
            }
        }
    }
    // retry_queued alongside queued: a chain requeued across a daemon
    // restart, or parked by a quiesce, re-enters here in that state.
    if !advance(
        inner,
        handle,
        &[DeliveryState::Queued, DeliveryState::RetryQueued],
        Step::to(DeliveryState::Gating),
    ) {
        return;
    }
    if let Some(notification) = &handle.notification {
        match notification.record_gating() {
            Ok(_) => {}
            Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => {
                // A claim or replacement retired this attempt before it
                // touched the pane.
                return;
            }
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification gating fact failed");
                notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                return;
            }
        }
    }
    let mut regate_hold = None;
    loop {
        let gate_outcome = gate(inner, handle, regate_hold.take()).await;
        if let Some(notification) = &handle.notification {
            match notification.ensure_current_gating() {
                Ok(()) => {}
                Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => return,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification gate outcome recheck failed");
                    notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                    return;
                }
            }
        }
        match gate_outcome {
            GateOutcome::Withdrawn => return,
            GateOutcome::BlockedPreWrite { cause, observation } => {
                if handle.notification.is_none() {
                    return;
                }
                persist_notification_prewrite_block(
                    inner,
                    worker,
                    handle,
                    cause,
                    Some(*observation),
                )
                .await;
                return;
            }
            GateOutcome::Deferred { cause } => {
                notify_notification_deferred(inner, handle, &cause);
                return;
            }
            GateOutcome::Park { hint } => {
                park_recipient(inner, worker, handle, hint).await;
                return;
            }
            GateOutcome::Attention { cause } => {
                advance(
                    inner,
                    handle,
                    &[DeliveryState::Gating],
                    Step::to(DeliveryState::AttentionRequired).cause(&cause),
                );
                notify_attention(inner, handle, &cause);
                return;
            }
            GateOutcome::Proceed {
                manifest_id,
                pane_pid,
                regate_evidence_changed,
            } => {
                if regate_evidence_changed {
                    reset_immediate_regates(handle);
                }
                // A quiesce that landed while this delivery was at the
                // gate: nothing may cross the paste boundary now. Park
                // pre-paste and hand the job back; it re-enters when the
                // pipeline resumes or requeues across the restart the
                // quiesce was for.
                if inner.engine.paused.load(Ordering::SeqCst) {
                    if advance(
                        inner,
                        handle,
                        &[DeliveryState::Gating],
                        Step::to(DeliveryState::RetryQueued).cause("quiesce"),
                    ) && !worker.requeue_current_front(handle)
                    {
                        error!(id = %handle.msg_id, "quiesce lost exact worker ownership");
                    }
                    return;
                }
                {
                    let mut st = handle.state.lock().expect("handle state lock");
                    st.attempts += 1;
                }
                if !advance(
                    inner,
                    handle,
                    &[DeliveryState::Gating],
                    Step::to(DeliveryState::Pasting),
                ) {
                    return;
                }
                match attempt_delivery(inner, handle, &manifest_id, pane_pid).await {
                    AttemptOutcome::Done => return,
                    // The mailbox fact records why this attempt is no longer
                    // current. This worker only proves it never wrote.
                    AttemptOutcome::NoLongerCurrentBeforeWrite => return,
                    AttemptOutcome::Failed(failure) => {
                        if fault_notification_worker(worker, &failure) {
                            return;
                        }
                        // Readiness moved between the gate's proof and
                        // the write. Nothing was written and no transport
                        // was spent, so this is not a retry: it goes back
                        // to the gate, which waits on the barrier's own
                        // release rather than on a budget.
                        if let Some(regate_cause) = failure.regate_cause() {
                            let action = regate_action(handle, regate_cause);
                            // The legal path back to the gate runs through
                            // RetryQueued. A mailbox race that cannot be
                            // re-proven settles as a durable pre-write block;
                            // legacy direct delivery waits on pane evidence.
                            if !advance(
                                inner,
                                handle,
                                &[DeliveryState::Pasting],
                                Step::to(DeliveryState::RetryQueued).cause(&failure.cause),
                            ) {
                                return;
                            }
                            if !advance(
                                inner,
                                handle,
                                &[DeliveryState::RetryQueued],
                                Step::to(DeliveryState::Gating).cause("regate"),
                            ) {
                                return;
                            }
                            match action {
                                RegateAction::ImmediateReproof => {}
                                RegateAction::Hold => {
                                    regate_hold = Some(failure.cause.clone());
                                }
                                RegateAction::BlockPreWrite => {
                                    if handle.notification.is_some() {
                                        persist_notification_prewrite_block(
                                            inner,
                                            worker,
                                            handle,
                                            NotificationPreWriteCause::WriteReadinessChanged,
                                            None,
                                        )
                                        .await;
                                        return;
                                    }
                                    // Legacy direct delivery has no durable
                                    // mailbox state or withdrawal verb. Keep
                                    // it held until exact pane evidence moves.
                                    regate_hold = Some(failure.cause.clone());
                                }
                            }
                            continue;
                        }
                        if !fail_attempt(inner, worker, handle, &failure).await {
                            return;
                        }
                        // Bounded retry: back through the gate.
                        if !advance(
                            inner,
                            handle,
                            &[DeliveryState::RetryQueued],
                            Step::to(DeliveryState::Gating).cause("retry"),
                        ) {
                            return;
                        }
                    }
                }
            }
        }
    }
}

enum AttemptOutcome {
    /// Delivery resolved (verified, unverified, or matcher-resolved).
    Done,
    /// The mailbox atomically withdrew this attempt before the pane write.
    NoLongerCurrentBeforeWrite,
    /// This attempt failed; the boundary feeds retry accounting.
    Failed(AttemptFailure),
}

/// The irreversible boundary for one failed attempt. Once a write may have
/// happened, repeating it is unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteBoundary {
    BeforeWrite,
    AfterWrite,
}

/// A delivery failure and its closed, semantic boundary. Call sites select a
/// named failure constructor, so an after-write cause cannot accidentally be
/// marked retryable by passing a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptFailure {
    cause: String,
    boundary: WriteBoundary,
    pre_write_block: Option<Box<PreWriteBlock>>,
    verify_outcome: Option<NotificationVerifyOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreWriteBlock {
    cause: NotificationPreWriteCause,
    observation: Option<NotificationPreWriteObservation>,
}

impl AttemptFailure {
    fn blocked_before_write(cause: impl Into<String>, block: NotificationPreWriteCause) -> Self {
        Self {
            cause: cause.into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: block,
                observation: None,
            })),
            verify_outcome: None,
        }
    }

    fn session_detached() -> Self {
        Self::blocked_before_write(
            "session_detached",
            NotificationPreWriteCause::SessionUnavailable,
        )
    }

    fn no_manifest() -> Self {
        Self::blocked_before_write(
            "no_manifest",
            NotificationPreWriteCause::ManifestUnavailable,
        )
    }

    fn payload_unavailable() -> Self {
        Self::blocked_before_write(
            "payload_unavailable",
            NotificationPreWriteCause::PayloadUnavailable,
        )
    }

    fn pane_rebound_before_paste() -> Self {
        Self::blocked_before_write(
            "pane_rebound",
            NotificationPreWriteCause::WriteReadinessChanged,
        )
    }

    /// The pane's manifest requires hook liveness and no admitting edge has
    /// been published for its current binding. Carries the observation so
    /// the durable block names the exact binding and the block itself.
    fn hook_admission_unproven(observation: Option<NotificationPreWriteObservation>) -> Self {
        Self {
            cause: HOOK_ADMISSION_UNPROVEN.into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: NotificationPreWriteCause::WriteReadinessChanged,
                observation,
            })),
            verify_outcome: None,
        }
    }

    fn binding_unprovable(observation: Option<NotificationPreWriteObservation>) -> Self {
        Self {
            cause: "binding_unprovable".into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: NotificationPreWriteCause::BindingUnprovable,
                observation,
            })),
            verify_outcome: None,
        }
    }

    fn pane_too_narrow(mut observation: NotificationPreWriteObservation) -> Self {
        observation.required_pane_width = Some(cyclops_proto::DOORBELL_V3_MIN_PANE_WIDTH);
        Self {
            cause: "pane_too_narrow".into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: Some(Box::new(PreWriteBlock {
                cause: NotificationPreWriteCause::WriteReadinessChanged,
                observation: Some(observation),
            })),
            verify_outcome: None,
        }
    }

    fn composer_ownership_unproven() -> Self {
        Self::blocked_before_write(
            "composer_ownership_unproven",
            NotificationPreWriteCause::ComposerOwnershipUnproven,
        )
    }

    /// Does this failure belong back at the gate rather than in the
    /// retry budget? True only where the cause is readiness moving under
    /// a delivery that had not yet written anything.
    fn regate_cause(&self) -> Option<RegateCause> {
        match self.cause.as_str() {
            "barrier_held" => Some(RegateCause::BarrierHeld),
            "binding_changed" => Some(RegateCause::BindingChanged),
            "capability_changed" => Some(RegateCause::CapabilityChanged),
            _ => None,
        }
    }

    /// The composer barrier was not this attempt's to take: somebody
    /// else's payload or a person's typing is in there. Nothing was
    /// written, so this returns to the gate.
    fn barrier_held() -> Self {
        Self {
            cause: "barrier_held".into(),
            boundary: WriteBoundary::BeforeWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    fn spool_failed() -> Self {
        Self::blocked_before_write("spool_failed", NotificationPreWriteCause::SpoolFailed)
    }

    fn paste_command_unwritten() -> Self {
        Self::blocked_before_write(
            "paste_command_unwritten",
            NotificationPreWriteCause::PasteCommandUnwritten,
        )
    }

    fn paste_failed() -> Self {
        Self {
            cause: "paste_failed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    fn verify_failed() -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome::ambiguous())
    }

    fn verify_failed_with(verify_outcome: NotificationVerifyOutcome) -> Self {
        Self {
            cause: "verify_failed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: Some(verify_outcome),
        }
    }

    fn verify_timeout() -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::Timeout,
            observed_composer: ComposerState::ComposerAmbiguous,
        })
    }

    fn verify_mismatch(observed_composer: ComposerState) -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::Mismatch,
            observed_composer,
        })
    }

    fn verify_owner_missing(observed_composer: ComposerState) -> Self {
        Self::verify_failed_with(NotificationVerifyOutcome {
            kind: NotificationVerifyFailureKind::OwnerMissing,
            observed_composer,
        })
    }

    fn pane_rebound_after_paste() -> Self {
        Self {
            cause: "pane_rebound_after_paste".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    fn submit_failed() -> Self {
        Self {
            cause: "submit_failed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    /// The pane changed hands after Enter. Terminal, and after the write
    /// boundary: the original occupant may well have received the message,
    /// so this says the outcome is unknown rather than claiming a failure.
    fn receipt_occupant_changed() -> Self {
        Self {
            cause: "receipt_occupant_changed".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    fn ack_timeout() -> Self {
        Self {
            cause: "ack_timeout".into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    /// The durable boundary could not be advanced after the attempt crossed it.
    /// Retrying could duplicate a notification whose append outcome is unknown.
    fn notification_record_failed() -> Self {
        Self {
            cause: NOTIFICATION_RECORD_FAILED.into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    fn claimed_staged_settlement_failed() -> Self {
        Self {
            cause: CLAIMED_STAGED_SETTLEMENT_FAILED.into(),
            boundary: WriteBoundary::AfterWrite,
            pre_write_block: None,
            verify_outcome: None,
        }
    }

    fn faults_notification_worker(&self) -> bool {
        self.cause == CLAIMED_STAGED_SETTLEMENT_FAILED
    }

    /// Map the injector's closed set of pre-submit causes to the semantic
    /// constructors above. Unknown injector errors remain conservatively
    /// after-write; they must never gain retryability by default.
    fn from_inject(cause: String) -> Self {
        match cause.as_str() {
            "spool_failed" => Self::spool_failed(),
            // The barrier refused before anything was written, so this is
            // readiness changing between the proof and the write, not a
            // transport failure. It goes back to the gate rather than to
            // a human.
            "barrier_held" => Self::barrier_held(),
            "prewrite_session_detached" => Self::session_detached(),
            "prewrite_binding_unprovable" => Self::binding_unprovable(None),
            "composer_ownership_unproven" => Self::composer_ownership_unproven(),
            // The pane's binding moved between the proof and the write.
            // Nothing was written, and re-proving it is the gate's job.
            "binding_changed" | "capability_changed" => Self {
                cause,
                boundary: WriteBoundary::BeforeWrite,
                pre_write_block: None,
                verify_outcome: None,
            },
            "paste_failed" => Self::paste_failed(),
            "verify_failed" => Self::verify_failed(),
            NOTIFICATION_RECORD_FAILED => Self::notification_record_failed(),
            _ => Self {
                cause,
                boundary: WriteBoundary::AfterWrite,
                pre_write_block: None,
                verify_outcome: None,
            },
        }
    }
}

fn fault_notification_worker(worker: &Worker, failure: &AttemptFailure) -> bool {
    if !failure.faults_notification_worker() {
        return false;
    }
    worker.set_fault(CLAIMED_STAGED_SETTLEMENT_FAILED);
    true
}

/// One injection attempt: paste, verify, submit, wait for an ACK tier.
///
/// The gate's admitting snapshot is re-checked against the live pane table.
/// The irreversible write and submit bookends require the same pane-root,
/// terminal leader, and admitted agent generations plus the same manifest. A
/// replacement occupant must never receive the payload or Enter.
async fn attempt_delivery(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
    admitted_pid: i32,
) -> AttemptOutcome {
    // This capability belongs only to the capture immediately before this
    // attempt's paste. A later retry must earn it again from fresh evidence.
    handle.set_working_clean_submit_admitted(false);
    let watcher = match exact_prewrite_watcher(inner, handle, manifest_id) {
        Ok(watcher) => watcher,
        Err(failure) => return AttemptOutcome::Failed(failure),
    };
    let Some(manifest) = inner.manifests.get(manifest_id) else {
        return AttemptOutcome::Failed(AttemptFailure::no_manifest());
    };
    let injector = TmuxInjector {
        client: watcher.client(),
        buffer: format!(
            "cyc-{}-{}",
            std::process::id(),
            inner.engine.buffer_seq.fetch_add(1, Ordering::Relaxed)
        ),
    };
    let observed_row = watcher.pane(&handle.pane_id);
    let pane_width = observed_row.as_ref().map(|row| row.width);
    let observed =
        observed_row.and_then(|row| fusion::admitted_binding(inner, handle.session_idx, &row));
    let selected = match select_attempt_payload(handle, manifest, observed.as_ref(), pane_width) {
        Ok(selected) => selected,
        Err(error) => {
            error!(id = %handle.msg_id, error = %error, "notification payload reconstruction failed");
            return AttemptOutcome::Failed(AttemptFailure::payload_unavailable());
        }
    };
    handle.set_attempt_payload(selected.bytes.clone(), selected.transport);
    // Spooled FIRST, and deliberately before the pause and the proof
    // below. Loading the buffer costs a control round trip, and a round
    // trip is time a person can type into the composer that no capture
    // afterwards would see, because the proof would already be behind it.
    // Spooling touches no pane, so moving it earlier costs nothing and
    // leaves the admitting capture as the last thing before the write.
    // What remains between the proof and the paste is the command
    // envelope itself, which is irreducible.
    if let Err(cause) = injector.spool(&selected.bytes).await {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
    }
    inject_pause(inner, "pre_paste").await;
    let watcher = match exact_prewrite_watcher(inner, handle, manifest_id) {
        Ok(watcher) => watcher,
        Err(failure) => {
            injector.discard().await;
            if failure.cause == "pane_rebound" {
                gate_line(
                    inner,
                    handle,
                    "rebound",
                    None,
                    Some("route_binding_changed"),
                );
            }
            return AttemptOutcome::Failed(failure);
        }
    };
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        injector.discard().await;
        gate_line(inner, handle, "rebound", None, Some(&detail));
        let failure = if detail == "pane_gone" {
            AttemptFailure::session_detached()
        } else {
            AttemptFailure::pane_rebound_before_paste()
        };
        return AttemptOutcome::Failed(failure);
    }
    // The gate's clean-composer evidence was current when it admitted, and
    // admission is a decision about a moment. A person can start typing in
    // the gap that follows, and the occupant re-check above would not
    // notice: same pane, same pid, same manifest, new draft. So the
    // readiness rule is asked again here, against a capture taken now,
    // immediately before the write that cannot be taken back.
    match crate::observe_pane(
        inner,
        handle.session_idx,
        &watcher,
        &handle.pane_id,
        true,
        "pre_paste",
    )
    .await
    {
        Some(det) => {
            // A positive human draft remains a hard boundary. For an
            // authenticated idle or working agent, however, an unreadable
            // composer is not a reason to strand a durable doorbell after
            // the gate already admitted the same live occupant.
            let unproven_composer_is_still_eligible = handle.notification.is_some()
                && watcher.pane(&handle.pane_id).is_some_and(|row| {
                    notification_pane_for_unproven_composer(inner, handle, &row, manifest_id, &det)
                        .is_some()
                });
            if !det.write_ready && !unproven_composer_is_still_eligible {
                let reason = det.write_block.as_deref().unwrap_or("unstamped");
                gate_line(
                    inner,
                    handle,
                    "hold",
                    None,
                    Some(&format!("not_write_ready:{reason}")),
                );
                injector.discard().await;
                if reason == HOOK_ADMISSION_UNPROVEN {
                    // Not a readiness flicker: nothing on the pane will
                    // clear it. Park the wake as a named durable block
                    // that carries the exact binding it was refused for.
                    let observation = watcher.pane(&handle.pane_id).and_then(|row| {
                        let mut observation =
                            composer_semantic_observation(inner, handle, &row, manifest_id)?;
                        observation.write_block = Some(HOOK_ADMISSION_UNPROVEN.to_string());
                        Some(observation)
                    });
                    return AttemptOutcome::Failed(AttemptFailure::hook_admission_unproven(
                        observation,
                    ));
                }
                return AttemptOutcome::Failed(AttemptFailure::pane_rebound_before_paste());
            }
            // A Working runtime is safe only in this narrow, positive shape.
            // Keep that admission with the in-flight notification: after the
            // paste, the exact doorbell itself naturally renders as input and
            // cannot repeat the clean-composer proof that made the write safe.
            handle.set_working_clean_submit_admitted(
                handle.notification.is_some()
                    && det.state == AgentState::Working
                    && (det.write_ready
                        || det.screen_proves_write_safe_composer()
                        || unproven_composer_is_still_eligible),
            );
        }
        None => {
            injector.discard().await;
            return AttemptOutcome::Failed(AttemptFailure::session_detached());
        }
    }
    // That recompute took a capture, so who owns the pane is checked again
    // after it: otherwise the newest fact about the composer would rest on
    // an older fact about whose composer it is.
    let watcher = match exact_prewrite_watcher(inner, handle, manifest_id) {
        Ok(watcher) => watcher,
        Err(failure) => {
            injector.discard().await;
            return AttemptOutcome::Failed(failure);
        }
    };
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        gate_line(inner, handle, "rebound", None, Some(&detail));
        injector.discard().await;
        let failure = if detail == "pane_gone" {
            AttemptFailure::session_detached()
        } else {
            AttemptFailure::pane_rebound_before_paste()
        };
        return AttemptOutcome::Failed(failure);
    }
    // The binding this write depends on, proven ONCE here, immediately
    // after the last capture that admitted it. Three lookups taken
    // separately can disagree with each other; this is one observation of
    // the leader, the agent and the rules that agent is running under.
    let Some(final_row) = watcher.pane(&handle.pane_id) else {
        injector.discard().await;
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    };
    let observed_binding = if inner
        .fail_next_final_binding_observation
        .swap(false, Ordering::SeqCst)
    {
        None
    } else {
        fusion::admitted_binding(inner, handle.session_idx, &final_row)
    };
    // Retain the last complete binding that this attempt genuinely observed.
    // If the terminal lookup itself is unavailable, this prior proof is the
    // durable baseline that prevents the unchanged occupant from looking like
    // a new route edge and reopening the same blocked attempt.
    let evidence_binding = observed_binding.as_ref().or(observed.as_ref());
    let observation =
        handle
            .notification
            .as_ref()
            .map(|notification| NotificationPreWriteObservation {
                pane_root: evidence_binding
                    .and_then(|binding| process_instance_id(binding.pane_root)),
                selected_manifest: Some(
                    NotificationManifestId::new(manifest_id)
                        .expect("loaded manifest ids are validated before delivery"),
                ),
                binding: evidence_binding
                    .and_then(|binding| notification_binding(notification.recipient(), binding)),
                route_evidence: Some(inner.route_evidence_id(handle.session_idx, &handle.pane_id)),
                pane_width: Some(final_row.width),
                required_pane_width: selected.required_pane_width(),
                write_block: None,
            });
    let proven = match observed_binding {
        // The gate admitted under a manifest, and the live read has to
        // still agree with it: a process that exec'd in place keeps its
        // identity while becoming another program.
        Some(binding) if binding.manifest == manifest_id => binding,
        _ => {
            // Widths are a paired observation used only by the pane-too-narrow
            // bookend below. Carrying either half through a binding failure
            // makes the durable observation invalid and strands the attempt.
            let observation = observation.map(|mut observation| {
                observation.pane_width = None;
                observation.required_pane_width = None;
                observation
            });
            gate_line(inner, handle, "rebound", None, Some("binding_unprovable"));
            injector.discard().await;
            return AttemptOutcome::Failed(AttemptFailure::binding_unprovable(observation));
        }
    };
    if let Some(cause) = notification_prewrite_bookend(
        &selected,
        handle
            .notification
            .as_ref()
            .map(NotificationContext::recipient),
        &proven,
        final_row.width,
    ) {
        injector.discard().await;
        if cause.starts_with("pane_too_narrow:") {
            return AttemptOutcome::Failed(AttemptFailure::pane_too_narrow(
                observation.expect("format 3 belongs to a notification"),
            ));
        }
        return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
    }
    inject_pause(inner, "post_final_prewrite").await;
    // The composer hold is installed AT the write boundary, by the injector,
    // not before the attempt and not after it resolves. Installing it before
    // the attempt would catch `spool_failed` and block its bounded transport
    // retry with no staged payload and no turn that could clear the hold.
    // Exhausted spool failures use the separate durable pre-write block.
    // Installing the composer hold after the attempt resolves would leave a
    // window where `verify_failed` (the paste may have
    // landed, nobody could prove what it did) is visible to another
    // delivery for the same pane before anything holds it.
    let target = match selected.transport {
        Some(NotificationTransport::Doorbell) => StagingTarget::ExactRow(&selected.bytes),
        Some(NotificationTransport::DirectPayload) | None => {
            StagingTarget::Sentinel(&handle.msg_id)
        }
    };
    let (staged_window, id_staged, payload_at_proof) = match inject(
        &injector,
        handle,
        manifest,
        target,
        &selected.bytes,
        &|| {
            if let Some(notification) = &handle.notification {
                notification
                    .ensure_current_gating()
                    .map_err(notification_write_cause)?;
            }
            // The last thing before the pane is asked to take the
            // payload: the same binding, read again, and equal. Nothing
            // has been written yet, so a change here is the world moving
            // rather than a transport failure.
            let (now, pane_width) = match handle_route(inner, handle) {
                HandleRoute::Exact(watcher) => {
                    let row = watcher
                        .pane(&handle.pane_id)
                        .ok_or_else(|| "prewrite_session_detached".to_string())?;
                    let binding = fusion::admitted_binding(inner, handle.session_idx, &row)
                        .ok_or_else(|| "prewrite_binding_unprovable".to_string())?;
                    (binding, row.width)
                }
                HandleRoute::BindingChanged => return Err("binding_changed".to_string()),
                HandleRoute::BindingUnprovable { .. } => {
                    return Err("prewrite_binding_unprovable".to_string())
                }
                HandleRoute::Unavailable => return Err("prewrite_session_detached".to_string()),
            };
            if now != proven {
                return Err("binding_changed".to_string());
            }
            if let Some(cause) = notification_prewrite_bookend(
                &selected,
                handle
                    .notification
                    .as_ref()
                    .map(NotificationContext::recipient),
                &now,
                pane_width,
            ) {
                return Err(cause);
            }
            let notification_binding = if handle.notification.is_some() {
                Some((
                    ProcessInstanceId::new(proven.pane_root.pid, proven.pane_root.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                    ProcessInstanceId::new(proven.leader.pid, proven.leader.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                    ProcessInstanceId::new(proven.agent.pid, proven.agent.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                ))
            } else {
                None
            };
            latch_hold(inner, handle, &proven)?;
            let mut unwritten_hold = UnwrittenHold::new(inner, handle, &proven);
            let should_panic_attempt = {
                let current_attempt = handle.notification.as_ref().map(|n| n.attempt_id());
                let mut guard = inner.fail_pre_record_writing.lock().unwrap();
                if let Some(target) = *guard {
                    if current_attempt == Some(target) {
                        *guard = None;
                        Some(target)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(target_attempt) = should_panic_attempt {
                panic!(
                    "worker exit at synchronous on_write boundary before first durable transition for attempt {target_attempt}"
                );
            }
            if let (Some(notification), Some((pane_root, leader, agent))) =
                (&handle.notification, notification_binding)
            {
                let transport = selected
                    .transport
                    .expect("notification attempts select a transport");
                if let Err(error) = notification.record_writing(
                    pane_root,
                    leader,
                    agent,
                    &proven.manifest,
                    transport,
                    selected.doorbell_format,
                ) {
                    return Err(notification_write_cause(error));
                }
            }
            handle.write_boundary_crossed.store(true, Ordering::SeqCst);
            unwritten_hold.commit();
            Ok(())
        },
    )
    .await
    {
        Ok(v) => v,
        Err(failure) => {
            return finish_attempt_delivery_inject_failure(
                inner,
                handle,
                &proven,
                observation,
                failure,
            );
        }
    };
    let mut staging_verified = !payload_at_proof.is_empty();
    if let Some(notification) = &handle.notification {
        if let Err(error) = notification.record_staged() {
            error!(id = %handle.msg_id, error = %error, "notification staged fact failed");
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
        }
    }
    if !advance(
        inner,
        handle,
        &[DeliveryState::Pasting],
        Step::to(DeliveryState::Staged),
    ) {
        return AttemptOutcome::Done;
    }

    let submit_key = if manifest.injection.submit.is_empty() {
        "Enter"
    } else {
        manifest.injection.submit.as_str()
    };
    inject_pause(inner, "pre_submit").await;
    if let Err(detail) = proven_binding_unchanged(inner, handle, &proven) {
        // The staged payload belongs to the occupant that verified it; the
        // submit key must never reach whoever replaced it.
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    // Verification proved a representation at a moment, and Enter is sent
    // at a later one. A person can append to the staged text, or replace
    // it, in between; pressing Enter then submits something nobody
    // verified and nobody wrote. Repaint is also not atomic: after a valid
    // paste proof, a capture can land between the terminal clear and the
    // renderer's next complete frame. Reuse the bounded post-paste evidence
    // schedule so that transient incomplete frames do not turn a clean,
    // owned doorbell into a false verify failure.
    let recheck = if staging_verified {
        match recheck_exact_staging_snapshot(
            &injector,
            &handle.pane_id,
            manifest,
            target,
            &selected.bytes,
            id_staged,
            &payload_at_proof,
        )
        .await
        {
            Ok(now) => now,
            Err(ExactStagingRecheck::Mismatch) => {
                unregister_ack(inner, handle);
                gate_line(inner, handle, "rebound", None, Some("staging_changed"));
                return AttemptOutcome::Failed(AttemptFailure::verify_mismatch(
                    ComposerState::ComposerAmbiguous,
                ));
            }
            Err(ExactStagingRecheck::Unobservable) if handle.notification.is_some() => {
                staging_verified = false;
                injector
                    .capture_joined_escaped(&handle.pane_id)
                    .await
                    .unwrap_or_default()
            }
            Err(ExactStagingRecheck::Unobservable) => {
                // Nobody looked, so nobody may press Enter.
                unregister_ack(inner, handle);
                gate_line(inner, handle, "rebound", None, Some("recheck_unobservable"));
                return AttemptOutcome::Failed(AttemptFailure::verify_timeout());
            }
        }
    } else {
        injector
            .capture_joined_escaped(&handle.pane_id)
            .await
            .unwrap_or_default()
    };
    // The capture above took time, so the occupant is checked once more
    // after it. Otherwise the last thing proven about who owns the pane is
    // older than the last thing proven about what is in it.
    if let Err(detail) = proven_binding_unchanged(inner, handle, &proven) {
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    if staging_verified {
        if let Err(detail) =
            notification_staged_action_safe(inner, handle, manifest, &recheck, &proven, true)
        {
            unregister_ack(inner, handle);
            gate_line(inner, handle, "rebound", None, Some(&detail));
            return AttemptOutcome::Failed(AttemptFailure::verify_failed());
        }
    }
    let notification_submit_reserved = if let Some(notification) = &handle.notification {
        match notification.reserve_submit() {
            Ok(SubmitReservation::Reserved) => true,
            Ok(SubmitReservation::ClaimedBeforeSubmit) => {
                return reconcile_claimed_notification_barrier(
                    inner,
                    handle,
                    manifest,
                    StagingExpectation {
                        target,
                        payload: &selected.bytes,
                    },
                    &proven,
                    &injector,
                    ClaimedStagedReconciliation::CurrentStaged,
                )
                .await;
            }
            Err(error) => {
                error!(id = %handle.msg_id, %error, "notification submit reservation failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
    } else {
        false
    };
    // Reserving submit appends a journal fact and therefore opens another
    // content and process replacement window. Re-capture the composer and
    // re-prove the complete binding after that append. `Submitting` reserves
    // one key attempt; it never authorizes changed or unobservable bytes.
    if notification_submit_reserved && staging_verified {
        inject_pause(inner, "post_submit_reservation").await;
        match recheck_exact_staging_snapshot(
            &injector,
            &handle.pane_id,
            manifest,
            target,
            &selected.bytes,
            id_staged,
            &payload_at_proof,
        )
        .await
        {
            Ok(now) => {
                if let Err(detail) =
                    notification_staged_action_safe(inner, handle, manifest, &now, &proven, true)
                {
                    gate_line(
                        inner,
                        handle,
                        "rebound",
                        None,
                        Some(&format!("{detail}_after_submit_reservation")),
                    );
                    return AttemptOutcome::Failed(AttemptFailure::verify_failed());
                }
            }
            Err(ExactStagingRecheck::Mismatch) => {
                gate_line(
                    inner,
                    handle,
                    "rebound",
                    None,
                    Some("staging_changed_after_submit_reservation"),
                );
                return AttemptOutcome::Failed(AttemptFailure::verify_mismatch(
                    ComposerState::ComposerAmbiguous,
                ));
            }
            Err(ExactStagingRecheck::Unobservable) if handle.notification.is_some() => {
                staging_verified = false;
            }
            Err(ExactStagingRecheck::Unobservable) => {
                gate_line(
                    inner,
                    handle,
                    "rebound",
                    None,
                    Some("reserved_staging_unobservable"),
                );
                return AttemptOutcome::Failed(AttemptFailure::verify_timeout());
            }
        }
    }
    if !notification_submit_reserved {
        if let Err(detail) = proven_binding_unchanged(inner, handle, &proven) {
            unregister_ack(inner, handle);
            gate_line(inner, handle, "rebound", None, Some(&detail));
            return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
        }
    }
    // The occupant re-check just passed: admitted_pid IS the process the
    // submit key goes to. Send-and-wait pins its wait on this pid.
    // Subscribe before Enter. A fast vendor can paint its entire working
    // phase before send-keys returns, so subscribing inside the receipt
    // waiter loses the only turn evidence that actually followed this key.
    let receipt_events = inner.events.subscribe();
    // This receiver has one job: retain a matching working edge until a
    // screen checkpoint has accounted for it. The main receipt receiver
    // still owns session lifecycle and lag handling below. Keeping those
    // responsibilities separate means a screen receipt cannot settle first
    // and strand the `turn_ended` wait behind an already-observed turn.
    let receipt_turn_events = inner.events.subscribe();
    // Keep an independent receiver alive through receipt settlement and into
    // `wait_pinned`. It can establish only an exact post-submit Working fact;
    // the composed wait opens its own fresh stream for current and future
    // state, so an older Idle cannot replay as a current answer.
    handle.replace_post_submit_turn_events(inner.events.subscribe());
    let receipt_submit_at = Instant::now();
    let receipt_submit_at_ms = unix_ms();
    handle.submitted_pid.store(admitted_pid, Ordering::SeqCst);
    // And the AGENT behind it, which is what a hook report is filed
    // under. The foreground leader can be a tool the agent handed the
    // terminal to, so the two are recorded separately and never
    // substituted for one another: the leader is terminal admission
    // evidence, the agent identity is who this delivery belongs to.
    *handle.submitted_agent.lock().expect("submitted agent lock") = Some(proven.agent);
    handle
        .submitted_at_ms
        .store(receipt_submit_at_ms, Ordering::SeqCst);
    *handle
        .submitted_manifest
        .lock()
        .expect("submitted manifest lock") = Some(proven.manifest.clone());
    // Registered here, after every proof and immediately before the key:
    // the measured hook edge lands 21-28ms after Enter, so this is early
    // enough, and it closes the window where a stale ACK from the same
    // occupant could set the early flag before any submit was attempted.
    register_ack(inner, handle);
    if let Err(cause) = injector.submit(&handle.pane_id, submit_key).await {
        unregister_ack(inner, handle);
        debug_assert_eq!(cause, "submit_failed");
        return AttemptOutcome::Failed(AttemptFailure::submit_failed());
    }
    if let Some(notification) = &handle.notification {
        let record_res = if staging_verified {
            notification.record_submitted()
        } else {
            notification.record_submitted_unverified()
        };
        match record_res {
            Ok(_) => {}
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification submitted fact failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
    }
    // The key is sent and the binding is recorded, so an acknowledgement
    // can arrive from here on, while the delivery is still `Staged`.
    // Always None in production.
    inject_pause(inner, "post_key").await;
    if !advance(
        inner,
        handle,
        &[DeliveryState::Staged],
        Step::to(DeliveryState::Submitted),
    ) {
        return AttemptOutcome::Done;
    }
    if !staging_verified {
        // One-time doorbell submitted unverified.
        // Enter was sent once, state is SubmittedUnverified, never duplicate Enter. Done.
        return AttemptOutcome::Done;
    }
    // Take any accepted early receipt before claim settlement can return.
    // A hook can carry the exact TurnKey while a concurrent socket claim has
    // already made the durable notification Notified. The claim must not
    // discard that stronger receipt.
    let early = take_accepted_early_ack(handle);
    let notified_during_submit_gap = if let Some(notification) = &handle.notification {
        match notification.settle_submitted_claim() {
            Ok(notified) => notified,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification claim recheck failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
    } else {
        false
    };
    if notified_during_submit_gap {
        if let Some(early) = early {
            advance_with_early_ack(inner, handle, early);
        } else {
            settle_notification_claim(
                inner,
                handle
                    .notification
                    .as_ref()
                    .expect("claim settlement belongs to a notification")
                    .attempt_id(),
            );
        }
        return AttemptOutcome::Done;
    }
    // The window this pause exists for: the delivery is Submitted after the
    // worker took any earlier record. A hook arriving now resolves the exact
    // submitted handle directly instead of installing another early record.
    // Always None in production.
    inject_pause(inner, "post_submit").await;
    // An acknowledgement that arrived between paste verification and the
    // Submitted line was taken under the same state lock the installer used.
    if let Some(early) = early {
        match record_notification_notified(inner, handle) {
            Ok(true) => {}
            Ok(false) => return AttemptOutcome::Done,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
        if advance_with_early_ack(inner, handle, early) {
            return AttemptOutcome::Done;
        }
    }
    let ack_outcome = await_ack(
        inner,
        handle,
        ReceiptWait {
            manifest,
            staged_window: &staged_window,
            id_staged,
            target,
            submit_at: receipt_submit_at,
            events: receipt_events,
            turn_events: receipt_turn_events,
        },
    )
    .await;
    // Test-only boundary after receipt observation has finished but before
    // this worker publishes its delivery verdict. It proves the composed
    // wait owns a receiver with no observation gap after an early receipt.
    inject_pause(inner, "post_receipt").await;
    match ack_outcome {
        AckOutcome::Resolved => AttemptOutcome::Done,
        AckOutcome::Screen => {
            // Stays registered: a late matching hook ACK upgrades it to
            // delivered_verified (the legal upgrade transition).
            match record_notification_notified(inner, handle) {
                Ok(true) => {}
                Ok(false) => return AttemptOutcome::Done,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                    return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
                }
            }
            let _ = advance(
                inner,
                handle,
                &[DeliveryState::Submitted],
                Step::to(DeliveryState::DeliveredUnverified)
                    .cause("screen_evidence")
                    .verified(VerifiedBy::Screen),
            );
            AttemptOutcome::Done
        }
        AckOutcome::Timeout => {
            unregister_ack(inner, handle);
            AttemptOutcome::Failed(AttemptFailure::ack_timeout())
        }
        AckOutcome::Rebound => {
            unregister_ack(inner, handle);
            AttemptOutcome::Failed(AttemptFailure::receipt_occupant_changed())
        }
    }
}

/// Resolve the exact injector failure arm of [`attempt_delivery`].
///
/// Keeping the durable correction, runtime boundary, and composer hold in one
/// arm makes their order directly testable without a live tmux process.
fn finish_attempt_delivery_inject_failure(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
    observation: Option<NotificationPreWriteObservation>,
    failure: InjectFailure,
) -> AttemptOutcome {
    match failure {
        InjectFailure::PasteCommandUnwritten => {
            if let Err(error) = correct_proven_unwritten_paste(handle) {
                error!(id = %handle.msg_id, error = %error, "notification unwritten correction failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
            rollback_unwritten_hold(inner, handle, proven);
            AttemptOutcome::Failed(AttemptFailure::paste_command_unwritten())
        }
        InjectFailure::Other(cause) => {
            if cause == NO_LONGER_CURRENT_BEFORE_WRITE {
                return AttemptOutcome::NoLongerCurrentBeforeWrite;
            }
            if let Some(width) = cause
                .strip_prefix("pane_too_narrow:")
                .and_then(|width| width.parse::<u32>().ok())
            {
                let mut observation = observation.expect("format 3 belongs to a notification");
                observation.pane_width = Some(width);
                return AttemptOutcome::Failed(AttemptFailure::pane_too_narrow(observation));
            }
            AttemptOutcome::Failed(AttemptFailure::from_inject(cause))
        }
    }
}

/// Re-prove that an automatic notification submit still owns this exact
/// staged composer. The caller separately compares the normalized bytes.
/// This check binds that content to the current process generations and
/// manifest, requires a terminal-safe visual state, and refuses any known
/// blocked-state or final-submit conflict. An ordinary in-flight notification
/// can use the exact proof when a vendor's short screen projection loses the
/// prompt row to chrome; recovery and terminal clear paths stay on the
/// quiet-frame rule.
fn notification_staged_action_safe(
    inner: &Arc<Inner>,
    handle: &DeliveryHandle,
    manifest: &Manifest,
    capture: &str,
    proven: &fusion::Binding,
    allow_inflight_working_admission: bool,
) -> Result<(), String> {
    let Some(notification) = &handle.notification else {
        return Ok(());
    };
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return Err("session_detached".to_string());
    };
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err("pane_gone".to_string());
    };
    if row.dead {
        return Err("pane_dead".to_string());
    }
    if row.in_mode {
        return Err("pane_in_mode".to_string());
    }
    let current = fusion::admitted_binding(inner, handle.session_idx, &row);
    if !binding_is_exact(current.as_ref(), proven) {
        return Err("binding_changed".to_string());
    }
    let state = manifest
        .evaluate_esc(&row.title, &strip_csi(capture), Some(capture))
        .map(|rule| rule.state);
    if matches!(
        state,
        Some(
            AgentState::BlockedModal
                | AgentState::BlockedPermission
                | AgentState::BlockedQuota
                | AgentState::Dead
        )
    ) {
        return Err("staged_manifest_state_unsafe".to_string());
    }
    let Some(agent) = process_instance_id(proven.agent) else {
        return Err("binding_unprovable".to_string());
    };
    // Exact bytes and an unchanged binding are stronger than a fixed tail
    // window that happened to omit a long wrapped prompt. This is deliberately
    // limited to a non-Working normal post-paste submit: a freshly observed
    // Working edge still needs the separately recorded clean-composer
    // admission. Claim recovery and terminal clear retain the stricter
    // quiet-frame rule below.
    if allow_inflight_working_admission
        && fusion::staged_exact_submit_ready(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &notification.attempt_id().to_string(),
            agent,
            &proven.manifest,
        )
    {
        return Ok(());
    }
    let working_clean_submit = allow_inflight_working_admission
        && state == Some(AgentState::Working)
        && handle.working_clean_submit_admitted();
    if !matches!(state, Some(AgentState::Idle | AgentState::IdleWithInput)) && !working_clean_submit
    {
        return Err("staged_manifest_state_unsafe".to_string());
    }
    let quiet_staged_action = fusion::staged_action_ready(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &notification.attempt_id().to_string(),
        agent,
        &proven.manifest,
    );
    let working_staged_action = working_clean_submit
        && fusion::staged_working_clean_action_ready(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &notification.attempt_id().to_string(),
            agent,
            &proven.manifest,
        );
    if !quiet_staged_action && !working_staged_action {
        return Err("staged_action_unsafe".to_string());
    }
    Ok(())
}

/// Take only receipt evidence that can settle the submitted delivery.
fn take_accepted_early_ack(handle: &DeliveryHandle) -> Option<PendingAck> {
    let mut state = handle.state.lock().expect("handle state lock");
    match state.early_ack.as_ref().map(|ack| ack.evidence) {
        Some(PendingAckEvidence::Receipt | PendingAckEvidence::DispatchAccepted) => {
            state.early_ack.take()
        }
        Some(PendingAckEvidence::DispatchPending) | None => None,
    }
}

/// Preserve exact hook receipt and TurnKey evidence across claim settlement.
fn advance_with_early_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    early: PendingAck,
) -> bool {
    advance(
        inner,
        handle,
        &[DeliveryState::Submitted],
        early_ack_step(early),
    )
}

fn early_ack_step(early: PendingAck) -> Step<'static> {
    let cause = match early.evidence {
        PendingAckEvidence::Receipt => "hook_ack",
        PendingAckEvidence::DispatchAccepted => "hook_dispatch_accepted_start",
        PendingAckEvidence::DispatchPending => unreachable!("pending dispatch was not taken"),
    };
    Step::to(DeliveryState::DeliveredVerified)
        .cause(cause)
        .verified(VerifiedBy::Hook)
        .turn_edge(early.edge_ms)
        .turn(early.turn)
}

async fn reconcile_recovered_claimed_notification_barrier(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    barrier: ClaimedNotificationBarrier,
) -> AttemptOutcome {
    let notification = handle
        .notification
        .as_ref()
        .expect("staged recovery belongs to a notification");
    let record = match notification.current_record() {
        Ok(record) => record,
        Err(_) => {
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
        }
    };
    let message = match notification.message_line() {
        Ok(message) => message,
        Err(_) => {
            return AttemptOutcome::Failed(AttemptFailure::from_inject(
                "claim_recovery_message_missing".to_string(),
            ));
        }
    };
    let Some(expected) = expected_notification_payload(&record, &message) else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_format_unknown".to_string(),
        ));
    };
    let Some(binding) = record.binding.as_ref() else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_binding_missing".to_string(),
        ));
    };
    let Some(pane_root) = binding.pane_root else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_pane_root_missing".to_string(),
        ));
    };
    let Some(leader) = binding.leader else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_leader_missing".to_string(),
        ));
    };
    let proven = fusion::Binding {
        pane_root: crate::identity::ProcId {
            pid: pane_root.pid(),
            birth: pane_root.birth(),
        },
        leader: crate::identity::ProcId {
            pid: leader.pid(),
            birth: leader.birth(),
        },
        agent: crate::identity::ProcId {
            pid: binding.agent.pid(),
            birth: binding.agent.birth(),
        },
        manifest: binding.manifest.as_str().to_string(),
    };
    let Some(manifest) = inner.manifests.get(&proven.manifest) else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_manifest_missing".to_string(),
        ));
    };
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return AttemptOutcome::Failed(AttemptFailure::from_inject(
            "claim_recovery_route_unavailable".to_string(),
        ));
    };
    let injector = TmuxInjector {
        client: watcher.client(),
        buffer: format!(
            "cyc-{}-{}",
            std::process::id(),
            inner.engine.buffer_seq.fetch_add(1, Ordering::Relaxed)
        ),
    };
    reconcile_claimed_notification_barrier(
        inner,
        handle,
        manifest,
        StagingExpectation {
            target: StagingTarget::ExactRow(&expected),
            payload: &expected,
        },
        &proven,
        &injector,
        ClaimedStagedReconciliation::Recovered(barrier),
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimedStagedReconciliation {
    CurrentStaged,
    Recovered(ClaimedNotificationBarrier),
}

impl ClaimedStagedReconciliation {
    fn barrier(self) -> ClaimedNotificationBarrier {
        match self {
            Self::CurrentStaged => ClaimedNotificationBarrier::Staged,
            Self::Recovered(barrier) => barrier,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StagingExpectation<'a> {
    target: StagingTarget<'a>,
    payload: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimedStagedComposer {
    ExactDoorbell,
    Clean,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimedStagedAction {
    ClearThenSettle,
    SettleOnly,
    Refuse,
}

fn claimed_staged_action(
    composer: ClaimedStagedComposer,
    reconciliation: ClaimedStagedReconciliation,
) -> ClaimedStagedAction {
    match (composer, reconciliation) {
        (ClaimedStagedComposer::ExactDoorbell, _) => ClaimedStagedAction::ClearThenSettle,
        (ClaimedStagedComposer::Clean, ClaimedStagedReconciliation::Recovered(_)) => {
            ClaimedStagedAction::SettleOnly
        }
        (ClaimedStagedComposer::Clean, ClaimedStagedReconciliation::CurrentStaged)
        | (ClaimedStagedComposer::Ambiguous, _) => ClaimedStagedAction::Refuse,
    }
}

fn classify_claimed_staged_composer(
    manifest: &Manifest,
    capture: &str,
    target: StagingTarget<'_>,
    expected_payload: &str,
) -> ClaimedStagedComposer {
    if exact_staging_proof(manifest, capture, target, expected_payload).is_some() {
        return ClaimedStagedComposer::ExactDoorbell;
    }
    if clean_composer_proof(manifest, capture) {
        return ClaimedStagedComposer::Clean;
    }
    ClaimedStagedComposer::Ambiguous
}

/// Reconcile an exact claimed notification barrier.
///
/// The claim proves payload retrieval, not Enter. Cyclops clears only an
/// exact, still-bound doorbell that it can reconstruct byte for byte. Any
/// missing proof becomes one post-write attention state.
async fn reconcile_claimed_notification_barrier<I: Injector>(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staging: StagingExpectation<'_>,
    proven: &fusion::Binding,
    injector: &I,
    reconciliation: ClaimedStagedReconciliation,
) -> AttemptOutcome {
    if proven_binding_unchanged(inner, handle, proven).is_err() {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    };
    let staged = match injector.capture_joined_escaped(&handle.pane_id).await {
        Ok(screen) => screen,
        Err(_) => return AttemptOutcome::Failed(AttemptFailure::verify_timeout()),
    };
    if proven_binding_unchanged(inner, handle, proven).is_err() {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    };
    if row.in_mode {
        return AttemptOutcome::Failed(AttemptFailure::verify_failed());
    }

    let composer =
        classify_claimed_staged_composer(manifest, &staged, staging.target, staging.payload);
    match claimed_staged_action(composer, reconciliation) {
        ClaimedStagedAction::ClearThenSettle => {
            if manifest.injection.clear_keys.is_empty() {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(
                    "claim_clear_unsupported".to_string(),
                ));
            }
            if let Err(cause) =
                notification_staged_action_safe(inner, handle, manifest, &staged, proven, false)
            {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
            }
            if let Err(cause) = injector
                .clear(&handle.pane_id, &manifest.injection.clear_keys)
                .await
            {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
            }
            if !observe_exact_composer_clear(inner, handle, manifest, proven, injector).await {
                return AttemptOutcome::Failed(AttemptFailure::from_inject(
                    "claim_clear_unconfirmed".to_string(),
                ));
            }
        }
        ClaimedStagedAction::SettleOnly => {
            // A crash can land after exact clear but before the settlement
            // fact. The fresh clean observation and exact process binding
            // authorize only the missing durable settlement. No terminal
            // input is sent again.
        }
        ClaimedStagedAction::Refuse => {
            let failure = match composer {
                ClaimedStagedComposer::Clean => {
                    AttemptFailure::verify_owner_missing(ComposerState::ComposerClean)
                }
                ClaimedStagedComposer::Ambiguous => AttemptFailure::verify_failed(),
                ClaimedStagedComposer::ExactDoorbell => {
                    unreachable!("an exact claimed doorbell cannot select the refusal action")
                }
            };
            return AttemptOutcome::Failed(failure);
        }
    }

    if proven_binding_unchanged(inner, handle, proven).is_err() {
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    inject_pause(inner, "pre_claimed_notification_settlement").await;
    let notification = handle
        .notification
        .as_ref()
        .expect("claim reconciliation belongs to a notification");
    let record = match settle_claimed_notification_after_clear(
        notification,
        reconciliation.barrier(),
    ) {
        Ok(record) => record,
        Err(error) => {
            error!(id = %handle.msg_id, %error, "claimed notification settlement failed twice; notification worker remains faulted");
            return AttemptOutcome::Failed(AttemptFailure::claimed_staged_settlement_failed());
        }
    };
    if let Some(binding) = record.binding.as_ref() {
        fusion::resolve_staged_hold(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &record.attempt_id.to_string(),
            binding.agent,
            binding.manifest.as_str(),
        )
        .await;
    }
    if let Some(messaging) = inner.workspace_messaging() {
        messaging.composer_barrier_retired(record.attempt_id);
        if let Err(error) = messaging.notification_head_changed(notification.recipient()) {
            error!(id = %handle.msg_id, %error, "cannot advance notification FIFO after staged claim");
        }
    } else {
        error!(
            id = %handle.msg_id,
            "cannot advance notification FIFO after staged claim without workspace messaging"
        );
    }
    AttemptOutcome::Done
}

/// Retry only the content-free durable settlement after a proven clear.
///
/// The first error may be an interrupted append whose outcome the caller did
/// not observe. The store operation is idempotent, so one immediate repeat can
/// discover an already-landed fact or append the missing one. It never clears
/// the composer or sends a terminal key.
fn settle_claimed_notification_after_clear(
    notification: &NotificationContext,
    barrier: ClaimedNotificationBarrier,
) -> Result<cyclops_proto::NotificationRecord, NotificationAdapterError> {
    let settle = || match barrier {
        ClaimedNotificationBarrier::Staged => notification.settle_claimed_staged_clear(),
        ClaimedNotificationBarrier::AckTimeout => {
            notification.settle_claimed_ack_timeout_reconciliation()
        }
    };
    match settle() {
        Ok(record) => Ok(record),
        Err(first) => {
            warn!(
                message_id = %notification.message_id(),
                attempt_id = %notification.attempt_id(),
                error = %first,
                "retrying claimed notification settlement once"
            );
            settle()
        }
    }
}

#[cfg(test)]
fn settle_claimed_staged_after_clear(
    notification: &NotificationContext,
) -> Result<cyclops_proto::NotificationRecord, NotificationAdapterError> {
    settle_claimed_notification_after_clear(notification, ClaimedNotificationBarrier::Staged)
}

async fn observe_exact_composer_clear<I: Injector>(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    proven: &fusion::Binding,
    injector: &I,
) -> bool {
    let mut last_delay = 0;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        let Some(watcher) = watcher_for_handle(inner, handle) else {
            return false;
        };
        if proven_binding_unchanged(inner, handle, proven).is_err() {
            return false;
        }
        let Ok(capture) = injector.capture_joined_escaped(&handle.pane_id).await else {
            continue;
        };
        let Some(row) = watcher.pane(&handle.pane_id) else {
            return false;
        };
        if proven_binding_unchanged(inner, handle, proven).is_err() {
            return false;
        }
        if !row.in_mode
            && clean_composer_proof(manifest, &capture)
            && notification_staged_action_safe(inner, handle, manifest, &capture, proven, false)
                .is_ok()
        {
            return true;
        }
    }
    false
}

/// Is the pane still held by the process and rules that Enter reached?
///
/// Receipt evidence answers "did the message land", and it can only
/// answer that about the occupant it was sent to. A pane id is reusable,
/// so a replacement process can clear the marker, change the window, emit
/// output, and even fire a hook carrying the old message id. None of that
/// is evidence about the delivery, and treating it as evidence is how a
/// record starts to lie.
fn submitted_binding_holds(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    let want_agent = *handle.submitted_agent.lock().expect("submitted agent lock");
    let want_manifest = handle
        .submitted_manifest
        .lock()
        .expect("submitted manifest lock")
        .clone();
    let (Some(want_agent), Some(want_manifest), Some(row)) =
        (want_agent, want_manifest, watcher.pane(&handle.pane_id))
    else {
        return false;
    };
    if row.dead {
        return false;
    }
    // The foreground leader is allowed to change after Enter. An agent can
    // hand the terminal to a tool or take it back without changing who owns
    // the delivery. Re-prove the admitted agent instance and its rules.
    fusion::admitted_binding(inner, handle.session_idx, &row)
        .is_some_and(|binding| binding.agent == want_agent && binding.manifest == want_manifest)
}

/// Pane-rebind re-check between the gate's admitting recompute and the
/// irreversible injection steps. The pane must still exist, be alive, keep
/// the pid it was admitted with, and bind to the manifest the gate
/// admitted. Err carries the mismatch detail for the gate ledger line; the
/// delivery then retries through the gate, which re-evaluates from scratch.
fn occupant_unchanged(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
    admitted_pid: i32,
) -> Result<(), String> {
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err("pane_gone".to_string());
    };
    if row.dead {
        return Err("pane_dead".to_string());
    }
    // Copy-mode after admission: the human is scrolling, and a paste now
    // lands somewhere neither of us can see. The gate checks this before
    // admitting, but admission is a decision about a moment and the human
    // can enter copy-mode inside the window that follows.
    if row.in_mode {
        return Err("pane_in_mode".to_string());
    }
    if fusion::foreground_pid(row.pane_pid) != admitted_pid {
        return Err("pane_pid_changed".to_string());
    }
    match fusion::bind_manifest_for(inner, handle.session_idx, &row) {
        Some(m) if m.agent.id == manifest_id => Ok(()),
        Some(_) => Err("manifest_changed".to_string()),
        None => Err("manifest_unbound".to_string()),
    }
}

/// Re-prove the complete binding captured at the write boundary.
///
/// PID numbers alone are reusable. The submit path must retain the same
/// terminal leader generation, admitted agent generation, and manifest that
/// authorized the paste.
fn proven_binding_unchanged(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) -> Result<(), String> {
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return Err("session_detached".to_string());
    };
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err("pane_gone".to_string());
    };
    if row.dead {
        return Err("pane_dead".to_string());
    }
    if row.in_mode {
        return Err("pane_in_mode".to_string());
    }
    let Some(current) = fusion::admitted_binding(inner, handle.session_idx, &row) else {
        return Err("binding_unprovable".to_string());
    };
    if !binding_is_exact(Some(&current), proven) {
        return Err("binding_changed".to_string());
    }
    Ok(())
}

fn binding_is_exact(current: Option<&fusion::Binding>, proven: &fusion::Binding) -> bool {
    current == Some(proven)
}

fn process_instance(pid: i32) -> Option<ProcessInstanceId> {
    let process = crate::identity::ProcId::of(pid)?;
    process_instance_id(process)
}

fn process_instance_id(process: crate::identity::ProcId) -> Option<ProcessInstanceId> {
    ProcessInstanceId::new(process.pid, process.birth).ok()
}

fn binding_unprovable_observation(
    inner: &Inner,
    handle: &DeliveryHandle,
    pane_pid: i32,
    manifest_id: &str,
) -> NotificationPreWriteObservation {
    NotificationPreWriteObservation {
        pane_root: process_instance(pane_pid),
        selected_manifest: NotificationManifestId::new(manifest_id).ok(),
        binding: None,
        route_evidence: Some(inner.route_evidence_id(handle.session_idx, &handle.pane_id)),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    }
}

/// A notification may continue without a clean-composer proof only for an
/// A notification may continue without a clean-composer proof for an
/// authenticated agent unless positive human input or a modal is present.
/// Cyclops must never type over a person's active text.
fn unproven_composer_is_eligible(detection: &Detection) -> bool {
    if detection.composer_semantic == Some(ComposerSemantic::HumanInput)
        || matches!(
            detection.state,
            AgentState::BlockedModal
                | AgentState::BlockedPermission
                | AgentState::BlockedQuota
                | AgentState::Dead
        )
        || detection.write_block.as_deref() == Some("composer_hold")
        || detection.write_block.as_deref() == Some("pane_in_mode")
    {
        return false;
    }
    true
}

/// Return the current foreground agent process for the explicit liveness
/// policy. Unreadable composers do not block a notification, but a stale or
/// mismatched process binding still does: that would risk typing into a shell
/// or a different agent.
fn notification_pane_for_unproven_composer(
    inner: &Inner,
    handle: &DeliveryHandle,
    row: &PaneRow,
    manifest_id: &str,
    detection: &Detection,
) -> Option<i32> {
    if !unproven_composer_is_eligible(detection) {
        return None;
    }
    if crate::deadlock::pane_runs_watch(row.pane_pid) {
        return None;
    }
    let binding = fusion::admitted_binding(inner, handle.session_idx, row)?;
    if binding.manifest != manifest_id {
        return None;
    }
    fusion::foreground_pid_checked(row.pane_pid)
}

#[allow(dead_code)]
fn composer_semantic_missing(manifest: &Manifest, detection: &Detection) -> bool {
    detection
        .readings
        .iter()
        .find(|reading| {
            reading.sensor == cyclops_proto::Sensor::Screen && reading.state == AgentState::Idle
        })
        .and_then(|reading| manifest.rules.iter().find(|rule| rule.id == reading.rule))
        .is_some_and(|rule| rule.composer_semantic.is_none())
}

fn composer_semantic_observation(
    inner: &Inner,
    handle: &DeliveryHandle,
    row: &PaneRow,
    manifest_id: &str,
) -> Option<NotificationPreWriteObservation> {
    let notification = handle.notification.as_ref()?;
    let binding = fusion::admitted_binding(inner, handle.session_idx, row)?;
    if binding.manifest != manifest_id {
        return None;
    }

    Some(NotificationPreWriteObservation {
        pane_root: Some(process_instance_id(binding.pane_root)?),
        selected_manifest: Some(NotificationManifestId::new(&binding.manifest).ok()?),
        binding: Some(notification_binding(notification.recipient(), &binding)?),
        route_evidence: Some(inner.route_evidence_id(handle.session_idx, &handle.pane_id)),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    })
}

fn notification_binding(
    recipient: RecipientKey,
    binding: &fusion::Binding,
) -> Option<NotificationBinding> {
    Some(NotificationBinding {
        recipient,
        pane_root: Some(process_instance_id(binding.pane_root)?),
        leader: Some(process_instance_id(binding.leader)?),
        agent: process_instance_id(binding.agent)?,
        manifest: NotificationManifestId::new(&binding.manifest).ok()?,
    })
}

/// Await the test-only injection pause, when one is installed. Production
/// never installs one; this is a no-op there.
pub(crate) async fn inject_pause(inner: &Arc<Inner>, phase: &'static str) {
    let hook = inner
        .inject_pause
        .lock()
        .expect("inject pause lock")
        .clone();
    if let Some(h) = hook {
        h(phase).await;
    }
}

/// The gate hold cause that no pane event will ever clear: the daemon
/// could not read who is in the pane.
const OBSERVATION_HOLD: &str = "occupant_unprovable";
/// The gate hold for an idle pane whose composer keeps reading `ambiguous`.
/// Held on events like any composer cause, but also on a timed wake at the
/// settle boundary: ambiguity that never changes emits no pane event, and
/// without the timer the wake would wait in memory forever instead of
/// settling as the durable `composer_semantic_ambiguous` block.
const AMBIGUOUS_COMPOSER_HOLD: &str = "not_write_ready:composer_semantic_ambiguous";
const WRITE_READINESS_OBSERVATION_HOLD: &str = "not_write_ready:occupant_unprovable";
/// The write block a hook-liveness manifest stamps when no admitting hook
/// edge has been published for the pane's current binding. Durable, never
/// retried: the wake parks as a named pre-write block until the recipient
/// claims, its next admitting edge reopens the oldest attempt once, or an
/// administrator withdraws the exact attempt.
pub(crate) const HOOK_ADMISSION_UNPROVEN: &str = "hook_admission_unproven";

/// How long that one cause waits before looking again. Short enough that
/// a transient `ps` failure costs a person nothing, long enough that a
/// permanently unreadable process table is not a spin.
const OBSERVATION_RETRY: Duration = Duration::from_millis(250);

/// Mailbox attempts remain in workspace Gating while an event can change the
/// answer. Named exhausted failures settle as durable BlockedPreWrite records.
/// Direct deliveries retain the legacy attention and quota outcomes.
fn workspace_prewrite_hold(handle: &DeliveryHandle, cause: &str) -> Option<String> {
    (!handle.owns_session_delivery_state()).then(|| cause.to_string())
}

fn gate_hold_action(handle: &DeliveryHandle, cause: &str) -> &'static str {
    if !handle.owns_session_delivery_state() && cause == "blocked_quota" {
        "wait"
    } else {
        "hold"
    }
}

fn workspace_prewrite_failure_is_deferred(
    handle: &DeliveryHandle,
    failure: &AttemptFailure,
) -> bool {
    !handle.owns_session_delivery_state() && matches!(failure.boundary, WriteBoundary::BeforeWrite)
}

/// Retry accounting. Only failures proven to precede the pane write may
/// consume the configured retry budget. True means the caller should retry
/// immediately. False means a direct delivery ended in attention_required or
/// a workspace notification remains durably held or blocked for recovery.
async fn fail_attempt(
    inner: &Arc<Inner>,
    worker: &Worker,
    handle: &Arc<DeliveryHandle>,
    failure: &AttemptFailure,
) -> bool {
    // What the budget has actually spent: attempts that reached the
    // transport, not attempts that stopped at a refused barrier.
    let spent = {
        let st = handle.state.lock().expect("handle state lock");
        st.attempts.saturating_sub(st.regates)
    };
    let from = [
        DeliveryState::Pasting,
        DeliveryState::Staged,
        DeliveryState::Submitted,
        DeliveryState::RetryQueued,
    ];
    if should_retry_attempt(handle, failure, spent, inner.cfg.delivery_retry_max) {
        advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::RetryQueued).cause(&failure.cause),
        )
    } else {
        if let (true, Some(block)) = (
            handle.notification.is_some(),
            failure.pre_write_block.as_deref(),
        ) {
            persist_notification_prewrite_block(
                inner,
                worker,
                handle,
                block.cause,
                block.observation.clone(),
            )
            .await;
            return false;
        }
        if workspace_prewrite_failure_is_deferred(handle, failure) {
            // The workspace attempt remains Gating. No pane write happened,
            // so a terminal notification would be false and a legacy state
            // would create a second authority. A later route event or daemon
            // restart can attach a fresh worker to the same durable attempt.
            notify_notification_deferred(inner, handle, &failure.cause);
            return false;
        }
        if matches!(failure.boundary, WriteBoundary::AfterWrite) {
            if let Some(notification) = &handle.notification {
                let result = match failure.verify_outcome {
                    Some(outcome) => notification.record_verify_attention(outcome),
                    None => {
                        notification.record_attention(notification_attention_cause(&failure.cause))
                    }
                };
                match result {
                    Ok(record) => {
                        if let Some(messaging) = inner.workspace_messaging() {
                            messaging.notification_attention_recorded(record);
                        }
                    }
                    Err(NotificationAdapterError::TerminalConflict(_)) => return false,
                    Err(error) => {
                        // The workspace journal remains at its last
                        // post-write state. Explicit restart recovery can
                        // close it without risking another pane write.
                        //
                        // That is the right durable choice and it used to
                        // be the whole response, which left the attempt
                        // invisible: still in flight, so `open_alarms`
                        // skips it (it filters on AttentionRequired), no
                        // wake block, so the scheduler reports nothing to
                        // do, and the recipient's head never advances. The
                        // pre-write sibling already faults the worker on a
                        // storage failure (`record_notification_prewrite_
                        // block`), so this is the same failure reported the
                        // same way rather than a new mechanism: the fault
                        // reaches `notification_worker_diagnostics` and so
                        // `cyclops status`, which is where an operator
                        // learns that this daemon needs a restart.
                        error!(id = %handle.msg_id, error = %error, "notification attention fact failed");
                        worker.set_fault(format!("notification attention storage failed: {error}"));
                    }
                }
            }
        }
        let moved = advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::AttentionRequired).cause(&failure.cause),
        );
        if moved {
            notify_attention(inner, handle, &failure.cause);
        }
        false
    }
}

fn notify_notification_prewrite_blocked(
    inner: &Arc<Inner>,
    handle: &DeliveryHandle,
    block: &MessagingPreWriteBlock,
) {
    let cause = serde_json::to_value(block.cause)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        &format!("notification to {} blocked before write", handle.to),
        &format!(
            "message {} attempt {}: {cause}; the mailbox remains claimable",
            handle.msg_id, block.attempt_id
        ),
        Some(&handle.msg_id),
        Some(handle.session_idx),
        About::pane(&handle.pane_id),
    );
}

fn notification_attention_cause(cause: &str) -> NotificationAttentionCause {
    match cause {
        "paste_failed" => NotificationAttentionCause::PasteFailed,
        "verify_failed" => NotificationAttentionCause::VerifyFailed,
        "pane_rebound_after_paste" => NotificationAttentionCause::PaneReboundAfterPaste,
        "submit_failed" => NotificationAttentionCause::SubmitFailed,
        "receipt_occupant_changed" => NotificationAttentionCause::ReceiptOccupantChanged,
        "ack_timeout" => NotificationAttentionCause::AckTimeout,
        _ => NotificationAttentionCause::TransportOutcomeUnknown,
    }
}

fn should_retry(failure: &AttemptFailure, spent: u32, retry_max: u32) -> bool {
    // Unproven hook admission is a durable block, never a retry budget
    // question: only an admitting edge, a claim, or a withdrawal moves it.
    if failure.cause == HOOK_ADMISSION_UNPROVEN {
        return false;
    }
    matches!(failure.boundary, WriteBoundary::BeforeWrite)
        && !matches!(
            failure.cause.as_str(),
            "pane_too_narrow" | "composer_ownership_unproven" | "binding_unprovable"
        )
        && spent <= retry_max
}

fn should_retry_attempt(
    handle: &DeliveryHandle,
    failure: &AttemptFailure,
    spent: u32,
    retry_max: u32,
) -> bool {
    // A workspace notification already has a durable Writing fact. Its exact
    // zero-byte correction must remain withdrawable instead of being replayed
    // automatically. Legacy direct delivery has no such durable attempt and
    // may use the existing bounded pre-write retry.
    !(handle.notification.is_some() && failure.cause == "paste_command_unwritten")
        && should_retry(failure, spent, retry_max)
}

fn notify_attention(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>, cause: &str) {
    if !handle.owns_session_delivery_state() {
        // Workspace NotificationState and messages.changed own mailbox
        // attention. A delivery-scoped ping would point at the suppressed
        // session projection and could never observe guarded resolution.
        return;
    }
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        &format!("delivery to {} needs attention", handle.to),
        &format!("message {}: {cause}", handle.msg_id),
        Some(&handle.msg_id),
        Some(handle.session_idx),
        About::delivery(&handle.to),
    );
}

/// Report a pre-write notification stall without inventing a terminal fact.
/// The payload and composer capture remain outside both the ping and logs.
fn notify_notification_deferred(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>, cause: &str) {
    admin_notify(
        inner,
        NotifyLevel::Fyi,
        "notification remains queued before write",
        &format!(
            "message {} to {} remains queued or gating ({cause})",
            handle.msg_id, handle.to
        ),
        None,
        None,
        About::default(),
    );
}

/// Quota parking: the in-flight delivery and everything queued behind it
/// park, the worker is flagged, and the admin is alerted once with the
/// reset hint. Nothing here ever requeues automatically.
async fn park_recipient(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
    hint: Option<String>,
) {
    if let Some(notification) = &handle.notification {
        match notification.record_quota_held() {
            Ok(_) => {}
            Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => return,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification quota-held fact failed");
                notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                return;
            }
        }
        advance(
            inner,
            handle,
            &[DeliveryState::Gating],
            Step::to(DeliveryState::ParkedBlockedQuota).cause("blocked_quota"),
        );
        let hint = hint.unwrap_or_else(|| "quota exhausted".to_string());
        admin_notify(
            inner,
            NotifyLevel::Urgent,
            &format!("{} held: quota exhausted", handle.to),
            &format!(
                "message {} to {} is held ({hint}); it will not resume automatically",
                handle.msg_id, handle.to
            ),
            Some(&handle.msg_id),
            Some(handle.session_idx),
            About::delivery(&handle.to),
        );
        // The positive reset edge can race this durable hold append. If it
        // already won, the edge's scan found no held attempt. Recheck the
        // exact route once after the hold exists so the attempt cannot be
        // stranded until another unrelated redraw.
        if let Some(observation) =
            fusion::quota_reset_observation_now(inner, handle.session_idx, &handle.pane_id)
        {
            crate::apply_messaging_observation(inner, observation);
        }
        return;
    }
    let hint = hint.unwrap_or_else(|| "quota exhausted".to_string());
    *worker.parked.lock().expect("parked lock") = Some(hint.clone());
    advance(
        inner,
        handle,
        &[DeliveryState::Gating],
        Step::to(DeliveryState::ParkedBlockedQuota)
            .cause("blocked_quota")
            .note(hint.clone()),
    );
    let drained = worker.drain_pending();
    let (direct, notifications) = split_legacy_parked_queue(drained);
    for h in direct {
        advance(
            inner,
            &h,
            &[DeliveryState::Queued],
            Step::to(DeliveryState::ParkedBlockedQuota)
                .cause("blocked_quota")
                .note(hint.clone()),
        );
    }
    if !notifications.is_empty() {
        // These handles were ahead of anything enqueued after the drain.
        worker.prepend(notifications);
        worker.notify.notify_one();
    }
    admin_notify(
        inner,
        NotifyLevel::Urgent,
        &format!("{} parked: quota exhausted", handle.to),
        &format!(
            "deliveries to {} are parked ({hint}); re-queue is an operator action",
            handle.to
        ),
        Some(&handle.msg_id),
        Some(handle.session_idx),
        About::delivery(&handle.to),
    );
}

fn legacy_park_hint(handle: &DeliveryHandle, hint: Option<String>) -> Option<String> {
    hint.filter(|_| handle.owns_session_delivery_state())
}

fn split_legacy_parked_queue(
    handles: Vec<Arc<DeliveryHandle>>,
) -> (Vec<Arc<DeliveryHandle>>, Vec<Arc<DeliveryHandle>>) {
    handles
        .into_iter()
        .partition(|handle| handle.owns_session_delivery_state())
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

enum GateOutcome {
    Proceed {
        manifest_id: String,
        /// pane_pid of the admitted occupant, re-checked before paste and
        /// submit: a pane whose occupant changed after admit must never be
        /// injected into.
        pane_pid: i32,
        /// The regate hold observed an exact state, readiness, or pane edge.
        /// Only this grants a fresh immediate re-proof allowance.
        regate_evidence_changed: bool,
    },
    Park {
        hint: Option<String>,
    },
    Attention {
        cause: String,
    },
    /// Repeated identical evidence proved this exact mailbox attempt cannot
    /// reach the write boundary. The durable block makes it visible and
    /// operator-withdrawable without touching the pane.
    BlockedPreWrite {
        cause: NotificationPreWriteCause,
        observation: Box<NotificationPreWriteObservation>,
    },
    /// A mailbox notification remains durably queued or gating. The
    /// in-memory worker stops, and the next route or restart reconciliation
    /// can attach a fresh worker without inventing a terminal session fact.
    Deferred {
        cause: String,
    },
    Withdrawn,
}

/// The delivery gate, in spec order: pane resolution and liveness, mode,
/// fused state (quota park, modal decline-or-hold, working composer proof,
/// idle_with_input hold, idle proceed). Event-driven: holds wake on fused
/// state changes, pane field changes, and session reattach. The recompute
/// that admits a delivery runs immediately before pasting, so the gate
/// snapshot is fresher than any human keystroke round-trip.
async fn gate(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    initial_hold: Option<String>,
) -> GateOutcome {
    let mut declines: HashMap<String, u32> = HashMap::new();
    let mut notified_rules: HashSet<String> = HashSet::new();
    let mut last_hold: Option<String> = None;
    let mut forced_hold = initial_hold;
    let mut regate_evidence_changed = false;
    // One-shot visibility for wedged holds: a delivery held in gating past
    // the configured threshold pings the admin exactly once.
    let mut hold_since: Option<Instant> = None;
    let mut hold_notified = false;
    // Subscribe once before the first evaluation and retain this receiver for
    // the gate's whole lifetime. Replacing it between re-evaluations leaves a
    // gap where a settled readiness edge can be published after an early pane
    // wake but before the next receiver exists, stranding a now-clean pane.
    let mut ev_rx = inner.events.subscribe();
    // When the idle-ambiguous composer hold began. Cleared whenever any
    // other verdict interrupts, so only unbroken ambiguity can outlive the
    // settle window and become the durable block.
    let mut ambiguous_since: Option<Instant> = None;
    let ambiguous_settle = Duration::from_millis(inner.cfg.ambiguous_composer_settle_ms);
    'gate: loop {
        // The event receiver predates every evaluation, so events published
        // mid-evaluation or between iterations remain buffered. Evaluation
        // itself is still authoritative.
        let watcher = watcher_for_handle(inner, handle);
        let mut pane_rx = watcher.as_ref().map(|w| w.subscribe());

        if let Some(notification) = &handle.notification {
            match notification.ensure_current_gating() {
                Ok(()) => {}
                Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => {
                    return GateOutcome::Withdrawn;
                }
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification gate recheck failed");
                    return GateOutcome::Deferred {
                        cause: NOTIFICATION_RECORD_FAILED.to_string(),
                    };
                }
            }
        }

        // `initial_hold` is only a receipt seed. For a notification that
        // starts while a human draft is visible, take the fresh gate path so
        // it can record the exact durable `composer_hold` refusal below;
        // carrying the cached `idle_with_input` value straight into the wait
        // loop would make that refusal invisible until another pane event.
        let initial_hold = forced_hold.take().filter(|cause| {
            // A cached `Working` verdict is only a receipt hint. Workspace
            // notifications must take the fresh capture path so a clean
            // composer can admit a doorbell during the turn. Likewise a
            // cached draft must be re-read so the durable composer hold is
            // recorded immediately. Direct deliveries retain their legacy
            // cached-hold behaviour.
            !(handle.notification.is_some()
                && matches!(cause.as_str(), "idle_with_input" | "working"))
        });
        let hold = if let Some(cause) = initial_hold {
            Some(cause)
        } else {
            match &watcher {
                None => Some("session_detached".to_string()),
                Some(w) => 'pane: {
                    let Some(row) = w.pane(&handle.pane_id) else {
                        if let Some(hold) = workspace_prewrite_hold(handle, "no_such_pane") {
                            break 'pane Some(hold);
                        }
                        return GateOutcome::Attention {
                            cause: "no_such_pane".to_string(),
                        };
                    };
                    if row.dead {
                        if let Some(hold) = workspace_prewrite_hold(handle, "pane_dead") {
                            break 'pane Some(hold);
                        }
                        return GateOutcome::Attention {
                            cause: "pane_dead".to_string(),
                        };
                    }
                    if row.in_mode {
                        // Human scrolling in copy-mode; %pane-mode-changed
                        // re-triggers via the pane event stream.
                        Some("pane_in_mode".to_string())
                    } else {
                        let Some(manifest) =
                            fusion::bind_manifest_for(inner, handle.session_idx, &row)
                        else {
                            if let Some(hold) = workspace_prewrite_hold(handle, "no_manifest") {
                                break 'pane Some(hold);
                            }
                            return GateOutcome::Attention {
                                cause: "no_manifest".to_string(),
                            };
                        };
                        let manifest_id = manifest.agent.id.clone();
                        let Some(det) = crate::observe_pane(
                            inner,
                            handle.session_idx,
                            w,
                            &handle.pane_id,
                            true,
                            "gate",
                        )
                        .await
                        else {
                            if let Some(hold) = workspace_prewrite_hold(handle, "no_such_pane") {
                                break 'pane Some(hold);
                            }
                            return GateOutcome::Attention {
                                cause: "no_such_pane".to_string(),
                            };
                        };

                        if handle.notification.is_some() {
                            if let Some(pane_pid) = notification_pane_for_unproven_composer(
                                inner,
                                handle,
                                &row,
                                &manifest_id,
                                &det,
                            ) {
                                gate_line(inner, handle, "proceed", Some(&det.decided_by), None);
                                return GateOutcome::Proceed {
                                    manifest_id,
                                    pane_pid,
                                    regate_evidence_changed,
                                };
                            }
                        }
                        if handle.notification.is_some()
                            && det.write_block.as_deref() == Some(HOOK_ADMISSION_UNPROVEN)
                        {
                            let Some(mut observation) =
                                composer_semantic_observation(inner, handle, &row, &manifest_id)
                            else {
                                return GateOutcome::BlockedPreWrite {
                                    cause: NotificationPreWriteCause::BindingUnprovable,
                                    observation: Box::new(binding_unprovable_observation(
                                        inner,
                                        handle,
                                        row.pane_pid,
                                        &manifest_id,
                                    )),
                                };
                            };
                            observation.write_block = Some(HOOK_ADMISSION_UNPROVEN.to_string());
                            return GateOutcome::BlockedPreWrite {
                                cause: NotificationPreWriteCause::WriteReadinessChanged,
                                observation: Box::new(observation),
                            };
                        }
                        match det.state {
                            AgentState::Idle => {
                                // Runtime idle is not permission to write. A
                                // turn-end hook can put the pane in idle while
                                // the composer holds a staged payload the screen
                                // sensor could not read. Proceeding pastes over it.
                                match (det.write_ready, det.write_block.as_deref()) {
                                    (true, _) => {
                                        // The admitted pid is what every
                                        // receipt is later held against, so an
                                        // unreadable process table is a
                                        // refusal, not a shrug. Falling back
                                        // to the pane root here would pin the
                                        // delivery to the SHELL and then
                                        // resolve receipts against whoever
                                        // sits at that prompt next.
                                        //
                                        // A HOLD rather than an ending: not
                                        // being able to name the occupant is
                                        // doubt, and nothing has been written
                                        // yet. A respawned pane updates its
                                        // pid in the table without emitting a
                                        // pane change, so the row can briefly
                                        // name a process that has already
                                        // exited, and ending the delivery
                                        // there would summon a human for a
                                        // table that was about to catch up.
                                        let admitted = fusion::admitted_binding(
                                            inner,
                                            handle.session_idx,
                                            &row,
                                        )
                                        .filter(|b| b.manifest == manifest_id);
                                        match admitted {
                                            None if handle.notification.is_some()
                                                && last_hold.as_deref()
                                                    == Some(OBSERVATION_HOLD) =>
                                            {
                                                return GateOutcome::BlockedPreWrite {
                                                    cause:
                                                        NotificationPreWriteCause::BindingUnprovable,
                                                    observation: Box::new(
                                                        binding_unprovable_observation(
                                                            inner,
                                                            handle,
                                                            row.pane_pid,
                                                            &manifest_id,
                                                        ),
                                                    ),
                                                };
                                            }
                                            None => Some(OBSERVATION_HOLD.to_string()),
                                            Some(_) => {
                                                match fusion::foreground_pid_checked(row.pane_pid) {
                                                    None => Some(OBSERVATION_HOLD.to_string()),
                                                    Some(pane_pid) => {
                                                        gate_line(
                                                            inner,
                                                            handle,
                                                            "proceed",
                                                            Some(&det.decided_by),
                                                            None,
                                                        );
                                                        return GateOutcome::Proceed {
                                                            manifest_id,
                                                            pane_pid,
                                                            regate_evidence_changed,
                                                        };
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Hold on an event, never a clock: the next
                                    // pane change re-evaluates, and a screen
                                    // sensor that can see the composer resolves
                                    // it without anyone pasting blind.
                                    // Fusion may have recorded the same failed
                                    // lookup in write readiness. Settle that path
                                    // after the same bounded second observation.
                                    (false, Some(OBSERVATION_HOLD))
                                        if handle.notification.is_some()
                                            && last_hold.as_deref()
                                                == Some(WRITE_READINESS_OBSERVATION_HOLD) =>
                                    {
                                        return GateOutcome::BlockedPreWrite {
                                            cause: NotificationPreWriteCause::BindingUnprovable,
                                            observation: Box::new(binding_unprovable_observation(
                                                inner,
                                                handle,
                                                row.pane_pid,
                                                &manifest_id,
                                            )),
                                        };
                                    }
                                    // A composer that reads `ambiguous` on an
                                    // idle pane may be one frame from proof (a
                                    // redraw caught mid-paint) or may never be
                                    // provable at all (a manifest whose rules
                                    // cannot classify this vendor's clean
                                    // composer). No single frame separates the
                                    // two, so the first reading holds — but
                                    // only for the settle window. Ambiguity
                                    // that outlives it is a manifest gap
                                    // wearing a transient's clothes, and no
                                    // pane event announces "still ambiguous",
                                    // so the wake settles as a durable,
                                    // operator-visible block instead of
                                    // waiting in memory forever. Working
                                    // frames never reach this arm (the
                                    // Working arm above owns them), so
                                    // mid-turn ambiguity — deliberate where a
                                    // vendor's mid-turn injection is
                                    // unmeasured — cannot escalate.
                                    (false, _)
                                        if handle.notification.is_some()
                                            && unproven_composer_is_eligible(&det) =>
                                    {
                                        if fusion::composer_has_unsubmitted_draft(
                                            inner,
                                            handle.session_idx,
                                            &handle.pane_id,
                                        ) {
                                            Some("composer_hold".to_string())
                                        } else {
                                            match fusion::foreground_pid_checked(row.pane_pid) {
                                                None => Some(OBSERVATION_HOLD.to_string()),
                                                Some(pane_pid) => {
                                                    gate_line(
                                                        inner,
                                                        handle,
                                                        "proceed",
                                                        Some(&det.decided_by),
                                                        None,
                                                    );
                                                    return GateOutcome::Proceed {
                                                        manifest_id,
                                                        pane_pid,
                                                        regate_evidence_changed,
                                                    };
                                                }
                                            }
                                        }
                                    }
                                    // A staged human draft is an exact boundary,
                                    // not a terminal delivery outcome. Keep the
                                    // notification in Gating until a pane edge
                                    // proves the draft was submitted or erased.
                                    (false, Some("composer_hold"))
                                        if handle.notification.is_some() =>
                                    {
                                        Some("composer_hold".to_string())
                                    }
                                    (false, reason) => Some(format!(
                                        "not_write_ready:{}",
                                        reason.unwrap_or("unstamped")
                                    )),
                                }
                            }
                            AgentState::Dead => {
                                if let Some(hold) = workspace_prewrite_hold(handle, "pane_dead") {
                                    break 'pane Some(hold);
                                }
                                return GateOutcome::Attention {
                                    cause: "pane_dead".to_string(),
                                };
                            }
                            AgentState::BlockedQuota => {
                                let hint = quota_hint(w, &handle.pane_id).await;
                                gate_line(
                                    inner,
                                    handle,
                                    "park",
                                    Some(&det.decided_by),
                                    Some("blocked_quota"),
                                );
                                return GateOutcome::Park { hint };
                            }
                            AgentState::BlockedModal | AgentState::BlockedPermission => {
                                let rule = inner.manifests.get(&manifest_id).and_then(|m| {
                                    m.rules
                                        .iter()
                                        .find(|r| r.id == det.decided_by && r.state.is_blocked())
                                });
                                match rule {
                                    Some(r)
                                        if r.auto_dismiss
                                            && !r.decline_keys.is_empty()
                                            && *declines.get(&r.id).unwrap_or(&0)
                                                < MAX_DECLINES =>
                                    {
                                        *declines.entry(r.id.clone()).or_insert(0) += 1;
                                        gate_line(inner, handle, "decline", Some(&r.id), None);
                                        let keys = r.decline_keys.clone();
                                        let rule_id = r.id.clone();
                                        if !send_decline_keys(
                                            w,
                                            &handle.pane_id,
                                            manifest,
                                            &rule_id,
                                            &keys,
                                        )
                                        .await
                                        {
                                            // The screen changed under the
                                            // decline (TOCTOU): the confirming
                                            // key was withheld. Back to the
                                            // gate loop to re-read reality.
                                            gate_line(
                                                inner,
                                                handle,
                                                "decline_aborted",
                                                Some(&rule_id),
                                                Some("modal_changed"),
                                            );
                                        }
                                        // One-shot settle so the dismissal
                                        // renders before the re-check; the
                                        // decline count bounds this loop.
                                        tokio::time::sleep(DECLINE_SPACING).await;
                                        continue 'gate;
                                    }
                                    _ => {
                                        // Trust/permission prompts belong to the
                                        // human: hold and alert, never dismiss.
                                        let rule_id = rule
                                            .map(|r| r.id.clone())
                                            .unwrap_or_else(|| det.decided_by.clone());
                                        if notified_rules.insert(rule_id.clone()) {
                                            admin_notify(
                                            inner,
                                            NotifyLevel::ActionRequired,
                                            &format!("{} blocked: {rule_id}", handle.to),
                                            &format!(
                                                "delivery {} is held; rule {rule_id} needs a decision",
                                                handle.msg_id
                                            ),
                                            Some(&handle.msg_id),
                                            Some(handle.session_idx),
                                            // The pane, not the delivery:
                                            // the delivery is only gating,
                                            // and the thing a human clears
                                            // is the prompt on the pane.
                                            About::pane(&handle.pane_id),
                                        );
                                        }
                                        Some(format!("blocked:{rule_id}"))
                                    }
                                }
                            }
                            AgentState::Working => {
                                // Runtime state is not permission to write,
                                // but it is not an automatic refusal either.
                                // Under the direct pane interruption contract:
                                // For notification doorbells, working state is an observation,
                                // not a delivery blocker. Only a proven non-Cyclops draft holds it.
                                if handle.notification.is_some() {
                                    if fusion::composer_has_unsubmitted_draft(
                                        inner,
                                        handle.session_idx,
                                        &handle.pane_id,
                                    ) {
                                        Some("composer_hold".to_string())
                                    } else {
                                        match fusion::foreground_pid_checked(row.pane_pid) {
                                            None if last_hold.as_deref()
                                                == Some(OBSERVATION_HOLD) =>
                                            {
                                                return GateOutcome::BlockedPreWrite {
                                                    cause:
                                                        NotificationPreWriteCause::BindingUnprovable,
                                                    observation: Box::new(
                                                        binding_unprovable_observation(
                                                            inner,
                                                            handle,
                                                            row.pane_pid,
                                                            &manifest_id,
                                                        ),
                                                    ),
                                                };
                                            }
                                            None => Some(OBSERVATION_HOLD.to_string()),
                                            Some(pane_pid) => {
                                                gate_line(
                                                    inner,
                                                    handle,
                                                    "proceed",
                                                    Some(&det.decided_by),
                                                    None,
                                                );
                                                return GateOutcome::Proceed {
                                                    manifest_id,
                                                    pane_pid,
                                                    regate_evidence_changed,
                                                };
                                            }
                                        }
                                    }
                                } else if !det.write_ready {
                                    Some(
                                        det.write_block
                                            .clone()
                                            .unwrap_or_else(|| "working".to_string()),
                                    )
                                } else {
                                    match fusion::foreground_pid_checked(row.pane_pid) {
                                        None => Some(OBSERVATION_HOLD.to_string()),
                                        Some(pane_pid) => {
                                            gate_line(
                                                inner,
                                                handle,
                                                "proceed",
                                                Some(&det.decided_by),
                                                None,
                                            );
                                            return GateOutcome::Proceed {
                                                manifest_id,
                                                pane_pid,
                                                regate_evidence_changed,
                                            };
                                        }
                                    }
                                }
                            }
                            // Human typing always wins. A notification has
                            // reached a conclusive pre-write refusal: publish
                            // it durably now, rather than waiting in memory
                            // for a turn that may never occur (for example a
                            // local slash command).
                            AgentState::IdleWithInput if handle.notification.is_some() => {
                                let Some(mut observation) = composer_semantic_observation(
                                    inner,
                                    handle,
                                    &row,
                                    &manifest_id,
                                ) else {
                                    return GateOutcome::BlockedPreWrite {
                                        cause: NotificationPreWriteCause::BindingUnprovable,
                                        observation: Box::new(binding_unprovable_observation(
                                            inner,
                                            handle,
                                            row.pane_pid,
                                            &manifest_id,
                                        )),
                                    };
                                };
                                observation.write_block = Some("composer_hold".to_string());
                                return GateOutcome::BlockedPreWrite {
                                    cause: NotificationPreWriteCause::WriteReadinessChanged,
                                    observation: Box::new(observation),
                                };
                            }
                            AgentState::IdleWithInput => Some("idle_with_input".to_string()),
                            AgentState::Unknown => Some("unknown".to_string()),
                        }
                    }
                }
            }
        };
        // Only unbroken ambiguity may settle: any other verdict in between
        // restarts the window from zero.
        if hold.as_deref() != Some(AMBIGUOUS_COMPOSER_HOLD) {
            ambiguous_since = None;
        }
        if let Some(cause) = hold {
            handle.set_hold(Some(normalize_hold_cause(&cause)));
            if last_hold.as_deref() != Some(cause.as_str()) {
                gate_line(
                    inner,
                    handle,
                    gate_hold_action(handle, &cause),
                    None,
                    Some(&cause),
                );
                last_hold = Some(cause.clone());
            }
            let since = *hold_since.get_or_insert_with(Instant::now);
            let notify_at = since + Duration::from_millis(inner.cfg.gate_hold_notify_ms);
            // A hold caused by a failed OBSERVATION has no edge coming to
            // release it. Every other cause is a fact about the pane, and
            // the pane announces when that changes; "we could not read the
            // process table" announces nothing, and a transient failure
            // would otherwise wedge the delivery for good. So that one
            // cause, and only that one, also wakes on a bounded retry.
            // The re-evaluation is the full gate: a fresh binding and
            // fresh clean-composer proof, never a shortcut back to
            // proceed.
            // The same doubt reaches the gate two ways: this gate's own
            // foreground check, and a stamped verdict that already
            // refused for it. Both are an observation that did not
            // answer, and neither produces a pane event to wake on.
            let unprovable =
                cause == OBSERVATION_HOLD || cause == format!("not_write_ready:{OBSERVATION_HOLD}");
            // The ambiguous-composer hold gets the same treatment for the
            // same reason: unchanged ambiguity emits no pane event, so the
            // settle boundary needs its own wake to become the durable
            // block rather than an indefinite in-memory wait.
            let retry_at = if unprovable {
                Some(Instant::now() + OBSERVATION_RETRY)
            } else if cause == AMBIGUOUS_COMPOSER_HOLD {
                ambiguous_since.map(|since| since + ambiguous_settle)
            } else if cause == "barrier_held" {
                Some(Instant::now() + Duration::from_millis(50))
            } else {
                None
            };
            let exact_evidence = tokio::select! {
                changed = wait_pane_change(
                    &mut ev_rx,
                    pane_rx.as_mut(),
                    handle.session_idx,
                    &handle.pane_id,
                    &handle.cancel,
                ) => changed,
                _ = async {
                    match retry_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => false,
                _ = tokio::time::sleep_until(notify_at), if !hold_notified => {
                    // A wedged hold must at least be visible. One ping per
                    // delivery; the hold itself keeps waiting on events.
                    hold_notified = true;
                    let (kind, about) = if handle.notification.is_some() {
                        ("notification", About::pane(&handle.pane_id))
                    } else {
                        ("delivery", About::delivery(&handle.to))
                    };
                    admin_notify(
                        inner,
                        NotifyLevel::ActionRequired,
                        &format!("{kind} to {} held in gating", handle.to),
                        &format!(
                            "message {} has been held for over {}ms ({cause})",
                            handle.msg_id, inner.cfg.gate_hold_notify_ms
                        ),
                        Some(&handle.msg_id),
                        Some(handle.session_idx),
                        about,
                    );
                    false
                }
            };
            regate_evidence_changed |= exact_evidence;
        }
    }
}

/// Keep receipt vocabulary stable and independent of vendor manifest rule
/// ids. Ledger gate lines retain the exact cause for diagnostics; receipts
/// expose only these normalized tokens.
fn normalize_hold_cause(cause: &str) -> &'static str {
    match cause {
        "session_detached" => "session_detached",
        "pane_in_mode" => "pane_in_mode",
        "working" => "working",
        "idle_with_input" => "idle_with_input",
        "held_for_existing_draft" => "held_for_existing_draft",
        "blocked_quota" => "blocked_quota",
        "unknown" => "unknown",
        c if c.split(':').next() == Some("blocked") => "blocked",
        // Runtime state is idle, but nothing proved the composer
        // was clean. Receipts say so plainly; the exact reason stays on
        // the gate ledger line.
        c if c.split(':').next() == Some("not_write_ready") => "not_write_ready",
        _ => "unknown",
    }
}

/// Manifest decline keys, in order, with spacing. The keys come from the
/// manifest rule, never a generic Enter/Escape.
///
/// TOCTOU guard: before the FINAL confirming key of a multi-key sequence
/// the screen is re-captured, and the same modal rule must still be the
/// winning match. A dialog that vanished or changed between keys (the
/// human answered it, the app redrew) must not receive the confirm.
/// Returns false when the sequence was aborted.
async fn send_decline_keys(
    watcher: &Arc<SessionWatcher>,
    pane_id: &str,
    manifest: &Manifest,
    rule_id: &str,
    keys: &[String],
) -> bool {
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(DECLINE_SPACING).await;
        }
        if i > 0 && i == keys.len() - 1 {
            let title = watcher.pane(pane_id).map(|r| r.title).unwrap_or_default();
            let screen = match watcher.client().capture_pane(pane_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(pane = pane_id, error = %e, "decline recheck capture failed");
                    return false;
                }
            };
            if !modal_still_matches(manifest, &title, &screen, rule_id) {
                return false;
            }
        }
        if let Err(e) = watcher.client().send_keys(pane_id, &[key.as_str()]).await {
            warn!(pane = pane_id, error = %e, "decline key failed");
            return true; // sent what we could; not a TOCTOU abort
        }
    }
    true
}

/// True while `rule_id` is still the winning match for this screen.
fn modal_still_matches(manifest: &Manifest, title: &str, screen: &str, rule_id: &str) -> bool {
    manifest
        .evaluate(title, screen)
        .is_some_and(|r| r.id == rule_id && r.state.is_blocked())
}

/// Parse the quota reset hint from the screen. Only the parsed phrase ever
/// leaves this function; raw captures stay out of the ledger.
async fn quota_hint(watcher: &Arc<SessionWatcher>, pane_id: &str) -> Option<String> {
    let screen = watcher.client().capture_pane(pane_id).await.ok()?;
    parse_reset_hint(&screen)
}

/// Mark a pane as holding text, without waiting for a sensor to see it.
///
/// Used after our own paste lands. A hold set here releases through the
/// same turn lifecycle as one a sensor raised: nothing about it is
/// special except that the evidence came from having done the write.
fn latch_hold(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) -> Result<(), String> {
    // A claim, not a set. The cached verdict and the readiness wake move
    // with it, so a pane whose composer holds our payload stops reporting
    // itself writable to anyone who asks between now and the next
    // recompute; and the claim names THIS attempt, so evidence arriving
    // late for an earlier delivery cannot settle this barrier.
    let owner = handle.barrier_owner();
    if fusion::claim_hold(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &owner,
        Some(proven.agent),
        Some(proven.manifest.as_str()),
    ) {
        handle.state.lock().expect("handle state lock").barrier = Some(owner);
        Ok(())
    } else {
        Err("barrier_held".to_string())
    }
}

fn rollback_unwritten_hold(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    proven: &fusion::Binding,
) {
    let owner = handle
        .state
        .lock()
        .expect("handle state lock")
        .barrier
        .clone();
    let Some(owner) = owner else {
        return;
    };
    if fusion::release_unwritten_hold(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &owner,
        proven.agent,
        &proven.manifest,
    ) {
        handle.state.lock().expect("handle state lock").barrier = None;
    }
}

/// Correct the provisional boundary after tmux proves that its command pipe
/// accepted no byte of the paste command.
///
/// The durable correction must succeed before the runtime boundary is cleared.
/// Otherwise restart recovery must continue treating the attempt as post-write.
fn correct_proven_unwritten_paste(handle: &DeliveryHandle) -> Result<(), NotificationAdapterError> {
    if let Some(notification) = &handle.notification {
        notification.record_paste_command_unwritten()?;
    }
    handle.write_boundary_crossed.store(false, Ordering::SeqCst);
    Ok(())
}

/// Releases a claimed composer barrier if the synchronous boundary hook unwinds.
struct UnwrittenHold<'a> {
    inner: &'a Arc<Inner>,
    handle: &'a Arc<DeliveryHandle>,
    binding: &'a fusion::Binding,
    armed: bool,
}

impl<'a> UnwrittenHold<'a> {
    fn new(
        inner: &'a Arc<Inner>,
        handle: &'a Arc<DeliveryHandle>,
        binding: &'a fusion::Binding,
    ) -> Self {
        Self {
            inner,
            handle,
            binding,
            armed: true,
        }
    }

    fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for UnwrittenHold<'_> {
    fn drop(&mut self) {
        if self.armed {
            rollback_unwritten_hold(self.inner, self.handle, self.binding);
        }
    }
}

fn notification_write_cause(error: NotificationAdapterError) -> String {
    match error {
        NotificationAdapterError::NoLongerCurrentBeforeWrite => {
            NO_LONGER_CURRENT_BEFORE_WRITE.to_string()
        }
        other => {
            error!(error = %other, "notification write fact failed");
            NOTIFICATION_RECORD_FAILED.to_string()
        }
    }
}

/// Record a receipt before the legacy delivery state claims it.
///
/// False means the notification already resolved the other way in a race.
fn record_notification_notified(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
) -> Result<bool, NotificationAdapterError> {
    let Some(notification) = &handle.notification else {
        return Ok(true);
    };
    match notification.record_notified() {
        Ok(record) => {
            if handle.notification_transport() == Some(NotificationTransport::DirectPayload) {
                notification.record_delivered_direct()?;
                let recipient = notification.recipient();
                if let Some(messaging) = inner.workspace_messaging() {
                    if let Err(error) = messaging.direct_delivery_settled(recipient) {
                        error!(
                            id = %handle.msg_id,
                            %recipient,
                            %error,
                            "direct delivery settled but the next mailbox item could not be scheduled"
                        );
                    }
                } else {
                    error!(
                        id = %handle.msg_id,
                        %recipient,
                        "direct delivery settled without workspace messaging"
                    );
                }
            } else {
                if let Some(messaging) = inner.workspace_messaging() {
                    messaging.notification_became_notified(record);
                }
            }
            Ok(true)
        }
        Err(NotificationAdapterError::TerminalConflict(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// What a receipt says about the turn that consumed our payload.
///
/// A hook acknowledgement can name both the exact payload and a TurnKey.
/// That key binds the hold to one turn, which releases only when the same
/// key reports an end and the screen is clean. Arrival timestamps never
/// substitute for that match.
///
/// A hook acknowledgement without a TurnKey, or a screen receipt, proves
/// consumption but selects the screen lifecycle. Its timestamp is retained
/// for diagnosis only.
fn settle_hold_on_receipt(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    verified_by: Option<VerifiedBy>,
    turn_edge_ms: Option<u64>,
    turn: Option<crate::turnkey::TurnKey>,
) {
    // A receipt only ever settles ITS OWN barrier: see `set_hold_owned`.
    // A delivery that never claimed one has none to settle. Its receipt
    // still resolves the delivery; it just says nothing about whatever
    // this pane's composer is holding for somebody else.
    let Some(owner) = handle
        .state
        .lock()
        .expect("handle state lock")
        .barrier
        .clone()
    else {
        return;
    };
    if verified_by == Some(VerifiedBy::Hook) {
        let since_ms = turn_edge_ms.unwrap_or_else(unix_ms);
        match turn {
            // The vendor named the turn that took this payload, so the
            // hold binds to it and joins the exact lifecycle: only that
            // turn's own end can end it.
            Some(turn) => {
                let _ = fusion::bind_turn(
                    inner,
                    handle.session_idx,
                    &handle.pane_id,
                    &owner,
                    turn,
                    since_ms,
                );
            }
            // A vendor that names no turns still acknowledges. The hold
            // stays on the screen lifecycle and carries the observed edge
            // only for diagnosis. Only a hold still waiting takes it.
            None => {
                fusion::set_hold_owned(
                    inner,
                    handle.session_idx,
                    &handle.pane_id,
                    &owner,
                    |hold| {
                        hold.is_waiting()
                            .then_some(cyclops_proto::ComposerHold::TurnStarted { since_ms })
                    },
                );
            }
        }
        return;
    }
    // A screen receipt names no turn, so it promotes this hold to the
    // screen lane. Consumption is proven, and the submit time is retained
    // for diagnosis. Reading the lane from the manifest instead would
    // leave a keyed vendor whose
    // hook was never installed holding a barrier forever, waiting on an
    // exact end that nobody is going to send. A matching ACK arriving
    // late can still upgrade this same owner to the exact lane.
    latch_turn_started(
        inner,
        handle.session_idx,
        &handle.pane_id,
        &owner,
        handle.submitted_at_ms.load(Ordering::SeqCst),
    );
}

/// Reconcile an authenticated claim with a doorbell whose submit succeeded.
///
/// A claim while the delivery is still staged proves only mailbox retrieval.
/// It wakes the worker so that worker can reserve submit or clear the exact
/// staged bytes. It never creates turn evidence.
pub(crate) fn settle_notification_claim(
    inner: &Arc<Inner>,
    attempt_id: NotificationAttemptId,
) -> bool {
    let Some(handle) = inner.engine.notification_handle(attempt_id) else {
        return false;
    };
    if handle.notification_transport() != Some(NotificationTransport::Doorbell) {
        return false;
    }
    let settled = advance(
        inner,
        &handle,
        &[DeliveryState::Submitted],
        Step::to(DeliveryState::DeliveredUnverified).cause("mailbox_claim"),
    );
    if settled
        || matches!(
            handle.state(),
            DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
        )
    {
        fusion::clear_hold_owner(
            inner,
            handle.session_idx,
            &handle.pane_id,
            &handle.barrier_owner(),
        );
        handle.ack.notify_one();
        return true;
    }
    if handle.state() == DeliveryState::Staged {
        handle.cancel.notify_one();
        return true;
    }
    false
}

fn latch_turn_started(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    since_ms: u64,
) -> bool {
    // An existing mark is never replaced. It records the first observed
    // turn or the submit boundary for the screen lifecycle; it does not
    // correlate a turn end. Only a manifest-declared TurnKey can do that.
    // `StagedDuringTurn` counts as waiting: a turn already running
    // cannot consume a person's draft, but this payload is one Cyclops
    // wrote and submitted itself, and the receipt names the turn that
    // took it.
    fusion::set_hold_owned(inner, session_idx, pane_id, owner, |hold| {
        hold.is_waiting()
            .then_some(cyclops_proto::ComposerHold::TurnStarted { since_ms })
    })
}

/// Block until an event that could change the gate verdict for this pane:
/// a fused state change, a session attach/detach, or a pane field change
/// (mode, death, title, command). Lag counts as doubt and wakes too.
async fn wait_pane_change(
    ev_rx: &mut broadcast::Receiver<Event>,
    pane_rx: Option<&mut broadcast::Receiver<PaneEvent>>,
    session_idx: usize,
    pane_id: &str,
    cancel: &Notify,
) -> bool {
    match pane_rx {
        Some(prx) => {
            let mut event_open = true;
            let mut pane_open = true;
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if event_open => match ev {
                        Ok(event) => if let Some(exact) = event_wake(&event, session_idx, pane_id) {
                            return exact;
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => return false,
                        Err(broadcast::error::RecvError::Closed) => event_open = false,
                    },
                    pe = prx.recv(), if pane_open => match pe {
                        Ok(event) => if let Some(exact) = pane_event_wake(&event, pane_id) {
                            return exact;
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => return false,
                        Err(broadcast::error::RecvError::Closed) => pane_open = false,
                    },
                    _ = cancel.notified() => return false,
                }
            }
        }
        None => {
            let mut event_open = true;
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if event_open => match ev {
                        Ok(event) => if let Some(exact) = event_wake(&event, session_idx, pane_id) {
                            return exact;
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => return false,
                        Err(broadcast::error::RecvError::Closed) => event_open = false,
                    },
                    _ = cancel.notified() => return false,
                }
            }
        }
    }
}

fn event_wake(event: &Event, session_idx: usize, pane_id: &str) -> Option<bool> {
    match event.event.as_str() {
        // A readiness change with no state change is exactly the
        // shape of a hold lifting, and it is the whole reason this
        // arm exists: without it a delivery sleeps through its own
        // release.
        "state" | "readiness" => (event.data["pane_id"] == pane_id
            && event.data["session_idx"] == session_idx)
            .then_some(true),
        "session" => Some(false),
        _ => None,
    }
}

fn pane_event_wake(event: &PaneEvent, pane_id: &str) -> Option<bool> {
    match event {
        PaneEvent::PaneChanged { id, .. } => (id == pane_id).then_some(true),
        PaneEvent::PaneRemoved(id) => (id == pane_id).then_some(true),
        PaneEvent::Disconnected => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Inject and verify
// ---------------------------------------------------------------------------

/// How payload bytes reach an agent and how the backend reads them back.
/// The gate, verify, and acknowledgment layers call through this seam only,
/// so a headless protocol
/// backend slots in per agent without touching them. [`TmuxInjector`] is
/// the terminal implementation. Errors are the short cause codes retry
/// accounting records.
pub(crate) trait Injector {
    /// Put the payload somewhere the pane can take it, WITHOUT the pane
    /// taking it.
    ///
    /// Separate from `commit` because spooling costs a control round
    /// trip, and any round trip is time a person can type in. Done here,
    /// that time is before the final proof rather than after it: the
    /// capture that admits the write is then the last thing to happen
    /// before the write. Spooling touches no pane and is freely
    /// retryable.
    async fn spool(&self, payload: &str) -> Result<(), String>;

    /// Hand the spooled payload to the pane's composer, without
    /// submitting.
    ///
    /// `on_write` runs immediately before the pane is asked to take it,
    /// which arms the conservative write boundary: everything before it is
    /// provably retryable, and everything from it onward may have left text
    /// in somebody's composer. Only a transport result proving that the
    /// command pipe accepted zero bytes can correct that boundary.
    ///
    /// It can FAIL, and then nothing is written. The barrier it installs
    /// is what stops the next delivery pasting over this one, so a paste
    /// that went ahead without it would create exactly the state the
    /// barrier exists to prevent, with nothing recording that it is
    /// there.
    async fn commit(
        &self,
        pane_id: &str,
        on_write: &(dyn Fn() -> Result<(), String> + Sync),
    ) -> Result<(), InjectFailure>;

    /// Drop a spooled payload the attempt is not going to write.
    async fn discard(&self);
    /// Press the submit key.
    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String>;
    /// Send one measured whole-composer clear sequence.
    async fn clear(&self, pane_id: &str, keys: &[String]) -> Result<(), String> {
        for key in keys {
            self.submit(pane_id, key).await?;
        }
        Ok(())
    }
    /// Read one escaped snapshot with tmux physical wraps joined. Exact
    /// composer extraction compares these logical rows with the bytes that
    /// were spooled.
    async fn capture_joined_escaped(&self, pane_id: &str) -> Result<String, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InjectFailure {
    /// The tmux command pipe accepted no byte of the paste command.
    PasteCommandUnwritten,
    /// Every other refusal or ambiguous outcome keeps its existing cause.
    Other(String),
}

impl From<String> for InjectFailure {
    fn from(cause: String) -> Self {
        Self::Other(cause)
    }
}

fn classify_paste_buffer_failure(error: TmuxError) -> InjectFailure {
    match error {
        TmuxError::Io(_) => InjectFailure::PasteCommandUnwritten,
        _ => InjectFailure::Other("paste_failed".to_string()),
    }
}

/// The tmux paste path: load-buffer through the adapter's private spool
/// (0600 file under the 0700 cyclops home, never the shared temp dir) into
/// a per-delivery unique buffer, paste-buffer -p (bracketed when the app
/// opted in), and -d so the buffer does not linger server-global. Submit uses
/// send-keys.
struct TmuxInjector {
    client: Arc<ControlClient>,
    /// Per-delivery unique buffer name.
    buffer: String,
}

impl Injector for TmuxInjector {
    async fn spool(&self, payload: &str) -> Result<(), String> {
        if let Err(e) = self
            .client
            .load_buffer(&self.buffer, payload.as_bytes())
            .await
        {
            warn!(buffer = %self.buffer, error = %e, "load-buffer failed");
            // Loading the private spool buffer happens before tmux is asked
            // to write to the pane, regardless of how the load command
            // failed. It is therefore safe to retry under the bounded
            // pre-write budget. A paste-buffer failure stays ambiguous unless
            // the command pipe proves that it accepted zero command bytes.
            return Err("spool_failed".to_string());
        }
        Ok(())
    }

    async fn discard(&self) {
        let _ = self.client.delete_buffer(&self.buffer).await;
    }

    async fn commit(
        &self,
        pane_id: &str,
        on_write: &(dyn Fn() -> Result<(), String> + Sync),
    ) -> Result<(), InjectFailure> {
        // The write boundary. Spooling is behind us and provably touched
        // no pane; the next call may put text in somebody's composer. Every
        // outcome except an exact zero-byte command failure is ambiguous
        // about whether it did. Whatever this hook installs has to be
        // installed BEFORE the await, not after
        // it returns, or an outcome that leaves a payload behind can be
        // acted on by another delivery first.
        //
        // A hook that cannot install it stops the write. Nothing has been
        // pasted at this point, so refusing is the cheap direction: the
        // buffer is dropped and the delivery retries under the pre-write
        // budget.
        if let Err(cause) = on_write() {
            let _ = self.client.delete_buffer(&self.buffer).await;
            return Err(InjectFailure::Other(cause));
        }
        if let Err(e) = self
            .client
            .paste_buffer(&self.buffer, pane_id, true, true)
            .await
        {
            warn!(buffer = %self.buffer, error = %e, "paste-buffer failed");
            // paste-buffer -d never ran, so the loaded buffer would linger
            // server-global with the payload in it. Best effort: the buffer
            // dies with the server either way.
            let _ = self.client.delete_buffer(&self.buffer).await;
            return Err(classify_paste_buffer_failure(e));
        }
        Ok(())
    }

    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String> {
        self.client.send_keys(pane_id, &[key]).await.map_err(|e| {
            warn!(error = %e, "submit key failed");
            "submit_failed".to_string()
        })
    }

    async fn clear(&self, pane_id: &str, keys: &[String]) -> Result<(), String> {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.client.send_keys(pane_id, &keys).await.map_err(|e| {
            warn!(error = %e, "composer clear failed");
            "claim_clear_failed".to_string()
        })
    }

    async fn capture_joined_escaped(&self, pane_id: &str) -> Result<String, String> {
        self.client
            .capture_pane_joined_escaped(pane_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The expected staged target to verify in the active composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StagingTarget<'a> {
    /// Multi-line payload ending in terminal sentinel `[cyclops:end <msg_id>]`.
    Sentinel(&'a str),
    /// Single-line payload matching exact expected terminal composer row.
    #[cfg_attr(not(test), allow(dead_code))]
    ExactRow(&'a str),
}

/// Result of extracting one active composer's visible payload.
///
/// `Hidden` is distinct from `Unprovable`: a collapsed chip is positive
/// evidence that bytes exist, but the screen cannot reveal which bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum ComposerContentProof {
    Visible(String),
    Hidden,
    Unsupported,
    Unprovable,
}

/// Paste the payload and verify the composer staged it. Returns the
/// composer-window snapshot the screen ACK tier compares against, plus
/// whether an id-carrying pattern proved the staging (feeds the tier-2
/// evidence rules).
///
/// Composer verification is the gate because bracketed-paste
/// degradation is not observable up front through tmux 3.6a.
async fn inject<I: Injector>(
    injector: &I,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    target: StagingTarget<'_>,
    expected_payload: &str,
    on_write: &(dyn Fn() -> Result<(), String> + Sync),
) -> Result<(String, bool, String), InjectFailure> {
    injector.commit(&handle.pane_id, on_write).await?;
    // The capture flavor follows the manifest's composer discriminators:
    // esc rules need the SGR-escaped grid or they fail closed, and a
    // composer that collapses a long paste into a chip hides the bytes.
    // That representation can identify a staged-input condition, but it
    // cannot prove exact Cyclops ownership or authorize Enter.
    let mut last_delay = 0;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        let capture = injector.capture_joined_escaped(&handle.pane_id).await;
        match capture {
            Ok(screen) => {
                if let Some((id_staged, payload_proof)) =
                    exact_staging_proof(manifest, &screen, target, expected_payload)
                {
                    // The comparison window is de-escaped text either way,
                    // so SGR churn (a blink, a focus change) can never fake
                    // a "changed composer" for the ACK tier.
                    return Ok((
                        bottom_window(&strip_csi(&screen), COMPOSER_WINDOW),
                        id_staged,
                        payload_proof,
                    ));
                }
            }
            Err(e) => debug!(error = %e, "verify capture failed"),
        }
    }
    if handle.notification.is_some() {
        let capture = injector
            .capture_joined_escaped(&handle.pane_id)
            .await
            .unwrap_or_default();
        return Ok((
            bottom_window(&strip_csi(&capture), COMPOSER_WINDOW),
            false,
            String::new(),
        ));
    }
    Err(InjectFailure::Other("verify_failed".to_string()))
}

/// What representation is visible in the active composer.
///
/// A visible target is still only structural evidence until the extracted
/// composer bytes match the expected payload. A collapsed chip proves only
/// that the vendor drew a chip. It cannot prove ownership or authorize Enter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StagedRepresentation {
    VisibleTarget,
    CollapsedChip,
}

fn staged_representation(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
) -> Option<StagedRepresentation> {
    match target {
        StagingTarget::Sentinel(msg_id) => {
            if sentinel_verified(manifest, screen, msg_id) {
                return Some(StagedRepresentation::VisibleTarget);
            }
            if marker_in_composer(manifest, screen) {
                return Some(StagedRepresentation::CollapsedChip);
            }
            None
        }
        StagingTarget::ExactRow(expected_row) => {
            if exact_row_verified(manifest, screen, expected_row) {
                return Some(StagedRepresentation::VisibleTarget);
            }
            match exact_composer_content_from_joined_capture(manifest, screen) {
                ComposerContentProof::Visible(content)
                    if content.contains('\n')
                        && visible_single_line_payload_matches(&content, expected_row) =>
                {
                    Some(StagedRepresentation::VisibleTarget)
                }
                ComposerContentProof::Hidden => Some(StagedRepresentation::CollapsedChip),
                ComposerContentProof::Visible(_)
                | ComposerContentProof::Unsupported
                | ComposerContentProof::Unprovable => None,
            }
        }
    }
}

#[cfg(test)]
fn sentinel_representation(
    manifest: &Manifest,
    screen: &str,
    _id_patterns: &[String],
    _other_patterns: &[String],
    msg_id: &str,
) -> Option<StagedRepresentation> {
    staged_representation(manifest, screen, StagingTarget::Sentinel(msg_id))
}

/// Is this delivery's sentinel the last payload row of the ACTIVE composer?
///
/// The question is structural, not a vocabulary lookup. Below the composer
/// every supported vendor paints a fixed sequence of rows: a box rule, a
/// model status row, sometimes a hint or a mode row. That measured layout
/// is `injection.composer_trailer_regex` (plain) and
/// `composer_trailer_regex_esc` (the same rows in the escaped capture),
/// entry i describing row i.
///
/// Verification proves, in order:
///
/// 1. The layout is measured for this vendor, and an escaped capture is in
///    hand. Neither is inferable, so both missing means refuse.
/// 2. The exact sentinel for this delivery appears as a whole row, and
///    exactly once. The comparison keeps left-side bytes: a row reading
///    " [cyclops:end <id>]" carries a leading space that the composer put
///    there, and two identical sentinels are an ambiguity about which one
///    transport owns rather than a reason to prefer the lower.
/// 3. At least one row follows it. Every measured vendor paints chrome
///    below the composer, so a capture that ends at the sentinel is a
///    capture that did not see the composer.
/// 4. The layout's REQUIRED prefix appears first, in order, immediately
///    below the sentinel, and anything after it is a later declared row,
///    still in order and never more rows than the layout has. Requiring
///    the anchors is what stops an arbitrary plausible tail from passing
///    with the box rule and status row simply absent, and order plus
///    cardinality is what binds the sentinel to the ACTIVE composer: a
///    sentinel left in the transcript has composer rows between it and
///    the chrome, and those claim no layout entry.
/// 5. Each following row matches its layout entry in BOTH forms. The
///    escaped form is the discriminator plain text cannot carry: on every
///    layout measured so far the vendor paints its chrome while a pasted
///    payload row arrives unstyled, so prose shaped like a status row fails
///    the escaped half. That is a property of those measured layouts rather
///    than of terminals in general.
///
/// Anything unproven refuses. A truncated or wrapped sentinel matches no
/// row; an unknown row ends the walk; a missing style ends it too.
fn sentinel_verified(manifest: &Manifest, screen: &str, msg_id: &str) -> bool {
    sentinel_proof(manifest, screen, msg_id).is_some()
}

/// The proven staged rows, when the sentinel path validates: every visible
/// row through the unique exact sentinel, and nothing after it.
///
/// The boundary comes from the proof rather than from pattern matching,
/// which matters because a payload row can read exactly like chrome. If
/// rows were dropped merely for looking like a status row, a human could
/// edit one and the comparison would never see it.
/// Remove the terminal's own right padding, and nothing else.
///
/// `capture-pane` pads every row out to the pane width with ASCII spaces,
/// and that padding is the one trailing thing on a row that is not
/// composer content. Rust's `trim_end` removes tabs, non-breaking spaces
/// and every other Unicode space as well, each of which a person can put
/// in a composer, so using it before an exact comparison would let a
/// sentinel followed by a tab read as exact.
fn unpad(row: &str) -> &str {
    row.trim_end_matches(' ')
}

/// The screen as physical rows, in order, with the terminal's own right
/// padding removed and the blank grid below the last content dropped.
///
/// Each row is kept in both forms: raw, and with escape sequences
/// removed. A blank row BETWEEN content survives, because it is composer
/// content and it means whatever sits above it was not the last thing on
/// the screen.
fn composer_rows(screen: &str) -> Vec<(&str, String)> {
    let mut rows: Vec<(&str, String)> = screen
        .lines()
        .map(|raw| (unpad(raw), unpad(&strip_csi(raw)).to_string()))
        .collect();
    while rows.last().is_some_and(|(_, plain)| plain.is_empty()) {
        rows.pop();
    }
    rows
}

/// Rows from `capture-pane -J`, which already omits unused grid cells.
///
/// Unlike the regular capture, `-J` preserves spaces that occupy terminal
/// cells. Those spaces may be composer content, so exact extraction keeps
/// them and drops only empty rows below the visible grid.
fn joined_composer_rows(screen: &str) -> Vec<(&str, String)> {
    let mut rows: Vec<(&str, String)> = screen.lines().map(|raw| (raw, strip_csi(raw))).collect();
    while rows.last().is_some_and(|(raw, _)| raw.is_empty()) {
        rows.pop();
    }
    rows
}

/// Do the vendor's declared trailer rows follow, in order, with nothing
/// else after them?
///
/// This is what makes a staging proof TERMINAL. Finding what was staged
/// says only that it is on the screen somewhere; proving that only the
/// vendor's own chrome follows it is what says the composer holds that
/// and nothing more. Without it, a line a person typed underneath rides
/// along and the submit key sends both.
///
/// Shared by both proofs deliberately. A visible payload and a collapsed
/// one are two ways of recognizing the same staged text, and a second
/// copy of this rule would be a second place for terminality to rot.
fn trailer_follows(manifest: &Manifest, suffix: &[(&str, String)]) -> bool {
    let layout = &manifest.composer_trailers;
    let layout_esc = &manifest.composer_trailers_esc;
    let required = manifest.injection.composer_trailer_required_prefix;
    // Unmeasured layout cannot answer the question.
    if layout.is_empty() || layout_esc.len() != layout.len() {
        return false;
    }
    if required == 0 || required > layout.len() {
        return false;
    }
    // Chrome always follows a real composer, and never more rows than the
    // layout declares.
    if suffix.len() < required || suffix.len() > layout.len() {
        return false;
    }
    let structural_unstyled = suffix.iter().all(|(raw, plain)| *raw == plain)
        && matches!(
            manifest.injection.unstyled_composer_proof,
            Some(cyclops_manifest::UnstyledComposerProof::StructuralTrailer)
        );
    if structural_unstyled {
        let Some((_, first)) = suffix.first() else {
            return false;
        };
        let belongs_to_composer = manifest
            .composer_prompt
            .as_ref()
            .is_some_and(|pattern| captured_content(pattern, first).is_some())
            || manifest
                .composer_continuation
                .as_ref()
                .is_some_and(|pattern| captured_content(pattern, first).is_some());
        if belongs_to_composer {
            return false;
        }
    }
    // Full span on the plain row, generically: a manifest that forgot an
    // anchor would otherwise accept trailing payload on a chrome row, and
    // no vendor should be able to weaken terminality by omission. The
    // escaped half supplies the style evidence, where a partial match is
    // meaningful because SGR runs surround the text.
    let matches = |i: usize, raw: &str, plain: &str| {
        whole_row(&layout[i], plain) && (layout_esc[i].is_match(raw) || structural_unstyled)
    };
    for (i, (raw, plain)) in suffix.iter().enumerate().take(required) {
        if !matches(i, raw, plain) {
            return false;
        }
    }
    // Later declared rows may be absent, but never reordered, and an
    // undeclared row claims nothing and refuses.
    let mut next = required;
    for (raw, plain) in &suffix[required..] {
        let mut claimed = false;
        while next < layout.len() {
            let i = next;
            next += 1;
            if matches(i, raw, plain) {
                claimed = true;
                break;
            }
        }
        if !claimed {
            return false;
        }
    }
    true
}

fn sentinel_proof(manifest: &Manifest, screen: &str, msg_id: &str) -> Option<String> {
    let want = sentinel_for(msg_id);
    let rows = composer_rows(screen);
    // The bounded tail is where the sentinel is looked for, so a token
    // further up the transcript can never be mistaken for the staged one.
    // The PROOF returned below still spans every visible row through it:
    // an edit above the search window is still an edit to the payload.
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];
    // Exactly one exact sentinel for THIS delivery. Two is ambiguity
    // about which one transport owns, and ambiguity fails closed.
    //
    // The token row is matched in the vendor's measured continuation shape,
    // with only the terminal's own trailing padding removed. Some composers
    // prefix every logical continuation row, including the sentinel. The raw
    // row must still equal the plain row so styling cannot hide extra bytes.
    //
    // KNOWN LIMIT, measured on tmux 3.6a: the default capture erases
    // trailing spaces a person typed, exactly as it erases the grid's own
    // padding, so spaces after the token are not distinguishable from
    // padding by any capture this takes. Every other trailing code point
    // is content and refuses. Closing the space case needs the composer
    // endpoint observed independently, bound to this same snapshot.
    let raw_hits: Vec<usize> = window
        .iter()
        .enumerate()
        .filter(|(_, (raw, plain))| {
            if raw != plain {
                return false;
            }
            *plain == want
                || manifest
                    .composer_continuation
                    .as_ref()
                    .is_some_and(|continuation| {
                        captured_continuation_content(manifest, continuation, plain)
                            == Some(want.as_str())
                    })
        })
        .map(|(i, _)| i)
        .collect();
    let hits: Vec<usize> = raw_hits
        .iter()
        .copied()
        .filter(|at| trailer_follows(manifest, &window[at + 1..]))
        .collect();
    let [at] = hits[..] else {
        return None;
    };
    if let Some(prompt) = manifest.composer_prompt.as_ref() {
        let want_header = format!("[cyclops {msg_id}] FROM:");
        let prompt_at = window[..=at]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, (_, plain))| {
                captured_content(prompt, plain)
                    .is_some_and(|content| content.starts_with(&want_header))
                    .then_some(index)
            });
        if let Some(prompt_at) = prompt_at {
            if raw_hits
                .iter()
                .filter(|hit| **hit >= prompt_at && **hit <= at)
                .count()
                != 1
            {
                return None;
            }
        } else if raw_hits.len() != 1 {
            return None;
        }
    } else if raw_hits.len() != 1 {
        return None;
    }
    Some(
        rows[..=start + at]
            .iter()
            .map(|(_, plain)| plain.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// The proven staged row, when an exact single-line composer row validates:
/// the unique terminal composer row matching the expected text (with optional
/// vendor prompt prefix verified against the manifest), followed directly by
/// the vendor's declared composer trailer chrome.
pub(crate) fn exact_row_proof(
    manifest: &Manifest,
    screen: &str,
    expected_row: &str,
) -> Option<String> {
    let want = unpad(expected_row);
    if want.is_empty() {
        return None;
    }
    let rows = composer_rows(screen);
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];

    let idle_rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|rule| rule.state == AgentState::IdleWithInput)
        .collect();
    let structural_unstyled = manifest.injection.unstyled_composer_proof
        == Some(UnstyledComposerProof::StructuralTrailer)
        && !screen.contains('\u{1b}');

    let hits: Vec<usize> = window
        .iter()
        .enumerate()
        .filter(|(_, (raw, plain))| {
            let p = unpad(plain);
            let r = unpad(raw);
            if idle_rules.is_empty() {
                return p == want || r == want;
            }
            if structural_unstyled
                && raw == plain
                && manifest
                    .composer_prompt
                    .as_ref()
                    .and_then(|prompt| captured_content(prompt, plain))
                    == Some(want)
            {
                return true;
            }
            if let Some(prefix) = p.strip_suffix(want) {
                let is_prompt_prefix = prefix
                    .chars()
                    .all(|c| !c.is_alphanumeric() && !c.is_control());
                if is_prompt_prefix && idle_rules.iter().any(|rule| rule.matches_row(plain, raw)) {
                    return true;
                }
            }
            if let Some(prefix) = r.strip_suffix(want) {
                let is_prompt_prefix = prefix
                    .chars()
                    .all(|c| !c.is_alphanumeric() && !c.is_control());
                if is_prompt_prefix && idle_rules.iter().any(|rule| rule.matches_row(plain, raw)) {
                    return true;
                }
            }
            false
        })
        .map(|(i, _)| i)
        .filter(|at| trailer_follows(manifest, &window[at + 1..]))
        .collect();

    let [at] = hits[..] else {
        return None;
    };

    if window[..at]
        .iter()
        .any(|(raw, plain)| idle_rules.iter().any(|rule| rule.matches_row(plain, raw)))
    {
        return None;
    }

    Some(window[at].1.trim().to_string())
}

pub(crate) fn exact_row_verified(manifest: &Manifest, screen: &str, expected_row: &str) -> bool {
    exact_row_proof(manifest, screen, expected_row).is_some()
}

/// Substituted staging patterns, split into id-carrying (contain the
/// message id after substitution) and generic. The id is always an
/// id-carrying pattern.
#[cfg(test)]
fn verify_patterns(manifest: &Manifest, msg_id: &str) -> (Vec<String>, Vec<String>) {
    let mut id = Vec::new();
    let mut other = Vec::new();
    for p in &manifest.injection.verify_pattern {
        if p.contains("<message_id>") {
            id.push(p.replace("<message_id>", msg_id));
        } else {
            other.push(p.clone());
        }
    }
    if id.is_empty() {
        id.push(msg_id.to_string());
    }
    (id, other)
}

/// Last `n` non-empty lines of a capture, top-down, joined.
fn bottom_window(screen: &str, n: usize) -> String {
    let mut lines: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(n)
        .collect();
    lines.reverse();
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// ACK tiers
// ---------------------------------------------------------------------------

enum AckOutcome {
    /// The matcher resolved it (delivered_verified is on the handle).
    Resolved,
    /// The pane changed hands after Enter, so no later evidence belongs to
    /// this delivery.
    Rebound,
    /// Screen evidence: marker left the composer and the pane moved.
    Screen,
    /// Neither tier inside the deadline.
    Timeout,
}

/// Outcome of one tier-2 evidence pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    /// The conjunctive rule held: marker gone plus turn evidence.
    Confirmed,
    /// The pane was observed and the evidence is not there (yet).
    Absent,
    /// Nobody looked: the watcher is gone (a detach can clear it before
    /// the lifecycle event is broadcast) or the capture failed. Doubt,
    /// never expiry, mirroring fusion's capture-failure handling.
    Unobservable,
    /// The pane changed hands after the submit key. Whatever is on screen
    /// now belongs to somebody else, so it can neither confirm nor deny
    /// this delivery, and waiting longer cannot fix that.
    Rebound,
}

/// What one checkpoint pass means for the ACK loop. Expiry may stand only
/// on a pass that actually looked and saw nothing; doubt freezes the clock
/// until observability returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointStep {
    Deliver,
    Rebound,
    Freeze,
    Expire,
    Wait,
}

fn checkpoint_step(evidence: Evidence, expired: bool) -> CheckpointStep {
    match evidence {
        Evidence::Confirmed => CheckpointStep::Deliver,
        // Its own outcome, deliberately: folding it into expiry would
        // record an ack timeout for a pane that changed hands, which is a
        // different fact and a worse one to leave in the ledger.
        Evidence::Rebound => CheckpointStep::Rebound,
        Evidence::Unobservable => CheckpointStep::Freeze,
        Evidence::Absent if expired => CheckpointStep::Expire,
        Evidence::Absent => CheckpointStep::Wait,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptRefresh {
    Observe,
    Resolved,
    Freeze,
    Rebound,
}

/// Classify the stable fusion refresh that precedes every receipt check.
///
/// A missing watcher is a detach and freezes time. A live watcher that no
/// longer has the submitted pane proves a rebound. Once a pane is observed,
/// only stale screen evidence, pane mode, or an unprovable occupant freezes
/// the clock. Other refusals remain observable facts and must not stop the
/// receipt ladder.
fn receipt_refresh_step(
    watcher_live: bool,
    detection: Option<&Detection>,
    resolved: bool,
) -> ReceiptRefresh {
    if resolved {
        return ReceiptRefresh::Resolved;
    }
    if !watcher_live {
        return ReceiptRefresh::Freeze;
    }
    let Some(detection) = detection else {
        return ReceiptRefresh::Rebound;
    };
    if detection.state == AgentState::Dead {
        return ReceiptRefresh::Rebound;
    }
    if detection.stale
        || matches!(
            detection.write_block.as_deref(),
            Some("pane_in_mode" | "occupant_unprovable")
        )
    {
        ReceiptRefresh::Freeze
    } else {
        ReceiptRefresh::Observe
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptStep {
    Resolved,
    Deliver,
    Rebound,
    Freeze,
    Expire,
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptPaneStep {
    Ignore,
    Recheck,
    Rebound,
    Freeze,
}

fn receipt_pane_step(
    event: &Result<PaneEvent, broadcast::error::RecvError>,
    pane_id: &str,
    frozen: bool,
) -> ReceiptPaneStep {
    match event {
        Ok(PaneEvent::PaneRemoved(id)) if id == pane_id => ReceiptPaneStep::Rebound,
        Ok(PaneEvent::PaneChanged { id, row, .. }) if id == pane_id => {
            if row.dead {
                ReceiptPaneStep::Rebound
            } else {
                ReceiptPaneStep::Recheck
            }
        }
        Ok(PaneEvent::OutputActivity { pane_id: id, .. }) if id == pane_id && frozen => {
            ReceiptPaneStep::Recheck
        }
        Ok(PaneEvent::Disconnected) | Err(broadcast::error::RecvError::Closed) => {
            ReceiptPaneStep::Freeze
        }
        _ => ReceiptPaneStep::Ignore,
    }
}

/// The per-delivery ACK timeline: the tier-1 hook window, the tier-2
/// screen-evidence checkpoints, and the give-up deadline.
///
/// While the session's control connection is down, the daemon cannot observe
/// the pane, so the clock freezes. On reattach every remaining instant shifts
/// by the outage duration. Time lost to a detach never counts against an
/// acknowledgment window.
struct AckClock {
    /// End of the tier-1 hook phase; None once the phase ended (or for
    /// screen-tier agents that never had one).
    hook_deadline: Option<Instant>,
    checkpoints: Vec<Instant>,
    next: usize,
    deadline: Instant,
    frozen_at: Option<Instant>,
}

impl AckClock {
    fn new(submit_at: Instant, hook_window: Option<Duration>) -> AckClock {
        AckClock {
            hook_deadline: hook_window.map(|w| submit_at + w),
            checkpoints: ACK_CHECKPOINTS_MS
                .iter()
                .map(|ms| submit_at + Duration::from_millis(*ms))
                .collect(),
            next: 0,
            deadline: submit_at + SCREEN_ACK_DEADLINE,
            frozen_at: None,
        }
    }

    fn frozen(&self) -> bool {
        self.frozen_at.is_some()
    }

    fn freeze(&mut self, now: Instant) {
        if self.frozen_at.is_none() {
            self.frozen_at = Some(now);
        }
    }

    /// Reattach: shift every remaining instant by the detach duration.
    fn unfreeze(&mut self, now: Instant) {
        let Some(at) = self.frozen_at.take() else {
            return;
        };
        let lost = now.saturating_duration_since(at);
        if let Some(h) = &mut self.hook_deadline {
            *h += lost;
        }
        for c in &mut self.checkpoints[self.next..] {
            *c += lost;
        }
        self.deadline += lost;
    }

    /// Next timer to arm: (instant, is_hook_phase_end). None while frozen;
    /// a frozen clock never fires.
    fn next_target(&self) -> Option<(Instant, bool)> {
        if self.frozen() {
            return None;
        }
        if let Some(h) = self.hook_deadline {
            return Some((h, true));
        }
        Some((
            self.checkpoints
                .get(self.next)
                .copied()
                .unwrap_or(self.deadline),
            false,
        ))
    }

    /// The tier-1 phase ended, so tier 2 opens here: one pass now, then the
    /// checkpoints the hook window did not cover.
    ///
    /// The passes inside the window are dropped rather than replayed. None
    /// of them ran (the hook deadline is the only armed timer while the
    /// phase lasts), and three captures of one screen in the same instant
    /// answer the same question three times.
    ///
    /// The pass AT `now` is the one that matters, and leaving it out is
    /// what shipped. MEASURED at the defaults: submit at +20ms, hook window
    /// closes at +1520ms, and the next unexpired checkpoint is submit+3000,
    /// so nothing looked at a pane that had held the evidence since +20ms
    /// for a second and a half. receipt_block_ms (2500) expires inside that
    /// hole, which is why every send to an agent whose hooks are not wired
    /// printed "queued" and no delivery badge ever reached the sender.
    fn end_hook_phase(&mut self, now: Instant) {
        self.hook_deadline = None;
        while self.next < self.checkpoints.len() && self.checkpoints[self.next] <= now {
            self.next += 1;
        }
        self.checkpoints.insert(self.next, now);
    }

    fn advance_checkpoint(&mut self) {
        self.next += 1;
    }

    fn expired(&self, now: Instant) -> bool {
        !self.frozen() && now >= self.deadline
    }
}

/// Receive from an optional pane-event stream; pends forever when the
/// session is detached (no watcher, no stream).
async fn recv_pane(
    rx: &mut Option<broadcast::Receiver<PaneEvent>>,
) -> Result<PaneEvent, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Tier 1: the manifest hook ACK inside ack_timeout_ms. Tier 2: screen
/// evidence until the deadline, checked on pane events and bounded
/// one-shot checkpoints. A hook ACK is accepted at any point.
///
/// The clock freezes across a session detach and a reattach runs an
/// immediate evidence pass BEFORE any deadline can expire, so a delivery
/// that landed during the outage resolves as delivered instead of being
/// resubmitted (the m1 soak's duplicate). Hook ACKs arriving during the
/// outage are accepted by the matcher independently of this loop.
struct ReceiptWait<'a> {
    manifest: &'a Manifest,
    staged_window: &'a str,
    id_staged: bool,
    target: StagingTarget<'a>,
    submit_at: Instant,
    events: broadcast::Receiver<Event>,
    turn_events: broadcast::Receiver<Event>,
}

fn receipt_is_resolved(handle: &DeliveryHandle) -> bool {
    matches!(
        handle.state(),
        DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
    )
}

/// Preserve the tier-1 diagnostic window when screen evidence resolves first.
/// A hook arriving before `deadline` records liveness and suppresses the ping.
fn schedule_missing_hook_diagnostic(
    inner: &Arc<Inner>,
    handle: &DeliveryHandle,
    manifest_id: &str,
    deadline: Option<Instant>,
) {
    let Some(deadline) = deadline else {
        return;
    };
    let Some(agent) = *handle.submitted_agent.lock().expect("submitted agent lock") else {
        return;
    };
    let pane = PaneKey::new(handle.session_idx, &handle.pane_id);
    let Some(binding) = inner.hook_liveness.binding(&pane, agent, manifest_id) else {
        return;
    };
    let task_inner = Arc::clone(inner);
    let msg_id = handle.msg_id.clone();
    let to = handle.to.clone();
    let mut stop = inner.stop.clone();
    inner.engine.spawn_descendant_task(async move {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                crate::selftest::notify_f1_once(&task_inner, &msg_id, &to, binding);
            }
            _ = stop.changed() => {}
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn receipt_checkpoint_pass(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staged_window: &str,
    id_staged: bool,
    target: StagingTarget<'_>,
    working_seen: bool,
    turn_events: &mut broadcast::Receiver<Event>,
    output_seen: bool,
    clock: &AckClock,
) -> ReceiptStep {
    // `turn_events` subscribed before Enter. A screen checkpoint can become
    // ready at the same instant as the state event, so account for every
    // already-buffered matching edge before that checkpoint is allowed to
    // settle the delivery. This is a fact recorder only; lifecycle handling
    // remains on the main receipt event stream.
    let working_seen = working_seen
        || handle.working_seen.load(Ordering::SeqCst)
        || record_buffered_working_evidence(turn_events, handle);
    let Some(watcher) = inner.watcher_of(handle.session_idx) else {
        return ReceiptStep::Freeze;
    };
    let detection = crate::observe_pane(
        inner,
        handle.session_idx,
        &watcher,
        &handle.pane_id,
        true,
        "receipt_checkpoint",
    )
    .await;
    let same_watcher = inner
        .watcher_of(handle.session_idx)
        .is_some_and(|current| Arc::ptr_eq(&current, &watcher));
    match receipt_refresh_step(
        same_watcher,
        detection.as_ref(),
        receipt_is_resolved(handle),
    ) {
        ReceiptRefresh::Resolved => ReceiptStep::Resolved,
        ReceiptRefresh::Freeze => ReceiptStep::Freeze,
        ReceiptRefresh::Rebound => ReceiptStep::Rebound,
        ReceiptRefresh::Observe => {
            match checkpoint_step(
                screen_evidence(
                    inner,
                    handle,
                    manifest,
                    staged_window,
                    id_staged,
                    target,
                    working_seen,
                    output_seen,
                )
                .await,
                clock.expired(Instant::now()),
            ) {
                CheckpointStep::Deliver => ReceiptStep::Deliver,
                CheckpointStep::Rebound => ReceiptStep::Rebound,
                CheckpointStep::Freeze => ReceiptStep::Freeze,
                CheckpointStep::Expire => ReceiptStep::Expire,
                CheckpointStep::Wait => ReceiptStep::Wait,
            }
        }
    }
}

/// Latch a matching working edge from the backlog present when this is called.
/// The scan is bounded by that fixed backlog, not an arbitrary count. A
/// receipt's main event stream handles later receipt lifecycle, while a
/// composed wait has already opened a fresh stream for later fused state.
///
/// If the receiver has lagged, this records no fact. Missing a turn can only
/// make `turn_ended` time out; inventing one would be a false success.
fn record_buffered_working_evidence(
    events: &mut broadcast::Receiver<Event>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    let backlog = events.len();
    for _ in 0..backlog {
        match events.try_recv() {
            Ok(event) => {
                if submitted_working_state_event(&event, handle) {
                    handle.working_seen.store(true, Ordering::SeqCst);
                    return true;
                }
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                return false;
            }
            Err(broadcast::error::TryRecvError::Lagged(_)) => return false,
        }
    }
    false
}

async fn await_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    wait: ReceiptWait<'_>,
) -> AckOutcome {
    let ReceiptWait {
        manifest,
        staged_window,
        id_staged,
        target,
        submit_at,
        events: mut ev_rx,
        turn_events: mut turn_ev_rx,
    } = wait;
    let tier1 = manifest.hooks.ack.is_some() && manifest.hooks.ack_payload_field.is_some();
    let mut pane_rx = inner.watcher_of(handle.session_idx).map(|w| w.subscribe());
    let mut working_seen = false;
    let output_seen = false;
    let mut clock = AckClock::new(
        submit_at,
        tier1.then(|| Duration::from_millis(inner.cfg.ack_timeout_ms)),
    );
    if pane_rx.is_none() {
        clock.freeze(Instant::now());
    }

    loop {
        // Asked before every sleep, not only on a notification. The
        // notification is edge-triggered and can fire before this loop is
        // listening for it, which would leave a delivery that is already
        // resolved waiting out the whole acknowledgement window.
        if matches!(
            handle.state(),
            DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
        ) {
            return AckOutcome::Resolved;
        }
        let checkpoint_target = clock.next_target();
        tokio::select! {
            _ = handle.ack.notified() => {
                if matches!(
                    handle.state(),
                    DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
                ) {
                    return AckOutcome::Resolved;
                }
            }
            _ = tokio::time::sleep_until(checkpoint_target.map(|(t, _)| t).unwrap_or_else(Instant::now)),
                if checkpoint_target.is_some() =>
            {
                let now = Instant::now();
                if checkpoint_target.is_some_and(|(_, hook_end)| hook_end) {
                    // Tier-1 window over: the delivery downgrades to screen
                    // evidence. On a pane that has never produced a hook
                    // edge, this is the missing-hook signature: configuration
                    // does not equal subscription. The admin hears once.
                    if tier1 {
                        if let Some(agent) = *handle
                            .submitted_agent
                            .lock()
                            .expect("submitted agent lock")
                        {
                            let pane = PaneKey::new(handle.session_idx, &handle.pane_id);
                            if let Some(binding) = inner.hook_liveness.binding(
                                &pane,
                                agent,
                                &manifest.agent.id,
                            ) {
                                crate::selftest::notify_f1_once(
                                    inner,
                                    &handle.msg_id,
                                    &handle.to,
                                    binding,
                                );
                            }
                        }
                    }
                    clock.end_hook_phase(now);
                    continue;
                }
                clock.advance_checkpoint();
                match receipt_checkpoint_pass(
                    inner, handle, manifest, staged_window, id_staged, target,
                    working_seen, &mut turn_ev_rx, output_seen, &clock,
                ).await {
                    ReceiptStep::Resolved => return AckOutcome::Resolved,
                    ReceiptStep::Deliver => {
                        schedule_missing_hook_diagnostic(
                            inner,
                            handle,
                            &manifest.agent.id,
                            clock.hook_deadline,
                        );
                        return AckOutcome::Screen;
                    }
                    ReceiptStep::Expire => return AckOutcome::Timeout,
                    ReceiptStep::Rebound => return AckOutcome::Rebound,
                    ReceiptStep::Freeze => {
                        // The stable refresh could not prove the screen or
                        // binding. A timeout here would stand on nothing.
                        clock.freeze(Instant::now());
                    }
                    ReceiptStep::Wait => {}
                }
            }
            // Reattach/detach truth for THIS session comes from
            // `inner.watcher_of(handle.session_idx)`, resolved fresh here,
            // never from matching a "session" event's own `data["name"]`
            // against a name captured at function entry: a followed
            // rename (`PaneEvent::SessionRenamed`, `rename_session_slot`
            // in lib.rs) changes the live name mid-wait, and a stale
            // snapshot then never matches an attach or a detach line
            // again. The clock freezes on the first outage and never
            // unfreezes, which is exactly the "ledger append silently
            // drops" failure mode `emit_state`'s doc comment describes,
            // in delivery-wait clothing. `watcher_of` cannot go stale: it
            // reads the live link for this exact idx, so what changed is
            // compared against what IS, not against a name.
            ev = ev_rx.recv() => {
                if track_state_event(&ev, handle) {
                    working_seen = true;
                    handle.working_seen.store(true, Ordering::SeqCst);
                }
                if is_session_event(&ev) {
                    let live = inner.watcher_of(handle.session_idx);
                    if pane_rx.is_none() {
                        if let Some(w) = live {
                            pane_rx = Some(w.subscribe());
                            clock.unfreeze(Instant::now());
                            // Reattach evidence pass, before any deadline
                            // can fire: did the payload arrive during the
                            // outage?
                            match receipt_checkpoint_pass(
                                inner, handle, manifest, staged_window, id_staged, target,
                                working_seen, &mut turn_ev_rx, output_seen, &clock,
                            ).await {
                                ReceiptStep::Resolved => return AckOutcome::Resolved,
                                ReceiptStep::Deliver => {
                                    schedule_missing_hook_diagnostic(
                                        inner,
                                        handle,
                                        &manifest.agent.id,
                                        clock.hook_deadline,
                                    );
                                    return AckOutcome::Screen;
                                }
                                ReceiptStep::Expire => return AckOutcome::Timeout,
                                ReceiptStep::Rebound => return AckOutcome::Rebound,
                                ReceiptStep::Freeze => clock.freeze(Instant::now()),
                                ReceiptStep::Wait => {}
                            }
                        }
                    } else if live.is_none() {
                        pane_rx = None;
                        clock.freeze(Instant::now());
                    }
                } else if matches!(ev, Err(broadcast::error::RecvError::Lagged(_)))
                    && clock.frozen()
                {
                    // A lagged event stream can swallow the reattach
                    // notice; reconcile against the link instead of
                    // staying frozen forever.
                    if let Some(w) = inner.watcher_of(handle.session_idx) {
                        pane_rx = Some(w.subscribe());
                        clock.unfreeze(Instant::now());
                    }
                }
            }
            pe = recv_pane(&mut pane_rx) => {
                match receipt_pane_step(&pe, &handle.pane_id, clock.frozen()) {
                    ReceiptPaneStep::Recheck => {
                        // Output is only a cue to look. PaneChanged carries a
                        // new watcher revision. Both resume a frozen clock and
                        // run the same stable receipt checkpoint immediately.
                        clock.unfreeze(Instant::now());
                        match receipt_checkpoint_pass(
                            inner, handle, manifest, staged_window, id_staged, target,
                            working_seen, &mut turn_ev_rx, output_seen, &clock,
                        ).await {
                            ReceiptStep::Resolved => return AckOutcome::Resolved,
                            ReceiptStep::Deliver => {
                                schedule_missing_hook_diagnostic(
                                    inner,
                                    handle,
                                    &manifest.agent.id,
                                    clock.hook_deadline,
                                );
                                return AckOutcome::Screen;
                            }
                            ReceiptStep::Expire => return AckOutcome::Timeout,
                            ReceiptStep::Rebound => return AckOutcome::Rebound,
                            ReceiptStep::Freeze => clock.freeze(Instant::now()),
                            ReceiptStep::Wait => {}
                        }
                    }
                    ReceiptPaneStep::Rebound => return AckOutcome::Rebound,
                    ReceiptPaneStep::Freeze => {
                        pane_rx = None;
                        clock.freeze(Instant::now());
                    }
                    ReceiptPaneStep::Ignore => {}
                }
            }
        }
    }
}

/// True when an event is an explicitly confirmed Working observation.
fn confirmed_working_state_event(event: &Event) -> bool {
    event.event == "state"
        && event.data["state"] == "working"
        && event.data["working_confirmed"] == true
}

/// True when an event proves a Working edge for this exact submitted delivery.
///
/// This does not associate a turn with a particular message. It only proves
/// that the submitted process generation entered Working after the submit
/// boundary, which is the conservative evidence `turn_ended` may carry from
/// delivery into its separate pane wait.
fn submitted_working_state_event(event: &Event, handle: &Arc<DeliveryHandle>) -> bool {
    if !confirmed_working_state_event(event) || event.data["pane_id"] != handle.pane_id.as_str() {
        return false;
    }
    if event.data["session_idx"].as_u64() != Some(handle.session_idx as u64) {
        return false;
    }
    let submitted_at_ms = handle.submitted_at_ms.load(Ordering::SeqCst);
    if event.data["observed_at_ms"]
        .as_u64()
        .is_none_or(|observed_at_ms| observed_at_ms < submitted_at_ms)
    {
        return false;
    }
    // The event carries the binding that produced it, so this asks whether
    // the working edge came from the process that received the submit.
    // It does not identify which message or task the turn handled.
    // Comparing against the row as it looks now
    // would accept a replacement's turn, and would keep accepting it for
    // as long as the pane happened to look familiar again.
    // Both halves of the identity travel on the event, because a pid
    // alone is transferable and this is a trust comparison.
    let Some(birth) = event.data["source_birth"].as_u64() else {
        return false;
    };
    let agent = crate::identity::ProcId {
        pid: event.data["source_pid"].as_i64().unwrap_or_default() as i32,
        birth,
    };
    let manifest = event.data["source_manifest"].as_str().unwrap_or_default();
    handle.submitted_binding_is(agent, manifest)
}

/// True when a wait may count an event as its Working phase. A standalone
/// `agent.wait` follows the fused contract. A composed wait additionally
/// retains the exact delivery identity it inherited across receipt settlement.
fn wait_working_event_is_eligible(
    event: &Event,
    submitted_turn: Option<&Arc<DeliveryHandle>>,
) -> bool {
    match submitted_turn {
        Some(handle) => submitted_working_state_event(event, handle),
        None => confirmed_working_state_event(event),
    }
}

/// Compatibility wrapper for receipt-path callers and focused tests.
fn track_state_event(
    ev: &Result<Event, broadcast::error::RecvError>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    ev.as_ref()
        .is_ok_and(|event| submitted_working_state_event(event, handle))
}

/// True when the event is a session lifecycle line: attach, detach, or
/// this daemon's own rename bookkeeping riding the same "session" name
/// (`session_lifecycle`, lib.rs). Which one, and whether it is about THIS
/// caller's session at all, is deliberately not decided here: see the doc
/// comment on `await_ack`'s event arm for why comparing against
/// `inner.watcher_of(session_idx)`'s live truth, not the event's own
/// `data["name"]`, is what a caller does with this.
fn is_session_event(ev: &Result<Event, broadcast::error::RecvError>) -> bool {
    matches!(ev, Ok(e) if e.event == "session")
}

/// Screen evidence for tier 2 uses the protocol's conjunctive form:
/// the marker left the composer AND turn evidence appeared.
///
/// "Left the composer" is manifest-driven: the marker still sits in the
/// composer only when an idle_with_input rule identifies a composer line
/// that carries it (staged-but-unsubmitted text, e.g. Claude's collapsed
/// paste on the `❯` line). Manifests without an idle_with_input rule
/// cannot pin staged text.
///
/// Turn evidence is a working state, output activity, or a changed
/// composer window. The changed window counts only when verification
/// demonstrably staged the id pattern: a redraw can change the window of a
/// pane that never took the paste, but it cannot have staged OUR id first.
/// (%output events can be swallowed by the watcher's per-pane rate limit
/// for single short bursts, MEASURED: a cat pane's echoed submit stays
/// under the 100ms floor, which is why the changed window matters.)
fn staging_target_still_present(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
) -> bool {
    // This is a post-submit presence check, not ownership proof. A collapsed
    // chip may show that the staged representation remains, but it never
    // authorizes a terminal key.
    match target {
        StagingTarget::Sentinel(msg_id) => {
            sentinel_verified(manifest, screen, msg_id) || marker_in_composer(manifest, screen)
        }
        StagingTarget::ExactRow(expected_row) => {
            staged_representation(manifest, screen, StagingTarget::ExactRow(expected_row)).is_some()
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn screen_evidence(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staged_window: &str,
    id_staged: bool,
    target: StagingTarget<'_>,
    working_seen: bool,
    output_seen: bool,
) -> Evidence {
    let Some(watcher) = inner.watcher_of(handle.session_idx) else {
        return Evidence::Unobservable;
    };
    // Use the staging capture flavor so esc-only composer discriminators
    // still apply and the de-escaped window comparison stays like-for-like.
    // The binding is checked on both sides of the read: a capture is not
    // instantaneous, and evidence about a pane whose occupant changed
    // while it was being read is evidence about nobody.
    if !submitted_binding_holds(inner, &watcher, handle) {
        return Evidence::Rebound;
    }
    let capture = if manifest.has_escaped_rules() {
        watcher.client().capture_pane_escaped(&handle.pane_id).await
    } else {
        watcher.client().capture_pane(&handle.pane_id).await
    };
    let Ok(screen) = capture else {
        return Evidence::Unobservable;
    };
    if !submitted_binding_holds(inner, &watcher, handle) {
        return Evidence::Rebound;
    }
    let changed = bottom_window(&strip_csi(&screen), COMPOSER_WINDOW) != staged_window;
    // "The marker left the COMPOSER", and the emphasis is the whole
    // point: a submitted message stays on screen, it just stops being
    // staged input. Asking whether the id appears anywhere in the bottom
    // region would answer yes forever, because the transcript keeps it.
    // So this asks the same two questions staging asked, which are both
    // composer-pinned: is our sentinel still the staged row, or is the
    // vendor's chip still on the composer line.
    let marker_present = staging_target_still_present(manifest, &screen, target);
    if !marker_present
        && tier2_evidence(
            manifest.hooks.ack_evidence,
            changed,
            id_staged,
            working_seen,
            output_seen,
        )
    {
        Evidence::Confirmed
    } else {
        Evidence::Absent
    }
}

/// The tier-2 turn-evidence rule, factored for the unit test: a changed
/// window alone is only evidence when the id demonstrably staged.
fn tier2_evidence(
    ack_evidence: AckEvidence,
    changed: bool,
    id_staged: bool,
    working_seen: bool,
    output_seen: bool,
) -> bool {
    let _ = output_seen;
    match ack_evidence {
        AckEvidence::Receipt => working_seen || (changed && id_staged),
        // A dispatch hook can precede a sibling hook that rejects the prompt.
        // Only the exact candidate's later visual acceptance resolves it.
        AckEvidence::Dispatch => false,
    }
}

/// Prove that the active composer contains the exact bytes selected for this
/// attempt. Visible payloads are reconstructed from joined logical rows and
/// compared byte for byte. A collapsed chip proves only that the vendor drew a
/// chip. Its hidden bytes cannot prove ownership and can never authorize a
/// submit key.
fn exact_staging_proof(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
    expected_payload: &str,
) -> Option<(bool, String)> {
    if staged_representation(manifest, screen, target) != Some(StagedRepresentation::VisibleTarget)
    {
        return None;
    }
    let content = match target {
        StagingTarget::Sentinel(msg_id) => {
            composer_content_from_joined_capture(manifest, screen, msg_id)
        }
        StagingTarget::ExactRow(_) => exact_composer_content_from_joined_capture(manifest, screen),
    };
    match content {
        ComposerContentProof::Visible(content)
            if visible_single_line_payload_matches(&content, expected_payload) =>
        {
            Some((true, expected_payload.to_string()))
        }
        ComposerContentProof::Visible(_)
        | ComposerContentProof::Hidden
        | ComposerContentProof::Unsupported
        | ComposerContentProof::Unprovable => None,
    }
}

/// Match one exact single-line payload after a terminal application has drawn
/// it over several visual composer rows.
///
/// Codex, Claude, and AGY wrap at word boundaries themselves, so tmux `-J`
/// cannot join those rows. They also repaint the unused suffix of each visual
/// composer row with ASCII spaces. Those renderer-owned suffix cells and the
/// one ASCII separator consumed at a wrap boundary are ignored. No other byte
/// may be added, removed, or reordered.
pub(crate) fn visible_single_line_payload_matches(visible: &str, expected: &str) -> bool {
    if visible == expected {
        return true;
    }
    if expected.contains('\n') || !visible.contains('\n') {
        return false;
    }

    let parts: Vec<&str> = visible.split('\n').collect();
    let mut offsets = vec![0usize];
    for (at, part) in parts.iter().enumerate() {
        let part = part.trim_end_matches(' ');
        if part.is_empty() {
            return false;
        }
        let mut next = Vec::with_capacity(offsets.len() * 2);
        for offset in offsets {
            let Some(remaining) = expected.get(offset..) else {
                continue;
            };
            let Some(remaining) = remaining.strip_prefix(part) else {
                continue;
            };
            let end = expected.len() - remaining.len();
            next.push(end);
            if at + 1 < parts.len() && expected.as_bytes().get(end) == Some(&b' ') {
                next.push(end + 1);
            }
        }
        next.sort_unstable();
        next.dedup();
        offsets = next;
        if offsets.is_empty() {
            return false;
        }
    }
    offsets.binary_search(&expected.len()).is_ok()
}

/// Re-prove the same exact normalized composer bytes selected earlier.
///
/// This comparison is used on both sides of the durable `Submitting`
/// reservation. A second valid-looking payload is still a mismatch.
fn exact_staging_snapshot_matches(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
    expected_payload: &str,
    id_staged: bool,
    payload_at_proof: &str,
) -> bool {
    let current = exact_staging_proof(manifest, screen, target, expected_payload);
    current.as_ref().map(|(_, proof)| proof.as_str()) == Some(payload_at_proof)
        && current.map(|(matched, _)| matched) == Some(id_staged)
}

/// Why the final exact-staging reread did not produce an owned doorbell.
///
/// A renderer can expose a partial frame while it clears and repaints. That
/// is distinguishable from a broken capture pipe: if at least one capture
/// completed but none restored the exact proof, the pane changed or remained
/// ambiguous and Enter stays withheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExactStagingRecheck {
    Mismatch,
    Unobservable,
}

/// Read the exact staged composer through the same bounded frame schedule as
/// the post-paste proof. This is a re-observation only: it never writes, and
/// it accepts only the same normalized payload and staging representation
/// already proven for this notification.
async fn recheck_exact_staging_snapshot<I: Injector>(
    injector: &I,
    pane_id: &str,
    manifest: &Manifest,
    target: StagingTarget<'_>,
    expected_payload: &str,
    id_staged: bool,
    payload_at_proof: &str,
) -> Result<String, ExactStagingRecheck> {
    let mut last_delay = 0;
    let mut observed = false;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        let Ok(screen) = injector.capture_joined_escaped(pane_id).await else {
            continue;
        };
        observed = true;
        if exact_staging_snapshot_matches(
            manifest,
            &screen,
            target,
            expected_payload,
            id_staged,
            payload_at_proof,
        ) {
            return Ok(screen);
        }
    }
    Err(if observed {
        ExactStagingRecheck::Mismatch
    } else {
        ExactStagingRecheck::Unobservable
    })
}

/// Closed screen-representation outcomes for the Gate 7 component harness.
///
/// This proof cannot authorize delivery. It deliberately excludes process
/// binding, pane mode, action safety, and durable composer holds. The daemon's
/// normal gate remains the only authority for a real write.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerRepresentationProof {
    ExactStaged,
    WriteSafeClean,
    WriteSafeGhost,
    HiddenOrAmbiguous,
}

/// Classifies the visible composer through production representation parsers.
///
/// Callers must not use this as a write-readiness decision. It exists only so
/// the opt-in live harness can measure the same exact staged-row and
/// clean-or-ghost screen representations the daemon consumes.
#[doc(hidden)]
pub fn prove_composer_representation(
    manifest: &Manifest,
    screen: &str,
    expected_staged: Option<&str>,
) -> ComposerRepresentationProof {
    if let Some(expected) = expected_staged {
        if exact_staging_proof(
            manifest,
            screen,
            StagingTarget::ExactRow(expected),
            expected,
        )
        .is_some()
        {
            return ComposerRepresentationProof::ExactStaged;
        }
    } else {
        if clean_composer_proof(manifest, screen) {
            return ComposerRepresentationProof::WriteSafeClean;
        }
        let plain = strip_csi(screen);
        let winner = fusion::screen_winner_esc(manifest, &plain, Some(screen));
        if winner.is_some_and(|rule| {
            rule.state == AgentState::Idle
                && rule.composer_semantic == Some(ComposerSemantic::GhostSuggestion)
        }) {
            return ComposerRepresentationProof::WriteSafeGhost;
        }
    }
    ComposerRepresentationProof::HiddenOrAmbiguous
}

/// Extract exact visible composer rows from a joined escaped capture.
///
/// The caller must use `capture-pane -J -e`. Joining removes only rows tmux
/// marked as physical wraps. Application-rendered line breaks remain and
/// need the manifest's prompt and continuation patterns. The terminal
/// sentinel plus the measured styled trailer bind the extraction to the
/// active composer rather than a transcript echo.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn composer_content_from_joined_capture(
    manifest: &Manifest,
    screen: &str,
    msg_id: &str,
) -> ComposerContentProof {
    if collapsed_chip_row(manifest, screen).is_some() {
        return ComposerContentProof::Hidden;
    }
    let (Some(prompt), Some(continuation)) = (
        manifest.composer_prompt.as_ref(),
        manifest.composer_continuation.as_ref(),
    ) else {
        return ComposerContentProof::Unsupported;
    };

    let rows = joined_composer_rows(screen);
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let want_sentinel = sentinel_for(msg_id);
    let sentinel_hits: Vec<usize> = rows[start..]
        .iter()
        .enumerate()
        .filter_map(|(offset, (_, plain))| {
            (captured_continuation_content(manifest, continuation, plain)
                == Some(want_sentinel.as_str()))
            .then_some(start + offset)
        })
        .filter(|at| trailer_follows(manifest, &rows[*at + 1..]))
        .collect();
    let &[sentinel_at] = sentinel_hits.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let want_header = format!("[cyclops {msg_id}] FROM:");
    let header = rows[..=sentinel_at]
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(at, (_, plain))| {
            captured_content(prompt, plain)
                .filter(|content| content.starts_with(&want_header))
                .map(|content| (at, content))
        })
        .next();
    let Some((prompt_at, first)) = header else {
        return ComposerContentProof::Unprovable;
    };

    let mut content = vec![first.to_string()];
    let mut sentinel_count = 0;
    for (_, plain) in &rows[prompt_at + 1..=sentinel_at] {
        if captured_content(prompt, plain).is_some() {
            return ComposerContentProof::Unprovable;
        }
        if plain.is_empty() {
            content.push(String::new());
        } else if let Some(line) = captured_continuation_content(manifest, continuation, plain) {
            if line == want_sentinel {
                sentinel_count += 1;
            }
            content.push(line.to_string());
        } else {
            return ComposerContentProof::Unprovable;
        }
    }
    if sentinel_count != 1 || content.last().is_none_or(|line| line != &want_sentinel) {
        return ComposerContentProof::Unprovable;
    }
    ComposerContentProof::Visible(content.join("\n"))
}

/// Extract the active single-line notification composer for a local diff.
///
/// The prompt must satisfy both the manifest extraction pattern and an
/// IdleWithInput rule. The declared trailer must follow the extracted rows
/// exactly. These two checks keep transcript prompts and unrelated pane text
/// out of the result.
pub(crate) fn exact_composer_content_from_joined_capture(
    manifest: &Manifest,
    screen: &str,
) -> ComposerContentProof {
    exact_composer_content_for_state(manifest, screen, AgentState::IdleWithInput, None)
}

/// Extract exact occupied input or a visibly empty clean composer.
///
/// Projection and recovery need both outcomes. Ghost suggestions and
/// ambiguous input remain unprovable, and collapsed paste chips remain hidden.
pub(crate) fn composer_content_for_projection_from_joined_capture(
    manifest: &Manifest,
    screen: &str,
) -> ComposerContentProof {
    match exact_composer_content_from_joined_capture(manifest, screen) {
        ComposerContentProof::Unprovable => exact_composer_content_for_state(
            manifest,
            screen,
            AgentState::Idle,
            Some(ComposerSemantic::Clean),
        ),
        proof => proof,
    }
}

fn exact_composer_content_for_state(
    manifest: &Manifest,
    screen: &str,
    state: AgentState,
    required_semantic: Option<ComposerSemantic>,
) -> ComposerContentProof {
    if collapsed_chip_row(manifest, screen).is_some() {
        return ComposerContentProof::Hidden;
    }
    let (Some(prompt), Some(continuation)) = (
        manifest.composer_prompt.as_ref(),
        manifest.composer_continuation.as_ref(),
    ) else {
        return ComposerContentProof::Unsupported;
    };
    let composer_rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|rule| {
            rule.state == state
                && required_semantic.is_none_or(|semantic| rule.composer_semantic == Some(semantic))
        })
        .collect();
    if composer_rules.is_empty() {
        return ComposerContentProof::Unprovable;
    }

    let rows = joined_composer_rows(screen);
    let structural_unstyled = manifest.injection.unstyled_composer_proof
        == Some(UnstyledComposerProof::StructuralTrailer)
        && !screen.contains('\u{1b}');
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];
    let trailers: Vec<usize> = (0..window.len())
        .filter(|at| trailer_follows(manifest, &window[*at..]))
        .collect();
    let [trailer_at] = trailers.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let prompts: Vec<(usize, &str)> = window[..*trailer_at]
        .iter()
        .enumerate()
        .filter_map(|(at, (raw, plain))| {
            let content = captured_content(prompt, plain)?;
            (composer_rules
                .iter()
                .any(|rule| rule.matches_row(plain, raw))
                || (state == AgentState::IdleWithInput && structural_unstyled && raw == plain))
                .then_some((at, content))
        })
        .filter(|(prompt_at, _)| {
            window[prompt_at + 1..*trailer_at].iter().all(|(_, plain)| {
                captured_continuation_content(manifest, continuation, plain).is_some()
            })
        })
        .collect();
    let [(prompt_at, first)] = prompts.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let mut content = vec![(*first).to_string()];
    for (_, plain) in &window[prompt_at + 1..*trailer_at] {
        if captured_content(prompt, plain).is_some() {
            return ComposerContentProof::Unprovable;
        }
        let Some(line) = captured_continuation_content(manifest, continuation, plain) else {
            return ComposerContentProof::Unprovable;
        };
        content.push(line.to_string());
    }
    // tmux 3.6a retains right-padding cells in a joined capture. Normalize
    // them only after a manifest rule has classified one prompt row as clean.
    // Occupied composer rows keep every byte for exact ownership checks.
    if state == AgentState::Idle
        && required_semantic == Some(ComposerSemantic::Clean)
        && content.len() == 1
        && content[0].bytes().all(|byte| byte == b' ')
    {
        content[0].clear();
    }
    ComposerContentProof::Visible(content.join("\n"))
}

#[cfg_attr(not(test), allow(dead_code))]
fn captured_content<'a>(pattern: &cyclops_manifest::Regex, row: &'a str) -> Option<&'a str> {
    let captures = pattern.captures(row)?;
    let whole = captures.get(0)?;
    if whole.start() != 0 || whole.end() != row.len() {
        return None;
    }
    captures.name("content").map(|content| content.as_str())
}

/// Extract one continuation row without allowing a legacy seed to redefine
/// the exact payload comparison.
///
/// The old shipped AGY pattern captured its renderer's two-cell continuation
/// gutter as `content`. In the measured AGY 1.1.23 doorbell, a non-space
/// payload byte immediately follows that gutter. Strip the gutter only for
/// the complete, exact pre-change shipped manifest source and only when a
/// third leading ASCII space is absent; a third space could be deliberate
/// input and must remain a mismatch. An operator-customized manifest with the
/// same regex does not match that source fingerprint and therefore fails
/// closed. New manifests express the gutter in their regex, so they never
/// enter this compatibility path.
const LEGACY_AGY_PRE_GUTTER_MANIFEST_SHA256: [u8; 32] = [
    0x9c, 0xfc, 0x99, 0xfd, 0x61, 0xc8, 0x36, 0xa6, 0x54, 0xce, 0x15, 0x24, 0x2c, 0xca, 0xa3, 0x7c,
    0xaf, 0x53, 0xaa, 0xbc, 0xee, 0x5e, 0xec, 0x1d, 0x02, 0xab, 0xee, 0x5b, 0xe2, 0x28, 0x94, 0x98,
];

fn captured_continuation_content<'a>(
    manifest: &Manifest,
    pattern: &cyclops_manifest::Regex,
    row: &'a str,
) -> Option<&'a str> {
    let content = captured_content(pattern, row)?;
    let legacy_agy_pattern = manifest.agent.id == "agy"
        && manifest.source_digest() == LEGACY_AGY_PRE_GUTTER_MANIFEST_SHA256
        && pattern.as_str() == "^(?P<content>.*)$";
    if legacy_agy_pattern && content.starts_with("  ") && content.as_bytes().get(2) != Some(&b' ') {
        return Some(&content[2..]);
    }
    Some(content)
}

/// Does this pattern match the ENTIRE row, rather than some run inside it?
///
/// Terminality again, in a second place: a chip pattern that matches a
/// substring proves a chip appeared somewhere on the row, not that the row
/// IS the chip, and a row carrying payload either side of it would pass.
/// Anchors in manifest data cannot be relied on for that, so the span is
/// checked here where no vendor can forget it.
fn whole_row(re: &cyclops_manifest::Regex, row: &str) -> bool {
    re.find(row)
        .is_some_and(|m| m.start() == 0 && m.end() == row.len())
}

fn marker_in_composer(manifest: &Manifest, screen: &str) -> bool {
    collapsed_chip_row(manifest, screen).is_some()
}

/// The recognized chip row when the collapsed representation matches.
///
/// Equality against this row is equality of the screen representation only.
/// The payload behind a chip is not on screen, so it cannot prove exact
/// notification ownership and cannot authorize Enter.
fn collapsed_chip_row(manifest: &Manifest, screen: &str) -> Option<String> {
    if manifest.composer_chips.is_empty()
        || manifest.composer_chips.len() != manifest.composer_chips_esc.len()
    {
        return None;
    }
    // No separate "is this an escaped capture" guard: the escaped half of
    // a measured chip contains escape bytes, so a plain capture fails it
    // on its own. Adding a guard on top would only stop a vendor whose
    // chip genuinely renders unstyled from ever declaring one.
    let rows = composer_rows(screen);
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];
    for rule in manifest
        .rules
        .iter()
        .filter(|r| r.state == AgentState::IdleWithInput)
    {
        let cyclops_manifest::Region::BottomNonEmptyLines(n) = rule.region else {
            continue;
        };
        // The rule's own region bounds where its chip may appear.
        let from = window.len().saturating_sub(n);
        // Exactly one chip row, for the same reason the sentinel needs
        // exactly one. A styled copy of the chip sitting in the
        // transcript above the live composer is the shape that produces
        // two, and which one transport owns is then a guess.
        let hits: Vec<usize> = window
            .iter()
            .enumerate()
            .skip(from)
            .filter(|(_, (raw, plain))| {
                let chip = manifest
                    .composer_chips
                    .iter()
                    .zip(manifest.composer_chips_esc.iter())
                    .any(|(p, e)| whole_row(p, plain.trim()) && whole_row(e, raw));
                // The manifest decides whether this row is its composer,
                // with its own clause semantics. Reimplementing that as
                // "plain matched OR escaped matched" would let either half
                // carry a rule that was written to need both.
                chip && rule.matches_row(plain, raw)
            })
            .map(|(i, _)| i)
            .collect();
        let [at] = hits[..] else {
            continue;
        };
        // A collapsed payload proves no more than a visible one does. The
        // chip says the composer holds a paste; only the vendor's own
        // chrome following it says the composer holds nothing ELSE, and a
        // line typed under the chip is exactly what that catches.
        if !trailer_follows(manifest, &window[at + 1..]) {
            continue;
        }
        return Some(window[at].1.trim().to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// ACK registry (used by the matcher in ack.rs)
// ---------------------------------------------------------------------------

fn register_ack(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) {
    let mut acks = inner.engine.acks.lock().expect("acks lock");
    let entry = acks
        .entry(PaneKey::new(handle.session_idx, &handle.pane_id))
        .or_default();
    entry.retain(|h| {
        !Arc::ptr_eq(h, handle)
            && matches!(
                h.state(),
                DeliveryState::Submitted | DeliveryState::DeliveredUnverified
            )
    });
    if entry.len() >= ACK_REGISTRY_CAP {
        entry.remove(0);
    }
    entry.push(Arc::clone(handle));
}

fn unregister_ack(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) {
    if let Some(entry) = inner
        .engine
        .acks
        .lock()
        .expect("acks lock")
        .get_mut(&PaneKey::new(handle.session_idx, &handle.pane_id))
    {
        entry.retain(|h| !Arc::ptr_eq(h, handle));
    }
}

/// Deliveries on a pane a hook ACK could match right now.
pub(crate) fn ack_candidates(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
) -> Vec<Arc<DeliveryHandle>> {
    inner
        .engine
        .acks
        .lock()
        .expect("acks lock")
        .get(&PaneKey::new(session_idx, pane_id))
        .map(|v| v.to_vec())
        .unwrap_or_default()
}

/// Resolve a hook ACK onto a delivery: verify a submitted one, or upgrade
/// a screen-verified one (the legal DeliveredUnverified -> Verified move
/// that keeps receipts honest). Racing ahead of the Submitted line sets
/// the early-ack flag the worker consumes.
pub(crate) fn resolve_hook_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    edge_ms: u64,
    turn: Option<crate::turnkey::TurnKey>,
) -> bool {
    // A hook proves a process ran a turn, and a pane id is reusable, so
    // the report has to come from the process Enter actually reached. The
    // caller already authenticated the reporting process and resolved its
    // row and manifest, so that binding is compared directly here. Looking
    // it up again through a live watcher would be worse than redundant: a
    // detached control connection has no watcher, and a legitimate hook
    // arriving during an outage would be thrown away.
    if !handle.submitted_binding_is(reporter, reporter_manifest) {
        return false;
    }
    // Classifying the state and installing an early acknowledgement are
    // ONE decision, under the lock the worker transitions through. Read
    // the state, see `Staged`, and install afterwards, and the worker can
    // move to `Submitted` and take in between: the record is then written
    // after the only read of it and the acknowledgement is lost.
    //
    // The FIRST one installed stands. A second acknowledgement for the
    // same delivery describes the same consumption, and overwriting would
    // move the edge to whichever report happened to arrive last.
    let state = {
        let mut st = handle.state.lock().expect("handle state lock");
        if st.state == DeliveryState::Staged && st.early_ack.is_none() {
            st.early_ack = Some(PendingAck {
                edge_ms,
                turn: turn.clone(),
                evidence: PendingAckEvidence::Receipt,
            });
        }
        st.state
    };
    let moved = match state {
        // Past the point where an early record would be read, so this
        // resolves the delivery here instead. `advance` is its own
        // transaction and refuses if the state moved again underneath,
        // which is the safe handoff back to the worker.
        DeliveryState::Submitted => match record_notification_notified(inner, handle) {
            Ok(true) => advance(
                inner,
                handle,
                &[DeliveryState::Submitted],
                Step::to(DeliveryState::DeliveredVerified)
                    .cause("hook_ack")
                    .verified(VerifiedBy::Hook)
                    .turn_edge(edge_ms)
                    .turn(turn),
            ),
            Ok(false) => false,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                false
            }
        },
        // A screen receipt that already resolved stands. The replacement
        // occupant cannot upgrade it, and it must not be taken away
        // either: the original binding earned it before the pane changed
        // hands, and the record does not retract what was true.
        DeliveryState::DeliveredUnverified => advance(
            inner,
            handle,
            &[DeliveryState::DeliveredUnverified],
            Step::to(DeliveryState::DeliveredVerified)
                .cause("hook_ack_upgrade")
                .verified(VerifiedBy::Hook)
                .turn_edge(edge_ms)
                .turn(turn),
        ),
        // Installed above, under the lock that read this state. The
        // worker takes it immediately after its Submitted line.
        DeliveryState::Staged => true,
        _ => false,
    };
    handle.ack.notify_waiters();
    moved
}

/// Record an exact hook dispatch that still needs visual acceptance.
///
/// This does not resolve the delivery. Vendors can invoke several prompt
/// hooks concurrently, and one sibling can reject the prompt after this hook
/// has already reported it. The matching turn remains pending until fusion
/// confirms a later Working observation for the same process and manifest.
pub(crate) fn record_dispatch_candidate(
    handle: &Arc<DeliveryHandle>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    edge_ms: u64,
    turn: Option<crate::turnkey::TurnKey>,
) -> bool {
    if !handle.submitted_binding_is(reporter, reporter_manifest) {
        return false;
    }
    let recorded = {
        let mut st = handle.state.lock().expect("handle state lock");
        if !matches!(
            st.state,
            DeliveryState::Staged | DeliveryState::Submitted | DeliveryState::DeliveredUnverified
        ) {
            return false;
        }
        match &st.early_ack {
            Some(existing)
                if existing.edge_ms != edge_ms
                    || existing.turn.as_ref() != turn.as_ref()
                    || existing.evidence != PendingAckEvidence::DispatchPending =>
            {
                false
            }
            Some(_) => true,
            None => {
                st.early_ack = Some(PendingAck {
                    edge_ms,
                    turn,
                    evidence: PendingAckEvidence::DispatchPending,
                });
                true
            }
        }
    };
    if recorded {
        handle.ack.notify_waiters();
    }
    recorded
}

/// Mark every exact payload match as ambiguous without turning any of them
/// into receipt evidence. This is the duplicate-bytes case: a pane hook cannot
/// identify which attempt it belongs to, so all candidates remain recoverable.
pub(crate) fn mark_dispatch_match_ambiguous(
    handle: &Arc<DeliveryHandle>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    cause: &str,
) -> bool {
    if !handle.submitted_binding_is(reporter, reporter_manifest) {
        return false;
    }
    let marked = {
        let mut state = handle.state.lock().expect("handle state lock");
        if !matches!(
            state.state,
            DeliveryState::Staged | DeliveryState::Submitted | DeliveryState::DeliveredUnverified
        ) {
            return false;
        }
        state.cause = Some(cause.to_string());
        true
    };
    if marked {
        handle.ack.notify_waiters();
    }
    marked
}

enum UnkeyedDispatchSelection {
    None,
    Unique(Arc<DeliveryHandle>, String),
    Ambiguous(Vec<Arc<DeliveryHandle>>),
}

fn select_unkeyed_dispatch_candidate(
    handles: Vec<Arc<DeliveryHandle>>,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    dispatch_edge_ms: u64,
) -> UnkeyedDispatchSelection {
    let mut matching = handles
        .into_iter()
        .filter_map(|handle| {
            if !handle.submitted_binding_is(reporter, reporter_manifest) {
                return None;
            }
            let state = handle.state.lock().expect("handle state lock");
            let owner = state
                .early_ack
                .as_ref()
                .filter(|pending| {
                    pending.turn.is_none()
                        && pending.evidence == PendingAckEvidence::DispatchPending
                        && pending.edge_ms == dispatch_edge_ms
                })
                .and_then(|_| state.barrier.clone())?;
            drop(state);
            Some((handle, owner))
        })
        .collect::<Vec<_>>();
    match matching.len() {
        0 => UnkeyedDispatchSelection::None,
        1 => {
            let (handle, owner) = matching.pop().expect("one dispatch candidate");
            UnkeyedDispatchSelection::Unique(handle, owner)
        }
        _ => UnkeyedDispatchSelection::Ambiguous(
            matching.into_iter().map(|(handle, _)| handle).collect(),
        ),
    }
}

/// Accept an unkeyed prompt dispatch after the exact pane shows a later
/// lifecycle-capable Working frame.
///
/// The prompt hook alone is provisional because another vendor hook may still
/// reject the prompt. The barrier owner makes the later visual observation
/// recipient- and attempt-specific even though the vendor exposes no turn id.
pub(crate) fn confirm_unkeyed_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    dispatch_edge_ms: u64,
    accepted_ms: u64,
) -> bool {
    if dispatch_edge_ms >= accepted_ms {
        return false;
    }
    let (handle, owner) = match select_unkeyed_dispatch_candidate(
        ack_candidates(inner, session_idx, pane_id),
        reporter,
        reporter_manifest,
        dispatch_edge_ms,
    ) {
        UnkeyedDispatchSelection::Unique(handle, owner) => (handle, owner),
        UnkeyedDispatchSelection::Ambiguous(handles) => {
            for handle in handles {
                mark_dispatch_match_ambiguous(
                    &handle,
                    reporter,
                    reporter_manifest,
                    "hook_dispatch_ambiguous",
                );
            }
            return false;
        }
        UnkeyedDispatchSelection::None => return false,
    };
    if !fusion::set_hold_owned(inner, session_idx, pane_id, &owner, Some) {
        return false;
    }
    let state = {
        let mut st = handle.state.lock().expect("handle state lock");
        let Some(current) = st.early_ack.as_mut() else {
            return false;
        };
        if current.turn.is_some()
            || current.evidence != PendingAckEvidence::DispatchPending
            || current.edge_ms != dispatch_edge_ms
        {
            return false;
        }
        current.evidence = PendingAckEvidence::DispatchAccepted;
        current.edge_ms = accepted_ms;
        st.state
    };
    handle.ack.notify_waiters();
    match state {
        DeliveryState::Staged => {}
        DeliveryState::Submitted => {
            let recorded = match record_notification_notified(inner, &handle) {
                Ok(recorded) => recorded,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                    return false;
                }
            };
            if recorded {
                let _ = advance(
                    inner,
                    &handle,
                    &[DeliveryState::Submitted],
                    Step::to(DeliveryState::DeliveredVerified)
                        .cause("hook_dispatch_accepted_start")
                        .verified(VerifiedBy::Hook)
                        .turn_edge(accepted_ms),
                );
            }
        }
        DeliveryState::DeliveredUnverified => {
            let _ = advance(
                inner,
                &handle,
                &[DeliveryState::DeliveredUnverified],
                Step::to(DeliveryState::DeliveredVerified)
                    .cause("hook_dispatch_accepted_start")
                    .verified(VerifiedBy::Hook)
                    .turn_edge(accepted_ms),
            );
        }
        _ => {}
    }
    true
}

/// Retire receipt evidence for one exact unkeyed hook edge. The pane runtime
/// candidate is managed separately in fusion and may still report Working for
/// a later human prompt.
pub(crate) fn reject_unkeyed_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    dispatch_edge_ms: u64,
    cause: &str,
) -> usize {
    let mut rejected = 0;
    for handle in ack_candidates(inner, session_idx, pane_id) {
        if !handle.submitted_binding_is(reporter, reporter_manifest) {
            continue;
        }
        let removed = {
            let mut state = handle.state.lock().expect("handle state lock");
            let matches = state.early_ack.as_ref().is_some_and(|pending| {
                pending.turn.is_none()
                    && pending.evidence == PendingAckEvidence::DispatchPending
                    && pending.edge_ms == dispatch_edge_ms
            });
            if matches {
                state.early_ack = None;
                state.cause = Some(cause.to_string());
            }
            matches
        };
        if removed {
            rejected += 1;
            handle.ack.notify_waiters();
        }
    }
    rejected
}

/// Bind an exact dispatch to its composer barrier without publishing receipt.
///
/// Lifecycle reconciliation uses this before it updates public state. The
/// exact end can then settle the barrier in that same observation, while the
/// delivery remains unresolved until the fused state is cached and emitted.
pub(crate) fn prepare_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    turn: &crate::turnkey::TurnKey,
) -> DispatchPreparation {
    let mut result = DispatchPreparation::default();
    for handle in ack_candidates(inner, session_idx, pane_id) {
        if !handle.submitted_binding_is(reporter, reporter_manifest) {
            continue;
        }
        let pending = {
            let st = handle.state.lock().expect("handle state lock");
            st.early_ack
                .as_ref()
                .filter(|pending| {
                    pending.turn.as_ref() == Some(turn)
                        && pending.evidence == PendingAckEvidence::DispatchPending
                })
                .and_then(|pending| st.barrier.clone().map(|owner| (owner, pending.edge_ms)))
        };
        let Some((owner, start_ms)) = pending else {
            continue;
        };
        if let Some(bound) =
            fusion::bind_turn(inner, session_idx, pane_id, &owner, turn.clone(), start_ms)
        {
            result.prepared = true;
            result.end_already_present |= bound.end_already_present;
        }
    }
    result
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DispatchPreparation {
    pub(crate) prepared: bool,
    pub(crate) end_already_present: bool,
}

/// Accept pending dispatches after the exact turn has independent evidence.
///
/// The usual evidence is a later visual Working observation, cached before
/// this call. A matching terminal hook also proves that a short turn existed
/// even when the watcher missed its Working frame. The composer barrier keeps
/// either path safe while the terminal outcome is reconciled.
pub(crate) fn confirm_dispatch_ack(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    reporter: crate::identity::ProcId,
    reporter_manifest: &str,
    turn: &crate::turnkey::TurnKey,
    accepted_ms: u64,
) {
    for handle in ack_candidates(inner, session_idx, pane_id) {
        if !handle.submitted_binding_is(reporter, reporter_manifest) {
            continue;
        }
        let state = {
            let mut st = handle.state.lock().expect("handle state lock");
            let Some(pending) = st.early_ack.as_mut() else {
                continue;
            };
            if pending.turn.as_ref() != Some(turn)
                || pending.evidence != PendingAckEvidence::DispatchPending
            {
                continue;
            }
            pending.evidence = PendingAckEvidence::DispatchAccepted;
            pending.edge_ms = accepted_ms;
            st.state
        };
        handle.ack.notify_waiters();
        match state {
            DeliveryState::Staged => {}
            DeliveryState::Submitted => {
                let recorded = match record_notification_notified(inner, &handle) {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                        continue;
                    }
                };
                if recorded {
                    let moved = advance(
                        inner,
                        &handle,
                        &[DeliveryState::Submitted],
                        Step::to(DeliveryState::DeliveredVerified)
                            .cause("hook_dispatch_accepted_start")
                            .verified(VerifiedBy::Hook)
                            .turn_edge(accepted_ms)
                            .turn(Some(turn.clone())),
                    );
                    if !moved && handle.state() == DeliveryState::DeliveredUnverified {
                        let _ = advance(
                            inner,
                            &handle,
                            &[DeliveryState::DeliveredUnverified],
                            Step::to(DeliveryState::DeliveredVerified)
                                .cause("hook_dispatch_accepted_start")
                                .verified(VerifiedBy::Hook)
                                .turn_edge(accepted_ms)
                                .turn(Some(turn.clone())),
                        );
                    }
                }
            }
            DeliveryState::DeliveredUnverified => {
                let _ = advance(
                    inner,
                    &handle,
                    &[DeliveryState::DeliveredUnverified],
                    Step::to(DeliveryState::DeliveredVerified)
                        .cause("hook_dispatch_accepted_start")
                        .verified(VerifiedBy::Hook)
                        .turn_edge(accepted_ms)
                        .turn(Some(turn.clone())),
                );
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// agent.wait
// ---------------------------------------------------------------------------

/// How a wait ended. Serialized into send-and-wait entries and agent.wait
/// error data; NotDelivered only occurs in send-and-wait composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitOutcome {
    /// The target reached `until`.
    Reached,
    /// The timeout expired first.
    Timeout,
    /// The pinned pane died or its occupant changed mid-wait; resolving
    /// anything else would be a false answer about a different process.
    OccupantChanged,
    /// Legacy direct send-and-wait only: the delivery resolved somewhere
    /// other than delivered, so there is no post-delivery pane transition
    /// to observe.
    NotDelivered,
}

/// A finished wait: how it ended, the fused state it ended on, and how
/// long it actually waited.
pub(crate) struct WaitEnd {
    pub(crate) outcome: WaitOutcome,
    pub(crate) state: AgentState,
    pub(crate) waited_ms: u64,
}

/// The pane row behind a wait target: the live table while attached, the
/// frozen last-known table during a detach (frozen rows cannot false-alarm
/// the pin; the reattach re-check settles it).
fn occupant_of(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> Option<PaneRow> {
    if let Some(w) = inner.watcher_of(session_idx) {
        return w.pane(pane_id);
    }
    inner
        .session(session_idx)?
        .last_panes
        .lock()
        .expect("last panes lock")
        .get(pane_id)
        .map(|pane| pane.row.clone())
}

/// True when the pinned occupant is gone: pane missing, dead, or running
/// under a different root pid than the one pinned at wait start.
fn occupant_gone(inner: &Arc<Inner>, session_idx: usize, pane_id: &str, pinned_pid: i32) -> bool {
    match occupant_of(inner, session_idx, pane_id) {
        Some(row) => row.dead || row.pane_pid != pinned_pid,
        None => true,
    }
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
/// Pinning: (pane_id, pane_pid) recorded at start, or supplied by the
/// caller as `pinned` when the wait answers for an earlier moment (the
/// send-and-wait path pins the occupant its delivery was SUBMITTED to).
/// The pane vanishing, dying, or changing root pid resolves
/// OccupantChanged, never a false success. The watcher emits a PanePid edge
/// for a root replacement, and every other pane wake still rechecks the pin
/// as a fail-closed defense against a delayed or lagged event.
pub(crate) async fn wait_pinned(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    until: WaitUntil,
    timeout: Duration,
    working_pre: bool,
    pin: WaitPin,
) -> WaitEnd {
    let started = Instant::now();
    let deadline = started + timeout;
    let end = |outcome: WaitOutcome, state: AgentState| WaitEnd {
        outcome,
        state,
        waited_ms: started.elapsed().as_millis() as u64,
    };
    // Subscribe before the baseline refresh. A composed send also carries a
    // fact-only receiver from before Enter; this fresh receiver owns the
    // current and future state sequence, so stale state cannot replay after
    // the baseline.
    let WaitPin {
        submitted_pid: pinned,
        turn_evidence,
    } = pin;
    let mut ev_rx = inner.events.subscribe();
    let (submitted_turn_handle, handoff_working) = match turn_evidence {
        Some(mut evidence) => {
            let handoff_working =
                record_buffered_working_evidence(&mut evidence.events, &evidence.handle);
            (Some(evidence.handle), handoff_working)
        }
        None => (None, false),
    };
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
    let Some(pinned_fg) =
        fusion::foreground_pid_checked(row.pane_pid).filter(|fg| pinned.is_none_or(|p| p == *fg))
    else {
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
    let mut working_seen = working_pre
        || handoff_working
        || (submitted_turn_handle.is_none()
            && state == AgentState::Working
            && fusion::cached_working_confirmed(inner, session_idx, pane_id));
    // Test-only boundary after the fresh baseline and fact-only handoff. It
    // lets the regression prove that historical events cannot finish a newer
    // current turn before the live pane stream wakes it.
    inject_pause(inner, "post_wait_baseline").await;
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
                        if state == AgentState::Working
                            && wait_working_event_is_eligible(
                                &e,
                                submitted_turn_handle.as_ref(),
                            )
                        {
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
                    if submitted_turn_handle.is_none()
                        && state == AgentState::Working
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
                    // Test-only proof that a composed wait reached a live
                    // pane wake after its baseline. A stale historical Idle
                    // would have returned before reaching this point.
                    inject_pause(inner, "wait_pane_wake").await;
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
    let end = wait_pinned(
        inner,
        session_idx,
        &pane_id,
        params.until,
        Duration::from_millis(timeout),
        false,
        WaitPin::default(),
    )
    .await;
    // `outcome` mirrors the send-and-wait entry shape: "reached" on
    // success, and the same word the error code carries otherwise.
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
        WaitOutcome::OccupantChanged | WaitOutcome::NotDelivered => Err(WireError {
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
fn until_word(until: WaitUntil) -> &'static str {
    match until {
        WaitUntil::Idle => "idle",
        WaitUntil::TurnEnded => "turn ended",
        WaitUntil::Blocked => "blocked",
    }
}


#[cfg(test)]
mod tests;

