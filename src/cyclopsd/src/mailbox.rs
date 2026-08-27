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
    RequestDigest, StatusBlockedNotification, StatusNextAction, TmuxPaneId, WorkspaceId,
    CANONICAL_RECORD_VERSION, DOORBELL_FORMAT_ATTEMPT_CLAIM, DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM,
    DOORBELL_FORMAT_COMPACT_CLAIM, NOTIFICATION_RESOLUTION_PROOF_VERSION,
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
    pub body: Option<String>,
    pub client_key: Option<String>,
    pub sender_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalDraft {
    kind: Kind,
    sender: RecipientKey,
    recipients: Vec<RecipientKey>,
    subject: Option<String>,
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
    recipient_label: String,
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

        let digest = RequestDigest::compute(
            draft.kind,
            draft.sender,
            &draft.recipients,
            draft.subject.as_deref(),
            draft.body.as_deref(),
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
            line.subject.as_deref(),
            line.body.as_deref(),
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
        Ok(PreparedMutation::NotificationResolutionIntent {
            attempt_id,
            resolution,
        })
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
            } => {
                self.resolution_intents.insert(attempt_id, resolution);
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
            }
            PreparedMutation::NotificationResolvedWithoutTerminalAction {
                attempt_id,
                resolution,
            } => {
                self.resolved_attempts.insert(attempt_id, resolution);
                self.active_notification_barriers.remove(&attempt_id);
            }
            PreparedMutation::NotificationResolved {
                attempt_id,
                resolution,
            } => {
                self.resolved_attempts.insert(attempt_id, resolution);
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
        let accepted_seq = self
            .resolution_action_sequences
            .get(&record.attempt_id)
            .copied()?;
        let claim_seq = self
            .claim_sequences
            .get(&(record.recipient, record.message_id.clone()))
            .copied()?;
        if claim_seq <= accepted_seq {
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
            recipient_label: parent_metadata.presentation.sender_label,
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

fn inbox_message(line: &LedgerLine) -> Result<InboxMessage, MailboxError> {
    let metadata = extract_message_metadata(line)?;
    Ok(InboxMessage {
        message_id: MessageId::new(&line.id)?,
        kind: line.kind,
        sender: Some(metadata.sender),
        sender_label: metadata.presentation.sender_label,
        subject: line.subject.clone(),
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
    #[cfg(test)]
    fail_next_batch_append: bool,
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
    exact_reconciliation: StdMutex<ExactReconciliationRequests>,
    attention_consumption_candidates:
        StdMutex<HashMap<NotificationAttemptId, AttentionConsumptionCandidate>>,
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
        Self {
            workspace_id: directory.workspace_id(),
            directory: RwLock::new(directory),
            store: Arc::new(StdMutex::new(store)),
            changes,
            resolving_attention: StdMutex::new(HashSet::new()),
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

    /// Queue or resume the oldest pending notification for one recipient.
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
        loop {
            let Some(message_id) = Self::first_actionable_pending_message_id(&store, recipient)
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
        if directory.identity_for_recipient(recipient).is_none() {
            return Err(MailboxDirectoryError::UnknownRecipient(recipient.to_string()).into());
        }
        let accepted = store.reply(
            mint_message_id(),
            ReplyDraft {
                sender: sender.key,
                reference,
                body: (!body.is_empty()).then_some(body),
                client_key,
                sender_label: sender.label,
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
        self.resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?
            .remove(&target.record.attempt_id);
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

    /// Return a durable claim ordered strictly after this exact action.
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
        self.resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?
            .remove(&target.record.attempt_id);
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
        self.resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?
            .remove(&attempt_id);
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
            #[cfg(test)]
            fail_next_batch_append: false,
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
    fn inject_next_batch_append_failure(&mut self) {
        self.fail_next_batch_append = true;
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
            body: draft.body,
            reply_to: Some(draft.reference),
            client_key: draft.client_key,
            supersedes: None,
            presentation: MessagePresentation {
                sender_label: draft.sender_label,
                recipient_labels: vec![cyclops_proto::RecipientPresentation {
                    recipient: derived.recipient,
                    label: derived.recipient_label,
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
                record.doorbell_format == Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM)
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
            .map(inbox_message)
            .transpose()?
            .ok_or_else(|| MailboxError::MessageNotFound(message_id.clone()))?;
        let prior_claim_seq = self
            .projection
            .claim_sequences
            .get(&(entry.recipient, message_id.clone()))
            .copied();
        // A repeat claim preserves the consumed attempt only when the claim
        // fact itself moved that attempt to Notified. A later screen or hook
        // receipt has another sequence and must not be attributed to claim.
        let consumed_doorbell_attempt = self
            .projection
            .notification(entry.recipient, &message_id)
            .filter(|record| {
                record.transport == NotificationTransport::Doorbell
                    && (matches!(
                        record.state,
                        NotificationState::Staged
                            | NotificationState::Submitting
                            | NotificationState::Submitted
                    ) || (record.state == NotificationState::Notified
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
                matches!(
                    record.state,
                    NotificationState::Queued
                        | NotificationState::Gating
                        | NotificationState::QuotaHeld
                        | NotificationState::QuotaResetObserved
                )
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
        if std::mem::take(&mut self.fail_next_batch_append) {
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
        if std::mem::take(&mut self.fail_next_batch_append) {
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

    /// Withdraw one exact unwritten notification while leaving its message pending.
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
        let fact = NotificationFact::NotificationResolutionIntent {
            record_version: CANONICAL_RECORD_VERSION,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution,
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
            let (state, cause) = if record.state == NotificationState::Submitted
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
mod tests {
    use super::*;
    use cyclops_proto::{
        scratch::scratch_dir, NotificationManifestId, ProcessInstanceId, RecipientPresentation,
        SessionInstanceId, TmuxPaneId, DOORBELL_V3_MIN_PANE_WIDTH,
    };
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::str::FromStr;

    struct StoreScratch {
        path: PathBuf,
    }

    impl StoreScratch {
        fn new(tag: &str) -> Self {
            Self {
                path: scratch_dir(&format!("message-store-{tag}-{}", uuid::Uuid::new_v4())),
            }
        }

        fn root(&self) -> StateRoot {
            StateRoot::open_or_create(&self.path).unwrap()
        }
    }

    impl Drop for StoreScratch {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    panic!("remove scratch {}: {error}", self.path.display());
                }
            }
        }
    }

    fn test_context() -> (WorkspaceId, RecipientKey, RecipientKey, RecipientKey) {
        let ws = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let sess = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let p1 = TmuxPaneId::from_str("%1").unwrap();
        let p2 = TmuxPaneId::from_str("%2").unwrap();

        let admin = RecipientKey::admin(ws);
        let bob = RecipientKey::agent(ws, sess, p1);
        let carol = RecipientKey::agent(ws, sess, p2);
        (ws, admin, bob, carol)
    }

    fn routes(
        recipients: impl IntoIterator<Item = RecipientKey>,
    ) -> HashMap<RecipientKey, MessageRecipientRoute> {
        recipients
            .into_iter()
            .filter_map(|recipient| {
                Some((
                    recipient,
                    MessageRecipientRoute {
                        label: recipient.pane_id()?.to_string(),
                        pane_id: recipient.pane_id()?,
                    },
                ))
            })
            .collect()
    }

    fn attempt(number: u64) -> NotificationAttemptId {
        NotificationAttemptId::parse(&format!("att-00000000-0000-4000-8000-{number:012x}")).unwrap()
    }

    fn exact_consumption(observed_at_ms: u64) -> NotificationResolutionConsumptionObservation {
        NotificationResolutionConsumptionObservation {
            evidence: NotificationResolutionConsumptionEvidence::AuthenticatedClaim,
            observed_at_ms,
        }
    }

    fn notification_binding(recipient: RecipientKey) -> NotificationBinding {
        NotificationBinding {
            recipient,
            pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
            leader: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
            agent: ProcessInstanceId::new(4242, 818_221).unwrap(),
            manifest: NotificationManifestId::new("codex").unwrap(),
        }
    }

    fn legacy_notification_binding(recipient: RecipientKey) -> NotificationBinding {
        NotificationBinding {
            pane_root: None,
            ..notification_binding(recipient)
        }
    }

    fn mailbox_send(address: &str, subject: &str, body: &str) -> MailboxSend {
        MailboxSend {
            addresses: vec![address.into()],
            recipient_keys: None,
            subject: subject.into(),
            body: body.into(),
            fyi: false,
            client_key: None,
            supersedes: None,
        }
    }

    fn exact_mailbox_send(
        recipient_keys: Vec<RecipientKey>,
        subject: &str,
        body: &str,
        client_key: Option<&str>,
    ) -> MailboxSend {
        MailboxSend {
            addresses: Vec::new(),
            recipient_keys: Some(recipient_keys),
            subject: subject.into(),
            body: body.into(),
            fyi: false,
            client_key: client_key.map(str::to_string),
            supersedes: None,
        }
    }

    fn next_change(
        events: &mut broadcast::Receiver<Event>,
        expected_seq: u64,
        expected: &[MessagesChangedArea],
    ) {
        let event = events.try_recv().expect("messages.changed event");
        assert_eq!(event.event, "messages.changed");
        assert_eq!(event.seq, Some(expected_seq));
        let data: MessagesChangedData = serde_json::from_value(event.data).unwrap();
        assert_eq!(data.workspace_seq, expected_seq);
        assert_eq!(
            data.changed,
            expected.iter().copied().collect::<BTreeSet<_>>()
        );
    }

    /// Gate 1: a durable reply routes to the ORIGINAL endpoint after the
    /// recipient's alias is renamed.
    ///
    /// `reply` derives its recipient from the referenced message's sender
    /// KEY, which is workspace plus session instance plus pane id and
    /// carries no label. A rename replaces a directory entry, never a key,
    /// so routing cannot follow it.
    ///
    /// The trap this pins is the one a label-based route would fall into:
    /// after the rename a DIFFERENT identity wears the sender's old label,
    /// so a reply resolved by name would land on the impostor.
    #[test]
    fn a_reply_routes_to_the_original_endpoint_after_an_alias_rename() {
        let scratch = StoreScratch::new("reply-after-rename");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _admin, bob, carol) = test_context();

        let before = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "observer".into(),
                },
            ],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let (sender, _) = broadcast::channel(64);
        let service = MailboxService::new_with_events(before, store, sender);

        let parent = service
            .send(
                MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                },
                mailbox_send("observer", "Question", "Body"),
            )
            .unwrap();

        // The rename, and the trap: bob takes a new label and carol takes
        // bob's old one, so "reviewer" now names a different endpoint.
        service
            .replace_directory(
                MailboxDirectory::new(
                    workspace,
                    [
                        MailboxIdentity {
                            key: bob,
                            label: "lead".into(),
                        },
                        MailboxIdentity {
                            key: carol,
                            label: "reviewer".into(),
                        },
                    ],
                )
                .unwrap(),
            )
            .unwrap();

        let reply = service
            .reply(
                MailboxIdentity {
                    key: carol,
                    label: "reviewer".into(),
                },
                parent.message_id.clone(),
                "Answer".into(),
                None,
            )
            .unwrap();

        assert_eq!(
            reply.recipient_keys,
            vec![bob],
            "the reply left the original endpoint after the rename"
        );
        assert_ne!(
            reply.recipient_keys,
            vec![carol],
            "the reply followed the old label to its new owner"
        );
        // MEASURED, and worth knowing: the rendered label is the one
        // RECORDED ON THE PARENT, not the endpoint's current name.
        // `derive_reply` returns `parent_metadata.presentation.sender_label`,
        // so after this rename the reply renders as addressed to "reviewer"
        // while routing to bob, and "reviewer" now names carol. Routing is
        // right and the display is stale. Pinned as behaviour rather than
        // silently corrected: which label a reply renders is a product
        // decision, not part of the routing clause.
        assert_eq!(
            reply.recipients,
            vec!["reviewer".to_string()],
            "the rendered label is the parent's recorded sender label"
        );
    }

    #[test]
    fn committed_mailbox_facts_publish_once_in_workspace_sequence_order() {
        let scratch = StoreScratch::new("change-events");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "observer".into(),
                },
            ],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let (sender, _) = broadcast::channel(64);
        let mut events = sender.subscribe();
        let service = MailboxService::new_with_events(directory, store, sender);

        let first = service
            .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
            .unwrap();
        next_change(
            &mut events,
            1,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ],
        );
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        next_change(&mut events, 2, &[MessagesChangedArea::Notifications]);
        let context = crate::notification_adapter::NotificationContext::new_with_changes(
            service.store_handle(),
            first.message_id.clone(),
            bob,
            queued.attempt_id,
            service.change_publisher(),
        );
        context.record_gating().unwrap();
        next_change(&mut events, 3, &[MessagesChangedArea::Notifications]);
        context.record_gating().unwrap();
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        context
            .record_writing(
                notification_binding(bob).pane_root.unwrap(),
                notification_binding(bob).leader.unwrap(),
                notification_binding(bob).agent,
                "codex",
                NotificationTransport::Doorbell,
                None,
            )
            .unwrap();
        next_change(&mut events, 4, &[MessagesChangedArea::Notifications]);
        context.record_staged().unwrap();
        next_change(&mut events, 5, &[MessagesChangedArea::Notifications]);
        context.reserve_submit().unwrap();
        next_change(&mut events, 6, &[MessagesChangedArea::Notifications]);
        context.record_submitted().unwrap();
        next_change(&mut events, 7, &[MessagesChangedArea::Notifications]);
        context.record_notified().unwrap();
        next_change(&mut events, 8, &[MessagesChangedArea::Notifications]);
        context.record_notified().unwrap();
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        service.claim(bob, first.message_id).unwrap();
        next_change(&mut events, 9, &[MessagesChangedArea::Mailboxes]);

        let second = service
            .send(service.admin(), mailbox_send("reviewer", "Second", "Body"))
            .unwrap();
        next_change(
            &mut events,
            10,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ],
        );
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        next_change(&mut events, 11, &[MessagesChangedArea::Notifications]);
        let context = crate::notification_adapter::NotificationContext::new_with_changes(
            service.store_handle(),
            second.message_id.clone(),
            bob,
            queued.attempt_id,
            service.change_publisher(),
        );
        context.record_gating().unwrap();
        next_change(&mut events, 12, &[MessagesChangedArea::Notifications]);
        context
            .record_writing(
                notification_binding(bob).pane_root.unwrap(),
                notification_binding(bob).leader.unwrap(),
                notification_binding(bob).agent,
                "codex",
                NotificationTransport::Doorbell,
                None,
            )
            .unwrap();
        next_change(&mut events, 13, &[MessagesChangedArea::Notifications]);
        context
            .record_attention(NotificationAttentionCause::VerifyFailed)
            .unwrap();
        next_change(
            &mut events,
            14,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        service
            .clear_alarms(admin, &[queued.attempt_id], None)
            .unwrap();
        next_change(&mut events, 15, &[MessagesChangedArea::Attention]);
        service
            .clear_alarms(admin, &[queued.attempt_id], None)
            .unwrap();
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        service.claim(bob, second.message_id).unwrap();
        next_change(&mut events, 16, &[MessagesChangedArea::Mailboxes]);

        let third = service
            .send(
                MailboxIdentity {
                    key: admin,
                    label: "admin".into(),
                },
                mailbox_send("reviewer", "Third", "Body"),
            )
            .unwrap();
        next_change(
            &mut events,
            17,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ],
        );
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        next_change(&mut events, 18, &[MessagesChangedArea::Notifications]);
        let context = crate::notification_adapter::NotificationContext::new_with_changes(
            service.store_handle(),
            third.message_id.clone(),
            bob,
            queued.attempt_id,
            service.change_publisher(),
        );
        context.record_gating().unwrap();
        next_change(&mut events, 19, &[MessagesChangedArea::Notifications]);
        context
            .record_writing(
                notification_binding(bob).pane_root.unwrap(),
                notification_binding(bob).leader.unwrap(),
                notification_binding(bob).agent,
                "codex",
                NotificationTransport::Doorbell,
                None,
            )
            .unwrap();
        next_change(&mut events, 20, &[MessagesChangedArea::Notifications]);
        context
            .record_attention(NotificationAttentionCause::VerifyFailed)
            .unwrap();
        next_change(
            &mut events,
            21,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        service.requeue_message(third.message_id).unwrap();
        next_change(
            &mut events,
            22,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );

        let fourth = service
            .send(
                service.admin(),
                mailbox_send("observer", "Late claim", "Body"),
            )
            .unwrap();
        next_change(
            &mut events,
            23,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ],
        );
        let queued = service.prepare_oldest_notification(carol).unwrap().unwrap();
        next_change(&mut events, 24, &[MessagesChangedArea::Notifications]);
        let context = crate::notification_adapter::NotificationContext::new_with_changes(
            service.store_handle(),
            fourth.message_id.clone(),
            carol,
            queued.attempt_id,
            service.change_publisher(),
        );
        context.record_gating().unwrap();
        next_change(&mut events, 25, &[MessagesChangedArea::Notifications]);
        context
            .record_writing(
                notification_binding(carol).pane_root.unwrap(),
                notification_binding(carol).leader.unwrap(),
                notification_binding(carol).agent,
                "codex",
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            )
            .unwrap();
        next_change(&mut events, 26, &[MessagesChangedArea::Notifications]);
        context.record_staged().unwrap();
        next_change(&mut events, 27, &[MessagesChangedArea::Notifications]);
        context.reserve_submit().unwrap();
        next_change(&mut events, 28, &[MessagesChangedArea::Notifications]);
        context.record_submitted().unwrap();
        next_change(&mut events, 29, &[MessagesChangedArea::Notifications]);
        context
            .record_attention(NotificationAttentionCause::AckTimeout)
            .unwrap();
        next_change(
            &mut events,
            30,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        let outcome = service.claim(carol, fourth.message_id.clone()).unwrap();
        assert!(matches!(
            outcome,
            ClaimOutcome::Claimed {
                claimed_ack_timeout_attempt: Some(found),
                ..
            } if found == queued.attempt_id
        ));
        next_change(&mut events, 31, &[MessagesChangedArea::Mailboxes]);
        {
            let store = service.store().unwrap();
            let record = store
                .projection()
                .notification(carol, &fourth.message_id)
                .unwrap();
            assert_eq!(record.state, NotificationState::AttentionRequired);
            assert_eq!(record.cause, Some(NotificationAttentionCause::AckTimeout));
        }
        context.settle_claimed_ack_timeout_reconciliation().unwrap();
        next_change(
            &mut events,
            32,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        let store = service.store().unwrap();
        let record = store
            .projection()
            .notification(carol, &fourth.message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::Notified);
        assert_eq!(record.cause, None);
    }

    #[test]
    fn supersession_and_claim_publish_distinct_notification_settlements() {
        let scratch = StoreScratch::new("supersession-change");
        let root = scratch.root();
        let (workspace, _, bob, carol) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "observer".into(),
                },
            ],
        )
        .unwrap();
        let store = MessageStore::open(
            &root,
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot",
        )
        .unwrap();
        let (sender, _) = broadcast::channel(8);
        let mut events = sender.subscribe();
        let service = MailboxService::new_with_events(directory, store, sender);

        let first = service
            .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
            .unwrap();
        next_change(
            &mut events,
            1,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ],
        );
        service.prepare_oldest_notification(bob).unwrap().unwrap();
        next_change(&mut events, 2, &[MessagesChangedArea::Notifications]);

        let mut replacement = mailbox_send("reviewer", "Replacement", "Body");
        replacement.supersedes = Some(first.message_id);
        service.send(service.admin(), replacement).unwrap();
        next_change(
            &mut events,
            3,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
                MessagesChangedArea::Notifications,
            ],
        );

        let claimable = service
            .send(service.admin(), mailbox_send("observer", "Claim", "Body"))
            .unwrap();
        next_change(
            &mut events,
            4,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ],
        );
        service.prepare_oldest_notification(carol).unwrap().unwrap();
        next_change(&mut events, 5, &[MessagesChangedArea::Notifications]);
        let lines_before_claim = service.journal_lines().unwrap().len();
        service.claim(carol, claimable.message_id.clone()).unwrap();
        next_change(
            &mut events,
            6,
            &[
                MessagesChangedArea::Mailboxes,
                MessagesChangedArea::Notifications,
            ],
        );
        let record = service
            .store()
            .unwrap()
            .projection()
            .notification(carol, &claimable.message_id)
            .cloned()
            .unwrap();
        assert_eq!(record.state, NotificationState::Withdrawn);
        let lines = service.journal_lines().unwrap();
        assert_eq!(lines.len(), lines_before_claim + 1);
        assert_eq!(
            lines.last().unwrap().data.as_ref().unwrap()["type"],
            "message_claimed"
        );
        assert!(lines.iter().all(|line| {
            line.data.as_ref().is_none_or(|data| {
                data["type"] != "notification_transition" || data["state"] != "withdrawn"
            })
        }));
        let dispositions = service.message_dispositions(&claimable.message_id).unwrap();
        assert_eq!(dispositions.len(), 1);
        assert_eq!(
            dispositions[0].notification_state,
            MessageNotificationState::NotStarted
        );
        assert_eq!(
            dispositions[0].notification_settlement,
            Some(MessageNotificationSettlement::WithdrawnByClaim)
        );
        let snapshot = service.messages_snapshot(carol, 10).unwrap();
        let notification = &snapshot
            .rows
            .iter()
            .find(|row| row.message_id == claimable.message_id)
            .unwrap()
            .recipients[0]
            .notification;
        assert_eq!(notification.state, MessageNotificationState::NotStarted);
        assert_eq!(
            notification.settlement,
            Some(MessageNotificationSettlement::WithdrawnByClaim)
        );
    }

    #[test]
    fn an_unlabeled_pane_uses_its_pane_id_once() {
        let (workspace, _, recipient, _) = test_context();
        let pane = TmuxPaneId::from_str("%1").unwrap();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: recipient,
                label: pane.to_string(),
            }],
        )
        .unwrap();

        assert_eq!(
            directory.resolve(&[pane.to_string()]).unwrap()[0].key,
            recipient
        );
    }

    #[test]
    fn duplicate_pane_ids_keep_exact_labels_and_broadcasts_but_refuse_raw_addressing() {
        let (workspace, _, first, _) = test_context();
        let pane = TmuxPaneId::from_str("%1").unwrap();
        let second_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let second = RecipientKey::agent(workspace, second_session, pane);
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: first,
                    label: "reviewer".into(),
                },
                MailboxIdentity {
                    key: second,
                    label: "implementer".into(),
                },
            ],
        )
        .unwrap();

        assert!(directory.agent_for_pane(pane).is_none());
        assert_eq!(
            directory.resolve(&["reviewer".into()]).unwrap()[0].key,
            first
        );
        assert_eq!(
            directory.resolve(&["implementer".into()]).unwrap()[0].key,
            second
        );
        assert!(matches!(
            directory.resolve(&[pane.to_string()]),
            Err(MailboxDirectoryError::UnknownRecipient(_))
        ));
        assert_eq!(
            directory
                .resolve(&["*".into()])
                .unwrap()
                .into_iter()
                .map(|identity| identity.key)
                .collect::<HashSet<_>>(),
            HashSet::from([first, second])
        );
    }

    #[test]
    fn exact_recipient_sends_use_current_identity_without_label_retargeting() {
        let scratch = StoreScratch::new("exact-recipient-send");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let pane = bob.pane_id().unwrap();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);

        let first = service
            .send(
                service.admin(),
                exact_mailbox_send(vec![bob, bob], "Exact", "Body", Some("exact-retry")),
            )
            .unwrap();
        assert_eq!(first.recipient_keys, [bob]);
        assert_eq!(first.recipients, ["reviewer"]);

        service
            .replace_directory(
                MailboxDirectory::new(
                    workspace,
                    [MailboxIdentity {
                        key: bob,
                        label: "implementer".into(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
        let retry = service
            .send(
                service.admin(),
                exact_mailbox_send(vec![bob], "Exact", "Body", Some("exact-retry")),
            )
            .unwrap();
        assert!(!retry.inserted);
        assert_eq!(retry.message_id, first.message_id);
        assert_eq!(retry.recipients, ["reviewer"]);

        let mut mixed_request = exact_mailbox_send(vec![bob], "Ambiguous", "", None);
        mixed_request.addresses.push("implementer".into());
        let mixed = service.send(service.admin(), mixed_request).unwrap_err();
        assert!(matches!(
            mixed,
            MailboxServiceError::Directory(MailboxDirectoryError::MixedRecipientSelectors)
        ));

        let replacement_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let replacement = RecipientKey::agent(workspace, replacement_session, pane);
        service
            .replace_directory(
                MailboxDirectory::new(
                    workspace,
                    [MailboxIdentity {
                        key: replacement,
                        label: "implementer".into(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();

        let stale = service
            .send(
                service.admin(),
                exact_mailbox_send(vec![bob], "Stale", "", None),
            )
            .unwrap_err();
        assert!(matches!(
            stale,
            MailboxServiceError::Directory(MailboxDirectoryError::UnknownRecipient(target))
                if target == bob.to_string()
        ));

        let current = service
            .send(
                service.admin(),
                exact_mailbox_send(vec![replacement], "Current", "", None),
            )
            .unwrap();
        assert_eq!(current.recipient_keys, [replacement]);
        assert_eq!(current.recipients, ["implementer"]);
    }

    #[test]
    fn concurrent_claims_report_the_head_from_the_claim_lock() {
        let scratch = StoreScratch::new("claim-head-lock");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = std::sync::Arc::new(MailboxService::new(directory, store));
        let first = service
            .send(
                service.admin(),
                mailbox_send("reviewer", "First", "First body"),
            )
            .unwrap()
            .message_id;
        let second = service
            .send(
                service.admin(),
                mailbox_send("reviewer", "Second", "Second body"),
            )
            .unwrap()
            .message_id;

        let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
        let first_worker = {
            let service = std::sync::Arc::clone(&service);
            let gate = std::sync::Arc::clone(&gate);
            let first = first.clone();
            std::thread::spawn(move || {
                gate.wait();
                service.claim(bob, first).unwrap()
            })
        };
        let second_worker = {
            let service = std::sync::Arc::clone(&service);
            let gate = std::sync::Arc::clone(&gate);
            let second = second.clone();
            std::thread::spawn(move || {
                gate.wait();
                service.claim(bob, second).unwrap()
            })
        };
        gate.wait();

        let first_outcome = first_worker.join().unwrap();
        let second_outcome = second_worker.join().unwrap();
        assert!(matches!(first_outcome, ClaimOutcome::Claimed { .. }));
        let ClaimOutcome::Claimed { skipped_oldest, .. } = second_outcome else {
            panic!("second message was not freshly claimed");
        };

        let store = service.store().unwrap();
        let first_seq = store
            .projection()
            .claim_sequences
            .get(&(bob, first.clone()))
            .copied()
            .unwrap();
        let second_seq = store
            .projection()
            .claim_sequences
            .get(&(bob, second))
            .copied()
            .unwrap();
        if first_seq < second_seq {
            assert_eq!(skipped_oldest, None);
        } else {
            assert_eq!(skipped_oldest, Some(first));
        }
    }

    #[test]
    fn exact_recipient_validation_stays_locked_through_acceptance() {
        let scratch = StoreScratch::new("exact-recipient-linearization");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let pane = bob.pane_id().unwrap();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = Arc::new(MailboxService::new(directory, store));
        let (resolved_tx, resolved_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sending = Arc::clone(&service);
        let send = std::thread::spawn(move || {
            sending.send_after_resolution(
                sending.admin(),
                exact_mailbox_send(vec![bob], "Exact", "Body", None),
                || {
                    resolved_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap();
                },
            )
        });

        resolved_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let directory_still_locked = matches!(
            service.directory.try_write(),
            Err(std::sync::TryLockError::WouldBlock)
        );
        release_tx.send(()).unwrap();
        let accepted = send.join().unwrap().unwrap();
        assert!(directory_still_locked);
        assert_eq!(accepted.recipient_keys, [bob]);

        let replacement_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let replacement = RecipientKey::agent(workspace, replacement_session, pane);
        service
            .replace_directory(
                MailboxDirectory::new(
                    workspace,
                    [MailboxIdentity {
                        key: replacement,
                        label: "implementer".into(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
        let stale = service
            .send(
                service.admin(),
                exact_mailbox_send(vec![bob], "Stale", "", None),
            )
            .unwrap_err();
        assert!(matches!(
            stale,
            MailboxServiceError::Directory(MailboxDirectoryError::UnknownRecipient(target))
                if target == bob.to_string()
        ));
    }

    #[test]
    fn service_directory_replacement_updates_routing_without_rewriting_messages() {
        let scratch = StoreScratch::new("directory-refresh");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let pane = TmuxPaneId::from_str("%1").unwrap();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let first = service
            .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
            .unwrap();
        let from_bob = service
            .send(
                MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                },
                mailbox_send("admin", "From reviewer", "Body"),
            )
            .unwrap();

        service
            .replace_directory(
                MailboxDirectory::new(
                    workspace,
                    [MailboxIdentity {
                        key: bob,
                        label: "implementer".into(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
        assert!(service
            .send(service.admin(), mailbox_send("reviewer", "Stale label", ""))
            .is_err());
        service
            .send(
                service.admin(),
                mailbox_send("implementer", "After rename", ""),
            )
            .unwrap();

        // Reply routing is derived from the durable sender recorded on
        // the referenced message. The label shown at send time may be
        // stale, but a rename must not turn the reply into an address
        // lookup against that old label.
        let reply = service
            .reply(
                MailboxIdentity {
                    key: bob,
                    label: "implementer".into(),
                },
                first.message_id.clone(),
                "Reply after rename".into(),
                None,
            )
            .unwrap();
        let reply_message = service
            .store()
            .unwrap()
            .projection()
            .get_message(&reply.message_id)
            .cloned()
            .unwrap();
        let reply_metadata = extract_message_metadata(&reply_message).unwrap();
        assert_eq!(reply_message.from, "implementer");
        assert_eq!(reply_message.to, ["admin"]);
        assert_eq!(reply_metadata.sender, bob);
        assert_eq!(reply_metadata.recipients, [service.admin().key]);

        let admin_reply = service
            .reply(
                service.admin(),
                from_bob.message_id.clone(),
                "Reply to renamed sender".into(),
                None,
            )
            .unwrap();
        let store = service.store().unwrap();
        let admin_reply_message = store
            .projection()
            .get_message(&admin_reply.message_id)
            .unwrap();
        let admin_reply_metadata = extract_message_metadata(admin_reply_message).unwrap();
        assert_eq!(admin_reply_message.to, ["reviewer"]);
        assert_eq!(admin_reply_metadata.recipients, [bob]);
        drop(store);

        let replacement_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let replacement_key = RecipientKey::agent(workspace, replacement_session, pane);
        service
            .replace_directory(
                MailboxDirectory::new(
                    workspace,
                    [MailboxIdentity {
                        key: replacement_key,
                        label: "implementer".into(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();

        assert_eq!(
            service.agent_for_pane(pane).unwrap().unwrap().key,
            replacement_key
        );
        assert!(matches!(
            service.reply(
                service.admin(),
                from_bob.message_id.clone(),
                "Replacement must not receive the predecessor reply".into(),
                None,
            ),
            Err(MailboxServiceError::Directory(
                MailboxDirectoryError::UnknownRecipient(recipient)
            )) if recipient == bob.to_string()
        ));
        let replacement_reply = service
            .reply(
                MailboxIdentity {
                    key: replacement_key,
                    label: "implementer".into(),
                },
                first.message_id.clone(),
                "Replacement must not inherit the thread".into(),
                None,
            )
            .unwrap_err();
        assert!(matches!(
            replacement_reply,
            MailboxServiceError::Store(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::ReplyNotVisible { sender, .. }
                    if *sender == replacement_key)
        ));
        assert!(service
            .send(service.admin(), mailbox_send("reviewer", "Stale", ""))
            .is_err());
        service
            .send(service.admin(), mailbox_send("implementer", "Second", ""))
            .unwrap();

        let store = service.store().unwrap();
        let original = store.projection().get_message(&first.message_id).unwrap();
        assert_eq!(original.to, ["reviewer"]);
        assert_eq!(store.projection().get_pending(bob).len(), 3);
        assert_eq!(store.projection().get_pending(replacement_key).len(), 1);
        assert!(service
            .agent_for_pane(TmuxPaneId::from_str("%9").unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn service_redacts_bodies_until_the_exact_recipient_claim() {
        let scratch = StoreScratch::new("body-privacy");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "bob".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "carol".into(),
                },
            ],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let accepted = service
            .send(
                MailboxIdentity {
                    key: bob,
                    label: "bob".into(),
                },
                mailbox_send("carol", "Private", "secret"),
            )
            .unwrap();
        let still_pending = service
            .send(
                MailboxIdentity {
                    key: bob,
                    label: "bob".into(),
                },
                mailbox_send("carol", "Other", "other secret"),
            )
            .unwrap();
        let message = service
            .journal_lines()
            .unwrap()
            .into_iter()
            .find(|line| line.id == accepted.message_id.as_str())
            .expect("canonical message line");

        let mut sender_view = vec![message.clone()];
        redact_message_bodies(Some(&service), Some(bob), &mut sender_view);
        assert_eq!(sender_view[0].body.as_deref(), Some("secret"));

        let mut recipient_view = vec![message.clone()];
        redact_message_bodies(Some(&service), Some(carol), &mut recipient_view);
        assert_eq!(recipient_view[0].body, None);

        let mut admin_view = vec![message.clone()];
        redact_message_bodies(Some(&service), Some(admin), &mut admin_view);
        assert_eq!(admin_view[0].body, None);

        let mut collision = message.clone();
        collision.body = Some("legacy collision body".into());
        collision.data = None;
        let mut collision_view = vec![collision];
        redact_message_bodies(Some(&service), Some(bob), &mut collision_view);
        assert_eq!(collision_view[0].body, None);

        service.claim(carol, accepted.message_id).unwrap();
        let mut claimed_view = vec![message.clone()];
        redact_message_bodies(Some(&service), Some(carol), &mut claimed_view);
        assert_eq!(claimed_view[0].body.as_deref(), Some("secret"));

        let mut other_view: Vec<_> = service
            .journal_lines()
            .unwrap()
            .into_iter()
            .filter(|line| line.id == still_pending.message_id.as_str())
            .collect();
        redact_message_bodies(Some(&service), Some(carol), &mut other_view);
        assert_eq!(other_view[0].body, None);

        let mut legacy = message;
        legacy.id = "m-legacy".into();
        legacy.data = None;
        for reader in [bob, carol, admin] {
            let mut lines = vec![legacy.clone()];
            redact_message_bodies(Some(&service), Some(reader), &mut lines);
            assert_eq!(lines[0].body, None);
        }
    }

    #[test]
    fn direct_delivery_grants_recipient_body_access_and_counts_as_settled() {
        let scratch = StoreScratch::new("direct-body-visibility");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-direct-visible").unwrap();
        let attempt_id = attempt(44);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(carol, vec![bob], "direct secret", None),
                1,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Queued,
                None,
                None,
                2,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Gating,
                None,
                None,
                3,
            )
            .unwrap();
        let binding = notification_binding(bob);
        for (ts, state) in [
            NotificationState::Writing,
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
            NotificationState::Notified,
        ]
        .into_iter()
        .enumerate()
        {
            if state == NotificationState::Writing {
                store
                    .append_notification_transition_with_transport_at(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        state,
                        Some(binding.clone()),
                        Some(NotificationTransport::DirectPayload),
                        None,
                        None,
                        4 + ts as u64,
                    )
                    .unwrap();
            } else {
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        state,
                        None,
                        None,
                        4 + ts as u64,
                    )
                    .unwrap();
            }
        }
        store
            .mark_delivered_direct_at(message_id.clone(), bob, attempt_id, 9)
            .unwrap();

        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "bob".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "carol".into(),
                },
            ],
        )
        .unwrap();
        let service = MailboxService::new(directory, store);
        let message = service
            .journal_lines()
            .unwrap()
            .into_iter()
            .find(|line| line.id == message_id.as_str() && line.kind == Kind::Msg)
            .unwrap();

        let mut recipient_view = vec![message.clone()];
        redact_message_bodies(Some(&service), Some(bob), &mut recipient_view);
        assert_eq!(recipient_view[0].body.as_deref(), Some("direct secret"));
        let mut admin_view = vec![message];
        redact_message_bodies(Some(&service), Some(admin), &mut admin_view);
        assert_eq!(admin_view[0].body, None);

        let snapshot = service.messages_snapshot(bob, 20).unwrap();
        assert_eq!(snapshot.counts.pending_entries, 0);
        assert_eq!(snapshot.counts.claimed_entries, 0);
        assert_eq!(snapshot.counts.active_messages, 0);
        assert_eq!(snapshot.counts.settled_messages, 1);
        assert_eq!(snapshot.counts.work_messages, 0);
        assert!(matches!(
            snapshot.rows[0].recipients[0].mailbox,
            MailboxEntryState::DeliveredDirect { .. }
        ));
    }

    #[test]
    fn oldest_pending_notification_is_stable_and_resumes_after_restart() {
        let scratch = StoreScratch::new("oldest-notification");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = || {
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                }],
            )
            .unwrap()
        };

        let first_id;
        let second_id;
        let first_attempt;
        {
            let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            let service = MailboxService::new(directory(), store);
            let first = service
                .send(service.admin(), mailbox_send("reviewer", "First", ""))
                .unwrap();
            let second = service
                .send(service.admin(), mailbox_send("reviewer", "Second", ""))
                .unwrap();
            first_id = first.message_id;
            second_id = second.message_id.clone();

            let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
            first_attempt = queued.attempt_id;
            assert_eq!(queued.message_id, first_id);
            assert_eq!(queued.state, NotificationState::Queued);
            assert_eq!(
                service
                    .prepare_oldest_notification(bob)
                    .unwrap()
                    .unwrap()
                    .attempt_id,
                first_attempt
            );
            assert!(service
                .store()
                .unwrap()
                .projection()
                .notification(bob, &second.message_id)
                .is_none());
        }

        let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(directory(), store);
        let resumed = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(resumed.message_id, first_id);
        assert_eq!(resumed.attempt_id, first_attempt);
        assert_eq!(service.journal_lines().unwrap().len(), 3);

        service.claim(bob, first_id).unwrap();
        let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(next.message_id, second_id);
        assert_ne!(next.attempt_id, first_attempt);
    }

    #[test]
    fn pending_operator_resolution_owns_the_claimed_barrier_after_restart() {
        let scratch = StoreScratch::new("operator-resolution-barrier");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = || {
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                }],
            )
            .unwrap()
        };
        let attempt_id;
        let later_message_id;

        {
            let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            let service = MailboxService::new(directory(), store);
            let first = service
                .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
                .unwrap();
            let later = service
                .send(service.admin(), mailbox_send("reviewer", "Later", "Body"))
                .unwrap();
            later_message_id = later.message_id;
            let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
            attempt_id = queued.attempt_id;
            let context = crate::notification_adapter::NotificationContext::new(
                service.store_handle(),
                first.message_id.clone(),
                bob,
                attempt_id,
            );
            context.record_gating().unwrap();
            context
                .record_writing(
                    notification_binding(bob).pane_root.unwrap(),
                    notification_binding(bob).leader.unwrap(),
                    notification_binding(bob).agent,
                    "codex",
                    NotificationTransport::Doorbell,
                    Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
                )
                .unwrap();
            context.record_staged().unwrap();
            context.reserve_submit().unwrap();
            context.record_submitted().unwrap();
            context
                .record_attention(NotificationAttentionCause::AckTimeout)
                .unwrap();
            service.claim(bob, first.message_id).unwrap();

            let target = service.attention_target(&attempt_id.to_string()).unwrap();
            service
                .record_attention_resolution_intent(&target, NotificationResolution::Complete)
                .unwrap();
            service
                .record_attention_resolution_action_accepted(
                    &target,
                    NotificationResolution::Complete,
                )
                .unwrap();
            service
                .record_attention_resolution_consumption_observed(&target, exact_consumption(23))
                .unwrap();
        }

        let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(directory(), store);
        assert!(service.prepare_oldest_notification(bob).unwrap().is_none());
        let target = service.attention_target(&attempt_id.to_string()).unwrap();
        assert_eq!(
            service
                .begin_attention_resolution(&target, NotificationResolution::Complete)
                .unwrap(),
            AttentionResolutionStart::ReconcileOnly
        );
        {
            let store = service.store().unwrap();
            assert!(store
                .projection()
                .notification(bob, &later_message_id)
                .is_none());
            assert_eq!(
                store
                    .projection()
                    .claimed_notification_barrier(bob)
                    .map(|record| record.attempt_id),
                Some(attempt_id)
            );
        }
        service
            .resolve_attention(&target, NotificationResolution::Complete)
            .unwrap();
        let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(next.message_id, later_message_id);
        assert_ne!(next.attempt_id, attempt_id);
    }

    #[test]
    fn blocked_binding_reopens_on_new_evidence_or_binding_and_keeps_fifo_identity() {
        let scratch = StoreScratch::new("blocked-binding-reopen");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let first = service
            .send(service.admin(), mailbox_send("reviewer", "First", ""))
            .unwrap();
        let second = service
            .send(service.admin(), mailbox_send("reviewer", "Second", ""))
            .unwrap();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            first.message_id.clone(),
            bob,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        let evidence = |generation| {
            Some(NotificationRouteEvidenceId {
                boot_id: "boot".into(),
                generation,
            })
        };
        let failed_observation = NotificationPreWriteObservation {
            pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
            selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
            binding: Some(notification_binding(bob)),
            route_evidence: evidence(7),
            pane_width: None,
            required_pane_width: None,
            write_block: None,
        };
        let lines_before_inner_schedule = service.journal_lines().unwrap().len();
        assert!(
            service
                .reopen_oldest_notification_after_route_evidence(
                    bob,
                    failed_observation.clone(),
                    false,
                )
                .unwrap()
                .is_none(),
            "the inner schedule must not move an attempt that is still Gating"
        );
        assert_eq!(
            service.journal_lines().unwrap().len(),
            lines_before_inner_schedule
        );

        // The durable block lands between the readiness schedule inside the
        // recompute and the event source's follow-on schedule. Both schedules
        // carry one evidence identity, so the second call is still a no-op.
        context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(failed_observation.clone()),
            )
            .unwrap();
        assert!(service.prepare_oldest_notification(bob).unwrap().is_none());

        let lines_before_repeat = service.journal_lines().unwrap().len();
        assert!(
            service
                .reopen_oldest_notification_after_route_evidence(
                    bob,
                    failed_observation.clone(),
                    false,
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);

        drop(context);
        drop(service);
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let replayed_store = MessageStore::open(&root, journal, workspace, "boot-replay").unwrap();
        let service = MailboxService::new(directory, replayed_store);
        let replayed_before_reopen = service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &first.message_id)
            .cloned()
            .unwrap();
        assert_eq!(
            replayed_before_reopen.state,
            NotificationState::BlockedPreWrite
        );
        assert_eq!(replayed_before_reopen.pre_write_reopen_count, 0);
        let lines_before_replayed_evidence = service.journal_lines().unwrap().len();
        assert!(
            service
                .reopen_oldest_notification_after_route_evidence(
                    bob,
                    failed_observation.clone(),
                    false,
                )
                .unwrap()
                .is_none()
        );
        let stale_observation = NotificationPreWriteObservation {
            route_evidence: evidence(6),
            ..failed_observation.clone()
        };
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, stale_observation, false)
            .unwrap()
            .is_none());
        assert_eq!(
            service.journal_lines().unwrap().len(),
            lines_before_replayed_evidence
        );

        let cross_pane_observation = NotificationPreWriteObservation {
            pane_root: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
            selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
            binding: Some(notification_binding(bob)),
            route_evidence: evidence(8),
            pane_width: None,
            required_pane_width: None,
            write_block: None,
        };
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, cross_pane_observation, false,)
            .unwrap()
            .is_none());
        let missing_leader_observation = NotificationPreWriteObservation {
            pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
            selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
            binding: Some(NotificationBinding {
                leader: None,
                ..notification_binding(bob)
            }),
            route_evidence: evidence(8),
            pane_width: None,
            required_pane_width: None,
            write_block: None,
        };
        assert!(
            service
                .reopen_oldest_notification_after_route_evidence(
                    bob,
                    missing_leader_observation,
                    false,
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);

        let proven_observation = NotificationPreWriteObservation {
            route_evidence: evidence(8),
            ..failed_observation.clone()
        };
        let reopened = service
            .reopen_oldest_notification_after_route_evidence(bob, proven_observation.clone(), false)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.message_id, first.message_id);
        assert_eq!(reopened.attempt_id, queued.attempt_id);
        assert_eq!(reopened.state, NotificationState::Gating);
        assert_eq!(
            service
                .prepare_oldest_notification(bob)
                .unwrap()
                .unwrap()
                .attempt_id,
            queued.attempt_id
        );
        assert_eq!(reopened.pre_write_reopen_count, 1);
        assert!(!service.journal_lines().unwrap().iter().any(|line| {
            line.data
                .as_ref()
                .is_some_and(|data| data["type"] == "notification_requeued")
        }));

        let reblocked_observation = NotificationPreWriteObservation {
            pane_root: failed_observation.pane_root,
            selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
            binding: None,
            route_evidence: evidence(8),
            pane_width: None,
            required_pane_width: None,
            write_block: None,
        };
        let reopened_context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            first.message_id.clone(),
            bob,
            queued.attempt_id,
        );
        reopened_context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(reblocked_observation),
            )
            .unwrap();
        let second_proof = NotificationPreWriteObservation {
            pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
            selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
            binding: Some(NotificationBinding {
                agent: ProcessInstanceId::new(4243, 818_222).unwrap(),
                ..notification_binding(bob)
            }),
            route_evidence: evidence(9),
            pane_width: None,
            required_pane_width: None,
            write_block: None,
        };
        let lines_before_second_proof = service.journal_lines().unwrap().len();
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, second_proof.clone(), false,)
            .unwrap()
            .is_none());
        assert_eq!(
            service.journal_lines().unwrap().len(),
            lines_before_second_proof
        );

        drop(reopened_context);
        drop(service);
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let reopened_store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(directory, reopened_store);
        let replayed = service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &first.message_id)
            .cloned()
            .unwrap();
        assert_eq!(replayed.state, NotificationState::BlockedPreWrite);
        assert_eq!(replayed.pre_write_reopen_count, 1);
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, second_proof, false)
            .unwrap()
            .is_none());

        service.claim(bob, first.message_id).unwrap();
        let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(next.message_id, second.message_id);
        assert_ne!(next.attempt_id, queued.attempt_id);

        let next_context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            second.message_id.clone(),
            bob,
            next.attempt_id,
        );
        next_context.record_gating().unwrap();
        next_context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(failed_observation.clone()),
            )
            .unwrap();
        let changed_binding = NotificationBinding {
            pane_root: Some(ProcessInstanceId::new(4999, 917_999).unwrap()),
            leader: Some(ProcessInstanceId::new(5000, 918_000).unwrap()),
            agent: ProcessInstanceId::new(5242, 918_221).unwrap(),
            ..notification_binding(bob)
        };
        let changed_observation = NotificationPreWriteObservation {
            write_block: None,
            pane_root: changed_binding.pane_root,
            selected_manifest: Some(changed_binding.manifest.clone()),
            binding: Some(changed_binding),
            route_evidence: evidence(8),
            pane_width: None,
            required_pane_width: None,
        };
        let reopened = service
            .reopen_oldest_notification_after_route_evidence(bob, changed_observation, false)
            .unwrap()
            .expect("a changed complete binding remains positive evidence");
        assert_eq!(reopened.message_id, second.message_id);
        assert_eq!(reopened.attempt_id, next.attempt_id);
        assert_eq!(reopened.pre_write_reopen_count, 1);
    }

    #[test]
    fn blocked_readiness_reopens_once_only_after_positive_exact_route_evidence() {
        let scratch = StoreScratch::new("blocked-readiness-reopen");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let message = service
            .send(service.admin(), mailbox_send("reviewer", "Ready", ""))
            .unwrap();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            message.message_id.clone(),
            bob,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        let evidence = |generation| {
            Some(NotificationRouteEvidenceId {
                boot_id: "boot".into(),
                generation,
            })
        };
        let blocked_observation = NotificationPreWriteObservation {
            pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
            selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
            binding: Some(notification_binding(bob)),
            route_evidence: evidence(7),
            pane_width: None,
            required_pane_width: None,
            write_block: None,
        };
        context
            .record_pre_write_block(
                NotificationPreWriteCause::WriteReadinessChanged,
                Some(blocked_observation.clone()),
            )
            .unwrap();

        let lines_before = service.journal_lines().unwrap().len();
        assert!(
            service
                .reopen_oldest_notification_after_route_evidence(
                    bob,
                    blocked_observation.clone(),
                    true,
                )
                .unwrap()
                .is_none()
        );
        let stale_observation = NotificationPreWriteObservation {
            route_evidence: evidence(6),
            ..blocked_observation.clone()
        };
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, stale_observation, true)
            .unwrap()
            .is_none());
        assert_eq!(service.journal_lines().unwrap().len(), lines_before);

        let later_observation = NotificationPreWriteObservation {
            route_evidence: evidence(8),
            ..blocked_observation
        };
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, later_observation.clone(), false,)
            .unwrap()
            .is_none());
        let reopened = service
            .reopen_oldest_notification_after_route_evidence(bob, later_observation.clone(), true)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.attempt_id, queued.attempt_id);
        assert_eq!(reopened.state, NotificationState::Gating);
        assert_eq!(reopened.pre_write_reopen_count, 1);

        let reopened_context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            message.message_id,
            bob,
            queued.attempt_id,
        );
        reopened_context
            .record_pre_write_block(
                NotificationPreWriteCause::WriteReadinessChanged,
                Some(later_observation.clone()),
            )
            .unwrap();
        let lines_before_repeat = service.journal_lines().unwrap().len();
        let final_observation = NotificationPreWriteObservation {
            route_evidence: evidence(9),
            ..later_observation
        };
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, final_observation, true)
            .unwrap()
            .is_none());
        assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);
    }

    #[test]
    fn worker_ownership_loss_is_journaled_and_live_projection_equals_replay() {
        let scratch = StoreScratch::new("scheduler-wake-block-replay");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let make_directory = || {
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                }],
            )
            .unwrap()
        };
        let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let service = MailboxService::new(make_directory(), store);
        let accepted = service
            .send(service.admin(), mailbox_send("reviewer", "Task", ""))
            .unwrap();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            accepted.message_id.clone(),
            bob,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_pre_write_block_with_wake_block(
                NotificationPreWriteCause::WorkerFailed,
                None,
                Some(MessageWakeBlock::WorkerSupervisorExited),
            )
            .unwrap();

        assert_eq!(
            service.notification_schedule_block(bob).unwrap(),
            Some(NotificationScheduleBlock {
                message_id: accepted.message_id.clone(),
                attempt_id: queued.attempt_id,
                block: MessageWakeBlock::WorkerSupervisorExited,
            })
        );
        let live_record = service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &accepted.message_id)
            .cloned()
            .unwrap();
        let snapshot = service.messages_snapshot(service.admin().key, 10).unwrap();
        let live_summary = snapshot.rows[0].recipients[0].notification.clone();
        assert_eq!(
            live_summary.wake_block,
            Some(MessageWakeBlock::WorkerSupervisorExited)
        );

        drop(context);
        drop(service);
        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(make_directory(), reopened);
        assert_eq!(
            service.notification_schedule_block(bob).unwrap(),
            Some(NotificationScheduleBlock {
                message_id: accepted.message_id.clone(),
                attempt_id: queued.attempt_id,
                block: MessageWakeBlock::WorkerSupervisorExited,
            })
        );
        let replayed_record = service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &accepted.message_id)
            .cloned()
            .unwrap();
        assert_eq!(replayed_record, live_record);
        let snapshot = service.messages_snapshot(service.admin().key, 10).unwrap();
        assert_eq!(snapshot.rows[0].recipients[0].notification, live_summary);
        assert_eq!(
            snapshot.rows[0].recipients[0].notification.wake_block,
            Some(MessageWakeBlock::WorkerSupervisorExited)
        );
    }

    #[test]
    fn operator_withdrawal_is_durable_idempotent_and_advances_notification_fifo() {
        let scratch = StoreScratch::new("operator-prewrite-withdrawal");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _carol) = test_context();
        let make_directory = || {
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                }],
            )
            .unwrap()
        };
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(make_directory(), store);
        let first = service
            .send(
                service.admin(),
                mailbox_send("reviewer", "First", "claimable body"),
            )
            .unwrap();
        let second = service
            .send(service.admin(), mailbox_send("reviewer", "Second", ""))
            .unwrap();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            first.message_id.clone(),
            bob,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(NotificationPreWriteObservation {
                    pane_root: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
                    selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                    binding: None,
                    route_evidence: None,
                    pane_width: None,
                    required_pane_width: None,
                    write_block: None,
                }),
            )
            .unwrap();

        let admin_blocked = service.messages_snapshot(admin, 10).unwrap();
        let blocked_row = admin_blocked
            .rows
            .iter()
            .find(|row| row.message_id == first.message_id)
            .unwrap();
        assert!(blocked_row.needs_action);
        assert_eq!(blocked_row.recipients[0].fifo_position, Some(1));
        assert!(blocked_row.recipients[0].can_withdraw_notification);
        assert_eq!(
            blocked_row.recipients[0]
                .current_route
                .as_ref()
                .map(|route| route.label.as_str()),
            Some("reviewer")
        );
        assert_eq!(
            blocked_row.recipients[0].notification.pre_write_cause,
            Some(NotificationPreWriteCause::BindingUnprovable)
        );
        let status_blocked = service.blocked_notification_snapshot(now_ms(), 32).unwrap();
        assert_eq!(status_blocked.total, 1);
        assert_eq!(status_blocked.rows.len(), 1);
        assert_eq!(
            status_blocked.rows[0].notification_attempt,
            queued.attempt_id
        );
        let recipient_blocked = service.messages_snapshot(bob, 10).unwrap();
        assert!(recipient_blocked.rows[0].needs_action);
        assert!(!recipient_blocked.rows[0].recipients[0].can_withdraw_notification);

        let before = service.journal_lines().unwrap().len();
        let (withdrawn, inserted) = service
            .withdraw_notification_before_write(admin, bob, queued.attempt_id)
            .unwrap();
        assert!(inserted);
        assert_eq!(withdrawn.state, NotificationState::WithdrawnByOperator);
        assert_eq!(service.journal_lines().unwrap().len(), before + 1);
        let line = service.journal_lines().unwrap().pop().unwrap();
        assert_eq!(line.from, admin.to_string());
        assert_eq!(line.to, vec![bob.to_string()]);
        assert!(line.subject.is_none() && line.body.is_none() && line.reply_to.is_none());
        assert!(line.deliveries.is_empty());
        assert_eq!(
            line.data.as_ref().unwrap()["type"],
            "notification_withdrawn_before_write"
        );

        let snapshot = service.messages_snapshot(admin, 10).unwrap();
        let first_notification = &snapshot
            .rows
            .iter()
            .find(|row| row.message_id == first.message_id)
            .unwrap()
            .recipients[0]
            .notification;
        assert_eq!(
            first_notification.state,
            MessageNotificationState::NotStarted
        );
        assert_eq!(first_notification.settlement, None);
        assert_eq!(first_notification.operator_withdrawn, Some(true));
        let first_recipient = &snapshot
            .rows
            .iter()
            .find(|row| row.message_id == first.message_id)
            .unwrap()
            .recipients[0];
        assert!(!first_recipient.can_withdraw_notification);
        assert_eq!(
            first_recipient.fifo_position, None,
            "the still-pending, withdrawn mailbox entry is pullable but no longer a notification FIFO item"
        );
        let second_recipient = &snapshot
            .rows
            .iter()
            .find(|row| row.message_id == second.message_id)
            .unwrap()
            .recipients[0];
        assert_eq!(
            second_recipient.fifo_position,
            Some(1),
            "the next actionable wake is first after the withdrawn head"
        );
        let first_disposition = service.message_dispositions(&first.message_id).unwrap();
        assert_eq!(
            first_disposition[0].position_ahead, None,
            "a withdrawn wake has no notification queue position"
        );
        let second_disposition = service.message_dispositions(&second.message_id).unwrap();
        assert_eq!(
            second_disposition[0].position_ahead,
            Some(0),
            "sender receipts count only actionable wakes ahead"
        );
        let status_blocked = service.blocked_notification_snapshot(now_ms(), 32).unwrap();
        assert_eq!(status_blocked.total, 0);
        assert!(status_blocked.rows.is_empty());
        assert!(
            !snapshot
                .rows
                .iter()
                .find(|row| row.message_id == first.message_id)
                .unwrap()
                .needs_action
        );

        let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(next.message_id, second.message_id);
        assert_ne!(next.attempt_id, queued.attempt_id);
        let before_repeat = service.journal_lines().unwrap().len();
        let (_, inserted) = service
            .withdraw_notification_before_write(admin, bob, queued.attempt_id)
            .unwrap();
        assert!(!inserted);
        assert_eq!(service.journal_lines().unwrap().len(), before_repeat);
        assert!(service
            .reopen_oldest_notification_after_route_evidence(
                bob,
                NotificationPreWriteObservation {
                    pane_root: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
                    selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                    binding: Some(notification_binding(bob)),
                    route_evidence: None,
                    pane_width: None,
                    required_pane_width: None,
                    write_block: None,
                },
                true,
            )
            .unwrap()
            .is_none());

        drop(context);
        drop(service);
        let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(make_directory(), store);
        let before_restart_repeat = service.journal_lines().unwrap().len();
        let (_, inserted) = service
            .withdraw_notification_before_write(admin, bob, queued.attempt_id)
            .unwrap();
        assert!(!inserted);
        assert_eq!(
            service.journal_lines().unwrap().len(),
            before_restart_repeat
        );
        assert_eq!(
            service
                .store()
                .unwrap()
                .projection()
                .notification(bob, &second.message_id)
                .unwrap()
                .attempt_id,
            next.attempt_id
        );

        let ClaimOutcome::Claimed { message, .. } =
            service.claim(bob, first.message_id.clone()).unwrap()
        else {
            panic!("the withdrawn wake must not consume the message");
        };
        assert_eq!(message.body.as_deref(), Some("claimable body"));
        assert_eq!(
            service
                .store()
                .unwrap()
                .projection()
                .notification(bob, &first.message_id)
                .unwrap()
                .state,
            NotificationState::WithdrawnByOperator
        );
    }

    #[test]
    fn operator_withdraws_queued_and_gating_wakes_without_promoting_them_to_work() {
        let scratch = StoreScratch::new("operator-unwritten-withdrawal");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let make_directory = || {
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                }],
            )
            .unwrap()
        };
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(make_directory(), store);
        let queued_message = service
            .send(service.admin(), mailbox_send("reviewer", "Queued", ""))
            .unwrap();
        let gating_message = service
            .send(service.admin(), mailbox_send("reviewer", "Gating", ""))
            .unwrap();

        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let snapshot = service.messages_snapshot(admin, 10).unwrap();
        let queued_row = snapshot
            .rows
            .iter()
            .find(|row| row.message_id == queued_message.message_id)
            .unwrap();
        assert!(queued_row.recipients[0].can_withdraw_notification);
        assert!(!queued_row.needs_action);
        assert_eq!(snapshot.counts.work_messages, 0);

        service
            .withdraw_notification_before_write(admin, bob, queued.attempt_id)
            .unwrap();
        let gating = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(gating.message_id, gating_message.message_id);
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            gating.message_id.clone(),
            bob,
            gating.attempt_id,
        );
        context.record_gating().unwrap();

        let snapshot = service.messages_snapshot(admin, 10).unwrap();
        let gating_row = snapshot
            .rows
            .iter()
            .find(|row| row.message_id == gating_message.message_id)
            .unwrap();
        assert!(gating_row.recipients[0].can_withdraw_notification);
        assert!(!gating_row.needs_action);
        assert_eq!(snapshot.counts.work_messages, 0);

        service
            .withdraw_notification_before_write(admin, bob, gating.attempt_id)
            .unwrap();
        drop(context);
        drop(service);

        let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(make_directory(), store);
        for message_id in [&queued_message.message_id, &gating_message.message_id] {
            assert_eq!(
                service
                    .store()
                    .unwrap()
                    .projection()
                    .notification(bob, message_id)
                    .unwrap()
                    .state,
                NotificationState::WithdrawnByOperator
            );
        }
    }

    #[test]
    fn blocked_status_sample_ignores_unrelated_message_volume() {
        let scratch = StoreScratch::new("blocked-status-unrelated-volume");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let blocked = service
            .send(
                service.admin(),
                mailbox_send("reviewer", "Blocked", "secret"),
            )
            .unwrap();
        let attempt = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            blocked.message_id.clone(),
            bob,
            attempt.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(NotificationPreWriteObservation {
                    pane_root: None,
                    selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                    binding: None,
                    route_evidence: None,
                    pane_width: None,
                    required_pane_width: None,
                    write_block: None,
                }),
            )
            .unwrap();

        let mut unrelated = None;
        for index in 0..128 {
            unrelated = Some(
                service
                    .send(
                        service.admin(),
                        mailbox_send("reviewer", &format!("Other {index}"), "other secret"),
                    )
                    .unwrap()
                    .message_id,
            );
        }
        // Corrupt an unrelated message's presentation metadata in memory.
        // A full message snapshot must inspect it and fail. The specialized
        // status query is driven only by current blocked notifications.
        service
            .store()
            .unwrap()
            .projection
            .messages
            .get_mut(&unrelated.unwrap())
            .unwrap()
            .data = None;

        let sample = service.blocked_notification_snapshot(now_ms(), 32).unwrap();
        assert_eq!(sample.total, 1);
        assert_eq!(sample.rows.len(), 1);
        assert_eq!(sample.rows[0].message_id, blocked.message_id);
        assert!(service.messages_snapshot(service.admin().key, 0).is_err());
    }

    #[test]
    fn blocked_status_sample_is_capped_and_deterministic() {
        let scratch = StoreScratch::new("blocked-status-cap");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let session = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let recipients: Vec<_> = (1..=7)
            .map(|index| {
                RecipientKey::agent(
                    workspace,
                    session,
                    TmuxPaneId::from_str(&format!("%{index}")).unwrap(),
                )
            })
            .collect();
        let directory = MailboxDirectory::new(
            workspace,
            recipients
                .iter()
                .enumerate()
                .map(|(index, recipient)| MailboxIdentity {
                    key: *recipient,
                    label: format!("agent-{index}"),
                }),
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let message = service
            .send(
                service.admin(),
                MailboxSend {
                    addresses: vec!["*".into()],
                    recipient_keys: None,
                    subject: "Broadcast".into(),
                    body: String::new(),
                    fyi: false,
                    client_key: None,
                    supersedes: None,
                },
            )
            .unwrap();
        for recipient in recipients {
            let attempt = service
                .prepare_oldest_notification(recipient)
                .unwrap()
                .unwrap();
            let context = crate::notification_adapter::NotificationContext::new(
                service.store_handle(),
                message.message_id.clone(),
                recipient,
                attempt.attempt_id,
            );
            context.record_gating().unwrap();
            context
                .record_pre_write_block(
                    NotificationPreWriteCause::BindingUnprovable,
                    Some(NotificationPreWriteObservation {
                        pane_root: None,
                        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                        binding: None,
                        route_evidence: None,
                        pane_width: None,
                        required_pane_width: None,
                        write_block: None,
                    }),
                )
                .unwrap();
        }

        let now = now_ms().saturating_add(100);
        let first = service.blocked_notification_snapshot(now, 4).unwrap();
        let second = service.blocked_notification_snapshot(now, 4).unwrap();
        assert_eq!(first.total, 7);
        assert_eq!(first.rows.len(), 4);
        assert_eq!(first.rows, second.rows);
        assert!(first.rows.iter().all(|row| {
            row.recipient.fifo_position == Some(1)
                && row.recipient.current_route.is_some()
                && row.recipient.can_withdraw_notification
        }));
    }

    #[test]
    fn operator_withdrawal_refuses_inexact_or_post_write_targets_without_appending() {
        let scratch = StoreScratch::new("operator-prewrite-withdrawal-refusals");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "implementer".into(),
                },
            ],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let first = service
            .send(service.admin(), mailbox_send("reviewer", "First", ""))
            .unwrap();
        let bob_attempt = service.prepare_oldest_notification(bob).unwrap().unwrap();

        let assert_refused_without_append = |result: Result<_, MailboxServiceError>| {
            assert!(result.is_err());
        };
        let before = service.journal_lines().unwrap().len();
        assert_refused_without_append(service.withdraw_notification_before_write(
            bob,
            bob,
            bob_attempt.attempt_id,
        ));
        assert_refused_without_append(service.withdraw_notification_before_write(
            admin,
            carol,
            bob_attempt.attempt_id,
        ));
        assert_refused_without_append(service.withdraw_notification_before_write(
            admin,
            bob,
            attempt(999),
        ));
        assert_eq!(service.journal_lines().unwrap().len(), before);

        service.claim(bob, first.message_id).unwrap();
        let before_claimed = service.journal_lines().unwrap().len();
        assert_refused_without_append(service.withdraw_notification_before_write(
            admin,
            bob,
            bob_attempt.attempt_id,
        ));
        assert_eq!(service.journal_lines().unwrap().len(), before_claimed);

        let post_write = service
            .send(service.admin(), mailbox_send("implementer", "Writing", ""))
            .unwrap();
        let carol_attempt = service.prepare_oldest_notification(carol).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            post_write.message_id,
            carol,
            carol_attempt.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_writing(
                ProcessInstanceId::new(4999, 899_999).unwrap(),
                ProcessInstanceId::new(5000, 900_000).unwrap(),
                ProcessInstanceId::new(5001, 900_001).unwrap(),
                "codex",
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            )
            .unwrap();
        let before_writing = service.journal_lines().unwrap().len();
        assert_refused_without_append(service.withdraw_notification_before_write(
            admin,
            carol,
            carol_attempt.attempt_id,
        ));
        assert_eq!(service.journal_lines().unwrap().len(), before_writing);
    }

    #[test]
    fn sender_filter_runs_before_the_oldest_message_limit() {
        let scratch = StoreScratch::new("sender-filter-before-limit");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let reviewer = service
            .identity_for_recipient(bob)
            .unwrap()
            .expect("reviewer identity");
        let admin = service.admin();

        service
            .send(
                reviewer,
                mailbox_send("reviewer", "Older self message", "private"),
            )
            .unwrap();
        service
            .send(
                admin.clone(),
                mailbox_send("reviewer", "Newer admin message", "private"),
            )
            .unwrap();

        let listed = service.list(bob, Some(admin.key), Some(1)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].sender, admin.key);
        assert_eq!(listed[0].subject.as_deref(), Some("Newer admin message"));
    }

    #[test]
    fn concurrent_senders_share_one_oldest_notification_attempt() {
        let scratch = StoreScratch::new("concurrent-oldest-notification");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = Arc::new(MailboxService::new(directory, store));
        let admin = service.admin();
        let reviewer = service
            .identity_for_recipient(bob)
            .unwrap()
            .expect("reviewer identity");
        let start = Arc::new(std::sync::Barrier::new(3));

        let send = |sender: MailboxIdentity, subject: &'static str| {
            let service = Arc::clone(&service);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                service
                    .send(sender, mailbox_send("reviewer", subject, "private"))
                    .unwrap()
            })
        };
        let from_admin = send(admin, "From admin");
        let from_reviewer = send(reviewer, "From reviewer");
        start.wait();
        let mut accepted = [from_admin.join().unwrap(), from_reviewer.join().unwrap()];
        accepted.sort_by_key(|result| result.seq);
        assert!(accepted[0].seq < accepted[1].seq);

        let prepare = Arc::new(std::sync::Barrier::new(3));
        let notify = || {
            let service = Arc::clone(&service);
            let prepare = Arc::clone(&prepare);
            std::thread::spawn(move || {
                prepare.wait();
                service.prepare_oldest_notification(bob).unwrap().unwrap()
            })
        };
        let first_prepare = notify();
        let second_prepare = notify();
        prepare.wait();
        let oldest = first_prepare.join().unwrap();
        let concurrent = second_prepare.join().unwrap();
        assert_eq!(concurrent.attempt_id, oldest.attempt_id);
        assert_eq!(concurrent.message_id, oldest.message_id);
        assert_eq!(oldest.message_id, accepted[0].message_id);
        let lines_after_first_attempt = service.journal_lines().unwrap().len();
        let same = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(same.attempt_id, oldest.attempt_id);
        assert_eq!(
            service.journal_lines().unwrap().len(),
            lines_after_first_attempt
        );
        assert!(service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &accepted[1].message_id)
            .is_none());

        let ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = service.claim(bob, accepted[0].message_id.clone()).unwrap()
        else {
            panic!("oldest message was not freshly claimed");
        };
        assert_eq!(withdrawn_attempt, Some(oldest.attempt_id));
        let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(next.message_id, accepted[1].message_id);
        assert_ne!(next.attempt_id, oldest.attempt_id);

        let store = service.store().unwrap();
        assert_eq!(
            store
                .projection()
                .notification(bob, &accepted[0].message_id)
                .unwrap()
                .state,
            NotificationState::Withdrawn
        );
        assert_eq!(
            store
                .projection()
                .notification(bob, &accepted[1].message_id)
                .unwrap()
                .state,
            NotificationState::Queued
        );
    }

    #[test]
    fn claim_keeps_post_write_attention_open_in_its_own_fact() {
        let scratch = StoreScratch::new("claim-withdrawal");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let service = MailboxService::new(directory, store);
        let accepted = service
            .send(service.admin(), mailbox_send("reviewer", "Task", "Body"))
            .unwrap();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        {
            let mut store = service.store().unwrap();
            alarm(&mut store, &accepted.message_id, bob, queued.attempt_id, 3);
        }

        let ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = service.claim(bob, accepted.message_id.clone()).unwrap()
        else {
            panic!("first claim must append a claim fact");
        };
        assert_eq!(withdrawn_attempt, None);
        let lines = service.journal_lines().unwrap();
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[6].data.as_ref().unwrap()["type"], "message_claimed");
        let attention = service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &accepted.message_id)
            .cloned()
            .unwrap();
        assert_eq!(attention.state, NotificationState::AttentionRequired);
        assert_eq!(attention.updated_seq, lines[5].seq);
        assert_eq!(
            attention.cause,
            Some(NotificationAttentionCause::SubmitFailed)
        );
        assert_eq!(service.store().unwrap().projection().open_alarms().len(), 1);
        assert_eq!(
            service
                .store()
                .unwrap()
                .projection()
                .active_notification_barriers()
                .len(),
            1
        );
        let admin_snapshot = service.messages_snapshot(service.admin().key, 10).unwrap();
        let recipient = &admin_snapshot
            .rows
            .iter()
            .find(|row| row.message_id == accepted.message_id)
            .unwrap()
            .recipients[0];
        assert_eq!(
            recipient.notification.state,
            MessageNotificationState::AttentionRequired
        );
        assert!(recipient.can_manage_attention);
        assert_eq!(
            service
                .attention_target(&queued.attempt_id.to_string())
                .unwrap()
                .record
                .attempt_id,
            queued.attempt_id
        );

        drop(service);
        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert_eq!(
            reopened
                .projection()
                .notification(bob, &accepted.message_id)
                .unwrap()
                .state,
            NotificationState::AttentionRequired
        );
        assert_eq!(reopened.projection().open_alarms().len(), 1);
        assert_eq!(
            reopened.projection().active_notification_barriers().len(),
            1
        );
    }

    #[test]
    fn claim_publishes_only_the_mailbox_when_attention_stays_open() {
        let scratch = StoreScratch::new("claim-attention-change");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let (sender, _) = broadcast::channel(8);
        let mut events = sender.subscribe();
        let service = MailboxService::new_with_events(directory, store, sender);
        let accepted = service
            .send(service.admin(), mailbox_send("reviewer", "Task", "Body"))
            .unwrap();
        next_change(
            &mut events,
            1,
            &[
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ],
        );
        {
            let mut store = service.store().unwrap();
            alarm(&mut store, &accepted.message_id, bob, attempt(1), 2);
        }
        let claim_seq = service
            .store()
            .unwrap()
            .projection()
            .last_sequence()
            .unwrap()
            + 1;

        service.claim(bob, accepted.message_id).unwrap();
        next_change(&mut events, claim_seq, &[MessagesChangedArea::Mailboxes]);
        assert_eq!(service.store().unwrap().projection().open_alarms().len(), 1);
    }

    fn draft(
        sender: RecipientKey,
        recipients: Vec<RecipientKey>,
        body: &str,
        client_key: Option<&str>,
    ) -> MessageDraft {
        let presentation = test_presentation(&recipients);
        MessageDraft {
            kind: Kind::Msg,
            sender,
            recipients,
            subject: Some("Task".into()),
            body: Some(body.into()),
            client_key: client_key.map(str::to_string),
            supersedes: None,
            presentation,
        }
    }

    fn reply_draft(sender: RecipientKey, reference: MessageId, body: &str) -> ReplyDraft {
        ReplyDraft {
            sender,
            reference,
            body: Some(body.into()),
            client_key: None,
            sender_label: "reply-sender".into(),
        }
    }

    fn test_presentation(recipients: &[RecipientKey]) -> MessagePresentation {
        MessagePresentation {
            sender_label: "sender-label".into(),
            recipient_labels: recipients
                .iter()
                .enumerate()
                .map(|(index, recipient)| RecipientPresentation {
                    recipient: *recipient,
                    label: format!("recipient-{index}"),
                })
                .collect(),
        }
    }

    fn assert_no_key(value: &serde_json::Value, forbidden: &str) {
        match value {
            serde_json::Value::Object(fields) => {
                assert!(
                    !fields.contains_key(forbidden),
                    "found forbidden key {forbidden}"
                );
                for value in fields.values() {
                    assert_no_key(value, forbidden);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    assert_no_key(value, forbidden);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn outbound_snapshot_survives_reopen_without_body_keys() {
        let scratch = StoreScratch::new("snapshot-reopen");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, carol) = test_context();
        let message_id = MessageId::new("m-outbound").unwrap();

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(bob, vec![carol], "secret body", None),
                    100,
                )
                .unwrap();
            let snapshot = store
                .projection()
                .messages_snapshot(bob, 20, &routes([bob, carol]))
                .unwrap();
            assert_eq!(snapshot.rows[0].direction, MessageDirection::Outbound);
            let json = serde_json::to_value(&snapshot).unwrap();
            for forbidden in ["body", "binding", "capture", "composer", "diff"] {
                assert_no_key(&json, forbidden);
            }
            assert!(!json.to_string().contains("secret body"));
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let snapshot = reopened
            .projection()
            .messages_snapshot(bob, 20, &routes([bob, carol]))
            .unwrap();
        assert_eq!(snapshot.workspace_seq, 1);
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].message_id, message_id);
        assert_eq!(snapshot.rows[0].direction, MessageDirection::Outbound);
    }

    #[test]
    fn snapshot_retains_claims_and_recomputes_fifo_positions() {
        let scratch = StoreScratch::new("snapshot-claim-fifo");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let first = MessageId::new("m-first").unwrap();
        let second = MessageId::new("m-second").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(first.clone(), draft(admin, vec![bob], "first", None), 100)
            .unwrap();
        store
            .accept_at(second.clone(), draft(admin, vec![bob], "second", None), 200)
            .unwrap();

        let before = store
            .projection()
            .messages_snapshot(bob, 20, &routes([bob]))
            .unwrap();
        assert_eq!(before.rows[0].recipients[0].fifo_position, Some(1));
        assert_eq!(before.rows[1].recipients[0].fifo_position, Some(2));
        assert_eq!(
            before.rows[0].recipients[0].notification.state,
            MessageNotificationState::NotStarted
        );
        assert!(before.rows[0].recipients[0]
            .notification
            .attempt_id
            .is_none());

        store.claim_at(bob, first.clone(), 300).unwrap();
        let after = store
            .projection()
            .messages_snapshot(bob, 20, &routes([bob]))
            .unwrap();
        let first_row = after
            .rows
            .iter()
            .find(|row| row.message_id == first)
            .unwrap();
        let second_row = after
            .rows
            .iter()
            .find(|row| row.message_id == second)
            .unwrap();
        assert!(first_row.recipients[0].mailbox.is_claimed());
        assert_eq!(first_row.recipients[0].fifo_position, None);
        assert_eq!(second_row.recipients[0].fifo_position, Some(1));
        assert_eq!(after.counts.pending_entries, 1);
        assert_eq!(after.counts.claimed_entries, 1);
        assert_eq!(after.workspace_seq, 3);
    }

    #[test]
    fn broadcast_snapshot_keeps_each_recipient_attempt_and_clearance() {
        let scratch = StoreScratch::new("snapshot-broadcast");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-broadcast").unwrap();
        let bob_attempt = attempt(1);
        let carol_attempt = attempt(2);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "broadcast", None),
                100,
            )
            .unwrap();

        store
            .queue_notification(message_id.clone(), bob, bob_attempt)
            .unwrap();
        for (state, binding) in [
            (NotificationState::Gating, None),
            (NotificationState::Writing, Some(notification_binding(bob))),
            (NotificationState::Staged, None),
            (NotificationState::Submitting, None),
            (NotificationState::Submitted, None),
            (NotificationState::Notified, None),
        ] {
            store
                .advance_notification(message_id.clone(), bob, bob_attempt, state, binding, None)
                .unwrap();
        }
        store.claim(bob, message_id.clone()).unwrap();

        store
            .queue_notification(message_id.clone(), carol, carol_attempt)
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                carol,
                carol_attempt,
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                carol,
                carol_attempt,
                NotificationState::Writing,
                Some(notification_binding(carol)),
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                carol,
                carol_attempt,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
            )
            .unwrap();
        store
            .clear_notification(message_id.clone(), carol, carol_attempt)
            .unwrap();

        let snapshot = store
            .projection()
            .messages_snapshot(admin, 20, &routes([admin, bob, carol]))
            .unwrap();
        let row = &snapshot.rows[0];
        let bob_state = row
            .recipients
            .iter()
            .find(|recipient| recipient.recipient == bob)
            .unwrap();
        let carol_state = row
            .recipients
            .iter()
            .find(|recipient| recipient.recipient == carol)
            .unwrap();
        assert!(bob_state.mailbox.is_claimed());
        assert_eq!(
            bob_state.notification.state,
            MessageNotificationState::Notified
        );
        assert_eq!(bob_state.notification.attempt_id, Some(bob_attempt));
        assert!(carol_state.mailbox.is_pending());
        assert_eq!(carol_state.fifo_position, Some(1));
        assert_eq!(
            carol_state.notification.state,
            MessageNotificationState::AttentionRequired
        );
        assert_eq!(
            carol_state.notification.cause,
            Some(NotificationAttentionCause::VerifyFailed)
        );
        assert_eq!(carol_state.notification.attention_cleared, Some(true));
        assert_eq!(snapshot.counts.open_attention_entries, 0);
    }

    #[test]
    fn snapshot_denies_nonparticipant_visibility_at_the_projection_boundary() {
        let (workspace, _, bob, carol) = test_context();
        let other_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let dave = RecipientKey::agent(
            workspace,
            other_session,
            TmuxPaneId::from_str("%3").unwrap(),
        );
        let mut projection = MailboxProjection::new(workspace);
        projection
            .apply_line(&sample_msg_line(
                1,
                "m-private",
                workspace,
                bob,
                vec![carol],
                Kind::Msg,
                None,
                "private body",
            ))
            .unwrap();

        assert_eq!(
            projection
                .messages_snapshot(bob, 20, &routes([bob, carol]))
                .unwrap()
                .rows
                .len(),
            1
        );
        assert_eq!(
            projection
                .messages_snapshot(carol, 20, &routes([bob, carol]))
                .unwrap()
                .rows
                .len(),
            1
        );
        let denied = projection
            .messages_snapshot(dave, 20, &routes([bob, carol, dave]))
            .unwrap();
        assert!(denied.rows.is_empty());
        assert_eq!(denied.counts.visible_messages, 0);
        let admin = RecipientKey::admin(workspace);
        assert_eq!(
            projection
                .messages_snapshot(admin, 20, &routes([bob, carol]))
                .unwrap()
                .rows
                .len(),
            1
        );
    }

    #[test]
    fn snapshot_bounds_settled_rows_without_hiding_thread_counts() {
        let scratch = StoreScratch::new("snapshot-settled-bound");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, carol) = test_context();
        let root_id = MessageId::new("m-thread-root").unwrap();
        let reply_id = MessageId::new("m-thread-reply").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(root_id.clone(), draft(bob, vec![carol], "root", None), 100)
            .unwrap();
        store.claim_at(carol, root_id.clone(), 200).unwrap();
        store
            .reply_at(
                reply_id.clone(),
                reply_draft(carol, root_id.clone(), "reply"),
                300,
            )
            .unwrap();
        store.claim_at(bob, reply_id.clone(), 400).unwrap();

        let snapshot = store
            .projection()
            .messages_snapshot(bob, 1, &routes([bob, carol]))
            .unwrap();
        assert_eq!(snapshot.counts.visible_messages, 2);
        assert_eq!(snapshot.counts.settled_messages, 2);
        assert_eq!(snapshot.counts.returned_messages, 1);
        assert_eq!(snapshot.counts.inbox_messages, 1);
        assert_eq!(snapshot.counts.outbound_messages, 1);
        assert_eq!(snapshot.counts.work_messages, 0);
        assert_eq!(snapshot.rows[0].message_id, reply_id);
        assert_eq!(snapshot.rows[0].thread_root, root_id);
        assert_eq!(snapshot.rows[0].thread_message_count, 2);
    }

    #[test]
    fn follow_pages_every_settled_message_beyond_the_snapshot_tail() {
        let scratch = StoreScratch::new("follow-settled-burst");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let mut expected = Vec::new();
        for index in 0..25 {
            let message_id = MessageId::new(format!("m-burst-{index:02}")).unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "settled", None),
                    100 + index,
                )
                .unwrap();
            store
                .claim_at(bob, message_id.clone(), 200 + index)
                .unwrap();
            expected.push(message_id);
        }

        let queue = store
            .projection()
            .messages_snapshot(admin, 20, &routes([admin, bob]))
            .unwrap();
        assert_eq!(queue.rows.len(), 20, "the queue tail stays bounded");

        let mut cursor = 0;
        let mut followed = Vec::new();
        loop {
            let page = store
                .projection()
                .messages_follow(admin, cursor, 7, &routes([admin, bob]))
                .unwrap();
            assert_eq!(page.after_seq, cursor);
            followed.extend(page.rows.iter().map(|row| row.message_id.clone()));
            cursor = page.through_seq;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(followed, expected);
        assert_eq!(cursor, queue.workspace_seq);
    }

    #[test]
    fn ten_thousand_message_snapshot_uses_the_mailbox_lookup_index() {
        const MESSAGE_COUNT: u64 = 10_000;
        const NOTIFICATION_COUNT: u64 = 100;

        let (workspace, admin, bob, _) = test_context();
        let mut projection = MailboxProjection::new(workspace);
        let mut seq = 0_u64;

        for number in 0..MESSAGE_COUNT {
            seq += 1;
            let id = format!("m-scale-{number:05}");
            projection
                .apply_line(&sample_msg_line(
                    seq,
                    &id,
                    workspace,
                    admin,
                    vec![bob],
                    Kind::Msg,
                    None,
                    "body",
                ))
                .unwrap();
        }

        for number in (0..MESSAGE_COUNT).step_by(2) {
            seq += 1;
            let id = MessageId::new(format!("m-scale-{number:05}")).unwrap();
            projection
                .apply_line(&sample_claim_line(seq, id, bob))
                .unwrap();
        }

        for number in (1..(NOTIFICATION_COUNT * 2)).step_by(2) {
            seq += 1;
            let id = MessageId::new(format!("m-scale-{number:05}")).unwrap();
            projection
                .apply_line(&sample_queued_notification_line(
                    seq,
                    id,
                    bob,
                    attempt(number),
                ))
                .unwrap();
        }

        // Each index value points into the authoritative FIFO map. The index
        // carries no mailbox state and keeps point reads out of a linear scan.
        assert_eq!(projection.mailbox_index.len(), MESSAGE_COUNT as usize);
        let last_id = MessageId::new("m-scale-09999").unwrap();
        assert_eq!(projection.get_entry(bob, &last_id).unwrap().seq, 10_000);

        let snapshot = projection
            .messages_snapshot(bob, 20, &routes([bob]))
            .unwrap();
        assert_eq!(snapshot.counts.visible_messages, MESSAGE_COUNT);
        assert_eq!(snapshot.counts.pending_entries, MESSAGE_COUNT / 2);
        assert_eq!(snapshot.counts.claimed_entries, MESSAGE_COUNT / 2);
        assert_eq!(snapshot.counts.work_messages, MESSAGE_COUNT / 2);
        assert_eq!(snapshot.counts.returned_messages, MESSAGE_COUNT / 2 + 20);
        assert_eq!(
            snapshot
                .rows
                .iter()
                .filter(|row| {
                    row.recipients[0].notification.state == MessageNotificationState::Queued
                })
                .count(),
            NOTIFICATION_COUNT as usize
        );
    }

    #[test]
    fn work_is_pending_for_an_agent_and_uncleared_attention_for_admin() {
        let scratch = StoreScratch::new("snapshot-work");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-work").unwrap();
        let attempt_id = attempt(9);
        let available = routes([admin, bob]);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "work", None),
                100,
            )
            .unwrap();

        let agent = store
            .projection()
            .messages_snapshot(bob, 20, &available)
            .unwrap();
        let admin_before = store
            .projection()
            .messages_snapshot(admin, 20, &available)
            .unwrap();
        assert_eq!(agent.counts.work_messages, 1);
        assert!(agent.rows[0].needs_action);
        assert!(!agent.rows[0].recipients[0].can_manage_attention);
        assert_eq!(admin_before.counts.work_messages, 0);
        assert!(!admin_before.rows[0].needs_action);
        assert!(!admin_before.rows[0].recipients[0].can_manage_attention);

        store
            .queue_notification(message_id.clone(), bob, attempt_id)
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                Some(notification_binding(bob)),
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
            )
            .unwrap();

        let admin_attention = store
            .projection()
            .messages_snapshot(admin, 20, &available)
            .unwrap();
        assert_eq!(admin_attention.counts.work_messages, 1);
        assert!(admin_attention.rows[0].needs_action);
        assert!(admin_attention.rows[0].recipients[0].can_manage_attention);

        store
            .clear_notification(message_id, bob, attempt_id)
            .unwrap();
        let admin_cleared = store
            .projection()
            .messages_snapshot(admin, 20, &available)
            .unwrap();
        assert_eq!(admin_cleared.counts.work_messages, 0);
        assert!(!admin_cleared.rows[0].needs_action);
        assert!(!admin_cleared.rows[0].recipients[0].can_manage_attention);
    }

    #[test]
    fn recipient_work_and_direction_do_not_spread_across_a_broadcast() {
        let scratch = StoreScratch::new("snapshot-broadcast-rows");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-broadcast-rows").unwrap();
        let available = routes([admin, bob, carol]);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "broadcast", None),
                100,
            )
            .unwrap();

        let bob_snapshot = store
            .projection()
            .messages_snapshot(bob, 20, &available)
            .unwrap();
        let bob_row = &bob_snapshot.rows[0];
        let bob_entry = bob_row
            .recipients
            .iter()
            .find(|entry| entry.recipient == bob)
            .unwrap();
        let carol_entry = bob_row
            .recipients
            .iter()
            .find(|entry| entry.recipient == carol)
            .unwrap();
        assert_eq!(bob_entry.direction, MessageDirection::Inbound);
        assert!(bob_entry.needs_action);
        assert_eq!(carol_entry.direction, MessageDirection::Workspace);
        assert!(!carol_entry.needs_action);

        let attempt_id = attempt(10);
        store
            .queue_notification(message_id.clone(), carol, attempt_id)
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                carol,
                attempt_id,
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                carol,
                attempt_id,
                NotificationState::Writing,
                Some(notification_binding(carol)),
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id,
                carol,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
            )
            .unwrap();

        let admin_snapshot = store
            .projection()
            .messages_snapshot(admin, 20, &available)
            .unwrap();
        let admin_row = &admin_snapshot.rows[0];
        let bob_entry = admin_row
            .recipients
            .iter()
            .find(|entry| entry.recipient == bob)
            .unwrap();
        let carol_entry = admin_row
            .recipients
            .iter()
            .find(|entry| entry.recipient == carol)
            .unwrap();
        assert_eq!(bob_entry.direction, MessageDirection::Outbound);
        assert!(!bob_entry.needs_action);
        assert!(!bob_entry.can_manage_attention);
        assert_eq!(carol_entry.direction, MessageDirection::Outbound);
        assert!(carol_entry.needs_action);
        assert!(carol_entry.can_manage_attention);
        assert!(admin_row.needs_action);

        let bob_snapshot = store
            .projection()
            .messages_snapshot(bob, 20, &available)
            .unwrap();
        assert!(bob_snapshot.rows[0]
            .recipients
            .iter()
            .all(|entry| !entry.can_manage_attention));
    }

    #[test]
    fn route_replacement_does_not_make_the_old_recipient_available() {
        let scratch = StoreScratch::new("snapshot-route-replacement");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, old_recipient, _) = test_context();
        let pane = TmuxPaneId::from_str("%1").unwrap();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: old_recipient,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let old_message = service
            .send(service.admin(), mailbox_send("reviewer", "Old", "body"))
            .unwrap()
            .message_id;
        assert!(
            service
                .messages_snapshot(service.admin().key, 20)
                .unwrap()
                .rows[0]
                .recipients[0]
                .available
        );

        let replacement_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let replacement = RecipientKey::agent(workspace, replacement_session, pane);
        service
            .replace_directory(
                MailboxDirectory::new(
                    workspace,
                    [MailboxIdentity {
                        key: replacement,
                        label: "reviewer".into(),
                    }],
                )
                .unwrap(),
            )
            .unwrap();
        let new_message = service
            .send(service.admin(), mailbox_send("reviewer", "New", "body"))
            .unwrap()
            .message_id;
        let snapshot = service.messages_snapshot(service.admin().key, 20).unwrap();
        let old = snapshot
            .rows
            .iter()
            .find(|row| row.message_id == old_message)
            .unwrap();
        let new = snapshot
            .rows
            .iter()
            .find(|row| row.message_id == new_message)
            .unwrap();
        assert!(!old.recipients[0].available);
        assert_eq!(old.recipients[0].recipient, old_recipient);
        assert!(new.recipients[0].available);
        assert_eq!(new.recipients[0].recipient, replacement);
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_msg_line(
        seq: u64,
        id: &str,
        ws: WorkspaceId,
        sender: RecipientKey,
        recipients: Vec<RecipientKey>,
        kind: Kind,
        client_key: Option<&str>,
        body: &str,
    ) -> LedgerLine {
        let msg_id = MessageId::new(id).unwrap();
        let digest = RequestDigest::compute(
            kind,
            sender,
            &recipients,
            Some("Task"),
            Some(body),
            None,
            None,
        )
        .unwrap();
        let presentation = MessagePresentation {
            sender_label: sender.to_string(),
            recipient_labels: recipients
                .iter()
                .map(|recipient| RecipientPresentation {
                    recipient: *recipient,
                    label: recipient.to_string(),
                })
                .collect(),
        };

        let metadata = MessageMetadata {
            record_version: CANONICAL_RECORD_VERSION,
            workspace_id: ws,
            sender,
            recipients: recipients.clone(),
            presentation,
            thread_root: msg_id,
            client_key: client_key.map(String::from),
            request_digest: digest,
            supersedes: None,
        };

        LedgerLine {
            seq,
            boot_id: "boot-1".into(),
            id: id.into(),
            ts: 1_700_000_000_000 + seq,
            kind,
            from: sender.to_string(),
            to: recipients.iter().map(|r| r.to_string()).collect(),
            subject: Some("Task".into()),
            body: Some(body.into()),
            reply_to: None,
            deliveries: vec![],
            data: Some(serde_json::to_value(metadata).unwrap()),
        }
    }

    fn sample_claim_line(seq: u64, message_id: MessageId, recipient: RecipientKey) -> LedgerLine {
        LedgerLine {
            seq,
            boot_id: "boot-1".into(),
            id: message_id.to_string(),
            ts: 1_700_000_000_000 + seq,
            kind: Kind::State,
            from: recipient.to_string(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(
                serde_json::to_value(MailboxFact::MessageClaimed {
                    record_version: CANONICAL_RECORD_VERSION,
                    message_id,
                    recipient,
                    claimant: recipient,
                })
                .unwrap(),
            ),
        }
    }

    fn sample_notification_state_line(
        seq: u64,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        state: NotificationState,
    ) -> LedgerLine {
        LedgerLine {
            seq,
            boot_id: "boot-1".into(),
            id: message_id.to_string(),
            ts: 1_700_000_000_000 + seq,
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: vec![recipient.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(
                serde_json::to_value(NotificationFact::NotificationTransition {
                    record_version: CANONICAL_RECORD_VERSION,
                    attempt_id,
                    message_id,
                    recipient,
                    state,
                    binding: None,
                    transport: None,
                    doorbell_format: None,
                    cause: None,
                    verify_outcome: None,
                    pre_write_cause: None,
                    wake_block: None,
                    pre_write_observation: None,
                })
                .unwrap(),
            ),
        }
    }

    fn sample_queued_notification_line(
        seq: u64,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> LedgerLine {
        sample_notification_state_line(
            seq,
            message_id,
            recipient,
            attempt_id,
            NotificationState::Queued,
        )
    }

    #[test]
    fn presentation_mismatch_fails_closed_with_failure_atomicity() {
        let (ws, admin, bob, _) = test_context();
        let mut proj = MailboxProjection::new(ws);

        let mut line_from =
            sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "Body");
        line_from.from = "ContradictorySenderPresentation".into();

        let err = proj.apply_line(&line_from).unwrap_err();
        assert!(matches!(
            err,
            MailboxError::PresentationMismatch { field: "from", .. }
        ));
        assert_eq!(proj.last_sequence(), None);
        assert_eq!(proj.get_pending(bob).len(), 0);

        let mut line_to = sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "Body");
        line_to.to = vec!["ContradictoryRecipientPresentation".into()];

        let err = proj.apply_line(&line_to).unwrap_err();
        assert!(matches!(
            err,
            MailboxError::PresentationMismatch { field: "to", .. }
        ));
        assert_eq!(proj.last_sequence(), None);
        assert_eq!(proj.get_pending(bob).len(), 0);
    }

    #[test]
    fn legacy_session_lines_without_metadata_rejected_by_projection() {
        let (ws, _admin, bob, _) = test_context();
        let mut proj = MailboxProjection::new(ws);

        let legacy_session_line = LedgerLine {
            seq: 1,
            boot_id: "boot-legacy".into(),
            id: "m-legacy".into(),
            ts: 1_700_000_000_000,
            kind: Kind::Msg,
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: Some("Old".into()),
            body: Some("Old message".into()),
            reply_to: None,
            deliveries: vec![],
            data: None, // No MessageMetadata
        };

        let err = proj.apply_line(&legacy_session_line).unwrap_err();
        assert!(matches!(err, MailboxError::MissingMetadata(_)));
        assert_eq!(proj.last_sequence(), None);
        assert_eq!(proj.get_pending(bob).len(), 0);
    }

    #[test]
    fn uncanonical_record_version_rejected() {
        let (ws, admin, bob, _) = test_context();
        let mut proj = MailboxProjection::new(ws);

        let mut line = sample_msg_line(1, "m-badver", ws, admin, vec![bob], Kind::Msg, None, "A");
        let mut meta: MessageMetadata = serde_json::from_value(line.data.clone().unwrap()).unwrap();
        meta.record_version = 999;
        line.data = Some(serde_json::to_value(meta).unwrap());

        let err = proj.apply_line(&line).unwrap_err();
        assert_eq!(
            err,
            MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: 999
            }
        );
        assert_eq!(proj.last_sequence(), None);
        assert_eq!(proj.get_pending(bob).len(), 0);
    }

    #[test]
    fn pre_append_acceptance_separates_retries_and_conflicts() {
        let (ws, admin, bob, _) = test_context();
        let proj = MailboxProjection::new(ws);

        let draft_1 = CanonicalDraft {
            kind: Kind::Msg,
            sender: admin,
            recipients: vec![bob],
            subject: Some("Task".into()),
            body: Some("B1".into()),
            reply_to: None,
            client_key: Some("key-1".into()),
            supersedes: None,
            presentation: test_presentation(&[bob]),
        };

        let outcome = proj.check_acceptance(&draft_1).unwrap();
        assert!(matches!(outcome, AcceptanceOutcome::New { .. }));

        let mut active_proj = proj;
        let line = sample_msg_line(
            1,
            "m-1",
            ws,
            admin,
            vec![bob],
            Kind::Msg,
            Some("key-1"),
            "B1",
        );
        active_proj.apply_line(&line).unwrap();

        let retry = active_proj.check_acceptance(&draft_1).unwrap();
        assert_eq!(
            retry,
            AcceptanceOutcome::Existing(MessageId::new("m-1").unwrap())
        );

        let draft_conflict = CanonicalDraft {
            kind: Kind::Msg,
            sender: admin,
            recipients: vec![bob],
            subject: Some("Task".into()),
            body: Some("B2_DIFF".into()),
            reply_to: None,
            client_key: Some("key-1".into()),
            supersedes: None,
            presentation: test_presentation(&[bob]),
        };
        let err = active_proj.check_acceptance(&draft_conflict).unwrap_err();
        assert!(matches!(err, MailboxError::DuplicateIdempotencyKey { .. }));
    }

    #[test]
    fn strict_monotonic_workspace_sequence_and_failure_atomicity() {
        let (ws, admin, bob, _) = test_context();
        let mut proj = MailboxProjection::new(ws);

        let line_seq1 = sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "A");
        let line_seq3 = sample_msg_line(3, "m-3", ws, admin, vec![bob], Kind::Msg, None, "B");

        proj.apply_line(&line_seq1).unwrap();
        assert_eq!(proj.get_pending(bob).len(), 1);
        assert_eq!(proj.last_sequence(), Some(1));

        let err = proj.apply_line(&line_seq3).unwrap_err();
        assert_eq!(
            err,
            MailboxError::NonContiguousSequence {
                expected: 2,
                found: 3
            }
        );

        assert_eq!(proj.last_sequence(), Some(1));
        assert_eq!(proj.get_pending(bob).len(), 1);
        assert!(proj.get_message(&MessageId::new("m-3").unwrap()).is_none());
    }

    #[test]
    fn claim_envelope_and_fact_binding_failure_atomicity() {
        let (ws, admin, bob, carol) = test_context();
        let mut proj = MailboxProjection::new(ws);

        let line1 = sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "A");
        proj.apply_line(&line1).unwrap();

        let claim_bad_ver = LedgerLine {
            seq: 2,
            boot_id: "boot-1".into(),
            id: "m-1".into(),
            ts: 1_700_000_001_000,
            kind: Kind::State,
            from: bob.to_string(),
            to: vec![],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: vec![],
            data: Some(
                serde_json::to_value(MailboxFact::MessageClaimed {
                    record_version: 99,
                    message_id: MessageId::new("m-1").unwrap(),
                    recipient: bob,
                    claimant: bob,
                })
                .unwrap(),
            ),
        };
        let err = proj.apply_line(&claim_bad_ver).unwrap_err();
        assert_eq!(
            err,
            MailboxError::InvalidRecordVersion {
                expected: CANONICAL_RECORD_VERSION,
                found: 99
            }
        );
        assert_eq!(proj.last_sequence(), Some(1));
        assert_eq!(proj.get_pending(bob).len(), 1);

        let claim_nonempty_to = LedgerLine {
            seq: 2,
            boot_id: "boot-1".into(),
            id: "m-1".into(),
            ts: 1_700_000_001_000,
            kind: Kind::State,
            from: bob.to_string(),
            to: vec!["extra_recipient".into()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: vec![],
            data: Some(
                serde_json::to_value(MailboxFact::MessageClaimed {
                    record_version: CANONICAL_RECORD_VERSION,
                    message_id: MessageId::new("m-1").unwrap(),
                    recipient: bob,
                    claimant: bob,
                })
                .unwrap(),
            ),
        };
        let err = proj.apply_line(&claim_nonempty_to).unwrap_err();
        assert!(matches!(
            err,
            MailboxError::PresentationMismatch { field: "to", .. }
        ));
        assert_eq!(proj.last_sequence(), Some(1));

        let claim_nonempty_subject = LedgerLine {
            seq: 2,
            boot_id: "boot-1".into(),
            id: "m-1".into(),
            ts: 1_700_000_001_000,
            kind: Kind::State,
            from: bob.to_string(),
            to: vec![],
            subject: Some("Unexpected Subject".into()),
            body: None,
            reply_to: None,
            deliveries: vec![],
            data: Some(
                serde_json::to_value(MailboxFact::MessageClaimed {
                    record_version: CANONICAL_RECORD_VERSION,
                    message_id: MessageId::new("m-1").unwrap(),
                    recipient: bob,
                    claimant: bob,
                })
                .unwrap(),
            ),
        };
        let err = proj.apply_line(&claim_nonempty_subject).unwrap_err();
        assert!(matches!(err, MailboxError::UncanonicalRow(_)));
        assert_eq!(proj.last_sequence(), Some(1));

        let claim_env_mismatch = LedgerLine {
            seq: 2,
            boot_id: "boot-1".into(),
            id: "m-diff".into(),
            ts: 1_700_000_001_000,
            kind: Kind::State,
            from: bob.to_string(),
            to: vec![],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: vec![],
            data: Some(
                serde_json::to_value(MailboxFact::MessageClaimed {
                    record_version: CANONICAL_RECORD_VERSION,
                    message_id: MessageId::new("m-1").unwrap(),
                    recipient: bob,
                    claimant: bob,
                })
                .unwrap(),
            ),
        };
        let err = proj.apply_line(&claim_env_mismatch).unwrap_err();
        assert!(matches!(err, MailboxError::EnvelopeMismatch { .. }));
        assert_eq!(proj.last_sequence(), Some(1));

        let claim_foreign = LedgerLine {
            seq: 2,
            boot_id: "boot-1".into(),
            id: "m-1".into(),
            ts: 1_700_000_001_000,
            kind: Kind::State,
            from: carol.to_string(),
            to: vec![],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: vec![],
            data: Some(
                serde_json::to_value(MailboxFact::MessageClaimed {
                    record_version: CANONICAL_RECORD_VERSION,
                    message_id: MessageId::new("m-1").unwrap(),
                    recipient: bob,
                    claimant: carol,
                })
                .unwrap(),
            ),
        };
        let err = proj.apply_line(&claim_foreign).unwrap_err();
        assert!(matches!(err, MailboxError::ClaimantMismatch { .. }));
        assert_eq!(proj.last_sequence(), Some(1));

        let claim_valid = LedgerLine {
            seq: 2,
            boot_id: "boot-1".into(),
            id: "m-1".into(),
            ts: 1_700_000_001_000,
            kind: Kind::State,
            from: bob.to_string(),
            to: vec![],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: vec![],
            data: Some(
                serde_json::to_value(MailboxFact::MessageClaimed {
                    record_version: CANONICAL_RECORD_VERSION,
                    message_id: MessageId::new("m-1").unwrap(),
                    recipient: bob,
                    claimant: bob,
                })
                .unwrap(),
            ),
        };
        proj.apply_line(&claim_valid).unwrap();
        assert_eq!(proj.last_sequence(), Some(2));
        assert_eq!(proj.get_pending(bob).len(), 0);
        assert_eq!(proj.get_mailbox(bob)[0].state.claimant(), Some(bob));
    }

    #[test]
    fn store_reopens_with_idempotent_accept_and_payload_bearing_claim() {
        let scratch = StoreScratch::new("reopen");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let original = MessageId::new("m-original").unwrap();
        let request = draft(admin, vec![bob], "Review code", Some("request-1"));

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            assert_eq!(
                store
                    .accept_at(original.clone(), request.clone(), 1_700_000_000_000)
                    .unwrap(),
                AcceptResult {
                    message_id: original.clone(),
                    inserted: true,
                    seq: 1,
                    recipients: vec!["recipient-0".into()],
                    recipient_keys: vec![bob],
                }
            );
            assert_eq!(store.projection().last_sequence(), Some(1));
            let listed = store.projection().list_mailbox(bob).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].sender_label, "sender-label");
            assert_eq!(listed[0].recipient_label, "recipient-0");
            let listed_json = serde_json::to_value(&listed[0]).unwrap();
            assert!(listed_json.get("body").is_none());

            let retry_id = MessageId::new("m-retry-candidate").unwrap();
            let mut retry = request;
            retry.presentation.sender_label = "renamed-sender".into();
            retry.presentation.recipient_labels[0].label = "renamed-recipient".into();
            assert_eq!(
                store.accept_at(retry_id, retry, 1_700_000_000_100).unwrap(),
                AcceptResult {
                    message_id: original.clone(),
                    inserted: false,
                    seq: 1,
                    recipients: vec!["recipient-0".into()],
                    recipient_keys: vec![bob],
                }
            );
            assert_eq!(store.projection().last_sequence(), Some(1));
            let listed = store.projection().list_mailbox(bob).unwrap();
            assert_eq!(listed[0].sender_label, "sender-label");
            assert_eq!(listed[0].recipient_label, "recipient-0");

            let first = store
                .claim_at(bob, original.clone(), 1_700_000_001_000)
                .unwrap();
            let ClaimOutcome::Claimed { entry, message, .. } = first else {
                panic!("first claim must append a claim fact");
            };
            assert_eq!(entry.message_id, original);
            assert_eq!(message.sender_label, "sender-label");
            assert_eq!(message.subject.as_deref(), Some("Task"));
            assert_eq!(message.body.as_deref(), Some("Review code"));

            let replay = store
                .claim_at(bob, original.clone(), 1_700_000_002_000)
                .unwrap();
            let ClaimOutcome::AlreadyClaimed { entry, message, .. } = replay else {
                panic!("re-claim must return the existing claim");
            };
            assert_eq!(entry.message_id, original);
            assert_eq!(message.subject.as_deref(), Some("Task"));
            assert_eq!(message.body.as_deref(), Some("Review code"));
            assert_eq!(store.projection().last_sequence(), Some(2));
        }

        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened.projection().get_pending(bob).is_empty());
        assert_eq!(reopened.projection().get_mailbox(bob).len(), 1);
        assert_eq!(reopened.projection().last_sequence(), Some(2));
        let listed = reopened.projection().list_mailbox(bob).unwrap();
        assert!(listed.is_empty());
        let ClaimOutcome::AlreadyClaimed { message, .. } = reopened.claim(bob, original).unwrap()
        else {
            panic!("claim state must survive restart");
        };
        assert_eq!(message.body.as_deref(), Some("Review code"));
        assert_eq!(reopened.projection().last_sequence(), Some(2));
    }

    #[test]
    fn direct_delivery_retires_pending_without_forging_a_claim_and_replays() {
        let scratch = StoreScratch::new("direct-delivery-replay");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-direct").unwrap();
        let attempt_id = attempt(77);

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Direct", None),
                    1,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    NotificationState::Queued,
                    None,
                    None,
                    2,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    NotificationState::Gating,
                    None,
                    None,
                    3,
                )
                .unwrap();
            let binding = notification_binding(bob);
            for (offset, state) in [
                NotificationState::Writing,
                NotificationState::Staged,
                NotificationState::Submitting,
                NotificationState::Submitted,
                NotificationState::Notified,
            ]
            .into_iter()
            .enumerate()
            {
                if state == NotificationState::Writing {
                    store
                        .append_notification_transition_with_transport_at(
                            message_id.clone(),
                            bob,
                            attempt_id,
                            state,
                            Some(binding.clone()),
                            Some(NotificationTransport::DirectPayload),
                            None,
                            None,
                            4 + offset as u64,
                        )
                        .unwrap();
                } else {
                    store
                        .append_notification_transition_at(
                            message_id.clone(),
                            bob,
                            attempt_id,
                            state,
                            None,
                            None,
                            4 + offset as u64,
                        )
                        .unwrap();
                }
            }
            let entry = store
                .mark_delivered_direct_at(message_id.clone(), bob, attempt_id, 9)
                .unwrap();
            assert!(matches!(
                entry.state,
                MailboxEntryState::DeliveredDirect {
                    attempt_id: found,
                    delivered_at: 9
                } if found == attempt_id
            ));
            assert_eq!(entry.state.claimant(), None);
            assert!(store.projection().get_pending(bob).is_empty());
            assert!(matches!(
                store.claim_at(bob, message_id.clone(), 10),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(*error, MailboxError::MessageNotPending(ref id) if id == &message_id)
            ));
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let entry = reopened.projection().get_entry(bob, &message_id).unwrap();
        assert!(matches!(
            entry.state,
            MailboxEntryState::DeliveredDirect { attempt_id: found, .. }
                if found == attempt_id
        ));
        let raw = fs::read_to_string(root.path().join(journal)).unwrap();
        let direct = raw
            .lines()
            .find(|line| line.contains("message_delivered_direct"))
            .expect("direct disposition fact");
        assert!(!direct.contains("claimant"));
    }

    #[test]
    fn restart_finishes_a_notified_direct_attempt_before_advancing_the_fifo() {
        let scratch = StoreScratch::new("direct-delivery-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let identity = MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        };
        let first_id;
        let second_id;

        {
            let directory = MailboxDirectory::new(workspace, [identity.clone()]).unwrap();
            let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            let service = MailboxService::new(directory, store);
            let first = service
                .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
                .unwrap();
            let second = service
                .send(service.admin(), mailbox_send("reviewer", "Second", "Body"))
                .unwrap();
            first_id = first.message_id.clone();
            second_id = second.message_id.clone();
            let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
            let context = crate::notification_adapter::NotificationContext::new(
                service.store_handle(),
                first.message_id,
                bob,
                queued.attempt_id,
            );
            context.record_gating().unwrap();
            context
                .record_writing(
                    notification_binding(bob).pane_root.unwrap(),
                    notification_binding(bob).leader.unwrap(),
                    notification_binding(bob).agent,
                    "codex",
                    NotificationTransport::DirectPayload,
                    None,
                )
                .unwrap();
            context.record_staged().unwrap();
            context.reserve_submit().unwrap();
            context.record_submitted().unwrap();
            context.record_notified().unwrap();
            assert!(service
                .store()
                .unwrap()
                .projection()
                .get_entry(bob, &first_id)
                .unwrap()
                .state
                .is_pending());
        }

        let directory = MailboxDirectory::new(workspace, [identity]).unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(directory, store);
        let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(next.message_id, second_id);
        let store = service.store().unwrap();
        assert!(matches!(
            store.projection().get_entry(bob, &first_id).unwrap().state,
            MailboxEntryState::DeliveredDirect { .. }
        ));
        assert_eq!(
            store
                .projection()
                .get_entry(bob, &first_id)
                .unwrap()
                .state
                .claimant(),
            None
        );
        drop(store);

        let raw = fs::read_to_string(root.path().join(journal)).unwrap();
        assert_eq!(raw.matches("message_delivered_direct").count(), 1);
        let same = service.prepare_oldest_notification(bob).unwrap().unwrap();
        assert_eq!(same.attempt_id, next.attempt_id);
    }

    #[test]
    fn restart_never_downgrades_an_ambiguous_doorbell_to_direct_delivery() {
        let scratch = StoreScratch::new("doorbell-attention-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let identity = MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        };
        let message_id;

        {
            let directory = MailboxDirectory::new(workspace, [identity.clone()]).unwrap();
            let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            let service = MailboxService::new(directory, store);
            let sent = service
                .send(
                    service.admin(),
                    mailbox_send("reviewer", "Doorbell", "Body"),
                )
                .unwrap();
            message_id = sent.message_id.clone();
            let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
            let context = crate::notification_adapter::NotificationContext::new(
                service.store_handle(),
                sent.message_id,
                bob,
                queued.attempt_id,
            );
            context.record_gating().unwrap();
            context
                .record_writing(
                    notification_binding(bob).pane_root.unwrap(),
                    notification_binding(bob).leader.unwrap(),
                    notification_binding(bob).agent,
                    "codex",
                    NotificationTransport::Doorbell,
                    Some(DOORBELL_FORMAT_COMPACT_CLAIM),
                )
                .unwrap();
            context
                .record_attention(NotificationAttentionCause::VerifyFailed)
                .unwrap();
        }

        let directory = MailboxDirectory::new(workspace, [identity]).unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let service = MailboxService::new(directory, store);
        assert!(service.prepare_oldest_notification(bob).unwrap().is_none());
        let store = service.store().unwrap();
        assert!(store
            .projection()
            .get_entry(bob, &message_id)
            .unwrap()
            .state
            .is_pending());
        assert_eq!(
            store
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .state,
            NotificationState::AttentionRequired
        );
        assert_eq!(
            store
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .doorbell_format,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM)
        );
        drop(store);
        let raw = fs::read_to_string(root.path().join(journal)).unwrap();
        assert!(!raw.contains("message_delivered_direct"));
        assert!(!raw.contains("message_claimed"));
    }

    #[test]
    fn canonical_rows_require_presentation_snapshots() {
        let (workspace, admin, bob, _) = test_context();
        let mut line = sample_msg_line(
            1,
            "m-missing-presentation",
            workspace,
            admin,
            vec![bob],
            Kind::Msg,
            None,
            "Body",
        );
        line.data
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("presentation");

        let mut projection = MailboxProjection::new(workspace);
        assert!(matches!(
            projection.apply_line(&line),
            Err(MailboxError::MissingMetadata(_))
        ));
        assert_eq!(projection.last_sequence(), None);
    }

    #[test]
    fn replay_refuses_presentation_labels_bound_to_the_wrong_recipient() {
        let (workspace, admin, bob, carol) = test_context();
        let mut projection = MailboxProjection::new(workspace);
        let mut line = sample_msg_line(
            1,
            "m-wrong-label-key",
            workspace,
            admin,
            vec![bob],
            Kind::Msg,
            None,
            "Body",
        );
        let mut metadata = extract_message_metadata(&line).unwrap();
        metadata.presentation = MessagePresentation {
            sender_label: "operator".into(),
            recipient_labels: vec![RecipientPresentation {
                recipient: carol,
                label: "reviewer".into(),
            }],
        };
        line.from = "operator".into();
        line.to = vec!["reviewer".into()];
        line.data = Some(serde_json::to_value(metadata).unwrap());

        assert!(matches!(
            projection.apply_line(&line),
            Err(MailboxError::InvalidPresentation(_))
        ));
        assert_eq!(projection.last_sequence(), None);
    }

    #[test]
    fn replies_derive_the_only_recipient_subject_and_thread_root() {
        let scratch = StoreScratch::new("replies");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let root_id = MessageId::new("m-root").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(root_id.clone(), draft(admin, vec![bob], "Root", None), 1)
            .unwrap();

        let missing = MessageId::new("m-missing").unwrap();
        let error = store
            .reply_at(
                MessageId::new("m-bad-missing").unwrap(),
                reply_draft(bob, missing.clone(), "Missing"),
                2,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::MessageNotFound(id) if id == &missing)
        ));

        let error = store
            .reply_at(
                MessageId::new("m-bad-hidden").unwrap(),
                reply_draft(carol, root_id.clone(), "Hidden"),
                3,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::ReplyNotVisible { .. })
        ));
        assert_eq!(store.projection().last_sequence(), Some(1));

        let first_reply = MessageId::new("m-reply-one").unwrap();
        store
            .reply_at(
                first_reply.clone(),
                reply_draft(bob, root_id.clone(), "First reply"),
                4,
            )
            .unwrap();
        let second_reply = MessageId::new("m-reply-two").unwrap();
        store
            .reply_at(
                second_reply.clone(),
                reply_draft(admin, first_reply.clone(), "Second reply"),
                5,
            )
            .unwrap();

        let first = store.projection().get_message(&first_reply).unwrap();
        let first_metadata = extract_message_metadata(first).unwrap();
        assert_eq!(first_metadata.recipients, [admin]);
        assert_eq!(first.subject.as_deref(), Some("Re: Task"));
        assert_eq!(first_metadata.thread_root, root_id);

        let second = store.projection().get_message(&second_reply).unwrap();
        let second_metadata = extract_message_metadata(second).unwrap();
        assert_eq!(second_metadata.recipients, [bob]);
        assert_eq!(second.subject.as_deref(), Some("Re: Task"));
        assert_eq!(second_metadata.thread_root, root_id);
        assert_eq!(store.projection().last_sequence(), Some(3));
    }

    #[test]
    fn supersession_is_an_atomic_auditable_mailbox_transition() {
        let scratch = StoreScratch::new("supersession");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let old_id = MessageId::new("m-old").unwrap();
        let replacement_id = MessageId::new("m-replacement").unwrap();

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(old_id.clone(), draft(admin, vec![bob], "Old", None), 1)
                .unwrap();
            let mut replacement = draft(admin, vec![bob], "New", None);
            replacement.supersedes = Some(old_id.clone());
            store
                .accept_at(replacement_id.clone(), replacement, 2)
                .unwrap();

            let old = store.projection().get_entry(bob, &old_id).unwrap();
            assert_eq!(
                old.state,
                MailboxEntryState::Superseded {
                    by: replacement_id.clone(),
                    superseded_at: 2,
                }
            );
            let pending = store.projection().get_pending(bob);
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].message_id, replacement_id);
            let replacement = store.projection().get_message(&replacement_id).unwrap();
            assert_eq!(
                extract_message_metadata(replacement).unwrap().supersedes,
                Some(old_id.clone())
            );
            assert!(matches!(
                store.claim_at(bob, old_id.clone(), 3),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::MessageNotPending(id) if id == &old_id)
            ));
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(matches!(
            &reopened.projection().get_entry(bob, &old_id).unwrap().state,
            MailboxEntryState::Superseded { by, .. } if by == &replacement_id
        ));
        assert_eq!(reopened.projection().get_pending(bob).len(), 1);
    }

    #[test]
    fn a_superseded_entry_is_never_a_requeue_target() {
        let scratch = StoreScratch::new("superseded-requeue");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let old_id = MessageId::new("m-superseded").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(old_id.clone(), draft(admin, vec![bob], "Old", None), 1)
            .unwrap();
        let mut replacement = draft(admin, vec![bob], "New", None);
        replacement.supersedes = Some(old_id.clone());
        store
            .accept_at(MessageId::new("m-replacement").unwrap(), replacement, 2)
            .unwrap();
        let before = store.projection().last_sequence();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "bob".into(),
            }],
        )
        .unwrap();
        let service = MailboxService::new(directory, store);

        assert!(service.requeue_message(old_id).unwrap().is_empty());
        assert_eq!(
            service.store().unwrap().projection().last_sequence(),
            before
        );
    }

    #[test]
    fn supersession_withdraws_queued_and_gating_notifications_on_replay() {
        let scratch = StoreScratch::new("supersession-notification");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let queued = MessageId::new("m-queued-old").unwrap();
        let queued_replacement = MessageId::new("m-queued-new").unwrap();
        let gating = MessageId::new("m-gating-old").unwrap();
        let gating_replacement = MessageId::new("m-gating-new").unwrap();

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(queued.clone(), draft(admin, vec![bob], "Queued", None), 1)
                .unwrap();
            store
                .append_notification_transition_at(
                    queued.clone(),
                    bob,
                    attempt(1),
                    NotificationState::Queued,
                    None,
                    None,
                    2,
                )
                .unwrap();
            let mut replacement = draft(admin, vec![bob], "Queued replacement", None);
            replacement.supersedes = Some(queued.clone());
            store.accept_at(queued_replacement, replacement, 3).unwrap();

            store
                .accept_at(gating.clone(), draft(admin, vec![bob], "Gating", None), 4)
                .unwrap();
            store
                .append_notification_transition_at(
                    gating.clone(),
                    bob,
                    attempt(2),
                    NotificationState::Queued,
                    None,
                    None,
                    5,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    gating.clone(),
                    bob,
                    attempt(2),
                    NotificationState::Gating,
                    None,
                    None,
                    6,
                )
                .unwrap();
            let mut replacement = draft(admin, vec![bob], "Gating replacement", None);
            replacement.supersedes = Some(gating.clone());
            store.accept_at(gating_replacement, replacement, 7).unwrap();

            for (message_id, attempt_id, updated_seq) in
                [(&queued, attempt(1), 3), (&gating, attempt(2), 7)]
            {
                let record = store.projection().notification(bob, message_id).unwrap();
                assert_eq!(record.attempt_id, attempt_id);
                assert_eq!(record.state, NotificationState::Superseded);
                assert_eq!(record.updated_seq, updated_seq);
                assert!(record.binding.is_none());
                assert!(record.cause.is_none());
            }
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert_eq!(
            reopened
                .projection()
                .notification(bob, &queued)
                .unwrap()
                .state,
            NotificationState::Superseded
        );
        assert_eq!(
            reopened
                .projection()
                .notification(bob, &gating)
                .unwrap()
                .state,
            NotificationState::Superseded
        );
    }

    #[test]
    fn quota_attempts_are_withdrawn_by_supersession_and_claim_on_replay() {
        let scratch = StoreScratch::new("quota-withdrawal");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let held = MessageId::new("m-quota-held-old").unwrap();
        let reset = MessageId::new("m-quota-reset-old").unwrap();

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(held.clone(), draft(admin, vec![bob], "Held", None), 1)
                .unwrap();
            quota_hold(&mut store, &held, bob, attempt(1), 2);
            let mut replacement = draft(admin, vec![bob], "Replacement", None);
            replacement.supersedes = Some(held.clone());
            store
                .accept_at(MessageId::new("m-quota-held-new").unwrap(), replacement, 5)
                .unwrap();

            store
                .accept_at(reset.clone(), draft(admin, vec![carol], "Reset", None), 6)
                .unwrap();
            quota_hold(&mut store, &reset, carol, attempt(2), 7);
            store
                .advance_notification(
                    reset.clone(),
                    carol,
                    attempt(2),
                    NotificationState::QuotaResetObserved,
                    None,
                    None,
                )
                .unwrap();
            let ClaimOutcome::Claimed {
                withdrawn_attempt, ..
            } = store.claim_at(carol, reset.clone(), 11).unwrap()
            else {
                panic!("quota-reset message was not claimed");
            };
            assert_eq!(withdrawn_attempt, Some(attempt(2)));

            assert_eq!(
                store.projection().notification(bob, &held).unwrap().state,
                NotificationState::Superseded
            );
            assert_eq!(
                store
                    .projection()
                    .notification(carol, &reset)
                    .unwrap()
                    .state,
                NotificationState::Withdrawn
            );
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert_eq!(
            reopened
                .projection()
                .notification(bob, &held)
                .unwrap()
                .state,
            NotificationState::Superseded
        );
        assert_eq!(
            reopened
                .projection()
                .notification(carol, &reset)
                .unwrap()
                .state,
            NotificationState::Withdrawn
        );
    }

    #[test]
    fn supersession_refuses_after_the_notification_write_boundary() {
        let scratch = StoreScratch::new("supersession-after-write");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let old_id = MessageId::new("m-writing-old").unwrap();
        let replacement_id = MessageId::new("m-writing-new").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(old_id.clone(), draft(admin, vec![bob], "Writing", None), 1)
            .unwrap();
        store
            .append_notification_transition_at(
                old_id.clone(),
                bob,
                attempt(1),
                NotificationState::Queued,
                None,
                None,
                2,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                old_id.clone(),
                bob,
                attempt(1),
                NotificationState::Gating,
                None,
                None,
                3,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                old_id.clone(),
                bob,
                attempt(1),
                NotificationState::Writing,
                Some(notification_binding(bob)),
                None,
                4,
            )
            .unwrap();

        let mut replacement = draft(admin, vec![bob], "Too late", None);
        replacement.supersedes = Some(old_id.clone());
        let error = store
            .accept_at(replacement_id.clone(), replacement, 5)
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::SupersessionNotificationStarted(id) if id == &old_id)
        ));
        assert!(store.projection().get_message(&replacement_id).is_none());
        assert_eq!(store.projection().last_sequence(), Some(4));
        assert_eq!(
            store.projection().notification(bob, &old_id).unwrap().state,
            NotificationState::Writing
        );
    }

    #[test]
    fn replay_refuses_reply_routing_or_subject_not_derived_from_parent() {
        let (workspace, admin, bob, carol) = test_context();
        let root = sample_msg_line(
            1,
            "m-root",
            workspace,
            admin,
            vec![bob],
            Kind::Msg,
            None,
            "Root",
        );
        let mut projection = MailboxProjection::new(workspace);
        projection.apply_line(&root).unwrap();

        let root_id = MessageId::new("m-root").unwrap();
        let reply_id = MessageId::new("m-reply").unwrap();
        let mut wrong_target = sample_msg_line(
            2,
            "m-reply",
            workspace,
            bob,
            vec![carol],
            Kind::Msg,
            None,
            "Reply",
        );
        wrong_target.reply_to = Some(root_id.to_string());
        wrong_target.subject = Some("Re: Task".into());
        let mut metadata = extract_message_metadata(&wrong_target).unwrap();
        metadata.thread_root = root_id.clone();
        metadata.request_digest = RequestDigest::compute(
            Kind::Msg,
            bob,
            &[carol],
            wrong_target.subject.as_deref(),
            wrong_target.body.as_deref(),
            Some(&root_id),
            None,
        )
        .unwrap();
        wrong_target.data = Some(serde_json::to_value(metadata).unwrap());
        assert!(matches!(
            projection.apply_line(&wrong_target),
            Err(MailboxError::ReplyRecipientMismatch { .. })
        ));

        let mut wrong_subject = wrong_target;
        let presentation = MessagePresentation {
            sender_label: bob.to_string(),
            recipient_labels: vec![RecipientPresentation {
                recipient: admin,
                label: admin.to_string(),
            }],
        };
        wrong_subject.to = vec![admin.to_string()];
        wrong_subject.subject = Some("Custom".into());
        let metadata = MessageMetadata {
            record_version: CANONICAL_RECORD_VERSION,
            workspace_id: workspace,
            sender: bob,
            recipients: vec![admin],
            presentation,
            thread_root: root_id.clone(),
            client_key: None,
            request_digest: RequestDigest::compute(
                Kind::Msg,
                bob,
                &[admin],
                wrong_subject.subject.as_deref(),
                wrong_subject.body.as_deref(),
                Some(&root_id),
                None,
            )
            .unwrap(),
            supersedes: None,
        };
        wrong_subject.data = Some(serde_json::to_value(metadata).unwrap());
        assert!(matches!(
            projection.apply_line(&wrong_subject),
            Err(MailboxError::ReplySubjectMismatch { message_id }) if message_id == reply_id
        ));
        assert_eq!(projection.last_sequence(), Some(1));
    }

    #[test]
    fn store_recovers_a_torn_tail_but_refuses_complete_corruption() {
        let scratch = StoreScratch::new("recovery");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    MessageId::new("m-one").unwrap(),
                    draft(admin, vec![bob], "One", None),
                    1,
                )
                .unwrap();
        }
        {
            let mut file = root.open_append(journal).unwrap();
            file.write_all(br#"{"seq":2,"boot_id":"boot-1","id":"m-torn""#)
                .unwrap();
            file.sync_data().unwrap();
        }
        {
            let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
            reopened
                .accept_at(
                    MessageId::new("m-two").unwrap(),
                    draft(admin, vec![bob], "Two", None),
                    2,
                )
                .unwrap();
            assert_eq!(reopened.projection().last_sequence(), Some(2));
            assert_eq!(reopened.projection().get_pending(bob).len(), 2);
        }
        {
            let mut file = root.open_append(journal).unwrap();
            file.write_all(b"complete corruption\n").unwrap();
            file.sync_data().unwrap();
        }

        assert!(matches!(
            MessageStore::open(&root, journal, workspace, "boot-3"),
            Err(MessageStoreError::Ledger(LedgerError::CorruptLine {
                line: 3,
                ..
            }))
        ));
    }

    /// Drive one attempt to the alarm state and return its identifier.
    ///
    /// Most operator tests start here. Clear acts only on alarms; requeue
    /// also accepts a quota hold after reset was positively observed.
    fn alarm(
        store: &mut MessageStore,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        base_ts: u64,
    ) {
        alarm_because(
            store,
            message_id,
            recipient,
            attempt_id,
            base_ts,
            NotificationAttentionCause::SubmitFailed,
        )
    }

    /// The same, raised for one named cause.
    ///
    /// How far the attempt gets is decided by the cause: a verify failure
    /// happens at the write boundary, a submit failure after the composer
    /// took the text. The closed vocabulary is asked rather than assumed.
    fn alarm_because(
        store: &mut MessageStore,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        base_ts: u64,
        cause: NotificationAttentionCause,
    ) {
        // A requeued attempt is already queued; queueing it again is an
        // illegal transition, so the first step is conditional.
        let already_queued = store
            .projection()
            .notification(recipient, message_id)
            .is_some_and(|record| {
                record.attempt_id == attempt_id && record.state == NotificationState::Queued
            });
        let mut steps = vec![
            (NotificationState::Queued, None),
            (NotificationState::Gating, None),
            (
                NotificationState::Writing,
                Some(notification_binding(recipient)),
            ),
        ];
        if !cause.valid_after(NotificationState::Writing) {
            steps.push((NotificationState::Staged, None));
        }
        for (offset, (state, binding)) in steps.into_iter().enumerate() {
            if already_queued && state == NotificationState::Queued {
                continue;
            }
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    recipient,
                    attempt_id,
                    state,
                    binding,
                    None,
                    base_ts + offset as u64,
                )
                .unwrap();
        }
        store
            .append_notification_transition_at(
                message_id.clone(),
                recipient,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(cause),
                base_ts + 4,
            )
            .unwrap();
    }

    fn legacy_alarm(
        store: &mut MessageStore,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        transport: NotificationTransport,
        doorbell_format: Option<u32>,
        base_ts: u64,
    ) {
        for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
            .into_iter()
            .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    recipient,
                    attempt_id,
                    state,
                    None,
                    None,
                    base_ts + offset as u64,
                )
                .unwrap();
        }
        store
            .append_notification_transition_with_transport_at(
                message_id.clone(),
                recipient,
                attempt_id,
                NotificationState::Writing,
                Some(legacy_notification_binding(recipient)),
                Some(transport),
                doorbell_format,
                None,
                base_ts + 2,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                recipient,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
                base_ts + 3,
            )
            .unwrap();
    }

    fn exact_doorbell_alarm(
        store: &mut MessageStore,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        base_ts: u64,
    ) {
        for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
            .into_iter()
            .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    recipient,
                    attempt_id,
                    state,
                    None,
                    None,
                    base_ts + offset as u64,
                )
                .unwrap();
        }
        store
            .append_notification_transition_with_transport_at(
                message_id.clone(),
                recipient,
                attempt_id,
                NotificationState::Writing,
                Some(notification_binding(recipient)),
                Some(NotificationTransport::Doorbell),
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
                None,
                base_ts + 2,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                recipient,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
                base_ts + 3,
            )
            .unwrap();
    }

    fn append_resolution_at(
        store: &mut MessageStore,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        proof_version: u32,
        resolution: NotificationResolution,
        ts: u64,
    ) -> Result<NotificationRecord, MessageStoreError> {
        store.append_notification_fact_at(
            message_id.clone(),
            recipient,
            NotificationFact::NotificationResolved {
                record_version: CANONICAL_RECORD_VERSION,
                proof_version,
                attempt_id,
                message_id: message_id.clone(),
                recipient,
                resolution,
            },
            ts,
        )
    }

    fn quota_hold(
        store: &mut MessageStore,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        base_ts: u64,
    ) {
        for (offset, state) in [
            NotificationState::Queued,
            NotificationState::Gating,
            NotificationState::QuotaHeld,
        ]
        .into_iter()
        .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    recipient,
                    attempt_id,
                    state,
                    None,
                    None,
                    base_ts + offset as u64,
                )
                .unwrap();
        }
    }

    fn notify_with_binding(
        store: &mut MessageStore,
        message_id: &MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        base_ts: u64,
    ) {
        for (offset, state) in [
            NotificationState::Queued,
            NotificationState::Gating,
            NotificationState::Writing,
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
            NotificationState::Notified,
        ]
        .into_iter()
        .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    recipient,
                    attempt_id,
                    state,
                    (state == NotificationState::Writing).then(|| notification_binding(recipient)),
                    None,
                    base_ts + offset as u64,
                )
                .unwrap();
        }
    }

    fn operator_store(tag: &str) -> (StoreScratch, MessageStore, MessageId, RecipientKey) {
        let scratch = StoreScratch::new(tag);
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let message_id = MessageId::new("m-operator").unwrap();
        store
            .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
            .unwrap();
        (scratch, store, message_id, bob)
    }

    fn broadcast_operator_store(
        tag: &str,
    ) -> (
        StoreScratch,
        MessageStore,
        MessageId,
        RecipientKey,
        RecipientKey,
    ) {
        let scratch = StoreScratch::new(tag);
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let message_id = MessageId::new("m-operator-broadcast").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Op", None),
                1,
            )
            .unwrap();
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        alarm(&mut store, &message_id, carol, attempt(2), 10);
        (scratch, store, message_id, bob, carol)
    }

    fn operator_directory(bob: RecipientKey, carol: RecipientKey) -> MailboxDirectory {
        MailboxDirectory::new(
            test_context().0,
            [
                MailboxIdentity {
                    key: bob,
                    label: "bob".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "carol".into(),
                },
            ],
        )
        .unwrap()
    }

    #[test]
    fn exact_reconciliation_edges_coalesce_without_getting_lost() {
        let (_scratch, store, _, bob) = operator_store("exact-reconciliation-edges");
        let carol = test_context().3;
        let service = MailboxService::new(operator_directory(bob, carol), store);
        let attempt_id = attempt(1);

        assert!(service.request_exact_reconciliation(attempt_id).unwrap());
        assert!(service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());
        assert!(!service.request_exact_reconciliation(attempt_id).unwrap());
        assert!(service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());
        assert!(!service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());

        assert!(service.request_exact_reconciliation(attempt_id).unwrap());
        assert!(service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());

        service
            .resolving_attention
            .lock()
            .unwrap()
            .insert(attempt_id);
        assert!(!service
            .park_exact_reconciliation_after_conflict(attempt_id)
            .unwrap());
        service
            .resolving_attention
            .lock()
            .unwrap()
            .remove(&attempt_id);
        assert!(service.resume_exact_reconciliation(attempt_id).unwrap());
        assert!(service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());
        assert!(!service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());

        assert!(service.request_exact_reconciliation(attempt_id).unwrap());
        assert!(service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());
        service
            .resolving_attention
            .lock()
            .unwrap()
            .insert(attempt_id);
        service
            .resolving_attention
            .lock()
            .unwrap()
            .remove(&attempt_id);
        assert!(service
            .park_exact_reconciliation_after_conflict(attempt_id)
            .unwrap());
        assert!(service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());
        assert!(!service
            .take_exact_reconciliation_request(attempt_id)
            .unwrap());
    }

    fn current_attempts(
        store: &MessageStore,
        message_id: &MessageId,
        recipients: [RecipientKey; 2],
    ) -> HashMap<RecipientKey, NotificationAttemptId> {
        recipients
            .into_iter()
            .map(|recipient| {
                (
                    recipient,
                    store
                        .projection()
                        .notification(recipient, message_id)
                        .unwrap()
                        .attempt_id,
                )
            })
            .collect()
    }

    fn batch_requeue_line(
        seq: u64,
        message_id: &MessageId,
        requeues: Vec<NotificationRequeue>,
    ) -> LedgerLine {
        LedgerLine {
            seq,
            boot_id: "boot-1".into(),
            id: message_id.to_string(),
            ts: 50,
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: requeues
                .iter()
                .map(|requeue| requeue.recipient.to_string())
                .collect(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(
                serde_json::to_value(NotificationFact::NotificationsRequeued {
                    record_version: CANONICAL_RECORD_VERSION,
                    message_id: message_id.clone(),
                    requeues,
                })
                .unwrap(),
            ),
        }
    }

    /// A requeue opens a fresh attempt and retires the one it replaces, so
    /// the identifier an operator saw can never be acted on again.
    #[test]
    fn a_requeue_retires_the_attempt_it_replaces() {
        let (scratch, mut store, message_id, bob) = operator_store("requeue-retires");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        assert_eq!(store.projection().open_alarms().len(), 1);

        let requeued = store
            .requeue_notification_at(message_id.clone(), bob, attempt(1), attempt(2), 10)
            .unwrap();
        assert_eq!(requeued.attempt_id, attempt(2));
        assert_eq!(requeued.state, NotificationState::Queued);

        // The old identifier names nothing, and a queued attempt is not an
        // alarm, so nothing is left for an operator to act on.
        assert!(store.projection().alarm_by_attempt(attempt(1)).is_none());
        assert!(store.projection().open_alarms().is_empty());

        let text = std::fs::read_to_string(store.journal_path()).unwrap();
        let line: LedgerLine = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(line.data.as_ref().unwrap()["type"], "notification_requeued");
        drop(store);

        let reopened = MessageStore::open(
            &scratch.root(),
            Path::new("workspaces/current/messages.ndjson"),
            test_context().0,
            "boot-2",
        )
        .unwrap();
        assert_eq!(
            reopened
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .attempt_id,
            attempt(2)
        );
    }

    #[test]
    fn quota_hold_waits_for_observation_and_explicit_requeue_across_restart() {
        let (scratch, mut store, message_id, bob) = operator_store("quota-explicit-requeue");
        quota_hold(&mut store, &message_id, bob, attempt(1), 2);
        let before_reset = store.projection().last_sequence().unwrap();
        let (workspace, admin, _, carol) = test_context();
        let service = MailboxService::new(operator_directory(bob, carol), store);

        assert!(service
            .requeue_message(message_id.clone())
            .unwrap()
            .is_empty());
        assert!(service.prepare_oldest_notification(bob).unwrap().is_none());
        assert_eq!(
            service.store().unwrap().projection().last_sequence(),
            Some(before_reset)
        );
        drop(service);

        let store = MessageStore::open(
            &scratch.root(),
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot-2",
        )
        .unwrap();
        assert_eq!(
            store
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .state,
            NotificationState::QuotaHeld
        );
        let (sender, _) = broadcast::channel(8);
        let mut events = sender.subscribe();
        let service =
            MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

        let observed = service.observe_quota_reset(bob).unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].attempt_id, attempt(1));
        assert_eq!(observed[0].state, NotificationState::QuotaResetObserved);
        next_change(
            &mut events,
            before_reset + 1,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        assert!(service.prepare_oldest_notification(bob).unwrap().is_none());

        let after_first_observation = service.store().unwrap().projection().last_sequence();
        assert!(service.observe_quota_reset(bob).unwrap().is_empty());
        assert_eq!(
            service.store().unwrap().projection().last_sequence(),
            after_first_observation
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop(service);

        let store = MessageStore::open(
            &scratch.root(),
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot-3",
        )
        .unwrap();
        assert_eq!(
            store
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .state,
            NotificationState::QuotaResetObserved
        );
        let service = MailboxService::new(operator_directory(bob, carol), store);
        assert!(service.prepare_oldest_notification(bob).unwrap().is_none());

        let snapshot = service.messages_snapshot(admin, 0).unwrap();
        let notification = &snapshot.rows[0].recipients[0].notification;
        assert_eq!(
            notification.state,
            MessageNotificationState::AttentionRequired
        );
        assert_eq!(
            notification.quota_state,
            Some(MessageQuotaState::ResetObserved)
        );
        assert!(!snapshot.rows[0].recipients[0].can_manage_attention);
        assert!(snapshot.rows[0].recipients[0].needs_action);

        let requeued = service.requeue_message(message_id.clone()).unwrap();
        assert_eq!(requeued.len(), 1);
        assert_eq!(requeued[0].state, NotificationState::Queued);
        assert_ne!(requeued[0].attempt_id, attempt(1));
        assert!(service
            .prepare_oldest_notification(bob)
            .unwrap()
            .is_some_and(|record| record.attempt_id == requeued[0].attempt_id));
        drop(service);

        let reopened = MessageStore::open(
            &scratch.root(),
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot-4",
        )
        .unwrap();
        let replayed = reopened
            .projection()
            .notification(bob, &message_id)
            .unwrap();
        assert_eq!(replayed.state, NotificationState::Queued);
        assert_eq!(replayed.attempt_id, requeued[0].attempt_id);
    }

    #[test]
    fn broadcast_quota_requeue_remains_one_content_free_atomic_fact() {
        let scratch = StoreScratch::new("quota-broadcast-requeue");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-quota-broadcast").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Quota", None),
                1,
            )
            .unwrap();
        quota_hold(&mut store, &message_id, bob, attempt(1), 2);
        quota_hold(&mut store, &message_id, carol, attempt(2), 10);
        let service = MailboxService::new(operator_directory(bob, carol), store);
        service.observe_quota_reset(bob).unwrap();
        service.observe_quota_reset(carol).unwrap();
        let before_lines = service.journal_lines().unwrap().len();

        let records = service.requeue_message(message_id.clone()).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|record| record.state == NotificationState::Queued));
        let lines = service.journal_lines().unwrap();
        assert_eq!(lines.len(), before_lines + 1);
        let fact = lines.last().unwrap();
        assert!(fact.subject.is_none());
        assert!(fact.body.is_none());
        assert_eq!(
            fact.data.as_ref().unwrap()["type"],
            "notifications_requeued"
        );
        let attempts: HashMap<_, _> = records
            .iter()
            .map(|record| (record.recipient, record.attempt_id))
            .collect();
        drop(service);

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        for recipient in [bob, carol] {
            let record = reopened
                .projection()
                .notification(recipient, &message_id)
                .unwrap();
            assert_eq!(record.state, NotificationState::Queued);
            assert_eq!(record.attempt_id, attempts[&recipient]);
        }
        drop(scratch);
    }

    #[test]
    fn a_broadcast_requeue_is_one_fact_one_event_and_replays_whole() {
        let (scratch, store, message_id, bob, carol) =
            broadcast_operator_store("requeue-batch-replay");
        let before_seq = store.projection().last_sequence().unwrap();
        let before_lines = std::fs::read_to_string(store.journal_path())
            .unwrap()
            .lines()
            .count();
        let (sender, _) = broadcast::channel(8);
        let mut events = sender.subscribe();
        let service =
            MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

        let records = service.requeue_message(message_id.clone()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .map(|record| record.recipient)
                .collect::<Vec<_>>(),
            [bob, carol]
        );
        assert!(records.iter().all(|record| {
            record.state == NotificationState::Queued && record.updated_seq == before_seq + 1
        }));
        next_change(
            &mut events,
            before_seq + 1,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        let attempts: HashMap<_, _> = records
            .iter()
            .map(|record| (record.recipient, record.attempt_id))
            .collect();
        let store = service.store().unwrap();
        let text = std::fs::read_to_string(store.journal_path()).unwrap();
        assert_eq!(text.lines().count(), before_lines + 1);
        let line: LedgerLine = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(line.seq, before_seq + 1);
        assert_eq!(line.to, [bob.to_string(), carol.to_string()]);
        assert_eq!(
            line.data.as_ref().unwrap()["type"],
            "notifications_requeued"
        );
        let NotificationFact::NotificationsRequeued { requeues, .. } =
            serde_json::from_value(line.data.clone().unwrap()).unwrap()
        else {
            panic!("last line is not a batch requeue");
        };
        assert_eq!(
            requeues
                .iter()
                .map(|requeue| requeue.recipient)
                .collect::<Vec<_>>(),
            [bob, carol]
        );
        assert!(line.subject.is_none());
        assert!(line.body.is_none());
        drop(store);

        let repeated = service.requeue_message(message_id.clone()).unwrap();
        assert!(repeated.is_empty());
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            service.store().unwrap().projection().last_sequence(),
            Some(before_seq + 1)
        );
        drop(service);

        let reopened = MessageStore::open(
            &scratch.root(),
            Path::new("workspaces/current/messages.ndjson"),
            test_context().0,
            "boot-2",
        )
        .unwrap();
        for recipient in [bob, carol] {
            let record = reopened
                .projection()
                .notification(recipient, &message_id)
                .unwrap();
            assert_eq!(record.state, NotificationState::Queued);
            assert_eq!(record.attempt_id, attempts[&recipient]);
            assert_eq!(record.updated_seq, before_seq + 1);
        }
    }

    #[test]
    fn broadcast_requeue_refuses_an_incomplete_barrier_before_any_append() {
        let scratch = StoreScratch::new("requeue-incomplete-barrier");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-requeue-incomplete-barrier").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Barrier", None),
                1,
            )
            .unwrap();
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        legacy_alarm(
            &mut store,
            &message_id,
            carol,
            attempt(2),
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            10,
        );
        let before_bytes = std::fs::read(store.journal_path()).unwrap();
        let before_seq = store.projection().last_sequence();
        let before_attempts = current_attempts(&store, &message_id, [bob, carol]);
        let before_barriers = store.projection().active_notification_barriers();
        let service = MailboxService::new(operator_directory(bob, carol), store);

        assert!(matches!(
            service.requeue_message(message_id.clone()),
            Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)))
                if matches!(
                    *error,
                    MailboxError::NotificationRequeueBarrierBindingIncomplete(id)
                        if id == attempt(2)
                )
        ));

        let store = service.store().unwrap();
        assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
        assert_eq!(store.projection().last_sequence(), before_seq);
        assert_eq!(
            current_attempts(&store, &message_id, [bob, carol]),
            before_attempts
        );
        assert_eq!(
            store.projection().active_notification_barriers(),
            before_barriers
        );
        drop(store);
        drop(service);

        MessageStore::open(&root, journal, workspace, "boot-2")
            .expect("the service guard does not alter replay semantics");
    }

    #[test]
    fn broadcast_requeue_preserves_an_exact_composer_barrier_and_its_handle() {
        let scratch = StoreScratch::new("requeue-exact-composer-barrier");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-requeue-exact-composer-barrier").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Barrier", None),
                1,
            )
            .unwrap();
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        exact_doorbell_alarm(&mut store, &message_id, carol, attempt(2), 10);
        let before_bytes = std::fs::read(store.journal_path()).unwrap();
        let before_seq = store.projection().last_sequence();
        let before_attempts = current_attempts(&store, &message_id, [bob, carol]);
        let service = MailboxService::new(operator_directory(bob, carol), store);

        assert!(matches!(
            service.requeue_message(message_id.clone()),
            Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)))
                if matches!(
                    *error,
                    MailboxError::NotificationRequeueExactComposerBarrier(id)
                        if id == attempt(2)
                )
        ));
        assert_eq!(
            service
                .attention_target(&attempt(2).to_string())
                .unwrap()
                .record
                .attempt_id,
            attempt(2)
        );

        let store = service.store().unwrap();
        assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
        assert_eq!(store.projection().last_sequence(), before_seq);
        assert_eq!(
            current_attempts(&store, &message_id, [bob, carol]),
            before_attempts
        );
        assert!(store
            .projection()
            .active_notification_barriers()
            .iter()
            .any(|record| record.attempt_id == attempt(2)));
    }

    #[test]
    fn a_claimed_legacy_attention_barrier_retires_without_a_terminal_action() {
        let scratch = StoreScratch::new("claimed-legacy-barrier-retirement");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-claimed-legacy-barrier").unwrap();
        let attempt_id = attempt(1);
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Legacy", None),
                1,
            )
            .unwrap();
        legacy_alarm(
            &mut store,
            &message_id,
            bob,
            attempt_id,
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            2,
        );
        let record = store
            .projection()
            .notification(bob, &message_id)
            .unwrap()
            .clone();
        assert!(store
            .retire_notification_barrier(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::RecipientClaimedComposerClear,
                None,
            )
            .is_err());

        store.claim_at(bob, message_id.clone(), 20).unwrap();
        store
            .retire_notification_barrier(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::RecipientClaimedComposerClear,
                None,
            )
            .unwrap();
        assert!(store.projection().active_notification_barriers().is_empty());
        assert_eq!(
            store.projection().notification(bob, &message_id),
            Some(&record)
        );
        assert!(matches!(
            &store.projection().get_entry(bob, &message_id).unwrap().state,
            MailboxEntryState::Claimed { claimant, .. } if *claimant == bob
        ));
        drop(store);

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened
            .projection()
            .active_notification_barriers()
            .is_empty());
    }

    #[test]
    fn a_claimed_legacy_notified_barrier_retires_after_clean_recovery() {
        let scratch = StoreScratch::new("claimed-legacy-notified-retirement");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-claimed-legacy-notified").unwrap();
        let attempt_id = attempt(1);
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Legacy notified", None),
                1,
            )
            .unwrap();
        for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
            .into_iter()
            .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    state,
                    None,
                    None,
                    2 + offset as u64,
                )
                .unwrap();
        }
        store
            .append_notification_transition_with_transport_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                Some(legacy_notification_binding(bob)),
                Some(NotificationTransport::Doorbell),
                Some(DOORBELL_FORMAT_COMPACT_CLAIM),
                None,
                4,
            )
            .unwrap();
        for (offset, state) in [
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
            NotificationState::Notified,
        ]
        .into_iter()
        .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    state,
                    None,
                    None,
                    5 + offset as u64,
                )
                .unwrap();
        }
        store.claim_at(bob, message_id.clone(), 9).unwrap();

        store
            .retire_notification_barrier(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::RecipientClaimedComposerClear,
                None,
            )
            .unwrap();

        assert!(store.projection().active_notification_barriers().is_empty());
        assert_eq!(
            store
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .state,
            NotificationState::Notified
        );
    }

    #[test]
    fn a_claim_after_writing_remains_valid_when_attention_lands_later() {
        let scratch = StoreScratch::new("claim-between-write-and-attention");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-claim-between-write-and-attention").unwrap();
        let attempt_id = attempt(1);
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Legacy", None),
                1,
            )
            .unwrap();
        for (state, ts) in [
            (NotificationState::Queued, 2),
            (NotificationState::Gating, 3),
        ] {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    state,
                    None,
                    None,
                    ts,
                )
                .unwrap();
        }
        store
            .append_notification_transition_with_transport_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                Some(legacy_notification_binding(bob)),
                Some(NotificationTransport::Doorbell),
                Some(DOORBELL_FORMAT_COMPACT_CLAIM),
                None,
                4,
            )
            .unwrap();
        store.claim_at(bob, message_id.clone(), 5).unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
                6,
            )
            .unwrap();

        store
            .retire_notification_barrier(
                message_id,
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::RecipientClaimedComposerClear,
                None,
            )
            .unwrap();
        assert!(store.projection().active_notification_barriers().is_empty());
    }

    #[test]
    fn a_failed_batch_append_changes_neither_projection_nor_replay() {
        let (scratch, mut store, message_id, bob, carol) =
            broadcast_operator_store("requeue-batch-append-failure");
        let journal = Path::new("workspaces/current/messages.ndjson");
        let before_seq = store.projection().last_sequence();
        let before_bytes = std::fs::read(store.journal_path()).unwrap();
        let before_attempts = current_attempts(&store, &message_id, [bob, carol]);
        store.inject_next_batch_append_failure();
        let (sender, _) = broadcast::channel(8);
        let mut events = sender.subscribe();
        let service =
            MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

        assert!(matches!(
            service.requeue_message(message_id.clone()),
            Err(MailboxServiceError::Store(MessageStoreError::Ledger(
                LedgerError::Io { .. }
            )))
        ));
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let store = service.store().unwrap();
        assert_eq!(store.projection().last_sequence(), before_seq);
        assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
        for recipient in [bob, carol] {
            assert_eq!(
                store
                    .projection()
                    .notification(recipient, &message_id)
                    .unwrap()
                    .attempt_id,
                before_attempts[&recipient]
            );
        }
        drop(store);
        drop(service);

        let reopened =
            MessageStore::open(&scratch.root(), journal, test_context().0, "boot-2").unwrap();
        assert_eq!(reopened.projection().last_sequence(), before_seq);
        for recipient in [bob, carol] {
            assert_eq!(
                reopened
                    .projection()
                    .notification(recipient, &message_id)
                    .unwrap()
                    .attempt_id,
                before_attempts[&recipient]
            );
        }
    }

    #[test]
    fn strict_replay_removes_a_torn_batch_without_moving_any_recipient() {
        let (scratch, store, message_id, bob, carol) =
            broadcast_operator_store("requeue-batch-torn-tail");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let before_seq = store.projection().last_sequence().unwrap();
        let before_bytes = std::fs::read(store.journal_path()).unwrap();
        let before_attempts = current_attempts(&store, &message_id, [bob, carol]);
        drop(store);

        let line = batch_requeue_line(
            before_seq + 1,
            &message_id,
            vec![
                NotificationRequeue {
                    prior_attempt_id: before_attempts[&bob],
                    attempt_id: attempt(3),
                    recipient: bob,
                },
                NotificationRequeue {
                    prior_attempt_id: before_attempts[&carol],
                    attempt_id: attempt(4),
                    recipient: carol,
                },
            ],
        );
        let bytes = serde_json::to_vec(&line).unwrap();
        let mut file = root.open_append(journal).unwrap();
        file.write_all(&bytes[..bytes.len() / 2]).unwrap();
        file.sync_data().unwrap();
        drop(file);

        let reopened = MessageStore::open(&root, journal, test_context().0, "boot-2").unwrap();
        assert_eq!(reopened.projection().last_sequence(), Some(before_seq));
        assert_eq!(
            std::fs::read(reopened.journal_path()).unwrap(),
            before_bytes
        );
        for recipient in [bob, carol] {
            let record = reopened
                .projection()
                .notification(recipient, &message_id)
                .unwrap();
            assert_eq!(record.state, NotificationState::AttentionRequired);
            assert_eq!(record.attempt_id, before_attempts[&recipient]);
        }
    }

    #[test]
    fn a_late_invalid_batch_target_refuses_before_any_projection_change() {
        let (_scratch, mut store, message_id, bob, carol) =
            broadcast_operator_store("requeue-batch-late-refusal");
        let before_seq = store.projection().last_sequence().unwrap();
        let line = batch_requeue_line(
            before_seq + 1,
            &message_id,
            vec![
                NotificationRequeue {
                    prior_attempt_id: attempt(1),
                    attempt_id: attempt(3),
                    recipient: bob,
                },
                NotificationRequeue {
                    prior_attempt_id: attempt(99),
                    attempt_id: attempt(4),
                    recipient: carol,
                },
            ],
        );

        assert!(matches!(
            store.projection.apply_line(&line),
            Err(MailboxError::NotificationAttemptMismatch { expected, found })
                if expected == attempt(2) && found == attempt(99)
        ));
        assert_eq!(store.projection().last_sequence(), Some(before_seq));
        for (recipient, attempt_id) in [(bob, attempt(1)), (carol, attempt(2))] {
            let record = store
                .projection()
                .notification(recipient, &message_id)
                .unwrap();
            assert_eq!(record.state, NotificationState::AttentionRequired);
            assert_eq!(record.attempt_id, attempt_id);
        }
    }

    /// Requeue and clear both refuse anything that is not an alarm.
    #[test]
    fn only_an_alarm_can_be_requeued_or_cleared() {
        let (_scratch, mut store, message_id, bob) = operator_store("only-alarms");
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Queued,
                None,
                None,
                2,
            )
            .unwrap();

        assert!(store
            .requeue_notification_at(message_id.clone(), bob, attempt(1), attempt(2), 3)
            .is_err());
        assert!(store
            .clear_notification_at(message_id.clone(), bob, attempt(1), 4)
            .is_err());
        // Neither refusal wrote anything.
        assert_eq!(store.projection().last_sequence(), Some(2));
    }

    /// A head whose attempt an operator resolved is not mailbox attention,
    /// even while its message is still the pending head.
    #[test]
    fn a_resolved_head_is_not_mailbox_attention() {
        let (_scratch, mut store, message_id, bob) = operator_store("resolved-head");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        let labels = HashMap::new();
        let before = store.projection().mailbox_attention_rows(&labels);
        assert_eq!(
            before.len(),
            1,
            "an open alarm on the head is one row: {before:?}"
        );
        assert_eq!(before[0].attempt_id, Some(attempt(1)));

        store
            .record_notification_resolution_intent(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        store
            .record_notification_resolution_action_accepted(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        store
            .resolve_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        let after = store.projection().mailbox_attention_rows(&labels);
        assert!(
            after.is_empty(),
            "a resolved attempt is neither an alarm nor a held head: {after:?}"
        );
    }

    /// Clearing twice acknowledges once. A repeated command must not grow
    /// the journal, or an operator retrying a timed-out call rewrites
    /// history for no reason.
    #[test]
    fn clearing_an_alarm_twice_appends_one_fact() {
        let (_scratch, mut store, message_id, bob) = operator_store("clear-idempotent");
        alarm(&mut store, &message_id, bob, attempt(1), 2);

        store
            .clear_notification_at(message_id.clone(), bob, attempt(1), 10)
            .unwrap();
        let after_first = store.projection().last_sequence();
        assert!(store.projection().alarm_cleared(attempt(1)));
        assert!(store.projection().open_alarms().is_empty());

        store
            .clear_notification_at(message_id.clone(), bob, attempt(1), 11)
            .unwrap();
        assert_eq!(store.projection().last_sequence(), after_first);

        let (_, _, _, carol) = test_context();
        let service = MailboxService::new(operator_directory(bob, carol), store);
        assert!(service.requeue_message(message_id).unwrap().is_empty());
        assert_eq!(
            service.store().unwrap().projection().last_sequence(),
            after_first
        );
    }

    #[test]
    fn clearing_several_alarms_appends_one_replayable_fact() {
        let (scratch, store, message_id, bob, carol) =
            broadcast_operator_store("clear-batch-atomic");
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, _, _) = test_context();
        let before = store.projection().last_sequence().unwrap();
        let (sender, _) = broadcast::channel(8);
        let mut events = sender.subscribe();
        let service =
            MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

        let requested = [attempt(2), attempt(1), attempt(1)];
        let summaries = service.clear_alarms(admin, &requested, None).unwrap();
        assert_eq!(
            summaries
                .iter()
                .map(|record| record.attempt_id)
                .collect::<Vec<_>>(),
            requested
        );
        assert_eq!(summaries.len(), requested.len());
        assert_eq!(summaries[0].message_id, message_id);
        assert_eq!(summaries[0].recipient, carol);
        assert_eq!(summaries[1].recipient, bob);
        assert_eq!(
            summaries[0].cause,
            Some(NotificationAttentionCause::SubmitFailed)
        );
        let store = service.store().unwrap();
        assert_eq!(store.projection().last_sequence(), Some(before + 1));
        assert!(store.projection().alarm_cleared(attempt(1)));
        assert!(store.projection().alarm_cleared(attempt(2)));
        drop(store);
        let line = service.journal_lines().unwrap().pop().unwrap();
        assert_eq!(line.data.as_ref().unwrap()["type"], "notifications_cleared");
        let fact: NotificationFact = serde_json::from_value(line.data.unwrap()).unwrap();
        let NotificationFact::NotificationsCleared { attempt_ids, .. } = fact else {
            panic!("last fact was not an atomic clearance");
        };
        assert_eq!(attempt_ids, vec![attempt(1), attempt(2)]);
        next_change(&mut events, before + 1, &[MessagesChangedArea::Attention]);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop(service);

        let reopened = MessageStore::open(&scratch.root(), journal, workspace, "boot-2").unwrap();
        assert!(reopened.projection().alarm_cleared(attempt(1)));
        assert!(reopened.projection().alarm_cleared(attempt(2)));
        assert!(reopened.projection().open_alarms().is_empty());
    }

    #[test]
    fn a_failed_clear_batch_changes_neither_journal_nor_projection() {
        let (scratch, mut store, _, bob, carol) =
            broadcast_operator_store("clear-batch-append-failure");
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, _, _) = test_context();
        let before_seq = store.projection().last_sequence();
        let before_bytes = std::fs::read(store.journal_path()).unwrap();
        store.inject_next_batch_append_failure();
        let (sender, _) = broadcast::channel(8);
        let mut events = sender.subscribe();
        let service =
            MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

        assert!(matches!(
            service.clear_alarms(admin, &[attempt(1), attempt(2)], None),
            Err(MailboxServiceError::Store(MessageStoreError::Ledger(
                LedgerError::Io { .. }
            )))
        ));
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let store = service.store().unwrap();
        assert_eq!(store.projection().last_sequence(), before_seq);
        assert!(!store.projection().alarm_cleared(attempt(1)));
        assert!(!store.projection().alarm_cleared(attempt(2)));
        assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
        drop(store);
        drop(service);

        let reopened = MessageStore::open(&scratch.root(), journal, workspace, "boot-2").unwrap();
        assert_eq!(reopened.projection().last_sequence(), before_seq);
        assert_eq!(reopened.projection().open_alarms().len(), 2);
    }

    #[test]
    fn an_age_clear_refuses_the_whole_batch_when_one_alarm_is_newer() {
        let (_scratch, store, _, bob, carol) = broadcast_operator_store("clear-batch-cutoff");
        let (_, admin, _, _) = test_context();
        let before_seq = store.projection().last_sequence();
        let before_bytes = std::fs::read(store.journal_path()).unwrap();
        let cutoff = store
            .projection()
            .notification_by_attempt(attempt(1))
            .unwrap()
            .updated_at;
        let service = MailboxService::new(operator_directory(bob, carol), store);

        assert!(matches!(
            service.clear_alarms(admin, &[attempt(1), attempt(2)], Some(cutoff)),
            Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)))
                if matches!(*error, MailboxError::NotificationNewerThanClearCutoff { attempt_id, .. }
                    if attempt_id == attempt(2))
        ));
        let store = service.store().unwrap();
        assert_eq!(store.projection().last_sequence(), before_seq);
        assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
        assert!(!store.projection().alarm_cleared(attempt(1)));
        assert!(!store.projection().alarm_cleared(attempt(2)));
    }

    /// A clearance names one attempt and cannot land on the attempt that
    /// replaced it. Otherwise an operator clearing what they previewed
    /// silences an alarm raised after they looked.
    #[test]
    fn a_clearance_never_lands_on_a_newer_attempt() {
        let (_scratch, mut store, message_id, bob) = operator_store("clear-superseded");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        store
            .requeue_notification_at(message_id.clone(), bob, attempt(1), attempt(2), 10)
            .unwrap();
        alarm(&mut store, &message_id, bob, attempt(2), 11);
        let before = store.projection().last_sequence();

        // The identifier the operator previewed is gone; the alarm now
        // standing is a different attempt and keeps standing.
        assert!(store
            .clear_notification_at(message_id.clone(), bob, attempt(1), 20)
            .is_err());
        assert_eq!(store.projection().last_sequence(), before);
        assert!(!store.projection().alarm_cleared(attempt(2)));
        assert_eq!(store.projection().open_alarms().len(), 1);
    }

    /// An acknowledgement is durable. A restart that forgot it would show
    /// the operator an alarm they have already dealt with.
    #[test]
    fn a_cleared_alarm_stays_cleared_across_a_restart() {
        let scratch = StoreScratch::new("clear-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-restart-clear").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            alarm(&mut store, &message_id, bob, attempt(1), 2);
            store
                .clear_notification_at(message_id.clone(), bob, attempt(1), 10)
                .unwrap();
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened.projection().alarm_cleared(attempt(1)));
        assert!(reopened.projection().open_alarms().is_empty());
        // The attempt itself is untouched: clearing acknowledges, it does
        // not rewrite the outcome that was recorded.
        let record = reopened
            .projection()
            .notification(bob, &message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::AttentionRequired);
        assert_eq!(record.attempt_id, attempt(1));
    }

    #[test]
    fn resolution_is_content_free_durable_and_not_repeatable() {
        let scratch = StoreScratch::new("resolve-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-resolve").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            alarm(&mut store, &message_id, bob, attempt(1), 2);
            let before_intent = store.projection().last_sequence();
            assert!(matches!(
                store.resolve_notification(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), before_intent);
            assert_eq!(store.projection().active_notification_barriers().len(), 1);
            store
                .record_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            assert!(!store.projection().attention_resolved(attempt(1)));
            assert_eq!(
                store.projection().active_notification_barriers().len(),
                1,
                "a durable intent is not a completed resolution"
            );
            let before_acceptance = store.projection().last_sequence();
            assert!(matches!(
                store.resolve_notification(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), before_acceptance);
            store
                .record_notification_resolution_action_accepted(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            let before_consumption = store.projection().last_sequence();
            assert!(matches!(
                store.resolve_notification(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), before_consumption);
            store
                .record_notification_resolution_consumption_observed(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    exact_consumption(9),
                )
                .unwrap();
            store
                .resolve_notification(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            assert!(store.projection().attention_resolved(attempt(1)));
            assert!(store.projection().active_notification_barriers().is_empty());
            assert!(store.projection().open_alarms().is_empty());
            let before_repeat = store.projection().last_sequence();
            assert!(matches!(
                store.resolve_notification(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationAlreadyResolved(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), before_repeat);
            let text = std::fs::read_to_string(store.journal_path()).unwrap();
            let resolution: LedgerLine =
                serde_json::from_str(text.lines().last().unwrap()).unwrap();
            assert!(resolution.subject.is_none());
            assert!(resolution.body.is_none());
            let data = resolution.data.as_ref().unwrap();
            assert_eq!(data["type"], "notification_resolved");
            assert_eq!(data["resolution"], "complete");
            assert_eq!(data["proof_version"], NOTIFICATION_RESOLUTION_PROOF_VERSION);
            assert!(data.get("composer").is_none());
            assert!(data.get("diff").is_none());
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened.projection().attention_resolved(attempt(1)));
        assert!(reopened.projection().open_alarms().is_empty());
        let snapshot = reopened
            .projection()
            .messages_snapshot(admin, 20, &routes([bob]))
            .unwrap();
        let recipient = &snapshot.rows[0].recipients[0];
        assert_eq!(
            recipient.notification.resolution,
            Some(NotificationResolution::Complete)
        );
        assert_eq!(recipient.notification.resolution_action_accepted, None);
        assert_eq!(recipient.notification.resolution_consumption_observed, None);
        assert!(!recipient.needs_action);
        assert_eq!(snapshot.counts.open_attention_entries, 0);
        assert_eq!(snapshot.counts.work_messages, 0);
    }

    fn assert_legacy_staged_submit_replays(
        tag: &str,
        transport: NotificationTransport,
        doorbell_format: Option<u32>,
    ) {
        let scratch = StoreScratch::new(tag);
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new(format!("m-{tag}")).unwrap();
        let submitted = {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
                .into_iter()
                .enumerate()
            {
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        attempt(1),
                        state,
                        None,
                        None,
                        2 + offset as u64,
                    )
                    .unwrap();
            }
            store
                .append_notification_transition_with_transport_at(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationState::Writing,
                    Some(legacy_notification_binding(bob)),
                    Some(transport),
                    doorbell_format,
                    None,
                    4,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationState::Staged,
                    None,
                    None,
                    5,
                )
                .unwrap();
            let line = sample_notification_state_line(
                store.projection().last_sequence().unwrap() + 1,
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Submitted,
            );
            assert!(matches!(
                store.projection.apply_line(&line),
                Err(MailboxError::InvalidNotificationTransition {
                    from: NotificationState::Staged,
                    to: NotificationState::Submitted,
                })
            ));
            line
        };
        let mut file = root.open_append(journal).unwrap();
        serde_json::to_writer(&mut file, &submitted).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_data().unwrap();

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert_eq!(
            reopened
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .state,
            NotificationState::Submitted
        );
    }

    #[test]
    fn shipped_staged_submit_edges_replay_without_weakening_live_appends() {
        assert_legacy_staged_submit_replays(
            "legacy-verbose-submit",
            NotificationTransport::Doorbell,
            None,
        );
        assert_legacy_staged_submit_replays(
            "legacy-doorbell-submit",
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
        );
        assert_legacy_staged_submit_replays(
            "legacy-direct-submit",
            NotificationTransport::DirectPayload,
            None,
        );
    }

    #[test]
    fn current_staged_submit_edge_is_rejected_during_replay() {
        let scratch = StoreScratch::new("current-direct-submit-refused");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-current-direct-submit").unwrap();
        let submitted = {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
                .into_iter()
                .enumerate()
            {
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        attempt(1),
                        state,
                        None,
                        None,
                        2 + offset as u64,
                    )
                    .unwrap();
            }
            store
                .append_notification_transition_with_transport_at(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationState::Writing,
                    Some(notification_binding(bob)),
                    Some(NotificationTransport::Doorbell),
                    Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
                    None,
                    4,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationState::Staged,
                    None,
                    None,
                    5,
                )
                .unwrap();
            sample_notification_state_line(
                store.projection().last_sequence().unwrap() + 1,
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Submitted,
            )
        };
        let mut file = root.open_append(journal).unwrap();
        serde_json::to_writer(&mut file, &submitted).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_data().unwrap();

        assert!(matches!(
            MessageStore::open(&root, journal, workspace, "boot-2"),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::InvalidNotificationTransition {
                    from: NotificationState::Staged,
                    to: NotificationState::Submitted,
                })
        ));
    }

    #[test]
    fn legacy_resolution_replays_only_for_legacy_write_contracts() {
        let scratch = StoreScratch::new("legacy-resolution-replay");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-legacy-resolution").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob, carol], "Op", None),
                    1,
                )
                .unwrap();
            legacy_alarm(
                &mut store,
                &message_id,
                bob,
                attempt(1),
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_COMPACT_CLAIM),
                2,
            );
            legacy_alarm(
                &mut store,
                &message_id,
                carol,
                attempt(2),
                NotificationTransport::DirectPayload,
                None,
                10,
            );
            store
                .record_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            for (recipient, attempt_id, ts) in [(bob, attempt(1), 20), (carol, attempt(2), 21)] {
                append_resolution_at(
                    &mut store,
                    &message_id,
                    recipient,
                    attempt_id,
                    0,
                    NotificationResolution::Complete,
                    ts,
                )
                .unwrap();
            }
            let text = std::fs::read_to_string(store.journal_path()).unwrap();
            let resolved: serde_json::Value =
                serde_json::from_str(text.lines().last().unwrap()).unwrap();
            assert!(resolved["data"].get("proof_version").is_none());
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened.projection().attention_resolved(attempt(1)));
        assert!(reopened.projection().attention_resolved(attempt(2)));
        assert!(reopened.projection().open_alarms().is_empty());
        assert!(reopened
            .projection()
            .active_notification_barriers()
            .is_empty());
        assert!(reopened.projection().resolution_actions_accepted.is_empty());
        assert!(reopened.projection().resolution_consumptions.is_empty());
    }

    #[test]
    fn legacy_resolution_refuses_downgrades_mismatches_and_hybrids() {
        let (_scratch, mut store, message_id, bob) = operator_store("legacy-resolution-refuse");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        store
            .record_notification_resolution_intent(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Complete,
            )
            .unwrap();
        let before = store.projection().last_sequence();
        assert!(matches!(
            append_resolution_at(
                &mut store,
                &message_id,
                bob,
                attempt(1),
                0,
                NotificationResolution::Complete,
                10,
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
        ));
        assert_eq!(store.projection().last_sequence(), before);

        let (_scratch, mut store, message_id, bob) =
            operator_store("legacy-resolution-intent-mismatch");
        legacy_alarm(
            &mut store,
            &message_id,
            bob,
            attempt(1),
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            2,
        );
        store
            .record_notification_resolution_intent(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        assert!(matches!(
            append_resolution_at(
                &mut store,
                &message_id,
                bob,
                attempt(1),
                0,
                NotificationResolution::Complete,
                10,
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
        ));

        let (_scratch, mut store, message_id, bob) = operator_store("legacy-resolution-hybrid");
        legacy_alarm(
            &mut store,
            &message_id,
            bob,
            attempt(1),
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            2,
        );
        store
            .record_notification_resolution_intent(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        store
            .record_notification_resolution_action_accepted(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        assert!(matches!(
            append_resolution_at(
                &mut store,
                &message_id,
                bob,
                attempt(1),
                0,
                NotificationResolution::Discard,
                10,
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
        ));
    }

    #[test]
    fn unknown_resolution_proof_version_is_rejected() {
        let (_scratch, mut store, message_id, bob) = operator_store("resolution-proof-version");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        let before = store.projection().last_sequence();
        assert!(matches!(
            append_resolution_at(
                &mut store,
                &message_id,
                bob,
                attempt(1),
                99,
                NotificationResolution::Complete,
                10,
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::InvalidNotificationFact(message)
                    if message.contains("unsupported notification resolution proof version 99"))
        ));
        assert_eq!(store.projection().last_sequence(), before);
    }

    #[test]
    fn current_discard_requires_terminal_action_acceptance() {
        let (_scratch, mut store, message_id, bob) = operator_store("discard-proof-boundary");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        store
            .record_notification_resolution_intent(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        let before_acceptance = store.projection().last_sequence();
        assert!(matches!(
            store.resolve_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
        ));
        assert_eq!(store.projection().last_sequence(), before_acceptance);

        store
            .record_notification_resolution_action_accepted(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();
        store
            .resolve_notification(message_id, bob, attempt(1), NotificationResolution::Discard)
            .unwrap();
        assert!(store.projection().attention_resolved(attempt(1)));
    }

    #[test]
    fn no_key_discard_can_resolve_from_its_matching_intent() {
        let (_scratch, mut store, message_id, bob) = operator_store("resolve-no-key-discard");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        store
            .record_notification_resolution_intent(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationResolution::Discard,
            )
            .unwrap();

        assert_eq!(
            store
                .projection()
                .attention_resolution_action_accepted(attempt(1)),
            None
        );
        assert_eq!(
            store
                .projection()
                .attention_resolution_consumption_observed(attempt(1)),
            None
        );
        let before_invalid_consumption = store.projection().last_sequence();
        assert!(matches!(
            store.record_notification_resolution_consumption_observed(
                message_id.clone(),
                bob,
                attempt(1),
                exact_consumption(9),
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
        ));
        assert_eq!(
            store.projection().last_sequence(),
            before_invalid_consumption
        );
        store
            .resolve_notification_without_terminal_action(message_id, bob, attempt(1))
            .unwrap();
        assert!(store.projection().attention_resolved(attempt(1)));
    }

    #[test]
    fn fresh_no_key_discard_is_one_atomic_replayable_fact() {
        let (scratch, mut store, message_id, bob) = operator_store("resolve-no-key-atomic");
        let journal = Path::new("workspaces/current/messages.ndjson");
        let workspace = store.projection().workspace_id();
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        store
            .resolve_notification_without_terminal_action(message_id.clone(), bob, attempt(1))
            .unwrap();
        assert!(store.projection().attention_resolved(attempt(1)));
        assert_eq!(
            store.projection().attention_resolution_intent(attempt(1)),
            None
        );
        drop(store);

        let reopened = MessageStore::open(&scratch.root(), journal, workspace, "boot-2").unwrap();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "bob".into(),
            }],
        )
        .unwrap();
        let service = MailboxService::new(directory, reopened);
        assert!(service
            .store()
            .unwrap()
            .projection()
            .attention_resolved(attempt(1)));
        let lines = service.journal_lines().unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.data.as_ref().is_some_and(|data| {
                        data["type"] == "notification_resolved_without_terminal_action"
                    })
                })
                .count(),
            1
        );
    }

    #[test]
    fn delayed_old_attempt_hook_cannot_confirm_its_replacement() {
        let (_scratch, store, message_id, bob) = operator_store("attempt-bound-hook");
        let workspace = store.projection().workspace_id();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "bob".into(),
            }],
        )
        .unwrap();
        let service = MailboxService::new(directory, store);
        let old_attempt = attempt(1);
        let replacement_attempt = attempt(2);
        let replacement_payload = cyclops_proto::render_doorbell_v3(replacement_attempt);
        let signal = Arc::new(AttentionConsumptionSignal::new());
        service
            .attention_consumption_candidates
            .lock()
            .unwrap()
            .insert(
                replacement_attempt,
                AttentionConsumptionCandidate {
                    message_id: message_id.clone(),
                    recipient: bob,
                    session_idx: 7,
                    pane_id: "%1".into(),
                    pane_root: ProcessInstanceId::new(40, 400).unwrap(),
                    agent: ProcessInstanceId::new(41, 401).unwrap(),
                    manifest: "claude".into(),
                    expected_payload: replacement_payload.clone(),
                    not_before_ms: 100,
                    signal: Arc::clone(&signal),
                },
            );
        let pane_root = crate::identity::ProcId {
            pid: 40,
            birth: 400,
        };
        let agent = crate::identity::ProcId {
            pid: 41,
            birth: 401,
        };

        assert!(!service.confirm_attention_consumption_hook(
            7,
            "%1",
            bob,
            pane_root,
            agent,
            "claude",
            &cyclops_proto::render_doorbell_v3(old_attempt),
            101,
        ));
        assert!(!service.confirm_attention_consumption_hook(
            7,
            "%1",
            bob,
            pane_root,
            agent,
            "claude",
            &cyclops_proto::render_doorbell_v1(&message_id),
            101,
        ));
        assert_eq!(signal.observation(), None);

        assert!(service.confirm_attention_consumption_hook(
            7,
            "%1",
            bob,
            pane_root,
            agent,
            "claude",
            &replacement_payload,
            101,
        ));
        assert_eq!(
            signal.observation().map(|observation| observation.evidence),
            Some(NotificationResolutionConsumptionEvidence::ExactHookPrompt)
        );
    }

    #[test]
    fn unmatched_resolution_intent_replays_as_uncertain() {
        let scratch = StoreScratch::new("resolution-intent-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-intent").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            alarm(&mut store, &message_id, bob, attempt(1), 2);
            store
                .record_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            assert!(store.projection().attention_resolution_pending(attempt(1)));
            let before_repeat = store.projection().last_sequence();
            assert!(matches!(
                store.record_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), before_repeat);
            assert!(matches!(
                store.requeue_notification_at(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    attempt(2),
                    10,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), before_repeat);

            let text = std::fs::read_to_string(store.journal_path()).unwrap();
            let intent: LedgerLine = serde_json::from_str(text.lines().last().unwrap()).unwrap();
            assert!(intent.subject.is_none());
            assert!(intent.body.is_none());
            assert_eq!(
                intent.data.as_ref().unwrap()["type"],
                "notification_resolution_intent"
            );
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened
            .projection()
            .attention_resolution_pending(attempt(1)));
        assert!(!reopened.projection().attention_resolved(attempt(1)));
        assert_eq!(reopened.projection().open_alarms().len(), 1);
        let snapshot = reopened
            .projection()
            .messages_snapshot(admin, 20, &routes([bob]))
            .unwrap();
        let recipient = &snapshot.rows[0].recipients[0];
        assert_eq!(
            recipient.notification.resolution_intent,
            Some(NotificationResolution::Complete)
        );
        assert_eq!(recipient.notification.resolution, None);
        assert!(recipient.needs_action);
        assert!(!recipient.can_manage_attention);

        let target = AttentionTarget {
            record: reopened
                .projection()
                .alarm_by_attempt(attempt(1))
                .unwrap()
                .clone(),
        };
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "bob".into(),
            }],
        )
        .unwrap();
        let service = MailboxService::new(directory, reopened);
        assert!(matches!(
            service.begin_attention_resolution(&target, NotificationResolution::Discard),
            Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
        ));
        assert_eq!(
            service
                .begin_attention_resolution(&target, NotificationResolution::Complete)
                .unwrap(),
            AttentionResolutionStart::IntentOnlyUncertain
        );
        service.cancel_attention_resolution(attempt(1)).unwrap();
        let store = service.store().unwrap();
        assert!(!store.projection().attention_resolved(attempt(1)));
        assert!(store.projection().attention_resolution_pending(attempt(1)));
        assert_eq!(store.projection().active_notification_barriers().len(), 1);
    }

    #[test]
    fn accepted_resolution_action_replays_and_is_idempotent() {
        let scratch = StoreScratch::new("resolution-action-accepted");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-action-accepted").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            alarm(&mut store, &message_id, bob, attempt(1), 2);
            store
                .record_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();

            let before_acceptance = store.projection().last_sequence();
            store
                .record_notification_resolution_action_accepted(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            let accepted_seq = store.projection().last_sequence();
            assert!(accepted_seq > before_acceptance);
            assert_eq!(
                store
                    .projection()
                    .attention_resolution_action_accepted(attempt(1)),
                Some(NotificationResolution::Complete)
            );

            store
                .record_notification_resolution_action_accepted(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            assert_eq!(store.projection().last_sequence(), accepted_seq);

            assert!(matches!(
                store.record_notification_resolution_action_accepted(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Discard,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), accepted_seq);
            assert!(matches!(
                store.withdraw_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), accepted_seq);
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert_eq!(
            reopened
                .projection()
                .attention_resolution_action_accepted(attempt(1)),
            Some(NotificationResolution::Complete)
        );
        let snapshot = reopened
            .projection()
            .messages_snapshot(admin, 20, &routes([bob]))
            .unwrap();
        let notification = &snapshot.rows[0].recipients[0].notification;
        assert_eq!(
            notification.resolution_intent,
            Some(NotificationResolution::Complete)
        );
        assert_eq!(
            notification.resolution_action_accepted,
            Some(NotificationResolution::Complete)
        );
        assert_eq!(notification.resolution_consumption_observed, None);
        assert_eq!(notification.resolution, None);
        let target = AttentionTarget {
            record: reopened
                .projection()
                .alarm_by_attempt(attempt(1))
                .unwrap()
                .clone(),
        };
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "bob".into(),
            }],
        )
        .unwrap();
        let service = MailboxService::new(directory, reopened);
        assert_eq!(
            service
                .begin_attention_resolution(&target, NotificationResolution::Complete)
                .unwrap(),
            AttentionResolutionStart::AcceptedUnconsumed
        );
    }

    #[test]
    fn observed_complete_consumption_replays_and_is_idempotent() {
        let scratch = StoreScratch::new("resolution-consumption-observed");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-consumption-observed").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            alarm(&mut store, &message_id, bob, attempt(1), 2);
            store
                .record_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            let before_acceptance = store.projection().last_sequence();
            assert!(matches!(
                store.record_notification_resolution_consumption_observed(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    exact_consumption(23),
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), before_acceptance);
            store
                .record_notification_resolution_action_accepted(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            let before_observation = store.projection().last_sequence();
            assert!(matches!(
                store.record_notification_resolution_consumption_observed(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    exact_consumption(0),
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::InvalidNotificationFact(_))
            ));
            assert_eq!(store.projection().last_sequence(), before_observation);
            store
                .record_notification_resolution_consumption_observed(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    exact_consumption(23),
                )
                .unwrap();
            let observed_seq = store.projection().last_sequence();
            let expected = NotificationResolutionConsumptionObservation {
                evidence: NotificationResolutionConsumptionEvidence::AuthenticatedClaim,
                observed_at_ms: 23,
            };
            assert_eq!(
                store
                    .projection()
                    .attention_resolution_consumption_observed(attempt(1)),
                Some(expected)
            );

            store
                .record_notification_resolution_consumption_observed(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    exact_consumption(23),
                )
                .unwrap();
            assert_eq!(store.projection().last_sequence(), observed_seq);
            assert!(matches!(
                store.record_notification_resolution_consumption_observed(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    exact_consumption(24),
                ),
                Err(MessageStoreError::Mailbox(error))
                    if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(1))
            ));
            assert_eq!(store.projection().last_sequence(), observed_seq);
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let expected = NotificationResolutionConsumptionObservation {
            evidence: NotificationResolutionConsumptionEvidence::AuthenticatedClaim,
            observed_at_ms: 23,
        };
        assert_eq!(
            reopened
                .projection()
                .attention_resolution_consumption_observed(attempt(1)),
            Some(expected)
        );
        let snapshot = reopened
            .projection()
            .messages_snapshot(admin, 20, &routes([bob]))
            .unwrap();
        assert_eq!(
            snapshot.rows[0].recipients[0]
                .notification
                .resolution_consumption_observed,
            Some(expected)
        );
        let target = AttentionTarget {
            record: reopened
                .projection()
                .alarm_by_attempt(attempt(1))
                .unwrap()
                .clone(),
        };
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "bob".into(),
            }],
        )
        .unwrap();
        let service = MailboxService::new(directory, reopened);
        assert_eq!(
            service
                .begin_attention_resolution(&target, NotificationResolution::Complete)
                .unwrap(),
            AttentionResolutionStart::ReconcileOnly
        );
    }

    #[test]
    fn broadcast_requeue_refuses_an_uncertain_action_before_any_append() {
        let scratch = StoreScratch::new("resolution-intent-broadcast-requeue");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-intent-broadcast").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Op", None),
                1,
            )
            .unwrap();
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        alarm(&mut store, &message_id, carol, attempt(2), 10);
        store
            .record_notification_resolution_intent(
                message_id.clone(),
                carol,
                attempt(2),
                NotificationResolution::Complete,
            )
            .unwrap();
        let before = store.projection().last_sequence();
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "reviewer".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "worker".into(),
                },
            ],
        )
        .unwrap();
        let service = MailboxService::new(directory, store);

        assert!(matches!(
            service.requeue_message(message_id.clone()),
            Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionAmbiguous(id) if *id == attempt(2))
        ));
        let store = service.store().unwrap();
        assert_eq!(store.projection().last_sequence(), before);
        assert_eq!(
            store
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .attempt_id,
            attempt(1)
        );
        assert_eq!(
            store
                .projection()
                .notification(carol, &message_id)
                .unwrap()
                .attempt_id,
            attempt(2)
        );
    }

    #[test]
    fn withdrawn_pre_key_intent_replays_as_retryable() {
        let scratch = StoreScratch::new("resolution-intent-withdrawn");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-withdrawn-intent").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
                .unwrap();
            alarm(&mut store, &message_id, bob, attempt(1), 2);
            store
                .record_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            store
                .withdraw_notification_resolution_intent(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationResolution::Complete,
                )
                .unwrap();
            assert!(!store.projection().attention_resolution_pending(attempt(1)));
        }

        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(!reopened
            .projection()
            .attention_resolution_pending(attempt(1)));
        reopened
            .record_notification_resolution_intent(
                message_id,
                bob,
                attempt(1),
                NotificationResolution::Complete,
            )
            .expect("withdrawn pre-key action may be retried");
    }

    #[test]
    fn resolution_refuses_the_wrong_attempt_without_writing() {
        let (_scratch, mut store, message_id, bob) = operator_store("resolve-wrong-attempt");
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        let before = store.projection().last_sequence();
        assert!(store
            .resolve_notification(message_id, bob, attempt(2), NotificationResolution::Discard,)
            .is_err());
        assert_eq!(store.projection().last_sequence(), before);
        assert!(!store.projection().attention_resolved(attempt(1)));
    }

    #[test]
    fn bare_message_target_refuses_broadcast_ambiguity_and_lists_attempts() {
        let scratch = StoreScratch::new("resolve-ambiguous");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-broadcast-attention").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Both", None),
                1,
            )
            .unwrap();
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        alarm(&mut store, &message_id, carol, attempt(2), 10);
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "bob".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "carol".into(),
                },
            ],
        )
        .unwrap();
        let service = MailboxService::new(directory, store);

        assert!(matches!(
            service.attention_target(message_id.as_str()),
            Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)))
                if matches!(error.as_ref(), MailboxError::AmbiguousAttentionTarget { candidates, .. }
                    if candidates == &vec![attempt(1), attempt(2)])
        ));
        assert_eq!(
            service
                .attention_target(&attempt(2).to_string())
                .unwrap()
                .record
                .recipient,
            carol
        );
    }

    /// A claim preserves post-write attention. Requeueing a broadcast creates
    /// a new attempt only for recipients whose mailbox entry is still pending.
    #[test]
    fn claim_keeps_its_alarm_but_broadcast_requeue_skips_the_claimed_entry() {
        let scratch = StoreScratch::new("requeue-whole");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-multi").unwrap();

        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Multi", None),
                1,
            )
            .unwrap();
        // carol's alarm is the older one, so it is processed first.
        alarm(&mut store, &message_id, carol, attempt(2), 2);
        alarm(&mut store, &message_id, bob, attempt(1), 10);
        // Bob claims his message, but the post-write alarm stays open.
        store.claim_at(bob, message_id.clone(), 20).unwrap();
        assert_eq!(
            store
                .projection()
                .open_alarms_for_message(&message_id)
                .len(),
            2
        );

        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: bob,
                    label: "bob".into(),
                },
                MailboxIdentity {
                    key: carol,
                    label: "carol".into(),
                },
            ],
        )
        .unwrap();
        let before = store.projection().last_sequence();
        let service = MailboxService::new(directory, store);

        let requeued = service
            .requeue_message(message_id.clone())
            .expect("requeue is not an error");
        assert_eq!(
            requeued.len(),
            1,
            "only the redeliverable alarm is requeued"
        );
        assert_eq!(requeued[0].recipient, carol);
        assert_eq!(requeued[0].state, NotificationState::Queued);
        assert_ne!(
            requeued[0].attempt_id,
            attempt(2),
            "a requeue mints a fresh attempt"
        );

        let store = service.store().expect("store lock");
        // Exactly one fact is appended for Carol's new attempt.
        assert_eq!(
            store.projection().last_sequence(),
            before.map(|s| s + 1),
            "a requeue wrote more or less than the one fact it reported"
        );
        let bobs = store.projection().notification(bob, &message_id).unwrap();
        assert_eq!(bobs.attempt_id, attempt(1));
        assert_eq!(bobs.state, NotificationState::AttentionRequired);
        let retired = store
            .projection()
            .alarm_by_attempt(attempt(1))
            .expect("a claim preserves the current attempt identity");
        assert_eq!(retired.recipient, bob);
        assert_eq!(retired.state, NotificationState::AttentionRequired);
        assert_eq!(store.projection().open_alarms().len(), 1);
    }

    /// The reason an alarm was raised survives a restart.
    ///
    /// The cause is the point of preview: an operator restarting the
    /// daemon and seeing every alarm reduced to "attention required" has
    /// lost the one fact that says what to do about it.
    #[test]
    fn an_alarm_cause_survives_a_restart() {
        let scratch = StoreScratch::new("cause-replay");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let verify = MessageId::new("m-verify").unwrap();
        let submit = MessageId::new("m-submit").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(verify.clone(), draft(admin, vec![bob], "V", None), 1)
                .unwrap();
            store
                .accept_at(submit.clone(), draft(admin, vec![carol], "S", None), 2)
                .unwrap();
            alarm_because(
                &mut store,
                &verify,
                bob,
                attempt(1),
                10,
                NotificationAttentionCause::VerifyFailed,
            );
            alarm_because(
                &mut store,
                &submit,
                carol,
                attempt(2),
                20,
                NotificationAttentionCause::SubmitFailed,
            );
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let alarms = reopened.projection().open_alarms();
        assert_eq!(alarms.len(), 2);
        // Oldest first, each still carrying the cause it was raised with.
        assert_eq!(alarms[0].message_id, verify);
        assert_eq!(
            alarms[0].cause,
            Some(NotificationAttentionCause::VerifyFailed)
        );
        assert_eq!(
            alarms[0].verify_outcome,
            Some(NotificationVerifyOutcome::ambiguous())
        );
        assert_eq!(alarms[1].message_id, submit);
        assert_eq!(
            alarms[1].cause,
            Some(NotificationAttentionCause::SubmitFailed)
        );
        assert_eq!(alarms[1].verify_outcome, None);

        let snapshot = reopened
            .projection()
            .messages_snapshot(admin, 10, &HashMap::new())
            .unwrap();
        let verify_summary = &snapshot
            .rows
            .iter()
            .find(|row| row.message_id == verify)
            .unwrap()
            .recipients[0]
            .notification;
        assert_eq!(
            verify_summary.verify_outcome,
            Some(NotificationVerifyOutcome::ambiguous())
        );
    }

    /// Preview is ordered oldest first and hides what has been cleared.
    #[test]
    fn preview_lists_uncleared_alarms_oldest_first() {
        let scratch = StoreScratch::new("preview-order");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let older = MessageId::new("m-older").unwrap();
        let newer = MessageId::new("m-newer").unwrap();
        store
            .accept_at(older.clone(), draft(admin, vec![bob], "Older", None), 1)
            .unwrap();
        store
            .accept_at(newer.clone(), draft(admin, vec![carol], "Newer", None), 2)
            .unwrap();
        alarm(&mut store, &older, bob, attempt(1), 10);
        alarm(&mut store, &newer, carol, attempt(2), 20);

        let alarms = store.projection().open_alarms();
        assert_eq!(alarms.len(), 2);
        assert_eq!(alarms[0].message_id, older);
        assert_eq!(alarms[1].message_id, newer);

        store
            .clear_notification_at(older.clone(), bob, attempt(1), 30)
            .unwrap();
        let alarms = store.projection().open_alarms();
        assert_eq!(alarms.len(), 1);
        assert_eq!(alarms[0].message_id, newer);
        assert_eq!(store.projection().open_alarms_for_message(&newer).len(), 1);
    }

    #[test]
    fn notification_binding_survives_restart_and_explicit_recovery() {
        let scratch = StoreScratch::new("notification-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-restart").unwrap();
        let attempt_id = attempt(1);
        let binding = notification_binding(bob);

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Restart", None),
                    1,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    NotificationState::Queued,
                    None,
                    None,
                    2,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    NotificationState::Gating,
                    None,
                    None,
                    3,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    NotificationState::Writing,
                    Some(binding.clone()),
                    None,
                    4,
                )
                .unwrap();
            let staged = store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    NotificationState::Staged,
                    None,
                    None,
                    5,
                )
                .unwrap();
            assert_eq!(staged.binding.as_ref(), Some(&binding));
            assert_eq!(
                store.projection().active_notification_barriers(),
                vec![staged.clone()]
            );
            assert_eq!(staged.transport, NotificationTransport::Doorbell);
            assert_eq!(staged.doorbell_format, None);
        }

        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let staged = reopened
            .projection()
            .notification(bob, &message_id)
            .unwrap();
        assert_eq!(staged.state, NotificationState::Staged);
        assert_eq!(staged.binding.as_ref(), Some(&binding));
        assert_eq!(staged.transport, NotificationTransport::Doorbell);
        assert_eq!(staged.doorbell_format, None);
        assert_eq!(reopened.projection().last_sequence(), Some(5));

        let recovered = reopened.recover_notifications_after_restart().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, NotificationState::AttentionRequired);
        assert_eq!(
            recovered[0].cause,
            Some(NotificationAttentionCause::DaemonRestart)
        );
        assert_eq!(recovered[0].binding.as_ref(), Some(&binding));
        assert_eq!(recovered[0].transport, NotificationTransport::Doorbell);
        assert_eq!(recovered[0].doorbell_format, None);
        assert_eq!(reopened.projection().last_sequence(), Some(6));
        assert_eq!(
            reopened.projection().active_notification_barriers(),
            vec![recovered[0].clone()]
        );

        drop(reopened);
        let replayed = MessageStore::open(&root, journal, workspace, "boot-3").unwrap();
        assert_eq!(
            replayed
                .projection()
                .notification(bob, &message_id)
                .unwrap(),
            &recovered[0]
        );
        assert_eq!(
            replayed.projection().active_notification_barriers(),
            vec![recovered[0].clone()]
        );
    }

    #[test]
    fn claimed_staged_doorbell_replays_for_the_same_attempt_reconciliation() {
        let scratch = StoreScratch::new("claimed-staged-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-claimed-staged").unwrap();
        let attempt_id = attempt(1);
        let binding = notification_binding(bob);

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Claimed staged", None),
                    1,
                )
                .unwrap();
            for (state, ts, binding) in [
                (NotificationState::Queued, 2, None),
                (NotificationState::Gating, 3, None),
                (NotificationState::Writing, 4, Some(binding.clone())),
                (NotificationState::Staged, 5, None),
            ] {
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        state,
                        binding,
                        None,
                        ts,
                    )
                    .unwrap();
            }
            store.claim_at(bob, message_id.clone(), 6).unwrap();
        }

        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened
            .recover_notifications_after_restart()
            .unwrap()
            .is_empty());
        let record = reopened
            .projection()
            .claimed_notification_barrier(bob)
            .expect("claimed staged attempt remains resumable");
        assert_eq!(record.attempt_id, attempt_id);
        assert_eq!(record.state, NotificationState::Staged);
        assert_eq!(record.binding.as_ref(), Some(&binding));
        assert_eq!(reopened.projection().last_sequence(), Some(6));

        drop(reopened);
        let replayed = MessageStore::open(&root, journal, workspace, "boot-3").unwrap();
        let record = replayed
            .projection()
            .claimed_notification_barrier(bob)
            .expect("second replay retains the exact attempt");
        assert_eq!(record.attempt_id, attempt_id);
        assert_eq!(record.binding.as_ref(), Some(&binding));
    }

    #[test]
    fn claimed_staged_clear_append_changes_state_and_barrier_together() {
        let scratch = StoreScratch::new("claimed-staged-clear-atomic");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-claimed-staged-clear").unwrap();
        let attempt_id = attempt(1);
        let binding = notification_binding(bob);
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Claimed staged clear", None),
                1,
            )
            .unwrap();
        for (state, ts, binding) in [
            (NotificationState::Queued, 2, None),
            (NotificationState::Gating, 3, None),
            (NotificationState::Writing, 4, Some(binding)),
            (NotificationState::Staged, 5, None),
        ] {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    state,
                    binding,
                    None,
                    ts,
                )
                .unwrap();
        }
        store.claim_at(bob, message_id.clone(), 6).unwrap();
        let before_bytes = std::fs::read(store.journal_path()).unwrap();
        let before_seq = store.projection().last_sequence();
        assert!(matches!(
            store.settle_claimed_staged_clear(message_id.clone(), bob, attempt(2)),
            Err(MessageStoreError::Mailbox(error))
                if matches!(*error, MailboxError::NotificationAttemptMismatch { .. })
        ));
        assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
        assert_eq!(
            store.projection().active_notification_barriers()[0].attempt_id,
            attempt_id,
            "a different attempt cannot retire this barrier"
        );
        store.inject_next_claimed_staged_clear_append_failure();

        assert!(matches!(
            store.settle_claimed_staged_clear(message_id.clone(), bob, attempt_id),
            Err(MessageStoreError::Ledger(LedgerError::Io { .. }))
        ));
        assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
        assert_eq!(store.projection().last_sequence(), before_seq);
        let staged = store.projection().notification(bob, &message_id).unwrap();
        assert_eq!(staged.state, NotificationState::Staged);
        assert_eq!(
            store.projection().active_notification_barriers(),
            vec![staged.clone()],
            "a failed append keeps the exact staged attempt as FIFO barrier owner"
        );

        let settled = store
            .settle_claimed_staged_clear(message_id.clone(), bob, attempt_id)
            .unwrap();
        assert_eq!(settled.state, NotificationState::WithdrawnAfterStaging);
        assert!(store.projection().active_notification_barriers().is_empty());
        assert_eq!(store.projection().last_sequence(), Some(7));
        let settled_bytes = std::fs::read(store.journal_path()).unwrap();
        let repeated = store
            .settle_claimed_staged_clear(message_id.clone(), bob, attempt_id)
            .unwrap();
        assert_eq!(repeated, settled);
        assert_eq!(store.projection().last_sequence(), Some(7));
        assert_eq!(
            std::fs::read(store.journal_path()).unwrap(),
            settled_bytes,
            "idempotent reconciliation does not append another settlement fact"
        );
        drop(store);

        let replayed = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert_eq!(
            replayed
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .state,
            NotificationState::WithdrawnAfterStaging
        );
        assert!(replayed
            .projection()
            .active_notification_barriers()
            .is_empty());
    }

    #[test]
    fn versioned_doorbell_formats_survive_attention_and_restart() {
        let scratch = StoreScratch::new("doorbell-format-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let cases = [
            (MessageId::new("m-compact").unwrap(), attempt(1), 1),
            (MessageId::new("m-attempt-message").unwrap(), attempt(2), 2),
            (MessageId::new("m-attempt-only").unwrap(), attempt(3), 3),
            (MessageId::new("m-future").unwrap(), attempt(4), 999),
        ];

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            for (index, (message_id, attempt_id, format)) in cases.iter().enumerate() {
                let base = 1 + index as u64 * 5;
                store
                    .accept_at(
                        message_id.clone(),
                        draft(admin, vec![bob], "Format", None),
                        base,
                    )
                    .unwrap();
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        *attempt_id,
                        NotificationState::Queued,
                        None,
                        None,
                        base + 1,
                    )
                    .unwrap();
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        *attempt_id,
                        NotificationState::Gating,
                        None,
                        None,
                        base + 2,
                    )
                    .unwrap();
                store
                    .append_notification_transition_with_transport_at(
                        message_id.clone(),
                        bob,
                        *attempt_id,
                        NotificationState::Writing,
                        Some(notification_binding(bob)),
                        Some(NotificationTransport::Doorbell),
                        Some(*format),
                        None,
                        base + 3,
                    )
                    .unwrap();
                let attention = store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        *attempt_id,
                        NotificationState::AttentionRequired,
                        None,
                        Some(NotificationAttentionCause::VerifyFailed),
                        base + 4,
                    )
                    .unwrap();
                assert_eq!(attention.doorbell_format, Some(*format));
            }
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        for (message_id, _, format) in cases {
            let record = reopened
                .projection()
                .notification(bob, &message_id)
                .unwrap();
            assert_eq!(record.state, NotificationState::AttentionRequired);
            assert_eq!(record.transport, NotificationTransport::Doorbell);
            assert_eq!(record.doorbell_format, Some(format));
        }
    }

    #[test]
    fn a_durable_barrier_retirement_survives_replay() {
        let scratch = StoreScratch::new("barrier-retirement");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-retired-recovery").unwrap();
        let attempt_id = attempt(1);

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Retire", None),
                    1,
                )
                .unwrap();
            alarm_because(
                &mut store,
                &message_id,
                bob,
                attempt_id,
                2,
                NotificationAttentionCause::VerifyFailed,
            );
            assert_eq!(store.projection().active_notification_barriers().len(), 1);

            store
                .retire_notification_barrier(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    NotificationBarrierRetirementCause::PaneGone,
                    None,
                )
                .unwrap();
            assert!(store.projection().active_notification_barriers().is_empty());
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened
            .projection()
            .active_notification_barriers()
            .is_empty());
        assert_eq!(
            reopened
                .projection()
                .notification(bob, &message_id)
                .unwrap()
                .state,
            NotificationState::AttentionRequired
        );
    }

    #[test]
    fn a_notified_attempt_remains_recoverable_until_explicit_retirement() {
        let scratch = StoreScratch::new("notified-barrier-restart");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-notified-restart").unwrap();
        let attempt_id = attempt(1);
        let binding = notification_binding(bob);

        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Notified", None),
                    1,
                )
                .unwrap();
            store
                .queue_notification(message_id.clone(), bob, attempt_id)
                .unwrap();
            for state in [NotificationState::Gating, NotificationState::Writing] {
                store
                    .advance_notification(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        state,
                        (state == NotificationState::Writing).then(|| binding.clone()),
                        None,
                    )
                    .unwrap();
            }
            for state in [
                NotificationState::Staged,
                NotificationState::Submitting,
                NotificationState::Submitted,
                NotificationState::Notified,
            ] {
                store
                    .advance_notification(message_id.clone(), bob, attempt_id, state, None, None)
                    .unwrap();
            }
            let active = store.projection().active_notification_barriers();
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].state, NotificationState::Notified);
        }

        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert!(reopened
            .recover_notifications_after_restart()
            .unwrap()
            .is_empty());
        let active = reopened.projection().active_notification_barriers();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].state, NotificationState::Notified);

        let mut recovery = crate::composer_recovery::RecoveryCoordinator::new([attempt_id]);
        let exact = recovery.active_for_recipient(&active, bob);
        assert_eq!(
            recovery.reconcile(&exact, Some(&binding), false, false),
            Some(crate::composer_recovery::RecoveryAction::Restore(
                attempt_id
            ))
        );
        assert!(matches!(
            recovery.reconcile(&exact, Some(&binding), true, false),
            Some(crate::composer_recovery::RecoveryAction::Retire {
                cause: NotificationBarrierRetirementCause::ComposerObservedClear,
                ..
            })
        ));

        reopened
            .retire_notification_barrier(
                message_id,
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::ComposerObservedClear,
                None,
            )
            .unwrap();
        assert!(reopened
            .projection()
            .active_notification_barriers()
            .is_empty());
    }

    #[test]
    fn a_later_bound_write_replaces_an_older_notified_barrier_for_the_same_recipient() {
        let scratch = StoreScratch::new("newer-write-bounds-barriers");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let older = MessageId::new("m-older-notified").unwrap();
        let newer = MessageId::new("m-newer-notified").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(older.clone(), draft(admin, vec![bob], "Older", None), 1)
            .unwrap();
        store
            .accept_at(newer.clone(), draft(admin, vec![bob], "Newer", None), 2)
            .unwrap();

        notify_with_binding(&mut store, &older, bob, attempt(1), 10);
        store
            .append_notification_transition_at(
                newer.clone(),
                bob,
                attempt(2),
                NotificationState::Queued,
                None,
                None,
                20,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                newer.clone(),
                bob,
                attempt(2),
                NotificationState::Gating,
                None,
                None,
                21,
            )
            .unwrap();
        assert_eq!(
            store.projection().active_notification_barriers()[0].attempt_id,
            attempt(1),
            "a pre-write attempt cannot retire the older barrier"
        );

        store
            .append_notification_transition_at(
                newer.clone(),
                bob,
                attempt(2),
                NotificationState::Writing,
                Some(notification_binding(bob)),
                None,
                22,
            )
            .unwrap();
        for (offset, state) in [
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
            NotificationState::Notified,
        ]
        .into_iter()
        .enumerate()
        {
            store
                .append_notification_transition_at(
                    newer.clone(),
                    bob,
                    attempt(2),
                    state,
                    None,
                    None,
                    23 + offset as u64,
                )
                .unwrap();
        }

        let active = store.projection().active_notification_barriers();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].message_id, newer);
        assert_eq!(active[0].attempt_id, attempt(2));
        assert_eq!(active[0].state, NotificationState::Notified);
    }

    #[test]
    fn bound_writes_for_different_recipients_keep_separate_barriers() {
        let scratch = StoreScratch::new("recipient-scoped-barriers");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let bob_message = MessageId::new("m-bob-notified").unwrap();
        let carol_message = MessageId::new("m-carol-notified").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(bob_message.clone(), draft(admin, vec![bob], "Bob", None), 1)
            .unwrap();
        store
            .accept_at(
                carol_message.clone(),
                draft(admin, vec![carol], "Carol", None),
                2,
            )
            .unwrap();

        notify_with_binding(&mut store, &bob_message, bob, attempt(1), 10);
        notify_with_binding(&mut store, &carol_message, carol, attempt(2), 20);

        let active = store.projection().active_notification_barriers();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].recipient, bob);
        assert_eq!(active[0].attempt_id, attempt(1));
        assert_eq!(active[1].recipient, carol);
        assert_eq!(active[1].attempt_id, attempt(2));
    }

    #[test]
    fn restart_recovers_only_the_newest_barrier_for_one_recipient() {
        let scratch = StoreScratch::new("restart-newest-recipient-barrier");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let older = MessageId::new("m-restart-older").unwrap();
        let newer = MessageId::new("m-restart-newer").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(older.clone(), draft(admin, vec![bob], "Older", None), 1)
                .unwrap();
            store
                .accept_at(newer.clone(), draft(admin, vec![bob], "Newer", None), 2)
                .unwrap();
            notify_with_binding(&mut store, &older, bob, attempt(1), 10);
            notify_with_binding(&mut store, &newer, bob, attempt(2), 20);
        }

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let active = reopened.projection().active_notification_barriers();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].message_id, newer);
        assert_eq!(active[0].attempt_id, attempt(2));
        assert_eq!(active[0].state, NotificationState::Notified);
    }

    #[test]
    fn an_attention_barrier_survives_until_a_later_bound_write() {
        let scratch = StoreScratch::new("attention-needs-later-write");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let alarmed = MessageId::new("m-alarmed-barrier").unwrap();
        let later = MessageId::new("m-later-write").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(alarmed.clone(), draft(admin, vec![bob], "Alarmed", None), 1)
            .unwrap();
        store
            .accept_at(later.clone(), draft(admin, vec![bob], "Later", None), 2)
            .unwrap();
        alarm_because(
            &mut store,
            &alarmed,
            bob,
            attempt(1),
            10,
            NotificationAttentionCause::VerifyFailed,
        );

        for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
            .into_iter()
            .enumerate()
        {
            store
                .append_notification_transition_at(
                    later.clone(),
                    bob,
                    attempt(2),
                    state,
                    None,
                    None,
                    20 + offset as u64,
                )
                .unwrap();
            let active = store.projection().active_notification_barriers();
            assert_eq!(active.len(), 1);
            assert_eq!(active[0].attempt_id, attempt(1));
            assert_eq!(active[0].state, NotificationState::AttentionRequired);
        }

        store
            .append_notification_transition_at(
                later,
                bob,
                attempt(2),
                NotificationState::Writing,
                Some(notification_binding(bob)),
                None,
                22,
            )
            .unwrap();
        let active = store.projection().active_notification_barriers();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].attempt_id, attempt(2));
        assert_eq!(active[0].state, NotificationState::Writing);
    }

    #[test]
    fn a_torn_retirement_tail_keeps_the_barrier_retryable_after_reopen() {
        use std::io::Write as _;

        let scratch = StoreScratch::new("barrier-retirement-retry");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-retirement-retry").unwrap();
        let attempt_id = attempt(1);

        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Retry", None),
                1,
            )
            .unwrap();
        alarm_because(
            &mut store,
            &message_id,
            bob,
            attempt_id,
            2,
            NotificationAttentionCause::VerifyFailed,
        );
        assert_eq!(store.projection().active_notification_barriers().len(), 1);
        drop(store);

        let path = root.path().join(journal);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"seq":999,"kind":"state"#).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        assert_eq!(
            reopened.projection().active_notification_barriers().len(),
            1,
            "reopen discarded the retry obligation"
        );
        reopened
            .retire_notification_barrier(
                message_id,
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::PaneGone,
                None,
            )
            .unwrap();
        assert!(reopened
            .projection()
            .active_notification_barriers()
            .is_empty());
    }

    #[test]
    fn a_leaderless_write_binding_arms_restart_recovery_through_replay() {
        let scratch = StoreScratch::new("legacy-recovery-binding");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-legacy-recovery").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Legacy", None),
                1,
            )
            .unwrap();
        store
            .queue_notification(message_id.clone(), bob, attempt(1))
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        let mut incomplete = notification_binding(bob);
        incomplete.leader = None;
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Writing,
                Some(incomplete),
                None,
            )
            .unwrap();
        assert_eq!(store.projection().active_notification_barriers().len(), 1);
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Staged,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Submitting,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Submitted,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification(
                message_id,
                bob,
                attempt(1),
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::AckTimeout),
            )
            .unwrap();
        drop(store);

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let active = reopened.projection().active_notification_barriers();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].attempt_id, attempt(1));
        assert_eq!(active[0].state, NotificationState::AttentionRequired);
        assert_eq!(active[0].binding.as_ref().unwrap().leader, None);
    }

    #[test]
    fn exact_recipient_claim_keeps_a_v2_ack_timeout_until_reconciled() {
        let scratch = StoreScratch::new("v2-ack-timeout-claim");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-v2-ack-timeout").unwrap();
        let attempt_id = attempt(1);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Late claim", None),
                1,
            )
            .unwrap();
        store
            .queue_notification(message_id.clone(), bob, attempt_id)
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification_with_transport(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                notification_binding(bob),
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_ATTEMPT_CLAIM),
            )
            .unwrap();
        for state in [
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
        ] {
            store
                .advance_notification(message_id.clone(), bob, attempt_id, state, None, None)
                .unwrap();
        }
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::AckTimeout),
            )
            .unwrap();

        let outcome = store.claim(bob, message_id.clone()).unwrap();
        assert!(matches!(
            outcome,
            ClaimOutcome::Claimed {
                claimed_ack_timeout_attempt: Some(found),
                ..
            } if found == attempt_id
        ));
        let record = store.projection().notification(bob, &message_id).unwrap();
        assert_eq!(record.state, NotificationState::AttentionRequired);
        assert_eq!(record.cause, Some(NotificationAttentionCause::AckTimeout));
        assert_eq!(
            store
                .projection()
                .open_alarms_for_message(&message_id)
                .len(),
            1
        );
        assert_eq!(
            store
                .projection()
                .claimed_notification_barrier(bob)
                .map(|record| record.attempt_id),
            Some(attempt_id)
        );
        let before_failed_append = store.projection().last_sequence();
        store.inject_next_claimed_ack_timeout_reconciliation_append_failure();
        assert!(store
            .settle_claimed_ack_timeout_reconciliation(message_id.clone(), bob, attempt_id)
            .is_err());
        assert_eq!(store.projection().last_sequence(), before_failed_append);
        assert_eq!(
            store
                .projection()
                .notification(bob, &message_id)
                .map(|record| (record.state, record.cause)),
            Some((
                NotificationState::AttentionRequired,
                Some(NotificationAttentionCause::AckTimeout),
            ))
        );
        assert_eq!(store.projection().active_notification_barriers().len(), 1);
        assert_eq!(
            store
                .projection()
                .open_alarms_for_message(&message_id)
                .len(),
            1
        );
        let error = store
            .retire_notification_barrier(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::LifecycleReconciled,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(error)
                if matches!(
                    *error,
                    MailboxError::NotificationBarrierRetirementState {
                        cause: NotificationBarrierRetirementCause::LifecycleReconciled,
                        state: NotificationState::AttentionRequired,
                    }
                )
        ));
        assert_eq!(store.projection().active_notification_barriers().len(), 1);
        assert_eq!(
            store
                .projection()
                .open_alarms_for_message(&message_id)
                .len(),
            1
        );

        store
            .settle_claimed_ack_timeout_reconciliation(message_id.clone(), bob, attempt_id)
            .unwrap();
        let record = store.projection().notification(bob, &message_id).unwrap();
        assert_eq!(record.state, NotificationState::Notified);
        assert_eq!(record.cause, None);
        assert!(store.projection().active_notification_barriers().is_empty());
        assert!(store
            .projection()
            .open_alarms_for_message(&message_id)
            .is_empty());
        let settled_seq = store.projection().last_sequence();
        let settled_lines = std::fs::read_to_string(store.journal_path())
            .unwrap()
            .lines()
            .count();
        store
            .settle_claimed_ack_timeout_reconciliation(message_id.clone(), bob, attempt_id)
            .unwrap();
        assert_eq!(store.projection().last_sequence(), settled_seq);
        assert_eq!(
            std::fs::read_to_string(store.journal_path())
                .unwrap()
                .lines()
                .count(),
            settled_lines
        );
        drop(store);

        let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        let record = reopened
            .projection()
            .notification(bob, &message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::Notified);
        assert_eq!(record.cause, None);
        assert!(reopened
            .projection()
            .open_alarms_for_message(&message_id)
            .is_empty());
    }

    #[test]
    fn attempt_locator_claim_accepts_only_the_current_authenticated_recipient() {
        let scratch = StoreScratch::new("attempt-claim");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-attempt-claim").unwrap();
        let old_attempt = attempt(1);
        let current_attempt = attempt(2);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Claim", None),
                1,
            )
            .unwrap();
        alarm_because(
            &mut store,
            &message_id,
            bob,
            old_attempt,
            2,
            NotificationAttentionCause::VerifyFailed,
        );
        store
            .requeue_notification(message_id.clone(), bob, old_attempt, current_attempt)
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                current_attempt,
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification_with_transport(
                message_id.clone(),
                bob,
                current_attempt,
                NotificationState::Writing,
                notification_binding(bob),
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            )
            .unwrap();

        assert!(matches!(
            store.claim_notification_locator(
                bob,
                cyclops_proto::notification_attempt_claim_locator(old_attempt),
                old_attempt,
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationAttemptUnknown(found)
                    if *found == old_attempt)
        ));
        assert!(matches!(
            store.claim_notification_locator(
                carol,
                cyclops_proto::notification_attempt_claim_locator(current_attempt),
                current_attempt,
            ),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationAttemptUnknown(found)
                    if *found == current_attempt)
        ));
        assert!(matches!(
            store
                .claim_notification_locator(
                    bob,
                    cyclops_proto::notification_attempt_claim_locator(current_attempt),
                    current_attempt,
                )
                .unwrap()
                .0,
            ClaimOutcome::Claimed { message, .. } if message.message_id == message_id
        ));
    }

    #[test]
    fn attempt_locator_distinguishes_legacy_messages_without_fallback_ambiguity() {
        let scratch = StoreScratch::new("attempt-locator-collision");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let never_issued = attempt(10);
        let legacy_locator = cyclops_proto::notification_attempt_claim_locator(never_issued);
        let current_attempt = attempt(11);
        let current_message = MessageId::new("m-current-attempt").unwrap();
        let colliding_locator = cyclops_proto::notification_attempt_claim_locator(current_attempt);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();

        store
            .accept_at(
                legacy_locator.clone(),
                draft(admin, vec![bob], "Imported legacy locator", None),
                1,
            )
            .unwrap();
        assert!(matches!(
            store
                .claim_notification_locator(bob, legacy_locator.clone(), never_issued)
                .unwrap()
                .0,
            ClaimOutcome::Claimed { message, .. } if message.message_id == legacy_locator
        ));

        store
            .accept_at(
                current_message.clone(),
                draft(admin, vec![bob], "Current attempt", None),
                2,
            )
            .unwrap();
        store
            .queue_notification(current_message.clone(), bob, current_attempt)
            .unwrap();
        store
            .advance_notification(
                current_message.clone(),
                bob,
                current_attempt,
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        store
            .advance_notification_with_transport(
                current_message.clone(),
                bob,
                current_attempt,
                NotificationState::Writing,
                notification_binding(bob),
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            )
            .unwrap();
        store
            .accept_at(
                colliding_locator.clone(),
                draft(admin, vec![bob], "Imported collision", None),
                3,
            )
            .unwrap();

        assert!(matches!(
            store.claim_notification_locator(bob, colliding_locator.clone(), current_attempt),
            Err(MessageStoreError::Mailbox(error))
                if matches!(
                    error.as_ref(),
                    MailboxError::NotificationAttemptClaimLocatorConflict(found)
                        if found == &colliding_locator
                )
        ));
        assert!(matches!(
            store.claim_notification_locator(carol, colliding_locator.clone(), current_attempt),
            Err(MessageStoreError::Mailbox(error))
                if matches!(
                    error.as_ref(),
                    MailboxError::NotificationAttemptUnknown(found)
                        if *found == current_attempt
                )
        ));
        assert!(store.projection().entry_is_pending(bob, &current_message));
        assert!(store.projection().entry_is_pending(bob, &colliding_locator));

        for state in [
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
        ] {
            store
                .advance_notification(
                    current_message.clone(),
                    bob,
                    current_attempt,
                    state,
                    None,
                    None,
                )
                .unwrap();
        }
        store
            .advance_notification(
                current_message.clone(),
                bob,
                current_attempt,
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::AckTimeout),
            )
            .unwrap();
        store
            .requeue_notification(current_message, bob, current_attempt, attempt(12))
            .unwrap();
        assert!(matches!(
            store.claim_notification_locator(bob, colliding_locator.clone(), current_attempt),
            Err(MessageStoreError::Mailbox(error))
                if matches!(
                    error.as_ref(),
                    MailboxError::NotificationAttemptUnknown(found)
                        if *found == current_attempt
                )
        ));
        assert!(store.projection().entry_is_pending(bob, &colliding_locator));
    }

    #[test]
    fn pane_width_edge_reopens_one_exact_prewrite_attempt() {
        let scratch = StoreScratch::new("pane-width-reopen");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, _, bob, _) = test_context();
        let directory = MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        let service = MailboxService::new(directory, store);
        let accepted = service
            .send(service.admin(), mailbox_send("reviewer", "Width", ""))
            .unwrap();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            accepted.message_id,
            bob,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        let narrow = NotificationPreWriteObservation {
            pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
            selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
            binding: Some(notification_binding(bob)),
            route_evidence: None,
            pane_width: Some(DOORBELL_V3_MIN_PANE_WIDTH - 1),
            required_pane_width: Some(DOORBELL_V3_MIN_PANE_WIDTH),
            write_block: None,
        };
        context
            .record_pre_write_block(
                NotificationPreWriteCause::WriteReadinessChanged,
                Some(narrow.clone()),
            )
            .unwrap();

        assert!(service.oldest_notification_has_width_block(bob).unwrap());
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, narrow.clone(), true)
            .unwrap()
            .is_none());
        let wide = NotificationPreWriteObservation {
            pane_width: Some(DOORBELL_V3_MIN_PANE_WIDTH),
            required_pane_width: None,
            write_block: None,
            ..narrow
        };
        let reopened = service
            .reopen_oldest_notification_after_route_evidence(bob, wide.clone(), true)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.attempt_id, queued.attempt_id);
        assert_eq!(reopened.state, NotificationState::Gating);
        assert_eq!(reopened.pre_write_reopen_count, 1);
        assert!(!service.oldest_notification_has_width_block(bob).unwrap());
        assert!(service
            .reopen_oldest_notification_after_route_evidence(bob, wide, true)
            .unwrap()
            .is_none());
    }

    #[test]
    fn clean_screen_retirement_is_rejected_for_an_ambiguous_attempt() {
        let scratch = StoreScratch::new("ambiguous-clean-retirement");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-ambiguous-clean").unwrap();
        let attempt_id = attempt(1);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Ambiguous", None),
                1,
            )
            .unwrap();
        alarm_because(
            &mut store,
            &message_id,
            bob,
            attempt_id,
            2,
            NotificationAttentionCause::VerifyFailed,
        );

        let error = store
            .retire_notification_barrier(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::ComposerObservedClear,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(error)
                if matches!(
                    *error,
                    MailboxError::NotificationBarrierRetirementState {
                        cause: NotificationBarrierRetirementCause::ComposerObservedClear,
                        state: NotificationState::AttentionRequired,
                    }
                )
        ));
        assert_eq!(store.projection().active_notification_barriers().len(), 1);

        store
            .retire_notification_barrier(
                message_id,
                bob,
                attempt_id,
                NotificationBarrierRetirementCause::LifecycleReconciled,
                None,
            )
            .unwrap();
        assert!(store.projection().active_notification_barriers().is_empty());
    }

    #[test]
    fn notification_attempt_ids_are_unique_across_broadcast_recipients() {
        let scratch = StoreScratch::new("notification-broadcast");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, carol) = test_context();
        let message_id = MessageId::new("m-broadcast").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob, carol], "Broadcast", None),
                1,
            )
            .unwrap();

        store
            .queue_notification(message_id.clone(), bob, attempt(1))
            .unwrap();
        let error = store
            .queue_notification(message_id.clone(), carol, attempt(1))
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationAttemptReused(id) if *id == attempt(1))
        ));
        assert_eq!(store.projection().last_sequence(), Some(2));

        store
            .queue_notification(message_id.clone(), carol, attempt(2))
            .unwrap();
        assert_eq!(store.projection().notifications_for(bob).len(), 1);
        assert_eq!(store.projection().notifications_for(carol).len(), 1);
        assert_eq!(
            store
                .projection()
                .notification(carol, &message_id)
                .unwrap()
                .attempt_id,
            attempt(2)
        );
    }

    #[test]
    fn doorbell_format_validation_is_failure_atomic() {
        let scratch = StoreScratch::new("doorbell-format-validation");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-format-validation").unwrap();
        let attempt_id = attempt(1);
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Format", None),
                1,
            )
            .unwrap();
        store
            .queue_notification(message_id.clone(), bob, attempt_id)
            .unwrap();
        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();

        let direct = store
            .advance_notification_with_transport(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                notification_binding(bob),
                NotificationTransport::DirectPayload,
                Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            )
            .unwrap_err();
        assert!(matches!(
            direct,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationDoorbellFormatForbidden)
        ));
        assert_eq!(store.projection().last_sequence(), Some(3));

        let unknown = store
            .advance_notification_with_transport(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                notification_binding(bob),
                NotificationTransport::Doorbell,
                Some(999),
            )
            .unwrap_err();
        assert!(matches!(
            unknown,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::UnsupportedNotificationDoorbellFormat(999))
        ));
        assert_eq!(store.projection().last_sequence(), Some(3));

        store
            .advance_notification_with_transport(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                notification_binding(bob),
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            )
            .unwrap();
        let non_writing = store
            .append_notification_transition_with_transport_at(
                message_id,
                bob,
                attempt_id,
                NotificationState::Staged,
                None,
                None,
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
                None,
                5,
            )
            .unwrap_err();
        assert!(matches!(
            non_writing,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationDoorbellFormatForbidden)
        ));
        assert_eq!(store.projection().last_sequence(), Some(4));
    }

    #[test]
    fn illegal_notification_transitions_and_requeues_are_failure_atomic() {
        let scratch = StoreScratch::new("notification-illegal");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-illegal").unwrap();
        let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Illegal", None),
                1,
            )
            .unwrap();

        let error = store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationNotFound { .. })
        ));
        assert_eq!(store.projection().last_sequence(), Some(1));

        store
            .queue_notification(message_id.clone(), bob, attempt(1))
            .unwrap();
        let error = store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::InvalidNotificationTransition {
                    from: NotificationState::Queued,
                    to: NotificationState::AttentionRequired,
                })
        ));
        assert_eq!(store.projection().last_sequence(), Some(2));

        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        let error = store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Writing,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationBindingRequired)
        ));
        assert_eq!(store.projection().last_sequence(), Some(3));

        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Writing,
                Some(notification_binding(bob)),
                None,
            )
            .unwrap();
        let error = store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::AckTimeout),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::InvalidNotificationCause {
                    cause: NotificationAttentionCause::AckTimeout,
                    state: NotificationState::Writing,
                })
        ));

        store
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::AttentionRequired,
                None,
                Some(NotificationAttentionCause::VerifyFailed),
            )
            .unwrap();
        assert_eq!(store.projection().last_sequence(), Some(5));

        let error = store
            .queue_notification(message_id.clone(), bob, attempt(2))
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationAttemptMismatch { .. })
        ));
        let error = store
            .requeue_notification(message_id.clone(), bob, attempt(1), attempt(1))
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationAttemptReused(id) if *id == attempt(1))
        ));

        store
            .requeue_notification(message_id.clone(), bob, attempt(1), attempt(2))
            .unwrap();
        let error = store
            .requeue_notification(message_id.clone(), bob, attempt(2), attempt(3))
            .unwrap_err();
        assert!(matches!(
            error,
            MessageStoreError::Mailbox(inner)
                if matches!(inner.as_ref(), MailboxError::NotificationRequeueRequiresAttention)
        ));
        assert_eq!(store.projection().last_sequence(), Some(6));

        let forged = LedgerLine {
            seq: 7,
            boot_id: "forged".into(),
            id: message_id.to_string(),
            ts: 7,
            kind: Kind::State,
            from: "human".into(),
            to: vec![bob.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(
                serde_json::to_value(NotificationFact::NotificationTransition {
                    record_version: CANONICAL_RECORD_VERSION,
                    attempt_id: attempt(2),
                    message_id: message_id.clone(),
                    recipient: bob,
                    state: NotificationState::Gating,
                    binding: None,
                    transport: None,
                    doorbell_format: None,
                    cause: None,
                    verify_outcome: None,
                    pre_write_cause: None,
                    wake_block: None,
                    pre_write_observation: None,
                })
                .unwrap(),
            ),
        };
        let error = store.projection.apply_line(&forged).unwrap_err();
        assert!(matches!(
            error,
            MailboxError::PresentationMismatch { field: "from", .. }
        ));
        assert_eq!(store.projection().last_sequence(), Some(6));
    }

    #[test]
    fn notification_replay_recovers_torn_tail_and_refuses_bad_facts() {
        let scratch = StoreScratch::new("notification-corruption");
        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let (workspace, admin, bob, _) = test_context();
        let message_id = MessageId::new("m-corrupt").unwrap();
        {
            let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Corruption", None),
                    1,
                )
                .unwrap();
            store
                .queue_notification(message_id.clone(), bob, attempt(1))
                .unwrap();
        }
        {
            let mut file = root.open_append(journal).unwrap();
            file.write_all(br#"{"seq":3,"boot_id":"boot-1","id":"m-corrupt""#)
                .unwrap();
            file.sync_data().unwrap();
        }
        {
            let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
            reopened
                .advance_notification(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    NotificationState::Gating,
                    None,
                    None,
                )
                .unwrap();
            assert_eq!(reopened.projection().last_sequence(), Some(3));
        }
        {
            let malformed = LedgerLine {
                seq: 4,
                boot_id: "boot-2".into(),
                id: message_id.to_string(),
                ts: 4,
                kind: Kind::State,
                from: "cyclopsd".into(),
                to: vec![bob.to_string()],
                subject: None,
                body: None,
                reply_to: None,
                deliveries: Vec::new(),
                data: Some(serde_json::json!({
                    "type": "notification_transition",
                    "record_version": CANONICAL_RECORD_VERSION,
                    "attempt_id": "att-bad!",
                    "message_id": message_id,
                    "recipient": bob,
                    "state": "writing",
                    "binding": null
                })),
            };
            let mut file = root.open_append(journal).unwrap();
            serde_json::to_writer(&mut file, &malformed).unwrap();
            file.write_all(b"\n").unwrap();
            file.sync_data().unwrap();
        }

        assert!(matches!(
            MessageStore::open(&root, journal, workspace, "boot-3"),
            Err(MessageStoreError::Mailbox(inner))
                if matches!(inner.as_ref(), MailboxError::InvalidNotificationFact(_))
        ));
    }
}
