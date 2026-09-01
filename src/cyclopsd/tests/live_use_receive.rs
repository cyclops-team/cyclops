//! A socket pull remains usable when a foreground watch gates pane delivery.

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(10);

const LIVE_USE_MANIFEST: &str = r#"
[agent]
id = "live-use"
display_name = "Live use fixture"
process_names = ["live-use-agent"]
argv_basenames = ["live-use-agent"]

[[rule]]
id = "watch_tool_working"
state = "working"
priority = 200
region = "pane_title"
contains = ["CYCLOPS-WATCH-ACTIVE"]

[[rule]]
id = "otherwise_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# The foreground watch is the fact under test. The fixture must still provide
# the positive composer evidence every terminal write requires, or a missing
# composer contract blocks the attempt for an unrelated reason.
[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']
"#;

struct CommandFifo(File);

impl CommandFifo {
    fn send(&mut self, fields: &[&str]) {
        writeln!(self.0, "{}", fields.join("\t")).expect("write fixture command");
        self.0.flush().expect("flush fixture command");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn foreground_watch_gates_the_doorbell_but_socket_pull_claims_the_message() {
    if !tmux_available() {
        return;
    }

    let dir = fresh_dir("cycle");
    let command_fifo = dir.join("commands.fifo");
    mkfifo(&command_fifo);
    let agent = dir.join("live-use-agent");
    let watch = dir.join("cyclops");
    fs::copy(agent_binary(), &agent).expect("copy agent fixture");
    fs::copy(agent_binary(), &watch).expect("copy watch fixture");

    let pane_command = format!("{} {}", agent.display(), command_fifo.display());
    let mut rig = Rig::new("live-use-cycle", LIVE_USE_MANIFEST, &pane_command, "").await;
    let pane = rig.pane_ids().await.remove(0);
    rig.label(&pane, "codex-test").await;
    wait_for_pane(&mut rig, &pane, "idle").await;
    rig.tmux.run_ok(&["split-window", "-d", "-t", &pane, "cat"]);
    rig.wait_attached(2).await;
    let sender_pane = rig
        .pane_ids()
        .await
        .into_iter()
        .find(|candidate| candidate != &pane)
        .expect("sender pane");
    rig.label(&sender_pane, "gemini-test").await;

    let mut commands = CommandFifo(open_fifo_writer(command_fifo).await);
    let watch_ready = dir.join("watch-ready");
    commands.send(&["watch", path(&watch), path(&watch_ready)]);
    read_nonempty(&watch_ready).await;
    wait_for_pane(&mut rig, &pane, "working").await;

    let subject = "Startup retrospective";
    let secret = "the mailbox payload must never touch this pane";
    let sent = rig
        .daemon
        .msg_send(
            "gemini-test",
            serde_json::from_value(json!({
                "to": ["codex-test"],
                "subject": subject,
                "body": secret,
                "client_key": "live-use-cycle"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();

    rig.ev
        .wait_event(Duration::from_secs(5), |event| {
            event["event"] == "messages.changed"
                && event["data"]["changed"]
                    .as_array()
                    .is_some_and(|areas| areas.iter().any(|area| area == "messages"))
        })
        .await;
    wait_for_notification_state(&rig, &message_id, "gating").await;

    let capture = rig.tmux.capture(&pane);
    assert!(
        !capture.contains(&message_id),
        "doorbell reached pane: {capture}"
    );
    assert!(!capture.contains(secret), "payload reached pane: {capture}");

    let processes = Command::new("ps")
        .args(["-axo", "pid=,pgid=,tpgid=,comm=,args="])
        .output()
        .expect("read fixture process table");
    let processes = String::from_utf8_lossy(&processes.stdout);
    let fixture_processes: Vec<_> = processes
        .lines()
        .filter(|line| line.contains(path(&dir)))
        .collect();
    assert!(
        fixture_processes
            .iter()
            .any(|line| { line.contains(&format!("{} watch --from gemini", watch.display())) }),
        "watch fixture missing from process table: {fixture_processes:?}"
    );

    let status = rig.ctl.request("status", json!({})).await;
    let diagnostics = status["result"]["diagnostics"]
        .as_array()
        .unwrap_or_else(|| panic!("no diagnostics in {status}; processes: {fixture_processes:?}"));
    assert_eq!(diagnostics.len(), 1, "unexpected diagnostics: {status}");
    assert_eq!(diagnostics[0]["code"], "deadlock_risk");
    assert_eq!(diagnostics[0]["message_id"], message_id);
    assert_eq!(diagnostics[0]["recipient_label"], "codex-test");
    assert_eq!(diagnostics[0]["pane_id"], pane);
    assert_eq!(diagnostics[0]["recipient"]["pane_id"], pane);
    assert!(!status.to_string().contains(subject));
    assert!(!status.to_string().contains(secret));

    let claim_out = dir.join("claim.json");
    let claim_params = json!({"message_id": message_id}).to_string();
    commands.send(&[
        "request",
        path(&socket_client_path()),
        path(rig.daemon.socket_path().as_path()),
        path(&claim_out),
        "inbox.claim",
        &claim_params,
    ]);
    let claimed: Value = serde_json::from_str(&read_nonempty(&claim_out).await).unwrap();
    assert!(claimed["error"].is_null(), "claim failed: {claimed}");
    assert_eq!(claimed["result"]["disposition"], "claimed");
    assert_eq!(claimed["result"]["message"]["sender_label"], "gemini-test");
    assert_eq!(
        claimed["result"]["message"]["sender"]["pane_id"],
        sender_pane
    );
    assert_eq!(claimed["result"]["message"]["body"], secret);

    // The claim response proves the public socket operation. Check the cached
    // observation separately: socket `status` first performs a bounded live
    // refresh and may honestly report `unknown` when that refresh is late.
    let status = json!({"result": rig.daemon.status(false)});
    let target = status["result"]["sessions"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["pane_id"] == pane)
        .expect("target pane remains in cached status");
    assert_eq!(target["state"], "working", "{status}");

    let capture = rig.tmux.capture(&pane);
    assert!(
        !capture.contains(&message_id),
        "claim wrote to pane: {capture}"
    );
    assert!(
        !capture.contains(secret),
        "claim wrote payload to pane: {capture}"
    );

    commands.send(&["exit"]);
    rig.shutdown().await;
}

async fn wait_for_pane(rig: &mut Rig, pane_id: &str, expected: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let pane = status["result"]["sessions"][0]["panes"]
            .as_array()
            .and_then(|panes| panes.iter().find(|pane| pane["pane_id"] == pane_id));
        if pane.is_some_and(|pane| pane["manifest"] == "live-use" && pane["state"] == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane never reached {expected}: {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_notification_state(rig: &Rig, message_id: &str, state: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let journal = workspace_journal(rig);
        if journal.lines().any(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|line| {
                line["id"] == message_id
                    && line["data"]["type"] == "notification_transition"
                    && line["data"]["state"] == state
            })
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} never reached {state}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn workspace_journal(rig: &Rig) -> String {
    let workspace = fs::read_dir(rig.home.join("workspaces"))
        .expect("workspace directory")
        .next()
        .expect("workspace entry")
        .expect("read workspace entry")
        .path();
    fs::read_to_string(workspace.join("messages.ndjson")).expect("workspace journal")
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-live-use-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create fixture directory");
    dir
}

fn agent_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let dir = fresh_dir("agent-bin");
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/live_use_agent.rs");
        let binary = dir.join("live-use-agent");
        let output = Command::new("rustc")
            .args([
                "--edition=2021",
                "-Dwarnings",
                path(&source),
                "-o",
                path(&binary),
            ])
            .output()
            .expect("compile live-use-agent");
        assert!(
            output.status.success(),
            "live-use-agent compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    })
}

fn socket_client_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/socket_client.py")
}

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("run mkfifo");
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

async fn read_nonempty(path: &Path) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(content) = fs::read_to_string(path) {
            if !content.trim().is_empty() {
                return content.trim().to_owned();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn path(value: &Path) -> &str {
    value.to_str().expect("fixture path is UTF-8")
}
