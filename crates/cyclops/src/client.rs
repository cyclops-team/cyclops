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

use cyclops_proto::Hello;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub enum ClientError {
    /// Nothing listening at the socket path.
    NotRunning,
    /// Connect did not finish inside CONNECT_TIMEOUT.
    ConnectTimeout,
    /// The daemon answered a request with a wire error.
    Server {
        code: String,
        message: String,
        /// Known targets, read tolerantly from the error object when the
        /// daemon includes them. Feeds the unknown-target copy.
        targets: Vec<String>,
    },
    /// The connection died or carried something that is not NDJSON.
    /// The payload is a short human cause, not a next step.
    Broken(String),
}

pub struct Client {
    reader: BufReader<UnixStream>,
    hello: Hello,
    next_id: u64,
    /// Event lines that arrived while a response was pending. Buffered so
    /// a subscribe race drops nothing; next_line drains these first.
    pending: VecDeque<String>,
}

impl Client {
    /// Connect to cyclops_proto::socket_path() and read the Hello line.
    ///
    /// UnixStream::connect has no timeout parameter, so the connect runs on
    /// a helper thread and we wait on a channel. Unix sockets normally
    /// connect instantly; the timeout catches a daemon with a full accept
    /// backlog. An abandoned helper thread costs nothing here because the
    /// process exits right after the error path.
    pub fn connect() -> Result<Self, ClientError> {
        let path = cyclops_proto::socket_path();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(UnixStream::connect(path));
        });
        let stream = match rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                return Err(match e.kind() {
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused => ClientError::NotRunning,
                    _ => ClientError::Broken(e.to_string()),
                })
            }
            Err(_) => return Err(ClientError::ConnectTimeout),
        };
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|e| ClientError::Broken(e.to_string()))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(ClientError::Broken(
                    "the connection closed before hello".into(),
                ))
            }
            Ok(_) => {}
            Err(e) => return Err(ClientError::Broken(read_cause(&e))),
        }
        let hello: Hello = serde_json::from_str(line.trim())
            .map_err(|_| ClientError::Broken("the hello line didn't parse".into()))?;
        Ok(Client {
            reader,
            hello,
            next_id: 1,
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
        let line = serde_json::to_string(&json!({"id": id, "method": method, "params": params}))
            .map_err(|e| ClientError::Broken(e.to_string()))?;
        {
            let mut w = self.reader.get_ref();
            w.write_all(line.as_bytes())
                .and_then(|_| w.write_all(b"\n"))
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
    }

    fn raw_line(&mut self) -> Result<String, ClientError> {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return Err(ClientError::Broken("the connection closed".into())),
                Ok(_) => {
                    let t = line.trim();
                    if !t.is_empty() {
                        return Ok(t.to_string());
                    }
                }
                Err(e) => return Err(ClientError::Broken(read_cause(&e))),
            }
        }
    }
}

/// Read errors carry a timeout cause worth naming for humans.
fn read_cause(e: &std::io::Error) -> String {
    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
        "no answer within 5 seconds".into()
    } else {
        e.to_string()
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}
