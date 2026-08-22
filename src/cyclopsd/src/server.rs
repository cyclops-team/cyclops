//! NDJSON socket server: hello line first, then a request loop per
//! connection, switching to event push after events.subscribe.
//!
//! Slow consumers never stall the daemon: every subscriber reads from its
//! own broadcast receiver, a lagged receiver is dropped with a warning,
//! and writes carry a timeout so a wedged client costs one connection.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
#[cfg(test)]
use cyclops_proto::TmuxPaneId;
use cyclops_proto::{
    AdminNotifyParams, AgentWaitParams, AlarmClearParams, AlarmClearResult, AlarmPreviewParams,
    AlarmPreviewResult, AlarmSummary, AttentionResolveParams, AttentionShowParams,
    ClaimDisposition, DeliveryState, Event, Hello, InboxClaimParams, InboxClaimResult,
    InboxListParams, InboxListResult, InboxSummaryEntry, MessagesSnapshotParams, MsgSendParams,
    NotificationAttemptId, NotificationAttentionCause, NotificationResolution, PaneReadParams,
    PaneReadResult, PaneReadSource, PingResult, ProcessInstanceId, QuiesceParams, RecipientKey,
    ReplyParams, Request, RequeueParams, RequeueResult, Response, SessionStatus, StateReportParams,
    StatusParams, StatusResult, SubscribeParams, WireError, PROTOCOL_VERSION,
};
use cyclops_state::{BoundSocketCleanup, StateRoot};
use cyclops_tmux::SessionWatcher;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};

use crate::{ack, delivery, fusion, identity, unix_ms, Inner};

/// Peer credentials captured once per connection, before the stream is
/// split. None means the kernel could not report them; identity-gated
/// methods fail closed on it.
type Peer = Option<identity::PeerConn>;

/// A write that does not finish inside this window means the client is
/// wedged; the connection is dropped rather than buffered without bound.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct BoundSocket {
    listener: Option<UnixListener>,
    cleanup: Option<BoundSocketCleanup>,
}

impl BoundSocket {
    pub(crate) fn into_parts(mut self) -> (UnixListener, BoundSocketCleanup) {
        (
            self.listener.take().expect("bound socket has one listener"),
            self.cleanup
                .take()
                .expect("bound socket has one cleanup guard"),
        )
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        drop(self.listener.take());
        if let Some(cleanup) = self.cleanup.take() {
            let _ = cleanup.remove();
        }
    }
}

/// Scrollback lines for pane.read source=recent when the caller gave none.
const DEFAULT_RECENT_LINES: u32 = 200;

/// Settled history is context, not an unbounded transcript.
const MAX_RECENT_SETTLED_MESSAGES: u32 = 100;

/// Protocol v1 methods that exist but land in a later milestone. One list,
/// so a new milestone replaces entries here instead of hunting through
/// dispatch. Empty as of M2 (msg.history/msg.thread landed).
const UNIMPLEMENTED: &[(&str, &str)] = &[];

/// Bind the daemon socket under `home`, creating the directory 0700.
///
/// Stale socket handling: if something answers at the path another daemon
/// is running and boot fails loudly; a refused connect means a leftover
/// file from a dead daemon, which is removed and rebound.
pub(crate) async fn bind_socket(state_root: &StateRoot) -> anyhow::Result<BoundSocket> {
    let home = state_root.path();
    if !state_root.path_matches_held_root()? {
        anyhow::bail!("state root path changed before socket bind");
    }
    let sock = home.join(cyclops_proto::SOCK_NAME);
    if sock.exists() {
        match UnixStream::connect(&sock).await {
            Ok(_) => anyhow::bail!("another cyclopsd is already running at {}", sock.display()),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                if !state_root.path_matches_held_root()? {
                    anyhow::bail!("state root path changed during stale socket check");
                }
                info!(socket = %sock.display(), "removing stale socket");
                state_root
                    .bound_socket_cleanup(std::ffi::OsStr::new(cyclops_proto::SOCK_NAME))?
                    .remove()
                    .with_context(|| format!("remove stale {}", sock.display()))?;
            }
            Err(e) => {
                // Not refused and not cleanly connectable. A wedged-but-live
                // daemon can surface here, so reclaiming the path could pull
                // the socket out from under it. Fail loudly instead; the
                // operator removes the file if the daemon is truly gone.
                anyhow::bail!(
                    "socket {} is in an unexpected state ({e}); if no cyclopsd is running, remove the file and retry",
                    sock.display()
                );
            }
        }
    }
    if !state_root.path_matches_held_root()? {
        anyhow::bail!("state root path changed before socket bind");
    }
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;
    let cleanup = state_root
        .bound_socket_cleanup(std::ffi::OsStr::new(cyclops_proto::SOCK_NAME))
        .with_context(|| format!("validate bound socket {}", sock.display()))?;
    let bound = BoundSocket {
        listener: Some(listener),
        cleanup: Some(cleanup),
    };
    if !state_root.path_matches_held_root()? {
        drop(bound);
        anyhow::bail!("state root path changed during socket bind");
    }
    Ok(bound)
}

/// Accept loop. Errors are logged and retried after a short pause so a
/// transient fd exhaustion does not spin the CPU.
pub(crate) async fn accept_loop(
    inner: Arc<Inner>,
    listener: UnixListener,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(handle_conn(Arc::clone(&inner), stream));
                }
                Err(e) => {
                    warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

/// What one processed request line means for the connection loop.
enum LineOutcome {
    Continue,
    Drop,
    Subscribed(Vec<String>, broadcast::Receiver<Event>),
}

/// What the pump produced: an event for a subscribed connection, or a line
/// from the client.
enum Pumped {
    Ev(Result<Event, broadcast::error::RecvError>),
    Line(std::io::Result<Option<String>>),
}

pub(crate) async fn handle_conn(inner: Arc<Inner>, stream: UnixStream) {
    // Peer credentials are read once, before the split consumes the
    // stream. Identity-gated methods (msg.send) fail closed without them.
    // Read once, and kept with the descriptor it came from so every
    // identity-gated request can ask again: a connection outlives a
    // request, and the process behind it need not.
    let fd = {
        use std::os::fd::AsRawFd;
        stream.as_raw_fd()
    };
    let peer: Peer = match identity::peer_identity(&stream) {
        Ok(id) => {
            debug!(uid = id.uid, pid = id.pid, "client connected");
            Some(identity::PeerConn { id, fd })
        }
        Err(e) => {
            debug!(error = %e, "client connected; peer credentials unavailable");
            None
        }
    };
    let (read_half, mut w) = stream.into_split();
    let hello = Hello {
        cyclops: env!("CARGO_PKG_VERSION").to_string(),
        proto: PROTOCOL_VERSION,
        boot_id: inner.boot_id.clone(),
    };
    let hello_line = serde_json::to_string(&hello).expect("hello serializes");
    if !write_line(&mut w, &hello_line).await {
        return;
    }
    let mut lines = BufReader::new(read_half).lines();
    let mut sub: Option<(broadcast::Receiver<Event>, Vec<String>)> = None;

    loop {
        let pumped = match &mut sub {
            Some((rx, _)) => tokio::select! {
                ev = rx.recv() => Pumped::Ev(ev),
                line = lines.next_line() => Pumped::Line(line),
            },
            None => Pumped::Line(lines.next_line().await),
        };
        match pumped {
            Pumped::Ev(Ok(ev)) => {
                let kinds = &sub.as_ref().expect("subscribed").1;
                if kind_matches(kinds, &ev.event) {
                    let Ok(line) = serde_json::to_string(&ev) else {
                        continue;
                    };
                    if !write_line(&mut w, &line).await {
                        return;
                    }
                }
            }
            Pumped::Ev(Err(broadcast::error::RecvError::Lagged(missed))) => {
                // Dropping beats blocking the daemon or silently thinning
                // the stream: the client reconnects and resyncs.
                warn!(missed, "subscriber too slow; dropping connection");
                return;
            }
            Pumped::Ev(Err(broadcast::error::RecvError::Closed)) => return,
            Pumped::Line(Ok(Some(line))) => match handle_line(&inner, &line, peer, &mut w).await {
                LineOutcome::Continue => {}
                LineOutcome::Drop => return,
                LineOutcome::Subscribed(kinds, rx) => sub = Some((rx, kinds)),
            },
            // EOF or read error: the client is gone.
            Pumped::Line(_) => return,
        }
    }
}

/// Process one request line: parse, dispatch, write the response.
async fn handle_line(
    inner: &Arc<Inner>,
    line: &str,
    peer: Peer,
    w: &mut OwnedWriteHalf,
) -> LineOutcome {
    if line.trim().is_empty() {
        return LineOutcome::Continue;
    }
    let req = match parse_request(line) {
        Ok(r) => r,
        Err(resp) => {
            // Malformed JSON answers with bad_request and a null id; the
            // connection stays open.
            let text = serde_json::to_string(&resp).expect("response serializes");
            return if write_line(w, &text).await {
                LineOutcome::Continue
            } else {
                LineOutcome::Drop
            };
        }
    };
    let (resp, subscribe) = dispatch(inner, req, peer).await;
    let text = serde_json::to_string(&resp).expect("response serializes");
    if let Some(params) = subscribe {
        // Subscribe before writing the ack so no event can fall between.
        let rx = inner.events.subscribe();
        if !write_line(w, &text).await {
            return LineOutcome::Drop;
        }
        return LineOutcome::Subscribed(params.kinds, rx);
    }
    if write_line(w, &text).await {
        LineOutcome::Continue
    } else {
        LineOutcome::Drop
    }
}

// The Err arm is a cold path (malformed client line) and the Response is
// written out immediately; boxing it would only add ceremony.
#[allow(clippy::result_large_err)]
fn parse_request(line: &str) -> Result<Request, Response> {
    serde_json::from_str::<Request>(line).map_err(|e| {
        Response::err(
            Value::Null,
            "bad_request",
            format!("not a valid request line: {e}"),
        )
    })
}

/// Prefix filter for events.subscribe. Empty means everything.
pub(crate) fn kind_matches(kinds: &[String], event: &str) -> bool {
    kinds.is_empty() || kinds.iter().any(|k| event.starts_with(k.as_str()))
}

/// Decode a request's params, or the bad_request response that names what
/// was wrong. The noun is caller-supplied ("msg.history params", "state
/// report") so every denial keeps its exact sentence; clients and tests
/// match on them.
// The Err is a full Response, built once on the cold path and immediately
// written to the socket; boxing it would cost every call site an unwrap.
#[allow(clippy::result_large_err)]
fn decode_params<T: serde::de::DeserializeOwned>(
    id: &Value,
    params: Value,
    what: &str,
) -> Result<T, Response> {
    serde_json::from_value(params)
        .map_err(|e| Response::err(id.clone(), "bad_request", format!("bad {what}: {e}")))
}

/// Method dispatch. Returns the response plus subscribe params when the
/// connection should switch to push mode.
pub(crate) async fn dispatch(
    inner: &Arc<Inner>,
    req: Request,
    peer: Peer,
) -> (Response, Option<SubscribeParams>) {
    let id = req.id.clone();
    match req.method.as_str() {
        "ping" => {
            let result = PingResult {
                pong: true,
                ts: unix_ms(),
            };
            (
                Response::ok(id, serde_json::to_value(result).expect("ping serializes")),
                None,
            )
        }
        "status" => {
            // Absent params mean the shipped answer: every member defaults,
            // and callers that predate the struct send none. A params
            // object that is present with a member of the WRONG TYPE is an
            // error rather than a default, because `status` is how a client
            // reconciles the deliveries still waiting on a human, and
            // answering that with an empty list is the one direction this
            // path should not fail in.
            //
            // An unknown FIELD NAME is accepted, and that is deliberate:
            // ADR-001 S2 makes every decode tolerate unknown fields, so a
            // field a newer client adds still works against this daemon,
            // and a typo is byte-identical to one. What the typo costs is
            // bounded and stated where the count is composed
            // (cyclops_proto::attention): an answer that did not carry the
            // backlog counts blocked panes alone, which UNDERSTATES, and
            // the client knows what it asked for. Any client that shows the
            // eye must set open_deliveries.
            let params: StatusParams = match req.params {
                Value::Null => StatusParams::default(),
                given => match decode_params(&id, given, "status params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                },
            };
            let result = status_result(inner, params.open_deliveries);
            (
                Response::ok(id, serde_json::to_value(result).expect("status serializes")),
                None,
            )
        }
        "pane.read" => (pane_read(inner, id, req.params).await, None),
        "daemon.quiesce" => {
            // The pre-restart hold. Absent params take the shipped bounds,
            // like `status`; the daemon owns the ceiling either way.
            let params: QuiesceParams = match req.params {
                Value::Null => QuiesceParams::default(),
                given => match decode_params(&id, given, "daemon.quiesce params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                },
            };
            let result = delivery::quiesce(inner, params.timeout_ms).await;
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("quiesce serializes"),
                ),
                None,
            )
        }
        "msg.send" => (msg_send(inner, id, req.params, peer).await, None),
        "msg.reply" => (msg_reply(inner, id, req.params, peer), None),
        "inbox.list" => (inbox_list(inner, id, req.params, peer), None),
        "inbox.claim" => (inbox_claim(inner, id, req.params, peer), None),
        "messages.snapshot" => (messages_snapshot(inner, id, req.params, peer), None),
        "msg.requeue" => (msg_requeue(inner, id, req.params, peer), None),
        "alarm.preview" => (alarm_preview(inner, id, req.params, peer), None),
        "alarm.clear" => (alarm_clear(inner, id, req.params, peer), None),
        "attention.show" => (attention_show(inner, id, req.params, peer).await, None),
        "attention.complete" => (
            attention_resolve(
                inner,
                id,
                req.params,
                peer,
                NotificationResolution::Complete,
            )
            .await,
            None,
        ),
        "attention.discard" => (
            attention_resolve(inner, id, req.params, peer, NotificationResolution::Discard).await,
            None,
        ),
        "msg.history" => {
            // cursor2 travels outside the HistoryParams struct (wire-additive;
            // the struct is shared with clients that predate the field).
            let cursor2 = match req.params.get("cursor2") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    return (
                        Response::err(id, "bad_request", "cursor2 must be a string"),
                        None,
                    )
                }
            };
            let params: cyclops_proto::HistoryParams =
                match decode_params(&id, req.params, "msg.history params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                };
            (
                from_result(
                    id,
                    crate::history::msg_history(inner, params, cursor2, peer),
                ),
                None,
            )
        }
        "msg.thread" => {
            let params: cyclops_proto::ThreadParams =
                match decode_params(&id, req.params, "msg.thread params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                };
            (
                from_result(id, crate::history::msg_thread(inner, &params.id, peer)),
                None,
            )
        }
        "agent.state.report" => {
            let params: StateReportParams = match decode_params(&id, req.params, "state report") {
                Ok(p) => p,
                Err(r) => return (r, None),
            };
            // Hook reports feed liveness and tier-1 ACK evidence, so a
            // forged one lets the record lie. The socket path is pinned to
            // the reporting pane exactly like msg.send pins senders; only
            // the in-process Daemon::report_state path is pre-trusted.
            let origin = match verify_report_origin(inner, peer, params.agent.as_deref()) {
                Ok(o) => o,
                Err(e) => {
                    return (
                        Response {
                            id,
                            result: None,
                            error: Some(e),
                        },
                        None,
                    )
                }
            };
            (
                from_result(id, ack::handle_report(inner, params, origin).await),
                None,
            )
        }
        "admin.notify" => {
            let params: AdminNotifyParams =
                match decode_params(&id, req.params, "admin.notify params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                };
            let seq = delivery::admin_notify(
                inner,
                params.level,
                &params.subject,
                &params.body,
                None,
                None,
                // An operator's own ping. It names no delivery and no
                // pane, so nothing downstream may decide it is stale.
                delivery::About::default(),
            );
            (
                Response::ok(id, json!({"notified": true, "seq": seq})),
                None,
            )
        }
        // No params: the daemon reads the selection itself. A method that
        // took a theme name would let a client and the config disagree
        // about what is on, and the config is what every other surface
        // reads.
        "theme.reload" => {
            let name = crate::reload_theme(inner).await;
            (Response::ok(id, json!({"theme": name})), None)
        }
        "agent.wait" => {
            let params: AgentWaitParams = match decode_params(&id, req.params, "agent.wait params")
            {
                Ok(p) => p,
                Err(r) => return (r, None),
            };
            (
                from_result(id, delivery::agent_wait(inner, params).await),
                None,
            )
        }
        "pane.label" => {
            let target = req.params["target"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if target.is_empty() {
                return (
                    Response::err(id, "bad_request", "pane.label needs a target"),
                    None,
                );
            }
            let label = req.params["label"].as_str().map(str::to_string);
            let manifest = req.params["manifest"].as_str().map(str::to_string);
            (
                from_result(id, crate::label_pane(inner, &target, label, manifest).await),
                None,
            )
        }
        // Start watching a tmux session the daemon was not booted with
        // (crate::watch_session's doc comment has the full story: this
        // does not touch config.toml, so a restart forgets it).
        "session.watch" => {
            let session = req.params["session"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if session.trim().is_empty() {
                return (
                    Response::err(id, "bad_request", "session.watch needs a session"),
                    None,
                );
            }
            match crate::watch_session(inner, &session).await {
                Ok((_, added)) => (
                    Response::ok(
                        id,
                        json!({"session": session, "watching": true, "added": added}),
                    ),
                    None,
                ),
                Err(e) => (
                    Response {
                        id,
                        result: None,
                        error: Some(e),
                    },
                    None,
                ),
            }
        }
        "hooks.verify" => {
            let params: cyclops_proto::HooksVerifyParams =
                match decode_params(&id, req.params, "hooks.verify params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                };
            (
                from_result(id, crate::selftest::verify(inner, params).await),
                None,
            )
        }
        "hooks.selftest" => {
            let params: cyclops_proto::HooksSelftestParams =
                match decode_params(&id, req.params, "hooks.selftest params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                };
            (
                from_result(id, crate::selftest::selftest(inner, params).await),
                None,
            )
        }
        "events.subscribe" => {
            let params: SubscribeParams = if req.params.is_null() {
                SubscribeParams {
                    kinds: Vec::new(),
                    cursor: None,
                }
            } else {
                match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return (
                            Response::err(id, "bad_request", format!("bad subscribe params: {e}")),
                            None,
                        )
                    }
                }
            };
            if params.cursor.is_some() {
                debug!("subscribe cursor ignored: ledger replay lands with the stream client (M3)");
            }
            (Response::ok(id, json!({"subscribed": true})), Some(params))
        }
        "workspace_ui.get" => {
            let result = crate::workspace_ui::workspace_ui_get(&inner.workspace_ui);
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("workspace_ui.get serializes"),
                ),
                None,
            )
        }
        "workspace_ui.set" => {
            let params: cyclops_proto::WorkspaceUiSetParams =
                match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return (
                            Response::err(
                                id,
                                "bad_request",
                                format!("bad workspace_ui.set params: {e}"),
                            ),
                            None,
                        )
                    }
                };
            crate::workspace_ui::workspace_ui_set(&inner.workspace_ui, &params);
            (Response::ok(id, json!({"saved": true})), None)
        }
        method => {
            if let Some((_, milestone)) = UNIMPLEMENTED.iter().find(|(m, _)| *m == method) {
                (
                    Response::err(id, "unimplemented", format!("coming in {milestone}")),
                    None,
                )
            } else {
                (
                    Response::err(id, "unknown_method", format!("unknown method {method:?}")),
                    None,
                )
            }
        }
    }
}

/// Wrap a handler's Result into a Response.
fn from_result(id: Value, result: Result<Value, cyclops_proto::WireError>) -> Response {
    match result {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response {
            id,
            result: None,
            error: Some(e),
        },
    }
}

/// The identity boundary both authenticated verbs stand on: the peer must
/// present credentials and be the daemon's own user. Fail-closed, and in
/// one place, because msg.send (sender attribution) and agent.state.report
/// (hook-ACK evidence) must never disagree about who is allowed in — a
/// tightening applied to one and not the other would leave the record
/// trusting a peer the other verb turns away.
fn daemon_peer(peer: Peer) -> Result<(u32, i32), WireError> {
    let deny = |message: String| WireError {
        code: "denied".to_string(),
        message,
        data: None,
    };
    let Some(conn) = peer else {
        return Err(deny("peer credentials unavailable".to_string()));
    };
    // Asked again, now. The credentials this connection was accepted with
    // belong to the process that opened it, and that process can exit, be
    // replaced at the same number, or re-execute into another program
    // while the socket stays open.
    let Some(id) = conn.current() else {
        return Err(deny(
            "the process that opened this connection is no longer the one on it".to_string(),
        ));
    };
    let daemon_uid = unsafe { libc::getuid() };
    if id.uid != daemon_uid {
        return Err(deny(format!("uid {} is not the daemon's user", id.uid)));
    }
    Ok((id.uid, id.pid))
}

async fn msg_send(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: MsgSendParams = match decode_params(&id, params, "msg.send params") {
        Ok(p) => p,
        Err(r) => return r,
    };
    if params.wait.is_some() {
        return Response::err(
            id,
            "notification_unavailable",
            "send wait is not supported for mailbox notifications",
        );
    }
    let (service, sender) = match mailbox_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    if params.reply_to.is_some() && (params.fyi || params.supersedes.is_some()) {
        return Response::err(
            id,
            "bad_request",
            "a reply cannot be an announcement or supersede another message",
        );
    }
    match crate::messaging::send(inner, &service, sender, params) {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("message acceptance serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn msg_reply(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: ReplyParams = match decode_params(&id, params, "msg.reply params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (service, sender) = match mailbox_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match crate::messaging::reply(
        inner,
        &service,
        sender,
        params.message_id,
        params.body,
        params.client_key,
    ) {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("reply acceptance serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn inbox_list(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: InboxListParams = match decode_params(&id, params, "inbox.list params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (service, caller) = match mailbox_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match service.list(caller.key, params.sender, params.limit) {
        Ok(entries) => {
            let entries = entries
                .into_iter()
                .map(|item| InboxSummaryEntry {
                    message_id: item.entry.message_id,
                    sender: Some(item.sender),
                    sender_label: item.sender_label,
                    subject: item.subject,
                    ts: item.entry.created_at,
                    thread_root: item.thread_root,
                })
                .collect();
            Response::ok(
                id,
                serde_json::to_value(InboxListResult { entries }).expect("inbox list serializes"),
            )
        }
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn inbox_claim(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: InboxClaimParams = match decode_params(&id, params, "inbox.claim params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (service, caller) = match mailbox_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    let result = match crate::messaging::claim(inner, &service, caller.key, params.message_id) {
        Ok(crate::mailbox::ClaimOutcome::Claimed { message, .. }) => InboxClaimResult {
            disposition: ClaimDisposition::Claimed,
            message,
        },
        Ok(crate::mailbox::ClaimOutcome::AlreadyClaimed { message, .. }) => InboxClaimResult {
            disposition: ClaimDisposition::AlreadyClaimed,
            message,
        },
        Err(error) => return wire_error_response(id, mailbox_service_error(error)),
    };
    Response::ok(
        id,
        serde_json::to_value(result).expect("inbox claim serializes"),
    )
}

fn messages_snapshot(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: MessagesSnapshotParams =
        match decode_params(&id, params, "messages.snapshot params") {
            Ok(params) => params,
            Err(response) => return response,
        };
    if params.recent_settled > MAX_RECENT_SETTLED_MESSAGES {
        return Response::err(
            id,
            "bad_request",
            format!("recent_settled cannot exceed {MAX_RECENT_SETTLED_MESSAGES}"),
        );
    }
    let (service, caller) = match mailbox_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match service.messages_snapshot(caller.key, params.recent_settled) {
        Ok(snapshot) => Response::ok(
            id,
            serde_json::to_value(snapshot).expect("messages snapshot serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn msg_requeue(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: RequeueParams = match decode_params(&id, params, "msg.requeue params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    if let Err(error) = require_mailbox_admin(inner, peer) {
        return wire_error_response(id, error);
    }
    let service = match mailbox_service(inner) {
        Ok(service) => service,
        Err(error) => return wire_error_response(id, error),
    };
    let requeued = match crate::messaging::requeue(inner, &service, params.message_id.clone()) {
        Ok(requeued) => requeued,
        Err(error) => return wire_error_response(id, mailbox_service_error(error)),
    };
    let result = RequeueResult {
        message_id: params.message_id,
        requeued,
    };
    Response::ok(
        id,
        serde_json::to_value(result).expect("requeue result serializes"),
    )
}

fn alarm_preview(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: AlarmPreviewParams = match decode_params(&id, params, "alarm.preview params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    if let Err(error) = require_mailbox_admin(inner, peer) {
        return wire_error_response(id, error);
    }
    let service = match mailbox_service(inner) {
        Ok(service) => service,
        Err(error) => return wire_error_response(id, error),
    };
    let records = match service.alarms_older_than(params.older_than_ms) {
        Ok(records) => records,
        Err(error) => return wire_error_response(id, mailbox_service_error(error)),
    };
    // Identity, state and age only. No subject and no body: an operator
    // deciding what to clear does not need the message contents.
    let entries = records
        .into_iter()
        .map(|record| AlarmSummary {
            id: record.attempt_id.to_string(),
            message_id: record.message_id.to_string(),
            recipient: record.recipient.to_string(),
            state: DeliveryState::AttentionRequired,
            // An attention record always carries a cause. If one ever
            // reached here without it, the honest answer is that the
            // outcome is unknown, not a specific failure it did not have.
            cause: record
                .cause
                .unwrap_or(NotificationAttentionCause::TransportOutcomeUnknown),
            ts: record.updated_at,
        })
        .collect();
    Response::ok(
        id,
        serde_json::to_value(AlarmPreviewResult { entries })
            .expect("alarm preview result serializes"),
    )
}

fn alarm_clear(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: AlarmClearParams = match decode_params(&id, params, "alarm.clear params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    if let Err(error) = require_mailbox_admin(inner, peer) {
        return wire_error_response(id, error);
    }
    if params.ids.is_empty() {
        return Response::err(id, "bad_request", "alarm.clear requires explicit alarm ids");
    }
    let mut attempts = Vec::with_capacity(params.ids.len());
    for raw in &params.ids {
        match NotificationAttemptId::parse(raw) {
            Ok(attempt) => attempts.push(attempt),
            Err(_) => return Response::err(id, "bad_request", format!("invalid alarm id '{raw}'")),
        }
    }
    let service = match mailbox_service(inner) {
        Ok(service) => service,
        Err(error) => return wire_error_response(id, error),
    };
    let cleared = match service.clear_alarms(&attempts) {
        Ok(cleared) => cleared,
        Err(error) => return wire_error_response(id, mailbox_service_error(error)),
    };
    let result = AlarmClearResult {
        cleared_ids: cleared.iter().map(ToString::to_string).collect(),
    };
    Response::ok(
        id,
        serde_json::to_value(result).expect("alarm clear result serializes"),
    )
}

async fn attention_show(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: AttentionShowParams = match decode_params(&id, params, "attention.show params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (service, caller) = match mailbox_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    if !caller.key.is_admin() {
        return wire_error_response(id, mailbox_admin_required());
    }
    let target = match attention_target(&service, &params.id) {
        Ok(target) => target,
        Err(error) => return wire_error_response(id, error),
    };
    // Diff mode returns the exact payload selected at the write boundary.
    // Direct compatibility attempts can therefore include message content.
    // The endpoint is admin-only, and neither diff input is logged or stored.
    let result = crate::attention_resolution::show(inner, &service, &target, params.diff).await;
    Response::ok(
        id,
        serde_json::to_value(result).expect("attention show result serializes"),
    )
}

async fn attention_resolve(
    inner: &Arc<Inner>,
    id: Value,
    params: Value,
    peer: Peer,
    resolution: NotificationResolution,
) -> Response {
    let params: AttentionResolveParams =
        match decode_params(&id, params, "attention resolution params") {
            Ok(params) => params,
            Err(response) => return response,
        };
    let (service, caller) = match mailbox_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    if !caller.key.is_admin() {
        return wire_error_response(id, mailbox_admin_required());
    }
    let target = match attention_target(&service, &params.id) {
        Ok(target) => target,
        Err(error) => return wire_error_response(id, error),
    };
    match crate::attention_resolution::resolve(inner, &service, &target, resolution).await {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("attention resolution result serializes"),
        ),
        Err(error) => wire_error_response(id, attention_action_error(error)),
    }
}

fn attention_target(
    service: &Arc<crate::mailbox::MailboxService>,
    raw: &str,
) -> Result<crate::mailbox::AttentionTarget, WireError> {
    match service.attention_target(raw) {
        Ok(target) => Ok(target),
        Err(crate::mailbox::MailboxServiceError::Store(
            crate::mailbox::MessageStoreError::Mailbox(error),
        )) => {
            if let crate::mailbox::MailboxError::AmbiguousAttentionTarget { candidates, .. } =
                error.as_ref()
            {
                return Err(WireError {
                    code: "ambiguous_attention".to_string(),
                    message: error.to_string(),
                    data: Some(json!({
                        "candidates": candidates.iter().map(ToString::to_string).collect::<Vec<_>>()
                    })),
                });
            }
            Err(mailbox_service_error(
                crate::mailbox::MailboxServiceError::Store(
                    crate::mailbox::MessageStoreError::Mailbox(error),
                ),
            ))
        }
        Err(error) => Err(mailbox_service_error(error)),
    }
}

fn attention_action_error(error: crate::attention_resolution::AttentionActionError) -> WireError {
    use crate::attention_resolution::AttentionActionError;

    match error {
        AttentionActionError::Store(error) => mailbox_service_error(error),
        AttentionActionError::Evidence(result) => WireError {
            code: "attention_evidence_failed".to_string(),
            message: "the staged notification did not pass every safety check".to_string(),
            data: Some(serde_json::to_value(result).expect("attention evidence serializes")),
        },
        AttentionActionError::DiscardUnsupported => WireError {
            code: "discard_unsupported".to_string(),
            message: error.to_string(),
            data: None,
        },
        AttentionActionError::Uncertain => WireError {
            code: "attention_action_uncertain".to_string(),
            message: error.to_string(),
            data: None,
        },
    }
}

fn require_mailbox_admin(inner: &Arc<Inner>, peer: Peer) -> Result<(), WireError> {
    let (_, identity) = mailbox_caller(inner, peer)?;
    if identity.key.is_admin() {
        Ok(())
    } else {
        Err(mailbox_admin_required())
    }
}

fn mailbox_admin_required() -> WireError {
    WireError {
        code: "denied".to_string(),
        message: "this operation requires the workspace administrator".to_string(),
        data: None,
    }
}

fn mailbox_service(inner: &Arc<Inner>) -> Result<Arc<crate::mailbox::MailboxService>, WireError> {
    inner.mailbox.clone().ok_or_else(|| WireError {
        code: "mailbox_unavailable".to_string(),
        message: "durable workspace identity is not connected".to_string(),
        data: None,
    })
}

pub(crate) fn mailbox_caller(
    inner: &Arc<Inner>,
    peer: Peer,
) -> Result<
    (
        Arc<crate::mailbox::MailboxService>,
        crate::mailbox::MailboxIdentity,
    ),
    WireError,
> {
    let (uid, pid) = daemon_peer(peer)?;
    let _publication = inner
        .mailbox_publication
        .lock()
        .expect("mailbox publication lock");
    let service = mailbox_service(inner)?;
    let panes = report_panes(inner);
    let observed: Vec<_> = panes
        .iter()
        .enumerate()
        .map(|(idx, pane)| (idx.to_string(), None, pane.root))
        .collect();
    let origin = identity::resolve_peer_origin_observed(uid, pid, &observed, |process| {
        crate::fusion::is_vendor_now(inner, process)
    });
    let caller = match origin {
        identity::PeerOrigin::Admin => service.admin(),
        identity::PeerOrigin::Pane {
            pane_id, pane_root, ..
        } => {
            let route =
                report_pane_at(&panes, &pane_id, pane_root).ok_or_else(mailbox_origin_denied)?;
            let root = ProcessInstanceId::new(route.root.pid, route.root.birth)
                .map_err(|_| mailbox_origin_denied())?;
            if inner
                .registry
                .lock()
                .expect("registry lock")
                .for_route(route.recipient_key, root)
                .is_none()
            {
                return Err(mailbox_origin_denied());
            }
            service
                .identity_for_recipient(route.recipient_key)
                .map_err(mailbox_service_error)?
                .ok_or_else(mailbox_origin_denied)?
        }
        identity::PeerOrigin::Unprovable => return Err(mailbox_origin_denied()),
    };
    Ok((service, caller))
}

fn mailbox_origin_denied() -> WireError {
    WireError {
        code: "denied".to_string(),
        message: "the socket peer does not resolve to one exact mailbox identity".to_string(),
        data: None,
    }
}

#[cfg(test)]
fn mailbox_identity_from_origin(
    inner: &Inner,
    service: &crate::mailbox::MailboxService,
    origin: identity::PeerOrigin,
) -> Result<crate::mailbox::MailboxIdentity, WireError> {
    match origin {
        identity::PeerOrigin::Admin => Ok(service.admin()),
        identity::PeerOrigin::Pane {
            pane_id, pane_root, ..
        } => {
            let pane = pane_id.parse::<TmuxPaneId>().map_err(|error| WireError {
                code: "denied".to_string(),
                message: error.to_string(),
                data: None,
            })?;
            let expected = crate::mailbox_recipient_for_origin(inner, pane, pane_root)
                .ok_or_else(mailbox_origin_denied)?;
            let identity = service
                .identity_for_recipient(expected)
                .map_err(mailbox_service_error)?
                .ok_or_else(mailbox_origin_denied)?;
            if identity.key != expected {
                return Err(mailbox_origin_denied());
            }
            Ok(identity)
        }
        identity::PeerOrigin::Unprovable => Err(WireError {
            code: "denied".to_string(),
            message: "the sending process could not be placed".to_string(),
            data: None,
        }),
    }
}

pub(crate) fn mailbox_service_error(error: crate::mailbox::MailboxServiceError) -> WireError {
    use crate::mailbox::{MailboxDirectoryError, MailboxError, MailboxServiceError};

    let (code, message) = match error {
        MailboxServiceError::Directory(MailboxDirectoryError::UnknownRecipient(target)) => {
            ("no_such_target", format!("no mailbox recipient {target:?}"))
        }
        MailboxServiceError::Directory(error) => ("bad_request", error.to_string()),
        MailboxServiceError::Store(crate::mailbox::MessageStoreError::Mailbox(error)) => {
            match error.as_ref() {
                MailboxError::MessageNotFound(_) | MailboxError::EntryNotFound { .. } => {
                    ("no_such_message", error.to_string())
                }
                MailboxError::MessageNotPending(_) => ("message_not_pending", error.to_string()),
                MailboxError::Type(_) => ("bad_request", error.to_string()),
                MailboxError::ReplyNotVisible { .. } | MailboxError::ClaimantMismatch { .. } => {
                    ("denied", error.to_string())
                }
                MailboxError::NotificationAttemptUnknown(_) => ("no_such_alarm", error.to_string()),
                MailboxError::DuplicateIdempotencyKey { .. }
                | MailboxError::AlreadyClaimed { .. }
                | MailboxError::NotificationAttemptMismatch { .. }
                | MailboxError::NotificationClearRequiresAttention
                | MailboxError::NotificationRequeueRequiresAttention => {
                    ("conflict", error.to_string())
                }
                MailboxError::NoUnresolvedAttention(_)
                | MailboxError::NotificationAlreadyResolved(_)
                | MailboxError::NotificationResolutionInProgress(_)
                | MailboxError::NotificationResolutionAmbiguous(_) => {
                    ("conflict", error.to_string())
                }
                MailboxError::InvalidAttentionTarget(_) => ("bad_request", error.to_string()),
                _ => ("mailbox_error", error.to_string()),
            }
        }
        MailboxServiceError::Store(error) => ("mailbox_error", error.to_string()),
        MailboxServiceError::Poisoned | MailboxServiceError::ForeignDirectory => {
            ("mailbox_error", error.to_string())
        }
    };
    WireError {
        code: code.to_string(),
        message,
        data: None,
    }
}

fn wire_error_response(id: Value, error: WireError) -> Response {
    Response {
        id,
        result: None,
        error: Some(error),
    }
}

/// (pane_id, label, pane_pid) rows for sender resolution, across every
/// attached session. Retained for pre-upgrade history identity.
pub(crate) fn sender_panes(inner: &Inner) -> Vec<(String, Option<String>, i32)> {
    let labels = inner.labels();
    inner
        .session_slots()
        .iter()
        .flat_map(|slot| {
            let link = slot.link.lock().expect("session link lock");
            link.watcher
                .as_ref()
                .map(|w| w.snapshot())
                .unwrap_or_default()
        })
        .map(|row| {
            let label = labels.get(&row.pane_id).cloned();
            (row.pane_id, label, row.pane_pid)
        })
        .collect()
}

/// One exact row eligible to authenticate a hook report.
struct ReportPane {
    session_idx: usize,
    recipient_key: RecipientKey,
    pane_id: String,
    label: Option<String>,
    root: identity::ProcId,
}

/// Rows for authenticated hook origins. Detached sessions use their
/// last-known panes because the process tree can outlive control mode.
fn report_panes(inner: &Inner) -> Vec<ReportPane> {
    let panes = crate::mailbox_panes(inner, None);
    let mut rows = Vec::new();
    for (session_idx, session_instance_id, pane) in panes {
        let Some(root) = pane.root else {
            continue;
        };
        let Ok(pane_id) = pane.row.pane_id.parse() else {
            continue;
        };
        let recipient_key = RecipientKey::agent(inner.workspace_id, session_instance_id, pane_id);
        let pane_root = ProcessInstanceId::new(root.pid, root.birth).ok();
        let label = pane_root.and_then(|pane_root| {
            inner
                .registry
                .lock()
                .expect("registry lock")
                .for_route(recipient_key, pane_root)
                .map(|adoption| adoption.label.clone())
        });
        rows.push(ReportPane {
            session_idx,
            recipient_key,
            pane_id: pane.row.pane_id.clone(),
            label,
            root,
        });
    }
    rows
}

/// Resolve the opaque route token returned by the process ancestry walk.
/// The token is an index into this one snapshot, never a tmux pane id.
/// Matching the observed root as well prevents a route from crossing to a
/// different session row or to a replacement process generation.
fn report_pane_at<'a>(
    panes: &'a [ReportPane],
    route_token: &str,
    pane_root: identity::ProcId,
) -> Option<&'a ReportPane> {
    let route_idx = route_token.parse::<usize>().ok()?;
    panes.get(route_idx).filter(|pane| pane.root == pane_root)
}

/// Re-read one authenticated route without resolving its raw pane id in a
/// different watched session.
pub(crate) fn report_route_row(
    inner: &Inner,
    origin: &ReportOrigin,
) -> Option<(cyclops_tmux::PaneRow, Option<Arc<SessionWatcher>>)> {
    let slot = inner.session(origin.session_idx)?;
    let (attached, watcher, instance_id) = {
        let link = slot.link.lock().expect("session link lock");
        (
            link.attached,
            link.watcher.as_ref().map(Arc::clone),
            link.identity.as_ref()?.session_instance_id(),
        )
    };
    if origin.recipient_key.session_instance_id() != Some(instance_id) {
        return None;
    }
    if attached {
        let watcher = watcher?;
        let row = watcher.pane(&origin.pane_id)?;
        (identity::ProcId::of(row.pane_pid) == Some(origin.pane_root))
            .then_some((row, Some(watcher)))
    } else {
        let pane = slot
            .last_panes
            .lock()
            .expect("last panes lock")
            .get(&origin.pane_id)
            .cloned()?;
        (pane.root == Some(origin.pane_root)).then_some((pane.row, None))
    }
}

/// Fail-closed origin check for agent.state.report over the socket: the
/// peer's process ancestry must land in the very pane `agent` names (its
/// label or its pane id). Honest reports pass by construction, because
/// `cyclops hook` runs as a child of the vendor CLI inside the pane.
/// Everything else is denied and NOT ingested: a same-uid process outside
/// the pane (the admin shell included) could otherwise forge hook liveness
/// and tier-1 ACK evidence, and the record must never lie.
/// Who a hook report is really from.
///
/// Derived from the socket peer and the pane table, never from the
/// request. Everything downstream uses THIS, so a respawn between
/// verification and ingestion cannot hand one occupant's hook to another.
pub(crate) struct ReportOrigin {
    /// The canonical name the daemon files this report under.
    pub(crate) recipient: String,
    pub(crate) pane_id: String,
    pub(crate) session_idx: usize,
    pub(crate) recipient_key: RecipientKey,
    pub(crate) pane_root: identity::ProcId,
    /// The AGENT INSTANCE that reported: the nearest process at or above
    /// the peer whose own argv says it is an agent this daemon ships a
    /// manifest for. Not the pane's root, which is usually a shell that
    /// outlives every agent run inside it, and not the tty's current
    /// foreground group, which can be a tool the agent handed the
    /// terminal to.
    ///
    /// An identity rather than a number, so a reused pid is a different
    /// agent instead of an heir to this one's trust.
    pub(crate) agent: crate::identity::ProcId,
    pub(crate) manifest: Option<String>,
}

fn verify_report_origin(
    inner: &Inner,
    peer: Peer,
    agent: Option<&str>,
) -> Result<ReportOrigin, WireError> {
    let deny = |message: String| WireError {
        code: "denied".to_string(),
        message,
        data: None,
    };
    let (uid, pid) = daemon_peer(peer)?;
    let panes = report_panes(inner);
    let observed: Vec<_> = panes
        .iter()
        .enumerate()
        .map(|(idx, pane)| (idx.to_string(), None, pane.root))
        .collect();
    // The origin is whichever watched pane this process actually lives in.
    // A shell outside every pane resolves to admin, which has no pane and
    // therefore no hooks to report.
    // One walk, one row: the pane whose pid the ancestry actually matched.
    let (route_idx, pane_root) =
        match identity::resolve_peer_origin_observed(uid, pid, &observed, |_| {
            identity::Vendorship::NotVendor
        }) {
            identity::PeerOrigin::Pane {
                pane_id, pane_root, ..
            } => (pane_id, pane_root),
            identity::PeerOrigin::Admin | identity::PeerOrigin::Unprovable => {
                return Err(deny(
                    "hook reports come from inside an agent pane; this peer is outside every \
                     watched pane (admin cannot post hook reports)"
                        .to_string(),
                ));
            }
        };
    let pane = report_pane_at(&panes, &route_idx, pane_root).ok_or_else(|| {
        deny("the authenticated hook route vanished during verification".to_string())
    })?;
    let pane_id = pane.pane_id.clone();
    let label = pane.label.clone();
    let pane_root = pane.root;
    let recipient = label.clone().unwrap_or_else(|| pane_id.clone());
    // A supplied name is an assertion about that origin, so it has to
    // agree with it. Disagreement is a denial rather than a correction:
    // whichever of the two is wrong, acting on the report would file it
    // under a name its own sender did not believe.
    if let Some(claim) = agent {
        if claim != recipient && claim != pane_id {
            return Err(deny(format!(
                "this report claims to be {claim:?} but comes from {recipient:?}"
            )));
        }
    }
    // Live row first, last-known row second. A detached session has no
    // watcher, and deriving this live-only would make every honest hook
    // during an outage look like it came from rules the pane no longer
    // uses. The detach-aware contract is older than this check and the
    // check must not quietly repeal it.
    let provisional = ReportOrigin {
        recipient: recipient.clone(),
        pane_id: pane_id.clone(),
        session_idx: pane.session_idx,
        recipient_key: pane.recipient_key,
        pane_root,
        agent: identity::ProcId { pid: 0, birth: 0 },
        manifest: None,
    };
    if report_route_row(inner, &provisional).is_none() {
        return Err(deny(
            "this pane is gone; there is nothing for a hook report to speak for".to_string(),
        ));
    }
    // Authentication, and the pin is not evidence for it.
    //
    // Landing inside the pane only proves the peer is SOMEWHERE in it,
    // and a pane sitting at its shell prompt keeps its adoption and its
    // manifest pin while anyone at that prompt runs anything. Reading the
    // terminal's current foreground does not fix that either: a
    // hand-started `cyclops hook` holds the tty while it runs, so it
    // would present itself as the pane's agent and the pin would agree
    // with it.
    //
    // What actually admits a report is descent: the nearest process at or
    // above the peer, up to the pane root, whose own argv says it is an
    // agent this daemon ships a manifest for. A hook helper is a child of
    // the agent that ran it, so that walk lands on the agent whether the
    // agent holds the tty or handed it over; a helper nobody's agent
    // started has no such ancestor and is refused.
    let Some((vendor, agent_pid)) =
        fusion::vendor_between(inner, pane.session_idx, &pane_id, pid, pane_root.pid)
    else {
        return Err(deny(
            "hook reports come from an agent process; nothing between this peer and the pane's \
             shell is an agent cyclops has a manifest for"
                .to_string(),
        ));
    };
    // The pin says which rules read the pane. It cannot admit a process,
    // but it must not contradict one either: if the operator pinned this
    // pane to one vendor and another is running in it, the two records
    // disagree and neither is safe to act on.
    let pane_root = ProcessInstanceId::new(pane_root.pid, pane_root.birth)
        .map_err(|_| deny("this pane root has no valid process identity".to_string()))?;
    let pinned = inner
        .registry
        .lock()
        .expect("registry lock")
        .for_route(pane.recipient_key, pane_root)
        .and_then(|adoption| adoption.manifest.clone());
    if let Some(pinned) = pinned {
        if pinned != vendor.agent.id {
            return Err(deny(format!(
                "this pane is pinned to {pinned:?} but {:?} is what is running in it",
                vendor.agent.id
            )));
        }
    }
    Ok(ReportOrigin {
        recipient,
        pane_id,
        session_idx: pane.session_idx,
        recipient_key: pane.recipient_key,
        pane_root: pane.root,
        agent: agent_pid,
        manifest: Some(vendor.agent.id.clone()),
    })
}

/// Assemble StatusResult from the session slots and the detection cache.
///
/// `open_deliveries` adds the ledger-folded backlog of deliveries still
/// waiting on a human. It is opt-in because it reads the session files,
/// and only a client reconciling attention at startup needs it.
pub(crate) fn status_result(inner: &Inner, open_deliveries: bool) -> StatusResult {
    // The ledger fold happens before the state locks are taken: it reads
    // files, and the fusion engine wants those locks back promptly.
    let open_deliveries = if open_deliveries {
        crate::history::open_deliveries(inner)
    } else {
        Vec::new()
    };
    let admin_unread = inner
        .mailbox
        .as_ref()
        .and_then(|service| service.pending_count(service.admin().key).ok())
        .unwrap_or(0) as u64;
    let adoptions = inner
        .registry
        .lock()
        .expect("registry lock")
        .exact_adoptions();
    let diagnostics = crate::deadlock::status_diagnostics(inner);
    let detections = inner.detections.lock().expect("detections lock");
    let sessions = inner
        .session_slots()
        .iter()
        .enumerate()
        .map(|(session_idx, slot)| {
            let link = slot.link.lock().expect("session link lock");
            let instance_id = link
                .identity
                .as_ref()
                .map(|identity| identity.session_instance_id());
            let rows = link
                .watcher
                .as_ref()
                .map(|w| w.snapshot())
                .unwrap_or_default();
            SessionStatus {
                name: slot.name(),
                attached: link.attached,
                panes: rows
                    .iter()
                    .map(|r| {
                        let pane = crate::PaneKey::new(session_idx, &r.pane_id);
                        let entry = detections.get(&pane);
                        let recipient = instance_id.and_then(|instance_id| {
                            Some(RecipientKey::agent(
                                inner.workspace_id,
                                instance_id,
                                r.pane_id.parse().ok()?,
                            ))
                        });
                        let pane_root = identity::ProcId::of(r.pane_pid)
                            .and_then(|root| ProcessInstanceId::new(root.pid, root.birth).ok());
                        let adoption = recipient.and_then(|recipient| {
                            adoptions.iter().find(|adoption| {
                                adoption.recipient == Some(recipient)
                                    && adoption.pane_root == pane_root
                            })
                        });
                        let mut ps = r.to_status(
                            adoption.map(|adoption| adoption.label.clone()),
                            entry.and_then(|e| e.manifest.clone()),
                            entry
                                .map(|e| e.detection.state)
                                .unwrap_or(cyclops_proto::AgentState::Unknown),
                        );
                        // How long the pane has been in that state, from
                        // the change mark fusion keeps. The roster's
                        // elapsed column is this number and nothing else.
                        ps.state_ms = entry.map(|e| e.since.elapsed().as_millis() as u64);
                        // The second answer, carried from the same stamp
                        // the gate obeys. A pane with no cached detection
                        // has nothing behind it, so it stays refused.
                        ps.write_ready = entry.is_some_and(|e| e.detection.write_ready);
                        ps.write_block = entry.and_then(|e| e.detection.write_block.clone());
                        // Hook liveness (amendment c): adopted panes whose
                        // manifest declares hooks carry the verified bit,
                        // scoped to the current occupant (edges from a
                        // replaced occupant count for nothing).
                        //
                        // The occupant lookup shells out, so it runs only
                        // for panes whose answer can be anything but
                        // false. A manifest that declares no hooks
                        // already settles it, and status should not spawn
                        // a process per pane to reprint that.
                        //
                        // The agent identity comes off the same stamp as
                        // everything else in this row, and deliberately so:
                        // resolving it here would inspect processes once
                        // per pane, on a call whose whole job is to print
                        // what the daemon already knows, and would hold
                        // the detection lock across all of it. A pane that
                        // changes hands is republished by the recompute
                        // its own output triggers.
                        let bound = entry.and_then(|e| e.manifest.as_deref());
                        ps.hooks_verified = bound.and_then(|m| {
                            crate::selftest::hooks_verified_for(
                                inner,
                                &pane,
                                adoption.is_some(),
                                Some(m),
                                entry.and_then(|e| e.agent),
                            )
                        });
                        // The manifest's own display name, from the same
                        // load the daemon did at boot: a client renders
                        // daemon identity data instead of re-parsing
                        // manifest TOML off disk to recover it.
                        ps.manifest_display_name = ps
                            .manifest
                            .as_ref()
                            .and_then(|id| inner.manifests.get(id))
                            .map(|m| m.agent.display_name.clone());
                        ps
                    })
                    .collect(),
            }
        })
        .collect();
    StatusResult {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        proto: PROTOCOL_VERSION,
        boot_id: inner.boot_id.clone(),
        uptime_ms: inner.started.elapsed().as_millis() as u64,
        tmux_version: inner.tmux_version.clone(),
        sessions,
        admin_unread,
        open_deliveries,
        diagnostics,
        // Always answered, empty set included: "I loaded none" is the fact
        // a client needs to explain an unknown pane, and it is exactly the
        // fact an omitted field would hide.
        manifests: Some(cyclops_proto::Manifests {
            ids: inner.manifests.keys().cloned().collect(),
            dir: inner.manifest_dir.as_ref().map(|d| d.display().to_string()),
        }),
        // The one process that can say this without guessing.
        pid: Some(std::process::id()),
    }
}

/// pane.read: resolve the target, then capture or return the detection
/// view. Targets are pane ids until the adoption registry lands (M1).
async fn pane_read(inner: &Arc<Inner>, id: Value, params: Value) -> Response {
    let params: PaneReadParams = match decode_params(&id, params, "pane.read params") {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some((session_idx, watcher, pane_id)) = resolve_target(inner, &params.target) else {
        let known = known_panes(inner);
        return Response::err(
            id,
            "no_such_target",
            format!(
                "no such target {:?}; known panes: {}",
                params.target,
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ),
        );
    };
    match params.source {
        PaneReadSource::Visible => match watcher.client().capture_pane(&pane_id).await {
            Ok(text) => {
                let text = cap_lines(text, params.lines);
                ok_read(id, &params.target, &pane_id, Some(text), None)
            }
            Err(e) => Response::err(id, "tmux_error", e.to_string()),
        },
        PaneReadSource::Recent => {
            let lines = params.lines.unwrap_or(DEFAULT_RECENT_LINES);
            match watcher.client().capture_pane_history(&pane_id, lines).await {
                Ok(text) => ok_read(id, &params.target, &pane_id, Some(text), None),
                Err(e) => Response::err(id, "tmux_error", e.to_string()),
            }
        }
        PaneReadSource::Detection => {
            // Reconcile on doubt: an explicit detection read refreshes with
            // the full sensor set instead of trusting the cache.
            let det = match fusion::recompute_pane(
                inner,
                session_idx,
                &watcher,
                &pane_id,
                true,
                "pane.read",
            )
            .await
            {
                // Both answers travel together, and neither is computed
                // here: fusion stamped them when it produced the verdict,
                // so this surface cannot disagree with the gate.
                Some(det) => det,
                None => return Response::err(id, "no_such_target", "pane vanished during read"),
            };
            // --raw: the screen beside what the sensors made of it, in the
            // same answer, so the two halves are one moment. Two separate
            // reads can straddle a state change and then the capture
            // contradicts the verdict it is supposed to explain.
            let raw = if params.include_raw {
                match watcher.client().capture_pane(&pane_id).await {
                    Ok(text) => Some(cap_lines(text, params.lines)),
                    Err(e) => return Response::err(id, "tmux_error", e.to_string()),
                }
            } else {
                None
            };
            ok_read(id, &params.target, &pane_id, raw, Some(det))
        }
    }
}

fn ok_read(
    id: Value,
    target: &str,
    pane_id: &str,
    text: Option<String>,
    detection: Option<cyclops_proto::Detection>,
) -> Response {
    let result = PaneReadResult {
        target: target.to_string(),
        pane_id: pane_id.to_string(),
        text,
        detection,
    };
    Response::ok(
        id,
        serde_json::to_value(result).expect("pane.read result serializes"),
    )
}

/// Keep only the last `n` lines when a cap was given.
fn cap_lines(text: String, cap: Option<u32>) -> String {
    match cap {
        None => text,
        Some(n) => {
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(n as usize);
            all[start..].join("\n")
        }
    }
}

/// Find the watcher owning a target: adoption label first, then pane id,
/// same resolution order as every other verb (the CLI promises "label or
/// pane id" everywhere). The session link lock is dropped before any
/// await; only the Arc leaves the closure.
///
/// Returns the slot's index alongside the watcher and pane id: a caller
/// resolving a session verdict off this (`pane.read source=detection`)
/// needs that idx rather than a name re-derived from the watcher, for the
/// same reason `emit_state` takes one — see its doc comment.
fn resolve_target(inner: &Inner, target: &str) -> Option<(usize, Arc<SessionWatcher>, String)> {
    let (idx, pane_id) = inner.resolve_recipient(target)?;
    inner.watcher_of(idx).and_then(|watcher| {
        watcher
            .pane(&pane_id)
            .map(|row| (idx, watcher, row.pane_id))
    })
}

fn known_panes(inner: &Inner) -> Vec<String> {
    inner
        .session_slots()
        .iter()
        .flat_map(|slot| {
            let link = slot.link.lock().expect("session link lock");
            link.watcher
                .as_ref()
                .map(|w| w.snapshot())
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.pane_id)
        })
        .collect()
}

/// Write one line with the write timeout. False means drop the connection.
async fn write_line(w: &mut OwnedWriteHalf, line: &str) -> bool {
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    match tokio::time::timeout(WRITE_TIMEOUT, w.write_all(buf.as_bytes())).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            debug!(error = %e, "client write failed");
            false
        }
        Err(_) => {
            warn!("client write stalled; dropping connection");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, DetEntry};
    use cyclops_proto::{
        LiveSessionKey, MessagesChangedArea, MessagesChangedData, MessagesSnapshotResult, OsBootId,
        ProcessInstanceId, RecipientKey, SessionIdentityBinding, SessionInstanceId, TmuxPaneId,
        TmuxSessionId, WorkspaceId,
    };
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    #[test]
    fn a_nonpending_claim_has_a_recoverable_wire_code() {
        let error = mailbox_service_error(crate::mailbox::MailboxServiceError::from(
            crate::mailbox::MailboxError::MessageNotPending("m-old".parse().unwrap()),
        ));

        assert_eq!(error.code, "message_not_pending");
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_before_recursive_state_repair() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-stale-socket-repair");
        let _ = std::fs::remove_dir_all(&home);
        let state_root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        let socket_path = home.join(cyclops_proto::SOCK_NAME);
        let stale = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(stale);

        let bound = bind_socket(&state_root).await.unwrap();
        let summary = state_root
            .repair_descendant_permissions(Some(std::ffi::OsStr::new(cyclops_proto::SOCK_NAME)))
            .unwrap();

        assert!(summary.live_socket_preserved);
        UnixStream::connect(&socket_path).await.unwrap();
        drop(bound);
        assert!(!socket_path.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn root_swap_is_refused_and_cleanup_does_not_touch_the_replacement() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let home = cyclops_proto::scratch::scratch_dir("cyc-bound-root-swap");
        let displaced = cyclops_proto::scratch::scratch_dir("cyc-bound-root-displaced");
        let replacement = cyclops_proto::scratch::scratch_dir("cyc-bound-root-replacement");
        for path in [&home, &displaced, &replacement] {
            let _ = std::fs::remove_dir_all(path);
        }
        let state_root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        let bound = bind_socket(&state_root).await.unwrap();

        std::fs::create_dir_all(&replacement).unwrap();
        let external_socket_path = replacement.join(cyclops_proto::SOCK_NAME);
        let external_socket =
            std::os::unix::net::UnixListener::bind(&external_socket_path).unwrap();
        let sentinel = replacement.join("sentinel");
        std::fs::write(&sentinel, b"external\n").unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o640)).unwrap();
        let socket_before = std::fs::symlink_metadata(&external_socket_path).unwrap();
        let sentinel_before = std::fs::metadata(&sentinel).unwrap();

        std::fs::rename(&home, &displaced).unwrap();
        std::fs::rename(&replacement, &home).unwrap();
        let repair = state_root
            .repair_descendant_permissions(Some(std::ffi::OsStr::new(cyclops_proto::SOCK_NAME)))
            .unwrap();

        assert!(crate::require_bound_socket_in_state_root(&repair, &state_root).is_err());
        drop(bound);

        assert!(!displaced.join(cyclops_proto::SOCK_NAME).exists());
        let socket_after = std::fs::symlink_metadata(home.join(cyclops_proto::SOCK_NAME)).unwrap();
        let sentinel_after = std::fs::metadata(home.join("sentinel")).unwrap();
        assert_eq!(socket_after.dev(), socket_before.dev());
        assert_eq!(socket_after.ino(), socket_before.ino());
        assert_eq!(socket_after.mode(), socket_before.mode());
        assert_eq!(sentinel_after.dev(), sentinel_before.dev());
        assert_eq!(sentinel_after.ino(), sentinel_before.ino());
        assert_eq!(sentinel_after.mode(), sentinel_before.mode());
        assert_eq!(std::fs::read(home.join("sentinel")).unwrap(), b"external\n");

        drop(external_socket);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&displaced);
    }

    #[tokio::test]
    async fn bound_socket_cleanup_refuses_a_replaced_socket_entry() {
        use std::os::unix::fs::MetadataExt as _;

        let home = cyclops_proto::scratch::scratch_dir("cyc-bound-socket-replaced");
        let external = cyclops_proto::scratch::scratch_dir("cyc-bound-socket-external");
        for path in [&home, &external] {
            let _ = std::fs::remove_dir_all(path);
        }
        let state_root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        let bound = bind_socket(&state_root).await.unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let external_path = external.join("replacement.sock");
        let external_socket = std::os::unix::net::UnixListener::bind(&external_path).unwrap();
        let bound_path = home.join(cyclops_proto::SOCK_NAME);
        std::fs::remove_file(&bound_path).unwrap();
        std::fs::rename(&external_path, &bound_path).unwrap();
        let before = std::fs::symlink_metadata(&bound_path).unwrap();

        drop(bound);

        let after = std::fs::symlink_metadata(&bound_path).unwrap();
        assert_eq!(after.dev(), before.dev());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        drop(external_socket);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&external);
    }

    fn bare_inner() -> Arc<Inner> {
        let home =
            cyclops_proto::scratch::scratch_dir(&format!("cyc-unit-{}", uuid::Uuid::new_v4()));
        let state_root = Arc::new(cyclops_state::StateRoot::open_or_create(&home).unwrap());
        let (registry, _) = crate::registry::Registry::load(Arc::clone(&state_root));
        let workspace_id = crate::workspaceid::load_or_create(&state_root).unwrap();
        let session_identities = crate::sessionstore::SessionIdentities::open(&state_root).unwrap();
        Arc::new(Inner {
            cfg: Config::defaults(&home),
            state_root,
            state_repair: cyclops_state::RepairSummary::default(),
            workspace_id,
            session_identities: StdMutex::new(session_identities),
            mailbox: None,
            composer_recovery: StdMutex::new(
                crate::composer_recovery::RecoveryCoordinator::default(),
            ),
            mailbox_publication: StdMutex::new(()),
            mailbox_publish_pause: StdMutex::new(None),
            boot_id: "b-test".into(),
            started: Instant::now(),
            tmux_version: "3.6a".into(),
            manifests: BTreeMap::new(),
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            events: broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::<crate::PaneKey, DetEntry>::new()),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
            turn_ends: StdMutex::new(crate::turnkey::Ends::new()),
            argv_cache: StdMutex::new(HashMap::new()),
            engine: crate::delivery::Engine::new(),
            ack_state: crate::ack::AckState::new(),
            hook_liveness: crate::selftest::HookLiveness::new(),
            inject_pause: StdMutex::new(None),
            fail_chrome_restore: std::sync::atomic::AtomicBool::new(false),
            workspace_ui: StdMutex::new(crate::workspace_ui::WorkspaceUiState::default()),
            // No production sender behind this in tests: nothing here
            // spawns a session_task or calls Daemon::shutdown.
            stop: watch::channel(false).1,
            extra_tasks: StdMutex::new(Vec::new()),
        })
    }

    /// A daemon with one session ledger seeded from the history fixture,
    /// which carries both states a human must clear. Scratch paths go
    /// through cyclops_proto::scratch so the suite runs off macOS (F24).
    fn inner_with_ledger(tag: &str) -> (Arc<Inner>, std::path::PathBuf) {
        let dir = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        let fixture = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/history.ndjson"),
        )
        .expect("fixture reads");
        let state_root = cyclops_state::StateRoot::open_or_create(&dir).expect("state root opens");
        let mut fixture_file = state_root
            .open_append(std::path::Path::new("ledger/main.ndjson"))
            .expect("fixture file opens");
        std::io::Write::write_all(&mut fixture_file, &fixture).expect("fixture writes");
        fixture_file.sync_data().expect("fixture syncs");
        let mut inner = bare_inner();
        Arc::get_mut(&mut inner)
            .expect("sole owner")
            .sessions
            .get_mut()
            .expect("sessions lock")
            .push(Arc::new(crate::SessionSlot::new(
                "main".into(),
                Arc::new(
                    cyclops_ledger::LedgerWriter::open(
                        &state_root,
                        std::path::Path::new("ledger/main.ndjson"),
                        "b-test",
                    )
                    .expect("ledger opens"),
                ),
            )));
        (inner, dir)
    }

    /// The stream UI's attention seed. It rides the existing status answer
    /// and only when asked: the fold reads the session files, and a caller
    /// that does not need the backlog must not pay for it.
    #[tokio::test]
    async fn status_serves_open_deliveries_only_when_asked() {
        let (inner, dir) = inner_with_ledger("cyc-status-open");
        assert!(
            status_result(&inner, false).open_deliveries.is_empty(),
            "the backlog is opt-in"
        );
        let open = status_result(&inner, true).open_deliveries;
        let got: Vec<(&str, &str)> = open
            .iter()
            .map(|d| (d.to.as_str(), d.id.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![("implementer", "m-bbbbbb"), ("admin", "m-dddddd")]
        );

        // The param travels on the wire, and a request without it (every
        // caller that predates the field) still gets the shipped answer.
        let ask = Request {
            id: json!(1),
            method: "status".into(),
            params: json!({"open_deliveries": true}),
        };
        let (resp, _) = dispatch(&inner, ask, own_peer()).await;
        let result: StatusResult =
            serde_json::from_value(resp.result.expect("status answers")).expect("decodes");
        assert_eq!(result.open_deliveries.len(), 2);
        let (resp, _) = dispatch(&inner, req("status"), own_peer()).await;
        let result: StatusResult =
            serde_json::from_value(resp.result.expect("status answers")).expect("decodes");
        assert!(result.open_deliveries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Absent params mean the shipped default; a member of the wrong TYPE
    /// is an error; an unknown FIELD NAME is accepted and defaults.
    ///
    /// The last one is the tolerant-protocol rule (ADR-001 S2) and it is
    /// the interesting case, because a typo and a field a newer client
    /// added are the same bytes. This pins that the answer is then the
    /// pane half alone, which is the understating direction, and never a
    /// refusal that would break a newer client against this daemon.
    #[tokio::test]
    async fn status_defaults_on_absent_params_and_rejects_malformed_ones() {
        let (inner, dir) = inner_with_ledger("cyc-status-params");

        // No params at all: the answer every pre-struct caller gets.
        for absent in [Value::Null, json!({})] {
            let (resp, _) = dispatch(
                &inner,
                Request {
                    id: json!(1),
                    method: "status".into(),
                    params: absent.clone(),
                },
                own_peer(),
            )
            .await;
            let result: StatusResult =
                serde_json::from_value(resp.result.expect("status answers")).expect("decodes");
            assert!(
                result.open_deliveries.is_empty(),
                "{absent} must mean the shipped default"
            );
        }

        // Wrong type, wrong shape: named errors, no answer.
        for bad in [
            json!({"open_deliveries": "yes"}),
            json!({"open_deliveries": 1}),
            json!(["open_deliveries"]),
            json!("open_deliveries"),
        ] {
            let (resp, _) = dispatch(
                &inner,
                Request {
                    id: json!(1),
                    method: "status".into(),
                    params: bad.clone(),
                },
                own_peer(),
            )
            .await;
            assert!(
                resp.result.is_none(),
                "{bad} was answered instead of refused"
            );
            let err = resp.error.expect("malformed params must be an error");
            assert_eq!(err.code, "bad_request", "{bad}");
            assert!(
                err.message.contains("status params"),
                "the error must name what was wrong: {}",
                err.message
            );
        }

        // An unknown field name: accepted, defaulted, answered. A newer
        // client's added param must not break against this daemon, and a
        // typo is the same bytes.
        for tolerated in [
            json!({"open_delivery": true}),
            json!({"open_deliveries": true, "a_field_from_next_year": 7}),
        ] {
            let (resp, _) = dispatch(
                &inner,
                Request {
                    id: json!(1),
                    method: "status".into(),
                    params: tolerated.clone(),
                },
                own_peer(),
            )
            .await;
            assert!(
                resp.error.is_none(),
                "{tolerated} was refused: {:?}",
                resp.error
            );
            let result: StatusResult =
                serde_json::from_value(resp.result.expect("status answers")).expect("decodes");
            // The typo asked for nothing, so it got the pane half alone.
            assert_eq!(
                result.open_deliveries.is_empty(),
                tolerated.get("open_deliveries").is_none(),
                "{tolerated} served the wrong half"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A daemon whose mailbox is connected and empty.
    fn inner_with_mailbox(tag: &str) -> (Arc<Inner>, std::path::PathBuf) {
        let path = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&path);
        let root = cyclops_state::StateRoot::open_or_create(&path).unwrap();
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let directory = crate::mailbox::MailboxDirectory::new(workspace, []).unwrap();
        let store = crate::mailbox::MessageStore::open(
            &root,
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot",
        )
        .unwrap();
        let mut inner = bare_inner();
        let service =
            crate::mailbox::MailboxService::new_with_events(directory, store, inner.events.clone());
        Arc::get_mut(&mut inner).expect("sole owner").mailbox = Some(Arc::new(service));
        (inner, path)
    }

    /// A daemon whose mailbox holds one message with one alarm on it.
    ///
    /// Built through the real store so the alarm the operator commands
    /// read is the one the projection produces, not a hand-made record.
    fn inner_with_alarm(
        tag: &str,
        cause: cyclops_proto::NotificationAttentionCause,
    ) -> (
        Arc<Inner>,
        std::path::PathBuf,
        NotificationAttemptId,
        String,
    ) {
        use cyclops_proto::{
            Kind, MessagePresentation, NotificationBinding, NotificationManifestId,
            NotificationState, ProcessInstanceId, RecipientPresentation,
        };

        let path = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&path);
        let root = cyclops_state::StateRoot::open_or_create(&path).unwrap();
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let session = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let pane = TmuxPaneId::from_str("%1").unwrap();
        let agent = RecipientKey::agent(workspace, session, pane);
        let admin = RecipientKey::admin(workspace);

        let directory = crate::mailbox::MailboxDirectory::new(
            workspace,
            [crate::mailbox::MailboxIdentity {
                key: agent,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let mut store = crate::mailbox::MessageStore::open(
            &root,
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot",
        )
        .unwrap();

        let message_id = cyclops_proto::MessageId::new("m-alarm").unwrap();
        store
            .accept(
                message_id.clone(),
                crate::mailbox::MessageDraft {
                    kind: Kind::Msg,
                    sender: admin,
                    recipients: vec![agent],
                    subject: Some("Subject".into()),
                    body: Some("Body".into()),
                    client_key: None,
                    supersedes: None,
                    presentation: MessagePresentation {
                        sender_label: "admin".into(),
                        recipient_labels: vec![RecipientPresentation {
                            recipient: agent,
                            label: "reviewer".into(),
                        }],
                    },
                },
            )
            .unwrap();

        let attempt_id = NotificationAttemptId::generate();
        let binding = NotificationBinding {
            recipient: agent,
            leader: Some(ProcessInstanceId::new(4000, 818_000).unwrap()),
            agent: ProcessInstanceId::new(4242, 818_221).unwrap(),
            manifest: NotificationManifestId::new("codex").unwrap(),
        };
        store
            .queue_notification(message_id.clone(), agent, attempt_id)
            .unwrap();
        let mut steps = vec![
            (NotificationState::Gating, None),
            (NotificationState::Writing, Some(binding)),
        ];
        // Ask the closed cause vocabulary which state it belongs after,
        // rather than hard-coding one path and hitting an illegal cause.
        if !cause.valid_after(NotificationState::Writing) {
            steps.push((NotificationState::Staged, None));
        }
        for (state, binding) in steps {
            store
                .advance_notification(message_id.clone(), agent, attempt_id, state, binding, None)
                .unwrap();
        }
        store
            .advance_notification(
                message_id.clone(),
                agent,
                attempt_id,
                NotificationState::AttentionRequired,
                None,
                Some(cause),
            )
            .unwrap();

        let mut inner = bare_inner();
        let service =
            crate::mailbox::MailboxService::new_with_events(directory, store, inner.events.clone());
        Arc::get_mut(&mut inner).expect("sole owner").mailbox = Some(Arc::new(service));
        (inner, path, attempt_id, message_id.to_string())
    }

    async fn ask_inner(inner: &Arc<Inner>, method: &str, params: Value) -> Response {
        let request = Request {
            params,
            ..req(method)
        };
        let (response, _) = dispatch(inner, request, own_peer()).await;
        response
    }

    #[tokio::test]
    async fn send_and_reply_report_the_authoritative_workspace_state() {
        let (inner, path) = inner_with_mailbox("workspace-send-reply");
        let sent = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": ["admin"],
                "subject": "Workspace message",
                "body": "Body",
                "client_key": "send-key"
            }),
        )
        .await;
        assert!(sent.error.is_none(), "{:?}", sent.error);
        let sent = sent.result.unwrap();
        assert_eq!(sent["deliveries"][0]["notification_state"], "not_started");

        let reply = ask_inner(
            &inner,
            "msg.reply",
            json!({
                "message_id": sent["msg_id"],
                "body": "Reply",
                "client_key": "reply-key"
            }),
        )
        .await;
        assert!(reply.error.is_none(), "{:?}", reply.error);
        assert_eq!(
            reply.result.as_ref().unwrap()["deliveries"][0]["notification_state"],
            "not_started"
        );

        let lines = inner.mailbox.as_ref().unwrap().journal_lines().unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    matches!(
                        line.kind,
                        cyclops_proto::Kind::Msg | cyclops_proto::Kind::Fyi
                    )
                })
                .count(),
            2
        );
        assert_eq!(lines[1].reply_to.as_deref(), sent["msg_id"].as_str());
        std::fs::remove_dir_all(path).ok();
    }

    #[tokio::test]
    async fn removed_send_wait_is_rejected_before_message_acceptance() {
        let (inner, path) = inner_with_mailbox("removed-send-wait");
        let response = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": ["admin"],
                "subject": "Do work",
                "wait": {"until": "done", "timeout_ms": 60_000}
            }),
        )
        .await;

        assert_eq!(response.error.unwrap().code, "notification_unavailable");
        assert!(inner
            .mailbox
            .as_ref()
            .unwrap()
            .journal_lines()
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(path).ok();
    }

    #[tokio::test]
    async fn subscribe_before_snapshot_closes_the_workspace_change_race() {
        let (inner, path) = inner_with_mailbox("workspace-change-race");
        let mut events = inner.events.subscribe();
        let send_params = json!({
            "to": ["admin"],
            "subject": "Workspace message",
            "body": "Private body",
            "client_key": "race-key"
        });

        let sent = ask_inner(&inner, "msg.send", send_params.clone()).await;
        assert!(sent.error.is_none(), "{:?}", sent.error);
        let sent = sent.result.unwrap();
        let snapshot = ask_inner(&inner, "messages.snapshot", json!({"recent_settled": 20})).await;
        let snapshot: MessagesSnapshotResult =
            serde_json::from_value(snapshot.result.unwrap()).unwrap();

        let event = events.recv().await.unwrap();
        assert_eq!(event.event, "messages.changed");
        let data: MessagesChangedData = serde_json::from_value(event.data).unwrap();
        assert_eq!(event.seq, Some(data.workspace_seq));
        assert_eq!(data.workspace_id, snapshot.workspace_id);
        assert!(data.workspace_seq <= snapshot.workspace_seq);
        assert_eq!(
            data.changed,
            [
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ]
            .into_iter()
            .collect()
        );

        let claimed = ask_inner(&inner, "inbox.claim", json!({"message_id": sent["msg_id"]})).await;
        assert!(claimed.error.is_none(), "{:?}", claimed.error);
        let claim_event = events.recv().await.unwrap();
        let claim_data: MessagesChangedData = serde_json::from_value(claim_event.data).unwrap();
        assert_eq!(
            claim_data.changed,
            [MessagesChangedArea::Mailboxes].into_iter().collect()
        );
        assert!(claim_data.workspace_seq > data.workspace_seq);

        let reply_params = json!({
            "message_id": sent["msg_id"],
            "body": "Reply body",
            "client_key": "race-reply-key"
        });
        let replied = ask_inner(&inner, "msg.reply", reply_params.clone()).await;
        assert!(replied.error.is_none(), "{:?}", replied.error);
        let reply_event = events.recv().await.unwrap();
        let reply_data: MessagesChangedData = serde_json::from_value(reply_event.data).unwrap();
        assert_eq!(
            reply_data.changed,
            [
                MessagesChangedArea::Messages,
                MessagesChangedArea::Mailboxes,
            ]
            .into_iter()
            .collect()
        );
        assert!(reply_data.workspace_seq > claim_data.workspace_seq);

        for (method, params) in [
            ("inbox.claim", json!({"message_id": sent["msg_id"]})),
            ("msg.send", send_params),
            ("msg.reply", reply_params),
            ("inbox.list", json!({})),
            ("messages.snapshot", json!({"recent_settled": 20})),
            ("alarm.preview", json!({"older_than_ms": 0})),
            (
                "msg.reply",
                json!({
                    "message_id": "m-does-not-exist",
                    "body": "Rejected",
                    "client_key": "failed-reply"
                }),
            ),
        ] {
            let _ = ask_inner(&inner, method, params).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), events.recv())
                .await
                .is_err(),
            "a read, retry, re-claim, or failed mutation emitted a change"
        );
        std::fs::remove_dir_all(path).ok();
    }

    #[tokio::test]
    async fn attention_show_is_read_only_and_failed_resolution_writes_nothing() {
        let (inner, path, attempt_id, _) = inner_with_alarm(
            "cyc-attention-show-read-only",
            NotificationAttentionCause::VerifyFailed,
        );
        let journal = path.join("workspaces/current/messages.ndjson");
        let before = std::fs::read_to_string(&journal).unwrap();

        let response = ask_inner(
            &inner,
            "attention.show",
            json!({"id": attempt_id.to_string(), "diff": true}),
        )
        .await;
        let shown: cyclops_proto::AttentionShowResult =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(shown.attempt_id, attempt_id);
        assert!(!shown.checks.all_pass());
        assert!(shown.expected.is_some());
        assert!(shown.observed.is_none());
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), before);

        let response = ask_inner(
            &inner,
            "attention.complete",
            json!({"id": attempt_id.to_string()}),
        )
        .await;
        assert_eq!(response.error.unwrap().code, "attention_evidence_failed");
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), before);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn a_post_intent_failure_is_never_reported_as_a_safe_refusal() {
        let error =
            attention_action_error(crate::attention_resolution::AttentionActionError::Uncertain);
        assert_eq!(error.code, "attention_action_uncertain");
        assert!(error.message.contains("outcome is uncertain"));
    }

    /// Preview names why an attempt needs attention, so an operator can
    /// tell a composer that never took the text from one that took it and
    /// did not send.
    #[tokio::test]
    async fn preview_reports_the_cause_without_message_content() {
        for (cause, expected) in [
            (
                cyclops_proto::NotificationAttentionCause::VerifyFailed,
                "verify_failed",
            ),
            (
                cyclops_proto::NotificationAttentionCause::SubmitFailed,
                "submit_failed",
            ),
        ] {
            let (inner, path, attempt_id, message_id) =
                inner_with_alarm(&format!("operator-cause-{expected}"), cause);
            let response = ask_inner(&inner, "alarm.preview", json!({"older_than_ms": 0})).await;
            assert!(response.error.is_none(), "{:?}", response.error);
            let value = response.result.unwrap();
            let entry = &value["entries"][0];
            assert_eq!(entry["cause"], expected);
            assert_eq!(entry["id"], attempt_id.to_string());
            assert_eq!(entry["message_id"], message_id);
            // The subject and body the message carries stay behind.
            assert!(entry.get("subject").is_none() && entry.get("body").is_none());
            std::fs::remove_dir_all(path).ok();
        }
    }

    #[tokio::test]
    async fn messages_snapshot_answers_from_the_authenticated_workspace_projection() {
        let (inner, path, attempt_id, message_id) = inner_with_alarm(
            "messages-snapshot",
            cyclops_proto::NotificationAttentionCause::VerifyFailed,
        );
        let response = ask_inner(&inner, "messages.snapshot", json!({})).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let value = response.result.unwrap();
        let row = &value["rows"][0];
        assert_eq!(row["message_id"], message_id);
        assert_eq!(row["direction"], "outbound");
        assert_eq!(row["needs_action"], true);
        assert_eq!(value["counts"]["work_messages"], 1);
        assert_eq!(value["counts"]["outbound_messages"], 1);
        assert_eq!(value["counts"]["inbox_messages"], 0);
        assert_eq!(row["recipients"][0]["available"], true);
        assert_eq!(row["recipients"][0]["mailbox"]["status"], "pending");
        assert_eq!(
            row["recipients"][0]["notification"]["state"],
            "attention_required"
        );
        assert_eq!(
            row["recipients"][0]["notification"]["attempt_id"],
            attempt_id.to_string()
        );
        assert_eq!(
            row["recipients"][0]["notification"]["cause"],
            "verify_failed"
        );
        assert_eq!(
            row["recipients"][0]["notification"]["attention_cleared"],
            false
        );
        assert!(row.get("body").is_none());
        assert!(!value.to_string().contains("Body"));

        let too_large =
            ask_inner(&inner, "messages.snapshot", json!({"recent_settled": 101})).await;
        assert_eq!(too_large.error.unwrap().code, "bad_request");
        std::fs::remove_dir_all(path).ok();
    }

    /// A message nobody has heard of is a mistake worth reporting. A known
    /// message with nothing in attention is a quiet, honest no.
    #[tokio::test]
    async fn requeue_separates_an_unknown_message_from_a_quiet_one() {
        let (inner, path, _, message_id) = inner_with_alarm(
            "operator-requeue-known",
            cyclops_proto::NotificationAttentionCause::SubmitFailed,
        );

        let response = ask_inner(&inner, "msg.requeue", json!({"message_id": "m-nobody"})).await;
        assert_eq!(response.error.unwrap().code, "no_such_message");

        // The alarm on the known message is requeued once.
        let response = ask_inner(&inner, "msg.requeue", json!({"message_id": message_id})).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result.unwrap()["requeued"], true);

        // The fresh attempt is queued, not an alarm, so a second requeue
        // has nothing to act on and says so without failing.
        let response = ask_inner(&inner, "msg.requeue", json!({"message_id": message_id})).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result.unwrap()["requeued"], false);
        std::fs::remove_dir_all(path).ok();
    }

    async fn call(tag: &str, method: &str, params: Value) -> Response {
        let (inner, path) = inner_with_mailbox(&format!("operator-{tag}"));
        let request = Request {
            params,
            ..req(method)
        };
        let (response, _) = dispatch(&inner, request, own_peer()).await;
        std::fs::remove_dir_all(path).ok();
        response
    }

    /// The operator commands answer from the durable projection instead of
    /// reporting it unavailable, and each one refuses what it cannot name.
    ///
    /// Every case runs through `dispatch`, so the admin gate each handler
    /// applies is exercised rather than bypassed.
    #[tokio::test]
    async fn operator_commands_answer_from_the_durable_projection() {
        // Preview on an empty workspace is an empty list, not an error.
        let response = call("preview", "alarm.preview", json!({"older_than_ms": 0})).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let result: AlarmPreviewResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(result.entries.is_empty());

        // A malformed identifier never reaches the store.
        let response = call("bad-id", "alarm.clear", json!({"ids": ["not-an-attempt"]})).await;
        assert_eq!(response.error.unwrap().code, "bad_request");

        // A well-formed identifier that names no alarm is refused.
        let unknown = NotificationAttemptId::generate().to_string();
        let response = call("unknown-id", "alarm.clear", json!({"ids": [unknown]})).await;
        assert_eq!(response.error.unwrap().code, "no_such_alarm");

        // Clearing nothing is refused rather than treated as success.
        let response = call("empty-ids", "alarm.clear", json!({"ids": []})).await;
        assert_eq!(response.error.unwrap().code, "bad_request");

        // A message this workspace never saw is named, not reported as a
        // quiet no. The quiet no belongs to a message that exists.
        let response = call("requeue", "msg.requeue", json!({"message_id": "m-absent"})).await;
        assert_eq!(response.error.unwrap().code, "no_such_message");
    }

    fn req(method: &str) -> Request {
        Request {
            id: json!(1),
            method: method.into(),
            params: json!({}),
        }
    }

    /// This process, over a real socket, so the peer is attested the way
    /// a client's would be rather than asserted by the test.
    ///
    /// The pair is leaked deliberately: the descriptor has to stay open
    /// for the whole test, because every gated request asks it again.
    fn own_peer() -> Peer {
        use std::os::fd::AsRawFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let fd = a.as_raw_fd();
        let id = identity::peer_identity_fd(fd).expect("peer identity");
        std::mem::forget(a);
        std::mem::forget(b);
        Some(identity::PeerConn { id, fd })
    }

    #[test]
    fn mailbox_caller_waits_for_route_publication() {
        let inner = bare_inner();
        let peer = own_peer();
        let publication = inner.mailbox_publication.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let inner = Arc::clone(&inner);
            let reader = scope.spawn(move || {
                started_tx.send(()).unwrap();
                let _ = mailbox_caller(&inner, peer);
                done_tx.send(()).unwrap();
            });
            started_rx.recv().unwrap();
            let overlapped = done_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_ok();
            drop(publication);
            reader.join().unwrap();
            assert!(!overlapped, "mailbox caller observed a partial publication");
        });
    }

    #[test]
    fn exact_route_tokens_separate_duplicate_pane_ids_and_reject_crossed_roots() {
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let pane = TmuxPaneId::from_str("%1").unwrap();
        let first_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let second_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        let first_root = identity::ProcId { pid: 41, birth: 1 };
        let second_root = identity::ProcId { pid: 42, birth: 2 };
        let panes = vec![
            ReportPane {
                session_idx: 0,
                recipient_key: RecipientKey::agent(workspace, first_session, pane),
                pane_id: pane.to_string(),
                label: Some("first".into()),
                root: first_root,
            },
            ReportPane {
                session_idx: 1,
                recipient_key: RecipientKey::agent(workspace, second_session, pane),
                pane_id: pane.to_string(),
                label: Some("second".into()),
                root: second_root,
            },
        ];

        let selected = report_pane_at(&panes, "1", second_root).unwrap();
        assert_eq!(selected.session_idx, 1);
        assert_eq!(selected.label.as_deref(), Some("second"));
        assert_eq!(
            selected.recipient_key,
            RecipientKey::agent(workspace, second_session, pane)
        );

        assert!(
            report_pane_at(&panes, "1", first_root).is_none(),
            "a route token cannot borrow another session's matching pane root"
        );
        assert!(report_pane_at(&panes, "%1", second_root).is_none());
    }

    #[test]
    fn mailbox_origin_requires_the_current_durable_pane_binding() {
        let inner = bare_inner();
        let path = inner.state_root.path().to_path_buf();
        let workspace = inner.workspace_id;
        let session = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let pane = TmuxPaneId::from_str("%1").unwrap();
        let key = RecipientKey::agent(workspace, session, pane);
        let root_process = identity::ProcId::of(std::process::id() as i32).unwrap();
        let ledger = cyclops_ledger::LedgerWriter::open(
            &inner.state_root,
            Path::new("ledger/mailbox-origin.ndjson"),
            "boot",
        )
        .unwrap();
        let slot = Arc::new(crate::SessionSlot::new(
            "mailbox-origin".into(),
            Arc::new(ledger),
        ));
        slot.last_panes.lock().unwrap().insert(
            pane.to_string(),
            crate::ObservedPane {
                row: cyclops_tmux::PaneRow {
                    pane_id: pane.to_string(),
                    window_id: "@1".into(),
                    window_name: "mailbox".into(),
                    title: String::new(),
                    dead: false,
                    in_mode: false,
                    current_command: "test".into(),
                    width: 80,
                    height: 24,
                    active: true,
                    pane_pid: root_process.pid,
                },
                root: Some(root_process),
            },
        );
        slot.link.lock().unwrap().identity = Some(SessionIdentityBinding::new(
            LiveSessionKey::new(
                workspace,
                OsBootId::new("boot-test").unwrap(),
                ProcessInstanceId::new(900, 1000).unwrap(),
                TmuxSessionId::from_str("$1").unwrap(),
            ),
            session,
        ));
        inner.sessions.lock().unwrap().push(Arc::clone(&slot));
        inner
            .registry
            .lock()
            .unwrap()
            .adopt(
                crate::registry::Adoption {
                    session: "mailbox-origin".into(),
                    pane_id: pane.to_string(),
                    label: "reviewer".into(),
                    recipient: Some(key),
                    pane_root: Some(
                        ProcessInstanceId::new(root_process.pid, root_process.birth).unwrap(),
                    ),
                    manifest: None,
                    pane_pid: root_process.pid,
                    window_id: "@1".into(),
                    border_format: None,
                },
                crate::registry::WindowChrome {
                    session: "mailbox-origin".into(),
                    window_id: "@1".into(),
                    border_status: None,
                },
            )
            .unwrap();
        let directory = crate::mailbox::MailboxDirectory::new(
            workspace,
            [crate::mailbox::MailboxIdentity {
                key,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
        let store = crate::mailbox::MessageStore::open(
            &inner.state_root,
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot",
        )
        .unwrap();
        let service = crate::mailbox::MailboxService::new(directory, store);

        assert_eq!(
            mailbox_identity_from_origin(&inner, &service, identity::PeerOrigin::Admin)
                .unwrap()
                .key,
            RecipientKey::admin(workspace)
        );
        assert_eq!(
            mailbox_identity_from_origin(
                &inner,
                &service,
                identity::PeerOrigin::Pane {
                    pane_id: "%1".into(),
                    label: Some("stale-display-name".into()),
                    pane_root: root_process,
                },
            )
            .unwrap()
            .key,
            key
        );
        let predecessor_root = identity::ProcId {
            pid: root_process.pid,
            birth: root_process.birth.checked_sub(1).unwrap_or(1),
        };
        slot.last_panes.lock().unwrap().get_mut("%1").unwrap().root = Some(predecessor_root);
        let reused_pid = mailbox_identity_from_origin(
            &inner,
            &service,
            identity::PeerOrigin::Pane {
                pane_id: "%1".into(),
                label: None,
                pane_root: root_process,
            },
        )
        .unwrap_err();
        assert_eq!(reused_pid.code, "denied");
        slot.last_panes.lock().unwrap().get_mut("%1").unwrap().root = Some(root_process);
        let replacement_session =
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap();
        slot.link.lock().unwrap().identity = Some(SessionIdentityBinding::new(
            LiveSessionKey::new(
                workspace,
                OsBootId::new("boot-test").unwrap(),
                ProcessInstanceId::new(901, 2000).unwrap(),
                TmuxSessionId::from_str("$2").unwrap(),
            ),
            replacement_session,
        ));
        let stale_directory = mailbox_identity_from_origin(
            &inner,
            &service,
            identity::PeerOrigin::Pane {
                pane_id: "%1".into(),
                label: None,
                pane_root: root_process,
            },
        )
        .unwrap_err();
        assert_eq!(stale_directory.code, "denied");
        let missing = mailbox_identity_from_origin(
            &inner,
            &service,
            identity::PeerOrigin::Pane {
                pane_id: "%2".into(),
                label: None,
                pane_root: identity::ProcId { pid: 30, birth: 2 },
            },
        )
        .unwrap_err();
        assert_eq!(missing.code, "denied");
        let unprovable =
            mailbox_identity_from_origin(&inner, &service, identity::PeerOrigin::Unprovable)
                .unwrap_err();
        assert_eq!(unprovable.code, "denied");
        drop(service);
        drop(inner);
        drop(slot);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Read the public method literals from the dispatch match itself.
    ///
    /// This deliberately mirrors [`emitted_events`]: the daemon has one
    /// executable catalogue, so documentation parity cannot stay green
    /// after a dispatch arm is added without its protocol entry. Methods
    /// reserved for a later milestone also answer through the fallback and
    /// remain part of the advertised catalogue.
    fn protocol_v1_methods() -> Vec<&'static str> {
        let source = include_str!("server.rs");
        let dispatch = source
            .split_once("match req.method.as_str() {")
            .expect("dispatch method match")
            .1
            .split_once("\n        method => {")
            .expect("dispatch fallback arm")
            .0;
        let mut methods: Vec<&str> = dispatch
            .lines()
            .filter_map(|line| {
                line.strip_prefix("        \"")
                    .and_then(|line| line.split_once("\" =>").map(|(method, _)| method))
            })
            .collect();
        methods.extend(UNIMPLEMENTED.iter().map(|(method, _)| *method));

        let mut unique = std::collections::BTreeSet::new();
        for method in &methods {
            assert!(unique.insert(*method), "duplicate protocol method {method}");
        }
        methods
    }

    #[tokio::test]
    async fn dispatch_covers_protocol_v1() {
        let inner = bare_inner();
        for method in protocol_v1_methods() {
            let (resp, _) = dispatch(&inner, req(method), own_peer()).await;
            if let Some(err) = &resp.error {
                assert_ne!(err.code, "unknown_method", "{method} fell through dispatch");
            }
        }
        let (resp, _) = dispatch(&inner, req("bogus.method"), own_peer()).await;
        assert_eq!(resp.error.unwrap().code, "unknown_method");
    }

    /// docs/reference/PROTOCOL.md is the page a script writer works from, and it is
    /// the only page that documents the wire. It shipped M5 without
    /// `theme.reload`, the method M5 added, and with no event catalogue at
    /// all: `events.subscribe` takes a `kinds` filter and the page named
    /// nothing to filter on.
    ///
    /// Both halves are read out of the daemon rather than listed here, so
    /// a new method or a new event fails until the page carries it.
    #[test]
    fn the_protocol_page_names_every_method_and_every_event() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let page = std::fs::read_to_string(root.join("../../docs/reference/PROTOCOL.md"))
            .expect("read docs/reference/PROTOCOL.md");
        let mut missing: Vec<String> = Vec::new();
        for method in protocol_v1_methods() {
            if !page.contains(&format!("`{method}`")) {
                missing.push(format!("method {method}"));
            }
        }
        for event in emitted_events(&root.join("src")) {
            if !page.contains(&format!("`{event}`")) {
                missing.push(format!("event {event}"));
            }
        }
        assert!(
            missing.is_empty(),
            "docs/reference/PROTOCOL.md documents the wire and does not mention these: {missing:#?}"
        );
    }

    /// Every event name the daemon emits, read off the `emit` call sites.
    ///
    /// The names are string literals scattered across the crate and there
    /// is no list of them anywhere else; scanning is what makes a ninth
    /// event fail the check above instead of shipping undocumented.
    fn emitted_events(src: &std::path::Path) -> Vec<String> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(src).expect("read src").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            for (idx, _) in text.match_indices(".emit(") {
                let rest = &text[idx..];
                let Some(open) = rest.find('"') else { continue };
                let Some(close) = rest[open + 1..].find('"') else {
                    continue;
                };
                let name = &rest[open + 1..open + 1 + close];
                // The event name is the first argument; anything else at
                // this position is not one and is not ours to document.
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        assert!(!names.is_empty(), "found no emit call sites in {src:?}");
        names
    }

    #[tokio::test]
    async fn unimplemented_methods_name_their_milestone() {
        let inner = bare_inner();
        for (method, milestone) in UNIMPLEMENTED {
            let (resp, _) = dispatch(&inner, req(method), own_peer()).await;
            let err = resp.error.expect("unimplemented answers with an error");
            assert_eq!(err.code, "unimplemented", "{method}");
            assert_eq!(err.message, format!("coming in {milestone}"), "{method}");
        }
    }

    /// msg.history is implemented from M2: an empty daemon answers with an
    /// empty page, and "me" filters stay fail-closed without credentials.
    #[tokio::test]
    async fn msg_history_answers_and_me_fails_closed() {
        let inner = bare_inner();
        let (resp, _) = dispatch(&inner, req("msg.history"), own_peer()).await;
        let result = resp.result.expect("history answers");
        assert_eq!(result["lines"], json!([]));
        assert!(result.get("next_cursor").is_none());

        let (resp, _) = dispatch(
            &inner,
            Request {
                id: json!(2),
                method: "msg.history".into(),
                params: json!({"to": "me"}),
            },
            None,
        )
        .await;
        assert_eq!(resp.error.unwrap().code, "denied");
    }

    #[tokio::test]
    async fn msg_thread_unknown_id_is_a_named_error() {
        let inner = bare_inner();
        let (resp, _) = dispatch(
            &inner,
            Request {
                id: json!(3),
                method: "msg.thread".into(),
                params: json!({"id": "m-nope00"}),
            },
            own_peer(),
        )
        .await;
        assert_eq!(resp.error.unwrap().code, "no_such_message");
    }

    #[tokio::test]
    async fn msg_send_fails_closed_without_peer_credentials() {
        let inner = bare_inner();
        let (resp, _) = dispatch(
            &inner,
            Request {
                id: json!(9),
                method: "msg.send".into(),
                params: json!({"to": ["reviewer"], "subject": "hi"}),
            },
            None,
        )
        .await;
        assert_eq!(resp.error.unwrap().code, "denied");
    }

    /// agent.state.report over the socket is pinned to the reporting pane:
    /// a same-uid peer outside every watched pane (this test process) is
    /// the admin, and admin cannot post hook reports. No credentials at
    /// all fails the same way.
    #[tokio::test]
    async fn state_report_over_socket_is_denied_outside_the_pane() {
        let inner = bare_inner();
        let params = json!({"agent": "reviewer", "event": "Stop"});
        let (resp, _) = dispatch(
            &inner,
            Request {
                id: json!(4),
                method: "agent.state.report".into(),
                params: params.clone(),
            },
            own_peer(),
        )
        .await;
        assert_eq!(resp.error.unwrap().code, "denied");

        let (resp, _) = dispatch(
            &inner,
            Request {
                id: json!(5),
                method: "agent.state.report".into(),
                params,
            },
            None,
        )
        .await;
        assert_eq!(resp.error.unwrap().code, "denied");
    }

    // Authority is checked when it is USED, not when the socket was
    // accepted, and that cannot be proven from inside one process: both
    // ends of a socketpair are this process, so the attested identity a
    // re-read returns is always this execution's own. The regression
    // lives in `tests/sender_identity.rs`, where a real client execs on
    // a connection it already opened.
    //
    // The foreign-uid case is gone with it. The uid now comes from the
    // kernel rather than from the caller, so it cannot be asserted, and
    // that branch is reachable only from a socket another user really
    // connected.

    #[tokio::test]
    async fn ping_and_status_answer_without_tmux() {
        let inner = bare_inner();
        let (resp, _) = dispatch(&inner, req("ping"), own_peer()).await;
        assert_eq!(resp.result.unwrap()["pong"], true);
        let (resp, _) = dispatch(&inner, req("status"), own_peer()).await;
        let result = resp.result.unwrap();
        assert_eq!(result["boot_id"], "b-test");
        assert_eq!(result["sessions"], json!([]));
    }

    #[tokio::test]
    async fn subscribe_acks_and_switches_mode() {
        let inner = bare_inner();
        let (resp, sub) = dispatch(
            &inner,
            Request {
                id: json!("s"),
                method: "events.subscribe".into(),
                params: json!({"kinds": ["state"]}),
            },
            own_peer(),
        )
        .await;
        assert_eq!(resp.result.unwrap()["subscribed"], true);
        assert_eq!(sub.unwrap().kinds, vec!["state"]);
    }

    #[test]
    fn kind_prefix_filter() {
        let none: Vec<String> = vec![];
        assert!(kind_matches(&none, "state"));
        let kinds = vec!["state".to_string(), "msg".to_string()];
        assert!(kind_matches(&kinds, "state"));
        assert!(kind_matches(&kinds, "state.changed"));
        assert!(kind_matches(&kinds, "msg.delivered"));
        assert!(!kind_matches(&kinds, "gate"));
    }

    #[test]
    fn malformed_json_is_bad_request_with_null_id() {
        let resp = parse_request("not json at all").unwrap_err();
        assert!(resp.id.is_null());
        assert_eq!(resp.error.unwrap().code, "bad_request");
    }

    #[test]
    fn cap_keeps_the_tail() {
        let text = "a\nb\nc\nd".to_string();
        assert_eq!(cap_lines(text.clone(), Some(2)), "c\nd");
        assert_eq!(cap_lines(text.clone(), Some(10)), "a\nb\nc\nd");
        assert_eq!(cap_lines(text, None), "a\nb\nc\nd");
    }
}
