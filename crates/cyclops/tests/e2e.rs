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
fn serve_once<F>(home: &Path, hello_line: Value, mut reply: F)
where
    F: FnMut(Value) -> (Vec<String>, bool) + Send + 'static,
{
    let listener = UnixListener::bind(home.join("sock")).expect("bind scratch socket");
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut w = stream;
        writeln!(w, "{hello_line}").expect("write hello");
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let req: Value = serde_json::from_str(line.trim()).expect("request parses");
            let (lines, close) = reply(req);
            for l in lines {
                if writeln!(w, "{l}").is_err() {
                    return;
                }
            }
            if close {
                return;
            }
        }
    });
}

fn run_cyclops(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .args(args)
        .output()
        .expect("run cyclops binary")
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
