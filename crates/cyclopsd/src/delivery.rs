//! The M1 delivery pipeline (docs/DELIVERY.md is the spec).
//!
//! One worker per target pane; deliveries to one recipient are strictly
//! FIFO. Every state transition appends a ledger line and emits an event.
//! Failures queue or park; they never drop (limbo is a bug).
//!
//! Zero-polling shape: workers sleep on queue notifies and wake on watcher
//! or fusion events. The only timers are per-delivery one-shots: the paste
//! verification re-reads, the tier-1 ACK window, the screen-evidence
//! checkpoints, and the decline-key spacing. Nothing runs on an interval.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_proto::{
    AgentState, Delivery, DeliveryReceipt, DeliveryState, Event, Kind, LedgerLine, MsgSendParams,
    MsgSendResult, NotifyLevel, VerifiedBy, WaitUntil, WireError,
};
use cyclops_tmux::{quote_arg, PaneEvent, SessionWatcher};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, warn};

use crate::{fusion, unix_ms, Inner};

/// Delivery gives up on evidence this long after submit (spec: neither ACK
/// tier within 5s goes to retry_queued).
const SCREEN_ACK_DEADLINE: Duration = Duration::from_secs(5);
/// One-shot screen-evidence checkpoints after submit. Events also wake the
/// waiter; these bound the captures per delivery.
const ACK_CHECKPOINTS_MS: [u64; 5] = [250, 750, 1500, 3000, 5000];
/// Post-paste verification re-reads (paste rendering can lag a frame).
/// Offsets from the paste, one capture each; bounded per attempt.
const VERIFY_DELAYS_MS: [u64; 4] = [0, 120, 240, 480];
/// Bottom non-empty lines scanned for the staged verify pattern.
const VERIFY_REGION: usize = 15;
/// Bottom non-empty lines treated as the composer zone for the
/// marker-left-composer check.
const COMPOSER_WINDOW: usize = 6;
/// Spacing between manifest decline keys (amendment g: ~250ms).
const DECLINE_SPACING: Duration = Duration::from_millis(250);
/// Attempts to auto-dismiss one modal rule before treating it as
/// non-dismissable (hold plus admin notify). Never loop.
const MAX_DECLINES: u32 = 3;
/// Default and ceiling for agent.wait / send-and-wait timeouts.
const WAIT_DEFAULT_MS: u64 = 60_000;
const WAIT_MAX_MS: u64 = 600_000;
/// Upgradeable delivered_unverified handles kept per pane for late hook
/// ACK upgrades.
const ACK_REGISTRY_CAP: usize = 32;

/// Delivery engine state. Lives in [`Inner`]; all behavior is free
/// functions taking the daemon state so nothing here holds locks across
/// awaits by construction.
pub(crate) struct Engine {
    /// One worker per target pane id.
    workers: StdMutex<HashMap<String, Arc<Worker>>>,
    /// Worker tasks, aborted on daemon shutdown.
    pub(crate) worker_tasks: StdMutex<Vec<JoinHandle<()>>>,
    /// Message ids ever issued or seen in the ledgers (unique per ledger).
    issued: StdMutex<HashSet<String>>,
    /// Per-delivery unique tmux buffer names (amendment e).
    buffer_seq: AtomicU64,
    /// Deliveries awaiting or upgradeable by a hook ACK, per pane id.
    acks: StdMutex<HashMap<String, Vec<Arc<DeliveryHandle>>>>,
}

impl Engine {
    pub(crate) fn new() -> Engine {
        Engine {
            workers: StdMutex::new(HashMap::new()),
            worker_tasks: StdMutex::new(Vec::new()),
            issued: StdMutex::new(HashSet::new()),
            buffer_seq: AtomicU64::new(0),
            acks: StdMutex::new(HashMap::new()),
        }
    }

    /// Seed the issued-id set from a ledger so new ids stay unique per
    /// ledger across daemon restarts.
    pub(crate) fn preload_ids(&self, lines: &[LedgerLine]) {
        let mut issued = self.issued.lock().expect("issued lock");
        for l in lines {
            if matches!(l.kind, Kind::Msg | Kind::Fyi) {
                issued.insert(l.id.clone());
            }
        }
    }

    /// Mint a short unique message id, e.g. "m-3f9c2a".
    fn mint_msg_id(&self) -> String {
        let mut issued = self.issued.lock().expect("issued lock");
        loop {
            let id = format!("m-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
            if issued.insert(id.clone()) {
                return id;
            }
        }
    }
}

/// Per-recipient FIFO worker. The task sleeps on `notify` when idle.
struct Worker {
    session_idx: usize,
    queue: StdMutex<VecDeque<Arc<DeliveryHandle>>>,
    notify: Notify,
    busy: AtomicBool,
    /// Set when quota parking hit this recipient; carries the reset hint.
    /// Cleared only by an operator verb (M2). Never auto-retried.
    parked: StdMutex<Option<String>>,
}

impl Worker {
    /// Deliveries ahead of `handle` from the sender's point of view.
    fn position_of(&self, handle: &Arc<DeliveryHandle>) -> u32 {
        let q = self.queue.lock().expect("worker queue lock");
        let busy = self.busy.load(Ordering::SeqCst) as u32;
        match q.iter().position(|h| Arc::ptr_eq(h, handle)) {
            Some(i) => i as u32 + busy,
            // Not queued: it is the in-flight one (or already resolved).
            None => 0,
        }
    }
}

/// One recipient's delivery, shared between the worker, the ACK matcher,
/// and receipt waiters.
pub(crate) struct DeliveryHandle {
    pub(crate) msg_id: String,
    /// Recipient as addressed (label, or pane id when unlabeled).
    pub(crate) to: String,
    pub(crate) pane_id: String,
    pub(crate) session_idx: usize,
    payload: String,
    state: StdMutex<HandleState>,
    state_tx: watch::Sender<DeliveryState>,
    /// Wakes the worker when the ACK matcher resolved this delivery.
    ack: Notify,
    /// Hook ACK that raced ahead of the Submitted transition; consumed by
    /// the worker right after submitting.
    early_ack: AtomicBool,
}

struct HandleState {
    state: DeliveryState,
    attempts: u32,
    verified_by: Option<VerifiedBy>,
    cause: Option<String>,
    /// Human hint carried into receipts (quota reset, attention cause).
    note: Option<String>,
}

impl DeliveryHandle {
    fn new(
        msg_id: &str,
        to: &str,
        pane_id: &str,
        session_idx: usize,
        payload: String,
    ) -> Arc<Self> {
        let (state_tx, _) = watch::channel(DeliveryState::Queued);
        Arc::new(DeliveryHandle {
            msg_id: msg_id.to_string(),
            to: to.to_string(),
            pane_id: pane_id.to_string(),
            session_idx,
            payload,
            state: StdMutex::new(HandleState {
                state: DeliveryState::Queued,
                attempts: 0,
                verified_by: None,
                cause: None,
                note: None,
            }),
            state_tx,
            ack: Notify::new(),
            early_ack: AtomicBool::new(false),
        })
    }

    pub(crate) fn state(&self) -> DeliveryState {
        self.state.lock().expect("handle state lock").state
    }

    fn snapshot(
        &self,
    ) -> (
        DeliveryState,
        u32,
        Option<VerifiedBy>,
        Option<String>,
        Option<String>,
    ) {
        let st = self.state.lock().expect("handle state lock");
        (
            st.state,
            st.attempts,
            st.verified_by,
            st.cause.clone(),
            st.note.clone(),
        )
    }
}

/// One transition request for [`advance`].
struct Step<'a> {
    next: DeliveryState,
    cause: Option<&'a str>,
    verified_by: Option<VerifiedBy>,
    note: Option<String>,
}

impl<'a> Step<'a> {
    fn to(next: DeliveryState) -> Step<'a> {
        Step {
            next,
            cause: None,
            verified_by: None,
            note: None,
        }
    }
    fn cause(mut self, c: &'a str) -> Step<'a> {
        self.cause = Some(c);
        self
    }
    fn verified(mut self, v: VerifiedBy) -> Step<'a> {
        self.verified_by = Some(v);
        self
    }
    fn note(mut self, n: String) -> Step<'a> {
        self.note = Some(n);
        self
    }
}

/// Every transition the pipeline performs, checked as legal against
/// `DeliveryState::can_transition_to` by a unit test and a debug assertion
/// in [`advance`].
#[cfg(test)]
const PIPELINE_TRANSITIONS: &[(DeliveryState, DeliveryState)] = {
    use DeliveryState::*;
    &[
        (Queued, Gating),
        (Queued, AttentionRequired),
        (Queued, ParkedBlockedQuota),
        (Gating, Pasting),
        (Gating, AttentionRequired),
        (Gating, ParkedBlockedQuota),
        (Pasting, Staged),
        (Pasting, RetryQueued),
        (Pasting, AttentionRequired),
        (Staged, Submitted),
        (Staged, RetryQueued),
        (Staged, AttentionRequired),
        (Submitted, DeliveredVerified),
        (Submitted, DeliveredUnverified),
        (Submitted, RetryQueued),
        (Submitted, AttentionRequired),
        (DeliveredUnverified, DeliveredVerified),
        (RetryQueued, Gating),
        (RetryQueued, AttentionRequired),
    ]
};

/// Apply one transition if the delivery is still in an expected state.
/// Returns false when a concurrent actor (ACK matcher vs worker timeout)
/// already moved it; the caller treats that as "someone else resolved it".
/// Legal-transition checking is a debug assertion: an illegal move is a
/// programming bug, and the ledger records what actually happened.
fn advance(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    allowed_from: &[DeliveryState],
    step: Step<'_>,
) -> bool {
    let (from, record) = {
        let mut st = handle.state.lock().expect("handle state lock");
        if !allowed_from.contains(&st.state) {
            return false;
        }
        let from = st.state;
        debug_assert!(
            from.can_transition_to(step.next),
            "illegal delivery transition {from:?} -> {:?}",
            step.next
        );
        if !from.can_transition_to(step.next) {
            error!(
                id = %handle.msg_id,
                ?from,
                to_state = ?step.next,
                "refusing illegal delivery transition"
            );
            return false;
        }
        st.state = step.next;
        if let Some(v) = step.verified_by {
            st.verified_by = Some(v);
        }
        st.cause = step.cause.map(str::to_string);
        if let Some(n) = &step.note {
            st.note = Some(n.clone());
        }
        (
            from,
            Delivery {
                to: handle.to.clone(),
                state: st.state,
                verified_by: st.verified_by,
                attempts: st.attempts,
                ts: unix_ms(),
                cause: st.cause.clone(),
            },
        )
    };
    let line = LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id: handle.msg_id.clone(),
        ts: 0,
        kind: Kind::State,
        from: "cyclopsd".to_string(),
        to: vec![handle.to.clone()],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: vec![record.clone()],
        data: Some(json!({
            "to": handle.to,
            "from": from,
            "to_state": step.next,
            "cause": step.cause,
        })),
    };
    let seq = inner.append_line(handle.session_idx, line);
    inner.emit(
        "delivery-state",
        json!({
            "id": handle.msg_id,
            "to": handle.to,
            "from": from,
            "to_state": step.next,
            "cause": step.cause,
            "verified_by": record.verified_by,
            "attempts": record.attempts,
            "note": step.note,
        }),
        seq,
    );
    // send_replace, not send: watch::Sender::send drops the value when no
    // receiver exists, and receipt blocking subscribes late. A worker that
    // resolves before the subscribe must still leave the state readable, or
    // the receipt waits out its whole cap on an already-final delivery.
    handle.state_tx.send_replace(step.next);
    true
}

/// Append a gate decision line and emit the matching event.
fn gate_line(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    action: &str,
    rule: Option<&str>,
    cause: Option<&str>,
) {
    let line = LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id: handle.msg_id.clone(),
        ts: 0,
        kind: Kind::Gate,
        from: "cyclopsd".to_string(),
        to: vec![handle.to.clone()],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: Vec::new(),
        data: Some(json!({
            "to": handle.to,
            "action": action,
            "rule": rule,
            "cause": cause,
        })),
    };
    let seq = inner.append_line(handle.session_idx, line);
    inner.emit(
        "gate",
        json!({
            "id": handle.msg_id,
            "to": handle.to,
            "action": action,
            "rule": rule,
            "cause": cause,
        }),
        seq,
    );
}

/// Write a kind=system admin notification line and broadcast the event.
/// `session_idx` scopes internal (delivery-driven) notifications to the
/// recipient's ledger; None (external admin.notify) writes to every
/// session ledger so any single-session reader sees it.
pub(crate) fn admin_notify(
    inner: &Arc<Inner>,
    level: NotifyLevel,
    subject: &str,
    body: &str,
    msg_id: Option<&str>,
    session_idx: Option<usize>,
) -> Option<u64> {
    let id = msg_id
        .map(str::to_string)
        .unwrap_or_else(|| inner.mint_event_id());
    let line = LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id: id.clone(),
        ts: 0,
        kind: Kind::System,
        from: "cyclopsd".to_string(),
        to: vec!["admin".to_string()],
        subject: Some(subject.to_string()),
        body: if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        },
        reply_to: None,
        deliveries: Vec::new(),
        data: Some(json!({"event": "admin_notify", "level": level})),
    };
    let sessions: Vec<usize> = match session_idx {
        Some(i) => vec![i],
        None => (0..inner.sessions.len()).collect(),
    };
    let mut first_seq = None;
    for idx in sessions {
        let seq = inner.append_line(idx, line.clone());
        if first_seq.is_none() {
            first_seq = seq;
        }
    }
    inner.emit(
        "admin-notify",
        json!({"id": id, "level": level, "subject": subject, "body": body}),
        first_seq,
    );
    first_seq
}

// ---------------------------------------------------------------------------
// msg.send
// ---------------------------------------------------------------------------

/// Render the injected payload. The daemon builds the envelope; nothing in
/// the request body can forge it (sender identity is structural).
pub(crate) fn render_payload(
    msg_id: &str,
    from: &str,
    subject: &str,
    body: &str,
    fyi: bool,
) -> String {
    let mut lines = vec![format!(
        "[cyclops {msg_id}] FROM: {from}  SUBJECT: {subject}"
    )];
    if !body.is_empty() {
        lines.push(body.to_string());
    }
    if !fyi {
        lines.push(format!(
            "Reply with: cyclops send {from} --subject \"...\" [--body ... | --body-file -]"
        ));
    }
    lines.join("\n")
}

/// The msg.send entry: ledger the message, fan deliveries out to per-pane
/// workers, and build receipts per DELIVERY.md semantics (block on the
/// idle path up to receipt_block_ms, immediate queued/parked otherwise).
pub(crate) async fn msg_send(
    inner: &Arc<Inner>,
    from: &str,
    params: MsgSendParams,
) -> Result<Value, WireError> {
    if inner.sessions.is_empty() {
        return Err(wire_err("no_such_target", "no sessions are watched"));
    }
    let names = expand_recipients(inner, &params.to)?;

    // Resolve each recipient before writing the msg line so the ledger's
    // delivery records carry the addressed names.
    let resolved: Vec<(String, Option<(usize, String)>)> = names
        .iter()
        .map(|n| (n.clone(), inner.resolve_recipient(n)))
        .collect();

    let msg_id = inner.engine.mint_msg_id();
    let payload = render_payload(&msg_id, from, &params.subject, &params.body, params.fyi);
    let now = unix_ms();
    let deliveries: Vec<Delivery> = names
        .iter()
        .map(|n| Delivery {
            to: n.clone(),
            state: DeliveryState::Queued,
            verified_by: None,
            attempts: 0,
            ts: now,
            cause: None,
        })
        .collect();
    let line = LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id: msg_id.clone(),
        ts: 0,
        kind: if params.fyi { Kind::Fyi } else { Kind::Msg },
        from: from.to_string(),
        to: names.clone(),
        subject: Some(params.subject.clone()),
        body: if params.body.is_empty() {
            None
        } else {
            Some(params.body.clone())
        },
        reply_to: params.reply_to.clone(),
        deliveries,
        data: None,
    };
    // One msg fact. With recipients across sessions the same line lands in
    // each involved per-session file; each file stays a complete stream.
    let mut involved: Vec<usize> = resolved
        .iter()
        .filter_map(|(_, r)| r.as_ref().map(|(idx, _)| *idx))
        .collect();
    involved.sort_unstable();
    involved.dedup();
    if involved.is_empty() {
        involved.push(0);
    }
    let mut first_seq = None;
    for idx in &involved {
        let seq = inner.append_line(*idx, line.clone());
        if first_seq.is_none() {
            first_seq = seq;
        }
    }
    let seq = first_seq.unwrap_or(0);
    inner.emit(
        "msg",
        json!({
            "id": msg_id,
            "from": from,
            "to": names,
            "subject": params.subject,
            "body": params.body,
            "fyi": params.fyi,
            "reply_to": params.reply_to,
        }),
        first_seq,
    );

    // Fan out. Each delivery record advances independently.
    let mut handles: Vec<Arc<DeliveryHandle>> = Vec::with_capacity(resolved.len());
    let mut blocking: Vec<Arc<DeliveryHandle>> = Vec::new();
    for (name, target) in &resolved {
        match target {
            None => {
                // Gate step 1: unresolvable recipient needs a human.
                let handle = DeliveryHandle::new(&msg_id, name, "", 0, String::new());
                advance(
                    inner,
                    &handle,
                    &[DeliveryState::Queued],
                    Step::to(DeliveryState::AttentionRequired)
                        .cause("no_such_pane")
                        .note(format!("no pane for {name:?}")),
                );
                admin_notify(
                    inner,
                    NotifyLevel::ActionRequired,
                    &format!("delivery to {name} needs attention"),
                    &format!("message {msg_id}: no such pane for {name:?}"),
                    Some(&msg_id),
                    None,
                );
                handles.push(handle);
            }
            Some((session_idx, pane_id)) => {
                let handle =
                    DeliveryHandle::new(&msg_id, name, pane_id, *session_idx, payload.clone());
                let worker = worker_for(inner, *session_idx, pane_id);
                let parked_hint = worker.parked.lock().expect("parked lock").clone();
                if let Some(hint) = parked_hint {
                    // Parked recipients never auto-retry; new sends park
                    // immediately with the reset hint (amendment f).
                    advance(
                        inner,
                        &handle,
                        &[DeliveryState::Queued],
                        Step::to(DeliveryState::ParkedBlockedQuota)
                            .cause("blocked_quota")
                            .note(hint),
                    );
                } else {
                    // Idle path blocks for its receipt; busy path answers
                    // queued immediately.
                    let idle_now = !worker.busy.load(Ordering::SeqCst)
                        && worker.queue.lock().expect("worker queue lock").is_empty()
                        && inner.cached_state(pane_id) == AgentState::Idle;
                    worker
                        .queue
                        .lock()
                        .expect("worker queue lock")
                        .push_back(Arc::clone(&handle));
                    worker.notify.notify_one();
                    if idle_now {
                        blocking.push(Arc::clone(&handle));
                    }
                }
                handles.push(handle);
            }
        }
    }

    // Receipts: block only on the idle path, capped by receipt_block_ms.
    let deadline = Instant::now() + Duration::from_millis(inner.cfg.receipt_block_ms);
    for handle in &blocking {
        let mut rx = handle.state_tx.subscribe();
        let _ = tokio::time::timeout_at(deadline, rx.wait_for(|s| receipt_resolved(*s))).await;
    }
    let receipts: Vec<DeliveryReceipt> = handles.iter().map(|h| receipt_of(inner, h)).collect();

    let result = MsgSendResult {
        msg_id: msg_id.clone(),
        seq,
        deliveries: receipts,
    };
    let mut value = serde_json::to_value(result).expect("msg.send result serializes");

    // Send-and-wait composes agent.wait onto the same call: after the
    // delivery resolves, block until the recipient reaches `until`.
    if let Some(spec) = &params.wait {
        let timeout = spec.timeout_ms.unwrap_or(WAIT_DEFAULT_MS).min(WAIT_MAX_MS);
        let wait_deadline = Instant::now() + Duration::from_millis(timeout);
        let mut waits = Vec::new();
        for handle in handles.iter().filter(|h| !h.pane_id.is_empty()) {
            let remaining = wait_deadline.saturating_duration_since(Instant::now());
            let (state, timed_out) =
                wait_until(inner, &handle.pane_id, spec.until, remaining).await;
            waits.push(json!({"to": handle.to, "state": state, "timed_out": timed_out}));
        }
        value["wait"] = Value::Array(waits);
    }
    Ok(value)
}

fn wire_err(code: &str, msg: impl Into<String>) -> WireError {
    WireError {
        code: code.to_string(),
        message: msg.into(),
    }
}

/// A receipt never conflates in-flight machinery with public states: a
/// delivery still moving reports queued with its position.
fn receipt_resolved(s: DeliveryState) -> bool {
    matches!(
        s,
        DeliveryState::DeliveredVerified
            | DeliveryState::DeliveredUnverified
            | DeliveryState::AttentionRequired
            | DeliveryState::ParkedBlockedQuota
    )
}

fn receipt_of(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) -> DeliveryReceipt {
    let (state, _, _, cause, note) = handle.snapshot();
    if receipt_resolved(state) {
        DeliveryReceipt {
            to: handle.to.clone(),
            state,
            position: None,
            note: note.or(cause),
        }
    } else {
        let position = inner
            .engine
            .workers
            .lock()
            .expect("workers lock")
            .get(&handle.pane_id)
            .map(|w| w.position_of(handle));
        DeliveryReceipt {
            to: handle.to.clone(),
            state: DeliveryState::Queued,
            position,
            note: None,
        }
    }
}

/// Expand the to-list: "*" means every labeled pane (explicit adoption is
/// the broadcast domain). Order is preserved, duplicates dropped.
fn expand_recipients(inner: &Arc<Inner>, to: &[String]) -> Result<Vec<String>, WireError> {
    let mut names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for t in to {
        if t == "*" {
            let mut labels: Vec<String> = inner
                .labels
                .lock()
                .expect("labels lock")
                .values()
                .cloned()
                .collect();
            labels.sort();
            for l in labels {
                if seen.insert(l.clone()) {
                    names.push(l);
                }
            }
        } else if !t.is_empty() && seen.insert(t.clone()) {
            names.push(t.clone());
        }
    }
    if names.is_empty() {
        return Err(wire_err(
            "bad_request",
            "no recipients: give labels, pane ids, or \"*\" with labeled panes",
        ));
    }
    Ok(names)
}

/// Get or spawn the FIFO worker owning one pane.
fn worker_for(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> Arc<Worker> {
    let mut workers = inner.engine.workers.lock().expect("workers lock");
    if let Some(w) = workers.get(pane_id) {
        return Arc::clone(w);
    }
    let worker = Arc::new(Worker {
        session_idx,
        queue: StdMutex::new(VecDeque::new()),
        notify: Notify::new(),
        busy: AtomicBool::new(false),
        parked: StdMutex::new(None),
    });
    workers.insert(pane_id.to_string(), Arc::clone(&worker));
    let task = tokio::spawn(worker_loop(Arc::clone(inner), Arc::clone(&worker)));
    inner
        .engine
        .worker_tasks
        .lock()
        .expect("worker tasks lock")
        .push(task);
    worker
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

async fn worker_loop(inner: Arc<Inner>, worker: Arc<Worker>) {
    loop {
        let job = worker.queue.lock().expect("worker queue lock").pop_front();
        match job {
            Some(handle) => {
                let parked_hint = worker.parked.lock().expect("parked lock").clone();
                if let Some(hint) = parked_hint {
                    // A job that raced in around the parking moment parks
                    // too; nothing behind a quota wall auto-retries.
                    advance(
                        &inner,
                        &handle,
                        &[DeliveryState::Queued],
                        Step::to(DeliveryState::ParkedBlockedQuota)
                            .cause("blocked_quota")
                            .note(hint),
                    );
                    continue;
                }
                worker.busy.store(true, Ordering::SeqCst);
                process(&inner, &worker, &handle).await;
                worker.busy.store(false, Ordering::SeqCst);
            }
            None => worker.notify.notified().await,
        }
    }
}

/// Drive one delivery through gate, inject, submit, ACK, bounded retry.
async fn process(inner: &Arc<Inner>, worker: &Arc<Worker>, handle: &Arc<DeliveryHandle>) {
    if !advance(
        inner,
        handle,
        &[DeliveryState::Queued],
        Step::to(DeliveryState::Gating),
    ) {
        return;
    }
    loop {
        match gate(inner, handle).await {
            GateOutcome::Park { hint } => {
                park_recipient(inner, worker, handle, hint).await;
                return;
            }
            GateOutcome::Attention { cause } => {
                advance(
                    inner,
                    handle,
                    &[DeliveryState::Gating],
                    Step::to(DeliveryState::AttentionRequired).cause(&cause),
                );
                notify_attention(inner, handle, &cause);
                return;
            }
            GateOutcome::Proceed { manifest_id } => {
                {
                    let mut st = handle.state.lock().expect("handle state lock");
                    st.attempts += 1;
                }
                if !advance(
                    inner,
                    handle,
                    &[DeliveryState::Gating],
                    Step::to(DeliveryState::Pasting),
                ) {
                    return;
                }
                match attempt_delivery(inner, worker, handle, &manifest_id).await {
                    AttemptOutcome::Done => return,
                    AttemptOutcome::Failed(cause) => {
                        if !fail_attempt(inner, handle, &cause) {
                            return;
                        }
                        // Bounded retry: back through the gate.
                        if !advance(
                            inner,
                            handle,
                            &[DeliveryState::RetryQueued],
                            Step::to(DeliveryState::Gating).cause("retry"),
                        ) {
                            return;
                        }
                    }
                }
            }
        }
    }
}

enum AttemptOutcome {
    /// Delivery resolved (verified, unverified, or matcher-resolved).
    Done,
    /// This attempt failed; cause feeds retry accounting.
    Failed(String),
}

/// One injection attempt: paste, verify, submit, wait for an ACK tier.
async fn attempt_delivery(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
) -> AttemptOutcome {
    let Some(watcher) = inner.watcher_of(worker.session_idx) else {
        return AttemptOutcome::Failed("session_detached".to_string());
    };
    let Some(manifest) = inner.manifests.get(manifest_id) else {
        return AttemptOutcome::Failed("no_manifest".to_string());
    };
    let staged_window = match inject(inner, &watcher, handle, manifest).await {
        Ok(w) => w,
        Err(cause) => return AttemptOutcome::Failed(cause),
    };
    if !advance(
        inner,
        handle,
        &[DeliveryState::Pasting],
        Step::to(DeliveryState::Staged),
    ) {
        return AttemptOutcome::Done;
    }
    // Register for hook ACK matching before the submit key: the measured
    // hook edge is 21-28ms after Enter and must not race the registry.
    register_ack(inner, handle);
    let submit_key = if manifest.injection.submit.is_empty() {
        "Enter"
    } else {
        manifest.injection.submit.as_str()
    };
    if let Err(e) = watcher
        .client()
        .send_keys(&handle.pane_id, &[submit_key])
        .await
    {
        warn!(id = %handle.msg_id, error = %e, "submit key failed");
        unregister_ack(inner, handle);
        // Move to retry_queued here; fail_attempt sees the state and only
        // does the bookkeeping.
        let _ = advance(
            inner,
            handle,
            &[DeliveryState::Staged],
            Step::to(DeliveryState::RetryQueued).cause("submit_failed"),
        );
        return AttemptOutcome::Failed("submit_failed".to_string());
    }
    if !advance(
        inner,
        handle,
        &[DeliveryState::Staged],
        Step::to(DeliveryState::Submitted),
    ) {
        return AttemptOutcome::Done;
    }
    // An ACK that arrived between paste-verify and the Submitted line.
    if handle.early_ack.swap(false, Ordering::SeqCst)
        && advance(
            inner,
            handle,
            &[DeliveryState::Submitted],
            Step::to(DeliveryState::DeliveredVerified)
                .cause("hook_ack")
                .verified(VerifiedBy::Hook),
        )
    {
        return AttemptOutcome::Done;
    }
    match await_ack(inner, &watcher, handle, manifest, &staged_window).await {
        AckOutcome::Resolved => AttemptOutcome::Done,
        AckOutcome::Screen => {
            // Stays registered: a late matching hook ACK upgrades it to
            // delivered_verified (the legal upgrade transition).
            let _ = advance(
                inner,
                handle,
                &[DeliveryState::Submitted],
                Step::to(DeliveryState::DeliveredUnverified)
                    .cause("screen_evidence")
                    .verified(VerifiedBy::Screen),
            );
            AttemptOutcome::Done
        }
        AckOutcome::Timeout => {
            unregister_ack(inner, handle);
            AttemptOutcome::Failed("ack_timeout".to_string())
        }
    }
}

/// Retry accounting. True means the caller should retry (state is
/// RetryQueued); false means the delivery ended in attention_required.
fn fail_attempt(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>, cause: &str) -> bool {
    let attempts = handle.state.lock().expect("handle state lock").attempts;
    let from = [
        DeliveryState::Pasting,
        DeliveryState::Staged,
        DeliveryState::Submitted,
        DeliveryState::RetryQueued,
    ];
    if attempts <= inner.cfg.delivery_retry_max {
        if handle.state() == DeliveryState::RetryQueued {
            return true; // submit_failed already moved it
        }
        advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::RetryQueued).cause(cause),
        )
    } else {
        let moved = advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::AttentionRequired)
                .cause(cause)
                .note(format!(
                    "delivery failed after {attempts} attempts: {cause}"
                )),
        );
        if moved {
            notify_attention(inner, handle, cause);
        }
        false
    }
}

fn notify_attention(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>, cause: &str) {
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        &format!("delivery to {} needs attention", handle.to),
        &format!("message {}: {cause}", handle.msg_id),
        Some(&handle.msg_id),
        Some(handle.session_idx),
    );
}

/// Quota parking: the in-flight delivery and everything queued behind it
/// park, the worker is flagged, and the admin is alerted once with the
/// reset hint. Nothing here ever re-queues (amendment f).
async fn park_recipient(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
    hint: Option<String>,
) {
    let hint = hint.unwrap_or_else(|| "quota exhausted".to_string());
    *worker.parked.lock().expect("parked lock") = Some(hint.clone());
    advance(
        inner,
        handle,
        &[DeliveryState::Gating],
        Step::to(DeliveryState::ParkedBlockedQuota)
            .cause("blocked_quota")
            .note(hint.clone()),
    );
    let drained: Vec<Arc<DeliveryHandle>> = worker
        .queue
        .lock()
        .expect("worker queue lock")
        .drain(..)
        .collect();
    for h in drained {
        advance(
            inner,
            &h,
            &[DeliveryState::Queued],
            Step::to(DeliveryState::ParkedBlockedQuota)
                .cause("blocked_quota")
                .note(hint.clone()),
        );
    }
    admin_notify(
        inner,
        NotifyLevel::Urgent,
        &format!("{} parked: quota exhausted", handle.to),
        &format!(
            "deliveries to {} are parked ({hint}); re-queue is an operator action",
            handle.to
        ),
        Some(&handle.msg_id),
        Some(handle.session_idx),
    );
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

enum GateOutcome {
    Proceed { manifest_id: String },
    Park { hint: Option<String> },
    Attention { cause: String },
}

/// The delivery gate, in spec order: pane resolution and liveness, mode,
/// fused state (quota park, modal decline-or-hold, working hold,
/// idle_with_input hold, idle proceed). Event-driven: holds wake on fused
/// state changes, pane field changes, and session reattach. The recompute
/// that admits a delivery runs immediately before pasting, so the gate
/// snapshot is fresher than any human keystroke round-trip.
async fn gate(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) -> GateOutcome {
    let mut declines: HashMap<String, u32> = HashMap::new();
    let mut notified_rules: HashSet<String> = HashSet::new();
    let mut last_hold: Option<String> = None;
    loop {
        // Subscribe before evaluating so nothing published mid-evaluation
        // is lost; evaluation itself is authoritative.
        let mut ev_rx = inner.events.subscribe();
        let watcher = inner.watcher_of(handle.session_idx);
        let mut pane_rx = watcher.as_ref().map(|w| w.subscribe());

        let hold = match &watcher {
            None => Some("session_detached".to_string()),
            Some(w) => {
                let Some(row) = w.pane(&handle.pane_id) else {
                    return GateOutcome::Attention {
                        cause: "no_such_pane".to_string(),
                    };
                };
                if row.dead {
                    return GateOutcome::Attention {
                        cause: "pane_dead".to_string(),
                    };
                }
                if row.in_mode {
                    // Human scrolling in copy-mode; %pane-mode-changed
                    // re-triggers via the pane event stream.
                    Some("pane_in_mode".to_string())
                } else {
                    let Some(manifest) =
                        fusion::bind_manifest(&inner.manifests, &row.current_command)
                    else {
                        return GateOutcome::Attention {
                            cause: "no_manifest".to_string(),
                        };
                    };
                    let manifest_id = manifest.agent.id.clone();
                    let Some(det) =
                        fusion::recompute_pane(inner, w, &handle.pane_id, true, "gate").await
                    else {
                        return GateOutcome::Attention {
                            cause: "no_such_pane".to_string(),
                        };
                    };
                    match det.state {
                        AgentState::Idle => {
                            gate_line(inner, handle, "proceed", Some(&det.decided_by), None);
                            return GateOutcome::Proceed { manifest_id };
                        }
                        AgentState::Dead => {
                            return GateOutcome::Attention {
                                cause: "pane_dead".to_string(),
                            };
                        }
                        AgentState::BlockedQuota => {
                            let hint = quota_hint(w, &handle.pane_id).await;
                            gate_line(
                                inner,
                                handle,
                                "park",
                                Some(&det.decided_by),
                                Some("blocked_quota"),
                            );
                            return GateOutcome::Park { hint };
                        }
                        AgentState::BlockedModal | AgentState::BlockedPermission => {
                            let rule = inner.manifests.get(&manifest_id).and_then(|m| {
                                m.rules
                                    .iter()
                                    .find(|r| r.id == det.decided_by && r.state.is_blocked())
                            });
                            match rule {
                                Some(r)
                                    if r.auto_dismiss
                                        && !r.decline_keys.is_empty()
                                        && *declines.get(&r.id).unwrap_or(&0) < MAX_DECLINES =>
                                {
                                    *declines.entry(r.id.clone()).or_insert(0) += 1;
                                    gate_line(inner, handle, "decline", Some(&r.id), None);
                                    let keys = r.decline_keys.clone();
                                    send_decline_keys(w, &handle.pane_id, &keys).await;
                                    // One-shot settle so the dismissal
                                    // renders before the re-check; the
                                    // decline count bounds this loop.
                                    tokio::time::sleep(DECLINE_SPACING).await;
                                    continue;
                                }
                                _ => {
                                    // Trust/permission prompts belong to the
                                    // human: hold and alert, never dismiss.
                                    let rule_id = rule
                                        .map(|r| r.id.clone())
                                        .unwrap_or_else(|| det.decided_by.clone());
                                    if notified_rules.insert(rule_id.clone()) {
                                        admin_notify(
                                            inner,
                                            NotifyLevel::ActionRequired,
                                            &format!("{} blocked: {rule_id}", handle.to),
                                            &format!(
                                                "delivery {} is held; rule {rule_id} needs a decision",
                                                handle.msg_id
                                            ),
                                            Some(&handle.msg_id),
                                            Some(handle.session_idx),
                                        );
                                    }
                                    Some(format!("blocked:{rule_id}"))
                                }
                            }
                        }
                        AgentState::Working => Some("working".to_string()),
                        // Human typing always wins.
                        AgentState::IdleWithInput => Some("idle_with_input".to_string()),
                        AgentState::Unknown => Some("unknown".to_string()),
                    }
                }
            }
        };
        if let Some(cause) = hold {
            if last_hold.as_deref() != Some(cause.as_str()) {
                gate_line(inner, handle, "hold", None, Some(&cause));
                last_hold = Some(cause);
            }
            wait_pane_change(&mut ev_rx, pane_rx.as_mut(), &handle.pane_id).await;
        }
    }
}

/// Manifest decline keys, in order, with spacing (amendment g: the keys
/// come from the manifest rule, never a generic Enter/Escape).
async fn send_decline_keys(watcher: &Arc<SessionWatcher>, pane_id: &str, keys: &[String]) {
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(DECLINE_SPACING).await;
        }
        if let Err(e) = watcher.client().send_keys(pane_id, &[key.as_str()]).await {
            warn!(pane = pane_id, error = %e, "decline key failed");
            return;
        }
    }
}

/// Parse the quota reset hint from the screen. Only the parsed phrase ever
/// leaves this function; raw captures stay out of the ledger.
async fn quota_hint(watcher: &Arc<SessionWatcher>, pane_id: &str) -> Option<String> {
    let screen = watcher.client().capture_pane(pane_id).await.ok()?;
    parse_reset_hint(&screen)
}

/// "Resets in 135h57m42s" -> "resets in 135h57m42s".
pub(crate) fn parse_reset_hint(screen: &str) -> Option<String> {
    let idx = screen.find("esets in ")?;
    let tail = &screen[idx + "esets in ".len()..];
    let token: String = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if token.is_empty() {
        None
    } else {
        Some(format!("resets in {token}"))
    }
}

/// Block until an event that could change the gate verdict for this pane:
/// a fused state change, a session attach/detach, or a pane field change
/// (mode, death, title, command). Lag counts as doubt and wakes too.
async fn wait_pane_change(
    ev_rx: &mut broadcast::Receiver<Event>,
    pane_rx: Option<&mut broadcast::Receiver<PaneEvent>>,
    pane_id: &str,
) {
    match pane_rx {
        Some(prx) => loop {
            tokio::select! {
                ev = ev_rx.recv() => if event_wakes(&ev, pane_id) { return },
                pe = prx.recv() => if pane_event_wakes(&pe, pane_id) { return },
            }
        },
        None => loop {
            let ev = ev_rx.recv().await;
            if event_wakes(&ev, pane_id) {
                return;
            }
        },
    }
}

fn event_wakes(ev: &Result<Event, broadcast::error::RecvError>, pane_id: &str) -> bool {
    match ev {
        Ok(e) => match e.event.as_str() {
            "state" => e.data["pane_id"] == pane_id,
            "session" => true,
            _ => false,
        },
        Err(broadcast::error::RecvError::Lagged(_)) => true,
        Err(broadcast::error::RecvError::Closed) => true,
    }
}

fn pane_event_wakes(pe: &Result<PaneEvent, broadcast::error::RecvError>, pane_id: &str) -> bool {
    match pe {
        Ok(PaneEvent::PaneChanged { id, .. }) => id == pane_id,
        Ok(PaneEvent::PaneRemoved(id)) => id == pane_id,
        Ok(PaneEvent::Disconnected) => true,
        Ok(_) => false,
        Err(broadcast::error::RecvError::Lagged(_)) => true,
        Err(broadcast::error::RecvError::Closed) => true,
    }
}

// ---------------------------------------------------------------------------
// Inject and verify
// ---------------------------------------------------------------------------

/// Paste the payload and verify the composer staged it. Returns the
/// composer-window snapshot used later by the screen ACK tier.
///
/// The payload travels through a file under the 0700 cyclops home (never
/// the shared system temp dir) into a per-delivery unique buffer
/// (amendment e), pasted with -p (bracketed when the app opted in, F17)
/// and -d so the buffer does not linger server-global.
async fn inject(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
    manifest: &cyclops_manifest::Manifest,
) -> Result<String, String> {
    let client = watcher.client();
    let buffer = format!(
        "cyc-{}-{}",
        std::process::id(),
        inner.engine.buffer_seq.fetch_add(1, Ordering::Relaxed)
    );
    let spool = inner.cfg.home.join("spool");
    if !spool.exists() {
        use std::os::unix::fs::DirBuilderExt;
        if let Err(e) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&spool)
        {
            warn!(error = %e, "cannot create spool dir");
            return Err("spool_failed".to_string());
        }
    }
    let path = spool.join(&buffer);
    if let Err(e) = tokio::fs::write(&path, handle.payload.as_bytes()).await {
        warn!(error = %e, "cannot write spool file");
        return Err("spool_failed".to_string());
    }
    let load = client
        .command(&format!(
            "load-buffer -b {} {}",
            quote_arg(&buffer),
            quote_arg(&path.to_string_lossy())
        ))
        .await;
    // Message content must not linger on disk.
    let _ = tokio::fs::remove_file(&path).await;
    if let Err(e) = load {
        warn!(id = %handle.msg_id, error = %e, "load-buffer failed");
        return Err("paste_failed".to_string());
    }
    if let Err(e) = client
        .paste_buffer(&buffer, &handle.pane_id, true, true)
        .await
    {
        warn!(id = %handle.msg_id, error = %e, "paste-buffer failed");
        return Err("paste_failed".to_string());
    }

    // Composer verification is the gate (amendment b): bracketed-paste
    // degradation is not observable up front through tmux 3.6a.
    let patterns = verify_patterns(manifest, &handle.msg_id);
    let mut last_delay = 0;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        match client.capture_pane(&handle.pane_id).await {
            Ok(screen) => {
                let region = bottom_window(&screen, VERIFY_REGION);
                if patterns_hit(&region, &patterns) {
                    return Ok(bottom_window(&screen, COMPOSER_WINDOW));
                }
            }
            Err(e) => debug!(error = %e, "verify capture failed"),
        }
    }
    Err("verify_failed".to_string())
}

/// Substituted staging patterns; the message id is always one of them.
fn verify_patterns(manifest: &cyclops_manifest::Manifest, msg_id: &str) -> Vec<String> {
    let mut out: Vec<String> = manifest
        .injection
        .verify_pattern
        .iter()
        .map(|p| p.replace("<message_id>", msg_id))
        .collect();
    if out.is_empty() {
        out.push(msg_id.to_string());
    }
    out
}

/// Last `n` non-empty lines of a capture, top-down, joined.
fn bottom_window(screen: &str, n: usize) -> String {
    let mut lines: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(n)
        .collect();
    lines.reverse();
    lines.join("\n")
}

fn patterns_hit(region: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| region.contains(p.as_str()))
}

// ---------------------------------------------------------------------------
// ACK tiers
// ---------------------------------------------------------------------------

enum AckOutcome {
    /// The matcher resolved it (delivered_verified is on the handle).
    Resolved,
    /// Screen evidence: marker left the composer and the pane moved.
    Screen,
    /// Neither tier inside the deadline.
    Timeout,
}

/// Tier 1: the manifest hook ACK inside ack_timeout_ms. Tier 2: screen
/// evidence until the 5s deadline, checked on pane events and bounded
/// one-shot checkpoints. A hook ACK is accepted at any point.
async fn await_ack(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
    manifest: &cyclops_manifest::Manifest,
    staged_window: &str,
) -> AckOutcome {
    let submit_at = Instant::now();
    let deadline = submit_at + SCREEN_ACK_DEADLINE;
    let tier1 = manifest.hooks.ack.is_some() && manifest.hooks.ack_payload_field.is_some();
    let patterns = verify_patterns(manifest, &handle.msg_id);
    let mut ev_rx = inner.events.subscribe();
    let mut pane_rx = watcher.subscribe();
    let mut working_seen = false;
    let mut output_seen = false;

    if tier1 {
        let hook_deadline = submit_at + Duration::from_millis(inner.cfg.ack_timeout_ms);
        loop {
            tokio::select! {
                _ = handle.ack.notified() => {
                    if handle.state() == DeliveryState::DeliveredVerified {
                        return AckOutcome::Resolved;
                    }
                }
                _ = tokio::time::sleep_until(hook_deadline) => break,
                ev = ev_rx.recv() => track_state_event(&ev, &handle.pane_id, &mut working_seen),
                pe = pane_rx.recv() => track_pane_event(&pe, &handle.pane_id, &mut output_seen),
            }
        }
    }

    let checkpoints: Vec<Instant> = ACK_CHECKPOINTS_MS
        .iter()
        .map(|ms| submit_at + Duration::from_millis(*ms))
        .filter(|t| *t > Instant::now())
        .collect();
    let mut next = 0;
    loop {
        let target = checkpoints.get(next).copied().unwrap_or(deadline);
        tokio::select! {
            _ = handle.ack.notified() => {
                if handle.state() == DeliveryState::DeliveredVerified {
                    return AckOutcome::Resolved;
                }
            }
            _ = tokio::time::sleep_until(target) => {
                next += 1;
                if screen_ack(watcher, handle, manifest, &patterns, staged_window, working_seen, output_seen).await {
                    return AckOutcome::Screen;
                }
                if Instant::now() >= deadline {
                    return AckOutcome::Timeout;
                }
            }
            ev = ev_rx.recv() => track_state_event(&ev, &handle.pane_id, &mut working_seen),
            pe = pane_rx.recv() => track_pane_event(&pe, &handle.pane_id, &mut output_seen),
        }
    }
}

fn track_state_event(
    ev: &Result<Event, broadcast::error::RecvError>,
    pane_id: &str,
    working_seen: &mut bool,
) {
    if let Ok(e) = ev {
        if e.event == "state" && e.data["pane_id"] == pane_id && e.data["state"] == "working" {
            *working_seen = true;
        }
    }
}

fn track_pane_event(
    pe: &Result<PaneEvent, broadcast::error::RecvError>,
    pane_id: &str,
    output_seen: &mut bool,
) {
    if let Ok(PaneEvent::OutputActivity { pane_id: p, .. }) = pe {
        if p == pane_id {
            *output_seen = true;
        }
    }
}

/// Screen evidence for tier 2: the marker left the composer and the pane
/// showed turn-start evidence.
///
/// "Left the composer" is manifest-driven: the marker still sits in the
/// composer only when an idle_with_input rule identifies a composer line
/// that carries it (staged-but-unsubmitted text, e.g. Claude's collapsed
/// paste on the `❯` line). Manifests without an idle_with_input rule
/// cannot pin staged text, so the changed-window signal decides alone.
///
/// A changed composer window is itself output evidence read through the
/// screen sensor; %output events can be swallowed by the watcher's
/// per-pane rate limit for single short bursts (MEASURED: a cat pane's
/// echoed submit stays under the 100ms floor).
async fn screen_ack(
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
    manifest: &cyclops_manifest::Manifest,
    patterns: &[String],
    staged_window: &str,
    working_seen: bool,
    output_seen: bool,
) -> bool {
    let Ok(screen) = watcher.client().capture_pane(&handle.pane_id).await else {
        return false;
    };
    let window = bottom_window(&screen, COMPOSER_WINDOW);
    let changed = window != staged_window;
    !marker_in_composer(manifest, &screen, patterns) && (changed || working_seen || output_seen)
}

/// True when a manifest idle_with_input rule matches a line in its own
/// region that carries one of the substituted patterns: the staged text is
/// still sitting in the composer, so the submit did not consume it.
fn marker_in_composer(
    manifest: &cyclops_manifest::Manifest,
    screen: &str,
    patterns: &[String],
) -> bool {
    let bottom_up: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .collect();
    for rule in manifest
        .rules
        .iter()
        .filter(|r| r.state == AgentState::IdleWithInput)
    {
        let cyclops_manifest::Region::BottomNonEmptyLines(n) = rule.region else {
            continue;
        };
        for line in bottom_up.iter().take(n) {
            if !patterns_hit(line, patterns) {
                continue;
            }
            let composer_line = rule
                .matcher
                .line_regex
                .iter()
                .chain(rule.any.iter().flat_map(|m| m.line_regex.iter()))
                .any(|r| r.is_match(line));
            if composer_line {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// ACK registry (used by the matcher in ack.rs)
// ---------------------------------------------------------------------------

fn register_ack(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) {
    let mut acks = inner.engine.acks.lock().expect("acks lock");
    let entry = acks.entry(handle.pane_id.clone()).or_default();
    entry.retain(|h| {
        !Arc::ptr_eq(h, handle)
            && matches!(
                h.state(),
                DeliveryState::Submitted | DeliveryState::DeliveredUnverified
            )
    });
    if entry.len() >= ACK_REGISTRY_CAP {
        entry.remove(0);
    }
    entry.push(Arc::clone(handle));
}

fn unregister_ack(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) {
    if let Some(entry) = inner
        .engine
        .acks
        .lock()
        .expect("acks lock")
        .get_mut(&handle.pane_id)
    {
        entry.retain(|h| !Arc::ptr_eq(h, handle));
    }
}

/// Deliveries on a pane a hook ACK could match right now.
pub(crate) fn ack_candidates(inner: &Arc<Inner>, pane_id: &str) -> Vec<Arc<DeliveryHandle>> {
    inner
        .engine
        .acks
        .lock()
        .expect("acks lock")
        .get(pane_id)
        .map(|v| v.to_vec())
        .unwrap_or_default()
}

/// Resolve a hook ACK onto a delivery: verify a submitted one, or upgrade
/// a screen-verified one (the legal DeliveredUnverified -> Verified move
/// that keeps receipts honest). Racing ahead of the Submitted line sets
/// the early-ack flag the worker consumes.
pub(crate) fn resolve_hook_ack(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) -> bool {
    let state = handle.state();
    let moved = match state {
        DeliveryState::Submitted => advance(
            inner,
            handle,
            &[DeliveryState::Submitted],
            Step::to(DeliveryState::DeliveredVerified)
                .cause("hook_ack")
                .verified(VerifiedBy::Hook),
        ),
        DeliveryState::DeliveredUnverified => advance(
            inner,
            handle,
            &[DeliveryState::DeliveredUnverified],
            Step::to(DeliveryState::DeliveredVerified)
                .cause("hook_ack_upgrade")
                .verified(VerifiedBy::Hook),
        ),
        DeliveryState::Staged => {
            handle.early_ack.store(true, Ordering::SeqCst);
            true
        }
        _ => false,
    };
    handle.ack.notify_waiters();
    moved
}

// ---------------------------------------------------------------------------
// agent.wait
// ---------------------------------------------------------------------------

/// Wait for a pane's fused state to satisfy `until`. Event-driven off the
/// state stream; returns (state, timed_out).
pub(crate) async fn wait_until(
    inner: &Arc<Inner>,
    pane_id: &str,
    until: WaitUntil,
    timeout: Duration,
) -> (AgentState, bool) {
    let mut rx = inner.events.subscribe();
    let deadline = Instant::now() + timeout;
    let mut state = inner.cached_state(pane_id);
    // Done means "the turn started by our delivery ended": a working phase
    // must be observed (or already running) before a non-working state
    // satisfies the wait.
    let mut working_seen = state == AgentState::Working;
    loop {
        let satisfied = match until {
            WaitUntil::Idle => state == AgentState::Idle,
            WaitUntil::Blocked => state.is_blocked(),
            WaitUntil::Done => working_seen && state != AgentState::Working,
        };
        if satisfied {
            return (state, false);
        }
        let ev = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return (state, true),
            ev = rx.recv() => ev,
        };
        match ev {
            Ok(e) if e.event == "state" && e.data["pane_id"] == pane_id => {
                if let Ok(s) = serde_json::from_value::<AgentState>(e.data["state"].clone()) {
                    state = s;
                    if state == AgentState::Working {
                        working_seen = true;
                    }
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Reconcile on doubt: re-read the cache.
                state = inner.cached_state(pane_id);
                if state == AgentState::Working {
                    working_seen = true;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return (state, true),
        }
    }
}

/// agent.wait entry for the socket server.
pub(crate) async fn agent_wait(
    inner: &Arc<Inner>,
    params: cyclops_proto::AgentWaitParams,
) -> Result<Value, WireError> {
    let Some((_, pane_id)) = inner.resolve_recipient(&params.target) else {
        return Err(wire_err(
            "no_such_target",
            format!("no such target {:?}", params.target),
        ));
    };
    let timeout = params
        .timeout_ms
        .unwrap_or(WAIT_DEFAULT_MS)
        .min(WAIT_MAX_MS);
    let started = Instant::now();
    let (state, timed_out) = wait_until(
        inner,
        &pane_id,
        params.until,
        Duration::from_millis(timeout),
    )
    .await;
    Ok(json!({
        "target": params.target,
        "pane_id": pane_id,
        "state": state,
        "timed_out": timed_out,
        "waited_ms": started.elapsed().as_millis() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every transition the pipeline can perform must be legal in the
    /// frozen state machine. If the proto table changes, this fails before
    /// any integration test does.
    #[test]
    fn pipeline_transitions_are_legal() {
        for (from, to) in PIPELINE_TRANSITIONS {
            assert!(
                from.can_transition_to(*to),
                "pipeline performs illegal transition {from:?} -> {to:?}"
            );
        }
    }

    #[test]
    fn payload_shape_matches_spec() {
        let p = render_payload(
            "m-3f9c2a",
            "codex",
            "Review the rate limiter",
            "please",
            false,
        );
        let lines: Vec<&str> = p.lines().collect();
        assert_eq!(
            lines[0],
            "[cyclops m-3f9c2a] FROM: codex  SUBJECT: Review the rate limiter"
        );
        assert_eq!(lines[1], "please");
        assert_eq!(
            lines[2],
            "Reply with: cyclops send codex --subject \"...\" [--body ... | --body-file -]"
        );
        assert!(
            !p.ends_with('\n'),
            "no trailing newline; submit is separate"
        );
    }

    #[test]
    fn fyi_payload_has_no_reply_hint() {
        let p = render_payload("m-1", "admin", "heads up", "body", true);
        assert!(!p.contains("Reply with:"));
    }

    #[test]
    fn empty_body_payload_is_header_plus_hint() {
        let p = render_payload("m-1", "admin", "s", "", false);
        assert_eq!(p.lines().count(), 2);
    }

    #[test]
    fn verify_patterns_substitute_and_default() {
        let m = cyclops_manifest::Manifest::parse(
            r#"
[agent]
id = "x"
display_name = "x"
[injection]
verify_pattern = ["<message_id>", "Pasted text"]
"#,
            std::path::Path::new("x.toml"),
        )
        .unwrap();
        let pats = verify_patterns(&m, "m-ab12");
        assert_eq!(pats, vec!["m-ab12".to_string(), "Pasted text".to_string()]);

        let empty = cyclops_manifest::Manifest::parse(
            "[agent]\nid = \"y\"\ndisplay_name = \"y\"\n",
            std::path::Path::new("y.toml"),
        )
        .unwrap();
        assert_eq!(verify_patterns(&empty, "m-1"), vec!["m-1".to_string()]);
    }

    #[test]
    fn marker_in_composer_is_manifest_driven() {
        // Claude-style: staged input matches the idle_with_input line rule.
        let m = cyclops_manifest::Manifest::parse(
            r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 950
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
"#,
            std::path::Path::new("c.toml"),
        )
        .unwrap();
        let patterns = vec!["m-ab12".to_string()];
        // Staged and unsubmitted: the composer line carries the marker.
        let staged = "transcript\n❯ [cyclops m-ab12] hello\n? for shortcuts";
        assert!(marker_in_composer(&m, staged, &patterns));
        // Submitted: composer cleared, marker only in the transcript.
        let submitted = "old [cyclops m-ab12] text\n❯ \n? for shortcuts";
        assert!(!marker_in_composer(&m, submitted, &patterns));
        // A manifest with no idle_with_input rule can never pin the marker.
        let bare = cyclops_manifest::Manifest::parse(
            "[agent]\nid = \"x\"\ndisplay_name = \"x\"\n",
            std::path::Path::new("x.toml"),
        )
        .unwrap();
        assert!(!marker_in_composer(&bare, staged, &patterns));
    }

    #[test]
    fn bottom_window_takes_non_empty_tail() {
        let screen = "a\n\nb\nc\n   \nd\n";
        assert_eq!(bottom_window(screen, 2), "c\nd");
        assert_eq!(bottom_window(screen, 10), "a\nb\nc\nd");
    }

    #[test]
    fn reset_hint_parses_and_stays_short() {
        let screen = "junk\n⚠ Individual quota reached. Resets in 135h57m42s.\nmore";
        assert_eq!(
            parse_reset_hint(screen).as_deref(),
            Some("resets in 135h57m42s")
        );
        assert_eq!(parse_reset_hint("no hint here"), None);
    }

    #[test]
    fn mint_ids_are_unique_and_shaped() {
        let e = Engine::new();
        let a = e.mint_msg_id();
        let b = e.mint_msg_id();
        assert_ne!(a, b);
        assert!(a.starts_with("m-") && a.len() == 8, "{a}");
    }
}
