//! Structural turn correlation: which turn a hook event names.
//!
//! A start and an end are the same turn when the manifest's declared
//! fields carry the same values, positionally and by type. Nothing here
//! compares timestamps: an end can be observed before the start it
//! belongs to, and a stringified or delimiter-joined key would make two
//! different turns look like one.

use cyclops_manifest::Manifest;
use serde_json::Value;

/// One field's value inside a turn key.
///
/// Strings only, which is what every vendor that can correlate turns
/// actually sends. Widening this needs a manifest that consumes it: a
/// number accepted here would have to answer whether the JSON string
/// "1" and the number 1 are one turn, and the safe answer costs nothing
/// while nobody sends numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum KeyField {
    Text(String),
}

/// The values a manifest's declared fields carry for one turn, in
/// declared order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TurnKey(Vec<KeyField>);

impl TurnKey {
    /// A dedupe key that cannot be forged by a value containing the
    /// separator: every part carries its own length, so `["x|y", "z"]`
    /// and `["x", "y|z"]` produce different strings.
    pub(crate) fn dedupe_key(&self, event: &str) -> String {
        let mut out = String::new();
        for KeyField::Text(part) in &self.0 {
            out.push_str(&part.len().to_string());
            out.push(':');
            out.push_str(part);
        }
        out.push_str(&format!("{}:{event}", event.len()));
        out
    }

    /// Build a key from field values directly, for tests that need a
    /// specific turn without going through a manifest and a payload.
    #[cfg(test)]
    pub(crate) fn for_test(parts: &[&str]) -> TurnKey {
        TurnKey(
            parts
                .iter()
                .map(|p| KeyField::Text(p.to_string()))
                .collect(),
        )
    }
}

/// What a payload says about which turn it belongs to.
///
/// Three answers, not two. A manifest that declares no fields selects a
/// different LIFECYCLE, which is a capability statement. A manifest that
/// declares fields and gets a payload that does not satisfy them has a
/// malformed event, which is a refusal. Collapsing the two would let a
/// broken or hostile payload drop a vendor onto screen evidence, and the
/// screen lane releases holds the exact lane would keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnCorrelation {
    /// This vendor never correlates turns. Screen lifecycle.
    Unconfigured,
    /// Every declared field named a value.
    Exact(TurnKey),
    /// Fields are declared and this payload did not satisfy them. The
    /// event is refused; it never selects another lane.
    Invalid(&'static str),
}

/// Which turn this payload names, under this manifest's declaration.
pub(crate) fn correlate(m: &Manifest, payload: &Value) -> TurnCorrelation {
    let fields = &m.hooks.turn_key_fields;
    if fields.is_empty() {
        return TurnCorrelation::Unconfigured;
    }
    let mut parts = Vec::with_capacity(fields.len());
    for name in fields {
        let Some(v) = payload.get(name) else {
            return TurnCorrelation::Invalid("missing field");
        };
        match key_field(v) {
            Some(part) => parts.push(part),
            None => return TurnCorrelation::Invalid(unsupported(v)),
        }
    }
    TurnCorrelation::Exact(TurnKey(parts))
}

/// Why this value cannot name a turn.
fn unsupported(v: &Value) -> &'static str {
    match v {
        Value::Null => "null field",
        Value::Object(_) => "object field",
        Value::Array(_) => "array field",
        Value::String(_) => "blank field",
        _ => "non-string field",
    }
}

/// One JSON value as a key field, or None when it cannot name a turn.
fn key_field(v: &Value) -> Option<KeyField> {
    match v {
        // A blank value names no turn, and every malformed event carrying
        // one would correlate with every other: an empty key is the
        // widest possible collision.
        Value::String(s) if !s.trim().is_empty() => Some(KeyField::Text(s.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn manifest(fields: &str) -> Manifest {
        let src = format!(
            "[agent]\nid = \"t\"\ndisplay_name = \"t\"\n\n[hooks]\n\
             turn_start = \"Start\"\nturn_start_evidence = \"confirmed\"\n\
             turn_end = \"Stop\"\nturn_end_evidence = \"confirmed\"\n{fields}"
        );
        Manifest::parse(&src, Path::new("t.toml")).expect("fixture manifest")
    }

    /// A vendor that declares nothing selects the screen lifecycle. That
    /// is a capability statement and the only thing that may select it.
    #[test]
    fn an_undeclared_key_is_a_lane_not_a_failure() {
        let m = manifest("");
        assert_eq!(
            correlate(&m, &json!({"turn_id": "t1"})),
            TurnCorrelation::Unconfigured
        );
    }

    /// Once fields are declared, a payload that does not satisfy them is
    /// REFUSED. It must not read as "no correlation available", because
    /// that is the screen lane and the screen lane releases holds this
    /// one would keep.
    #[test]
    fn a_declared_key_that_cannot_be_read_refuses() {
        let m = manifest("turn_key_fields = [\"session_id\", \"turn_id\"]\n");
        for payload in [
            json!({"session_id": "s1"}),
            json!({"session_id": "s1", "turn_id": null}),
            json!({"session_id": "s1", "turn_id": {"a": 1}}),
            json!({"session_id": "s1", "turn_id": [1]}),
            json!({"session_id": "s1", "turn_id": 1.5}),
            json!({"session_id": "s1", "turn_id": 7}),
            json!({"session_id": "s1", "turn_id": ""}),
            json!({"session_id": "s1", "turn_id": "   "}),
        ] {
            assert!(
                matches!(correlate(&m, &payload), TurnCorrelation::Invalid(_)),
                "{payload} did not refuse"
            );
        }
    }

    /// Position and separator both carry meaning: an order-independent
    /// key would make the first pair one turn, and a delimiter-joined one
    /// would make the second pair one turn. Type refusal is covered by
    /// `a_declared_key_that_cannot_be_read_refuses`.
    #[test]
    fn keys_differ_by_position_and_resist_delimiter_collisions() {
        let m = manifest("turn_key_fields = [\"a\", \"b\"]\n");
        let k = |v| match correlate(&m, &v) {
            TurnCorrelation::Exact(k) => k,
            other => panic!("expected a key, got {other:?}"),
        };
        assert_ne!(
            k(json!({"a": "s", "b": "t"})),
            k(json!({"a": "t", "b": "s"})),
            "the key is not positional"
        );
        assert_ne!(
            k(json!({"a": "x|y", "b": "z"})),
            k(json!({"a": "x", "b": "y|z"})),
            "a delimiter collision matched"
        );
        assert_eq!(
            k(json!({"a": "s", "b": "2"})),
            k(json!({"a": "s", "b": "2"}))
        );
    }

    /// Declared order is the comparison order.
    #[test]
    fn fields_are_read_in_declared_order() {
        let m = manifest("turn_key_fields = [\"b\", \"a\"]\n");
        let TurnCorrelation::Exact(TurnKey(parts)) =
            correlate(&m, &json!({"a": "first", "b": "second"}))
        else {
            panic!("expected a key");
        };
        assert_eq!(
            parts,
            vec![
                KeyField::Text("second".into()),
                KeyField::Text("first".into())
            ]
        );
    }
}

/// Authenticated turn ENDS for one pane, plus the turn an active hold is
/// waiting on.
///
/// One structure on purpose. A bounded store that did not know the active
/// key could evict the very end a hold is waiting for, and the pane would
/// then wait forever for evidence that had already arrived. Ends are kept
/// apart from the runtime hook reading so eviction there cannot destroy
/// lifecycle evidence either.
///
/// Bound to the agent and rules that reported them: a replacement
/// occupant's ends are not this one's, and a pane whose manifest changed
/// is being read by different rules.
#[derive(Debug)]
pub(crate) struct PaneEnds {
    agent: crate::identity::ProcId,
    manifest: String,
    /// The turn an active hold is waiting on. Never evicted.
    pinned: Option<TurnKey>,
    /// The pinned turn whose missing or mismatched end has already raised
    /// its one operator diagnostic.
    ///
    /// One slot, not a growing history: only one turn may own the pane's
    /// composer barrier, and changing that owner resets the allowance.
    /// Keeping it beside `pinned` makes the bound use the identical pane,
    /// process-generation, manifest, and turn identity as end matching.
    diagnosed_missing_end: Option<TurnKey>,
    /// Ends seen but not yet consumed, oldest first.
    seen: std::collections::VecDeque<TurnKey>,
    /// Has this store ever discarded an end to stay bounded?
    ///
    /// An end can arrive before the start it belongs to, so an end nobody
    /// is waiting for yet is not the same as an end nobody will ever want.
    /// Dropping one silently leaves a later start pinned on a turn whose
    /// only release evidence is gone, and the composer barrier never
    /// lifts. Raising the cap changes how often that happens, not whether
    /// it can, so the store says when it has happened instead.
    lost: bool,
}

/// Unconsumed ends one pane may hold, beyond the pinned one.
///
/// Small on purpose: an end is consumed by the hold it belongs to, so a
/// backlog is ends nobody is waiting for.
pub(crate) const ENDS_CAP: usize = 16;

pub(crate) type Ends = std::collections::HashMap<crate::PaneKey, PaneEnds>;

impl PaneEnds {
    /// The entry for this binding, cleared if the binding changed.
    fn bound<'a>(
        map: &'a mut Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
    ) -> &'a mut PaneEnds {
        let entry = map.entry(pane.clone()).or_insert_with(|| PaneEnds {
            agent,
            manifest: manifest.to_string(),
            pinned: None,
            diagnosed_missing_end: None,
            seen: std::collections::VecDeque::new(),
            lost: false,
        });
        // A different agent or a different rule set is a different
        // binding, and the previous one's ends say nothing about it.
        if entry.agent != agent || entry.manifest != manifest {
            entry.agent = agent;
            entry.manifest = manifest.to_string();
            entry.pinned = None;
            entry.diagnosed_missing_end = None;
            entry.seen.clear();
            // A new binding starts with no history and no doubt about it.
            entry.lost = false;
        }
        entry
    }

    /// Record one authenticated end.
    pub(crate) fn record(
        map: &mut Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
        turn: TurnKey,
    ) {
        let entry = PaneEnds::bound(map, pane, agent, manifest);
        if !entry.seen.contains(&turn) {
            entry.seen.push_back(turn);
        }
        // Eviction never takes the turn a hold is waiting on, and never
        // happens quietly: what goes is the oldest end no hold has
        // claimed, which is exactly the shape of an end still waiting for
        // its delayed start.
        while entry.seen.len() > ENDS_CAP {
            let Some(pos) = entry
                .seen
                .iter()
                .position(|k| Some(k) != entry.pinned.as_ref())
            else {
                break;
            };
            entry.seen.remove(pos);
            entry.lost = true;
        }
    }

    /// Name the turn an active hold is waiting on, so it survives
    /// eviction until that hold releases.
    ///
    /// False when a DIFFERENT turn already owns the hold. Two starts can
    /// race, and a delayed one can arrive while another turn is running;
    /// letting the later key overwrite the first would hand the hold to a
    /// turn nobody is waiting on and strand the one that is. Setting the
    /// same key again is idempotent. A binding change clears everything
    /// and is free to take a new pin.
    pub(crate) fn pin(
        map: &mut Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) -> bool {
        let entry = PaneEnds::bound(map, pane, agent, manifest);
        match &entry.pinned {
            Some(held) if held != turn => false,
            Some(_) => true,
            None => {
                entry.pinned = Some(turn.clone());
                entry.diagnosed_missing_end = None;
                true
            }
        }
    }

    /// Reserve the one missing-end diagnostic for this exact pinned turn.
    ///
    /// The caller decides whether the visual frame proves the turn ended;
    /// this store only makes that conclusion one-shot under the same exact
    /// identity that matches lifecycle ends. A replacement occupant or a
    /// different turn cannot spend this turn's allowance.
    pub(crate) fn reserve_missing_end_diagnostic(
        map: &mut Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) -> bool {
        let Some(entry) = map.get_mut(pane) else {
            return false;
        };
        if entry.agent != agent
            || entry.manifest != manifest
            || entry.pinned.as_ref() != Some(turn)
            || entry.diagnosed_missing_end.as_ref() == Some(turn)
        {
            return false;
        }
        entry.diagnosed_missing_end = Some(turn.clone());
        true
    }

    /// Is this exact turn's end stored for this exact binding?
    ///
    /// No timestamps. An end can be observed before the start it belongs
    /// to, so arrival order proves nothing and only the key matches.
    pub(crate) fn holds(
        map: &Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) -> bool {
        map.get(pane)
            .is_some_and(|e| e.agent == agent && e.manifest == manifest && e.seen.contains(turn))
    }

    /// Consume the active turn's end, once its hold has released on it.
    ///
    /// All or nothing. False changes nothing at all: a failed or
    /// speculative take that cleared the pin would unprotect the active
    /// key and let the next flood evict the very end it is waiting for,
    /// and one that removed any matching key would let a stale turn
    /// consume evidence belonging to the turn currently running.
    pub(crate) fn take(
        map: &mut Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) -> bool {
        let Some(e) = map.get_mut(pane) else {
            return false;
        };
        if e.agent != agent || e.manifest != manifest || e.pinned.as_ref() != Some(turn) {
            return false;
        }
        let Some(pos) = e.seen.iter().position(|k| k == turn) else {
            return false;
        };
        e.seen.remove(pos);
        e.pinned = None;
        e.diagnosed_missing_end = None;
        true
    }

    /// Has this pane's store discarded an end it might still have needed?
    ///
    /// While this is true, "no end for that turn" stops meaning "the turn
    /// has not ended". The absent end may be one this store threw away,
    /// so a hold waiting on it would wait forever and the pane would take
    /// no further turns. The caller has to escalate rather than keep
    /// waiting; it cannot be resolved from inside the store, because the
    /// evidence is what is missing.
    pub(crate) fn evidence_lost(
        map: &Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
    ) -> bool {
        map.get(pane)
            .is_some_and(|e| e.agent == agent && e.manifest == manifest && e.lost)
    }

    /// Stop waiting on a turn whose end is not what released the hold.
    ///
    /// New input superseding the old turn, and a dead pane, both leave a
    /// pin nobody is ever going to release: the hold moved on without
    /// that turn's end arriving. Retiring clears a matching pin, and
    /// drops a matching queued end if one is there, requiring neither.
    ///
    /// Deliberately weaker than `take`, which is the all-or-nothing
    /// consumption of an end that DID release a hold. Without this, a
    /// superseded turn stays pinned forever and every later start is
    /// refused as a hijack.
    pub(crate) fn retire(
        map: &mut Ends,
        pane: &crate::PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
        turn: &TurnKey,
    ) -> bool {
        let Some(e) = map.get_mut(pane) else {
            return false;
        };
        if e.agent != agent || e.manifest != manifest || e.pinned.as_ref() != Some(turn) {
            return false;
        }
        e.pinned = None;
        e.diagnosed_missing_end = None;
        if let Some(pos) = e.seen.iter().position(|k| k == turn) {
            e.seen.remove(pos);
        }
        true
    }

    /// Drop everything this pane held. A pane id is reusable, so a
    /// removed pane's ends must not be waiting when the id comes back.
    pub(crate) fn forget(map: &mut Ends, pane: &crate::PaneKey) {
        map.remove(pane);
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    fn pane() -> crate::PaneKey {
        crate::PaneKey::new(0, "%1")
    }

    fn proc(pid: i32) -> crate::identity::ProcId {
        crate::identity::ProcId {
            pid,
            birth: pid as u64 * 10,
        }
    }

    fn key(v: &str) -> TurnKey {
        TurnKey(vec![KeyField::Text(v.to_string())])
    }

    /// A clean screen releases a composer barrier only when the stored
    /// end NAMES this turn. A missing end, a wrong key, and a wrong
    /// process generation each hold it.
    ///
    /// This is the exact-lane half of Gate 2. The screen lane releases on
    /// an idle reading, so if the key comparison were loose the barrier
    /// would release on the first clean frame after ANY end, and a Cyclops
    /// write would land on a turn that never finished. The fresh clean
    /// screen is supplied in every case here precisely so that the key,
    /// and nothing else, is what decides.
    #[test]
    fn a_clean_screen_releases_the_barrier_only_on_this_turns_own_end() {
        let agent = proc(100);
        let other_generation = proc(200);
        let m = "t";
        let this_turn = key("turn-1");

        // A fresh clean screen: idle, screen-proven clean composer, no
        // sensor reading a turn as running.
        let clean = clean_idle_detection();
        assert!(
            clean.screen_proves_write_safe_composer(),
            "the fixture must supply the clean frame the release waits on"
        );

        let started = cyclops_proto::ComposerHold::TurnStarted { since_ms: 7 };

        // (a) An authenticated start with NO end at all.
        let mut ends = Ends::new();
        let ended = PaneEnds::holds(&ends, &pane(), agent, m, &this_turn);
        assert!(!ended, "no end was recorded");
        assert_eq!(
            started.advance(&clean, Some(ended)),
            started,
            "a clean frame released a barrier whose turn never ended"
        );

        // (b) An end that names a DIFFERENT turn.
        PaneEnds::record(&mut ends, &pane(), agent, m, key("turn-2"));
        let ended = PaneEnds::holds(&ends, &pane(), agent, m, &this_turn);
        assert!(!ended, "a different turn's end must not match");
        assert_eq!(
            started.advance(&clean, Some(ended)),
            started,
            "another turn's end released this barrier"
        );

        // (b2) The right key under a REPLACED process generation.
        let mut swapped = Ends::new();
        PaneEnds::record(
            &mut swapped,
            &pane(),
            other_generation,
            m,
            this_turn.clone(),
        );
        let ended = PaneEnds::holds(&swapped, &pane(), agent, m, &this_turn);
        assert!(!ended, "a replacement generation's end must not match");
        assert_eq!(
            started.advance(&clean, Some(ended)),
            started,
            "a replaced occupant's end released the predecessor's barrier"
        );

        // The contrast that makes the three above meaningful: this turn's
        // own end, under the same clean frame, DOES release.
        let mut exact = Ends::new();
        PaneEnds::record(&mut exact, &pane(), agent, m, this_turn.clone());
        let ended = PaneEnds::holds(&exact, &pane(), agent, m, &this_turn);
        assert!(ended, "this turn's own end must match");
        assert_eq!(
            started.advance(&clean, Some(ended)),
            cyclops_proto::ComposerHold::Clear,
            "the exact end plus a clean frame must release"
        );
    }

    fn clean_idle_detection() -> cyclops_proto::Detection {
        cyclops_proto::Detection {
            state: cyclops_proto::AgentState::Idle,
            readings: vec![cyclops_proto::SensorReading {
                sensor: cyclops_proto::Sensor::Screen,
                state: cyclops_proto::AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            unknown_reason: None,
            stale: false,
            write_ready: true,
            write_block: None,
            composer_semantic: Some(cyclops_proto::ComposerSemantic::Clean),
        }
    }

    /// The turn a hold is ACTIVELY waiting on outlives a flood of later
    /// ends. The pin is taken first here, so this covers protection of an
    /// already-active key rather than an end that arrived before its
    /// start.
    #[test]
    fn eviction_never_takes_the_turn_a_hold_is_waiting_on() {
        let mut ends = Ends::new();
        let (a, m) = (proc(7), "codex");
        PaneEnds::record(&mut ends, &pane(), a, m, key("early"));
        assert!(PaneEnds::pin(&mut ends, &pane(), a, m, &key("early")));
        for i in 0..(ENDS_CAP * 3) {
            PaneEnds::record(&mut ends, &pane(), a, m, key(&format!("later{i}")));
        }
        assert!(
            PaneEnds::holds(&ends, &pane(), a, m, &key("early")),
            "the pinned end was evicted"
        );
        assert!(
            !PaneEnds::holds(&ends, &pane(), a, m, &key("later0")),
            "nothing was evicted at all"
        );
    }

    /// An end that arrives before its start can be evicted, and the store
    /// has to admit it.
    ///
    /// The bug this pins: eviction protected only the key a hold had
    /// already pinned. An end can arrive BEFORE the start it belongs to,
    /// and such an end is pinned by nobody, so a flood of later ends
    /// silently discarded it. The delayed start then pinned a turn whose
    /// only release evidence was gone, and the composer barrier never
    /// lifted again.
    ///
    /// A bounded store cannot keep everything, so the fix is not a bigger
    /// cap: that changes how often this happens, not whether it can. What
    /// it can do is stop being silent, so absence stops meaning "the turn
    /// has not ended" and the caller escalates instead of waiting.
    #[test]
    fn a_discarded_end_is_admitted_rather_than_forgotten() {
        let mut ends = Ends::new();
        let (a, m) = (proc(7), "codex");
        assert!(
            !PaneEnds::evidence_lost(&ends, &pane(), a, m),
            "a store that has discarded nothing has lost nothing"
        );

        // The end arrives first. Nothing is waiting on it yet, so nothing
        // protects it.
        PaneEnds::record(&mut ends, &pane(), a, m, key("early"));
        for i in 0..ENDS_CAP {
            PaneEnds::record(&mut ends, &pane(), a, m, key(&format!("later{i}")));
        }
        assert!(
            !PaneEnds::holds(&ends, &pane(), a, m, &key("early")),
            "the fixture must actually overflow the cap"
        );
        assert!(
            PaneEnds::evidence_lost(&ends, &pane(), a, m),
            "an end went missing and the store said nothing"
        );

        // The delayed start finally binds. It can still take the pin; what
        // it cannot do is wait quietly for evidence that no longer exists.
        assert!(PaneEnds::pin(&mut ends, &pane(), a, m, &key("early")));
        assert!(PaneEnds::evidence_lost(&ends, &pane(), a, m));

        // A new binding is a new pane occupant with no history and no
        // doubt about it. Either half of the binding changing is enough:
        // a different process, or a different rule set.
        PaneEnds::record(&mut ends, &pane(), proc(8), m, key("fresh"));
        assert!(!PaneEnds::evidence_lost(&ends, &pane(), proc(8), m));

        let mut ends = Ends::new();
        PaneEnds::record(&mut ends, &pane(), a, m, key("early"));
        for i in 0..ENDS_CAP {
            PaneEnds::record(&mut ends, &pane(), a, m, key(&format!("later{i}")));
        }
        assert!(PaneEnds::evidence_lost(&ends, &pane(), a, m));
        PaneEnds::record(&mut ends, &pane(), a, "claude", key("fresh"));
        assert!(!PaneEnds::evidence_lost(&ends, &pane(), a, "claude"));
    }

    /// Consumption is bound to the same agent and rules that recorded it.
    /// A release arriving after the pane changed hands must not take a
    /// replacement's identically-valued key.
    #[test]
    fn a_late_release_cannot_consume_a_replacements_end() {
        let mut ends = Ends::new();
        PaneEnds::record(&mut ends, &pane(), proc(7), "codex", key("t1"));
        // The pane changes hands and the newcomer reports the same value,
        // and its own hold takes ownership of that turn.
        PaneEnds::record(&mut ends, &pane(), proc(8), "codex", key("t1"));
        assert!(PaneEnds::pin(
            &mut ends,
            &pane(),
            proc(8),
            "codex",
            &key("t1")
        ));
        assert!(
            !PaneEnds::take(&mut ends, &pane(), proc(7), "codex", &key("t1")),
            "a dead agent consumed the live one's end"
        );
        assert!(
            !PaneEnds::take(&mut ends, &pane(), proc(8), "claude", &key("t1")),
            "a different rule set consumed it"
        );
        assert!(PaneEnds::take(
            &mut ends,
            &pane(),
            proc(8),
            "codex",
            &key("t1")
        ));
        assert!(!PaneEnds::take(
            &mut ends,
            &pane(),
            proc(8),
            "codex",
            &key("t1")
        ));
    }

    /// A failed take changes nothing, including the pin. Otherwise a
    /// release that ran before its end arrived would unprotect the key,
    /// and the next flood would evict the end when it did arrive.
    #[test]
    fn a_failed_take_preserves_the_pin() {
        let mut ends = Ends::new();
        let (a, m) = (proc(7), "codex");
        assert!(PaneEnds::pin(&mut ends, &pane(), a, m, &key("t1")));
        // The hold releases before the end is stored: nothing to consume.
        assert!(!PaneEnds::take(&mut ends, &pane(), a, m, &key("t1")));
        // The end arrives, then a flood.
        PaneEnds::record(&mut ends, &pane(), a, m, key("t1"));
        for i in 0..(ENDS_CAP * 2) {
            PaneEnds::record(&mut ends, &pane(), a, m, key(&format!("later{i}")));
        }
        assert!(
            PaneEnds::holds(&ends, &pane(), a, m, &key("t1")),
            "the pin was dropped by a failed take and the end was evicted"
        );
        assert!(PaneEnds::take(&mut ends, &pane(), a, m, &key("t1")));
    }

    /// Only the turn that owns the hold may be consumed, and a second
    /// start cannot take the hold from the first.
    #[test]
    fn an_unpinned_key_cannot_be_taken_while_another_is_active() {
        let mut ends = Ends::new();
        let (a, m) = (proc(7), "codex");
        assert!(PaneEnds::pin(&mut ends, &pane(), a, m, &key("active")));
        assert!(
            !PaneEnds::pin(&mut ends, &pane(), a, m, &key("other")),
            "a later start hijacked the active hold"
        );
        assert!(
            PaneEnds::pin(&mut ends, &pane(), a, m, &key("active")),
            "re-pinning the same turn is not a hijack"
        );
        PaneEnds::record(&mut ends, &pane(), a, m, key("other"));
        assert!(
            !PaneEnds::take(&mut ends, &pane(), a, m, &key("other")),
            "a queued end that owns no hold was consumed"
        );
        assert!(PaneEnds::holds(&ends, &pane(), a, m, &key("other")));
    }

    /// A pane id is reusable, so a removed pane leaves nothing behind.
    #[test]
    fn a_removed_pane_forgets_its_ends() {
        let mut ends = Ends::new();
        let (a, m) = (proc(7), "codex");
        PaneEnds::record(&mut ends, &pane(), a, m, key("t1"));
        assert!(PaneEnds::pin(&mut ends, &pane(), a, m, &key("t1")));
        PaneEnds::forget(&mut ends, &pane());
        assert!(ends.is_empty(), "the pane's ends outlived it");
        assert!(!PaneEnds::holds(&ends, &pane(), a, m, &key("t1")));
    }
}
