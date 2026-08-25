//! Starting, stopping and reporting on cyclopsd, so a person does not
//! have to hold a terminal open for it.
//!
//! ## Why this exists
//!
//! `cyclopsd &` works and it is what the docs used to say. It also means
//! the daemon dies with the shell that started it, so the first run needs
//! two commands and a spare tab, and the order of those two commands
//! decides whether anything gets named. Making `cyclops start` own
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

use std::io::Read;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::client::{Client, ClientError};
use cyclops_proto::{DaemonShutdownResult, ProcessInstanceId, StatusResult};
use cyclops_state::StateRoot;

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

    ensure_running_from(home, &exe)
}

/// Start the daemon from an already validated active pair.
pub fn ensure_running_from(home: &Path, exe: &Path) -> Result<Started, String> {
    start_and_prove_from(home, exe, crate::BUILD_REF)
}

/// Start one exact daemon and retain ownership until its executable and build
/// match the selected pair. A failed proof drops the guard, which kills and
/// reaps only the process this call spawned.
pub(crate) fn start_and_prove_from(
    home: &Path,
    exe: &Path,
    build: &str,
) -> Result<Started, String> {
    if is_up() {
        return Ok(Started::AlreadyRunning);
    }
    let mut child = spawn_daemon(home, exe)?;
    if let Err(error) = prove_running_pair_generation(exe, build, child.process()) {
        return Err(format!(
            "spawned daemon failed selected-pair proof and was stopped: {error}"
        ));
    }
    child.disarm();
    Ok(Started::Spawned)
}

fn spawn_daemon(home: &Path, exe: &Path) -> Result<SpawnedDaemon, String> {
    if !exe.is_file() {
        return Err(format!("cyclopsd is missing at {}", exe.display()));
    }

    // 3. The log is opened before the spawn so a failure to open it is
    //    this function's error rather than a daemon that starts with
    //    nowhere to write.
    let log = log_path(home);
    let (out, errs) = open_log_files(home)?;

    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null()).stdout(out).stderr(errs);
    // Its own process group, so a Ctrl-C meant for the shell that ran
    // `cyclops start` does not also kill the daemon it just started.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("start {}: {e}", exe.display()))?;
    wait_for_spawned_daemon(child, &log, BOOT_WAIT, || match Client::connect() {
        Ok(client) => Ok(BootSocket::Answering(client.hello().daemon_process)),
        Err(ClientError::NotRunning | ClientError::ConnectTimeout(_)) => Ok(BootSocket::Absent),
        Err(error) => Err(crate::copy::client_error(&error, None)),
    })
}

#[derive(Clone, Copy)]
enum BootSocket {
    Absent,
    Answering(Option<ProcessInstanceId>),
}

/// Own the spawned process until its exact generation answers the socket.
fn wait_for_spawned_daemon<F>(
    child: std::process::Child,
    log: &Path,
    timeout: Duration,
    mut observe_socket: F,
) -> Result<SpawnedDaemon, String>
where
    F: FnMut() -> Result<BootSocket, String>,
{
    let expected_pid = child.id() as i32;
    let mut child = SpawnedDaemon::new(child);
    let deadline = Instant::now() + timeout;
    loop {
        match observe_socket()? {
            BootSocket::Absent => {}
            BootSocket::Answering(Some(process)) if process.pid() == expected_pid => {
                if observe_process(expected_pid) != Some(process) {
                    return Err(format!(
                        "spawned cyclopsd pid {expected_pid} changed generation before readiness"
                    ));
                }
                child.mark_ready(process);
                return Ok(child);
            }
            BootSocket::Answering(Some(process)) => {
                return Err(format!(
                    "another cyclopsd generation answered while pid {expected_pid} was starting: pid {}",
                    process.pid()
                ));
            }
            BootSocket::Answering(None) => {
                return Err(
                    "the answering cyclopsd does not report an exact process generation"
                        .to_string(),
                );
            }
        }
        match child.child_mut().try_wait() {
            Ok(Some(status)) => return Err(boot_failed(log, status.code())),
            Ok(None) => {}
            Err(error) => return Err(format!("inspect starting cyclopsd: {error}")),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "cyclopsd started but is not answering after {}s. What it has \
                 written so far is in {}.",
                timeout.as_secs_f64(),
                log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Kill and reap only the child this process spawned on every failed boot.
#[derive(Debug)]
struct SpawnedDaemon {
    child: Option<std::process::Child>,
    process: Option<ProcessInstanceId>,
}

impl SpawnedDaemon {
    fn new(child: std::process::Child) -> Self {
        Self {
            child: Some(child),
            process: None,
        }
    }

    fn child_mut(&mut self) -> &mut std::process::Child {
        self.child.as_mut().expect("spawned daemon guard is armed")
    }

    fn mark_ready(&mut self, process: ProcessInstanceId) {
        self.process = Some(process);
    }

    fn process(&self) -> ProcessInstanceId {
        self.process
            .expect("a ready spawned daemon has one process generation")
    }

    fn disarm(&mut self) {
        self.child.take();
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

/// Open both daemon output handles through the validated state root.
fn open_log_files(home: &Path) -> Result<(std::fs::File, std::fs::File), String> {
    let state_root = StateRoot::open_or_create(home)
        .map_err(|e| format!("open state root {}: {e}", home.display()))?;
    let log = log_path(home);
    let out = state_root
        .open_append(Path::new("cyclopsd.log"))
        .map_err(|e| format!("open {}: {e}", log.display()))?;
    let errs = state_root
        .open_append(Path::new("cyclopsd.log"))
        .map_err(|e| format!("open {}: {e}", log.display()))?;
    Ok((out.into_file(), errs.into_file()))
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
            if is_executable_file(&beside) {
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
        .find(|p| is_executable_file(p))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    metadata.is_file() && unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0
}

/// How long a restart waits for the stopped daemon to actually leave the
/// socket before giving up rather than racing a second one up.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// The quiesce bound a restart asks for: enough to cover a delivery's
/// whole post-submit ACK window (5s) with margin, so a restart attempted
/// mid-delivery waits it out instead of refusing.
const QUIESCE_ASK_MS: u64 = 8_000;

const GENERATION_REQUIRED: &str = "the running cyclopsd does not report an exact process generation. Stop it manually once, then rerun the update with the new pair";

/// One authenticated connection and the exact daemon generation behind it.
struct AuthenticatedDaemon {
    client: Client,
    process: ProcessInstanceId,
    build: Option<String>,
    executable: String,
    boot_id: String,
}

enum AuthenticationError {
    Predates,
    Failed(String),
}

fn authenticate(mut client: Client) -> Result<AuthenticatedDaemon, AuthenticationError> {
    let hello_process = client
        .hello()
        .daemon_process
        .ok_or(AuthenticationError::Predates)?;
    let hello_executable = client
        .hello()
        .daemon_executable
        .clone()
        .ok_or(AuthenticationError::Predates)?;
    let hello_build = client.hello().build.clone();
    let hello_boot = client.hello().boot_id.clone();
    let status: StatusResult =
        serde_json::from_value(client.request("status", serde_json::json!({})).map_err(
            |error| AuthenticationError::Failed(crate::copy::client_error(&error, None)),
        )?)
        .map_err(|error| AuthenticationError::Failed(format!("decode daemon status: {error}")))?;
    if status.daemon_process != Some(hello_process)
        || status.daemon_build != hello_build
        || status.daemon_executable.as_deref() != Some(hello_executable.as_str())
        || status.boot_id != hello_boot
    {
        return Err(AuthenticationError::Failed(
            "cyclopsd changed identity during authentication; nothing was signalled".to_string(),
        ));
    }
    if observe_process(hello_process.pid()) != Some(hello_process) {
        return Err(AuthenticationError::Failed(
            "cyclopsd process generation changed during authentication; nothing was signalled"
                .to_string(),
        ));
    }
    Ok(AuthenticatedDaemon {
        client,
        process: hello_process,
        build: hello_build,
        executable: hello_executable,
        boot_id: hello_boot,
    })
}

fn prove_running_pair_generation(
    executable: &Path,
    build: &str,
    process: ProcessInstanceId,
) -> Result<(), String> {
    prove_running_pair_expected(executable, build, Some(process))
}

fn prove_running_pair_expected(
    executable: &Path,
    build: &str,
    expected_process: Option<ProcessInstanceId>,
) -> Result<(), String> {
    let expected = std::fs::canonicalize(executable)
        .map_err(|error| format!("resolve selected daemon {}: {error}", executable.display()))?;
    let expected = expected
        .into_os_string()
        .into_string()
        .map_err(|_| "selected daemon path is not UTF-8".to_string())?;
    let client = Client::connect().map_err(|error| crate::copy::client_error(&error, None))?;
    let running = match authenticate(client) {
        Ok(running) => running,
        Err(AuthenticationError::Predates) => return Err(GENERATION_REQUIRED.to_string()),
        Err(AuthenticationError::Failed(error)) => return Err(error),
    };
    if expected_process.is_some_and(|expected| running.process != expected) {
        return Err(format!(
            "answering daemon process {:?} is not the spawned process {:?}",
            running.process, expected_process
        ));
    }
    if running.executable != expected {
        return Err(format!(
            "running daemon executable {} does not match selected {expected}",
            running.executable
        ));
    }
    if running.build.as_deref() != Some(build) {
        return Err(format!(
            "running daemon build {:?} does not match selected build {build}",
            running.build
        ));
    }
    Ok(())
}

/// Re-read the kernel generation immediately before requesting shutdown.
fn generation_matches(expected: ProcessInstanceId, observed: Option<ProcessInstanceId>) -> bool {
    observed == Some(expected)
}

#[cfg(target_os = "macos")]
fn observe_process(pid: i32) -> Option<ProcessInstanceId> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let birth = info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec;
    ProcessInstanceId::new(pid, birth).ok()
}

#[cfg(target_os = "linux")]
fn observe_process(pid: i32) -> Option<ProcessInstanceId> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_linux_process_stat(pid, &stat)
}

#[cfg(target_os = "linux")]
fn parse_linux_process_stat(pid: i32, stat: &str) -> Option<ProcessInstanceId> {
    let after = stat.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    let state = fields.next()?;
    // A dead child remains in /proc until its parent reaps it. It cannot
    // execute or own the authenticated socket, so it has left for stop proof.
    if matches!(state, "Z" | "X" | "x") {
        return None;
    }
    let birth = fields.nth(18)?.parse().ok()?;
    ProcessInstanceId::new(pid, birth).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn observe_process(_pid: i32) -> Option<ProcessInstanceId> {
    None
}

/// Why a restart did not happen. The caller words each case; only
/// [`RestartRefusal::Predates`] has a different fix from the others.
pub enum RestartRefusal {
    /// The running daemon does not report exact identity or cannot stop itself
    /// through the authenticated connection. The one-time migration is a stop
    /// with the old CLI followed by this command again.
    Predates,
    /// Something is between the paste and a resolved delivery. Carries the
    /// sentence naming it.
    Busy(String),
    /// Anything else: no daemon, a broken socket, a failed spawn.
    Failed(String),
}

impl RestartRefusal {
    pub fn why(&self) -> &str {
        match self {
            RestartRefusal::Predates => {
                "the running daemon predates this feature; it is the build you just replaced"
            }
            RestartRefusal::Busy(why) | RestartRefusal::Failed(why) => why,
        }
    }
}

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
pub fn restart(home: &Path) -> Result<u32, RestartRefusal> {
    // Resolve the intended daemon before stopping the authenticated one. A
    // process that races onto the socket after the stop is not a successful
    // restart unless this call spawned it and proves its executable and build.
    let executable = binary().ok_or_else(|| {
        RestartRefusal::Failed(
            "cyclopsd is not next to cyclops and not on your PATH. Reinstall with \
             ./scripts/install.sh, which puts both in the same directory."
                .to_string(),
        )
    })?;
    let pid = stop_selected_for_pair_change(&executable)?
        .ok_or_else(|| RestartRefusal::Failed("cyclopsd is not running.".to_string()))?;
    let started = start_and_prove_from(home, &executable, crate::BUILD_REF).map_err(|error| {
        RestartRefusal::Failed(format!(
            "original daemon pid {pid} is stopped; restart candidate did not remain active: {error}"
        ))
    })?;
    match started {
        Started::Spawned => {}
        Started::AlreadyRunning => {
            return Err(RestartRefusal::Failed(
                format!(
                    "original daemon pid {pid} is stopped; another daemon answered before the selected daemon started and was left untouched"
                ),
            ));
        }
    }
    Ok(pid)
}

/// Stop only when the authenticated daemon reports this selected executable.
pub fn stop_selected_for_pair_change(executable: &Path) -> Result<Option<u32>, RestartRefusal> {
    let executable = std::fs::canonicalize(executable).map_err(|error| {
        RestartRefusal::Failed(format!(
            "resolve selected daemon {}: {error}",
            executable.display()
        ))
    })?;
    let executable = executable
        .into_os_string()
        .into_string()
        .map_err(|_| RestartRefusal::Failed("selected daemon path is not UTF-8".to_string()))?;
    stop_for_pair_change_expected(Some(&executable))
}

fn stop_for_pair_change_expected(
    expected_executable: Option<&str>,
) -> Result<Option<u32>, RestartRefusal> {
    // Shutdown lawfully blocks for its whole quiesce bound. This connection
    // gets headroom over that bound instead of racing the daemon's answer.
    let client = match Client::connect_with_timeouts(
        Duration::from_secs(2),
        Duration::from_millis(QUIESCE_ASK_MS + 5_000),
    ) {
        Ok(client) => client,
        Err(ClientError::NotRunning) => return Ok(None),
        Err(error) => {
            return Err(RestartRefusal::Failed(crate::copy::client_error(
                &error, None,
            )))
        }
    };
    let mut running = match authenticate(client) {
        Ok(running) => running,
        Err(AuthenticationError::Predates) => return Err(RestartRefusal::Predates),
        Err(AuthenticationError::Failed(error)) => return Err(RestartRefusal::Failed(error)),
    };
    if expected_executable.is_some_and(|expected| running.executable != expected) {
        return Err(RestartRefusal::Failed(format!(
            "running daemon executable {} does not match selected {}; nothing was stopped",
            running.executable,
            expected_executable.expect("checked as some")
        )));
    }
    if !generation_matches(running.process, observe_process(running.process.pid())) {
        return Err(RestartRefusal::Failed(
            "cyclopsd process generation changed before shutdown; nothing was requested"
                .to_string(),
        ));
    }
    let stopped = match running.client.request(
        "daemon.shutdown",
        serde_json::json!({
            "daemon_process": running.process,
            "boot_id": running.boot_id.clone(),
            "timeout_ms": QUIESCE_ASK_MS,
        }),
    ) {
        Ok(v) => v,
        // A daemon that does not know the verb cannot stop its authenticated
        // generation without returning to the old client once.
        Err(ClientError::Server { ref code, .. }) if code == "unknown_method" => {
            return Err(RestartRefusal::Predates)
        }
        Err(e) => return Err(RestartRefusal::Failed(crate::copy::client_error(&e, None))),
    };
    let stopped: DaemonShutdownResult = serde_json::from_value(stopped).map_err(|error| {
        RestartRefusal::Failed(format!("decode daemon shutdown result: {error}"))
    })?;
    if !stopped.stopping {
        let open = stopped.in_flight;
        let named = if open.is_empty() {
            "a delivery is mid-flight".to_string()
        } else {
            format!("mid-flight: {}", open.join(", "))
        };
        return Err(RestartRefusal::Busy(format!(
            "{named}. Nothing was restarted; try again when it resolves."
        )));
    }
    let process = running.process;
    let pid = process.pid() as u32;
    let boot_id = running.boot_id.clone();
    drop(running);
    wait_for_authenticated_exit(process, &boot_id).map_err(RestartRefusal::Failed)?;
    Ok(Some(pid))
}

/// Ask the exact daemon on this authenticated connection to shut itself down.
///
/// The process generation and boot identity come from the daemon itself rather
/// than a pid file. The daemon answers before it triggers its internal shutdown.
pub fn stop() -> Result<u32, String> {
    let mut running = match Client::connect_with_timeouts(
        Duration::from_secs(2),
        Duration::from_millis(QUIESCE_ASK_MS + 5_000),
    ) {
        Ok(client) => match authenticate(client) {
            Ok(running) => running,
            Err(AuthenticationError::Predates) => return Err(GENERATION_REQUIRED.to_string()),
            Err(AuthenticationError::Failed(error)) => return Err(error),
        },
        Err(ClientError::NotRunning) => return Err("cyclopsd is not running.".to_string()),
        Err(e) => return Err(crate::copy::client_error(&e, None)),
    };
    if !generation_matches(running.process, observe_process(running.process.pid())) {
        return Err(
            "cyclopsd process generation changed before shutdown; nothing was requested"
                .to_string(),
        );
    }
    let result = running
        .client
        .request(
            "daemon.shutdown",
            serde_json::json!({
                "daemon_process": running.process,
                "boot_id": running.boot_id.clone(),
                "timeout_ms": QUIESCE_ASK_MS,
            }),
        )
        .map_err(|error| crate::copy::client_error(&error, None))?;
    let result: DaemonShutdownResult = serde_json::from_value(result)
        .map_err(|error| format!("decode daemon shutdown result: {error}"))?;
    if !result.stopping {
        let named = if result.in_flight.is_empty() {
            "a delivery is mid-flight".to_string()
        } else {
            format!("mid-flight: {}", result.in_flight.join(", "))
        };
        return Err(format!("{named}. Nothing was stopped."));
    }
    let process = running.process;
    let pid = process.pid() as u32;
    let boot_id = running.boot_id.clone();
    drop(running);
    wait_for_authenticated_exit(process, &boot_id)?;
    Ok(pid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitObservation {
    Gone,
    Waiting,
    Replaced,
}

fn classify_exit(
    expected_boot: &str,
    process_gone: bool,
    socket_boot: Option<&str>,
) -> ExitObservation {
    match socket_boot {
        Some(observed) if observed != expected_boot => ExitObservation::Replaced,
        None if process_gone => ExitObservation::Gone,
        _ => ExitObservation::Waiting,
    }
}

/// Prove that the authenticated socket instance left before a caller may
/// activate or start another pair.
fn wait_for_authenticated_exit(process: ProcessInstanceId, boot_id: &str) -> Result<(), String> {
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        let process_gone = observe_process(process.pid()) != Some(process);
        let connected =
            Client::connect_with_timeouts(Duration::from_millis(100), Duration::from_secs(1));
        let observed_boot = connected
            .as_ref()
            .ok()
            .map(|client| client.hello().boot_id.as_str());
        match classify_exit(boot_id, process_gone, observed_boot) {
            ExitObservation::Gone => return Ok(()),
            ExitObservation::Replaced => {
                return Err(format!(
                    "another cyclopsd boot replaced {boot_id} while pid {} was stopping; refusing to continue",
                    process.pid()
                ))
            }
            ExitObservation::Waiting => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "cyclopsd boot {boot_id} (pid {}) did not leave within {}s",
                process.pid(),
                STOP_WAIT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn sleeping_child() -> std::process::Child {
        Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn assert_process_gone(pid: i32) {
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "spawned daemon {pid} survived"
        );
    }

    #[test]
    fn daemon_log_handles_are_owner_only_and_append() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-daemon-log-state");
        let _ = std::fs::remove_dir_all(&home);
        let (mut out, mut errs) = open_log_files(&home).unwrap();

        out.write_all(b"out\n").unwrap();
        errs.write_all(b"err\n").unwrap();
        drop((out, errs));

        let log = log_path(&home);
        assert_eq!(std::fs::read(&log).unwrap(), b"out\nerr\n");
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_reused_pid_is_not_the_authenticated_daemon_generation() {
        let expected = ProcessInstanceId::new(4242, 100).unwrap();
        let reused = ProcessInstanceId::new(4242, 101).unwrap();

        assert!(generation_matches(expected, Some(expected)));
        assert!(!generation_matches(expected, Some(reused)));
        assert!(!generation_matches(expected, None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_zombie_has_left_its_process_generation() {
        let fields_before_birth = ["0"; 18].join(" ");
        let live = format!("4242 (cyclopsd worker) S {fields_before_birth} 9001");
        let zombie = format!("4242 (cyclopsd worker) Z {fields_before_birth} 9001");

        assert_eq!(
            parse_linux_process_stat(4242, &live),
            ProcessInstanceId::new(4242, 9001).ok()
        );
        assert_eq!(parse_linux_process_stat(4242, &zombie), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_unreaped_linux_child_is_already_gone_for_stop_proof() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn immediate-exit child");
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(2);
        let observed_zombie = loop {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')?
                        .1
                        .split_whitespace()
                        .next()
                        .map(str::to_string)
                });
            if state.as_deref() == Some("Z") {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let observed_generation = observe_process(pid);
        let status = child.wait().expect("reap immediate-exit child");

        assert!(observed_zombie, "child never entered the zombie state");
        assert_eq!(observed_generation, None);
        assert!(status.success());
    }

    #[test]
    fn a_replacement_boot_never_completes_the_authenticated_stop() {
        assert_eq!(
            classify_exit("boot-a", true, Some("boot-b")),
            ExitObservation::Replaced
        );
        assert_eq!(
            classify_exit("boot-a", false, Some("boot-a")),
            ExitObservation::Waiting
        );
        assert_eq!(classify_exit("boot-a", true, None), ExitObservation::Gone);
    }

    #[test]
    fn a_never_ready_spawn_is_killed_and_reaped_at_the_boot_bound() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-never-ready-daemon");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let child = sleeping_child();
        let pid = child.id() as i32;

        let error =
            wait_for_spawned_daemon(child, &home.join("log"), Duration::from_millis(60), || {
                Ok(BootSocket::Absent)
            })
            .unwrap_err();

        assert!(error.contains("not answering"), "{error}");
        assert_process_gone(pid);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn another_answering_generation_does_not_orphan_the_spawned_child() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-other-daemon-answers");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let child = sleeping_child();
        let pid = child.id() as i32;
        let other = ProcessInstanceId::new(pid + 1, 7).unwrap();

        let error =
            wait_for_spawned_daemon(child, &home.join("log"), Duration::from_secs(1), || {
                Ok(BootSocket::Answering(Some(other)))
            })
            .unwrap_err();

        assert!(error.contains("another cyclopsd generation"), "{error}");
        assert_process_gone(pid);
        let _ = std::fs::remove_dir_all(&home);
    }
}
