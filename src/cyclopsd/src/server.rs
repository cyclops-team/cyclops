//! NDJSON socket server: hello line first, then a request loop per
//! connection, switching to event push after events.subscribe.
//!
//! Slow consumers never stall the daemon: every subscriber reads from its
//! own broadcast receiver, a lagged receiver is dropped with a warning,
//! and writes carry a timeout so a wedged client costs one connection.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use cyclops_proto::{
    AdminNotifyParams, AgentWaitParams, Event, Hello, MsgSendParams, PaneReadParams,
    PaneReadResult, PaneReadSource, PingResult, Request, Response, SessionStatus,
    StateReportParams, StatusParams, StatusResult, SubscribeParams, WireError, PROTOCOL_VERSION,
};
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
type Peer = Option<(u32, i32)>;

/// A write that does not finish inside this window means the client is
/// wedged; the connection is dropped rather than buffered without bound.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Scrollback lines for pane.read source=recent when the caller gave none.
const DEFAULT_RECENT_LINES: u32 = 200;

/// Protocol v1 methods that exist but land in a later milestone. One list,
/// so a new milestone replaces entries here instead of hunting through
/// dispatch. Empty as of M2 (msg.history/msg.thread landed).
const UNIMPLEMENTED: &[(&str, &str)] = &[];

/// Bind the daemon socket under `home`, creating the directory 0700.
///
/// Stale socket handling: if something answers at the path another daemon
/// is running and boot fails loudly; a refused connect means a leftover
/// file from a dead daemon, which is removed and rebound.
pub(crate) async fn bind_socket(home: &Path) -> anyhow::Result<UnixListener> {
    if !home.exists() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(home)
            .with_context(|| format!("create {}", home.display()))?;
    }
    let sock = home.join(cyclops_proto::SOCK_NAME);
    if sock.exists() {
        match UnixStream::connect(&sock).await {
            Ok(_) => anyhow::bail!("another cyclopsd is already running at {}", sock.display()),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                info!(socket = %sock.display(), "removing stale socket");
                std::fs::remove_file(&sock)
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
    UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))
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
    let peer: Peer = match identity::peer_of(&stream) {
        Ok((uid, pid)) => {
            debug!(uid, pid, "client connected");
            Some((uid, pid))
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
                given => match serde_json::from_value(given) {
                    Ok(p) => p,
                    Err(e) => {
                        return (
                            Response::err(id, "bad_request", format!("bad status params: {e}")),
                            None,
                        )
                    }
                },
            };
            let result = status_result(inner, params.open_deliveries);
            (
                Response::ok(id, serde_json::to_value(result).expect("status serializes")),
                None,
            )
        }
        "pane.read" => (pane_read(inner, id, req.params).await, None),
        "msg.send" => (msg_send(inner, id, req.params, peer).await, None),
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
            let params: cyclops_proto::HistoryParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Response::err(id, "bad_request", format!("bad msg.history params: {e}")),
                        None,
                    )
                }
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
            let params: cyclops_proto::ThreadParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Response::err(id, "bad_request", format!("bad msg.thread params: {e}")),
                        None,
                    )
                }
            };
            (
                from_result(id, crate::history::msg_thread(inner, &params.id)),
                None,
            )
        }
        "agent.state.report" => {
            let params: StateReportParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Response::err(id, "bad_request", format!("bad state report: {e}")),
                        None,
                    )
                }
            };
            // Hook reports feed liveness and tier-1 ACK evidence, so a
            // forged one lets the record lie. The socket path is pinned to
            // the reporting pane exactly like msg.send pins senders; only
            // the in-process Daemon::report_state path is pre-trusted.
            if let Err(e) = verify_report_origin(inner, peer, &params.agent) {
                return (
                    Response {
                        id,
                        result: None,
                        error: Some(e),
                    },
                    None,
                );
            }
            (
                from_result(id, ack::handle_report(inner, params).await),
                None,
            )
        }
        "admin.notify" => {
            let params: AdminNotifyParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Response::err(id, "bad_request", format!("bad admin.notify params: {e}")),
                        None,
                    )
                }
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
            let params: AgentWaitParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Response::err(id, "bad_request", format!("bad agent.wait params: {e}")),
                        None,
                    )
                }
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
            let params: cyclops_proto::HooksVerifyParams = match serde_json::from_value(req.params)
            {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Response::err(id, "bad_request", format!("bad hooks.verify params: {e}")),
                        None,
                    )
                }
            };
            (
                from_result(id, crate::selftest::verify(inner, params).await),
                None,
            )
        }
        "hooks.selftest" => {
            let params: cyclops_proto::HooksSelftestParams =
                match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return (
                            Response::err(
                                id,
                                "bad_request",
                                format!("bad hooks.selftest params: {e}"),
                            ),
                            None,
                        )
                    }
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
    let Some((uid, pid)) = peer else {
        return Err(deny("peer credentials unavailable".to_string()));
    };
    let daemon_uid = unsafe { libc::getuid() };
    if uid != daemon_uid {
        return Err(deny(format!("uid {uid} is not the daemon's user")));
    }
    Ok((uid, pid))
}

/// msg.send over the socket: resolve the sender from peer credentials
/// (fail-closed: no credentials or a foreign uid is denied, and nothing
/// in the request body can override the resolved sender), then hand off
/// to the delivery pipeline.
async fn msg_send(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: MsgSendParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "bad_request", format!("bad msg.send params: {e}")),
    };
    let (uid, pid) = match daemon_peer(peer) {
        Ok(v) => v,
        Err(e) => return Response::err(id, &e.code, e.message),
    };
    let panes = sender_panes(inner);
    let from = match identity::resolve_sender(uid, pid, &panes) {
        identity::Sender::Agent(label) => label,
        identity::Sender::Pane(pane_id) => pane_id,
        identity::Sender::Admin => "admin".to_string(),
    };
    from_result(id, delivery::msg_send(inner, &from, params).await)
}

/// (pane_id, label, pane_pid) rows for sender resolution, across every
/// attached session. Shared with msg.history's "me" resolution.
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

/// (pane_id, label, pane_pid) rows for hook-report origin resolution:
/// every live pane plus, for DETACHED sessions, the last-known pane table.
/// A hook report does not need the tmux connection (the pane and its
/// process tree outlive a control-mode outage), so the reporter's ancestry
/// must still be resolvable while the daemon is detached.
fn report_panes(inner: &Inner) -> Vec<(String, Option<String>, i32)> {
    let mut rows = sender_panes(inner);
    let labels = inner.labels();
    for slot in inner.session_slots() {
        if slot.link.lock().expect("session link lock").attached {
            continue;
        }
        let last = slot.last_panes.lock().expect("last panes lock");
        for row in last.values() {
            if rows.iter().any(|(id, _, _)| id == &row.pane_id) {
                continue;
            }
            rows.push((
                row.pane_id.clone(),
                labels.get(&row.pane_id).cloned(),
                row.pane_pid,
            ));
        }
    }
    rows
}

/// Fail-closed origin check for agent.state.report over the socket: the
/// peer's process ancestry must land in the very pane `agent` names (its
/// label or its pane id). Honest reports pass by construction, because
/// `cyclops hook` runs as a child of the vendor CLI inside the pane.
/// Everything else is denied and NOT ingested: a same-uid process outside
/// the pane (the admin shell included) could otherwise forge hook liveness
/// and tier-1 ACK evidence, and the record must never lie.
fn verify_report_origin(inner: &Inner, peer: Peer, agent: &str) -> Result<(), WireError> {
    let deny = |message: String| WireError {
        code: "denied".to_string(),
        message,
        data: None,
    };
    let (uid, pid) = daemon_peer(peer)?;
    let panes = report_panes(inner);
    let allowed = match identity::resolve_sender(uid, pid, &panes) {
        identity::Sender::Agent(label) => {
            // The report may name the pane by label or by pane id.
            agent == label
                || panes
                    .iter()
                    .any(|(pane_id, l, _)| l.as_deref() == Some(label.as_str()) && pane_id == agent)
        }
        identity::Sender::Pane(pane_id) => agent == pane_id,
        identity::Sender::Admin => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(deny(format!(
            "hook reports for {agent:?} are only accepted from a process inside that pane; \
             this peer is not (admin cannot post hook reports)"
        )))
    }
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
    let detections = inner.detections.lock().expect("detections lock");
    let labels = inner.labels();
    let sessions = inner
        .session_slots()
        .iter()
        .map(|slot| {
            let link = slot.link.lock().expect("session link lock");
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
                        let entry = detections.get(&r.pane_id);
                        let mut ps = r.to_status(
                            labels.get(&r.pane_id).cloned(),
                            entry.and_then(|e| e.manifest.clone()),
                            entry
                                .map(|e| e.detection.state)
                                .unwrap_or(cyclops_proto::AgentState::Unknown),
                        );
                        // How long the pane has been in that state, from
                        // the change mark fusion keeps. The roster's
                        // elapsed column is this number and nothing else.
                        ps.state_ms = entry.map(|e| e.since.elapsed().as_millis() as u64);
                        // Hook liveness (amendment c): adopted panes whose
                        // manifest declares hooks carry the verified bit,
                        // scoped to the current occupant (edges from a
                        // replaced occupant count for nothing).
                        ps.hooks_verified = crate::selftest::hooks_verified_for(
                            inner,
                            &r.pane_id,
                            labels.contains_key(&r.pane_id),
                            entry.and_then(|e| e.manifest.as_deref()),
                            r.pane_pid,
                        );
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
        open_deliveries,
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
    let params: PaneReadParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "bad_request", format!("bad pane.read params: {e}")),
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
    let wanted = inner.label_target(target);
    inner
        .session_slots()
        .iter()
        .enumerate()
        .find_map(|(idx, slot)| {
            let link = slot.link.lock().expect("session link lock");
            link.watcher
                .as_ref()
                .and_then(|w| w.pane(&wanted).map(|row| (idx, Arc::clone(w), row.pane_id)))
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
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    fn bare_inner() -> Arc<Inner> {
        let home = cyclops_proto::scratch::scratch_dir("cyc-unit");
        let (registry, _) = crate::registry::Registry::load(&home);
        Arc::new(Inner {
            cfg: Config::defaults(&home),
            boot_id: "b-test".into(),
            started: Instant::now(),
            tmux_version: "3.6a".into(),
            manifests: BTreeMap::new(),
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            events: broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::<String, DetEntry>::new()),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
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
        let path = dir.join("ledger/main.ndjson");
        std::fs::create_dir_all(path.parent().expect("ledger dir")).expect("scratch dir");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/history.ndjson"),
            &path,
        )
        .expect("fixture copies");
        let mut inner = bare_inner();
        Arc::get_mut(&mut inner)
            .expect("sole owner")
            .sessions
            .get_mut()
            .expect("sessions lock")
            .push(Arc::new(crate::SessionSlot::new(
                "main".into(),
                Arc::new(
                    cyclops_ledger::LedgerWriter::open(&path, "b-test").expect("ledger opens"),
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

    fn req(method: &str) -> Request {
        Request {
            id: json!(1),
            method: method.into(),
            params: json!({}),
        }
    }

    fn own_peer() -> Peer {
        Some((unsafe { libc::getuid() }, std::process::id() as i32))
    }

    /// Every protocol v1 method answers with something that is not
    /// unknown_method: implemented, unimplemented, or a param error.
    /// Every method protocol v1 answers. One list, read by the dispatch
    /// check below and by the page that documents the wire.
    const PROTOCOL_V1: [&str; 17] = [
        "ping",
        "status",
        "msg.send",
        "msg.history",
        "msg.thread",
        "agent.wait",
        "agent.state.report",
        "pane.read",
        "pane.label",
        "session.watch",
        "events.subscribe",
        "admin.notify",
        "hooks.verify",
        "hooks.selftest",
        "theme.reload",
        "workspace_ui.get",
        "workspace_ui.set",
    ];

    #[tokio::test]
    async fn dispatch_covers_protocol_v1() {
        let inner = bare_inner();
        for method in PROTOCOL_V1 {
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
        for method in PROTOCOL_V1 {
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

    #[tokio::test]
    async fn msg_send_denies_foreign_uid() {
        let inner = bare_inner();
        let foreign = unsafe { libc::getuid() }.wrapping_add(1);
        let (resp, _) = dispatch(
            &inner,
            Request {
                id: json!(9),
                method: "msg.send".into(),
                params: json!({"to": ["reviewer"], "subject": "hi"}),
            },
            Some((foreign, 1)),
        )
        .await;
        assert_eq!(resp.error.unwrap().code, "denied");
    }

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
