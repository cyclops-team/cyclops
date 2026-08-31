//! How the workspace's event record is fed: `cyclops watch`'s own
//! startup contract, applied to the shared `cyclops_ui::Record`.
//!
//! `cyclops watch` reconciles three sources whose ages differ and whose
//! order is the whole correctness story — the daemon's bounded history is
//! history, the status seed is the daemon's answer about now, and a live
//! entry that arrives during startup is newer than either. The buffering
//! that enforces that order lives in `cyclops_ui::StreamProjectionState`
//! (src/cyclops-ui/src/projection.rs) and is deliberately transport-neutral:
//! it does not care that watch feeds it from spawned tokio tasks while
//! this panel feeds it from a boot sequence plus the workspace's one
//! `events.subscribe` reader thread.
//!
//! These three functions are the workspace's whole transport-to-model
//! surface, one per source, mirroring the three message arms in
//! `cyclops_ui`'s plain follow loop (src/cyclops-ui/src/plain.rs). The
//! cross-crate parity test drives THESE functions with a
//! backfill-plus-live transcript and asserts the panel shows exactly the
//! rows `cyclops watch` prints for it — so anything this module did
//! differently from watch (a skipped tail, a reordered seed, a duplicate
//! the seq dedup should have dropped) fails a test instead of drifting
//! silently.

use std::path::Path;

use cyclops_ui::{
    Entry, Record, StatusSeed, StreamInput, StreamProjection, StreamProjectionState, StreamUpdate,
};

/// History rows the boot replays: the same tail `cyclops watch` replays
/// by default (`--backfill`, src/cyclops/src/main.rs), so opening the
/// panel and opening the CLI start from the same history.
pub const BACKFILL: usize = 200;

/// One bounded replacement for the shared stream model. Loading touches the
/// daemon socket, so the application builds this off-loop after an ingress
/// gap and installs it as one result.
pub type Bootstrap = StreamProjection;

/// One live subscription entry, through the projection's startup buffering
/// and seq dedup. Before the backfill lands this buffers; after it, a
/// ledger-backed entry the replayed tail already carries is dropped
/// here rather than shown twice.
pub fn live(record: &mut Record, projection: &mut StreamProjectionState, entry: Entry) {
    let _ = apply(record, projection.apply(StreamInput::Live(entry)));
}

/// The daemon's status answer. Held until the backfill lands so it
/// reconciles over the replayed tail rather than under it; a fold taken
/// before a transition must not re-open an item that transition closed.
pub fn status(record: &mut Record, projection: &mut StreamProjectionState, seed: Box<StatusSeed>) {
    let _ = apply(record, projection.apply(StreamInput::Status(seed)));
}

/// The one-time daemon backfill. Applying what lands is the startup order
/// itself: replayed history first, then the held seed, then whatever
/// queued live while the two were loading.
pub fn backfill(
    record: &mut Record,
    projection: &mut StreamProjectionState,
    entries: Vec<Entry>,
    max_seq: Option<u64>,
) {
    let _ = apply(
        record,
        projection.apply(StreamInput::Backfill { entries, max_seq }),
    );
}

/// Apply pure projection updates to the workspace's stream model. The
/// workspace keeps only its rendering state here; the projection owns the
/// replay/current/live order and sequence suppression.
fn apply(record: &mut Record, updates: Vec<StreamUpdate>) -> Option<String> {
    let mut notice = None;
    for update in updates {
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
            StreamUpdate::Notice(text) => notice = Some(text),
        }
    }
    notice
}

/// Boot-time construction: one bounded status request, then one daemon-owned
/// history projection, exactly the two fetches `cyclops_ui` performs after an
/// acknowledged subscription. The status answer and history projection share
/// the daemon's authoritative source set. The workspace repeats this same
/// bounded replacement after an observed stream gap, never on a timer.
pub fn boot(
    record: &mut Record,
    projection: &mut StreamProjectionState,
    home: &Path,
) -> Option<String> {
    install(record, projection, load(home))
}

pub fn load(home: &Path) -> Bootstrap {
    let params = cyclops_proto::StatusParams {
        open_deliveries: true,
    };
    let seed = crate::daemon::status(home, params)
        .ok()
        .map(|s| Box::new(StatusSeed::from_status(&s)));
    let report = match crate::daemon::stream_backfill(home, BACKFILL) {
        Ok(result) => cyclops_ui::project_backfill(result),
        Err(error) => cyclops_ui::BackfillReport {
            entries: Vec::new(),
            max_seq: None,
            warning: Some(format!(
                "backfill unavailable; stream history has a gap: {error}. Use cyclops history for the durable record"
            )),
        },
    };
    StreamProjection {
        seed,
        entries: report.entries,
        max_seq: report.max_seq,
        warning: report.warning,
    }
}

pub fn install(
    record: &mut Record,
    projection: &mut StreamProjectionState,
    bootstrap: Bootstrap,
) -> Option<String> {
    apply(record, projection.replace(bootstrap))
}

#[cfg(test)]
mod tests {
    use cyclops_proto::{AgentState, PaneSnapshot};
    use cyclops_ui::EntryKind;

    use super::*;

    fn state(ts: u64, target: &str, pane_id: &str, state: AgentState) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq: None,
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

    fn seed_with(pane_id: &str, name: &str, state: AgentState) -> Box<StatusSeed> {
        Box::new(StatusSeed {
            watched: vec!["main".into()],
            panes: vec![PaneSnapshot {
                pane_id: pane_id.into(),
                name: name.into(),
                state,
            }],
            open: Vec::new(),
            admin_unread: 0,
            mailbox_routes: Vec::new(),
            roster: Vec::new(),
            mailbox: Vec::new(),
        })
    }

    fn admitted_lines(record: &Record) -> Vec<String> {
        let plain = cyclops_ui::Theme::none();
        record
            .admitted_entries()
            .flat_map(|e| e.lines(&plain))
            .collect()
    }

    /// The startup race the projection exists to win: the pane clears WHILE
    /// the daemon's status answer is in flight, so the live transition
    /// reaches the app before the older snapshot does. The seed must
    /// land first and the transition after it — an order swap would
    /// leave the register alarming about a pane that already moved on.
    #[test]
    fn a_live_transition_during_startup_outlives_an_older_status_snapshot() {
        let mut record = Record::new();
        let mut projection = StreamProjectionState::new();

        live(
            &mut record,
            &mut projection,
            state(2_000, "reviewer", "%1", AgentState::Idle),
        );
        status(
            &mut record,
            &mut projection,
            seed_with("%1", "reviewer", AgentState::BlockedPermission),
        );
        assert!(
            admitted_lines(&record).is_empty(),
            "nothing may land before the backfill orders it"
        );

        backfill(&mut record, &mut projection, Vec::new(), None);

        let lines = admitted_lines(&record);
        let last = lines.last().expect("the seed and the clearance landed");
        assert!(
            last.contains("cleared"),
            "the live transition must land after the seed it outran: {lines:#?}"
        );
    }

    /// A ledger-backed event that arrives live during startup is already
    /// in the replayed tail; the seq dedup must drop the live copy or
    /// every startup shows its recent history twice.
    #[test]
    fn a_ledger_backed_duplicate_arriving_live_is_dropped_by_seq() {
        let mut record = Record::new();
        let mut projection = StreamProjectionState::new();

        let mut replayed = state(1_000, "reviewer", "%1", AgentState::BlockedPermission);
        replayed.seq = Some(7);
        let mut duplicate = state(1_000, "reviewer", "%1", AgentState::BlockedPermission);
        duplicate.seq = Some(7);

        live(&mut record, &mut projection, duplicate);
        backfill(&mut record, &mut projection, vec![replayed], Some(7));

        let lines = admitted_lines(&record);
        assert_eq!(lines.len(), 1, "one fact, one row: {lines:#?}");
    }

    #[test]
    fn a_rebuild_returns_its_gap_warning_and_preserves_max_seq() {
        let mut record = Record::new();
        let mut projection = StreamProjectionState::new();
        let warning = "backfill incomplete; stream history has a gap".to_string();

        let returned = install(
            &mut record,
            &mut projection,
            Bootstrap {
                seed: None,
                entries: Vec::new(),
                max_seq: Some(7),
                warning: Some(warning.clone()),
            },
        );
        assert_eq!(returned, Some(warning));

        let mut duplicate = state(1_000, "reviewer", "%1", AgentState::BlockedPermission);
        duplicate.seq = Some(7);
        live(&mut record, &mut projection, duplicate);
        assert!(admitted_lines(&record).is_empty());

        let mut newer = state(2_000, "reviewer", "%1", AgentState::BlockedPermission);
        newer.seq = Some(8);
        live(&mut record, &mut projection, newer);
        assert_eq!(admitted_lines(&record).len(), 1);
    }
}
