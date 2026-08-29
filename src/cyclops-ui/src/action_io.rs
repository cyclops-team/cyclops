//! Detail reads and actions over the daemon socket.
//!
//! IO only. Nothing here decides whether an action is allowed; that is
//! [`crate::detail`]. This module builds params, sends one request on the
//! existing Unix socket, and reports what came back.
//!
//! Its own module so the action verbs stay clear of the snapshot and
//! event plumbing in [`crate::data`].
//!
//! The one distinction worth the code it costs: a daemon that answered
//! "no" is not the same as a socket that died. The first is final and the
//! operator should stop; the second may have landed, so a reply keeps its
//! draft and its idempotency key and can be sent again under the same key.

use std::path::Path;
use std::time::Duration;

use cyclops_proto::{MessageId, NotificationAttemptId};
use serde_json::{json, Value};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::detail::{Check, Loaded, ThreadEntry};
use crate::wire::{encode_json, FrameReader};

/// What a request is doing, which decides how its silence is read.
///
/// A read is not automatically harmless: an open that claims mutates a
/// mailbox. Carrying this with the request is what lets an unanswered
/// claiming open be recorded as unknown rather than as nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Read { claims: bool },
    Act(crate::detail::Action),
}

/// One in-flight request, named so its answer cannot land anywhere else.
///
/// The nonce is minted per request and never reused, so a response that
/// arrives after the reader closed one detail and opened another is
/// dropped rather than applied to a detail that never asked for it. The
/// target is carried too: two checks, because this one is about a body
/// reaching the wrong reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestToken {
    pub nonce: String,
    /// The frozen row AND the exact attempt, together.
    ///
    /// Both, because they answer different questions. The row says which
    /// detail may apply this answer and survives an alarm appearing or
    /// clearing underneath it. The attempt says what the request was
    /// actually about, and it is allowed to change: a requeue replaces
    /// it, so an answer carrying the old one must not be applied to a
    /// detail now looking at the new one.
    pub frozen: crate::queue::FrozenTarget,
    pub kind: RequestKind,
}

impl RequestToken {
    pub fn new(frozen: crate::queue::FrozenTarget, kind: RequestKind) -> RequestToken {
        RequestToken {
            nonce: uuid::Uuid::new_v4().to_string(),
            frozen,
            kind,
        }
    }

    /// The stable row this request belongs to.
    pub fn row(&self) -> &crate::queue::QueueTarget {
        &self.frozen.target
    }

    /// The exact attempt this request names, if any.
    pub fn attempt(&self) -> Option<cyclops_proto::NotificationAttemptId> {
        self.frozen.attempt
    }

    /// Did this request mutate anything, if it reached the daemon?
    pub fn mutates(&self) -> bool {
        match self.kind {
            RequestKind::Read { claims } => claims,
            RequestKind::Act(_) => true,
        }
    }

    pub fn action(&self) -> Option<crate::detail::Action> {
        match self.kind {
            RequestKind::Act(action) => Some(action),
            RequestKind::Read { .. } => None,
        }
    }
}

/// One thing to ask the daemon for an open detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRequest {
    /// Open a message. `claim` is true only for an inbound pending row:
    /// a claim is what authorizes the body, and claiming something you
    /// are only observing would take somebody else's mail.
    OpenMessage {
        message_id: MessageId,
        claim: bool,
    },
    /// Open an alarm. Always by attempt id: a message id resolves only
    /// when exactly one recipient is stuck, and a broadcast with two
    /// answers ambiguous_attention.
    OpenAttention {
        attempt_id: NotificationAttemptId,
    },
    Reply {
        message_id: MessageId,
        body: String,
        client_key: String,
    },
    WithdrawNotification {
        attempt_id: NotificationAttemptId,
        recipient: cyclops_proto::RecipientKey,
    },
    ClearAlarm {
        attempt_id: NotificationAttemptId,
    },
    AttentionComplete {
        attempt_id: NotificationAttemptId,
    },
    AttentionDiscard {
        attempt_id: NotificationAttemptId,
    },
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// A read succeeded and this is what the detail shows.
    Opened(Box<Loaded>),
    /// A mutation succeeded.
    Done(String),
    /// The daemon refused. Final: the operator should read it, not retry.
    Refused { code: String, message: String },
    /// The request never left this process. Nothing happened.
    ///
    /// A connect or hello failure is knowledge, not doubt: the daemon
    /// never saw the request, so every action is still available.
    NotSent(String),
    /// The request was written and the outcome is unknown. It may have
    /// landed, so a reply must be retried under the same key and the
    /// terminal verbs must not be repeated as fresh actions. A later
    /// matching terminal-accepted fact may permit no-key reconciliation.
    Uncertain(String),
}

/// One request on one connection.
///
/// A connection per request, matching the rest of this crate. These are
/// operator-paced actions, not a hot path.
pub async fn perform(sock: &Path, request: ActionRequest) -> ActionOutcome {
    match request {
        ActionRequest::OpenMessage { message_id, claim } => {
            open_message(sock, &message_id, claim).await
        }
        ActionRequest::OpenAttention { attempt_id } => open_attention(sock, attempt_id).await,
        ActionRequest::Reply {
            message_id,
            body,
            client_key,
        } => {
            let params = json!({
                "message_id": message_id,
                "body": body,
                "client_key": client_key,
            });
            match call(sock, "msg.reply", params).await {
                Ok(value) => ActionOutcome::Done(format!(
                    "replied as {}",
                    value["msg_id"].as_str().unwrap_or("(unknown)")
                )),
                Err(e) => e.into_outcome(),
            }
        }
        ActionRequest::WithdrawNotification {
            attempt_id,
            recipient,
        } => {
            let params = serde_json::to_value(cyclops_proto::NotificationWithdrawParams {
                attempt_id,
                recipient,
            })
            .expect("notification.withdraw params serialize");
            match call(sock, "notification.withdraw", params).await {
                Ok(value) => {
                    match serde_json::from_value::<cyclops_proto::NotificationWithdrawResult>(value)
                    {
                        Ok(result) => ActionOutcome::Done(format!(
                        "wake {} for {}; message remains claimable",
                        match result.disposition {
                            cyclops_proto::NotificationWithdrawDisposition::Withdrawn => {
                                "withdrawn"
                            }
                            cyclops_proto::NotificationWithdrawDisposition::AlreadyWithdrawn => {
                                "already withdrawn"
                            }
                        },
                        result.recipient
                    )),
                        Err(error) => ActionOutcome::Uncertain(format!(
                            "unreadable notification.withdraw result: {error}"
                        )),
                    }
                }
                Err(error) => error.into_outcome(),
            }
        }
        ActionRequest::ClearAlarm { attempt_id } => {
            let params = json!({ "ids": [attempt_id.to_string()] });
            match call(sock, "alarm.clear", params).await {
                Ok(_) => ActionOutcome::Done("alarm cleared".into()),
                Err(e) => e.into_outcome(),
            }
        }
        ActionRequest::AttentionComplete { attempt_id } => {
            resolve(
                sock,
                "attention.complete",
                attempt_id,
                "notification submitted",
            )
            .await
        }
        ActionRequest::AttentionDiscard { attempt_id } => {
            resolve(
                sock,
                "attention.discard",
                attempt_id,
                "notification discarded",
            )
            .await
        }
    }
}

/// The two verbs that end an alarm's life.
///
/// A repeat after success is refused with conflict rather than replayed.
/// An uncertain action is different: the same RPC enters the daemon's
/// no-key reconciliation path only after a current snapshot exposes a
/// matching terminal-accepted fact.
async fn resolve(
    sock: &Path,
    method: &str,
    attempt_id: NotificationAttemptId,
    word: &str,
) -> ActionOutcome {
    let params = json!({ "id": attempt_id.to_string() });
    match call(sock, method, params).await {
        Ok(value) => ActionOutcome::Done(format!(
            "{word} {}",
            value["attempt_id"]
                .as_str()
                .unwrap_or(&attempt_id.to_string())
        )),
        Err(Failure::Refused { code, message }) if code == "conflict" => ActionOutcome::Refused {
            code,
            message: format!("already resolved: {message}"),
        },
        // The daemon persisted the intent and then could not finish, so
        // the outcome is genuinely unknown on its side too. This is a
        // refusal in wire shape and an ambiguity in meaning. A later
        // snapshot may expose matching durable terminal acceptance for
        // no-key recovery. Intent alone never authorizes it.
        Err(Failure::Refused { code, message }) if code == "attention_action_uncertain" => {
            ActionOutcome::Uncertain(format!(
                "the daemon recorded the intent and could not finish: {message}"
            ))
        }
        Err(e) => e.into_outcome(),
    }
}

/// Open a message: the claim first, then the thread as enrichment.
///
/// The claim already returns the immutable body, so it is the whole
/// answer on its own. The thread is context, and a failure to fetch it
/// must not throw away a claim that landed: the reader keeps the body
/// and is told the thread is unavailable. Reclaiming is idempotent for
/// the same recipient, so reopening after an unanswered claim returns
/// AlreadyClaimed and the same bytes rather than duplicating anything.
async fn open_message(sock: &Path, message_id: &MessageId, claim: bool) -> ActionOutcome {
    let mut loaded = Loaded::default();
    if claim {
        match call(sock, "inbox.claim", json!({ "message_id": message_id })).await {
            Ok(value) => {
                // Say which happened. Recovering an existing claim and
                // taking a new one return the same bytes, and a reader
                // who cannot tell them apart cannot trust either.
                loaded.claim_note = Some(match value["disposition"].as_str() {
                    Some("already_claimed") => "recovered existing claim".into(),
                    _ => "claimed now".into(),
                });
                // A successful claim authorizes the message even when it
                // has no body value. Do not confuse absence with redaction.
                loaded.body_authorized = true;
                loaded.body = value["message"]["body"]
                    .as_str()
                    .map(crate::grid::safe_text);
            }
            Err(e) => return e.into_outcome(),
        }
    }
    match call(sock, "msg.thread", json!({ "id": message_id.to_string() })).await {
        Ok(value) => {
            let want = message_id.to_string();
            let thread_problem = match value["lines"].as_array() {
                None => Some("msg.thread returned no lines".to_string()),
                Some(lines) if lines.len() > crate::stream::RING_CAP => Some(format!(
                    "msg.thread exceeds the {}-item UI limit",
                    crate::stream::RING_CAP
                )),
                Some(_) => None,
            };
            if let Some(problem) = thread_problem {
                if loaded.body.is_some() || loaded.claim_note.is_some() {
                    loaded.thread_note = Some(format!("thread unavailable: {problem}"));
                    return ActionOutcome::Opened(Box::new(loaded));
                }
                return ActionOutcome::Uncertain(problem);
            }
            let lines = value["lines"].as_array().expect("validated above");
            for line in lines {
                let kind = line["kind"].as_str().unwrap_or("");
                if kind != "msg" && kind != "fyi" {
                    continue; // state and gate lines are not the thread
                }
                let id = line["id"].as_str().unwrap_or("").to_string();
                if id == want {
                    // The daemon strips a body it will not authorize, so
                    // whatever is here is what this reader may read.
                    if loaded.body.is_none() {
                        loaded.body = line["body"].as_str().map(crate::grid::safe_text);
                    }
                    loaded.body_authorized |= loaded.body.is_some();
                    continue;
                }
                loaded.thread.push(ThreadEntry {
                    message_id: id,
                    sender_label: crate::grid::safe_text(line["from"].as_str().unwrap_or("")),
                    subject: line["subject"].as_str().map(crate::grid::safe_text),
                    body: line["body"].as_str().map(crate::grid::safe_text),
                    ts: line["ts"].as_u64().unwrap_or(0),
                });
            }
            ActionOutcome::Opened(Box::new(loaded))
        }
        // The claim landed and the context did not. Throwing the body
        // away here would turn a successful claim into "nothing
        // happened" and leave the reader unable to act on mail they
        // already own.
        Err(e) => {
            if loaded.body.is_some() || loaded.claim_note.is_some() {
                loaded.thread_note = Some(format!("thread unavailable: {}", e.why()));
                ActionOutcome::Opened(Box::new(loaded))
            } else {
                e.into_outcome()
            }
        }
    }
}

async fn open_attention(sock: &Path, attempt_id: NotificationAttemptId) -> ActionOutcome {
    // The diff belongs here. It is local evidence, which is why it does
    // not belong in a LIST, but this is one attempt the operator opened
    // deliberately and is about to complete or discard. Five booleans
    // name which check failed and never what actually differs, and
    // "notification exact: no" is not something a person can act on.
    // One read per open, not one per row.
    let params = json!({ "id": attempt_id.to_string(), "diff": true });
    match call(sock, "attention.show", params).await {
        Ok(value) => {
            let mut loaded = Loaded::default();
            match serde_json::from_value::<cyclops_proto::AttentionChecks>(value["checks"].clone())
            {
                Ok(checks) => loaded.checks = named_checks(&checks),
                Err(e) => {
                    return ActionOutcome::Uncertain(format!("unreadable attention checks: {e}"))
                }
            }
            // Both are optional on the wire and stay optional here. A
            // missing observed means exact extraction failed, which the
            // renderer says out loud rather than hiding.
            loaded.expected = value["expected"].as_str().map(crate::grid::safe_text);
            loaded.observed = value["observed"].as_str().map(crate::grid::safe_text);
            ActionOutcome::Opened(Box::new(loaded))
        }
        Err(e) => e.into_outcome(),
    }
}

/// The five checks, in the daemon's order, under the daemon's own names.
///
/// Spelled out rather than reworded. A friendlier name here would be this
/// crate asserting what a check means, and the wire carries only the
/// field name and a boolean. If these read badly to an operator, the
/// names belong in the protocol next to the values.
fn named_checks(checks: &cyclops_proto::AttentionChecks) -> Vec<Check> {
    [
        ("notification exact", checks.notification_exact),
        ("trailer anchored", checks.trailer_anchored),
        ("process matches", checks.process_matches),
        ("manifest matches", checks.manifest_matches),
        ("terminal action safe", checks.terminal_action_safe),
    ]
    .into_iter()
    .map(|(name, passed)| Check {
        name: name.to_string(),
        passed,
        detail: None,
    })
    .collect()
}

/// A failed call, split by whether the daemon answered.
#[derive(Debug, Clone)]
enum Failure {
    /// The daemon answered no.
    Refused { code: String, message: String },
    /// The request was never written. Nothing happened.
    NotSent(String),
    /// It was written and nothing readable came back.
    Uncertain(String),
}

impl Failure {
    fn why(&self) -> &str {
        match self {
            Failure::Refused { message, .. } => message,
            Failure::NotSent(why) | Failure::Uncertain(why) => why,
        }
    }

    fn into_outcome(self) -> ActionOutcome {
        match self {
            Failure::Refused { code, message } => ActionOutcome::Refused { code, message },
            Failure::NotSent(why) => ActionOutcome::NotSent(why),
            Failure::Uncertain(why) => ActionOutcome::Uncertain(why),
        }
    }
}

/// Getting connected and greeted. A stall here is known-not-sent,
/// because nothing has been written yet.
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// Waiting for an answer to a request already on the wire. A stall here
/// is genuinely unknown: the daemon may have acted.
pub(crate) const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// One request, with the two phases timed separately.
///
/// A single deadline around the whole call made a connect stall look
/// like a maybe-landed mutation, which is the worst possible reading: it
/// withholds actions and tells an operator to go and look, over a
/// request the daemon never saw. The phases are split so silence before
/// the write is reported as the knowledge it is.
async fn call(sock: &Path, method: &str, params: Value) -> Result<Value, Failure> {
    let connected = match tokio::time::timeout(OPEN_TIMEOUT, open(sock)).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(Failure::NotSent(format!(
                "no connection within {}s",
                OPEN_TIMEOUT.as_secs()
            )))
        }
    };
    match tokio::time::timeout(ANSWER_TIMEOUT, ask(connected, method, params)).await {
        Ok(result) => result,
        Err(_) => Err(Failure::Uncertain(format!(
            "{method} did not answer within {}s",
            ANSWER_TIMEOUT.as_secs()
        ))),
    }
}

/// Connect and read the hello. Nothing here writes a request, so every
/// failure in this function is known-not-sent.
type Connection = (
    FrameReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
);

async fn open(sock: &Path) -> Result<Connection, Failure> {
    let stream = UnixStream::connect(sock)
        .await
        .map_err(|e| Failure::NotSent(format!("connect: {e}")))?;
    let (read, write) = stream.into_split();
    let mut frames = FrameReader::new(read);
    let frame = frames
        .next_frame()
        .await
        .map_err(|e| Failure::NotSent(format!("hello: {e}")))?
        .ok_or_else(|| Failure::NotSent("closed before the hello".into()))?;
    match serde_json::from_slice::<cyclops_proto::Hello>(&frame) {
        Ok(_) => Ok((frames, write)),
        Err(_) => Err(Failure::NotSent("not a cyclops daemon".into())),
    }
}

/// Write one request and read its answer.
///
/// Encoding failure is known-not-sent. Once the socket write begins, any
/// transport failure is uncertain because the peer may have accepted the
/// complete frame before the local error became visible.
async fn ask(connection: Connection, method: &str, params: Value) -> Result<Value, Failure> {
    let (mut frames, mut w) = connection;
    write_request(&mut w, method, params).await?;

    let frame = frames
        .next_frame()
        .await
        .map_err(|e| Failure::Uncertain(format!("read: {e}")))?
        .ok_or_else(|| Failure::Uncertain("connection closed before an answer".into()))?;
    let response: cyclops_proto::Response = serde_json::from_slice(&frame)
        .map_err(|e| Failure::Uncertain(format!("malformed {method} answer: {e}")))?;
    if response.id != 1 {
        return Err(Failure::Uncertain(format!(
            "{method} returned the wrong response id"
        )));
    }
    if let Some(error) = response.error {
        if error.code == cyclops_proto::FrameContract::TOO_LARGE_CODE {
            return Err(Failure::Uncertain(error.message));
        }
        return Err(Failure::Refused {
            code: error.code,
            message: error.message,
        });
    }
    response
        .result
        .ok_or_else(|| Failure::Uncertain(format!("{method} returned no result")))
}

async fn write_request(
    writer: &mut (impl AsyncWrite + Unpin),
    method: &str,
    params: Value,
) -> Result<(), Failure> {
    let request = json!({ "id": 1, "method": method, "params": params });
    let mut request =
        encode_json(&request).map_err(|e| Failure::NotSent(format!("encode request: {e}")))?;
    request.push(b'\n');
    writer
        .write_all(&request)
        .await
        .map_err(|e| Failure::Uncertain(format!("send: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::str::FromStr;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;

    struct PartialWriter {
        remaining: usize,
    }

    impl AsyncWrite for PartialWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "simulated socket failure",
                )));
            }
            let written = self.remaining.min(bytes.len());
            self.remaining -= written;
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_partial_action_write_remains_outcome_uncertain() {
        let mut writer = PartialWriter { remaining: 8 };
        let error = write_request(&mut writer, "ping", json!({}))
            .await
            .expect_err("the simulated partial write must fail");
        assert!(
            matches!(error, Failure::Uncertain(message) if message.contains("simulated socket failure"))
        );
    }

    /// A real greeting. The client rejects anything else, so a fixture
    /// that fakes one is testing a daemon that does not exist.
    fn hello_line() -> String {
        let hello = cyclops_proto::Hello {
            cyclops: "0.0.0-test".into(),
            build: None,
            daemon_process: None,
            daemon_executable: None,
            proto: cyclops_proto::PROTOCOL_VERSION,
            boot_id: "boot-test".into(),
        };
        format!("{}\n", serde_json::to_string(&hello).unwrap())
    }

    /// Serve exactly one request: greet, read it, answer with `result`.
    /// Hands back the request that was actually written, which is the
    /// thing under test.
    async fn one_call(name: &str, result: Value, request: ActionRequest) -> (Value, ActionOutcome) {
        let home = cyclops_proto::scratch::scratch_dir(name);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            write.write_all(hello_line().as_bytes()).await.unwrap();
            let mut lines = BufReader::new(read).lines();
            let asked: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let answer = serde_json::json!({ "id": asked["id"], "result": result });
            write
                .write_all(format!("{answer}\n").as_bytes())
                .await
                .unwrap();
            asked
        });

        let outcome = perform(&sock, request).await;
        (server.await.unwrap(), outcome)
    }

    #[tokio::test]
    async fn a_bounded_oversized_response_error_remains_outcome_uncertain() {
        let home = cyclops_proto::scratch::scratch_dir("ui-action-io-frame-too-large");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            write.write_all(hello_line().as_bytes()).await.unwrap();
            let mut lines = BufReader::new(read).lines();
            let asked: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let answer = json!({
                "id": asked["id"],
                "error": {
                    "code": cyclops_proto::FrameContract::TOO_LARGE_CODE,
                    "message": "daemon response was too large; request outcome is unknown"
                }
            });
            write
                .write_all(format!("{answer}\n").as_bytes())
                .await
                .unwrap();
        });

        let outcome = perform(
            &sock,
            ActionRequest::AttentionComplete {
                attempt_id: NotificationAttemptId::parse(
                    "att-00000000-0000-4000-8000-000000000019",
                )
                .unwrap(),
            },
        )
        .await;
        server.await.unwrap();
        match outcome {
            ActionOutcome::Uncertain(why) => assert!(why.contains("outcome is unknown"), "{why}"),
            other => panic!("oversized daemon response was misclassified: {other:?}"),
        }
    }

    #[tokio::test]
    async fn claiming_a_subject_only_message_authorizes_its_empty_body() {
        let home = cyclops_proto::scratch::scratch_dir("ui-action-io-empty-body");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            for expected in ["inbox.claim", "msg.thread"] {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                write.write_all(hello_line().as_bytes()).await.unwrap();
                let mut lines = BufReader::new(read).lines();
                let asked: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(asked["method"], expected);
                let result = if expected == "inbox.claim" {
                    json!({
                        "disposition": "claimed",
                        "message": {
                            "message_id": "m-empty",
                            "kind": "msg",
                            "sender_label": "reviewer",
                            "subject": "Subject only",
                            "thread_root": "m-empty"
                        }
                    })
                } else {
                    json!({
                        "lines": [{
                            "id": "m-empty",
                            "kind": "msg",
                            "from": "reviewer",
                            "to": ["implementer"],
                            "subject": "Subject only",
                            "ts": 1
                        }]
                    })
                };
                let answer = json!({"id": asked["id"], "result": result});
                write
                    .write_all(format!("{answer}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });

        let outcome = perform(
            &sock,
            ActionRequest::OpenMessage {
                message_id: MessageId::new("m-empty").unwrap(),
                claim: true,
            },
        )
        .await;
        server.await.unwrap();
        let ActionOutcome::Opened(loaded) = outcome else {
            panic!("subject-only message did not open: {outcome:?}");
        };
        assert!(loaded.body_authorized);
        assert!(loaded.body.is_none());
    }

    /// The operator is about to complete or discard on this evidence, so
    /// the request has to ask for it. Without diff:true the daemon returns
    /// booleans only and the detail has nothing to show.
    #[tokio::test]
    async fn opening_an_alarm_asks_for_the_diff_and_keeps_both_sides() {
        let attempt_id =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000007").unwrap();
        let (asked, outcome) = one_call(
            "ui-action-io-attention-diff",
            serde_json::json!({
                "attempt_id": attempt_id.to_string(),
                "checks": {
                    "notification_exact": false,
                    "trailer_anchored": true,
                    "process_matches": true,
                    "manifest_matches": true,
                    "terminal_action_safe": true,
                },
                "expected": "STAGED",
                "observed": "IN THE PANE",
            }),
            ActionRequest::OpenAttention { attempt_id },
        )
        .await;

        assert_eq!(asked["method"], "attention.show");
        assert_eq!(
            asked["params"]["diff"], true,
            "the detail asked for booleans only: {asked}"
        );
        match outcome {
            ActionOutcome::Opened(loaded) => {
                assert_eq!(loaded.expected.as_deref(), Some("STAGED"));
                assert_eq!(loaded.observed.as_deref(), Some("IN THE PANE"));
                assert_eq!(loaded.checks.len(), 5);
                assert_eq!(loaded.checks[0].name, "notification exact");
                assert!(!loaded.checks[0].passed);
            }
            other => panic!("the open did not land: {other:?}"),
        }
    }

    #[tokio::test]
    async fn withdrawal_names_the_exact_attempt_and_recipient() {
        let attempt_id =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000010").unwrap();
        let recipient = cyclops_proto::RecipientKey::agent(
            cyclops_proto::WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
            cyclops_proto::SessionInstanceId::from_str("00000000-0000-0000-0000-000000000002")
                .unwrap(),
            cyclops_proto::TmuxPaneId::from_str("%1").unwrap(),
        );
        for (name, disposition, expected) in [
            ("withdrawn", "withdrawn", "wake withdrawn"),
            (
                "already-withdrawn",
                "already_withdrawn",
                "wake already withdrawn",
            ),
        ] {
            let (asked, outcome) = one_call(
                &format!("ui-action-io-{name}"),
                json!({
                    "attempt_id": attempt_id,
                    "message_id": "m-blocked",
                    "recipient": recipient,
                    "disposition": disposition,
                }),
                ActionRequest::WithdrawNotification {
                    attempt_id,
                    recipient,
                },
            )
            .await;

            assert_eq!(asked["method"], "notification.withdraw");
            assert_eq!(asked["params"]["attempt_id"], attempt_id.to_string());
            assert_eq!(
                asked["params"]["recipient"],
                serde_json::to_value(recipient).unwrap()
            );
            match outcome {
                ActionOutcome::Done(message) => {
                    assert!(message.contains(expected), "{message}");
                    assert!(message.contains("message remains claimable"), "{message}");
                }
                other => panic!("withdrawal did not complete: {other:?}"),
            }
        }
    }

    /// Serve a connection that greets with `greeting` (or closes at once
    /// when it is None) and then says nothing.
    async fn bad_greeting(name: &str, greeting: Option<&str>) -> ActionOutcome {
        let home = cyclops_proto::scratch::scratch_dir(name);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();
        let owned = greeting.map(str::to_string);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_read, mut write) = stream.into_split();
            if let Some(text) = owned {
                let _ = write.write_all(text.as_bytes()).await;
                // Hold the socket open so this is a greeting problem and
                // not a closed connection.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let outcome = perform(
            &sock,
            ActionRequest::AttentionComplete {
                attempt_id: NotificationAttemptId::parse(
                    "att-00000000-0000-4000-8000-000000000009",
                )
                .unwrap(),
            },
        )
        .await;
        server.abort();
        outcome
    }

    /// A socket that closes before greeting never accepted anything, so
    /// this has to be NotSent. Reported as Uncertain it would withhold a
    /// non-idempotent verb and send an operator to inspect a pane over a
    /// request the daemon never saw.
    #[tokio::test]
    async fn a_close_before_the_hello_is_known_not_sent() {
        match bad_greeting("ui-action-io-eof-hello", None).await {
            ActionOutcome::NotSent(why) => assert!(why.contains("hello"), "{why}"),
            other => panic!("a closed socket was not reported as not-sent: {other:?}"),
        }
    }

    /// Something else listening on the path is not a daemon. Writing a
    /// terminal verb into it and then reading nothing back is the
    /// ambiguity this refuses to create.
    #[tokio::test]
    async fn a_greeting_that_is_not_a_hello_is_refused_before_any_write() {
        for (name, greeting) in [
            ("ui-action-io-junk-hello", "not json at all\n"),
            ("ui-action-io-wrong-hello", "{\"hello\":true}\n"),
        ] {
            match bad_greeting(name, Some(greeting)).await {
                ActionOutcome::NotSent(why) => {
                    assert!(why.contains("daemon"), "{why}")
                }
                other => panic!("{name}: accepted a bad greeting: {other:?}"),
            }
        }
    }

    /// A body arrives as opaque text from another agent and is drawn into
    /// a terminal. Sanitizing has to happen on the way IN, or every
    /// renderer has to remember to do it and one eventually will not.
    #[tokio::test]
    async fn a_hostile_body_is_sanitized_before_it_is_stored() {
        let message_id = MessageId::new("m-001").unwrap();
        let hostile = "a\u{1b}[2Jb\rc\td\u{9b}e";
        let (_, outcome) = one_call(
            "ui-action-io-hostile-body",
            serde_json::json!({
                "lines": [
                    { "id": "m-001", "kind": "msg", "from": hostile,
                      "subject": hostile, "body": hostile, "ts": 1 },
                    { "id": "m-000", "kind": "msg", "from": hostile,
                      "subject": hostile, "body": hostile, "ts": 0 },
                ],
            }),
            ActionRequest::OpenMessage {
                message_id: message_id.clone(),
                claim: false,
            },
        )
        .await;

        let loaded = match outcome {
            ActionOutcome::Opened(loaded) => loaded,
            other => panic!("the open did not land: {other:?}"),
        };
        let body = loaded.body.expect("a body came back");
        assert!(!body.contains('\u{1b}'), "ESC stored: {body:?}");
        assert!(!body.contains('\u{9b}'), "8-bit CSI stored: {body:?}");
        assert!(!body.contains('\r'), "CR stored: {body:?}");
        assert!(!body.contains('\t'), "tab stored: {body:?}");
        assert!(
            body.contains('a') && body.contains('e'),
            "text lost: {body:?}"
        );

        // The thread entry is a second, independent path into the frame.
        let entry = loaded.thread.first().expect("an earlier line came back");
        for field in [
            Some(entry.sender_label.clone()),
            entry.subject.clone(),
            entry.body.clone(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(!field.contains('\u{1b}'), "ESC stored in a thread entry");
            assert!(!field.contains('\r'), "CR stored in a thread entry");
        }
    }

    /// The daemon omits observed when exact extraction failed. That must
    /// survive as absence rather than becoming an empty string, because
    /// the renderer says so out loud.
    #[tokio::test]
    async fn an_omitted_observed_stays_absent() {
        let attempt_id =
            NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000008").unwrap();
        let (_, outcome) = one_call(
            "ui-action-io-attention-no-observed",
            serde_json::json!({
                "attempt_id": attempt_id.to_string(),
                "checks": {
                    "notification_exact": false,
                    "trailer_anchored": true,
                    "process_matches": true,
                    "manifest_matches": true,
                    "terminal_action_safe": true,
                },
                "expected": "STAGED",
            }),
            ActionRequest::OpenAttention { attempt_id },
        )
        .await;
        match outcome {
            ActionOutcome::Opened(loaded) => {
                assert_eq!(loaded.expected.as_deref(), Some("STAGED"));
                assert!(
                    loaded.observed.is_none(),
                    "a missing observed became {:?}",
                    loaded.observed
                );
            }
            other => panic!("the open did not land: {other:?}"),
        }
    }
}
