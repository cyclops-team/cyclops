//! Reusable stream projections received from the daemon.
//!
//! The watch adapter fetches these values. Presentation owns how a bounded
//! backfill becomes ordered entries and how an explicit gap stays visible.

use cyclops_proto::StreamBackfillResult;

use crate::stream::{Entry, Intake, StatusSeed};

/// Everything needed to replace the reusable stream model after an
/// acknowledged subscription. Missing pieces remain explicit in `warning`.
pub struct StreamProjection {
    pub seed: Option<Box<StatusSeed>>,
    pub entries: Vec<Entry>,
    pub max_seq: Option<u64>,
    pub warning: Option<String>,
}

/// A bounded daemon projection plus any explicitly reported loss.
#[derive(Debug)]
pub struct BackfillReport {
    /// Entries retained in timestamp order.
    pub entries: Vec<Entry>,
    /// Highest retained sequence when exactly one file supplied the tail.
    pub max_seq: Option<u64>,
    /// Visible gap text when requested history could not be represented whole.
    pub warning: Option<String>,
}

/// One authorized fact that can change a stream presentation.
///
/// The transport adapter decides when each fact arrives. This model decides
/// only how facts from one connection epoch become ordered presentation
/// updates; it does not know how a daemon, ledger, or terminal produced them.
pub enum StreamInput {
    /// A pushed transition from the live subscription.
    Live(Entry),
    /// The daemon's current status answer.
    Status(Box<StatusSeed>),
    /// A bounded retained tail from the daemon-owned source set.
    Backfill {
        entries: Vec<Entry>,
        max_seq: Option<u64>,
    },
}

/// One ordered update for a stream renderer or its application-owned adapter.
///
/// History, current status, and live transitions remain separate because they
/// have different authority and age. A renderer may paint them differently,
/// but it must apply them in this order.
pub enum StreamUpdate {
    /// A durable history row. It never changes current attention on its own.
    Replay(Entry),
    /// The daemon's authoritative answer about current state.
    Status(Box<StatusSeed>),
    /// A live transition after the retained history and current answer.
    Live(Entry),
    /// A visible uncertainty statement from an incomplete bounded read.
    Notice(String),
}

/// Pure connection-epoch ordering and duplicate suppression for a stream.
///
/// Its small interface gives both watch and workspace the same startup and
/// reconnect contract: history first, current status next, then live facts
/// that arrived while the two were loading. Callers no longer need to know
/// about the pending buffers or the ledger sequence comparison behind it.
#[derive(Default)]
pub struct StreamProjectionState {
    intake: Intake,
}

impl StreamProjectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once an authoritative bounded history has established this
    /// connection epoch's ordering and duplicate cursor.
    pub fn is_backfilled(&self) -> bool {
        self.intake.is_backfilled()
    }

    /// Apply one authorized fact and return the presentation updates it
    /// makes ready. Before history arrives, live and status facts stay
    /// buffered; after it, live facts flow through directly.
    pub fn apply(&mut self, input: StreamInput) -> Vec<StreamUpdate> {
        match input {
            StreamInput::Live(entry) => self
                .intake
                .entry(entry)
                .into_iter()
                .map(StreamUpdate::Live)
                .collect(),
            StreamInput::Status(seed) => self
                .intake
                .status(seed)
                .into_iter()
                .map(StreamUpdate::Status)
                .collect(),
            StreamInput::Backfill { entries, max_seq } => {
                let landed = self.intake.backfill(entries, max_seq);
                let mut updates = landed
                    .replayed
                    .into_iter()
                    .map(StreamUpdate::Replay)
                    .collect::<Vec<_>>();
                if let Some(seed) = landed.seed {
                    updates.push(StreamUpdate::Status(seed));
                }
                updates.extend(landed.live.into_iter().map(StreamUpdate::Live));
                updates
            }
        }
    }

    /// Replace the connection epoch from one acknowledged stream snapshot.
    ///
    /// Pending facts from an earlier epoch cannot be safely combined with a
    /// new daemon snapshot, so this intentionally starts a fresh ordering
    /// state before applying the snapshot's status and retained history.
    pub fn replace(&mut self, snapshot: StreamProjection) -> Vec<StreamUpdate> {
        self.intake = Intake::new();
        let StreamProjection {
            seed,
            entries,
            max_seq,
            warning,
        } = snapshot;
        let mut updates = Vec::new();
        if let Some(seed) = seed {
            updates.extend(self.apply(StreamInput::Status(seed)));
        }
        updates.extend(self.apply(StreamInput::Backfill { entries, max_seq }));
        if let Some(warning) = warning {
            updates.push(StreamUpdate::Notice(warning));
        }
        updates
    }
}

/// Convert the daemon-owned read projection into renderer-neutral entries.
pub fn project_backfill(result: StreamBackfillResult) -> BackfillReport {
    let entries = result.lines.iter().filter_map(Entry::from_ledger).collect();
    let warning = result.gap.filter(|gap| !gap.is_empty()).map(|gap| {
        let mut facts = Vec::new();
        if gap.unreadable_sources > 0 {
            facts.push(format!("{} unreadable sources", gap.unreadable_sources));
        }
        if gap.omitted_rows > 0 {
            facts.push(format!("{} rows beyond the retained limits", gap.omitted_rows));
        }
        format!(
            "backfill incomplete; stream history has a gap: {}. Use cyclops history for the durable record",
            facts.join(", ")
        )
    });
    BackfillReport {
        entries,
        max_seq: result.max_seq,
        warning,
    }
}

#[cfg(test)]
mod tests {
    use cyclops_proto::AgentState;

    use super::*;
    use crate::stream::EntryKind;

    fn entry(ts: u64, seq: Option<u64>) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq,
            id: None,
            kind: EntryKind::State {
                target: "reviewer".into(),
                recipient: None,
                session_idx: 0,
                pane_id: Some("%1".into()),
                state: AgentState::Idle,
            },
        }
    }

    /// The original startup regression: a live clearance can arrive before
    /// the older status answer. The projection must make the current answer
    /// visible before that live fact, even though the caller supplied it first.
    #[test]
    fn a_live_fact_waits_behind_history_and_current_status() {
        let mut projection = StreamProjectionState::new();
        assert!(projection
            .apply(StreamInput::Live(entry(3_000, Some(8))))
            .is_empty());
        assert!(projection
            .apply(StreamInput::Status(Box::default()))
            .is_empty());

        let updates = projection.apply(StreamInput::Backfill {
            entries: vec![entry(1_000, Some(7))],
            max_seq: Some(7),
        });

        assert!(matches!(updates.first(), Some(StreamUpdate::Replay(_))));
        assert!(matches!(updates.get(1), Some(StreamUpdate::Status(_))));
        assert!(matches!(updates.get(2), Some(StreamUpdate::Live(_))));
        assert!(projection.is_backfilled());
    }

    #[test]
    fn a_replacement_exposes_its_gap_only_after_the_authorized_facts() {
        let mut projection = StreamProjectionState::new();
        let updates = projection.replace(StreamProjection {
            seed: Some(Box::new(StatusSeed::default())),
            entries: vec![entry(1_000, Some(7))],
            max_seq: Some(7),
            warning: Some("backfill incomplete".into()),
        });

        assert!(matches!(updates.first(), Some(StreamUpdate::Replay(_))));
        assert!(matches!(updates.get(1), Some(StreamUpdate::Status(_))));
        assert!(
            matches!(updates.last(), Some(StreamUpdate::Notice(text)) if text == "backfill incomplete")
        );
    }
}
