//! NDJSON socket protocol: one JSON object per line.
//!
//! Server -> client, first line of every connection: [`Hello`].
//! Client -> server: [`Request`] lines, each carrying a caller-chosen `id`.
//! Server -> client: [`Response`] lines echoing that `id`, plus unsolicited
//! [`Event`] lines on connections that subscribed via `events.subscribe`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::AgentState;

/// First line the server writes on every connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// Daemon semver, e.g. "0.1.0".
    pub cyclops: String,
    /// Wire protocol version. Mismatch warns, never disconnects.
    pub proto: u32,
    /// Random id minted at daemon start. Changes on every restart, so a
    /// client can detect that ledger seq numbering restarted.
    pub boot_id: String,
}

/// One request line. `id` is echoed verbatim in the response; callers may use
/// numbers or strings. `params` is decoded per method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// One response line. Exactly one of `result` / `error` is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Response {
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: Value, code: &str, message: impl Into<String>) -> Self {
        Response {
            id,
            result: None,
            error: Some(WireError {
                code: code.into(),
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    /// Stable machine-readable code, e.g. "unknown_method", "denied",
    /// "no_such_target", "timeout", "occupant_changed".
    pub code: String,
    pub message: String,
    /// Structured extras for codes that carry evidence, e.g. agent.wait's
    /// timeout reports the state the target was in. Absent on most errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Unsolicited push on subscribed connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    #[serde(default)]
    pub data: Value,
    /// Ledger seq when the event corresponds to a ledger line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

// ---------------------------------------------------------------------------
// Method params and results. Methods use dot notation: "ping", "status",
// "msg.send", "msg.history", "msg.thread", "agent.wait", "agent.state.report",
// "pane.read", "events.subscribe", "admin.notify", "hooks.verify",
// "hooks.selftest", "theme.reload", "daemon.quiesce".
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub pong: bool,
    pub ts: u64,
}

/// `daemon.quiesce` params: hold the delivery pipeline still so a restart
/// loses nothing. Every member defaults so a bare call gets the shipped
/// bounds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuiesceParams {
    /// Bound on waiting for deliveries already past the paste to resolve.
    /// None takes the daemon's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// `daemon.quiesce` result. `quiet: true` means nothing is between the
/// paste and a resolved state anywhere in the fleet, and the pipeline is
/// held still for the stop that should follow (the daemon un-holds itself
/// if none does). Deliveries that have not reached a pane yet do not block
/// quiet: a restart requeues them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuiesceResult {
    pub quiet: bool,
    /// `"<msg id> -> <recipient>"` for each delivery still past the paste
    /// when the wait ran out. Empty when quiet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_flight: Vec<String>,
}

/// `status` params. Absent on every caller that predates the field, which
/// is why every member defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusParams {
    /// Ask for [`StatusResult::open_deliveries`]. Off by default because
    /// answering it folds the session ledgers.
    ///
    /// Any caller that SHOWS the eye must ask: half the rule
    /// ([`crate::attention`]) lives in this field, and an answer without
    /// it counts blocked panes alone. A caller that only reads pane state
    /// pays nothing by leaving it off.
    #[serde(default)]
    pub open_deliveries: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub daemon_version: String,
    pub proto: u32,
    pub boot_id: String,
    pub uptime_ms: u64,
    pub tmux_version: String,
    pub sessions: Vec<SessionStatus>,
    /// Deliveries whose latest recorded state still needs a human, folded
    /// from the whole record rather than a recent window, so age never
    /// hides one. Served only when [`StatusParams::open_deliveries`] asked
    /// for it. Additive optional field: old daemons omit it, old clients
    /// ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_deliveries: Vec<OpenDelivery>,
    /// The detection manifests this daemon is running on. Additive optional
    /// field: old daemons omit it, old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifests: Option<Manifests>,
    /// The daemon's own process id, so `cyclops daemon stop` signals the
    /// process that is actually holding the socket.
    ///
    /// It comes from the daemon rather than from a pid file because a file
    /// outlives the process that wrote it, and a stop reading a stale one
    /// would signal whatever inherited the number. Additive optional
    /// field: old daemons omit it, old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// A duration in the roster's words: seconds under a minute, then
/// minutes, then hours with the leftover minutes. Never more precise
/// than a reader can act on.
pub fn duration_words(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    match m % 60 {
        0 => format!("{h}h"),
        rest => format!("{h}h {rest}m"),
    }
}

/// What the daemon loaded for pane detection, and where from.
///
/// A pane no manifest binds reads `? unknown`, and a delivery to an unknown
/// pane ends in attention_required. Two different problems wear that same
/// label: the daemon holding no manifests at all, in which case nothing on
/// the machine can be addressed, and a full set that binds nothing in one
/// pane. The fixes are nothing alike, so a surface explaining an unknown
/// pane has to be able to tell them apart.
///
/// Absence of the whole field is a daemon that predates it, which is not
/// the same statement as `ids` being empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifests {
    /// Loaded manifest ids, sorted, e.g. ["agy", "claude", "codex"].
    /// Empty means every pane reads unknown.
    pub ids: Vec<String>,
    /// Directory they were read from. None when no directory was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// One delivery still waiting on a human: redelivery exhausted, or a quota
/// park (which never auto-retries, so nothing but an operator moves it).
///
/// Identity is (to, id), the same pair the delivery chain carries in the
/// ledger, so a client can match a seeded item against the transitions it
/// later sees on the wire and clear it on the right one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenDelivery {
    /// Message id this delivery belongs to, e.g. "m-3f9c2a".
    pub id: String,
    /// Recipient as addressed.
    pub to: String,
    pub state: crate::ledger::DeliveryState,
    /// Unix ms of the transition that left it here.
    pub ts: u64,
    /// Machine-readable cause, the same one the ledger record carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub name: String,
    /// True while the control-mode connection to this session is up.
    pub attached: bool,
    pub panes: Vec<PaneStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStatus {
    /// tmux pane id, e.g. "%3". Stable for the pane's lifetime.
    pub pane_id: String,
    pub window_id: String,
    pub window_name: String,
    /// Cyclops label if the pane was adopted/named, e.g. "reviewer".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Manifest id driving detection for this pane, e.g. "claude".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    pub title: String,
    pub current_command: String,
    pub dead: bool,
    pub in_mode: bool,
    /// The second status answer, additive and stamped by fusion: may a
    /// terminal write go into this pane right now. `state` alone cannot
    /// say, because idle means no turn is running, which is also true of a
    /// pane holding somebody's half-typed message. An older daemon sends
    /// neither field, and absent evidence is not permission.
    #[serde(default)]
    pub write_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_block: Option<String>,
    pub width: u32,
    pub height: u32,
    pub state: AgentState,
    /// How long the pane has been in `state`, in milliseconds, from the
    /// daemon's own clock: it is the one process that saw the transition.
    /// None from a daemon that predates the field, and for a pane whose
    /// state has not been computed yet. Additive optional field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_ms: Option<u64>,
    /// Hook liveness for adopted panes whose manifest declares hooks:
    /// Some(true) once any hook edge has been seen from the pane's CURRENT
    /// occupant this daemon run, Some(false) while none has (amendment c:
    /// configuration does not equal subscription, finding F1; an occupant
    /// restart invalidates its predecessor's edges). None when the pane is
    /// unadopted or its manifest declares no hooks. Additive optional
    /// field: old daemons omit it, old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_verified: Option<bool>,
    /// The bound manifest's human-readable name, e.g. "Claude Code" for
    /// manifest id "claude". The daemon already loaded this at boot; a
    /// client used to re-derive it by re-parsing manifest TOML off disk
    /// itself (see the ownership-cleanup paragraph in
    /// `.agents/planning/2026-08-03-cyclops-workspace-tui/recommendation.md`).
    /// None when the pane has no bound manifest, and always from a daemon
    /// that predates the field. Purely cosmetic: a caller that gets None
    /// falls back to the bare `manifest` id, never treats a miss as an
    /// error. Additive optional field: old daemons omit it, old clients
    /// ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneReadParams {
    /// Cyclops label or tmux pane id.
    pub target: String,
    #[serde(default)]
    pub source: PaneReadSource,
    /// Cap on returned lines (visible/recent sources, and the raw half
    /// of a detection read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// Detection reads only: also capture the visible pane, so the answer
    /// carries the screen beside what the sensors made of it. What
    /// `cyclops read --raw` asks for; debugging a manifest needs both
    /// halves in one look. Additive: old daemons ignore it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_raw: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneReadSource {
    /// capture-pane of the visible grid.
    #[default]
    Visible,
    /// capture-pane including scrollback tail.
    Recent,
    /// The detection view: fused state plus per-sensor readings.
    Detection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneReadResult {
    pub target: String,
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detection: Option<crate::state::Detection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeParams {
    /// Event name prefixes to receive; empty means everything.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Replay ledger-backed events after this seq before going live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
}

/// `workspace_ui.get` params. Additive; older daemons omit the method.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceUiGetParams {
    #[serde(default)]
    pub protocol_version: u32,
}

/// Last-active workspace/tab the workspace UI persisted through the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceUiGetResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_window: Option<String>,
}

/// `workspace_ui.set` params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceUiSetParams {
    pub session: String,
    pub window_id: String,
    #[serde(default)]
    pub protocol_version: u32,
}

// --- Messaging (implemented from M1; types are part of protocol v1) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsgSendParams {
    /// Recipient labels. Broadcast is explicit: multiple labels or "*".
    pub to: Vec<String>,
    pub subject: String,
    #[serde(default)]
    pub body: String,
    /// Announcement expecting no reply.
    #[serde(default)]
    pub fyi: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Compose send-and-wait: block until the recipient reaches a state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitSpec {
    pub until: WaitUntil,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitUntil {
    /// No turn is running. A runtime state only: reaching it does NOT
    /// prove the composer is empty or that a write would be accepted.
    /// Write-readiness is `Detection::write_ready`, and delivery asks it
    /// again at paste time regardless of what a wait returned.
    Idle,
    /// The turn started by our delivery ended.
    Done,
    /// The agent entered any blocked_* state.
    Blocked,
}

/// Receipt returned by msg.send: push state, pull context. The sender gets
/// the target's disposition, never auto-attached history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsgSendResult {
    pub msg_id: String,
    pub seq: u64,
    pub deliveries: Vec<DeliveryReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub to: String,
    pub state: crate::ledger::DeliveryState,
    /// Queue depth ahead of this message when state is queued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// Normalized reason the in-flight head delivery is held at the gate.
    /// Additive optional field: older daemons omit it and older clients
    /// ignore it. This is deliberately a stable token, never a manifest
    /// rule id (for example, `blocked`, not `blocked:trust_dialog`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<String>,
    /// Human hint, e.g. "resets in 135h57m", or the gate cause the caller
    /// words for itself, e.g. "no_manifest".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// tmux pane this delivery resolved to, e.g. "%3". Absent when the
    /// recipient answered to no pane at all, and on the record surfaces,
    /// which fold from ledger lines that carry no pane.
    ///
    /// It is here because the fixes for a stopped delivery are per pane:
    /// pinning a manifest names the pane, and a receipt that cannot say
    /// which pane leaves the reader to go and find it. Additive optional
    /// field: old daemons omit it, old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default = "default_history_limit")]
    pub limit: u32,
    /// Resume after this seq.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
}

fn default_history_limit() -> u32 {
    50
}

/// msg.history result: matching msg/fyi lines, oldest first (newest last).
/// Each line's `deliveries` are folded to the latest recorded state per
/// recipient, so a broadcast reads as one msg fact with N current badges.
/// The ledger files themselves are never rewritten; folding is a read model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResult {
    pub lines: Vec<crate::ledger::LedgerLine>,
    /// Seq of the newest returned line; pass as `cursor` to resume after it.
    /// Only present with a single watched session, where per-file seqs are
    /// unambiguous. Absent when nothing matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
    /// Opaque composite cursor covering every watched session; pass back as
    /// the request's `cursor2` param to resume after the returned lines.
    /// The daemon issues it whenever more than one session is watched (the
    /// plain `cursor` seq would be ambiguous there) and on any `cursor2`
    /// paged request. Additive optional field: old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor2: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadParams {
    pub id: String,
}

/// msg.thread result: the message's folded msg line, every state/gate line
/// sharing its id, and every msg whose reply_to chains to it (also folded),
/// ordered oldest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadResult {
    pub lines: Vec<crate::ledger::LedgerLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWaitParams {
    pub target: String,
    pub until: WaitUntil,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Posted by the `cyclops hook` receiver that vendor hook configs invoke.
/// `seq` is a per-source monotonic counter so out-of-order arrival is
/// detectable (hooks are separate short-lived processes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateReportParams {
    /// Cyclops label of the reporting agent pane, when the hook knows it.
    ///
    /// Optional because a hook often does not know it. The daemon derives
    /// the reporting origin from the authenticated socket peer, which it
    /// must compute anyway to verify the report; a value supplied here is
    /// an ASSERTION about that origin, checked against it and denied when
    /// it disagrees, never trusted on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Normalized event name, e.g. "UserPromptSubmit", "Stop".
    pub event: String,
    /// Per-label counter, sent only by a client that HAS a label. A
    /// label-free report carries none, so a dedupe window is never keyed
    /// by a name the daemon has not verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Raw vendor payload, passed through for matching and audit.
    #[serde(default)]
    pub payload: Value,
}

// --- Hook liveness (M2: hooks install + startup self-test, amendment c) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksVerifyParams {
    /// Cyclops label or tmux pane id.
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksVerifyResult {
    pub target: String,
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// ACK capability tier: 1 = payload-matchable hook ACK, 2 = screen
    /// evidence only.
    pub tier: u8,
    /// Same semantics as PaneStatus::hooks_verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks_verified: Option<bool>,
    pub events: Vec<HookEdgeAge>,
}

/// One hook event's last observed edge. `last_seen_ms_ago` is None when
/// the event has never been seen this daemon run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEdgeAge {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_ms_ago: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksSelftestParams {
    pub target: String,
    /// Cap on how long the daemon waits for the self-test delivery to
    /// resolve. Defaults server-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksSelftestResult {
    pub target: String,
    pub msg_id: String,
    /// Bound manifest id ("claude", "codex", "agy"): the CLI kind that
    /// `cyclops hooks install` takes, so failure copy can name a runnable
    /// command. Absent when no manifest binds the pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    pub tier: u8,
    /// Delivery state at resolution (or the in-flight state at timeout).
    pub state: crate::ledger::DeliveryState,
    /// True when the recipient's ACK hook fired carrying the marker.
    pub hook_ack: bool,
    pub waited_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminNotifyParams {
    pub level: NotifyLevel,
    pub subject: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyLevel {
    Fyi,
    ActionRequired,
    Urgent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_tolerates_unknown_fields() {
        let line = r#"{"id":1,"method":"ping","params":{},"future_field":true}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.method, "ping");
    }

    #[test]
    fn response_shape() {
        let r = Response::ok(serde_json::json!(7), serde_json::json!({"pong":true}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"id\":7"));
        assert!(!s.contains("error"));
    }

    #[test]
    fn missing_params_defaults_to_null() {
        let req: Request = serde_json::from_str(r#"{"id":"a","method":"status"}"#).unwrap();
        assert!(req.params.is_null());
    }

    /// The open-delivery seed is additive in both directions: a daemon that
    /// predates it omits the field, and a client that predates it omits the
    /// param. Neither side may fail on the other's absence.
    #[test]
    fn open_deliveries_is_additive_and_absence_tolerant() {
        let old = r#"{"daemon_version":"0.1.0","proto":1,"boot_id":"b","uptime_ms":1,
            "tmux_version":"3.6a","sessions":[]}"#;
        let s: StatusResult = serde_json::from_str(old).expect("old status still decodes");
        assert!(s.open_deliveries.is_empty());
        // Nothing open serializes to nothing on the wire.
        assert!(!serde_json::to_string(&s)
            .unwrap()
            .contains("open_deliveries"));

        let p: StatusParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!p.open_deliveries, "the seed is opt-in");
        let p: StatusParams =
            serde_json::from_value(serde_json::json!({"open_deliveries": true})).unwrap();
        assert!(p.open_deliveries);

        let d = OpenDelivery {
            id: "m-aaaaaa".into(),
            to: "implementer".into(),
            state: crate::ledger::DeliveryState::ParkedBlockedQuota,
            ts: 1_754_000_002_600,
            cause: Some("blocked_quota".into()),
        };
        let back: OpenDelivery = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back.state, crate::ledger::DeliveryState::ParkedBlockedQuota);
        assert_eq!(back.to, "implementer");
    }

    #[test]
    fn held_receipt_is_additive_and_omits_vendor_rule_ids() {
        let old = r#"{"to":"reviewer","state":"queued","position":0}"#;
        let receipt: DeliveryReceipt = serde_json::from_str(old).unwrap();
        assert_eq!(receipt.held_by, None);

        let receipt = DeliveryReceipt {
            to: "reviewer".into(),
            state: crate::ledger::DeliveryState::Queued,
            position: Some(0),
            held_by: Some("blocked".into()),
            note: None,
            pane: None,
        };
        let wire = serde_json::to_string(&receipt).unwrap();
        let value: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(value["held_by"], "blocked");
        assert_ne!(value["held_by"], "blocked:trust_dialog");
    }

    /// A daemon saying "I loaded none" and a daemon too old to say are two
    /// different facts, and the surface that explains an unknown pane picks
    /// a different next step for each. The wire has to keep them apart.
    #[test]
    fn a_silent_daemon_is_not_a_daemon_with_no_manifests() {
        let old = r#"{"daemon_version":"0.1.0","proto":1,"boot_id":"b","uptime_ms":1,
            "tmux_version":"3.6a","sessions":[]}"#;
        let s: StatusResult = serde_json::from_str(old).expect("old status still decodes");
        assert!(s.manifests.is_none(), "silence is not an empty set");

        let none_loaded = r#"{"daemon_version":"0.1.0","proto":1,"boot_id":"b","uptime_ms":1,
            "tmux_version":"3.6a","sessions":[],"manifests":{"ids":[]}}"#;
        let s: StatusResult = serde_json::from_str(none_loaded).expect("decodes");
        let m = s.manifests.expect("the daemon said so");
        assert!(m.ids.is_empty());
        assert!(m.dir.is_none());

        let loaded = r#"{"daemon_version":"0.1.0","proto":1,"boot_id":"b","uptime_ms":1,
            "tmux_version":"3.6a","sessions":[],
            "manifests":{"ids":["agy","claude"],"dir":"/h/manifests"}}"#;
        let s: StatusResult = serde_json::from_str(loaded).expect("decodes");
        let m = s.manifests.expect("the daemon said so");
        assert_eq!(m.ids, vec!["agy", "claude"]);
        assert_eq!(m.dir.as_deref(), Some("/h/manifests"));
    }
}
