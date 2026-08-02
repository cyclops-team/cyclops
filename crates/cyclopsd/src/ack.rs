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

/// Per-agent dedupe state.
pub(crate) struct AckState {
    agents: StdMutex<HashMap<String, AgentSeen>>,
}

#[derive(Default)]
struct AgentSeen {
    seqs: HashSet<u64>,
    seq_order: VecDeque<u64>,
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
    fn seen_seq(&self, agent: &str, seq: u64) -> bool {
        let mut agents = self.agents.lock().expect("ack agents lock");
        let entry = agents.entry(agent.to_string()).or_default();
        if !entry.seqs.insert(seq) {
            return true;
        }
        entry.seq_order.push_back(seq);
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
pub(crate) async fn handle_report(
    inner: &Arc<Inner>,
    params: StateReportParams,
) -> Result<Value, WireError> {
    let event = normalize_event(&params.event);

    let Some((session_idx, pane_id)) = inner.resolve_recipient(&params.agent) else {
        debug!(agent = %params.agent, "state report for unknown agent dropped");
        return Ok(json!({"applied": false, "reason": "unknown_agent"}));
    };

    // Dedupe: exact repost (same reporter seq), then cross-config
    // duplicates on (session_id, turn_id, event) (amendment d).
    if let Some(seq) = params.seq {
        if inner.ack_state.seen_seq(&params.agent, seq) {
            return Ok(json!({"applied": false, "duplicate": true}));
        }
    }
    if let Some((s, t)) = dedupe_ids(&params.payload) {
        let key = format!("{s}|{t}|{event}");
        if inner.ack_state.seen_key(&params.agent, key.as_str()) {
            return Ok(json!({"applied": false, "duplicate": true}));
        }
    }

    let Some(watcher) = inner.watcher_of(session_idx) else {
        return Ok(json!({"applied": false, "reason": "session_detached"}));
    };
    let Some(row) = watcher.pane(&pane_id) else {
        return Ok(json!({"applied": false, "reason": "no_such_pane"}));
    };
    let manifest = fusion::bind_manifest(&inner.manifests, &row.current_command);

    // ACK matching: the manifest hooks.ack event whose ack_payload_field
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
                            matched |= delivery::resolve_hook_ack(inner, &handle);
                        }
                    }
                }
            }
        }
    }

    // Fusion: a hook edge is a sensor reading. Only the turn-boundary
    // events map to states; everything else is a reconcile trigger.
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
                SensorReading {
                    sensor: Sensor::Hook,
                    state,
                    rule: params.event.clone(),
                    ts: unix_ms(),
                },
            );
    }
    // Reconcile on the edge; the recompute emits the state event and the
    // ledger line if the fused verdict moved.
    fusion::recompute_pane(inner, &watcher, &pane_id, false, "hook_report").await;

    Ok(json!({"applied": true, "matched": matched, "state": mapped}))
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
}
