//! NDJSON socket server: hello line first, then a request loop per
//! connection, switching to event push after events.subscribe.
//!
//! Slow consumers never stall the daemon: every subscriber reads from its
//! own broadcast receiver, a lagged receiver is dropped with a warning,
//! and writes carry a timeout so a wedged client costs one connection.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context as _;
use cyclops_proto::{
    AdminNotifyParams, AgentWaitParams, AlarmClearParams, AlarmPreviewParams,
    AttentionResolveParams, AttentionShowParams, DaemonShutdownParams, DaemonShutdownResult, Event,
    FrameContract, FrameSize, Hello, InboxClaimParams, InboxListParams, MessagesFollowParams,
    MessagesSnapshotParams, MsgSendParams, NotificationAttemptId, NotificationResolution,
    NotificationWithdrawParams, PaneReadParams, PaneReadResult, PaneReadSource, PingResult,
    ProcessInstanceId, QuiesceParams, RecipientKey, ReplyParams, Request, RequeueParams,
    RequeueResult, Response, SessionStatus, StateReportParams, StatusParams, StatusResult,
    StreamBackfillParams, SubscribeParams, WireError, PROTOCOL_VERSION,
};
#[cfg(test)]
use cyclops_proto::{AlarmPreviewResult, NotificationAttentionCause, TmuxPaneId};
use cyclops_state::{BoundSocketCleanup, StateRoot};
use cyclops_tmux::SessionWatcher;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};

use crate::{ack, delivery, fusion, identity, unix_ms, Inner};

/// Peer credentials captured once per connection, before the stream is
/// split. None means the kernel could not report them; identity-gated
/// methods fail closed on it.
pub(crate) type Peer = Option<identity::PeerConn>;

/// A write that does not finish inside this window means the client is
/// wedged; the connection is dropped rather than buffered without bound.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

const STATUS_BLOCKED_NOTIFICATION_LIMIT: usize = 32;
const STATUS_REFRESH_INCOMPLETE: &str = "status_refresh_incomplete";

/// The same source build identifier written on the daemon boot log line.
const BUILD_REF: &str = cyclops_proto::BUILD_REF;

/// One kernel observation shared by every hello and status answer.
fn daemon_process() -> Option<ProcessInstanceId> {
    static PROCESS: OnceLock<Option<ProcessInstanceId>> = OnceLock::new();
    *PROCESS.get_or_init(|| {
        let process = identity::ProcId::of(std::process::id() as i32)?;
        ProcessInstanceId::new(process.pid, process.birth).ok()
    })
}

/// One self-observed executable path shared by every hello and status answer.
fn daemon_executable() -> Option<String> {
    static EXECUTABLE: OnceLock<Option<String>> = OnceLock::new();
    EXECUTABLE
        .get_or_init(|| {
            let path = std::env::current_exe().ok()?;
            let path = std::fs::canonicalize(path).ok()?;
            path.into_os_string().into_string().ok()
        })
        .clone()
}

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
const MAX_FOLLOW_MESSAGES: u32 = 256;

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
                    inner
                        .engine
                        .spawn_descendant_task(handle_conn(Arc::clone(&inner), stream));
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
        build: Some(BUILD_REF.to_string()),
        daemon_process: daemon_process(),
        daemon_executable: daemon_executable(),
        proto: PROTOCOL_VERSION,
        boot_id: inner.boot_id.clone(),
    };
    let Ok(hello_frame) = encode_frame(&hello) else {
        warn!("daemon hello exceeds the official frame contract");
        return;
    };
    if !write_frame(&mut w, &hello_frame).await {
        return;
    }
    let mut reader = BufReader::new(read_half);
    let mut sub: Option<(broadcast::Receiver<Event>, Vec<String>)> = None;
    let mut stop = inner.stop.clone();

    loop {
        let pumped = tokio::select! {
            _ = stop.changed() => return,
            pumped = async {
                match &mut sub {
                    Some((rx, _)) => tokio::select! {
                        ev = rx.recv() => Pumped::Ev(ev),
                        line = read_frame(&mut reader) => Pumped::Line(line),
                    },
                    None => Pumped::Line(read_frame(&mut reader).await),
                }
            } => pumped,
        };
        match pumped {
            Pumped::Ev(Ok(ev)) => {
                let kinds = &sub.as_ref().expect("subscribed").1;
                if kind_matches(kinds, &ev.event) {
                    let frame = match encode_frame(&ev) {
                        Ok(frame) => frame,
                        Err(FrameEncodeError::TooLarge) => {
                            warn!("event exceeds the official frame contract; dropping connection");
                            return;
                        }
                        Err(FrameEncodeError::Serialize(error)) => {
                            warn!(error = %error, "event serialization failed; dropping connection");
                            return;
                        }
                    };
                    if !write_frame(&mut w, &frame).await {
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
            Pumped::Line(Ok(Some(line))) => match tokio::select! {
                _ = stop.changed() => return,
                outcome = handle_line(&inner, &line, peer, &mut w) => outcome,
            } {
                LineOutcome::Continue => {}
                LineOutcome::Drop => return,
                LineOutcome::Subscribed(kinds, rx) => sub = Some((rx, kinds)),
            },
            // EOF or read error: the client is gone.
            Pumped::Line(Err(error)) => {
                debug!(error = %error, "client frame rejected; dropping connection");
                return;
            }
            Pumped::Line(Ok(None)) => return,
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
            let Some(frame) = response_frame(&resp) else {
                return LineOutcome::Drop;
            };
            return if write_frame(w, &frame).await {
                LineOutcome::Continue
            } else {
                LineOutcome::Drop
            };
        }
    };
    let shutdown_after_write = req.method == "daemon.shutdown";
    let (resp, subscribe) = dispatch(inner, req, peer).await;
    let shutdown_after_write = shutdown_after_write
        && resp.error.is_none()
        && resp
            .result
            .as_ref()
            .and_then(|result| result.get("stopping"))
            .and_then(Value::as_bool)
            == Some(true);
    let Some(frame) = response_frame(&resp) else {
        return LineOutcome::Drop;
    };
    if let Some(params) = subscribe {
        // Subscribe before writing the ack so no event can fall between.
        let rx = inner.events.subscribe();
        if !write_frame(w, &frame).await {
            return LineOutcome::Drop;
        }
        return LineOutcome::Subscribed(params.kinds, rx);
    }
    if write_frame(w, &frame).await {
        if shutdown_after_write {
            inner.shutdown_request.send_replace(true);
            return LineOutcome::Drop;
        }
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
            let incomplete = crate::refresh_status_observations(inner).await;
            let result = status_result_with_refresh(inner, params.open_deliveries, &incomplete);
            (
                Response::ok(id, serde_json::to_value(result).expect("status serializes")),
                None,
            )
        }
        "status.reset" => {
            let params: cyclops_proto::StatusResetParams = match req.params {
                Value::Null => cyclops_proto::StatusResetParams::default(),
                given => match decode_params(&id, given, "status.reset params") {
                    Ok(p) => p,
                    Err(r) => return (r, None),
                },
            };
            let result = status_reset(inner, params).await;
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("status reset serializes"),
                ),
                None,
            )
        }
        "health.snapshot" => {
            if !matches!(&req.params, Value::Null)
                && !matches!(&req.params, Value::Object(fields) if fields.is_empty())
            {
                return (
                    Response::err(id, "bad_request", "health.snapshot accepts no parameters"),
                    None,
                );
            }
            // Health is observational. It reads the last committed daemon
            // projection and never captures panes, publishes facts, or wakes
            // delivery work.
            let result = status_result(inner, false);
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("health snapshot serializes"),
                ),
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
        "daemon.shutdown" => {
            let params: DaemonShutdownParams =
                match decode_params(&id, req.params, "daemon.shutdown params") {
                    Ok(params) => params,
                    Err(response) => return (response, None),
                };
            if daemon_process() != Some(params.daemon_process) || inner.boot_id != params.boot_id {
                return (
                    Response::err(
                        id,
                        "daemon_changed",
                        "daemon identity changed; nothing was stopped",
                    ),
                    None,
                );
            }
            let quiesced = delivery::quiesce(inner, params.timeout_ms).await;
            let result = DaemonShutdownResult {
                stopping: quiesced.quiet,
                in_flight: quiesced.in_flight,
            };
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("shutdown serializes"),
                ),
                None,
            )
        }
        "msg.send" => (msg_send(inner, id, req.params, peer).await, None),
        "msg.reply" => (msg_reply(inner, id, req.params, peer).await, None),
        "inbox.list" => (inbox_list(inner, id, req.params, peer), None),
        "inbox.claim" => (inbox_claim(inner, id, req.params, peer), None),
        "messages.snapshot" => (messages_snapshot(inner, id, req.params, peer), None),
        "messages.follow" => (messages_follow(inner, id, req.params, peer), None),
        "msg.requeue" => (msg_requeue(inner, id, req.params, peer), None),
        "notification.withdraw" => (notification_withdraw(inner, id, req.params, peer), None),
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
                from_result(
                    id,
                    crate::history::msg_thread(inner, &params.id, params.body, peer),
                ),
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
            // `cursor` is accepted only as a compatibility input. This
            // stream is deliberately ephemeral; authoritative recovery is a
            // snapshot or a domain-specific follow page.
            (Response::ok(id, json!({"subscribed": true})), Some(params))
        }
        "events.backfill" => {
            let params: StreamBackfillParams = if req.params.is_null() {
                StreamBackfillParams::default()
            } else {
                match decode_params(&id, req.params, "events.backfill params") {
                    Ok(params) => params,
                    Err(response) => return (response, None),
                }
            };
            let result = crate::history::stream_backfill(inner, params);
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("stream backfill serializes"),
                ),
                None,
            )
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
        "notification.force_submit.get" => {
            if let Err(error) = require_workspace_messaging_admin(inner, peer) {
                return (
                    Response {
                        id,
                        result: None,
                        error: Some(error),
                    },
                    None,
                );
            }
            let (enabled, delay_ms) = inner.force_submit.get();
            let result = cyclops_proto::ForceSubmitSettings {
                enabled,
                delay_seconds: u8::try_from(delay_ms / 1_000).unwrap_or(20).min(20),
            };
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("force-submit settings serialize"),
                ),
                None,
            )
        }
        "notification.force_submit.set" => {
            if let Err(error) = require_workspace_messaging_admin(inner, peer) {
                return (
                    Response {
                        id,
                        result: None,
                        error: Some(error),
                    },
                    None,
                );
            }
            let params: cyclops_proto::ForceSubmitSettingsSetParams =
                match decode_params(&id, req.params, "notification.force_submit.set params") {
                    Ok(params) => params,
                    Err(response) => return (response, None),
                };
            if params.delay_seconds > 20 {
                return (
                    Response::err(
                        id,
                        "bad_request",
                        "force-submit delay must be between 0 and 20 seconds",
                    ),
                    None,
                );
            }
            let delay_ms = u64::from(params.delay_seconds) * 1_000;
            if let Err(error) = inner
                .force_submit
                .save_and_set(params.enabled, delay_ms, || {
                    crate::config::save_force_notification_submit(
                        &inner.cfg.home,
                        params.enabled,
                        delay_ms,
                    )
                })
            {
                return (
                    Response::err(
                        id,
                        "storage_failed",
                        format!("cannot save force-submit setting: {error}"),
                    ),
                    None,
                );
            }
            if params.enabled {
                if let Some(messaging) = inner.workspace_messaging() {
                    messaging.force_submit_enabled();
                }
            }
            let result = cyclops_proto::ForceSubmitSettings {
                enabled: params.enabled,
                delay_seconds: params.delay_seconds,
            };
            (
                Response::ok(
                    id,
                    serde_json::to_value(result).expect("force-submit settings serialize"),
                ),
                None,
            )
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
/// (hook-ACK evidence) must never disagree about who is allowed in. A
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
    let (messaging, sender) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    if params
        .expected_caller
        .is_some_and(|expected| expected != sender.key)
    {
        return Response::err(
            id,
            "denied",
            "the authenticated mailbox caller changed after the client snapshot",
        );
    }
    if params.reply_to.is_some() && (params.fyi || params.supersedes.is_some()) {
        return Response::err(
            id,
            "bad_request",
            "a reply cannot be an announcement or supersede another message",
        );
    }
    match messaging.send(sender, params).await {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("message acceptance serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

async fn msg_reply(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: ReplyParams = match decode_params(&id, params, "msg.reply params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (messaging, sender) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match messaging
        .reply(
            sender,
            params.message_id,
            params.summary,
            params.body,
            params.client_key,
        )
        .await
    {
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
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match messaging.inbox_list(caller.key, params.sender, params.limit) {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("inbox list serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn inbox_claim(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: InboxClaimParams = match decode_params(&id, params, "inbox.claim params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    let result = match messaging.claim(caller.key, params.message_id) {
        Ok(result) => result,
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
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match messaging.messages_snapshot(caller.key, params.recent_settled) {
        Ok(snapshot) => Response::ok(
            id,
            serde_json::to_value(snapshot).expect("messages snapshot serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn messages_follow(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: MessagesFollowParams = match decode_params(&id, params, "messages.follow params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    if params.limit == 0 || params.limit > MAX_FOLLOW_MESSAGES {
        return Response::err(
            id,
            "bad_request",
            format!("limit must be between 1 and {MAX_FOLLOW_MESSAGES}"),
        );
    }
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match messaging.messages_follow(caller.key, params.after_seq, params.limit) {
        Ok(page) => Response::ok(
            id,
            serde_json::to_value(page).expect("messages follow page serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn msg_requeue(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: RequeueParams = match decode_params(&id, params, "msg.requeue params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    if !caller.key.is_admin() {
        return wire_error_response(id, mailbox_admin_required());
    }
    let requeued = match messaging.requeue(params.message_id.clone()) {
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

fn notification_withdraw(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: NotificationWithdrawParams =
        match decode_params(&id, params, "notification.withdraw params") {
            Ok(params) => params,
            Err(response) => return response,
        };
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    if !caller.key.is_admin() {
        return wire_error_response(id, mailbox_admin_required());
    }
    match messaging.withdraw_notification(caller.key, params.recipient, params.attempt_id) {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("notification withdrawal result serializes"),
        ),
        Err(error) => wire_error_response(id, mailbox_service_error(error)),
    }
}

fn alarm_preview(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: AlarmPreviewParams = match decode_params(&id, params, "alarm.preview params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match messaging.alarm_preview(caller.key, params.older_than_ms, unix_ms()) {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("alarm preview result serializes"),
        ),
        Err(error) => wire_error_response(id, messaging_attention_error(error)),
    }
}

fn alarm_clear(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: AlarmClearParams = match decode_params(&id, params, "alarm.clear params") {
        Ok(params) => params,
        Err(response) => return response,
    };
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
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match messaging.clear_alarms(caller.key, &attempts, params.cutoff_ms) {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("alarm clear result serializes"),
        ),
        Err(error) => wire_error_response(id, messaging_attention_error(error)),
    }
}

async fn attention_show(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: AttentionShowParams = match decode_params(&id, params, "attention.show params") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    attention_show_for_caller(inner, id, params, &messaging, caller.key).await
}

async fn attention_show_for_caller(
    inner: &Arc<Inner>,
    id: Value,
    params: AttentionShowParams,
    messaging: &crate::messaging::WorkspaceMessaging,
    caller: RecipientKey,
) -> Response {
    // Diff mode returns the exact payload selected at the write boundary.
    // Direct compatibility attempts can therefore include message content,
    // which is why only the administrator and the attempt's own recipient,
    // who may already claim that body, can ask. Neither diff input is
    // logged or stored.
    match crate::attention_resolution::show(inner, messaging, caller, &params.id, params.diff).await
    {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("attention show result serializes"),
        ),
        Err(error) => wire_error_response(id, messaging_attention_error(error)),
    }
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
    let (messaging, caller) = match workspace_messaging_caller(inner, peer) {
        Ok(caller) => caller,
        Err(error) => return wire_error_response(id, error),
    };
    match crate::attention_resolution::resolve(
        inner, &messaging, caller.key, &params.id, resolution,
    )
    .await
    {
        Ok(result) => Response::ok(
            id,
            serde_json::to_value(result).expect("attention resolution result serializes"),
        ),
        Err(error) => wire_error_response(id, attention_resolve_error(error)),
    }
}

fn messaging_attention_error(error: crate::messaging::MessagingAttentionError) -> WireError {
    match error {
        crate::messaging::MessagingAttentionError::Denied => mailbox_admin_required(),
        crate::messaging::MessagingAttentionError::Ambiguous {
            message,
            candidates,
        } => WireError {
            code: "ambiguous_attention".to_string(),
            message,
            data: Some(json!({
                "candidates": candidates.iter().map(ToString::to_string).collect::<Vec<_>>()
            })),
        },
        crate::messaging::MessagingAttentionError::Mailbox(error) => mailbox_service_error(error),
    }
}

fn attention_action_error(error: crate::attention_resolution::AttentionActionError) -> WireError {
    use crate::attention_resolution::AttentionActionError;

    match error {
        AttentionActionError::Store(error) => mailbox_service_error(error),
        AttentionActionError::ResolutionInProgress => WireError {
            code: "conflict".to_string(),
            message: error.to_string(),
            data: None,
        },
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
        AttentionActionError::ForceRefused(_) => WireError {
            code: "force_submit_refused".to_string(),
            message: error.to_string(),
            data: None,
        },
    }
}

fn attention_resolve_error(error: crate::attention_resolution::AttentionResolveError) -> WireError {
    match error {
        crate::attention_resolution::AttentionResolveError::Selection(error) => {
            messaging_attention_error(error)
        }
        crate::attention_resolution::AttentionResolveError::Action(error) => {
            attention_action_error(error)
        }
    }
}

fn require_workspace_messaging_admin(inner: &Arc<Inner>, peer: Peer) -> Result<(), WireError> {
    let (_, identity) = workspace_messaging_caller(inner, peer)?;
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

fn resolve_mailbox_identity(
    inner: &Arc<Inner>,
    (uid, pid): (u32, i32),
    admin: crate::mailbox::MailboxIdentity,
    identity_for_recipient: impl FnOnce(
        RecipientKey,
    ) -> Result<
        Option<crate::mailbox::MailboxIdentity>,
        crate::mailbox::MailboxServiceError,
    >,
) -> Result<crate::mailbox::MailboxIdentity, WireError> {
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
        identity::PeerOrigin::Admin => admin,
        identity::PeerOrigin::Pane {
            pane_id,
            pane_root,
            vendor_below,
            ..
        } => {
            // A shell is the local operator unless its ancestry crosses an
            // agent vendor. Labels are mutable display data and cannot grant
            // or revoke administrative authority.
            if !vendor_below {
                return Ok(admin);
            }
            let Some(route) = report_pane_at(&panes, &pane_id, pane_root) else {
                // An unconfigured vendor cannot acquire administrative
                // authority by running a Cyclops command.
                return Err(mailbox_origin_denied());
            };
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
            identity_for_recipient(route.recipient_key)
                .map_err(mailbox_service_error)?
                .ok_or_else(mailbox_origin_denied)?
        }
        identity::PeerOrigin::Unprovable => return Err(mailbox_origin_denied()),
    };
    Ok(caller)
}

fn workspace_messaging_caller(
    inner: &Arc<Inner>,
    peer: Peer,
) -> Result<
    (
        Arc<crate::messaging::WorkspaceMessaging>,
        crate::mailbox::MailboxIdentity,
    ),
    WireError,
> {
    let credentials = daemon_peer(peer)?;
    let messaging = inner.workspace_messaging().ok_or_else(|| WireError {
        code: "mailbox_unavailable".to_string(),
        message: "durable workspace identity is not connected".to_string(),
        data: None,
    })?;
    let caller = resolve_workspace_messaging_caller(inner, &messaging, credentials)?;
    Ok((messaging, caller))
}

pub(crate) fn workspace_messaging_caller_if_available(
    inner: &Arc<Inner>,
    peer: Peer,
) -> Result<
    Option<(
        Arc<crate::messaging::WorkspaceMessaging>,
        crate::mailbox::MailboxIdentity,
    )>,
    WireError,
> {
    let Some(messaging) = inner.workspace_messaging() else {
        return Ok(None);
    };
    let credentials = daemon_peer(peer)?;
    let caller = resolve_workspace_messaging_caller(inner, &messaging, credentials)?;
    Ok(Some((messaging, caller)))
}

fn resolve_workspace_messaging_caller(
    inner: &Arc<Inner>,
    messaging: &crate::messaging::WorkspaceMessaging,
    credentials: (u32, i32),
) -> Result<crate::mailbox::MailboxIdentity, WireError> {
    let caller = messaging.with_published(|messaging| {
        resolve_mailbox_identity(
            inner,
            credentials,
            messaging.admin_identity(),
            |recipient| messaging.identity_for_recipient(recipient),
        )
    })?;
    Ok(caller)
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
            vendor_below: false,
            ..
        } => Ok(service.admin()),
        identity::PeerOrigin::Pane {
            pane_id,
            pane_root,
            vendor_below: true,
            ..
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
        MailboxServiceError::NotificationSchedule(detail) => {
            ("notification_schedule_failed", detail)
        }
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
                MailboxError::DraftEmptyRecipients | MailboxError::Type(_) => {
                    ("bad_request", error.to_string())
                }
                MailboxError::ReplyNotVisible { .. } | MailboxError::ClaimantMismatch { .. } => {
                    ("denied", error.to_string())
                }
                MailboxError::NotificationAttemptUnknown(_) => ("no_such_alarm", error.to_string()),
                MailboxError::DuplicateIdempotencyKey { .. }
                | MailboxError::AlreadyClaimed { .. }
                | MailboxError::NotificationAttemptMismatch { .. }
                | MailboxError::NotificationAttemptClaimLocatorConflict(_)
                | MailboxError::NotificationClearRequiresAttention
                | MailboxError::NotificationRequeueRequiresAttention
                | MailboxError::NotificationRequeueBarrierBindingIncomplete(_)
                | MailboxError::NotificationRequeueExactComposerBarrier(_)
                | MailboxError::NotificationWithdrawalRequiresPreWrite
                | MailboxError::NotificationWithdrawalRecipientMismatch { .. } => {
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
        .active_session_slots()
        .iter()
        .flat_map(|(_, slot)| {
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

fn refuse_incomplete_status(pane: &mut cyclops_proto::PaneStatus) {
    pane.manifest = None;
    pane.manifest_display_name = None;
    pane.state = cyclops_proto::AgentState::Unknown;
    pane.state_ms = None;
    pane.write_ready = false;
    pane.write_block = Some(STATUS_REFRESH_INCOMPLETE.to_string());
    pane.composer = cyclops_proto::ComposerState::ComposerAmbiguous;
    pane.composer_proof = cyclops_proto::ComposerProof::Unprovable;
    pane.composer_reason = Some(STATUS_REFRESH_INCOMPLETE.to_string());
    if pane.notification_attempt.is_some() || pane.composer_candidates > 0 {
        pane.next_action = Some(crate::messaging::operator_composer_next_action(
            pane.notification_state,
            pane.notification_attempt.is_some(),
        ));
    } else {
        pane.next_action = None;
    }
    pane.working_confirmed = None;
    pane.hooks_verified = None;
}

/// Reset and cleanse status: close dead tmux panes, prune stale unattached
/// runtime sessions, forget orphan adoptions, and refresh live observations.
pub(crate) async fn status_reset(
    inner: &Arc<Inner>,
    params: cyclops_proto::StatusResetParams,
) -> cyclops_proto::StatusResetResult {
    let mut pruned_panes = 0;
    let mut pruned_sessions = 0;
    let mut pruned_adoptions = 0;

    // 1. Kill dead panes in attached watchers
    if params.kill_dead_panes {
        let slots = inner.active_session_slots();
        for (_, slot) in &slots {
            let watcher = {
                let link = slot.link.lock().expect("session link lock");
                link.watcher.as_ref().map(Arc::clone)
            };
            if let Some(w) = watcher {
                let snapshot = w.snapshot();
                let dead_panes: Vec<String> = snapshot
                    .into_iter()
                    .filter(|p| p.dead)
                    .map(|p| p.pane_id)
                    .collect();
                for pane_id in &dead_panes {
                    if w.client().kill_pane(pane_id).await.is_ok() {
                        pruned_panes += 1;
                    }
                }
                if !dead_panes.is_empty() {
                    let _ = w.reconcile_now().await;
                }
            }
        }
    }

    // 2. Retire unattached / stale non-persistent sessions
    if params.prune_stale_sessions {
        let slots = inner.active_session_slots();
        for (_, slot) in &slots {
            if !slot.is_persistent() {
                let is_attached = {
                    let link = slot.link.lock().expect("session link lock");
                    link.attached
                };
                if !is_attached && slot.retire_runtime_slot() {
                    pruned_sessions += 1;
                }
            }
        }
    }

    // 3. Collect active pane ids across all live watchers
    let live_pane_ids: HashSet<String> = {
        let slots = inner.active_session_slots();
        let mut set = HashSet::new();
        for (_, slot) in &slots {
            let link = slot.link.lock().expect("session link lock");
            if let Some(w) = &link.watcher {
                for p in w.snapshot() {
                    if !p.dead {
                        set.insert(p.pane_id);
                    }
                }
            }
        }
        set
    };

    // 4. Prune orphan adoptions
    if params.prune_stale_adoptions {
        let mut reg = inner.registry.lock().expect("registry lock");
        if let Ok(count) = reg.prune_orphans(&live_pane_ids) {
            pruned_adoptions = count;
        }
    }

    // 5. Clean up detections cache for non-live panes
    {
        let mut detections = inner.detections.lock().expect("detections lock");
        detections.retain(|key, _| live_pane_ids.contains(&key.pane_id));
    }

    // 6. Refresh observations
    let _ = crate::refresh_status_observations(inner).await;

    let (active_sessions, active_panes) = {
        let slots = inner.active_session_slots();
        let s_count = slots.len();
        let mut p_count = 0;
        for (_, slot) in &slots {
            let link = slot.link.lock().expect("session link lock");
            if let Some(w) = &link.watcher {
                p_count += w.snapshot().iter().filter(|p| !p.dead).count();
            }
        }
        (s_count, p_count)
    };

    cyclops_proto::StatusResetResult {
        reset: true,
        pruned_panes,
        pruned_sessions,
        pruned_adoptions,
        active_panes,
        active_sessions,
    }
}

/// Assemble StatusResult from the session slots and the detection cache.
///
/// `open_deliveries` adds the ledger-folded backlog of deliveries still
/// waiting on a human. It is opt-in because it reads the session files,
/// and only a client reconciling attention at startup needs it.
pub(crate) fn status_result(inner: &Arc<Inner>, open_deliveries: bool) -> StatusResult {
    status_result_with_refresh(inner, open_deliveries, &HashSet::new())
}

fn status_result_with_refresh(
    inner: &Arc<Inner>,
    open_deliveries: bool,
    incomplete_refreshes: &HashSet<crate::PaneKey>,
) -> StatusResult {
    // The ledger fold happens before the state locks are taken: it reads
    // files, and the fusion engine wants those locks back promptly.
    // Two halves, kept apart: the legacy session-ledger fold, and the
    // durable WorkspaceMessaging status projection. The latter includes
    // the pre-write blocks that blocked_notifications below also details;
    // the renderer dedups that detailed row by attempt id so one attempt
    // prints once.
    let include_mailbox_attention = open_deliveries;
    let open_deliveries = if include_mailbox_attention {
        crate::history::open_deliveries(inner)
    } else {
        Vec::new()
    };
    let messaging_status = inner
        .workspace_messaging()
        .map(|messaging| {
            messaging.status_snapshot(
                include_mailbox_attention,
                unix_ms(),
                STATUS_BLOCKED_NOTIFICATION_LIMIT,
            )
        })
        .unwrap_or_default();
    let adoptions = inner
        .registry
        .lock()
        .expect("registry lock")
        .exact_adoptions();
    let mut diagnostics =
        crate::deadlock::status_diagnostics(messaging_status.deadlock_candidates());
    diagnostics.extend(inner.engine.notification_worker_diagnostics());
    // Status joins durable notification state below. Snapshot typed,
    // content-free fusion facts first so no journal read runs under the
    // observation cache lock and this adapter never learns its representation.
    let pane_observations = fusion::pane_status_observations(inner);
    let sessions = inner
        .active_session_slots()
        .into_iter()
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
                identity: link.identity.clone(),
                panes: rows
                    .iter()
                    .map(|r| {
                        let pane = crate::PaneKey::new(session_idx, &r.pane_id);
                        let refresh_incomplete = incomplete_refreshes.contains(&pane);
                        let recipient = instance_id.and_then(|instance_id| {
                            Some(RecipientKey::agent(
                                inner.workspace_id,
                                instance_id,
                                r.pane_id.parse().ok()?,
                            ))
                        });
                        let observed_root = identity::ProcId::of(r.pane_pid);
                        let pane_root = observed_root
                            .and_then(|root| ProcessInstanceId::new(root.pid, root.birth).ok());
                        let observation = pane_observations
                            .get(&pane)
                            .and_then(|observation| observation.for_pane_root(observed_root));
                        let observation = observation.as_ref();
                        let adoption = recipient.and_then(|recipient| {
                            adoptions.iter().find(|adoption| {
                                adoption.recipient == Some(recipient)
                                    && adoption.pane_root == pane_root
                            })
                        });
                        let mut ps = r.to_status(
                            adoption.map(|adoption| adoption.label.clone()),
                            observation.and_then(|e| e.manifest.clone()),
                            observation
                                .map(|e| e.state)
                                .unwrap_or(cyclops_proto::AgentState::Unknown),
                        );
                        // How long the pane has been in that state, from
                        // the change mark fusion keeps. The roster's
                        // elapsed column is this number and nothing else.
                        ps.state_ms = observation.map(|e| e.state_ms);
                        // The second answer, carried from the same stamp
                        // the gate obeys. A pane with no cached detection
                        // has nothing behind it, so it stays refused.
                        ps.write_ready = observation.is_some_and(|e| e.write_ready);
                        ps.write_block = observation.and_then(|e| e.write_block.clone());
                        // From the same cached verdict as the state itself,
                        // so the word and the reason for it can never come
                        // from two different moments.
                        ps.unknown_reason = observation.and_then(|e| e.unknown_reason.clone());
                        ps.working_confirmed = (ps.state == cyclops_proto::AgentState::Working)
                            .then_some(observation.is_some_and(|e| e.working_confirmed));
                        ps.unread =
                            recipient.and_then(|recipient| messaging_status.unread_for(recipient));
                        if let Some(observation) = observation {
                            ps.composer = observation.composer.state;
                            ps.composer_proof = observation.composer.proof;
                            ps.notification_attempt = observation.composer.notification_attempt;
                            ps.composer_reason = observation.composer.reason.clone();
                            ps.composer_candidates = observation.composer.candidate_count;
                        }
                        let binding = recipient.and_then(|recipient| {
                            observation?.composer.notification_evidence(recipient)
                        });
                        let composer_status = messaging_status.composer_status(
                            recipient,
                            crate::messaging::MessagingComposerObservation {
                                composer: ps.composer,
                                proof: ps.composer_proof,
                                reason: ps.composer_reason,
                                detected_attempt: ps.notification_attempt,
                                detected_candidate_count: ps.composer_candidates,
                                pane_root,
                                binding,
                            },
                        );
                        ps.composer = composer_status.composer;
                        ps.composer_proof = composer_status.proof;
                        ps.composer_reason = composer_status.reason;
                        ps.composer_candidates = composer_status.candidate_count;
                        ps.notification_attempt = composer_status.attempt;
                        ps.notification_state = composer_status.notification_state;
                        ps.message_state = composer_status.message_state;
                        ps.next_action = composer_status.next_action;
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
                        let bound = observation.and_then(|e| e.manifest.as_deref());
                        ps.hooks_verified = bound.and_then(|m| {
                            crate::selftest::hooks_verified_for(
                                inner,
                                &pane,
                                adoption.is_some(),
                                Some(m),
                                observation.and_then(|e| e.agent),
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
                        if refresh_incomplete {
                            refuse_incomplete_status(&mut ps);
                        }
                        ps
                    })
                    .collect(),
            }
        })
        .collect();
    StatusResult {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        daemon_build: Some(BUILD_REF.to_string()),
        daemon_process: daemon_process(),
        daemon_executable: daemon_executable(),
        proto: PROTOCOL_VERSION,
        boot_id: inner.boot_id.clone(),
        uptime_ms: inner.started.elapsed().as_millis() as u64,
        tmux_version: inner.tmux_version.clone(),
        workspace_id: Some(inner.workspace_id),
        sessions,
        mailbox_routes: messaging_status.mailbox_routes,
        admin_unread: messaging_status.admin_unread,
        open_deliveries,
        diagnostics,
        blocked_notifications: messaging_status.blocked_notifications,
        blocked_notifications_total: messaging_status.blocked_notifications_total,
        // Always answered, empty set included: "I loaded none" is the fact
        // a client needs to explain an unknown pane, and it is exactly the
        // fact an omitted field would hide.
        manifests: Some(cyclops_proto::Manifests {
            ids: inner.manifests.keys().cloned().collect(),
            dir: inner.manifest_dir.as_ref().map(|d| d.display().to_string()),
        }),
        // The one process that can say this without guessing.
        pid: Some(std::process::id()),
        mailbox_attention: messaging_status.mailbox_attention,
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
            let det = match crate::observe_pane(
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
/// same reason `emit_state` takes one. See its doc comment.
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
        .active_session_slots()
        .iter()
        .flat_map(|(_, slot)| {
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

struct BoundedJson {
    bytes: Vec<u8>,
    oversized: bool,
}

impl BoundedJson {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            oversized: false,
        }
    }
}

impl std::io::Write for BoundedJson {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if matches!(
            FrameContract::classify_json_bytes(self.bytes.len().saturating_add(buf.len())),
            FrameSize::TooLarge
        ) {
            self.oversized = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "official daemon frame is too large",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum FrameEncodeError {
    TooLarge,
    Serialize(serde_json::Error),
}

fn frame_too_large(subject: &str) -> String {
    format!(
        "{subject} exceeds the {}-byte JSON frame limit (newline excluded)",
        FrameContract::MAX_JSON_BYTES
    )
}

fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameEncodeError> {
    let mut writer = BoundedJson::new();
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.bytes),
        Err(_) if writer.oversized => Err(FrameEncodeError::TooLarge),
        Err(error) => Err(FrameEncodeError::Serialize(error)),
    }
}

/// Preserve the request id when possible, but never emit a response outside
/// the official envelope. A caller that receives this fallback knows the
/// original outcome is uncertain and must inspect authoritative state.
fn response_frame(response: &Response) -> Option<Vec<u8>> {
    match encode_frame(response) {
        Ok(frame) => Some(frame),
        Err(FrameEncodeError::TooLarge) => {
            let fallback = Response::err(
                response.id.clone(),
                FrameContract::TOO_LARGE_CODE,
                format!(
                    "{}; the request outcome is unknown, so inspect authoritative state before retrying",
                    frame_too_large("daemon response")
                ),
            );
            encode_frame(&fallback).ok()
        }
        Err(FrameEncodeError::Serialize(error)) => {
            warn!(error = %error, "response serialization failed; dropping connection");
            None
        }
    }
}

/// Read one newline-terminated frame without allocating beyond the shared
/// JSON-object envelope. The delimiter is consumed but is not counted.
async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed during a daemon frame",
                ))
            };
        }
        if let Some(delimiter) = available
            .iter()
            .position(|byte| *byte == FrameContract::DELIMITER)
        {
            if matches!(
                FrameContract::classify_json_bytes(bytes.len().saturating_add(delimiter)),
                FrameSize::TooLarge
            ) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    frame_too_large("client request"),
                ));
            }
            bytes.extend_from_slice(&available[..delimiter]);
            reader.consume(delimiter + 1);
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "client frame was not UTF-8",
                )
            });
        }
        if matches!(
            FrameContract::classify_json_bytes(bytes.len().saturating_add(available.len())),
            FrameSize::TooLarge
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                frame_too_large("client request"),
            ));
        }
        bytes.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

/// Write one pre-encoded frame with the write timeout. False means drop the
/// connection.
async fn write_frame(w: &mut OwnedWriteHalf, frame: &[u8]) -> bool {
    if matches!(
        FrameContract::classify_json_bytes(frame.len()),
        FrameSize::TooLarge
    ) {
        return false;
    }
    let mut bytes = Vec::with_capacity(frame.len() + 1);
    bytes.extend_from_slice(frame);
    bytes.push(FrameContract::DELIMITER);
    match tokio::time::timeout(WRITE_TIMEOUT, w.write_all(&bytes)).await {
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
async fn write_line(w: &mut OwnedWriteHalf, line: &str) -> bool {
    write_frame(w, line.as_bytes()).await
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
    use std::io::Cursor;
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn daemon_ingress_boundary_excludes_the_newline_and_requires_it() {
        let mut exact = vec![b'x'; FrameContract::MAX_JSON_BYTES];
        exact.push(FrameContract::DELIMITER);
        let mut reader = BufReader::new(Cursor::new(exact));
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap().len(),
            FrameContract::MAX_JSON_BYTES
        );

        let mut oversized = vec![b'x'; FrameContract::MAX_JSON_BYTES + 1];
        oversized.push(FrameContract::DELIMITER);
        let mut reader = BufReader::new(Cursor::new(oversized));
        assert_eq!(
            read_frame(&mut reader).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );

        let mut reader = BufReader::new(Cursor::new(b"{}"));
        assert_eq!(
            read_frame(&mut reader).await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn oversized_ingress_is_dropped_before_dispatch() {
        let mut inner = bare_inner();
        let (stop, stop_rx) = watch::channel(false);
        Arc::get_mut(&mut inner).unwrap().stop = stop_rx;
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_conn(inner, server));
        let mut client = BufReader::new(client);
        let mut hello = String::new();
        client.read_line(&mut hello).await.unwrap();
        assert!(!hello.is_empty());

        let mut request = serde_json::to_vec(&json!({
            "id": 1,
            "method": "ping",
            "params": {
                "padding": "x".repeat(cyclops_proto::FrameContract::MAX_JSON_BYTES)
            }
        }))
        .unwrap();
        request.push(b'\n');
        client.get_mut().write_all(&request).await.unwrap();

        let mut response = String::new();
        let read = tokio::time::timeout(Duration::from_secs(1), client.read_line(&mut response))
            .await
            .expect("the daemon must close an oversized frame")
            .unwrap();
        assert_eq!(read, 0, "oversized request reached dispatch: {response}");
        task.await.unwrap();
        drop(stop);
    }

    #[tokio::test]
    async fn oversized_subscription_event_drops_instead_of_emitting_a_partial_frame() {
        let mut inner = bare_inner();
        let (stop, stop_rx) = watch::channel(false);
        Arc::get_mut(&mut inner).unwrap().stop = stop_rx;
        let events = inner.events.clone();
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_conn(inner, server));
        let mut client = BufReader::new(client);

        let mut hello = String::new();
        client.read_line(&mut hello).await.unwrap();
        client
            .get_mut()
            .write_all(b"{\"id\":1,\"method\":\"events.subscribe\",\"params\":{}}\n")
            .await
            .unwrap();
        let mut acknowledgement = String::new();
        client.read_line(&mut acknowledgement).await.unwrap();
        assert!(acknowledgement.contains("subscribed"));

        events
            .send(Event {
                event: "test.oversized".into(),
                data: json!({"padding": "x".repeat(FrameContract::MAX_JSON_BYTES)}),
                seq: None,
            })
            .unwrap();
        let mut event = String::new();
        let read = tokio::time::timeout(Duration::from_secs(1), client.read_line(&mut event))
            .await
            .expect("the daemon must close after refusing oversized event egress")
            .unwrap();
        assert_eq!(read, 0, "daemon emitted event bytes: {event}");
        task.await.unwrap();
        drop(stop);
    }

    #[tokio::test]
    async fn oversized_egress_is_never_written() {
        use tokio::io::AsyncReadExt as _;

        let (server, mut client) = UnixStream::pair().unwrap();
        let (_, mut writer) = server.into_split();
        let reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let oversized = "x".repeat(cyclops_proto::FrameContract::MAX_JSON_BYTES + 1);

        assert!(!write_line(&mut writer, &oversized).await);
        drop(writer);
        assert!(reader.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn exact_egress_boundary_writes_the_delimiter_outside_the_json_count() {
        use tokio::io::AsyncReadExt as _;

        let (server, mut client) = UnixStream::pair().unwrap();
        let (_, mut writer) = server.into_split();
        let reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        let exact = "x".repeat(FrameContract::MAX_JSON_BYTES);

        assert!(write_line(&mut writer, &exact).await);
        drop(writer);
        let bytes = reader.await.unwrap();
        assert_eq!(bytes.len(), FrameContract::max_line_bytes());
        assert_eq!(bytes.last(), Some(&FrameContract::DELIMITER));
    }

    #[test]
    fn oversized_response_becomes_a_bounded_uncertainty_error() {
        let response = Response::ok(
            json!(7),
            json!({"padding": "x".repeat(FrameContract::MAX_JSON_BYTES)}),
        );

        let frame = response_frame(&response).expect("a bounded fallback must fit");
        assert!(frame.len() <= FrameContract::MAX_JSON_BYTES);
        let fallback: Response = serde_json::from_slice(&frame).unwrap();
        assert_eq!(fallback.id, json!(7));
        let error = fallback.error.expect("the fallback is a wire error");
        assert_eq!(error.code, FrameContract::TOO_LARGE_CODE);
        assert!(error.message.contains("outcome is unknown"));
        assert!(fallback.result.is_none());
    }

    #[test]
    fn incomplete_refresh_refuses_live_claims_but_keeps_durable_identity() {
        let row = cyclops_tmux::PaneRow {
            pane_id: "%1".into(),
            window_id: "@1".into(),
            window_name: "main".into(),
            title: String::new(),
            dead: false,
            in_mode: false,
            current_command: "claude".into(),
            width: 120,
            height: 40,
            active: true,
            pane_pid: 42,
        };
        let attempt = NotificationAttemptId::generate();
        let mut pane = row.to_status(
            Some("reviewer".into()),
            Some("claude".into()),
            cyclops_proto::AgentState::Working,
        );
        pane.write_ready = true;
        pane.composer = cyclops_proto::ComposerState::CyclopsNotificationStaged;
        pane.composer_proof = cyclops_proto::ComposerProof::ExactNotification;
        pane.notification_attempt = Some(attempt);
        pane.composer_candidates = 1;
        pane.notification_state = Some(cyclops_proto::NotificationState::Staged);
        pane.message_state = Some(cyclops_proto::ComposerMessageState::Pending);
        pane.next_action = Some(cyclops_proto::ComposerNextAction::AutomaticSubmit);
        pane.working_confirmed = Some(true);

        refuse_incomplete_status(&mut pane);

        assert_eq!(pane.state, cyclops_proto::AgentState::Unknown);
        assert!(!pane.write_ready);
        assert_eq!(pane.write_block.as_deref(), Some(STATUS_REFRESH_INCOMPLETE));
        assert_eq!(
            pane.composer,
            cyclops_proto::ComposerState::ComposerAmbiguous
        );
        assert_eq!(
            pane.composer_proof,
            cyclops_proto::ComposerProof::Unprovable
        );
        assert_eq!(
            pane.composer_reason.as_deref(),
            Some(STATUS_REFRESH_INCOMPLETE)
        );
        assert_eq!(pane.notification_attempt, Some(attempt));
        assert_eq!(pane.composer_candidates, 1);
        assert_eq!(
            pane.notification_state,
            Some(cyclops_proto::NotificationState::Staged)
        );
        assert_eq!(
            pane.message_state,
            Some(cyclops_proto::ComposerMessageState::Pending)
        );
        assert_eq!(
            pane.next_action,
            Some(cyclops_proto::ComposerNextAction::CheckHealth)
        );
        assert_eq!(pane.working_confirmed, None);
    }

    #[test]
    fn incomplete_refresh_keeps_ambiguous_candidate_count_and_action() {
        let row = cyclops_tmux::PaneRow {
            pane_id: "%1".into(),
            window_id: "@1".into(),
            window_name: "main".into(),
            title: String::new(),
            dead: false,
            in_mode: false,
            current_command: "claude".into(),
            width: 120,
            height: 40,
            active: true,
            pane_pid: 42,
        };
        let mut pane = row.to_status(
            Some("reviewer".into()),
            Some("claude".into()),
            cyclops_proto::AgentState::Idle,
        );
        pane.composer_candidates = 2;

        refuse_incomplete_status(&mut pane);

        assert_eq!(pane.notification_attempt, None);
        assert_eq!(pane.composer_candidates, 2);
        assert_eq!(
            pane.next_action,
            Some(cyclops_proto::ComposerNextAction::InspectMessages)
        );
    }

    #[tokio::test]
    async fn exact_shutdown_is_requested_only_after_the_response_is_written() {
        let inner = bare_inner();
        let process = daemon_process().expect("this process has a kernel generation");
        let request = serde_json::to_string(&Request {
            id: json!(7),
            method: "daemon.shutdown".into(),
            params: json!({
                "daemon_process": process,
                "boot_id": inner.boot_id,
                "timeout_ms": 0,
            }),
        })
        .unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let (_, mut writer) = server.into_split();
        let mut shutdown = inner.shutdown_request.subscribe();

        let outcome = handle_line(&inner, &request, None, &mut writer).await;
        assert!(matches!(outcome, LineOutcome::Drop));
        shutdown.changed().await.unwrap();
        assert!(*shutdown.borrow());

        let mut line = String::new();
        let mut reader = BufReader::new(client);
        reader.read_line(&mut line).await.unwrap();
        let response: Response = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response.result.unwrap()["stopping"], true);
    }

    #[tokio::test]
    async fn changed_process_generation_never_requests_shutdown() {
        let inner = bare_inner();
        let process = daemon_process().expect("this process has a kernel generation");
        let replacement = ProcessInstanceId::new(process.pid(), process.birth() + 1).unwrap();
        let request = serde_json::to_string(&Request {
            id: json!(8),
            method: "daemon.shutdown".into(),
            params: json!({
                "daemon_process": replacement,
                "boot_id": inner.boot_id,
                "timeout_ms": 0,
            }),
        })
        .unwrap();
        let (server, _client) = UnixStream::pair().unwrap();
        let (_, mut writer) = server.into_split();

        let outcome = handle_line(&inner, &request, None, &mut writer).await;
        assert!(matches!(outcome, LineOutcome::Continue));
        assert!(!*inner.shutdown_request.borrow());
    }

    #[test]
    fn a_nonpending_claim_has_a_recoverable_wire_code() {
        let error = mailbox_service_error(crate::mailbox::MailboxServiceError::from(
            crate::mailbox::MailboxError::MessageNotPending("m-old".parse().unwrap()),
        ));

        assert_eq!(error.code, "message_not_pending");
    }

    #[test]
    fn a_locator_collision_has_a_stable_wire_conflict_code() {
        let locator = cyclops_proto::MessageId::new("m-att_AAAAAAAAQACAAAAAAAAAAQ").unwrap();
        let error = mailbox_service_error(crate::mailbox::MailboxServiceError::from(
            crate::mailbox::MailboxError::NotificationAttemptClaimLocatorConflict(locator),
        ));

        assert_eq!(error.code, "conflict");
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_before_recursive_state_repair() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-stale-socket-repair");
        let _ = std::fs::remove_dir_all(&home);
        let state_root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        let socket_path = home.join(cyclops_proto::SOCK_NAME);
        let stale = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(stale);

        // A connectable socket is not the stale fixture this test intends.
        // Confirm the dropped listener is gone before testing classification.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match tokio::time::timeout(Duration::from_millis(50), UnixStream::connect(&socket_path))
                .await
            {
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => break,
                // Linux may report one reset while the dropped listener is
                // still retiring. Keep waiting for the refused state that
                // `bind_socket` classifies as safe to reclaim.
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                Ok(Ok(stream)) => drop(stream),
                Ok(Err(error)) => panic!("stale socket fixture failed: {error}"),
                Err(_) => {}
            }
            assert!(
                Instant::now() < deadline,
                "stale socket fixture remained connectable"
            );
            tokio::task::yield_now().await;
        }

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
            force_submit: crate::ForceSubmitRuntime::new(false, 5_000),
            state_root,
            durable_record_forget_lease: StdMutex::new(None),
            state_repair: cyclops_state::RepairSummary::default(),
            workspace_id,
            session_identities: StdMutex::new(session_identities),
            mailbox: None,
            workspace_messaging: std::sync::OnceLock::new(),
            composer_recovery: Arc::new(StdMutex::new(
                crate::composer_recovery::RecoveryCoordinator::default(),
            )),
            mailbox_publication: Arc::new(StdMutex::new(())),
            unread_projection_gate: tokio::sync::Mutex::new(()),
            unread_projection_pending: StdMutex::new(HashSet::new()),
            unread_projection_wake: tokio::sync::Notify::new(),
            unread_projection_stopping: std::sync::atomic::AtomicBool::new(false),
            unread_projection_pause: StdMutex::new(None),
            chrome_repaint_pause: StdMutex::new(None),
            mailbox_publish_pause: StdMutex::new(None),
            boot_id: "b-test".into(),
            started: Instant::now(),
            tmux_version: "3.6a".into(),
            manifests: BTreeMap::new(),
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            session_registration: StdMutex::new(()),
            events: broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::<crate::PaneKey, DetEntry>::new()),
            route_evidence_generations: StdMutex::new(HashMap::new()),
            pane_observation_runtime: crate::fusion::PaneObservationRuntime::new(),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
            hook_lifecycle: StdMutex::new(crate::hook_lifecycle::Store::new()),
            turn_ends: StdMutex::new(crate::turnkey::Ends::new()),
            argv_cache: StdMutex::new(HashMap::new()),
            engine: crate::delivery::Engine::new(),
            ack_state: crate::ack::AckState::new(),
            hook_liveness: crate::selftest::HookLiveness::new(),
            inject_pause: StdMutex::new(None),
            name_reconcile_pause: StdMutex::new(None),
            fail_chrome_restore: std::sync::atomic::AtomicBool::new(false),
            fail_next_final_binding_observation: std::sync::atomic::AtomicBool::new(false),
            fail_next_admitted_binding_observation: std::sync::atomic::AtomicBool::new(false),
            fail_pre_record_writing: std::sync::Mutex::new(None),
            workspace_ui: StdMutex::new(crate::workspace_ui::WorkspaceUiState::default()),
            shutdown_request: watch::channel(false).0,
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
                    summary: None,
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
            pane_root: Some(ProcessInstanceId::new(3999, 817_999).unwrap()),
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

    /// A daemon with one exact notification stopped before any pane write.
    /// Two pending messages to the workspace administrator, which is the
    /// identity the test process resolves to, so it can claim them.
    fn inner_with_two_pending_to_admin(
        tag: &str,
    ) -> (
        Arc<Inner>,
        std::path::PathBuf,
        cyclops_proto::MessageId,
        cyclops_proto::MessageId,
    ) {
        use cyclops_proto::{Kind, MessagePresentation, RecipientPresentation};

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
        let first = cyclops_proto::MessageId::new("m-first").unwrap();
        let second = cyclops_proto::MessageId::new("m-second").unwrap();
        for (message_id, subject) in [(&first, "First"), (&second, "Second")] {
            store
                .accept(
                    message_id.clone(),
                    crate::mailbox::MessageDraft {
                        kind: Kind::Msg,
                        sender: agent,
                        recipients: vec![admin],
                        subject: Some(subject.into()),
                        summary: None,
                        body: Some("Body".into()),
                        client_key: None,
                        supersedes: None,
                        presentation: MessagePresentation {
                            sender_label: "reviewer".into(),
                            recipient_labels: vec![RecipientPresentation {
                                recipient: admin,
                                label: "admin".into(),
                            }],
                        },
                    },
                )
                .unwrap();
        }
        let mut inner = bare_inner();
        let service =
            crate::mailbox::MailboxService::new_with_events(directory, store, inner.events.clone());
        Arc::get_mut(&mut inner).expect("sole owner").mailbox = Some(Arc::new(service));
        (inner, path, first, second)
    }

    /// F5: claiming by id around the oldest pending message is allowed and
    /// must say so, because the skipped message still holds the FIFO head.
    #[tokio::test]
    async fn claim_by_id_names_the_oldest_pending_it_skipped() {
        let (inner, path, first, second) =
            inner_with_two_pending_to_admin("cyc-claim-names-skipped-oldest");

        let response = ask_inner(&inner, "inbox.claim", json!({"message_id": second})).await;
        let claimed: cyclops_proto::InboxClaimResult =
            serde_json::from_value(response.result.expect("second claim succeeds")).unwrap();
        assert_eq!(claimed.message.message_id, second);
        assert_eq!(claimed.skipped_oldest, Some(first.clone()));

        // A repeat claim is not a fresh skip.
        let response = ask_inner(&inner, "inbox.claim", json!({"message_id": second})).await;
        let repeated: cyclops_proto::InboxClaimResult =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(repeated.skipped_oldest, None);

        // Claiming the oldest skips nothing, and the field is absent on the wire.
        let response = ask_inner(&inner, "inbox.claim", json!({"message_id": first})).await;
        let raw = response.result.unwrap();
        assert!(raw.get("skipped_oldest").is_none(), "{raw}");
        let oldest: cyclops_proto::InboxClaimResult = serde_json::from_value(raw).unwrap();
        assert_eq!(oldest.message.message_id, first);
        std::fs::remove_dir_all(path).unwrap();
    }

    fn inner_with_blocked_notification(
        tag: &str,
    ) -> (
        Arc<Inner>,
        std::path::PathBuf,
        NotificationAttemptId,
        RecipientKey,
        cyclops_proto::MessageId,
    ) {
        use cyclops_proto::{NotificationManifestId, NotificationPreWriteCause};

        let path = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&path);
        let root = cyclops_state::StateRoot::open_or_create(&path).unwrap();
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let session = SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002").unwrap();
        let recipient =
            RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
        let directory = crate::mailbox::MailboxDirectory::new(
            workspace,
            [crate::mailbox::MailboxIdentity {
                key: recipient,
                label: "reviewer".into(),
            }],
        )
        .unwrap();
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
        let accepted = service
            .send(
                service.admin(),
                crate::mailbox::MailboxSend {
                    addresses: vec!["reviewer".into()],
                    recipient_keys: None,
                    subject: "Blocked".into(),
                    summary: None,
                    body: "claimable".into(),
                    fyi: false,
                    client_key: None,
                    supersedes: None,
                },
            )
            .unwrap();
        let attempt = service
            .prepare_oldest_notification(recipient)
            .unwrap()
            .unwrap();
        let context = crate::notification_adapter::NotificationContext::new(
            service.store_handle(),
            accepted.message_id.clone(),
            recipient,
            attempt.attempt_id,
        );
        context.record_gating().unwrap();
        context
            .record_pre_write_block(
                NotificationPreWriteCause::BindingUnprovable,
                Some(cyclops_proto::NotificationPreWriteObservation {
                    pane_root: None,
                    selected_manifest: Some(NotificationManifestId::new("codex").unwrap()),
                    binding: None,
                    route_evidence: None,
                    pane_width: None,
                    required_pane_width: None,
                    write_block: None,
                }),
            )
            .unwrap();
        Arc::get_mut(&mut inner).expect("sole owner").mailbox = Some(Arc::new(service));
        (
            inner,
            path,
            attempt.attempt_id,
            recipient,
            accepted.message_id,
        )
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
    async fn exact_send_targets_are_current_unambiguous_and_caller_scoped() {
        let (inner, path, _, recipient, _) = inner_with_blocked_notification("exact-send-targets");
        let exact = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": [],
                "recipient_keys": [recipient],
                "expected_caller": RecipientKey::admin(recipient.workspace_id()),
                "subject": "Exact route",
                "body": "Body",
                "client_key": "exact-server-send"
            }),
        )
        .await;
        assert!(exact.error.is_none(), "{:?}", exact.error);
        let exact = exact.result.unwrap();
        assert_eq!(exact["deliveries"][0]["to"], "reviewer");

        let snapshot = ask_inner(&inner, "messages.snapshot", json!({"recent_settled": 20})).await;
        let snapshot: MessagesSnapshotResult =
            serde_json::from_value(snapshot.result.unwrap()).unwrap();
        assert_eq!(
            snapshot.caller,
            Some(RecipientKey::admin(snapshot.workspace_id))
        );
        let row = snapshot
            .rows
            .iter()
            .find(|row| row.subject.as_deref() == Some("Exact route"))
            .unwrap();
        assert_eq!(row.recipients[0].recipient, recipient);
        assert_eq!(row.recipients[0].label, "reviewer");

        let message_count = snapshot.counts.visible_messages;
        let changed_caller = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": [],
                "recipient_keys": [recipient],
                "expected_caller": recipient,
                "subject": "Wrong sender"
            }),
        )
        .await;
        assert_eq!(changed_caller.error.unwrap().code, "denied");

        let mixed = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": ["reviewer"],
                "recipient_keys": [recipient],
                "subject": "Mixed"
            }),
        )
        .await;
        assert_eq!(mixed.error.unwrap().code, "bad_request");

        let empty = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": [],
                "recipient_keys": [],
                "subject": "Empty"
            }),
        )
        .await;
        assert_eq!(empty.error.unwrap().code, "bad_request");

        let replacement = RecipientKey::agent(
            recipient.workspace_id(),
            SessionInstanceId::from_str("00000000-0000-0000-0000-000000000003").unwrap(),
            recipient.pane_id().unwrap(),
        );
        let stale = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": [],
                "recipient_keys": [replacement],
                "subject": "Stale"
            }),
        )
        .await;
        assert_eq!(stale.error.unwrap().code, "no_such_target");

        let routed_reply = ask_inner(
            &inner,
            "msg.send",
            json!({
                "to": [],
                "recipient_keys": [recipient],
                "subject": "Ignored",
                "body": "Reply",
                "reply_to": exact["msg_id"]
            }),
        )
        .await;
        assert_eq!(routed_reply.error.unwrap().code, "bad_request");

        let after = ask_inner(&inner, "messages.snapshot", json!({"recent_settled": 20})).await;
        let after: MessagesSnapshotResult = serde_json::from_value(after.result.unwrap()).unwrap();
        assert_eq!(after.counts.visible_messages, message_count);
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
                "wait": {"until": "turn_ended", "timeout_ms": 60_000}
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
    async fn attention_show_endpoint_admits_the_exact_recipient_without_leaking_other_ids() {
        let (inner, path, attempt_id, _) = inner_with_alarm(
            "cyc-attention-show-recipient",
            NotificationAttentionCause::VerifyFailed,
        );
        let workspace: cyclops_proto::WorkspaceId =
            "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: cyclops_proto::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient =
            RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%1").unwrap());
        let stranger = RecipientKey::agent(workspace, session, TmuxPaneId::from_str("%2").unwrap());
        let messaging = inner.workspace_messaging().unwrap();
        let params = AttentionShowParams {
            id: attempt_id.to_string(),
            diff: true,
        };

        let shown =
            attention_show_for_caller(&inner, json!(1), params.clone(), &messaging, recipient)
                .await;
        assert!(shown.error.is_none(), "{:?}", shown.error);
        let shown: cyclops_proto::AttentionShowResult =
            serde_json::from_value(shown.result.unwrap()).unwrap();
        assert_eq!(shown.attempt_id, attempt_id);

        let denied =
            attention_show_for_caller(&inner, json!(2), params, &messaging, stranger).await;
        assert_eq!(denied.error.unwrap().code, "denied");

        let hidden = attention_show_for_caller(
            &inner,
            json!(3),
            AttentionShowParams {
                id: "att-00000000-0000-4000-8000-000000000099".into(),
                diff: true,
            },
            &messaging,
            stranger,
        )
        .await;
        assert_eq!(hidden.error.unwrap().code, "denied");
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Item 5: the status eye counts durable mailbox attention through a
    /// mailbox half kept apart from the legacy ledger fold: one row per
    /// attempt, exact recipient and attempt id on the row, stable across
    /// calls, and the same rows a messages.snapshot carries.
    #[test]
    fn status_mailbox_attention_folds_each_attempt_once() {
        let (inner, path, attempt_id, message_id) = inner_with_alarm(
            "cyc-status-eye-mailbox-attention",
            NotificationAttentionCause::VerifyFailed,
        );

        let first = status_result(&inner, true);
        assert!(
            first.open_deliveries.is_empty(),
            "the legacy half carries no mailbox rows: {:?}",
            first.open_deliveries
        );
        let rows: Vec<_> = first
            .mailbox_attention
            .iter()
            .filter(|row| row.id == message_id)
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "one row per attempt: {:?}",
            first.mailbox_attention
        );
        let row = rows[0];
        assert_eq!(row.to, "reviewer");
        assert_eq!(row.state, cyclops_proto::DeliveryState::AttentionRequired);
        assert_eq!(row.cause.as_deref(), Some("verify_failed"));
        assert_eq!(row.attempt_id, Some(attempt_id));
        assert!(row.recipient.is_some());

        let attention = cyclops_proto::Attention::from_status(&first);
        assert_eq!(
            attention.count(),
            1,
            "the eye counts the mailbox attempt once"
        );

        let second = status_result(&inner, true);
        assert_eq!(
            second.mailbox_attention.len(),
            first.mailbox_attention.len()
        );
        assert!(status_result(&inner, false).mailbox_attention.is_empty());

        // The snapshot carries the same rows, stamped by its own seq.
        let service = inner.mailbox.as_ref().expect("mailbox");
        let snapshot = service
            .messages_snapshot(service.admin().key, 0)
            .expect("snapshot");
        assert_eq!(
            snapshot
                .mailbox_attention
                .iter()
                .map(|row| (row.id.clone(), row.attempt_id))
                .collect::<Vec<_>>(),
            first
                .mailbox_attention
                .iter()
                .map(|row| (row.id.clone(), row.attempt_id))
                .collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Item 5: a queue head blocked before write is mailbox attention (the
    /// eye counts this array and a snapshot has no other), with the real
    /// state named in the cause, and is also detailed under
    /// blocked_notifications; the surface that prints both dedups the
    /// detailed row by attempt id.
    #[test]
    fn a_pre_write_blocked_head_is_mailbox_attention() {
        let (inner, path, attempt_id, recipient, message_id) =
            inner_with_blocked_notification("cyc-status-eye-pre-write-head");
        let res = status_result(&inner, true);
        let rows: Vec<_> = res
            .mailbox_attention
            .iter()
            .filter(|row| row.id == message_id.to_string())
            .collect();
        assert_eq!(rows.len(), 1, "{:?}", res.mailbox_attention);
        let row = rows[0];
        assert_eq!(row.recipient, Some(recipient));
        assert_eq!(row.attempt_id, Some(attempt_id));
        assert_eq!(row.state, cyclops_proto::DeliveryState::AttentionRequired);
        assert_eq!(
            row.cause
                .as_deref()
                .and_then(cyclops_proto::delivery_pre_write_reason),
            Some(cyclops_proto::NotificationPreWriteCause::BindingUnprovable.wire_name()),
            "cause must name the real state"
        );
        assert!(
            res.blocked_notifications
                .iter()
                .any(|entry| entry.notification_attempt == attempt_id),
            "the detailed row is still served: {:?}",
            res.blocked_notifications
        );
        assert_eq!(cyclops_proto::Attention::from_status(&res).count(), 1);
        std::fs::remove_dir_all(path).unwrap();
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
        assert!(error.message.contains("no second key"));
        assert!(error.message.contains("required durable evidence"));
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
    async fn clear_returns_only_the_exact_body_free_attempt_facts() {
        let (inner, path, attempt_id, message_id) = inner_with_alarm(
            "operator-clear-summary",
            cyclops_proto::NotificationAttentionCause::VerifyFailed,
        );
        let response = ask_inner(
            &inner,
            "alarm.clear",
            json!({"ids": [attempt_id.to_string()]}),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let value = response.result.unwrap();
        assert_eq!(value["cleared_ids"], json!([attempt_id.to_string()]));
        let summaries = value["summaries"].as_array().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0]["id"], attempt_id.to_string());
        assert_eq!(summaries[0]["message_id"], message_id);
        assert_eq!(summaries[0]["cause"], "verify_failed");
        assert!(summaries[0].get("subject").is_none());
        assert!(summaries[0].get("body").is_none());
        std::fs::remove_dir_all(path).ok();
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

    #[tokio::test]
    async fn external_admin_withdraws_one_exact_blocked_notification() {
        let (inner, path, attempt_id, recipient, message_id) =
            inner_with_blocked_notification("operator-withdraw-server");
        let params = json!({
            "attempt_id": attempt_id,
            "recipient": recipient,
        });
        let response = ask_inner(&inner, "notification.withdraw", params.clone()).await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let result: cyclops_proto::NotificationWithdrawResult =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.attempt_id, attempt_id);
        assert_eq!(result.message_id, message_id);
        assert_eq!(result.recipient, recipient);
        assert_eq!(
            result.disposition,
            cyclops_proto::NotificationWithdrawDisposition::Withdrawn
        );

        let repeated = ask_inner(&inner, "notification.withdraw", params).await;
        let repeated: cyclops_proto::NotificationWithdrawResult =
            serde_json::from_value(repeated.result.unwrap()).unwrap();
        assert_eq!(
            repeated.disposition,
            cyclops_proto::NotificationWithdrawDisposition::AlreadyWithdrawn
        );
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn status_exposes_one_body_free_blocked_wake_and_its_exact_action() {
        let (inner, path, attempt_id, recipient, message_id) =
            inner_with_blocked_notification("status-blocked-notification");

        let status = status_result(&inner, false);
        assert_eq!(status.blocked_notifications.len(), 1);
        assert_eq!(status.blocked_notifications_total, 1);
        let blocked = &status.blocked_notifications[0];
        assert_eq!(blocked.message_id, message_id);
        assert_eq!(blocked.notification_attempt, attempt_id);
        assert_eq!(blocked.recipient.recipient, recipient);
        assert_eq!(
            blocked.recipient.notification.pre_write_cause,
            Some(cyclops_proto::NotificationPreWriteCause::BindingUnprovable)
        );
        assert_eq!(
            blocked.next_action,
            Some(cyclops_proto::StatusNextAction::WithdrawNotification)
        );
        assert!(blocked.recipient.can_withdraw_notification);
        assert_eq!(blocked.recipient.fifo_position, Some(1));

        let encoded = serde_json::to_string(&status).unwrap();
        assert!(
            !encoded.contains("claimable"),
            "message body leaked: {encoded}"
        );

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
    fn optional_workspace_messaging_caller_waits_for_route_publication() {
        let (inner, path) = inner_with_mailbox("workspace-messaging-publication");
        let peer = own_peer();
        let messaging = inner.workspace_messaging().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let reader = messaging.with_published(|_| {
                let inner = Arc::clone(&inner);
                let reader = scope.spawn(move || {
                    started_tx.send(()).unwrap();
                    let _ = workspace_messaging_caller_if_available(&inner, peer);
                    done_tx.send(()).unwrap();
                });
                started_rx.recv().unwrap();
                let overlapped = done_rx
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .is_ok();
                assert!(!overlapped, "mailbox caller observed a partial publication");
                reader
            });
            reader.join().unwrap();
        });
        std::fs::remove_dir_all(path).ok();
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
        let binding = SessionIdentityBinding::new(
            LiveSessionKey::new(
                workspace,
                OsBootId::new("boot-test").unwrap(),
                ProcessInstanceId::new(900, 1000).unwrap(),
                TmuxSessionId::from_str("$1").unwrap(),
            ),
            session,
        );
        slot.link.lock().unwrap().identity = Some(binding.clone());
        inner.sessions.lock().unwrap().push(Arc::clone(&slot));
        let status = status_result(&inner, false);
        assert_eq!(status.workspace_id, Some(workspace));
        assert_eq!(status.sessions[0].identity.as_ref(), Some(&binding));
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
                    label: Some("operator-shell".into()),
                    pane_root: root_process,
                    vendor_below: false,
                },
            )
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
                    vendor_below: true,
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
                vendor_below: true,
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
                vendor_below: true,
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
                vendor_below: true,
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

        let (resp, _) = dispatch(&inner, req("health.snapshot"), own_peer()).await;
        let result = resp.result.expect("health snapshot answers");
        assert_eq!(result["boot_id"], "b-test");
        assert_eq!(result["sessions"], json!([]));
        assert!(result.get("open_deliveries").is_none());
    }

    #[test]
    fn health_snapshot_dispatch_cannot_enter_the_refresh_path() {
        let source = include_str!("server.rs");
        let arm = source
            .split_once("        \"health.snapshot\" => {")
            .expect("health snapshot dispatch arm")
            .1
            .split_once("        \"pane.read\" =>")
            .expect("next dispatch arm")
            .0;

        assert!(arm.contains("status_result(inner, false)"));
        for forbidden in [
            "refresh_status_observations",
            "observe_pane",
            "apply_messaging_observation",
            "open_deliveries: true",
            "engine.wake",
            "events.send",
        ] {
            assert!(
                !arm.contains(forbidden),
                "health snapshot entered mutating path {forbidden}"
            );
        }
    }

    /// Syntactic boundary tripwire, not a proof of refresh semantics. The
    /// server may request one named runtime refresh, but session discovery,
    /// pane iteration, task ownership, and observation application belong to
    /// the daemon runtime operation.
    #[test]
    fn status_dispatch_delegates_runtime_observation_refresh() {
        fn keeps_runtime_boundary(arm: &str) -> bool {
            arm.contains("crate::refresh_status_observations(inner).await")
                && [
                    "active_session_slots(",
                    "watcher_of(",
                    "run_status_refresh_jobs(",
                    "JoinSet",
                    "observe_pane(",
                ]
                .iter()
                .all(|forbidden| !arm.contains(forbidden))
        }

        let source = include_str!("server.rs");
        let arm = source
            .split_once("        \"status\" => {")
            .expect("status dispatch arm")
            .1
            .split_once("        \"health.snapshot\" =>")
            .expect("next dispatch arm")
            .0;

        assert!(keeps_runtime_boundary(arm));
        assert!(
            !keeps_runtime_boundary("let routes = inner.active_session_slots();"),
            "the tripwire must reject a direct runtime traversal"
        );
    }

    /// Syntactic architecture lint: these wire adapters may validate and
    /// serialize protocol values, but durable reads and mutations, locator
    /// interpretation, and post-commit work belong to WorkspaceMessaging.
    #[test]
    fn messaging_handlers_do_not_recover_mailbox_implementation_knowledge() {
        let source = include_str!("server.rs");
        for (name, next, authenticates) in [
            ("inbox_list", "inbox_claim", true),
            ("inbox_claim", "messages_snapshot", true),
            ("messages_snapshot", "messages_follow", true),
            ("messages_follow", "msg_requeue", true),
            ("msg_requeue", "notification_withdraw", true),
            ("notification_withdraw", "alarm_preview", true),
            ("alarm_preview", "alarm_clear", true),
            ("alarm_clear", "attention_show", true),
            ("attention_show", "attention_show_for_caller", true),
            ("attention_show_for_caller", "attention_resolve", false),
            ("attention_resolve", "messaging_attention_error", true),
        ] {
            let marker = format!("fn {name}(");
            let next_marker = format!("fn {next}(");
            let handler = source
                .split_once(&marker)
                .unwrap_or_else(|| panic!("handler {name}"))
                .1
                .split_once(&next_marker)
                .unwrap_or_else(|| panic!("handler after {name}"))
                .0;
            if authenticates {
                assert!(
                    handler.contains("workspace_messaging_caller"),
                    "{name} bypasses WorkspaceMessaging"
                );
            }
            for forbidden in [
                "mailbox_caller",
                "mailbox_publication",
                "MailboxService",
                "crate::mailbox::",
                "parse_notification_attempt_claim_locator",
                "schedule_recipient",
                "service.alarms_at_or_before",
                "service.clear_alarms",
                "service.attention_target",
                "alarm_summary",
            ] {
                assert!(
                    !handler.contains(forbidden),
                    "{name} recovered forbidden messaging knowledge: {forbidden}"
                );
            }
        }
    }

    #[test]
    fn force_submit_settings_authenticate_through_workspace_messaging() {
        let source = include_str!("server.rs");
        let settings = source
            .split_once("        \"notification.force_submit.get\" => {")
            .expect("force-submit get dispatch arm")
            .1
            .split_once("        method => {")
            .expect("dispatch fallback after force-submit settings")
            .0;
        assert_eq!(
            settings
                .matches("require_workspace_messaging_admin(inner, peer)")
                .count(),
            2
        );
        for forbidden in ["require_mailbox_admin", "mailbox_caller", "inner.mailbox"] {
            assert!(
                !settings.contains(forbidden),
                "force-submit settings recovered {forbidden}"
            );
        }
    }

    #[test]
    fn status_composition_does_not_recover_mailbox_projection_knowledge() {
        let source = include_str!("server.rs");
        let status = source
            .split_once("fn status_result_with_refresh(")
            .expect("status composition")
            .1
            .split_once("fn pane_read(")
            .expect("handler after status composition")
            .0;
        assert!(status.contains("messaging.status_snapshot("));
        assert!(status.contains("messaging_status.composer_status("));
        assert!(status.contains("fusion::pane_status_observations(inner)"));
        for forbidden in [
            "inner.mailbox",
            "inner.detections",
            "DetEntry",
            ".detection.",
            "ComposerProjection",
            "ActiveComposerNotification",
            "ExactOwnedRecoveryAction",
            "active_composer_notifications_snapshot",
            "composer_candidate_index",
            "composer_next_action",
            "notification_worker_owns",
            ".pending_count(",
            ".pending_recipients(",
            ".recipient_label(",
            ".identity_for_recipient(",
            ".routes(",
            ".mailbox_attention_rows(",
            ".blocked_notification_snapshot(",
        ] {
            assert!(
                !status.contains(forbidden),
                "status composition recovered mailbox knowledge through {forbidden}"
            );
        }
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

    #[tokio::test]
    async fn subscribe_cursor_is_compatibility_input_and_backfill_is_the_snapshot() {
        let inner = bare_inner();
        let (response, subscription) = dispatch(
            &inner,
            Request {
                id: json!("subscribe"),
                method: "events.subscribe".into(),
                params: json!({"cursor": 99}),
            },
            own_peer(),
        )
        .await;
        assert_eq!(response.result.unwrap()["subscribed"], true);
        assert_eq!(subscription.expect("subscription begins").cursor, Some(99));

        let (response, subscription) = dispatch(
            &inner,
            Request {
                id: json!("backfill"),
                method: "events.backfill".into(),
                params: json!({"limit": 20}),
            },
            own_peer(),
        )
        .await;
        assert!(subscription.is_none(), "a snapshot switched to push mode");
        let result: cyclops_proto::StreamBackfillResult =
            serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(result.lines.is_empty());
        assert_eq!(result.max_seq, None);
        assert_eq!(result.gap, None);
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
