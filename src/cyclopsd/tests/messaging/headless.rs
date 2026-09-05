//! Headless agents: a process with no pane, registered over the socket
//! and reachable through its mailbox alone.
//!
//! Identity is the whole subject, so every registration here is made from
//! a real process chosen for its ancestry, against a real daemon. The
//! agent is a small binary compiled under a vendor's argv name and started
//! by the test itself, outside every tmux pane; its socket clients run as
//! its children, exactly the shape `cyclops name --self` takes from an
//! agent that has no terminal.

use crate::common;

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::*;
use cyclops_proto::LedgerLine;
use serde_json::{json, Value};

/// This file's own manifest, with names nothing else on a machine is
/// called: `cycagent` is the fixture agent, `cycvendor` a shell wearing an
/// agent's name, and the python socket client is deliberately neither.
const NAMED_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Headless fixture"
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

/// python3 under names this file controls; see `sender_identity.rs`.
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

/// The FIFO-driven agent fixture, compiled once under the vendor's name.
fn agent_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let dir = cyclops_proto::scratch::scratch_dir("cyc-headless-agent-bin");
        std::fs::create_dir_all(&dir).expect("headless agent binary directory");
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
            .expect("compile headless agent fixture");
        assert!(
            output.status.success(),
            "headless agent compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    })
}

fn create_fifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("create agent FIFO");
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

/// A headless agent: the fixture binary started by this test, in no pane,
/// with its command FIFO open. Dropping the guard ends the process.
struct HeadlessAgent {
    child: Child,
    commands: File,
    pid: i32,
}

/// Where the fixture's FIFO and response files live: beside the rig home,
/// never inside it. Boot repairs the state tree and refuses a FIFO there.
fn fixture_dir(home: &Path) -> PathBuf {
    let dir = home.with_file_name(format!(
        "{}-fixture",
        home.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

impl HeadlessAgent {
    async fn start(home: &Path, tag: &str) -> HeadlessAgent {
        let fifo = fixture_dir(home).join(format!("{tag}.fifo"));
        let _ = std::fs::remove_file(&fifo);
        create_fifo(&fifo);
        let child = Command::new(agent_binary())
            .arg(&fifo)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn the headless agent fixture");
        let pid = child.id() as i32;
        let commands = open_fifo_writer(fifo).await;
        HeadlessAgent {
            child,
            commands,
            pid,
        }
    }

    /// One socket request from a child of the agent process.
    async fn request(&mut self, home: &Path, socket: &Path, method: &str, params: Value) -> Value {
        let out = fixture_dir(home).join(format!("{}-{}.json", method, unix_nanos()));
        let _ = std::fs::remove_file(&out);
        writeln!(
            self.commands,
            "request\t{}\t{}\t{}\t{}\t{}",
            client_path(),
            socket.display(),
            out.display(),
            method,
            params
        )
        .expect("write agent request");
        self.commands.flush().expect("flush agent request");
        response(&out).await
    }

    async fn register(&mut self, home: &Path, socket: &Path, label: &str) -> Value {
        self.request(home, socket, "headless.register", json!({"label": label}))
            .await
    }

    /// Tell the fixture to exit, and wait for the process to be gone.
    fn exit(&mut self) {
        let _ = writeln!(self.commands, "exit");
        let _ = self.commands.flush();
        let _ = self.child.wait();
    }
}

impl Drop for HeadlessAgent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
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

/// The notification transition lines for one message, in journal order.
fn transitions(rig: &Rig, message_id: &str) -> Vec<Value> {
    workspace_lines(rig)
        .into_iter()
        .filter(|line: &LedgerLine| line.id == message_id)
        .filter_map(|line| line.data)
        .filter(|data| data["type"] == "notification_transition")
        .collect()
}

/// Send as the operator and return the receipt, or the refusal.
async fn admin_send(rig: &Rig, params: Value) -> Result<Value, cyclops_proto::WireError> {
    let params = serde_json::from_value(params).expect("msg.send params");
    rig.daemon.msg_send("admin", params).await
}

/// A sent message's id, from the receipt.
fn message_id(receipt: &Value) -> String {
    receipt["msg_id"]
        .as_str()
        .expect("accepted message has an id")
        .to_string()
}

/// Wait until the daemon no longer addresses `label`: the route change is
/// the wake, the refusal is the authority.
async fn wait_unaddressable(rig: &mut Rig, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let refused = admin_send(
            rig,
            json!({"to": [label], "subject": "after", "summary": "After the exit.", "body": "b"}),
        )
        .await;
        match refused {
            Err(error) if error.code == "no_such_target" => return,
            Err(error) => panic!("unexpected refusal: {}", error.message),
            Ok(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "{label} stayed addressable after its process exited"
        );
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.route_changed"
            })
            .await;
    }
}

/// The registration is proven from the process tree: an agent started by
/// the test outside every pane registers, and every request its children
/// make is attributed to the label, with nothing in any request naming it.
#[tokio::test(flavor = "multi_thread")]
async fn a_vendor_outside_every_pane_registers_headless_and_sends_as_its_label() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlreg");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlreg", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;
    let socket = rig.daemon.socket_path();
    let home = rig.home.clone();

    let mut agent = HeadlessAgent::start(&home, "worker").await;
    let registered = agent.register(&home, &socket, "worker").await;
    assert!(
        registered["error"].is_null(),
        "the registration must be accepted: {registered}"
    );
    assert_eq!(registered["result"]["headless"], true);
    assert_eq!(registered["result"]["label"], "worker");
    assert_eq!(registered["result"]["recipient"]["kind"], "headless");
    assert_eq!(registered["result"]["pid"], agent.pid);
    assert_eq!(
        registered["result"]["detects_as"], "fix",
        "the root process is classified by its argv: {registered}"
    );
    rig.ev
        .wait_event_named(Duration::from_secs(5), "messages.route_changed", |e| {
            e["event"] == "messages.route_changed"
        })
        .await;

    // Registering again from the same process keeps the same key.
    let again = agent.register(&home, &socket, "worker").await;
    assert_eq!(
        again["result"]["recipient"], registered["result"]["recipient"],
        "the same root keeps its mailbox: {again}"
    );

    let sent = agent
        .request(
            &home,
            &socket,
            "msg.send",
            json!({"to": ["hooky"], "subject": "from-headless", "body": "b"}),
        )
        .await;
    assert!(sent["error"].is_null(), "the send must be accepted: {sent}");
    let from = workspace_lines(&rig)
        .into_iter()
        .find(|line| line.subject.as_deref() == Some("from-headless"))
        .expect("the message reached the workspace journal")
        .from;
    assert_eq!(from, "worker", "the sender is the registered label");

    // The roster shows it as an agent with no pane, and status is unmoved.
    let snapshot = rig
        .ctl
        .request("messages.snapshot", json!({"recent_settled": 5}))
        .await;
    let row = snapshot["result"]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["subject"] == "from-headless")
        .expect("the message is in the snapshot")
        .clone();
    assert_eq!(row["sender"]["kind"], "headless");
    assert_eq!(row["sender_label"], "worker");

    drop(agent);
    rig.shutdown().await;
}

/// A same-user shell with no agent above it is the operator, and the
/// operator cannot register a headless label: nothing is recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_shell_with_no_vendor_ancestor_cannot_register_headless() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlshell");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlshell", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;
    let socket = rig.daemon.socket_path();

    let out = rig.home.join("shell-register.json");
    let status = Command::new(bin.join("cycclient"))
        .arg(client_path())
        .arg(socket.display().to_string())
        .arg(out.display().to_string())
        .arg("headless.register")
        .arg(json!({"label": "worker"}).to_string())
        .status()
        .expect("run the shell client");
    assert!(status.success());
    let resp = response(&out).await;
    assert_eq!(
        resp["error"]["code"], "denied",
        "the operator's shell cannot register: {resp}"
    );
    let refused = admin_send(
        &rig,
        json!({"to": ["worker"], "subject": "nobody", "summary": "Nobody answers.", "body": "b"}),
    )
    .await;
    assert_eq!(
        refused.expect_err("nothing was registered").code,
        "no_such_target"
    );
    rig.shutdown().await;
}

/// A process inside a watched pane is that pane's, and the refusal says
/// so by name: a pane agent uses `cyclops name --self` from its pane.
#[tokio::test(flavor = "multi_thread")]
async fn registering_from_inside_a_watched_pane_is_refused() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlpane");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlpane", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    // The fixture agent in a second pane of the watched session.
    let fifo = rig.home.join("in-pane.fifo");
    create_fifo(&fifo);
    let in_pane = format!("{} {}", agent_binary().display(), fifo.display());
    rig.tmux
        .run_ok(&["split-window", "-t", &pane, "-d", &in_pane]);
    let mut commands = open_fifo_writer(fifo).await;
    let mut agent_pane = None;
    for _ in 0..100 {
        if let Some(p) = rig.pane_ids().await.into_iter().find(|p| p != &pane) {
            agent_pane = Some(p);
            break;
        }
        // No event: a pane whose first verdict is unknown publishes nothing.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let agent_pane = agent_pane.expect("the split pane appears in status");

    let out = rig.home.join("in-pane-register.json");
    let socket = rig.daemon.socket_path();
    writeln!(
        commands,
        "request\t{}\t{}\t{}\theadless.register\t{}",
        client_path(),
        socket.display(),
        out.display(),
        json!({"label": "worker"})
    )
    .expect("write in-pane request");
    commands.flush().expect("flush in-pane request");
    let resp = response(&out).await;
    assert_eq!(
        resp["error"]["code"], "use_pane",
        "a process inside a watched pane is refused: {resp}"
    );
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&agent_pane) && message.contains("cyclops name worker --self"),
        "the refusal names the pane and the pane verb: {message}"
    );

    let _ = writeln!(commands, "exit");
    rig.shutdown().await;
}

/// No pane, no gate, no write: the attempt closes `notified` with transport
/// `mailbox`, no binding and no verifier, and the receipt says the message
/// is in the mailbox with no pane.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_a_headless_recipient_closes_notified_with_transport_mailbox() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlmail");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlmail", NAMED_MANIFEST, &pane_cmd, "").await;
    let socket = rig.daemon.socket_path();
    let home = rig.home.clone();
    let mut agent = HeadlessAgent::start(&home, "worker").await;
    let registered = agent.register(&home, &socket, "worker").await;
    assert!(registered["error"].is_null(), "{registered}");

    let (receipt, _) = rig
        .send(json!({
            "to": ["worker"],
            "subject": "mailbox only",
            "summary": "Read this over the socket.",
            "body": "private body"
        }))
        .await;
    let delivery = &receipt["deliveries"][0];
    assert_eq!(delivery["to"], "worker");
    assert_eq!(delivery["notification_state"], "notified", "{receipt}");
    assert_eq!(delivery["note"], "in mailbox, no pane", "{receipt}");
    assert!(delivery["pane"].is_null(), "{receipt}");

    let id = message_id(&receipt);
    let recorded = transitions(&rig, &id);
    let states: Vec<&str> = recorded
        .iter()
        .filter_map(|data| data["state"].as_str())
        .collect();
    assert_eq!(states, ["queued", "notified"], "{recorded:?}");
    let closed = recorded.last().expect("the notified fact");
    assert_eq!(closed["transport"], "mailbox");
    assert!(closed["binding"].is_null(), "{closed}");
    assert!(closed["verified_by"].is_null(), "{closed}");
    assert!(closed["doorbell_format"].is_null(), "{closed}");

    // The snapshot names it available with a pane-less route.
    let snapshot = rig
        .ctl
        .request("messages.snapshot", json!({"recent_settled": 5}))
        .await;
    let recipient = snapshot["result"]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["message_id"] == id)
        .expect("the row")["recipients"][0]
        .clone();
    assert_eq!(recipient["available"], true, "{recipient}");
    assert_eq!(recipient["current_route"]["label"], "worker");
    assert!(recipient["current_route"]["pane_id"].is_null());
    assert_eq!(recipient["mailbox"]["status"], "pending");
    assert_eq!(recipient["notification"]["state"], "notified");

    // Nothing was written anywhere: the pane's ledger has no line for it.
    assert!(
        !rig.ledger_lines().iter().any(|line| line["id"] == id),
        "a mailbox-only delivery never touches a session ledger"
    );

    drop(agent);
    rig.shutdown().await;
}

/// The agent's own helper claims over the socket and gets the body; the
/// body reaches it through that claim alone (`msg.read` is refused).
#[tokio::test(flavor = "multi_thread")]
async fn a_descendant_of_the_headless_process_claims_over_the_socket() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlclaim");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlclaim", NAMED_MANIFEST, &pane_cmd, "").await;
    let socket = rig.daemon.socket_path();
    let home = rig.home.clone();
    let mut agent = HeadlessAgent::start(&home, "worker").await;
    let registered = agent.register(&home, &socket, "worker").await;
    assert!(registered["error"].is_null(), "{registered}");

    let (receipt, _) = rig
        .send(json!({
            "to": ["worker"],
            "subject": "claim me",
            "summary": "Claim this over the socket.",
            "body": "the private body"
        }))
        .await;
    let id = message_id(&receipt);

    let listed = agent.request(&home, &socket, "inbox.list", json!({})).await;
    assert_eq!(listed["result"]["entries"][0]["message_id"], id, "{listed}");

    let read = agent
        .request(&home, &socket, "msg.read", json!({"message_id": id}))
        .await;
    assert_eq!(
        read["error"]["code"], "forbidden",
        "an agent reads a body only through a claim: {read}"
    );

    let claimed = agent
        .request(&home, &socket, "inbox.claim", json!({"message_id": id}))
        .await;
    assert_eq!(claimed["result"]["disposition"], "claimed", "{claimed}");
    assert_eq!(claimed["result"]["message"]["body"], "the private body");
    assert_eq!(claimed["result"]["message"]["recipient_label"], "worker");

    // The claim settles nothing further: the mailbox close stays notified.
    let states: Vec<String> = transitions(&rig, &id)
        .iter()
        .filter_map(|data| data["state"].as_str().map(String::from))
        .collect();
    assert_eq!(states, ["queued", "notified"]);

    drop(agent);
    rig.shutdown().await;
}

/// The label is released by the process exit itself, through the
/// platform's exit event: no poll, no timer. After it the label is
/// unaddressable, the snapshot row is unavailable, and the pending entry
/// stays where the operator can read it.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_process_exit_retires_its_label() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlexit");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlexit", NAMED_MANIFEST, &pane_cmd, "").await;
    let socket = rig.daemon.socket_path();
    let home = rig.home.clone();
    let mut agent = HeadlessAgent::start(&home, "worker").await;
    let registered = agent.register(&home, &socket, "worker").await;
    assert!(registered["error"].is_null(), "{registered}");

    let (receipt, _) = rig
        .send(json!({
            "to": ["worker"],
            "subject": "before exit",
            "summary": "Sent while alive.",
            "body": "b"
        }))
        .await;
    let id = message_id(&receipt);

    agent.exit();
    wait_unaddressable(&mut rig, "worker").await;

    let snapshot = rig
        .ctl
        .request("messages.snapshot", json!({"recent_settled": 5}))
        .await;
    let recipient = snapshot["result"]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["message_id"] == id)
        .expect("the row")["recipients"][0]
        .clone();
    assert_eq!(recipient["available"], false, "{recipient}");
    assert!(recipient["current_route"].is_null(), "{recipient}");
    assert_eq!(recipient["mailbox"]["status"], "pending");
    assert_eq!(recipient["notification"]["state"], "notified");

    // The operator can still read what was sent.
    let read = rig.ctl.request("msg.read", json!({"message_id": id})).await;
    assert_eq!(read["result"]["body"], "b", "{read}");

    // A fresh registration is a fresh key: the old mailbox is not inherited.
    let mut second = HeadlessAgent::start(&home, "worker-2").await;
    let again = second.register(&home, &socket, "worker").await;
    assert!(again["error"].is_null(), "{again}");
    assert_ne!(
        again["result"]["recipient"], registered["result"]["recipient"],
        "a re-registration mints a new key"
    );
    let listed = second
        .request(&home, &socket, "inbox.list", json!({}))
        .await;
    assert_eq!(
        listed["result"]["entries"].as_array().map(Vec::len),
        Some(0),
        "the old key's entries are not the new registration's: {listed}"
    );

    drop(second);
    rig.shutdown().await;
}

/// Boot keeps a registration only for the same OS boot and a root process
/// still alive at the same birth; everything else is dropped before the
/// first directory is published.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_reverifies_headless_registrations_by_process_generation() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlboot");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let rig = Rig::new("hlboot", NAMED_MANIFEST, &pane_cmd, "").await;
    let socket = rig.daemon.socket_path();
    let home = rig.home.clone();
    let mut agent = HeadlessAgent::start(&home, "worker").await;
    let registered = agent.register(&home, &socket, "worker").await;
    assert!(registered["error"].is_null(), "{registered}");

    // Same boot, same process: the registration survives the restart and
    // the label answers again without re-registering.
    let rig = rig.reboot().await;
    let receipt = admin_send(
        &rig,
        json!({"to": ["worker"], "subject": "after reboot", "summary": "Still here.", "body": "b"}),
    )
    .await
    .expect("the surviving registration is addressable");
    assert_eq!(receipt["deliveries"][0]["note"], "in mailbox, no pane");
    let socket = rig.daemon.socket_path();
    let sent = agent
        .request(
            &home,
            &socket,
            "msg.send",
            json!({"to": ["admin"], "subject": "survived", "body": "b"}),
        )
        .await;
    assert!(sent["error"].is_null(), "{sent}");

    // Another boot: the row's process generation means nothing, so the
    // registration is dropped even though the process is still running.
    let registry = home.join("registry.json");
    let mut file: Value =
        serde_json::from_str(&std::fs::read_to_string(&registry).expect("registry readable"))
            .expect("registry parses");
    assert_eq!(file["headless"].as_array().map(Vec::len), Some(1));
    file["headless"][0]["os_boot_id"] = json!("another-boot");
    std::fs::write(&registry, serde_json::to_string_pretty(&file).unwrap()).unwrap();
    let rig = rig.reboot().await;
    let refused = admin_send(
        &rig,
        json!({"to": ["worker"], "subject": "gone", "summary": "Gone.", "body": "b"}),
    )
    .await;
    assert_eq!(
        refused
            .expect_err("a row from another boot is dropped")
            .code,
        "no_such_target"
    );

    // Same boot, dead process: re-register, then make the row name a
    // process that has exited, and reboot.
    let socket = rig.daemon.socket_path();
    let registered = agent.register(&home, &socket, "worker").await;
    assert!(registered["error"].is_null(), "{registered}");
    let mut file: Value =
        serde_json::from_str(&std::fs::read_to_string(&registry).expect("registry readable"))
            .expect("registry parses");
    let dead = Command::new("true").status().expect("run true");
    assert!(dead.success());
    file["headless"][0]["root"]["birth"] = json!(1);
    std::fs::write(&registry, serde_json::to_string_pretty(&file).unwrap()).unwrap();
    let rig = rig.reboot().await;
    let refused = admin_send(
        &rig,
        json!({"to": ["worker"], "subject": "dead", "summary": "Dead.", "body": "b"}),
    )
    .await;
    assert_eq!(
        refused
            .expect_err("a row for a dead generation is dropped")
            .code,
        "no_such_target"
    );

    drop(agent);
    rig.shutdown().await;
}

/// One roster, one namespace: a headless label cannot take a pane's name
/// and a pane cannot take a headless one.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_label_cannot_collide_with_a_pane_label() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlcollide");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlcollide", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;
    let socket = rig.daemon.socket_path();
    let home = rig.home.clone();
    let mut agent = HeadlessAgent::start(&home, "worker").await;

    let taken = agent.register(&home, &socket, "hooky").await;
    assert_eq!(taken["error"]["code"], "bad_request", "{taken}");
    assert!(
        taken["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already taken"),
        "{taken}"
    );

    let registered = agent.register(&home, &socket, "worker").await;
    assert!(registered["error"].is_null(), "{registered}");
    let renamed = rig
        .ctl
        .request("pane.label", json!({"target": &pane, "label": "worker"}))
        .await;
    assert_eq!(renamed["error"]["code"], "bad_request", "{renamed}");
    assert!(
        renamed["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("headless agent"),
        "{renamed}"
    );

    drop(agent);
    rig.shutdown().await;
}

/// `*` means every agent, pane or not.
#[tokio::test(flavor = "multi_thread")]
async fn broadcast_reaches_headless_recipients() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let bin = named_bin("hlall");
    let pane_cmd = format!("{}/cycagent {}", bin.display(), faketui_path());
    let mut rig = Rig::new("hlall", NAMED_MANIFEST, &pane_cmd, "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;
    let socket = rig.daemon.socket_path();
    let home = rig.home.clone();
    let mut agent = HeadlessAgent::start(&home, "worker").await;
    let registered = agent.register(&home, &socket, "worker").await;
    assert!(registered["error"].is_null(), "{registered}");

    let (receipt, _) = rig
        .send(json!({
            "to": ["*"],
            "subject": "to all",
            "summary": "Everyone reads this.",
            "body": "b"
        }))
        .await;
    let mut labels: Vec<String> = receipt["deliveries"]
        .as_array()
        .expect("deliveries")
        .iter()
        .map(|d| d["to"].as_str().unwrap_or_default().to_string())
        .collect();
    labels.sort();
    assert_eq!(labels, ["hooky", "worker"], "{receipt}");
    let worker = receipt["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["to"] == "worker")
        .unwrap();
    assert_eq!(worker["note"], "in mailbox, no pane");

    let listed = agent.request(&home, &socket, "inbox.list", json!({})).await;
    assert_eq!(
        listed["result"]["entries"][0]["subject"], "to all",
        "{listed}"
    );

    drop(agent);
    rig.shutdown().await;
}
