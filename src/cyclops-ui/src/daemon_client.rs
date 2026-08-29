//! One official client contract for the Cyclops daemon.
//!
//! Blocking CLI and workspace callers and async stream callers differ in how
//! they wait, not in what socket facts mean. This module owns Hello-first
//! connection, bounded NDJSON, request correlation, event buffering, refusal
//! decoding, post-write uncertainty, and subscription gap classification.
//! Callers choose timeout and reconnect policy and decode domain results.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream as BlockingUnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use cyclops_proto::{Event, FrameContract, FrameSize, Hello};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::UnixStream as AsyncUnixStream;

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// What an official caller knows after a client failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    KnownNotSent,
    Refused,
    OutcomeUnknown,
    StreamGap,
}

/// Shared transport outcomes. Presentation belongs to callers.
#[derive(Debug)]
pub enum ClientError {
    NotRunning(String),
    ConnectTimeout(Duration),
    HelloTimeout(Duration),
    ReadTimeout(Duration),
    RequestFrameTooLarge,
    DaemonFrameTooLarge,
    /// The daemon substituted a bounded error because the real response did
    /// not fit. The request may already have taken effect.
    OversizedResponse(String),
    InvalidHello(String),
    Server {
        code: String,
        message: String,
        targets: Vec<String>,
        data: Value,
    },
    NotSent(String),
    Unknown(String),
    Gap(String),
}

impl ClientError {
    pub fn certainty(&self) -> Certainty {
        match self {
            Self::NotRunning(_)
            | Self::ConnectTimeout(_)
            | Self::HelloTimeout(_)
            | Self::RequestFrameTooLarge
            | Self::InvalidHello(_)
            | Self::NotSent(_) => Certainty::KnownNotSent,
            Self::Server { .. } => Certainty::Refused,
            Self::ReadTimeout(_)
            | Self::DaemonFrameTooLarge
            | Self::OversizedResponse(_)
            | Self::Unknown(_) => Certainty::OutcomeUnknown,
            Self::Gap(_) => Certainty::StreamGap,
        }
    }

    /// A presentation-neutral cause for callers that already have their own
    /// outcome vocabulary.
    pub fn cause(&self) -> String {
        match self {
            Self::NotRunning(cause) => cause.clone(),
            Self::ConnectTimeout(waited) => {
                format!("connection did not open within {}s", waited.as_secs_f64())
            }
            Self::HelloTimeout(waited) => {
                format!("hello did not arrive within {}s", waited.as_secs_f64())
            }
            Self::ReadTimeout(waited) => {
                format!("no answer within {}s", waited.as_secs_f64())
            }
            Self::RequestFrameTooLarge => format!(
                "request exceeds the {}-byte JSON frame limit",
                FrameContract::MAX_JSON_BYTES
            ),
            Self::DaemonFrameTooLarge => format!(
                "daemon frame exceeds the {}-byte JSON frame limit",
                FrameContract::MAX_JSON_BYTES
            ),
            Self::OversizedResponse(message) => message.clone(),
            Self::InvalidHello(cause) => format!("invalid daemon hello: {cause}"),
            Self::Server { message, .. }
            | Self::NotSent(message)
            | Self::Unknown(message)
            | Self::Gap(message) => message.clone(),
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.cause())
    }
}

impl std::error::Error for ClientError {}

/// One verified daemon event plus its original frame for JSON passthrough.
#[derive(Debug, Clone)]
pub struct EventFrame {
    pub raw: Vec<u8>,
    pub event: Event,
}

impl EventFrame {
    pub fn raw_text(&self) -> &str {
        // Construction validates UTF-8 before this type exists.
        std::str::from_utf8(&self.raw).expect("event frame was validated as UTF-8")
    }
}

enum Incoming {
    Event(EventFrame),
    Result(Value),
}

fn decode_incoming(
    frame: Vec<u8>,
    expected_id: u64,
    method: &str,
) -> Result<Incoming, ClientError> {
    let value: Value = serde_json::from_slice(&frame)
        .map_err(|error| ClientError::Unknown(format!("malformed {method} answer: {error}")))?;
    if value.get("event").is_some() {
        let event = serde_json::from_value::<Event>(value)
            .map_err(|error| ClientError::Unknown(format!("malformed daemon event: {error}")))?;
        std::str::from_utf8(&frame)
            .map_err(|_| ClientError::Unknown("a daemon event was not UTF-8".into()))?;
        return Ok(Incoming::Event(EventFrame { raw: frame, event }));
    }
    if value.get("id") != Some(&json!(expected_id)) {
        return Err(ClientError::Unknown(format!(
            "{method} returned the wrong response id"
        )));
    }
    if let Some(error) = value.get("error") {
        let code = string_field(error, "code");
        let message = string_field(error, "message");
        if code == FrameContract::TOO_LARGE_CODE {
            return Err(ClientError::OversizedResponse(message));
        }
        return Err(ClientError::Server {
            code,
            message,
            targets: error
                .get("targets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            data: error.get("data").cloned().unwrap_or(Value::Null),
        });
    }
    value
        .get("result")
        .cloned()
        .map(Incoming::Result)
        .ok_or_else(|| ClientError::Unknown(format!("{method} returned no result")))
}

fn decode_event(frame: Vec<u8>) -> Result<EventFrame, ClientError> {
    let value: Value = serde_json::from_slice(&frame)
        .map_err(|error| ClientError::Gap(format!("malformed event frame: {error}")))?;
    if value.get("event").is_none() {
        return Err(ClientError::Gap(
            "daemon event stream sent a non-event record".into(),
        ));
    }
    let event = serde_json::from_value::<Event>(value)
        .map_err(|error| ClientError::Gap(format!("unreadable daemon event: {error}")))?;
    std::str::from_utf8(&frame)
        .map_err(|_| ClientError::Gap("a daemon event was not UTF-8".into()))?;
    Ok(EventFrame { raw: frame, event })
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

struct BoundedJson {
    bytes: Vec<u8>,
    oversized: bool,
}

impl BoundedJson {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(8 * 1024),
            oversized: false,
        }
    }
}

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if matches!(
            FrameContract::classify_json_bytes(self.bytes.len().saturating_add(bytes.len())),
            FrameSize::TooLarge
        ) {
            self.oversized = true;
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "official daemon frame is too large",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_request(id: u64, method: &str, params: Value) -> Result<Vec<u8>, ClientError> {
    let mut writer = BoundedJson::new();
    let request = json!({"id": id, "method": method, "params": params});
    match serde_json::to_writer(&mut writer, &request) {
        Ok(()) => {
            writer.bytes.push(FrameContract::DELIMITER);
            Ok(writer.bytes)
        }
        Err(_) if writer.oversized => Err(ClientError::RequestFrameTooLarge),
        Err(error) => Err(ClientError::NotSent(format!(
            "cannot encode {method} request: {error}"
        ))),
    }
}

fn read_blocking_frame(
    reader: &mut impl BufRead,
    timeout: Option<Duration>,
) -> Result<Option<Vec<u8>>, ClientError> {
    let mut frame = Vec::with_capacity(8 * 1024);
    loop {
        let available = reader.fill_buf().map_err(|error| {
            if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                timeout
                    .map(ClientError::ReadTimeout)
                    .unwrap_or_else(|| ClientError::Gap(error.to_string()))
            } else {
                ClientError::Unknown(error.to_string())
            }
        })?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(ClientError::Unknown(
                    "the connection closed during a daemon frame".into(),
                ))
            };
        }
        if let Some(delimiter) = available
            .iter()
            .position(|byte| *byte == FrameContract::DELIMITER)
        {
            if matches!(
                FrameContract::classify_json_bytes(frame.len().saturating_add(delimiter)),
                FrameSize::TooLarge
            ) {
                return Err(ClientError::DaemonFrameTooLarge);
            }
            frame.extend_from_slice(&available[..delimiter]);
            reader.consume(delimiter + 1);
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
        if matches!(
            FrameContract::classify_json_bytes(frame.len().saturating_add(available.len())),
            FrameSize::TooLarge
        ) {
            return Err(ClientError::DaemonFrameTooLarge);
        }
        let consumed = available.len();
        frame.extend_from_slice(available);
        reader.consume(consumed);
    }
}

/// Blocking adapter used by the CLI and workspace background threads.
pub struct BlockingClient {
    reader: BufReader<BlockingUnixStream>,
    hello: Hello,
    next_id: u64,
    read_timeout: Option<Duration>,
    pending: VecDeque<EventFrame>,
}

/// A connected daemon socket whose Hello has not yet been read.
///
/// Most callers should use [`BlockingClient::connect`]. The hook adapter uses
/// this phase capability so its one outer deadline can budget connection and
/// greeting separately without learning the raw socket type or Hello framing.
pub struct BlockingHello {
    stream: BlockingUnixStream,
}

impl BlockingHello {
    pub fn receive(self, read: Duration) -> Result<BlockingClient, ClientError> {
        BlockingClient::from_stream(self.stream, read)
    }
}

impl BlockingClient {
    pub fn connect() -> Result<Self, ClientError> {
        Self::connect_path(
            cyclops_proto::socket_path(),
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_READ_TIMEOUT,
        )
    }

    pub fn connect_with_timeouts(connect: Duration, read: Duration) -> Result<Self, ClientError> {
        Self::connect_path(cyclops_proto::socket_path(), connect, read)
    }

    pub fn connect_for_hello(connect: Duration) -> Result<BlockingHello, ClientError> {
        Ok(BlockingHello {
            stream: Self::connect_stream_path(cyclops_proto::socket_path(), connect)?,
        })
    }

    pub fn connect_path(
        path: impl Into<PathBuf>,
        connect: Duration,
        read: Duration,
    ) -> Result<Self, ClientError> {
        Self::from_stream(Self::connect_stream_path(path, connect)?, read)
    }

    fn connect_stream_path(
        path: impl Into<PathBuf>,
        connect: Duration,
    ) -> Result<BlockingUnixStream, ClientError> {
        let path = path.into();
        let (tx, rx) = std_mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(BlockingUnixStream::connect(path));
        });
        match rx.recv_timeout(connect) {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(error)) => Err(match error.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
                    ClientError::NotRunning(error.to_string())
                }
                _ => ClientError::NotSent(error.to_string()),
            }),
            Err(_) => Err(ClientError::ConnectTimeout(connect)),
        }
    }

    fn from_stream(stream: BlockingUnixStream, read: Duration) -> Result<Self, ClientError> {
        stream
            .set_read_timeout(Some(read))
            .map_err(|error| ClientError::NotSent(error.to_string()))?;
        stream
            .set_write_timeout(Some(read))
            .map_err(|error| ClientError::NotSent(error.to_string()))?;
        let mut reader = BufReader::new(stream);
        let frame = match read_blocking_frame(&mut reader, Some(read)) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                return Err(ClientError::NotSent(
                    "the connection closed before hello".into(),
                ))
            }
            Err(ClientError::ReadTimeout(_)) => return Err(ClientError::HelloTimeout(read)),
            Err(ClientError::DaemonFrameTooLarge) => {
                return Err(ClientError::NotSent(format!(
                    "daemon hello exceeds the {}-byte JSON frame limit",
                    FrameContract::MAX_JSON_BYTES
                )))
            }
            Err(error) => return Err(ClientError::NotSent(error.cause())),
        };
        let hello = serde_json::from_slice(&frame)
            .map_err(|error| ClientError::InvalidHello(error.to_string()))?;
        Ok(Self {
            reader,
            hello,
            next_id: 1,
            read_timeout: Some(read),
            pending: VecDeque::new(),
        })
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = encode_request(id, method, params)?;
        write_blocking_request(self.reader.get_mut(), method, &request)?;
        loop {
            let frame = match read_blocking_frame(&mut self.reader, self.read_timeout)? {
                Some(frame) => frame,
                None => {
                    return Err(ClientError::Unknown(format!(
                        "the connection closed before {method} answered"
                    )))
                }
            };
            match decode_incoming(frame, id, method)? {
                Incoming::Event(event) => self.pending.push_back(event),
                Incoming::Result(result) => return Ok(result),
            }
        }
    }

    pub fn subscribe(&mut self, params: Value) -> Result<(), ClientError> {
        let result = self.request("events.subscribe", params)?;
        if result.get("subscribed") == Some(&Value::Bool(true)) {
            Ok(())
        } else {
            Err(ClientError::Unknown(
                "events.subscribe returned no acknowledgement".into(),
            ))
        }
    }

    pub fn next_event(&mut self) -> Result<EventFrame, ClientError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        let frame = match read_blocking_frame(&mut self.reader, self.read_timeout) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Err(ClientError::Gap("the connection closed".into())),
            Err(ClientError::ReadTimeout(waited)) => return Err(ClientError::ReadTimeout(waited)),
            Err(ClientError::DaemonFrameTooLarge) => {
                return Err(ClientError::Gap(format!(
                    "daemon event exceeds the {}-byte JSON frame limit",
                    FrameContract::MAX_JSON_BYTES
                )))
            }
            Err(error) => return Err(ClientError::Gap(error.cause())),
        };
        decode_event(frame)
    }

    /// Remove the event-stream read deadline on a best-effort basis.
    ///
    /// On macOS, `setsockopt(SO_RCVTIMEO)` can return `EINVAL` after the peer
    /// closes (F18). Buffered frames remain readable and the next read reports
    /// the close, so surfacing this setter error would discard better evidence.
    pub fn clear_read_timeout(&mut self) {
        let _ = self.reader.get_ref().set_read_timeout(None);
        self.read_timeout = None;
    }

    /// Replace the active read deadline on the same best-effort basis as
    /// [`Self::clear_read_timeout`].
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        let _ = self.reader.get_ref().set_read_timeout(Some(timeout));
        self.read_timeout = Some(timeout);
    }
}

fn write_blocking_request(
    writer: &mut impl Write,
    method: &str,
    request: &[u8],
) -> Result<(), ClientError> {
    writer
        .write_all(request)
        .map_err(|error| ClientError::Unknown(format!("cannot write {method}: {error}")))
}

struct AsyncFrameReader<R> {
    inner: AsyncBufReader<R>,
    frame: Vec<u8>,
}

impl<R: AsyncRead + Unpin> AsyncFrameReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: AsyncBufReader::new(inner),
            frame: Vec::with_capacity(8 * 1024),
        }
    }

    async fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.frame.clear();
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return if self.frame.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "daemon frame ended without a newline",
                    ))
                };
            }
            if let Some(delimiter) = available
                .iter()
                .position(|byte| *byte == FrameContract::DELIMITER)
            {
                if matches!(
                    FrameContract::classify_json_bytes(self.frame.len().saturating_add(delimiter)),
                    FrameSize::TooLarge
                ) {
                    return Err(frame_too_large());
                }
                self.frame.extend_from_slice(&available[..delimiter]);
                self.inner.consume(delimiter + 1);
                if self.frame.last() == Some(&b'\r') {
                    self.frame.pop();
                }
                return Ok(Some(std::mem::take(&mut self.frame)));
            }
            if matches!(
                FrameContract::classify_json_bytes(
                    self.frame.len().saturating_add(available.len())
                ),
                FrameSize::TooLarge
            ) {
                return Err(frame_too_large());
            }
            let consumed = available.len();
            self.frame.extend_from_slice(available);
            self.inner.consume(consumed);
        }
    }
}

fn frame_too_large() -> io::Error {
    io::Error::new(ErrorKind::InvalidData, "official daemon frame is too large")
}

async fn receive_async_hello<R: AsyncRead + Unpin>(
    read: R,
    deadline: tokio::time::Instant,
    open: Duration,
) -> Result<(AsyncFrameReader<R>, Hello), ClientError> {
    let mut frames = AsyncFrameReader::new(read);
    let frame = tokio::time::timeout_at(deadline, frames.next_frame())
        .await
        .map_err(|_| ClientError::HelloTimeout(open))?
        .map_err(|error| {
            if error.kind() == ErrorKind::InvalidData {
                ClientError::NotSent(format!(
                    "daemon hello exceeds the {}-byte JSON frame limit",
                    FrameContract::MAX_JSON_BYTES
                ))
            } else {
                ClientError::NotSent(format!("hello: {error}"))
            }
        })?
        .ok_or_else(|| ClientError::NotSent("the connection closed before hello".into()))?;
    let hello = serde_json::from_slice(&frame)
        .map_err(|error| ClientError::InvalidHello(error.to_string()))?;
    Ok((frames, hello))
}

async fn write_async_request(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    method: &str,
    request: &[u8],
) -> Result<(), ClientError> {
    writer
        .write_all(request)
        .await
        .map_err(|error| ClientError::Unknown(format!("cannot write {method}: {error}")))
}

/// Async adapter used by stream and interactive actions.
pub struct AsyncClient {
    frames: AsyncFrameReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    hello: Hello,
    next_id: u64,
    pending: VecDeque<EventFrame>,
}

impl AsyncClient {
    pub async fn connect(path: &Path, open: Duration) -> Result<Self, ClientError> {
        // This is the caller's one pre-write deadline. Connecting and reading
        // Hello are distinct certainty phases, but they do not each get a
        // fresh copy of the budget.
        let deadline = tokio::time::Instant::now() + open;
        let stream = tokio::time::timeout_at(deadline, AsyncUnixStream::connect(path))
            .await
            .map_err(|_| ClientError::ConnectTimeout(open))?
            .map_err(|error| match error.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
                    ClientError::NotRunning(error.to_string())
                }
                _ => ClientError::NotSent(error.to_string()),
            })?;
        let (read, writer) = stream.into_split();
        let (frames, hello) = receive_async_hello(read, deadline, open).await?;
        Ok(Self {
            frames,
            writer,
            hello,
            next_id: 1,
            pending: VecDeque::new(),
        })
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        answer: Duration,
    ) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = encode_request(id, method, params)?;
        tokio::time::timeout(answer, async {
            write_async_request(&mut self.writer, method, &request).await?;
            loop {
                let frame = self
                    .frames
                    .next_frame()
                    .await
                    .map_err(|error| {
                        if error.kind() == ErrorKind::InvalidData {
                            ClientError::DaemonFrameTooLarge
                        } else {
                            ClientError::Unknown(format!("cannot read {method}: {error}"))
                        }
                    })?
                    .ok_or_else(|| {
                        ClientError::Unknown(format!(
                            "the connection closed before {method} answered"
                        ))
                    })?;
                match decode_incoming(frame, id, method)? {
                    Incoming::Event(event) => self.pending.push_back(event),
                    Incoming::Result(result) => return Ok(result),
                }
            }
        })
        .await
        .map_err(|_| ClientError::ReadTimeout(answer))?
    }

    pub async fn subscribe(&mut self, params: Value, answer: Duration) -> Result<(), ClientError> {
        let result = self.request("events.subscribe", params, answer).await?;
        if result.get("subscribed") == Some(&Value::Bool(true)) {
            Ok(())
        } else {
            Err(ClientError::Unknown(
                "events.subscribe returned no acknowledgement".into(),
            ))
        }
    }

    pub async fn next_event(&mut self) -> Result<EventFrame, ClientError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        let frame = self
            .frames
            .next_frame()
            .await
            .map_err(|error| {
                if error.kind() == ErrorKind::InvalidData {
                    ClientError::Gap(format!(
                        "daemon event exceeds the {}-byte JSON frame limit",
                        FrameContract::MAX_JSON_BYTES
                    ))
                } else {
                    ClientError::Gap(format!("daemon event read failed: {error}"))
                }
            })?
            .ok_or_else(|| ClientError::Gap("the connection closed".into()))?;
        decode_event(frame)
    }
}

#[cfg(test)]
mod tests {
    //! Shared-client contract evidence. This family becomes obsolete when the
    //! official protocol stops being newline-delimited request/response plus
    //! events, or when one mechanically shared IO adapter replaces both the
    //! blocking and async paths and carries equivalent conformance evidence.

    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    struct PartialAsyncWriter {
        remaining: usize,
    }

    struct PartialBlockingWriter {
        remaining: usize,
    }

    impl Write for PartialBlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "simulated socket failure",
                ));
            }
            let written = self.remaining.min(bytes.len());
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl tokio::io::AsyncWrite for PartialAsyncWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "simulated socket failure",
                )));
            }
            let written = self.remaining.min(bytes.len());
            self.remaining -= written;
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn certainty_is_one_shared_rule() {
        assert_eq!(
            ClientError::NotRunning("missing".into()).certainty(),
            Certainty::KnownNotSent
        );
        assert_eq!(
            ClientError::RequestFrameTooLarge.certainty(),
            Certainty::KnownNotSent
        );
        assert_eq!(
            ClientError::Server {
                code: "denied".into(),
                message: "no".into(),
                targets: Vec::new(),
                data: Value::Null,
            }
            .certainty(),
            Certainty::Refused
        );
        assert_eq!(
            ClientError::Unknown("closed".into()).certainty(),
            Certainty::OutcomeUnknown
        );
        assert_eq!(
            ClientError::Gap("closed".into()).certainty(),
            Certainty::StreamGap
        );
    }

    #[test]
    fn bounded_oversized_response_keeps_uncertainty_and_daemon_message() {
        let frame = serde_json::to_vec(&json!({
            "id": 7,
            "error": {
                "code": FrameContract::TOO_LARGE_CODE,
                "message": "request exceeds the official frame limit"
            }
        }))
        .unwrap();
        let error = match decode_incoming(frame, 7, "message.send") {
            Err(error) => error,
            Ok(_) => panic!("the daemon refusal must not decode as a result"),
        };
        assert!(matches!(
            error,
            ClientError::OversizedResponse(message)
                if message == "request exceeds the official frame limit"
        ));
    }

    #[test]
    fn blocking_request_refuses_oversize_before_socket_write() {
        let (client_stream, mut daemon_stream) = BlockingUnixStream::pair().unwrap();
        daemon_stream
            .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"test\"}\n")
            .unwrap();
        daemon_stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let daemon = thread::spawn(move || {
            let mut received = Vec::new();
            let _ = daemon_stream.read_to_end(&mut received);
            received
        });
        let mut client =
            BlockingClient::from_stream(client_stream, Duration::from_millis(20)).unwrap();
        let error = client
            .request(
                "ping",
                json!({"padding": "x".repeat(FrameContract::MAX_JSON_BYTES)}),
            )
            .unwrap_err();
        assert!(matches!(error, ClientError::RequestFrameTooLarge));
        drop(client);
        assert!(daemon.join().unwrap().is_empty());
    }

    #[test]
    fn blocking_contract_correlates_buffers_refusals_and_gaps() {
        let (client_stream, daemon_stream) = BlockingUnixStream::pair().unwrap();
        let daemon = thread::spawn(move || {
            let mut daemon = BufReader::new(daemon_stream);
            daemon
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"test\"}\n")
                .unwrap();

            let mut request = String::new();
            daemon.read_line(&mut request).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["id"],
                json!(1)
            );
            daemon
                .get_mut()
                .write_all(
                    b"{\"event\":\"state\",\"data\":{\"agent\":\"reviewer\"}}\n\
                      {\"id\":1,\"result\":{\"ok\":true}}\n",
                )
                .unwrap();

            request.clear();
            daemon.read_line(&mut request).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["id"],
                json!(2)
            );
            daemon
                .get_mut()
                .write_all(
                    b"{\"id\":2,\"error\":{\"code\":\"denied\",\"message\":\"no\",\"targets\":[\"reviewer\"],\"data\":{\"reason\":\"held\"}}}\n",
                )
                .unwrap();

            request.clear();
            daemon.read_line(&mut request).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["id"],
                json!(3)
            );
            daemon
                .get_mut()
                .write_all(b"{\"id\":3,\"result\":{\"subscribed\":true}}\n")
                .unwrap();
        });

        let mut client =
            BlockingClient::from_stream(client_stream, Duration::from_secs(1)).unwrap();
        assert_eq!(
            client.request("ping", json!({})).unwrap(),
            json!({"ok": true})
        );
        let buffered = client.next_event().unwrap();
        assert_eq!(buffered.event.event, "state");
        assert_eq!(buffered.event.data["agent"], json!("reviewer"));

        let refusal = client.request("deny", json!({})).unwrap_err();
        assert_eq!(refusal.certainty(), Certainty::Refused);
        assert!(matches!(
            refusal,
            ClientError::Server { targets, .. } if targets == vec!["reviewer"]
        ));

        client.subscribe(json!({})).unwrap();
        daemon.join().unwrap();
        assert!(matches!(
            client.next_event(),
            Err(error) if error.certainty() == Certainty::StreamGap
        ));
    }

    #[test]
    fn blocking_close_after_request_write_is_outcome_unknown() {
        let (client_stream, daemon_stream) = BlockingUnixStream::pair().unwrap();
        let daemon = thread::spawn(move || {
            let mut daemon = BufReader::new(daemon_stream);
            daemon
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"test\"}\n")
                .unwrap();
            let mut request = String::new();
            daemon.read_line(&mut request).unwrap();
            assert!(!request.is_empty());
        });
        let mut client =
            BlockingClient::from_stream(client_stream, Duration::from_secs(1)).unwrap();
        let error = client.request("maybe", json!({})).unwrap_err();
        assert_eq!(error.certainty(), Certainty::OutcomeUnknown);
        daemon.join().unwrap();
    }

    #[test]
    fn blocking_partial_write_is_outcome_unknown() {
        let mut writer = PartialBlockingWriter { remaining: 8 };
        let error = write_blocking_request(&mut writer, "ping", b"0123456789\n").unwrap_err();
        assert!(matches!(
            error,
            ClientError::Unknown(message) if message.contains("simulated socket failure")
        ));
    }

    #[tokio::test]
    async fn async_partial_write_is_outcome_unknown() {
        let mut writer = PartialAsyncWriter { remaining: 8 };
        let error = write_async_request(&mut writer, "ping", b"0123456789\n")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Unknown(message) if message.contains("simulated socket failure")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn async_connect_and_hello_consume_one_open_deadline() {
        let open = Duration::from_secs(10);
        let deadline = tokio::time::Instant::now() + open;
        let (read, _daemon) = tokio::io::duplex(64);

        // Model a connect phase that already consumed most of the one caller
        // budget, then prove Hello receives only what remains.
        tokio::time::advance(Duration::from_secs(7)).await;
        let receive = tokio::spawn(receive_async_hello(read, deadline, open));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;

        assert!(matches!(
            receive.await.unwrap(),
            Err(ClientError::HelloTimeout(waited)) if waited == open
        ));
    }

    #[tokio::test]
    async fn async_contract_correlates_buffers_refusals_and_gaps() {
        let root = cyclops_proto::scratch::scratch_dir("daemon-client-async-contract");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut requests = tokio::io::BufReader::new(read).lines();
            write
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"test\"}\n")
                .await
                .unwrap();

            let request = requests.next_line().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["id"],
                json!(1)
            );
            write
                .write_all(
                    b"{\"event\":\"state\",\"data\":{\"agent\":\"reviewer\"}}\n\
                      {\"id\":1,\"result\":{\"ok\":true}}\n",
                )
                .await
                .unwrap();

            let request = requests.next_line().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["id"],
                json!(2)
            );
            write
                .write_all(
                    b"{\"id\":2,\"error\":{\"code\":\"denied\",\"message\":\"no\",\"targets\":[\"reviewer\"],\"data\":{\"reason\":\"held\"}}}\n",
                )
                .await
                .unwrap();

            let request = requests.next_line().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["id"],
                json!(3)
            );
            write
                .write_all(b"{\"id\":3,\"result\":{\"subscribed\":true}}\n")
                .await
                .unwrap();
        });

        let mut client = AsyncClient::connect(&socket, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            client
                .request("ping", json!({}), Duration::from_secs(1))
                .await
                .unwrap(),
            json!({"ok": true})
        );
        let buffered = client.next_event().await.unwrap();
        assert_eq!(buffered.event.event, "state");
        assert_eq!(buffered.event.data["agent"], json!("reviewer"));

        let refusal = client
            .request("deny", json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(refusal.certainty(), Certainty::Refused);
        assert!(matches!(
            refusal,
            ClientError::Server { targets, .. } if targets == vec!["reviewer"]
        ));

        client
            .subscribe(json!({}), Duration::from_secs(1))
            .await
            .unwrap();
        daemon.await.unwrap();
        assert!(matches!(
            client.next_event().await,
            Err(error) if error.certainty() == Certainty::StreamGap
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn async_close_after_request_write_is_outcome_unknown() {
        let root = cyclops_proto::scratch::scratch_dir("daemon-client-async-close");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut requests = tokio::io::BufReader::new(read).lines();
            write
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"test\"}\n")
                .await
                .unwrap();
            assert!(requests.next_line().await.unwrap().is_some());
        });
        let mut client = AsyncClient::connect(&socket, Duration::from_secs(1))
            .await
            .unwrap();
        let error = client
            .request("maybe", json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.certainty(), Certainty::OutcomeUnknown);
        daemon.await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn async_frames_share_the_exact_newline_excluded_boundary() {
        let mut exact = vec![b'x'; FrameContract::MAX_JSON_BYTES];
        exact.push(FrameContract::DELIMITER);
        let mut reader = AsyncFrameReader::new(std::io::Cursor::new(exact));
        assert_eq!(
            reader.next_frame().await.unwrap().unwrap().len(),
            FrameContract::MAX_JSON_BYTES
        );

        let mut oversized = vec![b'x'; FrameContract::MAX_JSON_BYTES + 1];
        oversized.push(FrameContract::DELIMITER);
        let mut reader = AsyncFrameReader::new(std::io::Cursor::new(oversized));
        assert_eq!(
            reader.next_frame().await.unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn blocking_frames_share_the_exact_newline_excluded_boundary() {
        let mut exact = vec![b'x'; FrameContract::MAX_JSON_BYTES];
        exact.push(FrameContract::DELIMITER);
        let mut reader = BufReader::new(std::io::Cursor::new(exact));
        assert_eq!(
            read_blocking_frame(&mut reader, None)
                .unwrap()
                .unwrap()
                .len(),
            FrameContract::MAX_JSON_BYTES
        );

        let mut oversized = vec![b'x'; FrameContract::MAX_JSON_BYTES + 1];
        oversized.push(FrameContract::DELIMITER);
        let mut reader = BufReader::new(std::io::Cursor::new(oversized));
        assert!(matches!(
            read_blocking_frame(&mut reader, None),
            Err(ClientError::DaemonFrameTooLarge)
        ));
    }
}
