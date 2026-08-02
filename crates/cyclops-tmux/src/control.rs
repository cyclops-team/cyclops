//! Control-mode client: one `tmux -C` child per watched session.
//!
//! Reply correlation (verified live on 3.6a): every command written to
//! stdin produces one `%begin ... %end` or `%begin ... %error` block, blocks
//! arrive FIFO in command order, and the third `%begin` field is 1 for
//! blocks answering a command this client wrote and 0 for implicit blocks
//! (the one tmux emits right after attach). The closing `%end`/`%error`
//! repeats the `%begin` command number, which lets block content that merely
//! looks like a terminator (a captured pane showing control-mode text) pass
//! through safely.
//!
//! Flow control (validation amendment a): the client sets
//! `refresh-client -f pause-after=300` at attach, so a stalled consumer
//! pauses individual panes instead of stalling the connection until the
//! server-side 300 s disconnect. `%pause` is answered with an immediate
//! resume; the consumer still sees the pause as an event.
//!
//! The stream is read as byte lines, never UTF-8 lines (F22): pane bytes
//! at 0x80 and above ride %output/%extended-output verbatim, and a
//! multi-byte character split across two pty reads makes each of the two
//! lines invalid UTF-8 on its own. Decoding must never decide whether the
//! connection lives.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::error::TmuxError;
use crate::notify::{parse_notification_bytes, Notification};
use crate::quote::quote_arg;

/// How the control client reaches its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMode {
    /// `attach-session -t <session>`: the session must already exist.
    Attach,
    /// `new-session -A -s <session>`: create the session if needed, then
    /// attach. Used by tests and first-run flows.
    NewSession,
}

/// Everything needed to spawn a control client.
#[derive(Debug, Clone)]
pub struct ControlConfig {
    /// `-L` socket name. None uses the default tmux server. Tests always
    /// set this; nothing in this crate ever touches the default server on
    /// its own.
    pub socket_name: Option<String>,
    /// `-f` config file. Tests point this at /dev/null.
    pub config_file: Option<PathBuf>,
    /// Target session name.
    pub session: String,
    /// Attach to an existing session or create one.
    pub mode: ControlMode,
    /// Per-command reply timeout. A timeout is a command-level failure
    /// only: the connection stays up and the late reply is consumed in
    /// FIFO order, so correlation survives.
    pub command_timeout: Duration,
    /// Directory for [`ControlClient::load_buffer`] payload spool files,
    /// created 0o700 on first use. None falls back to the system temp dir.
    /// Payload files are always created 0o600 either way; point this at a
    /// private directory so payloads never touch a shared temp dir at all.
    pub buffer_spool_dir: Option<PathBuf>,
}

impl ControlConfig {
    /// Attach to an existing session.
    pub fn attach(session: impl Into<String>) -> Self {
        ControlConfig {
            socket_name: None,
            config_file: None,
            session: session.into(),
            mode: ControlMode::Attach,
            command_timeout: Duration::from_secs(10),
            buffer_spool_dir: None,
        }
    }

    /// Create the session if it does not exist, then attach.
    pub fn new_session(session: impl Into<String>) -> Self {
        ControlConfig {
            mode: ControlMode::NewSession,
            ..ControlConfig::attach(session)
        }
    }

    /// Run on an isolated `-L` socket.
    pub fn on_socket(mut self, name: impl Into<String>) -> Self {
        self.socket_name = Some(name.into());
        self
    }

    /// Use an explicit `-f` config file.
    pub fn with_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Override the reply timeout.
    pub fn with_command_timeout(mut self, t: Duration) -> Self {
        self.command_timeout = t;
        self
    }

    /// Spool `load_buffer` payload files under `dir` instead of the system
    /// temp dir. The directory is created 0o700 on first use.
    pub fn with_buffer_spool_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.buffer_spool_dir = Some(dir.into());
        self
    }
}

/// Reply slot for one in-flight command.
type ReplySlot = oneshot::Sender<Result<Vec<String>, TmuxError>>;

/// Shared write side: stdin plus the FIFO of waiting reply slots. The slot
/// is pushed while the stdin lock is held, so queue order always matches
/// write order, which is what makes FIFO correlation sound.
#[derive(Clone)]
struct CommandPipe {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: Arc<StdMutex<VecDeque<ReplySlot>>>,
}

impl CommandPipe {
    /// Queue a reply slot and write one command line. Returns the receiver
    /// for the reply. Fire-and-forget callers may drop the receiver; the
    /// slot is still consumed in order when the reply arrives.
    async fn submit(
        &self,
        cmd: &str,
    ) -> Result<oneshot::Receiver<Result<Vec<String>, TmuxError>>, TmuxError> {
        if cmd.contains('\n') || cmd.contains('\r') {
            return Err(TmuxError::Protocol(format!(
                "command contains a line break: {cmd:?}"
            )));
        }
        let (tx, rx) = oneshot::channel();
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(TmuxError::Disconnected);
        };
        self.pending.lock().expect("pending lock").push_back(tx);
        let mut line = String::with_capacity(cmd.len() + 1);
        line.push_str(cmd);
        line.push('\n');
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        }
        .await;
        if let Err(e) = write {
            // Our slot is the newest entry; remove it so correlation for
            // earlier commands is unharmed.
            self.pending.lock().expect("pending lock").pop_back();
            return Err(TmuxError::Io(e));
        }
        Ok(rx)
    }

    /// Drop stdin (tmux sees EOF) and fail everything still waiting.
    async fn close(&self) {
        *self.stdin.lock().await = None;
        let mut pending = self.pending.lock().expect("pending lock");
        while let Some(tx) = pending.pop_front() {
            let _ = tx.send(Err(TmuxError::Disconnected));
        }
    }
}

/// What one routed control-mode line means.
#[derive(Debug, PartialEq, Eq)]
enum Routed {
    /// A notification outside any reply block.
    Notify(Notification),
    /// A completed reply block. `client` is true when tmux flagged it as the
    /// answer to a command this client wrote (%begin flags field 1); the
    /// implicit attach-time block carries 0.
    Reply {
        client: bool,
        result: Result<Vec<String>, String>,
    },
}

/// Line-level state machine for the control stream.
///
/// Operates on raw byte lines: %output/%extended-output data is not
/// guaranteed to be valid UTF-8 on the wire (MEASURED on 3.6a, F22), so
/// decoding must never gate routing. Blocks are collected verbatim:
/// MEASURED on 3.6a, notifications do not interleave inside a %begin/%end
/// pair, they are written after the block closes. A content line is only
/// treated as the terminator when its command number matches the opening
/// %begin, so captured pane text containing "%end ..." cannot truncate a
/// block.
struct LineRouter {
    /// (client flag, command number, collected lines) of the open block.
    block: Option<(bool, Option<u64>, Vec<String>)>,
}

impl LineRouter {
    fn new() -> Self {
        LineRouter { block: None }
    }

    fn feed(&mut self, line: &[u8]) -> Option<Routed> {
        if let Some((_, num, lines)) = &mut self.block {
            if let Some(rest) = line
                .strip_prefix(b"%end ".as_slice())
                .or(if line == b"%end" { Some(&[][..]) } else { None })
            {
                if block_num(rest) == *num {
                    let (client, _, lines) = self.block.take().expect("open block");
                    return Some(Routed::Reply {
                        client,
                        result: Ok(lines),
                    });
                }
            } else if let Some(rest) = line.strip_prefix(b"%error ".as_slice()) {
                if block_num(rest) == *num {
                    let (client, _, lines) = self.block.take().expect("open block");
                    return Some(Routed::Reply {
                        client,
                        result: Err(lines.join("\n")),
                    });
                }
            }
            // Reply content is textual (formats, grids); pane bytes travel
            // on notification lines. Lossy conversion keeps a stray invalid
            // byte from poisoning the whole block.
            lines.push(String::from_utf8_lossy(line).into_owned());
            return None;
        }
        if let Some(rest) = line.strip_prefix(b"%begin ".as_slice()) {
            let num = block_num(rest);
            let client = block_field(rest, 2) == Some(b"1".as_slice());
            self.block = Some((client, num, Vec::new()));
            return None;
        }
        Some(Routed::Notify(parse_notification_bytes(line)))
    }
}

/// nth space-separated field of a %begin/%end/%error tail. These tails are
/// ASCII (timestamp, command number, flags).
fn block_field(rest: &[u8], n: usize) -> Option<&[u8]> {
    rest.split(|&b| b == b' ').filter(|f| !f.is_empty()).nth(n)
}

/// Command number: second field of a %begin/%end/%error tail.
fn block_num(rest: &[u8]) -> Option<u64> {
    std::str::from_utf8(block_field(rest, 1)?)
        .ok()
        .and_then(|n| n.parse().ok())
}

/// A live control-mode connection to one tmux session.
///
/// Cheap to share behind an `Arc`. All methods take `&self`.
pub struct ControlClient {
    pipe: CommandPipe,
    child: StdMutex<Option<Child>>,
    reader: StdMutex<Option<JoinHandle<()>>>,
    session: String,
    timeout: Duration,
    buffer_file_seq: AtomicU64,
    spool_dir: Option<PathBuf>,
}

impl ControlClient {
    /// Spawn `tmux -C` for the configured session and complete the attach
    /// handshake (setting the pause-after flow-control flag doubles as the
    /// handshake: a reply proves the attach worked).
    ///
    /// The TMUX environment variable is stripped from the child so a daemon
    /// itself running inside tmux does not trip the nested-session guard.
    ///
    /// Returns the client plus the stream of unsolicited notifications. The
    /// stream closing means the connection died; commands then fail with
    /// [`TmuxError::Disconnected`].
    pub async fn spawn(
        cfg: ControlConfig,
    ) -> Result<(ControlClient, mpsc::UnboundedReceiver<Notification>), TmuxError> {
        let mut cmd = Command::new("tmux");
        // -u forces UTF-8 handling for this client. MEASURED on 3.6a:
        // without it, a client whose LC_ALL/LC_CTYPE/LANG do not name UTF-8
        // gets command replies sanitized, control bytes and non-ASCII
        // replaced with '_'. That silently destroys tab-separated formats
        // and non-ASCII pane titles (Claude's spinner titles, F6) whenever
        // the daemon runs from a minimal environment (launchd, CI).
        cmd.arg("-u");
        if let Some(sock) = &cfg.socket_name {
            cmd.arg("-L").arg(sock);
        }
        if let Some(f) = &cfg.config_file {
            cmd.arg("-f").arg(f);
        }
        cmd.arg("-C");
        match cfg.mode {
            ControlMode::Attach => {
                cmd.args(["attach-session", "-t"]).arg(&cfg.session);
            }
            ControlMode::NewSession => {
                cmd.args(["new-session", "-A", "-s"]).arg(&cfg.session);
            }
        }
        cmd.env_remove("TMUX"); // nested-session guard
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| TmuxError::Spawn(e.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TmuxError::Spawn("no stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TmuxError::Spawn("no stdout pipe".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TmuxError::Spawn("no stderr pipe".into()))?;

        // Collect stderr for spawn diagnostics; tmux writes attach failures
        // there before exiting.
        let stderr_text = Arc::new(StdMutex::new(String::new()));
        {
            let buf = Arc::clone(&stderr_text);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    debug!(line = %l, "tmux stderr");
                    let mut b = buf.lock().expect("stderr buf lock");
                    b.push_str(&l);
                    b.push('\n');
                }
            });
        }

        let pipe = CommandPipe {
            stdin: Arc::new(Mutex::new(Some(stdin))),
            pending: Arc::new(StdMutex::new(VecDeque::new())),
        };
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(reader_task(stdout, pipe.clone(), notif_tx));

        let client = ControlClient {
            pipe,
            child: StdMutex::new(Some(child)),
            reader: StdMutex::new(Some(reader)),
            session: cfg.session,
            timeout: cfg.command_timeout,
            buffer_file_seq: AtomicU64::new(0),
            spool_dir: cfg.buffer_spool_dir,
        };

        // Handshake plus flow control in one command. A %error reply still
        // proves the attach worked (older tmux without pause-after), so only
        // transport-level failures are fatal here.
        match client.command("refresh-client -f pause-after=300").await {
            Ok(_) => {}
            Err(TmuxError::Command(msg)) => {
                warn!(%msg, "tmux rejected pause-after flow control, continuing without it");
            }
            Err(e) => {
                client.shutdown().await;
                let stderr = stderr_text.lock().expect("stderr buf lock").clone();
                return Err(TmuxError::Spawn(format!(
                    "attach handshake failed: {e}; tmux stderr: {}",
                    stderr.trim()
                )));
            }
        }

        Ok((client, notif_rx))
    }

    /// Session this client is attached to.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Run one tmux command and return its reply block lines.
    ///
    /// `%error` replies come back as [`TmuxError::Command`] with the error
    /// text. Commands must be a single line; the payload path for arbitrary
    /// bytes is [`ControlClient::load_buffer`].
    pub async fn command(&self, cmd: &str) -> Result<Vec<String>, TmuxError> {
        let rx = self.pipe.submit(cmd).await?;
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(TmuxError::Disconnected),
            Err(_) => Err(TmuxError::Timeout(cmd.to_string())),
        }
    }

    /// Visible grid of a pane as plain text, lines joined with newlines.
    pub async fn capture_pane(&self, pane_id: &str) -> Result<String, TmuxError> {
        let out = self
            .command(&format!("capture-pane -p -t {}", quote_arg(pane_id)))
            .await?;
        Ok(out.join("\n"))
    }

    /// Visible grid of a pane with SGR escape sequences preserved
    /// (capture-pane -e). This is the sensor for manifest `line_regex_esc`
    /// rules: rendering style the plain capture cannot express (codex ghost
    /// suggestions are dim, typed text is bare, F19). Safe through the
    /// byte-line reader (F22): the escaped content is plain text plus ASCII
    /// ESC bytes, which survive the reply block's lossy conversion intact.
    pub async fn capture_pane_escaped(&self, pane_id: &str) -> Result<String, TmuxError> {
        let out = self
            .command(&format!("capture-pane -e -p -t {}", quote_arg(pane_id)))
            .await?;
        Ok(out.join("\n"))
    }

    /// Visible grid plus the last `lines` of scrollback.
    pub async fn capture_pane_history(
        &self,
        pane_id: &str,
        lines: u32,
    ) -> Result<String, TmuxError> {
        let out = self
            .command(&format!(
                "capture-pane -p -t {} -S -{lines}",
                quote_arg(pane_id)
            ))
            .await?;
        Ok(out.join("\n"))
    }

    /// Expand a format string against a pane, e.g. `#{pane_current_command}`.
    pub async fn display(&self, pane_id: &str, format: &str) -> Result<String, TmuxError> {
        let out = self
            .command(&format!(
                "display-message -p -t {} {}",
                quote_arg(pane_id),
                quote_arg(format)
            ))
            .await?;
        Ok(out.join("\n"))
    }

    /// Send keys to a pane. Each element is either a tmux key name (Enter,
    /// Escape, C-c, ...) sent as a key, or arbitrary text sent literally
    /// with `-l`. Consecutive elements of the same kind share one command.
    ///
    /// Literals must not contain newlines (control mode is line based); use
    /// an explicit "Enter" element, or the buffer/paste path for payloads.
    pub async fn send_keys(&self, pane_id: &str, keys: &[&str]) -> Result<(), TmuxError> {
        let target = quote_arg(pane_id);
        let mut i = 0;
        while i < keys.len() {
            let first_is_key = is_key_name(keys[i]);
            let mut j = i + 1;
            while j < keys.len() && is_key_name(keys[j]) == first_is_key {
                j += 1;
            }
            let mut cmd = format!("send-keys -t {target}");
            let literal = !first_is_key;
            if literal {
                cmd.push_str(" -l");
            }
            cmd.push_str(" --");
            for k in &keys[i..j] {
                cmd.push(' ');
                cmd.push_str(&quote_arg(k));
            }
            self.command(&cmd).await?;
            i = j;
        }
        Ok(())
    }

    /// Load bytes into a named tmux buffer.
    ///
    /// Control-mode stdin is the command channel, so the content travels via
    /// a spool file that `load-buffer` reads; the file is created 0o600
    /// (payloads are never world-readable, even transiently) under the
    /// configured spool dir ([`ControlConfig::with_buffer_spool_dir`]) or
    /// the system temp dir, and is deleted afterwards whether or not the
    /// command succeeded. Buffer names are caller-owned; the daemon
    /// guarantees per-delivery uniqueness (amendment e, F4: named buffers
    /// are server-global and concurrent reuse corrupts).
    pub async fn load_buffer(&self, name: &str, bytes: &[u8]) -> Result<(), TmuxError> {
        debug_assert!(!name.is_empty(), "buffer name must be nonempty");
        let seq = self.buffer_file_seq.fetch_add(1, Ordering::Relaxed);
        let path = write_spool_file(self.spool_dir.as_deref(), seq, bytes).await?;
        let cmd = format!(
            "load-buffer -b {} {}",
            quote_arg(name),
            quote_arg(&path.to_string_lossy())
        );
        let res = self.command(&cmd).await;
        // Message content must not linger on disk.
        let _ = tokio::fs::remove_file(&path).await;
        res.map(|_| ())
    }

    /// Paste a named buffer into a pane. `bracketed` adds `-p` (bracket
    /// markers when the pane application requested bracketed paste),
    /// `delete` adds `-d` (delete the buffer after pasting).
    pub async fn paste_buffer(
        &self,
        name: &str,
        pane_id: &str,
        bracketed: bool,
        delete: bool,
    ) -> Result<(), TmuxError> {
        debug_assert!(!name.is_empty(), "buffer name must be nonempty");
        let mut cmd = String::from("paste-buffer");
        if bracketed {
            cmd.push_str(" -p");
        }
        if delete {
            cmd.push_str(" -d");
        }
        cmd.push_str(&format!(
            " -b {} -t {}",
            quote_arg(name),
            quote_arg(pane_id)
        ));
        self.command(&cmd).await.map(|_| ())
    }

    /// Clean shutdown: ask tmux to detach, close stdin, reap the child,
    /// stop the reader. Safe to call more than once. Best effort by design;
    /// `kill_on_drop` backstops every path.
    pub async fn shutdown(&self) {
        // Ask for a clean detach. Bounded: a wedged stdin pipe must never
        // hang shutdown. Fire and forget beyond that; the reply may never
        // come if tmux exits first.
        let detach_sent = matches!(
            tokio::time::timeout(
                Duration::from_millis(500),
                self.pipe.submit("detach-client")
            )
            .await,
            Ok(Ok(_))
        );
        self.pipe.close().await;
        let child = self.child.lock().expect("child lock").take();
        if let Some(mut child) = child {
            // When the detach could not be written the pipe was already
            // closed: the child has had stdin EOF since then and is not
            // leaving on its own (a dead reader also stops draining its
            // stdout, so it may be wedged flushing). Skip straight to kill
            // instead of blinding the owner for the full grace period.
            let grace = if detach_sent {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(100)
            };
            match tokio::time::timeout(grace, child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    warn!(detach_sent, "tmux control child did not exit, killing");
                    let _ = child.kill().await;
                }
            }
        }
        // Reader normally finishes on EOF; abort is the backstop.
        if let Some(h) = self.reader.lock().expect("reader lock").take() {
            h.abort();
        }
    }
}

/// Write one load-buffer payload spool file: exclusive create, mode 0o600,
/// under `dir` (created 0o700 when missing) or the system temp dir. A stale
/// same-named file (a dead process that reused our pid) is removed first so
/// the exclusive create cannot write through it.
async fn write_spool_file(
    dir: Option<&std::path::Path>,
    seq: u64,
    bytes: &[u8],
) -> Result<PathBuf, TmuxError> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = match dir {
        Some(d) => {
            if !d.exists() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(d)
                    .map_err(TmuxError::Io)?;
            }
            d.to_path_buf()
        }
        None => std::env::temp_dir(),
    };
    let path = dir.join(format!("cyclops-buf-{}-{seq}", std::process::id()));
    let _ = tokio::fs::remove_file(&path).await;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    let mut f = opts.open(&path).await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    Ok(path)
}

/// Reads the control stream: resolves reply blocks against the pending FIFO
/// and forwards notifications. Auto-resumes paused panes (amendment a).
/// On EOF it closes the pipe, which fails every waiting command with
/// Disconnected, and drops the notification sender so the consumer sees the
/// stream end.
async fn reader_task(
    stdout: ChildStdout,
    pipe: CommandPipe,
    notif_tx: mpsc::UnboundedSender<Notification>,
) {
    // Byte lines, never UTF-8 lines. tmux escapes control bytes octally but
    // passes bytes >= 0x80 through verbatim, and a multi-byte character
    // split across two pty reads produces two lines that are each invalid
    // UTF-8 on their own (MEASURED on 3.6a, F22). A UTF-8-decoding reader
    // dies on the first such line and takes the connection with it; that
    // was the M1 soak's 8-drops-in-80s bug under a Claude TUI pane.
    let mut reader = BufReader::new(stdout);
    let mut line: Vec<u8> = Vec::new();
    let mut router = LineRouter::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => break, // EOF: the transport is really gone
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "control stream read failed");
                break;
            }
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        match router.feed(&line) {
            None => {}
            Some(Routed::Notify(n)) => {
                if let Notification::Pause { pane } = &n {
                    debug!(%pane, "flow control paused pane, resuming");
                    let pipe = pipe.clone();
                    let cmd = format!(
                        "refresh-client -A {}",
                        quote_arg(&format!("{pane}:continue"))
                    );
                    // Separate task: the reader must keep draining so it can
                    // route this very command's reply.
                    tokio::spawn(async move {
                        if let Err(e) = pipe.submit(&cmd).await {
                            warn!(error = %e, "failed to resume paused pane");
                        }
                    });
                }
                if let Notification::Other(raw) = &n {
                    debug!(line = %raw, "unrecognized control line");
                }
                // Consumer gone is fine; keep draining for command replies.
                let _ = notif_tx.send(n);
            }
            Some(Routed::Reply { client, result }) => {
                if !client {
                    // The implicit block right after attach, or another
                    // unsolicited block: consumed with no waiting caller.
                    debug!("consumed unsolicited reply block");
                    continue;
                }
                let slot = pipe.pending.lock().expect("pending lock").pop_front();
                match slot {
                    Some(tx) => {
                        let _ = tx.send(result.map_err(TmuxError::Command));
                    }
                    None => debug!("reply block with no waiting caller"),
                }
            }
        }
    }
    pipe.close().await;
}

/// Key names the daemon sends as keys rather than literal text: the named
/// tmux keys plus modifier chains (C-, M-, S-) ending in a named key or a
/// single character. Everything else goes through `send-keys -l`.
fn is_key_name(k: &str) -> bool {
    const NAMED: &[&str] = &[
        "Enter", "Escape", "Space", "Tab", "BTab", "BSpace", "Up", "Down", "Left", "Right", "Home",
        "End", "NPage", "PPage", "PageDown", "PageUp", "IC", "Insert", "DC", "Delete", "F1", "F2",
        "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    ];
    if NAMED.contains(&k) {
        return true;
    }
    let mut rest = k;
    let mut saw_modifier = false;
    while let Some(r) = rest
        .strip_prefix("C-")
        .or_else(|| rest.strip_prefix("M-"))
        .or_else(|| rest.strip_prefix("S-"))
    {
        saw_modifier = true;
        rest = r;
    }
    saw_modifier && !rest.is_empty() && (NAMED.contains(&rest) || rest.chars().count() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_consumes_unsolicited_initial_block_then_correlates() {
        // Transcript shape verbatim from 3.6a: the attach-time block carries
        // flags 0, replies to our commands carry flags 1.
        let mut r = LineRouter::new();
        assert_eq!(r.feed(b"%begin 1785658188 276 0"), None);
        assert_eq!(
            r.feed(b"%end 1785658188 276 0"),
            Some(Routed::Reply {
                client: false,
                result: Ok(vec![])
            })
        );
        assert_eq!(
            r.feed(b"%session-changed $0 probe"),
            Some(Routed::Notify(Notification::SessionChanged {
                session: "$0".into(),
                name: "probe".into()
            }))
        );
        assert_eq!(r.feed(b"%begin 1785658188 283 1"), None);
        assert_eq!(r.feed(b"line one"), None);
        assert_eq!(r.feed(b"line two"), None);
        assert_eq!(
            r.feed(b"%end 1785658188 283 1"),
            Some(Routed::Reply {
                client: true,
                result: Ok(vec!["line one".into(), "line two".into()])
            })
        );
    }

    #[test]
    fn router_error_block_carries_text() {
        let mut r = LineRouter::new();
        assert_eq!(r.feed(b"%begin 1785658267 288 1"), None);
        assert_eq!(
            r.feed(b"parse error: unknown command: bogus-command-xyz"),
            None
        );
        assert_eq!(
            r.feed(b"%error 1785658267 288 1"),
            Some(Routed::Reply {
                client: true,
                result: Err("parse error: unknown command: bogus-command-xyz".into())
            })
        );
    }

    #[test]
    fn router_ignores_lookalike_terminators_inside_blocks() {
        // Captured pane content can contain control-mode text. Only a
        // terminator repeating the opening command number closes the block.
        let mut r = LineRouter::new();
        assert_eq!(r.feed(b"%begin 100 7 1"), None);
        assert_eq!(r.feed(b"%end 100 99 1"), None); // content, wrong number
        assert_eq!(r.feed(b"%error 100 98 1"), None); // content, wrong number
        assert_eq!(
            r.feed(b"%end 101 7 1"),
            Some(Routed::Reply {
                client: true,
                result: Ok(vec!["%end 100 99 1".into(), "%error 100 98 1".into()])
            })
        );
    }

    #[test]
    fn router_survives_invalid_utf8_lines() {
        // Wire truth on 3.6a (F22): raw bytes >= 0x80 and split multi-byte
        // fragments appear on notification lines. Routing must stay
        // byte-faithful for output data and lossy-tolerant inside blocks.
        let mut r = LineRouter::new();
        match r.feed(b"%output %0 X\xffY") {
            Some(Routed::Notify(Notification::Output { pane, data })) => {
                assert_eq!(pane, "%0");
                assert_eq!(data, b"X\xffY");
            }
            other => panic!("wrong route: {other:?}"),
        }
        match r.feed(b"%extended-output %0 3 : \xe2\xa0") {
            Some(Routed::Notify(Notification::ExtendedOutput { data, .. })) => {
                assert_eq!(data, b"\xe2\xa0");
            }
            other => panic!("wrong route: {other:?}"),
        }
        // Inside a reply block an invalid byte must not derail terminator
        // matching; content degrades lossily instead.
        assert_eq!(r.feed(b"%begin 200 9 1"), None);
        assert_eq!(r.feed(b"grid \xff line"), None);
        assert_eq!(
            r.feed(b"%end 200 9 1"),
            Some(Routed::Reply {
                client: true,
                result: Ok(vec!["grid \u{FFFD} line".into()])
            })
        );
    }

    #[tokio::test]
    async fn spool_file_is_owner_only_and_dir_created_0700() {
        use std::os::unix::fs::PermissionsExt;
        let base = cyclops_proto::scratch::scratch_dir("cyclops-spool-test");
        let _ = std::fs::remove_dir_all(&base);
        let path = write_spool_file(Some(&base), 7, b"secret payload")
            .await
            .expect("spool write");
        let dir_mode = std::fs::metadata(&base)
            .expect("spool dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "spool dir is owner-only");
        let file_mode = std::fs::metadata(&path)
            .expect("spool file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "spool file is owner-only");
        assert_eq!(std::fs::read(&path).expect("read back"), b"secret payload");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn key_name_classification() {
        for k in [
            "Enter", "Escape", "Tab", "BSpace", "F5", "C-c", "M-Enter", "C-M-x", "S-Up",
        ] {
            assert!(is_key_name(k), "{k} should be a key name");
        }
        for k in ["hello", "a", "5", "echo hi", "C-", "X-a", "-l", "ls -la"] {
            assert!(!is_key_name(k), "{k} should be literal");
        }
    }
}
