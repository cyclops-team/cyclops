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
/// Recovery may resume `Queued` or `Gating`, but any unresolved state from
/// `Writing` onward has an ambiguous outcome and requires attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationState {
    Queued,
    Gating,
    /// Quota was positively observed before any composer write.
    QuotaHeld,
    /// A later positive screen observation no longer showed quota.
    /// Only an explicit administrator requeue may leave this state.
    QuotaResetObserved,
    Writing,
    Staged,
    Submitted,
    Notified,
    AttentionRequired,
    /// The message was replaced before this attempt crossed the write boundary.
    Superseded,
}

impl NotificationState {
    /// Legal transitions for one attempt. Requeue is a separate fact.
    pub fn can_transition_to(self, next: NotificationState) -> bool {
        use NotificationState::*;
        match self {
            Queued => next == Gating,
            Gating => matches!(next, Writing | QuotaHeld),
            QuotaHeld => next == QuotaResetObserved,
            QuotaResetObserved => false,
            Writing => matches!(next, Staged | AttentionRequired),
            Staged => matches!(next, Submitted | AttentionRequired),
            Submitted => matches!(next, Notified | AttentionRequired),
            Notified | AttentionRequired | Superseded => false,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::QuotaHeld
                | Self::QuotaResetObserved
                | Self::Notified
                | Self::AttentionRequired
                | Self::Superseded
        )
    }
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

/// Durable reason an attempt no longer owns staged composer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationBarrierRetirementCause {
    /// A post-restart turn ended with the exact key carried by the recovered
    /// hold, and the same composer then read clean.
    LifecycleReconciled,
    /// A receipt-bearing attempt and the same bound composer read clean.
    ComposerObservedClear,
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
                PaneReboundAfterPaste | SubmitFailed | DaemonRestart | TransportOutcomeUnknown
            ),
            Submitted => matches!(
                self,
                ReceiptOccupantChanged | AckTimeout | DaemonRestart | TransportOutcomeUnknown
            ),
            Queued | Gating | QuotaHeld | QuotaResetObserved | Notified | AttentionRequired
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

/// Compact claim-command doorbell written by current daemons.
pub const DOORBELL_FORMAT_COMPACT_CLAIM: u32 = 1;

/// Content-free identity observed immediately before writing to the pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationBinding {
    pub recipient: RecipientKey,
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
    pub started_seq: u64,
    pub updated_seq: u64,
    pub updated_at: u64,
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
    /// Durable boundary before an operator terminal action.
    ///
    /// A matching final resolution proves the key was accepted. An intent
    /// without that fact is ambiguous and must never be repeated.
    NotificationResolutionIntent {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
    },
    /// Proven pre-key refusal of one durable resolution intent.
    NotificationResolutionIntentWithdrawn {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
    },
    /// Operator resolution of one exact staged notification attempt.
    NotificationResolved {
        record_version: u32,
        attempt_id: NotificationAttemptId,
        message_id: MessageId,
        recipient: RecipientKey,
        resolution: NotificationResolution,
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

/// Rebuild compact doorbell format 1 for writing and recovery.
pub fn render_doorbell_v1(oldest_msg_id: &MessageId) -> String {
    format!("cyclops inbox claim {oldest_msg_id}")
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

    const STATES: [NotificationState; 10] = [
        NotificationState::Queued,
        NotificationState::Gating,
        NotificationState::QuotaHeld,
        NotificationState::QuotaResetObserved,
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitted,
        NotificationState::Notified,
        NotificationState::AttentionRequired,
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
    fn writing_record_carries_transport_separately_from_identity() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let binding = NotificationBinding {
            recipient: RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
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
    fn transition_table_has_no_pre_write_attention_or_retry() {
        use NotificationState::*;
        let legal = [
            (Queued, Gating),
            (Gating, Writing),
            (Gating, QuotaHeld),
            (QuotaHeld, QuotaResetObserved),
            (Writing, Staged),
            (Writing, AttentionRequired),
            (Staged, Submitted),
            (Staged, AttentionRequired),
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
    fn attention_causes_match_the_prior_state() {
        use NotificationAttentionCause::*;
        use NotificationState::*;

        assert!(PasteFailed.valid_after(Writing));
        assert!(SubmitFailed.valid_after(Staged));
        assert!(AckTimeout.valid_after(Submitted));
        assert!(DaemonRestart.valid_after(Writing));
        assert!(!AckTimeout.valid_after(Writing));
        assert!(!VerifyFailed.valid_after(Gating));
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
    fn current_doorbell_fits_the_narrow_validation_pane() {
        let msg_id = MessageId::new("m-0123456789abcdef0123456789abcdef").unwrap();
        let doorbell = render_doorbell_v1(&msg_id);

        assert!(
            2 + doorbell.chars().count() <= 60,
            "prompt plus generated message id must fit one narrow row: {doorbell}"
        );
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
            doorbell_format: Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            cause: None,
        };
        let encoded = serde_json::to_value(current).unwrap();
        assert_eq!(encoded["doorbell_format"], DOORBELL_FORMAT_COMPACT_CLAIM);
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
                "recipient",
                "record_version",
                "resolution",
                "type",
            ]
        );
        assert_eq!(value["resolution"], "discard");
        assert!(value.get("body").is_none());
        assert!(value.get("composer").is_none());
        assert!(value.get("diff").is_none());
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
    fn old_bindings_replay_without_claiming_a_terminal_leader() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let binding = NotificationBinding {
            recipient: RecipientKey::admin(workspace),
            leader: Some(ProcessInstanceId::new(40, 89).unwrap()),
            agent: ProcessInstanceId::new(42, 90).unwrap(),
            manifest: NotificationManifestId::new("codex").unwrap(),
        };
        let mut old = serde_json::to_value(binding).unwrap();
        old.as_object_mut().unwrap().remove("leader");
        let binding: NotificationBinding = serde_json::from_value(old).unwrap();
        assert_eq!(binding.leader, None);
    }

    #[test]
    fn resolution_intent_is_content_free() {
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
                NotificationFact::NotificationResolutionIntentWithdrawn {
                    record_version: 1,
                    attempt_id,
                    message_id: message_id.clone(),
                    recipient,
                    resolution: NotificationResolution::Complete,
                },
                "notification_resolution_intent_withdrawn",
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
        }
    }
}
