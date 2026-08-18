//! Starting, stopping and reporting on cyclopsd, so a person does not
//! have to hold a terminal open for it.
//!
//! ## Why this exists
//!
//! `cyclopsd &` works and it is what the docs used to say. It also means
//! the daemon dies with the shell that started it, so the first run needs
//! two commands and a spare tab, and the order of those two commands
//! decides whether anything gets named (F33). Making `cyclops start` own
//! the daemon removes the ordering question rather than explaining it.
//!
//! ## The rules this follows
//!
//! 1. Never start a second one. The daemon already refuses to boot when
//!    another holds the socket; this checks first so the common case is
//!    silent rather than an error the caller has to interpret.
//! 2. Never leave a failure invisible. A daemon that dies at boot writes
//!    why to its log, and the caller reads the log rather than reporting
//!    a connection failure that says nothing (the SUN_LEN dead end).
//! 3. Never outlive its own output. Logs go to a file under the home, so
//!    a detached daemon is not writing onto somebody's terminal.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::client::{Client, ClientError};

/// How long to wait for a spawned daemon to answer its socket. Boot is
/// milliseconds; this is the margin for a loaded machine.
const BOOT_WAIT: Duration = Duration::from_secs(10);

/// Where a detached daemon's stderr goes.
pub fn log_path(home: &Path) -> PathBuf {
    home.join("cyclopsd.log")
}

/// Is a daemon answering right now?
pub fn is_up() -> bool {
    Client::connect().is_ok()
}

/// What happened when a caller asked for a running daemon.
pub enum Started {
    /// One was already there. Nothing was done.
    AlreadyRunning,
    /// This call started one, and it is answering.
    Spawned,
}

/// Make sure a daemon is running, starting one if it is not.
///
/// Steps, in the order they have to happen:
///
/// 1. Ask. A daemon that answers is the whole job, and asking is cheaper
///    than any check that guesses from a pid file or a socket's existence.
/// 2. Find the binary. Next to this one first, because the installer puts
///    the pair in the same directory and that copy is the matching build.
/// 3. Spawn it detached, with its output on a file.
/// 4. Wait for it to answer, and if it never does, say why using what it
///    wrote on the way down.
pub fn ensure_running(home: &Path) -> Result<Started, String> {
    // 1.
    if is_up() {
        return Ok(Started::AlreadyRunning);
    }

    // 2.
    let exe = binary().ok_or_else(|| {
        "cyclopsd is not next to cyclops and not on your PATH. Reinstall with \
         ./scripts/install.sh, which puts both in the same directory."
            .to_string()
    })?;

    // 3. The log is opened before the spawn so a failure to open it is
    //    this function's error rather than a daemon that starts with
    //    nowhere to write.
    std::fs::create_dir_all(home).map_err(|e| format!("create {}: {e}", home.display()))?;
    let log = log_path(home);
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("open {}: {e}", log.display()))?;
    let errs = out
        .try_clone()
        .map_err(|e| format!("open {}: {e}", log.display()))?;

    let mut cmd = Command::new(&exe);
    cmd.stdin(Stdio::null()).stdout(out).stderr(errs);
    // Its own process group, so a Ctrl-C meant for the shell that ran
    // `cyclops start` does not also kill the daemon it just started.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("start {}: {e}", exe.display()))?;

    // 4.
    let deadline = Instant::now() + BOOT_WAIT;
    loop {
        if is_up() {
            return Ok(Started::Spawned);
        }
        // A daemon that exited is never going to answer, and it has
        // already written the reason. Waiting out the deadline would
        // replace a real error with a timeout.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(boot_failed(&log, status.code()));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "cyclopsd started but is not answering after {}s. What it has \
                 written so far is in {}.",
                BOOT_WAIT.as_secs(),
                log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The message for a daemon that exited during boot.
///
/// It carries the daemon's own last words. Without them the caller sees
/// "could not start", and the actual cause (a home too long to bind, a
/// config it will not read) stays in a file nobody thought to open.
fn boot_failed(log: &Path, code: Option<i32>) -> String {
    let mut why = match last_error_line(log) {
        Some(line) => format!("cyclopsd could not start: {line}"),
        None => match code {
            Some(c) => format!("cyclopsd exited with status {c} during boot"),
            None => "cyclopsd was killed during boot".to_string(),
        },
    };
    why.push_str(&format!("\nIts whole log is {}.", log.display()));
    why
}

/// The last line of the log that looks like a failure, with the tracing
/// prefix stripped.
///
/// Reads the tail rather than the file: a log that has been running for a
/// week should not be loaded to report one line.
fn last_error_line(log: &Path) -> Option<String> {
    const TAIL: u64 = 8192;
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    if len > TAIL {
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(len - TAIL)).ok()?;
    }
    let mut text = String::new();
    f.read_to_string(&mut text).ok()?;
    text.lines()
        .rev()
        .find(|l| l.contains("ERROR") || l.contains("boot failed"))
        .map(|l| {
            // `<timestamp> ERROR cyclopsd: boot failed: ...` down to the
            // part a person needs.
            match l.split_once("boot failed: ") {
                Some((_, rest)) => rest.trim().to_string(),
                None => match l.split_once("ERROR ") {
                    Some((_, rest)) => rest.trim().to_string(),
                    None => l.trim().to_string(),
                },
            }
        })
}

/// `cyclopsd` next to this binary, else whatever is on PATH.
///
/// Beside-first matters: an operator with an old copy on PATH and a fresh
/// pair in ~/.local/bin should get the fresh one, because the CLI and the
/// daemon speak a versioned protocol to each other.
fn binary() -> Option<PathBuf> {
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let beside = dir.join("cyclopsd");
            if beside.is_file() {
                return Some(beside);
            }
        }
    }
    which("cyclopsd")
}

/// First match for a bare name on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// How long a restart waits for the stopped daemon to actually leave the
/// socket before giving up rather than racing a second one up.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// The quiesce bound a restart asks for: enough to cover a delivery's
/// whole post-submit ACK window (5s) with margin, so a restart attempted
/// mid-delivery waits it out instead of refusing.
const QUIESCE_ASK_MS: u64 = 8_000;

/// Restart the daemon on the binaries installed now, losing nothing:
/// quiesce, stop, wait it out, start.
///
/// Refuses rather than interrupts. A delivery between the paste and a
/// resolved state is the one thing a restart could orphan, so the daemon
/// is asked to hold the pipeline and wait those windows out first
/// (`daemon.quiesce`); a fleet that stays mid-flight past the bound gets
/// an error naming what is still moving, and nothing is stopped.
/// Deliveries that have not reached a pane never block this: the next
/// boot requeues them.
pub fn restart(home: &Path) -> Result<u32, String> {
    // The quiesce lawfully blocks for its whole bound (deliveries past
    // the paste can take the full 5s ACK window to resolve), so this one
    // request gets a read deadline with headroom over the bound it asks
    // for, instead of the default that would race it.
    let mut client = match Client::connect_with_timeouts(
        Duration::from_secs(2),
        Duration::from_millis(QUIESCE_ASK_MS + 5_000),
    ) {
        Ok(c) => c,
        Err(ClientError::NotRunning) => return Err("cyclopsd is not running.".to_string()),
        Err(e) => return Err(crate::copy::client_error(&e, None)),
    };
    let quiesced = client
        .request(
            "daemon.quiesce",
            serde_json::json!({ "timeout_ms": QUIESCE_ASK_MS }),
        )
        .map_err(|e| crate::copy::client_error(&e, None))?;
    if !quiesced
        .get("quiet")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let open: Vec<String> = quiesced
            .get("in_flight")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let named = if open.is_empty() {
            "a delivery is mid-flight".to_string()
        } else {
            format!("mid-flight: {}", open.join(", "))
        };
        return Err(format!(
            "{named}. Nothing was restarted; try again when it resolves."
        ));
    }
    drop(client);
    let pid = stop()?;
    // Wait for the old daemon to leave the socket: a new one refuses to
    // boot while another still answers there.
    let deadline = Instant::now() + STOP_WAIT;
    while is_up() {
        if Instant::now() >= deadline {
            return Err(format!(
                "cyclopsd (pid {pid}) is still answering {}s after the stop; \
                 not starting a second one.",
                STOP_WAIT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    ensure_running(home)?;
    Ok(pid)
}

/// Ask the running daemon to shut down, by signalling the process that
/// holds the socket.
///
/// The pid comes from the daemon itself rather than from a pid file: a
/// file can outlive the process that wrote it, and then a stop would
/// signal whatever inherited the number.
pub fn stop() -> Result<u32, String> {
    let mut client = match Client::connect() {
        Ok(c) => c,
        Err(ClientError::NotRunning) => return Err("cyclopsd is not running.".to_string()),
        Err(e) => return Err(crate::copy::client_error(&e, None)),
    };
    let status = client
        .request("status", serde_json::json!({}))
        .map_err(|e| crate::copy::client_error(&e, None))?;
    let pid = status
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            "this cyclopsd does not report its pid, so it cannot be stopped this \
             way. Find it with: ps ax | grep cyclopsd"
                .to_string()
        })? as u32;

    // SIGTERM: the daemon removes its socket and exits cleanly on it.
    #[cfg(unix)]
    {
        let out = Command::new("kill")
            .arg(pid.to_string())
            .output()
            .map_err(|e| format!("kill {pid}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "could not stop cyclopsd (pid {pid}): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(pid)
}
