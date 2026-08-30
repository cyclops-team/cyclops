//! Explicit boundary for retained pre-workspace messaging behavior.
//!
//! This module does not declare these paths publicly supported and does not
//! promise indefinite compatibility. It makes the currently retained boundary
//! visible: direct in-process delivery, restart settlement for direct delivery
//! chains, and session-journal replay all enter through here. Every readable
//! format remains readable; deletion needs a separate caller and history
//! decision.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cyclops_proto::{LedgerLine, MsgSendParams, WireError};
use cyclops_state::StateRoot;
use serde_json::Value;
use tracing::{error, warn};

use crate::{delivery, Inner, SessionSlot};

/// Capability proving that a direct-delivery writer entered through this
/// compatibility boundary. The field and the only value stay private here, so
/// new callers cannot reach the retained writer or restart settlement by
/// accident.
pub(crate) struct BoundaryToken(());

const BOUNDARY: BoundaryToken = BoundaryToken(());

/// Compatibility-sensitive in-process delivery used by self-test, repository
/// tests, and possible external embedders. Public support status is unverified.
pub(crate) async fn deliver_payload(
    inner: &Arc<Inner>,
    from: &str,
    params: MsgSendParams,
) -> Result<Value, WireError> {
    delivery::msg_send(inner, from, params, &BOUNDARY).await
}

/// Settle direct-delivery chains left open by a prior daemon run.
pub(crate) fn recover_direct_deliveries(inner: &Arc<Inner>, replayed: &[(usize, Vec<LedgerLine>)]) {
    delivery::close_limbo(inner, replayed, &BOUNDARY);
}

pub(crate) struct SessionJournalReplay {
    /// Globally unique files for id preload and the history read model.
    pub(crate) files: Vec<(String, Vec<LedgerLine>)>,
    /// One causally ordered stream per unambiguous configured root.
    pub(crate) recovery: Vec<(usize, Vec<LedgerLine>)>,
    /// Sources named by the retained history graph that were not readable.
    pub(crate) unreadable_sources: usize,
}

pub(crate) struct HistorySources {
    pub(crate) files: Vec<(String, Vec<LedgerLine>)>,
    pub(crate) unreadable_sources: usize,
}

struct SessionJournalNode {
    journal: String,
    lines: Vec<LedgerLine>,
    links: Vec<String>,
    owners: BTreeSet<usize>,
}

/// Return retained session-journal sources for the history read model.
///
/// The source label is intentionally part of the compatibility boundary so a
/// current reader does not need to know how old session journals are named or
/// linked.
pub(crate) fn history_sources(inner: &Inner) -> Vec<(String, Vec<LedgerLine>)> {
    history_sources_report(inner).files
}

/// The retained history sources plus explicit read loss for presentation.
///
/// Ordinary history keeps its compatibility behavior, while a bounded UI
/// projection can tell the operator that a named source was unreadable.
pub(crate) fn history_sources_report(inner: &Inner) -> HistorySources {
    let mut unreadable_sources = 0;
    let roots = inner
        .session_slots()
        .into_iter()
        .enumerate()
        .map(|(idx, slot)| {
            let (lines, unreadable) = read_session(&slot);
            unreadable_sources += usize::from(unreadable);
            (idx, slot.journal_file_name().unwrap_or_default(), lines)
        })
        .collect();
    let replay = session_journal_replay(&inner.state_root, roots);
    unreadable_sources = unreadable_sources.saturating_add(replay.unreadable_sources);
    let files = replay
        .files
        .into_iter()
        .map(|(journal, lines)| (format!("session-journal:{journal}"), lines))
        .collect();
    HistorySources {
        files,
        unreadable_sources,
    }
}

/// Root session journals plus every rename-linked journal they reach.
///
/// History and boot recovery share this traversal so a linked compatibility
/// chain cannot be visible to readers while remaining invisible to id preload
/// or restart settlement. History sees each file once. Recovery sees each
/// unambiguous family descendants-first and its configured root last, so a
/// terminal fact appended to the root on an earlier boot follows the linked
/// message that fact closes.
pub(crate) fn session_journal_replay(
    state_root: &StateRoot,
    roots: Vec<(usize, String, Vec<LedgerLine>)>,
) -> SessionJournalReplay {
    let mut unreadable_sources = 0;
    let mut nodes = Vec::<SessionJournalNode>::new();
    let mut by_journal = HashMap::<String, usize>::new();
    let mut root_nodes = Vec::new();
    let mut pending = VecDeque::<(usize, usize)>::new();
    for (idx, journal, lines) in roots {
        let node_idx = match by_journal.get(&journal).copied() {
            Some(node_idx) => node_idx,
            None => {
                let node_idx = nodes.len();
                by_journal.insert(journal.clone(), node_idx);
                nodes.push(SessionJournalNode {
                    links: linked_session_journals(&lines),
                    journal,
                    lines,
                    owners: BTreeSet::new(),
                });
                node_idx
            }
        };
        root_nodes.push((idx, node_idx));
        if nodes[node_idx].owners.insert(idx) {
            pending.push_back((node_idx, idx));
        }
    }

    // The queue is one (journal, configured owner) pair. A cycle can revisit
    // a file, but it cannot add the same owner twice, so traversal is bounded
    // by files times configured roots.
    while let Some((node_idx, owner)) = pending.pop_front() {
        let links = nodes[node_idx].links.clone();
        for linked in links {
            let linked_idx = match by_journal.get(&linked).copied() {
                Some(linked_idx) => linked_idx,
                None => {
                    let descendant = PathBuf::from("ledger").join(&linked);
                    let lines = match cyclops_ledger::read_after(state_root, &descendant, 0) {
                        Ok(lines) => lines,
                        Err(read_error) => {
                            warn!(journal = linked, error = %read_error, "linked session journal is unreadable");
                            unreadable_sources += 1;
                            Vec::new()
                        }
                    };
                    let linked_idx = nodes.len();
                    by_journal.insert(linked.clone(), linked_idx);
                    nodes.push(SessionJournalNode {
                        links: linked_session_journals(&lines),
                        journal: linked,
                        lines,
                        owners: BTreeSet::new(),
                    });
                    linked_idx
                }
            };
            if nodes[linked_idx].owners.insert(owner) {
                pending.push_back((linked_idx, owner));
            }
        }
    }

    for node in &nodes {
        if node.owners.len() > 1 {
            error!(
                journal = node.journal,
                owners = ?node.owners,
                "linked session journal has more than one configured owner; restart recovery will leave it untouched"
            );
        }
    }

    fn collect_descendants_first(
        node_idx: usize,
        owner: usize,
        nodes: &[SessionJournalNode],
        by_journal: &HashMap<String, usize>,
        visited: &mut HashSet<usize>,
        order: &mut Vec<usize>,
    ) {
        if !visited.insert(node_idx)
            || nodes[node_idx].owners.len() != 1
            || !nodes[node_idx].owners.contains(&owner)
        {
            return;
        }
        for linked in &nodes[node_idx].links {
            if let Some(linked_idx) = by_journal.get(linked).copied() {
                collect_descendants_first(linked_idx, owner, nodes, by_journal, visited, order);
            }
        }
        order.push(node_idx);
    }

    let mut recovery = Vec::new();
    let mut history_order = Vec::new();
    let mut history_seen = HashSet::new();
    for (owner, root_idx) in root_nodes {
        let mut visited = HashSet::new();
        let mut family = Vec::new();
        collect_descendants_first(
            root_idx,
            owner,
            &nodes,
            &by_journal,
            &mut visited,
            &mut family,
        );
        let lines = family
            .iter()
            .flat_map(|node_idx| nodes[*node_idx].lines.iter().cloned())
            .collect();
        for node_idx in family {
            if history_seen.insert(node_idx) {
                history_order.push(node_idx);
            }
        }
        recovery.push((owner, lines));
    }
    // Ambiguously owned or otherwise unreachable nodes stay visible to
    // history and id preload, but follow the unambiguous causal families.
    for node_idx in 0..nodes.len() {
        if history_seen.insert(node_idx) {
            history_order.push(node_idx);
        }
    }
    let mut nodes = nodes.into_iter().map(Some).collect::<Vec<_>>();
    let files = history_order
        .into_iter()
        .map(|node_idx| {
            let node = nodes[node_idx].take().expect("journal emitted once");
            (node.journal, node.lines)
        })
        .collect();
    SessionJournalReplay {
        files,
        recovery,
        unreadable_sources,
    }
}

fn linked_session_journals(lines: &[LedgerLine]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| line.data.as_ref())
        .filter(|data| data["event"] == "session_slot_aliased")
        .filter_map(|data| data["canonical_journal"].as_str())
        .filter_map(|file| {
            if valid_session_journal_file(file) {
                Some(file.to_owned())
            } else {
                warn!(journal = file, "invalid linked session journal was ignored");
                None
            }
        })
        .collect()
}

fn valid_session_journal_file(file: &str) -> bool {
    let path = Path::new(file);
    matches!(
        path.components().collect::<Vec<_>>().as_slice(),
        [Component::Normal(_)]
    ) && path
        .extension()
        .is_some_and(|extension| extension == "ndjson")
}

fn read_session(slot: &SessionSlot) -> (Vec<LedgerLine>, bool) {
    match slot.ledger.read_after(0) {
        Ok(lines) => (lines, false),
        Err(error) => {
            warn!(session = %slot.name(), error = %error, "compatibility history read failed; treating as empty");
            (Vec::new(), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use cyclops_proto::Kind;
    use serde_json::json;

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            Self(cyclops_proto::scratch::scratch_dir(&format!(
                "compatibility-{tag}-{}",
                uuid::Uuid::new_v4().simple()
            )))
        }

        fn root(&self) -> StateRoot {
            StateRoot::open_or_create(&self.0).expect("state root opens")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn line(id: &str) -> LedgerLine {
        LedgerLine {
            seq: 1,
            boot_id: "boot".into(),
            id: id.into(),
            ts: 1,
            kind: Kind::System,
            from: "cyclopsd".into(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: None,
        }
    }

    fn link(id: &str, journal: &str) -> LedgerLine {
        let mut line = line(id);
        line.data = Some(json!({
            "event": "session_slot_aliased",
            "canonical_journal": journal,
        }));
        line
    }

    fn write_journal(root: &StateRoot, journal: &str, lines: &[LedgerLine]) {
        let relative = Path::new("ledger").join(journal);
        let mut file = root.open_append(&relative).expect("journal opens");
        for line in lines {
            serde_json::to_writer(&mut file, line).expect("line writes");
            writeln!(file).expect("newline writes");
        }
        file.flush().expect("journal flushes");
    }

    #[test]
    fn rename_linked_history_is_unique_and_recovery_is_descendants_first() {
        let scratch = Scratch::new("linked");
        let root = scratch.root();
        write_journal(&root, "child.ndjson", &[line("child")]);

        let replay = session_journal_replay(
            &root,
            vec![(0, "root.ndjson".into(), vec![link("root", "child.ndjson")])],
        );

        assert_eq!(
            replay
                .files
                .iter()
                .map(|(journal, _)| journal.as_str())
                .collect::<Vec<_>>(),
            ["child.ndjson", "root.ndjson"]
        );
        assert_eq!(
            replay.recovery[0]
                .1
                .iter()
                .map(|line| line.id.as_str())
                .collect::<Vec<_>>(),
            ["child", "root"]
        );
    }

    #[test]
    fn ambiguously_owned_history_stays_readable_but_cannot_drive_recovery() {
        let scratch = Scratch::new("ambiguous");
        let root = scratch.root();
        write_journal(&root, "shared.ndjson", &[line("shared")]);

        let replay = session_journal_replay(
            &root,
            vec![
                (
                    0,
                    "first.ndjson".into(),
                    vec![link("first", "shared.ndjson")],
                ),
                (
                    1,
                    "second.ndjson".into(),
                    vec![link("second", "shared.ndjson")],
                ),
            ],
        );

        assert!(replay.files.iter().any(|(journal, lines)| {
            journal == "shared.ndjson" && lines.iter().any(|line| line.id == "shared")
        }));
        assert!(replay
            .recovery
            .iter()
            .all(|(_, lines)| lines.iter().all(|line| line.id != "shared")));
    }
}
