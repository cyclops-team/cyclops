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
//! (the restart sweep over chains this file left open), `agent_wait` and
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
//! checkpoints, the decline-key spacing, the gate's single wedged-hold
//! ping, and the two deadlines a caller asked for (`receipt_block_ms`,
//! and `timeout_ms` on a wait). Nothing runs on an interval.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_manifest::{mailbox_capability, strip_csi, Manifest};
use cyclops_proto::{
    AgentState, Delivery, DeliveryReceipt, DeliveryState, Event, Kind, LedgerLine, MessageId,
    MsgSendParams, MsgSendResult, NotificationAttemptId, NotificationAttentionCause,
    NotificationTransport, NotifyLevel, ProcessInstanceId, QuiesceResult, RecipientKey, VerifiedBy,
    WaitUntil, WireError,
};
use cyclops_tmux::{ControlClient, PaneEvent, PaneRow, SessionWatcher};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, warn};

use crate::notification_adapter::{NotificationAdapterError, NotificationContext};
use crate::{daemon_line, fusion, unix_ms, Inner, PaneKey};

/// Delivery gives up on evidence this long after submit (spec: neither ACK
/// tier within 5s goes to retry_queued).
const SCREEN_ACK_DEADLINE: Duration = Duration::from_secs(5);
/// One-shot screen-evidence checkpoints after submit. Events also wake the
/// waiter; these bound the captures per delivery.
const ACK_CHECKPOINTS_MS: [u64; 5] = [250, 750, 1500, 3000, 5000];
/// Post-paste verification re-reads (paste rendering can lag a frame).
/// Offsets from the paste, one capture each; bounded per attempt.
const VERIFY_DELAYS_MS: [u64; 4] = [0, 120, 240, 480];
/// Bottom non-empty lines scanned for the staged verify pattern.
const VERIFY_REGION: usize = 15;
/// Bottom non-empty lines treated as the composer zone for the
/// marker-left-composer check.
const COMPOSER_WINDOW: usize = 6;
/// Spacing between manifest decline keys (amendment g: ~250ms).
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
const SUPERSEDED_BEFORE_WRITE: &str = "superseded_before_write";
const NOTIFICATION_RECORD_FAILED: &str = "notification_record_failed";
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
) -> Result<AttemptPayload, NotificationAdapterError> {
    let Some(notification) = &handle.notification else {
        return Ok(AttemptPayload {
            bytes: handle.payload(),
            transport: None,
            doorbell_format: None,
            capability: None,
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
    if capability.is_some() {
        return Ok(AttemptPayload {
            bytes: cyclops_proto::render_doorbell_v1(notification.message_id()),
            transport: Some(NotificationTransport::Doorbell),
            doorbell_format: Some(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM),
            capability,
        });
    }

    let message = notification.message_line()?;
    Ok(AttemptPayload {
        bytes: render_canonical_message_payload(&message),
        transport: Some(NotificationTransport::DirectPayload),
        doorbell_format: None,
        capability: None,
    })
}

/// Delivery engine state. Lives in [`Inner`]; all behavior is free
/// functions taking the daemon state so nothing here holds locks across
/// awaits by construction.
pub(crate) struct Engine {
    /// Legacy direct-delivery workers, keyed by exact watched pane route.
    workers: StdMutex<HashMap<PaneKey, Arc<Worker>>>,
    /// Canonical mailbox notification workers, keyed by durable recipient.
    notification_workers: StdMutex<HashMap<RecipientKey, Arc<Worker>>>,
    /// Worker tasks, aborted on daemon shutdown.
    pub(crate) worker_tasks: StdMutex<Vec<JoinHandle<()>>>,
    /// Message ids ever issued or seen in the ledgers (unique per ledger).
    issued: StdMutex<HashSet<String>>,
    /// Per-delivery unique tmux buffer names (amendment e).
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
}

impl Engine {
    pub(crate) fn new() -> Engine {
        Engine {
            workers: StdMutex::new(HashMap::new()),
            notification_workers: StdMutex::new(HashMap::new()),
            worker_tasks: StdMutex::new(Vec::new()),
            issued: StdMutex::new(HashSet::new()),
            buffer_seq: AtomicU64::new(0),
            acks: StdMutex::new(HashMap::new()),
            open: StdMutex::new(Vec::new()),
            notification_attempts: StdMutex::new(HashMap::new()),
            paused: AtomicBool::new(false),
        }
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

    /// Un-hold the pipeline and wake every worker.
    fn resume_workers(&self) {
        self.paused.store(false, Ordering::SeqCst);
        for worker in self.workers.lock().expect("workers lock").values() {
            worker.notify.notify_one();
        }
        for worker in self
            .notification_workers
            .lock()
            .expect("notification workers lock")
            .values()
        {
            worker.notify.notify_one();
        }
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
        let mut issued = self.issued.lock().expect("issued lock");
        loop {
            let id = format!("m-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
            if issued.insert(id.clone()) {
                return id;
            }
        }
    }
}

/// Per-recipient FIFO worker. The task sleeps on `notify` when idle.
struct Worker {
    queue: StdMutex<VecDeque<Arc<DeliveryHandle>>>,
    notify: Notify,
    busy: AtomicBool,
    /// Set when quota parking hit this recipient; carries the reset hint.
    /// Cleared only by an operator verb (M2). Never auto-retried.
    parked: StdMutex<Option<String>>,
}

impl Worker {
    /// Deliveries ahead of `handle` from the sender's point of view.
    fn position_of(&self, handle: &Arc<DeliveryHandle>) -> u32 {
        let q = self.queue.lock().expect("worker queue lock");
        let busy = self.busy.load(Ordering::SeqCst) as u32;
        match q.iter().position(|h| Arc::ptr_eq(h, handle)) {
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
    /// with `submitted_pid` it is the delivery's BINDING: the process and
    /// the vendor rules that Enter actually reached. Receipt evidence is
    /// only evidence about that binding, so a replacement occupant cannot
    /// resolve, or upgrade, a delivery it never received.
    submitted_manifest: StdMutex<Option<String>>,
}

/// A vendor acknowledgement that landed before the delivery was ready
/// for it: the edge it was taken at, and the turn it named if it named
/// one.
#[derive(Debug, Clone)]
struct EarlyAck {
    edge_ms: u64,
    turn: Option<crate::turnkey::TurnKey>,
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
    early_ack: Option<EarlyAck>,
    /// Monotonic count of direct-delivery barrier claims, including
    /// refused ones. Mailbox notifications use their durable attempt id.
    /// Separate from `attempts` because a refused claim wrote nothing and
    /// must not cost transport budget.
    claims: u32,
    /// Attempts that ended at a refused barrier claim. `attempts` is
    /// append-only history and never runs backward; this is subtracted
    /// from it to get the transport budget actually spent, because a
    /// refused claim wrote nothing.
    regates: u32,
    /// The barrier claim this delivery currently holds. Set only when a
    /// claim was granted, and compared before any later settlement so a
    /// receipt cannot release a barrier this delivery no longer owns.
    barrier: Option<String>,
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
                barrier: None,
            }),
            state_tx,
            ack: Notify::new(),
            cancel: Notify::new(),
            working_seen: AtomicBool::new(false),
            submitted_pid: AtomicI32::new(0),
            submitted_agent: StdMutex::new(None),
            submitted_at_ms: std::sync::atomic::AtomicU64::new(0),
            submitted_manifest: StdMutex::new(None),
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
    let line = LedgerLine {
        to: vec![handle.to.clone()],
        ..daemon_line(
            Kind::Gate,
            handle.msg_id.clone(),
            json!({
                "to": handle.to,
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
/// recipient's ledger; None (external admin.notify) writes to every
/// session ledger so any single-session reader sees it.
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
        None => (0..inner.session_count()).collect(),
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
/// Those windows are seconds by construction — the verify re-reads and
/// the ACK deadline — so on a healthy fleet this answers quickly.
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
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(QUIESCE_HOLD_FALLBACK_MS)).await;
                if held.engine.paused.load(Ordering::SeqCst) {
                    warn!("quiesce hold expired with no stop; resuming deliveries");
                    held.engine.resume_workers();
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
// Restart-limbo closure
// ---------------------------------------------------------------------------

/// Resolve deliveries a previous daemon run left unresolved (GOALS: limbo
/// is a bug). Runs once at boot over the replayed session ledgers, and
/// the pre-write boundary decides each chain's fate, the same boundary
/// the running pipeline retries by:
///
/// - Before the paste (queued, gating, retry_queued): nothing has touched
///   the pane, so the chain is REQUEUED — payload rebuilt from the msg
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
pub(crate) fn close_limbo(inner: &Arc<Inner>, replayed: &[(usize, Vec<LedgerLine>)]) {
    /// What `render_payload` needs to rebuild a requeued delivery's bytes,
    /// straight off the msg line.
    struct Envelope {
        from: String,
        subject: String,
        body: String,
        fyi: bool,
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
    for (idx, lines) in replayed {
        // (msg id, recipient) -> (latest state, attempts).
        let mut chains: HashMap<(String, String), (DeliveryState, u32)> = HashMap::new();
        let mut envelopes: HashMap<String, Envelope> = HashMap::new();
        for line in lines {
            if !legacy_recovery_owns(&line.id, &workspace_ids) {
                continue;
            }
            match line.kind {
                Kind::Msg | Kind::Fyi => {
                    envelopes.entry(line.id.clone()).or_insert(Envelope {
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
                            chains
                                .entry((line.id.clone(), d.to.clone()))
                                .or_insert((d.state, d.attempts));
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
                    let attempts = line.deliveries.first().map(|d| d.attempts).unwrap_or(0);
                    chains.insert((line.id.clone(), to.to_string()), (state, attempts));
                }
                _ => {}
            }
        }
        let mut dangling: Vec<((String, String), (DeliveryState, u32))> = chains
            .into_iter()
            .filter(|(_, (state, _))| !receipt_resolved(*state))
            .collect();
        dangling.sort_by(|a, b| a.0.cmp(&b.0));
        for ((id, to), (state, attempts)) in dangling {
            if receipt_is_queued(state) {
                let target = envelopes.get(&id).zip(requeue_target(inner, &to, *idx));
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
                            &[*idx],
                            &id,
                            &to,
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
                        vec![*idx],
                        payload,
                    );
                    {
                        let mut st = handle.state.lock().expect("handle state lock");
                        st.state = requeue_state;
                        st.attempts = attempts;
                    }
                    inner.engine.track(&handle);
                    let worker = worker_for(inner, sess_idx, &pane_id);
                    worker
                        .queue
                        .lock()
                        .expect("worker queue lock")
                        .push_back(handle);
                    worker.notify.notify_one();
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
                &[*idx],
                &id,
                &to,
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
    }
    if !requeued.is_empty() {
        requeued.sort();
        requeued.dedup();
        // Fyi, not action-required: these deliveries are being handled,
        // and a ping that claims a human is needed while naming nothing a
        // human can do is the contradiction M3 banned.
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
        lines.push(format!("Reply: cyclops send {from} --subject \"...\""));
    }
    lines.push(sentinel_for(msg_id));
    lines.join("\n")
}

/// Rebuild the exact direct payload from one validated canonical message row.
pub(crate) fn render_canonical_message_payload(message: &LedgerLine) -> String {
    render_payload(
        &message.id,
        &message.from,
        message.subject.as_deref().unwrap_or_default(),
        message.body.as_deref().unwrap_or_default(),
        message.kind == Kind::Fyi,
    )
}

/// The terminal sentinel: the last line of every payload.
///
/// Verification used to hunt only the leading id, which is the one token a
/// wrapped payload provably scrolls out of the bottom capture region while
/// the tail stays on screen. This token sits where the capture can always
/// see it. It is deliberately not the reply hint: transport evidence must
/// not depend on human-facing copy that changes.
pub(crate) fn sentinel_for(msg_id: &str) -> String {
    format!("[cyclops:end {msg_id}]")
}

/// Is this hook prompt the payload this delivery rendered?
///
/// A prompt that merely mentions a message id proves nothing about which
/// delivery it is. Bodies quote each other: a later message whose subject
/// or body cites an earlier id contains both, and a substring search
/// matched both. That upgraded the earlier delivery to verified on
/// evidence belonging to a different one, and a false verified fact in
/// the ledger cannot be taken back.
///
/// Framing alone is not enough either. A header and a terminal sentinel
/// still leave everything between them free, and the pre-submit race is
/// irreducible: somebody can edit the body in the composer and leave both
/// markers intact. Recording `delivered_verified` for bytes that differ
/// from the immutable ledger message is the same lie in a smaller window.
///
/// So the comparison is the whole rendered payload, which the handle
/// already owns.
///
/// Byte equality, with ONE allowance: the hook text may carry a single
/// trailing newline the payload does not, because a composer submit may
/// or may not include the closing newline.
///
/// The allowance is one-sided on purpose. The rendered payload is the
/// immutable ledger message and is never rewritten to make a comparison
/// succeed: a sender whose body deliberately contains CRLF would
/// otherwise match hook bytes that differ from what was sent.
///
/// Nothing else is normalized, and everything else fails closed: the
/// delivery stays unverified and the screen remains the evidence. That
/// includes a vendor that wraps the prompt in its own chrome. This
/// allowance is provisional and narrow until a content-free probe
/// measures what each participating vendor actually preserves; widening
/// it without that measurement would be guessing about the one comparison
/// that decides whether a receipt is honest.
pub(crate) fn prompt_matches(text: &str, payload: &str) -> bool {
    text == payload || text.strip_suffix('\n') == Some(payload)
}

/// The msg.send entry: ledger the message, fan deliveries out to per-pane
/// workers, and build receipts per DELIVERY.md semantics (block on the
/// idle path up to receipt_block_ms, immediate queued/parked otherwise).
pub(crate) async fn msg_send(
    inner: &Arc<Inner>,
    from: &str,
    params: MsgSendParams,
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
    inner.emit(
        "msg",
        json!({
            "id": msg_id,
            "from": from,
            "to": names,
            "subject": params.subject,
            "body": params.body,
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
                let worker = worker_for(inner, *session_idx, pane_id);
                let parked_hint = worker.parked.lock().expect("parked lock").clone();
                if let Some(hint) = parked_hint {
                    // Parked recipients never auto-retry; new sends park
                    // immediately with the reset hint (amendment f).
                    advance(
                        inner,
                        &handle,
                        &[DeliveryState::Queued],
                        Step::to(DeliveryState::ParkedBlockedQuota)
                            .cause("blocked_quota")
                            .note(hint),
                    );
                } else {
                    // Block for this receipt only when the worker starts on
                    // it now AND the gate answers without waiting on anyone.
                    let first_in_line = !worker.busy.load(Ordering::SeqCst)
                        && worker.queue.lock().expect("worker queue lock").is_empty();
                    let answers_now = gate_answers_now(inner, *session_idx, pane_id);
                    // The worker is woken asynchronously. Seed the head's
                    // first hold disposition before enqueueing it, so a
                    // receipt cannot race the worker between queue insertion
                    // and its first gate evaluation and report `queued · 0
                    // ahead` for a delivery already held on the target.
                    if first_in_line && !answers_now {
                        handle.set_hold(initial_hold(inner, *session_idx, pane_id));
                    }
                    worker
                        .queue
                        .lock()
                        .expect("worker queue lock")
                        .push_back(Arc::clone(&handle));
                    worker.notify.notify_one();
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
    // (DELIVERY.md), so `done` can never be satisfied by a turn that
    // predates the delivery. A delivery that ends anywhere but delivered
    // has no turn to watch; its entry reports the delivery state instead
    // of a fabricated wait result. Every entry carries the same
    // {outcome, state, waited_ms} shape agent.wait resolves with.
    if let Some(spec) = &params.wait {
        // Test seam between delivery resolution and the wait, so a test can
        // swap the pane occupant deterministically. No-op in production.
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
            let end = wait_pinned(
                inner,
                handle.session_idx,
                &handle.pane_id,
                spec.until,
                remaining,
                working_pre,
                (submitted != 0).then_some(submitted),
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
/// Everything else holds: a working pane, a human mid-keystroke, a modal
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
        .map(|w| w.position_of(handle));
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

/// Get or spawn the FIFO worker owning one pane.
fn worker_for(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> Arc<Worker> {
    let pane = PaneKey::new(session_idx, pane_id);
    let mut workers = inner.engine.workers.lock().expect("workers lock");
    if let Some(w) = workers.get(&pane) {
        return Arc::clone(w);
    }
    let worker = Arc::new(Worker {
        queue: StdMutex::new(VecDeque::new()),
        notify: Notify::new(),
        busy: AtomicBool::new(false),
        parked: StdMutex::new(None),
    });
    workers.insert(pane, Arc::clone(&worker));
    let task = tokio::spawn(worker_loop(Arc::clone(inner), Arc::clone(&worker)));
    inner
        .engine
        .worker_tasks
        .lock()
        .expect("worker tasks lock")
        .push(task);
    worker
}

/// Get or spawn the FIFO worker owning one exact mailbox recipient.
fn notification_worker_for(inner: &Arc<Inner>, recipient: RecipientKey) -> Arc<Worker> {
    let mut workers = inner
        .engine
        .notification_workers
        .lock()
        .expect("notification workers lock");
    if let Some(worker) = workers.get(&recipient) {
        return Arc::clone(worker);
    }
    let worker = Arc::new(Worker {
        queue: StdMutex::new(VecDeque::new()),
        notify: Notify::new(),
        busy: AtomicBool::new(false),
        parked: StdMutex::new(None),
    });
    workers.insert(recipient, Arc::clone(&worker));
    let task = tokio::spawn(worker_loop(Arc::clone(inner), Arc::clone(&worker)));
    inner
        .engine
        .worker_tasks
        .lock()
        .expect("worker tasks lock")
        .push(task);
    worker
}

/// Attach an already-queued mailbox notification to the pane's existing FIFO worker.
///
/// Recipient selection and oldest-pending policy belong to the coordinator.
#[allow(dead_code)]
pub(crate) fn enqueue_notification_attempt(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    display_recipient: &str,
    notification: NotificationContext,
) -> Arc<DeliveryHandle> {
    let attempt_id = notification.attempt_id();
    let recipient = notification.recipient();
    let mut active = inner
        .engine
        .notification_attempts
        .lock()
        .expect("notification attempts lock");
    active.retain(|_, handle| handle.strong_count() > 0);
    if let Some(handle) = active.get(&attempt_id).and_then(std::sync::Weak::upgrade) {
        return handle;
    }
    let doorbell = cyclops_proto::render_doorbell_v1(notification.message_id());
    let handle = DeliveryHandle::for_notification(
        display_recipient,
        pane_id,
        session_idx,
        doorbell,
        notification,
    );
    active.insert(attempt_id, Arc::downgrade(&handle));
    drop(active);
    inner.engine.track(&handle);
    let worker = notification_worker_for(inner, recipient);
    worker
        .queue
        .lock()
        .expect("worker queue lock")
        .push_back(Arc::clone(&handle));
    worker.notify.notify_one();
    handle
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

async fn worker_loop(inner: Arc<Inner>, worker: Arc<Worker>) {
    loop {
        // A quiesce holds the pipeline still: finish nothing new until
        // resume_workers notifies. Jobs stay queued (pre-paste, safe
        // across the restart the quiesce is for).
        if inner.engine.paused.load(Ordering::SeqCst) {
            worker.notify.notified().await;
            continue;
        }
        let job = worker.queue.lock().expect("worker queue lock").pop_front();
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
                    continue;
                }
                worker.busy.store(true, Ordering::SeqCst);
                process(&inner, &worker, &handle).await;
                worker.busy.store(false, Ordering::SeqCst);
            }
            None => worker.notify.notified().await,
        }
    }
}

/// Resolve the live watcher that still owns this handle's route.
///
/// Direct delivery keeps its legacy slot lookup. A mailbox notification is
/// addressable only while the same session instance remains attached in the
/// original slot and still contains the recipient's exact pane.
fn watcher_for_handle(inner: &Inner, handle: &DeliveryHandle) -> Option<Arc<SessionWatcher>> {
    let Some(notification) = &handle.notification else {
        return inner.watcher_of(handle.session_idx);
    };
    let recipient = notification.recipient();
    let session_instance_id = recipient.session_instance_id()?;
    if recipient.pane_id()?.to_string() != handle.pane_id {
        return None;
    }
    let slot = inner.session(handle.session_idx)?;
    let watcher = {
        let link = slot.link.lock().expect("session link lock");
        if !link.attached
            || link
                .identity
                .as_ref()
                .map(|identity| identity.session_instance_id())
                != Some(session_instance_id)
        {
            return None;
        }
        link.watcher.as_ref().map(Arc::clone)
    }?;
    let row = watcher.pane(&handle.pane_id)?;
    let root = crate::identity::ProcId::of(row.pane_pid)?;
    let pane_root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
    inner
        .registry
        .lock()
        .expect("registry lock")
        .for_route(recipient, pane_root)?;
    Some(watcher)
}

/// Drive one delivery through gate, inject, submit, ACK, bounded retry.
async fn process(inner: &Arc<Inner>, worker: &Arc<Worker>, handle: &Arc<DeliveryHandle>) {
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
            Err(NotificationAdapterError::SupersededBeforeWrite) => {
                // The replacement message owns the terminal supersession
                // fact. This stale handle has not touched the pane.
                return;
            }
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification gating fact failed");
                notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                return;
            }
        }
    }
    loop {
        let gate_outcome = gate(inner, handle).await;
        if let Some(notification) = &handle.notification {
            match notification.ensure_current_gating() {
                Ok(()) => {}
                Err(NotificationAdapterError::SupersededBeforeWrite) => return,
                Err(error) => {
                    error!(id = %handle.msg_id, error = %error, "notification gate outcome recheck failed");
                    notify_notification_deferred(inner, handle, NOTIFICATION_RECORD_FAILED);
                    return;
                }
            }
        }
        match gate_outcome {
            GateOutcome::Withdrawn => return,
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
            } => {
                // A quiesce that landed while this delivery was at the
                // gate: nothing may cross the paste boundary now. Park
                // pre-paste and hand the job back; it re-enters when the
                // pipeline resumes — or requeues across the restart the
                // quiesce was for.
                if inner.engine.paused.load(Ordering::SeqCst) {
                    if advance(
                        inner,
                        handle,
                        &[DeliveryState::Gating],
                        Step::to(DeliveryState::RetryQueued).cause("quiesce"),
                    ) {
                        worker
                            .queue
                            .lock()
                            .expect("worker queue lock")
                            .push_front(Arc::clone(handle));
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
                    // The replacement message records the authoritative
                    // supersession. This worker only proves the old attempt
                    // never wrote.
                    AttemptOutcome::SupersededBeforeWrite => return,
                    AttemptOutcome::Failed(failure) => {
                        // Readiness moved between the gate's proof and
                        // the write. Nothing was written and no transport
                        // was spent, so this is not a retry: it goes back
                        // to the gate, which waits on the barrier's own
                        // release rather than on a budget.
                        if failure.regate() {
                            handle.state.lock().expect("handle state lock").regates += 1;
                            // The legal path back to the gate runs through
                            // RetryQueued. The budget is what is skipped
                            // here, not the state machine.
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
                            continue;
                        }
                        if !fail_attempt(inner, handle, &failure) {
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
    SupersededBeforeWrite,
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
}

impl AttemptFailure {
    fn session_detached() -> Self {
        Self {
            cause: "session_detached".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn no_manifest() -> Self {
        Self {
            cause: "no_manifest".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn payload_unavailable() -> Self {
        Self {
            cause: "payload_unavailable".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn pane_rebound_before_paste() -> Self {
        Self {
            cause: "pane_rebound".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    /// Does this failure belong back at the gate rather than in the
    /// retry budget? True only where the cause is readiness moving under
    /// a delivery that had not yet written anything.
    fn regate(&self) -> bool {
        self.cause == "barrier_held"
            || self.cause == "binding_changed"
            || self.cause == "capability_changed"
    }

    /// The composer barrier was not this attempt's to take: somebody
    /// else's payload or a person's typing is in there. Nothing was
    /// written, so this returns to the gate.
    fn barrier_held() -> Self {
        Self {
            cause: "barrier_held".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn spool_failed() -> Self {
        Self {
            cause: "spool_failed".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn paste_failed() -> Self {
        Self {
            cause: "paste_failed".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn verify_failed() -> Self {
        Self {
            cause: "verify_failed".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn pane_rebound_after_paste() -> Self {
        Self {
            cause: "pane_rebound_after_paste".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn submit_failed() -> Self {
        Self {
            cause: "submit_failed".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    /// The pane changed hands after Enter. Terminal, and after the write
    /// boundary: the original occupant may well have received the message,
    /// so this says the outcome is unknown rather than claiming a failure.
    fn receipt_occupant_changed() -> Self {
        Self {
            cause: "receipt_occupant_changed".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn ack_timeout() -> Self {
        Self {
            cause: "ack_timeout".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    /// The durable boundary could not be advanced after the attempt crossed it.
    /// Retrying could duplicate a notification whose append outcome is unknown.
    fn notification_record_failed() -> Self {
        Self {
            cause: NOTIFICATION_RECORD_FAILED.into(),
            boundary: WriteBoundary::AfterWrite,
        }
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
            // The pane's binding moved between the proof and the write.
            // Nothing was written, and re-proving it is the gate's job.
            "binding_changed" | "capability_changed" => Self {
                cause,
                boundary: WriteBoundary::BeforeWrite,
            },
            "paste_failed" => Self::paste_failed(),
            "verify_failed" => Self::verify_failed(),
            NOTIFICATION_RECORD_FAILED => Self::notification_record_failed(),
            _ => Self {
                cause,
                boundary: WriteBoundary::AfterWrite,
            },
        }
    }
}

/// One injection attempt: paste, verify, submit, wait for an ACK tier.
///
/// The gate's admitting snapshot is re-checked against the live pane table
/// immediately before the paste and again immediately before the submit
/// key (`admitted_pid`): a pane whose occupant changed after admit (agent
/// exited to a shell, another CLI took over) must never receive the
/// payload or the Enter, because a shell occupant would EXECUTE it.
async fn attempt_delivery(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
    admitted_pid: i32,
) -> AttemptOutcome {
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
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
    let observed = watcher_for_handle(inner, handle)
        .and_then(|watcher| watcher.pane(&handle.pane_id))
        .and_then(|row| fusion::admitted_binding(inner, handle.session_idx, &row));
    let selected = match select_attempt_payload(handle, manifest, observed.as_ref()) {
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
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        injector.discard().await;
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    };
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        injector.discard().await;
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_before_paste());
    }
    // The gate's clean-composer evidence was current when it admitted, and
    // admission is a decision about a moment. A person can start typing in
    // the gap that follows, and the occupant re-check above would not
    // notice: same pane, same pid, same manifest, new draft. So the
    // readiness rule is asked again here, against a capture taken now,
    // immediately before the write that cannot be taken back.
    match fusion::recompute_pane(
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
            // Permission is a positive stamp, never the absence of a
            // refusal: an unstamped verdict means nobody decided, and
            // nobody deciding is not the same as deciding yes.
            if !det.write_ready {
                let reason = det.write_block.as_deref().unwrap_or("unstamped");
                gate_line(
                    inner,
                    handle,
                    "hold",
                    None,
                    Some(&format!("not_write_ready:{reason}")),
                );
                injector.discard().await;
                return AttemptOutcome::Failed(AttemptFailure::pane_rebound_before_paste());
            }
        }
        None => {
            injector.discard().await;
            return AttemptOutcome::Failed(AttemptFailure::session_detached());
        }
    }
    // That recompute took a capture, so who owns the pane is checked again
    // after it: otherwise the newest fact about the composer would rest on
    // an older fact about whose composer it is.
    let Some(watcher) = watcher_for_handle(inner, handle) else {
        injector.discard().await;
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    };
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        gate_line(inner, handle, "rebound", None, Some(&detail));
        injector.discard().await;
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_before_paste());
    }
    // The binding this write depends on, proven ONCE here, immediately
    // after the last capture that admitted it. Three lookups taken
    // separately can disagree with each other; this is one observation of
    // the leader, the agent and the rules that agent is running under.
    let proven = match watcher_for_handle(inner, handle)
        .and_then(|w| w.pane(&handle.pane_id))
        .and_then(|row| fusion::admitted_binding(inner, handle.session_idx, &row))
    {
        // The gate admitted under a manifest, and the live read has to
        // still agree with it: a process that exec'd in place keeps its
        // identity while becoming another program.
        Some(b) if b.manifest == manifest_id => b,
        _ => {
            gate_line(inner, handle, "rebound", None, Some("binding_unprovable"));
            injector.discard().await;
            return AttemptOutcome::Failed(AttemptFailure::pane_rebound_before_paste());
        }
    };
    // The hold is installed AT the write boundary, by the injector, not
    // before the attempt and not after it resolves. Before the attempt
    // would catch `spool_failed`, which is proven pre-write and is meant
    // to retry: a hold there would block that retry forever, with no
    // staged payload and no turn coming to clear it. After it resolves
    // would leave a window where `verify_failed` (the paste may have
    // landed, nobody could prove what it did) is visible to another
    // delivery for the same pane before anything holds it.
    let target = match selected.transport {
        Some(NotificationTransport::Doorbell) => StagingTarget::ExactRow(&selected.bytes),
        Some(NotificationTransport::DirectPayload) | None => {
            StagingTarget::Sentinel(&handle.msg_id)
        }
    };
    let (staged_window, id_staged, payload_at_proof) =
        match inject(&injector, handle, manifest, target, &|| {
            if let Some(notification) = &handle.notification {
                notification
                    .ensure_current_gating()
                    .map_err(notification_write_cause)?;
            }
            // The last thing before the pane is asked to take the
            // payload: the same binding, read again, and equal. Nothing
            // has been written yet, so a change here is the world moving
            // rather than a transport failure.
            let now = watcher_for_handle(inner, handle)
                .and_then(|w| w.pane(&handle.pane_id))
                .and_then(|row| fusion::admitted_binding(inner, handle.session_idx, &row));
            if now.as_ref() != Some(&proven) {
                return Err("binding_changed".to_string());
            }
            if matches!(selected.transport, Some(NotificationTransport::Doorbell)) {
                let current = selected.capability.as_ref().is_some_and(|proof| {
                    proof.recheck(
                        handle
                            .notification
                            .as_ref()
                            .expect("doorbell transport belongs to a notification")
                            .recipient(),
                        proven.agent,
                        &proven.manifest,
                    )
                });
                if !current {
                    return Err("capability_changed".to_string());
                }
            }
            let notification_binding = if handle.notification.is_some() {
                Some((
                    ProcessInstanceId::new(proven.leader.pid, proven.leader.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                    ProcessInstanceId::new(proven.agent.pid, proven.agent.birth)
                        .map_err(|_| NOTIFICATION_RECORD_FAILED.to_string())?,
                ))
            } else {
                None
            };
            latch_hold(inner, handle, &proven)?;
            if let (Some(notification), Some((leader, agent))) =
                (&handle.notification, notification_binding)
            {
                let transport = selected
                    .transport
                    .expect("notification attempts select a transport");
                if let Err(error) = notification.record_writing(
                    leader,
                    agent,
                    &proven.manifest,
                    transport,
                    selected.doorbell_format,
                ) {
                    rollback_unwritten_hold(inner, handle, &proven);
                    return Err(notification_write_cause(error));
                }
            }
            Ok(())
        })
        .await
        {
            Ok(v) => v,
            Err(cause) => {
                if cause == SUPERSEDED_BEFORE_WRITE {
                    return AttemptOutcome::SupersededBeforeWrite;
                }
                return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
            }
        };
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
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        // The staged payload belongs to the occupant that verified it; the
        // submit key must never reach whoever replaced it.
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    // Verification proved a representation at a moment, and Enter is sent
    // at a later one. A person can append to the staged text, or replace
    // it, in between; pressing Enter then submits something nobody
    // verified and nobody wrote. So the exact staged representation is
    // proven again here, from a capture taken now.
    let recheck = if manifest.has_escaped_rules() {
        injector.capture_escaped(&handle.pane_id).await
    } else {
        injector.capture(&handle.pane_id).await
    };
    match recheck {
        Ok(now) => {
            // Not just "something valid is staged": the SAME thing must be
            // staged. A human can replace one verified representation with
            // another between the proof and the key, and a check that only
            // asks "is this a valid staging?" would wave that through.
            let still_staged = staged_verified_target(manifest, &now, target);
            if payload_proof_target(manifest, &now, target).as_deref()
                != Some(payload_at_proof.as_str())
                || still_staged != Some(id_staged)
            {
                unregister_ack(inner, handle);
                gate_line(inner, handle, "rebound", None, Some("staging_changed"));
                return AttemptOutcome::Failed(AttemptFailure::verify_failed());
            }
        }
        Err(_) => {
            // Nobody looked, so nobody may press Enter.
            unregister_ack(inner, handle);
            gate_line(inner, handle, "rebound", None, Some("recheck_unobservable"));
            return AttemptOutcome::Failed(AttemptFailure::verify_failed());
        }
    }
    // The capture above took time, so the occupant is checked once more
    // after it. Otherwise the last thing proven about who owns the pane is
    // older than the last thing proven about what is in it.
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    // The occupant re-check just passed: admitted_pid IS the process the
    // submit key goes to. Send-and-wait pins its wait on this pid.
    handle.submitted_pid.store(admitted_pid, Ordering::SeqCst);
    // And the AGENT behind it, which is what a hook report is filed
    // under. The foreground leader can be a tool the agent handed the
    // terminal to, so the two are recorded separately and never
    // substituted for one another: the leader is terminal admission
    // evidence, the agent identity is who this delivery belongs to.
    *handle.submitted_agent.lock().expect("submitted agent lock") = Some(proven.agent);
    handle.submitted_at_ms.store(unix_ms(), Ordering::SeqCst);
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
        if let Err(error) = notification.record_submitted() {
            error!(id = %handle.msg_id, error = %error, "notification submitted fact failed");
            return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
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
    // The window this pause exists for: the delivery is Submitted and the
    // early record has not been taken yet, which is where an
    // acknowledgement arriving now has to choose between installing and
    // resolving. Always None in production.
    inject_pause(inner, "post_submit").await;
    // An acknowledgement that arrived between paste-verify and the
    // Submitted line. Taken under the state lock, which is the lock the
    // installer read the state through, so one of the two always sees the
    // other: either this take finds it, or the installer saw `Submitted`
    // and resolved the delivery itself.
    let early = handle
        .state
        .lock()
        .expect("handle state lock")
        .early_ack
        .take();
    if let Some(early) = early {
        match record_notification_notified(inner, handle) {
            Ok(true) => {}
            Ok(false) => return AttemptOutcome::Done,
            Err(error) => {
                error!(id = %handle.msg_id, error = %error, "notification receipt fact failed");
                return AttemptOutcome::Failed(AttemptFailure::notification_record_failed());
            }
        }
        if advance(
            inner,
            handle,
            &[DeliveryState::Submitted],
            Step::to(DeliveryState::DeliveredVerified)
                .cause("hook_ack")
                .verified(VerifiedBy::Hook)
                .turn_edge(early.edge_ms)
                .turn(early.turn),
        ) {
            return AttemptOutcome::Done;
        }
    }
    match await_ack(inner, handle, manifest, &staged_window, id_staged).await {
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
    let want_pid = handle.submitted_pid.load(Ordering::SeqCst);
    let want_manifest = handle
        .submitted_manifest
        .lock()
        .expect("submitted manifest lock")
        .clone();
    let (Some(want_manifest), Some(row)) = (want_manifest, watcher.pane(&handle.pane_id)) else {
        return false;
    };
    // The agent instance, not the pane's root process: a shell outlives
    // the agent that ran inside it, so pinning the root would let the
    // next agent launched at the same prompt inherit this delivery.
    if row.dead || fusion::foreground_pid(row.pane_pid) != want_pid {
        return false;
    }
    fusion::bind_manifest_for(inner, handle.session_idx, &row)
        .is_some_and(|m| m.agent.id == want_manifest)
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

/// How long that one cause waits before looking again. Short enough that
/// a transient `ps` failure costs a person nothing, long enough that a
/// permanently unreadable process table is not a spin.
const OBSERVATION_RETRY: Duration = Duration::from_millis(250);

/// Mailbox attempts have no legal pre-write terminal state. They remain in
/// workspace Gating until a pane event permits a fresh evaluation. Direct
/// deliveries retain the legacy attention and quota outcomes.
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
/// a workspace notification remains durably Gating for later reconciliation.
fn fail_attempt(
    inner: &Arc<Inner>,
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
    if should_retry(failure, spent, inner.cfg.delivery_retry_max) {
        advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::RetryQueued).cause(&failure.cause),
        )
    } else {
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
                match notification.record_attention(notification_attention_cause(&failure.cause)) {
                    Ok(_) => {}
                    Err(NotificationAdapterError::TerminalConflict(_)) => return false,
                    Err(error) => {
                        // The workspace journal remains at its last
                        // post-write state. Explicit restart recovery can
                        // close it without risking another pane write.
                        error!(id = %handle.msg_id, error = %error, "notification attention fact failed");
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
    matches!(failure.boundary, WriteBoundary::BeforeWrite) && spent <= retry_max
}

fn notify_attention(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>, cause: &str) {
    if !handle.owns_session_delivery_state() {
        // Workspace NotificationState and messages.changed own mailbox
        // attention. A delivery-scoped ping would point at the suppressed
        // session projection and could never observe operator resolution.
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
/// reset hint. Nothing here ever re-queues (amendment f).
async fn park_recipient(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
    hint: Option<String>,
) {
    if let Some(notification) = &handle.notification {
        match notification.record_quota_held() {
            Ok(_) => {}
            Err(NotificationAdapterError::SupersededBeforeWrite) => return,
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
        if fusion::quota_reset_observed_now(inner, handle.session_idx, &handle.pane_id) {
            observe_quota_reset(inner, handle.session_idx, &handle.pane_id);
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
    let drained: Vec<Arc<DeliveryHandle>> = worker
        .queue
        .lock()
        .expect("worker queue lock")
        .drain(..)
        .collect();
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
        let mut queue = worker.queue.lock().expect("worker queue lock");
        // These handles were ahead of anything enqueued after the drain.
        // Push in reverse so their original FIFO order stays intact.
        for notification in notifications.into_iter().rev() {
            queue.push_front(notification);
        }
        drop(queue);
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

/// Persist a positive quota-reset observation and expose only the explicit
/// administrator verb. This never queues a worker or moves a delivery.
pub(crate) fn observe_quota_reset(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) {
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    let Some(recipient) = inner.recipient_key(session_idx, pane_id) else {
        return;
    };
    let observed = match service.observe_quota_reset(recipient) {
        Ok(observed) => observed,
        Err(error) => {
            error!(%recipient, %error, "cannot record observed quota reset");
            return;
        }
    };
    if observed.is_empty() {
        return;
    }
    let label = service
        .identity_for_recipient(recipient)
        .ok()
        .flatten()
        .map(|identity| identity.label)
        .unwrap_or_else(|| pane_id.to_string());
    for record in observed {
        admin_notify(
            inner,
            NotifyLevel::ActionRequired,
            &format!("quota reset observed for {label}"),
            &quota_reset_notice(&record.message_id),
            Some(record.message_id.as_str()),
            Some(session_idx),
            About::delivery(&label),
        );
    }
}

fn quota_reset_notice(message_id: &MessageId) -> String {
    format!("message {message_id} remains held; run `cyclops requeue {message_id}`")
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
    },
    Park {
        hint: Option<String>,
    },
    Attention {
        cause: String,
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
/// fused state (quota park, modal decline-or-hold, working hold,
/// idle_with_input hold, idle proceed). Event-driven: holds wake on fused
/// state changes, pane field changes, and session reattach. The recompute
/// that admits a delivery runs immediately before pasting, so the gate
/// snapshot is fresher than any human keystroke round-trip.
async fn gate(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) -> GateOutcome {
    let mut declines: HashMap<String, u32> = HashMap::new();
    let mut notified_rules: HashSet<String> = HashSet::new();
    let mut last_hold: Option<String> = None;
    // One-shot visibility for wedged holds: a delivery held in gating past
    // the configured threshold pings the admin exactly once.
    let mut hold_since: Option<Instant> = None;
    let mut hold_notified = false;
    'gate: loop {
        // Subscribe before evaluating so nothing published mid-evaluation
        // is lost; evaluation itself is authoritative.
        let mut ev_rx = inner.events.subscribe();
        let watcher = watcher_for_handle(inner, handle);
        let mut pane_rx = watcher.as_ref().map(|w| w.subscribe());

        if let Some(notification) = &handle.notification {
            match notification.ensure_current_gating() {
                Ok(()) => {}
                Err(NotificationAdapterError::SupersededBeforeWrite) => {
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

        let hold = match &watcher {
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
                    let Some(manifest) = fusion::bind_manifest_for(inner, handle.session_idx, &row)
                    else {
                        if let Some(hold) = workspace_prewrite_hold(handle, "no_manifest") {
                            break 'pane Some(hold);
                        }
                        return GateOutcome::Attention {
                            cause: "no_manifest".to_string(),
                        };
                    };
                    let manifest_id = manifest.agent.id.clone();
                    let Some(det) = fusion::recompute_pane(
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
                    match det.state {
                        AgentState::Idle => {
                            // Runtime idle is not permission to write
                            // (INVARIANTS rule 12). A turn-end hook can put
                            // the pane in idle while the composer holds a
                            // staged payload the screen sensor could not
                            // read; proceeding there pastes over it.
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
                                    match fusion::foreground_pid_checked(row.pane_pid) {
                                        None => Some("occupant_unprovable".to_string()),
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
                                            };
                                        }
                                    }
                                }
                                // Hold on an event, never a clock: the next
                                // pane change re-evaluates, and a screen
                                // sensor that can see the composer resolves
                                // it without anyone pasting blind.
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
                                        && *declines.get(&r.id).unwrap_or(&0) < MAX_DECLINES =>
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
                        AgentState::Working => Some("working".to_string()),
                        // Human typing always wins.
                        AgentState::IdleWithInput => Some("idle_with_input".to_string()),
                        AgentState::Unknown => Some("unknown".to_string()),
                    }
                }
            }
        };
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
            let retry_at = unprovable.then(|| Instant::now() + OBSERVATION_RETRY);
            tokio::select! {
                _ = wait_pane_change(
                    &mut ev_rx,
                    pane_rx.as_mut(),
                    handle.session_idx,
                    &handle.pane_id,
                    &handle.cancel,
                ) => {}
                _ = async {
                    match retry_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
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
                }
            }
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
        "blocked_quota" => "blocked_quota",
        "unknown" => "unknown",
        c if c.split(':').next() == Some("blocked") => "blocked",
        // Rule 12: idle by runtime state, but nothing proved the composer
        // was clean. Receipts say so plainly; the exact reason stays on
        // the gate ledger line.
        c if c.split(':').next() == Some("not_write_ready") => "not_write_ready",
        _ => "unknown",
    }
}

/// Manifest decline keys, in order, with spacing (amendment g: the keys
/// come from the manifest rule, never a generic Enter/Escape).
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

/// "Resets in 135h57m42s" -> "resets in 135h57m42s".
pub(crate) fn parse_reset_hint(screen: &str) -> Option<String> {
    let idx = screen.find("esets in ")?;
    let tail = &screen[idx + "esets in ".len()..];
    let token: String = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if token.is_empty() {
        None
    } else {
        Some(format!("resets in {token}"))
    }
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

fn notification_write_cause(error: NotificationAdapterError) -> String {
    match error {
        NotificationAdapterError::SupersededBeforeWrite => SUPERSEDED_BEFORE_WRITE.to_string(),
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
        Ok(_) => {
            if handle.notification_transport() == Some(NotificationTransport::DirectPayload) {
                notification.record_delivered_direct()?;
                if let Some(service) = &inner.mailbox {
                    if let Err(error) = crate::messaging::schedule_recipient(
                        inner,
                        service,
                        notification.recipient(),
                    ) {
                        error!(
                            id = %handle.msg_id,
                            recipient = %notification.recipient(),
                            %error,
                            "direct delivery settled but the next mailbox item could not be scheduled"
                        );
                    }
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
                fusion::bind_turn(
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

fn latch_turn_started(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    since_ms: u64,
) {
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
    });
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
) {
    match pane_rx {
        Some(prx) => loop {
            tokio::select! {
                ev = ev_rx.recv() => if event_wakes(&ev, session_idx, pane_id) { return },
                pe = prx.recv() => if pane_event_wakes(&pe, pane_id) { return },
                _ = cancel.notified() => return,
            }
        },
        None => loop {
            tokio::select! {
                ev = ev_rx.recv() => if event_wakes(&ev, session_idx, pane_id) { return },
                _ = cancel.notified() => return,
            }
        },
    }
}

fn event_wakes(
    ev: &Result<Event, broadcast::error::RecvError>,
    session_idx: usize,
    pane_id: &str,
) -> bool {
    match ev {
        Ok(e) => match e.event.as_str() {
            // A readiness change with no state change is exactly the
            // shape of a hold lifting, and it is the whole reason this
            // arm exists: without it a delivery sleeps through its own
            // release.
            "state" | "readiness" => {
                e.data["pane_id"] == pane_id && e.data["session_idx"] == session_idx
            }
            "session" => true,
            _ => false,
        },
        Err(broadcast::error::RecvError::Lagged(_)) => true,
        Err(broadcast::error::RecvError::Closed) => true,
    }
}

fn pane_event_wakes(pe: &Result<PaneEvent, broadcast::error::RecvError>, pane_id: &str) -> bool {
    match pe {
        Ok(PaneEvent::PaneChanged { id, .. }) => id == pane_id,
        Ok(PaneEvent::PaneRemoved(id)) => id == pane_id,
        Ok(PaneEvent::Disconnected) => true,
        Ok(_) => false,
        Err(broadcast::error::RecvError::Lagged(_)) => true,
        Err(broadcast::error::RecvError::Closed) => true,
    }
}

// ---------------------------------------------------------------------------
// Inject and verify
// ---------------------------------------------------------------------------

/// How payload bytes reach an agent and how the backend reads them back
/// (amendment i; GOALS: delivery behind an adapter). The gate, verify, and
/// ACK layers above call through this seam only, so a headless protocol
/// backend slots in per agent without touching them. [`TmuxInjector`] is
/// the M1 implementation. Errors are the short cause codes retry
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
    /// which is the write boundary: everything before it is provably
    /// retryable, and everything from it onward may have left text in
    /// somebody's composer.
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
    ) -> Result<(), String>;

    /// Drop a spooled payload the attempt is not going to write.
    async fn discard(&self);
    /// Press the submit key.
    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String>;
    /// Read back the visible grid (this backend's verification sensor).
    async fn capture(&self, pane_id: &str) -> Result<String, String>;
    /// Read back the grid with SGR escapes (capture-pane -e). Verification
    /// takes this flavor when the manifest's composer discriminators are
    /// `line_regex_esc` clauses (codex.toml), which a plain capture can
    /// never satisfy.
    async fn capture_escaped(&self, pane_id: &str) -> Result<String, String>;
}

/// The tmux paste path: load-buffer through the adapter's private spool
/// (0600 file under the 0700 cyclops home, never the shared temp dir) into
/// a per-delivery unique buffer (amendment e), paste-buffer -p (bracketed
/// when the app opted in, F17) -d so the buffer does not linger
/// server-global, send-keys for submit.
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
            // pre-write budget; only paste-buffer failures are ambiguous.
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
    ) -> Result<(), String> {
        // The write boundary. Spooling is behind us and provably touched
        // no pane; the next call may put text in somebody's composer, and
        // its failure is ambiguous about whether it did. Whatever this
        // hook installs has to be installed BEFORE the await, not after
        // it returns, or an outcome that leaves a payload behind can be
        // acted on by another delivery first.
        //
        // A hook that cannot install it stops the write. Nothing has been
        // pasted at this point, so refusing is the cheap direction: the
        // buffer is dropped and the delivery retries under the pre-write
        // budget.
        if let Err(cause) = on_write() {
            let _ = self.client.delete_buffer(&self.buffer).await;
            return Err(cause);
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
            return Err("paste_failed".to_string());
        }
        Ok(())
    }

    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String> {
        self.client.send_keys(pane_id, &[key]).await.map_err(|e| {
            warn!(error = %e, "submit key failed");
            "submit_failed".to_string()
        })
    }

    async fn capture(&self, pane_id: &str) -> Result<String, String> {
        self.client
            .capture_pane(pane_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn capture_escaped(&self, pane_id: &str) -> Result<String, String> {
        self.client
            .capture_pane_escaped(pane_id)
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
/// Composer verification is the gate (amendment b): bracketed-paste
/// degradation is not observable up front through tmux 3.6a.
async fn inject<I: Injector>(
    injector: &I,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    target: StagingTarget<'_>,
    on_write: &(dyn Fn() -> Result<(), String> + Sync),
) -> Result<(String, bool, String), String> {
    injector.commit(&handle.pane_id, on_write).await?;
    // The capture flavor follows the manifest's composer discriminators:
    // esc rules need the SGR-escaped grid or they fail closed, and a
    // composer that collapses a long paste into a "[Pasted Content …]"
    // chip leaves the escaped composer line as the ONLY thing that can
    // verify the staging (the message id is hidden inside the chip).
    let escaped = manifest.has_escaped_rules();
    let mut last_delay = 0;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        let capture = if escaped {
            injector.capture_escaped(&handle.pane_id).await
        } else {
            injector.capture(&handle.pane_id).await
        };
        match capture {
            Ok(screen) => {
                if let Some(id_staged) = staged_verified_target(manifest, &screen, target) {
                    // The comparison window is de-escaped text either way,
                    // so SGR churn (a blink, a focus change) can never fake
                    // a "changed composer" for the ACK tier.
                    return Ok((
                        bottom_window(&strip_csi(&screen), COMPOSER_WINDOW),
                        id_staged,
                        payload_proof_target(manifest, &screen, target).unwrap_or_default(),
                    ));
                }
            }
            Err(e) => debug!(error = %e, "verify capture failed"),
        }
    }
    Err("verify_failed".to_string())
}

/// Did this capture prove the paste staged? `Some(id_matched)` when it
/// did, `None` when nothing on screen proves it.
///
/// Two kinds of evidence, tried in order, and the order is the point:
///
/// 1. The terminal sentinel, proven to be the last payload row of the
///    active composer. This is the only evidence that covers the whole
///    payload, because the sentinel is the last byte written.
/// 2. A composer-pinned generic pattern, for a vendor that collapsed the
///    paste into a chip. A chip hides the id and the sentinel alike, so
///    the chip on the composer line is all that survives it.
///
/// Verify that the staged target (sentinel or exact row) or a collapsed chip
/// is staged in the active composer.
pub(crate) fn staged_verified_target(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
) -> Option<bool> {
    match target {
        StagingTarget::Sentinel(msg_id) => {
            if sentinel_verified(manifest, screen, msg_id) {
                return Some(true);
            }
            if marker_in_composer(manifest, screen) {
                return Some(false);
            }
            None
        }
        StagingTarget::ExactRow(expected_row) => {
            if exact_row_verified(manifest, screen, expected_row) {
                return Some(true);
            }
            if marker_in_composer(manifest, screen) {
                return Some(false);
            }
            None
        }
    }
}

#[cfg(test)]
fn staged_verified(
    manifest: &Manifest,
    screen: &str,
    _id_patterns: &[String],
    _other_patterns: &[String],
    msg_id: &str,
) -> Option<bool> {
    staged_verified_target(manifest, screen, StagingTarget::Sentinel(msg_id))
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
    // Full span on the plain row, generically: a manifest that forgot an
    // anchor would otherwise accept trailing payload on a chrome row, and
    // no vendor should be able to weaken terminality by omission. The
    // escaped half supplies the style evidence, where a partial match is
    // meaningful because SGR runs surround the text.
    let matches = |i: usize, raw: &str, plain: &str| {
        whole_row(&layout[i], plain) && layout_esc[i].is_match(raw)
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
    // A plain capture where the escaped one is required cannot answer.
    if !screen.contains('\u{1b}') {
        return None;
    }
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
    // The token row is matched RAW, with only the terminal's own trailing
    // padding removed. The measured sentinel row carries no styling at
    // all, and normalizing before comparing is what lets bytes ride along
    // behind it: a torn `ESC [` swallows the rest of the line and a
    // complete sequence is removed outright, so either one reduces the
    // sentinel plus arbitrary content to the exact token. This row proves
    // nothing follows it, so nothing may.
    //
    // KNOWN LIMIT, measured on tmux 3.6a: the default capture erases
    // trailing spaces a person typed, exactly as it erases the grid's own
    // padding, so spaces after the token are not distinguishable from
    // padding by any capture this takes. Every other trailing code point
    // is content and refuses. Closing the space case needs the composer
    // endpoint observed independently, bound to this same snapshot.
    let hits: Vec<usize> = window
        .iter()
        .enumerate()
        .filter(|(_, (raw, plain))| *plain == want && *raw == want)
        .map(|(i, _)| i)
        .collect();
    let [at] = hits[..] else {
        return None;
    };
    if !trailer_follows(manifest, &window[at + 1..]) {
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

    let hits: Vec<usize> = window
        .iter()
        .enumerate()
        .filter(|(_, (raw, plain))| {
            let p = unpad(plain);
            let r = unpad(raw);
            if idle_rules.is_empty() {
                return p == want || r == want;
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
        .collect();

    let [at] = hits[..] else {
        return None;
    };

    if !trailer_follows(manifest, &window[at + 1..]) {
        return None;
    }

    // Prove the doorbell is the COMPLETE active composer content:
    // No preceding row in the search window may match an IdleWithInput composer rule.
    if !idle_rules.is_empty() {
        for (raw, plain) in &window[..at] {
            if idle_rules.iter().any(|rule| rule.matches_row(plain, raw)) {
                return None;
            }
        }
    }

    Some(window[at].1.trim().to_string())
}

pub(crate) fn exact_row_verified(manifest: &Manifest, screen: &str, expected_row: &str) -> bool {
    exact_row_proof(manifest, screen, expected_row).is_some()
}

/// Substituted staging patterns, split into id-carrying (contain the
/// message id after substitution) and generic. The id is always an
/// id-carrying pattern.
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
/// until observability returns (detach-aware ACKs, v1.1 amendment 2).
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

/// The per-delivery ACK timeline: the tier-1 hook window, the tier-2
/// screen-evidence checkpoints, and the give-up deadline.
///
/// Detach-aware (DELIVERY.md v1.1 amendment 2): while the session's
/// control connection is down the daemon cannot observe the pane, so the
/// clock freezes; on reattach every remaining instant shifts by the outage
/// duration. Time lost to a detach never counts against an ACK window.
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
async fn await_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staged_window: &str,
    id_staged: bool,
) -> AckOutcome {
    let submit_at = Instant::now();
    let tier1 = manifest.hooks.ack.is_some() && manifest.hooks.ack_payload_field.is_some();
    let (id_patterns, other_patterns) = verify_patterns(manifest, &handle.msg_id);
    let patterns: Vec<String> = id_patterns.into_iter().chain(other_patterns).collect();
    let mut ev_rx = inner.events.subscribe();
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
        if handle.state() == DeliveryState::DeliveredVerified {
            return AckOutcome::Resolved;
        }
        let target = clock.next_target();
        tokio::select! {
            _ = handle.ack.notified() => {
                if handle.state() == DeliveryState::DeliveredVerified {
                    return AckOutcome::Resolved;
                }
            }
            _ = tokio::time::sleep_until(target.map(|(t, _)| t).unwrap_or_else(Instant::now)),
                if target.is_some() =>
            {
                let now = Instant::now();
                if target.is_some_and(|(_, hook_end)| hook_end) {
                    // Tier-1 window over: the delivery downgrades to screen
                    // evidence. On a pane that has NEVER produced a hook
                    // edge this is the F1 signature (configuration does not
                    // equal subscription); the admin hears about it once.
                    if tier1 {
                        crate::selftest::notify_f1_once(
                            inner,
                            &handle.msg_id,
                            &handle.to,
                            &handle.pane_id,
                            handle.session_idx,
                            &manifest.agent.id,
                        );
                    }
                    clock.end_hook_phase(now);
                    continue;
                }
                clock.advance_checkpoint();
                let evidence = screen_evidence(
                    inner, handle, manifest, &patterns, staged_window,
                    id_staged, working_seen, output_seen,
                ).await;
                match checkpoint_step(evidence, clock.expired(Instant::now())) {
                    CheckpointStep::Deliver => return AckOutcome::Screen,
                    CheckpointStep::Expire => return AckOutcome::Timeout,
                    CheckpointStep::Rebound => return AckOutcome::Rebound,
                    CheckpointStep::Freeze => {
                        // The pass could not look (watcher cleared before
                        // its detach event, or the capture failed): a
                        // Timeout here would stand on nothing. Freeze; a
                        // session edge, pane activity, or a lag reconcile
                        // unfreezes.
                        clock.freeze(Instant::now());
                    }
                    CheckpointStep::Wait => {}
                }
            }
            // Reattach/detach truth for THIS session comes from
            // `inner.watcher_of(handle.session_idx)`, resolved fresh here,
            // never from matching a "session" event's own `data["name"]`
            // against a name captured at function entry: a followed
            // rename (`PaneEvent::SessionRenamed`, `rename_session_slot`
            // in lib.rs) changes the live name mid-wait, and a stale
            // snapshot then never matches an attach OR a detach line
            // again — the clock freezes on the FIRST outage and never
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
                            match screen_evidence(
                                inner, handle, manifest, &patterns, staged_window,
                                id_staged, working_seen, output_seen,
                            ).await {
                                Evidence::Confirmed => return AckOutcome::Screen,
                                // Still blind right after the edge: stay
                                // frozen rather than letting the shifted
                                // deadlines run on an unobserved pane.
                                Evidence::Unobservable => clock.freeze(Instant::now()),
                                // Nothing this pane does now speaks for
                                // the delivery: stop waiting for it to.
                                Evidence::Rebound => return AckOutcome::Rebound,
                                Evidence::Absent => {}
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
                match pe {
                    Ok(PaneEvent::OutputActivity { pane_id: p, .. }) if p == handle.pane_id => {
                        // Output is a CUE to look, not evidence in itself.
                        // %output carries a pane id and bytes, never the
                        // pid that wrote them, and the watcher table can
                        // still hold the previous occupant when a
                        // replacement speaks first. Attributing those
                        // bytes to this delivery would be a guess, so the
                        // look below is what decides, and it checks the
                        // binding on both sides of its capture.
                        // A frozen clock with a live pane stream is doubt
                        // from a failed capture; the pane speaking again
                        // is the cue to look and, if observable, resume.
                        if clock.frozen() {
                            match screen_evidence(
                                inner, handle, manifest, &patterns, staged_window,
                                id_staged, working_seen, output_seen,
                            ).await {
                                Evidence::Confirmed => return AckOutcome::Screen,
                                Evidence::Absent => clock.unfreeze(Instant::now()),
                                Evidence::Rebound => return AckOutcome::Rebound,
                                Evidence::Unobservable => {}
                            }
                        }
                    }
                    Ok(PaneEvent::Disconnected)
                    | Err(broadcast::error::RecvError::Closed) => {
                        pane_rx = None;
                        clock.freeze(Instant::now());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// True when the event is a working fused-state change for this pane.
fn track_state_event(
    ev: &Result<Event, broadcast::error::RecvError>,
    handle: &Arc<DeliveryHandle>,
) -> bool {
    let Ok(e) = ev else { return false };
    if e.event != "state" || e.data["pane_id"] != handle.pane_id.as_str() {
        return false;
    }
    if e.data["state"] != "working" {
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
    let Some(birth) = e.data["source_birth"].as_u64() else {
        return false;
    };
    let agent = crate::identity::ProcId {
        pid: e.data["source_pid"].as_i64().unwrap_or_default() as i32,
        birth,
    };
    let manifest = e.data["source_manifest"].as_str().unwrap_or_default();
    handle.submitted_binding_is(agent, manifest)
}

/// True when the event is a session lifecycle line — attach, detach, or
/// this daemon's own rename bookkeeping riding the same "session" name
/// (`session_lifecycle`, lib.rs). Which one, and whether it is about THIS
/// caller's session at all, is deliberately not decided here: see the doc
/// comment on `await_ack`'s event arm for why comparing against
/// `inner.watcher_of(session_idx)`'s live truth, not the event's own
/// `data["name"]`, is what a caller does with this.
fn is_session_event(ev: &Result<Event, broadcast::error::RecvError>) -> bool {
    matches!(ev, Ok(e) if e.event == "session")
}

/// Screen evidence for tier 2, spec conjunctive form (v1.1 amendment 1):
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
#[allow(clippy::too_many_arguments)]
async fn screen_evidence(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    patterns: &[String],
    staged_window: &str,
    id_staged: bool,
    working_seen: bool,
    output_seen: bool,
) -> Evidence {
    let Some(watcher) = inner.watcher_of(handle.session_idx) else {
        return Evidence::Unobservable;
    };
    // Same capture flavor the staging verify used, so the marker check can
    // pin the composer line through esc-only discriminators and the window
    // The binding is checked on both sides of the read: a capture is not
    // instantaneous, and evidence about a pane whose occupant changed
    // while it was being read is evidence about nobody.
    if !submitted_binding_holds(inner, &watcher, handle) {
        return Evidence::Rebound;
    }
    // comparison is against like text (staged_window is de-escaped).
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
    let _ = patterns;
    let marker_present = sentinel_verified(manifest, &screen, &handle.msg_id)
        || marker_in_composer(manifest, &screen);
    if !marker_present && tier2_evidence(changed, id_staged, working_seen, output_seen) {
        Evidence::Confirmed
    } else {
        Evidence::Absent
    }
}

/// The tier-2 turn-evidence rule, factored for the unit test: a changed
/// window alone is only evidence when the id demonstrably staged.
fn tier2_evidence(changed: bool, id_staged: bool, working_seen: bool, output_seen: bool) -> bool {
    let _ = output_seen;
    working_seen || (changed && id_staged)
}

/// Did the vendor collapse this paste into its chip, on the composer row?
///
/// The chip is the alternate representation: it hides the message id and
/// the sentinel alike, so nothing else on screen can prove the staging.
/// Proving the chip therefore means matching the row the vendor actually
/// draws, in both plain and escaped form, on a row a composer rule pins.
///
/// It used to be a substring test against the generic `verify_pattern`
/// entries, which is why this is written the long way now: a message
/// whose own subject contained the word "Pasted" satisfied it, and a
/// truncated payload whose sentinel never arrived submitted itself.
/// What the submit key is bound to: the staged representation exactly as
/// the proof that validated it saw, and nothing else.
///
/// The boundary is taken from that proof rather than from pattern
/// matching. Dropping rows because they look like chrome would hand the
/// collision adversary a way in: a payload row reading like a status row
/// would vanish from the comparison, and a human edit to it with it.
///
/// Chrome is excluded because chrome animates. Claude counts context
/// down, codex counts a turn up, and binding rows that change on their
/// own would refuse every delivery slower than a tick.
pub(crate) fn payload_proof_target(
    manifest: &Manifest,
    screen: &str,
    target: StagingTarget<'_>,
) -> Option<String> {
    match target {
        StagingTarget::Sentinel(msg_id) => {
            sentinel_proof(manifest, screen, msg_id).or_else(|| chip_proof(manifest, screen))
        }
        StagingTarget::ExactRow(expected_row) => {
            exact_row_proof(manifest, screen, expected_row).or_else(|| chip_proof(manifest, screen))
        }
    }
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
    if chip_proof(manifest, screen).is_some() {
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
            (captured_content(continuation, plain) == Some(want_sentinel.as_str()))
                .then_some(start + offset)
        })
        .collect();
    let &[sentinel_at] = sentinel_hits.as_slice() else {
        return ComposerContentProof::Unprovable;
    };
    if !trailer_follows(manifest, &rows[sentinel_at + 1..]) {
        return ComposerContentProof::Unprovable;
    }

    let want_header = format!("[cyclops {msg_id}] FROM:");
    let headers: Vec<(usize, &str)> = rows[..=sentinel_at]
        .iter()
        .enumerate()
        .filter_map(|(at, (_, plain))| {
            captured_content(prompt, plain)
                .filter(|content| content.starts_with(&want_header))
                .map(|content| (at, content))
        })
        .collect();
    let [(prompt_at, first)] = headers.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let mut content = vec![(*first).to_string()];
    let mut sentinel_count = 0;
    for (_, plain) in &rows[prompt_at + 1..=sentinel_at] {
        if captured_content(prompt, plain).is_some() {
            return ComposerContentProof::Unprovable;
        }
        if plain.is_empty() {
            content.push(String::new());
        } else if let Some(line) = captured_content(continuation, plain) {
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
    if chip_proof(manifest, screen).is_some() {
        return ComposerContentProof::Hidden;
    }
    let (Some(prompt), Some(continuation)) = (
        manifest.composer_prompt.as_ref(),
        manifest.composer_continuation.as_ref(),
    ) else {
        return ComposerContentProof::Unsupported;
    };
    let idle_rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|rule| rule.state == AgentState::IdleWithInput)
        .collect();
    if idle_rules.is_empty() {
        return ComposerContentProof::Unprovable;
    }

    let rows = joined_composer_rows(screen);
    let start = rows.len().saturating_sub(VERIFY_REGION);
    let window = &rows[start..];
    let prompts: Vec<(usize, &str)> = window
        .iter()
        .enumerate()
        .filter_map(|(at, (raw, plain))| {
            let content = captured_content(prompt, plain)?;
            idle_rules
                .iter()
                .any(|rule| rule.matches_row(plain, raw))
                .then_some((at, content))
        })
        .collect();
    let [(prompt_at, first)] = prompts.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let trailers: Vec<usize> = (prompt_at + 1..window.len())
        .filter(|at| trailer_follows(manifest, &window[*at..]))
        .collect();
    let [trailer_at] = trailers.as_slice() else {
        return ComposerContentProof::Unprovable;
    };

    let mut content = vec![(*first).to_string()];
    for (_, plain) in &window[prompt_at + 1..*trailer_at] {
        if captured_content(prompt, plain).is_some() {
            return ComposerContentProof::Unprovable;
        }
        let Some(line) = captured_content(continuation, plain) else {
            return ComposerContentProof::Unprovable;
        };
        content.push(line.to_string());
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
    chip_proof(manifest, screen).is_some()
}

/// The proven chip row, when the collapsed representation validates.
///
/// Equality against this row is equality of the SCREEN representation and
/// nothing more: the payload behind a chip is not on screen, so it cannot
/// be compared. That is the same limit the directive accepts by keeping
/// the chip as an alternate at all.
fn chip_proof(manifest: &Manifest, screen: &str) -> Option<String> {
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
            st.early_ack = Some(EarlyAck {
                edge_ms,
                turn: turn.clone(),
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
    /// Send-and-wait only: the delivery resolved somewhere other than
    /// delivered, so there is no turn to watch.
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
/// state; done is the working -> idle edge, so the CURRENT turn (already
/// working, or `working_pre` from a delivery handle) or the NEXT turn
/// counts, and a blocked state mid-turn keeps waiting rather than passing
/// as done.
///
/// Pinning: (pane_id, pane_pid) recorded at start, or supplied by the
/// caller as `pinned` when the wait answers for an earlier moment (the
/// send-and-wait path pins the occupant its delivery was SUBMITTED to).
/// The pane vanishing, dying, or changing root pid resolves
/// OccupantChanged, never a false success. pane_pid changes are silent in
/// the watcher (no PaneField), so the pin is re-read on every wake for
/// this pane, including output activity: a swapped occupant that neither
/// prints nor changes any pane field stays undetected until it does, the
/// same residual window the delivery pipeline accepts.
pub(crate) async fn wait_pinned(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    until: WaitUntil,
    timeout: Duration,
    working_pre: bool,
    pinned: Option<i32>,
) -> WaitEnd {
    let started = Instant::now();
    let deadline = started + timeout;
    let end = |outcome: WaitOutcome, state: AgentState| WaitEnd {
        outcome,
        state,
        waited_ms: started.elapsed().as_millis() as u64,
    };
    // Subscribe before the first read so no edge can fall between.
    let mut ev_rx = inner.events.subscribe();
    let mut pane_rx = inner.watcher_of(session_idx).map(|w| w.subscribe());
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
    // root check it always had, which is there for a silent respawn.
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
    let mut working_seen = working_pre || state == AgentState::Working;
    loop {
        if state == AgentState::Dead {
            return end(WaitOutcome::OccupantChanged, state);
        }
        let satisfied = match until {
            WaitUntil::Idle => state == AgentState::Idle,
            WaitUntil::Blocked => state.is_blocked(),
            WaitUntil::Done => {
                working_seen && matches!(state, AgentState::Idle | AgentState::IdleWithInput)
            }
        };
        if satisfied {
            // The state edge says the turn ended. It does not say WHOSE
            // turn, so the identity is re-proven before the wait calls it
            // reached.
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
                        if state == AgentState::Working {
                            working_seen = true;
                        }
                    }
                }
                // Attach/detach truth for THIS session comes from
                // `inner.watcher_of(session_idx)`, resolved fresh here,
                // never from matching this event's own `data["name"]`
                // against a name captured at entry: a followed rename
                // changes the live name mid-wait, and a stale snapshot
                // then never matches an attach line again — see the doc
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
                    if state == AgentState::Working {
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
                    // Output is the one signal a silent pid swap gives off
                    // (respawn-pane updates the row without a PaneChanged).
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
    let end = wait_pinned(
        inner,
        session_idx,
        &pane_id,
        params.until,
        Duration::from_millis(timeout),
        false,
        None,
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
        WaitUntil::Done => "done",
        WaitUntil::Blocked => "blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use cyclops_proto::{
        MessageId, MessagePresentation, NotificationAttemptId, NotificationState, RecipientKey,
        RecipientPresentation, SessionInstanceId, TmuxPaneId, WorkspaceId,
    };
    use cyclops_state::StateRoot;

    use crate::mailbox::{MessageDraft, MessageStore};

    struct NotificationScratch(PathBuf);

    impl Drop for NotificationScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn notification_fixture(
        tag: &str,
    ) -> (
        NotificationScratch,
        Arc<StdMutex<MessageStore>>,
        NotificationContext,
        Arc<DeliveryHandle>,
        RecipientKey,
    ) {
        let path = cyclops_proto::scratch::scratch_dir(&format!(
            "notification-adapter-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        let root = StateRoot::open_or_create(&path).unwrap();
        let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
        let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
        let recipient =
            RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
        let admin = RecipientKey::admin(workspace);
        let message_id = MessageId::new(format!("m-{tag}")).unwrap();
        let attempt_id = NotificationAttemptId::generate();
        let mut store = MessageStore::open(
            &root,
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot",
        )
        .unwrap();
        store
            .accept(
                message_id.clone(),
                MessageDraft {
                    kind: Kind::Msg,
                    sender: admin,
                    recipients: vec![recipient],
                    subject: Some("Wake".into()),
                    body: Some("Review the mailbox".into()),
                    client_key: None,
                    supersedes: None,
                    presentation: MessagePresentation {
                        sender_label: "admin".into(),
                        recipient_labels: vec![RecipientPresentation {
                            recipient,
                            label: "reviewer".into(),
                        }],
                    },
                },
            )
            .unwrap();
        store
            .queue_notification(message_id.clone(), recipient, attempt_id)
            .unwrap();
        let store = Arc::new(StdMutex::new(store));
        let context = NotificationContext::new(
            Arc::clone(&store),
            message_id.clone(),
            recipient,
            attempt_id,
        );
        let doorbell = cyclops_proto::render_doorbell_v1(&message_id);
        let handle =
            DeliveryHandle::for_notification("reviewer", "%1", 0, doorbell, context.clone());
        (NotificationScratch(path), store, context, handle, recipient)
    }

    fn notification_state(
        store: &Arc<StdMutex<MessageStore>>,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> cyclops_proto::NotificationRecord {
        store
            .lock()
            .unwrap()
            .projection()
            .notification(recipient, message_id)
            .cloned()
            .unwrap()
    }

    #[test]
    fn quota_reset_notice_names_the_exact_message_wide_command() {
        let message_id = MessageId::new("m-quota").unwrap();
        assert_eq!(
            quota_reset_notice(&message_id),
            "message m-quota remains held; run `cyclops requeue m-quota`"
        );
    }

    fn prepare_notification_receipt(context: &NotificationContext) {
        context.record_gating().unwrap();
        context
            .record_writing(
                ProcessInstanceId::new(4000, 818_000).unwrap(),
                ProcessInstanceId::new(4242, 818_221).unwrap(),
                "codex",
                NotificationTransport::Doorbell,
                None,
            )
            .unwrap();
        context.record_staged().unwrap();
        context.record_submitted().unwrap();
    }

    #[test]
    fn notification_and_direct_handles_have_distinct_projection_owners() {
        let (_scratch, _store, _context, notification, _recipient) =
            notification_fixture("projection-owner");
        let direct = DeliveryHandle::new("m-direct-owner", "reviewer", "%1", 0, "payload".into());

        assert!(!notification.owns_session_delivery_state());
        assert!(direct.owns_session_delivery_state());
    }

    #[test]
    fn screen_and_hook_receipts_keep_the_canonical_barrier_active() {
        for source in ["screen", "hook"] {
            let (_scratch, store, context, _handle, recipient) =
                notification_fixture(&format!("{source}-notified-barrier"));
            prepare_notification_receipt(&context);

            context.record_notified().unwrap();
            let active = store
                .lock()
                .unwrap()
                .projection()
                .active_notification_barriers();
            assert_eq!(active.len(), 1, "{source} receipt dropped the barrier");
            assert_eq!(active[0].recipient, recipient);
            assert_eq!(active[0].state, NotificationState::Notified);
        }
    }

    #[test]
    fn mailbox_capability_proof_is_exact_and_binding_scoped() {
        let scratch = cyclops_proto::scratch::scratch_dir(&format!(
            "mailbox-capability-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&scratch).unwrap();
        let capability_file = scratch.join("SKILL.md");
        std::fs::write(&capability_file, mailbox_capability::SHIPPED_SKILL).unwrap();
        let manifest = Manifest::parse(
            &format!(
                "[agent]\nid = \"fix\"\ndisplay_name = \"Fix\"\n[messaging]\nmailbox_capability_file = {:?}\n",
                capability_file.to_string_lossy()
            ),
            Path::new("fix.toml"),
        )
        .unwrap();
        let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
        let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
        let recipient =
            RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
        let agent = crate::identity::ProcId { pid: 41, birth: 90 };
        let proof = select_mailbox_capability(&manifest, recipient, agent, "fix")
            .expect("canonical skill proves capability");
        assert!(proof.recheck(recipient, agent, "fix"));
        assert!(!proof.recheck(
            recipient,
            crate::identity::ProcId { pid: 41, birth: 91 },
            "fix"
        ));
        assert!(!proof.recheck(recipient, agent, "replacement"));

        std::fs::write(&capability_file, b"older shipped skill").unwrap();
        assert!(select_mailbox_capability(&manifest, recipient, agent, "fix").is_none());
        std::fs::write(&capability_file, b"operator edited this skill").unwrap();
        assert!(!proof.recheck(recipient, agent, "fix"));
        std::fs::remove_file(&capability_file).unwrap();
        assert!(select_mailbox_capability(&manifest, recipient, agent, "fix").is_none());
        std::fs::create_dir(&capability_file).unwrap();
        assert!(select_mailbox_capability(&manifest, recipient, agent, "fix").is_none());
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn notification_prewrite_refusals_hold_without_a_legacy_terminal_state() {
        let (_scratch, store, context, notification, recipient) =
            notification_fixture("prewrite-policy");
        let direct = DeliveryHandle::new("m-direct-policy", "reviewer", "%1", 0, "payload".into());

        context.record_gating().unwrap();
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Gating
        );

        for cause in ["no_such_pane", "pane_dead", "no_manifest", "blocked_quota"] {
            assert_eq!(
                workspace_prewrite_hold(&notification, cause).as_deref(),
                Some(cause)
            );
            assert_eq!(workspace_prewrite_hold(&direct, cause), None);
        }
        assert_eq!(gate_hold_action(&notification, "blocked_quota"), "wait");
        assert_eq!(gate_hold_action(&notification, "no_manifest"), "hold");
        assert_eq!(gate_hold_action(&direct, "blocked_quota"), "hold");

        assert!(workspace_prewrite_failure_is_deferred(
            &notification,
            &AttemptFailure::spool_failed()
        ));
        assert!(!workspace_prewrite_failure_is_deferred(
            &notification,
            &AttemptFailure::verify_failed()
        ));
        assert!(!workspace_prewrite_failure_is_deferred(
            &direct,
            &AttemptFailure::spool_failed()
        ));
    }

    #[test]
    fn a_notification_bypasses_an_already_parked_legacy_worker() {
        let (_scratch, _store, _context, notification, _recipient) =
            notification_fixture("already-parked");
        let direct = DeliveryHandle::new("m-parked-direct", "reviewer", "%1", 0, "payload".into());
        let hint = Some("reset tomorrow".to_string());

        assert!(legacy_park_hint(&notification, hint.clone()).is_none());
        assert_eq!(
            legacy_park_hint(&direct, hint).as_deref(),
            Some("reset tomorrow")
        );
    }

    #[test]
    fn a_direct_quota_park_preserves_workspace_notifications_behind_it() {
        let (_scratch, store, context, notification, recipient) =
            notification_fixture("queued-behind-park");
        let first = DeliveryHandle::new("m-parked-first", "reviewer", "%1", 0, "first".into());
        let last = DeliveryHandle::new("m-parked-last", "reviewer", "%1", 0, "last".into());

        let (direct, workspace) = split_legacy_parked_queue(vec![
            Arc::clone(&first),
            Arc::clone(&notification),
            Arc::clone(&last),
        ]);

        assert_eq!(
            direct
                .iter()
                .map(|handle| handle.msg_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m-parked-first", "m-parked-last"]
        );
        assert_eq!(workspace.len(), 1);
        assert!(Arc::ptr_eq(&workspace[0], &notification));
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Queued
        );
    }

    #[test]
    fn restart_recovery_skips_workspace_messages_but_keeps_direct_deliveries() {
        let workspace_ids = HashSet::from(["m-workspace".to_string()]);

        assert!(!legacy_recovery_owns("m-workspace", &workspace_ids));
        assert!(legacy_recovery_owns("m-direct", &workspace_ids));
    }

    fn supersede_notification(
        store: &Arc<StdMutex<MessageStore>>,
        recipient: RecipientKey,
        message_id: &MessageId,
        replacement: &str,
    ) {
        store
            .lock()
            .unwrap()
            .accept(
                MessageId::new(replacement).unwrap(),
                MessageDraft {
                    kind: Kind::Msg,
                    sender: RecipientKey::admin(recipient.workspace_id()),
                    recipients: vec![recipient],
                    subject: Some("Replacement".into()),
                    body: None,
                    client_key: None,
                    supersedes: Some(message_id.clone()),
                    presentation: MessagePresentation {
                        sender_label: "admin".into(),
                        recipient_labels: vec![RecipientPresentation {
                            recipient,
                            label: "reviewer".into(),
                        }],
                    },
                },
            )
            .unwrap();
    }

    #[test]
    fn retry_policy_only_retries_failures_proven_before_the_write() {
        let cases = [
            (AttemptFailure::session_detached(), "session_detached", true),
            (AttemptFailure::no_manifest(), "no_manifest", true),
            (
                AttemptFailure::pane_rebound_before_paste(),
                "pane_rebound",
                true,
            ),
            (AttemptFailure::spool_failed(), "spool_failed", true),
            (AttemptFailure::paste_failed(), "paste_failed", false),
            (AttemptFailure::verify_failed(), "verify_failed", false),
            (
                AttemptFailure::pane_rebound_after_paste(),
                "pane_rebound_after_paste",
                false,
            ),
            (AttemptFailure::submit_failed(), "submit_failed", false),
            (AttemptFailure::ack_timeout(), "ack_timeout", false),
            (
                AttemptFailure::notification_record_failed(),
                NOTIFICATION_RECORD_FAILED,
                false,
            ),
        ];
        for (failure, cause, retryable) in cases {
            assert_eq!(failure.cause, cause);
            assert_eq!(
                should_retry(&failure, 1, 1),
                retryable,
                "retry policy changed for {cause}"
            );
        }
        let exhausted = AttemptFailure::spool_failed();
        assert!(!should_retry(&exhausted, 2, 1));

        // The production mapping keeps unknown injector errors conservative
        // too: they can never opt into the pre-write retry budget.
        let unknown = AttemptFailure::from_inject("future_failure".into());
        assert!(!should_retry(&unknown, 1, 1));
    }

    /// Every transition the pipeline can perform must be legal in the
    /// frozen state machine. If the proto table changes, this fails before
    /// any integration test does.
    #[test]
    fn pipeline_transitions_are_legal() {
        for (from, to) in PIPELINE_TRANSITIONS {
            assert!(
                from.can_transition_to(*to),
                "pipeline performs illegal transition {from:?} -> {to:?}"
            );
        }
    }

    #[test]
    fn payload_shape_matches_spec() {
        let p = render_payload(
            "m-3f9c2a",
            "codex",
            "Review the rate limiter",
            "please",
            false,
        );
        let lines: Vec<&str> = p.lines().collect();
        assert_eq!(
            lines[0],
            "[cyclops m-3f9c2a] FROM: codex  SUBJECT: Review the rate limiter"
        );
        assert_eq!(lines[1], "please");
        assert_eq!(lines[2], "Reply: cyclops send codex --subject \"...\"");
        assert!(
            !p.ends_with('\n'),
            "no trailing newline; submit is separate"
        );
    }

    #[test]
    fn fyi_payload_has_no_reply_hint() {
        let p = render_payload("m-1", "codex", "heads up", "body", true);
        assert!(!p.contains("Reply:"));
    }

    /// Every payload ends with the terminal sentinel, whatever else the
    /// envelope carries. The measured failure is that a long payload wraps
    /// and pushes the leading id out of the verify region while the tail
    /// stays visible, so verification needs a token at the end.
    #[test]
    fn payload_ends_with_the_terminal_sentinel() {
        for (fyi, from) in [(false, "codex"), (true, "codex"), (false, "admin")] {
            let p = render_payload("m-3f9c2a", from, "subject", "body", fyi);
            assert_eq!(
                p.lines().next_back(),
                Some("[cyclops:end m-3f9c2a]"),
                "fyi={fyi} from={from}"
            );
        }
    }

    /// A hook ACK verifies the bytes this delivery sent, or nothing.
    ///
    /// Two bugs this pins. The first: the matcher took any prompt
    /// CONTAINING a waiting delivery's id, so a later message quoting an
    /// earlier one upgraded that earlier delivery on somebody else's
    /// evidence. The second: matching the header and terminal sentinel
    /// alone still left the body free, and the pre-submit race is
    /// irreducible, so an edited body could be recorded as verified
    /// against the immutable ledger message it no longer is.
    #[test]
    fn a_hook_ack_verifies_the_payload_or_nothing() {
        let a = render_payload("m-aaa", "codex", "ship it", "body", false);
        let b = render_payload(
            "m-bbb",
            "codex",
            "re: m-aaa",
            &format!("you said:\n{a}\nwhat now?"),
            false,
        );

        assert!(prompt_matches(&a, &a), "the delivery's own bytes verify");
        assert!(b.contains("m-aaa"), "the quoting case this exists for");
        assert!(!prompt_matches(&b, &a), "quoted text is not a claim");
        assert!(prompt_matches(&b, &b));

        // Intact header and sentinel, edited body: the framing is
        // unchanged and the content is not the message that was sent.
        let edited = a.replace("body", "body, plus a line nobody sent");
        assert!(edited.starts_with("[cyclops m-aaa]"));
        assert!(edited.ends_with(&sentinel_for("m-aaa")));
        assert!(!prompt_matches(&edited, &a), "framing is not content");

        // Content before or after the payload is content.
        assert!(!prompt_matches(&format!("note\n{a}"), &a));
        assert!(!prompt_matches(&format!("{a}\nnote"), &a));

        // Whitespace inside the body is content too.
        assert!(!prompt_matches(&a.replace("ship it", "ship  it"), &a));

        // The one allowance: the closing newline a composer submit may or
        // may not carry. One, on the hook side, and nothing else.
        assert!(prompt_matches(&format!("{a}\n"), &a));
        assert!(!prompt_matches(&format!("{a}\n\n"), &a));
        assert!(!prompt_matches(&format!("{a}  "), &a));
        assert!(!prompt_matches(&format!(" {a}"), &a));

        // Line endings are content until a probe says otherwise, and the
        // payload is never rewritten to make a match succeed: a sender
        // whose body deliberately carries CRLF must not be verified by
        // hook bytes that dropped it.
        let crlf = render_payload("m-ccc", "codex", "s", "one\r\ntwo", false);
        assert!(!prompt_matches(&crlf.replace("\r\n", "\n"), &crlf));
        assert!(!prompt_matches(&a.replace('\n', "\r\n"), &a));
    }

    /// The sentinel is deliberately not the reply hint: transport
    /// verification must not depend on human-facing CLI copy.
    #[test]
    fn sentinel_is_independent_of_the_reply_hint() {
        let with_hint = render_payload("m-a", "codex", "s", "b", false);
        let without_hint = render_payload("m-a", "codex", "s", "b", true);
        assert!(with_hint.ends_with("[cyclops:end m-a]"));
        assert!(without_hint.ends_with("[cyclops:end m-a]"));
    }

    /// A legacy direct payload from the operator carries no reply line.
    ///
    /// `admin` is a durable mailbox address but no pane can hold that label.
    /// The mailbox claim output prints the validated `cyclops reply <id>`
    /// form. This compatibility renderer preserves its older payload shape
    /// and therefore omits one for admin.
    #[test]
    fn a_legacy_operator_payload_has_no_pane_addressed_reply_hint() {
        let p = render_payload("m-1", cyclops_proto::label::ADMIN, "ship it", "now", false);
        assert!(!p.contains("Reply:"), "{p}");
        assert_eq!(
            p, "[cyclops m-1] FROM: admin  SUBJECT: ship it\nnow\n[cyclops:end m-1]",
            "the header, the body, and the sentinel: no reply hint"
        );
        // An agent-to-agent message still gets one: those targets exist.
        let p = render_payload("m-2", "reviewer", "ship it", "now", false);
        assert!(p.contains("Reply: cyclops send reviewer"), "{p}");
    }

    #[test]
    fn empty_body_payload_is_header_plus_hint() {
        let p = render_payload("m-1", "codex", "s", "", false);
        let lines: Vec<&str> = p.lines().collect();
        assert_eq!(lines.len(), 3, "header, hint, sentinel: no empty body line");
        assert_eq!(lines[0], "[cyclops m-1] FROM: codex  SUBJECT: s");
        assert_eq!(lines[2], "[cyclops:end m-1]");
    }

    #[test]
    fn verify_patterns_substitute_split_and_default() {
        let m = Manifest::parse(
            r#"
[agent]
id = "x"
display_name = "x"
[injection]
verify_pattern = ["<message_id>", "Pasted text"]
"#,
            std::path::Path::new("x.toml"),
        )
        .unwrap();
        let (id, other) = verify_patterns(&m, "m-ab12");
        assert_eq!(id, vec!["m-ab12".to_string()]);
        assert_eq!(other, vec!["Pasted text".to_string()]);

        let empty = Manifest::parse(
            "[agent]\nid = \"y\"\ndisplay_name = \"y\"\n",
            std::path::Path::new("y.toml"),
        )
        .unwrap();
        let (id, other) = verify_patterns(&empty, "m-1");
        assert_eq!(id, vec!["m-1".to_string()]);
        assert!(other.is_empty());
    }

    /// A manifest carrying the measured composer layout: a box rule then a
    /// status row, each described in plain and escaped form.
    pub(super) fn sentinel_manifest() -> cyclops_manifest::Manifest {
        cyclops_manifest::Manifest::parse(
            r#"
[agent]
id = "s"
display_name = "s"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^\x1b\[39m❯']

[injection]
composer_trailer_regex = ['^─+$', '^\s*Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[90m─', '^\x1b\[38;5;\d+mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ (?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
"#,
            std::path::Path::new("s.toml"),
        )
        .unwrap()
    }

    /// The measured chrome block, escaped the way the vendor paints it.
    pub(super) const CHROME: &str = "\u{1b}[90m────────\n\u{1b}[38;5;152mModel x · Ctx: 78%";

    /// The failure this unit exists for: a long payload wraps, the leading
    /// id scrolls out of the region, and only the tail is visible.
    #[test]
    fn sentinel_verifies_a_wrapped_payload_whose_id_left_the_region() {
        let m = sentinel_manifest();
        let screen = format!(
            "\u{1b}[39m❯ [cyclops m-3f9c2a] FROM: codex  SUBJECT: long\n\
             wrapped continuation line one\n\
             [cyclops:end m-3f9c2a]\n{CHROME}"
        );
        assert!(sentinel_verified(&m, &screen, "m-3f9c2a"));
    }

    /// Nothing may follow the token that proves nothing follows it.
    ///
    /// The bug this pins: the sentinel row was compared after escape
    /// stripping. A torn `ESC [` swallows the rest of the line and a
    /// complete sequence is removed outright, so the sentinel plus any
    /// trailing bytes reduced to the exact token and verified. The
    /// measured row is unstyled, so the raw row itself has to be the
    /// token, with only the terminal's trailing padding removed.
    #[test]
    fn nothing_may_follow_the_terminal_sentinel() {
        let m = sentinel_manifest();
        let want = "[cyclops:end m-3f9c2a]";
        for forged in [
            // Torn CSI: the forgiving normalizer eats the remainder.
            format!("{want}\u{1b}["),
            format!("{want}\u{1b}[38;5and a whole sentence nobody sent"),
            // Complete CSI, which normalizes away just as cleanly.
            format!("{want}\u{1b}[0m"),
            format!("{want}\u{1b}[2K"),
            format!("{want}\u{1b}[1;5H"),
            // Operating-system commands, both terminator forms.
            format!("{want}\u{1b}]8;;http://example.com\u{7}"),
            format!("{want}\u{1b}]0;title\u{1b}\\"),
            // A bare ESC, dropped silently by the forgiving version.
            format!("{want}\u{1b}"),
            // And plain content, which was always refused.
            format!("{want} plus a human sentence"),
            // Whitespace a person can type is content. Only the
            // terminal's ASCII padding is not.
            format!("{want}\t"),
            format!("{want}\u{a0}"),
            format!("{want}\u{2003}"),
            // Styling in front of the token is content on this row too:
            // the measured row is unstyled.
            format!("\u{1b}[39m{want}"),
        ] {
            let screen = format!("\u{1b}[39m❯ body\n{forged}\n{CHROME}");
            assert!(
                !sentinel_verified(&m, &screen, "m-3f9c2a"),
                "must fail closed on {forged:?}"
            );
        }

        // The measured shape still verifies: the row is exactly the token,
        // and the terminal's trailing padding is not content.
        let screen = format!("\u{1b}[39m❯ body\n{want}   \n{CHROME}");
        assert!(sentinel_verified(&m, &screen, "m-3f9c2a"));
    }

    /// A sentinel split by the terminal edge proves nothing about what else
    /// the capture lost, so it refuses.
    #[test]
    fn truncated_or_wrapped_sentinel_fails_closed() {
        let m = sentinel_manifest();
        for tail in [
            "[cyclops:end m-3f9c",
            "[cyclops:end\nm-3f9c2a]",
            "cyclops:end m-3f9c2a]",
        ] {
            let screen = format!("\u{1b}[39m❯ body\n{tail}\n{CHROME}");
            assert!(
                !sentinel_verified(&m, &screen, "m-3f9c2a"),
                "must fail closed on {tail:?}"
            );
        }
    }

    /// Payload after the sentinel means the capture is not the whole story.
    #[test]
    fn payload_after_the_sentinel_fails_closed() {
        let m = sentinel_manifest();
        let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\nstray text\n{CHROME}");
        assert!(!sentinel_verified(&m, &screen, "m-1"));
    }

    /// Two identical sentinels are an ambiguity about which row transport
    /// owns, not a reason to prefer the lower one.
    #[test]
    fn a_duplicate_sentinel_fails_closed() {
        let m = sentinel_manifest();
        let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\n[cyclops:end m-1]\n{CHROME}");
        assert!(!sentinel_verified(&m, &screen, "m-1"));
    }

    /// A blank row after the sentinel is composer content: the sentinel
    /// was not the last thing on the row below. Filtering it away and
    /// accepting the chrome underneath is how a payload gap disappears.
    #[test]
    fn a_blank_row_after_the_sentinel_fails_closed() {
        let m = sentinel_manifest();
        for gap in ["\n", "\n\n"] {
            let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]{gap}\n{CHROME}");
            assert!(
                !sentinel_verified(&m, &screen, "m-1"),
                "blank payload row must refuse: {gap:?}"
            );
        }
    }

    /// Leading bytes belong to the composer, so the row is not the exact
    /// transport token however familiar it looks.
    #[test]
    fn leading_bytes_before_the_sentinel_fail_closed() {
        let m = sentinel_manifest();
        for lead in [" ", "\t", "x "] {
            let screen = format!("\u{1b}[39m❯ body\n{lead}[cyclops:end m-1]\n{CHROME}");
            assert!(
                !sentinel_verified(&m, &screen, "m-1"),
                "leading {lead:?} must refuse"
            );
        }
    }

    /// A capture that ends at the sentinel never saw the composer's chrome,
    /// so it never saw the composer. Vacuous truth is not evidence.
    #[test]
    fn a_sentinel_with_nothing_after_it_fails_closed() {
        let m = sentinel_manifest();
        assert!(!sentinel_verified(
            &m,
            "\u{1b}[39m❯ body\n[cyclops:end m-1]",
            "m-1"
        ));
    }

    /// A sentinel that scrolled into the transcript has the composer
    /// between it and the chrome, and a composer row claims no layout
    /// entry. Both an empty composer and one holding other text refuse.
    #[test]
    fn a_transcript_echo_of_the_sentinel_never_verifies() {
        let m = sentinel_manifest();
        for composer in ["\u{1b}[39m❯ ", "\u{1b}[39m❯ something else"] {
            let screen = format!("[cyclops:end m-1]\n{composer}\n{CHROME}");
            assert!(!sentinel_verified(&m, &screen, "m-1"), "{composer:?}");
        }
    }

    /// Chrome-shaped prose inserted before the real chrome must not be
    /// walked past: it is unstyled, so it claims no layout entry.
    #[test]
    fn chrome_shaped_payload_before_the_chrome_fails_closed() {
        let m = sentinel_manifest();
        for line in ["Model y · Ctx: 50%", "────────"] {
            let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\n{line}\n{CHROME}");
            assert!(
                !sentinel_verified(&m, &screen, "m-1"),
                "unstyled {line:?} must not pass as chrome"
            );
        }
    }

    /// Order is part of the layout: the status row cannot precede the rule.
    #[test]
    fn chrome_out_of_measured_order_fails_closed() {
        let m = sentinel_manifest();
        let screen = "\u{1b}[39m❯ body\n[cyclops:end m-1]\n\u{1b}[38;5;152mModel x · Ctx: 78%\n\u{1b}[90m────────";
        assert!(!sentinel_verified(&m, screen, "m-1"));
    }

    /// Without an escaped capture the styling evidence is absent, so the
    /// answer is refuse rather than guess.
    #[test]
    fn a_plain_capture_never_verifies_the_sentinel() {
        let m = sentinel_manifest();
        assert!(!sentinel_verified(
            &m,
            "❯ body\n[cyclops:end m-1]\n────────\nModel x · Ctx: 78%",
            "m-1"
        ));
    }

    /// An unmeasured vendor cannot answer the terminality question at all.
    #[test]
    fn an_undeclared_vendor_never_verifies_by_sentinel() {
        let bare = cyclops_manifest::Manifest::parse(
            "[agent]\nid = \"x\"\ndisplay_name = \"x\"\n",
            std::path::Path::new("x.toml"),
        )
        .unwrap();
        let screen = format!("\u{1b}[39m❯ body\n[cyclops:end m-1]\n{CHROME}");
        assert!(!sentinel_verified(&bare, &screen, "m-1"));
    }

    /// A visible leading id is not evidence: every one of these renders the
    /// header while the tail is missing or malformed, which is what a
    /// truncated paste looks like, and none may verify.
    #[test]
    fn a_visible_leading_id_never_verifies_without_a_sound_sentinel() {
        let m = sentinel_manifest();
        let (id, other) = verify_patterns(&m, "m-3f9c2a");
        let head = "\u{1b}[39m❯ [cyclops m-3f9c2a] FROM: codex  SUBJECT: long";
        for (name, screen) in [
            ("missing sentinel", format!("{head}\nbody\n{CHROME}")),
            (
                "truncated sentinel",
                format!("{head}\nbody\n[cyclops:end m-3f9\n{CHROME}"),
            ),
            (
                "payload after sentinel",
                format!("{head}\nbody\n[cyclops:end m-3f9c2a]\nstray\n{CHROME}"),
            ),
            (
                "no chrome at all",
                format!("{head}\nbody\n[cyclops:end m-3f9c2a]"),
            ),
        ] {
            assert_eq!(
                staged_verified(&m, &screen, &id, &other, "m-3f9c2a"),
                None,
                "{name} must not verify on the leading id"
            );
        }
        let ok = format!("{head}\nbody\n[cyclops:end m-3f9c2a]\n{CHROME}");
        assert_eq!(
            staged_verified(&m, &ok, &id, &other, "m-3f9c2a"),
            Some(true)
        );
    }

    /// The chip proof is manifest data plus a composer pin, and it needs
    /// both halves: the row must render as the vendor's chip AND sit on a
    /// row a composer rule recognizes. A manifest that declares no chip
    /// syntax has no chip lane at all.
    #[test]
    fn marker_in_composer_is_manifest_driven() {
        let m = cyclops_manifest::Manifest::parse(
            r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#,
            std::path::Path::new("c.toml"),
        )
        .unwrap();
        // Staged and unsubmitted: the composer row IS the chip.
        let staged = "transcript\n\u{1b}[39m❯\u{a0}[Pasted text #1]\n? for shortcuts";
        assert!(marker_in_composer(&m, staged));
        // Submitted: composer cleared, the chip only in the transcript.
        let submitted = "old [Pasted text #1]\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
        assert!(!marker_in_composer(&m, submitted));
        // A manifest with no chip syntax can never pin one.
        let bare = cyclops_manifest::Manifest::parse(
            "[agent]\nid = \"x\"\ndisplay_name = \"x\"\n",
            std::path::Path::new("x.toml"),
        )
        .unwrap();
        assert!(!marker_in_composer(&bare, staged));
    }

    #[test]
    fn bottom_window_takes_non_empty_tail() {
        let screen = "a\n\nb\nc\n   \nd\n";
        assert_eq!(bottom_window(screen, 2), "c\nd");
        assert_eq!(bottom_window(screen, 10), "a\nb\nc\nd");
    }

    #[test]
    fn reset_hint_parses_and_stays_short() {
        let screen = "junk\n⚠ Individual quota reached. Resets in 135h57m42s.\nmore";
        assert_eq!(
            parse_reset_hint(screen).as_deref(),
            Some("resets in 135h57m42s")
        );
        assert_eq!(parse_reset_hint("no hint here"), None);
    }

    #[test]
    fn mint_ids_are_unique_and_shaped() {
        let e = Engine::new();
        let a = e.mint_msg_id();
        let b = e.mint_msg_id();
        assert_ne!(a, b);
        assert!(a.starts_with("m-") && a.len() == 8, "{a}");
    }

    // -----------------------------------------------------------------
    // Post-paste verification ignores stale transcript text.
    // -----------------------------------------------------------------

    const COMPOSER_MANIFEST: &str = r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+( \+\d+ lines)?\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+( \+\d+ lines)?\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#;

    fn composer_manifest() -> Manifest {
        Manifest::parse(COMPOSER_MANIFEST, std::path::Path::new("c.toml")).unwrap()
    }

    #[test]
    fn stale_generic_pattern_does_not_verify() {
        let m = composer_manifest();
        let (id, other) = verify_patterns(&m, "m-new01");
        // "Pasted text" from a PREVIOUS message sits in the transcript;
        // the composer is empty. Nothing staged.
        let screen =
            "you: [Pasted text #1 +9 lines]\nassistant: done\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
        assert_eq!(staged_verified(&m, screen, &id, &other, "m-new01"), None);
        // The same chip ON the composer line is a real staging.
        let staged = "transcript\n\u{1b}[39m❯\u{a0}[Pasted text #2 +9 lines]\n? for shortcuts";
        assert_eq!(
            staged_verified(&m, staged, &id, &other, "m-new01"),
            Some(false)
        );
        // A visible id proves the head arrived and nothing more, which is
        // also what a truncated paste looks like, so it does not verify.
        let id_anywhere = "transcript\n❯ [cyclops m-new01] hello\n? for shortcuts";
        assert_eq!(
            staged_verified(&m, id_anywhere, &id, &other, "m-new01"),
            None
        );
    }

    /// The whole inject path with a mock backend: stale transcript chips
    /// fail every verification read while a chip on the composer line passes.
    pub(super) struct MockInjector {
        screens: StdMutex<Vec<String>>,
        cursor: std::sync::atomic::AtomicUsize,
        pub(super) pasted: StdMutex<Vec<String>>,
        spooled: StdMutex<Option<String>>,
    }

    impl MockInjector {
        pub(super) fn new(screens: Vec<&str>) -> MockInjector {
            MockInjector {
                screens: StdMutex::new(screens.into_iter().map(String::from).collect()),
                cursor: std::sync::atomic::AtomicUsize::new(0),
                pasted: StdMutex::new(Vec::new()),
                spooled: StdMutex::new(None),
            }
        }
    }

    impl Injector for MockInjector {
        async fn spool(&self, payload: &str) -> Result<(), String> {
            *self.spooled.lock().unwrap() = Some(payload.to_string());
            Ok(())
        }

        async fn discard(&self) {
            *self.spooled.lock().unwrap() = None;
        }

        async fn commit(
            &self,
            _pane_id: &str,
            on_write: &(dyn Fn() -> Result<(), String> + Sync),
        ) -> Result<(), String> {
            on_write()?;
            let payload = self
                .spooled
                .lock()
                .unwrap()
                .clone()
                .expect("commit without a spooled payload");
            self.pasted.lock().unwrap().push(payload);
            Ok(())
        }
        async fn submit(&self, _pane_id: &str, _key: &str) -> Result<(), String> {
            Ok(())
        }
        // The canned screens are authored in whichever flavor the test's
        // manifest asks for: the escaped read returns them raw, the plain
        // read returns them de-escaped (identity for plain fixtures) —
        // the same relationship the two tmux captures have.
        async fn capture(&self, _pane_id: &str) -> Result<String, String> {
            self.capture_escaped(_pane_id).await.map(|s| strip_csi(&s))
        }
        async fn capture_escaped(&self, _pane_id: &str) -> Result<String, String> {
            let screens = self.screens.lock().unwrap();
            let i = self.cursor.fetch_add(1, Ordering::Relaxed);
            Ok(screens[i.min(screens.len() - 1)].clone())
        }
    }

    #[tokio::test]
    async fn notification_facts_follow_real_inject_submit_and_receipt_boundaries() {
        let (_scratch, store, context, handle, recipient) = notification_fixture("boundaries");
        context.record_gating().unwrap();
        let payload = handle.payload();
        let manifest = sentinel_manifest();
        let screen = format!("\u{1b}[39m❯ {payload}\n{CHROME}");
        let injector = MockInjector::new(vec![&screen]);
        injector.spool(&payload).await.unwrap();
        inject(
            &injector,
            &handle,
            &manifest,
            StagingTarget::ExactRow(&payload),
            &|| {
                assert!(injector.pasted.lock().unwrap().is_empty());
                context
                    .record_writing(
                        ProcessInstanceId::new(4000, 818_000).unwrap(),
                        ProcessInstanceId::new(4242, 818_221).unwrap(),
                        "codex",
                        NotificationTransport::Doorbell,
                        None,
                    )
                    .map(|_| ())
                    .map_err(notification_write_cause)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Writing
        );

        context.record_staged().unwrap();
        injector.submit("%1", "Enter").await.unwrap();
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Staged,
            "send-keys success is not a receipt"
        );
        context.record_submitted().unwrap();
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Submitted
        );
        context.record_notified().unwrap();
        let notified = notification_state(&store, recipient, context.message_id());
        assert_eq!(notified.state, cyclops_proto::NotificationState::Notified);
        assert_eq!(notified.binding.unwrap().manifest.as_str(), "codex");
    }

    #[tokio::test]
    async fn superseded_notification_aborts_inside_on_write_without_pasting() {
        let (_scratch, store, context, handle, recipient) = notification_fixture("superseded");
        context.record_gating().unwrap();
        supersede_notification(
            &store,
            recipient,
            context.message_id(),
            "m-superseded-replacement",
        );

        let manifest = sentinel_manifest();
        let payload = handle.payload();
        let screen = format!("\u{1b}[39m❯ {payload}\n{CHROME}");
        let injector = MockInjector::new(vec![&screen]);
        injector.spool(&payload).await.unwrap();
        let error = inject(
            &injector,
            &handle,
            &manifest,
            StagingTarget::ExactRow(&payload),
            &|| {
                context
                    .ensure_current_gating()
                    .map_err(notification_write_cause)
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error, SUPERSEDED_BEFORE_WRITE);
        assert!(injector.pasted.lock().unwrap().is_empty());
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Superseded
        );
    }

    #[tokio::test]
    async fn claim_withdrawal_wakes_the_exact_attempt_and_prevents_the_write() {
        let (_scratch, store, context, handle, recipient) = notification_fixture("claimed");
        context.record_gating().unwrap();
        let outcome = store
            .lock()
            .unwrap()
            .claim(recipient, context.message_id().clone())
            .unwrap();
        let crate::mailbox::ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = outcome
        else {
            panic!("first claim must append a claim fact");
        };
        assert_eq!(withdrawn_attempt, Some(context.attempt_id()));

        let engine = Engine::new();
        engine
            .notification_attempts
            .lock()
            .unwrap()
            .insert(context.attempt_id(), Arc::downgrade(&handle));
        engine.cancel_notification(context.attempt_id());
        tokio::time::timeout(Duration::from_millis(100), handle.cancel.notified())
            .await
            .expect("claim wakes the withdrawn attempt");

        let manifest = sentinel_manifest();
        let payload = handle.payload();
        let screen = format!("\u{1b}[39m❯ {payload}\n{CHROME}");
        let injector = MockInjector::new(vec![&screen]);
        injector.spool(&payload).await.unwrap();
        let error = inject(
            &injector,
            &handle,
            &manifest,
            StagingTarget::ExactRow(&payload),
            &|| {
                context
                    .ensure_current_gating()
                    .map_err(notification_write_cause)
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error, SUPERSEDED_BEFORE_WRITE);
        assert!(injector.pasted.lock().unwrap().is_empty());
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Superseded
        );
    }

    #[test]
    fn withdrawn_notification_never_enters_the_delivery_gate() {
        let (_scratch, store, context, _handle, recipient) =
            notification_fixture("withdrawn-before-gate");
        supersede_notification(
            &store,
            recipient,
            context.message_id(),
            "m-withdrawn-replacement",
        );

        assert!(matches!(
            context.record_gating(),
            Err(NotificationAdapterError::SupersededBeforeWrite)
        ));
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Superseded
        );
    }

    #[test]
    fn notification_gate_admission_is_idempotent_for_worker_reentry() {
        let (_scratch, store, context, _handle, recipient) = notification_fixture("gate-reentry");

        let first = context.record_gating().unwrap();
        let second = context.record_gating().unwrap();

        assert_eq!(first, second);
        assert_eq!(
            notification_state(&store, recipient, context.message_id()).state,
            cyclops_proto::NotificationState::Gating
        );
    }

    #[test]
    fn notification_faults_map_to_the_closed_attention_taxonomy() {
        for (cause, expected) in [
            ("paste_failed", NotificationAttentionCause::PasteFailed),
            ("verify_failed", NotificationAttentionCause::VerifyFailed),
            (
                "pane_rebound_after_paste",
                NotificationAttentionCause::PaneReboundAfterPaste,
            ),
            ("submit_failed", NotificationAttentionCause::SubmitFailed),
            (
                "receipt_occupant_changed",
                NotificationAttentionCause::ReceiptOccupantChanged,
            ),
            ("ack_timeout", NotificationAttentionCause::AckTimeout),
            (
                NOTIFICATION_RECORD_FAILED,
                NotificationAttentionCause::TransportOutcomeUnknown,
            ),
        ] {
            assert_eq!(notification_attention_cause(cause), expected, "{cause}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_staging_proof_records_attention_without_retrying_the_paste() {
        let (_scratch, store, context, handle, recipient) = notification_fixture("verify-fault");
        context.record_gating().unwrap();
        let manifest = sentinel_manifest();
        let injector = MockInjector::new(vec!["transcript\n❯\n? for shortcuts"]);
        let payload = handle.payload();
        injector.spool(&payload).await.unwrap();
        let result = inject(
            &injector,
            &handle,
            &manifest,
            StagingTarget::ExactRow(&payload),
            &|| {
                context
                    .record_writing(
                        ProcessInstanceId::new(4000, 818_000).unwrap(),
                        ProcessInstanceId::new(4242, 818_221).unwrap(),
                        "codex",
                        NotificationTransport::Doorbell,
                        None,
                    )
                    .map(|_| ())
                    .map_err(notification_write_cause)
            },
        )
        .await;
        assert_eq!(result, Err("verify_failed".into()));
        assert_eq!(injector.pasted.lock().unwrap().len(), 1);

        context
            .record_attention(notification_attention_cause("verify_failed"))
            .unwrap();
        let attention = notification_state(&store, recipient, context.message_id());
        assert_eq!(
            attention.state,
            cyclops_proto::NotificationState::AttentionRequired
        );
        assert_eq!(
            attention.cause,
            Some(NotificationAttentionCause::VerifyFailed)
        );
        assert!(attention.binding.is_some());
    }

    /// A refusal at the write boundary stops the write and costs no
    /// transport budget.
    ///
    /// The callback is the last thing between a proof and the pane taking
    /// the payload, and it is where the barrier is claimed and the pane's
    /// binding is compared again. Both of the things it can refuse for,
    /// somebody else holding the composer and the pane becoming another
    /// program, are the world moving rather than transport failing:
    /// nothing was written, so the delivery goes back to the gate instead
    /// of spending a retry or summoning a human.
    #[tokio::test]
    async fn a_refused_write_boundary_never_pastes_and_never_spends_budget() {
        let m = composer_manifest();
        let handle = DeliveryHandle::new("m-x", "worker", "%1", 0, "payload".into());
        for cause in ["barrier_held", "binding_changed", "capability_changed"] {
            let mock = MockInjector::new(vec!["transcript\n\u{1b}[39m❯\u{a0}\n? for shortcuts"]);
            let payload = handle.payload();
            mock.spool(&payload).await.expect("spool");
            assert_eq!(
                inject(
                    &mock,
                    &handle,
                    &m,
                    StagingTarget::Sentinel(&handle.msg_id),
                    &|| Err(cause.to_string())
                )
                .await,
                Err(cause.to_string())
            );
            assert!(
                mock.pasted.lock().unwrap().is_empty(),
                "{cause} still reached the pane"
            );

            let failure = AttemptFailure::from_inject(cause.to_string());
            assert_eq!(
                failure.boundary,
                WriteBoundary::BeforeWrite,
                "{cause} must not be treated as possibly-written"
            );
            assert!(
                failure.regate(),
                "{cause} belongs back at the gate, not in the retry budget"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn inject_rejects_stale_screen_and_accepts_staged() {
        let m = composer_manifest();
        let handle = DeliveryHandle::new("m-new01", "worker", "%1", 0, "payload".into());

        let stale = "you: [Pasted text #1 +9 lines]\nold turn\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
        let mock = MockInjector::new(vec![stale]);
        let payload = handle.payload();
        mock.spool(&payload).await.expect("spool");
        assert_eq!(
            inject(
                &mock,
                &handle,
                &m,
                StagingTarget::Sentinel(&handle.msg_id),
                &|| Ok(())
            )
            .await,
            Err("verify_failed".to_string())
        );
        assert_eq!(mock.pasted.lock().unwrap().len(), 1, "payload was pasted");

        let staged = "transcript\n\u{1b}[39m❯\u{a0}[Pasted text #2 +9 lines]\n? for shortcuts";
        let mock = MockInjector::new(vec![stale, staged]);
        mock.spool(&payload).await.expect("spool");
        let (window, id_staged, _proof) = inject(
            &mock,
            &handle,
            &m,
            StagingTarget::Sentinel(&handle.msg_id),
            &|| Ok(()),
        )
        .await
        .expect("staged verifies");
        assert!(!id_staged, "generic pattern staged it, not the id");
        assert!(window.contains("Pasted text #2"));
    }

    /// The shipped codex manifest, parsed as data for the two tests below:
    /// its only composer discriminators are `line_regex_esc` clauses, on
    /// purpose (a plain capture cannot tell its ghost text from typed
    /// text).
    fn codex_manifest() -> Manifest {
        let m = Manifest::parse(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/codex.toml"
            )),
            std::path::Path::new("codex.toml"),
        )
        .expect("shipped codex manifest parses");
        assert!(m.has_escaped_rules(), "codex discriminates by SGR");
        m
    }

    /// Lines as codex-cli 0.147.0 actually draws them, captured from a
    /// live pane on 2026-08-17 (the full screens are
    /// cyclops-manifest/tests/fixtures/codex_pasted_chip_*). The chip is
    /// COLORED and the transcript glyph is bold-DIM where the composer's
    /// is bold-only; both facts decide the assertions below, and an
    /// invented approximation of either passed while the real thing
    /// failed.
    const CODEX_COMPOSER_CHIP: &str =
        "\u{1b}[1m›\u{1b}[0m \u{1b}[38;5;6m[Pasted Content 2828 chars]\u{1b}[39m";
    const CODEX_COMPOSER_GHOST: &str =
        "\u{1b}[1m›\u{1b}[0m \u{1b}[2mSummarize recent commits\u{1b}[0m";
    /// The measured composer trailer below that chip, from the same
    /// capture: a blank row, then the model row. Both are painted every
    /// time, and they are what proves the chip is the last thing in the
    /// composer rather than merely present in it.
    const CODEX_TRAILER: &str = "\n\n  \u{1b}[38;2;246;226;183mgpt-5.6-sol high\u{1b}[39m · /tmp/x";
    const CODEX_TRANSCRIPT_LINE: &str =
        "\u{1b}[1;2m›  \u{1b}[0m[cyclops m-diag01] FROM: tester  SUBJECT: verify chip rendering";

    /// The field failure this fixes, pinned on the shipped manifest and
    /// real captures: a message long enough to collapse renders as a
    /// "[Pasted Content N chars]" chip that hides the id, so the generic
    /// "Pasted" tier is the only staging evidence left. That tier
    /// pins the marker to the composer line, which for codex is
    /// recognizable only in an escaped capture. Every verify re-read
    /// failed, verify_before_submit withheld Enter, and the payload sat
    /// staged in the recipient's composer behind "outcome unknown".
    #[test]
    fn codex_collapsed_paste_verifies_through_the_escaped_composer_line() {
        let m = codex_manifest();
        let (id, other) = verify_patterns(&m, "m-jean01");

        let staged = format!("transcript above\n{CODEX_COMPOSER_CHIP}{CODEX_TRAILER}");
        assert_eq!(
            staged_verified(&m, &staged, &id, &other, "m-x"),
            Some(false)
        );

        // A chip in the TRANSCRIPT (bold-dim glyph) over an empty
        // composer: an earlier message, already submitted. Nothing staged.
        let stale = format!(
            "\u{1b}[1;2m›  \u{1b}[0m[Pasted Content 900 chars]\n{CODEX_COMPOSER_GHOST}{CODEX_TRAILER}"
        );
        assert_eq!(staged_verified(&m, &stale, &id, &other, "m-x"), None);

        // A short message renders literally: the id proves it anywhere in
        // the region, chip or no chip.
        // A short message renders literally, so its sentinel is on screen
        // and is what verifies it: the id alone never does. The status row
        // below it must arrive PAINTED, which is what separates the real
        // chrome from prose shaped like it.
        // Codex paints a blank separator between the composer and status.
        let literal = format!(
            "{CODEX_TRANSCRIPT_LINE}\n\u{1b}[1m›\u{1b}[0m [cyclops m-jean01] hello\n[cyclops:end m-jean01]\n\n  \u{1b}[38;2;246;226;183mgpt-5.6-sol high\u{1b}[39m · /tmp/x"
        );
        assert_eq!(
            staged_verified(&m, &literal, &id, &other, "m-jean01"),
            Some(true)
        );
    }

    /// The whole inject() path against the codex manifest: the escaped
    /// capture is the one that decides — its de-escaped sibling cannot
    /// tell the composer glyph from the transcript's — and the delivery
    /// proceeds to submit instead of erroring verify_failed.
    #[tokio::test(start_paused = true)]
    async fn inject_verifies_codex_collapse_via_the_escaped_capture() {
        let m = codex_manifest();
        let handle = DeliveryHandle::new("m-jean01", "codex", "%1", 0, "payload".into());
        let staged = format!("transcript above\n{CODEX_COMPOSER_CHIP}{CODEX_TRAILER}");
        let mock = MockInjector::new(vec![staged.as_str()]);
        let payload = handle.payload();
        mock.spool(&payload).await.expect("spool");
        let (window, id_staged, _proof) = inject(
            &mock,
            &handle,
            &m,
            StagingTarget::Sentinel(&handle.msg_id),
            &|| Ok(()),
        )
        .await
        .expect("collapse stages");
        assert!(
            !id_staged,
            "the chip hides the id; the composer line proved it"
        );
        assert!(window.contains("[Pasted Content 2828 chars]"), "{window}");
        // The ACK comparison window is de-escaped, so later SGR churn
        // cannot fake a changed composer.
        assert!(!window.contains('\u{1b}'), "{window}");
    }

    // -----------------------------------------------------------------
    // Tier-2 evidence (fix D) and the detach-aware clock (fix E)
    // -----------------------------------------------------------------

    #[test]
    fn tier2_changed_window_alone_needs_the_id_staged() {
        // A changed window with no staged id is a redraw, not delivery
        // evidence (this returned true before fix D).
        assert!(!tier2_evidence(true, false, false, false));
        assert!(tier2_evidence(true, true, false, false));
        assert!(tier2_evidence(false, false, true, false));
        // Output activity is no longer evidence on its own: %output names
        // a pane and its bytes, never the process that wrote them, so a
        // replacement occupant's noise could otherwise resolve a delivery
        // it never received. It survives as a cue to look.
        assert!(!tier2_evidence(false, false, false, true));
        // Marker gone but nothing else moved: not evidence.
        assert!(!tier2_evidence(false, true, false, false));
    }

    #[test]
    fn unobservable_evidence_freezes_instead_of_expiring() {
        // The detach race: the watcher is cleared before its lifecycle
        // event is broadcast, so a checkpoint's evidence pass cannot look.
        // Before the fix an expired clock returned Timeout here and the
        // retry double-pasted a delivery that may have landed.
        assert_eq!(
            checkpoint_step(Evidence::Unobservable, true),
            CheckpointStep::Freeze
        );
        assert_eq!(
            checkpoint_step(Evidence::Unobservable, false),
            CheckpointStep::Freeze
        );
        // Expiry stands only on a pass that looked and saw nothing.
        assert_eq!(
            checkpoint_step(Evidence::Absent, true),
            CheckpointStep::Expire
        );
        assert_eq!(
            checkpoint_step(Evidence::Absent, false),
            CheckpointStep::Wait
        );
        assert_eq!(
            checkpoint_step(Evidence::Confirmed, true),
            CheckpointStep::Deliver
        );
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Every target a clock hands out, in milliseconds from the submit it
    /// was built on, paired with whether it is the hook-phase end.
    ///
    /// The ACK ladder is a sequence, and asserting it one `next_target` at
    /// a time hides the shape and makes a moved rung read as an unrelated
    /// number. Reading it as offsets also makes the arithmetic checkable
    /// by eye: 1500 is the hook window, 250/750/1500/3000/5000 are the
    /// checkpoints, 5000 is the give-up deadline.
    fn timeline(mut c: AckClock, submit_at: Instant) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        // Bounded so a clock that stops advancing fails as a wrong
        // timeline instead of hanging the suite.
        for _ in 0..12 {
            let Some((at, hook_end)) = c.next_target() else {
                break;
            };
            out.push(((at - submit_at).as_millis() as u64, hook_end));
            if hook_end {
                c.end_hook_phase(at);
            } else if c.expired(at) {
                break;
            } else {
                c.advance_checkpoint();
            }
        }
        out
    }

    /// The clock reads no wall time after it is built.
    ///
    /// Everything it hands out is derived from the submit instant it was
    /// given, so two clocks built ten minutes apart must produce the same
    /// timeline. This is the property that keeps the assertions above from
    /// being load-sensitive, and it is worth stating once rather than
    /// leaving every reader to re-derive it from `next_target`.
    #[tokio::test(start_paused = true)]
    async fn the_ack_timeline_does_not_depend_on_when_the_clock_was_built() {
        let early = Instant::now();
        let late = early + ms(600_000);
        assert_eq!(
            timeline(AckClock::new(early, Some(ms(1500))), early),
            timeline(AckClock::new(late, Some(ms(1500))), late)
        );
        assert_eq!(
            timeline(AckClock::new(early, None), early),
            timeline(AckClock::new(late, None), late)
        );
    }

    /// Instants are asserted as offsets from the submit the clock was
    /// built on, never as wall-clock values: nothing in `AckClock` reads
    /// the clock after construction (proved by
    /// `the_ack_timeline_does_not_depend_on_when_the_clock_was_built`), so
    /// every number below is arithmetic and none of it is a race.
    #[tokio::test(start_paused = true)]
    async fn ack_clock_freezes_across_detach_and_extends_deadlines() {
        let t0 = Instant::now();
        let at = |c: &AckClock| {
            c.next_target()
                .map(|(t, hook)| ((t - t0).as_millis() as u64, hook))
        };
        let mut c = AckClock::new(t0, Some(ms(1500)));
        assert_eq!(at(&c), Some((1500, true)));

        // Detach at +200ms: the clock stops firing entirely.
        c.freeze(t0 + ms(200));
        assert!(c.frozen());
        assert_eq!(c.next_target(), None);
        assert!(!c.expired(t0 + ms(60_000)), "a frozen clock never expires");
        // A second freeze keeps the first freeze instant.
        c.freeze(t0 + ms(300));

        // Reattach at +6200ms: 6s of outage extend every deadline.
        c.unfreeze(t0 + ms(6200));
        assert_eq!(at(&c), Some((7500, true)));
        c.end_hook_phase(t0 + ms(7500));
        // Checkpoints shifted by 6s. The ones the hook phase covered
        // (250/750/1500 -> 6250/6750/7500) are dropped and replaced by one
        // pass now, so tier 2 opens with a look instead of a wait.
        assert_eq!(at(&c), Some((7500, false)));
        c.advance_checkpoint();
        assert_eq!(at(&c), Some((9000, false)));
        c.advance_checkpoint();
        assert_eq!(at(&c), Some((11_000, false)));
        c.advance_checkpoint();
        // Past the checkpoints the final deadline is also shifted.
        assert_eq!(at(&c), Some((11_000, false)));
        assert!(!c.expired(t0 + ms(10_999)));
        assert!(c.expired(t0 + ms(11_000)));
    }

    /// A screen-tier agent has no hook window, so the ladder is the
    /// checkpoints and nothing else.
    #[tokio::test(start_paused = true)]
    async fn ack_clock_without_hook_window_goes_straight_to_checkpoints() {
        let t0 = Instant::now();
        assert_eq!(
            timeline(AckClock::new(t0, None), t0),
            vec![
                (250, false),
                (750, false),
                (1500, false),
                (3000, false),
                (5000, false),
            ]
        );
    }

    /// The shipped numbers, and the hole the receipt fell through.
    ///
    /// ack_timeout_ms is 1500 and every manifest for a real CLI declares a
    /// hook, so a pane whose hooks are not wired spends the whole window
    /// waiting for an ACK that never comes. When it closes, the screen has
    /// held the evidence since the submit, and the second entry here is
    /// the look that reads it.
    ///
    /// That entry is the fix. Without it the timeline ran
    /// [(1500, hook), (3000, ...)]: a second and a half in which tier 2
    /// had opened and nothing looked, with receipt_block_ms (2500)
    /// expiring inside it. A 1.5s gap between the first two entries is
    /// exactly that defect coming back.
    #[tokio::test(start_paused = true)]
    async fn tier2_opens_the_moment_the_hook_window_closes() {
        let t0 = Instant::now();
        assert_eq!(
            timeline(AckClock::new(t0, Some(ms(1500))), t0),
            vec![(1500, true), (1500, false), (3000, false), (5000, false),]
        );
    }

    /// Queued is a claim about the pane: nothing has been typed into it.
    #[test]
    fn a_receipt_calls_a_delivery_queued_only_before_the_paste() {
        use DeliveryState::*;
        for s in [Queued, Gating, RetryQueued] {
            assert!(receipt_is_queued(s), "{s:?} is waiting on the recipient");
        }
        for s in [Pasting, Staged, Submitted] {
            assert!(
                !receipt_is_queued(s),
                "{s:?} has the payload in the pane and may not report queued"
            );
        }
        // Resolved states never reach the question.
        for s in [
            DeliveredVerified,
            DeliveredUnverified,
            AttentionRequired,
            ParkedBlockedQuota,
        ] {
            assert!(receipt_resolved(s), "{s:?}");
        }
    }

    #[test]
    fn only_the_position_zero_head_can_recover_a_hold_token() {
        assert_eq!(
            held_by_for_position(None, None, Some("working".into())),
            None
        );
        assert_eq!(
            held_by_for_position(Some(1), None, Some("working".into())),
            None
        );
        assert_eq!(
            held_by_for_position(Some(0), None, Some("working".into())),
            Some("working".into())
        );
        assert_eq!(
            held_by_for_position(Some(0), Some("blocked".into()), Some("working".into())),
            Some("blocked".into())
        );
    }

    // -----------------------------------------------------------------
    // Decline TOCTOU (fix G: modal must still match before the confirm)
    // -----------------------------------------------------------------

    #[test]
    fn modal_match_is_rechecked_by_rule_id() {
        let m = Manifest::parse(
            r#"
[agent]
id = "x"
display_name = "x"

[[rule]]
id = "update_modal"
state = "blocked_modal"
priority = 1300
region = "bottom_non_empty_lines(8)"
contains = ["FAKE-UPDATE-AVAILABLE"]
decline_keys = ["3", "Enter"]
auto_dismiss = true

[[rule]]
id = "other_modal"
state = "blocked_modal"
priority = 1200
region = "bottom_non_empty_lines(8)"
contains = ["OTHER-DIALOG"]
"#,
            std::path::Path::new("x.toml"),
        )
        .unwrap();
        assert!(modal_still_matches(
            &m,
            "t",
            "text\nFAKE-UPDATE-AVAILABLE\nmore",
            "update_modal"
        ));
        // Dialog vanished: never send the confirming key.
        assert!(!modal_still_matches(
            &m,
            "t",
            "plain shell output",
            "update_modal"
        ));
        // A DIFFERENT dialog appeared: the confirm belongs to nobody.
        assert!(!modal_still_matches(
            &m,
            "t",
            "OTHER-DIALOG",
            "update_modal"
        ));
    }

    // -----------------------------------------------------------------
    // Buffer hygiene (fix G: delete-buffer after a failed paste)
    // -----------------------------------------------------------------

    /// Real tmux on an isolated -L socket: when paste-buffer fails after
    /// load-buffer succeeded, the loaded buffer must not linger
    /// server-global with the payload in it.
    #[tokio::test]
    async fn paste_failure_deletes_the_loaded_buffer() {
        if !cyclops_testrig::tmux_available() {
            eprintln!("skipping: tmux not on PATH");
            return;
        }
        let pid = std::process::id();
        // The rig owns the server: teardown kills it AND unlinks its socket,
        // and runs on unwind, which a kill at the end of the body does not.
        let tmux = cyclops_testrig::TmuxServer::new("dubuf");
        let spool = cyclops_proto::scratch::scratch_dir("cyc-dubuf-spool");
        let cfg = cyclops_tmux::ControlConfig::new_session("dubuf")
            .on_socket(tmux.socket())
            .with_config_file("/dev/null")
            .with_buffer_spool_dir(&spool);
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("tmux spawns");
        let client = Arc::new(client);
        let injector = TmuxInjector {
            client: Arc::clone(&client),
            buffer: format!("cyc-{pid}-t"),
        };
        // %9999 does not exist: load-buffer succeeds, paste-buffer fails.
        injector.spool("secret payload").await.expect("spool");
        let err = injector.commit("%9999", &|| Ok(())).await.unwrap_err();
        assert_eq!(err, "paste_failed");
        let buffers = client.command("list-buffers").await.unwrap_or_default();
        assert!(
            buffers.iter().all(|l| !l.contains(&injector.buffer)),
            "buffer lingered after failed paste: {buffers:?}"
        );
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&spool);
    }
}

#[cfg(test)]
mod chip_proof {
    use super::*;

    fn chip_manifest() -> Manifest {
        Manifest::parse(
            r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+( \+\d+ lines)?\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+( \+\d+ lines)?\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#,
            std::path::Path::new("c.toml"),
        )
        .unwrap()
    }

    /// The chip is the alternate proof of a whole staged payload, so it
    /// has to be the whole row. Anything else on the row means the row is
    /// not the chip, and the payload around it is unaccounted for.
    #[test]
    fn a_chip_with_text_around_it_is_not_a_chip() {
        let m = chip_manifest();
        let good = "\u{1b}[39m❯\u{a0}[Pasted text #1 +8 lines]\n? for shortcuts";
        assert!(marker_in_composer(&m, good), "the measured row must pass");

        for bad in [
            "\u{1b}[39m❯\u{a0}[Pasted text #1 +8 lines] and then some",
            "\u{1b}[39m❯\u{a0}see [Pasted text #1 +8 lines]",
        ] {
            let screen = format!("{bad}\n? for shortcuts");
            assert!(
                !marker_in_composer(&m, &screen),
                "payload beside the chip must refuse: {bad:?}"
            );
        }

        // And a line typed UNDER the chip is payload nobody accounted
        // for: the chip is still exact, but it is no longer the last
        // thing in the composer.
        let after = "\u{1b}[39m❯\u{a0}[Pasted text #1 +8 lines]\nand then some\n? for shortcuts";
        assert!(
            !marker_in_composer(&m, after),
            "a row under the chip must refuse"
        );
    }

    /// The exact collision that made the old substring test unsafe: a
    /// message whose SUBJECT contains the word verified a paste whose
    /// sentinel had never arrived, and the truncated payload submitted
    /// itself.
    #[test]
    fn a_subject_containing_the_chip_words_never_verifies() {
        let m = chip_manifest();
        for row in [
            "\u{1b}[39m❯\u{a0}[cyclops m-1] FROM: codex  SUBJECT: Pasted text handling",
            "\u{1b}[39m❯\u{a0}[cyclops m-1] FROM: codex  SUBJECT: Pasted",
        ] {
            let screen = format!("{row}\n? for shortcuts");
            assert!(
                !marker_in_composer(&m, &screen),
                "a subject is not a chip: {row:?}"
            );
        }
    }

    /// A chip that scrolled into the transcript is not the composer's.
    #[test]
    fn a_transcript_echo_of_a_chip_never_verifies() {
        let m = chip_manifest();
        let echo = "you: [Pasted text #1 +8 lines]\n\u{1b}[39m❯\u{a0}\n? for shortcuts";
        assert!(!marker_in_composer(&m, echo));
    }

    /// Without an escaped capture the styling half cannot be checked.
    #[test]
    fn a_plain_capture_never_proves_a_chip() {
        let m = chip_manifest();
        assert!(!marker_in_composer(
            &m,
            "❯ [Pasted text #1 +8 lines]\n? for shortcuts"
        ));
    }
}

#[cfg(test)]
mod shipped_chip_proof {
    use super::tests::{sentinel_manifest, MockInjector, CHROME};
    use super::*;
    use cyclops_proto::MessageId;

    fn claude() -> Manifest {
        Manifest::parse(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/claude.toml"
            )),
            std::path::Path::new("claude.toml"),
        )
        .expect("shipped claude manifest parses")
    }

    fn codex() -> Manifest {
        Manifest::parse(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/codex.toml"
            )),
            std::path::Path::new("codex.toml"),
        )
        .expect("shipped codex manifest parses")
    }

    /// Codex paints a blank row between the composer and its model status
    /// row, so a raw-wrapped sentinel's suffix starts with a row the
    /// layout has to describe. It did not, and every raw-wrapped codex
    /// delivery refused: correct fail-closed behaviour, and a whole
    /// vendor lane with no sentinel path.
    ///
    /// The chrome here is verbatim from a real capture. The composer row
    /// is synthetic, so this proves only the declared trailer layout.
    #[test]
    fn a_codex_raw_wrap_verifies_through_its_measured_blank_separator() {
        let real = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_pasted_chip_esc.txt"
        ));
        let mut rows: Vec<String> = real.split('\n').map(str::to_string).collect();
        let chip = rows
            .iter()
            .position(|r| r.contains("[Pasted Content"))
            .expect("the real capture's composer row");
        // A raw wrap puts the payload in the composer instead of a chip,
        // and the sentinel is its last row.
        rows[chip] = "\u{1b}[1m›\u{1b}[0m the last line of the body".to_string();
        rows.insert(chip + 1, sentinel_for("m-9f1"));
        let screen = rows.join("\n");
        assert!(
            sentinel_verified(&codex(), &screen, "m-9f1"),
            "the shipped codex layout still refuses its own chrome:\n{screen}"
        );

        // The blank row is declared, not ignored. A SECOND blank is a row
        // the layout does not describe, which is what a truncated capture
        // looks like, and it still refuses.
        let mut extra = rows.clone();
        extra.insert(chip + 2, String::new());
        assert!(
            !sentinel_verified(&codex(), &extra.join("\n"), "m-9f1"),
            "an undeclared blank row was accepted"
        );
    }

    /// The shipped manifest against real captures, through the production
    /// proof rather than an inline fixture shaped to suit it.
    ///
    /// An inline manifest proves the algorithm; it cannot prove that the
    /// patterns Cyclops actually ships match the screens Claude actually
    /// draws. Those are different claims, and only the second one is
    /// about delivering a message to a real agent.
    #[test]
    fn the_shipped_claude_chip_verifies_and_its_echo_does_not() {
        let m = claude();
        let staged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_pasted_chip_esc.txt"
        ));
        assert!(
            marker_in_composer(&m, staged),
            "the shipped chip row must prove a staged paste"
        );

        // The prompt-echo capture is the same CLI with no chip on the
        // composer: whatever else is on screen, nothing there is a staged
        // payload, and claiming otherwise would submit on a redraw.
        let echo = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_prompt_echo_esc.txt"
        ));
        assert!(
            !marker_in_composer(&m, echo),
            "an echo with no composer chip must not verify"
        );

        // The plain sibling of that capture is what a manifest without
        // escaped rules would be handed. The chip proof needs the styling
        // half, so this refuses too, and the two fixtures are now both
        // load-bearing rather than one of them sitting unreferenced.
        let echo_plain = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_prompt_echo_plain.txt"
        ));
        assert!(!marker_in_composer(&m, echo_plain));
    }

    /// The halves disagreeing is the case an OR cannot survive: the chip
    /// row renders exactly as the vendor draws it, the escaped composer
    /// clause holds, and the plain one does not. Under "plain OR escaped"
    /// the escaped half alone would carry the proof; under the manifest's
    /// own semantics both are required and this refuses.
    #[test]
    fn a_row_that_satisfies_only_the_escaped_clause_refuses() {
        let m = Manifest::parse(
            r#"
[agent]
id = "d"
display_name = "d"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^NEVER-MATCHES-THIS-ROW$']
line_regex_esc = ['\x1b\[39m❯\x{a0}']

[injection]
composer_chip_regex = ['^\s*❯\s+\[Pasted text #\d+\]\s*$']
composer_chip_regex_esc = ['\x1b\[39m❯\x{a0}\[Pasted text #\d+\]']
composer_trailer_regex = ['^\? for shortcuts\s*$']
composer_trailer_regex_esc = ['\? for shortcuts']
composer_trailer_required_prefix = 1
"#,
            std::path::Path::new("d.toml"),
        )
        .unwrap();
        let row = "\u{1b}[39m❯\u{a0}[Pasted text #1]";
        assert!(
            !marker_in_composer(&m, row),
            "one satisfied clause is not the rule the manifest wrote"
        );
    }

    /// Both shipped clauses have to carry the proof, so breaking either
    /// one must make production refuse.
    ///
    /// This is the half of the contract a passing test cannot show on its
    /// own: with an OR, the escaped clause alone kept the proof alive and
    /// the vendor's plain pattern could rot untouched for as long as
    /// nobody looked. Each half is broken here in turn, against the same
    /// real capture, and each break must be fatal.
    #[test]
    fn breaking_either_shipped_clause_refuses_the_chip() {
        let staged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_pasted_chip_esc.txt"
        ));
        assert!(marker_in_composer(&claude(), staged), "baseline");

        let shipped = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../resources/manifests/claude.toml"
        ));
        for (half, broken) in [
            (
                "plain",
                shipped.replace(
                    "line_regex = ['^\\s*❯\\s+\\S']",
                    "line_regex = ['^NEVER-MATCHES-THIS-ROW$']",
                ),
            ),
            (
                "escaped",
                shipped.replace(
                    "line_regex_esc = ['\\x1b\\[39m❯\\x{a0}[^\\x1b]']",
                    "line_regex_esc = ['NEVER-MATCHES-THIS-ROW']",
                ),
            ),
        ] {
            assert_ne!(broken, shipped, "the {half} clause moved; update this test");
            let m = Manifest::parse(&broken, std::path::Path::new("claude.toml"))
                .expect("broken manifest still parses");
            assert!(
                !marker_in_composer(&m, staged),
                "breaking the shipped {half} clause must refuse the chip"
            );
        }
    }

    #[test]
    fn compact_doorbell_verifies_as_one_exact_row_at_narrow_width() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-0123456789abcdef0123456789abcdef")
            .expect("valid generated message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);
        let screen = format!("\u{1b}[39m❯ {doorbell}\n{CHROME}");

        assert!(2 + doorbell.chars().count() <= 60);
        assert!(
            exact_row_verified(&m, &screen, &doorbell),
            "exact doorbell in composer must verify"
        );
        assert_eq!(
            staged_verified_target(&m, &screen, StagingTarget::ExactRow(&doorbell)),
            Some(true),
            "target helper must return true for exact row staging"
        );
    }

    #[test]
    fn compact_doorbell_verifies_across_claudes_measured_status_widths() {
        let manifest = claude();
        let msg_id = MessageId::new("m-0123456789abcdef0123456789abcdef")
            .expect("valid generated message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);
        let statuses = [
            (60, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cy…\u{1b}[39m"),
            (80, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cyclops-worktrees/mess…\u{1b}[39m"),
            (100, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cyclops-worktrees/messaging-integration\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;72m…\u{1b}[39m"),
            (125, "\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m~/projects/agentic_dev/cyclops-worktrees/messaging-integration\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;72mCtx: 95%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m5h: 93%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m7d: …\u{1b}[39m"),
        ];

        for (width, status) in statuses {
            let rule = "─".repeat(width);
            let screen = format!("\u{1b}[39m❯\u{a0}{doorbell}\n\u{1b}[38;5;244m{rule}\n{status}");
            assert!(
                exact_row_verified(&manifest, &screen, &doorbell),
                "the measured {width}-column Claude trailer must preserve exact proof"
            );
            assert_eq!(
                exact_composer_content_from_joined_capture(&manifest, &screen),
                ComposerContentProof::Visible(doorbell.clone()),
                "attention extraction must agree at {width} columns"
            );
        }
    }

    #[test]
    fn application_wrapped_exact_row_remains_unverified() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-0123456789abcdef0123456789abcdef")
            .expect("valid generated message id");
        let doorbell = cyclops_proto::render_legacy_doorbell(&msg_id);
        let split = doorbell
            .find("m-0123456789abcdef0123456789abcdef'")
            .expect("second message id");
        let screen = format!(
            "\u{1b}[39m❯ {}\n  {}\n{CHROME}",
            doorbell[..split].trim_end(),
            &doorbell[split..]
        );

        assert!(
            !exact_row_verified(&m, &screen, &doorbell),
            "application line breaks are not exact bytes and must fail closed"
        );
        assert_eq!(
            staged_verified_target(&m, &screen, StagingTarget::ExactRow(&doorbell)),
            None
        );
    }

    #[test]
    fn staged_verified_target_accepts_collapsed_chip_for_exact_row() {
        let m = claude();
        let staged = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/claude_pasted_chip_esc.txt"
        ));
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        assert_eq!(
            staged_verified_target(&m, staged, StagingTarget::ExactRow(&doorbell)),
            Some(false),
            "collapsed chip must verify as alternate staging without raw text"
        );
    }

    #[test]
    fn human_draft_in_composer_refuses_verification() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        // Case 1: Human typed draft before the doorbell row on the same line
        let screen_before = format!("\u{1b}[39m❯ draft prefix text {doorbell}\n{CHROME}");
        assert!(
            !exact_row_verified(&m, &screen_before, &doorbell),
            "draft text before doorbell must refuse"
        );

        // Case 2: Human typed draft after the doorbell row on the same line
        let screen_after = format!("\u{1b}[39m❯ {doorbell} draft suffix text\n{CHROME}");
        assert!(
            !exact_row_verified(&m, &screen_after, &doorbell),
            "draft text after doorbell must refuse"
        );

        // Case 3: Human draft on a row between doorbell and chrome
        let screen_multi = format!("\u{1b}[39m❯ {doorbell}\nhuman second line\n{CHROME}");
        assert!(
            !exact_row_verified(&m, &screen_multi, &doorbell),
            "multiline human draft below doorbell must refuse"
        );

        // Case 4: Separate human draft row before the doorbell row and chrome after it (adversarial capture)
        let screen_draft_above = format!(
            "\u{1b}[39m❯ my unfinished thought\n\
             {doorbell}\n\
             {CHROME}"
        );
        assert!(
            !exact_row_verified(&m, &screen_draft_above, &doorbell),
            "separate human draft row before doorbell must refuse"
        );
        assert_eq!(
            staged_verified_target(&m, &screen_draft_above, StagingTarget::ExactRow(&doorbell)),
            None,
            "adversarial draft above doorbell must return None"
        );

        // Case 5: Prompt on draft row above and prompt on doorbell row below
        let screen_two_prompts = format!(
            "\u{1b}[39m❯ my unfinished thought\n\
             \u{1b}[39m❯ {doorbell}\n\
             {CHROME}"
        );
        assert!(
            !exact_row_verified(&m, &screen_two_prompts, &doorbell),
            "two prompt rows in composer must refuse"
        );
    }

    #[test]
    fn exact_composer_diff_extracts_only_the_active_prompt() {
        let manifest = sentinel_manifest();
        let message_id = MessageId::new("m-3f9c2a").unwrap();
        let doorbell = cyclops_proto::render_doorbell_v1(&message_id);
        let screen = format!(
            "\u{1b}[1;2m❯ old transcript prompt\u{1b}[0m\n\
             \u{1b}[39m❯ {doorbell} trailing human input\n{CHROME}"
        );

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &screen),
            ComposerContentProof::Visible(format!("{doorbell} trailing human input"))
        );
    }

    #[test]
    fn exact_composer_diff_refuses_two_active_prompts() {
        let manifest = sentinel_manifest();
        let screen = format!(
            "\u{1b}[39m❯ first staged row\n\
             \u{1b}[39m❯ second staged row\n{CHROME}"
        );

        assert_eq!(
            exact_composer_content_from_joined_capture(&manifest, &screen),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn modal_dialog_blocking_composer_refuses_verification() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        // Screen where a modal dialog is present instead of the composer trailer chrome
        let screen_modal = format!(
            "\u{1b}[39m❯ {doorbell}\n\
             \u{1b}[31m[Modal: Do you trust this folder? (y/n)]\u{1b}[0m\n\
             \u{1b}[90m[Press Enter to confirm, Esc to cancel]\u{1b}[0m"
        );
        assert!(
            !exact_row_verified(&m, &screen_modal, &doorbell),
            "modal dialog blocking composer must refuse staging verification"
        );
        assert_eq!(
            staged_verified_target(&m, &screen_modal, StagingTarget::ExactRow(&doorbell)),
            None
        );
    }

    #[tokio::test]
    async fn inject_verifies_exact_row_target() {
        let m = sentinel_manifest();
        let handle = DeliveryHandle::new("m-exact01", "worker", "%1", 0, "payload".into());
        let msg_id = MessageId::new(&handle.msg_id).expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);
        let screen = format!("\u{1b}[39m❯ {doorbell}\n{CHROME}");
        let mock = MockInjector::new(vec![screen.as_str()]);
        mock.spool(&doorbell).await.expect("spool");
        let (window, id_staged, proof) = inject(
            &mock,
            &handle,
            &m,
            StagingTarget::ExactRow(&doorbell),
            &|| Ok(()),
        )
        .await
        .expect("exact row inject must succeed");
        assert!(id_staged);
        assert!(proof.contains(&doorbell));
        assert!(window.contains(&doorbell));
    }

    #[test]
    fn exact_row_changed_before_submit_recheck_withholds_enter() {
        let m = sentinel_manifest();
        let msg_id = MessageId::new("m-3f9c2a").expect("valid message id");
        let doorbell = cyclops_proto::render_doorbell_v1(&msg_id);

        let initial_screen = format!("\u{1b}[39m❯ {doorbell}\n{CHROME}");
        let initial_staged =
            staged_verified_target(&m, &initial_screen, StagingTarget::ExactRow(&doorbell));
        let initial_proof =
            payload_proof_target(&m, &initial_screen, StagingTarget::ExactRow(&doorbell));

        assert_eq!(initial_staged, Some(true));
        assert!(
            initial_proof
                .as_deref()
                .map(|p| p.contains(&doorbell))
                .unwrap_or(false),
            "initial proof must contain doorbell"
        );

        // Case 1: Human typed additional text after the doorbell before Enter is sent
        let changed_after = format!("\u{1b}[39m❯ {doorbell} append edit\n{CHROME}");
        let recheck_staged =
            staged_verified_target(&m, &changed_after, StagingTarget::ExactRow(&doorbell));
        let recheck_proof =
            payload_proof_target(&m, &changed_after, StagingTarget::ExactRow(&doorbell));

        let would_submit = recheck_proof.as_deref() == initial_proof.as_deref()
            && recheck_staged == initial_staged;
        assert!(
            !would_submit,
            "recheck must detect changed text after doorbell and withhold enter"
        );

        // Case 2: Human inserted a draft row above the doorbell before Enter is sent
        let changed_above = format!("\u{1b}[39m❯ draft line\n{doorbell}\n{CHROME}");
        let recheck_above_staged =
            staged_verified_target(&m, &changed_above, StagingTarget::ExactRow(&doorbell));
        let recheck_above_proof =
            payload_proof_target(&m, &changed_above, StagingTarget::ExactRow(&doorbell));

        let would_submit_above = recheck_above_proof.as_deref() == initial_proof.as_deref()
            && recheck_above_staged == initial_staged;
        assert!(
            !would_submit_above,
            "recheck must detect draft row above doorbell and withhold enter"
        );
    }
}

#[cfg(test)]
mod composer_content_proof {
    use super::*;

    const PROBE_BODY: &str = "This is a deliberately long composer-only probe that wraps across physical terminal rows without being submitted to any model, and it contains punctuation: [] {} <> ! ? plus Unicode λ 漢字.";
    const CLAUDE_TRAILER: &str = "\u{1b}[38;5;244m────────────────────────────────────────────────────────────────────────────────\n\u{1b}[39m  \u{1b}[38;5;174mOpus 5 (1M context)\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;216mxhigh\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;230m/tmp/project\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m5h: 92%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;181m7d: 36%\u{1b}[38;5;246m \u{1b}[2m·\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;180m1000K window\u{1b}[39m";

    fn decoded_fixture(hex: &str) -> String {
        let compact: String = hex
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert_eq!(compact.len() % 2, 0, "fixture has a partial byte");
        let bytes = compact
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("hex is ASCII");
                u8::from_str_radix(pair, 16).expect("fixture contains hex")
            })
            .collect();
        String::from_utf8(bytes).expect("capture is UTF-8")
    }

    fn shipped(id: &str) -> Manifest {
        let source = match id {
            "claude" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/claude.toml"
            )),
            "codex" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/codex.toml"
            )),
            "agy" => include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../resources/manifests/agy.toml"
            )),
            _ => panic!("unknown shipped manifest {id}"),
        };
        Manifest::parse(source, std::path::Path::new(id)).expect("shipped manifest parses")
    }

    fn claude_capture(payload: &str) -> String {
        let mut rows = payload.lines();
        let first = rows.next().expect("payload has an envelope");
        let mut screen = format!("\u{1b}[39m❯\u{a0}{first}");
        for row in rows {
            screen.push_str("\n  ");
            screen.push_str(row);
        }
        screen.push('\n');
        screen.push_str(CLAUDE_TRAILER);
        screen
    }

    #[test]
    fn current_raw_captures_extract_the_rebuilt_payload() {
        let expected = render_payload("m-wrapprobe", "test", "exact wrap probe", PROBE_BODY, true);
        for (vendor, capture) in [
            (
                "claude",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/claude_raw_composer_2_1_239_esc.txt"
                ))
                .to_string(),
            ),
            (
                "codex",
                decoded_fixture(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/codex_raw_composer_0_149_0_esc.hex"
                ))),
            ),
        ] {
            assert_eq!(
                composer_content_from_joined_capture(&shipped(vendor), &capture, "m-wrapprobe"),
                ComposerContentProof::Visible(expected.clone()),
                "{vendor} did not reconstruct the rendered payload"
            );
        }
    }

    #[test]
    fn joined_capture_preserves_payload_trailing_spaces() {
        let expected = render_payload("m-space", "test", "spaces", "body", true);
        let edited_capture = claude_capture(&expected).replace(
            "  body\n  [cyclops:end m-space]",
            "  body \n  [cyclops:end m-space]",
        );
        let edited_payload = expected.replace("body\n", "body \n");
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &edited_capture, "m-space"),
            ComposerContentProof::Visible(edited_payload)
        );

        let edited_sentinel = claude_capture(&expected)
            .replace("  [cyclops:end m-space]\n", "  [cyclops:end m-space] \n");
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &edited_sentinel, "m-space"),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn codex_hex_fixtures_preserve_measured_trailing_cells() {
        let raw = decoded_fixture(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_raw_composer_0_149_0_esc.hex"
        )));
        let collapsed = decoded_fixture(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../cyclops-manifest/tests/fixtures/codex_collapsed_chip_0_149_0_esc.hex"
        )));
        let raw_rows: Vec<&str> = raw.lines().collect();
        let collapsed_rows: Vec<&str> = collapsed.lines().collect();
        assert!(raw_rows[0].ends_with(' '));
        assert_eq!(raw_rows[4], " ");
        assert!(collapsed_rows[0].ends_with(' '));
        assert!(collapsed_rows[2].ends_with(' '));
    }

    #[test]
    fn prompt_may_be_outside_the_sentinel_search_window() {
        let body = (0..24)
            .map(|line| format!("body line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let expected = render_payload("m-long", "test", "long", &body, true);
        let capture = claude_capture(&expected);
        let prompt_at = composer_rows(&capture)
            .iter()
            .position(|(_, plain)| plain.starts_with("❯"))
            .expect("prompt row");
        assert!(
            composer_rows(&capture).len() - prompt_at > VERIFY_REGION,
            "fixture did not put the prompt outside the search window"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &capture, "m-long"),
            ComposerContentProof::Visible(expected)
        );
    }

    #[test]
    fn transcript_echoes_and_multiple_matching_headers_refuse() {
        let expected = render_payload("m-echo", "test", "current", "new body", true);
        let active = claude_capture(&expected);
        let echoed = format!(
            "\u{1b}[38;5;239m\u{1b}[48;5;237m❯ \u{1b}[38;5;231m[cyclops m-echo] FROM: test  SUBJECT: old\u{1b}[39m\n  old body\n  [cyclops:end m-echo]\nassistant response\n{active}"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &echoed, "m-echo"),
            ComposerContentProof::Unprovable,
            "two same-id headers are ambiguous even when one resembles a transcript"
        );

        let empty_composer = format!(
            "\u{1b}[38;5;239m\u{1b}[48;5;237m❯ \u{1b}[38;5;231m[cyclops m-echo] FROM: test  SUBJECT: old\u{1b}[39m\n  old body\n  [cyclops:end m-echo]\nassistant response\n\u{1b}[39m❯\u{a0}\n{CLAUDE_TRAILER}"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &empty_composer, "m-echo"),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn repeated_sentinel_and_trailing_content_refuse() {
        let repeated = render_payload(
            "m-repeat",
            "test",
            "quoted sentinel",
            "before\n[cyclops:end m-repeat]\nafter",
            true,
        );
        assert_eq!(
            composer_content_from_joined_capture(
                &shipped("claude"),
                &claude_capture(&repeated),
                "m-repeat"
            ),
            ComposerContentProof::Unprovable
        );

        let expected = render_payload("m-trail", "test", "trailing", "body", true);
        let capture = claude_capture(&expected).replace(
            "  [cyclops:end m-trail]\n\u{1b}[38;5;244m",
            "  [cyclops:end m-trail]\n  human addition\n\u{1b}[38;5;244m",
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &capture, "m-trail"),
            ComposerContentProof::Unprovable
        );

        let distant_body = std::iter::once("[cyclops:end m-distant]".to_string())
            .chain((0..20).map(|line| format!("body line {line:02}")))
            .collect::<Vec<_>>()
            .join("\n");
        let distant = render_payload(
            "m-distant",
            "test",
            "distant duplicate",
            &distant_body,
            true,
        );
        let distant_capture = claude_capture(&distant);
        let rows = joined_composer_rows(&distant_capture);
        let first_duplicate = rows
            .iter()
            .position(|(_, plain)| plain == "  [cyclops:end m-distant]")
            .expect("body sentinel");
        assert!(
            first_duplicate < rows.len().saturating_sub(VERIFY_REGION),
            "duplicate remained inside the bounded search window"
        );
        assert_eq!(
            composer_content_from_joined_capture(&shipped("claude"), &distant_capture, "m-distant"),
            ComposerContentProof::Unprovable
        );
    }

    #[test]
    fn payload_chrome_shapes_spaces_and_blank_lines_are_preserved() {
        let body = "  indented\n\n────────────────────────────────\nOpus 5 · xhigh · /tmp/x · 5h: 1% · 7d: 2% · 1000K window";
        let expected = render_payload("m-shape", "test", "shapes", body, false);
        assert_eq!(
            composer_content_from_joined_capture(
                &shipped("claude"),
                &claude_capture(&expected),
                "m-shape"
            ),
            ComposerContentProof::Visible(expected)
        );
    }

    #[test]
    fn collapsed_chips_are_hidden_and_unmeasured_vendors_are_unsupported() {
        for (vendor, capture) in [
            (
                "claude",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/claude_collapsed_chip_2_1_239_esc.txt"
                ))
                .to_string(),
            ),
            (
                "codex",
                decoded_fixture(include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../cyclops-manifest/tests/fixtures/codex_collapsed_chip_0_149_0_esc.hex"
                ))),
            ),
        ] {
            assert_eq!(
                composer_content_from_joined_capture(&shipped(vendor), &capture, "m-hidden"),
                ComposerContentProof::Hidden,
                "{vendor} chip bytes were treated as visible"
            );
        }
        assert_eq!(
            composer_content_from_joined_capture(&shipped("agy"), "anything", "m-unsupported"),
            ComposerContentProof::Unsupported
        );
    }
}
