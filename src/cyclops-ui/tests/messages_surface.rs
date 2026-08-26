//! One authenticated snapshot, turned into the queue a person reads.
//!
//! The adapter is where the wire shape and the reading shape differ: the
//! wire answers per message with a recipient list, and a person acts per
//! message and recipient. Everything below is a rule about that gap.

use std::str::FromStr;

use cyclops_proto::{
    Kind, MailboxEntryState, MessageDirection, MessageId, MessageNotificationSettlement,
    MessageNotificationState, MessageNotificationSummary, MessageQuotaState,
    MessageRecipientSummary, MessageSnapshotRow, MessagesChangedArea, MessagesChangedData,
    MessagesSnapshotCounts, MessagesSnapshotResult, NotificationAttemptId,
    NotificationAttentionCause, NotificationPreWriteCause, RecipientKey, SessionInstanceId,
    TmuxPaneId, WorkspaceId,
};
use cyclops_ui::queue::render;
use cyclops_ui::{
    rows_from_snapshot, Direction, HumanQueue, QueueTarget, RefreshGate, Scope, WakeWord,
};

const SIZES: [(usize, usize); 5] = [(14, 8), (24, 12), (80, 24), (96, 24), (160, 40)];

fn workspace() -> WorkspaceId {
    WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap()
}

fn agent(pane: &str) -> RecipientKey {
    RecipientKey::agent(
        workspace(),
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap(),
        TmuxPaneId::from_str(pane).unwrap(),
    )
}

fn attempt(n: u64) -> NotificationAttemptId {
    NotificationAttemptId::parse(&format!("att-00000000-0000-4000-8000-{n:012x}")).unwrap()
}

fn wake(state: MessageNotificationState) -> MessageNotificationSummary {
    MessageNotificationSummary {
        state,
        wake_block: None,
        quota_state: None,
        settlement: None,
        operator_withdrawn: None,
        attempt_id: None,
        cause: None,
        verify_outcome: None,
        pre_write_cause: None,
        pre_write_block: None,
        pre_write_pane_width: None,
        pre_write_required_pane_width: None,
        attention_cleared: None,
        resolution: None,
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        updated_at: Some(5_000),
    }
}

fn alarm(n: u64, cleared: bool) -> MessageNotificationSummary {
    MessageNotificationSummary {
        state: MessageNotificationState::AttentionRequired,
        wake_block: None,
        quota_state: None,
        settlement: None,
        operator_withdrawn: None,
        attempt_id: Some(attempt(n)),
        cause: Some(NotificationAttentionCause::VerifyFailed),
        verify_outcome: None,
        pre_write_cause: None,
        pre_write_block: None,
        pre_write_pane_width: None,
        pre_write_required_pane_width: None,
        attention_cleared: Some(cleared),
        resolution: None,
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        updated_at: Some(6_000),
    }
}

fn quota(n: u64, state: MessageQuotaState) -> MessageNotificationSummary {
    MessageNotificationSummary {
        state: MessageNotificationState::AttentionRequired,
        wake_block: None,
        quota_state: Some(state),
        settlement: None,
        operator_withdrawn: None,
        attempt_id: Some(attempt(n)),
        cause: None,
        verify_outcome: None,
        pre_write_cause: None,
        pre_write_block: None,
        pre_write_pane_width: None,
        pre_write_required_pane_width: None,
        attention_cleared: None,
        resolution: None,
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        updated_at: Some(6_000),
    }
}

fn to(
    pane: &str,
    label: &str,
    notification: MessageNotificationSummary,
    direction: MessageDirection,
    needs_action: bool,
) -> MessageRecipientSummary {
    MessageRecipientSummary {
        recipient: agent(pane),
        label: label.into(),
        direction,
        needs_action,
        can_manage_attention: false,
        can_withdraw_notification: false,
        current_route: None,
        available: true,
        mailbox: MailboxEntryState::Pending,
        fifo_position: Some(1),
        notification,
    }
}

/// The caller's own mailbox: inbound, and waiting on them.
fn mine(pane: &str, label: &str, n: MessageNotificationSummary) -> MessageRecipientSummary {
    to(pane, label, n, MessageDirection::Inbound, true)
}

/// An alarm the caller is allowed to act on.
///
/// Attention recovery is admin work, and `target_for` only addresses an
/// attempt when the row is NOT the caller's own pending mail. A recipient
/// looking at their own unclaimed message gets a claim target however
/// loudly its doorbell is ringing, which is the point of keeping mailbox
/// and attention targets distinct. So an attention row is the admin's
/// view: outbound, and a mailbox that is not theirs to claim.
fn alarmed(pane: &str, label: &str, n: MessageNotificationSummary) -> MessageRecipientSummary {
    let mut r = to(pane, label, n, MessageDirection::Outbound, true);
    r.mailbox = MailboxEntryState::Claimed {
        claimant: agent(pane),
        claimed_at: 2_000,
    };
    r
}

/// Somebody else's mailbox on a message the caller can see. In neither
/// their inbox nor their work, whatever the message-level answer says.
fn theirs(pane: &str, label: &str, n: MessageNotificationSummary) -> MessageRecipientSummary {
    to(pane, label, n, MessageDirection::Workspace, false)
}

fn row(
    id: &str,
    seq: u64,
    direction: MessageDirection,
    needs_action: bool,
    recipients: Vec<MessageRecipientSummary>,
) -> MessageSnapshotRow {
    MessageSnapshotRow {
        message_id: MessageId::new(id).unwrap(),
        seq,
        ts: seq * 1000,
        kind: Kind::Msg,
        direction,
        sender: RecipientKey::admin(workspace()),
        sender_label: "admin".into(),
        recipients,
        subject: Some(format!("subject for {id}")),
        reply_to: None,
        thread_root: MessageId::new(id).unwrap(),
        thread_message_count: 1,
        active: true,
        needs_action,
    }
}

fn snapshot(seq: u64, rows: Vec<MessageSnapshotRow>) -> MessagesSnapshotResult {
    MessagesSnapshotResult {
        workspace_id: workspace(),
        caller: None,
        workspace_seq: seq,
        counts: MessagesSnapshotCounts {
            visible_messages: rows.len() as u64,
            returned_messages: rows.len() as u64,
            inbox_messages: 0,
            outbound_messages: 0,
            work_messages: 0,
            active_messages: rows.len() as u64,
            settled_messages: 0,
            pending_entries: 0,
            claimed_entries: 0,
            open_attention_entries: 0,
        },
        rows,
        mailbox_attention: Vec::new(),
    }
}

fn changed(seq: u64) -> MessagesChangedData {
    MessagesChangedData {
        workspace_id: workspace(),
        workspace_seq: seq,
        changed: [MessagesChangedArea::Messages].into_iter().collect(),
    }
}

fn loaded() -> HumanQueue {
    let wire = snapshot(
        42,
        vec![
            row(
                "m-broadcast",
                1,
                MessageDirection::Inbound,
                true,
                vec![
                    mine("%1", "reviewer", wake(MessageNotificationState::Notified)),
                    // The alarmed mailbox is not the caller's. An
                    // administrator can see it; it is not their inbox.
                    to(
                        "%2",
                        "codex",
                        alarm(7, false),
                        MessageDirection::Workspace,
                        true,
                    ),
                ],
            ),
            row(
                "m-sent",
                2,
                MessageDirection::Outbound,
                false,
                vec![mine(
                    "%3",
                    "impl",
                    wake(MessageNotificationState::NotStarted),
                )],
            ),
        ],
    );
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&wire));
    q
}

/// One row per message and recipient, and an open alarm owns its row.
///
/// A broadcast to two agents is two pieces of work, and the recipient
/// whose wake attempt failed is addressed by the attempt, because that is
/// what an action resolves. Neither recipient gets a second row.
#[test]
fn a_broadcast_becomes_one_row_per_recipient_and_never_two_per_pair() {
    let mut q = loaded();
    q.set_scope(Scope::All);
    assert_eq!(q.len(), 3, "two recipients plus one outbound");

    // Every row of a broadcast is its own identity, and exactly one of
    // them carries the attempt. The identity does not encode the alarm.
    let attempts: Vec<Option<NotificationAttemptId>> = q.visible().map(|r| r.attention).collect();
    assert_eq!(
        attempts.iter().filter(|a| a.is_some()).count(),
        1,
        "only the alarmed recipient carries an attempt"
    );
    assert!(attempts.contains(&Some(attempt(7))));
    let targets: Vec<QueueTarget> = q.visible().map(|r| r.target.clone()).collect();
    assert!(targets.contains(&QueueTarget::new(
        MessageId::new("m-broadcast").unwrap(),
        agent("%1"),
    )));

    // The alarmed pair appears once, not as a message row and an alarm.
    let broadcast: Vec<_> = q
        .visible()
        .filter(|r| r.message_id.as_str() == "m-broadcast")
        .collect();
    assert_eq!(broadcast.len(), 2);
    assert_eq!(
        broadcast
            .iter()
            .filter(|r| r.recipient == agent("%2"))
            .count(),
        1,
        "the alarmed recipient has exactly one row"
    );

    // Targets stay distinct even though two rows share a message id.
    // On the TARGET, not on `id()`: that is display text, documented as
    // showing the same string for two rows of one broadcast. The pair is
    // what has to be unique, because the pair is what selection, freezing
    // and every token comparison key on.
    let unique: std::collections::HashSet<&QueueTarget> = targets.iter().collect();
    assert_eq!(unique.len(), 3, "{targets:?}");
}

/// Scope follows the mailbox, not the message.
///
/// The message-level direction and needs_action answer once for the whole
/// message. Stamping them onto every fanned row put other recipients'
/// mail in the caller's inbox and made one recipient's alarm look like
/// work on all of them. The fixture below says the opposite thing at the
/// two levels on purpose: the message reads inbound and work, while only
/// one of the two mailboxes is the caller's.
#[test]
fn recipient_scope_beats_the_message_level_answer() {
    let wire = snapshot(
        3,
        vec![row(
            "m-broadcast",
            1,
            // What a copying adapter would spread over both rows.
            MessageDirection::Inbound,
            true,
            vec![
                mine("%1", "reviewer", wake(MessageNotificationState::Notified)),
                to(
                    "%2",
                    "codex",
                    alarm(7, false),
                    MessageDirection::Workspace,
                    true,
                ),
            ],
        )],
    );
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&wire));

    q.set_scope(Scope::All);
    assert_eq!(q.len(), 2);
    let ours = q
        .visible()
        .find(|r| r.recipient == agent("%1"))
        .expect("the caller's mailbox");
    let other = q
        .visible()
        .find(|r| r.recipient == agent("%2"))
        .expect("the other mailbox");
    assert_eq!(ours.direction, Direction::Inbound);
    assert_eq!(
        other.direction,
        Direction::Observed,
        "the message reads inbound, but this mailbox is not the caller's"
    );

    // Inbox is the caller's mailbox alone.
    q.set_scope(Scope::Inbox);
    let ids: Vec<RecipientKey> = q.visible().map(|r| r.recipient).collect();
    assert_eq!(ids, vec![agent("%1")], "another mailbox reached the inbox");

    // Outbound is empty: the caller sent nothing here.
    q.set_scope(Scope::Outbound);
    assert_eq!(q.len(), 0);

    // Work holds the caller's pending row and the alarm an administrator
    // is being asked to look at, and each is there on its own answer.
    q.set_scope(Scope::Work);
    let work: Vec<RecipientKey> = q.visible().map(|r| r.recipient).collect();
    assert_eq!(work.len(), 2);
    assert!(work.contains(&agent("%1")) && work.contains(&agent("%2")));

    // And with the alarm acknowledged, only the caller's own row is work.
    let calm = snapshot(
        4,
        vec![row(
            "m-broadcast",
            1,
            MessageDirection::Inbound,
            true,
            vec![
                mine("%1", "reviewer", wake(MessageNotificationState::Notified)),
                theirs("%2", "codex", alarm(7, true)),
            ],
        )],
    );
    q.replace(rows_from_snapshot(&calm));
    q.set_scope(Scope::Work);
    let work: Vec<RecipientKey> = q.visible().map(|r| r.recipient).collect();
    assert_eq!(
        work,
        vec![agent("%1")],
        "one recipient's alarm spread to another mailbox"
    );
}

/// Every durable notification phase reaches the queue without collapsing.
#[test]
fn wake_states_map_without_losing_delivery_progress() {
    let cases = [
        (MessageNotificationState::NotStarted, WakeWord::NotStarted),
        (MessageNotificationState::Queued, WakeWord::Queued),
        (MessageNotificationState::Gating, WakeWord::Gating),
        (MessageNotificationState::Writing, WakeWord::Writing),
        (MessageNotificationState::Staged, WakeWord::Staged),
        (MessageNotificationState::Submitted, WakeWord::Submitted),
        (MessageNotificationState::Notified, WakeWord::Notified),
        (MessageNotificationState::Superseded, WakeWord::Superseded),
    ];
    for (wire, word) in cases {
        let q = {
            let mut q = HumanQueue::new();
            q.replace(rows_from_snapshot(&snapshot(
                1,
                vec![row(
                    "m-1",
                    1,
                    MessageDirection::Inbound,
                    true,
                    vec![mine("%1", "reviewer", wake(wire))],
                )],
            )));
            q.set_scope(Scope::All);
            q
        };
        assert_eq!(q.visible().next().unwrap().wake, word, "{wire:?}");
    }
    assert_ne!(WakeWord::Queued, WakeWord::Gating);
    assert_ne!(WakeWord::Gating, WakeWord::Staged);
    assert_ne!(WakeWord::Staged, WakeWord::Submitted);

    let mut withdrawn = wake(MessageNotificationState::NotStarted);
    withdrawn.settlement = Some(MessageNotificationSettlement::WithdrawnByClaim);
    let mut queue = HumanQueue::new();
    queue.replace(rows_from_snapshot(&snapshot(
        2,
        vec![row(
            "m-withdrawn",
            2,
            MessageDirection::Inbound,
            false,
            vec![mine("%1", "reviewer", withdrawn)],
        )],
    )));
    queue.set_scope(Scope::All);
    assert_eq!(queue.visible().next().unwrap().wake, WakeWord::Withdrawn);

    let mut blocked = wake(MessageNotificationState::Gating);
    blocked.attempt_id = Some(attempt(21));
    blocked.pre_write_cause = Some(NotificationPreWriteCause::BindingUnprovable);
    let mut blocked_to = theirs("%1", "reviewer", blocked);
    blocked_to.needs_action = true;
    blocked_to.can_withdraw_notification = true;
    let rows = rows_from_snapshot(&snapshot(
        3,
        vec![row(
            "m-blocked",
            3,
            MessageDirection::Workspace,
            true,
            vec![blocked_to],
        )],
    ));
    assert_eq!(rows.rows[0].wake, WakeWord::BlockedBeforeWrite);
    assert!(rows.rows[0].needs_human());
    assert!(rows.rows[0].can_withdraw_notification);

    // The named block rides from the wire into the row; the width pair is
    // decided once by the proto rule, so a cause without widths has none.
    let mut named = wake(MessageNotificationState::Gating);
    named.attempt_id = Some(attempt(22));
    named.pre_write_cause = Some(NotificationPreWriteCause::WriteReadinessChanged);
    named.pre_write_block = Some("hook_admission_unproven".into());
    let rows = rows_from_snapshot(&snapshot(
        3,
        vec![row(
            "m-named",
            3,
            MessageDirection::Workspace,
            true,
            vec![theirs("%1", "reviewer", named)],
        )],
    ));
    assert_eq!(rows.rows[0].wake, WakeWord::BlockedBeforeWrite);
    assert_eq!(
        rows.rows[0].pre_write_block.as_deref(),
        Some("hook_admission_unproven")
    );
    assert_eq!(rows.rows[0].pane_width_block, None);

    let mut operator_withdrawn = wake(MessageNotificationState::NotStarted);
    operator_withdrawn.attempt_id = Some(attempt(21));
    operator_withdrawn.operator_withdrawn = Some(true);
    let rows = rows_from_snapshot(&snapshot(
        4,
        vec![row(
            "m-operator-withdrawn",
            4,
            MessageDirection::Workspace,
            false,
            vec![theirs("%1", "reviewer", operator_withdrawn)],
        )],
    ));
    assert_eq!(rows.rows[0].wake, WakeWord::WithdrawnByOperator);
    assert!(!rows.rows[0].needs_human());

    for (quota_state, word) in [
        (MessageQuotaState::Held, WakeWord::QuotaHeld),
        (
            MessageQuotaState::ResetObserved,
            WakeWord::QuotaResetObserved,
        ),
    ] {
        let rows = rows_from_snapshot(&snapshot(
            1,
            vec![row(
                "m-quota",
                1,
                MessageDirection::Outbound,
                true,
                vec![alarmed("%1", "reviewer", quota(10, quota_state))],
            )],
        ));
        assert_eq!(rows.rows[0].wake, word);
        assert!(rows.rows[0].needs_human());
        assert!(!rows.rows[0].can_manage_attention);
    }

    let mut uncertain = alarm(10, false);
    uncertain.resolution_intent = Some(cyclops_proto::NotificationResolution::Complete);
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&snapshot(
        1,
        vec![row(
            "m-uncertain",
            1,
            MessageDirection::Outbound,
            true,
            vec![to(
                "%1",
                "reviewer",
                uncertain,
                MessageDirection::Outbound,
                true,
            )],
        )],
    )));
    q.set_scope(Scope::All);
    let only = q.visible().next().unwrap();
    assert_eq!(only.wake, WakeWord::ResolutionIncomplete);
    assert!(only.needs_human());
    assert_eq!(only.attention, Some(attempt(10)));
    assert_eq!(
        only.resolution_intent,
        Some(cyclops_proto::NotificationResolution::Complete)
    );
    assert_eq!(only.resolution_action_accepted, None);

    let mut accepted = alarm(10, false);
    accepted.resolution_intent = Some(cyclops_proto::NotificationResolution::Complete);
    accepted.resolution_action_accepted = Some(cyclops_proto::NotificationResolution::Complete);
    let rows = rows_from_snapshot(&snapshot(
        2,
        vec![row(
            "m-accepted-uncertain",
            2,
            MessageDirection::Outbound,
            true,
            vec![to(
                "%1",
                "reviewer",
                accepted,
                MessageDirection::Outbound,
                true,
            )],
        )],
    ));
    assert_eq!(
        rows.rows[0].resolution_action_accepted,
        Some(cyclops_proto::NotificationResolution::Complete)
    );
    assert_eq!(rows.rows[0].resolution_consumption_observed, None);

    let mut consumed = alarm(10, false);
    consumed.resolution_intent = Some(cyclops_proto::NotificationResolution::Complete);
    consumed.resolution_action_accepted = Some(cyclops_proto::NotificationResolution::Complete);
    consumed.resolution_consumption_observed = Some(
        cyclops_proto::NotificationResolutionConsumptionObservation {
            evidence: cyclops_proto::NotificationResolutionConsumptionEvidence::WorkingEdge,
            observed_at_ms: 7_000,
        },
    );
    let rows = rows_from_snapshot(&snapshot(
        3,
        vec![row(
            "m-consumed-uncertain",
            3,
            MessageDirection::Outbound,
            true,
            vec![to(
                "%1",
                "reviewer",
                consumed,
                MessageDirection::Outbound,
                true,
            )],
        )],
    ));
    assert!(rows.rows[0].resolution_consumption_observed.is_some());

    // A recipient acts on their pending message, not on the admin-only
    // alarm raised by its failed wake.
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&snapshot(
        2,
        vec![row(
            "m-recipient-alarm",
            2,
            MessageDirection::Inbound,
            true,
            vec![mine("%1", "reviewer", alarm(11, false))],
        )],
    )));
    let only = q.visible().next().unwrap();
    assert!(only.attention.is_none() || !only.can_manage_attention);

    // An incomplete attention summary stays visible but cannot manufacture
    // an attempt target.
    let mut incomplete = alarm(12, false);
    incomplete.resolution_intent = Some(cyclops_proto::NotificationResolution::Discard);
    incomplete.attempt_id = None;
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&snapshot(
        3,
        vec![row(
            "m-incomplete-alarm",
            3,
            MessageDirection::Outbound,
            true,
            vec![to(
                "%1",
                "reviewer",
                incomplete,
                MessageDirection::Outbound,
                true,
            )],
        )],
    )));
    let only = q.visible().next().unwrap();
    assert_eq!(only.wake, WakeWord::ResolutionIncomplete);
    assert_eq!(
        only.resolution_intent,
        Some(cyclops_proto::NotificationResolution::Discard)
    );
    assert!(only.attention.is_none() || !only.can_manage_attention);

    // An acknowledged alarm reads as cleared and is no longer work.
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&snapshot(
        1,
        vec![row(
            "m-1",
            1,
            MessageDirection::Inbound,
            false,
            vec![theirs("%1", "reviewer", alarm(9, true))],
        )],
    )));
    q.set_scope(Scope::All);
    let only = q.visible().next().unwrap();
    assert_eq!(only.wake, WakeWord::Cleared);
    assert!(
        !only.can_manage_attention,
        "a cleared alarm is not an action target"
    );
    // For the observer who acknowledged it. The recipient's own pending
    // entry is a different question and stays their work.
    q.set_scope(Scope::Work);
    assert_eq!(
        q.len(),
        0,
        "an acknowledged alarm is still work for its observer"
    );

    for (resolution, word) in [
        (
            cyclops_proto::NotificationResolution::Complete,
            WakeWord::ResolvedSubmitted,
        ),
        (
            cyclops_proto::NotificationResolution::Discard,
            WakeWord::ResolvedDiscarded,
        ),
    ] {
        let mut resolved = alarm(10, false);
        resolved.resolution = Some(resolution);
        let mut q = HumanQueue::new();
        q.replace(rows_from_snapshot(&snapshot(
            2,
            vec![row(
                "m-2",
                2,
                MessageDirection::Outbound,
                false,
                vec![theirs("%1", "reviewer", resolved)],
            )],
        )));
        q.set_scope(Scope::All);
        let only = q.visible().next().unwrap();
        assert_eq!(only.wake, word);
        assert!(!only.can_manage_attention);
    }
}

/// The resting queue names the exact phase instead of reducing progress to waiting.
#[test]
fn rendered_rows_distinguish_gating_staged_and_submitted() {
    let mut queue = HumanQueue::new();
    queue.replace(rows_from_snapshot(&snapshot(
        3,
        vec![
            row(
                "m-gating",
                1,
                MessageDirection::Inbound,
                true,
                vec![mine(
                    "%1",
                    "gating-agent",
                    wake(MessageNotificationState::Gating),
                )],
            ),
            row(
                "m-staged",
                2,
                MessageDirection::Inbound,
                true,
                vec![mine(
                    "%2",
                    "staged-agent",
                    wake(MessageNotificationState::Staged),
                )],
            ),
            row(
                "m-submitted",
                3,
                MessageDirection::Inbound,
                true,
                vec![mine(
                    "%3",
                    "submitted-agent",
                    wake(MessageNotificationState::Submitted),
                )],
            ),
        ],
    )));
    queue.set_scope(Scope::All);

    let frame = render(&queue, 160, 24).join("\n");
    for phase in [WakeWord::Gating, WakeWord::Staged, WakeWord::Submitted] {
        assert!(frame.contains(phase.cell()), "missing {phase:?}: {frame}");
    }
}

/// Direction and Work are answered for whoever asked.
#[test]
fn direction_and_work_are_caller_relative() {
    let wire = snapshot(
        1,
        vec![
            row(
                "m-in",
                1,
                MessageDirection::Inbound,
                true,
                vec![mine(
                    "%1",
                    "reviewer",
                    wake(MessageNotificationState::Notified),
                )],
            ),
            row(
                "m-self",
                2,
                MessageDirection::SelfAddressed,
                true,
                vec![to(
                    "%2",
                    "codex",
                    wake(MessageNotificationState::Notified),
                    MessageDirection::SelfAddressed,
                    true,
                )],
            ),
            row(
                "m-out",
                3,
                MessageDirection::Outbound,
                false,
                vec![to(
                    "%3",
                    "impl",
                    wake(MessageNotificationState::Notified),
                    MessageDirection::Outbound,
                    false,
                )],
            ),
            row(
                "m-other",
                4,
                MessageDirection::Workspace,
                false,
                vec![theirs(
                    "%4",
                    "docs",
                    wake(MessageNotificationState::Notified),
                )],
            ),
        ],
    );
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&wire));

    q.set_scope(Scope::All);
    assert_eq!(q.len(), 4);

    // A message addressed to yourself is in your mailbox.
    q.set_scope(Scope::Inbox);
    let ids: Vec<&str> = q.visible().map(|r| r.message_id.as_str()).collect();
    assert_eq!(ids, vec!["m-in", "m-self"]);

    q.set_scope(Scope::Outbound);
    let ids: Vec<&str> = q.visible().map(|r| r.message_id.as_str()).collect();
    assert_eq!(ids, vec!["m-out"]);

    // Somebody else's mail is observed, and belongs to neither mailbox.
    q.set_scope(Scope::All);
    let observed = q
        .visible()
        .find(|r| r.message_id.as_str() == "m-other")
        .map(|r| r.direction);
    assert_eq!(observed, Some(Direction::Observed));

    // Work is what the daemon marked, not a guess from direction.
    q.set_scope(Scope::Work);
    let ids: Vec<&str> = q.visible().map(|r| r.message_id.as_str()).collect();
    assert_eq!(ids, vec!["m-in", "m-self"]);
}

/// A row keeps the durable keys, so a rename cannot move an action.
#[test]
fn rows_carry_durable_keys_not_labels() {
    let mut q = loaded();
    q.set_scope(Scope::All);
    let row = q
        .visible()
        .find(|r| r.recipient == agent("%1"))
        .expect("the first recipient");
    assert_eq!(row.recipient, agent("%1"));
    assert_eq!(row.recipient_label, "reviewer");
    assert_eq!(
        row.target,
        QueueTarget::new(MessageId::new("m-broadcast").unwrap(), agent("%1"))
    );
    assert_eq!(q.watermark(), 42, "the snapshot watermark rides the queue");
}

/// An empty workspace renders, at every size, without pretending.
#[test]
fn an_empty_snapshot_renders_at_every_size() {
    let mut q = HumanQueue::new();
    q.replace(rows_from_snapshot(&snapshot(7, Vec::new())));
    assert_eq!(q.len(), 0);
    assert!(q.freeze().is_none(), "nothing selected, nothing to freeze");

    for (w, h) in SIZES {
        let frame = render(&q, w, h);
        assert_eq!(frame.len(), h, "{w}x{h}");
        for line in &frame {
            assert_eq!(
                cyclops_ui::grid::display_width(line),
                w,
                "{w}x{h}: {line:?}"
            );
        }
    }
}

/// The surface draws at every size the workspace can hand it, with a
/// broadcast and an alarm present, and never shows a body.
#[test]
fn the_surface_draws_at_every_size_without_a_body() {
    let mut q = loaded();
    for scope in Scope::ORDER {
        q.set_scope(scope);
        for (w, h) in SIZES {
            let frame = render(&q, w, h);
            assert_eq!(frame.len(), h, "{scope:?} {w}x{h}");
            for line in &frame {
                assert_eq!(
                    cyclops_ui::grid::display_width(line),
                    w,
                    "{scope:?} {w}x{h}: {line:?}"
                );
            }
        }
    }
}

/// The refresh contract, driven directly: one fetch in flight, one
/// follow-up however many edges land during it, and a reconnect that
/// replaces everything before later edges are believed. Nothing here
/// reads a clock, because nothing in the gate does.
#[test]
fn the_refresh_gate_fetches_once_per_edge_burst_and_resnapshots_on_reconnect() {
    let mut gate = RefreshGate::new();

    // Nothing happens before the daemon is reachable.
    gate.mark_dirty();
    assert!(gate.begin().is_none(), "a fetch before connecting");

    gate.connected();
    assert!(gate.is_connected());
    let first = gate.begin().expect("a reconnect owes a whole snapshot");
    assert!(gate.is_fetching());

    // Edges during a fetch do not start a second one.
    assert!(gate.begin().is_none());
    gate.messages_changed(&changed(1));
    gate.messages_changed(&changed(2));
    gate.messages_changed(&changed(3));
    assert!(
        gate.begin().is_none(),
        "a second fetch while one is in flight"
    );

    // Exactly one follow-up for the whole burst.
    assert!(gate.finish_snapshot(first, &snapshot(3, Vec::new())));
    let follow_up = gate.begin().expect("the burst is owed one follow-up");
    assert!(gate.finish_snapshot(follow_up, &snapshot(3, Vec::new())));
    assert!(gate.begin().is_none(), "a fetch nobody asked for");
    gate.messages_changed(&changed(3));
    assert!(gate.begin().is_none(), "a duplicate edge caused a fetch");

    // A gap drops the in-flight read: its answer would predate the gap.
    gate.mark_dirty();
    assert!(gate.begin().is_some());
    gate.disconnected();
    assert!(!gate.is_connected());
    assert!(!gate.is_fetching());
    assert!(gate.begin().is_none(), "a fetch while disconnected");

    // Coming back owes a whole snapshot, whether or not an edge arrived.
    gate.connected();
    assert!(gate.begin().is_some());
}

/// A skipped journal edge means an answer started under the old horizon
/// cannot replace the queue, even if it happens to report the new number.
#[test]
fn a_sequence_gap_discards_the_in_flight_answer_and_forces_a_new_snapshot() {
    let mut gate = RefreshGate::new();
    gate.connected();
    let seed = gate.begin().unwrap();
    assert!(gate.finish_snapshot(seed, &snapshot(10, Vec::new())));

    gate.mark_dirty();
    let stale = gate.begin().unwrap();
    gate.messages_changed(&changed(12));
    assert!(
        !gate.finish_snapshot(stale, &snapshot(12, Vec::new())),
        "a request from before the gap replaced the queue"
    );

    let replacement = gate.begin().expect("the gap owes a whole snapshot");
    assert!(gate.finish_snapshot(replacement, &snapshot(12, Vec::new())));
    assert!(gate.begin().is_none());
}

/// A second successful subscription is a new generation. An answer from
/// the old socket cannot clear or replace the new generation's request.
#[test]
fn a_second_subscription_rejects_the_first_generations_answer() {
    let mut gate = RefreshGate::new();
    gate.connected();
    let old = gate.begin().unwrap();
    gate.disconnected();

    gate.connected();
    let current = gate.begin().unwrap();
    assert!(!gate.finish_snapshot(old, &snapshot(1, Vec::new())));
    assert!(
        gate.is_fetching(),
        "the stale answer cleared the new request"
    );
    assert!(gate.finish_snapshot(current, &snapshot(1, Vec::new())));
}

/// The surface is a third view in the existing stream UI, not a second
/// application and not a replacement for either stream view.
#[test]
fn messages_is_a_sibling_view_of_the_two_stream_views() {
    use cyclops_ui::{build, App, Filter, Key, Theme, View};

    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    assert_eq!(app.view, View::Admin);
    app.handle_key(Key::Tab);
    assert_eq!(app.view, View::Firehose, "the stream views still cycle");
    app.handle_key(Key::Tab);
    assert_eq!(app.view, View::Messages);
    app.handle_key(Key::Tab);
    assert_eq!(app.view, View::Admin, "the cycle closes");

    // With a snapshot applied, the Messages view draws the queue.
    app.view = View::Messages;
    app.apply_messages(&snapshot(
        9,
        vec![row(
            "m-visible",
            1,
            MessageDirection::Outbound,
            true,
            vec![alarmed("%1", "reviewer", alarm(3, false))],
        )],
    ));
    assert_eq!(app.queue.watermark(), 9);
    assert_eq!(app.queue.len(), 1);

    let frame = build(&mut app, 96, 24);
    assert_eq!(frame.len(), 24);
    let text = frame.join("\n");
    // The alarm shows as a wake word. The attempt id is not row text any
    // more: the row is the message, and the exact attempt belongs to the
    // detail and to the confirmation that names what an action will do.
    assert!(text.contains("needs attention"), "{text}");
    assert!(text.contains("m-visible"), "{text}");
    // The stream view is untouched behind it.
    app.view = View::Admin;
    let stream = build(&mut app, 96, 24);
    assert_eq!(stream.len(), 24);
    assert!(!stream.join("\n").contains("needs attention"));
}

/// A snapshot arriving clears the in-flight fetch, so the next edge is
/// allowed to start one.
#[test]
fn applying_a_snapshot_releases_the_gate() {
    use cyclops_ui::{App, Filter, Theme, View};

    let mut app = App::new(Theme::none(), View::Messages, Filter::default());
    app.refresh.connected();
    let request = app.wants_messages().expect("a reconnect owes a snapshot");
    assert!(app.wants_messages().is_none(), "one fetch at a time");

    assert!(app
        .apply_messages_response(request, &snapshot(1, Vec::new()))
        .is_some());
    assert!(app.wants_messages().is_none(), "nothing changed since");

    app.refresh.mark_dirty();
    assert!(
        app.wants_messages().is_some(),
        "an edge after a snapshot is owed one"
    );
}

/// The two levels survive the wire as separate answers.
///
/// A serde round trip, because the whole defect was one level being read
/// where the other was meant. If a rename or a flatten ever collapsed
/// them, every scope on this surface would silently answer for the
/// message again.
#[test]
fn recipient_and_message_scoping_are_separate_on_the_wire() {
    let wire = snapshot(
        1,
        vec![row(
            "m-1",
            1,
            MessageDirection::Inbound,
            true,
            vec![theirs(
                "%9",
                "someone",
                wake(MessageNotificationState::Notified),
            )],
        )],
    );
    let json = serde_json::to_value(&wire).expect("snapshot serializes");
    let row = &json["rows"][0];
    assert_eq!(row["direction"], "inbound");
    assert_eq!(row["needs_action"], true);
    assert_eq!(row["recipients"][0]["direction"], "workspace");
    assert_eq!(row["recipients"][0]["needs_action"], false);

    let back: MessagesSnapshotResult = serde_json::from_value(json).expect("snapshot round trips");
    assert_eq!(back, wire);
    let to = &back.rows[0].recipients[0];
    assert_eq!(to.direction, MessageDirection::Workspace);
    assert!(!to.needs_action);
    assert_eq!(back.rows[0].direction, MessageDirection::Inbound);
    assert!(back.rows[0].needs_action);
}
