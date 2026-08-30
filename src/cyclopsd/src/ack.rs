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

use cyclops_manifest::{AckEvidence, LifecycleCertainty, LifecycleRole};
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
/// May this report publish an admission-eligible liveness edge?
///
/// `SessionStart` qualifies when the exact manifest declares it available:
/// it says the agent process is up with its hooks wired and no turn has
/// been asked of it. `UserPromptSubmit` qualifies only when this exact
/// manifest configures it as a lifecycle start AND the report path installed
/// or retained an active start for the exact binding, so the edge can never
/// be seen without its start. A manifest that merely lists
/// `UserPromptSubmit` as available never publishes it: with no start
/// installed, a later clean screen would otherwise read idle during the
/// submitted turn. Every other event is refused here and again by the store.
pub(crate) fn admitting_edge_qualifies(
    m: &cyclops_manifest::Manifest,
    event: &str,
    active_start_installed: bool,
) -> bool {
    let available = m
        .hooks
        .available
        .iter()
        .any(|name| normalize_event(name) == event);
    if event == normalize_event("SessionStart") {
        return available;
    }
    if event == normalize_event("UserPromptSubmit") {
        let configured_start = matches!(
            m.hooks.lifecycle_event(event),
            Some((LifecycleRole::Start, _))
        );
        return available && configured_start && active_start_installed;
    }
    false
}

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
    // Bound diagnostic edge, captured BEFORE sequence dedupe or any lifecycle
    // mutation. A route that is not open yet cannot record or publish
    // anything; the report is answered retryable with no state changed, and
    // because dedupe has not run the retry can still publish. Reports never
    // open routes.
    let bound = match origin.manifest.as_deref() {
        Some(manifest_id) => match inner.hook_liveness.bind_diagnostic(
            &pane,
            &params.event,
            edge_ms,
            origin.agent,
            manifest_id,
        ) {
            Ok(binding) => Some(binding),
            Err(crate::selftest::RouteNotOpen) => {
                return Ok(json!({
                    "applied": false,
                    "reason": "hook_route_not_ready",
                    "retryable": true
                }));
            }
        },
        None => None,
    };

    // Only the events that make up a turn have to name one. Turn fields
    // describe the turn lifecycle, not every hook a vendor emits: a
    // SessionStart or a tool-use edge legitimately carries no turn id,
    // and demanding one would refuse valid reports.
    let lifecycle = manifest_of(inner, &origin).and_then(|m| m.hooks.lifecycle_event(&event));
    let is_ack = manifest_of(inner, &origin).is_some_and(|m| {
        m.hooks
            .ack
            .as_deref()
            .is_some_and(|name| normalize_event(name) == event)
    });
    let lifecycle_event = lifecycle.is_some() || is_ack;
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
    // Candidate terminal hooks may legitimately fire more than once for one
    // turn. A vendor can run matching end hooks concurrently, block one, then
    // retry the same turn. The lifecycle store makes simultaneous candidates
    // idempotent and retires a blocked candidate on later Working evidence. A
    // permanent turn/event key would discard the later real attempt. Reporter
    // sequence numbers still reject exact transport replays above.
    let dupe_key = if matches!(lifecycle, Some((_, LifecycleCertainty::Candidate))) {
        None
    } else {
        match &correlation {
            Some(turnkey::TurnCorrelation::Exact(turn)) => Some(turn.dedupe_key(&event)),
            _ => dedupe_ids(&params.payload).map(|(s, t)| format!("{s}|{t}|{event}")),
        }
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
    let is_turn_start = matches!(lifecycle, Some((LifecycleRole::Start, _)));
    let is_turn_end = matches!(lifecycle, Some((LifecycleRole::End, _)));
    let lifecycle_confirmed = matches!(lifecycle, Some((_, LifecycleCertainty::Confirmed)));
    let exact_turn = match &correlation {
        Some(turnkey::TurnCorrelation::Exact(turn)) => Some(turn.clone()),
        _ => None,
    };
    let unkeyed_dispatch_start = is_turn_start
        && matches!(
            lifecycle,
            Some((LifecycleRole::Start, LifecycleCertainty::Candidate))
        )
        && exact_turn.is_none()
        && is_ack
        && manifest.is_some_and(|m| m.hooks.ack_evidence == AckEvidence::Dispatch);
    if let (Some((role, LifecycleCertainty::Candidate)), Some(turn), Some(id)) =
        (lifecycle, exact_turn.clone(), origin.manifest.as_deref())
    {
        let settle_ms = manifest
            .map(|m| m.hooks.turn_end_settle_ms)
            .unwrap_or_default();
        let mut candidates = inner.hook_lifecycle.lock().expect("hook lifecycle lock");
        let end_candidate = match role {
            LifecycleRole::Start => {
                candidates.record_start(&pane, origin.agent, id, turn, &params.event, edge_ms);
                None
            }
            LifecycleRole::End => Some(candidates.record_end(
                &pane,
                origin.agent,
                id,
                turn,
                &params.event,
                edge_ms,
                settle_ms,
            )),
        };
        drop(candidates);
        if let Some(candidate) = end_candidate {
            fusion::schedule_candidate_end_recheck(inner, &pane, candidate);
        }
    }
    // A conclusive end for the same exact prompt proves that the prompt was
    // accepted and a turn existed. Candidate Stop remains neutral until a
    // later settled visual observation confirms it.
    let start_confirmed_by_end = if is_turn_end && lifecycle_confirmed {
        exact_turn.as_ref().and_then(|turn| {
            origin.manifest.as_deref().and_then(|id| {
                inner
                    .hook_lifecycle
                    .lock()
                    .expect("hook lifecycle lock")
                    .take_start_for_turn(&pane, origin.agent, id, turn)
            })
        })
    } else {
        None
    };
    let mapped = match lifecycle {
        Some((LifecycleRole::Start, LifecycleCertainty::Confirmed)) => Some(AgentState::Working),
        Some((LifecycleRole::End, LifecycleCertainty::Confirmed)) => Some(AgentState::Idle),
        _ if unkeyed_dispatch_start => Some(AgentState::Working),
        None if is_ack
            && manifest.is_some_and(|m| m.hooks.ack_evidence == AckEvidence::Receipt) =>
        {
            Some(AgentState::Working)
        }
        _ => None,
    };
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
    if is_turn_end && lifecycle_confirmed {
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
            let mut lifecycle = inner.hook_lifecycle.lock().expect("hook lifecycle lock");
            lifecycle.clear_end(&pane, turn);
            lifecycle.clear_visual_end(&pane, origin.agent, id, turn);
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
    let mut applied_state = mapped;
    let mut replaced_provisional_edge = None;
    if let Some(state) = mapped {
        let mut readings = inner.hook_readings.lock().expect("hook readings lock");
        let active_start = readings.get(&pane).is_some_and(|current| {
            current.active_start_for(origin.agent, origin.manifest.as_deref())
        });
        let matching_end = readings
            .get(&pane)
            .is_some_and(|current| match &correlation {
                // A confirmed exact end on this binding also ends a persistent
                // unkeyed start: the start had no key to match, and the end is
                // separately proven on the same agent generation.
                Some(turnkey::TurnCorrelation::Exact(turn)) => {
                    current.active_start_matches(
                        origin.agent,
                        origin.manifest.as_deref(),
                        Some(turn),
                    ) || current.unkeyed_latch_ended_by(
                        origin.agent,
                        origin.manifest.as_deref(),
                        edge_ms,
                    )
                }
                Some(turnkey::TurnCorrelation::Unconfigured) => {
                    current.confirmed_unkeyed_start_for(origin.agent, origin.manifest.as_deref())
                }
                Some(turnkey::TurnCorrelation::Invalid(_)) | None => false,
            });
        let conflicting_active = readings.get(&pane).is_some_and(|current| {
            current.active_start_for(origin.agent, origin.manifest.as_deref()) && !matching_end
        });
        let keyed_confirmed_end = is_turn_end && lifecycle_confirmed && exact_turn.is_some();
        let should_insert = if keyed_confirmed_end {
            matching_end || (start_confirmed_by_end.is_some() && !conflicting_active)
        } else {
            (is_turn_start && lifecycle_confirmed)
                || unkeyed_dispatch_start
                || !active_start
                || (is_turn_end && lifecycle_confirmed && matching_end)
        };
        // A receipt hook can be separate from the lifecycle hook. It may add
        // receipt evidence, but it cannot downgrade a live start into a
        // transient Working sample that later expires without an end.
        if should_insert {
            let reading = SensorReading {
                sensor: Sensor::Hook,
                state,
                rule: params.event.clone(),
                ts: edge_ms,
            };
            let entry = if unkeyed_dispatch_start {
                fusion::HookEntry::provisional_start(origin.agent, origin.manifest.clone(), reading)
            } else if is_turn_start && lifecycle_confirmed && exact_turn.is_some() {
                // A confirmed start owns runtime state until matching end
                // evidence or binding retirement replaces it.
                fusion::HookEntry::turn_started(
                    origin.agent,
                    origin.manifest.clone(),
                    reading,
                    exact_turn
                        .clone()
                        .expect("confirmed start has an exact turn"),
                )
            } else if is_turn_start && lifecycle_confirmed {
                fusion::HookEntry::unkeyed_turn_started(
                    origin.agent,
                    origin.manifest.clone(),
                    reading,
                )
            } else if keyed_confirmed_end {
                fusion::HookEntry::turn_ended(
                    origin.agent,
                    origin.manifest.clone(),
                    reading,
                    exact_turn.clone().expect("keyed end has a turn"),
                )
            } else {
                fusion::HookEntry::bound(origin.agent, origin.manifest.clone(), reading)
            };
            let replaced = readings.insert(pane.clone(), entry);
            replaced_provisional_edge = replaced.and_then(|entry| {
                entry.provisional_edge_for(origin.agent, origin.manifest.as_deref())
            });
        } else {
            applied_state = None;
        }
        drop(readings);
    }
    if let (Some(previous_edge), Some(manifest_id)) =
        (replaced_provisional_edge, origin.manifest.as_deref())
    {
        delivery::reject_unkeyed_dispatch_ack(
            inner,
            session_idx,
            &pane_id,
            origin.agent,
            manifest_id,
            previous_edge,
            "hook_dispatch_superseded",
        );
    }
    // Admission-eligible publication comes after the manifest declared this
    // event and after any start it carries has been installed above, and
    // before the recompute below is scheduled: one causal order, so a
    // recompute can never see an eligible edge without its active start.
    if let (Some(m), Some(manifest_id)) = (manifest_of(inner, &origin), origin.manifest.as_deref())
    {
        let active_start_installed = inner
            .hook_readings
            .lock()
            .expect("hook readings lock")
            .get(&pane)
            .is_some_and(|current| current.active_start_for(origin.agent, Some(manifest_id)));
        if admitting_edge_qualifies(m, &event, active_start_installed) {
            if let Some(binding) = &bound {
                // The captured lifetime must still be live: a route closed and
                // reopened since the diagnostic edge belongs to a replacement
                // occupant, which inherits nothing and is not retried against.
                if inner
                    .hook_liveness
                    .publish_admission(binding, &params.event)
                    .is_err()
                {
                    return Ok(json!({"applied": false, "reason": "occupant_changed"}));
                }
            }
        }
    }
    if unkeyed_dispatch_start && applied_state.is_some() {
        fusion::schedule_unkeyed_dispatch_recheck(inner, &pane);
    }
    // A candidate end (Claude's Stop can continue through additionalContext)
    // may trigger a fresh look at the screen but never clears a persistent
    // start on its own; the screen's lifecycle evidence does that.
    if is_turn_end && !lifecycle_confirmed {
        fusion::schedule_lifecycle_recheck(inner, &pane);
    }
    if is_turn_end && lifecycle_confirmed {
        if let (Some(turn), Some(manifest)) = (exact_turn.as_ref(), origin.manifest.as_deref()) {
            delivery::prepare_dispatch_ack(
                inner,
                origin.session_idx,
                &pane_id,
                origin.agent,
                manifest,
                turn,
            );
        }
    }
    if let Some(start) = &start_confirmed_by_end {
        crate::composer_recovery::bind_post_recovery_turn(
            inner,
            origin.session_idx,
            &pane_id,
            start.turn.clone(),
            start.edge_ms,
        );
    }
    if is_turn_start && lifecycle_confirmed {
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
    let mut dispatch_already_ended = None;
    if let Some(m) = manifest {
        if is_ack {
            if let Some(field) = &m.hooks.ack_payload_field {
                if let Some(text) = params.payload.get(field).and_then(Value::as_str) {
                    if m.hooks.ack_evidence == AckEvidence::Receipt {
                        if let Some(messaging) = inner.workspace_messaging() {
                            matched |= messaging.attention_consumption_observed(
                                crate::messaging::MessagingAttentionConsumptionObservation::new(
                                    session_idx,
                                    &pane_id,
                                    origin.recipient_key,
                                    origin.pane_root,
                                    origin.agent,
                                    &m.agent.id,
                                    text,
                                    edge_ms,
                                ),
                            );
                        }
                    }
                    let matching: Vec<_> = delivery::ack_candidates(inner, session_idx, &pane_id)
                        .into_iter()
                        .filter(|handle| handle.claims_prompt(text))
                        .collect();
                    let unique_unkeyed_dispatch = !unkeyed_dispatch_start
                        || m.hooks.ack_evidence != AckEvidence::Dispatch
                        || matching.len() == 1;
                    for handle in matching {
                        if !unique_unkeyed_dispatch {
                            delivery::mark_dispatch_match_ambiguous(
                                &handle,
                                origin.agent,
                                &m.agent.id,
                                "hook_dispatch_ambiguous",
                            );
                            continue;
                        }
                        // The turn the vendor named in THIS payload,
                        // correlated once above. A delivery whose
                        // acknowledgement names its turn binds to it
                        // and leaves the screen lifecycle behind.
                        let turn = match &correlation {
                            Some(turnkey::TurnCorrelation::Exact(turn)) => Some(turn.clone()),
                            _ => None,
                        };
                        matched |= match m.hooks.ack_evidence {
                            AckEvidence::Receipt => delivery::resolve_hook_ack(
                                inner,
                                &handle,
                                origin.agent,
                                &m.agent.id,
                                edge_ms,
                                turn,
                            ),
                            AckEvidence::Dispatch => {
                                let recorded = delivery::record_dispatch_candidate(
                                    &handle,
                                    origin.agent,
                                    &m.agent.id,
                                    edge_ms,
                                    turn.clone(),
                                );
                                if recorded {
                                    if let Some(turn) = turn.as_ref() {
                                        let preparation = delivery::prepare_dispatch_ack(
                                            inner,
                                            session_idx,
                                            &pane_id,
                                            origin.agent,
                                            &m.agent.id,
                                            turn,
                                        );
                                        if preparation.end_already_present {
                                            inner
                                                .hook_lifecycle
                                                .lock()
                                                .expect("hook lifecycle lock")
                                                .take_start_for_turn(
                                                    &pane,
                                                    origin.agent,
                                                    &m.agent.id,
                                                    turn,
                                                );
                                            dispatch_already_ended = Some(turn.clone());
                                        }
                                    }
                                }
                                recorded
                            }
                        };
                    }
                }
            }
        }
    }

    // Reconcile on the authenticated edge. It is causal route evidence even
    // when the fused verdict stays put: a pre-write block can be appended
    // after this recompute, and the later generation is what reopens that
    // exact attempt. Detached sessions have no sensors to reconcile; their
    // stored reading waits for reattach.
    let live = watcher.is_some();
    if let Some(w) = watcher {
        let route_evidence = inner.advance_route_evidence(session_idx, &pane_id);
        crate::observe_pane_for_route_evidence(
            inner,
            session_idx,
            &w,
            &pane_id,
            is_turn_end && lifecycle_confirmed,
            "hook_report",
            &route_evidence,
        )
        .await;
        if let Some(messaging) = inner.workspace_messaging() {
            messaging.route_evidence_observed(crate::messaging::MessagingRouteEvidence::new(
                session_idx,
                &pane_id,
                route_evidence,
            ));
        }
    }
    if is_turn_end && lifecycle_confirmed {
        if let (Some(turn), Some(manifest)) = (exact_turn.as_ref(), origin.manifest.as_deref()) {
            delivery::confirm_dispatch_ack(
                inner,
                origin.session_idx,
                &pane_id,
                origin.agent,
                manifest,
                turn,
                edge_ms,
            );
        }
    }
    if let (Some(turn), Some(manifest)) =
        (dispatch_already_ended.as_ref(), origin.manifest.as_deref())
    {
        delivery::confirm_dispatch_ack(
            inner,
            origin.session_idx,
            &pane_id,
            origin.agent,
            manifest,
            turn,
            edge_ms,
        );
    }

    Ok(json!({"applied": true, "matched": matched, "state": applied_state, "live": live}))
}

#[cfg(test)]
mod tests {
    /// An available-only UserPromptSubmit never qualifies: the manifest must
    /// configure it as a lifecycle start and the report path must have
    /// installed the active start. SessionStart qualifies by declaration.
    #[test]
    fn available_only_user_prompt_submit_never_enters_the_admission_store() {
        let with_start = cyclops_manifest::Manifest::parse(
            r#"
[agent]
id = "hooked"
display_name = "Hooked fixture"
process_names = ["hooked"]

[hooks]
config_mechanism = "test"
available = ["SessionStart", "UserPromptSubmit", "Stop"]
turn_start = "UserPromptSubmit"
turn_start_evidence = "candidate"
ack = "UserPromptSubmit"
ack_evidence = "dispatch"
ack_payload_field = "prompt"
"#,
            std::path::Path::new("hooked.toml"),
        )
        .unwrap();
        let available_only = cyclops_manifest::Manifest::parse(
            r#"
[agent]
id = "listed"
display_name = "Listed fixture"
process_names = ["listed"]

[hooks]
config_mechanism = "test"
available = ["SessionStart", "UserPromptSubmit", "Stop"]
"#,
            std::path::Path::new("listed.toml"),
        )
        .unwrap();
        let prompt = super::normalize_event("UserPromptSubmit");
        let session = super::normalize_event("SessionStart");
        let stop = super::normalize_event("Stop");
        assert!(super::admitting_edge_qualifies(&with_start, &prompt, true));
        assert!(
            !super::admitting_edge_qualifies(&with_start, &prompt, false),
            "start not installed"
        );
        assert!(
            !super::admitting_edge_qualifies(&available_only, &prompt, true),
            "available-only"
        );
        assert!(!super::admitting_edge_qualifies(
            &available_only,
            &prompt,
            false
        ));
        assert!(super::admitting_edge_qualifies(
            &with_start,
            &session,
            false
        ));
        assert!(super::admitting_edge_qualifies(
            &available_only,
            &session,
            false
        ));
        assert!(!super::admitting_edge_qualifies(&with_start, &stop, true));
        let liveness = crate::selftest::HookLiveness::new();
        let pane = crate::PaneKey::new(0, "%1");
        let agent = crate::identity::ProcId { pid: 7, birth: 70 };
        liveness.open(&pane);
        let binding = liveness
            .bind_diagnostic(&pane, "UserPromptSubmit", 1, agent, "listed")
            .expect("route open");
        if super::admitting_edge_qualifies(&available_only, &prompt, false) {
            liveness
                .publish_admission(&binding, "UserPromptSubmit")
                .expect("lifetime live");
        }
        assert!(!liveness.seen_admitting_edge(&pane, agent, "listed"));
    }
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
