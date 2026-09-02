//! Opt-in transport benchmark for one frozen release candidate.
//!
//! This is evidence collection, not an ordinary regression test. It uses only
//! private tmux servers and scratch homes, never the user's daemon or tmux
//! server, and emits one content-free JSON line for the release record. Build
//! both candidate binaries from the frozen checkout, then run this test from
//! that same clean checkout with `cargo test --release --test release_transport_benchmark -- --ignored`,
//! `CYC_RELEASE_SHA`, `CYC_RELEASE_CYCLOPS`, and `CYC_RELEASE_CYCLOPSD` set.
//! The live-vendor campaign owns agent response time because a fixture process
//! cannot measure a model.

mod common;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{
    manual_lifecycle_composer_pane, HomeGuard, Rig, TmuxGuard, BUSY_MANIFEST, CAT_MANIFEST,
};
use cyclops_proto::{DeliveryState, MessageNotificationState};
use serde_json::{json, Value};

const CHILD_OUTPUT_CAP: usize = 1024 * 1024;

#[derive(Clone, Copy)]
enum CapturedOutputFailure {
    TimedOut,
    WaitFailed,
    ExitStatus(Option<i32>),
    UnexpectedStderr(usize),
    InvalidJson,
}

fn captured_output_failure(label: &str, failure: CapturedOutputFailure) -> String {
    let reason = match failure {
        CapturedOutputFailure::TimedOut => "exceeded 15 seconds".to_string(),
        CapturedOutputFailure::WaitFailed => "could not observe child status".to_string(),
        CapturedOutputFailure::ExitStatus(code) => {
            format!("failed with status {code:?}")
        }
        CapturedOutputFailure::UnexpectedStderr(bytes) => {
            format!("polluted JSON stderr with {bytes} captured bytes")
        }
        CapturedOutputFailure::InvalidJson => "emitted invalid JSON".to_string(),
    };
    format!("{label} {reason}; captured output withheld")
}

fn samples() -> usize {
    std::env::var("CYC_RELEASE_SAMPLES")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .filter(|count: &usize| *count >= 10)
        .unwrap_or(30)
}

fn micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

fn distribution(mut values: Vec<u64>) -> Value {
    values.sort_unstable();
    let at = |percentile: usize| {
        let index = (values.len() - 1) * percentile / 100;
        values[index]
    };
    json!({
        "n": values.len(),
        "unit": "us",
        "p50": at(50),
        "p95": at(95),
        "p99": at(99),
        "max": values[values.len() - 1],
    })
}

fn validate_frozen_sha(sha: &str) -> Result<&str, String> {
    if sha.len() != 40 {
        return Err(format!(
            "CYC_RELEASE_SHA must be 40 characters (got {})",
            sha.len()
        ));
    }
    if !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("CYC_RELEASE_SHA must contain only hexadecimal characters".to_string());
    }
    Ok(sha)
}

fn expected_build_ref_prefix(sha: &str) -> Result<&str, String> {
    validate_frozen_sha(sha).map(|valid| &valid[..7])
}

fn extract_version_build_ref(version: &str) -> Option<&str> {
    version
        .strip_suffix(')')
        .and_then(|without_close| without_close.rsplit_once(" (").map(|(_, build)| build))
}

fn extract_package_version<'a>(version: &'a str, program: &str) -> Option<&'a str> {
    version
        .strip_prefix(program)?
        .strip_prefix(' ')?
        .split_once(" (")
        .map(|(package_version, _)| package_version)
}

fn status_daemon_matches_candidate(status: &Value, candidate_version: &str) -> bool {
    let Some(expected) = extract_package_version(candidate_version, "cyclopsd") else {
        return false;
    };
    status.get("daemon_version").and_then(Value::as_str) == Some(expected)
}

fn listed_agent<'a>(list: &'a Value, pane: &str) -> Option<&'a Value> {
    list.get("agents")?
        .as_array()?
        .iter()
        .find(|item| item.get("pane_id").and_then(Value::as_str) == Some(pane))
}

fn message_was_durably_accepted(result: &Value) -> bool {
    let Some(object) = result.as_object() else {
        return false;
    };
    let msg_id = object
        .get("msg_id")
        .and_then(Value::as_str)
        .and_then(|raw| cyclops_proto::MessageId::new(raw).ok());
    let seq = object.get("seq").and_then(Value::as_u64);
    let deliveries = object.get("deliveries").and_then(Value::as_array);
    let inserted_is_valid = matches!(object.get("inserted"), None | Some(Value::Bool(_)));
    msg_id.is_some()
        && seq.is_some_and(|value| value > 0)
        && deliveries.is_some()
        && inserted_is_valid
}

fn validate_frozen_version(version: &str, sha: &str, artifact: &str) -> Result<String, String> {
    let expected_prefix = expected_build_ref_prefix(sha)?;
    let Some(build_ref) = extract_version_build_ref(version) else {
        return Err(format!(
            "{artifact} has an unrecognized --version line: {version:?}"
        ));
    };
    if build_ref != expected_prefix {
        return Err(format!(
            "{artifact} build ref prefix {build_ref:?} does not match expected prefix {expected_prefix:?} from SHA {sha}"
        ));
    }
    Ok(build_ref.to_string())
}

fn validate_release_profile(debug_assertions: bool) -> Result<(), String> {
    if debug_assertions {
        Err("the frozen benchmark must be compiled with `cargo test --release`".to_string())
    } else {
        Ok(())
    }
}

fn validate_frozen_checkout_state(head: &str, status: &str, sha: &str) -> Result<(), String> {
    validate_frozen_sha(sha)?;
    if head.trim() != sha {
        return Err(format!(
            "benchmark checkout HEAD {:?} does not match CYC_RELEASE_SHA {:?}",
            head.trim(),
            sha
        ));
    }
    if !status.trim().is_empty() {
        return Err(format!("benchmark checkout is dirty:\n{status}"));
    }
    Ok(())
}

fn validate_frozen_checkout(sha: &str) -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let head = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| format!("failed to run git rev-parse: {error}"))?;
    if !head.status.success() {
        return Err(format!("git rev-parse failed for {root:?}"));
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain=v1", "--untracked-files=normal"])
        .output()
        .map_err(|error| format!("failed to run git status: {error}"))?;
    if !status.status.success() {
        return Err(format!("git status failed for {root:?}"));
    }
    let head_str = std::str::from_utf8(&head.stdout).map_err(|e| e.to_string())?;
    let status_str = std::str::from_utf8(&status.stdout).map_err(|e| e.to_string())?;
    validate_frozen_checkout_state(head_str, status_str, sha)
}

fn delivery_receipts_prove_wake(result: &Value) -> bool {
    let Some(deliveries) = result.get("deliveries").and_then(Value::as_array) else {
        return false;
    };
    !deliveries.is_empty()
        && deliveries.iter().all(|receipt| {
            let Ok(state) = serde_json::from_value::<DeliveryState>(receipt["state"].clone())
            else {
                return false;
            };
            if receipt
                .get("pre_write_cause")
                .is_some_and(|value| !value.is_null())
                || receipt
                    .get("wake_block")
                    .is_some_and(|value| !value.is_null())
                || cyclops_proto::delivery_needs_human(state)
            {
                return false;
            }
            match receipt.get("notification_state").and_then(Value::as_str) {
                Some("submitted" | "notified" | "submitted_unverified") => true,
                Some(_) => false,
                None => matches!(
                    state,
                    DeliveryState::Submitted
                        | DeliveryState::DeliveredVerified
                        | DeliveryState::DeliveredUnverified
                ),
            }
        })
}

fn delivery_receipt_summary(result: &Value) -> Vec<String> {
    result
        .get("deliveries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|receipt| {
            let state = receipt
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("missing");
            let notification = receipt
                .get("notification_state")
                .and_then(Value::as_str)
                .unwrap_or("none");
            let pre_write_blocked = receipt
                .get("pre_write_cause")
                .is_some_and(|value| !value.is_null());
            let wake_blocked = receipt
                .get("wake_block")
                .is_some_and(|value| !value.is_null());
            format!(
                "state={state},notification={notification},pre_write_blocked={pre_write_blocked},wake_blocked={wake_blocked}"
            )
        })
        .collect()
}

#[derive(Debug)]
struct CandidateBinary {
    path: PathBuf,
    version: String,
    build_ref_prefix: String,
}

#[derive(Debug)]
struct FrozenCandidate {
    sha: String,
    cli: CandidateBinary,
    daemon: CandidateBinary,
}

fn drain_bounded(mut reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(CHILD_OUTPUT_CAP.min(8192));
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(retained);
        }
        let remaining = CHILD_OUTPUT_CAP.saturating_sub(retained.len());
        retained.extend_from_slice(&chunk[..read.min(remaining)]);
    }
}

fn bounded_output(command: &mut Command, label: &str) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {label}: {error}"));

    let mut stdout_pipe = child.stdout.take().expect("child stdout");
    let mut stderr_pipe = child.stderr.take().expect("child stderr");

    let stdout_handle = thread::spawn(move || drain_bounded(&mut stdout_pipe));
    let stderr_handle = thread::spawn(move || drain_bounded(&mut stderr_pipe));

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _status = child.wait().expect("collect killed child status");
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                panic!(
                    "{}",
                    captured_output_failure(label, CapturedOutputFailure::TimedOut)
                );
            }
            Err(_error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_handle.join();
                let _ = stderr_handle.join();
                panic!(
                    "{}",
                    captured_output_failure(label, CapturedOutputFailure::WaitFailed)
                );
            }
        }
    };
    let stdout = stdout_handle
        .join()
        .expect("join stdout thread")
        .expect("read child stdout");
    let stderr = stderr_handle
        .join()
        .expect("join stderr thread")
        .expect("read child stderr");
    Output {
        status,
        stdout,
        stderr,
    }
}

fn try_candidate_binary(
    env_name: &str,
    program: &str,
    sha: &str,
) -> Result<CandidateBinary, String> {
    let path_str = std::env::var(env_name)
        .map_err(|_| format!("set {env_name} to the frozen candidate {program} binary"))?;
    let path = PathBuf::from(&path_str);
    if !path.is_file() {
        return Err(format!(
            "candidate {program} binary does not exist at {path:?}"
        ));
    }
    let output = bounded_output(
        Command::new(&path).arg("--version"),
        &format!("probe candidate {program}"),
    );
    if !output.status.success() {
        return Err(captured_output_failure(
            &format!("candidate {program} --version"),
            CapturedOutputFailure::ExitStatus(output.status.code()),
        ));
    }
    let version = String::from_utf8(output.stdout)
        .map_err(|e| format!("candidate version is not UTF-8: {e}"))?
        .trim()
        .to_string();
    if !version.starts_with(&format!("{program} ")) {
        return Err(format!(
            "candidate {program} returned wrong identity: {version:?}"
        ));
    }
    let build_ref_prefix = validate_frozen_version(&version, sha, &format!("candidate {program}"))?;
    Ok(CandidateBinary {
        path,
        version,
        build_ref_prefix,
    })
}

fn try_frozen_candidate() -> Result<FrozenCandidate, String> {
    validate_release_profile(cfg!(debug_assertions))?;
    let sha = std::env::var("CYC_RELEASE_SHA")
        .map_err(|_| "set CYC_RELEASE_SHA to the frozen 40-character commit".to_string())?;
    validate_frozen_sha(&sha)?;
    let expected_prefix = expected_build_ref_prefix(&sha)?;
    let test_build_ref = cyclops_proto::BUILD_REF;
    if test_build_ref != expected_prefix {
        return Err(format!(
            "benchmark test binary build ref {test_build_ref:?} does not match expected prefix {expected_prefix:?} for SHA {sha}"
        ));
    }
    validate_frozen_checkout(&sha)?;
    let cli = try_candidate_binary("CYC_RELEASE_CYCLOPS", "cyclops", &sha)?;
    let daemon = try_candidate_binary("CYC_RELEASE_CYCLOPSD", "cyclopsd", &sha)?;
    Ok(FrozenCandidate { sha, cli, daemon })
}

struct CandidateDaemon {
    child: Option<Child>,
}

impl CandidateDaemon {
    fn spawn(binary: &Path, home: &Path) -> Self {
        let child = Command::new(binary)
            .env("CYCLOPS_HOME", home)
            .env_remove("CYCLOPS_AGENT")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("CARGO_TARGET_DIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn frozen candidate daemon");
        Self { child: Some(child) }
    }

    fn wait_ready(&mut self, socket: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.child
                    .as_mut()
                    .expect("candidate daemon child")
                    .try_wait()
                    .expect("poll candidate daemon")
                    .is_none(),
                "candidate daemon exited before creating its private socket"
            );
            if socket.exists() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "candidate daemon did not create {socket:?} within 15 seconds"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_clean_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = self
                .child
                .as_mut()
                .expect("candidate daemon child")
                .try_wait()
                .expect("poll stopped candidate daemon");
            if let Some(status) = status {
                assert!(status.success(), "candidate daemon exited with {status}");
                self.child = None;
                return;
            }
            assert!(
                Instant::now() < deadline,
                "candidate daemon did not stop within 15 seconds"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for CandidateDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct ExternalCandidateRig {
    daemon: CandidateDaemon,
    home: PathBuf,
    busy_pane: String,
    worker_pane: String,
    _tmux: TmuxGuard,
    _home_guard: HomeGuard,
}

const RECIPIENT_COMMAND_LOOP: &str = r#"import json
import pathlib
import subprocess
import sys

for line in sys.stdin:
    request = json.loads(line)
    completed = subprocess.run(request["argv"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    pathlib.Path(request["stdout"]).write_bytes(completed.stdout)
    pathlib.Path(request["stderr"]).write_bytes(completed.stderr)
    status = pathlib.Path(request["status"])
    temporary = status.with_suffix(".tmp")
    temporary.write_text(str(completed.returncode), encoding="utf-8")
    temporary.replace(status)
"#;

struct RecipientCommandOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn doorbell_locator_from_capture(capture: &str) -> Option<String> {
    capture.lines().find_map(|line| {
        let start = line.find("cyclops inbox claim ")?;
        let payload = line[start..].trim_end();
        cyclops_proto::parse_doorbell_v3(payload).map(|attempt_id| {
            cyclops_proto::notification_attempt_claim_locator(attempt_id).to_string()
        })
    })
}

fn candidate_doorbell_locator(tmux: &TmuxGuard, pane: &str) -> Option<String> {
    let output = tmux.run(&["capture-pane", "-p", "-S", "-", "-t", pane]);
    if !output.status.success() {
        return None;
    }
    let capture = String::from_utf8(output.stdout).ok()?;
    doorbell_locator_from_capture(&capture)
}

fn wait_for_candidate_doorbell_locator(tmux: &TmuxGuard, pane: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(locator) = candidate_doorbell_locator(tmux, pane) {
            return locator;
        }
        assert!(
            Instant::now() < deadline,
            "candidate pane {pane:?} never rendered an exact Format 3 doorbell"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn pane_for_window(tmux: &TmuxGuard, window_name: &str) -> String {
    let output = tmux.run(&[
        "list-panes",
        "-s",
        "-t",
        "=release-candidate",
        "-F",
        "#{pane_id}\t#{window_name}",
    ]);
    assert!(
        output.status.success(),
        "{}",
        captured_output_failure(
            "list private candidate panes",
            CapturedOutputFailure::ExitStatus(output.status.code())
        )
    );
    String::from_utf8(output.stdout)
        .expect("tmux pane list is UTF-8")
        .lines()
        .find_map(|line| {
            let (pane, window) = line.split_once('\t')?;
            (window == window_name).then(|| pane.to_string())
        })
        .unwrap_or_else(|| panic!("private candidate window {window_name:?} has no pane"))
}

impl ExternalCandidateRig {
    fn start(candidate: &FrozenCandidate) -> Self {
        let tmux = TmuxGuard::new("release-candidate");
        let busy_command = "printf 'BUSY-MARKER\\n'; exec cat";
        let worker_command = manual_lifecycle_composer_pane();
        tmux.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "release-candidate",
            "-n",
            "blocked",
            "-x",
            "160",
            "-y",
            "40",
            busy_command,
        ]);
        tmux.run_ok(&[
            "new-window",
            "-d",
            "-t",
            "=release-candidate",
            "-n",
            "worker",
            &worker_command,
        ]);
        let busy_pane = pane_for_window(&tmux, "blocked");
        let worker_pane = pane_for_window(&tmux, "worker");

        let home = cyclops_proto::scratch::scratch_dir("cyc-release-candidate");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("manifests")).expect("create private candidate home");
        let home_guard = HomeGuard(home.clone());
        let capability = home.join("cyclops-skill.md");
        fs::write(
            &capability,
            include_bytes!("../../../skills/cyclops/SKILL.md"),
        )
        .expect("write private mailbox capability evidence");
        let mailbox_capability = format!(
            "\n[messaging]\nmailbox_capability_file = {:?}\n",
            capability.display().to_string()
        );
        fs::write(
            home.join("manifests/fix.toml"),
            format!("{BUSY_MANIFEST}{mailbox_capability}"),
        )
        .expect("write candidate manifest");
        fs::write(
            home.join("config.toml"),
            format!(
                "sessions = [\"release-candidate\"]\n\
                 tmux_socket = {:?}\n\
                 tmux_config = \"/dev/null\"\n\
                 manifest_dir = {:?}\n\
                 receipt_block_ms = 5000\n\
                 ack_timeout_ms = 1500\n",
                tmux.socket(),
                home.join("manifests").display().to_string(),
            ),
        )
        .expect("write private candidate config");
        fs::write(
            home.join("recipient-command-loop.py"),
            RECIPIENT_COMMAND_LOOP,
        )
        .expect("write recipient command loop");

        let mut daemon = CandidateDaemon::spawn(&candidate.daemon.path, &home);
        daemon.wait_ready(&home.join("sock"));
        Self {
            daemon,
            home,
            busy_pane,
            worker_pane,
            _tmux: tmux,
            _home_guard: home_guard,
        }
    }

    fn replace_worker_with_recipient_command_loop(&self, cli: &CandidateBinary) {
        let command = format!(
            "exec env CYCLOPS_HOME={} NO_COLOR=1 python3 -u {}",
            shell_quote(&self.home.display().to_string()),
            shell_quote(
                &self
                    .home
                    .join("recipient-command-loop.py")
                    .display()
                    .to_string()
            )
        );
        self._tmux
            .run_ok(&["respawn-pane", "-k", "-t", &self.worker_pane, &command]);
        wait_for_candidate_name(cli, &self.home, &self.worker_pane, "worker", "fix");
        let worker = wait_for_candidate_state(cli, &self.home, &self.worker_pane, "idle");
        assert_eq!(
            worker["manifest"], "fix",
            "replacement recipient process did not bind the candidate manifest"
        );
    }

    fn run_cli_as_recipient(
        &self,
        cli: &CandidateBinary,
        tag: &str,
        args: &[&str],
    ) -> RecipientCommandOutput {
        let stdout = self.home.join(format!("recipient-{tag}.stdout"));
        let stderr = self.home.join(format!("recipient-{tag}.stderr"));
        let status = self.home.join(format!("recipient-{tag}.status"));
        for path in [&stdout, &stderr, &status] {
            let _ = fs::remove_file(path);
        }
        let mut argv = vec![cli.path.display().to_string()];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        let request = json!({
            "argv": argv,
            "stdout": stdout,
            "stderr": stderr,
            "status": status,
        })
        .to_string();
        self._tmux
            .run_ok(&["send-keys", "-t", &self.worker_pane, "-l", &request]);
        self._tmux
            .run_ok(&["send-keys", "-t", &self.worker_pane, "Enter"]);

        let deadline = Instant::now() + Duration::from_secs(15);
        let code = loop {
            if let Ok(raw) = fs::read_to_string(&status) {
                if let Ok(code) = raw.parse::<i32>() {
                    break code;
                }
            }
            assert!(
                Instant::now() < deadline,
                "recipient candidate command {tag:?} did not finish"
            );
            thread::sleep(Duration::from_millis(20));
        };
        RecipientCommandOutput {
            code,
            stdout: fs::read(stdout).expect("read recipient candidate stdout"),
            stderr: fs::read(stderr).expect("read recipient candidate stderr"),
        }
    }

    fn shutdown(&mut self, cli: &CandidateBinary) {
        let stopped = run_candidate_cli(cli, &self.home, &["daemon", "stop", "--json"]);
        assert!(
            stopped.status.success(),
            "{}",
            captured_output_failure(
                "candidate daemon stop",
                CapturedOutputFailure::ExitStatus(stopped.status.code())
            )
        );
        self.daemon.wait_for_clean_exit();
    }
}

fn run_candidate_cli(binary: &CandidateBinary, home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(&binary.path);
    command
        .env("CYCLOPS_HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("CYCLOPS_AGENT")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("CARGO_TARGET_DIR")
        .args(args);
    bounded_output(&mut command, "candidate CLI")
}

fn json_output(output: &Output, label: &str) -> Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        captured_output_failure(
            label,
            CapturedOutputFailure::UnexpectedStderr(output.stderr.len())
        )
    );
    assert!(
        output.status.success(),
        "{label} returned non-zero exit status"
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_error| {
        panic!(
            "{}",
            captured_output_failure(label, CapturedOutputFailure::InvalidJson)
        )
    })
}

fn json_output_allowing_nonzero(output: &Output, label: &str) -> Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        captured_output_failure(
            label,
            CapturedOutputFailure::UnexpectedStderr(output.stderr.len())
        )
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_error| {
        panic!(
            "{}",
            captured_output_failure(label, CapturedOutputFailure::InvalidJson)
        )
    })
}

fn wait_for_candidate_state(cli: &CandidateBinary, home: &Path, pane: &str, wanted: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last_state = None;
    loop {
        let output = run_candidate_cli(cli, home, &["list", "--json"]);
        let list = json_output(&output, "cyclops list --json");
        if let Some(item) = listed_agent(&list, pane) {
            if item["state"] == wanted {
                return item.clone();
            }
            last_state = item["state"].as_str().map(str::to_owned);
        }
        assert!(
            Instant::now() < deadline,
            "candidate pane {pane:?} never reached state {wanted:?}; last state was {last_state:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_candidate_name(
    cli: &CandidateBinary,
    home: &Path,
    pane: &str,
    label: &str,
    manifest: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = run_candidate_cli(
            cli,
            home,
            &["name", pane, label, "--manifest", manifest, "--json"],
        );
        if output.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "candidate pane {pane:?} was never available for naming"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn run_external_candidate_contract(candidate: &FrozenCandidate) -> Value {
    let mut rig = ExternalCandidateRig::start(candidate);

    let status_output = run_candidate_cli(&candidate.cli, &rig.home, &["status", "--json"]);
    let status = json_output(&status_output, "cyclops status --json");
    assert!(
        status_daemon_matches_candidate(&status, &candidate.daemon.version),
        "running candidate daemon version mismatch"
    );

    wait_for_candidate_name(&candidate.cli, &rig.home, &rig.busy_pane, "blocked", "fix");
    wait_for_candidate_name(&candidate.cli, &rig.home, &rig.worker_pane, "worker", "fix");

    let initial_worker =
        wait_for_candidate_state(&candidate.cli, &rig.home, &rig.worker_pane, "idle");
    assert_eq!(
        initial_worker["manifest"], "fix",
        "candidate daemon failed to load worker manifest"
    );

    wait_for_candidate_state(&candidate.cli, &rig.home, &rig.busy_pane, "working");

    let default_send_output = run_candidate_cli(
        &candidate.cli,
        &rig.home,
        &[
            "send",
            "blocked",
            "--body",
            "default unproven wake accepts into mailbox",
            "--subject",
            "contract-default",
            "--summary",
            "The blocked wake must remain durable. Inspect the receipt without assuming delivery.",
            "--json",
        ],
    );
    let default_send = json_output(&default_send_output, "cyclops send --json");
    assert!(
        message_was_durably_accepted(&default_send),
        "default send must accept into mailbox even when wake is unproven"
    );
    assert!(
        !delivery_receipts_prove_wake(&default_send),
        "default send on a busy pane must not claim proven wake"
    );

    let blocked_send = run_candidate_cli(
        &candidate.cli,
        &rig.home,
        &[
            "send",
            "blocked",
            "--body",
            "require-wake on a busy pane must fail closed",
            "--subject",
            "contract-blocked",
            "--summary",
            "The guarded wake must fail closed. Confirm that mailbox acceptance remains durable.",
            "--require-wake",
            "--json",
        ],
    );
    assert_eq!(
        blocked_send.status.code(),
        Some(1),
        "send --require-wake on a blocked pane must exit with code 1"
    );
    let blocked_json: Value = serde_json::from_slice(&blocked_send.stdout)
        .expect("parse send --require-wake failure json");
    assert!(
        message_was_durably_accepted(&blocked_json),
        "require-wake rejection must still record durable mailbox acceptance"
    );
    assert!(
        !delivery_receipts_prove_wake(&blocked_json),
        "require-wake rejection must not report proven wake"
    );

    let success_send_output = run_candidate_cli(
        &candidate.cli,
        &rig.home,
        &[
            "send",
            "worker",
            "--body",
            "require-wake on a clean idle pane must prove delivery",
            "--subject",
            "contract-success",
            "--summary",
            "The clean-pane wake should complete. Confirm the exact delivery receipt.",
            "--require-wake",
            "--json",
        ],
    );
    let success_send =
        json_output_allowing_nonzero(&success_send_output, "cyclops send --require-wake --json");
    assert!(
        message_was_durably_accepted(&success_send),
        "require-wake on an idle pane must succeed"
    );
    assert!(
        delivery_receipts_prove_wake(&success_send),
        "require-wake on an idle pane must prove delivery; receipt metadata: {:?}",
        delivery_receipt_summary(&success_send)
    );
    assert!(
        success_send_output.status.success(),
        "require-wake returned non-zero despite a wake-proven receipt"
    );
    let message_id = success_send["msg_id"].as_str().expect("send message id");

    let claim_locator = wait_for_candidate_doorbell_locator(&rig._tmux, &rig.worker_pane);
    assert_ne!(
        claim_locator, message_id,
        "Format 3 must name the exact notification attempt, not only its message"
    );
    rig.replace_worker_with_recipient_command_loop(&candidate.cli);

    let claim_plain = rig.run_cli_as_recipient(
        &candidate.cli,
        "claim",
        &["inbox", "claim", &claim_locator, "--plain"],
    );
    assert_eq!(
        claim_plain.code,
        0,
        "recipient claim failed with status {} and {} captured stderr bytes; captured output withheld",
        claim_plain.code,
        claim_plain.stderr.len()
    );
    assert!(
        claim_plain.stderr.is_empty(),
        "recipient claim polluted stderr with {} captured bytes; captured output withheld",
        claim_plain.stderr.len()
    );
    let claim_plain_text = String::from_utf8(claim_plain.stdout).expect("claim --plain is UTF-8");
    assert!(
        claim_plain_text.contains("require-wake on a clean idle pane must prove delivery"),
        "claim --plain did not print message body"
    );
    assert!(
        claim_plain_text.contains(&format!("[cyclops {message_id}]")),
        "claim --plain did not print envelope marker"
    );

    let re_claim_plain = rig.run_cli_as_recipient(
        &candidate.cli,
        "reclaim",
        &["inbox", "claim", &claim_locator, "--plain"],
    );
    assert_eq!(
        re_claim_plain.code,
        0,
        "repeat recipient claim failed with status {} and {} captured stderr bytes; captured output withheld",
        re_claim_plain.code,
        re_claim_plain.stderr.len()
    );
    assert!(
        re_claim_plain.stderr.is_empty(),
        "repeat recipient claim polluted stderr with {} captured bytes; captured output withheld",
        re_claim_plain.stderr.len()
    );
    let re_claim_text =
        String::from_utf8(re_claim_plain.stdout).expect("repeat claim --plain is UTF-8");
    assert_eq!(
        re_claim_text, claim_plain_text,
        "repeated claim must be idempotent and return identical envelope"
    );

    rig.shutdown(&candidate.cli);

    json!({
        "status": "passed",
        "daemon_status_version_matches_candidate": true,
        "default_send_accepts_without_proven_wake": true,
        "require_wake_fails_closed_on_blocked_pane": true,
        "require_wake_proves_delivery_on_idle_pane": true,
        "claim_plain_emits_envelope_and_body": true,
        "claim_plain_is_idempotent": true
    })
}

#[test]
fn test_sha40_validation() {
    let valid = "aa93af51f0781c705a980c735271bdcaf080c8a5";
    assert_eq!(validate_frozen_sha(valid).unwrap(), valid);
    assert_eq!(expected_build_ref_prefix(valid).unwrap(), "aa93af5");

    assert!(validate_frozen_sha("short").is_err());
    assert!(validate_frozen_sha("aa93af51f0781c705a980c735271bdcaf080c8aG").is_err());
}

#[test]
fn test_frozen_version_validation() {
    let sha = "aa93af51f0781c705a980c735271bdcaf080c8a5";
    let version = "cyclops 0.1.0 (aa93af5)";
    assert_eq!(
        validate_frozen_version(version, sha, "cyclops").unwrap(),
        "aa93af5"
    );

    let wrong = "cyclops 0.1.0 (deadbee)";
    assert!(validate_frozen_version(wrong, sha, "cyclops").is_err());

    let dirty = "cyclops 0.1.0 (aa93af5.dirty)";
    assert!(validate_frozen_version(dirty, sha, "cyclops").is_err());
}

#[test]
fn test_release_profile_validation() {
    assert!(validate_release_profile(false).is_ok());
    assert!(validate_release_profile(true).is_err());
}

#[test]
fn test_status_daemon_identity_uses_the_protocol_field_and_package_version() {
    let status = json!({"daemon_version": "0.1.0"});
    assert!(status_daemon_matches_candidate(
        &status,
        "cyclopsd 0.1.0 (aa93af5)"
    ));
    assert!(!status_daemon_matches_candidate(
        &json!({"daemon": {"version": "cyclopsd 0.1.0 (aa93af5)"}}),
        "cyclopsd 0.1.0 (aa93af5)"
    ));
    assert!(!status_daemon_matches_candidate(
        &json!({"daemon_version": "0.2.0"}),
        "cyclopsd 0.1.0 (aa93af5)"
    ));
}

#[test]
fn test_list_pane_lookup_uses_the_cli_agents_field() {
    let list = json!({
        "home": "/private/tmp/cyc", // scratch-path-lint: non-filesystem JSON fixture
        "sessions": ["release-candidate"],
        "agents": [
            {"pane_id": "%1", "agent": "blocked"},
            {"pane_id": "%2", "agent": "worker"}
        ]
    });
    assert_eq!(listed_agent(&list, "%2").unwrap()["agent"], "worker");
    assert!(listed_agent(&list, "%9").is_none());
    assert!(listed_agent(&json!([]), "%2").is_none());
}

#[test]
fn test_capture_extracts_only_an_exact_format_3_attempt_locator() {
    let attempt =
        cyclops_proto::NotificationAttemptId::parse("att-aa93af51-f078-4c70-8a59-805c735271bd")
            .unwrap();
    let payload = cyclops_proto::render_doorbell_v3(attempt);
    let locator = cyclops_proto::notification_attempt_claim_locator(attempt).to_string();
    let capture = format!("transcript\n❯ {payload}   \nModel x · Ctx: 78%\n");
    assert_eq!(
        doorbell_locator_from_capture(&capture).as_deref(),
        Some(locator.as_str())
    );
    assert_eq!(
        doorbell_locator_from_capture("❯ cyclops inbox claim m-message-only\n"),
        None
    );
}

#[test]
fn test_durable_acceptance_uses_the_current_send_envelope() {
    assert!(message_was_durably_accepted(&json!({
        "msg_id": "m-accepted",
        "seq": 1,
        "inserted": true,
        "deliveries": []
    })));
    for invalid in [
        json!({"status": "accepted"}),
        json!({"msg_id": "accepted", "seq": 1, "deliveries": []}),
        json!({"msg_id": "m-accepted", "seq": 0, "deliveries": []}),
        json!({"msg_id": "m-accepted", "seq": 1}),
        json!({"msg_id": "m-accepted", "seq": 1, "deliveries": [], "inserted": "yes"}),
    ] {
        assert!(!message_was_durably_accepted(&invalid), "{invalid}");
    }
}

#[test]
fn test_frozen_checkout_validation() {
    let sha = "aa93af51f0781c705a980c735271bdcaf080c8a5";
    assert!(validate_frozen_checkout_state(sha, "", sha).is_ok());

    let wrong_head = "003c302b70583068952580cf793cfa5b0ea1696b";
    assert!(validate_frozen_checkout_state(wrong_head, "", sha).is_err());

    let dirty = " M src/cyclopsd\n";
    assert!(validate_frozen_checkout_state(sha, dirty, sha).is_err());
}

#[test]
fn test_bounded_output_drains_past_the_retained_cap() {
    let mut command = Command::new("python3");
    command.args([
        "-c",
        "import sys; sys.stdout.write('A' * 2097152); sys.stderr.write('B' * 2097152)",
    ]);
    let output = bounded_output(&mut command, "large pipe test");
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), CHILD_OUTPUT_CAP);
    assert_eq!(output.stderr.len(), CHILD_OUTPUT_CAP);
}

#[test]
fn test_candidate_failure_diagnostics_withhold_captured_content() {
    let reasons = [
        CapturedOutputFailure::TimedOut,
        CapturedOutputFailure::WaitFailed,
        CapturedOutputFailure::ExitStatus(Some(7)),
        CapturedOutputFailure::ExitStatus(None),
        CapturedOutputFailure::UnexpectedStderr(42),
        CapturedOutputFailure::InvalidJson,
    ];
    for reason in reasons {
        let message = captured_output_failure("candidate CLI", reason);
        assert!(message.contains("captured output withheld"));
        assert!(!message.contains("PRIVATE-BENCHMARK-PAYLOAD"));
    }
}

#[test]
fn test_wake_proof_logic() {
    let proven = json!({"deliveries": [
        {"to": "one", "state": "queued", "notification_state": "submitted"},
        {"to": "two", "state": "queued", "notification_state": "notified"}
    ]});
    assert!(delivery_receipts_prove_wake(&proven));

    let mixed = json!({"deliveries": [
        {"to": "one", "state": "queued", "notification_state": "submitted"},
        {"to": "two", "state": "queued", "notification_state": "staged"}
    ]});
    assert!(!delivery_receipts_prove_wake(&mixed));

    let unknown = json!({"deliveries": [
        {"to": "one", "state": "queued", "notification_state": "from_next_year"}
    ]});
    assert!(!delivery_receipts_prove_wake(&unknown));
}

async fn wait_for_notification(rig: &Rig, message_id: &str, wanted: MessageNotificationState) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = rig
            .daemon
            .messages_snapshot_for_test("admin", 100)
            .expect("messages snapshot");
        let state = snapshot.rows.iter().find_map(|row| {
            (row.message_id.as_str() == message_id).then(|| {
                row.recipients
                    .iter()
                    .find(|recipient| recipient.label == "worker")
                    .map(|recipient| recipient.notification.state)
            })?
        });
        if state == Some(wanted) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} never reached {wanted:?}; last state {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "opt-in frozen-candidate benchmark; requires release profile, clean exact SHA, and both candidate binaries"]
async fn frozen_candidate_separates_transport_phases() {
    let candidate = try_frozen_candidate().unwrap_or_else(|preflight_error| {
        panic!("frozen transport benchmark preflight failed: {preflight_error}")
    });

    let cli_contract = run_external_candidate_contract(&candidate);
    let count = samples();
    let mut rig = Rig::new(
        "release-transport",
        CAT_MANIFEST,
        &manual_lifecycle_composer_pane(),
        "receipt_block_ms = 5000\nack_timeout_ms = 1500\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // Warm every lane before collecting serial samples.
    let ping = rig.ctl.request("ping", json!({})).await;
    assert!(ping.get("error").is_none(), "ping warm-up failed: {ping}");
    let warm_cli = Command::new(&candidate.cli.path)
        .arg("--version")
        .output()
        .expect("warm candidate CLI");
    assert!(warm_cli.status.success());

    let mut socket_rpc = Vec::with_capacity(count);
    for _ in 0..count {
        let started = Instant::now();
        let response = rig.ctl.request("ping", json!({})).await;
        socket_rpc.push(micros(started));
        assert!(response.get("error").is_none(), "ping failed: {response}");
    }

    let mut cli_startup = Vec::with_capacity(count);
    for _ in 0..count {
        let started = Instant::now();
        let output = Command::new(&candidate.cli.path)
            .arg("--version")
            .output()
            .expect("run candidate CLI");
        cli_startup.push(micros(started));
        assert!(output.status.success(), "candidate CLI startup failed");
    }

    let mut durable_acceptance = Vec::with_capacity(count);
    let mut claim = Vec::with_capacity(count);
    for index in 0..count {
        let started = Instant::now();
        let sent = rig
            .ctl
            .request(
                "msg.send",
                json!({
                    "to": ["admin"],
                    "subject": format!("release acceptance {index}"),
                    "body": "content excluded from benchmark output",
                    "client_key": format!("release-acceptance-{index}"),
                }),
            )
            .await;
        durable_acceptance.push(micros(started));
        assert!(sent.get("error").is_none(), "msg.send failed: {sent}");
        let message_id = sent["result"]["msg_id"]
            .as_str()
            .expect("msg.send result message id");

        let started = Instant::now();
        let claimed = rig
            .ctl
            .request("inbox.claim", json!({"message_id": message_id}))
            .await;
        claim.push(micros(started));
        assert!(
            claimed.get("error").is_none(),
            "inbox.claim failed: {claimed}"
        );
    }

    let notification_samples = count.min(20);
    let mut notification = Vec::with_capacity(notification_samples);
    for index in 0..notification_samples {
        let started = Instant::now();
        let sent = rig
            .daemon
            .msg_send(
                "admin",
                serde_json::from_value(json!({
                    "to": ["worker"],
                    "subject": format!("release notification {index}"),
                    "body": "content excluded from benchmark output",
                    "client_key": format!("release-notification-{index}"),
                }))
                .expect("notification params"),
            )
            .await
            .expect("notification send");
        let message_id = sent["msg_id"].as_str().expect("notification message id");
        wait_for_notification(&rig, message_id, MessageNotificationState::Notified).await;
        notification.push(micros(started));
        rig.daemon
            .claim_message_for_test("worker", message_id)
            .expect("settle notification sample by exact claim");
    }

    let report = json!({
        "schema": 2,
        "kind": "cyclops_frozen_transport_benchmark",
        "frozen_commit_sha40": candidate.sha,
        "benchmark_test_build_ref": cyclops_proto::BUILD_REF,
        "external_candidate_binary_evidence": {
            "cli_version": candidate.cli.version,
            "daemon_version": candidate.daemon.version,
            "cli_build_ref_prefix": candidate.cli.build_ref_prefix,
            "daemon_build_ref_prefix": candidate.daemon.build_ref_prefix
        },
        "production_cli_contract": cli_contract,
        "sample_mode": "serial",
        "external_candidate_process_timing": {
            "cli_version_startup": distribution(cli_startup)
        },
        "in_process_fixture_transport_timing": {
            "raw_persistent_socket_rpc": distribution(socket_rpc),
            "raw_durable_acceptance_rpc": distribution(durable_acceptance),
            "raw_notification_pipeline": distribution(notification),
            "raw_claim_rpc": distribution(claim),
            "agent_response": {
                "status": "measured_by_separate_live_vendor_campaign"
            }
        }
    });
    println!("CYCLOPS_RELEASE_TRANSPORT_JSON={report}");
    rig.shutdown().await;
}
