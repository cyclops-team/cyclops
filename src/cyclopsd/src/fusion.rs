//! Sensor fusion: title tier plus screen tier over manifest rules, with
//! output activity as a recompute trigger (never a verdict), and the hook
//! sensor fed by agent.state.report (M1).
//!
//! Tier semantics mirror `Manifest::evaluate`: rules are already sorted by
//! priority, the first match in a region class wins that tier, and the
//! fused verdict is whichever tier winner sits earlier in that same order.
//! When both tiers produced a rule and their states differ, the verdict
//! still goes to the higher-priority rule but the disagreement is exposed
//! on the Detection (GOALS: observable, not an error).
//!
//! Every recompute evaluates the screen tier when the selected manifest has
//! screen rules. A title is current state evidence, but it cannot prove that a
//! permission dialog, quota screen, human draft, or active status row is
//! absent. Output events remain coalesced by the watcher, so this policy adds
//! one bounded capture per recompute rather than a polling loop.
//!
//! Only the screen sensor can prove composer readiness. A verdict without a
//! positive clean-composer reading still refuses writes under rule 12.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use cyclops_manifest::{
    strip_csi, AckEvidence, CompiledRule, LifecycleCertainty, Manifest, Region,
};
use cyclops_proto::{
    AgentState, ComposerHold, ComposerProof, ComposerSemantic, ComposerState, Detection,
    NotificationAttemptId, NotificationAttentionCause, NotificationRouteEvidenceId,
    NotificationState, ProcessInstanceId, RecipientKey, Sensor, SensorReading,
};
#[cfg(test)]
use cyclops_proto::{NotificationTransport, DOORBELL_FORMAT_COMPACT_CLAIM};
use cyclops_tmux::{PaneRow, SessionWatcher, TmuxError};
use tracing::debug;

use crate::{turnkey, unix_ms, ComposerProjection, DetEntry, Inner, PaneKey};

/// A transient hook reading older than this is spent: it can no longer decide
/// fused state on its own. A confirmed exact lifecycle start is retained until
/// its matching end replaces it. A provisional unkeyed start is reconciled by
/// later visual evidence.
const HOOK_READING_TTL_MS: u64 = 300_000;
/// Delay before the first stable visual reconciliation of an unkeyed start.
/// A clean frame is neutral at every age. Positive Working accepts the edge;
/// staged input, a blocking screen, or pane mode rejects it.
const UNKEYED_DISPATCH_SETTLE_MS: u64 = 500;
/// One bounded follow-up for contradictory, stale, or held current evidence.
/// New pane events may schedule another pass; the worker itself never polls.
const CONTINUOUS_EVIDENCE_RECHECK_MS: u64 = 100;
/// Consecutive rules-tier verdicts contradicting the hook reading before
/// the reading is invalidated.
const HOOK_DISAGREE_LIMIT: u32 = 3;

/// Stored hook sensor state per pane: the reading plus how many deciding
/// rules-tier recomputes have contradicted it in a row.
pub(crate) struct HookEntry {
    /// The occupant that reported it, and the rules it was read under.
    pane_pid: crate::identity::ProcId,
    manifest: Option<String>,
    pub(crate) reading: SensorReading,
    pub(crate) disagreements: u32,
    /// This reading came from the manifest's turn-start edge. A confirmed exact
    /// start remains active until its matching end or binding retirement. A
    /// provisional start remains active only until visual reconciliation.
    active_start: bool,
    /// The start itself is independent evidence that a turn exists.
    ///
    /// An unkeyed prompt-submit hook is provisional: it updates runtime status
    /// immediately, but a later visual Working observation must confirm it
    /// before delivery or `wait done` may rely on it.
    confirmed_start: bool,
    /// This reading is a conclusive end for one exact turn. It remains an
    /// edge, not a persistent Idle level: a later current visual Working
    /// observation supersedes it.
    authoritative_end: bool,
    /// The exact turn when the manifest can name it. `None` can describe a
    /// binding-scoped confirmed lifecycle or provisional dispatch evidence,
    /// but it can never authorize exact-turn settlement.
    active_turn: Option<turnkey::TurnKey>,
    /// Deadline for one stable visual reconciliation of an unkeyed start.
    /// The hook edge itself remains `reading.ts`; the deadline never stands in
    /// for a second event.
    provisional_ready_at_ms: Option<u64>,
    /// This start was a provisional candidate that a visual Working frame
    /// promoted. Only such a latch may be ended by the screen's lifecycle
    /// evidence; an authenticated confirmed start keeps its hook-tier end.
    promoted: bool,
}

impl HookEntry {
    /// A reading that remembers whose turn it reported.
    ///
    /// A hook edge is a fact about one process read through one set of
    /// rules. Kept unbound, it outlives both: a replacement occupant
    /// inherits the predecessor's "working", and a pane whose manifest
    /// changed keeps being read by rules that no longer apply.
    pub(crate) fn bound(
        pane_pid: crate::identity::ProcId,
        manifest: Option<String>,
        reading: SensorReading,
    ) -> HookEntry {
        HookEntry {
            reading,
            disagreements: 0,
            pane_pid,
            manifest,
            active_start: false,
            confirmed_start: false,
            authoritative_end: false,
            active_turn: None,
            provisional_ready_at_ms: None,
            promoted: false,
        }
    }
    /// Promote a visually accepted provisional dispatch start into a
    /// persistent unkeyed start on the same binding, keeping its original
    /// edge. The latch then ends only on an exact keyed end for this
    /// binding, a binding replacement, or one observation of a conclusive
    /// lifecycle-evidence idle screen winner on an idle-class fused frame
    /// with the binding proven stable across that capture; never on a
    /// candidate end, a generic clean composer, a priority, or a timer.
    pub(crate) fn promote(self) -> HookEntry {
        debug_assert!(self.active_start && !self.confirmed_start);
        HookEntry {
            confirmed_start: true,
            provisional_ready_at_ms: None,
            promoted: true,
            ..self
        }
    }

    /// A confirmed turn-start reading that owns runtime state until its exact
    /// lifecycle evidence reports the turn ended.
    pub(crate) fn turn_started(
        pane_pid: crate::identity::ProcId,
        manifest: Option<String>,
        reading: SensorReading,
        turn: turnkey::TurnKey,
    ) -> HookEntry {
        debug_assert_eq!(reading.state, AgentState::Working);
        HookEntry {
            active_start: true,
            confirmed_start: true,
            active_turn: Some(turn),
            ..HookEntry::bound(pane_pid, manifest, reading)
        }
    }

    /// A confirmed start from a vendor whose hooks cannot name turns.
    ///
    /// This owns runtime state under one process and manifest binding. It does
    /// not create exact-turn evidence and cannot settle message receipt or a
    /// composer barrier.
    pub(crate) fn unkeyed_turn_started(
        pane_pid: crate::identity::ProcId,
        manifest: Option<String>,
        reading: SensorReading,
    ) -> HookEntry {
        debug_assert_eq!(reading.state, AgentState::Working);
        HookEntry {
            active_start: true,
            confirmed_start: true,
            ..HookEntry::bound(pane_pid, manifest, reading)
        }
    }

    /// A prompt-submit edge that updates runtime status while its acceptance
    /// remains subject to an independent visual observation.
    pub(crate) fn provisional_start(
        pane_pid: crate::identity::ProcId,
        manifest: Option<String>,
        reading: SensorReading,
    ) -> HookEntry {
        debug_assert_eq!(reading.state, AgentState::Working);
        HookEntry {
            active_start: true,
            confirmed_start: false,
            provisional_ready_at_ms: Some(reading.ts.saturating_add(UNKEYED_DISPATCH_SETTLE_MS)),
            ..HookEntry::bound(pane_pid, manifest, reading)
        }
    }

    /// A conclusive end that owns runtime state until the visual sensors
    /// reach a terminal frame or a later start replaces it.
    pub(crate) fn turn_ended(
        pane_pid: crate::identity::ProcId,
        manifest: Option<String>,
        reading: SensorReading,
        turn: turnkey::TurnKey,
    ) -> HookEntry {
        debug_assert_eq!(reading.state, AgentState::Idle);
        HookEntry {
            authoritative_end: true,
            active_turn: Some(turn),
            ..HookEntry::bound(pane_pid, manifest, reading)
        }
    }

    /// Does this reading still describe the pane in front of us?
    ///
    /// Exact equality, with no escape hatch. A zero pid would mean nobody
    /// established whose turn this was, and a reading nobody can attribute
    /// must not be usable as evidence about whoever holds the pane now.
    fn describes(&self, agent: Option<crate::identity::ProcId>, manifest: Option<&str>) -> bool {
        agent == Some(self.pane_pid) && self.manifest.as_deref() == manifest
    }

    /// Does this exact process binding already own an active turn start?
    pub(crate) fn active_start_for(
        &self,
        agent: crate::identity::ProcId,
        manifest: Option<&str>,
    ) -> bool {
        self.active_start && self.describes(Some(agent), manifest)
    }

    fn provisional_start_for(
        &self,
        agent: crate::identity::ProcId,
        manifest: Option<&str>,
    ) -> bool {
        self.active_start_for(agent, manifest) && !self.confirmed_start
    }

    fn provisional_recheck_for(
        &self,
        agent: crate::identity::ProcId,
        manifest: Option<&str>,
    ) -> Option<ProvisionalStartRecheck> {
        self.provisional_start_for(agent, manifest)
            .then(|| ProvisionalStartRecheck {
                agent,
                manifest: manifest.map(str::to_string),
                edge_ms: self.reading.ts,
                ready_at_ms: self
                    .provisional_ready_at_ms
                    .expect("provisional start has a reconciliation deadline"),
            })
    }

    pub(crate) fn provisional_edge_for(
        &self,
        agent: crate::identity::ProcId,
        manifest: Option<&str>,
    ) -> Option<u64> {
        self.provisional_recheck_for(agent, manifest)
            .map(|candidate| candidate.edge_ms)
    }

    fn confirmed_start_for(&self, agent: crate::identity::ProcId, manifest: Option<&str>) -> bool {
        self.active_start_for(agent, manifest) && self.confirmed_start
    }

    /// Does this binding own a confirmed lifecycle that cannot name turns?
    pub(crate) fn confirmed_unkeyed_start_for(
        &self,
        agent: crate::identity::ProcId,
        manifest: Option<&str>,
    ) -> bool {
        self.confirmed_start_for(agent, manifest) && self.active_turn.is_none()
    }
    /// Does a confirmed exact end at `end_edge_ms` on this binding end a
    /// persistent unkeyed start? The start had no key to match, so the end
    /// must be separately proven on the same agent generation and manifest
    /// and must come strictly after the stored start edge: a stale,
    /// reordered, or same-instant end is not evidence that this turn ended.
    pub(crate) fn unkeyed_latch_ended_by(
        &self,
        agent: crate::identity::ProcId,
        manifest: Option<&str>,
        end_edge_ms: u64,
    ) -> bool {
        self.confirmed_unkeyed_start_for(agent, manifest) && end_edge_ms > self.reading.ts
    }

    /// Does an end name the active turn under this exact process binding?
    pub(crate) fn active_start_matches(
        &self,
        agent: crate::identity::ProcId,
        manifest: Option<&str>,
        turn: Option<&turnkey::TurnKey>,
    ) -> bool {
        let Some(turn) = turn else {
            return false;
        };
        self.active_start_for(agent, manifest) && self.active_turn.as_ref() == Some(turn)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProvisionalStartRecheck {
    agent: crate::identity::ProcId,
    manifest: Option<String>,
    edge_ms: u64,
    ready_at_ms: u64,
}

/// Candidate start accepted by a later visual Working observation.
struct ConfirmedCandidateStart {
    edge: crate::hook_lifecycle::Candidate,
    accepted_ms: u64,
    terminal: bool,
}

enum LifecycleRecheckWork {
    Terminal(
        crate::hook_lifecycle::TerminalKind,
        crate::hook_lifecycle::Candidate,
    ),
    Provisional(ProvisionalStartRecheck),
    Observation(u64),
}

impl LifecycleRecheckWork {
    fn ready_at_ms(&self) -> u64 {
        match self {
            Self::Terminal(_, candidate) => candidate.ready_at_ms,
            Self::Provisional(candidate) => candidate.ready_at_ms,
            Self::Observation(ready_at_ms) => *ready_at_ms,
        }
    }

    fn cause(&self) -> &'static str {
        match self {
            Self::Terminal(crate::hook_lifecycle::TerminalKind::End, _) => "candidate_end_settled",
            Self::Terminal(crate::hook_lifecycle::TerminalKind::VisualEnd, _) => {
                "candidate_visual_end_settled"
            }
            Self::Provisional(_) => "unkeyed_dispatch_settled",
            Self::Observation(_) => "continuous_evidence_recheck",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleObservation {
    None,
    Visual,
    Stable,
}

impl LifecycleObservation {
    fn from_cause(cause: &str) -> Self {
        match cause {
            "output_settled"
            | "bootstrap"
            | "lag_reconcile"
            | "pane_reconciled"
            | "status"
            | "pane.read"
            | "gate"
            | "pre_paste"
            | "prewrite_block_reconcile"
            | "receipt_checkpoint"
            | "candidate_end_settled"
            | "candidate_visual_end_settled"
            | "unkeyed_dispatch_settled"
            | "continuous_evidence_recheck" => Self::Stable,
            "pane_added" | "pane_changed" => Self::Visual,
            _ => Self::None,
        }
    }

    fn is_visual(self) -> bool {
        self != Self::None
    }
}

pub(crate) fn schedule_candidate_end_recheck(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    _candidate: crate::hook_lifecycle::Candidate,
) {
    schedule_lifecycle_recheck(inner, pane);
}

pub(crate) fn schedule_unkeyed_dispatch_recheck(inner: &Arc<Inner>, pane: &PaneKey) {
    schedule_lifecycle_recheck(inner, pane);
}

pub(crate) struct LifecycleRecheckTask {
    notify: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

pub(crate) fn schedule_lifecycle_recheck(inner: &Arc<Inner>, pane: &PaneKey) {
    if *inner.stop.borrow() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if let Some(notify) = inner
        .lifecycle_rechecks
        .lock()
        .expect("lifecycle rechecks lock")
        .get(pane)
        .map(|entry| Arc::clone(&entry.notify))
    {
        notify.notify_one();
        return;
    }
    let has_terminal = inner
        .hook_lifecycle
        .lock()
        .expect("hook lifecycle lock")
        .has_terminal_candidates(pane);
    let has_provisional = provisional_recheck(inner, pane).is_some();
    let needs_observation = targeted_reobservation_needed(inner, pane);
    if !has_terminal && !has_provisional && !needs_observation {
        return;
    }
    let mut rechecks = inner
        .lifecycle_rechecks
        .lock()
        .expect("lifecycle rechecks lock");
    // Shutdown sets the stop latch before draining this registry. Check it
    // while holding the insertion lock so every published entry remains
    // joinable by shutdown.
    if *inner.stop.borrow() {
        return;
    }
    if let Some(entry) = rechecks.get(pane) {
        entry.notify.notify_one();
        return;
    }
    let notify = Arc::new(tokio::sync::Notify::new());
    let worker_inner = Arc::clone(inner);
    let worker_pane = pane.clone();
    let worker_notify = Arc::clone(&notify);
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let task = runtime.spawn(async move {
        if start_rx.await.is_err() {
            return;
        }
        lifecycle_recheck_worker(worker_inner, worker_pane, worker_notify).await;
    });
    rechecks.insert(
        pane.clone(),
        LifecycleRecheckTask {
            notify: Arc::clone(&notify),
            task,
        },
    );
    // The task cannot inspect or retire its entry until the registry owns its
    // handle. Sending under the same lock also closes shutdown's drain race.
    let _ = start_tx.send(());
}

async fn lifecycle_recheck_worker(
    inner: Arc<Inner>,
    pane: PaneKey,
    notify: Arc<tokio::sync::Notify>,
) {
    let mut stop = inner.stop.clone();
    let mut attempted_terminal = HashSet::new();
    let mut attempted_provisional = HashSet::new();
    let mut attempted_observation = false;
    loop {
        if *stop.borrow() || !lifecycle_recheck_is_current(&inner, &pane, &notify) {
            remove_lifecycle_recheck(&inner, &pane, &notify);
            return;
        }
        let terminal = inner
            .hook_lifecycle
            .lock()
            .expect("hook lifecycle lock")
            .next_terminal_recheck_excluding(&pane, &attempted_terminal)
            .map(|(kind, candidate)| LifecycleRecheckWork::Terminal(kind, candidate));
        let provisional = provisional_recheck(&inner, &pane)
            .filter(|candidate| {
                !attempted_provisional.contains(&(candidate.agent, candidate.edge_ms))
            })
            .map(LifecycleRecheckWork::Provisional);
        let observation = (!attempted_observation && targeted_reobservation_needed(&inner, &pane))
            .then(|| {
                LifecycleRecheckWork::Observation(
                    unix_ms().saturating_add(CONTINUOUS_EVIDENCE_RECHECK_MS),
                )
            });
        let next = [terminal, provisional, observation]
            .into_iter()
            .flatten()
            .min_by_key(LifecycleRecheckWork::ready_at_ms);
        let Some(work) = next else {
            if retire_lifecycle_recheck_if_empty(&inner, &pane, &notify) {
                return;
            }
            // Every current candidate already had one stable observation.
            // Wait for new evidence instead of repeatedly capturing the same
            // frame. A notification resets the pass so every candidate can be
            // considered again against that new evidence.
            tokio::select! {
                _ = notify.notified() => {
                    attempted_terminal.clear();
                    attempted_provisional.clear();
                    attempted_observation = false;
                }
                _ = stop.changed() => {}
            }
            continue;
        };
        let delay = work.ready_at_ms().saturating_sub(unix_ms());
        if delay > 0 {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                _ = notify.notified() => {
                    attempted_terminal.clear();
                    attempted_provisional.clear();
                    attempted_observation = false;
                    continue;
                },
                _ = stop.changed() => continue,
            }
        }
        if *stop.borrow() || !lifecycle_recheck_is_current(&inner, &pane, &notify) {
            remove_lifecycle_recheck(&inner, &pane, &notify);
            return;
        }
        let Some(watcher) = inner.watcher_of(pane.session_idx) else {
            remove_lifecycle_recheck(&inner, &pane, &notify);
            return;
        };
        let cause = work.cause();
        let Some(_detection) = recompute_pane(
            &inner,
            pane.session_idx,
            &watcher,
            &pane.pane_id,
            true,
            cause,
        )
        .await
        else {
            remove_lifecycle_recheck(&inner, &pane, &notify);
            return;
        };
        match work {
            LifecycleRecheckWork::Terminal(kind, candidate) => {
                let still_current = inner
                    .hook_lifecycle
                    .lock()
                    .expect("hook lifecycle lock")
                    .terminal_candidate_is_current(&pane, kind, &candidate);
                if still_current {
                    let key = crate::hook_lifecycle::Store::terminal_recheck_key(kind, &candidate);
                    // This candidate did not settle against the current
                    // evidence. Move on so one unresolved turn cannot starve
                    // another turn's later deadline.
                    attempted_terminal.insert(key);
                }
            }
            LifecycleRecheckWork::Provisional(candidate) => {
                let still_current =
                    provisional_recheck(&inner, &pane).is_some_and(|current| current == candidate);
                if still_current {
                    attempted_provisional.insert((candidate.agent, candidate.edge_ms));
                }
            }
            LifecycleRecheckWork::Observation(_) => {
                attempted_observation = true;
            }
        }
    }
}

fn targeted_reobservation_needed(inner: &Inner, pane: &PaneKey) -> bool {
    inner
        .detections
        .lock()
        .expect("detections lock")
        .get(pane)
        .is_some_and(|entry| needs_targeted_reobservation(&entry.detection, entry.hold))
}

fn needs_targeted_reobservation(detection: &Detection, hold: ComposerHold) -> bool {
    detection.stale || detection.disagreement || hold.refuses()
}

fn provisional_recheck(inner: &Inner, pane: &PaneKey) -> Option<ProvisionalStartRecheck> {
    let readings = inner.hook_readings.lock().expect("hook readings lock");
    let entry = readings.get(pane)?;
    entry.provisional_recheck_for(entry.pane_pid, entry.manifest.as_deref())
}

fn lifecycle_recheck_is_current(
    inner: &Inner,
    pane: &PaneKey,
    notify: &Arc<tokio::sync::Notify>,
) -> bool {
    inner
        .lifecycle_rechecks
        .lock()
        .expect("lifecycle rechecks lock")
        .get(pane)
        .is_some_and(|current| Arc::ptr_eq(&current.notify, notify))
}

fn retire_lifecycle_recheck_if_empty(
    inner: &Inner,
    pane: &PaneKey,
    notify: &Arc<tokio::sync::Notify>,
) -> bool {
    let mut rechecks = inner
        .lifecycle_rechecks
        .lock()
        .expect("lifecycle rechecks lock");
    if !rechecks
        .get(pane)
        .is_some_and(|current| Arc::ptr_eq(&current.notify, notify))
    {
        return true;
    }
    let pending_terminal = inner
        .hook_lifecycle
        .lock()
        .expect("hook lifecycle lock")
        .has_terminal_candidates(pane);
    let pending_provisional = provisional_recheck(inner, pane).is_some();
    if pending_terminal || pending_provisional {
        return false;
    }
    rechecks.remove(pane);
    true
}

fn remove_lifecycle_recheck(inner: &Inner, pane: &PaneKey, notify: &Arc<tokio::sync::Notify>) {
    let mut rechecks = inner
        .lifecycle_rechecks
        .lock()
        .expect("lifecycle rechecks lock");
    if rechecks
        .get(pane)
        .is_some_and(|current| Arc::ptr_eq(&current.notify, notify))
    {
        rechecks.remove(pane);
    }
}

pub(crate) fn cancel_lifecycle_recheck(inner: &Inner, pane: &PaneKey) {
    let mut rechecks = inner
        .lifecycle_rechecks
        .lock()
        .expect("lifecycle rechecks lock");
    let entry = rechecks.remove(pane);
    inner
        .hook_lifecycle
        .lock()
        .expect("hook lifecycle lock")
        .forget(pane);
    drop(rechecks);
    stop_lifecycle_recheck_entry(entry);
}

fn cancel_lifecycle_recheck_task(inner: &Inner, pane: &PaneKey) {
    let mut rechecks = inner
        .lifecycle_rechecks
        .lock()
        .expect("lifecycle rechecks lock");
    let entry = rechecks.remove(pane);
    drop(rechecks);
    stop_lifecycle_recheck_entry(entry);
}

fn stop_lifecycle_recheck_entry(entry: Option<LifecycleRecheckTask>) {
    if let Some(entry) = entry {
        entry.notify.notify_one();
        let task = entry.task;
        // A recheck can discover a manifest or process rebound inside its own
        // recompute. Let that recompute commit the new binding. Aborting the
        // current task here would cancel it at its next await and leave the
        // cache on the old occupant.
        if tokio::task::try_id().is_none_or(|current| current != task.id()) {
            task.abort();
        }
    }
}

pub(crate) fn take_lifecycle_recheck_tasks(inner: &Inner) -> Vec<tokio::task::JoinHandle<()>> {
    let entries = std::mem::take(
        &mut *inner
            .lifecycle_rechecks
            .lock()
            .expect("lifecycle rechecks lock"),
    );
    entries
        .into_values()
        .map(|entry| {
            entry.notify.notify_one();
            entry.task
        })
        .collect()
}

fn positive_visual_working(inner: &Inner, manifest: &str, detection: &Detection) -> bool {
    let manifest = inner.manifests.get(manifest);
    !detection.stale
        && detection.state == AgentState::Working
        && detection.readings.iter().any(|reading| {
            if reading.sensor == Sensor::Hook || reading.state != AgentState::Working {
                return false;
            }
            manifest.is_none_or(|manifest| {
                let mut rules = manifest.rules.iter().filter(|rule| rule.id == reading.rule);
                let Some(first) = rules.next() else {
                    return false;
                };
                first.lifecycle_evidence && rules.all(|rule| rule.lifecycle_evidence)
            })
        })
}

fn terminal_visual_state(detection: &Detection, in_mode: bool) -> Option<AgentState> {
    if in_mode || detection.stale {
        return None;
    }
    let screen = detection
        .readings
        .iter()
        .filter(|reading| reading.sensor == Sensor::Screen)
        .map(|reading| reading.state)
        .next()?;
    if !matches!(screen, AgentState::Idle | AgentState::IdleWithInput) {
        return None;
    }
    let terminal = detection
        .readings
        .iter()
        .filter(|reading| reading.sensor != Sensor::Hook)
        .all(|reading| matches!(reading.state, AgentState::Idle | AgentState::IdleWithInput));
    terminal.then_some(screen)
}

fn visual_rejects_start(detection: &Detection, in_mode: bool) -> bool {
    if in_mode {
        return true;
    }
    if detection.stale {
        return false;
    }
    let decisive_screen = detection.readings.iter().any(|reading| {
        reading.sensor == Sensor::Screen
            && (reading.state == AgentState::IdleWithInput || reading.state.is_blocked())
    });
    decisive_screen
}

/// Reconcile pane runtime and one exact dispatch candidate from the same hook
/// edge. Runtime is pane-scoped. Receipt remains attempt-scoped and requires a
/// pending candidate with this exact edge.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn reconcile_unkeyed_dispatch_start(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
    detection: &Detection,
    in_mode: bool,
    observed_ms: u64,
    observation: LifecycleObservation,
) -> bool {
    reconcile_unkeyed_dispatch_start_with_evidence(
        inner,
        pane,
        agent,
        manifest,
        detection,
        in_mode,
        observed_ms,
        observed_ms,
        true,
        observation,
    )
}

#[allow(clippy::too_many_arguments)]
fn reconcile_unkeyed_dispatch_start_with_evidence(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
    detection: &Detection,
    in_mode: bool,
    observed_ms: u64,
    evidence_ms: u64,
    binding_stable: bool,
    observation: LifecycleObservation,
) -> bool {
    if !binding_stable
        || !observation.is_visual()
        || !inner.manifests.get(manifest).is_some_and(|m| {
            m.hooks.ack_evidence == AckEvidence::Dispatch && m.hooks.turn_key_fields.is_empty()
        })
    {
        return false;
    }
    let mut readings = inner.hook_readings.lock().expect("hook readings lock");
    let provisional = readings
        .get(pane)
        .and_then(|entry| entry.provisional_recheck_for(agent, Some(manifest)));
    let Some(provisional) = provisional else {
        return false;
    };
    if provisional.edge_ms >= evidence_ms {
        return false;
    }

    let accepted = positive_visual_working(inner, manifest, detection);
    let rejected = visual_rejects_start(detection, in_mode);
    if !accepted && !rejected {
        return false;
    }

    let entry = readings.remove(pane);
    let removed = entry.is_some();
    if accepted && !rejected {
        if let Some(entry) = entry {
            readings.insert(pane.clone(), entry.promote());
        }
    }
    drop(readings);
    if removed {
        if rejected {
            crate::delivery::reject_unkeyed_dispatch_ack(
                inner,
                pane.session_idx,
                &pane.pane_id,
                agent,
                manifest,
                provisional.edge_ms,
                "hook_dispatch_conflicted",
            );
        } else if accepted {
            crate::delivery::confirm_unkeyed_dispatch_ack(
                inner,
                pane.session_idx,
                &pane.pane_id,
                agent,
                manifest,
                provisional.edge_ms,
                observed_ms,
            );
        }
    }
    accepted && !rejected && removed
}

/// Whether the current Working cache is backed by a lifecycle-capable visual
/// observation or a confirmed hook start. Provisional hook status is excluded.
pub(crate) fn cached_working_confirmed(inner: &Inner, session_idx: usize, pane_id: &str) -> bool {
    let pane = PaneKey::new(session_idx, pane_id);
    inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&pane)
        .is_some_and(|entry| entry.working_confirmed)
}

fn working_is_confirmed(
    inner: &Inner,
    pane: &PaneKey,
    detection: &Detection,
    agent: Option<crate::identity::ProcId>,
    manifest: Option<&str>,
) -> bool {
    if detection.state != AgentState::Working {
        return false;
    }
    if manifest.is_some_and(|manifest| positive_visual_working(inner, manifest, detection)) {
        return true;
    }
    match (agent, manifest) {
        (Some(agent), manifest) => inner
            .hook_readings
            .lock()
            .expect("hook readings lock")
            .get(pane)
            .is_some_and(|entry| entry.confirmed_start_for(agent, manifest)),
        _ => false,
    }
}

fn retire_obsolete_visual_end(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
) {
    let active_turn = inner
        .hook_readings
        .lock()
        .expect("hook readings lock")
        .get(pane)
        .filter(|entry| entry.active_start_for(agent, Some(manifest)))
        .and_then(|entry| entry.active_turn.clone());
    let Some(active_turn) = active_turn else {
        return;
    };
    // Visual idle cannot end a keyed lifecycle. Retire candidates created by
    // older code, then keep the exact start until its matching keyed end and a
    // clean composer settle through the normal terminal-candidate path.
    inner
        .hook_lifecycle
        .lock()
        .expect("hook lifecycle lock")
        .clear_visual_end(pane, agent, manifest, &active_turn);
}

struct TerminalDrain {
    confirmations: Vec<ConfirmedCandidateStart>,
}

const MAX_TERMINAL_DRAIN_PER_OBSERVATION: usize = 128;

/// One stable terminal frame can settle every exact Stop that became ready
/// while the pane was detached. Draining here avoids depending on timer tasks
/// that correctly exited when no watcher was available.
fn drain_terminal_candidates(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
    observed_ms: u64,
    evidence_ms: u64,
) -> TerminalDrain {
    let mut confirmations = Vec::new();
    for _ in 0..MAX_TERMINAL_DRAIN_PER_OBSERVATION {
        let active_turn = inner
            .hook_readings
            .lock()
            .expect("hook readings lock")
            .get(pane)
            .filter(|entry| entry.active_start_for(agent, Some(manifest)))
            .and_then(|entry| entry.active_turn.clone());
        let settlement = inner
            .hook_lifecycle
            .lock()
            .expect("hook lifecycle lock")
            .settle_next_ready_terminal_with_evidence(
                pane,
                agent,
                manifest,
                active_turn.as_ref(),
                observed_ms,
                evidence_ms,
            );
        let Some(settlement) = settlement else {
            break;
        };
        let end = settlement.end;
        turnkey::PaneEnds::record(
            &mut inner.turn_ends.lock().expect("turn ends lock"),
            pane,
            agent,
            manifest,
            end.turn.clone(),
        );
        let mut readings = inner.hook_readings.lock().expect("hook readings lock");
        if readings
            .get(pane)
            .is_some_and(|entry| entry.active_start_matches(agent, Some(manifest), Some(&end.turn)))
        {
            readings.remove(pane);
        }
        drop(readings);
        if let Some(start) = settlement.start {
            // Pin the paired dispatch before another unrelated end is
            // recorded. PaneEnds bounds unpinned history, so deferring every
            // pin until the whole drain finishes could evict the active
            // delivery's exact end inside one large detached backlog.
            crate::delivery::prepare_dispatch_ack(
                inner,
                pane.session_idx,
                &pane.pane_id,
                start.agent,
                &start.manifest,
                &start.turn,
            );
            confirmations.push(ConfirmedCandidateStart {
                edge: start,
                accepted_ms: observed_ms,
                terminal: true,
            });
        }
    }
    TerminalDrain { confirmations }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn reconcile_candidate_lifecycles(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
    detection: &Detection,
    in_mode: bool,
    observed_ms: u64,
    observation: LifecycleObservation,
) -> Vec<ConfirmedCandidateStart> {
    reconcile_candidate_lifecycles_with_evidence(
        inner,
        pane,
        agent,
        manifest,
        detection,
        in_mode,
        observed_ms,
        observed_ms,
        true,
        observation,
    )
}

#[allow(clippy::too_many_arguments)]
fn reconcile_candidate_lifecycles_with_evidence(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
    detection: &Detection,
    in_mode: bool,
    observed_ms: u64,
    evidence_ms: u64,
    binding_stable: bool,
    observation: LifecycleObservation,
) -> Vec<ConfirmedCandidateStart> {
    if !binding_stable {
        return Vec::new();
    }
    let mut confirmations = Vec::new();
    if observation == LifecycleObservation::Stable
        && terminal_visual_state(detection, in_mode).is_some()
    {
        let drained =
            drain_terminal_candidates(inner, pane, agent, manifest, observed_ms, evidence_ms);
        confirmations.extend(drained.confirmations);
    }
    confirmations.extend(reconcile_candidate_lifecycle_with_evidence(
        inner,
        pane,
        agent,
        manifest,
        detection,
        in_mode,
        observed_ms,
        evidence_ms,
        observation,
    ));
    confirmations
}

/// Promote or retire candidate lifecycle edges using a later watcher revision.
/// Raw hook-triggered recomputes cannot reach a candidate because they do not
/// advance the visual revision.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn reconcile_candidate_lifecycle(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
    detection: &Detection,
    in_mode: bool,
    observed_ms: u64,
    observation: LifecycleObservation,
) -> Option<ConfirmedCandidateStart> {
    reconcile_candidate_lifecycle_with_evidence(
        inner,
        pane,
        agent,
        manifest,
        detection,
        in_mode,
        observed_ms,
        observed_ms,
        observation,
    )
}

#[allow(clippy::too_many_arguments)]
fn reconcile_candidate_lifecycle_with_evidence(
    inner: &Arc<Inner>,
    pane: &PaneKey,
    agent: crate::identity::ProcId,
    manifest: &str,
    detection: &Detection,
    in_mode: bool,
    observed_ms: u64,
    evidence_ms: u64,
    observation: LifecycleObservation,
) -> Option<ConfirmedCandidateStart> {
    let active_turn = inner
        .hook_readings
        .lock()
        .expect("hook readings lock")
        .get(pane)
        .filter(|entry| entry.active_start_for(agent, Some(manifest)))
        .and_then(|entry| entry.active_turn.clone());
    let (start, end) = {
        let store = inner.hook_lifecycle.lock().expect("hook lifecycle lock");
        let end = store.end_for(pane, agent, manifest, active_turn.as_ref());
        (store.start_for(pane, agent, manifest), end)
    };
    // The evidence timestamp names the event that caused this observation.
    // An edge arriving after those bytes cannot be confirmed by a capture
    // that ran later, even if the observation commits a newer revision.
    let start = start.filter(|edge| edge.edge_ms < evidence_ms);
    let end = end.filter(|edge| edge.edge_ms < evidence_ms);

    let terminal_state = terminal_visual_state(detection, in_mode);
    if let Some(end) = end.as_ref() {
        let working_belongs_to_end = active_turn.as_ref() == Some(&end.turn)
            && !inner
                .hook_lifecycle
                .lock()
                .expect("hook lifecycle lock")
                .has_other_start_for(pane, agent, manifest, &end.turn);
        if positive_visual_working(inner, manifest, detection)
            && working_belongs_to_end
            && observed_ms >= end.ready_at_ms
        {
            // A Stop hook reports before Claude has combined every concurrent
            // Stop decision. Later Working proves at least one sibling kept
            // this exact turn alive. A candidate for another turn makes the
            // frame ambiguous, so it cannot retire this Stop.
            let consumed = inner
                .hook_lifecycle
                .lock()
                .expect("hook lifecycle lock")
                .clear_terminal_candidates(pane, end);
            if !consumed {
                return None;
            }
        } else if observation != LifecycleObservation::Stable || observed_ms < end.ready_at_ms {
            let deferred = inner
                .hook_lifecycle
                .lock()
                .expect("hook lifecycle lock")
                .defer_end(pane, end);
            if !deferred {
                return None;
            }
        } else if terminal_state.is_some() {
            let settlement = inner
                .hook_lifecycle
                .lock()
                .expect("hook lifecycle lock")
                .settle_terminal_candidate(pane, end);
            let settlement = settlement?;
            turnkey::PaneEnds::record(
                &mut inner.turn_ends.lock().expect("turn ends lock"),
                pane,
                agent,
                manifest,
                end.turn.clone(),
            );
            {
                let mut readings = inner.hook_readings.lock().expect("hook readings lock");
                if readings.get(pane).is_some_and(|entry| {
                    entry.active_start_matches(agent, Some(manifest), Some(&end.turn))
                }) {
                    readings.remove(pane);
                }
            }
            if let Some(start) = settlement.start {
                return Some(ConfirmedCandidateStart {
                    edge: start,
                    accepted_ms: observed_ms,
                    terminal: true,
                });
            }
        } else {
            let deferred = inner
                .hook_lifecycle
                .lock()
                .expect("hook lifecycle lock")
                .defer_end(pane, end);
            if !deferred {
                return None;
            }
        }
    }

    retire_obsolete_visual_end(inner, pane, agent, manifest);

    let start = start?;
    if active_turn.is_some() {
        inner
            .hook_lifecycle
            .lock()
            .expect("hook lifecycle lock")
            .defer_start(pane, &start);
        return None;
    }
    if visual_rejects_start(detection, in_mode) {
        // Staged input or a blocking screen proves this prompt did not enter a
        // turn. A clean composer is neutral: Claude clears accepted input
        // before it paints the first Working frame.
        let _ = inner
            .hook_lifecycle
            .lock()
            .expect("hook lifecycle lock")
            .clear_start(pane, &start);
        return None;
    }
    if !positive_visual_working(inner, manifest, detection) {
        let _ = inner
            .hook_lifecycle
            .lock()
            .expect("hook lifecycle lock")
            .defer_start(pane, &start);
        return None;
    }
    let already_ended = turnkey::PaneEnds::holds(
        &inner.turn_ends.lock().expect("turn ends lock"),
        pane,
        agent,
        manifest,
        &start.turn,
    );
    let consumed = inner
        .hook_lifecycle
        .lock()
        .expect("hook lifecycle lock")
        .clear_start(pane, &start);
    if !consumed {
        return None;
    }
    if already_ended {
        return None;
    }
    let reading = SensorReading {
        sensor: Sensor::Hook,
        state: AgentState::Working,
        rule: format!("{}:visual_confirmed", start.event),
        ts: observed_ms,
    };
    inner
        .hook_readings
        .lock()
        .expect("hook readings lock")
        .insert(
            pane.clone(),
            HookEntry::turn_started(
                agent,
                Some(manifest.to_string()),
                reading,
                start.turn.clone(),
            ),
        );
    // A confirmed end can race the promotion between the first check and
    // insertion. Recheck after the active reading exists. If the end landed
    // first, remove only the exact reading this promotion installed.
    let ended_after_insert = turnkey::PaneEnds::holds(
        &inner.turn_ends.lock().expect("turn ends lock"),
        pane,
        agent,
        manifest,
        &start.turn,
    );
    if ended_after_insert {
        let mut readings = inner.hook_readings.lock().expect("hook readings lock");
        if readings.get(pane).is_some_and(|entry| {
            entry.active_start_matches(agent, Some(manifest), Some(&start.turn))
        }) {
            readings.remove(pane);
        }
        return None;
    }
    Some(ConfirmedCandidateStart {
        edge: start,
        accepted_ms: observed_ms,
        terminal: false,
    })
}

/// Bind a manifest to a pane by its foreground command. Deterministic:
/// manifests iterate in id order.
pub(crate) fn bind_manifest<'a>(
    manifests: &'a BTreeMap<String, Manifest>,
    current_command: &str,
) -> Option<&'a Manifest> {
    manifests
        .values()
        .find(|m| m.agent.process_names.iter().any(|p| p == current_command))
}

/// Bind a manifest to a pane row: the explicit pin first, then the comm
/// name, then the live process ancestry.
///
/// The pin is `cyclops name --manifest <id>` and it wins outright. It
/// exists because both automatic routes read what the pane is RUNNING, and
/// a wrapper script, a `sh -c`, or a versioned symlink (F21) can leave a
/// real agent looking like nothing in particular. A person who says which
/// CLI is in the pane is better evidence than a process name.
///
/// pane_current_command is the kernel comm of the RESOLVED executable, so
/// native installs can report a bare version string ("2.1.220", F21) and
/// never bind by comm; the invoked argv[0] basename still says "claude".
/// The foreground argv cache keeps that common case cheap. When a tool owns
/// the terminal, the live ancestry walk finds the admitted agent above it.
///
/// Both routes start at the pane's FOREGROUND process, not `pane_pid`. See
/// [`foreground_pid`]: an agent started by typing its name at a shell
/// prompt is a child of the pane's first process, and reading `pane_pid`
/// binds every such pane to the shell instead of the agent. The ancestry
/// route is also what keeps the manifest stable while the agent gives the
/// terminal to a child tool.
pub(crate) fn bind_manifest_for<'a>(
    inner: &'a Inner,
    session_idx: usize,
    row: &PaneRow,
) -> Option<&'a Manifest> {
    let pinned = inner.session(session_idx).and_then(|slot| {
        let session_instance_id = {
            let link = slot.link.lock().expect("session link lock");
            link.identity.as_ref()?.session_instance_id()
        };
        let pane = row.pane_id.parse().ok()?;
        let root = crate::identity::ProcId::of(row.pane_pid)?;
        let pane_root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
        inner
            .adoption_for_observed_route(
                RecipientKey::agent(inner.workspace_id, session_instance_id, pane),
                &row.pane_id,
                pane_root,
            )
            .and_then(|adoption| adoption.manifest.clone())
    });
    if let Some(pinned) = pinned {
        // A pin that names nothing loaded falls through to detection
        // rather than blinding the pane: the manifest set can shrink
        // between the adoption and this recompute.
        if let Some(m) = inner.manifests.get(&pinned) {
            return Some(m);
        }
    }
    if let Some(m) = bind_manifest(&inner.manifests, &row.current_command) {
        return Some(m);
    }
    if row.pane_pid <= 0 {
        return None;
    }
    let leader = foreground_pid(row.pane_pid);
    argv_bound_manifest(inner, session_idx, &row.pane_id, leader)
        .or_else(|| vendor_between(inner, session_idx, &row.pane_id, leader, row.pane_pid))
        .map(|(m, _)| m)
}

/// The agent instance a process is working for, proven from the process
/// tree and its argv.
///
/// [`bind_manifest_for`] answers which RULES should read a pane, and an
/// operator's pin is good evidence for that. This answers who is
/// ALLOWED to speak for the pane, and a pin cannot establish that: it is
/// a claim about the pane, and a pane sitting at its shell prompt keeps
/// its pin while anyone at that prompt runs anything they like. So this
/// route reads only live argv, refuses when nothing between `from` and
/// the pane root is a program the daemon ships a manifest for, and
/// refuses when ps cannot be read at all.
pub(crate) fn vendor_between<'a>(
    inner: &'a Inner,
    _session_idx: usize,
    pane_id: &str,
    from: i32,
    root: i32,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    // The binding is built INSIDE the ancestry walk and returned as it
    // stands. Returning a pid for a second lookup would leave a gap where
    // that process exits, its number goes to another vendor, and the
    // second read produces a valid-looking binding for a process this
    // walk never saw.
    crate::identity::vendor_ancestor(from, root, |p| argv_live(inner, pane_id, p))
}

fn vendor_between_observed<'a>(
    inner: &'a Inner,
    _session_idx: usize,
    pane_id: &str,
    from: i32,
    root: i32,
) -> crate::identity::VendorAncestor<(&'a Manifest, crate::identity::ProcId)> {
    crate::identity::vendor_ancestor_observed(from, root, |p| argv_live_observed(inner, pane_id, p))
}

/// The same binding, read LIVE, with no cache consulted.
///
/// The cache is keyed by process identity, which a reused pid cannot
/// forge, but pid and birth both survive an in-place `exec`: a process can
/// bind as a vendor, exec into something else, and keep the identity it
/// was admitted under. Cursor's launcher does exactly that
/// (`exec -a "$0" "$NODE_BIN" ...`), so exec is not a hypothetical here.
///
/// A stale answer on the manifest-binding path costs a wrong rule set
/// until the next probe. On the authentication path it admits a process
/// that is no longer an agent, so that path pays for a fresh read every
/// time.
/// Is this process, right now, one of the vendors this daemon ships
/// rules for?
///
/// Live argv, never the cache. The cache remembers what a pid WAS, and
/// this question is about what it IS: a process that exec'd into or out
/// of a vendor keeps its pid, and the cached answer would outlive the
/// program it described.
///
/// Three answers, because the caller is proving a NEGATIVE with it: an
/// ancestor nobody could read might be the agent, and treating that as
/// "not an agent" is how an orphaned vendor chain borrows the operator's
/// name.
pub(crate) fn is_vendor_now(inner: &Inner, pid: i32) -> crate::identity::Vendorship {
    use crate::identity::Vendorship;
    match vendor_read(inner, pid, argv_basename, crate::identity::proc_facts) {
        VendorRead::Vendor(_, _) => Vendorship::Vendor,
        VendorRead::NotVendor => Vendorship::NotVendor,
        VendorRead::Unprovable => Vendorship::Unprovable,
    }
}

fn argv_live<'a>(
    inner: &'a Inner,
    pane_id: &str,
    pid: i32,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    let _ = pane_id;
    match vendor_read(inner, pid, argv_basename, crate::identity::proc_facts) {
        VendorRead::Vendor(m, proc) => Some((m, proc)),
        VendorRead::NotVendor | VendorRead::Unprovable => None,
    }
}

fn argv_live_observed<'a>(
    inner: &'a Inner,
    pane_id: &str,
    pid: i32,
) -> crate::identity::VendorAncestor<(&'a Manifest, crate::identity::ProcId)> {
    let _ = pane_id;
    match vendor_read(inner, pid, argv_basename, crate::identity::proc_facts) {
        VendorRead::Vendor(manifest, process) => {
            crate::identity::VendorAncestor::Vendor((manifest, process))
        }
        VendorRead::NotVendor => crate::identity::VendorAncestor::NotVendor,
        VendorRead::Unprovable => crate::identity::VendorAncestor::Unprovable,
    }
}

/// One live read of what a process IS, for everything that needs to know.
///
/// Two answers where a caller wants a binding, three where a caller is
/// proving a negative, and one definition behind both. Two copies of this
/// would be two definitions of "a vendor of ours", and they would drift:
/// one path would admit a process the other refused to classify.
enum VendorRead<'a> {
    Vendor(&'a Manifest, crate::identity::ProcId),
    NotVendor,
    Unprovable,
}

/// The body of [`argv_live`], with both observations injected so a test
/// can prove it never consults the cache.
fn vendor_read<'a, A, F>(inner: &'a Inner, pid: i32, read_argv: A, read_facts: F) -> VendorRead<'a>
where
    A: Fn(i32) -> Option<String>,
    F: Fn(i32) -> Option<(crate::identity::ProcId, u32)>,
{
    // pid 1 is init by definition rather than by observation on macOS:
    // MEASURED on macOS 26.5, `proc_pidinfo` for it is refused to a normal
    // user, so neither its uid nor its argv reads, and it sits at the top
    // of every ancestry walk. Not applied on Linux, where /proc/1 is
    // readable and pid 1 inside a process namespace can be any program,
    // an agent included.
    #[cfg(target_os = "macos")]
    if pid == 1 {
        return VendorRead::NotVendor;
    }
    // Identity and owner together, from one observation. A uid read on
    // its own proves nothing about the process the identity names:
    // credentials can change without the start time moving, and a pid can
    // be handed on between two separate reads.
    let Some(before) = read_facts(pid) else {
        return VendorRead::Unprovable;
    };
    // Every vendor this daemon admits runs as the daemon's own user, so a
    // process owned by anybody else is not one. Structural, and it needs
    // no argv read, which matters because another user's argv is not
    // readable at all.
    if before.1 != unsafe { libc::getuid() } {
        return VendorRead::NotVendor;
    }
    let Some(base) = read_argv(pid) else {
        return VendorRead::Unprovable;
    };
    // Both halves are re-proven across the argv read, for the same reason
    // the cached path re-proves identity: two observations, one moving
    // system. A changed owner refuses here as surely as a changed process.
    if read_facts(pid) != Some(before) {
        return VendorRead::Unprovable;
    }
    let proc = before.0;
    match manifest_for_basename(&inner.manifests, &base) {
        Some(m) => VendorRead::Vendor(m, proc),
        None => VendorRead::NotVendor,
    }
}

/// The agent instance this pane is running right now, by the same rule.
///
/// Starts from the terminal's foreground leader and walks up, so it lands
/// on the agent whether the agent itself holds the tty or has handed it
/// to something it spawned.
pub(crate) fn admitted_vendor<'a>(
    inner: &'a Inner,
    session_idx: usize,
    row: &PaneRow,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    let leader = foreground_pid_checked(row.pane_pid)?;
    vendor_between(inner, session_idx, &row.pane_id, leader, row.pane_pid)
}

/// Everything a write depends on about a pane, read as ONE observation.
///
/// Four facts that have to agree, and are worthless apart: the pane-root
/// process generation, the foreground leader holding the terminal, the agent
/// process the payload belongs to, and the rules that agent is running under
/// right now.
///
/// The manifest is part of it rather than something remembered from the
/// gate because a process can exec in place: same pid, same start time,
/// same identity, different program. Comparing a remembered manifest
/// against a cached verdict would let a payload written for one vendor
/// land in another's composer under the first one's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) pane_root: crate::identity::ProcId,
    pub(crate) leader: crate::identity::ProcId,
    pub(crate) agent: crate::identity::ProcId,
    pub(crate) manifest: String,
}

/// Match a durable notification binding to one current process observation.
///
/// Older incomplete records remain replayable but cannot authorize composer
/// ownership or a terminal action.
pub(crate) fn notification_binding_matches(
    record: &cyclops_proto::NotificationRecord,
    recipient: RecipientKey,
    current: &Binding,
) -> bool {
    let Some(expected) = record.binding.as_ref() else {
        return false;
    };
    let Ok(pane_root) = ProcessInstanceId::new(current.pane_root.pid, current.pane_root.birth)
    else {
        return false;
    };
    let Ok(leader) = ProcessInstanceId::new(current.leader.pid, current.leader.birth) else {
        return false;
    };
    let Ok(agent) = ProcessInstanceId::new(current.agent.pid, current.agent.birth) else {
        return false;
    };

    expected.recipient == record.recipient
        && recipient == record.recipient
        && expected.pane_root == Some(pane_root)
        && expected.leader == Some(leader)
        && expected.agent == agent
        && expected.manifest.as_str() == current.manifest
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BindingObservation {
    Bound(Binding),
    NotVendor,
    Gone,
    Unprovable,
}

fn admitted_binding_observation(
    inner: &Inner,
    session_idx: usize,
    row: &PaneRow,
) -> BindingObservation {
    let Some(pane_root) = crate::identity::ProcId::of(row.pane_pid) else {
        return BindingObservation::Unprovable;
    };
    let Some(leader_pid) = foreground_pid_checked(row.pane_pid) else {
        return BindingObservation::Unprovable;
    };
    let Some(leader) = crate::identity::ProcId::of(leader_pid) else {
        return BindingObservation::Unprovable;
    };
    let observed =
        vendor_between_observed(inner, session_idx, &row.pane_id, leader.pid, row.pane_pid);
    if crate::identity::ProcId::of(row.pane_pid) != Some(pane_root)
        || foreground_pid_checked(row.pane_pid).and_then(crate::identity::ProcId::of)
            != Some(leader)
    {
        return BindingObservation::Unprovable;
    }
    match observed {
        crate::identity::VendorAncestor::Vendor((manifest, agent)) => {
            BindingObservation::Bound(Binding {
                pane_root,
                leader,
                agent,
                manifest: manifest.agent.id.clone(),
            })
        }
        crate::identity::VendorAncestor::NotVendor => BindingObservation::NotVendor,
        crate::identity::VendorAncestor::Unprovable => BindingObservation::Unprovable,
    }
}

fn pane_binding_observation(
    inner: &Inner,
    session_idx: usize,
    row: &PaneRow,
    occupant: Occupant,
) -> BindingObservation {
    if row.dead || occupant == Occupant::Gone {
        BindingObservation::Gone
    } else if occupant == Occupant::Unprovable {
        BindingObservation::Unprovable
    } else {
        admitted_binding_observation(inner, session_idx, row)
    }
}

fn binding_replacement_proven(
    prior_agent: Option<crate::identity::ProcId>,
    prior_manifest: Option<&str>,
    observation: &BindingObservation,
    observed_manifest: Option<&str>,
) -> bool {
    match observation {
        BindingObservation::Bound(binding) => prior_agent.is_some_and(|prior_agent| {
            binding.agent != prior_agent || prior_manifest != observed_manifest
        }),
        BindingObservation::NotVendor | BindingObservation::Gone => true,
        BindingObservation::Unprovable => false,
    }
}

/// The current binding of a pane, or None if any part of it could not be
/// proven now.
pub(crate) fn admitted_binding(
    inner: &Inner,
    session_idx: usize,
    row: &PaneRow,
) -> Option<Binding> {
    match admitted_binding_observation(inner, session_idx, row) {
        BindingObservation::Bound(binding) => Some(binding),
        BindingObservation::NotVendor
        | BindingObservation::Gone
        | BindingObservation::Unprovable => None,
    }
}

/// The manifest that claims this argv[0] basename, by either declared name.
pub(crate) fn manifest_for_basename<'a>(
    manifests: &'a BTreeMap<String, Manifest>,
    base: &str,
) -> Option<&'a Manifest> {
    manifests.values().find(|m| {
        m.agent.argv_basenames.iter().any(|name| name == base)
            || m.agent.process_names.iter().any(|name| name == base)
    })
}

/// The pid whose argv says what a pane is RUNNING.
///
/// tmux's `pane_pid` is the pane's FIRST process, which for an interactive
/// pane is the shell and stays the shell for the pane's whole life. An
/// agent the user starts by typing its name at that prompt is a child of
/// it, so `pane_pid` names the shell no matter what is on screen.
/// MEASURED (tmux 3.7b, Claude Code 2.1.222): a pane running Claude Code
/// reports `pane_current_command` "2.1.222", `pane_pid` the zsh, and
/// `ps -o args=` on that pid "-zsh". Nothing in either sensor says
/// "claude", so the pane bound no manifest and carried no state at all.
///
/// The agent instance's identity, and the reason a pane id and a pane pid
/// are not one.
///
/// `pane_pid` is the process tmux spawned, which for an interactive pane
/// is the SHELL, and it does not change for the pane's whole life. Bind
/// safety evidence to it and an agent can exit, another can be launched
/// at the same prompt, and the second inherits everything the first was
/// trusted for: same pane, same root pid, same command, same manifest.
///
/// The tty's foreground process group is the job the terminal is actually
/// talking to, and a process group's id is its leader's pid, so `tpgid`
/// resolves straight to the running agent. A shell idle at its prompt is
/// its own foreground group and resolves back to `pane_pid` unchanged,
/// which is what makes the agent's exit unbind the manifest again.
pub(crate) fn foreground_pid(pane_pid: i32) -> i32 {
    foreground_pid_checked(pane_pid).unwrap_or(pane_pid)
}

/// The same lookup, with the observation failure kept separate from the
/// answer.
///
/// [`foreground_pid`] reports the pane root when `ps` cannot be read. That
/// is right for BINDING a manifest: a pane nobody can observe binds
/// nothing new, and the shell is the honest fallback identity. It is wrong
/// for holding a pin. A caller comparing a stored agent pid against a
/// silently substituted shell pid compares two different domains and gets
/// a confident wrong answer, so a pin resolves through this and treats
/// `None` as the occupant being gone.
/// Is this process still there?
///
/// `kill(pid, 0)` sends nothing: it asks the kernel to resolve the pid and
/// check permission. `ESRCH` is the one answer that means gone; `EPERM`
/// means it exists and belongs to somebody else. Used to tell a pane whose
/// process EXITED apart from one whose process table could not be read,
/// which are the same `None` from a `ps` that failed and must not be the
/// same decision.
pub(crate) fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 delivers nothing and only inspects.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// What one observation could prove about who is in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Occupant {
    /// The foreground process group leader, read now.
    Leader(i32),
    /// The pane's process is gone. Proven, and prior state may retire.
    Gone,
    /// Nothing could be read. Not evidence, and nothing may retire on it.
    Unprovable,
}

/// Read a pane's foreground leader, keeping "gone" apart from "unknown".
pub(crate) fn occupant_of(pane_pid: i32) -> Occupant {
    match foreground_pid_checked(pane_pid) {
        Some(leader) => Occupant::Leader(leader),
        None if !pid_alive(pane_pid) => Occupant::Gone,
        None => Occupant::Unprovable,
    }
}

pub(crate) fn foreground_pid_checked(pane_pid: i32) -> Option<i32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "tpgid=", "-p", &pane_pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tpgid(&String::from_utf8_lossy(&out.stdout))
}

/// Change a pane's hold, but only if a named delivery still owns it.
///
/// Evidence arrives late. A delivery that resolved on screen evidence
/// stays in the acknowledgement registry, its barrier releases, and the
/// NEXT delivery claims the composer. A correlated acknowledgement for
/// the first one can land after all of that, and an unowned mutation
/// would then move a barrier belonging to a delivery it says nothing
/// about, binding or releasing the wrong turn.
///
/// So the token that claimed the barrier is what may change it, and the
/// token is required: a delivery that never claimed one has nothing to
/// settle, and letting it through unowned is the same defect by a
/// shorter route. A receipt whose owner no longer matches still resolves
/// its own delivery; it just does not touch somebody else's composer.
pub(crate) fn set_hold_owned(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    change: impl FnOnce(ComposerHold) -> Option<ComposerHold>,
) -> bool {
    let (prior_ready, now_key, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        if entry.hold_owner.as_deref() != Some(owner) {
            return false;
        }
        let Some(hold) = change(entry.hold) else {
            // The caller declined to change it, which is not a failure:
            // the barrier that is already there is the one it wanted.
            return true;
        };
        if hold == entry.hold {
            return true;
        }
        let prior_ready = readiness_key(entry);
        entry.hold = hold;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, hold);
        (prior_ready, readiness_key(entry), entry.detection.clone())
    };
    wake_readiness_after_mutation(inner, session_idx, pane_id, prior_ready, now_key, &det);
    true
}

/// Bind an acknowledged turn to the barrier a delivery is holding.
///
/// This is what puts a pane on the exact lifecycle. Until a hold carries
/// a turn key it runs on the screen, where a delayed end from the
/// PREVIOUS turn is indistinguishable from this one's and can release a
/// payload nothing consumed. The key names the turn that took this
/// delivery, so only that turn's own end can end it.
///
/// One transaction over both stores, in this function's usual order,
/// detections then turn ends. The pin has to be in place before the hold
/// starts waiting on the key, or a burst of later ends can evict the one
/// piece of evidence that would release it.
///
/// Refuses without touching anything when the barrier belongs to another
/// delivery, when the pane's binding cannot be named, or when the hold is
/// already waiting on a DIFFERENT turn. Binding the same key again is
/// idempotent: an acknowledgement can arrive more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundTurn {
    /// The exact end was already durable when this turn took the pin.
    /// Callers retain this fact even if reconciliation consumes the store
    /// before they publish the corresponding receipt.
    pub(crate) end_already_present: bool,
}

pub(crate) fn bind_turn(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    turn: turnkey::TurnKey,
    since_ms: u64,
) -> Option<BoundTurn> {
    let (prior_ready, now_key, det, end_already_present) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let pane = PaneKey::new(session_idx, pane_id);
        let entry = map.get_mut(&pane)?;
        if entry.hold_owner.as_deref() != Some(owner) {
            return None;
        }
        // The end store is keyed on the pane's binding, so an unnamed
        // binding has nothing to key on.
        let (Some(agent), Some(manifest)) = (entry.agent, entry.manifest.clone()) else {
            return None;
        };
        if entry.turn.as_ref().is_some_and(|t| *t != turn) {
            return None;
        }
        let mut turn_ends = inner.turn_ends.lock().expect("turn ends lock");
        if !turnkey::PaneEnds::pin(&mut turn_ends, &pane, agent, &manifest, &turn) {
            return None;
        }
        let end_already_present =
            turnkey::PaneEnds::holds(&turn_ends, &pane, agent, &manifest, &turn);
        entry.turn = Some(turn);
        // Only a hold that is still WAITING takes the mark. One that
        // already carries a witnessed edge has stronger evidence than an
        // acknowledgement's timestamp.
        if entry.hold.is_waiting() {
            entry.hold = ComposerHold::TurnStarted { since_ms };
        }
        let prior_ready = readiness_key(entry);
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (
            prior_ready,
            readiness_key(entry),
            entry.detection.clone(),
            end_already_present,
        )
    };
    wake_readiness_after_mutation(inner, session_idx, pane_id, prior_ready, now_key, &det);
    Some(BoundTurn {
        end_already_present,
    })
}

/// Claim the composer barrier for one delivery attempt, at the write
/// boundary.
///
/// Exactly one attempt owns it at a time, and the owner travels with it
/// so delayed evidence cannot settle somebody else's barrier: a hook
/// upgrade for a delivery that finished long ago must not promote or
/// clear a hold belonging to the payload sitting in the composer now.
///
/// Success means this owner holds it: a fresh claim, or the same owner
/// claiming again. A different owner refuses, and its caller must not
/// write.
pub(crate) fn claim_hold(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: Option<crate::identity::ProcId>,
    manifest: Option<&str>,
) -> bool {
    let (prior_ready, now_key, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        // An admitted agent is a POSITIVE prerequisite for a write, not
        // the absence of a mismatch. A manifest can be pinned by an
        // operator, which chooses rules without authenticating anything,
        // so a shell prompt sitting under an always-idle rule set can
        // look write-ready. Comparing two `None`s as equal is how a
        // payload reaches that shell.
        let Some(agent) = agent else {
            return false;
        };
        // And it has to be the binding the caller proved, or this is a
        // different pane occupant than the one it is about to write to.
        if entry.agent != Some(agent) || entry.manifest.as_deref() != manifest {
            return false;
        }
        // A fresh claim requires a composer this daemon believes is
        // EMPTY and unclaimed. An unowned barrier is not free to take: it
        // is what the sensors raised because somebody's text is in there,
        // and a human typing between the last capture and this moment
        // produces exactly that. Only the same owner may re-claim a
        // barrier it already holds.
        match (entry.hold, entry.hold_owner.as_deref()) {
            (ComposerHold::Clear, None) => {}
            (_, Some(held)) if held == owner => {}
            _ => return false,
        }
        let prior_ready = readiness_key(entry);
        entry.hold_owner = Some(owner.to_string());
        entry.hold = ComposerHold::Staged;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (prior_ready, readiness_key(entry), entry.detection.clone())
    };
    wake_readiness_after_mutation(inner, session_idx, pane_id, prior_ready, now_key, &det);
    true
}

/// Release this attempt's barrier after proving that no terminal command byte was written.
///
/// This covers a refused durable write fact and a paste command whose first
/// pipe write failed before accepting bytes. Exact owner and binding checks
/// prevent a failed attempt from clearing a person's draft or another delivery.
pub(crate) fn release_unwritten_hold(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: crate::identity::ProcId,
    manifest: &str,
) -> bool {
    let (prior_ready, now_key, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        if entry.hold != ComposerHold::Staged
            || entry.hold_owner.as_deref() != Some(owner)
            || entry.agent != Some(agent)
            || entry.manifest.as_deref() != Some(manifest)
        {
            return false;
        }
        let prior_ready = readiness_key(entry);
        entry.hold = ComposerHold::Clear;
        entry.hold_owner = None;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (prior_ready, readiness_key(entry), entry.detection.clone())
    };
    wake_readiness_after_mutation(inner, session_idx, pane_id, prior_ready, now_key, &det);
    true
}

/// Confirm that a guarded resolution still owns the staged composer.
///
/// Exact payload capture proves the bytes. This check proves that no live
/// lifecycle or blocked-state evidence makes a terminal key unsafe.
pub(crate) fn staged_action_ready(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: cyclops_proto::ProcessInstanceId,
    manifest: &str,
) -> bool {
    let expected = crate::identity::ProcId {
        pid: agent.pid(),
        birth: agent.birth(),
    };
    let map = inner.detections.lock().expect("detections lock");
    map.get(&PaneKey::new(session_idx, pane_id))
        .is_some_and(|entry| staged_entry_ready(entry, owner, expected, manifest))
}

fn staged_entry_ready(
    entry: &DetEntry,
    owner: &str,
    agent: crate::identity::ProcId,
    manifest: &str,
) -> bool {
    entry.hold == ComposerHold::Staged
        && entry.hold_owner.as_deref() == Some(owner)
        && entry.agent == Some(agent)
        && entry.manifest.as_deref() == Some(manifest)
        && staged_frame_is_quiet(entry)
}

/// Release this attempt's composer barrier after a guarded resolution.
///
/// The caller has already proved the exact composer bytes. This final
/// check keeps the release bound to the process generation and manifest
/// recorded before the paste, so it cannot clear another occupant's hold.
/// The exact settlement may race a lifecycle recompute that promotes the
/// owned hold after the composer was cleared. That promotion must not leave
/// an already-settled notification blocking the pane.
pub(crate) async fn resolve_staged_hold(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: cyclops_proto::ProcessInstanceId,
    manifest: &str,
) -> bool {
    let pane = PaneKey::new(session_idx, pane_id);
    let recompute_gate = pane_recompute_gate(inner, &pane);
    let _recompute_guard = recompute_gate.lock().await;
    let expected = crate::identity::ProcId {
        pid: agent.pid(),
        birth: agent.birth(),
    };
    let (prior_ready, now_key, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&pane) else {
            return false;
        };
        if entry.hold == ComposerHold::Clear
            || entry.hold_owner.as_deref() != Some(owner)
            || entry.agent != Some(expected)
            || entry.manifest.as_deref() != Some(manifest)
        {
            return false;
        }
        if let Some(turn) = entry.turn.as_ref() {
            turnkey::PaneEnds::retire(
                &mut inner.turn_ends.lock().expect("turn ends lock"),
                &pane,
                expected,
                manifest,
                turn,
            );
        }
        let prior_ready = readiness_key(entry);
        entry.hold = ComposerHold::Clear;
        entry.hold_owner = None;
        entry.turn = None;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (prior_ready, readiness_key(entry), entry.detection.clone())
    };
    wake_readiness_after_mutation(inner, session_idx, pane_id, prior_ready, now_key, &det);
    true
}

/// Wake anyone gating on this pane's readiness when the answer moved.
///
/// Broadcast only. Runtime state and write-readiness move independently,
/// so a pane can refuse and then allow with no state edge between: a
/// composer hold lifting is exactly that shape, and a delivery sleeping
/// on the refusal would sleep through its own release. A `state` line
/// would be a transition that never happened, so this is its own event
/// and it names no ledger line.
///
/// First sight is not a public readiness change. A caller carrying a causal
/// token may still reconcile it. Tokenless status, lifecycle, and synthetic
/// captures are observational only and never create route evidence.
///
/// What a readiness wake compares: the public write verdict and, third,
/// whether the pane's own staged notification is action-ready for its
/// owner. The third component never makes the pane write-ready for a
/// follower; it exists because an owned staged doorbell keeps the honest
/// state at `idle_with_input`, which leaves the public pair unchanged
/// across the very transition (working to idle-class) that must wake the
/// exact-owned reconciliation. Without it that reconciliation is never
/// requested and a claimed doorbell is never cleared.
type ReadinessKey = (bool, Option<String>, bool);

/// Is this pane's own staged hold ready for its owner's action? The same
/// evidence `staged_entry_ready` demands, minus the owner and binding
/// identity, which the reconciliation seam re-proves itself.
fn staged_hold_ready(entry: &DetEntry) -> bool {
    entry.hold == ComposerHold::Staged && entry.hold_owner.is_some() && staged_frame_is_quiet(entry)
}

/// Is this frame quiet enough for the owner's own action on its staged
/// notification? Idle-class fused states qualify. `Unknown` qualifies only
/// when it is the honest reading of a staged row: the screen read the row as
/// human input (never a ghost or a bare prompt), every retained reading is
/// idle-class (an active start's Working reading refuses), a current Screen
/// reading exists (hook-only readings, a failed or an empty capture refuse),
/// the capture is fresh and out of mode. Blocked states never qualify. The exact bytes are
/// proven again by the caller before any key is sent.
fn staged_frame_is_quiet(entry: &DetEntry) -> bool {
    let idle_class =
        |state: AgentState| matches!(state, AgentState::Idle | AgentState::IdleWithInput);
    let readings_quiet = entry
        .detection
        .readings
        .iter()
        .any(|reading| reading.sensor == Sensor::Screen)
        && entry
            .detection
            .readings
            .iter()
            .all(|reading| idle_class(reading.state));
    let unknown_staged = entry.detection.state == AgentState::Unknown
        && entry.detection.composer_semantic == Some(ComposerSemantic::HumanInput);
    !entry.in_mode
        && !entry.detection.stale
        && (idle_class(entry.detection.state) || unknown_staged)
        && readings_quiet
}

/// Runtime-idle admission by process-bound liveness. A separate verdict from
/// lifecycle termination: it answers "may a wake start now?" for a pane whose
/// lifecycle evidence is absent (a fresh pane before its first completed
/// turn, or a pane whose completed suffix scrolled away), never "did the
/// turn end?". It admits `Idle` only when the fused state is `unknown`
/// because the clean composer is not lifecycle evidence, the exact current
/// agent generation has produced an authenticated `SessionStart` or
/// `UserPromptSubmit` edge in this pane lifetime (telemetry and attention
/// edges never qualify), no start is active for that
/// generation, and the current capture is a nonstale, out-of-mode,
/// binding-stable frame whose screen winner is a clean idle-class composer
/// row. It never touches a hook entry and cannot end a latch.
#[allow(clippy::too_many_arguments)]
fn liveness_admits_idle(
    state: AgentState,
    decided_by: &str,
    screen: Option<&CompiledRule>,
    has_screen_reading: bool,
    liveness_verified: bool,
    active_start: bool,
    stale: bool,
    in_mode: bool,
    binding_stable: bool,
) -> bool {
    state == AgentState::Unknown
        && decided_by == "idle_unconfirmed"
        && screen.is_some_and(|rule| {
            matches!(rule.state, AgentState::Idle | AgentState::IdleWithInput)
                && rule.composer_semantic == Some(ComposerSemantic::Clean)
        })
        && has_screen_reading
        && liveness_verified
        && !active_start
        && !stale
        && !in_mode
        && binding_stable
}

/// What runtime-idle admission decided for this recompute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionOutcome {
    /// Admission did not apply: not an unknown clean frame, or refused by
    /// one of the distinct outcomes (draft, ghost, ambiguous, stale, mode,
    /// binding change, active start).
    NotApplicable,
    /// Every predicate held and a qualifying edge from this daemon boot
    /// exists: the pane is idle.
    Admitted,
    /// Every predicate held except a qualifying edge from this daemon boot.
    /// The pane stays unknown and its write block is named
    /// `hook_admission_unproven`: a durable, recoverable pre-write block,
    /// never a replay of an older boot's edge.
    Unproven,
}

#[allow(clippy::too_many_arguments)]
fn liveness_idle_admission(
    inner: &Inner,
    route: &PaneKey,
    admitted: Option<crate::identity::ProcId>,
    manifest_id: Option<&str>,
    screen: Option<&CompiledRule>,
    binding_stable: bool,
    in_mode: bool,
    detection: &mut Detection,
) -> AdmissionOutcome {
    let (Some(agent), Some(manifest_id)) = (admitted, manifest_id) else {
        return AdmissionOutcome::NotApplicable;
    };
    let liveness_verified = inner
        .manifests
        .get(manifest_id)
        .is_some_and(crate::selftest::declares_hooks)
        && inner
            .hook_liveness
            .seen_admitting_edge(route, agent, manifest_id);
    let active_start = inner
        .hook_readings
        .lock()
        .expect("hook readings lock")
        .get(route)
        .is_some_and(|entry| entry.active_start_for(agent, Some(manifest_id)));
    let has_screen_reading = detection
        .readings
        .iter()
        .any(|reading| reading.sensor == Sensor::Screen);
    let frame_admissible = liveness_admits_idle(
        detection.state,
        &detection.decided_by,
        screen,
        has_screen_reading,
        true,
        active_start,
        detection.stale,
        in_mode,
        binding_stable,
    );
    if !frame_admissible {
        return AdmissionOutcome::NotApplicable;
    }
    if !liveness_verified {
        return AdmissionOutcome::Unproven;
    }
    detection.state = AgentState::Idle;
    detection.decided_by = format!(
        "liveness:{}",
        screen.map(|rule| rule.id.as_str()).unwrap_or("screen")
    );
    AdmissionOutcome::Admitted
}

fn readiness_key(entry: &DetEntry) -> ReadinessKey {
    (
        entry.detection.write_ready,
        entry.detection.write_block.clone(),
        staged_hold_ready(entry),
    )
}

/// Callers compute `now` from the freshly stamped entry under their own
/// `detections` guard; this function never takes that lock, because several
/// callers still hold it here.
fn wake_readiness(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    prior: Option<ReadinessKey>,
    now: ReadinessKey,
    det: &Detection,
    route_evidence: Option<&NotificationRouteEvidenceId>,
) {
    let decision = readiness_wake_decision(prior.as_ref(), &now);
    if decision.emit_public {
        inner.emit(
            "readiness",
            serde_json::json!({
                "pane_id": pane_id,
                "session_idx": session_idx,
                "write_ready": det.write_ready,
                "write_block": det.write_block,
            }),
            None,
        );
    }
    if decision.reconcile_route {
        if let Some(route_evidence) = route_evidence {
            crate::messaging::schedule_route_evidence(inner, session_idx, pane_id, route_evidence);
        }
    }
}

/// Publish a readiness mutation, minting one token only when it creates a
/// route-reconciliation edge.
fn wake_readiness_after_mutation(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    prior: ReadinessKey,
    now: ReadinessKey,
    det: &Detection,
) {
    let route_evidence = readiness_wake_decision(Some(&prior), &now)
        .reconcile_route
        .then(|| inner.advance_route_evidence(session_idx, pane_id));
    wake_readiness(
        inner,
        session_idx,
        pane_id,
        Some(prior),
        now,
        det,
        route_evidence.as_ref(),
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReadinessWakeDecision {
    emit_public: bool,
    reconcile_route: bool,
}

fn readiness_wake_decision(
    prior: Option<&ReadinessKey>,
    now: &ReadinessKey,
) -> ReadinessWakeDecision {
    let public_changed = prior.is_some_and(|prior| (&prior.0, &prior.1) != (&now.0, &now.1));
    let staged_changed = prior.is_some_and(|prior| prior.2 != now.2);
    ReadinessWakeDecision {
        emit_public: public_changed,
        reconcile_route: prior.is_none() || public_changed || staged_changed,
    }
}

/// Keep a pane's last verdict after its capture failed, as a refusal.
///
/// Reporting keeps the last known answer; writing must not. Marking it
/// stale is what stops the gate from reading a retained clean composer as
/// permission to paste (rule 12), and the restamp is where that becomes
/// a refusal rather than a fact each reader has to re-derive.
///
/// It is written back, not just returned. The map is what status and
/// every other consumer read, so handing the refusal to the immediate
/// caller alone would leave all of them on the pre-failure record, which
/// still says write_ready. `since` is left alone: the state did not
/// change, only the confidence in it.
fn retain_stale(
    map: &mut std::collections::HashMap<PaneKey, DetEntry>,
    pane: &PaneKey,
    in_mode: bool,
    occupant: Option<i32>,
    manifest: Option<&str>,
) -> Option<Detection> {
    // Exact match on both, and an unprovable occupant matches nothing.
    // A pane id names a place: an agent can exit and another start at the
    // same shell prompt, inheriting the pane id, the root pid and often
    // the manifest too. Retaining on the pane id alone would hand the
    // newcomer a turn its predecessor was having, and the stale flag does
    // not repair that. It blocks the write; the record still names the
    // wrong agent as working.
    let entry = map.get_mut(pane).filter(|e| {
        occupant.is_some() && e.occupant == occupant && e.manifest.as_deref() == manifest
    })?;
    let mut p = entry.detection.clone();
    p.stale = true;
    let p = p.stamped(in_mode, entry.hold);
    entry.detection = p.clone();
    let attempt = entry.composer.notification_attempt.or_else(|| {
        entry
            .hold_owner
            .as_deref()
            .and_then(|owner| NotificationAttemptId::parse(owner).ok())
    });
    let candidate_count = entry.composer.candidate_count as usize;
    entry.composer = ambiguous_composer_projection(
        attempt,
        ComposerProof::Unprovable,
        "detection_stale",
        candidate_count,
    );
    entry.working_confirmed = false;
    Some(p)
}

/// A `ps -o tpgid=` line as a pid. A pane with no controlling terminal
/// reports -1, which names no process and must not be looked up.
pub(crate) fn parse_tpgid(line: &str) -> Option<i32> {
    let value: i32 = line.trim().parse().ok()?;
    (value > 0).then_some(value)
}

/// Bind a manifest by the argv[0] basename of a pane's foreground process,
/// memoising the reading only once it has actually bound something. The ps
/// spawn runs when comm binding already missed; never on a clock.
///
/// The asymmetry, remember a hit and re-probe a miss, is load-bearing, not
/// an optimisation. Vendor CLIs ship a shell wrapper that re-execs itself
/// in place, so the pid is stable across the exec while argv[0] flips from
/// the wrapper's interpreter to the agent's own name. cursor-agent's
/// wrapper ends in `exec -a "$0" "$NODE_BIN" ... index.js "$@"`, and
/// MEASURED (cursor-agent 2026.07.23-e383d2b) pid 37750 read:
///
/// ```text
/// t+0.00s  ps args = bash /Users/x/.local/bin/agent    -> "bash",  binds nothing
/// t+0.25s  ps args = /Users/x/.local/bin/agent ...     -> "agent", binds cursor
/// ```
///
/// Recomputes are output-driven and typing `agent` at a prompt echoes that
/// line immediately, so the probe lands in the first window often. Keyed on
/// (pane, pid), a cache that remembered "bash" could never correct itself.
/// The pid never changes, and the pane would read unknown, carry no state
/// and refuse delivery for the rest of that process's life. So a basename
/// that binds nothing means "not settled yet", never "no agent here".
///
/// One entry per pane: the foreground pid changes with every job the shell
/// runs, and keeping the losers would grow the map for the pane's whole
/// life without any of them ever being read again.
pub(crate) fn argv_bound_manifest<'a>(
    inner: &'a Inner,
    session_idx: usize,
    pane_id: &str,
    pid: i32,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    // Keyed by process IDENTITY, not by the number. A pid is transferable:
    // an agent exits, the kernel hands its number to something unrelated,
    // and a cache keyed on the number alone would answer "claude" for a
    // process that has never been claude. On an authentication path that
    // is not a stale read, it is an admission of the wrong process.
    //
    // The identity read is the same kernel record the ancestry walk uses,
    // so it costs no extra spawn, and a process that has exited cannot be
    // identified at all, which fails closed.
    argv_bound_with(
        inner,
        session_idx,
        pane_id,
        pid,
        argv_basename,
        crate::identity::ProcId::of,
    )
}

/// The body of [`argv_bound_manifest`], with both observations injected so
/// a test can interleave a pid reuse between them.
fn argv_bound_with<'a, A, I>(
    inner: &'a Inner,
    session_idx: usize,
    pane_id: &str,
    pid: i32,
    read_argv: A,
    read_ident: I,
) -> Option<(&'a Manifest, crate::identity::ProcId)>
where
    A: Fn(i32) -> Option<String>,
    I: Fn(i32) -> Option<crate::identity::ProcId>,
{
    let proc = read_ident(pid)?;
    let pane = PaneKey::new(session_idx, pane_id);
    let key = (pane.clone(), proc);
    let cached = inner
        .argv_cache
        .lock()
        .expect("argv cache lock")
        .get(&key)
        .cloned();
    if let Some(base) = cached {
        return manifest_for_basename(&inner.manifests, &base).map(|m| (m, proc));
    }
    // Spawn outside the lock: ps is slower than every other holder of it.
    let base = read_argv(pid)?;
    // The identity was read BEFORE the argv, and they are two separate
    // observations of a system that does not hold still. A process can
    // exit and its number be handed on between them, in which case this
    // argv describes the REPLACEMENT while the key names the predecessor.
    // Filing that would authorize the newcomer under an identity it never
    // had, so the identity is re-proven against the same birth before
    // anything is written down or returned.
    if read_ident(pid) != Some(proc) {
        return None;
    }
    let bound = manifest_for_basename(&inner.manifests, &base)?;
    let mut cache = inner.argv_cache.lock().expect("argv cache lock");
    cache.retain(|(cached_pane, _), _| cached_pane != &pane);
    // One entry per pane, so a reused pid cannot even collide with a
    // stale sibling entry: the pane's previous binding is already gone.
    cache.insert(key, base);
    // The identity is returned WITH the manifest, from this one verified
    // observation. Handing back only the manifest would leave the caller
    // to re-read the identity, and a process replaced between the two
    // reads would pair one process's identity with another's rules.
    Some((bound, proc))
}

/// argv[0] basename of a live pid via `ps -o args=`.
fn argv_basename(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_argv_basename(&String::from_utf8_lossy(&out.stdout))
}

/// First whitespace-separated token of a ps args line, basename only.
pub(crate) fn parse_argv_basename(args_line: &str) -> Option<String> {
    let first = args_line.split_whitespace().next()?;
    let base = first.rsplit('/').next()?;
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// What a recompute should do with the stored hook reading.
#[derive(Debug, PartialEq, Eq)]
enum HookAction {
    /// Reading is live: feed it to fusion.
    Use,
    /// A transient reading is spent by TTL or repeated rules-tier
    /// contradiction. Drop it from the store and fuse without it.
    Drop,
}

/// Age one hook entry against the rules-tier verdict of this recompute.
/// Disagreement only counts when the rules actually decided something;
/// agreement resets the streak.
/// `idle_confirmed` says the selected screen winner certified idle this
/// recompute ([`winner_confirms_idle`]); `in_mode` says the pane is in a
/// tmux mode, where the capture does not show the composer; `binding_stable`
/// says the pane's binding was the same before and after the capture, the
/// bookends that tie the frame to this agent generation.
fn hook_action_observed(
    entry: &mut HookEntry,
    detection: &Detection,
    idle_confirmed: bool,
    in_mode: bool,
    binding_stable: bool,
    now_ms: u64,
) -> HookAction {
    // An authenticated start is a lifecycle fact, not a sample. The idle
    // composer often remains on screen until Claude paints its first output,
    // and repeated captures of that frame do not end the turn. A missing end
    // must remain visible as Working instead of silently ageing to Idle.
    if entry.active_start {
        // A promoted candidate start has no key an end hook could match, so
        // its only screen-side terminal is one observation of a conclusive
        // lifecycle-evidence idle winner on an idle-class fused frame, taken
        // out of mode, nonstale, with the binding proven stable around the
        // capture. A rule that needs more than one observation to be
        // conclusive is not lifecycle evidence at all; a bare, ghosted, or
        // typed composer row is not that evidence (lifecycle_evidence is
        // false on it). An authenticated confirmed start is stronger
        // evidence than any screen frame and ends only on the hook tier or a
        // binding change: the idle composer stays on screen until the first
        // output and must never erase it.
        if entry.promoted && entry.active_turn.is_none() {
            let fused_idle = matches!(
                detection.state,
                AgentState::Idle | AgentState::IdleWithInput
            );
            if idle_confirmed && fused_idle && !in_mode && !detection.stale && binding_stable {
                return HookAction::Drop;
            }
        }
        return HookAction::Use;
    }
    if entry.authoritative_end {
        let newer_visual_working = detection.readings.iter().any(|reading| {
            reading.sensor != Sensor::Hook
                && reading.state == AgentState::Working
                && reading.ts >= entry.reading.ts
        });
        return if matches!(
            detection.state,
            AgentState::Idle | AgentState::IdleWithInput
        ) || newer_visual_working
        {
            HookAction::Drop
        } else {
            HookAction::Use
        };
    }
    if now_ms.saturating_sub(entry.reading.ts) > HOOK_READING_TTL_MS {
        return HookAction::Drop;
    }
    if detection.state == AgentState::Unknown {
        return HookAction::Use;
    }
    if detection.state == entry.reading.state {
        entry.disagreements = 0;
        return HookAction::Use;
    }
    entry.disagreements += 1;
    if entry.disagreements >= HOOK_DISAGREE_LIMIT {
        HookAction::Drop
    } else {
        HookAction::Use
    }
}

/// Apply one bound hook reading to the visual verdict.
///
/// An active start owns the runtime answer before the first output frame. The
/// screen still owns blocked states because lifecycle hooks do not observe
/// permission prompts, modals, or quota exhaustion. Disagreement remains
/// explicit, and write-readiness is stamped separately after fusion.
fn apply_hook_reading(
    detection: &mut Detection,
    reading: SensorReading,
    active_start: bool,
    authoritative_end: bool,
) {
    let hook_state = reading.state;
    let hook_rule = reading.rule.clone();
    let hook_ts = reading.ts;
    let visual_state = detection.state;
    let newer_visual_working = detection.readings.iter().any(|visual| {
        visual.sensor != Sensor::Hook && visual.state == AgentState::Working && visual.ts >= hook_ts
    });
    detection.readings.push(reading);

    if authoritative_end && newer_visual_working {
        // Stop ended its bound turn. A later current-level Working reading
        // describes what is on the pane now and cannot be overwritten by
        // retaining that historical edge as an Idle level.
    } else if authoritative_end
        && hook_state == AgentState::Idle
        && matches!(visual_state, AgentState::Unknown | AgentState::Idle)
    {
        detection.state = AgentState::Idle;
        detection.decided_by = format!("hook:{hook_rule}");
    } else if active_start && hook_state == AgentState::Working && !visual_state.is_blocked() {
        if visual_state != AgentState::Unknown && visual_state != AgentState::Working {
            detection.disagreement = true;
        }
        detection.state = AgentState::Working;
        detection.decided_by = format!("hook:{hook_rule}");
    } else if visual_state == AgentState::Unknown {
        detection.state = hook_state;
        detection.decided_by = format!("hook:{hook_rule}");
    } else if hook_state != visual_state {
        detection.disagreement = true;
    }
}

/// Highest-priority pane_title rule matching the title.
pub(crate) fn title_winner<'m>(m: &'m Manifest, title: &str) -> Option<&'m CompiledRule> {
    m.rules
        .iter()
        .find(|r| r.region == Region::PaneTitle && r.matches(title, &[title]))
}

fn manifest_uses_screen_tier(m: &Manifest) -> bool {
    m.rules.iter().any(|rule| rule.region != Region::PaneTitle)
}

/// Highest-priority screen-region rule matching the capture. Region
/// slicing matches `Manifest::evaluate`: bottom N non-empty lines,
/// restored to top-down order. Production goes through
/// [`screen_winner_esc`] (the recompute may carry an escaped capture);
/// this plain form serves the tests that assert single-capture behavior.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn screen_winner<'m>(m: &'m Manifest, screen: &str) -> Option<&'m CompiledRule> {
    screen_winner_esc(m, screen, None)
}

/// [`screen_winner`] with an optional SGR-escaped capture of the same grid
/// (capture-pane -e), so `line_regex_esc` rules can fire. Escaped lines are
/// judged non-empty on their CSI-stripped text, mirroring
/// `Manifest::evaluate_esc`, so both captures slice the same screen rows.
pub(crate) fn screen_winner_esc<'m>(
    m: &'m Manifest,
    screen: &str,
    screen_esc: Option<&str>,
) -> Option<&'m CompiledRule> {
    let (non_empty, non_empty_esc) = screen_regions(screen, screen_esc);
    m.rules
        .iter()
        .find(|r| screen_rule_matches(r, &non_empty, non_empty_esc.as_deref()))
}

/// Bottom-up non-empty rows of a capture and, when an escaped capture
/// exists, the same rows with their SGR bytes.
fn screen_regions<'s>(
    screen: &'s str,
    screen_esc: Option<&'s str>,
) -> (Vec<&'s str>, Option<Vec<&'s str>>) {
    let non_empty: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let non_empty_esc: Option<Vec<&str>> = screen_esc.map(|s| {
        s.lines()
            .rev()
            .filter(|l| !strip_csi(l).trim().is_empty())
            .collect()
    });
    (non_empty, non_empty_esc)
}

fn screen_rule_matches(
    r: &CompiledRule,
    non_empty: &[&str],
    non_empty_esc: Option<&[&str]>,
) -> bool {
    match r.region {
        Region::PaneTitle => false,
        Region::BottomNonEmptyLines(n) => {
            let mut sel: Vec<&str> = non_empty.iter().take(n).copied().collect();
            sel.reverse();
            let esc = non_empty_esc.map(|ne| {
                let mut sel: Vec<&str> = ne.iter().take(n).copied().collect();
                sel.reverse();
                sel
            });
            r.matches_esc(&sel.join("\n"), &sel, esc.as_deref())
        }
    }
}

/// Does the selected screen winner certify an idle-class state? Only the
/// rule the screen tier actually selected may do so, and only when the
/// manifest marks it `lifecycle_evidence`: a composer row measured mid-turn
/// (`lifecycle_evidence = false`) may carry a composer semantic but cannot
/// license an idle verdict, and a low-priority catch-all idle rule that
/// merely also matches underneath a working or blocked winner certifies
/// nothing.
pub(crate) fn winner_confirms_idle(winner: Option<&CompiledRule>) -> bool {
    winner.is_some_and(|rule| {
        rule.lifecycle_evidence
            && matches!(rule.state, AgentState::Idle | AgentState::IdleWithInput)
    })
}

/// Capture the sensor set a manifest needs: the plain grid, plus the
/// SGR-escaped grid when any rule carries `line_regex_esc` clauses (codex
/// ghost vs typed text, F19). A failed escaped capture fails the whole
/// read: with the plain capture alone the esc rules fail closed and typed
/// human text reads as idle, which is the injection hazard they exist to
/// prevent. The caller's doubt handling covers both captures.
async fn capture_screens(
    watcher: &SessionWatcher,
    m: &Manifest,
    pane_id: &str,
) -> Result<(String, Option<String>), TmuxError> {
    if !m.has_escaped_rules() {
        let plain = watcher.client().capture_pane(pane_id).await?;
        return Ok((plain, None));
    }
    let esc = watcher.client().capture_pane_escaped(pane_id).await?;
    let plain = strip_csi(&esc);
    Ok((plain, Some(esc)))
}

/// Fuse the tier winners into a Detection. Both readings are kept whenever
/// both tiers fired, whatever the verdict.
/// `screen_required` says the manifest relies on the screen tier, so the
/// screen was consulted (or should have been and could not be).
/// `idle_confirmed` says the selected screen winner is an idle-class rule the
/// manifest marks `lifecycle_evidence` ([`winner_confirms_idle`]). When the
/// screen tier is required and nothing confirmed idle, no idle-class verdict
/// is published: a non-idle screen rule decides, otherwise the pane is
/// `unknown`, which is never write-ready. A title is a lagging sensor and an
/// idle title over a mid-turn screen is exactly the frame that admits a write
/// into a working pane; a bare or ghosted composer row is measured mid-turn
/// too, so it may carry its composer semantic but not the idle verdict. A
/// manifest without a screen tier keeps deciding by title.
pub(crate) fn fuse(
    m: &Manifest,
    title: Option<&CompiledRule>,
    screen: Option<&CompiledRule>,
    screen_required: bool,
    idle_confirmed: bool,
    ts: u64,
) -> Detection {
    let mut readings = Vec::new();
    if let Some(r) = title {
        readings.push(SensorReading {
            sensor: Sensor::Title,
            state: r.state,
            rule: r.id.clone(),
            ts,
        });
    }
    if let Some(r) = screen {
        readings.push(SensorReading {
            sensor: Sensor::Screen,
            state: r.state,
            rule: r.id.clone(),
            ts,
        });
    }
    // First rule in priority order that one of the tiers selected. Compared
    // by address: both winners are references into m.rules.
    let winner = m.rules.iter().find(|r| {
        let rp: *const CompiledRule = *r;
        title.is_some_and(|t| std::ptr::eq(rp, t)) || screen.is_some_and(|s| std::ptr::eq(rp, s))
    });
    let idle_class =
        |state: AgentState| matches!(state, AgentState::Idle | AgentState::IdleWithInput);
    if screen_required && !idle_confirmed && winner.is_some_and(|w| idle_class(w.state)) {
        return match screen.filter(|s| !idle_class(s.state)) {
            Some(s) => Detection {
                state: s.state,
                disagreement: true,
                decided_by: s.id.clone(),
                stale: false,
                write_ready: false,
                write_block: None,
                composer_semantic: s.composer_semantic,
                readings,
            },
            None => Detection {
                state: AgentState::Unknown,
                disagreement: false,
                decided_by: "idle_unconfirmed".into(),
                stale: false,
                write_ready: false,
                write_block: None,
                composer_semantic: screen.and_then(|rule| rule.composer_semantic),
                readings,
            },
        };
    }
    match winner {
        Some(w) => Detection {
            state: w.state,
            disagreement: matches!((title, screen), (Some(t), Some(s)) if t.state != s.state),
            decided_by: w.id.clone(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: screen.and_then(|rule| rule.composer_semantic),
            readings,
        },
        None => Detection {
            state: AgentState::Unknown,
            readings,
            disagreement: false,
            decided_by: "no_rule".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        },
    }
}

/// Advance one pane's composer hold, and settle the turn key it waits on.
///
/// Which lifecycle this hold runs on belongs to the HOLD, not to the
/// vendor. A hold carrying a bound turn key runs exact: only that turn's
/// own end ends it, matched structurally and never by arrival time,
/// because an end can be observed before the start it belongs to.
///
/// A hold with no bound key runs on the screen, even where the vendor is
/// capable of naming its turns. Reading the lane off the manifest
/// instead would wedge every keyed vendor whose hook was never installed,
/// or whose exact ACK never arrived: the hold would wait on an end that
/// nobody is going to send. A late matching ACK can still upgrade the
/// same owner to the exact lane; what it cannot do is resurrect a hold
/// the screen lane already released, because the owner no longer matches.
///
/// An end is consumed only once it has DONE something: the hold cleared,
/// or new input superseded the old turn and the hold fell back to
/// `Staged`. Leaving the key pinned in either case would refuse the next
/// distinct start as a hijack, and the pane would never take another
/// turn.
///
/// It has NOT done anything while the hold is still `TurnStarted`. That
/// is an end that landed while a sensor still reads the turn as running,
/// and the release is waiting on the clean frame that follows. Taking
/// the end there spends the only proof this turn ever ended, and the
/// clean frame finds nothing to release against: the barrier holds
/// forever.
fn settle_turn(
    ends: &mut turnkey::Ends,
    pane: &PaneKey,
    agent: Option<crate::identity::ProcId>,
    manifest: Option<&str>,
    turn: Option<&turnkey::TurnKey>,
    hold: ComposerHold,
    det: &Detection,
) -> (ComposerHold, bool) {
    let ended = turn.map(|t| {
        turnkey::PaneEnds::holds(
            ends,
            pane,
            agent.expect("a carried turn implies a proven agent"),
            manifest.unwrap_or_default(),
            t,
        )
    });
    let next = hold.advance(det, ended);
    if let (Some(t), Some(agent), Some(id)) = (turn, agent, manifest) {
        match next {
            // Still waiting on this turn's own end.
            ComposerHold::TurnStarted { .. } => {}
            // Released on that end. Consume it, all or nothing.
            ComposerHold::Clear if ended == Some(true) => {
                turnkey::PaneEnds::take(ends, pane, agent, id, t);
            }
            // The hold stopped waiting on this turn WITHOUT its end:
            // new input superseded it, or the pane died. Retire the pin
            // either way, or the next distinct start is refused as a
            // hijack against a turn nobody will ever end.
            _ => {
                turnkey::PaneEnds::retire(ends, pane, agent, id, t);
            }
        }
    }
    // A hold still waiting on an end that the store may have thrown away
    // is waiting on nothing. It stays refused, because releasing on
    // absent evidence is the failure this whole lane exists to prevent,
    // but it stops being an ordinary wait and says so.
    let stranded = ended == Some(false)
        && matches!(next, ComposerHold::TurnStarted { .. })
        && match (turn, agent, manifest) {
            (Some(_), Some(agent), Some(id)) => {
                turnkey::PaneEnds::evidence_lost(ends, pane, agent, id)
            }
            _ => false,
        };
    (next, stranded)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposerCapture {
    NotRead,
    Visible(String),
    Hidden,
    Unprovable,
    BindingChanged,
}

impl From<crate::delivery::ComposerContentProof> for ComposerCapture {
    fn from(proof: crate::delivery::ComposerContentProof) -> Self {
        match proof {
            crate::delivery::ComposerContentProof::Visible(content) => Self::Visible(content),
            crate::delivery::ComposerContentProof::Hidden => Self::Hidden,
            crate::delivery::ComposerContentProof::Unsupported
            | crate::delivery::ComposerContentProof::Unprovable => Self::Unprovable,
        }
    }
}

fn composer_capture_binding_is_stable(
    row_before: &PaneRow,
    row_after: Option<&PaneRow>,
    recipient_before: Option<RecipientKey>,
    recipient_after: Option<RecipientKey>,
    binding_before: &BindingObservation,
    binding_after: Option<&BindingObservation>,
) -> bool {
    let Some(row_after) = row_after else {
        return false;
    };
    !row_after.dead
        && !row_after.in_mode
        && row_after.pane_pid == row_before.pane_pid
        && recipient_after == recipient_before
        && binding_after == Some(binding_before)
}

fn semantic_composer_projection(semantic: Option<ComposerSemantic>) -> ComposerProjection {
    match semantic {
        Some(ComposerSemantic::Clean) => ComposerProjection {
            state: ComposerState::ComposerClean,
            proof: ComposerProof::ManifestRule,
            notification_attempt: None,
            reason: None,
            candidate_count: 0,
            binding: None,
        },
        Some(ComposerSemantic::HumanInput) => ComposerProjection {
            state: ComposerState::HumanDraft,
            proof: ComposerProof::ManifestRule,
            notification_attempt: None,
            reason: None,
            candidate_count: 0,
            binding: None,
        },
        Some(ComposerSemantic::GhostSuggestion) => ComposerProjection {
            state: ComposerState::VendorGhostSuggestion,
            proof: ComposerProof::ManifestRule,
            notification_attempt: None,
            reason: None,
            candidate_count: 0,
            binding: None,
        },
        Some(ComposerSemantic::Ambiguous) => ComposerProjection {
            state: ComposerState::ComposerAmbiguous,
            proof: ComposerProof::Ambiguous,
            notification_attempt: None,
            reason: Some("manifest_rule_ambiguous"),
            candidate_count: 0,
            binding: None,
        },
        None => ComposerProjection::default(),
    }
}

fn ambiguous_composer_projection(
    attempt: Option<NotificationAttemptId>,
    proof: ComposerProof,
    reason: &'static str,
    candidate_count: usize,
) -> ComposerProjection {
    ComposerProjection {
        state: ComposerState::ComposerAmbiguous,
        proof,
        notification_attempt: attempt,
        reason: Some(reason),
        candidate_count: u32::try_from(candidate_count).unwrap_or(u32::MAX),
        binding: None,
    }
}

fn claimed_legacy_recovery_ready(
    detection: &Detection,
    in_mode: bool,
    detection_manifest: Option<&str>,
    binding: &cyclops_proto::NotificationBinding,
    capture: &ComposerCapture,
) -> bool {
    crate::composer_recovery::clean_composer_for_binding(
        detection,
        in_mode,
        detection_manifest,
        binding,
    ) && detection.composer_semantic == Some(ComposerSemantic::Clean)
        && matches!(capture, ComposerCapture::Visible(content) if content.is_empty())
}

fn notification_submission_recorded(record: &cyclops_proto::NotificationRecord) -> bool {
    match record.state {
        NotificationState::Submitted | NotificationState::Notified => true,
        NotificationState::AttentionRequired => matches!(
            record.cause,
            Some(
                NotificationAttentionCause::ReceiptOccupantChanged
                    | NotificationAttentionCause::AckTimeout
            )
        ),
        NotificationState::Queued
        | NotificationState::Gating
        | NotificationState::BlockedPreWrite
        | NotificationState::QuotaHeld
        | NotificationState::QuotaResetObserved
        | NotificationState::Writing
        | NotificationState::Staged
        | NotificationState::Submitting
        | NotificationState::Withdrawn
        | NotificationState::WithdrawnAfterStaging
        | NotificationState::WithdrawnByOperator
        | NotificationState::Superseded => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn project_composer(
    semantic: Option<ComposerSemantic>,
    owner: Option<&str>,
    detection: &Detection,
    in_mode: bool,
    binding: &BindingObservation,
    recipient: Option<RecipientKey>,
    capture: &ComposerCapture,
    candidates: &[crate::mailbox::ActiveComposerNotification],
    candidate_store_available: bool,
) -> ComposerProjection {
    let parsed_owner = owner.and_then(|value| NotificationAttemptId::parse(value).ok());
    if !candidate_store_available {
        return ambiguous_composer_projection(
            parsed_owner,
            ComposerProof::Unprovable,
            "notification_store_unavailable",
            candidates.len(),
        );
    }
    if in_mode {
        return ambiguous_composer_projection(
            parsed_owner,
            ComposerProof::Ambiguous,
            "pane_in_mode",
            candidates.len(),
        );
    }
    if detection.stale {
        return ambiguous_composer_projection(
            parsed_owner,
            ComposerProof::Unprovable,
            "detection_stale",
            candidates.len(),
        );
    }
    if matches!(capture, ComposerCapture::BindingChanged) {
        return ambiguous_composer_projection(
            parsed_owner,
            ComposerProof::Ambiguous,
            "binding_changed_during_capture",
            candidates.len(),
        );
    }
    if owner.is_none() && candidates.is_empty() {
        return semantic_composer_projection(semantic);
    }
    if owner.is_some() && parsed_owner.is_none() && candidates.is_empty() {
        return ambiguous_composer_projection(
            None,
            ComposerProof::Unprovable,
            "direct_delivery_hold_unprovable",
            0,
        );
    }

    let attempt =
        match (parsed_owner, candidates) {
            (Some(attempt), [candidate]) if candidate.record.attempt_id == attempt => attempt,
            (None, [candidate]) => {
                let reason =
                    if candidate.record.binding.as_ref().is_none_or(|binding| {
                        binding.pane_root.is_none() || binding.leader.is_none()
                    }) {
                        "durable_binding_incomplete"
                    } else {
                        "notification_owner_missing"
                    };
                return ambiguous_composer_projection(
                    Some(candidate.record.attempt_id),
                    ComposerProof::Unprovable,
                    reason,
                    1,
                );
            }
            (Some(attempt), []) => {
                return ambiguous_composer_projection(
                    Some(attempt),
                    ComposerProof::Ambiguous,
                    "notification_attempt_mismatch",
                    0,
                );
            }
            (Some(attempt), [_]) => {
                return ambiguous_composer_projection(
                    Some(attempt),
                    ComposerProof::Ambiguous,
                    "notification_attempt_mismatch",
                    1,
                );
            }
            (owner, _) => {
                return ambiguous_composer_projection(
                    owner,
                    ComposerProof::Ambiguous,
                    "multiple_active_notifications",
                    candidates.len(),
                );
            }
        };
    let candidate = &candidates[0];

    if matches!(binding, BindingObservation::Unprovable) {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "binding_unprovable",
            candidates.len(),
        );
    }
    if detection.disagreement
        || detection.state.is_blocked()
        || matches!(
            binding,
            BindingObservation::NotVendor | BindingObservation::Gone
        )
    {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Ambiguous,
            "terminal_state_unsafe",
            candidates.len(),
        );
    }
    if semantic.is_none() {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_semantic_unprovable",
            candidates.len(),
        );
    }

    let Some(current_recipient) = recipient else {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "recipient_unprovable",
            candidates.len(),
        );
    };
    let Some(expected_binding) = candidate.record.binding.as_ref() else {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "durable_binding_incomplete",
            candidates.len(),
        );
    };
    let BindingObservation::Bound(current_binding) = binding else {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "binding_unprovable",
            candidates.len(),
        );
    };
    let Some(expected_pane_root) = expected_binding.pane_root else {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "durable_binding_incomplete",
            candidates.len(),
        );
    };
    let Some(expected_leader) = expected_binding.leader else {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "durable_binding_incomplete",
            candidates.len(),
        );
    };
    let current_pane_root = ProcessInstanceId::new(
        current_binding.pane_root.pid,
        current_binding.pane_root.birth,
    )
    .ok();
    let current_leader =
        ProcessInstanceId::new(current_binding.leader.pid, current_binding.leader.birth).ok();
    let current_agent =
        ProcessInstanceId::new(current_binding.agent.pid, current_binding.agent.birth).ok();
    if expected_binding.recipient != candidate.record.recipient
        || current_recipient != candidate.record.recipient
        || Some(expected_pane_root) != current_pane_root
        || Some(expected_leader) != current_leader
        || Some(expected_binding.agent) != current_agent
        || expected_binding.manifest.as_str() != current_binding.manifest
    {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Ambiguous,
            "binding_mismatch",
            candidates.len(),
        );
    }

    let Some(expected) = candidate.message.as_ref().and_then(|message| {
        crate::delivery::expected_notification_payload(&candidate.record, message)
    }) else {
        return ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "notification_payload_unprovable",
            candidates.len(),
        );
    };
    match capture {
        ComposerCapture::Visible(actual)
            if actual == &expected && semantic == Some(ComposerSemantic::HumanInput) =>
        {
            ComposerProjection {
                // Visible exact bytes are staged regardless of what the durable
                // submit path previously recorded. This is the swallowed-submit
                // case status must expose rather than declaring consumed.
                state: ComposerState::CyclopsNotificationStaged,
                proof: ComposerProof::ExactNotification,
                notification_attempt: Some(attempt),
                reason: None,
                candidate_count: 1,
                binding: Some(current_binding.clone()),
            }
        }
        ComposerCapture::Visible(actual)
            if semantic == Some(ComposerSemantic::Clean)
                && actual.is_empty()
                && notification_submission_recorded(&candidate.record) =>
        {
            ComposerProjection {
                state: ComposerState::CyclopsNotificationSubmitted,
                proof: ComposerProof::ExactNotification,
                notification_attempt: Some(attempt),
                reason: None,
                candidate_count: 1,
                binding: Some(current_binding.clone()),
            }
        }
        ComposerCapture::Visible(_) => ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Ambiguous,
            "composer_content_mismatch",
            candidates.len(),
        ),
        ComposerCapture::Hidden => ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_hidden",
            candidates.len(),
        ),
        ComposerCapture::NotRead => ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_not_read",
            candidates.len(),
        ),
        ComposerCapture::Unprovable => ambiguous_composer_projection(
            Some(attempt),
            ComposerProof::Unprovable,
            "composer_capture_unprovable",
            candidates.len(),
        ),
        ComposerCapture::BindingChanged => unreachable!("handled before candidate projection"),
    }
}

fn pane_recompute_gate(inner: &Arc<Inner>, pane: &PaneKey) -> Arc<tokio::sync::Mutex<()>> {
    let mut gates = inner
        .pane_recomputes
        .lock()
        .expect("pane recompute gates lock");
    Arc::clone(
        gates
            .entry(pane.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

/// Recompute one pane's Detection, update the cache, and emit a "state"
/// event when the fused state changed. Manifests with screen rules always run
/// the full sensor set. `force_screen` additionally captures for title-only
/// manifests used by explicit inspection paths.
/// Returns None when the pane is gone from the table.
///
/// `session_idx` is the caller's stable session-slot index, not re-derived
/// here from `watcher.session()`: see [`crate::emit_state`]'s doc comment
/// for the rename race that distinction closes. Every call site already
/// has one, from wherever it entered the session (an event's own
/// `session_task`, a resolved recipient, a delivery handle).
pub(crate) async fn recompute_pane(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &SessionWatcher,
    pane_id: &str,
    force_screen: bool,
    cause: &str,
) -> Option<Detection> {
    recompute_pane_with_evidence(
        inner,
        session_idx,
        watcher,
        pane_id,
        force_screen,
        cause,
        None,
        None,
    )
    .await
}

/// Recompute while reusing one supplied route-evidence token.
///
/// Causal event sources mint the token before entering. Synthetic
/// reconciliation supplies the current token so it cannot create an edge.
pub(crate) async fn recompute_pane_for_route_evidence(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &SessionWatcher,
    pane_id: &str,
    force_screen: bool,
    cause: &str,
    route_evidence: &NotificationRouteEvidenceId,
) -> Option<Detection> {
    recompute_pane_with_evidence(
        inner,
        session_idx,
        watcher,
        pane_id,
        force_screen,
        cause,
        None,
        Some(route_evidence),
    )
    .await
}

/// Recompute after a settled output burst while preserving when that output
/// was observed. The capture may run later, but it must not confirm a hook
/// edge that arrived after the bytes which triggered it.
pub(crate) async fn recompute_pane_from_output(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &SessionWatcher,
    pane_id: &str,
    force_screen: bool,
    cause: &str,
    evidence_ms: u64,
    route_evidence: &NotificationRouteEvidenceId,
) -> Option<Detection> {
    recompute_pane_with_evidence(
        inner,
        session_idx,
        watcher,
        pane_id,
        force_screen,
        cause,
        Some(evidence_ms),
        Some(route_evidence),
    )
    .await
}

async fn recompute_pane_with_evidence(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &SessionWatcher,
    pane_id: &str,
    force_screen: bool,
    cause: &str,
    source_evidence_ms: Option<u64>,
    route_evidence: Option<&NotificationRouteEvidenceId>,
) -> Option<Detection> {
    let route = PaneKey::new(session_idx, pane_id);
    let prior_working_confirmed = cached_working_confirmed(inner, session_idx, pane_id);
    // One pane has one observation timeline. The capture is part of the
    // transaction: serializing only the cache write would still let a stale
    // clean frame captured first commit after a newer draft or Working frame.
    // The map keeps one stable gate per route for the daemon's lifetime so a
    // waiter can never race a newly-created replacement gate.
    let recompute_gate = pane_recompute_gate(inner, &route);
    let _recompute_guard = recompute_gate.lock().await;
    let lifecycle_observation = LifecycleObservation::from_cause(cause);
    let Some(row) = watcher.pane(pane_id) else {
        inner
            .detections
            .lock()
            .expect("detections lock")
            .remove(&route);
        cancel_lifecycle_recheck(inner, &route);
        return None;
    };
    let recovery_recipient =
        crate::composer_recovery::exact_recipient(inner, session_idx, watcher, &row);
    let (recovery_records, recovery_store_error) = match recovery_recipient {
        Some(recipient) => match crate::composer_recovery::active_for_recipient(inner, recipient) {
            Ok(records) => (records, None),
            Err(reason) => (Vec::new(), Some(reason)),
        },
        None => (Vec::new(), None),
    };
    let (composer_candidates, composer_store_available) = match recovery_recipient {
        Some(recipient) => match inner
            .mailbox
            .as_ref()
            .and_then(|service| service.active_composer_notifications(recipient).ok())
        {
            Some(candidates) => (candidates, true),
            None => (Vec::new(), false),
        },
        None => (Vec::new(), true),
    };
    let recovering = !recovery_records.is_empty() || recovery_store_error.is_some();
    let manifest = bind_manifest_for(inner, session_idx, &row);
    let manifest_id = manifest.map(|m| m.agent.id.clone());
    // Resolved once, before anything that needs it: the pane's admitted
    // AGENT, which is the domain hook reports are filed under. The
    // foreground leader can be a helper the agent spawned, and the pane
    // root is a shell that outlives every agent run in it.
    //
    // Out here because it can spawn a process, and nothing waits on the
    // detection cache while `ps` runs.
    let seen = occupant_of(row.pane_pid);
    let occupant = match seen {
        Occupant::Leader(leader) => Some(leader),
        Occupant::Gone | Occupant::Unprovable => None,
    };
    let binding_observation = pane_binding_observation(inner, session_idx, &row, seen);
    let unobservable = binding_observation == BindingObservation::Unprovable;
    let observed_binding = match &binding_observation {
        BindingObservation::Bound(binding) => Some(binding.clone()),
        BindingObservation::NotVendor
        | BindingObservation::Gone
        | BindingObservation::Unprovable => None,
    };
    let admitted = match &binding_observation {
        BindingObservation::Bound(binding) => Some(binding.agent),
        BindingObservation::NotVendor
        | BindingObservation::Gone
        | BindingObservation::Unprovable => None,
    };
    // A live admitted binding fills a transient gap in the independent rule
    // lookup. A configured pin or command match still wins when present.
    let manifest_id = manifest_id.or_else(|| match &binding_observation {
        BindingObservation::Bound(binding) => Some(binding.manifest.clone()),
        BindingObservation::NotVendor
        | BindingObservation::Gone
        | BindingObservation::Unprovable => None,
    });
    let observed_manifest = manifest_id.as_deref();
    let cached_binding_rebound = inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&route)
        .is_some_and(|entry| {
            binding_replacement_proven(
                entry.agent,
                entry.manifest.as_deref(),
                &binding_observation,
                observed_manifest,
            )
        });
    // Candidate ingress can establish the replacement binding before this
    // recompute sees it. Compare against the store's own exact binding, not
    // only the detection cache, which may correctly be empty after an earlier
    // unprovable observation.
    let lifecycle_binding_rebound = {
        let mut lifecycle = inner.hook_lifecycle.lock().expect("hook lifecycle lock");
        match (&binding_observation, observed_manifest) {
            (BindingObservation::Bound(binding), Some(manifest)) => {
                lifecycle.forget_if_binding_differs(&route, binding.agent, manifest)
            }
            (BindingObservation::NotVendor | BindingObservation::Gone, _) => {
                lifecycle.forget(&route);
                true
            }
            (BindingObservation::Bound(_), None) | (BindingObservation::Unprovable, _) => false,
        }
    };
    if cached_binding_rebound || lifecycle_binding_rebound {
        // Keep a bucket already rebound by record_start/record_end. In every
        // proven replacement case the old worker must stop.
        cancel_lifecycle_recheck_task(inner, &route);
        // A current-binding edge can land between retirement and task
        // cancellation. Re-arm after cancellation so either ordering leaves
        // one worker attached to the surviving bucket.
        schedule_lifecycle_recheck(inner, &route);
    }
    // A process that EXITED is proof: whatever it was holding is gone
    // with it, and prior state retires normally. A process table that
    // could not be READ is not proof of anything. Nothing this pane holds
    // was disproved by a lookup that failed to answer, so the binding,
    // the hold, its owner and the turn it waits on are frozen below
    // rather than recomputed, and the verdict refuses.
    let prior_hold_owner = inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&route)
        .and_then(|entry| entry.hold_owner.clone());
    let inspect_composer = prior_hold_owner.is_some() || !composer_candidates.is_empty();
    let mut composer_capture = ComposerCapture::NotRead;

    // Kept for the emitted event: the verdict below consumes manifest_id.
    let source_manifest = manifest_id.clone().unwrap_or_default();
    let ts = unix_ms();
    // State and ledger timestamps name this capture. Lifecycle eligibility
    // names the event that caused it. Capping protects the same ordering if
    // the system clock moves while an output burst is settling.
    let evidence_ms = source_evidence_ms.unwrap_or(ts).min(ts);
    let candidate_lane = manifest.is_some_and(|m| {
        m.hooks.turn_start_evidence == LifecycleCertainty::Candidate
            || m.hooks.turn_end_evidence == LifecycleCertainty::Candidate
    });

    if !row.dead && row.in_mode && !recovering {
        // Copy mode and other pane modes gate delivery; they are not agent
        // states. Keep the prior verdict; status exposes in_mode per row.
        //
        // Resolved before the lock: it spawns a process, and nothing else
        // in the daemon should wait on the detection cache while a `ps`
        // runs.
        let mut map = inner.detections.lock().expect("detections lock");
        let prior_ready = map.get(&route).map(readiness_key);
        // Stamped INTO the cache, not just onto the returned copy. The
        // cache is what status and pane.read read, so stamping only the
        // return value would leave every surface reporting the readiness
        // this pane had before the human started scrolling.
        let det = match map.get_mut(&route) {
            Some(e) => {
                let same_binding = unobservable
                    || (e.agent == admitted && e.manifest.as_deref() == manifest_id.as_deref());
                // Only an observation that answered may rewrite the
                // binding: overwriting it with the nothing a failed
                // lookup returned would strand the hold it protects.
                if !unobservable {
                    e.binding = observed_binding.clone();
                    e.manifest = manifest_id.clone();
                    e.occupant = occupant;
                    e.agent = admitted;
                }
                if !same_binding {
                    e.quota_screen_clear = false;
                }
                e.in_mode = true;
                e.detection = e.detection.clone().stamped(true, e.hold);
                if unobservable {
                    e.detection = e.detection.clone().occupant_unprovable();
                }
                e.composer = project_composer(
                    e.detection.composer_semantic,
                    e.hold_owner.as_deref(),
                    &e.detection,
                    true,
                    &binding_observation,
                    recovery_recipient,
                    &ComposerCapture::NotRead,
                    &composer_candidates,
                    composer_store_available,
                );
                e.detection.clone()
            }
            None => {
                let mut det = Detection {
                    state: AgentState::Unknown,
                    readings: Vec::new(),
                    disagreement: false,
                    decided_by: "pane_in_mode".into(),
                    stale: false,
                    write_ready: false,
                    write_block: None,
                    composer_semantic: None,
                }
                .stamped(true, ComposerHold::default());
                if unobservable {
                    det = det.occupant_unprovable();
                }
                let composer = project_composer(
                    det.composer_semantic,
                    None,
                    &det,
                    true,
                    &binding_observation,
                    recovery_recipient,
                    &ComposerCapture::NotRead,
                    &composer_candidates,
                    composer_store_available,
                );
                map.insert(
                    route.clone(),
                    DetEntry {
                        detection: det.clone(),
                        binding: observed_binding.clone(),
                        manifest: manifest_id.clone(),
                        occupant,
                        agent: admitted,
                        in_mode: true,
                        quota_screen_clear: false,
                        hold: ComposerHold::default(),
                        turn: None,
                        hold_owner: None,
                        composer,
                        working_confirmed: false,
                        since: std::time::Instant::now(),
                    },
                );
                det
            }
        };
        let now_key = map
            .get(&route)
            .map(readiness_key)
            .unwrap_or_else(|| (det.write_ready, det.write_block.clone(), false));
        drop(map);
        if candidate_lane {
            if let (Some(agent), Some(manifest)) = (admitted, manifest_id.as_deref()) {
                reconcile_unkeyed_dispatch_start_with_evidence(
                    inner,
                    &route,
                    agent,
                    manifest,
                    &det,
                    true,
                    ts,
                    evidence_ms,
                    true,
                    lifecycle_observation,
                );
            }
        }
        // Entering a mode refuses a write without touching the runtime
        // state, so this is the wake that has no state edge behind it.
        wake_readiness(
            inner,
            session_idx,
            pane_id,
            prior_ready,
            now_key,
            &det,
            route_evidence,
        );
        if !is_candidate_recheck_cause(cause) {
            schedule_lifecycle_recheck(inner, &route);
        }
        return Some(det);
    }

    let mut capture_binding_changed = false;
    let mut idle_confirmed = false;
    let mut screen_winner_id: Option<String> = None;
    let mut admission = AdmissionOutcome::NotApplicable;
    let mut detection = if row.dead {
        Detection {
            state: AgentState::Dead,
            readings: Vec::new(),
            disagreement: false,
            decided_by: "pane_dead".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
    } else if let Some(m) = manifest {
        let t_rule = title_winner(m, &row.title);
        // A title cannot establish that the screen contains no stronger or
        // safer current evidence. When a manifest declares a screen tier, the
        // same recompute always evaluates it. `force_screen` retains its
        // meaning for title-only manifests used by explicit inspection paths.
        let need_screen = force_screen || manifest_uses_screen_tier(m);
        let mut capture_failed = false;
        let (screen, screen_esc) = if need_screen {
            match capture_screens(watcher, m, pane_id).await {
                Ok((s, esc)) => (Some(s), esc),
                Err(e) => {
                    // Sensor failure is doubt, not evidence: keep the prior
                    // verdict rather than flipping state on a broken read.
                    debug!(pane = pane_id, error = %e, "capture failed; keeping prior state");
                    let retained = {
                        let mut map = inner.detections.lock().expect("detections lock");
                        let prior_ready = map.get(&route).map(readiness_key);
                        let retained = retain_stale(
                            &mut map,
                            &route,
                            row.in_mode,
                            foreground_pid_checked(row.pane_pid),
                            manifest_id.as_deref(),
                        );
                        let now_key = map
                            .get(&route)
                            .map(readiness_key)
                            .unwrap_or((false, None, false));
                        retained.map(|det| (prior_ready, now_key, det))
                    };
                    if let Some((prior_ready, now_key, p)) = retained {
                        // The refusal is news like any other: a pane that
                        // was write-ready and is now refused on stale
                        // evidence has to wake whoever was gating on the
                        // old answer.
                        wake_readiness(
                            inner,
                            session_idx,
                            pane_id,
                            prior_ready,
                            now_key,
                            &p,
                            route_evidence,
                        );
                        if !is_candidate_recheck_cause(cause) {
                            schedule_lifecycle_recheck(inner, &route);
                        }
                        return Some(p);
                    }
                    // Nothing cached describes whoever is in the pane now,
                    // so there is nothing to retain. Fall through and let
                    // the title tier answer for the current occupant: a
                    // fresh reading of a different sensor is not
                    // inheritance, it refuses the write on its own (no
                    // screen reading, rule 12), and a verdict with no
                    // reading at all is relabelled sensor_error below.
                    capture_failed = true;
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        if inspect_composer {
            composer_capture = match watcher.client().capture_pane_joined_escaped(pane_id).await {
                Ok(joined) => {
                    crate::delivery::composer_content_for_projection_from_joined_capture(m, &joined)
                        .into()
                }
                Err(_) => ComposerCapture::NotRead,
            };
        }
        capture_binding_changed = if need_screen || inspect_composer {
            let row_after_capture = watcher.pane(pane_id);
            let recipient_after_capture = row_after_capture.as_ref().and_then(|current| {
                crate::composer_recovery::exact_recipient(inner, session_idx, watcher, current)
            });
            let binding_after_capture = row_after_capture.as_ref().map(|current| {
                let seen_after_capture = occupant_of(current.pane_pid);
                pane_binding_observation(inner, session_idx, current, seen_after_capture)
            });
            !composer_capture_binding_is_stable(
                &row,
                row_after_capture.as_ref(),
                recovery_recipient,
                recipient_after_capture,
                &binding_observation,
                binding_after_capture.as_ref(),
            )
        } else {
            false
        };
        if capture_binding_changed {
            // Screen bytes authorize only the exact process generations that
            // surrounded the capture. A replacement during either capture
            // makes both readiness and composer ownership unprovable.
            composer_capture = ComposerCapture::BindingChanged;
        }
        let s_rule = screen
            .as_deref()
            .and_then(|s| screen_winner_esc(m, s, screen_esc.as_deref()));
        idle_confirmed = winner_confirms_idle(s_rule);
        screen_winner_id = s_rule.map(|rule| rule.id.clone());
        let mut det = fuse(
            m,
            t_rule,
            s_rule,
            manifest_uses_screen_tier(m),
            idle_confirmed,
            ts,
        );
        // No prior to fall back on and the screen sensor errored: the rule
        // set was never fully consulted, and the record must not claim it
        // was (GOALS: the record never lies).
        if capture_failed {
            det.stale = true;
            if det.decided_by == "no_rule" || det.decided_by == "idle_unconfirmed" {
                det.decided_by = "sensor_error".into();
            }
        }
        if capture_binding_changed {
            det = det.refused("binding_changed_during_capture");
        }
        det
    } else {
        Detection {
            state: AgentState::Unknown,
            readings: Vec::new(),
            disagreement: false,
            decided_by: "no_manifest".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
    };

    if candidate_lane
        && !row.dead
        && !detection.stale
        && !capture_binding_changed
        && lifecycle_observation.is_visual()
    {
        inner
            .hook_lifecycle
            .lock()
            .expect("hook lifecycle lock")
            .note_visual_change(&route);
    }
    if candidate_lane && !row.dead && !detection.stale {
        if let (Some(agent), Some(manifest)) = (admitted, manifest_id.as_deref()) {
            reconcile_unkeyed_dispatch_start_with_evidence(
                inner,
                &route,
                agent,
                manifest,
                &detection,
                row.in_mode,
                ts,
                evidence_ms,
                !capture_binding_changed,
                lifecycle_observation,
            );
        }
    }
    let confirmed_candidates = if candidate_lane && !row.dead {
        match (admitted, manifest_id.as_deref()) {
            (Some(agent), Some(manifest)) => reconcile_candidate_lifecycles_with_evidence(
                inner,
                &route,
                agent,
                manifest,
                &detection,
                row.in_mode,
                ts,
                evidence_ms,
                !capture_binding_changed,
                lifecycle_observation,
            ),
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    for confirmed in &confirmed_candidates {
        if confirmed.terminal {
            crate::delivery::prepare_dispatch_ack(
                inner,
                session_idx,
                pane_id,
                confirmed.edge.agent,
                &confirmed.edge.manifest,
                &confirmed.edge.turn,
            );
        }
        crate::composer_recovery::bind_post_recovery_turn(
            inner,
            session_idx,
            pane_id,
            confirmed.edge.turn.clone(),
            confirmed.accepted_ms,
        );
    }

    // Hook sensor (agent.state.report): high-precision edges, incomplete
    // coverage. An authenticated start owns runtime Working before visual
    // output appears. Rules still own blocked states because no tested CLI
    // hooks its modals or quota. Other hook readings decide only where rules
    // see nothing and remain subject to transient-reading eviction.
    if !row.dead {
        let hook = {
            let mut map = inner.hook_readings.lock().expect("hook readings lock");
            match map.get_mut(&route) {
                None => None,
                // A reading from a different occupant, or read under rules
                // this pane no longer uses, is not a reading about this
                // pane at all. Dropping it is the point: kept, it would
                // let a predecessor's turn describe its successor.
                // Attribution FAILED, which is doubt, not proof of a
                // different occupant. The reading is not used this round
                // and is not destroyed either: a single transient `ps`
                // failure would otherwise delete a turn-end edge the hold
                // is waiting for, and the hold would then wait forever
                // for evidence that had already arrived and been thrown
                // away.
                Some(_) if admitted.is_none() => None,
                Some(entry) if !entry.describes(admitted, manifest_id.as_deref()) => {
                    map.remove(&route);
                    None
                }
                Some(entry) => {
                    match hook_action_observed(
                        entry,
                        &detection,
                        idle_confirmed,
                        row.in_mode,
                        !capture_binding_changed,
                        ts,
                    ) {
                        HookAction::Use => Some((
                            entry.reading.clone(),
                            entry.active_start,
                            entry.authoritative_end,
                        )),
                        HookAction::Drop => {
                            map.remove(&route);
                            None
                        }
                    }
                }
            }
        };
        if let Some((reading, active_start, authoritative_end)) = hook {
            apply_hook_reading(&mut detection, reading, active_start, authoritative_end);
        }
    }
    if let Some(m) = manifest {
        let screen_rule = screen_winner_id
            .as_deref()
            .and_then(|id| m.rules.iter().find(|rule| rule.id == id));
        admission = liveness_idle_admission(
            inner,
            &route,
            admitted,
            manifest_id.as_deref(),
            screen_rule,
            !capture_binding_changed,
            row.in_mode,
            &mut detection,
        );
    }

    let recovery_live = recovery_recipient.and_then(|recipient| match &binding_observation {
        BindingObservation::Bound(binding) => {
            crate::composer_recovery::observed_binding(recipient, binding)
        }
        BindingObservation::NotVendor
        | BindingObservation::Gone
        | BindingObservation::Unprovable => None,
    });
    let recovery_clean = recovery_live.as_ref().is_some_and(|binding| {
        crate::composer_recovery::clean_composer_for_binding(
            &detection,
            row.in_mode,
            manifest_id.as_deref(),
            binding,
        )
    });
    let exact_claim_after_write = match recovery_records.as_slice() {
        [record] => inner
            .mailbox
            .as_ref()
            .and_then(|service| service.exact_recipient_claimed_after_write(record).ok())
            .unwrap_or(false),
        _ => false,
    };
    let legacy_claimed_clean = exact_claim_after_write
        && recovery_live.as_ref().is_some_and(|binding| {
            claimed_legacy_recovery_ready(
                &detection,
                row.in_mode,
                manifest_id.as_deref(),
                binding,
                &composer_capture,
            )
        });
    let mut recovery_action = if let Some(reason) = recovery_store_error {
        Some(crate::composer_recovery::RecoveryAction::Hold(reason))
    } else {
        inner
            .composer_recovery
            .lock()
            .expect("composer recovery lock")
            .reconcile(
                &recovery_records,
                recovery_live.as_ref(),
                recovery_clean,
                legacy_claimed_clean,
            )
    };
    let retired_attempt = match recovery_action.as_ref() {
        Some(action @ crate::composer_recovery::RecoveryAction::Retire { .. }) => {
            match crate::composer_recovery::persist(inner, action) {
                Ok(attempt_id) => {
                    recovery_action = None;
                    Some(attempt_id)
                }
                Err(reason) => {
                    recovery_action = Some(crate::composer_recovery::RecoveryAction::Hold(reason));
                    None
                }
            }
        }
        _ => None,
    };
    if matches!(
        recovery_action,
        Some(crate::composer_recovery::RecoveryAction::Restore(_))
    ) {
        match crate::composer_recovery::retire_exact_lifecycle(
            inner,
            session_idx,
            pane_id,
            recovery_live.as_ref(),
            recovery_clean,
        ) {
            crate::composer_recovery::LifecycleRetirement::NotReady => {}
            crate::composer_recovery::LifecycleRetirement::Durable(_) => {
                // The matching end is still pinned. Normal settlement below
                // may now clear the runtime hold and consume it.
                recovery_action = None;
            }
            crate::composer_recovery::LifecycleRetirement::Blocked(reason) => {
                recovery_action = Some(crate::composer_recovery::RecoveryAction::Hold(reason));
            }
        }
    }

    let working_confirmed =
        working_is_confirmed(inner, &route, &detection, admitted, manifest_id.as_deref());

    let (prior, prior_ready, now_key, detection, probe_quota_reset, composer_changed) = {
        let mut map = inner.detections.lock().expect("detections lock");
        if matches!(
            recovery_action.as_ref(),
            Some(crate::composer_recovery::RecoveryAction::Hold(
                "composer_recovery_retirement_pending"
            ))
        ) {
            let pending = recovery_records
                .first()
                .and_then(|record| {
                    inner
                        .composer_recovery
                        .lock()
                        .expect("composer recovery lock")
                        .retirement_pending_reason(record.attempt_id)
                })
                .map(crate::composer_recovery::RecoveryAction::Hold);
            recovery_action = pending;
        }
        let prior_entry = map.get(&route);
        let prior = prior_entry.map(|e| e.detection.state);
        let prior_ready = prior_entry.map(readiness_key);
        // The hold describes one AGENT's composer, so it is carried on
        // the vendor identity and its rules, never on the foreground
        // group. A vendor that hands the terminal to a tool it spawned
        // and takes it back changes the foreground group twice without
        // ever ceasing to be the agent holding that composer; carrying
        // the hold on the group would clear it twice, and a runtime
        // `working` state would mask that until the pane came back
        // write-ready with the hold gone.
        let carried = prior_entry
            .filter(|e| admitted.is_some() && e.agent == admitted && e.manifest == manifest_id);
        let prior_quota_screen_clear = carried.is_some_and(|entry| entry.quota_screen_clear);
        // Holds carry only across observations of the same cached agent
        // and manifest. First sight of an occupant therefore starts clear.
        let base_hold = carried.map(|entry| entry.hold).unwrap_or_default();
        let mut turn = carried.and_then(|entry| entry.turn.clone());
        let hold_owner = carried.and_then(|entry| entry.hold_owner.clone());
        let (base_hold, hold_owner, clear_turn, recovery_refusal) =
            crate::composer_recovery::merge_barrier(
                recovery_action.as_ref(),
                retired_attempt,
                base_hold,
                hold_owner,
                detection.turn_running_at().is_some(),
            );
        if clear_turn {
            turn = None;
        }
        // Any unresolved recovered action owns the runtime barrier. It may
        // not fall through the ordinary screen lifecycle: ambiguous restart
        // states require an exact post-recovery start and end, and a failed
        // retirement append must keep both the hold and its end reusable.
        //
        // The one safe transition is the bookkeeping step that ends a turn
        // already running when recovery restored the barrier. That turn
        // cannot consume the payload, so the hold becomes Staged and waits
        // for the next exact start.
        let recovered_hold = recovery_hold_before_durable_retirement(
            recovery_action.as_ref(),
            base_hold,
            &detection,
        );
        // `settle_turn` owns the lane rule. Called here, under both
        // locks, because the advance and the consumption of an exact end
        // are one decision: splitting them leaves a window where another
        // route to this pane sees the hold released while the old key is
        // still pinned, and the next bind is refused as a hijack. Lock
        // order is this function's own, detections then turn ends.
        // An observation that did not answer settles nothing. The
        // binding, the hold, its owner and the turn it waits on are
        // carried forward untouched: none of them were disproved, and
        // recomputing from a screen whose process is unproven is how a
        // barrier gets cleared by a failed `ps`. Runtime state still
        // publishes, so liveness and status keep moving; only the write
        // answer becomes a refusal.
        let frozen = unobservable.then_some(prior_entry).flatten().cloned();
        let (hold, stranded, final_turn, final_owner) = match &frozen {
            Some(entry) => (
                entry.hold,
                false,
                entry.turn.clone(),
                entry.hold_owner.clone(),
            ),
            None if recovered_hold.is_some() => (
                recovered_hold.expect("guarded recovered hold"),
                false,
                turn,
                hold_owner,
            ),
            None => {
                let (hold, stranded) = settle_turn(
                    &mut inner.turn_ends.lock().expect("turn ends lock"),
                    &route,
                    admitted,
                    manifest_id.as_deref(),
                    turn.as_ref(),
                    base_hold,
                    &detection,
                );
                let final_turn = matches!(hold, ComposerHold::TurnStarted { .. })
                    .then_some(turn)
                    .flatten();
                let final_owner = (hold != ComposerHold::Clear)
                    .then_some(hold_owner)
                    .flatten();
                (hold, stranded, final_turn, final_owner)
            }
        };
        // Stamped BEFORE it is cached, because the cache is what the gate
        // and every status surface read. Stamping afterwards would leave
        // them all reading a verdict nobody finished.
        let mut detection = detection.stamped(row.in_mode, hold);
        if unobservable {
            detection = detection.occupant_unprovable();
        } else if stranded {
            detection = detection.refused("turn_evidence_lost");
        }
        if let Some(reason) = recovery_refusal {
            detection = detection.refused(reason);
        }
        // Restart truth: a clean, current, exact-bound frame with no active
        // start and no qualifying edge from THIS daemon boot stays unknown
        // and names its block, so the notification path records a durable,
        // recoverable pre-write block instead of guessing. Applied after the
        // stamp so the name survives readiness computation, like the other
        // named refusals above.
        if admission == AdmissionOutcome::Unproven {
            detection = detection.refused("hook_admission_unproven");
        }
        let composer = project_composer(
            detection.composer_semantic,
            final_owner.as_deref(),
            &detection,
            row.in_mode,
            &binding_observation,
            recovery_recipient,
            &composer_capture,
            &composer_candidates,
            composer_store_available,
        );
        let composer_changed = prior_entry.is_none_or(|entry| entry.composer != composer);
        // A positive screen baseline is enough to discover durable quota
        // holds after restart. Carry that baseline across title-only and
        // hook-only redraws for the same occupant. A positive quota screen
        // clears it and forces screen capture until reset is observed.
        let prior_quota_screen_clear = frozen
            .as_ref()
            .map(|entry| entry.quota_screen_clear)
            .unwrap_or(prior_quota_screen_clear);
        let quota_screen_clear = if unobservable {
            prior_quota_screen_clear
        } else if positive_quota_reset_observation(&detection) {
            true
        } else if detection.state == AgentState::BlockedQuota
            && detection
                .readings
                .iter()
                .any(|reading| reading.sensor == Sensor::Screen)
        {
            false
        } else {
            prior_quota_screen_clear
        };
        let probe_quota_reset =
            !unobservable && quota_reset_probe_needed(prior_quota_screen_clear, &detection);
        // `since` marks the state CHANGING, so a recompute that confirms
        // the same state carries the old mark forward. Without this the
        // elapsed column would reset on every unrelated event.
        let since = match map.get(&route) {
            Some(e) if e.detection.state == detection.state => e.since,
            _ => std::time::Instant::now(),
        };
        map.insert(
            route.clone(),
            DetEntry {
                detection: detection.clone(),
                binding: match &frozen {
                    Some(e) => e.binding.clone(),
                    None => observed_binding,
                },
                // A binding is rewritten only by an observation that
                // answered. Overwriting it with the nothing a failed
                // lookup returned would leave the next SUCCESSFUL
                // recompute unable to match, and it would drop the very
                // hold this froze to protect.
                manifest: match &frozen {
                    Some(e) => e.manifest.clone(),
                    None => manifest_id,
                },
                occupant: match &frozen {
                    Some(e) => e.occupant,
                    None => occupant,
                },
                agent: match &frozen {
                    Some(e) => e.agent,
                    None => admitted,
                },
                in_mode: row.in_mode,
                quota_screen_clear,
                hold,
                turn: final_turn,
                // The claim is retired with the barrier it protected: a
                // cleared hold owns nothing, so the next attempt is free
                // to take it.
                hold_owner: final_owner,
                composer,
                working_confirmed,
                since,
            },
        );
        {
            let now_key = map
                .get(&route)
                .map(readiness_key)
                .unwrap_or((false, None, false));
            (
                prior,
                prior_ready,
                now_key,
                detection,
                probe_quota_reset,
                composer_changed,
            )
        }
    };
    // A readiness change under an UNCHANGED runtime state is still news
    // for anyone gating on it. The hold lifting is the case that matters:
    // the pane reads idle before and after, so no state edge exists, and
    // a delivery sleeping on `not_write_ready:composer_hold` would sleep
    // through its own release. This wake is broadcast only. It is not a
    // state transition and must never be written to the ledger as one.
    wake_readiness(
        inner,
        session_idx,
        pane_id,
        prior_ready,
        now_key,
        &detection,
        route_evidence,
    );
    if probe_quota_reset {
        crate::delivery::observe_quota_reset(inner, session_idx, pane_id);
    }
    // First sight of a pane that reads Unknown is baseline, not a change.
    let state_changed = prior != Some(detection.state)
        && !(prior.is_none() && detection.state == AgentState::Unknown);
    let certainty_changed = detection.state == AgentState::Working
        && prior == Some(AgentState::Working)
        && prior_working_confirmed != working_confirmed;
    let changed = state_changed || certainty_changed;
    if state_changed || composer_changed {
        if let Some(recipient) = recovery_recipient {
            crate::attention_resolution::schedule_exact_owned_reconciliation(inner, recipient);
        }
    }
    if changed {
        debug!(
            pane = pane_id,
            state = %detection.state,
            prior = ?prior,
            cause,
            "fused state changed"
        );
        inner.emit_state(
            session_idx,
            pane_id,
            &detection,
            prior,
            cause,
            (admitted, source_manifest.as_str()),
            working_confirmed,
        );
        // The border says what this row says, from the same edge. No
        // timer, no second rule: an adopted pane's chrome moves exactly
        // when the fused state it names moves.
        crate::repaint_chrome(inner, session_idx, watcher, pane_id).await;
    }
    for confirmed in confirmed_candidates {
        crate::delivery::confirm_dispatch_ack(
            inner,
            session_idx,
            pane_id,
            confirmed.edge.agent,
            &confirmed.edge.manifest,
            &confirmed.edge.turn,
            confirmed.accepted_ms,
        );
    }
    if !is_candidate_recheck_cause(cause) {
        schedule_lifecycle_recheck(inner, &PaneKey::new(session_idx, pane_id));
    }
    Some(detection)
}

fn is_candidate_recheck_cause(cause: &str) -> bool {
    matches!(
        cause,
        "candidate_end_settled"
            | "candidate_visual_end_settled"
            | "unkeyed_dispatch_settled"
            | "continuous_evidence_recheck"
            | "receipt_checkpoint"
    )
}

/// Quota is a screen-only fact. Leaving it requires a fresh, agreeing
/// screen classification, not hook-derived idle or an unknown frame.
fn positive_quota_reset_observation(detection: &Detection) -> bool {
    let state_disproves_quota = detection.state != AgentState::BlockedQuota
        && (detection.state.is_blocked()
            || matches!(
                detection.state,
                AgentState::Idle | AgentState::IdleWithInput | AgentState::Working
            ));
    !detection.stale
        && !detection.disagreement
        && state_disproves_quota
        && detection.readings.iter().any(|reading| {
            reading.sensor == Sensor::Screen
                && reading.state == detection.state
                && reading.state != AgentState::BlockedQuota
        })
}

/// Recheck the cached exact route after a quota hold is made durable.
/// This closes the race where the positive reset edge lands just before
/// the delivery worker appends `QuotaHeld` and therefore finds no target.
pub(crate) fn quota_reset_observed_now(inner: &Inner, session_idx: usize, pane_id: &str) -> bool {
    inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&PaneKey::new(session_idx, pane_id))
        .is_some_and(|entry| entry.quota_screen_clear)
}

fn quota_reset_probe_needed(prior_screen_clear: bool, current: &Detection) -> bool {
    !prior_screen_clear && positive_quota_reset_observation(current)
}

/// Hold recovered runtime state until its retirement fact is durable.
///
/// The only transition allowed before then ends a turn that was already
/// running when the barrier was restored. That turn cannot consume the staged
/// payload, so recovery waits in `Staged` for the next exact start.
fn recovery_hold_before_durable_retirement(
    action: Option<&crate::composer_recovery::RecoveryAction>,
    hold: ComposerHold,
    detection: &Detection,
) -> Option<ComposerHold> {
    let action = action?;
    Some(
        if matches!(action, crate::composer_recovery::RecoveryAction::Restore(_))
            && hold == ComposerHold::StagedDuringTurn
            && detection.turn_running_at().is_none()
        {
            ComposerHold::Staged
        } else {
            hold
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StdMutex;
    use std::collections::HashMap;
    use std::path::Path;

    fn pane() -> PaneKey {
        PaneKey::new(0, "%1")
    }

    const FIXTURE: &str = r#"
[agent]
id = "bash"
display_name = "Bash fixture"
process_names = ["bash"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']

[[rule]]
id = "screen_busy"
state = "working"
priority = 800
region = "bottom_non_empty_lines(3)"
line_regex = ['^FIXPROMPT']
"#;

    const TITLE_AND_COMPOSER_FIXTURE: &str = r#"
[agent]
id = "bash-composer"
display_name = "Bash fixture with a composer rule"
process_names = ["bash"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']

[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 900
region = "bottom_non_empty_lines(3)"
line_regex = ['^\$ $']

[[rule]]
id = "screen_busy"
state = "working"
priority = 800
region = "bottom_non_empty_lines(3)"
line_regex = ['^FIXPROMPT']
"#;

    const CURRENT_TIERS_FIXTURE: &str = r#"
[agent]
id = "current-tiers"
display_name = "Current tiers fixture"
process_names = ["fixture"]

[[rule]]
id = "title_working"
state = "working"
priority = 1100
region = "pane_title"
regex = ['^WORKING']

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']

[[rule]]
id = "screen_modal"
state = "blocked_modal"
priority = 1200
region = "bottom_non_empty_lines(3)"
line_regex = ['^PERMISSION']

[[rule]]
id = "screen_working"
state = "working"
priority = 1150
region = "bottom_non_empty_lines(3)"
line_regex = ['^ACTIVE']
"#;

    fn manifest() -> Manifest {
        Manifest::parse(FIXTURE, Path::new("bash.toml")).unwrap()
    }

    fn current_tiers_manifest() -> Manifest {
        Manifest::parse(CURRENT_TIERS_FIXTURE, Path::new("current-tiers.toml")).unwrap()
    }

    fn quota_detection(sensor: Sensor, state: AgentState) -> Detection {
        Detection {
            state,
            readings: vec![SensorReading {
                sensor,
                state,
                rule: "quota-probe".into(),
                ts: 7,
            }],
            disagreement: false,
            decided_by: "quota-probe".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
    }

    #[test]
    fn first_readiness_sight_reconciles_without_publishing_a_transition() {
        let ready = (true, None, false);
        assert_eq!(
            readiness_wake_decision(None, &ready),
            ReadinessWakeDecision {
                emit_public: false,
                reconcile_route: true,
            }
        );

        assert_eq!(
            readiness_wake_decision(Some(&ready), &ready),
            ReadinessWakeDecision {
                emit_public: false,
                reconcile_route: false,
            }
        );

        let held = (false, Some("composer_hold".to_string()), false);
        assert_eq!(
            readiness_wake_decision(Some(&held), &ready),
            ReadinessWakeDecision {
                emit_public: true,
                reconcile_route: true,
            }
        );
    }
    /// An owned staged doorbell keeps the public pair unchanged across the
    /// working-to-idle-class edge; the third component is what wakes the
    /// exact-owned reconciliation, and it is never a public readiness event.
    #[test]
    fn staged_hold_readiness_alone_reconciles_the_route_without_a_public_wake() {
        let before: ReadinessKey = (false, Some("not_idle".into()), false);
        let after: ReadinessKey = (false, Some("not_idle".into()), true);
        let decision = readiness_wake_decision(Some(&before), &after);
        assert!(decision.reconcile_route);
        assert!(!decision.emit_public);
        let same = readiness_wake_decision(Some(&after), &after);
        assert!(!same.reconcile_route);
        assert!(!same.emit_public);
    }

    #[test]
    fn tokenless_readiness_is_observational_and_a_mutation_mints_once() {
        let inner = inner_with(BTreeMap::new());
        let pane_id = "%1";
        let mut ready = quota_detection(Sensor::Screen, AgentState::Idle);
        ready.write_ready = true;
        let held: ReadinessKey = (false, Some("composer_hold".to_string()), false);
        let ready_key: ReadinessKey = (true, None, false);
        let initial = inner.route_evidence_id(0, pane_id);

        wake_readiness(
            &inner,
            0,
            pane_id,
            Some(held.clone()),
            ready_key.clone(),
            &ready,
            None,
        );
        assert_eq!(inner.route_evidence_id(0, pane_id), initial);

        wake_readiness_after_mutation(&inner, 0, pane_id, held, ready_key, &ready);
        assert_eq!(inner.route_evidence_id(0, pane_id).generation, 1);
    }

    #[test]
    fn quota_reset_store_probe_runs_once_per_positive_screen_edge() {
        let clean = quota_detection(Sensor::Screen, AgentState::Idle);
        let quota = quota_detection(Sensor::Screen, AgentState::BlockedQuota);
        let hook_only = quota_detection(Sensor::Hook, AgentState::Idle);

        assert!(quota_reset_probe_needed(false, &clean));
        assert!(
            !quota_reset_probe_needed(true, &clean),
            "an identical clean redraw repeated quota store work"
        );
        assert!(quota_reset_probe_needed(false, &clean));
        assert!(!quota_reset_probe_needed(false, &hook_only));
        assert!(!quota_reset_probe_needed(true, &quota));

        let mut stale = clean.clone();
        stale.stale = true;
        assert!(!quota_reset_probe_needed(false, &stale));
        let mut disagreeing = clean.clone();
        disagreeing.disagreement = true;
        assert!(!quota_reset_probe_needed(false, &disagreeing));
    }

    #[test]
    fn only_unstable_or_held_evidence_gets_a_bounded_follow_up() {
        let stable = lifecycle_detection(Sensor::Screen, AgentState::Idle);
        assert!(!needs_targeted_reobservation(&stable, ComposerHold::Clear));

        let mut stale = stable.clone();
        stale.stale = true;
        assert!(needs_targeted_reobservation(&stale, ComposerHold::Clear));

        let mut disagreeing = stable.clone();
        disagreeing.disagreement = true;
        assert!(needs_targeted_reobservation(
            &disagreeing,
            ComposerHold::Clear
        ));
        assert!(needs_targeted_reobservation(&stable, ComposerHold::Staged));
    }

    fn lifecycle_detection(sensor: Sensor, state: AgentState) -> Detection {
        Detection {
            state,
            readings: vec![SensorReading {
                sensor,
                state,
                rule: "fixture".into(),
                ts: 10,
            }],
            disagreement: false,
            decided_by: "fixture".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
    }

    fn lifecycle_manifest() -> Manifest {
        Manifest::parse(
            r#"
[agent]
id = "claude"
display_name = "Claude fixture"

[hooks]
turn_start = "UserPromptSubmit"
turn_start_evidence = "candidate"
ack = "UserPromptSubmit"
ack_evidence = "dispatch"
ack_payload_field = "prompt"

[[rule]]
id = "trusted_working"
state = "working"
priority = 100
region = "pane_title"
contains = ["working"]

[[rule]]
id = "advisory_working"
state = "working"
priority = 90
region = "bottom_non_empty_lines(3)"
lifecycle_evidence = false
contains = ["working"]
"#,
            Path::new("claude.toml"),
        )
        .unwrap()
    }

    fn named_lifecycle_detection(rule: &str, sensor: Sensor, state: AgentState) -> Detection {
        let mut detection = lifecycle_detection(sensor, state);
        detection.readings[0].rule = rule.into();
        detection.decided_by = rule.into();
        detection
    }

    #[test]
    fn advisory_working_never_mutates_an_exact_lifecycle() {
        let mut manifests = BTreeMap::new();
        manifests.insert("claude".into(), lifecycle_manifest());
        let inner = inner_with(manifests);
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        let advisory =
            named_lifecycle_detection("advisory_working", Sensor::Screen, AgentState::Working);
        let trusted =
            named_lifecycle_detection("trusted_working", Sensor::Title, AgentState::Working);
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "UserPromptSubmit",
            5,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);

        assert!(reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &advisory,
            false,
            6,
            LifecycleObservation::Stable,
        )
        .is_none());
        assert!(inner.hook_readings.lock().unwrap().get(&pane).is_none());

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &trusted,
            false,
            7,
            LifecycleObservation::Stable,
        )
        .expect("trusted visual evidence confirms the candidate");

        let visual_end = inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .record_visual_end(&pane, agent, "claude", turn.clone(), 8, 3_000)
            .expect("visual end recorded");
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &advisory,
            false,
            9,
            LifecycleObservation::Stable,
        );
        assert!(
            !inner
                .hook_lifecycle
                .lock()
                .unwrap()
                .visual_end_is_current(&pane, &visual_end),
            "obsolete visual-only terminal candidate survived"
        );
        assert!(
            inner
                .hook_readings
                .lock()
                .unwrap()
                .get(&pane)
                .is_some_and(|entry| entry.active_start_for(agent, Some("claude"))),
            "retiring visual-only evidence cleared the exact start"
        );

        let stop = inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .record_end(&pane, agent, "claude", turn, "Stop", 10, 3_000);
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &advisory,
            false,
            11,
            LifecycleObservation::Stable,
        );
        let candidates = inner.hook_lifecycle.lock().unwrap();
        assert!(candidates.end_is_current(&pane, &stop));
    }

    #[test]
    fn candidate_start_needs_a_later_visual_working_observation() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "UserPromptSubmit",
            5,
        );
        let working = lifecycle_detection(Sensor::Title, AgentState::Working);
        assert!(
            reconcile_candidate_lifecycle(
                &inner,
                &pane,
                agent,
                "claude",
                &working,
                false,
                6,
                LifecycleObservation::None,
            )
            .is_none(),
            "the hook-triggered view confirmed its own candidate"
        );
        assert!(inner.hook_readings.lock().unwrap().get(&pane).is_none());

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let confirmed = reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            7,
            LifecycleObservation::Stable,
        )
        .expect("later Working confirms the candidate");
        assert_eq!(confirmed.edge.turn, turn);
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.active_start_for(agent, Some("claude"))));
    }

    #[test]
    fn unkeyed_clean_frames_are_neutral_until_positive_visual_evidence() {
        let mut manifests = BTreeMap::new();
        manifests.insert("claude".into(), lifecycle_manifest());
        let inner = inner_with(manifests);
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            HookEntry::provisional_start(
                agent,
                Some("claude".into()),
                SensorReading {
                    sensor: Sensor::Hook,
                    state: AgentState::Working,
                    rule: "UserPromptSubmit".into(),
                    ts: 1_000,
                },
            ),
        );
        let clean = lifecycle_detection(Sensor::Screen, AgentState::Idle);

        assert!(!reconcile_unkeyed_dispatch_start(
            &inner,
            &pane,
            agent,
            "claude",
            &clean,
            false,
            1_100,
            LifecycleObservation::Stable,
        ));
        assert!(
            inner
                .hook_readings
                .lock()
                .unwrap()
                .get(&pane)
                .is_some_and(|entry| entry.provisional_start_for(agent, Some("claude"))),
            "the immediate clean frame rejected a prompt before output could appear"
        );

        assert!(!reconcile_unkeyed_dispatch_start(
            &inner,
            &pane,
            agent,
            "claude",
            &clean,
            false,
            1_000 + UNKEYED_DISPATCH_SETTLE_MS,
            LifecycleObservation::Stable,
        ));
        assert!(
            inner
                .hook_readings
                .lock()
                .unwrap()
                .get(&pane)
                .is_some_and(|entry| entry.provisional_start_for(agent, Some("claude"))),
            "elapsed time turned a neutral clean frame into rejection evidence"
        );

        assert!(reconcile_unkeyed_dispatch_start(
            &inner,
            &pane,
            agent,
            "claude",
            &named_lifecycle_detection("trusted_working", Sensor::Title, AgentState::Working),
            false,
            1_000 + UNKEYED_DISPATCH_SETTLE_MS + 1,
            LifecycleObservation::Stable,
        ));
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.confirmed_unkeyed_start_for(agent, Some("claude"))));
    }

    #[test]
    fn delayed_output_cannot_confirm_a_newer_unkeyed_start() {
        let mut manifests = BTreeMap::new();
        manifests.insert("claude".into(), lifecycle_manifest());
        let inner = inner_with(manifests);
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            HookEntry::provisional_start(
                agent,
                Some("claude".into()),
                SensorReading {
                    sensor: Sensor::Hook,
                    state: AgentState::Working,
                    rule: "UserPromptSubmit".into(),
                    ts: 1_000,
                },
            ),
        );
        let working =
            named_lifecycle_detection("trusted_working", Sensor::Title, AgentState::Working);

        assert!(!reconcile_unkeyed_dispatch_start_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            1_200,
            900,
            true,
            LifecycleObservation::Stable,
        ));
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.provisional_start_for(agent, Some("claude"))));

        assert!(reconcile_unkeyed_dispatch_start_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            1_300,
            1_100,
            true,
            LifecycleObservation::Stable,
        ));
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.confirmed_unkeyed_start_for(agent, Some("claude"))));
    }

    #[test]
    fn unkeyed_conflicts_override_a_working_title() {
        let mut manifests = BTreeMap::new();
        manifests.insert("claude".into(), lifecycle_manifest());
        let inner = inner_with(manifests);
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };

        for (state, in_mode, stale) in [
            (AgentState::IdleWithInput, false, false),
            (AgentState::BlockedPermission, false, false),
            (AgentState::Idle, true, false),
            (AgentState::Idle, true, true),
        ] {
            inner.hook_readings.lock().unwrap().insert(
                pane.clone(),
                HookEntry::provisional_start(
                    agent,
                    Some("claude".into()),
                    SensorReading {
                        sensor: Sensor::Hook,
                        state: AgentState::Working,
                        rule: "UserPromptSubmit".into(),
                        ts: 1_000,
                    },
                ),
            );
            let mut detection =
                named_lifecycle_detection("trusted_working", Sensor::Title, AgentState::Working);
            detection.readings.push(SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: "conflicting_screen".into(),
                ts: 10,
            });
            detection.stale = stale;
            assert!(!reconcile_unkeyed_dispatch_start(
                &inner,
                &pane,
                agent,
                "claude",
                &detection,
                in_mode,
                2_000,
                LifecycleObservation::Stable,
            ));
            assert!(
                inner.hook_readings.lock().unwrap().get(&pane).is_none(),
                "{state} with pane mode {in_mode} and stale={stale} retained a conflicted unkeyed start"
            );
        }
    }

    #[test]
    fn a_binding_change_during_capture_consumes_no_lifecycle_edge() {
        let mut manifests = BTreeMap::new();
        manifests.insert("claude".into(), lifecycle_manifest());
        let inner = inner_with(manifests);
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            HookEntry::provisional_start(
                agent,
                Some("claude".into()),
                SensorReading {
                    sensor: Sensor::Hook,
                    state: AgentState::Working,
                    rule: "UserPromptSubmit".into(),
                    ts: 1_000,
                },
            ),
        );
        let working =
            named_lifecycle_detection("trusted_working", Sensor::Title, AgentState::Working);

        assert!(!reconcile_unkeyed_dispatch_start_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            2_000,
            2_000,
            false,
            LifecycleObservation::Stable,
        ));
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.provisional_start_for(agent, Some("claude"))));

        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        let stop = inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .record_end(&pane, agent, "claude", turn, "Stop", 1_100, 0);
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let idle = lifecycle_detection(Sensor::Screen, AgentState::Idle);

        assert!(reconcile_candidate_lifecycles_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &idle,
            false,
            2_000,
            2_000,
            false,
            LifecycleObservation::Stable,
        )
        .is_empty());
        assert!(inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .end_is_current(&pane, &stop));
    }

    #[test]
    fn a_later_unkeyed_edge_cannot_confirm_the_edge_it_replaced() {
        let mut manifests = BTreeMap::new();
        manifests.insert("claude".into(), lifecycle_manifest());
        let inner = inner_with(manifests);
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let start = |ts| {
            HookEntry::provisional_start(
                agent,
                Some("claude".into()),
                SensorReading {
                    sensor: Sensor::Hook,
                    state: AgentState::Working,
                    rule: "UserPromptSubmit".into(),
                    ts,
                },
            )
        };
        let first = start(1_000);
        let second = start(1_100);
        assert_eq!(
            first.provisional_edge_for(agent, Some("claude")),
            Some(1_000)
        );
        inner
            .hook_readings
            .lock()
            .unwrap()
            .insert(pane.clone(), second);

        assert!(reconcile_unkeyed_dispatch_start(
            &inner,
            &pane,
            agent,
            "claude",
            &named_lifecycle_detection("trusted_working", Sensor::Title, AgentState::Working),
            false,
            1_101,
            LifecycleObservation::Stable,
        ));
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.confirmed_unkeyed_start_for(agent, Some("claude"))));
    }

    #[test]
    fn an_edge_arriving_during_capture_waits_for_the_next_observation() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            turn,
            "UserPromptSubmit",
            10,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let working = lifecycle_detection(Sensor::Screen, AgentState::Working);
        assert!(
            reconcile_candidate_lifecycle(
                &inner,
                &pane,
                agent,
                "claude",
                &working,
                false,
                10,
                LifecycleObservation::Stable,
            )
            .is_none(),
            "an observation that began with the hook must not confirm it"
        );
        assert!(
            reconcile_candidate_lifecycle(
                &inner,
                &pane,
                agent,
                "claude",
                &working,
                false,
                11,
                LifecycleObservation::Stable,
            )
            .is_some(),
            "the next later observation should confirm the same edge"
        );
    }

    #[test]
    fn clean_composer_preserves_a_dispatch_until_later_working() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "UserPromptSubmit",
            5,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let idle = lifecycle_detection(Sensor::Screen, AgentState::Idle);
        assert!(reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &idle,
            false,
            6,
            LifecycleObservation::Stable,
        )
        .is_none());
        assert!(
            inner
                .hook_lifecycle
                .lock()
                .unwrap()
                .start_for(&pane, agent, "claude")
                .is_none(),
            "the neutral observation was consumed once"
        );

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        assert!(
            reconcile_candidate_lifecycle(
                &inner,
                &pane,
                agent,
                "claude",
                &idle,
                false,
                6_006,
                LifecycleObservation::Stable,
            )
            .is_none(),
            "elapsed time and repeated clean frames are not rejection evidence"
        );

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let working = lifecycle_detection(Sensor::Title, AgentState::Working);
        assert!(
            reconcile_candidate_lifecycle(
                &inner,
                &pane,
                agent,
                "claude",
                &working,
                false,
                6_007,
                LifecycleObservation::Stable,
            )
            .is_some(),
            "the exact candidate must survive Claude's delayed first Working frame"
        );
    }

    #[test]
    fn clean_composer_never_retires_an_active_exact_turn() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            turn,
            "UserPromptSubmit",
            5,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &lifecycle_detection(Sensor::Title, AgentState::Working),
            false,
            6,
            LifecycleObservation::Stable,
        )
        .expect("start confirmed");

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &lifecycle_detection(Sensor::Screen, AgentState::Idle),
            false,
            400,
            LifecycleObservation::Stable,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &lifecycle_detection(Sensor::Screen, AgentState::Idle),
            false,
            10_000,
            LifecycleObservation::Stable,
        );
        assert!(
            inner
                .hook_readings
                .lock()
                .unwrap()
                .get(&pane)
                .is_some_and(|entry| entry.active_start_for(agent, Some("claude"))),
            "repeated settled visual idle fabricated an exact end"
        );
    }

    #[test]
    fn unrelated_candidate_cannot_orphan_an_active_exact_turn() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let first = turnkey::TurnKey::for_test(&["session", "first"]);
        let second = turnkey::TurnKey::for_test(&["session", "second"]);
        let working = lifecycle_detection(Sensor::Screen, AgentState::Working);
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            HookEntry::turn_started(
                agent,
                Some("claude".into()),
                working.readings[0].clone(),
                first.clone(),
            ),
        );
        let visual_end = inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .record_visual_end(&pane, agent, "claude", first, 10, 0)
            .expect("visual end recorded");
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            second.clone(),
            "UserPromptSubmit",
            20,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);

        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &lifecycle_detection(Sensor::Screen, AgentState::Idle),
            false,
            30,
            LifecycleObservation::Stable,
        );

        let mut candidates = inner.hook_lifecycle.lock().unwrap();
        assert!(!candidates.visual_end_is_current(&pane, &visual_end));
        assert!(candidates.has_pending_for(&pane, agent, "claude"));
        candidates.note_visual_change(&pane);
        assert_eq!(
            candidates
                .start_for(&pane, agent, "claude")
                .expect("unrelated start remains pending")
                .turn,
            second
        );
        drop(candidates);
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.active_start_for(agent, Some("claude"))));
    }

    #[test]
    fn a_later_turn_cannot_overwrite_an_active_turns_pending_stop() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let first = turnkey::TurnKey::for_test(&["session", "first"]);
        let second = turnkey::TurnKey::for_test(&["session", "second"]);
        let working = lifecycle_detection(Sensor::Title, AgentState::Working);
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            HookEntry::turn_started(
                agent,
                Some("claude".into()),
                working.readings[0].clone(),
                first.clone(),
            ),
        );
        let first_end = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            first.clone(),
            "Stop",
            10,
            3_000,
        );
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            second.clone(),
            "UserPromptSubmit",
            20,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);

        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            4_000,
            LifecycleObservation::Stable,
        );

        assert!(inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .end_is_current(&pane, &first_end));
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.active_start_matches(agent, Some("claude"), Some(&first))));

        let second_end = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            second.clone(),
            "Stop",
            5_000,
            0,
        );
        let idle = lifecycle_detection(Sensor::Screen, AgentState::Idle);
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &idle,
            false,
            5_001,
            LifecycleObservation::Stable,
        );
        assert!(turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane,
            agent,
            "claude",
            &first,
        ));
        assert!(inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .end_is_current(&pane, &second_end));

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let confirmed = reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &idle,
            false,
            5_002,
            LifecycleObservation::Stable,
        )
        .expect("the second turn settles independently");
        assert_eq!(confirmed.edge.turn, second);
        assert!(confirmed.terminal);
        assert!(inner.hook_readings.lock().unwrap().get(&pane).is_none());
        assert!(!inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .has_pending_for(&pane, agent, "claude"));
    }

    #[test]
    fn delayed_working_evidence_cannot_cancel_a_retried_stop() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        let working = lifecycle_detection(Sensor::Title, AgentState::Working);
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            HookEntry::turn_started(
                agent,
                Some("claude".into()),
                working.readings[0].clone(),
                turn.clone(),
            ),
        );

        let first = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "Stop",
            10,
            0,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            20,
            20,
            LifecycleObservation::Stable,
        );
        assert!(!inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .end_is_current(&pane, &first));

        let retry = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "Stop",
            30,
            0,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            40,
            20,
            LifecycleObservation::Stable,
        );
        assert!(inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .end_is_current(&pane, &retry));
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.active_start_matches(agent, Some("claude"), Some(&turn))));

        let idle = lifecycle_detection(Sensor::Screen, AgentState::Idle);
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &idle,
            false,
            50,
            40,
            LifecycleObservation::Stable,
        );
        assert!(turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane,
            agent,
            "claude",
            &turn,
        ));
        assert!(inner.hook_readings.lock().unwrap().get(&pane).is_none());
    }

    #[test]
    fn working_retires_a_candidate_stop_only_after_its_settle_boundary() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        let working = lifecycle_detection(Sensor::Title, AgentState::Working);
        inner.hook_readings.lock().unwrap().insert(
            pane.clone(),
            HookEntry::turn_started(
                agent,
                Some("claude".into()),
                working.readings[0].clone(),
                turn.clone(),
            ),
        );
        let end = inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .record_end(&pane, agent, "claude", turn, "Stop", 10, 100);

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            20,
            20,
            LifecycleObservation::Stable,
        );
        assert!(inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .end_is_current(&pane, &end));

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle_with_evidence(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            110,
            110,
            LifecycleObservation::Stable,
        );
        assert!(!inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .end_is_current(&pane, &end));
    }

    #[test]
    fn one_terminal_reattach_observation_drains_every_completed_turn() {
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let idle = lifecycle_detection(Sensor::Screen, AgentState::Idle);
        for order in [[0_usize, 1, 2], [2_usize, 1, 0]] {
            let inner = inner_with(BTreeMap::new());
            let turns = [
                turnkey::TurnKey::for_test(&["session", "first"]),
                turnkey::TurnKey::for_test(&["session", "second"]),
                turnkey::TurnKey::for_test(&["session", "third"]),
            ];
            for index in order {
                inner.hook_lifecycle.lock().unwrap().record_start(
                    &pane,
                    agent,
                    "claude",
                    turns[index].clone(),
                    "UserPromptSubmit",
                    5,
                );
                inner.hook_lifecycle.lock().unwrap().record_end(
                    &pane,
                    agent,
                    "claude",
                    turns[index].clone(),
                    "Stop",
                    10,
                    0,
                );
            }
            inner
                .hook_lifecycle
                .lock()
                .unwrap()
                .note_visual_change(&pane);

            let confirmed = reconcile_candidate_lifecycles(
                &inner,
                &pane,
                agent,
                "claude",
                &idle,
                false,
                20,
                LifecycleObservation::Stable,
            );

            let mut actual = confirmed
                .iter()
                .map(|item| item.edge.turn.dedupe_key(""))
                .collect::<Vec<_>>();
            actual.sort();
            let mut expected = turns
                .iter()
                .map(|turn| turn.dedupe_key(""))
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(actual, expected);
            for turn in &turns {
                assert!(turnkey::PaneEnds::holds(
                    &inner.turn_ends.lock().unwrap(),
                    &pane,
                    agent,
                    "claude",
                    turn,
                ));
            }
            assert!(!inner
                .hook_lifecycle
                .lock()
                .unwrap()
                .has_pending_for(&pane, agent, "claude"));
        }
    }

    #[test]
    fn staged_payload_and_blocking_screen_reject_a_dispatch_candidate() {
        for state in [AgentState::IdleWithInput, AgentState::BlockedPermission] {
            let detection = lifecycle_detection(Sensor::Screen, state);
            assert!(
                visual_rejects_start(&detection, false),
                "{state} must not leave a dispatch eligible for a later turn"
            );
        }
    }

    #[test]
    fn candidate_stop_needs_a_quiet_horizon_and_resumed_working_rejects_it() {
        let inner = inner_with(BTreeMap::new());
        let pane = pane();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "prompt"]);
        inner.hook_lifecycle.lock().unwrap().record_start(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "UserPromptSubmit",
            5,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let working = lifecycle_detection(Sensor::Title, AgentState::Working);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            6,
            LifecycleObservation::Stable,
        )
        .expect("start confirmed");

        inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "Stop",
            100,
            3_000,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        let idle = lifecycle_detection(Sensor::Screen, AgentState::Idle);
        assert!(
            reconcile_candidate_lifecycle(
                &inner,
                &pane,
                agent,
                "claude",
                &idle,
                false,
                400,
                LifecycleObservation::Stable,
            )
            .is_none(),
            "ordinary output settling cannot confirm a candidate Stop"
        );
        assert!(inner
            .hook_readings
            .lock()
            .unwrap()
            .get(&pane)
            .is_some_and(|entry| entry.active_start_for(agent, Some("claude"))));
        assert!(!turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane,
            agent,
            "claude",
            &turn
        ));

        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &working,
            false,
            1_650,
            LifecycleObservation::Visual,
        );
        assert!(
            inner
                .hook_lifecycle
                .lock()
                .unwrap()
                .end_for(&pane, agent, "claude", Some(&turn))
                .is_none(),
            "resumed Working must reject Stop before the timer fires"
        );
        inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turn.clone(),
            "Stop",
            2_000,
            3_000,
        );
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &idle,
            false,
            2_300,
            LifecycleObservation::Stable,
        );
        assert!(!turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane,
            agent,
            "claude",
            &turn
        ));
        inner
            .hook_lifecycle
            .lock()
            .unwrap()
            .note_visual_change(&pane);
        reconcile_candidate_lifecycle(
            &inner,
            &pane,
            agent,
            "claude",
            &idle,
            false,
            5_001,
            LifecycleObservation::Stable,
        );
        assert!(turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane,
            agent,
            "claude",
            &turn
        ));
        assert!(inner.hook_readings.lock().unwrap().get(&pane).is_none());
    }

    #[test]
    fn unresolved_recovery_never_enters_the_ordinary_screen_lifecycle() {
        let attempt_id = cyclops_proto::NotificationAttemptId::generate();
        let restore = crate::composer_recovery::RecoveryAction::Restore(attempt_id);
        let hold =
            crate::composer_recovery::RecoveryAction::Hold("composer_recovery_retirement_failed");
        let working = Detection {
            state: AgentState::Working,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Working,
                rule: "working".into(),
                ts: 8,
            }],
            disagreement: false,
            decided_by: "working".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        };
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 9,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        };

        assert_eq!(
            recovery_hold_before_durable_retirement(Some(&restore), ComposerHold::Staged, &working,),
            Some(ComposerHold::Staged),
            "a screen-only start cannot bind recovered state"
        );
        assert_eq!(
            recovery_hold_before_durable_retirement(
                Some(&restore),
                ComposerHold::StagedDuringTurn,
                &clean,
            ),
            Some(ComposerHold::Staged),
            "the pre-recovery turn may end without consuming the payload"
        );
        assert_eq!(
            recovery_hold_before_durable_retirement(
                Some(&hold),
                ComposerHold::TurnStarted { since_ms: 8 },
                &clean,
            ),
            Some(ComposerHold::TurnStarted { since_ms: 8 }),
            "a failed append cannot release an exact recovered turn"
        );
        assert_eq!(
            recovery_hold_before_durable_retirement(
                None,
                ComposerHold::TurnStarted { since_ms: 8 },
                &clean,
            ),
            None,
            "durable retirement returns control to ordinary settlement"
        );
    }

    /// An exact end is evidence, and evidence is not spent until it is
    /// used.
    ///
    /// The bug this pins: the end was consumed the moment it existed,
    /// including when the screen still painted the turn as running and
    /// the hold stayed `TurnStarted`. The clean frame that arrived next
    /// then found no matching end, and the barrier never released. The
    /// end has to survive until it actually moves the hold.
    #[test]
    fn an_end_is_kept_until_it_can_release_the_hold() {
        let screen = |state, rule: &str, ts| Detection {
            state,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: rule.into(),
                ts,
            }],
            disagreement: false,
            decided_by: rule.into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: match state {
                AgentState::Idle => Some(ComposerSemantic::Clean),
                AgentState::IdleWithInput => Some(ComposerSemantic::HumanInput),
                _ => None,
            },
        };
        let working = screen(AgentState::Working, "spinner", 7);
        let clean = screen(AgentState::Idle, "composer_empty", 9);
        let typed = screen(AgentState::IdleWithInput, "composer_text", 9);

        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["s-1", "t-1"]);
        let armed = || {
            let mut ends = turnkey::Ends::new();
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", turn.clone());
            assert!(turnkey::PaneEnds::pin(
                &mut ends,
                &pane(),
                agent,
                "codex",
                &turn
            ));
            ends
        };
        let held =
            |ends: &turnkey::Ends| turnkey::PaneEnds::holds(ends, &pane(), agent, "codex", &turn);
        let started = ComposerHold::TurnStarted { since_ms: 5 };

        // The end lands while a sensor still reads the turn as running.
        // Nothing has released, so nothing is spent.
        let mut ends = armed();
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            started,
            &working,
        )
        .0;
        assert_eq!(hold, started, "a running turn keeps its hold");
        assert!(held(&ends), "the end that has not released yet is kept");

        // The clean frame that follows releases, and consumes it once.
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            hold,
            &clean,
        )
        .0;
        assert_eq!(
            hold,
            ComposerHold::Clear,
            "an ended turn plus a clean composer releases"
        );
        assert!(!held(&ends), "the end that released is consumed");

        // Text in the composer supersedes the old turn: the hold falls
        // back to Staged and the key must not stay pinned, or the next
        // distinct start is refused as a hijack.
        let mut ends = armed();
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            started,
            &typed,
        )
        .0;
        assert_eq!(hold, ComposerHold::Staged);
        assert!(!held(&ends), "a superseded turn releases its key");

        // A hold with no bound key runs on the screen and never touches
        // the end store, even where the vendor can name its turns.
        let mut ends = armed();
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            None,
            started,
            &clean,
        )
        .0;
        assert_eq!(
            hold,
            ComposerHold::Clear,
            "the screen lane releases on a clean composer"
        );
        assert!(held(&ends), "the screen lane consumes nothing");
    }

    /// A hold waiting on evidence the store threw away says so.
    ///
    /// An end can arrive before the start it belongs to, and nothing
    /// protects such an end from a flood of later ones. When the delayed
    /// start finally binds, "no end for this turn" no longer means "the
    /// turn has not ended": it may mean the proof is gone. Waiting on it
    /// is waiting forever, so the verdict stops being an ordinary hold
    /// and carries a reason a person can read.
    ///
    /// The release rule is deliberately unchanged. Releasing on absent
    /// evidence is the exact failure this lane exists to prevent.
    #[test]
    fn a_stranded_hold_says_so_instead_of_waiting() {
        let screen = |state, rule: &str, ts| Detection {
            state,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: rule.into(),
                ts,
            }],
            disagreement: false,
            decided_by: rule.into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: Some(ComposerSemantic::Clean),
        };
        let clean = screen(AgentState::Idle, "composer_empty", 9);

        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let early = turnkey::TurnKey::for_test(&["s-1", "early"]);
        let started = ComposerHold::TurnStarted { since_ms: 5 };
        let step = |ends: &mut turnkey::Ends, manifest: &str, turn: &turnkey::TurnKey, hold| {
            settle_turn(
                ends,
                &pane(),
                Some(agent),
                Some(manifest),
                Some(turn),
                hold,
                &clean,
            )
        };
        // An end, then more distinct ends than the store can hold. The
        // first one was waiting for a start that had not arrived, so
        // nothing protected it.
        let flooded = || {
            let mut ends = turnkey::Ends::new();
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", early.clone());
            for i in 0..turnkey::ENDS_CAP {
                turnkey::PaneEnds::record(
                    &mut ends,
                    &pane(),
                    agent,
                    "codex",
                    turnkey::TurnKey::for_test(&["s-1", &format!("later{i}")]),
                );
            }
            ends
        };

        // The delayed start binds, the composer reads clean, and the end
        // it is waiting on is not there. The hold stands, and it is
        // stranded rather than merely waiting.
        let mut ends = flooded();
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "codex",
            &early
        ));
        assert_eq!(step(&mut ends, "codex", &early, started), (started, true));

        // An unrelated overflow does not stop a turn whose own end IS
        // present from releasing normally.
        let mut ends = flooded();
        let live = turnkey::TurnKey::for_test(&["s-1", "live"]);
        turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", live.clone());
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "codex",
            &live
        ));
        assert_eq!(
            step(&mut ends, "codex", &live, started),
            (ComposerHold::Clear, false),
            "a present end releases, whatever else the store lost"
        );

        // A different rule set on the same pane is a different binding,
        // and a new binding starts with no history and no doubt about it.
        let mut ends = flooded();
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "claude",
            &early
        ));
        assert_eq!(step(&mut ends, "claude", &early, started), (started, false));
    }

    /// A turn the hold stopped waiting on must not stay pinned.
    ///
    /// The bug this pins: the pin was released only by consuming an END.
    /// When new composer input superseded a turn before that turn's end
    /// arrived, the hold moved to `Staged` and dropped the key, but the
    /// pin stayed. Nothing afterwards knew which key to release, so every
    /// later start was refused as a hijack and the pane never took
    /// another turn.
    #[test]
    fn a_superseded_turn_does_not_stay_pinned() {
        let screen = |state, rule: &str, ts| Detection {
            state,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: rule.into(),
                ts,
            }],
            disagreement: false,
            decided_by: rule.into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: match state {
                AgentState::Idle => Some(ComposerSemantic::Clean),
                AgentState::IdleWithInput => Some(ComposerSemantic::HumanInput),
                _ => None,
            },
        };
        let clean = screen(AgentState::Idle, "composer_empty", 9);
        let typed = screen(AgentState::IdleWithInput, "composer_text", 9);

        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let t1 = turnkey::TurnKey::for_test(&["s-1", "t-1"]);
        let t2 = turnkey::TurnKey::for_test(&["s-1", "t-2"]);
        let mut ends = turnkey::Ends::new();
        let started = ComposerHold::TurnStarted { since_ms: 5 };
        // These cases are about the pin, so the stranded flag is dropped
        // here: `a_stranded_hold_says_so_instead_of_waiting` is what
        // covers it.
        let step = |ends: &mut turnkey::Ends, turn: &turnkey::TurnKey, hold, det: &Detection| {
            settle_turn(
                ends,
                &pane(),
                Some(agent),
                Some("codex"),
                Some(turn),
                hold,
                det,
            )
            .0
        };

        // A turn is running and no end has arrived for it.
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "codex",
            &t1
        ));

        // Somebody types. The hold stops waiting on t1 without ever
        // seeing t1 end.
        assert_eq!(step(&mut ends, &t1, started, &typed), ComposerHold::Staged);

        // The observable consequence: the next distinct turn can take the
        // pin. Before the fix this refused, permanently.
        assert!(
            turnkey::PaneEnds::pin(&mut ends, &pane(), agent, "codex", &t2),
            "a retired pin leaves the next turn free to take it"
        );

        // t1's end finally arrives. It belongs to a turn nobody waits on
        // and must not release t2.
        turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", t1.clone());
        assert_eq!(
            step(&mut ends, &t2, started, &clean),
            started,
            "another turn's end does not end this one"
        );

        // Only t2's own end does.
        turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", t2.clone());
        assert_eq!(step(&mut ends, &t2, started, &clean), ComposerHold::Clear);
        assert!(!turnkey::PaneEnds::holds(
            &ends,
            &pane(),
            agent,
            "codex",
            &t2
        ));
    }

    /// Binding a turn is what puts a pane on the exact lifecycle, and it
    /// is the delivery holding the barrier that may do it.
    ///
    /// Until a hold carries a key it runs on the screen, where an end
    /// delayed from the previous turn is indistinguishable from this
    /// one's and can release a payload nothing consumed. So the bind has
    /// to reach production, and it has to refuse everything that is not
    /// this delivery's own turn.
    ///
    /// Each refusal starts from an EMPTY end store on purpose. A pin
    /// already held refuses a second key on its own, which would let a
    /// missing owner or turn check pass this test for the wrong reason.
    #[test]
    fn only_the_delivery_holding_the_barrier_binds_its_turn() {
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
        .stamped(false, ComposerHold::Clear);
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let inner = inner_with(BTreeMap::new());
        let t1 = turnkey::TurnKey::for_test(&["s-1", "t-1"]);
        let t2 = turnkey::TurnKey::for_test(&["s-1", "t-2"]);

        let entry = |owner: Option<&str>, hold, turn: Option<&turnkey::TurnKey>| DetEntry {
            detection: clean.clone(),
            binding: None,
            manifest: Some("codex".into()),
            occupant: Some(71),
            agent: Some(agent),
            turn: turn.cloned(),
            in_mode: false,
            quota_screen_clear: false,
            hold,
            hold_owner: owner.map(str::to_string),
            composer: ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        };
        // Every case starts from a known pane state AND an empty end
        // store, so each refusal below is the guard under test.
        let start = |e: DetEntry| {
            inner
                .detections
                .lock()
                .expect("detections lock")
                .insert(pane(), e);
            *inner.turn_ends.lock().expect("turn ends lock") = turnkey::Ends::new();
        };
        let bound = || {
            let map = inner.detections.lock().expect("detections lock");
            let e = map.get(&pane()).expect("entry");
            (e.hold, e.turn.clone())
        };
        let free = |t: &turnkey::TurnKey| {
            // The pin is observable through its consequence: a turn that
            // is already pinned refuses a different key.
            let mut ends = inner.turn_ends.lock().expect("turn ends lock");
            turnkey::PaneEnds::pin(&mut ends, &pane(), agent, "codex", t)
        };

        // The delivery holding the barrier binds its own turn, and the
        // hold leaves the screen lifecycle for it.
        start(entry(Some("m-1#1"), ComposerHold::Staged, None));
        assert!(bind_turn(&inner, 0, "%1", "m-1#1", t1.clone(), 500).is_some());
        assert_eq!(
            bound(),
            (
                ComposerHold::TurnStarted { since_ms: 500 },
                Some(t1.clone())
            )
        );
        assert!(!free(&t2), "the key was not pinned against eviction");

        // Binding the same turn again is idempotent: an acknowledgement
        // can arrive more than once, and the first witnessed edge stands.
        assert!(bind_turn(&inner, 0, "%1", "m-1#1", t1.clone(), 900).is_some());
        assert_eq!(
            bound(),
            (
                ComposerHold::TurnStarted { since_ms: 500 },
                Some(t1.clone())
            )
        );

        // The end check belongs to the same turn-store transaction as the
        // pin. Reconciliation may consume the end immediately after this
        // call, but the dispatch receipt still retains what the transaction
        // observed.
        start(entry(Some("m-1#1"), ComposerHold::Staged, None));
        turnkey::PaneEnds::record(
            &mut inner.turn_ends.lock().expect("turn ends lock"),
            &pane(),
            agent,
            "codex",
            t1.clone(),
        );
        let ended =
            bind_turn(&inner, 0, "%1", "m-1#1", t1.clone(), 500).expect("matching turn binds");
        assert!(ended.end_already_present);
        assert!(turnkey::PaneEnds::take(
            &mut inner.turn_ends.lock().expect("turn ends lock"),
            &pane(),
            agent,
            "codex",
            &t1,
        ));
        assert!(
            ended.end_already_present,
            "the captured fact must survive consumption"
        );

        // A hold already waiting on one turn is not a second turn's to
        // take, even from the delivery that owns the barrier.
        start(entry(
            Some("m-1#1"),
            ComposerHold::TurnStarted { since_ms: 500 },
            Some(&t1),
        ));
        assert!(bind_turn(&inner, 0, "%1", "m-1#1", t2.clone(), 900).is_none());
        assert_eq!(
            bound(),
            (ComposerHold::TurnStarted { since_ms: 500 }, Some(t1))
        );

        // Another delivery's receipt cannot bind a turn to this barrier.
        // That is the late-acknowledgement shape: the first delivery
        // released, the next claimed the composer, and evidence for the
        // first arrives afterwards.
        start(entry(Some("m-2#1"), ComposerHold::Staged, None));
        assert!(bind_turn(&inner, 0, "%1", "m-1#1", t2.clone(), 900).is_none());
        assert_eq!(bound(), (ComposerHold::Staged, None));

        // An unowned barrier is nobody's to bind.
        start(entry(None, ComposerHold::Staged, None));
        assert!(bind_turn(&inner, 0, "%1", "m-2#1", t2.clone(), 900).is_none());
        assert_eq!(bound(), (ComposerHold::Staged, None));

        // And a pane whose binding cannot be named has nothing to key the
        // end store on.
        let mut unbound = entry(Some("m-2#1"), ComposerHold::Staged, None);
        unbound.agent = None;
        start(unbound);
        assert!(bind_turn(&inner, 0, "%1", "m-2#1", t2, 900).is_none());
        assert_eq!(bound(), (ComposerHold::Staged, None));
    }

    /// The composer barrier is not first-come-first-served.
    ///
    /// The bug this pins: the claim checked only whether SOMEBODY ELSE
    /// owned the barrier. A person typing after the last capture raises
    /// an unowned hold, and an unowned hold read as "free" let the next
    /// delivery take it and paste over their text. A fresh claim needs a
    /// composer this daemon believes is empty AND unclaimed; only the
    /// same owner may re-claim what it already holds.
    #[tokio::test]
    async fn a_fresh_claim_refuses_a_barrier_it_does_not_own() {
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
        .stamped(false, ComposerHold::Clear);
        let admitted = crate::identity::ProcId {
            pid: 4242,
            birth: 7,
        };
        let agent = Some(admitted);
        let entry = |hold, owner: Option<&str>| DetEntry {
            detection: clean.clone(),
            binding: None,
            manifest: Some("bash".into()),
            occupant: Some(4242),
            agent,
            turn: None,
            in_mode: false,
            quota_screen_clear: false,
            hold,
            hold_owner: owner.map(str::to_string),
            composer: ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        };

        let inner = inner_with(BTreeMap::new());
        let put = |e: DetEntry| {
            inner
                .detections
                .lock()
                .expect("detections lock")
                .insert(pane(), e);
        };
        let hold_now = || {
            let map = inner.detections.lock().expect("detections lock");
            let e = map.get(&pane()).expect("entry");
            (e.hold, e.hold_owner.clone())
        };

        // Clear and unowned: the only shape a fresh claim may take.
        put(entry(ComposerHold::Clear, None));
        assert!(claim_hold(&inner, 0, "%1", "m-1#1", agent, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Staged, Some("m-1#1".into())));

        // Same owner, already staged: idempotent.
        assert!(claim_hold(&inner, 0, "%1", "m-1#1", agent, Some("bash")));

        // A different delivery may not take a barrier that is held.
        assert!(!claim_hold(&inner, 0, "%1", "m-2#1", agent, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Staged, Some("m-1#1".into())));

        assert!(!release_unwritten_hold(
            &inner, 0, "%1", "m-2#1", admitted, "bash"
        ));
        assert!(!release_unwritten_hold(
            &inner, 0, "%1", "m-1#1", admitted, "codex"
        ));
        assert!(release_unwritten_hold(
            &inner, 0, "%1", "m-1#1", admitted, "bash"
        ));
        assert_eq!(hold_now(), (ComposerHold::Clear, None));
        assert!(!release_unwritten_hold(
            &inner, 0, "%1", "m-1#1", admitted, "bash"
        ));

        let attempt = "att-00000000-0000-4000-8000-000000000001";
        put(entry(ComposerHold::Staged, Some(attempt)));
        let process = cyclops_proto::ProcessInstanceId::new(admitted.pid, admitted.birth).unwrap();
        assert!(staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));
        assert!(!staged_action_ready(
            &inner,
            0,
            "%1",
            "att-00000000-0000-4000-8000-000000000002",
            process,
            "bash"
        ));
        let mut working = entry(ComposerHold::Staged, Some(attempt));
        working.detection.readings.push(SensorReading {
            sensor: Sensor::Hook,
            state: AgentState::Working,
            rule: "turn_start".into(),
            ts: 2,
        });
        put(working);
        assert!(!staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));

        let mut blocked = entry(ComposerHold::Staged, Some(attempt));
        blocked.detection.state = AgentState::BlockedPermission;
        put(blocked);
        assert!(!staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));

        let mut stale = entry(ComposerHold::Staged, Some(attempt));
        stale.detection.stale = true;
        put(stale);
        assert!(!staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));

        put(entry(ComposerHold::Staged, Some(attempt)));
        assert!(
            !resolve_staged_hold(
                &inner,
                0,
                "%1",
                "att-00000000-0000-4000-8000-000000000002",
                process,
                "bash"
            )
            .await
        );
        assert_eq!(hold_now(), (ComposerHold::Staged, Some(attempt.into())));
        assert!(resolve_staged_hold(&inner, 0, "%1", attempt, process, "bash").await);
        assert_eq!(hold_now(), (ComposerHold::Clear, None));

        // A recompute that started before settlement may still promote the
        // old hold. Resolution waits for that commit, then clears it last.
        let turn = turnkey::TurnKey::for_test(&["settled"]);
        let mut promoted = entry(ComposerHold::TurnStarted { since_ms: 9 }, Some(attempt));
        promoted.turn = Some(turn.clone());
        put(entry(ComposerHold::Staged, Some(attempt)));
        let recompute_gate = pane_recompute_gate(&inner, &pane());
        let recompute_guard = recompute_gate.lock().await;
        let mut resolution = Box::pin(resolve_staged_hold(
            &inner, 0, "%1", attempt, process, "bash",
        ));
        tokio::select! {
            biased;
            result = &mut resolution => panic!("resolution crossed an active recompute: {result}"),
            () = tokio::task::yield_now() => {}
        }
        put(promoted);
        assert!(turnkey::PaneEnds::pin(
            &mut inner.turn_ends.lock().expect("turn ends lock"),
            &pane(),
            admitted,
            "bash",
            &turn,
        ));
        drop(recompute_guard);
        assert!(resolution.await);
        assert_eq!(hold_now(), (ComposerHold::Clear, None));
        assert!(turnkey::PaneEnds::pin(
            &mut inner.turn_ends.lock().expect("turn ends lock"),
            &pane(),
            admitted,
            "bash",
            &turnkey::TurnKey::for_test(&["next"]),
        ));

        // The race this exists for: a person types between the proof and
        // the write, a recompute records the text, and nobody owns it.
        for hold in [
            ComposerHold::Staged,
            ComposerHold::TurnStarted { since_ms: 9 },
        ] {
            put(entry(hold, None));
            assert!(
                !claim_hold(&inner, 0, "%1", "m-3#1", agent, Some("bash")),
                "an unowned {hold:?} is somebody's text, not a free barrier"
            );
            assert_eq!(hold_now(), (hold, None), "a refused claim changes nothing");
        }

        // A proven binding is still required on top of all of that.
        put(entry(ComposerHold::Clear, None));
        assert!(!claim_hold(&inner, 0, "%1", "m-4#1", agent, Some("codex")));
        assert!(!claim_hold(&inner, 0, "%1", "m-4#1", None, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Clear, None));

        // An unauthenticated pane refuses even when the cache agrees that
        // nobody is home. A pinned manifest chooses rules without proving
        // a process, so two absent identities matching would put a
        // payload into a shell prompt.
        let mut unbound = entry(ComposerHold::Clear, None);
        unbound.agent = None;
        put(unbound);
        assert!(!claim_hold(&inner, 0, "%1", "m-5#1", None, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Clear, None));
    }

    /// A failed capture has to leave the same refusal everywhere.
    ///
    /// The bug this pins: the retained verdict was returned to the caller
    /// that asked for it and never written back, so `status` and every
    /// other cache reader kept the pre-failure record, which still said
    /// write_ready. Two consumers, two answers, from one observation
    /// failure.
    #[test]
    fn a_failed_capture_refuses_in_the_cache_too() {
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: Some(ComposerSemantic::Clean),
        }
        .stamped(false, ComposerHold::Clear);
        assert!(clean.write_ready, "fixture must start write-ready");

        let mut map = std::collections::HashMap::new();
        let since = std::time::Instant::now();
        map.insert(
            pane(),
            DetEntry {
                detection: clean,
                binding: None,
                manifest: Some("bash".into()),
                occupant: Some(4242),
                agent: None,
                turn: None,
                in_mode: false,
                quota_screen_clear: false,
                hold: ComposerHold::Clear,
                hold_owner: None,
                composer: ComposerProjection::default(),
                working_confirmed: false,
                since,
            },
        );

        let returned = retain_stale(&mut map, &pane(), false, Some(4242), Some("bash"))
            .expect("same occupant");
        let cached = &map[&pane()].detection;
        for (who, det) in [("returned", &returned), ("cached", cached)] {
            assert!(det.stale, "{who} verdict is not marked stale");
            assert!(!det.write_ready, "{who} verdict still authorizes a write");
            assert_eq!(
                det.write_block.as_deref(),
                Some("stale_screen_evidence"),
                "{who} verdict names the wrong reason"
            );
            assert_eq!(det.state, AgentState::Idle, "{who} state must not move");
        }
        assert_eq!(
            map[&pane()].since,
            since,
            "confidence changed, not the state"
        );
    }

    /// The pane id outlives the agent, so a retained verdict must not.
    ///
    /// Shape: agent A runs in a pane and is observed working. A exits back
    /// to the same shell, agent B starts at the same prompt, and B's first
    /// capture fails. Same pane id, same root pid, possibly the same
    /// manifest. Retaining on pane id alone hands B a turn A was having,
    /// and the stale flag does not fix that: it blocks the write, while
    /// the record still says the wrong agent is working.
    #[test]
    fn a_retained_verdict_never_describes_a_replacement_occupant() {
        let working = Detection {
            state: AgentState::Working,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Working,
                rule: "screen_busy".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "screen_busy".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
        .stamped(false, ComposerHold::Clear);
        let entry_a = || DetEntry {
            detection: working.clone(),
            binding: None,
            manifest: Some("agent-a".into()),
            occupant: Some(111),
            agent: None,
            in_mode: false,
            quota_screen_clear: false,
            hold: ComposerHold::Clear,
            hold_owner: None,
            turn: None,
            composer: ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        };

        // Each of these is a different occupant from A's: a new leader, a
        // different manifest, and an unprovable foreground.
        for (case, occupant, manifest) in [
            ("agent B took the prompt", Some(222), Some("agent-a")),
            ("the manifest changed", Some(111), Some("agent-b")),
            ("nobody could prove it", None, Some("agent-a")),
        ] {
            let mut map = std::collections::HashMap::new();
            map.insert(pane(), entry_a());
            assert!(
                retain_stale(&mut map, &pane(), false, occupant, manifest).is_none(),
                "{case}: A's verdict was handed to somebody else"
            );
            // Refusing to retain also has to leave A's record alone
            // rather than half-editing it: the caller's fall-through is
            // what replaces it, with readings taken for whoever is there.
            let cached = &map[&pane()];
            assert_eq!(cached.occupant, Some(111), "{case}");
            assert!(!cached.detection.stale, "{case}: A's record was edited");
        }
    }

    #[test]
    fn binding_is_by_process_name_in_id_order() {
        let mut map = BTreeMap::new();
        map.insert("bash".to_string(), manifest());
        assert_eq!(
            bind_manifest(&map, "bash").map(|m| m.agent.id.as_str()),
            Some("bash")
        );
        assert!(bind_manifest(&map, "vim").is_none());
    }

    #[test]
    fn tier_winners() {
        let m = manifest();
        assert_eq!(
            title_winner(&m, "IDLE ready").map(|r| r.id.as_str()),
            Some("title_idle")
        );
        assert!(title_winner(&m, "mac").is_none());
        assert_eq!(
            screen_winner(&m, "junk\nFIXPROMPT ").map(|r| r.id.as_str()),
            Some("screen_busy")
        );
        assert!(screen_winner(&m, "nothing here").is_none());
    }

    #[test]
    fn a_title_idle_never_outranks_a_non_idle_screen_rule() {
        let m = manifest();
        let t = title_winner(&m, "IDLE ready");
        let s = screen_winner(&m, "FIXPROMPT ");
        let d = fuse(&m, t, s, true, false, 1);
        assert_eq!(d.state, AgentState::Working);
        assert_eq!(d.decided_by, "screen_busy");
        assert!(d.disagreement);
        assert!(!d.write_ready);
        assert_eq!(d.readings.len(), 2);
        assert_eq!(d.readings[0].sensor, Sensor::Title);
        assert_eq!(d.readings[0].rule, "title_idle");
        assert_eq!(d.readings[1].sensor, Sensor::Screen);
        assert_eq!(d.readings[1].rule, "screen_busy");
    }

    #[test]
    fn current_screen_evidence_survives_repeated_title_observations() {
        let m = current_tiers_manifest();
        assert!(manifest_uses_screen_tier(&m));

        let idle_title = title_winner(&m, "IDLE ready");
        let working_screen = screen_winner(&m, "ACTIVE");
        for observed_at in [10, 11, 12] {
            let detection = fuse(&m, idle_title, working_screen, true, false, observed_at);
            assert_eq!(detection.state, AgentState::Working);
            assert_eq!(detection.decided_by, "screen_working");
            assert!(detection.disagreement);
            assert!(!detection.write_ready);
        }

        let working_title = title_winner(&m, "WORKING now");
        let modal_screen = screen_winner(&m, "PERMISSION required");
        let blocked = fuse(&m, working_title, modal_screen, true, false, 13);
        assert_eq!(blocked.state, AgentState::BlockedModal);
        assert_eq!(blocked.decided_by, "screen_modal");
        assert!(blocked.disagreement);
        assert!(!blocked.write_ready);
    }

    #[test]
    fn single_tier_is_no_disagreement() {
        let m = manifest();
        let s = screen_winner(&m, "FIXPROMPT ");
        let d = fuse(&m, None, s, true, false, 1);
        assert_eq!(d.state, AgentState::Working);
        assert_eq!(d.decided_by, "screen_busy");
        assert!(!d.disagreement);
        assert_eq!(d.readings.len(), 1);
    }

    #[test]
    fn no_rule_is_unknown() {
        let m = manifest();
        let d = fuse(&m, None, None, true, false, 1);
        assert_eq!(d.state, AgentState::Unknown);
        assert_eq!(d.decided_by, "no_rule");
        assert!(d.readings.is_empty());
    }
    /// MEASURED 2026-08-26 on a live Claude Code pane: the idle sparkle title
    /// stays in place for the whole turn, so a capture that lacks a matching
    /// spinner row used to publish `idle` from the title alone; 1203 such
    /// flaps in six hours, and 25 doorbell writes admitted into a working
    /// pane behind them. An observed screen with nothing idle-shaped is not
    /// idle evidence.
    #[test]
    fn a_title_idle_with_no_screen_rule_is_unknown_not_idle() {
        let m = manifest();
        let t = title_winner(&m, "IDLE ready");
        assert!(t.is_some());
        let d = fuse(&m, t, None, true, false, 1);
        assert_eq!(d.state, AgentState::Unknown);
        assert_eq!(d.decided_by, "idle_unconfirmed");
        assert!(!d.write_ready);
        assert!(!d.disagreement);
        assert_eq!(d.readings.len(), 1);
        assert_eq!(d.readings[0].sensor, Sensor::Title);
    }
    /// A screen tier that only knows working or blocked states cannot
    /// confirm idle, so a screen-tier manifest never publishes idle from
    /// its title alone: the pane is unknown until a lifecycle-evidence idle
    /// screen rule matches, which is the fail-closed direction.
    #[test]
    fn a_screen_tier_manifest_without_a_confirmed_idle_never_reads_idle() {
        let m = manifest();
        assert!(manifest_uses_screen_tier(&m));
        assert!(!winner_confirms_idle(screen_winner(&m, "nothing here")));
        let t = title_winner(&m, "IDLE ready");
        assert!(t.is_some());
        let d = fuse(&m, t, None, manifest_uses_screen_tier(&m), false, 1);
        assert_eq!(d.state, AgentState::Unknown);
        assert_eq!(d.decided_by, "idle_unconfirmed");
        assert!(!d.write_ready);
    }
    /// An idle title agreeing with a lifecycle-evidence idle screen rule
    /// keeps deciding, with the screen rule's composer semantic riding
    /// along; the same manifest with nothing idle-shaped on screen no
    /// longer lets the title decide.
    #[test]
    fn a_title_idle_confirmed_by_an_idle_screen_rule_still_decides() {
        let m = Manifest::parse(
            TITLE_AND_COMPOSER_FIXTURE,
            Path::new("title-and-composer.toml"),
        )
        .unwrap();
        assert!(winner_confirms_idle(screen_winner(&m, "$ ")));
        assert!(!winner_confirms_idle(screen_winner(&m, "FIXPROMPT ")));
        let t = title_winner(&m, "IDLE ready");
        let s = screen_winner(&m, "$ ");
        assert_eq!(s.map(|r| r.id.as_str()), Some("composer_empty"));
        let d = fuse(&m, t, s, true, true, 1);
        assert_eq!(d.state, AgentState::Idle);
        assert_eq!(d.decided_by, "title_idle");
        assert!(!d.disagreement);
        assert_eq!(d.composer_semantic, Some(ComposerSemantic::Clean));
        let unconfirmed = fuse(&m, t, None, true, false, 1);
        assert_eq!(unconfirmed.state, AgentState::Unknown);
        assert_eq!(unconfirmed.decided_by, "idle_unconfirmed");
    }
    /// A composer row measured mid-turn keeps its semantic but cannot
    /// confirm idle once the manifest marks it `lifecycle_evidence = false`.
    #[test]
    fn a_mid_turn_composer_row_carries_its_semantic_but_never_confirms_idle() {
        let m = Manifest::parse(
            &TITLE_AND_COMPOSER_FIXTURE.replace(
                "composer_semantic = \"clean\"\npriority = 900",
                "composer_semantic = \"clean\"\npriority = 900\nlifecycle_evidence = false",
            ),
            Path::new("mid-turn-composer.toml"),
        )
        .unwrap();
        let confirmed = winner_confirms_idle(screen_winner(&m, "$ "));
        assert!(!confirmed);
        let t = title_winner(&m, "IDLE ready");
        let s = screen_winner(&m, "$ ");
        assert_eq!(s.map(|r| r.id.as_str()), Some("composer_empty"));
        let d = fuse(&m, t, s, true, confirmed, 1);
        assert_eq!(d.state, AgentState::Unknown);
        assert_eq!(d.decided_by, "idle_unconfirmed");
        assert_eq!(d.composer_semantic, Some(ComposerSemantic::Clean));
        assert!(!d.write_ready);
    }
    trait WithState {
        fn with_state(self, state: AgentState) -> Self;
    }
    impl WithState for SensorReading {
        fn with_state(mut self, state: AgentState) -> Self {
            self.state = state;
            self
        }
    }
    fn working_reading(ts: u64) -> SensorReading {
        SensorReading {
            sensor: Sensor::Hook,
            state: AgentState::Working,
            rule: "UserPromptSubmit".into(),
            ts,
        }
    }
    fn visual(state: AgentState, stale: bool) -> Detection {
        Detection {
            state,
            readings: Vec::new(),
            disagreement: false,
            decided_by: "screen".into(),
            stale,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
        }
    }
    /// A visually accepted provisional dispatch start keeps its original edge
    /// and binding when it becomes persistent, and stops being provisional.
    #[test]
    fn a_promoted_provisional_start_keeps_its_edge_and_binding() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let provisional =
            HookEntry::provisional_start(agent, Some("fix".into()), working_reading(1000));
        assert!(provisional.provisional_start_for(agent, Some("fix")));
        let promoted = provisional.promote();
        assert!(promoted.confirmed_unkeyed_start_for(agent, Some("fix")));
        assert!(!promoted.provisional_start_for(agent, Some("fix")));
        assert_eq!(promoted.reading.ts, 1000);
        assert_eq!(promoted.reading.state, AgentState::Working);
        assert!(promoted.provisional_ready_at_ms.is_none());
        assert!(promoted.describes(Some(agent), Some("fix")));
        let other = crate::identity::ProcId { pid: 8, birth: 80 };
        assert!(!promoted.describes(Some(other), Some("fix")));
    }
    /// MEASURED 2026-08-26: the provisional start was removed on the first
    /// Working frame and the next idle-shaped frame won. A persistent start
    /// holds over every unconfirmed idle frame, over stale, in-mode, and
    /// binding-changed captures, and over working frames; one conclusive
    /// lifecycle-evidence idle winner on an idle-class fused frame with
    /// stable bookends ends it.
    #[test]
    fn a_persistent_unkeyed_start_ends_on_one_conclusive_terminal_frame() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let mut entry =
            HookEntry::provisional_start(agent, Some("fix".into()), working_reading(1000))
                .promote();
        let idle = visual(AgentState::Idle, false);
        for ts in 0..20 {
            assert_eq!(
                hook_action_observed(&mut entry, &idle, false, false, true, 2000 + ts),
                HookAction::Use,
                "unconfirmed idle frame {ts} must never end the latch"
            );
        }
        let working = visual(AgentState::Working, false);
        assert_eq!(
            hook_action_observed(&mut entry, &working, false, false, true, 2100),
            HookAction::Use
        );
        let stale_idle = visual(AgentState::Idle, true);
        assert_eq!(
            hook_action_observed(&mut entry, &stale_idle, true, false, true, 2101),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(&mut entry, &idle, true, true, true, 2102),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(&mut entry, &idle, true, false, false, 2103),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(&mut entry, &idle, true, false, true, 2104),
            HookAction::Drop
        );
    }
    /// Repeated generic `composer_empty` (lifecycle_evidence = false) or title
    /// idle frames fuse to unknown and never end the latch, however many.
    #[test]
    fn repeated_generic_composer_or_title_idle_never_ends_the_latch() {
        let m = Manifest::parse(
            &TITLE_AND_COMPOSER_FIXTURE.replace(
                "composer_semantic = \"clean\"\npriority = 900",
                "composer_semantic = \"clean\"\npriority = 900\nlifecycle_evidence = false",
            ),
            Path::new("generic-idle.toml"),
        )
        .unwrap();
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let mut entry = HookEntry::provisional_start(
            agent,
            Some("bash-composer".into()),
            working_reading(1000),
        )
        .promote();
        let t = title_winner(&m, "IDLE ready");
        let s = screen_winner(&m, "$ ");
        assert_eq!(s.map(|r| r.id.as_str()), Some("composer_empty"));
        let confirmed = winner_confirms_idle(s);
        assert!(!confirmed);
        for ts in 0..20 {
            let fused = fuse(&m, t, s, true, confirmed, 1);
            assert_eq!(fused.state, AgentState::Unknown);
            assert_eq!(
                hook_action_observed(&mut entry, &fused, confirmed, false, true, 2000 + ts),
                HookAction::Use
            );
        }
        let title_only = fuse(&m, t, None, true, false, 1);
        assert_eq!(title_only.state, AgentState::Unknown);
        assert_eq!(
            hook_action_observed(&mut entry, &title_only, false, false, true, 3000),
            HookAction::Use
        );
    }
    const CATCH_ALL_LIFECYCLE_FIXTURE: &str = r#"
[agent]
id = "catch-all"
display_name = "Catch-all lifecycle fixture"
process_names = ["fixture"]

[[rule]]
id = "screen_modal"
state = "blocked_modal"
priority = 1200
region = "bottom_non_empty_lines(3)"
line_regex = ['^PERMISSION']

[[rule]]
id = "screen_working"
state = "working"
priority = 1100
region = "bottom_non_empty_lines(3)"
line_regex = ['^ACTIVE']

[[rule]]
id = "composer_typed"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 1000
lifecycle_evidence = false
region = "bottom_non_empty_lines(3)"
line_regex = ['^> \S']

[[rule]]
id = "always_idle"
state = "idle"
priority = 70
lifecycle_evidence = true
region = "bottom_non_empty_lines(3)"
regex = ['^']
"#;
    /// MUTATION: a low-priority catch-all lifecycle idle rule (`^`) matches
    /// underneath every higher-priority winner. Only the selected winner may
    /// certify idle, and the latch also needs the fused state to be
    /// idle-class, so repeated working, typed-input, and blocked frames keep
    /// the persistent start; only the catch-all winning twice ends it.
    #[test]
    fn a_catch_all_lifecycle_idle_rule_never_ends_a_latch_underneath_a_winner() {
        let m = Manifest::parse(CATCH_ALL_LIFECYCLE_FIXTURE, Path::new("catch-all.toml")).unwrap();
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let mut entry =
            HookEntry::provisional_start(agent, Some("catch-all".into()), working_reading(1000))
                .promote();
        let frames = [
            ("ACTIVE", "screen_working", AgentState::Working),
            ("> draft", "composer_typed", AgentState::Unknown),
            (
                "PERMISSION required",
                "screen_modal",
                AgentState::BlockedModal,
            ),
        ];
        for (capture, rule, state) in frames {
            let winner = screen_winner(&m, capture);
            assert_eq!(winner.map(|r| r.id.as_str()), Some(rule));
            assert!(
                !winner_confirms_idle(winner),
                "{rule} must not certify idle"
            );
            let fused = fuse(&m, None, winner, true, winner_confirms_idle(winner), 1);
            assert_eq!(fused.state, state);
            for ts in 0..4 {
                assert_eq!(
                    hook_action_observed(
                        &mut entry,
                        &fused,
                        winner_confirms_idle(winner),
                        false,
                        true,
                        2000 + ts
                    ),
                    HookAction::Use,
                    "{rule} frame {ts} must retain the latch"
                );
            }
        }
        let winner = screen_winner(&m, "plain shell output");
        assert_eq!(winner.map(|r| r.id.as_str()), Some("always_idle"));
        assert!(winner_confirms_idle(winner));
        let fused = fuse(&m, None, winner, true, true, 1);
        assert_eq!(fused.state, AgentState::Idle);
        assert_eq!(
            hook_action_observed(&mut entry, &fused, true, false, true, 3000),
            HookAction::Drop
        );
    }
    /// The latch also refuses a certified-idle flag whose fused state is not
    /// idle-class, which is the shape a stale or hook-overridden frame takes.
    #[test]
    fn a_latch_needs_the_fused_state_idle_as_well_as_the_winning_evidence() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let mut entry =
            HookEntry::provisional_start(agent, Some("fix".into()), working_reading(1000))
                .promote();
        let working = visual(AgentState::Working, false);
        for ts in 0..4 {
            assert_eq!(
                hook_action_observed(&mut entry, &working, true, false, true, 2000 + ts),
                HookAction::Use
            );
        }
    }
    /// A confirmed exact end ends a persistent unkeyed start only on the same
    /// binding and only when it comes strictly after the stored start edge.
    #[test]
    fn an_exact_end_ends_an_unkeyed_latch_only_when_later_on_the_same_binding() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let entry = HookEntry::provisional_start(agent, Some("fix".into()), working_reading(1000))
            .promote();
        assert!(
            !entry.unkeyed_latch_ended_by(agent, Some("fix"), 999),
            "stale end"
        );
        assert!(
            !entry.unkeyed_latch_ended_by(agent, Some("fix"), 1000),
            "same instant"
        );
        assert!(
            entry.unkeyed_latch_ended_by(agent, Some("fix"), 1001),
            "later end"
        );
        let other = crate::identity::ProcId { pid: 8, birth: 80 };
        assert!(
            !entry.unkeyed_latch_ended_by(other, Some("fix"), 1001),
            "other generation"
        );
        assert!(
            !entry.unkeyed_latch_ended_by(agent, Some("other"), 1001),
            "other manifest"
        );
        let provisional =
            HookEntry::provisional_start(agent, Some("fix".into()), working_reading(1000));
        assert!(
            !provisional.unkeyed_latch_ended_by(agent, Some("fix"), 1001),
            "not yet promoted"
        );
    }
    fn staged_entry(
        state: AgentState,
        semantic: Option<ComposerSemantic>,
        readings: Vec<SensorReading>,
        stale: bool,
        in_mode: bool,
        owner: &str,
    ) -> DetEntry {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        DetEntry {
            detection: Detection {
                state,
                readings,
                disagreement: false,
                decided_by: "test".into(),
                stale,
                write_ready: false,
                write_block: None,
                composer_semantic: semantic,
            },
            binding: None,
            manifest: Some("fix".into()),
            occupant: Some(4242),
            agent: Some(agent),
            turn: None,
            in_mode,
            quota_screen_clear: false,
            hold: ComposerHold::Staged,
            hold_owner: Some(owner.to_string()),
            composer: ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        }
    }
    fn screen_reading(state: AgentState) -> SensorReading {
        SensorReading {
            sensor: Sensor::Screen,
            state,
            rule: "screen".into(),
            ts: 1,
        }
    }
    /// A staged row is not lifecycle evidence, so a pane holding our exact
    /// doorbell fuses to `unknown`; the owner's own action on it is admitted
    /// only when that unknown is the honest reading of a staged human-input
    /// row on a quiet, fresh, out-of-mode frame with the exact owner.
    #[test]
    fn an_unknown_staged_frame_admits_only_the_exact_owner_on_a_quiet_frame() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let quiet = || vec![screen_reading(AgentState::IdleWithInput)];
        let ok = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::HumanInput),
            quiet(),
            false,
            false,
            "att-1",
        );
        assert!(
            staged_entry_ready(&ok, "att-1", agent, "fix"),
            "exact end then unknown plus exact proof clears once"
        );
        assert!(staged_hold_ready(&ok));
        // exact end then unknown: a retained idle hook reading is still quiet
        let ended = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::HumanInput),
            vec![
                screen_reading(AgentState::IdleWithInput),
                working_reading(1).with_state(AgentState::Idle),
            ],
            false,
            false,
            "att-1",
        );
        assert!(staged_entry_ready(&ended, "att-1", agent, "fix"));
        // active start plus exact doorbell refuses
        let active = staged_entry(
            AgentState::Working,
            Some(ComposerSemantic::HumanInput),
            vec![
                screen_reading(AgentState::IdleWithInput),
                working_reading(1),
            ],
            false,
            false,
            "att-1",
        );
        assert!(
            !staged_entry_ready(&active, "att-1", agent, "fix"),
            "active start refuses"
        );
        // unknown with no readings refuses
        let empty = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::HumanInput),
            vec![],
            false,
            false,
            "att-1",
        );
        assert!(
            !staged_entry_ready(&empty, "att-1", agent, "fix"),
            "no readings refuses"
        );
        // ghost refuses
        let ghost = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::GhostSuggestion),
            quiet(),
            false,
            false,
            "att-1",
        );
        assert!(
            !staged_entry_ready(&ghost, "att-1", agent, "fix"),
            "ghost refuses"
        );
        // bare prompt (clean) is not a staged row either
        let bare = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::Clean),
            quiet(),
            false,
            false,
            "att-1",
        );
        assert!(
            !staged_entry_ready(&bare, "att-1", agent, "fix"),
            "bare prompt refuses"
        );
        // blocked refuses
        let blocked = staged_entry(
            AgentState::BlockedModal,
            Some(ComposerSemantic::HumanInput),
            vec![screen_reading(AgentState::BlockedModal)],
            false,
            false,
            "att-1",
        );
        assert!(
            !staged_entry_ready(&blocked, "att-1", agent, "fix"),
            "blocked refuses"
        );
        // stale refuses
        let stale = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::HumanInput),
            quiet(),
            true,
            false,
            "att-1",
        );
        assert!(
            !staged_entry_ready(&stale, "att-1", agent, "fix"),
            "stale refuses"
        );
        // mode refuses
        let in_mode = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::HumanInput),
            quiet(),
            false,
            true,
            "att-1",
        );
        assert!(
            !staged_entry_ready(&in_mode, "att-1", agent, "fix"),
            "mode refuses"
        );
        // wrong owner, generation, or manifest refuses
        assert!(
            !staged_entry_ready(&ok, "att-2", agent, "fix"),
            "wrong owner refuses"
        );
        let other = crate::identity::ProcId { pid: 8, birth: 80 };
        assert!(
            !staged_entry_ready(&ok, "att-1", other, "fix"),
            "wrong generation refuses"
        );
        assert!(
            !staged_entry_ready(&ok, "att-1", agent, "other"),
            "wrong manifest refuses"
        );
        // extra text is refused by the callers' exact byte proof; this seam
        // never sees bytes, so an idle-class frame stays admitted here.
        let idle_with_input = staged_entry(
            AgentState::IdleWithInput,
            Some(ComposerSemantic::HumanInput),
            quiet(),
            false,
            false,
            "att-1",
        );
        assert!(staged_entry_ready(&idle_with_input, "att-1", agent, "fix"));
    }
    /// An authenticated confirmed start (not a promoted candidate) is never
    /// ended by the screen: repeated idle composer frames before the first
    /// output keep it Working, as the lifecycle contract requires.
    #[test]
    fn a_confirmed_start_is_never_ended_by_a_screen_terminal() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let mut entry =
            HookEntry::unkeyed_turn_started(agent, Some("fix".into()), working_reading(1000));
        assert!(entry.confirmed_unkeyed_start_for(agent, Some("fix")));
        let idle = visual(AgentState::Idle, false);
        for ts in 0..6 {
            assert_eq!(
                hook_action_observed(&mut entry, &idle, true, false, true, 2000 + ts),
                HookAction::Use,
                "confirmed start frame {ts}"
            );
        }
        assert!(
            entry.unkeyed_latch_ended_by(agent, Some("fix"), 3000),
            "the hook tier still ends it"
        );
    }
    const SHIPPED_CLAUDE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../resources/manifests/claude.toml"
    ));
    /// The named sequence for a fresh Claude pane: unknown before its first
    /// authenticated edge (SessionStart), idle once liveness is verified and
    /// no start is active, Working immediately after UserPromptSubmit, and
    /// never admitted while any start is active.
    #[test]
    fn a_fresh_claude_pane_is_unknown_until_session_start_then_idle_and_never_while_a_start_is_active(
    ) {
        let m = Manifest::parse(SHIPPED_CLAUDE, Path::new("claude.toml")).unwrap();
        let rule_120 = "─".repeat(120);
        let plain = [
            rule_120.as_str(),
            "❯",
            rule_120.as_str(),
            "  ⏵⏵ don't ask on (shift+tab to cycle)",
        ]
        .join("\n");
        let styled_rule = format!("\u{1b}[38;5;244m{rule_120}");
        let esc = [
            styled_rule.as_str(),
            "\u{1b}[39m❯",
            styled_rule.as_str(),
            "\u{1b}[39m  \u{1b}[38;5;210m⏵⏵ don't ask on\u{1b}[38;5;246m (shift+tab to cycle)\u{1b}[39m",
        ]
        .join("\n");
        let s = screen_winner_esc(&m, &plain, Some(&esc));
        assert_eq!(s.map(|r| r.id.as_str()), Some("composer_empty"));
        assert!(
            !winner_confirms_idle(s),
            "composer_empty is measured mid-turn"
        );
        let t = title_winner(&m, "✳ Fresh pane");
        let fused = fuse(&m, t, s, true, false, 1);
        assert_eq!(fused.state, AgentState::Unknown);
        assert_eq!(fused.decided_by, "idle_unconfirmed");
        assert_eq!(fused.composer_semantic, Some(ComposerSemantic::Clean));
        let has_screen = fused.readings.iter().any(|r| r.sensor == Sensor::Screen);
        assert!(has_screen);
        let admits = |verified: bool, active: bool, stale: bool, in_mode: bool, stable: bool| {
            liveness_admits_idle(
                fused.state,
                &fused.decided_by,
                s,
                has_screen,
                verified,
                active,
                stale,
                in_mode,
                stable,
            )
        };
        // Liveness is exact-binding and event-specific: telemetry and attention
        // edges leave the pane unknown, SessionStart admits it, and a later
        // generation on the same pane starts unknown again.
        let liveness = crate::selftest::HookLiveness::new();
        let pane = PaneKey::new(0, "%9");
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        liveness.open(&pane);
        assert!(
            !liveness.seen_admitting_edge(&pane, agent, "claude"),
            "no edge yet"
        );
        for (ts, event) in [
            (10, "Notification"),
            (11, "Stop"),
            (12, "StopFailure"),
            (13, "PermissionRequest"),
        ] {
            let _ = liveness.bind_diagnostic(&pane, event, ts, agent, "claude");
            let bound = liveness
                .bind_diagnostic(&pane, event, ts, agent, "claude")
                .expect("route open");
            liveness
                .publish_admission(&bound, event)
                .expect("lifetime live");
            assert!(
                liveness.seen_any(&pane, agent, "claude"),
                "{event} proves wiring"
            );
            assert!(
                !liveness.seen_admitting_edge(&pane, agent, "claude"),
                "{event} must leave the pane unknown"
            );
            assert!(!admits(
                liveness.seen_admitting_edge(&pane, agent, "claude"),
                false,
                false,
                false,
                true
            ));
        }
        // The diagnostic record alone never admits, even for SessionStart:
        // only the separately published admitting edge does.
        let _ = liveness.bind_diagnostic(&pane, "SessionStart", 14, agent, "claude");
        assert!(
            !liveness.seen_admitting_edge(&pane, agent, "claude"),
            "diagnostic record is not admission"
        );
        let bound = liveness
            .bind_diagnostic(&pane, "SessionStart", 15, agent, "claude")
            .expect("route open");
        liveness
            .publish_admission(&bound, "SessionStart")
            .expect("lifetime live");
        assert!(
            liveness.seen_admitting_edge(&pane, agent, "claude"),
            "SessionStart admits"
        );
        assert!(admits(
            liveness.seen_admitting_edge(&pane, agent, "claude"),
            false,
            false,
            false,
            true
        ));
        let other = crate::identity::ProcId { pid: 8, birth: 80 };
        assert!(
            !liveness.seen_admitting_edge(&pane, other, "claude"),
            "another generation begins unknown"
        );
        let prompt_first = PaneKey::new(0, "%10");
        liveness.open(&prompt_first);
        let bound = liveness
            .bind_diagnostic(&prompt_first, "UserPromptSubmit", 20, agent, "claude")
            .expect("route open");
        liveness
            .publish_admission(&bound, "UserPromptSubmit")
            .expect("lifetime live");
        assert!(
            liveness.seen_admitting_edge(&prompt_first, agent, "claude"),
            "UserPromptSubmit qualifies"
        );
        // A closed pane forgets its admitting edges with its lifetime.
        liveness.close(&prompt_first);
        liveness.open(&prompt_first);
        assert!(
            !liveness.seen_admitting_edge(&prompt_first, agent, "claude"),
            "a new lifetime begins unknown"
        );
        assert!(
            !admits(false, false, false, false, true),
            "before any admitting edge"
        );
        assert!(
            admits(true, false, false, false, true),
            "after SessionStart"
        );
        let mut admitted = fused.clone();
        admitted.state = AgentState::Idle;
        admitted.decided_by = "liveness:composer_empty".into();
        let stamped = admitted.clone().stamped(false, ComposerHold::Clear);
        assert!(stamped.write_ready, "{stamped:?}");
        let mut working = admitted.clone();
        apply_hook_reading(&mut working, working_reading(2), true, false);
        assert_eq!(
            working.state,
            AgentState::Working,
            "immediately after UserPromptSubmit"
        );
        assert!(
            !admits(true, true, false, false, true),
            "never while a start is active"
        );
        assert!(!admits(true, false, true, false, true), "stale");
        assert!(!admits(true, false, false, true, true), "in mode");
        assert!(!admits(true, false, false, false, false), "binding changed");
        assert!(
            !liveness_admits_idle(
                fused.state,
                &fused.decided_by,
                s,
                false,
                true,
                false,
                false,
                false,
                true
            ),
            "no screen reading"
        );
        for id in ["composer_ghost_suggestion", "composer_has_staged_input"] {
            let rule = m.rules.iter().find(|r| r.id == id);
            assert!(rule.is_some(), "{id}");
            assert!(
                !liveness_admits_idle(
                    fused.state,
                    &fused.decided_by,
                    rule,
                    has_screen,
                    true,
                    false,
                    false,
                    false,
                    true
                ),
                "{id} never admits"
            );
        }
        assert!(
            !liveness_admits_idle(
                AgentState::Idle,
                "composer_completed_terminal_suffix_2_1_246",
                s,
                has_screen,
                true,
                false,
                false,
                false,
                true
            ),
            "a lifecycle terminal frame needs no admission"
        );
    }
    /// Contract regressions for the bound hook handshake: a report before the
    /// route is open records nothing and is retryable; the retry after open
    /// publishes one diagnostic edge and one admission edge; a close and
    /// reopen between the diagnostic binding and the publication refuses the
    /// old lifetime, and the replacement inherits nothing.
    #[test]
    fn a_bound_handshake_records_nothing_before_open_and_refuses_an_expired_lifetime() {
        use crate::selftest::{HookLiveness, LifetimeExpired, RouteNotOpen};
        let liveness = HookLiveness::new();
        let pane = PaneKey::new(0, "%11");
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        // before route open: retryable, nothing recorded
        assert_eq!(
            liveness.bind_diagnostic(&pane, "SessionStart", 1, agent, "claude"),
            Err(RouteNotOpen)
        );
        assert!(!liveness.seen_any(&pane, agent, "claude"));
        assert!(!liveness.seen_admitting_edge(&pane, agent, "claude"));
        // retry after open with the same payload: one diagnostic, one admission
        liveness.open(&pane);
        let bound = liveness
            .bind_diagnostic(&pane, "SessionStart", 1, agent, "claude")
            .expect("route open");
        assert!(liveness.seen_any(&pane, agent, "claude"));
        assert!(
            !liveness.seen_admitting_edge(&pane, agent, "claude"),
            "not yet published"
        );
        assert_eq!(liveness.publish_admission(&bound, "SessionStart"), Ok(()));
        assert!(liveness.seen_admitting_edge(&pane, agent, "claude"));
        assert_eq!(liveness.edge_counts(&bound), (1, 1));
        // a duplicate publication of the same edge is idempotent, and a lost
        // response followed by the same sequence rebinds the same binding
        // and republishes without a second edge of either kind
        assert_eq!(liveness.publish_admission(&bound, "SessionStart"), Ok(()));
        assert!(liveness.seen_admitting_edge(&pane, agent, "claude"));
        let rebound = liveness
            .bind_diagnostic(&pane, "SessionStart", 1, agent, "claude")
            .expect("route still open");
        assert_eq!(rebound, bound);
        assert_eq!(liveness.publish_admission(&rebound, "SessionStart"), Ok(()));
        assert_eq!(liveness.edge_counts(&bound), (1, 1));
        // close and reopen between binding and publication: refused, and the
        // replacement lifetime inherits nothing
        let pane2 = PaneKey::new(0, "%12");
        liveness.open(&pane2);
        let stale = liveness
            .bind_diagnostic(&pane2, "SessionStart", 2, agent, "claude")
            .expect("route open");
        liveness.close(&pane2);
        liveness.open(&pane2);
        assert_eq!(
            liveness.publish_admission(&stale, "SessionStart"),
            Err(LifetimeExpired)
        );
        assert!(!liveness.seen_admitting_edge(&pane2, agent, "claude"));
        // non-admitting events are a no-op even with a live binding
        let bound = liveness
            .bind_diagnostic(&pane2, "Stop", 3, agent, "claude")
            .expect("route open");
        assert_eq!(liveness.publish_admission(&bound, "Stop"), Ok(()));
        assert!(!liveness.seen_admitting_edge(&pane2, agent, "claude"));
    }
    /// Hook-only readings never make a staged frame quiet, whatever they say.
    #[test]
    fn a_staged_frame_needs_a_current_screen_reading() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let hook_only = staged_entry(
            AgentState::Unknown,
            Some(ComposerSemantic::HumanInput),
            vec![working_reading(1).with_state(AgentState::Idle)],
            false,
            false,
            "att-1",
        );
        assert!(!staged_entry_ready(&hook_only, "att-1", agent, "fix"));
        assert!(!staged_hold_ready(&hook_only));
    }
    /// A keyed start is retired by its key, never by a screen terminal.
    #[test]
    fn a_keyed_start_is_not_ended_by_screen_bookends() {
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        let turn = turnkey::TurnKey::for_test(&["s", "t"]);
        let mut entry =
            HookEntry::turn_started(agent, Some("fix".into()), working_reading(1000), turn);
        let idle = visual(AgentState::Idle, false);
        for _ in 0..3 {
            assert_eq!(
                hook_action_observed(&mut entry, &idle, true, false, true, 2000),
                HookAction::Use
            );
        }
    }
    /// A manifest that never captures the screen still decides by title.
    #[test]
    fn a_title_idle_alone_still_decides_when_no_screen_was_observed() {
        let m = manifest();
        let t = title_winner(&m, "IDLE ready");
        let d = fuse(&m, t, None, false, false, 1);
        assert_eq!(d.state, AgentState::Idle);
        assert_eq!(d.decided_by, "title_idle");
        assert!(!d.disagreement);
    }

    fn composer_candidate(
        state: NotificationState,
    ) -> (
        crate::mailbox::ActiveComposerNotification,
        RecipientKey,
        BindingObservation,
    ) {
        let workspace = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        let session = "00000000-0000-4000-8000-000000000002".parse().unwrap();
        let recipient = RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let pane_root = crate::identity::ProcId { pid: 69, birth: 1 };
        let leader = crate::identity::ProcId { pid: 70, birth: 2 };
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let message_id = cyclops_proto::MessageId::new("m-composer").unwrap();
        let attempt_id =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000003").unwrap();
        let record = cyclops_proto::NotificationRecord {
            attempt_id,
            message_id: message_id.clone(),
            recipient,
            state,
            binding: Some(cyclops_proto::NotificationBinding {
                recipient,
                pane_root: Some(ProcessInstanceId::new(pane_root.pid, pane_root.birth).unwrap()),
                leader: Some(ProcessInstanceId::new(leader.pid, leader.birth).unwrap()),
                agent: ProcessInstanceId::new(agent.pid, agent.birth).unwrap(),
                manifest: cyclops_proto::NotificationManifestId::new("claude").unwrap(),
            }),
            transport: NotificationTransport::Doorbell,
            doorbell_format: Some(DOORBELL_FORMAT_COMPACT_CLAIM),
            cause: None,
            verify_outcome: None,
            pre_write_cause: None,
            wake_block: None,
            pre_write_observation: None,
            pre_write_reopen_count: 0,
            started_seq: 2,
            updated_seq: 3,
            updated_at: 4,
        };
        let message = cyclops_proto::LedgerLine {
            seq: 1,
            boot_id: "boot".into(),
            id: message_id.to_string(),
            ts: 1,
            kind: cyclops_proto::Kind::Msg,
            from: "admin".into(),
            to: vec!["claude".into()],
            subject: Some("subject".into()),
            body: Some("body".into()),
            reply_to: None,
            deliveries: Vec::new(),
            data: None,
        };
        (
            crate::mailbox::ActiveComposerNotification {
                record,
                message: Some(message),
                entry_state: Some(cyclops_proto::MailboxEntryState::Pending),
                recovery_action: crate::mailbox::ExactOwnedRecoveryAction::Ineligible,
            },
            recipient,
            BindingObservation::Bound(Binding {
                pane_root,
                leader,
                agent,
                manifest: "claude".into(),
            }),
        )
    }

    fn composer_detection(semantic: Option<ComposerSemantic>) -> Detection {
        let state = match semantic {
            Some(ComposerSemantic::Clean | ComposerSemantic::GhostSuggestion) => AgentState::Idle,
            _ => AgentState::IdleWithInput,
        };
        Detection {
            state,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: "composer".into(),
                ts: 5,
            }],
            disagreement: false,
            decided_by: "composer".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: semantic,
        }
    }

    #[test]
    fn composer_projection_exposes_the_six_closed_states() {
        assert_eq!(
            semantic_composer_projection(Some(ComposerSemantic::Clean)).state,
            ComposerState::ComposerClean
        );
        assert_eq!(
            semantic_composer_projection(Some(ComposerSemantic::HumanInput)).state,
            ComposerState::HumanDraft
        );
        assert_eq!(
            semantic_composer_projection(Some(ComposerSemantic::GhostSuggestion)).state,
            ComposerState::VendorGhostSuggestion
        );
        assert_eq!(
            semantic_composer_projection(Some(ComposerSemantic::Ambiguous)).state,
            ComposerState::ComposerAmbiguous
        );

        let (candidate, recipient, binding) = composer_candidate(NotificationState::Submitted);
        let expected = cyclops_proto::render_doorbell_v1(&candidate.record.message_id);
        let staged = project_composer(
            Some(ComposerSemantic::HumanInput),
            Some(&candidate.record.attempt_id.to_string()),
            &composer_detection(Some(ComposerSemantic::HumanInput)),
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Visible(expected),
            std::slice::from_ref(&candidate),
            true,
        );
        assert_eq!(staged.state, ComposerState::CyclopsNotificationStaged);
        assert_eq!(staged.proof, ComposerProof::ExactNotification);

        let submitted = project_composer(
            Some(ComposerSemantic::Clean),
            Some(&candidate.record.attempt_id.to_string()),
            &composer_detection(Some(ComposerSemantic::Clean)),
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Visible(String::new()),
            std::slice::from_ref(&candidate),
            true,
        );
        assert_eq!(submitted.state, ComposerState::CyclopsNotificationSubmitted);
        assert_eq!(submitted.proof, ComposerProof::ExactNotification);
    }

    #[test]
    fn submitted_record_with_visible_exact_bytes_is_still_staged() {
        let (candidate, recipient, binding) = composer_candidate(NotificationState::Submitted);
        let expected = cyclops_proto::render_doorbell_v1(&candidate.record.message_id);
        let projection = project_composer(
            Some(ComposerSemantic::HumanInput),
            Some(&candidate.record.attempt_id.to_string()),
            &composer_detection(Some(ComposerSemantic::HumanInput)),
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Visible(expected),
            std::slice::from_ref(&candidate),
            true,
        );

        assert_eq!(projection.state, ComposerState::CyclopsNotificationStaged);
        assert_eq!(projection.proof, ComposerProof::ExactNotification);
    }

    #[test]
    fn submit_intent_and_post_staging_withdrawal_do_not_claim_submission() {
        for state in [
            NotificationState::Submitting,
            NotificationState::WithdrawnAfterStaging,
        ] {
            let (candidate, recipient, binding) = composer_candidate(state);
            let projection = project_composer(
                Some(ComposerSemantic::Clean),
                Some(&candidate.record.attempt_id.to_string()),
                &composer_detection(Some(ComposerSemantic::Clean)),
                false,
                &binding,
                Some(recipient),
                &ComposerCapture::Visible(String::new()),
                std::slice::from_ref(&candidate),
                true,
            );

            assert_eq!(
                projection.state,
                ComposerState::ComposerAmbiguous,
                "{state:?}"
            );
            assert_eq!(projection.proof, ComposerProof::Ambiguous, "{state:?}");
        }
    }

    #[test]
    fn copy_mode_and_stale_frames_never_reuse_a_semantic_as_current() {
        let mut detection = composer_detection(Some(ComposerSemantic::Clean));
        let copy_mode = project_composer(
            detection.composer_semantic,
            None,
            &detection,
            true,
            &BindingObservation::Unprovable,
            None,
            &ComposerCapture::NotRead,
            &[],
            true,
        );
        assert_eq!(copy_mode.state, ComposerState::ComposerAmbiguous);
        assert_eq!(copy_mode.proof, ComposerProof::Ambiguous);
        assert_eq!(copy_mode.reason, Some("pane_in_mode"));

        detection.stale = true;
        let stale = project_composer(
            detection.composer_semantic,
            None,
            &detection,
            false,
            &BindingObservation::Unprovable,
            None,
            &ComposerCapture::NotRead,
            &[],
            true,
        );
        assert_eq!(stale.state, ComposerState::ComposerAmbiguous);
        assert_eq!(stale.proof, ComposerProof::Unprovable);
        assert_eq!(stale.reason, Some("detection_stale"));
    }

    #[test]
    fn process_change_during_capture_never_projects_exact_ownership() {
        for (state, semantic, capture) in [
            (
                NotificationState::Staged,
                ComposerSemantic::HumanInput,
                ComposerCapture::BindingChanged,
            ),
            (
                NotificationState::Submitted,
                ComposerSemantic::Clean,
                ComposerCapture::BindingChanged,
            ),
        ] {
            let (candidate, recipient, binding) = composer_candidate(state);
            let projection = project_composer(
                Some(semantic),
                Some(&candidate.record.attempt_id.to_string()),
                &composer_detection(Some(semantic)),
                false,
                &binding,
                Some(recipient),
                &capture,
                std::slice::from_ref(&candidate),
                true,
            );

            assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
            assert_eq!(projection.proof, ComposerProof::Ambiguous);
            assert_eq!(projection.reason, Some("binding_changed_during_capture"));
            assert_eq!(projection.binding, None);
        }
    }

    #[test]
    fn capture_bookend_requires_the_same_route_mode_and_process_generations() {
        let (_, recipient, binding) = composer_candidate(NotificationState::Staged);
        let before = PaneRow {
            pane_id: "%1".into(),
            window_id: "@1".into(),
            window_name: "main".into(),
            title: String::new(),
            dead: false,
            in_mode: false,
            current_command: "claude".into(),
            width: 120,
            height: 40,
            active: true,
            pane_pid: 4000,
        };
        let after = before.clone();
        assert!(composer_capture_binding_is_stable(
            &before,
            Some(&after),
            Some(recipient),
            Some(recipient),
            &binding,
            Some(&binding),
        ));

        for changed in [
            {
                let mut row = after.clone();
                row.in_mode = true;
                row
            },
            {
                let mut row = after.clone();
                row.dead = true;
                row
            },
            {
                let mut row = after.clone();
                row.pane_pid += 1;
                row
            },
        ] {
            assert!(!composer_capture_binding_is_stable(
                &before,
                Some(&changed),
                Some(recipient),
                Some(recipient),
                &binding,
                Some(&binding),
            ));
        }
        assert!(!composer_capture_binding_is_stable(
            &before,
            Some(&after),
            Some(recipient),
            None,
            &binding,
            Some(&binding),
        ));

        let BindingObservation::Bound(mut replaced_agent) = binding.clone() else {
            unreachable!("fixture has a complete binding")
        };
        replaced_agent.agent.birth += 1;
        let replaced_agent = BindingObservation::Bound(replaced_agent);
        assert!(!composer_capture_binding_is_stable(
            &before,
            Some(&after),
            Some(recipient),
            Some(recipient),
            &binding,
            Some(&replaced_agent),
        ));
    }

    #[test]
    fn conflicting_runtime_readings_never_project_exact_ownership() {
        let (candidate, recipient, binding) = composer_candidate(NotificationState::Staged);
        let expected = cyclops_proto::render_doorbell_v1(&candidate.record.message_id);
        let mut detection = composer_detection(Some(ComposerSemantic::HumanInput));
        detection.disagreement = true;
        detection.readings.push(SensorReading {
            sensor: Sensor::Hook,
            state: AgentState::Working,
            rule: "turn_start".into(),
            ts: 6,
        });
        let projection = project_composer(
            detection.composer_semantic,
            Some(&candidate.record.attempt_id.to_string()),
            &detection,
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Visible(expected),
            std::slice::from_ref(&candidate),
            true,
        );

        assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
        assert_eq!(projection.proof, ComposerProof::Ambiguous);
        assert_eq!(projection.reason, Some("terminal_state_unsafe"));
        assert_eq!(projection.binding, None);
    }

    #[test]
    fn submitted_record_with_unprovable_capture_never_claims_exact_ownership() {
        let (candidate, recipient, binding) = composer_candidate(NotificationState::Submitted);
        let projection = project_composer(
            Some(ComposerSemantic::Clean),
            Some(&candidate.record.attempt_id.to_string()),
            &composer_detection(Some(ComposerSemantic::Clean)),
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Unprovable,
            std::slice::from_ref(&candidate),
            true,
        );

        assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
        assert_eq!(projection.proof, ComposerProof::Unprovable);
        assert_eq!(projection.reason, Some("composer_capture_unprovable"));
        assert!(projection.binding.is_none());
    }

    #[test]
    fn exact_notification_projection_fails_closed_on_hidden_or_extra_content() {
        let (candidate, recipient, binding) = composer_candidate(NotificationState::Submitted);
        let owner = candidate.record.attempt_id.to_string();
        let expected = cyclops_proto::render_doorbell_v1(&candidate.record.message_id);
        for (capture, proof) in [
            (
                ComposerCapture::Visible(format!("{expected} unexpected")),
                ComposerProof::Ambiguous,
            ),
            (ComposerCapture::Hidden, ComposerProof::Unprovable),
        ] {
            let projection = project_composer(
                Some(ComposerSemantic::HumanInput),
                Some(&owner),
                &composer_detection(Some(ComposerSemantic::HumanInput)),
                false,
                &binding,
                Some(recipient),
                &capture,
                std::slice::from_ref(&candidate),
                true,
            );
            assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
            assert_eq!(projection.proof, proof);
        }

        let projection = project_composer(
            Some(ComposerSemantic::HumanInput),
            Some(&owner),
            &composer_detection(Some(ComposerSemantic::HumanInput)),
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Visible(expected),
            &[candidate.clone(), candidate],
            true,
        );
        assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
        assert_eq!(projection.proof, ComposerProof::Ambiguous);
        assert_eq!(projection.reason, Some("multiple_active_notifications"));
        assert_eq!(projection.candidate_count, 2);
        assert!(projection.binding.is_none());
    }

    #[test]
    fn a_direct_hold_is_not_reported_as_multiple_notifications() {
        let direct = project_composer(
            Some(ComposerSemantic::Clean),
            Some("m-direct#1"),
            &composer_detection(Some(ComposerSemantic::Clean)),
            false,
            &BindingObservation::Unprovable,
            None,
            &ComposerCapture::Visible(String::new()),
            &[],
            true,
        );
        assert_eq!(direct.state, ComposerState::ComposerAmbiguous);
        assert_eq!(direct.proof, ComposerProof::Unprovable);
        assert_eq!(direct.reason, Some("direct_delivery_hold_unprovable"));
        assert_eq!(direct.candidate_count, 0);
    }

    #[test]
    fn pane_root_generation_is_required_for_exact_composer_ownership() {
        let (candidate, recipient, binding) = composer_candidate(NotificationState::Submitted);
        let owner = candidate.record.attempt_id.to_string();
        let expected = cyclops_proto::render_doorbell_v1(&candidate.record.message_id);
        let BindingObservation::Bound(mut replaced_root) = binding.clone() else {
            unreachable!("composer fixture has a complete binding")
        };
        replaced_root.pane_root = crate::identity::ProcId {
            pid: replaced_root.pane_root.pid,
            birth: replaced_root.pane_root.birth + 1,
        };
        let projection = project_composer(
            Some(ComposerSemantic::HumanInput),
            Some(&owner),
            &composer_detection(Some(ComposerSemantic::HumanInput)),
            false,
            &BindingObservation::Bound(replaced_root),
            Some(recipient),
            &ComposerCapture::Visible(expected.clone()),
            std::slice::from_ref(&candidate),
            true,
        );
        assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
        assert_eq!(projection.proof, ComposerProof::Ambiguous);

        let mut legacy = candidate;
        legacy
            .record
            .binding
            .as_mut()
            .expect("composer fixture has a durable binding")
            .pane_root = None;
        let projection = project_composer(
            Some(ComposerSemantic::HumanInput),
            Some(&owner),
            &composer_detection(Some(ComposerSemantic::HumanInput)),
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Visible(expected),
            std::slice::from_ref(&legacy),
            true,
        );
        assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
        assert_eq!(projection.proof, ComposerProof::Unprovable);
    }

    #[test]
    fn an_ownerless_legacy_candidate_names_its_missing_durable_binding() {
        let (mut candidate, recipient, binding) =
            composer_candidate(NotificationState::AttentionRequired);
        candidate
            .record
            .binding
            .as_mut()
            .expect("composer fixture has a durable binding")
            .pane_root = None;
        let expected = cyclops_proto::render_doorbell_v1(&candidate.record.message_id);

        let projection = project_composer(
            Some(ComposerSemantic::HumanInput),
            None,
            &composer_detection(Some(ComposerSemantic::HumanInput)),
            false,
            &binding,
            Some(recipient),
            &ComposerCapture::Visible(expected),
            std::slice::from_ref(&candidate),
            true,
        );

        assert_eq!(projection.state, ComposerState::ComposerAmbiguous);
        assert_eq!(projection.proof, ComposerProof::Unprovable);
        assert_eq!(projection.reason, Some("durable_binding_incomplete"));
        assert_eq!(
            projection.notification_attempt,
            Some(candidate.record.attempt_id)
        );
    }

    #[test]
    fn claimed_legacy_recovery_requires_semantic_clean_and_exact_visible_empty() {
        let (candidate, _, _) = composer_candidate(NotificationState::AttentionRequired);
        let binding = candidate
            .record
            .binding
            .as_ref()
            .expect("composer fixture has a durable binding");
        let clean = composer_detection(Some(ComposerSemantic::Clean));
        assert!(claimed_legacy_recovery_ready(
            &clean,
            false,
            Some("claude"),
            binding,
            &ComposerCapture::Visible(String::new()),
        ));

        for semantic in [
            ComposerSemantic::GhostSuggestion,
            ComposerSemantic::HumanInput,
            ComposerSemantic::Ambiguous,
        ] {
            assert!(!claimed_legacy_recovery_ready(
                &composer_detection(Some(semantic)),
                false,
                Some("claude"),
                binding,
                &ComposerCapture::Visible(String::new()),
            ));
        }
        for capture in [
            ComposerCapture::Visible("text".into()),
            ComposerCapture::Hidden,
            ComposerCapture::NotRead,
            ComposerCapture::BindingChanged,
        ] {
            assert!(!claimed_legacy_recovery_ready(
                &clean,
                false,
                Some("claude"),
                binding,
                &capture,
            ));
        }
    }

    fn entry(state: AgentState, ts: u64) -> HookEntry {
        HookEntry::bound(
            crate::identity::ProcId { pid: 1, birth: 1 },
            None,
            SensorReading {
                sensor: Sensor::Hook,
                state,
                rule: "Stop".into(),
                ts,
            },
        )
    }

    fn start_entry(ts: u64) -> HookEntry {
        HookEntry::provisional_start(
            crate::identity::ProcId { pid: 1, birth: 1 },
            None,
            SensorReading {
                sensor: Sensor::Hook,
                state: AgentState::Working,
                rule: "UserPromptSubmit".into(),
                ts,
            },
        )
    }

    fn end_entry(ts: u64) -> HookEntry {
        HookEntry::turn_ended(
            crate::identity::ProcId { pid: 1, birth: 1 },
            None,
            SensorReading {
                sensor: Sensor::Hook,
                state: AgentState::Idle,
                rule: "StopFailure".into(),
                ts,
            },
            turnkey::TurnKey::for_test(&["session", "prompt"]),
        )
    }

    #[test]
    fn hook_reading_ages_out_on_ttl() {
        let mut e = entry(AgentState::Working, 1_000);
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Unknown),
                false,
                false,
                true,
                1_000 + HOOK_READING_TTL_MS
            ),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Unknown),
                false,
                false,
                true,
                1_001 + HOOK_READING_TTL_MS
            ),
            HookAction::Drop
        );
    }

    #[test]
    fn hook_reading_invalidated_by_repeated_disagreement() {
        let mut e = entry(AgentState::Working, 1_000);
        // Rules see nothing: no evidence against the hook.
        for _ in 0..10 {
            assert_eq!(
                hook_action_observed(
                    &mut e,
                    &lifecycle_detection(Sensor::Screen, AgentState::Unknown),
                    false,
                    false,
                    true,
                    2_000
                ),
                HookAction::Use
            );
        }
        // Two contradictions survive, the third invalidates.
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Idle),
                false,
                false,
                true,
                2_000
            ),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Idle),
                false,
                false,
                true,
                2_000
            ),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Idle),
                false,
                false,
                true,
                2_000
            ),
            HookAction::Drop
        );
    }

    #[test]
    fn hook_agreement_resets_the_disagreement_streak() {
        let mut e = entry(AgentState::Working, 1_000);
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Idle),
                false,
                false,
                true,
                2_000
            ),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Idle),
                false,
                false,
                true,
                2_000
            ),
            HookAction::Use
        );
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Working),
                false,
                false,
                true,
                2_000
            ),
            HookAction::Use
        );
        assert_eq!(e.disagreements, 0);
        assert_eq!(
            hook_action_observed(
                &mut e,
                &lifecycle_detection(Sensor::Screen, AgentState::Idle),
                false,
                false,
                true,
                2_000
            ),
            HookAction::Use
        );
    }

    #[test]
    fn an_active_start_never_silently_ages_to_idle() {
        let mut entry = start_entry(1_000);
        for round in 0..10 {
            assert_eq!(
                hook_action_observed(
                    &mut entry,
                    &lifecycle_detection(Sensor::Screen, AgentState::Idle),
                    false,
                    false,
                    true,
                    1_001 + HOOK_READING_TTL_MS + round
                ),
                HookAction::Use,
                "idle frame {round} discarded the active start"
            );
        }
        assert_eq!(entry.disagreements, 0);
    }

    #[test]
    fn an_exact_active_start_accepts_only_its_lifecycle_end() {
        let agent = crate::identity::ProcId { pid: 1, birth: 1 };
        let t1 = turnkey::TurnKey::for_test(&["session", "turn-1"]);
        let t2 = turnkey::TurnKey::for_test(&["session", "turn-2"]);
        let reading = SensorReading {
            sensor: Sensor::Hook,
            state: AgentState::Working,
            rule: "UserPromptSubmit".into(),
            ts: 1,
        };
        let exact = HookEntry::turn_started(agent, Some("codex".into()), reading, t1.clone());
        assert!(exact.active_start_matches(agent, Some("codex"), Some(&t1)));
        assert!(!exact.active_start_matches(agent, Some("codex"), Some(&t2)));
        assert!(
            !exact.active_start_matches(agent, Some("codex"), None),
            "an unkeyed end cannot authorize exact lifecycle settlement"
        );
    }

    #[test]
    fn an_unkeyed_confirmed_start_accepts_only_an_unkeyed_end() {
        let agent = crate::identity::ProcId { pid: 1, birth: 1 };
        let reading = SensorReading {
            sensor: Sensor::Hook,
            state: AgentState::Working,
            rule: "PreInvocation".into(),
            ts: 1,
        };
        let unkeyed = HookEntry::unkeyed_turn_started(agent, Some("agy".into()), reading.clone());
        let provisional = HookEntry::provisional_start(agent, Some("agy".into()), reading);
        let turn = turnkey::TurnKey::for_test(&["turn"]);

        assert!(unkeyed.confirmed_unkeyed_start_for(agent, Some("agy")));
        assert!(!unkeyed.active_start_matches(agent, Some("agy"), Some(&turn)));
        assert!(!provisional.confirmed_unkeyed_start_for(agent, Some("agy")));
        assert!(!unkeyed.confirmed_unkeyed_start_for(agent, Some("other")));
    }

    #[test]
    fn an_active_start_reports_working_without_authorizing_a_write() {
        for visual_state in [AgentState::Idle, AgentState::IdleWithInput] {
            let mut detection = Detection {
                state: visual_state,
                readings: vec![SensorReading {
                    sensor: Sensor::Screen,
                    state: visual_state,
                    rule: "composer".into(),
                    ts: 2,
                }],
                disagreement: false,
                decided_by: "composer".into(),
                stale: false,
                write_ready: false,
                write_block: None,
                composer_semantic: None,
            };
            let start = start_entry(3);
            apply_hook_reading(
                &mut detection,
                start.reading,
                start.active_start,
                start.authoritative_end,
            );
            let detection = detection.stamped(false, ComposerHold::Clear);

            assert_eq!(detection.state, AgentState::Working, "{visual_state}");
            assert!(detection.disagreement, "{visual_state}");
            assert_eq!(detection.decided_by, "hook:UserPromptSubmit");
            assert!(!detection.write_ready, "{visual_state}");
        }
    }

    #[test]
    fn a_current_bound_start_and_agreeing_screen_stay_working() {
        let agent = crate::identity::ProcId { pid: 1, birth: 1 };
        for observed_at in [10, 11, 12] {
            let mut detection = lifecycle_detection(Sensor::Screen, AgentState::Working);
            detection.readings[0].ts = observed_at;
            let start = HookEntry::unkeyed_turn_started(
                agent,
                Some("fixture".into()),
                SensorReading {
                    sensor: Sensor::Hook,
                    state: AgentState::Working,
                    rule: "PreInvocation".into(),
                    ts: 3,
                },
            );
            apply_hook_reading(
                &mut detection,
                start.reading,
                start.active_start,
                start.authoritative_end,
            );
            let detection = detection.stamped(false, ComposerHold::Clear);

            assert_eq!(detection.state, AgentState::Working);
            assert!(!detection.disagreement);
            assert!(!detection.write_ready);
        }
    }

    #[test]
    fn blocked_visuals_remain_authoritative_during_an_active_start() {
        let states = [
            AgentState::Unknown,
            AgentState::Idle,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
            AgentState::Dead,
        ];
        for blocked in states.into_iter().filter(|state| state.is_blocked()) {
            let mut detection = Detection {
                state: blocked,
                readings: vec![SensorReading {
                    sensor: Sensor::Screen,
                    state: blocked,
                    rule: "blocked_screen".into(),
                    ts: 2,
                }],
                disagreement: false,
                decided_by: "blocked_screen".into(),
                stale: false,
                write_ready: false,
                write_block: None,
                composer_semantic: None,
            };
            let start = start_entry(3);
            apply_hook_reading(
                &mut detection,
                start.reading,
                start.active_start,
                start.authoritative_end,
            );
            let detection = detection.stamped(false, ComposerHold::Clear);

            assert_eq!(detection.state, blocked);
            assert_eq!(detection.decided_by, "blocked_screen");
            assert!(detection.disagreement);
            assert!(!detection.write_ready);
        }
    }

    #[test]
    fn a_conclusive_end_never_overwrites_current_working_or_safety_state() {
        for (visual, expected) in [
            (AgentState::Working, AgentState::Working),
            (AgentState::Unknown, AgentState::Idle),
            (AgentState::IdleWithInput, AgentState::IdleWithInput),
            (AgentState::BlockedModal, AgentState::BlockedModal),
            (AgentState::BlockedPermission, AgentState::BlockedPermission),
            (AgentState::BlockedQuota, AgentState::BlockedQuota),
        ] {
            let mut detection = Detection {
                state: visual,
                readings: vec![SensorReading {
                    sensor: Sensor::Screen,
                    state: visual,
                    rule: "visual".into(),
                    ts: 2,
                }],
                disagreement: false,
                decided_by: "visual".into(),
                stale: false,
                write_ready: false,
                write_block: None,
                composer_semantic: None,
            };
            let end = end_entry(3);
            apply_hook_reading(
                &mut detection,
                end.reading,
                end.active_start,
                end.authoritative_end,
            );
            assert_eq!(detection.state, expected, "visual state {visual}");
            assert!(
                visual == AgentState::Working
                    || visual == AgentState::IdleWithInput
                    || visual == AgentState::Unknown
                    || visual.is_blocked(),
                "current visual safety state changed"
            );
        }
    }

    #[test]
    fn an_old_stop_cannot_override_repeated_current_visual_working() {
        for observed_at in [3, 10, 11, 12] {
            let mut detection = lifecycle_detection(Sensor::Screen, AgentState::Working);
            detection.readings[0].ts = observed_at;
            let end = end_entry(3);
            apply_hook_reading(
                &mut detection,
                end.reading,
                end.active_start,
                end.authoritative_end,
            );
            let detection = detection.stamped(false, ComposerHold::Clear);

            assert_eq!(detection.state, AgentState::Working);
            assert_eq!(detection.decided_by, "fixture");
            assert!(!detection.write_ready);
        }
    }

    #[test]
    fn a_conclusive_end_is_an_edge_not_a_persistent_idle_level() {
        for state in [
            AgentState::Working,
            AgentState::Idle,
            AgentState::IdleWithInput,
        ] {
            let mut end = end_entry(3);
            let current = lifecycle_detection(Sensor::Screen, state);
            assert_eq!(
                hook_action_observed(&mut end, &current, false, false, true, 4),
                HookAction::Drop
            );
        }
        let mut end = end_entry(3);
        let blocked = lifecycle_detection(Sensor::Screen, AgentState::BlockedModal);
        assert_eq!(
            hook_action_observed(&mut end, &blocked, false, false, true, 4),
            HookAction::Use
        );
    }

    // The shipped codex esc rules: dim after the glyph is a ghost
    // suggestion (idle), bare text is typed input (idle_with_input), the
    // plain rule is the idle-biased fallback.
    const ESC_FIXTURE: &str = r#"
[agent]
id = "codex"
display_name = "Codex esc fixture"
process_names = ["codex"]

[[rule]]
id = "composer_typed_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+[^\x1b\s]']

[[rule]]
id = "composer_ghost_suggestion"
state = "idle"
priority = 1040
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+\x1b\[2m']

[[rule]]
id = "composer_empty_or_ghost"
state = "idle"
priority = 1000
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*›']
"#;

    #[test]
    fn screen_winner_esc_discriminates_typed_from_ghost() {
        let m = Manifest::parse(ESC_FIXTURE, Path::new("codex.toml")).unwrap();
        let typed_plain = "› fix the rate limiter in gateway.rs";
        let typed_esc = "\u{1b}[1m›\u{1b}[0m fix the rate limiter in gateway.rs";
        let ghost_plain = "› Find and fix a bug in @filename";
        let ghost_esc = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mFind and fix a bug in @filename\u{1b}[0m";

        // With the escaped capture the esc rules decide.
        let r = screen_winner_esc(&m, typed_plain, Some(typed_esc)).unwrap();
        assert_eq!(r.id, "composer_typed_input");
        assert_eq!(r.state, AgentState::IdleWithInput);
        let r = screen_winner_esc(&m, ghost_plain, Some(ghost_esc)).unwrap();
        assert_eq!(r.id, "composer_ghost_suggestion");
        assert_eq!(r.state, AgentState::Idle);

        // Without one the esc rules fail closed: idle-biased fallback,
        // which is exactly the gap the daemon-side capture closes.
        let r = screen_winner(&m, typed_plain).unwrap();
        assert_eq!(r.id, "composer_empty_or_ghost");
        assert_eq!(r.state, AgentState::Idle);

        assert!(m.has_escaped_rules());
        assert!(!manifest().has_escaped_rules());
    }

    #[test]
    fn argv_basename_parses_ps_args_output() {
        assert_eq!(
            parse_argv_basename("/Users/x/.local/bin/claude --continue\n"),
            Some("claude".into())
        );
        assert_eq!(parse_argv_basename("  cat  \n"), Some("cat".into()));
        assert_eq!(parse_argv_basename("\n"), None);
        assert_eq!(parse_argv_basename(""), None);
    }

    const SLEEP_FIXTURE: &str = r#"
[agent]
id = "sleeper"
display_name = "Sleep fixture"
process_names = []
argv_basenames = ["sleep"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']
"#;

    #[test]
    fn a_recovered_exact_end_is_durable_before_runtime_clearance() {
        use crate::mailbox::{MailboxDirectory, MailboxIdentity, MailboxSend, MessageStore};
        use crate::notification_adapter::NotificationContext;
        use cyclops_proto::{
            NotificationAttentionCause, NotificationBinding, NotificationManifestId,
            NotificationTransport, ProcessInstanceId, RecipientKey, SessionInstanceId, TmuxPaneId,
        };

        let mut inner = inner_with(BTreeMap::new());
        let session = "00000000-0000-4000-8000-000000000002"
            .parse::<SessionInstanceId>()
            .unwrap();
        let tmux_pane = "%1".parse::<TmuxPaneId>().unwrap();
        let recipient = RecipientKey::agent(inner.workspace_id, session, tmux_pane);
        let directory = MailboxDirectory::new(
            inner.workspace_id,
            [MailboxIdentity {
                key: recipient,
                label: "codex".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(
            &inner.state_root,
            Path::new("workspaces/recovery/messages.ndjson"),
            inner.workspace_id,
            "boot",
        )
        .unwrap();
        let service = Arc::new(crate::mailbox::MailboxService::new(directory, store));
        let accepted = service
            .send(
                service.admin(),
                MailboxSend {
                    addresses: vec!["codex".into()],
                    recipient_keys: None,
                    subject: "recover".into(),
                    body: "body".into(),
                    fyi: false,
                    client_key: None,
                    supersedes: None,
                },
            )
            .unwrap();
        let queued = service
            .prepare_oldest_notification(recipient)
            .unwrap()
            .unwrap();
        let context = NotificationContext::new(
            service.store_handle(),
            accepted.message_id,
            recipient,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        context
            .record_writing(
                ProcessInstanceId::new(69, 1).unwrap(),
                ProcessInstanceId::new(70, 2).unwrap(),
                ProcessInstanceId::new(agent.pid, agent.birth).unwrap(),
                "codex",
                NotificationTransport::Doorbell,
                None,
            )
            .unwrap();
        context
            .record_attention(NotificationAttentionCause::VerifyFailed)
            .unwrap();
        Arc::get_mut(&mut inner).unwrap().mailbox = Some(Arc::clone(&service));
        *inner.composer_recovery.lock().unwrap() =
            crate::composer_recovery::RecoveryCoordinator::new([queued.attempt_id]);

        let turn = turnkey::TurnKey::for_test(&["session", "turn"]);
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 9,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: Some(ComposerSemantic::Clean),
        };
        inner.detections.lock().unwrap().insert(
            pane(),
            DetEntry {
                detection: clean.clone(),
                binding: None,
                manifest: Some("codex".into()),
                occupant: Some(70),
                agent: Some(agent),
                in_mode: false,
                quota_screen_clear: false,
                hold: ComposerHold::TurnStarted { since_ms: 8 },
                turn: Some(turn.clone()),
                hold_owner: Some(queued.attempt_id.to_string()),
                composer: ComposerProjection::default(),
                working_confirmed: false,
                since: std::time::Instant::now(),
            },
        );
        {
            let mut ends = inner.turn_ends.lock().unwrap();
            assert!(turnkey::PaneEnds::pin(
                &mut ends,
                &pane(),
                agent,
                "codex",
                &turn
            ));
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", turn.clone());
        }
        let live = NotificationBinding {
            recipient,
            pane_root: Some(ProcessInstanceId::new(69, 1).unwrap()),
            leader: Some(ProcessInstanceId::new(70, 2).unwrap()),
            agent: ProcessInstanceId::new(agent.pid, agent.birth).unwrap(),
            manifest: NotificationManifestId::new("codex").unwrap(),
        };

        assert_eq!(
            crate::composer_recovery::retire_exact_lifecycle(&inner, 0, "%1", Some(&live), true,),
            crate::composer_recovery::LifecycleRetirement::Durable(queued.attempt_id)
        );
        assert!(service.active_notification_barriers().unwrap().is_empty());
        assert!(turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane(),
            agent,
            "codex",
            &turn
        ));

        let (hold, stranded) = settle_turn(
            &mut inner.turn_ends.lock().unwrap(),
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            ComposerHold::TurnStarted { since_ms: 8 },
            &clean,
        );
        assert_eq!(hold, ComposerHold::Clear);
        assert!(!stranded);
        assert!(!turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane(),
            agent,
            "codex",
            &turn
        ));
    }

    #[test]
    fn a_failed_recovery_retirement_keeps_the_exact_end_and_hold() {
        let inner = inner_with(BTreeMap::new());
        let attempt_id = cyclops_proto::NotificationAttemptId::generate();
        *inner.composer_recovery.lock().unwrap() =
            crate::composer_recovery::RecoveryCoordinator::new([attempt_id]);
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "turn"]);
        inner.detections.lock().unwrap().insert(
            pane(),
            DetEntry {
                detection: Detection {
                    state: AgentState::Idle,
                    readings: Vec::new(),
                    disagreement: false,
                    decided_by: "fixture".into(),
                    stale: false,
                    write_ready: false,
                    write_block: None,
                    composer_semantic: Some(ComposerSemantic::Clean),
                },
                binding: None,
                manifest: Some("codex".into()),
                occupant: Some(70),
                agent: Some(agent),
                in_mode: false,
                quota_screen_clear: false,
                hold: ComposerHold::TurnStarted { since_ms: 8 },
                turn: Some(turn.clone()),
                hold_owner: Some(attempt_id.to_string()),
                composer: ComposerProjection::default(),
                working_confirmed: false,
                since: std::time::Instant::now(),
            },
        );
        {
            let mut ends = inner.turn_ends.lock().unwrap();
            assert!(turnkey::PaneEnds::pin(
                &mut ends,
                &pane(),
                agent,
                "codex",
                &turn
            ));
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", turn.clone());
        }
        let recipient = RecipientKey::agent(
            inner.workspace_id,
            "00000000-0000-4000-8000-000000000002".parse().unwrap(),
            "%1".parse().unwrap(),
        );
        let live = cyclops_proto::NotificationBinding {
            recipient,
            pane_root: Some(cyclops_proto::ProcessInstanceId::new(69, 1).unwrap()),
            leader: Some(cyclops_proto::ProcessInstanceId::new(70, 2).unwrap()),
            agent: cyclops_proto::ProcessInstanceId::new(agent.pid, agent.birth).unwrap(),
            manifest: cyclops_proto::NotificationManifestId::new("codex").unwrap(),
        };

        assert_eq!(
            crate::composer_recovery::retire_exact_lifecycle(&inner, 0, "%1", Some(&live), true,),
            crate::composer_recovery::LifecycleRetirement::Blocked(
                "composer_recovery_store_unavailable"
            )
        );
        let entry = inner
            .detections
            .lock()
            .unwrap()
            .get(&pane())
            .unwrap()
            .clone();
        assert_eq!(entry.hold, ComposerHold::TurnStarted { since_ms: 8 });
        assert_eq!(entry.turn.as_ref(), Some(&turn));
        assert!(turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane(),
            agent,
            "codex",
            &turn
        ));
    }

    fn inner_with(manifests: BTreeMap<String, Manifest>) -> Arc<Inner> {
        inner_with_stop(manifests, tokio::sync::watch::channel(false).1)
    }

    fn inner_with_stop(
        manifests: BTreeMap<String, Manifest>,
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> Arc<Inner> {
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-argv-cache-{}",
            uuid::Uuid::new_v4()
        ));
        let state_root = Arc::new(cyclops_state::StateRoot::open_or_create(&home).unwrap());
        let (registry, _) = crate::registry::Registry::load(Arc::clone(&state_root));
        let workspace_id = crate::workspaceid::load_or_create(&state_root).unwrap();
        let session_identities = crate::sessionstore::SessionIdentities::open(&state_root).unwrap();
        Arc::new(Inner {
            cfg: crate::Config::defaults(&home),
            state_root,
            state_repair: cyclops_state::RepairSummary::default(),
            workspace_id,
            session_identities: StdMutex::new(session_identities),
            mailbox: None,
            composer_recovery: StdMutex::new(
                crate::composer_recovery::RecoveryCoordinator::default(),
            ),
            mailbox_publication: StdMutex::new(()),
            mailbox_publish_pause: StdMutex::new(None),
            boot_id: "b-test".into(),
            started: std::time::Instant::now(),
            tmux_version: "3.6a".into(),
            manifests,
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            session_registration: StdMutex::new(()),
            events: tokio::sync::broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::new()),
            route_evidence_generations: StdMutex::new(HashMap::new()),
            pane_recomputes: StdMutex::new(HashMap::new()),
            lifecycle_rechecks: StdMutex::new(HashMap::new()),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
            hook_lifecycle: StdMutex::new(crate::hook_lifecycle::Store::new()),
            turn_ends: StdMutex::new(crate::turnkey::Ends::new()),
            argv_cache: StdMutex::new(HashMap::new()),
            engine: crate::delivery::Engine::new(),
            ack_state: crate::ack::AckState::new(),
            hook_liveness: crate::selftest::HookLiveness::new(),
            inject_pause: StdMutex::new(None),
            fail_chrome_restore: std::sync::atomic::AtomicBool::new(false),
            fail_next_final_binding_observation: std::sync::atomic::AtomicBool::new(false),
            workspace_ui: StdMutex::new(crate::workspace_ui::WorkspaceUiState::default()),
            shutdown_request: tokio::sync::watch::channel(false).0,
            stop,
            extra_tasks: StdMutex::new(Vec::new()),
        })
    }

    #[test]
    fn explicit_full_screen_decisions_are_stable_lifecycle_observations() {
        for cause in [
            "status",
            "pane.read",
            "gate",
            "pre_paste",
            "prewrite_block_reconcile",
        ] {
            assert_eq!(
                LifecycleObservation::from_cause(cause),
                LifecycleObservation::Stable
            );
        }
        assert_eq!(
            LifecycleObservation::from_cause("agent.state.report"),
            LifecycleObservation::None
        );
    }

    #[test]
    fn candidate_binding_retirement_requires_positive_replacement_evidence() {
        let old_agent = crate::identity::ProcId { pid: 70, birth: 2 };
        let new_agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let bound = |agent, manifest: &str| {
            BindingObservation::Bound(Binding {
                pane_root: crate::identity::ProcId { pid: 69, birth: 1 },
                leader: agent,
                agent,
                manifest: manifest.to_string(),
            })
        };

        assert!(!binding_replacement_proven(
            Some(old_agent),
            Some("claude"),
            &BindingObservation::Unprovable,
            None,
        ));
        assert!(!binding_replacement_proven(
            Some(old_agent),
            Some("claude"),
            &bound(old_agent, "claude"),
            Some("claude"),
        ));
        assert!(!binding_replacement_proven(
            None,
            None,
            &bound(new_agent, "claude"),
            Some("claude"),
        ));
        assert!(binding_replacement_proven(
            Some(old_agent),
            Some("claude"),
            &bound(new_agent, "claude"),
            Some("claude"),
        ));
        assert!(binding_replacement_proven(
            Some(old_agent),
            Some("claude"),
            &bound(old_agent, "codex"),
            Some("codex"),
        ));
        for absent in [BindingObservation::NotVendor, BindingObservation::Gone] {
            assert!(binding_replacement_proven(None, None, &absent, None,));
        }
    }

    #[tokio::test]
    async fn pane_observations_share_one_route_gate() {
        let inner = inner_with(BTreeMap::new());
        let route = PaneKey::new(0, "%1");
        let first = pane_recompute_gate(&inner, &route);
        let same = pane_recompute_gate(&inner, &route);
        let other = pane_recompute_gate(&inner, &PaneKey::new(0, "%2"));

        let _guard = first.lock().await;
        assert!(
            same.try_lock().is_err(),
            "same-pane capture was not serialized"
        );
        assert!(other.try_lock().is_ok(), "unrelated panes shared one gate");
    }

    #[tokio::test]
    async fn same_pane_candidates_share_one_recheck_worker() {
        let inner = inner_with(BTreeMap::new());
        let pane = PaneKey::new(0, "%1");
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let first = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turnkey::TurnKey::for_test(&["session", "first"]),
            "Stop",
            10,
            3_000,
        );
        schedule_candidate_end_recheck(&inner, &pane, first);
        let original = inner
            .lifecycle_rechecks
            .lock()
            .unwrap()
            .get(&pane)
            .map(|entry| Arc::clone(&entry.notify))
            .expect("first candidate owns a worker");

        let replacement = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turnkey::TurnKey::for_test(&["session", "second"]),
            "Stop",
            20,
            3_000,
        );
        schedule_candidate_end_recheck(&inner, &pane, replacement);
        {
            let rechecks = inner.lifecycle_rechecks.lock().unwrap();
            assert_eq!(rechecks.len(), 1);
            assert!(rechecks
                .get(&pane)
                .is_some_and(|current| Arc::ptr_eq(&current.notify, &original)));
        }

        cancel_lifecycle_recheck(&inner, &pane);
        tokio::task::yield_now().await;
        assert!(inner.lifecycle_rechecks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn external_observation_wakes_a_parked_recheck_after_settlement() {
        let inner = inner_with(BTreeMap::new());
        let pane = PaneKey::new(0, "%1");
        let notify = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(std::future::pending());
        inner.lifecycle_rechecks.lock().unwrap().insert(
            pane.clone(),
            LifecycleRecheckTask {
                notify: Arc::clone(&notify),
                task,
            },
        );

        schedule_lifecycle_recheck(&inner, &pane);
        tokio::time::timeout(Duration::from_millis(50), notify.notified())
            .await
            .expect("an external observation must wake the parked worker");
        cancel_lifecycle_recheck(&inner, &pane);
    }

    #[tokio::test]
    async fn a_recheck_can_retire_itself_without_cancelling_its_recompute() {
        let inner = inner_with(BTreeMap::new());
        let pane = PaneKey::new(0, "%1");
        let notify = Arc::new(tokio::sync::Notify::new());
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let task_inner = Arc::clone(&inner);
        let task_pane = pane.clone();
        let task = tokio::spawn(async move {
            let _ = start_rx.await;
            cancel_lifecycle_recheck(&task_inner, &task_pane);
            tokio::task::yield_now().await;
            let _ = done_tx.send(());
        });
        inner
            .lifecycle_rechecks
            .lock()
            .unwrap()
            .insert(pane, LifecycleRecheckTask { notify, task });

        start_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(100), done_rx)
            .await
            .expect("self-retirement aborted the active recompute")
            .expect("recheck task ended before reporting completion");
        assert!(inner.lifecycle_rechecks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn shutdown_drain_owns_every_published_recheck_task() {
        let inner = inner_with(BTreeMap::new());
        let pane = PaneKey::new(0, "%1");
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let candidate = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turnkey::TurnKey::for_test(&["session", "turn"]),
            "Stop",
            unix_ms(),
            60_000,
        );

        schedule_candidate_end_recheck(&inner, &pane, candidate);
        let mut tasks = take_lifecycle_recheck_tasks(&inner);

        assert_eq!(tasks.len(), 1, "the registry published no joinable task");
        assert!(inner.lifecycle_rechecks.lock().unwrap().is_empty());
        let mut task = tasks.pop().expect("one registered task");
        task.abort();
        let _ = (&mut task).await;
    }

    #[tokio::test]
    async fn shutdown_latch_refuses_new_lifecycle_workers() {
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let inner = inner_with_stop(BTreeMap::new(), stop_rx);
        let pane = PaneKey::new(0, "%1");
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let candidate = inner.hook_lifecycle.lock().unwrap().record_end(
            &pane,
            agent,
            "claude",
            turnkey::TurnKey::for_test(&["session", "turn"]),
            "Stop",
            10,
            0,
        );
        stop_tx.send(true).unwrap();

        schedule_candidate_end_recheck(&inner, &pane, candidate);

        assert!(inner.lifecycle_rechecks.lock().unwrap().is_empty());
    }

    #[test]
    fn receipt_checkpoint_is_stable_without_rearming_its_recheck_worker() {
        assert_eq!(
            LifecycleObservation::from_cause("receipt_checkpoint"),
            LifecycleObservation::Stable
        );
        assert!(is_candidate_recheck_cause("receipt_checkpoint"));
    }

    #[test]
    fn exact_route_detection_cache_separates_duplicate_pane_ids() {
        let inner = inner_with(BTreeMap::new());
        let entry = |state| DetEntry {
            detection: Detection {
                state,
                readings: Vec::new(),
                disagreement: false,
                decided_by: "test".into(),
                stale: false,
                write_ready: false,
                write_block: None,
                composer_semantic: None,
            },
            binding: None,
            manifest: None,
            occupant: None,
            agent: None,
            in_mode: false,
            quota_screen_clear: false,
            hold: ComposerHold::Clear,
            turn: None,
            hold_owner: None,
            composer: ComposerProjection::default(),
            working_confirmed: false,
            since: std::time::Instant::now(),
        };
        inner
            .detections
            .lock()
            .unwrap()
            .insert(PaneKey::new(0, "%1"), entry(AgentState::Idle));
        inner
            .detections
            .lock()
            .unwrap()
            .insert(PaneKey::new(1, "%1"), entry(AgentState::Working));

        assert_eq!(inner.cached_state(0, "%1"), AgentState::Idle);
        assert_eq!(inner.cached_state(1, "%1"), AgentState::Working);
    }

    /// A wrapper caught before its `exec` reads as the interpreter and binds
    /// nothing. Remembering that would pin the pane unknown for the life of
    /// the process, because the exec keeps the pid the cache is keyed on.
    #[test]
    fn a_basename_that_binds_nothing_is_re_probed_not_memoised() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        // No manifest claims "sleep": the reading is a miss, and a miss must
        // leave the cache empty so the next recompute probes again.
        let blind = inner_with(BTreeMap::new());
        assert!(argv_bound_manifest(&blind, 0, "%0", pid).is_none());
        assert!(
            blind.argv_cache.lock().unwrap().is_empty(),
            "a non-binding basename must not be memoised"
        );

        // The same pid, once a manifest claims it, binds and is remembered.
        let mut map = BTreeMap::new();
        map.insert(
            "sleeper".to_string(),
            Manifest::parse(SLEEP_FIXTURE, Path::new("sleeper.toml")).unwrap(),
        );
        let bound = inner_with(map);
        assert_eq!(
            argv_bound_manifest(&bound, 0, "%0", pid).map(|(m, _)| m.agent.id.as_str()),
            Some("sleeper")
        );
        let proc = crate::identity::ProcId::of(pid).expect("the child is alive");
        assert_eq!(
            bound
                .argv_cache
                .lock()
                .unwrap()
                .get(&(PaneKey::new(0, "%0"), proc)),
            Some(&"sleep".to_string()),
            "a binding basename is memoised for the pane"
        );

        // The SAME pid with a different birth is a different process, and
        // it reads nothing. That is the pid-reuse case: the number can be
        // handed to anything, and a cache that answered on the number
        // alone would hand the newcomer this agent's identity.
        let impostor = crate::identity::ProcId {
            pid,
            birth: proc.birth + 1,
        };
        assert_eq!(
            bound
                .argv_cache
                .lock()
                .unwrap()
                .get(&(PaneKey::new(0, "%0"), impostor)),
            None,
            "a reused pid inherited a binding it never earned"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A cached positive binding is not authentication evidence.
    ///
    /// pid and birth both survive an in-place `exec`, so a process can
    /// bind as a vendor, exec into something that is not one, and keep
    /// the identity it was admitted under. Cursor's launcher execs in
    /// place, so this is the supported process model, not an edge case.
    /// The authentication path therefore reads argv live every time; only
    /// the manifest-binding path may reuse a cached answer.
    #[test]
    fn a_cached_binding_never_authenticates_after_an_exec() {
        let mut map = BTreeMap::new();
        map.insert(
            "sleeper".to_string(),
            Manifest::parse(SLEEP_FIXTURE, Path::new("sleeper.toml")).unwrap(),
        );
        let inner = inner_with(map);
        let steady = |pid: i32| Some(crate::identity::ProcId { pid, birth: 7 });

        // Seed a positive cache for this exact identity.
        assert!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("sleep".to_string()), steady).is_some(),
            "fixture: the binding has to be cached first"
        );
        assert!(!inner.argv_cache.lock().unwrap().is_empty());

        // Same process, same identity, different program. The cached path
        // still answers from what it remembered, which is exactly why it
        // must not be the authentication route.
        assert!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("bash".to_string()), steady).is_some(),
            "fixture: the cache is expected to answer stale here"
        );
        // The live route is asked about the SAME cached identity, with the
        // post-exec argv. If it ever consulted the cache it would answer
        // "sleeper" here and this would fail.
        let us = unsafe { libc::getuid() };
        let ours = |pid: i32| Some((crate::identity::ProcId { pid, birth: 7 }, us));
        assert!(
            matches!(
                vendor_read(&inner, 4242, |_| Some("bash".to_string()), ours),
                VendorRead::NotVendor
            ),
            "authentication answered from a cached pre-exec binding"
        );
        // And it still binds when the live argv really is the vendor, so
        // the refusal above is the exec and not the plumbing.
        assert!(matches!(
            vendor_read(&inner, 4242, |_| Some("sleep".to_string()), ours),
            VendorRead::Vendor(_, _)
        ));

        // One definition, two callers. A process owned by somebody else
        // is not a vendor of ours whichever route asks, and one nobody
        // could read is doubt rather than a no.
        let theirs = |pid: i32| Some((crate::identity::ProcId { pid, birth: 7 }, us + 1));
        assert!(matches!(
            vendor_read(&inner, 4242, |_| Some("sleep".to_string()), theirs),
            VendorRead::NotVendor
        ));
        for unreadable in [
            vendor_read(&inner, 4242, |_| Some("sleep".to_string()), |_| None),
            vendor_read(&inner, 4242, |_| None, ours),
        ] {
            assert!(matches!(unreadable, VendorRead::Unprovable));
        }

        // The owner is proven ACROSS the argv read, not before it. Both
        // halves come from one observation, and both have to still hold
        // afterwards: credentials can change without the start time
        // moving, and a number can be handed on between two reads.
        let reads = std::sync::atomic::AtomicU64::new(0);
        let owner_changes = |pid: i32| {
            let n = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some((
                crate::identity::ProcId { pid, birth: 7 },
                if n == 0 { us } else { us + 1 },
            ))
        };
        assert!(
            matches!(
                vendor_read(&inner, 4242, |_| Some("sleep".to_string()), owner_changes),
                VendorRead::Unprovable
            ),
            "the owner changed under the probe and it bound anyway"
        );

        let swaps = std::sync::atomic::AtomicU64::new(0);
        let process_changes = |pid: i32| {
            let n = swaps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some((
                crate::identity::ProcId {
                    pid,
                    birth: if n == 0 { 7 } else { 8 },
                },
                us,
            ))
        };
        assert!(
            matches!(
                vendor_read(&inner, 4242, |_| Some("sleep".to_string()), process_changes),
                VendorRead::Unprovable
            ),
            "the process changed under the probe and it bound anyway"
        );
    }

    /// The identity read and the argv read are two observations of a
    /// system that does not hold still, and a pid can change hands
    /// between them.
    ///
    /// Injected rather than raced, because the OS will not reuse a pid on
    /// demand: the argv probe swaps the identity underneath as its side
    /// effect, which is exactly the interleaving. The classification has
    /// to be refused, and nothing may be written down: filing it would
    /// authorize the replacement under the predecessor's identity, and a
    /// cache-hit test cannot see that at all.
    #[test]
    fn a_pid_reused_between_the_two_reads_binds_nothing() {
        let mut map = BTreeMap::new();
        map.insert(
            "sleeper".to_string(),
            Manifest::parse(SLEEP_FIXTURE, Path::new("sleeper.toml")).unwrap(),
        );
        let inner = inner_with(map);

        let reads = std::sync::atomic::AtomicU64::new(0);
        let ident = |pid: i32| {
            // First read one process, every read after it another.
            let n = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(crate::identity::ProcId {
                pid,
                birth: if n == 0 { 100 } else { 200 },
            })
        };
        assert!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("sleep".to_string()), ident).is_none(),
            "a replacement process was classified as its predecessor"
        );
        assert!(
            inner.argv_cache.lock().unwrap().is_empty(),
            "a binding nobody could prove was memoised anyway"
        );

        // The same inputs with a stable identity do bind, so the refusal
        // above is the interleaving and not the fixture.
        let steady = |pid: i32| Some(crate::identity::ProcId { pid, birth: 100 });
        assert_eq!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("sleep".to_string()), steady)
                .map(|(m, _)| m.agent.id.as_str()),
            Some("sleeper")
        );
    }

    #[test]
    fn basename_binding_matches_either_declared_name() {
        let mut map = BTreeMap::new();
        map.insert("bash".to_string(), manifest());
        // process_names
        assert_eq!(
            manifest_for_basename(&map, "bash").map(|m| m.agent.id.as_str()),
            Some("bash")
        );
        // the wrapper's pre-exec interpreter is not a claim on the pane
        assert!(manifest_for_basename(&map, "node").is_none());
        assert!(manifest_for_basename(&BTreeMap::new(), "bash").is_none());
    }

    /// MEASURED 2026-08-06 (Claude Code 2.1.221, tmux 3.6a, live rig): a
    /// pane running a native claude read pane_current_command "2.1.221"
    /// (version symlink, F21), `ps -o args=` on pane_pid "-zsh", and
    /// tpgid " 19989\n" whose args were "claude". Pins every hop of the
    /// binding chain against the shipped manifests on that data alone.
    #[test]
    fn measured_claude_binding_triple_2_1_221() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
        let shipped: BTreeMap<String, Manifest> = cyclops_manifest::load_dir(&dir)
            .unwrap()
            .into_iter()
            .collect();

        // Comm route: nothing claims the bare version string.
        assert!(bind_manifest(&shipped, "2.1.221").is_none());

        // pane_pid's argv is the login shell and binds nothing.
        let shell = parse_argv_basename("-zsh\n").unwrap();
        assert_eq!(shell, "-zsh");
        assert!(manifest_for_basename(&shipped, &shell).is_none());

        // The measured tpgid line resolves to the foreground group leader.
        assert_eq!(parse_tpgid(" 19989\n"), Some(19989));

        // That leader's argv is what binds the claude manifest.
        let agent = parse_argv_basename("claude\n").unwrap();
        assert_eq!(
            manifest_for_basename(&shipped, &agent).map(|m| m.agent.id.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn tpgid_parses_ps_output_and_rejects_no_terminal() {
        assert_eq!(parse_tpgid("  6254\n"), Some(6254));
        // A pane with no controlling terminal: -1 names no process, and 0
        // is not a pid either. Both must fall back to pane_pid rather than
        // send `ps -p` after something that cannot exist.
        assert_eq!(parse_tpgid("   -1\n"), None);
        assert_eq!(parse_tpgid("0\n"), None);
        assert_eq!(parse_tpgid("\n"), None);
        assert_eq!(parse_tpgid("not a pid\n"), None);
    }
}
