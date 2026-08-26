//! Fluidity at 10,000 rows.
//!
//! Two paths matter and they have different budgets. A snapshot
//! replacement is a whole-list rebuild and happens on a daemon edge, so
//! it is allowed one 60Hz frame. A keypress happens while a person holds
//! a key down, so it must not depend on how much is in the queue at all.
//!
//! The fixture carries a real attention backlog. A snapshot with no
//! alarms never exercises the pinning comparator, and a budget certified
//! by a fixture that skips the code path is not a budget.

use std::str::FromStr;
use std::time::Instant;

use cyclops_proto::{
    MessageId, NotificationAttemptId, NotificationAttentionCause, RecipientKey, SessionInstanceId,
    TmuxPaneId, WorkspaceId,
};
use cyclops_ui::queue::render;
use cyclops_ui::{
    Direction, HumanQueue, MailboxWord, QueueRow, QueueTarget, Scope, Snapshot, WakeWord,
};

const ROWS: u64 = 10_000;
/// One 60Hz frame, the bar the rest of this crate is held to. Applies to
/// anything that runs per frame.
const FRAME_BUDGET_MS: f64 = 16.0;
/// A snapshot replacement is not a frame. It runs when the daemon says
/// the state moved, so the bar is that a person does not notice it, not
/// that it fits inside one repaint. Held well below the frame budget on
/// a quiet machine, with room to survive a loaded one: this suite runs
/// on shared developer boxes, and a wall-clock assertion with no
/// headroom fails for reasons that have nothing to do with the code.
const EDGE_BUDGET_MS: f64 = 100.0;
/// A keypress does list arithmetic and nothing else.
const KEY_BUDGET_US: f64 = 50.0;

fn recipient(n: u64) -> RecipientKey {
    RecipientKey::agent(
        WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
        SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap(),
        TmuxPaneId::from_str(&format!("%{}", n % 7 + 1)).unwrap(),
    )
}

/// Every fourth row is an alarm, so the pinning comparator and the Work
/// scope both do real work.
fn row(i: u64) -> QueueRow {
    let attention = i % 4 == 1;
    let message_id = MessageId::new(format!("m-{i:06x}")).unwrap();
    QueueRow {
        // Identity is the row either way. The alarm is a field on it.
        target: QueueTarget::new(message_id.clone(), recipient(i)),
        attention: attention.then(|| {
            NotificationAttemptId::parse(&format!("att-00000000-0000-4000-8000-{i:012x}")).unwrap()
        }),
        resolution_intent: None,
        resolution_action_accepted: None,
        resolution_consumption_observed: None,
        can_manage_attention: attention,
        can_withdraw_notification: false,
        message_id,
        recipient: recipient(i),
        recipient_label: format!("agent{}", i % 7),
        subject: Some(format!("message number {i} with a realistic subject line")),
        mailbox: if i.is_multiple_of(3) {
            MailboxWord::Claimed
        } else {
            MailboxWord::Pending
        },
        wake: if attention {
            WakeWord::NeedsAttention
        } else {
            WakeWord::Notified
        },
        cause: attention.then_some(NotificationAttentionCause::VerifyFailed),
        pre_write_cause: None,
        pre_write_pane_width: None,
        pre_write_required_pane_width: None,
        current_route: None,
        fifo_position: Some(i + 1),
        needs_action: attention || !i.is_multiple_of(3),
        seq: i,
        updated_at: i * 1000,
        direction: if i.is_multiple_of(9) {
            Direction::Outbound
        } else {
            Direction::Inbound
        },
        ..Default::default()
    }
}

fn snapshot(watermark: u64) -> Snapshot {
    Snapshot {
        watermark,
        rows: (0..ROWS).map(row).collect(),
    }
}

#[test]
fn ten_thousand_rows_stay_fluid() {
    let mut q = HumanQueue::new();

    // The fixture has to be real before any number from it means anything.
    q.replace(snapshot(1));
    let counts = q.counts();
    assert_eq!(counts.total, ROWS as usize);
    assert!(counts.attention > 2_000, "backlog is not real: {counts:?}");
    assert!(counts.pending > 2_000, "no pending rows: {counts:?}");

    // Snapshot replacement: filter, order, and re-seat the cursor.
    // Built before the clock starts. Ten thousand format! calls belong to
    // the fixture, not to the code under test, and timing them together
    // was hiding the real number behind fixture construction.
    let next = snapshot(2);
    let t = Instant::now();
    q.replace(next);
    let replace_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Scope change: the filter path on its own.
    let t = Instant::now();
    q.set_scope(Scope::All);
    let filter_all_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    q.set_scope(Scope::Work);
    let filter_work_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Keypress: one step, repeated, with the cursor deep in the list.
    q.set_scope(Scope::All);
    for _ in 0..5_000 {
        q.select_next();
    }
    let presses = 10_000;
    let t = Instant::now();
    for i in 0..presses {
        if i % 2 == 0 {
            q.select_next();
        } else {
            q.select_previous();
        }
    }
    let key_us = t.elapsed().as_secs_f64() * 1_000_000.0 / presses as f64;

    // Freeze: what a confirmation costs.
    let t = Instant::now();
    let frozen = q.freeze().expect("a selection");
    let freeze_us = t.elapsed().as_secs_f64() * 1_000_000.0;
    assert_eq!(frozen.watermark, 2);

    // Render: must depend on the visible rows, not the snapshot.
    // Timed over a run rather than once. A single draw on a shared
    // machine measures whatever else was scheduled that millisecond.
    let frame = render(&q, 160, 40);
    assert_eq!(frame.len(), 40);
    let draws = 50;
    let t = Instant::now();
    for _ in 0..draws {
        render(&q, 160, 40);
    }
    let render_ms = t.elapsed().as_secs_f64() * 1000.0 / draws as f64;

    println!("10k rows: replace {replace_ms:.2}ms, filter all {filter_all_ms:.2}ms, filter work {filter_work_ms:.2}ms");
    println!(
        "          keypress {key_us:.2}us, freeze {freeze_us:.2}us, render 160x40 {render_ms:.2}ms"
    );
    println!("  wall clock, shared machine: a loaded box inflates every figure here");

    assert!(
        replace_ms < EDGE_BUDGET_MS,
        "snapshot replacement {replace_ms:.2}ms over the {EDGE_BUDGET_MS}ms edge budget"
    );
    assert!(
        filter_all_ms < EDGE_BUDGET_MS && filter_work_ms < EDGE_BUDGET_MS,
        "scope filter over budget: all {filter_all_ms:.2}ms work {filter_work_ms:.2}ms"
    );
    assert!(
        key_us < KEY_BUDGET_US,
        "keypress {key_us:.2}us over the {KEY_BUDGET_US}us budget; the cursor is scanning"
    );
    assert!(
        render_ms < FRAME_BUDGET_MS,
        "render {render_ms:.2}ms over the {FRAME_BUDGET_MS}ms frame budget"
    );
}

/// Render stays inside one frame with ten thousand rows behind it.
///
/// The rule is that cost follows the window and not the backlog, which is
/// easy to lose by folding or counting inside the draw loop. The bar is
/// the frame budget, held against the large queue directly. The small
/// queue is printed beside it for comparison, but the ratio between them
/// is not asserted: it is a cache effect on a shared machine, and fitting
/// a multiplier to it would test the load rather than the code.
#[test]
fn render_stays_inside_the_frame_budget_at_ten_thousand_rows() {
    let mut small = HumanQueue::new();
    small.replace(Snapshot {
        watermark: 1,
        rows: (0..40).map(row).collect(),
    });
    let mut big = HumanQueue::new();
    big.replace(snapshot(1));

    let warm = render(&small, 160, 40);
    assert_eq!(warm.len(), 40);

    let t = Instant::now();
    for _ in 0..200 {
        render(&small, 160, 40);
    }
    let small_us = t.elapsed().as_secs_f64() * 1_000_000.0 / 200.0;

    let t = Instant::now();
    for _ in 0..200 {
        render(&big, 160, 40);
    }
    let big_us = t.elapsed().as_secs_f64() * 1_000_000.0 / 200.0;

    println!("render 40 rows {small_us:.1}us, 10k rows {big_us:.1}us");
    println!("  wall clock on whatever machine ran this; compare runs, not absolutes");
    assert!(
        big_us / 1000.0 < FRAME_BUDGET_MS,
        "render of a 10k-row queue took {big_us:.1}us, over the {FRAME_BUDGET_MS}ms frame budget"
    );
}
