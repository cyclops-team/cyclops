//!
//! Append-only journal replay, durable message store, and sequence ordering.

use std::collections::BTreeSet;
use std::path::Path;

use cyclops_ledger::{now_ms, LedgerError, LedgerWriter};
use cyclops_proto::{
    doorbell_format_names_exact_attempt, Kind, LedgerLine, MailboxEntryState, MailboxFact,
    MessageId, MessageMetadata, MessagePresentation, MessageWakeBlock, NotificationAttemptId,
    NotificationAttentionCause, NotificationBarrierRetirementCause, NotificationBinding,
    NotificationFact, NotificationPreWriteCause, NotificationPreWriteObservation,
    NotificationRecord, NotificationRequeue, NotificationResolution,
    NotificationResolutionConsumptionObservation, NotificationState, NotificationTransport,
    NotificationVerifyOutcome, RecipientKey, WorkspaceId, CANONICAL_RECORD_VERSION,
    DOORBELL_FORMAT_ATTEMPT_CLAIM, DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM,
    DOORBELL_FORMAT_COMPACT_CLAIM, NOTIFICATION_RESOLUTION_PROOF_VERSION,
};
use cyclops_state::StateRoot;

use super::*;

/// Durable owner of one workspace journal and its in-memory projection.
pub struct MessageStore {
    pub(crate) writer: LedgerWriter,
    pub(crate) projection: MailboxProjection,
    pub(crate) fail_notification_recovery_append: Option<NotificationAttemptId>,
    #[cfg(test)]
    pub(crate) fail_batch_append: bool,
    #[cfg(test)]
    pub(crate) fail_claimed_staged_clear_appends: usize,
    #[cfg(test)]
    pub(crate) fail_claimed_ack_timeout_reconciliation_appends: usize,
    #[cfg(test)]
    pub(crate) fail_pre_write_block_appends: usize,
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

    pub(crate) fn accept_at(
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

    pub(crate) fn reply_at(
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

    pub(crate) fn accept_canonical_at(
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
    pub(crate) fn claim_notification_locator(
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

    pub(crate) fn claim_at(
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

    /// Replay only: no longer written since 1.1.0. Tests seed the fact an
    /// older daemon wrote for a direct payload delivery.
    #[cfg(test)]
    pub(crate) fn mark_delivered_direct_at(
        &mut self,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        ts: u64,
    ) -> Result<cyclops_proto::MailboxEntry, MessageStoreError> {
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
    pub(crate) fn append_notification_transition_at(
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
    pub(crate) fn append_notification_transition_with_transport_at(
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
    pub(crate) fn append_notification_transition_full_at(
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

    /// Replay only: no longer written since 1.1.0. Tests seed the fact an
    /// older daemon wrote when it spent a doorbell's one reminder allowance.
    #[cfg(test)]
    pub(crate) fn queue_unclaimed_reminder_at(
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

    pub(crate) fn requeue_notifications(
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

    pub(crate) fn requeue_notification_at(
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

    pub(crate) fn requeue_notifications_at(
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

    pub(crate) fn clear_notification_at(
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
    pub(crate) fn clear_notifications_at(
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

    pub(crate) fn resolve_notification(
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

    pub(crate) fn resolve_notification_without_terminal_action(
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

    pub(crate) fn record_notification_resolution_intent(
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

    pub(crate) fn record_forced_notification_resolution_intent(
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
    pub(crate) fn reserve_forced_notification_resolution_action(
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

    pub(crate) fn record_notification_resolution_intent_kind(
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

    pub(crate) fn record_notification_resolution_action_accepted(
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

    pub(crate) fn record_notification_resolution_consumption_observed(
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

    pub(crate) fn withdraw_notification_resolution_intent(
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

    pub(crate) fn append_notification_fact_at(
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
