//!
//! In-memory projection, drafts, mutations, and unread tracking.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cyclops_proto::{
    InboxMessage, Kind, LedgerLine, MailboxEntry, MailboxEntryState, MailboxFact, MailboxListItem,
    MailboxTypeError, MessageDirection, MessageId, MessageMetadata, MessageNotificationSettlement,
    MessageNotificationState, MessageNotificationSummary, MessagePresentation, MessageQuotaState,
    MessageRecipientRoute, MessageRecipientSummary, MessageSnapshotRow, MessageWakeBlock,
    MessagesFollowResult, MessagesSnapshotCounts, MessagesSnapshotResult, NotificationAttemptId,
    NotificationAttentionCause, NotificationBarrierRetirementCause, NotificationFact,
    NotificationPreWriteCause, NotificationRecord, NotificationRequeue, NotificationResolution,
    NotificationResolutionConsumptionObservation, NotificationRouteEvidenceId, NotificationState,
    NotificationTransport, RecipientKey, RequestContent, RequestDigest, StatusBlockedNotification,
    StatusNextAction, WorkspaceId, CANONICAL_RECORD_VERSION, DOORBELL_FORMAT_COMPACT_CLAIM,
    NOTIFICATION_RESOLUTION_PROOF_VERSION,
};

use super::*;

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
    /// The sender asked for a raw write.
    pub raw: bool,
}

/// One active composer barrier with the durable message and mailbox facts
/// needed to reconstruct its expected bytes. This value stays daemon-local.
#[derive(Debug, Clone)]
pub(crate) struct ActiveComposerNotification {
    pub(crate) record: NotificationRecord,
    pub(crate) message: Option<LedgerLine>,
    pub(crate) entry_state: Option<MailboxEntryState>,
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
    /// The sender asked for a raw write.
    pub raw: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanonicalDraft {
    pub(crate) kind: Kind,
    pub(crate) sender: RecipientKey,
    pub(crate) recipients: Vec<RecipientKey>,
    pub(crate) subject: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) body: Option<String>,
    pub(crate) reply_to: Option<MessageId>,
    pub(crate) client_key: Option<String>,
    pub(crate) supersedes: Option<MessageId>,
    pub(crate) presentation: MessagePresentation,
    pub(crate) raw: bool,
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
    },
    /// Entry was already claimed by this claimant; returns existing entry and canonical message line.
    AlreadyClaimed {
        entry: MailboxEntry,
        message: InboxMessage,
        withdrawn_attempt: Option<NotificationAttemptId>,
        consumed_doorbell_attempt: Option<NotificationAttemptId>,
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
    #[error("notification verified_by is only allowed on the Notified transition")]
    NotificationVerifiedByForbidden,
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
    pub(crate) workspace_id: WorkspaceId,
    /// Last applied monotonic sequence number across the single workspace journal.
    pub(crate) last_workspace_seq: Option<u64>,
    /// Canonical messages indexed by unique ID.
    pub(crate) messages: HashMap<MessageId, LedgerLine>,
    /// Idempotency index: (sender, client_key) -> (message_id, request_digest).
    pub(crate) idempotency_index: HashMap<(RecipientKey, String), (MessageId, RequestDigest)>,
    /// Ordered mailbox entries per recipient (strictly ordered by monotonic workspace sequence).
    pub(crate) mailboxes: HashMap<RecipientKey, BTreeMap<u64, MailboxEntry>>,
    /// Mailbox sequence for each durable recipient and message.
    ///
    /// Entries remain owned by `mailboxes`; this index avoids scanning a
    /// recipient's full FIFO for point reads and state transitions.
    pub(crate) mailbox_index: HashMap<(RecipientKey, MessageId), u64>,
    /// Current notification attempt per durable recipient and message.
    pub(crate) notifications: HashMap<(RecipientKey, MessageId), NotificationRecord>,
    /// Post-write attempts whose composer barrier has not been retired.
    ///
    /// This is a projection of journal facts, not a second durable store.
    /// Requeue can replace the current attempt while an older attempt still
    /// owns staged composer state, so the key is the exact attempt id.
    pub(crate) active_notification_barriers: HashMap<NotificationAttemptId, NotificationRecord>,
    /// ACK-timeout claim reconciliations applied from their dedicated fact.
    pub(crate) claimed_ack_timeout_reconciliations: HashSet<NotificationAttemptId>,
    /// Every attempt identifier seen in this workspace, including superseded attempts.
    pub(crate) notification_attempts: HashSet<NotificationAttemptId>,
    /// Attempts an operator has acknowledged. Kept beside the records
    /// rather than inside them so a clearance never rewrites the attempt
    /// it acknowledges.
    pub(crate) cleared_attempts: HashSet<NotificationAttemptId>,
    /// Atomic clearance command identifiers already applied.
    pub(crate) clearance_batches: HashSet<String>,
    /// Operator resolutions keyed by exact attempt identity.
    pub(crate) resolved_attempts: HashMap<NotificationAttemptId, NotificationResolution>,
    /// Terminal action intents recorded before a terminal write.
    pub(crate) resolution_intents: HashMap<NotificationAttemptId, NotificationResolution>,
    /// Forced intents selected by the default-off submit fallback.
    ///
    /// This is retained only while the matching intent is open so a replayed
    /// final key reservation can prove it belongs to that narrowly-scoped
    /// fallback rather than to an ordinary operator action.
    pub(crate) forced_resolution_intents: HashSet<NotificationAttemptId>,
    /// Final forced terminal-key reservations ordered with mailbox claims.
    pub(crate) resolution_action_reservations:
        HashMap<NotificationAttemptId, NotificationResolution>,
    /// Workspace sequence of each forced terminal-key reservation.
    ///
    /// A recipient claim after this sequence can provide Complete consumption
    /// evidence once terminal acceptance is also durable, even if the claim
    /// arrived in the unavoidable interval before the actual terminal IO.
    pub(crate) resolution_action_reservation_sequences: HashMap<NotificationAttemptId, u64>,
    /// Terminal action keys accepted by the terminal for an exact intent.
    pub(crate) resolution_actions_accepted: HashMap<NotificationAttemptId, NotificationResolution>,
    /// Workspace sequence of each terminal action-accepted boundary.
    pub(crate) resolution_action_sequences: HashMap<NotificationAttemptId, u64>,
    /// Workspace sequence of each exact recipient claim.
    pub(crate) claim_sequences: HashMap<(RecipientKey, MessageId), u64>,
    /// Workspace sequence where each attempt crossed its terminal write boundary.
    pub(crate) notification_write_sequences: HashMap<NotificationAttemptId, u64>,
    /// Complete actions with exact, causally correlated consumption evidence.
    pub(crate) resolution_consumptions:
        HashMap<NotificationAttemptId, NotificationResolutionConsumptionObservation>,
}

pub(crate) enum PreparedMutation {
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

pub(crate) struct NotificationProjectionUpdate {
    key: (RecipientKey, MessageId),
    record: NotificationRecord,
}

pub(crate) fn uses_legacy_notification_write_contract(record: &NotificationRecord) -> bool {
    let legacy_transport = match record.transport {
        NotificationTransport::Doorbell => matches!(
            record.doorbell_format,
            None | Some(DOORBELL_FORMAT_COMPACT_CLAIM)
        ),
        NotificationTransport::DirectPayload => record.doorbell_format.is_none(),
        NotificationTransport::Raw => false,
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
pub(crate) fn notification_transition_allowed(
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

pub(crate) fn notification_pre_write_width_block(
    record: &NotificationRecord,
) -> Option<(u32, u32)> {
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
pub(crate) fn notification_wake_block(
    record: &NotificationRecord,
    attention_resolution_pending: bool,
) -> Option<MessageWakeBlock> {
    record.wake_block.or_else(|| {
        attention_resolution_pending.then_some(MessageWakeBlock::AttentionResolutionPending)
    })
}

pub(crate) fn route_evidence_is_later(
    prior: &NotificationRouteEvidenceId,
    current: &NotificationRouteEvidenceId,
) -> bool {
    prior.boot_id != current.boot_id || current.generation > prior.generation
}

pub(crate) struct ReplyDerivation {
    pub(crate) recipient: RecipientKey,
    pub(crate) thread_root: MessageId,
    pub(crate) subject: Option<String>,
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
    pub(crate) fn check_acceptance(
        &self,
        draft: &CanonicalDraft,
    ) -> Result<AcceptanceOutcome, MailboxError> {
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

    pub(crate) fn apply_owned(&mut self, line: LedgerLine) -> Result<(), MailboxError> {
        let prepared = self.prepare_line(&line)?;
        self.commit_line(line, prepared);
        Ok(())
    }

    pub(crate) fn apply_replayed_owned(&mut self, line: LedgerLine) -> Result<(), MailboxError> {
        let prepared = self.prepare_line_inner(&line, true)?;
        self.commit_line(line, prepared);
        Ok(())
    }

    pub(crate) fn prepare_line(&self, line: &LedgerLine) -> Result<PreparedMutation, MailboxError> {
        self.prepare_line_inner(line, false)
    }

    pub(crate) fn prepare_line_inner(
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

    pub(crate) fn prepare_state(
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

    pub(crate) fn prepare_message(
        &self,
        line: &LedgerLine,
    ) -> Result<PreparedMutation, MailboxError> {
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
                        verified_by: None,
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

    pub(crate) fn prepare_claim(
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
                        verified_by: None,
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

    pub(crate) fn prepare_delivered_direct(
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

    pub(crate) fn prepare_notification_transition(
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
            verified_by,
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

        // A raw write proves nothing about the occupant, so it is the one
        // Writing transition that carries no binding.
        let raw = transport == Some(NotificationTransport::Raw);
        if state == NotificationState::Writing && !raw {
            let Some(binding) = binding.as_ref() else {
                return Err(MailboxError::NotificationBindingRequired);
            };
            if binding.recipient != recipient {
                return Err(MailboxError::NotificationBindingMismatch);
            }
        } else if binding.is_some() {
            return Err(MailboxError::NotificationBindingForbidden);
        }
        if verified_by.is_some() && state != NotificationState::Notified {
            return Err(MailboxError::NotificationVerifiedByForbidden);
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
                        verified_by: None,
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
                    verified_by,
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

    pub(crate) fn prepare_unclaimed_reminder_queued(
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
        record.verified_by = None;
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

    pub(crate) fn prepare_notification_requeue(
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

    pub(crate) fn prepare_notification_requeues(
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

    pub(crate) fn prepare_notification_requeue_record(
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
                verified_by: None,
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

    pub(crate) fn prepare_notification_clear(
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

    pub(crate) fn prepare_notification_clears(
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

    pub(crate) fn prepare_notification_withdrawal(
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
                verified_by: None,
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

    pub(crate) fn prepare_notification_resolution(
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

    pub(crate) fn prepare_notification_resolution_without_terminal_action(
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

    pub(crate) fn prepare_notification_resolution_intent(
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

    pub(crate) fn prepare_notification_resolution_action_reserved(
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

    pub(crate) fn validate_forced_notification_resolution_action_reservation(
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

    pub(crate) fn prepare_notification_resolution_action_accepted(
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

    pub(crate) fn validate_notification_resolution_action_accepted(
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

    pub(crate) fn prepare_notification_resolution_consumption_observed(
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

    pub(crate) fn validate_notification_resolution_consumption_observed(
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

    pub(crate) fn prepare_notification_claimed_staged_clear(
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

    pub(crate) fn prepare_notification_claimed_ack_timeout_reconciliation(
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

    pub(crate) fn prepare_notification_barrier_retirement(
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

    pub(crate) fn prepare_notification_resolution_intent_withdrawn(
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

    pub(crate) fn validate_notification_envelope(
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

    pub(crate) fn validate_notification_requeues_envelope(
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

    pub(crate) fn require_pending_entry(
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

    pub(crate) fn commit_line(&mut self, line: LedgerLine, prepared: PreparedMutation) {
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

    /// Oldest pre-submit operator notification whose mailbox payload was
    /// already claimed through the socket. Retrieval does not relinquish this
    /// notification's FIFO position.
    pub(crate) fn claimed_operator_notification(
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

    pub(crate) fn get_entry_mut(
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
    pub(crate) fn notification_withdrawn_by_operator(
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

    pub(crate) fn notification_settlement(
        &self,
        record: &NotificationRecord,
    ) -> Option<MessageNotificationSettlement> {
        MessageNotificationSettlement::from_notification(record.state)
    }

    pub(crate) fn notification_summary(
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
                verified_by: None,
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
            verified_by: record.verified_by,
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

    pub(crate) fn requeueable_for_message(
        &self,
        message_id: &MessageId,
    ) -> Vec<&NotificationRecord> {
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
    pub(crate) fn blocked_notification_snapshot(
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

    pub(crate) fn derive_reply(
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

    pub(crate) fn supersession_thread_root(
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

    pub(crate) fn supersession_notification_is_pre_write(
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

pub(crate) fn extract_message_metadata(line: &LedgerLine) -> Result<MessageMetadata, MailboxError> {
    line.data
        .as_ref()
        .and_then(|d| serde_json::from_value::<MessageMetadata>(d.clone()).ok())
        .ok_or_else(|| MailboxError::MissingMetadata(line.id.clone()))
}

pub(crate) fn inbox_message(
    line: &LedgerLine,
    claimant: RecipientKey,
) -> Result<InboxMessage, MailboxError> {
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

pub(crate) fn projection_allows_message_body(
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

pub(crate) fn mint_message_id() -> MessageId {
    MessageId::new(format!("m-{}", uuid::Uuid::new_v4().simple())).expect("UUID message id")
}
