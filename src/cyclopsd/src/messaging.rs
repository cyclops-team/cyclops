//! Coordinates the durable mailbox with the existing pane notification worker.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_proto::{
    AgentState, AlarmClearResult, AlarmPreviewResult, AlarmSummary, ClaimDisposition, ComposerHold,
    ComposerProof, ComposerSemantic, ComposerState, DeliveryReceipt, DeliveryState,
    InboxClaimResult, InboxListResult, InboxSummaryEntry, MessageId, MessageWakeBlock,
    MessagesFollowResult, MessagesSnapshotResult, MsgSendParams, MsgSendResult,
    NotificationAttemptId, NotificationAttentionCause, NotificationBarrierRetirementCause,
    NotificationBinding, NotificationManifestId, NotificationPreWriteCause,
    NotificationPreWriteObservation, NotificationRecord, NotificationResolution,
    NotificationResolutionConsumptionObservation, NotificationRouteEvidenceId, NotificationState,
    NotificationWithdrawDisposition, NotificationWithdrawResult, NotifyLevel, OpenDelivery,
    ProcessInstanceId, RecipientKey, StatusBlockedNotification, StatusMailboxRoute,
};
use cyclops_tmux::{PaneRow, SessionWatcher};
use tokio::time::Instant;
use tracing::{debug, error};

use crate::delivery;
#[cfg(test)]
use crate::mailbox::UnclaimedReminderQueue;
use crate::mailbox::{
    AcceptResult, AttentionConsumptionSignal, AttentionResolutionStart, AttentionTarget,
    ClaimOutcome, ExactOwnedRecoveryAction, MailboxError, MailboxIdentity, MailboxSend,
    MailboxService, MailboxServiceError, MessageStoreError,
};
use crate::notification_adapter::{NotificationAdapterError, NotificationContext};

pub(crate) struct NotificationRoute {
    pub(crate) session_idx: usize,
    pub(crate) pane_id: String,
    pub(crate) label: String,
    pub(crate) watcher: Arc<SessionWatcher>,
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

/// One explicit post-commit effect requested by an observation application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingAdminNotice {
    pub(crate) level: NotifyLevel,
    pub(crate) subject: String,
    pub(crate) body: String,
    pub(crate) message_id: MessageId,
    pub(crate) session_idx: usize,
    pub(crate) recipient_label: String,
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

/// One immutable authenticated hook observation for an exact attention
/// consumption candidate.
///
/// Hook handling proves the process and payload facts. `WorkspaceMessaging`
/// owns candidate lookup, exact durable-binding comparison, and the one-shot
/// consumption signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingAttentionConsumptionObservation {
    session_idx: usize,
    pane_id: String,
    recipient: RecipientKey,
    pane_root: crate::identity::ProcId,
    agent: crate::identity::ProcId,
    manifest: String,
    prompt: String,
    observed_at_ms: u64,
}

impl MessagingAttentionConsumptionObservation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_idx: usize,
        pane_id: impl Into<String>,
        recipient: RecipientKey,
        pane_root: crate::identity::ProcId,
        agent: crate::identity::ProcId,
        manifest: impl Into<String>,
        prompt: impl Into<String>,
        observed_at_ms: u64,
    ) -> Self {
        Self {
            session_idx,
            pane_id: pane_id.into(),
            recipient,
            pane_root,
            agent,
            manifest: manifest.into(),
            prompt: prompt.into(),
            observed_at_ms,
        }
    }
}

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
        NotificationState::Submitted | NotificationState::Notified => true,
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
            if actual == &expected
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

/// Boot-local facts supplied by the daemon composition adapter.
///
/// WorkspaceMessaging asks for these named capabilities while it still owns
/// the durable record. Callers never receive the record or recovery variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MessagingComposerRuntimeFacts {
    pub(crate) active_worker_owns: bool,
    pub(crate) clear_supported: bool,
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
    recovery_action: ExactOwnedRecoveryAction,
    runtime: MessagingComposerRuntimeFacts,
}

/// Opaque durable half of one composer-recovery observation.
///
/// Fusion may ask whether screen capture is required and later return current
/// terminal evidence, but it cannot inspect the journal records or recovery
/// coordinator that made the answer necessary.
pub(crate) struct MessagingComposerRecoveryProbe {
    records: Vec<NotificationRecord>,
    store_error: Option<&'static str>,
}

impl MessagingComposerRecoveryProbe {
    pub(crate) fn none() -> Self {
        Self {
            records: Vec::new(),
            store_error: None,
        }
    }

    pub(crate) fn store_unavailable() -> Self {
        Self {
            records: Vec::new(),
            store_error: Some("composer_recovery_store_unavailable"),
        }
    }

    pub(crate) fn is_recovering(&self) -> bool {
        !self.records.is_empty() || self.store_error.is_some()
    }
}

/// Immutable physical evidence supplied to durable composer recovery.
///
/// Process, screen, and manifest adapters prove these facts. The messaging
/// Module decides how they join to an active durable barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessagingComposerRecoveryObservation {
    pub(crate) binding: Option<NotificationBinding>,
    pub(crate) clean_composer: bool,
    pub(crate) legacy_composer_ready: bool,
}

/// Opaque recovery decision carried through one fusion transaction.
pub(crate) struct MessagingComposerRecoveryPlan {
    action: Option<crate::composer_recovery::RecoveryAction>,
    attempt_id: Option<NotificationAttemptId>,
    retired_attempt: Option<NotificationAttemptId>,
}

impl MessagingComposerRecoveryPlan {
    pub(crate) fn store_unavailable() -> Self {
        Self {
            action: Some(crate::composer_recovery::RecoveryAction::Hold(
                "composer_recovery_store_unavailable",
            )),
            attempt_id: None,
            retired_attempt: None,
        }
    }

    pub(crate) fn merge_without_module(
        self,
        base_hold: ComposerHold,
        owner: Option<String>,
        turn_running: bool,
    ) -> MessagingComposerBarrierUpdate {
        self.barrier_update(base_hold, owner, turn_running)
    }

    fn barrier_update(
        self,
        base_hold: ComposerHold,
        owner: Option<String>,
        turn_running: bool,
    ) -> MessagingComposerBarrierUpdate {
        let (hold, owner, clear_turn, refusal) = crate::composer_recovery::merge_barrier(
            self.action.as_ref(),
            self.retired_attempt,
            base_hold,
            owner,
            turn_running,
        );
        let recovered_hold = self.action.as_ref().map(|action| {
            if matches!(action, crate::composer_recovery::RecoveryAction::Restore(_))
                && hold == ComposerHold::StagedDuringTurn
                && !turn_running
            {
                ComposerHold::Staged
            } else {
                hold
            }
        });
        MessagingComposerBarrierUpdate {
            hold,
            owner,
            clear_turn,
            refusal,
            recovered_hold,
        }
    }
}

/// Finished runtime barrier update. No durable record or recovery variant
/// crosses the Module boundary.
pub(crate) struct MessagingComposerBarrierUpdate {
    pub(crate) hold: ComposerHold,
    pub(crate) owner: Option<String>,
    pub(crate) clear_turn: bool,
    pub(crate) refusal: Option<&'static str>,
    pub(crate) recovered_hold: Option<ComposerHold>,
}

/// One Module-owned decision for an elected exact-attention worker.
///
/// The runtime adapter owns task execution. It does not inspect the mailbox
/// projection, recovery policy, or boot-local election locks.
pub(crate) enum ExactAttentionWork {
    Retire,
    Recheck,
    Resolve {
        target: Box<AttentionTarget>,
        resolution: NotificationResolution,
    },
}

/// Boot-local consumption observation registered through WorkspaceMessaging.
///
/// Dropping the handle deterministically removes the candidate without
/// exposing the mailbox service to the terminal mechanism.
pub(crate) struct AttentionConsumptionRegistration {
    service: Arc<MailboxService>,
    attempt_id: NotificationAttemptId,
    signal: Arc<AttentionConsumptionSignal>,
}

impl AttentionConsumptionRegistration {
    pub(crate) fn signal(&self) -> Arc<AttentionConsumptionSignal> {
        Arc::clone(&self.signal)
    }
}

impl Drop for AttentionConsumptionRegistration {
    fn drop(&mut self) {
        self.service
            .unregister_attention_consumption_candidate(self.attempt_id);
    }
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

/// Durable transitions and requested effects produced by one observation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ObservationApplication {
    durable_messages: Vec<MessageId>,
    pub(crate) notices: Vec<MessagingAdminNotice>,
}

/// Stable operation failures for selecting or administering attention.
///
/// The socket adapter maps these outcomes to wire errors without inspecting
/// the mailbox projection or its lookup rules.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MessagingAttentionError {
    #[error("this operation requires the workspace administrator")]
    Denied,
    #[error("{message}")]
    Ambiguous {
        message: String,
        candidates: Vec<NotificationAttemptId>,
    },
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

        status.next_action = Some(composer_next_action(
            status.composer,
            candidate.record.state,
            status.message_state,
            candidate.recovery_action,
            candidate.runtime,
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

fn composer_next_action(
    composer: cyclops_proto::ComposerState,
    notification: NotificationState,
    message: Option<cyclops_proto::ComposerMessageState>,
    recovery_action: ExactOwnedRecoveryAction,
    runtime: MessagingComposerRuntimeFacts,
) -> cyclops_proto::ComposerNextAction {
    use cyclops_proto::{ComposerMessageState, ComposerNextAction, ComposerState};

    let exact_composer = matches!(
        composer,
        ComposerState::CyclopsNotificationStaged | ComposerState::CyclopsNotificationSubmitted
    );
    if exact_composer && notification == NotificationState::AttentionRequired {
        return match recovery_action {
            ExactOwnedRecoveryAction::Submit => ComposerNextAction::AutomaticSubmit,
            ExactOwnedRecoveryAction::Clear if runtime.clear_supported => {
                ComposerNextAction::AutomaticReconcile
            }
            ExactOwnedRecoveryAction::Reconcile => ComposerNextAction::AutomaticReconcile,
            ExactOwnedRecoveryAction::Ineligible
            | ExactOwnedRecoveryAction::Clear
            | ExactOwnedRecoveryAction::Inspect => ComposerNextAction::InspectAttention,
        };
    }
    if !exact_composer || !runtime.active_worker_owns {
        return operator_composer_next_action(Some(notification), true);
    }
    match (composer, notification, message) {
        (
            ComposerState::CyclopsNotificationStaged,
            NotificationState::Staged,
            Some(ComposerMessageState::Pending),
        ) => ComposerNextAction::AutomaticSubmit,
        (
            ComposerState::CyclopsNotificationStaged,
            NotificationState::Staged,
            Some(ComposerMessageState::Claimed),
        ) => ComposerNextAction::AutomaticReconcile,
        (
            ComposerState::CyclopsNotificationStaged | ComposerState::CyclopsNotificationSubmitted,
            NotificationState::Submitting | NotificationState::Submitted,
            _,
        ) => ComposerNextAction::AutomaticReconcile,
        _ => operator_composer_next_action(Some(notification), true),
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

    fn composer_runtime_facts(
        &self,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        manifest: Option<&NotificationManifestId>,
    ) -> MessagingComposerRuntimeFacts;

    fn settle_notification_claim(&self, attempt_id: NotificationAttemptId);

    fn observe_claimed_composer(
        &self,
        service: &Arc<MailboxService>,
        claimant: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<(), MailboxServiceError>;

    fn recover_claimed_notification(
        &self,
        service: &Arc<MailboxService>,
        claimant: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<(), MailboxServiceError>;

    fn cancel_notification(&self, attempt_id: NotificationAttemptId);

    fn spawn_exact_attention_worker(&self, attempt_id: NotificationAttemptId);

    fn reconcile_route_evidence(&self, evidence: MessagingRouteEvidence);

    fn reconcile_current_route(&self, session_idx: usize, pane_id: String);

    fn schedule_unclaimed_reminder(&self, record: NotificationRecord);

    fn schedule_force_submit(&self, record: NotificationRecord);

    fn schedule_force_submit_candidates(&self);

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
    composer_recovery: Arc<StdMutex<crate::composer_recovery::RecoveryCoordinator>>,
}

impl WorkspaceMessaging {
    #[cfg(test)]
    pub(crate) fn new(
        service: Arc<MailboxService>,
        publication: Arc<StdMutex<()>>,
        effects: Arc<dyn WorkspaceMessagingEffects>,
    ) -> Self {
        Self::new_with_recovery(
            service,
            publication,
            effects,
            Arc::new(StdMutex::new(
                crate::composer_recovery::RecoveryCoordinator::default(),
            )),
        )
    }

    pub(crate) fn new_with_recovery(
        service: Arc<MailboxService>,
        publication: Arc<StdMutex<()>>,
        effects: Arc<dyn WorkspaceMessagingEffects>,
        composer_recovery: Arc<StdMutex<crate::composer_recovery::RecoveryCoordinator>>,
    ) -> Self {
        Self {
            service,
            publication,
            effects,
            composer_recovery,
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

    /// Read the exact active durable barriers for one physical composer.
    pub(crate) fn composer_recovery_probe(
        &self,
        recipient: RecipientKey,
    ) -> MessagingComposerRecoveryProbe {
        let canonical = match self.service.active_notification_barriers() {
            Ok(records) => records,
            Err(_) => return MessagingComposerRecoveryProbe::store_unavailable(),
        };
        let mut recovery = self
            .composer_recovery
            .lock()
            .expect("composer recovery lock");
        let records = recovery.active_for_recipient(&canonical, recipient);
        let store_error = (recovery.writer_unknown() && !records.is_empty())
            .then_some("composer_recovery_reopen_required");
        MessagingComposerRecoveryProbe {
            records,
            store_error,
        }
    }

    /// Join current physical evidence to the exact durable recovery head and
    /// persist any immediately proven retirement.
    pub(crate) fn reconcile_composer_recovery(
        &self,
        probe: MessagingComposerRecoveryProbe,
        observation: MessagingComposerRecoveryObservation,
    ) -> MessagingComposerRecoveryPlan {
        let attempt_id = probe.records.first().map(|record| record.attempt_id);
        let exact_claim_after_write = match probe.records.as_slice() {
            [record] => self
                .service
                .exact_recipient_claimed_after_write(record)
                .unwrap_or(false),
            _ => false,
        };
        let legacy_claimed_clean = exact_claim_after_write && observation.legacy_composer_ready;
        let mut action = if let Some(reason) = probe.store_error {
            Some(crate::composer_recovery::RecoveryAction::Hold(reason))
        } else {
            self.composer_recovery
                .lock()
                .expect("composer recovery lock")
                .reconcile(
                    &probe.records,
                    observation.binding.as_ref(),
                    observation.clean_composer,
                    legacy_claimed_clean,
                )
        };
        let retired_attempt = match action.as_ref() {
            Some(retirement @ crate::composer_recovery::RecoveryAction::Retire { .. }) => {
                match self.persist_composer_recovery(retirement) {
                    Ok(attempt_id) => {
                        action = None;
                        Some(attempt_id)
                    }
                    Err(reason) => {
                        action = Some(crate::composer_recovery::RecoveryAction::Hold(reason));
                        None
                    }
                }
            }
            _ => None,
        };
        MessagingComposerRecoveryPlan {
            action,
            attempt_id,
            retired_attempt,
        }
    }

    /// Persist an exact post-restart lifecycle retirement when the physical
    /// adapter supplies the matching completed attempt.
    pub(crate) fn settle_composer_recovery_lifecycle(
        &self,
        mut plan: MessagingComposerRecoveryPlan,
        candidate: Option<NotificationAttemptId>,
    ) -> MessagingComposerRecoveryPlan {
        let Some(candidate) = candidate else {
            return plan;
        };
        if !matches!(
            plan.action,
            Some(crate::composer_recovery::RecoveryAction::Restore(attempt))
                if attempt == candidate
        ) {
            return plan;
        }
        let canonical = match self.service.active_notification_barriers() {
            Ok(records) => records,
            Err(_) => {
                plan.action = Some(crate::composer_recovery::RecoveryAction::Hold(
                    "composer_recovery_store_unavailable",
                ));
                return plan;
            }
        };
        if canonical.iter().any(|record| {
            record.attempt_id == candidate && record.needs_claimed_ack_timeout_reconciliation()
        }) {
            plan.action = Some(crate::composer_recovery::RecoveryAction::Hold(
                "claimed_notification_reconciliation_pending",
            ));
            return plan;
        }
        let record = match self
            .composer_recovery
            .lock()
            .expect("composer recovery lock")
            .reserve_record(&canonical, candidate)
        {
            Ok(Some(record)) => record,
            Ok(None) => {
                plan.action = None;
                return plan;
            }
            Err(reason) => {
                plan.action = Some(crate::composer_recovery::RecoveryAction::Hold(reason));
                return plan;
            }
        };
        let action = crate::composer_recovery::RecoveryAction::Retire {
            record: Box::new(record),
            cause: NotificationBarrierRetirementCause::LifecycleReconciled,
            replacement: None,
        };
        plan.action = match self.persist_composer_recovery(&action) {
            Ok(_) => None,
            Err(reason) => Some(crate::composer_recovery::RecoveryAction::Hold(reason)),
        };
        plan
    }

    /// Merge an opaque recovery decision into the runtime barrier while
    /// revalidating any concurrent durable retirement.
    pub(crate) fn merge_composer_recovery_barrier(
        &self,
        mut plan: MessagingComposerRecoveryPlan,
        base_hold: ComposerHold,
        owner: Option<String>,
        turn_running: bool,
    ) -> MessagingComposerBarrierUpdate {
        if matches!(
            plan.action,
            Some(crate::composer_recovery::RecoveryAction::Hold(
                "composer_recovery_retirement_pending"
            ))
        ) {
            plan.action = plan.attempt_id.and_then(|attempt_id| {
                self.composer_recovery
                    .lock()
                    .expect("composer recovery lock")
                    .retirement_pending_reason(attempt_id)
                    .map(crate::composer_recovery::RecoveryAction::Hold)
            });
        }
        plan.barrier_update(base_hold, owner, turn_running)
    }

    pub(crate) fn track_composer_recovery(&self, attempt_id: NotificationAttemptId) {
        self.composer_recovery
            .lock()
            .expect("composer recovery lock")
            .track(attempt_id);
    }

    pub(crate) fn composer_recovery_contains(&self, attempt_id: NotificationAttemptId) -> bool {
        self.composer_recovery
            .lock()
            .expect("composer recovery lock")
            .contains(attempt_id)
    }

    pub(crate) fn composer_barrier_retired(&self, attempt_id: NotificationAttemptId) {
        self.composer_recovery
            .lock()
            .expect("composer recovery lock")
            .retired(attempt_id);
    }

    pub(crate) fn retire_gone_composer_recipient(
        &self,
        recipient: RecipientKey,
    ) -> Result<(), &'static str> {
        self.retire_composer_recipient(
            recipient,
            NotificationBarrierRetirementCause::PaneGone,
            None,
        )
    }

    pub(crate) fn retire_replaced_composer_recipient(
        &self,
        recipient: RecipientKey,
        replacement: Option<NotificationBinding>,
    ) -> Result<(), &'static str> {
        self.retire_composer_recipient(
            recipient,
            NotificationBarrierRetirementCause::OccupantReplaced,
            replacement,
        )
    }

    fn retire_composer_recipient(
        &self,
        recipient: RecipientKey,
        cause: NotificationBarrierRetirementCause,
        replacement: Option<NotificationBinding>,
    ) -> Result<(), &'static str> {
        let records: Vec<_> = self
            .service
            .active_notification_barriers()
            .map_err(|_| "composer_recovery_store_unavailable")?
            .into_iter()
            .filter(|record| record.recipient == recipient)
            .collect();
        if records.is_empty() {
            return Ok(());
        }
        if cause == NotificationBarrierRetirementCause::OccupantReplaced && replacement.is_none() {
            return Err("composer_recovery_replacement_unproven");
        }
        if self
            .composer_recovery
            .lock()
            .expect("composer recovery lock")
            .writer_unknown()
        {
            return Err("composer_recovery_reopen_required");
        }
        for record in records {
            if let Err(error) =
                self.service
                    .retire_notification_barrier(&record, cause, replacement.clone())
            {
                if crate::composer_recovery::writer_requires_reopen(&error) {
                    self.composer_recovery
                        .lock()
                        .expect("composer recovery lock")
                        .require_reopen();
                }
                return Err("composer_recovery_retirement_failed");
            }
            self.composer_barrier_retired(record.attempt_id);
        }
        Ok(())
    }

    fn persist_composer_recovery(
        &self,
        action: &crate::composer_recovery::RecoveryAction,
    ) -> Result<NotificationAttemptId, &'static str> {
        let crate::composer_recovery::RecoveryAction::Retire {
            record,
            cause,
            replacement,
        } = action
        else {
            return Err("composer_recovery_not_a_retirement");
        };
        let attempt_id = record.attempt_id;
        let result = self
            .service
            .retire_notification_barrier(record, *cause, replacement.clone());
        let writer_unknown = result
            .as_ref()
            .is_err_and(crate::composer_recovery::writer_requires_reopen);
        let mut recovery = self
            .composer_recovery
            .lock()
            .expect("composer recovery lock");
        if result.is_ok() {
            recovery.retired(attempt_id);
        } else {
            recovery.retirement_failed(attempt_id);
            if writer_unknown {
                recovery.require_reopen();
            }
        }
        result
            .map(|()| attempt_id)
            .map_err(|_| "composer_recovery_retirement_failed")
    }

    /// Read the current directory and its matching daemon route publication as
    /// one transaction without exposing the synchronization mechanism.
    pub(crate) fn with_published<T>(&self, read: impl FnOnce(&Self) -> T) -> T {
        let _publication = self.publication.lock().expect("mailbox publication lock");
        read(self)
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
        let (withdrawn, consumed_doorbell, claimed_ack_timeout) = match &outcome {
            ClaimOutcome::Claimed {
                withdrawn_attempt,
                consumed_doorbell_attempt,
                claimed_ack_timeout_attempt,
                ..
            }
            | ClaimOutcome::AlreadyClaimed {
                withdrawn_attempt,
                consumed_doorbell_attempt,
                claimed_ack_timeout_attempt,
                ..
            } => (
                *withdrawn_attempt,
                *consumed_doorbell_attempt,
                *claimed_ack_timeout_attempt,
            ),
        };
        if let Some(attempt_id) = consumed_doorbell {
            self.effects.settle_notification_claim(attempt_id);
            if let Err(error) =
                self.effects
                    .observe_claimed_composer(&self.service, claimant, attempt_id)
            {
                error!(%claimant, %error, "cannot observe claimed notification composer");
            }
        }
        if let Some(attempt_id) = claimed_ack_timeout {
            if let Err(error) =
                self.effects
                    .recover_claimed_notification(&self.service, claimant, attempt_id)
            {
                error!(%claimant, %error, "cannot schedule claimed notification recovery");
            }
        }
        if let Some(attempt_id) = withdrawn {
            self.effects.cancel_notification(attempt_id);
        }
        self.exact_owned_evidence_changed(claimant);
        if claimed_ack_timeout.is_none() {
            if let Err(error) = self.effects.schedule_notification(&self.service, claimant) {
                error!(%claimant, %error, "cannot schedule mailbox notification after claim");
            }
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

    /// Continue one recipient FIFO after another durable path changed its
    /// current notification head.
    ///
    /// Delivery and terminal mechanisms report the committed outcome; they do
    /// not receive the mailbox service or choose the worker that follows it.
    pub(crate) fn notification_head_changed(
        &self,
        recipient: RecipientKey,
    ) -> Result<(), MailboxServiceError> {
        self.effects
            .schedule_notification(&self.service, recipient)
            .map(|_| ())
    }

    /// Apply the shared post-commit consequences of a direct mailbox delivery.
    pub(crate) fn direct_delivery_settled(
        &self,
        recipient: RecipientKey,
    ) -> Result<(), MailboxServiceError> {
        self.effects.invalidate_unread(recipient);
        self.notification_head_changed(recipient)
    }

    /// Apply one immutable route observation without exposing reconciliation
    /// or worker topology to the observer.
    pub(crate) fn route_evidence_observed(&self, evidence: MessagingRouteEvidence) {
        self.effects.reconcile_route_evidence(evidence);
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

    /// Apply post-commit policy for one durable attention record.
    pub(crate) fn notification_attention_recorded(&self, record: NotificationRecord) {
        if !record.needs_exact_owned_reconciliation() {
            return;
        }
        self.exact_owned_evidence_changed(record.recipient);
        self.effects.schedule_force_submit(record);
    }

    /// Apply one relevant composer or claim evidence edge.
    ///
    /// Candidate selection, exact-owned policy, and worker election stay
    /// inside the messaging Module. The runtime receives only elected attempt
    /// ids to execute.
    pub(crate) fn exact_owned_evidence_changed(&self, recipient: RecipientKey) {
        let candidates = match self.service.active_composer_notifications(recipient) {
            Ok(candidates) => candidates,
            Err(error) => {
                debug!(%recipient, %error, "cannot select exact-attention reconciliation work");
                return;
            }
        };
        for attempt_id in candidates
            .into_iter()
            .filter(|candidate| candidate.record.needs_exact_owned_reconciliation())
            .map(|candidate| candidate.record.attempt_id)
        {
            match self.service.request_exact_reconciliation(attempt_id) {
                Ok(true) => self.effects.spawn_exact_attention_worker(attempt_id),
                Ok(false) => {}
                Err(error) => debug!(%attempt_id, %error, "cannot elect exact-attention worker"),
            }
        }
    }

    /// Re-elect work parked behind an explicit resolution reservation.
    pub(crate) fn resume_exact_attention_reconciliation(&self, attempt_id: NotificationAttemptId) {
        match self.service.resume_exact_reconciliation(attempt_id) {
            Ok(true) => self.effects.spawn_exact_attention_worker(attempt_id),
            Ok(false) => {}
            Err(error) => debug!(%attempt_id, %error, "cannot resume exact-attention worker"),
        }
    }

    /// Consume one elected evidence edge and return only the exact action the
    /// runtime may attempt. Projection lookup and automatic policy stay here.
    pub(crate) fn next_exact_attention_work(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> ExactAttentionWork {
        match self.service.take_exact_reconciliation_request(attempt_id) {
            Ok(false) => return ExactAttentionWork::Retire,
            Ok(true) => {}
            Err(error) => {
                debug!(%attempt_id, %error, "cannot consume exact-attention work");
                return ExactAttentionWork::Retire;
            }
        }
        let target = match self.service.attention_target(&attempt_id.to_string()) {
            Ok(target) => target,
            Err(error) => {
                debug!(%attempt_id, %error, "exact-attention target is no longer actionable");
                return ExactAttentionWork::Recheck;
            }
        };
        match self.service.automatic_attention_resolution(&target) {
            Ok(Some(resolution)) => ExactAttentionWork::Resolve {
                target: Box::new(target),
                resolution,
            },
            Ok(None) => ExactAttentionWork::Recheck,
            Err(error) => {
                debug!(%attempt_id, %error, "cannot select exact-attention resolution");
                ExactAttentionWork::Recheck
            }
        }
    }

    /// Preserve an evidence edge that collided with an explicit resolver.
    /// True means the current runtime worker remains elected and should
    /// continue; false means the reservation owner will re-elect it later.
    pub(crate) fn park_exact_attention_after_conflict(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> bool {
        match self
            .service
            .park_exact_reconciliation_after_conflict(attempt_id)
        {
            Ok(continue_running) => continue_running,
            Err(error) => {
                debug!(%attempt_id, %error, "cannot park exact-attention work");
                false
            }
        }
    }

    /// Arm the optional reminder only for the first proven doorbell.
    pub(crate) fn notification_became_notified(&self, record: NotificationRecord) {
        if record.state == NotificationState::Notified
            && record.transport == cyclops_proto::NotificationTransport::Doorbell
            && record.unclaimed_reminder_count == 0
        {
            self.effects.schedule_unclaimed_reminder(record);
        }
    }

    /// Reconsider existing exact attention attempts after the operator enables
    /// force-submit. The server persists the setting; messaging owns the work
    /// that follows from it.
    pub(crate) fn force_submit_enabled(&self) {
        self.effects.schedule_force_submit_candidates();
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
                            let runtime = messaging.effects.composer_runtime_facts(
                                candidate.record.recipient,
                                candidate.record.attempt_id,
                                candidate
                                    .record
                                    .binding
                                    .as_ref()
                                    .map(|binding| &binding.manifest),
                            );
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
                                        recovery_action: candidate.recovery_action,
                                        runtime,
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

    /// Select one attention attempt for a read without exposing projection
    /// lookup or recipient-privacy policy to the requesting adapter.
    pub(crate) fn attention_for_show(
        &self,
        caller: RecipientKey,
        raw: &str,
    ) -> Result<AttentionTarget, MessagingAttentionError> {
        let target = match self.attention_target(raw) {
            Ok(target) => target,
            Err(_) if !caller.is_admin() => return Err(MessagingAttentionError::Denied),
            Err(error) => return Err(error),
        };
        if !caller.is_admin() && caller != target.record.recipient {
            return Err(MessagingAttentionError::Denied);
        }
        Ok(target)
    }

    /// Select one exact attention attempt for an administrator mutation.
    pub(crate) fn attention_for_resolution(
        &self,
        caller: RecipientKey,
        raw: &str,
    ) -> Result<AttentionTarget, MessagingAttentionError> {
        self.require_admin(caller)?;
        self.attention_target(raw)
    }

    /// Select one exact attempt for a Module-elected runtime action.
    pub(crate) fn attention_for_runtime(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<AttentionTarget, MessagingAttentionError> {
        self.attention_target(&attempt_id.to_string())
    }

    /// Resolve the current terminal route through the composition adapter.
    pub(crate) fn attention_terminal_route(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<NotificationRoute>, MailboxServiceError> {
        self.effects.notification_route(&self.service, recipient)
    }

    /// Rebuild the exact body-free terminal payload from the current durable
    /// message and attempt without exposing message lookup to terminal code.
    pub(crate) fn expected_attention_notification(
        &self,
        target: &AttentionTarget,
    ) -> Option<String> {
        let message = self.service.message_line(&target.record.message_id).ok()?;
        delivery::expected_notification_payload(&target.record, &message)
    }

    /// Register one exact post-dispatch consumption candidate. The returned
    /// handle owns deterministic cleanup.
    pub(crate) fn register_attention_consumption(
        &self,
        target: &AttentionTarget,
        session_idx: usize,
        pane_id: String,
        expected_payload: String,
        dispatch_started_ms: u64,
    ) -> Result<Option<AttentionConsumptionRegistration>, MailboxServiceError> {
        self.service
            .register_attention_consumption_candidate(
                target,
                session_idx,
                pane_id,
                expected_payload,
                dispatch_started_ms,
            )
            .map(|signal| {
                signal.map(|signal| AttentionConsumptionRegistration {
                    service: Arc::clone(&self.service),
                    attempt_id: target.record.attempt_id,
                    signal,
                })
            })
    }

    /// Match one authenticated hook observation to an exact registered
    /// attention candidate without exposing candidate storage or durable
    /// binding rules to the hook adapter.
    pub(crate) fn attention_consumption_observed(
        &self,
        observation: MessagingAttentionConsumptionObservation,
    ) -> bool {
        self.service.confirm_attention_consumption_hook(
            observation.session_idx,
            &observation.pane_id,
            observation.recipient,
            observation.pane_root,
            observation.agent,
            &observation.manifest,
            &observation.prompt,
            observation.observed_at_ms,
        )
    }

    /// Reserve one exact attention attempt and classify its durable recovery
    /// state before any terminal action.
    pub(crate) fn begin_attention_resolution(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<AttentionResolutionStart, MailboxServiceError> {
        self.service.begin_attention_resolution(target, resolution)
    }

    pub(crate) fn cancel_attention_resolution(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<(), MailboxServiceError> {
        self.service.cancel_attention_resolution(attempt_id)
    }

    pub(crate) fn force_submit_target_is_pending(
        &self,
        target: &AttentionTarget,
    ) -> Result<bool, MailboxServiceError> {
        self.service.force_submit_target_is_pending(target)
    }

    /// Commit the explicit operator or automatic resolution intent selected by
    /// the durable messaging projection.
    pub(crate) fn record_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
        requested: NotificationResolution,
        automatic: bool,
    ) -> Result<NotificationResolution, MailboxServiceError> {
        if automatic {
            self.service
                .record_automatic_attention_resolution_intent(target)
        } else {
            self.service
                .record_attention_resolution_intent(target, requested)?;
            Ok(requested)
        }
    }

    pub(crate) fn record_forced_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
    ) -> Result<(), MailboxServiceError> {
        self.service
            .record_forced_attention_resolution_intent(target)
    }

    pub(crate) fn record_attention_resolution_action_accepted(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        self.service
            .record_attention_resolution_action_accepted(target, resolution)
    }

    pub(crate) fn attention_claim_consumption(
        &self,
        target: &AttentionTarget,
    ) -> Result<Option<NotificationResolutionConsumptionObservation>, MailboxServiceError> {
        self.service.attention_claim_consumption(target)
    }

    pub(crate) fn record_attention_resolution_consumption_observed(
        &self,
        target: &AttentionTarget,
        observation: NotificationResolutionConsumptionObservation,
    ) -> Result<(), MailboxServiceError> {
        self.service
            .record_attention_resolution_consumption_observed(target, observation)
    }

    pub(crate) fn commit_attention_resolution(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        self.service.resolve_attention(target, resolution)
    }

    pub(crate) fn commit_attention_without_terminal_action(
        &self,
        target: &AttentionTarget,
    ) -> Result<(), MailboxServiceError> {
        self.service
            .resolve_attention_without_terminal_action(target)
    }

    /// Commit one proven pre-key refusal without conflating an append failure
    /// with the boot-local reservation release that follows it.
    pub(crate) fn withdraw_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        self.service
            .withdraw_attention_resolution_intent(target, resolution)
    }

    /// Release a durably withdrawn pre-key intent and continue its recipient
    /// FIFO. A scheduling failure does not make the durable withdrawal
    /// uncertain.
    pub(crate) fn finish_attention_intent_withdrawal(
        &self,
        target: &AttentionTarget,
    ) -> Result<(), MailboxServiceError> {
        self.service
            .cancel_attention_resolution(target.record.attempt_id)?;
        if let Err(error) = self.notification_head_changed(target.record.recipient) {
            error!(
                recipient = %target.record.recipient,
                %error,
                "cannot schedule mailbox notification after attention intent withdrawal"
            );
        }
        Ok(())
    }

    fn require_admin(&self, caller: RecipientKey) -> Result<(), MessagingAttentionError> {
        if caller.is_admin() {
            Ok(())
        } else {
            Err(MessagingAttentionError::Denied)
        }
    }

    fn attention_target(&self, raw: &str) -> Result<AttentionTarget, MessagingAttentionError> {
        match self.service.attention_target(raw) {
            Ok(target) => Ok(target),
            Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error))) => {
                if let MailboxError::AmbiguousAttentionTarget { candidates, .. } = error.as_ref() {
                    return Err(MessagingAttentionError::Ambiguous {
                        message: error.to_string(),
                        candidates: candidates.clone(),
                    });
                }
                Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)).into())
            }
            Err(error) => Err(error.into()),
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
    ) -> Result<MsgSendResult, MailboxServiceError> {
        let accepted = self
            .service
            .reply_with_summary(sender, reference, summary, body, client_key)?;
        self.finish_acceptance(accepted, false).await
    }

    /// Apply one committed pane observation to durable messaging truth.
    ///
    /// This operation never captures a pane or resolves a live route. It owns
    /// the durable and post-commit consequences justified by supplied evidence
    /// and decides which explicit notices the daemon composition root commits.
    pub(crate) fn apply_observation(
        &self,
        observation: crate::fusion::PaneMessagingObservation,
    ) -> Result<ObservationApplication, MailboxServiceError> {
        let (recipient, session_idx, pane_id) = match observation {
            crate::fusion::PaneMessagingObservation::RouteEvidenceObserved { evidence } => {
                self.route_evidence_observed(evidence);
                return Ok(ObservationApplication::default());
            }
            crate::fusion::PaneMessagingObservation::QuotaResetObserved {
                recipient,
                session_idx,
                pane_id,
            } => (recipient, session_idx, pane_id),
            crate::fusion::PaneMessagingObservation::ExactOwnedEvidenceChanged { recipient } => {
                self.exact_owned_evidence_changed(recipient);
                return Ok(ObservationApplication::default());
            }
        };
        let observed = self.service.observe_quota_reset(recipient)?;
        if observed.is_empty() {
            return Ok(ObservationApplication::default());
        }
        let label =
            quota_reset_recipient_label(self.service.identity_for_recipient(recipient), pane_id);
        let notices: Vec<_> = observed
            .iter()
            .map(|record| MessagingAdminNotice {
                level: NotifyLevel::ActionRequired,
                subject: format!("quota reset observed for {label}"),
                body: quota_reset_notice(&record.message_id),
                message_id: record.message_id.clone(),
                session_idx,
                recipient_label: label.clone(),
            })
            .collect();
        Ok(ObservationApplication {
            durable_messages: observed
                .into_iter()
                .map(|record| record.message_id)
                .collect(),
            notices,
        })
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

fn quota_reset_notice(message_id: &MessageId) -> String {
    format!("message {message_id} remains held; run `cyclops requeue {message_id}`")
}

/// Preserve the post-commit recovery cue even when current identity metadata
/// is absent or temporarily unreadable. The immutable observation's pane ID is
/// less friendly than a current label, but it still names the held recipient.
fn quota_reset_recipient_label(
    identity: Result<Option<MailboxIdentity>, MailboxServiceError>,
    pane_id: String,
) -> String {
    match identity {
        Ok(Some(identity)) => identity.label,
        Ok(None) => pane_id,
        Err(error) => {
            error!(%error, %pane_id, "cannot resolve quota-reset recipient label");
            pane_id
        }
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
    use crate::messaging_runtime::{
        cached_entry_is_write_ready, record_unowned_notification, wait_and_queue_unclaimed_reminder,
    };

    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use cyclops_proto::{
        scratch::scratch_dir, Event, NotificationAttentionCause, NotificationResolution,
        NotificationTransport, NotificationVerifyOutcome, SessionInstanceId, TmuxPaneId,
        WorkspaceId, DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM,
    };
    use cyclops_state::StateRoot;
    use tokio::sync::broadcast;

    use crate::mailbox::{MailboxDirectory, MessageStore};

    /// Syntactic architecture lint: the durable operation Module may request
    /// named effects, but its construction and daemon-root adapter belong to
    /// the composition root.
    #[test]
    fn workspace_messaging_core_cannot_recover_the_daemon_root() {
        let source = include_str!("messaging.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .expect("WorkspaceMessaging test boundary")
            .0;

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

    /// Syntactic architecture lint: runtime and observation adapters may
    /// supply physical evidence, but durable recovery records, variants, and
    /// coordinator state remain private to WorkspaceMessaging.
    #[test]
    fn composer_recovery_policy_cannot_leak_back_into_runtime_callers() {
        let fusion = include_str!("fusion.rs")
            .split_once("#[cfg(test)]")
            .expect("fusion test boundary")
            .0;
        for forbidden in [
            "composer_recovery::RecoveryAction",
            "composer_recovery::persist",
            "active_notification_barriers",
            "exact_recipient_claimed_after_write",
            ".composer_recovery",
        ] {
            assert!(
                !fusion.contains(forbidden),
                "fusion recovered durable composer policy: {forbidden}"
            );
        }

        let recovery = include_str!("composer_recovery.rs")
            .split_once("#[cfg(test)]")
            .expect("composer recovery test boundary")
            .0;
        for forbidden in ["inner.mailbox", ".composer_recovery\n"] {
            assert!(
                !recovery.contains(forbidden),
                "physical composer evidence recovered durable state: {forbidden}"
            );
        }

        for (adapter, source) in [
            ("delivery", include_str!("delivery.rs")),
            ("messaging runtime", include_str!("messaging_runtime.rs")),
            ("ack", include_str!("ack.rs")),
        ] {
            assert!(
                !source.contains(".composer_recovery"),
                "{adapter} reached into composer recovery coordinator state"
            );
        }
    }

    /// Syntactic architecture lint: fusion may identify immutable pane facts,
    /// but the composition root is the only handoff to messaging policy.
    #[test]
    fn pane_observation_cannot_apply_messaging_policy_directly() {
        let fusion = include_str!("fusion.rs")
            .split_once("#[cfg(test)]")
            .expect("fusion test boundary")
            .0;
        for forbidden in [
            ".exact_owned_evidence_changed(",
            ".route_evidence_observed(",
            "workspace_messaging()",
            ".composer_recovery_probe(",
            ".composer_projection_probe(",
            ".reconcile_composer_recovery(",
            ".settle_composer_recovery_lifecycle(",
            ".merge_composer_recovery_barrier(",
        ] {
            assert!(
                !fusion.contains(forbidden),
                "fusion applied messaging policy directly: {forbidden}"
            );
        }

        let recovery = include_str!("composer_recovery.rs")
            .split_once("#[cfg(test)]")
            .expect("composer recovery test boundary")
            .0;
        assert!(
            !recovery.contains("workspace_messaging()"),
            "physical composer evidence reached the messaging Module directly"
        );
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
                recovery_action: ExactOwnedRecoveryAction::Ineligible,
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
        let production = source.split_once("#[cfg(test)]").unwrap().0;
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
        ObserveClaimedComposer(RecipientKey, NotificationAttemptId),
        RecoverClaimedNotification(RecipientKey, NotificationAttemptId),
        CancelNotification(NotificationAttemptId),
        SpawnExactAttentionWorker(NotificationAttemptId),
        ReconcileRouteEvidence(MessagingRouteEvidence),
        ReconcileCurrentRoute(usize, String),
        ScheduleUnclaimedReminder(NotificationAttemptId),
        ScheduleForceSubmit(NotificationAttemptId),
        ScheduleForceSubmitCandidates,
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

        fn composer_runtime_facts(
            &self,
            _recipient: RecipientKey,
            _attempt_id: NotificationAttemptId,
            _manifest: Option<&NotificationManifestId>,
        ) -> MessagingComposerRuntimeFacts {
            MessagingComposerRuntimeFacts {
                active_worker_owns: true,
                clear_supported: true,
            }
        }

        fn settle_notification_claim(&self, attempt_id: NotificationAttemptId) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::SettleClaim(attempt_id));
        }

        fn observe_claimed_composer(
            &self,
            _service: &Arc<MailboxService>,
            claimant: RecipientKey,
            attempt_id: NotificationAttemptId,
        ) -> Result<(), MailboxServiceError> {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ObserveClaimedComposer(claimant, attempt_id));
            Ok(())
        }

        fn recover_claimed_notification(
            &self,
            _service: &Arc<MailboxService>,
            claimant: RecipientKey,
            attempt_id: NotificationAttemptId,
        ) -> Result<(), MailboxServiceError> {
            self.calls.lock().expect("acceptance calls lock").push(
                RecordedEffect::RecoverClaimedNotification(claimant, attempt_id),
            );
            Ok(())
        }

        fn cancel_notification(&self, attempt_id: NotificationAttemptId) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::CancelNotification(attempt_id));
        }

        fn spawn_exact_attention_worker(&self, attempt_id: NotificationAttemptId) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::SpawnExactAttentionWorker(attempt_id));
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

        fn schedule_unclaimed_reminder(&self, record: NotificationRecord) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ScheduleUnclaimedReminder(record.attempt_id));
        }

        fn schedule_force_submit(&self, record: NotificationRecord) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ScheduleForceSubmit(record.attempt_id));
        }

        fn schedule_force_submit_candidates(&self) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ScheduleForceSubmitCandidates);
        }

        fn receipt_block(&self) -> Duration {
            Duration::ZERO
        }
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
        context.record_quota_held().unwrap();
        assert_eq!(service.observe_quota_reset(reviewer).unwrap().len(), 1);

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

    // Obsolete if delivery or attention mechanisms regain the mailbox service
    // and directly choose how a settled head advances its recipient FIFO.
    #[test]
    fn workspace_messaging_owns_external_settlement_follow_up_order() {
        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-settlement-effects", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );

        messaging.notification_head_changed(reviewer).unwrap();
        messaging.direct_delivery_settled(reviewer).unwrap();

        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::Schedule(reviewer),
                RecordedEffect::InvalidateUnread(reviewer),
                RecordedEffect::Schedule(reviewer),
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
        messaging.force_submit_enabled();

        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::ReconcileRouteEvidence(route),
                RecordedEffect::ReconcileCurrentRoute(2, "%7".to_string()),
                RecordedEffect::ScheduleForceSubmitCandidates,
            ]
        );
    }

    // Obsolete if delivery interprets durable notification variants to choose
    // reminder, attention reconciliation, or force-submit scheduling.
    #[test]
    fn workspace_messaging_owns_durable_notification_follow_up_policy() {
        let (_scratch, service, events, _reviewer, _) =
            mailbox_service("workspace-messaging-notified-policy", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (_accepted, context, _) = queued_attempt(&service);
        let notified = record_notified_doorbell(&context);

        messaging.notification_became_notified(notified.clone());
        messaging.notification_became_notified(notified);

        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::ScheduleUnclaimedReminder(context.attempt_id()),
                RecordedEffect::ScheduleUnclaimedReminder(context.attempt_id()),
            ],
            "repeated mechanism calls remain safe because the runtime helper and durable queue are idempotent"
        );

        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-attention-policy", 8);
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
            .record_verify_attention(NotificationVerifyOutcome::ambiguous())
            .unwrap();

        messaging.notification_attention_recorded(attention.clone());

        assert_eq!(
            effects.calls(),
            vec![
                RecordedEffect::SpawnExactAttentionWorker(attention.attempt_id),
                RecordedEffect::ScheduleForceSubmit(attention.attempt_id),
            ]
        );
        match messaging.next_exact_attention_work(attention.attempt_id) {
            ExactAttentionWork::Resolve { target, resolution } => {
                assert_eq!(target.record.attempt_id, attention.attempt_id);
                assert_eq!(target.record.recipient, reviewer);
                assert_eq!(resolution, NotificationResolution::Complete);
            }
            ExactAttentionWork::Retire | ExactAttentionWork::Recheck => {
                panic!("elected exact-owned attempt did not produce its durable policy")
            }
        }
        assert!(matches!(
            messaging.next_exact_attention_work(attention.attempt_id),
            ExactAttentionWork::Retire
        ));
    }

    /// Syntactic architecture lint: the exact terminal mechanism performs one
    /// requested action. It cannot select projection candidates, manipulate
    /// election locks, or spawn messaging workers.
    #[test]
    fn attention_terminal_mechanism_cannot_recover_messaging_internals() {
        let compact: String = include_str!("attention_resolution.rs")
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        for forbidden in [
            "active_composer_notifications(",
            "request_exact_reconciliation(",
            "take_exact_reconciliation_request(",
            "automatic_attention_resolution(",
            "park_exact_reconciliation_after_conflict(",
            "resume_exact_reconciliation(",
            "spawn_descendant_task(",
            "Arc<MailboxService>",
            "&MailboxService",
            "service.",
            "messaging::notification_route(",
            "messaging_runtime::notification_route(",
            "register_attention_consumption_candidate(",
            "message_line(",
        ] {
            assert!(
                !compact.contains(forbidden),
                "terminal mechanism recovered messaging worker topology: {forbidden}"
            );
        }
    }

    /// Syntactic architecture lint: delivery and terminal mechanisms report a
    /// committed recipient change to WorkspaceMessaging; they cannot call the
    /// scheduler with a mailbox projection themselves.
    #[test]
    fn mechanisms_cannot_schedule_recipient_fifos_directly() {
        for (name, source) in [
            ("delivery", include_str!("delivery.rs")),
            (
                "attention resolution",
                include_str!("attention_resolution.rs"),
            ),
        ] {
            for forbidden in [
                "messaging::schedule_recipient(",
                "messaging_runtime::schedule_recipient(",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{name} recovered direct recipient scheduling knowledge through {forbidden}"
                );
            }
        }
    }

    /// Syntactic architecture lint: runtime callers publish evidence or invoke
    /// a named WorkspaceMessaging operation; only the composition adapter may
    /// choose one of the retained scheduling mechanisms.
    #[test]
    fn runtime_callers_cannot_schedule_messaging_work_directly() {
        for (name, source) in [
            ("fusion", include_str!("fusion.rs")),
            ("authenticated ACK", include_str!("ack.rs")),
            ("delivery", include_str!("delivery.rs")),
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

    /// Syntactic architecture lint: the authenticated hook adapter proves one
    /// immutable observation. Candidate storage and matching stay inside the
    /// messaging Module.
    #[test]
    fn authenticated_hook_cannot_access_messaging_internals() {
        let source = include_str!("ack.rs");
        let production = source
            .split_once("#[cfg(test)]\nmod tests")
            .expect("authenticated hook test boundary")
            .0;

        assert!(
            production.contains("MessagingAttentionConsumptionObservation::new"),
            "authenticated hook stopped publishing its immutable observation"
        );
        for forbidden in [
            "inner.mailbox",
            "confirm_attention_consumption_hook",
            "attention_consumption_candidates",
            "MailboxService",
        ] {
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
        let source = include_str!("delivery.rs");
        let production = source
            .split_once("#[cfg(test)]\nmod tests")
            .expect("delivery test boundary")
            .0;

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
    fn workspace_messaging_owns_alarm_projection_and_attention_selection_without_inner() {
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
            .record_verify_attention(NotificationVerifyOutcome::ambiguous())
            .unwrap();
        let admin = service.admin().key;

        assert!(matches!(
            messaging.alarm_preview(reviewer, 0, u64::MAX),
            Err(MessagingAttentionError::Denied)
        ));
        let preview = messaging.alarm_preview(admin, 0, u64::MAX).unwrap();
        assert_eq!(preview.entries.len(), 1);
        assert_eq!(preview.entries[0].id, attention.attempt_id.to_string());
        assert_eq!(
            preview.entries[0].cause,
            NotificationAttentionCause::VerifyFailed
        );

        let shown = messaging
            .attention_for_show(reviewer, &attention.attempt_id.to_string())
            .unwrap();
        assert_eq!(shown.record.attempt_id, attention.attempt_id);
        assert!(matches!(
            messaging.attention_for_show(observer, &attention.attempt_id.to_string()),
            Err(MessagingAttentionError::Denied)
        ));
        assert!(matches!(
            messaging.attention_for_show(observer, "att-00000000-0000-4000-8000-000000000099"),
            Err(MessagingAttentionError::Denied)
        ));
        assert!(matches!(
            messaging.attention_for_resolution(reviewer, &attention.attempt_id.to_string()),
            Err(MessagingAttentionError::Denied)
        ));
        assert_eq!(
            messaging
                .attention_for_resolution(admin, &attention.attempt_id.to_string())
                .unwrap()
                .record
                .attempt_id,
            attention.attempt_id
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

    // Obsolete if the terminal mechanism again appends or withdraws durable
    // resolution intent directly instead of asking WorkspaceMessaging.
    #[test]
    fn workspace_messaging_owns_attention_intent_and_pre_key_withdrawal() {
        let (scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-attention-commit", 8);
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
            .record_verify_attention(NotificationVerifyOutcome::ambiguous())
            .unwrap();
        let target = messaging
            .attention_for_resolution(service.admin().key, &attention.attempt_id.to_string())
            .unwrap();
        let journal_path = scratch
            .0
            .join("workspaces")
            .join("current")
            .join("messages.ndjson");
        let before_lines = fs::read_to_string(&journal_path).unwrap().lines().count();

        assert_eq!(
            messaging
                .begin_attention_resolution(&target, NotificationResolution::Complete)
                .unwrap(),
            AttentionResolutionStart::Fresh
        );
        assert_eq!(
            messaging
                .record_attention_resolution_intent(
                    &target,
                    NotificationResolution::Complete,
                    false,
                )
                .unwrap(),
            NotificationResolution::Complete
        );
        messaging
            .withdraw_attention_resolution_intent(&target, NotificationResolution::Complete)
            .unwrap();
        messaging
            .finish_attention_intent_withdrawal(&target)
            .unwrap();

        assert_eq!(
            fs::read_to_string(&journal_path).unwrap().lines().count(),
            before_lines + 2,
            "one intent and one withdrawal remain the complete durable pre-key trace"
        );
        assert_eq!(effects.calls(), vec![RecordedEffect::Schedule(reviewer)]);
        assert_eq!(
            messaging
                .begin_attention_resolution(&target, NotificationResolution::Complete)
                .unwrap(),
            AttentionResolutionStart::Fresh
        );
        messaging
            .cancel_attention_resolution(attention.attempt_id)
            .unwrap();
    }

    // Obsolete if terminal or hook code again reads message rows, matches
    // durable bindings, or owns boot-local consumption candidates.
    #[test]
    fn workspace_messaging_owns_attention_payload_and_consumption_registration() {
        let (_scratch, service, events, _reviewer, _) =
            mailbox_service("workspace-messaging-attention-support", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging =
            WorkspaceMessaging::new(Arc::clone(&service), Arc::new(StdMutex::new(())), effects);
        let (_accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        let attention = context
            .record_verify_attention(NotificationVerifyOutcome::ambiguous())
            .unwrap();
        let target = messaging
            .attention_for_runtime(attention.attempt_id)
            .unwrap();
        let expected = messaging
            .expected_attention_notification(&target)
            .expect("current message rebuilds its exact doorbell");
        let pane_id = target.record.recipient.pane_id().unwrap().to_string();

        let registration = messaging
            .register_attention_consumption(&target, 0, pane_id.clone(), expected.clone(), 0)
            .unwrap()
            .expect("exact-attempt doorbells register consumption");
        let signal = registration.signal();
        let binding = target
            .record
            .binding
            .as_ref()
            .expect("written doorbell retains its exact durable binding");
        let pane_root = binding
            .pane_root
            .expect("written doorbell retains its pane root");
        assert!(messaging.attention_consumption_observed(
            MessagingAttentionConsumptionObservation::new(
                0,
                pane_id.clone(),
                target.record.recipient,
                crate::identity::ProcId {
                    pid: pane_root.pid(),
                    birth: pane_root.birth(),
                },
                crate::identity::ProcId {
                    pid: binding.agent.pid(),
                    birth: binding.agent.birth(),
                },
                binding.manifest.as_str(),
                expected.clone(),
                1,
            ),
        ));
        assert_eq!(
            signal.observation(),
            Some(NotificationResolutionConsumptionObservation {
                evidence: cyclops_proto::NotificationResolutionConsumptionEvidence::ExactHookPrompt,
                observed_at_ms: 1,
            })
        );
        assert!(messaging
            .register_attention_consumption(&target, 0, pane_id.clone(), expected.clone(), 0)
            .is_err());
        drop(registration);
        let replacement = messaging
            .register_attention_consumption(&target, 0, pane_id, expected, 0)
            .unwrap()
            .expect("dropping the Module handle releases the exact candidate");
        drop(replacement);
    }

    /// Syntactic architecture lint: terminal code may prove and execute one
    /// exact action, but every durable resolution boundary belongs to the
    /// messaging Module.
    #[test]
    fn attention_terminal_mechanism_cannot_commit_messaging_state_directly() {
        let compact: String = include_str!("attention_resolution.rs")
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        for forbidden in [
            "service.begin_attention_resolution(",
            "service.cancel_attention_resolution(",
            "service.record_attention_resolution_intent(",
            "service.record_automatic_attention_resolution_intent(",
            "service.record_forced_attention_resolution_intent(",
            "service.record_attention_resolution_action_accepted(",
            "service.record_attention_resolution_consumption_observed(",
            "service.resolve_attention(",
            "service.resolve_attention_without_terminal_action(",
            "service.withdraw_attention_resolution_intent(",
        ] {
            assert!(
                !compact.contains(forbidden),
                "terminal mechanism recovered durable messaging mutation: {forbidden}"
            );
        }
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
        context.record_quota_held().unwrap();

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
            Some("quota_held")
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

    // Obsolete if fusion again commits quota-reset messaging state itself, or
    // if reset observation begins to requeue held work without operator action.
    #[test]
    fn workspace_messaging_owns_the_quota_reset_transition_and_notice_without_inner() {
        let (scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-quota-reset", 8);
        let effects = Arc::new(RecordingEffects::new(events));
        let messaging = WorkspaceMessaging::new(
            Arc::clone(&service),
            Arc::new(StdMutex::new(())),
            effects.clone(),
        );
        let (accepted, context, _) = queued_attempt(&service);
        context.record_gating().unwrap();
        context.record_quota_held().unwrap();

        let journal_path = scratch
            .0
            .join("workspaces")
            .join("current")
            .join("messages.ndjson");
        let before_lines = fs::read_to_string(&journal_path).unwrap().lines().count();
        let application = messaging
            .apply_observation(PaneMessagingObservation::quota_reset(reviewer, 2, "%7"))
            .unwrap();

        assert_eq!(
            application.durable_messages,
            vec![accepted.message_id.clone()]
        );
        assert_eq!(application.notices.len(), 1);
        assert_eq!(
            application.notices[0],
            MessagingAdminNotice {
                level: NotifyLevel::ActionRequired,
                subject: "quota reset observed for reviewer".to_string(),
                body: quota_reset_notice(&accepted.message_id),
                message_id: accepted.message_id.clone(),
                session_idx: 2,
                recipient_label: "reviewer".to_string(),
            }
        );
        assert!(effects.calls().is_empty());
        assert_eq!(
            fs::read_to_string(&journal_path).unwrap().lines().count(),
            before_lines + 1,
            "one observation appends one durable transition"
        );
        let disposition = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            disposition.notification_state_raw,
            Some(NotificationState::QuotaResetObserved)
        );
        assert!(
            service
                .prepare_oldest_notification(reviewer)
                .unwrap()
                .is_none(),
            "observation never requeues the held attempt"
        );

        let after_first = fs::read_to_string(&journal_path).unwrap().lines().count();
        let calls_after_first = effects.calls();
        let repeated = messaging
            .apply_observation(PaneMessagingObservation::quota_reset(reviewer, 2, "%7"))
            .unwrap();
        assert_eq!(repeated, ObservationApplication::default());
        assert_eq!(
            fs::read_to_string(&journal_path).unwrap().lines().count(),
            after_first
        );
        assert_eq!(effects.calls(), calls_after_first);
    }

    // Obsolete if fusion again invokes exact-owned messaging policy directly
    // instead of returning immutable evidence to the composition root.
    #[test]
    fn workspace_messaging_applies_an_exact_owned_pane_observation() {
        let (_scratch, service, events, reviewer, _) =
            mailbox_service("workspace-messaging-exact-owned-observation", 8);
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
            .record_verify_attention(NotificationVerifyOutcome::ambiguous())
            .unwrap();

        let application = messaging
            .apply_observation(PaneMessagingObservation::exact_owned_evidence_changed(
                reviewer,
            ))
            .unwrap();

        assert_eq!(application, ObservationApplication::default());
        assert_eq!(
            effects.calls(),
            vec![RecordedEffect::SpawnExactAttentionWorker(
                attention.attempt_id
            )]
        );
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

        let application = messaging
            .apply_observation(PaneMessagingObservation::route_evidence(evidence.clone()))
            .unwrap();

        assert_eq!(application, ObservationApplication::default());
        assert_eq!(
            effects.calls(),
            vec![RecordedEffect::ReconcileRouteEvidence(evidence)]
        );
    }

    #[test]
    fn a_directory_read_failure_cannot_suppress_the_quota_reset_recovery_cue() {
        assert_eq!(
            quota_reset_recipient_label(Err(MailboxServiceError::Poisoned), "%7".to_string()),
            "%7"
        );
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

    fn binding(leader_birth: u64) -> crate::fusion::Binding {
        crate::fusion::Binding {
            pane_root: crate::identity::ProcId {
                pid: 10,
                birth: 100,
            },
            leader: crate::identity::ProcId {
                pid: 20,
                birth: leader_birth,
            },
            agent: crate::identity::ProcId {
                pid: 30,
                birth: 300,
            },
            manifest: "claude".into(),
        }
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

    fn record_notified_doorbell(
        context: &NotificationContext,
    ) -> cyclops_proto::NotificationRecord {
        context.record_gating().unwrap();
        record_doorbell_write(context);
        context.record_staged().unwrap();
        assert_eq!(
            context.reserve_submit().unwrap(),
            crate::notification_adapter::SubmitReservation::Reserved
        );
        context.record_submitted().unwrap();
        context.record_notified().unwrap()
    }

    fn composer_runtime(
        active_worker_owns: bool,
        clear_supported: bool,
    ) -> MessagingComposerRuntimeFacts {
        MessagingComposerRuntimeFacts {
            active_worker_owns,
            clear_supported,
        }
    }

    #[test]
    fn workspace_messaging_owns_composer_recovery_policy() {
        use cyclops_proto::{
            ComposerMessageState, ComposerNextAction, ComposerState, NotificationState,
        };

        assert_eq!(
            composer_next_action(
                ComposerState::CyclopsNotificationStaged,
                NotificationState::Staged,
                Some(ComposerMessageState::Pending),
                ExactOwnedRecoveryAction::Ineligible,
                composer_runtime(true, false),
            ),
            ComposerNextAction::AutomaticSubmit
        );
        assert_eq!(
            composer_next_action(
                ComposerState::CyclopsNotificationStaged,
                NotificationState::Staged,
                Some(ComposerMessageState::Claimed),
                ExactOwnedRecoveryAction::Ineligible,
                composer_runtime(true, false),
            ),
            ComposerNextAction::AutomaticReconcile
        );
        assert_eq!(
            composer_next_action(
                ComposerState::CyclopsNotificationStaged,
                NotificationState::AttentionRequired,
                Some(ComposerMessageState::Pending),
                ExactOwnedRecoveryAction::Submit,
                composer_runtime(false, false),
            ),
            ComposerNextAction::AutomaticSubmit
        );
        assert_eq!(
            composer_next_action(
                ComposerState::CyclopsNotificationStaged,
                NotificationState::AttentionRequired,
                Some(ComposerMessageState::Pending),
                ExactOwnedRecoveryAction::Inspect,
                composer_runtime(false, false),
            ),
            ComposerNextAction::InspectAttention
        );
        assert_eq!(
            composer_next_action(
                ComposerState::CyclopsNotificationStaged,
                NotificationState::AttentionRequired,
                Some(ComposerMessageState::Claimed),
                ExactOwnedRecoveryAction::Clear,
                composer_runtime(false, true),
            ),
            ComposerNextAction::AutomaticReconcile
        );
        assert_eq!(
            composer_next_action(
                ComposerState::CyclopsNotificationStaged,
                NotificationState::AttentionRequired,
                Some(ComposerMessageState::Claimed),
                ExactOwnedRecoveryAction::Clear,
                composer_runtime(false, false),
            ),
            ComposerNextAction::InspectAttention
        );
        for message in [ComposerMessageState::Pending, ComposerMessageState::Claimed] {
            assert_eq!(
                composer_next_action(
                    ComposerState::CyclopsNotificationStaged,
                    NotificationState::Staged,
                    Some(message),
                    ExactOwnedRecoveryAction::Ineligible,
                    composer_runtime(false, false),
                ),
                ComposerNextAction::CheckHealth,
                "{message:?}"
            );
        }
        for state in [NotificationState::Submitting, NotificationState::Submitted] {
            assert_eq!(
                composer_next_action(
                    ComposerState::CyclopsNotificationStaged,
                    state,
                    Some(ComposerMessageState::Pending),
                    ExactOwnedRecoveryAction::Ineligible,
                    composer_runtime(true, false),
                ),
                ComposerNextAction::AutomaticReconcile,
                "{state:?}"
            );
            assert_eq!(
                composer_next_action(
                    ComposerState::CyclopsNotificationStaged,
                    state,
                    Some(ComposerMessageState::Pending),
                    ExactOwnedRecoveryAction::Ineligible,
                    composer_runtime(false, false),
                ),
                ComposerNextAction::CheckHealth,
                "{state:?}"
            );
        }
        for (state, expected) in [
            (NotificationState::Notified, ComposerNextAction::CheckHealth),
            (
                NotificationState::AttentionRequired,
                ComposerNextAction::InspectAttention,
            ),
            (
                NotificationState::WithdrawnAfterStaging,
                ComposerNextAction::CheckHealth,
            ),
        ] {
            assert_eq!(
                composer_next_action(
                    ComposerState::CyclopsNotificationStaged,
                    state,
                    Some(ComposerMessageState::Claimed),
                    ExactOwnedRecoveryAction::Ineligible,
                    composer_runtime(true, false),
                ),
                expected,
                "{state:?}"
            );
        }
        assert_eq!(
            composer_next_action(
                ComposerState::ComposerAmbiguous,
                NotificationState::Staged,
                Some(ComposerMessageState::Pending),
                ExactOwnedRecoveryAction::Ineligible,
                composer_runtime(true, false),
            ),
            ComposerNextAction::CheckHealth
        );
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
        context.record_staged().unwrap();
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
            recovery_action: active.recovery_action,
            runtime: composer_runtime(true, true),
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
        assert_eq!(exact.next_action, Some(ComposerNextAction::AutomaticSubmit));

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
    fn require_wake_waits_past_writing_and_staged_for_the_exact_attempt() {
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

        context.record_staged().unwrap();
        let staged = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert!(!has_first_durable_disposition(&staged, &head, true));

        context.reserve_submit().unwrap();
        context.record_submitted().unwrap();
        let submitted = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert!(has_first_durable_disposition(&submitted, &head, true));
    }

    #[test]
    fn cached_readiness_belongs_to_one_complete_process_binding() {
        let original = binding(200);
        let entry = crate::DetEntry {
            detection: cyclops_proto::Detection {
                state: cyclops_proto::AgentState::Idle,
                readings: Vec::new(),
                disagreement: false,
                decided_by: "fixture".into(),
                unknown_reason: None,
                stale: false,
                write_ready: true,
                write_block: None,
                composer_semantic: Some(cyclops_proto::ComposerSemantic::Clean),
            },
            binding: Some(original.clone()),
            manifest: Some("claude".into()),
            occupant: Some(20),
            agent: Some(original.agent),
            in_mode: false,
            quota_screen_clear: true,
            hold: cyclops_proto::ComposerHold::Clear,
            turn: None,
            hold_owner: None,
            composer: crate::ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        };

        assert!(cached_entry_is_write_ready(&entry, false, &original));
        assert!(
            !cached_entry_is_write_ready(&entry, false, &binding(201)),
            "a reused leader pid with a new generation cannot inherit readiness"
        );
    }

    #[test]
    fn cached_working_readiness_keeps_a_stamped_composer_proof() {
        let original = binding(200);
        let mut entry = crate::DetEntry {
            detection: cyclops_proto::Detection {
                state: cyclops_proto::AgentState::Working,
                readings: vec![
                    cyclops_proto::SensorReading {
                        sensor: cyclops_proto::Sensor::Screen,
                        state: cyclops_proto::AgentState::Idle,
                        rule: "composer_empty".into(),
                        ts: 1,
                    },
                    cyclops_proto::SensorReading {
                        sensor: cyclops_proto::Sensor::Title,
                        state: cyclops_proto::AgentState::Working,
                        rule: "title_working".into(),
                        ts: 1,
                    },
                ],
                disagreement: true,
                decided_by: "title_working".into(),
                unknown_reason: None,
                stale: false,
                write_ready: true,
                write_block: None,
                composer_semantic: Some(cyclops_proto::ComposerSemantic::Clean),
            },
            binding: Some(original.clone()),
            manifest: Some("claude".into()),
            occupant: Some(20),
            agent: Some(original.agent),
            in_mode: false,
            quota_screen_clear: true,
            hold: cyclops_proto::ComposerHold::Clear,
            turn: None,
            hold_owner: None,
            composer: crate::ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        };

        assert!(
            cached_entry_is_write_ready(&entry, false, &original),
            "a stamped Working + clean-composer verdict remains usable from the cache"
        );
        entry.detection.stale = true;
        assert!(!cached_entry_is_write_ready(&entry, false, &original));
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
    fn verify_failed_without_a_scheduler_fact_has_no_wake_block() {
        let (_scratch, service, _events, _recipient, _) =
            mailbox_service("verify-failed-no-wake-block", 8);
        let (accepted, context, _head) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        context
            .record_verify_attention(NotificationVerifyOutcome::ambiguous())
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
    fn ack_timeout_without_a_scheduler_fact_has_no_wake_block() {
        let (_scratch, service, _events, _recipient, _) =
            mailbox_service("ack-timeout-no-wake-block", 8);
        let (accepted, context, _head) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        context.record_staged().unwrap();
        context.reserve_submit().unwrap();
        context.record_submitted().unwrap();
        context
            .record_attention(NotificationAttentionCause::AckTimeout)
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
    fn quota_records_without_a_scheduler_fact_have_no_wake_block() {
        let (_scratch, service, _events, recipient, _) = mailbox_service("quota-no-wake-block", 8);
        let (accepted, context, _head) = queued_attempt(&service);
        context.record_gating().unwrap();
        context.record_quota_held().unwrap();

        let held = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            held.notification_state_raw,
            Some(NotificationState::QuotaHeld)
        );
        assert_eq!(held.wake_block, None);
        assert_eq!(receipt_from_disposition(held, None).wake_block, None);

        service.observe_quota_reset(recipient).unwrap();
        let reset = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            reset.notification_state_raw,
            Some(NotificationState::QuotaResetObserved)
        );
        assert_eq!(reset.wake_block, None);
        assert_eq!(receipt_from_disposition(reset, None).wake_block, None);
    }

    #[test]
    fn a_pending_exact_resolution_is_the_receipts_wake_block() {
        let (scratch, service, _events, recipient, _) =
            mailbox_service("pending-resolution-receipt", 8);
        let (accepted, context, _head) = queued_attempt(&service);
        context.record_gating().unwrap();
        record_doorbell_write(&context);
        let attention = context
            .record_verify_attention(NotificationVerifyOutcome::ambiguous())
            .unwrap();
        let target = service
            .attention_target(&attention.attempt_id.to_string())
            .unwrap();
        service
            .record_attention_resolution_intent(&target, NotificationResolution::Complete)
            .unwrap();
        let schedule_block = service
            .notification_schedule_block(recipient)
            .unwrap()
            .unwrap();
        assert_eq!(schedule_block.message_id, accepted.message_id);
        assert_eq!(schedule_block.attempt_id, attention.attempt_id);
        assert_eq!(
            schedule_block.block,
            MessageWakeBlock::AttentionResolutionPending
        );

        let disposition = service
            .message_dispositions(&accepted.message_id)
            .unwrap()
            .remove(0);
        assert_eq!(
            disposition.wake_block,
            Some(MessageWakeBlock::AttentionResolutionPending)
        );
        assert_eq!(
            receipt_from_disposition(disposition, None).wake_block,
            Some(MessageWakeBlock::AttentionResolutionPending)
        );

        let live = service.message_dispositions(&accepted.message_id).unwrap();
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
        assert_eq!(
            receipt_from_disposition(replayed[0].clone(), None).wake_block,
            Some(MessageWakeBlock::AttentionResolutionPending)
        );
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
    async fn a_due_reminder_waits_for_the_prior_barrier_then_queues_once() {
        let (_scratch, service, events, _recipient, _) = mailbox_service("reminder-barrier", 8);
        let (accepted, context, _) = queued_attempt(&service);
        let notified = record_notified_doorbell(&context);
        let attempt_id = notified.attempt_id;
        let mut receiver = events.subscribe();
        let wait_service = Arc::clone(&service);
        let waiter = tokio::spawn(async move {
            wait_and_queue_unclaimed_reminder(
                &wait_service,
                attempt_id,
                Duration::from_secs(10),
                &mut receiver,
            )
            .await
        });

        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the old write barrier must win");
        assert_eq!(
            service.message_dispositions(&accepted.message_id).unwrap()[0].notification_state_raw,
            Some(NotificationState::Notified)
        );

        service
            .retire_notification_barrier(
                &notified,
                cyclops_proto::NotificationBarrierRetirementCause::ComposerObservedClear,
                None,
            )
            .unwrap();
        let queued = waiter.await.unwrap().unwrap().unwrap();
        assert_eq!(queued.state, NotificationState::Gating);
        assert_eq!(queued.attempt_id, attempt_id);
        assert_eq!(queued.unclaimed_reminder_count, 1);
        assert_eq!(
            service.queue_unclaimed_reminder(attempt_id).unwrap(),
            UnclaimedReminderQueue::Obsolete
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_claim_obsoletes_the_exact_reminder_without_a_fact_or_terminal_io() {
        let (_scratch, service, events, recipient, _) = mailbox_service("reminder-claim", 8);
        let (accepted, context, _) = queued_attempt(&service);
        let notified = record_notified_doorbell(&context);
        let attempt_id = notified.attempt_id;
        let lines_before = service.journal_lines().unwrap().len();
        let mut receiver = events.subscribe();
        let wait_service = Arc::clone(&service);
        let waiter = tokio::spawn(async move {
            wait_and_queue_unclaimed_reminder(
                &wait_service,
                attempt_id,
                Duration::from_secs(10),
                &mut receiver,
            )
            .await
        });
        service.claim(recipient, accepted.message_id).unwrap();

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(waiter.await.unwrap().unwrap(), None);
        let lines = service.journal_lines().unwrap();
        assert_eq!(lines.len(), lines_before + 1, "only the claim appends");
        assert!(lines.iter().all(|line| {
            line.data
                .as_ref()
                .and_then(|data| data.get("type"))
                .and_then(|v| v.as_str())
                != Some("notification_unclaimed_reminder_queued")
        }));
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
