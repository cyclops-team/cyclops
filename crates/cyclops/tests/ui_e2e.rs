//! Headless `cyclops ui` end to end: a canned daemon on a scratch socket
//! feeds events through the real subscription, a fixture ledger feeds the
//! backfill, and the plain follow mode (stdout is a pipe, so the TUI
//! never engages) proves classification, ordering, dedupe, the hanging
//! body line, and the eye word without a terminal anywhere.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use serde_json::{json, Value};

/// Scratch home unique per test and process, under the relocatable
/// scratch root. Kept short: Unix socket paths cap out around 104 bytes
/// on macOS, which is why the root is not the OS temp dir (F24). That the
/// root really relocates is proven once, in cyclopsd's scratch_override
/// test; restating it here as a starts_with could not fail.
fn scratch_home(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyu-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch home");
    dir
}

/// Serve `conns` sequential connections: hello line first, then the
/// closure answers each request line with (reply lines, close-after).
fn serve_conns<F>(home: &Path, conns: usize, mut reply: F)
where
    F: FnMut(Value) -> (Vec<String>, bool) + Send + 'static,
{
    let listener = UnixListener::bind(home.join("sock")).expect("bind scratch socket");
    thread::spawn(move || {
        for _ in 0..conns {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut w = stream;
            writeln!(
                w,
                "{}",
                json!({"cyclops": "0.1.0", "proto": 1, "boot_id": "b-ui"})
            )
            .expect("write hello");
            'conn: loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break 'conn,
                    Ok(_) => {}
                }
                let req: Value = serde_json::from_str(line.trim()).expect("request parses");
                let (lines, close) = reply(req);
                for l in lines {
                    if writeln!(w, "{l}").is_err() {
                        break 'conn;
                    }
                }
                if close {
                    break 'conn;
                }
            }
        }
    });
}

fn run_ui(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env_remove("NO_COLOR")
        .args(args)
        .output()
        .expect("run cyclops binary")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Two msg facts in the session ledger: one to admin (seq 1), one agent
/// to agent (seq 2). Timestamps are fixed so the clock gutter is exact.
///
/// Seq 1 carries a two-line body so the backfill path pins plain mode's
/// hanging body line: the first body line prints at the content column,
/// the rest does not. Seq 2 carries none, so the bodyless message stays
/// one line.
fn write_ledger(home: &Path) {
    let dir = home.join("ledger");
    fs::create_dir_all(&dir).expect("create ledger dir");
    let lines = [
        json!({"seq": 1, "boot_id": "b-ui", "id": "m-a", "ts": 43_471_000u64, "kind": "msg",
               "from": "codex", "to": ["admin"], "subject": "build done",
               "body": "ci is green on main\nsecond line stays off"}),
        json!({"seq": 2, "boot_id": "b-ui", "id": "m-b", "ts": 43_472_000u64, "kind": "msg",
               "from": "codex", "to": ["reviewer"], "subject": "review this"}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(dir.join("main.ndjson"), text).expect("write fixture ledger");
}

/// The canned daemon: the subscribe connection acks and streams four
/// events, then hangs up; the status connection answers one status.
/// Event seqs sit above the ledger tail, so nothing dedupes away; one
/// event repeats ledger seq 2 to prove the dedupe drops it.
///
/// Seq 10 carries a body so the live event path pins the hanging body
/// line too, not just the backfill path. Seq 11 carries an empty body,
/// which is the same as none: no second line.
fn serve_canned(home: &Path) {
    serve_conns(home, 2, move |req| match req["method"].as_str() {
        Some("events.subscribe") => (
            vec![
                json!({"id": req["id"], "result": {"subscribed": true}}).to_string(),
                // A live duplicate of ledger seq 2: must not print twice.
                json!({"event": "msg", "seq": 2, "data": {"ts": 43_472_000u64, "id": "m-b",
                       "from": "codex", "to": ["reviewer"], "subject": "review this"}})
                .to_string(),
                json!({"event": "msg", "seq": 10, "data": {"ts": 43_480_000u64, "id": "m-c",
                       "from": "reviewer", "to": ["admin"], "subject": "need a decision",
                       "body": "the retry budget is spent"}})
                .to_string(),
                json!({"event": "msg", "seq": 11, "data": {"ts": 43_481_000u64, "id": "m-d",
                       "from": "codex", "to": ["reviewer"], "subject": "agent chatter",
                       "body": ""}})
                .to_string(),
                json!({"event": "delivery-state", "seq": 12, "data": {"ts": 43_482_000u64,
                       "id": "m-d", "to": "reviewer", "to_state": "attention_required",
                       "cause": "no_such_pane"}})
                .to_string(),
                json!({"event": "state", "seq": 13, "data": {"ts": 43_483_000u64,
                       "target": "reviewer", "pane_id": "%1", "state": "working"}})
                .to_string(),
            ],
            true,
        ),
        Some("status") => (
            vec![json!({"id": req["id"], "result": {
                "daemon_version": "0.1.0", "proto": 1, "boot_id": "b-ui",
                "uptime_ms": 1000, "tmux_version": "3.6a",
                "sessions": [{"name": "main", "attached": true, "panes": [{
                    "pane_id": "%1", "window_id": "@1", "window_name": "agents",
                    "agent": "reviewer", "manifest": "claude", "title": "reviewer",
                    "current_command": "claude", "dead": false, "in_mode": false,
                    "width": 120, "height": 40, "state": "idle"
                }]}]
            }})
            .to_string()],
            true,
        ),
        other => panic!("unexpected method {other:?}"),
    });
}

#[test]
fn ui_plain_admin_stream_is_calm_and_ends_honestly() {
    let home = scratch_home("adm");
    write_ledger(&home);
    serve_canned(&home);
    let out = run_ui(&home, &["ui", "--plain"]);
    // The daemon hung up: the follow ends with the standard copy, exit 1.
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Both msg lines carry a body, so each hangs its first body line at
    // the content column (10 columns, under the clock gutter). The
    // backfill message's second body line stays off the stream. The eye
    // line names its item: plain mode has no header to point at.
    let expected = "12:04:31  codex → admin  build done\n\
                    \x20         ci is green on main\n\
                    12:04:40  reviewer → admin  need a decision\n\
                    \x20         the retry budget is spent\n\
                    12:04:42  reviewer  ⚠ needs attention · no pane with that name\n\
                    eye opening · 1 needs attention · reviewer  ⚠ needs attention\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "lost the connection to cyclops: the connection closed. Check that cyclopsd is still running, then retry."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ui_plain_firehose_shows_everything_once() {
    let home = scratch_home("fh");
    write_ledger(&home);
    serve_canned(&home);
    let out = run_ui(&home, &["ui", "--plain", "--firehose"]);
    assert_eq!(out.status.code(), Some(1));
    // Backfill first (the admin msg AND the agent-to-agent msg), then the
    // live events in arrival order; the live duplicate of ledger seq 2
    // printed once, from the backfill. Messages with a body hang their
    // first body line; "review this" (no body) and "agent chatter"
    // (empty body) stay one line each.
    //
    // The eye line is asserted apart from the stream. It prints once per
    // batch of messages the loop folds together, and how many live events
    // land in the startup batch is a race with the daemon's answer, so
    // pinning its position would pin the race instead of the behavior.
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let eye = "eye opening · 1 needs attention · reviewer  ⚠ needs attention";
    assert_eq!(
        stdout.lines().filter(|l| *l == eye).count(),
        1,
        "the eye line printed {stdout:?}"
    );
    let stream: Vec<&str> = stdout.lines().filter(|l| *l != eye).collect();
    let expected = vec![
        "12:04:31  codex → admin  build done",
        "          ci is green on main",
        "12:04:32  codex → reviewer  review this",
        "12:04:40  reviewer → admin  need a decision",
        "          the retry budget is spent",
        "12:04:41  codex → reviewer  agent chatter",
        "12:04:42  reviewer  ⚠ needs attention · no pane with that name",
        "12:04:43  reviewer  ● working",
    ];
    assert_eq!(stream, expected);
    // It follows the line that raised it, wherever the batch boundary fell.
    let eye_at = stdout.lines().position(|l| l == eye).expect("the eye line");
    let raised_at = stdout
        .lines()
        .position(|l| l.contains("⚠ needs attention · no pane with that name"))
        .expect("the attention line");
    assert!(eye_at > raised_at, "{stdout:?}");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ui_plain_filter_narrows_the_firehose() {
    let home = scratch_home("flt");
    write_ledger(&home);
    serve_canned(&home);
    let out = run_ui(&home, &["ui", "--plain", "--firehose", "--from", "codex"]);
    assert_eq!(out.status.code(), Some(1));
    // Only codex-sent messages pass; the delivery, the admin msg from
    // reviewer, and the state line drop. The eye still reports, because
    // attention is about the system, not the filter. The one surviving
    // body line follows its message through the filter.
    let expected = "12:04:31  codex → admin  build done\n\
                    \x20         ci is green on main\n\
                    12:04:32  codex → reviewer  review this\n\
                    12:04:41  codex → reviewer  agent chatter\n\
                    eye opening · 1 needs attention · reviewer  ⚠ needs attention\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ui_daemon_down_reports_and_exits_one() {
    let home = scratch_home("down");
    let out = run_ui(&home, &["ui", "--plain"]);
    assert_eq!(out.status.code(), Some(1));
    // The same sentence the CLI prints, from the same place. It used to
    // be a literal here and a literal there, so the stream would have
    // kept saying `cyclopsd &` after `cyclops start` took the job over.
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        cyclops_proto::NOT_RUNNING
    );
    let _ = fs::remove_dir_all(&home);
}

/// One blocked pane and one parked delivery, served the way cyclopsd
/// serves them: the delivery half rides `status` only for a caller that
/// asked (`cyclops_proto::StatusParams`), because folding it reads the
/// session ledgers.
fn answer(open_deliveries: bool) -> Value {
    let mut result = json!({
        "daemon_version": "0.1.0", "proto": 1, "boot_id": "b-ui",
        "uptime_ms": 120_000, "tmux_version": "3.6a",
        "sessions": [{"name": "main", "attached": true, "panes": [{
            "pane_id": "%1", "window_id": "@1", "window_name": "agents",
            "agent": "reviewer", "manifest": "claude", "title": "reviewer",
            "current_command": "claude", "dead": false, "in_mode": false,
            "width": 120, "height": 40, "state": "blocked_permission"
        }]}]
    });
    if open_deliveries {
        result["open_deliveries"] = json!([{
            "id": "m-park", "to": "implementer",
            "state": "parked_blocked_quota", "ts": 43_471_000u64,
            "cause": "blocked_quota"
        }]);
    }
    result
}

/// Two eye surfaces, one daemon, one instant: they must agree.
///
/// The daemon answers the delivery half only when asked, so a surface that
/// forgets to ask reads a calm rig while the other shouts about the same
/// backlog. `cyclops status` sent an empty params object and counted panes
/// alone; `cyclops ui` asked and counted both. Driving both against the
/// same canned daemon is the only shape of test that can see that, because
/// each surface was self-consistent.
#[test]
fn the_status_grid_and_the_stream_read_one_daemon_the_same_way() {
    let home = scratch_home("two");
    // Three connections: the status grid takes one, the plain follow takes
    // its subscription and its own status.
    serve_conns(&home, 3, move |req| match req["method"].as_str() {
        Some("status") => {
            let asked = req["params"]["open_deliveries"] == json!(true);
            (
                vec![json!({"id": req["id"], "result": answer(asked)}).to_string()],
                true,
            )
        }
        Some("events.subscribe") => (
            vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
            true,
        ),
        other => panic!("unexpected method {other:?}"),
    });

    let grid = stdout_of(&run_ui(&home, &["status"]));
    let stream = stdout_of(&run_ui(&home, &["ui", "--plain"]));

    // Both halves of the rule, counted once, said the same way on both.
    assert_eq!(
        grid.lines().next().unwrap_or_default(),
        "◉ 2 cyclops · watching main · tmux 3.6a · up 2m · 2 need attention",
        "{grid}"
    );
    assert!(
        stream
            .lines()
            .any(|l| l.starts_with("eye open · 2 need attention · ")),
        "{stream}"
    );
    // And each surface still puts a line behind every number it shows.
    assert!(grid.contains("  reviewer  ⚠ blocked_permission"), "{grid}");
    assert!(grid.contains("  implementer  ⊘ parked · quota"), "{grid}");
    assert!(stream.contains("implementer  ⊘ parked · quota"), "{stream}");
    assert!(
        stream.contains("reviewer  ⚠ blocked_permission"),
        "{stream}"
    );
    let _ = fs::remove_dir_all(&home);
}

/// A follow never ends saying a pane that closed needs a human.
///
/// The daemon's answer says %1 is blocked; the pane then leaves the table.
/// The snapshot that would drop the item is taken once, at startup, and
/// nothing re-takes it (zero polling), so the pane-removed event is the
/// only thing that can. Without it the eye stays open on a pane that no
/// longer exists for as long as the follow runs.
#[test]
fn a_pane_that_closes_takes_its_attention_item_with_it() {
    let home = scratch_home("gone");
    serve_conns(&home, 2, move |req| match req["method"].as_str() {
        Some("events.subscribe") => (
            vec![
                json!({"id": req["id"], "result": {"subscribed": true}}).to_string(),
                json!({"event": "pane-removed", "data": {"ts": 43_481_000u64,
                       "session": "main", "pane_id": "%1"}})
                .to_string(),
            ],
            true,
        ),
        Some("status") => (
            vec![json!({"id": req["id"], "result": answer(false)}).to_string()],
            true,
        ),
        other => panic!("unexpected method {other:?}"),
    });
    let stream = stdout_of(&run_ui(&home, &["ui", "--plain", "--firehose"]));
    // The event arrived and the firehose said so.
    assert!(stream.contains("%1 closed"), "{stream}");
    // And nothing needs a human, so the eye never speaks at all.
    assert!(
        !stream.lines().any(|l| l.starts_with("eye ")),
        "the eye reported a pane that is gone: {stream}"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn ui_json_is_a_usage_error_pointing_at_watch() {
    let home = scratch_home("uj");
    let out = run_ui(&home, &["ui", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "cyclops ui has no --json form. The machine stream is: cyclops watch --json"
    );
    let _ = fs::remove_dir_all(&home);
}
