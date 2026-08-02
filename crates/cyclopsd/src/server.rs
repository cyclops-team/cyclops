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
    StateReportParams, StatusResult, SubscribeParams, PROTOCOL_VERSION,
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
/// so M2 replaces entries here instead of hunting through dispatch.
const UNIMPLEMENTED: &[(&str, &str)] = &[
    // The ledger already records everything these will read.
    ("msg.history", "M2"),
    ("msg.thread", "M2"),
];

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
            let result = status_result(inner);
            (
                Response::ok(id, serde_json::to_value(result).expect("status serializes")),
                None,
            )
        }
        "pane.read" => (pane_read(inner, id, req.params).await, None),
        "msg.send" => (msg_send(inner, id, req.params, peer).await, None),
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
            );
            (
                Response::ok(id, json!({"notified": true, "seq": seq})),
                None,
            )
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
            (
                from_result(id, crate::label_pane(inner, &target, label)),
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

/// msg.send over the socket: resolve the sender from peer credentials
/// (fail-closed: no credentials or a foreign uid is denied, and nothing
/// in the request body can override the resolved sender), then hand off
/// to the delivery pipeline.
async fn msg_send(inner: &Arc<Inner>, id: Value, params: Value, peer: Peer) -> Response {
    let params: MsgSendParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "bad_request", format!("bad msg.send params: {e}")),
    };
    let Some((uid, pid)) = peer else {
        return Response::err(id, "denied", "peer credentials unavailable");
    };
    let daemon_uid = unsafe { libc::getuid() };
    if uid != daemon_uid {
        return Response::err(id, "denied", format!("uid {uid} is not the daemon's user"));
    }
    let panes = sender_panes(inner);
    let from = match identity::resolve_sender(uid, pid, &panes) {
        identity::Sender::Agent(label) => label,
        identity::Sender::Pane(pane_id) => pane_id,
        identity::Sender::Admin => "admin".to_string(),
    };
    from_result(id, delivery::msg_send(inner, &from, params).await)
}

/// (pane_id, label, pane_pid) rows for sender resolution, across every
/// attached session.
fn sender_panes(inner: &Inner) -> Vec<(String, Option<String>, i32)> {
    let labels = inner.labels.lock().expect("labels lock");
    inner
        .sessions
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

/// Assemble StatusResult from the session slots and the detection cache.
pub(crate) fn status_result(inner: &Inner) -> StatusResult {
    let detections = inner.detections.lock().expect("detections lock");
    let labels = inner.labels.lock().expect("labels lock");
    let sessions = inner
        .sessions
        .iter()
        .map(|slot| {
            let link = slot.link.lock().expect("session link lock");
            let rows = link
                .watcher
                .as_ref()
                .map(|w| w.snapshot())
                .unwrap_or_default();
            SessionStatus {
                name: slot.name.clone(),
                attached: link.attached,
                panes: rows
                    .iter()
                    .map(|r| {
                        let entry = detections.get(&r.pane_id);
                        r.to_status(
                            labels.get(&r.pane_id).cloned(),
                            entry.and_then(|e| e.manifest.clone()),
                            entry
                                .map(|e| e.detection.state)
                                .unwrap_or(cyclops_proto::AgentState::Unknown),
                        )
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
    }
}

/// pane.read: resolve the target, then capture or return the detection
/// view. Targets are pane ids until the adoption registry lands (M1).
async fn pane_read(inner: &Arc<Inner>, id: Value, params: Value) -> Response {
    let params: PaneReadParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::err(id, "bad_request", format!("bad pane.read params: {e}")),
    };
    let Some((watcher, pane_id)) = resolve_target(inner, &params.target) else {
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
            match fusion::recompute_pane(inner, &watcher, &pane_id, true, "pane.read").await {
                Some(det) => ok_read(id, &params.target, &pane_id, None, Some(det)),
                None => Response::err(id, "no_such_target", "pane vanished during read"),
            }
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

/// Find the watcher owning a pane id. The session link lock is dropped
/// before any await; only the Arc leaves the closure.
fn resolve_target(inner: &Inner, target: &str) -> Option<(Arc<SessionWatcher>, String)> {
    inner.sessions.iter().find_map(|slot| {
        let link = slot.link.lock().expect("session link lock");
        link.watcher
            .as_ref()
            .and_then(|w| w.pane(target).map(|row| (Arc::clone(w), row.pane_id)))
    })
}

fn known_panes(inner: &Inner) -> Vec<String> {
    inner
        .sessions
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
        Arc::new(Inner {
            cfg: Config::defaults(Path::new("/private/tmp/cyc-unit")),
            boot_id: "b-test".into(),
            started: Instant::now(),
            tmux_version: "3.6a".into(),
            manifests: BTreeMap::new(),
            sessions: Vec::new(),
            events: broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::<String, DetEntry>::new()),
            labels: StdMutex::new(HashMap::new()),
            hook_readings: StdMutex::new(HashMap::new()),
            engine: crate::delivery::Engine::new(),
            ack_state: crate::ack::AckState::new(),
        })
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
    #[tokio::test]
    async fn dispatch_covers_protocol_v1() {
        let inner = bare_inner();
        let v1 = [
            "ping",
            "status",
            "msg.send",
            "msg.history",
            "msg.thread",
            "agent.wait",
            "agent.state.report",
            "pane.read",
            "pane.label",
            "events.subscribe",
            "admin.notify",
        ];
        for method in v1 {
            let (resp, _) = dispatch(&inner, req(method), own_peer()).await;
            if let Some(err) = &resp.error {
                assert_ne!(err.code, "unknown_method", "{method} fell through dispatch");
            }
        }
        let (resp, _) = dispatch(&inner, req("bogus.method"), own_peer()).await;
        assert_eq!(resp.error.unwrap().code, "unknown_method");
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
