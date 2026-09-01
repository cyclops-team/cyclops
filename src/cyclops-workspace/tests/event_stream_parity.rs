//! E2 parity: the workspace sidebar's Stream tab must show the same
//! ordered, plain-text rows `cyclops watch` shows for the identical
//! transcript —
//! and it must ACQUIRE them the same way, not merely format them the
//! same way.
//!
//! One startup-plus-live transcript is driven, step by step, into two
//! records: one fed through the same pure projection that `cyclops watch`
//! consumes, one fed through the workspace's real
//! transport-to-model seam (`cyclops_workspace::event_record`, the
//! functions `App` boot and the `AppMsg::StreamEntry` arm call in
//! production). The transcript exercises the whole startup contract: a
//! live entry that races ahead of the backfill, a status seed that must
//! wait for it, a replayed ledger tail, a seq-duplicate the dedup must
//! drop, and post-startup live traffic. If the workspace skipped the
//! tail, reordered the seed, or showed the duplicate twice — each a real
//! divergence at some point in this surface's history — the two row lists
//! stop being identical.

use cyclops_proto::{AgentState, NotifyLevel, PaneSnapshot};
use cyclops_ui::{
    Entry, EntryKind, Record, StatusSeed, StreamInput, StreamProjectionState, StreamUpdate, Theme,
};
use cyclops_workspace::event_record;

fn msg(ts: u64, seq: Option<u64>, from: &str, to: &[&str], subject: &str) -> Entry {
    Entry {
        uid: 0,
        ts,
        seq,
        id: Some("m-1".into()),
        kind: EntryKind::Msg {
            from: from.into(),
            to: to.iter().map(|t| t.to_string()).collect(),
            endpoints: None,
            subject: subject.into(),
            fyi: false,
        },
    }
}

fn state(ts: u64, seq: Option<u64>, target: &str, pane_id: &str, state: AgentState) -> Entry {
    Entry {
        uid: 0,
        ts,
        seq,
        id: None,
        kind: EntryKind::State {
            target: target.into(),
            recipient: None,
            session_idx: 0,
            pane_id: Some(pane_id.into()),
            state,
        },
    }
}

fn ping(ts: u64, pane_id: &str, subject: &str) -> Entry {
    Entry {
        uid: 0,
        ts,
        seq: None,
        id: Some("p-1".into()),
        kind: EntryKind::Notify {
            level: NotifyLevel::ActionRequired,
            subject: subject.into(),
            pane_id: Some(pane_id.into()),
            to: None,
            recipient: None,
            deliveries: Vec::new(),
        },
    }
}

/// One startup-plus-live step, transport-neutral: what arrived, not how.
enum Step {
    Live(Entry),
    Status(Box<StatusSeed>),
    Backfill(Vec<Entry>, Option<u64>),
}

/// The canonical transcript. The ledger's tail holds a message and the
/// block it recorded (seq 5 and 6); the subscription races the tail read
/// and delivers seq 6 a second time before the backfill lands; the
/// daemon's status answer agrees the pane is still blocked; then live
/// traffic resolves the alarm (which also outlives the admin ping aimed
/// at it) and a routine transition arrives that the calm view must never
/// admit.
fn transcript() -> Vec<Step> {
    let blocked = || {
        state(
            1_500,
            Some(6),
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        )
    };
    vec![
        Step::Live(blocked()),
        Step::Status(Box::new(StatusSeed {
            watched: vec!["main".into()],
            panes: vec![PaneSnapshot {
                pane_id: "%1".into(),
                name: "reviewer".into(),
                state: AgentState::BlockedPermission,
            }],
            open: Vec::new(),
            admin_unread: 0,
            mailbox_routes: Vec::new(),
            roster: Vec::new(),
            mailbox: Vec::new(),
        })),
        Step::Backfill(
            vec![
                msg(1_000, Some(5), "codex", &["admin"], "backfilled note"),
                blocked(),
            ],
            Some(6),
        ),
        Step::Live(ping(3_000, "%1", "reviewer needs you")),
        Step::Live(state(3_500, None, "reviewer", "%1", AgentState::Idle)),
        Step::Live(state(4_000, None, "codex", "%2", AgentState::Working)),
    ]
}

/// The watch adapter's only knowledge is how its `Record` uses semantic
/// presentation updates. Ordering and sequence suppression stay in the shared
/// projection state.
fn feed_like_watch(record: &mut Record, projection: &mut StreamProjectionState, step: Step) {
    let input = match step {
        Step::Live(entry) => StreamInput::Live(entry),
        Step::Status(seed) => StreamInput::Status(seed),
        Step::Backfill(entries, max_seq) => StreamInput::Backfill { entries, max_seq },
    };
    for update in projection.apply(input) {
        match update {
            StreamUpdate::Replay(entry) => record.replay(entry),
            StreamUpdate::Status(seed) => {
                for entry in record.seed(&seed.panes, &seed.open) {
                    record.replay(entry);
                }
            }
            StreamUpdate::Live(entry) => {
                record.live(entry);
            }
            StreamUpdate::Notice(_) => {}
        }
    }
}

/// The workspace's feed: the production seam itself.
fn feed_like_workspace(record: &mut Record, projection: &mut StreamProjectionState, step: Step) {
    match step {
        Step::Live(e) => event_record::live(record, projection, e),
        Step::Status(seed) => event_record::status(record, projection, seed),
        Step::Backfill(entries, max_seq) => {
            event_record::backfill(record, projection, entries, max_seq)
        }
    }
}

/// What `cyclops watch`'s follow mode prints for one admitted entry
/// (`src/cyclops-ui/src/plain.rs` `print_line`, comfortable density):
/// the calm-view decision is `Record::admits`, and the words are
/// `Entry::lines` with no color.
fn cyclops_watch_rows(record: &Record) -> Vec<String> {
    let plain = Theme::none();
    record
        .admitted_entries()
        .flat_map(|e| e.lines(&plain))
        .collect()
}

#[test]
fn the_workspace_stream_acquires_and_shows_the_rows_cyclops_watch_does() {
    let mut watch_record = Record::new();
    let mut watch_projection = StreamProjectionState::new();
    let mut stream_record = Record::new();
    let mut stream_projection = StreamProjectionState::new();

    // Interleave per step (not per consumer) so the two `Record::seed`
    // wall-clock stamps land as close together as two calls can.
    for (watch_step, stream_step) in transcript().into_iter().zip(transcript()) {
        feed_like_watch(&mut watch_record, &mut watch_projection, watch_step);
        feed_like_workspace(&mut stream_record, &mut stream_projection, stream_step);
    }

    let watch_rows = cyclops_watch_rows(&watch_record);
    let stream_rows: Vec<String> = cyclops_workspace::event_stream_rows(&stream_record)
        .into_iter()
        .flat_map(|row| row.lines)
        .collect();

    assert_eq!(
        watch_rows, stream_rows,
        "one transcript, two consumers, one history — acquisition included"
    );

    // The properties the transcript was built to prove, asserted on the
    // workspace's rows (already known equal to watch's above):
    //
    // 1. The replayed ledger tail is present — the workspace does not start
    //    from the seed alone.
    assert_eq!(stream_rows[0], "00:00:01  codex → admin  backfilled note");
    // 2. The seq-6 block shows exactly once: the live copy that raced
    //    the backfill was dropped by seq, the replayed one kept. The
    //    clearance row quotes the same state word ("cleared · was …"),
    //    so it is excluded from the count rather than mistaken for a
    //    second block.
    let blocked_word = AgentState::BlockedPermission.to_string();
    let blocked_rows = stream_rows
        .iter()
        .filter(|r| r.ends_with(&blocked_word) && !r.contains("cleared"))
        .count();
    assert_eq!(blocked_rows, 1, "{stream_rows:#?}");
    // 3. The alarm resolved, so the record appended a clearance (rule 8:
    //    append, never retract) and the ping aimed at the alarm outlived
    //    it — admitted nowhere.
    assert!(
        stream_rows.iter().any(|r| r.contains("cleared")),
        "{stream_rows:#?}"
    );
    assert!(
        !stream_rows.iter().any(|r| r.contains("needs you")),
        "{stream_rows:#?}"
    );
    // 4. The routine transition never surfaces in the calm view.
    assert!(
        !stream_rows
            .iter()
            .any(|r| r.ends_with(&AgentState::Working.to_string())),
        "{stream_rows:#?}"
    );
}
