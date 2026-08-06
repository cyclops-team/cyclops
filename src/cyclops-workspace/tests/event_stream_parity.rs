//! E2 parity: the workspace event panel must show the same ordered,
//! plain-text rows `cyclops watch` shows for the identical transcript.
//!
//! `cyclops_ui::stream::Record`/`Entry` are the shared, backend-neutral
//! model (src/cyclops-ui/src/stream.rs); `cyclops_workspace::
//! event_stream_rows` is the workspace panel's actual row-producing path
//! (`src/cyclops-workspace/src/render.rs`, also called by
//! `paint_event_stream`). This test feeds one backfill-plus-live
//! transcript to two separately fed `Record`s and asserts that reading
//! one the way `cyclops watch`'s follow mode does (`Record::admits` plus
//! `Entry::lines`, `src/cyclops-ui/src/plain.rs` `print_line`) and
//! reading the other through `event_stream_rows` produce byte-identical
//! rows in the same order. Neither path may re-sort, re-filter beyond the
//! model's own admission decision, or reword a line.

use cyclops_proto::{AgentState, NotifyLevel, PaneSnapshot};
use cyclops_ui::{Entry, EntryKind, Record, StatusSeed, Theme};

fn msg(ts: u64, from: &str, to: &[&str], subject: &str) -> Entry {
    Entry {
        uid: 0,
        ts,
        seq: None,
        id: Some("m-1".into()),
        kind: EntryKind::Msg {
            from: from.into(),
            to: to.iter().map(|t| t.to_string()).collect(),
            subject: subject.into(),
            body: None,
            fyi: false,
        },
    }
}

fn state(ts: u64, target: &str, pane_id: &str, state: AgentState) -> Entry {
    Entry {
        uid: 0,
        ts,
        seq: None,
        id: None,
        kind: EntryKind::State {
            target: target.into(),
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
            deliveries: Vec::new(),
        },
    }
}

/// Feed the canonical backfill-plus-live shape onto one `Record`: a
/// replayed ledger line, the daemon's startup reconciliation (seed), and
/// two live transitions — one that resolves the seed's alarm and one
/// routine transition that must never surface. Mirrors the ordering
/// `cyclops_ui::stream::Intake` enforces (replayed tail, then seed, then
/// live backlog) without needing `Intake` itself: both consumers under
/// test read a `Record` that already holds this history, exactly as the
/// workspace's own boot-then-live-feed path builds one (no ledger tail,
/// per E2's scope; see `App::record`'s doc in `src/cyclops-workspace/
/// src/app.rs`).
fn build_record() -> Record {
    let mut record = Record::new();

    // 1. Replayed history: a message to admin, deterministic clock.
    record.replay(msg(1_000, "codex", &["admin"], "backfilled note"));

    // 2. The daemon's one-time status answer: reviewer is blocked right
    //    now. `Record::seed` stamps this line with the wall clock (it is
    //    the daemon's answer about "now", not a replayed transition), so
    //    its own text is not asserted verbatim below — only that it is
    //    identical between the two `Record`s under test.
    let seed = StatusSeed {
        watched: vec!["main".into()],
        panes: vec![PaneSnapshot {
            pane_id: "%1".into(),
            name: "reviewer".into(),
            state: AgentState::BlockedPermission,
        }],
        open: Vec::new(),
        roster: Vec::new(),
    };
    for e in record.seed(&seed.panes, &seed.open) {
        record.replay(e);
    }

    // 3. A live admin ping about the pane the seed just put in the
    //    register. Both consumers below filter the FULL ring at the end
    //    of this function, by which point step 4 has already resolved
    //    the alarm this ping points at — so it will not be admitted
    //    either, the same "outlived its own alarm" case
    //    `src/cyclops-ui/src/stream.rs` documents on `Record::admits`.
    //    It stays in the transcript because agreeing on that exclusion is
    //    exactly the kind of divergence a private projection could get
    //    wrong silently.
    record.live(ping(2_000, "%1", "reviewer needs you"));

    // 4. The pane clears. Not itself admin-visible (idle needs nobody),
    //    but it resolves the seed's alarm, so the record appends a
    //    `Cleared` row right behind it (rule 8: append, never retract) —
    //    and that row is what admits the ping's absence above.
    record.live(state(3_000, "reviewer", "%1", AgentState::Idle));

    // 5. A routine transition on an unrelated pane: on the record, never
    //    in the calm view.
    record.live(state(4_000, "codex", "%2", AgentState::Working));

    record
}

/// What `cyclops watch`'s follow mode prints for one admitted entry
/// (`src/cyclops-ui/src/plain.rs` `print_line`, comfortable density):
/// the calm-view decision is `Record::admits`, and the words are
/// `Entry::lines` with no color.
fn cyclops_watch_rows(record: &Record) -> Vec<String> {
    let plain = Theme::none();
    record
        .entries()
        .filter(|e| record.admits(e))
        .flat_map(|e| e.lines(&plain, true))
        .collect()
}

#[test]
fn the_workspace_panel_shows_the_same_rows_cyclops_watch_does() {
    let watch_record = build_record();
    let panel_record = build_record();

    let watch_rows = cyclops_watch_rows(&watch_record);
    let panel_rows: Vec<String> = cyclops_workspace::event_stream_rows(&panel_record)
        .into_iter()
        .flat_map(|row| row.lines)
        .collect();

    assert_eq!(
        watch_rows, panel_rows,
        "the panel's row-producing path must not re-sort, re-filter, or reword a line"
    );

    // Three rows survive the calm-view filter, in record order: the
    // backfilled message, the seed's reconciliation line, and the
    // clearance the live idle transition produced. The ping from step 3
    // is correctly excluded — its alarm resolved before either consumer
    // asked, so admission and exclusion agree on both sides too.
    assert_eq!(panel_rows.len(), 3, "{panel_rows:#?}");
    assert_eq!(panel_rows[0], "00:00:01  codex → admin  backfilled note");
    assert!(
        panel_rows[1].ends_with("reviewer  ⚠ blocked_permission"),
        "{:?}",
        panel_rows[1]
    );
    assert_eq!(
        panel_rows[2],
        "00:00:03  reviewer  ✔ cleared · was ⚠ blocked_permission"
    );
}
