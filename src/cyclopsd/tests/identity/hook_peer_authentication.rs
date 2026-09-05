//! Hook reports are admitted from the current agent's process tree.
//!
//! These tests use real processes and the daemon's Unix socket. They cover a
//! direct hook child, a hook beside a foreground tool, and a retained
//! connection from an agent that the pane replaced.

use crate::common;

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);

const HOOK_AUTH_MANIFEST: &str = r#"
[agent]
id = "hookauth"
display_name = "Hook auth fixture"
process_names = ["cycauth-agent"]
argv_basenames = ["cycauth-agent"]

[hooks]
turn_start = "UserPromptSubmit"
turn_start_evidence = "candidate"
ack = "UserPromptSubmit"
ack_evidence = "dispatch"
ack_payload_field = "prompt"

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
composer_semantic = "clean"
"#;

struct CommandFifo(File);

impl CommandFifo {
    fn send(&mut self, fields: &[&str]) {
        writeln!(self.0, "{}", fields.join("\t")).expect("write fixture command");
        self.0.flush().expect("flush fixture command");
    }
}

struct Agent {
    pid: u32,
    commands: CommandFifo,
}

struct HookRig {
    rig: Rig,
    dir: PathBuf,
    socket: PathBuf,
    controller_pid: u32,
    controller: CommandFifo,
}

impl HookRig {
    async fn start(tag: &str, label: &str) -> Self {
        let dir = fresh_dir(tag);
        let controller_fifo = dir.join("controller.fifo");
        mkfifo(&controller_fifo);

        let pane_command = format!(
            "python3 {} {} {} {}",
            controller_path().display(),
            controller_fifo.display(),
            agent_binary().display(),
            dir.join("controller.pid").display(),
        );
        let mut rig = Rig::new(tag, HOOK_AUTH_MANIFEST, &pane_command, "").await;
        let pane = rig.pane_ids().await.remove(0);
        rig.label(&pane, label).await;

        let socket = rig.daemon.socket_path();
        let controller = CommandFifo(open_fifo_writer(controller_fifo).await);
        let controller_pid = read_pid(&dir.join("controller.pid")).await;
        Self {
            rig,
            dir,
            socket,
            controller_pid,
            controller,
        }
    }

    async fn start_agent(&mut self, name: &str) -> Agent {
        let fifo = self.path(&format!("{name}.fifo"));
        let pid_file = self.path(&format!("{name}.pid"));
        mkfifo(&fifo);
        self.controller
            .send(&["start", path(&fifo), path(&pid_file)]);

        let pid = read_pid(&pid_file).await;
        assert_ppid(pid, self.controller_pid);
        assert_eq!(
            process_field(self.controller_pid, "tpgid="),
            Some(pid),
            "agent {pid} must own the pane terminal"
        );
        Agent {
            pid,
            commands: CommandFifo(open_fifo_writer(fifo).await),
        }
    }

    async fn wait_for_agent_exit(&mut self, done_file: &Path) {
        self.controller.send(&["wait", path(done_file)]);
        read_nonempty(done_file).await;
    }

    async fn wait_for_current_agent(&mut self, agent: &Agent) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert_ppid(agent.pid, self.controller_pid);
            let status = self.rig.ctl.request("status", json!({})).await;
            let pane = &status["result"]["sessions"][0]["panes"][0];
            if pane["current_command"] == "cycauth-agent" && pane["manifest"] == "hookauth" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not observe replacement agent {}: {status}",
                agent.pid
            );
            // No event: `current_command` and `manifest` are status fields with no
            // announcement of their own.
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_for_manifest(&mut self, manifest: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = self.rig.ctl.request("status", json!({})).await;
            let pane = &status["result"]["sessions"][0]["panes"][0];
            if pane["manifest"] == manifest {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon lost manifest {manifest:?} while a child held the terminal: {status}"
            );
            // No event: binding a manifest publishes nothing.
            tokio::time::sleep(POLL).await;
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    async fn shutdown(mut self) {
        self.controller.send(&["quit"]);
        self.rig.shutdown().await;
    }
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-hookauth-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create hook fixture directory");
    dir
}

fn agent_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let dir = fresh_dir("agent-bin");
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/cycauth_agent.rs");
        let binary = dir.join("cycauth-agent");
        let output = Command::new("rustc")
            .args([
                "--edition=2021",
                "-Dwarnings",
                path(&source),
                "-o",
                path(&binary),
            ])
            .output()
            .expect("compile cycauth-agent");
        assert!(
            output.status.success(),
            "cycauth-agent compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    })
}

fn controller_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/mock_pane_controller.py")
}

fn client_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/hook_peer_client.py")
}

fn path(value: &Path) -> &str {
    value.to_str().expect("fixture path is UTF-8")
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
        // No event: the fixture answers by writing a file.
        tokio::time::sleep(POLL).await;
    }
}

async fn read_pid(path: &Path) -> u32 {
    read_nonempty(path).await.parse().expect("fixture pid")
}

async fn topology(path: &Path) -> HashMap<String, u32> {
    read_nonempty(path)
        .await
        .lines()
        .map(|line| {
            let (key, value) = line.split_once('=').expect("topology field");
            (key.to_owned(), value.parse().expect("topology pid"))
        })
        .collect()
}

fn ppid_of(pid: u32) -> Option<u32> {
    process_field(pid, "ppid=")
}

fn pgid_of(pid: u32) -> Option<u32> {
    process_field(pid, "pgid=")
}

fn process_field(pid: u32, field: &str) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", field])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn assert_ppid(pid: u32, expected: u32) {
    assert_eq!(
        ppid_of(pid),
        Some(expected),
        "pid {pid} must have parent {expected}"
    );
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

fn report_params(label: &str) -> String {
    json!({
        "agent": label,
        "event": "UserPromptSubmit",
        "seq": 1,
        "payload": {}
    })
    .to_string()
}

async fn response(path: &Path) -> Value {
    serde_json::from_str(&read_nonempty(path).await).expect("parse hook response")
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_child_hook_is_admitted() {
    if !tmux_available() {
        return;
    }

    let mut fixture = HookRig::start("hookauth-direct", "agent-a").await;
    let mut agent = fixture.start_agent("agent-a").await;
    let result = fixture.path("direct-result.json");
    let topo = fixture.path("direct-topology.txt");
    let ready = fixture.path("direct-ready.txt");
    let send = fixture.path("direct-send.txt");
    let params = report_params("agent-a");

    agent.commands.send(&[
        "child",
        path(&client_path()),
        path(&fixture.socket),
        path(&result),
        path(&topo),
        path(&ready),
        path(&send),
        &params,
    ]);

    let topo = topology(&topo).await;
    assert_ppid(topo["hook_pid"], agent.pid);
    fs::write(&send, "go").expect("release direct hook");
    let response = response(&result).await;
    assert!(response["error"].is_null(), "hook was refused: {response}");

    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sibling_hook_while_foreground_tool_runs_is_admitted() {
    if !tmux_available() {
        return;
    }

    let mut fixture = HookRig::start("hookauth-sibling", "agent-b").await;
    let mut agent = fixture.start_agent("agent-a").await;
    let result = fixture.path("sibling-result.json");
    let topo = fixture.path("sibling-topology.txt");
    let tool_ready = fixture.path("tool-ready.txt");
    let hook_ready = fixture.path("hook-ready.txt");
    let send = fixture.path("sibling-send.txt");
    let params = report_params("agent-b");

    agent.commands.send(&[
        "sibling",
        path(&client_path()),
        path(&fixture.socket),
        path(&topo),
        path(&result),
        path(&tool_ready),
        path(&hook_ready),
        path(&send),
        &params,
    ]);

    let topo = topology(&topo).await;
    let tool_pid = topo["foreground_tool_pid"];
    let hook_pid = topo["hook_pid"];
    assert_ppid(tool_pid, agent.pid);
    assert_ppid(hook_pid, agent.pid);
    assert_ne!(
        pgid_of(tool_pid),
        pgid_of(agent.pid),
        "foreground tool needs its own process group"
    );
    assert_eq!(
        process_field(fixture.controller_pid, "tpgid="),
        Some(tool_pid),
        "foreground tool must own the pane terminal"
    );
    fixture.wait_for_manifest("hookauth").await;

    fs::write(&send, "go").expect("release sibling hook");
    let response = response(&result).await;
    assert!(response["error"].is_null(), "hook was refused: {response}");
    assert_eq!(response["result"]["applied"], true, "{response}");

    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_holder_from_replaced_agent_is_refused() {
    if !tmux_available() {
        return;
    }

    let mut fixture = HookRig::start("hookauth-orphan", "agent-c").await;
    let mut agent_a = fixture.start_agent("agent-a").await;
    let agent_a_pid = agent_a.pid;
    let ready = fixture.path("holder-ready.txt");
    let send = fixture.path("holder-send.txt");
    let result = fixture.path("holder-result.json");
    let params = report_params("agent-c");

    agent_a.commands.send(&[
        "hold-connection",
        path(&client_path()),
        path(&fixture.socket),
        path(&ready),
        path(&send),
        path(&result),
        &params,
    ]);
    let holder_pid = read_pid(&ready).await;
    assert!(process_alive(holder_pid), "connection holder exited early");
    assert_ppid(holder_pid, agent_a_pid);

    agent_a.commands.send(&["exit"]);
    drop(agent_a);
    let exited = fixture.path("agent-a-exited.txt");
    fixture.wait_for_agent_exit(&exited).await;

    let agent_b = fixture.start_agent("agent-b").await;
    fixture.wait_for_current_agent(&agent_b).await;
    assert!(process_alive(holder_pid), "connection holder exited early");
    assert_ne!(
        ppid_of(holder_pid),
        Some(agent_a_pid),
        "connection holder stayed in the old agent tree"
    );

    fs::write(&send, "go").expect("release retained connection");
    let response = response(&result).await;
    assert_eq!(
        response["error"]["code"], "denied",
        "old agent connection was admitted: {response}"
    );

    fixture.shutdown().await;
}
