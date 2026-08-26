//! Durable one-shot notification records and transitions.
//!
//! Notification records contain no message body or captured terminal text.
//! They describe one attempt to wake one durable recipient for one message.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::{MessageId, ProcessInstanceId, RecipientKey};

/// Errors returned by validated notification identifiers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotificationTypeError {
    #[error("notification attempt id must use canonical att-<uuid> form")]
    InvalidAttemptId,
    #[error("notification manifest id must be a non-empty printable token")]
    InvalidManifestId,
}

/// Globally unique identifier for one notification attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationAttemptId(Uuid);

impl NotificationAttemptId {
    /// Generate a canonical version-4 attempt identifier.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse the exact lower-case, hyphenated wire representation.
    pub fn parse(value: &str) -> Result<Self, NotificationTypeError> {
        let suffix = value
            .strip_prefix("att-")
            .ok_or(NotificationTypeError::InvalidAttemptId)?;
        let uuid = Uuid::parse_str(suffix).map_err(|_| NotificationTypeError::InvalidAttemptId)?;
        if uuid.is_nil() || uuid.hyphenated().to_string() != suffix {
            return Err(NotificationTypeError::InvalidAttemptId);
        }
        Ok(Self(uuid))
    }
}

impl FromStr for NotificationAttemptId {
    type Err = NotificationTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for NotificationAttemptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "att-{}", self.0.hyphenated())
    }
}

impl Serialize for NotificationAttemptId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for NotificationAttemptId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Manifest identity captured at the write boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationManifestId(String);

impl NotificationManifestId {
    pub fn new(value: impl Into<String>) -> Result<Self, NotificationTypeError> {
        let value = value.into();
        if value.is_empty() || !value.chars().all(|character| character.is_ascii_graphic()) {
            return Err(NotificationTypeError::InvalidManifestId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for NotificationManifestId {
    type Err = NotificationTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Display for NotificationManifestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for NotificationManifestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NotificationManifestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// State of one recipient's one-shot wake notification.
///
/// `Writing` is the durable boundary before the external composer write.
/// Recovery may resume `Queued` or `Gating`. A claimed `Staged` doorbell may
/// resume only to re-prove and clear that exact attempt. A claimed `Submitted`
/// doorbell may settle as `Notified`. Other unresolved states from `Writing`
/// onward require attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationState {
    Queued,
    Gating,
    /// Repeated pre-write evidence could not prove a safe terminal binding.
    /// No pane bytes were written. A relevant route change or explicit
    /// operator action may move this exact attempt again.
    BlockedPreWrite,
    /// Quota was positively observed before any composer write.
    QuotaHeld,
    /// A later positive screen observation no longer showed quota.
    /// Only an explicit administrator requeue may leave this state.
    QuotaResetObserved,
    Writing,
    Staged,
    /// Terminal submit intent was durably reserved while the mailbox entry
    /// was still pending. No submit key is proven until `Submitted` follows.
    Submitting,
    Submitted,
    Notified,
    AttentionRequired,
    /// An authenticated claim made this pre-write wake unnecessary.
    Withdrawn,
    /// An authenticated claim won before submit. Cyclops either re-proved and
    /// cleared its exact staged bytes, or recovered after a crash with the same
    /// binding and positive visible-empty composer proof. The earlier `Staged`
    /// fact is preserved.
    /// Only `NotificationClaimedStagedCleared` may create this state because
    /// the same fact must retire the exact composer barrier.
    WithdrawnAfterStaging,
    /// An administrator suppressed this exact pre-write wake attempt.
    /// The mailbox item remains pending and claimable.
    WithdrawnByOperator,
    /// The message was replaced before this attempt crossed the write boundary.
    Superseded,
}

impl NotificationState {
    /// Legal transitions for one attempt. Requeue is a separate fact.
    pub fn can_transition_to(self, next: NotificationState) -> bool {
        use NotificationState::*;
        match self {
            Queued => matches!(next, Gating | WithdrawnByOperator),
            Gating => matches!(
                next,
                BlockedPreWrite | Writing | QuotaHeld | WithdrawnByOperator
            ),
            BlockedPreWrite => matches!(next, Gating | Withdrawn | WithdrawnByOperator),
            QuotaHeld => next == QuotaResetObserved,
            QuotaResetObserved => false,
            Writing => matches!(next, Staged | AttentionRequired),
            Staged => matches!(next, Submitting | AttentionRequired),
            Submitting => matches!(next, Submitted | AttentionRequired),
            Submitted => matches!(next, Notified | AttentionRequired),
            Notified
            | AttentionRequired
            | Withdrawn
            | WithdrawnAfterStaging
            | WithdrawnByOperator
            | Superseded => false,
        }
    }

    /// Whether an administrator can prove that this attempt has not written
    /// terminal bytes and may withdraw it without touching the message.
    pub fn can_withdraw_before_write(self) -> bool {
        matches!(self, Self::Queued | Self::Gating | Self::BlockedPreWrite)
    }

    /// Settle notification work after the exact recipient claims the message.
    ///
    /// A pre-write wake is unnecessary after retrieval. A proven submitted
    /// doorbell is consumed, but this does not prove task completion. Staged,
    /// submitting, direct payload, and unresolved attention states remain
    /// unchanged.
    pub fn settled_by_claim(self, transport: NotificationTransport) -> Self {
        use NotificationState::*;

        match (self, transport) {
            (Queued | Gating | BlockedPreWrite | QuotaHeld | QuotaResetObserved, _) => Withdrawn,
            (Submitted, NotificationTransport::Doorbell) => Notified,
            _ => self,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::QuotaHeld
                | Self::QuotaResetObserved
                | Self::Notified
                | Self::AttentionRequired
                | Self::Withdrawn
                | Self::WithdrawnAfterStaging
                | Self::WithdrawnByOperator
                | Self::Superseded
        )
    }
}

/// Closed reasons a wake stopped before any terminal write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPreWriteCause {
    /// The session or pane route was unavailable before the terminal write.
    SessionUnavailable,
    /// No manifest could be selected for the live pane before the write.
    ManifestUnavailable,
    /// The durable message payload could not be rebuilt before the write.
    PayloadUnavailable,
    /// The pane or admitted process changed after readiness was proven.
    WriteReadinessChanged,
    /// The terminal paste buffer could not be prepared before the write.
    SpoolFailed,
    /// The manifest selected at the gate could not be proven against the
    /// live process ancestry immediately before the write.
    BindingUnprovable,
    /// The matched screen rule does not classify its composer ownership.
    /// No terminal write can be authorized until the manifest is repaired.
    ComposerSemanticMissing,
    /// The delivery worker exited twice before any terminal write.
    WorkerFailed,
}

impl NotificationPreWriteCause {
    /// Stable protocol spelling used by terminal and JSON clients.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::SessionUnavailable => "session_unavailable",
            Self::ManifestUnavailable => "manifest_unavailable",
            Self::PayloadUnavailable => "payload_unavailable",
            Self::WriteReadinessChanged => "write_readiness_changed",
            Self::SpoolFailed => "spool_failed",
            Self::BindingUnprovable => "binding_unprovable",
            Self::ComposerSemanticMissing => "composer_semantic_missing",
            Self::WorkerFailed => "worker_failed",
        }
    }

    /// Human-readable reason without changing the protocol vocabulary.
    pub const fn label(self) -> &'static str {
        match self {
            Self::SessionUnavailable => "session unavailable",
            Self::ManifestUnavailable => "manifest unavailable",
            Self::PayloadUnavailable => "payload unavailable",
            Self::WriteReadinessChanged => "write readiness changed",
            Self::SpoolFailed => "paste buffer preparation failed",
            Self::BindingUnprovable => "binding unprovable",
            Self::ComposerSemanticMissing => "composer ownership rule missing",
            Self::WorkerFailed => "worker failed",
        }
    }
}

/// Content-free process evidence captured when a pre-write attempt stops.
///
/// A later scheduler compares this stamp with a fresh observation. The same
/// stamp cannot reopen the attempt and produce another identical retry chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationPreWriteObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_root: Option<ProcessInstanceId>,
    /// Manifest selected by the gate for this attempt.
    ///
    /// This remains present when process ancestry proves a different
    /// manifest. Correcting a pin therefore changes the durable observation
    /// even when the pane process generation does not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_manifest: Option<NotificationManifestId>,
    /// The complete binding observed at the write boundary.
    ///
    /// None is a failed proof. A binding is kept whole so the journal cannot
    /// represent a partially proven leader, agent, or manifest as authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<NotificationBinding>,
    /// Pane width observed without storing any terminal content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_width: Option<u32>,
    /// Minimum pane width required by the selected notification format.
    ///
    /// Stored beside the observed width so replay does not reinterpret an
    /// older block after a future format changes its threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_pane_width: Option<u32>,
}

/// Closed causes for an ambiguous outcome after the write boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAttentionCause {
    PasteFailed,
    VerifyFailed,
    PaneReboundAfterPaste,
    SubmitFailed,
    ReceiptOccupantChanged,
    AckTimeout,
    DaemonRestart,
    TransportOutcomeUnknown,
}

/// Operator decision for one staged notification attempt.
///
/// The journal stores only this closed decision and the attempt identity.
/// Composer text and diagnostic diffs never enter durable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationResolution {
    Complete,
    Discard,
}

/// Closed evidence that a Complete action consumed the staged composer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationResolutionConsumptionEvidence {
    /// An authenticated hook carried an exact-attempt payload whose
    /// lossless token parsed to this exact attempt under the same binding.
    ExactHookPrompt,
    /// The exact durable recipient claimed this message after the terminal
    /// action-accepted fact.
    AuthenticatedClaim,
    /// Legacy evidence from an uncorrelated runtime transition.
    ///
    /// This remains readable for journal compatibility but does not authorize
    /// Complete settlement.
    WorkingEdge,
}

impl NotificationResolutionConsumptionEvidence {
    /// Does this observation identify the exact attempt payload or claim?
    pub fn proves_exact_consumption(self) -> bool {
        matches!(self, Self::ExactHookPrompt | Self::AuthenticatedClaim)
    }
}

/// Durable, content-free consumption evidence for one Complete action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationResolutionConsumptionObservation {
    pub evidence: NotificationResolutionConsumptionEvidence,
    pub observed_at_ms: u64,
}

/// Durable reason an attempt no longer owns staged composer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationBarrierRetirementCause {
    /// A post-restart turn ended with the exact key carried by the recovered
    /// hold, and the same composer then read clean.
    LifecycleReconciled,
    /// A settled attempt and the same bound composer read clean.
    ComposerObservedClear,
    /// The exact recipient claimed an incomplete legacy attempt after its
    /// write boundary, and its current manifest then proved a clean composer.
    /// No terminal key or delivery-completion claim is implied.
    RecipientClaimedComposerClear,
    /// A different agent generation or manifest owns the physical pane.
    OccupantReplaced,
    /// A server-wide pane observation proved the physical pane was gone.
    PaneGone,
}

impl NotificationAttentionCause {
    /// Reject causes that could not occur from the recorded prior state.
    pub fn valid_after(self, state: NotificationState) -> bool {
        use NotificationAttentionCause::*;
        use NotificationState::*;

        match state {
            Writing => matches!(
                self,
                PasteFailed | VerifyFailed | DaemonRestart | TransportOutcomeUnknown
            ),
            Staged => matches!(
                self,
                VerifyFailed
                    | PaneReboundAfterPaste
                    | SubmitFailed
                    | DaemonRestart
                    | TransportOutcomeUnknown
            ),
            Submitting => matches!(
                self,
                VerifyFailed
                    | PaneReboundAfterPaste
                    | SubmitFailed
                    | DaemonRestart
                    | TransportOutcomeUnknown
            ),
            Submitted => matches!(
                self,
                ReceiptOccupantChanged | AckTimeout | DaemonRestart | TransportOutcomeUnknown
            ),
            Queued
            | Gating
            | BlockedPreWrite
            | QuotaHeld
            | QuotaResetObserved
            | Notified
            | AttentionRequired
            | Withdrawn
            | WithdrawnAfterStaging
            | WithdrawnByOperator
            | Superseded => false,
        }
    }
}

/// Payload transport fixed at the durable terminal write boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationTransport {
    /// Content-free wake notification that directs an agent to its mailbox.
    #[default]
    Doorbell,
    /// Full message payload delivered through the verified terminal pipeline.
    DirectPayload,
}

/// Compact claim-command doorbell written by older versioned daemons.
pub const DOORBELL_FORMAT_COMPACT_CLAIM: u32 = 1;
/// Claim-command doorbell carrying an injective token for the exact attempt.
pub const DOORBELL_FORMAT_ATTEMPT_CLAIM: u32 = 2;
/// Single-row claim command carrying only the exact attempt token.
pub const DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM: u32 = 3;
/// Minimum pane width that can carry doorbell format 3 as one exact row.
pub const DOORBELL_V3_MIN_PANE_WIDTH: u32 = 60;
/// Message-shaped namespace reserved for exact notification-attempt claims.
pub const NOTIFICATION_ATTEMPT_CLAIM_LOCATOR_PREFIX: &str = "m-att_";
/// Current proof contract for a terminal-action resolution fact.
///
/// Missing values identify legacy facts written before action acceptance and
/// exact consumption became separate durable boundaries.
pub const NOTIFICATION_RESOLUTION_PROOF_VERSION: u32 = 1;

/// Content-free identity observed immediately before writing to the pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationBinding {
    pub recipient: RecipientKey,
    /// Process generation at the root of the tmux pane.
    ///
    /// Older journal rows omit this field. They replay but cannot authorize a
    /// later terminal action because pane identity is incomplete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_root: Option<ProcessInstanceId>,
    /// Foreground process that owned the terminal at the write boundary.
    ///
    /// Older journal rows omit this field. They still replay, but cannot
    /// authorize a later terminal action because the full binding is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<ProcessInstanceId>,
    pub agent: ProcessInstanceId,
    pub manifest: NotificationManifestId,
}

/// Current projection for one message and recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationRecord {
    pub attempt_id: NotificationAttemptId,
    pub message_id: MessageId,
    pub recipient: RecipientKey,
    pub state: NotificationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<NotificationBinding>,
    /// Payload shape fixed when this attempt crossed the terminal write boundary.
    ///
    /// Old projected records predate transport metadata and used a doorbell.
    #[serde(default)]
    pub transport: NotificationTransport,
    /// Exact doorbell byte format fixed at the write boundary.
    ///
    /// Missing values identify the legacy verbose doorbell. Numeric values
    /// remain forward compatible so older binaries can replay newer facts and
    /// refuse recovery when they cannot reconstruct the exact bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doorbell_format: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<NotificationAttentionCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_write_cause: Option<NotificationPreWriteCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_write_observation: Option<NotificationPreWriteObservation>,
    /// Automatic evidence-driven reopens already used by this attempt.
    ///
    /// One attempt may reopen once. Further recovery is explicit operator
    /// work, which prevents proof flapping from becoming a retry loop.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub pre_write_reopen_count: u8,
    pub started_seq: u64,
    pub updated_seq: u64,
    pub updated_at: u64,
}

impl NotificationRecord {
    /// Whether this exact failed wake may enter automatic composer recovery.
    ///
    /// The terminal still has to prove the complete binding, exact rendered
    /// doorbell, and action-safe composer before it may send a key. This
    /// predicate only selects the durable attempt class that supports that
    /// proof.
    pub fn needs_exact_owned_reconciliation(&self) -> bool {
        self.state == NotificationState::AttentionRequired
            && self.cause == Some(NotificationAttentionCause::VerifyFailed)
            && self.transport == NotificationTransport::Doorbell
            && doorbell_format_names_exact_attempt(self.doorbell_format)
            && self.binding.as_ref().is_some_and(|binding| {
                binding.recipient == self.recipient
                    && binding.pane_root.is_some()
                    && binding.leader.is_some()
            })
    }

    /// Whether a late claim may enter exact ACK-timeout reconciliation.
    ///
    /// This predicate does not settle the notification. The daemon must still
    /// prove that the exact staged doorbell was cleared, or that the same bound
    /// composer is visibly clean, before appending the settlement fact.
    pub fn needs_claimed_ack_timeout_reconciliation(&self) -> bool {
        self.state == NotificationState::AttentionRequired
            && self.cause == Some(NotificationAttentionCause::AckTimeout)
            && self.transport == NotificationTransport::Doorbell
            && doorbell_format_names_exact_attempt(self.doorbell_format)
            && self.binding.as_ref().is_some_and(|binding| {
                binding.recipient == self.recipient
                    && binding.pane_root.is_some()
                    && binding.leader.is_some()
            })
    }
}

/// One recipient moved from an attention attempt to a fresh queued attempt.
///
/// The enclosing fact owns the message identity and record version so one
/// broadcast requeue remains one compact journal row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationRequeue {
    pub prior_attempt_id: NotificationAttemptId,
    pub attempt_id: NotificationAttemptId,
    pub recipient: RecipientKey,
}

/// Notification state facts stored in the workspace journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationFact {
    NotificationTransition {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        state: NotificationState,
        #[serde(skip_serializing_if = "Option::is_none")]
        binding: Option<NotificationBinding>,
        /// Present only on the Writing transition. Missing legacy values mean Doorbell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transport: Option<NotificationTransport>,
        /// Present only for a versioned Doorbell Writing transition.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        doorbell_format: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cause: Option<NotificationAttentionCause>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_write_cause: Option<NotificationPreWriteCause>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_write_observation: Option<NotificationPreWriteObservation>,
    },
    NotificationRequeued {
        record_version: u32,
        prior_attempt_id: NotificationAttemptId,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
    },
    /// One operator command requeued several recipients atomically.
    ///
    /// The entries contain identities only. Replaying this one fact moves
    /// every named recipient or none of them.
    NotificationsRequeued {
        record_version: u32,
        message_id: MessageId,
        requeues: Vec<NotificationRequeue>,
    },
    /// An operator acknowledged one attention-required attempt.
    ///
    /// Names the exact attempt so a clearance cannot land on the attempt
    /// that replaced it. Carries no message content.
    NotificationCleared {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
    },
    /// One operator command acknowledged several exact attempts atomically.
    ///
    /// Attempt identifiers are sorted and unique. The optional cutoff binds
    /// an age-selected command to the snapshot the operator confirmed.
    NotificationsCleared {
        record_version: u32,
        batch_id: String,
        attempt_ids: Vec<NotificationAttemptId>,
        operator: RecipientKey,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cutoff_ms: Option<u64>,
    },
    /// An administrator withdrew one exact wake that was proven pre-write.
    ///
    /// The mailbox entry remains pending and claimable. Only the notification
    /// attempt is retired, so the recipient's next FIFO entry may be notified.
    NotificationWithdrawnBeforeWrite {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        operator: RecipientKey,
    },
    /// Durable intent recorded before a terminal resolution action.
    ///
    /// Intent alone does not prove that a terminal key was accepted. A later
    /// request must not send a second key or reconcile from composer state
    /// until a matching action-accepted fact exists. A request for the other
    /// resolution refuses.
    NotificationResolutionIntent {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
    },
    /// Durable proof that the terminal action key was accepted by the terminal.
    ///
    /// This does not prove that the composer consumed the action. Recovery
    /// must still recheck the exact attempt and its durable process binding.
    NotificationResolutionActionAccepted {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
    },
    /// Durable proof that an accepted Complete action consumed the composer.
    NotificationResolutionConsumptionObserved {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        evidence: NotificationResolutionConsumptionEvidence,
        observed_at_ms: u64,
    },
    /// Proven pre-key refusal of one durable resolution intent.
    NotificationResolutionIntentWithdrawn {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
    },
    /// Atomic no-key Discard after two fresh exact-empty composer proofs.
    ///
    /// This fact is the first durable boundary on the no-key path. A crash
    /// before it leaves no action to reconcile; a crash after it replays the
    /// completed resolution. The projection refuses every resolution except
    /// Discard and every attempt with an accepted terminal action.
    NotificationResolvedWithoutTerminalAction {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
    },
    /// Operator resolution of one exact staged notification attempt after
    /// positive post-action composer evidence.
    NotificationResolved {
        record_version: u32,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        proof_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
    },
    /// An authenticated claim won before submit, after the doorbell was staged.
    ///
    /// The daemon appends this fact after exact staged bytes are cleared, or
    /// during crash recovery after the same binding and a positive visible-empty
    /// composer proof. Replay changes the notification state and retires its
    /// composer barrier together.
    NotificationClaimedStagedCleared {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
    },
    /// A late exact recipient claim reconciled one doorbell ACK timeout.
    ///
    /// The daemon appends this only after the exact bound composer was cleared,
    /// or after crash recovery proved that same composer visibly clean. The
    /// fact carries identity only and retires the composer barrier atomically.
    NotificationClaimedAckTimeoutReconciled {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
    },
    /// Durable retirement of one post-write composer barrier.
    NotificationBarrierRetired {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        cause: NotificationBarrierRetirementCause,
        /// Exact replacement binding for an occupant-replacement proof.
        #[serde(skip_serializing_if = "Option::is_none")]
        replacement: Option<NotificationBinding>,
    },
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// Rebuild compact doorbell format 1 for writing and recovery.
pub fn render_doorbell_v1(oldest_msg_id: &MessageId) -> String {
    format!("cyclops inbox claim {oldest_msg_id}")
}

/// Rebuild attempt-bound doorbell format 2 for writing and recovery.
///
/// The suffix is a shell comment, so the command remains directly runnable.
/// Its 22 URL-safe characters encode every bit of the attempt UUID. Exact
/// payload receipts therefore cannot alias two attempts for the same message.
pub fn render_doorbell_v2(oldest_msg_id: &MessageId, attempt_id: NotificationAttemptId) -> String {
    format!(
        "cyclops inbox claim {oldest_msg_id} #c:{}",
        notification_attempt_token(attempt_id)
    )
}

/// Rebuild single-row attempt-locator doorbell format 3.
pub fn render_doorbell_v3(attempt_id: NotificationAttemptId) -> String {
    format!(
        "cyclops inbox claim {}",
        notification_attempt_claim_locator(attempt_id)
    )
}

/// Parse the exact message and attempt identities carried by doorbell v2.
///
/// A single trailing newline is the only accepted transport normalization,
/// matching authenticated hook payload handling. The attempt token is decoded
/// losslessly and returned as the validated typed identifier.
pub fn parse_doorbell_v2(payload: &str) -> Option<(MessageId, NotificationAttemptId)> {
    let payload = payload.strip_suffix('\n').unwrap_or(payload);
    let rest = payload.strip_prefix("cyclops inbox claim ")?;
    let (message_id, token) = rest.split_once(" #c:")?;
    if token.chars().any(char::is_whitespace) {
        return None;
    }
    Some((
        MessageId::new(message_id).ok()?,
        parse_notification_attempt_token(token)?,
    ))
}

/// Encode every bit of one attempt identity as 22 URL-safe characters.
fn notification_attempt_token(attempt_id: NotificationAttemptId) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = attempt_id.0.as_bytes();
    let mut encoded = String::with_capacity(22);
    for chunk in bytes[..15].as_chunks::<3>().0 {
        encoded.push(ALPHABET[(chunk[0] >> 2) as usize] as char);
        encoded.push(ALPHABET[(((chunk[0] & 0x03) << 4) | (chunk[1] >> 4)) as usize] as char);
        encoded.push(ALPHABET[(((chunk[1] & 0x0f) << 2) | (chunk[2] >> 6)) as usize] as char);
        encoded.push(ALPHABET[(chunk[2] & 0x3f) as usize] as char);
    }
    let last = bytes[15];
    encoded.push(ALPHABET[(last >> 2) as usize] as char);
    encoded.push(ALPHABET[((last & 0x03) << 4) as usize] as char);
    encoded
}

/// Decode only the canonical, lossless 22-character attempt token.
fn parse_notification_attempt_token(encoded: &str) -> Option<NotificationAttemptId> {
    if encoded.len() != 22 || !encoded.is_ascii() {
        return None;
    }
    let values: Vec<u8> = encoded
        .bytes()
        .map(decode_attempt_character)
        .collect::<Option<_>>()?;
    if values[21] & 0x0f != 0 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (chunk, values) in bytes[..15]
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(values[..20].as_chunks::<4>().0)
    {
        chunk[0] = (values[0] << 2) | (values[1] >> 4);
        chunk[1] = (values[1] << 4) | (values[2] >> 2);
        chunk[2] = (values[2] << 6) | values[3];
    }
    bytes[15] = (values[20] << 2) | (values[21] >> 4);
    let uuid = Uuid::from_bytes(bytes);
    let attempt_id = (!uuid.is_nil()).then_some(NotificationAttemptId(uuid))?;
    (notification_attempt_token(attempt_id) == encoded).then_some(attempt_id)
}

/// Build the canonical message-shaped locator understood by `inbox.claim`.
///
/// Production message ids are `m-` plus 32 lowercase hex characters. The
/// reserved `m-att_` prefix is therefore disjoint from every minted id while
/// remaining valid input for older positional claim clients.
pub fn notification_attempt_claim_locator(attempt_id: NotificationAttemptId) -> MessageId {
    MessageId::new(format!(
        "{NOTIFICATION_ATTEMPT_CLAIM_LOCATOR_PREFIX}{}",
        notification_attempt_token(attempt_id)
    ))
    .expect("the reserved attempt locator is a valid message-shaped id")
}

/// Decode only the canonical reserved locator for one exact attempt.
pub fn parse_notification_attempt_claim_locator(
    message_id: &MessageId,
) -> Option<NotificationAttemptId> {
    let token = message_id
        .as_str()
        .strip_prefix(NOTIFICATION_ATTEMPT_CLAIM_LOCATOR_PREFIX)?;
    let attempt_id = parse_notification_attempt_token(token)?;
    (notification_attempt_claim_locator(attempt_id).as_str() == message_id.as_str())
        .then_some(attempt_id)
}

/// Parse the exact attempt identity carried by doorbell format 3.
pub fn parse_doorbell_v3(payload: &str) -> Option<NotificationAttemptId> {
    let payload = payload.strip_suffix('\n').unwrap_or(payload);
    let locator = payload.strip_prefix("cyclops inbox claim ")?;
    let message_id = MessageId::new(locator).ok()?;
    parse_notification_attempt_claim_locator(&message_id)
}

/// Whether a doorbell format names one exact notification attempt.
pub const fn doorbell_format_names_exact_attempt(format: Option<u32>) -> bool {
    matches!(
        format,
        Some(DOORBELL_FORMAT_ATTEMPT_CLAIM | DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM)
    )
}

fn decode_attempt_character(character: u8) -> Option<u8> {
    match character {
        b'A'..=b'Z' => Some(character - b'A'),
        b'a'..=b'z' => Some(character - b'a' + 26),
        b'0'..=b'9' => Some(character - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// Rebuild the original doorbell for unresolved attempts written by an older daemon.
pub fn render_legacy_doorbell(oldest_msg_id: &MessageId) -> String {
    format!(
        "[cyclops] pending message {oldest_msg_id}: claim with 'cyclops inbox claim {oldest_msg_id}'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [NotificationState; 15] = [
        NotificationState::Queued,
        NotificationState::Gating,
        NotificationState::BlockedPreWrite,
        NotificationState::QuotaHeld,
        NotificationState::QuotaResetObserved,
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
        NotificationState::Notified,
        NotificationState::AttentionRequired,
        NotificationState::Withdrawn,
        NotificationState::WithdrawnAfterStaging,
        NotificationState::WithdrawnByOperator,
        NotificationState::Superseded,
    ];

    #[test]
    fn attempt_id_generation_and_strict_serde() {
        let generated = NotificationAttemptId::generate();
        let wire = serde_json::to_string(&generated).unwrap();
        let decoded: NotificationAttemptId = serde_json::from_str(&wire).unwrap();
        assert_eq!(decoded, generated);
        assert_eq!(generated.to_string().len(), 40);

        for invalid in [
            "",
            "att-",
            "att!550e8400-e29b-41d4-a716-446655440000",
            "att-550E8400-E29B-41D4-A716-446655440000",
            "att-550e8400e29b41d4a716446655440000",
            "att-00000000-0000-0000-0000-000000000000",
            "att-550e8400-e29b-41d4-a716-446655440000!",
        ] {
            assert!(NotificationAttemptId::parse(invalid).is_err(), "{invalid}");
            let json = serde_json::to_string(invalid).unwrap();
            assert!(serde_json::from_str::<NotificationAttemptId>(&json).is_err());
        }
    }

    #[test]
    fn manifest_id_is_nonempty_and_printable() {
        assert_eq!(
            NotificationManifestId::new("codex-0.147").unwrap().as_str(),
            "codex-0.147"
        );
        for invalid in ["", "two words", "line\nbreak"] {
            assert!(NotificationManifestId::new(invalid).is_err());
        }
    }

    #[test]
    fn pre_write_cause_names_match_the_wire() {
        let cases = [
            (
                NotificationPreWriteCause::SessionUnavailable,
                "session_unavailable",
            ),
            (
                NotificationPreWriteCause::ManifestUnavailable,
                "manifest_unavailable",
            ),
            (
                NotificationPreWriteCause::PayloadUnavailable,
                "payload_unavailable",
            ),
            (
                NotificationPreWriteCause::WriteReadinessChanged,
                "write_readiness_changed",
            ),
            (NotificationPreWriteCause::SpoolFailed, "spool_failed"),
            (
                NotificationPreWriteCause::BindingUnprovable,
                "binding_unprovable",
            ),
            (
                NotificationPreWriteCause::ComposerSemanticMissing,
                "composer_semantic_missing",
            ),
            (NotificationPreWriteCause::WorkerFailed, "worker_failed"),
        ];

        for (cause, wire_name) in cases {
            assert_eq!(cause.wire_name(), wire_name);
            assert_eq!(serde_json::to_value(cause).unwrap(), wire_name);
            assert!(!cause.label().is_empty());
        }
    }

    #[test]
    fn width_block_keeps_the_closed_prewrite_cause_rollback_decodable() {
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        enum LegacyPreWriteCause {
            WriteReadinessChanged,
        }
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        struct LegacyObservation {
            selected_manifest: Option<String>,
        }
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        struct LegacyBlock {
            pre_write_cause: LegacyPreWriteCause,
            pre_write_observation: LegacyObservation,
        }

        let current = serde_json::json!({
            "pre_write_cause": NotificationPreWriteCause::WriteReadinessChanged,
            "pre_write_observation": {
                "selected_manifest": "codex",
                "pane_width": 59,
                "required_pane_width": 60
            }
        });
        let legacy: LegacyBlock = serde_json::from_value(current).unwrap();
        assert_eq!(
            legacy.pre_write_cause,
            LegacyPreWriteCause::WriteReadinessChanged
        );
        assert_eq!(
            legacy.pre_write_observation.selected_manifest.as_deref(),
            Some("codex")
        );

        let prior: NotificationPreWriteObservation = serde_json::from_value(serde_json::json!({
            "selected_manifest": "codex",
            "pane_width": 59
        }))
        .unwrap();
        assert_eq!(prior.pane_width, Some(59));
        assert_eq!(prior.required_pane_width, None);
    }

    #[test]
    fn writing_record_carries_transport_separately_from_identity() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let binding = NotificationBinding {
            recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
            pane_root: Some(ProcessInstanceId::new(39, 88).unwrap()),
            leader: Some(ProcessInstanceId::new(40, 89).unwrap()),
            agent: ProcessInstanceId::new(41, 90).unwrap(),
            manifest: NotificationManifestId::new("test").unwrap(),
        };
        let record = NotificationRecord {
            attempt_id: NotificationAttemptId::generate(),
            message_id: MessageId::new("m-direct").unwrap(),
            recipient: binding.recipient,
            state: NotificationState::Writing,
            binding: Some(binding),
            transport: NotificationTransport::DirectPayload,
            doorbell_format: None,
            cause: None,
            pre_write_cause: None,
            pre_write_observation: None,
            pre_write_reopen_count: 0,
            started_seq: 1,
            updated_seq: 3,
            updated_at: 4,
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["transport"], "direct_payload");
        assert_eq!(
            serde_json::from_value::<NotificationRecord>(value)
                .unwrap()
                .transport,
            NotificationTransport::DirectPayload
        );
    }

    #[test]
    fn transition_table_keeps_pre_write_blocking_on_the_same_attempt() {
        use NotificationState::*;
        let legal = [
            (Queued, Gating),
            (Queued, WithdrawnByOperator),
            (Gating, BlockedPreWrite),
            (Gating, WithdrawnByOperator),
            (BlockedPreWrite, Gating),
            (BlockedPreWrite, Withdrawn),
            (BlockedPreWrite, WithdrawnByOperator),
            (Gating, Writing),
            (Gating, QuotaHeld),
            (QuotaHeld, QuotaResetObserved),
            (Writing, Staged),
            (Writing, AttentionRequired),
            (Staged, Submitting),
            (Staged, AttentionRequired),
            (Submitting, Submitted),
            (Submitting, AttentionRequired),
            (Submitted, Notified),
            (Submitted, AttentionRequired),
        ];

        for from in STATES {
            for to in STATES {
                assert_eq!(
                    from.can_transition_to(to),
                    legal.contains(&(from, to)),
                    "unexpected transition {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn operator_withdrawal_stops_at_the_write_boundary() {
        use NotificationState::*;

        for state in STATES {
            assert_eq!(
                state.can_withdraw_before_write(),
                matches!(state, Queued | Gating | BlockedPreWrite),
                "unexpected operator withdrawal authority for {state:?}"
            );
        }
    }

    #[test]
    fn attention_causes_match_the_prior_state() {
        use NotificationAttentionCause::*;
        use NotificationState::*;

        assert!(PasteFailed.valid_after(Writing));
        assert!(VerifyFailed.valid_after(Staged));
        assert!(VerifyFailed.valid_after(Submitting));
        assert!(SubmitFailed.valid_after(Staged));
        assert!(SubmitFailed.valid_after(Submitting));
        assert!(PaneReboundAfterPaste.valid_after(Submitting));
        assert!(AckTimeout.valid_after(Submitted));
        assert!(DaemonRestart.valid_after(Writing));
        assert!(!AckTimeout.valid_after(Writing));
        assert!(!VerifyFailed.valid_after(Gating));
    }

    #[test]
    fn claim_settlement_withdraws_only_pre_write_wakes() {
        use NotificationState::*;

        for state in [
            Queued,
            Gating,
            BlockedPreWrite,
            QuotaHeld,
            QuotaResetObserved,
        ] {
            assert_eq!(
                state.settled_by_claim(NotificationTransport::Doorbell),
                Withdrawn
            );
            assert_eq!(
                state.settled_by_claim(NotificationTransport::DirectPayload),
                Withdrawn
            );
        }
        for state in [Staged, Submitting] {
            assert_eq!(
                state.settled_by_claim(NotificationTransport::Doorbell),
                state
            );
            assert_eq!(
                state.settled_by_claim(NotificationTransport::DirectPayload),
                state
            );
        }
        assert_eq!(
            Submitted.settled_by_claim(NotificationTransport::Doorbell),
            Notified
        );
        assert_eq!(
            Submitted.settled_by_claim(NotificationTransport::DirectPayload),
            Submitted
        );
        for state in [
            Writing,
            Notified,
            AttentionRequired,
            Withdrawn,
            WithdrawnAfterStaging,
            WithdrawnByOperator,
            Superseded,
        ] {
            assert_eq!(
                state.settled_by_claim(NotificationTransport::Doorbell),
                state
            );
            assert_eq!(
                state.settled_by_claim(NotificationTransport::DirectPayload),
                state
            );
        }
    }

    #[test]
    fn exact_attempt_ack_timeout_requires_composer_reconciliation_after_claim() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient = RecipientKey::agent(workspace, session, "%3".parse().unwrap());
        let binding = NotificationBinding {
            recipient,
            pane_root: Some(ProcessInstanceId::new(39, 88).unwrap()),
            leader: Some(ProcessInstanceId::new(40, 89).unwrap()),
            agent: ProcessInstanceId::new(41, 90).unwrap(),
            manifest: NotificationManifestId::new("claude").unwrap(),
        };
        let record = NotificationRecord {
            attempt_id: NotificationAttemptId::generate(),
            message_id: MessageId::new("m-late-claim").unwrap(),
            recipient,
            state: NotificationState::AttentionRequired,
            binding: Some(binding),
            transport: NotificationTransport::Doorbell,
            doorbell_format: Some(DOORBELL_FORMAT_ATTEMPT_CLAIM),
            cause: Some(NotificationAttentionCause::AckTimeout),
            pre_write_cause: None,
            pre_write_observation: None,
            pre_write_reopen_count: 0,
            started_seq: 1,
            updated_seq: 7,
            updated_at: 8,
        };

        assert!(record.needs_claimed_ack_timeout_reconciliation());

        let mut current = record.clone();
        current.doorbell_format = Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM);
        assert!(current.needs_claimed_ack_timeout_reconciliation());
        current.cause = Some(NotificationAttentionCause::VerifyFailed);
        assert!(current.needs_exact_owned_reconciliation());

        let mut invalid = record.clone();
        invalid.cause = Some(NotificationAttentionCause::VerifyFailed);
        assert!(!invalid.needs_claimed_ack_timeout_reconciliation());
        assert!(invalid.needs_exact_owned_reconciliation());

        let mut invalid_exact = invalid.clone();
        invalid_exact.doorbell_format = Some(DOORBELL_FORMAT_COMPACT_CLAIM);
        assert!(!invalid_exact.needs_exact_owned_reconciliation());

        let mut invalid_exact = invalid.clone();
        invalid_exact.binding.as_mut().unwrap().pane_root = None;
        assert!(!invalid_exact.needs_exact_owned_reconciliation());

        let mut invalid = record.clone();
        invalid.doorbell_format = Some(DOORBELL_FORMAT_COMPACT_CLAIM);
        assert!(!invalid.needs_claimed_ack_timeout_reconciliation());

        let mut invalid = record.clone();
        invalid.binding.as_mut().unwrap().leader = None;
        assert!(!invalid.needs_claimed_ack_timeout_reconciliation());

        let mut invalid = record;
        invalid.binding.as_mut().unwrap().recipient = RecipientKey::admin(workspace);
        assert!(!invalid.needs_claimed_ack_timeout_reconciliation());
    }

    #[test]
    fn doorbell_renderer_format() {
        let msg_id = MessageId::new("m-3f9c2a").unwrap();
        assert_eq!(render_doorbell_v1(&msg_id), "cyclops inbox claim m-3f9c2a");
        assert_eq!(
            render_legacy_doorbell(&msg_id),
            "[cyclops] pending message m-3f9c2a: claim with 'cyclops inbox claim m-3f9c2a'"
        );
    }

    #[test]
    fn doorbell_v2_losslessly_names_one_exact_attempt() {
        let message_id = MessageId::new("m-3f9c2a").unwrap();
        let first =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001").unwrap();
        let second =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000002").unwrap();
        let first_payload = render_doorbell_v2(&message_id, first);
        let second_payload = render_doorbell_v2(&message_id, second);

        assert_ne!(first_payload, second_payload);
        assert_eq!(
            parse_doorbell_v2(&first_payload),
            Some((message_id.clone(), first))
        );
        assert_eq!(
            parse_doorbell_v2(&format!("{second_payload}\n")),
            Some((message_id, second))
        );
        assert_eq!(
            parse_doorbell_v2(&render_doorbell_v1(&MessageId::new("m-3f9c2a").unwrap())),
            None
        );
    }

    #[test]
    fn doorbell_v3_is_exact_single_line_and_rejects_noncanonical_locators() {
        let first =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001").unwrap();
        let second =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000002").unwrap();
        let hyphen =
            NotificationAttemptId::parse("att-f8000000-0000-4000-8000-000000000001").unwrap();
        let double_hyphen =
            NotificationAttemptId::parse("att-fbe00000-0000-4000-8000-000000000001").unwrap();
        let first_token = notification_attempt_token(first);
        let first_locator = notification_attempt_claim_locator(first);
        let first_payload = render_doorbell_v3(first);

        assert_eq!(first_token.len(), 22);
        assert_eq!(first_locator.as_str(), format!("m-att_{first_token}"));
        assert_eq!(
            parse_notification_attempt_claim_locator(&first_locator),
            Some(first)
        );
        assert_ne!(first_payload, render_doorbell_v3(second));
        assert_eq!(parse_doorbell_v3(&first_payload), Some(first));
        assert_eq!(
            parse_doorbell_v3(&format!("{first_payload}\n")),
            Some(first)
        );
        assert_eq!(parse_notification_attempt_token(&first_token), Some(first));
        assert_eq!(parse_doorbell_v3(&format!("{first_payload} extra")), None);
        assert_eq!(parse_doorbell_v3(&format!(" {first_payload}")), None);
        assert_eq!(
            parse_notification_attempt_token("AAAAAAAAQACAAAAAAAAAAR"),
            None
        );
        assert_eq!(
            parse_notification_attempt_claim_locator(
                &MessageId::new("m-att_AAAAAAAAQACAAAAAAAAAAR").unwrap()
            ),
            None
        );
        assert_eq!(
            parse_notification_attempt_claim_locator(
                &MessageId::new("m-00000000000040008000000000000001").unwrap()
            ),
            None
        );
        assert_eq!(parse_doorbell_v2(&first_payload), None);
        assert_eq!(notification_attempt_token(hyphen), "-AAAAAAAQACAAAAAAAAAAQ");
        assert_eq!(
            notification_attempt_token(double_hyphen),
            "--AAAAAAQACAAAAAAAAAAQ"
        );
        assert_eq!(
            parse_notification_attempt_token("--AAAAAAQACAAAAAAAAAAQ"),
            Some(double_hyphen)
        );
        assert_eq!(
            parse_doorbell_v3("cyclops inbox claim m-att_--AAAAAAQACAAAAAAAAAAQ"),
            Some(double_hyphen)
        );
        assert_eq!(2 + first_payload.chars().count(), 50);
        assert!(2 + first_payload.chars().count() <= DOORBELL_V3_MIN_PANE_WIDTH as usize);
    }

    #[test]
    fn legacy_compact_doorbell_fits_the_narrow_validation_pane() {
        let msg_id = MessageId::new("m-0123456789abcdef0123456789abcdef").unwrap();
        let doorbell = render_doorbell_v1(&msg_id);

        assert!(
            2 + doorbell.chars().count() <= 60,
            "prompt plus generated message id must fit one narrow row: {doorbell}"
        );
    }

    #[test]
    fn attempt_bound_doorbell_stays_within_two_narrow_rows() {
        let message_id = MessageId::new("m-0123456789abcdef0123456789abcdef").unwrap();
        let attempt_id =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001").unwrap();
        let doorbell = render_doorbell_v2(&message_id, attempt_id);

        assert!(
            2 + doorbell.chars().count() <= 120,
            "prompt plus full daemon ids must fit two narrow rows: {doorbell}"
        );
        assert_eq!(parse_doorbell_v2(&doorbell), Some((message_id, attempt_id)));
    }

    #[test]
    fn doorbell_format_is_additive_and_optional_on_the_wire() {
        #[allow(dead_code)]
        #[derive(serde::Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum LegacyNotificationFact {
            NotificationTransition {
                record_version: u32,
                attempt_id: NotificationAttemptId,
                message_id: MessageId,
                recipient: RecipientKey,
                state: NotificationState,
                binding: Option<NotificationBinding>,
                #[serde(default)]
                transport: Option<NotificationTransport>,
                cause: Option<NotificationAttentionCause>,
            },
        }

        let message_id = MessageId::new("m-compact").unwrap();
        let attempt_id = NotificationAttemptId::generate();
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient = RecipientKey::agent(workspace, session, "%3".parse().unwrap());
        let legacy = serde_json::json!({
            "type": "notification_transition",
            "record_version": 1,
            "attempt_id": attempt_id,
            "message_id": message_id,
            "recipient": recipient,
            "state": "writing",
            "binding": null,
            "transport": "doorbell",
            "cause": null
        });
        let decoded: NotificationFact = serde_json::from_value(legacy).unwrap();
        assert!(matches!(
            decoded,
            NotificationFact::NotificationTransition {
                doorbell_format: None,
                ..
            }
        ));

        let current = NotificationFact::NotificationTransition {
            record_version: 1,
            attempt_id,
            message_id,
            recipient,
            state: NotificationState::Writing,
            binding: None,
            transport: Some(NotificationTransport::Doorbell),
            doorbell_format: Some(DOORBELL_FORMAT_ATTEMPT_CLAIM),
            cause: None,
            pre_write_cause: None,
            pre_write_observation: None,
        };
        let encoded = serde_json::to_value(current).unwrap();
        assert_eq!(encoded["doorbell_format"], DOORBELL_FORMAT_ATTEMPT_CLAIM);
        let legacy_reader: LegacyNotificationFact =
            serde_json::from_value(encoded).expect("legacy reader ignores additive fields");
        assert!(matches!(
            legacy_reader,
            LegacyNotificationFact::NotificationTransition {
                record_version: 1,
                transport: Some(NotificationTransport::Doorbell),
                ..
            }
        ));
    }

    #[test]
    fn resolution_fact_contains_identity_and_decision_only() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let fact = NotificationFact::NotificationResolved {
            record_version: 1,
            proof_version: NOTIFICATION_RESOLUTION_PROOF_VERSION,
            attempt_id: NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000003")
                .unwrap(),
            message_id: MessageId::new("m-private").unwrap(),
            recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
            resolution: NotificationResolution::Discard,
        };

        let value = serde_json::to_value(&fact).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "attempt_id",
                "message_id",
                "proof_version",
                "recipient",
                "record_version",
                "resolution",
                "type",
            ]
        );
        assert_eq!(
            value["proof_version"],
            NOTIFICATION_RESOLUTION_PROOF_VERSION
        );
        assert_eq!(value["resolution"], "discard");
        assert!(value.get("body").is_none());
        assert!(value.get("composer").is_none());
        assert!(value.get("diff").is_none());

        let mut legacy = value;
        legacy.as_object_mut().unwrap().remove("proof_version");
        assert!(matches!(
            serde_json::from_value::<NotificationFact>(legacy).unwrap(),
            NotificationFact::NotificationResolved {
                proof_version: 0,
                ..
            }
        ));
    }

    #[test]
    fn batch_requeue_fact_contains_identity_only() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let fact = NotificationFact::NotificationsRequeued {
            record_version: 1,
            message_id: MessageId::new("m-private").unwrap(),
            requeues: vec![NotificationRequeue {
                prior_attempt_id: NotificationAttemptId::parse(
                    "att-00000000-0000-4000-8000-000000000003",
                )
                .unwrap(),
                attempt_id: NotificationAttemptId::parse(
                    "att-00000000-0000-4000-8000-000000000004",
                )
                .unwrap(),
                recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
            }],
        };

        let value = serde_json::to_value(&fact).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["message_id", "record_version", "requeues", "type"]);
        let mut entry_keys: Vec<_> = value["requeues"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        entry_keys.sort();
        assert_eq!(entry_keys, ["attempt_id", "prior_attempt_id", "recipient"]);
        assert_eq!(value["type"], "notifications_requeued");
        let replayed: NotificationFact = serde_json::from_value(value).unwrap();
        assert_eq!(replayed, fact);
    }

    #[test]
    fn barrier_retirement_contains_identity_without_composer_content() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let fact = NotificationFact::NotificationBarrierRetired {
            record_version: 1,
            attempt_id: NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001")
                .unwrap(),
            message_id: MessageId::new("m-recovery").unwrap(),
            recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
            cause: NotificationBarrierRetirementCause::PaneGone,
            replacement: None,
        };

        let value = serde_json::to_value(fact).unwrap();
        assert_eq!(value["type"], "notification_barrier_retired");
        assert_eq!(value["cause"], "pane_gone");
        assert!(value.get("body").is_none());
        assert!(value.get("composer").is_none());
        assert!(value.get("diff").is_none());
    }

    #[test]
    fn claimed_staged_clear_fact_contains_identity_only() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let fact = NotificationFact::NotificationClaimedStagedCleared {
            record_version: 1,
            attempt_id: NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001")
                .unwrap(),
            message_id: MessageId::new("m-claimed-staged").unwrap(),
            recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
        };

        let value = serde_json::to_value(&fact).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "attempt_id",
                "message_id",
                "recipient",
                "record_version",
                "type",
            ]
        );
        assert_eq!(value["type"], "notification_claimed_staged_cleared");
        assert!(value.get("body").is_none());
        assert!(value.get("composer").is_none());
        assert!(value.get("diff").is_none());
        assert_eq!(
            serde_json::from_value::<NotificationFact>(value).unwrap(),
            fact
        );
    }

    #[test]
    fn claimed_ack_timeout_reconciliation_fact_contains_identity_only() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let fact = NotificationFact::NotificationClaimedAckTimeoutReconciled {
            record_version: 1,
            attempt_id: NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001")
                .unwrap(),
            message_id: MessageId::new("m-claimed-ack-timeout").unwrap(),
            recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
        };

        let value = serde_json::to_value(&fact).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "attempt_id",
                "message_id",
                "recipient",
                "record_version",
                "type",
            ]
        );
        assert_eq!(value["type"], "notification_claimed_ack_timeout_reconciled");
        assert!(value.get("body").is_none());
        assert!(value.get("composer").is_none());
        assert!(value.get("diff").is_none());
        assert_eq!(
            serde_json::from_value::<NotificationFact>(value).unwrap(),
            fact
        );
    }

    #[test]
    fn old_bindings_replay_without_claiming_a_terminal_leader() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let binding = NotificationBinding {
            recipient: RecipientKey::admin(workspace),
            pane_root: Some(ProcessInstanceId::new(39, 88).unwrap()),
            leader: Some(ProcessInstanceId::new(40, 89).unwrap()),
            agent: ProcessInstanceId::new(42, 90).unwrap(),
            manifest: NotificationManifestId::new("codex").unwrap(),
        };
        let mut old = serde_json::to_value(binding).unwrap();
        old.as_object_mut().unwrap().remove("pane_root");
        old.as_object_mut().unwrap().remove("leader");
        let binding: NotificationBinding = serde_json::from_value(old).unwrap();
        assert_eq!(binding.pane_root, None);
        assert_eq!(binding.leader, None);
    }

    #[test]
    fn resolution_boundaries_are_content_free() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let attempt_id =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000003").unwrap();
        let message_id = MessageId::new("m-private").unwrap();
        let recipient = RecipientKey::agent(workspace, session, "%3".parse().unwrap());
        for (fact, fact_type) in [
            (
                NotificationFact::NotificationResolutionIntent {
                    record_version: 1,
                    attempt_id,
                    message_id: message_id.clone(),
                    recipient,
                    resolution: NotificationResolution::Complete,
                },
                "notification_resolution_intent",
            ),
            (
                NotificationFact::NotificationResolutionActionAccepted {
                    record_version: 1,
                    attempt_id,
                    message_id: message_id.clone(),
                    recipient,
                    resolution: NotificationResolution::Complete,
                },
                "notification_resolution_action_accepted",
            ),
            (
                NotificationFact::NotificationResolutionIntentWithdrawn {
                    record_version: 1,
                    attempt_id,
                    message_id: message_id.clone(),
                    recipient,
                    resolution: NotificationResolution::Complete,
                },
                "notification_resolution_intent_withdrawn",
            ),
            (
                NotificationFact::NotificationResolvedWithoutTerminalAction {
                    record_version: 1,
                    attempt_id,
                    message_id: message_id.clone(),
                    recipient,
                    resolution: NotificationResolution::Discard,
                },
                "notification_resolved_without_terminal_action",
            ),
        ] {
            let value = serde_json::to_value(&fact).unwrap();
            let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(
                keys,
                [
                    "attempt_id",
                    "message_id",
                    "recipient",
                    "record_version",
                    "resolution",
                    "type",
                ]
            );
            assert_eq!(value["type"], fact_type);
            assert!(value.get("body").is_none());
            assert!(value.get("composer").is_none());
            assert!(value.get("diff").is_none());
            assert_eq!(
                serde_json::from_value::<NotificationFact>(value).unwrap(),
                fact
            );
        }
    }

    #[test]
    fn resolution_consumption_observation_is_content_free_and_typed() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let fact = NotificationFact::NotificationResolutionConsumptionObserved {
            record_version: 1,
            attempt_id: NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000003")
                .unwrap(),
            message_id: MessageId::new("m-private").unwrap(),
            recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
            evidence: NotificationResolutionConsumptionEvidence::ExactHookPrompt,
            observed_at_ms: 17,
        };

        let value = serde_json::to_value(&fact).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "attempt_id",
                "evidence",
                "message_id",
                "observed_at_ms",
                "recipient",
                "record_version",
                "type",
            ]
        );
        assert_eq!(
            value["type"],
            "notification_resolution_consumption_observed"
        );
        assert_eq!(value["evidence"], "exact_hook_prompt");
        assert_eq!(value["observed_at_ms"], 17);
        assert!(value.get("body").is_none());
        assert!(value.get("composer").is_none());
        assert!(value.get("diff").is_none());
        assert_eq!(
            serde_json::from_value::<NotificationFact>(value).unwrap(),
            fact
        );
    }
}
