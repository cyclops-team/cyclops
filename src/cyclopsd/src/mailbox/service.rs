//!
//! Socket request handling, mutation publishing, and reconciliation.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use cyclops_ledger::now_ms;
use cyclops_proto::{
    Event, Kind, LedgerLine, MailboxListItem, MessageId, MessageNotificationState,
    MessagePresentation, MessageQuotaState, MessagesChangedArea, MessagesChangedData,
    MessagesFollowResult, MessagesSnapshotResult, NotificationAttemptId, NotificationPreWriteCause,
    NotificationPreWriteObservation, NotificationRecord, NotificationRequeue, NotificationState,
    RecipientKey, TmuxPaneId, WorkspaceId,
};
use tokio::sync::broadcast;

use super::*;

pub struct MailboxService {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) directory: RwLock<MailboxDirectory>,
    pub(crate) store: Arc<StdMutex<MessageStore>>,
    pub(crate) changes: Option<MessageChangePublisher>,
}

/// Publishes committed workspace changes while the store lock still orders them.
#[derive(Clone)]
pub(crate) struct MessageChangePublisher {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) events: broadcast::Sender<Event>,
}

impl MessageChangePublisher {
    pub(crate) fn new(workspace_id: WorkspaceId, events: broadcast::Sender<Event>) -> Self {
        Self {
            workspace_id,
            events,
        }
    }

    /// Call while holding the workspace store lock that ordered this append.
    pub(crate) fn publish(&self, workspace_seq: u64, changed: &[MessagesChangedArea]) {
        debug_assert!(!changed.is_empty());
        let data = MessagesChangedData {
            workspace_id: self.workspace_id,
            workspace_seq,
            changed: changed.iter().copied().collect::<BTreeSet<_>>(),
        };
        let _ = self.events.send(Event {
            event: "messages.changed".into(),
            data: serde_json::to_value(data).expect("messages.changed data serializes"),
            seq: Some(workspace_seq),
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MailboxServiceError {
    #[error(transparent)]
    Directory(#[from] MailboxDirectoryError),
    #[error(transparent)]
    Store(#[from] MessageStoreError),
    #[error("mailbox store lock is poisoned")]
    Poisoned,
    #[error("replacement mailbox directory belongs to another workspace")]
    ForeignDirectory,
    #[error("notification scheduler state could not be recorded: {0}")]
    NotificationSchedule(String),
    /// A body reaches an agent only through a claim; `msg.read` is the
    /// operator's read and refuses every other caller.
    #[error("bodies reach an agent only through a claim")]
    OperatorRequired,
}

impl From<MailboxError> for MailboxServiceError {
    fn from(error: MailboxError) -> Self {
        Self::Store(MessageStoreError::from(error))
    }
}

impl MailboxService {
    pub fn new(directory: MailboxDirectory, store: MessageStore) -> Self {
        Self::new_inner(directory, store, None)
    }

    pub(crate) fn new_with_events(
        directory: MailboxDirectory,
        store: MessageStore,
        events: broadcast::Sender<Event>,
    ) -> Self {
        let workspace_id = store.projection().workspace_id();
        debug_assert_eq!(directory.workspace_id(), workspace_id);
        Self::new_inner(
            directory,
            store,
            Some(MessageChangePublisher::new(workspace_id, events)),
        )
    }

    pub(crate) fn new_inner(
        directory: MailboxDirectory,
        store: MessageStore,
        changes: Option<MessageChangePublisher>,
    ) -> Self {
        Self {
            workspace_id: directory.workspace_id(),
            directory: RwLock::new(directory),
            store: Arc::new(StdMutex::new(store)),
            changes,
        }
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) fn seal(&self) -> Result<(), MailboxServiceError> {
        self.store()?
            .writer
            .seal()
            .map_err(MessageStoreError::from)?;
        Ok(())
    }

    pub fn admin(&self) -> MailboxIdentity {
        MailboxIdentity {
            key: RecipientKey::admin(self.workspace_id),
            label: "admin".to_string(),
        }
    }

    pub fn agent_for_pane(
        &self,
        pane: TmuxPaneId,
    ) -> Result<Option<MailboxIdentity>, MailboxServiceError> {
        Ok(self.directory()?.agent_for_pane(pane))
    }

    pub fn identity_for_recipient(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<MailboxIdentity>, MailboxServiceError> {
        Ok(self.directory()?.identity_for_recipient(recipient))
    }

    pub fn identity_for_address(
        &self,
        address: &str,
    ) -> Result<MailboxIdentity, MailboxServiceError> {
        if address == "*" {
            return Err(MailboxDirectoryError::UnknownRecipient(address.to_string()).into());
        }
        let mut resolved = self.directory()?.resolve(&[address.to_string()])?;
        Ok(resolved.remove(0))
    }

    pub fn routes(&self) -> Result<Vec<MailboxIdentity>, MailboxServiceError> {
        Ok(self.directory()?.routes())
    }

    pub fn replace_directory(
        &self,
        directory: MailboxDirectory,
    ) -> Result<(), MailboxServiceError> {
        if directory.workspace_id() != self.workspace_id {
            return Err(MailboxServiceError::ForeignDirectory);
        }
        *self
            .directory
            .write()
            .map_err(|_| MailboxServiceError::Poisoned)? = directory;
        Ok(())
    }

    pub fn send(
        &self,
        sender: MailboxIdentity,
        request: MailboxSend,
    ) -> Result<AcceptResult, MailboxServiceError> {
        self.send_after_resolution(sender, request, || {})
    }

    pub(crate) fn send_after_resolution(
        &self,
        sender: MailboxIdentity,
        request: MailboxSend,
        after_resolution: impl FnOnce(),
    ) -> Result<AcceptResult, MailboxServiceError> {
        let supersedes = request.supersedes.clone();
        // Keep this read guard through the append. A route replacement cannot
        // invalidate an exact recipient after validation but before acceptance.
        let directory = self.directory()?;
        let broadcast = request.recipient_keys.is_none() && request.addresses == ["*"];
        let recipients = match request.recipient_keys.as_deref() {
            Some(_) if !request.addresses.is_empty() => {
                return Err(MailboxDirectoryError::MixedRecipientSelectors.into());
            }
            Some(recipient_keys) => directory.resolve_recipient_keys(recipient_keys)?,
            None => directory.resolve(&request.addresses)?,
        };
        after_resolution();
        let draft = MessageDraft {
            kind: if request.fyi { Kind::Fyi } else { Kind::Msg },
            sender: sender.key,
            recipients: recipients.iter().map(|identity| identity.key).collect(),
            subject: Some(request.subject),
            summary: request.summary,
            body: (!request.body.is_empty()).then_some(request.body),
            client_key: request.client_key,
            supersedes: request.supersedes,
            raw: request.raw,
            broadcast,
            presentation: MessagePresentation {
                sender_label: sender.label,
                recipient_labels: recipients
                    .into_iter()
                    .map(|identity| cyclops_proto::RecipientPresentation {
                        recipient: identity.key,
                        label: identity.label,
                    })
                    .collect(),
            },
        };
        let mut store = self.store()?;
        let withdrew_notification = supersedes.as_ref().is_some_and(|message_id| {
            draft.recipients.first().is_some_and(|recipient| {
                store
                    .projection()
                    .notification(*recipient, message_id)
                    .is_some()
            })
        });
        // A replacement joins the thread of the message it supersedes; the
        // store checks the same supersession again under the same lock.
        let thread_root = store
            .projection()
            .supersession_thread_root(draft.sender, &draft.recipients, draft.supersedes.as_ref())
            .map_err(MessageStoreError::from)?;
        let message_id = store.mint_message_id(thread_root.as_ref());
        let accepted = store.accept(message_id, draft)?;
        if accepted.inserted {
            let changed = if withdrew_notification {
                &[
                    MessagesChangedArea::Messages,
                    MessagesChangedArea::Mailboxes,
                    MessagesChangedArea::Notifications,
                ][..]
            } else {
                &[
                    MessagesChangedArea::Messages,
                    MessagesChangedArea::Mailboxes,
                ][..]
            };
            self.publish_change(accepted.seq, changed);
        }
        Ok(accepted)
    }

    pub fn list(
        &self,
        recipient: RecipientKey,
        sender: Option<RecipientKey>,
        limit: Option<u32>,
    ) -> Result<Vec<MailboxListItem>, MailboxServiceError> {
        let mut entries = self
            .store()?
            .projection()
            .list_mailbox(recipient)
            .map_err(MessageStoreError::from)?;
        if let Some(sender) = sender {
            entries.retain(|entry| entry.sender == sender);
        }
        if let Some(limit) = limit {
            entries.truncate(limit as usize);
        }
        Ok(entries)
    }

    pub fn pending_count(&self, recipient: RecipientKey) -> Result<usize, MailboxServiceError> {
        Ok(self.store()?.projection().pending_count(recipient))
    }

    pub fn recipient_label(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<String>, MailboxServiceError> {
        Ok(self.store()?.projection().recipient_label(recipient))
    }

    pub(crate) fn pending_recipients(&self) -> Result<Vec<RecipientKey>, MailboxServiceError> {
        let store = self.store()?;
        let mut recipients: Vec<_> = store
            .projection()
            .mailboxes
            .iter()
            .filter_map(|(recipient, entries)| {
                (entries.values().any(|entry| entry.state.is_pending())
                    || store
                        .projection()
                        .claimed_operator_notification(*recipient)
                        .is_some())
                .then_some(*recipient)
            })
            .collect();
        recipients.sort();
        Ok(recipients)
    }

    /// The notification queue skips an operator-withdrawn wake while keeping
    /// its mailbox entry pending and pullable. Every notification scheduler
    /// path must make that same choice, otherwise a withdrawn head can hide a
    /// later blocked attempt forever.
    pub(crate) fn first_actionable_pending_message_id(
        store: &MessageStore,
        recipient: RecipientKey,
    ) -> Option<MessageId> {
        store
            .projection()
            .get_pending(recipient)
            .iter()
            .find(|entry| {
                !store
                    .projection()
                    .notification_withdrawn_by_operator(recipient, &entry.message_id)
            })
            .map(|entry| entry.message_id.clone())
    }

    /// Oldest mailbox entry whose pane doorbell has not reached a terminal
    /// result. A socket claim retrieves the body, but does not cancel its
    /// separate human-visible notification obligation.
    pub(crate) fn first_actionable_notification_message_id(
        store: &MessageStore,
        recipient: RecipientKey,
    ) -> Option<MessageId> {
        store
            .projection()
            .mailboxes
            .get(&recipient)?
            .values()
            .find(|entry| {
                if entry.state.is_pending() {
                    return !store
                        .projection()
                        .notification_withdrawn_by_operator(recipient, &entry.message_id);
                }
                if !entry.state.is_claimed() {
                    return false;
                }
                store
                    .projection()
                    .notification(recipient, &entry.message_id)
                    .is_some_and(|record| {
                        matches!(
                            record.state,
                            NotificationState::Queued
                                | NotificationState::Gating
                                | NotificationState::Writing
                                | NotificationState::Staged
                                | NotificationState::Submitting
                                | NotificationState::Submitted
                                | NotificationState::SubmittedUnverified
                        )
                    })
            })
            .map(|entry| entry.message_id.clone())
    }

    /// Queue or resume the oldest unnotified doorbell for one recipient.
    ///
    /// This method owns the atomic mailbox decision so concurrent sends
    /// cannot mint two attempts. The scheduler binds or explicitly blocks
    /// the returned attempt before reporting its wake outcome.
    pub(crate) fn prepare_oldest_notification(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<NotificationRecord>, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(None);
        }
        let mut store = self.store()?;
        if let Some(record) = store
            .projection()
            .claimed_operator_notification(recipient)
            .cloned()
        {
            return Ok(matches!(
                record.state,
                NotificationState::Queued | NotificationState::Gating
            )
            .then_some(record));
        }
        let Some(message_id) = Self::first_actionable_notification_message_id(&store, recipient)
        else {
            return Ok(None);
        };
        match store
            .projection()
            .notification(recipient, &message_id)
            .cloned()
        {
            None => {
                let record = store.queue_notification(
                    message_id,
                    recipient,
                    NotificationAttemptId::generate(),
                )?;
                self.publish_change(record.updated_seq, &[MessagesChangedArea::Notifications]);
                Ok(Some(record))
            }
            Some(record)
                if matches!(
                    record.state,
                    NotificationState::Queued | NotificationState::Gating
                ) =>
            {
                Ok(Some(record))
            }
            Some(_) => Ok(None),
        }
    }

    /// Explain a `prepare_oldest_notification` miss without inventing success.
    ///
    /// A normal empty/admin mailbox and an already visible or in-flight head
    /// need no scheduler warning. Durable attention and pre-write holds do.
    pub(crate) fn notification_schedule_block(
        &self,
        recipient: RecipientKey,
    ) -> Result<Option<NotificationScheduleBlock>, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(None);
        }
        let store = self.store()?;
        let Some(message_id) = Self::first_actionable_pending_message_id(&store, recipient) else {
            return Ok(None);
        };
        let Some(record) = store.projection().notification(recipient, &message_id) else {
            return Ok(None);
        };
        let block = notification_wake_block(
            record,
            store
                .projection()
                .attention_resolution_pending(record.attempt_id),
        );
        Ok(block.map(|block| NotificationScheduleBlock {
            message_id,
            attempt_id: record.attempt_id,
            block,
        }))
    }

    /// Reopen the oldest blocked attempt after exact route evidence changes.
    ///
    /// Ordinary mailbox changes never call this operation. The caller must
    /// supply one complete, live binding observation from the route that will
    /// receive the write. A readiness block additionally requires a positive
    /// current write-ready verdict. Repeated observations are no-ops, and the
    /// attempt id is retained so a route change cannot become an implicit
    /// requeue.
    pub(crate) fn reopen_oldest_notification_after_route_evidence(
        &self,
        recipient: RecipientKey,
        observation: NotificationPreWriteObservation,
        write_ready: bool,
    ) -> Result<Option<NotificationRecord>, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(None);
        }
        let mut store = self.store()?;
        let Some(message_id) = Self::first_actionable_pending_message_id(&store, recipient) else {
            return Ok(None);
        };
        let Some(current) = store
            .projection()
            .notification(recipient, &message_id)
            .cloned()
        else {
            return Ok(None);
        };
        let complete_binding = observation.binding.as_ref().is_some_and(|binding| {
            observation.pane_root.is_some()
                && binding.pane_root.is_some()
                && binding.leader.is_some()
                && observation.pane_root == binding.pane_root
                && binding.recipient == recipient
                && observation.selected_manifest.as_ref() == Some(&binding.manifest)
        });
        let cause_changed = match current.pre_write_cause {
            Some(
                NotificationPreWriteCause::SessionUnavailable
                | NotificationPreWriteCause::WorkerFailed,
            ) => complete_binding,
            Some(NotificationPreWriteCause::BindingUnprovable) => {
                current.pre_write_observation.as_ref().is_none_or(|prior| {
                    match (&prior.route_evidence, &observation.route_evidence) {
                        (Some(prior), Some(current)) => route_evidence_is_later(prior, current),
                        (None, Some(_)) => true,
                        (Some(_), None) => false,
                        (None, None) => {
                            prior.pane_root != observation.pane_root
                                || prior.selected_manifest != observation.selected_manifest
                                || prior.binding != observation.binding
                        }
                    }
                })
            }
            Some(NotificationPreWriteCause::WriteReadinessChanged) => {
                let later_route_evidence = current
                    .pre_write_observation
                    .as_ref()
                    .and_then(|prior| prior.route_evidence.as_ref())
                    .zip(observation.route_evidence.as_ref())
                    .is_some_and(|(prior, current)| route_evidence_is_later(prior, current));
                write_ready && later_route_evidence
            }
            _ => false,
        };
        if current.state != NotificationState::BlockedPreWrite
            || !complete_binding
            || !cause_changed
            || current.pre_write_reopen_count >= 1
        {
            return Ok(None);
        }
        let record = store.advance_notification(
            message_id,
            recipient,
            current.attempt_id,
            NotificationState::Gating,
            None,
            None,
        )?;
        self.publish_change(record.updated_seq, &[MessagesChangedArea::Notifications]);
        Ok(Some(record))
    }

    pub(crate) fn message_dispositions(
        &self,
        message_id: &MessageId,
    ) -> Result<Vec<MessageDisposition>, MailboxServiceError> {
        let store = self.store()?;
        let message = store.projection().get_message(message_id).ok_or_else(|| {
            MessageStoreError::from(MailboxError::MessageNotFound(message_id.clone()))
        })?;
        let metadata = extract_message_metadata(message).map_err(MessageStoreError::from)?;
        let (_, labels) = presentation_labels(&metadata.recipients, &metadata.presentation)
            .map_err(MessageStoreError::from)?;
        Ok(metadata
            .recipients
            .iter()
            .copied()
            .zip(labels)
            .map(|(recipient, label)| {
                let pending = store.projection().get_pending(recipient);
                let position_ahead = pending
                    .iter()
                    .filter(|entry| {
                        !store
                            .projection()
                            .notification_withdrawn_by_operator(recipient, &entry.message_id)
                    })
                    .position(|entry| &entry.message_id == message_id)
                    .and_then(|position| u32::try_from(position).ok());
                let record = store.projection().notification(recipient, message_id);
                let notification_state_raw = record.map(|record| record.state);
                let notification_state = record
                    .map(|record| record.state.into())
                    .unwrap_or(MessageNotificationState::NotStarted);
                let quota_state =
                    record.and_then(|record| MessageQuotaState::from_notification(record.state));
                let notification_settlement =
                    record.and_then(|record| store.projection().notification_settlement(record));
                MessageDisposition {
                    recipient,
                    label,
                    attempt_id: record.map(|record| record.attempt_id),
                    notification_state_raw,
                    notification_state,
                    quota_state,
                    notification_settlement,
                    pre_write_cause: record.and_then(|record| record.pre_write_cause),
                    wake_block: record.and_then(|record| {
                        notification_wake_block(
                            record,
                            store
                                .projection()
                                .attention_resolution_pending(record.attempt_id),
                        )
                    }),
                    position_ahead,
                }
            })
            .collect())
    }

    pub(crate) fn store_handle(&self) -> Arc<StdMutex<MessageStore>> {
        Arc::clone(&self.store)
    }

    #[doc(hidden)]
    pub(crate) fn inject_notification_recovery_append_failure(
        &self,
        attempt_id: NotificationAttemptId,
    ) {
        if let Ok(mut store) = self.store.lock() {
            store.inject_notification_recovery_append_failure(attempt_id);
        }
    }

    pub(crate) fn change_publisher(&self) -> Option<MessageChangePublisher> {
        self.changes.clone()
    }

    pub fn messages_snapshot(
        &self,
        caller: RecipientKey,
        recent_settled: u32,
    ) -> Result<MessagesSnapshotResult, MailboxServiceError> {
        // Availability is a current route observation. It is captured for
        // this answer but is not covered by the workspace journal sequence.
        let current_routes = self.directory()?.current_routes();
        self.store()?
            .projection()
            .messages_snapshot(caller, recent_settled.into(), &current_routes)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    pub fn messages_follow(
        &self,
        caller: RecipientKey,
        after_seq: u64,
        limit: u32,
    ) -> Result<MessagesFollowResult, MailboxServiceError> {
        let current_routes = self.directory()?.current_routes();
        self.store()?
            .projection()
            .messages_follow(caller, after_seq, limit, &current_routes)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    /// The full durable mailbox attention set `status` and
    /// `messages.snapshot` serve, from the same projection: open alarms and
    /// held queue heads in every held state, pre-write blocks included.
    pub(crate) fn mailbox_attention_rows(
        &self,
    ) -> Result<Vec<cyclops_proto::OpenDelivery>, MailboxServiceError> {
        let directory = self.directory()?;
        let labels: HashMap<RecipientKey, String> = directory
            .current_routes()
            .iter()
            .map(|(key, route)| (*key, route.label.clone()))
            .collect();
        Ok(self.store()?.projection().mailbox_attention_rows(&labels))
    }

    /// Bounded body-free pre-write failures for the operator status view.
    pub(crate) fn blocked_notification_snapshot(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<BlockedNotificationSnapshot, MailboxServiceError> {
        let current_routes = self.directory()?.current_routes();
        self.store()?
            .projection()
            .blocked_notification_snapshot(self.admin().key, &current_routes, now, limit)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    /// Read the authoritative workspace journal in append order.
    pub fn journal_lines(&self) -> Result<Vec<LedgerLine>, MailboxServiceError> {
        self.store()?
            .writer
            .read_after(0)
            .map_err(MessageStoreError::from)
            .map_err(Into::into)
    }

    /// Message identifiers owned by this workspace journal.
    ///
    /// Compatibility session journals may contain older copies of these
    /// messages. Restart recovery must not treat those copies as a second
    /// delivery authority.
    pub(crate) fn workspace_message_ids(
        &self,
    ) -> Result<std::collections::HashSet<String>, MailboxServiceError> {
        Ok(self
            .store()?
            .projection()
            .messages
            .keys()
            .map(|id| id.as_str().to_string())
            .collect())
    }

    /// All active composer barriers with message and mailbox facts from one
    /// projection read. Status groups this snapshot without rescanning it.
    pub(crate) fn active_composer_notifications_snapshot(
        &self,
    ) -> Result<Vec<ActiveComposerNotification>, MailboxServiceError> {
        let store = self.store()?;
        Ok(store
            .projection()
            .active_notification_barriers()
            .into_iter()
            .map(|record| ActiveComposerNotification {
                message: store.projection().get_message(&record.message_id).cloned(),
                entry_state: store
                    .projection()
                    .get_entry(record.recipient, &record.message_id)
                    .map(|entry| entry.state.clone()),
                record,
            })
            .collect())
    }

    /// Active composer barriers for one durable recipient, captured with the
    /// message and mailbox facts from the same projection read.
    pub(crate) fn active_composer_notifications(
        &self,
        recipient: RecipientKey,
    ) -> Result<Vec<ActiveComposerNotification>, MailboxServiceError> {
        Ok(self
            .active_composer_notifications_snapshot()?
            .into_iter()
            .filter(|candidate| candidate.record.recipient == recipient)
            .collect())
    }

    /// Content-free gate records for operational diagnostics.
    /// The current notification attempt for one message and recipient.
    pub(crate) fn notification_for_message(
        &self,
        recipient: RecipientKey,
        message_id: &MessageId,
    ) -> Result<Option<NotificationRecord>, MailboxServiceError> {
        Ok(self
            .store()?
            .projection()
            .notifications
            .get(&(recipient, message_id.clone()))
            .cloned())
    }

    pub(crate) fn gating_notifications(
        &self,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        Ok(self.store()?.projection().gating_notifications())
    }

    pub fn claim(
        &self,
        claimant: RecipientKey,
        message_id: MessageId,
    ) -> Result<ClaimOutcome, MailboxServiceError> {
        let mut store = self.store()?;
        let notification_changed = store
            .projection()
            .notification(claimant, &message_id)
            .map(|record| record.state.settled_by_claim(record.transport) != record.state)
            .unwrap_or_default();
        let outcome = store.claim(claimant, message_id)?;
        if let ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = &outcome
        {
            let seq = store
                .projection()
                .last_sequence()
                .expect("a fresh claim advances the workspace sequence");
            let changed = if notification_changed || withdrawn_attempt.is_some() {
                &[
                    MessagesChangedArea::Mailboxes,
                    MessagesChangedArea::Notifications,
                ][..]
            } else {
                &[MessagesChangedArea::Mailboxes][..]
            };
            self.publish_change(seq, changed);
        }
        Ok(outcome)
    }

    /// Claim a format 3 attempt locator or an imported legacy message id.
    pub(crate) fn claim_notification_locator(
        &self,
        claimant: RecipientKey,
        locator: MessageId,
        attempt_id: NotificationAttemptId,
    ) -> Result<ClaimOutcome, MailboxServiceError> {
        let mut store = self.store()?;
        let (outcome, notification_changed) =
            store.claim_notification_locator(claimant, locator, attempt_id)?;
        if let ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = &outcome
        {
            let seq = store
                .projection()
                .last_sequence()
                .expect("a fresh locator claim advances the workspace sequence");
            let changed = if notification_changed || withdrawn_attempt.is_some() {
                &[
                    MessagesChangedArea::Mailboxes,
                    MessagesChangedArea::Notifications,
                ][..]
            } else {
                &[MessagesChangedArea::Mailboxes][..]
            };
            self.publish_change(seq, changed);
        }
        Ok(outcome)
    }

    pub fn reply(
        &self,
        sender: MailboxIdentity,
        reference: MessageId,
        body: String,
        client_key: Option<String>,
    ) -> Result<AcceptResult, MailboxServiceError> {
        self.reply_with_summary(sender, reference, None, body, client_key, false)
    }

    pub fn reply_with_summary(
        &self,
        sender: MailboxIdentity,
        reference: MessageId,
        summary: Option<String>,
        body: String,
        client_key: Option<String>,
        raw: bool,
    ) -> Result<AcceptResult, MailboxServiceError> {
        // Reply routing is the referenced message's immutable sender key,
        // never its presentation label. Keep the current directory read
        // through the append so a rename preserves the route while a
        // replacement incarnation cannot enter between validation and
        // acceptance and inherit the predecessor's thread.
        let directory = self.directory()?;
        let mut store = self.store()?;
        let reference = if let Some(attempt_id) =
            cyclops_proto::parse_notification_attempt_claim_locator(&reference)
        {
            store
                .projection()
                .notification_by_attempt(attempt_id)
                .cloned()
                .map(|record| record.message_id)
                .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?
        } else {
            reference
        };
        let derived = store.projection().derive_reply(sender.key, &reference)?;
        let recipient = derived.recipient;
        let Some(destination) = directory.identity_for_recipient(recipient) else {
            return Err(MailboxDirectoryError::UnknownRecipient(recipient.to_string()).into());
        };
        // The reply's id carries the parent's thread.
        let message_id = store.mint_message_id(Some(&derived.thread_root));
        let accepted = store.reply(
            message_id,
            ReplyDraft {
                sender: sender.key,
                reference,
                summary,
                body: (!body.is_empty()).then_some(body),
                client_key,
                sender_label: sender.label,
                recipient_label: destination.label,
                raw,
            },
        )?;
        if accepted.inserted {
            self.publish_change(
                accepted.seq,
                &[
                    MessagesChangedArea::Messages,
                    MessagesChangedArea::Mailboxes,
                ],
            );
        }
        Ok(accepted)
    }

    /// Requeue every uncleared alarm on one message.
    ///
    /// Operator action only. The command mints one fresh attempt per
    /// recipient and appends one journal fact for the whole command.
    /// Nothing is written to a pane here and no retry is scheduled: each
    /// new attempt is queued and the delivery path picks it up on its own
    /// terms.
    pub fn requeue_message(
        &self,
        message_id: MessageId,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let mut store = self.store()?;
        // A message nobody has heard of and a message with nothing in
        // attention are different answers: the first is a mistake worth
        // reporting, the second is a quiet success.
        if store.projection().get_message(&message_id).is_none() {
            return Err(MessageStoreError::from(MailboxError::MessageNotFound(message_id)).into());
        }
        // Resolve every semantic target before appending anything, the way
        // clear_alarms does. Appending inside the loop and aborting on a
        // later refusal leaves some recipients holding new attempt ids
        // while the operator is told the whole requeue failed, and the
        // ids they were shown are gone with no successful reply naming
        // the replacements.
        let projection = store.projection();
        // An alarm whose entry has been claimed or superseded cannot be
        // redelivered. It stays visible and stays clearable; it is simply
        // not a requeue target, and skipping it here is what prevents a
        // predictable partial application. The fresh attempt goes through
        // the ordinary gate, where a doorbell still sitting in the composer
        // reads as human input and holds.
        let selected: Vec<_> = projection
            .requeueable_for_message(&message_id)
            .into_iter()
            .filter(|record| projection.entry_is_pending(record.recipient, &message_id))
            .collect();
        let requeues: Vec<_> = selected
            .into_iter()
            .map(|record| NotificationRequeue {
                prior_attempt_id: record.attempt_id,
                attempt_id: NotificationAttemptId::generate(),
                recipient: record.recipient,
            })
            .collect();

        let requeued = store.requeue_notifications(message_id, requeues)?;
        if let Some(record) = requeued.first() {
            self.publish_change(
                record.updated_seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(requeued)
    }

    /// Uncleared alarms at or before one absolute cutoff, oldest first.
    ///
    /// Read-only. Nothing here appends to the journal.
    pub fn alarms_at_or_before(
        &self,
        cutoff_ms: u64,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let store = self.store()?;
        Ok(store
            .projection()
            .open_alarms()
            .into_iter()
            .filter(|record| record.updated_at <= cutoff_ms)
            .cloned()
            .collect())
    }

    /// Acknowledge alarms named by exact attempt identifier.
    ///
    /// Every identifier is resolved before any fact is appended, so a
    /// request naming one unknown or superseded attempt changes nothing.
    /// Identifiers already acknowledged resolve and append nothing.
    /// Returned records are captured under the same lock as validation and
    /// clearance, in request order, including repeated identifiers.
    pub fn clear_alarms(
        &self,
        operator: RecipientKey,
        ids: &[NotificationAttemptId],
        cutoff_ms: Option<u64>,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let mut store = self.store()?;
        if operator != RecipientKey::admin(store.projection().workspace_id()) {
            return Err(MailboxError::NotificationClearOperatorInvalid.into());
        }

        let mut fresh = BTreeSet::new();
        let mut summaries = Vec::with_capacity(ids.len());
        for id in ids {
            let record = store
                .projection()
                .alarm_by_attempt(*id)
                .cloned()
                .ok_or_else(|| {
                    MessageStoreError::from(MailboxError::NotificationAttemptUnknown(*id))
                })?;
            if record.state != NotificationState::AttentionRequired {
                return Err(MessageStoreError::from(
                    MailboxError::NotificationClearRequiresAttention,
                )
                .into());
            }
            if let Some(cutoff_ms) = cutoff_ms {
                if record.updated_at > cutoff_ms {
                    return Err(MailboxError::NotificationNewerThanClearCutoff {
                        attempt_id: *id,
                        updated_at: record.updated_at,
                        cutoff_ms,
                    }
                    .into());
                }
            }
            if !store.projection().alarm_cleared(*id) {
                fresh.insert(*id);
            }
            summaries.push(record);
        }

        if !fresh.is_empty() {
            let before = store.projection().last_sequence();
            store.clear_notifications_at(
                operator,
                fresh.into_iter().collect(),
                cutoff_ms,
                now_ms(),
            )?;
            let after = store.projection().last_sequence();
            if after != before {
                self.publish_change(
                    after.expect("a fresh alarm clearance advances the workspace sequence"),
                    &[MessagesChangedArea::Attention],
                );
            }
        }
        Ok(summaries)
    }

    /// Withdraw one exact unwritten wake without changing its mailbox entry.
    ///
    /// The returned boolean is true only when this call appended the durable
    /// fact. Repeating the exact operation is a successful no-op.
    /// The operator's read of one message, body included, with no claim and
    /// no journal append. A doorbell locator resolves to its message.
    pub fn read_message(
        &self,
        reader: RecipientKey,
        reference: MessageId,
    ) -> Result<InboxMessage, MailboxServiceError> {
        if !reader.is_admin() {
            return Err(MailboxServiceError::OperatorRequired);
        }
        let store = self.store()?;
        let message_id = match cyclops_proto::parse_notification_attempt_claim_locator(&reference) {
            Some(attempt_id) => store
                .projection()
                .notification_by_attempt(attempt_id)
                .map(|record| record.message_id.clone())
                .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))
                .map_err(MessageStoreError::from)?,
            None => reference,
        };
        let line = store
            .projection()
            .get_message(&message_id)
            .ok_or(MailboxError::MessageNotFound(message_id))
            .map_err(MessageStoreError::from)?;
        Ok(operator_message(line).map_err(MessageStoreError::from)?)
    }

    pub fn withdraw_notification_before_write(
        &self,
        operator: RecipientKey,
        recipient: RecipientKey,
        attempt_id: NotificationAttemptId,
    ) -> Result<(NotificationRecord, bool), MailboxServiceError> {
        let mut store = self.store()?;
        let before = store.projection().last_sequence();
        let record = store.withdraw_notification_before_write(operator, recipient, attempt_id)?;
        let inserted = store.projection().last_sequence() != before;
        if inserted {
            self.publish_change(record.updated_seq, &[MessagesChangedArea::Notifications]);
        }
        Ok((record, inserted))
    }

    pub(crate) fn store(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, MessageStore>, MailboxServiceError> {
        self.store.lock().map_err(|_| MailboxServiceError::Poisoned)
    }

    pub(crate) fn publish_change(&self, workspace_seq: u64, changed: &[MessagesChangedArea]) {
        if let Some(publisher) = &self.changes {
            publisher.publish(workspace_seq, changed);
        }
    }

    pub(crate) fn directory(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, MailboxDirectory>, MailboxServiceError> {
        self.directory
            .read()
            .map_err(|_| MailboxServiceError::Poisoned)
    }
}

/// Remove bodies the durable reader has not authored or claimed.
///
/// One store lock covers the complete result set. Lines absent from or not
/// content-identical to the workspace projection always lose bodies.
pub(crate) fn redact_message_bodies(
    service: Option<&MailboxService>,
    reader: Option<RecipientKey>,
    lines: &mut [LedgerLine],
) {
    let (Some(service), Some(reader)) = (service, reader) else {
        for line in lines {
            line.body = None;
        }
        return;
    };
    let Ok(store) = service.store() else {
        for line in lines {
            line.body = None;
        }
        return;
    };
    let projection = store.projection();
    for line in lines {
        if !projection_allows_message_body(projection, reader, line) {
            line.body = None;
        }
    }
}
