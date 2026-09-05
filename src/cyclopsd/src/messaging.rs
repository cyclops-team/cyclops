//! Coordinates the durable mailbox with the existing pane notification worker.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_proto::{
    AgentState, AlarmClearResult, AlarmPreviewResult, AlarmSummary, ClaimDisposition,
    ComposerProof, ComposerSemantic, ComposerState, DeliveryReceipt, DeliveryState, HistoryParams,
    HistoryResult, InboxClaimResult, InboxListResult, InboxSummaryEntry, MessageId,
    MessageWakeBlock, MessagesFollowResult, MessagesSnapshotResult, MsgSendParams, MsgSendResult,
    NotificationAttemptId, NotificationAttentionCause, NotificationBinding, NotificationManifestId,
    NotificationPreWriteCause, NotificationPreWriteObservation, NotificationRecord,
    NotificationRouteEvidenceId, NotificationState, NotificationWithdrawDisposition,
    NotificationWithdrawResult, OpenDelivery, ProcessInstanceId, RecipientKey,
    StatusBlockedNotification, StatusMailboxRoute, ThreadResult,
};
use cyclops_tmux::PaneRow;
use tokio::time::Instant;
use tracing::{error, warn};

use crate::delivery;
use crate::mailbox::{
    AcceptResult, ClaimOutcome, MailboxDirectory, MailboxIdentity, MailboxSend, MailboxService,
    MailboxServiceError,
};
use crate::notification_adapter::{NotificationAdapterError, NotificationContext};
use crate::session_history::SessionHistorySources;

pub(crate) struct NotificationRoute {
    pub(crate) session_idx: usize,
    pub(crate) pane_id: String,
    pub(crate) label: String,
    pub(crate) row: PaneRow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScheduledHead {
    message_id: MessageId,
    attempt_id: NotificationAttemptId,
}

impl ScheduledHead {
    pub(crate) fn new(message_id: MessageId, attempt_id: NotificationAttemptId) -> Self {
        Self {
            message_id,
            attempt_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecipientScheduleOutcome {
    WorkerOwned {
        head: ScheduledHead,
        observe_first_disposition: bool,
    },
    NoWakeNeeded,
    Blocked {
        head: ScheduledHead,
        block: MessageWakeBlock,
    },
}

/// Build one receipt only from the exact current mailbox projection.
fn receipt_from_disposition(
    disposition: crate::mailbox::MessageDisposition,
    pane: Option<String>,
) -> DeliveryReceipt {
    DeliveryReceipt {
        to: disposition.label,
        state: DeliveryState::Queued,
        notification_state: Some(disposition.notification_state),
        quota_state: disposition.quota_state,
        notification_settlement: disposition.notification_settlement,
        pre_write_cause: disposition.pre_write_cause,
        wake_block: disposition.wake_block,
        position: disposition.position_ahead,
        held_by: None,
        note: None,
        pane,
    }
}

/// Preserve the durable acceptance result when the scheduler could not record
/// its own disposition. The message already exists and retrying an unkeyed send
/// would create a duplicate; the receipt therefore carries a fail-closed wake
/// diagnosis instead of converting acceptance into an RPC error.
fn receipt_with_schedule_truth(
    disposition: crate::mailbox::MessageDisposition,
    pane: Option<String>,
    scheduler_state_unavailable: bool,
) -> DeliveryReceipt {
    let mut receipt = receipt_from_disposition(disposition, pane);
    if scheduler_state_unavailable && receipt.wake_block.is_none() {
        receipt.wake_block = Some(MessageWakeBlock::SchedulerStateUnavailable);
    }
    receipt
}

#[derive(Debug, Default)]
struct AcceptanceSchedule {
    outcomes: HashMap<RecipientKey, RecipientScheduleOutcome>,
    unavailable: HashSet<RecipientKey>,
}

/// One immutable causal token proving that a pane route was freshly
/// observed.
///
/// Fusion and authenticated hook handling produce this evidence. They do not
/// decide which durable notification or attention work follows from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingRouteEvidence {
    pub(crate) session_idx: usize,
    pub(crate) pane_id: String,
    pub(crate) evidence_id: NotificationRouteEvidenceId,
}

/// Body-free result of one durable pre-write block transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MessagingPreWriteBlock {
    pub(crate) attempt_id: NotificationAttemptId,
    pub(crate) cause: NotificationPreWriteCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessagingPreWriteBlockOutcome {
    Recorded(MessagingPreWriteBlock),
    Obsolete,
}

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct MessagingPreWriteBlockError(#[from] NotificationAdapterError);

/// Current pane evidence supplied to the body-free messaging status operation.
///
/// The status adapter observes these facts. It does not join them to durable
/// notification records or decide recovery policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingComposerObservation {
    pub(crate) composer: cyclops_proto::ComposerState,
    pub(crate) proof: cyclops_proto::ComposerProof,
    pub(crate) reason: Option<String>,
    pub(crate) detected_attempt: Option<NotificationAttemptId>,
    pub(crate) detected_candidate_count: u32,
    pub(crate) pane_root: Option<ProcessInstanceId>,
    pub(crate) binding: Option<NotificationBinding>,
}

/// Finished body-free messaging decision copied onto one pane status row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingComposerStatus {
    pub(crate) composer: cyclops_proto::ComposerState,
    pub(crate) proof: cyclops_proto::ComposerProof,
    pub(crate) reason: Option<String>,
    pub(crate) candidate_count: u32,
    pub(crate) attempt: Option<NotificationAttemptId>,
    pub(crate) notification_state: Option<NotificationState>,
    pub(crate) message_state: Option<cyclops_proto::ComposerMessageState>,
    pub(crate) next_action: Option<cyclops_proto::ComposerNextAction>,
}

/// Current terminal binding class supplied to the composer projection.
///
/// Fusion proves the physical process facts. It does not compare them with a
/// durable notification record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingComposerPhysicalBinding {
    pub(crate) pane_root: ProcessInstanceId,
    pub(crate) leader: ProcessInstanceId,
    pub(crate) agent: ProcessInstanceId,
    pub(crate) manifest: NotificationManifestId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MessagingComposerBindingObservation {
    Bound(MessagingComposerPhysicalBinding),
    NotVendor,
    Gone,
    Unprovable,
}

/// Exact composer content evidence captured between stable process bookends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MessagingComposerCapture {
    NotRead,
    Visible(String),
    Hidden,
    Unprovable,
    BindingChanged,
}

/// Immutable terminal facts for one runtime composer projection.
///
/// The observation contains no journal record, mailbox entry, or recovery
/// variant. WorkspaceMessaging joins it to the opaque durable probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingRuntimeComposerObservation {
    pub(crate) semantic: Option<ComposerSemantic>,
    pub(crate) owner: Option<String>,
    pub(crate) in_mode: bool,
    pub(crate) detection_stale: bool,
    pub(crate) terminal_state_unsafe: bool,
    pub(crate) binding: MessagingComposerBindingObservation,
    pub(crate) recipient: Option<RecipientKey>,
    pub(crate) capture: MessagingComposerCapture,
}

/// Finished body-free composer ownership decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingRuntimeComposerProjection {
    pub(crate) state: ComposerState,
    pub(crate) proof: ComposerProof,
    pub(crate) notification_attempt: Option<NotificationAttemptId>,
    pub(crate) reason: Option<&'static str>,
    pub(crate) candidate_count: u32,
    /// True only when the supplied current binding exactly matches the one
    /// durably attached to the selected notification attempt.
    pub(crate) binding_verified: bool,
}

impl Default for MessagingRuntimeComposerProjection {
    fn default() -> Self {
        Self {
            state: ComposerState::ComposerAmbiguous,
            proof: ComposerProof::Unprovable,
            notification_attempt: None,
            reason: Some("composer_not_observed"),
            candidate_count: 0,
            binding_verified: false,
        }
    }
}

/// Opaque durable half of one runtime composer projection.
///
/// Fusion may ask whether an exact content capture is needed and later return
/// immutable terminal evidence. It cannot inspect candidate records, message
/// payloads, journal variants, or durable candidate cardinality.
pub(crate) struct MessagingComposerProjectionProbe {
    candidates: Vec<crate::mailbox::ActiveComposerNotification>,
    store_available: bool,
}

impl MessagingComposerProjectionProbe {
    pub(crate) fn none() -> Self {
        Self {
            candidates: Vec::new(),
            store_available: true,
        }
    }

    pub(crate) fn store_unavailable() -> Self {
        Self {
            candidates: Vec::new(),
            store_available: false,
        }
    }

    /// Exact content is needed only for an existing hold owner or a durable
    /// candidate. The durable count remains private.
    pub(crate) fn requires_capture(&self, owner_present: bool) -> bool {
        owner_present || !self.candidates.is_empty()
    }

    /// Join the immutable terminal observation to the durable candidate set.
    pub(crate) fn project(
        &self,
        observation: MessagingRuntimeComposerObservation,
    ) -> MessagingRuntimeComposerProjection {
        project_runtime_composer(&self.candidates, self.store_available, observation)
    }
}

fn semantic_runtime_composer_projection(
    semantic: Option<ComposerSemantic>,
) -> MessagingRuntimeComposerProjection {
    match semantic {
        Some(ComposerSemantic::Clean) => MessagingRuntimeComposerProjection {
            state: ComposerState::ComposerClean,
            proof: ComposerProof::ManifestRule,
            notification_attempt: None,
            reason: None,
            candidate_count: 0,
            binding_verified: false,
        },
        Some(ComposerSemantic::HumanInput) => MessagingRuntimeComposerProjection {
            state: ComposerState::HumanDraft,
            proof: ComposerProof::ManifestRule,
            notification_attempt: None,
            reason: None,
            candidate_count: 0,
            binding_verified: false,
        },
        Some(ComposerSemantic::GhostSuggestion) => MessagingRuntimeComposerProjection {
            state: ComposerState::VendorGhostSuggestion,
            proof: ComposerProof::ManifestRule,
            notification_attempt: None,
            reason: None,
            candidate_count: 0,
            binding_verified: false,
        },
        Some(ComposerSemantic::Ambiguous) => ambiguous_runtime_composer_projection(
            None,
            ComposerProof::Ambiguous,
            "manifest_rule_ambiguous",
            0,
        ),
        None => MessagingRuntimeComposerProjection::default(),
    }
}

fn ambiguous_runtime_composer_projection(
    attempt: Option<NotificationAttemptId>,
    proof: ComposerProof,
    reason: &'static str,
    candidate_count: usize,
) -> MessagingRuntimeComposerProjection {
    MessagingRuntimeComposerProjection {
        state: ComposerState::ComposerAmbiguous,
        proof,
        notification_attempt: attempt,
        reason: Some(reason),
        candidate_count: u32::try_from(candidate_count).unwrap_or(u32::MAX),
        binding_verified: false,
    }
}

fn notification_submission_recorded(record: &NotificationRecord) -> bool {
    match record.state {
        NotificationState::Submitted
        | NotificationState::SubmittedUnverified
        | NotificationState::Notified => true,
        NotificationState::AttentionRequired => matches!(
            record.cause,
            Some(
                NotificationAttentionCause::ReceiptOccupantChanged
                    | NotificationAttentionCause::AckTimeout
            )
        ),
        NotificationState::Queued
        | NotificationState::Gating
        | NotificationState::BlockedPreWrite
        | NotificationState::QuotaHeld
        | NotificationState::QuotaResetObserved
        | NotificationState::Writing
        | NotificationState::Staged
        | NotificationState::Submitting
        | NotificationState::Withdrawn
        | NotificationState::WithdrawnAfterStaging
        | NotificationState::WithdrawnByOperator
        | NotificationState::Superseded => false,
    }
}

fn project_runtime_composer(
    candidates: &[crate::mailbox::ActiveComposerNotification],
    store_available: bool,
    observation: MessagingRuntimeComposerObservation,
) -> MessagingRuntimeComposerProjection {
    let parsed_owner = observation
        .owner
        .as_deref()
        .and_then(|value| NotificationAttemptId::parse(value).ok());
    if !store_available {
        return ambiguous_runtime_composer_projection(
            parsed_owner,
            ComposerProof::Unprovable,
            "notification_store_unavailable",
            candidates.len(),
        );
    }
    if observation.in_mode {
        return ambiguous_runtime_composer_projection(
            parsed_owner,
            ComposerProof::Ambiguous,
            "pane_in_mode",
            candidates.len(),
        );
    }
    if observation.detection_stale {
        return ambiguous_runtime_composer_projection(
            parsed_owner,
            ComposerProof::Unprovable,
            "detection_stale",
            candidates.len(),
        );
    }
    if matches!(
        observation.capture,
        MessagingComposerCapture::BindingChanged
    ) {
        return ambiguous_runtime_composer_projection(
            parsed_owner,
            ComposerProof::Ambiguous,
            "binding_changed_during_capture",
            candidates.len(),
        );
    }
    if observation.owner.is_none() && candidates.is_empty() {
        return semantic_runtime_composer_projection(observation.semantic);
    }
    if observation.owner.is_some() && parsed_owner.is_none() && candidates.is_empty() {
        return ambiguous_runtime_composer_projection(
            None,
            ComposerProof::Unprovable,
            "direct_delivery_hold_unprovable",
            0,
        );
    }

    let attempt =
        match (parsed_owner, candidates) {
            (Some(attempt), [candidate]) if candidate.record.attempt_id == attempt => attempt,
            (None, [candidate]) => {
                let reason =
                    if candidate.record.binding.as_ref().is_none_or(|binding| {
                        binding.pane_root.is_none() || binding.leader.is_none()
                    }) {
                        "durable_binding_incomplete"
                    } else {
                        "notification_owner_missing"
                    };
                return ambiguous_runtime_composer_projection(
                    Some(candidate.record.attempt_id),
                    ComposerProof::Unprovable,
                    reason,
                    1,
                );
            }
            (Some(attempt), []) => {
                return ambiguous_runtime_composer_projection(
                    Some(attempt),
                    ComposerProof::Ambiguous,
                    "notification_attempt_mismatch",
                    0,
                );
            }
            (Some(attempt), [_]) => {
                return ambiguous_runtime_composer_projection(
                    Some(attempt),
                    ComposerProof::Ambiguous,
                    "notification_attempt_mismatch",
                    1,
                );
            }
            (owner, _) => {
                return ambiguous_runtime_composer_projection(
                    owner,
                    ComposerProof::Ambiguous,
                    "multiple_active_notifications",
                    candidates.len(),
                );
            }
        };
    let candidate = &candidates[0];

    if matches!(
        observation.binding,
        MessagingComposerBindingObservation::Unprovable
    ) {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "binding_unprovable",
            candidates.len(),
        );
    }
    if observation.terminal_state_unsafe
        || matches!(
            observation.binding,
            MessagingComposerBindingObservation::NotVendor
                | MessagingComposerBindingObservation::Gone
        )
    {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Ambiguous,
            "terminal_state_unsafe",
            candidates.len(),
        );
    }
    if observation.semantic.is_none() {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_semantic_unprovable",
            candidates.len(),
        );
    }
    let Some(current_recipient) = observation.recipient else {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "recipient_unprovable",
            candidates.len(),
        );
    };
    let Some(expected_binding) = candidate.record.binding.as_ref() else {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "durable_binding_incomplete",
            candidates.len(),
        );
    };
    let MessagingComposerBindingObservation::Bound(current_binding) = &observation.binding else {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "binding_unprovable",
            candidates.len(),
        );
    };
    if expected_binding.pane_root.is_none() || expected_binding.leader.is_none() {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "durable_binding_incomplete",
            candidates.len(),
        );
    }
    if expected_binding.recipient != candidate.record.recipient
        || current_recipient != candidate.record.recipient
        || expected_binding.pane_root != Some(current_binding.pane_root)
        || expected_binding.leader != Some(current_binding.leader)
        || expected_binding.agent != current_binding.agent
        || expected_binding.manifest != current_binding.manifest
    {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Ambiguous,
            "binding_mismatch",
            candidates.len(),
        );
    }

    let Some(expected) = candidate
        .message
        .as_ref()
        .and_then(|message| delivery::expected_notification_payload(&candidate.record, message))
    else {
        return ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "notification_payload_unprovable",
            candidates.len(),
        );
    };
    match &observation.capture {
        MessagingComposerCapture::Visible(actual)
            if delivery::visible_single_line_payload_matches(actual, &expected)
                && observation.semantic == Some(ComposerSemantic::HumanInput) =>
        {
            MessagingRuntimeComposerProjection {
                state: ComposerState::CyclopsNotificationStaged,
                proof: ComposerProof::ExactNotification,
                notification_attempt: Some(attempt),
                reason: None,
                candidate_count: 1,
                binding_verified: true,
            }
        }
        MessagingComposerCapture::Visible(actual)
            if observation.semantic == Some(ComposerSemantic::Clean)
                && actual.is_empty()
                && notification_submission_recorded(&candidate.record) =>
        {
            MessagingRuntimeComposerProjection {
                state: ComposerState::CyclopsNotificationSubmitted,
                proof: ComposerProof::ExactNotification,
                notification_attempt: Some(attempt),
                reason: None,
                candidate_count: 1,
                binding_verified: true,
            }
        }
        MessagingComposerCapture::Visible(_) => ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Ambiguous,
            "composer_content_mismatch",
            candidates.len(),
        ),
        MessagingComposerCapture::Hidden => ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_hidden",
            candidates.len(),
        ),
        MessagingComposerCapture::NotRead => ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_not_read",
            candidates.len(),
        ),
        MessagingComposerCapture::Unprovable => ambiguous_runtime_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_capture_unprovable",
            candidates.len(),
        ),
        MessagingComposerCapture::BindingChanged => {
            unreachable!("handled before candidate projection")
        }
    }
}

/// One immutable, body-free observation of the current terminal route.
///
/// The daemon adapter observes these runtime facts. `WorkspaceMessaging`
/// joins them to the durable notification head and decides whether the route
/// is eligible for foreground-watch diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingNotificationRouteObservation {
    pub(crate) pane_id: String,
    pub(crate) recipient_label: String,
    pub(crate) pane_pid: i32,
    pub(crate) agent_state: AgentState,
}

/// Body-free input for the foreground-watch process diagnostic.
///
/// Durable notification variants and route lookup stay inside
/// `WorkspaceMessaging`; the process adapter receives only the exact attempt
/// and pane facts it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingDeadlockCandidate {
    pub(crate) message_id: MessageId,
    pub(crate) notification_attempt: NotificationAttemptId,
    pub(crate) recipient: RecipientKey,
    pub(crate) recipient_label: String,
    pub(crate) pane_id: String,
    pub(crate) pane_pid: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessagingComposerCandidate {
    record: NotificationRecord,
    message_state: Option<cyclops_proto::ComposerMessageState>,
}

impl MessagingRouteEvidence {
    pub(crate) fn new(
        session_idx: usize,
        pane_id: impl Into<String>,
        evidence_id: NotificationRouteEvidenceId,
    ) -> Self {
        Self {
            session_idx,
            pane_id: pane_id.into(),
            evidence_id,
        }
    }
}

/// Stable operation failures for selecting or administering attention.
///
/// The socket adapter maps these outcomes to wire errors without inspecting
/// the mailbox projection or its lookup rules.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MessagingAttentionError {
    #[error("this operation requires the workspace administrator")]
    Denied,
    #[error(transparent)]
    Mailbox(#[from] MailboxServiceError),
}

/// Body-free durable messaging facts used while composing daemon status.
///
/// The status surface receives this projection instead of reading mailbox
/// variants, directory fallbacks, or notification indexes itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceMessagingStatus {
    pub(crate) mailbox_routes: Vec<StatusMailboxRoute>,
    pub(crate) admin_unread: u64,
    pub(crate) mailbox_attention: Vec<OpenDelivery>,
    pub(crate) blocked_notifications: Vec<StatusBlockedNotification>,
    pub(crate) blocked_notifications_total: u64,
    unread_by_recipient: HashMap<RecipientKey, u64>,
    projection_readable: bool,
    deadlock_candidates: Vec<MessagingDeadlockCandidate>,
    composer_candidates:
        Option<HashMap<RecipientKey, HashMap<NotificationAttemptId, MessagingComposerCandidate>>>,
}

impl WorkspaceMessagingStatus {
    pub(crate) fn unread_for(&self, recipient: RecipientKey) -> Option<u64> {
        self.projection_readable.then_some(
            self.unread_by_recipient
                .get(&recipient)
                .copied()
                .unwrap_or(0),
        )
    }

    pub(crate) fn deadlock_candidates(&self) -> &[MessagingDeadlockCandidate] {
        &self.deadlock_candidates
    }

    /// Join one immutable pane observation to the durable composer barriers.
    ///
    /// This is the only operation that interprets candidate cardinality,
    /// durable bindings, mailbox entry variants, recovery variants, or worker
    /// ownership for status presentation.
    pub(crate) fn composer_status(
        &self,
        recipient: Option<RecipientKey>,
        observation: MessagingComposerObservation,
    ) -> MessagingComposerStatus {
        let mut status = MessagingComposerStatus {
            composer: observation.composer,
            proof: observation.proof,
            reason: observation.reason,
            candidate_count: observation.detected_candidate_count,
            attempt: observation.detected_attempt,
            notification_state: None,
            message_state: None,
            next_action: None,
        };

        if let Some(candidates) =
            recipient.and_then(|recipient| self.composer_candidates.as_ref()?.get(&recipient))
        {
            status.candidate_count = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
            status.attempt = if candidates.len() == 1 {
                candidates.keys().next().copied()
            } else {
                None
            };
        }

        let Some(attempt_id) = status.attempt else {
            if status.candidate_count > 0 {
                status.next_action = Some(cyclops_proto::ComposerNextAction::InspectMessages);
            }
            return status;
        };

        let candidate = recipient.and_then(|recipient| {
            self.composer_candidates
                .as_ref()?
                .get(&recipient)?
                .get(&attempt_id)
        });
        let Some(candidate) = candidate else {
            // A retired or unreadable durable barrier cannot inherit a prior
            // exact status stamp. Status is evidence, not authority.
            status.composer = cyclops_proto::ComposerState::ComposerAmbiguous;
            status.proof = cyclops_proto::ComposerProof::Unprovable;
            status.next_action = Some(operator_composer_next_action(
                status.notification_state,
                true,
            ));
            return status;
        };

        status.notification_state = Some(candidate.record.state);
        status.message_state = candidate.message_state;
        let durable_binding_complete = candidate
            .record
            .binding
            .as_ref()
            .is_some_and(|binding| binding.pane_root.is_some() && binding.leader.is_some());
        let binding_unprovable = observation.pane_root.is_none()
            || observation.binding.is_none()
            || !durable_binding_complete;
        let binding_matches = recipient.is_some_and(|recipient| {
            candidate.record.recipient == recipient
                && observation.binding.as_ref().is_some_and(|binding| {
                    binding.pane_root == observation.pane_root
                        && candidate.record.binding.as_ref() == Some(binding)
                })
        });
        if matches!(
            status.composer,
            cyclops_proto::ComposerState::CyclopsNotificationStaged
                | cyclops_proto::ComposerState::CyclopsNotificationSubmitted
        ) && !binding_matches
        {
            status.composer = cyclops_proto::ComposerState::ComposerAmbiguous;
            if binding_unprovable {
                status.proof = cyclops_proto::ComposerProof::Unprovable;
                status.reason = Some("binding_unprovable".to_string());
            } else {
                status.proof = cyclops_proto::ComposerProof::Ambiguous;
                status.reason = Some("binding_mismatch".to_string());
            }
            status.next_action = Some(operator_composer_next_action(
                status.notification_state,
                true,
            ));
            return status;
        }

        status.next_action = Some(operator_composer_next_action(
            status.notification_state,
            true,
        ));
        status
    }
}

pub(crate) fn operator_composer_next_action(
    notification: Option<NotificationState>,
    has_exact_attempt: bool,
) -> cyclops_proto::ComposerNextAction {
    use cyclops_proto::ComposerNextAction;

    match notification {
        Some(NotificationState::AttentionRequired) if has_exact_attempt => {
            ComposerNextAction::InspectAttention
        }
        Some(
            NotificationState::Writing
            | NotificationState::Staged
            | NotificationState::Submitting
            | NotificationState::Submitted
            | NotificationState::Notified
            | NotificationState::WithdrawnAfterStaging,
        ) if has_exact_attempt => ComposerNextAction::CheckHealth,
        _ => ComposerNextAction::InspectMessages,
    }
}

/// Narrow post-commit capabilities needed by durable message acceptance.
///
/// `WorkspaceMessaging` receives this Interface from the daemon composition
/// root and cannot traverse daemon state. These named capabilities are the only
/// bridge from accepted durable facts to notification scheduling, unread
/// invalidation, message-change observation, and pane receipt metadata.
pub(crate) trait WorkspaceMessagingEffects: Send + Sync {
    fn subscribe_messages_changed(&self) -> tokio::sync::broadcast::Receiver<cyclops_proto::Event>;

    fn schedule_notification(
        &self,
        service: &Arc<MailboxService>,
        recipient: RecipientKey,
    ) -> Result<RecipientScheduleOutcome, MailboxServiceError>;

    fn invalidate_unread(&self, recipient: RecipientKey);

    fn notification_route(
        &self,
        service: &MailboxService,
        recipient: RecipientKey,
    ) -> Result<Option<NotificationRoute>, MailboxServiceError>;

    fn notification_route_observation(
        &self,
        service: &MailboxService,
        recipient: RecipientKey,
    ) -> Result<Option<MessagingNotificationRouteObservation>, MailboxServiceError>;

    fn settle_notification_claim(&self, attempt_id: NotificationAttemptId);

    fn cancel_notification(&self, attempt_id: NotificationAttemptId);

    fn reconcile_route_evidence(&self, evidence: MessagingRouteEvidence);

    fn reconcile_current_route(&self, session_idx: usize, pane_id: String);

    fn receipt_block(&self) -> Duration;
}

/// Internal messaging Module for one workspace.
///
/// The module owns durable send/reply acceptance; inbox, message, alarm,
/// attention-selection, and status reads; claims, requeue, exact pre-write
/// withdrawal; and the post-commit actions that follow those mutations.
/// Callers supply an authenticated identity and request; they do not receive
/// the journal, projection, publication lock, worker, unread scheduler, or
/// daemon composition root.
pub(crate) struct WorkspaceMessaging {
    service: Arc<MailboxService>,
    publication: Arc<StdMutex<()>>,
    effects: Arc<dyn WorkspaceMessagingEffects>,
}

impl WorkspaceMessaging {
    pub(crate) fn new(
        service: Arc<MailboxService>,
        publication: Arc<StdMutex<()>>,
        effects: Arc<dyn WorkspaceMessagingEffects>,
    ) -> Self {
        Self {
            service,
            publication,
            effects,
        }
    }

    /// Capture the durable candidates needed to classify one runtime
    /// composer without exposing their records or message bodies.
    pub(crate) fn composer_projection_probe(
        &self,
        recipient: Option<RecipientKey>,
    ) -> MessagingComposerProjectionProbe {
        let Some(recipient) = recipient else {
            return MessagingComposerProjectionProbe::none();
        };
        match self.service.active_composer_notifications(recipient) {
            Ok(candidates) => MessagingComposerProjectionProbe {
                candidates,
                store_available: true,
            },
            Err(_) => MessagingComposerProjectionProbe::store_unavailable(),
        }
    }

    /// Read the current directory and its matching daemon route publication as
    /// one transaction without exposing the synchronization mechanism.
    pub(crate) fn with_published<T>(&self, read: impl FnOnce(&Self) -> T) -> T {
        let _publication = self.publication.lock().expect("mailbox publication lock");
        read(self)
    }

    /// Read the authorized workspace-first history projection.
    ///
    /// The caller supplies an authenticated durable identity and the immutable
    /// sources returned by `SessionHistoryAdapter`. Current journal
    /// access, collision rules, visibility, and body release remain inside
    /// this module. The publication lock is deliberately not held across replay.
    pub(crate) fn history(
        &self,
        caller: MailboxIdentity,
        params: HistoryParams,
        cursor2: Option<String>,
        compatibility: SessionHistorySources,
    ) -> Result<HistoryResult, cyclops_proto::WireError> {
        let reader = crate::history::HistoryReader::workspace(caller.label.clone(), caller.key);
        let record = self.history_record(compatibility);
        let mut result = crate::history::query_history(&record, params, cursor2, Some(&reader))?;
        crate::mailbox::redact_message_bodies(
            Some(&self.service),
            Some(caller.key),
            &mut result.lines,
        );
        Ok(result)
    }

    /// Read one authorized workspace-first message thread.
    pub(crate) fn thread(
        &self,
        caller: MailboxIdentity,
        id: &str,
        reveal_body: bool,
        compatibility: SessionHistorySources,
    ) -> Result<ThreadResult, cyclops_proto::WireError> {
        let reader = crate::history::HistoryReader::workspace(caller.label.clone(), caller.key);
        let record = self.history_record(compatibility);
        let mut result = crate::history::query_thread(&record, id, Some(&reader))?;
        if !(caller.key.is_admin() && reveal_body) {
            crate::mailbox::redact_message_bodies(
                Some(&self.service),
                Some(caller.key),
                &mut result.lines,
            );
        }
        Ok(result)
    }

    /// Fold open direct-delivery records with current-workspace collision rules
    /// without exposing workspace message IDs to the status caller.
    pub(crate) fn retained_open_deliveries(
        &self,
        compatibility: SessionHistorySources,
    ) -> Vec<OpenDelivery> {
        let record = self.history_record(compatibility);
        let mut open = crate::history::open_from_record(&record);
        if !record.has_workspace_source() {
            let workspace_ids = match self.service.workspace_message_ids() {
                Ok(ids) => ids,
                Err(error) => {
                    warn!(error = %error, "open delivery ownership is unreadable");
                    return Vec::new();
                }
            };
            open.retain(|delivery| !workspace_ids.contains(&delivery.id));
        }
        open
    }

    fn history_record(
        &self,
        compatibility: SessionHistorySources,
    ) -> crate::history::HistoryRecord {
        let workspace = match self.service.journal_lines() {
            Ok(lines) if !lines.is_empty() => {
                Some((format!("workspace:{}", self.service.workspace_id()), lines))
            }
            Ok(_) => None,
            Err(error) => {
                warn!(error = %error, "workspace message history is unreadable");
                None
            }
        };
        crate::history::HistoryRecord::new(workspace, compatibility)
    }

    /// Publish the complete current participant directory.
    ///
    /// The daemon supplies observed physical identities while this Module owns
    /// the durable projection replacement. Callers use [`Self::with_published`]
    /// when route state and authenticated reads must move atomically with the
    /// replacement.
    pub(crate) fn replace_directory(
        &self,
        directory: MailboxDirectory,
    ) -> Result<(), MailboxServiceError> {
        self.service.replace_directory(directory)
    }

    pub(crate) fn identity_for_address(
        &self,
        address: &str,
    ) -> Result<MailboxIdentity, MailboxServiceError> {
        self.service.identity_for_address(address)
    }

    pub(crate) fn admin_identity(&self) -> MailboxIdentity {
        self.service.admin()
    }

    pub(crate) fn notification_for_message(
        &self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> Result<Option<NotificationRecord>, MailboxServiceError> {
        self.service.notification_for_message(recipient, message_id)
    }

    pub(crate) fn identity_for_recipient(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<MailboxIdentity>, MailboxServiceError> {
        self.service.identity_for_recipient(recipient)
    }

    pub(crate) fn inbox_list(
        &self,
        caller: RecipientKey,
        sender: Option<RecipientKey>,
        limit: Option<u32>,
    ) -> Result<InboxListResult, MailboxServiceError> {
        let entries = self
            .service
            .list(caller, sender, limit)?
            .into_iter()
            .map(|item| InboxSummaryEntry {
                message_id: item.entry.message_id,
                sender: Some(item.sender),
                sender_label: item.sender_label,
                subject: item.subject,
                ts: item.entry.created_at,
                thread_root: item.thread_root,
            })
            .collect();
        Ok(InboxListResult { entries })
    }

    pub(crate) fn claim(
        &self,
        claimant: RecipientKey,
        message_id: MessageId,
    ) -> Result<InboxClaimResult, MailboxServiceError> {
        // Only this operation interprets the reserved locator. Every other
        // message-id consumer keeps treating the same bytes as a literal
        // historical id.
        let outcome = match cyclops_proto::parse_notification_attempt_claim_locator(&message_id) {
            Some(attempt_id) => self
                .service
                .claim_notification_locator(claimant, message_id, attempt_id)?,
            None => self.service.claim(claimant, message_id)?,
        };
        self.finish_claim(claimant, outcome)
    }

    fn finish_claim(
        &self,
        claimant: RecipientKey,
        outcome: ClaimOutcome,
    ) -> Result<InboxClaimResult, MailboxServiceError> {
        let (withdrawn, consumed_doorbell) = match &outcome {
            ClaimOutcome::Claimed {
                withdrawn_attempt,
                consumed_doorbell_attempt,
                ..
            }
            | ClaimOutcome::AlreadyClaimed {
                withdrawn_attempt,
                consumed_doorbell_attempt,
                ..
            } => (*withdrawn_attempt, *consumed_doorbell_attempt),
        };
        if let Some(attempt_id) = consumed_doorbell {
            self.effects.settle_notification_claim(attempt_id);
        }
        if let Some(attempt_id) = withdrawn {
            self.effects.cancel_notification(attempt_id);
        }
        if let Err(error) = self.effects.schedule_notification(&self.service, claimant) {
            error!(%claimant, %error, "cannot schedule mailbox notification after claim");
        }
        self.effects.invalidate_unread(claimant);

        Ok(match outcome {
            ClaimOutcome::Claimed {
                message,
                skipped_oldest,
                ..
            } => InboxClaimResult {
                disposition: ClaimDisposition::Claimed,
                message,
                skipped_oldest,
            },
            ClaimOutcome::AlreadyClaimed { message, .. } => InboxClaimResult {
                disposition: ClaimDisposition::AlreadyClaimed,
                message,
                skipped_oldest: None,
            },
        })
    }

    pub(crate) fn messages_snapshot(
        &self,
        caller: RecipientKey,
        recent_settled: u32,
    ) -> Result<MessagesSnapshotResult, MailboxServiceError> {
        self.service.messages_snapshot(caller, recent_settled)
    }

    pub(crate) fn messages_follow(
        &self,
        caller: RecipientKey,
        after_seq: u64,
        limit: u32,
    ) -> Result<MessagesFollowResult, MailboxServiceError> {
        self.service.messages_follow(caller, after_seq, limit)
    }

    pub(crate) fn requeue(&self, message_id: MessageId) -> Result<bool, MailboxServiceError> {
        let records = self.service.requeue_message(message_id)?;
        let recipients: HashSet<_> = records.iter().map(|record| record.recipient).collect();
        for recipient in recipients {
            if let Err(error) = self.effects.schedule_notification(&self.service, recipient) {
                error!(%recipient, %error, "cannot schedule requeued mailbox notification");
            }
            self.effects.invalidate_unread(recipient);
        }
        Ok(!records.is_empty())
    }

    /// Withdraw one exact unwritten wake and advance the recipient FIFO.
    pub(crate) fn withdraw_notification(
        &self,
        operator: RecipientKey,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<NotificationWithdrawResult, MailboxServiceError> {
        let (record, inserted) = self.with_published(|messaging| {
            messaging
                .service
                .withdraw_notification_before_write(operator, recipient, attempt_id)
        })?;
        self.effects.cancel_notification(attempt_id);
        if let Err(error) = self.effects.schedule_notification(&self.service, recipient) {
            error!(%recipient, %error, "cannot schedule mailbox notification after withdrawal");
        }
        self.effects.invalidate_unread(recipient);
        Ok(NotificationWithdrawResult {
            attempt_id,
            message_id: record.message_id,
            recipient,
            disposition: if inserted {
                NotificationWithdrawDisposition::Withdrawn
            } else {
                NotificationWithdrawDisposition::AlreadyWithdrawn
            },
        })
    }

    /// Apply one immutable route observation without exposing reconciliation
    /// or worker topology to the observer.
    pub(crate) fn route_evidence_observed(&self, evidence: MessagingRouteEvidence) {
        self.effects.reconcile_route_evidence(evidence);
    }

    /// Resume every durable FIFO that may now have a route.
    ///
    /// Directory and lifecycle callers report availability; this Module owns
    /// the pending-recipient projection and post-commit scheduling choice.
    pub(crate) fn availability_changed(&self) {
        let recipients = match self.service.pending_recipients() {
            Ok(recipients) => recipients,
            Err(error) => {
                error!(%error, "cannot inspect pending mailbox notifications");
                return;
            }
        };
        for recipient in recipients {
            if let Err(error) = self.effects.schedule_notification(&self.service, recipient) {
                error!(%recipient, %error, "cannot schedule mailbox notification");
            }
        }
    }

    /// Persist one known pre-write refusal and publish its route consequence
    /// without exposing the journal lock or terminal-state variants.
    pub(crate) fn record_notification_prewrite_block(
        &self,
        notification: &NotificationContext,
        cause: NotificationPreWriteCause,
        observation: Option<NotificationPreWriteObservation>,
        route_evidence: NotificationRouteEvidenceId,
        session_idx: usize,
        pane_id: impl Into<String>,
    ) -> Result<MessagingPreWriteBlockOutcome, MessagingPreWriteBlockError> {
        let observation =
            if cause == NotificationPreWriteCause::WriteReadinessChanged && observation.is_none() {
                // A readiness race can lack binding or width evidence, but it still
                // needs the route generation under which the write was refused.
                Some(NotificationPreWriteObservation {
                    write_block: None,
                    pane_root: None,
                    selected_manifest: None,
                    binding: None,
                    route_evidence: Some(route_evidence),
                    pane_width: None,
                    required_pane_width: None,
                })
            } else {
                observation
            };
        let outcome = self.record_prewrite_transition(notification, cause, observation, false)?;
        if matches!(outcome, MessagingPreWriteBlockOutcome::Recorded(_)) {
            self.notification_prewrite_blocked(session_idx, pane_id);
        }
        Ok(outcome)
    }

    /// Convert an exhausted notification supervisor into one durable
    /// pre-write block before its worker releases FIFO ownership.
    pub(crate) fn record_worker_failed_prewrite(
        &self,
        notification: &NotificationContext,
    ) -> Result<MessagingPreWriteBlockOutcome, MessagingPreWriteBlockError> {
        self.record_prewrite_transition(
            notification,
            NotificationPreWriteCause::WorkerFailed,
            None,
            true,
        )
    }

    fn record_prewrite_transition(
        &self,
        notification: &NotificationContext,
        cause: NotificationPreWriteCause,
        observation: Option<NotificationPreWriteObservation>,
        ensure_gating: bool,
    ) -> Result<MessagingPreWriteBlockOutcome, MessagingPreWriteBlockError> {
        let recorded = {
            let _publication = self.publication.lock().expect("mailbox publication lock");
            (|| {
                if ensure_gating {
                    notification.record_gating()?;
                }
                let wake_block = (cause == NotificationPreWriteCause::ComposerOwnershipUnproven)
                    .then_some(MessageWakeBlock::ComposerOwnershipUnproven);
                notification.record_pre_write_block_with_wake_block(cause, observation, wake_block)
            })()
        };
        match recorded {
            Ok(record) => Ok(MessagingPreWriteBlockOutcome::Recorded(
                MessagingPreWriteBlock {
                    attempt_id: record.attempt_id,
                    cause: record.pre_write_cause.unwrap_or(cause),
                },
            )),
            Err(NotificationAdapterError::NoLongerCurrentBeforeWrite) => {
                Ok(MessagingPreWriteBlockOutcome::Obsolete)
            }
            Err(NotificationAdapterError::TerminalConflict(
                NotificationState::Withdrawn
                | NotificationState::WithdrawnByOperator
                | NotificationState::Superseded,
            )) => Ok(MessagingPreWriteBlockOutcome::Obsolete),
            Err(error) => Err(error.into()),
        }
    }

    /// Reconsider the current route after a durable pre-write block commits.
    ///
    /// This uses the already-minted route generation and never invents a new
    /// observation edge.
    pub(crate) fn notification_prewrite_blocked(
        &self,
        session_idx: usize,
        pane_id: impl Into<String>,
    ) {
        self.effects
            .reconcile_current_route(session_idx, pane_id.into());
    }

    pub(crate) fn alarm_preview(
        &self,
        caller: RecipientKey,
        older_than_ms: u64,
        observed_at_ms: u64,
    ) -> Result<AlarmPreviewResult, MessagingAttentionError> {
        self.require_admin(caller)?;
        let cutoff_ms = observed_at_ms.saturating_sub(older_than_ms);
        let entries = self
            .service
            .alarms_at_or_before(cutoff_ms)?
            .iter()
            .map(alarm_summary)
            .collect();
        Ok(AlarmPreviewResult { entries, cutoff_ms })
    }

    pub(crate) fn clear_alarms(
        &self,
        caller: RecipientKey,
        attempts: &[NotificationAttemptId],
        cutoff_ms: Option<u64>,
    ) -> Result<AlarmClearResult, MessagingAttentionError> {
        self.require_admin(caller)?;
        let summaries = self.service.clear_alarms(caller, attempts, cutoff_ms)?;
        Ok(AlarmClearResult {
            cleared_ids: summaries
                .iter()
                .map(|record| record.attempt_id.to_string())
                .collect(),
            summaries: summaries.iter().map(alarm_summary).collect(),
        })
    }

    /// Build the coherent body-free mailbox half of daemon status.
    pub(crate) fn status_snapshot(
        &self,
        include_attention: bool,
        observed_at_ms: u64,
        blocked_limit: usize,
    ) -> WorkspaceMessagingStatus {
        self.with_published(|messaging| {
            let service = &messaging.service;
            let admin = service.admin().key;
            let admin_unread = service.pending_count(admin);
            let projection_readable = admin_unread.is_ok();
            let admin_unread = admin_unread.unwrap_or(0) as u64;
            let mut unread_by_recipient = HashMap::new();
            if projection_readable {
                unread_by_recipient.insert(admin, admin_unread);
            }

            let mut mailbox_routes: Vec<StatusMailboxRoute> = service
                .routes()
                .ok()
                .map(|routes| {
                    routes
                        .into_iter()
                        .map(|identity| {
                            let unread = service
                                .pending_count(identity.key)
                                .ok()
                                .map(|count| count as u64);
                            if let Some(unread) = unread {
                                unread_by_recipient.insert(identity.key, unread);
                            }
                            StatusMailboxRoute {
                                recipient: identity.key,
                                label: identity.label,
                                unread,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            if let Ok(pending) = service.pending_recipients() {
                for key in pending {
                    let unread = service.pending_count(key).ok().map(|count| count as u64);
                    if let Some(unread) = unread {
                        unread_by_recipient.insert(key, unread);
                    }
                    if !mailbox_routes.iter().any(|route| route.recipient == key)
                        && unread.unwrap_or(0) > 0
                    {
                        let label = service
                            .recipient_label(key)
                            .ok()
                            .flatten()
                            .or_else(|| {
                                service
                                    .identity_for_recipient(key)
                                    .ok()
                                    .flatten()
                                    .map(|identity| identity.label)
                            })
                            .unwrap_or_else(|| key.to_string());
                        mailbox_routes.push(StatusMailboxRoute {
                            recipient: key,
                            label,
                            unread,
                        });
                    }
                }
            }

            let mailbox_attention = if include_attention {
                service.mailbox_attention_rows().unwrap_or_default()
            } else {
                Vec::new()
            };
            let blocked = service
                .blocked_notification_snapshot(observed_at_ms, blocked_limit)
                .unwrap_or_default();
            let deadlock_candidates = match service.gating_notifications() {
                Ok(records) => records
                    .into_iter()
                    .filter_map(|record| {
                        let route = match messaging
                            .effects
                            .notification_route_observation(service, record.recipient)
                        {
                            Ok(Some(route)) => route,
                            Ok(None) => return None,
                            Err(error) => {
                                error!(
                                    %error,
                                    "messaging status could not observe a notification route"
                                );
                                return None;
                            }
                        };
                        (route.agent_state == AgentState::Working).then_some(
                            MessagingDeadlockCandidate {
                                message_id: record.message_id,
                                notification_attempt: record.attempt_id,
                                recipient: record.recipient,
                                recipient_label: route.recipient_label,
                                pane_id: route.pane_id,
                                pane_pid: route.pane_pid,
                            },
                        )
                    })
                    .collect(),
                Err(error) => {
                    error!(%error, "messaging status could not read gating notifications");
                    Vec::new()
                }
            };
            let composer_candidates =
                service
                    .active_composer_notifications_snapshot()
                    .ok()
                    .map(|candidates| {
                        let mut grouped = HashMap::new();
                        for candidate in candidates {
                            grouped
                                .entry(candidate.record.recipient)
                                .or_insert_with(HashMap::new)
                                .insert(
                                    candidate.record.attempt_id,
                                    MessagingComposerCandidate {
                                        record: candidate.record,
                                        message_state: candidate
                                            .entry_state
                                            .as_ref()
                                            .map(cyclops_proto::ComposerMessageState::from),
                                    },
                                );
                        }
                        grouped
                    });

            WorkspaceMessagingStatus {
                mailbox_routes,
                admin_unread,
                mailbox_attention,
                blocked_notifications: blocked.rows,
                blocked_notifications_total: blocked.total,
                unread_by_recipient,
                projection_readable,
                deadlock_candidates,
                composer_candidates,
            }
        })
    }

    fn require_admin(&self, caller: RecipientKey) -> Result<(), MessagingAttentionError> {
        if caller.is_admin() {
            Ok(())
        } else {
            Err(MessagingAttentionError::Denied)
        }
    }

    async fn finish_acceptance(
        &self,
        accepted: AcceptResult,
        require_wake: bool,
    ) -> Result<MsgSendResult, MailboxServiceError> {
        // Subscribe before scheduling. A worker may commit its first
        // disposition before enqueue returns; the immediate projection read
        // below remains the authority, and this receiver prevents losing a
        // later commit.
        let events = self.effects.subscribe_messages_changed();
        let schedule = schedule_accepted_notifications(&accepted, |recipient| {
            self.effects.schedule_notification(&self.service, recipient)
        });
        // The journal append is the acceptance boundary. Pane chrome is a
        // best-effort projection of that truth and must never hold the response
        // behind a slow tmux server. One daemon-owned worker coalesces
        // dirtiness per recipient and re-derives the current durable count.
        for recipient in accepted.recipient_keys.iter().copied() {
            self.effects.invalidate_unread(recipient);
        }
        let deadline = Instant::now() + self.effects.receipt_block();
        let dispositions = observe_first_durable_dispositions(
            &self.service,
            &accepted.message_id,
            &schedule.outcomes,
            events,
            deadline,
            require_wake,
        )
        .await?;
        Ok(MsgSendResult {
            msg_id: accepted.message_id.to_string(),
            seq: accepted.seq,
            deliveries: dispositions
                .into_iter()
                .map(|disposition| {
                    let recipient = disposition.recipient;
                    let pane = self
                        .effects
                        .notification_route(&self.service, disposition.recipient)?
                        .map(|route| route.pane_id);
                    Ok(receipt_with_schedule_truth(
                        disposition,
                        pane,
                        schedule.unavailable.contains(&recipient),
                    ))
                })
                .collect::<Result<Vec<_>, MailboxServiceError>>()?,
            inserted: Some(accepted.inserted),
        })
    }

    pub(crate) async fn send(
        &self,
        sender: MailboxIdentity,
        params: MsgSendParams,
    ) -> Result<MsgSendResult, MailboxServiceError> {
        let require_wake = params.require_wake;
        if params.reply_to.is_some() && (!params.to.is_empty() || params.recipient_keys.is_some()) {
            return Err(crate::mailbox::MailboxDirectoryError::ReplyRecipientSelectors.into());
        }
        if params.recipient_keys.is_some() && !params.to.is_empty() {
            return Err(crate::mailbox::MailboxDirectoryError::MixedRecipientSelectors.into());
        }
        let accepted = match params.reply_to {
            Some(reference) => self.service.reply_with_summary(
                sender,
                MessageId::new(reference)
                    .map_err(crate::mailbox::MailboxError::from)
                    .map_err(crate::mailbox::MessageStoreError::from)
                    .map_err(MailboxServiceError::from)?,
                params.summary,
                params.body,
                params.client_key,
                params.raw,
            )?,
            None => self.service.send(
                sender,
                MailboxSend {
                    addresses: params.to,
                    recipient_keys: params.recipient_keys,
                    subject: params.subject,
                    summary: params.summary,
                    body: params.body,
                    fyi: params.fyi,
                    client_key: params.client_key,
                    supersedes: params.supersedes,
                    raw: params.raw,
                },
            )?,
        };
        self.finish_acceptance(accepted, require_wake).await
    }

    pub(crate) async fn reply(
        &self,
        sender: MailboxIdentity,
        reference: MessageId,
        summary: Option<String>,
        body: String,
        client_key: Option<String>,
        raw: bool,
    ) -> Result<MsgSendResult, MailboxServiceError> {
        let accepted = self
            .service
            .reply_with_summary(sender, reference, summary, body, client_key, raw)?;
        self.finish_acceptance(accepted, false).await
    }

    /// Apply one committed pane observation to durable messaging truth.
    ///
    /// This operation never captures a pane or resolves a live route. It owns
    /// the post-commit consequences justified by supplied evidence.
    pub(crate) fn apply_observation(&self, observation: crate::fusion::PaneMessagingObservation) {
        match observation {
            crate::fusion::PaneMessagingObservation::RouteEvidenceObserved { evidence } => {
                self.route_evidence_observed(evidence);
            }
        }
    }
}

fn alarm_summary(record: &NotificationRecord) -> AlarmSummary {
    AlarmSummary {
        id: record.attempt_id.to_string(),
        message_id: record.message_id.to_string(),
        recipient: record.recipient.to_string(),
        state: DeliveryState::AttentionRequired,
        // An attention record always carries a cause. If one ever reaches
        // here without it, report an unknown outcome instead of inventing one.
        cause: record
            .cause
            .unwrap_or(NotificationAttentionCause::TransportOutcomeUnknown),
        ts: record.updated_at,
    }
}

fn has_first_durable_disposition(
    disposition: &crate::mailbox::MessageDisposition,
    head: &ScheduledHead,
    require_wake: bool,
) -> bool {
    if disposition.attempt_id != Some(head.attempt_id) {
        return true;
    }
    if require_wake {
        !matches!(
            disposition.notification_state_raw,
            Some(
                NotificationState::Queued
                    | NotificationState::Gating
                    | NotificationState::Writing
                    | NotificationState::Staged
                    | NotificationState::Submitting
            )
        )
    } else {
        !matches!(
            disposition.notification_state_raw,
            Some(NotificationState::Queued | NotificationState::Gating)
        )
    }
}

async fn wait_for_messages_change(
    events: &mut tokio::sync::broadcast::Receiver<cyclops_proto::Event>,
    deadline: Instant,
) -> bool {
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(event)) if event.event == "messages.changed" => return true,
            Ok(Ok(_)) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => return true,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => return false,
        }
    }
}

async fn observe_first_durable_dispositions(
    service: &MailboxService,
    message_id: &MessageId,
    outcomes: &HashMap<RecipientKey, RecipientScheduleOutcome>,
    events: tokio::sync::broadcast::Receiver<cyclops_proto::Event>,
    deadline: Instant,
    require_wake: bool,
) -> Result<Vec<crate::mailbox::MessageDisposition>, MailboxServiceError> {
    observe_first_durable_dispositions_with(
        message_id,
        outcomes,
        events,
        deadline,
        require_wake,
        || service.message_dispositions(message_id),
        Instant::now,
    )
    .await
}

async fn observe_first_durable_dispositions_with(
    message_id: &MessageId,
    outcomes: &HashMap<RecipientKey, RecipientScheduleOutcome>,
    mut events: tokio::sync::broadcast::Receiver<cyclops_proto::Event>,
    deadline: Instant,
    require_wake: bool,
    mut read_projection: impl FnMut() -> Result<
        Vec<crate::mailbox::MessageDisposition>,
        MailboxServiceError,
    >,
    mut now: impl FnMut() -> Instant,
) -> Result<Vec<crate::mailbox::MessageDisposition>, MailboxServiceError> {
    let mut pending: HashMap<RecipientKey, ScheduledHead> = outcomes
        .iter()
        .filter_map(|(recipient, outcome)| match outcome {
            RecipientScheduleOutcome::WorkerOwned {
                head,
                observe_first_disposition: true,
            } if head.message_id == *message_id => Some((*recipient, head.clone())),
            _ => None,
        })
        .collect();

    loop {
        let dispositions = read_projection()?;
        pending.retain(|recipient, head| {
            dispositions
                .iter()
                .find(|disposition| disposition.recipient == *recipient)
                .is_none_or(|disposition| {
                    !has_first_durable_disposition(disposition, head, require_wake)
                })
        });
        if pending.is_empty() {
            return Ok(dispositions);
        }
        if now() >= deadline {
            return read_projection();
        }

        if !wait_for_messages_change(&mut events, deadline).await {
            // The deadline is only a response bound. It never records a
            // delivery decision. Take one final authoritative projection
            // snapshot so a fact committed at the boundary is not lost.
            return read_projection();
        }
    }
}

/// Attempt every broadcast recipient without revoking durable acceptance.
///
/// A scheduling error occurs after the message append has been synced. Keep
/// attempting the other recipients and return the affected recipient in the
/// unavailable set so the accepted receipt fails closed without inviting an
/// unkeyed retry and duplicate message.
fn schedule_accepted_notifications(
    accepted: &AcceptResult,
    mut schedule: impl FnMut(RecipientKey) -> Result<RecipientScheduleOutcome, MailboxServiceError>,
) -> AcceptanceSchedule {
    let mut report = AcceptanceSchedule::default();
    for recipient in accepted.recipient_keys.iter().copied() {
        match schedule(recipient) {
            Ok(outcome) => {
                report.outcomes.insert(recipient, outcome);
            }
            Err(error) => {
                error!(%recipient, %error, "cannot schedule accepted mailbox notification");
                report.unavailable.insert(recipient);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use crate::fusion::PaneMessagingObservation;
    use crate::messaging_runtime::record_unowned_notification;

    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use cyclops_proto::{
        scratch::scratch_dir, Event, NotificationAttentionCause, NotificationTransport,
        SessionInstanceId, TmuxPaneId, WorkspaceId, DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM,
    };
    use cyclops_state::StateRoot;
    use tokio::sync::broadcast;

    use crate::mailbox::{MailboxDirectory, MessageStore};

    /// Return the source prefix before the file's primary test module.
    ///
    /// Some source files contain small `#[cfg(test)]` helpers before later
    /// production items. Stopping at the first test attribute would hide that
    /// production from an architecture lint. The primary test module is last
    /// in every file passed here, so this prefix keeps the later production
    /// region in those audited files visible.
    fn source_before_primary_tests<'a>(source: &'a str, file: &str) -> &'a str {
        let boundary = ["#[cfg(test)]", "mod tests"].join("\n");
        source
            .split_once(&boundary)
            .unwrap_or_else(|| panic!("{file} primary test boundary"))
            .0
    }

    fn delivery_production_source() -> String {
        format!(
            "{}{}{}{}{}",
            include_str!("delivery/worker.rs"),
            include_str!("delivery/gate.rs"),
            include_str!("delivery/inject.rs"),
            include_str!("delivery/terminal.rs"),
            source_before_primary_tests(include_str!("delivery/mod.rs"), "delivery.rs"),
        )
    }

    /// Obsolete when these lints select Rust items structurally instead of
    /// finding the production prefix with a textual module boundary.
    #[test]
    #[should_panic(expected = "simulated source recovered forbidden_dependency")]
    fn source_boundary_lint_rejects_a_forbidden_reference_after_an_early_test_item() {
        let source = r#"
fn before() {}
#[cfg(test)]
use crate::test_support;
fn later_production() { forbidden_dependency(); }
#[cfg(test)]
mod tests {}
"#;
        let production = source_before_primary_tests(source, "simulated source");

        assert!(
            !production.contains("forbidden_dependency"),
            "simulated source recovered forbidden_dependency"
        );
    }

    /// Syntactic architecture lint: the durable operation Module may request
    /// named effects, but its construction and daemon-root adapter belong to
    /// the composition root.
    #[test]
    fn workspace_messaging_core_cannot_recover_the_daemon_root() {
        let source = include_str!("messaging.rs");
        let production = source_before_primary_tests(source, "messaging.rs");

        for forbidden in [
            "Inner",
            "Weak<",
            "PaneKey",
            "DaemonWorkspaceMessagingEffects",
            "messaging_runtime",
            "spawn_descendant_task",
            "enqueue_notification_attempt",
            "observe_pane(",
        ] {
            assert!(
                !production.contains(forbidden),
                "WorkspaceMessaging recovered daemon-root knowledge: {forbidden}"
            );
        }
        let daemon_root_impl = ["impl ", "Inner {"].concat();
        assert!(
            !source.contains(&daemon_root_impl),
            "WorkspaceMessaging construction returned to the operation module"
        );
    }

    /// Syntactic architecture lint: the pane sensor supplies physical
    /// evidence; durable composer state stays private to WorkspaceMessaging.
    #[test]
    fn fusion_cannot_read_durable_composer_state() {
        let fusion = source_before_primary_tests(include_str!("fusion.rs"), "fusion.rs");
        for forbidden in [
            "active_notification_barriers",
            "exact_recipient_claimed_after_write",
        ] {
            assert!(
                !fusion.contains(forbidden),
                "fusion recovered durable composer policy: {forbidden}"
            );
        }
    }

    /// Syntactic architecture lint: notification scheduling consumes one
    /// immutable observation result instead of interpreting the fusion cache.
    #[test]
    fn notification_runtime_cannot_read_detection_cache_internals() {
        let runtime = include_str!("messaging_runtime.rs");
        for forbidden in [
            ".detections",
            "DetEntry",
            ".detection.write_ready",
            ".detection.stale",
            ".detection.disagreement",
        ] {
            assert!(
                !runtime.contains(forbidden),
                "notification runtime recovered fusion cache knowledge: {forbidden}"
            );
        }
        assert!(
            runtime.contains("cached_notification_observation("),
            "notification runtime bypassed its typed observation seam"
        );
    }

    /// Syntactic architecture lint: fusion may identify immutable pane facts,
    /// but the composition root is the only handoff to messaging policy.
    #[test]
    fn pane_observation_cannot_apply_messaging_policy_directly() {
        let fusion = source_before_primary_tests(include_str!("fusion.rs"), "fusion.rs");
        for forbidden in [
            ".route_evidence_observed(",
            "workspace_messaging()",
            "confirm_dispatch_ack(",
        ] {
            assert!(
                !fusion.contains(forbidden),
                "fusion applied messaging policy directly: {forbidden}"
            );
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            Self(scratch_dir(&format!(
                "message-receipt-{tag}-{}",
                uuid::Uuid::new_v4()
            )))
        }

        fn root(&self) -> StateRoot {
            StateRoot::open_or_create(&self.0).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    panic!("remove scratch {}: {error}", self.0.display());
                }
            }
        }
    }

    fn test_directory() -> (WorkspaceId, MailboxDirectory, RecipientKey, RecipientKey) {
        let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
        let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
        let reviewer = RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%3").unwrap());
        let observer = RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%4").unwrap());
        let directory = MailboxDirectory::new(
            workspace,
            [
                MailboxIdentity {
                    key: reviewer,
                    label: "reviewer".into(),
                },
                MailboxIdentity {
                    key: observer,
                    label: "observer".into(),
                },
            ],
        )
        .unwrap();
        (workspace, directory, reviewer, observer)
    }

    fn send_to(service: &MailboxService, addresses: &[&str], subject: &str) -> AcceptResult {
        service
            .send(
                service.admin(),
                MailboxSend {
                    addresses: addresses.iter().map(|address| (*address).into()).collect(),
                    recipient_keys: None,
                    subject: subject.into(),
                    summary: None,
                    body: "Body".into(),
                    fyi: false,
                    client_key: None,
                    supersedes: None,
                    raw: false,
                },
            )
            .unwrap()
    }

    fn prepare_context(
        service: &Arc<MailboxService>,
        recipient: RecipientKey,
    ) -> (cyclops_proto::NotificationRecord, NotificationContext) {
        let record = service
            .prepare_oldest_notification(recipient)
            .unwrap()
            .unwrap();
        let context = NotificationContext::new_with_changes(
            service.store_handle(),
            record.message_id.clone(),
            recipient,
            record.attempt_id,
            service.change_publisher(),
        );
        (record, context)
    }

    fn mailbox_service(
        tag: &str,
        event_capacity: usize,
    ) -> (
        Scratch,
        Arc<MailboxService>,
        broadcast::Sender<Event>,
        RecipientKey,
        RecipientKey,
    ) {
        let scratch = Scratch::new(tag);
        let (workspace, directory, recipient, observer) = test_directory();
        let store = MessageStore::open(
            &scratch.root(),
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot",
        )
        .unwrap();
        let (events, _) = broadcast::channel(event_capacity);
        let service = Arc::new(MailboxService::new_with_events(
            directory,
            store,
            events.clone(),
        ));
        (scratch, service, events, recipient, observer)
    }

    fn runtime_composer_candidate(
        state: NotificationState,
    ) -> (
        crate::mailbox::ActiveComposerNotification,
        RecipientKey,
        MessagingComposerPhysicalBinding,
    ) {
        let workspace = WorkspaceId::from_str("00000000-0000-4000-8000-000000000001").unwrap();
        let session = SessionInstanceId::from_str("00000000-0000-4000-8000-000000000002").unwrap();
        let recipient =
            RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
        let message_id = MessageId::new("m-composer").unwrap();
        let attempt_id =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000003").unwrap();
        let binding = NotificationBinding {
            recipient,
            pane_root: Some(ProcessInstanceId::new(69, 1).unwrap()),
            leader: Some(ProcessInstanceId::new(70, 2).unwrap()),
            agent: ProcessInstanceId::new(71, 3).unwrap(),
            manifest: NotificationManifestId::new("claude").unwrap(),
        };
        let physical_binding = MessagingComposerPhysicalBinding {
            pane_root: binding.pane_root.unwrap(),
            leader: binding.leader.unwrap(),
            agent: binding.agent,
            manifest: binding.manifest.clone(),
        };
        let record = NotificationRecord {
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            state,
            binding: Some(binding.clone()),
            transport: NotificationTransport::Doorbell,
            doorbell_format: Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            cause: None,
            verified_by: None,
            verify_outcome: None,
            pre_write_cause: None,
            wake_block: None,
            pre_write_observation: None,
            pre_write_reopen_count: 0,
            unclaimed_reminder_count: 0,
            started_seq: 2,
            updated_seq: 3,
            updated_at: 4,
        };
        let message = cyclops_proto::LedgerLine {
            seq: 1,
            boot_id: "boot".into(),
            id: message_id.to_string(),
            ts: 1,
            kind: cyclops_proto::Kind::Msg,
            from: "admin".into(),
            to: vec!["claude".into()],
            subject: Some("subject".into()),
            body: Some("body".into()),
            reply_to: None,
            deliveries: Vec::new(),
            data: None,
        };
        (
            crate::mailbox::ActiveComposerNotification {
                record,
                message: Some(message),
                entry_state: Some(cyclops_proto::MailboxEntryState::Pending),
            },
            recipient,
            physical_binding,
        )
    }

    fn runtime_composer_probe(
        candidates: Vec<crate::mailbox::ActiveComposerNotification>,
    ) -> MessagingComposerProjectionProbe {
        MessagingComposerProjectionProbe {
            candidates,
            store_available: true,
        }
    }

    fn runtime_composer_observation(
        semantic: Option<ComposerSemantic>,
        owner: Option<NotificationAttemptId>,
        recipient: Option<RecipientKey>,
        binding: MessagingComposerBindingObservation,
        capture: MessagingComposerCapture,
    ) -> MessagingRuntimeComposerObservation {
        MessagingRuntimeComposerObservation {
            semantic,
            owner: owner.map(|attempt| attempt.to_string()),
            in_mode: false,
            detection_stale: false,
            terminal_state_unsafe: false,
            binding,
            recipient,
            capture,
        }
    }

    #[test]
    fn runtime_composer_projection_owns_semantic_and_exact_notification_states() {
        for (semantic, expected) in [
            (ComposerSemantic::Clean, ComposerState::ComposerClean),
            (ComposerSemantic::HumanInput, ComposerState::HumanDraft),
            (
                ComposerSemantic::GhostSuggestion,
                ComposerState::VendorGhostSuggestion,
            ),
            (
                ComposerSemantic::Ambiguous,
                ComposerState::ComposerAmbiguous,
            ),
        ] {
            let projected =
                MessagingComposerProjectionProbe::none().project(runtime_composer_observation(
                    Some(semantic),
                    None,
                    None,
                    MessagingComposerBindingObservation::Unprovable,
                    MessagingComposerCapture::NotRead,
                ));
            assert_eq!(projected.state, expected, "{semantic:?}");
        }

        let (candidate, recipient, binding) =
            runtime_composer_candidate(NotificationState::Submitted);
        let attempt = candidate.record.attempt_id;
        let expected = cyclops_proto::render_doorbell_v3(attempt);
        let probe = runtime_composer_probe(vec![candidate]);
        let staged = probe.project(runtime_composer_observation(
            Some(ComposerSemantic::HumanInput),
            Some(attempt),
            Some(recipient),
            MessagingComposerBindingObservation::Bound(binding.clone()),
            MessagingComposerCapture::Visible(expected),
        ));
        assert_eq!(staged.state, ComposerState::CyclopsNotificationStaged);
        assert_eq!(staged.proof, ComposerProof::ExactNotification);
        assert!(staged.binding_verified);

        let submitted = probe.project(runtime_composer_observation(
            Some(ComposerSemantic::Clean),
            Some(attempt),
            Some(recipient),
            MessagingComposerBindingObservation::Bound(binding),
            MessagingComposerCapture::Visible(String::new()),
        ));
        assert_eq!(submitted.state, ComposerState::CyclopsNotificationSubmitted);
        assert_eq!(submitted.proof, ComposerProof::ExactNotification);
        assert!(submitted.binding_verified);
    }

    #[test]
    fn runtime_composer_projection_keeps_an_exact_wrapped_doorbell_owned() {
        let (candidate, recipient, binding) =
            runtime_composer_candidate(NotificationState::AttentionRequired);
        let attempt = candidate.record.attempt_id;
        let expected = cyclops_proto::render_doorbell_v3(attempt);
        let (first, continuation) = expected
            .split_once(" claim ")
            .expect("format 3 has a claim continuation");
        let visible = format!("{first}\nclaim {continuation}");
        let probe = runtime_composer_probe(vec![candidate]);

        let staged = probe.project(runtime_composer_observation(
            Some(ComposerSemantic::HumanInput),
            Some(attempt),
            Some(recipient),
            MessagingComposerBindingObservation::Bound(binding.clone()),
            MessagingComposerCapture::Visible(visible.clone()),
        ));
        assert_eq!(staged.state, ComposerState::CyclopsNotificationStaged);
        assert_eq!(staged.proof, ComposerProof::ExactNotification);
        assert!(staged.binding_verified);

        let changed = probe.project(runtime_composer_observation(
            Some(ComposerSemantic::HumanInput),
            Some(attempt),
            Some(recipient),
            MessagingComposerBindingObservation::Bound(binding),
            MessagingComposerCapture::Visible(visible.replacen("claim", "claimed", 1)),
        ));
        assert_eq!(changed.state, ComposerState::ComposerAmbiguous);
        assert_eq!(changed.reason, Some("composer_content_mismatch"));
        assert!(!changed.binding_verified);
    }

    #[test]
    fn runtime_composer_projection_fails_closed_on_unsettled_or_unsafe_evidence() {
        for state in [
            NotificationState::Submitting,
            NotificationState::WithdrawnAfterStaging,
        ] {
            let (candidate, recipient, binding) = runtime_composer_candidate(state);
            let attempt = candidate.record.attempt_id;
            let projected =
                runtime_composer_probe(vec![candidate]).project(runtime_composer_observation(
                    Some(ComposerSemantic::Clean),
                    Some(attempt),
                    Some(recipient),
                    MessagingComposerBindingObservation::Bound(binding),
                    MessagingComposerCapture::Visible(String::new()),
                ));
            assert_eq!(projected.state, ComposerState::ComposerAmbiguous);
            assert_eq!(projected.proof, ComposerProof::Ambiguous);
            assert!(!projected.binding_verified);
        }

        let (candidate, recipient, binding) = runtime_composer_candidate(NotificationState::Staged);
        let attempt = candidate.record.attempt_id;
        let probe = runtime_composer_probe(vec![candidate]);
        for (mut observation, reason, proof) in [
            (
                MessagingRuntimeComposerObservation {
                    in_mode: true,
                    ..runtime_composer_observation(
                        Some(ComposerSemantic::HumanInput),
                        Some(attempt),
                        Some(recipient),
                        MessagingComposerBindingObservation::Bound(binding.clone()),
                        MessagingComposerCapture::NotRead,
                    )
                },
                "pane_in_mode",
                ComposerProof::Ambiguous,
            ),
            (
                MessagingRuntimeComposerObservation {
                    detection_stale: true,
                    ..runtime_composer_observation(
                        Some(ComposerSemantic::HumanInput),
                        Some(attempt),
                        Some(recipient),
                        MessagingComposerBindingObservation::Bound(binding.clone()),
                        MessagingComposerCapture::NotRead,
                    )
                },
                "detection_stale",
                ComposerProof::Unprovable,
            ),
            (
                MessagingRuntimeComposerObservation {
                    terminal_state_unsafe: true,
                    ..runtime_composer_observation(
                        Some(ComposerSemantic::HumanInput),
                        Some(attempt),
                        Some(recipient),
                        MessagingComposerBindingObservation::Bound(binding.clone()),
                        MessagingComposerCapture::NotRead,
                    )
                },
                "terminal_state_unsafe",
                ComposerProof::Ambiguous,
            ),
            (
                runtime_composer_observation(
                    Some(ComposerSemantic::HumanInput),
                    Some(attempt),
                    Some(recipient),
                    MessagingComposerBindingObservation::Bound(binding.clone()),
                    MessagingComposerCapture::BindingChanged,
                ),
                "binding_changed_during_capture",
                ComposerProof::Ambiguous,
            ),
            (
                runtime_composer_observation(
                    Some(ComposerSemantic::HumanInput),
                    Some(attempt),
                    Some(recipient),
                    MessagingComposerBindingObservation::Bound(binding.clone()),
                    MessagingComposerCapture::Hidden,
                ),
                "composer_hidden",
                ComposerProof::Unprovable,
            ),
        ] {
            let projected = probe.project(observation.clone());
            assert_eq!(projected.reason, Some(reason));
            assert_eq!(projected.proof, proof);
            assert!(!projected.binding_verified);
            observation.in_mode = false;
        }
    }

    #[test]
    fn runtime_composer_projection_owns_candidate_cardinality_and_binding_join() {
        let (candidate, recipient, binding) =
            runtime_composer_candidate(NotificationState::Submitted);
        let attempt = candidate.record.attempt_id;
        let expected = cyclops_proto::render_doorbell_v3(attempt);

        let mut second = candidate.clone();
        second.record.attempt_id = NotificationAttemptId::generate();
        let multiple = runtime_composer_probe(vec![candidate.clone(), second]).project(
            runtime_composer_observation(
                Some(ComposerSemantic::HumanInput),
                Some(attempt),
                Some(recipient),
                MessagingComposerBindingObservation::Bound(binding.clone()),
                MessagingComposerCapture::Visible(expected.clone()),
            ),
        );
        assert_eq!(multiple.reason, Some("multiple_active_notifications"));
        assert_eq!(multiple.candidate_count, 2);

        let mut replaced_root = binding.clone();
        replaced_root.pane_root = ProcessInstanceId::new(69, 2).unwrap();
        let mismatched =
            runtime_composer_probe(vec![candidate.clone()]).project(runtime_composer_observation(
                Some(ComposerSemantic::HumanInput),
                Some(attempt),
                Some(recipient),
                MessagingComposerBindingObservation::Bound(replaced_root),
                MessagingComposerCapture::Visible(expected.clone()),
            ));
        assert_eq!(mismatched.reason, Some("binding_mismatch"));
        assert_eq!(mismatched.proof, ComposerProof::Ambiguous);

        let mut legacy = candidate;
        legacy
            .record
            .binding
            .as_mut()
            .expect("fixture has a durable binding")
            .pane_root = None;
        let incomplete =
            runtime_composer_probe(vec![legacy.clone()]).project(runtime_composer_observation(
                Some(ComposerSemantic::HumanInput),
                None,
                Some(recipient),
                MessagingComposerBindingObservation::Bound(binding),
                MessagingComposerCapture::Visible(expected),
            ));
        assert_eq!(incomplete.reason, Some("durable_binding_incomplete"));
        assert_eq!(incomplete.notification_attempt, Some(attempt));
        assert_eq!(incomplete.proof, ComposerProof::Unprovable);

        let direct =
            MessagingComposerProjectionProbe::none().project(MessagingRuntimeComposerObservation {
                owner: Some("m-direct#1".into()),
                ..runtime_composer_observation(
                    Some(ComposerSemantic::Clean),
                    None,
                    None,
                    MessagingComposerBindingObservation::Unprovable,
                    MessagingComposerCapture::Visible(String::new()),
                )
            });
        assert_eq!(direct.reason, Some("direct_delivery_hold_unprovable"));
        assert_eq!(direct.candidate_count, 0);

        let unavailable = MessagingComposerProjectionProbe::store_unavailable().project(
            runtime_composer_observation(
                Some(ComposerSemantic::Clean),
                None,
                Some(recipient),
                MessagingComposerBindingObservation::Unprovable,
                MessagingComposerCapture::NotRead,
            ),
        );
        assert_eq!(unavailable.reason, Some("notification_store_unavailable"));
        assert_eq!(unavailable.proof, ComposerProof::Unprovable);
    }

    /// Obsolete if fusion again interprets durable composer candidates,
    /// notification payloads, or journal-state variants.
    #[test]
    fn fusion_cannot_recover_runtime_composer_projection_internals() {
        let source = include_str!("fusion.rs");
        let production = source_before_primary_tests(source, "fusion.rs");
        for forbidden in [
            "ActiveComposerNotification",
            "active_composer_notifications(",
            "expected_notification_payload(",
            "notification_submission_recorded(",
            "multiple_active_notifications",
            "notification_attempt_mismatch",
            "durable_binding_incomplete",
            "notification_payload_unprovable",
        ] {
            assert!(
                !production.contains(forbidden),
                "fusion recovered durable composer projection policy: {forbidden}"
            );
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedEffect {
        Subscribe,
        Schedule(RecipientKey),
        InvalidateUnread(RecipientKey),
        ResolvePane(RecipientKey),
        ObserveNotificationRoute(RecipientKey),
        SettleClaim(NotificationAttemptId),
        CancelNotification(NotificationAttemptId),
        ReconcileRouteEvidence(MessagingRouteEvidence),
        ReconcileCurrentRoute(usize, String),
    }

    struct RecordingEffects {
        events: broadcast::Sender<Event>,
        calls: StdMutex<Vec<RecordedEffect>>,
        notification_routes: StdMutex<HashMap<RecipientKey, MessagingNotificationRouteObservation>>,
    }

    impl RecordingEffects {
        fn new(events: broadcast::Sender<Event>) -> Self {
            Self {
                events,
                calls: StdMutex::new(Vec::new()),
                notification_routes: StdMutex::new(HashMap::new()),
            }
        }

        fn calls(&self) -> Vec<RecordedEffect> {
            self.calls.lock().expect("acceptance calls lock").clone()
        }

        fn set_notification_route(
            &self,
            recipient: RecipientKey,
            route: MessagingNotificationRouteObservation,
        ) {
            self.notification_routes
                .lock()
                .expect("notification routes lock")
                .insert(recipient, route);
        }
    }

    impl WorkspaceMessagingEffects for RecordingEffects {
        fn subscribe_messages_changed(
            &self,
        ) -> tokio::sync::broadcast::Receiver<cyclops_proto::Event> {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::Subscribe);
            self.events.subscribe()
        }

        fn schedule_notification(
            &self,
            _service: &Arc<MailboxService>,
            recipient: RecipientKey,
        ) -> Result<RecipientScheduleOutcome, MailboxServiceError> {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::Schedule(recipient));
            Ok(RecipientScheduleOutcome::NoWakeNeeded)
        }

        fn invalidate_unread(&self, recipient: RecipientKey) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::InvalidateUnread(recipient));
        }

        fn notification_route(
            &self,
            _service: &MailboxService,
            recipient: RecipientKey,
        ) -> Result<Option<NotificationRoute>, MailboxServiceError> {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ResolvePane(recipient));
            Ok(None)
        }

        fn notification_route_observation(
            &self,
            _service: &MailboxService,
            recipient: RecipientKey,
        ) -> Result<Option<MessagingNotificationRouteObservation>, MailboxServiceError> {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ObserveNotificationRoute(recipient));
            Ok(self
                .notification_routes
                .lock()
                .expect("notification routes lock")
                .get(&recipient)
                .cloned())
        }

        fn settle_notification_claim(&self, attempt_id: NotificationAttemptId) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::SettleClaim(attempt_id));
        }

        fn cancel_notification(&self, attempt_id: NotificationAttemptId) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::CancelNotification(attempt_id));
        }

        fn reconcile_route_evidence(&self, evidence: MessagingRouteEvidence) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ReconcileRouteEvidence(evidence));
        }

        fn reconcile_current_route(&self, session_idx: usize, pane_id: String) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ReconcileCurrentRoute(session_idx, pane_id));
        }

        fn receipt_block(&self) -> Duration {
            Duration::ZERO
        }
    }

    fn history_params(limit: u32, cursor: Option<u64>) -> HistoryParams {
        HistoryParams {
            with: None,
            from: None,
            to: None,
            limit,
            cursor,
        }
    }

    #[test]
    fn workspace_messaging_owns_history_visibility_redaction_and_collision_precedence() {
        let (_scratch, service, events, reviewer, observer) =
            mailbox_service("workspace-messaging-history", 8);
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            Arc::new(RecordingEffects::new(events)),
        );
        let accepted = send_to(&service, &["reviewer"], "canonical subject");
        let current = service
            .journal_lines()
            .unwrap()
            .into_iter()
            .find(|line| line.id == accepted.message_id.as_str())
            .expect("current message");
        let mut collision = current.clone();
        collision.boot_id = "legacy".into();
        collision.subject = Some("legacy collision".into());
        collision.body = Some("legacy collision body".into());
        collision.data = None;
        let mut legacy_only = collision.clone();
        legacy_only.id = "m-legacy-only".into();
        legacy_only.seq = legacy_only.seq.saturating_add(1);
        legacy_only.from = "reviewer".into();
        legacy_only.to = vec!["observer".into()];
        legacy_only.subject = Some("legacy only".into());
        legacy_only.body = Some("legacy private body".into());
        let compatibility = || SessionHistorySources {
            files: vec![(
                "session-journal:legacy.ndjson".into(),
                vec![collision.clone(), legacy_only.clone()],
            )],
            unreadable_sources: 0,
        };

        let admin = messaging
            .history(
                service.admin(),
                history_params(10, None),
                None,
                compatibility(),
            )
            .unwrap();
        let canonical = admin
            .lines
            .iter()
            .find(|line| line.id == accepted.message_id.as_str())
            .expect("canonical message");
        assert_eq!(canonical.subject.as_deref(), Some("canonical subject"));
        assert_eq!(canonical.body.as_deref(), Some("Body"));
        let legacy = admin
            .lines
            .iter()
            .find(|line| line.id == "m-legacy-only")
            .expect("legacy metadata remains visible to admin");
        assert_eq!(legacy.body, None);

        let reviewer_identity = service
            .identity_for_recipient(reviewer)
            .unwrap()
            .expect("reviewer identity");
        let before_claim = messaging
            .history(
                reviewer_identity.clone(),
                history_params(10, None),
                None,
                compatibility(),
            )
            .unwrap();
        assert_eq!(before_claim.lines.len(), 1);
        assert_eq!(before_claim.lines[0].id, accepted.message_id.as_str());
        assert_eq!(before_claim.lines[0].body, None);

        let observer_identity = service
            .identity_for_recipient(observer)
            .unwrap()
            .expect("observer identity");
        let outsider = messaging
            .history(
                observer_identity.clone(),
                history_params(10, None),
                None,
                compatibility(),
            )
            .unwrap();
        assert!(outsider.lines.is_empty());
        let error = messaging
            .thread(
                observer_identity,
                accepted.message_id.as_str(),
                false,
                compatibility(),
            )
            .unwrap_err();
        assert_eq!(error.code, "no_such_message");

        service
            .claim(reviewer, accepted.message_id.clone())
            .unwrap();
        let after_claim = messaging
            .history(
                reviewer_identity.clone(),
                history_params(10, None),
                None,
                compatibility(),
            )
            .unwrap();
        assert_eq!(after_claim.lines[0].body.as_deref(), Some("Body"));
        let thread = messaging
            .thread(
                reviewer_identity,
                accepted.message_id.as_str(),
                false,
                compatibility(),
            )
            .unwrap();
        assert_eq!(thread.lines[0].body.as_deref(), Some("Body"));
        assert!(thread
            .lines
            .iter()
            .all(|line| line.body.as_deref() != Some("legacy collision body")));
    }

    #[test]
    fn current_workspace_ownership_hides_a_legacy_open_delivery_copy() {
        let (_scratch, service, events, _, _) =
            mailbox_service("workspace-messaging-open-collision", 8);
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            Arc::new(RecordingEffects::new(events)),
        );
        let accepted = send_to(&service, &["reviewer"], "canonical owner");
        let mut legacy = service.journal_lines().unwrap();
        let legacy_message = legacy
            .iter_mut()
            .find(|line| line.id == accepted.message_id.as_str())
            .expect("legacy message copy");
        legacy_message.ts = legacy_message.ts.saturating_add(10_000);
        legacy_message.deliveries = vec![cyclops_proto::Delivery {
            to: "reviewer".into(),
            state: DeliveryState::AttentionRequired,
            verified_by: None,
            attempts: 1,
            ts: legacy_message.ts,
            cause: Some("legacy_ghost".into()),
        }];
        assert_eq!(
            crate::history::open_from(&[legacy.clone()], None).len(),
            1,
            "the legacy copy must be open when read without its current owner"
        );

        let open = messaging.retained_open_deliveries(SessionHistorySources {
            files: vec![("session-journal:legacy.ndjson".into(), legacy)],
            unreadable_sources: 0,
        });
        assert!(
            open.is_empty(),
            "the current workspace owns the colliding ID and hides the legacy copy"
        );
    }

    #[test]
    fn an_empty_workspace_keeps_the_single_compatibility_cursor_contract() {
        let (_scratch, service, events, _, _) = mailbox_service("history-empty-workspace", 8);
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            Arc::new(RecordingEffects::new(events)),
        );
        let line = |seq: u64, id: &str| cyclops_proto::LedgerLine {
            seq,
            boot_id: "legacy".into(),
            id: id.into(),
            ts: seq,
            kind: cyclops_proto::Kind::Msg,
            from: "legacy-sender".into(),
            to: vec!["legacy-recipient".into()],
            subject: Some(format!("legacy {seq}")),
            body: Some("legacy body".into()),
            reply_to: None,
            deliveries: Vec::new(),
            data: None,
        };
        let compatibility = SessionHistorySources {
            files: vec![(
                "session-journal:only.ndjson".into(),
                vec![line(1, "m-first"), line(2, "m-second")],
            )],
            unreadable_sources: 0,
        };

        let result = messaging
            .history(
                service.admin(),
                history_params(1, None),
                None,
                compatibility,
            )
            .unwrap();
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].id, "m-second");
        assert_eq!(result.lines[0].body, None);
        assert_eq!(result.next_cursor, Some(2));
        assert_eq!(result.next_cursor2, None);
    }

    // Obsolete if durable acceptance and its post-commit effects no longer form one
    // WorkspaceMessaging operation.
    #[tokio::test]
    async fn workspace_messaging_owns_acceptance_and_the_post_commit_trace_without_inner() {
        let (scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-boundary", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );

        let result = messaging
            .send(
                service.admin(),
                MsgSendParams {
                    to: vec!["reviewer".to_string()],
                    recipient_keys: None,
                    expected_caller: None,
                    subject: "Boundary".to_string(),
                    summary: Some("Keep the durable trace. Remove caller knowledge.".to_string()),
                    body: "The module owns this acceptance.".to_string(),
                    fyi: false,
                    client_key: Some("workspace-messaging-boundary".to_string()),
                    reply_to: None,
                    supersedes: None,
                    wait: None,
                    require_wake: false,
                    raw: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.inserted, Some(true));
        assert_eq!(result.deliveries.len(), 1);
        assert_eq!(result.deliveries[0].to, "reviewer");
        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::Subscribe,
                RecordedEffect::Schedule(reviewer),
                RecordedEffect::InvalidateUnread(reviewer),
                RecordedEffect::ResolvePane(reviewer),
            ]
        );

        let journal = fs::read_to_string(
            scratch
                .0
                .join("workspaces")
                .join("current")
                .join("messages.ndjson"),
        )
        .unwrap();
        assert!(journal.ends_with('\n'));
        let lines: Vec<_> = journal.lines().collect();
        assert_eq!(lines.len(), 1, "one send remains one durable message fact");
        let line: cyclops_proto::LedgerLine = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(line.kind, cyclops_proto::Kind::Msg);
        assert_eq!(line.id, result.msg_id);
        assert_eq!(line.seq, result.seq);
        assert_eq!(line.to, vec!["reviewer"]);
        let metadata: cyclops_proto::MessageMetadata =
            serde_json::from_value(line.data.unwrap()).unwrap();
        assert_eq!(metadata.recipients, vec![reviewer]);
    }

    /// A raw send to a recipient without a pane is accepted durably, records
    /// the request, and rings nothing.
    #[tokio::test]
    async fn a_raw_send_to_admin_is_accepted_durably_without_a_notification() {
        let (_scratch, service, events, reviewer, _) = mailbox_service("raw-to-admin", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let admin = service.admin().key;

        let result = messaging
            .send(
                MailboxIdentity {
                    key: reviewer,
                    label: "reviewer".to_string(),
                },
                MsgSendParams {
                    to: vec!["admin".to_string()],
                    recipient_keys: None,
                    expected_caller: None,
                    subject: "Raw".to_string(),
                    summary: None,
                    body: "pasted whole when a pane exists".to_string(),
                    fyi: false,
                    client_key: Some("raw-to-admin".to_string()),
                    reply_to: None,
                    supersedes: None,
                    wait: None,
                    require_wake: false,
                    raw: true,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.inserted, Some(true));
        let store = service.store().unwrap();
        assert!(store.projection().notifications_for(admin).is_empty());
        let message = store
            .projection()
            .get_message(&MessageId::new(&result.msg_id).unwrap())
            .cloned()
            .expect("durable message");
        let metadata: cyclops_proto::MessageMetadata =
            serde_json::from_value(message.data.unwrap()).unwrap();
        assert!(metadata.raw, "the raw request is part of the durable fact");
    }

    // Obsolete if inbox reads or claim coordination escape the
    // WorkspaceMessaging interface and callers again need projection types or
    // post-commit scheduling knowledge.
    #[test]
    fn workspace_messaging_owns_inbox_reads_claim_and_follow_up_effects_without_inner() {
        let (scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-claim", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let accepted = send_to(&service, &["reviewer"], "Claim boundary");

        let listed = messaging.inbox_list(reviewer, None, None).unwrap();
        assert_eq!(listed.entries.len(), 1);
        assert_eq!(listed.entries[0].message_id, accepted.message_id);

        let snapshot = messaging.messages_snapshot(reviewer, 20).unwrap();
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].message_id, accepted.message_id);

        let followed = messaging.messages_follow(reviewer, 0, 8).unwrap();
        assert_eq!(followed.rows.len(), 1);
        assert_eq!(followed.rows[0].message_id, accepted.message_id);

        let journal_path = scratch
            .0
            .join("workspaces")
            .join("current")
            .join("messages.ndjson");
        let before_claim = fs::read_to_string(&journal_path).unwrap().lines().count();
        let claimed = messaging
            .claim(reviewer, accepted.message_id.clone())
            .unwrap();

        assert_eq!(claimed.disposition, ClaimDisposition::Claimed);
        assert_eq!(claimed.message.message_id, accepted.message_id);
        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::Schedule(reviewer),
                RecordedEffect::InvalidateUnread(reviewer),
            ]
        );
        assert_eq!(
            fs::read_to_string(&journal_path).unwrap().lines().count(),
            before_claim + 1,
            "one claim remains one durable mailbox fact"
        );
    }

    // Obsolete if requeue or pre-write withdrawal callers again coordinate
    // durable notification mutations with workers, cancellation, or unread
    // projection themselves.
    #[test]
    fn workspace_messaging_owns_requeue_and_withdrawal_post_commit_effects_without_inner() {
        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-requeue", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        context
            .record_attention(NotificationAttentionCause::SubmitFailed)
            .unwrap();

        assert!(messaging.requeue(accepted.message_id).unwrap());
        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::Schedule(reviewer),
                RecordedEffect::InvalidateUnread(reviewer),
            ]
        );

        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-withdrawal", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (_accepted, _context, head) = queued_attempt(&service);

        let withdrawn = messaging
            .withdraw_notification(service.admin().key, reviewer, head.attempt_id)
            .unwrap();
        assert_eq!(
            withdrawn.disposition,
            NotificationWithdrawDisposition::Withdrawn
        );
        assert_eq!(withdrawn.recipient, reviewer);
        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::CancelNotification(head.attempt_id),
                RecordedEffect::Schedule(reviewer),
                RecordedEffect::InvalidateUnread(reviewer),
            ]
        );
    }

    // Obsolete if runtime observers or adapters again choose reconciliation,
    // reminder, or force-submit workers themselves.
    #[test]
    fn workspace_messaging_owns_runtime_evidence_consequences_without_inner() {
        let (_scratch, service, events, _reviewer, _) =
            mailbox_service("workspace-messaging-runtime-evidence", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let route = MessagingRouteEvidence::new(
            2,
            "%7",
            NotificationRouteEvidenceId {
                boot_id: "boot".to_string(),
                generation: 9,
            },
        );
        messaging.route_evidence_observed(route.clone());
        messaging.notification_prewrite_blocked(2, "%7");

        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::ReconcileRouteEvidence(route),
                RecordedEffect::ReconcileCurrentRoute(2, "%7".to_string()),
            ]
        );
    }

    /// Syntactic architecture lint: delivery and terminal mechanisms report a
    /// committed recipient change to WorkspaceMessaging; they cannot call the
    /// scheduler with a mailbox projection themselves.
    #[test]
    fn mechanisms_cannot_schedule_recipient_fifos_directly() {
        let delivery_src = delivery_production_source();
        for forbidden in [
            "messaging::schedule_recipient(",
            "messaging_runtime::schedule_recipient(",
        ] {
            assert!(
                !delivery_src.contains(forbidden),
                "delivery recovered direct recipient scheduling knowledge through {forbidden}"
            );
        }
    }

    /// Syntactic architecture lint: runtime callers publish evidence or invoke
    /// a named WorkspaceMessaging operation; only the composition adapter may
    /// choose one of the retained scheduling mechanisms.
    #[test]
    fn runtime_callers_cannot_schedule_messaging_work_directly() {
        let delivery_src = delivery_production_source();
        for (name, source) in [
            ("fusion", include_str!("fusion.rs")),
            ("authenticated ACK", include_str!("ack.rs")),
            ("delivery", delivery_src.as_str()),
            ("socket server", include_str!("server.rs")),
        ] {
            for forbidden in ["messaging::schedule_", "messaging_runtime::schedule_"] {
                assert!(
                    !source.contains(forbidden),
                    "{name} recovered direct messaging-worker knowledge through {forbidden}"
                );
            }
        }
    }

    /// Syntactic architecture lint: daemon lifecycle and tmux event sources
    /// publish typed observations or named availability changes. Only the
    /// `WorkspaceMessagingEffects` adapter may invoke retained schedulers.
    #[test]
    fn composition_event_sources_cannot_bypass_workspace_messaging() {
        let source = include_str!("lib.rs");
        assert_eq!(
            source
                .matches("messaging_runtime::schedule_route_evidence(")
                .count(),
            1,
            "route scheduling must remain confined to the effects adapter"
        );
        assert!(
            !source.contains("messaging_runtime::schedule_available("),
            "composition event source bypasses WorkspaceMessaging"
        );
        for required in [
            "apply_messaging_availability_change(",
            "PaneMessagingObservation::route_evidence(",
        ] {
            assert!(
                source.contains(required),
                "composition root is missing typed messaging handoff {required}"
            );
        }
    }

    /// Syntactic architecture lint: participant publication may observe daemon
    /// routes and registry entries, but it cannot recover the concrete mailbox
    /// service or its synchronization mechanism.
    #[test]
    fn participant_directory_callers_use_the_workspace_messaging_boundary() {
        let source = include_str!("lib.rs");
        let production = source_before_primary_tests(source, "lib.rs");
        for (start, next) in [
            (
                "fn publish_mailbox_directory(",
                "#[cfg(test)]\npub(crate) fn mailbox_recipient_for_origin(",
            ),
            (
                "fn commit_adoption_during_publication(",
                "/// The chrome restore `--clear` runs",
            ),
            (
                "fn rebind_same_session_adoptions(",
                "fn retire_pane_process_trust(",
            ),
            ("fn replace_pane_process(", "async fn handle_pane_event("),
        ] {
            let section = production
                .split_once(start)
                .unwrap_or_else(|| panic!("participant section {start}"))
                .1
                .split_once(next)
                .unwrap_or_else(|| panic!("participant section after {start}"))
                .0;
            for forbidden in [
                "mailbox_publication",
                "inner.mailbox",
                "service.replace_directory",
            ] {
                assert!(
                    !section.contains(forbidden),
                    "participant section {start} recovered {forbidden}"
                );
            }
        }
        assert!(production.contains("with_messaging_publication("));
        assert!(production.contains("publish_mailbox_directory(inner, messaging"));
        assert!(production.contains(".replace_directory(directory)"));
    }

    /// Syntactic architecture lint: the authenticated hook adapter resolves
    /// receipts through the delivery engine and never reaches the mailbox.
    #[test]
    fn authenticated_hook_cannot_access_messaging_internals() {
        let source = include_str!("ack.rs");
        let production = source_before_primary_tests(source, "ack.rs");

        for forbidden in ["inner.mailbox", "MailboxService"] {
            assert!(
                !production.contains(forbidden),
                "authenticated hook recovered messaging internals through {forbidden}"
            );
        }
    }

    /// Syntactic architecture lint: delivery supplies physical evidence and
    /// reacts to a body-free result. It cannot recover the publication lock or
    /// append a pre-write transition itself.
    #[test]
    fn delivery_cannot_own_the_prewrite_transaction() {
        let production = delivery_production_source();

        for required in [
            "record_notification_prewrite_block(",
            "record_worker_failed_prewrite(",
        ] {
            assert!(
                production.contains(required),
                "delivery stopped using the WorkspaceMessaging boundary: {required}"
            );
        }
        for forbidden in [
            "mailbox_publication",
            "record_pre_write_block_with_wake_block",
            "record_pre_write_block(NotificationPreWriteCause::WorkerFailed",
        ] {
            assert!(
                !production.contains(forbidden),
                "delivery recovered pre-write messaging internals through {forbidden}"
            );
        }
    }

    // Obsolete if delivery again synthesizes route baselines, chooses wake
    // blocks, or interprets durable terminal variants itself.
    #[test]
    fn workspace_messaging_owns_the_prewrite_transaction_and_policy() {
        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-prewrite", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        let baseline = NotificationRouteEvidenceId {
            boot_id: "boot".into(),
            generation: 7,
        };

        assert_eq!(
            messaging
                .record_notification_prewrite_block(
                    &context,
                    NotificationPreWriteCause::WriteReadinessChanged,
                    None,
                    baseline.clone(),
                    2,
                    "%7",
                )
                .unwrap(),
            MessagingPreWriteBlockOutcome::Recorded(MessagingPreWriteBlock {
                attempt_id: context.attempt_id(),
                cause: NotificationPreWriteCause::WriteReadinessChanged,
            })
        );
        let record = service
            .store_handle()
            .lock()
            .unwrap()
            .projection()
            .notification(reviewer, &accepted.message_id)
            .cloned()
            .expect("pre-write block is durable");
        let observation = record
            .pre_write_observation
            .expect("readiness race keeps its route baseline");
        assert_eq!(observation.route_evidence, Some(baseline));
        assert!(observation.binding.is_none());
        assert_eq!(
            effects.calls(),
            vec![RecordedEffect::ReconcileCurrentRoute(2, "%7".to_string())]
        );
    }

    #[test]
    fn workspace_messaging_prewrite_preserves_claims_and_classifies_obsolete_work() {
        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-prewrite-claim", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging =
            WorkspaceMessaging::new(Arc::clone(&service), Arc::new(StdMutex::new(())), effects);
        let (accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        service
            .claim(reviewer, accepted.message_id.clone())
            .unwrap();
        let route = NotificationRouteEvidenceId {
            boot_id: "boot".into(),
            generation: 1,
        };

        assert!(matches!(
            messaging
                .record_notification_prewrite_block(
                    &context,
                    NotificationPreWriteCause::WriteReadinessChanged,
                    None,
                    route.clone(),
                    0,
                    "%3",
                )
                .unwrap(),
            MessagingPreWriteBlockOutcome::Recorded(_)
        ));
        assert_eq!(
            service.message_dispositions(&accepted.message_id).unwrap()[0].notification_state_raw,
            Some(NotificationState::BlockedPreWrite),
            "a body claim does not retire the operator-visible notification"
        );
        messaging
            .withdraw_notification(service.admin().key, reviewer, context.attempt_id())
            .unwrap();
        assert_eq!(
            messaging
                .record_notification_prewrite_block(
                    &context,
                    NotificationPreWriteCause::WriteReadinessChanged,
                    None,
                    route,
                    0,
                    "%3",
                )
                .unwrap(),
            MessagingPreWriteBlockOutcome::Obsolete
        );

        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-worker-obsolete", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging =
            WorkspaceMessaging::new(Arc::clone(&service), Arc::new(StdMutex::new(())), effects);
        let (_accepted, context, _) = queued_attempt(&service);
        messaging
            .withdraw_notification(service.admin().key, reviewer, context.attempt_id())
            .unwrap();
        assert_eq!(
            messaging.record_worker_failed_prewrite(&context).unwrap(),
            MessagingPreWriteBlockOutcome::Obsolete,
            "supervisor exhaustion cannot revive an operator-withdrawn attempt"
        );
    }

    #[test]
    fn workspace_messaging_prewrite_reports_storage_uncertainty_without_a_false_block() {
        let (_scratch, service, events, _reviewer, _) =
            mailbox_service("workspace-messaging-prewrite-failure", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        service
            .store_handle()
            .lock()
            .unwrap()
            .inject_next_pre_write_block_append_failure();

        assert!(messaging
            .record_notification_prewrite_block(
                &context,
                NotificationPreWriteCause::WriteReadinessChanged,
                None,
                NotificationRouteEvidenceId {
                    boot_id: "boot".into(),
                    generation: 1,
                },
                0,
                "%3",
            )
            .is_err());
        let disposition = service.message_dispositions(&accepted.message_id).unwrap();
        assert_eq!(
            disposition[0].notification_state_raw,
            Some(NotificationState::Gating)
        );
        assert_eq!(disposition[0].pre_write_cause, None);
        assert!(effects.calls().is_empty());

        let (_scratch, service, events, _reviewer, _) =
            mailbox_service("workspace-messaging-worker-failure", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging =
            WorkspaceMessaging::new(Arc::clone(&service), Arc::new(StdMutex::new(())), effects);
        let (_accepted, context, _) = queued_attempt(&service);
        assert_eq!(
            messaging.record_worker_failed_prewrite(&context).unwrap(),
            MessagingPreWriteBlockOutcome::Recorded(MessagingPreWriteBlock {
                attempt_id: context.attempt_id(),
                cause: NotificationPreWriteCause::WorkerFailed,
            })
        );
        assert_eq!(
            context.current_record().unwrap().state,
            NotificationState::BlockedPreWrite
        );
    }

    // Obsolete if alarm or attention adapters again inspect notification
    // records, resolve ambiguous targets, or decide recipient visibility.
    #[test]
    fn workspace_messaging_owns_alarm_projection_without_inner() {
        let (_scratch, service, events, reviewer, observer) =
            mailbox_service("workspace-messaging-attention", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (_accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        let attention = context
            .record_attention(NotificationAttentionCause::SubmitFailed)
            .unwrap();
        let admin = service.admin().key;

        assert!(matches!(
            messaging.alarm_preview(reviewer, 0, u64::MAX),
            Err(MessagingAttentionError::Denied)
        ));
        assert!(matches!(
            messaging.alarm_preview(observer, 0, u64::MAX),
            Err(MessagingAttentionError::Denied)
        ));
        let preview = messaging.alarm_preview(admin, 0, u64::MAX).unwrap();
        assert_eq!(preview.entries.len(), 1);
        assert_eq!(preview.entries[0].id, attention.attempt_id.to_string());
        assert_eq!(
            preview.entries[0].cause,
            NotificationAttentionCause::SubmitFailed
        );

        let cleared = messaging
            .clear_alarms(admin, &[attention.attempt_id], Some(u64::MAX))
            .unwrap();
        assert_eq!(cleared.cleared_ids, vec![attention.attempt_id.to_string()]);
        assert_eq!(cleared.summaries.len(), 1);
        assert_eq!(cleared.summaries[0].id, preview.entries[0].id);
        assert_eq!(cleared.summaries[0].cause, preview.entries[0].cause);
        assert!(effects.calls().is_empty());
    }

    // Obsolete if daemon status again reconstructs mailbox routes, unread
    // counts, held attention, or blocked-notification samples itself.
    #[test]
    fn workspace_messaging_owns_the_body_free_status_projection_without_inner() {
        let (_scratch, service, events, reviewer, observer) =
            mailbox_service("workspace-messaging-status", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let accepted = send_to(&service, &["reviewer", "observer"], "Status boundary");
        let (_record, context) = prepare_context(&service, reviewer);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        context
            .record_attention(NotificationAttentionCause::SubmitFailed)
            .unwrap();

        let quiet = messaging.status_snapshot(false, u64::MAX, 32);
        assert!(quiet.mailbox_attention.is_empty());
        assert_eq!(quiet.admin_unread, 0);
        assert_eq!(quiet.unread_for(reviewer), Some(1));
        assert_eq!(quiet.unread_for(observer), Some(1));
        assert_eq!(quiet.mailbox_routes.len(), 3);
        assert!(quiet.mailbox_routes.iter().any(|route| {
            route.recipient == reviewer && route.label == "reviewer" && route.unread == Some(1)
        }));
        assert!(quiet.mailbox_routes.iter().any(|route| {
            route.recipient == observer && route.label == "observer" && route.unread == Some(1)
        }));

        let detailed = messaging.status_snapshot(true, u64::MAX, 32);
        assert_eq!(detailed.mailbox_attention.len(), 1);
        assert_eq!(
            detailed.mailbox_attention[0].id,
            accepted.message_id.to_string()
        );
        assert_eq!(detailed.mailbox_attention[0].recipient, Some(reviewer));
        assert_eq!(
            detailed.mailbox_attention[0].cause.as_deref(),
            Some("submit_failed")
        );
        assert!(detailed.blocked_notifications.is_empty());
        assert_eq!(detailed.blocked_notifications_total, 0);
        assert!(effects.calls().is_empty());
    }

    // Obsolete if status diagnostics again read gating records, resolve routes,
    // or interpret current agent state outside WorkspaceMessaging.
    #[test]
    fn workspace_messaging_owns_foreground_watch_candidate_selection() {
        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-deadlock-candidates", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        effects.set_notification_route(
            reviewer,
            MessagingNotificationRouteObservation {
                pane_id: "%3".to_string(),
                recipient_label: "reviewer".to_string(),
                pane_pid: 4242,
                agent_state: AgentState::Working,
            },
        );

        let status = messaging.status_snapshot(false, u64::MAX, 32);
        assert_eq!(
            status.deadlock_candidates(),
            &[MessagingDeadlockCandidate {
                message_id: accepted.message_id,
                notification_attempt: context.attempt_id(),
                recipient: reviewer,
                recipient_label: "reviewer".to_string(),
                pane_id: "%3".to_string(),
                pane_pid: 4242,
            }]
        );
        assert_eq!(
            effects.calls(),
            vec![RecordedEffect::ObserveNotificationRoute(reviewer)]
        );

        effects.set_notification_route(
            reviewer,
            MessagingNotificationRouteObservation {
                pane_id: "%3".to_string(),
                recipient_label: "reviewer".to_string(),
                pane_pid: 4242,
                agent_state: AgentState::Idle,
            },
        );
        assert!(messaging
            .status_snapshot(false, u64::MAX, 32)
            .deadlock_candidates()
            .is_empty());
    }

    /// Syntactic architecture lint: process diagnosis consumes body-free
    /// candidates and cannot regain durable messaging or daemon-root access.
    #[test]
    fn foreground_watch_diagnostics_cannot_read_messaging_internals() {
        let source = include_str!("deadlock.rs");
        for forbidden in [
            "MailboxService",
            "gating_notifications",
            "notification_route",
            "messaging_runtime",
            "cached_state",
            "crate::Inner",
        ] {
            assert!(
                !source.contains(forbidden),
                "foreground-watch diagnostics recovered messaging knowledge: {forbidden}"
            );
        }
    }

    // Obsolete if readiness handling again invokes route reconciliation from
    // fusion instead of returning immutable evidence to the composition root.
    #[test]
    fn workspace_messaging_applies_a_readiness_route_observation() {
        let (_scratch, service, events, _reviewer, _) =
            mailbox_service("workspace-messaging-route-observation", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let evidence = MessagingRouteEvidence::new(
            2,
            "%7",
            NotificationRouteEvidenceId {
                boot_id: "boot".to_string(),
                generation: 9,
            },
        );

        messaging.apply_observation(PaneMessagingObservation::route_evidence(evidence.clone()));

        assert_eq!(
            effects.calls(),
            vec![RecordedEffect::ReconcileRouteEvidence(evidence)]
        );
    }

    // Obsolete if daemon lifecycle code regains pending-recipient projection
    // knowledge or directly loops the notification scheduler.
    #[test]
    fn workspace_messaging_owns_availability_and_replay_candidate_selection() {
        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-availability", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        send_to(&service, &["reviewer"], "Available");

        messaging.availability_changed();

        assert_eq!(effects.calls(), vec![RecordedEffect::Schedule(reviewer)]);
    }

    fn queued_attempt(
        service: &Arc<MailboxService>,
    ) -> (AcceptResult, NotificationContext, ScheduledHead) {
        let accepted = send_to(service, &["reviewer"], "Receipt");
        let recipient = accepted.recipient_keys[0];
        let (record, context) = prepare_context(service, recipient);
        let head = ScheduledHead::new(record.message_id.clone(), record.attempt_id);
        (accepted, context, head)
    }

    fn durable_observation(recipient: RecipientKey) -> NotificationPreWriteObservation {
        let pane_root = ProcessInstanceId::new(4000, 818_000).unwrap();
        NotificationPreWriteObservation {
            write_block: None,
            pane_root: Some(pane_root),
            selected_manifest: Some(NotificationManifestId::new("claude").unwrap()),
            binding: Some(NotificationBinding {
                recipient,
                pane_root: Some(pane_root),
                leader: Some(ProcessInstanceId::new(4001, 818_001).unwrap()),
                agent: ProcessInstanceId::new(4002, 818_002).unwrap(),
                manifest: NotificationManifestId::new("claude").unwrap(),
            }),
            route_evidence: None,
            pane_width: Some(120),
            required_pane_width: None,
        }
    }

    fn record_doorbell_write(context: &NotificationContext) {
        let observation = durable_observation(context.recipient());
        let binding = observation.binding.unwrap();
        context
            .record_writing(
                binding.pane_root.unwrap(),
                binding.leader.unwrap(),
                binding.agent,
                binding.manifest.as_str(),
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            )
            .unwrap();
    }

    #[test]
    fn workspace_messaging_joins_composer_evidence_without_exposing_candidates() {
        use cyclops_proto::{ComposerNextAction, ComposerProof, ComposerState};

        let (_scratch, service, _events, recipient, _) =
            mailbox_service("composer-status-ownership", 8);
        send_to(&service, &["reviewer"], "Composer status");
        let (_record, context) = prepare_context(&service, recipient);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        let active = service
            .active_composer_notifications_snapshot()
            .unwrap()
            .pop()
            .expect("one active composer candidate");
        let attempt_id = active.record.attempt_id;
        let binding = active.record.binding.clone().expect("complete binding");
        let candidate = MessagingComposerCandidate {
            record: active.record,
            message_state: active
                .entry_state
                .as_ref()
                .map(cyclops_proto::ComposerMessageState::from),
        };
        let mut candidates = HashMap::new();
        candidates.insert(attempt_id, candidate.clone());
        let status = WorkspaceMessagingStatus {
            composer_candidates: Some(HashMap::from([(recipient, candidates)])),
            ..WorkspaceMessagingStatus::default()
        };
        let observation = MessagingComposerObservation {
            composer: ComposerState::CyclopsNotificationStaged,
            proof: ComposerProof::ExactNotification,
            reason: None,
            detected_attempt: Some(attempt_id),
            detected_candidate_count: 1,
            pane_root: binding.pane_root,
            binding: Some(binding.clone()),
        };

        let exact = status.composer_status(Some(recipient), observation.clone());
        assert_eq!(exact.attempt, Some(attempt_id));
        assert_eq!(exact.candidate_count, 1);
        assert_eq!(exact.next_action, Some(ComposerNextAction::CheckHealth));

        let mut mismatched = observation.clone();
        mismatched.binding.as_mut().unwrap().manifest =
            NotificationManifestId::new("other").unwrap();
        let mismatched = status.composer_status(Some(recipient), mismatched);
        assert_eq!(mismatched.composer, ComposerState::ComposerAmbiguous);
        assert_eq!(mismatched.proof, ComposerProof::Ambiguous);
        assert_eq!(mismatched.reason.as_deref(), Some("binding_mismatch"));

        let mut unprovable = observation.clone();
        unprovable.binding = None;
        let unprovable = status.composer_status(Some(recipient), unprovable);
        assert_eq!(unprovable.proof, ComposerProof::Unprovable);
        assert_eq!(unprovable.reason.as_deref(), Some("binding_unprovable"));

        let missing = WorkspaceMessagingStatus::default()
            .composer_status(Some(recipient), observation.clone());
        assert_eq!(missing.composer, ComposerState::ComposerAmbiguous);
        assert_eq!(missing.proof, ComposerProof::Unprovable);
        assert_eq!(
            missing.next_action,
            Some(ComposerNextAction::InspectMessages)
        );

        let second_attempt = NotificationAttemptId::generate();
        let mut multiple = HashMap::new();
        multiple.insert(attempt_id, candidate.clone());
        let mut second = candidate;
        second.record.attempt_id = second_attempt;
        multiple.insert(second_attempt, second);
        let status = WorkspaceMessagingStatus {
            composer_candidates: Some(HashMap::from([(recipient, multiple)])),
            ..WorkspaceMessagingStatus::default()
        };
        let multiple = status.composer_status(Some(recipient), observation);
        assert_eq!(multiple.candidate_count, 2);
        assert_eq!(multiple.attempt, None);
        assert_eq!(
            multiple.next_action,
            Some(ComposerNextAction::InspectMessages)
        );
    }

    #[test]
    fn require_wake_waits_past_writing_for_the_exact_attempt() {
        let (_scratch, service, _events, _recipient, _) =
            mailbox_service("require-wake-boundary", 8);
        let (accepted, context, head) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);

        let writing = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert!(has_first_durable_disposition(&writing, &head, false));
        assert!(!has_first_durable_disposition(&writing, &head, true));

        context.record_submitted().unwrap();
        let submitted = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert!(has_first_durable_disposition(&submitted, &head, true));
    }

    #[test]
    fn a_scheduler_failure_keeps_acceptance_and_does_not_skip_later_recipients() {
        let (_scratch, service, _events, reviewer, observer) =
            mailbox_service("accepted-scheduler-failure", 8);
        let accepted = send_to(&service, &["reviewer", "observer"], "Broadcast");

        let mut attempted = Vec::new();
        let report = schedule_accepted_notifications(&accepted, |recipient| {
            attempted.push(recipient);
            if recipient == reviewer {
                Err(MailboxServiceError::Poisoned)
            } else {
                Ok(RecipientScheduleOutcome::NoWakeNeeded)
            }
        });
        assert_eq!(attempted, vec![reviewer, observer]);
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(
            report.outcomes[&observer],
            RecipientScheduleOutcome::NoWakeNeeded
        );
        assert_eq!(report.unavailable, HashSet::from([reviewer]));

        let receipts: Vec<_> = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .into_iter()
            .map(|disposition| {
                let unavailable = report.unavailable.contains(&disposition.recipient);
                receipt_with_schedule_truth(disposition, None, unavailable)
            })
            .collect();
        assert_eq!(
            receipts
                .iter()
                .find(|receipt| receipt.to == "reviewer")
                .unwrap()
                .wake_block,
            Some(MessageWakeBlock::SchedulerStateUnavailable),
            "the accepted recipient gets a truthful nonzero-exit receipt"
        );
        assert_eq!(
            receipts
                .iter()
                .find(|receipt| receipt.to == "observer")
                .unwrap()
                .wake_block,
            None,
            "one recipient's scheduler failure cannot contaminate another"
        );
    }

    #[test]
    fn a_blocked_head_never_contaminates_a_follower_receipt() {
        let (_scratch, service, _events, recipient, _) = mailbox_service("follower-block", 8);
        let first = send_to(&service, &["reviewer"], "First");
        let second = send_to(&service, &["reviewer"], "Second");
        let (_record, context) = prepare_context(&service, recipient);
        context.record_gating().unwrap();
        context
            .record_pre_write_block_with_wake_block(
                NotificationPreWriteCause::WorkerFailed,
                None,
                Some(MessageWakeBlock::WorkerSupervisorExited),
            )
            .unwrap();

        let head = service.message_dispositions(&first.message_id).unwrap();
        assert_eq!(
            head[0].wake_block,
            Some(MessageWakeBlock::WorkerSupervisorExited)
        );
        let follower = service.message_dispositions(&second.message_id).unwrap();
        assert_eq!(follower[0].position_ahead, Some(1));
        assert_eq!(follower[0].attempt_id, None);
        assert_eq!(follower[0].pre_write_cause, None);
        assert_eq!(
            follower[0].wake_block, None,
            "a follower may not inherit the FIFO head's scheduler block"
        );
    }

    #[test]
    fn a_recorded_scheduler_failure_is_identical_live_replayed_and_in_the_receipt() {
        let (scratch, service, _events, recipient, _) =
            mailbox_service("scheduler-block-replay", 8);
        let (accepted, context, _head) = queued_attempt(&service);
        let outcome = record_unowned_notification(
            &StdMutex::new(()),
            &context,
            NotificationPreWriteCause::WorkerFailed,
            MessageWakeBlock::WorkerSupervisorExited,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            RecipientScheduleOutcome::Blocked {
                block: MessageWakeBlock::WorkerSupervisorExited,
                ..
            }
        ));

        let live = service.message_dispositions(&accepted.message_id).unwrap();
        let live_receipt = receipt_from_disposition(live[0].clone(), None);
        assert_eq!(
            live_receipt.wake_block,
            Some(MessageWakeBlock::WorkerSupervisorExited)
        );
        assert_eq!(
            live_receipt.pre_write_cause,
            Some(NotificationPreWriteCause::WorkerFailed)
        );

        drop(context);
        drop(service);
        let (workspace, directory, replayed_recipient, _) = test_directory();
        assert_eq!(replayed_recipient, recipient);
        let store = MessageStore::open(
            &scratch.root(),
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot-replay",
        )
        .unwrap();
        let replayed = MailboxService::new(directory, store)
            .message_dispositions(&accepted.message_id)
            .unwrap();
        assert_eq!(replayed, live);
        let replayed_receipt = receipt_from_disposition(replayed[0].clone(), None);
        assert_eq!(
            serde_json::to_value(replayed_receipt).unwrap(),
            serde_json::to_value(live_receipt).unwrap()
        );
    }

    #[test]
    fn post_write_attention_without_a_scheduler_fact_has_no_wake_block() {
        let (_scratch, service, _events, _recipient, _) =
            mailbox_service("post-write-attention-no-wake-block", 8);
        let (accepted, context, _head) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        context
            .record_attention(NotificationAttentionCause::SubmitFailed)
            .unwrap();

        let disposition = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            disposition.notification_state_raw,
            Some(NotificationState::AttentionRequired)
        );
        assert_eq!(disposition.wake_block, None);
        assert_eq!(receipt_from_disposition(disposition, None).wake_block, None);
    }

    #[test]
    fn a_scheduler_disposition_append_failure_is_propagated() {
        let (_scratch, service, _events, _recipient, _) =
            mailbox_service("scheduler-block-append-failure", 8);
        let (accepted, context, _head) = queued_attempt(&service);
        service
            .store_handle()
            .lock()
            .unwrap()
            .inject_next_pre_write_block_append_failure();

        let error = record_unowned_notification(
            &StdMutex::new(()),
            &context,
            NotificationPreWriteCause::WorkerFailed,
            MessageWakeBlock::WorkerSupervisorExited,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MailboxServiceError::NotificationSchedule(_)
        ));
        let dispositions = service.message_dispositions(&accepted.message_id).unwrap();
        assert_eq!(
            dispositions[0].notification_state_raw,
            Some(NotificationState::Gating)
        );
        assert_eq!(dispositions[0].pre_write_cause, None);
        assert_eq!(dispositions[0].wake_block, None);
    }

    #[tokio::test]
    async fn a_reopened_attempt_does_not_keep_its_stale_schedule_block() {
        let (_scratch, service, events, reviewer, observer) =
            mailbox_service("reopened-receipt", 8);
        let accepted = send_to(&service, &["reviewer", "observer"], "Broadcast");

        let (reviewer_record, reviewer_context) = prepare_context(&service, reviewer);
        reviewer_context.record_gating().unwrap();
        reviewer_context
            .record_pre_write_block_with_wake_block(
                NotificationPreWriteCause::SessionUnavailable,
                None,
                Some(MessageWakeBlock::RouteUnavailable),
            )
            .unwrap();

        let (observer_record, observer_context) = prepare_context(&service, observer);
        observer_context.record_gating().unwrap();

        let outcomes = HashMap::from([
            (
                reviewer,
                RecipientScheduleOutcome::Blocked {
                    head: ScheduledHead::new(
                        reviewer_record.message_id.clone(),
                        reviewer_record.attempt_id,
                    ),
                    block: MessageWakeBlock::RouteUnavailable,
                },
            ),
            (
                observer,
                RecipientScheduleOutcome::WorkerOwned {
                    head: ScheduledHead::new(
                        observer_record.message_id.clone(),
                        observer_record.attempt_id,
                    ),
                    observe_first_disposition: true,
                },
            ),
        ]);
        let receiver = events.subscribe();
        let observe = observe_first_durable_dispositions(
            &service,
            &accepted.message_id,
            &outcomes,
            receiver,
            Instant::now() + Duration::from_secs(1),
            false,
        );
        let advance = async {
            tokio::task::yield_now().await;
            let reopened = service
                .reopen_oldest_notification_after_route_evidence(
                    reviewer,
                    durable_observation(reviewer),
                    true,
                )
                .unwrap()
                .unwrap();
            assert_eq!(reopened.attempt_id, reviewer_record.attempt_id);
            observer_context
                .record_pre_write_block_with_wake_block(
                    NotificationPreWriteCause::WorkerFailed,
                    None,
                    Some(MessageWakeBlock::WorkerSupervisorExited),
                )
                .unwrap();
        };
        let (dispositions, ()) = tokio::join!(observe, advance);
        let dispositions = dispositions.unwrap();
        let disposition = dispositions
            .into_iter()
            .find(|disposition| disposition.recipient == reviewer)
            .unwrap();
        assert_eq!(
            disposition.attempt_id,
            Some(reviewer_record.attempt_id),
            "the same durable attempt must remain current"
        );
        assert_eq!(
            disposition.notification_state_raw,
            Some(NotificationState::Gating)
        );
        assert_eq!(disposition.pre_write_cause, None);
        assert_eq!(disposition.wake_block, None);

        let receipt = receipt_from_disposition(disposition, None);
        assert_eq!(receipt.pre_write_cause, None);
        assert_eq!(
            receipt.wake_block, None,
            "a stale scheduling result must not override the exact projection"
        );
    }

    #[tokio::test]
    async fn a_current_block_without_a_scheduler_fact_invents_no_wake_block() {
        let (_scratch, service, events, recipient, _) = mailbox_service("initial-read", 8);
        let (accepted, context, head) = queued_attempt(&service);
        context.record_gating().unwrap();
        let receiver = events.subscribe();
        context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(NotificationPreWriteObservation {
                    write_block: None,
                    pane_root: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
                    selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                    binding: None,
                    route_evidence: None,
                    pane_width: None,
                    required_pane_width: None,
                }),
            )
            .unwrap();
        assert_eq!(
            service.notification_schedule_block(recipient).unwrap(),
            None
        );
        let outcomes = HashMap::from([(
            recipient,
            RecipientScheduleOutcome::WorkerOwned {
                head,
                observe_first_disposition: true,
            },
        )]);

        let dispositions = observe_first_durable_dispositions(
            &service,
            &accepted.message_id,
            &outcomes,
            receiver,
            Instant::now() + Duration::from_secs(1),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            dispositions[0].notification_state_raw,
            Some(NotificationState::BlockedPreWrite)
        );
        assert_eq!(
            dispositions[0].pre_write_cause,
            Some(NotificationPreWriteCause::BindingUnprovable)
        );
        assert_eq!(
            dispositions[0].wake_block, None,
            "a current block without a recorded scheduler outcome stays unknown"
        );
    }

    #[test]
    fn a_pre_wake_block_journal_row_replays_without_inventing_a_scheduler_outcome() {
        let (scratch, service, _events, recipient, _) =
            mailbox_service("legacy-block-no-wake-block", 8);
        let (accepted, context, head) = queued_attempt(&service);
        context.record_gating().unwrap();
        let seq = service
            .store_handle()
            .lock()
            .unwrap()
            .projection()
            .last_sequence()
            .unwrap()
            + 1;
        let line = cyclops_proto::LedgerLine {
            seq,
            boot_id: "boot-before-wake-block".into(),
            id: accepted.message_id.to_string(),
            ts: 1_700_000_000_000 + seq,
            kind: cyclops_proto::Kind::State,
            from: "cyclopsd".into(),
            to: vec![recipient.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::json!({
                "type": "notification_transition",
                "record_version": cyclops_proto::CANONICAL_RECORD_VERSION,
                "attempt_id": head.attempt_id,
                "message_id": accepted.message_id,
                "recipient": recipient,
                "state": "blocked_pre_write",
                "pre_write_cause": "worker_failed"
            })),
        };
        assert!(line.data.as_ref().unwrap().get("wake_block").is_none());
        drop(context);
        drop(service);

        let root = scratch.root();
        let journal = Path::new("workspaces/current/messages.ndjson");
        let mut file = root.open_append(journal).unwrap();
        serde_json::to_writer(&mut file, &line).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_data().unwrap();
        drop(file);

        let (workspace, directory, replayed_recipient, _) = test_directory();
        assert_eq!(replayed_recipient, recipient);
        let replayed = MailboxService::new(
            directory,
            MessageStore::open(&root, journal, workspace, "boot-replay").unwrap(),
        );
        assert_eq!(
            replayed.notification_schedule_block(recipient).unwrap(),
            None
        );
        let disposition = replayed
            .message_dispositions(&line.id.parse().unwrap())
            .unwrap()
            .remove(0);
        assert_eq!(
            disposition.notification_state_raw,
            Some(NotificationState::BlockedPreWrite)
        );
        assert_eq!(disposition.wake_block, None);
        assert_eq!(receipt_from_disposition(disposition, None).wake_block, None);
    }

    #[tokio::test]
    async fn a_lagged_change_stream_invalidates_the_projection() {
        let (events, _) = broadcast::channel(1);
        let mut receiver = events.subscribe();
        for seq in 1..=3 {
            events
                .send(Event {
                    event: "state".into(),
                    data: serde_json::Value::Null,
                    seq: Some(seq),
                })
                .unwrap();
        }
        assert!(
            wait_for_messages_change(&mut receiver, Instant::now() + Duration::from_secs(1)).await,
            "lag must trigger an authoritative projection reread"
        );
    }

    #[tokio::test]
    async fn receipt_observation_timeout_writes_no_delivery_fact() {
        let (_scratch, service, events, recipient, _) = mailbox_service("timeout-pure", 8);
        let (accepted, context, head) = queued_attempt(&service);
        context.record_gating().unwrap();
        let receiver = events.subscribe();
        let lines_before = service.journal_lines().unwrap().len();
        let outcomes = HashMap::from([(
            recipient,
            RecipientScheduleOutcome::WorkerOwned {
                head,
                observe_first_disposition: true,
            },
        )]);

        let dispositions = observe_first_durable_dispositions(
            &service,
            &accepted.message_id,
            &outcomes,
            receiver,
            Instant::now() + Duration::from_millis(10),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            dispositions[0].notification_state_raw,
            Some(NotificationState::Gating)
        );
        assert_eq!(service.journal_lines().unwrap().len(), lines_before);
    }

    #[tokio::test(start_paused = true)]
    async fn receipt_timeout_takes_one_final_projection_read() {
        let (_scratch, service, events, recipient, _) = mailbox_service("timeout-final-read", 8);
        let (accepted, context, head) = queued_attempt(&service);
        context.record_gating().unwrap();
        let outcomes = HashMap::from([(
            recipient,
            RecipientScheduleOutcome::WorkerOwned {
                head,
                observe_first_disposition: true,
            },
        )]);
        let receiver = events.subscribe();
        let mut reads = 0;

        let dispositions = observe_first_durable_dispositions_with(
            &accepted.message_id,
            &outcomes,
            receiver,
            Instant::now() + Duration::from_secs(10),
            false,
            || {
                reads += 1;
                if reads == 2 {
                    context
                        .record_pre_write_block_with_wake_block(
                            NotificationPreWriteCause::WorkerFailed,
                            None,
                            Some(MessageWakeBlock::WorkerSupervisorExited),
                        )
                        .unwrap();
                }
                service.message_dispositions(&accepted.message_id)
            },
            Instant::now,
        )
        .await
        .unwrap();

        assert_eq!(reads, 2, "timeout must take one final projection read");
        assert_eq!(
            dispositions[0].notification_state_raw,
            Some(NotificationState::BlockedPreWrite)
        );
        assert_eq!(
            dispositions[0].wake_block,
            Some(MessageWakeBlock::WorkerSupervisorExited)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_projection_read_that_crosses_the_deadline_takes_one_final_read() {
        let (_scratch, service, events, recipient, _) =
            mailbox_service("deadline-crossing-final-read", 8);
        let (accepted, context, head) = queued_attempt(&service);
        context.record_gating().unwrap();
        let outcomes = HashMap::from([(
            recipient,
            RecipientScheduleOutcome::WorkerOwned {
                head,
                observe_first_disposition: true,
            },
        )]);
        let receiver = events.subscribe();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut reads = 0;

        let dispositions = observe_first_durable_dispositions_with(
            &accepted.message_id,
            &outcomes,
            receiver,
            deadline,
            false,
            || {
                reads += 1;
                if reads == 2 {
                    context
                        .record_pre_write_block_with_wake_block(
                            NotificationPreWriteCause::WorkerFailed,
                            None,
                            Some(MessageWakeBlock::WorkerSupervisorExited),
                        )
                        .unwrap();
                }
                service.message_dispositions(&accepted.message_id)
            },
            || deadline,
        )
        .await
        .unwrap();

        assert_eq!(reads, 2, "a deadline crossing must force a final read");
        assert_eq!(
            dispositions[0].notification_state_raw,
            Some(NotificationState::BlockedPreWrite)
        );
        assert_eq!(
            dispositions[0].wake_block,
            Some(MessageWakeBlock::WorkerSupervisorExited)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn broadcast_heads_share_one_receipt_observation_deadline() {
        let (_scratch, service, events, reviewer, observer) = mailbox_service("shared-deadline", 8);
        let accepted = service
            .send(
                service.admin(),
                MailboxSend {
                    addresses: vec!["reviewer".into(), "observer".into()],
                    recipient_keys: None,
                    subject: "Broadcast".into(),
                    summary: None,
                    body: "Body".into(),
                    fyi: false,
                    client_key: None,
                    supersedes: None,
                    raw: false,
                },
            )
            .unwrap();
        let mut outcomes = HashMap::new();
        let mut contexts = Vec::new();
        for recipient in [reviewer, observer] {
            let record = service
                .prepare_oldest_notification(recipient)
                .unwrap()
                .unwrap();
            let context = NotificationContext::new_with_changes(
                service.store_handle(),
                record.message_id.clone(),
                recipient,
                record.attempt_id,
                service.change_publisher(),
            );
            context.record_gating().unwrap();
            outcomes.insert(
                recipient,
                RecipientScheduleOutcome::WorkerOwned {
                    head: ScheduledHead::new(record.message_id.clone(), record.attempt_id),
                    observe_first_disposition: true,
                },
            );
            contexts.push(context);
        }
        let receiver = events.subscribe();
        let started = Instant::now();
        let deadline = started + Duration::from_secs(10);

        let dispositions = observe_first_durable_dispositions(
            &service,
            &accepted.message_id,
            &outcomes,
            receiver,
            deadline,
            false,
        )
        .await
        .unwrap();
        assert_eq!(Instant::now() - started, Duration::from_secs(10));
        assert_eq!(dispositions.len(), 2);
        assert!(dispositions.iter().all(|disposition| {
            disposition.notification_state_raw == Some(NotificationState::Gating)
        }));
        drop(contexts);
    }
}
