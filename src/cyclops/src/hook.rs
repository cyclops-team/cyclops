//! The receiver vendor hook configs invoke: `cyclops hook <event>`.
//!
//! Runs inside vendor hook budgets, so the contract is strict: fast,
//! silent, exit 0 no matter what. A hook that fails loudly or slowly
//! breaks the agent CLI that called it, which is worse than one lost
//! report. Failures append a line to $CYCLOPS_HOME/hook-errors.log.
//!
//! The event name is an argument, not a payload field, because agy hook
//! payloads carry no event-name field at all, so every vendor hook
//! entry registers a distinct self-tagging command.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::client::Client;
use crate::client::ClientError;
use crate::copy;
use cyclops_proto::StateReportParams;
use cyclops_state::StateRoot;

/// Optional sequence namespace for label-free generated hook commands.
const AGENT_ENV: &str = "CYCLOPS_AGENT";

/// Total wall-clock budget. The per-phase caps below keep the worst case
/// near it; the typical run is single-digit milliseconds.
const BUDGET: Duration = Duration::from_secs(3);
const STDIN_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const HELLO_TIMEOUT: Duration = Duration::from_millis(500);

pub fn run(event: &str, agent_flag: Option<&str>) -> i32 {
    let deadline = Instant::now() + BUDGET;
    if let Err(cause) = post(event, agent_flag, deadline) {
        log_error(event, &cause);
    }
    0
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// What one phase may spend: the remaining budget, capped by the phase's
/// own limit. A spent budget is refused here, before any socket is opened
/// or waited on, so no phase ever extends past the deadline by a floor.
fn phase_budget(deadline: Instant, cap: Duration) -> Result<Duration, String> {
    phase_budget_at(Instant::now(), deadline, cap)
}

/// [`phase_budget`] against an explicit clock reading, so the ordering of
/// phases can be proven with a scripted clock and no sockets.
fn phase_budget_at(now: Instant, deadline: Instant, cap: Duration) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(now);
    if left.is_zero() {
        return Err("hook budget spent".into());
    }
    Ok(left.min(cap))
}

/// Connect, then Hello, then the request, each budgeted from what is left
/// of the deadline at the moment it starts. The clock is read again after
/// every phase returns, so whatever a phase spent is gone before the next
/// budget is set: a later phase can only shrink, never extend the deadline,
/// and no phase starts once the budget is spent. The outer error is the
/// spent budget (final); the inner one is the client's, for the caller to
/// classify. The clock and all three phases are injectable so a test proves
/// the ordering without a daemon.
fn connect_hello_request<S, C, R>(
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    connect: impl FnOnce(Duration) -> Result<S, ClientError>,
    hello: impl FnOnce(S, Duration) -> Result<C, ClientError>,
    request: impl FnOnce(C, Duration) -> Result<R, ClientError>,
) -> Result<Result<R, ClientError>, String> {
    let connect_budget = phase_budget_at(now(), deadline, CONNECT_TIMEOUT)?;
    let stream = match connect(connect_budget) {
        Ok(stream) => stream,
        Err(e) => return Ok(Err(e)),
    };
    let hello_budget = phase_budget_at(now(), deadline, HELLO_TIMEOUT)?;
    let client = match hello(stream, hello_budget) {
        Ok(client) => client,
        Err(e) => return Ok(Err(e)),
    };
    let read_budget = phase_budget_at(now(), deadline, BUDGET)?;
    Ok(request(client, read_budget))
}

fn post(event: &str, agent_flag: Option<&str>, deadline: Instant) -> Result<(), String> {
    let agent = agent_flag.map(String::from);
    let sequence_namespace = agent.clone().or_else(|| {
        std::env::var(AGENT_ENV)
            .ok()
            .filter(|name| !name.is_empty())
    });
    let home = cyclops_proto::cyclops_home();
    // stdin is read exactly once and the sequence is allocated exactly once:
    // every retry below resends the identical parameters, so the daemon's
    // dedupe sees one report however many times the wire carried it.
    let payload = match read_stdin(phase_budget(deadline, STDIN_TIMEOUT)?) {
        Ok(v) => v,
        Err(cause) => {
            log_error(event, &format!("{cause}; reported with an empty payload"));
            Value::Null
        }
    };
    let seq = match &sequence_namespace {
        Some(a) => Some(next_seq(&home, a)?),
        None => None,
    };
    let params = serde_json::to_value(StateReportParams {
        agent,
        event: event.to_string(),
        seq,
        payload,
    })
    .expect("state report params serialize");
    let session_start = is_session_start(event);
    let mut backoff = RETRY_BACKOFF_MIN;
    loop {
        // A spent budget fails here, before another connect, so a backoff
        // that slept up to the deadline never buys one more attempt.
        let outcome = send_once(&params, deadline)?;
        match retry_decision(session_start, &outcome, remaining(deadline)) {
            Retry::Done => return Ok(()),
            Retry::Fail(cause) => return Err(cause),
            Retry::Again => {
                std::thread::sleep(backoff.min(remaining(deadline)));
                backoff = (backoff * 2).min(RETRY_BACKOFF_MAX);
            }
        }
    }
}

/// One connect, one request, one classified outcome. Every phase takes its
/// time from the shared deadline and none of them may start once it is
/// spent: the error names the spent budget as the final cause.
fn send_once(params: &Value, deadline: Instant) -> Result<Outcome, String> {
    let answer = connect_hello_request(
        deadline,
        Instant::now,
        Client::connect_stream,
        Client::from_stream,
        |mut c, read| {
            c.set_read_timeout(read);
            c.request("agent.state.report", params.clone())
        },
    )?;
    Ok(match answer {
        Ok(response) => Outcome::Answered(response),
        Err(e) => classify(e),
    })
}

/// The daemon answered (any JSON), refused the request as a wire error, or
/// the outcome is unknown because the connection or the reply was lost.
enum Outcome {
    Answered(Value),
    Denied(String),
    Unknown(String),
}

fn classify(e: ClientError) -> Outcome {
    match e {
        ClientError::Server { .. } => Outcome::Denied(copy::client_error(&e, None)),
        other => Outcome::Unknown(copy::client_error(&other, None)),
    }
}

enum Retry {
    Done,
    Again,
    Fail(String),
}

const RETRY_BACKOFF_MIN: Duration = Duration::from_millis(50);
const RETRY_BACKOFF_MAX: Duration = Duration::from_millis(400);

fn is_session_start(event: &str) -> bool {
    let folded: String = event
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    folded == "sessionstart"
}

/// Success is only `applied: true` or `duplicate: true` (the first report
/// landed). Only `SessionStart` is ever retried, and only for two outcomes:
/// the daemon's exact retryable route-not-ready tuple (`applied: false`,
/// `reason: hook_route_not_ready`, `retryable: true`; the pane's route is
/// not open yet and nothing was recorded), or an unknown outcome where the
/// connection or the reply was lost. Every other `applied: false` answer,
/// a missing `applied`, manifest mismatch, occupant change, malformed
/// input, and every denial are final and logged. The budget is the shared
/// three-second hook budget; once it is spent nothing retries, the last
/// failure is logged once, and the hook still exits zero.
fn retry_decision(session_start: bool, outcome: &Outcome, remaining: Duration) -> Retry {
    match outcome {
        Outcome::Answered(response) => {
            if response.get("applied") == Some(&Value::Bool(true))
                || response.get("duplicate") == Some(&Value::Bool(true))
            {
                return Retry::Done;
            }
            let reason = response
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no reason");
            let route_not_ready = response.get("applied") == Some(&Value::Bool(false))
                && reason == "hook_route_not_ready"
                && response.get("retryable") == Some(&Value::Bool(true));
            if !route_not_ready {
                return Retry::Fail(format!("daemon did not apply the report: {reason}"));
            }
            if !session_start {
                return Retry::Fail(
                    "daemon route not ready; only SessionStart retries that answer".into(),
                );
            }
            if remaining.is_zero() {
                return Retry::Fail(
                    "daemon route not ready for SessionStart within the hook budget".into(),
                );
            }
            Retry::Again
        }
        Outcome::Denied(cause) => Retry::Fail(cause.clone()),
        Outcome::Unknown(cause) => {
            if session_start && !remaining.is_zero() {
                Retry::Again
            } else {
                Retry::Fail(cause.clone())
            }
        }
    }
}

/// Read all of stdin on a helper thread so a stalled pipe cannot eat the
/// budget. Empty input is a null payload. Input that is not JSON is kept
/// for audit under {"raw": ...} instead of being dropped: tolerance of
/// vendor payload shapes means never validating them.
fn read_stdin(timeout: Duration) -> Result<Value, String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = String::new();
        let res = std::io::stdin().read_to_string(&mut buf).map(|_| buf);
        let _ = tx.send(res);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(text)) => {
            let t = text.trim();
            if t.is_empty() {
                Ok(Value::Null)
            } else {
                Ok(serde_json::from_str(t).unwrap_or_else(|_| json!({ "raw": t })))
            }
        }
        Ok(Err(e)) => Err(format!("stdin read failed: {e}")),
        Err(_) => Err("stdin gave no payload in time".into()),
    }
}

/// Next value of the per-agent monotonic counter, persisted at
/// $CYCLOPS_HOME/hookseq/<agent>. Hooks are separate short-lived
/// processes, so monotonicity has to live in a file; the OS file lock
/// serializes concurrent invocations for the same agent.
fn next_seq(home: &Path, agent: &str) -> Result<u64, String> {
    // Labels are daemon-validated, but a hook config can carry anything;
    // never let one path-traverse out of hookseq.
    let descendant = Path::new("hookseq").join(agent.replace('/', "_"));
    let state_root = StateRoot::open_or_create(home)
        .map_err(|e| format!("can't open state root {}: {e}", home.display()))?;
    let path = state_root.path().join(&descendant);
    let mut f = state_root
        .open_append(&descendant)
        .map_err(|e| format!("can't open {}: {e}", path.display()))?;
    f.lock()
        .map_err(|e| format!("can't lock {}: {e}", path.display()))?;
    let mut text = String::new();
    f.read_to_string(&mut text)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?;
    // Garbage and a fresh file both restart at 1: a reset counter is
    // detectable downstream, a dead hook is not.
    let next = text.trim().parse::<u64>().unwrap_or(0).saturating_add(1);
    f.seek(SeekFrom::Start(0))
        .and_then(|_| f.set_len(0))
        .and_then(|_| f.write_all(next.to_string().as_bytes()))
        .map_err(|e| format!("can't write {}: {e}", path.display()))?;
    // Lock releases when f drops.
    Ok(next)
}

/// One line per failure: UTC timestamp, event, cause. Never prints and
/// never errors; a hook's stdio belongs to the vendor CLI.
fn log_error(event: &str, cause: &str) {
    let home = cyclops_proto::cyclops_home();
    log_error_at(&home, event, cause);
}

const HOOK_ERROR_LOG_BYTES: u64 = 256 * 1024;
const HOOK_ERROR_LINE_BYTES: usize = 4096;

fn log_error_at(home: &Path, event: &str, cause: &str) {
    let Ok(state_root) = StateRoot::open_or_create(home) else {
        return;
    };
    let Ok(mut f) = state_root.open_append(Path::new("hook-errors.log")) else {
        return;
    };
    if !matches!(f.try_lock(), Ok(true)) {
        return;
    }

    let prefix = format!(
        "{} hook {}: ",
        utc_stamp(crate::render::now_ms()),
        log_field(event, 128)
    );
    let available = HOOK_ERROR_LINE_BYTES.saturating_sub(prefix.len() + 1);
    let line = format!("{prefix}{}\n", log_field(cause, available));
    let Ok(length) = f.seek(SeekFrom::End(0)) else {
        return;
    };
    if length.saturating_add(line.len() as u64) > HOOK_ERROR_LOG_BYTES
        && (f.set_len(0).is_err() || f.seek(SeekFrom::Start(0)).is_err())
    {
        return;
    }
    let _ = f.write_all(line.as_bytes());
}

/// Keep one hook failure on one bounded log line.
fn log_field(value: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars() {
        let character = if matches!(character, '\n' | '\r') {
            ' '
        } else {
            character
        };
        if out.len() + character.len_utf8() > max_bytes {
            break;
        }
        out.push(character);
    }
    out
}

/// UTC "YYYY-MM-DDTHH:MM:SSZ" from Unix ms. std ships no calendar; this
/// is enough for a debug log.
fn utc_stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        (s % 3600) / 60,
        s % 60
    )
}

/// Days since 1970-01-01 to (year, month, day). Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn utc_stamp_known_instants() {
        assert_eq!(utc_stamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_stamp(86_400_000), "1970-01-02T00:00:00Z");
        // The billennium: 1e9 seconds.
        assert_eq!(utc_stamp(1_000_000_000_000), "2001-09-09T01:46:40Z");
    }

    #[test]
    fn seq_counter_starts_at_one_and_advances() {
        // Scratch state goes through the shared root, not the OS
        // temp dir, so CYCLOPS_TEST_TMP relocates this test with the rest.
        // That the root really relocates is proven once, in cyclopsd's
        // scratch_override test.
        let home = cyclops_proto::scratch::scratch_dir("cyc-hookseq");
        let _ = fs::remove_dir_all(&home);
        assert_eq!(next_seq(&home, "reviewer").unwrap(), 1);
        assert_eq!(next_seq(&home, "reviewer").unwrap(), 2);
        // Independent per agent.
        assert_eq!(next_seq(&home, "implementer").unwrap(), 1);
        // Garbage resets to 1 instead of killing the hook.
        fs::write(home.join("hookseq/reviewer"), "not a number").unwrap();
        assert_eq!(next_seq(&home, "reviewer").unwrap(), 1);
        // Labels cannot escape the hookseq dir.
        assert_eq!(next_seq(&home, "../evil").unwrap(), 1);
        assert!(home.join("hookseq/.._evil").exists());
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("hookseq")), 0o700);
        assert_eq!(mode(&home.join("hookseq/reviewer")), 0o600);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn seq_counter_refuses_a_symlink_without_mutating_its_target() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-hookseq-link");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("hookseq")).unwrap();
        let external = home.with_extension("external");
        let _ = fs::remove_file(&external);
        fs::write(&external, "41").unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&external, home.join("hookseq/reviewer")).unwrap();

        assert!(next_seq(&home, "reviewer").is_err());
        assert_eq!(fs::read_to_string(&external).unwrap(), "41");
        assert_eq!(mode(&external), 0o640);

        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_file(&external);
    }

    #[test]
    fn hook_error_log_is_owner_only_and_appends() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-hook-errors");
        let _ = fs::remove_dir_all(&home);

        log_error_at(&home, "Stop", "first");
        log_error_at(&home, "Stop", "second");

        let path = home.join("hook-errors.log");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("hook Stop: first"));
        assert!(text.contains("hook Stop: second"));
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&path), 0o600);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn hook_error_log_is_bounded_and_one_line_per_failure() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-hook-errors-bounded");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("hook-errors.log"),
            vec![b'x'; HOOK_ERROR_LOG_BYTES as usize],
        )
        .unwrap();

        log_error_at(&home, "Stop\nforged", &"failure\n".repeat(2000));

        let path = home.join("hook-errors.log");
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() <= HOOK_ERROR_LINE_BYTES);
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert!(String::from_utf8(bytes)
            .unwrap()
            .contains("hook Stop forged: failure failure"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn hook_error_log_never_waits_for_another_writer() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-hook-errors-locked");
        let _ = fs::remove_dir_all(&home);
        let state_root = StateRoot::open_or_create(&home).unwrap();
        let mut held = state_root
            .open_append(Path::new("hook-errors.log"))
            .unwrap();
        held.write_all(b"existing\n").unwrap();
        held.lock().unwrap();

        log_error_at(&home, "Stop", "must not wait");
        assert_eq!(
            fs::read(home.join("hook-errors.log")).unwrap(),
            b"existing\n"
        );

        drop(held);
        log_error_at(&home, "Stop", "written after release");
        assert!(fs::read_to_string(home.join("hook-errors.log"))
            .unwrap()
            .contains("hook Stop: written after release"));
        let _ = fs::remove_dir_all(&home);
    }

    /// Deterministic: an already-expired deadline stays expired, so every
    /// phase refuses before it opens or waits on anything, and a live
    /// deadline is capped by the phase limit, never extended by a floor.
    #[test]
    fn a_spent_budget_refuses_every_phase_and_never_extends() {
        let expired = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            phase_budget(expired, CONNECT_TIMEOUT),
            Err("hook budget spent".to_string())
        );
        assert_eq!(
            phase_budget(expired, BUDGET),
            Err("hook budget spent".to_string())
        );
        let live = Instant::now() + Duration::from_secs(10);
        let capped = phase_budget(live, HELLO_TIMEOUT).unwrap();
        assert!(capped <= HELLO_TIMEOUT && !capped.is_zero(), "{capped:?}");
        let not_ready = Outcome::Answered(serde_json::json!({
            "applied": false, "reason": "hook_route_not_ready", "retryable": true
        }));
        let lost = Outcome::Unknown("connection reset".into());
        assert!(matches!(
            retry_decision(true, &not_ready, Duration::ZERO),
            Retry::Fail(_)
        ));
        assert!(matches!(
            retry_decision(true, &lost, Duration::ZERO),
            Retry::Fail(_)
        ));
    }

    /// Deterministic, no sockets: a scripted clock stands in for time. The
    /// connect is granted what is left at its start; the Hello is budgeted
    /// from what is left AFTER the connect returned, so the connect's spend
    /// shrinks it; the request is budgeted from what is left after the
    /// Hello, so the Hello's spend shrinks it in turn.
    #[test]
    fn hello_is_budgeted_from_what_the_connect_left() {
        let t0 = Instant::now();
        let deadline = t0 + BUDGET;
        // 2.95 s spent connecting: 50 ms remain for the Hello; the Hello
        // spends 20 ms of it, so the request gets the last 30 ms.
        let mut ticks = vec![
            t0,
            t0 + Duration::from_millis(2950),
            t0 + Duration::from_millis(2970),
        ]
        .into_iter();
        let result = connect_hello_request(
            deadline,
            move || ticks.next().expect("three clock reads"),
            |connect_budget| {
                assert_eq!(
                    connect_budget, CONNECT_TIMEOUT,
                    "capped, not the whole budget"
                );
                Ok::<(), ClientError>(())
            },
            |(), hello_budget| {
                assert_eq!(
                    hello_budget,
                    Duration::from_millis(50),
                    "what the connect left"
                );
                Ok::<(), ClientError>(())
            },
            |(), read_budget| {
                assert_eq!(
                    read_budget,
                    Duration::from_millis(30),
                    "what the Hello left"
                );
                Ok::<(), ClientError>(())
            },
        );
        assert!(
            matches!(result, Ok(Ok(()))),
            "all three phases run, each within its budget"
        );
    }

    /// A Hello that consumes the remaining budget leaves nothing for the
    /// request: the request callback is never invoked and the spent budget
    /// is the final cause.
    #[test]
    fn a_hello_that_spends_the_budget_leaves_no_request() {
        let t0 = Instant::now();
        let deadline = t0 + BUDGET;
        let mut ticks = vec![t0, t0 + Duration::from_secs(1), deadline].into_iter();
        let mut request_started = false;
        let result = connect_hello_request(
            deadline,
            move || ticks.next().expect("three clock reads"),
            |_| Ok::<(), ClientError>(()),
            |(), hello_budget| {
                assert_eq!(hello_budget, HELLO_TIMEOUT, "plenty left, so the cap");
                Ok::<(), ClientError>(())
            },
            |(), _| {
                request_started = true;
                Ok::<(), ClientError>(())
            },
        );
        assert!(
            matches!(result, Err(ref cause) if cause == "hook budget spent"),
            "the spent budget is the final cause"
        );
        assert!(
            !request_started,
            "the request must never start on a spent budget"
        );
    }

    #[test]
    fn a_connect_that_spends_the_budget_leaves_no_hello() {
        let t0 = Instant::now();
        let deadline = t0 + BUDGET;
        let mut ticks = vec![t0, deadline].into_iter();
        let mut hello_started = false;
        let mut request_started = false;
        let result = connect_hello_request(
            deadline,
            move || ticks.next().expect("two clock reads"),
            |_| Ok::<(), ClientError>(()),
            |(), _| {
                hello_started = true;
                Ok::<(), ClientError>(())
            },
            |(), _| {
                request_started = true;
                Ok::<(), ClientError>(())
            },
        );
        assert!(
            matches!(result, Err(ref cause) if cause == "hook budget spent"),
            "the spent budget is the final cause"
        );
        assert!(
            !hello_started && !request_started,
            "neither later phase may start on a spent budget"
        );
    }

    #[test]
    fn a_spent_budget_never_starts_the_connect() {
        let t0 = Instant::now();
        let deadline = t0 + BUDGET;
        let mut ticks = vec![deadline].into_iter();
        let mut connect_started = false;
        let result = connect_hello_request(
            deadline,
            move || ticks.next().expect("one clock read"),
            |_| {
                connect_started = true;
                Ok::<(), ClientError>(())
            },
            |(), _| Ok::<(), ClientError>(()),
            |(), _| Ok::<(), ClientError>(()),
        );
        assert!(
            matches!(result, Err(ref cause) if cause == "hook budget spent"),
            "the spent budget is the final cause"
        );
        assert!(!connect_started);
    }

    /// A connect failure is the client's to classify, never a spent budget.
    #[test]
    fn a_failed_connect_is_classified_not_budgeted() {
        let t0 = Instant::now();
        let deadline = t0 + BUDGET;
        let mut ticks = vec![t0].into_iter();
        let later_started = std::cell::Cell::new(false);
        let result = connect_hello_request(
            deadline,
            move || ticks.next().expect("one clock read"),
            |_| Err::<(), ClientError>(ClientError::NotRunning),
            |(), _| {
                later_started.set(true);
                Ok::<(), ClientError>(())
            },
            |(), _| {
                later_started.set(true);
                Ok::<(), ClientError>(())
            },
        );
        assert!(!later_started.get(), "a failed connect ends the sequence");
        assert!(
            matches!(result, Ok(Err(ClientError::NotRunning))),
            "a connect failure is classified, not budgeted"
        );
    }

    /// Success is applied or duplicate, nothing else: an answer without
    /// `applied`, or `applied: false` for any reason but the exact
    /// retryable route-not-ready tuple, is a logged failure.
    #[test]
    fn only_applied_or_duplicate_answers_succeed() {
        let plenty = Duration::from_secs(2);
        let bare = Outcome::Answered(serde_json::json!({"state": "idle"}));
        let not_applied = Outcome::Answered(serde_json::json!({"applied": false}));
        let not_retryable = Outcome::Answered(serde_json::json!({
            "applied": false, "reason": "hook_route_not_ready", "retryable": false
        }));
        let applied_but_named = Outcome::Answered(serde_json::json!({
            "applied": true, "reason": "hook_route_not_ready", "retryable": true
        }));
        assert!(matches!(
            retry_decision(true, &bare, plenty),
            Retry::Fail(_)
        ));
        assert!(matches!(
            retry_decision(true, &not_applied, plenty),
            Retry::Fail(_)
        ));
        assert!(matches!(
            retry_decision(true, &not_retryable, plenty),
            Retry::Fail(_)
        ));
        assert!(matches!(
            retry_decision(true, &applied_but_named, plenty),
            Retry::Done
        ));
    }

    #[test]
    fn only_session_start_retries_and_only_for_route_not_ready_or_unknown_outcome() {
        let plenty = Duration::from_secs(2);
        let spent = Duration::ZERO;
        let not_ready = Outcome::Answered(serde_json::json!({
            "applied": false, "reason": "hook_route_not_ready", "retryable": true
        }));
        let duplicate = Outcome::Answered(serde_json::json!({"applied": false, "duplicate": true}));
        let applied = Outcome::Answered(serde_json::json!({"applied": true, "state": "idle"}));
        let occupant =
            Outcome::Answered(serde_json::json!({"applied": false, "reason": "occupant_changed"}));
        let manifest =
            Outcome::Answered(serde_json::json!({"applied": false, "reason": "manifest_changed"}));
        let denied = Outcome::Denied("bad_request".into());
        let lost = Outcome::Unknown("connection reset".into());
        assert!(matches!(
            retry_decision(true, &not_ready, plenty),
            Retry::Again
        ));
        assert!(
            matches!(retry_decision(true, &not_ready, spent), Retry::Fail(_)),
            "budget spent"
        );
        assert!(
            matches!(retry_decision(false, &not_ready, plenty), Retry::Fail(_)),
            "other events never retry; route not ready is a logged failure for them"
        );
        assert!(
            matches!(retry_decision(true, &duplicate, plenty), Retry::Done),
            "duplicate is success"
        );
        assert!(matches!(
            retry_decision(true, &applied, plenty),
            Retry::Done
        ));
        assert!(
            matches!(retry_decision(true, &occupant, plenty), Retry::Fail(_)),
            "occupant change is a final, logged failure"
        );
        assert!(
            matches!(retry_decision(true, &manifest, plenty), Retry::Fail(_)),
            "manifest mismatch is a final, logged failure"
        );
        assert!(
            matches!(retry_decision(true, &denied, plenty), Retry::Fail(_)),
            "denial is final"
        );
        assert!(
            matches!(retry_decision(true, &lost, plenty), Retry::Again),
            "unknown outcome retries SessionStart"
        );
        assert!(
            matches!(retry_decision(false, &lost, plenty), Retry::Fail(_)),
            "unknown outcome is final for others"
        );
        assert!(matches!(retry_decision(true, &lost, spent), Retry::Fail(_)));
        assert!(is_session_start("SessionStart"));
        assert!(is_session_start("session_start"));
        assert!(!is_session_start("UserPromptSubmit"));
        assert!(!is_session_start("Stop"));
    }
}
