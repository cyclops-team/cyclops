//! Coordinates the durable mailbox with the existing pane notification worker.

use std::collections::HashSet;
use std::sync::Arc;

use cyclops_proto::{
    DeliveryReceipt, DeliveryState, MessageId, MsgSendParams, MsgSendResult, NotificationBinding,
    NotificationManifestId, NotificationPreWriteObservation, NotificationWithdrawDisposition,
    NotificationWithdrawResult, ProcessInstanceId, RecipientKey,
};
use cyclops_tmux::{PaneRow, SessionWatcher};
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
    for (session_idx, slot) in inner.session_slots().into_iter().enumerate() {
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
        pane_width: Some(route.row.width),
        required_pane_width: None,
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

fn enqueue_prepared_notification(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
    route: NotificationRoute,
    record: cyclops_proto::NotificationRecord,
    rerun_existing: bool,
) {
    let context = NotificationContext::new_with_changes(
        service.store_handle(),
        record.message_id,
        recipient,
        record.attempt_id,
        service.change_publisher(),
    );
    let _ = crate::delivery::enqueue_notification_attempt(
        inner,
        route.session_idx,
        &route.pane_id,
        &route.label,
        context,
        rerun_existing,
    );
}

/// Schedule only the oldest pending entry for one durable recipient.
pub(crate) fn schedule_recipient(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
) -> Result<(), MailboxServiceError> {
    if inner.engine.is_stopping() {
        return Ok(());
    }
    let Some(route) = notification_route(inner, service, recipient)? else {
        return Ok(());
    };
    let Some(record) = service.prepare_oldest_notification(recipient)? else {
        return Ok(());
    };
    enqueue_prepared_notification(inner, service, recipient, route, record, false);
    Ok(())
}

/// Reopen one blocked attempt under the publication lock.
///
/// The returned route and record are handed to the delivery engine only
/// after every registry and mailbox lock has been released.
fn prepare_recipient_after_route_evidence(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
) -> Result<Option<(NotificationRoute, cyclops_proto::NotificationRecord)>, MailboxServiceError> {
    let Some(route) = notification_route(inner, service, recipient)? else {
        return Ok(None);
    };
    let Some(observation) = proven_binding_observation(inner, recipient, &route) else {
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
    let Some(confirmed) = proven_binding_observation(inner, recipient, &confirmed_route) else {
        return Ok(None);
    };
    if confirmed != observation
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
) -> Result<(), MailboxServiceError> {
    let prepared = {
        let _publication = inner
            .mailbox_publication
            .lock()
            .expect("mailbox publication lock");
        prepare_recipient_after_route_evidence(inner, service, recipient)?
    };
    if let Some((route, record)) = prepared {
        enqueue_prepared_notification(inner, service, recipient, route, record, true);
    }
    Ok(())
}

/// Reconcile one pane after its route, process, or readiness evidence changed.
pub(crate) fn schedule_route_changed(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) {
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    let Some(recipient) = inner.recipient_key(session_idx, pane_id) else {
        return;
    };
    if let Err(error) = schedule_recipient(inner, service, recipient) {
        error!(%recipient, %error, "cannot schedule mailbox notification for changed route");
    }
    if let Err(error) = schedule_recipient_after_route_evidence(inner, service, recipient) {
        error!(%recipient, %error, "cannot reopen blocked mailbox notification");
    }
    crate::attention_resolution::schedule_exact_owned_reconciliation(inner, recipient);
}

/// Reconsider a durable width block after one actual pane-size edge.
///
/// Size-only events remain irrelevant to normal delivery and state fusion.
/// Once the exact blocked attempt reopens, later size events are no-ops.
pub(crate) fn schedule_pane_size_changed(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) {
    let Some(service) = inner.mailbox.as_ref() else {
        return;
    };
    let Some(recipient) = inner.recipient_key(session_idx, pane_id) else {
        return;
    };
    match service.oldest_notification_has_width_block(recipient) {
        Ok(true) => {
            if let Err(error) = schedule_recipient_after_route_evidence(inner, service, recipient) {
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

fn finish_acceptance(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    accepted: AcceptResult,
) -> Result<MsgSendResult, MailboxServiceError> {
    for recipient in accepted.recipient_keys.iter().copied() {
        if let Err(error) = schedule_recipient(inner, service, recipient) {
            error!(%recipient, %error, "cannot schedule accepted mailbox notification");
        }
    }
    let dispositions = service.message_dispositions(&accepted.message_id)?;
    Ok(MsgSendResult {
        msg_id: accepted.message_id.to_string(),
        seq: accepted.seq,
        deliveries: dispositions
            .into_iter()
            .map(|disposition| {
                let pane = notification_route(inner, service, disposition.recipient)?
                    .map(|route| route.pane_id);
                Ok(DeliveryReceipt {
                    to: disposition.label,
                    state: DeliveryState::Queued,
                    notification_state: Some(disposition.notification_state),
                    quota_state: disposition.quota_state,
                    notification_settlement: disposition.notification_settlement,
                    position: disposition.position_ahead,
                    held_by: None,
                    note: None,
                    pane,
                })
            })
            .collect::<Result<Vec<_>, MailboxServiceError>>()?,
        inserted: Some(accepted.inserted),
    })
}

pub(crate) fn send(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    sender: MailboxIdentity,
    params: MsgSendParams,
) -> Result<MsgSendResult, MailboxServiceError> {
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
                subject: params.subject,
                body: params.body,
                fyi: params.fyi,
                client_key: params.client_key,
                supersedes: params.supersedes,
            },
        )?,
    };
    finish_acceptance(inner, service, accepted)
}

pub(crate) fn reply(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    sender: MailboxIdentity,
    reference: MessageId,
    body: String,
    client_key: Option<String>,
) -> Result<MsgSendResult, MailboxServiceError> {
    let accepted = service.reply(sender, reference, body, client_key)?;
    finish_acceptance(inner, service, accepted)
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
    enqueue_prepared_notification(inner, service, recipient, route, record, true);
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
}
