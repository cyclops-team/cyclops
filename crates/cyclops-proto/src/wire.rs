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
// "hooks.selftest".
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub pong: bool,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub daemon_version: String,
    pub proto: u32,
    pub boot_id: String,
    pub uptime_ms: u64,
    pub tmux_version: String,
    pub sessions: Vec<SessionStatus>,
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
    pub width: u32,
    pub height: u32,
    pub state: AgentState,
    /// Hook liveness for adopted panes whose manifest declares hooks:
    /// Some(true) once any hook edge has been seen from the pane's CURRENT
    /// occupant this daemon run, Some(false) while none has (amendment c:
    /// configuration does not equal subscription, finding F1; an occupant
    /// restart invalidates its predecessor's edges). None when the pane is
    /// unadopted or its manifest declares no hooks. Additive optional
    /// field: old daemons omit it, old clients ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_verified: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneReadParams {
    /// Cyclops label or tmux pane id.
    pub target: String,
    #[serde(default)]
    pub source: PaneReadSource,
    /// Cap on returned lines (visible/recent sources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
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
    /// Composer ready, no turn running.
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
    /// Human hint, e.g. "resets in 135h57m".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    /// Cyclops label of the reporting agent pane.
    pub agent: String,
    /// Normalized event name, e.g. "UserPromptSubmit", "Stop".
    pub event: String,
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
}
