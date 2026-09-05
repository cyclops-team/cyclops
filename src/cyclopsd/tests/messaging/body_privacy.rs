//! Workspace message bodies follow durable sender and claim authority.

use crate::common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use cyclops_proto::NotificationState;
use serde_json::{json, Value};

pub(crate) const IDENTITY_MANIFEST: &str = r#"
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

pub(crate) fn named_clients(tag: &str) -> PathBuf {
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

pub(crate) fn agent_command_loop(client_dir: &Path) -> String {
    format!(
        "{}/cycagent -u -c 'import shlex,subprocess,sys\nfor line in sys.stdin:\n try: subprocess.run(shlex.split(line))\n except Exception: pass'",
        client_dir.display()
    )
}

fn codex_ghost_agent_command_loop(client_dir: &Path) -> String {
    format!(
        "{}/cycagent -u {}/tests/common/socket_agent.py",
        client_dir.display(),
        env!("CARGO_MANIFEST_DIR")
    )
}

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

pub(crate) async fn pane_request(
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

pub(crate) async fn wait_manifest_bound(rig: &mut Rig, pane_count: usize) {
    wait_manifest_bound_to(rig, pane_count, "body-privacy-fixture").await;
}

/// Wait until every fixture pane is bound to the manifest `manifest_id`.
pub(crate) async fn wait_manifest_bound_to(rig: &mut Rig, pane_count: usize, manifest_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let panes = status["result"]["sessions"][0]["panes"]
            .as_array()
            .unwrap_or_else(|| panic!("no panes in {status}"));
        if panes.len() == pane_count && panes.iter().all(|pane| pane["manifest"] == manifest_id) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture panes never bound the identity manifest: {status}"
        );
        // No event: binding a manifest publishes nothing, and status blanks
        // `manifest` while its live refresh is incomplete.
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

fn notification_state_count(rig: &Rig, message_id: &str, state: &str) -> usize {
    workspace_lines(rig)
        .into_iter()
        .filter(|line| {
            line["id"] == message_id
                && line["data"]["type"] == "notification_transition"
                && line["data"]["state"] == state
        })
        .count()
}

async fn wait_for_notification_state(rig: &mut Rig, message_id: &str, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if notification_state_count(rig, message_id, state) > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} did not reach {state}: {:#?}",
            workspace_lines(rig)
                .into_iter()
                .filter(|line| line["id"] == message_id)
                .collect::<Vec<_>>()
        );
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
    }
}

pub(crate) async fn wait_for_notification_submitted(rig: &mut Rig, message_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if notification_state_count(rig, message_id, "submitted") > 0
            || notification_state_count(rig, message_id, "submitted_unverified") > 0
            || notification_state_count(rig, message_id, "notified") > 0
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} was not submitted: {:#?}",
            workspace_lines(rig)
                .into_iter()
                .filter(|line| line["id"] == message_id)
                .collect::<Vec<_>>()
        );
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
    }
}

async fn wait_for_notification_attempt(rig: &mut Rig, message_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(attempt) = notification_attempts(rig, message_id).into_iter().next() {
            return attempt;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} never started: {:#?}",
            workspace_lines(rig)
                .into_iter()
                .filter(|line| line["id"] == message_id)
                .collect::<Vec<_>>()
        );
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
    }
}

fn compact_doorbell(attempt_id: &str) -> String {
    cyclops_proto::render_doorbell_v3(
        cyclops_proto::NotificationAttemptId::parse(attempt_id).unwrap(),
    )
}

fn pane_history(rig: &Rig, pane: &str) -> String {
    let output = rig.tmux.run(&["capture-pane", "-p", "-S", "-", "-t", pane]);
    assert!(
        output.status.success(),
        "capture pane history failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
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
    wait_for_notification_submitted(&mut rig, &message_id).await;

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

    let admin_thread_full = rig
        .ctl
        .request("msg.thread", json!({"id": message_id, "body": true}))
        .await;
    assert_eq!(
        message_line(&admin_thread_full, &message_id)["body"],
        secret,
        "an inspecting admin with body: true must be authorized to inspect the message body"
    );

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

    wait_for_notification_submitted(&mut rig, &first_id).await;

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
    // The claim frees the FIFO and the second doorbell rings at once; the
    // next request is typed only after that row has landed.
    wait_for_notification_submitted(&mut rig, &second_id).await;

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
    assert_eq!(second_row["recipients"][0]["mailbox"]["status"], "pending");
    let second_attempt = wait_for_notification_attempt(&mut rig, &second_id).await;
    assert_eq!(notification_attempts(&rig, &first_id).len(), 1);
    assert_eq!(notification_attempts(&rig, &second_id).len(), 1);
    assert_ne!(
        notification_attempts(&rig, &first_id),
        BTreeSet::from([second_attempt])
    );

    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_claim_withdraws_a_blocked_attempt_and_releases_the_fifo() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = include_str!("../../../../resources/manifests/codex.toml")
        .replace(
            "process_names = [\"codex\"]",
            "process_names = [\"Python\", \"python3\"]\nargv_basenames = [\"cycagent\"]",
        )
        .replace(
            "[messaging]\nmailbox_capability_file = \"~/.agents/skills/cyclops/SKILL.md\"\n",
            "",
        );
    let client_dir = named_clients("blocked-claim-releases-fifo");
    let pane_command = codex_ghost_agent_command_loop(&client_dir);
    let mut rig = Rig::new(
        "blocked-claim-releases-fifo",
        &manifest,
        &pane_command,
        "delivery_retry_max = 0\n",
    )
    .await;
    wait_pane_state(&mut rig, "idle").await;
    let recipient = rig.pane_ids().await[0].clone();
    rig.label(&recipient, "recipient").await;

    rig.daemon.fail_next_final_binding_observation();
    let first = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["recipient"],
                "subject": "First",
                "body": "first private body",
                "client_key": "blocked-claim-first"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &first_id, "blocked_pre_write").await;
    let first_attempt = notification_attempts(&rig, &first_id)
        .into_iter()
        .next()
        .expect("blocked notification keeps its attempt identity");
    let blocked_state = serde_json::to_value(NotificationState::BlockedPreWrite).unwrap();
    let blocked = workspace_lines(&rig)
        .into_iter()
        .find(|line| {
            line["id"] == first_id
                && line["data"]["type"] == "notification_transition"
                && line["data"]["state"] == blocked_state
        })
        .expect("blocked transition is durable");
    assert_eq!(
        blocked["data"]["pre_write_cause"],
        "write_readiness_changed"
    );
    assert_eq!(notification_state_count(&rig, &first_id, "writing"), 0);
    assert!(!pane_history(&rig, &recipient).contains(&compact_doorbell(&first_attempt)));

    let second = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["recipient"],
                "subject": "Second",
                "body": "second private body",
                "client_key": "blocked-claim-second"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    assert!(notification_attempts(&rig, &second_id).is_empty());

    // The command runs as a descendant of the exact watched cycagent pane.
    // The fixture paints Codex Working before opening the socket, so releasing
    // the FIFO cannot let the second attempt write while this claim runs.
    let claimed = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "claim-blocked-first",
        "inbox.claim",
        json!({"message_id": first_id}),
    )
    .await;
    assert!(claimed["error"].is_null(), "claim failed: {claimed}");
    assert_eq!(claimed["result"]["disposition"], "claimed");
    assert_eq!(claimed["result"]["message"]["body"], "first private body");
    assert!(workspace_lines(&rig)
        .into_iter()
        .any(|line| { line["id"] == first_id && line["data"]["type"] == "message_claimed" }));

    // The claim withdrew the blocked attempt: the recipient holds the body,
    // so the wake has nothing left to announce and the FIFO moves on. Read
    // the projection over the admin socket: the second doorbell may now
    // land in the loop pane and must not race a command typed there.
    let lines_after_claim = workspace_lines(&rig).len();
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    assert!(snapshot["error"].is_null(), "snapshot failed: {snapshot}");
    let rows = snapshot["result"]["rows"].as_array().unwrap();
    let first_row = rows
        .iter()
        .find(|row| row["message_id"] == first_id)
        .expect("claimed first message remains visible");
    assert_eq!(first_row["recipients"][0]["mailbox"]["status"], "claimed");
    let first_notification = &first_row["recipients"][0]["notification"];
    assert_eq!(first_notification["state"], "not_started", "{first_row}");
    assert_eq!(first_notification["settlement"], "withdrawn_by_claim");
    assert_eq!(
        first_notification["pre_write_cause"],
        "claimed_before_write"
    );

    let second_attempt = wait_for_notification_attempt(&mut rig, &second_id).await;
    wait_for_notification_state(&mut rig, &second_id, "writing").await;
    assert_eq!(notification_attempts(&rig, &first_id).len(), 1);
    assert_eq!(notification_attempts(&rig, &second_id).len(), 1);
    assert_ne!(first_attempt, second_attempt);
    assert_eq!(notification_state_count(&rig, &first_id, "writing"), 0);
    assert!(notification_state_count(&rig, &second_id, "writing") > 0);

    // The operator withdrawing the already-withdrawn attempt is answered
    // with the record and appends nothing.
    let lines_before_withdraw = workspace_lines(&rig).len();
    let withdrawn = rig
        .ctl
        .request(
            "notification.withdraw",
            json!({
                "attempt_id": first_attempt,
                "recipient": first_row["recipients"][0]["recipient"].clone()
            }),
        )
        .await;
    assert!(withdrawn["error"].is_null(), "{withdrawn}");
    let first_lines = |lines: &[Value]| lines.iter().filter(|l| l["id"] == first_id).count();
    assert_eq!(
        first_lines(&workspace_lines(&rig)),
        first_lines(&workspace_lines(&rig)[..lines_before_withdraw]),
        "the operator withdrawal appended a fact for the withdrawn attempt"
    );
    assert!(lines_after_claim <= lines_before_withdraw);
    let history = pane_history(&rig, &recipient);
    assert!(!history.contains(&compact_doorbell(&first_attempt)));

    rig.shutdown().await;
}

/// `msg.read` is the operator's read: the admin origin gets the body
/// without claiming, the message stays pending for its recipient, and the
/// read appends nothing. Every agent caller is refused, the message's own
/// recipient included, because a body reaches an agent only through a
/// claim.
#[tokio::test(flavor = "multi_thread")]
async fn the_operator_reads_a_body_without_claiming_and_an_agent_is_refused() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let client_dir = named_clients("msg-read");
    let pane_command = agent_command_loop(&client_dir);
    let mut rig = Rig::new("msgread", IDENTITY_MANIFEST, &pane_command, "").await;
    let first = rig.pane_ids().await[0].clone();
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", &first, &pane_command]);
    rig.wait_attached(2).await;
    wait_manifest_bound(&mut rig, 2).await;
    let panes = rig.pane_ids().await;
    let sender = panes[0].clone();
    let recipient = panes[1].clone();
    rig.label(&sender, "sender").await;
    rig.label(&recipient, "recipient").await;

    let secret = "operator-readable body the recipient has not claimed";
    let sent = pane_request(
        &mut rig,
        &client_dir,
        &sender,
        "read-send",
        "msg.send",
        json!({
            "to": ["recipient"],
            "subject": "private",
            "summary": "Read this from the mailbox.",
            "body": secret
        }),
    )
    .await;
    assert!(sent["error"].is_null(), "send failed: {sent}");
    let message_id = sent["result"]["msg_id"]
        .as_str()
        .expect("accepted message id")
        .to_string();
    assert_eq!(sent["result"]["thread_root"], message_id);
    wait_for_notification_submitted(&mut rig, &message_id).await;

    // The operator reads the body without claiming and without appending.
    let lines_before = workspace_lines(&rig).len();
    let read = rig
        .ctl
        .request("msg.read", json!({"message_id": message_id}))
        .await;
    assert!(read["error"].is_null(), "operator read failed: {read}");
    assert_eq!(read["result"]["message_id"], message_id);
    assert_eq!(read["result"]["body"], secret);
    assert_eq!(read["result"]["sender_label"], "sender");
    assert_eq!(read["result"]["recipient_label"], "recipient");
    assert_eq!(read["result"]["thread_root"], message_id);
    assert_eq!(
        workspace_lines(&rig).len(),
        lines_before,
        "msg.read appended"
    );
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let row = snapshot["result"]["rows"]
        .as_array()
        .expect("snapshot rows")
        .iter()
        .find(|row| row["message_id"] == message_id)
        .expect("the message stays visible");
    assert_eq!(
        row["recipients"][0]["mailbox"]["status"], "pending",
        "the operator's read claimed the message: {row}"
    );

    // The doorbell's own locator resolves to the same message.
    let doorbell = doorbell_for(&rig, &message_id);
    let locator = doorbell.rsplit(' ').next().expect("locator");
    let read_by_locator = rig
        .ctl
        .request("msg.read", json!({"message_id": locator}))
        .await;
    assert_eq!(read_by_locator["result"]["message_id"], message_id);
    assert_eq!(read_by_locator["result"]["body"], secret);

    // Agents are refused: the recipient, and the sender who authored it.
    for (pane, tag) in [(&recipient, "recipient-read"), (&sender, "sender-read")] {
        let refused = pane_request(
            &mut rig,
            &client_dir,
            pane,
            tag,
            "msg.read",
            json!({"message_id": message_id}),
        )
        .await;
        assert!(
            refused["result"].is_null(),
            "an agent read a body: {refused}"
        );
        assert_eq!(refused["error"]["code"], "forbidden", "{refused}");
        assert_eq!(
            refused["error"]["message"],
            "bodies reach an agent only through a claim"
        );
    }

    // The operator's read left the claim to the recipient.
    let claimed = pane_request(
        &mut rig,
        &client_dir,
        &recipient,
        "recipient-claim",
        "inbox.claim",
        json!({"message_id": message_id}),
    )
    .await;
    assert_eq!(claimed["result"]["disposition"], "claimed", "{claimed}");
    assert_eq!(claimed["result"]["message"]["body"], secret);

    rig.daemon.shutdown().await;
}
