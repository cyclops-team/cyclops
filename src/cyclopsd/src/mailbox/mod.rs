//! In-memory projection and query model for recipient mailboxes.
//!
//! Reconstructs deterministic mailbox states (pending and claimed entries)
//! from a single authoritative workspace journal.

pub(crate) mod directory;
pub(crate) mod projection;
pub(crate) mod service;
pub(crate) mod store;

#[cfg(test)]
mod tests;

pub use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
pub use std::path::Path;
pub use std::sync::{Arc, Mutex as StdMutex, RwLock};

pub use directory::*;
pub use projection::*;
pub use service::*;
pub use store::*;

// Re-export external types previously exposed in mailbox root for tests and sibling modules
pub use cyclops_ledger::{now_ms, LedgerError, LedgerWriter};
pub use cyclops_proto::{
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
pub use cyclops_state::StateRoot;

pub use tokio::sync::broadcast;
