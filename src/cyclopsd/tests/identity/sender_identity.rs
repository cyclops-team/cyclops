//! Who the daemon thinks sent a message, proven over the real socket.
//!
//! The daemon resolves a sender from the CALLING process's ancestry, so
//! the only honest way to test it is to make the request from a process
//! chosen for its ancestry. Both cases here do that with real processes
//! against a real daemon:
//!
//! - a client descending from the admitted agent in a watched pane resolves
//!   to that agent;
//! - a client running under a vendor process, outside every watched pane,
//!   is refused, and nothing reaches the ledger.
//!
//! The second one is built rather than borrowed. A suite started from
//! inside an agent CLI is already a vendor descendant, and a suite started
//! from a plain shell is not, so relying on the runner's own ancestry
//! would make the result depend on where the tests were launched. The
//! vendor here is a shell this test spawns under a shipped vendor's argv
//! name, which is the same shape and is true everywhere.
//!
//! A separate control starts from the test runner without a vendor parent and
//! proves that a same-user shell remains the workspace administrator.

use crate::common;

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use common::*;
use serde_json::{json, Value};

/// This file's own manifest, with names nothing else on a machine is
/// called.
///
/// The shared fixture claims `python3`, `sh` and `bash`, which makes
/// every helper in these tests a vendor and would let the negative case
/// below pass without its vendor-named parent doing anything. The names
/// here can only come from a symlink or an argv this test made.
const NAMED_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Sender fixture"
process_names = ["cycagent", "cycvendor"]
argv_basenames = ["cycagent"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

[[rule]]
id = "composer_empty"
state = "idle"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']
"#;

fn client_path() -> String {
    format!(
        "{}/tests/common/socket_client.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// A directory of python3 symlinks under names this file controls.
///
/// The name a process runs under is what the daemon classifies it by, so
/// choosing that name is how these tests decide which processes are
/// agents. `cycagent` is what the watched pane runs; `cycclient` is the
/// socket client, deliberately NOT an agent, so a denial can only come
/// from something above it.
fn named_bin(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}-bin"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let out = Command::new("sh")
        .args(["-c", "command -v python3"])
        .output()
        .expect("look up python3");
    let py = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!py.is_empty(), "python3 must be on PATH");
    for name in ["cycagent", "cycclient"] {
        let link = dir.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&py, &link).expect("symlink python3");
    }
    dir
}

fn agent_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let dir = cyclops_proto::scratch::scratch_dir("cyc-sender-agent-bin");
        std::fs::create_dir_all(&dir).expect("sender agent binary directory");
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/live_use_agent.rs");
        let binary = dir.join("cycagent");
        let output = Command::new("rustc")
            .args([
                "--edition=2021",
                "-Dwarnings",
                source.to_str().expect("source path is UTF-8"),
                "-o",
                binary.to_str().expect("binary path is UTF-8"),
            ])
            .output()
            .expect("compile sender agent fixture");
        assert!(
            output.status.success(),
            "sender agent compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    })
}

fn create_fifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("create sender agent FIFO");
    assert!(status.success(), "mkfifo failed for {}", path.display());
}

async fn open_fifo_writer(path: PathBuf) -> File {
    tokio::task::spawn_blocking(move || {
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("open FIFO {}: {error}", path.display()))
    })
    .await
    .expect("join FIFO open")
}

fn agent_request(
    commands: &mut File,
    client: &Path,
    socket: &Path,
    output: &Path,
    method: &str,
    params: &Value,
) {
    writeln!(
        commands,
        "request\t{}\t{}\t{}\t{}\t{}",
        client.display(),
        socket.display(),
        output.display(),
        method,
        params
    )
    .expect("write sender agent request");
    commands.flush().expect("flush sender agent request");
}

async fn wait_for_manifest(rig: &mut Rig, pane_id: &str) {
    let mut last = Value::Null;
    for _ in 0..100 {
        let status = rig.ctl.request("status", json!({})).await;
        let bound = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|session| session["panes"].as_array().into_iter().flatten())
            .any(|pane| pane["pane_id"] == pane_id && pane["manifest"] == "fix");
        if bound {
            return;
        }
        last = status;
        // No event: binding a manifest publishes nothing, and status blanks
        // `manifest` while its live refresh is incomplete.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("pane {pane_id} never bound the sender fixture manifest: {last}");
}

/// Wait for the client to write its response file, then read it.
async fn response(out: &Path) -> Value {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(out) {
            if !text.trim().is_empty() {
                return serde_json::from_str(&text).expect("client response parses");
            }
        }
        // No event: the fixture answers by writing a file.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("client never answered: {}", out.display());
}

fn workspace_lines(home: &Path) -> Vec<Value> {
    let workspace = std::fs::read_to_string(home.join("identity/workspace-id"))
        .expect("workspace identity is readable");
    let path = home
        .join("workspaces")
        .join(workspace.trim())
        .join("messages.ndjson");
    let text = std::fs::read_to_string(path).expect("workspace journal is readable");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("workspace line is valid JSON"))
        .collect()
}

/// A request from inside a watched pane is that pane's, by ancestry.
///
/// The pane root is what the walk has to reach. Nothing about the request
/// says who sent it, and nothing in it could: the label comes from the
/// process tree the connection was opened from.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_below_a_current_agent_sends_as_that_agent() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("sendid");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("sendid", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    // A second pane in the same watched session runs the client, so the
    // recipient's composer is left alone.
    let sender_fifo = rig.home.join("sender-agent.fifo");
    create_fifo(&sender_fifo);
    let sender_agent = format!("{} {}", agent_binary().display(), sender_fifo.display());
    rig.tmux
        .run_ok(&["split-window", "-t", &pane, "-d", &sender_agent]);
    let mut sender_commands = open_fifo_writer(sender_fifo).await;
    // The watcher learns about the new pane on its own subscription tick.
    let mut sender_pane = None;
    for _ in 0..100 {
        if let Some(p) = rig.pane_ids().await.into_iter().find(|p| p != &pane) {
            sender_pane = Some(p);
            break;
        }
        // No event: a pane whose first verdict is unknown publishes nothing
        // (fusion skips the first unknown), so the pane list is asked for.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sender_pane = sender_pane.expect("the split pane appears in status");
    rig.label(&sender_pane, "sender").await;
    wait_for_manifest(&mut rig, &sender_pane).await;

    let out = rig.home.join("inside.json");
    let socket = rig.daemon.socket_path();
    agent_request(
        &mut sender_commands,
        Path::new(&client_path()),
        &socket,
        &out,
        "msg.send",
        &json!({"to": ["hooky"], "subject": "inside", "body": "b"}),
    );

    let resp = response(&out).await;
    assert!(resp["error"].is_null(), "the send must be accepted: {resp}");

    let denied_out = rig.home.join("agent-withdraw-denied.json");
    agent_request(
        &mut sender_commands,
        Path::new(&client_path()),
        &socket,
        &denied_out,
        "notification.withdraw",
        &json!({
            "attempt_id": "att-00000000-0000-4000-8000-000000000001",
            "recipient": {
                "kind": "agent",
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "session_instance_id": "00000000-0000-0000-0000-000000000002",
                "pane_id": "%1"
            }
        }),
    );
    let denied = response(&denied_out).await;
    assert_eq!(
        denied["error"]["code"], "denied",
        "an agent pane cannot exercise an administrator recovery verb: {denied}"
    );

    let message_id = resp["result"]["msg_id"]
        .as_str()
        .expect("accepted message has an id");

    let from = workspace_lines(&rig.home)
        .into_iter()
        .find(|l| l["kind"] == "msg" && l["subject"] == "inside")
        .expect("the message reached the workspace journal")["from"]
        .clone();
    assert_eq!(
        from, "sender",
        "the sender is the admitted agent the request descended from"
    );

    assert!(
        !rig.ledger_lines()
            .iter()
            .any(|line| line["subject"] == "inside"),
        "new messages must not be copied into a session ledger"
    );
    let history = rig.ctl.request("msg.history", json!({})).await;
    let history_count = history["result"]["lines"]
        .as_array()
        .expect("history lines")
        .iter()
        .filter(|line| line["id"] == message_id)
        .count();
    assert_eq!(
        history_count, 1,
        "history returns the message once: {history}"
    );
    let thread = rig
        .ctl
        .request("msg.thread", json!({"id": message_id}))
        .await;
    let thread_count = thread["result"]["lines"]
        .as_array()
        .expect("thread lines")
        .iter()
        .filter(|line| line["kind"] == "msg" && line["id"] == message_id)
        .count();
    assert_eq!(thread_count, 1, "thread returns the message once: {thread}");

    rig.shutdown().await;
}

/// A vendor process outside every watched pane is nobody the daemon can
/// name, and refusing it has to happen before anything is written.
///
/// The shape: an agent, or a helper it spawned, that is not in a pane this
/// daemon watches. Walking out of the process tree without meeting a pane
/// proves only that it is outside them; it does not make it the operator,
/// and the operator is the most trusted name a recipient can read.
#[tokio::test(flavor = "multi_thread")]
async fn a_vendor_outside_every_watched_pane_is_refused() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("sendorph");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("sendorph", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;
    let before = workspace_lines(&rig.home);

    let out = rig.home.join("orphan.json");
    let socket = rig.daemon.socket_path();
    // A shell wearing an AGENT's name, with the client as its child. The
    // client's own name is not an agent's, so the denial below can only
    // come from this parent. The trailing `:` keeps the shell from
    // exec'ing the client in place, which would drop the name entirely.
    let script = format!(
        "{}/cycclient {} {} {} msg.send '{}'; :",
        bin.display(),
        client_path(),
        socket.display(),
        out.display(),
        json!({"to": ["hooky"], "subject": "orphan", "body": "b"})
    );
    let mut child = Command::new("/bin/sh")
        .arg0("cycvendor")
        .arg("-c")
        .arg(&script)
        .spawn()
        .expect("spawn the vendor-named shell");

    let resp = response(&out).await;
    assert_eq!(
        resp["error"]["code"], "denied",
        "a vendor outside every watched pane must be refused: {resp}"
    );
    let _ = child.wait();

    // Zero ledger mutation, checked before anything else runs. Not just
    // no message: no delivery, no state line, nothing. A refusal that
    // wrote anything at all would leave a record nobody sent.
    assert_eq!(
        workspace_lines(&rig.home),
        before,
        "a refused sender must not move the workspace journal by one line"
    );

    // The control that makes the assertion above mean something: the
    // SAME client, from this test process, with no agent-named parent
    // between them. If that were refused too, the denial would be about
    // the client rather than about what it is running under, and this
    // test would be proving nothing.
    let control = rig.home.join("control.json");
    let ok = Command::new(bin.join("cycclient"))
        .arg(client_path())
        .arg(socket.display().to_string())
        .arg(control.display().to_string())
        .arg("msg.send")
        .arg(json!({"to": ["hooky"], "subject": "control", "body": "b"}).to_string())
        .status()
        .expect("run the control client");
    assert!(ok.success());
    let resp = response(&control).await;
    assert!(
        resp["error"].is_null(),
        "the same client without an agent above it must be accepted: {resp}"
    );

    // And the only thing the ledger gained is the control's message.
    let lines = workspace_lines(&rig.home);
    assert!(
        !lines.iter().any(|l| l["subject"] == "orphan"),
        "no message and no delivery for the refused send"
    );
    assert!(
        lines.iter().any(|l| l["subject"] == "control"),
        "the accepted control send is on the record"
    );
    let control = lines
        .iter()
        .find(|line| line["subject"] == "control")
        .expect("control message");
    assert_eq!(
        control["from"], "admin",
        "a same-user caller with no agent ancestor is the operator"
    );

    rig.shutdown().await;
}

/// A headless registration is one more root the walk can stop at. A helper
/// the registered agent started descends from that root and sends as the
/// label; the request names nothing, and no pane is involved anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_helper_started_by_a_headless_agent_is_that_agent() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("sendheadless");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("sendheadless", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;
    let socket = rig.daemon.socket_path();

    // The agent fixture, started by this test in no pane at all.
    let fifo = rig.home.join("headless-agent.fifo");
    create_fifo(&fifo);
    let mut child = Command::new(agent_binary())
        .arg(&fifo)
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn the headless agent fixture");
    let mut commands = open_fifo_writer(fifo).await;

    let registered_out = rig.home.join("headless-register.json");
    agent_request(
        &mut commands,
        Path::new(&client_path()),
        &socket,
        &registered_out,
        "headless.register",
        &json!({"label": "worker"}),
    );
    let registered = response(&registered_out).await;
    assert!(
        registered["error"].is_null(),
        "the registration must be accepted: {registered}"
    );

    // The helper is the python client the fixture runs: a child of the
    // agent, not an agent itself.
    let out = rig.home.join("headless-send.json");
    agent_request(
        &mut commands,
        Path::new(&client_path()),
        &socket,
        &out,
        "msg.send",
        &json!({"to": ["hooky"], "subject": "from a headless helper", "body": "b"}),
    );
    let resp = response(&out).await;
    assert!(resp["error"].is_null(), "the send must be accepted: {resp}");
    let from = workspace_lines(&rig.home)
        .into_iter()
        .find(|l| l["kind"] == "msg" && l["subject"] == "from a headless helper")
        .expect("the message reached the workspace journal")["from"]
        .clone();
    assert_eq!(
        from, "worker",
        "the sender is the registered headless label"
    );

    let _ = writeln!(commands, "exit");
    let _ = child.wait();
    rig.shutdown().await;
}

/// A vendor process that does not descend from the registered root is
/// still a vendor outside every pane: refused, and never the headless
/// label. The registration binds one process generation, not a name.
#[tokio::test(flavor = "multi_thread")]
async fn another_vendor_process_cannot_resolve_to_a_headless_label() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("sendotherheadless");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("sendotherheadless", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;
    let socket = rig.daemon.socket_path();

    let fifo = rig.home.join("headless-agent.fifo");
    create_fifo(&fifo);
    let mut child = Command::new(agent_binary())
        .arg(&fifo)
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn the headless agent fixture");
    let mut commands = open_fifo_writer(fifo).await;
    let registered_out = rig.home.join("headless-register.json");
    agent_request(
        &mut commands,
        Path::new(&client_path()),
        &socket,
        &registered_out,
        "headless.register",
        &json!({"label": "worker"}),
    );
    let registered = response(&registered_out).await;
    assert!(registered["error"].is_null(), "{registered}");
    let before = workspace_lines(&rig.home);

    // A different vendor-named shell, sibling to the registered agent.
    let out = rig.home.join("other-vendor.json");
    let script = format!(
        "{}/cycclient {} {} {} msg.send '{}'; :",
        bin.display(),
        client_path(),
        socket.display(),
        out.display(),
        json!({"to": ["hooky"], "subject": "impostor", "body": "b"})
    );
    let mut other = Command::new("/bin/sh")
        .arg0("cycvendor")
        .arg("-c")
        .arg(&script)
        .spawn()
        .expect("spawn the other vendor-named shell");
    let resp = response(&out).await;
    assert_eq!(
        resp["error"]["code"], "denied",
        "a vendor outside the registered tree must be refused: {resp}"
    );
    let _ = other.wait();
    assert_eq!(
        workspace_lines(&rig.home),
        before,
        "a refused sender must not move the workspace journal by one line"
    );

    let _ = writeln!(commands, "exit");
    let _ = child.wait();
    rig.shutdown().await;
}

/// "me" is resolved the same way a sender is, from the caller's process
/// tree, so it has to be asked from a process whose tree says something.
///
/// The read side would otherwise hand one caller another caller's
/// messages: whoever asks gets whoever the daemon guessed.
#[tokio::test(flavor = "multi_thread")]
async fn me_on_the_read_side_is_the_calling_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("sendme");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("sendme", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    let asker_fifo = rig.home.join("asker-agent.fifo");
    create_fifo(&asker_fifo);
    let asker_agent = format!("{} {}", agent_binary().display(), asker_fifo.display());
    rig.tmux
        .run_ok(&["split-window", "-t", &pane, "-d", &asker_agent]);
    let mut asker_commands = open_fifo_writer(asker_fifo).await;
    let mut sender_pane = None;
    for _ in 0..100 {
        if let Some(p) = rig.pane_ids().await.into_iter().find(|p| p != &pane) {
            sender_pane = Some(p);
            break;
        }
        // No event: a pane whose first verdict is unknown publishes nothing
        // (fusion skips the first unknown), so the pane list is asked for.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sender_pane = sender_pane.expect("the split pane appears in status");
    rig.label(&sender_pane, "asker").await;
    wait_for_manifest(&mut rig, &sender_pane).await;

    // Two messages the rig sends as the operator, and one the pane sends
    // as itself. "me", asked from that pane, must find only its own.
    rig.send(json!({"to": ["hooky"], "subject": "not mine", "body": "b"}))
        .await;
    let sent = rig.home.join("me-send.json");
    let asked = rig.home.join("me-history.json");
    let socket = rig.daemon.socket_path();
    agent_request(
        &mut asker_commands,
        Path::new(&client_path()),
        &socket,
        &sent,
        "msg.send",
        &json!({"to": ["hooky"], "subject": "mine", "body": "b"}),
    );
    let resp = response(&sent).await;
    assert!(resp["error"].is_null(), "the pane's own send: {resp}");
    rig.label(&sender_pane, "renamed").await;

    agent_request(
        &mut asker_commands,
        Path::new(&client_path()),
        &socket,
        &asked,
        "msg.history",
        &json!({"from": "me"}),
    );
    let resp = response(&asked).await;
    let subjects: Vec<String> = resp["result"]["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("no lines in {resp}"))
        .iter()
        .map(|l| l["subject"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        subjects,
        vec!["mine".to_string()],
        "\"me\" is the pane that asked, not whoever else has been sending"
    );

    rig.shutdown().await;
}

/// A connection is authority for the process that opened it, and only
/// while that process is still the one on it.
///
/// The bug this pins: peer credentials were read once at accept and
/// trusted for every later request. A connection outlives a request, so a
/// client could send, replace itself with another program, and keep
/// sending on the same socket under the first program's name.
///
/// The pid does not change across an exec, and neither does a start time.
/// What separates the two is the kernel's own execution generation, which
/// is why the daemon asks the kernel again rather than remembering.
/// macOS exposes that generation. Linux can only pin the process birth.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_execs_loses_the_connection_it_opened() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("sendexec");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("sendexec", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    let before = rig.home.join("before.json");
    let after = rig.home.join("after.json");
    let client = format!(
        "{}/tests/common/socket_client_exec.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut child = Command::new(bin.join("cycclient"))
        .arg(&client)
        .arg(rig.daemon.socket_path())
        .arg(&before)
        .arg(&after)
        .spawn()
        .expect("spawn the exec client");

    let first = response(&before).await;
    assert!(
        first["error"].is_null(),
        "the process that opened the connection may send: {first}"
    );

    let second = response(&after).await;
    assert_eq!(
        second["error"]["code"], "denied",
        "the same connection under a different program: {second}"
    );
    let _ = child.wait();

    let lines = workspace_lines(&rig.home);
    assert!(
        lines.iter().any(|l| l["subject"] == "before-exec"),
        "the first send is on the record"
    );
    assert!(
        !lines.iter().any(|l| l["subject"] == "after-exec"),
        "nothing may be appended for a peer the connection no longer answers for"
    );

    rig.shutdown().await;
}
