//! End-to-end transport tests: a canned daemon on a scratch socket, the
//! real binary as a subprocess with CYCLOPS_HOME pointed at the scratch
//! dir. No tmux anywhere near these tests, no network, no real home.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use serde_json::{json, Value};

const GEMINI_ENDPOINT: &str =
    "agent:00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/%9";

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
    json!({
        "cyclops": "0.1.0",
        "build": env!("CYCLOPS_BUILD_REF"),
        "proto": proto,
        "boot_id": "b-e2e"
    })
}

/// Kernel start time of a pid, or None when the process is gone.
#[cfg(target_os = "macos")]
fn birth_of(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if rc != size {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(target_os = "linux")]
fn birth_of(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn birth_of(_pid: u32) -> Option<u64> {
    None
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

/// Copy the built client into an isolated executable pair.
///
/// The daemon stand-in is never started by the restart refusal test. It only
/// provides the executable identity that the client authenticates.
fn paired_client(directory: &Path) -> (PathBuf, PathBuf) {
    fs::create_dir(directory).unwrap();
    let source = Path::new(env!("CARGO_BIN_EXE_cyclops"));
    let client = directory.join("cyclops");
    let daemon = directory.join("cyclopsd");
    for destination in [&client, &daemon] {
        fs::copy(source, destination).unwrap();
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
    }
    (
        fs::canonicalize(client).unwrap(),
        fs::canonicalize(daemon).unwrap(),
    )
}

fn assert_json_failure(out: &Output, exit: i32, expected: Value) {
    assert_eq!(out.status.code(), Some(exit));
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout: {stdout}");
    let actual: Value = serde_json::from_str(lines[0]).expect("one JSON failure object");
    assert_eq!(actual, expected);
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
    run_cyclops_binary_io(
        Path::new(env!("CARGO_BIN_EXE_cyclops")),
        home,
        envs,
        args,
        stdin,
    )
}

fn run_cyclops_binary_io(
    binary: &Path,
    home: &Path,
    envs: &[(&str, &str)],
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    let mut cmd = Command::new(binary);
    cmd.env("CYCLOPS_HOME", home)
        .env_remove("CYCLOPS_AGENT")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        // Installer cache tests choose their own target directory.
        .env_remove("CARGO_TARGET_DIR")
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
        "daemon_build": env!("CYCLOPS_BUILD_REF"),
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
        "mailbox_routes": [
            {
                "recipient": {
                    "kind": "admin",
                    "workspace_id": "00000000-0000-0000-0000-000000000001"
                },
                "label": "admin"
            },
            {
                "recipient": {
                    "kind": "agent",
                    "workspace_id": "00000000-0000-0000-0000-000000000001",
                    "session_instance_id": "00000000-0000-0000-0000-000000000002",
                    "pane_id": "%1"
                },
                "label": "reviewer"
            },
            {
                "recipient": {
                    "kind": "agent",
                    "workspace_id": "00000000-0000-0000-0000-000000000001",
                    "session_instance_id": "00000000-0000-0000-0000-000000000002",
                    "pane_id": "%2"
                },
                "label": "implementer"
            },
            {
                "recipient": {
                    "kind": "agent",
                    "workspace_id": "00000000-0000-0000-0000-000000000001",
                    "session_instance_id": "00000000-0000-0000-0000-000000000002",
                    "pane_id": "%4"
                },
                "label": "%4"
            }
        ],
        // What a real daemon answers with. %4 above is the reason it
        // matters: the grid labels that pane unknown, and only this field
        // says whether the machine has no manifests at all or three that
        // do not bind vim.
        "manifests": {"ids": ["agy", "claude", "codex"], "dir": "/h/manifests"}
    })
}

/// The block the canned answer's one unknown pane earns, under the grid.
const UNKNOWN_NOTE: &str = "\n  %4 reads unknown, and this daemon gave no reason\n  Update cyclops so the daemon reports one, or inspect the pane with: cyclops read <pane> --source detection\n";

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

/// A new tmux pane reaches its first shell command before the daemon's watcher
/// necessarily publishes it. `name --self` waits through that one bounded
/// discovery race without retrying any other naming failure.
#[test]
fn self_name_retries_a_new_pane_until_the_watcher_publishes_it() {
    let home = scratch_home("name-self-discovery");
    serve_conns(&home, hello(1), 1, move |req| {
        assert_eq!(req["method"], "pane.label");
        assert_eq!(req["params"]["target"], json!("%18"));
        assert_eq!(req["params"]["label"], json!("codey-research"));
        let answer = if req["id"] == json!(1) {
            json!({
                "id": req["id"],
                "error": {"code": "no_such_target", "message": "pane is not published yet"}
            })
        } else {
            json!({
                "id": req["id"],
                "result": {"target": "%18", "pane_id": "%18", "label": "codey-research"}
            })
        };
        (vec![answer.to_string()], false)
    });

    let out = run_cyclops_io(
        &home,
        &[("TMUX_PANE", "%18")],
        &["name", "codey-research", "--self", "--plain"],
        None,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "✔ named codey-research · %18\n"
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

/// The bootstrap crossing, seen in the field: the daemon still running is
/// the build being replaced, so it answers unknown_method to the restart
/// handshake. That is not an error to retry — it is an old daemon — so
/// the copy says which pair of commands crosses instead of leaving a raw
/// protocol code on screen.
#[test]
fn daemon_restart_against_an_older_daemon_names_the_one_time_fix() {
    let home = scratch_home("rold");
    serve_once(&home, hello(1), |req| {
        assert_eq!(req["method"], "daemon.quiesce", "{req}");
        (
            vec![json!({
                "id": req["id"],
                "error": {"code": "unknown_method", "message": "unknown method \"daemon.quiesce\""},
            })
            .to_string()],
            true,
        )
    });
    let out = run_cyclops(&home, &["daemon", "restart"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("predates this feature"), "{err}");
    assert!(err.contains("cyclops daemon stop"), "{err}");
    // The raw wire code is a diagnostic, not an instruction.
    assert!(!err.contains("unknown_method"), "{err}");
    let _ = fs::remove_dir_all(&home);
}

/// A restart never interrupts a delivery past the paste. A refused shutdown
/// names what is moving and does not signal the authenticated daemon.
#[test]
fn daemon_restart_refuses_while_mid_flight() {
    let home = scratch_home("rmf");
    let (client, daemon) = paired_client(&home.join("bin"));
    let pid = std::process::id();
    let birth = birth_of(pid).expect("observe test process generation");
    let process = json!({"pid": pid, "birth": birth});
    let daemon_executable = daemon.display().to_string();
    let hello_line = json!({
        "cyclops": "0.1.0",
        "build": env!("CYCLOPS_BUILD_REF"),
        "daemon_process": process.clone(),
        "daemon_executable": daemon_executable.clone(),
        "proto": 1,
        "boot_id": "b-e2e"
    });
    let mut status = canned_status();
    status["daemon_process"] = process.clone();
    status["daemon_executable"] = json!(daemon_executable);
    serve_once(&home, hello_line, move |req| match req["method"].as_str() {
        Some("status") => (
            vec![json!({"id": req["id"], "result": status.clone()}).to_string()],
            false,
        ),
        Some("daemon.shutdown") => {
            assert_eq!(req["params"]["daemon_process"], process, "{req}");
            assert_eq!(req["params"]["boot_id"], "b-e2e", "{req}");
            (
                vec![json!({
                    "id": req["id"],
                    "result": {
                        "stopping": false,
                        "in_flight": ["m-abc123 -> codex"]
                    },
                })
                .to_string()],
                true,
            )
        }
        other => panic!("unexpected method {other:?}"),
    });
    let out = run_cyclops_binary_io(&client, &home, &[], &["daemon", "restart"], None);
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
fn build_mismatch_is_reported_by_health_and_machine_readable_status() {
    let home = scratch_home("bm");
    let mut canned = canned_status();
    canned["daemon_build"] = json!("shadowed-build");
    let expected = canned.clone();
    serve_conns(
        &home,
        json!({
            "cyclops": "0.1.0",
            "build": "shadowed-build",
            "proto": 1,
            "boot_id": "b-shadowed"
        }),
        2,
        move |req| {
            let result = match req["method"].as_str() {
                Some("ping") => json!({"pong": true, "ts": 1}),
                Some("status") => canned.clone(),
                other => panic!("unexpected method: {other:?}"),
            };
            (
                vec![json!({"id": req["id"], "result": result}).to_string()],
                false,
            )
        },
    );

    let note = format!(
        "note: cyclopsd is build shadowed-build, this cyclops is build {}. Continuing; run cyclops daemon restart to load the installed daemon build.",
        env!("CYCLOPS_BUILD_REF")
    );
    let ping = run_cyclops(&home, &["ping", "--json"]);
    assert!(ping.status.success());
    assert_eq!(String::from_utf8_lossy(&ping.stderr).trim(), note);

    let out = run_cyclops(&home, &["status", "--json"]);
    assert!(out.status.success());
    let status: Value = serde_json::from_slice(&out.stdout).expect("status remains JSON");
    assert_eq!(status, expected);
    assert_eq!(status["daemon_build"], "shadowed-build");
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), note);
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
fn watch_rejects_every_unknown_display_alias_before_it_waits() {
    let home = scratch_home("wuf");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "status");
        (
            vec![json!({"id": req["id"], "result": canned_status()}).to_string()],
            true,
        )
    });

    let out = run_cyclops(
        &home,
        &["watch", "--from", "gemini", "--to", "me", "--plain"],
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown active display labels \"gemini\", \"me\""),
        "{stderr}"
    );
    assert!(stderr.contains("cyclops list --all"), "{stderr}");
    assert!(
        stderr.contains("cyclops inbox next --timeout 30s"),
        "{stderr}"
    );
    assert!(stderr.contains("renaming"), "{stderr}");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn deprecated_ui_rejects_the_same_unknown_display_alias() {
    let home = scratch_home("uuf");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "status");
        (
            vec![json!({"id": req["id"], "result": canned_status()}).to_string()],
            true,
        )
    });

    let out = run_cyclops(&home, &["ui", "--from", "gemini", "--plain"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cyclops ui is deprecated"), "{stderr}");
    assert!(
        stderr.contains("unknown active display label \"gemini\""),
        "{stderr}"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn watch_json_refuses_tui_only_display_filters_as_json() {
    let home = scratch_home("wjf");

    let out = run_cyclops(&home, &["watch", "--from", "ghost", "--json"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let value: Value = serde_json::from_slice(&out.stdout).expect("JSON usage error");
    assert_eq!(value["code"], "unsupported_watch_filter");
    assert!(value["message"].as_str().unwrap().contains("--from"));
    assert!(value["message"].as_str().unwrap().contains("--json"));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_subscribes_before_listing_and_claims_after_one_event() {
    let home = scratch_home("inx");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => {
                assert_eq!(req["method"], "events.subscribe");
                assert_eq!(req["params"]["kinds"], json!(["messages.changed"]));
                (
                    vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                    false,
                )
            }
            2 => {
                assert_eq!(req["method"], "inbox.list");
                assert_eq!(req["params"]["limit"], 1);
                assert_eq!(
                    req["params"]["sender"],
                    json!({
                        "kind": "agent",
                        "workspace_id": "00000000-0000-4000-8000-000000000001",
                        "session_instance_id": "00000000-0000-4000-8000-000000000002",
                        "pane_id": "%9"
                    })
                );
                let changed = json!({
                    "event": "messages.changed",
                    "data": {
                        "workspace_id": "00000000-0000-0000-0000-000000000001",
                        "workspace_seq": 8,
                        "changed": ["mailboxes"]
                    },
                    "seq": 8
                })
                .to_string();
                (
                    vec![
                        json!({"id": req["id"], "result": {"entries": []}}).to_string(),
                        changed.clone(),
                        changed.clone(),
                        changed,
                    ],
                    false,
                )
            }
            3 => {
                assert_eq!(req["method"], "inbox.list");
                (
                    vec![json!({"id": req["id"], "result": {"entries": [{
                        "message_id": "m-live-use",
                        "sender": {
                            "kind": "agent",
                            "workspace_id": "00000000-0000-4000-8000-000000000001",
                            "session_instance_id": "00000000-0000-4000-8000-000000000002",
                            "pane_id": "%9"
                        },
                        "sender_label": "gemini-test",
                        "subject": "Startup retrospective",
                        "ts": 8,
                        "thread_root": "m-live-use"
                    }]}})
                    .to_string()],
                    false,
                )
            }
            4 => {
                assert_eq!(req["method"], "inbox.claim");
                assert_eq!(req["params"]["message_id"], "m-live-use");
                (
                    vec![json!({"id": req["id"], "result": {
                        "disposition": "claimed",
                        "message": {
                            "message_id": "m-live-use",
                            "kind": "msg",
                            "sender": {
                                "kind": "agent",
                                "workspace_id": "00000000-0000-4000-8000-000000000001",
                                "session_instance_id": "00000000-0000-4000-8000-000000000002",
                                "pane_id": "%9"
                            },
                            "sender_label": "gemini-test",
                            "subject": "Startup retrospective",
                            "body": "The socket path breaks the circular wait.",
                            "thread_root": "m-live-use"
                        }
                    }})
                    .to_string()],
                    true,
                )
            }
            _ => panic!("unexpected request {req}"),
        }
    });

    let out = run_cyclops(
        &home,
        &[
            "inbox",
            "next",
            "--from",
            GEMINI_ENDPOINT,
            "--timeout",
            "1s",
            "--plain",
        ],
    );

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "[cyclops m-live-use] FROM: gemini-test  SUBJECT: Startup retrospective\n\
         The socket path breaks the circular wait.\n\
         Reply: cyclops reply m-live-use --body \"...\"\n"
    );
    assert!(out.stderr.is_empty());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_relists_when_the_selected_message_was_superseded() {
    let home = scratch_home("ins");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => (
                vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                false,
            ),
            2 => (
                vec![json!({"id": req["id"], "result": {"entries": [{
                    "message_id": "m-old",
                    "sender": {
                        "kind": "agent",
                        "workspace_id": "00000000-0000-4000-8000-000000000001",
                        "session_instance_id": "00000000-0000-4000-8000-000000000002",
                        "pane_id": "%9"
                    },
                    "sender_label": "gemini-test",
                    "subject": "Old request",
                    "ts": 1,
                    "thread_root": "m-old"
                }]}})
                .to_string()],
                false,
            ),
            3 => {
                assert_eq!(req["method"], "inbox.claim");
                assert_eq!(req["params"]["message_id"], "m-old");
                let changed = json!({
                    "event": "messages.changed",
                    "data": {
                        "workspace_id": "00000000-0000-4000-8000-000000000001",
                        "workspace_seq": 9,
                        "changed": ["messages", "mailboxes", "notifications"]
                    },
                    "seq": 9
                });
                (
                    vec![
                        changed.to_string(),
                        json!({
                            "id": req["id"],
                            "error": {
                                "code": "message_not_pending",
                                "message": "message 'm-old' is no longer pending"
                            }
                        })
                        .to_string(),
                    ],
                    false,
                )
            }
            4 => (
                vec![json!({"id": req["id"], "result": {"entries": [{
                    "message_id": "m-new",
                    "sender": {
                        "kind": "agent",
                        "workspace_id": "00000000-0000-4000-8000-000000000001",
                        "session_instance_id": "00000000-0000-4000-8000-000000000002",
                        "pane_id": "%9"
                    },
                    "sender_label": "gemini-test",
                    "subject": "Replacement request",
                    "ts": 9,
                    "thread_root": "m-new"
                }]}})
                .to_string()],
                false,
            ),
            5 => (
                vec![json!({"id": req["id"], "result": {
                    "disposition": "claimed",
                    "message": {
                        "message_id": "m-new",
                        "kind": "msg",
                        "sender": {
                            "kind": "agent",
                            "workspace_id": "00000000-0000-4000-8000-000000000001",
                            "session_instance_id": "00000000-0000-4000-8000-000000000002",
                            "pane_id": "%9"
                        },
                        "sender_label": "gemini-test",
                        "subject": "Replacement request",
                        "body": "Use the replacement.",
                        "thread_root": "m-new"
                    }
                }})
                .to_string()],
                true,
            ),
            _ => panic!("unexpected request {req}"),
        }
    });

    let out = run_cyclops(
        &home,
        &[
            "inbox",
            "next",
            "--from",
            GEMINI_ENDPOINT,
            "--timeout",
            "1s",
            "--plain",
        ],
    );

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("[cyclops m-new]"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_has_a_bounded_no_message_exit() {
    let home = scratch_home("int");
    serve_once(&home, hello(1), move |req| match req["method"].as_str() {
        Some("events.subscribe") => (
            vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
            false,
        ),
        Some("inbox.list") => (
            vec![json!({"id": req["id"], "result": {"entries": []}}).to_string()],
            false,
        ),
        _ => panic!("unexpected request {req}"),
    });

    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "100ms", "--plain"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "no pending message arrived within 100ms. Increase --timeout or inspect the queue with cyclops inbox list."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_timeout_has_a_stable_json_outcome() {
    let home = scratch_home("inj");
    serve_once(&home, hello(1), move |req| match req["method"].as_str() {
        Some("events.subscribe") => (
            vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
            false,
        ),
        Some("inbox.list") => {
            assert_eq!(
                req["params"]["sender"]["pane_id"], "%9",
                "the no-match wait keeps its durable sender filter"
            );
            (
                vec![json!({"id": req["id"], "result": {"entries": []}}).to_string()],
                false,
            )
        }
        _ => panic!("unexpected request {req}"),
    });

    let out = run_cyclops(
        &home,
        &[
            "inbox",
            "next",
            "--from",
            GEMINI_ENDPOINT,
            "--timeout",
            "10ms",
            "--json",
        ],
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let value: Value = serde_json::from_slice(&out.stdout).expect("JSON timeout answer");
    assert_eq!(value["code"], "timeout");
    assert_eq!(value["data"]["pending"], false);
    assert_eq!(value["data"]["waited_ms"], 10);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_one_structured_daemon_failure() {
    let home = scratch_home("ine");
    serve_once(&home, hello(1), move |req| {
        assert_eq!(req["method"], "events.subscribe");
        (
            vec![json!({
                "id": req["id"],
                "error": {
                    "code": "unknown_method",
                    "message": "events.subscribe is unavailable"
                }
            })
            .to_string()],
            true,
        )
    });

    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "1s", "--json"]);

    assert_json_failure(
        &out,
        1,
        json!({
            "code": "unknown_method",
            "message": "events.subscribe is unavailable",
            "data": null
        }),
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_one_structured_list_failure() {
    let home = scratch_home("inr");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => (
                vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                false,
            ),
            2 => (
                vec![json!({
                    "id": req["id"],
                    "error": {
                        "code": "mailbox_error",
                        "message": "mailbox projection is unavailable"
                    }
                })
                .to_string()],
                true,
            ),
            _ => panic!("unexpected request {req}"),
        }
    });

    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "1s", "--json"]);

    assert_json_failure(
        &out,
        1,
        json!({
            "code": "mailbox_error",
            "message": "mailbox projection is unavailable",
            "data": null
        }),
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_an_unreadable_answer_as_one_object() {
    let home = scratch_home("inu");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => (
                vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                false,
            ),
            2 => (
                vec![json!({"id": req["id"], "result": "not an inbox list"}).to_string()],
                true,
            ),
            _ => panic!("unexpected request {req}"),
        }
    });

    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "1s", "--json"]);

    assert_json_failure(
        &out,
        1,
        json!({
            "code": "unreadable_answer",
            "message": "cyclops answered in a shape this client doesn't understand. The daemon and CLI are probably far apart in version; update the older one.",
            "data": null
        }),
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_a_dropped_stream_as_one_object() {
    let home = scratch_home("ing");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => (
                vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                false,
            ),
            2 => (
                vec![json!({"id": req["id"], "result": {"entries": []}}).to_string()],
                true,
            ),
            _ => panic!("unexpected request {req}"),
        }
    });

    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "1s", "--json"]);

    assert_json_failure(
        &out,
        1,
        json!({
            "code": "connection_lost",
            "message": "lost the connection to cyclops: the connection closed. The request may already have landed. Check that cyclopsd is running and inspect current state. Only repeat a send or reply with the same explicit --client-key.",
            "data": null
        }),
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_one_structured_claim_failure() {
    let home = scratch_home("inf");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => (
                vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                false,
            ),
            2 => (
                vec![json!({"id": req["id"], "result": {"entries": [{
                    "message_id": "m-denied",
                    "sender": {
                        "kind": "admin",
                        "workspace_id": "00000000-0000-4000-8000-000000000001"
                    },
                    "sender_label": "admin",
                    "ts": 1,
                    "thread_root": "m-denied"
                }]}})
                .to_string()],
                false,
            ),
            3 => (
                vec![json!({
                    "id": req["id"],
                    "error": {
                        "code": "denied",
                        "message": "claimant no longer owns this mailbox",
                        "data": {"message_id": "m-denied"}
                    }
                })
                .to_string()],
                true,
            ),
            _ => panic!("unexpected request {req}"),
        }
    });

    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "1s", "--json"]);

    assert_json_failure(
        &out,
        1,
        json!({
            "code": "denied",
            "message": "claimant no longer owns this mailbox",
            "data": {"message_id": "m-denied"}
        }),
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_names_an_uncertain_claim_instead_of_an_empty_timeout() {
    let home = scratch_home("inc");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => (
                vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                false,
            ),
            2 => (
                vec![json!({"id": req["id"], "result": {"entries": [{
                    "message_id": "m-uncertain",
                    "sender": {
                        "kind": "admin",
                        "workspace_id": "00000000-0000-4000-8000-000000000001"
                    },
                    "sender_label": "admin",
                    "ts": 1,
                    "thread_root": "m-uncertain"
                }]}})
                .to_string()],
                false,
            ),
            3 => {
                assert_eq!(req["method"], "inbox.claim");
                assert_eq!(req["params"]["message_id"], "m-uncertain");
                (Vec::new(), false)
            }
            _ => panic!("unexpected request {req}"),
        }
    });

    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "50ms", "--json"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let value: Value = serde_json::from_slice(&out.stdout).expect("JSON uncertain answer");
    assert_eq!(value["code"], "claim_outcome_unknown");
    assert_eq!(value["data"]["message_id"], "m-uncertain");
    assert!(value["message"]
        .as_str()
        .unwrap()
        .contains("may already be claimed"));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_never_claims_when_the_daemon_omits_the_selected_sender() {
    let home = scratch_home("inm");
    let mut step = 0_u8;
    serve_once(&home, hello(1), move |req| {
        step += 1;
        match step {
            1 => (
                vec![json!({"id": req["id"], "result": {"subscribed": true}}).to_string()],
                false,
            ),
            2 => (
                vec![json!({"id": req["id"], "result": {"entries": [{
                    "message_id": "m-old-daemon",
                    "sender_label": "gemini-test",
                    "ts": 1,
                    "thread_root": "m-old-daemon"
                }]}})
                .to_string()],
                true,
            ),
            _ => panic!("the unproven entry must not be claimed: {req}"),
        }
    });

    let out = run_cyclops(
        &home,
        &[
            "inbox",
            "next",
            "--from",
            GEMINI_ENDPOINT,
            "--timeout",
            "1s",
            "--json",
        ],
    );

    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let value: Value = serde_json::from_slice(&out.stdout).expect("JSON compatibility answer");
    assert_eq!(value["code"], "sender_filter_unavailable");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_rejects_a_zero_budget_before_waiting() {
    let home = scratch_home("inz");
    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "0ms"]);

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "can't read \"0ms\" as a duration. Use forms like 90s, 2m, 1m30s, or 500ms."
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_invalid_input_as_one_object() {
    let home = scratch_home("inb");
    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "0ms", "--json"]);

    assert_json_failure(
        &out,
        2,
        json!({
            "code": "bad_duration",
            "message": "can't read \"0ms\" as a duration. Use forms like 90s, 2m, 1m30s, or 500ms.",
            "data": {"value": "0ms"}
        }),
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_rejects_an_unrepresentable_budget_before_connecting() {
    let home = scratch_home("ino");
    let out = run_cyclops(
        &home,
        &["inbox", "next", "--timeout", "18446744073709551615s"],
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("can't read"));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_rejects_a_display_label_as_a_sender_selector() {
    let home = scratch_home("inl");
    let out = run_cyclops(
        &home,
        &["inbox", "next", "--from", "gemini-test", "--timeout", "1s"],
    );

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("recipient key must use canonical"));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_an_invalid_sender_as_one_object() {
    let home = scratch_home("ini");
    let out = run_cyclops(
        &home,
        &[
            "inbox",
            "next",
            "--from",
            "gemini-test",
            "--timeout",
            "1s",
            "--json",
        ],
    );

    assert_json_failure(
        &out,
        2,
        json!({
            "code": "invalid_recipient_key",
            "message": "recipient key must use canonical admin:<workspace-id> or agent:<workspace-id>/<session-instance-id>/%<pane> form",
            "data": {"value": "gemini-test"}
        }),
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn inbox_next_json_reports_a_connection_failure_as_one_object() {
    let home = scratch_home("ind");
    let out = run_cyclops(&home, &["inbox", "next", "--timeout", "1s", "--json"]);

    assert_json_failure(
        &out,
        1,
        json!({
            "code": "not_running",
            "message": cyclops_proto::NOT_RUNNING,
            "data": null
        }),
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
        assert!(req["params"]["reply_to"].is_null());
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
fn send_parked_with_required_wake_exits_one_with_reset_hint() {
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
        &[
            "send",
            "reviewer",
            "--subject",
            "s",
            "--body",
            "b",
            "--require-wake",
        ],
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

#[test]
fn accepted_send_with_blocked_wake_defaults_to_success() {
    let home = scratch_home("sk-default");
    serve_once(&home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": {
                "msg_id": "m-b3", "seq": 13, "inserted": true,
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
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("accepted m-b3\n"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("message is kept as parked"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn mailbox_attention_with_required_wake_exits_one_in_plain_and_json() {
    let production_result = || {
        json!({
            "msg_id": "m-mailbox", "seq": 14, "inserted": true,
            "deliveries": [{
                "to": "reviewer",
                "state": "queued",
                "notification_state": "attention_required",
                "quota_state": "held"
            }]
        })
    };

    let plain_home = scratch_home("sk-mailbox-plain");
    serve_once(&plain_home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": production_result()}).to_string()],
            false,
        )
    });
    let plain = run_cyclops(
        &plain_home,
        &["send", "reviewer", "--subject", "s", "--require-wake"],
    );
    assert_eq!(plain.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&plain.stdout),
        "accepted m-mailbox\n✓ accepted · wake quota held\n"
    );
    let _ = fs::remove_dir_all(&plain_home);

    let json_home = scratch_home("sk-mailbox-json");
    let expected = production_result();
    let response = expected.clone();
    serve_once(&json_home, hello(1), move |req| {
        (
            vec![json!({"id": req["id"], "result": response}).to_string()],
            false,
        )
    });
    let machine = run_cyclops(
        &json_home,
        &[
            "send",
            "reviewer",
            "--subject",
            "s",
            "--require-wake",
            "--json",
        ],
    );
    assert_eq!(machine.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&machine.stdout).trim(),
        expected.to_string()
    );
    assert!(machine.stderr.is_empty());
    let _ = fs::remove_dir_all(&json_home);
}

/// A send to a pane nothing detects, in the shape the daemon answers
/// with: the gate's machine cause plus the pane as data. The badge words
/// the cause and names the pane, the follow-up says the message did not
/// arrive and carries the command that fixes it. `--require-wake` lets a
/// script require more than durable acceptance.
#[test]
fn send_to_an_undetected_pane_says_it_did_not_arrive_and_required_wake_exits_one() {
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
    let out = run_cyclops(
        &home,
        &["send", "worker", "--subject", "hello", "--require-wake"],
    );
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
    assert!(out.status.success());
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
    let out = run_cyclops(
        &home,
        &[
            "send",
            "reviewer",
            "--subject",
            "s",
            "--json",
            "--require-wake",
        ],
    );
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
            vec![json!({"id": req["id"], "result": {"ok": true, "applied": true}}).to_string()],
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
    assert_eq!(seen[0]["params"]["agent"], serde_json::Value::Null);
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

    // Label-free hooks reach the daemon, which derives their origin from
    // authenticated socket credentials. Only the connection fails here.
    let out = run_cyclops_io(&home, &[], &["hook", "Stop"], Some("{}"));
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    let log = fs::read_to_string(home.join("hook-errors.log")).expect("hook error log");
    assert!(!log.contains("no agent identity"), "log: {log}");
    assert_eq!(log.trim().lines().count(), 2, "log: {log}");
    assert!(
        log.trim()
            .lines()
            .all(|l| l.contains("cyclops isn't running")),
        "log: {log}"
    );
    // A label-free hook consumes no counter: it would otherwise share one
    // namespace with every other label-free hook on the machine.
    assert!(!home.join("hookseq").join("Stop").exists());
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
        assert_eq!(req["params"]["until"], "turn_ended");
        // Default --timeout is 60s.
        assert_eq!(req["params"]["timeout_ms"], 60_000);
        (
            vec![json!({"id": req["id"], "error": {
                "code": "timeout",
                "message": "reviewer did not reach turn ended within 60000ms; state was working",
                "data": {"target": "reviewer", "until": "turn_ended", "state": "working", "waited_ms": 60_001}
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["wait", "reviewer", "--until", "turn-ended"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "reviewer didn't reach turn ended within 60 seconds. Last state: working. Give it more time with --timeout, or look in with cyclops status."
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
                "data": {"target": "reviewer", "until": "turn_ended", "state": "dead", "waited_ms": 1200}
            }})
            .to_string()],
            false,
        )
    });
    let out = run_cyclops(&home, &["wait", "reviewer", "--until", "turn-ended"]);
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
    let cache = text
        .lines()
        .find_map(|line| line.strip_prefix("SAW "))
        .map(PathBuf::from)
        .expect("the installer should report its cache path");
    assert!(
        !cache.starts_with(&home),
        "Cargo build artifacts must stay outside CYCLOPS_HOME: {}",
        cache.display()
    );
    // Named, because a gigabyte-scale directory the operator never asked
    // for is one they should be told about rather than find.
    assert!(text.contains("building in"), "{text}");
    // And it is still there once the clone is gone, which is the point.
    assert!(cache.is_dir(), "the cache must outlive the clone");
    assert_eq!(
        fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o700
    );

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
    let _ = fs::remove_dir_all(&cache);
}
