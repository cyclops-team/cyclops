//! Blocking NDJSON transport to cyclopsd over its Unix socket.
//!
//! std only, no async runtime: the CLI makes one request and exits, or
//! tails one subscription. The server writes a Hello line first on every
//! connection (shpool pattern S2); a version mismatch is the caller's cue
//! to warn, never a reason to disconnect.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use cyclops_proto::{FrameContract, FrameSize, Hello};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub enum ClientError {
    /// Nothing listening at the socket path.
    NotRunning,
    /// Connect did not finish inside the carried budget.
    ConnectTimeout(Duration),
    /// A bounded socket read reached its caller-supplied deadline.
    ReadTimeout(Duration),
    /// The request exceeded the official frame envelope before any socket
    /// byte was written.
    RequestFrameTooLarge,
    /// The daemon sent a hello, response, or event outside the official frame
    /// envelope.
    DaemonFrameTooLarge,
    /// The daemon answered a request with a wire error.
    Server {
        code: String,
        message: String,
        /// Known targets, read tolerantly from the error object when the
        /// daemon includes them. Feeds the unknown-target copy.
        targets: Vec<String>,
        /// Structured extras some codes carry (agent.wait's timeout and
        /// occupant_changed report the state). Null when absent.
        data: Value,
    },
    /// The connection died or carried something that is not NDJSON.
    /// The payload is a short human cause, not a next step.
    Broken(String),
}

pub struct Client {
    reader: BufReader<UnixStream>,
    hello: Hello,
    next_id: u64,
    /// Active read deadline, kept so timeout errors can name it honestly.
    read_timeout: Option<Duration>,
    /// Event lines that arrived while a response was pending. Buffered so
    /// a subscribe race drops nothing; next_line drains these first.
    pending: VecDeque<String>,
}

impl Client {
    /// Connect with the interactive defaults: 2s connect, 5s reads.
    pub fn connect() -> Result<Self, ClientError> {
        Self::connect_with_timeouts(CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// Connect to cyclops_proto::socket_path() and read the Hello line.
    ///
    /// UnixStream::connect has no timeout parameter, so the connect runs on
    /// a helper thread and we wait on a channel. Unix sockets normally
    /// connect instantly; the timeout catches a daemon with a full accept
    /// backlog. An abandoned helper thread costs nothing here because the
    /// process exits right after the error path.
    ///
    /// The explicit-budget form exists for the hook receiver, which runs
    /// inside vendor hook time limits and cannot afford the defaults.
    pub fn connect_with_timeouts(connect: Duration, read: Duration) -> Result<Self, ClientError> {
        Self::from_stream(Self::connect_stream(connect)?, read)
    }

    /// Phase one of the handshake: the socket, within `connect`, and no
    /// Hello read. Split from [`Client::from_stream`] so a caller on a hard
    /// deadline can budget the Hello read from what is left AFTER the
    /// connect returned, instead of granting both phases up front.
    pub fn connect_stream(connect: Duration) -> Result<UnixStream, ClientError> {
        let path = cyclops_proto::socket_path();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(UnixStream::connect(path));
        });
        match rx.recv_timeout(connect) {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => Err(match e.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => ClientError::NotRunning,
                _ => ClientError::Broken(e.to_string()),
            }),
            Err(_) => Err(ClientError::ConnectTimeout(connect)),
        }
    }

    /// Phase two: read the Hello line within `read` on a connected stream.
    pub fn from_stream(stream: UnixStream, read: Duration) -> Result<Self, ClientError> {
        stream
            .set_read_timeout(Some(read))
            .map_err(|e| ClientError::Broken(e.to_string()))?;
        let mut reader = BufReader::new(stream);
        let line = match read_frame(&mut reader, Some(read))? {
            Some(line) => line,
            None => {
                return Err(ClientError::Broken(
                    "the connection closed before hello".into(),
                ))
            }
        };
        let hello: Hello = serde_json::from_str(&line)
            .map_err(|_| ClientError::Broken("the hello line didn't parse".into()))?;
        Ok(Client {
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

    /// Send one request and wait for its response. Event lines that arrive
    /// first are buffered for next_line, not dropped. Returns the result
    /// value; a wire error becomes ClientError::Server.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = encode_request(&json!({"id": id, "method": method, "params": params}))?;
        {
            let mut w = self.reader.get_ref();
            w.write_all(&line)
                .and_then(|_| w.write_all(&[FrameContract::DELIMITER]))
                .map_err(|e| ClientError::Broken(e.to_string()))?;
        }
        loop {
            let raw = self.raw_line()?;
            let v: Value = serde_json::from_str(&raw)
                .map_err(|_| ClientError::Broken("a reply line didn't parse".into()))?;
            if v.get("event").is_some() {
                self.pending.push_back(raw);
                continue;
            }
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                return Err(ClientError::Server {
                    code: str_field(err, "code"),
                    message: str_field(err, "message"),
                    targets: err
                        .get("targets")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    data: err.get("data").cloned().unwrap_or(Value::Null),
                });
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Next non-empty line from the daemon: buffered events first, then the
    /// socket. Trimmed.
    pub fn next_line(&mut self) -> Result<String, ClientError> {
        if let Some(l) = self.pending.pop_front() {
            return Ok(l);
        }
        self.raw_line()
    }

    /// Drop the read deadline for event streaming: watch blocks on the next
    /// event indefinitely, which is the point.
    ///
    /// Failure is swallowed on purpose. On macOS, setsockopt(SO_RCVTIMEO)
    /// returns EINVAL once the peer has closed (MEASURED against a canned
    /// server that hangs up right after writing). Buffered lines are still
    /// readable, and the next read reports the close honestly, so failing
    /// here would hide data and misname the error.
    pub fn clear_read_timeout(&mut self) {
        let _ = self.reader.get_ref().set_read_timeout(None);
        self.read_timeout = None;
    }

    /// Shrink or extend the read deadline mid-connection. The hook receiver
    /// sets this to its remaining budget before the one request it makes.
    /// Setter failure is swallowed for the same F18 reason as
    /// clear_read_timeout.
    pub fn set_read_timeout(&mut self, d: Duration) {
        let _ = self.reader.get_ref().set_read_timeout(Some(d));
        self.read_timeout = Some(d);
    }

    fn raw_line(&mut self) -> Result<String, ClientError> {
        loop {
            match read_frame(&mut self.reader, self.read_timeout)? {
                None => return Err(ClientError::Broken("the connection closed".into())),
                Some(line) => {
                    if !line.trim().is_empty() {
                        return Ok(line);
                    }
                }
            }
        }
    }
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

impl Write for BoundedJson {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if matches!(
            FrameContract::classify_json_bytes(self.bytes.len().saturating_add(buf.len())),
            FrameSize::TooLarge
        ) {
            self.oversized = true;
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
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

fn encode_request(value: &Value) -> Result<Vec<u8>, ClientError> {
    let mut writer = BoundedJson::new();
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(writer.bytes),
        Err(_) if writer.oversized => Err(ClientError::RequestFrameTooLarge),
        Err(error) => Err(ClientError::Broken(error.to_string())),
    }
}

/// Read one newline-terminated official frame without allocating beyond its
/// JSON-object envelope. The delimiter is consumed but is not counted.
fn read_frame<R: BufRead>(
    reader: &mut R,
    timeout: Option<Duration>,
) -> Result<Option<String>, ClientError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| match (error.kind(), timeout) {
                (ErrorKind::WouldBlock | ErrorKind::TimedOut, Some(duration)) => {
                    ClientError::ReadTimeout(duration)
                }
                _ => ClientError::Broken(error.to_string()),
            })?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(ClientError::Broken(
                    "the connection closed during a daemon frame".into(),
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
                return Err(ClientError::DaemonFrameTooLarge);
            }
            bytes.extend_from_slice(&available[..delimiter]);
            reader.consume(delimiter + 1);
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| ClientError::Broken("a daemon frame wasn't UTF-8".into()));
        }
        if matches!(
            FrameContract::classify_json_bytes(bytes.len().saturating_add(available.len())),
            FrameSize::TooLarge
        ) {
            return Err(ClientError::DaemonFrameTooLarge);
        }
        bytes.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::io::{Read, Write};

    #[test]
    fn inbound_boundary_excludes_the_newline_and_requires_it() {
        let mut exact = vec![b'x'; FrameContract::MAX_JSON_BYTES];
        exact.push(FrameContract::DELIMITER);
        let mut reader = BufReader::new(Cursor::new(exact));
        let exact = match read_frame(&mut reader, None) {
            Ok(Some(frame)) => frame,
            _ => panic!("the exact boundary must be accepted"),
        };
        assert_eq!(exact.len(), FrameContract::MAX_JSON_BYTES);

        let mut oversized = vec![b'x'; FrameContract::MAX_JSON_BYTES + 1];
        oversized.push(FrameContract::DELIMITER);
        let mut reader = BufReader::new(Cursor::new(oversized));
        assert!(matches!(
            read_frame(&mut reader, None),
            Err(ClientError::DaemonFrameTooLarge)
        ));

        let mut reader = BufReader::new(Cursor::new(b"{}"));
        assert!(matches!(
            read_frame(&mut reader, None),
            Err(ClientError::Broken(message)) if message.contains("during a daemon frame")
        ));
    }

    #[test]
    fn oversized_requests_are_known_not_sent() {
        let (client_stream, mut daemon_stream) = UnixStream::pair().unwrap();
        daemon_stream
            .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"test\"}\n")
            .unwrap();
        daemon_stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let daemon = thread::spawn(move || {
            let mut received = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match daemon_stream.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {
                        received.push(byte[0]);
                        if byte[0] == b'\n' {
                            daemon_stream
                                .write_all(b"{\"id\":1,\"ok\":true,\"result\":{}}\n")
                                .unwrap();
                            break;
                        }
                    }
                    Err(error)
                        if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                    {
                        break;
                    }
                    Err(error) => panic!("test daemon read failed: {error}"),
                }
            }
            received
        });
        let mut client = Client::from_stream(client_stream, Duration::from_millis(20))
            .unwrap_or_else(|_| panic!("the test hello must be accepted"));

        let error = client
            .request(
                "ping",
                json!({"padding": "x".repeat(cyclops_proto::FrameContract::MAX_JSON_BYTES)}),
            )
            .expect_err("the request must be rejected before a write");
        assert!(matches!(error, ClientError::RequestFrameTooLarge));
        assert!(daemon.join().unwrap().is_empty());
    }
}
