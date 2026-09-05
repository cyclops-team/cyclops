//! Mailbox and durable messaging data structures.
//!
//! Pure data types defining message identifiers, semantic request digests,
//! per-recipient mailbox entries, and state transition facts.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::{Kind, RecipientKey, WorkspaceId};

/// Explicit canonical workspace record version discriminator.
pub const CANONICAL_RECORD_VERSION: u32 = 1;

/// Errors occurring during mailbox type validation and deserialization.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MailboxTypeError {
    #[error("message id must be 'cyc-<thread>-<message>' (two runs of 8 lowercase hex characters) or a legacy 'm-' id: '{0}'")]
    InvalidMessageId(String),
    #[error(
        "request digest must have 'v1:' prefix followed by 64 lowercase hex characters: '{0}'"
    )]
    InvalidRequestDigest(String),
    #[error("failed to serialize semantic payload for digest: {0}")]
    SerializationError(String),
    #[error("message summary must be one non-empty line with no control characters")]
    InvalidMessageSummary,
}

/// Validate the preview line pasted beside the exact claim command.
///
/// One non-empty line with no control characters (so no newline), so
/// terminal staging and durable reconstruction see the same bytes. There is
/// no length cap: the pane shows one row and a long summary simply wraps,
/// and the CLI warns the sender when a summary runs long.
pub fn validate_message_summary(summary: &str) -> Result<(), MailboxTypeError> {
    if summary.is_empty() || summary.trim() != summary || summary.chars().any(char::is_control) {
        return Err(MailboxTypeError::InvalidMessageSummary);
    }
    Ok(())
}

/// The pane preview for a message that carries no sender-authored summary.
///
/// The subject is the one line a sender already wrote for a reader, so it
/// is the preview. The body is used only when the subject is empty, and then
/// only its first line, so a body never reaches a pane by accident: bodies
/// stay in the mailbox until claimed. Either source is cut to a single line
/// of at most 200 characters. None when neither yields a valid summary.
pub fn derive_message_summary(body: &str, subject: &str) -> Option<String> {
    const DERIVED_SUMMARY_MAX_CHARS: usize = 200;
    [subject, body].into_iter().find_map(|source| {
        let line = source
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?;
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let summary: String = line.chars().take(DERIVED_SUMMARY_MAX_CHARS).collect();
        let summary = summary.trim_end().to_string();
        validate_message_summary(&summary).ok().map(|()| summary)
    })
}

/// Prefix of every message id the daemon mints now.
pub const MESSAGE_ID_PREFIX: &str = "cyc-";
/// Length of each hex run in a `cyc-<thread>-<message>` id.
pub const MESSAGE_ID_PART_LEN: usize = 8;

/// Validated unique identifier for a message record.
///
/// The daemon mints `cyc-<thread>-<message>`, two runs of eight lowercase
/// hex characters: every message in one thread shares the `<thread>` run and
/// gets its own `<message>` run, so a reader can tell from the id alone which
/// conversation a message belongs to. Older journals hold the legacy `m-`
/// form (`m-` plus 32 lowercase hex characters), which still replays, and
/// the reserved `m-att_<22-character token>` locator a doorbell line carries
/// shares this type so a positional claim client can pass it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageId(String);

impl MessageId {
    /// Construct and validate a message identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, MailboxTypeError> {
        let value = value.into();
        if let Some(parts) = value.strip_prefix(MESSAGE_ID_PREFIX) {
            let valid = parts
                .split_once('-')
                .is_some_and(|(thread, message)| is_id_part(thread) && is_id_part(message));
            if !valid {
                return Err(MailboxTypeError::InvalidMessageId(value));
            }
            return Ok(Self(value));
        }
        let suffix = value
            .strip_prefix("m-")
            .ok_or_else(|| MailboxTypeError::InvalidMessageId(value.clone()))?;

        if suffix.is_empty()
            || !suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(MailboxTypeError::InvalidMessageId(value));
        }
        Ok(Self(value))
    }

    /// Mint the id of a new thread root: fresh `<thread>` and `<message>`
    /// runs.
    pub fn mint_root() -> Self {
        let random = uuid::Uuid::new_v4().simple().to_string();
        Self(format!(
            "{MESSAGE_ID_PREFIX}{}-{}",
            &random[..MESSAGE_ID_PART_LEN],
            &random[random.len() - MESSAGE_ID_PART_LEN..]
        ))
    }

    /// Mint the id of a message inside the thread rooted at `thread_root`:
    /// the root's `<thread>` run and a fresh `<message>` run. A legacy root
    /// has no `<thread>` run of its own, so its replies share one derived
    /// from the root id, which keeps every reply to it in one thread.
    pub fn mint_in_thread(thread_root: &MessageId) -> Self {
        let random = uuid::Uuid::new_v4().simple().to_string();
        Self(format!(
            "{MESSAGE_ID_PREFIX}{}-{}",
            thread_root.thread_key(),
            &random[..MESSAGE_ID_PART_LEN]
        ))
    }

    /// The `<thread>` run of a `cyc-` id. None for a legacy id.
    pub fn thread_part(&self) -> Option<&str> {
        self.parts().map(|(thread, _)| thread)
    }

    /// The `<message>` run of a `cyc-` id. None for a legacy id.
    pub fn message_part(&self) -> Option<&str> {
        self.parts().map(|(_, message)| message)
    }

    /// The `<thread>` run every message in this id's thread carries when
    /// this id is the thread root: its own run, or for a legacy root a
    /// stable run derived from the id.
    fn thread_key(&self) -> String {
        match self.thread_part() {
            Some(thread) => thread.to_string(),
            None => {
                let digest = Sha256::digest(self.0.as_bytes());
                digest
                    .iter()
                    .take(MESSAGE_ID_PART_LEN / 2)
                    .map(|byte| format!("{byte:02x}"))
                    .collect()
            }
        }
    }

    fn parts(&self) -> Option<(&str, &str)> {
        self.0
            .strip_prefix(MESSAGE_ID_PREFIX)
            .and_then(|parts| parts.split_once('-'))
    }

    /// Access the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_id_part(part: &str) -> bool {
    part.len() == MESSAGE_ID_PART_LEN
        && part
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl FromStr for MessageId {
    type Err = MailboxTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for MessageId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Validated stable versioned SHA-256 semantic request digest (e.g. "v1:<64 hex>").
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestDigest(String);

/// Message fields covered by a semantic request digest.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestContent<'a> {
    pub subject: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub body: Option<&'a str>,
}

impl RequestDigest {
    /// Construct and validate a request digest string.
    pub fn parse(value: impl Into<String>) -> Result<Self, MailboxTypeError> {
        let value = value.into();
        let hex_part = value
            .strip_prefix("v1:")
            .ok_or_else(|| MailboxTypeError::InvalidRequestDigest(value.clone()))?;

        if hex_part.len() != 64
            || !hex_part
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        {
            return Err(MailboxTypeError::InvalidRequestDigest(value));
        }
        Ok(Self(value))
    }

    /// Compute a canonical versioned SHA-256 digest from semantic message fields.
    ///
    /// Note: thread_root is daemon-derived and excluded from the request digest
    /// to preserve idempotency identity between initial requests and retries.
    pub fn compute(
        kind: Kind,
        sender: RecipientKey,
        recipients: &[RecipientKey],
        content: RequestContent<'_>,
        reply_to: Option<&MessageId>,
        supersedes: Option<&MessageId>,
    ) -> Result<Self, MailboxTypeError> {
        let mut sorted_recipients = recipients.to_vec();
        sorted_recipients.sort();

        #[derive(Serialize)]
        struct CanonicalSemanticPayload<'a> {
            kind: Kind,
            sender: RecipientKey,
            recipients: &'a [RecipientKey],
            subject: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            summary: Option<&'a str>,
            body: Option<&'a str>,
            reply_to: Option<&'a MessageId>,
            #[serde(skip_serializing_if = "Option::is_none")]
            supersedes: Option<&'a MessageId>,
        }

        let payload = CanonicalSemanticPayload {
            kind,
            sender,
            recipients: &sorted_recipients,
            subject: content.subject,
            summary: content.summary,
            body: content.body,
            reply_to,
            supersedes,
        };

        let bytes = serde_json::to_vec(&payload)
            .map_err(|e| MailboxTypeError::SerializationError(e.to_string()))?;

        let hash = Sha256::digest(&bytes);
        let mut hex_str = String::with_capacity(67);
        hex_str.push_str("v1:");
        for byte in hash {
            use std::fmt::Write;
            let _ = write!(hex_str, "{:02x}", byte);
        }
        Ok(Self(hex_str))
    }

    /// Access the raw digest string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RequestDigest {
    type Err = MailboxTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for RequestDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for RequestDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RequestDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(s).map_err(serde::de::Error::custom)
    }
}

/// Strongly typed message metadata stored in the ledger line's structured data payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMetadata {
    /// Explicit canonical workspace record version discriminator (must equal CANONICAL_RECORD_VERSION).
    pub record_version: u32,
    /// Workspace scope boundary.
    pub workspace_id: WorkspaceId,
    /// Authoritative sender key.
    pub sender: RecipientKey,
    /// Target recipient addresses.
    pub recipients: Vec<RecipientKey>,
    /// Immutable human-readable labels captured when the message is accepted.
    pub presentation: MessagePresentation,
    /// Sender-authored two-sentence preview shown beside an exact claim.
    /// Older messages omit it and keep their original notification format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Authoritative thread root message identifier (required).
    pub thread_root: MessageId,
    /// Optional sender-scoped idempotency client key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    /// Stable versioned SHA-256 semantic request digest.
    pub request_digest: RequestDigest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<MessageId>,
    /// The sender asked for a raw write: the whole message is pasted and
    /// submitted with no composer check and no receipt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw: bool,
    /// The sender addressed every agent (`*`) rather than naming them, so
    /// the doorbell header reads `to all` instead of listing the labels.
    /// Absent on rows written before it existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub broadcast: bool,
}

/// Human-readable labels bound to authoritative message identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePresentation {
    pub sender_label: String,
    pub recipient_labels: Vec<RecipientPresentation>,
}

/// One recipient key and the immutable label shown for this message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientPresentation {
    pub recipient: RecipientKey,
    pub label: String,
}

/// Invariant-preserving lifecycle state of an entry in a recipient's mailbox.
///
/// The claimant is the authenticated [`RecipientKey`] of the agent that claimed
/// the entry, establishing a durable routing identity rather than a transient process ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MailboxEntryState {
    /// Waiting in recipient's mailbox to be claimed.
    Pending,
    /// Claimed by the authenticated recipient key.
    Claimed {
        claimant: RecipientKey,
        claimed_at: u64,
    },
    /// The compatibility lane delivered the full payload through the verified
    /// terminal pipeline. This does not imply a mailbox fetch or task completion.
    DeliveredDirect {
        attempt_id: crate::NotificationAttemptId,
        delivered_at: u64,
    },
    Superseded {
        by: MessageId,
        superseded_at: u64,
    },
}

impl MailboxEntryState {
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    pub fn is_claimed(&self) -> bool {
        matches!(self, Self::Claimed { .. })
    }

    pub fn claimant(&self) -> Option<RecipientKey> {
        match self {
            Self::Pending | Self::DeliveredDirect { .. } | Self::Superseded { .. } => None,
            Self::Claimed { claimant, .. } => Some(*claimant),
        }
    }
}

/// Projected mailbox entry for a single recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxEntry {
    /// Canonical message identifier.
    pub message_id: MessageId,
    /// Recipient address.
    pub recipient: RecipientKey,
    /// Invariant-preserving mailbox state.
    pub state: MailboxEntryState,
    /// Authoritative monotonic sequence number from the workspace journal.
    pub seq: u64,
    /// Creation timestamp (Unix ms).
    pub created_at: u64,
}

/// Body-free mailbox row suitable for list output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxListItem {
    pub entry: MailboxEntry,
    pub kind: Kind,
    pub sender: RecipientKey,
    pub sender_label: String,
    pub recipient_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    pub thread_root: MessageId,
}

/// Mailbox-specific state transition facts recorded in ledger data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MailboxFact {
    /// A recipient claimed a pending message.
    MessageClaimed {
        /// Explicit canonical record version discriminator (must equal CANONICAL_RECORD_VERSION).
        record_version: u32,
        /// Message identifier being claimed.
        message_id: MessageId,
        /// Recipient address owning the mailbox entry.
        recipient: RecipientKey,
        /// Authenticated claimant identity (must equal recipient).
        claimant: RecipientKey,
    },
    /// Replay only: no longer written since 1.1.0. An older daemon retired
    /// a mailbox entry after its direct payload delivery.
    MessageDeliveredDirect {
        record_version: u32,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: crate::NotificationAttemptId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionInstanceId;
    use crate::TmuxPaneId;

    fn test_recipients() -> (WorkspaceId, RecipientKey, RecipientKey) {
        let ws = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let sess = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let p1 = TmuxPaneId::from_str("%1").unwrap();
        let p2 = TmuxPaneId::from_str("%2").unwrap();
        (
            ws,
            RecipientKey::agent(ws, sess, p1),
            RecipientKey::agent(ws, sess, p2),
        )
    }

    #[test]
    fn message_id_validation_and_serde() {
        let valid = MessageId::new("m-valid_123").unwrap();
        assert_eq!(valid.as_str(), "m-valid_123");

        let json = serde_json::to_string(&valid).unwrap();
        let deserialized: MessageId = serde_json::from_str(&json).unwrap();
        assert_eq!(valid, deserialized);

        // Negative tests
        assert!(MessageId::new("").is_err());
        assert!(MessageId::new("plain_id").is_err());
        assert!(MessageId::new("m-").is_err());
        assert!(MessageId::new("m-spaces not allowed").is_err());

        // Serde deserialization negative test
        assert!(serde_json::from_str::<MessageId>(r#""invalid""#).is_err());
    }

    #[test]
    fn thread_aware_ids_carry_their_parts_and_legacy_ids_still_parse() {
        let threaded = MessageId::new("cyc-1a2b3c4d-5e6f7a8b").unwrap();
        assert_eq!(threaded.thread_part(), Some("1a2b3c4d"));
        assert_eq!(threaded.message_part(), Some("5e6f7a8b"));
        let json = serde_json::to_string(&threaded).unwrap();
        assert_eq!(serde_json::from_str::<MessageId>(&json).unwrap(), threaded);

        // Strict: two runs of exactly eight lowercase hex characters.
        for invalid in [
            "cyc-",
            "cyc-1a2b3c4d",
            "cyc-1a2b3c4d-",
            "cyc-1A2B3C4D-5e6f7a8b",
            "cyc-1a2b3c4-5e6f7a8b",
            "cyc-1a2b3c4d-5e6f7a8b9",
            "cyc-1a2b3c4d-5e6f7a8b-0",
            "cyc-ghijklmn-5e6f7a8b",
        ] {
            assert!(MessageId::new(invalid).is_err(), "{invalid}");
        }

        // The forms an old journal and a doorbell line carry.
        let legacy = MessageId::new("m-0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(legacy.thread_part(), None);
        assert_eq!(legacy.message_part(), None);
        let locator = MessageId::new("m-att_ASNFZ4mrTe-BI0VniavN7w").unwrap();
        assert_eq!(locator.thread_part(), None);
    }

    #[test]
    fn minted_ids_share_the_thread_run_and_mint_a_fresh_message_run() {
        let root = MessageId::mint_root();
        assert!(MessageId::new(root.as_str()).is_ok(), "{root}");
        let reply = MessageId::mint_in_thread(&root);
        assert!(MessageId::new(reply.as_str()).is_ok(), "{reply}");
        assert_eq!(reply.thread_part(), root.thread_part());
        assert_ne!(reply.message_part(), root.message_part());
        assert_ne!(reply, root);

        // Replies to a legacy root all land in one thread derived from it.
        let legacy = MessageId::new("m-0123456789abcdef0123456789abcdef").unwrap();
        let first = MessageId::mint_in_thread(&legacy);
        let second = MessageId::mint_in_thread(&legacy);
        assert_eq!(first.thread_part(), second.thread_part());
        assert_eq!(first.thread_part().map(str::len), Some(MESSAGE_ID_PART_LEN));
        assert_ne!(first.message_part(), second.message_part());
    }

    #[test]
    fn direct_delivery_is_not_a_claim() {
        let (_, recipient, _) = test_recipients();
        let attempt_id =
            crate::NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000003")
                .unwrap();
        let state = MailboxEntryState::DeliveredDirect {
            attempt_id,
            delivered_at: 42,
        };
        assert!(!state.is_pending());
        assert!(!state.is_claimed());
        assert_eq!(state.claimant(), None);

        let fact = MailboxFact::MessageDeliveredDirect {
            record_version: 1,
            message_id: MessageId::new("m-direct").unwrap(),
            recipient,
            attempt_id,
        };
        let value = serde_json::to_value(fact).unwrap();
        assert_eq!(value["type"], "message_delivered_direct");
        assert!(value.get("claimant").is_none());
    }

    #[test]
    fn request_digest_exact_golden_vector_and_invariance() {
        let (ws, r1, r2) = test_recipients();
        let admin = RecipientKey::admin(ws);

        // Golden vector computation
        let d1 = RequestDigest::compute(
            Kind::Msg,
            admin,
            &[r1, r2],
            RequestContent {
                subject: Some("Subject"),
                summary: None,
                body: Some("Body"),
            },
            None,
            None,
        )
        .unwrap();

        // Pinned exact literal v1 hash string constant
        let expected_literal_hash =
            "v1:cbb53d257583e9de5b11890d0a61ef3a9f139ae0ce10c0b289dba7b318782e05";
        assert_eq!(d1.as_str(), expected_literal_hash);
        assert_eq!(d1.as_str().len(), 67);

        // Recipient order invariance
        let d2 = RequestDigest::compute(
            Kind::Msg,
            admin,
            &[r2, r1],
            RequestContent {
                subject: Some("Subject"),
                summary: None,
                body: Some("Body"),
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(d1, d2);

        // One-field-change divergence tests
        let d_diff_subject = RequestDigest::compute(
            Kind::Msg,
            admin,
            &[r1, r2],
            RequestContent {
                subject: Some("Different Subject"),
                summary: None,
                body: Some("Body"),
            },
            None,
            None,
        )
        .unwrap();
        assert_ne!(d1, d_diff_subject);

        let d_diff_kind = RequestDigest::compute(
            Kind::Fyi,
            admin,
            &[r1, r2],
            RequestContent {
                subject: Some("Subject"),
                summary: None,
                body: Some("Body"),
            },
            None,
            None,
        )
        .unwrap();
        assert_ne!(d1, d_diff_kind);

        let reply_id = MessageId::new("m-reply_1").unwrap();
        let d_with_reply = RequestDigest::compute(
            Kind::Msg,
            admin,
            &[r1, r2],
            RequestContent {
                subject: Some("Subject"),
                summary: None,
                body: Some("Body"),
            },
            Some(&reply_id),
            None,
        )
        .unwrap();
        assert_ne!(d1, d_with_reply);

        let d_with_summary = RequestDigest::compute(
            Kind::Msg,
            admin,
            &[r1, r2],
            RequestContent {
                subject: Some("Subject"),
                summary: Some("First sentence. Second sentence."),
                body: Some("Body"),
            },
            None,
            None,
        )
        .unwrap();
        assert_ne!(d1, d_with_summary);

        // Serde deserialization validation
        let json = serde_json::to_string(&d1).unwrap();
        let deserialized: RequestDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(d1, deserialized);

        assert!(serde_json::from_str::<RequestDigest>(r#""invalid_digest""#).is_err());
        assert!(serde_json::from_str::<RequestDigest>(
            r#""v1:uppercaseHEX1234567890abcdef1234567890abcdef1234567890abcdef1234567890""#
        )
        .is_err());
    }

    #[test]
    fn message_summary_is_one_line_without_control_characters_and_no_cap() {
        assert!(validate_message_summary("Tests pass. The change is ready.").is_ok());
        assert!(validate_message_summary("One sentence only.").is_ok());
        assert!(validate_message_summary("no punctuation at all").is_ok());
        // Length is the sender's call: the pane shows one row and the CLI
        // warns, but the daemon accepts a long single line.
        let long = format!("{}. Done.", "a".repeat(400));
        assert!(validate_message_summary(&long).is_ok());

        for invalid in [
            "",
            "First sentence.\nSecond sentence.",
            " First sentence. Second sentence.",
            "First sentence. Second sentence. ",
            "tab\tinside",
        ] {
            assert_eq!(
                validate_message_summary(invalid),
                Err(MailboxTypeError::InvalidMessageSummary),
                "accepted invalid summary: {invalid:?}"
            );
        }
    }

    #[test]
    fn derived_summary_is_the_subject_and_only_then_the_body_first_line() {
        assert_eq!(
            derive_message_summary("Tests pass. The change is ready.", "Review the parser"),
            Some("Review the parser".to_string())
        );
        assert_eq!(
            derive_message_summary("line one\nline two", "  \t "),
            Some("line one".to_string())
        );
        assert_eq!(
            derive_message_summary("\n\n  spaced   words  ", ""),
            Some("spaced words".to_string())
        );
        assert_eq!(derive_message_summary("", "  \t "), None);
        let long = "x".repeat(400);
        let derived = derive_message_summary("", &long).expect("a long subject still derives");
        assert_eq!(derived.chars().count(), 200);
        assert_eq!(derive_message_summary("first\u{7}bell", ""), None);
    }
}
