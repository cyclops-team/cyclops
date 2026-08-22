//! Durable facts emitted by the existing delivery worker for a mailbox notification.

use std::sync::{Arc, Mutex as StdMutex};

use cyclops_proto::{
    LedgerLine, MailboxEntry, MessageId, MessagesChangedArea, NotificationAttemptId,
    NotificationAttentionCause, NotificationBinding, NotificationManifestId, NotificationRecord,
    NotificationState, NotificationTransport, ProcessInstanceId, RecipientKey,
};

use crate::mailbox::{MessageChangePublisher, MessageStore, MessageStoreError};

/// One durable notification attempt attached to a delivery handle.
#[derive(Clone)]
pub(crate) struct NotificationContext {
    store: Arc<StdMutex<MessageStore>>,
    message_id: MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    changes: Option<MessageChangePublisher>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NotificationAdapterError {
    #[error("notification store lock is poisoned")]
    StoreLockPoisoned,
    #[error("notification attempt was superseded before the pane write")]
    SupersededBeforeWrite,
    #[error("notification already resolved as {0:?}")]
    TerminalConflict(NotificationState),
    #[error(transparent)]
    Store(#[from] MessageStoreError),
    #[error("invalid notification binding: {0}")]
    InvalidBinding(String),
    #[error("notification message is missing from the workspace journal")]
    MessageMissing,
}

impl NotificationContext {
    #[allow(dead_code)]
    pub(crate) fn new(
        store: Arc<StdMutex<MessageStore>>,
        message_id: MessageId,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Self {
        Self {
            store,
            message_id,
            recipient,
            attempt_id,
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
        Self {
            store,
            message_id,
            recipient,
            attempt_id,
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

    pub(crate) fn message_line(&self) -> Result<LedgerLine, NotificationAdapterError> {
        self.store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?
            .projection()
            .get_message(&self.message_id)
            .cloned()
            .ok_or(NotificationAdapterError::MessageMissing)
    }

    pub(crate) fn record_gating(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let pending = store
            .projection()
            .get_entry(self.recipient, &self.message_id)
            .is_some_and(|entry| entry.state.is_pending());
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned();
        if !pending
            || current
                .as_ref()
                .is_none_or(|record| record.attempt_id != self.attempt_id)
        {
            return Err(NotificationAdapterError::SupersededBeforeWrite);
        }
        let current = current.expect("current attempt checked above");
        if current.state == NotificationState::Gating {
            return Ok(current);
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
        let pending = store
            .projection()
            .get_entry(self.recipient, &self.message_id)
            .is_some_and(|entry| entry.state.is_pending());
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .is_some_and(|record| {
                record.attempt_id == self.attempt_id && record.state == NotificationState::Gating
            });
        if !pending || !current {
            return Err(NotificationAdapterError::SupersededBeforeWrite);
        }
        Ok(())
    }

    /// Atomically recheck ownership and record the irreversible write boundary.
    pub(crate) fn record_writing(
        &self,
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
        let pending = store
            .projection()
            .get_entry(self.recipient, &self.message_id)
            .is_some_and(|entry| entry.state.is_pending());
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .is_some_and(|record| {
                record.attempt_id == self.attempt_id && record.state == NotificationState::Gating
            });
        if !pending || !current {
            return Err(NotificationAdapterError::SupersededBeforeWrite);
        }
        let binding = NotificationBinding {
            recipient: self.recipient,
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

    pub(crate) fn record_staged(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        self.advance(NotificationState::Staged, None, None)
    }

    pub(crate) fn record_submitted(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        self.advance(NotificationState::Submitted, None, None)
    }

    pub(crate) fn record_notified(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        self.record_terminal(NotificationState::Notified, None)
    }

    pub(crate) fn record_delivered_direct(
        &self,
    ) -> Result<(MailboxEntry, u64), NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let entry = store.mark_delivered_direct(
            self.message_id.clone(),
            self.recipient,
            self.attempt_id,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("direct disposition appended after its message");
        if let Some(publisher) = &self.changes {
            publisher.publish(
                seq,
                &[
                    MessagesChangedArea::Messages,
                    MessagesChangedArea::Mailboxes,
                ],
            );
        }
        Ok((entry, seq))
    }

    pub(crate) fn record_attention(
        &self,
        cause: NotificationAttentionCause,
    ) -> Result<NotificationRecord, NotificationAdapterError> {
        self.record_terminal(NotificationState::AttentionRequired, Some(cause))
    }

    pub(crate) fn record_quota_held(&self) -> Result<NotificationRecord, NotificationAdapterError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| NotificationAdapterError::StoreLockPoisoned)?;
        let current = store
            .projection()
            .notification(self.recipient, &self.message_id)
            .cloned()
            .ok_or(NotificationAdapterError::SupersededBeforeWrite)?;
        if current.attempt_id != self.attempt_id {
            return Err(NotificationAdapterError::SupersededBeforeWrite);
        }
        if current.state == NotificationState::QuotaHeld {
            return Ok(current);
        }
        if current.state != NotificationState::Gating {
            return Err(NotificationAdapterError::TerminalConflict(current.state));
        }
        self.advance_locked(&mut store, NotificationState::QuotaHeld, None, None)
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
            if current.attempt_id != self.attempt_id {
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

    fn publish_transition(&self, record: &NotificationRecord) {
        if let Some(publisher) = &self.changes {
            let changed = if matches!(
                record.state,
                NotificationState::AttentionRequired
                    | NotificationState::QuotaHeld
                    | NotificationState::QuotaResetObserved
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
