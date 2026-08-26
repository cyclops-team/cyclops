//! The open detail: what it allows, what it refuses, and what it keeps.
//!
//! All pure. The state machine and the renderer are the parts that decide
//! whether an operator can mis-target an action, so they are tested
//! without a daemon in the way.

use std::str::FromStr;

use cyclops_proto::{
    MessageId, MessageRecipientRoute, NotificationAttemptId, NotificationAttentionCause,
    NotificationPreWriteCause, NotificationResolution, RecipientKey, SessionInstanceId, TmuxPaneId,
    WorkspaceId,
};
use cyclops_ui::detail::render;
use cyclops_ui::{
    Action, Back, Check, Detail, Direction, Loaded, MailboxWord, QueueRow, QueueTarget, Request,
    Stage, ThreadEntry, WakeWord,
};

const SIZES: [(usize, usize); 3] = [(14, 8), (80, 24), (160, 40)];

fn agent(pane: &str) -> RecipientKey {
    RecipientKey::agent(
        WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap(),
        TmuxPaneId::from_str(pane).unwrap(),
    )
}

fn attempt(n: u64) -> NotificationAttemptId {
    NotificationAttemptId::parse(&format!("att-00000000-0000-4000-8000-{n:012x}")).unwrap()
}

fn row(
    target: QueueTarget,
    direction: Direction,
    mailbox: MailboxWord,
    wake: WakeWord,
) -> QueueRow {
    QueueRow {
        target,
        attention: None,
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        can_manage_attention: false,
        can_withdraw_notification: false,
        message_id: MessageId::new("m-001").unwrap(),
        recipient: agent("%1"),
        recipient_label: "reviewer".into(),
        subject: Some("a subject".into()),
        mailbox,
        wake,
        cause: None,
        pre_write_cause: None,
        wake_block: None,
        pre_write_pane_width: None,
        pre_write_required_pane_width: None,
        current_route: None,
        fifo_position: Some(1),
        needs_action: true,
        seq: 1,
        updated_at: 1000,
        direction,
        ..Default::default()
    }
}

/// An alarm this reader is authorized to resolve. The capability is the
/// daemon's answer, so a test that wants the actions has to say so.
fn alarm_row(
    attempt_id: NotificationAttemptId,
    direction: Direction,
    mailbox: MailboxWord,
    wake: WakeWord,
) -> QueueRow {
    let mut r = row(
        QueueTarget::new(MessageId::new("m-001").unwrap(), agent("%1")),
        direction,
        mailbox,
        wake,
    );
    r.attention = Some(attempt_id);
    r.can_manage_attention = true;
    r
}

fn inbound_pending() -> QueueRow {
    row(
        QueueTarget::new(MessageId::new("m-001").unwrap(), agent("%1")),
        Direction::Inbound,
        MailboxWord::Pending,
        WakeWord::Notified,
    )
}

fn outbound() -> QueueRow {
    row(
        QueueTarget::new(MessageId::new("m-001").unwrap(), agent("%1")),
        Direction::Outbound,
        MailboxWord::Pending,
        WakeWord::Notified,
    )
}

fn alarmed() -> QueueRow {
    let mut r = alarm_row(
        attempt(7),
        Direction::Inbound,
        MailboxWord::Pending,
        WakeWord::NeedsAttention,
    );
    r.cause = Some(NotificationAttentionCause::VerifyFailed);
    r
}

fn blocked() -> QueueRow {
    let mut r = row(
        QueueTarget::new(MessageId::new("m-001").unwrap(), agent("%1")),
        Direction::Observed,
        MailboxWord::Pending,
        WakeWord::BlockedBeforeWrite,
    );
    r.attention = Some(attempt(8));
    r.can_withdraw_notification = true;
    r.pre_write_cause = Some(NotificationPreWriteCause::BindingUnprovable);
    r.current_route = Some(MessageRecipientRoute {
        label: "reviewer-now".into(),
        pane_id: "%1".parse().unwrap(),
    });
    r
}

fn checks(all_pass: bool) -> Vec<Check> {
    vec![
        Check {
            name: "pane still holds the staged text".into(),
            passed: true,
            detail: None,
        },
        Check {
            name: "occupant unchanged since the write".into(),
            passed: all_pass,
            detail: None,
        },
    ]
}

fn opened(row: &QueueRow, loaded: Loaded) -> Detail {
    let mut d = Detail::open(row, 42);
    d.loaded_ok(loaded);
    d
}

/// Only what the row and the daemon's answer allow, and nothing before
/// the read comes back.
#[test]
fn a_detail_offers_only_the_actions_its_target_allows() {
    // Nothing at all while the read is in flight.
    let opening = Detail::open(&inbound_pending(), 42);
    assert_eq!(*opening.stage(), Stage::Opening);
    assert!(opening.allowed().is_empty(), "actions before the read");

    // Opening an inbound pending row claims it, so there is no claim
    // action. Reply requires the daemon's read authorization, not a
    // non-empty body value.
    let d = opened(&inbound_pending(), Loaded::default());
    assert!(
        d.allowed().is_empty(),
        "an action without an authorized body"
    );
    let d = opened(
        &inbound_pending(),
        Loaded {
            body: Some("the payload".into()),
            ..Loaded::default()
        },
    );
    assert_eq!(d.allowed(), vec![Action::Reply]);

    let empty = opened(
        &inbound_pending(),
        Loaded {
            body_authorized: true,
            ..Loaded::default()
        },
    );
    assert_eq!(empty.allowed(), vec![Action::Reply]);
    let frame = cyclops_ui::detail::render(&empty, 80, 24).join("\n");
    assert!(frame.contains("message has no body"), "{frame}");
    assert!(!frame.contains("body not authorized"), "{frame}");

    // A row you SENT offers no reply, even though you can read the body
    // because you wrote it. The daemon routes a reply to the parent's
    // sender, so replying here would address the message to yourself.
    let d = opened(
        &outbound(),
        Loaded {
            body: Some("the payload".into()),
            ..Loaded::default()
        },
    );
    assert!(
        d.allowed().is_empty(),
        "reply offered on an outbound row, which would send to yourself"
    );
    assert!(!d.allows(Action::Reply));

    // An alarm offers acknowledgement. Complete and discard appear only
    // when every check passed.
    let d = opened(
        &alarmed(),
        Loaded {
            checks: checks(false),
            ..Loaded::default()
        },
    );
    assert_eq!(d.allowed(), vec![Action::ClearAlarm]);
    assert!(!d.allows(Action::AttentionComplete));
    assert!(!d.allows(Action::AttentionDiscard));

    let d = opened(
        &alarmed(),
        Loaded {
            checks: checks(true),
            ..Loaded::default()
        },
    );
    assert!(d.allows(Action::AttentionComplete) && d.allows(Action::AttentionDiscard));

    // A message target never offers an attention action, whatever else
    // is true of it.
    let d = opened(
        &inbound_pending(),
        Loaded {
            checks: checks(true),
            body: Some("b".into()),
            ..Loaded::default()
        },
    );
    for forbidden in [
        Action::AttentionComplete,
        Action::AttentionDiscard,
        Action::ClearAlarm,
    ] {
        assert!(!d.allows(forbidden), "{forbidden:?} offered on a message");
    }
}

#[test]
fn a_blocked_wake_offers_one_exact_recipient_scoped_withdrawal() {
    let row = blocked();
    let mut detail = opened(&row, Loaded::default());
    assert_eq!(detail.allowed(), vec![Action::WithdrawNotification]);

    let Request::Confirm(sentence) = detail.request(Action::WithdrawNotification) else {
        panic!("withdrawal must require exact confirmation");
    };
    assert!(sentence.contains(&attempt(8).to_string()), "{sentence}");
    assert!(sentence.contains(&agent("%1").to_string()), "{sentence}");

    assert_eq!(detail.confirm(), Some(Action::WithdrawNotification));
    detail.done(Action::WithdrawNotification, "wake withdrawn");
    assert!(!detail.can_withdraw_notification());
    assert_eq!(detail.wake(), WakeWord::WithdrawnByOperator);
    assert!(!detail.allowed().contains(&Action::WithdrawNotification));

    let frame = render(&detail, 80, 24).join("\n");
    assert!(frame.contains("message remains claimable"), "{frame}");
}

#[test]
fn a_width_block_says_what_was_observed_and_required() {
    let mut row = blocked();
    row.pre_write_cause = Some(NotificationPreWriteCause::WriteReadinessChanged);
    row.pre_write_pane_width = Some(59);
    row.pre_write_required_pane_width = Some(60);

    let frame = render(&opened(&row, Loaded::default()), 80, 24).join("\n");
    assert!(
        frame.contains("pane too narrow (59, requires 60)"),
        "{frame}"
    );
    assert!(!frame.contains("write readiness changed"), "{frame}");
}

#[test]
fn a_blocked_detail_names_the_durable_scheduler_cause() {
    let mut row = blocked();
    row.wake_block = Some(cyclops_proto::MessageWakeBlock::WorkerSupervisorExited);

    let frame = render(&opened(&row, Loaded::default()), 80, 24).join("\n");
    assert!(frame.contains("worker supervisor exited"), "{frame}");
    assert!(!frame.contains("scheduler state unavailable"), "{frame}");
}

#[test]
fn every_daemon_authorized_unwritten_wake_offers_withdrawal() {
    for wake in [WakeWord::Queued, WakeWord::Gating] {
        let mut row = row(
            QueueTarget::new(MessageId::new("m-001").unwrap(), agent("%1")),
            Direction::Observed,
            MailboxWord::Pending,
            wake,
        );
        row.attention = Some(attempt(9));
        row.can_withdraw_notification = true;
        row.needs_action = false;

        let detail = opened(&row, Loaded::default());
        assert_eq!(detail.allowed(), vec![Action::WithdrawNotification]);
    }

    for wake in [WakeWord::Writing, WakeWord::Staged, WakeWord::Submitted] {
        let mut row = row(
            QueueTarget::new(MessageId::new("m-001").unwrap(), agent("%1")),
            Direction::Observed,
            MailboxWord::Pending,
            wake,
        );
        row.attention = Some(attempt(10));
        row.can_withdraw_notification = true;
        assert!(opened(&row, Loaded::default()).allowed().is_empty());
    }
}

#[test]
fn quota_detail_explains_wait_and_the_message_wide_admin_command() {
    let mut held_row = outbound();
    held_row.wake = WakeWord::QuotaHeld;
    held_row.attention = Some(attempt(9));
    let held = opened(&held_row, Loaded::default());
    assert!(held.allowed().is_empty());
    let held_text = render(&held, 160, 40).join("\n");
    assert!(held_text.contains("wait for a quota reset"), "{held_text}");
    assert!(
        held_text.contains("will not resume automatically"),
        "{held_text}"
    );

    let mut reset_row = held_row;
    reset_row.wake = WakeWord::QuotaResetObserved;
    let reset = opened(&reset_row, Loaded::default());
    assert!(reset.allowed().is_empty());
    let reset_text = render(&reset, 160, 40).join("\n");
    assert!(
        reset_text.contains("`cyclops requeue m-001`"),
        "{reset_text}"
    );
    assert!(
        reset_text.contains("every eligible recipient on the message"),
        "{reset_text}"
    );
}

/// Anything that changes what an agent sees is confirmed by name first,
/// and a refusal to confirm changes nothing.
#[test]
fn destructive_actions_confirm_by_name_and_cancel_clean() {
    let mut d = opened(
        &alarmed(),
        Loaded {
            checks: checks(true),
            ..Loaded::default()
        },
    );

    let request = d.request(Action::AttentionDiscard);
    let sentence = match request {
        Request::Confirm(s) => s,
        other => panic!("discard did not ask: {other:?}"),
    };
    assert!(
        sentence.contains(&attempt(7).to_string()),
        "the confirmation does not name the attempt: {sentence}"
    );
    assert!(
        sentence.contains("reviewer") && sentence.contains(&agent("%1").to_string()),
        "the confirmation does not name the frozen broadcast recipient: {sentence}"
    );
    assert!(sentence.contains("discard"), "{sentence}");
    assert_eq!(*d.stage(), Stage::Confirming(Action::AttentionDiscard));

    // Escape drops it and nothing was performed.
    assert_eq!(d.escape(), Back::Cancelled);
    assert_eq!(*d.stage(), Stage::Open);
    assert_eq!(d.confirm(), None, "a cancelled confirmation still fired");

    // Saying yes hands back exactly the action that was confirmed.
    d.request(Action::AttentionComplete);
    assert_eq!(d.confirm(), Some(Action::AttentionComplete));
    assert_eq!(*d.stage(), Stage::Acting(Action::AttentionComplete));

    // Reply is the operator's own writing and is not confirmed.
    let mut d = opened(
        &inbound_pending(),
        Loaded {
            body: Some("b".into()),
            ..Loaded::default()
        },
    );
    assert_eq!(d.request(Action::Reply), Request::Perform(Action::Reply));

    // An action the row does not allow is refused without a stage change.
    let mut d = opened(&outbound(), Loaded::default());
    assert!(matches!(d.request(Action::Reply), Request::Refused(_)));
    assert_eq!(*d.stage(), Stage::Open);
}

#[test]
fn a_broadcast_action_names_the_frozen_recipient() {
    let mut selected = alarmed();
    selected.target = QueueTarget::new(MessageId::new("m-001").unwrap(), agent("%2"));
    selected.recipient = agent("%2");
    selected.recipient_label = "codex-reviewer".into();
    let mut detail = opened(
        &selected,
        Loaded {
            checks: checks(true),
            ..Loaded::default()
        },
    );

    let Request::Confirm(sentence) = detail.request(Action::AttentionDiscard) else {
        panic!("broadcast recipient action did not ask for confirmation");
    };
    assert!(sentence.contains("codex-reviewer"), "{sentence}");
    assert!(sentence.contains(&agent("%2").to_string()), "{sentence}");
    assert!(!sentence.contains(&agent("%1").to_string()), "{sentence}");
}

/// A row that vanished under an open detail freezes it rather than
/// moving it. Nothing retargets, and nothing acts.
#[test]
fn a_vanished_target_stops_actions_and_never_retargets() {
    let mut d = opened(
        &alarmed(),
        Loaded {
            checks: checks(true),
            ..Loaded::default()
        },
    );
    let frozen = d.target().clone();
    d.request(Action::AttentionDiscard);
    assert_eq!(*d.stage(), Stage::Confirming(Action::AttentionDiscard));

    // The snapshot no longer lists it. The confirmation on screen still
    // names what the operator read.
    d.observe_snapshot(None);
    assert!(d.is_stale());
    assert_eq!(d.target(), &frozen, "the frozen target moved");
    assert_eq!(
        *d.stage(),
        Stage::Confirming(Action::AttentionDiscard),
        "a snapshot cancelled a confirmation the operator opened"
    );
    assert!(
        d.allowed().is_empty(),
        "a vanished row still offered actions"
    );

    // Coming back does not silently re-arm anything either.
    d.observe_snapshot(Some(&alarmed()));
    assert!(!d.is_stale());
    assert_eq!(d.target(), &frozen);
}

/// A failure keeps the detail open, keeps the draft, and keeps one key
/// for one set of bytes.
#[test]
fn a_failure_preserves_the_draft_and_its_idempotency_key() {
    let mut d = opened(
        &inbound_pending(),
        Loaded {
            body: Some("the payload".into()),
            ..Loaded::default()
        },
    );
    d.draft_mut().set("half a reply");
    d.draft_mut().push('!');

    let mut minted = 0;
    let mut mint = || {
        minted += 1;
        format!("key-{minted}")
    };
    let first = d.draft_mut().key_for_send(&mut mint);

    // The send is uncertain: it may have landed.
    d.failed(Some(Action::Reply), "connection closed before a receipt");
    assert!(matches!(d.stage(), Stage::Failed { .. }));
    assert_eq!(
        d.draft().text(),
        "half a reply!",
        "the draft was lost on failure"
    );

    // Retrying the same bytes reuses the key, so the daemon can refuse a
    // duplicate rather than accept a second message.
    let again = d.draft_mut().key_for_send(&mut mint);
    assert_eq!(again, first, "a retry of one draft took a second key");

    // Editing makes it a different message, which takes a new key.
    d.draft_mut().push('?');
    let edited = d.draft_mut().key_for_send(&mut mint);
    assert_ne!(edited, first, "edited bytes reused the old key");

    // The detail is still usable after a failure.
    assert!(d.allows(Action::Reply));
    assert_eq!(d.escape(), Back::Closed);
}

/// The detail renders at the sizes the workspace hands it, and never
/// shows a body the daemon did not authorize.
#[test]
fn the_detail_renders_at_every_size_and_leaks_no_body() {
    let secret = "SUPERSECRETPAYLOAD";
    let unauthorized = opened(&outbound(), Loaded::default());
    let authorized = opened(
        &inbound_pending(),
        Loaded {
            body: Some(secret.into()),
            thread: vec![ThreadEntry {
                message_id: "m-000".into(),
                sender_label: "admin".into(),
                subject: Some("earlier".into()),
                body: None,
                ts: 1,
            }],
            claim_note: Some("claimed now".into()),
            ..Loaded::default()
        },
    );

    for (w, h) in SIZES {
        for (name, d) in [("unauthorized", &unauthorized), ("authorized", &authorized)] {
            let frame = render(d, w, h);
            assert_eq!(frame.len(), h, "{name} {w}x{h}: wrong height");
            for line in &frame {
                assert_eq!(
                    cyclops_ui::grid::display_width(line),
                    w,
                    "{name} {w}x{h}: {line:?}"
                );
            }
        }
        // The unauthorized detail says so and shows nothing.
        let text = render(&unauthorized, w, h).join("\n");
        assert!(!text.contains(secret), "{w}x{h}: body leaked");
    }

    // Where a body was authorized and there is room, it is shown.
    assert!(render(&authorized, 80, 24).join("\n").contains(secret));
    // At sidebar width there is no room for one, so there is none.
    assert!(!render(&authorized, 14, 8).join("\n").contains(secret));
}

/// The confirmation sentence survives into the frame, naming the target.
#[test]
fn an_open_confirmation_names_its_target_on_screen() {
    let mut d = opened(
        &alarmed(),
        Loaded {
            checks: checks(true),
            ..Loaded::default()
        },
    );
    d.request(Action::AttentionComplete);
    let text = render(&d, 160, 40).join("\n");
    assert!(text.contains(&attempt(7).to_string()), "{text}");
    assert!(text.contains("submit staged notification"), "{text}");
    assert!(text.contains("esc"), "no way out on screen");
}

/// Ambiguity withholds exactly what it must and nothing else.
///
/// The two terminal verbs are not idempotent, so an ambiguous outcome
/// from one of them must not offer either again. Reply carries an
/// idempotency key and clearing an alarm is idempotent by design, so
/// blocking those would strand an operator over somebody else's doubt.
#[test]
fn ambiguity_withholds_only_the_verbs_that_cannot_repeat() {
    let loaded = || Loaded {
        checks: checks(true),
        ..Loaded::default()
    };

    // A terminal verb whose outcome is unknown retires the pair, and
    // leaves the idempotent action standing.
    let mut d = opened(&alarmed(), loaded());
    d.request(Action::AttentionDiscard);
    d.confirm();
    d.uncertain(
        Some(Action::AttentionDiscard),
        "socket closed after the send",
    );
    assert!(matches!(d.stage(), Stage::Uncertain { .. }));
    assert_eq!(
        d.allowed(),
        vec![Action::ClearAlarm],
        "ambiguity took away more than the terminal pair"
    );

    // A request that never left changes nothing at all.
    let mut d = opened(&alarmed(), loaded());
    d.not_sent(Some(Action::AttentionComplete), "connect: no such file");
    assert!(matches!(d.stage(), Stage::NotSent { .. }));
    assert_eq!(
        d.allowed(),
        vec![
            Action::AttentionComplete,
            Action::AttentionDiscard,
            Action::ClearAlarm
        ],
        "a request the daemon never saw withheld something"
    );

    // A conflict says the attempt is already resolved. The terminal pair
    // never comes back, and no later reload revives it.
    let mut d = opened(&alarmed(), loaded());
    d.refused(
        Some(Action::AttentionComplete),
        "conflict",
        "already resolved",
    );
    assert!(d.is_resolved());
    assert_eq!(d.allowed(), vec![Action::ClearAlarm]);
    d.loaded_ok(loaded());
    assert_eq!(
        d.allowed(),
        vec![Action::ClearAlarm],
        "a reload revived a resolved attempt"
    );

    // A success does the same: what landed cannot land again.
    let mut d = opened(&alarmed(), loaded());
    d.done(Action::AttentionComplete, "notification submitted");
    assert!(d.is_resolved());
    assert_eq!(d.allowed(), vec![Action::ClearAlarm]);

    // Ambiguity about a reply says nothing about the alarm.
    let mut d = opened(&alarmed(), loaded());
    d.uncertain(Some(Action::ClearAlarm), "no answer");
    assert!(
        d.allows(Action::AttentionComplete),
        "a clear that went unanswered blocked the terminal verbs"
    );

    // The screen explains an ambiguous outcome rather than inviting one.
    let mut d = opened(&alarmed(), loaded());
    d.uncertain(Some(Action::AttentionDiscard), "no answer");
    let text = render(&d, 160, 40).join("\n");
    assert!(text.contains("may have landed"), "{text}");
    assert!(text.contains("reopen"), "{text}");
}

/// Requeue is not offered here.
///
/// msg.requeue acts on every uncleared alarm of a message, so a
/// confirmation naming one attempt could mutate another recipient's on a
/// broadcast. Until the verb is scoped to one attempt it stays in the CLI.
#[test]
fn requeue_is_absent_from_a_per_attempt_detail() {
    let d = opened(
        &alarmed(),
        Loaded {
            checks: checks(true),
            ..Loaded::default()
        },
    );
    let words: Vec<&str> = d.allowed().iter().map(|a| a.word()).collect();
    assert!(
        !words.iter().any(|w| w.contains("requeue")),
        "requeue offered against one attempt: {words:?}"
    );
    assert_eq!(
        words,
        vec![
            "submit staged notification",
            "discard staged notification",
            "clear alarm"
        ]
    );
}

/// A long body scrolls and keeps every byte, and its shape survives.
#[test]
fn a_long_body_scrolls_and_keeps_its_shape() {
    let long_token = "x".repeat(400);
    let body = format!(
        "first line\n\n    indented line kept as written\n{}\n{}",
        long_token,
        (0..80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut d = opened(
        &inbound_pending(),
        Loaded {
            body: Some(body.clone()),
            ..Loaded::default()
        },
    );

    let first = render(&d, 80, 24);
    assert_eq!(first.len(), 24);
    for line in &first {
        assert_eq!(cyclops_ui::grid::display_width(line), 80);
    }
    // Indentation is content, not noise.
    assert!(
        first.iter().any(|l| l.starts_with("    indented")),
        "indentation was reflowed away"
    );

    // 7: every byte survives a wrap boundary, stated as the property
    // rather than inspected through a padded frame. A rendered line is
    // padded to the width, so trailing spaces are invisible there and an
    // assertion made on frames passes whether or not they were dropped.
    for text in [
        "alpha  beta   gamma delta epsilon zeta eta theta iota kappa lambda mu nu",
        "        deeply indented and then some words that will certainly wrap over",
        &"z".repeat(300),
        "trailing spaces at the end   ",
        "a b  c   d    e     f      g       h        i         j          k",
    ] {
        for width in [8usize, 13, 24, 80] {
            let pieces = cyclops_ui::detail::wrap(text, width);
            assert_eq!(
                pieces.concat(),
                *text,
                "wrap at {width} changed the text: {pieces:?}"
            );
            for piece in &pieces {
                assert!(
                    cyclops_ui::grid::display_width(piece) <= width,
                    "wrap at {width} produced a line wider than the frame: {piece:?}"
                );
            }
        }
    }

    // Scrolling reaches what the first frame could not show, and the
    // long token is carried across rows rather than cut.
    let mut seen = first.join("");
    for _ in 0..40 {
        d.scroll_by(4);
        seen.push_str(&render(&d, 80, 24).join(""));
    }
    assert!(
        seen.contains("line 79"),
        "the end of the body was unreachable"
    );
    assert!(
        seen.matches('x').count() >= 400,
        "the long token lost bytes: {} of 400",
        seen.matches('x').count()
    );

    // Scrolling past the end still renders a full, exact frame.
    d.scroll_by(10_000);
    let last = render(&d, 80, 24);
    assert_eq!(last.len(), 24);
    for line in &last {
        assert_eq!(cyclops_ui::grid::display_width(line), 80);
    }
}

/// The subject and both state dimensions survive Enter.
#[test]
fn the_detail_keeps_the_subject_and_both_states() {
    let d = opened(&inbound_pending(), Loaded::default());
    assert_eq!(d.subject(), Some("a subject"));
    let text = render(&d, 160, 40).join("\n");
    assert!(text.contains("a subject"), "{text}");
    assert!(text.contains("pending"), "mailbox state lost: {text}");
    assert!(text.contains("notified"), "wake state lost: {text}");
}

/// A row that stays but changes underneath owes a fresh read, and still
/// never moves.
#[test]
fn changed_facts_owe_a_reload_without_retargeting() {
    let mut d = opened(
        &alarmed(),
        Loaded {
            checks: checks(true),
            ..Loaded::default()
        },
    );
    let frozen = d.target().clone();
    assert!(!d.needs_reload());

    // Same target, different facts: the alarm was cleared elsewhere.
    let mut moved = alarmed();
    moved.wake = WakeWord::Cleared;
    d.observe_snapshot(Some(&moved));
    assert!(d.needs_reload(), "changed facts did not owe a read");
    assert!(!d.is_stale(), "a present row was called stale");
    assert_eq!(d.target(), &frozen, "the target moved with the facts");

    // The reload landing clears the debt.
    d.loaded_ok(Loaded::default());
    assert!(!d.needs_reload());
}

/// The app-level journey: opening, the second Enter, and a row that
/// leaves the list under an open confirmation.
mod through_the_app {
    use super::*;
    use cyclops_proto::{
        Kind, MailboxEntryState, MessageDirection, MessageNotificationState,
        MessageNotificationSummary, MessageRecipientSummary, MessageSnapshotRow,
        MessagesSnapshotCounts, MessagesSnapshotResult,
    };
    use cyclops_ui::{build, ActionRequest, App, Filter, Key, Theme, View};

    pub fn workspace() -> WorkspaceId {
        WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap()
    }

    pub fn snapshot(seq: u64, rows: Vec<MessageSnapshotRow>) -> MessagesSnapshotResult {
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
        }
    }

    pub fn wire_row_n(id: &str, alarmed: bool, n: u64) -> MessageSnapshotRow {
        let notification = MessageNotificationSummary {
            state: if alarmed {
                MessageNotificationState::AttentionRequired
            } else {
                MessageNotificationState::Notified
            },
            wake_block: None,
            quota_state: None,
            settlement: None,
            operator_withdrawn: None,
            attempt_id: alarmed.then(|| attempt(n)),
            cause: alarmed.then_some(NotificationAttentionCause::VerifyFailed),
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            attention_cleared: alarmed.then_some(false),
            resolution: None,
            resolution_intent: None,
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            updated_at: Some(5_000),
        };
        // Two different callers, because the daemon addresses two
        // different actions. An alarm is admin work on a message the
        // admin sent, so that row is Outbound and its mailbox is not the
        // caller's to claim. An unalarmed row is the recipient's own
        // pending mail, which is a claim and never an attention target.
        // Building both the same way would model a row the daemon does
        // not produce.
        let direction = if alarmed {
            MessageDirection::Outbound
        } else {
            MessageDirection::Inbound
        };
        let mailbox = if alarmed {
            MailboxEntryState::Claimed {
                claimant: agent("%1"),
                claimed_at: 2_000,
            }
        } else {
            MailboxEntryState::Pending
        };
        MessageSnapshotRow {
            message_id: MessageId::new(id).unwrap(),
            seq: 1,
            ts: 1000,
            kind: Kind::Msg,
            direction,
            sender: RecipientKey::admin(workspace()),
            sender_label: "admin".into(),
            recipients: vec![MessageRecipientSummary {
                recipient: agent("%1"),
                label: "reviewer".into(),
                direction,
                needs_action: true,
                // Set from the wire, exactly as the daemon answers it.
                can_manage_attention: alarmed,
                can_withdraw_notification: false,
                current_route: None,
                available: true,
                mailbox,
                fifo_position: Some(1),
                notification,
            }],
            subject: Some("a subject".into()),
            reply_to: None,
            thread_root: MessageId::new(id).unwrap(),
            thread_message_count: 1,
            active: true,
            needs_action: true,
        }
    }

    /// An alarm whose terminal accepted its action but has no final outcome.
    /// `wake_word` reads it as `ResolutionIncomplete`, while fresh-action authority
    /// remains false and only matching no-key reconciliation is available.
    pub fn wire_row_uncertain(id: &str) -> MessageSnapshotRow {
        let mut row = wire_row_n(id, true, 7);
        row.recipients[0].notification.resolution_intent =
            Some(cyclops_proto::NotificationResolution::Complete);
        row.recipients[0].notification.resolution_action_accepted =
            Some(cyclops_proto::NotificationResolution::Complete);
        row.recipients[0]
            .notification
            .resolution_consumption_observed = Some(
            cyclops_proto::NotificationResolutionConsumptionObservation {
                evidence: cyclops_proto::NotificationResolutionConsumptionEvidence::WorkingEdge,
                observed_at_ms: 5_001,
            },
        );
        row.recipients[0].can_manage_attention = false;
        row
    }

    /// The same row after its own claim landed: identical target, moved
    /// mailbox word. That is what arms a reload, as opposed to a target
    /// change, which makes the detail stale instead.
    pub fn wire_row_claimed(id: &str) -> MessageSnapshotRow {
        let mut row = wire_row_n(id, false, 7);
        row.recipients[0].mailbox = MailboxEntryState::Claimed {
            claimant: agent("%1"),
            claimed_at: 2_000,
        };
        row
    }

    /// One row through an alarm's whole life, with everything BUT the
    /// alarm held still: same direction, same mailbox, same recipient.
    /// Only the notification moves, which is the point.
    pub fn wire_lifecycle(id: &str, attempt_n: Option<u64>, cleared: bool) -> MessageSnapshotRow {
        let mut row = wire_row_n(id, true, attempt_n.unwrap_or(7));
        let n = &mut row.recipients[0];
        match attempt_n {
            Some(k) => {
                n.notification.state = MessageNotificationState::AttentionRequired;
                n.notification.attempt_id = Some(attempt(k));
                n.notification.attention_cleared = Some(cleared);
                n.can_manage_attention = !cleared;
            }
            None => {
                n.notification.state = MessageNotificationState::Notified;
                n.notification.attempt_id = None;
                n.notification.attention_cleared = None;
                n.can_manage_attention = false;
            }
        }
        row
    }

    pub fn wire_row(id: &str, alarmed: bool) -> MessageSnapshotRow {
        wire_row_n(id, alarmed, 7)
    }

    pub fn app_with(rows: Vec<MessageSnapshotRow>, seq: u64) -> App {
        let mut app = App::new(Theme::none(), View::Messages, Filter::default());
        // An App starts Connecting and refuses mutations, which is the
        // point. These tests are about what the surface does with a live
        // daemon, so they acknowledge one first; the tests that are about
        // the connection itself do not.
        app.refresh.connected();
        let request = app.wants_messages().expect("connect owes a snapshot");
        assert!(app.apply_messages_response(request, &snapshot(seq, rows)));
        app
    }

    /// Enter opens once. A second press while the read is in flight does
    /// not open a second detail or fire a second request.
    #[test]
    fn a_second_enter_does_not_open_a_second_detail() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.open_detail().expect("the first Enter opens");
        assert!(app.detail.is_some());
        assert_eq!(*app.detail.as_ref().unwrap().stage(), Stage::Opening);

        // The read it owes claims, because this row is the reader's own
        // and still pending.
        let (token, request) = app.take_detail_read().expect("a read is owed");
        assert!(matches!(
            request,
            ActionRequest::OpenMessage { claim: true, .. }
        ));
        assert!(token.mutates(), "a claiming read is not marked a mutation");

        // A second Enter while that read is out starts nothing.
        assert!(
            app.open_detail().is_none(),
            "a second Enter started a second read"
        );
        assert!(
            app.take_detail_read().is_none(),
            "a second read went out while one was in flight"
        );

        // And the detail still offers nothing until the read returns.
        assert!(app.detail.as_ref().unwrap().allowed().is_empty());
    }

    /// An inbound pending row claims on open. An alarm opens by attempt
    /// id, never by message id.
    #[test]
    fn opening_picks_the_request_the_row_authorizes() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.open_detail().expect("opens");
        let (token, request) = app.take_detail_read().expect("a read is owed");
        match request {
            ActionRequest::OpenAttention { attempt_id } => assert_eq!(attempt_id, attempt(7)),
            other => panic!("an alarm opened as {other:?}"),
        }
        // Reading an alarm mutates nothing.
        assert!(!token.mutates());
    }

    /// A snapshot arriving under an open confirmation marks it stale and
    /// changes nothing else. The frozen target does not move and the
    /// confirmation stays on screen naming what the operator read.
    #[test]
    fn a_snapshot_never_retargets_an_open_confirmation() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.open_detail();
        let detail = app.detail.as_mut().unwrap();
        detail.loaded_ok(Loaded {
            checks: checks(true),
            ..Loaded::default()
        });
        detail.request(Action::AttentionDiscard);
        let frozen = detail.target().clone();

        // The row is gone, and a different row takes its place.
        app.apply_messages(&snapshot(10, vec![wire_row_n("m-999", true, 8)]));

        let detail = app.detail.as_ref().unwrap();
        assert_eq!(
            detail.target(),
            &frozen,
            "the frozen target followed the list"
        );
        assert_eq!(
            *detail.stage(),
            Stage::Confirming(Action::AttentionDiscard),
            "a snapshot cancelled the operator's confirmation"
        );
        assert!(detail.is_stale());
        assert!(
            detail.allowed().is_empty(),
            "a stale detail still offered actions"
        );

        // Saying yes now still names the frozen target, and the frame
        // says the row is gone.
        let text = build(&mut app, 80, 24).join("\n");
        assert!(text.contains("no longer listed"), "{text}");
    }

    /// Escape backs out of a confirmation, then out of the detail, and
    /// never leaves an action pending.
    #[test]
    fn escape_backs_out_without_mutating() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.open_detail();
        let detail = app.detail.as_mut().unwrap();
        detail.loaded_ok(Loaded {
            checks: checks(true),
            ..Loaded::default()
        });
        detail.request(Action::AttentionComplete);

        app.handle_key(Key::Esc);
        assert!(
            app.detail.is_some(),
            "escape closed the detail, not the confirmation"
        );
        assert_eq!(*app.detail.as_ref().unwrap().stage(), Stage::Open);
        assert!(!app.has_pending(), "escape queued an action");

        app.handle_key(Key::Esc);
        assert!(app.detail.is_none(), "escape did not close the detail");
        assert!(!app.has_pending());
    }

    /// A destructive action takes two deliberate keys, and the first one
    /// alone sends nothing.
    #[test]
    fn a_destructive_action_needs_the_number_and_then_the_yes() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.open_detail();
        let detail = app.detail.as_mut().unwrap();
        detail.loaded_ok(Loaded {
            checks: checks(true),
            ..Loaded::default()
        });
        let first = detail.allowed()[0];
        assert!(
            first.needs_confirmation(),
            "the fixture is not testing a confirm"
        );

        app.handle_key(Key::Char('1'));
        assert!(
            !app.has_pending(),
            "a numbered destructive action fired without a confirmation"
        );
        assert_eq!(
            *app.detail.as_ref().unwrap().stage(),
            Stage::Confirming(first)
        );

        app.handle_key(Key::Char('y'));
        assert!(app.has_pending(), "the yes did not resolve a request");
    }

    /// While a detail is open it owns the keyboard, so a stream key
    /// cannot move the list underneath it.
    #[test]
    fn an_open_detail_owns_the_keyboard() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.open_detail();
        app.handle_key(Key::Tab);
        assert_eq!(app.view, View::Messages, "tab changed view under a detail");
        assert!(app.detail.is_some());
    }
}

/// The journey the event loop actually drives.
///
/// These go through App only, in the order run_tui uses: a key arrives,
/// the loop asks what to send, the IO answers, the loop applies it. No
/// Detail method is called directly, because the defect being guarded
/// against was a model that worked while nothing called it.
mod loop_journey {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::{build, ActionOutcome, ActionRequest, App, Key};

    /// Stand in for the action task: take whatever the loop wanted to
    /// send, answer it, and hand the answer back under its own token,
    /// exactly as the real task does.
    fn pump(app: &mut App, answer: impl FnOnce(&ActionRequest) -> ActionOutcome) -> ActionRequest {
        let (token, request) = app
            .take_pending()
            .or_else(|| app.take_detail_read())
            .expect("the loop had nothing to send");
        let outcome = answer(&request);
        app.apply_action(token, outcome);
        request
    }

    /// Enter opens, the read lands, an action is chosen and confirmed,
    /// and the answer reaches the detail. End to end through App.
    #[test]
    fn enter_opens_and_a_confirmed_action_reaches_the_daemon() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);

        // Enter in Messages opens the row rather than jumping a pane.
        app.handle_key(Key::Enter);
        assert!(app.detail.is_some(), "Enter did not open the detail");
        assert!(app.detail_read_owed(), "the loop was owed no read");

        let opened = pump(&mut app, |request| {
            assert!(
                matches!(request, ActionRequest::OpenAttention { .. }),
                "an alarm opened as {request:?}"
            );
            ActionOutcome::Opened(Box::new(Loaded {
                checks: checks(true),
                ..Loaded::default()
            }))
        });
        assert!(matches!(opened, ActionRequest::OpenAttention { .. }));
        assert!(!app.detail_read_owed(), "the read was not marked done");

        // Choose the first action, confirm it, and the loop sends it.
        app.handle_key(Key::Char('1'));
        assert!(!app.has_pending(), "a number alone sent something");
        app.handle_key(Key::Char('y'));
        assert!(app.has_pending());

        let sent = pump(&mut app, |request| {
            assert!(matches!(request, ActionRequest::AttentionComplete { .. }));
            ActionOutcome::Done("notification submitted".into())
        });
        assert!(matches!(sent, ActionRequest::AttentionComplete { .. }));

        // The attempt is resolved, so the terminal pair is gone, and the
        // list is owed a fresh read because the daemon moved.
        let detail = app.detail.as_ref().unwrap();
        assert!(detail.is_resolved());
        assert_eq!(detail.allowed(), vec![Action::ClearAlarm]);
    }

    /// A reply is typed, sent, fails, and is sent again under the same
    /// key without retyping.
    #[test]
    fn a_reply_is_typed_sent_and_retried_under_one_key() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.handle_key(Key::Enter);
        pump(&mut app, |_| {
            ActionOutcome::Opened(Box::new(Loaded {
                body: Some("the payload".into()),
                ..Loaded::default()
            }))
        });

        // The only action is reply, and choosing it opens the composer
        // rather than sending an empty body.
        assert_eq!(app.detail.as_ref().unwrap().allowed(), vec![Action::Reply]);
        app.handle_key(Key::Char('1'));
        assert!(app.detail.as_ref().unwrap().is_composing());
        assert!(!app.has_pending(), "reply sent before it was written");

        for ch in "on it".chars() {
            app.handle_key(Key::Char(ch));
        }
        app.handle_key(Key::Backspace);
        assert_eq!(app.detail.as_ref().unwrap().draft().text(), "on i");
        // A digit is a character while composing, not an action.
        app.handle_key(Key::Char('1'));
        assert_eq!(app.detail.as_ref().unwrap().draft().text(), "on i1");
        // The draft is on screen.
        assert!(build(&mut app, 96, 24).join("\n").contains("on i1"));

        // Enter is a newline now, not a send. Nothing leaves on it.
        app.handle_key(Key::Enter);
        assert_eq!(app.detail.as_ref().unwrap().draft().text(), "on i1\n");
        assert!(!app.has_pending(), "enter sent a half-written reply");
        app.handle_key(Key::Backspace);

        app.handle_key(Key::CtrlD);
        assert!(
            app.has_pending(),
            "the reply was not resolved into a request"
        );

        let first_key = std::cell::RefCell::new(String::new());
        pump(&mut app, |request| {
            match request {
                ActionRequest::Reply {
                    body, client_key, ..
                } => {
                    assert_eq!(body, "on i1");
                    *first_key.borrow_mut() = client_key.clone();
                }
                other => panic!("reply sent as {other:?}"),
            }
            ActionOutcome::Uncertain("no answer".into())
        });

        // The draft survived and reply is still available, because a
        // reply carries a key and can go again.
        let detail = app.detail.as_ref().unwrap();
        assert_eq!(detail.draft().text(), "on i1");
        assert!(
            detail.allows(Action::Reply),
            "an uncertain reply blocked reply"
        );

        // Sending the same bytes again uses the same key.
        app.handle_key(Key::Char('1'));
        app.handle_key(Key::CtrlD);
        pump(&mut app, |request| {
            match request {
                ActionRequest::Reply { client_key, .. } => assert_eq!(
                    client_key,
                    &*first_key.borrow(),
                    "the retry took a second idempotency key"
                ),
                other => panic!("{other:?}"),
            }
            ActionOutcome::Done("replied as m-002".into())
        });
    }

    /// The footer numbers what the keyboard accepts.
    #[test]
    fn the_footer_numbers_the_actions_the_keys_take() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.handle_key(Key::Enter);
        pump(&mut app, |_| {
            ActionOutcome::Opened(Box::new(Loaded {
                checks: checks(true),
                ..Loaded::default()
            }))
        });

        let text = build(&mut app, 160, 40).join("\n");
        for (n, word) in [
            (1, "submit staged notification"),
            (2, "discard staged notification"),
            (3, "clear alarm"),
        ] {
            assert!(
                text.contains(&format!("{n} {word}")),
                "action {n} is not numbered on screen: {text}"
            );
        }
    }

    /// Scrolling is bound to the keys a reader will try.
    #[test]
    fn a_long_body_scrolls_from_the_keyboard() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.handle_key(Key::Enter);
        let body = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        pump(&mut app, |_| {
            ActionOutcome::Opened(Box::new(Loaded {
                body: Some(body.clone()),
                ..Loaded::default()
            }))
        });

        let first = build(&mut app, 80, 24).join("\n");
        for key in [Key::Down, Key::Char('j'), Key::WheelDown] {
            let before = app.detail.as_ref().unwrap().scroll();
            app.handle_key(key);
            assert!(
                app.detail.as_ref().unwrap().scroll() > before,
                "{key:?} did not scroll down"
            );
        }
        for key in [Key::Up, Key::Char('k'), Key::WheelUp] {
            let before = app.detail.as_ref().unwrap().scroll();
            app.handle_key(key);
            assert!(
                app.detail.as_ref().unwrap().scroll() < before,
                "{key:?} did not scroll up"
            );
        }

        for _ in 0..60 {
            app.handle_key(Key::Down);
        }
        let later = build(&mut app, 80, 24).join("\n");
        assert_ne!(first, later, "scrolling changed nothing on screen");
        assert!(first.contains("line 0"), "the first frame was not the top");
        assert!(
            !later.contains("line 0"),
            "the top is still on screen: {later}"
        );
        assert!(later.contains("line 60"), "the body did not move: {later}");
    }
}

/// A response may only touch the detail that asked for it.
///
/// These are the races the token exists for. Without it a body fetched
/// for one row renders in another row's frame, and an intent approved
/// against one target is sent against whatever is open when the loop
/// drains. Both are reachable with ordinary keystrokes, not tight
/// timing: open, escape, open again is enough.
mod token_matching {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::{ActionOutcome, ActionRequest, Key};

    fn body(text: &str) -> ActionOutcome {
        ActionOutcome::Opened(Box::new(Loaded {
            body: Some(text.into()),
            ..Loaded::default()
        }))
    }

    fn alarm_checks() -> ActionOutcome {
        ActionOutcome::Opened(Box::new(Loaded {
            checks: checks(true),
            ..Loaded::default()
        }))
    }

    /// A read answered after the reader moved to another row is dropped
    /// whole. The second detail never sees the first message's body.
    #[test]
    fn a_stale_response_never_reaches_the_detail_that_replaced_it() {
        let mut app = app_with(vec![wire_row("m-001", false), wire_row("m-002", false)], 9);

        app.open_detail().expect("A opens");
        let a_target = app.detail.as_ref().unwrap().target().target.clone();
        let (a_token, _) = app.take_detail_read().expect("A owes a read");

        // Leave A and open B before A answers.
        app.handle_key(Key::Esc);
        assert!(app.detail.is_none());
        app.queue.select_next();
        app.open_detail().expect("B opens");
        let b_target = app.detail.as_ref().unwrap().target().target.clone();
        assert_ne!(a_target, b_target, "the fixture opened the same row twice");

        // A's answer arrives now, carrying A's body.
        app.apply_action(a_token, body("SECRET FROM A"));

        let b = app.detail.as_ref().expect("B is still open");
        assert!(
            b.loaded().body.is_none(),
            "A's body landed in B's detail: {:?}",
            b.loaded().body
        );
        assert_eq!(*b.stage(), Stage::Opening, "a stale answer moved B's stage");
        assert_eq!(b.target().target, b_target, "B was retargeted");
        assert!(
            app.detail_read_owed(),
            "the stale answer satisfied B's read"
        );
    }

    /// A second answer to the same request changes nothing.
    #[test]
    fn a_duplicate_response_is_ignored() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.open_detail().expect("opens");
        let (token, _) = app.take_detail_read().expect("owes a read");

        app.apply_action(token.clone(), body("the payload"));
        assert_eq!(
            app.detail.as_ref().unwrap().loaded().body.as_deref(),
            Some("the payload")
        );

        app.apply_action(token, body("A LATER DIFFERENT BODY"));
        assert_eq!(
            app.detail.as_ref().unwrap().loaded().body.as_deref(),
            Some("the payload"),
            "a duplicate response overwrote the detail"
        );
    }

    /// An intent approved on one row is sent against that row.
    #[test]
    fn a_confirmed_action_keeps_the_target_it_was_confirmed_against() {
        let mut app = app_with(vec![wire_row("m-001", true), wire_row("m-002", true)], 9);
        app.open_detail().expect("A opens");
        let (token, _) = app.take_detail_read().expect("owes a read");
        let a_target = app.detail.as_ref().unwrap().target().target.clone();
        app.apply_action(token, alarm_checks());

        app.handle_key(Key::Char('2'));
        assert_eq!(
            *app.detail.as_ref().unwrap().stage(),
            Stage::Confirming(Action::AttentionDiscard)
        );
        app.handle_key(Key::Char('y'));
        assert!(app.has_pending(), "the yes resolved nothing");

        // Already built. What goes out names A whatever happens next.
        let (sent, request) = app.take_pending().expect("a request is waiting");
        assert_eq!(*sent.row(), a_target, "the request left A's target");
        match request {
            ActionRequest::AttentionDiscard { attempt_id } => {
                assert_eq!(attempt_id, attempt(7), "the verb named the wrong attempt")
            }
            other => panic!("discard was built as {other:?}"),
        }
    }

    /// While a request is out, keys that could start another are refused.
    /// Scrolling stays live because it cannot retarget anything.
    #[test]
    fn input_locks_while_a_request_is_in_flight() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.open_detail().expect("opens");
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(token, alarm_checks());

        app.handle_key(Key::Char('3'));
        app.handle_key(Key::Char('y'));
        let (in_flight, _) = app.take_pending().expect("a request is waiting");
        assert!(app.in_flight().is_some());

        app.handle_key(Key::Char('1'));
        app.handle_key(Key::Char('y'));
        assert!(
            !app.has_pending(),
            "a second action was queued while one was in flight"
        );
        assert!(app.take_pending().is_none());
        assert!(app.take_detail_read().is_none());

        let before = app.detail.as_ref().unwrap().scroll();
        app.handle_key(Key::Down);
        assert!(
            app.detail.as_ref().unwrap().scroll() > before,
            "scrolling was locked"
        );

        app.apply_action(in_flight, ActionOutcome::Done("alarm cleared".into()));
        assert!(app.in_flight().is_none());
    }

    /// Refused, not-sent and uncertain each reach the detail through the
    /// token and leave it in the state that outcome means.
    #[test]
    fn every_outcome_lands_through_its_token() {
        let armed = || {
            let mut app = app_with(vec![wire_row("m-001", true)], 9);
            app.open_detail().expect("opens");
            let (token, _) = app.take_detail_read().expect("owes a read");
            app.apply_action(token, alarm_checks());
            app.handle_key(Key::Char('1'));
            app.handle_key(Key::Char('y'));
            app
        };

        let mut app = armed();
        let (token, _) = app.take_pending().expect("waiting");
        app.apply_action(
            token,
            ActionOutcome::Refused {
                code: "conflict".into(),
                message: "already resolved".into(),
            },
        );
        let d = app.detail.as_ref().unwrap();
        assert!(d.is_resolved());
        assert_eq!(d.allowed(), vec![Action::ClearAlarm]);

        let mut app = armed();
        let (token, _) = app.take_pending().expect("waiting");
        app.apply_action(token, ActionOutcome::NotSent("connect refused".into()));
        let d = app.detail.as_ref().unwrap();
        assert!(matches!(d.stage(), Stage::NotSent { .. }));
        assert!(
            d.allows(Action::AttentionComplete),
            "not-sent withheld a verb the daemon never saw"
        );

        let mut app = armed();
        let (token, _) = app.take_pending().expect("waiting");
        app.apply_action(token, ActionOutcome::Uncertain("no answer".into()));
        let d = app.detail.as_ref().unwrap();
        assert!(matches!(d.stage(), Stage::Uncertain { .. }));
        assert_eq!(d.allowed(), vec![Action::ClearAlarm]);
    }
}

/// The target half of the token check, proven directly.
///
/// The whole token is compared first, and that comparison already covers
/// the target, so the two guards cannot be told apart by handing back a
/// different token. They differ only when the token still matches and
/// the DETAIL has changed underneath it. No key reaches that today:
/// leaving a detail forgets its request. The guard is defence in depth
/// against a future path that keeps one alive across a change of detail,
/// and unreachable code is the code that rots, so the state is built
/// directly here rather than left unexercised.
#[test]
fn a_live_token_whose_detail_changed_underneath_is_still_dropped() {
    use cyclops_ui::{ActionOutcome, Detail};
    use through_the_app::*;

    let mut app = app_with(vec![wire_row("m-001", false), wire_row("m-002", false)], 9);
    app.open_detail().expect("A opens");
    let (token, _) = app.take_detail_read().expect("A owes a read");

    // The request is still live and still wanted. The detail is now a
    // different row: what a future retargeting path would produce.
    app.queue.select_next();
    let other = app.queue.selected().expect("a second row").clone();
    app.detail = Some(Detail::open(&other, app.queue.watermark()));
    assert_ne!(
        app.detail.as_ref().unwrap().target().target,
        *token.row(),
        "the fixture did not actually change the detail"
    );

    app.apply_action(
        token,
        ActionOutcome::Opened(Box::new(Loaded {
            body: Some("SECRET FOR THE OTHER ROW".into()),
            ..Loaded::default()
        })),
    );

    let d = app.detail.as_ref().expect("still open");
    assert!(
        d.loaded().body.is_none(),
        "a body for another target reached this detail: {:?}",
        d.loaded().body
    );
    assert_eq!(
        *d.stage(),
        Stage::Opening,
        "the wrong target moved the stage"
    );
}

/// Disposition comes from the snapshot, never from `attention.show`.
///
/// The daemon's `AttentionShowResult` carries evidence and not outcome, so a
/// detail that asked only the daemon would offer both terminal verbs on an
/// attempt somebody else already finished. The daemon refuses, but offering
/// an action that cannot succeed is the defect being tested.
mod seeded_disposition {
    use super::*;

    #[test]
    fn an_attempt_another_operator_submitted_offers_no_terminal_verb() {
        let d = Detail::open(
            &alarm_row(
                attempt(7),
                Direction::Outbound,
                MailboxWord::Claimed,
                WakeWord::ResolvedSubmitted,
            ),
            9,
        );
        assert!(d.is_resolved(), "the wake word said it was resolved");
    }

    #[test]
    fn an_attempt_another_operator_discarded_offers_no_terminal_verb() {
        let d = Detail::open(
            &alarm_row(
                attempt(7),
                Direction::Outbound,
                MailboxWord::Claimed,
                WakeWord::ResolvedDiscarded,
            ),
            9,
        );
        assert!(d.is_resolved(), "discard is terminal too");
    }

    /// Every check passing is what makes this worth testing: the gate on
    /// the terminal verbs is checks-pass AND not-resolved, so a resolved
    /// attempt with clean evidence is exactly where a missing seed shows.
    #[test]
    fn a_resolved_attempt_with_passing_checks_still_offers_only_clear() {
        let mut d = Detail::open(
            &alarm_row(
                attempt(7),
                Direction::Outbound,
                MailboxWord::Claimed,
                WakeWord::ResolvedSubmitted,
            ),
            9,
        );
        d.loaded_ok(Loaded {
            checks: vec![Check {
                name: "notification exact".into(),
                passed: true,
                detail: None,
            }],
            ..Loaded::default()
        });
        let allowed = d.allowed();
        assert!(
            !allowed.contains(&Action::AttentionComplete)
                && !allowed.contains(&Action::AttentionDiscard),
            "a resolved attempt re-offered a terminal verb: {allowed:?}"
        );
        assert!(
            allowed.contains(&Action::ClearAlarm),
            "clearing the alarm is still safe: {allowed:?}"
        );
    }

    /// A matching durable terminal acceptance exposes only no-key reconciliation.
    #[test]
    fn an_accepted_uncertain_attempt_offers_only_its_matching_reconciliation() {
        for (intent, matching, opposite, phrase) in [
            (
                NotificationResolution::Complete,
                Action::AttentionComplete,
                Action::AttentionDiscard,
                "reconcile prior uncertain submit",
            ),
            (
                NotificationResolution::Discard,
                Action::AttentionDiscard,
                Action::AttentionComplete,
                "reconcile prior uncertain discard",
            ),
        ] {
            let mut row = alarm_row(
                attempt(7),
                Direction::Outbound,
                MailboxWord::Claimed,
                WakeWord::ResolutionIncomplete,
            );
            row.can_manage_attention = false;
            row.resolution_intent = Some(intent);
            row.resolution_action_accepted = Some(intent);
            if intent == NotificationResolution::Complete {
                row.resolution_consumption_observed = Some(
                    cyclops_proto::NotificationResolutionConsumptionObservation {
                        evidence: cyclops_proto::NotificationResolutionConsumptionEvidence::AuthenticatedClaim,
                        observed_at_ms: 2_000,
                    },
                );
            }
            let mut d = Detail::open(&row, 9);
            d.loaded_ok(Loaded::default());
            assert_eq!(d.allowed(), vec![matching]);
            assert!(!d.allows(opposite));
            let Request::Confirm(copy) = d.request(matching) else {
                panic!("matching reconciliation did not ask for confirmation")
            };
            assert!(copy.contains(phrase), "{copy}");
            assert!(copy.contains("no second key will be sent"), "{copy}");
        }
    }

    /// A Complete pre-key intent is a durable uncertainty marker, not proof
    /// that a key reached the terminal. It cannot expose another key or
    /// no-key settlement.
    #[test]
    fn intent_without_terminal_acceptance_offers_no_terminal_action() {
        let mut row = alarm_row(
            attempt(7),
            Direction::Outbound,
            MailboxWord::Claimed,
            WakeWord::ResolutionIncomplete,
        );
        row.can_manage_attention = false;
        row.resolution_intent = Some(NotificationResolution::Complete);
        let mut d = Detail::open(&row, 9);
        d.loaded_ok(Loaded::default());

        assert!(d.allowed().is_empty());
        assert!(!d.allows(Action::AttentionComplete));
        assert!(!d.allows(Action::AttentionDiscard));

        let frame = render(&d, 96, 24).join("\n");
        assert!(frame.contains("terminal acceptance is unproven"), "{frame}");
        assert!(
            frame.contains("no submit, discard, or reconciliation action is available"),
            "{frame}"
        );

        let frozen = d.target().clone();
        row.resolution_action_accepted = Some(NotificationResolution::Complete);
        d.observe_snapshot(Some(&row));
        assert_eq!(d.target(), &frozen, "snapshot change retargeted the detail");
        d.loaded_ok(Loaded::default());
        assert!(d.allowed().is_empty());
        let frame = render(&d, 96, 24).join("\n");
        assert!(
            frame.contains("terminal accepted, task start unproven"),
            "{frame}"
        );

        row.resolution_consumption_observed = Some(
            cyclops_proto::NotificationResolutionConsumptionObservation {
                evidence:
                    cyclops_proto::NotificationResolutionConsumptionEvidence::AuthenticatedClaim,
                observed_at_ms: 2_000,
            },
        );
        d.observe_snapshot(Some(&row));
        assert_eq!(
            d.target(),
            &frozen,
            "consumption proof retargeted the detail"
        );
        d.loaded_ok(Loaded::default());
        assert_eq!(d.allowed(), vec![Action::AttentionComplete]);

        row.resolution_action_accepted = Some(NotificationResolution::Discard);
        d.observe_snapshot(Some(&row));
        d.loaded_ok(Loaded::default());
        assert!(d.allowed().is_empty());
        let frame = render(&d, 96, 24).join("\n");
        assert!(
            frame.contains("terminal intent and accepted action records disagree"),
            "{frame}"
        );
    }

    #[test]
    fn intent_only_discard_offers_exact_empty_no_key_reconciliation() {
        let mut row = alarm_row(
            attempt(8),
            Direction::Outbound,
            MailboxWord::Claimed,
            WakeWord::ResolutionIncomplete,
        );
        row.can_manage_attention = false;
        row.resolution_intent = Some(NotificationResolution::Discard);
        let mut detail = Detail::open(&row, 11);
        detail.loaded_ok(Loaded::default());

        assert_eq!(detail.allowed(), vec![Action::AttentionDiscard]);
        assert_eq!(
            detail.action_word(Action::AttentionDiscard),
            "reconcile exact-empty discard without a key"
        );
        let Request::Confirm(copy) = detail.request(Action::AttentionDiscard) else {
            panic!("intent-only Discard did not expose its no-key recovery")
        };
        assert!(copy.contains("no second key will be sent"), "{copy}");

        assert_eq!(detail.escape(), cyclops_ui::Back::Cancelled);
        let frame = render(&detail, 96, 24).join("\n");
        assert!(
            frame.contains("exact-empty no-key discard reconciliation is available"),
            "{frame}"
        );
        assert!(frame.contains("no terminal key is sent"), "{frame}");
    }

    /// Tightening only. A snapshot that predates this operator's own
    /// terminal action must not hand the verbs back.
    #[test]
    fn an_older_snapshot_cannot_reopen_a_resolved_attempt() {
        let mut d = Detail::open(
            &alarm_row(
                attempt(7),
                Direction::Outbound,
                MailboxWord::Claimed,
                WakeWord::NeedsAttention,
            ),
            9,
        );
        d.done(Action::AttentionComplete, "notification submitted");
        assert!(d.is_resolved(), "this operator just resolved it");
        d.observe_snapshot(Some(&alarm_row(
            attempt(7),
            Direction::Outbound,
            MailboxWord::Claimed,
            WakeWord::NeedsAttention,
        )));
        assert!(
            d.is_resolved(),
            "a stale snapshot un-resolved a finished attempt"
        );
    }
}

/// The whole path: matching intent, terminal acceptance, and consumption
/// evidence open the exact attempt and send only its reconciliation RPC.
#[test]
fn an_uncertain_wire_row_sends_only_its_matching_reconciliation_rpc() {
    use through_the_app::*;

    let mut app = app_with(vec![wire_row_uncertain("m-001")], 9);
    let row = app.queue.selected().expect("a row");
    assert_eq!(
        row.wake,
        WakeWord::ResolutionIncomplete,
        "the wire fixture did not produce an uncertain wake"
    );
    assert!(
        row.attention.is_some(),
        "an uncertain alarm is still an attention target: {:?}",
        row.target
    );
    assert!(!row.can_manage_attention);
    assert_eq!(
        row.resolution_intent,
        Some(NotificationResolution::Complete)
    );
    assert_eq!(
        row.resolution_action_accepted,
        Some(NotificationResolution::Complete)
    );
    assert!(row.resolution_consumption_observed.is_some());

    app.open_detail().expect("it opens");
    let (token, request) = app.take_detail_read().expect("it owes a read");
    assert!(matches!(
        request,
        cyclops_ui::ActionRequest::OpenAttention { attempt_id }
            if attempt_id == attempt(7)
    ));
    app.apply_action(token, cyclops_ui::ActionOutcome::Opened(Box::default()));

    let allowed = app.detail.as_ref().expect("still open").allowed();
    assert_eq!(allowed, vec![Action::AttentionComplete]);
    app.handle_key(cyclops_ui::Key::Char('1'));
    let frame = cyclops_ui::build(&mut app, 96, 24).join("\n");
    assert!(
        frame.contains("terminal accepted the submit action"),
        "{frame}"
    );
    assert!(
        frame.contains("reconcile prior uncertain submit"),
        "{frame}"
    );
    let words = frame.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(words.contains("no second key will be sent"), "{frame}");
    app.handle_key(cyclops_ui::Key::Char('y'));
    let (_, request) = app.take_pending().expect("reconciliation request");
    assert!(matches!(
        request,
        cyclops_ui::ActionRequest::AttentionComplete { attempt_id }
            if attempt_id == attempt(7)
    ));
}

/// The diff is what an operator acts on. Five booleans name which rule
/// broke and never what actually differs.
mod attention_diff {
    use super::*;

    fn shown(expected: Option<&str>, observed: Option<&str>) -> String {
        let mut d = Detail::open(
            &alarm_row(
                attempt(7),
                Direction::Outbound,
                MailboxWord::Claimed,
                WakeWord::NeedsAttention,
            ),
            9,
        );
        d.loaded_ok(Loaded {
            checks: vec![Check {
                name: "notification exact".into(),
                passed: false,
                detail: None,
            }],
            expected: expected.map(str::to_string),
            observed: observed.map(str::to_string),
            ..Loaded::default()
        });
        render(&d, 80, 24).join("\n")
    }

    #[test]
    fn both_sides_are_shown_when_the_daemon_returned_them() {
        let text = shown(Some("STAGED PAYLOAD"), Some("WHAT THE PANE HELD"));
        assert!(text.contains("STAGED PAYLOAD"), "{text}");
        assert!(text.contains("WHAT THE PANE HELD"), "{text}");
    }

    /// Extraction failing is a finding, not a blank line. It is often the
    /// reason a check did not pass.
    #[test]
    fn a_pane_that_could_not_be_read_says_so() {
        let text = shown(Some("STAGED PAYLOAD"), None);
        assert!(
            text.contains("could not be read exactly"),
            "a failed extraction rendered as nothing: {text}"
        );
    }

    /// A message detail has no attention evidence, and asking for a diff
    /// on one would be asking the wrong question.
    #[test]
    fn a_message_detail_shows_no_diff_section() {
        let d = opened(
            &inbound_pending(),
            Loaded {
                body: Some("the payload".into()),
                ..Loaded::default()
            },
        );
        let text = render(&d, 80, 24).join("\n");
        assert!(!text.contains("in the pane"), "{text}");
    }

    /// Every width, because the diff is the longest thing on the surface
    /// and a narrow frame is where an overflow would show.
    #[test]
    fn the_diff_never_overflows_its_frame() {
        for (w, h) in SIZES {
            let mut d = Detail::open(
                &alarm_row(
                    attempt(7),
                    Direction::Outbound,
                    MailboxWord::Claimed,
                    WakeWord::NeedsAttention,
                ),
                9,
            );
            d.loaded_ok(Loaded {
                expected: Some(
                    "a staged line that is deliberately far wider than any of these frames".into(),
                ),
                observed: Some("and an observed line that is also much too wide to fit".into()),
                ..Loaded::default()
            });
            for line in render(&d, w, h) {
                assert!(
                    line.chars().count() == w,
                    "at {w}x{h} a line was {} wide: {line:?}",
                    line.chars().count()
                );
            }
        }
    }
}

/// Pasted bytes are text, never commands.
mod pasting {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::{Command, Key};

    /// Open a detail with a body and the composer already running.
    fn composing() -> cyclops_ui::App {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.handle_key(Key::Enter);
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            cyclops_ui::ActionOutcome::Opened(Box::new(Loaded {
                body: Some("the payload".into()),
                ..Loaded::default()
            })),
        );
        app.handle_key(Key::Char('1'));
        assert!(app.detail.as_ref().unwrap().is_composing());
        app
    }

    /// The whole hostile paste: a second line starting with q, and digits
    /// that are actions when typed. Every byte must land in the draft.
    #[test]
    fn a_pasted_payload_never_quits_and_never_acts() {
        let mut app = composing();
        assert!(app.handle_key(Key::PasteStart).is_none());
        let mut quit = false;
        // Esc first, and it is the one that matters. While composing the
        // composer already swallows letters and digits, so a paste guard
        // that only handled those would be dead code. Esc is different:
        // uncaught it ends compose, and every byte after it in the same
        // paste is a command again, starting with q.
        for key in [
            Key::Char('f'),
            Key::Char('i'),
            Key::Char('x'),
            Key::Enter,
            Key::Esc,
            Key::Char('q'),
            Key::Char('1'),
            Key::Char('y'),
        ] {
            if app.handle_key(key) == Some(Command::Quit) {
                quit = true;
            }
        }
        app.handle_key(Key::PasteEnd);

        assert!(!quit, "a pasted q quit the UI");
        assert!(
            app.detail.as_ref().unwrap().is_composing(),
            "a pasted esc closed the composer, re-arming commands mid-paste"
        );
        assert!(!app.has_pending(), "a pasted key sent something");
        assert_eq!(
            app.detail.as_ref().unwrap().draft().text(),
            "fix\nq1y",
            "the paste did not land verbatim in the draft"
        );
        assert!(app.detail.is_some(), "a pasted esc closed the detail");
    }

    /// Nothing to type into means nothing happens. Silently running the
    /// bytes as commands is the failure being prevented.
    #[test]
    fn a_paste_outside_the_composer_does_nothing_at_all() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.handle_key(Key::Enter);
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            cyclops_ui::ActionOutcome::Opened(Box::new(Loaded {
                body: Some("the payload".into()),
                ..Loaded::default()
            })),
        );
        assert!(!app.detail.as_ref().unwrap().is_composing());

        app.handle_key(Key::PasteStart);
        let mut quit = false;
        for key in [Key::Char('q'), Key::Char('1'), Key::Char('y'), Key::Enter] {
            if app.handle_key(key) == Some(Command::Quit) {
                quit = true;
            }
        }
        app.handle_key(Key::PasteEnd);

        assert!(!quit, "a paste quit a detail that was not composing");
        assert!(!app.has_pending(), "a paste started an action");
        assert!(app.detail.is_some());
        assert!(
            !app.detail.as_ref().unwrap().is_composing(),
            "a paste opened the composer by itself"
        );
    }

    /// No key inside a paste ends it, acts, or quits. The decoder emits a
    /// paste only once it has the terminator, so quarantine is closed at
    /// the boundary rather than by trusting the payload's own bytes.
    #[test]
    fn no_pasted_byte_can_leave_quarantine_early() {
        let mut app = composing();
        app.handle_key(Key::PasteStart);
        let mut quit = false;
        // Esc then q is the exact hole a payload-triggered abort left.
        for key in [
            Key::Char('a'),
            Key::Esc,
            Key::Char('q'),
            Key::CtrlC,
            Key::Char('q'),
        ] {
            if app.handle_key(key) == Some(Command::Quit) {
                quit = true;
            }
        }
        assert!(!quit, "a pasted esc or ctrl-c let the next byte quit");
        assert!(app.detail.is_some(), "a pasted esc closed the detail");
        assert_eq!(
            app.detail.as_ref().unwrap().draft().text(),
            "aqq",
            "the payload did not land as text"
        );

        // The terminator, and only the terminator, ends it.
        app.handle_key(Key::PasteEnd);
        assert_eq!(app.handle_key(Key::CtrlC), Some(Command::Quit));
    }

    /// The guard has to sit above every route, not just the detail. A
    /// paste landing on the queue used to run its bytes as commands.
    #[test]
    fn a_paste_on_the_queue_never_reaches_command_handling() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        assert!(app.detail.is_none(), "no detail is open");

        app.handle_key(Key::PasteStart);
        let mut quit = false;
        for key in [
            Key::Char('q'),
            Key::Enter,
            Key::Char('1'),
            Key::Char('w'),
            Key::Esc,
        ] {
            if app.handle_key(key) == Some(Command::Quit) {
                quit = true;
            }
        }
        app.handle_key(Key::PasteEnd);

        assert!(!quit, "a pasted q quit from the queue");
        assert!(app.detail.is_none(), "a pasted enter opened a detail");
        assert!(app.input.is_none(), "a pasted w opened a filter input");
        assert!(!app.has_pending(), "a paste started an action");
    }
}

/// Findings from the adversarial port audit. Each of these was reachable
/// on the branch before the port.
mod audit {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::{ActionOutcome, Command, Key};

    fn open_message(app: &mut cyclops_ui::App) {
        app.handle_key(Key::Enter);
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                body: Some("the payload".into()),
                ..Loaded::default()
            })),
        );
    }

    /// A failure left needs_reload set, so the loop re-issued the same
    /// read every turn at connect-failure speed, redrawing the whole
    /// frame each time, until the operator pressed Esc.
    #[test]
    fn a_failed_reload_is_not_owed_again_forever() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        open_message(&mut app);

        // A snapshot moves the row's facts, so a reload is owed.
        app.apply_messages(&snapshot(10, vec![wire_row_claimed("m-001")]));
        let (token, _) = app.take_detail_read().expect("the reload is owed");
        app.apply_action(token, ActionOutcome::NotSent("no socket".into()));

        assert!(
            app.take_detail_read().is_none(),
            "the failed reload is still owed, so the loop will spin on it"
        );
    }

    /// Esc while a terminal verb is on the wire used to close the detail,
    /// which dropped the answer on its token: the detail never learned it
    /// had succeeded and the queue was never re-read, so the operator
    /// reopened and ran a non-idempotent verb a second time.
    #[test]
    fn escape_does_not_abandon_an_action_already_sent() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.handle_key(Key::Enter);
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );

        let allowed = app.detail.as_ref().unwrap().allowed();
        let index = allowed
            .iter()
            .position(|a| *a == Action::AttentionComplete)
            .expect("complete is offered");
        app.handle_key(Key::Char((b'1' + index as u8) as char));
        app.handle_key(Key::Char('y'));
        let (token, _) = app.take_pending().expect("the verb was sent");

        app.handle_key(Key::Esc);
        assert!(
            app.detail.is_some(),
            "esc closed a detail with a terminal verb in flight"
        );

        // The answer still lands, so the detail records that it happened.
        app.apply_action(token, ActionOutcome::Done("notification submitted".into()));
        assert!(
            app.detail.as_ref().unwrap().is_resolved(),
            "the answer was discarded and the attempt looks unresolved"
        );
    }

    /// Both guards used to sit above the composer, and both can arm with
    /// no keystroke. A `y` or a digit typed into a reply body was
    /// swallowed and the mangled text was sent.
    #[test]
    fn a_read_in_flight_does_not_eat_the_reply_being_typed() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        open_message(&mut app);
        app.handle_key(Key::Char('1'));
        assert!(app.detail.as_ref().unwrap().is_composing());

        // A background snapshot arms a reload, which puts a read in
        // flight without the operator touching anything.
        app.apply_messages(&snapshot(10, vec![wire_row_claimed("m-001")]));
        let _ = app.take_detail_read().expect("a reload went out");

        for ch in "y1 ok".chars() {
            app.handle_key(Key::Char(ch));
        }
        assert_eq!(
            app.detail.as_ref().unwrap().draft().text(),
            "y1 ok",
            "a guard meant for actions ate the reply body"
        );
    }

    /// A landed reply left its draft and its key in place. Sending the
    /// same bytes again reused the key, the daemon deduped it, and the
    /// operator was told a second message was sent.
    #[test]
    fn a_sent_reply_stops_being_a_draft() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        open_message(&mut app);
        app.handle_key(Key::Char('1'));
        for ch in "ack".chars() {
            app.handle_key(Key::Char(ch));
        }
        app.handle_key(Key::CtrlD);
        let (token, _) = app.take_pending().expect("the reply was sent");
        app.apply_action(token, ActionOutcome::Done("replied as m-002".into()));

        let detail = app.detail.as_ref().unwrap();
        assert_eq!(
            detail.draft().text(),
            "",
            "the sent text is still shown as an unsent draft"
        );
        assert!(
            detail.draft().key().is_none(),
            "the landed reply kept its idempotency key"
        );
    }

    /// Reply skipped request(), which is where staleness is enforced, so
    /// it was the one action a stale detail could still perform.
    #[test]
    fn a_stale_detail_cannot_open_the_composer() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        open_message(&mut app);

        // The row leaves the snapshot entirely.
        app.apply_messages(&snapshot(10, Vec::new()));
        assert!(app.detail.as_ref().unwrap().is_stale());
        assert!(app.detail.as_ref().unwrap().allowed().is_empty());

        app.handle_key(Key::Char('1'));
        assert!(
            !app.detail.as_ref().unwrap().is_composing(),
            "a stale detail opened its composer"
        );
        app.handle_key(Key::CtrlD);
        assert!(!app.has_pending(), "a stale detail sent a reply");
        assert_ne!(app.handle_key(Key::Char('j')), Some(Command::Quit));
    }
}

/// Untrusted text is written by other agents and by people. It reaches a
/// terminal, so it is an injection surface until it is sanitized.
mod control_injection {
    use super::*;
    use cyclops_ui::grid::safe_text;

    /// Everything hostile a body can carry, in one string.
    fn hostile() -> String {
        format!(
            "start{esc}[2J{esc}]52;c;cGFzdA=={bel}wiped\rOVER\ttab{c1}end{del}",
            esc = '\u{1b}',
            bel = '\u{7}',
            c1 = '\u{9b}',
            del = '\u{7f}',
        )
    }

    /// Not one escape survives, and nothing is silently deleted except
    /// the carriage return.
    #[test]
    fn no_escape_survives_sanitizing() {
        let out = safe_text(&hostile());
        assert!(!out.contains('\u{1b}'), "ESC survived: {out:?}");
        assert!(!out.contains('\u{9b}'), "8-bit CSI survived: {out:?}");
        assert!(!out.contains('\u{7}'), "BEL survived: {out:?}");
        assert!(!out.contains('\u{7f}'), "DEL survived: {out:?}");
        assert!(!out.contains('\r'), "CR survived: {out:?}");
        assert!(!out.contains('\t'), "tab survived: {out:?}");
        // The readable text is still readable.
        assert!(out.contains("start") && out.contains("wiped") && out.contains("end"));
    }

    /// A newline is the one control with a meaning here: wrap splits on
    /// it. Losing it would run every paragraph of a body together.
    #[test]
    fn a_newline_is_kept_and_other_scripts_are_untouched() {
        assert_eq!(safe_text("a\nb"), "a\nb");
        for text in ["héllo", "日本語", "🙂 ok", "Ελληνικά"] {
            assert_eq!(safe_text(text), text, "{text} was mangled");
        }
    }

    /// The frame's width arithmetic scores a control byte as one column,
    /// so an unsanitized body both attacks the terminal AND makes every
    /// width assertion lie. Every row must be exactly the frame width.
    #[test]
    fn a_hostile_body_never_breaks_the_frame() {
        for (w, h) in SIZES {
            let d = opened(
                &inbound_pending(),
                Loaded {
                    body: Some(safe_text(&hostile())),
                    claim_note: Some(safe_text(&hostile())),
                    ..Loaded::default()
                },
            );
            let frame = render(&d, w, h);
            assert_eq!(frame.len(), h, "wrong height at {w}x{h}");
            for line in &frame {
                assert_eq!(
                    line.chars().count(),
                    w,
                    "at {w}x{h} a row was {} wide: {line:?}",
                    line.chars().count()
                );
                assert!(!line.contains('\u{1b}'), "an escape reached the frame");
                assert!(!line.contains('\r'), "a CR reached the frame");
            }
        }
    }

    /// The attention diff is extracted from a pane, so its contents are
    /// whatever an agent printed. Same rule.
    #[test]
    fn a_hostile_attention_diff_never_breaks_the_frame() {
        for (w, h) in SIZES {
            let mut d = Detail::open(
                &alarm_row(
                    attempt(7),
                    Direction::Outbound,
                    MailboxWord::Claimed,
                    WakeWord::NeedsAttention,
                ),
                9,
            );
            d.loaded_ok(Loaded {
                expected: Some(safe_text(&hostile())),
                observed: Some(safe_text(&hostile())),
                ..Loaded::default()
            });
            for line in render(&d, w, h) {
                assert_eq!(line.chars().count(), w, "at {w}x{h}: {line:?}");
                assert!(!line.contains('\u{1b}'));
            }
        }
    }

    /// A subject rides the header, which is not wrapped, so it is the
    /// easiest row to overflow.
    #[test]
    fn a_hostile_subject_never_breaks_the_frame() {
        let mut r = inbound_pending();
        r.subject = Some(safe_text(&hostile()));
        r.recipient_label = safe_text(&hostile());
        let d = opened(&r, Loaded::default());
        for (w, h) in SIZES {
            for line in render(&d, w, h) {
                assert_eq!(line.chars().count(), w, "at {w}x{h}: {line:?}");
                assert!(!line.contains('\u{1b}'));
            }
        }
    }
}

/// A confirmation is only worth anything if the operator can read what
/// they are confirming.
mod too_small_to_confirm {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::Key;

    /// 24x6 passed the old gate. Two rows of body cannot show five checks
    /// plus a staged and an observed block, and 24 columns cut the
    /// confirmation before the attempt id it names.
    #[test]
    fn a_frame_that_cannot_show_the_evidence_refuses_to_act() {
        assert!(
            !cyclops_ui::detail::can_show_actions(24, 6),
            "a frame with two body rows still enables mutation"
        );
        assert!(cyclops_ui::detail::can_show_actions(80, 24));
    }

    /// The confirmation must name the whole attempt id, not a prefix of
    /// it, at every size where acting is allowed.
    #[test]
    fn the_confirmation_names_its_whole_target_wherever_acting_is_allowed() {
        let mut d = opened(
            &alarm_row(
                attempt(7),
                Direction::Outbound,
                MailboxWord::Claimed,
                WakeWord::NeedsAttention,
            ),
            Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            },
        );
        d.request(Action::AttentionDiscard);
        for (w, h) in [(48, 12), (80, 24), (160, 40)] {
            assert!(cyclops_ui::detail::can_show_actions(w, h));
            let text = render(&d, w, h).join("\n");
            assert!(
                text.contains(&attempt(7).to_string()),
                "at {w}x{h} the confirmation was cut before its target:\n{text}"
            );
        }
    }

    /// A display label comes from runtime identity and can be arbitrarily
    /// long. Confirmation rendering must stay bounded by frame height.
    #[test]
    fn a_long_recipient_label_cannot_wedge_a_small_confirmation() {
        let mut row = alarm_row(
            attempt(7),
            Direction::Outbound,
            MailboxWord::Claimed,
            WakeWord::NeedsAttention,
        );
        row.recipient_label = "x".repeat(1_000);
        let mut detail = opened(
            &row,
            Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            },
        );
        detail.request(Action::AttentionDiscard);

        let frame = render(&detail, 48, 12);
        assert_eq!(frame.len(), 12);
        assert!(
            frame.join("\n").contains(&attempt(7).to_string()),
            "the bounded footer lost the action target"
        );
    }

    /// The key handler and the renderer must agree, so a digit cannot act
    /// on a frame that drew no actions.
    #[test]
    fn a_digit_does_nothing_on_a_frame_too_small_to_review() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.handle_key(Key::Enter);
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            cyclops_ui::ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );
        cyclops_ui::build(&mut app, 24, 6);
        app.handle_key(Key::Char('1'));
        app.handle_key(Key::Char('y'));
        assert!(
            !app.has_pending(),
            "a frame too small to review still sent an action"
        );
    }

    /// A notice consumes one row. At the minimum action height, that
    /// makes the evidence too small and the digit must stop working.
    #[test]
    fn a_status_row_counts_against_the_evidence_height() {
        let mut app = app_with(vec![wire_row("m-001", true)], 9);
        app.handle_key(Key::Enter);
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            cyclops_ui::ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );
        app.notice = Some("a concurrent notice".into());
        let frame = cyclops_ui::build(&mut app, 48, 12).join("\n");
        assert!(frame.contains("a concurrent notice"), "{frame}");
        assert!(!frame.contains("1 submit staged notification"), "{frame}");

        app.handle_key(Key::Char('1'));
        assert_eq!(*app.detail.as_ref().unwrap().stage(), Stage::Open);
        assert!(!app.has_pending());
    }
}

/// An alarm appears, clears, is requeued, and has its attempt replaced.
/// Through all of it the row is the same row.
mod lifecycle {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::{ActionOutcome, Key};

    fn identity(app: &cyclops_ui::App) -> QueueTarget {
        app.queue.selected().expect("a row").target.clone()
    }

    /// The transition that used to rename the row out from under the
    /// cursor and the open detail.
    #[test]
    fn an_alarm_appearing_and_clearing_never_changes_the_row() {
        let mut app = app_with(vec![wire_lifecycle("m-001", None, false)], 9);
        let before = identity(&app);
        assert!(app.queue.selected().unwrap().attention.is_none());

        // The alarm appears.
        app.apply_messages(&snapshot(10, vec![wire_lifecycle("m-001", Some(7), false)]));
        assert_eq!(identity(&app), before, "an alarm renamed the row");
        assert_eq!(app.queue.selected().unwrap().attention, Some(attempt(7)));
        assert!(app.queue.selected().unwrap().can_manage_attention);

        // And is cleared.
        app.apply_messages(&snapshot(11, vec![wire_lifecycle("m-001", Some(7), true)]));
        assert_eq!(identity(&app), before, "clearing renamed the row");
        assert!(
            !app.queue.selected().unwrap().can_manage_attention,
            "a cleared alarm still offered recovery"
        );

        // Requeued under a NEW attempt.
        app.apply_messages(&snapshot(12, vec![wire_lifecycle("m-001", Some(8), false)]));
        assert_eq!(identity(&app), before, "a requeue renamed the row");
        assert_eq!(app.queue.selected().unwrap().attention, Some(attempt(8)));
    }

    /// The detail used to go stale the moment its own action succeeded,
    /// because the row it froze stopped existing under that name.
    #[test]
    fn an_open_detail_survives_its_own_alarm_clearing() {
        let mut app = app_with(vec![wire_lifecycle("m-001", Some(7), false)], 9);
        app.open_detail().expect("it opens");
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );
        assert!(!app.detail.as_ref().unwrap().is_stale());

        // The alarm this detail was opened against is cleared.
        app.apply_messages(&snapshot(10, vec![wire_lifecycle("m-001", Some(7), true)]));
        assert!(
            !app.detail.as_ref().unwrap().is_stale(),
            "a cleared alarm made its own detail stale"
        );
    }

    /// Identity is stable, so the attempt has to carry the difference.
    ///
    /// Built directly, like the row-half guard next to it. Leaving a
    /// detail forgets its request, so no keystroke reaches the state where
    /// the token still matches and the DETAIL's attempt has moved: the
    /// nonce check fires first and the attempt check never runs. It is
    /// defence in depth for a future path that keeps a request alive
    /// across a requeue, and unreachable code is the code that rots.
    #[test]
    fn a_live_token_whose_attempt_was_replaced_is_dropped() {
        use cyclops_ui::Detail;

        let mut app = app_with(vec![wire_lifecycle("m-001", Some(7), false)], 9);
        app.open_detail().expect("it opens");
        let (token, _) = app.take_detail_read().expect("owes a read");

        // Same row, requeued under a new attempt, with the request still
        // live: what a future retargeting path would produce.
        let requeued = {
            let mut q = cyclops_ui::HumanQueue::new();
            q.replace(cyclops_ui::rows_from_snapshot(&snapshot(
                10,
                vec![wire_lifecycle("m-001", Some(8), false)],
            )));
            q.selected().expect("a row").clone()
        };
        app.detail = Some(Detail::open(&requeued, 10));
        assert_eq!(
            app.detail.as_ref().unwrap().target().target,
            *token.row(),
            "the fixture changed the row, not just the attempt"
        );
        assert_ne!(
            app.detail.as_ref().unwrap().target().attempt,
            token.attempt(),
            "the fixture did not actually replace the attempt"
        );

        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                body: Some("EVIDENCE FOR THE REPLACED ATTEMPT".into()),
                ..Loaded::default()
            })),
        );
        assert!(
            app.detail.as_ref().unwrap().loaded().body.is_none(),
            "evidence for a replaced attempt reached its successor: {:?}",
            app.detail.as_ref().unwrap().loaded().body
        );
    }

    /// An answer about the replaced attempt must not land on a detail now
    /// looking at its successor.
    #[test]
    fn an_answer_about_a_replaced_attempt_is_dropped() {
        let mut app = app_with(vec![wire_lifecycle("m-001", Some(7), false)], 9);
        app.open_detail().expect("it opens");
        let (stale_token, _) = app.take_detail_read().expect("owes a read");

        // Requeued: same row, new attempt. The reader reopens against it.
        app.apply_messages(&snapshot(10, vec![wire_lifecycle("m-001", Some(8), false)]));
        app.handle_key(Key::Esc);
        app.open_detail().expect("it reopens");
        assert_eq!(
            app.detail.as_ref().unwrap().target().attempt,
            Some(attempt(8))
        );

        // The first read finally answers, naming attempt 7.
        app.apply_action(
            stale_token,
            ActionOutcome::Opened(Box::new(Loaded {
                body: Some("EVIDENCE FOR THE REPLACED ATTEMPT".into()),
                ..Loaded::default()
            })),
        );
        assert!(
            app.detail.as_ref().unwrap().loaded().body.is_none(),
            "evidence for a replaced attempt reached its successor"
        );
    }

    /// A confirmed verb names the attempt the operator was shown, not
    /// whichever one the row carries by the time they say yes.
    #[test]
    fn a_confirmed_verb_names_the_attempt_that_was_on_screen() {
        let mut app = app_with(vec![wire_lifecycle("m-001", Some(7), false)], 9);
        app.open_detail().expect("it opens");
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );
        let allowed = app.detail.as_ref().unwrap().allowed();
        let index = allowed
            .iter()
            .position(|a| *a == Action::AttentionComplete)
            .expect("complete is offered");
        app.handle_key(Key::Char((b'1' + index as u8) as char));

        // The row is requeued between the confirmation and the yes.
        app.apply_messages(&snapshot(10, vec![wire_lifecycle("m-001", Some(8), false)]));
        app.handle_key(Key::Char('y'));

        let (_, request) = app.take_pending().expect("the verb was sent");
        match request {
            cyclops_ui::ActionRequest::AttentionComplete { attempt_id } => assert_eq!(
                attempt_id,
                attempt(7),
                "the verb followed the row instead of the evidence"
            ),
            other => panic!("wrong request: {other:?}"),
        }
    }
}

/// Connection truth, generation invalidation, and the stale-evidence
/// latch. One state machine: what may be shown, and what may be sent.
mod connection {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::{ActionOutcome, Key, Link};

    fn alarmed_app() -> cyclops_ui::App {
        let mut app = app_with(vec![wire_lifecycle("m-001", Some(7), false)], 9);
        app.open_detail().expect("it opens");
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );
        app
    }

    /// A UI that starts life claiming to be connected shows an empty
    /// queue as fact rather than as a question.
    #[test]
    fn an_app_does_not_start_connected() {
        let app = cyclops_ui::App::new(
            cyclops_ui::Theme::none(),
            cyclops_ui::View::Messages,
            cyclops_ui::Filter::default(),
        );
        assert_eq!(app.refresh.link(), Link::Connecting);
        assert!(!app.refresh.may_mutate());
    }

    /// Startup is Connecting while an attempt runs. If that attempt
    /// fails, Lost must offer R instead of waiting forever.
    #[test]
    fn a_failed_start_becomes_recoverable() {
        let mut gate = cyclops_ui::RefreshGate::new();
        assert_eq!(gate.link(), Link::Connecting);
        gate.disconnected();
        assert_eq!(gate.link(), Link::Lost);
        assert!(gate.reconnecting());
        assert_eq!(gate.link(), Link::Connecting);
        gate.connected();
        gate.disconnected();
        assert_eq!(gate.link(), Link::Lost);
        assert!(!gate.may_mutate());
    }

    /// A non-idempotent verb must not be written into a socket that is
    /// not acknowledged.
    #[test]
    fn a_lost_connection_sends_nothing() {
        let mut app = alarmed_app();
        let allowed = app.detail.as_ref().unwrap().allowed();
        let index = allowed
            .iter()
            .position(|a| *a == Action::AttentionComplete)
            .expect("complete is offered");

        app.refresh.disconnected();
        app.handle_key(Key::Char((b'1' + index as u8) as char));
        app.handle_key(Key::Char('y'));
        assert!(
            !app.has_pending(),
            "a terminal verb was sent while disconnected"
        );
    }

    /// A daemon restart used to freeze Messages for the process lifetime:
    /// the subscription ran once and then reported lost forever. Asking
    /// again is a keystroke, never a timer.
    #[test]
    fn a_lost_connection_can_be_asked_again_by_hand() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.refresh.disconnected();
        assert_eq!(app.refresh.link(), Link::Lost);

        assert_eq!(
            app.handle_key(Key::Char('R')),
            Some(cyclops_ui::Command::Reconnect)
        );
        assert!(
            app.reconnect_owed,
            "the loop was never told to open a subscription"
        );
        assert_eq!(app.refresh.link(), Link::Connecting);
    }

    /// One reconnect may be in flight. Repeated R presses while it is
    /// connecting must not create parallel subscriptions.
    #[test]
    fn repeated_reconnect_keys_start_one_subscription() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.refresh.disconnected();

        assert_eq!(
            app.handle_key(Key::Char('R')),
            Some(cyclops_ui::Command::Reconnect)
        );
        assert!(std::mem::take(&mut app.reconnect_owed));
        assert_eq!(app.refresh.link(), Link::Connecting);

        assert_eq!(app.handle_key(Key::Char('R')), None);
        assert!(!app.reconnect_owed);

        // A failed replacement subscription returns to Lost and permits
        // one new explicit attempt.
        app.refresh.disconnected();
        assert_eq!(app.refresh.link(), Link::Lost);
        assert_eq!(
            app.handle_key(Key::Char('R')),
            Some(cyclops_ui::Command::Reconnect)
        );
    }

    /// A subscription acknowledgement proves transport, not that the
    /// old detail is current. Mutations stay blocked until the required
    /// post-gap snapshot lands, and that read never retargets the detail.
    #[test]
    fn reconnect_waits_for_a_current_snapshot_before_actions_resume() {
        let mut app = alarmed_app();
        let frozen = app.detail.as_ref().unwrap().target().clone();
        let action = Action::AttentionComplete;
        let index = app
            .detail
            .as_ref()
            .unwrap()
            .allowed()
            .iter()
            .position(|candidate| *candidate == action)
            .unwrap();

        app.refresh.disconnected();
        assert_eq!(
            app.handle_key(Key::Char('R')),
            Some(cyclops_ui::Command::Reconnect)
        );
        app.refresh.connected();
        assert!(app.refresh.is_connected());
        assert!(!app.refresh.may_mutate());
        let frame = cyclops_ui::build(&mut app, 80, 24).join("\n");
        assert!(frame.contains("refreshing messages"), "{frame}");

        app.handle_key(Key::Char((b'1' + index as u8) as char));
        app.handle_key(Key::Char('y'));
        assert!(!app.has_pending(), "stale evidence crossed the reconnect");
        assert_eq!(*app.detail.as_ref().unwrap().stage(), Stage::Open);

        let request = app.wants_messages().expect("reconnect owes a snapshot");
        assert!(app.apply_messages_response(
            request,
            &snapshot(10, vec![wire_lifecycle("m-001", Some(7), false)])
        ));
        assert!(app.refresh.may_mutate());
        assert_eq!(app.detail.as_ref().unwrap().target(), &frozen);

        app.notice = None;
        app.handle_key(Key::Char((b'1' + index as u8) as char));
        app.handle_key(Key::Char('y'));
        let (_, request) = app.take_pending().expect("current evidence can act");
        match request {
            cyclops_ui::ActionRequest::AttentionComplete { attempt_id } => {
                assert_eq!(attempt_id, attempt(7))
            }
            other => panic!("wrong request after refresh: {other:?}"),
        }
    }

    /// A failed snapshot cannot leave a connected-looking, permanently
    /// disabled surface. It preserves the last read, offers one explicit
    /// reconnect, and only re-enables the frozen action after a new read.
    #[test]
    fn snapshot_failure_recovers_only_through_r_and_a_current_snapshot() {
        let mut app = alarmed_app();
        let frozen = app.detail.as_ref().unwrap().target().clone();
        let action = Action::AttentionComplete;
        let index = app
            .detail
            .as_ref()
            .unwrap()
            .allowed()
            .iter()
            .position(|candidate| *candidate == action)
            .unwrap();

        app.refresh.mark_dirty();
        let failed = app.wants_messages().expect("change owes a snapshot");
        assert!(app.refresh.finish_failure(failed));
        app.notice = Some("messages unavailable: socket closed".into());
        assert_eq!(app.refresh.link(), Link::Lost);
        assert!(!app.refresh.may_mutate());

        app.handle_key(Key::Char((b'1' + index as u8) as char));
        app.handle_key(Key::Char('y'));
        assert!(!app.has_pending());
        let lost = cyclops_ui::build(&mut app, 96, 24).join("\n");
        assert!(
            lost.contains("messages unavailable: socket closed"),
            "{lost}"
        );
        assert!(lost.contains("R reconnect"), "{lost}");

        assert_eq!(
            app.handle_key(Key::Char('R')),
            Some(cyclops_ui::Command::Reconnect)
        );
        app.refresh.connected();
        let request = app.wants_messages().expect("reconnect owes a snapshot");
        assert!(app.apply_messages_response(
            request,
            &snapshot(10, vec![wire_lifecycle("m-001", Some(7), false)])
        ));
        assert_eq!(app.detail.as_ref().unwrap().target(), &frozen);
        assert!(app.refresh.may_mutate());

        app.notice = None;
        app.handle_key(Key::Char((b'1' + index as u8) as char));
        app.handle_key(Key::Char('y'));
        let (_, request) = app.take_pending().expect("current evidence can act");
        match request {
            cyclops_ui::ActionRequest::AttentionComplete { attempt_id } => {
                assert_eq!(attempt_id, attempt(7))
            }
            other => panic!("wrong request after failure recovery: {other:?}"),
        }
    }

    /// Connection truth and notices stay visible on the Messages frame.
    #[test]
    fn messages_shows_connecting_lost_and_notice_states() {
        let mut app = cyclops_ui::App::new(
            cyclops_ui::Theme::none(),
            cyclops_ui::View::Messages,
            cyclops_ui::Filter::default(),
        );
        let connecting = cyclops_ui::build(&mut app, 80, 24).join("\n");
        assert!(connecting.contains("connecting to cyclops"), "{connecting}");

        app.refresh.connected();
        let request = app.wants_messages().expect("connect owes a snapshot");
        assert!(app.apply_messages_response(request, &snapshot(1, Vec::new())));
        app.notice = Some("snapshot refused safely".into());
        let noticed = cyclops_ui::build(&mut app, 80, 24).join("\n");
        assert!(noticed.contains("snapshot refused safely"), "{noticed}");

        app.refresh.disconnected();
        let lost = cyclops_ui::build(&mut app, 80, 24).join("\n");
        assert!(lost.contains("connection lost"), "{lost}");
        assert!(lost.contains("R reconnect"), "{lost}");
        assert!(lost.contains("stale"), "{lost}");
    }

    /// An async notice is supplemental. It must never hide the exact
    /// confirmation while the y key still performs that action.
    #[test]
    fn a_notice_never_replaces_an_actionable_confirmation() {
        let mut app = alarmed_app();
        let action = Action::AttentionComplete;
        let index = app
            .detail
            .as_ref()
            .unwrap()
            .allowed()
            .iter()
            .position(|candidate| *candidate == action)
            .unwrap();
        app.handle_key(Key::Char((b'1' + index as u8) as char));
        app.notice = Some("snapshot refresh failed".into());

        let frame = cyclops_ui::build(&mut app, 80, 24).join("\n");
        assert!(frame.contains("snapshot refresh failed"), "{frame}");
        assert!(frame.contains(&attempt(7).to_string()), "{frame}");
        assert!(frame.contains("at seq 9? y"), "{frame}");
        assert!(frame.contains("to confirm, esc to cancel"), "{frame}");

        app.handle_key(Key::Char('y'));
        assert!(app.has_pending(), "the visible confirmation did not act");
    }

    /// And it does not shadow a key that means something else while the
    /// connection is healthy.
    #[test]
    fn reconnect_is_offered_only_while_the_link_is_down() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        assert!(app.refresh.is_connected());
        assert_ne!(
            app.handle_key(Key::Char('R')),
            Some(cyclops_ui::Command::Reconnect)
        );
        assert!(!app.reconnect_owed);
    }

    /// Reconnecting resets the horizon: everything on screen predates the
    /// gap, so a whole snapshot is owed before any later edge is believed.
    #[test]
    fn reconnecting_forces_a_whole_snapshot() {
        let mut gate = cyclops_ui::RefreshGate::new();
        gate.connected();
        let first = gate.begin().expect("a read is owed at connect");
        gate.disconnected();
        gate.connected();

        let second = gate.begin().expect("a whole snapshot is owed again");
        assert_ne!(first, second, "the reconnect reused its old generation");
        let snapshot = snapshot(10, vec![wire_row("m-001", false)]);
        assert!(
            !gate.finish_snapshot(first, &snapshot),
            "an answer from before the gap was accepted"
        );
    }

    /// A dirty edge must invalidate the answer already in flight, not
    /// merely owe another fetch. Otherwise the reply to a request made
    /// BEFORE the change still lands and is believed.
    #[test]
    fn a_dirty_edge_refuses_the_answer_already_in_flight() {
        let mut gate = cyclops_ui::RefreshGate::new();
        gate.connected();
        let request = gate.begin().expect("a first request is owed");

        // Something changed after that request went out.
        gate.mark_dirty();

        let snapshot = snapshot(10, vec![wire_row("m-001", false)]);
        assert!(
            !gate.finish_snapshot(request, &snapshot),
            "a snapshot from before the change was accepted"
        );
        assert!(
            gate.begin().is_some(),
            "no fresh read was owed after refusing the stale one"
        );
    }

    /// Evidence measured against facts that have moved cannot be acted
    /// on, and a FAILED reload must not quietly declare it current
    /// again.
    #[test]
    fn stale_evidence_blocks_the_terminal_verbs_until_a_reload_lands() {
        let mut app = alarmed_app();
        assert!(app
            .detail
            .as_ref()
            .unwrap()
            .allows(Action::AttentionComplete));

        // The row's facts move under the open detail.
        app.apply_messages(&snapshot(10, vec![wire_lifecycle("m-001", Some(8), false)]));
        assert!(app.detail.as_ref().unwrap().evidence_stale());
        assert!(
            !app.detail
                .as_ref()
                .unwrap()
                .allows(Action::AttentionComplete),
            "stale checks still offered a terminal verb"
        );

        // The reload fails. Evidence is still stale.
        let (token, _) = app.take_detail_read().expect("a reload is owed");
        app.apply_action(token, ActionOutcome::NotSent("no socket".into()));
        assert!(
            app.detail.as_ref().unwrap().evidence_stale(),
            "a failed reload declared stale evidence current"
        );
        assert!(!app
            .detail
            .as_ref()
            .unwrap()
            .allows(Action::AttentionComplete));
        // And it does not spin.
        assert!(app.take_detail_read().is_none());

        // The operator asks again, by hand, and it lands.
        app.handle_key(Key::Char('r'));
        let (token, _) = app.take_detail_read().expect("retry re-owes the read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );
        assert!(!app.detail.as_ref().unwrap().evidence_stale());
        assert!(app
            .detail
            .as_ref()
            .unwrap()
            .allows(Action::AttentionComplete));
    }
}

/// Queue input and visible help must describe the same surface. Stream
/// controls must not mutate hidden stream state while Messages is open.
mod messages_keyboard_and_frame {
    use super::through_the_app::*;
    use super::*;
    use cyclops_proto::MessageDirection;
    use cyclops_ui::{build, ActionOutcome, Key, Scope, View};

    #[test]
    fn footer_help_and_keys_agree() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        let frame = build(&mut app, 80, 24).join("\n");
        assert!(frame.contains("s scope"), "{frame}");
        assert!(frame.contains("tab view"), "{frame}");
        assert!(frame.contains("? help"), "{frame}");
        assert!(!frame.contains("a actions"), "{frame}");

        app.handle_key(Key::Char('?'));
        let help = build(&mut app, 80, 24).join("\n");
        assert!(help.contains("Messages keys"), "{help}");
        assert!(help.contains("s      next scope"), "{help}");
        assert!(help.contains("R      reconnect"), "{help}");

        app.handle_key(Key::Char('?'));
        let before = app.queue.scope();
        app.handle_key(Key::Char('s'));
        assert_ne!(
            app.queue.scope(),
            before,
            "documented scope key did nothing"
        );
        app.handle_key(Key::Tab);
        assert_eq!(app.view, View::Admin, "documented view key did nothing");
    }

    #[test]
    fn help_never_creates_invisible_detail_input_capture() {
        let mut app = app_with(vec![wire_lifecycle("m-001", Some(7), false)], 9);
        app.open_detail().expect("it opens");
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                checks: vec![Check {
                    name: "notification exact".into(),
                    passed: true,
                    detail: None,
                }],
                ..Loaded::default()
            })),
        );
        let action = Action::AttentionComplete;
        let index = app
            .detail
            .as_ref()
            .unwrap()
            .allowed()
            .iter()
            .position(|candidate| *candidate == action)
            .unwrap();

        app.handle_key(Key::Char('?'));
        assert!(!app.overlay);
        let notice = build(&mut app, 80, 24).join("\n");
        assert!(notice.contains("detail keys are shown below"), "{notice}");
        assert!(notice.contains("1 submit staged notification"), "{notice}");

        app.handle_key(Key::Char((b'1' + index as u8) as char));
        let confirming = build(&mut app, 80, 24).join("\n");
        assert!(confirming.contains(&attempt(7).to_string()), "{confirming}");
        app.handle_key(Key::Char('y'));
        assert!(
            app.has_pending(),
            "the visible detail stopped accepting input"
        );
    }

    #[test]
    fn stream_only_keys_do_nothing_in_messages() {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        let density = app.density;
        let roster = app.show_roster;
        let pinned = app.pinned;
        let selected = app.selected;

        for key in [
            Key::Char('c'),
            Key::Char('a'),
            Key::Char('w'),
            Key::Char('f'),
            Key::Char('t'),
            Key::End,
        ] {
            app.handle_key(key);
        }

        assert_eq!(app.density, density);
        assert_eq!(app.show_roster, roster);
        assert_eq!(app.pinned, pinned);
        assert_eq!(app.selected, selected);
        assert!(app.input.is_none(), "a hidden stream filter captured input");
    }

    #[test]
    fn removal_then_enter_in_one_batch_cannot_open_a_replacement() {
        let mut app = app_with(vec![wire_row("m-001", false), wire_row("m-002", false)], 9);
        let selected = app
            .queue
            .visible()
            .find(|row| row.message_id.as_str() == "m-002")
            .unwrap()
            .target
            .clone();
        assert!(app.queue.select(&selected));

        app.apply_messages(&snapshot(10, vec![wire_row("m-001", false)]));
        app.handle_key(Key::Enter);

        assert!(app.queue.selected().is_none());
        assert!(app.detail.is_none(), "Enter opened the replacement row");
        let frame = build(&mut app, 80, 24).join("\n");
        assert!(frame.contains("message changed; select a row"), "{frame}");
    }

    #[test]
    fn scope_then_enter_in_one_batch_cannot_open_a_replacement() {
        let inbound = wire_row("m-in", false);
        let mut outbound = wire_row("m-out", true);
        outbound.direction = MessageDirection::Outbound;
        outbound.recipients[0].direction = MessageDirection::Outbound;
        let mut app = app_with(vec![inbound, outbound], 9);
        app.queue.set_scope(Scope::All);
        let selected = app
            .queue
            .visible()
            .find(|row| row.message_id.as_str() == "m-out")
            .unwrap()
            .target
            .clone();
        assert!(app.queue.select(&selected));

        // All -> Inbox removes m-out. Enter arrives before the redraw.
        app.handle_key(Key::Char('s'));
        app.handle_key(Key::Enter);

        assert_eq!(app.queue.scope(), Scope::Inbox);
        assert!(app.queue.selected().is_none());
        assert!(app.detail.is_none(), "Enter opened the inbox replacement");
        let frame = build(&mut app, 80, 24).join("\n");
        assert!(frame.contains("message changed; select a row"), "{frame}");
    }
}

/// The reply being written has to be on screen while it is written.
mod composer_visibility {
    use super::through_the_app::*;
    use super::*;
    use cyclops_ui::{ActionOutcome, Key};

    fn composing_under_a_long_thread() -> cyclops_ui::App {
        let mut app = app_with(vec![wire_row("m-001", false)], 9);
        app.handle_key(Key::Enter);
        let (token, _) = app.take_detail_read().expect("owes a read");
        app.apply_action(
            token,
            ActionOutcome::Opened(Box::new(Loaded {
                body: Some("the payload\n".repeat(40)),
                thread: (0..20)
                    .map(|i| ThreadEntry {
                        message_id: format!("m-{i:03}"),
                        sender_label: "someone".into(),
                        subject: Some(format!("earlier {i}")),
                        body: Some(format!("an earlier line {i}")),
                        ts: i,
                    })
                    .collect(),
                ..Loaded::default()
            })),
        );
        app.handle_key(Key::Char('1'));
        assert!(app.detail.as_ref().unwrap().is_composing());
        app
    }

    /// The caret says the surface is taking input, including before the
    /// first character is typed.
    #[test]
    fn a_composer_shows_a_caret_from_the_start() {
        let mut app = composing_under_a_long_thread();
        let text = cyclops_ui::build(&mut app, 80, 24).join("\n");
        assert!(
            text.contains('\u{2502}'),
            "no caret while composing:\n{text}"
        );
    }

    /// A reply typed under a long thread was being written off the bottom
    /// of the frame.
    #[test]
    fn the_view_follows_the_draft_through_a_long_thread() {
        let mut app = composing_under_a_long_thread();
        for ch in "the thing I am typing".chars() {
            app.handle_key(Key::Char(ch));
        }
        let text = cyclops_ui::build(&mut app, 80, 24).join("\n");
        assert!(
            text.contains("the thing I am typing"),
            "the draft scrolled out of view:\n{text}"
        );
    }

    /// Every width, because a wrapped draft plus a caret is where a
    /// width assertion would break.
    #[test]
    fn a_composed_draft_never_breaks_the_frame() {
        let mut app = composing_under_a_long_thread();
        for ch in "a very long reply that will certainly need to wrap somewhere".chars() {
            app.handle_key(Key::Char(ch));
        }
        for (w, h) in [(48, 12), (80, 24), (160, 40)] {
            for line in cyclops_ui::build(&mut app, w, h) {
                assert_eq!(line.chars().count(), w, "at {w}x{h}: {line:?}");
            }
        }
    }
}
