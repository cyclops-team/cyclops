//! Coordinates the durable mailbox with the existing pane notification worker.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_proto::{
    AlarmClearResult, AlarmPreviewResult, AlarmSummary, ClaimDisposition, DeliveryReceipt,
    DeliveryState, InboxClaimResult, InboxListResult, InboxSummaryEntry, MessageId,
    MessageWakeBlock, MessagesFollowResult, MessagesSnapshotResult, MsgSendParams, MsgSendResult,
    NotificationAttemptId, NotificationAttentionCause, NotificationBinding, NotificationManifestId,
    NotificationPreWriteCause, NotificationPreWriteObservation, NotificationRecord,
    NotificationRouteEvidenceId, NotificationState, NotificationWithdrawDisposition,
    NotificationWithdrawResult, NotifyLevel, OpenDelivery, ProcessInstanceId, RecipientKey,
    StatusBlockedNotification, StatusMailboxRoute,
};
use cyclops_tmux::{PaneRow, SessionWatcher};
use tokio::time::Instant;
use tracing::{debug, error};

use crate::delivery;
use crate::mailbox::{
    AcceptResult, AttentionTarget, ClaimOutcome, MailboxError, MailboxIdentity, MailboxSend,
    MailboxService, MailboxServiceError, MessageStoreError, UnclaimedReminderQueue,
};
use crate::notification_adapter::NotificationContext;
use crate::{Inner, PaneKey};

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
    fn new(message_id: MessageId, attempt_id: NotificationAttemptId) -> Self {
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

    fn notification_pane(
        &self,
        service: &MailboxService,
        recipient: RecipientKey,
    ) -> Result<Option<String>, MailboxServiceError>;

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

    fn reconcile_claimed_recipient(&self, claimant: RecipientKey);

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
        self.effects.reconcile_claimed_recipient(claimant);
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
        self.effects.reconcile_claimed_recipient(record.recipient);
        self.effects.schedule_force_submit(record);
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

            WorkspaceMessagingStatus {
                mailbox_routes,
                admin_unread,
                mailbox_attention,
                blocked_notifications: blocked.rows,
                blocked_notifications_total: blocked.total,
                unread_by_recipient,
                projection_readable,
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

    /// Narrow handoff to the terminal-resolution mechanism. Ordinary callers
    /// never receive this service or an `AttentionTarget`.
    pub(crate) fn attention_service(&self) -> Arc<MailboxService> {
        Arc::clone(&self.service)
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
                        .notification_pane(&self.service, disposition.recipient)?;
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
    /// This operation never captures a pane, resolves a live route, schedules
    /// a delivery, or requeues a quota hold. It commits only the transitions
    /// justified by the supplied evidence and decides which explicit
    /// post-commit notices the daemon composition root must commit.
    pub(crate) fn apply_observation(
        &self,
        observation: crate::fusion::PaneMessagingObservation,
    ) -> Result<ObservationApplication, MailboxServiceError> {
        let crate::fusion::PaneMessagingObservation::QuotaResetObserved {
            recipient,
            session_idx,
            pane_id,
        } = observation;
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

/// Resolve a durable recipient only when its exact session instance is attached.
pub(crate) fn notification_route(
    inner: &Inner,
    service: &MailboxService,
    recipient: RecipientKey,
) -> Result<Option<NotificationRoute>, MailboxServiceError> {
    let (Some(session_instance_id), Some(pane_id)) =
        (recipient.session_instance_id(), recipient.pane_id())
    else {
        return Ok(None);
    };
    let pane_id = pane_id.to_string();
    let mut matched = None;
    for (session_idx, slot) in inner.active_session_slots() {
        let watcher = {
            let link = slot.link.lock().expect("session link lock");
            if !link.attached
                || link
                    .identity
                    .as_ref()
                    .map(|identity| identity.session_instance_id())
                    != Some(session_instance_id)
            {
                continue;
            }
            link.watcher.as_ref().map(Arc::clone)
        };
        let Some(watcher) = watcher else {
            continue;
        };
        let Some(row) = watcher.pane(&pane_id) else {
            continue;
        };
        let label = service
            .identity_for_recipient(recipient)?
            .map(|identity| identity.label)
            .unwrap_or_else(|| pane_id.clone());
        let route = NotificationRoute {
            session_idx,
            pane_id: pane_id.clone(),
            label,
            watcher,
            row,
        };
        if matched.replace(route).is_some() {
            return Ok(None);
        }
    }
    Ok(matched)
}

fn process_instance(process: crate::identity::ProcId) -> Option<ProcessInstanceId> {
    ProcessInstanceId::new(process.pid, process.birth).ok()
}

/// Capture the complete live binding that would authorize a terminal write.
///
/// A manifest pin alone is not enough. The selected rules and the process
/// ancestry must identify the same vendor before a blocked attempt can move.
fn proven_binding_observation(
    inner: &Inner,
    recipient: RecipientKey,
    route: &NotificationRoute,
    route_evidence: &NotificationRouteEvidenceId,
) -> Option<NotificationPreWriteObservation> {
    let Some(pane_root) =
        crate::identity::ProcId::of(route.row.pane_pid).and_then(process_instance)
    else {
        debug!(%recipient, pane = %route.pane_id, "route binding lacks a proven pane root");
        return None;
    };
    let adopted_root = inner
        .registry
        .lock()
        .expect("registry lock")
        .for_recipient(recipient)
        .and_then(|adoption| adoption.pane_root);
    if adopted_root != Some(pane_root) {
        debug!(
            %recipient,
            pane = %route.pane_id,
            ?pane_root,
            ?adopted_root,
            "route binding is not exact for this pane root"
        );
        return None;
    }
    let Some(binding) = crate::fusion::admitted_binding(inner, route.session_idx, &route.row)
    else {
        debug!(%recipient, pane = %route.pane_id, "route binding has no admitted vendor ancestry");
        return None;
    };
    let Some(selected) = crate::fusion::bind_manifest_for(inner, route.session_idx, &route.row)
    else {
        debug!(%recipient, pane = %route.pane_id, "route binding has no selected manifest");
        return None;
    };
    if binding.manifest != selected.agent.id {
        debug!(%recipient, pane = %route.pane_id, selected = %selected.agent.id, admitted = %binding.manifest, "route binding disagrees with the selected manifest");
        return None;
    }
    Some(NotificationPreWriteObservation {
        pane_root: Some(pane_root),
        selected_manifest: Some(NotificationManifestId::new(&selected.agent.id).ok()?),
        binding: Some(NotificationBinding {
            recipient,
            pane_root: Some(process_instance(binding.pane_root)?),
            leader: Some(process_instance(binding.leader)?),
            agent: process_instance(binding.agent)?,
            manifest: NotificationManifestId::new(binding.manifest).ok()?,
        }),
        route_evidence: Some(route_evidence.clone()),
        pane_width: Some(route.row.width),
        required_pane_width: None,
        write_block: None,
    })
}

/// Confirm that the cached composer verdict belongs to the same live agent
/// and manifest as the route observation used to reopen a blocked attempt.
fn route_is_write_ready(
    inner: &Inner,
    route: &NotificationRoute,
    observation: &NotificationPreWriteObservation,
) -> bool {
    let Some(binding) = observation.binding.as_ref() else {
        return false;
    };
    let (Some(pane_root), Some(leader)) = (binding.pane_root, binding.leader) else {
        return false;
    };
    let expected = crate::fusion::Binding {
        pane_root: crate::identity::ProcId {
            pid: pane_root.pid(),
            birth: pane_root.birth(),
        },
        leader: crate::identity::ProcId {
            pid: leader.pid(),
            birth: leader.birth(),
        },
        agent: crate::identity::ProcId {
            pid: binding.agent.pid(),
            birth: binding.agent.birth(),
        },
        manifest: binding.manifest.to_string(),
    };
    inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&PaneKey::new(route.session_idx, &route.pane_id))
        .is_some_and(|entry| cached_entry_is_write_ready(entry, route.row.in_mode, &expected))
}

fn cached_entry_is_write_ready(
    entry: &crate::DetEntry,
    pane_in_mode: bool,
    expected: &crate::fusion::Binding,
) -> bool {
    // This trusts the stamped write verdict; its composer proof already adjudicates sensor disagreement.
    !pane_in_mode
        && entry.detection.write_ready
        && !entry.detection.stale
        && !entry.in_mode
        && entry.binding.as_ref() == Some(expected)
}

/// Whether the already-computed pane verdict lets the worker decide now.
///
/// This is only a receipt-latency hint. The delivery worker still performs
/// every route, process, manifest, and terminal binding proof. A stale hint
/// can make the caller wait to its cap, but can never authorize a write.
/// `idle_with_input` is also immediately decidable: it is a conclusive
/// pre-write refusal, not permission to type.
fn cached_route_can_decide_now(inner: &Inner, route: &NotificationRoute) -> bool {
    if route.row.in_mode {
        return false;
    }
    inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&PaneKey::new(route.session_idx, &route.pane_id))
        .is_some_and(|entry| {
            !entry.in_mode
                && !entry.detection.stale
                // A disagreement means the picture is not settled enough for an early receipt,
                // even when the stamped write verdict has independently proved a clean composer.
                && !entry.detection.disagreement
                && (entry.detection.write_ready
                    || entry.detection.state == cyclops_proto::AgentState::IdleWithInput)
        })
}

fn enqueue_prepared_notification(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
    route: NotificationRoute,
    record: cyclops_proto::NotificationRecord,
    rerun_existing: bool,
) -> Result<RecipientScheduleOutcome, MailboxServiceError> {
    let head = ScheduledHead::new(record.message_id.clone(), record.attempt_id);
    let observe_first_disposition = cached_route_can_decide_now(inner, &route);
    let context = NotificationContext::new_with_changes(
        service.store_handle(),
        record.message_id,
        recipient,
        record.attempt_id,
        service.change_publisher(),
    );
    let attempt_id = context.attempt_id();
    match crate::delivery::enqueue_notification_attempt(
        inner,
        route.session_idx,
        &route.pane_id,
        &route.label,
        context.clone(),
        rerun_existing,
    ) {
        Ok(_) if inner.engine.notification_worker_owns(recipient, attempt_id) => {
            Ok(RecipientScheduleOutcome::WorkerOwned {
                head,
                observe_first_disposition,
            })
        }
        Ok(_) => park_unowned_notification(
            inner,
            &context,
            NotificationPreWriteCause::WorkerFailed,
            MessageWakeBlock::EnqueueRefused,
        ),
        Err(refusal) => {
            let (cause, block) = match refusal {
                crate::delivery::NotificationEnqueueRefusal::DaemonStopping => (
                    NotificationPreWriteCause::SessionUnavailable,
                    MessageWakeBlock::DaemonStopping,
                ),
                crate::delivery::NotificationEnqueueRefusal::WorkerFaulted => (
                    NotificationPreWriteCause::WorkerFailed,
                    MessageWakeBlock::WorkerFaulted,
                ),
                crate::delivery::NotificationEnqueueRefusal::WorkerSupervisorExited => (
                    NotificationPreWriteCause::WorkerFailed,
                    MessageWakeBlock::WorkerSupervisorExited,
                ),
                crate::delivery::NotificationEnqueueRefusal::PayloadUnavailable
                | crate::delivery::NotificationEnqueueRefusal::ClassificationUnavailable => (
                    NotificationPreWriteCause::PayloadUnavailable,
                    MessageWakeBlock::EnqueueRefused,
                ),
                crate::delivery::NotificationEnqueueRefusal::AttemptUnowned => (
                    NotificationPreWriteCause::WorkerFailed,
                    MessageWakeBlock::EnqueueRefused,
                ),
            };
            park_unowned_notification(inner, &context, cause, block)
        }
    }
}

fn park_unowned_notification(
    inner: &Inner,
    context: &NotificationContext,
    cause: NotificationPreWriteCause,
    block: MessageWakeBlock,
) -> Result<RecipientScheduleOutcome, MailboxServiceError> {
    record_unowned_notification(&inner.mailbox_publication, context, cause, block)
}

/// Publish the scheduler stop before exposing it to a sender.
fn record_unowned_notification(
    publication: &StdMutex<()>,
    context: &NotificationContext,
    cause: NotificationPreWriteCause,
    block: MessageWakeBlock,
) -> Result<RecipientScheduleOutcome, MailboxServiceError> {
    let head = ScheduledHead::new(context.message_id().clone(), context.attempt_id());
    let recorded = {
        let _publication = publication.lock().expect("mailbox publication lock");
        context
            .record_gating()
            .and_then(|_| context.record_pre_write_block_with_wake_block(cause, None, Some(block)))
    };
    recorded
        .map(|_| RecipientScheduleOutcome::Blocked { head, block })
        .map_err(|error| MailboxServiceError::NotificationSchedule(error.to_string()))
}

/// Schedule only the oldest pending entry for one durable recipient.
pub(crate) fn schedule_recipient(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
) -> Result<RecipientScheduleOutcome, MailboxServiceError> {
    let Some(record) = service.prepare_oldest_notification(recipient)? else {
        return Ok(service
            .notification_schedule_block(recipient)?
            .map(|blocked| RecipientScheduleOutcome::Blocked {
                head: ScheduledHead::new(blocked.message_id, blocked.attempt_id),
                block: blocked.block,
            })
            .unwrap_or(RecipientScheduleOutcome::NoWakeNeeded));
    };
    let context = NotificationContext::new_with_changes(
        service.store_handle(),
        record.message_id.clone(),
        recipient,
        record.attempt_id,
        service.change_publisher(),
    );
    if inner.engine.is_stopping() {
        return park_unowned_notification(
            inner,
            &context,
            NotificationPreWriteCause::SessionUnavailable,
            MessageWakeBlock::DaemonStopping,
        );
    }
    let Some(route) = notification_route(inner, service, recipient)? else {
        return park_unowned_notification(
            inner,
            &context,
            NotificationPreWriteCause::SessionUnavailable,
            MessageWakeBlock::RouteUnavailable,
        );
    };
    enqueue_prepared_notification(inner, service, recipient, route, record, false)
}

/// Reopen one blocked attempt under the publication lock.
///
/// The returned route and record are handed to the delivery engine only
/// after every registry and mailbox lock has been released.
fn prepare_recipient_after_route_evidence(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
    route_evidence: &NotificationRouteEvidenceId,
) -> Result<Option<(NotificationRoute, cyclops_proto::NotificationRecord)>, MailboxServiceError> {
    let Some(route) = notification_route(inner, service, recipient)? else {
        return Ok(None);
    };
    if inner.route_evidence_id(route.session_idx, &route.pane_id) != *route_evidence {
        return Ok(None);
    }
    let Some(observation) = proven_binding_observation(inner, recipient, &route, route_evidence)
    else {
        return Ok(None);
    };
    let write_ready = route_is_write_ready(inner, &route, &observation);
    let Some(confirmed_route) = notification_route(inner, service, recipient)? else {
        return Ok(None);
    };
    if confirmed_route.session_idx != route.session_idx || confirmed_route.pane_id != route.pane_id
    {
        return Ok(None);
    }
    let Some(confirmed) =
        proven_binding_observation(inner, recipient, &confirmed_route, route_evidence)
    else {
        return Ok(None);
    };
    if confirmed != observation
        || inner.route_evidence_id(route.session_idx, &route.pane_id) != *route_evidence
        || route_is_write_ready(inner, &confirmed_route, &confirmed) != write_ready
    {
        return Ok(None);
    }
    let Some(record) = service.reopen_oldest_notification_after_route_evidence(
        recipient,
        confirmed,
        write_ready,
    )?
    else {
        return Ok(None);
    };
    Ok(Some((route, record)))
}

fn schedule_recipient_after_route_evidence(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
    route_evidence: &NotificationRouteEvidenceId,
) -> Result<(), MailboxServiceError> {
    let prepared = {
        let _publication = inner
            .mailbox_publication
            .lock()
            .expect("mailbox publication lock");
        prepare_recipient_after_route_evidence(inner, service, recipient, route_evidence)?
    };
    if let Some((route, record)) = prepared {
        enqueue_prepared_notification(inner, service, recipient, route, record, true)?;
    }
    Ok(())
}

/// Reconcile one pane under the token minted by its causal event source.
pub(crate) fn schedule_route_evidence(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    route_evidence: &NotificationRouteEvidenceId,
) {
    if inner.route_evidence_id(session_idx, pane_id) != *route_evidence {
        return;
    }
    schedule_route_reconciliation_with_evidence(inner, session_idx, pane_id, route_evidence);
}

/// Reconcile current route state without inventing a new evidence edge.
///
/// Delivery uses this after a durable pre-write block so an edge that raced
/// ahead of the append is not lost. If no such edge occurred, the durable and
/// live observations retain the same identity and the pass is a no-op.
pub(crate) fn schedule_route_reconciliation(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) {
    let route_evidence = inner.route_evidence_id(session_idx, pane_id);
    schedule_route_reconciliation_with_evidence(inner, session_idx, pane_id, &route_evidence);
}

fn schedule_route_reconciliation_with_evidence(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    route_evidence: &NotificationRouteEvidenceId,
) {
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    let Some(recipient) = inner.recipient_key(session_idx, pane_id) else {
        return;
    };
    if let Err(error) = schedule_recipient(inner, service, recipient) {
        error!(%recipient, %error, "cannot schedule mailbox notification for changed route");
    }
    if let Err(error) =
        schedule_recipient_after_route_evidence(inner, service, recipient, route_evidence)
    {
        error!(%recipient, %error, "cannot reopen blocked mailbox notification");
    }
    crate::attention_resolution::schedule_exact_owned_reconciliation(inner, recipient);
}

/// Reconsider a durable width block after one actual pane-size edge.
///
/// Size-only events remain irrelevant to normal delivery and state fusion.
/// Once the exact blocked attempt reopens, later size events are no-ops.
pub(crate) fn schedule_pane_size_changed(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    route_evidence: &NotificationRouteEvidenceId,
) {
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    let Some(recipient) = inner.recipient_key(session_idx, pane_id) else {
        return;
    };
    match service.oldest_notification_has_width_block(recipient) {
        Ok(true) => {
            if let Err(error) =
                schedule_recipient_after_route_evidence(inner, service, recipient, route_evidence)
            {
                error!(%recipient, %error, "cannot reopen width-blocked mailbox notification");
            }
        }
        Ok(false) => {}
        Err(error) => {
            error!(%recipient, %error, "cannot inspect width-blocked mailbox notification");
        }
    }
}

/// Resume queued work after a route appears or a daemon restarts.
pub(crate) fn schedule_available(inner: &Arc<Inner>) {
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    let recipients = match service.pending_recipients() {
        Ok(recipients) => recipients,
        Err(error) => {
            error!(%error, "cannot inspect pending mailbox notifications");
            return;
        }
    };
    for recipient in recipients {
        if let Err(error) = schedule_recipient(inner, service, recipient) {
            error!(%recipient, %error, "cannot schedule mailbox notification");
        }
    }
}

/// Arm one exact-attempt, one-shot reminder after a proven doorbell.
///
/// The deadline wakes once. If the prior write barrier is still active, the
/// task waits only on durable workspace change events until that exact barrier
/// retires or the attempt becomes obsolete. It never polls the pane and never
/// invents a second readiness rule; the ordinary notification worker owns the
/// eventual stamped composer-safe gate.
pub(crate) fn schedule_unclaimed_reminder(
    inner: &Arc<Inner>,
    record: cyclops_proto::NotificationRecord,
) {
    let Some(threshold_ms) = inner.cfg.unclaimed_reminder_ms else {
        return;
    };
    if record.state != NotificationState::Notified
        || record.transport != cyclops_proto::NotificationTransport::Doorbell
        || record.unclaimed_reminder_count != 0
    {
        return;
    }
    let Some(service) = inner.mailbox.as_ref().map(Arc::clone) else {
        return;
    };
    let elapsed_ms = crate::unix_ms().saturating_sub(record.updated_at);
    let delay = Duration::from_millis(threshold_ms.saturating_sub(elapsed_ms));
    let attempt_id = record.attempt_id;
    let mut events = inner.events.subscribe();
    let task_inner = Arc::clone(inner);
    inner.engine.spawn_descendant_task(async move {
        tokio::time::sleep(delay).await;
        let first = service.queue_unclaimed_reminder(attempt_id);
        let outcome = match first {
            Ok(UnclaimedReminderQueue::WaitingForPriorBarrier) => {
                reconcile_due_unclaimed_reminder_barrier(
                    &task_inner,
                    &service,
                    record.recipient,
                    attempt_id,
                )
                .await;
                wait_and_queue_unclaimed_reminder(
                    &service,
                    attempt_id,
                    Duration::ZERO,
                    &mut events,
                )
                .await
            }
            Ok(UnclaimedReminderQueue::Queued(record)) => Ok(Some(*record)),
            Ok(UnclaimedReminderQueue::Obsolete) => Ok(None),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(Some(record)) => {
                if let Err(error) = schedule_recipient(&task_inner, &service, record.recipient) {
                    error!(attempt = %attempt_id, %error, "cannot schedule queued unclaimed reminder");
                }
            }
            Ok(None) => {}
            Err(error) => {
                error!(attempt = %attempt_id, %error, "cannot queue unclaimed reminder");
            }
        }
    });
}

/// One due reminder may ask the existing barrier one fresh question.
///
/// This is a named one-shot deadline, not a polling loop. Tracking the exact
/// attempt makes the standard recovery pass eligible to retire only that
/// barrier, and the forced capture still requires the recorded occupant plus
/// positive clean-composer evidence.
async fn reconcile_due_unclaimed_reminder_barrier(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
) {
    inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock")
        .track(attempt_id);
    let Ok(Some(route)) = notification_route(inner, service, recipient) else {
        return;
    };
    crate::observe_pane(
        inner,
        route.session_idx,
        &route.watcher,
        &route.pane_id,
        true,
        "unclaimed_reminder_due",
    )
    .await;
}

async fn wait_and_queue_unclaimed_reminder(
    service: &MailboxService,
    attempt_id: NotificationAttemptId,
    delay: Duration,
    events: &mut tokio::sync::broadcast::Receiver<cyclops_proto::Event>,
) -> Result<Option<cyclops_proto::NotificationRecord>, MailboxServiceError> {
    tokio::time::sleep(delay).await;
    loop {
        match service.queue_unclaimed_reminder(attempt_id)? {
            UnclaimedReminderQueue::Queued(record) => return Ok(Some(*record)),
            UnclaimedReminderQueue::Obsolete => return Ok(None),
            UnclaimedReminderQueue::WaitingForPriorBarrier => {}
        }
        match events.recv().await {
            Ok(event) if event.event == "messages.changed" => {}
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(None),
        }
    }
}

/// Re-arm pending exact reminders after journal replay.
pub(crate) fn schedule_unclaimed_reminders(inner: &Arc<Inner>) {
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    match service.unclaimed_reminder_candidates() {
        Ok(records) => {
            for record in records {
                schedule_unclaimed_reminder(inner, record);
            }
        }
        Err(error) => error!(%error, "cannot inspect unclaimed reminder candidates"),
    }
}

/// Arm the explicit post-paste escape hatch for one exact verify-failed
/// doorbell. Multiple callers may arm the same attempt; durable resolution
/// intent elects one key and makes every competing timer a no-op.
pub(crate) fn schedule_force_submit(inner: &Arc<Inner>, record: cyclops_proto::NotificationRecord) {
    if !record.needs_exact_owned_reconciliation() || !inner.force_submit.get().0 {
        return;
    }
    let Some(service) = inner.mailbox.as_ref().map(Arc::clone) else {
        return;
    };
    let task_inner = Arc::clone(inner);
    inner.engine.spawn_descendant_task(async move {
        loop {
            let (enabled, threshold_ms) = task_inner.force_submit.get();
            if !enabled {
                return;
            }
            let elapsed_ms = crate::unix_ms().saturating_sub(record.updated_at);
            let remaining_ms = threshold_ms.saturating_sub(elapsed_ms);
            if remaining_ms == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(remaining_ms)).await;
        }

        let pane_id = record
            .recipient
            .pane_id()
            .map(|pane_id| pane_id.to_string())
            .unwrap_or_default();
        let result = loop {
            let target = match service.attention_target(&record.attempt_id.to_string()) {
                Ok(target) => target,
                Err(_) => return,
            };
            match crate::attention_resolution::force_complete(&task_inner, &service, &target).await
            {
                Err(crate::attention_resolution::AttentionActionError::Store(error))
                    if error.notification_resolution_in_progress() =>
                {
                    // The ordinary exact-evidence worker may already own this
                    // attempt. It is bounded, and the forced path must wait
                    // for that safer decision instead of losing its timer to
                    // a transient in-memory reservation.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                result => break result,
            }
        };
        match result {
            Ok(_) => delivery::admin_notify(
                &task_inner,
                cyclops_proto::NotifyLevel::Fyi,
                "forced notification submit completed",
                &format!(
                    "message {} attempt {} used the configured post-paste escape hatch",
                    record.message_id, record.attempt_id
                ),
                Some(record.message_id.as_str()),
                None,
                delivery::About::pane(&pane_id),
            ),
            Err(crate::attention_resolution::AttentionActionError::ForceRefused(_))
            | Err(crate::attention_resolution::AttentionActionError::Store(_)) => None,
            Err(error) => delivery::admin_notify(
                &task_inner,
                cyclops_proto::NotifyLevel::ActionRequired,
                "forced notification submit remains uncertain",
                &format!(
                    "message {} attempt {} accepted no second key: {error}",
                    record.message_id, record.attempt_id
                ),
                Some(record.message_id.as_str()),
                None,
                delivery::About::pane(&pane_id),
            ),
        };
    });
}

/// Re-arm unresolved exact attempts after daemon replay or when the operator
/// enables or shortens the setting at runtime.
pub(crate) fn schedule_force_submit_candidates(inner: &Arc<Inner>) {
    if !inner.force_submit.get().0 {
        return;
    }
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    match service.force_submit_candidates() {
        Ok(records) => {
            for record in records {
                schedule_force_submit(inner, record);
            }
        }
        Err(error) => error!(%error, "cannot inspect force-submit candidates"),
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

/// Reconcile one exact doorbell barrier against a fresh post-claim screen.
///
/// A claim can move Submitted to Notified after its delivery handle has
/// retired. Tracking the exact attempt makes later pane observations eligible
/// for durable composer-barrier retirement. The forced capture closes the
/// no-output case where the composer was already clean when the claim landed.
/// Claim identity alone never retires the barrier: recovery still requires the
/// same bound occupant and manifest plus positive clean-composer evidence.
pub(crate) fn schedule_claimed_composer_observation(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
    attempt_id: cyclops_proto::NotificationAttemptId,
) -> Result<(), MailboxServiceError> {
    inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock")
        .track(attempt_id);
    let Some(route) = notification_route(inner, service, recipient)? else {
        return Ok(());
    };
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(());
    }
    let task_inner = Arc::clone(inner);
    let session_idx = route.session_idx;
    let pane_id = route.pane_id.clone();
    let watcher = Arc::clone(&route.watcher);
    inner.engine.spawn_descendant_task(async move {
        crate::observe_pane(
            &task_inner,
            session_idx,
            &watcher,
            &pane_id,
            true,
            "claimed_notification",
        )
        .await;
    });
    Ok(())
}

/// Reconcile the composer barrier after a late claim identifies an ACK timeout.
///
/// The delivery handle may still be retiring or already gone. Track the
/// durable attempt, enqueue a fresh recovery handle for the same FIFO owner,
/// then request one current screen reading. A stale handle cannot erase the
/// replacement because the attempt index is pointer-checked on retirement.
pub(crate) fn schedule_claimed_notification_recovery(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
    attempt_id: cyclops_proto::NotificationAttemptId,
) -> Result<(), MailboxServiceError> {
    inner
        .composer_recovery
        .lock()
        .expect("composer recovery lock")
        .track(attempt_id);
    let Some(route) = notification_route(inner, service, recipient)? else {
        return Ok(());
    };
    let Some(record) = service.prepare_oldest_notification(recipient)? else {
        return Ok(());
    };
    if record.attempt_id != attempt_id {
        error!(
            %recipient,
            expected_attempt = %attempt_id,
            found_attempt = %record.attempt_id,
            "claimed notification recovery lost FIFO ownership"
        );
        return Ok(());
    }
    let session_idx = route.session_idx;
    let pane_id = route.pane_id.clone();
    let watcher = Arc::clone(&route.watcher);
    enqueue_prepared_notification(inner, service, recipient, route, record, true)?;
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(());
    }
    let task_inner = Arc::clone(inner);
    inner.engine.spawn_descendant_task(async move {
        crate::observe_pane(
            &task_inner,
            session_idx,
            &watcher,
            &pane_id,
            true,
            "claimed_ack_timeout",
        )
        .await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::fusion::PaneMessagingObservation;

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
        let core = source
            .split_once("pub(crate) trait WorkspaceMessagingEffects")
            .expect("WorkspaceMessaging effects Interface")
            .1
            .split_once("fn alarm_summary(")
            .expect("end of WorkspaceMessaging operation Module")
            .0;

        for forbidden in ["Inner", "Weak<", "DaemonWorkspaceMessagingEffects"] {
            assert!(
                !core.contains(forbidden),
                "WorkspaceMessaging recovered daemon-root knowledge: {forbidden}"
            );
        }
        let daemon_root_impl = ["impl ", "Inner {"].concat();
        assert!(
            !source.contains(&daemon_root_impl),
            "WorkspaceMessaging construction returned to the operation module"
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedEffect {
        Subscribe,
        Schedule(RecipientKey),
        InvalidateUnread(RecipientKey),
        ResolvePane(RecipientKey),
        SettleClaim(NotificationAttemptId),
        ObserveClaimedComposer(RecipientKey, NotificationAttemptId),
        RecoverClaimedNotification(RecipientKey, NotificationAttemptId),
        CancelNotification(NotificationAttemptId),
        ReconcileClaimedRecipient(RecipientKey),
        ReconcileRouteEvidence(MessagingRouteEvidence),
        ReconcileCurrentRoute(usize, String),
        ScheduleUnclaimedReminder(NotificationAttemptId),
        ScheduleForceSubmit(NotificationAttemptId),
        ScheduleForceSubmitCandidates,
    }

    struct RecordingEffects {
        events: broadcast::Sender<Event>,
        calls: StdMutex<Vec<RecordedEffect>>,
    }

    impl RecordingEffects {
        fn new(events: broadcast::Sender<Event>) -> Self {
            Self {
                events,
                calls: StdMutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<RecordedEffect> {
            self.calls.lock().expect("acceptance calls lock").clone()
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

        fn notification_pane(
            &self,
            _service: &MailboxService,
            recipient: RecipientKey,
        ) -> Result<Option<String>, MailboxServiceError> {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ResolvePane(recipient));
            Ok(None)
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

        fn reconcile_claimed_recipient(&self, claimant: RecipientKey) {
            self.calls
                .lock()
                .expect("acceptance calls lock")
                .push(RecordedEffect::ReconcileClaimedRecipient(claimant));
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
                RecordedEffect::ReconcileClaimedRecipient(reviewer),
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
                RecordedEffect::ReconcileClaimedRecipient(reviewer),
                RecordedEffect::ScheduleForceSubmit(attention.attempt_id),
            ]
        );
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
            assert!(
                !source.contains("messaging::schedule_recipient("),
                "{name} recovered direct recipient scheduling knowledge"
            );
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
            assert!(
                !source.contains("messaging::schedule_"),
                "{name} recovered direct messaging-worker knowledge"
            );
        }
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
