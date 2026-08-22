//! Coordinates the durable mailbox with the existing pane notification worker.

use std::collections::HashSet;
use std::sync::Arc;

use cyclops_proto::{
    DeliveryReceipt, DeliveryState, MessageId, MsgSendParams, MsgSendResult, RecipientKey,
};
use cyclops_tmux::{PaneRow, SessionWatcher};
use tracing::error;

use crate::mailbox::{
    AcceptResult, ClaimOutcome, MailboxIdentity, MailboxSend, MailboxService, MailboxServiceError,
};
use crate::notification_adapter::NotificationContext;
use crate::Inner;

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

/// Schedule only the oldest pending entry for one durable recipient.
pub(crate) fn schedule_recipient(
    inner: &Arc<Inner>,
    service: &Arc<MailboxService>,
    recipient: RecipientKey,
) -> Result<(), MailboxServiceError> {
    let Some(route) = notification_route(inner, service, recipient)? else {
        return Ok(());
    };
    let Some(record) = service.prepare_oldest_notification(recipient)? else {
        return Ok(());
    };
    let context = NotificationContext::new_with_changes(
        service.store_handle(),
        record.message_id,
        recipient,
        record.attempt_id,
        service.change_publisher(),
    );
    crate::delivery::enqueue_notification_attempt(
        inner,
        route.session_idx,
        &route.pane_id,
        &route.label,
        context,
    );
    Ok(())
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
    let withdrawn = match &outcome {
        ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        }
        | ClaimOutcome::AlreadyClaimed {
            withdrawn_attempt, ..
        } => *withdrawn_attempt,
    };
    if let Some(attempt_id) = withdrawn {
        inner.engine.cancel_notification(attempt_id);
    }
    if let Err(error) = schedule_recipient(inner, service, claimant) {
        error!(%claimant, %error, "cannot schedule mailbox notification after claim");
    }
    Ok(outcome)
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
