//! cyclopsd entry point: logging, config, signals. Everything else lives
//! in the library so integration tests can boot the daemon in-process.

use std::collections::VecDeque;
use std::fmt;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::Parser;
use cyclops_state::{StateFile, StateRoot};
use tracing::{error, info, warn};
use tracing_subscriber::fmt::MakeWriter;

use cyclopsd::Config;

const VERSION: &str = cyclops_proto::VERSION_WITH_BUILD;

#[derive(Parser)]
#[command(
    name = "cyclopsd",
    version = VERSION,
    about = "The Cyclops daemon for pane state, durable mailboxes, and safe notifications"
)]
struct Cli {}

const DAEMON_LOG_BYTES: usize = 1024 * 1024;
const DAEMON_LOG_RETAIN_BYTES: usize = 512 * 1024;
const DAEMON_LOG_EVENT_BYTES: usize = 64 * 1024;
const DAEMON_PANIC_BYTES: usize = 4096;
const EVENT_TRUNCATED: &[u8] = b"[earlier log bytes truncated] ";

/// Descriptor-bound sink for the daemon's process-lifetime tracing output.
#[derive(Clone)]
struct DaemonLog {
    file: Arc<Mutex<StateFile>>,
}

impl DaemonLog {
    fn open(home: &Path) -> Result<Self, cyclops_state::StateError> {
        let root = StateRoot::open_or_create(home)?;
        let file = root.open_append(Path::new("cyclopsd.log"))?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    fn append(&self, record: &[u8]) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("daemon log writer lock is poisoned"))?;
        file.append_bounded(record, DAEMON_LOG_BYTES, DAEMON_LOG_RETAIN_BYTES)
    }

    fn try_append(&self, record: &[u8]) {
        let Ok(mut file) = self.file.try_lock() else {
            return;
        };
        let _ = file.try_append_bounded(record, DAEMON_LOG_BYTES, DAEMON_LOG_RETAIN_BYTES);
    }
}

/// One tracing event buffered to a fixed limit before one bounded append.
struct DaemonEventWriter {
    log: DaemonLog,
    bytes: VecDeque<u8>,
    truncated: bool,
    committed: bool,
}

impl DaemonEventWriter {
    fn new(log: DaemonLog) -> Self {
        Self {
            log,
            bytes: VecDeque::with_capacity(DAEMON_LOG_EVENT_BYTES),
            truncated: false,
            committed: false,
        }
    }

    fn keep_tail(&mut self, bytes: &[u8]) {
        let limit = DAEMON_LOG_EVENT_BYTES - EVENT_TRUNCATED.len();
        if bytes.len() >= limit {
            self.bytes.clear();
            self.bytes
                .extend(bytes[bytes.len() - limit..].iter().copied());
            return;
        }
        while self.bytes.len().saturating_add(bytes.len()) > limit {
            let _ = self.bytes.pop_front();
        }
        self.bytes.extend(bytes.iter().copied());
    }

    fn commit(&mut self) -> io::Result<()> {
        if self.committed {
            return Ok(());
        }
        self.committed = true;
        if self.bytes.is_empty() {
            return Ok(());
        }
        while self.truncated
            && self
                .bytes
                .front()
                .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
        {
            let _ = self.bytes.pop_front();
        }
        let mut record = Vec::with_capacity(EVENT_TRUNCATED.len() + self.bytes.len());
        if self.truncated {
            record.extend_from_slice(EVENT_TRUNCATED);
        }
        record.extend(self.bytes.iter().copied());
        self.log.append(&record)
    }
}

impl io::Write for DaemonEventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.committed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "daemon log event is already committed",
            ));
        }
        if !self.truncated && self.bytes.len().saturating_add(bytes.len()) <= DAEMON_LOG_EVENT_BYTES
        {
            self.bytes.extend(bytes.iter().copied());
            return Ok(bytes.len());
        }
        if !self.truncated {
            self.truncated = true;
            let limit = DAEMON_LOG_EVENT_BYTES - EVENT_TRUNCATED.len();
            while self.bytes.len() > limit {
                let _ = self.bytes.pop_front();
            }
        }
        self.keep_tail(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit()
    }
}

struct BoundedLine {
    text: String,
    limit: usize,
}

impl fmt::Write for BoundedLine {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for character in value.chars() {
            let character = if matches!(character, '\n' | '\r') {
                ' '
            } else {
                character
            };
            if self.text.len() + character.len_utf8() > self.limit {
                break;
            }
            self.text.push(character);
        }
        Ok(())
    }
}

fn panic_record(value: &dyn fmt::Display) -> Vec<u8> {
    let mut line = BoundedLine {
        text: String::with_capacity(DAEMON_PANIC_BYTES),
        limit: DAEMON_PANIC_BYTES - 1,
    };
    let _ = write!(&mut line, "cyclopsd panic: {value}");
    let mut bytes = line.text.into_bytes();
    bytes.push(b'\n');
    bytes
}

impl Drop for DaemonEventWriter {
    fn drop(&mut self) {
        let _ = self.commit();
    }
}

impl<'a> MakeWriter<'a> for DaemonLog {
    type Writer = DaemonEventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        DaemonEventWriter::new(self.clone())
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    Cli::parse();
    let home = cyclops_proto::cyclops_home();
    let log = match DaemonLog::open(&home) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("cyclopsd: cannot open bounded log: {error}");
            return ExitCode::FAILURE;
        }
    };
    let panic_log = log.clone();
    // CYCLOPS_LOG uses EnvFilter syntax. The default is info.
    let filter = tracing_subscriber::EnvFilter::try_from_env("CYCLOPS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(log)
        .init();
    std::panic::set_hook(Box::new(move |panic| {
        panic_log.try_append(&panic_record(panic));
    }));

    let (cfg, warnings) = match Config::load(&home) {
        Ok(v) => v,
        Err(e) => {
            error!("config: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    for w in &warnings {
        warn!("config: {w}");
    }
    if cfg.sessions.is_empty() {
        info!("no sessions configured; watching nothing, status still answers");
    }

    let daemon = match cyclopsd::boot(cfg).await {
        Ok(d) => d,
        Err(e) => {
            error!("boot failed: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    info!(socket = %daemon.socket_path().display(), "cyclopsd ready");

    tokio::select! {
        _ = wait_for_signal() => {}
        _ = daemon.shutdown_requested() => info!("authenticated shutdown requested"),
    }
    daemon.shutdown().await;
    ExitCode::SUCCESS
}

/// Block until SIGINT or SIGTERM.
async fn wait_for_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("SIGINT received, shutting down"),
        _ = term.recv() => info!("SIGTERM received, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn daemon_log_caps_oversized_events_and_keeps_the_tail() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-daemon-log-oversize");
        let _ = fs::remove_dir_all(&home);
        let log = DaemonLog::open(&home).unwrap();
        let mut writer = DaemonEventWriter::new(log);
        let mut event = vec![b'x'; DAEMON_LOG_EVENT_BYTES * 2];
        event.extend_from_slice(b"latest failure context\n");

        writer.write_all(&event).unwrap();
        writer.flush().unwrap();

        let bytes = fs::read(home.join("cyclopsd.log")).unwrap();
        assert!(bytes.len() <= DAEMON_LOG_EVENT_BYTES);
        assert!(bytes.starts_with(EVENT_TRUNCATED));
        assert!(bytes.ends_with(b"latest failure context\n"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_log_caps_fragmented_oversized_events() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-daemon-log-fragmented");
        let _ = fs::remove_dir_all(&home);
        let log = DaemonLog::open(&home).unwrap();
        let mut writer = DaemonEventWriter::new(log);

        for _ in 0..DAEMON_LOG_EVENT_BYTES * 2 {
            writer.write_all(b"x").unwrap();
        }
        writer.write_all(b"latest fragmented context\n").unwrap();
        writer.flush().unwrap();

        let bytes = fs::read(home.join("cyclopsd.log")).unwrap();
        assert!(bytes.len() <= DAEMON_LOG_EVENT_BYTES);
        assert!(bytes.starts_with(EVENT_TRUNCATED));
        assert!(bytes.ends_with(b"latest fragmented context\n"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn panic_records_are_one_bounded_line() {
        let record = panic_record(&"failure\n".repeat(DAEMON_PANIC_BYTES));

        assert!(record.len() <= DAEMON_PANIC_BYTES);
        assert_eq!(record.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert!(record.starts_with(b"cyclopsd panic: failure failure"));
    }

    #[test]
    fn panic_log_never_waits_for_an_event_writer() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-daemon-panic-locked");
        let _ = fs::remove_dir_all(&home);
        let log = DaemonLog::open(&home).unwrap();
        let held = log.file.lock().unwrap();

        log.try_append(b"must not wait\n");
        assert_eq!(fs::read(home.join("cyclopsd.log")).unwrap(), b"");

        drop(held);
        log.try_append(b"written after release\n");
        assert_eq!(
            fs::read(home.join("cyclopsd.log")).unwrap(),
            b"written after release\n"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_log_stays_bounded_for_the_process_lifetime() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-daemon-log-bounded");
        let _ = fs::remove_dir_all(&home);
        let log = DaemonLog::open(&home).unwrap();

        for index in 0..20 {
            let mut writer = DaemonEventWriter::new(log.clone());
            let line = format!(
                "event {index:02} {}\n",
                "x".repeat(DAEMON_LOG_EVENT_BYTES - 16)
            );
            writer.write_all(line.as_bytes()).unwrap();
            writer.flush().unwrap();
        }

        let bytes = fs::read(home.join("cyclopsd.log")).unwrap();
        assert!(bytes.len() <= DAEMON_LOG_BYTES);
        assert!(bytes.ends_with(b"\n"));
        assert!(String::from_utf8(bytes).unwrap().contains("event 19"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn daemon_log_refuses_a_linked_file_without_mutating_it() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-daemon-log-linked");
        let external = cyclops_proto::scratch::scratch_dir("cyc-daemon-log-external");
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&external);
        let _root = StateRoot::open_or_create(&home).unwrap();
        fs::create_dir_all(&external).unwrap();
        let target = external.join("target.log");
        fs::write(&target, b"external bytes").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        fs::hard_link(&target, home.join("cyclopsd.log")).unwrap();

        assert!(DaemonLog::open(&home).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"external bytes");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&external);
    }
}
