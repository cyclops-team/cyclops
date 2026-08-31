//! Reusable stream projections received from the daemon.
//!
//! The watch adapter fetches these values. Presentation owns how a bounded
//! backfill becomes ordered entries and how an explicit gap stays visible.

use cyclops_proto::StreamBackfillResult;

use crate::stream::{Entry, StatusSeed};

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
