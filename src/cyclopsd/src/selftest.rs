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

use crate::{ack, daemon_line, delivery, unix_ms, Inner, PaneKey};

/// Default and ceiling for how long hooks.selftest waits for its delivery
/// to resolve. The happy path resolves inside the receipt block; this only
/// bounds a busy or wedged target.
const SELFTEST_DEFAULT_MS: u64 = 10_000;
const SELFTEST_MAX_MS: u64 = 60_000;

/// One pane's edges: normalized event name -> (raw vendor spelling for
/// display, last-seen unix ms).
type PaneEdges = HashMap<String, (String, u64)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PaneLifetime(u64);

/// Exact hook setup whose evidence may settle one delayed diagnostic.
///
/// The pane lifetime prevents a sleeper from reviving state after physical
/// pane loss. Process and manifest keep replacement and repin evidence apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct HookBinding {
    pane: PaneKey,
    lifetime: PaneLifetime,
    agent: crate::identity::ProcId,
    manifest: String,
}

#[derive(Default)]
struct HookLivenessState {
    next_lifetime: u64,
    live: HashMap<PaneKey, PaneLifetime>,
    edges: HashMap<HookBinding, PaneEdges>,
    /// Admission-eligible edges, kept apart from `edges`: published by the
    /// report path only after the manifest declared the event and any start
    /// the edge carries was installed, so a recompute can never observe an
    /// eligible edge without its start. `edges` stays diagnostic.
    admitting: HashMap<HookBinding, HashSet<String>>,
    f1_notified: HashSet<HookBinding>,
}

/// Per-pane hook edge record. Keys are exact watched routes. In-memory and
/// boot-scoped: "ever" means "this daemon run", which is exactly the
/// question at adoption/boot (did THIS setup ever produce an edge).
pub(crate) struct HookLiveness {
    state: StdMutex<HookLivenessState>,
}
/// The pane has no live lifetime: its route is not open yet (or is closed).
/// Reports never open routes, so the caller answers retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteNotOpen;
/// The captured pane lifetime is no longer the live one: a replacement
/// occupant owns the pane now and inherits nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LifetimeExpired;

impl HookLiveness {
    pub(crate) fn new() -> HookLiveness {
        HookLiveness {
            state: StdMutex::new(HookLivenessState::default()),
        }
    }

    /// Mark one authoritative pane route live. Reattach is idempotent. A route
    /// reopened after physical loss receives a new lifetime.
    pub(crate) fn open(&self, pane: &PaneKey) {
        let mut state = self.state.lock().expect("hook liveness lock");
        if state.live.contains_key(pane) {
            return;
        }
        state.next_lifetime = state
            .next_lifetime
            .checked_add(1)
            .expect("pane lifetime exhausted");
        let lifetime = PaneLifetime(state.next_lifetime);
        state.live.insert(pane.clone(), lifetime);
    }

    /// Bind a diagnostic to the pane lifetime that is live now.
    ///
    /// This never opens a route. Lifecycle code owns open and close, so a
    /// delayed task cannot recreate a pane that has already disappeared.
    pub(crate) fn binding(
        &self,
        pane: &PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
    ) -> Option<HookBinding> {
        let state = self.state.lock().expect("hook liveness lock");
        Some(HookBinding {
            pane: pane.clone(),
            lifetime: *state.live.get(pane)?,
            agent,
            manifest: manifest.to_string(),
        })
    }

    /// Record one agent.state.report edge for an exact process and manifest.
    /// Duplicates count: a duplicate still proves the hook config is loaded
    /// and firing. Process generations remain distinct so a delayed check
    /// for one submitted occupant cannot read another occupant's history.
    /// Record one diagnostic edge under the exact binding that is live right
    /// now and hand that binding back. The binding carries the pane
    /// lifetime, the agent process identity including birth, and the
    /// manifest; a later publication uses it verbatim and never looks a
    /// lifetime up again. When no lifetime is live the route is not open:
    /// nothing is recorded and the caller must answer retryable, because
    /// reports never open routes.
    pub(crate) fn bind_diagnostic(
        &self,
        pane: &PaneKey,
        raw_event: &str,
        ts: u64,
        agent: crate::identity::ProcId,
        manifest: &str,
    ) -> Result<HookBinding, RouteNotOpen> {
        let mut state = self.state.lock().expect("hook liveness lock");
        let Some(lifetime) = state.live.get(pane).copied() else {
            return Err(RouteNotOpen);
        };
        let binding = HookBinding {
            pane: pane.clone(),
            lifetime,
            agent,
            manifest: manifest.to_string(),
        };
        state
            .edges
            .entry(binding.clone())
            .or_default()
            .insert(ack::normalize_event(raw_event), (raw_event.to_string(), ts));
        Ok(binding)
    }

    /// True once any hook edge has been seen from the CURRENT occupant of
    /// this pane and manifest. Other process generations or manifests count
    /// for nothing.
    /// Exact-binding, event-specific liveness for runtime-idle admission:
    /// only a normalized `SessionStart` or `UserPromptSubmit` edge from the
    /// current agent generation in this pane lifetime qualifies. `Stop` and
    /// `StopFailure` are telemetry, `Notification` and `PermissionRequest`
    /// are attention edges; none of them says the agent is at rest with a
    /// composer it owns, so none of them admits a pane. `seen_any` stays the
    /// broader "hooks are wired at all" answer behind `hooks_verified`.
    pub(crate) fn seen_admitting_edge(
        &self,
        pane: &PaneKey,
        current_agent: crate::identity::ProcId,
        manifest: &str,
    ) -> bool {
        let Some(binding) = self.binding(pane, current_agent, manifest) else {
            return false;
        };
        let state = self.state.lock().expect("hook liveness lock");
        state
            .admitting
            .get(&binding)
            .is_some_and(|events| !events.is_empty())
    }
    /// Publish an admission-eligible edge for a binding captured earlier by
    /// [`HookLiveness::bind_diagnostic`]. Only a normalized `SessionStart` or
    /// `UserPromptSubmit` is ever stored; any other event is a no-op here.
    /// The captured pane lifetime must still be the live one: a route that
    /// closed and reopened in between belongs to a replacement occupant,
    /// which inherits nothing, so the publication is refused and the caller
    /// answers `occupant_changed`. No new lifetime is ever looked up here.
    /// The report path calls this after the manifest declared the event and
    /// after any start the edge carries was installed.
    pub(crate) fn publish_admission(
        &self,
        binding: &HookBinding,
        raw_event: &str,
    ) -> Result<(), LifetimeExpired> {
        const ADMITTING_EVENTS: [&str; 2] = ["SessionStart", "UserPromptSubmit"];
        let event = ack::normalize_event(raw_event);
        let mut state = self.state.lock().expect("hook liveness lock");
        if state.live.get(&binding.pane) != Some(&binding.lifetime) {
            return Err(LifetimeExpired);
        }
        if !ADMITTING_EVENTS
            .iter()
            .any(|admitting| ack::normalize_event(admitting) == event)
        {
            return Ok(());
        }
        state
            .admitting
            .entry(binding.clone())
            .or_default()
            .insert(event);
        Ok(())
    }
    /// Test-only: how many distinct diagnostic events and admission-eligible
    /// events this exact binding holds, so a repeated report can be proven
    /// to collapse to one of each rather than accumulate.
    #[cfg(test)]
    pub(crate) fn edge_counts(&self, binding: &HookBinding) -> (usize, usize) {
        let state = self.state.lock().expect("hook liveness lock");
        (
            state.edges.get(binding).map_or(0, HashMap::len),
            state.admitting.get(binding).map_or(0, HashSet::len),
        )
    }
    pub(crate) fn seen_any(
        &self,
        pane: &PaneKey,
        current_agent: crate::identity::ProcId,
        manifest: &str,
    ) -> bool {
        let Some(binding) = self.binding(pane, current_agent, manifest) else {
            return false;
        };
        self.state
            .lock()
            .expect("hook liveness lock")
            .edges
            .get(&binding)
            .is_some_and(|edges| !edges.is_empty())
    }

    /// Whether any of `lifecycle` has been observed for this exact binding
    /// since the daemon started.
    ///
    /// Three records here look like they answer this and two of them do
    /// not, so the distinction is worth stating once.
    ///
    /// `seen_any` is every raw hook: a Notification, a StopFailure or a
    /// PermissionRequest all make it true, and none of them is a turn edge.
    /// Answering "has a turn been reported" with it lets an ACK-only or
    /// attention-only pane look like a pane that has been reporting turns.
    ///
    /// `seen_admitting_edge` is the injection-safety subset, published only
    /// after the manifest declared the event and any start it carries was
    /// installed. It answers whether a WRITE may be authorised. Explaining
    /// runtime with it would couple the explanation to the terminal-write
    /// gate, which is exactly the coupling the runtime work is removing,
    /// and it would drop events like AGY's PreInvocation.
    ///
    /// This one intersects the durable diagnostic edges with the event
    /// names the manifest actually declares as lifecycle roles. No
    /// admission concept, no new state, and nothing but turn edges.
    pub(crate) fn seen_declared_lifecycle(
        &self,
        pane: &PaneKey,
        current_agent: crate::identity::ProcId,
        manifest: &str,
        lifecycle: &[&str],
    ) -> bool {
        if lifecycle.is_empty() {
            return false;
        }
        let Some(binding) = self.binding(pane, current_agent, manifest) else {
            return false;
        };
        self.state
            .lock()
            .expect("hook liveness lock")
            .edges
            .get(&binding)
            .is_some_and(|edges| {
                // The edge map is keyed by the NORMALIZED event name while a
                // manifest declares the vendor's raw spelling, so the
                // comparison has to normalize too. The normalizer is shared
                // with the parser on purpose: two copies would be two
                // definitions of "the same event".
                lifecycle
                    .iter()
                    .any(|event| edges.contains_key(&crate::ack::normalize_event(event)))
            })
    }

    /// Last-seen unix ms per normalized event, with the raw spelling.
    /// Empty when the recorded edges belong to another process or manifest.
    fn snapshot(
        &self,
        pane: &PaneKey,
        current_agent: crate::identity::ProcId,
        manifest: &str,
    ) -> PaneEdges {
        let Some(binding) = self.binding(pane, current_agent, manifest) else {
            return PaneEdges::new();
        };
        self.state
            .lock()
            .expect("hook liveness lock")
            .edges
            .get(&binding)
            .cloned()
            .unwrap_or_default()
    }

    /// Pane closed: its edges and its F1 one-shot die with it.
    pub(crate) fn close(&self, pane: &PaneKey) {
        let mut state = self.state.lock().expect("hook liveness lock");
        state.live.remove(pane);
        state.edges.retain(|binding, _| &binding.pane != pane);
        state.admitting.retain(|binding, _| &binding.pane != pane);
        state.f1_notified.retain(|binding| &binding.pane != pane);
    }

    /// Reserve one F1 notification only when this exact live binding has no
    /// edge. The lifetime check, absence check, and reservation share one lock
    /// so close and concurrent hook arrival cannot split the decision.
    pub(crate) fn reserve_f1_if_no_edges(&self, binding: &HookBinding) -> bool {
        let mut state = self.state.lock().expect("hook liveness lock");
        if state.live.get(&binding.pane) != Some(&binding.lifetime) {
            return false;
        }
        if state
            .edges
            .get(binding)
            .is_some_and(|edges| !edges.is_empty())
        {
            return false;
        }
        state.f1_notified.insert(binding.clone())
    }
}

/// True when the manifest wires any hook event, i.e. hook liveness is a
/// meaningful question for panes it binds.
pub(crate) fn declares_hooks(m: &Manifest) -> bool {
    !m.hooks.lifecycle_names().is_empty() || m.hooks.ack.is_some()
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
    pane: &PaneKey,
    adopted: bool,
    manifest_id: Option<&str>,
    agent: Option<crate::identity::ProcId>,
) -> Option<bool> {
    if !adopted {
        return None;
    }
    let m = inner.manifests.get(manifest_id?)?;
    if !declares_hooks(m) {
        return None;
    }
    // The ADMITTED AGENT, not the pane's root process. Reports are filed
    // under the agent's own identity, and a pane root is a shell that
    // never emitted a hook in its life. An agent nobody can identify
    // right now has no proven liveness either.
    Some(agent.is_some_and(|a| inner.hook_liveness.seen_any(pane, a, &m.agent.id)))
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
        "claude" => "Claude has no live hook edges. Its active settings file probably \
            lacks Cyclops entries. Run: cyclops start --setup-only --wire-hooks, then \
            restart Claude. For an isolated --settings launch, run: cyclops hooks install \
            claude --agent <label>"
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

/// First tier-1 ACK timeout for the exact submitted occupant with zero hook
/// edges: one admin ping naming the likely F1 cause. The delivery itself
/// has already downgraded to the screen tier; this only makes the why
/// visible. Liveness is per occupant, so a restarted occupant without
/// hooks pings again instead of hiding behind its predecessor's edges.
pub(crate) fn notify_f1_once(inner: &Arc<Inner>, msg_id: &str, to: &str, binding: HookBinding) {
    // Use the exact live binding proven immediately before Enter. A process
    // re-read can observe a replacement, and a delayed task can outlive its
    // pane. Neither may settle this attempt's setup diagnostic.
    if !inner.hook_liveness.reserve_f1_if_no_edges(&binding) {
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
            f1_cause(&binding.manifest)
        ),
        Some(msg_id),
        Some(binding.pane.session_idx),
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
    let Some((session_idx, pane_id)) = inner.resolve_recipient(&params.target) else {
        return Err(WireError {
            code: "no_such_target".to_string(),
            message: format!("no such target {:?}", params.target),
            data: None,
        });
    };
    let row = inner
        .watcher_of(session_idx)
        .and_then(|watcher| watcher.pane(&pane_id))
        .ok_or_else(|| WireError {
            code: "no_such_target".to_string(),
            message: "pane vanished during verify".to_string(),
            data: None,
        })?;
    let manifest = crate::fusion::bind_manifest_for(inner, session_idx, &row);
    let adopted = inner.label_for_route(session_idx, &pane_id).is_some();
    let (manifest_id, tier) = match manifest {
        Some(m) => (Some(m.agent.id.clone()), tier_of(m)),
        None => (None, 2),
    };
    let agent = crate::fusion::admitted_vendor(inner, session_idx, &row).map(|(_, proc)| proc);
    let pane = PaneKey::new(session_idx, &pane_id);
    let hooks_verified = hooks_verified_for(inner, &pane, adopted, manifest_id.as_deref(), agent);

    // Declared key events first (manifest order: ack, turn_start,
    // turn_end, deduped on the normalized name), then anything else that
    // actually fired. Edges from a previous occupant are not listed.
    let seen = agent
        .zip(manifest_id.as_deref())
        .map(|(a, id)| inner.hook_liveness.snapshot(&pane, a, id))
        .unwrap_or_default();
    let now = unix_ms();
    let mut events: Vec<HookEdgeAge> = Vec::new();
    let mut listed: HashSet<String> = HashSet::new();
    if let Some(m) = manifest {
        for declared in m
            .hooks
            .ack
            .as_deref()
            .into_iter()
            .chain(m.hooks.lifecycle_names())
        {
            let key = ack::normalize_event(declared);
            if !listed.insert(key.clone()) {
                continue;
            }
            events.push(HookEdgeAge {
                event: declared.to_string(),
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
        .watcher_of(session_idx)
        .and_then(|watcher| watcher.pane(&pane_id))
        .and_then(|row| {
            crate::fusion::bind_manifest_for(inner, session_idx, &row)
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
        recipient_keys: None,
        expected_caller: None,
        subject: "[cyclops] hook self-test".to_string(),
        summary: None,
        body: "Reply not needed.".to_string(),
        fyi: true,
        client_key: None,
        reply_to: None,
        supersedes: None,
        wait: None,
        require_wake: false,
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
    let lines = slot.ledger.read_after(0).ok()?;
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

    /// A synthetic identity: tests care that two of them differ, not what
    /// the kernel would have said.
    fn proc(pid: i32) -> crate::identity::ProcId {
        crate::identity::ProcId {
            pid,
            birth: pid as u64 * 10,
        }
    }

    fn pane(id: &str) -> PaneKey {
        PaneKey::new(0, id)
    }

    fn open(liveness: &HookLiveness, pane: &PaneKey) {
        liveness.open(pane);
    }

    fn binding(
        liveness: &HookLiveness,
        pane: &PaneKey,
        agent: crate::identity::ProcId,
        manifest: &str,
    ) -> HookBinding {
        liveness
            .binding(pane, agent, manifest)
            .expect("live pane binding")
    }

    /// The F1 setup diagnostic fires at most once per binding.
    ///
    /// This is the ZERO-HOOK-EDGE case, not a missing lifecycle end: it
    /// reports "hooks configured but never seen", and any edge at all,
    /// including a start, suppresses it. Named precisely because the two
    /// are easy to confuse and only one of them is this.
    ///
    /// The bound is not a timer or a counter, it is set membership keyed
    /// on the exact binding, so the three ways this could go wrong reduce
    /// to one question: can a second reservation for the same binding
    /// succeed. The two suppressions carry as much weight as the count.
    #[test]
    fn the_f1_setup_diagnostic_reserves_at_most_once_per_binding() {
        let liveness = HookLiveness::new();
        let route = pane("%1");
        open(&liveness, &route);
        let bound = binding(&liveness, &route, proc(100), "test");

        assert!(
            liveness.reserve_f1_if_no_edges(&bound),
            "the first zero-edge binding must be reportable"
        );
        assert!(
            !liveness.reserve_f1_if_no_edges(&bound),
            "the same binding reported a second diagnostic"
        );

        // Any edge at all, start or end, means hooks were seen.
        let with_edges = pane("%2");
        open(&liveness, &with_edges);
        let edged = binding(&liveness, &with_edges, proc(200), "test");
        let _ = liveness.bind_diagnostic(&with_edges, "Stop", 1_000, proc(200), "test");
        assert!(
            !liveness.reserve_f1_if_no_edges(&edged),
            "a pane that produced any hook edge is not an unseen-hooks case"
        );

        // A replaced occupant is not the pane the delivery was about. The
        // binding is captured before the swap, exactly as the delayed task
        // holds it, so this is the stale-task case and not a fresh read.
        let swapped = pane("%3");
        open(&liveness, &swapped);
        let stale = binding(&liveness, &swapped, proc(300), "test");
        liveness.close(&swapped);
        open(&liveness, &swapped);
        assert!(
            !liveness.reserve_f1_if_no_edges(&stale),
            "a diagnostic settled against a replaced occupant"
        );
    }

    #[test]
    fn liveness_records_and_forgets() {
        let l = HookLiveness::new();
        let route = pane("%1");
        open(&l, &route);
        assert!(!l.seen_any(&route, proc(100), "test"));
        let _ = l.bind_diagnostic(&route, "UserPromptSubmit", 1_000, proc(100), "test");
        let _ = l.bind_diagnostic(&route, "user_prompt_submit", 2_000, proc(100), "test");
        assert!(l.seen_any(&route, proc(100), "test"));
        // Normalized spellings share a slot; the raw name and ts are the
        // latest ones.
        let snap = l.snapshot(&route, proc(100), "test");
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap.get("userpromptsubmit"),
            Some(&("user_prompt_submit".to_string(), 2_000))
        );
        l.close(&route);
        assert!(!l.seen_any(&route, proc(100), "test"));
    }

    #[test]
    fn exact_route_liveness_separates_duplicate_pane_ids() {
        let liveness = HookLiveness::new();
        let first = PaneKey::new(0, "%1");
        let second = PaneKey::new(1, "%1");
        open(&liveness, &first);
        open(&liveness, &second);
        let _ = liveness.bind_diagnostic(&first, "Stop", 1_000, proc(100), "test");

        assert!(liveness.seen_any(&first, proc(100), "test"));
        assert!(
            !liveness.seen_any(&second, proc(100), "test"),
            "a hook edge cannot cross watched session routes"
        );

        let _ = liveness.bind_diagnostic(&second, "UserPromptSubmit", 2_000, proc(200), "test");
        liveness.close(&first);
        assert!(!liveness.seen_any(&first, proc(100), "test"));
        assert!(
            liveness.seen_any(&second, proc(200), "test"),
            "forgetting one route cannot erase the duplicate pane in another session"
        );
    }

    /// The F1 stale-liveness hole: edges belong to the occupant that
    /// produced them. An occupant swap never transfers either occupant's
    /// evidence to the other.
    #[test]
    fn occupant_swap_invalidates_liveness() {
        let l = HookLiveness::new();
        let route = pane("%1");
        open(&l, &route);
        let _ = l.bind_diagnostic(&route, "UserPromptSubmit", 1_000, proc(100), "test");
        assert!(l.seen_any(&route, proc(100), "test"));
        // The occupant restarted: same pane, new pid, no edges from it yet.
        assert!(
            !l.seen_any(&route, proc(200), "test"),
            "old occupant's edges must not count"
        );
        assert!(l.snapshot(&route, proc(200), "test").is_empty());
        // The old process's exact history remains available to a delayed
        // diagnostic for a delivery that was submitted to it.
        assert!(l.seen_any(&route, proc(100), "test"));
        let _ = l.bind_diagnostic(&route, "Stop", 3_000, proc(200), "test");
        assert!(l.seen_any(&route, proc(200), "test"));
        assert!(l.seen_any(&route, proc(100), "test"));
        let snap = l.snapshot(&route, proc(200), "test");
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key("stop"));
    }

    #[test]
    fn f1_reservation_is_atomic_once_per_occupant_and_resets_with_the_pane() {
        let l = HookLiveness::new();
        let first = pane("%1");
        let second = pane("%2");
        open(&l, &first);
        open(&l, &second);
        let first_agent = binding(&l, &first, proc(100), "test");
        let second_route = binding(&l, &second, proc(300), "test");
        assert!(l.reserve_f1_if_no_edges(&first_agent));
        assert!(!l.reserve_f1_if_no_edges(&first_agent));
        assert!(l.reserve_f1_if_no_edges(&second_route));
        // A new occupant of the same pane gets its own one-shot.
        let replacement = binding(&l, &first, proc(200), "test");
        assert!(l.reserve_f1_if_no_edges(&replacement));
        assert!(!l.reserve_f1_if_no_edges(&replacement));
        // Returning to the first exact generation cannot reserve it twice.
        assert!(!l.reserve_f1_if_no_edges(&first_agent));

        l.close(&first);
        assert!(
            !l.reserve_f1_if_no_edges(&replacement),
            "a captured binding cannot revive a closed pane"
        );
        open(&l, &first);
        let reopened = binding(&l, &first, proc(200), "test");
        assert_ne!(replacement, reopened);
        assert!(l.reserve_f1_if_no_edges(&reopened));
    }

    #[test]
    fn an_exact_hook_edge_suppresses_only_its_occupants_f1_reservation() {
        let l = HookLiveness::new();
        let route = pane("%1");
        open(&l, &route);
        let first = binding(&l, &route, proc(100), "test");
        let second = binding(&l, &route, proc(200), "test");
        let _ = l.bind_diagnostic(&route, "Stop", 1_000, proc(100), "test");
        assert!(!l.reserve_f1_if_no_edges(&first));
        assert!(l.reserve_f1_if_no_edges(&second));

        let _ = l.bind_diagnostic(&route, "Stop", 2_000, proc(200), "test");
        assert!(!l.reserve_f1_if_no_edges(&second));
        assert!(!l.reserve_f1_if_no_edges(&first));
    }

    #[test]
    fn manifests_do_not_share_hook_edges_or_f1_reservations() {
        let l = HookLiveness::new();
        let route = pane("%1");
        let agent = proc(100);
        open(&l, &route);
        let _ = l.bind_diagnostic(&route, "Stop", 1_000, agent, "claude");

        assert!(l.seen_any(&route, agent, "claude"));
        assert!(!l.seen_any(&route, agent, "codex"));
        assert!(!l.reserve_f1_if_no_edges(&binding(&l, &route, agent, "claude")));
        assert!(l.reserve_f1_if_no_edges(&binding(&l, &route, agent, "codex")));
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
            "[agent]\nid = \"a\"\ndisplay_name = \"a\"\n\n[hooks]\nturn_end = \"Stop\"\nturn_end_evidence = \"confirmed\"\n",
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
    fn f1_cause_names_vendor_wiring_traps() {
        let c = f1_cause("codex");
        assert!(c.contains("untrusted directory"));
        assert!(c.contains("CODEX_HOME"));
        assert!(c.contains("trust_level"));
        assert!(c.contains("does NOT fix"));
        assert!(f1_cause("claude").contains("--wire-hooks"));
        assert!(f1_cause("claude").contains("--settings"));
        assert!(f1_cause("agy").contains(".agents/hooks.json"));
        assert!(f1_cause("cursor").contains("CURSOR_CONFIG_DIR"));
        assert!(f1_cause("mystery").contains("mystery"));
    }
}
