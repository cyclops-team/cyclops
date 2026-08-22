//! agent.state.report ingestion: hook-edge fusion input and the ACK
//! matcher (amendment d dedupe, per-agent ACK capability tiers).
//!
//! Hooks are separate short-lived vendor processes posting through the
//! socket, so reports can arrive duplicated (Codex fires each event from
//! both config layers) and out of order. Dedupe keys on (session_id,
//! turn_id, event) where payloads carry them, plus the reporter's own seq
//! counter. Unmatched reports still feed fusion: a hook edge is a sensor
//! reading, and doubt triggers a reconcile.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};

use cyclops_proto::{AgentState, Sensor, SensorReading, StateReportParams, WireError};
use serde_json::{json, Value};
use tracing::debug;

use crate::{delivery, fusion, turnkey, unix_ms, Inner, PaneKey};

/// Dedupe memory per agent; old keys roll off.
const SEEN_CAP: usize = 256;
/// A replayed seq at or below this reads as a counter reset: the hook's
/// file counter restarts at 1 after garbage or deletion, so genuine resets
/// replay from the bottom of the range.
const RESET_SEQ_CEILING: u64 = 8;
/// Consecutive replayed below-max seqs before a reset is assumed anyway
/// (a counter that restarted from a partially-preserved file).
const RESET_REPLAY_STREAK: u32 = 3;

/// Per-agent dedupe state.
pub(crate) struct AckState {
    agents: StdMutex<HashMap<String, AgentSeen>>,
}

#[derive(Default)]
struct AgentSeen {
    seqs: HashSet<u64>,
    seq_order: VecDeque<u64>,
    /// Highest seq ever ingested. A replayed LOWER seq is only a counter
    /// reset when it is small or keeps repeating; a lone repost of an old
    /// seq is a duplicate.
    max_seq: u64,
    /// Consecutive replayed below-max seqs; a streak marks a reset.
    low_replays: u32,
    keys: HashSet<String>,
    key_order: VecDeque<String>,
}

impl AckState {
    pub(crate) fn new() -> AckState {
        AckState {
            agents: StdMutex::new(HashMap::new()),
        }
    }

    /// True when this (agent, seq) was already ingested.
    ///
    /// The hook's file counter restarts at 1 after garbage or deletion
    /// (cyclops/src/hook.rs), so a replayed below-max seq CAN mean a reset.
    /// But only a small seq (<= RESET_SEQ_CEILING) or a consecutive streak
    /// of below-max replays reads as one: an exact repost of a lone older
    /// out-of-order seq is a duplicate and must not wipe the dedupe window.
    /// Out-of-order arrival of a fresh seq stays legal, and an exact repost
    /// of the newest seq stays a duplicate. The (session_id, turn_id,
    /// event) key dedupe remains the backstop either way.
    fn seen_seq(&self, agent: &str, seq: u64) -> bool {
        let mut agents = self.agents.lock().expect("ack agents lock");
        let entry = agents.entry(agent.to_string()).or_default();
        if entry.seqs.contains(&seq) {
            let reset = seq < entry.max_seq && {
                entry.low_replays += 1;
                seq <= RESET_SEQ_CEILING || entry.low_replays >= RESET_REPLAY_STREAK
            };
            if !reset {
                return true;
            }
            entry.seqs.clear();
            entry.seq_order.clear();
            entry.max_seq = 0;
            entry.low_replays = 0;
        } else {
            entry.low_replays = 0;
        }
        entry.seqs.insert(seq);
        entry.seq_order.push_back(seq);
        entry.max_seq = entry.max_seq.max(seq);
        if entry.seq_order.len() > SEEN_CAP {
            if let Some(old) = entry.seq_order.pop_front() {
                entry.seqs.remove(&old);
            }
        }
        false
    }

    /// True when this (agent, session, turn, event) was already ingested.
    fn seen_key(&self, agent: &str, key: &str) -> bool {
        let mut agents = self.agents.lock().expect("ack agents lock");
        let entry = agents.entry(agent.to_string()).or_default();
        if !entry.keys.insert(key.to_string()) {
            return true;
        }
        entry.key_order.push_back(key.to_string());
        if entry.key_order.len() > SEEN_CAP {
            if let Some(old) = entry.key_order.pop_front() {
                entry.keys.remove(&old);
            }
        }
        false
    }
}

/// Vendor event names differ in casing conventions; comparisons happen on
/// this normalized form ("UserPromptSubmit" == "user_prompt_submit").
pub(crate) fn normalize_event(event: &str) -> String {
    // One normalizer, shared with the parser. Two copies of this would be
    // two definitions of "the same event", and the parser's refusals
    // would stop matching the runtime's comparisons.
    cyclops_manifest::normalize_event(event)
}

/// The rules this report was AUTHENTICATED under.
///
/// Never a fresh bind: re-deriving here would let an operator's pin, or a
/// stale watcher command, reinterpret an authenticated report under
/// another vendor's rules. The server proved which agent this is, and
/// these are the rules it proved it under.
fn manifest_of<'a>(
    inner: &'a Inner,
    origin: &crate::server::ReportOrigin,
) -> Option<&'a cyclops_manifest::Manifest> {
    origin
        .manifest
        .as_deref()
        .and_then(|id| inner.manifests.get(id))
}

/// (session_id, turn_id) from a vendor payload, tolerant of the observed
/// key spellings. The fallback route, for vendors that declare no turn
/// fields: it stringifies and joins, so it cannot tell the number 7 from
/// the string "7". That is acceptable only where the alternative is no
/// dedupe at all.
fn dedupe_ids(payload: &Value) -> Option<(String, String)> {
    let get = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            payload.get(*k).map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
        })
    };
    let session = get(&["session_id", "sessionId", "conversationId"])?;
    let turn = get(&["turn_id", "turnId", "invocationNum", "turn"])?;
    Some((session, turn))
}

/// Handle one agent.state.report. Never errors for unknown agents: hooks
/// must not break the agent CLI they run inside, so unresolvable reports
/// are acknowledged and dropped with a debug log.
///
/// A report does not need the tmux connection: while a session is detached
/// the pane is resolved against its last-known table, so hook ACKs stay
/// visible through a control-connection outage (the m1 soak lost a
/// delivery to exactly this blindness). Fusion recompute is skipped on
/// that path; the reading is stored and reconciles at reattach.
pub(crate) async fn handle_report(
    inner: &Arc<Inner>,
    params: StateReportParams,
    origin: crate::server::ReportOrigin,
) -> Result<Value, WireError> {
    let event = normalize_event(&params.event);

    // The origin was resolved from the socket peer during verification,
    // and it is what everything below uses. Re-resolving the request's own
    // name here would reopen the window it was verified to close: a pane
    // can change hands between the two lookups, and the second one would
    // answer for whoever holds it now.
    let recipient = origin.recipient.clone();
    let pane_id = origin.pane_id.clone();
    let session_idx = origin.session_idx;
    let Some((row, watcher)) = crate::server::report_route_row(inner, &origin) else {
        debug!(agent = %recipient, "state report for stale exact route dropped");
        return Ok(json!({"applied": false, "reason": "occupant_changed"}));
    };
    let pane = PaneKey::new(session_idx, &pane_id);
    // A report the daemon could not attribute to a process is not a
    // report about anyone: refused first, so an unplaceable origin is
    // never reported as somebody else having taken the pane.
    if origin.agent.pid == 0 {
        return Ok(json!({"applied": false, "reason": "unattributable_origin"}));
    }
    // The pane must still be held by the same agent process AND still be
    // read by the same rules. A pid alone is not the binding: a manifest
    // pin or a config change can reinterpret an old hook under new
    // semantics without the process ever changing. Exact equality, with
    // no zero-pid escape hatch, because "we could not tell" is not "it
    // matches".
    //
    // Both halves come from the same place the origin did: the agent
    // instance proven from the process tree, never the pane's pin and
    // never whoever currently holds the tty.
    let admitted = fusion::admitted_vendor(inner, session_idx, &row);
    if admitted.as_ref().map(|(_, proc)| *proc) != Some(origin.agent) {
        return Ok(json!({"applied": false, "reason": "occupant_changed"}));
    }
    if admitted.map(|(m, _)| m.agent.id.clone()) != origin.manifest {
        return Ok(json!({"applied": false, "reason": "manifest_changed"}));
    }

    // Hook liveness (amendment c): any report that resolves to a pane
    // proves its hook config is loaded and firing. Recorded before dedupe
    // on purpose: a duplicate is still a live edge. Keyed by the occupant
    // pid so a restarted occupant never inherits its predecessor's edges.

    // Dedupe windows belong to an occupant, not to a name: a label can
    // move between panes and a pane can change hands, and either would
    // otherwise let one process inherit another's counter.
    // The FULL identity, birth included. A dedupe window is a claim
    // about one process's counter, and a reused pid would otherwise
    // inherit its predecessor's window: the replacement's very first
    // report would be discarded as a duplicate of a report it never sent.
    // The manifest belongs in the namespace too. A process identity
    // survives an in-place exec, which this project explicitly supports
    // (a vendor launcher execs itself), so the same pid and birth can
    // change from one vendor's rules to another's. Without the manifest,
    // the new vendor's first report inherits its predecessor's sequence
    // and turn-key windows and can be discarded as a duplicate of
    // something it never sent.
    let dedupe_ns = format!(
        "{}#{}.{}@{}",
        origin.recipient_key,
        origin.agent.pid,
        origin.agent.birth,
        origin.manifest.as_deref().unwrap_or("-")
    );

    // One timestamp for this edge, taken once, so nothing downstream
    // re-reads a clock or a mutable slot that may already hold a later
    // edge.
    //
    // This edge arrived, whatever else is wrong with it. Wiring liveness
    // is recorded before anything can refuse the report, because it
    // answers a different question from the rest of this function: are
    // this pane's hooks wired and firing at all. A payload that names no
    // turn still proves that much, and it is the ONLY thing such a
    // payload is allowed to change.
    let edge_ms = unix_ms();
    inner
        .hook_liveness
        .record(&pane, &params.event, edge_ms, origin.agent);

    // Only the events that make up a turn have to name one. Turn fields
    // describe the turn lifecycle, not every hook a vendor emits: a
    // SessionStart or a tool-use edge legitimately carries no turn id,
    // and demanding one would refuse valid reports.
    let lifecycle_event = manifest_of(inner, &origin).is_some_and(|m| {
        [&m.hooks.turn_start, &m.hooks.turn_end, &m.hooks.ack]
            .iter()
            .filter_map(|n| n.as_deref())
            .any(|n| normalize_event(n) == event)
    });
    // Correlation before any window is touched. A malformed lifecycle
    // event is refused, and a refusal has to leave nothing behind:
    // consuming its sequence number would make the next VALID event
    // carrying that number look like a repost, so one bad payload could
    // swallow a real turn edge.
    let correlation = lifecycle_event
        .then(|| manifest_of(inner, &origin).map(|m| turnkey::correlate(m, &params.payload)))
        .flatten();
    if let Some(turnkey::TurnCorrelation::Invalid(why)) = correlation {
        debug!(pane = %pane_id, why, "lifecycle event names no turn; refused");
        return Ok(json!({"applied": false, "reason": "unnameable_turn"}));
    }

    // Dedupe: exact repost (same reporter seq), then cross-config
    // duplicates on the turn this event names plus the event itself.
    if let Some(seq) = params.seq {
        if inner.ack_state.seen_seq(&dedupe_ns, seq) {
            return Ok(json!({"applied": false, "duplicate": true}));
        }
    }
    // A vendor that names its turns is deduped over those typed values.
    // The legacy route stringifies each scalar and joins with a bar, so
    // it cannot tell the number 7 from the string "7", nor `["x|y", "z"]`
    // from `["x", "y|z"]`. That is acceptable only where the alternative
    // is no dedupe at all.
    let dupe_key = match &correlation {
        Some(turnkey::TurnCorrelation::Exact(turn)) => Some(turn.dedupe_key(&event)),
        _ => dedupe_ids(&params.payload).map(|(s, t)| format!("{s}|{t}|{event}")),
    };
    if let Some(key) = dupe_key {
        if inner.ack_state.seen_key(&dedupe_ns, key.as_str()) {
            return Ok(json!({"applied": false, "duplicate": true}));
        }
    }

    let manifest = manifest_of(inner, &origin);

    // ACK matching: the manifest hooks.ack event whose ack_payload_field
    // Fusion first, ACK second, and the order is load-bearing. Resolving
    // an ACK can complete a delivery, which wakes the next one for this
    // recipient, and that delivery gates on the fused state this very
    // reading feeds. Store the reading afterwards and the woken delivery
    // reads a pane that has not yet been told a turn started.
    //
    // The reading carries the binding it came from for the same reason
    // the ACK does: a hook is a fact about one occupant under one set of
    // rules, and fusion drops it when the pane no longer matches.
    let is_turn_start = manifest.is_some_and(|m| {
        m.hooks
            .turn_start
            .as_deref()
            .is_some_and(|n| normalize_event(n) == event)
    });
    let is_turn_end = manifest.is_some_and(|m| {
        m.hooks
            .turn_end
            .as_deref()
            .is_some_and(|n| normalize_event(n) == event)
    });
    let mapped = manifest.and_then(|m| {
        let is =
            |name: &Option<String>| name.as_deref().is_some_and(|n| normalize_event(n) == event);
        if is(&m.hooks.turn_start) || is(&m.hooks.ack) {
            Some(AgentState::Working)
        } else if is(&m.hooks.turn_end) {
            Some(AgentState::Idle)
        } else {
            None
        }
    });
    // A turn END is lifecycle evidence, stored where the runtime sensor's
    // eviction rules cannot reach it and matched by the turn it names
    // rather than by when it arrived. The composer hold may not consume
    // it for seconds, and a vendor still painting its working row when
    // the end arrives would otherwise have the record erased for
    // disagreeing with the screen three times running.
    //
    // The correlation is the one computed above, not a second call: two
    // reads of a payload invite drift between them. A vendor that names
    // no turns stores nothing, because its lifecycle is the screen, and a
    // malformed one never reaches here at all.
    if is_turn_end {
        if let (Some(turnkey::TurnCorrelation::Exact(turn)), Some(id)) =
            (&correlation, origin.manifest.as_deref())
        {
            turnkey::PaneEnds::record(
                &mut inner.turn_ends.lock().expect("turn ends lock"),
                &pane,
                origin.agent,
                id,
                turn.clone(),
            );
        }
    }
    // A START for a turn this pane has ALREADY seen the end of is not a
    // turn running. Hook reports arrive out of order, and a delayed start
    // that published `working` would leave the runtime saying so with no
    // later event to correct it: the turn is over, so nothing else is
    // coming. The hold would then wait forever on a clean composer it can
    // never be released against.
    //
    // Publishing nothing rather than `idle`: a stale start says nothing
    // about what the pane is doing NOW, and asserting idle would be a
    // second wrong answer. The end already stored is what releases the
    // hold, through the recompute below.
    let mapped = match (mapped, &correlation) {
        (Some(AgentState::Working), Some(turnkey::TurnCorrelation::Exact(turn))) => {
            let ended = origin.manifest.as_deref().is_some_and(|id| {
                turnkey::PaneEnds::holds(
                    &inner.turn_ends.lock().expect("turn ends lock"),
                    &pane,
                    origin.agent,
                    id,
                    turn,
                )
            });
            (!ended).then_some(AgentState::Working)
        }
        (m, _) => m,
    };
    if let Some(state) = mapped {
        inner
            .hook_readings
            .lock()
            .expect("hook readings lock")
            .insert(
                pane.clone(),
                fusion::HookEntry::bound(
                    origin.agent,
                    origin.manifest.clone(),
                    SensorReading {
                        sensor: Sensor::Hook,
                        state,
                        rule: params.event.clone(),
                        ts: edge_ms,
                    },
                ),
            );
    }
    if is_turn_start {
        if let Some(turnkey::TurnCorrelation::Exact(turn)) = &correlation {
            crate::composer_recovery::bind_post_recovery_turn(
                inner,
                origin.session_idx,
                &pane_id,
                turn.clone(),
                edge_ms,
            );
        }
    }
    // Resolve any waiting delivery whose own payload framing owns this
    // prompt. Not any prompt that mentions its id: see `prompt_names`.
    let mut matched = false;
    if let Some(m) = manifest {
        let is_ack = m
            .hooks
            .ack
            .as_deref()
            .is_some_and(|a| normalize_event(a) == event);
        if is_ack {
            if let Some(field) = &m.hooks.ack_payload_field {
                if let Some(text) = params.payload.get(field).and_then(Value::as_str) {
                    for handle in delivery::ack_candidates(inner, session_idx, &pane_id) {
                        if handle.claims_prompt(text) {
                            // The turn the vendor named in THIS payload,
                            // correlated once above. A delivery whose
                            // acknowledgement names its turn binds to it
                            // and leaves the screen lifecycle behind.
                            let turn = match &correlation {
                                Some(turnkey::TurnCorrelation::Exact(turn)) => Some(turn.clone()),
                                _ => None,
                            };
                            matched |= delivery::resolve_hook_ack(
                                inner,
                                &handle,
                                origin.agent,
                                &m.agent.id,
                                edge_ms,
                                turn,
                            );
                        }
                    }
                }
            }
        }
    }

    // Reconcile on the edge; the recompute emits the state event and the
    // ledger line if the fused verdict moved. Detached sessions have no
    // sensors to reconcile; the stored reading waits for reattach.
    let live = watcher.is_some();
    if let Some(w) = watcher {
        fusion::recompute_pane(inner, session_idx, &w, &pane_id, false, "hook_report").await;
    }

    Ok(json!({"applied": true, "matched": matched, "state": mapped, "live": live}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_normalize_across_vendor_spellings() {
        assert_eq!(normalize_event("UserPromptSubmit"), "userpromptsubmit");
        assert_eq!(normalize_event("user_prompt_submit"), "userpromptsubmit");
        assert_eq!(normalize_event("user-prompt-submit"), "userpromptsubmit");
        assert_eq!(normalize_event("Stop"), "stop");
    }

    #[test]
    fn dedupe_ids_tolerate_vendor_spellings() {
        let codex = json!({"session_id": "s1", "turn_id": "t1"});
        assert_eq!(dedupe_ids(&codex), Some(("s1".into(), "t1".into())));
        let agy = json!({"conversationId": "c9", "invocationNum": 3});
        assert_eq!(dedupe_ids(&agy), Some(("c9".into(), "3".into())));
        assert_eq!(dedupe_ids(&json!({"session_id": "s"})), None);
        assert_eq!(dedupe_ids(&json!({})), None);
    }

    #[test]
    fn seen_key_dedupes_and_rolls_off() {
        let st = AckState::new();
        assert!(!st.seen_key("codex", "s|t|userpromptsubmit"));
        assert!(st.seen_key("codex", "s|t|userpromptsubmit"));
        // Different agent namespaces do not collide.
        assert!(!st.seen_key("claude", "s|t|userpromptsubmit"));
        // Rolls off after SEEN_CAP fresh keys.
        for i in 0..SEEN_CAP {
            assert!(!st.seen_key("codex", &format!("k{i}")));
        }
        assert!(!st.seen_key("codex", "s|t|userpromptsubmit"));
    }

    #[test]
    fn seen_seq_dedupes_out_of_order_arrivals() {
        let st = AckState::new();
        assert!(!st.seen_seq("a", 5));
        assert!(!st.seen_seq("a", 3)); // out of order is fine
        assert!(st.seen_seq("a", 5)); // exact repost is not
    }

    #[test]
    fn seq_counter_reset_clears_the_window() {
        let st = AckState::new();
        for i in 1..=5 {
            assert!(!st.seen_seq("a", i));
        }
        // hookseq file lost: the hook restarts at 1. The replayed low seq
        // is a reset, not a duplicate, and the window starts over.
        assert!(
            !st.seen_seq("a", 1),
            "reset seq 1 must not read as duplicate"
        );
        assert!(
            !st.seen_seq("a", 2),
            "post-reset seq 2 must not read as duplicate"
        );
        // Exact repost after the reset still dedupes.
        assert!(st.seen_seq("a", 2));
    }

    #[test]
    fn lone_out_of_order_repost_is_duplicate_not_reset() {
        let st = AckState::new();
        for i in 1..=100 {
            assert!(!st.seen_seq("a", i));
        }
        // An exact repost of an old-but-not-small seq is a duplicate.
        // Before the fix this read as a counter reset and wiped the
        // window, so the next reposts sailed through as fresh.
        assert!(st.seen_seq("a", 50), "repost of 50 must be a duplicate");
        assert!(st.seen_seq("a", 99), "window must survive the repost");
        assert!(st.seen_seq("a", 100), "newest repost stays a duplicate");
        // A fresh seq still ingests normally.
        assert!(!st.seen_seq("a", 101));
        // A genuinely small replayed seq is still a reset.
        assert!(!st.seen_seq("a", 1), "small replay is a real reset");
    }

    #[test]
    fn consecutive_low_replays_read_as_reset() {
        let st = AckState::new();
        for i in 1..=100 {
            assert!(!st.seen_seq("a", i));
        }
        // A hook that restarted from a partially-preserved counter replays
        // from the middle of the range: the streak marks the reset.
        assert!(st.seen_seq("a", 40), "first low replay is a duplicate");
        assert!(st.seen_seq("a", 41), "second low replay is a duplicate");
        assert!(
            !st.seen_seq("a", 42),
            "third consecutive low replay is a reset"
        );
        // The window restarted: the old high seqs are fresh again.
        assert!(!st.seen_seq("a", 43));
    }
}
