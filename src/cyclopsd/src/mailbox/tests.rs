use super::*;
use cyclops_proto::{
    scratch::scratch_dir, NotificationManifestId, ProcessInstanceId, RecipientPresentation,
    SessionInstanceId, TmuxPaneId,
};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;

struct StoreScratch {
    path: PathBuf,
}

impl StoreScratch {
    fn new(tag: &str) -> Self {
        Self {
            path: scratch_dir(&format!("message-store-{tag}-{}", uuid::Uuid::new_v4())),
        }
    }

    fn root(&self) -> StateRoot {
        StateRoot::open_or_create(&self.path).unwrap()
    }
}

impl Drop for StoreScratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                panic!("remove scratch {}: {error}", self.path.display());
            }
        }
    }
}

fn test_context() -> (WorkspaceId, RecipientKey, RecipientKey, RecipientKey) {
    let ws = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let sess = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
    let p1 = TmuxPaneId::from_str("%1").unwrap();
    let p2 = TmuxPaneId::from_str("%2").unwrap();

    let admin = RecipientKey::admin(ws);
    let bob = RecipientKey::agent(ws, sess, p1);
    let carol = RecipientKey::agent(ws, sess, p2);
    (ws, admin, bob, carol)
}

fn routes(
    recipients: impl IntoIterator<Item = RecipientKey>,
) -> HashMap<RecipientKey, MessageRecipientRoute> {
    recipients
        .into_iter()
        .filter_map(|recipient| {
            Some((
                recipient,
                MessageRecipientRoute {
                    label: recipient.pane_id()?.to_string(),
                    pane_id: recipient.pane_id()?,
                },
            ))
        })
        .collect()
}

fn attempt(number: u64) -> NotificationAttemptId {
    NotificationAttemptId::parse(&format!("att-00000000-0000-4000-8000-{number:012x}")).unwrap()
}

fn notification_binding(recipient: RecipientKey) -> NotificationBinding {
    NotificationBinding {
        recipient,
        pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
        leader: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
        agent: ProcessInstanceId::new(4242, 818_221).unwrap(),
        manifest: NotificationManifestId::new("codex").unwrap(),
    }
}

fn legacy_notification_binding(recipient: RecipientKey) -> NotificationBinding {
    NotificationBinding {
        pane_root: None,
        ..notification_binding(recipient)
    }
}

fn mailbox_send(address: &str, subject: &str, body: &str) -> MailboxSend {
    MailboxSend {
        addresses: vec![address.into()],
        recipient_keys: None,
        subject: subject.into(),
        summary: None,
        body: body.into(),
        fyi: false,
        client_key: None,
        supersedes: None,
        raw: false,
    }
}

fn exact_mailbox_send(
    recipient_keys: Vec<RecipientKey>,
    subject: &str,
    body: &str,
    client_key: Option<&str>,
) -> MailboxSend {
    MailboxSend {
        addresses: Vec::new(),
        recipient_keys: Some(recipient_keys),
        subject: subject.into(),
        summary: None,
        body: body.into(),
        fyi: false,
        client_key: client_key.map(str::to_string),
        supersedes: None,
        raw: false,
    }
}

#[test]
fn historical_message_rows_larger_than_the_socket_frame_remain_readable_and_unchanged() {
    let scratch = StoreScratch::new("oversized-historical-row");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _admin, bob, carol) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "observer".into(),
            },
        ],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot-old").unwrap();
    let service = MailboxService::new(directory, store);
    let body = "x".repeat(cyclops_proto::FrameContract::MAX_JSON_BYTES + 1);
    let accepted = service
        .send(
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            mailbox_send("observer", "Historical large row", &body),
        )
        .unwrap();
    let message_id = accepted.message_id.clone();
    drop(service);

    let path = root.path().join(journal);
    let before = fs::read(&path).unwrap();
    assert!(before.len() > cyclops_proto::FrameContract::MAX_JSON_BYTES);

    let reopened = MessageStore::open(&root, journal, workspace, "boot-current").unwrap();
    let replayed = reopened
        .projection()
        .get_message(&message_id)
        .expect("the historical oversized row remains readable");
    assert_eq!(replayed.body.as_deref(), Some(body.as_str()));
    drop(reopened);
    assert_eq!(
        fs::read(path).unwrap(),
        before,
        "replay rewrote the journal"
    );
}

fn next_change(
    events: &mut broadcast::Receiver<Event>,
    expected_seq: u64,
    expected: &[MessagesChangedArea],
) {
    let event = events.try_recv().expect("messages.changed event");
    assert_eq!(event.event, "messages.changed");
    assert_eq!(event.seq, Some(expected_seq));
    let data: MessagesChangedData = serde_json::from_value(event.data).unwrap();
    assert_eq!(data.workspace_seq, expected_seq);
    assert_eq!(
        data.changed,
        expected.iter().copied().collect::<BTreeSet<_>>()
    );
}

/// Gate 1: a durable reply routes to the ORIGINAL endpoint after the
/// recipient's alias is renamed.
///
/// `reply` derives its recipient from the referenced message's sender
/// KEY, which is workspace plus session instance plus pane id and
/// carries no label. A rename replaces a directory entry, never a key,
/// so routing cannot follow it.
///
/// The trap this pins is the one a label-based route would fall into:
/// after the rename a DIFFERENT identity wears the sender's old label,
/// so a reply resolved by name would land on the impostor.
#[test]
fn a_reply_routes_to_the_original_endpoint_after_an_alias_rename() {
    let scratch = StoreScratch::new("reply-after-rename");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _admin, bob, carol) = test_context();

    let before = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "observer".into(),
            },
        ],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let (sender, _) = broadcast::channel(64);
    let service = MailboxService::new_with_events(before, store, sender);

    let parent = service
        .send(
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            mailbox_send("observer", "Question", "Body"),
        )
        .unwrap();

    // The rename, and the trap: bob takes a new label and carol takes
    // bob's old one, so "reviewer" now names a different endpoint.
    service
        .replace_directory(
            MailboxDirectory::new(
                workspace,
                [
                    MailboxIdentity {
                        key: bob,
                        label: "lead".into(),
                    },
                    MailboxIdentity {
                        key: carol,
                        label: "reviewer".into(),
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();

    let reply = service
        .reply(
            MailboxIdentity {
                key: carol,
                label: "reviewer".into(),
            },
            parent.message_id.clone(),
            "Answer".into(),
            None,
        )
        .unwrap();

    assert_eq!(
        reply.recipient_keys,
        vec![bob],
        "the reply left the original endpoint after the rename"
    );
    assert_ne!(
        reply.recipient_keys,
        vec![carol],
        "the reply followed the old label to its new owner"
    );
    // The presented name is the destination's CURRENT one. Rendering
    // the parent's historical label would print "reviewer", which now
    // names carol, so the row would route to bob while naming someone
    // else. The parent keeps its own historical label in its own fact.
    assert_eq!(
        reply.recipients,
        vec!["lead".to_string()],
        "the reply presented a stale label for the durable destination"
    );

    // The presentation is DURABLE: it is stamped into the ledger line
    // at acceptance, not re-derived on read. Replay it from the journal
    // under a fresh boot and the name must be the same one, so a later
    // rename cannot retroactively move it either way. The live service
    // holds the journal writer, so it goes first.
    let reply_id = reply.message_id.clone();
    drop(reply);
    drop(service);
    let replayed = MessageStore::open(&root, journal, workspace, "boot-replay").unwrap();
    let replayed_reply = replayed
        .projection()
        .get_message(&reply_id)
        .expect("the reply survives replay");
    assert_eq!(
        replayed_reply.to,
        vec!["lead".to_string()],
        "replay re-derived the label instead of reading the recorded fact"
    );
    let replayed_metadata = extract_message_metadata(replayed_reply).unwrap();
    assert_eq!(
        replayed_metadata.recipients,
        vec![bob],
        "replay lost the durable destination key"
    );
}

#[test]
fn committed_mailbox_facts_publish_once_in_workspace_sequence_order() {
    let scratch = StoreScratch::new("change-events");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "observer".into(),
            },
        ],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let (sender, _) = broadcast::channel(64);
    let mut events = sender.subscribe();
    let service = MailboxService::new_with_events(directory, store, sender);

    let first = service
        .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
        .unwrap();
    next_change(
        &mut events,
        1,
        &[
            MessagesChangedArea::Messages,
            MessagesChangedArea::Mailboxes,
        ],
    );
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    next_change(&mut events, 2, &[MessagesChangedArea::Notifications]);
    let context = crate::notification_adapter::NotificationContext::new_with_changes(
        service.store_handle(),
        first.message_id.clone(),
        bob,
        queued.attempt_id,
        service.change_publisher(),
    );
    context.record_gating().unwrap();
    next_change(&mut events, 3, &[MessagesChangedArea::Notifications]);
    context.record_gating().unwrap();
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    context
        .record_writing(
            notification_binding(bob).pane_root.unwrap(),
            notification_binding(bob).leader.unwrap(),
            notification_binding(bob).agent,
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    next_change(&mut events, 4, &[MessagesChangedArea::Notifications]);
    context.record_submitted().unwrap();
    next_change(&mut events, 5, &[MessagesChangedArea::Notifications]);
    context
        .record_notified(Some(cyclops_proto::VerifiedBy::Hook))
        .unwrap();
    next_change(&mut events, 6, &[MessagesChangedArea::Notifications]);
    context
        .record_notified(Some(cyclops_proto::VerifiedBy::Hook))
        .unwrap();
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    service.claim(bob, first.message_id).unwrap();
    next_change(&mut events, 7, &[MessagesChangedArea::Mailboxes]);

    let second = service
        .send(service.admin(), mailbox_send("reviewer", "Second", "Body"))
        .unwrap();
    next_change(
        &mut events,
        8,
        &[
            MessagesChangedArea::Messages,
            MessagesChangedArea::Mailboxes,
        ],
    );
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    next_change(&mut events, 9, &[MessagesChangedArea::Notifications]);
    let context = crate::notification_adapter::NotificationContext::new_with_changes(
        service.store_handle(),
        second.message_id.clone(),
        bob,
        queued.attempt_id,
        service.change_publisher(),
    );
    context.record_gating().unwrap();
    next_change(&mut events, 10, &[MessagesChangedArea::Notifications]);
    context
        .record_writing(
            notification_binding(bob).pane_root.unwrap(),
            notification_binding(bob).leader.unwrap(),
            notification_binding(bob).agent,
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    next_change(&mut events, 11, &[MessagesChangedArea::Notifications]);
    context
        .record_attention(NotificationAttentionCause::VerifyFailed)
        .unwrap();
    next_change(
        &mut events,
        12,
        &[
            MessagesChangedArea::Notifications,
            MessagesChangedArea::Attention,
        ],
    );
    service
        .clear_alarms(admin, &[queued.attempt_id], None)
        .unwrap();
    next_change(&mut events, 13, &[MessagesChangedArea::Attention]);
    service
        .clear_alarms(admin, &[queued.attempt_id], None)
        .unwrap();
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    service.claim(bob, second.message_id).unwrap();
    next_change(&mut events, 14, &[MessagesChangedArea::Mailboxes]);

    let third = service
        .send(
            MailboxIdentity {
                key: admin,
                label: "admin".into(),
            },
            mailbox_send("reviewer", "Third", "Body"),
        )
        .unwrap();
    next_change(
        &mut events,
        15,
        &[
            MessagesChangedArea::Messages,
            MessagesChangedArea::Mailboxes,
        ],
    );
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    next_change(&mut events, 16, &[MessagesChangedArea::Notifications]);
    let context = crate::notification_adapter::NotificationContext::new_with_changes(
        service.store_handle(),
        third.message_id.clone(),
        bob,
        queued.attempt_id,
        service.change_publisher(),
    );
    context.record_gating().unwrap();
    next_change(&mut events, 17, &[MessagesChangedArea::Notifications]);
    context
        .record_writing(
            notification_binding(bob).pane_root.unwrap(),
            notification_binding(bob).leader.unwrap(),
            notification_binding(bob).agent,
            "codex",
            NotificationTransport::Doorbell,
            None,
        )
        .unwrap();
    next_change(&mut events, 18, &[MessagesChangedArea::Notifications]);
    context
        .record_attention(NotificationAttentionCause::VerifyFailed)
        .unwrap();
    next_change(
        &mut events,
        19,
        &[
            MessagesChangedArea::Notifications,
            MessagesChangedArea::Attention,
        ],
    );
    service.requeue_message(third.message_id).unwrap();
    next_change(
        &mut events,
        20,
        &[
            MessagesChangedArea::Notifications,
            MessagesChangedArea::Attention,
        ],
    );
}

#[test]
fn supersession_and_claim_publish_distinct_notification_settlements() {
    let scratch = StoreScratch::new("supersession-change");
    let root = scratch.root();
    let (workspace, _, bob, carol) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "observer".into(),
            },
        ],
    )
    .unwrap();
    let store = MessageStore::open(
        &root,
        Path::new("workspaces/current/messages.ndjson"),
        workspace,
        "boot",
    )
    .unwrap();
    let (sender, _) = broadcast::channel(8);
    let mut events = sender.subscribe();
    let service = MailboxService::new_with_events(directory, store, sender);

    let first = service
        .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
        .unwrap();
    next_change(
        &mut events,
        1,
        &[
            MessagesChangedArea::Messages,
            MessagesChangedArea::Mailboxes,
        ],
    );
    service.prepare_oldest_notification(bob).unwrap().unwrap();
    next_change(&mut events, 2, &[MessagesChangedArea::Notifications]);

    let mut replacement = mailbox_send("reviewer", "Replacement", "Body");
    replacement.supersedes = Some(first.message_id);
    service.send(service.admin(), replacement).unwrap();
    next_change(
        &mut events,
        3,
        &[
            MessagesChangedArea::Messages,
            MessagesChangedArea::Mailboxes,
            MessagesChangedArea::Notifications,
        ],
    );

    let claimable = service
        .send(service.admin(), mailbox_send("observer", "Claim", "Body"))
        .unwrap();
    next_change(
        &mut events,
        4,
        &[
            MessagesChangedArea::Messages,
            MessagesChangedArea::Mailboxes,
        ],
    );
    service.prepare_oldest_notification(carol).unwrap().unwrap();
    next_change(&mut events, 5, &[MessagesChangedArea::Notifications]);
    let lines_before_claim = service.journal_lines().unwrap().len();
    service.claim(carol, claimable.message_id.clone()).unwrap();
    next_change(&mut events, 6, &[MessagesChangedArea::Mailboxes]);
    let record = service
        .store()
        .unwrap()
        .projection()
        .notification(carol, &claimable.message_id)
        .cloned()
        .unwrap();
    assert_eq!(record.state, NotificationState::Queued);
    let lines = service.journal_lines().unwrap();
    assert_eq!(lines.len(), lines_before_claim + 1);
    assert_eq!(
        lines.last().unwrap().data.as_ref().unwrap()["type"],
        "message_claimed"
    );
    assert!(lines.iter().all(|line| {
        line.data.as_ref().is_none_or(|data| {
            data["type"] != "notification_transition" || data["state"] != "withdrawn"
        })
    }));
    let dispositions = service.message_dispositions(&claimable.message_id).unwrap();
    assert_eq!(dispositions.len(), 1);
    assert_eq!(
        dispositions[0].notification_state,
        MessageNotificationState::Queued
    );
    assert_eq!(dispositions[0].notification_settlement, None);
    let snapshot = service.messages_snapshot(carol, 10).unwrap();
    let notification = &snapshot
        .rows
        .iter()
        .find(|row| row.message_id == claimable.message_id)
        .unwrap()
        .recipients[0]
        .notification;
    assert_eq!(notification.state, MessageNotificationState::Queued);
    assert_eq!(notification.settlement, None);
}

#[test]
fn an_unlabeled_pane_uses_its_pane_id_once() {
    let (workspace, _, recipient, _) = test_context();
    let pane = TmuxPaneId::from_str("%1").unwrap();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: recipient,
            label: pane.to_string(),
        }],
    )
    .unwrap();

    assert_eq!(
        directory.resolve(&[pane.to_string()]).unwrap()[0].key,
        recipient
    );
}

#[test]
fn duplicate_pane_ids_keep_exact_labels_and_broadcasts_but_refuse_raw_addressing() {
    let (workspace, _, first, _) = test_context();
    let pane = TmuxPaneId::from_str("%1").unwrap();
    let second_session =
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
    let second = RecipientKey::agent(workspace, second_session, pane);
    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: first,
                label: "reviewer".into(),
            },
            MailboxIdentity {
                key: second,
                label: "implementer".into(),
            },
        ],
    )
    .unwrap();

    assert!(directory.agent_for_pane(pane).is_none());
    assert_eq!(
        directory.resolve(&["reviewer".into()]).unwrap()[0].key,
        first
    );
    assert_eq!(
        directory.resolve(&["implementer".into()]).unwrap()[0].key,
        second
    );
    assert!(matches!(
        directory.resolve(&[pane.to_string()]),
        Err(MailboxDirectoryError::UnknownRecipient(_))
    ));
    assert_eq!(
        directory
            .resolve(&["*".into()])
            .unwrap()
            .into_iter()
            .map(|identity| identity.key)
            .collect::<HashSet<_>>(),
        HashSet::from([first, second])
    );
}

#[test]
fn exact_recipient_sends_use_current_identity_without_label_retargeting() {
    let scratch = StoreScratch::new("exact-recipient-send");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let pane = bob.pane_id().unwrap();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);

    let first = service
        .send(
            service.admin(),
            exact_mailbox_send(vec![bob, bob], "Exact", "Body", Some("exact-retry")),
        )
        .unwrap();
    assert_eq!(first.recipient_keys, [bob]);
    assert_eq!(first.recipients, ["reviewer"]);

    service
        .replace_directory(
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: bob,
                    label: "implementer".into(),
                }],
            )
            .unwrap(),
        )
        .unwrap();
    let retry = service
        .send(
            service.admin(),
            exact_mailbox_send(vec![bob], "Exact", "Body", Some("exact-retry")),
        )
        .unwrap();
    assert!(!retry.inserted);
    assert_eq!(retry.message_id, first.message_id);
    assert_eq!(retry.recipients, ["reviewer"]);

    let mut mixed_request = exact_mailbox_send(vec![bob], "Ambiguous", "", None);
    mixed_request.addresses.push("implementer".into());
    let mixed = service.send(service.admin(), mixed_request).unwrap_err();
    assert!(matches!(
        mixed,
        MailboxServiceError::Directory(MailboxDirectoryError::MixedRecipientSelectors)
    ));

    let replacement_session =
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
    let replacement = RecipientKey::agent(workspace, replacement_session, pane);
    service
        .replace_directory(
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: replacement,
                    label: "implementer".into(),
                }],
            )
            .unwrap(),
        )
        .unwrap();

    let stale = service
        .send(
            service.admin(),
            exact_mailbox_send(vec![bob], "Stale", "", None),
        )
        .unwrap_err();
    assert!(matches!(
        stale,
        MailboxServiceError::Directory(MailboxDirectoryError::UnknownRecipient(target))
            if target == bob.to_string()
    ));

    let current = service
        .send(
            service.admin(),
            exact_mailbox_send(vec![replacement], "Current", "", None),
        )
        .unwrap();
    assert_eq!(current.recipient_keys, [replacement]);
    assert_eq!(current.recipients, ["implementer"]);
}

#[test]
fn concurrent_claims_report_the_head_from_the_claim_lock() {
    let scratch = StoreScratch::new("claim-head-lock");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = std::sync::Arc::new(MailboxService::new(directory, store));
    let first = service
        .send(
            service.admin(),
            mailbox_send("reviewer", "First", "First body"),
        )
        .unwrap()
        .message_id;
    let second = service
        .send(
            service.admin(),
            mailbox_send("reviewer", "Second", "Second body"),
        )
        .unwrap()
        .message_id;

    let gate = std::sync::Arc::new(std::sync::Barrier::new(3));
    let first_worker = {
        let service = std::sync::Arc::clone(&service);
        let gate = std::sync::Arc::clone(&gate);
        let first = first.clone();
        std::thread::spawn(move || {
            gate.wait();
            service.claim(bob, first).unwrap()
        })
    };
    let second_worker = {
        let service = std::sync::Arc::clone(&service);
        let gate = std::sync::Arc::clone(&gate);
        let second = second.clone();
        std::thread::spawn(move || {
            gate.wait();
            service.claim(bob, second).unwrap()
        })
    };
    gate.wait();

    let first_outcome = first_worker.join().unwrap();
    let second_outcome = second_worker.join().unwrap();
    assert!(matches!(first_outcome, ClaimOutcome::Claimed { .. }));
    let ClaimOutcome::Claimed { skipped_oldest, .. } = second_outcome else {
        panic!("second message was not freshly claimed");
    };

    let store = service.store().unwrap();
    let first_seq = store
        .projection()
        .claim_sequences
        .get(&(bob, first.clone()))
        .copied()
        .unwrap();
    let second_seq = store
        .projection()
        .claim_sequences
        .get(&(bob, second))
        .copied()
        .unwrap();
    if first_seq < second_seq {
        assert_eq!(skipped_oldest, None);
    } else {
        assert_eq!(skipped_oldest, Some(first));
    }
}

#[test]
fn exact_recipient_validation_stays_locked_through_acceptance() {
    let scratch = StoreScratch::new("exact-recipient-linearization");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let pane = bob.pane_id().unwrap();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = Arc::new(MailboxService::new(directory, store));
    let (resolved_tx, resolved_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let sending = Arc::clone(&service);
    let send = std::thread::spawn(move || {
        sending.send_after_resolution(
            sending.admin(),
            exact_mailbox_send(vec![bob], "Exact", "Body", None),
            || {
                resolved_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            },
        )
    });

    resolved_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let directory_still_locked = matches!(
        service.directory.try_write(),
        Err(std::sync::TryLockError::WouldBlock)
    );
    release_tx.send(()).unwrap();
    let accepted = send.join().unwrap().unwrap();
    assert!(directory_still_locked);
    assert_eq!(accepted.recipient_keys, [bob]);

    let replacement_session =
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
    let replacement = RecipientKey::agent(workspace, replacement_session, pane);
    service
        .replace_directory(
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: replacement,
                    label: "implementer".into(),
                }],
            )
            .unwrap(),
        )
        .unwrap();
    let stale = service
        .send(
            service.admin(),
            exact_mailbox_send(vec![bob], "Stale", "", None),
        )
        .unwrap_err();
    assert!(matches!(
        stale,
        MailboxServiceError::Directory(MailboxDirectoryError::UnknownRecipient(target))
            if target == bob.to_string()
    ));
}

#[test]
fn service_directory_replacement_updates_routing_without_rewriting_messages() {
    let scratch = StoreScratch::new("directory-refresh");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let pane = TmuxPaneId::from_str("%1").unwrap();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let first = service
        .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
        .unwrap();
    let from_bob = service
        .send(
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            mailbox_send("admin", "From reviewer", "Body"),
        )
        .unwrap();

    service
        .replace_directory(
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: bob,
                    label: "implementer".into(),
                }],
            )
            .unwrap(),
        )
        .unwrap();
    assert!(service
        .send(service.admin(), mailbox_send("reviewer", "Stale label", ""))
        .is_err());
    service
        .send(
            service.admin(),
            mailbox_send("implementer", "After rename", ""),
        )
        .unwrap();

    // Reply routing is derived from the durable sender recorded on
    // the referenced message. The label shown at send time may be
    // stale, but a rename must not turn the reply into an address
    // lookup against that old label.
    let reply = service
        .reply(
            MailboxIdentity {
                key: bob,
                label: "implementer".into(),
            },
            first.message_id.clone(),
            "Reply after rename".into(),
            None,
        )
        .unwrap();
    let reply_message = service
        .store()
        .unwrap()
        .projection()
        .get_message(&reply.message_id)
        .cloned()
        .unwrap();
    let reply_metadata = extract_message_metadata(&reply_message).unwrap();
    assert_eq!(reply_message.from, "implementer");
    assert_eq!(reply_message.to, ["admin"]);
    assert_eq!(reply_metadata.sender, bob);
    assert_eq!(reply_metadata.recipients, [service.admin().key]);

    let admin_reply = service
        .reply(
            service.admin(),
            from_bob.message_id.clone(),
            "Reply to renamed sender".into(),
            None,
        )
        .unwrap();
    let store = service.store().unwrap();
    let admin_reply_message = store
        .projection()
        .get_message(&admin_reply.message_id)
        .unwrap();
    let admin_reply_metadata = extract_message_metadata(admin_reply_message).unwrap();
    // A reply is a NEW message and presents the destination's current
    // name. Rendering the parent's historical "reviewer" here would
    // address a label the directory has since moved, so the row would
    // route to bob while naming somebody else. The parent's own fact
    // keeps its historical label; this rewrites nothing.
    assert_eq!(admin_reply_message.to, ["implementer"]);
    assert_eq!(admin_reply_metadata.recipients, [bob]);
    drop(store);

    let replacement_session =
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
    let replacement_key = RecipientKey::agent(workspace, replacement_session, pane);
    service
        .replace_directory(
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: replacement_key,
                    label: "implementer".into(),
                }],
            )
            .unwrap(),
        )
        .unwrap();

    assert_eq!(
        service.agent_for_pane(pane).unwrap().unwrap().key,
        replacement_key
    );
    assert!(matches!(
        service.reply(
            service.admin(),
            from_bob.message_id.clone(),
            "Replacement must not receive the predecessor reply".into(),
            None,
        ),
        Err(MailboxServiceError::Directory(
            MailboxDirectoryError::UnknownRecipient(recipient)
        )) if recipient == bob.to_string()
    ));
    let replacement_reply = service
        .reply(
            MailboxIdentity {
                key: replacement_key,
                label: "implementer".into(),
            },
            first.message_id.clone(),
            "Replacement must not inherit the thread".into(),
            None,
        )
        .unwrap_err();
    assert!(matches!(
        replacement_reply,
        MailboxServiceError::Store(MessageStoreError::Mailbox(error))
            if matches!(error.as_ref(), MailboxError::ReplyNotVisible { sender, .. }
                if *sender == replacement_key)
    ));
    assert!(service
        .send(service.admin(), mailbox_send("reviewer", "Stale", ""))
        .is_err());
    service
        .send(service.admin(), mailbox_send("implementer", "Second", ""))
        .unwrap();

    let store = service.store().unwrap();
    let original = store.projection().get_message(&first.message_id).unwrap();
    assert_eq!(original.to, ["reviewer"]);
    assert_eq!(store.projection().get_pending(bob).len(), 3);
    assert_eq!(store.projection().get_pending(replacement_key).len(), 1);
    assert!(service
        .agent_for_pane(TmuxPaneId::from_str("%9").unwrap())
        .unwrap()
        .is_none());
}

#[test]
fn service_redacts_bodies_until_the_exact_recipient_claim() {
    let scratch = StoreScratch::new("body-privacy");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "bob".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "carol".into(),
            },
        ],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let accepted = service
        .send(
            MailboxIdentity {
                key: bob,
                label: "bob".into(),
            },
            mailbox_send("carol", "Private", "secret"),
        )
        .unwrap();
    let still_pending = service
        .send(
            MailboxIdentity {
                key: bob,
                label: "bob".into(),
            },
            mailbox_send("carol", "Other", "other secret"),
        )
        .unwrap();
    let message = service
        .journal_lines()
        .unwrap()
        .into_iter()
        .find(|line| line.id == accepted.message_id.as_str())
        .expect("canonical message line");

    let mut sender_view = vec![message.clone()];
    redact_message_bodies(Some(&service), Some(bob), &mut sender_view);
    assert_eq!(sender_view[0].body.as_deref(), Some("secret"));

    let mut recipient_view = vec![message.clone()];
    redact_message_bodies(Some(&service), Some(carol), &mut recipient_view);
    assert_eq!(recipient_view[0].body, None);

    let mut admin_view = vec![message.clone()];
    redact_message_bodies(Some(&service), Some(admin), &mut admin_view);
    assert_eq!(admin_view[0].body, None);

    let mut collision = message.clone();
    collision.body = Some("legacy collision body".into());
    collision.data = None;
    let mut collision_view = vec![collision];
    redact_message_bodies(Some(&service), Some(bob), &mut collision_view);
    assert_eq!(collision_view[0].body, None);

    service.claim(carol, accepted.message_id).unwrap();
    let mut claimed_view = vec![message.clone()];
    redact_message_bodies(Some(&service), Some(carol), &mut claimed_view);
    assert_eq!(claimed_view[0].body.as_deref(), Some("secret"));

    let mut other_view: Vec<_> = service
        .journal_lines()
        .unwrap()
        .into_iter()
        .filter(|line| line.id == still_pending.message_id.as_str())
        .collect();
    redact_message_bodies(Some(&service), Some(carol), &mut other_view);
    assert_eq!(other_view[0].body, None);

    let mut legacy = message;
    legacy.id = "m-legacy".into();
    legacy.data = None;
    for reader in [bob, carol, admin] {
        let mut lines = vec![legacy.clone()];
        redact_message_bodies(Some(&service), Some(reader), &mut lines);
        assert_eq!(lines[0].body, None);
    }
}

#[test]
fn direct_delivery_grants_recipient_body_access_and_counts_as_settled() {
    let scratch = StoreScratch::new("direct-body-visibility");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-direct-visible").unwrap();
    let attempt_id = attempt(44);
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(carol, vec![bob], "direct secret", None),
            1,
        )
        .unwrap();
    store
        .append_notification_transition_at(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Queued,
            None,
            None,
            2,
        )
        .unwrap();
    store
        .append_notification_transition_at(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Gating,
            None,
            None,
            3,
        )
        .unwrap();
    let binding = notification_binding(bob);
    for (ts, state) in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
        NotificationState::Notified,
    ]
    .into_iter()
    .enumerate()
    {
        if state == NotificationState::Writing {
            store
                .append_notification_transition_with_transport_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    state,
                    Some(binding.clone()),
                    Some(NotificationTransport::DirectPayload),
                    None,
                    None,
                    4 + ts as u64,
                )
                .unwrap();
        } else {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt_id,
                    state,
                    None,
                    None,
                    4 + ts as u64,
                )
                .unwrap();
        }
    }
    store
        .mark_delivered_direct_at(message_id.clone(), bob, attempt_id, 9)
        .unwrap();

    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "bob".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "carol".into(),
            },
        ],
    )
    .unwrap();
    let service = MailboxService::new(directory, store);
    let message = service
        .journal_lines()
        .unwrap()
        .into_iter()
        .find(|line| line.id == message_id.as_str() && line.kind == Kind::Msg)
        .unwrap();

    let mut recipient_view = vec![message.clone()];
    redact_message_bodies(Some(&service), Some(bob), &mut recipient_view);
    assert_eq!(recipient_view[0].body.as_deref(), Some("direct secret"));
    let mut admin_view = vec![message];
    redact_message_bodies(Some(&service), Some(admin), &mut admin_view);
    assert_eq!(admin_view[0].body, None);

    let snapshot = service.messages_snapshot(bob, 20).unwrap();
    assert_eq!(snapshot.counts.pending_entries, 0);
    assert_eq!(snapshot.counts.claimed_entries, 0);
    assert_eq!(snapshot.counts.active_messages, 0);
    assert_eq!(snapshot.counts.settled_messages, 1);
    assert_eq!(snapshot.counts.work_messages, 0);
    assert!(matches!(
        snapshot.rows[0].recipients[0].mailbox,
        MailboxEntryState::DeliveredDirect { .. }
    ));
}

#[test]
fn oldest_pending_notification_is_stable_and_resumes_after_restart() {
    let scratch = StoreScratch::new("oldest-notification");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = || {
        MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap()
    };

    let first_id;
    let second_id;
    let first_attempt;
    {
        let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let service = MailboxService::new(directory(), store);
        let first = service
            .send(service.admin(), mailbox_send("reviewer", "First", ""))
            .unwrap();
        let second = service
            .send(service.admin(), mailbox_send("reviewer", "Second", ""))
            .unwrap();
        first_id = first.message_id;
        second_id = second.message_id.clone();

        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        first_attempt = queued.attempt_id;
        assert_eq!(queued.message_id, first_id);
        assert_eq!(queued.state, NotificationState::Queued);
        assert_eq!(
            service
                .prepare_oldest_notification(bob)
                .unwrap()
                .unwrap()
                .attempt_id,
            first_attempt
        );
        assert!(service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &second.message_id)
            .is_none());
    }

    let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let service = MailboxService::new(directory(), store);
    let resumed = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(resumed.message_id, first_id);
    assert_eq!(resumed.attempt_id, first_attempt);
    assert_eq!(service.journal_lines().unwrap().len(), 3);

    service.claim(bob, first_id).unwrap();
    let same_after_claim = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(same_after_claim.attempt_id, first_attempt);
    service
        .withdraw_notification_before_write(service.admin().key, bob, first_attempt)
        .unwrap();
    let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(next.message_id, second_id);
    assert_ne!(next.attempt_id, first_attempt);
}

#[test]
fn socket_claim_does_not_skip_a_later_reply_doorbell() {
    let scratch = StoreScratch::new("claimed-replies-still-ring");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = || {
        MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap()
    };
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory(), store);
    let first = service
        .send(service.admin(), mailbox_send("reviewer", "First", ""))
        .unwrap();
    let second = service
        .send(service.admin(), mailbox_send("reviewer", "Second", ""))
        .unwrap();
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        first.message_id.clone(),
        bob,
        queued.attempt_id,
    );
    let binding = notification_binding(bob);
    context.record_gating().unwrap();
    context
        .record_writing(
            binding.pane_root.unwrap(),
            binding.leader.unwrap(),
            binding.agent,
            "codex",
            NotificationTransport::Doorbell,
            Some(4),
        )
        .unwrap();
    context.record_submitted().unwrap();
    context
        .record_notified(Some(cyclops_proto::VerifiedBy::Hook))
        .unwrap();
    service.claim(bob, first.message_id).unwrap();

    let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(next.message_id, second.message_id);
    assert_eq!(next.state, NotificationState::Queued);
}

#[test]
fn claimed_message_without_prior_notification_does_not_block_later_pending_doorbell() {
    let scratch = StoreScratch::new("claimed-no-notification-still-rings");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = || {
        MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap()
    };
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory(), store);
    let first = service
        .send(service.admin(), mailbox_send("reviewer", "First", ""))
        .unwrap();
    service.claim(bob, first.message_id).unwrap();

    let second = service
        .send(service.admin(), mailbox_send("reviewer", "Second", ""))
        .unwrap();
    let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(next.message_id, second.message_id);
    assert_eq!(next.state, NotificationState::Queued);
}

#[test]
fn blocked_binding_reopens_on_new_evidence_or_binding_and_keeps_fifo_identity() {
    let scratch = StoreScratch::new("blocked-binding-reopen");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let first = service
        .send(service.admin(), mailbox_send("reviewer", "First", ""))
        .unwrap();
    let second = service
        .send(service.admin(), mailbox_send("reviewer", "Second", ""))
        .unwrap();
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        first.message_id.clone(),
        bob,
        queued.attempt_id,
    );
    context.record_gating().unwrap();
    let evidence = |generation| {
        Some(NotificationRouteEvidenceId {
            boot_id: "boot".into(),
            generation,
        })
    };
    let failed_observation = NotificationPreWriteObservation {
        pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
        binding: Some(notification_binding(bob)),
        route_evidence: evidence(7),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    };
    let lines_before_inner_schedule = service.journal_lines().unwrap().len();
    assert!(
            service
                .reopen_oldest_notification_after_route_evidence(
                    bob,
                    failed_observation.clone(),
                    false,
                )
                .unwrap()
                .is_none(),
            "the inner schedule must not move an attempt that is still Gating"
        );
    assert_eq!(
        service.journal_lines().unwrap().len(),
        lines_before_inner_schedule
    );

    // The durable block lands between the readiness schedule inside the
    // recompute and the event source's follow-on schedule. Both schedules
    // carry one evidence identity, so the second call is still a no-op.
    context
        .record_pre_write_block(
            NotificationPreWriteCause::BindingUnprovable,
            Some(failed_observation.clone()),
        )
        .unwrap();
    assert!(service.prepare_oldest_notification(bob).unwrap().is_none());

    let lines_before_repeat = service.journal_lines().unwrap().len();
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, failed_observation.clone(), false,)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);

    drop(context);
    drop(service);
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let replayed_store = MessageStore::open(&root, journal, workspace, "boot-replay").unwrap();
    let service = MailboxService::new(directory, replayed_store);
    let replayed_before_reopen = service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &first.message_id)
        .cloned()
        .unwrap();
    assert_eq!(
        replayed_before_reopen.state,
        NotificationState::BlockedPreWrite
    );
    assert_eq!(replayed_before_reopen.pre_write_reopen_count, 0);
    let lines_before_replayed_evidence = service.journal_lines().unwrap().len();
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, failed_observation.clone(), false,)
        .unwrap()
        .is_none());
    let stale_observation = NotificationPreWriteObservation {
        route_evidence: evidence(6),
        ..failed_observation.clone()
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, stale_observation, false)
        .unwrap()
        .is_none());
    assert_eq!(
        service.journal_lines().unwrap().len(),
        lines_before_replayed_evidence
    );

    let cross_pane_observation = NotificationPreWriteObservation {
        pane_root: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
        binding: Some(notification_binding(bob)),
        route_evidence: evidence(8),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, cross_pane_observation, false,)
        .unwrap()
        .is_none());
    let missing_leader_observation = NotificationPreWriteObservation {
        pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
        binding: Some(NotificationBinding {
            leader: None,
            ..notification_binding(bob)
        }),
        route_evidence: evidence(8),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, missing_leader_observation, false,)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);

    let proven_observation = NotificationPreWriteObservation {
        route_evidence: evidence(8),
        ..failed_observation.clone()
    };
    let reopened = service
        .reopen_oldest_notification_after_route_evidence(bob, proven_observation.clone(), false)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.message_id, first.message_id);
    assert_eq!(reopened.attempt_id, queued.attempt_id);
    assert_eq!(reopened.state, NotificationState::Gating);
    assert_eq!(
        service
            .prepare_oldest_notification(bob)
            .unwrap()
            .unwrap()
            .attempt_id,
        queued.attempt_id
    );
    assert_eq!(reopened.pre_write_reopen_count, 1);
    assert!(!service.journal_lines().unwrap().iter().any(|line| {
        line.data
            .as_ref()
            .is_some_and(|data| data["type"] == "notification_requeued")
    }));

    let reblocked_observation = NotificationPreWriteObservation {
        pane_root: failed_observation.pane_root,
        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
        binding: None,
        route_evidence: evidence(8),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    };
    let reopened_context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        first.message_id.clone(),
        bob,
        queued.attempt_id,
    );
    reopened_context
        .record_pre_write_block(
            NotificationPreWriteCause::BindingUnprovable,
            Some(reblocked_observation),
        )
        .unwrap();
    let second_proof = NotificationPreWriteObservation {
        pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
        binding: Some(NotificationBinding {
            agent: ProcessInstanceId::new(4243, 818_222).unwrap(),
            ..notification_binding(bob)
        }),
        route_evidence: evidence(9),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    };
    let lines_before_second_proof = service.journal_lines().unwrap().len();
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, second_proof.clone(), false,)
        .unwrap()
        .is_none());
    assert_eq!(
        service.journal_lines().unwrap().len(),
        lines_before_second_proof
    );

    drop(reopened_context);
    drop(service);
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let reopened_store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let service = MailboxService::new(directory, reopened_store);
    let replayed = service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &first.message_id)
        .cloned()
        .unwrap();
    assert_eq!(replayed.state, NotificationState::BlockedPreWrite);
    assert_eq!(replayed.pre_write_reopen_count, 1);
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, second_proof, false)
        .unwrap()
        .is_none());

    service.claim(bob, first.message_id).unwrap();
    assert!(service.prepare_oldest_notification(bob).unwrap().is_none());
    service
        .withdraw_notification_before_write(service.admin().key, bob, queued.attempt_id)
        .unwrap();
    let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(next.message_id, second.message_id);
    assert_ne!(next.attempt_id, queued.attempt_id);

    let next_context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        second.message_id.clone(),
        bob,
        next.attempt_id,
    );
    next_context.record_gating().unwrap();
    next_context
        .record_pre_write_block(
            NotificationPreWriteCause::BindingUnprovable,
            Some(failed_observation.clone()),
        )
        .unwrap();
    let changed_binding = NotificationBinding {
        pane_root: Some(ProcessInstanceId::new(4999, 917_999).unwrap()),
        leader: Some(ProcessInstanceId::new(5000, 918_000).unwrap()),
        agent: ProcessInstanceId::new(5242, 918_221).unwrap(),
        ..notification_binding(bob)
    };
    let changed_observation = NotificationPreWriteObservation {
        write_block: None,
        pane_root: changed_binding.pane_root,
        selected_manifest: Some(changed_binding.manifest.clone()),
        binding: Some(changed_binding),
        route_evidence: evidence(8),
        pane_width: None,
        required_pane_width: None,
    };
    let reopened = service
        .reopen_oldest_notification_after_route_evidence(bob, changed_observation, false)
        .unwrap()
        .expect("a changed complete binding remains positive evidence");
    assert_eq!(reopened.message_id, second.message_id);
    assert_eq!(reopened.attempt_id, next.attempt_id);
    assert_eq!(reopened.pre_write_reopen_count, 1);
}

#[test]
fn blocked_readiness_reopens_once_only_after_positive_exact_route_evidence() {
    let scratch = StoreScratch::new("blocked-readiness-reopen");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let message = service
        .send(service.admin(), mailbox_send("reviewer", "Ready", ""))
        .unwrap();
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        message.message_id.clone(),
        bob,
        queued.attempt_id,
    );
    context.record_gating().unwrap();
    let evidence = |generation| {
        Some(NotificationRouteEvidenceId {
            boot_id: "boot".into(),
            generation,
        })
    };
    let blocked_observation = NotificationPreWriteObservation {
        pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
        selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
        binding: Some(notification_binding(bob)),
        route_evidence: evidence(7),
        pane_width: None,
        required_pane_width: None,
        write_block: None,
    };
    context
        .record_pre_write_block(
            NotificationPreWriteCause::WriteReadinessChanged,
            Some(blocked_observation.clone()),
        )
        .unwrap();

    let lines_before = service.journal_lines().unwrap().len();
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, blocked_observation.clone(), true,)
        .unwrap()
        .is_none());
    let stale_observation = NotificationPreWriteObservation {
        route_evidence: evidence(6),
        ..blocked_observation.clone()
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, stale_observation, true)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before);

    let later_observation = NotificationPreWriteObservation {
        route_evidence: evidence(8),
        ..blocked_observation
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, later_observation.clone(), false,)
        .unwrap()
        .is_none());
    let reopened = service
        .reopen_oldest_notification_after_route_evidence(bob, later_observation.clone(), true)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.attempt_id, queued.attempt_id);
    assert_eq!(reopened.state, NotificationState::Gating);
    assert_eq!(reopened.pre_write_reopen_count, 1);

    let reopened_context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        message.message_id,
        bob,
        queued.attempt_id,
    );
    reopened_context
        .record_pre_write_block(
            NotificationPreWriteCause::WriteReadinessChanged,
            Some(later_observation.clone()),
        )
        .unwrap();
    let lines_before_repeat = service.journal_lines().unwrap().len();
    let final_observation = NotificationPreWriteObservation {
        route_evidence: evidence(9),
        ..later_observation
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, final_observation, true)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);
}

/// A `composer_semantic_ambiguous` block settles a wake whose composer
/// kept reading ambiguous on an idle pane. Unlike the static
/// `composer_semantic_missing` gap it may reopen without anyone
/// repairing anything — but only on LATER route evidence whose cached
/// verdict is actually write-ready. Ambiguity is not evidence: neither
/// unchanged-generation write-readiness nor later still-ambiguous
/// frames reopen it, and the automatic reopen spends once.
#[test]
fn blocked_ambiguous_composer_reopens_once_only_on_later_write_ready_evidence() {
    let scratch = StoreScratch::new("blocked-ambiguous-reopen");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let message = service
        .send(service.admin(), mailbox_send("reviewer", "Wake", ""))
        .unwrap();
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        message.message_id.clone(),
        bob,
        queued.attempt_id,
    );
    context.record_gating().unwrap();
    let evidence = |generation| {
        Some(NotificationRouteEvidenceId {
            boot_id: "boot".into(),
            generation,
        })
    };
    let blocked_observation = NotificationPreWriteObservation {
        pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
        selected_manifest: Some(NotificationManifestId::new("cursor").unwrap()),
        binding: Some(NotificationBinding {
            manifest: NotificationManifestId::new("cursor").unwrap(),
            ..notification_binding(bob)
        }),
        route_evidence: evidence(7),
        pane_width: None,
        required_pane_width: None,
        write_block: Some("composer_semantic_ambiguous".into()),
    };
    context
        .record_pre_write_block(
            NotificationPreWriteCause::WriteReadinessChanged,
            Some(blocked_observation.clone()),
        )
        .unwrap();

    // The durable detail remains an additive observation beside the
    // long-standing closed cause. Strict NDJSON replay must therefore
    // rebuild it without asking a newer binary to understand a new enum
    // spelling, and old JSON readers can ignore the optional detail.
    let live_record = service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &message.message_id)
        .cloned()
        .expect("live blocked record");
    drop(context);
    drop(service);
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let replayed_store = MessageStore::open(&root, journal, workspace, "boot-replay").unwrap();
    let service = MailboxService::new(directory, replayed_store);
    let replayed = service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &message.message_id)
        .cloned()
        .expect("strict replayed blocked record");
    assert_eq!(replayed, live_record);
    assert_eq!(
        replayed.pre_write_cause,
        Some(NotificationPreWriteCause::WriteReadinessChanged)
    );
    assert_eq!(
        replayed
            .pre_write_observation
            .as_ref()
            .and_then(|observation| observation.write_block.as_deref()),
        Some("composer_semantic_ambiguous")
    );

    // The same generation cannot reopen, however ready it claims to be:
    // this is the evidence the block was recorded against.
    let lines_before = service.journal_lines().unwrap().len();
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, blocked_observation.clone(), true,)
        .unwrap()
        .is_none());
    // Later evidence that is still not write-ready is still ambiguity.
    let later_observation = NotificationPreWriteObservation {
        route_evidence: evidence(8),
        ..blocked_observation.clone()
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, later_observation.clone(), false,)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before);

    // Later, write-ready evidence is the frame the manifest could
    // finally prove: the wake reopens, once.
    let reopened = service
        .reopen_oldest_notification_after_route_evidence(bob, later_observation.clone(), true)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.attempt_id, queued.attempt_id);
    assert_eq!(reopened.state, NotificationState::Gating);
    assert_eq!(reopened.pre_write_reopen_count, 1);

    let reopened_context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        message.message_id,
        bob,
        queued.attempt_id,
    );
    reopened_context
        .record_pre_write_block(
            NotificationPreWriteCause::WriteReadinessChanged,
            Some(later_observation.clone()),
        )
        .unwrap();
    let lines_before_repeat = service.journal_lines().unwrap().len();
    let final_observation = NotificationPreWriteObservation {
        route_evidence: evidence(9),
        ..later_observation
    };
    assert!(service
        .reopen_oldest_notification_after_route_evidence(bob, final_observation, true)
        .unwrap()
        .is_none());
    assert_eq!(service.journal_lines().unwrap().len(), lines_before_repeat);
}

#[test]
fn worker_ownership_loss_is_journaled_and_live_projection_equals_replay() {
    let scratch = StoreScratch::new("scheduler-wake-block-replay");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let make_directory = || {
        MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap()
    };
    let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    let service = MailboxService::new(make_directory(), store);
    let accepted = service
        .send(service.admin(), mailbox_send("reviewer", "Task", ""))
        .unwrap();
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        accepted.message_id.clone(),
        bob,
        queued.attempt_id,
    );
    context.record_gating().unwrap();
    context
        .record_pre_write_block_with_wake_block(
            NotificationPreWriteCause::WorkerFailed,
            None,
            Some(MessageWakeBlock::WorkerSupervisorExited),
        )
        .unwrap();

    assert_eq!(
        service.notification_schedule_block(bob).unwrap(),
        Some(NotificationScheduleBlock {
            message_id: accepted.message_id.clone(),
            attempt_id: queued.attempt_id,
            block: MessageWakeBlock::WorkerSupervisorExited,
        })
    );
    let live_record = service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &accepted.message_id)
        .cloned()
        .unwrap();
    let snapshot = service.messages_snapshot(service.admin().key, 10).unwrap();
    let live_summary = snapshot.rows[0].recipients[0].notification.clone();
    assert_eq!(
        live_summary.wake_block,
        Some(MessageWakeBlock::WorkerSupervisorExited)
    );

    drop(context);
    drop(service);
    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let service = MailboxService::new(make_directory(), reopened);
    assert_eq!(
        service.notification_schedule_block(bob).unwrap(),
        Some(NotificationScheduleBlock {
            message_id: accepted.message_id.clone(),
            attempt_id: queued.attempt_id,
            block: MessageWakeBlock::WorkerSupervisorExited,
        })
    );
    let replayed_record = service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &accepted.message_id)
        .cloned()
        .unwrap();
    assert_eq!(replayed_record, live_record);
    let snapshot = service.messages_snapshot(service.admin().key, 10).unwrap();
    assert_eq!(snapshot.rows[0].recipients[0].notification, live_summary);
    assert_eq!(
        snapshot.rows[0].recipients[0].notification.wake_block,
        Some(MessageWakeBlock::WorkerSupervisorExited)
    );
}

#[test]
fn operator_withdrawal_is_durable_idempotent_and_advances_notification_fifo() {
    let scratch = StoreScratch::new("operator-prewrite-withdrawal");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _carol) = test_context();
    let make_directory = || {
        MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap()
    };
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(make_directory(), store);
    let first = service
        .send(
            service.admin(),
            mailbox_send("reviewer", "First", "claimable body"),
        )
        .unwrap();
    let second = service
        .send(service.admin(), mailbox_send("reviewer", "Second", ""))
        .unwrap();
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        first.message_id.clone(),
        bob,
        queued.attempt_id,
    );
    context.record_gating().unwrap();
    context
        .record_pre_write_block(
            NotificationPreWriteCause::BindingUnprovable,
            Some(NotificationPreWriteObservation {
                pane_root: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
                selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                binding: None,
                route_evidence: None,
                pane_width: None,
                required_pane_width: None,
                write_block: None,
            }),
        )
        .unwrap();

    let admin_blocked = service.messages_snapshot(admin, 10).unwrap();
    let blocked_row = admin_blocked
        .rows
        .iter()
        .find(|row| row.message_id == first.message_id)
        .unwrap();
    assert!(blocked_row.needs_action);
    assert_eq!(blocked_row.recipients[0].fifo_position, Some(1));
    assert!(blocked_row.recipients[0].can_withdraw_notification);
    assert_eq!(
        blocked_row.recipients[0]
            .current_route
            .as_ref()
            .map(|route| route.label.as_str()),
        Some("reviewer")
    );
    assert_eq!(
        blocked_row.recipients[0].notification.pre_write_cause,
        Some(NotificationPreWriteCause::BindingUnprovable)
    );
    let status_blocked = service.blocked_notification_snapshot(now_ms(), 32).unwrap();
    assert_eq!(status_blocked.total, 1);
    assert_eq!(status_blocked.rows.len(), 1);
    assert_eq!(
        status_blocked.rows[0].notification_attempt,
        queued.attempt_id
    );
    let recipient_blocked = service.messages_snapshot(bob, 10).unwrap();
    assert!(recipient_blocked.rows[0].needs_action);
    assert!(!recipient_blocked.rows[0].recipients[0].can_withdraw_notification);

    let before = service.journal_lines().unwrap().len();
    let (withdrawn, inserted) = service
        .withdraw_notification_before_write(admin, bob, queued.attempt_id)
        .unwrap();
    assert!(inserted);
    assert_eq!(withdrawn.state, NotificationState::WithdrawnByOperator);
    assert_eq!(service.journal_lines().unwrap().len(), before + 1);
    let line = service.journal_lines().unwrap().pop().unwrap();
    assert_eq!(line.from, admin.to_string());
    assert_eq!(line.to, vec![bob.to_string()]);
    assert!(line.subject.is_none() && line.body.is_none() && line.reply_to.is_none());
    assert!(line.deliveries.is_empty());
    assert_eq!(
        line.data.as_ref().unwrap()["type"],
        "notification_withdrawn_before_write"
    );

    let snapshot = service.messages_snapshot(admin, 10).unwrap();
    let first_notification = &snapshot
        .rows
        .iter()
        .find(|row| row.message_id == first.message_id)
        .unwrap()
        .recipients[0]
        .notification;
    assert_eq!(
        first_notification.state,
        MessageNotificationState::NotStarted
    );
    assert_eq!(first_notification.settlement, None);
    assert_eq!(first_notification.operator_withdrawn, Some(true));
    let first_recipient = &snapshot
        .rows
        .iter()
        .find(|row| row.message_id == first.message_id)
        .unwrap()
        .recipients[0];
    assert!(!first_recipient.can_withdraw_notification);
    assert_eq!(
            first_recipient.fifo_position, None,
            "the still-pending, withdrawn mailbox entry is pullable but no longer a notification FIFO item"
        );
    let second_recipient = &snapshot
        .rows
        .iter()
        .find(|row| row.message_id == second.message_id)
        .unwrap()
        .recipients[0];
    assert_eq!(
        second_recipient.fifo_position,
        Some(1),
        "the next actionable wake is first after the withdrawn head"
    );
    let first_disposition = service.message_dispositions(&first.message_id).unwrap();
    assert_eq!(
        first_disposition[0].position_ahead, None,
        "a withdrawn wake has no notification queue position"
    );
    let second_disposition = service.message_dispositions(&second.message_id).unwrap();
    assert_eq!(
        second_disposition[0].position_ahead,
        Some(0),
        "sender receipts count only actionable wakes ahead"
    );
    let status_blocked = service.blocked_notification_snapshot(now_ms(), 32).unwrap();
    assert_eq!(status_blocked.total, 0);
    assert!(status_blocked.rows.is_empty());
    assert!(
        !snapshot
            .rows
            .iter()
            .find(|row| row.message_id == first.message_id)
            .unwrap()
            .needs_action
    );

    let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(next.message_id, second.message_id);
    assert_ne!(next.attempt_id, queued.attempt_id);
    let before_repeat = service.journal_lines().unwrap().len();
    let (_, inserted) = service
        .withdraw_notification_before_write(admin, bob, queued.attempt_id)
        .unwrap();
    assert!(!inserted);
    assert_eq!(service.journal_lines().unwrap().len(), before_repeat);
    assert!(service
        .reopen_oldest_notification_after_route_evidence(
            bob,
            NotificationPreWriteObservation {
                pane_root: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
                selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                binding: Some(notification_binding(bob)),
                route_evidence: None,
                pane_width: None,
                required_pane_width: None,
                write_block: None,
            },
            true,
        )
        .unwrap()
        .is_none());

    drop(context);
    drop(service);
    let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let service = MailboxService::new(make_directory(), store);
    let before_restart_repeat = service.journal_lines().unwrap().len();
    let (_, inserted) = service
        .withdraw_notification_before_write(admin, bob, queued.attempt_id)
        .unwrap();
    assert!(!inserted);
    assert_eq!(
        service.journal_lines().unwrap().len(),
        before_restart_repeat
    );
    assert_eq!(
        service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &second.message_id)
            .unwrap()
            .attempt_id,
        next.attempt_id
    );

    let ClaimOutcome::Claimed { message, .. } =
        service.claim(bob, first.message_id.clone()).unwrap()
    else {
        panic!("the withdrawn wake must not consume the message");
    };
    assert_eq!(message.body.as_deref(), Some("claimable body"));
    assert_eq!(
        service
            .store()
            .unwrap()
            .projection()
            .notification(bob, &first.message_id)
            .unwrap()
            .state,
        NotificationState::WithdrawnByOperator
    );
}

#[test]
fn operator_withdraws_queued_and_gating_wakes_without_promoting_them_to_work() {
    let scratch = StoreScratch::new("operator-unwritten-withdrawal");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let make_directory = || {
        MailboxDirectory::new(
            workspace,
            [MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            }],
        )
        .unwrap()
    };
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(make_directory(), store);
    let queued_message = service
        .send(service.admin(), mailbox_send("reviewer", "Queued", ""))
        .unwrap();
    let gating_message = service
        .send(service.admin(), mailbox_send("reviewer", "Gating", ""))
        .unwrap();

    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let snapshot = service.messages_snapshot(admin, 10).unwrap();
    let queued_row = snapshot
        .rows
        .iter()
        .find(|row| row.message_id == queued_message.message_id)
        .unwrap();
    assert!(queued_row.recipients[0].can_withdraw_notification);
    assert!(!queued_row.needs_action);
    assert_eq!(snapshot.counts.work_messages, 0);

    service
        .withdraw_notification_before_write(admin, bob, queued.attempt_id)
        .unwrap();
    let gating = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(gating.message_id, gating_message.message_id);
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        gating.message_id.clone(),
        bob,
        gating.attempt_id,
    );
    context.record_gating().unwrap();

    let snapshot = service.messages_snapshot(admin, 10).unwrap();
    let gating_row = snapshot
        .rows
        .iter()
        .find(|row| row.message_id == gating_message.message_id)
        .unwrap();
    assert!(gating_row.recipients[0].can_withdraw_notification);
    assert!(!gating_row.needs_action);
    assert_eq!(snapshot.counts.work_messages, 0);

    service
        .withdraw_notification_before_write(admin, bob, gating.attempt_id)
        .unwrap();
    drop(context);
    drop(service);

    let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let service = MailboxService::new(make_directory(), store);
    for message_id in [&queued_message.message_id, &gating_message.message_id] {
        assert_eq!(
            service
                .store()
                .unwrap()
                .projection()
                .notification(bob, message_id)
                .unwrap()
                .state,
            NotificationState::WithdrawnByOperator
        );
    }
}

#[test]
fn blocked_status_sample_ignores_unrelated_message_volume() {
    let scratch = StoreScratch::new("blocked-status-unrelated-volume");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let blocked = service
        .send(
            service.admin(),
            mailbox_send("reviewer", "Blocked", "secret"),
        )
        .unwrap();
    let attempt = service.prepare_oldest_notification(bob).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        blocked.message_id.clone(),
        bob,
        attempt.attempt_id,
    );
    context.record_gating().unwrap();
    context
        .record_pre_write_block(
            NotificationPreWriteCause::BindingUnprovable,
            Some(NotificationPreWriteObservation {
                pane_root: None,
                selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                binding: None,
                route_evidence: None,
                pane_width: None,
                required_pane_width: None,
                write_block: None,
            }),
        )
        .unwrap();

    let mut unrelated = None;
    for index in 0..128 {
        unrelated = Some(
            service
                .send(
                    service.admin(),
                    mailbox_send("reviewer", &format!("Other {index}"), "other secret"),
                )
                .unwrap()
                .message_id,
        );
    }
    // Corrupt an unrelated message's presentation metadata in memory.
    // A full message snapshot must inspect it and fail. The specialized
    // status query is driven only by current blocked notifications.
    service
        .store()
        .unwrap()
        .projection
        .messages
        .get_mut(&unrelated.unwrap())
        .unwrap()
        .data = None;

    let sample = service.blocked_notification_snapshot(now_ms(), 32).unwrap();
    assert_eq!(sample.total, 1);
    assert_eq!(sample.rows.len(), 1);
    assert_eq!(sample.rows[0].message_id, blocked.message_id);
    assert!(service.messages_snapshot(service.admin().key, 0).is_err());
}

#[test]
fn blocked_status_sample_is_capped_and_deterministic() {
    let scratch = StoreScratch::new("blocked-status-cap");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
    let session = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
    let recipients: Vec<_> = (1..=7)
        .map(|index| {
            RecipientKey::agent(
                workspace,
                session,
                TmuxPaneId::from_str(&format!("%{index}")).unwrap(),
            )
        })
        .collect();
    let directory = MailboxDirectory::new(
        workspace,
        recipients
            .iter()
            .enumerate()
            .map(|(index, recipient)| MailboxIdentity {
                key: *recipient,
                label: format!("agent-{index}"),
            }),
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let message = service
        .send(
            service.admin(),
            MailboxSend {
                addresses: vec!["*".into()],
                recipient_keys: None,
                subject: "Broadcast".into(),
                summary: None,
                body: String::new(),
                fyi: false,
                client_key: None,
                supersedes: None,
                raw: false,
            },
        )
        .unwrap();
    for recipient in recipients {
        let attempt = service
            .prepare_oldest_notification(recipient)
            .unwrap()
            .unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            message.message_id.clone(),
            recipient,
            attempt.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(NotificationPreWriteObservation {
                    pane_root: None,
                    selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                    binding: None,
                    route_evidence: None,
                    pane_width: None,
                    required_pane_width: None,
                    write_block: None,
                }),
            )
            .unwrap();
    }

    let now = now_ms().saturating_add(100);
    let first = service.blocked_notification_snapshot(now, 4).unwrap();
    let second = service.blocked_notification_snapshot(now, 4).unwrap();
    assert_eq!(first.total, 7);
    assert_eq!(first.rows.len(), 4);
    assert_eq!(first.rows, second.rows);
    assert!(first.rows.iter().all(|row| {
        row.recipient.fifo_position == Some(1)
            && row.recipient.current_route.is_some()
            && row.recipient.can_withdraw_notification
    }));
}

#[test]
fn operator_withdrawal_accepts_claimed_prewrite_but_refuses_inexact_or_post_write_targets() {
    let scratch = StoreScratch::new("operator-prewrite-withdrawal-refusals");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "reviewer".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "implementer".into(),
            },
        ],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let first = service
        .send(service.admin(), mailbox_send("reviewer", "First", ""))
        .unwrap();
    let bob_attempt = service.prepare_oldest_notification(bob).unwrap().unwrap();

    let assert_refused_without_append = |result: Result<_, MailboxServiceError>| {
        assert!(result.is_err());
    };
    let before = service.journal_lines().unwrap().len();
    assert_refused_without_append(service.withdraw_notification_before_write(
        bob,
        bob,
        bob_attempt.attempt_id,
    ));
    assert_refused_without_append(service.withdraw_notification_before_write(
        admin,
        carol,
        bob_attempt.attempt_id,
    ));
    assert_refused_without_append(service.withdraw_notification_before_write(
        admin,
        bob,
        attempt(999),
    ));
    assert_eq!(service.journal_lines().unwrap().len(), before);

    service.claim(bob, first.message_id).unwrap();
    let before_claimed = service.journal_lines().unwrap().len();
    let (withdrawn, inserted) = service
        .withdraw_notification_before_write(admin, bob, bob_attempt.attempt_id)
        .unwrap();
    assert!(inserted);
    assert_eq!(withdrawn.state, NotificationState::WithdrawnByOperator);
    assert_eq!(service.journal_lines().unwrap().len(), before_claimed + 1);

    let post_write = service
        .send(service.admin(), mailbox_send("implementer", "Writing", ""))
        .unwrap();
    let carol_attempt = service.prepare_oldest_notification(carol).unwrap().unwrap();
    let context = crate::notification_adapter::NotificationContext::new(
        service.store_handle(),
        post_write.message_id,
        carol,
        carol_attempt.attempt_id,
    );
    context.record_gating().unwrap();
    context
        .record_writing(
            ProcessInstanceId::new(4999, 899_999).unwrap(),
            ProcessInstanceId::new(5000, 900_000).unwrap(),
            ProcessInstanceId::new(5001, 900_001).unwrap(),
            "codex",
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
        )
        .unwrap();
    let before_writing = service.journal_lines().unwrap().len();
    assert_refused_without_append(service.withdraw_notification_before_write(
        admin,
        carol,
        carol_attempt.attempt_id,
    ));
    assert_eq!(service.journal_lines().unwrap().len(), before_writing);
}

#[test]
fn sender_filter_runs_before_the_oldest_message_limit() {
    let scratch = StoreScratch::new("sender-filter-before-limit");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let reviewer = service
        .identity_for_recipient(bob)
        .unwrap()
        .expect("reviewer identity");
    let admin = service.admin();

    service
        .send(
            reviewer,
            mailbox_send("reviewer", "Older self message", "private"),
        )
        .unwrap();
    service
        .send(
            admin.clone(),
            mailbox_send("reviewer", "Newer admin message", "private"),
        )
        .unwrap();

    let listed = service.list(bob, Some(admin.key), Some(1)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].sender, admin.key);
    assert_eq!(listed[0].subject.as_deref(), Some("Newer admin message"));
}

#[test]
fn concurrent_senders_share_one_oldest_notification_attempt() {
    let scratch = StoreScratch::new("concurrent-oldest-notification");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = Arc::new(MailboxService::new(directory, store));
    let admin = service.admin();
    let reviewer = service
        .identity_for_recipient(bob)
        .unwrap()
        .expect("reviewer identity");
    let start = Arc::new(std::sync::Barrier::new(3));

    let send = |sender: MailboxIdentity, subject: &'static str| {
        let service = Arc::clone(&service);
        let start = Arc::clone(&start);
        std::thread::spawn(move || {
            start.wait();
            service
                .send(sender, mailbox_send("reviewer", subject, "private"))
                .unwrap()
        })
    };
    let from_admin = send(admin, "From admin");
    let from_reviewer = send(reviewer, "From reviewer");
    start.wait();
    let mut accepted = [from_admin.join().unwrap(), from_reviewer.join().unwrap()];
    accepted.sort_by_key(|result| result.seq);
    assert!(accepted[0].seq < accepted[1].seq);

    let prepare = Arc::new(std::sync::Barrier::new(3));
    let notify = || {
        let service = Arc::clone(&service);
        let prepare = Arc::clone(&prepare);
        std::thread::spawn(move || {
            prepare.wait();
            service.prepare_oldest_notification(bob).unwrap().unwrap()
        })
    };
    let first_prepare = notify();
    let second_prepare = notify();
    prepare.wait();
    let oldest = first_prepare.join().unwrap();
    let concurrent = second_prepare.join().unwrap();
    assert_eq!(concurrent.attempt_id, oldest.attempt_id);
    assert_eq!(concurrent.message_id, oldest.message_id);
    assert_eq!(oldest.message_id, accepted[0].message_id);
    let lines_after_first_attempt = service.journal_lines().unwrap().len();
    let same = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(same.attempt_id, oldest.attempt_id);
    assert_eq!(
        service.journal_lines().unwrap().len(),
        lines_after_first_attempt
    );
    assert!(service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &accepted[1].message_id)
        .is_none());

    let ClaimOutcome::Claimed {
        withdrawn_attempt, ..
    } = service.claim(bob, accepted[0].message_id.clone()).unwrap()
    else {
        panic!("oldest message was not freshly claimed");
    };
    assert_eq!(withdrawn_attempt, None);
    let same_after_claim = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(same_after_claim.message_id, oldest.message_id);
    assert_eq!(same_after_claim.attempt_id, oldest.attempt_id);

    let store = service.store().unwrap();
    assert_eq!(
        store
            .projection()
            .notification(bob, &accepted[0].message_id)
            .unwrap()
            .state,
        NotificationState::Queued
    );
    assert!(store
        .projection()
        .notification(bob, &accepted[1].message_id)
        .is_none());
}

#[test]
fn claim_keeps_post_write_attention_open_in_its_own_fact() {
    let scratch = StoreScratch::new("claim-withdrawal");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    let service = MailboxService::new(directory, store);
    let accepted = service
        .send(service.admin(), mailbox_send("reviewer", "Task", "Body"))
        .unwrap();
    let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
    {
        let mut store = service.store().unwrap();
        alarm(&mut store, &accepted.message_id, bob, queued.attempt_id, 3);
    }

    let ClaimOutcome::Claimed {
        withdrawn_attempt, ..
    } = service.claim(bob, accepted.message_id.clone()).unwrap()
    else {
        panic!("first claim must append a claim fact");
    };
    assert_eq!(withdrawn_attempt, None);
    let lines = service.journal_lines().unwrap();
    assert_eq!(lines.len(), 6);
    assert_eq!(lines[5].data.as_ref().unwrap()["type"], "message_claimed");
    let attention = service
        .store()
        .unwrap()
        .projection()
        .notification(bob, &accepted.message_id)
        .cloned()
        .unwrap();
    assert_eq!(attention.state, NotificationState::AttentionRequired);
    assert_eq!(attention.updated_seq, lines[4].seq);
    assert_eq!(
        attention.cause,
        Some(NotificationAttentionCause::SubmitFailed)
    );
    assert_eq!(service.store().unwrap().projection().open_alarms().len(), 1);
    assert_eq!(
        service
            .store()
            .unwrap()
            .projection()
            .active_notification_barriers()
            .len(),
        1
    );
    let admin_snapshot = service.messages_snapshot(service.admin().key, 10).unwrap();
    let recipient = &admin_snapshot
        .rows
        .iter()
        .find(|row| row.message_id == accepted.message_id)
        .unwrap()
        .recipients[0];
    assert_eq!(
        recipient.notification.state,
        MessageNotificationState::AttentionRequired
    );
    assert!(recipient.can_manage_attention);

    drop(service);
    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    assert_eq!(
        reopened
            .projection()
            .notification(bob, &accepted.message_id)
            .unwrap()
            .state,
        NotificationState::AttentionRequired
    );
    assert_eq!(reopened.projection().open_alarms().len(), 1);
    assert_eq!(
        reopened.projection().active_notification_barriers().len(),
        1
    );
}

#[test]
fn claim_publishes_only_the_mailbox_when_attention_stays_open() {
    let scratch = StoreScratch::new("claim-attention-change");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let (sender, _) = broadcast::channel(8);
    let mut events = sender.subscribe();
    let service = MailboxService::new_with_events(directory, store, sender);
    let accepted = service
        .send(service.admin(), mailbox_send("reviewer", "Task", "Body"))
        .unwrap();
    next_change(
        &mut events,
        1,
        &[
            MessagesChangedArea::Messages,
            MessagesChangedArea::Mailboxes,
        ],
    );
    {
        let mut store = service.store().unwrap();
        alarm(&mut store, &accepted.message_id, bob, attempt(1), 2);
    }
    let claim_seq = service
        .store()
        .unwrap()
        .projection()
        .last_sequence()
        .unwrap()
        + 1;

    service.claim(bob, accepted.message_id).unwrap();
    next_change(&mut events, claim_seq, &[MessagesChangedArea::Mailboxes]);
    assert_eq!(service.store().unwrap().projection().open_alarms().len(), 1);
}

fn draft(
    sender: RecipientKey,
    recipients: Vec<RecipientKey>,
    body: &str,
    client_key: Option<&str>,
) -> MessageDraft {
    let presentation = test_presentation(&recipients);
    MessageDraft {
        kind: Kind::Msg,
        sender,
        recipients,
        subject: Some("Task".into()),
        summary: None,
        body: Some(body.into()),
        client_key: client_key.map(str::to_string),
        supersedes: None,
        presentation,
        raw: false,
    }
}

fn reply_draft(sender: RecipientKey, reference: MessageId, body: &str) -> ReplyDraft {
    ReplyDraft {
        sender,
        reference,
        summary: None,
        body: Some(body.into()),
        client_key: None,
        sender_label: "reply-sender".into(),
        recipient_label: "reply-recipient".into(),
        raw: false,
    }
}

fn test_presentation(recipients: &[RecipientKey]) -> MessagePresentation {
    MessagePresentation {
        sender_label: "sender-label".into(),
        recipient_labels: recipients
            .iter()
            .enumerate()
            .map(|(index, recipient)| RecipientPresentation {
                recipient: *recipient,
                label: format!("recipient-{index}"),
            })
            .collect(),
    }
}

fn assert_no_key(value: &serde_json::Value, forbidden: &str) {
    match value {
        serde_json::Value::Object(fields) => {
            assert!(
                !fields.contains_key(forbidden),
                "found forbidden key {forbidden}"
            );
            for value in fields.values() {
                assert_no_key(value, forbidden);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                assert_no_key(value, forbidden);
            }
        }
        _ => {}
    }
}

#[test]
fn outbound_snapshot_survives_reopen_without_body_keys() {
    let scratch = StoreScratch::new("snapshot-reopen");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, carol) = test_context();
    let message_id = MessageId::new("m-outbound").unwrap();

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(bob, vec![carol], "secret body", None),
                100,
            )
            .unwrap();
        let snapshot = store
            .projection()
            .messages_snapshot(bob, 20, &routes([bob, carol]))
            .unwrap();
        assert_eq!(snapshot.rows[0].direction, MessageDirection::Outbound);
        let json = serde_json::to_value(&snapshot).unwrap();
        for forbidden in ["body", "binding", "capture", "composer", "diff"] {
            assert_no_key(&json, forbidden);
        }
        assert!(!json.to_string().contains("secret body"));
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let snapshot = reopened
        .projection()
        .messages_snapshot(bob, 20, &routes([bob, carol]))
        .unwrap();
    assert_eq!(snapshot.workspace_seq, 1);
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].message_id, message_id);
    assert_eq!(snapshot.rows[0].direction, MessageDirection::Outbound);
}

#[test]
fn snapshot_retains_claims_and_recomputes_fifo_positions() {
    let scratch = StoreScratch::new("snapshot-claim-fifo");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let first = MessageId::new("m-first").unwrap();
    let second = MessageId::new("m-second").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(first.clone(), draft(admin, vec![bob], "first", None), 100)
        .unwrap();
    store
        .accept_at(second.clone(), draft(admin, vec![bob], "second", None), 200)
        .unwrap();

    let before = store
        .projection()
        .messages_snapshot(bob, 20, &routes([bob]))
        .unwrap();
    assert_eq!(before.rows[0].recipients[0].fifo_position, Some(1));
    assert_eq!(before.rows[1].recipients[0].fifo_position, Some(2));
    assert_eq!(
        before.rows[0].recipients[0].notification.state,
        MessageNotificationState::NotStarted
    );
    assert!(before.rows[0].recipients[0]
        .notification
        .attempt_id
        .is_none());

    store.claim_at(bob, first.clone(), 300).unwrap();
    let after = store
        .projection()
        .messages_snapshot(bob, 20, &routes([bob]))
        .unwrap();
    let first_row = after
        .rows
        .iter()
        .find(|row| row.message_id == first)
        .unwrap();
    let second_row = after
        .rows
        .iter()
        .find(|row| row.message_id == second)
        .unwrap();
    assert!(first_row.recipients[0].mailbox.is_claimed());
    assert_eq!(first_row.recipients[0].fifo_position, None);
    assert_eq!(second_row.recipients[0].fifo_position, Some(1));
    assert_eq!(after.counts.pending_entries, 1);
    assert_eq!(after.counts.claimed_entries, 1);
    assert_eq!(after.workspace_seq, 3);
}

#[test]
fn broadcast_snapshot_keeps_each_recipient_attempt_and_clearance() {
    let scratch = StoreScratch::new("snapshot-broadcast");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-broadcast").unwrap();
    let bob_attempt = attempt(1);
    let carol_attempt = attempt(2);
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob, carol], "broadcast", None),
            100,
        )
        .unwrap();

    store
        .queue_notification(message_id.clone(), bob, bob_attempt)
        .unwrap();
    for (state, binding) in [
        (NotificationState::Gating, None),
        (NotificationState::Writing, Some(notification_binding(bob))),
        (NotificationState::Staged, None),
        (NotificationState::Submitting, None),
        (NotificationState::Submitted, None),
        (NotificationState::Notified, None),
    ] {
        store
            .advance_notification(message_id.clone(), bob, bob_attempt, state, binding, None)
            .unwrap();
    }
    store.claim(bob, message_id.clone()).unwrap();

    store
        .queue_notification(message_id.clone(), carol, carol_attempt)
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            carol,
            carol_attempt,
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            carol,
            carol_attempt,
            NotificationState::Writing,
            Some(notification_binding(carol)),
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            carol,
            carol_attempt,
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::VerifyFailed),
        )
        .unwrap();
    store
        .clear_notification(message_id.clone(), carol, carol_attempt)
        .unwrap();

    let snapshot = store
        .projection()
        .messages_snapshot(admin, 20, &routes([admin, bob, carol]))
        .unwrap();
    let row = &snapshot.rows[0];
    let bob_state = row
        .recipients
        .iter()
        .find(|recipient| recipient.recipient == bob)
        .unwrap();
    let carol_state = row
        .recipients
        .iter()
        .find(|recipient| recipient.recipient == carol)
        .unwrap();
    assert!(bob_state.mailbox.is_claimed());
    assert_eq!(
        bob_state.notification.state,
        MessageNotificationState::Notified
    );
    assert_eq!(bob_state.notification.attempt_id, Some(bob_attempt));
    assert!(carol_state.mailbox.is_pending());
    assert_eq!(carol_state.fifo_position, Some(1));
    assert_eq!(
        carol_state.notification.state,
        MessageNotificationState::AttentionRequired
    );
    assert_eq!(
        carol_state.notification.cause,
        Some(NotificationAttentionCause::VerifyFailed)
    );
    assert_eq!(carol_state.notification.attention_cleared, Some(true));
    assert_eq!(snapshot.counts.open_attention_entries, 0);
}

#[test]
fn snapshot_denies_nonparticipant_visibility_at_the_projection_boundary() {
    let (workspace, _, bob, carol) = test_context();
    let other_session =
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
    let dave = RecipientKey::agent(
        workspace,
        other_session,
        TmuxPaneId::from_str("%3").unwrap(),
    );
    let mut projection = MailboxProjection::new(workspace);
    projection
        .apply_line(&sample_msg_line(
            1,
            "m-private",
            workspace,
            bob,
            vec![carol],
            Kind::Msg,
            None,
            "private body",
        ))
        .unwrap();

    assert_eq!(
        projection
            .messages_snapshot(bob, 20, &routes([bob, carol]))
            .unwrap()
            .rows
            .len(),
        1
    );
    assert_eq!(
        projection
            .messages_snapshot(carol, 20, &routes([bob, carol]))
            .unwrap()
            .rows
            .len(),
        1
    );
    let denied = projection
        .messages_snapshot(dave, 20, &routes([bob, carol, dave]))
        .unwrap();
    assert!(denied.rows.is_empty());
    assert_eq!(denied.counts.visible_messages, 0);
    let admin = RecipientKey::admin(workspace);
    assert_eq!(
        projection
            .messages_snapshot(admin, 20, &routes([bob, carol]))
            .unwrap()
            .rows
            .len(),
        1
    );
}

#[test]
fn snapshot_bounds_settled_rows_without_hiding_thread_counts() {
    let scratch = StoreScratch::new("snapshot-settled-bound");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, carol) = test_context();
    let root_id = MessageId::new("m-thread-root").unwrap();
    let reply_id = MessageId::new("m-thread-reply").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(root_id.clone(), draft(bob, vec![carol], "root", None), 100)
        .unwrap();
    store.claim_at(carol, root_id.clone(), 200).unwrap();
    store
        .reply_at(
            reply_id.clone(),
            reply_draft(carol, root_id.clone(), "reply"),
            300,
        )
        .unwrap();
    store.claim_at(bob, reply_id.clone(), 400).unwrap();

    let snapshot = store
        .projection()
        .messages_snapshot(bob, 1, &routes([bob, carol]))
        .unwrap();
    assert_eq!(snapshot.counts.visible_messages, 2);
    assert_eq!(snapshot.counts.settled_messages, 2);
    assert_eq!(snapshot.counts.returned_messages, 1);
    assert_eq!(snapshot.counts.inbox_messages, 1);
    assert_eq!(snapshot.counts.outbound_messages, 1);
    assert_eq!(snapshot.counts.work_messages, 0);
    assert_eq!(snapshot.rows[0].message_id, reply_id);
    assert_eq!(snapshot.rows[0].thread_root, root_id);
    assert_eq!(snapshot.rows[0].thread_message_count, 2);
}

#[test]
fn follow_pages_every_settled_message_beyond_the_snapshot_tail() {
    let scratch = StoreScratch::new("follow-settled-burst");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let mut expected = Vec::new();
    for index in 0..25 {
        let message_id = MessageId::new(format!("m-burst-{index:02}")).unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "settled", None),
                100 + index,
            )
            .unwrap();
        store
            .claim_at(bob, message_id.clone(), 200 + index)
            .unwrap();
        expected.push(message_id);
    }

    let queue = store
        .projection()
        .messages_snapshot(admin, 20, &routes([admin, bob]))
        .unwrap();
    assert_eq!(queue.rows.len(), 20, "the queue tail stays bounded");

    let mut cursor = 0;
    let mut followed = Vec::new();
    loop {
        let page = store
            .projection()
            .messages_follow(admin, cursor, 7, &routes([admin, bob]))
            .unwrap();
        assert_eq!(page.after_seq, cursor);
        followed.extend(page.rows.iter().map(|row| row.message_id.clone()));
        cursor = page.through_seq;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(followed, expected);
    assert_eq!(cursor, queue.workspace_seq);
}

#[test]
fn ten_thousand_message_snapshot_uses_the_mailbox_lookup_index() {
    const MESSAGE_COUNT: u64 = 10_000;
    const NOTIFICATION_COUNT: u64 = 100;

    let (workspace, admin, bob, _) = test_context();
    let mut projection = MailboxProjection::new(workspace);
    let mut seq = 0_u64;

    for number in 0..MESSAGE_COUNT {
        seq += 1;
        let id = format!("m-scale-{number:05}");
        projection
            .apply_line(&sample_msg_line(
                seq,
                &id,
                workspace,
                admin,
                vec![bob],
                Kind::Msg,
                None,
                "body",
            ))
            .unwrap();
    }

    for number in (0..MESSAGE_COUNT).step_by(2) {
        seq += 1;
        let id = MessageId::new(format!("m-scale-{number:05}")).unwrap();
        projection
            .apply_line(&sample_claim_line(seq, id, bob))
            .unwrap();
    }

    for number in (1..(NOTIFICATION_COUNT * 2)).step_by(2) {
        seq += 1;
        let id = MessageId::new(format!("m-scale-{number:05}")).unwrap();
        projection
            .apply_line(&sample_queued_notification_line(
                seq,
                id,
                bob,
                attempt(number),
            ))
            .unwrap();
    }

    // Each index value points into the authoritative FIFO map. The index
    // carries no mailbox state and keeps point reads out of a linear scan.
    assert_eq!(projection.mailbox_index.len(), MESSAGE_COUNT as usize);
    let last_id = MessageId::new("m-scale-09999").unwrap();
    assert_eq!(projection.get_entry(bob, &last_id).unwrap().seq, 10_000);

    let snapshot = projection
        .messages_snapshot(bob, 20, &routes([bob]))
        .unwrap();
    assert_eq!(snapshot.counts.visible_messages, MESSAGE_COUNT);
    assert_eq!(snapshot.counts.pending_entries, MESSAGE_COUNT / 2);
    assert_eq!(snapshot.counts.claimed_entries, MESSAGE_COUNT / 2);
    assert_eq!(snapshot.counts.work_messages, MESSAGE_COUNT / 2);
    assert_eq!(snapshot.counts.returned_messages, MESSAGE_COUNT / 2 + 20);
    assert_eq!(
        snapshot
            .rows
            .iter()
            .filter(|row| {
                row.recipients[0].notification.state == MessageNotificationState::Queued
            })
            .count(),
        NOTIFICATION_COUNT as usize
    );
}

#[test]
fn work_is_pending_for_an_agent_and_uncleared_attention_for_admin() {
    let scratch = StoreScratch::new("snapshot-work");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-work").unwrap();
    let attempt_id = attempt(9);
    let available = routes([admin, bob]);
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob], "work", None),
            100,
        )
        .unwrap();

    let agent = store
        .projection()
        .messages_snapshot(bob, 20, &available)
        .unwrap();
    let admin_before = store
        .projection()
        .messages_snapshot(admin, 20, &available)
        .unwrap();
    assert_eq!(agent.counts.work_messages, 1);
    assert!(agent.rows[0].needs_action);
    assert!(!agent.rows[0].recipients[0].can_manage_attention);
    assert_eq!(admin_before.counts.work_messages, 0);
    assert!(!admin_before.rows[0].needs_action);
    assert!(!admin_before.rows[0].recipients[0].can_manage_attention);

    store
        .queue_notification(message_id.clone(), bob, attempt_id)
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Writing,
            Some(notification_binding(bob)),
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::VerifyFailed),
        )
        .unwrap();

    let admin_attention = store
        .projection()
        .messages_snapshot(admin, 20, &available)
        .unwrap();
    assert_eq!(admin_attention.counts.work_messages, 1);
    assert!(admin_attention.rows[0].needs_action);
    assert!(admin_attention.rows[0].recipients[0].can_manage_attention);

    store
        .clear_notification(message_id, bob, attempt_id)
        .unwrap();
    let admin_cleared = store
        .projection()
        .messages_snapshot(admin, 20, &available)
        .unwrap();
    assert_eq!(admin_cleared.counts.work_messages, 0);
    assert!(!admin_cleared.rows[0].needs_action);
    assert!(!admin_cleared.rows[0].recipients[0].can_manage_attention);
}

#[test]
fn recipient_work_and_direction_do_not_spread_across_a_broadcast() {
    let scratch = StoreScratch::new("snapshot-broadcast-rows");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-broadcast-rows").unwrap();
    let available = routes([admin, bob, carol]);
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob, carol], "broadcast", None),
            100,
        )
        .unwrap();

    let bob_snapshot = store
        .projection()
        .messages_snapshot(bob, 20, &available)
        .unwrap();
    let bob_row = &bob_snapshot.rows[0];
    let bob_entry = bob_row
        .recipients
        .iter()
        .find(|entry| entry.recipient == bob)
        .unwrap();
    let carol_entry = bob_row
        .recipients
        .iter()
        .find(|entry| entry.recipient == carol)
        .unwrap();
    assert_eq!(bob_entry.direction, MessageDirection::Inbound);
    assert!(bob_entry.needs_action);
    assert_eq!(carol_entry.direction, MessageDirection::Workspace);
    assert!(!carol_entry.needs_action);

    let attempt_id = attempt(10);
    store
        .queue_notification(message_id.clone(), carol, attempt_id)
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            carol,
            attempt_id,
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            carol,
            attempt_id,
            NotificationState::Writing,
            Some(notification_binding(carol)),
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id,
            carol,
            attempt_id,
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::VerifyFailed),
        )
        .unwrap();

    let admin_snapshot = store
        .projection()
        .messages_snapshot(admin, 20, &available)
        .unwrap();
    let admin_row = &admin_snapshot.rows[0];
    let bob_entry = admin_row
        .recipients
        .iter()
        .find(|entry| entry.recipient == bob)
        .unwrap();
    let carol_entry = admin_row
        .recipients
        .iter()
        .find(|entry| entry.recipient == carol)
        .unwrap();
    assert_eq!(bob_entry.direction, MessageDirection::Outbound);
    assert!(!bob_entry.needs_action);
    assert!(!bob_entry.can_manage_attention);
    assert_eq!(carol_entry.direction, MessageDirection::Outbound);
    assert!(carol_entry.needs_action);
    assert!(carol_entry.can_manage_attention);
    assert!(admin_row.needs_action);

    let bob_snapshot = store
        .projection()
        .messages_snapshot(bob, 20, &available)
        .unwrap();
    assert!(bob_snapshot.rows[0]
        .recipients
        .iter()
        .all(|entry| !entry.can_manage_attention));
}

#[test]
fn route_replacement_does_not_make_the_old_recipient_available() {
    let scratch = StoreScratch::new("snapshot-route-replacement");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, old_recipient, _) = test_context();
    let pane = TmuxPaneId::from_str("%1").unwrap();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: old_recipient,
            label: "reviewer".into(),
        }],
    )
    .unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    let service = MailboxService::new(directory, store);
    let old_message = service
        .send(service.admin(), mailbox_send("reviewer", "Old", "body"))
        .unwrap()
        .message_id;
    assert!(
        service
            .messages_snapshot(service.admin().key, 20)
            .unwrap()
            .rows[0]
            .recipients[0]
            .available
    );

    let replacement_session =
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
    let replacement = RecipientKey::agent(workspace, replacement_session, pane);
    service
        .replace_directory(
            MailboxDirectory::new(
                workspace,
                [MailboxIdentity {
                    key: replacement,
                    label: "reviewer".into(),
                }],
            )
            .unwrap(),
        )
        .unwrap();
    let new_message = service
        .send(service.admin(), mailbox_send("reviewer", "New", "body"))
        .unwrap()
        .message_id;
    let snapshot = service.messages_snapshot(service.admin().key, 20).unwrap();
    let old = snapshot
        .rows
        .iter()
        .find(|row| row.message_id == old_message)
        .unwrap();
    let new = snapshot
        .rows
        .iter()
        .find(|row| row.message_id == new_message)
        .unwrap();
    assert!(!old.recipients[0].available);
    assert_eq!(old.recipients[0].recipient, old_recipient);
    assert!(new.recipients[0].available);
    assert_eq!(new.recipients[0].recipient, replacement);
}

#[allow(clippy::too_many_arguments)]
fn sample_msg_line(
    seq: u64,
    id: &str,
    ws: WorkspaceId,
    sender: RecipientKey,
    recipients: Vec<RecipientKey>,
    kind: Kind,
    client_key: Option<&str>,
    body: &str,
) -> LedgerLine {
    let msg_id = MessageId::new(id).unwrap();
    let digest = RequestDigest::compute(
        kind,
        sender,
        &recipients,
        RequestContent {
            subject: Some("Task"),
            summary: None,
            body: Some(body),
        },
        None,
        None,
    )
    .unwrap();
    let presentation = MessagePresentation {
        sender_label: sender.to_string(),
        recipient_labels: recipients
            .iter()
            .map(|recipient| RecipientPresentation {
                recipient: *recipient,
                label: recipient.to_string(),
            })
            .collect(),
    };

    let metadata = MessageMetadata {
        record_version: CANONICAL_RECORD_VERSION,
        workspace_id: ws,
        sender,
        recipients: recipients.clone(),
        presentation,
        summary: None,
        thread_root: msg_id,
        client_key: client_key.map(String::from),
        request_digest: digest,
        supersedes: None,
        raw: false,
    };

    LedgerLine {
        seq,
        boot_id: "boot-1".into(),
        id: id.into(),
        ts: 1_700_000_000_000 + seq,
        kind,
        from: sender.to_string(),
        to: recipients.iter().map(|r| r.to_string()).collect(),
        subject: Some("Task".into()),
        body: Some(body.into()),
        reply_to: None,
        deliveries: vec![],
        data: Some(serde_json::to_value(metadata).unwrap()),
    }
}

fn sample_claim_line(seq: u64, message_id: MessageId, recipient: RecipientKey) -> LedgerLine {
    LedgerLine {
        seq,
        boot_id: "boot-1".into(),
        id: message_id.to_string(),
        ts: 1_700_000_000_000 + seq,
        kind: Kind::State,
        from: recipient.to_string(),
        to: Vec::new(),
        subject: None,
        body: None,
        reply_to: None,
        deliveries: Vec::new(),
        data: Some(
            serde_json::to_value(MailboxFact::MessageClaimed {
                record_version: CANONICAL_RECORD_VERSION,
                message_id,
                recipient,
                claimant: recipient,
            })
            .unwrap(),
        ),
    }
}

fn sample_notification_state_line(
    seq: u64,
    message_id: MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    state: NotificationState,
) -> LedgerLine {
    LedgerLine {
        seq,
        boot_id: "boot-1".into(),
        id: message_id.to_string(),
        ts: 1_700_000_000_000 + seq,
        kind: Kind::State,
        from: "cyclopsd".into(),
        to: vec![recipient.to_string()],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: Vec::new(),
        data: Some(
            serde_json::to_value(NotificationFact::NotificationTransition {
                record_version: CANONICAL_RECORD_VERSION,
                attempt_id,
                message_id,
                recipient,
                state,
                binding: None,
                transport: None,
                doorbell_format: None,
                cause: None,
                verified_by: None,
                verify_outcome: None,
                pre_write_cause: None,
                wake_block: None,
                pre_write_observation: None,
            })
            .unwrap(),
        ),
    }
}

fn sample_queued_notification_line(
    seq: u64,
    message_id: MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
) -> LedgerLine {
    sample_notification_state_line(
        seq,
        message_id,
        recipient,
        attempt_id,
        NotificationState::Queued,
    )
}

#[test]
fn presentation_mismatch_fails_closed_with_failure_atomicity() {
    let (ws, admin, bob, _) = test_context();
    let mut proj = MailboxProjection::new(ws);

    let mut line_from = sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "Body");
    line_from.from = "ContradictorySenderPresentation".into();

    let err = proj.apply_line(&line_from).unwrap_err();
    assert!(matches!(
        err,
        MailboxError::PresentationMismatch { field: "from", .. }
    ));
    assert_eq!(proj.last_sequence(), None);
    assert_eq!(proj.get_pending(bob).len(), 0);

    let mut line_to = sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "Body");
    line_to.to = vec!["ContradictoryRecipientPresentation".into()];

    let err = proj.apply_line(&line_to).unwrap_err();
    assert!(matches!(
        err,
        MailboxError::PresentationMismatch { field: "to", .. }
    ));
    assert_eq!(proj.last_sequence(), None);
    assert_eq!(proj.get_pending(bob).len(), 0);
}

#[test]
fn legacy_session_lines_without_metadata_rejected_by_projection() {
    let (ws, _admin, bob, _) = test_context();
    let mut proj = MailboxProjection::new(ws);

    let legacy_session_line = LedgerLine {
        seq: 1,
        boot_id: "boot-legacy".into(),
        id: "m-legacy".into(),
        ts: 1_700_000_000_000,
        kind: Kind::Msg,
        from: "alice".into(),
        to: vec!["bob".into()],
        subject: Some("Old".into()),
        body: Some("Old message".into()),
        reply_to: None,
        deliveries: vec![],
        data: None, // No MessageMetadata
    };

    let err = proj.apply_line(&legacy_session_line).unwrap_err();
    assert!(matches!(err, MailboxError::MissingMetadata(_)));
    assert_eq!(proj.last_sequence(), None);
    assert_eq!(proj.get_pending(bob).len(), 0);
}

#[test]
fn uncanonical_record_version_rejected() {
    let (ws, admin, bob, _) = test_context();
    let mut proj = MailboxProjection::new(ws);

    let mut line = sample_msg_line(1, "m-badver", ws, admin, vec![bob], Kind::Msg, None, "A");
    let mut meta: MessageMetadata = serde_json::from_value(line.data.clone().unwrap()).unwrap();
    meta.record_version = 999;
    line.data = Some(serde_json::to_value(meta).unwrap());

    let err = proj.apply_line(&line).unwrap_err();
    assert_eq!(
        err,
        MailboxError::InvalidRecordVersion {
            expected: CANONICAL_RECORD_VERSION,
            found: 999
        }
    );
    assert_eq!(proj.last_sequence(), None);
    assert_eq!(proj.get_pending(bob).len(), 0);
}

#[test]
fn pre_append_acceptance_separates_retries_and_conflicts() {
    let (ws, admin, bob, _) = test_context();
    let proj = MailboxProjection::new(ws);

    let draft_1 = CanonicalDraft {
        kind: Kind::Msg,
        sender: admin,
        recipients: vec![bob],
        subject: Some("Task".into()),
        summary: None,
        body: Some("B1".into()),
        reply_to: None,
        client_key: Some("key-1".into()),
        supersedes: None,
        presentation: test_presentation(&[bob]),
        raw: false,
    };

    let outcome = proj.check_acceptance(&draft_1).unwrap();
    assert!(matches!(outcome, AcceptanceOutcome::New { .. }));

    let mut active_proj = proj;
    let line = sample_msg_line(
        1,
        "m-1",
        ws,
        admin,
        vec![bob],
        Kind::Msg,
        Some("key-1"),
        "B1",
    );
    active_proj.apply_line(&line).unwrap();

    let retry = active_proj.check_acceptance(&draft_1).unwrap();
    assert_eq!(
        retry,
        AcceptanceOutcome::Existing(MessageId::new("m-1").unwrap())
    );

    let draft_conflict = CanonicalDraft {
        kind: Kind::Msg,
        sender: admin,
        recipients: vec![bob],
        subject: Some("Task".into()),
        summary: None,
        body: Some("B2_DIFF".into()),
        reply_to: None,
        client_key: Some("key-1".into()),
        supersedes: None,
        presentation: test_presentation(&[bob]),
        raw: false,
    };
    let err = active_proj.check_acceptance(&draft_conflict).unwrap_err();
    assert!(matches!(err, MailboxError::DuplicateIdempotencyKey { .. }));
}

#[test]
fn invalid_summary_is_refused_before_acceptance_changes_projection_state() {
    let (workspace, admin, recipient, _) = test_context();
    let projection = MailboxProjection::new(workspace);
    let draft = CanonicalDraft {
        kind: Kind::Msg,
        sender: admin,
        recipients: vec![recipient],
        subject: Some("Review".into()),
        summary: Some("First line.\nSecond line.".into()),
        body: Some("Private body".into()),
        reply_to: None,
        client_key: Some("invalid-summary".into()),
        supersedes: None,
        presentation: test_presentation(&[recipient]),
        raw: false,
    };

    assert!(matches!(
        projection.check_acceptance(&draft),
        Err(MailboxError::Type(MailboxTypeError::InvalidMessageSummary))
    ));
    assert_eq!(projection.last_sequence(), None);
    assert!(projection.get_pending(recipient).is_empty());
}

#[test]
fn strict_monotonic_workspace_sequence_and_failure_atomicity() {
    let (ws, admin, bob, _) = test_context();
    let mut proj = MailboxProjection::new(ws);

    let line_seq1 = sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "A");
    let line_seq3 = sample_msg_line(3, "m-3", ws, admin, vec![bob], Kind::Msg, None, "B");

    proj.apply_line(&line_seq1).unwrap();
    assert_eq!(proj.get_pending(bob).len(), 1);
    assert_eq!(proj.last_sequence(), Some(1));

    let err = proj.apply_line(&line_seq3).unwrap_err();
    assert_eq!(
        err,
        MailboxError::NonContiguousSequence {
            expected: 2,
            found: 3
        }
    );

    assert_eq!(proj.last_sequence(), Some(1));
    assert_eq!(proj.get_pending(bob).len(), 1);
    assert!(proj.get_message(&MessageId::new("m-3").unwrap()).is_none());
}

#[test]
fn claim_envelope_and_fact_binding_failure_atomicity() {
    let (ws, admin, bob, carol) = test_context();
    let mut proj = MailboxProjection::new(ws);

    let line1 = sample_msg_line(1, "m-1", ws, admin, vec![bob], Kind::Msg, None, "A");
    proj.apply_line(&line1).unwrap();

    let claim_bad_ver = LedgerLine {
        seq: 2,
        boot_id: "boot-1".into(),
        id: "m-1".into(),
        ts: 1_700_000_001_000,
        kind: Kind::State,
        from: bob.to_string(),
        to: vec![],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: vec![],
        data: Some(
            serde_json::to_value(MailboxFact::MessageClaimed {
                record_version: 99,
                message_id: MessageId::new("m-1").unwrap(),
                recipient: bob,
                claimant: bob,
            })
            .unwrap(),
        ),
    };
    let err = proj.apply_line(&claim_bad_ver).unwrap_err();
    assert_eq!(
        err,
        MailboxError::InvalidRecordVersion {
            expected: CANONICAL_RECORD_VERSION,
            found: 99
        }
    );
    assert_eq!(proj.last_sequence(), Some(1));
    assert_eq!(proj.get_pending(bob).len(), 1);

    let claim_nonempty_to = LedgerLine {
        seq: 2,
        boot_id: "boot-1".into(),
        id: "m-1".into(),
        ts: 1_700_000_001_000,
        kind: Kind::State,
        from: bob.to_string(),
        to: vec!["extra_recipient".into()],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: vec![],
        data: Some(
            serde_json::to_value(MailboxFact::MessageClaimed {
                record_version: CANONICAL_RECORD_VERSION,
                message_id: MessageId::new("m-1").unwrap(),
                recipient: bob,
                claimant: bob,
            })
            .unwrap(),
        ),
    };
    let err = proj.apply_line(&claim_nonempty_to).unwrap_err();
    assert!(matches!(
        err,
        MailboxError::PresentationMismatch { field: "to", .. }
    ));
    assert_eq!(proj.last_sequence(), Some(1));

    let claim_nonempty_subject = LedgerLine {
        seq: 2,
        boot_id: "boot-1".into(),
        id: "m-1".into(),
        ts: 1_700_000_001_000,
        kind: Kind::State,
        from: bob.to_string(),
        to: vec![],
        subject: Some("Unexpected Subject".into()),
        body: None,
        reply_to: None,
        deliveries: vec![],
        data: Some(
            serde_json::to_value(MailboxFact::MessageClaimed {
                record_version: CANONICAL_RECORD_VERSION,
                message_id: MessageId::new("m-1").unwrap(),
                recipient: bob,
                claimant: bob,
            })
            .unwrap(),
        ),
    };
    let err = proj.apply_line(&claim_nonempty_subject).unwrap_err();
    assert!(matches!(err, MailboxError::UncanonicalRow(_)));
    assert_eq!(proj.last_sequence(), Some(1));

    let claim_env_mismatch = LedgerLine {
        seq: 2,
        boot_id: "boot-1".into(),
        id: "m-diff".into(),
        ts: 1_700_000_001_000,
        kind: Kind::State,
        from: bob.to_string(),
        to: vec![],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: vec![],
        data: Some(
            serde_json::to_value(MailboxFact::MessageClaimed {
                record_version: CANONICAL_RECORD_VERSION,
                message_id: MessageId::new("m-1").unwrap(),
                recipient: bob,
                claimant: bob,
            })
            .unwrap(),
        ),
    };
    let err = proj.apply_line(&claim_env_mismatch).unwrap_err();
    assert!(matches!(err, MailboxError::EnvelopeMismatch { .. }));
    assert_eq!(proj.last_sequence(), Some(1));

    let claim_foreign = LedgerLine {
        seq: 2,
        boot_id: "boot-1".into(),
        id: "m-1".into(),
        ts: 1_700_000_001_000,
        kind: Kind::State,
        from: carol.to_string(),
        to: vec![],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: vec![],
        data: Some(
            serde_json::to_value(MailboxFact::MessageClaimed {
                record_version: CANONICAL_RECORD_VERSION,
                message_id: MessageId::new("m-1").unwrap(),
                recipient: bob,
                claimant: carol,
            })
            .unwrap(),
        ),
    };
    let err = proj.apply_line(&claim_foreign).unwrap_err();
    assert!(matches!(err, MailboxError::ClaimantMismatch { .. }));
    assert_eq!(proj.last_sequence(), Some(1));

    let claim_valid = LedgerLine {
        seq: 2,
        boot_id: "boot-1".into(),
        id: "m-1".into(),
        ts: 1_700_000_001_000,
        kind: Kind::State,
        from: bob.to_string(),
        to: vec![],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: vec![],
        data: Some(
            serde_json::to_value(MailboxFact::MessageClaimed {
                record_version: CANONICAL_RECORD_VERSION,
                message_id: MessageId::new("m-1").unwrap(),
                recipient: bob,
                claimant: bob,
            })
            .unwrap(),
        ),
    };
    proj.apply_line(&claim_valid).unwrap();
    assert_eq!(proj.last_sequence(), Some(2));
    assert_eq!(proj.get_pending(bob).len(), 0);
    assert_eq!(proj.get_mailbox(bob)[0].state.claimant(), Some(bob));
}

#[test]
fn store_reopens_with_idempotent_accept_and_payload_bearing_claim() {
    let scratch = StoreScratch::new("reopen");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let original = MessageId::new("m-original").unwrap();
    let request = draft(admin, vec![bob], "Review code", Some("request-1"));

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        assert_eq!(
            store
                .accept_at(original.clone(), request.clone(), 1_700_000_000_000)
                .unwrap(),
            AcceptResult {
                message_id: original.clone(),
                inserted: true,
                seq: 1,
                recipients: vec!["recipient-0".into()],
                recipient_keys: vec![bob],
            }
        );
        assert_eq!(store.projection().last_sequence(), Some(1));
        let listed = store.projection().list_mailbox(bob).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].sender_label, "sender-label");
        assert_eq!(listed[0].recipient_label, "recipient-0");
        let listed_json = serde_json::to_value(&listed[0]).unwrap();
        assert!(listed_json.get("body").is_none());

        let retry_id = MessageId::new("m-retry-candidate").unwrap();
        let mut retry = request;
        retry.presentation.sender_label = "renamed-sender".into();
        retry.presentation.recipient_labels[0].label = "renamed-recipient".into();
        assert_eq!(
            store.accept_at(retry_id, retry, 1_700_000_000_100).unwrap(),
            AcceptResult {
                message_id: original.clone(),
                inserted: false,
                seq: 1,
                recipients: vec!["recipient-0".into()],
                recipient_keys: vec![bob],
            }
        );
        assert_eq!(store.projection().last_sequence(), Some(1));
        let listed = store.projection().list_mailbox(bob).unwrap();
        assert_eq!(listed[0].sender_label, "sender-label");
        assert_eq!(listed[0].recipient_label, "recipient-0");

        let first = store
            .claim_at(bob, original.clone(), 1_700_000_001_000)
            .unwrap();
        let ClaimOutcome::Claimed { entry, message, .. } = first else {
            panic!("first claim must append a claim fact");
        };
        assert_eq!(entry.message_id, original);
        assert_eq!(message.recipient_label.as_deref(), Some("recipient-0"));
        assert_eq!(message.sender_label, "sender-label");
        assert_eq!(message.subject.as_deref(), Some("Task"));
        assert_eq!(message.body.as_deref(), Some("Review code"));

        let replay = store
            .claim_at(bob, original.clone(), 1_700_000_002_000)
            .unwrap();
        let ClaimOutcome::AlreadyClaimed { entry, message, .. } = replay else {
            panic!("re-claim must return the existing claim");
        };
        assert_eq!(entry.message_id, original);
        assert_eq!(message.recipient_label.as_deref(), Some("recipient-0"));
        assert_eq!(message.subject.as_deref(), Some("Task"));
        assert_eq!(message.body.as_deref(), Some("Review code"));
        assert_eq!(store.projection().last_sequence(), Some(2));
    }

    let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    assert!(reopened.projection().get_pending(bob).is_empty());
    assert_eq!(reopened.projection().get_mailbox(bob).len(), 1);
    assert_eq!(reopened.projection().last_sequence(), Some(2));
    let listed = reopened.projection().list_mailbox(bob).unwrap();
    assert!(listed.is_empty());
    let ClaimOutcome::AlreadyClaimed { message, .. } = reopened.claim(bob, original).unwrap()
    else {
        panic!("claim state must survive restart");
    };
    assert_eq!(message.recipient_label.as_deref(), Some("recipient-0"));
    assert_eq!(message.body.as_deref(), Some("Review code"));
    assert_eq!(reopened.projection().last_sequence(), Some(2));
}

#[test]
fn broadcast_claim_names_only_the_authenticated_recipient() {
    let scratch = StoreScratch::new("broadcast-claim-recipient-label");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-broadcast-claim").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob, carol], "Review together", None),
            1,
        )
        .unwrap();

    let ClaimOutcome::Claimed { message, .. } = store.claim_at(carol, message_id, 2).unwrap()
    else {
        panic!("the authenticated broadcast recipient must claim its own entry");
    };
    assert_eq!(message.recipient_label.as_deref(), Some("recipient-1"));
    assert_eq!(message.sender_label, "sender-label");
}

#[test]
fn direct_delivery_retires_pending_without_forging_a_claim_and_replays() {
    let scratch = StoreScratch::new("direct-delivery-replay");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-direct").unwrap();
    let attempt_id = attempt(77);

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Direct", None),
                1,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Queued,
                None,
                None,
                2,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Gating,
                None,
                None,
                3,
            )
            .unwrap();
        let binding = notification_binding(bob);
        for (offset, state) in [
            NotificationState::Writing,
            NotificationState::Staged,
            NotificationState::Submitting,
            NotificationState::Submitted,
            NotificationState::Notified,
        ]
        .into_iter()
        .enumerate()
        {
            if state == NotificationState::Writing {
                store
                    .append_notification_transition_with_transport_at(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        state,
                        Some(binding.clone()),
                        Some(NotificationTransport::DirectPayload),
                        None,
                        None,
                        4 + offset as u64,
                    )
                    .unwrap();
            } else {
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        state,
                        None,
                        None,
                        4 + offset as u64,
                    )
                    .unwrap();
            }
        }
        let entry = store
            .mark_delivered_direct_at(message_id.clone(), bob, attempt_id, 9)
            .unwrap();
        assert!(matches!(
            entry.state,
            MailboxEntryState::DeliveredDirect {
                attempt_id: found,
                delivered_at: 9
            } if found == attempt_id
        ));
        assert_eq!(entry.state.claimant(), None);
        assert!(store.projection().get_pending(bob).is_empty());
        assert!(matches!(
            store.claim_at(bob, message_id.clone(), 10),
            Err(MessageStoreError::Mailbox(error))
                if matches!(*error, MailboxError::MessageNotPending(ref id) if id == &message_id)
        ));
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let entry = reopened.projection().get_entry(bob, &message_id).unwrap();
    assert!(matches!(
        entry.state,
        MailboxEntryState::DeliveredDirect { attempt_id: found, .. }
            if found == attempt_id
    ));
    let raw = fs::read_to_string(root.path().join(journal)).unwrap();
    let direct = raw
        .lines()
        .find(|line| line.contains("message_delivered_direct"))
        .expect("direct disposition fact");
    assert!(!direct.contains("claimant"));
}

/// Replay only: an older daemon could leave a `notified` direct payload
/// attempt with its entry still pending. The record and entry replay
/// intact, nothing repairs them with a retired fact, and the exact
/// recipient claim is what advances the FIFO.
#[test]
fn a_replayed_notified_direct_attempt_stays_pending_until_the_recipient_claims() {
    let scratch = StoreScratch::new("direct-delivery-restart");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let identity = MailboxIdentity {
        key: bob,
        label: "reviewer".into(),
    };
    let first_id;
    let second_id;

    {
        let directory = MailboxDirectory::new(workspace, [identity.clone()]).unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let service = MailboxService::new(directory, store);
        let first = service
            .send(service.admin(), mailbox_send("reviewer", "First", "Body"))
            .unwrap();
        let second = service
            .send(service.admin(), mailbox_send("reviewer", "Second", "Body"))
            .unwrap();
        first_id = first.message_id.clone();
        second_id = second.message_id.clone();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            first.message_id,
            bob,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_writing(
                notification_binding(bob).pane_root.unwrap(),
                notification_binding(bob).leader.unwrap(),
                notification_binding(bob).agent,
                "codex",
                NotificationTransport::DirectPayload,
                None,
            )
            .unwrap();
        context.record_submitted().unwrap();
        context
            .record_notified(Some(cyclops_proto::VerifiedBy::Hook))
            .unwrap();
    }

    let directory = MailboxDirectory::new(workspace, [identity]).unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let service = MailboxService::new(directory, store);
    {
        let store = service.store().unwrap();
        let record = store.projection().notification(bob, &first_id).unwrap();
        assert_eq!(record.state, NotificationState::Notified);
        assert_eq!(record.transport, NotificationTransport::DirectPayload);
        assert!(store
            .projection()
            .get_entry(bob, &first_id)
            .unwrap()
            .state
            .is_pending());
    }
    assert!(
        service.prepare_oldest_notification(bob).unwrap().is_none(),
        "a notified head waits for its claim; no retired fact repairs it"
    );
    let raw = fs::read_to_string(root.path().join(journal)).unwrap();
    assert_eq!(raw.matches("message_delivered_direct").count(), 0);

    service.claim(bob, first_id.clone()).unwrap();
    let next = service.prepare_oldest_notification(bob).unwrap().unwrap();
    assert_eq!(next.message_id, second_id);
}

#[test]
fn restart_never_downgrades_an_ambiguous_doorbell_to_direct_delivery() {
    let scratch = StoreScratch::new("doorbell-attention-restart");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, _, bob, _) = test_context();
    let identity = MailboxIdentity {
        key: bob,
        label: "reviewer".into(),
    };
    let message_id;

    {
        let directory = MailboxDirectory::new(workspace, [identity.clone()]).unwrap();
        let store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let service = MailboxService::new(directory, store);
        let sent = service
            .send(
                service.admin(),
                mailbox_send("reviewer", "Doorbell", "Body"),
            )
            .unwrap();
        message_id = sent.message_id.clone();
        let queued = service.prepare_oldest_notification(bob).unwrap().unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            sent.message_id,
            bob,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_writing(
                notification_binding(bob).pane_root.unwrap(),
                notification_binding(bob).leader.unwrap(),
                notification_binding(bob).agent,
                "codex",
                NotificationTransport::Doorbell,
                Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            )
            .unwrap();
        context
            .record_attention(NotificationAttentionCause::VerifyFailed)
            .unwrap();
    }

    let directory = MailboxDirectory::new(workspace, [identity]).unwrap();
    let store = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let service = MailboxService::new(directory, store);
    assert!(service.prepare_oldest_notification(bob).unwrap().is_none());
    let store = service.store().unwrap();
    assert!(store
        .projection()
        .get_entry(bob, &message_id)
        .unwrap()
        .state
        .is_pending());
    assert_eq!(
        store
            .projection()
            .notification(bob, &message_id)
            .unwrap()
            .state,
        NotificationState::AttentionRequired
    );
    assert_eq!(
        store
            .projection()
            .notification(bob, &message_id)
            .unwrap()
            .doorbell_format,
        Some(DOORBELL_FORMAT_COMPACT_CLAIM)
    );
    drop(store);
    let raw = fs::read_to_string(root.path().join(journal)).unwrap();
    assert!(!raw.contains("message_delivered_direct"));
    assert!(!raw.contains("message_claimed"));
}

#[test]
fn canonical_rows_require_presentation_snapshots() {
    let (workspace, admin, bob, _) = test_context();
    let mut line = sample_msg_line(
        1,
        "m-missing-presentation",
        workspace,
        admin,
        vec![bob],
        Kind::Msg,
        None,
        "Body",
    );
    line.data
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
        .unwrap()
        .remove("presentation");

    let mut projection = MailboxProjection::new(workspace);
    assert!(matches!(
        projection.apply_line(&line),
        Err(MailboxError::MissingMetadata(_))
    ));
    assert_eq!(projection.last_sequence(), None);
}

#[test]
fn replay_refuses_presentation_labels_bound_to_the_wrong_recipient() {
    let (workspace, admin, bob, carol) = test_context();
    let mut projection = MailboxProjection::new(workspace);
    let mut line = sample_msg_line(
        1,
        "m-wrong-label-key",
        workspace,
        admin,
        vec![bob],
        Kind::Msg,
        None,
        "Body",
    );
    let mut metadata = extract_message_metadata(&line).unwrap();
    metadata.presentation = MessagePresentation {
        sender_label: "operator".into(),
        recipient_labels: vec![RecipientPresentation {
            recipient: carol,
            label: "reviewer".into(),
        }],
    };
    line.from = "operator".into();
    line.to = vec!["reviewer".into()];
    line.data = Some(serde_json::to_value(metadata).unwrap());

    assert!(matches!(
        projection.apply_line(&line),
        Err(MailboxError::InvalidPresentation(_))
    ));
    assert_eq!(projection.last_sequence(), None);
}

#[test]
fn replies_derive_the_only_recipient_subject_and_thread_root() {
    let scratch = StoreScratch::new("replies");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let root_id = MessageId::new("m-root").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(root_id.clone(), draft(admin, vec![bob], "Root", None), 1)
        .unwrap();

    let missing = MessageId::new("m-missing").unwrap();
    let error = store
        .reply_at(
            MessageId::new("m-bad-missing").unwrap(),
            reply_draft(bob, missing.clone(), "Missing"),
            2,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::MessageNotFound(id) if id == &missing)
    ));

    let error = store
        .reply_at(
            MessageId::new("m-bad-hidden").unwrap(),
            reply_draft(carol, root_id.clone(), "Hidden"),
            3,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::ReplyNotVisible { .. })
    ));
    assert_eq!(store.projection().last_sequence(), Some(1));

    let first_reply = MessageId::new("m-reply-one").unwrap();
    store
        .reply_at(
            first_reply.clone(),
            reply_draft(bob, root_id.clone(), "First reply"),
            4,
        )
        .unwrap();
    let second_reply = MessageId::new("m-reply-two").unwrap();
    store
        .reply_at(
            second_reply.clone(),
            reply_draft(admin, first_reply.clone(), "Second reply"),
            5,
        )
        .unwrap();

    let first = store.projection().get_message(&first_reply).unwrap();
    let first_metadata = extract_message_metadata(first).unwrap();
    assert_eq!(first_metadata.recipients, [admin]);
    assert_eq!(first.subject.as_deref(), Some("Re: Task"));
    assert_eq!(first_metadata.thread_root, root_id);

    let second = store.projection().get_message(&second_reply).unwrap();
    let second_metadata = extract_message_metadata(second).unwrap();
    assert_eq!(second_metadata.recipients, [bob]);
    assert_eq!(second.subject.as_deref(), Some("Re: Task"));
    assert_eq!(second_metadata.thread_root, root_id);
    assert_eq!(store.projection().last_sequence(), Some(3));
}

#[test]
fn supersession_is_an_atomic_auditable_mailbox_transition() {
    let scratch = StoreScratch::new("supersession");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let old_id = MessageId::new("m-old").unwrap();
    let replacement_id = MessageId::new("m-replacement").unwrap();

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(old_id.clone(), draft(admin, vec![bob], "Old", None), 1)
            .unwrap();
        let mut replacement = draft(admin, vec![bob], "New", None);
        replacement.supersedes = Some(old_id.clone());
        store
            .accept_at(replacement_id.clone(), replacement, 2)
            .unwrap();

        let old = store.projection().get_entry(bob, &old_id).unwrap();
        assert_eq!(
            old.state,
            MailboxEntryState::Superseded {
                by: replacement_id.clone(),
                superseded_at: 2,
            }
        );
        let pending = store.projection().get_pending(bob);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message_id, replacement_id);
        let replacement = store.projection().get_message(&replacement_id).unwrap();
        assert_eq!(
            extract_message_metadata(replacement).unwrap().supersedes,
            Some(old_id.clone())
        );
        assert!(matches!(
            store.claim_at(bob, old_id.clone(), 3),
            Err(MessageStoreError::Mailbox(error))
                if matches!(error.as_ref(), MailboxError::MessageNotPending(id) if id == &old_id)
        ));
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    assert!(matches!(
        &reopened.projection().get_entry(bob, &old_id).unwrap().state,
        MailboxEntryState::Superseded { by, .. } if by == &replacement_id
    ));
    assert_eq!(reopened.projection().get_pending(bob).len(), 1);
}

#[test]
fn a_superseded_entry_is_never_a_requeue_target() {
    let scratch = StoreScratch::new("superseded-requeue");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let old_id = MessageId::new("m-superseded").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(old_id.clone(), draft(admin, vec![bob], "Old", None), 1)
        .unwrap();
    let mut replacement = draft(admin, vec![bob], "New", None);
    replacement.supersedes = Some(old_id.clone());
    store
        .accept_at(MessageId::new("m-replacement").unwrap(), replacement, 2)
        .unwrap();
    let before = store.projection().last_sequence();
    let directory = MailboxDirectory::new(
        workspace,
        [MailboxIdentity {
            key: bob,
            label: "bob".into(),
        }],
    )
    .unwrap();
    let service = MailboxService::new(directory, store);

    assert!(service.requeue_message(old_id).unwrap().is_empty());
    assert_eq!(
        service.store().unwrap().projection().last_sequence(),
        before
    );
}

#[test]
fn supersession_withdraws_queued_and_gating_notifications_on_replay() {
    let scratch = StoreScratch::new("supersession-notification");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let queued = MessageId::new("m-queued-old").unwrap();
    let queued_replacement = MessageId::new("m-queued-new").unwrap();
    let gating = MessageId::new("m-gating-old").unwrap();
    let gating_replacement = MessageId::new("m-gating-new").unwrap();

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(queued.clone(), draft(admin, vec![bob], "Queued", None), 1)
            .unwrap();
        store
            .append_notification_transition_at(
                queued.clone(),
                bob,
                attempt(1),
                NotificationState::Queued,
                None,
                None,
                2,
            )
            .unwrap();
        let mut replacement = draft(admin, vec![bob], "Queued replacement", None);
        replacement.supersedes = Some(queued.clone());
        store.accept_at(queued_replacement, replacement, 3).unwrap();

        store
            .accept_at(gating.clone(), draft(admin, vec![bob], "Gating", None), 4)
            .unwrap();
        store
            .append_notification_transition_at(
                gating.clone(),
                bob,
                attempt(2),
                NotificationState::Queued,
                None,
                None,
                5,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                gating.clone(),
                bob,
                attempt(2),
                NotificationState::Gating,
                None,
                None,
                6,
            )
            .unwrap();
        let mut replacement = draft(admin, vec![bob], "Gating replacement", None);
        replacement.supersedes = Some(gating.clone());
        store.accept_at(gating_replacement, replacement, 7).unwrap();

        for (message_id, attempt_id, updated_seq) in
            [(&queued, attempt(1), 3), (&gating, attempt(2), 7)]
        {
            let record = store.projection().notification(bob, message_id).unwrap();
            assert_eq!(record.attempt_id, attempt_id);
            assert_eq!(record.state, NotificationState::Superseded);
            assert_eq!(record.updated_seq, updated_seq);
            assert!(record.binding.is_none());
            assert!(record.cause.is_none());
        }
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    assert_eq!(
        reopened
            .projection()
            .notification(bob, &queued)
            .unwrap()
            .state,
        NotificationState::Superseded
    );
    assert_eq!(
        reopened
            .projection()
            .notification(bob, &gating)
            .unwrap()
            .state,
        NotificationState::Superseded
    );
}

#[test]
fn quota_attempts_preserve_operator_notification_state_across_claim_and_replay() {
    let scratch = StoreScratch::new("quota-withdrawal");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let held = MessageId::new("m-quota-held-old").unwrap();
    let reset = MessageId::new("m-quota-reset-old").unwrap();

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(held.clone(), draft(admin, vec![bob], "Held", None), 1)
            .unwrap();
        quota_hold(&mut store, &held, bob, attempt(1), 2);
        let mut replacement = draft(admin, vec![bob], "Replacement", None);
        replacement.supersedes = Some(held.clone());
        store
            .accept_at(MessageId::new("m-quota-held-new").unwrap(), replacement, 5)
            .unwrap();

        store
            .accept_at(reset.clone(), draft(admin, vec![carol], "Reset", None), 6)
            .unwrap();
        quota_hold(&mut store, &reset, carol, attempt(2), 7);
        store
            .advance_notification(
                reset.clone(),
                carol,
                attempt(2),
                NotificationState::QuotaResetObserved,
                None,
                None,
            )
            .unwrap();
        let ClaimOutcome::Claimed {
            withdrawn_attempt, ..
        } = store.claim_at(carol, reset.clone(), 11).unwrap()
        else {
            panic!("quota-reset message was not claimed");
        };
        assert_eq!(withdrawn_attempt, None);

        assert_eq!(
            store.projection().notification(bob, &held).unwrap().state,
            NotificationState::Superseded
        );
        assert_eq!(
            store
                .projection()
                .notification(carol, &reset)
                .unwrap()
                .state,
            NotificationState::QuotaResetObserved
        );
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    assert_eq!(
        reopened
            .projection()
            .notification(bob, &held)
            .unwrap()
            .state,
        NotificationState::Superseded
    );
    assert_eq!(
        reopened
            .projection()
            .notification(carol, &reset)
            .unwrap()
            .state,
        NotificationState::QuotaResetObserved
    );
}

#[test]
fn supersession_refuses_after_the_notification_write_boundary() {
    let scratch = StoreScratch::new("supersession-after-write");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let old_id = MessageId::new("m-writing-old").unwrap();
    let replacement_id = MessageId::new("m-writing-new").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(old_id.clone(), draft(admin, vec![bob], "Writing", None), 1)
        .unwrap();
    store
        .append_notification_transition_at(
            old_id.clone(),
            bob,
            attempt(1),
            NotificationState::Queued,
            None,
            None,
            2,
        )
        .unwrap();
    store
        .append_notification_transition_at(
            old_id.clone(),
            bob,
            attempt(1),
            NotificationState::Gating,
            None,
            None,
            3,
        )
        .unwrap();
    store
        .append_notification_transition_at(
            old_id.clone(),
            bob,
            attempt(1),
            NotificationState::Writing,
            Some(notification_binding(bob)),
            None,
            4,
        )
        .unwrap();

    let mut replacement = draft(admin, vec![bob], "Too late", None);
    replacement.supersedes = Some(old_id.clone());
    let error = store
        .accept_at(replacement_id.clone(), replacement, 5)
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::SupersessionNotificationStarted(id) if id == &old_id)
    ));
    assert!(store.projection().get_message(&replacement_id).is_none());
    assert_eq!(store.projection().last_sequence(), Some(4));
    assert_eq!(
        store.projection().notification(bob, &old_id).unwrap().state,
        NotificationState::Writing
    );
}

#[test]
fn replay_refuses_reply_routing_or_subject_not_derived_from_parent() {
    let (workspace, admin, bob, carol) = test_context();
    let root = sample_msg_line(
        1,
        "m-root",
        workspace,
        admin,
        vec![bob],
        Kind::Msg,
        None,
        "Root",
    );
    let mut projection = MailboxProjection::new(workspace);
    projection.apply_line(&root).unwrap();

    let root_id = MessageId::new("m-root").unwrap();
    let reply_id = MessageId::new("m-reply").unwrap();
    let mut wrong_target = sample_msg_line(
        2,
        "m-reply",
        workspace,
        bob,
        vec![carol],
        Kind::Msg,
        None,
        "Reply",
    );
    wrong_target.reply_to = Some(root_id.to_string());
    wrong_target.subject = Some("Re: Task".into());
    let mut metadata = extract_message_metadata(&wrong_target).unwrap();
    metadata.thread_root = root_id.clone();
    metadata.request_digest = RequestDigest::compute(
        Kind::Msg,
        bob,
        &[carol],
        RequestContent {
            subject: wrong_target.subject.as_deref(),
            summary: metadata.summary.as_deref(),
            body: wrong_target.body.as_deref(),
        },
        Some(&root_id),
        None,
    )
    .unwrap();
    wrong_target.data = Some(serde_json::to_value(metadata).unwrap());
    assert!(matches!(
        projection.apply_line(&wrong_target),
        Err(MailboxError::ReplyRecipientMismatch { .. })
    ));

    let mut wrong_subject = wrong_target;
    let presentation = MessagePresentation {
        sender_label: bob.to_string(),
        recipient_labels: vec![RecipientPresentation {
            recipient: admin,
            label: admin.to_string(),
        }],
    };
    wrong_subject.to = vec![admin.to_string()];
    wrong_subject.subject = Some("Custom".into());
    let metadata = MessageMetadata {
        record_version: CANONICAL_RECORD_VERSION,
        workspace_id: workspace,
        sender: bob,
        recipients: vec![admin],
        presentation,
        summary: None,
        thread_root: root_id.clone(),
        client_key: None,
        request_digest: RequestDigest::compute(
            Kind::Msg,
            bob,
            &[admin],
            RequestContent {
                subject: wrong_subject.subject.as_deref(),
                summary: None,
                body: wrong_subject.body.as_deref(),
            },
            Some(&root_id),
            None,
        )
        .unwrap(),
        supersedes: None,
        raw: false,
    };
    wrong_subject.data = Some(serde_json::to_value(metadata).unwrap());
    assert!(matches!(
        projection.apply_line(&wrong_subject),
        Err(MailboxError::ReplySubjectMismatch { message_id }) if message_id == reply_id
    ));
    assert_eq!(projection.last_sequence(), Some(1));
}

#[test]
fn store_recovers_a_torn_tail_but_refuses_complete_corruption() {
    let scratch = StoreScratch::new("recovery");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                MessageId::new("m-one").unwrap(),
                draft(admin, vec![bob], "One", None),
                1,
            )
            .unwrap();
    }
    {
        let mut file = root.open_append(journal).unwrap();
        file.write_all(br#"{"seq":2,"boot_id":"boot-1","id":"m-torn""#)
            .unwrap();
        file.sync_data().unwrap();
    }
    {
        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        reopened
            .accept_at(
                MessageId::new("m-two").unwrap(),
                draft(admin, vec![bob], "Two", None),
                2,
            )
            .unwrap();
        assert_eq!(reopened.projection().last_sequence(), Some(2));
        assert_eq!(reopened.projection().get_pending(bob).len(), 2);
    }
    {
        let mut file = root.open_append(journal).unwrap();
        file.write_all(b"complete corruption\n").unwrap();
        file.sync_data().unwrap();
    }

    assert!(matches!(
        MessageStore::open(&root, journal, workspace, "boot-3"),
        Err(MessageStoreError::Ledger(LedgerError::CorruptLine {
            line: 3,
            ..
        }))
    ));
}

/// Drive one attempt to the alarm state and return its identifier.
///
/// Most operator tests start here. Clear acts only on alarms; requeue
/// also accepts a quota hold after reset was positively observed.
fn alarm(
    store: &mut MessageStore,
    message_id: &MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    base_ts: u64,
) {
    alarm_because(
        store,
        message_id,
        recipient,
        attempt_id,
        base_ts,
        NotificationAttentionCause::SubmitFailed,
    )
}

/// The same, raised for one named cause.
///
/// How far the attempt gets is decided by the cause: a verify failure
/// happens at the write boundary, a submit failure after the composer
/// took the text. The closed vocabulary is asked rather than assumed.
fn alarm_because(
    store: &mut MessageStore,
    message_id: &MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    base_ts: u64,
    cause: NotificationAttentionCause,
) {
    // A requeued attempt is already queued; queueing it again is an
    // illegal transition, so the first step is conditional.
    let already_queued = store
        .projection()
        .notification(recipient, message_id)
        .is_some_and(|record| {
            record.attempt_id == attempt_id && record.state == NotificationState::Queued
        });
    let mut steps = vec![
        (NotificationState::Queued, None),
        (NotificationState::Gating, None),
        (
            NotificationState::Writing,
            Some(notification_binding(recipient)),
        ),
    ];
    if !cause.valid_after(NotificationState::Writing) {
        steps.push((NotificationState::Staged, None));
    }
    for (offset, (state, binding)) in steps.into_iter().enumerate() {
        if already_queued && state == NotificationState::Queued {
            continue;
        }
        store
            .append_notification_transition_at(
                message_id.clone(),
                recipient,
                attempt_id,
                state,
                binding,
                None,
                base_ts + offset as u64,
            )
            .unwrap();
    }
    store
        .append_notification_transition_at(
            message_id.clone(),
            recipient,
            attempt_id,
            NotificationState::AttentionRequired,
            None,
            Some(cause),
            base_ts + 4,
        )
        .unwrap();
}

fn append_resolution_at(
    store: &mut MessageStore,
    message_id: &MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    proof_version: u32,
    resolution: NotificationResolution,
    ts: u64,
) -> Result<NotificationRecord, MessageStoreError> {
    store.append_notification_fact_at(
        message_id.clone(),
        recipient,
        NotificationFact::NotificationResolved {
            record_version: CANONICAL_RECORD_VERSION,
            proof_version,
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            resolution,
        },
        ts,
    )
}

fn quota_hold(
    store: &mut MessageStore,
    message_id: &MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    base_ts: u64,
) {
    for (offset, state) in [
        NotificationState::Queued,
        NotificationState::Gating,
        NotificationState::QuotaHeld,
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append_notification_transition_at(
                message_id.clone(),
                recipient,
                attempt_id,
                state,
                None,
                None,
                base_ts + offset as u64,
            )
            .unwrap();
    }
}

fn notify_with_binding(
    store: &mut MessageStore,
    message_id: &MessageId,
    recipient: RecipientKey,
    attempt_id: NotificationAttemptId,
    base_ts: u64,
) {
    for (offset, state) in [
        NotificationState::Queued,
        NotificationState::Gating,
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
        NotificationState::Notified,
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append_notification_transition_at(
                message_id.clone(),
                recipient,
                attempt_id,
                state,
                (state == NotificationState::Writing).then(|| notification_binding(recipient)),
                None,
                base_ts + offset as u64,
            )
            .unwrap();
    }
}

fn operator_store(tag: &str) -> (StoreScratch, MessageStore, MessageId, RecipientKey) {
    let scratch = StoreScratch::new(tag);
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    let message_id = MessageId::new("m-operator").unwrap();
    store
        .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
        .unwrap();
    (scratch, store, message_id, bob)
}

fn broadcast_operator_store(
    tag: &str,
) -> (
    StoreScratch,
    MessageStore,
    MessageId,
    RecipientKey,
    RecipientKey,
) {
    let scratch = StoreScratch::new(tag);
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    let message_id = MessageId::new("m-operator-broadcast").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob, carol], "Op", None),
            1,
        )
        .unwrap();
    alarm(&mut store, &message_id, bob, attempt(1), 2);
    alarm(&mut store, &message_id, carol, attempt(2), 10);
    (scratch, store, message_id, bob, carol)
}

fn operator_directory(bob: RecipientKey, carol: RecipientKey) -> MailboxDirectory {
    MailboxDirectory::new(
        test_context().0,
        [
            MailboxIdentity {
                key: bob,
                label: "bob".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "carol".into(),
            },
        ],
    )
    .unwrap()
}

fn current_attempts(
    store: &MessageStore,
    message_id: &MessageId,
    recipients: [RecipientKey; 2],
) -> HashMap<RecipientKey, NotificationAttemptId> {
    recipients
        .into_iter()
        .map(|recipient| {
            (
                recipient,
                store
                    .projection()
                    .notification(recipient, message_id)
                    .unwrap()
                    .attempt_id,
            )
        })
        .collect()
}

fn batch_requeue_line(
    seq: u64,
    message_id: &MessageId,
    requeues: Vec<NotificationRequeue>,
) -> LedgerLine {
    LedgerLine {
        seq,
        boot_id: "boot-1".into(),
        id: message_id.to_string(),
        ts: 50,
        kind: Kind::State,
        from: "cyclopsd".into(),
        to: requeues
            .iter()
            .map(|requeue| requeue.recipient.to_string())
            .collect(),
        subject: None,
        body: None,
        reply_to: None,
        deliveries: Vec::new(),
        data: Some(
            serde_json::to_value(NotificationFact::NotificationsRequeued {
                record_version: CANONICAL_RECORD_VERSION,
                message_id: message_id.clone(),
                requeues,
            })
            .unwrap(),
        ),
    }
}

/// A requeue opens a fresh attempt and retires the one it replaces, so
/// the identifier an operator saw can never be acted on again.
#[test]
fn a_requeue_retires_the_attempt_it_replaces() {
    let (scratch, mut store, message_id, bob) = operator_store("requeue-retires");
    alarm(&mut store, &message_id, bob, attempt(1), 2);
    assert_eq!(store.projection().open_alarms().len(), 1);

    let requeued = store
        .requeue_notification_at(message_id.clone(), bob, attempt(1), attempt(2), 10)
        .unwrap();
    assert_eq!(requeued.attempt_id, attempt(2));
    assert_eq!(requeued.state, NotificationState::Queued);

    // The old identifier names nothing, and a queued attempt is not an
    // alarm, so nothing is left for an operator to act on.
    assert!(store.projection().alarm_by_attempt(attempt(1)).is_none());
    assert!(store.projection().open_alarms().is_empty());

    let text = std::fs::read_to_string(store.journal_path()).unwrap();
    let line: LedgerLine = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(line.data.as_ref().unwrap()["type"], "notification_requeued");
    drop(store);

    let reopened = MessageStore::open(
        &scratch.root(),
        Path::new("workspaces/current/messages.ndjson"),
        test_context().0,
        "boot-2",
    )
    .unwrap();
    assert_eq!(
        reopened
            .projection()
            .notification(bob, &message_id)
            .unwrap()
            .attempt_id,
        attempt(2)
    );
}

#[test]
fn broadcast_requeue_remains_one_content_free_atomic_fact() {
    let scratch = StoreScratch::new("broadcast-requeue");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-broadcast-requeue").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob, carol], "Broadcast", None),
            1,
        )
        .unwrap();
    alarm(&mut store, &message_id, bob, attempt(1), 2);
    alarm(&mut store, &message_id, carol, attempt(2), 10);
    let service = MailboxService::new(operator_directory(bob, carol), store);
    let before_lines = service.journal_lines().unwrap().len();

    let records = service.requeue_message(message_id.clone()).unwrap();
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.state == NotificationState::Queued));
    let lines = service.journal_lines().unwrap();
    assert_eq!(lines.len(), before_lines + 1);
    let fact = lines.last().unwrap();
    assert!(fact.subject.is_none());
    assert!(fact.body.is_none());
    assert_eq!(
        fact.data.as_ref().unwrap()["type"],
        "notifications_requeued"
    );
    let attempts: HashMap<_, _> = records
        .iter()
        .map(|record| (record.recipient, record.attempt_id))
        .collect();
    drop(service);

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    for recipient in [bob, carol] {
        let record = reopened
            .projection()
            .notification(recipient, &message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::Queued);
        assert_eq!(record.attempt_id, attempts[&recipient]);
    }
    drop(scratch);
}

#[test]
fn a_broadcast_requeue_is_one_fact_one_event_and_replays_whole() {
    let (scratch, store, message_id, bob, carol) = broadcast_operator_store("requeue-batch-replay");
    let before_seq = store.projection().last_sequence().unwrap();
    let before_lines = std::fs::read_to_string(store.journal_path())
        .unwrap()
        .lines()
        .count();
    let (sender, _) = broadcast::channel(8);
    let mut events = sender.subscribe();
    let service = MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

    let records = service.requeue_message(message_id.clone()).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .map(|record| record.recipient)
            .collect::<Vec<_>>(),
        [bob, carol]
    );
    assert!(records.iter().all(|record| {
        record.state == NotificationState::Queued && record.updated_seq == before_seq + 1
    }));
    next_change(
        &mut events,
        before_seq + 1,
        &[
            MessagesChangedArea::Notifications,
            MessagesChangedArea::Attention,
        ],
    );
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    let attempts: HashMap<_, _> = records
        .iter()
        .map(|record| (record.recipient, record.attempt_id))
        .collect();
    let store = service.store().unwrap();
    let text = std::fs::read_to_string(store.journal_path()).unwrap();
    assert_eq!(text.lines().count(), before_lines + 1);
    let line: LedgerLine = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(line.seq, before_seq + 1);
    assert_eq!(line.to, [bob.to_string(), carol.to_string()]);
    assert_eq!(
        line.data.as_ref().unwrap()["type"],
        "notifications_requeued"
    );
    let NotificationFact::NotificationsRequeued { requeues, .. } =
        serde_json::from_value(line.data.clone().unwrap()).unwrap()
    else {
        panic!("last line is not a batch requeue");
    };
    assert_eq!(
        requeues
            .iter()
            .map(|requeue| requeue.recipient)
            .collect::<Vec<_>>(),
        [bob, carol]
    );
    assert!(line.subject.is_none());
    assert!(line.body.is_none());
    drop(store);

    let repeated = service.requeue_message(message_id.clone()).unwrap();
    assert!(repeated.is_empty());
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(
        service.store().unwrap().projection().last_sequence(),
        Some(before_seq + 1)
    );
    drop(service);

    let reopened = MessageStore::open(
        &scratch.root(),
        Path::new("workspaces/current/messages.ndjson"),
        test_context().0,
        "boot-2",
    )
    .unwrap();
    for recipient in [bob, carol] {
        let record = reopened
            .projection()
            .notification(recipient, &message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::Queued);
        assert_eq!(record.attempt_id, attempts[&recipient]);
        assert_eq!(record.updated_seq, before_seq + 1);
    }
}

#[test]
fn a_claim_after_writing_remains_valid_when_attention_lands_later() {
    let scratch = StoreScratch::new("claim-between-write-and-attention");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-claim-between-write-and-attention").unwrap();
    let attempt_id = attempt(1);
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob], "Legacy", None),
            1,
        )
        .unwrap();
    for (state, ts) in [
        (NotificationState::Queued, 2),
        (NotificationState::Gating, 3),
    ] {
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                state,
                None,
                None,
                ts,
            )
            .unwrap();
    }
    store
        .append_notification_transition_with_transport_at(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Writing,
            Some(legacy_notification_binding(bob)),
            Some(NotificationTransport::Doorbell),
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            None,
            4,
        )
        .unwrap();
    store.claim_at(bob, message_id.clone(), 5).unwrap();
    store
        .append_notification_transition_at(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::VerifyFailed),
            6,
        )
        .unwrap();

    let record = store.projection().notification(bob, &message_id).unwrap();
    assert_eq!(record.state, NotificationState::AttentionRequired);
    assert!(matches!(
        store.projection().get_entry(bob, &message_id),
        Some(entry) if matches!(entry.state, cyclops_proto::MailboxEntryState::Claimed { .. })
    ));
}

#[test]
fn a_failed_batch_append_changes_neither_projection_nor_replay() {
    let (scratch, mut store, message_id, bob, carol) =
        broadcast_operator_store("requeue-batch-append-failure");
    let journal = Path::new("workspaces/current/messages.ndjson");
    let before_seq = store.projection().last_sequence();
    let before_bytes = std::fs::read(store.journal_path()).unwrap();
    let before_attempts = current_attempts(&store, &message_id, [bob, carol]);
    store.inject_next_batch_append_failure();
    let (sender, _) = broadcast::channel(8);
    let mut events = sender.subscribe();
    let service = MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

    assert!(matches!(
        service.requeue_message(message_id.clone()),
        Err(MailboxServiceError::Store(MessageStoreError::Ledger(
            LedgerError::Io { .. }
        )))
    ));
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let store = service.store().unwrap();
    assert_eq!(store.projection().last_sequence(), before_seq);
    assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
    for recipient in [bob, carol] {
        assert_eq!(
            store
                .projection()
                .notification(recipient, &message_id)
                .unwrap()
                .attempt_id,
            before_attempts[&recipient]
        );
    }
    drop(store);
    drop(service);

    let reopened =
        MessageStore::open(&scratch.root(), journal, test_context().0, "boot-2").unwrap();
    assert_eq!(reopened.projection().last_sequence(), before_seq);
    for recipient in [bob, carol] {
        assert_eq!(
            reopened
                .projection()
                .notification(recipient, &message_id)
                .unwrap()
                .attempt_id,
            before_attempts[&recipient]
        );
    }
}

#[test]
fn strict_replay_removes_a_torn_batch_without_moving_any_recipient() {
    let (scratch, store, message_id, bob, carol) =
        broadcast_operator_store("requeue-batch-torn-tail");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let before_seq = store.projection().last_sequence().unwrap();
    let before_bytes = std::fs::read(store.journal_path()).unwrap();
    let before_attempts = current_attempts(&store, &message_id, [bob, carol]);
    drop(store);

    let line = batch_requeue_line(
        before_seq + 1,
        &message_id,
        vec![
            NotificationRequeue {
                prior_attempt_id: before_attempts[&bob],
                attempt_id: attempt(3),
                recipient: bob,
            },
            NotificationRequeue {
                prior_attempt_id: before_attempts[&carol],
                attempt_id: attempt(4),
                recipient: carol,
            },
        ],
    );
    let bytes = serde_json::to_vec(&line).unwrap();
    let mut file = root.open_append(journal).unwrap();
    file.write_all(&bytes[..bytes.len() / 2]).unwrap();
    file.sync_data().unwrap();
    drop(file);

    let reopened = MessageStore::open(&root, journal, test_context().0, "boot-2").unwrap();
    assert_eq!(reopened.projection().last_sequence(), Some(before_seq));
    assert_eq!(
        std::fs::read(reopened.journal_path()).unwrap(),
        before_bytes
    );
    for recipient in [bob, carol] {
        let record = reopened
            .projection()
            .notification(recipient, &message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::AttentionRequired);
        assert_eq!(record.attempt_id, before_attempts[&recipient]);
    }
}

#[test]
fn a_late_invalid_batch_target_refuses_before_any_projection_change() {
    let (_scratch, mut store, message_id, bob, carol) =
        broadcast_operator_store("requeue-batch-late-refusal");
    let before_seq = store.projection().last_sequence().unwrap();
    let line = batch_requeue_line(
        before_seq + 1,
        &message_id,
        vec![
            NotificationRequeue {
                prior_attempt_id: attempt(1),
                attempt_id: attempt(3),
                recipient: bob,
            },
            NotificationRequeue {
                prior_attempt_id: attempt(99),
                attempt_id: attempt(4),
                recipient: carol,
            },
        ],
    );

    assert!(matches!(
        store.projection.apply_line(&line),
        Err(MailboxError::NotificationAttemptMismatch { expected, found })
            if expected == attempt(2) && found == attempt(99)
    ));
    assert_eq!(store.projection().last_sequence(), Some(before_seq));
    for (recipient, attempt_id) in [(bob, attempt(1)), (carol, attempt(2))] {
        let record = store
            .projection()
            .notification(recipient, &message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::AttentionRequired);
        assert_eq!(record.attempt_id, attempt_id);
    }
}

/// Requeue and clear both refuse anything that is not an alarm.
#[test]
fn only_an_alarm_can_be_requeued_or_cleared() {
    let (_scratch, mut store, message_id, bob) = operator_store("only-alarms");
    store
        .append_notification_transition_at(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Queued,
            None,
            None,
            2,
        )
        .unwrap();

    assert!(store
        .requeue_notification_at(message_id.clone(), bob, attempt(1), attempt(2), 3)
        .is_err());
    assert!(store
        .clear_notification_at(message_id.clone(), bob, attempt(1), 4)
        .is_err());
    // Neither refusal wrote anything.
    assert_eq!(store.projection().last_sequence(), Some(2));
}

/// Clearing twice acknowledges once. A repeated command must not grow
/// the journal, or an operator retrying a timed-out call rewrites
/// history for no reason.
#[test]
fn clearing_an_alarm_twice_appends_one_fact() {
    let (_scratch, mut store, message_id, bob) = operator_store("clear-idempotent");
    alarm(&mut store, &message_id, bob, attempt(1), 2);

    store
        .clear_notification_at(message_id.clone(), bob, attempt(1), 10)
        .unwrap();
    let after_first = store.projection().last_sequence();
    assert!(store.projection().alarm_cleared(attempt(1)));
    assert!(store.projection().open_alarms().is_empty());

    store
        .clear_notification_at(message_id.clone(), bob, attempt(1), 11)
        .unwrap();
    assert_eq!(store.projection().last_sequence(), after_first);

    let (_, _, _, carol) = test_context();
    let service = MailboxService::new(operator_directory(bob, carol), store);
    assert!(service.requeue_message(message_id).unwrap().is_empty());
    assert_eq!(
        service.store().unwrap().projection().last_sequence(),
        after_first
    );
}

#[test]
fn clearing_several_alarms_appends_one_replayable_fact() {
    let (scratch, store, message_id, bob, carol) = broadcast_operator_store("clear-batch-atomic");
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, _, _) = test_context();
    let before = store.projection().last_sequence().unwrap();
    let (sender, _) = broadcast::channel(8);
    let mut events = sender.subscribe();
    let service = MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

    let requested = [attempt(2), attempt(1), attempt(1)];
    let summaries = service.clear_alarms(admin, &requested, None).unwrap();
    assert_eq!(
        summaries
            .iter()
            .map(|record| record.attempt_id)
            .collect::<Vec<_>>(),
        requested
    );
    assert_eq!(summaries.len(), requested.len());
    assert_eq!(summaries[0].message_id, message_id);
    assert_eq!(summaries[0].recipient, carol);
    assert_eq!(summaries[1].recipient, bob);
    assert_eq!(
        summaries[0].cause,
        Some(NotificationAttentionCause::SubmitFailed)
    );
    let store = service.store().unwrap();
    assert_eq!(store.projection().last_sequence(), Some(before + 1));
    assert!(store.projection().alarm_cleared(attempt(1)));
    assert!(store.projection().alarm_cleared(attempt(2)));
    drop(store);
    let line = service.journal_lines().unwrap().pop().unwrap();
    assert_eq!(line.data.as_ref().unwrap()["type"], "notifications_cleared");
    let fact: NotificationFact = serde_json::from_value(line.data.unwrap()).unwrap();
    let NotificationFact::NotificationsCleared { attempt_ids, .. } = fact else {
        panic!("last fact was not an atomic clearance");
    };
    assert_eq!(attempt_ids, vec![attempt(1), attempt(2)]);
    next_change(&mut events, before + 1, &[MessagesChangedArea::Attention]);
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    drop(service);

    let reopened = MessageStore::open(&scratch.root(), journal, workspace, "boot-2").unwrap();
    assert!(reopened.projection().alarm_cleared(attempt(1)));
    assert!(reopened.projection().alarm_cleared(attempt(2)));
    assert!(reopened.projection().open_alarms().is_empty());
}

#[test]
fn a_failed_clear_batch_changes_neither_journal_nor_projection() {
    let (scratch, mut store, _, bob, carol) =
        broadcast_operator_store("clear-batch-append-failure");
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, _, _) = test_context();
    let before_seq = store.projection().last_sequence();
    let before_bytes = std::fs::read(store.journal_path()).unwrap();
    store.inject_next_batch_append_failure();
    let (sender, _) = broadcast::channel(8);
    let mut events = sender.subscribe();
    let service = MailboxService::new_with_events(operator_directory(bob, carol), store, sender);

    assert!(matches!(
        service.clear_alarms(admin, &[attempt(1), attempt(2)], None),
        Err(MailboxServiceError::Store(MessageStoreError::Ledger(
            LedgerError::Io { .. }
        )))
    ));
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let store = service.store().unwrap();
    assert_eq!(store.projection().last_sequence(), before_seq);
    assert!(!store.projection().alarm_cleared(attempt(1)));
    assert!(!store.projection().alarm_cleared(attempt(2)));
    assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
    drop(store);
    drop(service);

    let reopened = MessageStore::open(&scratch.root(), journal, workspace, "boot-2").unwrap();
    assert_eq!(reopened.projection().last_sequence(), before_seq);
    assert_eq!(reopened.projection().open_alarms().len(), 2);
}

#[test]
fn an_age_clear_refuses_the_whole_batch_when_one_alarm_is_newer() {
    let (_scratch, store, _, bob, carol) = broadcast_operator_store("clear-batch-cutoff");
    let (_, admin, _, _) = test_context();
    let before_seq = store.projection().last_sequence();
    let before_bytes = std::fs::read(store.journal_path()).unwrap();
    let cutoff = store
        .projection()
        .notification_by_attempt(attempt(1))
        .unwrap()
        .updated_at;
    let service = MailboxService::new(operator_directory(bob, carol), store);

    assert!(matches!(
        service.clear_alarms(admin, &[attempt(1), attempt(2)], Some(cutoff)),
        Err(MailboxServiceError::Store(MessageStoreError::Mailbox(error)))
            if matches!(*error, MailboxError::NotificationNewerThanClearCutoff { attempt_id, .. }
                if attempt_id == attempt(2))
    ));
    let store = service.store().unwrap();
    assert_eq!(store.projection().last_sequence(), before_seq);
    assert_eq!(std::fs::read(store.journal_path()).unwrap(), before_bytes);
    assert!(!store.projection().alarm_cleared(attempt(1)));
    assert!(!store.projection().alarm_cleared(attempt(2)));
}

/// A clearance names one attempt and cannot land on the attempt that
/// replaced it. Otherwise an operator clearing what they previewed
/// silences an alarm raised after they looked.
#[test]
fn a_clearance_never_lands_on_a_newer_attempt() {
    let (_scratch, mut store, message_id, bob) = operator_store("clear-superseded");
    alarm(&mut store, &message_id, bob, attempt(1), 2);
    store
        .requeue_notification_at(message_id.clone(), bob, attempt(1), attempt(2), 10)
        .unwrap();
    alarm(&mut store, &message_id, bob, attempt(2), 11);
    let before = store.projection().last_sequence();

    // The identifier the operator previewed is gone; the alarm now
    // standing is a different attempt and keeps standing.
    assert!(store
        .clear_notification_at(message_id.clone(), bob, attempt(1), 20)
        .is_err());
    assert_eq!(store.projection().last_sequence(), before);
    assert!(!store.projection().alarm_cleared(attempt(2)));
    assert_eq!(store.projection().open_alarms().len(), 1);
}

/// An acknowledgement is durable. A restart that forgot it would show
/// the operator an alarm they have already dealt with.
#[test]
fn a_cleared_alarm_stays_cleared_across_a_restart() {
    let scratch = StoreScratch::new("clear-restart");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-restart-clear").unwrap();
    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
            .unwrap();
        alarm(&mut store, &message_id, bob, attempt(1), 2);
        store
            .clear_notification_at(message_id.clone(), bob, attempt(1), 10)
            .unwrap();
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    assert!(reopened.projection().alarm_cleared(attempt(1)));
    assert!(reopened.projection().open_alarms().is_empty());
    // The attempt itself is untouched: clearing acknowledges, it does
    // not rewrite the outcome that was recorded.
    let record = reopened
        .projection()
        .notification(bob, &message_id)
        .unwrap();
    assert_eq!(record.state, NotificationState::AttentionRequired);
    assert_eq!(record.attempt_id, attempt(1));
}

fn assert_legacy_staged_submit_replays(
    tag: &str,
    transport: NotificationTransport,
    doorbell_format: Option<u32>,
) {
    let scratch = StoreScratch::new(tag);
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new(format!("m-{tag}")).unwrap();
    let submitted = {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
            .unwrap();
        for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
            .into_iter()
            .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    state,
                    None,
                    None,
                    2 + offset as u64,
                )
                .unwrap();
        }
        store
            .append_notification_transition_with_transport_at(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Writing,
                Some(legacy_notification_binding(bob)),
                Some(transport),
                doorbell_format,
                None,
                4,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Staged,
                None,
                None,
                5,
            )
            .unwrap();
        let line = sample_notification_state_line(
            store.projection().last_sequence().unwrap() + 1,
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Submitted,
        );
        assert!(matches!(
            store.projection.apply_line(&line),
            Err(MailboxError::InvalidNotificationTransition {
                from: NotificationState::Staged,
                to: NotificationState::Submitted,
            })
        ));
        line
    };
    let mut file = root.open_append(journal).unwrap();
    serde_json::to_writer(&mut file, &submitted).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_data().unwrap();

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    assert_eq!(
        reopened
            .projection()
            .notification(bob, &message_id)
            .unwrap()
            .state,
        NotificationState::Submitted
    );
}

#[test]
fn shipped_staged_submit_edges_replay_without_weakening_live_appends() {
    assert_legacy_staged_submit_replays(
        "legacy-verbose-submit",
        NotificationTransport::Doorbell,
        None,
    );
    assert_legacy_staged_submit_replays(
        "legacy-doorbell-submit",
        NotificationTransport::Doorbell,
        Some(DOORBELL_FORMAT_COMPACT_CLAIM),
    );
    assert_legacy_staged_submit_replays(
        "legacy-direct-submit",
        NotificationTransport::DirectPayload,
        None,
    );
}

#[test]
fn current_staged_submit_edge_is_rejected_during_replay() {
    let scratch = StoreScratch::new("current-direct-submit-refused");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-current-direct-submit").unwrap();
    let submitted = {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(message_id.clone(), draft(admin, vec![bob], "Op", None), 1)
            .unwrap();
        for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
            .into_iter()
            .enumerate()
        {
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    attempt(1),
                    state,
                    None,
                    None,
                    2 + offset as u64,
                )
                .unwrap();
        }
        store
            .append_notification_transition_with_transport_at(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Writing,
                Some(notification_binding(bob)),
                Some(NotificationTransport::Doorbell),
                Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
                None,
                4,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Staged,
                None,
                None,
                5,
            )
            .unwrap();
        sample_notification_state_line(
            store.projection().last_sequence().unwrap() + 1,
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Submitted,
        )
    };
    let mut file = root.open_append(journal).unwrap();
    serde_json::to_writer(&mut file, &submitted).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_data().unwrap();

    assert!(matches!(
        MessageStore::open(&root, journal, workspace, "boot-2"),
        Err(MessageStoreError::Mailbox(error))
            if matches!(error.as_ref(), MailboxError::InvalidNotificationTransition {
                from: NotificationState::Staged,
                to: NotificationState::Submitted,
            })
    ));
}

#[test]
fn unknown_resolution_proof_version_is_rejected() {
    let (_scratch, mut store, message_id, bob) = operator_store("resolution-proof-version");
    alarm(&mut store, &message_id, bob, attempt(1), 2);
    let before = store.projection().last_sequence();
    assert!(matches!(
        append_resolution_at(
            &mut store,
            &message_id,
            bob,
            attempt(1),
            99,
            NotificationResolution::Complete,
            10,
        ),
        Err(MessageStoreError::Mailbox(error))
            if matches!(error.as_ref(), MailboxError::InvalidNotificationFact(message)
                if message.contains("unsupported notification resolution proof version 99"))
    ));
    assert_eq!(store.projection().last_sequence(), before);
}

/// A claim preserves post-write attention. Requeueing a broadcast creates
/// a new attempt only for recipients whose mailbox entry is still pending.
#[test]
fn claim_keeps_its_alarm_but_broadcast_requeue_skips_the_claimed_entry() {
    let scratch = StoreScratch::new("requeue-whole");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-multi").unwrap();

    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob, carol], "Multi", None),
            1,
        )
        .unwrap();
    // carol's alarm is the older one, so it is processed first.
    alarm(&mut store, &message_id, carol, attempt(2), 2);
    alarm(&mut store, &message_id, bob, attempt(1), 10);
    // Bob claims his message, but the post-write alarm stays open.
    store.claim_at(bob, message_id.clone(), 20).unwrap();
    assert_eq!(
        store
            .projection()
            .open_alarms_for_message(&message_id)
            .len(),
        2
    );

    let directory = MailboxDirectory::new(
        workspace,
        [
            MailboxIdentity {
                key: bob,
                label: "bob".into(),
            },
            MailboxIdentity {
                key: carol,
                label: "carol".into(),
            },
        ],
    )
    .unwrap();
    let before = store.projection().last_sequence();
    let service = MailboxService::new(directory, store);

    let requeued = service
        .requeue_message(message_id.clone())
        .expect("requeue is not an error");
    assert_eq!(
        requeued.len(),
        1,
        "only the redeliverable alarm is requeued"
    );
    assert_eq!(requeued[0].recipient, carol);
    assert_eq!(requeued[0].state, NotificationState::Queued);
    assert_ne!(
        requeued[0].attempt_id,
        attempt(2),
        "a requeue mints a fresh attempt"
    );

    let store = service.store().expect("store lock");
    // Exactly one fact is appended for Carol's new attempt.
    assert_eq!(
        store.projection().last_sequence(),
        before.map(|s| s + 1),
        "a requeue wrote more or less than the one fact it reported"
    );
    let bobs = store.projection().notification(bob, &message_id).unwrap();
    assert_eq!(bobs.attempt_id, attempt(1));
    assert_eq!(bobs.state, NotificationState::AttentionRequired);
    let retired = store
        .projection()
        .alarm_by_attempt(attempt(1))
        .expect("a claim preserves the current attempt identity");
    assert_eq!(retired.recipient, bob);
    assert_eq!(retired.state, NotificationState::AttentionRequired);
    assert_eq!(store.projection().open_alarms().len(), 1);
}

/// The reason an alarm was raised survives a restart.
///
/// The cause is the point of preview: an operator restarting the
/// daemon and seeing every alarm reduced to "attention required" has
/// lost the one fact that says what to do about it.
#[test]
fn an_alarm_cause_survives_a_restart() {
    let scratch = StoreScratch::new("cause-replay");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let verify = MessageId::new("m-verify").unwrap();
    let submit = MessageId::new("m-submit").unwrap();
    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(verify.clone(), draft(admin, vec![bob], "V", None), 1)
            .unwrap();
        store
            .accept_at(submit.clone(), draft(admin, vec![carol], "S", None), 2)
            .unwrap();
        alarm_because(
            &mut store,
            &verify,
            bob,
            attempt(1),
            10,
            NotificationAttentionCause::VerifyFailed,
        );
        alarm_because(
            &mut store,
            &submit,
            carol,
            attempt(2),
            20,
            NotificationAttentionCause::SubmitFailed,
        );
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let alarms = reopened.projection().open_alarms();
    assert_eq!(alarms.len(), 2);
    // Oldest first, each still carrying the cause it was raised with.
    assert_eq!(alarms[0].message_id, verify);
    assert_eq!(
        alarms[0].cause,
        Some(NotificationAttentionCause::VerifyFailed)
    );
    assert_eq!(
        alarms[0].verify_outcome,
        Some(NotificationVerifyOutcome::ambiguous())
    );
    assert_eq!(alarms[1].message_id, submit);
    assert_eq!(
        alarms[1].cause,
        Some(NotificationAttentionCause::SubmitFailed)
    );
    assert_eq!(alarms[1].verify_outcome, None);

    let snapshot = reopened
        .projection()
        .messages_snapshot(admin, 10, &HashMap::new())
        .unwrap();
    let verify_summary = &snapshot
        .rows
        .iter()
        .find(|row| row.message_id == verify)
        .unwrap()
        .recipients[0]
        .notification;
    assert_eq!(
        verify_summary.verify_outcome,
        Some(NotificationVerifyOutcome::ambiguous())
    );
}

/// Preview is ordered oldest first and hides what has been cleared.
#[test]
fn preview_lists_uncleared_alarms_oldest_first() {
    let scratch = StoreScratch::new("preview-order");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    let older = MessageId::new("m-older").unwrap();
    let newer = MessageId::new("m-newer").unwrap();
    store
        .accept_at(older.clone(), draft(admin, vec![bob], "Older", None), 1)
        .unwrap();
    store
        .accept_at(newer.clone(), draft(admin, vec![carol], "Newer", None), 2)
        .unwrap();
    alarm(&mut store, &older, bob, attempt(1), 10);
    alarm(&mut store, &newer, carol, attempt(2), 20);

    let alarms = store.projection().open_alarms();
    assert_eq!(alarms.len(), 2);
    assert_eq!(alarms[0].message_id, older);
    assert_eq!(alarms[1].message_id, newer);

    store
        .clear_notification_at(older.clone(), bob, attempt(1), 30)
        .unwrap();
    let alarms = store.projection().open_alarms();
    assert_eq!(alarms.len(), 1);
    assert_eq!(alarms[0].message_id, newer);
    assert_eq!(store.projection().open_alarms_for_message(&newer).len(), 1);
}

#[test]
fn notification_binding_survives_restart_and_explicit_recovery() {
    let scratch = StoreScratch::new("notification-restart");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-restart").unwrap();
    let attempt_id = attempt(1);
    let binding = notification_binding(bob);

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Restart", None),
                1,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Queued,
                None,
                None,
                2,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Gating,
                None,
                None,
                3,
            )
            .unwrap();
        store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Writing,
                Some(binding.clone()),
                None,
                4,
            )
            .unwrap();
        let staged = store
            .append_notification_transition_at(
                message_id.clone(),
                bob,
                attempt_id,
                NotificationState::Staged,
                None,
                None,
                5,
            )
            .unwrap();
        assert_eq!(staged.binding.as_ref(), Some(&binding));
        assert_eq!(
            store.projection().active_notification_barriers(),
            vec![staged.clone()]
        );
        assert_eq!(staged.transport, NotificationTransport::Doorbell);
        assert_eq!(staged.doorbell_format, None);
    }

    let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let staged = reopened
        .projection()
        .notification(bob, &message_id)
        .unwrap();
    assert_eq!(staged.state, NotificationState::Staged);
    assert_eq!(staged.binding.as_ref(), Some(&binding));
    assert_eq!(staged.transport, NotificationTransport::Doorbell);
    assert_eq!(staged.doorbell_format, None);
    assert_eq!(reopened.projection().last_sequence(), Some(5));

    let recovered = reopened.recover_notifications_after_restart().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].state, NotificationState::AttentionRequired);
    assert_eq!(
        recovered[0].cause,
        Some(NotificationAttentionCause::DaemonRestart)
    );
    assert_eq!(recovered[0].binding.as_ref(), Some(&binding));
    assert_eq!(recovered[0].transport, NotificationTransport::Doorbell);
    assert_eq!(recovered[0].doorbell_format, None);
    assert_eq!(reopened.projection().last_sequence(), Some(6));
    assert_eq!(
        reopened.projection().active_notification_barriers(),
        vec![recovered[0].clone()]
    );

    drop(reopened);
    let replayed = MessageStore::open(&root, journal, workspace, "boot-3").unwrap();
    assert_eq!(
        replayed
            .projection()
            .notification(bob, &message_id)
            .unwrap(),
        &recovered[0]
    );
    assert_eq!(
        replayed.projection().active_notification_barriers(),
        vec![recovered[0].clone()]
    );
}

/// The restart closure from the unsafe side: every attempt the last boot
/// left after its write and before a receipt closes to `daemon_restart`,
/// a claimed submit is the one receipt a restart can still honor, and a
/// second boot finds nothing left to close.
#[test]
fn restart_closes_every_unreceipted_write_to_daemon_restart_and_restores_nothing() {
    let scratch = StoreScratch::new("restart-closure");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let writing = MessageId::new("m-writing").unwrap();
    let submitted = MessageId::new("m-submitted").unwrap();
    let claimed = MessageId::new("m-claimed-unverified").unwrap();
    let raw = MessageId::new("m-raw").unwrap();

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        let mut ts = 0;
        let mut next = || {
            ts += 1;
            ts
        };
        for (index, (message_id, states)) in [
            (&writing, &[NotificationState::Writing][..]),
            (
                &submitted,
                &[NotificationState::Writing, NotificationState::Submitted][..],
            ),
            (&claimed, &[NotificationState::Writing][..]),
        ]
        .into_iter()
        .enumerate()
        {
            let attempt_id = attempt(index as u64 + 1);
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Restart", None),
                    next(),
                )
                .unwrap();
            for state in [NotificationState::Queued, NotificationState::Gating] {
                store
                    .append_notification_transition_at(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        state,
                        None,
                        None,
                        next(),
                    )
                    .unwrap();
            }
            for state in states {
                let writing = *state == NotificationState::Writing;
                store
                    .append_notification_transition_with_transport_at(
                        message_id.clone(),
                        bob,
                        attempt_id,
                        *state,
                        writing.then(|| notification_binding(bob)),
                        writing.then_some(NotificationTransport::Doorbell),
                        writing.then_some(cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM),
                        None,
                        next(),
                    )
                    .unwrap();
            }
        }
        // A claim that lands while the doorbell is being written, followed by
        // the Enter the daemon did not live to receipt.
        store.claim_at(bob, claimed.clone(), next()).unwrap();
        store
            .append_notification_transition_at(
                claimed.clone(),
                bob,
                attempt(3),
                NotificationState::SubmittedUnverified,
                None,
                None,
                next(),
            )
            .unwrap();

        store
            .accept_at(raw.clone(), draft(admin, vec![bob], "Raw", None), next())
            .unwrap();
        for state in [NotificationState::Queued, NotificationState::Gating] {
            store
                .append_notification_transition_at(
                    raw.clone(),
                    bob,
                    attempt(4),
                    state,
                    None,
                    None,
                    next(),
                )
                .unwrap();
        }
        store
            .append_notification_transition_with_transport_at(
                raw.clone(),
                bob,
                attempt(4),
                NotificationState::Writing,
                None,
                Some(NotificationTransport::Raw),
                None,
                None,
                next(),
            )
            .unwrap();
    }

    let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let recovered = reopened.recover_notifications_after_restart().unwrap();
    assert_eq!(recovered.len(), 4);
    let state_of = |message_id: &MessageId| {
        reopened
            .projection()
            .notification(bob, message_id)
            .cloned()
            .expect("recovered attempt")
    };
    for message_id in [&writing, &submitted, &raw] {
        let record = state_of(message_id);
        assert_eq!(record.state, NotificationState::AttentionRequired);
        assert_eq!(
            record.cause,
            Some(NotificationAttentionCause::DaemonRestart),
            "{message_id}"
        );
        assert_eq!(record.verified_by, None);
    }
    assert_eq!(state_of(&raw).transport, NotificationTransport::Raw);
    assert_eq!(state_of(&raw).binding, None);
    let receipted = state_of(&claimed);
    assert_eq!(
        receipted.state,
        NotificationState::Notified,
        "a claim after Enter is the receipt"
    );
    assert_eq!(receipted.cause, None);
    assert_eq!(receipted.verified_by, None);
    assert!(
        reopened
            .recover_notifications_after_restart()
            .unwrap()
            .is_empty(),
        "a second pass finds nothing left after the write boundary"
    );

    drop(reopened);
    let mut third = MessageStore::open(&root, journal, workspace, "boot-3").unwrap();
    assert!(third
        .recover_notifications_after_restart()
        .unwrap()
        .is_empty());
    assert_eq!(
        third
            .projection()
            .notification(bob, &writing)
            .unwrap()
            .state,
        NotificationState::AttentionRequired
    );
    assert_eq!(
        third
            .projection()
            .notification(bob, &claimed)
            .unwrap()
            .state,
        NotificationState::Notified
    );
}

#[test]
fn versioned_doorbell_formats_survive_attention_and_restart() {
    let scratch = StoreScratch::new("doorbell-format-restart");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let cases = [
        (MessageId::new("m-compact").unwrap(), attempt(1), 1),
        (MessageId::new("m-attempt-message").unwrap(), attempt(2), 2),
        (MessageId::new("m-attempt-only").unwrap(), attempt(3), 3),
        (MessageId::new("m-future").unwrap(), attempt(4), 999),
    ];

    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        for (index, (message_id, attempt_id, format)) in cases.iter().enumerate() {
            let base = 1 + index as u64 * 5;
            store
                .accept_at(
                    message_id.clone(),
                    draft(admin, vec![bob], "Format", None),
                    base,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    *attempt_id,
                    NotificationState::Queued,
                    None,
                    None,
                    base + 1,
                )
                .unwrap();
            store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    *attempt_id,
                    NotificationState::Gating,
                    None,
                    None,
                    base + 2,
                )
                .unwrap();
            store
                .append_notification_transition_with_transport_at(
                    message_id.clone(),
                    bob,
                    *attempt_id,
                    NotificationState::Writing,
                    Some(notification_binding(bob)),
                    Some(NotificationTransport::Doorbell),
                    Some(*format),
                    None,
                    base + 3,
                )
                .unwrap();
            let attention = store
                .append_notification_transition_at(
                    message_id.clone(),
                    bob,
                    *attempt_id,
                    NotificationState::AttentionRequired,
                    None,
                    Some(NotificationAttentionCause::VerifyFailed),
                    base + 4,
                )
                .unwrap();
            assert_eq!(attention.doorbell_format, Some(*format));
        }
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    for (message_id, _, format) in cases {
        let record = reopened
            .projection()
            .notification(bob, &message_id)
            .unwrap();
        assert_eq!(record.state, NotificationState::AttentionRequired);
        assert_eq!(record.transport, NotificationTransport::Doorbell);
        assert_eq!(record.doorbell_format, Some(format));
    }
}

#[test]
fn a_later_bound_write_replaces_an_older_notified_barrier_for_the_same_recipient() {
    let scratch = StoreScratch::new("newer-write-bounds-barriers");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let older = MessageId::new("m-older-notified").unwrap();
    let newer = MessageId::new("m-newer-notified").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    store
        .accept_at(older.clone(), draft(admin, vec![bob], "Older", None), 1)
        .unwrap();
    store
        .accept_at(newer.clone(), draft(admin, vec![bob], "Newer", None), 2)
        .unwrap();

    notify_with_binding(&mut store, &older, bob, attempt(1), 10);
    store
        .append_notification_transition_at(
            newer.clone(),
            bob,
            attempt(2),
            NotificationState::Queued,
            None,
            None,
            20,
        )
        .unwrap();
    store
        .append_notification_transition_at(
            newer.clone(),
            bob,
            attempt(2),
            NotificationState::Gating,
            None,
            None,
            21,
        )
        .unwrap();
    assert_eq!(
        store.projection().active_notification_barriers()[0].attempt_id,
        attempt(1),
        "a pre-write attempt cannot retire the older barrier"
    );

    store
        .append_notification_transition_at(
            newer.clone(),
            bob,
            attempt(2),
            NotificationState::Writing,
            Some(notification_binding(bob)),
            None,
            22,
        )
        .unwrap();
    for (offset, state) in [
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
        NotificationState::Notified,
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append_notification_transition_at(
                newer.clone(),
                bob,
                attempt(2),
                state,
                None,
                None,
                23 + offset as u64,
            )
            .unwrap();
    }

    let active = store.projection().active_notification_barriers();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].message_id, newer);
    assert_eq!(active[0].attempt_id, attempt(2));
    assert_eq!(active[0].state, NotificationState::Notified);
}

#[test]
fn bound_writes_for_different_recipients_keep_separate_barriers() {
    let scratch = StoreScratch::new("recipient-scoped-barriers");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let bob_message = MessageId::new("m-bob-notified").unwrap();
    let carol_message = MessageId::new("m-carol-notified").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    store
        .accept_at(bob_message.clone(), draft(admin, vec![bob], "Bob", None), 1)
        .unwrap();
    store
        .accept_at(
            carol_message.clone(),
            draft(admin, vec![carol], "Carol", None),
            2,
        )
        .unwrap();

    notify_with_binding(&mut store, &bob_message, bob, attempt(1), 10);
    notify_with_binding(&mut store, &carol_message, carol, attempt(2), 20);

    let active = store.projection().active_notification_barriers();
    assert_eq!(active.len(), 2);
    assert_eq!(active[0].recipient, bob);
    assert_eq!(active[0].attempt_id, attempt(1));
    assert_eq!(active[1].recipient, carol);
    assert_eq!(active[1].attempt_id, attempt(2));
}

#[test]
fn restart_recovers_only_the_newest_barrier_for_one_recipient() {
    let scratch = StoreScratch::new("restart-newest-recipient-barrier");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let older = MessageId::new("m-restart-older").unwrap();
    let newer = MessageId::new("m-restart-newer").unwrap();
    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(older.clone(), draft(admin, vec![bob], "Older", None), 1)
            .unwrap();
        store
            .accept_at(newer.clone(), draft(admin, vec![bob], "Newer", None), 2)
            .unwrap();
        notify_with_binding(&mut store, &older, bob, attempt(1), 10);
        notify_with_binding(&mut store, &newer, bob, attempt(2), 20);
    }

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let active = reopened.projection().active_notification_barriers();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].message_id, newer);
    assert_eq!(active[0].attempt_id, attempt(2));
    assert_eq!(active[0].state, NotificationState::Notified);
}

#[test]
fn an_attention_barrier_survives_until_a_later_bound_write() {
    let scratch = StoreScratch::new("attention-needs-later-write");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let alarmed = MessageId::new("m-alarmed-barrier").unwrap();
    let later = MessageId::new("m-later-write").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
    store
        .accept_at(alarmed.clone(), draft(admin, vec![bob], "Alarmed", None), 1)
        .unwrap();
    store
        .accept_at(later.clone(), draft(admin, vec![bob], "Later", None), 2)
        .unwrap();
    alarm_because(
        &mut store,
        &alarmed,
        bob,
        attempt(1),
        10,
        NotificationAttentionCause::VerifyFailed,
    );

    for (offset, state) in [NotificationState::Queued, NotificationState::Gating]
        .into_iter()
        .enumerate()
    {
        store
            .append_notification_transition_at(
                later.clone(),
                bob,
                attempt(2),
                state,
                None,
                None,
                20 + offset as u64,
            )
            .unwrap();
        let active = store.projection().active_notification_barriers();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].attempt_id, attempt(1));
        assert_eq!(active[0].state, NotificationState::AttentionRequired);
    }

    store
        .append_notification_transition_at(
            later,
            bob,
            attempt(2),
            NotificationState::Writing,
            Some(notification_binding(bob)),
            None,
            22,
        )
        .unwrap();
    let active = store.projection().active_notification_barriers();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].attempt_id, attempt(2));
    assert_eq!(active[0].state, NotificationState::Writing);
}

#[test]
fn a_leaderless_write_binding_arms_restart_recovery_through_replay() {
    let scratch = StoreScratch::new("legacy-recovery-binding");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-legacy-recovery").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob], "Legacy", None),
            1,
        )
        .unwrap();
    store
        .queue_notification(message_id.clone(), bob, attempt(1))
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();
    let mut incomplete = notification_binding(bob);
    incomplete.leader = None;
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Writing,
            Some(incomplete),
            None,
        )
        .unwrap();
    assert_eq!(store.projection().active_notification_barriers().len(), 1);
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Staged,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Submitting,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Submitted,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification(
            message_id,
            bob,
            attempt(1),
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::AckTimeout),
        )
        .unwrap();
    drop(store);

    let reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
    let active = reopened.projection().active_notification_barriers();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].attempt_id, attempt(1));
    assert_eq!(active[0].state, NotificationState::AttentionRequired);
    assert_eq!(active[0].binding.as_ref().unwrap().leader, None);
}

#[test]
fn attempt_locator_claim_accepts_only_the_current_authenticated_recipient() {
    let scratch = StoreScratch::new("attempt-claim");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-attempt-claim").unwrap();
    let old_attempt = attempt(1);
    let current_attempt = attempt(2);
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob], "Claim", None),
            1,
        )
        .unwrap();
    alarm_because(
        &mut store,
        &message_id,
        bob,
        old_attempt,
        2,
        NotificationAttentionCause::VerifyFailed,
    );
    store
        .requeue_notification(message_id.clone(), bob, old_attempt, current_attempt)
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            current_attempt,
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification_with_transport(
            message_id.clone(),
            bob,
            current_attempt,
            NotificationState::Writing,
            notification_binding(bob),
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
        )
        .unwrap();

    assert!(matches!(
        store.claim_notification_locator(
            bob,
            cyclops_proto::notification_attempt_claim_locator(old_attempt),
            old_attempt,
        ),
        Err(MessageStoreError::Mailbox(error))
            if matches!(error.as_ref(), MailboxError::NotificationAttemptUnknown(found)
                if *found == old_attempt)
    ));
    assert!(matches!(
        store.claim_notification_locator(
            carol,
            cyclops_proto::notification_attempt_claim_locator(current_attempt),
            current_attempt,
        ),
        Err(MessageStoreError::Mailbox(error))
            if matches!(error.as_ref(), MailboxError::NotificationAttemptUnknown(found)
                if *found == current_attempt)
    ));
    assert!(matches!(
        store
            .claim_notification_locator(
                bob,
                cyclops_proto::notification_attempt_claim_locator(current_attempt),
                current_attempt,
            )
            .unwrap()
            .0,
        ClaimOutcome::Claimed { message, .. } if message.message_id == message_id
    ));
}

#[test]
fn attempt_locator_distinguishes_legacy_messages_without_fallback_ambiguity() {
    let scratch = StoreScratch::new("attempt-locator-collision");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let never_issued = attempt(10);
    let legacy_locator = cyclops_proto::notification_attempt_claim_locator(never_issued);
    let current_attempt = attempt(11);
    let current_message = MessageId::new("m-current-attempt").unwrap();
    let colliding_locator = cyclops_proto::notification_attempt_claim_locator(current_attempt);
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();

    store
        .accept_at(
            legacy_locator.clone(),
            draft(admin, vec![bob], "Imported legacy locator", None),
            1,
        )
        .unwrap();
    assert!(matches!(
        store
            .claim_notification_locator(bob, legacy_locator.clone(), never_issued)
            .unwrap()
            .0,
        ClaimOutcome::Claimed { message, .. } if message.message_id == legacy_locator
    ));

    store
        .accept_at(
            current_message.clone(),
            draft(admin, vec![bob], "Current attempt", None),
            2,
        )
        .unwrap();
    store
        .queue_notification(current_message.clone(), bob, current_attempt)
        .unwrap();
    store
        .advance_notification(
            current_message.clone(),
            bob,
            current_attempt,
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();
    store
        .advance_notification_with_transport(
            current_message.clone(),
            bob,
            current_attempt,
            NotificationState::Writing,
            notification_binding(bob),
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
        )
        .unwrap();
    store
        .accept_at(
            colliding_locator.clone(),
            draft(admin, vec![bob], "Imported collision", None),
            3,
        )
        .unwrap();

    assert!(matches!(
        store.claim_notification_locator(bob, colliding_locator.clone(), current_attempt),
        Err(MessageStoreError::Mailbox(error))
            if matches!(
                error.as_ref(),
                MailboxError::NotificationAttemptClaimLocatorConflict(found)
                    if found == &colliding_locator
            )
    ));
    assert!(matches!(
        store.claim_notification_locator(carol, colliding_locator.clone(), current_attempt),
        Err(MessageStoreError::Mailbox(error))
            if matches!(
                error.as_ref(),
                MailboxError::NotificationAttemptUnknown(found)
                    if *found == current_attempt
            )
    ));
    assert!(store.projection().entry_is_pending(bob, &current_message));
    assert!(store.projection().entry_is_pending(bob, &colliding_locator));

    for state in [
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        store
            .advance_notification(
                current_message.clone(),
                bob,
                current_attempt,
                state,
                None,
                None,
            )
            .unwrap();
    }
    store
        .advance_notification(
            current_message.clone(),
            bob,
            current_attempt,
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::AckTimeout),
        )
        .unwrap();
    store
        .requeue_notification(current_message, bob, current_attempt, attempt(12))
        .unwrap();
    assert!(matches!(
        store.claim_notification_locator(bob, colliding_locator.clone(), current_attempt),
        Err(MessageStoreError::Mailbox(error))
            if matches!(
                error.as_ref(),
                MailboxError::NotificationAttemptUnknown(found)
                    if *found == current_attempt
            )
    ));
    assert!(store.projection().entry_is_pending(bob, &colliding_locator));
}

#[test]
fn notification_attempt_ids_are_unique_across_broadcast_recipients() {
    let scratch = StoreScratch::new("notification-broadcast");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, carol) = test_context();
    let message_id = MessageId::new("m-broadcast").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob, carol], "Broadcast", None),
            1,
        )
        .unwrap();

    store
        .queue_notification(message_id.clone(), bob, attempt(1))
        .unwrap();
    let error = store
        .queue_notification(message_id.clone(), carol, attempt(1))
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationAttemptReused(id) if *id == attempt(1))
    ));
    assert_eq!(store.projection().last_sequence(), Some(2));

    store
        .queue_notification(message_id.clone(), carol, attempt(2))
        .unwrap();
    assert_eq!(store.projection().notifications_for(bob).len(), 1);
    assert_eq!(store.projection().notifications_for(carol).len(), 1);
    assert_eq!(
        store
            .projection()
            .notification(carol, &message_id)
            .unwrap()
            .attempt_id,
        attempt(2)
    );
}

#[test]
fn doorbell_format_validation_is_failure_atomic() {
    let scratch = StoreScratch::new("doorbell-format-validation");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-format-validation").unwrap();
    let attempt_id = attempt(1);
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob], "Format", None),
            1,
        )
        .unwrap();
    store
        .queue_notification(message_id.clone(), bob, attempt_id)
        .unwrap();
    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();

    let direct = store
        .advance_notification_with_transport(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Writing,
            notification_binding(bob),
            NotificationTransport::DirectPayload,
            Some(DOORBELL_FORMAT_COMPACT_CLAIM),
        )
        .unwrap_err();
    assert!(matches!(
        direct,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationDoorbellFormatForbidden)
    ));
    assert_eq!(store.projection().last_sequence(), Some(3));

    let unknown = store
        .advance_notification_with_transport(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Writing,
            notification_binding(bob),
            NotificationTransport::Doorbell,
            Some(999),
        )
        .unwrap_err();
    assert!(matches!(
        unknown,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::UnsupportedNotificationDoorbellFormat(999))
    ));
    assert_eq!(store.projection().last_sequence(), Some(3));

    store
        .advance_notification_with_transport(
            message_id.clone(),
            bob,
            attempt_id,
            NotificationState::Writing,
            notification_binding(bob),
            NotificationTransport::Doorbell,
            Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
        )
        .unwrap();
    let non_writing = store
        .append_notification_transition_with_transport_at(
            message_id,
            bob,
            attempt_id,
            NotificationState::Staged,
            None,
            None,
            Some(DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM),
            None,
            5,
        )
        .unwrap_err();
    assert!(matches!(
        non_writing,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationDoorbellFormatForbidden)
    ));
    assert_eq!(store.projection().last_sequence(), Some(4));
}

#[test]
fn illegal_notification_transitions_and_requeues_are_failure_atomic() {
    let scratch = StoreScratch::new("notification-illegal");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-illegal").unwrap();
    let mut store = MessageStore::open(&root, journal, workspace, "boot").unwrap();
    store
        .accept_at(
            message_id.clone(),
            draft(admin, vec![bob], "Illegal", None),
            1,
        )
        .unwrap();

    let error = store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationNotFound { .. })
    ));
    assert_eq!(store.projection().last_sequence(), Some(1));

    store
        .queue_notification(message_id.clone(), bob, attempt(1))
        .unwrap();
    let error = store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::VerifyFailed),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::InvalidNotificationTransition {
                from: NotificationState::Queued,
                to: NotificationState::AttentionRequired,
            })
    ));
    assert_eq!(store.projection().last_sequence(), Some(2));

    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Gating,
            None,
            None,
        )
        .unwrap();
    let error = store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Writing,
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationBindingRequired)
    ));
    assert_eq!(store.projection().last_sequence(), Some(3));

    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::Writing,
            Some(notification_binding(bob)),
            None,
        )
        .unwrap();
    let error = store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::AckTimeout),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::InvalidNotificationCause {
                cause: NotificationAttentionCause::AckTimeout,
                state: NotificationState::Writing,
            })
    ));

    store
        .advance_notification(
            message_id.clone(),
            bob,
            attempt(1),
            NotificationState::AttentionRequired,
            None,
            Some(NotificationAttentionCause::VerifyFailed),
        )
        .unwrap();
    assert_eq!(store.projection().last_sequence(), Some(5));

    let error = store
        .queue_notification(message_id.clone(), bob, attempt(2))
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationAttemptMismatch { .. })
    ));
    let error = store
        .requeue_notification(message_id.clone(), bob, attempt(1), attempt(1))
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationAttemptReused(id) if *id == attempt(1))
    ));

    store
        .requeue_notification(message_id.clone(), bob, attempt(1), attempt(2))
        .unwrap();
    let error = store
        .requeue_notification(message_id.clone(), bob, attempt(2), attempt(3))
        .unwrap_err();
    assert!(matches!(
        error,
        MessageStoreError::Mailbox(inner)
            if matches!(inner.as_ref(), MailboxError::NotificationRequeueRequiresAttention)
    ));
    assert_eq!(store.projection().last_sequence(), Some(6));

    let forged = LedgerLine {
        seq: 7,
        boot_id: "forged".into(),
        id: message_id.to_string(),
        ts: 7,
        kind: Kind::State,
        from: "human".into(),
        to: vec![bob.to_string()],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: Vec::new(),
        data: Some(
            serde_json::to_value(NotificationFact::NotificationTransition {
                record_version: CANONICAL_RECORD_VERSION,
                attempt_id: attempt(2),
                message_id: message_id.clone(),
                recipient: bob,
                state: NotificationState::Gating,
                binding: None,
                transport: None,
                doorbell_format: None,
                cause: None,
                verified_by: None,
                verify_outcome: None,
                pre_write_cause: None,
                wake_block: None,
                pre_write_observation: None,
            })
            .unwrap(),
        ),
    };
    let error = store.projection.apply_line(&forged).unwrap_err();
    assert!(matches!(
        error,
        MailboxError::PresentationMismatch { field: "from", .. }
    ));
    assert_eq!(store.projection().last_sequence(), Some(6));
}

#[test]
fn notification_replay_recovers_torn_tail_and_refuses_bad_facts() {
    let scratch = StoreScratch::new("notification-corruption");
    let root = scratch.root();
    let journal = Path::new("workspaces/current/messages.ndjson");
    let (workspace, admin, bob, _) = test_context();
    let message_id = MessageId::new("m-corrupt").unwrap();
    {
        let mut store = MessageStore::open(&root, journal, workspace, "boot-1").unwrap();
        store
            .accept_at(
                message_id.clone(),
                draft(admin, vec![bob], "Corruption", None),
                1,
            )
            .unwrap();
        store
            .queue_notification(message_id.clone(), bob, attempt(1))
            .unwrap();
    }
    {
        let mut file = root.open_append(journal).unwrap();
        file.write_all(br#"{"seq":3,"boot_id":"boot-1","id":"m-corrupt""#)
            .unwrap();
        file.sync_data().unwrap();
    }
    {
        let mut reopened = MessageStore::open(&root, journal, workspace, "boot-2").unwrap();
        reopened
            .advance_notification(
                message_id.clone(),
                bob,
                attempt(1),
                NotificationState::Gating,
                None,
                None,
            )
            .unwrap();
        assert_eq!(reopened.projection().last_sequence(), Some(3));
    }
    {
        let malformed = LedgerLine {
            seq: 4,
            boot_id: "boot-2".into(),
            id: message_id.to_string(),
            ts: 4,
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: vec![bob.to_string()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::json!({
                "type": "notification_transition",
                "record_version": CANONICAL_RECORD_VERSION,
                "attempt_id": "att-bad!",
                "message_id": message_id,
                "recipient": bob,
                "state": "writing",
                "binding": null
            })),
        };
        let mut file = root.open_append(journal).unwrap();
        serde_json::to_writer(&mut file, &malformed).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_data().unwrap();
    }

    assert!(matches!(
        MessageStore::open(&root, journal, workspace, "boot-3"),
        Err(MessageStoreError::Mailbox(inner))
            if matches!(inner.as_ref(), MailboxError::InvalidNotificationFact(_))
    ));
}
