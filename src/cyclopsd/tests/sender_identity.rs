//! Who the daemon thinks sent a message, proven over the real socket.
//!
//! The daemon resolves a sender from the CALLING process's ancestry, so
//! the only honest way to test it is to make the request from a process
//! chosen for its ancestry. Both cases here do that with real processes
//! against a real daemon:
//!
//! - a client running inside a watched pane resolves to that pane;
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
//! The positive operator case stays a unit test with an injected chain
//! (`identity::tests`): a suite running under an agent cannot honestly
//! produce a chain with no agent in it.

mod common;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
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

/// Wait for the client to write its response file, then read it.
async fn response(out: &Path) -> Value {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(out) {
            if !text.trim().is_empty() {
                return serde_json::from_str(&text).expect("client response parses");
            }
        }
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
async fn a_client_inside_a_watched_pane_sends_as_that_pane() {
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
    rig.tmux.run_ok(&["split-window", "-t", &pane, "-d", "sh"]);
    // The watcher learns about the new pane on its own subscription tick.
    let mut sender_pane = None;
    for _ in 0..100 {
        if let Some(p) = rig.pane_ids().await.into_iter().find(|p| p != &pane) {
            sender_pane = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sender_pane = sender_pane.expect("the split pane appears in status");
    rig.label(&sender_pane, "sender").await;

    let out = rig.home.join("inside.json");
    let socket = rig.daemon.socket_path();
    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        &sender_pane,
        &format!(
            "{}/cycclient {} {} {} msg.send '{}'",
            bin.display(),
            client_path(),
            socket.display(),
            out.display(),
            json!({"to": ["hooky"], "subject": "inside", "body": "b"})
        ),
        "Enter",
    ]);

    let resp = response(&out).await;
    assert!(resp["error"].is_null(), "the send must be accepted: {resp}");

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
        "the sender is the pane the request came from"
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

    rig.tmux.run_ok(&["split-window", "-t", &pane, "-d", "sh"]);
    let mut sender_pane = None;
    for _ in 0..100 {
        if let Some(p) = rig.pane_ids().await.into_iter().find(|p| p != &pane) {
            sender_pane = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sender_pane = sender_pane.expect("the split pane appears in status");
    rig.label(&sender_pane, "asker").await;

    // Two messages the rig sends as the operator, and one the pane sends
    // as itself. "me", asked from that pane, must find only its own.
    rig.send(json!({"to": ["hooky"], "subject": "not mine", "body": "b"}))
        .await;
    let sent = rig.home.join("me-send.json");
    let asked = rig.home.join("me-history.json");
    let socket = rig.daemon.socket_path();
    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        &sender_pane,
        &format!(
            "{}/cycclient {} {} {} msg.send '{}'",
            bin.display(),
            client_path(),
            socket.display(),
            sent.display(),
            json!({"to": ["hooky"], "subject": "mine", "body": "b"})
        ),
        "Enter",
    ]);
    let resp = response(&sent).await;
    assert!(resp["error"].is_null(), "the pane's own send: {resp}");
    rig.label(&sender_pane, "renamed").await;

    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        &sender_pane,
        &format!(
            "{}/cycclient {} {} {} msg.history '{}'",
            bin.display(),
            client_path(),
            socket.display(),
            asked.display(),
            json!({"from": "me"})
        ),
        "Enter",
    ]);
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
