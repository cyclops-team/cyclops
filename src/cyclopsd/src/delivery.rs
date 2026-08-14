//! The delivery pipeline (docs/development/DELIVERY.md is the spec, and the flow with
//! its decision points drawn is docs/development/ARCHITECTURE.md).
//!
//! One worker per target pane; deliveries to one recipient are strictly
//! FIFO. Every state transition appends a ledger line and emits an event.
//! Failures queue or park; they never drop (limbo is a bug).
//!
//! Four things live here that read like they belong elsewhere, and each
//! one is here because it needs the same handle the pipeline holds:
//! `admin_notify` (a ping usually points at a delivery), `close_limbo`
//! (the restart sweep over chains this file left open), `agent_wait` and
//! `wait_pinned` (send-and-wait pins on the pid the submit went to), and
//! `About`, the item a ping names so a reader can stop showing it.
//!
//! What is NOT decided here: whether a pane is idle (`fusion.rs`), which
//! keys dismiss a modal (`cyclops-manifest` data), whether a finished
//! delivery still needs a human (`cyclops_proto::attention`), and how any
//! of it is worded for a person (the CLI).
//!
//! Zero-polling shape: workers sleep on queue notifies and wake on watcher
//! or fusion events. Every timer is a one-shot tied to one delivery: the
//! paste verification re-reads, the tier-1 ACK window, the screen-evidence
//! checkpoints, the decline-key spacing, the gate's single wedged-hold
//! ping, and the two deadlines a caller asked for (`receipt_block_ms`,
//! and `timeout_ms` on a wait). Nothing runs on an interval.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use cyclops_manifest::Manifest;
use cyclops_proto::{
    AgentState, Delivery, DeliveryReceipt, DeliveryState, Event, Kind, LedgerLine, MsgSendParams,
    MsgSendResult, NotifyLevel, VerifiedBy, WaitUntil, WireError,
};
use cyclops_tmux::{ControlClient, PaneEvent, PaneRow, SessionWatcher};
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
    /// Session files this delivery's state lines append to. Normally just
    /// the hosting session; an unresolvable recipient records into every
    /// session file that carries the msg line, so each file stays a
    /// complete stream.
    ledger_sessions: Vec<usize>,
    payload: String,
    state: StdMutex<HandleState>,
    state_tx: watch::Sender<DeliveryState>,
    /// Wakes the worker when the ACK matcher resolved this delivery.
    ack: Notify,
    /// Hook ACK that raced ahead of the Submitted transition; consumed by
    /// the worker right after submitting.
    early_ack: AtomicBool,
    /// Turn evidence at or after this delivery's submit. Recorded by the
    /// ACK waiter; anchors send-and-wait `done` so a working phase that
    /// predates the delivery never counts.
    working_seen: AtomicBool,
    /// pane_pid of the occupant this delivery was submitted to, recorded
    /// right before the submit key. Send-and-wait pins its wait on THIS
    /// occupant, not whoever lives in the pane when the wait starts; an
    /// impostor that swaps in between must read occupant_changed, never a
    /// report about itself. 0 until a submit happened.
    submitted_pid: AtomicI32,
}

struct HandleState {
    state: DeliveryState,
    attempts: u32,
    verified_by: Option<VerifiedBy>,
    cause: Option<String>,
    /// Human hint carried into receipts (quota reset, attention cause).
    note: Option<String>,
    /// Normalized gate hold token for the in-flight head, if any.
    held_by: Option<String>,
}

impl DeliveryHandle {
    fn new(
        msg_id: &str,
        to: &str,
        pane_id: &str,
        session_idx: usize,
        payload: String,
    ) -> Arc<Self> {
        Self::with_ledger_sessions(msg_id, to, pane_id, session_idx, vec![session_idx], payload)
    }

    fn with_ledger_sessions(
        msg_id: &str,
        to: &str,
        pane_id: &str,
        session_idx: usize,
        ledger_sessions: Vec<usize>,
        payload: String,
    ) -> Arc<Self> {
        let (state_tx, _) = watch::channel(DeliveryState::Queued);
        Arc::new(DeliveryHandle {
            msg_id: msg_id.to_string(),
            to: to.to_string(),
            pane_id: pane_id.to_string(),
            session_idx,
            ledger_sessions,
            payload,
            state: StdMutex::new(HandleState {
                state: DeliveryState::Queued,
                attempts: 0,
                verified_by: None,
                cause: None,
                note: None,
                held_by: None,
            }),
            state_tx,
            ack: Notify::new(),
            early_ack: AtomicBool::new(false),
            working_seen: AtomicBool::new(false),
            submitted_pid: AtomicI32::new(0),
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
        Option<String>,
    ) {
        let st = self.state.lock().expect("handle state lock");
        (
            st.state,
            st.attempts,
            st.verified_by,
            st.cause.clone(),
            st.note.clone(),
            st.held_by.clone(),
        )
    }

    fn set_hold(&self, hold: Option<&str>) {
        self.state.lock().expect("handle state lock").held_by = hold.map(str::to_string);
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

/// Write one delivery transition to the record: the `Kind::State` line in
/// every named session ledger, then the matching `delivery-state` event.
/// One writer for both the running pipeline ([`advance`]) and the restart
/// closure ([`close_limbo`]): each used to build these by hand, and the
/// restart event had already lost three fields (`verified_by`, `attempts`,
/// `note`) the live one carried.
#[allow(clippy::too_many_arguments)]
fn emit_delivery_state(
    inner: &Arc<Inner>,
    sessions: &[usize],
    msg_id: &str,
    to: &str,
    from: DeliveryState,
    next: DeliveryState,
    cause: Option<&str>,
    note: Option<&str>,
    record: &Delivery,
) -> Option<u64> {
    let line = LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id: msg_id.to_string(),
        ts: 0,
        kind: Kind::State,
        from: "cyclopsd".to_string(),
        to: vec![to.to_string()],
        subject: None,
        body: None,
        reply_to: None,
        deliveries: vec![record.clone()],
        data: Some(json!({
            "to": to,
            "from": from,
            "to_state": next,
            "cause": cause,
        })),
    };
    // Every session file carrying this delivery's msg line gets the state
    // line too; a per-session ledger is a complete stream on its own.
    let mut seq = None;
    for idx in sessions {
        let s = inner.append_line(*idx, line.clone());
        if seq.is_none() {
            seq = s;
        }
    }
    inner.emit(
        "delivery-state",
        json!({
            "id": msg_id,
            "to": to,
            "from": from,
            "to_state": next,
            "cause": cause,
            "verified_by": record.verified_by,
            "attempts": record.attempts,
            "note": note,
        }),
        seq,
    );
    seq
}

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
        if step.next != DeliveryState::Gating {
            st.held_by = None;
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
    emit_delivery_state(
        inner,
        &handle.ledger_sessions,
        &handle.msg_id,
        &handle.to,
        from,
        step.next,
        step.cause,
        step.note.as_deref(),
        &record,
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

/// What a ping is ABOUT: the attention item a reader can go and clear.
///
/// A ping says a human is needed but is not itself a state, so nothing
/// ever transitions it and no register can hold it. Naming its item is
/// what lets a reader's stream tell a ping that still stands from one
/// whose subject already resolved (cyclops-ui `App::admits`). Additive on
/// the wire: a ping that names nothing omits both fields, and a reader
/// that predates them ignores them.
#[derive(Default)]
pub(crate) struct About {
    /// The pane whose blocked state raised the ping.
    pane_id: Option<String>,
    /// The recipient of the delivery the ping is about. That delivery's
    /// id is the ping's own `msg_id`, so this pairs with `Some(msg_id)`.
    to: Option<String>,
    /// Every delivery a ping about SEVERAL of them names, when one ping
    /// covers a batch. The ping's own `msg_id` can only name one, and the
    /// restart closure ends a whole run's worth at once.
    deliveries: Vec<DeliveryRef>,
}

/// One delivery a batch ping points at, keyed the way the register keys
/// it: recipient plus message.
///
/// Named fields, not a pair: both are strings, they sit next to each
/// other, and a transposition would compile and then quietly point every
/// ping at nothing.
pub(crate) struct DeliveryRef {
    pub(crate) to: String,
    pub(crate) msg_id: String,
}

impl About {
    /// A ping about a pane a human must unblock.
    pub(crate) fn pane(pane_id: &str) -> About {
        About {
            pane_id: Some(pane_id.to_string()),
            ..About::default()
        }
    }

    /// A ping about one delivery to `to`. Pass the message id as `msg_id`
    /// or the ping names a recipient without saying which delivery.
    pub(crate) fn delivery(to: &str) -> About {
        About {
            to: Some(to.to_string()),
            ..About::default()
        }
    }

    /// A ping about many deliveries at once. A reader holds it against
    /// the register per item and shows it while ANY of them still stands,
    /// so one summary line cannot outlive the whole batch it summarizes.
    pub(crate) fn deliveries(deliveries: Vec<DeliveryRef>) -> About {
        About {
            deliveries,
            ..About::default()
        }
    }
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
    about: About,
) -> Option<u64> {
    let id = msg_id
        .map(str::to_string)
        .unwrap_or_else(|| inner.mint_event_id());
    // The item the ping points at, written once and worn by both the
    // ledger line's data and the live event, so a reader replaying the
    // record and one on the push read the same ping.
    let mut about_fields = serde_json::Map::new();
    if let Some(pane_id) = &about.pane_id {
        about_fields.insert("pane_id".into(), json!(pane_id));
    }
    if let Some(to) = &about.to {
        about_fields.insert("to".into(), json!(to));
    }
    if !about.deliveries.is_empty() {
        // The batch form of the `to` field above, spelled the same way:
        // recipient plus message id, which is the register's key for the
        // delivery half. Additive and absent when the ping names one item
        // or none.
        about_fields.insert(
            "deliveries".into(),
            json!(about
                .deliveries
                .iter()
                .map(|d| json!({"to": d.to, "id": d.msg_id}))
                .collect::<Vec<_>>()),
        );
    }
    let with_about = |mut v: Value| {
        if let Value::Object(map) = &mut v {
            map.extend(about_fields.clone());
        }
        v
    };
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
        data: Some(with_about(json!({"event": "admin_notify", "level": level}))),
    };
    let sessions: Vec<usize> = match session_idx {
        Some(i) => vec![i],
        None => (0..inner.session_count()).collect(),
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
        with_about(json!({"id": id, "level": level, "subject": subject, "body": body})),
        first_seq,
    );
    first_seq
}

// ---------------------------------------------------------------------------
// Restart-limbo closure
// ---------------------------------------------------------------------------

/// Close deliveries a previous daemon run left unresolved (GOALS: limbo is
/// a bug). Runs once at boot over the replayed session ledgers: any
/// delivery whose latest state is still in-flight gets a state line to
/// attention_required (cause: daemon_restart), and ONE aggregated
/// admin.notify lists everything closed.
///
/// A msg line's `hosted` list names the recipients whose chains live in
/// that file, so a chain recorded in another session's file is never
/// falsely closed here; a delivery that died before its first state line
/// still closes through its hosted msg record.
pub(crate) fn close_limbo(inner: &Arc<Inner>, replayed: &[(usize, Vec<LedgerLine>)]) {
    let mut closed: Vec<String> = Vec::new();
    // The same closures as identities, so the one ping can name them and
    // a reader can hold it to the register (cyclops-ui `App::admits`).
    let mut named: Vec<DeliveryRef> = Vec::new();
    for (idx, lines) in replayed {
        // (msg id, recipient) -> (latest state, attempts).
        let mut chains: HashMap<(String, String), (DeliveryState, u32)> = HashMap::new();
        for line in lines {
            match line.kind {
                Kind::Msg | Kind::Fyi => {
                    // `hosted` names the recipients whose chains live in
                    // this file. Ledgers from before the field existed were
                    // single-file: a msg line with no hosted list hosts
                    // every recipient it names, so those chains still get
                    // closed instead of dangling forever.
                    let hosted: Option<HashSet<&str>> = line
                        .data
                        .as_ref()
                        .and_then(|d| d.get("hosted"))
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).collect());
                    for d in &line.deliveries {
                        let is_hosted = hosted.as_ref().is_none_or(|h| h.contains(d.to.as_str()));
                        if is_hosted {
                            chains
                                .entry((line.id.clone(), d.to.clone()))
                                .or_insert((d.state, d.attempts));
                        }
                    }
                }
                Kind::State => {
                    let Some(data) = &line.data else { continue };
                    let (Some(to), Ok(state)) = (
                        data["to"].as_str(),
                        serde_json::from_value::<DeliveryState>(data["to_state"].clone()),
                    ) else {
                        continue; // fused-state line, not a delivery line
                    };
                    let attempts = line.deliveries.first().map(|d| d.attempts).unwrap_or(0);
                    chains.insert((line.id.clone(), to.to_string()), (state, attempts));
                }
                _ => {}
            }
        }
        let mut dangling: Vec<((String, String), (DeliveryState, u32))> = chains
            .into_iter()
            .filter(|(_, (state, _))| !receipt_resolved(*state))
            .collect();
        dangling.sort_by(|a, b| a.0.cmp(&b.0));
        for ((id, to), (state, attempts)) in dangling {
            let record = Delivery {
                to: to.clone(),
                state: DeliveryState::AttentionRequired,
                verified_by: None,
                attempts,
                ts: unix_ms(),
                cause: Some("daemon_restart".to_string()),
            };
            emit_delivery_state(
                inner,
                &[*idx],
                &id,
                &to,
                state,
                DeliveryState::AttentionRequired,
                Some("daemon_restart"),
                None,
                &record,
            );
            closed.push(format!("{id} -> {to}"));
            named.push(DeliveryRef {
                to: to.clone(),
                msg_id: id.clone(),
            });
        }
    }
    if closed.is_empty() {
        return;
    }
    closed.sort();
    closed.dedup();
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        "deliveries interrupted by daemon restart",
        &format!(
            "closed as attention_required (cause: daemon_restart): {}",
            closed.join(", ")
        ),
        None,
        None,
        // Every delivery this closed, by name. One ping over many is
        // still a ping about each of them: it claims a human is needed,
        // so a calm stream has to be able to ask the register whether any
        // of them still does. Naming nothing is what put a closed eye
        // over "action required" once the batch had been dealt with.
        About::deliveries(named),
    );
}

// ---------------------------------------------------------------------------
// msg.send
// ---------------------------------------------------------------------------

/// Render the injected payload. The daemon builds the envelope; nothing in
/// the request body can forge it (sender identity is structural).
///
/// The reply line is dropped for two senders, for the same reason both
/// times: the command would not work.
///
/// - An `fyi` expects no answer, and offering one invites a reply the
///   sender did not ask for.
/// - `admin` is the operator, and [`cyclops_proto::label`] reserves that
///   name so no pane can ever hold it. `cyclops send admin` therefore has
///   no target and fails with `no_such_target`, every time. Pasting it into
///   an agent's composer spends a turn teaching the agent that the obvious
///   move fails. Saying nothing is honest; a route back to the operator is
///   the admin inbox, which does not exist yet.
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
    if !fyi && from != cyclops_proto::label::ADMIN {
        lines.push(format!("Reply: cyclops send {from} --subject \"...\""));
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
    if inner.session_count() == 0 {
        return Err(wire_err("no_such_target", "no sessions are watched"));
    }
    let typed = expand_recipients(inner, &params.to)?;

    // Resolve each recipient before writing the msg line, and canonicalize
    // the resolved ones to their ledger name: the pane's label, or the
    // pane id when unlabeled. "%1" and its label are the same recipient;
    // the record carries one name, so history filters match however the
    // sender addressed it. Unresolvable names stay as typed (the
    // attention_required record should show what was asked for).
    let mut resolved: Vec<(String, Option<(usize, String)>)> = Vec::new();
    let mut canonical_seen: HashSet<String> = HashSet::new();
    for n in &typed {
        let target = inner.resolve_recipient(n);
        let name = match &target {
            Some((_, pane_id)) => inner.label_of(pane_id).unwrap_or_else(|| pane_id.clone()),
            None => n.clone(),
        };
        if canonical_seen.insert(name.clone()) {
            resolved.push((name, target));
        }
    }
    let names: Vec<String> = resolved.iter().map(|(n, _)| n.clone()).collect();

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
    // Each copy names the recipients whose delivery chains that file hosts,
    // so a restart can tell a chain that belongs elsewhere from one that
    // died before its first state line.
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
        let mut copy = line.clone();
        let mut hosted: Vec<&str> = resolved
            .iter()
            .filter(|(_, r)| r.as_ref().is_some_and(|(i, _)| i == idx))
            .map(|(n, _)| n.as_str())
            .collect();
        // Unresolvable recipients record their chain in every involved
        // file; every file hosts them.
        hosted.extend(
            resolved
                .iter()
                .filter(|(_, r)| r.is_none())
                .map(|(n, _)| n.as_str()),
        );
        copy.data = Some(json!({ "hosted": hosted }));
        let seq = inner.append_line(*idx, copy);
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
                // Gate step 1: unresolvable recipient needs a human. The
                // resolution line lands in every file that carries the msg
                // line, never a session the message does not involve.
                let handle = DeliveryHandle::with_ledger_sessions(
                    &msg_id,
                    name,
                    "",
                    involved[0],
                    involved.clone(),
                    String::new(),
                );
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
                    About::delivery(name),
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
                    // Block for this receipt only when the worker starts on
                    // it now AND the gate answers without waiting on anyone.
                    let first_in_line = !worker.busy.load(Ordering::SeqCst)
                        && worker.queue.lock().expect("worker queue lock").is_empty();
                    let answers_now = gate_answers_now(inner, *session_idx, pane_id);
                    // The worker is woken asynchronously. Seed the head's
                    // first hold disposition before enqueueing it, so a
                    // receipt cannot race the worker between queue insertion
                    // and its first gate evaluation and report `queued · 0
                    // ahead` for a delivery already held on the target.
                    if first_in_line && !answers_now {
                        handle.set_hold(initial_hold(inner, *session_idx, pane_id));
                    }
                    worker
                        .queue
                        .lock()
                        .expect("worker queue lock")
                        .push_back(Arc::clone(&handle));
                    worker.notify.notify_one();
                    if first_in_line && answers_now {
                        blocking.push(Arc::clone(&handle));
                    }
                }
                handles.push(handle);
            }
        }
    }

    // Receipts: block only where the verdict is coming, capped by
    // receipt_block_ms.
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

    // Send-and-wait composes agent.wait onto the same call: the wait
    // starts only AFTER the delivery reaches a resolved state
    // (DELIVERY.md), so `done` can never be satisfied by a turn that
    // predates the delivery. A delivery that ends anywhere but delivered
    // has no turn to watch; its entry reports the delivery state instead
    // of a fabricated wait result. Every entry carries the same
    // {outcome, state, waited_ms} shape agent.wait resolves with.
    if let Some(spec) = &params.wait {
        // Test seam between delivery resolution and the wait, so a test can
        // swap the pane occupant deterministically. No-op in production.
        inject_pause(inner, "pre_wait").await;
        let timeout = spec.timeout_ms.unwrap_or(WAIT_DEFAULT_MS).min(WAIT_MAX_MS);
        let wait_deadline = Instant::now() + Duration::from_millis(timeout);
        let mut waits = Vec::new();
        for handle in handles.iter() {
            if handle.pane_id.is_empty() {
                // Every recipient reports (DELIVERY.md). A pane-less
                // recipient resolved at send time (attention_required);
                // there is no pane to watch, so the state is null and the
                // delivery field carries the resolution.
                waits.push(json!({
                    "to": handle.to,
                    "outcome": WaitOutcome::NotDelivered,
                    "state": Value::Null,
                    "waited_ms": 0,
                    "delivery": handle.state(),
                }));
                continue;
            }
            let started = Instant::now();
            let mut rx = handle.state_tx.subscribe();
            let resolved =
                tokio::time::timeout_at(wait_deadline, rx.wait_for(|s| receipt_resolved(*s)))
                    .await
                    .is_ok();
            let delivery_state = handle.state();
            let delivered = matches!(
                delivery_state,
                DeliveryState::DeliveredVerified | DeliveryState::DeliveredUnverified
            );
            if !delivered {
                // Resolved but not delivered: no turn to watch. Unresolved
                // by the deadline: the wait timed out waiting for the
                // delivery itself.
                waits.push(json!({
                    "to": handle.to,
                    "outcome": if resolved {
                        WaitOutcome::NotDelivered
                    } else {
                        WaitOutcome::Timeout
                    },
                    "state": inner.cached_state(&handle.pane_id),
                    "waited_ms": started.elapsed().as_millis() as u64,
                    "delivery": delivery_state,
                }));
                continue;
            }
            let remaining = wait_deadline.saturating_duration_since(Instant::now());
            // Turn evidence at or after this delivery's submit: the gate
            // held the pane idle until our submit, so a working state read
            // now is the turn this delivery started.
            let working_pre = handle.working_seen.load(Ordering::SeqCst);
            // Pin the wait on the occupant the delivery was SUBMITTED to,
            // not whoever lives in the pane now: an occupant swap between
            // submit and wait start must read occupant_changed instead of
            // answering for the impostor.
            let submitted = handle.submitted_pid.load(Ordering::SeqCst);
            let end = wait_pinned(
                inner,
                handle.session_idx,
                &handle.pane_id,
                spec.until,
                remaining,
                working_pre,
                (submitted != 0).then_some(submitted),
            )
            .await;
            waits.push(json!({
                "to": handle.to,
                "outcome": end.outcome,
                "state": end.state,
                "waited_ms": end.waited_ms,
                "delivery": delivery_state,
            }));
        }
        value["wait"] = Value::Array(waits);
    }
    Ok(value)
}

/// Will the gate reach a verdict on this pane without waiting on the agent
/// or the human? That is the question the receipt blocks on, and it is not
/// the same question as "is the pane idle".
///
/// Two shapes answer immediately and they end opposite ways. An idle pane
/// proceeds and the delivery resolves inside the block. A pane no manifest
/// binds is refused: the gate returns no_manifest before it ever looks at a
/// state (MEASURED at 7ms), because nothing can be typed into a pane cyclops
/// cannot read. Reporting that one "queued · 0 ahead" and exiting 0 told the
/// sender their message was on its way to a pane it could never reach.
///
/// Everything else holds: a working pane, a human mid-keystroke, a modal
/// waiting on a person. Those senders get their queue position now rather
/// than a 2.5s wait for a badge that is not coming, which is the property
/// docs/guides/send.md promises for a busy target.
///
/// The verdict itself stays the gate's: this only decides whether the
/// receipt is worth waiting for. A pane that binds a manifest between this
/// call and the gate simply takes the idle path, and the block is capped
/// either way.
fn gate_answers_now(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> bool {
    let cached = inner.cached_state(pane_id);
    if matches!(
        cached,
        AgentState::Idle | AgentState::BlockedQuota | AgentState::Dead
    ) {
        return true;
    }
    let Some(watcher) = inner.watcher_of(session_idx) else {
        // Detached: the gate holds until the session comes back.
        return false;
    };
    // A pane that left the table between resolution and here is answered
    // by the gate's no_such_pane, which is just as immediate.
    match watcher.pane(pane_id) {
        Some(row) => row.dead || fusion::bind_manifest_for(inner, &row).is_none(),
        None => true,
    }
}

/// Best synchronous estimate of the first gate hold. This is only a receipt
/// seed; the event-driven gate remains authoritative and replaces or clears
/// it as soon as it evaluates the pane. No capture or retry is introduced.
fn initial_hold(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> Option<&'static str> {
    let Some(watcher) = inner.watcher_of(session_idx) else {
        return Some("session_detached");
    };
    let Some(row) = watcher.pane(pane_id) else {
        return Some("unknown");
    };
    if row.in_mode {
        return Some("pane_in_mode");
    }
    match inner.cached_state(pane_id) {
        AgentState::Working => Some("working"),
        AgentState::IdleWithInput => Some("idle_with_input"),
        AgentState::BlockedModal | AgentState::BlockedPermission => Some("blocked"),
        AgentState::Unknown => Some("unknown"),
        AgentState::Idle | AgentState::BlockedQuota | AgentState::Dead => None,
    }
}

fn wire_err(code: &str, msg: impl Into<String>) -> WireError {
    WireError {
        code: code.to_string(),
        message: msg.into(),
        data: None,
    }
}

/// The four states a delivery stops moving in. A receipt taken on any of
/// them is final; anything else is still in the pipeline.
fn receipt_resolved(s: DeliveryState) -> bool {
    matches!(
        s,
        DeliveryState::DeliveredVerified
            | DeliveryState::DeliveredUnverified
            | DeliveryState::AttentionRequired
            | DeliveryState::ParkedBlockedQuota
    )
}

/// Queued means nothing has been typed into the pane yet.
///
/// That covers the three states where the delivery is waiting on the
/// recipient rather than on cyclops: behind another message, held at the
/// gate for a turn to end or a human to stop typing, or waiting out a
/// bounded retry. From the sender's side those are one thing, and the
/// queue position is the honest detail.
///
/// Past the paste it is a different fact and it may not wear the same
/// word: the payload is in the pane, the composer verified it, the submit
/// key may already be sent. Reporting "queued" there tells the sender the
/// opposite of what happened.
fn receipt_is_queued(s: DeliveryState) -> bool {
    matches!(
        s,
        DeliveryState::Queued | DeliveryState::Gating | DeliveryState::RetryQueued
    )
}

fn receipt_of(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>) -> DeliveryReceipt {
    let (state, _, _, cause, note, held_by) = handle.snapshot();
    // The pane the delivery resolved to, so the caller can name it and
    // build the per-pane fix. Empty for a recipient that answered to no
    // pane, which is the one case there is nothing to name.
    let pane = (!handle.pane_id.is_empty()).then(|| handle.pane_id.clone());
    if receipt_resolved(state) {
        return DeliveryReceipt {
            to: handle.to.clone(),
            state,
            position: None,
            // The gate's machine cause travels as-is when the daemon had
            // no detail to add; wording it belongs to the surface showing
            // it (cyclops_ui::grid::cause_words), not here.
            note: note.or(cause),
            pane,
            held_by: None,
        };
    }
    if !receipt_is_queued(state) {
        return DeliveryReceipt {
            to: handle.to.clone(),
            state,
            position: None,
            note: None,
            pane,
            held_by: None,
        };
    }
    let position = inner
        .engine
        .workers
        .lock()
        .expect("workers lock")
        .get(&handle.pane_id)
        .map(|w| w.position_of(handle));
    // A prior job can finish between enqueue and this snapshot. If that
    // handoff makes this handle position zero, recover the current target
    // hold synchronously; followers never inherit the head's token.
    let fallback = (position == Some(0) && held_by.is_none())
        .then(|| initial_hold(inner, handle.session_idx, &handle.pane_id).map(str::to_string))
        .flatten();
    let held_by = held_by_for_position(position, held_by, fallback);
    DeliveryReceipt {
        to: handle.to.clone(),
        state: DeliveryState::Queued,
        position,
        note: None,
        pane,
        held_by,
    }
}

fn held_by_for_position(
    position: Option<u32>,
    held_by: Option<String>,
    fallback: Option<String>,
) -> Option<String> {
    (position == Some(0))
        .then(|| held_by.or(fallback))
        .flatten()
}

/// Expand the to-list: "*" means every labeled pane (explicit adoption is
/// the broadcast domain). Order is preserved, duplicates dropped.
fn expand_recipients(inner: &Arc<Inner>, to: &[String]) -> Result<Vec<String>, WireError> {
    let mut names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for t in to {
        if t == "*" {
            let mut labels: Vec<String> = inner.labels().into_values().collect();
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
            GateOutcome::Proceed {
                manifest_id,
                pane_pid,
            } => {
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
                match attempt_delivery(inner, worker, handle, &manifest_id, pane_pid).await {
                    AttemptOutcome::Done => return,
                    AttemptOutcome::Failed(failure) => {
                        if !fail_attempt(inner, handle, &failure) {
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
    /// This attempt failed; the boundary feeds retry accounting.
    Failed(AttemptFailure),
}

/// The irreversible boundary for one failed attempt. Once a write may have
/// happened, repeating it is unsafe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteBoundary {
    BeforeWrite,
    AfterWrite,
}

/// A delivery failure and its closed, semantic boundary. Call sites select a
/// named failure constructor, so an after-write cause cannot accidentally be
/// marked retryable by passing a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptFailure {
    cause: String,
    boundary: WriteBoundary,
}

impl AttemptFailure {
    fn session_detached() -> Self {
        Self {
            cause: "session_detached".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn no_manifest() -> Self {
        Self {
            cause: "no_manifest".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn pane_rebound_before_paste() -> Self {
        Self {
            cause: "pane_rebound".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn spool_failed() -> Self {
        Self {
            cause: "spool_failed".into(),
            boundary: WriteBoundary::BeforeWrite,
        }
    }

    fn paste_failed() -> Self {
        Self {
            cause: "paste_failed".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn verify_failed() -> Self {
        Self {
            cause: "verify_failed".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn pane_rebound_after_paste() -> Self {
        Self {
            cause: "pane_rebound_after_paste".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn submit_failed() -> Self {
        Self {
            cause: "submit_failed".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    fn ack_timeout() -> Self {
        Self {
            cause: "ack_timeout".into(),
            boundary: WriteBoundary::AfterWrite,
        }
    }

    /// Map the injector's closed set of pre-submit causes to the semantic
    /// constructors above. Unknown injector errors remain conservatively
    /// after-write; they must never gain retryability by default.
    fn from_inject(cause: String) -> Self {
        match cause.as_str() {
            "spool_failed" => Self::spool_failed(),
            "paste_failed" => Self::paste_failed(),
            "verify_failed" => Self::verify_failed(),
            _ => Self {
                cause,
                boundary: WriteBoundary::AfterWrite,
            },
        }
    }
}

/// One injection attempt: paste, verify, submit, wait for an ACK tier.
///
/// The gate's admitting snapshot is re-checked against the live pane table
/// immediately before the paste and again immediately before the submit
/// key (`admitted_pid`): a pane whose occupant changed after admit (agent
/// exited to a shell, another CLI took over) must never receive the
/// payload or the Enter, because a shell occupant would EXECUTE it.
async fn attempt_delivery(
    inner: &Arc<Inner>,
    worker: &Arc<Worker>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
    admitted_pid: i32,
) -> AttemptOutcome {
    let Some(watcher) = inner.watcher_of(worker.session_idx) else {
        return AttemptOutcome::Failed(AttemptFailure::session_detached());
    };
    let Some(manifest) = inner.manifests.get(manifest_id) else {
        return AttemptOutcome::Failed(AttemptFailure::no_manifest());
    };
    let injector = TmuxInjector {
        client: watcher.client(),
        buffer: format!(
            "cyc-{}-{}",
            std::process::id(),
            inner.engine.buffer_seq.fetch_add(1, Ordering::Relaxed)
        ),
    };
    inject_pause(inner, "pre_paste").await;
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_before_paste());
    }
    let (staged_window, id_staged) = match inject(&injector, handle, manifest).await {
        Ok(v) => v,
        Err(cause) => {
            return AttemptOutcome::Failed(AttemptFailure::from_inject(cause));
        }
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
    inject_pause(inner, "pre_submit").await;
    if let Err(detail) = occupant_unchanged(inner, &watcher, handle, manifest_id, admitted_pid) {
        // The staged payload belongs to the occupant that verified it; the
        // submit key must never reach whoever replaced it.
        unregister_ack(inner, handle);
        gate_line(inner, handle, "rebound", None, Some(&detail));
        return AttemptOutcome::Failed(AttemptFailure::pane_rebound_after_paste());
    }
    // The occupant re-check just passed: admitted_pid IS the process the
    // submit key goes to. Send-and-wait pins its wait on this pid.
    handle.submitted_pid.store(admitted_pid, Ordering::SeqCst);
    if let Err(cause) = injector.submit(&handle.pane_id, submit_key).await {
        unregister_ack(inner, handle);
        debug_assert_eq!(cause, "submit_failed");
        return AttemptOutcome::Failed(AttemptFailure::submit_failed());
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
    match await_ack(inner, handle, manifest, &staged_window, id_staged).await {
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
            AttemptOutcome::Failed(AttemptFailure::ack_timeout())
        }
    }
}

/// Pane-rebind re-check between the gate's admitting recompute and the
/// irreversible injection steps. The pane must still exist, be alive, keep
/// the pid it was admitted with, and bind to the manifest the gate
/// admitted. Err carries the mismatch detail for the gate ledger line; the
/// delivery then retries through the gate, which re-evaluates from scratch.
fn occupant_unchanged(
    inner: &Arc<Inner>,
    watcher: &Arc<SessionWatcher>,
    handle: &Arc<DeliveryHandle>,
    manifest_id: &str,
    admitted_pid: i32,
) -> Result<(), String> {
    let Some(row) = watcher.pane(&handle.pane_id) else {
        return Err("pane_gone".to_string());
    };
    if row.dead {
        return Err("pane_dead".to_string());
    }
    if row.pane_pid != admitted_pid {
        return Err("pane_pid_changed".to_string());
    }
    match fusion::bind_manifest_for(inner, &row) {
        Some(m) if m.agent.id == manifest_id => Ok(()),
        Some(_) => Err("manifest_changed".to_string()),
        None => Err("manifest_unbound".to_string()),
    }
}

/// Await the test-only injection pause, when one is installed. Production
/// never installs one; this is a no-op there.
async fn inject_pause(inner: &Arc<Inner>, phase: &'static str) {
    let hook = inner
        .inject_pause
        .lock()
        .expect("inject pause lock")
        .clone();
    if let Some(h) = hook {
        h(phase).await;
    }
}

/// Retry accounting. Only failures proven to precede the pane write may
/// consume the configured retry budget. True means the caller should retry
/// (state is RetryQueued); false means the delivery ended in attention_required.
fn fail_attempt(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    failure: &AttemptFailure,
) -> bool {
    let attempts = handle.state.lock().expect("handle state lock").attempts;
    let from = [
        DeliveryState::Pasting,
        DeliveryState::Staged,
        DeliveryState::Submitted,
        DeliveryState::RetryQueued,
    ];
    if should_retry(failure, attempts, inner.cfg.delivery_retry_max) {
        advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::RetryQueued).cause(&failure.cause),
        )
    } else {
        let moved = advance(
            inner,
            handle,
            &from,
            Step::to(DeliveryState::AttentionRequired).cause(&failure.cause),
        );
        if moved {
            notify_attention(inner, handle, &failure.cause);
        }
        false
    }
}

fn should_retry(failure: &AttemptFailure, attempts: u32, retry_max: u32) -> bool {
    matches!(failure.boundary, WriteBoundary::BeforeWrite) && attempts <= retry_max
}

fn notify_attention(inner: &Arc<Inner>, handle: &Arc<DeliveryHandle>, cause: &str) {
    admin_notify(
        inner,
        NotifyLevel::ActionRequired,
        &format!("delivery to {} needs attention", handle.to),
        &format!("message {}: {cause}", handle.msg_id),
        Some(&handle.msg_id),
        Some(handle.session_idx),
        About::delivery(&handle.to),
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
        About::delivery(&handle.to),
    );
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

enum GateOutcome {
    Proceed {
        manifest_id: String,
        /// pane_pid of the admitted occupant, re-checked before paste and
        /// submit: a pane whose occupant changed after admit must never be
        /// injected into.
        pane_pid: i32,
    },
    Park {
        hint: Option<String>,
    },
    Attention {
        cause: String,
    },
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
    // One-shot visibility for wedged holds: a delivery held in gating past
    // the configured threshold pings the admin exactly once.
    let mut hold_since: Option<Instant> = None;
    let mut hold_notified = false;
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
                    let Some(manifest) = fusion::bind_manifest_for(inner, &row) else {
                        return GateOutcome::Attention {
                            cause: "no_manifest".to_string(),
                        };
                    };
                    let manifest_id = manifest.agent.id.clone();
                    let Some(det) = fusion::recompute_pane(
                        inner,
                        handle.session_idx,
                        w,
                        &handle.pane_id,
                        true,
                        "gate",
                    )
                    .await
                    else {
                        return GateOutcome::Attention {
                            cause: "no_such_pane".to_string(),
                        };
                    };
                    match det.state {
                        AgentState::Idle => {
                            gate_line(inner, handle, "proceed", Some(&det.decided_by), None);
                            return GateOutcome::Proceed {
                                manifest_id,
                                pane_pid: row.pane_pid,
                            };
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
                                    let rule_id = r.id.clone();
                                    if !send_decline_keys(
                                        w,
                                        &handle.pane_id,
                                        manifest,
                                        &rule_id,
                                        &keys,
                                    )
                                    .await
                                    {
                                        // The screen changed under the
                                        // decline (TOCTOU): the confirming
                                        // key was withheld. Back to the
                                        // gate loop to re-read reality.
                                        gate_line(
                                            inner,
                                            handle,
                                            "decline_aborted",
                                            Some(&rule_id),
                                            Some("modal_changed"),
                                        );
                                    }
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
                                            // The pane, not the delivery:
                                            // the delivery is only gating,
                                            // and the thing a human clears
                                            // is the prompt on the pane.
                                            About::pane(&handle.pane_id),
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
            handle.set_hold(Some(normalize_hold_cause(&cause)));
            if last_hold.as_deref() != Some(cause.as_str()) {
                gate_line(inner, handle, "hold", None, Some(&cause));
                last_hold = Some(cause.clone());
            }
            let since = *hold_since.get_or_insert_with(Instant::now);
            let notify_at = since + Duration::from_millis(inner.cfg.gate_hold_notify_ms);
            tokio::select! {
                _ = wait_pane_change(&mut ev_rx, pane_rx.as_mut(), &handle.pane_id) => {}
                _ = tokio::time::sleep_until(notify_at), if !hold_notified => {
                    // A wedged hold must at least be visible. One ping per
                    // delivery; the hold itself keeps waiting on events.
                    hold_notified = true;
                    admin_notify(
                        inner,
                        NotifyLevel::ActionRequired,
                        &format!("delivery to {} held in gating", handle.to),
                        &format!(
                            "message {} has been held for over {}ms ({cause})",
                            handle.msg_id, inner.cfg.gate_hold_notify_ms
                        ),
                        Some(&handle.msg_id),
                        Some(handle.session_idx),
                        About::delivery(&handle.to),
                    );
                }
            }
        }
    }
}

/// Keep receipt vocabulary stable and independent of vendor manifest rule
/// ids. Ledger gate lines retain the exact cause for diagnostics; receipts
/// expose only these normalized tokens.
fn normalize_hold_cause(cause: &str) -> &'static str {
    match cause {
        "session_detached" => "session_detached",
        "pane_in_mode" => "pane_in_mode",
        "working" => "working",
        "idle_with_input" => "idle_with_input",
        "unknown" => "unknown",
        c if c.split(':').next() == Some("blocked") => "blocked",
        _ => "unknown",
    }
}

/// Manifest decline keys, in order, with spacing (amendment g: the keys
/// come from the manifest rule, never a generic Enter/Escape).
///
/// TOCTOU guard: before the FINAL confirming key of a multi-key sequence
/// the screen is re-captured, and the same modal rule must still be the
/// winning match. A dialog that vanished or changed between keys (the
/// human answered it, the app redrew) must not receive the confirm.
/// Returns false when the sequence was aborted.
async fn send_decline_keys(
    watcher: &Arc<SessionWatcher>,
    pane_id: &str,
    manifest: &Manifest,
    rule_id: &str,
    keys: &[String],
) -> bool {
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(DECLINE_SPACING).await;
        }
        if i > 0 && i == keys.len() - 1 {
            let title = watcher.pane(pane_id).map(|r| r.title).unwrap_or_default();
            let screen = match watcher.client().capture_pane(pane_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(pane = pane_id, error = %e, "decline recheck capture failed");
                    return false;
                }
            };
            if !modal_still_matches(manifest, &title, &screen, rule_id) {
                return false;
            }
        }
        if let Err(e) = watcher.client().send_keys(pane_id, &[key.as_str()]).await {
            warn!(pane = pane_id, error = %e, "decline key failed");
            return true; // sent what we could; not a TOCTOU abort
        }
    }
    true
}

/// True while `rule_id` is still the winning match for this screen.
fn modal_still_matches(manifest: &Manifest, title: &str, screen: &str, rule_id: &str) -> bool {
    manifest
        .evaluate(title, screen)
        .is_some_and(|r| r.id == rule_id && r.state.is_blocked())
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

/// How payload bytes reach an agent and how the backend reads them back
/// (amendment i; GOALS: delivery behind an adapter). The gate, verify, and
/// ACK layers above call through this seam only, so a headless protocol
/// backend slots in per agent without touching them. [`TmuxInjector`] is
/// the M1 implementation. Errors are the short cause codes retry
/// accounting records.
pub(crate) trait Injector {
    /// Deliver the payload into the pane's composer without submitting.
    async fn paste(&self, pane_id: &str, payload: &str) -> Result<(), String>;
    /// Press the submit key.
    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String>;
    /// Read back the visible grid (this backend's verification sensor).
    async fn capture(&self, pane_id: &str) -> Result<String, String>;
}

/// The tmux paste path: load-buffer through the adapter's private spool
/// (0600 file under the 0700 cyclops home, never the shared temp dir) into
/// a per-delivery unique buffer (amendment e), paste-buffer -p (bracketed
/// when the app opted in, F17) -d so the buffer does not linger
/// server-global, send-keys for submit.
struct TmuxInjector {
    client: Arc<ControlClient>,
    /// Per-delivery unique buffer name.
    buffer: String,
}

impl Injector for TmuxInjector {
    async fn paste(&self, pane_id: &str, payload: &str) -> Result<(), String> {
        if let Err(e) = self
            .client
            .load_buffer(&self.buffer, payload.as_bytes())
            .await
        {
            warn!(buffer = %self.buffer, error = %e, "load-buffer failed");
            // Loading the private spool buffer happens before tmux is asked
            // to write to the pane, regardless of how the load command
            // failed. It is therefore safe to retry under the bounded
            // pre-write budget; only paste-buffer failures are ambiguous.
            return Err("spool_failed".to_string());
        }
        if let Err(e) = self
            .client
            .paste_buffer(&self.buffer, pane_id, true, true)
            .await
        {
            warn!(buffer = %self.buffer, error = %e, "paste-buffer failed");
            // paste-buffer -d never ran, so the loaded buffer would linger
            // server-global with the payload in it. Best effort: the buffer
            // dies with the server either way.
            let _ = self.client.delete_buffer(&self.buffer).await;
            return Err("paste_failed".to_string());
        }
        Ok(())
    }

    async fn submit(&self, pane_id: &str, key: &str) -> Result<(), String> {
        self.client.send_keys(pane_id, &[key]).await.map_err(|e| {
            warn!(error = %e, "submit key failed");
            "submit_failed".to_string()
        })
    }

    async fn capture(&self, pane_id: &str) -> Result<String, String> {
        self.client
            .capture_pane(pane_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Paste the payload and verify the composer staged it. Returns the
/// composer-window snapshot the screen ACK tier compares against, plus
/// whether an id-carrying pattern proved the staging (feeds the tier-2
/// evidence rules).
///
/// Composer verification is the gate (amendment b): bracketed-paste
/// degradation is not observable up front through tmux 3.6a.
async fn inject<I: Injector>(
    injector: &I,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
) -> Result<(String, bool), String> {
    injector.paste(&handle.pane_id, &handle.payload).await?;
    let (id_patterns, other_patterns) = verify_patterns(manifest, &handle.msg_id);
    let mut last_delay = 0;
    for delay in VERIFY_DELAYS_MS {
        if delay > last_delay {
            tokio::time::sleep(Duration::from_millis(delay - last_delay)).await;
        }
        last_delay = delay;
        match injector.capture(&handle.pane_id).await {
            Ok(screen) => {
                if let Some(id_staged) =
                    staged_verified(manifest, &screen, &id_patterns, &other_patterns)
                {
                    return Ok((bottom_window(&screen, COMPOSER_WINDOW), id_staged));
                }
            }
            Err(e) => debug!(error = %e, "verify capture failed"),
        }
    }
    Err("verify_failed".to_string())
}

/// Did this capture prove the paste staged? Id-carrying patterns are
/// unique to the delivery and count anywhere in the verify region; generic
/// patterns ("Pasted text") count only on a manifest composer line, so
/// residue from an EARLIER message in the transcript can never verify a
/// paste that did not stage. Some(id_matched) when staged.
fn staged_verified(
    manifest: &Manifest,
    screen: &str,
    id_patterns: &[String],
    other_patterns: &[String],
) -> Option<bool> {
    let region = bottom_window(screen, VERIFY_REGION);
    if patterns_hit(&region, id_patterns) {
        return Some(true);
    }
    if marker_in_composer(manifest, screen, other_patterns) {
        return Some(false);
    }
    None
}

/// Substituted staging patterns, split into id-carrying (contain the
/// message id after substitution) and generic. The id is always an
/// id-carrying pattern.
fn verify_patterns(manifest: &Manifest, msg_id: &str) -> (Vec<String>, Vec<String>) {
    let mut id = Vec::new();
    let mut other = Vec::new();
    for p in &manifest.injection.verify_pattern {
        if p.contains("<message_id>") {
            id.push(p.replace("<message_id>", msg_id));
        } else {
            other.push(p.clone());
        }
    }
    if id.is_empty() {
        id.push(msg_id.to_string());
    }
    (id, other)
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

/// Outcome of one tier-2 evidence pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    /// The conjunctive rule held: marker gone plus turn evidence.
    Confirmed,
    /// The pane was observed and the evidence is not there (yet).
    Absent,
    /// Nobody looked: the watcher is gone (a detach can clear it before
    /// the lifecycle event is broadcast) or the capture failed. Doubt,
    /// never expiry, mirroring fusion's capture-failure handling.
    Unobservable,
}

/// What one checkpoint pass means for the ACK loop. Expiry may stand only
/// on a pass that actually looked and saw nothing; doubt freezes the clock
/// until observability returns (detach-aware ACKs, v1.1 amendment 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointStep {
    Deliver,
    Freeze,
    Expire,
    Wait,
}

fn checkpoint_step(evidence: Evidence, expired: bool) -> CheckpointStep {
    match evidence {
        Evidence::Confirmed => CheckpointStep::Deliver,
        Evidence::Unobservable => CheckpointStep::Freeze,
        Evidence::Absent if expired => CheckpointStep::Expire,
        Evidence::Absent => CheckpointStep::Wait,
    }
}

/// The per-delivery ACK timeline: the tier-1 hook window, the tier-2
/// screen-evidence checkpoints, and the give-up deadline.
///
/// Detach-aware (DELIVERY.md v1.1 amendment 2): while the session's
/// control connection is down the daemon cannot observe the pane, so the
/// clock freezes; on reattach every remaining instant shifts by the outage
/// duration. Time lost to a detach never counts against an ACK window.
struct AckClock {
    /// End of the tier-1 hook phase; None once the phase ended (or for
    /// screen-tier agents that never had one).
    hook_deadline: Option<Instant>,
    checkpoints: Vec<Instant>,
    next: usize,
    deadline: Instant,
    frozen_at: Option<Instant>,
}

impl AckClock {
    fn new(submit_at: Instant, hook_window: Option<Duration>) -> AckClock {
        AckClock {
            hook_deadline: hook_window.map(|w| submit_at + w),
            checkpoints: ACK_CHECKPOINTS_MS
                .iter()
                .map(|ms| submit_at + Duration::from_millis(*ms))
                .collect(),
            next: 0,
            deadline: submit_at + SCREEN_ACK_DEADLINE,
            frozen_at: None,
        }
    }

    fn frozen(&self) -> bool {
        self.frozen_at.is_some()
    }

    fn freeze(&mut self, now: Instant) {
        if self.frozen_at.is_none() {
            self.frozen_at = Some(now);
        }
    }

    /// Reattach: shift every remaining instant by the detach duration.
    fn unfreeze(&mut self, now: Instant) {
        let Some(at) = self.frozen_at.take() else {
            return;
        };
        let lost = now.saturating_duration_since(at);
        if let Some(h) = &mut self.hook_deadline {
            *h += lost;
        }
        for c in &mut self.checkpoints[self.next..] {
            *c += lost;
        }
        self.deadline += lost;
    }

    /// Next timer to arm: (instant, is_hook_phase_end). None while frozen;
    /// a frozen clock never fires.
    fn next_target(&self) -> Option<(Instant, bool)> {
        if self.frozen() {
            return None;
        }
        if let Some(h) = self.hook_deadline {
            return Some((h, true));
        }
        Some((
            self.checkpoints
                .get(self.next)
                .copied()
                .unwrap_or(self.deadline),
            false,
        ))
    }

    /// The tier-1 phase ended, so tier 2 opens here: one pass now, then the
    /// checkpoints the hook window did not cover.
    ///
    /// The passes inside the window are dropped rather than replayed. None
    /// of them ran (the hook deadline is the only armed timer while the
    /// phase lasts), and three captures of one screen in the same instant
    /// answer the same question three times.
    ///
    /// The pass AT `now` is the one that matters, and leaving it out is
    /// what shipped. MEASURED at the defaults: submit at +20ms, hook window
    /// closes at +1520ms, and the next unexpired checkpoint is submit+3000,
    /// so nothing looked at a pane that had held the evidence since +20ms
    /// for a second and a half. receipt_block_ms (2500) expires inside that
    /// hole, which is why every send to an agent whose hooks are not wired
    /// printed "queued" and no delivery badge ever reached the sender.
    fn end_hook_phase(&mut self, now: Instant) {
        self.hook_deadline = None;
        while self.next < self.checkpoints.len() && self.checkpoints[self.next] <= now {
            self.next += 1;
        }
        self.checkpoints.insert(self.next, now);
    }

    fn advance_checkpoint(&mut self) {
        self.next += 1;
    }

    fn expired(&self, now: Instant) -> bool {
        !self.frozen() && now >= self.deadline
    }
}

/// Receive from an optional pane-event stream; pends forever when the
/// session is detached (no watcher, no stream).
async fn recv_pane(
    rx: &mut Option<broadcast::Receiver<PaneEvent>>,
) -> Result<PaneEvent, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Tier 1: the manifest hook ACK inside ack_timeout_ms. Tier 2: screen
/// evidence until the deadline, checked on pane events and bounded
/// one-shot checkpoints. A hook ACK is accepted at any point.
///
/// The clock freezes across a session detach and a reattach runs an
/// immediate evidence pass BEFORE any deadline can expire, so a delivery
/// that landed during the outage resolves as delivered instead of being
/// resubmitted (the m1 soak's duplicate). Hook ACKs arriving during the
/// outage are accepted by the matcher independently of this loop.
async fn await_ack(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    staged_window: &str,
    id_staged: bool,
) -> AckOutcome {
    let submit_at = Instant::now();
    let tier1 = manifest.hooks.ack.is_some() && manifest.hooks.ack_payload_field.is_some();
    let (id_patterns, other_patterns) = verify_patterns(manifest, &handle.msg_id);
    let patterns: Vec<String> = id_patterns.into_iter().chain(other_patterns).collect();
    let mut ev_rx = inner.events.subscribe();
    let mut pane_rx = inner.watcher_of(handle.session_idx).map(|w| w.subscribe());
    let mut working_seen = false;
    let mut output_seen = false;
    let mut clock = AckClock::new(
        submit_at,
        tier1.then(|| Duration::from_millis(inner.cfg.ack_timeout_ms)),
    );
    if pane_rx.is_none() {
        clock.freeze(Instant::now());
    }

    loop {
        let target = clock.next_target();
        tokio::select! {
            _ = handle.ack.notified() => {
                if handle.state() == DeliveryState::DeliveredVerified {
                    return AckOutcome::Resolved;
                }
            }
            _ = tokio::time::sleep_until(target.map(|(t, _)| t).unwrap_or_else(Instant::now)),
                if target.is_some() =>
            {
                let now = Instant::now();
                if target.is_some_and(|(_, hook_end)| hook_end) {
                    // Tier-1 window over: the delivery downgrades to screen
                    // evidence. On a pane that has NEVER produced a hook
                    // edge this is the F1 signature (configuration does not
                    // equal subscription); the admin hears about it once.
                    if tier1 {
                        crate::selftest::notify_f1_once(
                            inner,
                            &handle.msg_id,
                            &handle.to,
                            &handle.pane_id,
                            handle.session_idx,
                            &manifest.agent.id,
                        );
                    }
                    clock.end_hook_phase(now);
                    continue;
                }
                clock.advance_checkpoint();
                let evidence = screen_evidence(
                    inner, handle, manifest, &patterns, staged_window,
                    id_staged, working_seen, output_seen,
                ).await;
                match checkpoint_step(evidence, clock.expired(Instant::now())) {
                    CheckpointStep::Deliver => return AckOutcome::Screen,
                    CheckpointStep::Expire => return AckOutcome::Timeout,
                    CheckpointStep::Freeze => {
                        // The pass could not look (watcher cleared before
                        // its detach event, or the capture failed): a
                        // Timeout here would stand on nothing. Freeze; a
                        // session edge, pane activity, or a lag reconcile
                        // unfreezes.
                        clock.freeze(Instant::now());
                    }
                    CheckpointStep::Wait => {}
                }
            }
            // Reattach/detach truth for THIS session comes from
            // `inner.watcher_of(handle.session_idx)`, resolved fresh here,
            // never from matching a "session" event's own `data["name"]`
            // against a name captured at function entry: a followed
            // rename (`PaneEvent::SessionRenamed`, `rename_session_slot`
            // in lib.rs) changes the live name mid-wait, and a stale
            // snapshot then never matches an attach OR a detach line
            // again — the clock freezes on the FIRST outage and never
            // unfreezes, which is exactly the "ledger append silently
            // drops" failure mode `emit_state`'s doc comment describes,
            // in delivery-wait clothing. `watcher_of` cannot go stale: it
            // reads the live link for this exact idx, so what changed is
            // compared against what IS, not against a name.
            ev = ev_rx.recv() => {
                if track_state_event(&ev, &handle.pane_id) {
                    working_seen = true;
                    handle.working_seen.store(true, Ordering::SeqCst);
                }
                if is_session_event(&ev) {
                    let live = inner.watcher_of(handle.session_idx);
                    if pane_rx.is_none() {
                        if let Some(w) = live {
                            pane_rx = Some(w.subscribe());
                            clock.unfreeze(Instant::now());
                            // Reattach evidence pass, before any deadline
                            // can fire: did the payload arrive during the
                            // outage?
                            match screen_evidence(
                                inner, handle, manifest, &patterns, staged_window,
                                id_staged, working_seen, output_seen,
                            ).await {
                                Evidence::Confirmed => return AckOutcome::Screen,
                                // Still blind right after the edge: stay
                                // frozen rather than letting the shifted
                                // deadlines run on an unobserved pane.
                                Evidence::Unobservable => clock.freeze(Instant::now()),
                                Evidence::Absent => {}
                            }
                        }
                    } else if live.is_none() {
                        pane_rx = None;
                        clock.freeze(Instant::now());
                    }
                } else if matches!(ev, Err(broadcast::error::RecvError::Lagged(_)))
                    && clock.frozen()
                {
                    // A lagged event stream can swallow the reattach
                    // notice; reconcile against the link instead of
                    // staying frozen forever.
                    if let Some(w) = inner.watcher_of(handle.session_idx) {
                        pane_rx = Some(w.subscribe());
                        clock.unfreeze(Instant::now());
                    }
                }
            }
            pe = recv_pane(&mut pane_rx) => {
                match pe {
                    Ok(PaneEvent::OutputActivity { pane_id: p, .. }) if p == handle.pane_id => {
                        output_seen = true;
                        // A frozen clock with a live pane stream is doubt
                        // from a failed capture; the pane speaking again
                        // is the cue to look and, if observable, resume.
                        if clock.frozen() {
                            match screen_evidence(
                                inner, handle, manifest, &patterns, staged_window,
                                id_staged, working_seen, output_seen,
                            ).await {
                                Evidence::Confirmed => return AckOutcome::Screen,
                                Evidence::Absent => clock.unfreeze(Instant::now()),
                                Evidence::Unobservable => {}
                            }
                        }
                    }
                    Ok(PaneEvent::Disconnected)
                    | Err(broadcast::error::RecvError::Closed) => {
                        pane_rx = None;
                        clock.freeze(Instant::now());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// True when the event is a working fused-state change for this pane.
fn track_state_event(ev: &Result<Event, broadcast::error::RecvError>, pane_id: &str) -> bool {
    matches!(ev, Ok(e)
        if e.event == "state" && e.data["pane_id"] == pane_id && e.data["state"] == "working")
}

/// True when the event is a session lifecycle line — attach, detach, or
/// this daemon's own rename bookkeeping riding the same "session" name
/// (`session_lifecycle`, lib.rs). Which one, and whether it is about THIS
/// caller's session at all, is deliberately not decided here: see the doc
/// comment on `await_ack`'s event arm for why comparing against
/// `inner.watcher_of(session_idx)`'s live truth, not the event's own
/// `data["name"]`, is what a caller does with this.
fn is_session_event(ev: &Result<Event, broadcast::error::RecvError>) -> bool {
    matches!(ev, Ok(e) if e.event == "session")
}

/// Screen evidence for tier 2, spec conjunctive form (v1.1 amendment 1):
/// the marker left the composer AND turn evidence appeared.
///
/// "Left the composer" is manifest-driven: the marker still sits in the
/// composer only when an idle_with_input rule identifies a composer line
/// that carries it (staged-but-unsubmitted text, e.g. Claude's collapsed
/// paste on the `❯` line). Manifests without an idle_with_input rule
/// cannot pin staged text.
///
/// Turn evidence is a working state, output activity, or a changed
/// composer window. The changed window counts only when verification
/// demonstrably staged the id pattern: a redraw can change the window of a
/// pane that never took the paste, but it cannot have staged OUR id first.
/// (%output events can be swallowed by the watcher's per-pane rate limit
/// for single short bursts, MEASURED: a cat pane's echoed submit stays
/// under the 100ms floor, which is why the changed window matters.)
#[allow(clippy::too_many_arguments)]
async fn screen_evidence(
    inner: &Arc<Inner>,
    handle: &Arc<DeliveryHandle>,
    manifest: &Manifest,
    patterns: &[String],
    staged_window: &str,
    id_staged: bool,
    working_seen: bool,
    output_seen: bool,
) -> Evidence {
    let Some(watcher) = inner.watcher_of(handle.session_idx) else {
        return Evidence::Unobservable;
    };
    let Ok(screen) = watcher.client().capture_pane(&handle.pane_id).await else {
        return Evidence::Unobservable;
    };
    let changed = bottom_window(&screen, COMPOSER_WINDOW) != staged_window;
    if !marker_in_composer(manifest, &screen, patterns)
        && tier2_evidence(changed, id_staged, working_seen, output_seen)
    {
        Evidence::Confirmed
    } else {
        Evidence::Absent
    }
}

/// The tier-2 turn-evidence rule, factored for the unit test: a changed
/// window alone is only evidence when the id demonstrably staged.
fn tier2_evidence(changed: bool, id_staged: bool, working_seen: bool, output_seen: bool) -> bool {
    working_seen || output_seen || (changed && id_staged)
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

/// How a wait ended. Serialized into send-and-wait entries and agent.wait
/// error data; NotDelivered only occurs in send-and-wait composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitOutcome {
    /// The target reached `until`.
    Reached,
    /// The timeout expired first.
    Timeout,
    /// The pinned pane died or its occupant changed mid-wait; resolving
    /// anything else would be a false answer about a different process.
    OccupantChanged,
    /// Send-and-wait only: the delivery resolved somewhere other than
    /// delivered, so there is no turn to watch.
    NotDelivered,
}

/// A finished wait: how it ended, the fused state it ended on, and how
/// long it actually waited.
pub(crate) struct WaitEnd {
    pub(crate) outcome: WaitOutcome,
    pub(crate) state: AgentState,
    pub(crate) waited_ms: u64,
}

/// The pane row behind a wait target: the live table while attached, the
/// frozen last-known table during a detach (frozen rows cannot false-alarm
/// the pin; the reattach re-check settles it).
fn occupant_of(inner: &Arc<Inner>, session_idx: usize, pane_id: &str) -> Option<PaneRow> {
    if let Some(w) = inner.watcher_of(session_idx) {
        return w.pane(pane_id);
    }
    inner
        .session(session_idx)?
        .last_panes
        .lock()
        .expect("last panes lock")
        .get(pane_id)
        .cloned()
}

/// True when the pinned occupant is gone: pane missing, dead, or running
/// under a different root pid than the one pinned at wait start.
fn occupant_gone(inner: &Arc<Inner>, session_idx: usize, pane_id: &str, pinned_pid: i32) -> bool {
    match occupant_of(inner, session_idx, pane_id) {
        Some(row) => row.dead || row.pane_pid != pinned_pid,
        None => true,
    }
}

/// Wait for a pane's fused state to satisfy `until`, pinned to the pane
/// occupant present at wait start. Event-driven off the fusion broadcast
/// and the session watcher stream; the deadline is the only timer.
///
/// Semantics (protocol spec): idle is fused Idle; blocked is any blocked_*
/// state; done is the working -> idle edge, so the CURRENT turn (already
/// working, or `working_pre` from a delivery handle) or the NEXT turn
/// counts, and a blocked state mid-turn keeps waiting rather than passing
/// as done.
///
/// Pinning: (pane_id, pane_pid) recorded at start, or supplied by the
/// caller as `pinned` when the wait answers for an earlier moment (the
/// send-and-wait path pins the occupant its delivery was SUBMITTED to).
/// The pane vanishing, dying, or changing root pid resolves
/// OccupantChanged, never a false success. pane_pid changes are silent in
/// the watcher (no PaneField), so the pin is re-read on every wake for
/// this pane, including output activity: a swapped occupant that neither
/// prints nor changes any pane field stays undetected until it does, the
/// same residual window the delivery pipeline accepts.
pub(crate) async fn wait_pinned(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    until: WaitUntil,
    timeout: Duration,
    working_pre: bool,
    pinned: Option<i32>,
) -> WaitEnd {
    let started = Instant::now();
    let deadline = started + timeout;
    let end = |outcome: WaitOutcome, state: AgentState| WaitEnd {
        outcome,
        state,
        waited_ms: started.elapsed().as_millis() as u64,
    };
    // Subscribe before the first read so no edge can fall between.
    let mut ev_rx = inner.events.subscribe();
    let mut pane_rx = inner.watcher_of(session_idx).map(|w| w.subscribe());
    let mut state = inner.cached_state(pane_id);
    // Pin the occupant. A pane already dead or gone can never honestly
    // reach `until`, and a caller-pinned occupant that is already replaced
    // is already an occupant change.
    let pinned_pid = match occupant_of(inner, session_idx, pane_id) {
        Some(row) if !row.dead && pinned.is_none_or(|p| p == row.pane_pid) => {
            pinned.unwrap_or(row.pane_pid)
        }
        _ => return end(WaitOutcome::OccupantChanged, state),
    };
    let mut working_seen = working_pre || state == AgentState::Working;
    loop {
        if state == AgentState::Dead {
            return end(WaitOutcome::OccupantChanged, state);
        }
        let satisfied = match until {
            WaitUntil::Idle => state == AgentState::Idle,
            WaitUntil::Blocked => state.is_blocked(),
            WaitUntil::Done => {
                working_seen && matches!(state, AgentState::Idle | AgentState::IdleWithInput)
            }
        };
        if satisfied {
            return end(WaitOutcome::Reached, state);
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return end(WaitOutcome::Timeout, state),
            ev = ev_rx.recv() => match ev {
                Ok(e) if e.event == "state" && e.data["pane_id"] == pane_id => {
                    if let Ok(s) = serde_json::from_value::<AgentState>(e.data["state"].clone()) {
                        state = s;
                        if state == AgentState::Working {
                            working_seen = true;
                        }
                    }
                }
                // Attach/detach truth for THIS session comes from
                // `inner.watcher_of(session_idx)`, resolved fresh here,
                // never from matching this event's own `data["name"]`
                // against a name captured at entry: a followed rename
                // changes the live name mid-wait, and a stale snapshot
                // then never matches an attach line again — see the doc
                // comment on `await_ack`'s (this function's sibling wait)
                // event arm for the full failure this avoids.
                Ok(e) if e.event == "session" => match inner.watcher_of(session_idx) {
                    Some(w) if pane_rx.is_none() => {
                        // Reattach: fresh stream, then re-verify the pin
                        // against the live table (the pane may have died
                        // or been replaced during the outage).
                        pane_rx = Some(w.subscribe());
                        if occupant_gone(inner, session_idx, pane_id, pinned_pid) {
                            return end(WaitOutcome::OccupantChanged, state);
                        }
                    }
                    // Detached: fused state stops moving; keep waiting on
                    // the deadline and the reattach edge. Also the no-op
                    // case (already attached, this event was about a
                    // different session or already-applied edge).
                    None => pane_rx = None,
                    _ => {}
                },
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Reconcile on doubt: re-read the cache and the pin.
                    state = inner.cached_state(pane_id);
                    if state == AgentState::Working {
                        working_seen = true;
                    }
                    if occupant_gone(inner, session_idx, pane_id, pinned_pid) {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return end(WaitOutcome::Timeout, state)
                }
            },
            pe = recv_pane(&mut pane_rx) => match pe {
                Ok(PaneEvent::PaneRemoved(id)) if id == pane_id => {
                    return end(WaitOutcome::OccupantChanged, state);
                }
                Ok(PaneEvent::PaneChanged { id, row, .. }) if id == pane_id => {
                    if row.dead || row.pane_pid != pinned_pid {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Ok(PaneEvent::OutputActivity { pane_id: p, .. }) if p == pane_id => {
                    // Output is the one signal a silent pid swap gives off
                    // (respawn-pane updates the row without a PaneChanged).
                    if occupant_gone(inner, session_idx, pane_id, pinned_pid) {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Ok(PaneEvent::Disconnected) | Err(broadcast::error::RecvError::Closed) => {
                    pane_rx = None;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if occupant_gone(inner, session_idx, pane_id, pinned_pid) {
                        return end(WaitOutcome::OccupantChanged, state);
                    }
                }
                Ok(_) => {}
            },
        }
    }
}

/// agent.wait entry for the socket server. Reached answers with the state
/// and waited_ms; timeout and occupant_changed answer as wire errors whose
/// data carries the same fields, so a caller always learns the state the
/// target was in.
pub(crate) async fn agent_wait(
    inner: &Arc<Inner>,
    params: cyclops_proto::AgentWaitParams,
) -> Result<Value, WireError> {
    let Some((session_idx, pane_id)) = inner.resolve_recipient(&params.target) else {
        return Err(wire_err(
            "no_such_target",
            format!("no such target {:?}", params.target),
        ));
    };
    let timeout = params
        .timeout_ms
        .unwrap_or(WAIT_DEFAULT_MS)
        .min(WAIT_MAX_MS);
    let until_word = until_word(params.until);
    let end = wait_pinned(
        inner,
        session_idx,
        &pane_id,
        params.until,
        Duration::from_millis(timeout),
        false,
        None,
    )
    .await;
    // `outcome` mirrors the send-and-wait entry shape: "reached" on
    // success, and the same word the error code carries otherwise.
    let data = json!({
        "target": params.target,
        "pane_id": pane_id,
        "until": params.until,
        "outcome": end.outcome,
        "state": end.state,
        "waited_ms": end.waited_ms,
    });
    match end.outcome {
        WaitOutcome::Reached => Ok(data),
        WaitOutcome::Timeout => Err(WireError {
            code: "timeout".to_string(),
            message: format!(
                "{} did not reach {until_word} within {timeout}ms; state was {}",
                params.target, end.state
            ),
            data: Some(data),
        }),
        WaitOutcome::OccupantChanged | WaitOutcome::NotDelivered => Err(WireError {
            code: "occupant_changed".to_string(),
            message: format!(
                "the pane behind {} died or changed occupant while waiting",
                params.target
            ),
            data: Some(data),
        }),
    }
}

/// The wire word for an until mode, for human-facing error copy.
fn until_word(until: WaitUntil) -> &'static str {
    match until {
        WaitUntil::Idle => "idle",
        WaitUntil::Done => "done",
        WaitUntil::Blocked => "blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_only_retries_failures_proven_before_the_write() {
        let cases = [
            (AttemptFailure::session_detached(), "session_detached", true),
            (AttemptFailure::no_manifest(), "no_manifest", true),
            (
                AttemptFailure::pane_rebound_before_paste(),
                "pane_rebound",
                true,
            ),
            (AttemptFailure::spool_failed(), "spool_failed", true),
            (AttemptFailure::paste_failed(), "paste_failed", false),
            (AttemptFailure::verify_failed(), "verify_failed", false),
            (
                AttemptFailure::pane_rebound_after_paste(),
                "pane_rebound_after_paste",
                false,
            ),
            (AttemptFailure::submit_failed(), "submit_failed", false),
            (AttemptFailure::ack_timeout(), "ack_timeout", false),
        ];
        for (failure, cause, retryable) in cases {
            assert_eq!(failure.cause, cause);
            assert_eq!(
                should_retry(&failure, 1, 1),
                retryable,
                "retry policy changed for {cause}"
            );
        }
        let exhausted = AttemptFailure::spool_failed();
        assert!(!should_retry(&exhausted, 2, 1));

        // The production mapping keeps unknown injector errors conservative
        // too: they can never opt into the pre-write retry budget.
        let unknown = AttemptFailure::from_inject("future_failure".into());
        assert!(!should_retry(&unknown, 1, 1));
    }

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
        assert_eq!(lines[2], "Reply: cyclops send codex --subject \"...\"");
        assert!(
            !p.ends_with('\n'),
            "no trailing newline; submit is separate"
        );
    }

    #[test]
    fn fyi_payload_has_no_reply_hint() {
        let p = render_payload("m-1", "codex", "heads up", "body", true);
        assert!(!p.contains("Reply:"));
    }

    /// A message from the operator carries no reply line.
    ///
    /// `admin` is reserved (`cyclops_proto::label`), so no pane can hold
    /// it and `cyclops send admin` answers `no_such_target` every time.
    /// The workspace composer sends as `admin`, which made this the
    /// COMMON case: nearly every message a human writes was arriving with
    /// a command that cannot run attached to it.
    #[test]
    fn a_message_from_the_operator_offers_no_reply_that_would_fail() {
        let p = render_payload("m-1", cyclops_proto::label::ADMIN, "ship it", "now", false);
        assert!(!p.contains("Reply:"), "{p}");
        assert_eq!(
            p, "[cyclops m-1] FROM: admin  SUBJECT: ship it\nnow",
            "the header and the body, and nothing else"
        );
        // An agent-to-agent message still gets one: those targets exist.
        let p = render_payload("m-2", "reviewer", "ship it", "now", false);
        assert!(p.contains("Reply: cyclops send reviewer"), "{p}");
    }

    #[test]
    fn empty_body_payload_is_header_plus_hint() {
        let p = render_payload("m-1", "codex", "s", "", false);
        assert_eq!(p.lines().count(), 2);
    }

    #[test]
    fn verify_patterns_substitute_split_and_default() {
        let m = Manifest::parse(
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
        let (id, other) = verify_patterns(&m, "m-ab12");
        assert_eq!(id, vec!["m-ab12".to_string()]);
        assert_eq!(other, vec!["Pasted text".to_string()]);

        let empty = Manifest::parse(
            "[agent]\nid = \"y\"\ndisplay_name = \"y\"\n",
            std::path::Path::new("y.toml"),
        )
        .unwrap();
        let (id, other) = verify_patterns(&empty, "m-1");
        assert_eq!(id, vec!["m-1".to_string()]);
        assert!(other.is_empty());
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

    // -----------------------------------------------------------------
    // Post-paste verification (fix B: stale screen text must not verify)
    // -----------------------------------------------------------------

    const COMPOSER_MANIFEST: &str = r#"
[agent]
id = "c"
display_name = "c"

[[rule]]
id = "composer_has_staged_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>", "Pasted text"]
"#;

    fn composer_manifest() -> Manifest {
        Manifest::parse(COMPOSER_MANIFEST, std::path::Path::new("c.toml")).unwrap()
    }

    #[test]
    fn stale_generic_pattern_does_not_verify() {
        let m = composer_manifest();
        let (id, other) = verify_patterns(&m, "m-new01");
        // "Pasted text" from a PREVIOUS message sits in the transcript;
        // the composer is empty. Nothing staged.
        let screen = "you: [Pasted text #1 +9 lines]\nassistant: done\n❯ \n? for shortcuts";
        assert_eq!(staged_verified(&m, screen, &id, &other), None);
        // The same chip ON the composer line is a real staging.
        let staged = "transcript\n❯ [Pasted text #2 +9 lines]\n? for shortcuts";
        assert_eq!(staged_verified(&m, staged, &id, &other), Some(false));
        // The substituted id counts anywhere in the region: it is unique
        // to this delivery.
        let id_anywhere = "transcript\n❯ [cyclops m-new01] hello\n? for shortcuts";
        assert_eq!(staged_verified(&m, id_anywhere, &id, &other), Some(true));
    }

    /// The whole inject() path with a mock backend: the stale screen fails
    /// all verify re-reads (this failed before fix B: any "Pasted text" in
    /// the bottom 15 lines verified), and a composer-line staging passes.
    struct MockInjector {
        screens: StdMutex<Vec<String>>,
        cursor: std::sync::atomic::AtomicUsize,
        pasted: StdMutex<Vec<String>>,
    }

    impl MockInjector {
        fn new(screens: Vec<&str>) -> MockInjector {
            MockInjector {
                screens: StdMutex::new(screens.into_iter().map(String::from).collect()),
                cursor: std::sync::atomic::AtomicUsize::new(0),
                pasted: StdMutex::new(Vec::new()),
            }
        }
    }

    impl Injector for MockInjector {
        async fn paste(&self, _pane_id: &str, payload: &str) -> Result<(), String> {
            self.pasted.lock().unwrap().push(payload.to_string());
            Ok(())
        }
        async fn submit(&self, _pane_id: &str, _key: &str) -> Result<(), String> {
            Ok(())
        }
        async fn capture(&self, _pane_id: &str) -> Result<String, String> {
            let screens = self.screens.lock().unwrap();
            let i = self.cursor.fetch_add(1, Ordering::Relaxed);
            Ok(screens[i.min(screens.len() - 1)].clone())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn inject_rejects_stale_screen_and_accepts_staged() {
        let m = composer_manifest();
        let handle = DeliveryHandle::new("m-new01", "worker", "%1", 0, "payload".into());

        let stale = "you: [Pasted text #1 +9 lines]\nold turn\n❯ \n? for shortcuts";
        let mock = MockInjector::new(vec![stale]);
        assert_eq!(
            inject(&mock, &handle, &m).await,
            Err("verify_failed".to_string())
        );
        assert_eq!(mock.pasted.lock().unwrap().len(), 1, "payload was pasted");

        let staged = "transcript\n❯ [Pasted text #2 +9 lines]\n? for shortcuts";
        let mock = MockInjector::new(vec![stale, staged]);
        let (window, id_staged) = inject(&mock, &handle, &m).await.expect("staged verifies");
        assert!(!id_staged, "generic pattern staged it, not the id");
        assert!(window.contains("Pasted text #2"));
    }

    // -----------------------------------------------------------------
    // Tier-2 evidence (fix D) and the detach-aware clock (fix E)
    // -----------------------------------------------------------------

    #[test]
    fn tier2_changed_window_alone_needs_the_id_staged() {
        // A changed window with no staged id is a redraw, not delivery
        // evidence (this returned true before fix D).
        assert!(!tier2_evidence(true, false, false, false));
        assert!(tier2_evidence(true, true, false, false));
        assert!(tier2_evidence(false, false, true, false));
        assert!(tier2_evidence(false, false, false, true));
        // Marker gone but nothing else moved: not evidence.
        assert!(!tier2_evidence(false, true, false, false));
    }

    #[test]
    fn unobservable_evidence_freezes_instead_of_expiring() {
        // The detach race: the watcher is cleared before its lifecycle
        // event is broadcast, so a checkpoint's evidence pass cannot look.
        // Before the fix an expired clock returned Timeout here and the
        // retry double-pasted a delivery that may have landed.
        assert_eq!(
            checkpoint_step(Evidence::Unobservable, true),
            CheckpointStep::Freeze
        );
        assert_eq!(
            checkpoint_step(Evidence::Unobservable, false),
            CheckpointStep::Freeze
        );
        // Expiry stands only on a pass that looked and saw nothing.
        assert_eq!(
            checkpoint_step(Evidence::Absent, true),
            CheckpointStep::Expire
        );
        assert_eq!(
            checkpoint_step(Evidence::Absent, false),
            CheckpointStep::Wait
        );
        assert_eq!(
            checkpoint_step(Evidence::Confirmed, true),
            CheckpointStep::Deliver
        );
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Every target a clock hands out, in milliseconds from the submit it
    /// was built on, paired with whether it is the hook-phase end.
    ///
    /// The ACK ladder is a sequence, and asserting it one `next_target` at
    /// a time hides the shape and makes a moved rung read as an unrelated
    /// number. Reading it as offsets also makes the arithmetic checkable
    /// by eye: 1500 is the hook window, 250/750/1500/3000/5000 are the
    /// checkpoints, 5000 is the give-up deadline.
    fn timeline(mut c: AckClock, submit_at: Instant) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        // Bounded so a clock that stops advancing fails as a wrong
        // timeline instead of hanging the suite.
        for _ in 0..12 {
            let Some((at, hook_end)) = c.next_target() else {
                break;
            };
            out.push(((at - submit_at).as_millis() as u64, hook_end));
            if hook_end {
                c.end_hook_phase(at);
            } else if c.expired(at) {
                break;
            } else {
                c.advance_checkpoint();
            }
        }
        out
    }

    /// The clock reads no wall time after it is built.
    ///
    /// Everything it hands out is derived from the submit instant it was
    /// given, so two clocks built ten minutes apart must produce the same
    /// timeline. This is the property that keeps the assertions above from
    /// being load-sensitive, and it is worth stating once rather than
    /// leaving every reader to re-derive it from `next_target`.
    #[tokio::test(start_paused = true)]
    async fn the_ack_timeline_does_not_depend_on_when_the_clock_was_built() {
        let early = Instant::now();
        let late = early + ms(600_000);
        assert_eq!(
            timeline(AckClock::new(early, Some(ms(1500))), early),
            timeline(AckClock::new(late, Some(ms(1500))), late)
        );
        assert_eq!(
            timeline(AckClock::new(early, None), early),
            timeline(AckClock::new(late, None), late)
        );
    }

    /// Instants are asserted as offsets from the submit the clock was
    /// built on, never as wall-clock values: nothing in `AckClock` reads
    /// the clock after construction (proved by
    /// `the_ack_timeline_does_not_depend_on_when_the_clock_was_built`), so
    /// every number below is arithmetic and none of it is a race.
    #[tokio::test(start_paused = true)]
    async fn ack_clock_freezes_across_detach_and_extends_deadlines() {
        let t0 = Instant::now();
        let at = |c: &AckClock| {
            c.next_target()
                .map(|(t, hook)| ((t - t0).as_millis() as u64, hook))
        };
        let mut c = AckClock::new(t0, Some(ms(1500)));
        assert_eq!(at(&c), Some((1500, true)));

        // Detach at +200ms: the clock stops firing entirely.
        c.freeze(t0 + ms(200));
        assert!(c.frozen());
        assert_eq!(c.next_target(), None);
        assert!(!c.expired(t0 + ms(60_000)), "a frozen clock never expires");
        // A second freeze keeps the first freeze instant.
        c.freeze(t0 + ms(300));

        // Reattach at +6200ms: 6s of outage extend every deadline.
        c.unfreeze(t0 + ms(6200));
        assert_eq!(at(&c), Some((7500, true)));
        c.end_hook_phase(t0 + ms(7500));
        // Checkpoints shifted by 6s. The ones the hook phase covered
        // (250/750/1500 -> 6250/6750/7500) are dropped and replaced by one
        // pass now, so tier 2 opens with a look instead of a wait.
        assert_eq!(at(&c), Some((7500, false)));
        c.advance_checkpoint();
        assert_eq!(at(&c), Some((9000, false)));
        c.advance_checkpoint();
        assert_eq!(at(&c), Some((11_000, false)));
        c.advance_checkpoint();
        // Past the checkpoints the final deadline is also shifted.
        assert_eq!(at(&c), Some((11_000, false)));
        assert!(!c.expired(t0 + ms(10_999)));
        assert!(c.expired(t0 + ms(11_000)));
    }

    /// A screen-tier agent has no hook window, so the ladder is the
    /// checkpoints and nothing else.
    #[tokio::test(start_paused = true)]
    async fn ack_clock_without_hook_window_goes_straight_to_checkpoints() {
        let t0 = Instant::now();
        assert_eq!(
            timeline(AckClock::new(t0, None), t0),
            vec![
                (250, false),
                (750, false),
                (1500, false),
                (3000, false),
                (5000, false),
            ]
        );
    }

    /// The shipped numbers, and the hole the receipt fell through.
    ///
    /// ack_timeout_ms is 1500 and every manifest for a real CLI declares a
    /// hook, so a pane whose hooks are not wired spends the whole window
    /// waiting for an ACK that never comes. When it closes, the screen has
    /// held the evidence since the submit, and the second entry here is
    /// the look that reads it.
    ///
    /// That entry is the fix. Without it the timeline ran
    /// [(1500, hook), (3000, ...)]: a second and a half in which tier 2
    /// had opened and nothing looked, with receipt_block_ms (2500)
    /// expiring inside it. A 1.5s gap between the first two entries is
    /// exactly that defect coming back.
    #[tokio::test(start_paused = true)]
    async fn tier2_opens_the_moment_the_hook_window_closes() {
        let t0 = Instant::now();
        assert_eq!(
            timeline(AckClock::new(t0, Some(ms(1500))), t0),
            vec![(1500, true), (1500, false), (3000, false), (5000, false),]
        );
    }

    /// Queued is a claim about the pane: nothing has been typed into it.
    #[test]
    fn a_receipt_calls_a_delivery_queued_only_before_the_paste() {
        use DeliveryState::*;
        for s in [Queued, Gating, RetryQueued] {
            assert!(receipt_is_queued(s), "{s:?} is waiting on the recipient");
        }
        for s in [Pasting, Staged, Submitted] {
            assert!(
                !receipt_is_queued(s),
                "{s:?} has the payload in the pane and may not report queued"
            );
        }
        // Resolved states never reach the question.
        for s in [
            DeliveredVerified,
            DeliveredUnverified,
            AttentionRequired,
            ParkedBlockedQuota,
        ] {
            assert!(receipt_resolved(s), "{s:?}");
        }
    }

    #[test]
    fn only_the_position_zero_head_can_recover_a_hold_token() {
        assert_eq!(
            held_by_for_position(None, None, Some("working".into())),
            None
        );
        assert_eq!(
            held_by_for_position(Some(1), None, Some("working".into())),
            None
        );
        assert_eq!(
            held_by_for_position(Some(0), None, Some("working".into())),
            Some("working".into())
        );
        assert_eq!(
            held_by_for_position(Some(0), Some("blocked".into()), Some("working".into())),
            Some("blocked".into())
        );
    }

    // -----------------------------------------------------------------
    // Decline TOCTOU (fix G: modal must still match before the confirm)
    // -----------------------------------------------------------------

    #[test]
    fn modal_match_is_rechecked_by_rule_id() {
        let m = Manifest::parse(
            r#"
[agent]
id = "x"
display_name = "x"

[[rule]]
id = "update_modal"
state = "blocked_modal"
priority = 1300
region = "bottom_non_empty_lines(8)"
contains = ["FAKE-UPDATE-AVAILABLE"]
decline_keys = ["3", "Enter"]
auto_dismiss = true

[[rule]]
id = "other_modal"
state = "blocked_modal"
priority = 1200
region = "bottom_non_empty_lines(8)"
contains = ["OTHER-DIALOG"]
"#,
            std::path::Path::new("x.toml"),
        )
        .unwrap();
        assert!(modal_still_matches(
            &m,
            "t",
            "text\nFAKE-UPDATE-AVAILABLE\nmore",
            "update_modal"
        ));
        // Dialog vanished: never send the confirming key.
        assert!(!modal_still_matches(
            &m,
            "t",
            "plain shell output",
            "update_modal"
        ));
        // A DIFFERENT dialog appeared: the confirm belongs to nobody.
        assert!(!modal_still_matches(
            &m,
            "t",
            "OTHER-DIALOG",
            "update_modal"
        ));
    }

    // -----------------------------------------------------------------
    // Buffer hygiene (fix G: delete-buffer after a failed paste)
    // -----------------------------------------------------------------

    /// Real tmux on an isolated -L socket: when paste-buffer fails after
    /// load-buffer succeeded, the loaded buffer must not linger
    /// server-global with the payload in it.
    #[tokio::test]
    async fn paste_failure_deletes_the_loaded_buffer() {
        if !cyclops_testrig::tmux_available() {
            eprintln!("skipping: tmux not on PATH");
            return;
        }
        let pid = std::process::id();
        // The rig owns the server: teardown kills it AND unlinks its socket,
        // and runs on unwind, which a kill at the end of the body does not.
        let tmux = cyclops_testrig::TmuxServer::new("dubuf");
        let spool = cyclops_proto::scratch::scratch_dir("cyc-dubuf-spool");
        let cfg = cyclops_tmux::ControlConfig::new_session("dubuf")
            .on_socket(tmux.socket())
            .with_config_file("/dev/null")
            .with_buffer_spool_dir(&spool);
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("tmux spawns");
        let client = Arc::new(client);
        let injector = TmuxInjector {
            client: Arc::clone(&client),
            buffer: format!("cyc-{pid}-t"),
        };
        // %9999 does not exist: load-buffer succeeds, paste-buffer fails.
        let err = injector.paste("%9999", "secret payload").await.unwrap_err();
        assert_eq!(err, "paste_failed");
        let buffers = client.command("list-buffers").await.unwrap_or_default();
        assert!(
            buffers.iter().all(|l| !l.contains(&injector.buffer)),
            "buffer lingered after failed paste: {buffers:?}"
        );
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&spool);
    }
}
