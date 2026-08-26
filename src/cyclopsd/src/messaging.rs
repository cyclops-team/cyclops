//! Coordinates the durable mailbox with the existing pane notification worker.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_proto::{
    DeliveryReceipt, DeliveryState, MessageId, MessageWakeBlock, MsgSendParams, MsgSendResult,
    NotificationAttemptId, NotificationBinding, NotificationManifestId, NotificationPreWriteCause,
    NotificationPreWriteObservation, NotificationRouteEvidenceId, NotificationState,
    NotificationWithdrawDisposition, NotificationWithdrawResult, ProcessInstanceId, RecipientKey,
};
use cyclops_tmux::{PaneRow, SessionWatcher};
use tokio::time::Instant;
use tracing::{debug, error};

use crate::mailbox::{
    AcceptResult, ClaimOutcome, MailboxIdentity, MailboxSend, MailboxService, MailboxServiceError,
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
    !pane_in_mode
        && entry.detection.write_ready
        && !entry.detection.stale
        && !entry.detection.disagreement
        && !entry.in_mode
        && entry.binding.as_ref() == Some(expected)
}

/// Whether the already-computed pane verdict lets the worker decide now.
///
/// This is only a receipt-latency hint. The delivery worker still performs
/// every route, process, manifest, and terminal binding proof. A stale hint
/// can make the caller wait to its cap, but can never authorize a write.
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
                && !entry.detection.disagreement
                && entry.detection.write_ready
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

fn has_first_durable_disposition(
    disposition: &crate::mailbox::MessageDisposition,
    head: &ScheduledHead,
) -> bool {
    if disposition.attempt_id != Some(head.attempt_id) {
        return true;
    }
    !matches!(
        disposition.notification_state_raw,
        Some(NotificationState::Queued | NotificationState::Gating)
    )
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
) -> Result<Vec<crate::mailbox::MessageDisposition>, MailboxServiceError> {
    observe_first_durable_dispositions_with(
        message_id,
        outcomes,
        events,
        deadline,
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
                .is_none_or(|disposition| !has_first_durable_disposition(disposition, head))
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

async fn finish_acceptance(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    accepted: AcceptResult,
) -> Result<MsgSendResult, MailboxServiceError> {
    // Subscribe before scheduling. A worker may commit its first disposition
    // before enqueue returns; the immediate projection read below remains the
    // authority, and this receiver prevents losing a later commit.
    let events = inner.events.subscribe();
    let schedule_outcomes = schedule_accepted_notifications(&accepted, |recipient| {
        schedule_recipient(inner, service, recipient)
    })?;
    let deadline = Instant::now() + Duration::from_millis(inner.cfg.receipt_block_ms);
    let dispositions = observe_first_durable_dispositions(
        service,
        &accepted.message_id,
        &schedule_outcomes,
        events,
        deadline,
    )
    .await?;
    Ok(MsgSendResult {
        msg_id: accepted.message_id.to_string(),
        seq: accepted.seq,
        deliveries: dispositions
            .into_iter()
            .map(|disposition| {
                let pane = notification_route(inner, service, disposition.recipient)?
                    .map(|route| route.pane_id);
                Ok(receipt_from_disposition(disposition, pane))
            })
            .collect::<Result<Vec<_>, MailboxServiceError>>()?,
        inserted: Some(accepted.inserted),
    })
}

/// Attempt every broadcast recipient, then propagate the first failure whose
/// durable disposition could not be recorded.
fn schedule_accepted_notifications(
    accepted: &AcceptResult,
    mut schedule: impl FnMut(RecipientKey) -> Result<RecipientScheduleOutcome, MailboxServiceError>,
) -> Result<HashMap<RecipientKey, RecipientScheduleOutcome>, MailboxServiceError> {
    let mut outcomes = HashMap::new();
    let mut first_error = None;
    for recipient in accepted.recipient_keys.iter().copied() {
        match schedule(recipient) {
            Ok(outcome) => {
                outcomes.insert(recipient, outcome);
            }
            Err(error) => {
                error!(%recipient, %error, "cannot schedule accepted mailbox notification");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    first_error.map_or(Ok(outcomes), Err)
}

pub(crate) async fn send(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    sender: MailboxIdentity,
    params: MsgSendParams,
) -> Result<MsgSendResult, MailboxServiceError> {
    if params.reply_to.is_some() && (!params.to.is_empty() || params.recipient_keys.is_some()) {
        return Err(crate::mailbox::MailboxDirectoryError::ReplyRecipientSelectors.into());
    }
    if params.recipient_keys.is_some() && !params.to.is_empty() {
        return Err(crate::mailbox::MailboxDirectoryError::MixedRecipientSelectors.into());
    }
    let accepted = match params.reply_to {
        Some(reference) => service.reply(
            sender,
            MessageId::new(reference)
                .map_err(crate::mailbox::MailboxError::from)
                .map_err(crate::mailbox::MessageStoreError::from)
                .map_err(MailboxServiceError::from)?,
            params.body,
            params.client_key,
        )?,
        None => service.send(
            sender,
            MailboxSend {
                addresses: params.to,
                recipient_keys: params.recipient_keys,
                subject: params.subject,
                body: params.body,
                fyi: params.fyi,
                client_key: params.client_key,
                supersedes: params.supersedes,
            },
        )?,
    };
    finish_acceptance(inner, service, accepted).await
}

pub(crate) async fn reply(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    sender: MailboxIdentity,
    reference: MessageId,
    body: String,
    client_key: Option<String>,
) -> Result<MsgSendResult, MailboxServiceError> {
    let accepted = service.reply(sender, reference, body, client_key)?;
    finish_acceptance(inner, service, accepted).await
}

pub(crate) fn claim(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    claimant: RecipientKey,
    message_id: MessageId,
) -> Result<ClaimOutcome, MailboxServiceError> {
    let outcome = service.claim(claimant, message_id)?;
    finish_claim(inner, service, claimant, outcome)
}

pub(crate) fn claim_notification_locator(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    claimant: RecipientKey,
    locator: MessageId,
    attempt_id: cyclops_proto::NotificationAttemptId,
) -> Result<ClaimOutcome, MailboxServiceError> {
    let outcome = service.claim_notification_locator(claimant, locator, attempt_id)?;
    finish_claim(inner, service, claimant, outcome)
}

fn finish_claim(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    claimant: RecipientKey,
    outcome: ClaimOutcome,
) -> Result<ClaimOutcome, MailboxServiceError> {
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
        crate::delivery::settle_notification_claim(inner, attempt_id);
        if let Err(error) =
            schedule_claimed_composer_observation(inner, service, claimant, attempt_id)
        {
            error!(%claimant, %error, "cannot observe claimed notification composer");
        }
    }
    if let Some(attempt_id) = claimed_ack_timeout {
        if let Err(error) =
            schedule_claimed_notification_recovery(inner, service, claimant, attempt_id)
        {
            error!(%claimant, %error, "cannot schedule claimed notification recovery");
        }
    }
    if let Some(attempt_id) = withdrawn {
        inner.engine.cancel_notification(attempt_id);
    }
    crate::attention_resolution::schedule_exact_owned_reconciliation(inner, claimant);
    if claimed_ack_timeout.is_none() {
        if let Err(error) = schedule_recipient(inner, service, claimant) {
            error!(%claimant, %error, "cannot schedule mailbox notification after claim");
        }
    }
    Ok(outcome)
}

/// Reconcile one exact doorbell barrier against a fresh post-claim screen.
///
/// A claim can move Submitted to Notified after its delivery handle has
/// retired. Tracking the exact attempt makes later pane observations eligible
/// for durable composer-barrier retirement. The forced capture closes the
/// no-output case where the composer was already clean when the claim landed.
/// Claim identity alone never retires the barrier: recovery still requires the
/// same bound occupant and manifest plus positive clean-composer evidence.
fn schedule_claimed_composer_observation(
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
        crate::fusion::recompute_pane(
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
fn schedule_claimed_notification_recovery(
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
        crate::fusion::recompute_pane(
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

pub(crate) fn requeue(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    message_id: MessageId,
) -> Result<bool, MailboxServiceError> {
    let records = service.requeue_message(message_id)?;
    let recipients: HashSet<_> = records.iter().map(|record| record.recipient).collect();
    for recipient in recipients {
        if let Err(error) = schedule_recipient(inner, service, recipient) {
            error!(%recipient, %error, "cannot schedule requeued mailbox notification");
        }
    }
    Ok(!records.is_empty())
}

/// Withdraw one exact unwritten wake and advance the recipient's FIFO.
pub(crate) fn withdraw_notification(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    operator: RecipientKey,
    recipient: RecipientKey,
    attempt_id: cyclops_proto::NotificationAttemptId,
) -> Result<NotificationWithdrawResult, MailboxServiceError> {
    let (record, inserted) = {
        let _publication = inner
            .mailbox_publication
            .lock()
            .expect("mailbox publication lock");
        service.withdraw_notification_before_write(operator, recipient, attempt_id)?
    };
    inner.engine.cancel_notification(attempt_id);
    if let Err(error) = schedule_recipient(inner, service, recipient) {
        error!(%recipient, %error, "cannot schedule mailbox notification after withdrawal");
    }
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

#[cfg(test)]
mod tests {
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

    #[test]
    fn cached_readiness_belongs_to_one_complete_process_binding() {
        let original = binding(200);
        let entry = crate::DetEntry {
            detection: cyclops_proto::Detection {
                state: cyclops_proto::AgentState::Idle,
                readings: Vec::new(),
                disagreement: false,
                decided_by: "fixture".into(),
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
    fn a_scheduler_persistence_failure_does_not_skip_later_broadcast_recipients() {
        let workspace = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let session = "00000000-0000-4000-8000-000000000002".parse().unwrap();
        let first = RecipientKey::agent(workspace, session, "%3".parse().unwrap());
        let middle = RecipientKey::agent(workspace, session, "%4".parse().unwrap());
        let last = RecipientKey::agent(workspace, session, "%5".parse().unwrap());
        let accepted = AcceptResult {
            message_id: MessageId::new("m-accepted-schedule-failed").unwrap(),
            inserted: true,
            seq: 41,
            recipients: vec!["first".into(), "middle".into(), "last".into()],
            recipient_keys: vec![first, middle, last],
        };

        let mut attempted = Vec::new();
        let error = schedule_accepted_notifications(&accepted, |recipient| {
            attempted.push(recipient);
            if recipient == middle {
                Err(MailboxServiceError::Poisoned)
            } else {
                Ok(RecipientScheduleOutcome::WorkerOwned {
                    head: ScheduledHead::new(
                        accepted.message_id.clone(),
                        NotificationAttemptId::generate(),
                    ),
                    observe_first_disposition: false,
                })
            }
        })
        .unwrap_err();
        assert_eq!(attempted, vec![first, middle, last]);
        assert!(matches!(error, MailboxServiceError::Poisoned));
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
