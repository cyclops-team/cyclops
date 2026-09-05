//! Daemon-root adapter for notification scheduling and terminal recovery.
//!
//! Durable messaging policy lives in `messaging.rs`. This module is the
//! retained host mechanism: it may compose daemon services needed to carry out
//! named `WorkspaceMessagingEffects`, but it is not visible to operation
//! callers and returns only Module-owned outcomes.

use std::sync::{Arc, Mutex as StdMutex};

use cyclops_proto::{
    MessageWakeBlock, NotificationBinding, NotificationManifestId, NotificationPreWriteCause,
    NotificationPreWriteObservation, NotificationRouteEvidenceId, ProcessInstanceId, RecipientKey,
};
use tracing::{debug, error};

use crate::mailbox::{MailboxService, MailboxServiceError};
use crate::messaging::{NotificationRoute, RecipientScheduleOutcome, ScheduledHead};
use crate::notification_adapter::NotificationContext;
use crate::{Inner, PaneKey};

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
}
