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
/// Floor for the response wait so a spent budget still reads the reply of
/// an already-written request instead of instantly misreporting it.
const MIN_WAIT: Duration = Duration::from_millis(50);

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

fn post(event: &str, agent_flag: Option<&str>, deadline: Instant) -> Result<(), String> {
    // The label is optional: the daemon derives the reporting origin from
    // the authenticated socket peer, so a hook that does not know its own
    // name can still report. A label supplied here is an assertion about
    // that origin, checked against it and denied on disagreement.
    let agent = agent_flag.map(String::from);
    let sequence_namespace = agent.clone().or_else(|| {
        std::env::var(AGENT_ENV)
            .ok()
            .filter(|name| !name.is_empty())
    });
    let home = cyclops_proto::cyclops_home();
    // Payload trouble is logged but never fatal: the event edge matters
    // more than its audit payload.
    let payload = match read_stdin(remaining(deadline).min(STDIN_TIMEOUT)) {
        Ok(v) => v,
        Err(cause) => {
            log_error(event, &format!("{cause}; reported with an empty payload"));
            Value::Null
        }
    };
    // The counter is per-label, so a report without one carries none
    // rather than sharing a namespace with every other label-free hook.
    let seq = match &sequence_namespace {
        Some(a) => Some(next_seq(&home, a)?),
        None => None,
    };
    let mut c = Client::connect_with_timeouts(
        remaining(deadline).min(CONNECT_TIMEOUT),
        remaining(deadline).min(HELLO_TIMEOUT).max(MIN_WAIT),
    )
    .map_err(|e| copy::client_error(&e, None))?;
    // No proto-mismatch warning here: hooks own no terminal to warn on,
    // and the protocol is tolerant by design.
    c.set_read_timeout(remaining(deadline).max(MIN_WAIT));
    let params = serde_json::to_value(StateReportParams {
        agent,
        event: event.to_string(),
        seq,
        payload,
    })
    .expect("state report params serialize");
    c.request("agent.state.report", params)
        .map(|_| ())
        .map_err(|e| copy::client_error(&e, None))
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
}
