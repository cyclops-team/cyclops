//! In-memory projection and query model for recipient mailboxes.
//!
//! Reconstructs deterministic mailbox states (pending and claimed entries)
//! from a single authoritative workspace journal.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use cyclops_ledger::{now_ms, LedgerError, LedgerWriter};
use cyclops_proto::{
    doorbell_format_names_exact_attempt, Event, InboxMessage, Kind, LedgerLine, MailboxEntry,
    MailboxEntryState, MailboxFact, MailboxListItem, MailboxTypeError, MessageDirection, MessageId,
    MessageMetadata, MessageNotificationSettlement, MessageNotificationState,
    MessageNotificationSummary, MessagePresentation, MessageQuotaState, MessageRecipientRoute,
    MessageRecipientSummary, MessageSnapshotRow, MessageWakeBlock, MessagesChangedArea,
    MessagesChangedData, MessagesFollowResult, MessagesSnapshotCounts, MessagesSnapshotResult,
    NotificationAttemptId, NotificationAttentionCause, NotificationBarrierRetirementCause,
    NotificationBinding, NotificationFact, NotificationPreWriteCause,
    NotificationPreWriteObservation, NotificationRecord, NotificationRequeue,
    NotificationResolution, NotificationResolutionConsumptionEvidence,
    NotificationResolutionConsumptionObservation, NotificationRouteEvidenceId, NotificationState,
    NotificationTransport, NotificationVerifyOutcome, ProcessInstanceId, RecipientKey,
    RequestContent, RequestDigest, StatusBlockedNotification, StatusNextAction, TmuxPaneId,
    WorkspaceId, CANONICAL_RECORD_VERSION, DOORBELL_FORMAT_ATTEMPT_CLAIM,
    DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM, DOORBELL_FORMAT_COMPACT_CLAIM,
    NOTIFICATION_RESOLUTION_PROOF_VERSION,
};
use cyclops_state::StateRoot;
use tokio::sync::broadcast;

/// Draft message parameters for pre-append acceptance verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDraft {
    pub kind: Kind,
    pub sender: RecipientKey,
    pub recipients: Vec<RecipientKey>,
    pub subject: Option<String>,
    pub summary: Option<String>,
    pub body: Option<String>,
    pub client_key: Option<String>,
    pub supersedes: Option<MessageId>,
    pub presentation: MessagePresentation,
}

/// One active composer barrier with the durable message and mailbox facts
/// needed to reconstruct its expected bytes. This value stays daemon-local.
#[derive(Debug, Clone)]
pub(crate) struct ActiveComposerNotification {
    pub(crate) record: NotificationRecord,
    pub(crate) message: Option<LedgerLine>,
    pub(crate) entry_state: Option<MailboxEntryState>,
    pub(crate) recovery_action: ExactOwnedRecoveryAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactOwnedRecoveryAction {
    Ineligible,
    Submit,
    Clear,
    Reconcile,
    Inspect,
}

/// Bounded body-free status projection of durable pre-write failures.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlockedNotificationSnapshot {
    pub(crate) rows: Vec<StatusBlockedNotification>,
    pub(crate) total: u64,
}

/// A reply request whose routing and subject are derived from its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyDraft {
    pub sender: RecipientKey,
    pub reference: MessageId,
    pub summary: Option<String>,
    pub body: Option<String>,
    pub client_key: Option<String>,
    pub sender_label: String,
    /// The destination's label AS IT IS NOW, resolved from the directory
    /// against the durable destination key. A reply is a new message and
    /// presents a current name; the parent keeps its historical sender
    /// label in its own fact, which this never rewrites.
    pub recipient_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalDraft {
    kind: Kind,
    sender: RecipientKey,
    recipients: Vec<RecipientKey>,
    subject: Option<String>,
    summary: Option<String>,
    body: Option<String>,
    reply_to: Option<MessageId>,
    client_key: Option<String>,
    supersedes: Option<MessageId>,
    presentation: MessagePresentation,
}

/// Result of pre-append idempotency verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptanceOutcome {
    /// New message accepted; contains validated semantic request digest to persist in metadata.
    New { request_digest: RequestDigest },
    /// Identical retry recognized; returns existing message identifier without appending a second record.
    Existing(MessageId),
}

/// Durable acceptance result used to suppress duplicate notification work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptResult {
    pub message_id: MessageId,
    pub inserted: bool,
    pub seq: u64,
    pub recipients: Vec<String>,
    pub recipient_keys: Vec<RecipientKey>,
}

/// Payload-bearing outcome of claiming a mailbox entry.
#[derive(Debug, Clone)]
pub enum ClaimOutcome {
    /// Entry successfully claimed; returns projected entry and canonical message line.
    Claimed {
        entry: MailboxEntry,
        message: InboxMessage,
        /// Oldest pending message immediately before this claim fact was
        /// appended, when this claim took a later FIFO entry.
        skipped_oldest: Option<MessageId>,
        withdrawn_attempt: Option<NotificationAttemptId>,
        consumed_doorbell_attempt: Option<NotificationAttemptId>,
        claimed_ack_timeout_attempt: Option<NotificationAttemptId>,
    },
    /// Entry was already claimed by this claimant; returns existing entry and canonical message line.
    AlreadyClaimed {
        entry: MailboxEntry,
        message: InboxMessage,
        withdrawn_attempt: Option<NotificationAttemptId>,
        consumed_doorbell_attempt: Option<NotificationAttemptId>,
        claimed_ack_timeout_attempt: Option<NotificationAttemptId>,
    },
}

/// Errors occurring during mailbox fact application or ledger reconstruction.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MailboxError {
    #[error("message '{0}' not found")]
    MessageNotFound(MessageId),
    #[error("sender '{sender}' cannot reply to message '{reply_to}' because it is not visible")]
    ReplyNotVisible {
        reply_to: MessageId,
        sender: RecipientKey,
    },
    #[error(
        "reply '{message_id}' must have exactly the referenced sender '{expected}' as recipient"
    )]
    ReplyRecipientMismatch {
        message_id: MessageId,
        expected: RecipientKey,
    },
    #[error("reply '{message_id}' has a subject that does not match its referenced message")]
    ReplySubjectMismatch { message_id: MessageId },
    #[error("reply '{message_id}' must use message kind")]
    ReplyKindMismatch { message_id: MessageId },
    #[error(
        "message '{message_id}' has thread root '{found}', but reply ancestry requires '{expected}'"
    )]
    ThreadRootMismatch {
        message_id: MessageId,
        expected: MessageId,
        found: MessageId,
    },
    #[error("entry for recipient '{recipient}' on message '{message_id}' not found")]
    EntryNotFound {
        message_id: MessageId,
        recipient: RecipientKey,
    },
    #[error("message '{message_id}' for recipient '{recipient}' already claimed by '{existing_claimant}'")]
    AlreadyClaimed {
        message_id: MessageId,
        recipient: RecipientKey,
        existing_claimant: RecipientKey,
    },
    #[error(
        "claimant '{claimant}' does not match recipient '{recipient}' on message '{message_id}'"
    )]
    ClaimantMismatch {
        message_id: MessageId,
        recipient: RecipientKey,
        claimant: RecipientKey,
    },
    #[error("state envelope id '{envelope_id}' does not match fact message id '{fact_id}'")]
    EnvelopeMismatch {
        envelope_id: String,
        fact_id: MessageId,
    },
    #[error("duplicate message id in workspace journal: '{0}'")]
    DuplicateMessageId(MessageId),
    #[error("non-contiguous workspace journal sequence: expected {expected}, found {found}")]
    NonContiguousSequence { expected: u64, found: u64 },
    #[error("idempotency key '{key}' from sender '{sender}' already used by message '{existing_id}' with conflicting request digest")]
    DuplicateIdempotencyKey {
        sender: RecipientKey,
        key: String,
        existing_id: MessageId,
    },
    #[error("message draft has empty recipient set")]
    DraftEmptyRecipients,
    #[error("message draft contains duplicate recipient '{0}'")]
    DraftDuplicateRecipient(RecipientKey),
    #[error("message '{message_id}' has empty recipient set in journal")]
    EmptyRecipients { message_id: MessageId },
    #[error("message '{message_id}' contains duplicate recipient '{recipient}' in journal")]
    DuplicateRecipient {
        message_id: MessageId,
        recipient: RecipientKey,
    },
    #[error("client key cannot be empty if specified")]
    EmptyClientKey,
    #[error("supersession requires exactly one recipient")]
    SupersessionRequiresSingleRecipient,
    #[error("message '{0}' cannot be superseded because it is not pending")]
    SupersessionNotPending(MessageId),
    #[error("message '{0}' cannot be superseded because notification writing has started")]
    SupersessionNotificationStarted(MessageId),
    #[error("message '{0}' is no longer pending")]
    MessageNotPending(MessageId),
    #[error("message '{0}' cannot be superseded by a different sender or recipient")]
    SupersessionIdentityMismatch(MessageId),
    #[error("a reply cannot also supersede a message")]
    ReplySupersessionConflict,
    #[error("invalid message presentation: {0}")]
    InvalidPresentation(String),
    #[error("unsupported event kind for mailbox messaging: {0:?}")]
    InvalidKind(Kind),
    #[error("workspace mismatch: expected '{expected}', found '{found}'")]
    WorkspaceMismatch {
        expected: WorkspaceId,
        found: WorkspaceId,
    },
    #[error("message cursor {cursor} is ahead of workspace head {head}")]
    InvalidCursor { cursor: u64, head: u64 },
    #[error("invalid record version: expected {expected}, found {found}")]
    InvalidRecordVersion { expected: u32, found: u32 },
    #[error("uncanonical journal row: {0}")]
    UncanonicalRow(String),
    #[error("presentation mismatch for field '{field}': presentation '{presentation}' contradicts authoritative '{authoritative}'")]
    PresentationMismatch {
        field: &'static str,
        presentation: String,
        authoritative: String,
    },
    #[error("request digest mismatch: stored '{stored}', recomputed '{computed}'")]
    DigestMismatch {
        stored: RequestDigest,
        computed: RequestDigest,
    },
    #[error("missing or invalid MessageMetadata on message line: '{0}'")]
    MissingMetadata(String),
    #[error("invalid mailbox fact: {0}")]
    InvalidFact(String),
    #[error("notification for message '{message_id}' and recipient '{recipient}' not found")]
    NotificationNotFound {
        message_id: MessageId,
        recipient: RecipientKey,
    },
    #[error("notification requires a pending mailbox entry for message '{message_id}' and recipient '{recipient}'")]
    NotificationMessageNotPending {
        message_id: MessageId,
        recipient: RecipientKey,
    },
    #[error("direct delivery is not proven by the notified attempt '{attempt_id}'")]
    DirectDeliveryNotProven { attempt_id: NotificationAttemptId },
    #[error("notification attempt '{0}' is already used in this workspace")]
    NotificationAttemptReused(NotificationAttemptId),
    #[error("notification fact attempt '{found}' does not match current attempt '{expected}'")]
    NotificationAttemptMismatch {
        expected: NotificationAttemptId,
        found: NotificationAttemptId,
    },
    #[error("illegal notification transition from {from:?} to {to:?}")]
    InvalidNotificationTransition {
        from: NotificationState,
        to: NotificationState,
    },
    #[error("notification Writing transition requires one durable binding")]
    NotificationBindingRequired,
    #[error("notification binding is only allowed on the Writing transition")]
    NotificationBindingForbidden,
    #[error("notification binding recipient does not match the fact recipient")]
    NotificationBindingMismatch,
    #[error("notification transport is only allowed on the Writing transition")]
    NotificationTransportForbidden,
    #[error("notification doorbell format is only allowed on a Doorbell Writing transition")]
    NotificationDoorbellFormatForbidden,
    #[error("unsupported notification doorbell format '{0}'")]
    UnsupportedNotificationDoorbellFormat(u32),
    #[error("notification attention transition requires one post-write cause")]
    NotificationCauseRequired,
    #[error("notification cause is only allowed on an attention transition")]
    NotificationCauseForbidden,
    #[error("verify_failed requires a content-free verification outcome")]
    NotificationVerifyOutcomeRequired,
    #[error("verification outcome is only valid for verify_failed attention")]
    NotificationVerifyOutcomeForbidden,
    #[error("a blocked pre-write notification requires one pre-write cause")]
    NotificationPreWriteCauseRequired,
    #[error("a blocked pre-write notification requires the selected gate manifest")]
    NotificationPreWriteObservationRequired,
    #[error(
        "pane-too-narrow notification width '{observed}' is not below required width '{required}'"
    )]
    NotificationPaneWidthNotNarrow { observed: u32, required: u32 },
    #[error("notification attempt already used its automatic pre-write reopen")]
    NotificationPreWriteReopenExhausted,
    #[error("pre-write evidence is only allowed on a blocked pre-write transition")]
    NotificationPreWriteCauseForbidden,
    #[error("notification cause {cause:?} is invalid after state {state:?}")]
    InvalidNotificationCause {
        cause: NotificationAttentionCause,
        state: NotificationState,
    },
    #[error("notification recipient must be an agent mailbox, not admin")]
    NotificationRecipientNotAgent,
    #[error("notification requeue requires attention or an observed quota reset")]
    NotificationRequeueRequiresAttention,
    #[error(
        "notification attempt '{0}' has an unresolved post-write composer barrier with an incomplete durable binding"
    )]
    NotificationRequeueBarrierBindingIncomplete(NotificationAttemptId),
    #[error(
        "notification attempt '{0}' still owns an exact staged composer notification; resolve it before requeueing"
    )]
    NotificationRequeueExactComposerBarrier(NotificationAttemptId),
    #[error("notification clearance requires an attention-required attempt")]
    NotificationClearRequiresAttention,
    #[error(
        "notification attempt '{attempt_id}' at {updated_at} is newer than cutoff {cutoff_ms}"
    )]
    NotificationNewerThanClearCutoff {
        attempt_id: NotificationAttemptId,
        updated_at: u64,
        cutoff_ms: u64,
    },
    #[error("notification clearance requires the workspace administrator")]
    NotificationClearOperatorInvalid,
    #[error("notification withdrawal requires a queued, gating, or blocked pre-write attempt")]
    NotificationWithdrawalRequiresPreWrite,
    #[error("notification withdrawal requires the workspace administrator")]
    NotificationWithdrawalOperatorInvalid,
    #[error(
        "notification attempt recipient '{found}' does not match requested recipient '{expected}'"
    )]
    NotificationWithdrawalRecipientMismatch {
        expected: RecipientKey,
        found: RecipientKey,
    },
    #[error("notification attempt '{0}' names no current attempt in this workspace")]
    NotificationAttemptUnknown(NotificationAttemptId),
    #[error("notification claim locator '{0}' conflicts with a stored message id")]
    NotificationAttemptClaimLocatorConflict(MessageId),
    #[error("notification attempt '{0}' was already resolved")]
    NotificationAlreadyResolved(NotificationAttemptId),
    #[error("notification attempt '{0}' has an unresolved terminal action")]
    NotificationResolutionAmbiguous(NotificationAttemptId),
    #[error("notification attempt '{0}' has no active composer barrier")]
    NotificationBarrierNotActive(NotificationAttemptId),
    #[error("occupant replacement retirement requires one full replacement binding")]
    NotificationBarrierReplacementRequired,
    #[error("replacement binding is only allowed for occupant replacement retirement")]
    NotificationBarrierReplacementForbidden,
    #[error("occupant replacement retirement must name a different agent generation or manifest")]
    NotificationBarrierReplacementUnchanged,
    #[error("barrier retirement cause {cause:?} is invalid for notification state {state:?}")]
    NotificationBarrierRetirementState {
        cause: NotificationBarrierRetirementCause,
        state: NotificationState,
    },
    #[error("notification attempt '{0}' is already being resolved")]
    NotificationResolutionInProgress(NotificationAttemptId),
    #[error("notification attempt '{0}' is not eligible for exact-owned recovery")]
    NotificationAutomaticResolutionNotEligible(NotificationAttemptId),
    #[error("message '{0}' has no unresolved attention attempt")]
    NoUnresolvedAttention(MessageId),
    #[error("message '{message_id}' has multiple unresolved attention attempts")]
    AmbiguousAttentionTarget {
        message_id: MessageId,
        candidates: Vec<NotificationAttemptId>,
    },
    #[error("attention target '{0}' is neither an attempt id nor a message id")]
    InvalidAttentionTarget(String),
    #[error("invalid notification fact: {0}")]
    InvalidNotificationFact(String),
    #[error(transparent)]
    Type(#[from] MailboxTypeError),
}

/// In-memory projection of mailboxes derived from canonical workspace journal lines.
#[derive(Debug, Clone)]
pub struct MailboxProjection {
    /// Workspace boundary this projection belongs to.
    workspace_id: WorkspaceId,
    /// Last applied monotonic sequence number across the single workspace journal.
    last_workspace_seq: Option<u64>,
    /// Canonical messages indexed by unique ID.
    messages: HashMap<MessageId, LedgerLine>,
    /// Idempotency index: (sender, client_key) -> (message_id, request_digest).
    idempotency_index: HashMap<(RecipientKey, String), (MessageId, RequestDigest)>,
    /// Ordered mailbox entries per recipient (strictly ordered by monotonic workspace sequence).
    mailboxes: HashMap<RecipientKey, BTreeMap<u64, MailboxEntry>>,
    /// Mailbox sequence for each durable recipient and message.
    ///
    /// Entries remain owned by `mailboxes`; this index avoids scanning a
    /// recipient's full FIFO for point reads and state transitions.
    mailbox_index: HashMap<(RecipientKey, MessageId), u64>,
    /// Current notification attempt per durable recipient and message.
    notifications: HashMap<(RecipientKey, MessageId), NotificationRecord>,
    /// Post-write attempts whose composer barrier has not been retired.
    ///
    /// This is a projection of journal facts, not a second durable store.
    /// Requeue can replace the current attempt while an older attempt still
    /// owns staged composer state, so the key is the exact attempt id.
    active_notification_barriers: HashMap<NotificationAttemptId, NotificationRecord>,
    /// ACK-timeout claim reconciliations applied from their dedicated fact.
    claimed_ack_timeout_reconciliations: HashSet<NotificationAttemptId>,
    /// Every attempt identifier seen in this workspace, including superseded attempts.
    notification_attempts: HashSet<NotificationAttemptId>,
    /// Attempts an operator has acknowledged. Kept beside the records
    /// rather than inside them so a clearance never rewrites the attempt
    /// it acknowledges.
    cleared_attempts: HashSet<NotificationAttemptId>,
    /// Atomic clearance command identifiers already applied.
    clearance_batches: HashSet<String>,
    /// Operator resolutions keyed by exact attempt identity.
    resolved_attempts: HashMap<NotificationAttemptId, NotificationResolution>,
    /// Terminal action intents recorded before a terminal write.
    resolution_intents: HashMap<NotificationAttemptId, NotificationResolution>,
    /// Forced intents selected by the default-off submit fallback.
    ///
    /// This is retained only while the matching intent is open so a replayed
    /// final key reservation can prove it belongs to that narrowly-scoped
    /// fallback rather than to an ordinary operator action.
    forced_resolution_intents: HashSet<NotificationAttemptId>,
    /// Final forced terminal-key reservations ordered with mailbox claims.
    resolution_action_reservations: HashMap<NotificationAttemptId, NotificationResolution>,
    /// Workspace sequence of each forced terminal-key reservation.
    ///
    /// A recipient claim after this sequence can provide Complete consumption
    /// evidence once terminal acceptance is also durable, even if the claim
    /// arrived in the unavoidable interval before the actual terminal IO.
    resolution_action_reservation_sequences: HashMap<NotificationAttemptId, u64>,
    /// Terminal action keys accepted by the terminal for an exact intent.
    resolution_actions_accepted: HashMap<NotificationAttemptId, NotificationResolution>,
    /// Workspace sequence of each terminal action-accepted boundary.
    resolution_action_sequences: HashMap<NotificationAttemptId, u64>,
    /// Workspace sequence of each exact recipient claim.
    claim_sequences: HashMap<(RecipientKey, MessageId), u64>,
    /// Workspace sequence where each attempt crossed its terminal write boundary.
    notification_write_sequences: HashMap<NotificationAttemptId, u64>,
    /// Complete actions with exact, causally correlated consumption evidence.
    resolution_consumptions:
        HashMap<NotificationAttemptId, NotificationResolutionConsumptionObservation>,
}

enum PreparedMutation {
    Message {
        message_id: MessageId,
        metadata: MessageMetadata,
        superseded_notification: Option<Box<NotificationProjectionUpdate>>,
    },
    Claim {
        message_id: MessageId,
        recipient: RecipientKey,
        claimant: RecipientKey,
        notification_update: Option<Box<NotificationProjectionUpdate>>,
    },
    DeliveredDirect {
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    },
    Notification {
        key: (RecipientKey, MessageId),
        record: NotificationRecord,
        new_attempt: bool,
    },
    NotificationRequeues {
        records: Vec<((RecipientKey, MessageId), NotificationRecord)>,
    },
    NotificationCleared {
        attempt_id: NotificationAttemptId,
    },
    NotificationsCleared {
        batch_id: String,
        attempt_ids: Vec<NotificationAttemptId>,
    },
    NotificationWithdrawnBeforeWrite {
        key: (RecipientKey, MessageId),
        record: NotificationRecord,
    },
    NotificationResolutionIntent {
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
        forced: bool,
    },
    NotificationResolutionActionReserved {
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    },
    NotificationResolutionActionAccepted {
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    },
    NotificationResolutionConsumptionObserved {
        attempt_id: NotificationAttemptId,
        observation: NotificationResolutionConsumptionObservation,
    },
    NotificationResolutionIntentWithdrawn {
        attempt_id: NotificationAttemptId,
    },
    NotificationResolvedWithoutTerminalAction {
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    },
    NotificationResolved {
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    },
    NotificationClaimedStagedCleared {
        key: (RecipientKey, MessageId),
        record: NotificationRecord,
        attempt_id: NotificationAttemptId,
    },
    NotificationClaimedAckTimeoutReconciled {
        key: (RecipientKey, MessageId),
        record: NotificationRecord,
        attempt_id: NotificationAttemptId,
    },
    NotificationBarrierRetired {
        attempt_id: NotificationAttemptId,
    },
}

struct NotificationProjectionUpdate {
    key: (RecipientKey, MessageId),
    record: NotificationRecord,
}

fn uses_legacy_notification_write_contract(record: &NotificationRecord) -> bool {
    let legacy_transport = match record.transport {
        NotificationTransport::Doorbell => matches!(
            record.doorbell_format,
            None | Some(DOORBELL_FORMAT_COMPACT_CLAIM)
        ),
        NotificationTransport::DirectPayload => record.doorbell_format.is_none(),
    };
    legacy_transport
        && record
            .binding
            .as_ref()
            .is_some_and(|binding| binding.pane_root.is_none())
}

pub(crate) fn uses_incomplete_legacy_doorbell_contract(record: &NotificationRecord) -> bool {
    record.transport == NotificationTransport::Doorbell
        && matches!(
            record.doorbell_format,
            None | Some(DOORBELL_FORMAT_COMPACT_CLAIM)
        )
        && record
            .binding
            .as_ref()
            .is_some_and(|binding| binding.pane_root.is_none())
}

/// Admit the one cause-gated correction whose transport proves zero command
/// bytes, plus the historical direct Staged to Submitted replay edge.
fn notification_transition_allowed(
    record: &NotificationRecord,
    next: NotificationState,
    pre_write_cause: Option<NotificationPreWriteCause>,
    replaying: bool,
) -> bool {
    record.state.can_transition_to(next)
        || (record.state == NotificationState::Writing
            && next == NotificationState::BlockedPreWrite
            && pre_write_cause == Some(NotificationPreWriteCause::PasteCommandUnwritten))
        || (replaying
            && record.state == NotificationState::Staged
            && next == NotificationState::Submitted
            && uses_legacy_notification_write_contract(record))
}

fn notification_pre_write_width_block(record: &NotificationRecord) -> Option<(u32, u32)> {
    if record.state != NotificationState::BlockedPreWrite
        || record.pre_write_cause != Some(NotificationPreWriteCause::WriteReadinessChanged)
    {
        return None;
    }
    record
        .pre_write_observation
        .as_ref()?
        .pane_width
        .zip(record.pre_write_observation.as_ref()?.required_pane_width)
        .filter(|(observed, required)| observed < required)
}

/// Exact projected scheduler outcome or a pending durable resolution.
fn notification_wake_block(
    record: &NotificationRecord,
    attention_resolution_pending: bool,
) -> Option<MessageWakeBlock> {
    record.wake_block.or_else(|| {
        attention_resolution_pending.then_some(MessageWakeBlock::AttentionResolutionPending)
    })
}

fn route_evidence_is_later(
    prior: &NotificationRouteEvidenceId,
    current: &NotificationRouteEvidenceId,
) -> bool {
    prior.boot_id != current.boot_id || current.generation > prior.generation
}

struct ReplyDerivation {
    recipient: RecipientKey,
    thread_root: MessageId,
    subject: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxIdentity {
    pub key: RecipientKey,
    pub label: String,
}

pub struct MailboxSend {
    pub addresses: Vec<String>,
    pub recipient_keys: Option<Vec<RecipientKey>>,
    pub subject: String,
    pub summary: Option<String>,
    pub body: String,
    pub fyi: bool,
    pub client_key: Option<String>,
    pub supersedes: Option<MessageId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageDisposition {
    pub recipient: RecipientKey,
    pub label: String,
    pub attempt_id: Option<NotificationAttemptId>,
    pub notification_state_raw: Option<NotificationState>,
    pub notification_state: MessageNotificationState,
    pub quota_state: Option<MessageQuotaState>,
    pub notification_settlement: Option<MessageNotificationSettlement>,
    pub pre_write_cause: Option<NotificationPreWriteCause>,
    pub wake_block: Option<MessageWakeBlock>,
    pub position_ahead: Option<u32>,
}

/// Exact durable FIFO head whose existing state prevents a fresh schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotificationScheduleBlock {
    pub message_id: MessageId,
    pub attempt_id: NotificationAttemptId,
    pub block: MessageWakeBlock,
}

/// Immutable authorization inputs for one unresolved attention attempt.
#[derive(Debug, Clone)]
pub(crate) struct AttentionTarget {
    pub(crate) record: NotificationRecord,
}

/// How an exact resolution action may proceed after reserving its attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionResolutionStart {
    /// No durable terminal action exists for this attempt.
    Fresh,
    /// A pre-key intent exists, but terminal acceptance was never recorded.
    IntentOnlyUncertain,
    /// The terminal accepted Complete, but consumption was not observed.
    AcceptedUnconsumed,
    /// The matching terminal action may have landed and may only be reconciled.
    ReconcileOnly,
}

pub struct MailboxDirectory {
    workspace_id: WorkspaceId,
    by_address: HashMap<String, MailboxIdentity>,
    by_pane: HashMap<TmuxPaneId, MailboxIdentity>,
    by_recipient: HashMap<RecipientKey, MailboxIdentity>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MailboxDirectoryError {
    #[error("mailbox identity belongs to another workspace")]
    ForeignWorkspace,
    #[error("admin is not an agent directory entry")]
    AdminEntry,
    #[error("mailbox label must be non-empty and contain no control characters")]
    InvalidLabel,
    #[error("mailbox address '{0}' identifies more than one recipient")]
    DuplicateAddress(String),
    #[error("recipient '{0}' is not in the durable mailbox directory")]
    UnknownRecipient(String),
    #[error("recipient labels and durable recipient keys cannot be combined")]
    MixedRecipientSelectors,
    #[error("a reply derives its recipient from reply_to and cannot name recipients")]
    ReplyRecipientSelectors,
    #[error("'*' must be the only recipient address")]
    MixedBroadcast,
}

impl MailboxDirectory {
    pub fn new(
        workspace_id: WorkspaceId,
        agents: impl IntoIterator<Item = MailboxIdentity>,
    ) -> Result<Self, MailboxDirectoryError> {
        let mut directory = Self {
            workspace_id,
            by_address: HashMap::new(),
            by_pane: HashMap::new(),
            by_recipient: HashMap::new(),
        };
        let mut pane_candidates: HashMap<TmuxPaneId, Vec<MailboxIdentity>> = HashMap::new();
        for identity in agents {
            if identity.key.workspace_id() != workspace_id {
                return Err(MailboxDirectoryError::ForeignWorkspace);
            }
            let pane = identity
                .key
                .pane_id()
                .ok_or(MailboxDirectoryError::AdminEntry)?;
            if identity.label.is_empty() || identity.label.chars().any(char::is_control) {
                return Err(MailboxDirectoryError::InvalidLabel);
            }
            if identity.label != pane.to_string() {
                let address = identity.label.clone();
                if directory
                    .by_address
                    .insert(address.clone(), identity.clone())
                    .is_some()
                {
                    return Err(MailboxDirectoryError::DuplicateAddress(address));
                }
            }
            if directory
                .by_recipient
                .insert(identity.key, identity.clone())
                .is_some()
            {
                return Err(MailboxDirectoryError::DuplicateAddress(pane.to_string()));
            }
            pane_candidates.entry(pane).or_default().push(identity);
        }
        for (pane, candidates) in pane_candidates {
            let [identity] = candidates.as_slice() else {
                continue;
            };
            let address = pane.to_string();
            if directory
                .by_address
                .insert(address.clone(), identity.clone())
                .is_some()
            {
                return Err(MailboxDirectoryError::DuplicateAddress(address));
            }
            directory.by_pane.insert(pane, identity.clone());
        }
        Ok(directory)
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn admin(&self) -> MailboxIdentity {
        MailboxIdentity {
            key: RecipientKey::admin(self.workspace_id),
            label: "admin".to_string(),
        }
    }

    pub fn agent_for_pane(&self, pane: TmuxPaneId) -> Option<MailboxIdentity> {
        self.by_pane.get(&pane).cloned()
    }

    pub fn identity_for_recipient(&self, recipient: RecipientKey) -> Option<MailboxIdentity> {
        if recipient == self.admin().key {
            return Some(self.admin());
        }
        self.by_recipient.get(&recipient).cloned()
    }

    /// Current human-facing routes keyed by their durable mailbox identity.
    pub fn routes(&self) -> Vec<MailboxIdentity> {
        let mut routes = Vec::with_capacity(self.by_recipient.len() + 1);
        routes.push(self.admin());
        routes.extend(self.by_recipient.values().cloned());
        routes.sort_by_key(|identity| identity.key);
        routes
    }

    fn current_routes(&self) -> HashMap<RecipientKey, MessageRecipientRoute> {
        self.by_recipient
            .iter()
            .filter_map(|(recipient, identity)| {
                Some((
                    *recipient,
                    MessageRecipientRoute {
                        label: identity.label.clone(),
                        pane_id: recipient.pane_id()?,
                    },
                ))
            })
            .collect()
    }

    pub fn resolve(
        &self,
        addresses: &[String],
    ) -> Result<Vec<MailboxIdentity>, MailboxDirectoryError> {
        if addresses == ["*".to_string()] {
            let mut identities: Vec<_> = self.by_recipient.values().cloned().collect();
            identities.sort_by_key(|identity| identity.key);
            return Ok(identities);
        }
        if addresses.iter().any(|address| address == "*") {
            return Err(MailboxDirectoryError::MixedBroadcast);
        }
        let mut seen = HashSet::new();
        let mut identities = Vec::with_capacity(addresses.len());
        for address in addresses {
            let identity = if address == "admin" {
                self.admin()
            } else {
                self.by_address
                    .get(address)
                    .cloned()
                    .ok_or_else(|| MailboxDirectoryError::UnknownRecipient(address.clone()))?
            };
            if seen.insert(identity.key) {
                identities.push(identity);
            }
        }
        Ok(identities)
    }

    fn resolve_recipient_keys(
        &self,
        recipient_keys: &[RecipientKey],
    ) -> Result<Vec<MailboxIdentity>, MailboxDirectoryError> {
        let mut seen = HashSet::new();
        let mut identities = Vec::with_capacity(recipient_keys.len());
        for recipient in recipient_keys {
            if recipient.workspace_id() != self.workspace_id {
                return Err(MailboxDirectoryError::ForeignWorkspace);
            }
            let identity = self
                .identity_for_recipient(*recipient)
                .ok_or_else(|| MailboxDirectoryError::UnknownRecipient(recipient.to_string()))?;
            if seen.insert(identity.key) {
                identities.push(identity);
            }
        }
        Ok(identities)
    }
}

impl MailboxProjection {
    /// Initialize an empty projection bound to a specific workspace domain.
    pub fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            last_workspace_seq: None,
            messages: HashMap::new(),
            idempotency_index: HashMap::new(),
            mailboxes: HashMap::new(),
            mailbox_index: HashMap::new(),
            notifications: HashMap::new(),
            active_notification_barriers: HashMap::new(),
            claimed_ack_timeout_reconciliations: HashSet::new(),
            cleared_attempts: HashSet::new(),
            clearance_batches: HashSet::new(),
            resolved_attempts: HashMap::new(),
            resolution_intents: HashMap::new(),
            forced_resolution_intents: HashSet::new(),
            resolution_action_reservations: HashMap::new(),
            resolution_action_reservation_sequences: HashMap::new(),
            resolution_actions_accepted: HashMap::new(),
            resolution_action_sequences: HashMap::new(),
            claim_sequences: HashMap::new(),
            notification_write_sequences: HashMap::new(),
            resolution_consumptions: HashMap::new(),
            notification_attempts: HashSet::new(),
        }
    }

    /// Bound workspace identifier.
    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Last observed sequence number in the workspace journal.
    pub fn last_sequence(&self) -> Option<u64> {
        self.last_workspace_seq
    }

    /// Pre-append idempotency check: determines if request is a new submission or an identical retry.
    fn check_acceptance(&self, draft: &CanonicalDraft) -> Result<AcceptanceOutcome, MailboxError> {
        if draft.kind != Kind::Msg && draft.kind != Kind::Fyi {
            return Err(MailboxError::InvalidKind(draft.kind));
        }

        if draft.sender.workspace_id() != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: draft.sender.workspace_id(),
            });
        }

        if draft.recipients.is_empty() {
            return Err(MailboxError::DraftEmptyRecipients);
        }

        let mut seen = HashSet::new();
        for recipient in &draft.recipients {
            if recipient.workspace_id() != self.workspace_id {
                return Err(MailboxError::WorkspaceMismatch {
                    expected: self.workspace_id,
                    found: recipient.workspace_id(),
                });
            }
            if !seen.insert(*recipient) {
                return Err(MailboxError::DraftDuplicateRecipient(*recipient));
            }
        }

        if let Some(ref key) = draft.client_key {
            if key.is_empty() {
                return Err(MailboxError::EmptyClientKey);
            }
        }
        presentation_labels(&draft.recipients, &draft.presentation)?;
        if let Some(summary) = draft.summary.as_deref() {
            cyclops_proto::validate_message_summary(summary)?;
        }

        let digest = RequestDigest::compute(
            draft.kind,
            draft.sender,
            &draft.recipients,
            RequestContent {
                subject: draft.subject.as_deref(),
                summary: draft.summary.as_deref(),
                body: draft.body.as_deref(),
            },
            draft.reply_to.as_ref(),
            draft.supersedes.as_ref(),
        )?;

        if let Some(ref key) = draft.client_key {
            let map_key = (draft.sender, key.clone());
            if let Some((existing_id, existing_digest)) = self.idempotency_index.get(&map_key) {
                if existing_digest == &digest {
                    return Ok(AcceptanceOutcome::Existing(existing_id.clone()));
                }
                return Err(MailboxError::DuplicateIdempotencyKey {
                    sender: draft.sender,
                    key: key.clone(),
                    existing_id: existing_id.clone(),
                });
            }
        }

        self.supersession_thread_root(draft.sender, &draft.recipients, draft.supersedes.as_ref())?;

        Ok(AcceptanceOutcome::New {
            request_digest: digest,
        })
    }

    /// Validate and apply one canonical journal row without partial mutation.
    pub fn apply_line(&mut self, line: &LedgerLine) -> Result<(), MailboxError> {
        self.apply_owned(line.clone())
    }

    fn apply_owned(&mut self, line: LedgerLine) -> Result<(), MailboxError> {
        let prepared = self.prepare_line(&line)?;
        self.commit_line(line, prepared);
        Ok(())
    }

    fn apply_replayed_owned(&mut self, line: LedgerLine) -> Result<(), MailboxError> {
        let prepared = self.prepare_line_inner(&line, true)?;
        self.commit_line(line, prepared);
        Ok(())
    }

    fn prepare_line(&self, line: &LedgerLine) -> Result<PreparedMutation, MailboxError> {
        self.prepare_line_inner(line, false)
    }

    fn prepare_line_inner(
        &self,
        line: &LedgerLine,
        replaying: bool,
    ) -> Result<PreparedMutation, MailboxError> {
        let previous = self.last_workspace_seq.unwrap_or(0);
        let expected = previous
            .checked_add(1)
            .ok_or(MailboxError::NonContiguousSequence {
                expected: previous,
                found: line.seq,
            })?;
        if line.seq != expected {
            return Err(MailboxError::NonContiguousSequence {
                expected,
                found: line.seq,
            });
        }

        match line.kind {
            Kind::Msg | Kind::Fyi => self.prepare_message(line),
            Kind::State => self.prepare_state(line, replaying),
            _ => Err(MailboxError::UncanonicalRow(format!(
                "unsupported workspace row kind {:?} seq {}",
                line.kind, line.seq
            ))),
        }
    }

    fn prepare_state(
        &self,
        line: &LedgerLine,
        replaying: bool,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact_type = line
            .data
            .as_ref()
            .and_then(|data| data.get("type"))
            .and_then(serde_json::Value::as_str);
        match fact_type {
            Some("message_claimed") => self.prepare_claim(line),
            Some("message_delivered_direct") => self.prepare_delivered_direct(line),
            Some("notification_transition") => {
                self.prepare_notification_transition(line, replaying)
            }
            Some("notification_unclaimed_reminder_queued") => {
                self.prepare_unclaimed_reminder_queued(line)
            }
            Some("notification_requeued") => self.prepare_notification_requeue(line),
            Some("notifications_requeued") => self.prepare_notification_requeues(line),
            Some("notification_cleared") => self.prepare_notification_clear(line),
            Some("notifications_cleared") => self.prepare_notification_clears(line),
            Some("notification_withdrawn_before_write") => {
                self.prepare_notification_withdrawal(line)
            }
            Some("notification_resolution_intent") => {
                self.prepare_notification_resolution_intent(line)
            }
            Some("notification_resolution_action_reserved") => {
                self.prepare_notification_resolution_action_reserved(line)
            }
            Some("notification_resolution_action_accepted") => {
                self.prepare_notification_resolution_action_accepted(line)
            }
            Some("notification_resolution_consumption_observed") => {
                self.prepare_notification_resolution_consumption_observed(line)
            }
            Some("notification_resolution_intent_withdrawn") => {
                self.prepare_notification_resolution_intent_withdrawn(line)
            }
            Some("notification_resolved_without_terminal_action") => {
                self.prepare_notification_resolution_without_terminal_action(line)
            }
            Some("notification_resolved") => self.prepare_notification_resolution(line),
            Some("notification_claimed_staged_cleared") => {
                self.prepare_notification_claimed_staged_clear(line)
            }
            Some("notification_claimed_ack_timeout_reconciled") => {
                self.prepare_notification_claimed_ack_timeout_reconciliation(line)
            }
            Some("notification_barrier_retired") => {
                self.prepare_notification_barrier_retirement(line)
            }
            _ => Err(MailboxError::UncanonicalRow(format!(
                "unknown or uncanonical state row seq {}",
                line.seq
            ))),
        }
    }

    fn prepare_message(&self, line: &LedgerLine) -> Result<PreparedMutation, MailboxError> {
        let message_id = MessageId::new(&line.id)?;
        if self.messages.contains_key(&message_id) {
            return Err(MailboxError::DuplicateMessageId(message_id));
        }

        let metadata = extract_message_metadata(line)?;
        if metadata.record_version != CANONICAL_RECORD_VERSION {
            return Err(MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: metadata.record_version,
            });
        }
        if metadata.workspace_id != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: metadata.workspace_id,
            });
        }
        if metadata.sender.workspace_id() != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: metadata.sender.workspace_id(),
            });
        }
        if metadata.recipients.is_empty() {
            return Err(MailboxError::EmptyRecipients {
                message_id: message_id.clone(),
            });
        }

        let (sender_label, recipient_labels) =
            presentation_labels(&metadata.recipients, &metadata.presentation)?;
        if line.from != sender_label {
            return Err(MailboxError::PresentationMismatch {
                field: "from",
                presentation: line.from.clone(),
                authoritative: sender_label,
            });
        }
        if line.to != recipient_labels {
            return Err(MailboxError::PresentationMismatch {
                field: "to",
                presentation: format!("{:?}", line.to),
                authoritative: format!("{:?}", recipient_labels),
            });
        }

        let mut seen_recipients = HashSet::new();
        for recipient in &metadata.recipients {
            if recipient.workspace_id() != self.workspace_id {
                return Err(MailboxError::WorkspaceMismatch {
                    expected: self.workspace_id,
                    found: recipient.workspace_id(),
                });
            }
            if !seen_recipients.insert(*recipient) {
                return Err(MailboxError::DuplicateRecipient {
                    message_id: message_id.clone(),
                    recipient: *recipient,
                });
            }
        }

        let reply_to = line.reply_to.as_deref().map(MessageId::new).transpose()?;
        if reply_to.is_some() && metadata.supersedes.is_some() {
            return Err(MailboxError::ReplySupersessionConflict);
        }
        let expected_thread_root = if let Some(reference) = reply_to.as_ref() {
            if line.kind != Kind::Msg {
                return Err(MailboxError::ReplyKindMismatch {
                    message_id: message_id.clone(),
                });
            }
            let derived = self.derive_reply(metadata.sender, reference)?;
            if metadata.recipients.as_slice() != [derived.recipient] {
                return Err(MailboxError::ReplyRecipientMismatch {
                    message_id: message_id.clone(),
                    expected: derived.recipient,
                });
            }
            if line.subject != derived.subject {
                return Err(MailboxError::ReplySubjectMismatch {
                    message_id: message_id.clone(),
                });
            }
            derived.thread_root
        } else if let Some(thread_root) = self.supersession_thread_root(
            metadata.sender,
            &metadata.recipients,
            metadata.supersedes.as_ref(),
        )? {
            thread_root
        } else {
            message_id.clone()
        };
        if metadata.thread_root != expected_thread_root {
            return Err(MailboxError::ThreadRootMismatch {
                message_id: message_id.clone(),
                expected: expected_thread_root,
                found: metadata.thread_root,
            });
        }

        let computed = RequestDigest::compute(
            line.kind,
            metadata.sender,
            &metadata.recipients,
            RequestContent {
                subject: line.subject.as_deref(),
                summary: metadata.summary.as_deref(),
                body: line.body.as_deref(),
            },
            reply_to.as_ref(),
            metadata.supersedes.as_ref(),
        )?;
        if metadata.request_digest != computed {
            return Err(MailboxError::DigestMismatch {
                stored: metadata.request_digest,
                computed,
            });
        }

        if let Some(ref client_key) = metadata.client_key {
            if let Some((existing_id, _)) = self
                .idempotency_index
                .get(&(metadata.sender, client_key.clone()))
            {
                return Err(MailboxError::DuplicateIdempotencyKey {
                    sender: metadata.sender,
                    key: client_key.clone(),
                    existing_id: existing_id.clone(),
                });
            }
        }

        let superseded_notification = metadata.supersedes.as_ref().and_then(|superseded| {
            let recipient = metadata.recipients[0];
            let key = (recipient, superseded.clone());
            self.notifications.get(&key).map(|current| {
                debug_assert!(matches!(
                    current.state,
                    NotificationState::Queued
                        | NotificationState::Gating
                        | NotificationState::BlockedPreWrite
                        | NotificationState::QuotaHeld
                        | NotificationState::QuotaResetObserved
                        | NotificationState::WithdrawnByOperator
                        | NotificationState::AttentionRequired
                ));
                Box::new(NotificationProjectionUpdate {
                    key,
                    record: NotificationRecord {
                        attempt_id: current.attempt_id,
                        message_id: current.message_id.clone(),
                        recipient: current.recipient,
                        state: NotificationState::Superseded,
                        binding: None,
                        transport: current.transport,
                        doorbell_format: current.doorbell_format,
                        cause: None,
                        verify_outcome: None,
                        pre_write_cause: None,
                        wake_block: None,
                        pre_write_observation: None,
                        pre_write_reopen_count: current.pre_write_reopen_count,
                        unclaimed_reminder_count: current.unclaimed_reminder_count,
                        started_seq: current.started_seq,
                        updated_seq: line.seq,
                        updated_at: line.ts,
                    },
                })
            })
        });

        Ok(PreparedMutation::Message {
            message_id,
            metadata,
            superseded_notification,
        })
    }

    fn prepare_claim(&self, line: &LedgerLine) -> Result<PreparedMutation, MailboxError> {
        let data = line.data.as_ref().ok_or_else(|| {
            MailboxError::UncanonicalRow(format!(
                "unknown or uncanonical state row seq {}",
                line.seq
            ))
        })?;
        let fact: MailboxFact = serde_json::from_value(data.clone()).map_err(|error| {
            MailboxError::InvalidFact(format!("malformed message_claimed: {error}"))
        })?;
        let MailboxFact::MessageClaimed {
            record_version,
            message_id,
            recipient,
            claimant,
        } = fact
        else {
            return Err(MailboxError::InvalidFact("expected message_claimed".into()));
        };

        if record_version != CANONICAL_RECORD_VERSION {
            return Err(MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: record_version,
            });
        }
        if line.id != message_id.as_str() {
            return Err(MailboxError::EnvelopeMismatch {
                envelope_id: line.id.clone(),
                fact_id: message_id,
            });
        }
        let claimant_text = claimant.to_string();
        if line.from != claimant_text {
            return Err(MailboxError::PresentationMismatch {
                field: "from",
                presentation: line.from.clone(),
                authoritative: claimant_text,
            });
        }
        if !line.to.is_empty() {
            return Err(MailboxError::PresentationMismatch {
                field: "to",
                presentation: format!("{:?}", line.to),
                authoritative: "[]".into(),
            });
        }
        if line.subject.is_some()
            || line.body.is_some()
            || line.reply_to.is_some()
            || !line.deliveries.is_empty()
        {
            return Err(MailboxError::UncanonicalRow(format!(
                "claim row seq {} contains non-empty message fields",
                line.seq
            )));
        }
        if recipient.workspace_id() != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: recipient.workspace_id(),
            });
        }
        if claimant.workspace_id() != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: claimant.workspace_id(),
            });
        }
        if claimant != recipient {
            return Err(MailboxError::ClaimantMismatch {
                message_id,
                recipient,
                claimant,
            });
        }

        let entry =
            self.get_entry(recipient, &message_id)
                .ok_or_else(|| MailboxError::EntryNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        match &entry.state {
            MailboxEntryState::Pending => {}
            MailboxEntryState::Claimed {
                claimant: existing, ..
            } => {
                return Err(MailboxError::AlreadyClaimed {
                    message_id,
                    recipient,
                    existing_claimant: *existing,
                });
            }
            MailboxEntryState::Superseded { .. } => {
                return Err(MailboxError::MessageNotPending(message_id));
            }
            MailboxEntryState::DeliveredDirect { .. } => {
                return Err(MailboxError::MessageNotPending(message_id));
            }
        }

        let notification_update = self
            .notifications
            .get(&(recipient, message_id.clone()))
            .and_then(|current| {
                let state = current.state.settled_by_claim(current.transport);
                if state == current.state {
                    return None;
                }
                Some(Box::new(NotificationProjectionUpdate {
                    key: (recipient, message_id.clone()),
                    record: NotificationRecord {
                        attempt_id: current.attempt_id,
                        message_id: current.message_id.clone(),
                        recipient: current.recipient,
                        state,
                        binding: if state == NotificationState::Notified {
                            current.binding.clone()
                        } else {
                            None
                        },
                        transport: current.transport,
                        doorbell_format: current.doorbell_format,
                        cause: None,
                        verify_outcome: None,
                        pre_write_cause: None,
                        wake_block: None,
                        pre_write_observation: None,
                        pre_write_reopen_count: current.pre_write_reopen_count,
                        unclaimed_reminder_count: current.unclaimed_reminder_count,
                        started_seq: current.started_seq,
                        updated_seq: line.seq,
                        updated_at: line.ts,
                    },
                }))
            });

        Ok(PreparedMutation::Claim {
            message_id,
            recipient,
            claimant,
            notification_update,
        })
    }

    fn prepare_delivered_direct(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let data = line.data.as_ref().ok_or_else(|| {
            MailboxError::UncanonicalRow(format!(
                "unknown or uncanonical state row seq {}",
                line.seq
            ))
        })?;
        let fact: MailboxFact = serde_json::from_value(data.clone()).map_err(|error| {
            MailboxError::InvalidFact(format!("malformed message_delivered_direct: {error}"))
        })?;
        let MailboxFact::MessageDeliveredDirect {
            record_version,
            message_id,
            recipient,
            attempt_id,
        } = fact
        else {
            return Err(MailboxError::InvalidFact(
                "expected message_delivered_direct".into(),
            ));
        };
        if record_version != CANONICAL_RECORD_VERSION {
            return Err(MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: record_version,
            });
        }
        if line.id != message_id.as_str() {
            return Err(MailboxError::EnvelopeMismatch {
                envelope_id: line.id.clone(),
                fact_id: message_id,
            });
        }
        if line.from != "cyclopsd" {
            return Err(MailboxError::PresentationMismatch {
                field: "from",
                presentation: line.from.clone(),
                authoritative: "cyclopsd".into(),
            });
        }
        if !line.to.is_empty()
            || line.subject.is_some()
            || line.body.is_some()
            || line.reply_to.is_some()
            || !line.deliveries.is_empty()
        {
            return Err(MailboxError::UncanonicalRow(format!(
                "direct delivery row seq {} contains message fields",
                line.seq
            )));
        }
        self.require_pending_entry(recipient, &message_id)?;
        let notification = self.notification(recipient, &message_id).ok_or_else(|| {
            MailboxError::NotificationNotFound {
                message_id: message_id.clone(),
                recipient,
            }
        })?;
        if notification.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: notification.attempt_id,
                found: attempt_id,
            });
        }
        if notification.state != NotificationState::Notified
            || notification.transport != NotificationTransport::DirectPayload
        {
            return Err(MailboxError::DirectDeliveryNotProven { attempt_id });
        }
        Ok(PreparedMutation::DeliveredDirect {
            message_id,
            recipient,
            attempt_id,
        })
    }

    fn prepare_notification_transition(
        &self,
        line: &LedgerLine,
        replaying: bool,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationTransition {
            record_version,
            attempt_id,
            message_id,
            recipient,
            state,
            binding,
            transport,
            doorbell_format,
            cause,
            verify_outcome,
            pre_write_cause,
            wake_block,
            pre_write_observation,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_transition".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let key = (recipient, message_id.clone());
        let current = self.notifications.get(&key);

        match current {
            None if state != NotificationState::Queued => {
                return Err(MailboxError::NotificationNotFound {
                    message_id,
                    recipient,
                });
            }
            Some(current) if current.attempt_id != attempt_id => {
                return Err(MailboxError::NotificationAttemptMismatch {
                    expected: current.attempt_id,
                    found: attempt_id,
                });
            }
            Some(current)
                if current.state == NotificationState::BlockedPreWrite
                    && state == NotificationState::Gating
                    && current.pre_write_reopen_count >= 1 =>
            {
                return Err(MailboxError::NotificationPreWriteReopenExhausted);
            }
            Some(current)
                if pre_write_cause == Some(NotificationPreWriteCause::PasteCommandUnwritten)
                    && !(current.state == NotificationState::Writing
                        && state == NotificationState::BlockedPreWrite) =>
            {
                return Err(MailboxError::InvalidNotificationTransition {
                    from: current.state,
                    to: state,
                });
            }
            Some(current)
                if !notification_transition_allowed(current, state, pre_write_cause, replaying) =>
            {
                return Err(MailboxError::InvalidNotificationTransition {
                    from: current.state,
                    to: state,
                });
            }
            _ => {}
        }

        if state == NotificationState::Writing {
            let Some(binding) = binding.as_ref() else {
                return Err(MailboxError::NotificationBindingRequired);
            };
            if binding.recipient != recipient {
                return Err(MailboxError::NotificationBindingMismatch);
            }
        } else if binding.is_some() {
            return Err(MailboxError::NotificationBindingForbidden);
        }
        if state != NotificationState::Writing && transport.is_some() {
            return Err(MailboxError::NotificationTransportForbidden);
        }
        if state != NotificationState::Writing && doorbell_format.is_some() {
            return Err(MailboxError::NotificationDoorbellFormatForbidden);
        }
        if doorbell_format.is_some()
            && !matches!(
                transport.unwrap_or_default(),
                NotificationTransport::Doorbell
            )
        {
            return Err(MailboxError::NotificationDoorbellFormatForbidden);
        }

        if state == NotificationState::AttentionRequired {
            let Some(attention_cause) = cause else {
                return Err(MailboxError::NotificationCauseRequired);
            };
            let prior = current.ok_or_else(|| MailboxError::NotificationNotFound {
                message_id: message_id.clone(),
                recipient,
            })?;
            if !attention_cause.valid_after(prior.state) {
                return Err(MailboxError::InvalidNotificationCause {
                    cause: attention_cause,
                    state: prior.state,
                });
            }
            match (attention_cause, verify_outcome) {
                (NotificationAttentionCause::VerifyFailed, None) if !replaying => {
                    return Err(MailboxError::NotificationVerifyOutcomeRequired);
                }
                (NotificationAttentionCause::VerifyFailed, _) | (_, None) => {}
                (_, Some(_)) => {
                    return Err(MailboxError::NotificationVerifyOutcomeForbidden);
                }
            }
        } else if cause.is_some() {
            return Err(MailboxError::NotificationCauseForbidden);
        } else if verify_outcome.is_some() {
            return Err(MailboxError::NotificationVerifyOutcomeForbidden);
        }
        if state == NotificationState::BlockedPreWrite {
            if pre_write_cause.is_none() {
                return Err(MailboxError::NotificationPreWriteCauseRequired);
            }
            let width_observation = pre_write_observation.as_ref().and_then(|observation| {
                observation.pane_width.zip(observation.required_pane_width)
            });
            let has_partial_width_observation =
                pre_write_observation.as_ref().is_some_and(|observation| {
                    observation.pane_width.is_some() != observation.required_pane_width.is_some()
                });
            if (matches!(
                pre_write_cause,
                Some(
                    NotificationPreWriteCause::BindingUnprovable
                        | NotificationPreWriteCause::ComposerSemanticMissing
                )
            ) || width_observation.is_some()
                && pre_write_cause == Some(NotificationPreWriteCause::WriteReadinessChanged))
                && pre_write_observation
                    .as_ref()
                    .and_then(|observation| observation.selected_manifest.as_ref())
                    .is_none()
            {
                return Err(MailboxError::NotificationPreWriteObservationRequired);
            }
            if has_partial_width_observation {
                return Err(MailboxError::NotificationPreWriteObservationRequired);
            }
            if let Some((observed, required)) = width_observation {
                if pre_write_cause != Some(NotificationPreWriteCause::WriteReadinessChanged) {
                    return Err(MailboxError::NotificationPreWriteObservationRequired);
                }
                if observed >= required || required == 0 {
                    return Err(MailboxError::NotificationPaneWidthNotNarrow {
                        observed,
                        required,
                    });
                }
            }
        } else if pre_write_cause.is_some()
            || wake_block.is_some()
            || pre_write_observation.is_some()
        {
            return Err(MailboxError::NotificationPreWriteCauseForbidden);
        }

        let (record, new_attempt) = match current {
            None => {
                if state != NotificationState::Queued {
                    return Err(MailboxError::NotificationNotFound {
                        message_id,
                        recipient,
                    });
                }
                self.require_pending_entry(recipient, &message_id)?;
                if self.notification_attempts.contains(&attempt_id) {
                    return Err(MailboxError::NotificationAttemptReused(attempt_id));
                }
                (
                    NotificationRecord {
                        attempt_id,
                        message_id: message_id.clone(),
                        recipient,
                        state,
                        binding: None,
                        transport: NotificationTransport::Doorbell,
                        doorbell_format: None,
                        cause: None,
                        verify_outcome: None,
                        pre_write_cause: None,
                        wake_block: None,
                        pre_write_observation: None,
                        pre_write_reopen_count: 0,
                        unclaimed_reminder_count: 0,
                        started_seq: line.seq,
                        updated_seq: line.seq,
                        updated_at: line.ts,
                    },
                    true,
                )
            }
            Some(current) => (
                NotificationRecord {
                    attempt_id,
                    message_id: message_id.clone(),
                    recipient,
                    state,
                    binding: if state == NotificationState::Writing {
                        binding
                    } else if state == NotificationState::BlockedPreWrite
                        && pre_write_cause == Some(NotificationPreWriteCause::PasteCommandUnwritten)
                    {
                        None
                    } else {
                        current.binding.clone()
                    },
                    transport: if state == NotificationState::Writing {
                        transport.unwrap_or_default()
                    } else {
                        current.transport
                    },
                    doorbell_format: if state == NotificationState::Writing {
                        doorbell_format
                    } else if state == NotificationState::BlockedPreWrite
                        && pre_write_cause == Some(NotificationPreWriteCause::PasteCommandUnwritten)
                    {
                        None
                    } else {
                        current.doorbell_format
                    },
                    cause,
                    verify_outcome,
                    pre_write_cause,
                    wake_block,
                    pre_write_observation: pre_write_observation.map(|observation| *observation),
                    pre_write_reopen_count: if current.state == NotificationState::BlockedPreWrite
                        && state == NotificationState::Gating
                    {
                        current.pre_write_reopen_count.saturating_add(1)
                    } else {
                        current.pre_write_reopen_count
                    },
                    unclaimed_reminder_count: current.unclaimed_reminder_count,
                    started_seq: current.started_seq,
                    updated_seq: line.seq,
                    updated_at: line.ts,
                },
                false,
            ),
        };

        Ok(PreparedMutation::Notification {
            key,
            record,
            new_attempt,
        })
    }

    fn prepare_unclaimed_reminder_queued(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationUnclaimedReminderQueued {
            record_version,
            attempt_id,
            message_id,
            recipient,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_unclaimed_reminder_queued".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        self.require_pending_entry(recipient, &message_id)?;
        let key = (recipient, message_id.clone());
        let current =
            self.notifications
                .get(&key)
                .ok_or_else(|| MailboxError::NotificationNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::Notified
            || current.transport != NotificationTransport::Doorbell
            || current.unclaimed_reminder_count != 0
            || self.active_notification_barriers.contains_key(&attempt_id)
        {
            return Err(MailboxError::InvalidNotificationFact(
                "unclaimed reminder requires one pending, notified doorbell with a retired prior barrier and unused allowance"
                    .into(),
            ));
        }

        let mut record = current.clone();
        record.state = NotificationState::Gating;
        record.cause = None;
        record.verify_outcome = None;
        record.pre_write_cause = None;
        record.wake_block = None;
        record.pre_write_observation = None;
        record.unclaimed_reminder_count = 1;
        record.updated_seq = line.seq;
        record.updated_at = line.ts;
        Ok(PreparedMutation::Notification {
            key,
            record,
            new_attempt: false,
        })
    }

    fn prepare_notification_requeue(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationRequeued {
            record_version,
            prior_attempt_id,
            attempt_id,
            message_id,
            recipient,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_requeued".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let (key, record) = self.prepare_notification_requeue_record(
            line,
            message_id,
            recipient,
            prior_attempt_id,
            attempt_id,
        )?;

        Ok(PreparedMutation::Notification {
            key,
            record,
            new_attempt: true,
        })
    }

    fn prepare_notification_requeues(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationsRequeued {
            record_version,
            message_id,
            requeues,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notifications_requeued".into(),
            ));
        };

        self.validate_notification_requeues_envelope(line, record_version, &message_id, &requeues)?;

        let mut attempts = HashSet::with_capacity(requeues.len());
        let mut records = Vec::with_capacity(requeues.len());
        for requeue in requeues {
            if !attempts.insert(requeue.attempt_id) {
                return Err(MailboxError::NotificationAttemptReused(requeue.attempt_id));
            }
            records.push(self.prepare_notification_requeue_record(
                line,
                message_id.clone(),
                requeue.recipient,
                requeue.prior_attempt_id,
                requeue.attempt_id,
            )?);
        }

        Ok(PreparedMutation::NotificationRequeues { records })
    }

    fn prepare_notification_requeue_record(
        &self,
        line: &LedgerLine,
        message_id: MessageId,
        recipient: RecipientKey,
        prior_attempt_id: NotificationAttemptId,
        attempt_id: NotificationAttemptId,
    ) -> Result<((RecipientKey, MessageId), NotificationRecord), MailboxError> {
        self.require_pending_entry(recipient, &message_id)?;
        let key = (recipient, message_id.clone());
        let current =
            self.notifications
                .get(&key)
                .ok_or_else(|| MailboxError::NotificationNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        if current.attempt_id != prior_attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: prior_attempt_id,
            });
        }
        if !matches!(
            current.state,
            NotificationState::AttentionRequired | NotificationState::QuotaResetObserved
        ) {
            return Err(MailboxError::NotificationRequeueRequiresAttention);
        }
        if self.resolution_intents.contains_key(&prior_attempt_id) {
            return Err(MailboxError::NotificationResolutionAmbiguous(
                prior_attempt_id,
            ));
        }
        if self.notification_attempts.contains(&attempt_id) {
            return Err(MailboxError::NotificationAttemptReused(attempt_id));
        }

        Ok((
            key,
            NotificationRecord {
                attempt_id,
                message_id,
                recipient,
                state: NotificationState::Queued,
                binding: None,
                transport: NotificationTransport::Doorbell,
                doorbell_format: None,
                cause: None,
                verify_outcome: None,
                pre_write_cause: None,
                wake_block: None,
                pre_write_observation: None,
                pre_write_reopen_count: 0,
                unclaimed_reminder_count: 0,
                started_seq: line.seq,
                updated_seq: line.seq,
                updated_at: line.ts,
            },
        ))
    }

    fn prepare_notification_clear(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationCleared {
            record_version,
            attempt_id,
            message_id,
            recipient,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_cleared".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        // No pending-entry requirement, unlike requeue: acknowledging an
        // alarm does not redeliver anything, so it stays available after
        // the entry leaves the pending state.
        let current = self
            .notifications
            .get(&(recipient, message_id.clone()))
            .ok_or(MailboxError::NotificationNotFound {
                message_id,
                recipient,
            })?;
        // A clearance names one attempt. If the record has moved on, the
        // operator is acknowledging something that is no longer the alarm.
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::AttentionRequired {
            return Err(MailboxError::NotificationClearRequiresAttention);
        }

        // Replaying a clearance already in the journal is not an error.
        Ok(PreparedMutation::NotificationCleared { attempt_id })
    }

    fn prepare_notification_clears(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationsCleared {
            record_version,
            batch_id,
            attempt_ids,
            operator,
            cutoff_ms,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notifications_cleared".into(),
            ));
        };

        if record_version != CANONICAL_RECORD_VERSION {
            return Err(MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: record_version,
            });
        }
        if operator != RecipientKey::admin(self.workspace_id) {
            return Err(MailboxError::InvalidNotificationFact(
                "notification clearance operator is not the workspace administrator".into(),
            ));
        }
        if batch_id.is_empty() || line.id != batch_id {
            return Err(MailboxError::InvalidNotificationFact(
                "notification clearance batch id does not match its ledger row".into(),
            ));
        }
        if self.clearance_batches.contains(&batch_id) {
            return Err(MailboxError::InvalidNotificationFact(format!(
                "duplicate notification clearance batch '{batch_id}'"
            )));
        }
        if line.from != operator.to_string()
            || line.subject.is_some()
            || line.body.is_some()
            || line.reply_to.is_some()
            || !line.deliveries.is_empty()
        {
            return Err(MailboxError::InvalidNotificationFact(
                "notification clearance batch has an invalid content envelope".into(),
            ));
        }
        if attempt_ids.is_empty() {
            return Err(MailboxError::InvalidNotificationFact(
                "notification clearance batch is empty".into(),
            ));
        }
        let canonical: Vec<_> = attempt_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if canonical != attempt_ids {
            return Err(MailboxError::InvalidNotificationFact(
                "notification clearance attempts are not sorted and unique".into(),
            ));
        }

        let mut recipients = BTreeSet::new();
        for attempt_id in &attempt_ids {
            if self.cleared_attempts.contains(attempt_id) {
                return Err(MailboxError::InvalidNotificationFact(format!(
                    "notification attempt '{attempt_id}' was already cleared"
                )));
            }
            let record = self
                .notification_by_attempt(*attempt_id)
                .ok_or(MailboxError::NotificationAttemptUnknown(*attempt_id))?;
            if record.state != NotificationState::AttentionRequired {
                return Err(MailboxError::NotificationClearRequiresAttention);
            }
            if cutoff_ms.is_some_and(|cutoff| record.updated_at > cutoff) {
                return Err(MailboxError::InvalidNotificationFact(format!(
                    "notification attempt '{attempt_id}' is newer than the confirmed cutoff"
                )));
            }
            recipients.insert(record.recipient.to_string());
        }
        if line.to != recipients.into_iter().collect::<Vec<_>>() {
            return Err(MailboxError::InvalidNotificationFact(
                "notification clearance recipients do not match its attempts".into(),
            ));
        }

        Ok(PreparedMutation::NotificationsCleared {
            batch_id,
            attempt_ids,
        })
    }

    fn prepare_notification_withdrawal(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationWithdrawnBeforeWrite {
            record_version,
            attempt_id,
            message_id,
            recipient,
            operator,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_withdrawn_before_write".into(),
            ));
        };
        if record_version != CANONICAL_RECORD_VERSION {
            return Err(MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: record_version,
            });
        }
        if line.id != message_id.as_str() {
            return Err(MailboxError::EnvelopeMismatch {
                envelope_id: line.id.clone(),
                fact_id: message_id,
            });
        }
        let admin = RecipientKey::admin(self.workspace_id);
        if operator != admin {
            return Err(MailboxError::NotificationWithdrawalOperatorInvalid);
        }
        if line.from != operator.to_string() {
            return Err(MailboxError::PresentationMismatch {
                field: "from",
                presentation: line.from.clone(),
                authoritative: operator.to_string(),
            });
        }
        let authoritative_to = vec![recipient.to_string()];
        if line.to != authoritative_to {
            return Err(MailboxError::PresentationMismatch {
                field: "to",
                presentation: line.to.join(","),
                authoritative: authoritative_to.join(","),
            });
        }
        if line.subject.is_some()
            || line.body.is_some()
            || line.reply_to.is_some()
            || !line.deliveries.is_empty()
        {
            return Err(MailboxError::UncanonicalRow(format!(
                "notification withdrawal row seq {} contains message fields",
                line.seq
            )));
        }
        let entry =
            self.get_entry(recipient, &message_id)
                .ok_or_else(|| MailboxError::EntryNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        if !entry.state.is_pending() && !entry.state.is_claimed() {
            return Err(MailboxError::NotificationMessageNotPending {
                message_id: message_id.clone(),
                recipient,
            });
        }
        let key = (recipient, message_id.clone());
        let current =
            self.notifications
                .get(&key)
                .ok_or_else(|| MailboxError::NotificationNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if !current.state.can_withdraw_before_write() {
            return Err(MailboxError::NotificationWithdrawalRequiresPreWrite);
        }
        Ok(PreparedMutation::NotificationWithdrawnBeforeWrite {
            key,
            record: NotificationRecord {
                attempt_id,
                message_id,
                recipient,
                state: NotificationState::WithdrawnByOperator,
                binding: None,
                transport: current.transport,
                doorbell_format: current.doorbell_format,
                cause: None,
                verify_outcome: None,
                pre_write_cause: None,
                wake_block: None,
                pre_write_observation: None,
                pre_write_reopen_count: current.pre_write_reopen_count,
                unclaimed_reminder_count: current.unclaimed_reminder_count,
                started_seq: current.started_seq,
                updated_seq: line.seq,
                updated_at: line.ts,
            },
        })
    }

    fn prepare_notification_resolution(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationResolved {
            record_version,
            proof_version,
            attempt_id,
            message_id,
            recipient,
            resolution,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_resolved".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let current = self
            .notifications
            .get(&(recipient, message_id))
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::AttentionRequired || current.binding.is_none() {
            return Err(MailboxError::NotificationClearRequiresAttention);
        }
        if self.resolved_attempts.contains_key(&attempt_id) {
            return Err(MailboxError::NotificationAlreadyResolved(attempt_id));
        }
        match proof_version {
            0 => {
                // Legacy final facts predate separate action and consumption
                // boundaries. Limit this compatibility path to the incomplete
                // bindings and doorbell formats written by shipped daemons.
                if !uses_legacy_notification_write_contract(current)
                    || self.resolution_actions_accepted.contains_key(&attempt_id)
                    || self.resolution_consumptions.contains_key(&attempt_id)
                    || self
                        .resolution_intents
                        .get(&attempt_id)
                        .is_some_and(|recorded| *recorded != resolution)
                {
                    return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
                }
            }
            NOTIFICATION_RESOLUTION_PROOF_VERSION => {
                if self.resolution_intents.get(&attempt_id) != Some(&resolution) {
                    return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
                }
                match (
                    resolution,
                    self.resolution_actions_accepted.get(&attempt_id).copied(),
                    self.resolution_consumptions.get(&attempt_id),
                ) {
                    (
                        NotificationResolution::Complete,
                        Some(NotificationResolution::Complete),
                        Some(observation),
                    ) if observation.evidence.proves_exact_consumption() => {}
                    (
                        NotificationResolution::Discard,
                        Some(NotificationResolution::Discard),
                        None,
                    ) => {}
                    _ => {
                        return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
                    }
                }
            }
            unsupported => {
                return Err(MailboxError::InvalidNotificationFact(format!(
                    "unsupported notification resolution proof version {unsupported}"
                )));
            }
        }
        Ok(PreparedMutation::NotificationResolved {
            attempt_id,
            resolution,
        })
    }

    fn prepare_notification_resolution_without_terminal_action(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationResolvedWithoutTerminalAction {
            record_version,
            attempt_id,
            message_id,
            recipient,
            resolution,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_resolved_without_terminal_action".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let current = self
            .notifications
            .get(&(recipient, message_id))
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::AttentionRequired || current.binding.is_none() {
            return Err(MailboxError::NotificationClearRequiresAttention);
        }
        if self.resolved_attempts.contains_key(&attempt_id) {
            return Err(MailboxError::NotificationAlreadyResolved(attempt_id));
        }
        if resolution != NotificationResolution::Discard
            || self.resolution_actions_accepted.contains_key(&attempt_id)
            || self.resolution_consumptions.contains_key(&attempt_id)
            || self
                .resolution_intents
                .get(&attempt_id)
                .is_some_and(|recorded| *recorded != NotificationResolution::Discard)
        {
            return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
        }
        Ok(
            PreparedMutation::NotificationResolvedWithoutTerminalAction {
                attempt_id,
                resolution,
            },
        )
    }

    fn prepare_notification_resolution_intent(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationResolutionIntent {
            record_version,
            attempt_id,
            message_id,
            recipient,
            resolution,
            forced,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_resolution_intent".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let current = self
            .notifications
            .get(&(recipient, message_id))
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::AttentionRequired || current.binding.is_none() {
            return Err(MailboxError::NotificationClearRequiresAttention);
        }
        if self.resolved_attempts.contains_key(&attempt_id) {
            return Err(MailboxError::NotificationAlreadyResolved(attempt_id));
        }
        if self.resolution_intents.contains_key(&attempt_id) {
            return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
        }
        if forced && resolution != NotificationResolution::Complete {
            return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
        }
        Ok(PreparedMutation::NotificationResolutionIntent {
            attempt_id,
            resolution,
            forced,
        })
    }

    fn prepare_notification_resolution_action_reserved(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationResolutionActionReserved {
            record_version,
            attempt_id,
            message_id,
            recipient,
            resolution,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_resolution_action_reserved".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        self.validate_forced_notification_resolution_action_reservation(
            &message_id,
            recipient,
            attempt_id,
            resolution,
        )?;
        Ok(PreparedMutation::NotificationResolutionActionReserved {
            attempt_id,
            resolution,
        })
    }

    fn validate_forced_notification_resolution_action_reservation(
        &self,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    ) -> Result<bool, MailboxError> {
        let current = self
            .notifications
            .get(&(recipient, message_id.clone()))
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if !current.needs_exact_owned_reconciliation()
            || self.notification(recipient, message_id) != Some(current)
            || self.active_notification_barriers.get(&attempt_id) != Some(current)
        {
            return Err(MailboxError::NotificationClearRequiresAttention);
        }
        if !self.entry_is_pending(recipient, message_id) {
            return Err(MailboxError::NotificationMessageNotPending {
                message_id: message_id.clone(),
                recipient,
            });
        }
        if resolution != NotificationResolution::Complete
            || self.resolved_attempts.contains_key(&attempt_id)
            || self.resolution_actions_accepted.contains_key(&attempt_id)
            || self.resolution_consumptions.contains_key(&attempt_id)
            || self.resolution_intents.get(&attempt_id) != Some(&resolution)
            || !self.forced_resolution_intents.contains(&attempt_id)
        {
            return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
        }
        match self.resolution_action_reservations.get(&attempt_id) {
            Some(recorded) if *recorded == resolution => Ok(true),
            Some(_) => Err(MailboxError::NotificationResolutionAmbiguous(attempt_id)),
            None => Ok(false),
        }
    }

    fn prepare_notification_resolution_action_accepted(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationResolutionActionAccepted {
            record_version,
            attempt_id,
            message_id,
            recipient,
            resolution,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_resolution_action_accepted".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        self.validate_notification_resolution_action_accepted(
            &message_id,
            recipient,
            attempt_id,
            resolution,
        )?;
        Ok(PreparedMutation::NotificationResolutionActionAccepted {
            attempt_id,
            resolution,
        })
    }

    fn validate_notification_resolution_action_accepted(
        &self,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    ) -> Result<bool, MailboxError> {
        let current = self
            .notifications
            .get(&(recipient, message_id.clone()))
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::AttentionRequired || current.binding.is_none() {
            return Err(MailboxError::NotificationClearRequiresAttention);
        }
        if self.resolved_attempts.contains_key(&attempt_id) {
            return Err(MailboxError::NotificationAlreadyResolved(attempt_id));
        }
        if self.resolution_intents.get(&attempt_id) != Some(&resolution) {
            return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
        }
        match self.resolution_actions_accepted.get(&attempt_id) {
            Some(recorded) if *recorded == resolution => Ok(true),
            Some(_) => Err(MailboxError::NotificationResolutionAmbiguous(attempt_id)),
            None => Ok(false),
        }
    }

    fn prepare_notification_resolution_consumption_observed(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationResolutionConsumptionObserved {
            record_version,
            attempt_id,
            message_id,
            recipient,
            evidence,
            observed_at_ms,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_resolution_consumption_observed".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let observation = NotificationResolutionConsumptionObservation {
            evidence,
            observed_at_ms,
        };
        self.validate_notification_resolution_consumption_observed(
            &message_id,
            recipient,
            attempt_id,
            observation,
        )?;
        Ok(
            PreparedMutation::NotificationResolutionConsumptionObserved {
                attempt_id,
                observation,
            },
        )
    }

    fn validate_notification_resolution_consumption_observed(
        &self,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        observation: NotificationResolutionConsumptionObservation,
    ) -> Result<bool, MailboxError> {
        if observation.observed_at_ms == 0 {
            return Err(MailboxError::InvalidNotificationFact(
                "resolution consumption observation requires a positive timestamp".into(),
            ));
        }
        let current = self
            .notifications
            .get(&(recipient, message_id.clone()))
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::AttentionRequired || current.binding.is_none() {
            return Err(MailboxError::NotificationClearRequiresAttention);
        }
        if self.resolved_attempts.contains_key(&attempt_id) {
            return Err(MailboxError::NotificationAlreadyResolved(attempt_id));
        }
        if self.resolution_intents.get(&attempt_id) != Some(&NotificationResolution::Complete)
            || self.resolution_actions_accepted.get(&attempt_id)
                != Some(&NotificationResolution::Complete)
        {
            return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
        }
        match self.resolution_consumptions.get(&attempt_id) {
            Some(recorded) if *recorded == observation => Ok(true),
            Some(_) => Err(MailboxError::NotificationResolutionAmbiguous(attempt_id)),
            None => Ok(false),
        }
    }

    fn prepare_notification_claimed_staged_clear(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationClaimedStagedCleared {
            record_version,
            attempt_id,
            message_id,
            recipient,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_claimed_staged_cleared".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let key = (recipient, message_id.clone());
        let current =
            self.notifications
                .get(&key)
                .ok_or_else(|| MailboxError::NotificationNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if current.state != NotificationState::Staged
            || current.transport != NotificationTransport::Doorbell
        {
            return Err(MailboxError::InvalidNotificationTransition {
                from: current.state,
                to: NotificationState::WithdrawnAfterStaging,
            });
        }
        let claimed_by_recipient = self.get_entry(recipient, &message_id).is_some_and(|entry| {
            matches!(
                &entry.state,
                MailboxEntryState::Claimed { claimant, .. } if *claimant == recipient
            )
        });
        if !claimed_by_recipient {
            return Err(MailboxError::InvalidNotificationFact(
                "claimed staged clear requires the exact recipient claim".into(),
            ));
        }
        let active = self
            .active_notification_barriers
            .get(&attempt_id)
            .ok_or(MailboxError::NotificationBarrierNotActive(attempt_id))?;
        if active.message_id != message_id || active.recipient != recipient {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: active.attempt_id,
                found: attempt_id,
            });
        }
        if active.state != NotificationState::Staged
            || active.binding != current.binding
            || active.transport != current.transport
            || active.doorbell_format != current.doorbell_format
        {
            return Err(MailboxError::InvalidNotificationFact(
                "claimed staged clear does not match the exact active barrier".into(),
            ));
        }

        let mut record = current.clone();
        record.state = NotificationState::WithdrawnAfterStaging;
        record.cause = None;
        record.pre_write_cause = None;
        record.pre_write_observation = None;
        record.updated_seq = line.seq;
        record.updated_at = line.ts;
        Ok(PreparedMutation::NotificationClaimedStagedCleared {
            key,
            record,
            attempt_id,
        })
    }

    fn prepare_notification_claimed_ack_timeout_reconciliation(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationClaimedAckTimeoutReconciled {
            record_version,
            attempt_id,
            message_id,
            recipient,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_claimed_ack_timeout_reconciled".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let key = (recipient, message_id.clone());
        let current =
            self.notifications
                .get(&key)
                .ok_or_else(|| MailboxError::NotificationNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if !current.needs_claimed_ack_timeout_reconciliation() {
            return Err(MailboxError::InvalidNotificationTransition {
                from: current.state,
                to: NotificationState::Notified,
            });
        }
        if !self.exact_recipient_claimed_after_write(current) {
            return Err(MailboxError::InvalidNotificationFact(
                "ACK-timeout reconciliation requires an exact recipient claim after write".into(),
            ));
        }
        let active = self
            .active_notification_barriers
            .get(&attempt_id)
            .ok_or(MailboxError::NotificationBarrierNotActive(attempt_id))?;
        if active != current {
            return Err(MailboxError::InvalidNotificationFact(
                "ACK-timeout reconciliation does not match the exact active barrier".into(),
            ));
        }

        let mut record = current.clone();
        record.state = NotificationState::Notified;
        record.cause = None;
        record.pre_write_cause = None;
        record.pre_write_observation = None;
        record.updated_seq = line.seq;
        record.updated_at = line.ts;
        Ok(PreparedMutation::NotificationClaimedAckTimeoutReconciled {
            key,
            record,
            attempt_id,
        })
    }

    fn prepare_notification_barrier_retirement(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationBarrierRetired {
            record_version,
            attempt_id,
            message_id,
            recipient,
            cause,
            replacement,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_barrier_retired".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let active = self
            .active_notification_barriers
            .get(&attempt_id)
            .ok_or(MailboxError::NotificationBarrierNotActive(attempt_id))?;
        if active.message_id != message_id || active.recipient != recipient {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: active.attempt_id,
                found: attempt_id,
            });
        }

        match cause {
            NotificationBarrierRetirementCause::OccupantReplaced => {
                let replacement = replacement
                    .as_ref()
                    .filter(|binding| binding.leader.is_some())
                    .ok_or(MailboxError::NotificationBarrierReplacementRequired)?;
                if replacement.recipient != recipient {
                    return Err(MailboxError::NotificationBindingMismatch);
                }
                if active.binding.as_ref().is_some_and(|binding| {
                    binding.agent == replacement.agent && binding.manifest == replacement.manifest
                }) {
                    return Err(MailboxError::NotificationBarrierReplacementUnchanged);
                }
            }
            NotificationBarrierRetirementCause::ComposerObservedClear => {
                if replacement.is_some() {
                    return Err(MailboxError::NotificationBarrierReplacementForbidden);
                }
                if !matches!(
                    active.state,
                    NotificationState::Notified | NotificationState::WithdrawnAfterStaging
                ) {
                    return Err(MailboxError::NotificationBarrierRetirementState {
                        cause,
                        state: active.state,
                    });
                }
            }
            NotificationBarrierRetirementCause::RecipientClaimedComposerClear => {
                if replacement.is_some() {
                    return Err(MailboxError::NotificationBarrierReplacementForbidden);
                }
                if !matches!(
                    active.state,
                    NotificationState::AttentionRequired | NotificationState::Notified
                ) || !uses_incomplete_legacy_doorbell_contract(active)
                    || !self.exact_recipient_claimed_after_write(active)
                {
                    return Err(MailboxError::NotificationBarrierRetirementState {
                        cause,
                        state: active.state,
                    });
                }
            }
            NotificationBarrierRetirementCause::LifecycleReconciled => {
                if replacement.is_some() {
                    return Err(MailboxError::NotificationBarrierReplacementForbidden);
                }
                if active.needs_claimed_ack_timeout_reconciliation()
                    && self.exact_recipient_claimed_after_write(active)
                {
                    return Err(MailboxError::NotificationBarrierRetirementState {
                        cause,
                        state: active.state,
                    });
                }
            }
            NotificationBarrierRetirementCause::PaneGone => {
                if replacement.is_some() {
                    return Err(MailboxError::NotificationBarrierReplacementForbidden);
                }
            }
        }

        Ok(PreparedMutation::NotificationBarrierRetired { attempt_id })
    }

    fn prepare_notification_resolution_intent_withdrawn(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
        let fact: NotificationFact = serde_json::from_value(
            line.data
                .clone()
                .ok_or_else(|| MailboxError::InvalidNotificationFact("missing data".into()))?,
        )
        .map_err(|error| MailboxError::InvalidNotificationFact(error.to_string()))?;
        let NotificationFact::NotificationResolutionIntentWithdrawn {
            record_version,
            attempt_id,
            message_id,
            recipient,
            resolution,
        } = fact
        else {
            return Err(MailboxError::InvalidNotificationFact(
                "expected notification_resolution_intent_withdrawn".into(),
            ));
        };

        self.validate_notification_envelope(line, record_version, &message_id, recipient)?;
        let current = self
            .notifications
            .get(&(recipient, message_id))
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.attempt_id != attempt_id {
            return Err(MailboxError::NotificationAttemptMismatch {
                expected: current.attempt_id,
                found: attempt_id,
            });
        }
        if self.resolution_intents.get(&attempt_id) != Some(&resolution)
            || self.resolved_attempts.contains_key(&attempt_id)
            || self
                .resolution_action_reservations
                .contains_key(&attempt_id)
            || self.resolution_actions_accepted.contains_key(&attempt_id)
            || self.resolution_consumptions.contains_key(&attempt_id)
        {
            return Err(MailboxError::NotificationResolutionAmbiguous(attempt_id));
        }
        Ok(PreparedMutation::NotificationResolutionIntentWithdrawn { attempt_id })
    }

    fn validate_notification_envelope(
        &self,
        line: &LedgerLine,
        record_version: u32,
        message_id: &MessageId,
        recipient: RecipientKey,
    ) -> Result<(), MailboxError> {
        if record_version != CANONICAL_RECORD_VERSION {
            return Err(MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: record_version,
            });
        }
        if recipient.workspace_id() != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: recipient.workspace_id(),
            });
        }
        if recipient.is_admin() {
            return Err(MailboxError::NotificationRecipientNotAgent);
        }
        if line.id != message_id.as_str() {
            return Err(MailboxError::EnvelopeMismatch {
                envelope_id: line.id.clone(),
                fact_id: message_id.clone(),
            });
        }
        if line.from != "cyclopsd" {
            return Err(MailboxError::PresentationMismatch {
                field: "from",
                presentation: line.from.clone(),
                authoritative: "cyclopsd".into(),
            });
        }
        let expected_to = vec![recipient.to_string()];
        if line.to != expected_to {
            return Err(MailboxError::PresentationMismatch {
                field: "to",
                presentation: format!("{:?}", line.to),
                authoritative: format!("{expected_to:?}"),
            });
        }
        if line.subject.is_some()
            || line.body.is_some()
            || line.reply_to.is_some()
            || !line.deliveries.is_empty()
        {
            return Err(MailboxError::UncanonicalRow(format!(
                "notification row seq {} contains message or delivery fields",
                line.seq
            )));
        }
        if !self.messages.contains_key(message_id) {
            return Err(MailboxError::MessageNotFound(message_id.clone()));
        }
        if self.get_entry(recipient, message_id).is_none() {
            return Err(MailboxError::EntryNotFound {
                message_id: message_id.clone(),
                recipient,
            });
        }
        Ok(())
    }

    fn validate_notification_requeues_envelope(
        &self,
        line: &LedgerLine,
        record_version: u32,
        message_id: &MessageId,
        requeues: &[NotificationRequeue],
    ) -> Result<(), MailboxError> {
        if requeues.len() < 2 {
            return Err(MailboxError::InvalidNotificationFact(
                "notifications_requeued requires at least two recipients".into(),
            ));
        }
        if record_version != CANONICAL_RECORD_VERSION {
            return Err(MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: record_version,
            });
        }

        let mut recipients = HashSet::with_capacity(requeues.len());
        for requeue in requeues {
            if requeue.recipient.workspace_id() != self.workspace_id {
                return Err(MailboxError::WorkspaceMismatch {
                    expected: self.workspace_id,
                    found: requeue.recipient.workspace_id(),
                });
            }
            if requeue.recipient.is_admin() {
                return Err(MailboxError::NotificationRecipientNotAgent);
            }
            if !recipients.insert(requeue.recipient) {
                return Err(MailboxError::InvalidNotificationFact(
                    "notifications_requeued contains a duplicate recipient".into(),
                ));
            }
        }
        if line.id != message_id.as_str() {
            return Err(MailboxError::EnvelopeMismatch {
                envelope_id: line.id.clone(),
                fact_id: message_id.clone(),
            });
        }
        if line.from != "cyclopsd" {
            return Err(MailboxError::PresentationMismatch {
                field: "from",
                presentation: line.from.clone(),
                authoritative: "cyclopsd".into(),
            });
        }
        let expected_to: Vec<_> = requeues
            .iter()
            .map(|requeue| requeue.recipient.to_string())
            .collect();
        if line.to != expected_to {
            return Err(MailboxError::PresentationMismatch {
                field: "to",
                presentation: format!("{:?}", line.to),
                authoritative: format!("{expected_to:?}"),
            });
        }
        if line.subject.is_some()
            || line.body.is_some()
            || line.reply_to.is_some()
            || !line.deliveries.is_empty()
        {
            return Err(MailboxError::UncanonicalRow(format!(
                "notification row seq {} contains message or delivery fields",
                line.seq
            )));
        }
        if !self.messages.contains_key(message_id) {
            return Err(MailboxError::MessageNotFound(message_id.clone()));
        }
        for recipient in recipients {
            if self.get_entry(recipient, message_id).is_none() {
                return Err(MailboxError::EntryNotFound {
                    message_id: message_id.clone(),
                    recipient,
                });
            }
        }
        Ok(())
    }

    fn require_pending_entry(
        &self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> Result<(), MailboxError> {
        let entry =
            self.get_entry(recipient, message_id)
                .ok_or_else(|| MailboxError::EntryNotFound {
                    message_id: message_id.clone(),
                    recipient,
                })?;
        if !entry.state.is_pending() {
            return Err(MailboxError::NotificationMessageNotPending {
                message_id: message_id.clone(),
                recipient,
            });
        }
        Ok(())
    }

    fn commit_line(&mut self, line: LedgerLine, prepared: PreparedMutation) {
        self.last_workspace_seq = Some(line.seq);
        match prepared {
            PreparedMutation::Message {
                message_id,
                metadata,
                superseded_notification,
            } => {
                if let Some(superseded) = &metadata.supersedes {
                    let recipient = metadata.recipients[0];
                    let entry = self
                        .get_entry_mut(recipient, superseded)
                        .expect("prepared supersession retains its mailbox entry");
                    entry.state = MailboxEntryState::Superseded {
                        by: message_id.clone(),
                        superseded_at: line.ts,
                    };
                }
                if let Some(client_key) = metadata.client_key {
                    self.idempotency_index.insert(
                        (metadata.sender, client_key),
                        (message_id.clone(), metadata.request_digest),
                    );
                }
                for recipient in metadata.recipients {
                    let entry = MailboxEntry {
                        message_id: message_id.clone(),
                        recipient,
                        state: MailboxEntryState::Pending,
                        seq: line.seq,
                        created_at: line.ts,
                    };
                    self.mailboxes
                        .entry(recipient)
                        .or_default()
                        .insert(line.seq, entry);
                    let previous = self
                        .mailbox_index
                        .insert((recipient, message_id.clone()), line.seq);
                    debug_assert!(previous.is_none(), "prepared message is unique");
                }
                if let Some(superseded) = superseded_notification {
                    self.notifications.insert(superseded.key, superseded.record);
                }
                self.messages.insert(message_id, line);
            }
            PreparedMutation::Claim {
                message_id,
                recipient,
                claimant,
                notification_update,
            } => {
                self.claim_sequences
                    .insert((recipient, message_id.clone()), line.seq);
                let entry = self
                    .get_entry_mut(recipient, &message_id)
                    .expect("prepared claim retains its mailbox entry");
                entry.state = MailboxEntryState::Claimed {
                    claimant,
                    claimed_at: line.ts,
                };
                if let Some(update) = notification_update {
                    if self
                        .active_notification_barriers
                        .contains_key(&update.record.attempt_id)
                    {
                        self.active_notification_barriers
                            .insert(update.record.attempt_id, update.record.clone());
                    }
                    self.notifications.insert(update.key, update.record);
                }
            }
            PreparedMutation::DeliveredDirect {
                message_id,
                recipient,
                attempt_id,
            } => {
                let entry = self
                    .get_entry_mut(recipient, &message_id)
                    .expect("prepared direct delivery retains its mailbox entry");
                entry.state = MailboxEntryState::DeliveredDirect {
                    attempt_id,
                    delivered_at: line.ts,
                };
            }
            PreparedMutation::Notification {
                key,
                record,
                new_attempt,
            } => {
                if new_attempt {
                    self.notification_attempts.insert(record.attempt_id);
                }
                let bound_write =
                    record.state == NotificationState::Writing && record.binding.is_some();
                if bound_write {
                    self.notification_write_sequences
                        .insert(record.attempt_id, line.seq);
                    // A later admitted write proves the prior barrier released.
                    // RecipientKey keeps compaction on the exact durable route.
                    self.active_notification_barriers
                        .retain(|attempt_id, active| {
                            *attempt_id == record.attempt_id || active.recipient != record.recipient
                        });
                }
                let proven_unwritten = record.state == NotificationState::BlockedPreWrite
                    && record.pre_write_cause
                        == Some(NotificationPreWriteCause::PasteCommandUnwritten);
                if proven_unwritten {
                    self.active_notification_barriers.remove(&record.attempt_id);
                    self.notification_write_sequences.remove(&record.attempt_id);
                } else if bound_write
                    || self
                        .active_notification_barriers
                        .contains_key(&record.attempt_id)
                {
                    self.active_notification_barriers
                        .insert(record.attempt_id, record.clone());
                }
                self.notifications.insert(key, record);
            }
            PreparedMutation::NotificationRequeues { records } => {
                for (key, record) in records {
                    self.notification_attempts.insert(record.attempt_id);
                    self.notifications.insert(key, record);
                }
            }
            PreparedMutation::NotificationCleared { attempt_id } => {
                self.cleared_attempts.insert(attempt_id);
            }
            PreparedMutation::NotificationsCleared {
                batch_id,
                attempt_ids,
            } => {
                self.clearance_batches.insert(batch_id);
                self.cleared_attempts.extend(attempt_ids);
            }
            PreparedMutation::NotificationWithdrawnBeforeWrite { key, record } => {
                self.notifications.insert(key, record);
            }
            PreparedMutation::NotificationResolutionIntent {
                attempt_id,
                resolution,
                forced,
            } => {
                self.resolution_intents.insert(attempt_id, resolution);
                if forced {
                    self.forced_resolution_intents.insert(attempt_id);
                }
            }
            PreparedMutation::NotificationResolutionActionReserved {
                attempt_id,
                resolution,
            } => {
                self.resolution_action_reservations
                    .insert(attempt_id, resolution);
                self.resolution_action_reservation_sequences
                    .insert(attempt_id, line.seq);
            }
            PreparedMutation::NotificationResolutionActionAccepted {
                attempt_id,
                resolution,
            } => {
                self.resolution_actions_accepted
                    .insert(attempt_id, resolution);
                self.resolution_action_sequences
                    .insert(attempt_id, line.seq);
            }
            PreparedMutation::NotificationResolutionConsumptionObserved {
                attempt_id,
                observation,
            } => {
                self.resolution_consumptions.insert(attempt_id, observation);
            }
            PreparedMutation::NotificationResolutionIntentWithdrawn { attempt_id } => {
                self.resolution_intents.remove(&attempt_id);
                self.forced_resolution_intents.remove(&attempt_id);
            }
            PreparedMutation::NotificationResolvedWithoutTerminalAction {
                attempt_id,
                resolution,
            } => {
                self.resolved_attempts.insert(attempt_id, resolution);
                self.forced_resolution_intents.remove(&attempt_id);
                self.active_notification_barriers.remove(&attempt_id);
            }
            PreparedMutation::NotificationResolved {
                attempt_id,
                resolution,
            } => {
                self.resolved_attempts.insert(attempt_id, resolution);
                self.forced_resolution_intents.remove(&attempt_id);
                self.active_notification_barriers.remove(&attempt_id);
            }
            PreparedMutation::NotificationClaimedStagedCleared {
                key,
                record,
                attempt_id,
            } => {
                self.notifications.insert(key, record);
                self.active_notification_barriers.remove(&attempt_id);
            }
            PreparedMutation::NotificationClaimedAckTimeoutReconciled {
                key,
                record,
                attempt_id,
            } => {
                self.notifications.insert(key, record);
                self.active_notification_barriers.remove(&attempt_id);
                self.claimed_ack_timeout_reconciliations.insert(attempt_id);
            }
            PreparedMutation::NotificationBarrierRetired { attempt_id } => {
                self.active_notification_barriers.remove(&attempt_id);
            }
        }
    }

    /// Retrieve canonical message line by ID.
    pub fn get_message(&self, message_id: &MessageId) -> Option<&LedgerLine> {
        self.messages.get(message_id)
    }

    /// Is this recipient's entry still waiting to be claimed?
    ///
    /// Requeue redelivers, so it needs one. Clearance only acknowledges,
    /// so it does not.
    pub fn entry_is_pending(&self, recipient: RecipientKey, message_id: &MessageId) -> bool {
        self.get_entry(recipient, message_id)
            .is_some_and(|entry| entry.state.is_pending())
    }

    /// Whether this exact force-submit timer target is still eligible under
    /// the workspace journal lock.
    fn force_submit_target_is_pending(&self, target: &AttentionTarget) -> bool {
        let current = self.alarm_by_attempt(target.record.attempt_id);
        current == Some(&target.record)
            && current.is_some_and(NotificationRecord::needs_exact_owned_reconciliation)
            && self.notification(target.record.recipient, &target.record.message_id) == current
            && self
                .active_notification_barriers
                .get(&target.record.attempt_id)
                == current
            && self.entry_is_pending(target.record.recipient, &target.record.message_id)
    }

    /// Choose the terminal recovery for one exact active composer barrier.
    ///
    /// An existing durable intent always wins. For a fresh action, pending
    /// means submit the staged doorbell and a claim ordered after the write
    /// means clear it. Selection and intent persistence share the store lock
    /// so a concurrent claim lands wholly before or after that boundary.
    fn exact_owned_resolution(&self, target: &AttentionTarget) -> Option<NotificationResolution> {
        let current = self.alarm_by_attempt(target.record.attempt_id)?;
        if current != &target.record
            || self.attention_resolved(current.attempt_id)
            || !current.needs_exact_owned_reconciliation()
            || self.notification(current.recipient, &current.message_id) != Some(current)
            || self.active_notification_barriers.get(&current.attempt_id) != Some(current)
        {
            return None;
        }
        if let Some(recorded) = self.attention_resolution_intent(current.attempt_id) {
            return Some(recorded);
        }
        if self.entry_is_pending(current.recipient, &current.message_id) {
            Some(NotificationResolution::Complete)
        } else if self.exact_recipient_claimed_after_write(current) {
            Some(NotificationResolution::Discard)
        } else {
            None
        }
    }

    fn exact_owned_recovery_action(
        &self,
        current: &NotificationRecord,
    ) -> ExactOwnedRecoveryAction {
        if self.attention_resolved(current.attempt_id)
            || !current.needs_exact_owned_reconciliation()
            || self.notification(current.recipient, &current.message_id) != Some(current)
            || self.active_notification_barriers.get(&current.attempt_id) != Some(current)
        {
            return ExactOwnedRecoveryAction::Ineligible;
        }
        match self.attention_resolution_intent(current.attempt_id) {
            None if self.entry_is_pending(current.recipient, &current.message_id) => {
                ExactOwnedRecoveryAction::Submit
            }
            None if self.exact_recipient_claimed_after_write(current) => {
                ExactOwnedRecoveryAction::Clear
            }
            Some(NotificationResolution::Complete)
                if self.attention_resolution_action_accepted(current.attempt_id)
                    == Some(NotificationResolution::Complete)
                    && (self
                        .attention_resolution_consumption_observed(current.attempt_id)
                        .is_some()
                        || self.exact_claim_after_attention_action(current).is_some()) =>
            {
                ExactOwnedRecoveryAction::Reconcile
            }
            Some(NotificationResolution::Discard)
                if self.attention_resolution_action_accepted(current.attempt_id)
                    == Some(NotificationResolution::Discard) =>
            {
                ExactOwnedRecoveryAction::Reconcile
            }
            Some(_) => ExactOwnedRecoveryAction::Inspect,
            None => ExactOwnedRecoveryAction::Ineligible,
        }
    }

    /// Oldest exact notification barrier owned by a durable recipient claim.
    ///
    /// A staged doorbell and an exact-attempt ACK-timeout doorbell keep their original
    /// FIFO position until byte-exact composer reconciliation settles them.
    pub(crate) fn claimed_notification_barrier(
        &self,
        recipient: RecipientKey,
    ) -> Option<&NotificationRecord> {
        self.notifications
            .values()
            .filter(|record| {
                record.recipient == recipient
                    && record.transport == NotificationTransport::Doorbell
                    && (record.state == NotificationState::Staged
                        || record.needs_claimed_ack_timeout_reconciliation())
                    && self.exact_recipient_claimed_after_write(record)
                    && self
                        .active_notification_barriers
                        .get(&record.attempt_id)
                        .is_some_and(|active| active == *record)
            })
            .min_by_key(|record| record.started_seq)
    }

    /// Oldest pre-submit operator notification whose mailbox payload was
    /// already claimed through the socket. Retrieval does not relinquish this
    /// notification's FIFO position.
    fn claimed_operator_notification(
        &self,
        recipient: RecipientKey,
    ) -> Option<&NotificationRecord> {
        self.notifications
            .values()
            .filter(|record| {
                record.recipient == recipient
                    && record.transport == NotificationTransport::Doorbell
                    && matches!(
                        record.state,
                        NotificationState::Queued
                            | NotificationState::Gating
                            | NotificationState::BlockedPreWrite
                            | NotificationState::QuotaHeld
                            | NotificationState::QuotaResetObserved
                            | NotificationState::Writing
                            | NotificationState::Staged
                            | NotificationState::Submitting
                    )
                    && self
                        .get_entry(recipient, &record.message_id)
                        .is_some_and(|entry| entry.state.is_claimed())
            })
            .min_by_key(|record| record.started_seq)
    }

    /// Retrieve one recipient's entry for a message.
    pub fn get_entry(
        &self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> Option<&MailboxEntry> {
        let seq = self.mailbox_index.get(&(recipient, message_id.clone()))?;
        self.mailboxes.get(&recipient)?.get(seq)
    }

    fn get_entry_mut(
        &mut self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> Option<&mut MailboxEntry> {
        let seq = *self.mailbox_index.get(&(recipient, message_id.clone()))?;
        self.mailboxes.get_mut(&recipient)?.get_mut(&seq)
    }

    /// Retrieve all mailbox entries for a specific recipient in authoritative FIFO order.
    pub fn get_mailbox(&self, recipient: RecipientKey) -> Vec<&MailboxEntry> {
        self.mailboxes
            .get(&recipient)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    /// Retrieve all pending mailbox entries for a recipient in authoritative FIFO order.
    pub fn get_pending(&self, recipient: RecipientKey) -> Vec<&MailboxEntry> {
        self.mailboxes
            .get(&recipient)
            .map(|m| m.values().filter(|e| e.state.is_pending()).collect())
            .unwrap_or_default()
    }

    /// Current notification attempt for one recipient and message.
    pub fn notification(
        &self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> Option<&NotificationRecord> {
        self.notifications.get(&(recipient, message_id.clone()))
    }

    /// A withdrawn wake leaves its mailbox entry pending and pullable, but it
    /// no longer occupies notification FIFO. Scheduler selection and every
    /// notification-facing FIFO position use this same predicate so an
    /// operator never sees a queue position that disagrees with what will
    /// actually be notified next.
    fn notification_withdrawn_by_operator(
        &self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> bool {
        self.notification(recipient, message_id)
            .is_some_and(|record| record.state == NotificationState::WithdrawnByOperator)
    }

    /// Full post-write records whose composer barriers remain active.
    pub(crate) fn active_notification_barriers(&self) -> Vec<NotificationRecord> {
        let mut records: Vec<_> = self
            .active_notification_barriers
            .values()
            .cloned()
            .collect();
        records.sort_by_key(|record| record.started_seq);
        records
    }

    /// Current notification attempts held at the readiness gate.
    pub(crate) fn gating_notifications(&self) -> Vec<NotificationRecord> {
        let mut records: Vec<_> = self
            .notifications
            .values()
            .filter(|record| record.state == NotificationState::Gating)
            .cloned()
            .collect();
        records.sort_by_key(|record| record.started_seq);
        records
    }

    /// Current notification attempts for a recipient, ordered by start sequence.
    /// Attention-required attempts no operator has acknowledged yet,
    /// oldest first. Ordered so two calls on one projection agree.
    pub fn open_alarms(&self) -> Vec<&NotificationRecord> {
        let mut alarms: Vec<_> = self
            .notifications
            .values()
            .filter(|record| record.state == NotificationState::AttentionRequired)
            .filter(|record| !self.resolved_attempts.contains_key(&record.attempt_id))
            .filter(|record| !self.cleared_attempts.contains(&record.attempt_id))
            .collect();
        alarms.sort_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then_with(|| a.attempt_id.to_string().cmp(&b.attempt_id.to_string()))
        });
        alarms
    }

    /// Uncleared alarms for one message, across every recipient.
    pub fn open_alarms_for_message(&self, message_id: &MessageId) -> Vec<&NotificationRecord> {
        self.open_alarms()
            .into_iter()
            .filter(|record| &record.message_id == message_id)
            .collect()
    }

    /// The record an attempt identifier currently names, cleared or not.
    ///
    /// Only the current attempt of a record matches. An identifier that a
    /// requeue has superseded names nothing, which is what keeps a stale
    /// clearance off the attempt that replaced it.
    pub fn alarm_by_attempt(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Option<&NotificationRecord> {
        self.notification_by_attempt(attempt_id)
    }

    pub fn notification_by_attempt(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Option<&NotificationRecord> {
        self.notifications
            .values()
            .find(|record| record.attempt_id == attempt_id)
    }

    fn notification_settlement(
        &self,
        record: &NotificationRecord,
    ) -> Option<MessageNotificationSettlement> {
        MessageNotificationSettlement::from_notification(record.state)
    }

    fn notification_summary(
        &self,
        record: Option<&NotificationRecord>,
    ) -> MessageNotificationSummary {
        let Some(record) = record else {
            return MessageNotificationSummary {
                state: MessageNotificationState::NotStarted,
                wake_block: None,
                quota_state: None,
                settlement: None,
                operator_withdrawn: None,
                attempt_id: None,
                cause: None,
                verify_outcome: None,
                pre_write_cause: None,
                pre_write_pane_width: None,
                pre_write_required_pane_width: None,
                pre_write_block: None,
                attention_cleared: None,
                resolution: None,
                resolution_intent: None,
                resolution_action_accepted: None,
                resolution_consumption_observed: None,
                updated_at: None,
            };
        };
        let attention_cleared = (record.state == NotificationState::AttentionRequired)
            .then(|| self.alarm_cleared(record.attempt_id));
        let resolution = self.resolved_attempts.get(&record.attempt_id).copied();
        let resolution_intent = self
            .resolution_intents
            .get(&record.attempt_id)
            .copied()
            .filter(|_| resolution.is_none());
        let resolution_action_accepted = self
            .resolution_actions_accepted
            .get(&record.attempt_id)
            .copied()
            .filter(|_| resolution.is_none());
        let resolution_consumption_observed = self
            .resolution_consumptions
            .get(&record.attempt_id)
            .copied()
            .filter(|observation| observation.evidence.proves_exact_consumption())
            .filter(|_| resolution.is_none());
        let width_block = notification_pre_write_width_block(record);
        MessageNotificationSummary {
            state: record.state.into(),
            wake_block: record.wake_block.or_else(|| {
                self.attention_resolution_pending(record.attempt_id)
                    .then_some(MessageWakeBlock::AttentionResolutionPending)
            }),
            quota_state: MessageQuotaState::from_notification(record.state),
            settlement: self.notification_settlement(record),
            operator_withdrawn: (record.state == NotificationState::WithdrawnByOperator)
                .then_some(true),
            attempt_id: Some(record.attempt_id),
            cause: record.cause,
            verify_outcome: record.verify_outcome,
            pre_write_cause: record.pre_write_cause,
            pre_write_pane_width: width_block.map(|(observed, _)| observed),
            pre_write_required_pane_width: width_block.map(|(_, required)| required),
            pre_write_block: record
                .pre_write_observation
                .as_ref()
                .and_then(|observation| observation.write_block.clone()),
            attention_cleared,
            resolution,
            resolution_intent,
            resolution_action_accepted,
            resolution_consumption_observed,
            updated_at: Some(record.updated_at),
        }
    }

    /// Has an operator already acknowledged this attempt?
    pub fn alarm_cleared(&self, attempt_id: NotificationAttemptId) -> bool {
        self.cleared_attempts.contains(&attempt_id)
    }

    pub fn attention_resolved(&self, attempt_id: NotificationAttemptId) -> bool {
        self.resolved_attempts.contains_key(&attempt_id)
    }

    pub fn attention_resolution_pending(&self, attempt_id: NotificationAttemptId) -> bool {
        self.resolution_intents.contains_key(&attempt_id)
            && !self.resolved_attempts.contains_key(&attempt_id)
    }

    pub fn attention_resolution_intent(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Option<NotificationResolution> {
        if self.attention_resolution_pending(attempt_id) {
            self.resolution_intents.get(&attempt_id).copied()
        } else {
            None
        }
    }

    pub(crate) fn attention_resolution_action_accepted(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Option<NotificationResolution> {
        self.resolution_actions_accepted.get(&attempt_id).copied()
    }

    pub(crate) fn attention_resolution_consumption_observed(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Option<NotificationResolutionConsumptionObservation> {
        self.resolution_consumptions
            .get(&attempt_id)
            .copied()
            .filter(|observation| observation.evidence.proves_exact_consumption())
    }

    fn exact_claim_after_attention_action(
        &self,
        record: &NotificationRecord,
    ) -> Option<NotificationResolutionConsumptionObservation> {
        // Ordinary actions linearize when their terminal key is accepted.
        // Forced Complete uses its separately validated reservation instead:
        // the reservation and inbox claim share the journal lock, while the
        // terminal write necessarily occurs after that lock is released.
        let accepted_seq = self
            .resolution_action_sequences
            .get(&record.attempt_id)
            .copied()?;
        let action_seq = self
            .resolution_action_reservation_sequences
            .get(&record.attempt_id)
            .copied()
            .unwrap_or(accepted_seq);
        let claim_seq = self
            .claim_sequences
            .get(&(record.recipient, record.message_id.clone()))
            .copied()?;
        if claim_seq <= action_seq {
            return None;
        }
        let entry = self.get_entry(record.recipient, &record.message_id)?;
        let MailboxEntryState::Claimed {
            claimant,
            claimed_at,
        } = &entry.state
        else {
            return None;
        };
        if *claimant != record.recipient {
            return None;
        }
        Some(NotificationResolutionConsumptionObservation {
            evidence: NotificationResolutionConsumptionEvidence::AuthenticatedClaim,
            observed_at_ms: *claimed_at,
        })
    }

    pub(crate) fn exact_recipient_claimed_after_write(&self, record: &NotificationRecord) -> bool {
        let Some(write_seq) = self
            .notification_write_sequences
            .get(&record.attempt_id)
            .copied()
        else {
            return false;
        };
        let Some(claim_seq) = self
            .claim_sequences
            .get(&(record.recipient, record.message_id.clone()))
            .copied()
        else {
            return false;
        };
        claim_seq > write_seq
            && self
                .get_entry(record.recipient, &record.message_id)
                .is_some_and(|entry| {
                    matches!(
                        &entry.state,
                        MailboxEntryState::Claimed { claimant, .. }
                            if *claimant == record.recipient
                    )
                })
    }

    fn unresolved_attention_for_message(&self, message_id: &MessageId) -> Vec<&NotificationRecord> {
        let mut records: Vec<_> = self
            .notifications
            .values()
            .filter(|record| &record.message_id == message_id)
            .filter(|record| record.state == NotificationState::AttentionRequired)
            .filter(|record| !self.resolved_attempts.contains_key(&record.attempt_id))
            .collect();
        records.sort_by_key(|record| record.started_seq);
        records
    }

    fn requeueable_for_message(&self, message_id: &MessageId) -> Vec<&NotificationRecord> {
        let mut records: Vec<_> = self
            .notifications
            .values()
            .filter(|record| &record.message_id == message_id)
            .filter(|record| {
                record.state == NotificationState::QuotaResetObserved
                    || (record.state == NotificationState::AttentionRequired
                        && !self.cleared_attempts.contains(&record.attempt_id))
            })
            .filter(|record| !self.resolved_attempts.contains_key(&record.attempt_id))
            .collect();
        records.sort_by_key(|record| record.started_seq);
        records
    }

    fn quota_held_for_recipient(&self, recipient: RecipientKey) -> Vec<&NotificationRecord> {
        let mut records: Vec<_> = self
            .notifications
            .values()
            .filter(|record| record.recipient == recipient)
            .filter(|record| record.state == NotificationState::QuotaHeld)
            .collect();
        records.sort_by_key(|record| record.started_seq);
        records
    }

    pub fn notifications_for(&self, recipient: RecipientKey) -> Vec<&NotificationRecord> {
        let mut records: Vec<_> = self
            .notifications
            .values()
            .filter(|record| record.recipient == recipient)
            .collect();
        records.sort_by_key(|record| record.started_seq);
        records
    }

    pub fn pending_count(&self, recipient: RecipientKey) -> usize {
        self.mailboxes
            .get(&recipient)
            .map(|entries| {
                entries
                    .values()
                    .filter(|entry| entry.state.is_pending())
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn recipient_label(&self, recipient: RecipientKey) -> Option<String> {
        let entries = self.mailboxes.get(&recipient)?;
        for entry in entries.values().rev() {
            if let Some(message) = self.messages.get(&entry.message_id) {
                if let Ok(metadata) = extract_message_metadata(message) {
                    if let Ok((_, recipient_labels)) =
                        presentation_labels(&metadata.recipients, &metadata.presentation)
                    {
                        if let Some(pos) = metadata.recipients.iter().position(|k| *k == recipient)
                        {
                            if let Some(label) = recipient_labels.get(pos) {
                                return Some(label.clone());
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// List mailbox metadata and immutable display labels without message bodies.
    pub fn list_mailbox(
        &self,
        recipient: RecipientKey,
    ) -> Result<Vec<MailboxListItem>, MailboxError> {
        self.get_pending(recipient)
            .into_iter()
            .map(|entry| {
                let message = self
                    .messages
                    .get(&entry.message_id)
                    .ok_or_else(|| MailboxError::MessageNotFound(entry.message_id.clone()))?;
                let metadata = extract_message_metadata(message)?;
                let (sender_label, recipient_labels) =
                    presentation_labels(&metadata.recipients, &metadata.presentation)?;
                let recipient_index = metadata
                    .recipients
                    .iter()
                    .position(|key| *key == recipient)
                    .ok_or_else(|| MailboxError::EntryNotFound {
                        message_id: entry.message_id.clone(),
                        recipient,
                    })?;
                Ok(MailboxListItem {
                    entry: entry.clone(),
                    kind: message.kind,
                    sender: metadata.sender,
                    sender_label,
                    recipient_label: recipient_labels[recipient_index].clone(),
                    subject: message.subject.clone(),
                    reply_to: message
                        .reply_to
                        .as_deref()
                        .map(MessageId::new)
                        .transpose()?,
                    thread_root: metadata.thread_root,
                })
            })
            .collect()
    }

    /// Build one body-free view for the authenticated workspace caller.
    /// Durable mailbox attention as `OpenDelivery` rows: one row per live
    /// attempt, from open `attention_required` alarms and from each
    /// recipient's pending head whose record is `attention_required` (an
    /// operator clearance only acknowledges), `quota_held`,
    /// `quota_reset_observed`, or `blocked_pre_write`. Held states project
    /// onto the two legacy words a human must act on, with the cause naming
    /// the real state. Identity is exact: rows are deduplicated by recipient
    /// key plus message id and by attempt id, and an attempt an operator has
    /// resolved is never a row. A pre-write-blocked head stays a row here
    /// even though `status` also details it under `blocked_notifications`,
    /// because the eye counts this array and a snapshot has no other; the
    /// surface that prints both dedups the detailed row by attempt id.
    ///
    /// The same rows ride `status` and `messages.snapshot`, read from this
    /// one projection, so every surface counts the same record.
    pub(crate) fn mailbox_attention_rows(
        &self,
        labels: &HashMap<RecipientKey, String>,
    ) -> Vec<cyclops_proto::OpenDelivery> {
        use cyclops_proto::DeliveryState;

        let label_for =
            |key: &RecipientKey| labels.get(key).cloned().unwrap_or_else(|| key.to_string());
        let row = |record: &NotificationRecord| {
            let (state, cause) = match record.state {
                NotificationState::QuotaHeld => (
                    DeliveryState::ParkedBlockedQuota,
                    Some("quota_held".to_string()),
                ),
                NotificationState::QuotaResetObserved => (
                    DeliveryState::ParkedBlockedQuota,
                    Some("quota_reset_observed".to_string()),
                ),
                NotificationState::BlockedPreWrite => {
                    // A named write block on the observation is the exact
                    // reason (for example hook_admission_unproven); the enum
                    // cause stays for readers that only know it.
                    let why = record
                        .pre_write_observation
                        .as_ref()
                        .and_then(|observation| observation.write_block.clone())
                        .or_else(|| {
                            record
                                .pre_write_cause
                                .map(|cause| cause.wire_name().to_string())
                        })
                        .or_else(|| record.wake_block.map(|block| block.wire_name().to_string()))
                        .unwrap_or_else(|| "unknown".to_string());
                    (
                        DeliveryState::AttentionRequired,
                        Some(cyclops_proto::delivery_pre_write_cause(&why)),
                    )
                }
                _ => (
                    DeliveryState::AttentionRequired,
                    record
                        .cause
                        .and_then(|cause| serde_json::to_value(cause).ok())
                        .and_then(|value| value.as_str().map(str::to_string))
                        .or_else(|| record.wake_block.map(|block| block.wire_name().to_string())),
                ),
            };
            cyclops_proto::OpenDelivery {
                id: record.message_id.to_string(),
                to: label_for(&record.recipient),
                recipient: Some(record.recipient),
                state,
                ts: record.updated_at,
                cause,
                attempt_id: Some(record.attempt_id),
            }
        };
        let mut seen: HashSet<(RecipientKey, MessageId)> = HashSet::new();
        let mut seen_attempts: HashSet<NotificationAttemptId> = HashSet::new();
        let mut rows = Vec::new();
        let mut push = |record: &NotificationRecord| {
            if !seen_attempts.insert(record.attempt_id) {
                return;
            }
            if seen.insert((record.recipient, record.message_id.clone())) {
                rows.push(row(record));
            }
        };
        for record in self.open_alarms() {
            push(record);
        }
        for recipient in self.mailboxes.keys() {
            let Some(head) = self.get_pending(*recipient).into_iter().next() else {
                continue;
            };
            let Some(record) = self
                .notifications
                .get(&(*recipient, head.message_id.clone()))
            else {
                continue;
            };
            if self.resolved_attempts.contains_key(&record.attempt_id) {
                continue;
            }
            if matches!(
                record.state,
                NotificationState::AttentionRequired
                    | NotificationState::QuotaHeld
                    | NotificationState::QuotaResetObserved
                    | NotificationState::BlockedPreWrite
            ) {
                push(record);
            }
        }
        rows
    }

    pub fn messages_snapshot(
        &self,
        caller: RecipientKey,
        recent_settled: u64,
        current_routes: &HashMap<RecipientKey, MessageRecipientRoute>,
    ) -> Result<MessagesSnapshotResult, MailboxError> {
        if caller.workspace_id() != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: caller.workspace_id(),
            });
        }

        let mut fifo_positions = HashMap::new();
        for (recipient, entries) in &self.mailboxes {
            let mut position = 0_u64;
            for entry in entries.values() {
                if entry.state.is_pending()
                    && !self.notification_withdrawn_by_operator(*recipient, &entry.message_id)
                {
                    position += 1;
                    fifo_positions.insert((*recipient, entry.message_id.clone()), position);
                }
            }
        }

        let mut visible = Vec::new();
        for line in self.messages.values() {
            let metadata = extract_message_metadata(line)?;
            if caller.is_admin()
                || metadata.sender == caller
                || metadata.recipients.contains(&caller)
            {
                visible.push((line, metadata));
            }
        }
        visible.sort_by_key(|(line, _)| line.seq);

        let mut thread_counts = HashMap::new();
        for (_, metadata) in &visible {
            *thread_counts
                .entry(metadata.thread_root.clone())
                .or_insert(0_u64) += 1;
        }

        let mut pending_entries = 0_u64;
        let mut claimed_entries = 0_u64;
        let mut open_attention_entries = 0_u64;
        let mut inbox_messages = 0_u64;
        let mut outbound_messages = 0_u64;
        let mut work_messages = 0_u64;
        let mut projected = Vec::with_capacity(visible.len());

        for (line, metadata) in visible {
            let message_id = MessageId::new(&line.id)?;
            let (_, recipient_labels) =
                presentation_labels(&metadata.recipients, &metadata.presentation)?;
            let mut active = false;
            let mut caller_pending = false;
            let mut recipients = Vec::with_capacity(metadata.recipients.len());

            for (index, recipient) in metadata.recipients.iter().copied().enumerate() {
                let entry = self.get_entry(recipient, &message_id).ok_or_else(|| {
                    MailboxError::EntryNotFound {
                        message_id: message_id.clone(),
                        recipient,
                    }
                })?;
                if entry.state.is_pending() {
                    pending_entries += 1;
                    active = true;
                    if recipient == caller {
                        caller_pending = true;
                    }
                } else if entry.state.is_claimed() {
                    claimed_entries += 1;
                }

                let record = self.notification(recipient, &message_id);
                let notification = self.notification_summary(record);
                if let Some(record) = record {
                    let attention_open = notification.attention_cleared == Some(false)
                        && notification.resolution.is_none();
                    let quota_open = matches!(
                        record.state,
                        NotificationState::QuotaHeld | NotificationState::QuotaResetObserved
                    );
                    if attention_open || quota_open {
                        open_attention_entries += 1;
                        active = true;
                    } else if !record.state.is_terminal() {
                        active = true;
                    }
                }

                let recipient_direction = match (metadata.sender == caller, recipient == caller) {
                    (true, true) => MessageDirection::SelfAddressed,
                    (true, false) => MessageDirection::Outbound,
                    (false, true) => MessageDirection::Inbound,
                    (false, false) => MessageDirection::Workspace,
                };
                let recipient_attention_open = notification.state
                    == MessageNotificationState::AttentionRequired
                    && notification.attention_cleared == Some(false)
                    && notification.resolution.is_none();
                let can_manage_attention = caller.is_admin()
                    && recipient_attention_open
                    && notification.resolution_intent.is_none();
                let recipient_quota_open = notification.quota_state.is_some();
                let recipient_pre_write_blocked = notification.pre_write_cause.is_some()
                    && notification.operator_withdrawn != Some(true);
                let can_withdraw_notification = caller.is_admin()
                    && entry.state.is_pending()
                    && record.is_some_and(|record| record.state.can_withdraw_before_write());
                let recipient_needs_action = (recipient == caller && entry.state.is_pending())
                    || (caller.is_admin()
                        && (recipient_attention_open
                            || recipient_quota_open
                            || recipient_pre_write_blocked));

                recipients.push(MessageRecipientSummary {
                    recipient,
                    label: recipient_labels[index].clone(),
                    direction: recipient_direction,
                    needs_action: recipient_needs_action,
                    can_manage_attention,
                    can_withdraw_notification,
                    current_route: current_routes.get(&recipient).cloned(),
                    available: recipient.is_admin() || current_routes.contains_key(&recipient),
                    mailbox: entry.state.clone(),
                    fifo_position: fifo_positions
                        .get(&(recipient, message_id.clone()))
                        .copied(),
                    notification,
                });
            }

            let sent = metadata.sender == caller;
            let received = metadata.recipients.contains(&caller);
            let direction = match (sent, received) {
                (true, true) => MessageDirection::SelfAddressed,
                (true, false) => MessageDirection::Outbound,
                (false, true) => MessageDirection::Inbound,
                (false, false) => MessageDirection::Workspace,
            };
            if sent {
                outbound_messages += 1;
            }
            if received {
                inbox_messages += 1;
            }
            // Withdrawal authority is broader than human work. Queued and
            // ordinary gating attempts remain visible in All without entering
            // Work solely because an administrator could suppress them.
            let row_has_admin_work = recipients.iter().any(|recipient| {
                recipient.can_manage_attention || recipient.notification.pre_write_cause.is_some()
            });
            let needs_action = caller_pending || (caller.is_admin() && row_has_admin_work);
            if needs_action {
                work_messages += 1;
            }
            let row = MessageSnapshotRow {
                message_id,
                seq: line.seq,
                ts: line.ts,
                kind: line.kind,
                direction,
                sender: metadata.sender,
                sender_label: metadata.presentation.sender_label,
                recipients,
                subject: line.subject.clone(),
                reply_to: line.reply_to.as_deref().map(MessageId::new).transpose()?,
                thread_root: metadata.thread_root.clone(),
                thread_message_count: thread_counts[&metadata.thread_root],
                active,
                needs_action,
            };
            projected.push((active, row));
        }

        let active_messages = projected.iter().filter(|(active, _)| *active).count() as u64;
        let settled_messages = projected.len() as u64 - active_messages;
        let settled_to_skip = settled_messages.saturating_sub(recent_settled);
        let mut settled_seen = 0_u64;
        let rows: Vec<_> = projected
            .into_iter()
            .filter_map(|(active, row)| {
                if active {
                    return Some(row);
                }
                let keep = settled_seen >= settled_to_skip;
                settled_seen += 1;
                keep.then_some(row)
            })
            .collect();
        let counts = MessagesSnapshotCounts {
            visible_messages: active_messages + settled_messages,
            returned_messages: rows.len() as u64,
            inbox_messages,
            outbound_messages,
            work_messages,
            active_messages,
            settled_messages,
            pending_entries,
            claimed_entries,
            open_attention_entries,
        };

        let labels: HashMap<RecipientKey, String> = current_routes
            .iter()
            .map(|(key, route)| (*key, route.label.clone()))
            .collect();
        let mailbox_attention = self.mailbox_attention_rows(&labels);

        Ok(MessagesSnapshotResult {
            workspace_id: self.workspace_id,
            caller: Some(caller),
            workspace_seq: self.last_workspace_seq.unwrap_or(0),
            counts,
            rows,
            mailbox_attention,
        })
    }

    /// Return one bounded page of every visible message created after a
    /// durable cursor. Settled rows are never trimmed on this path.
    pub fn messages_follow(
        &self,
        caller: RecipientKey,
        after_seq: u64,
        limit: u32,
        current_routes: &HashMap<RecipientKey, MessageRecipientRoute>,
    ) -> Result<MessagesFollowResult, MailboxError> {
        let snapshot = self.messages_snapshot(caller, u64::MAX, current_routes)?;
        if after_seq > snapshot.workspace_seq {
            return Err(MailboxError::InvalidCursor {
                cursor: after_seq,
                head: snapshot.workspace_seq,
            });
        }
        let mut rows: Vec<_> = snapshot
            .rows
            .into_iter()
            .filter(|row| row.seq > after_seq)
            .take(limit as usize + 1)
            .collect();
        let has_more = rows.len() > limit as usize;
        if has_more {
            rows.pop();
        }
        let through_seq = if has_more {
            rows.last().map_or(after_seq, |row| row.seq)
        } else {
            snapshot.workspace_seq
        };
        Ok(MessagesFollowResult {
            workspace_id: self.workspace_id,
            after_seq,
            through_seq,
            has_more,
            rows,
        })
    }

    /// Build the bounded body-free pre-write failure sample used by status.
    fn blocked_notification_snapshot(
        &self,
        caller: RecipientKey,
        current_routes: &HashMap<RecipientKey, MessageRecipientRoute>,
        now: u64,
        limit: usize,
    ) -> Result<BlockedNotificationSnapshot, MailboxError> {
        if caller.workspace_id() != self.workspace_id {
            return Err(MailboxError::WorkspaceMismatch {
                expected: self.workspace_id,
                found: caller.workspace_id(),
            });
        }

        // Keep only the oldest requested records while scanning the
        // notification index once. Unrelated messages never enter this path.
        let mut selected = BTreeMap::new();
        let mut total = 0_u64;
        for record in self.notifications.values().filter(|record| {
            record.state == NotificationState::BlockedPreWrite
                && record.pre_write_cause.is_some()
                && self.notification_settlement(record).is_none()
                && !self.resolved_attempts.contains_key(&record.attempt_id)
        }) {
            total = total.saturating_add(1);
            if limit == 0 {
                continue;
            }
            let line = self
                .get_message(&record.message_id)
                .ok_or_else(|| MailboxError::MessageNotFound(record.message_id.clone()))?;
            let metadata = extract_message_metadata(line)?;
            let recipient_index = metadata
                .recipients
                .iter()
                .position(|recipient| *recipient == record.recipient)
                .ok_or_else(|| MailboxError::EntryNotFound {
                    message_id: record.message_id.clone(),
                    recipient: record.recipient,
                })?;
            selected.insert(
                (line.seq, recipient_index, record.attempt_id),
                record.clone(),
            );
            if selected.len() > limit {
                selected.pop_last();
            }
        }

        let mut selected_by_recipient: HashMap<RecipientKey, HashSet<MessageId>> = HashMap::new();
        for record in selected.values() {
            selected_by_recipient
                .entry(record.recipient)
                .or_default()
                .insert(record.message_id.clone());
        }
        let mut fifo_positions = HashMap::new();
        for (recipient, selected_messages) in selected_by_recipient {
            let Some(entries) = self.mailboxes.get(&recipient) else {
                continue;
            };
            let mut position = 0_u64;
            for entry in entries.values() {
                if !entry.state.is_pending()
                    || self.notification_withdrawn_by_operator(recipient, &entry.message_id)
                {
                    continue;
                }
                position += 1;
                if selected_messages.contains(&entry.message_id) {
                    fifo_positions.insert((recipient, entry.message_id.clone()), position);
                }
            }
        }

        let mut rows = Vec::with_capacity(selected.len());
        for record in selected.into_values() {
            let line = self
                .get_message(&record.message_id)
                .ok_or_else(|| MailboxError::MessageNotFound(record.message_id.clone()))?;
            let metadata = extract_message_metadata(line)?;
            let (_, recipient_labels) =
                presentation_labels(&metadata.recipients, &metadata.presentation)?;
            let recipient_index = metadata
                .recipients
                .iter()
                .position(|recipient| *recipient == record.recipient)
                .ok_or_else(|| MailboxError::EntryNotFound {
                    message_id: record.message_id.clone(),
                    recipient: record.recipient,
                })?;
            let entry = self
                .get_entry(record.recipient, &record.message_id)
                .ok_or_else(|| MailboxError::EntryNotFound {
                    message_id: record.message_id.clone(),
                    recipient: record.recipient,
                })?;
            let notification = self.notification_summary(Some(&record));
            let recipient_attention_open = notification.state
                == MessageNotificationState::AttentionRequired
                && notification.attention_cleared == Some(false)
                && notification.resolution.is_none();
            let can_manage_attention = caller.is_admin()
                && recipient_attention_open
                && notification.resolution_intent.is_none();
            let recipient_quota_open = notification.quota_state.is_some();
            let recipient_pre_write_blocked = notification.pre_write_cause.is_some()
                && notification.operator_withdrawn != Some(true);
            let can_withdraw_notification = caller.is_admin()
                && entry.state.is_pending()
                && record.state.can_withdraw_before_write();
            let direction = match (metadata.sender == caller, record.recipient == caller) {
                (true, true) => MessageDirection::SelfAddressed,
                (true, false) => MessageDirection::Outbound,
                (false, true) => MessageDirection::Inbound,
                (false, false) => MessageDirection::Workspace,
            };
            let recipient = MessageRecipientSummary {
                recipient: record.recipient,
                label: recipient_labels[recipient_index].clone(),
                direction,
                needs_action: (record.recipient == caller && entry.state.is_pending())
                    || (caller.is_admin()
                        && (recipient_attention_open
                            || recipient_quota_open
                            || recipient_pre_write_blocked)),
                can_manage_attention,
                can_withdraw_notification,
                current_route: current_routes.get(&record.recipient).cloned(),
                available: record.recipient.is_admin()
                    || current_routes.contains_key(&record.recipient),
                mailbox: entry.state.clone(),
                fifo_position: fifo_positions
                    .get(&(record.recipient, record.message_id.clone()))
                    .copied(),
                notification,
            };
            rows.push(StatusBlockedNotification {
                message_id: record.message_id,
                notification_attempt: record.attempt_id,
                waiting_age_ms: now.saturating_sub(record.updated_at),
                next_action: can_withdraw_notification
                    .then_some(StatusNextAction::WithdrawNotification),
                recipient,
            });
        }

        Ok(BlockedNotificationSnapshot { rows, total })
    }

    /// Lookup an existing message by sender and client idempotency key.
    pub fn find_by_idempotency(&self, sender: RecipientKey, key: &str) -> Option<&LedgerLine> {
        self.idempotency_index
            .get(&(sender, key.to_string()))
            .and_then(|(id, _)| self.get_message(id))
    }

    fn derive_reply(
        &self,
        sender: RecipientKey,
        reference: &MessageId,
    ) -> Result<ReplyDerivation, MailboxError> {
        let parent = self
            .messages
            .get(reference)
            .ok_or_else(|| MailboxError::MessageNotFound(reference.clone()))?;
        let parent_metadata = extract_message_metadata(parent)?;
        let visible =
            parent_metadata.sender == sender || parent_metadata.recipients.contains(&sender);
        if !visible {
            return Err(MailboxError::ReplyNotVisible {
                reply_to: reference.clone(),
                sender,
            });
        }
        Ok(ReplyDerivation {
            recipient: parent_metadata.sender,
            thread_root: parent_metadata.thread_root,
            subject: reply_subject(parent.subject.as_deref()),
        })
    }

    fn supersession_thread_root(
        &self,
        sender: RecipientKey,
        recipients: &[RecipientKey],
        supersedes: Option<&MessageId>,
    ) -> Result<Option<MessageId>, MailboxError> {
        let Some(target) = supersedes else {
            return Ok(None);
        };
        if recipients.len() != 1 {
            return Err(MailboxError::SupersessionRequiresSingleRecipient);
        }
        let message = self
            .messages
            .get(target)
            .ok_or_else(|| MailboxError::MessageNotFound(target.clone()))?;
        let metadata = extract_message_metadata(message)?;
        if metadata.sender != sender || metadata.recipients != recipients {
            return Err(MailboxError::SupersessionIdentityMismatch(target.clone()));
        }
        let entry =
            self.get_entry(recipients[0], target)
                .ok_or_else(|| MailboxError::EntryNotFound {
                    message_id: target.clone(),
                    recipient: recipients[0],
                })?;
        if !entry.state.is_pending() {
            return Err(MailboxError::SupersessionNotPending(target.clone()));
        }
        if !self.supersession_notification_is_pre_write(recipients[0], target) {
            return Err(MailboxError::SupersessionNotificationStarted(
                target.clone(),
            ));
        }
        Ok(Some(metadata.thread_root))
    }

    fn supersession_notification_is_pre_write(
        &self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> bool {
        self.notifications
            .get(&(recipient, message_id.clone()))
            .is_none_or(|record| {
                matches!(
                    record.state,
                    NotificationState::Queued
                        | NotificationState::Gating
                        | NotificationState::BlockedPreWrite
                        | NotificationState::QuotaHeld
                        | NotificationState::QuotaResetObserved
                        | NotificationState::WithdrawnByOperator
                )
            })
    }
}

fn extract_message_metadata(line: &LedgerLine) -> Result<MessageMetadata, MailboxError> {
    line.data
        .as_ref()
        .and_then(|d| serde_json::from_value::<MessageMetadata>(d.clone()).ok())
        .ok_or_else(|| MailboxError::MissingMetadata(line.id.clone()))
}

fn inbox_message(line: &LedgerLine, claimant: RecipientKey) -> Result<InboxMessage, MailboxError> {
    let metadata = extract_message_metadata(line)?;
    let recipient_label = metadata
        .presentation
        .recipient_labels
        .iter()
        .find(|presentation| presentation.recipient == claimant)
        .map(|presentation| presentation.label.clone());
    Ok(InboxMessage {
        message_id: MessageId::new(&line.id)?,
        kind: line.kind,
        recipient_label,
        sender: Some(metadata.sender),
        sender_label: metadata.presentation.sender_label,
        subject: line.subject.clone(),
        summary: metadata.summary,
        body: line.body.clone(),
        reply_to: line.reply_to.as_deref().map(MessageId::new).transpose()?,
        thread_root: metadata.thread_root,
    })
}

fn presentation_labels(
    recipients: &[RecipientKey],
    presentation: &MessagePresentation,
) -> Result<(String, Vec<String>), MailboxError> {
    validate_display_label("sender", &presentation.sender_label)?;
    if presentation.recipient_labels.len() != recipients.len() {
        return Err(MailboxError::InvalidPresentation(format!(
            "expected {} recipient labels, found {}",
            recipients.len(),
            presentation.recipient_labels.len()
        )));
    }
    let mut labels = Vec::with_capacity(recipients.len());
    for (index, (expected, snapshot)) in recipients
        .iter()
        .zip(&presentation.recipient_labels)
        .enumerate()
    {
        if snapshot.recipient != *expected {
            return Err(MailboxError::InvalidPresentation(format!(
                "recipient label {index} is bound to '{}', expected '{}'",
                snapshot.recipient, expected
            )));
        }
        validate_display_label("recipient", &snapshot.label)?;
        labels.push(snapshot.label.clone());
    }
    Ok((presentation.sender_label.clone(), labels))
}

fn reply_subject(parent: Option<&str>) -> Option<String> {
    parent.map(|subject| {
        if subject.starts_with("Re: ") {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        }
    })
}

fn validate_display_label(kind: &str, label: &str) -> Result<(), MailboxError> {
    if label.is_empty() || label.chars().any(char::is_control) {
        return Err(MailboxError::InvalidPresentation(format!(
            "{kind} label must be non-empty and contain no control characters"
        )));
    }
    Ok(())
}

/// Durable owner of one workspace journal and its in-memory projection.
pub struct MessageStore {
    writer: LedgerWriter,
    projection: MailboxProjection,
    fail_notification_recovery_append: Option<NotificationAttemptId>,
    #[cfg(test)]
    fail_batch_append: bool,
    #[cfg(test)]
    fail_claimed_staged_clear_appends: usize,
    #[cfg(test)]
    fail_claimed_ack_timeout_reconciliation_appends: usize,
    #[cfg(test)]
    fail_pre_write_block_appends: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MessageStoreError {
    #[error(transparent)]
    Mailbox(Box<MailboxError>),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("workspace message metadata serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl From<MailboxError> for MessageStoreError {
    fn from(error: MailboxError) -> Self {
        Self::Mailbox(Box::new(error))
    }
}

pub struct MailboxService {
    workspace_id: WorkspaceId,
    directory: RwLock<MailboxDirectory>,
    store: Arc<StdMutex<MessageStore>>,
    changes: Option<MessageChangePublisher>,
    resolving_attention: StdMutex<HashSet<NotificationAttemptId>>,
    /// Private handoff for work that lost the exact in-memory resolution
    /// reservation. This is not a durable message event: a resolver may give
    /// up before appending anything.
    attention_resolution_releases: broadcast::Sender<NotificationAttemptId>,
    exact_reconciliation: StdMutex<ExactReconciliationRequests>,
    attention_consumption_candidates:
        StdMutex<HashMap<NotificationAttemptId, AttentionConsumptionCandidate>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnclaimedReminderQueue {
    Queued(Box<NotificationRecord>),
    WaitingForPriorBarrier,
    Obsolete,
}

#[derive(Default)]
struct ExactReconciliationRequests {
    running: HashSet<NotificationAttemptId>,
    dirty: HashSet<NotificationAttemptId>,
}

/// One exact causal observation waiting to become a durable consumption fact.
pub(crate) struct AttentionConsumptionSignal {
    observation: StdMutex<Option<NotificationResolutionConsumptionObservation>>,
}

impl AttentionConsumptionSignal {
    fn new() -> Self {
        Self {
            observation: StdMutex::new(None),
        }
    }

    fn confirm(&self, observation: NotificationResolutionConsumptionObservation) -> bool {
        let Ok(mut current) = self.observation.lock() else {
            return false;
        };
        if current.is_some() {
            return false;
        }
        *current = Some(observation);
        true
    }

    pub(crate) fn observation(&self) -> Option<NotificationResolutionConsumptionObservation> {
        self.observation.lock().ok().and_then(|value| *value)
    }
}

struct AttentionConsumptionCandidate {
    message_id: MessageId,
    recipient: RecipientKey,
    session_idx: usize,
    pane_id: String,
    pane_root: ProcessInstanceId,
    agent: ProcessInstanceId,
    manifest: String,
    expected_payload: String,
    not_before_ms: u64,
    signal: Arc<AttentionConsumptionSignal>,
}

/// Publishes committed workspace changes while the store lock still orders them.
#[derive(Clone)]
pub(crate) struct MessageChangePublisher {
    workspace_id: WorkspaceId,
    events: broadcast::Sender<Event>,
}

impl MessageChangePublisher {
    fn new(workspace_id: WorkspaceId, events: broadcast::Sender<Event>) -> Self {
        Self {
            workspace_id,
            events,
        }
    }

    /// Call while holding the workspace store lock that ordered this append.
    pub(crate) fn publish(&self, workspace_seq: u64, changed: &[MessagesChangedArea]) {
        debug_assert!(!changed.is_empty());
        let data = MessagesChangedData {
            workspace_id: self.workspace_id,
            workspace_seq,
            changed: changed.iter().copied().collect::<BTreeSet<_>>(),
        };
        let _ = self.events.send(Event {
            event: "messages.changed".into(),
            data: serde_json::to_value(data).expect("messages.changed data serializes"),
            seq: Some(workspace_seq),
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MailboxServiceError {
    #[error(transparent)]
    Directory(#[from] MailboxDirectoryError),
    #[error(transparent)]
    Store(#[from] MessageStoreError),
    #[error("mailbox store lock is poisoned")]
    Poisoned,
    #[error("replacement mailbox directory belongs to another workspace")]
    ForeignDirectory,
    #[error("notification scheduler state could not be recorded: {0}")]
    NotificationSchedule(String),
}

impl From<MailboxError> for MailboxServiceError {
    fn from(error: MailboxError) -> Self {
        Self::Store(MessageStoreError::from(error))
    }
}

impl MailboxServiceError {
    pub(crate) fn notification_resolution_in_progress(&self) -> bool {
        matches!(
            self,
            Self::Store(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionInProgress(_))
        )
    }
}

impl MailboxService {
    pub fn new(directory: MailboxDirectory, store: MessageStore) -> Self {
        Self::new_inner(directory, store, None)
    }

    pub(crate) fn new_with_events(
        directory: MailboxDirectory,
        store: MessageStore,
        events: broadcast::Sender<Event>,
    ) -> Self {
        let workspace_id = store.projection().workspace_id();
        debug_assert_eq!(directory.workspace_id(), workspace_id);
        Self::new_inner(
            directory,
            store,
            Some(MessageChangePublisher::new(workspace_id, events)),
        )
    }

    fn new_inner(
        directory: MailboxDirectory,
        store: MessageStore,
        changes: Option<MessageChangePublisher>,
    ) -> Self {
        // One release edge is enough to prompt one blocked force-submit task
        // to revalidate. The stream is shared, so lag is not proof that this
        // attempt was released.
        let (attention_resolution_releases, _) = broadcast::channel(1);
        Self {
            workspace_id: directory.workspace_id(),
            directory: RwLock::new(directory),
            store: Arc::new(StdMutex::new(store)),
            changes,
            resolving_attention: StdMutex::new(HashSet::new()),
            attention_resolution_releases,
            exact_reconciliation: StdMutex::new(ExactReconciliationRequests::default()),
            attention_consumption_candidates: StdMutex::new(HashMap::new()),
        }
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) fn seal(&self) -> Result<(), MailboxServiceError> {
        self.store()?
            .writer
            .seal()
            .map_err(MessageStoreError::from)?;
        Ok(())
    }

    pub fn admin(&self) -> MailboxIdentity {
        MailboxIdentity {
            key: RecipientKey::admin(self.workspace_id),
            label: "admin".to_string(),
        }
    }

    pub fn agent_for_pane(
        &self,
        pane: TmuxPaneId,
    ) -> Result<Option<MailboxIdentity>, MailboxServiceError> {
        Ok(self.directory()?.agent_for_pane(pane))
    }

    pub fn identity_for_recipient(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<MailboxIdentity>, MailboxServiceError> {
        Ok(self.directory()?.identity_for_recipient(recipient))
    }

    pub fn identity_for_address(
        &self,
        address: &str,
    ) -> Result<MailboxIdentity, MailboxServiceError> {
        if address == "*" {
            return Err(MailboxDirectoryError::UnknownRecipient(address.to_string()).into());
        }
        let mut resolved = self.directory()?.resolve(&[address.to_string()])?;
        Ok(resolved.remove(0))
    }

    pub fn routes(&self) -> Result<Vec<MailboxIdentity>, MailboxServiceError> {
        Ok(self.directory()?.routes())
    }

    pub fn replace_directory(
        &self,
        directory: MailboxDirectory,
    ) -> Result<(), MailboxServiceError> {
        if directory.workspace_id() != self.workspace_id {
            return Err(MailboxServiceError::ForeignDirectory);
        }
        *self
            .directory
            .write()
            .map_err(|_| MailboxServiceError::Poisoned)? = directory;
        Ok(())
    }

    pub fn send(
        &self,
        sender: MailboxIdentity,
        request: MailboxSend,
    ) -> Result<AcceptResult, MailboxServiceError> {
        self.send_after_resolution(sender, request, || {})
    }

    fn send_after_resolution(
        &self,
        sender: MailboxIdentity,
        request: MailboxSend,
        after_resolution: impl FnOnce(),
    ) -> Result<AcceptResult, MailboxServiceError> {
        let supersedes = request.supersedes.clone();
        // Keep this read guard through the append. A route replacement cannot
        // invalidate an exact recipient after validation but before acceptance.
        let directory = self.directory()?;
        let recipients = match request.recipient_keys.as_deref() {
            Some(_) if !request.addresses.is_empty() => {
                return Err(MailboxDirectoryError::MixedRecipientSelectors.into());
            }
            Some(recipient_keys) => directory.resolve_recipient_keys(recipient_keys)?,
            None => directory.resolve(&request.addresses)?,
        };
        after_resolution();
        let draft = MessageDraft {
            kind: if request.fyi { Kind::Fyi } else { Kind::Msg },
            sender: sender.key,
            recipients: recipients.iter().map(|identity| identity.key).collect(),
            subject: Some(request.subject),
            summary: request.summary,
            body: (!request.body.is_empty()).then_some(request.body),
            client_key: request.client_key,
            supersedes: request.supersedes,
            presentation: MessagePresentation {
                sender_label: sender.label,
                recipient_labels: recipients
                    .into_iter()
                    .map(|identity| cyclops_proto::RecipientPresentation {
                        recipient: identity.key,
                        label: identity.label,
                    })
                    .collect(),
            },
        };
        let mut store = self.store()?;
        let withdrew_notification = supersedes.as_ref().is_some_and(|message_id| {
            draft.recipients.first().is_some_and(|recipient| {
                store
                    .projection()
                    .notification(*recipient, message_id)
                    .is_some()
            })
        });
        let accepted = store.accept(mint_message_id(), draft)?;
        if accepted.inserted {
            let changed = if withdrew_notification {
                &[
                    MessagesChangedArea::Messages,
                    MessagesChangedArea::Mailboxes,
                    MessagesChangedArea::Notifications,
                ][..]
            } else {
                &[
                    MessagesChangedArea::Messages,
                    MessagesChangedArea::Mailboxes,
                ][..]
            };
            self.publish_change(accepted.seq, changed);
        }
        Ok(accepted)
    }

    pub fn list(
        &self,
        recipient: RecipientKey,
        sender: Option<RecipientKey>,
        limit: Option<u32>,
    ) -> Result<Vec<MailboxListItem>, MailboxServiceError> {
        let mut entries = self
            .store()?
            .projection()
            .list_mailbox(recipient)
            .map_err(MessageStoreError::from)?;
        if let Some(sender) = sender {
            entries.retain(|entry| entry.sender == sender);
        }
        if let Some(limit) = limit {
            entries.truncate(limit as usize);
        }
        Ok(entries)
    }

    pub fn pending_count(&self, recipient: RecipientKey) -> Result<usize, MailboxServiceError> {
        Ok(self.store()?.projection().pending_count(recipient))
    }

    pub fn recipient_label(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<String>, MailboxServiceError> {
        Ok(self.store()?.projection().recipient_label(recipient))
    }

    pub(crate) fn pending_recipients(&self) -> Result<Vec<RecipientKey>, MailboxServiceError> {
        let store = self.store()?;
        let mut recipients: Vec<_> = store
            .projection()
            .mailboxes
            .iter()
            .filter_map(|(recipient, entries)| {
                (entries.values().any(|entry| entry.state.is_pending())
                    || store
                        .projection()
                        .claimed_operator_notification(*recipient)
                        .is_some()
                    || store
                        .projection()
                        .claimed_notification_barrier(*recipient)
                        .is_some())
                .then_some(*recipient)
            })
            .collect();
        recipients.sort();
        Ok(recipients)
    }

    /// The notification queue skips an operator-withdrawn wake while keeping
    /// its mailbox entry pending and pullable. Every notification scheduler
    /// path must make that same choice, otherwise a withdrawn head can hide a
    /// later blocked attempt forever.
    fn first_actionable_pending_message_id(
        store: &MessageStore,
        recipient: RecipientKey,
    ) -> Option<MessageId> {
        store
            .projection()
            .get_pending(recipient)
            .iter()
            .find(|entry| {
                !store
                    .projection()
                    .notification_withdrawn_by_operator(recipient, &entry.message_id)
            })
            .map(|entry| entry.message_id.clone())
    }

    /// Oldest mailbox entry whose pane doorbell has not reached a terminal
    /// result. A socket claim retrieves the body, but does not cancel its
    /// separate human-visible notification obligation.
    fn first_actionable_notification_message_id(
        store: &MessageStore,
        recipient: RecipientKey,
    ) -> Option<MessageId> {
        store
            .projection()
            .mailboxes
            .get(&recipient)?
            .values()
            .find(|entry| {
                if entry.state.is_pending() {
                    return !store
                        .projection()
                        .notification_withdrawn_by_operator(recipient, &entry.message_id);
                }
                if !entry.state.is_claimed() {
                    return false;
                }
                store
                    .projection()
                    .notification(recipient, &entry.message_id)
                    .is_none_or(|record| {
                        matches!(
                            record.state,
                            NotificationState::Queued
                                | NotificationState::Gating
                                | NotificationState::Writing
                                | NotificationState::Staged
                                | NotificationState::Submitting
                                | NotificationState::Submitted
                                | NotificationState::SubmittedUnverified
                        )
                    })
            })
            .map(|entry| entry.message_id.clone())
    }

    /// Queue or resume the oldest unnotified doorbell for one recipient.
    ///
    /// This method owns the atomic mailbox decision so concurrent sends
    /// cannot mint two attempts. The scheduler binds or explicitly blocks
    /// the returned attempt before reporting its wake outcome.
    pub(crate) fn prepare_oldest_notification(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<NotificationRecord>, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(None);
        }
        let mut store = self.store()?;
        if let Some(record) = store
            .projection()
            .claimed_notification_barrier(recipient)
            .cloned()
        {
            // A durable resolution chain owns this exact barrier until it is
            // reconciled or explicitly withdrawn. Starting the automatic
            // claimed-barrier path here could settle the notification first
            // and leave the operator chain permanently incomplete.
            if store
                .projection()
                .attention_resolution_pending(record.attempt_id)
            {
                return Ok(None);
            }
            return Ok(Some(record));
        }
        if let Some(record) = store
            .projection()
            .claimed_operator_notification(recipient)
            .cloned()
        {
            return Ok(matches!(
                record.state,
                NotificationState::Queued | NotificationState::Gating
            )
            .then_some(record));
        }
        loop {
            let Some(message_id) =
                Self::first_actionable_notification_message_id(&store, recipient)
            else {
                return Ok(None);
            };
            match store
                .projection()
                .notification(recipient, &message_id)
                .cloned()
            {
                None => {
                    let record = store.queue_notification(
                        message_id,
                        recipient,
                        NotificationAttemptId::generate(),
                    )?;
                    self.publish_change(record.updated_seq, &[MessagesChangedArea::Notifications]);
                    return Ok(Some(record));
                }
                Some(record)
                    if matches!(
                        record.state,
                        NotificationState::Queued | NotificationState::Gating
                    ) =>
                {
                    return Ok(Some(record));
                }
                Some(record)
                    if record.state == NotificationState::Notified
                        && record.transport == NotificationTransport::DirectPayload =>
                {
                    store.mark_delivered_direct(message_id, recipient, record.attempt_id)?;
                    let seq = store
                        .projection()
                        .last_sequence()
                        .expect("direct restart repair appends a mailbox fact");
                    self.publish_change(
                        seq,
                        &[
                            MessagesChangedArea::Messages,
                            MessagesChangedArea::Mailboxes,
                        ],
                    );
                }
                Some(_) => return Ok(None),
            }
        }
    }

    /// Explain a `prepare_oldest_notification` miss without inventing success.
    ///
    /// A normal empty/admin mailbox and an already visible or in-flight head
    /// need no scheduler warning. Durable attention and pre-write holds do.
    pub(crate) fn notification_schedule_block(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<NotificationScheduleBlock>, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(None);
        }
        let store = self.store()?;
        if let Some(record) = store.projection().claimed_notification_barrier(recipient) {
            if store
                .projection()
                .attention_resolution_pending(record.attempt_id)
            {
                return Ok(Some(NotificationScheduleBlock {
                    message_id: record.message_id.clone(),
                    attempt_id: record.attempt_id,
                    block: MessageWakeBlock::AttentionResolutionPending,
                }));
            }
        }
        let Some(message_id) = Self::first_actionable_pending_message_id(&store, recipient) else {
            return Ok(None);
        };
        let Some(record) = store.projection().notification(recipient, &message_id) else {
            return Ok(None);
        };
        let block = notification_wake_block(
            record,
            store
                .projection()
                .attention_resolution_pending(record.attempt_id),
        );
        Ok(block.map(|block| NotificationScheduleBlock {
            message_id,
            attempt_id: record.attempt_id,
            block,
        }))
    }

    /// Whether the oldest pending wake is blocked only by its recorded width.
    pub(crate) fn oldest_notification_has_width_block(
        &self,
        recipient: RecipientKey,
    ) -> Result<bool, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(false);
        }
        let store = self.store()?;
        let Some(message_id) = Self::first_actionable_pending_message_id(&store, recipient) else {
            return Ok(false);
        };
        Ok(store
            .projection()
            .notification(recipient, &message_id)
            .and_then(notification_pre_write_width_block)
            .is_some())
    }

    /// Reopen the oldest blocked attempt after exact route evidence changes.
    ///
    /// Ordinary mailbox changes never call this operation. The caller must
    /// supply one complete, live binding observation from the route that will
    /// receive the write. A readiness block additionally requires a positive
    /// current write-ready verdict. Repeated observations are no-ops, and the
    /// attempt id is retained so a route change cannot become an implicit
    /// requeue.
    pub(crate) fn reopen_oldest_notification_after_route_evidence(
        &self,
        recipient: RecipientKey,
        observation: NotificationPreWriteObservation,
        write_ready: bool,
    ) -> Result<Option<NotificationRecord>, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(None);
        }
        let mut store = self.store()?;
        let Some(message_id) = Self::first_actionable_pending_message_id(&store, recipient) else {
            return Ok(None);
        };
        let Some(current) = store
            .projection()
            .notification(recipient, &message_id)
            .cloned()
        else {
            return Ok(None);
        };
        let complete_binding = observation.binding.as_ref().is_some_and(|binding| {
            observation.pane_root.is_some()
                && binding.pane_root.is_some()
                && binding.leader.is_some()
                && observation.pane_root == binding.pane_root
                && binding.recipient == recipient
                && observation.selected_manifest.as_ref() == Some(&binding.manifest)
        });
        let cause_changed = match current.pre_write_cause {
            Some(
                NotificationPreWriteCause::SessionUnavailable
                | NotificationPreWriteCause::WorkerFailed,
            ) => complete_binding,
            Some(NotificationPreWriteCause::BindingUnprovable) => {
                current.pre_write_observation.as_ref().is_none_or(|prior| {
                    match (&prior.route_evidence, &observation.route_evidence) {
                        (Some(prior), Some(current)) => route_evidence_is_later(prior, current),
                        (None, Some(_)) => true,
                        (Some(_), None) => false,
                        (None, None) => {
                            prior.pane_root != observation.pane_root
                                || prior.selected_manifest != observation.selected_manifest
                                || prior.binding != observation.binding
                        }
                    }
                })
            }
            Some(NotificationPreWriteCause::WriteReadinessChanged) => {
                match notification_pre_write_width_block(&current) {
                    Some((observed, required)) => observation
                        .pane_width
                        .is_some_and(|width| width >= required && width != observed),
                    None => {
                        let later_route_evidence = current
                            .pre_write_observation
                            .as_ref()
                            .and_then(|prior| prior.route_evidence.as_ref())
                            .zip(observation.route_evidence.as_ref())
                            .is_some_and(|(prior, current)| {
                                route_evidence_is_later(prior, current)
                            });
                        write_ready && later_route_evidence
                    }
                }
            }
            _ => false,
        };
        if current.state != NotificationState::BlockedPreWrite
            || !complete_binding
            || !cause_changed
            || current.pre_write_reopen_count >= 1
        {
            return Ok(None);
        }
        let record = store.advance_notification(
            message_id,
            recipient,
            current.attempt_id,
            NotificationState::Gating,
            None,
            None,
        )?;
        self.publish_change(record.updated_seq, &[MessagesChangedArea::Notifications]);
        Ok(Some(record))
    }

    pub(crate) fn message_dispositions(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<MessageDisposition>, MailboxServiceError> {
        let store = self.store()?;
        let message = store.projection().get_message(message_id).ok_or_else(|| {
            MessageStoreError::from(MailboxError::MessageNotFound(message_id.clone()))
        })?;
        let metadata = extract_message_metadata(message).map_err(MessageStoreError::from)?;
        let (_, labels) = presentation_labels(&metadata.recipients, &metadata.presentation)
            .map_err(MessageStoreError::from)?;
        Ok(metadata
            .recipients
            .iter()
            .copied()
            .zip(labels)
            .map(|(recipient, label)| {
                let pending = store.projection().get_pending(recipient);
                let position_ahead = pending
                    .iter()
                    .filter(|entry| {
                        !store
                            .projection()
                            .notification_withdrawn_by_operator(recipient, &entry.message_id)
                    })
                    .position(|entry| &entry.message_id == message_id)
                    .and_then(|position| u32::try_from(position).ok());
                let record = store.projection().notification(recipient, message_id);
                let notification_state_raw = record.map(|record| record.state);
                let notification_state = record
                    .map(|record| record.state.into())
                    .unwrap_or(MessageNotificationState::NotStarted);
                let quota_state =
                    record.and_then(|record| MessageQuotaState::from_notification(record.state));
                let notification_settlement =
                    record.and_then(|record| store.projection().notification_settlement(record));
                MessageDisposition {
                    recipient,
                    label,
                    attempt_id: record.map(|record| record.attempt_id),
                    notification_state_raw,
                    notification_state,
                    quota_state,
                    notification_settlement,
                    pre_write_cause: record.and_then(|record| record.pre_write_cause),
                    wake_block: record.and_then(|record| {
                        notification_wake_block(
                            record,
                            store
                                .projection()
                                .attention_resolution_pending(record.attempt_id),
                        )
                    }),
                    position_ahead,
                }
            })
            .collect())
    }

    /// Return one canonical message row for internal delivery verification.
    pub(crate) fn message_line(
        &self,
        message_id: &MessageId,
    ) -> Result<LedgerLine, MailboxServiceError> {
        self.store()?
            .projection()
            .get_message(message_id)
            .cloned()
            .ok_or_else(|| MailboxError::MessageNotFound(message_id.clone()).into())
    }

    pub(crate) fn store_handle(&self) -> Arc<StdMutex<MessageStore>> {
        Arc::clone(&self.store)
    }

    #[doc(hidden)]
    pub(crate) fn inject_notification_recovery_append_failure(
        &self,
        attempt_id: NotificationAttemptId,
    ) {
        if let Ok(mut store) = self.store.lock() {
            store.inject_notification_recovery_append_failure(attempt_id);
        }
    }

    pub(crate) fn change_publisher(&self) -> Option<MessageChangePublisher> {
        self.changes.clone()
    }

    pub fn messages_snapshot(
        &self,
        caller: RecipientKey,
        recent_settled: u32,
    ) -> Result<MessagesSnapshotResult, MailboxServiceError> {
        // Availability is a current route observation. It is captured for
        // this answer but is not covered by the workspace journal sequence.
        let current_routes = self.directory()?.current_routes();
        self.store()?
            .projection()
            .messages_snapshot(caller, recent_settled.into(), &current_routes)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    pub fn messages_follow(
        &self,
        caller: RecipientKey,
        after_seq: u64,
        limit: u32,
    ) -> Result<MessagesFollowResult, MailboxServiceError> {
        let current_routes = self.directory()?.current_routes();
        self.store()?
            .projection()
            .messages_follow(caller, after_seq, limit, &current_routes)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    /// The full durable mailbox attention set `status` and
    /// `messages.snapshot` serve, from the same projection: open alarms and
    /// held queue heads in every held state, pre-write blocks included.
    pub(crate) fn mailbox_attention_rows(
        &self,
    ) -> Result<Vec<cyclops_proto::OpenDelivery>, MailboxServiceError> {
        let directory = self.directory()?;
        let labels: HashMap<RecipientKey, String> = directory
            .current_routes()
            .iter()
            .map(|(key, route)| (*key, route.label.clone()))
            .collect();
        Ok(self.store()?.projection().mailbox_attention_rows(&labels))
    }

    /// Bounded body-free pre-write failures for the operator status view.
    pub(crate) fn blocked_notification_snapshot(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<BlockedNotificationSnapshot, MailboxServiceError> {
        let current_routes = self.directory()?.current_routes();
        self.store()?
            .projection()
            .blocked_notification_snapshot(self.admin().key, &current_routes, now, limit)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    /// Read the authoritative workspace journal in append order.
    pub fn journal_lines(&self) -> Result<Vec<LedgerLine>, MailboxServiceError> {
        self.store()?
            .writer
            .read_after(0)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    /// Message identifiers owned by this workspace journal.
    ///
    /// Compatibility session journals may contain older copies of these
    /// messages. Restart recovery must not treat those copies as a second
    /// delivery authority.
    pub(crate) fn workspace_message_ids(
        &self,
    ) -> Result<std::collections::HashSet<String>, MailboxServiceError> {
        Ok(self
            .store()?
            .projection()
            .messages
            .keys()
            .map(|id| id.as_str().to_string())
            .collect())
    }

    /// Canonical post-write composer barriers for restart reconciliation.
    pub(crate) fn active_notification_barriers(
        &self,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        Ok(self.store()?.projection().active_notification_barriers())
    }

    pub(crate) fn exact_recipient_claimed_after_write(
        &self,
        record: &NotificationRecord,
    ) -> Result<bool, MailboxServiceError> {
        Ok(self
            .store()?
            .projection()
            .exact_recipient_claimed_after_write(record))
    }

    /// All active composer barriers with message and mailbox facts from one
    /// projection read. Status groups this snapshot without rescanning it.
    pub(crate) fn active_composer_notifications_snapshot(
        &self,
    ) -> Result<Vec<ActiveComposerNotification>, MailboxServiceError> {
        let store = self.store()?;
        Ok(store
            .projection()
            .active_notification_barriers()
            .into_iter()
            .map(|record| ActiveComposerNotification {
                message: store.projection().get_message(&record.message_id).cloned(),
                entry_state: store
                    .projection()
                    .get_entry(record.recipient, &record.message_id)
                    .map(|entry| entry.state.clone()),
                recovery_action: store.projection().exact_owned_recovery_action(&record),
                record,
            })
            .collect())
    }

    /// Active composer barriers for one durable recipient, captured with the
    /// message and mailbox facts from the same projection read.
    pub(crate) fn active_composer_notifications(
        &self,
        recipient: RecipientKey,
    ) -> Result<Vec<ActiveComposerNotification>, MailboxServiceError> {
        Ok(self
            .active_composer_notifications_snapshot()?
            .into_iter()
            .filter(|candidate| candidate.record.recipient == recipient)
            .collect())
    }

    /// Content-free gate records for operational diagnostics.
    pub(crate) fn gating_notifications(
        &self,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        Ok(self.store()?.projection().gating_notifications())
    }

    /// Doorbells that may arm one exact-attempt reminder timer.
    pub(crate) fn unclaimed_reminder_candidates(
        &self,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let store = self.store()?;
        let mut records: Vec<_> = store
            .projection()
            .notifications
            .values()
            .filter(|record| {
                record.state == NotificationState::Notified
                    && record.transport == NotificationTransport::Doorbell
                    && record.unclaimed_reminder_count == 0
                    && store
                        .projection()
                        .get_entry(record.recipient, &record.message_id)
                        .is_some_and(|entry| entry.state.is_pending())
            })
            .cloned()
            .collect();
        records.sort_by_key(|record| record.updated_seq);
        Ok(records)
    }

    /// Exact verify-failed attempts eligible for the operator's opt-in timed
    /// Enter escape hatch. The final forced reservation, rather than this
    /// candidate scan, prevents duplicate scheduler calls from sending two keys.
    pub(crate) fn force_submit_candidates(
        &self,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let store = self.store()?;
        let mut records: Vec<_> = store
            .projection()
            .notifications
            .values()
            .filter(|record| record.needs_exact_owned_reconciliation())
            .filter(|record| {
                store
                    .projection()
                    .get_entry(record.recipient, &record.message_id)
                    .is_some_and(|entry| entry.state.is_pending())
            })
            .cloned()
            .collect();
        records.sort_by_key(|record| record.updated_seq);
        Ok(records)
    }

    /// Revalidate the exact timer target before force preparation. The final
    /// forced reservation, not this read, is the claim-ordering boundary.
    pub(crate) fn force_submit_target_is_pending(
        &self,
        target: &AttentionTarget,
    ) -> Result<bool, MailboxServiceError> {
        let store = self.store()?;
        Ok(store.projection().force_submit_target_is_pending(target))
    }

    /// Atomically classify or queue one due reminder under the store lock.
    pub(crate) fn queue_unclaimed_reminder(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<UnclaimedReminderQueue, MailboxServiceError> {
        let mut store = self.store()?;
        let Some(current) = store
            .projection()
            .notification_by_attempt(attempt_id)
            .cloned()
        else {
            return Ok(UnclaimedReminderQueue::Obsolete);
        };
        let pending = store
            .projection()
            .get_entry(current.recipient, &current.message_id)
            .is_some_and(|entry| entry.state.is_pending());
        if !pending
            || current.state != NotificationState::Notified
            || current.transport != NotificationTransport::Doorbell
            || current.unclaimed_reminder_count != 0
        {
            return Ok(UnclaimedReminderQueue::Obsolete);
        }
        if store
            .projection()
            .active_notification_barriers
            .contains_key(&attempt_id)
        {
            return Ok(UnclaimedReminderQueue::WaitingForPriorBarrier);
        }
        let record = store
            .queue_unclaimed_reminder(attempt_id)?
            .expect("eligibility checked under the same store lock");
        self.publish_change(record.updated_seq, &[MessagesChangedArea::Notifications]);
        Ok(UnclaimedReminderQueue::Queued(Box::new(record)))
    }

    /// Persist one exact reason that a composer barrier no longer applies.
    pub(crate) fn retire_notification_barrier(
        &self,
        record: &NotificationRecord,
        cause: NotificationBarrierRetirementCause,
        replacement: Option<NotificationBinding>,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.retire_notification_barrier(
            record.message_id.clone(),
            record.recipient,
            record.attempt_id,
            cause,
            replacement,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("barrier retirement advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
    }

    pub fn claim(
        &self,
        claimant: RecipientKey,
        message_id: MessageId,
    ) -> Result<ClaimOutcome, MailboxServiceError> {
        let mut store = self.store()?;
        let notification_changed = store
            .projection()
            .notification(claimant, &message_id)
            .map(|record| record.state.settled_by_claim(record.transport) != record.state)
            .unwrap_or_default();
        let outcome = store.claim(claimant, message_id)?;
        if let ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = &outcome
        {
            let seq = store
                .projection()
                .last_sequence()
                .expect("a fresh claim advances the workspace sequence");
            let changed = if notification_changed || withdrawn_attempt.is_some() {
                &[
                    MessagesChangedArea::Mailboxes,
                    MessagesChangedArea::Notifications,
                ][..]
            } else {
                &[MessagesChangedArea::Mailboxes][..]
            };
            self.publish_change(seq, changed);
        }
        Ok(outcome)
    }

    /// Claim a format 3 attempt locator or an imported legacy message id.
    pub(crate) fn claim_notification_locator(
        &self,
        claimant: RecipientKey,
        locator: MessageId,
        attempt_id: NotificationAttemptId,
    ) -> Result<ClaimOutcome, MailboxServiceError> {
        let mut store = self.store()?;
        let (outcome, notification_changed) =
            store.claim_notification_locator(claimant, locator, attempt_id)?;
        if let ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = &outcome
        {
            let seq = store
                .projection()
                .last_sequence()
                .expect("a fresh locator claim advances the workspace sequence");
            let changed = if notification_changed || withdrawn_attempt.is_some() {
                &[
                    MessagesChangedArea::Mailboxes,
                    MessagesChangedArea::Notifications,
                ][..]
            } else {
                &[MessagesChangedArea::Mailboxes][..]
            };
            self.publish_change(seq, changed);
        }
        Ok(outcome)
    }

    pub fn reply(
        &self,
        sender: MailboxIdentity,
        reference: MessageId,
        body: String,
        client_key: Option<String>,
    ) -> Result<AcceptResult, MailboxServiceError> {
        self.reply_with_summary(sender, reference, None, body, client_key)
    }

    pub fn reply_with_summary(
        &self,
        sender: MailboxIdentity,
        reference: MessageId,
        summary: Option<String>,
        body: String,
        client_key: Option<String>,
    ) -> Result<AcceptResult, MailboxServiceError> {
        // Reply routing is the referenced message's immutable sender key,
        // never its presentation label. Keep the current directory read
        // through the append so a rename preserves the route while a
        // replacement incarnation cannot enter between validation and
        // acceptance and inherit the predecessor's thread.
        let directory = self.directory()?;
        let mut store = self.store()?;
        let recipient = store
            .projection()
            .derive_reply(sender.key, &reference)?
            .recipient;
        let Some(destination) = directory.identity_for_recipient(recipient) else {
            return Err(MailboxDirectoryError::UnknownRecipient(recipient.to_string()).into());
        };
        let accepted = store.reply(
            mint_message_id(),
            ReplyDraft {
                sender: sender.key,
                reference,
                summary,
                body: (!body.is_empty()).then_some(body),
                client_key,
                sender_label: sender.label,
                recipient_label: destination.label,
            },
        )?;
        if accepted.inserted {
            self.publish_change(
                accepted.seq,
                &[
                    MessagesChangedArea::Messages,
                    MessagesChangedArea::Mailboxes,
                ],
            );
        }
        Ok(accepted)
    }

    /// Record a positive non-quota observation without resuming delivery.
    /// The returned records are the exact messages an administrator may
    /// now requeue explicitly.
    pub(crate) fn observe_quota_reset(
        &self,
        recipient: RecipientKey,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let mut store = self.store()?;
        let targets: Vec<_> = store
            .projection()
            .quota_held_for_recipient(recipient)
            .into_iter()
            .map(|record| {
                (
                    record.message_id.clone(),
                    record.recipient,
                    record.attempt_id,
                )
            })
            .collect();
        let mut observed = Vec::with_capacity(targets.len());
        for (message_id, recipient, attempt_id) in targets {
            let record = store.advance_notification(
                message_id,
                recipient,
                attempt_id,
                NotificationState::QuotaResetObserved,
                None,
                None,
            )?;
            self.publish_change(
                record.updated_seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
            observed.push(record);
        }
        Ok(observed)
    }

    /// Requeue every uncleared alarm or reset-observed quota hold on one message.
    ///
    /// Operator action only. The command mints one fresh attempt per
    /// recipient and appends one journal fact for the whole command.
    /// Nothing is written to a pane here and no retry is scheduled: each
    /// new attempt is queued and the delivery path picks it up on its own
    /// terms.
    pub fn requeue_message(
        &self,
        message_id: MessageId,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        let mut store = self.store()?;
        // A message nobody has heard of and a message with nothing in
        // attention are different answers: the first is a mistake worth
        // reporting, the second is a quiet success.
        if store.projection().get_message(&message_id).is_none() {
            return Err(MessageStoreError::from(MailboxError::MessageNotFound(message_id)).into());
        }
        // Resolve every semantic target before appending anything, the way
        // clear_alarms does. Appending inside the loop and aborting on a
        // later refusal leaves some recipients holding new attempt ids
        // while the operator is told the whole requeue failed, and the
        // ids they were shown are gone with no successful reply naming
        // the replacements.
        let projection = store.projection();
        let requeueable = projection.requeueable_for_message(&message_id);
        if let Some(record) = requeueable
            .iter()
            .filter(|record| record.state == NotificationState::AttentionRequired)
            .find(|record| projection.attention_resolution_pending(record.attempt_id))
        {
            return Err(
                MessageStoreError::from(MailboxError::NotificationResolutionAmbiguous(
                    record.attempt_id,
                ))
                .into(),
            );
        }
        let selected: Vec<_> = requeueable
            .into_iter()
            // An alarm whose entry has been claimed or superseded cannot
            // be redelivered. It stays visible and stays clearable; it is
            // simply not a requeue target, and skipping it here is what
            // prevents a predictable partial application.
            .filter(|record| projection.entry_is_pending(record.recipient, &message_id))
            .filter(|record| !resolving.contains(&record.attempt_id))
            .collect();
        if let Some(record) = selected.iter().copied().find(|record| {
            projection
                .active_notification_barriers
                .get(&record.attempt_id)
                .is_some_and(NotificationRecord::needs_exact_owned_reconciliation)
        }) {
            return Err(MessageStoreError::from(
                MailboxError::NotificationRequeueExactComposerBarrier(record.attempt_id),
            )
            .into());
        }
        if let Some(record) = selected.iter().copied().find(|record| {
            projection
                .active_notification_barriers
                .get(&record.attempt_id)
                .is_some_and(|active| {
                    active.binding.as_ref().is_none_or(|binding| {
                        binding.pane_root.is_none() || binding.leader.is_none()
                    })
                })
        }) {
            return Err(MessageStoreError::from(
                MailboxError::NotificationRequeueBarrierBindingIncomplete(record.attempt_id),
            )
            .into());
        }
        let requeues: Vec<_> = selected
            .into_iter()
            .map(|record| NotificationRequeue {
                prior_attempt_id: record.attempt_id,
                attempt_id: NotificationAttemptId::generate(),
                recipient: record.recipient,
            })
            .collect();

        let requeued = store.requeue_notifications(message_id, requeues)?;
        if let Some(record) = requeued.first() {
            self.publish_change(
                record.updated_seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(requeued)
    }

    /// Uncleared alarms at or before one absolute cutoff, oldest first.
    ///
    /// Read-only. Nothing here appends to the journal.
    pub fn alarms_at_or_before(
        &self,
        cutoff_ms: u64,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let store = self.store()?;
        Ok(store
            .projection()
            .open_alarms()
            .into_iter()
            .filter(|record| record.updated_at <= cutoff_ms)
            .cloned()
            .collect())
    }

    /// Acknowledge alarms named by exact attempt identifier.
    ///
    /// Every identifier is resolved before any fact is appended, so a
    /// request naming one unknown or superseded attempt changes nothing.
    /// Identifiers already acknowledged resolve and append nothing.
    /// Returned records are captured under the same lock as validation and
    /// clearance, in request order, including repeated identifiers.
    pub fn clear_alarms(
        &self,
        operator: RecipientKey,
        ids: &[NotificationAttemptId],
        cutoff_ms: Option<u64>,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let mut store = self.store()?;
        if operator != RecipientKey::admin(store.projection().workspace_id()) {
            return Err(MailboxError::NotificationClearOperatorInvalid.into());
        }

        let mut fresh = BTreeSet::new();
        let mut summaries = Vec::with_capacity(ids.len());
        for id in ids {
            let record = store
                .projection()
                .alarm_by_attempt(*id)
                .cloned()
                .ok_or_else(|| {
                    MessageStoreError::from(MailboxError::NotificationAttemptUnknown(*id))
                })?;
            if record.state != NotificationState::AttentionRequired {
                return Err(MessageStoreError::from(
                    MailboxError::NotificationClearRequiresAttention,
                )
                .into());
            }
            if let Some(cutoff_ms) = cutoff_ms {
                if record.updated_at > cutoff_ms {
                    return Err(MailboxError::NotificationNewerThanClearCutoff {
                        attempt_id: *id,
                        updated_at: record.updated_at,
                        cutoff_ms,
                    }
                    .into());
                }
            }
            if !store.projection().alarm_cleared(*id) {
                fresh.insert(*id);
            }
            summaries.push(record);
        }

        if !fresh.is_empty() {
            let before = store.projection().last_sequence();
            store.clear_notifications_at(
                operator,
                fresh.into_iter().collect(),
                cutoff_ms,
                now_ms(),
            )?;
            let after = store.projection().last_sequence();
            if after != before {
                self.publish_change(
                    after.expect("a fresh alarm clearance advances the workspace sequence"),
                    &[MessagesChangedArea::Attention],
                );
            }
        }
        Ok(summaries)
    }

    /// Withdraw one exact unwritten wake without changing its mailbox entry.
    ///
    /// The returned boolean is true only when this call appended the durable
    /// fact. Repeating the exact operation is a successful no-op.
    pub fn withdraw_notification_before_write(
        &self,
        operator: RecipientKey,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<(NotificationRecord, bool), MailboxServiceError> {
        let mut store = self.store()?;
        let before = store.projection().last_sequence();
        let record = store.withdraw_notification_before_write(operator, recipient, attempt_id)?;
        let inserted = store.projection().last_sequence() != before;
        if inserted {
            self.publish_change(record.updated_seq, &[MessagesChangedArea::Notifications]);
        }
        Ok((record, inserted))
    }

    /// Resolve an exact attempt id, or one message id with one unresolved attempt.
    pub(crate) fn attention_target(
        &self,
        raw: &str,
    ) -> Result<AttentionTarget, MailboxServiceError> {
        let store = self.store()?;
        let projection = store.projection();
        let record = if let Ok(attempt_id) = NotificationAttemptId::parse(raw) {
            let record = projection
                .alarm_by_attempt(attempt_id)
                .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
            if projection.attention_resolved(attempt_id) {
                return Err(MailboxError::NotificationAlreadyResolved(attempt_id).into());
            }
            if record.state != NotificationState::AttentionRequired {
                return Err(MailboxError::NotificationClearRequiresAttention.into());
            }
            record
        } else if let Ok(message_id) = MessageId::new(raw) {
            let matches = projection.unresolved_attention_for_message(&message_id);
            match matches.as_slice() {
                [] => return Err(MailboxError::NoUnresolvedAttention(message_id).into()),
                [record] => *record,
                many => {
                    return Err(MailboxError::AmbiguousAttentionTarget {
                        message_id,
                        candidates: many.iter().map(|record| record.attempt_id).collect(),
                    }
                    .into())
                }
            }
        } else {
            return Err(MailboxError::InvalidAttentionTarget(raw.to_string()).into());
        };
        projection
            .get_message(&record.message_id)
            .ok_or_else(|| MailboxError::MessageNotFound(record.message_id.clone()))?;
        Ok(AttentionTarget {
            record: record.clone(),
        })
    }

    /// Append the one content-free resolution fact for an exact attempt.
    pub(crate) fn resolve_attention(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        {
            let mut store = self.store()?;
            store.resolve_notification(
                target.record.message_id.clone(),
                target.record.recipient,
                target.record.attempt_id,
                resolution,
            )?;
            let seq = store
                .projection()
                .last_sequence()
                .expect("attention resolution advances the workspace sequence");
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        self.release_attention_resolution(target.record.attempt_id)?;
        Ok(())
    }

    /// Read the current durable policy for exact-owned recovery.
    pub(crate) fn automatic_attention_resolution(
        &self,
        target: &AttentionTarget,
    ) -> Result<Option<NotificationResolution>, MailboxServiceError> {
        Ok(self.store()?.projection().exact_owned_resolution(target))
    }

    /// Record a relevant evidence edge and elect at most one worker.
    pub(crate) fn request_exact_reconciliation(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        requests.dirty.insert(attempt_id);
        Ok(requests.running.insert(attempt_id))
    }

    /// Consume one evidence edge or retire the attempt worker atomically.
    pub(crate) fn take_exact_reconciliation_request(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if requests.dirty.remove(&attempt_id) {
            return Ok(true);
        }
        requests.running.remove(&attempt_id);
        Ok(false)
    }

    /// Preserve an edge that collided with an explicit resolution.
    ///
    /// The reservation check and worker handoff share the same critical
    /// section. Either the explicit resolver sees the parked edge when it
    /// releases its reservation, or this worker sees that the reservation
    /// already ended and continues immediately.
    pub(crate) fn park_exact_reconciliation_after_conflict(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        requests.dirty.insert(attempt_id);
        requests.running.remove(&attempt_id);
        if resolving.contains(&attempt_id) {
            return Ok(false);
        }
        Ok(requests.running.insert(attempt_id))
    }

    /// Re-elect a parked worker after the explicit reservation ends.
    pub(crate) fn resume_exact_reconciliation(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if resolving.contains(&attempt_id) {
            return Ok(false);
        }
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        Ok(requests.dirty.contains(&attempt_id) && requests.running.insert(attempt_id))
    }

    /// Persist the exact-owned recovery choice at its mailbox linearization point.
    pub(crate) fn record_automatic_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
    ) -> Result<NotificationResolution, MailboxServiceError> {
        let mut store = self.store()?;
        let resolution = store.projection().exact_owned_resolution(target).ok_or(
            MailboxError::NotificationAutomaticResolutionNotEligible(target.record.attempt_id),
        )?;
        if store
            .projection()
            .attention_resolution_intent(target.record.attempt_id)
            .is_some()
        {
            return Ok(resolution);
        }
        store.record_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention resolution intent advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(resolution)
    }

    /// Persist the terminal write boundary before a resolution action.
    pub(crate) fn record_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.record_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention resolution intent advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
    }

    /// Persist the timed escape hatch before its one terminal key. The fact
    /// carries no message content and marks the action as forced for audit.
    pub(crate) fn record_forced_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.record_forced_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            NotificationResolution::Complete,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("forced resolution intent advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
    }

    /// Atomically reserve one forced Complete key with mailbox claims.
    ///
    /// A claim ordered before this append makes the timer a no-op. Once the
    /// reservation is durable, a later claim remains a normal retrieval but
    /// cannot revoke the one key the fallback is about to send or count as
    /// consumption until the action-accepted fact exists.
    pub(crate) fn reserve_forced_attention_resolution_action(
        &self,
        target: &AttentionTarget,
    ) -> Result<bool, MailboxServiceError> {
        let mut store = self.store()?;
        if !store.projection().force_submit_target_is_pending(target) {
            return Ok(false);
        }
        let prior_seq = store.projection().last_sequence();
        store.reserve_forced_notification_resolution_action(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("forced key reservation advances the workspace sequence");
        if Some(seq) != prior_seq {
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(true)
    }

    /// Persist terminal acceptance for the exact durable resolution intent.
    pub(crate) fn record_attention_resolution_action_accepted(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        let prior_seq = store.projection().last_sequence();
        store.record_notification_resolution_action_accepted(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention action acceptance retains a workspace sequence");
        if Some(seq) != prior_seq {
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(())
    }

    /// Persist exact causal consumption for one accepted Complete action.
    pub(crate) fn record_attention_resolution_consumption_observed(
        &self,
        target: &AttentionTarget,
        observation: NotificationResolutionConsumptionObservation,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        let prior_seq = store.projection().last_sequence();
        store.record_notification_resolution_consumption_observed(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            observation,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention consumption observation retains a workspace sequence");
        if Some(seq) != prior_seq {
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(())
    }

    /// Register one boot-local exact-attempt matcher before the terminal key.
    ///
    /// The payload remains in memory. Only a closed evidence kind and timestamp
    /// can cross the durable boundary after terminal acceptance is recorded.
    pub(crate) fn register_attention_consumption_candidate(
        &self,
        target: &AttentionTarget,
        session_idx: usize,
        pane_id: String,
        expected_payload: String,
        not_before_ms: u64,
    ) -> Result<Option<Arc<AttentionConsumptionSignal>>, MailboxServiceError> {
        if !doorbell_format_names_exact_attempt(target.record.doorbell_format) {
            return Ok(None);
        }
        let binding = target
            .record
            .binding
            .as_ref()
            .ok_or(MailboxError::NotificationClearRequiresAttention)?;
        let pane_root = binding
            .pane_root
            .ok_or(MailboxError::NotificationClearRequiresAttention)?;
        let signal = Arc::new(AttentionConsumptionSignal::new());
        let candidate = AttentionConsumptionCandidate {
            message_id: target.record.message_id.clone(),
            recipient: target.record.recipient,
            session_idx,
            pane_id,
            pane_root,
            agent: binding.agent,
            manifest: binding.manifest.as_str().to_string(),
            expected_payload,
            not_before_ms,
            signal: Arc::clone(&signal),
        };
        let mut candidates = self
            .attention_consumption_candidates
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if candidates.contains_key(&target.record.attempt_id) {
            return Err(
                MailboxError::NotificationResolutionInProgress(target.record.attempt_id).into(),
            );
        }
        candidates.insert(target.record.attempt_id, candidate);
        Ok(Some(signal))
    }

    pub(crate) fn unregister_attention_consumption_candidate(
        &self,
        attempt_id: NotificationAttemptId,
    ) {
        if let Ok(mut candidates) = self.attention_consumption_candidates.lock() {
            candidates.remove(&attempt_id);
        }
    }

    /// Match an authenticated hook only when its token names this exact
    /// attempt and its payload and durable binding also match.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn confirm_attention_consumption_hook(
        &self,
        session_idx: usize,
        pane_id: &str,
        recipient: RecipientKey,
        pane_root: crate::identity::ProcId,
        agent: crate::identity::ProcId,
        manifest: &str,
        prompt: &str,
        observed_at_ms: u64,
    ) -> bool {
        let Some((message_id, attempt_id)) = cyclops_proto::parse_doorbell_v3(prompt)
            .map(|attempt_id| (None, attempt_id))
            .or_else(|| {
                cyclops_proto::parse_doorbell_v2(prompt)
                    .map(|(message_id, attempt_id)| (Some(message_id), attempt_id))
            })
        else {
            return false;
        };
        let Ok(candidates) = self.attention_consumption_candidates.lock() else {
            return false;
        };
        let matching: Vec<_> = candidates
            .iter()
            .filter(|(candidate_attempt, candidate)| {
                **candidate_attempt == attempt_id
                    && message_id
                        .as_ref()
                        .is_none_or(|message_id| &candidate.message_id == message_id)
                    && candidate.session_idx == session_idx
                    && candidate.pane_id == pane_id
                    && candidate.recipient == recipient
                    && candidate.pane_root.pid() == pane_root.pid
                    && candidate.pane_root.birth() == pane_root.birth
                    && candidate.agent.pid() == agent.pid
                    && candidate.agent.birth() == agent.birth
                    && candidate.manifest == manifest
                    && observed_at_ms >= candidate.not_before_ms
                    && crate::delivery::prompt_matches(prompt, &candidate.expected_payload)
            })
            .collect();
        if matching.len() != 1 {
            return false;
        }
        matching[0]
            .1
            .signal
            .confirm(NotificationResolutionConsumptionObservation {
                evidence: NotificationResolutionConsumptionEvidence::ExactHookPrompt,
                observed_at_ms,
            })
    }

    /// Return a durable claim eligible to count as this action's consumption.
    /// Ordinary actions need a claim after acceptance. Forced Complete instead
    /// orders claims against its preceding reservation, but waits for acceptance
    /// before treating an intervening claim as consumption evidence.
    pub(crate) fn attention_claim_consumption(
        &self,
        target: &AttentionTarget,
    ) -> Result<Option<NotificationResolutionConsumptionObservation>, MailboxServiceError> {
        Ok(self
            .store()?
            .projection()
            .exact_claim_after_attention_action(&target.record))
    }

    /// Append one atomic no-key Discard fact and release the reservation.
    pub(crate) fn resolve_attention_without_terminal_action(
        &self,
        target: &AttentionTarget,
    ) -> Result<(), MailboxServiceError> {
        {
            let mut store = self.store()?;
            store.resolve_notification_without_terminal_action(
                target.record.message_id.clone(),
                target.record.recipient,
                target.record.attempt_id,
            )?;
            let seq = store
                .projection()
                .last_sequence()
                .expect("no-key attention resolution advances the workspace sequence");
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        self.release_attention_resolution(target.record.attempt_id)?;
        Ok(())
    }

    /// Withdraw a proven pre-key refusal so the action may be attempted again.
    pub(crate) fn withdraw_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.withdraw_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention intent withdrawal advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
    }

    /// Reserve an attempt and distinguish a fresh action from no-key recovery.
    pub(crate) fn begin_attention_resolution(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<AttentionResolutionStart, MailboxServiceError> {
        let mut resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if resolving.contains(&target.record.attempt_id) {
            return Err(
                MailboxError::NotificationResolutionInProgress(target.record.attempt_id).into(),
            );
        }
        let store = self.store()?;
        let projection = store.projection();
        let current = projection
            .alarm_by_attempt(target.record.attempt_id)
            .ok_or(MailboxError::NotificationAttemptUnknown(
                target.record.attempt_id,
            ))?;
        if current != &target.record || projection.attention_resolved(current.attempt_id) {
            return Err(MailboxError::NotificationAlreadyResolved(current.attempt_id).into());
        }
        let accepted = projection.attention_resolution_action_accepted(current.attempt_id);
        let consumed = projection.attention_resolution_consumption_observed(current.attempt_id);
        let start = match projection.attention_resolution_intent(current.attempt_id) {
            Some(recorded) if recorded == resolution => match (resolution, accepted, consumed) {
                (
                    NotificationResolution::Complete,
                    Some(NotificationResolution::Complete),
                    Some(_),
                )
                | (NotificationResolution::Discard, Some(NotificationResolution::Discard), None) => {
                    AttentionResolutionStart::ReconcileOnly
                }
                (
                    NotificationResolution::Complete,
                    Some(NotificationResolution::Complete),
                    None,
                ) => AttentionResolutionStart::AcceptedUnconsumed,
                (
                    NotificationResolution::Complete | NotificationResolution::Discard,
                    None,
                    None,
                ) => AttentionResolutionStart::IntentOnlyUncertain,
                _ => {
                    return Err(
                        MailboxError::NotificationResolutionAmbiguous(current.attempt_id).into(),
                    )
                }
            },
            Some(_) => {
                return Err(
                    MailboxError::NotificationResolutionAmbiguous(current.attempt_id).into(),
                )
            }
            None if accepted.is_none() && consumed.is_none() => AttentionResolutionStart::Fresh,
            None => {
                return Err(
                    MailboxError::NotificationResolutionAmbiguous(current.attempt_id).into(),
                )
            }
        };
        resolving.insert(target.record.attempt_id);
        Ok(start)
    }

    pub(crate) fn cancel_attention_resolution(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<(), MailboxServiceError> {
        self.release_attention_resolution(attempt_id)
    }

    /// Subscribe before attempting a force-submit reservation. A matching
    /// release requires the caller to revalidate the exact attempt; it says
    /// nothing about durable settlement.
    pub(crate) fn subscribe_attention_resolution_releases(
        &self,
    ) -> broadcast::Receiver<NotificationAttemptId> {
        self.attention_resolution_releases.subscribe()
    }

    /// End one boot-local resolution reservation and wake exact waiters after
    /// the ownership change. A cancellation can leave the journal unchanged,
    /// so this must not be represented as `messages.changed`.
    fn release_attention_resolution(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<(), MailboxServiceError> {
        let released = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?
            .remove(&attempt_id);
        if released {
            let _ = self.attention_resolution_releases.send(attempt_id);
        }
        Ok(())
    }

    fn store(&self) -> Result<std::sync::MutexGuard<'_, MessageStore>, MailboxServiceError> {
        self.store.lock().map_err(|_| MailboxServiceError::Poisoned)
    }

    fn publish_change(&self, workspace_seq: u64, changed: &[MessagesChangedArea]) {
        if let Some(publisher) = &self.changes {
            publisher.publish(workspace_seq, changed);
        }
    }

    fn directory(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, MailboxDirectory>, MailboxServiceError> {
        self.directory
            .read()
            .map_err(|_| MailboxServiceError::Poisoned)
    }
}

/// Remove bodies the durable reader has not authored or claimed.
///
/// One store lock covers the complete result set. Lines absent from or not
/// content-identical to the workspace projection always lose bodies.
pub(crate) fn redact_message_bodies(
    service: Option<&MailboxService>,
    reader: Option<RecipientKey>,
    lines: &mut [LedgerLine],
) {
    let (Some(service), Some(reader)) = (service, reader) else {
        for line in lines {
            line.body = None;
        }
        return;
    };
    let Ok(store) = service.store() else {
        for line in lines {
            line.body = None;
        }
        return;
    };
    let projection = store.projection();
    for line in lines {
        if !projection_allows_message_body(projection, reader, line) {
            line.body = None;
        }
    }
}

fn projection_allows_message_body(
    projection: &MailboxProjection,
    reader: RecipientKey,
    line: &LedgerLine,
) -> bool {
    if !matches!(line.kind, Kind::Msg | Kind::Fyi) {
        return false;
    }
    MessageId::new(line.id.clone())
        .ok()
        .and_then(|message_id| {
            let message = projection.get_message(&message_id)?;
            let same_canonical_content = line.kind == message.kind
                && line.from == message.from
                && line.to == message.to
                && line.subject == message.subject
                && line.body == message.body
                && line.reply_to == message.reply_to
                && line.seq == message.seq
                && line.ts == message.ts
                && line.boot_id == message.boot_id;
            if !same_canonical_content {
                return None;
            }
            let metadata = extract_message_metadata(message).ok()?;
            Some((message_id, metadata))
        })
        .is_some_and(|(message_id, metadata)| {
            metadata.sender == reader
                || projection
                    .get_entry(reader, &message_id)
                    .is_some_and(|entry| match &entry.state {
                        MailboxEntryState::Claimed { claimant, .. } => *claimant == reader,
                        MailboxEntryState::DeliveredDirect { .. } => true,
                        MailboxEntryState::Pending | MailboxEntryState::Superseded { .. } => false,
                    })
        })
}

fn mint_message_id() -> MessageId {
    MessageId::new(format!("m-{}", uuid::Uuid::new_v4().simple())).expect("UUID message id")
}

impl MessageStore {
    /// Open one strict workspace journal and rebuild its complete projection.
    pub fn open(
        root: &StateRoot,
        descendant: &Path,
        workspace_id: WorkspaceId,
        boot_id: &str,
    ) -> Result<Self, MessageStoreError> {
        let (writer, lines) = LedgerWriter::open_strict_with_replay(root, descendant, boot_id)?;
        let mut projection = MailboxProjection::new(workspace_id);
        for line in lines {
            projection.apply_replayed_owned(line)?;
        }
        Ok(Self {
            writer,
            projection,
            fail_notification_recovery_append: None,
            #[cfg(test)]
            fail_batch_append: false,
            #[cfg(test)]
            fail_claimed_staged_clear_appends: 0,
            #[cfg(test)]
            fail_claimed_ack_timeout_reconciliation_appends: 0,
            #[cfg(test)]
            fail_pre_write_block_appends: 0,
        })
    }

    pub fn projection(&self) -> &MailboxProjection {
        &self.projection
    }

    pub fn journal_path(&self) -> &Path {
        self.writer.path()
    }

    #[cfg(test)]
    pub(crate) fn inject_next_batch_append_failure(&mut self) {
        self.fail_batch_append = true;
    }

    #[doc(hidden)]
    pub(crate) fn inject_notification_recovery_append_failure(
        &mut self,
        attempt_id: NotificationAttemptId,
    ) {
        self.fail_notification_recovery_append = Some(attempt_id);
    }

    #[cfg(test)]
    pub(crate) fn inject_next_claimed_staged_clear_append_failure(&mut self) {
        self.fail_claimed_staged_clear_appends = 1;
    }

    #[cfg(test)]
    pub(crate) fn inject_claimed_staged_clear_append_failures(&mut self, count: usize) {
        self.fail_claimed_staged_clear_appends = count;
    }

    #[cfg(test)]
    pub(crate) fn inject_next_claimed_ack_timeout_reconciliation_append_failure(&mut self) {
        self.fail_claimed_ack_timeout_reconciliation_appends = 1;
    }

    #[cfg(test)]
    pub(crate) fn inject_next_pre_write_block_append_failure(&mut self) {
        self.fail_pre_write_block_appends = 1;
    }

    /// Accept a new message or identify an exact retry.
    pub fn accept(
        &mut self,
        message_id: MessageId,
        draft: MessageDraft,
    ) -> Result<AcceptResult, MessageStoreError> {
        self.accept_at(message_id, draft, now_ms())
    }

    fn accept_at(
        &mut self,
        message_id: MessageId,
        draft: MessageDraft,
        ts: u64,
    ) -> Result<AcceptResult, MessageStoreError> {
        let draft = CanonicalDraft {
            kind: draft.kind,
            sender: draft.sender,
            recipients: draft.recipients,
            subject: draft.subject,
            summary: draft.summary,
            body: draft.body,
            reply_to: None,
            client_key: draft.client_key,
            supersedes: draft.supersedes,
            presentation: draft.presentation,
        };
        self.accept_canonical_at(message_id, draft, ts)
    }

    /// Accept a reply using routing and subject derived from the referenced message.
    pub fn reply(
        &mut self,
        message_id: MessageId,
        draft: ReplyDraft,
    ) -> Result<AcceptResult, MessageStoreError> {
        self.reply_at(message_id, draft, now_ms())
    }

    fn reply_at(
        &mut self,
        message_id: MessageId,
        draft: ReplyDraft,
        ts: u64,
    ) -> Result<AcceptResult, MessageStoreError> {
        let derived = self
            .projection
            .derive_reply(draft.sender, &draft.reference)?;
        let canonical = CanonicalDraft {
            kind: Kind::Msg,
            sender: draft.sender,
            recipients: vec![derived.recipient],
            subject: derived.subject,
            summary: draft.summary,
            body: draft.body,
            reply_to: Some(draft.reference),
            client_key: draft.client_key,
            supersedes: None,
            presentation: MessagePresentation {
                sender_label: draft.sender_label,
                recipient_labels: vec![cyclops_proto::RecipientPresentation {
                    recipient: derived.recipient,
                    label: draft.recipient_label,
                }],
            },
        };
        self.accept_canonical_at(message_id, canonical, ts)
    }

    fn accept_canonical_at(
        &mut self,
        message_id: MessageId,
        draft: CanonicalDraft,
        ts: u64,
    ) -> Result<AcceptResult, MessageStoreError> {
        let request_digest = match self.projection.check_acceptance(&draft)? {
            AcceptanceOutcome::Existing(existing) => {
                let message = self
                    .projection
                    .get_message(&existing)
                    .expect("idempotency index retains its message");
                return Ok(AcceptResult {
                    message_id: existing,
                    inserted: false,
                    seq: message.seq,
                    recipients: message.to.clone(),
                    recipient_keys: extract_message_metadata(message)?.recipients,
                });
            }
            AcceptanceOutcome::New { request_digest } => request_digest,
        };
        let thread_root = match draft.reply_to.as_ref() {
            Some(reference) => {
                self.projection
                    .derive_reply(draft.sender, reference)?
                    .thread_root
            }
            None => self
                .projection
                .supersession_thread_root(
                    draft.sender,
                    &draft.recipients,
                    draft.supersedes.as_ref(),
                )?
                .unwrap_or_else(|| message_id.clone()),
        };
        let metadata = MessageMetadata {
            record_version: CANONICAL_RECORD_VERSION,
            workspace_id: self.projection.workspace_id(),
            sender: draft.sender,
            recipients: draft.recipients.clone(),
            presentation: draft.presentation.clone(),
            summary: draft.summary.clone(),
            thread_root,
            client_key: draft.client_key.clone(),
            request_digest,
            supersedes: draft.supersedes.clone(),
        };
        let (_, recipient_labels) = presentation_labels(&draft.recipients, &draft.presentation)?;
        let recipient_keys = metadata.recipients.clone();
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: if ts == 0 { now_ms().max(1) } else { ts },
            kind: draft.kind,
            from: draft.presentation.sender_label,
            to: recipient_labels,
            subject: draft.subject,
            body: draft.body,
            reply_to: draft.reply_to.map(|id| id.to_string()),
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(metadata)?),
        };

        let seq = line.seq;
        let recipients = line.to.clone();
        let prepared = self.projection.prepare_line(&line)?;
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(AcceptResult {
            message_id,
            inserted: true,
            seq,
            recipients,
            recipient_keys,
        })
    }

    /// Claim one message. Re-claiming returns the same entry and payload.
    pub fn claim(
        &mut self,
        claimant: RecipientKey,
        message_id: MessageId,
    ) -> Result<ClaimOutcome, MessageStoreError> {
        self.claim_at(claimant, message_id, now_ms())
    }

    /// Resolve a reserved format 3 locator without shadowing legacy messages.
    /// The boolean reports whether the claim also changes notification state.
    fn claim_notification_locator(
        &mut self,
        claimant: RecipientKey,
        locator: MessageId,
        attempt_id: NotificationAttemptId,
    ) -> Result<(ClaimOutcome, bool), MessageStoreError> {
        let real_message_exists = self.projection.get_message(&locator).is_some();
        let attempt_was_issued = self.projection.notification_attempts.contains(&attempt_id);
        if !attempt_was_issued {
            let notification_changed = self
                .projection
                .notification(claimant, &locator)
                .map(|record| record.state.settled_by_claim(record.transport) != record.state)
                .unwrap_or_default();
            let outcome = self.claim_at(claimant, locator, now_ms())?;
            return Ok((outcome, notification_changed));
        }

        let record = self
            .projection
            .notification_by_attempt(attempt_id)
            .cloned()
            .filter(|record| {
                doorbell_format_names_exact_attempt(record.doorbell_format)
                    && record.recipient == claimant
            })
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if real_message_exists {
            return Err(MailboxError::NotificationAttemptClaimLocatorConflict(locator).into());
        }
        let notification_changed = record.state.settled_by_claim(record.transport) != record.state;
        let outcome = self.claim_at(claimant, record.message_id, now_ms())?;
        Ok((outcome, notification_changed))
    }

    fn claim_at(
        &mut self,
        claimant: RecipientKey,
        message_id: MessageId,
        ts: u64,
    ) -> Result<ClaimOutcome, MessageStoreError> {
        let skipped_oldest = self
            .projection
            .get_pending(claimant)
            .into_iter()
            .next()
            .map(|entry| entry.message_id.clone())
            .filter(|oldest| oldest != &message_id);
        let entry = self
            .projection
            .get_entry(claimant, &message_id)
            .cloned()
            .ok_or_else(|| MailboxError::EntryNotFound {
                message_id: message_id.clone(),
                recipient: claimant,
            })?;
        let message = self
            .projection
            .get_message(&message_id)
            .map(|line| inbox_message(line, claimant))
            .transpose()?
            .ok_or_else(|| MailboxError::MessageNotFound(message_id.clone()))?;
        let prior_claim_seq = self
            .projection
            .claim_sequences
            .get(&(entry.recipient, message_id.clone()))
            .copied();
        // A repeat claim preserves the consumed attempt only when the claim
        // fact itself moved a submitted doorbell to Notified. Retrieval before
        // submit does not consume the independent operator notification.
        let consumed_doorbell_attempt = self
            .projection
            .notification(entry.recipient, &message_id)
            .filter(|record| {
                record.transport == NotificationTransport::Doorbell
                    && (record.state == NotificationState::Submitted
                        || record.state == NotificationState::SubmittedUnverified
                        || (record.state == NotificationState::Notified
                            && prior_claim_seq == Some(record.updated_seq)))
            })
            .map(|record| record.attempt_id);
        let claimed_ack_timeout_attempt = self
            .projection
            .notification(entry.recipient, &message_id)
            .filter(|record| record.needs_claimed_ack_timeout_reconciliation())
            .map(|record| record.attempt_id);

        match &entry.state {
            MailboxEntryState::Pending => {}
            MailboxEntryState::Claimed {
                claimant: existing, ..
            } => {
                if *existing == claimant {
                    return Ok(ClaimOutcome::AlreadyClaimed {
                        entry,
                        message,
                        withdrawn_attempt: None,
                        consumed_doorbell_attempt,
                        claimed_ack_timeout_attempt: None,
                    });
                }
                return Err(MailboxError::AlreadyClaimed {
                    message_id,
                    recipient: entry.recipient,
                    existing_claimant: *existing,
                }
                .into());
            }
            MailboxEntryState::Superseded { .. } => {
                return Err(MailboxError::MessageNotPending(message_id).into());
            }
            MailboxEntryState::DeliveredDirect { .. } => {
                return Err(MailboxError::MessageNotPending(message_id).into());
            }
        }

        let withdrawn_attempt = self
            .projection
            .notification(entry.recipient, &message_id)
            .filter(|record| {
                record.state.settled_by_claim(record.transport) == NotificationState::Withdrawn
            })
            .map(|record| record.attempt_id);
        let fact = MailboxFact::MessageClaimed {
            record_version: CANONICAL_RECORD_VERSION,
            message_id: message_id.clone(),
            recipient: entry.recipient,
            claimant,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: if ts == 0 { now_ms().max(1) } else { ts },
            kind: Kind::State,
            from: claimant.to_string(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };

        let prepared = self.projection.prepare_line(&line)?;
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        let updated = self
            .projection
            .get_entry(claimant, &message_id)
            .cloned()
            .expect("prepared claim retains its mailbox entry");
        Ok(ClaimOutcome::Claimed {
            entry: updated,
            message,
            skipped_oldest,
            withdrawn_attempt,
            consumed_doorbell_attempt,
            claimed_ack_timeout_attempt,
        })
    }

    /// Start one notification attempt in the queued state.
    pub fn queue_notification(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.append_notification_transition_at(
            message_id,
            recipient,
            attempt_id,
            NotificationState::Queued,
            None,
            None,
            now_ms(),
        )
    }

    pub fn mark_delivered_direct(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<MailboxEntry, MessageStoreError> {
        self.mark_delivered_direct_at(message_id, recipient, attempt_id, now_ms())
    }

    fn mark_delivered_direct_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        ts: u64,
    ) -> Result<MailboxEntry, MessageStoreError> {
        if let Some(entry) = self.projection.get_entry(recipient, &message_id) {
            if matches!(
                entry.state,
                MailboxEntryState::DeliveredDirect {
                    attempt_id: existing,
                    ..
                } if existing == attempt_id
            ) {
                return Ok(entry.clone());
            }
        }
        let fact = MailboxFact::MessageDeliveredDirect {
            record_version: CANONICAL_RECORD_VERSION,
            message_id: message_id.clone(),
            recipient,
            attempt_id,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: if ts == 0 { now_ms().max(1) } else { ts },
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };
        let prepared = self.projection.prepare_line(&line)?;
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(self
            .projection
            .get_entry(recipient, &message_id)
            .cloned()
            .expect("prepared direct delivery retains its mailbox entry"))
    }

    /// Advance the current notification attempt by one legal state.
    pub fn advance_notification(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        state: NotificationState,
        binding: Option<NotificationBinding>,
        cause: Option<NotificationAttentionCause>,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.append_notification_transition_at(
            message_id,
            recipient,
            attempt_id,
            state,
            binding,
            cause,
            now_ms(),
        )
    }

    /// Record one post-write verification failure with its content-free evidence.
    pub fn advance_notification_with_verify_outcome(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        outcome: NotificationVerifyOutcome,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.append_notification_transition_full_at(
            message_id,
            recipient,
            attempt_id,
            NotificationState::AttentionRequired,
            None,
            None,
            None,
            Some(NotificationAttentionCause::VerifyFailed),
            Some(outcome),
            None,
            None,
            None,
            now_ms(),
        )
    }

    /// Stop one exact attempt before any terminal write.
    pub fn block_notification_before_write(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        cause: NotificationPreWriteCause,
        observation: Option<NotificationPreWriteObservation>,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.block_notification_before_write_with_wake_block(
            message_id,
            recipient,
            attempt_id,
            cause,
            observation,
            None,
        )
    }

    /// Stop one exact attempt while retaining its scheduler diagnosis.
    pub fn block_notification_before_write_with_wake_block(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        cause: NotificationPreWriteCause,
        observation: Option<NotificationPreWriteObservation>,
        wake_block: Option<MessageWakeBlock>,
    ) -> Result<NotificationRecord, MessageStoreError> {
        #[cfg(test)]
        if self.fail_pre_write_block_appends > 0 {
            self.fail_pre_write_block_appends -= 1;
            return Err(LedgerError::Io {
                path: self.writer.path().to_path_buf(),
                source: std::io::Error::other("injected pre-write block journal append failure"),
            }
            .into());
        }
        self.append_notification_transition_full_at(
            message_id,
            recipient,
            attempt_id,
            NotificationState::BlockedPreWrite,
            None,
            None,
            None,
            None,
            None,
            Some(cause),
            wake_block,
            observation,
            now_ms(),
        )
    }

    /// Advance a notification while fixing its payload transport at Writing.
    #[allow(clippy::too_many_arguments)]
    pub fn advance_notification_with_transport(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        state: NotificationState,
        binding: NotificationBinding,
        transport: NotificationTransport,
        doorbell_format: Option<u32>,
    ) -> Result<NotificationRecord, MessageStoreError> {
        if let Some(format) = doorbell_format {
            if !matches!(
                format,
                DOORBELL_FORMAT_COMPACT_CLAIM
                    | DOORBELL_FORMAT_ATTEMPT_CLAIM
                    | DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM
                    | cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM
            ) {
                return Err(MailboxError::UnsupportedNotificationDoorbellFormat(format).into());
            }
        }
        self.append_notification_transition_with_transport_at(
            message_id,
            recipient,
            attempt_id,
            state,
            Some(binding),
            Some(transport),
            doorbell_format,
            None,
            now_ms(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn append_notification_transition_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        state: NotificationState,
        binding: Option<NotificationBinding>,
        cause: Option<NotificationAttentionCause>,
        ts: u64,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.append_notification_transition_with_transport_at(
            message_id, recipient, attempt_id, state, binding, None, None, cause, ts,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn append_notification_transition_with_transport_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        state: NotificationState,
        binding: Option<NotificationBinding>,
        transport: Option<NotificationTransport>,
        doorbell_format: Option<u32>,
        cause: Option<NotificationAttentionCause>,
        ts: u64,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let verify_outcome = (state == NotificationState::AttentionRequired
            && cause == Some(NotificationAttentionCause::VerifyFailed))
        .then(NotificationVerifyOutcome::ambiguous);
        self.append_notification_transition_full_at(
            message_id,
            recipient,
            attempt_id,
            state,
            binding,
            transport,
            doorbell_format,
            cause,
            verify_outcome,
            None,
            None,
            None,
            ts,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn append_notification_transition_full_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        state: NotificationState,
        binding: Option<NotificationBinding>,
        transport: Option<NotificationTransport>,
        doorbell_format: Option<u32>,
        cause: Option<NotificationAttentionCause>,
        verify_outcome: Option<NotificationVerifyOutcome>,
        pre_write_cause: Option<NotificationPreWriteCause>,
        wake_block: Option<MessageWakeBlock>,
        pre_write_observation: Option<NotificationPreWriteObservation>,
        ts: u64,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let fact = NotificationFact::NotificationTransition {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            state,
            binding,
            transport,
            doorbell_format,
            cause,
            verify_outcome,
            pre_write_cause,
            wake_block,
            pre_write_observation: pre_write_observation.map(Box::new),
        };
        self.append_notification_fact_at(message_id, recipient, fact, ts)
    }

    /// Spend the single reminder allowance for one exact pending doorbell.
    ///
    /// Obsolete timers are normal: a claim, withdrawal, or replacement may
    /// win before the deadline. Those cases return `None` without appending.
    pub(crate) fn queue_unclaimed_reminder(
        &mut self,
        attempt_id: NotificationAttemptId,
    ) -> Result<Option<NotificationRecord>, MessageStoreError> {
        self.queue_unclaimed_reminder_at(attempt_id, now_ms())
    }

    fn queue_unclaimed_reminder_at(
        &mut self,
        attempt_id: NotificationAttemptId,
        ts: u64,
    ) -> Result<Option<NotificationRecord>, MessageStoreError> {
        let Some(current) = self.projection.notification_by_attempt(attempt_id).cloned() else {
            return Ok(None);
        };
        let pending = self
            .projection
            .get_entry(current.recipient, &current.message_id)
            .is_some_and(|entry| entry.state.is_pending());
        if !pending
            || current.state != NotificationState::Notified
            || current.transport != NotificationTransport::Doorbell
            || current.unclaimed_reminder_count != 0
            || self
                .projection
                .active_notification_barriers
                .contains_key(&attempt_id)
        {
            return Ok(None);
        }
        let fact = NotificationFact::NotificationUnclaimedReminderQueued {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: current.message_id.clone(),
            recipient: current.recipient,
        };
        self.append_notification_fact_at(current.message_id, current.recipient, fact, ts)
            .map(Some)
    }

    /// Start a new queued attempt after an operator explicitly requeues attention.
    pub fn requeue_notification(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        prior_attempt_id: NotificationAttemptId,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.requeue_notification_at(
            message_id,
            recipient,
            prior_attempt_id,
            attempt_id,
            now_ms(),
        )
    }

    fn requeue_notifications(
        &mut self,
        message_id: MessageId,
        requeues: Vec<NotificationRequeue>,
    ) -> Result<Vec<NotificationRecord>, MessageStoreError> {
        match requeues.as_slice() {
            [] => Ok(Vec::new()),
            [requeue] => self
                .requeue_notification(
                    message_id,
                    requeue.recipient,
                    requeue.prior_attempt_id,
                    requeue.attempt_id,
                )
                .map(|record| vec![record]),
            _ => self.requeue_notifications_at(message_id, requeues, now_ms()),
        }
    }

    fn requeue_notification_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        prior_attempt_id: NotificationAttemptId,
        attempt_id: NotificationAttemptId,
        ts: u64,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let fact = NotificationFact::NotificationRequeued {
            record_version: CANONICAL_RECORD_VERSION,
            prior_attempt_id,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
        };
        self.append_notification_fact_at(message_id, recipient, fact, ts)
    }

    fn requeue_notifications_at(
        &mut self,
        message_id: MessageId,
        requeues: Vec<NotificationRequeue>,
        ts: u64,
    ) -> Result<Vec<NotificationRecord>, MessageStoreError> {
        let recipients: Vec<_> = requeues.iter().map(|requeue| requeue.recipient).collect();
        let fact = NotificationFact::NotificationsRequeued {
            record_version: CANONICAL_RECORD_VERSION,
            message_id: message_id.clone(),
            requeues,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: if ts == 0 { now_ms().max(1) } else { ts },
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: recipients.iter().map(ToString::to_string).collect(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };

        let prepared = self.projection.prepare_line(&line)?;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_batch_append) {
            return Err(LedgerError::Io {
                path: self.writer.path().to_path_buf(),
                source: std::io::Error::other("injected workspace journal append failure"),
            }
            .into());
        }
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);

        Ok(recipients
            .into_iter()
            .map(|recipient| {
                self.projection
                    .notification(recipient, &message_id)
                    .cloned()
                    .expect("prepared batch requeue retains every notification")
            })
            .collect())
    }

    /// Record an operator's acknowledgement of one attention-required attempt.
    ///
    /// Idempotent by design: acknowledging an attempt that is already
    /// acknowledged appends nothing, so repeating the command does not
    /// grow the journal or move the record.
    pub fn clear_notification(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.clear_notification_at(message_id, recipient, attempt_id, now_ms())
    }

    fn clear_notification_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        ts: u64,
    ) -> Result<NotificationRecord, MessageStoreError> {
        // The repeat case still has to name the current attempt. Anything
        // else falls through to the append path so it is refused there
        // rather than quietly reported as already done.
        if self.projection.alarm_cleared(attempt_id) {
            if let Some(record) = self.projection.notification(recipient, &message_id) {
                if record.attempt_id == attempt_id {
                    return Ok(record.clone());
                }
            }
        }
        let fact = NotificationFact::NotificationCleared {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
        };
        self.append_notification_fact_at(message_id, recipient, fact, ts)
    }

    /// Acknowledge several attention attempts with one durable journal fact.
    fn clear_notifications_at(
        &mut self,
        operator: RecipientKey,
        attempt_ids: Vec<NotificationAttemptId>,
        cutoff_ms: Option<u64>,
        ts: u64,
    ) -> Result<(), MessageStoreError> {
        if operator != RecipientKey::admin(self.projection.workspace_id()) {
            return Err(MailboxError::NotificationClearOperatorInvalid.into());
        }
        if attempt_ids.is_empty() {
            return Ok(());
        }

        let mut recipients = BTreeSet::new();
        for attempt_id in &attempt_ids {
            let record = self
                .projection
                .notification_by_attempt(*attempt_id)
                .ok_or(MailboxError::NotificationAttemptUnknown(*attempt_id))?;
            recipients.insert(record.recipient.to_string());
        }
        let batch_id = format!("clear-{}", uuid::Uuid::new_v4().simple());
        let fact = NotificationFact::NotificationsCleared {
            record_version: CANONICAL_RECORD_VERSION,
            batch_id: batch_id.clone(),
            attempt_ids,
            operator,
            cutoff_ms,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: batch_id,
            ts: if ts == 0 { now_ms().max(1) } else { ts },
            kind: Kind::State,
            from: operator.to_string(),
            to: recipients.into_iter().collect(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };

        let prepared = self.projection.prepare_line(&line)?;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_batch_append) {
            return Err(LedgerError::Io {
                path: self.writer.path().to_path_buf(),
                source: std::io::Error::other("injected workspace journal append failure"),
            }
            .into());
        }
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(())
    }

    /// Withdraw one exact unwritten operator notification without changing its
    /// independently pending or claimed mailbox entry.
    pub fn withdraw_notification_before_write(
        &mut self,
        operator: RecipientKey,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        if operator != RecipientKey::admin(self.projection.workspace_id()) {
            return Err(MailboxError::NotificationWithdrawalOperatorInvalid.into());
        }
        if self
            .projection
            .notification_by_attempt(attempt_id)
            .is_some_and(|record| record.state == NotificationState::WithdrawnByOperator)
        {
            let current = self
                .projection
                .notification_by_attempt(attempt_id)
                .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
            if current.recipient != recipient {
                return Err(MailboxError::NotificationWithdrawalRecipientMismatch {
                    expected: recipient,
                    found: current.recipient,
                }
                .into());
            }
            return Ok(current.clone());
        }
        let current = self
            .projection
            .notification_by_attempt(attempt_id)
            .cloned()
            .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
        if current.recipient != recipient {
            return Err(MailboxError::NotificationWithdrawalRecipientMismatch {
                expected: recipient,
                found: current.recipient,
            }
            .into());
        }
        if !current.state.can_withdraw_before_write() {
            return Err(MailboxError::NotificationWithdrawalRequiresPreWrite.into());
        }
        let fact = NotificationFact::NotificationWithdrawnBeforeWrite {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: current.message_id.clone(),
            recipient,
            operator,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: current.message_id.to_string(),
            ts: now_ms().max(1),
            kind: Kind::State,
            from: operator.to_string(),
            to: vec![recipient.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };
        let prepared = self.projection.prepare_line(&line)?;
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(self
            .projection
            .notification(recipient, &current.message_id)
            .cloned()
            .expect("prepared withdrawal retains its notification"))
    }

    fn resolve_notification(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let fact = NotificationFact::NotificationResolved {
            record_version: CANONICAL_RECORD_VERSION,
            proof_version: NOTIFICATION_RESOLUTION_PROOF_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution,
        };
        self.append_notification_fact_at(message_id, recipient, fact, now_ms())
    }

    fn resolve_notification_without_terminal_action(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let fact = NotificationFact::NotificationResolvedWithoutTerminalAction {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution: NotificationResolution::Discard,
        };
        self.append_notification_fact_at(message_id, recipient, fact, now_ms())
    }

    fn record_notification_resolution_intent(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.record_notification_resolution_intent_kind(
            message_id, recipient, attempt_id, resolution, false,
        )
    }

    fn record_forced_notification_resolution_intent(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    ) -> Result<NotificationRecord, MessageStoreError> {
        self.record_notification_resolution_intent_kind(
            message_id, recipient, attempt_id, resolution, true,
        )
    }

    /// Append the forced Complete key reservation after validating its claim
    /// ordering boundary in this same store instance.
    fn reserve_forced_notification_resolution_action(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let already_reserved = self
            .projection
            .validate_forced_notification_resolution_action_reservation(
                &message_id,
                recipient,
                attempt_id,
                NotificationResolution::Complete,
            )?;
        if already_reserved {
            return Ok(self
                .projection
                .notification(recipient, &message_id)
                .expect("validated forced reservation retains its notification")
                .clone());
        }
        let fact = NotificationFact::NotificationResolutionActionReserved {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution: NotificationResolution::Complete,
        };
        self.append_notification_fact_at(message_id, recipient, fact, now_ms())
    }

    fn record_notification_resolution_intent_kind(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
        forced: bool,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let fact = NotificationFact::NotificationResolutionIntent {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution,
            forced,
        };
        self.append_notification_fact_at(message_id, recipient, fact, now_ms())
    }

    fn record_notification_resolution_action_accepted(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let already_recorded = self
            .projection
            .validate_notification_resolution_action_accepted(
                &message_id,
                recipient,
                attempt_id,
                resolution,
            )?;
        if already_recorded {
            return Ok(self
                .projection
                .notification(recipient, &message_id)
                .expect("validated action acceptance retains its notification")
                .clone());
        }
        let fact = NotificationFact::NotificationResolutionActionAccepted {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution,
        };
        self.append_notification_fact_at(message_id, recipient, fact, now_ms())
    }

    fn record_notification_resolution_consumption_observed(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        observation: NotificationResolutionConsumptionObservation,
    ) -> Result<NotificationRecord, MessageStoreError> {
        if !observation.evidence.proves_exact_consumption() {
            return Err(MailboxError::InvalidNotificationFact(
                "new resolution consumption facts require exact causal evidence".into(),
            )
            .into());
        }
        let already_recorded = self
            .projection
            .validate_notification_resolution_consumption_observed(
                &message_id,
                recipient,
                attempt_id,
                observation,
            )?;
        if already_recorded {
            return Ok(self
                .projection
                .notification(recipient, &message_id)
                .expect("validated consumption observation retains its notification")
                .clone());
        }
        let fact = NotificationFact::NotificationResolutionConsumptionObserved {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            evidence: observation.evidence,
            observed_at_ms: observation.observed_at_ms,
        };
        self.append_notification_fact_at(message_id, recipient, fact, now_ms())
    }

    fn withdraw_notification_resolution_intent(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        resolution: NotificationResolution,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let fact = NotificationFact::NotificationResolutionIntentWithdrawn {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution,
        };
        self.append_notification_fact_at(message_id, recipient, fact, now_ms())
    }

    fn append_notification_fact_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        fact: NotificationFact,
        ts: u64,
    ) -> Result<NotificationRecord, MessageStoreError> {
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: if ts == 0 { now_ms().max(1) } else { ts },
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: vec![recipient.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };
        let prepared = self.projection.prepare_line(&line)?;
        if let Some(target) = &self.fail_notification_recovery_append {
            if let Some(data) = &line.data {
                if data
                    .get("attempt_id")
                    .and_then(|a| a.as_str())
                    .and_then(|s| NotificationAttemptId::parse(s).ok())
                    == Some(*target)
                {
                    self.fail_notification_recovery_append = None;
                    return Err(LedgerError::Io {
                        path: self.writer.path().to_path_buf(),
                        source: std::io::Error::other(
                            "injected workspace journal recovery append failure",
                        ),
                    }
                    .into());
                }
            }
        }
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(self
            .projection
            .notification(recipient, &message_id)
            .cloned()
            .expect("prepared notification retains its record"))
    }

    /// Settle a claimed staged doorbell and retire its barrier in one fact.
    ///
    /// The caller owns either the external exact-clear proof or the crash
    /// recovery proof of the same binding plus a visible empty composer. This
    /// append changes both projections or neither, so an IO failure leaves the
    /// staged attempt as the recipient FIFO owner.
    pub(crate) fn settle_claimed_staged_clear(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        if let Some(current) =
            self.projection
                .notification(recipient, &message_id)
                .filter(|record| {
                    record.attempt_id == attempt_id
                        && record.state == NotificationState::WithdrawnAfterStaging
                })
        {
            let claimed_by_recipient = self
                .projection
                .get_entry(recipient, &message_id)
                .is_some_and(|entry| {
                    matches!(
                        &entry.state,
                        MailboxEntryState::Claimed { claimant, .. } if *claimant == recipient
                    )
                });
            if current.transport != NotificationTransport::Doorbell
                || !claimed_by_recipient
                || self
                    .projection
                    .active_notification_barriers
                    .contains_key(&attempt_id)
            {
                return Err(MailboxError::InvalidNotificationFact(
                    "settled claimed staged clear has inconsistent projection state".into(),
                )
                .into());
            }
            return Ok(current.clone());
        }

        let fact = NotificationFact::NotificationClaimedStagedCleared {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: now_ms().max(1),
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: vec![recipient.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };
        let prepared = self.projection.prepare_line(&line)?;
        #[cfg(test)]
        if self.fail_claimed_staged_clear_appends > 0 {
            self.fail_claimed_staged_clear_appends -= 1;
            return Err(LedgerError::Io {
                path: self.writer.path().to_path_buf(),
                source: std::io::Error::other(
                    "injected claimed staged clear journal append failure",
                ),
            }
            .into());
        }
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(self
            .projection
            .notification(recipient, &message_id)
            .cloned()
            .expect("claimed staged clear retains its notification record"))
    }

    /// Settle a claimed exact-attempt ACK timeout after composer reconciliation.
    ///
    /// The dedicated identity-only fact moves the notification to Notified and
    /// retires its barrier together. Until that append succeeds, the alarm
    /// remains visible and the attempt remains the recipient FIFO owner.
    pub(crate) fn settle_claimed_ack_timeout_reconciliation(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationRecord, MessageStoreError> {
        if self
            .projection
            .claimed_ack_timeout_reconciliations
            .contains(&attempt_id)
        {
            let current = self
                .projection
                .notification(recipient, &message_id)
                .filter(|record| {
                    record.attempt_id == attempt_id
                        && record.state == NotificationState::Notified
                        && record.transport == NotificationTransport::Doorbell
                        && doorbell_format_names_exact_attempt(record.doorbell_format)
                        && !self
                            .projection
                            .active_notification_barriers
                            .contains_key(&attempt_id)
                })
                .ok_or_else(|| {
                    MailboxError::InvalidNotificationFact(
                        "settled ACK-timeout reconciliation has inconsistent projection state"
                            .into(),
                    )
                })?;
            return Ok(current.clone());
        }

        let fact = NotificationFact::NotificationClaimedAckTimeoutReconciled {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: now_ms().max(1),
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: vec![recipient.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };
        let prepared = self.projection.prepare_line(&line)?;
        #[cfg(test)]
        if self.fail_claimed_ack_timeout_reconciliation_appends > 0 {
            self.fail_claimed_ack_timeout_reconciliation_appends -= 1;
            return Err(LedgerError::Io {
                path: self.writer.path().to_path_buf(),
                source: std::io::Error::other(
                    "injected claimed ACK-timeout reconciliation journal append failure",
                ),
            }
            .into());
        }
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(self
            .projection
            .notification(recipient, &message_id)
            .cloned()
            .expect("ACK-timeout reconciliation retains its notification record"))
    }

    /// Retire one active composer barrier after external safety proof.
    pub(crate) fn retire_notification_barrier(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        cause: NotificationBarrierRetirementCause,
        replacement: Option<NotificationBinding>,
    ) -> Result<(), MessageStoreError> {
        let fact = NotificationFact::NotificationBarrierRetired {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            cause,
            replacement,
        };
        let line = LedgerLine {
            seq: self.writer.next_seq(),
            boot_id: self.writer.boot_id().to_string(),
            id: message_id.to_string(),
            ts: now_ms().max(1),
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: vec![recipient.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::to_value(fact)?),
        };
        let prepared = self.projection.prepare_line(&line)?;
        let persisted = self.writer.append(line)?;
        self.projection.commit_line(persisted, prepared);
        Ok(())
    }

    /// Resolve post-write attempts left ambiguous by a daemon restart.
    ///
    /// Opening a store only replays. Daemon startup calls this explicitly
    /// after it is ready to persist recovery facts.
    pub fn recover_notifications_after_restart(
        &mut self,
    ) -> Result<Vec<NotificationRecord>, MessageStoreError> {
        let unresolved: Vec<_> = self
            .projection
            .notifications
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    NotificationState::Writing
                        | NotificationState::Staged
                        | NotificationState::Submitting
                        | NotificationState::Submitted
                        | NotificationState::SubmittedUnverified
                )
            })
            .cloned()
            .collect();
        let mut recovered = Vec::with_capacity(unresolved.len());
        for record in unresolved {
            let claimed = self
                .projection
                .get_entry(record.recipient, &record.message_id)
                .is_some_and(|entry| entry.state.is_claimed());
            if record.state == NotificationState::Staged
                && record.transport == NotificationTransport::Doorbell
                && claimed
            {
                // Claim won before terminal submit intent. Preserve Staged and
                // its binding so the exact attempt can resume clear proof.
                continue;
            }
            let (state, cause) = if (record.state == NotificationState::Submitted
                || record.state == NotificationState::SubmittedUnverified)
                && record.transport == NotificationTransport::Doorbell
                && claimed
            {
                (NotificationState::Notified, None)
            } else {
                (
                    NotificationState::AttentionRequired,
                    Some(NotificationAttentionCause::DaemonRestart),
                )
            };
            recovered.push(self.advance_notification(
                record.message_id,
                record.recipient,
                record.attempt_id,
                state,
                None,
                cause,
            )?);
        }
        Ok(recovered)
    }
}


#[cfg(test)]
mod tests;

