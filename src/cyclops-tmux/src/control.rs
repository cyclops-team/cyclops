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
//!
//! Notifications enter a bounded ordered queue. When that queue fills, the
//! reader retains its queued prefix, records one explicit continuity gap, and
//! keeps reading so a correlated command reply cannot deadlock behind output.
//! The consumer reconciles from an authoritative snapshot after the gap. An
//! oversized control line still closes the connection.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::error::TmuxError;
use crate::notify::{parse_notification_bytes, Notification};
use crate::quote::quote_arg;

static SPOOL_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Notifications retained outside the control reader. One extra channel slot
/// is reserved for the continuity marker and does not increase this payload
/// capacity.
pub const NOTIFICATION_CAPACITY: usize = 256;
/// Aggregate retained notification payload. Item capacity still bounds
/// bookkeeping, while this budget prevents a queue of large output records
/// from retaining `capacity * line_limit` bytes.
pub const NOTIFICATION_MAX_QUEUED_BYTES: usize = 8 << 20;
/// Largest tmux control line accepted into memory. A larger line closes the
/// connection rather than growing one read without bound.
pub const CONTROL_LINE_MAX_BYTES: usize = 1 << 20;
/// Aggregate reply data accepted between one `%begin` and its terminator.
pub const CONTROL_BLOCK_MAX_BYTES: usize = 8 << 20;
/// Reply rows accepted in one block. This also bounds the `Vec<String>`
/// bookkeeping when rows themselves are empty.
pub const CONTROL_BLOCK_MAX_LINES: usize = 65_536;
/// Commands awaiting correlated reply blocks. General concurrent callers are
/// refused before writing when no slot is available. The serialized workspace
/// input owner may wait for a slot, but the reply FIFO stays fixed at this
/// bound.
pub const PENDING_REPLY_CAPACITY: usize = 64;
/// Pause resume asks waiting behind the single resume writer.
const PAUSE_RESUME_CAPACITY: usize = 64;

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
    /// Name for the first window when [`ControlMode::NewSession`] creates
    /// the session. Ignored by attach mode.
    pub initial_window_name: Option<String>,
    /// Per-command reply timeout. A timeout is a command-level failure
    /// only: the connection stays up and the late reply is consumed in
    /// FIFO order, so correlation survives.
    pub command_timeout: Duration,
    /// Directory for [`ControlClient::load_buffer`] payload spool files,
    /// created 0o700 on first use. None falls back to the system temp dir.
    /// Payload files are always created 0o600 either way; point this at a
    /// private directory so payloads never touch a shared temp dir at all.
    pub buffer_spool_dir: Option<PathBuf>,
    /// Held state root and descendant directory for Cyclops-owned spool files.
    pub state_buffer_spool: Option<(Arc<cyclops_state::StateRoot>, PathBuf)>,
}

impl ControlConfig {
    /// Attach to an existing session.
    pub fn attach(session: impl Into<String>) -> Self {
        ControlConfig {
            socket_name: None,
            config_file: None,
            session: session.into(),
            mode: ControlMode::Attach,
            initial_window_name: None,
            command_timeout: Duration::from_secs(10),
            buffer_spool_dir: None,
            state_buffer_spool: None,
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

    /// Give a newly-created session's first window an explicit name.
    pub fn with_initial_window_name(mut self, name: impl Into<String>) -> Self {
        self.initial_window_name = Some(name.into());
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

    /// Spool through a held state root while tmux consumes the pathname.
    pub fn with_state_buffer_spool(
        mut self,
        root: Arc<cyclops_state::StateRoot>,
        descendant: impl Into<PathBuf>,
    ) -> Self {
        self.state_buffer_spool = Some((root, descendant.into()));
        self
    }
}

enum ReplyTarget {
    Caller(oneshot::Sender<Result<Vec<String>, TmuxError>>),
    Resume { pane: String },
}

/// Reply slot for one in-flight command. The permit is retained until the
/// reader consumes the correlated block or connection shutdown drains it.
struct ReplySlot {
    target: ReplyTarget,
    _permit: OwnedSemaphorePermit,
}

/// One reserved reply slot for a pane-input command.
///
/// The workspace holds this only after its event loop observes that the
/// bounded reply FIFO is full. The fields stay private so capacity can only
/// be spent by [`ControlClient::send_keys_unconfirmed_reserved`].
pub struct InputCapacity {
    reply_slots: Arc<Semaphore>,
    permit: OwnedSemaphorePermit,
}

/// Shared write side: stdin plus the FIFO of waiting reply slots. The slot
/// is pushed while the stdin lock is held, so queue order always matches
/// write order, which is what makes FIFO correlation sound.
#[derive(Clone)]
struct CommandPipe {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: Arc<StdMutex<VecDeque<ReplySlot>>>,
    reply_slots: Arc<Semaphore>,
    /// Commands successfully written, across every clone of this pipe.
    /// Exists so a caller (a test, or a future cost budget) can prove a
    /// batched adapter call issues a small, fixed number of tmux commands
    /// rather than one per item it iterates over — see
    /// [`ControlClient::commands_issued`].
    issued: Arc<AtomicU64>,
}

impl CommandPipe {
    /// Queue a reply slot and write one command line. Returns the receiver
    /// for the reply. Fire-and-forget callers may drop the receiver; the
    /// slot is still consumed in order when the reply arrives.
    async fn submit(
        &self,
        cmd: &str,
    ) -> Result<oneshot::Receiver<Result<Vec<String>, TmuxError>>, TmuxError> {
        let (tx, rx) = oneshot::channel();
        self.submit_target(cmd, ReplyTarget::Caller(tx)).await?;
        Ok(rx)
    }

    /// Queue an adapter-owned resume. Its successful reply becomes a
    /// `Continue` notification in the reader itself, at the exact point that
    /// reply appears in the ordered control stream.
    async fn submit_resume(&self, cmd: &str, pane: String) -> Result<(), TmuxError> {
        validate_command_line(cmd)?;
        let permit = self.reserve_capacity().await?;
        self.write_target(cmd, ReplyTarget::Resume { pane }, permit)
            .await
    }

    async fn submit_target(&self, cmd: &str, target: ReplyTarget) -> Result<(), TmuxError> {
        validate_command_line(cmd)?;
        let permit = match Arc::clone(&self.reply_slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => return Err(TmuxError::Busy),
            Err(tokio::sync::TryAcquireError::Closed) => return Err(TmuxError::Disconnected),
        };
        self.write_target(cmd, target, permit).await
    }

    async fn reserve_capacity(&self) -> Result<OwnedSemaphorePermit, TmuxError> {
        Arc::clone(&self.reply_slots)
            .acquire_owned()
            .await
            .map_err(|_| TmuxError::Disconnected)
    }

    async fn write_target(
        &self,
        cmd: &str,
        target: ReplyTarget,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), TmuxError> {
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(TmuxError::Disconnected);
        };
        self.pending
            .lock()
            .expect("pending lock")
            .push_back(ReplySlot {
                target,
                _permit: permit,
            });
        let mut line = String::with_capacity(cmd.len() + 1);
        line.push_str(cmd);
        line.push('\n');
        let write = write_command_line(stdin, line.as_bytes()).await;
        if let Err(error) = write {
            return Err(self.poison_after_write_failure(&mut *guard, error));
        }
        self.issued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// A failed write breaks FIFO correlation for every command on this pipe.
    /// Close it once, fail pending callers, and preserve whether this command
    /// was provably unwritten or may have reached tmux.
    fn poison_after_write_failure(
        &self,
        stdin: &mut Option<ChildStdin>,
        error: CommandWriteError,
    ) -> TmuxError {
        *stdin = None;
        self.reply_slots.close();
        let mut pending = self.pending.lock().expect("pending lock");
        while let Some(slot) = pending.pop_front() {
            if let ReplyTarget::Caller(tx) = slot.target {
                let _ = tx.send(Err(TmuxError::Disconnected));
            }
        }
        match error {
            CommandWriteError::Unwritten(error) => TmuxError::Io(error),
            CommandWriteError::Uncertain(error) => TmuxError::WriteUncertain(error),
        }
    }

    /// Drop stdin (tmux sees EOF) and fail everything still waiting.
    async fn close(&self) {
        // Wake capacity waiters before draining slots. Existing permits may
        // finish their current write, but no new writer can enter this pipe.
        self.reply_slots.close();
        *self.stdin.lock().await = None;
        let mut pending = self.pending.lock().expect("pending lock");
        while let Some(slot) = pending.pop_front() {
            if let ReplyTarget::Caller(tx) = slot.target {
                let _ = tx.send(Err(TmuxError::Disconnected));
            }
        }
    }
}

enum CommandWriteError {
    Unwritten(std::io::Error),
    Uncertain(std::io::Error),
}

async fn write_command_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    line: &[u8],
) -> Result<(), CommandWriteError> {
    let mut written = 0;
    while written < line.len() {
        match writer.write(&line[written..]).await {
            Ok(0) => {
                let error = std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "tmux command pipe accepted zero bytes",
                );
                return Err(if written == 0 {
                    CommandWriteError::Unwritten(error)
                } else {
                    CommandWriteError::Uncertain(error)
                });
            }
            Ok(bytes) => written += bytes,
            Err(error) if written == 0 => return Err(CommandWriteError::Unwritten(error)),
            Err(error) => return Err(CommandWriteError::Uncertain(error)),
        }
    }
    writer.flush().await.map_err(CommandWriteError::Uncertain)
}

fn validate_command_line(cmd: &str) -> Result<(), TmuxError> {
    if cmd.contains('\n') || cmd.contains('\r') {
        return Err(TmuxError::Protocol(format!(
            "command contains a line break: {cmd:?}"
        )));
    }
    Ok(())
}

struct QueuedNotification {
    notification: Notification,
    _item_permit: Option<OwnedSemaphorePermit>,
    _byte_permit: Option<OwnedSemaphorePermit>,
}

#[derive(Clone)]
struct NotificationSink {
    tx: mpsc::Sender<QueuedNotification>,
    bytes: Arc<Semaphore>,
    items: Arc<Semaphore>,
    gap: Arc<StdMutex<NotificationGap>>,
}

#[derive(Default)]
struct NotificationGap {
    pending: bool,
    epoch: u64,
}

/// Bounded unsolicited control notifications. `recv` releases a regular
/// entry's item and byte permits before handing its notification to the
/// caller. The single continuity marker owns neither permit.
pub struct NotificationReceiver {
    rx: NotificationQueue,
}

enum NotificationQueue {
    Budgeted {
        rx: mpsc::Receiver<QueuedNotification>,
        gap: Arc<StdMutex<NotificationGap>>,
    },
    Direct(mpsc::Receiver<Notification>),
}

impl NotificationReceiver {
    pub async fn recv(&mut self) -> Option<Notification> {
        match &mut self.rx {
            NotificationQueue::Budgeted { rx, .. } => {
                rx.recv().await.map(|queued| queued.notification)
            }
            NotificationQueue::Direct(rx) => rx.recv().await,
        }
    }

    pub fn try_recv(&mut self) -> Result<Notification, mpsc::error::TryRecvError> {
        match &mut self.rx {
            NotificationQueue::Budgeted { rx, .. } => {
                rx.try_recv().map(|queued| queued.notification)
            }
            NotificationQueue::Direct(rx) => rx.try_recv(),
        }
    }

    /// Stop the producer at the current stream segment. Used when a
    /// downstream bounded hop loses continuity before this receiver does.
    pub fn hold_continuity(&self) -> u64 {
        if let NotificationQueue::Budgeted { gap, .. } = &self.rx {
            let mut gap = gap.lock().expect("notification gap lock");
            gap.pending = true;
            gap.epoch
        } else {
            0
        }
    }

    /// Discard the invalid suffix and let the producer begin a new segment
    /// only after the consumer replaced all derived state.
    pub fn resume_after_reconcile(&mut self, expected_epoch: u64) -> bool {
        match &mut self.rx {
            NotificationQueue::Budgeted { rx, gap } => {
                let mut gap = gap.lock().expect("notification gap lock");
                if gap.epoch != expected_epoch {
                    return false;
                }
                while rx.try_recv().is_ok() {}
                gap.pending = false;
                true
            }
            NotificationQueue::Direct(rx) => {
                while rx.try_recv().is_ok() {}
                true
            }
        }
    }

    /// Adapt an already bounded notification receiver. `ControlClient`
    /// supplies the stronger aggregate byte budget; this adapter exists for
    /// bounded in-process producers such as integration probes.
    pub fn from_bounded(rx: mpsc::Receiver<Notification>) -> Self {
        Self {
            rx: NotificationQueue::Direct(rx),
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
    /// A reply block exceeded its item or byte envelope.
    BlockOverflow,
}

struct OpenBlock {
    client: bool,
    command: Option<u64>,
    lines: Vec<String>,
    bytes: usize,
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
    block: Option<OpenBlock>,
}

impl LineRouter {
    fn new() -> Self {
        LineRouter { block: None }
    }

    fn feed(&mut self, line: &[u8]) -> Option<Routed> {
        if let Some(block) = &mut self.block {
            if let Some(rest) = line
                .strip_prefix(b"%end ".as_slice())
                .or(if line == b"%end" { Some(&[][..]) } else { None })
            {
                if block_num(rest) == block.command {
                    let block = self.block.take().expect("open block");
                    return Some(Routed::Reply {
                        client: block.client,
                        result: Ok(block.lines),
                    });
                }
            } else if let Some(rest) = line.strip_prefix(b"%error ".as_slice()) {
                if block_num(rest) == block.command {
                    let block = self.block.take().expect("open block");
                    return Some(Routed::Reply {
                        client: block.client,
                        result: Err(block.lines.join("\n")),
                    });
                }
            }
            let next_bytes = block.bytes.saturating_add(line.len()).saturating_add(1);
            if block.lines.len() >= CONTROL_BLOCK_MAX_LINES || next_bytes > CONTROL_BLOCK_MAX_BYTES
            {
                self.block = None;
                return Some(Routed::BlockOverflow);
            }
            // Reply content is textual (formats, grids); pane bytes travel
            // on notification lines. Lossy conversion keeps a stray invalid
            // byte from poisoning the whole block.
            block.bytes = next_bytes;
            block.lines.push(String::from_utf8_lossy(line).into_owned());
            return None;
        }
        if let Some(rest) = line.strip_prefix(b"%begin ".as_slice()) {
            self.block = Some(OpenBlock {
                command: block_num(rest),
                client: block_field(rest, 2) == Some(b"1".as_slice()),
                lines: Vec::new(),
                bytes: 0,
            });
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
    state_spool: Option<(Arc<cyclops_state::StateRoot>, PathBuf)>,
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
    ) -> Result<(ControlClient, NotificationReceiver), TmuxError> {
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
                cmd.args(["attach-session", "-t"])
                    .arg(crate::cmd::session_target(&cfg.session));
            }
            ControlMode::NewSession => {
                cmd.args(["new-session", "-A", "-s"]).arg(&cfg.session);
                if let Some(name) = &cfg.initial_window_name {
                    cmd.arg("-n").arg(name);
                }
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
            reply_slots: Arc::new(Semaphore::new(PENDING_REPLY_CAPACITY)),
            issued: Arc::new(AtomicU64::new(0)),
        };
        let (notif_tx, notif_rx) = mpsc::channel(NOTIFICATION_CAPACITY + 1);
        let gap = Arc::new(StdMutex::new(NotificationGap::default()));
        let notification_sink = NotificationSink {
            tx: notif_tx,
            bytes: Arc::new(Semaphore::new(NOTIFICATION_MAX_QUEUED_BYTES)),
            items: Arc::new(Semaphore::new(NOTIFICATION_CAPACITY)),
            gap: Arc::clone(&gap),
        };
        let reader = tokio::spawn(reader_task(stdout, pipe.clone(), notification_sink));

        let client = ControlClient {
            pipe,
            child: StdMutex::new(Some(child)),
            reader: StdMutex::new(Some(reader)),
            session: cfg.session,
            timeout: cfg.command_timeout,
            buffer_file_seq: AtomicU64::new(0),
            spool_dir: cfg.buffer_spool_dir,
            state_spool: cfg.state_buffer_spool,
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

        Ok((
            client,
            NotificationReceiver {
                rx: NotificationQueue::Budgeted { rx: notif_rx, gap },
            },
        ))
    }

    /// Session this client is attached to.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Total control-mode commands written on this connection, confirmed
    /// or fire-and-forget alike (every path through
    /// [`CommandPipe::write_target`]). Counts lines written, not replies
    /// received. Includes the
    /// attach handshake's own command, so callers proving "a small fixed
    /// number of commands" should compare a before/after delta rather than
    /// the absolute value.
    pub fn commands_issued(&self) -> u64 {
        self.pipe.issued.load(Ordering::Relaxed)
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

    /// Queue a trusted command without waiting for its reply block.
    ///
    /// The reader still consumes the command's FIFO reply slot, so later
    /// request/response correlation remains intact. This is reserved for
    /// latency-sensitive commands whose output cannot affect the caller,
    /// such as forwarding a keypress.
    async fn command_unconfirmed(&self, cmd: &str) -> Result<(), TmuxError> {
        let _reply = self.pipe.submit(cmd).await?;
        Ok(())
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

    /// Visible grid with SGR preserved and tmux-wrapped rows joined.
    ///
    /// Application-rendered line breaks remain separate. This is the
    /// capture used when exact composer extraction needs logical rows.
    pub async fn capture_pane_joined_escaped(&self, pane_id: &str) -> Result<String, TmuxError> {
        let out = self
            .command(&format!("capture-pane -e -J -p -t {}", quote_arg(pane_id)))
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

    /// Resolve one pane's root pid from the whole tmux server.
    ///
    /// A session watcher reports a pane as removed when it moves to another
    /// session. `list-panes -a` distinguishes that route change from physical
    /// pane loss without polling or opening another client.
    pub async fn server_pane_pid(&self, pane_id: &str) -> Result<Option<i32>, TmuxError> {
        let out = self
            .command(&format!(
                "list-panes -a -F {}",
                quote_arg("#{pane_id}\t#{pane_pid}")
            ))
            .await?;
        parse_server_pane_pid(&out, pane_id)
    }

    /// Send keys to a pane. Each element is either a tmux key name (Enter,
    /// Escape, C-c, ...) sent as a key, or arbitrary text sent literally
    /// with `-l`. Consecutive elements of the same kind share one command.
    ///
    /// Literals must not contain newlines (control mode is line based); use
    /// an explicit "Enter" element, or the buffer/paste path for payloads.
    pub async fn send_keys(&self, pane_id: &str, keys: &[&str]) -> Result<(), TmuxError> {
        for cmd in send_keys_commands(pane_id, keys) {
            self.command(&cmd).await?;
        }
        Ok(())
    }

    /// Forward keys without waiting for tmux's empty success reply.
    ///
    /// Generated `send-keys` commands have no useful response body. The
    /// write itself is still awaited and ordered; only the reply wait is
    /// removed so an interactive client does not add a tmux round trip to
    /// every keystroke.
    pub async fn send_keys_unconfirmed(
        &self,
        pane_id: &str,
        keys: &[&str],
    ) -> Result<(), TmuxError> {
        for cmd in send_keys_commands(pane_id, keys) {
            self.command_unconfirmed(&cmd).await?;
        }
        Ok(())
    }

    /// Wait for one bounded reply slot without blocking the control reader.
    ///
    /// The returned future owns its semaphore wait and does not borrow this
    /// client. An event loop can keep it pending while it drains tmux output,
    /// then spend the opaque capacity on the exact pane-input batch that was
    /// held. Closing the connection resolves the future as `Disconnected`.
    pub fn reserve_input_capacity(
        &self,
    ) -> impl std::future::Future<Output = Result<InputCapacity, TmuxError>> + Send + 'static {
        let reply_slots = Arc::clone(&self.pipe.reply_slots);
        async move {
            let owner = Arc::clone(&reply_slots);
            reply_slots
                .acquire_owned()
                .await
                .map(|permit| InputCapacity {
                    reply_slots: owner,
                    permit,
                })
                .map_err(|_| TmuxError::Disconnected)
        }
    }

    /// Forward one held pane-input batch using capacity reserved by
    /// [`ControlClient::reserve_input_capacity`].
    ///
    /// One input event must remain one tmux command. Refusing a batch that
    /// expands to another shape prevents a later command from escaping the
    /// reservation and failing after part of the event was written.
    pub async fn send_keys_unconfirmed_reserved(
        &self,
        pane_id: &str,
        keys: &[&str],
        capacity: InputCapacity,
    ) -> Result<(), TmuxError> {
        if !Arc::ptr_eq(&self.pipe.reply_slots, &capacity.reply_slots) {
            return Err(TmuxError::Disconnected);
        }
        let mut commands = send_keys_commands(pane_id, keys);
        if commands.len() != 1 {
            return Err(TmuxError::Protocol(format!(
                "reserved pane input expanded to {} commands",
                commands.len()
            )));
        }
        let cmd = commands.pop().expect("one reserved command");
        let (tx, _reply) = oneshot::channel();
        validate_command_line(&cmd)?;
        self.pipe
            .write_target(&cmd, ReplyTarget::Caller(tx), capacity.permit)
            .await
    }

    /// Load bytes into a named tmux buffer.
    ///
    /// Control-mode stdin is the command channel, so the content travels via
    /// a spool file that `load-buffer` reads; the file is created 0o600
    /// (payloads are never world-readable, even transiently) under the
    /// held state root, configured spool dir, or system temp dir. A held
    /// root cleans up only the exact published inode. Every form deletes
    /// the file after the command. Buffer names are caller-owned; the daemon
    /// guarantees per-delivery uniqueness (amendment e, F4: named buffers
    /// are server-global and concurrent reuse corrupts).
    pub async fn load_buffer(&self, name: &str, bytes: &[u8]) -> Result<(), TmuxError> {
        debug_assert!(!name.is_empty(), "buffer name must be nonempty");
        let seq = self.buffer_file_seq.fetch_add(1, Ordering::Relaxed);
        let state_file = match &self.state_spool {
            Some((root, directory)) => Some(create_state_spool_file(root, directory, seq, bytes)?),
            None => None,
        };
        let path = match &state_file {
            Some(file) => file.path().to_path_buf(),
            None => write_spool_file(self.spool_dir.as_deref(), seq, bytes).await?,
        };
        let cmd = format!(
            "load-buffer -b {} {}",
            quote_arg(name),
            quote_arg(&path.to_string_lossy())
        );
        let res = self.command(&cmd).await;
        // Message content must not linger on disk.
        match state_file {
            Some(file) => {
                let _ = file.remove();
            }
            None => {
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
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

    /// Delete one named server-global buffer. Used as cleanup when a
    /// load succeeded but the matching paste did not consume it with `-d`.
    pub async fn delete_buffer(&self, name: &str) -> Result<(), TmuxError> {
        self.command(&format!("delete-buffer -b {}", quote_arg(name)))
            .await
            .map(|_| ())
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

fn parse_server_pane_pid(lines: &[String], pane_id: &str) -> Result<Option<i32>, TmuxError> {
    let mut found = None;
    for line in lines {
        let Some((id, raw_pid)) = line.split_once('\t') else {
            return Err(TmuxError::Protocol(format!(
                "list-panes -a line has no pid field: {line:?}"
            )));
        };
        if id != pane_id {
            continue;
        }
        if raw_pid.is_empty() {
            return Ok(None);
        }
        let pid = raw_pid.parse::<i32>().map_err(|_| {
            TmuxError::Protocol(format!("list-panes -a has invalid pane pid: {line:?}"))
        })?;
        if found.is_some_and(|prior| prior != pid) {
            return Err(TmuxError::Protocol(format!(
                "list-panes -a disagrees about {pane_id}: {line:?}"
            )));
        }
        found = Some(pid);
    }
    Ok(found)
}

fn send_keys_commands(pane_id: &str, keys: &[&str]) -> Vec<String> {
    let target = quote_arg(pane_id);
    let mut commands = Vec::new();
    let mut i = 0;
    while i < keys.len() {
        let first_is_key = is_key_name(keys[i]);
        let mut j = i + 1;
        while j < keys.len() && is_key_name(keys[j]) == first_is_key {
            j += 1;
        }
        let mut cmd = format!("send-keys -t {target}");
        if !first_is_key {
            cmd.push_str(" -l");
        }
        cmd.push_str(" --");
        for key in &keys[i..j] {
            cmd.push(' ');
            cmd.push_str(&quote_arg(key));
        }
        commands.push(cmd);
        i = j;
    }
    commands
}

/// Write one load-buffer payload spool file: exclusive create, mode 0o600,
/// under `dir` (created 0o700 when missing) or the system temp dir. Names
/// are process-global and collisions are retried without removing anything.
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
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    for _ in 0..32 {
        let path = dir.join(spool_file_name(seq));
        let mut file = match opts.open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        file.write_all(bytes).await?;
        file.flush().await?;
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a tmux spool file",
    )
    .into())
}

fn create_state_spool_file(
    root: &cyclops_state::StateRoot,
    directory: &std::path::Path,
    seq: u64,
    bytes: &[u8],
) -> Result<cyclops_state::TransientStateFile, TmuxError> {
    for _ in 0..32 {
        let descendant = directory.join(spool_file_name(seq));
        match root.create_transient_file(&descendant, bytes) {
            Ok(file) => return Ok(file),
            Err(cyclops_state::StateError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(error) => return Err(std::io::Error::other(error.to_string()).into()),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a state tmux spool file",
    )
    .into())
}

fn spool_file_name(seq: u64) -> String {
    let unique = SPOOL_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("cyclops-buf-{}-{seq}-{unique}", std::process::id())
}

/// Reads the control stream: resolves reply blocks against the pending FIFO
/// and forwards notifications. Auto-resumes paused panes (amendment a).
/// On EOF it closes the pipe, which fails every waiting command with
/// Disconnected, and drops the notification sender so the consumer sees the
/// stream end.
async fn reader_task(stdout: ChildStdout, pipe: CommandPipe, notif_tx: NotificationSink) {
    // Byte lines, never UTF-8 lines. tmux escapes control bytes octally but
    // passes bytes >= 0x80 through verbatim, and a multi-byte character
    // split across two pty reads produces two lines that are each invalid
    // UTF-8 on their own (MEASURED on 3.6a, F22). A UTF-8-decoding reader
    // dies on the first such line and takes the connection with it; that
    // was the M1 soak's 8-drops-in-80s bug under a Claude TUI pane.
    let mut reader = BufReader::new(stdout);
    let mut line: Vec<u8> = Vec::new();
    let mut router = LineRouter::new();
    let (resume_tx, mut resume_rx) = mpsc::channel::<String>(PAUSE_RESUME_CAPACITY);
    let resume_pipe = pipe.clone();
    tokio::spawn(async move {
        while let Some(pane) = resume_rx.recv().await {
            let cmd = format!(
                "refresh-client -A {}",
                quote_arg(&format!("{pane}:continue"))
            );
            if let Err(error) = resume_pipe.submit_resume(&cmd, pane.clone()).await {
                warn!(%error, %pane, "failed to submit paused-pane resume");
                resume_pipe.close().await;
                return;
            }
        }
    });
    loop {
        line.clear();
        match read_control_line(&mut reader, &mut line).await {
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
                    let pane = pane.clone();
                    if enqueue_notification(&notif_tx, n).exceeds_envelope() {
                        warn!(
                            bytes = NOTIFICATION_MAX_QUEUED_BYTES,
                            "tmux notification exceeded its queue envelope; reconnecting"
                        );
                        break;
                    }
                    match resume_tx.try_send(pane.clone()) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            warn!(%pane, "paused-pane resume worker stopped; reconnecting");
                            break;
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            warn!(
                                capacity = PAUSE_RESUME_CAPACITY,
                                %pane,
                                "paused-pane resume queue overflowed; reconnecting"
                            );
                            break;
                        }
                    }
                    continue;
                }
                if let Notification::Other(raw) = &n {
                    debug!(line = %raw, "unrecognized control line");
                }
                // Consumer gone is fine; keep draining for command replies.
                if enqueue_notification(&notif_tx, n).exceeds_envelope() {
                    warn!(
                        bytes = NOTIFICATION_MAX_QUEUED_BYTES,
                        "tmux notification exceeded its queue envelope; reconnecting"
                    );
                    break;
                }
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
                    Some(ReplySlot {
                        target: ReplyTarget::Caller(tx),
                        ..
                    }) => {
                        let _ = tx.send(result.map_err(TmuxError::Command));
                    }
                    Some(ReplySlot {
                        target: ReplyTarget::Resume { pane },
                        ..
                    }) => match result {
                        Ok(_) => {
                            // tmux 3.7b may omit `%continue`. The successful
                            // correlated reply is authoritative and is
                            // emitted before the reader accepts another line.
                            if enqueue_notification(
                                &notif_tx,
                                Notification::Continue { pane: pane.clone() },
                            )
                            .exceeds_envelope()
                            {
                                warn!(
                                    bytes = NOTIFICATION_MAX_QUEUED_BYTES,
                                    %pane,
                                    "tmux resume notification exceeded its queue envelope; reconnecting"
                                );
                                break;
                            }
                        }
                        Err(error) => {
                            warn!(%error, %pane, "tmux rejected paused-pane resume");
                        }
                    },
                    None => debug!("reply block with no waiting caller"),
                }
            }
            Some(Routed::BlockOverflow) => {
                warn!(
                    bytes = CONTROL_BLOCK_MAX_BYTES,
                    lines = CONTROL_BLOCK_MAX_LINES,
                    "tmux reply block exceeded its envelope; reconnecting"
                );
                break;
            }
        }
    }
    pipe.close().await;
}

/// Read one byte line without allowing a malformed or hostile control record
/// to grow the allocation past the transport envelope.
async fn read_control_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<usize> {
    line.clear();
    loop {
        let (take, complete) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return Ok(line.len());
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |index| index + 1);
            if line.len().saturating_add(take) > CONTROL_LINE_MAX_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("tmux control line exceeds {CONTROL_LINE_MAX_BYTES} bytes"),
                ));
            }
            line.extend_from_slice(&available[..take]);
            (take, newline.is_some())
        };
        reader.consume(take);
        if complete {
            return Ok(line.len());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnqueueResult {
    Sent,
    ReceiverClosed,
    GapRecorded,
    EnvelopeExceeded,
}

impl EnqueueResult {
    fn exceeds_envelope(self) -> bool {
        matches!(self, Self::EnvelopeExceeded)
    }
}

/// Queue one notification without ever delaying correlated reply ingress.
///
/// The regular queue owns fixed item and byte permits. Its channel has one
/// additional slot reserved for [`Notification::ContinuityLost`]. When
/// either permit is unavailable, the first dropped notification places that
/// marker after the retained prefix. Later notifications are dropped until
/// the consumer confirms an authoritative reconciliation, so every
/// discontinuity is explicit and coalesced without unbounded memory or a
/// retry loop.
fn enqueue_notification(tx: &NotificationSink, notification: Notification) -> EnqueueResult {
    let bytes = notification_retained_bytes(&notification).max(1);
    if bytes > NOTIFICATION_MAX_QUEUED_BYTES {
        return EnqueueResult::EnvelopeExceeded;
    }
    let mut gap = tx.gap.lock().expect("notification gap lock");
    if gap.pending {
        gap.epoch = gap.epoch.wrapping_add(1);
        return EnqueueResult::GapRecorded;
    }
    let Ok(bytes) = u32::try_from(bytes) else {
        return EnqueueResult::EnvelopeExceeded;
    };
    let item_permit = match Arc::clone(&tx.items).try_acquire_owned() {
        Ok(permit) => permit,
        Err(tokio::sync::TryAcquireError::NoPermits) => {
            return record_notification_gap(tx, &mut gap);
        }
        Err(tokio::sync::TryAcquireError::Closed) => return EnqueueResult::ReceiverClosed,
    };
    let byte_permit = match Arc::clone(&tx.bytes).try_acquire_many_owned(bytes) {
        Ok(permit) => permit,
        Err(tokio::sync::TryAcquireError::NoPermits) => {
            return record_notification_gap(tx, &mut gap);
        }
        Err(tokio::sync::TryAcquireError::Closed) => return EnqueueResult::ReceiverClosed,
    };
    match tx.tx.try_send(QueuedNotification {
        notification,
        _item_permit: Some(item_permit),
        _byte_permit: Some(byte_permit),
    }) {
        Ok(()) => EnqueueResult::Sent,
        Err(mpsc::error::TrySendError::Closed(_)) => EnqueueResult::ReceiverClosed,
        Err(mpsc::error::TrySendError::Full(_)) => record_notification_gap(tx, &mut gap),
    }
}

fn record_notification_gap(tx: &NotificationSink, gap: &mut NotificationGap) -> EnqueueResult {
    if gap.pending {
        gap.epoch = gap.epoch.wrapping_add(1);
        return EnqueueResult::GapRecorded;
    }
    gap.pending = true;
    gap.epoch = gap.epoch.wrapping_add(1);
    match tx.tx.try_send(QueuedNotification {
        notification: Notification::ContinuityLost,
        _item_permit: None,
        _byte_permit: None,
    }) {
        Ok(()) => EnqueueResult::GapRecorded,
        Err(mpsc::error::TrySendError::Closed(_)) => {
            gap.pending = false;
            EnqueueResult::ReceiverClosed
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Regular entries can consume only `NOTIFICATION_CAPACITY`
            // slots, leaving one for this marker. Reaching this arm means
            // the queue invariants were broken, so reconnect rather than
            // continue after silent loss.
            gap.pending = false;
            EnqueueResult::EnvelopeExceeded
        }
    }
}

fn notification_retained_bytes(notification: &Notification) -> usize {
    match notification {
        Notification::ContinuityLost => 0,
        Notification::Output { pane, data } | Notification::ExtendedOutput { pane, data, .. } => {
            pane.len().saturating_add(data.len())
        }
        Notification::SessionChanged { session, name }
        | Notification::WindowRenamed {
            window: session,
            name,
        }
        | Notification::UnlinkedWindowRenamed {
            window: session,
            name,
        }
        | Notification::WindowPaneChanged {
            window: session,
            pane: name,
        } => session.len().saturating_add(name.len()),
        Notification::SessionRenamed { session, name } => session
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(name.len()),
        Notification::ClientSessionChanged {
            client,
            session,
            name,
        } => client
            .len()
            .saturating_add(session.len())
            .saturating_add(name.len()),
        Notification::SubscriptionChanged {
            name,
            session,
            window,
            pane,
            value,
        } => name
            .len()
            .saturating_add(session.as_ref().map_or(0, String::len))
            .saturating_add(window.as_ref().map_or(0, String::len))
            .saturating_add(pane.as_ref().map_or(0, String::len))
            .saturating_add(value.len()),
        Notification::WindowAdd { window }
        | Notification::WindowClose { window }
        | Notification::UnlinkedWindowAdd { window }
        | Notification::UnlinkedWindowClose { window } => window.len(),
        Notification::LayoutChange { window, rest } => window.len().saturating_add(rest.len()),
        Notification::PaneModeChanged { pane }
        | Notification::Pause { pane }
        | Notification::Continue { pane } => pane.len(),
        Notification::ClientDetached { client } => client.len(),
        Notification::Exit { reason } => reason.as_ref().map_or(0, String::len),
        Notification::Other(raw) => raw.len(),
        Notification::SessionsChanged => 0,
    }
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

    fn command_pipe_without_replies(capacity: usize) -> (CommandPipe, Child) {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn command sink");
        let stdin = child.stdin.take().expect("command sink stdin");
        (
            CommandPipe {
                stdin: Arc::new(Mutex::new(Some(stdin))),
                pending: Arc::new(StdMutex::new(VecDeque::new())),
                reply_slots: Arc::new(Semaphore::new(capacity)),
                issued: Arc::new(AtomicU64::new(0)),
            },
            child,
        )
    }

    /// The classification seam, deterministically: a writer that fails
    /// before accepting a single byte yields `Unwritten`, no OS pipe timing
    /// involved. The partial and flush cases below prove `Uncertain`.
    #[tokio::test]
    async fn a_failure_before_the_first_command_byte_is_proven_unwritten() {
        struct FirstWriteFails;

        impl AsyncWrite for FirstWriteFails {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed before the first byte",
                )))
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let mut writer = FirstWriteFails;
        assert!(matches!(
            write_command_line(&mut writer, b"display-message -p not-sent\n").await,
            Err(CommandWriteError::Unwritten(_))
        ));
    }

    /// A classified write failure poisons the pipe, fails pending callers,
    /// and prevents a later command from entering the broken FIFO.
    #[tokio::test]
    async fn a_write_failure_poisons_the_pipe_and_fails_pending_callers() {
        let (pipe, mut child) = command_pipe_without_replies(2);
        let (tx, rx) = oneshot::channel();
        let permit = Arc::clone(&pipe.reply_slots)
            .try_acquire_owned()
            .expect("reply capacity");
        pipe.pending
            .lock()
            .expect("pending lock")
            .push_back(ReplySlot {
                target: ReplyTarget::Caller(tx),
                _permit: permit,
            });

        let mapped = {
            let mut stdin = pipe.stdin.lock().await;
            pipe.poison_after_write_failure(
                &mut *stdin,
                CommandWriteError::Unwritten(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed before the first byte",
                )),
            )
        };
        assert!(matches!(mapped, TmuxError::Io(_)));
        assert!(matches!(rx.await, Ok(Err(TmuxError::Disconnected))));
        assert!(pipe.stdin.lock().await.is_none());
        assert!(pipe.pending.lock().expect("pending lock").is_empty());
        assert!(matches!(
            pipe.submit("display-message -p never-replay").await,
            Err(TmuxError::Disconnected)
        ));
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("command sink exits after stdin closes")
            .expect("wait for command sink");
        assert!(status.success(), "command sink exit: {status}");
    }

    #[tokio::test]
    async fn a_partial_command_write_is_uncertain() {
        struct PartialFailure(bool);

        impl AsyncWrite for PartialFailure {
            fn poll_write(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                bytes: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                if self.0 {
                    std::task::Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "write failed after a prefix",
                    )))
                } else {
                    self.0 = true;
                    std::task::Poll::Ready(Ok(bytes.len().min(3)))
                }
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        assert!(matches!(
            write_command_line(
                &mut PartialFailure(false),
                b"display-message -p uncertain\n"
            )
            .await,
            Err(CommandWriteError::Uncertain(_))
        ));
    }

    #[tokio::test]
    async fn a_flush_failure_is_observed_after_the_full_command_write() {
        struct FlushFailure;

        impl AsyncWrite for FlushFailure {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                bytes: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Ok(bytes.len()))
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "flush failed",
                )))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        assert!(matches!(
            write_command_line(&mut FlushFailure, b"display-message -p sent\n").await,
            Err(CommandWriteError::Uncertain(error))
                if error.kind() == std::io::ErrorKind::BrokenPipe
        ));
    }

    fn notification_channel(
        item_capacity: usize,
        byte_capacity: usize,
    ) -> (NotificationSink, NotificationReceiver) {
        let (tx, rx) = mpsc::channel(item_capacity + 1);
        let gap = Arc::new(StdMutex::new(NotificationGap::default()));
        (
            NotificationSink {
                tx,
                bytes: Arc::new(Semaphore::new(byte_capacity)),
                items: Arc::new(Semaphore::new(item_capacity)),
                gap: Arc::clone(&gap),
            },
            NotificationReceiver {
                rx: NotificationQueue::Budgeted { rx, gap },
            },
        )
    }

    #[tokio::test]
    async fn reserved_input_capacity_never_exceeds_the_reply_bound() {
        let (pipe, mut child) = command_pipe_without_replies(PENDING_REPLY_CAPACITY);
        for index in 0..PENDING_REPLY_CAPACITY {
            let _reply = pipe
                .submit(&format!("display-message -p {index}"))
                .await
                .expect("fill bounded reply queue");
        }
        assert_eq!(pipe.pending.lock().expect("pending lock").len(), 64);
        assert!(matches!(
            pipe.submit("display-message -p fail-fast").await,
            Err(TmuxError::Busy)
        ));

        let waiting_pipe = pipe.clone();
        let mut waiting = tokio::spawn(async move {
            let permit = waiting_pipe.reserve_capacity().await?;
            let (tx, rx) = oneshot::channel();
            waiting_pipe
                .write_target(
                    "display-message -p reserved",
                    ReplyTarget::Caller(tx),
                    permit,
                )
                .await?;
            Ok::<_, TmuxError>(rx)
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err(),
            "awaited input must remain pending while all reply slots are held"
        );
        assert_eq!(pipe.issued.load(Ordering::Relaxed), 64);

        let completed = pipe.pending.lock().expect("pending lock").pop_front();
        drop(completed);
        let _reply = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("capacity waiter wakes")
            .expect("capacity task completes")
            .expect("reserved command is written");
        assert_eq!(pipe.issued.load(Ordering::Relaxed), 65);
        assert_eq!(pipe.pending.lock().expect("pending lock").len(), 64);

        pipe.close().await;
        child.wait().await.expect("command sink exits");
    }

    #[tokio::test]
    async fn closing_the_pipe_wakes_capacity_waiters_as_disconnected() {
        let (pipe, mut child) = command_pipe_without_replies(1);
        let _reply = pipe
            .submit("display-message -p first")
            .await
            .expect("fill reply queue");

        let waiting_pipe = pipe.clone();
        let mut waiting = tokio::spawn(async move { waiting_pipe.reserve_capacity().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err(),
            "second command must wait for bounded capacity"
        );

        pipe.close().await;
        let result = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("closed pipe wakes waiter")
            .expect("capacity task completes");
        assert!(matches!(result, Err(TmuxError::Disconnected)));
        child.wait().await.expect("command sink exits");
    }

    #[tokio::test]
    async fn paused_pane_resume_waits_for_reply_capacity_without_closing_the_pipe() {
        let (pipe, mut child) = command_pipe_without_replies(1);
        let _reply = pipe
            .submit("display-message -p first")
            .await
            .expect("fill reply queue");

        let waiting_pipe = pipe.clone();
        let mut waiting = tokio::spawn(async move {
            waiting_pipe
                .submit_resume("refresh-client -A %1:continue", "%1".to_string())
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err(),
            "resume must wait while the reply FIFO is full"
        );

        drop(pipe.pending.lock().expect("pending lock").pop_front());
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("resume wakes when capacity opens")
            .expect("resume task completes")
            .expect("resume command is written");
        assert_eq!(pipe.pending.lock().expect("pending lock").len(), 1);

        pipe.close().await;
        child.wait().await.expect("command sink exits");
    }

    #[tokio::test]
    async fn pause_is_observed_before_its_correlated_continue() {
        let script = r#"
            printf '%s\n' '%pause %5'
            IFS= read -r command
            test "$command" = "refresh-client -A '%5:continue'" || exit 42
            printf '%s\n' '%begin 1 1 1' '%end 1 1 1'
        "#;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn scripted control peer");
        let stdin = child.stdin.take().expect("script stdin");
        let stdout = child.stdout.take().expect("script stdout");
        let pipe = CommandPipe {
            stdin: Arc::new(Mutex::new(Some(stdin))),
            pending: Arc::new(StdMutex::new(VecDeque::new())),
            reply_slots: Arc::new(Semaphore::new(PENDING_REPLY_CAPACITY)),
            issued: Arc::new(AtomicU64::new(0)),
        };
        let (sink, mut rx) = notification_channel(4, 1024);

        let reader = tokio::spawn(reader_task(stdout, pipe, sink));
        let pause = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("pause arrives")
            .expect("notification queue stays open");
        assert_eq!(pause, Notification::Pause { pane: "%5".into() });
        let resume = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("correlated continue arrives")
            .expect("notification queue stays open");
        assert_eq!(resume, Notification::Continue { pane: "%5".into() });

        reader.await.expect("reader task exits");
        let status = child.wait().await.expect("script exits");
        assert!(status.success(), "script rejected the resume command");
    }

    #[tokio::test]
    async fn correlated_reply_passes_two_saturated_notification_hops() {
        let script = r#"
            IFS= read -r command
            test "$command" = "display-message -p answer" || exit 42
            index=0
            while test "$index" -lt 12; do
                printf '%s\n' '%sessions-changed'
                index=$((index + 1))
            done
            printf '%s\n' '%begin 1 1 1' 'answer' '%end 1 1 1'
            IFS= read -r hold
        "#;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn scripted control peer");
        let stdin = child.stdin.take().expect("script stdin");
        let stdout = child.stdout.take().expect("script stdout");
        let pipe = CommandPipe {
            stdin: Arc::new(Mutex::new(Some(stdin))),
            pending: Arc::new(StdMutex::new(VecDeque::new())),
            reply_slots: Arc::new(Semaphore::new(PENDING_REPLY_CAPACITY)),
            issued: Arc::new(AtomicU64::new(0)),
        };
        let (sink, mut notifications) = notification_channel(2, 1024);
        let reader = tokio::spawn(reader_task(stdout, pipe.clone(), sink));

        // Model the workspace's second bounded hop. Its consumer is busy
        // awaiting the command below, so the forwarder blocks after taking
        // one item out of the control queue.
        let (workspace_tx, mut workspace_rx) = mpsc::channel(1);
        workspace_tx
            .try_send(Notification::Other("occupied".into()))
            .expect("fill workspace notification hop");
        let forwarder = tokio::spawn(async move {
            while let Some(notification) = notifications.recv().await {
                if workspace_tx.send(notification).await.is_err() {
                    break;
                }
            }
        });

        let reply = pipe
            .submit("display-message -p answer")
            .await
            .expect("command enters the bounded reply FIFO");
        let answer = tokio::time::timeout(Duration::from_secs(1), reply)
            .await
            .expect("notification saturation must not delay the reply")
            .expect("reply sender stays live")
            .expect("script answers successfully");
        assert_eq!(answer, vec!["answer".to_string()]);
        assert!(
            !reader.is_finished(),
            "the healthy control connection must not reconnect to make progress"
        );

        assert!(matches!(
            workspace_rx.recv().await,
            Some(Notification::Other(value)) if value == "occupied"
        ));
        let mut retained_prefix = 0usize;
        loop {
            match workspace_rx.recv().await {
                Some(Notification::SessionsChanged) => retained_prefix += 1,
                Some(Notification::ContinuityLost) => break,
                other => panic!("unexpected notification before continuity gap: {other:?}"),
            }
        }
        assert!((1..=3).contains(&retained_prefix));

        pipe.close().await;
        reader
            .await
            .expect("reader task exits after explicit close");
        forwarder.await.expect("notification forwarder exits");
        child.wait().await.expect("script exits");
    }

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
    fn router_refuses_reply_blocks_past_either_envelope() {
        let mut bytes = LineRouter::new();
        bytes.block = Some(OpenBlock {
            client: true,
            command: Some(7),
            lines: Vec::new(),
            bytes: CONTROL_BLOCK_MAX_BYTES,
        });
        assert_eq!(bytes.feed(b"x"), Some(Routed::BlockOverflow));
        assert!(bytes.block.is_none());

        let mut lines = LineRouter::new();
        lines.block = Some(OpenBlock {
            client: true,
            command: Some(8),
            lines: vec![String::new(); CONTROL_BLOCK_MAX_LINES],
            bytes: 0,
        });
        assert_eq!(lines.feed(b""), Some(Routed::BlockOverflow));
        assert!(lines.block.is_none());
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

    #[test]
    fn notification_overflow_preserves_the_prefix_and_coalesces_one_gap() {
        let (tx, mut rx) = notification_channel(1, NOTIFICATION_MAX_QUEUED_BYTES);
        assert_eq!(
            enqueue_notification(&tx, Notification::SessionsChanged),
            EnqueueResult::Sent
        );
        assert_eq!(
            enqueue_notification(
                &tx,
                Notification::WindowAdd {
                    window: "@1".into(),
                },
            ),
            EnqueueResult::GapRecorded
        );
        assert_eq!(
            enqueue_notification(
                &tx,
                Notification::WindowClose {
                    window: "@2".into()
                }
            ),
            EnqueueResult::GapRecorded,
            "one gap covers the whole dropped segment"
        );
        assert!(matches!(rx.try_recv(), Ok(Notification::SessionsChanged)));
        assert!(matches!(rx.try_recv(), Ok(Notification::ContinuityLost)));
        assert_eq!(
            enqueue_notification(&tx, Notification::SessionsChanged),
            EnqueueResult::GapRecorded,
            "reading the marker alone cannot admit a new segment"
        );
        let epoch = rx.hold_continuity();
        assert!(rx.resume_after_reconcile(epoch));
        assert_eq!(
            enqueue_notification(&tx, Notification::SessionsChanged),
            EnqueueResult::Sent,
            "authoritative reconciliation starts one new segment"
        );
        drop(rx);
        assert_eq!(
            enqueue_notification(&tx, Notification::SessionsChanged),
            EnqueueResult::ReceiverClosed
        );
    }

    #[test]
    fn notification_payload_budget_records_a_gap_independently_of_item_capacity() {
        let (tx, mut rx) = notification_channel(2, 4);
        assert_eq!(
            enqueue_notification(
                &tx,
                Notification::Output {
                    pane: "%0".into(),
                    data: b"ab".to_vec(),
                },
            ),
            EnqueueResult::Sent
        );
        assert_eq!(
            enqueue_notification(&tx, Notification::SessionsChanged),
            EnqueueResult::GapRecorded,
            "byte exhaustion records loss without waiting"
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(Notification::Output { pane, data })
                if pane == "%0" && data == b"ab"
        ));
        assert!(matches!(rx.try_recv(), Ok(Notification::ContinuityLost)));
    }

    #[test]
    fn an_event_after_the_snapshot_refuses_the_continuity_cutover() {
        let (tx, mut rx) = notification_channel(1, NOTIFICATION_MAX_QUEUED_BYTES);
        assert_eq!(
            enqueue_notification(&tx, Notification::SessionsChanged),
            EnqueueResult::Sent
        );
        assert_eq!(
            enqueue_notification(&tx, Notification::SessionsChanged),
            EnqueueResult::GapRecorded
        );
        assert!(matches!(rx.try_recv(), Ok(Notification::SessionsChanged)));
        assert!(matches!(rx.try_recv(), Ok(Notification::ContinuityLost)));

        let snapshot_epoch = rx.hold_continuity();
        assert_eq!(
            enqueue_notification(
                &tx,
                Notification::Output {
                    pane: "%0".into(),
                    data: b"after capture".to_vec(),
                },
            ),
            EnqueueResult::GapRecorded
        );
        assert!(
            !rx.resume_after_reconcile(snapshot_epoch),
            "an event after capture keeps the source barrier closed"
        );
    }

    #[test]
    fn notification_larger_than_the_byte_envelope_is_refused() {
        let (tx, _rx) = notification_channel(1, NOTIFICATION_MAX_QUEUED_BYTES);
        assert_eq!(
            enqueue_notification(
                &tx,
                Notification::Output {
                    pane: "%0".into(),
                    data: vec![b'x'; NOTIFICATION_MAX_QUEUED_BYTES],
                },
            ),
            EnqueueResult::EnvelopeExceeded
        );
    }

    #[tokio::test]
    async fn control_lines_have_a_preallocation_byte_limit() {
        let mut exact = vec![b'x'; CONTROL_LINE_MAX_BYTES];
        exact[CONTROL_LINE_MAX_BYTES - 1] = b'\n';
        let mut reader = BufReader::new(exact.as_slice());
        let mut line = Vec::new();
        assert_eq!(
            read_control_line(&mut reader, &mut line).await.unwrap(),
            CONTROL_LINE_MAX_BYTES
        );

        let oversized = vec![b'x'; CONTROL_LINE_MAX_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        let error = read_control_line(&mut reader, &mut line)
            .await
            .expect_err("an oversized control line must close the connection");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(line.len() <= CONTROL_LINE_MAX_BYTES);
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
    fn state_spool_is_owner_only_and_exactly_cleaned_up() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = cyclops_proto::scratch::scratch_dir("cyclops-state-spool-test");
        let _ = std::fs::remove_dir_all(&base);
        let root = cyclops_state::StateRoot::open_or_create(&base).unwrap();
        let file =
            create_state_spool_file(&root, std::path::Path::new("spool"), 9, b"secret").unwrap();
        let path = file.path().to_path_buf();

        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(file);
        assert!(!path.exists());
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

    #[test]
    fn server_pane_lookup_accepts_linked_duplicates_but_rejects_disagreement() {
        let linked = vec!["%7\t410".into(), "%2\t220".into(), "%7\t410".into()];
        assert_eq!(parse_server_pane_pid(&linked, "%7").unwrap(), Some(410));
        assert_eq!(parse_server_pane_pid(&linked, "%9").unwrap(), None);

        let disagreement = vec!["%7\t410".into(), "%7\t411".into()];
        assert!(matches!(
            parse_server_pane_pid(&disagreement, "%7"),
            Err(TmuxError::Protocol(_))
        ));
        assert!(matches!(
            parse_server_pane_pid(&["%7".into()], "%7"),
            Err(TmuxError::Protocol(_))
        ));
    }

    #[test]
    fn a_literal_containing_esc_survives_the_dash_l_command_intact() {
        // An SGR mouse report (e.g. the wheel-forwarding byte string for an
        // alt-screen pane with mouse reporting on) is not a tmux key name,
        // so it must ride the `-l` literal path with every byte — ESC
        // included — untouched; `quote_arg` only strips \n, \r, and \0.
        let report = "\x1b[<64;1;1M";
        let commands = send_keys_commands("%3", &[report]);
        assert_eq!(
            commands,
            vec![format!("send-keys -t '%3' -l -- '{report}'")],
            "the ESC byte must reach the generated command unmodified"
        );
    }
}
