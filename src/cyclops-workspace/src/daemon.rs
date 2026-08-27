//! Small, bounded requests to cyclopsd for workspace decoration and naming.
//!
//! The daemon speaks Hello-first NDJSON. Every helper in this module consumes
//! that Hello before sending its request; keeping the transport here prevents
//! confirmation, decoration, and naming from drifting into subtly different
//! protocol clients.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use cyclops_proto::{Hello, PaneStatus, Request, Response, StatusParams, StatusResult, SOCK_NAME};
use serde_json::{json, Value};

const IO_TIMEOUT: Duration = Duration::from_millis(250);
/// Largest hello or response retained from the daemon. Status and action
/// payloads share this envelope so a count-bounded app queue cannot receive
/// one unbounded item.
pub(crate) const DAEMON_LINE_MAX_BYTES: usize = 1 << 20;

/// Deadline for a send, which is a different kind of request from every
/// other one here.
///
/// The rest are questions the daemon can answer from what it already knows,
/// so 250ms is generous. A send is not: the daemon holds the response while
/// it waits for the recipient to acknowledge, up to `receipt_block_ms`, and
/// answering early is the whole point of the blocking window. Reusing
/// IO_TIMEOUT here reports a timeout on a message that was delivered, which
/// is the worst answer available.
///
/// It is only safe to wait this long because the send runs off the UI
/// thread; nothing on this deadline may ever be called from the draw loop.
const SEND_TIMEOUT: Duration = Duration::from_secs(15);

fn connect(home: &Path) -> Result<BufReader<UnixStream>, String> {
    connect_with(home, IO_TIMEOUT)
}

fn connect_with(home: &Path, timeout: Duration) -> Result<BufReader<UnixStream>, String> {
    let path = home.join(SOCK_NAME);
    let stream = UnixStream::connect(&path)
        .map_err(|error| format!("cyclopsd is unavailable at {}: {error}", path.display()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("cannot set cyclopsd read deadline: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("cannot set cyclopsd write deadline: {error}"))?;
    let mut reader = BufReader::new(stream);

    let hello = read_value(&mut reader, "hello")?;
    serde_json::from_value::<Hello>(hello)
        .map_err(|error| format!("cyclopsd sent an unreadable hello: {error}"))?;
    Ok(reader)
}

fn write_request(stream: &mut UnixStream, method: &str, params: Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(&Request {
        id: json!(1),
        method: method.to_string(),
        params,
    })
    .map_err(|error| format!("cannot encode {method} request: {error}"))?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|error| format!("cannot write {method} request: {error}"))
}

/// One request on a fresh connection. Requests are infrequent user actions
/// or event-triggered refreshes, so a short-lived connection is simpler and
/// safer than sharing the daemon subscription's stream.
pub(crate) fn request(home: &Path, method: &str, params: Value) -> Result<Value, String> {
    exchange(&mut connect(home)?, method, params)
}

/// The request/response half of [`request`], on an already-open
/// connection. Split out because [`theme_reload`] has to tell "nothing
/// answered on the socket" apart from "a daemon answered and refused".
fn exchange(
    reader: &mut BufReader<UnixStream>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    write_request(reader.get_mut(), method, params)?;

    // A fresh, unsubscribed connection has exactly one response after its
    // Hello. Anything else is a protocol error; failing immediately keeps a
    // malformed peer from extending this bounded request indefinitely.
    let response = read_value(reader, method)?;
    let response = serde_json::from_value::<Response>(response)
        .map_err(|error| format!("cyclopsd sent an unreadable {method} response: {error}"))?;
    if response.id != json!(1) {
        return Err(format!(
            "cyclopsd replied to {method} with the wrong request id"
        ));
    }
    if let Some(error) = response.error {
        return Err(error.message);
    }
    response
        .result
        .ok_or_else(|| format!("cyclopsd omitted the {method} result"))
}

fn read_value(reader: &mut BufReader<UnixStream>, context: &str) -> Result<Value, String> {
    let line = read_bounded_line(reader, context)?;
    serde_json::from_slice(&line)
        .map_err(|error| format!("cyclopsd sent unreadable {context}: {error}"))
}

fn read_bounded_line(reader: &mut impl BufRead, context: &str) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    loop {
        let (take, complete) = {
            let available = reader
                .fill_buf()
                .map_err(|error| format!("cannot read cyclopsd {context}: {error}"))?;
            if available.is_empty() {
                if line.is_empty() {
                    return Err(format!("cyclopsd closed during {context}"));
                }
                return Ok(line);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |index| index + 1);
            if line.len().saturating_add(take) > DAEMON_LINE_MAX_BYTES {
                return Err(format!(
                    "cyclopsd {context} exceeded {DAEMON_LINE_MAX_BYTES} bytes"
                ));
            }
            line.extend_from_slice(&available[..take]);
            (take, newline.is_some())
        };
        reader.consume(take);
        if complete {
            return Ok(line);
        }
    }
}

/// A daemon refusal with its stable machine-readable code intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonRefusal {
    pub code: String,
    pub message: String,
    pub data: Option<Value>,
}

impl From<cyclops_proto::WireError> for DaemonRefusal {
    fn from(error: cyclops_proto::WireError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

impl DaemonRefusal {
    #[cfg(test)]
    pub(crate) fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
            data: None,
        }
    }
}

/// Result of a composer send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    Accepted(String),
    /// The request did not reach the daemon.
    NotSent(String),
    /// The daemon answered no. The code is kept for state-specific recovery.
    Rejected(DaemonRefusal),
    /// The request write began, but no trustworthy response arrived.
    Unknown(String),
}

/// Send one message, and report the receipt the way `cyclops send` does.
///
/// MUST NOT be called from the draw loop. It waits on [`SEND_TIMEOUT`],
/// which is measured in seconds because the daemon holds the response for
/// the acknowledgement window; on the UI thread that is a frozen workspace.
/// `app` runs it on a thread of its own and takes the answer back as a
/// message.
///
/// The returned string is the receipt line, already in the vocabulary the
/// CLI prints, so the composer shows the operator the same words `cyclops
/// send` would have. Once the request write begins, transport failure is
/// outcome-unknown because the daemon may already have accepted it.
pub fn send_message(
    home: &Path,
    to: &str,
    subject: &str,
    body: &str,
    client_key: &str,
) -> SendOutcome {
    send_message_request(
        home,
        MessageRequest {
            to: vec![to.to_string()],
            recipient_keys: None,
            expected_caller: None,
            subject,
            body,
            fyi: false,
            reply_to: None,
            client_key,
        },
    )
}

/// Exact sender and recipients used by the Messages composer.
pub struct ExactMessageRequest<'a> {
    pub recipient_keys: Option<Vec<cyclops_proto::RecipientKey>>,
    pub expected_caller: cyclops_proto::RecipientKey,
    pub subject: &'a str,
    pub body: &'a str,
    pub fyi: bool,
    pub reply_to: Option<String>,
    pub client_key: &'a str,
}

/// Send from the Messages pane using exact recipients or a reply reference.
pub fn send_message_full(home: &Path, request: ExactMessageRequest<'_>) -> SendOutcome {
    send_message_request(
        home,
        MessageRequest {
            to: Vec::new(),
            recipient_keys: request.recipient_keys,
            expected_caller: Some(request.expected_caller),
            subject: request.subject,
            body: request.body,
            fyi: request.fyi,
            reply_to: request.reply_to,
            client_key: request.client_key,
        },
    )
}

struct MessageRequest<'a> {
    to: Vec<String>,
    recipient_keys: Option<Vec<cyclops_proto::RecipientKey>>,
    expected_caller: Option<cyclops_proto::RecipientKey>,
    subject: &'a str,
    body: &'a str,
    fyi: bool,
    reply_to: Option<String>,
    client_key: &'a str,
}

fn send_message_request(home: &Path, request: MessageRequest<'_>) -> SendOutcome {
    let params = match serde_json::to_value(cyclops_proto::MsgSendParams {
        to: request.to,
        recipient_keys: request.recipient_keys,
        expected_caller: request.expected_caller,
        subject: request.subject.to_string(),
        body: request.body.to_string(),
        fyi: request.fyi,
        client_key: Some(request.client_key.to_string()),
        reply_to: request.reply_to,
        supersedes: None,
        wait: None,
    }) {
        Ok(params) => params,
        Err(error) => {
            return SendOutcome::NotSent(format!("cannot encode the message: {error}"));
        }
    };
    let mut reader = match connect_with(home, SEND_TIMEOUT) {
        Ok(reader) => reader,
        Err(error) => return SendOutcome::NotSent(error),
    };
    if let Err(error) = write_request(reader.get_mut(), "msg.send", params) {
        return SendOutcome::Unknown(error);
    }
    let value = match read_value(&mut reader, "msg.send") {
        Ok(value) => value,
        Err(error) => return SendOutcome::Unknown(error),
    };
    let response = match serde_json::from_value::<Response>(value) {
        Ok(response) => response,
        Err(error) => {
            return SendOutcome::Unknown(format!(
                "cyclopsd sent an unreadable msg.send response: {error}"
            ));
        }
    };
    if response.id != json!(1) {
        return SendOutcome::Unknown(
            "cyclopsd replied to msg.send with the wrong request id".to_string(),
        );
    }
    if let Some(error) = response.error {
        return SendOutcome::Rejected(error.into());
    }
    let Some(value) = response.result else {
        return SendOutcome::Unknown("cyclopsd omitted the msg.send result".to_string());
    };
    let result: cyclops_proto::MsgSendResult = match serde_json::from_value(value) {
        Ok(result) => result,
        Err(error) => {
            return SendOutcome::Unknown(format!(
                "cyclopsd sent an unreadable send result: {error}"
            ));
        }
    };
    if result.inserted == Some(false) {
        SendOutcome::Accepted(format!("already accepted {}", result.msg_id))
    } else {
        SendOutcome::Accepted(format!("accepted {}", receipt_line(&result)))
    }
}

/// One line for what happened to a send, in the receipt vocabulary.
fn receipt_line(result: &cyclops_proto::MsgSendResult) -> String {
    match result.deliveries.first() {
        Some(delivery) => format!(
            "{} · {}",
            result.msg_id,
            cyclops_ui::grid::receipt_badge(delivery, &cyclops_ui::grid::Plain)
        ),
        None => format!("{} · on the record", result.msg_id),
    }
}

/// Fetch a bounded snapshot of recent messages for the Messages TUI.
pub fn fetch_messages_snapshot(
    home: &Path,
    limit: usize,
) -> Result<cyclops_proto::MessagesSnapshotResult, String> {
    let params = serde_json::to_value(cyclops_proto::MessagesSnapshotParams {
        recent_settled: u32::try_from(limit).unwrap_or(u32::MAX).min(100),
    })
    .map_err(|e| format!("cannot encode messages.snapshot params: {e}"))?;
    let value = request(home, "messages.snapshot", params)?;
    serde_json::from_value(value)
        .map_err(|e| format!("cannot decode messages.snapshot result: {e}"))
}

/// Current daemon status. Callers pass the shared protocol params so the
/// choice to include the delivery half of attention stays visible at the
/// surface that needs it.
pub fn status(home: &Path, params: StatusParams) -> Result<StatusResult, String> {
    let params = serde_json::to_value(params)
        .map_err(|error| format!("cannot encode cyclopsd status request: {error}"))?;
    let value = request(home, "status", params)?;
    serde_json::from_value(value).map_err(|error| format!("unreadable cyclopsd status: {error}"))
}

/// True when the daemon reports an adopted agent in this pane.
pub fn pane_has_agent(home: &Path, pane_id: &str) -> bool {
    status(home, StatusParams::default())
        .map(|status| {
            status
                .sessions
                .iter()
                .flat_map(|session| session.panes.iter())
                .any(|pane: &PaneStatus| pane.pane_id == pane_id && pane.agent.is_some())
        })
        .unwrap_or(false)
}

/// Ask cyclopsd to watch a session it was not booted with.
///
/// `sessions` in config.toml is the daemon's BOOT set, and a workspace this
/// UI creates is never in it. An unwatched session has no pane table at all,
/// so nothing in it can be detected, named, or given a state: its sidebar
/// row would stay empty for as long as the daemon runs.
///
/// Callers ask once per session id and never again. A tmux control
/// connection survives a session rename — the daemon holds the session
/// object, not its name — so re-asking under the new name would build a
/// second slot naming something nothing answers to, and leave its watcher
/// retrying an attach for the daemon's whole life.
pub fn watch_session(home: &Path, session: &str) -> Result<(), String> {
    request(home, "session.watch", json!({"session": session}))?;
    Ok(())
}

/// How far a theme nudge got: the two answers a caller has to tell apart
/// because they are two different stories (the CLI's `Switch`).
#[derive(Debug, PartialEq, Eq)]
pub enum ThemeReload {
    /// A daemon answered; carries the theme it says it is NOW painting,
    /// which is not always the one just chosen (a `CYCLOPS_THEME` pinned
    /// in its environment beats the config key). `None` when the answer
    /// named no theme, or the daemon refused: either way nothing on
    /// screen is confirmed to have moved.
    Painting(Option<String>),
    /// Nothing answered on the socket. There is no screen to be wrong
    /// about; the next command reads the key.
    NoDaemon,
}

/// Tell a running cyclopsd the theme key moved: the same `theme.reload`
/// nudge `cyclops theme <name>` sends. Takes no theme name on purpose,
/// the daemon re-resolves the selection itself.
pub fn theme_reload(home: &Path) -> ThemeReload {
    let Ok(mut reader) = connect(home) else {
        return ThemeReload::NoDaemon;
    };
    match exchange(&mut reader, "theme.reload", json!({})) {
        Ok(result) => ThemeReload::Painting(
            result
                .get("theme")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        Err(_) => ThemeReload::Painting(None),
    }
}

/// Assign the pane's Cyclops identity. Detection remains the daemon's job;
/// omitting `manifest` preserves the CLI's normal auto-detection behavior.
pub fn label_pane(home: &Path, pane_id: &str, label: &str) -> Result<(), String> {
    if let Some(why) = cyclops_proto::label::refusal(label) {
        return Err(why);
    }
    request(
        home,
        "pane.label",
        json!({"target": pane_id, "label": label, "manifest": Value::Null}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn response_lines_stop_before_allocating_past_the_envelope() {
        let oversized = vec![b'x'; DAEMON_LINE_MAX_BYTES + 1];
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(oversized));
        let error = read_bounded_line(&mut reader, "test response")
            .expect_err("an oversized response must be refused");
        assert!(error.contains("exceeded"), "{error}");
        assert!(reader.buffer().len() <= DAEMON_LINE_MAX_BYTES);
    }

    #[test]
    fn request_consumes_the_mandatory_hello_before_the_response() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-daemon-hello");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let socket = home.join(SOCK_NAME);
        let listener = UnixListener::bind(&socket).expect("listen");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut request = String::new();
            reader.read_line(&mut request).expect("request");
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["method"],
                "ping"
            );
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"pong\":true}}\n")
                .expect("response");
        });

        let response = request(&home, "ping", json!({})).expect("request succeeds");
        assert_eq!(response["pong"], true);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_lost_acceptance_response_retries_to_the_original_message() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-send-idempotent-retry");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener = UnixListener::bind(home.join(SOCK_NAME)).expect("listen");
        let server = std::thread::spawn(move || {
            let mut accepted_key = None;
            for attempt in 0..2 {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream);
                reader
                    .get_mut()
                    .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                    .expect("hello");
                let mut line = String::new();
                reader.read_line(&mut line).expect("request");
                let request: Value = serde_json::from_str(&line).expect("JSON request");
                assert_eq!(request["method"], "msg.send");
                let key = request["params"]["client_key"]
                    .as_str()
                    .expect("client key")
                    .to_string();

                if attempt == 0 {
                    accepted_key = Some(key);
                    continue;
                }
                assert_eq!(Some(&key), accepted_key.as_ref());
                reader
                    .get_mut()
                    .write_all(
                        b"{\"id\":1,\"result\":{\"msg_id\":\"m-original\",\"seq\":7,\"deliveries\":[],\"inserted\":false}}\n",
                    )
                    .expect("retry response");
            }
            accepted_key.expect("first request was accepted")
        });

        let first = send_message(
            &home,
            "reviewer",
            "ship it",
            "ship it",
            "workspace-stable-key",
        );
        assert!(matches!(first, SendOutcome::Unknown(_)), "{first:?}");
        let retry = send_message(
            &home,
            "reviewer",
            "ship it",
            "ship it",
            "workspace-stable-key",
        );
        assert_eq!(
            retry,
            SendOutcome::Accepted("already accepted m-original".to_string())
        );
        assert_eq!(server.join().expect("server"), "workspace-stable-key");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_daemon_refusal_is_rejected_not_outcome_unknown() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-send-rejected");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener = UnixListener::bind(home.join(SOCK_NAME)).expect("listen");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            reader
                .get_mut()
                .write_all(
                    b"{\"id\":1,\"error\":{\"code\":\"unknown_recipient\",\"message\":\"no such recipient\"}}\n",
                )
                .expect("response");
        });

        let outcome = send_message(&home, "missing", "hello", "hello", "workspace-rejected-key");
        assert_eq!(
            outcome,
            SendOutcome::Rejected(DaemonRefusal::new("unknown_recipient", "no such recipient"))
        );
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn theme_reload_reads_what_the_daemon_says_it_is_painting() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-theme-reload");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener = UnixListener::bind(home.join(SOCK_NAME)).expect("listen");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            let request: Value = serde_json::from_str(&line).expect("JSON request");
            assert_eq!(request["method"], "theme.reload");
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"theme\":\"solar\"}}\n")
                .expect("response");
        });

        assert_eq!(
            theme_reload(&home),
            ThemeReload::Painting(Some("solar".into()))
        );
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn theme_reload_with_nothing_on_the_socket_is_no_daemon() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-theme-reload-down");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        assert_eq!(theme_reload(&home), ThemeReload::NoDaemon);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn pane_labels_fail_before_io_when_the_name_is_reserved() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-bad-label");
        let error = label_pane(&home, "%1", "admin").expect_err("reserved label");
        assert!(error.contains("you"), "{error}");
    }

    #[test]
    fn pane_label_uses_the_daemons_addressing_method() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-pane-label");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener = UnixListener::bind(home.join(SOCK_NAME)).expect("listen");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            let request: Value = serde_json::from_str(&line).expect("JSON request");
            assert_eq!(request["method"], "pane.label");
            assert_eq!(request["params"]["target"], "%7");
            assert_eq!(request["params"]["label"], "reviewer");
            assert!(request["params"]["manifest"].is_null());
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"labeled\":true}}\n")
                .expect("response");
        });

        label_pane(&home, "%7", "reviewer").expect("label succeeds");
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(home);
    }
}
