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
use crate::notify::{parse_notification, Notification};
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
    /// Per-command reply timeout.
    pub command_timeout: Duration,
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
/// Blocks are collected verbatim: MEASURED on 3.6a, notifications do not
/// interleave inside a %begin/%end pair, they are written after the block
/// closes. A content line is only treated as the terminator when its command
/// number matches the opening %begin, so captured pane text containing
/// "%end ..." cannot truncate a block.
struct LineRouter {
    /// (client flag, command number, collected lines) of the open block.
    block: Option<(bool, Option<u64>, Vec<String>)>,
}

impl LineRouter {
    fn new() -> Self {
        LineRouter { block: None }
    }

    fn feed(&mut self, line: &str) -> Option<Routed> {
        if let Some((_, num, lines)) = &mut self.block {
            if let Some(rest) =
                line.strip_prefix("%end ")
                    .or(if line == "%end" { Some("") } else { None })
            {
                if block_num(rest) == *num {
                    let (client, _, lines) = self.block.take().expect("open block");
                    return Some(Routed::Reply {
                        client,
                        result: Ok(lines),
                    });
                }
            } else if let Some(rest) = line.strip_prefix("%error ") {
                if block_num(rest) == *num {
                    let (client, _, lines) = self.block.take().expect("open block");
                    return Some(Routed::Reply {
                        client,
                        result: Err(lines.join("\n")),
                    });
                }
            }
            lines.push(line.to_string());
            return None;
        }
        if let Some(rest) = line.strip_prefix("%begin ") {
            let num = block_num(rest);
            let client = rest.split_whitespace().nth(2) == Some("1");
            self.block = Some((client, num, Vec::new()));
            return None;
        }
        Some(Routed::Notify(parse_notification(line)))
    }
}

/// Command number: second whitespace field of a %begin/%end/%error tail.
fn block_num(rest: &str) -> Option<u64> {
    rest.split_whitespace().nth(1).and_then(|n| n.parse().ok())
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
    /// a temp file that `load-buffer` reads; the file is deleted afterwards
    /// whether or not the command succeeded. Buffer names are caller-owned;
    /// the daemon guarantees per-delivery uniqueness (amendment e, F4:
    /// named buffers are server-global and concurrent reuse corrupts).
    pub async fn load_buffer(&self, name: &str, bytes: &[u8]) -> Result<(), TmuxError> {
        debug_assert!(!name.is_empty(), "buffer name must be nonempty");
        let seq = self.buffer_file_seq.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("cyclops-buf-{}-{seq}", std::process::id()));
        tokio::fs::write(&path, bytes).await?;
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
        // Fire and forget: the reply may never come if tmux exits first.
        if let Ok(rx) = self.pipe.submit("detach-client").await {
            drop(rx);
        }
        self.pipe.close().await;
        let child = self.child.lock().expect("child lock").take();
        if let Some(mut child) = child {
            match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    warn!("tmux control child did not exit after detach, killing");
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
    let mut lines = BufReader::new(stdout).lines();
    let mut router = LineRouter::new();
    while let Ok(Some(line)) = lines.next_line().await {
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
        assert_eq!(r.feed("%begin 1785658188 276 0"), None);
        assert_eq!(
            r.feed("%end 1785658188 276 0"),
            Some(Routed::Reply {
                client: false,
                result: Ok(vec![])
            })
        );
        assert_eq!(
            r.feed("%session-changed $0 probe"),
            Some(Routed::Notify(Notification::SessionChanged {
                session: "$0".into(),
                name: "probe".into()
            }))
        );
        assert_eq!(r.feed("%begin 1785658188 283 1"), None);
        assert_eq!(r.feed("line one"), None);
        assert_eq!(r.feed("line two"), None);
        assert_eq!(
            r.feed("%end 1785658188 283 1"),
            Some(Routed::Reply {
                client: true,
                result: Ok(vec!["line one".into(), "line two".into()])
            })
        );
    }

    #[test]
    fn router_error_block_carries_text() {
        let mut r = LineRouter::new();
        assert_eq!(r.feed("%begin 1785658267 288 1"), None);
        assert_eq!(
            r.feed("parse error: unknown command: bogus-command-xyz"),
            None
        );
        assert_eq!(
            r.feed("%error 1785658267 288 1"),
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
        assert_eq!(r.feed("%begin 100 7 1"), None);
        assert_eq!(r.feed("%end 100 99 1"), None); // content, wrong number
        assert_eq!(r.feed("%error 100 98 1"), None); // content, wrong number
        assert_eq!(
            r.feed("%end 101 7 1"),
            Some(Routed::Reply {
                client: true,
                result: Ok(vec!["%end 100 99 1".into(), "%error 100 98 1".into()])
            })
        );
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
