//! End-to-end transport tests: a canned daemon on a scratch socket, the
//! real binary as a subprocess with CYCLOPS_HOME pointed at the scratch
//! dir. No tmux anywhere near these tests, no network, no real home.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use serde_json::{json, Value};

/// Scratch home unique per test and process, under the OS temp dir. Kept
/// short: Unix socket paths cap out around 104 bytes on macOS.
fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cyc-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch home");
    dir
}

fn hello(proto: u32) -> Value {
    json!({"cyclops": "0.1.0", "proto": proto, "boot_id": "b-e2e"})
}

/// Serve one connection: write the hello line, then answer each request
/// with the closure's lines. A true close flag hangs up after writing,
/// which is how the stream tests simulate a daemon shutdown. The thread is
/// detached; client-side assertions carry the test.
fn serve_once<F>(home: &Path, hello_line: Value, reply: F)
where
    F: FnMut(Value) -> (Vec<String>, bool) + Send + 'static,
{
    serve_conns(home, hello_line, 1, reply);
}

/// serve_once generalized to `conns` sequential connections sharing one
/// reply closure. The hook tests need it: each hook invocation is its own
/// short-lived process and connection.
fn serve_conns<F>(home: &Path, hello_line: Value, conns: usize, mut reply: F)
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
            writeln!(w, "{hello_line}").expect("write hello");
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

fn run_cyclops(home: &Path, args: &[&str]) -> Output {
    run_cyclops_io(home, &[], args, None)
}

/// run_cyclops with extra env vars and optional piped stdin. CYCLOPS_AGENT
/// is scrubbed first so the developer's shell can't leak an identity into
/// the hook tests.
fn run_cyclops_io(
    home: &Path,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cyclops"));
    cmd.env("CYCLOPS_HOME", home)
        .env_remove("CYCLOPS_AGENT")
        .args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let Some(input) = stdin else {
        return cmd.output().expect("run cyclops binary");
    };
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cyclops");
    {
        let mut si = child.stdin.take().expect("child stdin");
        si.write_all(input.as_bytes()).expect("write child stdin");
        // Dropping closes the pipe; the child sees EOF.
    }
    child.wait_with_output().expect("wait for cyclops")
}

fn canned_status() -> Value {
    json!({
        "daemon_version": "0.1.0",
        "proto": 1,
        "boot_id": "b-e2e",
        "uptime_ms": 120_000,
        "tmux_version": "3.6a",
        "sessions": [{
            "name": "main",
            "attached": true,
            "panes": [
                {
                    "pane_id": "%1", "window_id": "@1", "window_name": "agents",
                    "agent": "reviewer", "manifest": "claude",
                    "title": "Run the tests", "current_command": "claude",
                    "dead": false, "in_mode": false, "width": 120, "height": 40,
                    "state": "working"
                },
                {
                    "pane_id": "%2", "window_id": "@1", "window_name": "agents",
                    "agent": "implementer", "manifest": "claude",
                    "title": "implementer", "current_command": "claude",
                    "dead": false, "in_mode": false, "width": 120, "height": 40,
                    "state": "idle"
                },
                {
                    "pane_id": "%4", "window_id": "@1", "window_name": "agents",
                    "title": "", "current_command": "vim",
                    "dead": false, "in_mode": false, "width": 120, "height": 40,
                    "state": "unknown"
                }
            ]
        }]
    })
}

#[test]
fn status_json_prints_the_raw_result() {
    let home = scratch_home("sj");
    let canned = canned_status();
    let expected = canned.to_string();
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "status");
        (
            vec![json!({"id": req["id"], "result": canned}).to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["status", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn status_plain_renders_the_grid() {
    let home = scratch_home("sp");
    let canned = canned_status();
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": canned}).to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["status", "--plain"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = "◉ cyclops · watching main · tmux 3.6a · up 2m\n\
                    \n\
                    \x20 reviewer     ● working  Run the tests\n\
                    \x20 implementer  ○ idle\n\
                    \x20 %4           ? unknown  vim\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn daemon_not_running_copy_and_exit_code() {
    let home = scratch_home("nr");
    let out = run_cyclops(&home, &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "cyclops isn't running. Start it with: cyclopsd &"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn proto_mismatch_warns_and_continues() {
    let home = scratch_home("pm");
    let canned = canned_status();
    let expected = canned.to_string();
    serve_once(&home, hello(99), move |req| {
        (
            vec![json!({"id": req["id"], "result": canned}).to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["status", "--json"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "note: cyclopsd speaks protocol 99, this cyclops speaks 1. Continuing; update the older side."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn unknown_target_names_it_and_lists_known() {
    let home = scratch_home("ut");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "pane.read");
        assert_eq!(req["params"]["target"], "ghost");
        assert_eq!(req["params"]["source"], "visible");
        (
            vec![json!({
                "id": req["id"],
                "error": {
                    "code": "no_such_target",
                    "message": "no such target",
                    "targets": ["reviewer", "implementer"]
                }
            })
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["read", "ghost"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "no agent or pane called \"ghost\". Cyclops knows: reviewer, implementer."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn watch_json_streams_events_then_reports_the_close() {
    let home = scratch_home("wj");
    let ev1 = json!({"event": "agent.state", "data": {"agent": "reviewer", "state": "working"}});
    let ev2 = json!({"event": "agent.state", "data": {"agent": "reviewer", "state": "idle"}});
    let (l1, l2) = (ev1.to_string(), ev2.to_string());
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "events.subscribe");
        assert_eq!(req["params"]["kinds"], json!(["agent"]));
        // Ack, two events, then hang up: the client should render both
        // events and then report the closed stream.
        (
            vec![
                json!({"id": req["id"], "result": {"subscribed": true}}).to_string(),
                l1.clone(),
                l2.clone(),
            ],
            true,
        )
    });
    let out = run_cyclops(&home, &["watch", "--kinds", "agent", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<String> = stdout.lines().map(String::from).collect();
    assert_eq!(
        lines,
        vec![ev1.to_string(), ev2.to_string()],
        "child stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("lost the connection to cyclops"),
        "stderr: {stderr}"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_happy_path_stdin_body_delivers_verified() {
    let home = scratch_home("sd");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "msg.send");
        assert_eq!(req["params"]["to"], json!(["reviewer"]));
        assert_eq!(req["params"]["subject"], "Review the rate limiter");
        assert_eq!(req["params"]["body"], "please look at src/limiter.rs");
        assert_eq!(req["params"]["fyi"], false);
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-3f9c2a", "seq": 7,
                "deliveries": [{"to": "reviewer", "state": "delivered_verified"}]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops_io(
        &home,
        &[],
        &[
            "send",
            "reviewer",
            "--subject",
            "Review the rate limiter",
            "--body-file",
            "-",
        ],
        Some("please look at src/limiter.rs"),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "✓ delivered · verified\n"
    );
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_broadcast_renders_the_aligned_grid() {
    let home = scratch_home("sb");
    serve_once(&home, hello(1), move |req| {
        // Positional target merges with --to, positional first.
        assert_eq!(req["params"]["to"], json!(["reviewer", "implementer"]));
        assert_eq!(req["params"]["fyi"], true);
        assert_eq!(req["params"]["reply_to"], "m-11aa22");
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-9c0ffe", "seq": 8,
                "deliveries": [
                    {"to": "reviewer", "state": "delivered_verified"},
                    {"to": "implementer", "state": "queued", "position": 2}
                ]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops_io(
        &home,
        &[],
        &[
            "send",
            "reviewer",
            "--to",
            "implementer",
            "--subject",
            "Sync",
            "--body",
            "b",
            "--fyi",
            "--reply-to",
            "m-11aa22",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = "\x20 reviewer     ✓ delivered · verified\n\
                    \x20 implementer  ● queued · 2 ahead\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_all_targets_star_and_reports_screen_verification() {
    let home = scratch_home("sa");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["params"]["to"], json!(["*"]));
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-aa", "seq": 9,
                "deliveries": [{"to": "agy", "state": "delivered_unverified"}]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["send", "--all", "--subject", "Standup"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "✓ delivered · unverified (screen)\n"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_parked_exits_one_with_reset_hint() {
    let home = scratch_home("sk");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-bb", "seq": 10,
                "deliveries": [{
                    "to": "reviewer", "state": "parked_blocked_quota",
                    "note": "resets in 135h"
                }]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(
        &home,
        &["send", "reviewer", "--subject", "s", "--body", "b"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "⛔ parked · quota, resets in 135h\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "reviewer is out of quota, resets in 135h. The message is kept as parked; requeue it once the quota resets."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_json_passthrough_keeps_the_exit_code() {
    let home = scratch_home("sq");
    let result = json!({
        "msg_id": "m-cc", "seq": 11,
        "deliveries": [{"to": "reviewer", "state": "attention_required", "note": "target pane is gone"}]
    });
    let expected = result.to_string();
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": result}).to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["send", "reviewer", "--subject", "s", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
    assert!(
        out.stderr.is_empty(),
        "json mode stays machine-clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_unknown_recipient_names_it_and_lists_known() {
    let home = scratch_home("su");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({
                "id": req["id"],
                "error": {
                    "code": "no_such_target",
                    "message": "no such target",
                    "targets": ["reviewer", "implementer"]
                }
            })
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["send", "ghost", "--subject", "s"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "no agent or pane called \"ghost\". Cyclops knows: reviewer, implementer."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_without_recipient_is_a_usage_error() {
    let home = scratch_home("sn");
    // No daemon on purpose: usage errors must not hide behind a down one.
    let out = run_cyclops(&home, &["send", "--subject", "s"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "no recipient. Name one (cyclops send reviewer --subject \"...\"), or pass --to or --all."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn hook_posts_wellformed_reports_and_stays_silent() {
    use std::sync::{Arc, Mutex};
    let home = scratch_home("hk");
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let record = seen.clone();
    serve_conns(&home, hello(1), 2, move |req| {
        record.lock().unwrap().push(req.clone());
        (
            vec![json!({"id": req["id"], "result": {"ok": true}}).to_string()],
            true,
        )
    });
    let payload = r#"{"session_id":"s-1","transcript_path":"/private/tmp/t.jsonl"}"#;
    let out = run_cyclops_io(
        &home,
        &[("CYCLOPS_AGENT", "reviewer")],
        &["hook", "Stop"],
        Some(payload),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "hook wrote stdout");
    assert!(out.stderr.is_empty(), "hook wrote stderr");
    let out2 = run_cyclops_io(
        &home,
        &[("CYCLOPS_AGENT", "reviewer")],
        &["hook", "UserPromptSubmit"],
        Some(payload),
    );
    assert_eq!(out2.status.code(), Some(0));
    assert!(out2.stdout.is_empty() && out2.stderr.is_empty());

    // The child read its response before exiting, so by now the server
    // thread has recorded both requests.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "requests: {seen:?}");
    assert_eq!(seen[0]["method"], "agent.state.report");
    assert_eq!(seen[0]["params"]["agent"], "reviewer");
    assert_eq!(seen[0]["params"]["event"], "Stop");
    assert_eq!(seen[0]["params"]["seq"], 1);
    assert_eq!(seen[0]["params"]["payload"]["session_id"], "s-1");
    // The file counter survives across hook processes.
    assert_eq!(seen[1]["params"]["event"], "UserPromptSubmit");
    assert_eq!(seen[1]["params"]["seq"], 2);
    assert_eq!(
        fs::read_to_string(home.join("hookseq/reviewer")).unwrap(),
        "2"
    );
    // Success leaves no error log behind.
    assert!(!home.join("hook-errors.log").exists());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn hook_daemon_down_exits_zero_and_logs() {
    let home = scratch_home("hd");
    let out = run_cyclops_io(
        &home,
        &[],
        &["hook", "Stop", "--agent", "implementer"],
        Some("{}"),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "hook wrote stdout");
    assert!(out.stderr.is_empty(), "hook wrote stderr");
    let log = fs::read_to_string(home.join("hook-errors.log")).expect("hook error log");
    assert!(log.contains("hook Stop:"), "log: {log}");
    assert!(log.contains("cyclops isn't running"), "log: {log}");
    assert_eq!(log.trim().lines().count(), 1, "log: {log}");
    // --agent selected the counter file; the seq was consumed before the
    // connect failed, which keeps gaps visible downstream.
    assert_eq!(
        fs::read_to_string(home.join("hookseq/implementer")).unwrap(),
        "1"
    );

    // Without any identity the hook still exits 0 and says why in the log.
    let out = run_cyclops_io(&home, &[], &["hook", "Stop"], Some("{}"));
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    let log = fs::read_to_string(home.join("hook-errors.log")).expect("hook error log");
    assert!(log.contains("no agent identity"), "log: {log}");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn read_prints_pane_text_verbatim() {
    let home = scratch_home("rt");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "pane.read");
        assert_eq!(req["params"]["lines"], 5);
        assert_eq!(req["params"]["source"], "recent");
        (
            vec![json!({
                "id": req["id"],
                "result": {"target": "reviewer", "pane_id": "%1", "text": "line one\nline two"}
            })
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(
        &home,
        &["read", "reviewer", "--lines", "5", "--source", "recent"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "line one\nline two\n");
    let _ = fs::remove_dir_all(&home);
}
