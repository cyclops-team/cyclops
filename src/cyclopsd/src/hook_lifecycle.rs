//! Candidate lifecycle edges that need later terminal evidence.
//!
//! Some vendors invoke every matching hook concurrently. A hook process can
//! therefore report a prompt start or stop before the vendor knows whether a
//! sibling hook will accept or block it. Those reports are dispatch facts, not
//! runtime state. This store keeps them bound to one process generation and
//! manifest until a later watcher event supplies visual evidence.

use std::collections::{HashMap, HashSet};

use crate::{identity::ProcId, turnkey::TurnKey, PaneKey};

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    generation: u64,
    pub(crate) agent: ProcId,
    pub(crate) manifest: String,
    pub(crate) turn: TurnKey,
    pub(crate) event: String,
    pub(crate) edge_ms: u64,
    pub(crate) ready_at_ms: u64,
    visual_revision: u64,
}

pub(crate) struct TerminalSettlement {
    pub(crate) end: Candidate,
    pub(crate) start: Option<Candidate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TerminalKind {
    End,
    VisualEnd,
}

#[derive(Debug, Default)]
struct Pending {
    starts: HashMap<TurnKey, Candidate>,
    ends: HashMap<TurnKey, Candidate>,
    visual_ends: HashMap<TurnKey, Candidate>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.starts.is_empty() && self.ends.is_empty() && self.visual_ends.is_empty()
    }

    fn binding_differs(&self, agent: ProcId, manifest: &str) -> bool {
        self.starts
            .values()
            .chain(self.ends.values())
            .chain(self.visual_ends.values())
            .any(|edge| edge.agent != agent || edge.manifest != manifest)
    }
}

#[derive(Debug, Default)]
pub(crate) struct Store {
    pending: HashMap<PaneKey, Pending>,
    visual_revisions: HashMap<PaneKey, u64>,
    next_generation: u64,
}

impl Store {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn allocate_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.next_generation
    }

    /// Record a completed visual observation. The caller timestamps the
    /// observation before capture, so a hook edge that arrives during capture
    /// cannot be confirmed by those older pixels.
    pub(crate) fn note_visual_change(&mut self, pane: &PaneKey) {
        let revision = self.visual_revisions.entry(pane.clone()).or_default();
        *revision = revision.saturating_add(1);
    }

    pub(crate) fn has_pending_for(&self, pane: &PaneKey, agent: ProcId, manifest: &str) -> bool {
        self.pending.get(pane).is_some_and(|pending| {
            pending
                .starts
                .values()
                .chain(pending.ends.values())
                .chain(pending.visual_ends.values())
                .any(|edge| edge.agent == agent && edge.manifest == manifest)
        })
    }

    pub(crate) fn has_terminal_candidates(&self, pane: &PaneKey) -> bool {
        self.pending
            .get(pane)
            .is_some_and(|pending| !pending.ends.is_empty() || !pending.visual_ends.is_empty())
    }

    #[cfg(test)]
    pub(crate) fn next_terminal_recheck(
        &self,
        pane: &PaneKey,
    ) -> Option<(TerminalKind, Candidate)> {
        self.next_terminal_recheck_excluding(pane, &HashSet::new())
    }

    pub(crate) fn next_terminal_recheck_excluding(
        &self,
        pane: &PaneKey,
        attempted: &HashSet<(TerminalKind, u64)>,
    ) -> Option<(TerminalKind, Candidate)> {
        let pending = self.pending.get(pane)?;
        pending
            .ends
            .values()
            .map(|edge| (TerminalKind::End, edge))
            .chain(
                pending
                    .visual_ends
                    .values()
                    .map(|edge| (TerminalKind::VisualEnd, edge)),
            )
            .filter(|(kind, edge)| !attempted.contains(&(*kind, edge.generation)))
            .min_by_key(|(kind, edge)| {
                (
                    edge.ready_at_ms,
                    edge.edge_ms,
                    *kind,
                    edge.turn.dedupe_key(""),
                )
            })
            .map(|(kind, edge)| (kind, edge.clone()))
    }

    pub(crate) fn terminal_recheck_key(
        kind: TerminalKind,
        candidate: &Candidate,
    ) -> (TerminalKind, u64) {
        (kind, candidate.generation)
    }

    pub(crate) fn terminal_candidate_is_current(
        &self,
        pane: &PaneKey,
        kind: TerminalKind,
        candidate: &Candidate,
    ) -> bool {
        let Some(pending) = self.pending.get(pane) else {
            return false;
        };
        let current = match kind {
            TerminalKind::End => pending.ends.get(&candidate.turn),
            TerminalKind::VisualEnd => pending.visual_ends.get(&candidate.turn),
        };
        current.is_some_and(|current| same_candidate(current, candidate))
    }

    pub(crate) fn record_start(
        &mut self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        turn: TurnKey,
        event: &str,
        edge_ms: u64,
    ) {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let generation = self.allocate_generation();
        let pending = self.pending.entry(pane.clone()).or_default();
        if pending.binding_differs(agent, manifest) {
            *pending = Pending::default();
        }
        let candidate = Candidate {
            generation,
            agent,
            manifest: manifest.to_string(),
            turn: turn.clone(),
            event: event.to_string(),
            edge_ms,
            ready_at_ms: edge_ms,
            visual_revision: revision,
        };
        pending.starts.insert(turn.clone(), candidate);
        pending.visual_ends.remove(&turn);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_end(
        &mut self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        turn: TurnKey,
        event: &str,
        edge_ms: u64,
        settle_ms: u64,
    ) -> Candidate {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let generation = self.allocate_generation();
        let pending = self.pending.entry(pane.clone()).or_default();
        if pending.binding_differs(agent, manifest) {
            *pending = Pending::default();
        }
        pending.visual_ends.remove(&turn);
        let candidate = Candidate {
            generation,
            agent,
            manifest: manifest.to_string(),
            turn,
            event: event.to_string(),
            edge_ms,
            ready_at_ms: edge_ms.saturating_add(settle_ms),
            visual_revision: revision,
        };
        pending
            .ends
            .insert(candidate.turn.clone(), candidate.clone());
        candidate
    }

    /// Record the first stable terminal frame for an active exact turn.
    /// Returns true only when the caller must arm the one-shot recheck.
    #[cfg(test)]
    pub(crate) fn record_visual_end(
        &mut self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        turn: TurnKey,
        edge_ms: u64,
        settle_ms: u64,
    ) -> Option<Candidate> {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let generation = self.allocate_generation();
        let pending = self.pending.entry(pane.clone()).or_default();
        if pending.binding_differs(agent, manifest) {
            *pending = Pending::default();
        }
        if pending.visual_ends.contains_key(&turn) {
            return None;
        }
        let candidate = Candidate {
            generation,
            agent,
            manifest: manifest.to_string(),
            turn,
            event: "visual_terminal".to_string(),
            edge_ms,
            ready_at_ms: edge_ms.saturating_add(settle_ms),
            visual_revision: revision,
        };
        pending
            .visual_ends
            .insert(candidate.turn.clone(), candidate.clone());
        Some(candidate)
    }

    #[cfg(test)]
    pub(crate) fn end_is_current(&self, pane: &PaneKey, candidate: &Candidate) -> bool {
        self.pending
            .get(pane)
            .and_then(|pending| pending.ends.get(&candidate.turn))
            .is_some_and(|current| same_candidate(current, candidate))
    }

    #[cfg(test)]
    pub(crate) fn end_observation_revision(
        &self,
        pane: &PaneKey,
        candidate: &Candidate,
    ) -> Option<u64> {
        self.pending
            .get(pane)
            .and_then(|pending| pending.ends.get(&candidate.turn))
            .filter(|current| same_candidate(current, candidate))
            .map(|current| current.visual_revision)
    }

    #[cfg(test)]
    pub(crate) fn visual_end_is_current(&self, pane: &PaneKey, candidate: &Candidate) -> bool {
        self.pending
            .get(pane)
            .and_then(|pending| pending.visual_ends.get(&candidate.turn))
            .is_some_and(|current| same_candidate(current, candidate))
    }

    pub(crate) fn clear_visual_end(
        &mut self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) {
        if let Some(pending) = self.pending.get_mut(pane) {
            if pending
                .visual_ends
                .get(turn)
                .is_some_and(|edge| edge.agent == agent && edge.manifest == manifest)
            {
                pending.visual_ends.remove(turn);
            }
            if pending.is_empty() {
                self.pending.remove(pane);
            }
        }
    }

    /// Retire both terminal candidates for one exact turn in one store
    /// transaction. A screen observation can begin before a Stop arrives and
    /// finish after it, leaving both candidates present. Clearing them in
    /// separate lock acquisitions lets that in-flight observation recreate
    /// the visual candidate between the two clears.
    pub(crate) fn clear_terminal_candidates(
        &mut self,
        pane: &PaneKey,
        candidate: &Candidate,
    ) -> bool {
        let mut consumed = false;
        if let Some(pending) = self.pending.get_mut(pane) {
            if pending
                .ends
                .get(&candidate.turn)
                .is_some_and(|edge| same_candidate(edge, candidate))
            {
                pending.ends.remove(&candidate.turn);
                consumed = true;
            }
            if consumed
                && pending
                    .visual_ends
                    .get(&candidate.turn)
                    .is_some_and(|edge| {
                        edge.agent == candidate.agent && edge.manifest == candidate.manifest
                    })
            {
                pending.visual_ends.remove(&candidate.turn);
            }
            if pending.is_empty() {
                self.pending.remove(pane);
            }
        }
        consumed
    }

    /// Consume one exact terminal candidate and the current start for the
    /// same turn in one transaction. The end token is checked first, so a
    /// stale observation cannot settle a replacement Stop. Once it matches,
    /// the exact end proves an out-of-order start for that turn as well.
    pub(crate) fn settle_terminal_candidate(
        &mut self,
        pane: &PaneKey,
        candidate: &Candidate,
    ) -> Option<TerminalSettlement> {
        let pending = self.pending.get_mut(pane)?;
        if !pending
            .ends
            .get(&candidate.turn)
            .is_some_and(|edge| same_candidate(edge, candidate))
        {
            return None;
        }
        pending.ends.remove(&candidate.turn);
        if pending
            .visual_ends
            .get(&candidate.turn)
            .is_some_and(|edge| {
                edge.agent == candidate.agent && edge.manifest == candidate.manifest
            })
        {
            pending.visual_ends.remove(&candidate.turn);
        }
        let start = pending
            .starts
            .get(&candidate.turn)
            .is_some_and(|edge| {
                edge.agent == candidate.agent && edge.manifest == candidate.manifest
            })
            .then(|| pending.starts.remove(&candidate.turn))
            .flatten();
        if pending.is_empty() {
            self.pending.remove(pane);
        }
        Some(TerminalSettlement {
            end: candidate.clone(),
            start,
        })
    }

    /// Select and consume the next ready exact Stop in one transaction.
    ///
    /// A replacement Stop can reuse the same turn key with a new generation.
    /// Keeping selection and removal under one store lock prevents a stale
    /// selection from spinning against repeated replacements.
    pub(crate) fn settle_next_ready_terminal(
        &mut self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        preferred: Option<&TurnKey>,
        observed_ms: u64,
    ) -> Option<TerminalSettlement> {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let pending = self.pending.get(pane)?;
        let eligible = |edge: &&Candidate| {
            edge.agent == agent
                && edge.manifest == manifest
                && revision > edge.visual_revision
                && edge.edge_ms < observed_ms
                && edge.ready_at_ms <= observed_ms
        };
        let preferred_end = preferred
            .and_then(|turn| pending.ends.get(turn))
            .filter(eligible)
            .cloned();
        let end = preferred_end.or_else(|| {
            pending
                .ends
                .values()
                .filter(eligible)
                .min_by_key(|edge| (edge.edge_ms, edge.turn.dedupe_key("")))
                .cloned()
        })?;
        self.settle_terminal_candidate(pane, &end)
    }

    /// Candidate start visible to this later watcher revision and binding.
    pub(crate) fn start_for(
        &self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
    ) -> Option<Candidate> {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let mut matches = self.pending.get(pane)?.starts.values().filter(|edge| {
            edge.agent == agent && edge.manifest == manifest && revision > edge.visual_revision
        });
        let candidate = matches.next()?.clone();
        matches.next().is_none().then_some(candidate)
    }

    pub(crate) fn has_other_start_for(
        &self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) -> bool {
        self.pending.get(pane).is_some_and(|pending| {
            pending
                .starts
                .values()
                .any(|edge| edge.agent == agent && edge.manifest == manifest && &edge.turn != turn)
        })
    }

    /// Candidate end visible to this later watcher revision and binding.
    pub(crate) fn end_for(
        &self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        preferred: Option<&TurnKey>,
    ) -> Option<Candidate> {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let pending = self.pending.get(pane)?;
        let eligible = |edge: &&Candidate| {
            edge.agent == agent && edge.manifest == manifest && revision > edge.visual_revision
        };
        if let Some(turn) = preferred {
            return pending.ends.get(turn).filter(eligible).cloned();
        }
        pending
            .ends
            .values()
            .filter(eligible)
            .min_by_key(|edge| (edge.edge_ms, edge.turn.dedupe_key("")))
            .cloned()
    }

    /// Take a pending start when a conclusive lifecycle end proves that exact
    /// turn existed. Candidate ends do not use this path because a concurrent
    /// hook may still keep the vendor turn alive.
    pub(crate) fn take_start_for_turn(
        &mut self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) -> Option<Candidate> {
        let pending = self.pending.get_mut(pane)?;
        let matches = pending
            .starts
            .get(turn)
            .is_some_and(|edge| edge.agent == agent && edge.manifest == manifest);
        let start = matches.then(|| pending.starts.remove(turn)).flatten();
        if pending.is_empty() {
            self.pending.remove(pane);
        }
        start
    }

    pub(crate) fn clear_start(&mut self, pane: &PaneKey, candidate: &Candidate) -> bool {
        let mut consumed = false;
        if let Some(pending) = self.pending.get_mut(pane) {
            if pending
                .starts
                .get(&candidate.turn)
                .is_some_and(|edge| same_candidate(edge, candidate))
            {
                pending.starts.remove(&candidate.turn);
                consumed = true;
            }
            if pending.is_empty() {
                self.pending.remove(pane);
            }
        }
        consumed
    }

    pub(crate) fn defer_start(&mut self, pane: &PaneKey, candidate: &Candidate) -> bool {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let Some(edge) = self
            .pending
            .get_mut(pane)
            .and_then(|pending| pending.starts.get_mut(&candidate.turn))
            .filter(|edge| same_candidate(edge, candidate))
        else {
            return false;
        };
        edge.visual_revision = revision;
        true
    }

    pub(crate) fn clear_end(&mut self, pane: &PaneKey, turn: &TurnKey) {
        if let Some(pending) = self.pending.get_mut(pane) {
            pending.ends.remove(turn);
            if pending.is_empty() {
                self.pending.remove(pane);
            }
        }
    }

    pub(crate) fn defer_end(&mut self, pane: &PaneKey, candidate: &Candidate) -> bool {
        let revision = self.visual_revisions.get(pane).copied().unwrap_or_default();
        let Some(edge) = self
            .pending
            .get_mut(pane)
            .and_then(|pending| pending.ends.get_mut(&candidate.turn))
            .filter(|edge| same_candidate(edge, candidate))
        else {
            return false;
        };
        edge.visual_revision = revision;
        true
    }

    pub(crate) fn forget(&mut self, pane: &PaneKey) {
        self.pending.remove(pane);
        self.visual_revisions.remove(pane);
    }

    /// Retire a bucket only when it belongs to an older process or manifest.
    /// Candidate ingress can establish the new binding before fusion retires
    /// its cached predecessor, so unconditional retirement would discard the
    /// first valid edge from the replacement.
    pub(crate) fn forget_if_binding_differs(
        &mut self,
        pane: &PaneKey,
        agent: ProcId,
        manifest: &str,
    ) -> bool {
        let differs = self
            .pending
            .get(pane)
            .is_some_and(|pending| pending.binding_differs(agent, manifest));
        if differs {
            self.forget(pane);
        }
        differs
    }
}

fn same_candidate(left: &Candidate, right: &Candidate) -> bool {
    left.generation == right.generation
        && left.agent == right.agent
        && left.manifest == right.manifest
        && left.turn == right.turn
        && left.edge_ms == right.edge_ms
        && left.ready_at_ms == right.ready_at_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pid: i32) -> ProcId {
        ProcId { pid, birth: 7 }
    }

    #[test]
    fn candidates_require_a_later_visual_revision_and_exact_binding() {
        let pane = PaneKey::new(0, "%1");
        let turn = TurnKey::for_test(&["session", "prompt"]);
        let mut store = Store::new();
        store.record_start(&pane, agent(10), "claude", turn.clone(), "start", 5);
        assert!(store.start_for(&pane, agent(10), "claude").is_none());
        store.note_visual_change(&pane);
        assert!(store.start_for(&pane, agent(11), "claude").is_none());
        assert!(store.start_for(&pane, agent(10), "other").is_none());
        assert_eq!(
            store
                .start_for(&pane, agent(10), "claude")
                .expect("later exact observation")
                .turn,
            turn
        );
    }

    #[test]
    fn exact_starts_coexist_and_ambiguous_visual_evidence_confirms_neither() {
        let pane = PaneKey::new(0, "%1");
        let first = TurnKey::for_test(&["session", "first"]);
        let second = TurnKey::for_test(&["session", "second"]);
        let mut store = Store::new();
        store.record_start(&pane, agent(10), "claude", first.clone(), "start", 5);
        store.record_start(&pane, agent(10), "claude", second.clone(), "start", 6);
        store.note_visual_change(&pane);

        assert!(store.start_for(&pane, agent(10), "claude").is_none());
        assert_eq!(
            store
                .take_start_for_turn(&pane, agent(10), "claude", &first)
                .expect("exact first candidate")
                .turn,
            first
        );
        assert_eq!(
            store
                .start_for(&pane, agent(10), "claude")
                .expect("remaining unambiguous candidate")
                .turn,
            second
        );
    }

    #[test]
    fn a_visual_end_is_cleared_only_by_its_exact_turn() {
        let pane = PaneKey::new(0, "%1");
        let first = TurnKey::for_test(&["session", "first"]);
        let second = TurnKey::for_test(&["session", "second"]);
        let mut store = Store::new();
        let candidate = store
            .record_visual_end(&pane, agent(10), "claude", first.clone(), 10, 3_000)
            .expect("candidate recorded");

        store.clear_visual_end(&pane, agent(10), "claude", &second);
        assert!(store.visual_end_is_current(&pane, &candidate));

        store.clear_visual_end(&pane, agent(10), "claude", &first);
        assert!(!store.visual_end_is_current(&pane, &candidate));
    }

    #[test]
    fn another_turn_cannot_cancel_a_visual_end_candidate() {
        let pane = PaneKey::new(0, "%1");
        let first = TurnKey::for_test(&["session", "first"]);
        let second = TurnKey::for_test(&["session", "second"]);
        let mut store = Store::new();
        let candidate = store
            .record_visual_end(&pane, agent(10), "claude", first, 10, 3_000)
            .expect("candidate recorded");

        store.record_start(
            &pane,
            agent(10),
            "claude",
            second.clone(),
            "UserPromptSubmit",
            20,
        );
        assert!(store.visual_end_is_current(&pane, &candidate));

        store.record_end(&pane, agent(10), "claude", second, "Stop", 30, 3_000);
        assert!(store.visual_end_is_current(&pane, &candidate));
    }

    #[test]
    fn a_visual_end_rebinds_the_whole_pane_candidate_bucket() {
        let pane = PaneKey::new(0, "%1");
        let old_turn = TurnKey::for_test(&["old", "turn"]);
        let visual_turn = TurnKey::for_test(&["new", "visual"]);
        let later_turn = TurnKey::for_test(&["new", "start"]);
        let mut store = Store::new();
        store.record_start(&pane, agent(10), "claude", old_turn, "start", 5);
        let visual = store
            .record_visual_end(&pane, agent(11), "claude", visual_turn, 10, 3_000)
            .expect("new binding records visual end");
        store.record_start(&pane, agent(11), "claude", later_turn, "start", 20);

        assert!(!store.has_pending_for(&pane, agent(10), "claude"));
        assert!(store.visual_end_is_current(&pane, &visual));
    }

    #[test]
    fn end_recheck_tokens_remain_bound_to_each_exact_turn() {
        let pane = PaneKey::new(0, "%1");
        let first = TurnKey::for_test(&["session", "first"]);
        let second = TurnKey::for_test(&["session", "second"]);
        let mut store = Store::new();
        let first_candidate =
            store.record_end(&pane, agent(10), "claude", first, "Stop", 10, 3_000);
        store.note_visual_change(&pane);
        store.defer_end(&pane, &first_candidate);
        assert!(store.end_is_current(&pane, &first_candidate));

        let second_candidate =
            store.record_end(&pane, agent(10), "claude", second, "Stop", 20, 3_000);
        assert!(store.end_is_current(&pane, &first_candidate));
        assert!(store.end_is_current(&pane, &second_candidate));

        store.clear_end(&pane, &first_candidate.turn);
        assert!(!store.end_is_current(&pane, &first_candidate));
        assert!(store.end_is_current(&pane, &second_candidate));
    }

    #[test]
    fn a_recheck_retries_until_reconciliation_observes_its_candidate() {
        let pane = PaneKey::new(0, "%1");
        let turn = TurnKey::for_test(&["session", "prompt"]);
        let mut store = Store::new();
        let candidate = store.record_end(&pane, agent(10), "claude", turn.clone(), "Stop", 10, 0);

        let before = store
            .end_observation_revision(&pane, &candidate)
            .expect("candidate current");
        store.note_visual_change(&pane);
        assert_eq!(
            store.end_observation_revision(&pane, &candidate),
            Some(before),
            "capture completion alone must not claim reconciliation",
        );
        store.defer_end(&pane, &candidate);
        assert_ne!(
            store.end_observation_revision(&pane, &candidate),
            Some(before)
        );
        assert!(store.end_is_current(&pane, &candidate));
    }

    #[test]
    fn a_stale_reconcile_token_cannot_mutate_a_same_turn_replacement() {
        let pane = PaneKey::new(0, "%1");
        let turn = TurnKey::for_test(&["session", "prompt"]);
        let mut store = Store::new();
        store.record_start(
            &pane,
            agent(10),
            "claude",
            turn.clone(),
            "UserPromptSubmit",
            10,
        );
        store.note_visual_change(&pane);
        let old_start = store
            .start_for(&pane, agent(10), "claude")
            .expect("old start visible");
        store.record_start(
            &pane,
            agent(10),
            "claude",
            turn.clone(),
            "UserPromptSubmit",
            20,
        );
        store.clear_start(&pane, &old_start);
        store.defer_start(&pane, &old_start);
        store.note_visual_change(&pane);
        assert_eq!(
            store
                .start_for(&pane, agent(10), "claude")
                .expect("replacement start survives")
                .edge_ms,
            20,
        );

        let old_end = store.record_end(&pane, agent(10), "claude", turn.clone(), "Stop", 30, 0);
        let new_end = store.record_end(&pane, agent(10), "claude", turn, "Stop", 40, 0);
        store.clear_terminal_candidates(&pane, &old_end);
        store.defer_end(&pane, &old_end);
        assert!(store.end_is_current(&pane, &new_end));
    }

    #[test]
    fn next_recheck_tracks_the_latest_same_turn_generation() {
        let pane = PaneKey::new(0, "%1");
        let turn = TurnKey::for_test(&["session", "prompt"]);
        let mut store = Store::new();
        let stale = store.record_end(&pane, agent(10), "claude", turn.clone(), "Stop", 10, 0);
        let current = store.record_end(&pane, agent(10), "claude", turn, "Stop", 20, 0);

        let (kind, selected) = store
            .next_terminal_recheck(&pane)
            .expect("replacement remains scheduled");
        assert_eq!(kind, TerminalKind::End);
        assert!(!store.terminal_candidate_is_current(&pane, kind, &stale));
        assert!(store.terminal_candidate_is_current(&pane, kind, &current));
        assert_eq!(selected.edge_ms, 20);
    }

    #[test]
    fn an_unresolved_generation_does_not_hide_another_deadline() {
        let pane = PaneKey::new(0, "%1");
        let mut store = Store::new();
        let first = store.record_end(
            &pane,
            agent(10),
            "claude",
            TurnKey::for_test(&["session", "first"]),
            "Stop",
            10,
            0,
        );
        let second = store.record_end(
            &pane,
            agent(10),
            "claude",
            TurnKey::for_test(&["session", "second"]),
            "Stop",
            20,
            100,
        );
        let attempted = HashSet::from([Store::terminal_recheck_key(TerminalKind::End, &first)]);

        let (kind, selected) = store
            .next_terminal_recheck_excluding(&pane, &attempted)
            .expect("the later candidate remains schedulable");

        assert_eq!(kind, TerminalKind::End);
        assert_eq!(selected.edge_ms, second.edge_ms);
        assert!(store.terminal_candidate_is_current(&pane, kind, &selected));
    }

    #[test]
    fn an_unready_preferred_turn_does_not_block_a_ready_stop() {
        let pane = PaneKey::new(0, "%1");
        let preferred = TurnKey::for_test(&["session", "preferred"]);
        let ready = TurnKey::for_test(&["session", "ready"]);
        let mut store = Store::new();
        let future = store.record_end(
            &pane,
            agent(10),
            "claude",
            preferred.clone(),
            "Stop",
            100,
            1_000,
        );
        store.record_end(&pane, agent(10), "claude", ready.clone(), "Stop", 10, 0);
        store.note_visual_change(&pane);

        let settled = store
            .settle_next_ready_terminal(&pane, agent(10), "claude", Some(&preferred), 200)
            .expect("the ready unrelated turn must settle");

        assert_eq!(settled.end.turn, ready);
        assert!(store.end_is_current(&pane, &future));
    }

    #[test]
    fn ready_terminal_selection_and_consumption_are_one_transaction() {
        let pane = PaneKey::new(0, "%1");
        let first = TurnKey::for_test(&["session", "first"]);
        let second = TurnKey::for_test(&["session", "second"]);
        let mut store = Store::new();
        store.record_end(&pane, agent(10), "claude", first.clone(), "Stop", 10, 0);
        store.record_end(&pane, agent(10), "claude", second.clone(), "Stop", 15, 0);
        store.record_end(&pane, agent(10), "claude", first.clone(), "Stop", 20, 0);
        store.note_visual_change(&pane);

        let preferred = store
            .settle_next_ready_terminal(&pane, agent(10), "claude", Some(&first), 30)
            .expect("active replacement settles exactly");
        assert_eq!(preferred.end.turn, first);
        assert_eq!(preferred.end.edge_ms, 20);

        let remaining = store
            .settle_next_ready_terminal(&pane, agent(10), "claude", None, 30)
            .expect("remaining turn settles next");
        assert_eq!(remaining.end.turn, second);
        assert!(!store.has_terminal_candidates(&pane));
    }

    #[test]
    fn terminal_retirement_clears_coexisting_exact_candidates() {
        let pane = PaneKey::new(0, "%1");
        let turn = TurnKey::for_test(&["session", "prompt"]);
        let mut store = Store::new();
        let end = store.record_end(&pane, agent(10), "claude", turn.clone(), "Stop", 100, 3_000);
        let visual = store
            .record_visual_end(&pane, agent(10), "claude", turn.clone(), 99, 3_000)
            .expect("in-flight visual observation records its candidate");
        assert!(store.end_is_current(&pane, &end));
        assert!(store.visual_end_is_current(&pane, &visual));

        store.clear_terminal_candidates(&pane, &end);

        assert!(!store.end_is_current(&pane, &end));
        assert!(!store.visual_end_is_current(&pane, &visual));
        assert!(!store.has_pending_for(&pane, agent(10), "claude"));
    }
}
