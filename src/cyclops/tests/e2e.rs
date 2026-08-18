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

/// Scratch home unique per test and process, under the relocatable
/// scratch root. Kept short: Unix socket paths cap out around 104 bytes
/// on macOS, which is why the root is not the OS temp dir (F24). That the
/// root really relocates is proven once, in cyclopsd's scratch_override
/// test; restating it here as a starts_with could not fail.
fn scratch_home(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}"));
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
/// the hook tests. TMUX/TMUX_PANE are scrubbed for the same reason: a suite
/// run inside tmux would put every child "in tmux"; TMUX_PANE would also make
/// `cyclops list` scope to the caller's session and collide with canned
/// fixture panes (%1, %2). A test that needs a pane sets TMUX_PANE through
/// `envs`, which land after the scrub.
fn run_cyclops_io(
    home: &Path,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cyclops"));
    cmd.env("CYCLOPS_HOME", home)
        .env_remove("CYCLOPS_AGENT")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
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
        }],
        // What a real daemon answers with. %4 above is the reason it
        // matters: the grid labels that pane unknown, and only this field
        // says whether the machine has no manifests at all or three that
        // do not bind vim.
        "manifests": {"ids": ["agy", "claude", "codex"], "dir": "/h/manifests"}
    })
}

/// The block the canned answer's one unknown pane earns, under the grid.
const UNKNOWN_NOTE: &str = "\n  1 pane reads unknown: none of agy, claude, codex matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %4 <label> --manifest <id>. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md.\n";

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
    // Nothing blocked in the canned answer, so the eye stays closed.
    let expected = format!(
        "‿ cyclops · watching main · tmux 3.6a · up 2m\n\
         \n\
         \x20 reviewer     ● working  Run the tests\n\
         \x20 implementer  ○ idle\n\
         \x20 %4           ? unknown  vim\n{UNKNOWN_NOTE}"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn list_renders_the_roster_and_asks_status_for_it() {
    let home = scratch_home("lr");
    let canned = canned_status();
    serve_once(&home, hello(1), move |req| {
        // One question, not a second one: the roster is already in the
        // status answer, so a `pane.list` method would be a second place
        // for it to come from.
        assert_eq!(req["method"], "status");
        (
            vec![json!({"id": req["id"], "result": canned}).to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["list", "--plain"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The header names the watched session and the home the client asked
    // through: the roster of a second daemon on a second home reads
    // differently on its first line.
    let expected = format!(
        "watching main · home {}\n\
         \n\
         \x20 reviewer     ● working  Run the tests\n\
         \x20 implementer  ○ idle\n",
        home.display()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn list_json_carries_the_same_rows() {
    let home = scratch_home("lj");
    let canned = canned_status();
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": canned}).to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["list", "--json"]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("json output");
    let agents = v["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 2, "{v}");
    assert_eq!(agents[0]["agent"], json!("reviewer"));
    assert_eq!(agents[0]["state"], json!("working"));
    assert_eq!(agents[1]["agent"], json!("implementer"));
    // The header's facts, as additive fields: which home this client
    // asked through, and the sessions the daemon watches.
    assert_eq!(v["home"], json!(home.display().to_string()), "{v}");
    assert_eq!(v["sessions"], json!(["main"]), "{v}");
    // Nothing was elided, so nothing claims to have been.
    assert!(v.get("also_watching").is_none(), "{v}");
    let _ = fs::remove_dir_all(&home);
}

/// canned_status plus a second watched session, for the list-scoping
/// tests: the daemon watches "main" and "ops", and the caller sits in
/// one of them.
fn canned_status_two_sessions() -> Value {
    let mut v = canned_status();
    v["sessions"].as_array_mut().expect("sessions").push(json!({
        "name": "ops",
        "attached": true,
        "panes": [{
            "pane_id": "%7", "window_id": "@2", "window_name": "agents",
            "agent": "deployer", "manifest": "claude",
            "title": "", "current_command": "claude",
            "dead": false, "in_mode": false, "width": 120, "height": 40,
            "state": "idle"
        }]
    }));
    v
}

/// Inside tmux, the roster is the caller's session: the pane id tmux put
/// in the environment locates it, the other sessions' agents are elided,
/// and the header plus a dim note say exactly what happened and the way
/// out. This is the shipped surface for the "list in a fresh tab shows a
/// random session" defect.
#[test]
fn list_scopes_to_the_callers_session_inside_tmux() {
    let home = scratch_home("lsc");
    let canned = canned_status_two_sessions();
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": canned}).to_string()],
            false,
        )
    });
    // %2 is implementer's pane in "main".
    let out = run_cyclops_io(&home, &[("TMUX_PANE", "%2")], &["list", "--plain"], None);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = format!(
        "watching main · home {}\n\
         \x20 also watching ops · cyclops list --all to see every session\n\
         \n\
         \x20 reviewer     ● working  Run the tests\n\
         \x20 implementer  ○ idle\n",
        home.display()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

/// --all restores today's full dump, note-free, even inside tmux. And
/// with no TMUX_PANE or a pane the daemon does not watch, scoping never
/// engages: the full roster is byte for byte what it always was.
#[test]
fn list_all_and_unmatched_callers_keep_the_full_roster() {
    let home = scratch_home("lfa");
    let canned = canned_status_two_sessions();
    serve_conns(&home, hello(1), 3, move |req| {
        (
            vec![json!({"id": req["id"], "result": canned.clone()}).to_string()],
            true,
        )
    });
    let expected = format!(
        "watching main, ops · home {}\n\
         \n\
         \x20 reviewer     ● working  Run the tests\n\
         \x20 implementer  ○ idle\n\
         \x20 deployer     ○ idle\n",
        home.display()
    );
    // Inside tmux with --all.
    let out = run_cyclops_io(
        &home,
        &[("TMUX_PANE", "%2")],
        &["list", "--all", "--plain"],
        None,
    );
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    // Outside tmux.
    let out = run_cyclops(&home, &["list", "--plain"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    // Inside tmux, in a pane the daemon does not watch.
    let out = run_cyclops_io(&home, &[("TMUX_PANE", "%99")], &["list", "--plain"], None);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

/// --json scopes exactly as the grid does (parity, not a second shape):
/// the same elided rows, the scoped `sessions`, and the note's fact as an
/// additive `also_watching`. With --all the answer is the full one and
/// the additive field never appears.
#[test]
fn list_json_scopes_identically_and_honors_all() {
    let home = scratch_home("lsj");
    let canned = canned_status_two_sessions();
    serve_conns(&home, hello(1), 2, move |req| {
        (
            vec![json!({"id": req["id"], "result": canned.clone()}).to_string()],
            true,
        )
    });
    // %7 is deployer's pane in "ops": the smaller session, so a scoped
    // answer cannot be mistaken for a truncated full one.
    let out = run_cyclops_io(&home, &[("TMUX_PANE", "%7")], &["list", "--json"], None);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("json output");
    let agents = v["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 1, "{v}");
    assert_eq!(agents[0]["agent"], json!("deployer"));
    assert_eq!(v["sessions"], json!(["ops"]), "{v}");
    assert_eq!(v["also_watching"], json!(["main"]), "{v}");

    let out = run_cyclops_io(
        &home,
        &[("TMUX_PANE", "%7")],
        &["list", "--json", "--all"],
        None,
    );
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).expect("json output");
    assert_eq!(v["agents"].as_array().expect("agents").len(), 3, "{v}");
    assert_eq!(v["sessions"], json!(["main", "ops"]), "{v}");
    assert!(v.get("also_watching").is_none(), "{v}");
    let _ = fs::remove_dir_all(&home);
}

/// The verb the ladder starts with. What matters here is the wire: a name
/// and an optional pin go out, and `--clear` sends a null label rather
/// than the string "null" or nothing at all.
#[test]
fn name_sends_the_label_and_the_pin() {
    let home = scratch_home("nm");
    serve_conns(&home, hello(1), 2, move |req| {
        assert_eq!(req["method"], "pane.label");
        let p = req["params"].clone();
        let result = if p["label"].is_null() {
            assert_eq!(p["target"], json!("reviewer"));
            json!({"target": "reviewer", "pane_id": "%3", "label": null})
        } else {
            assert_eq!(p["target"], json!("%3"));
            assert_eq!(p["label"], json!("reviewer"));
            assert_eq!(p["manifest"], json!("claude"));
            json!({"target": "%3", "pane_id": "%3", "label": "reviewer", "manifest": "claude"})
        };
        (
            vec![json!({"id": req["id"], "result": result}).to_string()],
            false,
        )
    });
    let out = run_cyclops(
        &home,
        &["name", "%3", "reviewer", "--manifest", "claude", "--plain"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "✔ named reviewer · %3, detects as claude\n"
    );

    let out = run_cyclops(&home, &["name", "reviewer", "--clear", "--plain"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "✔ cleared · %3 is unnamed\n"
    );
    let _ = fs::remove_dir_all(&home);
}

/// A name is required unless it is being taken back, and the two cannot
/// be asked for at once. Usage mistakes never reach the daemon.
#[test]
fn name_usage_mistakes_stop_before_the_socket() {
    let home = scratch_home("nmu");
    for args in [
        vec!["name", "%3"],
        vec!["name", "%3", "reviewer", "--clear"],
        vec!["name", "%3", "--clear", "--manifest", "claude"],
    ] {
        let out = run_cyclops(&home, &args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} should be a usage error"
        );
    }
    let _ = fs::remove_dir_all(&home);
}

/// `--clear` used to promise the pane's tmux border back flat out. It is
/// the daemon that puts it back, only while it can still reach the pane,
/// and the operator's own border format is the thing at stake, so the
/// help says under what condition rather than asserting the outcome.
#[test]
fn clear_does_not_promise_a_border_it_cannot_confirm() {
    let home = scratch_home("nhelp");
    let out = run_cyclops(&home, &["name", "--help"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("when cyclopsd can still reach the pane"),
        "{text}"
    );
    // And it says what happens when it cannot: the clear fails and the
    // name stays, because that record is the only copy of the operator's
    // own border settings left once tmux is wearing cyclops's.
    assert!(text.contains("the clear fails"), "{text}");
    assert!(text.contains("the name is kept"), "{text}");
    assert!(text.contains("Run it again"), "{text}");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn status_plain_opens_the_eye_on_a_blocked_agent() {
    // The shipped surface, not just the renderer: a blocked agent is the
    // only thing that opens the mark, and it names the count in words.
    // Tags name the scratch home, so every one in this file is distinct:
    // two tests sharing a tag share a socket and race.
    let home = scratch_home("seye");
    let mut canned = canned_status();
    canned["sessions"][0]["panes"][1]["state"] = json!("blocked_permission");
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
    let expected = format!(
        "◑ 1 cyclops · watching main · tmux 3.6a · up 2m · 1 needs attention\n\
         \n\
         \x20 reviewer     ● working             Run the tests\n\
         \x20 implementer  ⚠ blocked_permission\n\
         \x20 %4           ? unknown             vim\n{UNKNOWN_NOTE}"
    );
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
        cyclops_proto::NOT_RUNNING
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn daemon_restart_without_a_daemon_says_so() {
    let home = scratch_home("rnr");
    let out = run_cyclops(&home, &["daemon", "restart"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "cyclopsd is not running.",
    );
    let _ = fs::remove_dir_all(&home);
}

/// A restart never interrupts a delivery past the paste: a not-quiet
/// quiesce answer refuses the whole verb, names what is moving, and no
/// stop is attempted (the canned server would have seen its request).
#[test]
fn daemon_restart_refuses_while_mid_flight() {
    let home = scratch_home("rmf");
    serve_once(&home, hello(1), |req| {
        assert_eq!(req["method"], "daemon.quiesce", "{req}");
        (
            vec![json!({
                "id": req["id"],
                "result": {"quiet": false, "in_flight": ["m-abc123 -> codex"]},
            })
            .to_string()],
            true,
        )
    });
    let out = run_cyclops(&home, &["daemon", "restart"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("m-abc123 -> codex"), "{err}");
    assert!(err.contains("Nothing was restarted"), "{err}");
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
        "✔ delivered · verified\n"
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
    let expected = "\x20 reviewer     ✔ delivered · verified\n\
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
        "⊘ parked · quota, resets in 135h\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "reviewer is out of quota, resets in 135h. The message is kept as parked; requeue it once the quota resets."
    );
    let _ = fs::remove_dir_all(&home);
}

/// A send to a pane nothing detects, in the shape the daemon answers
/// with: the gate's machine cause plus the pane as data. The badge words
/// the cause and names the pane, the follow-up says the message did not
/// arrive and carries the command that fixes it, and the exit code keeps a
/// script from branching on it as a delivery.
#[test]
fn send_to_an_undetected_pane_says_it_did_not_arrive_and_exits_one() {
    let home = scratch_home("sund");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-ee", "seq": 14,
                "deliveries": [{
                    "to": "worker", "state": "attention_required",
                    "note": "no_manifest", "pane": "%1"
                }]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["send", "worker", "--subject", "hello"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "⚠ needs attention · nothing detects %1\n"
    );
    // Pasteable: the pane as the target, the name it already answers to as
    // the label. Passing the label as the target would rename the pane.
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "worker did not get this message; it is on the record and needs attention. Teach cyclops what runs in %1: cyclops name %1 worker --manifest <id>. cyclops status names the manifests that are loaded, and docs/reference/MANIFESTS.md is how to write one."
    );
    let _ = fs::remove_dir_all(&home);
}

/// The same refusal reaching a client through a daemon that predates the
/// pane field. The badge still words the cause, and the follow-up drops
/// the command rather than printing one with a hole in it.
#[test]
fn an_older_daemon_without_the_pane_field_still_gets_worded_copy() {
    let home = scratch_home("sold");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-e2", "seq": 16,
                "deliveries": [{
                    "to": "worker", "state": "attention_required",
                    "note": "no_manifest"
                }]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["send", "worker", "--subject", "hello"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "⚠ needs attention · nothing detects its pane\n"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("--manifest"),
        "offered a command it could not fill in: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

/// A receipt taken while the payload is already in the pane. It is not
/// queued, it does not claim to be delivered, and the follow-up says where
/// the badge turns up.
#[test]
fn send_in_flight_reports_the_state_it_is_in_not_queued() {
    let home = scratch_home("si");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-ff", "seq": 15,
                "deliveries": [{"to": "worker", "state": "submitted"}]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["send", "worker", "--subject", "hello"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "● submitted\n");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "the message is in worker's pane; cyclops is still waiting for the confirmation. It lands on the record either way: cyclops history shows the badge."
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

// ---------------------------------------------------------------------------
// M2 read side: history and thread
// ---------------------------------------------------------------------------

/// Canned folded msg line as msg.history/msg.thread return them. Callers
/// that need a body set it on the returned value.
fn canned_msg(
    id: &str,
    ts: u64,
    kind: &str,
    from: &str,
    to: Value,
    subject: &str,
    deliveries: Value,
) -> Value {
    json!({
        "seq": 3, "boot_id": "b-e2e", "id": id, "ts": ts, "kind": kind,
        "from": from, "to": to, "subject": subject, "deliveries": deliveries
    })
}

#[test]
fn history_plain_renders_the_grid_with_broadcast_badges() {
    let home = scratch_home("hp");
    let lines = json!([
        canned_msg(
            "m-aaa",
            1_753_000_000_000u64,
            "msg",
            "codex",
            json!(["reviewer"]),
            "Review the rate limiter",
            json!([{"to": "reviewer", "state": "delivered_verified", "verified_by": "hook", "attempts": 1, "ts": 1_753_000_001_000u64}])
        ),
        canned_msg(
            "m-bbb",
            1_753_172_800_000u64,
            "fyi",
            "admin",
            json!(["reviewer", "implementer"]),
            "Standup",
            json!([
                {"to": "reviewer", "state": "delivered_verified", "verified_by": "hook", "attempts": 1, "ts": 1_753_172_801_000u64},
                {"to": "implementer", "state": "parked_blocked_quota", "attempts": 0, "ts": 1_753_172_801_000u64, "cause": "blocked_quota"}
            ])
        ),
    ]);
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "msg.history");
        assert_eq!(req["params"]["with"], "reviewer");
        assert_eq!(req["params"]["limit"], 50);
        (
            vec![
                json!({"id": req["id"], "result": {"lines": lines, "next_cursor": 9}}).to_string(),
            ],
            false,
        )
    });
    let out = run_cyclops(&home, &["history", "--with", "reviewer"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Fixed 2025 timestamps render date gutters (relative gutters would
    // depend on the wall clock); the broadcast is one fact, two badges.
    let expected = "\x20 Jul 20 2025  codex → reviewer       Review the rate limiter  ✔ delivered · verified\n\
                    \x20 Jul 22 2025  admin → 2 agents  fyi  Standup\n\
                    \x20              reviewer     ✔ delivered · verified\n\
                    \x20              implementer  ⊘ parked · quota\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn history_json_passthrough_and_cursor_params() {
    let home = scratch_home("hj");
    let result = json!({"lines": [], "next_cursor": 42});
    let expected = result.to_string();
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["params"]["from"], "codex");
        assert_eq!(req["params"]["to"], "me");
        assert_eq!(req["params"]["limit"], 5);
        assert_eq!(req["params"]["cursor"], 7);
        assert!(req["params"].get("with").is_none());
        (
            vec![json!({"id": req["id"], "result": result}).to_string()],
            false,
        )
    });
    let out = run_cyclops(
        &home,
        &[
            "history", "--from", "codex", "--to", "me", "--limit", "5", "--cursor", "7", "--json",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn history_empty_states_invite_a_send() {
    let home = scratch_home("he");
    serve_conns(&home, hello(1), 2, move |req| {
        (
            vec![json!({"id": req["id"], "result": {"lines": []}}).to_string()],
            true,
        )
    });
    let out = run_cyclops(&home, &["history"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "No messages yet. Send one: cyclops send <target> --subject ..."
    );
    let out = run_cyclops(&home, &["history", "--with", "reviewer"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "No messages with reviewer yet. Send one: cyclops send reviewer --subject ..."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn history_with_conflicts_with_from_to() {
    let home = scratch_home("hc");
    // Usage error straight from clap; the daemon is never contacted.
    let out = run_cyclops(&home, &["history", "--with", "a", "--from", "b"]);
    assert_eq!(out.status.code(), Some(2));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn thread_plain_renders_messages_with_bodies() {
    let home = scratch_home("tp");
    // State lines ride along in the result; the human view keeps messages
    // and badges only.
    let state_line = json!({
        "seq": 5, "boot_id": "b-e2e", "id": "m-aaa", "ts": 1_753_000_001_000u64,
        "kind": "state", "from": "cyclopsd", "to": ["reviewer"],
        "deliveries": [{"to": "reviewer", "state": "gating", "attempts": 0, "ts": 1_753_000_001_000u64}],
        "data": {"to": "reviewer", "from": "queued", "to_state": "gating"}
    });
    let mut ask = canned_msg(
        "m-aaa",
        1_753_000_000_000u64,
        "msg",
        "codex",
        json!(["reviewer"]),
        "Review the rate limiter",
        json!([{"to": "reviewer", "state": "delivered_verified", "verified_by": "hook", "attempts": 1, "ts": 1_753_000_002_000u64}]),
    );
    ask["body"] = json!("gateway.rs:120");
    let mut reply = canned_msg(
        "m-ccc",
        1_753_172_800_000u64,
        "msg",
        "reviewer",
        json!(["codex"]),
        "Re: Review the rate limiter",
        json!([{"to": "codex", "state": "delivered_verified", "verified_by": "hook", "attempts": 1, "ts": 1_753_172_801_000u64}]),
    );
    reply["body"] = json!("Done.");
    let lines = json!([ask, state_line, reply]);
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "msg.thread");
        assert_eq!(req["params"]["id"], "m-aaa");
        (
            vec![json!({"id": req["id"], "result": {"lines": lines}}).to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["thread", "m-aaa"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = "\x20 Jul 20 2025  codex → reviewer  Review the rate limiter      ✔ delivered · verified\n\
                    \x20              gateway.rs:120\n\
                    \n\
                    \x20 Jul 22 2025  reviewer → codex  Re: Review the rate limiter  ✔ delivered · verified\n\
                    \x20              Done.\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn thread_unknown_id_passes_the_daemon_copy_through() {
    let home = scratch_home("tu");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({
                "id": req["id"],
                "error": {
                    "code": "no_such_message",
                    "message": "no message \"m-nope\" in the record. Run cyclops history to see what's there."
                }
            })
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["thread", "m-nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "no message \"m-nope\" in the record. Run cyclops history to see what's there."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn wait_reached_renders_the_badge_and_exits_zero() {
    let home = scratch_home("wr");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "agent.wait");
        assert_eq!(req["params"]["target"], "reviewer");
        assert_eq!(req["params"]["until"], "idle");
        assert_eq!(req["params"]["timeout_ms"], 90_000);
        (
            vec![json!({"id": req["id"], "result": {
                "target": "reviewer", "pane_id": "%1", "until": "idle",
                "state": "idle", "waited_ms": 3000
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(
        &home,
        &["wait", "reviewer", "--until", "idle", "--timeout", "90s"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "○ idle · waited 3s\n");
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn wait_timeout_exits_two_with_copy() {
    let home = scratch_home("wt");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["params"]["until"], "done");
        // Default --timeout is 60s.
        assert_eq!(req["params"]["timeout_ms"], 60_000);
        (
            vec![json!({"id": req["id"], "error": {
                "code": "timeout",
                "message": "reviewer did not reach done within 60000ms; state was working",
                "data": {"target": "reviewer", "until": "done", "state": "working", "waited_ms": 60_001}
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["wait", "reviewer", "--until", "done"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "reviewer didn't reach done within 60 seconds. Last state: working. Give it more time with --timeout, or look in with cyclops status."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn wait_occupant_changed_exits_three_with_copy() {
    let home = scratch_home("wo");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "error": {
                "code": "occupant_changed",
                "message": "the pane behind reviewer died or changed occupant while waiting",
                "data": {"target": "reviewer", "until": "done", "state": "dead", "waited_ms": 1200}
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["wait", "reviewer", "--until", "done"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "reviewer's pane died or changed occupant while waiting, so the wait can't answer for the agent you asked about. Check cyclops status and relabel the pane if a new agent owns it."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn wait_json_prints_the_error_object_and_keeps_the_exit_code() {
    let home = scratch_home("wjs");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "error": {
                "code": "timeout",
                "message": "words",
                "data": {"state": "working", "waited_ms": 500}
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(
        &home,
        &[
            "wait",
            "reviewer",
            "--until",
            "idle",
            "--timeout",
            "500ms",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("json error object");
    assert_eq!(v["code"], "timeout");
    assert_eq!(v["data"]["state"], "working");
    assert!(
        out.stderr.is_empty(),
        "json mode stays machine-clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn wait_bad_duration_is_a_usage_error() {
    let home = scratch_home("wb");
    // No daemon on purpose: usage errors must not hide behind a down one.
    let out = run_cyclops(
        &home,
        &["wait", "reviewer", "--until", "idle", "--timeout", "soon"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "can't read \"soon\" as a duration. Use forms like 90s, 2m, 1m30s, or 500ms."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn send_wait_passes_the_spec_and_renders_the_outcome() {
    let home = scratch_home("sw");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "msg.send");
        assert_eq!(req["params"]["wait"]["until"], "done");
        assert_eq!(req["params"]["wait"]["timeout_ms"], 120_000);
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-dd", "seq": 12,
                "deliveries": [{"to": "reviewer", "state": "delivered_verified"}],
                "wait": [{
                    "to": "reviewer", "outcome": "reached", "state": "idle",
                    "waited_ms": 42_000, "delivery": "delivered_verified"
                }]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(
        &home,
        &[
            "send",
            "reviewer",
            "--subject",
            "go",
            "--body",
            "b",
            "--wait",
            "done",
            "--timeout",
            "2m",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "✔ delivered · verified\nwait: ○ idle · waited 42s\n"
    );
    let _ = fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// M2 hooks verify/selftest rendering: canned daemon, exact copy
// ---------------------------------------------------------------------------

#[test]
fn hooks_selftest_ack_renders_the_badge_voice() {
    let home = scratch_home("sta");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "hooks.selftest");
        assert_eq!(req["params"]["target"], "reviewer");
        (
            vec![json!({"id": req["id"], "result": {
                "target": "reviewer", "msg_id": "m-77aa88", "manifest": "claude",
                "tier": 1, "state": "delivered_verified", "hook_ack": true,
                "waited_ms": 12
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["hooks", "selftest", "reviewer"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "reviewer · ✔ ack hook fired with the marker · ✔ delivered · verified · m-77aa88\n"
    );
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn hooks_selftest_failure_names_a_runnable_install_command() {
    let home = scratch_home("stf");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "target": "reviewer", "msg_id": "m-9b1c2d", "manifest": "codex",
                "tier": 1, "state": "delivered_unverified", "hook_ack": false,
                "waited_ms": 1800
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["hooks", "selftest", "reviewer"]);
    assert_eq!(out.status.code(), Some(1));
    // The wire state renders as the send badge, never raw snake_case.
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "reviewer · ⚠ no hook ack · ✓ delivered · unverified (screen) · m-9b1c2d\n"
    );
    // install takes the CLI kind (the bound manifest), --agent the label:
    // this command runs as printed.
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "The ack hook never reported the marker. Its config is probably not loaded; \
         cyclops hooks install codex --agent reviewer prints the wiring and the trust caveats."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn hooks_selftest_screen_tier_keeps_the_delivery_state_answer() {
    let home = scratch_home("sts");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "target": "agy", "msg_id": "m-40e5f6", "manifest": "agy",
                "tier": 2, "state": "delivered_unverified", "hook_ack": false,
                "waited_ms": 900
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["hooks", "selftest", "agy"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "agy · ⚠ no hook ack · ✓ delivered · unverified (screen) · m-40e5f6\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "agy has no payload-matchable ack hook (screen tier); a hook ack can \
         never confirm it. The delivery state above is the whole answer."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn hooks_verify_unlabeled_pane_names_the_label_step() {
    let home = scratch_home("vul");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "hooks.verify");
        assert_eq!(req["params"]["target"], "%4");
        // Manifest bound, hooks declared, but no hooks_verified bit: the
        // pane is unadopted, so edge tracking has not started.
        (
            vec![json!({"id": req["id"], "result": {
                "target": "%4", "pane_id": "%4", "manifest": "claude", "tier": 1,
                "events": [
                    {"event": "UserPromptSubmit"},
                    {"event": "Stop"}
                ]
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["hooks", "verify", "%4"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = "%4 · tier 1 · hook tracking starts when the pane has a label\n\
                    \n\
                    \x20 UserPromptSubmit  never\n\
                    \x20 Stop              never\n";
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "%4 declares hooks but has no label, and hook edges only count for a \
         labeled pane. Name the pane (cyclops status shows every pane and its \
         label), then rerun cyclops hooks verify."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn hooks_verify_without_manifest_reads_no_hooks_declared() {
    let home = scratch_home("vnm");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "target": "%4", "pane_id": "%4", "tier": 2, "events": []
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["hooks", "verify", "%4"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "%4 · tier 2 · no hooks declared\n"
    );
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

/// A name nobody could address is a usage error, so it must arrive
/// without a daemon in the way. This is the rule the dispatch comment in
/// main.rs states, and `cyclops name` broke it when the check moved.
///
/// The wording lives in cyclops_proto::label and is tested there; what
/// this pins is that the CLI reaches it, exits 2, and never opens a
/// socket to find out.
#[test]
fn a_reserved_name_is_refused_without_a_daemon() {
    let home = scratch_home("name-reserved");
    for reserved in ["admin", "*", "%9"] {
        let out = run_cyclops(&home, &["name", "%0", reserved, "--plain"]);
        assert_eq!(out.status.code(), Some(2), "{reserved} must be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&cyclops_proto::label::refusal(reserved).expect("refused")),
            "{reserved}: {err}"
        );
        // The connect error would name the daemon. Seeing it here means
        // the refusal happened too late to be useful.
        assert!(
            !err.contains("cyclopsd"),
            "{reserved} asked the daemon: {err}"
        );
    }
    let _ = fs::remove_dir_all(&home);
}

/// `--self` names the pane the command runs in, which is how an agent
/// registers itself on startup without being told its own pane id.
#[test]
fn self_names_the_calling_pane_and_says_so_when_there_is_none() {
    let home = scratch_home("name-self");

    // Outside tmux there is no pane to name. The refusal carries the way
    // to do it anyway, with the name the operator already typed.
    let out = run_cyclops_io(&home, &[], &["name", "picked", "--self", "--plain"], None);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not in one"), "{err}");
    assert!(err.contains("cyclops name %0 picked"), "{err}");

    // Inside one, the pane id comes from the environment tmux sets, and
    // the positional is the name. With no daemon this gets as far as the
    // connection and no further, which is enough to prove the argument
    // was read as a name rather than as a target.
    let out = run_cyclops_io(
        &home,
        &[("TMUX_PANE", "%7")],
        &["name", "picked", "--self", "--plain"],
        None,
    );
    assert_ne!(
        out.status.code(),
        Some(2),
        "--self inside tmux is not a usage error"
    );

    // And a reserved name is still refused through --self.
    let out = run_cyclops_io(
        &home,
        &[("TMUX_PANE", "%7")],
        &["name", "admin", "--self", "--plain"],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is you"));

    let _ = fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// cyclops update: the refusals that need no clone and no network
// ---------------------------------------------------------------------------

/// The update's own output is the installer's stream, so --json is a
/// usage error that points at the machine-readable alternative. No
/// daemon, no network, and nothing on disk moves.
#[test]
fn update_json_is_refused_with_the_alternative_named() {
    let home = scratch_home("uj");
    let out = run_cyclops(&home, &["update", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no --json form"), "{err}");
    assert!(err.contains("cyclops --version"), "{err}");
    let _ = fs::remove_dir_all(&home);
}

/// A source that cannot be read fails with the repo named and exit 1,
/// before anything is installed. The env overrides are the installer's
/// own, so pointing them at a hole is the cheap way to prove the update
/// never guesses past a dead source. Which sentence appears depends on
/// this build's own ref (a clean sha fails at ls-remote, a .dirty or
/// unknown one at the clone), and both name the repo and the ref.
#[test]
fn update_with_an_unreachable_source_names_it_and_exits_one() {
    let home = scratch_home("uu");
    let out = run_cyclops_io(
        &home,
        &[
            ("CYCLOPS_REPO", "/nonexistent-cyclops-update-source"),
            ("CYCLOPS_REF", "main"),
        ],
        &["update", "--plain"],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The running build and the source were said before the failure.
    assert!(stdout.contains("cyclops "), "{stdout}");
    assert!(
        stdout.contains("source /nonexistent-cyclops-update-source at main"),
        "{stdout}"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("/nonexistent-cyclops-update-source"), "{err}");
    assert!(err.contains("main"), "{err}");
    let _ = fs::remove_dir_all(&home);
}

/// `cyclops update` builds into a directory that outlives the clone.
///
/// Without this the clone is a fresh temp dir every run, so cargo starts
/// with an empty target/, rebuilds the whole dependency tree for a one
/// commit change, and then `Scratch` deletes the result on the way out. The
/// next update starts cold again. install.sh has always read
/// CARGO_TARGET_DIR; nothing set it.
///
/// Driven through a stub repo whose installer only reports its environment,
/// because the real one is a release build of the world and this is a
/// question about one variable.
#[test]
fn update_builds_into_a_cache_that_outlives_the_clone() {
    let home = scratch_home("cyc-update-cache");
    let repo = home.join("fake-repo");
    std::fs::create_dir_all(repo.join("scripts")).expect("stub repo");
    std::fs::write(
        repo.join("scripts/install.sh"),
        "echo \"SAW ${CARGO_TARGET_DIR:-<unset>}\"\n",
    )
    .expect("stub installer");
    for args in [
        &["init", "-q", "."][..],
        &["add", "-A"][..],
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ][..],
    ] {
        let ok = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .status()
            .expect("git")
            .success();
        assert!(ok, "stub repo setup failed at {args:?}");
    }

    let cache = home.join("build-cache");
    let out = run_cyclops_io(
        &home,
        &[
            ("CYCLOPS_REPO", repo.to_str().expect("utf8")),
            ("CYCLOPS_REF", "HEAD"),
        ],
        &["update", "--plain"],
        None,
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(&format!("SAW {}", cache.display())),
        "the installer should build into the cache:\n{text}"
    );
    // Named, because a gigabyte-scale directory the operator never asked
    // for is one they should be told about rather than find.
    assert!(text.contains("building in"), "{text}");
    // And it is still there once the clone is gone, which is the point.
    assert!(cache.is_dir(), "the cache must outlive the clone");

    // An operator who already chose a target dir keeps it.
    let mine = home.join("mine");
    let out = run_cyclops_io(
        &home,
        &[
            ("CYCLOPS_REPO", repo.to_str().expect("utf8")),
            ("CYCLOPS_REF", "HEAD"),
            ("CARGO_TARGET_DIR", mine.to_str().expect("utf8")),
        ],
        &["update", "--plain"],
        None,
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains(&format!("SAW {}", mine.display())),
        "an explicit CARGO_TARGET_DIR must win:\n{text}"
    );
}
