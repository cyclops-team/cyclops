//! Durable facts emitted by the existing delivery worker for a mailbox notification.

use std::sync::{Arc, Mutex as StdMutex};

use cyclops_proto::{
    LedgerLine, MailboxEntryState, MessageId, MessageMetadata, MessageWakeBlock,
    MessagesChangedArea, NotificationAttemptId, NotificationAttentionCause, NotificationBinding,
    NotificationManifestId, NotificationPreWriteCause, NotificationPreWriteObservation,
    NotificationRecord, NotificationState, NotificationTransport, ProcessInstanceId, RecipientKey,
    VerifiedBy,
};

use crate::mailbox::{MessageChangePublisher, MessageStore, MessageStoreError};

/// One durable notification attempt attached to a delivery handle.
#[derive(Clone)]
pub(crate) struct NotificationContext {
    store: Arc<StdMutex<MessageStore>>,
    message_id: MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    /// Durable execution generation for this exact attempt.
    run_epoch: u8,
    changes: Option<MessageChangePublisher>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NotificationAdapterError {
    #[error("notification store lock is poisoned")]
    StoreLockPoisoned,
    #[error("notification attempt is no longer current before the pane write")]
    NoLongerCurrentBeforeWrite,
    #[error("notification already resolved as {0:?}")]
    TerminalConflict(NotificationState),
    #[error(transparent)]
    Store(#[from] MessageStoreError),
    #[error("invalid notification binding: {0}")]
    InvalidBinding(String),
    #[error("notification message is missing from the workspace journal")]
    MessageMissing,
}

/// A doorbell or raw write may still go out after the recipient claimed the
/// message: the wake is for the operator's eyes and the claim is what the
/// recipient did. Only the retired direct payload was withdrawn by a claim.
fn entry_allows_notification(state: &MailboxEntryState, transport: NotificationTransport) -> bool {
    state.is_pending() || (state.is_claimed() && transport != NotificationTransport::DirectPayload)
}

impl NotificationContext {
    #[cfg(test)]
    pub(crate) fn new(
        store: Arc<StdMutex<MessageStore>>,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Self {
        let run_epoch = notification_run_epoch(&store, recipient, &message_id, attempt_id);
        Self {
            store,
            message_id,
            recipient,
            attempt_id,
            run_epoch,
            changes: None,
        }
    }

    pub(crate) fn new_with_changes(
        store: Arc<StdMutex<MessageStore>>,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
        changes: Option<MessageChangePublisher>,
    ) -> Self {
        let run_epoch = notification_run_epoch(&store, recipient, &message_id, attempt_id);
        Self {
            store,
            message_id,
            recipient,
            attempt_id,
            run_epoch,
            changes,
        }
    }

    pub(crate) fn message_id(&self) -> &MessageId {
        &self.message_id
    }

    pub(crate) fn attempt_id(&self) -> NotificationAttemptId {
        self.attempt_id
    }

    pub(crate) fn recipient(&self) -> RecipientKey {
        self.recipient
    }

    pub(crate) fn run_epoch(&self) -> u8 {
        self.run_epoch
    }

    /// Did the sender ask for a raw write? Read from the message's own
    /// metadata, so the choice travels with the message and not the attempt.
    pub(crate) fn raw_transport(&self) -> bool {
        self.message_line()
            .ok()
            .and_then(|line| line.data)
            .and_then(|data| serde_json::from_value::<MessageMetadata>(data).ok())
            .is_some_and(|metadata| metadata.raw)
    }

    pub(crate) fn message_line(&self) -> Result<LedgerLine, NotificationAdapterError> {
        self.store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?
            .projection()
            .get_message(&self.message_id)
            .cloned()
            .ok_or(NotificationAdapterError::MessageMissing)
    }

    pub(crate) fn current_record(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        self.store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned()
            .ok_or(NotificationAdapterError::NoLongerCurrentBeforeWrite)
    }

    pub(crate) fn record_gating(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned();
        let entry_allows = current.as_ref().is_some_and(|record| {
            store
                .projection()
                .get_entry(self.recipient, &self.message_id)
                .is_some_and(|entry| entry_allows_notification(&entry.state, record.transport))
        });
        if !entry_allows || current.as_ref().is_none_or(|record| !self.owns(record)) {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        let current = current.expect("current attempt checked above");
        if current.state == NotificationState::Gating {
            return Ok(current);
        }
        if matches!(
            current.state,
            NotificationState::Withdrawn
                | NotificationState::WithdrawnByOperator
                | NotificationState::Superseded
        ) {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        if current.state != NotificationState::Queued {
            return Err(NotificationAdapterError::TerminalConflict(current.state));
        }
        self.advance_locked(&mut store, NotificationState::Gating, None, None)
    }

    pub(crate) fn ensure_current_gating(&self) -> Result<(), NotificationAdapterError> {
        let store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .filter(|record| self.owns(record) && record.state == NotificationState::Gating);
        let entry_allows = current.is_some_and(|record| {
            store
                .projection()
                .get_entry(self.recipient, &self.message_id)
                .is_some_and(|entry| entry_allows_notification(&entry.state, record.transport))
        });
        if !entry_allows {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        Ok(())
    }

    /// Atomically recheck ownership and record the irreversible write boundary.
    pub(crate) fn record_writing(
        &self,
        pane_root: ProcessInstanceId,
        leader: ProcessInstanceId,
        agent: ProcessInstanceId,
        manifest: &str,
        transport: NotificationTransport,
        doorbell_format: Option<u32>,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let manifest = NotificationManifestId::new(manifest)
            .map_err(|error| NotificationAdapterError::InvalidBinding(error.to_string()))?;
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .filter(|record| self.owns(record) && record.state == NotificationState::Gating);
        let entry_allows = current.is_some_and(|_record| {
            store
                .projection()
                .get_entry(self.recipient, &self.message_id)
                .is_some_and(|entry| entry_allows_notification(&entry.state, transport))
        });
        if !entry_allows {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        let binding = NotificationBinding {
            recipient: self.recipient,
            pane_root: Some(pane_root),
            leader: Some(leader),
            agent,
            manifest,
        };
        let record = store.advance_notification_with_transport(
            self.message_id.clone(),
            self.recipient,
            self.attempt_id,
            NotificationState::Writing,
            binding,
            transport,
            doorbell_format,
        )?;
        self.publish_transition(&record);
        Ok(record)
    }

    /// The raw write boundary. No binding: nothing about the occupant was
    /// proven, and the record must not pretend otherwise.
    pub(crate) fn record_writing_raw(
        &self,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .filter(|record| self.owns(record) && record.state == NotificationState::Gating);
        let entry_allows = current.is_some_and(|_record| {
            store
                .projection()
                .get_entry(self.recipient, &self.message_id)
                .is_some_and(|entry| {
                    entry_allows_notification(&entry.state, NotificationTransport::Raw)
                })
        });
        if !entry_allows {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        let record = store.advance_notification_raw_writing(
            self.message_id.clone(),
            self.recipient,
            self.attempt_id,
        )?;
        self.publish_transition(&record);
        Ok(record)
    }

    pub(crate) fn record_submitted(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        self.advance(NotificationState::Submitted, None, None)
    }

    pub(crate) fn record_submitted_unverified(
        &self,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        self.advance(NotificationState::SubmittedUnverified, None, None)
    }

    /// The receipt, or the honest absence of one: `verified_by` is what
    /// proved the doorbell was consumed, and None says nothing did.
    pub(crate) fn record_notified(
        &self,
        verified_by: Option<VerifiedBy>,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        if let Some(current) = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned()
        {
            if !self.owns(&current) {
                return Err(NotificationAdapterError::TerminalConflict(current.state));
            }
            if current.state == NotificationState::Notified {
                return Ok(current);
            }
            if current.state.is_terminal() {
                return Err(NotificationAdapterError::TerminalConflict(current.state));
            }
        }
        let record = store.advance_notification_notified(
            self.message_id.clone(),
            self.recipient,
            self.attempt_id,
            verified_by,
        )?;
        self.publish_transition(&record);
        Ok(record)
    }

    /// Settle a successfully submitted doorbell when its exact mailbox entry
    /// has already been claimed.
    ///
    /// `Submitting` wins the terminal key. A concurrent pull claim still
    /// returns this message exactly once, while the reserved doorbell may
    /// submit the same message id. Only a successful key send reaches this
    /// method and advances the attempt to `Notified`.
    pub(crate) fn settle_submitted_claim(&self) -> Result<bool, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned()
            .ok_or(NotificationAdapterError::NoLongerCurrentBeforeWrite)?;
        if !self.owns(&current) {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        let claimed = store
            .projection()
            .get_entry(self.recipient, &self.message_id)
            .is_some_and(|entry| {
                matches!(
                    &entry.state,
                    MailboxEntryState::Claimed { claimant, .. } if *claimant == self.recipient
                )
            });
        if current.state == NotificationState::Notified {
            return Ok(claimed);
        }
        if (current.state != NotificationState::Submitted
            && current.state != NotificationState::SubmittedUnverified)
            || !claimed
        {
            return Ok(false);
        }
        self.advance_locked(&mut store, NotificationState::Notified, None, None)?;
        Ok(true)
    }

    pub(crate) fn record_attention(
        &self,
        cause: NotificationAttentionCause,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        self.record_terminal(NotificationState::AttentionRequired, Some(cause))
    }

    /// Stop this exact attempt after proving that no terminal write occurred.
    #[cfg(test)]
    pub(crate) fn record_pre_write_block(
        &self,
        cause: NotificationPreWriteCause,
        observation: Option<NotificationPreWriteObservation>,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        self.record_pre_write_block_with_wake_block(cause, observation, None)
    }

    /// Stop this attempt and retain why no scheduler worker owns its wake.
    pub(crate) fn record_pre_write_block_with_wake_block(
        &self,
        cause: NotificationPreWriteCause,
        observation: Option<NotificationPreWriteObservation>,
        wake_block: Option<MessageWakeBlock>,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned()
            .ok_or(NotificationAdapterError::NoLongerCurrentBeforeWrite)?;
        if !self.owns(&current) {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        if current.state == NotificationState::BlockedPreWrite {
            return Ok(current);
        }
        if current.state != NotificationState::Gating {
            return Err(NotificationAdapterError::TerminalConflict(current.state));
        }
        let record = store.block_notification_before_write_with_wake_block(
            self.message_id.clone(),
            self.recipient,
            self.attempt_id,
            cause,
            observation,
            wake_block,
        )?;
        self.publish_transition(&record);
        Ok(record)
    }

    /// Correct the durable write boundary after the transport proves that
    /// the paste command pipe accepted zero command bytes.
    pub(crate) fn record_paste_command_unwritten(
        &self,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned()
            .ok_or(NotificationAdapterError::NoLongerCurrentBeforeWrite)?;
        if !self.owns(&current) {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        if current.state == NotificationState::BlockedPreWrite
            && current.pre_write_cause == Some(NotificationPreWriteCause::PasteCommandUnwritten)
        {
            return Ok(current);
        }
        if current.state != NotificationState::Writing {
            return Err(NotificationAdapterError::TerminalConflict(current.state));
        }
        let record = store.block_notification_before_write(
            self.message_id.clone(),
            self.recipient,
            self.attempt_id,
            NotificationPreWriteCause::PasteCommandUnwritten,
            None,
        )?;
        self.publish_transition(&record);
        Ok(record)
    }

    fn record_terminal(
        &self,
        state: NotificationState,
        cause: Option<NotificationAttentionCause>,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        if let Some(current) = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned()
        {
            if !self.owns(&current) {
                return Err(NotificationAdapterError::TerminalConflict(current.state));
            }
            if current.state == state {
                return Ok(current);
            }
            if current.state.is_terminal() {
                return Err(NotificationAdapterError::TerminalConflict(current.state));
            }
        }
        self.advance_locked(&mut store, state, None, cause)
    }

    fn advance(
        &self,
        state: NotificationState,
        binding: Option<NotificationBinding>,
        cause: Option<NotificationAttentionCause>,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        self.advance_locked(&mut store, state, binding, cause)
    }

    fn advance_locked(
        &self,
        store: &mut MessageStore,
        state: NotificationState,
        binding: Option<NotificationBinding>,
        cause: Option<NotificationAttentionCause>,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .ok_or(NotificationAdapterError::NoLongerCurrentBeforeWrite)?;
        if !self.owns(current) {
            return Err(NotificationAdapterError::NoLongerCurrentBeforeWrite);
        }
        let record = store.advance_notification(
            self.message_id.clone(),
            self.recipient,
            self.attempt_id,
            state,
            binding,
            cause,
        )?;
        self.publish_transition(&record);
        Ok(record)
    }

    fn owns(&self, record: &NotificationRecord) -> bool {
        record.attempt_id == self.attempt_id && record.execution_epoch() == self.run_epoch
    }

    fn publish_transition(&self, record: &NotificationRecord) {
        if let Some(publisher) = &self.changes {
            let changed = if matches!(
                record.state,
                NotificationState::AttentionRequired | NotificationState::BlockedPreWrite
            ) {
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ][..]
            } else {
                &[MessagesChangedArea::Notifications][..]
            };
            publisher.publish(record.updated_seq, changed);
        }
    }
}

fn notification_run_epoch(
    store: &Arc<StdMutex<MessageStore>>,
    recipient: RecipientKey,
    message_id: &MessageId,
    attempt_id: NotificationAttemptId,
) -> u8 {
    store
        .lock()
        .ok()
        .and_then(|store| {
            store
                .projection()
                .notification(recipient, message_id)
                .filter(|record| record.attempt_id == attempt_id)
                .map(NotificationRecord::execution_epoch)
        })
        .unwrap_or(0)
}
