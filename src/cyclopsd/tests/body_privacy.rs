//! Workspace message bodies follow durable sender and claim authority.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const IDENTITY_MANIFEST: &str = r#"
[agent]
id = "body-privacy-fixture"
display_name = "Body privacy fixture"
process_names = ["Python", "python3"]
argv_basenames = ["cycagent"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']
"#;

fn client_path() -> String {
    format!(
        "{}/tests/common/socket_client.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn named_clients(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}-bin"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let output = Command::new("sh")
        .args(["-c", "command -v python3"])
        .output()
        .expect("look up python3");
    let python = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(!python.is_empty(), "python3 must be on PATH");
    for name in ["cycagent", "cycclient"] {
        let link = dir.join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).expect("remove stale client link");
        }
        std::os::unix::fs::symlink(&python, link).expect("symlink python3");
    }
    dir
}

fn agent_command_loop(client_dir: &Path) -> String {
    format!(
        "{}/cycagent -u -c 'import shlex,subprocess,sys; [subprocess.run(shlex.split(line)) for line in sys.stdin]'",
        client_dir.display()
    )
}

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

async fn pane_request(
    rig: &mut Rig,
    client_dir: &Path,
    pane: &str,
    tag: &str,
    method: &str,
    params: Value,
) -> Value {
    let out = rig.home.join(format!("{tag}.json"));
    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        pane,
        &format!(
            "{}/cycclient {} {} {} {} '{}'",
            client_dir.display(),
            client_path(),
            rig.daemon.socket_path().display(),
            out.display(),
            method,
            params
        ),
        "Enter",
    ]);
    response(&out).await
}

async fn wait_manifest_bound(rig: &mut Rig, pane_count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let panes = status["result"]["sessions"][0]["panes"]
            .as_array()
            .unwrap_or_else(|| panic!("no panes in {status}"));
        if panes.len() == pane_count
            && panes
                .iter()
                .all(|pane| pane["manifest"] == "body-privacy-fixture")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture panes never bound the identity manifest: {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn message_line<'a>(response: &'a Value, message_id: &str) -> &'a Value {
    response["result"]["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("no lines in {response}"))
        .iter()
        .find(|line| {
            matches!(line["kind"].as_str(), Some("msg" | "fyi")) && line["id"] == message_id
        })
        .unwrap_or_else(|| panic!("message {message_id} missing from {response}"))
}

fn workspace_lines(rig: &Rig) -> Vec<Value> {
    let workspace = fs::read_dir(rig.home.join("workspaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = fs::read_to_string(workspace.join("messages.ndjson")).unwrap();
    // Ignore only a final line that a concurrent writer has not terminated yet.
    let complete = if raw.ends_with('\n') {
        raw.as_str()
    } else {
        raw.rsplit_once('\n').map_or("", |(lines, _)| lines)
    };
    complete
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn notification_attempts(rig: &Rig, message_id: &str) -> BTreeSet<String> {
    workspace_lines(rig)
        .into_iter()
        .filter_map(|line| {
            (line["id"] == message_id && line["data"]["type"] == "notification_transition")
                .then(|| line["data"]["attempt_id"].as_str().unwrap().to_string())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn history_and_thread_release_bodies_only_after_the_exact_claim() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let client_dir = named_clients("body-privacy");
    let pane_command = agent_command_loop(&client_dir);
    let mut rig = Rig::new("bodyprivacy", IDENTITY_MANIFEST, &pane_command, "").await;
    let first = rig.pane_ids().await[0].clone();
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", &first, &pane_command]);
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", &first, &pane_command]);
    rig.wait_attached(3).await;
    wait_manifest_bound(&mut rig, 3).await;
    let panes = rig.pane_ids().await;
    let sender = panes[0].clone();
    let recipient = panes[1].clone();
    let observer = panes[2].clone();
    rig.label(&sender, "sender").await;
    rig.label(&recipient, "recipient").await;
    rig.label(&observer, "observer").await;

    let secret = "workspace body only exact participants may read";
    let sent = pane_request(
        &mut rig,
        &client_dir,
        &sender,
        "send",
        "msg.send",
        json!({"to": ["recipient"], "subject": "private", "body": secret}),
    )
    .await;
    assert!(sent["error"].is_null(), "send failed: {sent}");
    let message_id = sent["result"]["msg_id"]
        .as_str()
        .expect("accepted message id")
        .to_string();

    let recipient_history = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "recipient-history-before",
        "msg.history",
        json!({}),
    )
    .await;
    assert!(
        message_line(&recipient_history, &message_id)
            .get("body")
            .is_none(),
        "an unclaimed recipient saw the body: {recipient_history}"
    );
    let recipient_thread = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "recipient-thread-before",
        "msg.thread",
        json!({"id": message_id}),
    )
    .await;
    assert!(
        message_line(&recipient_thread, &message_id)
            .get("body")
            .is_none(),
        "an unclaimed recipient saw the thread body: {recipient_thread}"
    );

    let sender_history = pane_request(
        &mut rig,
        &client_dir,
        &sender,
        "sender-history",
        "msg.history",
        json!({}),
    )
    .await;
    assert_eq!(
        message_line(&sender_history, &message_id)["body"],
        secret,
        "the sender lost its authored body"
    );
    let sender_thread = pane_request(
        &mut rig,
        &client_dir,
        &sender,
        "sender-thread",
        "msg.thread",
        json!({"id": message_id}),
    )
    .await;
    assert_eq!(message_line(&sender_thread, &message_id)["body"], secret);

    let admin_history = rig.ctl.request("msg.history", json!({})).await;
    assert!(
        message_line(&admin_history, &message_id)
            .get("body")
            .is_none(),
        "an observing admin saw another sender's body: {admin_history}"
    );
    let admin_thread = rig
        .ctl
        .request("msg.thread", json!({"id": message_id}))
        .await;
    assert!(message_line(&admin_thread, &message_id)
        .get("body")
        .is_none());

    let observer_history = pane_request(
        &mut rig,
        &client_dir,
        &observer,
        "observer-history",
        "msg.history",
        json!({}),
    )
    .await;
    assert!(
        !observer_history["result"]["lines"]
            .as_array()
            .expect("history lines")
            .iter()
            .any(|line| line["id"] == message_id),
        "an inaccessible message appeared in history: {observer_history}"
    );
    let observer_thread = pane_request(
        &mut rig,
        &client_dir,
        &observer,
        "observer-thread",
        "msg.thread",
        json!({"id": message_id}),
    )
    .await;
    assert_eq!(observer_thread["error"]["code"], "no_such_message");

    let claimed = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "recipient-claim",
        "inbox.claim",
        json!({"message_id": message_id}),
    )
    .await;
    assert!(claimed["error"].is_null(), "claim failed: {claimed}");
    assert_eq!(claimed["result"]["message"]["body"], secret);

    let recipient_history = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "recipient-history-after",
        "msg.history",
        json!({}),
    )
    .await;
    assert_eq!(
        message_line(&recipient_history, &message_id)["body"],
        secret
    );
    let recipient_thread = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "recipient-thread-after",
        "msg.thread",
        json!({"id": message_id}),
    )
    .await;
    assert_eq!(message_line(&recipient_thread, &message_id)["body"], secret);

    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unadopted_pane_cannot_claim_its_former_mailbox_body() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let client_dir = named_clients("unadopted-claim");
    let pane_command = agent_command_loop(&client_dir);
    let mut rig = Rig::new("unadopted-claim", IDENTITY_MANIFEST, &pane_command, "").await;
    wait_manifest_bound(&mut rig, 1).await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "recipient").await;

    let secret = "body must remain sealed after adoption is cleared";
    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["recipient"],
                "subject": "Private",
                "body": secret,
                "client_key": "unadopted-claim"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();

    let cleared = rig
        .ctl
        .request("pane.label", json!({"target": pane, "label": null}))
        .await;
    assert!(cleared["error"].is_null(), "clear failed: {cleared}");

    let denied = pane_request(
        &mut rig,
        &client_dir,
        &pane,
        "claim-after-clear",
        "inbox.claim",
        json!({"message_id": message_id}),
    )
    .await;
    assert_eq!(denied["error"]["code"], "denied");
    assert!(
        !denied.to_string().contains(secret),
        "an unadopted caller received mailbox body bytes: {denied}"
    );

    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claiming_the_oldest_withdraws_only_its_attempt_and_schedules_the_next() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let client_dir = named_clients("claim-schedules-next");
    let pane_command = agent_command_loop(&client_dir);
    let mut rig = Rig::new("claim-schedules-next", IDENTITY_MANIFEST, &pane_command, "").await;
    wait_manifest_bound(&mut rig, 1).await;
    let recipient = rig.pane_ids().await[0].clone();
    rig.label(&recipient, "recipient").await;

    let first = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["recipient"],
                "subject": "First",
                "body": "first private body",
                "client_key": "claim-first"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    let second = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["recipient"],
                "subject": "Second",
                "body": "second private body",
                "client_key": "claim-second"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    assert_eq!(notification_attempts(&rig, &first_id).len(), 1);
    assert!(notification_attempts(&rig, &second_id).is_empty());

    let claimed = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "claim-first",
        "inbox.claim",
        json!({"message_id": first_id}),
    )
    .await;
    assert!(claimed["error"].is_null(), "claim failed: {claimed}");
    assert_eq!(claimed["result"]["disposition"], "claimed");
    assert_eq!(claimed["result"]["message"]["body"], "first private body");

    let snapshot = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "snapshot-after-claim",
        "messages.snapshot",
        json!({}),
    )
    .await;
    assert!(snapshot["error"].is_null(), "snapshot failed: {snapshot}");
    let rows = snapshot["result"]["rows"].as_array().unwrap();
    let first_row = rows
        .iter()
        .find(|row| row["message_id"] == first_id)
        .unwrap();
    let second_row = rows
        .iter()
        .find(|row| row["message_id"] == second_id)
        .unwrap();
    assert_eq!(first_row["recipients"][0]["mailbox"]["status"], "claimed");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"],
        "superseded"
    );
    assert_eq!(second_row["recipients"][0]["mailbox"]["status"], "pending");
    assert!(matches!(
        second_row["recipients"][0]["notification"]["state"].as_str(),
        Some("queued" | "gating")
    ));
    assert_eq!(notification_attempts(&rig, &first_id).len(), 1);
    assert_eq!(notification_attempts(&rig, &second_id).len(), 1);
    assert_ne!(
        notification_attempts(&rig, &first_id),
        notification_attempts(&rig, &second_id)
    );

    rig.shutdown().await;
}
