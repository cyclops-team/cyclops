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

pub(crate) mod gate;
pub(crate) mod inject;
pub(crate) mod terminal;
pub(crate) mod worker;

pub(crate) use gate::*;
pub use inject::*;
pub(crate) use terminal::*;
pub(crate) use worker::*;

#[cfg(test)]
use cyclops_proto::NotificationRouteEvidenceId;

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
/// Default and ceiling for agent.wait / send-and-wait timeouts.
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
#[derive(Debug, Clone)]
pub(crate) struct MailboxCapabilityProof {
    pub(crate) recipient: RecipientKey,
    pub(crate) agent: crate::identity::ProcId,
    pub(crate) manifest: String,
    pub(crate) file: PathBuf,
    pub(crate) expected_digest: [u8; 32],
}

pub(crate) struct AttemptPayload {
    pub(crate) bytes: String,
    pub(crate) transport: Option<NotificationTransport>,
    pub(crate) doorbell_format: Option<u32>,
    pub(crate) capability: Option<MailboxCapabilityProof>,
    pub(crate) capability_required: bool,
}

impl AttemptPayload {
    pub(crate) fn required_pane_width(&self) -> Option<u32> {
        None
    }
}

impl MailboxCapabilityProof {
    pub(crate) fn recheck(
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
pub(crate) fn notification_prewrite_bookend(
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

pub(crate) fn select_mailbox_capability(
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

pub(crate) fn select_attempt_payload(
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
    pub(crate) workers: StdMutex<HashMap<PaneKey, LegacyWorker>>,
    /// Active mailbox workers and their tasks, keyed by durable recipient.
    pub(crate) notification_workers: StdMutex<HashMap<RecipientKey, NotificationWorker>>,
    /// Message ids ever issued or seen in the ledgers (unique per ledger).
    pub(crate) issued: StdMutex<HashSet<String>>,
    /// Per-delivery unique tmux buffer names.
    pub(crate) buffer_seq: AtomicU64,
    /// Deliveries awaiting or upgradeable by a hook ACK, per exact route.
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

    pub(crate) fn replace_notification_run(
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
    pub(crate) fn with_legacy_worker<T, S, F>(
        &self,
        pane: PaneKey,
        spawn: S,
        action: F,
    ) -> Option<T>
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
    pub(crate) fn retire_legacy_worker(&self, pane: &PaneKey, worker: &Arc<Worker>) -> bool {
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

    pub(crate) fn legacy_worker_is_current(&self, pane: &PaneKey, worker: &Arc<Worker>) -> bool {
        self.workers
            .lock()
            .expect("workers lock")
            .get(pane)
            .is_some_and(|entry| Arc::ptr_eq(&entry.worker, worker))
    }

    /// Un-hold the pipeline and wake every worker.
    pub(crate) fn resume_workers(&self) {
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
    pub(crate) fn mint_msg_id(&self) -> String {
        self.mint_msg_id_with(|| format!("m-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]))
    }

    pub(crate) fn mint_msg_id_with(&self, mut candidate: impl FnMut() -> String) -> String {
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
    pub(crate) ledger_sessions: Vec<usize>,
    pub(crate) payload: StdMutex<String>,
    /// Durable one-shot notification facts emitted at this worker's real boundaries.
    pub(crate) notification: Option<NotificationContext>,
    /// Payload shape persisted at the notification write boundary.
    pub(crate) notification_transport: StdMutex<Option<cyclops_proto::NotificationTransport>>,
    pub(crate) state: StdMutex<HandleState>,
    pub(crate) state_tx: watch::Sender<DeliveryState>,
    /// Wakes the worker when the ACK matcher resolved this delivery.
    pub(crate) ack: Notify,
    /// Wakes a held gate after the same claim fact withdraws its attempt.
    pub(crate) cancel: Notify,
    /// Working evidence at or after this delivery's submit.
    ///
    /// The legacy composed wait uses this only to reject a working phase
    /// that predates the submit. It does not correlate a turn to a message.
    pub(crate) working_seen: AtomicBool,
    /// A receiver opened before the submit key and handed to a composed wait.
    /// It may contain older broadcasts, so the wait treats it only as a
    /// source of an exact post-submit Working fact. Its state sequence never
    /// becomes the wait's live state.
    pub(crate) post_submit_turn_events: StdMutex<Option<broadcast::Receiver<Event>>>,
    /// pane_pid of the occupant this delivery was submitted to, recorded
    /// right before the submit key. Send-and-wait pins its wait on THIS
    /// occupant, not whoever lives in the pane when the wait starts; an
    /// impostor that swaps in between must read occupant_changed, never a
    /// report about itself. 0 until a submit happened.
    pub(crate) submitted_pid: AtomicI32,
    /// The admitted AGENT identity the submit key reached, birth included
    /// so a reused pid is a different agent rather than an heir to this
    /// delivery's trust.
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
    /// Human hint carried into receipts (quota reset, attention cause).
    pub(crate) note: Option<String>,
    /// Normalized gate hold token for the in-flight head, if any.
    pub(crate) held_by: Option<String>,
    /// An acknowledgement that arrived before the delivery reached the
    /// state that consumes one.
    ///
    /// Kept HERE, under the same lock as the state, because installing it
    /// and classifying the state are ONE decision. Read the state, see
    /// `Staged`, and install afterwards, and the worker can move to
    /// `Submitted` and consume in between: the record is then written
    /// after the only read of it, and a valid acknowledgement is lost.
    pub(crate) early_ack: Option<PendingAck>,
    /// Monotonic count of direct-delivery barrier claims, including
    /// refused ones. Mailbox notifications use their durable attempt id.
    /// Separate from `attempts` because a refused claim wrote nothing and
    /// must not cost transport budget.
    pub(crate) claims: u32,
    /// Count of write-boundary refusals that wrote no pane bytes. Attempts
    /// remain append-only, so retry accounting subtracts this cumulative
    /// count rather than charging a refusal as transport work.
    pub(crate) regates: u32,
    /// Binding and capability each receive one immediate re-proof after an
    /// exact pane or readiness edge. Repeated refusal under unchanged
    /// evidence settles as a durable pre-write block.
    pub(crate) regate_reproof_used: [bool; 2],
    /// The barrier claim this delivery currently holds. Set only when a
    /// claim was granted, and compared before any later settlement so a
    /// receipt cannot release a barrier this delivery no longer owns.
    pub(crate) barrier: Option<String>,
}

/// The pre-Enter event receiver travels with the exact delivery it observed.
/// It is consumed only as a fact source by the composed `send --wait` path;
/// the wait itself starts a fresh live receiver after its baseline.
pub(crate) struct SubmittedTurnEvidence {
    pub(crate) events: broadcast::Receiver<Event>,
    pub(crate) handle: Arc<DeliveryHandle>,
}

/// Identity facts a wait must preserve from a preceding delivery.
#[derive(Default)]
pub(crate) struct WaitPin {
    pub(crate) submitted_pid: Option<i32>,
    pub(crate) turn_evidence: Option<SubmittedTurnEvidence>,
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

    pub(crate) fn barrier_owner(&self) -> String {
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

    pub(crate) fn replace_post_submit_turn_events(&self, events: broadcast::Receiver<Event>) {
        *self
            .post_submit_turn_events
            .lock()
            .expect("post-submit turn events lock") = Some(events);
    }

    pub(crate) fn take_post_submit_turn_evidence(
        self: &Arc<Self>,
    ) -> Option<SubmittedTurnEvidence> {
        self.post_submit_turn_events
            .lock()
            .expect("post-submit turn events lock")
            .take()
            .map(|events| SubmittedTurnEvidence {
                events,
                handle: Arc::clone(self),
            })
    }

    pub(crate) fn new(
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

    pub(crate) fn with_ledger_sessions(
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

    pub(crate) fn for_notification(
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
    pub(crate) fn build(
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

    pub(crate) fn restore_claimed_notification_barrier(&self) {
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
    pub(crate) fn owns_session_delivery_state(&self) -> bool {
        self.notification.is_none()
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
    /// The recipient of the delivery the ping is about. That delivery's
    /// id is the ping's own `msg_id`, so this pairs with `Some(msg_id)`.
    pub(crate) to: Option<String>,
    /// Every delivery a ping about SEVERAL of them names, when one ping
    /// covers a batch. The ping's own `msg_id` can only name one, and the
    /// restart closure ends a whole run's worth at once.
    pub(crate) deliveries: Vec<DeliveryRef>,
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

pub(crate) fn legacy_recovery_owns(message_id: &str, workspace_ids: &HashSet<String>) -> bool {
    !workspace_ids.contains(message_id)
}

/// Where a requeued delivery should go: the adopted pane for a label, in
/// the session the adoption names, provided that session is watched this
/// boot; or the name itself when it already is a pane id (such a chain
/// lives in the session file that hosted it, which is the session the
/// pane resolved into at send time). None means there is nothing to
/// requeue into and the chain closes instead.
pub(crate) fn requeue_target(
    inner: &Arc<Inner>,
    to: &str,
    hosted_idx: usize,
) -> Option<(usize, String)> {
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
pub(crate) fn gate_answers_now(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> bool {
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
pub(crate) fn initial_hold(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
) -> Option<&'static str> {
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

pub(crate) fn receipt_of(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) -> DeliveryReceipt {
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

pub(crate) fn held_by_for_position(
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
pub(crate) fn expand_recipients(
    inner: &Arc<Inner>,
    to: &[String],
) -> Result<Vec<String>, WireError> {
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
/// Result of checking the live route against its durable mailbox binding.
pub(crate) enum HandleRoute {
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
pub(crate) fn handle_route(inner: &Inner, handle: &DeliveryHandle) -> HandleRoute {
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
pub(crate) fn watcher_for_handle(
    inner: &Inner,
    handle: &DeliveryHandle,
) -> Option<Arc<SessionWatcher>> {
    match handle_route(inner, handle) {
        HandleRoute::Exact(watcher) => Some(watcher),
        HandleRoute::BindingChanged
        | HandleRoute::BindingUnprovable { .. }
        | HandleRoute::Unavailable => None,
    }
}

/// Resolve the route for a write that has not crossed the terminal boundary.
pub(crate) fn exact_prewrite_watcher(
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
pub(crate) fn until_word(until: WaitUntil) -> &'static str {
    match until {
        WaitUntil::Idle => "idle",
        WaitUntil::TurnEnded => "turn ended",
        WaitUntil::Blocked => "blocked",
    }
}

#[cfg(test)]
mod tests;
