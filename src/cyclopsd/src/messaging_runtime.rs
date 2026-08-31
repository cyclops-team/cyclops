//! Daemon-root adapter for notification scheduling and terminal recovery.
//!
//! Durable messaging policy lives in `messaging.rs`. This module is the
//! retained host mechanism: it may compose daemon services needed to carry out
//! named `WorkspaceMessagingEffects`, but it is not visible to operation
//! callers and returns only Module-owned outcomes.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_proto::{
    MessageWakeBlock, NotificationAttemptId, NotificationBinding, NotificationManifestId,
    NotificationPreWriteCause, NotificationPreWriteObservation, NotificationRouteEvidenceId,
    NotificationState, ProcessInstanceId, RecipientKey,
};
use tracing::{debug, error};

use crate::mailbox::{MailboxService, MailboxServiceError, UnclaimedReminderQueue};
use crate::messaging::{NotificationRoute, RecipientScheduleOutcome, ScheduledHead};
use crate::notification_adapter::NotificationContext;
use crate::{delivery, Inner, PaneKey};

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
    crate::fusion::cached_notification_observation(
        inner,
        &PaneKey::new(route.session_idx, &route.pane_id),
    )
    .is_some_and(|observation| observation.write_ready_for(route.row.in_mode, &expected))
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
    crate::fusion::cached_notification_observation(
        inner,
        &PaneKey::new(route.session_idx, &route.pane_id),
    )
    .is_some_and(|observation| observation.can_decide_notification_now(route.row.in_mode))
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
pub(crate) fn record_unowned_notification(
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
    if let Some(workspace_messaging) = inner.workspace_messaging() {
        workspace_messaging.exact_owned_evidence_changed(recipient);
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
    if let Some(messaging) = inner.workspace_messaging() {
        messaging.track_composer_recovery(attempt_id);
    }
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

pub(crate) async fn wait_and_queue_unclaimed_reminder(
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

/// Wait for a conflicting resolver to release this exact attempt.
///
/// A release is a boot-local ownership edge, not a durable mailbox change.
/// Broadcast lag means a release edge was lost, not that this attempt was
/// released. The caller revalidates once rather than polling on a timer.
async fn wait_for_attention_resolution_release(
    releases: &mut tokio::sync::broadcast::Receiver<NotificationAttemptId>,
    attempt_id: NotificationAttemptId,
) -> bool {
    loop {
        match releases.recv().await {
            Ok(released) if released == attempt_id => return true,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return true,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
        }
    }
}

/// Arm the explicit post-paste escape hatch for one exact verify-failed
/// doorbell. Multiple callers may arm the same attempt; durable resolution
/// intent elects one key and makes every competing timer a no-op.
pub(crate) fn schedule_force_submit(inner: &Arc<Inner>, record: cyclops_proto::NotificationRecord) {
    if !record.needs_exact_owned_reconciliation() || !inner.force_submit.get().0 {
        return;
    }
    let Some(messaging) = inner.workspace_messaging() else {
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
        // Subscribe before trying to reserve the action. A release that races
        // the conflict stays observable, and one revalidation is enough for
        // this opt-in escape hatch. A second owner is allowed to finish; this
        // task never retries terminal input on its own.
        let mut releases = messaging.subscribe_attention_resolution_releases();
        let target = match messaging.attention_for_runtime(record.attempt_id) {
            Ok(target) => target,
            Err(_) => return,
        };
        let result =
            match crate::attention_resolution::force_complete(&task_inner, &messaging, &target)
                .await
            {
                Err(crate::attention_resolution::AttentionActionError::ResolutionInProgress) => {
                    delivery::inject_pause(
                        &task_inner,
                        "force_submit_waiting_for_resolution_release",
                    )
                    .await;
                    if !wait_for_attention_resolution_release(&mut releases, record.attempt_id)
                        .await
                    {
                        return;
                    }
                    let target = match messaging.attention_for_runtime(record.attempt_id) {
                        Ok(target) => target,
                        Err(_) => return,
                    };
                    match crate::attention_resolution::force_complete(
                        &task_inner,
                        &messaging,
                        &target,
                    )
                    .await
                    {
                        Err(
                            crate::attention_resolution::AttentionActionError::ResolutionInProgress,
                        ) => return,
                        result => result,
                    }
                }
                result => result,
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
    if let Some(messaging) = inner.workspace_messaging() {
        messaging.track_composer_recovery(attempt_id);
    }
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
    if let Some(messaging) = inner.workspace_messaging() {
        messaging.track_composer_recovery(attempt_id);
    }
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
    use super::wait_for_attention_resolution_release;
    use cyclops_proto::NotificationAttemptId;

    fn attempt(number: u64) -> NotificationAttemptId {
        NotificationAttemptId::parse(&format!("att-00000000-0000-4000-8000-{number:012x}")).unwrap()
    }

    #[tokio::test]
    async fn force_submit_waits_for_the_matching_reservation_release() {
        let (sender, mut releases) = tokio::sync::broadcast::channel(4);
        let wanted = attempt(1);
        sender.send(attempt(2)).unwrap();
        sender.send(wanted).unwrap();

        assert!(wait_for_attention_resolution_release(&mut releases, wanted).await);
    }

    #[tokio::test]
    async fn force_submit_stops_when_its_reservation_handoff_closes() {
        let (sender, mut releases) = tokio::sync::broadcast::channel(1);
        drop(sender);

        assert!(!wait_for_attention_resolution_release(&mut releases, attempt(1)).await);
    }
}
