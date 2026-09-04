//!
//! Socket request handling, mutation publishing, and reconciliation.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use cyclops_ledger::now_ms;
use cyclops_proto::{
    doorbell_format_names_exact_attempt, Event, Kind, LedgerLine, MailboxListItem, MessageId,
    MessageNotificationState, MessagePresentation, MessageQuotaState, MessageWakeBlock,
    MessagesChangedArea, MessagesChangedData, MessagesFollowResult, MessagesSnapshotResult,
    NotificationAttemptId, NotificationBarrierRetirementCause, NotificationBinding,
    NotificationPreWriteCause, NotificationPreWriteObservation, NotificationRecord,
    NotificationRequeue, NotificationResolution, NotificationResolutionConsumptionEvidence,
    NotificationResolutionConsumptionObservation, NotificationState, ProcessInstanceId,
    RecipientKey, TmuxPaneId, WorkspaceId,
};
use tokio::sync::broadcast;

use super::*;

pub struct MailboxService {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) directory: RwLock<MailboxDirectory>,
    pub(crate) store: Arc<StdMutex<MessageStore>>,
    pub(crate) changes: Option<MessageChangePublisher>,
    pub(crate) resolving_attention: StdMutex<HashSet<NotificationAttemptId>>,
    /// Private handoff for work that lost the exact in-memory resolution
    /// reservation. This is not a durable message event: a resolver may give
    /// up before appending anything.
    pub(crate) attention_resolution_releases: broadcast::Sender<NotificationAttemptId>,
    pub(crate) exact_reconciliation: StdMutex<ExactReconciliationRequests>,
    pub(crate) attention_consumption_candidates:
        StdMutex<HashMap<NotificationAttemptId, AttentionConsumptionCandidate>>,
}

#[derive(Default)]
pub(crate) struct ExactReconciliationRequests {
    running: HashSet<NotificationAttemptId>,
    dirty: HashSet<NotificationAttemptId>,
}

/// One exact causal observation waiting to become a durable consumption fact.
pub(crate) struct AttentionConsumptionSignal {
    observation: StdMutex<Option<NotificationResolutionConsumptionObservation>>,
}

impl AttentionConsumptionSignal {
    pub(crate) fn new() -> Self {
        Self {
            observation: StdMutex::new(None),
        }
    }

    pub(crate) fn confirm(
        &self,
        observation: NotificationResolutionConsumptionObservation,
    ) -> bool {
        let Ok(mut current) = self.observation.lock() else {
            return false;
        };
        if current.is_some() {
            return false;
        }
        *current = Some(observation);
        true
    }

    pub(crate) fn observation(&self) -> Option<NotificationResolutionConsumptionObservation> {
        self.observation.lock().ok().and_then(|value| *value)
    }
}

pub(crate) struct AttentionConsumptionCandidate {
    pub(crate) message_id: MessageId,
    pub(crate) recipient: RecipientKey,
    pub(crate) session_idx: usize,
    pub(crate) pane_id: String,
    pub(crate) pane_root: ProcessInstanceId,
    pub(crate) agent: ProcessInstanceId,
    pub(crate) manifest: String,
    pub(crate) expected_payload: String,
    pub(crate) not_before_ms: u64,
    pub(crate) signal: Arc<AttentionConsumptionSignal>,
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
}

impl From<MailboxError> for MailboxServiceError {
    fn from(error: MailboxError) -> Self {
        Self::Store(MessageStoreError::from(error))
    }
}

impl MailboxServiceError {
    pub(crate) fn notification_resolution_in_progress(&self) -> bool {
        matches!(
            self,
            Self::Store(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::NotificationResolutionInProgress(_))
        )
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
        // One release edge is enough to prompt one blocked force-submit task
        // to revalidate. The stream is shared, so lag is not proof that this
        // attempt was released.
        let (attention_resolution_releases, _) = broadcast::channel(1);
        Self {
            workspace_id: directory.workspace_id(),
            directory: RwLock::new(directory),
            store: Arc::new(StdMutex::new(store)),
            changes,
            resolving_attention: StdMutex::new(HashSet::new()),
            attention_resolution_releases,
            exact_reconciliation: StdMutex::new(ExactReconciliationRequests::default()),
            attention_consumption_candidates: StdMutex::new(HashMap::new()),
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
        let accepted = store.accept(mint_message_id(), draft)?;
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
                        .is_some()
                    || store
                        .projection()
                        .claimed_notification_barrier(*recipient)
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
            .claimed_notification_barrier(recipient)
            .cloned()
        {
            // A durable resolution chain owns this exact barrier until it is
            // reconciled or explicitly withdrawn. Starting the automatic
            // claimed-barrier path here could settle the notification first
            // and leave the operator chain permanently incomplete.
            if store
                .projection()
                .attention_resolution_pending(record.attempt_id)
            {
                return Ok(None);
            }
            return Ok(Some(record));
        }
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
        if let Some(record) = store.projection().claimed_notification_barrier(recipient) {
            if store
                .projection()
                .attention_resolution_pending(record.attempt_id)
            {
                return Ok(Some(NotificationScheduleBlock {
                    message_id: record.message_id.clone(),
                    attempt_id: record.attempt_id,
                    block: MessageWakeBlock::AttentionResolutionPending,
                }));
            }
        }
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

    /// Whether the oldest pending wake is blocked only by its recorded width.
    pub(crate) fn oldest_notification_has_width_block(
        &self,
        recipient: RecipientKey,
    ) -> Result<bool, MailboxServiceError> {
        if recipient.is_admin() {
            return Ok(false);
        }
        let store = self.store()?;
        let Some(message_id) = Self::first_actionable_pending_message_id(&store, recipient) else {
            return Ok(false);
        };
        Ok(store
            .projection()
            .notification(recipient, &message_id)
            .and_then(notification_pre_write_width_block)
            .is_some())
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
                match notification_pre_write_width_block(&current) {
                    Some((observed, required)) => observation
                        .pane_width
                        .is_some_and(|width| width >= required && width != observed),
                    None => {
                        let later_route_evidence = current
                            .pre_write_observation
                            .as_ref()
                            .and_then(|prior| prior.route_evidence.as_ref())
                            .zip(observation.route_evidence.as_ref())
                            .is_some_and(|(prior, current)| {
                                route_evidence_is_later(prior, current)
                            });
                        write_ready && later_route_evidence
                    }
                }
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

    /// Return one canonical message row for internal delivery verification.
    pub(crate) fn message_line(
        &self,
        message_id: &MessageId,
    ) -> Result<LedgerLine, MailboxServiceError> {
        self.store()?
            .projection()
            .get_message(message_id)
            .cloned()
            .ok_or_else(|| MailboxError::MessageNotFound(message_id.clone()).into())
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

    /// Canonical post-write composer barriers for restart reconciliation.
    pub(crate) fn active_notification_barriers(
        &self,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        Ok(self.store()?.projection().active_notification_barriers())
    }

    pub(crate) fn exact_recipient_claimed_after_write(
        &self,
        record: &NotificationRecord,
    ) -> Result<bool, MailboxServiceError> {
        Ok(self
            .store()?
            .projection()
            .exact_recipient_claimed_after_write(record))
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
                recovery_action: store.projection().exact_owned_recovery_action(&record),
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

    /// Exact verify-failed attempts eligible for the operator's opt-in timed
    /// Enter escape hatch. The final forced reservation, rather than this
    /// candidate scan, prevents duplicate scheduler calls from sending two keys.
    pub(crate) fn force_submit_candidates(
        &self,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let store = self.store()?;
        let mut records: Vec<_> = store
            .projection()
            .notifications
            .values()
            .filter(|record| record.needs_exact_owned_reconciliation())
            .filter(|record| {
                store
                    .projection()
                    .get_entry(record.recipient, &record.message_id)
                    .is_some_and(|entry| entry.state.is_pending())
            })
            .cloned()
            .collect();
        records.sort_by_key(|record| record.updated_seq);
        Ok(records)
    }

    /// Revalidate the exact timer target before force preparation. The final
    /// forced reservation, not this read, is the claim-ordering boundary.
    pub(crate) fn force_submit_target_is_pending(
        &self,
        target: &AttentionTarget,
    ) -> Result<bool, MailboxServiceError> {
        let store = self.store()?;
        Ok(store.projection().force_submit_target_is_pending(target))
    }

    /// Persist one exact reason that a composer barrier no longer applies.
    pub(crate) fn retire_notification_barrier(
        &self,
        record: &NotificationRecord,
        cause: NotificationBarrierRetirementCause,
        replacement: Option<NotificationBinding>,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.retire_notification_barrier(
            record.message_id.clone(),
            record.recipient,
            record.attempt_id,
            cause,
            replacement,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("barrier retirement advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
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
        self.reply_with_summary(sender, reference, None, body, client_key)
    }

    pub fn reply_with_summary(
        &self,
        sender: MailboxIdentity,
        reference: MessageId,
        summary: Option<String>,
        body: String,
        client_key: Option<String>,
    ) -> Result<AcceptResult, MailboxServiceError> {
        // Reply routing is the referenced message's immutable sender key,
        // never its presentation label. Keep the current directory read
        // through the append so a rename preserves the route while a
        // replacement incarnation cannot enter between validation and
        // acceptance and inherit the predecessor's thread.
        let directory = self.directory()?;
        let mut store = self.store()?;
        let reference = if reference.as_str() == "--last" || reference.as_str() == "-" {
            store
                .projection()
                .mailboxes
                .get(&sender.key)
                .and_then(|m| {
                    m.values()
                        .filter(|e| e.state.is_claimed())
                        .max_by_key(|e| e.seq)
                        .map(|e| e.message_id.clone())
                })
                .ok_or_else(|| MailboxError::MessageNotFound(reference.clone()))?
        } else if let Some(attempt_id) =
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
        let recipient = store
            .projection()
            .derive_reply(sender.key, &reference)?
            .recipient;
        let Some(destination) = directory.identity_for_recipient(recipient) else {
            return Err(MailboxDirectoryError::UnknownRecipient(recipient.to_string()).into());
        };
        let accepted = store.reply(
            mint_message_id(),
            ReplyDraft {
                sender: sender.key,
                reference,
                summary,
                body: (!body.is_empty()).then_some(body),
                client_key,
                sender_label: sender.label,
                recipient_label: destination.label,
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

    /// Record a positive non-quota observation without resuming delivery.
    /// The returned records are the exact messages an administrator may
    /// now requeue explicitly.
    pub(crate) fn observe_quota_reset(
        &self,
        recipient: RecipientKey,
    ) -> Result<Vec<NotificationRecord>, MailboxServiceError> {
        let mut store = self.store()?;
        let targets: Vec<_> = store
            .projection()
            .quota_held_for_recipient(recipient)
            .into_iter()
            .map(|record| {
                (
                    record.message_id.clone(),
                    record.recipient,
                    record.attempt_id,
                )
            })
            .collect();
        let mut observed = Vec::with_capacity(targets.len());
        for (message_id, recipient, attempt_id) in targets {
            let record = store.advance_notification(
                message_id,
                recipient,
                attempt_id,
                NotificationState::QuotaResetObserved,
                None,
                None,
            )?;
            self.publish_change(
                record.updated_seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
            observed.push(record);
        }
        Ok(observed)
    }

    /// Requeue every uncleared alarm or reset-observed quota hold on one message.
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
        let resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
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
        let requeueable = projection.requeueable_for_message(&message_id);
        if let Some(record) = requeueable
            .iter()
            .filter(|record| record.state == NotificationState::AttentionRequired)
            .find(|record| projection.attention_resolution_pending(record.attempt_id))
        {
            return Err(
                MessageStoreError::from(MailboxError::NotificationResolutionAmbiguous(
                    record.attempt_id,
                ))
                .into(),
            );
        }
        let selected: Vec<_> = requeueable
            .into_iter()
            // An alarm whose entry has been claimed or superseded cannot
            // be redelivered. It stays visible and stays clearable; it is
            // simply not a requeue target, and skipping it here is what
            // prevents a predictable partial application.
            .filter(|record| projection.entry_is_pending(record.recipient, &message_id))
            .filter(|record| !resolving.contains(&record.attempt_id))
            .collect();
        if let Some(record) = selected.iter().copied().find(|record| {
            projection
                .active_notification_barriers
                .get(&record.attempt_id)
                .is_some_and(NotificationRecord::needs_exact_owned_reconciliation)
        }) {
            return Err(MessageStoreError::from(
                MailboxError::NotificationRequeueExactComposerBarrier(record.attempt_id),
            )
            .into());
        }
        if let Some(record) = selected.iter().copied().find(|record| {
            projection
                .active_notification_barriers
                .get(&record.attempt_id)
                .is_some_and(|active| {
                    active.binding.as_ref().is_none_or(|binding| {
                        binding.pane_root.is_none() || binding.leader.is_none()
                    })
                })
        }) {
            return Err(MessageStoreError::from(
                MailboxError::NotificationRequeueBarrierBindingIncomplete(record.attempt_id),
            )
            .into());
        }
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

    /// Resolve an exact attempt id, or one message id with one unresolved attempt.
    pub(crate) fn attention_target(
        &self,
        raw: &str,
    ) -> Result<AttentionTarget, MailboxServiceError> {
        let store = self.store()?;
        let projection = store.projection();
        let record = if let Ok(attempt_id) = NotificationAttemptId::parse(raw) {
            let record = projection
                .alarm_by_attempt(attempt_id)
                .ok_or(MailboxError::NotificationAttemptUnknown(attempt_id))?;
            if projection.attention_resolved(attempt_id) {
                return Err(MailboxError::NotificationAlreadyResolved(attempt_id).into());
            }
            if record.state != NotificationState::AttentionRequired {
                return Err(MailboxError::NotificationClearRequiresAttention.into());
            }
            record
        } else if let Ok(message_id) = MessageId::new(raw) {
            let matches = projection.unresolved_attention_for_message(&message_id);
            match matches.as_slice() {
                [] => return Err(MailboxError::NoUnresolvedAttention(message_id).into()),
                [record] => *record,
                many => {
                    return Err(MailboxError::AmbiguousAttentionTarget {
                        message_id,
                        candidates: many.iter().map(|record| record.attempt_id).collect(),
                    }
                    .into())
                }
            }
        } else {
            return Err(MailboxError::InvalidAttentionTarget(raw.to_string()).into());
        };
        projection
            .get_message(&record.message_id)
            .ok_or_else(|| MailboxError::MessageNotFound(record.message_id.clone()))?;
        Ok(AttentionTarget {
            record: record.clone(),
        })
    }

    /// Append the one content-free resolution fact for an exact attempt.
    pub(crate) fn resolve_attention(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        {
            let mut store = self.store()?;
            store.resolve_notification(
                target.record.message_id.clone(),
                target.record.recipient,
                target.record.attempt_id,
                resolution,
            )?;
            let seq = store
                .projection()
                .last_sequence()
                .expect("attention resolution advances the workspace sequence");
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        self.release_attention_resolution(target.record.attempt_id)?;
        Ok(())
    }

    /// Read the current durable policy for exact-owned recovery.
    pub(crate) fn automatic_attention_resolution(
        &self,
        target: &AttentionTarget,
    ) -> Result<Option<NotificationResolution>, MailboxServiceError> {
        Ok(self.store()?.projection().exact_owned_resolution(target))
    }

    /// Record a relevant evidence edge and elect at most one worker.
    pub(crate) fn request_exact_reconciliation(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        requests.dirty.insert(attempt_id);
        Ok(requests.running.insert(attempt_id))
    }

    /// Consume one evidence edge or retire the attempt worker atomically.
    pub(crate) fn take_exact_reconciliation_request(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if requests.dirty.remove(&attempt_id) {
            return Ok(true);
        }
        requests.running.remove(&attempt_id);
        Ok(false)
    }

    /// Preserve an edge that collided with an explicit resolution.
    ///
    /// The reservation check and worker handoff share the same critical
    /// section. Either the explicit resolver sees the parked edge when it
    /// releases its reservation, or this worker sees that the reservation
    /// already ended and continues immediately.
    pub(crate) fn park_exact_reconciliation_after_conflict(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        requests.dirty.insert(attempt_id);
        requests.running.remove(&attempt_id);
        if resolving.contains(&attempt_id) {
            return Ok(false);
        }
        Ok(requests.running.insert(attempt_id))
    }

    /// Re-elect a parked worker after the explicit reservation ends.
    pub(crate) fn resume_exact_reconciliation(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<bool, MailboxServiceError> {
        let resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if resolving.contains(&attempt_id) {
            return Ok(false);
        }
        let mut requests = self
            .exact_reconciliation
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        Ok(requests.dirty.contains(&attempt_id) && requests.running.insert(attempt_id))
    }

    /// Persist the exact-owned recovery choice at its mailbox linearization point.
    pub(crate) fn record_automatic_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
    ) -> Result<NotificationResolution, MailboxServiceError> {
        let mut store = self.store()?;
        let resolution = store.projection().exact_owned_resolution(target).ok_or(
            MailboxError::NotificationAutomaticResolutionNotEligible(target.record.attempt_id),
        )?;
        if store
            .projection()
            .attention_resolution_intent(target.record.attempt_id)
            .is_some()
        {
            return Ok(resolution);
        }
        store.record_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention resolution intent advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(resolution)
    }

    /// Persist the terminal write boundary before a resolution action.
    pub(crate) fn record_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.record_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention resolution intent advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
    }

    /// Persist the timed escape hatch before its one terminal key. The fact
    /// carries no message content and marks the action as forced for audit.
    pub(crate) fn record_forced_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.record_forced_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            NotificationResolution::Complete,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("forced resolution intent advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
    }

    /// Atomically reserve one forced Complete key with mailbox claims.
    ///
    /// A claim ordered before this append makes the timer a no-op. Once the
    /// reservation is durable, a later claim remains a normal retrieval but
    /// cannot revoke the one key the fallback is about to send or count as
    /// consumption until the action-accepted fact exists.
    pub(crate) fn reserve_forced_attention_resolution_action(
        &self,
        target: &AttentionTarget,
    ) -> Result<bool, MailboxServiceError> {
        let mut store = self.store()?;
        if !store.projection().force_submit_target_is_pending(target) {
            return Ok(false);
        }
        let prior_seq = store.projection().last_sequence();
        store.reserve_forced_notification_resolution_action(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("forced key reservation advances the workspace sequence");
        if Some(seq) != prior_seq {
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(true)
    }

    /// Persist terminal acceptance for the exact durable resolution intent.
    pub(crate) fn record_attention_resolution_action_accepted(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        let prior_seq = store.projection().last_sequence();
        store.record_notification_resolution_action_accepted(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention action acceptance retains a workspace sequence");
        if Some(seq) != prior_seq {
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(())
    }

    /// Persist exact causal consumption for one accepted Complete action.
    pub(crate) fn record_attention_resolution_consumption_observed(
        &self,
        target: &AttentionTarget,
        observation: NotificationResolutionConsumptionObservation,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        let prior_seq = store.projection().last_sequence();
        store.record_notification_resolution_consumption_observed(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            observation,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention consumption observation retains a workspace sequence");
        if Some(seq) != prior_seq {
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        Ok(())
    }

    /// Register one boot-local exact-attempt matcher before the terminal key.
    ///
    /// The payload remains in memory. Only a closed evidence kind and timestamp
    /// can cross the durable boundary after terminal acceptance is recorded.
    pub(crate) fn register_attention_consumption_candidate(
        &self,
        target: &AttentionTarget,
        session_idx: usize,
        pane_id: String,
        expected_payload: String,
        not_before_ms: u64,
    ) -> Result<Option<Arc<AttentionConsumptionSignal>>, MailboxServiceError> {
        if !doorbell_format_names_exact_attempt(target.record.doorbell_format) {
            return Ok(None);
        }
        let binding = target
            .record
            .binding
            .as_ref()
            .ok_or(MailboxError::NotificationClearRequiresAttention)?;
        let pane_root = binding
            .pane_root
            .ok_or(MailboxError::NotificationClearRequiresAttention)?;
        let signal = Arc::new(AttentionConsumptionSignal::new());
        let candidate = AttentionConsumptionCandidate {
            message_id: target.record.message_id.clone(),
            recipient: target.record.recipient,
            session_idx,
            pane_id,
            pane_root,
            agent: binding.agent,
            manifest: binding.manifest.as_str().to_string(),
            expected_payload,
            not_before_ms,
            signal: Arc::clone(&signal),
        };
        let mut candidates = self
            .attention_consumption_candidates
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if candidates.contains_key(&target.record.attempt_id) {
            return Err(
                MailboxError::NotificationResolutionInProgress(target.record.attempt_id).into(),
            );
        }
        candidates.insert(target.record.attempt_id, candidate);
        Ok(Some(signal))
    }

    pub(crate) fn unregister_attention_consumption_candidate(
        &self,
        attempt_id: NotificationAttemptId,
    ) {
        if let Ok(mut candidates) = self.attention_consumption_candidates.lock() {
            candidates.remove(&attempt_id);
        }
    }

    /// Match an authenticated hook only when its token names this exact
    /// attempt and its payload and durable binding also match.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn confirm_attention_consumption_hook(
        &self,
        session_idx: usize,
        pane_id: &str,
        recipient: RecipientKey,
        pane_root: crate::identity::ProcId,
        agent: crate::identity::ProcId,
        manifest: &str,
        prompt: &str,
        observed_at_ms: u64,
    ) -> bool {
        let Some((message_id, attempt_id)) = cyclops_proto::parse_doorbell_v3(prompt)
            .map(|attempt_id| (None, attempt_id))
            .or_else(|| {
                cyclops_proto::parse_doorbell_v2(prompt)
                    .map(|(message_id, attempt_id)| (Some(message_id), attempt_id))
            })
        else {
            return false;
        };
        let Ok(candidates) = self.attention_consumption_candidates.lock() else {
            return false;
        };
        let matching: Vec<_> = candidates
            .iter()
            .filter(|(candidate_attempt, candidate)| {
                **candidate_attempt == attempt_id
                    && message_id
                        .as_ref()
                        .is_none_or(|message_id| &candidate.message_id == message_id)
                    && candidate.session_idx == session_idx
                    && candidate.pane_id == pane_id
                    && candidate.recipient == recipient
                    && candidate.pane_root.pid() == pane_root.pid
                    && candidate.pane_root.birth() == pane_root.birth
                    && candidate.agent.pid() == agent.pid
                    && candidate.agent.birth() == agent.birth
                    && candidate.manifest == manifest
                    && observed_at_ms >= candidate.not_before_ms
                    && crate::delivery::prompt_matches(prompt, &candidate.expected_payload)
            })
            .collect();
        if matching.len() != 1 {
            return false;
        }
        matching[0]
            .1
            .signal
            .confirm(NotificationResolutionConsumptionObservation {
                evidence: NotificationResolutionConsumptionEvidence::ExactHookPrompt,
                observed_at_ms,
            })
    }

    /// Return a durable claim eligible to count as this action's consumption.
    /// Ordinary actions need a claim after acceptance. Forced Complete instead
    /// orders claims against its preceding reservation, but waits for acceptance
    /// before treating an intervening claim as consumption evidence.
    pub(crate) fn attention_claim_consumption(
        &self,
        target: &AttentionTarget,
    ) -> Result<Option<NotificationResolutionConsumptionObservation>, MailboxServiceError> {
        Ok(self
            .store()?
            .projection()
            .exact_claim_after_attention_action(&target.record))
    }

    /// Append one atomic no-key Discard fact and release the reservation.
    pub(crate) fn resolve_attention_without_terminal_action(
        &self,
        target: &AttentionTarget,
    ) -> Result<(), MailboxServiceError> {
        {
            let mut store = self.store()?;
            store.resolve_notification_without_terminal_action(
                target.record.message_id.clone(),
                target.record.recipient,
                target.record.attempt_id,
            )?;
            let seq = store
                .projection()
                .last_sequence()
                .expect("no-key attention resolution advances the workspace sequence");
            self.publish_change(
                seq,
                &[
                    MessagesChangedArea::Notifications,
                    MessagesChangedArea::Attention,
                ],
            );
        }
        self.release_attention_resolution(target.record.attempt_id)?;
        Ok(())
    }

    /// Withdraw a proven pre-key refusal so the action may be attempted again.
    pub(crate) fn withdraw_attention_resolution_intent(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<(), MailboxServiceError> {
        let mut store = self.store()?;
        store.withdraw_notification_resolution_intent(
            target.record.message_id.clone(),
            target.record.recipient,
            target.record.attempt_id,
            resolution,
        )?;
        let seq = store
            .projection()
            .last_sequence()
            .expect("attention intent withdrawal advances the workspace sequence");
        self.publish_change(
            seq,
            &[
                MessagesChangedArea::Notifications,
                MessagesChangedArea::Attention,
            ],
        );
        Ok(())
    }

    /// Reserve an attempt and distinguish a fresh action from no-key recovery.
    pub(crate) fn begin_attention_resolution(
        &self,
        target: &AttentionTarget,
        resolution: NotificationResolution,
    ) -> Result<AttentionResolutionStart, MailboxServiceError> {
        let mut resolving = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?;
        if resolving.contains(&target.record.attempt_id) {
            return Err(
                MailboxError::NotificationResolutionInProgress(target.record.attempt_id).into(),
            );
        }
        let store = self.store()?;
        let projection = store.projection();
        let current = projection
            .alarm_by_attempt(target.record.attempt_id)
            .ok_or(MailboxError::NotificationAttemptUnknown(
                target.record.attempt_id,
            ))?;
        if current != &target.record || projection.attention_resolved(current.attempt_id) {
            return Err(MailboxError::NotificationAlreadyResolved(current.attempt_id).into());
        }
        let accepted = projection.attention_resolution_action_accepted(current.attempt_id);
        let consumed = projection.attention_resolution_consumption_observed(current.attempt_id);
        let start = match projection.attention_resolution_intent(current.attempt_id) {
            Some(recorded) if recorded == resolution => match (resolution, accepted, consumed) {
                (
                    NotificationResolution::Complete,
                    Some(NotificationResolution::Complete),
                    Some(_),
                )
                | (NotificationResolution::Discard, Some(NotificationResolution::Discard), None) => {
                    AttentionResolutionStart::ReconcileOnly
                }
                (
                    NotificationResolution::Complete,
                    Some(NotificationResolution::Complete),
                    None,
                ) => AttentionResolutionStart::AcceptedUnconsumed,
                (
                    NotificationResolution::Complete | NotificationResolution::Discard,
                    None,
                    None,
                ) => AttentionResolutionStart::IntentOnlyUncertain,
                _ => {
                    return Err(
                        MailboxError::NotificationResolutionAmbiguous(current.attempt_id).into(),
                    )
                }
            },
            Some(_) => {
                return Err(
                    MailboxError::NotificationResolutionAmbiguous(current.attempt_id).into(),
                )
            }
            None if accepted.is_none() && consumed.is_none() => AttentionResolutionStart::Fresh,
            None => {
                return Err(
                    MailboxError::NotificationResolutionAmbiguous(current.attempt_id).into(),
                )
            }
        };
        resolving.insert(target.record.attempt_id);
        Ok(start)
    }

    pub(crate) fn cancel_attention_resolution(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<(), MailboxServiceError> {
        self.release_attention_resolution(attempt_id)
    }

    /// Subscribe before attempting a force-submit reservation. A matching
    /// release requires the caller to revalidate the exact attempt; it says
    /// nothing about durable settlement.
    pub(crate) fn subscribe_attention_resolution_releases(
        &self,
    ) -> broadcast::Receiver<NotificationAttemptId> {
        self.attention_resolution_releases.subscribe()
    }

    /// End one boot-local resolution reservation and wake exact waiters after
    /// the ownership change. A cancellation can leave the journal unchanged,
    /// so this must not be represented as `messages.changed`.
    pub(crate) fn release_attention_resolution(
        &self,
        attempt_id: NotificationAttemptId,
    ) -> Result<(), MailboxServiceError> {
        let released = self
            .resolving_attention
            .lock()
            .map_err(|_| MailboxServiceError::Poisoned)?
            .remove(&attempt_id);
        if released {
            let _ = self.attention_resolution_releases.send(attempt_id);
        }
        Ok(())
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
