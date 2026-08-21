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

use crate::{delivery, fusion, unix_ms, Inner};

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
    event
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// (session_id, turn_id) from a vendor payload, tolerant of the three
/// observed key spellings. Both must be present for amendment-d dedupe.
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
    let (row, watcher, session_idx) = match inner.resolve_recipient(&pane_id) {
        Some((idx, id)) => {
            let Some(watcher) = inner.watcher_of(idx) else {
                // resolve_recipient only answers from live watchers; a
                // detach between the two calls lands on the fallback.
                return Ok(json!({"applied": false, "reason": "session_detached"}));
            };
            let Some(row) = watcher.pane(&id) else {
                return Ok(json!({"applied": false, "reason": "no_such_pane"}));
            };
            (row, Some(watcher), idx)
        }
        None => match inner.resolve_recipient_last_known(&pane_id) {
            Some((idx, row)) => (row, None, idx),
            None => {
                debug!(agent = %recipient, "state report for unknown agent dropped");
                return Ok(json!({"applied": false, "reason": "unknown_agent"}));
            }
        },
    };
    // A report the daemon could not attribute to a process is not a
    // report about anyone: refused first, so an unplaceable origin is
    // never reported as somebody else having taken the pane.
    if origin.pane_pid == 0 {
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
    let admitted = fusion::admitted_vendor(inner, &row);
    if admitted.as_ref().map(|(_, pid)| *pid) != Some(origin.pane_pid) {
        return Ok(json!({"applied": false, "reason": "occupant_changed"}));
    }
    if admitted.map(|(m, _)| m.agent.id.clone()) != origin.manifest {
        return Ok(json!({"applied": false, "reason": "manifest_changed"}));
    }

    // Hook liveness (amendment c): any report that resolves to a pane
    // proves its hook config is loaded and firing. Recorded before dedupe
    // on purpose: a duplicate is still a live edge. Keyed by the occupant
    // pid so a restarted occupant never inherits its predecessor's edges.
    inner
        .hook_liveness
        .record(&pane_id, &params.event, unix_ms(), origin.pane_pid);

    // Dedupe windows belong to an occupant, not to a name: a label can
    // move between panes and a pane can change hands, and either would
    // otherwise let one process inherit another's counter.
    let dedupe_ns = format!("{}#{}", origin.pane_id, origin.pane_pid);

    // Dedupe: exact repost (same reporter seq), then cross-config
    // duplicates on (session_id, turn_id, event) (amendment d).
    if let Some(seq) = params.seq {
        if inner.ack_state.seen_seq(&dedupe_ns, seq) {
            return Ok(json!({"applied": false, "duplicate": true}));
        }
    }
    if let Some((s, t)) = dedupe_ids(&params.payload) {
        let key = format!("{s}|{t}|{event}");
        if inner.ack_state.seen_key(&dedupe_ns, key.as_str()) {
            return Ok(json!({"applied": false, "duplicate": true}));
        }
    }

    let manifest = fusion::bind_manifest_for(inner, &row);

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
    if let Some(state) = mapped {
        inner
            .hook_readings
            .lock()
            .expect("hook readings lock")
            .insert(
                pane_id.clone(),
                fusion::HookEntry::bound(
                    origin.pane_pid,
                    origin.manifest.clone(),
                    SensorReading {
                        sensor: Sensor::Hook,
                        state,
                        rule: params.event.clone(),
                        ts: unix_ms(),
                    },
                ),
            );
    }
    // contains a waiting delivery's message id.
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
                    for handle in delivery::ack_candidates(inner, &pane_id) {
                        if text.contains(&handle.msg_id) {
                            matched |= delivery::resolve_hook_ack(
                                inner,
                                &handle,
                                origin.pane_pid,
                                &m.agent.id,
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
