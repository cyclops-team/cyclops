//! NDJSON socket protocol: one JSON object per line.
//!
//! Server -> client, first line of every connection: [`Hello`].
//! Client -> server: [`Request`] lines, each carrying a caller-chosen `id`.
//! Server -> client: [`Response`] lines echoing that `id`, plus unsolicited
//! [`Event`] lines on connections that subscribed via `events.subscribe`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::{AgentState, ComposerProof, ComposerState};

/// First line the server writes on every connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// Daemon semver, e.g. "0.1.0".
    pub cyclops: String,
    /// Exact source build stamped into the daemon binary.
    ///
    /// Additive and optional so a current client can still inspect a daemon
    /// that predates build identity. `None` is itself useful evidence that
    /// the running daemon is old.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Exact daemon process generation observed once at startup.
    ///
    /// A PID alone can be reused. Update and rollback require both fields
    /// before they may signal a running daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_process: Option<crate::ProcessInstanceId>,
    /// Canonical executable path observed once by the daemon itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_executable: Option<String>,
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

/// Coarse workspace projections invalidated by a durable messaging fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagesChangedArea {
    Messages,
    Mailboxes,
    Notifications,
    Attention,
}

/// Content-free wake signal for clients that rebuild from messages.snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesChangedData {
    pub workspace_id: crate::identity::WorkspaceId,
    pub workspace_seq: u64,
    pub changed: BTreeSet<MessagesChangedArea>,
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

/// `daemon.shutdown` parameters. The daemon quiesces and stops itself only
/// when these values still identify this exact boot and process generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonShutdownParams {
    pub daemon_process: crate::ProcessInstanceId,
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonShutdownResult {
    pub stopping: bool,
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

/// Content-free warning about a notification that cannot wake its recipient.
/// The durable recipient key and attempt id keep the warning bound to one
/// route and one notification while display labels remain presentation only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusDiagnostic {
    pub code: String,
    pub message_id: crate::mailbox::MessageId,
    pub notification_attempt: crate::notification::NotificationAttemptId,
    pub recipient: crate::identity::RecipientKey,
    pub recipient_label: String,
    pub pane_id: String,
}

/// Closed operator actions advertised by `status` diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusNextAction {
    /// Workspace administrator action. Agent callers may display it but may
    /// not execute it with their mailbox identity.
    WithdrawNotification,
}

/// One durable wake stopped before any terminal write.
///
/// The nested recipient row is the same daemon-owned projection used by
/// `messages.snapshot`, so status cannot invent a different FIFO, route, or
/// authorization answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusBlockedNotification {
    pub message_id: crate::mailbox::MessageId,
    pub notification_attempt: crate::notification::NotificationAttemptId,
    pub recipient: MessageRecipientSummary,
    pub waiting_age_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<StatusNextAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub daemon_version: String,
    /// Exact source build stamped into the daemon binary.
    ///
    /// Old daemons omit it and old clients ignore it. A missing value does
    /// not mean the daemon matches the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_build: Option<String>,
    /// Same daemon process generation carried by the connection hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_process: Option<crate::ProcessInstanceId>,
    /// Same canonical executable path carried by the connection hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_executable: Option<String>,
    pub proto: u32,
    pub boot_id: String,
    pub uptime_ms: u64,
    pub tmux_version: String,
    pub sessions: Vec<SessionStatus>,
    /// Current display routes for durable mailboxes.
    ///
    /// The key is the endpoint identity. The label is presentation data
    /// resolved once by clients that offer human-friendly selectors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mailbox_routes: Vec<StatusMailboxRoute>,
    /// Pending messages in the workspace administrator's durable inbox.
    #[serde(default)]
    pub admin_unread: u64,
    /// Deliveries whose latest recorded state still needs a human, folded
    /// from the whole record rather than a recent window, so age never
    /// hides one. Served only when [`StatusParams::open_deliveries`] asked
    /// for it. Additive optional field: old daemons omit it, old clients
    /// ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_deliveries: Vec<OpenDelivery>,
    /// Content-free operational diagnostics derived from exact live routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<StatusDiagnostic>,
    /// Durable pre-write wake failures. Body-free and independent of route
    /// availability, so a detached recipient cannot hide operator work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_notifications: Vec<StatusBlockedNotification>,
    /// Total durable pre-write wake failures before the bounded row sample.
    #[serde(default)]
    pub blocked_notifications_total: u64,
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

/// One current display label bound to an immutable mailbox endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMailboxRoute {
    pub recipient: crate::identity::RecipientKey,
    pub label: String,
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
    /// Immutable recipient on current delivery rows. Absent only for
    /// compatibility records written before durable endpoint identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<crate::identity::RecipientKey>,
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
    /// Content class from the daemon's current composer observation.
    #[serde(default)]
    pub composer: ComposerState,
    /// Evidence strength behind `composer`.
    #[serde(default)]
    pub composer_proof: ComposerProof,
    /// Exact durable attempt when the composer barrier names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_attempt: Option<crate::NotificationAttemptId>,
    /// Stable, content-free reason why composer ownership is unresolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composer_reason: Option<String>,
    /// Active durable composer barriers considered for this pane.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub composer_candidates: u32,
    /// Current durable notification state for `notification_attempt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_state: Option<crate::NotificationState>,
    /// Current durable mailbox state for the attempt's message and recipient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_state: Option<ComposerMessageState>,
    /// Concrete next step for an owned or unresolved Cyclops notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<ComposerNextAction>,
    pub width: u32,
    pub height: u32,
    pub state: AgentState,
    /// How long the pane has been in `state`, in milliseconds, from the
    /// daemon's own clock: it is the one process that saw the transition.
    /// None from a daemon that predates the field, and for a pane whose
    /// state has not been computed yet. Additive optional field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_ms: Option<u64>,
    /// Whether current Working has visual or exact lifecycle confirmation.
    /// False is a provisional authenticated start, not runtime idleness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_confirmed: Option<bool>,
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

/// Body-free mailbox state used by the composer status projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerMessageState {
    Pending,
    Claimed,
    DeliveredDirect,
    Superseded,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

impl From<&crate::MailboxEntryState> for ComposerMessageState {
    fn from(state: &crate::MailboxEntryState) -> Self {
        match state {
            crate::MailboxEntryState::Pending => Self::Pending,
            crate::MailboxEntryState::Claimed { .. } => Self::Claimed,
            crate::MailboxEntryState::DeliveredDirect { .. } => Self::DeliveredDirect,
            crate::MailboxEntryState::Superseded { .. } => Self::Superseded,
        }
    }
}

/// Closed next steps for a Cyclops-owned or unresolved composer barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerNextAction {
    /// The active worker will submit the exact staged notification.
    AutomaticSubmit,
    /// The active worker will reconcile one uncertain or consumed attempt.
    AutomaticReconcile,
    /// Workspace administrator inspection of an exact `AttentionRequired` attempt.
    InspectAttention,
    /// Inspect durable mailbox and notification state without assuming one attempt.
    InspectMessages,
    /// Diagnose an inactive or faulted post-write worker before restarting it.
    CheckHealth,
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
    /// Exact durable recipients. New interactive clients use this instead of
    /// labels so a rename cannot retarget a message between selection and send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_keys: Option<Vec<crate::identity::RecipientKey>>,
    /// Authenticated sender observed by an interactive client before send.
    /// The daemon refuses when the current socket caller differs, so a stale
    /// UI cannot silently send under another mailbox identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_caller: Option<crate::identity::RecipientKey>,
    pub subject: String,
    #[serde(default)]
    pub body: String,
    /// Announcement expecting no reply.
    #[serde(default)]
    pub fyi: bool,
    /// Sender-scoped idempotency key for exact retry deduplication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
    /// Message this send replies to. Routing and subject are derived by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<crate::mailbox::MessageId>,
    /// Removed send-and-wait field retained for protocol compatibility.
    /// The mailbox `msg.send` endpoint rejects any non-null value because
    /// pane state cannot prove that a specific message was completed.
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
    /// A working-to-idle edge was observed for the same pane occupant.
    /// This does not identify which message or task the turn handled.
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
    /// False when a client idempotency key matched an existing message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inserted: Option<bool>,
}

/// Why the recipient FIFO head has no live Cyclops wake owner.
///
/// This is separate from the durable notification state. A message may be
/// accepted behind another FIFO item while that head is explicitly blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageWakeBlock {
    DaemonStopping,
    RouteUnavailable,
    AttentionResolutionPending,
    WorkerFaulted,
    WorkerSupervisorExited,
    EnqueueRefused,
    ComposerOwnershipUnproven,
    SchedulerStateUnavailable,
}

impl MessageWakeBlock {
    /// Stable protocol spelling used by terminal and JSON clients.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::DaemonStopping => "daemon_stopping",
            Self::RouteUnavailable => "route_unavailable",
            Self::AttentionResolutionPending => "attention_resolution_pending",
            Self::WorkerFaulted => "worker_faulted",
            Self::WorkerSupervisorExited => "worker_supervisor_exited",
            Self::EnqueueRefused => "enqueue_refused",
            Self::ComposerOwnershipUnproven => "composer_ownership_unproven",
            Self::SchedulerStateUnavailable => "scheduler_state_unavailable",
        }
    }

    /// Human wording shared by receipts, snapshots, and operator surfaces.
    pub const fn label(self) -> &'static str {
        match self {
            Self::DaemonStopping => "daemon stopping",
            Self::RouteUnavailable => "route unavailable",
            Self::AttentionResolutionPending => "attention resolution pending",
            Self::WorkerFaulted => "worker faulted",
            Self::WorkerSupervisorExited => "worker supervisor exited",
            Self::EnqueueRefused => "scheduler refused ownership",
            Self::ComposerOwnershipUnproven => "complete composer ownership unproven",
            Self::SchedulerStateUnavailable => "scheduler state unavailable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub to: String,
    pub state: crate::ledger::DeliveryState,
    /// Durable mailbox notification state when this receipt came from the
    /// workspace messaging path. Absent on legacy payload deliveries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_state: Option<MessageNotificationState>,
    /// Exact quota disposition for workspace notifications. The legacy
    /// `notification_state` remains `attention_required` so older clients
    /// can decode the receipt and still treat it as operator-visible work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_state: Option<MessageQuotaState>,
    /// Exact claim settlement hidden behind the compatibility state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_settlement: Option<MessageNotificationSettlement>,
    /// Exact scheduler reason the recipient FIFO head has no live wake owner.
    /// Absent for a worker-owned head and for an ordinary item queued behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_block: Option<MessageWakeBlock>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Match the authoritative sender endpoint, never its display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<crate::identity::RecipientKey>,
}

/// One body-free pending inbox summary for the authenticated caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxSummaryEntry {
    pub message_id: crate::mailbox::MessageId,
    /// Authoritative endpoint. Older daemons omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<crate::identity::RecipientKey>,
    pub sender_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub ts: u64,
    pub thread_root: crate::mailbox::MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxListResult {
    pub entries: Vec<InboxSummaryEntry>,
}

fn default_recent_settled() -> u32 {
    20
}

/// Body-free workspace message snapshot parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesSnapshotParams {
    /// Keep every active message plus this many most-recent settled messages.
    #[serde(default = "default_recent_settled")]
    pub recent_settled: u32,
}

impl Default for MessagesSnapshotParams {
    fn default() -> Self {
        Self {
            recent_settled: default_recent_settled(),
        }
    }
}

fn default_follow_limit() -> u32 {
    128
}

/// Cursor read for durable, body-free message arrivals.
///
/// Unlike `messages.snapshot`, this never drops settled rows to keep a
/// queue compact. The caller advances only to the `through_seq` returned
/// by each bounded page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesFollowParams {
    pub after_seq: u64,
    #[serde(default = "default_follow_limit")]
    pub limit: u32,
}

/// A message's direction relative to the authenticated caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    Inbound,
    Outbound,
    SelfAddressed,
    /// An administrator observing a message they did not send or receive.
    Workspace,
}

fn default_recipient_direction() -> MessageDirection {
    MessageDirection::Workspace
}

/// Read-side compatibility state. `NotStarted` means no active wake exists;
/// an additive settlement field distinguishes a withdrawn pre-write attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageNotificationState {
    NotStarted,
    Queued,
    Gating,
    Writing,
    Staged,
    Submitted,
    Notified,
    AttentionRequired,
    Superseded,
}

impl From<crate::notification::NotificationState> for MessageNotificationState {
    fn from(state: crate::notification::NotificationState) -> Self {
        use crate::notification::NotificationState;
        match state {
            NotificationState::Queued => Self::Queued,
            NotificationState::Gating => Self::Gating,
            // Keep the original closed state vocabulary decodable by old
            // clients. New clients read `pre_write_cause` for the block.
            NotificationState::BlockedPreWrite => Self::Gating,
            // Keep the original closed wire vocabulary decodable by old
            // clients. New clients read `quota_state` for the exact phase.
            NotificationState::QuotaHeld | NotificationState::QuotaResetObserved => {
                Self::AttentionRequired
            }
            NotificationState::Writing => Self::Writing,
            NotificationState::Staged => Self::Staged,
            // The terminal intent is durable, but a submit key is not proven
            // until the following Submitted fact. Keep old clients honest.
            NotificationState::Submitting => Self::Staged,
            NotificationState::Submitted => Self::Submitted,
            NotificationState::Notified => Self::Notified,
            NotificationState::AttentionRequired => Self::AttentionRequired,
            // A pre-write withdrawal never proved that its wake reached the
            // recipient. Newer clients read the additive settlement fields.
            NotificationState::Withdrawn => Self::NotStarted,
            NotificationState::WithdrawnByOperator => Self::NotStarted,
            // Bytes crossed the write boundary but were cleared before
            // submit. Preserve that conservative phase for older clients;
            // the additive settlement names the exact outcome.
            NotificationState::WithdrawnAfterStaging => Self::Staged,
            NotificationState::Superseded => Self::Superseded,
        }
    }
}

/// Additive quota detail for a notification whose compatibility state is
/// `attention_required`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageQuotaState {
    Held,
    ResetObserved,
}

impl MessageQuotaState {
    pub fn from_notification(state: crate::notification::NotificationState) -> Option<Self> {
        use crate::notification::NotificationState;
        match state {
            NotificationState::QuotaHeld => Some(Self::Held),
            NotificationState::QuotaResetObserved => Some(Self::ResetObserved),
            _ => None,
        }
    }
}

/// Additive detail for a notification settled by an authenticated claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageNotificationSettlement {
    WithdrawnByClaim,
}

impl MessageNotificationSettlement {
    pub fn from_notification(state: crate::notification::NotificationState) -> Option<Self> {
        use crate::notification::NotificationState;
        match state {
            NotificationState::Withdrawn | NotificationState::WithdrawnAfterStaging => {
                Some(Self::WithdrawnByClaim)
            }
            _ => None,
        }
    }
}

/// Current body-free notification projection for one recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageNotificationSummary {
    pub state: MessageNotificationState,
    /// Exact durable reason this wake has no live terminal owner.
    ///
    /// Missing values identify legacy records whose exact scheduler outcome
    /// was not journaled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_block: Option<MessageWakeBlock>,
    /// Exact quota phase. When present, `state` remains
    /// `attention_required` for old-client compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_state: Option<MessageQuotaState>,
    /// Exact claim settlement hidden behind the compatibility state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<MessageNotificationSettlement>,
    /// True when an administrator withdrew this exact pre-write wake.
    ///
    /// The mailbox item remains pending. The optional field keeps older
    /// clients compatible while distinguishing this from a recipient claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_withdrawn: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<crate::notification::NotificationAttemptId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::notification::NotificationAttentionCause>,
    /// Content-free detail recorded with a `verify_failed` transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_outcome: Option<crate::notification::NotificationVerifyOutcome>,
    /// Exact reason an attempt stopped before any terminal write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_write_cause: Option<crate::notification::NotificationPreWriteCause>,
    /// Pane width observed for a compatibility-encoded width block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_write_pane_width: Option<u32>,
    /// Minimum width recorded with `pre_write_pane_width`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_write_required_pane_width: Option<u32>,
    /// Present only for attention-required attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_cleared: Option<bool>,
    /// Durable resolution of this exact attention attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<crate::notification::NotificationResolution>,
    /// Pre-key terminal action intent without a final outcome.
    /// Never infer that the terminal accepted a key from this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_intent: Option<crate::notification::NotificationResolution>,
    /// The terminal accepted the action key, but composer consumption and the
    /// final resolution are not yet proven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_action_accepted: Option<crate::notification::NotificationResolution>,
    /// Content-free evidence that an accepted Complete action consumed the
    /// staged composer input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_consumption_observed:
        Option<crate::notification::NotificationResolutionConsumptionObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
}

impl MessageNotificationSummary {
    /// Exact observed and required widths for a format-specific pre-write block.
    pub fn pane_width_block(&self) -> Option<(u32, u32)> {
        if self.pre_write_cause
            != Some(crate::notification::NotificationPreWriteCause::WriteReadinessChanged)
        {
            return None;
        }
        self.pre_write_pane_width
            .zip(self.pre_write_required_pane_width)
            .filter(|(observed, required)| observed < required)
    }
}

/// One recipient's mailbox and notification state for a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecipientSummary {
    pub recipient: crate::identity::RecipientKey,
    pub label: String,
    /// This recipient row's direction relative to the authenticated caller.
    #[serde(default = "default_recipient_direction")]
    pub direction: MessageDirection,
    /// Whether this recipient row belongs in the caller's Work view.
    #[serde(default)]
    pub needs_action: bool,
    /// Whether the authenticated caller may resolve this recipient's open alarm.
    ///
    /// Clients must not infer this authority from direction, mailbox state, or
    /// notification state. The daemon answers it for the exact recipient row.
    #[serde(default)]
    pub can_manage_attention: bool,
    /// Whether the authenticated caller may withdraw this exact unwritten wake.
    ///
    /// Clients must not infer this authority from visible notification state.
    #[serde(default)]
    pub can_withdraw_notification: bool,
    /// Current live route, separate from the immutable send-time label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_route: Option<MessageRecipientRoute>,
    /// Current route availability, outside the workspace journal watermark.
    pub available: bool,
    pub mailbox: crate::mailbox::MailboxEntryState,
    /// One-based position among the recipient's currently pending messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fifo_position: Option<u64>,
    pub notification: MessageNotificationSummary,
}

/// Current display route for one durable recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecipientRoute {
    pub label: String,
    pub pane_id: crate::identity::TmuxPaneId,
}

/// Stable body-free row returned by `messages.snapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSnapshotRow {
    pub message_id: crate::mailbox::MessageId,
    pub seq: u64,
    pub ts: u64,
    pub kind: crate::ledger::Kind,
    pub direction: MessageDirection,
    pub sender: crate::identity::RecipientKey,
    pub sender_label: String,
    pub recipients: Vec<MessageRecipientSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<crate::mailbox::MessageId>,
    pub thread_root: crate::mailbox::MessageId,
    /// Number of messages in this thread visible to the caller.
    pub thread_message_count: u64,
    pub active: bool,
    /// Whether this row belongs in the authenticated caller's Work view.
    pub needs_action: bool,
}

/// Counts cover every visible message, including settled rows omitted by the bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesSnapshotCounts {
    pub visible_messages: u64,
    pub returned_messages: u64,
    pub inbox_messages: u64,
    pub outbound_messages: u64,
    pub work_messages: u64,
    pub active_messages: u64,
    pub settled_messages: u64,
    pub pending_entries: u64,
    pub claimed_entries: u64,
    pub open_attention_entries: u64,
}

/// One authenticated, body-free read of workspace messaging state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesSnapshotResult {
    pub workspace_id: crate::identity::WorkspaceId,
    /// Authenticated caller for this snapshot. Older daemons omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<crate::identity::RecipientKey>,
    /// Highest workspace journal sequence folded into this snapshot.
    pub workspace_seq: u64,
    pub counts: MessagesSnapshotCounts,
    pub rows: Vec<MessageSnapshotRow>,
}

/// One bounded page of durable message arrivals visible to the
/// authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagesFollowResult {
    pub workspace_id: crate::identity::WorkspaceId,
    /// Cursor supplied by the caller.
    pub after_seq: u64,
    /// Highest sequence fully covered by this page. The next request uses
    /// this value as `after_seq`.
    pub through_seq: u64,
    /// More visible message rows exist before the workspace head.
    pub has_more: bool,
    pub rows: Vec<MessageSnapshotRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimDisposition {
    Claimed,
    AlreadyClaimed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxClaimParams {
    pub message_id: crate::mailbox::MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxClaimResult {
    pub disposition: ClaimDisposition,
    pub message: InboxMessage,
    /// Present when a fresh claim by id took a message that was not the
    /// recipient's oldest pending one: the oldest at claim time, which
    /// still holds that recipient's FIFO head. Absent for oldest-first
    /// claims, repeat claims, and clients that predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_oldest: Option<crate::mailbox::MessageId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub message_id: crate::mailbox::MessageId,
    pub kind: crate::ledger::Kind,
    /// Authoritative endpoint. Older daemons omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<crate::identity::RecipientKey>,
    pub sender_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<crate::mailbox::MessageId>,
    pub thread_root: crate::mailbox::MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyParams {
    pub message_id: crate::mailbox::MessageId,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequeueParams {
    pub message_id: crate::mailbox::MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequeueResult {
    pub message_id: crate::mailbox::MessageId,
    pub requeued: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationWithdrawParams {
    pub attempt_id: crate::notification::NotificationAttemptId,
    pub recipient: crate::identity::RecipientKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationWithdrawResult {
    pub attempt_id: crate::notification::NotificationAttemptId,
    pub message_id: crate::mailbox::MessageId,
    pub recipient: crate::identity::RecipientKey,
    pub disposition: NotificationWithdrawDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationWithdrawDisposition {
    Withdrawn,
    AlreadyWithdrawn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmPreviewParams {
    pub older_than_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmSummary {
    pub id: String,
    pub message_id: String,
    pub recipient: String,
    pub state: crate::ledger::DeliveryState,
    /// Why the attempt needs attention. Closed set, so an operator can
    /// tell a failed verify from a failed submit without reading the
    /// message.
    pub cause: crate::notification::NotificationAttentionCause,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmPreviewResult {
    pub entries: Vec<AlarmSummary>,
    /// Absolute cutoff used to select `entries`.
    #[serde(default)]
    pub cutoff_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmClearParams {
    pub ids: Vec<String>,
    /// Preview cutoff for an age-selected clear. Direct-id clears omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmClearResult {
    pub cleared_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionShowParams {
    /// Exact notification attempt id, or a message id with one unresolved match.
    pub id: String,
    /// Return the two local diff inputs to the authenticated client.
    #[serde(default)]
    pub diff: bool,
}

/// Five fail-closed checks for a staged notification attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionChecks {
    pub notification_exact: bool,
    pub trailer_anchored: bool,
    pub process_matches: bool,
    pub manifest_matches: bool,
    pub terminal_action_safe: bool,
}

impl AttentionChecks {
    pub fn all_pass(&self) -> bool {
        self.notification_exact
            && self.trailer_anchored
            && self.process_matches
            && self.manifest_matches
            && self.terminal_action_safe
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionShowResult {
    pub attempt_id: crate::notification::NotificationAttemptId,
    pub message_id: crate::mailbox::MessageId,
    pub recipient: crate::identity::RecipientKey,
    pub checks: AttentionChecks,
    /// Verification evidence captured when this attempt entered attention.
    ///
    /// Missing values identify legacy attempts. Current composer checks remain
    /// separate because they describe the pane at inspection time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_outcome: Option<crate::notification::NotificationVerifyOutcome>,
    /// Present only for an explicit diff request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// Present only when exact visible extraction succeeded for a diff request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionResolveParams {
    /// Exact notification attempt id, or a message id with one unresolved match.
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionResolveResult {
    pub attempt_id: crate::notification::NotificationAttemptId,
    pub resolution: crate::notification::NotificationResolution,
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
    fn messages_changed_has_an_exact_content_free_wire_shape() {
        let workspace_id =
            serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000001"))
                .unwrap();
        let data = MessagesChangedData {
            workspace_id,
            workspace_seq: 41,
            changed: [
                MessagesChangedArea::Mailboxes,
                MessagesChangedArea::Notifications,
            ]
            .into_iter()
            .collect(),
        };
        let event = Event {
            event: "messages.changed".into(),
            data: serde_json::to_value(&data).unwrap(),
            seq: Some(41),
        };

        let wire = serde_json::to_value(event).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "event": "messages.changed",
                "data": {
                    "workspace_id": "00000000-0000-0000-0000-000000000001",
                    "workspace_seq": 41,
                    "changed": ["mailboxes", "notifications"]
                },
                "seq": 41
            })
        );
        let round_trip: MessagesChangedData = serde_json::from_value(wire["data"].clone()).unwrap();
        assert_eq!(round_trip, data);
    }

    #[test]
    fn deadlock_risk_has_an_exact_content_free_wire_shape() {
        let diagnostic = StatusDiagnostic {
            code: "deadlock_risk".into(),
            message_id: "m-startup".parse().unwrap(),
            notification_attempt: "att-00000000-0000-4000-8000-000000000001".parse().unwrap(),
            recipient: serde_json::from_value(serde_json::json!({
                "kind": "agent",
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "session_instance_id": "00000000-0000-0000-0000-000000000002",
                "pane_id": "%1"
            }))
            .unwrap(),
            recipient_label: "codex-test".into(),
            pane_id: "%1".into(),
        };

        let wire = serde_json::to_value(diagnostic).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "code": "deadlock_risk",
                "message_id": "m-startup",
                "notification_attempt": "att-00000000-0000-4000-8000-000000000001",
                "recipient": {
                    "kind": "agent",
                    "workspace_id": "00000000-0000-0000-0000-000000000001",
                    "session_instance_id": "00000000-0000-0000-0000-000000000002",
                    "pane_id": "%1"
                },
                "recipient_label": "codex-test",
                "pane_id": "%1"
            })
        );
        assert!(wire.get("body").is_none());
        assert!(wire.get("subject").is_none());
    }

    #[test]
    fn inbox_summary_exposes_a_durable_sender_without_content() {
        let sender: crate::identity::RecipientKey =
            "agent:00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/%9"
                .parse()
                .unwrap();
        let entry = InboxSummaryEntry {
            message_id: "m-startup".parse().unwrap(),
            sender: Some(sender),
            sender_label: "gemini-test".into(),
            subject: Some("Startup retrospective".into()),
            ts: 12,
            thread_root: "m-startup".parse().unwrap(),
        };

        let wire = serde_json::to_value(entry).unwrap();
        assert_eq!(wire["sender"]["pane_id"], "%9");
        assert_eq!(wire["sender_label"], "gemini-test");
        assert!(wire.get("body").is_none());
        let old: InboxSummaryEntry = serde_json::from_value(serde_json::json!({
            "message_id": "m-old",
            "sender_label": "gemini-test",
            "ts": 11,
            "thread_root": "m-old"
        }))
        .unwrap();
        assert_eq!(old.sender, None);
    }

    #[test]
    fn message_claim_keeps_its_original_wire_shape() {
        let message = serde_json::to_value(InboxClaimParams {
            message_id: "m-one".parse().unwrap(),
        })
        .unwrap();

        assert_eq!(message, serde_json::json!({"message_id": "m-one"}));
    }

    #[test]
    fn missing_params_defaults_to_null() {
        let req: Request = serde_json::from_str(r#"{"id":"a","method":"status"}"#).unwrap();
        assert!(req.params.is_null());
    }

    #[test]
    fn daemon_build_identity_is_additive_on_hello_and_status() {
        let old_hello: Hello =
            serde_json::from_str(r#"{"cyclops":"0.1.0","proto":1,"boot_id":"old"}"#)
                .expect("an old hello still decodes");
        assert_eq!(old_hello.build, None);

        let hello = Hello {
            cyclops: "0.1.0".into(),
            build: Some("abc1234".into()),
            daemon_process: Some(crate::ProcessInstanceId::new(42, 7).unwrap()),
            daemon_executable: Some("/opt/cyclopsd".into()),
            proto: 1,
            boot_id: "new".into(),
        };
        let hello_wire = serde_json::to_value(hello).unwrap();
        assert_eq!(hello_wire["build"], "abc1234");
        assert_eq!(hello_wire["daemon_process"]["pid"], 42);
        assert_eq!(hello_wire["daemon_executable"], "/opt/cyclopsd");

        let old_status = r#"{"daemon_version":"0.1.0","proto":1,"boot_id":"old",
            "uptime_ms":1,"tmux_version":"3.6a","sessions":[]}"#;
        let mut status: StatusResult =
            serde_json::from_str(old_status).expect("an old status still decodes");
        assert_eq!(status.daemon_build, None);
        assert_eq!(status.daemon_process, None);
        assert_eq!(status.daemon_executable, None);
        status.daemon_build = Some("abc1234".into());
        let status_wire = serde_json::to_value(status).unwrap();
        assert_eq!(status_wire["daemon_build"], "abc1234");
    }

    #[test]
    fn a_legacy_pane_without_composer_fields_fails_closed() {
        let pane: PaneStatus = serde_json::from_value(serde_json::json!({
            "pane_id": "%1",
            "window_id": "@1",
            "window_name": "main",
            "title": "",
            "current_command": "claude",
            "dead": false,
            "in_mode": false,
            "width": 80,
            "height": 24,
            "state": "idle"
        }))
        .expect("legacy pane status still decodes");

        assert!(!pane.write_ready);
        assert_eq!(pane.composer, ComposerState::ComposerAmbiguous);
        assert_eq!(pane.composer_proof, ComposerProof::Unprovable);
        assert_eq!(pane.next_action, None);
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
            recipient: None,
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
        assert_eq!(receipt.notification_state, None);
        assert_eq!(receipt.quota_state, None);
        assert_eq!(receipt.notification_settlement, None);
        assert_eq!(receipt.wake_block, None);

        let receipt = DeliveryReceipt {
            to: "reviewer".into(),
            state: crate::ledger::DeliveryState::Queued,
            notification_state: None,
            quota_state: None,
            notification_settlement: None,
            wake_block: None,
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

    #[test]
    fn mailbox_notification_state_is_optional_and_keeps_its_own_vocabulary() {
        for (state, expected) in [
            (MessageNotificationState::NotStarted, "not_started"),
            (MessageNotificationState::Queued, "queued"),
            (MessageNotificationState::Gating, "gating"),
        ] {
            let receipt = DeliveryReceipt {
                to: "reviewer".into(),
                state: crate::ledger::DeliveryState::Queued,
                notification_state: Some(state),
                quota_state: None,
                notification_settlement: None,
                wake_block: None,
                position: None,
                held_by: None,
                note: None,
                pane: None,
            };
            let wire = serde_json::to_value(&receipt).unwrap();
            let round_trip: DeliveryReceipt = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(round_trip.notification_state, Some(state));
            assert_eq!(wire["notification_state"], expected);
            assert_eq!(wire["state"], "queued");
        }

        let legacy: serde_json::Value = serde_json::to_value(DeliveryReceipt {
            to: "reviewer".into(),
            state: crate::ledger::DeliveryState::Queued,
            notification_state: None,
            quota_state: None,
            notification_settlement: None,
            wake_block: None,
            position: None,
            held_by: None,
            note: None,
            pane: None,
        })
        .unwrap();
        assert!(legacy.get("notification_state").is_none());

        let withdrawn = DeliveryReceipt {
            to: "reviewer".into(),
            state: crate::ledger::DeliveryState::Queued,
            notification_state: Some(MessageNotificationState::NotStarted),
            quota_state: None,
            notification_settlement: Some(MessageNotificationSettlement::WithdrawnByClaim),
            wake_block: None,
            position: None,
            held_by: None,
            note: None,
            pane: None,
        };
        let wire = serde_json::to_value(&withdrawn).unwrap();
        assert_eq!(wire["notification_state"], "not_started");
        assert_eq!(wire["notification_settlement"], "withdrawn_by_claim");
        let round_trip: DeliveryReceipt = serde_json::from_value(wire).unwrap();
        assert_eq!(
            round_trip.notification_settlement,
            Some(MessageNotificationSettlement::WithdrawnByClaim)
        );
    }

    #[test]
    fn mailbox_wake_block_is_additive_and_closed() {
        let receipt = DeliveryReceipt {
            to: "reviewer".into(),
            state: crate::ledger::DeliveryState::Queued,
            notification_state: Some(MessageNotificationState::Queued),
            quota_state: None,
            notification_settlement: None,
            wake_block: Some(MessageWakeBlock::WorkerSupervisorExited),
            position: None,
            held_by: None,
            note: None,
            pane: Some("%3".into()),
        };
        let wire = serde_json::to_value(&receipt).unwrap();
        assert_eq!(wire["wake_block"], "worker_supervisor_exited");
        assert_eq!(
            serde_json::from_value::<DeliveryReceipt>(wire)
                .unwrap()
                .wake_block,
            Some(MessageWakeBlock::WorkerSupervisorExited)
        );

        let legacy_summary: MessageNotificationSummary =
            serde_json::from_value(serde_json::json!({ "state": "gating" })).unwrap();
        assert_eq!(legacy_summary.wake_block, None);
        assert_eq!(
            MessageWakeBlock::ComposerOwnershipUnproven.wire_name(),
            "composer_ownership_unproven"
        );
    }

    #[test]
    fn durable_send_result_preserves_protocol_v1_receipts() {
        let result = MsgSendResult {
            msg_id: "m-compatible".into(),
            seq: 7,
            deliveries: vec![DeliveryReceipt {
                to: "reviewer".into(),
                state: crate::ledger::DeliveryState::Queued,
                notification_state: None,
                quota_state: None,
                notification_settlement: None,
                wake_block: None,
                position: None,
                held_by: None,
                note: None,
                pane: None,
            }],
            inserted: Some(true),
        };
        let wire = serde_json::to_value(&result).unwrap();

        #[derive(Deserialize)]
        struct LegacyMsgSendResult {
            msg_id: String,
            seq: u64,
            deliveries: Vec<DeliveryReceipt>,
        }
        let old: LegacyMsgSendResult = serde_json::from_value(wire).unwrap();
        assert_eq!(old.msg_id, "m-compatible");
        assert_eq!(old.seq, 7);
        assert_eq!(old.deliveries.len(), 1);

        let legacy_wire = serde_json::json!({
            "msg_id": "m-compatible",
            "seq": 7,
            "deliveries": [{"to": "reviewer", "state": "queued"}]
        });
        let current: MsgSendResult = serde_json::from_value(legacy_wire).unwrap();
        assert_eq!(current.msg_id, "m-compatible");
        assert_eq!(current.inserted, None);
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

    /// An alarm summary names why attention is needed, in the closed
    /// vocabulary, and still carries no message content.
    ///
    /// The distinction the operator acts on is verify against submit: one
    /// says the composer never took the text, the other says it took it
    /// and the send did not land.
    #[test]
    fn an_alarm_summary_names_its_cause_and_no_content() {
        use crate::notification::NotificationAttentionCause;

        let summary = AlarmSummary {
            id: "att-00000000-0000-4000-8000-000000000001".into(),
            message_id: "m-1".into(),
            recipient: "reviewer".into(),
            state: crate::ledger::DeliveryState::AttentionRequired,
            cause: NotificationAttentionCause::VerifyFailed,
            ts: 7,
        };
        let value = serde_json::to_value(&summary).expect("summary serializes");
        let mut keys: Vec<_> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["cause", "id", "message_id", "recipient", "state", "ts"]
        );
        assert_eq!(value["cause"], "verify_failed");

        // The two causes an operator most needs to tell apart stay apart.
        let submit = AlarmSummary {
            cause: NotificationAttentionCause::SubmitFailed,
            ..summary.clone()
        };
        assert_eq!(
            serde_json::to_value(&submit).expect("summary serializes")["cause"],
            "submit_failed"
        );

        let decoded: AlarmSummary =
            serde_json::from_value(serde_json::to_value(&summary).unwrap()).expect("round trip");
        assert_eq!(decoded.cause, NotificationAttentionCause::VerifyFailed);
    }

    #[test]
    fn legacy_recipient_summary_defaults_new_work_fields_safely() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient = crate::RecipientKey::agent(workspace, session, "%3".parse().unwrap());
        let current = MessageRecipientSummary {
            recipient,
            label: "reviewer".into(),
            direction: MessageDirection::Inbound,
            needs_action: true,
            can_manage_attention: true,
            can_withdraw_notification: false,
            current_route: None,
            available: true,
            mailbox: crate::MailboxEntryState::Pending,
            fifo_position: Some(1),
            notification: MessageNotificationSummary {
                state: MessageNotificationState::NotStarted,
                wake_block: None,
                quota_state: None,
                settlement: None,
                operator_withdrawn: None,
                attempt_id: None,
                cause: None,
                verify_outcome: None,
                pre_write_cause: None,
                pre_write_pane_width: None,
                pre_write_required_pane_width: None,
                attention_cleared: None,
                resolution: None,
                resolution_intent: None,
                resolution_action_accepted: None,
                resolution_consumption_observed: None,
                updated_at: None,
            },
        };
        let mut legacy = serde_json::to_value(current).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("direction");
        object.remove("needs_action");
        object.remove("can_manage_attention");
        object.remove("can_withdraw_notification");
        object.remove("current_route");

        let decoded: MessageRecipientSummary = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.direction, MessageDirection::Workspace);
        assert!(!decoded.needs_action);
        assert!(!decoded.can_manage_attention);
        assert!(!decoded.can_withdraw_notification);
        assert!(decoded.current_route.is_none());
    }

    #[test]
    fn exact_send_targets_and_snapshot_callers_are_additive() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient = crate::RecipientKey::agent(workspace, session, "%3".parse().unwrap());

        let exact: MsgSendParams = serde_json::from_value(serde_json::json!({
            "to": [],
            "recipient_keys": [recipient],
            "subject": "Exact route"
        }))
        .unwrap();
        assert_eq!(exact.recipient_keys, Some(vec![recipient]));
        assert!(exact.expected_caller.is_none());

        let legacy: MsgSendParams = serde_json::from_value(serde_json::json!({
            "to": ["reviewer"],
            "subject": "Label route"
        }))
        .unwrap();
        assert!(legacy.recipient_keys.is_none());

        let legacy_snapshot: MessagesSnapshotResult = serde_json::from_value(serde_json::json!({
            "workspace_id": workspace,
            "workspace_seq": 0,
            "counts": {
                "visible_messages": 0,
                "returned_messages": 0,
                "inbox_messages": 0,
                "outbound_messages": 0,
                "work_messages": 0,
                "active_messages": 0,
                "settled_messages": 0,
                "pending_entries": 0,
                "claimed_entries": 0,
                "open_attention_entries": 0
            },
            "rows": []
        }))
        .unwrap();
        assert!(legacy_snapshot.caller.is_none());
    }

    #[test]
    fn notification_resolution_is_optional_and_round_trips() {
        let unresolved = MessageNotificationSummary {
            state: MessageNotificationState::AttentionRequired,
            wake_block: None,
            quota_state: None,
            settlement: None,
            operator_withdrawn: None,
            attempt_id: None,
            cause: Some(crate::NotificationAttentionCause::VerifyFailed),
            verify_outcome: Some(crate::NotificationVerifyOutcome {
                kind: crate::NotificationVerifyFailureKind::Mismatch,
                observed_composer: crate::ComposerState::ComposerAmbiguous,
            }),
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            attention_cleared: Some(false),
            resolution: None,
            resolution_intent: None,
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            updated_at: Some(7),
        };
        let unresolved_wire = serde_json::to_value(&unresolved).unwrap();
        assert!(unresolved_wire.get("resolution").is_none());
        let decoded: MessageNotificationSummary = serde_json::from_value(unresolved_wire).unwrap();
        assert_eq!(decoded.resolution, None);
        assert_eq!(decoded.verify_outcome, unresolved.verify_outcome);

        let resolved = MessageNotificationSummary {
            resolution: Some(crate::NotificationResolution::Discard),
            ..unresolved
        };
        let resolved_wire = serde_json::to_value(&resolved).unwrap();
        assert_eq!(resolved_wire["resolution"], "discard");
        let decoded: MessageNotificationSummary = serde_json::from_value(resolved_wire).unwrap();
        assert_eq!(
            decoded.resolution,
            Some(crate::NotificationResolution::Discard)
        );
    }

    #[test]
    fn notification_summary_exposes_width_detail_without_changing_the_closed_cause() {
        #[derive(Debug, Deserialize)]
        struct LegacySummary {
            state: MessageNotificationState,
            pre_write_cause: crate::NotificationPreWriteCause,
        }

        let wire = serde_json::json!({
            "state": "gating",
            "pre_write_cause": "write_readiness_changed",
            "pre_write_pane_width": 59,
            "pre_write_required_pane_width": 60
        });
        let current: MessageNotificationSummary = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(current.pane_width_block(), Some((59, 60)));
        let legacy: LegacySummary = serde_json::from_value(wire).unwrap();
        assert_eq!(legacy.state, MessageNotificationState::Gating);
        assert_eq!(
            legacy.pre_write_cause,
            crate::NotificationPreWriteCause::WriteReadinessChanged
        );
    }

    #[test]
    fn quota_detail_keeps_the_old_notification_enum_decodable() {
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        enum LegacyNotificationState {
            NotStarted,
            Queued,
            Gating,
            Writing,
            Staged,
            Submitted,
            Notified,
            AttentionRequired,
            Superseded,
        }

        #[derive(Deserialize)]
        struct LegacySummary {
            state: LegacyNotificationState,
        }

        for (internal, quota) in [
            (crate::NotificationState::QuotaHeld, MessageQuotaState::Held),
            (
                crate::NotificationState::QuotaResetObserved,
                MessageQuotaState::ResetObserved,
            ),
        ] {
            let summary = MessageNotificationSummary {
                state: internal.into(),
                wake_block: None,
                quota_state: MessageQuotaState::from_notification(internal),
                settlement: None,
                operator_withdrawn: None,
                attempt_id: None,
                cause: None,
                verify_outcome: None,
                pre_write_cause: None,
                pre_write_pane_width: None,
                pre_write_required_pane_width: None,
                attention_cleared: None,
                resolution: None,
                resolution_intent: None,
                resolution_action_accepted: None,
                resolution_consumption_observed: None,
                updated_at: Some(7),
            };
            let wire = serde_json::to_value(&summary).unwrap();
            assert_eq!(wire["state"], "attention_required");
            assert_eq!(wire["quota_state"], serde_json::to_value(quota).unwrap());

            let legacy: LegacySummary = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(legacy.state, LegacyNotificationState::AttentionRequired);
            let current: MessageNotificationSummary = serde_json::from_value(wire).unwrap();
            assert_eq!(current.quota_state, Some(quota));
        }

        let old_wire = serde_json::json!({"state": "attention_required"});
        let current: MessageNotificationSummary = serde_json::from_value(old_wire).unwrap();
        assert_eq!(current.quota_state, None);
        assert_eq!(current.settlement, None);
        assert_eq!(current.verify_outcome, None);
    }

    #[test]
    fn resolution_boundaries_are_distinct_from_completion() {
        let summary = MessageNotificationSummary {
            state: MessageNotificationState::AttentionRequired,
            wake_block: None,
            quota_state: None,
            settlement: None,
            operator_withdrawn: None,
            attempt_id: None,
            cause: Some(crate::NotificationAttentionCause::VerifyFailed),
            verify_outcome: None,
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            attention_cleared: Some(false),
            resolution: None,
            resolution_intent: Some(crate::NotificationResolution::Complete),
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            updated_at: Some(7),
        };
        let wire = serde_json::to_value(&summary).unwrap();
        assert_eq!(wire["resolution_intent"], "complete");
        assert!(wire.get("resolution").is_none());
        let decoded: MessageNotificationSummary = serde_json::from_value(wire).unwrap();
        assert_eq!(
            decoded.resolution_intent,
            Some(crate::NotificationResolution::Complete)
        );
        assert_eq!(decoded.resolution_action_accepted, None);
        assert_eq!(decoded.resolution, None);

        let accepted = MessageNotificationSummary {
            resolution_action_accepted: Some(crate::NotificationResolution::Complete),
            ..summary
        };
        let wire = serde_json::to_value(&accepted).unwrap();
        assert_eq!(wire["resolution_intent"], "complete");
        assert_eq!(wire["resolution_action_accepted"], "complete");
        assert!(wire.get("resolution_consumption_observed").is_none());
        assert!(wire.get("resolution").is_none());
        let decoded: MessageNotificationSummary = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded, accepted);

        let consumed = MessageNotificationSummary {
            resolution_consumption_observed: Some(
                crate::NotificationResolutionConsumptionObservation {
                    evidence: crate::NotificationResolutionConsumptionEvidence::WorkingEdge,
                    observed_at_ms: 9,
                },
            ),
            ..accepted
        };
        let wire = serde_json::to_value(&consumed).unwrap();
        assert_eq!(wire["resolution_intent"], "complete");
        assert_eq!(wire["resolution_action_accepted"], "complete");
        assert_eq!(
            wire["resolution_consumption_observed"]["evidence"],
            "working_edge"
        );
        assert_eq!(wire["resolution_consumption_observed"]["observed_at_ms"], 9);
        assert!(wire.get("resolution").is_none());
        let decoded: MessageNotificationSummary = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded, consumed);
    }

    #[test]
    fn withdrawn_claim_settlement_keeps_old_clients_honest() {
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        enum LegacyNotificationState {
            NotStarted,
            Staged,
        }

        #[derive(Deserialize)]
        struct LegacySummary {
            state: LegacyNotificationState,
        }

        for (internal, expected, legacy) in [
            (
                crate::NotificationState::Withdrawn,
                "not_started",
                LegacyNotificationState::NotStarted,
            ),
            (
                crate::NotificationState::WithdrawnAfterStaging,
                "staged",
                LegacyNotificationState::Staged,
            ),
        ] {
            let summary = MessageNotificationSummary {
                state: internal.into(),
                wake_block: None,
                quota_state: None,
                settlement: MessageNotificationSettlement::from_notification(internal),
                operator_withdrawn: None,
                attempt_id: None,
                cause: None,
                verify_outcome: None,
                pre_write_cause: None,
                pre_write_pane_width: None,
                pre_write_required_pane_width: None,
                attention_cleared: None,
                resolution: None,
                resolution_intent: None,
                resolution_action_accepted: None,
                resolution_consumption_observed: None,
                updated_at: Some(7),
            };
            let wire = serde_json::to_value(&summary).unwrap();

            assert_eq!(wire["state"], expected);
            assert_eq!(wire["settlement"], "withdrawn_by_claim");
            assert_eq!(
                serde_json::from_value::<LegacySummary>(wire.clone())
                    .unwrap()
                    .state,
                legacy
            );
            assert_eq!(
                serde_json::from_value::<MessageNotificationSummary>(wire)
                    .unwrap()
                    .settlement,
                Some(MessageNotificationSettlement::WithdrawnByClaim)
            );
        }

        assert_eq!(
            MessageNotificationState::from(crate::NotificationState::WithdrawnByOperator),
            MessageNotificationState::NotStarted
        );
    }

    #[test]
    fn attention_show_omits_diff_inputs_until_requested() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let base = AttentionShowResult {
            attempt_id: crate::NotificationAttemptId::parse(
                "att-00000000-0000-4000-8000-000000000003",
            )
            .unwrap(),
            message_id: crate::MessageId::new("m-private").unwrap(),
            recipient: crate::RecipientKey::agent(workspace, session, "%3".parse().unwrap()),
            checks: AttentionChecks {
                notification_exact: true,
                trailer_anchored: true,
                process_matches: true,
                manifest_matches: true,
                terminal_action_safe: true,
            },
            verify_outcome: None,
            expected: None,
            observed: None,
        };

        let value = serde_json::to_value(&base).unwrap();
        assert!(value.get("expected").is_none());
        assert!(value.get("observed").is_none());
        assert!(base.checks.all_pass());

        let legacy: AttentionShowResult = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(legacy.verify_outcome, None);

        let with_diff = AttentionShowResult {
            verify_outcome: Some(crate::NotificationVerifyOutcome {
                kind: crate::NotificationVerifyFailureKind::Timeout,
                observed_composer: crate::ComposerState::ComposerAmbiguous,
            }),
            expected: Some("expected bytes".into()),
            observed: Some("observed bytes".into()),
            ..base
        };
        let value = serde_json::to_value(&with_diff).unwrap();
        assert_eq!(value["expected"], "expected bytes");
        assert_eq!(value["observed"], "observed bytes");
        assert_eq!(value["verify_outcome"]["kind"], "timeout");
        assert!(value.get("diff").is_none());
    }
}
