//! The queue model a human reads, and the frames it renders.
//!
//! Each test names one rule the queue has to hold. Testing them here is
//! worth it because the model is pure: no daemon, no socket, no terminal,
//! so a rule either holds in the type or it does not hold at all.

use std::str::FromStr;

use cyclops_proto::{
    MessageId, NotificationAttemptId, NotificationAttentionCause, RecipientKey, SessionInstanceId,
    TmuxPaneId, WorkspaceId,
};
use cyclops_ui::queue::render;
use cyclops_ui::{
    Direction, HumanQueue, MailboxWord, QueueRow, QueueTarget, Scope, Snapshot, WakeWord,
};

const SIZES: [(usize, usize); 5] = [(14, 8), (24, 12), (80, 24), (96, 24), (160, 40)];

fn workspace() -> WorkspaceId {
    WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap()
}

fn recipient(pane: &str) -> RecipientKey {
    RecipientKey::agent(
        workspace(),
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap(),
        TmuxPaneId::from_str(pane).unwrap(),
    )
}

fn attempt(n: u64) -> NotificationAttemptId {
    NotificationAttemptId::parse(&format!("att-00000000-0000-4000-8000-{n:012x}")).unwrap()
}

/// A message row's address: the message AND whose mailbox it is in.
fn msg_target(id: &str, to: RecipientKey) -> QueueTarget {
    QueueTarget::new(MessageId::new(id).unwrap(), to)
}

fn message(id: &str, seq: u64, label: &str) -> QueueRow {
    QueueRow {
        target: msg_target(id, recipient("%1")),
        attention: None,
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        can_manage_attention: false,
        can_withdraw_notification: false,
        message_id: MessageId::new(id).unwrap(),
        recipient: recipient("%1"),
        recipient_label: label.into(),
        subject: Some(format!("subject for {id}")),
        mailbox: MailboxWord::Pending,
        wake: WakeWord::Notified,
        cause: None,
        pre_write_cause: None,
        current_route: None,
        fifo_position: Some(1),
        needs_action: true,
        seq,
        updated_at: seq * 1000,
        direction: Direction::Inbound,
    }
}

fn alarm(id: &str, n: u64, seq: u64, label: &str) -> QueueRow {
    QueueRow {
        // The row is still the message in %2's mailbox. The alarm is a
        // fact about it, not a different row.
        target: msg_target(id, recipient("%2")),
        attention: Some(attempt(n)),
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        can_manage_attention: true,
        can_withdraw_notification: false,
        message_id: MessageId::new(id).unwrap(),
        recipient: recipient("%2"),
        recipient_label: label.into(),
        subject: Some(format!("subject for {id}")),
        mailbox: MailboxWord::Pending,
        wake: WakeWord::NeedsAttention,
        cause: Some(NotificationAttentionCause::VerifyFailed),
        pre_write_cause: None,
        current_route: None,
        fifo_position: Some(1),
        needs_action: true,
        seq,
        updated_at: seq * 1000,
        direction: Direction::Inbound,
    }
}

fn snapshot(watermark: u64, rows: Vec<QueueRow>) -> Snapshot {
    Snapshot { watermark, rows }
}

fn loaded() -> HumanQueue {
    let mut q = HumanQueue::new();
    q.replace(snapshot(
        10,
        vec![
            message("m-001", 1, "reviewer"),
            message("m-002", 2, "codex"),
            alarm("m-003", 7, 3, "impl"),
            message("m-004", 4, "docs"),
            alarm("m-005", 8, 5, "builder"),
        ],
    ));
    q
}

/// Attention sits above the inbox, and inside each band the daemon's FIFO
/// order is preserved. The queue never sorts by age, subject, or label.
#[test]
fn attention_is_pinned_and_fifo_holds_inside_each_band() {
    let q = loaded();
    // Named by the message, not by the attempt: an alarm does not rename
    // the row it is attached to. The BAND is what attention decides.
    let ids: Vec<String> = q.visible().map(|r| r.target.id()).collect();
    assert_eq!(
        ids,
        vec![
            "m-003".to_string(),
            "m-005".to_string(),
            "m-001".to_string(),
            "m-002".to_string(),
            "m-004".to_string(),
        ],
        "attention first, then FIFO, and FIFO inside each band"
    );
}

/// The selected row survives insertion, a rename, and a reorder, because
/// the cursor is an id and not a position. Audit acceptance 9 and 14.
#[test]
fn selection_survives_insertion_rename_and_reorder() {
    let mut q = loaded();
    let chosen = msg_target("m-002", recipient("%1"));
    assert!(q.select(&chosen));

    // A new alarm arrives above it, the recipient is renamed, and the
    // daemon hands back the rows in a different order.
    let mut rows = vec![
        message("m-004", 4, "docs"),
        alarm("m-009", 9, 0, "newcomer"),
        message("m-002", 2, "codex-renamed"),
        message("m-001", 1, "reviewer"),
        alarm("m-003", 7, 3, "impl"),
        alarm("m-005", 8, 5, "builder"),
    ];
    rows.reverse();
    q.replace(snapshot(11, rows));

    let still = q.selected().expect("something is selected");
    assert_eq!(still.target, chosen, "the cursor followed the id");
    assert_eq!(
        still.recipient_label, "codex-renamed",
        "a rename changes chrome, not identity"
    );
}

/// When the selected row is gone, no positional replacement becomes the
/// new action target. The next Enter must not open a row the operator did
/// not select.
#[test]
fn a_vanished_selection_clears_instead_of_retargeting() {
    let mut q = loaded();
    let chosen = msg_target("m-002", recipient("%1"));
    q.select(&chosen);
    // m-002 is claimed away. Everything else stands.
    q.replace(snapshot(
        12,
        vec![
            message("m-001", 1, "reviewer"),
            alarm("m-003", 7, 3, "impl"),
            message("m-004", 4, "docs"),
            alarm("m-005", 8, 5, "builder"),
        ],
    ));

    assert!(q.selected().is_none(), "a different row became selected");
    assert!(q.freeze().is_none(), "a different row became actionable");

    // Replaying the same snapshot must not silently arm the first row.
    q.replace(snapshot(
        12,
        vec![
            message("m-001", 1, "reviewer"),
            alarm("m-003", 7, 3, "impl"),
            message("m-004", 4, "docs"),
            alarm("m-005", 8, 5, "builder"),
        ],
    ));
    assert!(q.selected().is_none());
    assert!(q.freeze().is_none());
}

/// An action names one id and the state it was read at. Nothing about a
/// row's position travels with it.
#[test]
fn a_frozen_target_names_one_id_and_its_watermark() {
    let mut q = loaded();
    let chosen = msg_target("m-003", recipient("%2"));
    q.select(&chosen);

    let frozen = q.freeze().expect("a selection freezes");
    assert_eq!(frozen.target, chosen);
    assert_eq!(frozen.watermark, 10);

    // The queue moves on. The frozen target does not: it still names the
    // id and the watermark the operator confirmed against, which is what
    // lets the daemon refuse work aimed at a state that has moved.
    q.replace(snapshot(99, vec![message("m-001", 1, "reviewer")]));
    assert_eq!(frozen.watermark, 10);
    assert_eq!(frozen.target, chosen);
    assert!(
        q.freeze().is_none(),
        "the current queue retargeted the frozen action"
    );
}

/// Each scope admits what it says, and the cursor is kept whenever the
/// row it names is still on screen.
#[test]
fn scopes_admit_what_they_say_and_keep_a_visible_selection() {
    let mut q = HumanQueue::new();
    let mut outbound = message("m-100", 6, "reviewer");
    outbound.direction = Direction::Outbound;
    outbound.wake = WakeWord::Notified;
    // The daemon answers Work for whoever asked, so the fixture says what
    // it would say: what you sent and what you already claimed is not
    // waiting on you.
    outbound.needs_action = false;
    let mut claimed = message("m-101", 7, "codex");
    claimed.mailbox = MailboxWord::Claimed;
    claimed.needs_action = false;
    q.replace(snapshot(
        1,
        vec![
            message("m-001", 1, "reviewer"),
            alarm("m-003", 7, 3, "impl"),
            outbound,
            claimed,
        ],
    ));

    q.set_scope(Scope::All);
    assert_eq!(q.len(), 4);

    q.set_scope(Scope::Inbox);
    assert_eq!(q.len(), 3, "outbound is not inbox");

    q.set_scope(Scope::Outbound);
    assert_eq!(q.len(), 1);
    assert!(
        q.selected().is_none(),
        "a scope change selected a positional replacement"
    );
    let outbound_target = msg_target("m-100", recipient("%1"));
    assert!(q.select(&outbound_target));

    // Work is what the daemon marked for this reader, plus any open
    // alarm. A claimed message is not work, and neither is what you sent.
    q.set_scope(Scope::Work);
    let ids: Vec<String> = q.visible().map(|r| r.target.id()).collect();
    assert_eq!(ids, vec!["m-003".to_string(), "m-001".to_string()]);

    // A selection that stays visible across a scope change is kept.
    let chosen = msg_target("m-001", recipient("%1"));
    q.select(&chosen);
    q.set_scope(Scope::All);
    assert_eq!(q.selected().unwrap().target, chosen);
}

/// A scope change and Enter can arrive in one input batch, before the
/// new frame is drawn. Losing the selected id must disarm Enter instead
/// of selecting the row at the same position in the new scope.
#[test]
fn a_scope_that_hides_the_selection_clears_the_action_target() {
    let mut inbound = message("m-in", 1, "inbound");
    inbound.direction = Direction::Inbound;
    let mut outbound = message("m-out", 2, "outbound");
    outbound.direction = Direction::Outbound;
    outbound.needs_action = false;

    let mut q = HumanQueue::new();
    q.replace(snapshot(1, vec![inbound, outbound]));
    q.set_scope(Scope::All);
    assert!(q.select(&msg_target("m-in", recipient("%1"))));

    q.set_scope(Scope::Outbound);
    assert!(q.selected().is_none());
    assert!(q.freeze().is_none());
    assert_eq!(q.len(), 1, "the replacement row is still visible");
}

/// Once a live update clears selection, the first navigation key must
/// not skip a row because the old cursor position no longer exists.
#[test]
fn navigation_from_no_selection_starts_at_the_requested_edge() {
    let mut q = loaded();
    let vanished = msg_target("m-002", recipient("%1"));
    assert!(q.select(&vanished));
    q.replace(snapshot(
        12,
        vec![
            message("m-001", 1, "reviewer"),
            alarm("m-003", 7, 3, "impl"),
            message("m-004", 4, "docs"),
        ],
    ));
    assert!(q.selected().is_none());

    let first = q.visible().next().unwrap().target.clone();
    q.select_next();
    assert_eq!(q.selected().unwrap().target, first);

    q.replace(snapshot(
        13,
        vec![
            message("m-001", 1, "reviewer"),
            alarm("m-003", 7, 3, "impl"),
            message("m-004", 4, "docs"),
        ],
    ));
    // Explicitly clear via a scope that hides the current row, then
    // return to All without selecting a replacement.
    q.set_scope(Scope::Outbound);
    q.set_scope(Scope::All);
    assert!(q.selected().is_none());
    let last = q.visible().last().unwrap().target.clone();
    q.select_previous();
    assert_eq!(q.selected().unwrap().target, last);
}

/// One row per message and recipient, never two.
///
/// A message pending for somebody whose wake attempt needs attention is
/// one thing a person deals with, not two, so it appears once and is
/// addressed by the attempt. Emitting both a Message row and an Attention
/// row for the same pair would show the same work twice and give it two
/// different ids to act on.
///
/// The adapter is what enforces this. The test is here so the rule is
/// written down against the model that depends on it, and so a queue fed
/// a duplicate fails visibly rather than rendering it.
#[test]
fn a_message_and_recipient_appear_once_and_an_alarm_owns_the_row() {
    let mut q = HumanQueue::new();
    let mut alarmed = message("m-001", 1, "reviewer");
    // Both facts on one row: still waiting to be claimed, and its wake
    // attempt needs a human. The identity does NOT change for it: the
    // row is still the message in that mailbox, and the attempt is
    // carried alongside as what an action resolves.
    alarmed.attention = Some(attempt(7));
    alarmed.can_manage_attention = true;
    alarmed.wake = WakeWord::NeedsAttention;
    alarmed.cause = Some(NotificationAttentionCause::VerifyFailed);
    q.replace(snapshot(1, vec![alarmed, message("m-002", 2, "codex")]));
    q.set_scope(Scope::All);

    let rows: Vec<&str> = q.visible().map(|r| r.message_id.as_str()).collect();
    assert_eq!(rows, vec!["m-001", "m-002"], "one row per message");
    let first = q.visible().next().unwrap();
    assert_eq!(
        first.target,
        msg_target("m-001", recipient("%1")),
        "an alarm changed the row's identity"
    );
    assert_eq!(first.attention, Some(attempt(7)));
    assert_eq!(first.mailbox, MailboxWord::Pending);
    assert_eq!(first.wake, WakeWord::NeedsAttention);

    // Work shows it once, not once per fact.
    q.set_scope(Scope::Work);
    assert_eq!(q.len(), 2);
    assert_eq!(
        q.visible()
            .filter(|r| r.message_id.as_str() == "m-001")
            .count(),
        1
    );

    // The two header counts answer different questions, so one row that
    // is both pending and alarmed is counted in both. That is correct and
    // is not a duplicate row.
    let counts = q.counts();
    assert_eq!(counts.visible, 2);
    assert_eq!(counts.attention, 1);
    assert_eq!(counts.pending, 2);
}

/// A broadcast puts one message in two mailboxes, and the two rows are
/// different targets.
///
/// The message id alone names both, so a cursor or a confirmation keyed
/// on it would land on whichever row came first. Selecting the second
/// recipient and then acting would claim, requeue or discard the first
/// recipient's copy instead: the operator reads one id, sees the right
/// row highlighted, and hits a different one.
#[test]
fn a_broadcast_keeps_its_recipients_apart_through_a_reorder() {
    let one = recipient("%1");
    let two = recipient("%2");
    let row_for = |to: RecipientKey, label: &str, seq: u64| QueueRow {
        target: msg_target("m-broadcast", to),
        attention: None,
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        can_manage_attention: false,
        can_withdraw_notification: false,
        message_id: MessageId::new("m-broadcast").unwrap(),
        recipient: to,
        recipient_label: label.into(),
        subject: Some("one message, two mailboxes".into()),
        mailbox: MailboxWord::Pending,
        wake: WakeWord::Notified,
        cause: None,
        pre_write_cause: None,
        current_route: None,
        fifo_position: Some(1),
        needs_action: true,
        seq,
        updated_at: seq * 1000,
        direction: Direction::Inbound,
    };

    let mut q = HumanQueue::new();
    q.replace(snapshot(
        5,
        vec![row_for(one, "reviewer", 1), row_for(two, "codex", 2)],
    ));
    assert_eq!(q.len(), 2, "two mailboxes, two rows");

    // The two rows carry the same message id and are still distinct.
    let ids: Vec<String> = q.visible().map(|r| r.target.id()).collect();
    assert_eq!(ids, vec!["m-broadcast", "m-broadcast"]);
    assert_ne!(
        msg_target("m-broadcast", one),
        msg_target("m-broadcast", two),
        "the recipient is part of the address"
    );

    // Take the SECOND recipient's row.
    let chosen = msg_target("m-broadcast", two);
    assert!(q.select(&chosen));
    assert_eq!(q.selected().unwrap().recipient, two);
    assert_eq!(q.selected().unwrap().recipient_label, "codex");

    // The daemon hands the same two rows back the other way round, with a
    // rename on the one that was not selected.
    q.replace(snapshot(
        6,
        vec![
            row_for(two, "codex", 2),
            row_for(one, "reviewer-renamed", 1),
        ],
    ));

    let still = q.selected().expect("something is selected");
    assert_eq!(still.target, chosen, "the cursor followed the exact row");
    assert_eq!(still.recipient, two, "and not the other recipient's copy");
    assert_eq!(still.recipient_label, "codex");

    let frozen = q.freeze().expect("a selection freezes");
    assert_eq!(frozen.target, chosen);
    assert_eq!(
        frozen.target.recipient(),
        Some(two),
        "the frozen target names the recipient the operator chose"
    );
    assert_eq!(frozen.watermark, 6);
}

/// No attempt is not the same as an attempt that has not written yet.
///
/// "none" reads as nothing to do. A row with no attempt started has not
/// been woken at all, which is a different situation from one that is
/// queued or gating, and an operator deciding whether to requeue needs
/// to tell them apart.
#[test]
fn no_attempt_reads_as_not_started_and_is_not_waiting() {
    assert!(WakeWord::NotStarted.cell().contains("not started"));
    assert!(!WakeWord::NotStarted.cell().contains("none"));
    assert!(!WakeWord::NotStarted.short().contains("none"));
    assert_eq!(WakeWord::Withdrawn.cell(), "= withdrawn");
    assert_eq!(WakeWord::Withdrawn.short(), "=wdrn");

    let words: Vec<&str> = [
        WakeWord::NotStarted,
        WakeWord::Queued,
        WakeWord::Gating,
        WakeWord::Writing,
        WakeWord::Staged,
        WakeWord::Submitted,
        WakeWord::BlockedBeforeWrite,
        WakeWord::Notified,
        WakeWord::Withdrawn,
        WakeWord::WithdrawnByOperator,
        WakeWord::NeedsAttention,
        WakeWord::ResolutionIncomplete,
        WakeWord::Cleared,
        WakeWord::ResolvedSubmitted,
        WakeWord::ResolvedDiscarded,
        WakeWord::Superseded,
    ]
    .iter()
    .map(|w| w.cell())
    .collect();
    let mut unique = words.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), words.len(), "two wake states read the same");

    // And the short forms stay apart too, at the width where they are used.
    let shorts: Vec<&str> = [
        WakeWord::NotStarted,
        WakeWord::Queued,
        WakeWord::Gating,
        WakeWord::Writing,
        WakeWord::Staged,
        WakeWord::Submitted,
        WakeWord::BlockedBeforeWrite,
        WakeWord::Notified,
        WakeWord::Withdrawn,
        WakeWord::WithdrawnByOperator,
        WakeWord::NeedsAttention,
        WakeWord::ResolutionIncomplete,
        WakeWord::Cleared,
        WakeWord::ResolvedSubmitted,
        WakeWord::ResolvedDiscarded,
        WakeWord::Superseded,
    ]
    .iter()
    .map(|w| w.short())
    .collect();
    let mut unique = shorts.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), shorts.len());

    let mut q = HumanQueue::new();
    let mut fresh = message("m-001", 1, "reviewer");
    fresh.wake = WakeWord::NotStarted;
    q.replace(snapshot(1, vec![fresh]));
    q.set_scope(Scope::All);
    let frame = render(&q, 96, 24).join("\n");
    assert!(frame.contains("not started"), "{frame}");
}

/// The part of an id every width renders. The sidebar has room for the
/// tail only, so that is what a frame is searched for.
fn tail(target: &QueueTarget) -> String {
    let id = target.id();
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

/// The line the cursor is on, found the way a reader finds it.
fn marked(frame: &[String]) -> Option<&String> {
    frame.iter().find(|line| line.starts_with('>'))
}

/// A long queue scrolls, so the cursor is never off the frame.
///
/// The renderer drew the first screenful whatever the cursor was doing,
/// so on any list longer than the window the operator moved the cursor
/// down, the highlight left the screen, and the next action targeted a
/// row they could not see.
#[test]
fn the_cursor_stays_on_screen_in_a_long_queue() {
    let rows: Vec<QueueRow> = (0..100)
        .map(|i| message(&format!("m-{i:03}"), i, "reviewer"))
        .collect();
    let mut q = HumanQueue::new();
    q.replace(snapshot(1, rows));
    q.set_scope(Scope::All);
    assert_eq!(q.len(), 100);

    // The full table, the 14-column sidebar, and a frame too short for
    // either. All three draw a body, so all three can lose the cursor.
    for (w, h) in [(80usize, 24usize), (14, 8), (80, 4)] {
        assert!(
            q.len() > h,
            "{w}x{h}: the fixture must not fit in one frame"
        );
        q.select(&msg_target("m-000", recipient("%1")));

        for step in 0..60usize {
            let frame = render(&q, w, h);
            let wanted = tail(&q.selected().expect("a selection").target);
            let line = marked(&frame).unwrap_or_else(|| {
                panic!("{w}x{h} step {step}: cursor is off the frame\n{frame:#?}")
            });
            assert!(
                line.contains(&wanted),
                "{w}x{h} step {step}: marked line is not the selected row: {line:?}"
            );
            q.select_next();
        }

        for step in 0..60usize {
            q.select_previous();
            let frame = render(&q, w, h);
            let wanted = tail(&q.selected().expect("a selection").target);
            let line = marked(&frame)
                .unwrap_or_else(|| panic!("{w}x{h} back {step}: cursor is off the frame"));
            assert!(line.contains(&wanted), "{w}x{h} back {step}: {line:?}");
        }

        for line in render(&q, w, h) {
            assert_eq!(cyclops_ui::grid::display_width(&line), w, "{w}x{h}");
        }
    }

    let (w, h) = (80usize, 24usize);

    // A snapshot replacement keeps the cursor on screen too, including
    // when the row it names has moved a long way down the list.
    let chosen = msg_target("m-080", recipient("%1"));
    assert!(q.select(&chosen));
    let reordered: Vec<QueueRow> = (0..100)
        .rev()
        .map(|i| message(&format!("m-{i:03}"), 100 - i, "reviewer"))
        .collect();
    q.replace(snapshot(2, reordered));
    let frame = render(&q, w, h);
    assert_eq!(q.selected().unwrap().target, chosen);
    let line = marked(&frame).expect("cursor is off the frame after replacement");
    assert!(line.contains("m-080"), "{line:?}");

    // The frame is still exactly the size asked for at every step.
    assert_eq!(frame.len(), h);
    for line in &frame {
        assert_eq!(cyclops_ui::grid::display_width(line), w);
    }
}

/// Between the sidebar and a full table, both state words survive whole.
///
/// A table sized to the terminal but built wider than the line gets
/// trimmed at the end, and the end is the wake state. Dropping the
/// recipient and the subject instead keeps the two words an operator
/// acts on.
#[test]
fn narrow_widths_keep_both_state_words_whole() {
    let mut row = message("m-001", 1, "a-very-long-recipient-label");
    row.mailbox = MailboxWord::Superseded;
    row.wake = WakeWord::NotStarted;
    row.subject = Some("a subject that must not survive at this width".into());
    let mut q = HumanQueue::new();
    q.replace(snapshot(1, vec![row]));
    q.set_scope(Scope::All);
    q.select(&msg_target("m-001", recipient("%1")));

    for w in [20usize, 24, 28] {
        let frame = render(&q, w, 12);
        for line in &frame {
            assert_eq!(
                cyclops_ui::grid::display_width(line),
                w,
                "{w}: line is not {w} columns: {line:?}"
            );
        }
        let line = marked(&frame).unwrap_or_else(|| panic!("{w}: no cursor line"));
        assert!(
            line.contains(MailboxWord::Superseded.short()),
            "{w}: mailbox word lost: {line:?}"
        );
        assert!(
            line.contains(WakeWord::NotStarted.short()),
            "{w}: wake word truncated: {line:?}"
        );
        assert!(
            !line.contains("a-very-long"),
            "{w}: recipient kept at the cost of the state: {line:?}"
        );
        assert!(
            !line.contains("a subject"),
            "{w}: subject kept at the cost of the state: {line:?}"
        );
    }
}

/// A row has nowhere to put a body.
///
/// The guarantee is structural rather than a rule the renderer follows,
/// so the test guards the structure: if someone adds a body field later,
/// this fails before any frame can leak one.
#[test]
fn a_row_has_no_body_field() {
    let row = message("m-001", 1, "reviewer");
    let debug = format!("{row:?}");
    assert!(
        !debug.to_lowercase().contains("body"),
        "QueueRow grew a body field: {debug}"
    );
}

/// Every frame is exactly the size it was asked for, at every width the
/// workspace can hand it, and no frame leaks a body.
#[test]
fn frames_are_exactly_the_size_asked_for() {
    let mut q = loaded();
    q.select(&msg_target("m-001", recipient("%1")));

    for scope in Scope::ORDER {
        q.set_scope(scope);
        for (w, h) in SIZES {
            let frame = render(&q, w, h);
            assert_eq!(frame.len(), h, "{scope:?} {w}x{h}: wrong height");
            for (i, line) in frame.iter().enumerate() {
                assert_eq!(
                    cyclops_ui::grid::display_width(line),
                    w,
                    "{scope:?} {w}x{h}: line {i} is not {w} columns: {line:?}"
                );
            }
        }
    }
}

/// The narrow sidebar still says how much is waiting and that there is
/// somewhere to go. An empty queue says so rather than rendering blank.
#[test]
fn the_narrow_sidebar_still_carries_counts_and_a_way_in() {
    let q = loaded();
    let frame = render(&q, 14, 8);
    assert!(
        frame[0].contains("msg"),
        "no count in the sidebar: {frame:?}"
    );
    assert!(
        frame.iter().any(|l| l.contains('!')),
        "attention is not marked at 14 columns: {frame:?}"
    );
    assert!(
        frame.iter().any(|l| l.contains("enter")),
        "no way in from the sidebar: {frame:?}"
    );

    let empty = HumanQueue::new();
    let frame = render(&empty, 80, 24);
    assert_eq!(frame.len(), 24);
    assert!(frame[frame.len() - 1].contains("select a row"));
}

/// State is a word plus a symbol at every width, never a colour and never
/// a bare glyph. Mailbox and wake stay separate: a notified message is
/// not a claimed one.
#[test]
fn state_is_words_and_symbols_and_the_two_records_stay_apart() {
    for word in [
        MailboxWord::Pending,
        MailboxWord::Claimed,
        MailboxWord::Superseded,
    ] {
        for cell in [word.cell(), word.short()] {
            assert!(cell.chars().any(|c| c.is_ascii_alphabetic()), "{cell:?}");
            assert!(
                cell.chars().any(|c| !c.is_ascii_alphanumeric() && c != ' '),
                "{cell:?} has no symbol"
            );
        }
    }
    for word in [
        WakeWord::Queued,
        WakeWord::Gating,
        WakeWord::Writing,
        WakeWord::Staged,
        WakeWord::Submitted,
        WakeWord::BlockedBeforeWrite,
        WakeWord::Notified,
        WakeWord::Withdrawn,
        WakeWord::WithdrawnByOperator,
        WakeWord::NeedsAttention,
        WakeWord::ResolutionIncomplete,
        WakeWord::Cleared,
        WakeWord::ResolvedSubmitted,
        WakeWord::ResolvedDiscarded,
        WakeWord::NotStarted,
        WakeWord::Superseded,
    ] {
        for cell in [word.cell(), word.short()] {
            assert!(cell.chars().any(|c| c.is_ascii_alphabetic()), "{cell:?}");
        }
    }

    // The words never claim delivery or reading.
    for w in [
        WakeWord::Notified,
        WakeWord::Queued,
        WakeWord::Gating,
        WakeWord::Writing,
        WakeWord::Staged,
        WakeWord::Submitted,
        WakeWord::BlockedBeforeWrite,
        WakeWord::Withdrawn,
        WakeWord::WithdrawnByOperator,
        WakeWord::ResolutionIncomplete,
        WakeWord::Cleared,
        WakeWord::ResolvedSubmitted,
        WakeWord::ResolvedDiscarded,
    ] {
        let cell = w.cell().to_lowercase();
        for forbidden in ["delivered", "read", "complete", "done"] {
            assert!(!cell.contains(forbidden), "{cell:?} claims {forbidden}");
        }
    }

    let mut q = loaded();
    q.set_scope(Scope::All);
    let frame = render(&q, 96, 24).join("\n");
    assert!(frame.contains("pending"), "{frame}");
    assert!(frame.contains("needs attention"), "{frame}");
}
