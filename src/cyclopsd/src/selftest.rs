//! Hook liveness and the startup self-test (amendment c: configuration
//! does not equal subscription).
//!
//! A hook config can be perfectly rendered and still never fire: codex
//! silently loads zero hooks in an untrusted directory (finding F1), claude
//! only reads a settings file it was launched with, agy only reads
//! .agents/hooks.json in the workspace. The daemon therefore tracks, per
//! pane, the last time each hook event actually arrived via
//! agent.state.report, and exposes three things built on that record:
//!
//! - `hooks_verified` on PaneStatus: false for an adopted tier-declaring
//!   pane no hook edge has EVER reached this daemon run, true once one has.
//! - `hooks.verify`: the per-event last-seen ages behind that bit.
//! - `hooks.selftest`: a daemon-driven no-op round trip through the normal
//!   delivery pipeline (fyi, subject "[cyclops] hook self-test") reporting
//!   whether the ACK hook fired with the marker. Costs the target one
//!   trivial turn.
//!
//! The first delivery that times out its tier-1 ACK window on a pane with
//! zero edges ever seen also pings the admin once, naming the likely F1
//! cause; the delivery itself downgrades to screen evidence as usual (no
//! hang, no loss).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use cyclops_manifest::Manifest;
use cyclops_proto::{
    DeliveryState, HookEdgeAge, HooksSelftestParams, HooksSelftestResult, HooksVerifyParams,
    HooksVerifyResult, Kind, LedgerLine, MsgSendParams, NotifyLevel, WireError,
};
use serde_json::{json, Value};
use tokio::time::{Duration, Instant};

use crate::{ack, daemon_line, delivery, unix_ms, Inner};

/// Default and ceiling for how long hooks.selftest waits for its delivery
/// to resolve. The happy path resolves inside the receipt block; this only
/// bounds a busy or wedged target.
const SELFTEST_DEFAULT_MS: u64 = 10_000;
const SELFTEST_MAX_MS: u64 = 60_000;

/// One pane's edges: normalized event name -> (raw vendor spelling for
/// display, last-seen unix ms).
type PaneEdges = HashMap<String, (String, u64)>;

/// One pane's liveness: the edges plus the pane_pid of the occupant they
/// came from. Edges are evidence about ONE occupant's hook config; an
/// occupant restart without hooks must not keep showing hooks_verified
/// behind the previous occupant's edges (the F1 stale-liveness hole).
struct OccupantEdges {
    pane_pid: i32,
    edges: PaneEdges,
}

/// Per-pane hook edge record. Keys are pane ids. In-memory and
/// boot-scoped: "ever" means "this daemon run", which is exactly the
/// question at adoption/boot (did THIS setup ever produce an edge).
pub(crate) struct HookLiveness {
    edges: StdMutex<HashMap<String, OccupantEdges>>,
    /// Occupant (pane id -> pane_pid) whose F1 notification already went
    /// out: one ping per occupant, never a loop.
    f1_notified: StdMutex<HashMap<String, i32>>,
}

impl HookLiveness {
    pub(crate) fn new() -> HookLiveness {
        HookLiveness {
            edges: StdMutex::new(HashMap::new()),
            f1_notified: StdMutex::new(HashMap::new()),
        }
    }

    /// Record one agent.state.report edge for the occupant `pane_pid`.
    /// Duplicates count: a duplicate still proves the hook config is loaded
    /// and firing. An edge from a NEW occupant retires the old occupant's
    /// edges: they were never evidence about this process.
    pub(crate) fn record(&self, pane_id: &str, raw_event: &str, ts: u64, pane_pid: i32) {
        let mut edges = self.edges.lock().expect("hook liveness lock");
        let entry = edges
            .entry(pane_id.to_string())
            .or_insert_with(|| OccupantEdges {
                pane_pid,
                edges: PaneEdges::new(),
            });
        if entry.pane_pid != pane_pid {
            entry.edges.clear();
            entry.pane_pid = pane_pid;
        }
        entry
            .edges
            .insert(ack::normalize_event(raw_event), (raw_event.to_string(), ts));
    }

    /// True once any hook edge has been seen from the CURRENT occupant of
    /// this pane. Edges recorded under a different pane_pid are a previous
    /// occupant's and count for nothing.
    pub(crate) fn seen_any(&self, pane_id: &str, current_pid: i32) -> bool {
        self.edges
            .lock()
            .expect("hook liveness lock")
            .get(pane_id)
            .is_some_and(|o| o.pane_pid == current_pid && !o.edges.is_empty())
    }

    /// Last-seen unix ms per normalized event, with the raw spelling.
    /// Empty when the recorded edges belong to a previous occupant.
    fn snapshot(&self, pane_id: &str, current_pid: i32) -> PaneEdges {
        self.edges
            .lock()
            .expect("hook liveness lock")
            .get(pane_id)
            .filter(|o| o.pane_pid == current_pid)
            .map(|o| o.edges.clone())
            .unwrap_or_default()
    }

    /// Pane closed: its edges and its F1 one-shot die with it.
    pub(crate) fn forget(&self, pane_id: &str) {
        self.edges
            .lock()
            .expect("hook liveness lock")
            .remove(pane_id);
        self.f1_notified
            .lock()
            .expect("f1 notified lock")
            .remove(pane_id);
    }

    /// True exactly once per pane occupant: the caller sends the F1
    /// notification. A new occupant gets its own one-shot.
    fn first_f1(&self, pane_id: &str, current_pid: i32) -> bool {
        let mut notified = self.f1_notified.lock().expect("f1 notified lock");
        if notified.get(pane_id) == Some(&current_pid) {
            return false;
        }
        notified.insert(pane_id.to_string(), current_pid);
        true
    }
}

/// True when the manifest wires any hook event, i.e. hook liveness is a
/// meaningful question for panes it binds.
pub(crate) fn declares_hooks(m: &Manifest) -> bool {
    m.hooks.turn_start.is_some() || m.hooks.turn_end.is_some() || m.hooks.ack.is_some()
}

/// ACK capability tier (DELIVERY.md): 1 = payload-matchable hook ACK,
/// 2 = screen evidence only.
pub(crate) fn tier_of(m: &Manifest) -> u8 {
    if m.hooks.ack.is_some() && m.hooks.ack_payload_field.is_some() {
        1
    } else {
        2
    }
}

/// hooks_verified for one status row. `adopted` and `manifest_id` come from
/// the caller's already-held status locks so nothing re-locks here.
pub(crate) fn hooks_verified_for(
    inner: &Inner,
    pane_id: &str,
    adopted: bool,
    manifest_id: Option<&str>,
    pane_pid: i32,
) -> Option<bool> {
    if !adopted {
        return None;
    }
    let m = inner.manifests.get(manifest_id?)?;
    if !declares_hooks(m) {
        return None;
    }
    Some(inner.hook_liveness.seen_any(pane_id, pane_pid))
}

/// The likely reason a configured hook set never fires, per CLI. F1 is the
/// codex trap this whole module exists for.
pub(crate) fn f1_cause(manifest_id: &str) -> String {
    match manifest_id {
        "codex" => "codex loads zero hooks in an untrusted directory (finding F1): \
            put hooks.json in CODEX_HOME, or seed trust in config.toml \
            ([projects.\"<dir>\"] trust_level = \"trusted\"). \
            --dangerously-bypass-hook-trust does NOT fix this. \
            Run: cyclops hooks install codex --agent <label>"
            .to_string(),
        "claude" => "claude only reads hooks from a settings file it was launched with; \
            it was probably started without --settings pointing at the rendered config. \
            Run: cyclops hooks install claude --agent <label>"
            .to_string(),
        "agy" => "agy only reads .agents/hooks.json in the workspace; the file is \
            probably missing where this agent runs. \
            Run: cyclops hooks install agy --agent <label>"
            .to_string(),
        "cursor" => "cursor reads hooks from ~/.cursor/hooks.json or \
            <workspace>/.cursor/hooks.json; CURSOR_CONFIG_DIR does NOT apply to \
            hooks, so a hooks.json placed there never loads. \
            Run: cyclops hooks install cursor --agent <label>"
            .to_string(),
        other => format!("the {other} hook config is probably not loaded by the running CLI"),
    }
}

/// First tier-1 ACK timeout on a pane whose CURRENT occupant has zero hook
/// edges: one admin ping naming the likely F1 cause. The delivery itself
/// has already downgraded to the screen tier; this only makes the why
/// visible. Liveness is per occupant, so a restarted occupant without
/// hooks pings again instead of hiding behind its predecessor's edges.
pub(crate) fn notify_f1_once(
    inner: &Arc<Inner>,
    msg_id: &str,
    to: &str,
    pane_id: &str,
    session_idx: usize,
    manifest_id: &str,
) {
    // The current occupant's pid from the live table; a vanished pane has
    // no occupant to warn about.
    let Some(pane_pid) = inner
        .watcher_of(session_idx)
        .and_then(|w| w.pane(pane_id))
        .map(|row| row.pane_pid)
    else {
        return;
    };
    if inner.hook_liveness.seen_any(pane_id, pane_pid) {
        return;
    }
    if !inner.hook_liveness.first_f1(pane_id, pane_pid) {
        return;
    }
    delivery::admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        &format!("{to}: hooks configured but never seen"),
        &format!(
            "message {msg_id} got no tier-1 hook ACK and no hook edge has ever \
             reached this daemon from that pane; the delivery downgraded to \
             screen evidence. Likely cause: {}",
            f1_cause(manifest_id)
        ),
        Some(msg_id),
        Some(session_idx),
        // The delivery this is about. It downgraded to the screen tier
        // rather than stalling, so the rule counts it as nobody's to
        // clear, and a reader's calm stream holds the ping to that.
        delivery::About::delivery(to),
    );
}

/// hooks.verify: tier, hooks_verified, and per-event last-seen ages for
/// one target pane.
pub(crate) async fn verify(
    inner: &Arc<Inner>,
    params: HooksVerifyParams,
) -> Result<Value, WireError> {
    let Some((_, pane_id)) = inner.resolve_recipient(&params.target) else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("no such target {:?}", params.target),
            data: None,
        });
    };
    let row = inner
        .session_slots()
        .iter()
        .find_map(|slot| {
            let link = slot.link.lock().expect("session link lock");
            link.watcher.as_ref().and_then(|w| w.pane(&pane_id))
        })
        .ok_or_else(|| WireError {
            code: "no_such_target".to_string(),
            message: "pane vanished during verify".to_string(),
            data: None,
        })?;
    let manifest = crate::fusion::bind_manifest_for(inner, &row);
    let adopted = inner.label_of(&pane_id).is_some();
    let (manifest_id, tier) = match manifest {
        Some(m) => (Some(m.agent.id.clone()), tier_of(m)),
        None => (None, 2),
    };
    let hooks_verified = hooks_verified_for(
        inner,
        &pane_id,
        adopted,
        manifest_id.as_deref(),
        row.pane_pid,
    );

    // Declared key events first (manifest order: ack, turn_start,
    // turn_end, deduped on the normalized name), then anything else that
    // actually fired. Edges from a previous occupant are not listed.
    let seen = inner.hook_liveness.snapshot(&pane_id, row.pane_pid);
    let now = unix_ms();
    let mut events: Vec<HookEdgeAge> = Vec::new();
    let mut listed: HashSet<String> = HashSet::new();
    if let Some(m) = manifest {
        for declared in [&m.hooks.ack, &m.hooks.turn_start, &m.hooks.turn_end]
            .into_iter()
            .flatten()
        {
            let key = ack::normalize_event(declared);
            if !listed.insert(key.clone()) {
                continue;
            }
            events.push(HookEdgeAge {
                event: declared.clone(),
                last_seen_ms_ago: seen.get(&key).map(|(_, ts)| now.saturating_sub(*ts)),
            });
        }
    }
    let mut extra: Vec<HookEdgeAge> = seen
        .iter()
        .filter(|(key, _)| !listed.contains(*key))
        .map(|(_, (raw, ts))| HookEdgeAge {
            event: raw.clone(),
            last_seen_ms_ago: Some(now.saturating_sub(*ts)),
        })
        .collect();
    extra.sort_by(|a, b| a.event.cmp(&b.event));
    events.extend(extra);

    let result = HooksVerifyResult {
        target: params.target,
        pane_id,
        manifest: manifest_id,
        tier,
        hooks_verified,
        events,
    };
    Ok(serde_json::to_value(result).expect("hooks.verify result serializes"))
}

/// hooks.selftest: send one fyi marker through the normal delivery
/// pipeline and report whether the ACK hook fired with it. The recipient
/// is asked for no action, so the cost is one trivial turn.
pub(crate) async fn selftest(
    inner: &Arc<Inner>,
    params: HooksSelftestParams,
) -> Result<Value, WireError> {
    let Some((session_idx, pane_id)) = inner.resolve_recipient(&params.target) else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("no such target {:?}", params.target),
            data: None,
        });
    };
    let (manifest_id, tier) = inner
        .session_slots()
        .iter()
        .find_map(|slot| {
            let link = slot.link.lock().expect("session link lock");
            link.watcher.as_ref().and_then(|w| w.pane(&pane_id))
        })
        .and_then(|row| {
            crate::fusion::bind_manifest_for(inner, &row)
                .map(|m| (Some(m.agent.id.clone()), tier_of(m)))
        })
        .unwrap_or((None, 2));

    let timeout = params
        .timeout_ms
        .unwrap_or(SELFTEST_DEFAULT_MS)
        .min(SELFTEST_MAX_MS);
    let started = Instant::now();
    // Subscribe before sending so a resolution racing the receipt is never
    // missed.
    let mut rx = inner.events.subscribe();
    let send_params = MsgSendParams {
        to: vec![params.target.clone()],
        subject: "[cyclops] hook self-test".to_string(),
        body: "Reply not needed.".to_string(),
        fyi: true,
        reply_to: None,
        wait: None,
    };
    let receipt = delivery::msg_send(inner, "cyclopsd", send_params).await?;
    let msg_id = receipt["msg_id"].as_str().unwrap_or_default().to_string();
    let mut state: DeliveryState =
        serde_json::from_value(receipt["deliveries"][0]["state"].clone())
            .unwrap_or(DeliveryState::Queued);

    // The receipt resolves on the idle path; a busy target answers queued
    // and the delivery-state stream carries the resolution.
    let deadline = started + Duration::from_millis(timeout);
    while !resolved(state) {
        let ev = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = rx.recv() => ev,
        };
        match ev {
            Ok(e) if e.event == "delivery-state" && e.data["id"] == msg_id.as_str() => {
                if let Ok(s) = serde_json::from_value::<DeliveryState>(e.data["to_state"].clone()) {
                    state = s;
                }
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Reconcile on doubt: the ledger has the truth.
                if let Some(s) = ledger_state(inner, session_idx, &msg_id, &params.target) {
                    state = s;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }

    let hook_ack = state == DeliveryState::DeliveredVerified;
    let result = HooksSelftestResult {
        target: params.target.clone(),
        msg_id: msg_id.clone(),
        manifest: manifest_id,
        tier,
        state,
        hook_ack,
        waited_ms: started.elapsed().as_millis() as u64,
    };
    // Self-test results are system facts (ledger Kind::System charter).
    inner.append_line(
        session_idx,
        LedgerLine {
            to: vec![params.target],
            ..daemon_line(
                Kind::System,
                msg_id,
                json!({
                    "event": "hook_selftest",
                    "tier": tier,
                    "state": state,
                    "hook_ack": hook_ack,
                }),
            )
        },
    );
    Ok(serde_json::to_value(result).expect("hooks.selftest result serializes"))
}

fn resolved(s: DeliveryState) -> bool {
    matches!(
        s,
        DeliveryState::DeliveredVerified
            | DeliveryState::DeliveredUnverified
            | DeliveryState::AttentionRequired
            | DeliveryState::ParkedBlockedQuota
    )
}

/// Latest delivery state for (msg, recipient) read back from the session
/// ledger, for the lagged-stream path only.
fn ledger_state(
    inner: &Arc<Inner>,
    session_idx: usize,
    msg_id: &str,
    to: &str,
) -> Option<DeliveryState> {
    let slot = inner.session(session_idx)?;
    let lines = cyclops_ledger::read_after(slot.ledger.path(), 0).ok()?;
    lines
        .iter()
        .rev()
        .filter(|l| matches!(l.kind, Kind::State) && l.id == msg_id)
        .find_map(|l| {
            let data = l.data.as_ref()?;
            if data.get("to")?.as_str()? != to {
                return None;
            }
            serde_json::from_value(data.get("to_state")?.clone()).ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn liveness_records_and_forgets() {
        let l = HookLiveness::new();
        assert!(!l.seen_any("%1", 100));
        l.record("%1", "UserPromptSubmit", 1_000, 100);
        l.record("%1", "user_prompt_submit", 2_000, 100);
        assert!(l.seen_any("%1", 100));
        // Normalized spellings share a slot; the raw name and ts are the
        // latest ones.
        let snap = l.snapshot("%1", 100);
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap.get("userpromptsubmit"),
            Some(&("user_prompt_submit".to_string(), 2_000))
        );
        l.forget("%1");
        assert!(!l.seen_any("%1", 100));
    }

    /// The F1 stale-liveness hole: edges belong to the occupant that
    /// produced them. An occupant swap (new pane_pid) invalidates the old
    /// edges on read AND on the next record.
    #[test]
    fn occupant_swap_invalidates_liveness() {
        let l = HookLiveness::new();
        l.record("%1", "UserPromptSubmit", 1_000, 100);
        assert!(l.seen_any("%1", 100));
        // The occupant restarted: same pane, new pid, no edges from it yet.
        assert!(
            !l.seen_any("%1", 200),
            "old occupant's edges must not count"
        );
        assert!(l.snapshot("%1", 200).is_empty());
        // The old pid's view is still intact until the new occupant speaks.
        assert!(l.seen_any("%1", 100));
        // First edge from the new occupant retires the old record entirely.
        l.record("%1", "Stop", 3_000, 200);
        assert!(l.seen_any("%1", 200));
        assert!(!l.seen_any("%1", 100));
        let snap = l.snapshot("%1", 200);
        assert_eq!(snap.len(), 1, "old occupant's edges were retired");
        assert!(snap.contains_key("stop"));
    }

    #[test]
    fn f1_notify_is_once_per_occupant_and_resets_with_the_pane() {
        let l = HookLiveness::new();
        assert!(l.first_f1("%1", 100));
        assert!(!l.first_f1("%1", 100));
        assert!(l.first_f1("%2", 300));
        // A new occupant of the same pane gets its own one-shot.
        assert!(l.first_f1("%1", 200));
        assert!(!l.first_f1("%1", 200));
        l.forget("%1");
        assert!(l.first_f1("%1", 200));
    }

    #[test]
    fn tier_and_declaration_read_the_manifest() {
        let tier1 = Manifest::parse(
            "[agent]\nid = \"c\"\ndisplay_name = \"c\"\n\n[hooks]\nack = \"UserPromptSubmit\"\nack_payload_field = \"prompt\"\n",
            Path::new("c.toml"),
        )
        .unwrap();
        assert_eq!(tier_of(&tier1), 1);
        assert!(declares_hooks(&tier1));

        // agy shape: turn events but no payload-matchable ACK.
        let tier2 = Manifest::parse(
            "[agent]\nid = \"a\"\ndisplay_name = \"a\"\n\n[hooks]\nturn_end = \"Stop\"\n",
            Path::new("a.toml"),
        )
        .unwrap();
        assert_eq!(tier_of(&tier2), 2);
        assert!(declares_hooks(&tier2));

        let none = Manifest::parse(
            "[agent]\nid = \"x\"\ndisplay_name = \"x\"\n",
            Path::new("x.toml"),
        )
        .unwrap();
        assert_eq!(tier_of(&none), 2);
        assert!(!declares_hooks(&none));
    }

    #[test]
    fn f1_cause_names_the_codex_trap() {
        let c = f1_cause("codex");
        assert!(c.contains("untrusted directory"));
        assert!(c.contains("CODEX_HOME"));
        assert!(c.contains("trust_level"));
        assert!(c.contains("does NOT fix"));
        assert!(f1_cause("claude").contains("--settings"));
        assert!(f1_cause("agy").contains(".agents/hooks.json"));
        assert!(f1_cause("cursor").contains("CURSOR_CONFIG_DIR"));
        assert!(f1_cause("mystery").contains("mystery"));
    }
}
