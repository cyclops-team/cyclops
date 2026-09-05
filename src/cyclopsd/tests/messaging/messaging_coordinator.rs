use crate::common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    composer_pane, faketui_path, hold_script, swallowing_animated_composer_pane, tmux_available,
    wait_pane_state, HomeGuard, Rig, TestClient, CAT_MANIFEST, LIVENESS_MANIFEST, MODAL_MANIFEST,
};
use cyclops_proto::{
    ComposerHold, Kind, LedgerLine, MsgSendParams, NotificationAttemptId, NotificationState,
    RecipientKey,
};
use serde_json::{json, Value};
use tokio::net::UnixDatagram;

/// The escaped Codex composer distinction from the live `/model` incident:
/// dim text is a ghost suggestion, while bare text is a human draft. The
/// fixture deliberately returns to a ghost without emitting a turn, which is
/// enough to prove this must become a durable pre-write block rather than an
/// in-memory wait.
const ESC_COMPOSER_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Escaped composer fixture"
process_names = ["python3", "python", "Python", "cat", "sh", "bash", "dash"]

[[rule]]
id = "title_working"
state = "working"
priority = 1200
region = "pane_title"
regex = ['^WORKING']

[[rule]]
id = "composer_typed_input"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+[^\x1b\s]']

[[rule]]
id = "composer_ghost_suggestion"
state = "idle"
composer_semantic = "ghost_suggestion"
priority = 1040
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+\x1b\[2m']

[[rule]]
id = "composer_empty_or_ghost"
state = "idle"
composer_semantic = "ambiguous"
priority = 1000
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*›']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
"#;

fn workspace_lines(rig: &Rig) -> Vec<LedgerLine> {
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
            let data = line.data?;
            (line.id == message_id && data["type"] == "notification_transition")
                .then(|| data["attempt_id"].as_str().unwrap().to_string())
        })
        .collect()
}

fn notification_transition(
    rig: &Rig,
    message_id: &str,
    state: NotificationState,
) -> Option<LedgerLine> {
    workspace_lines(rig).into_iter().find(|line| {
        line.id == message_id
            && line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_transition"
                    && serde_json::from_value::<NotificationState>(data["state"].clone())
                        .is_ok_and(|actual| actual == state)
            })
    })
}

fn notification_state_count(rig: &Rig, message_id: &str, state: NotificationState) -> usize {
    workspace_lines(rig)
        .into_iter()
        .filter(|line| {
            line.id == message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition"
                        && serde_json::from_value::<NotificationState>(data["state"].clone())
                            .is_ok_and(|actual| actual == state)
                })
        })
        .count()
}

async fn wait_for_notification_state(rig: &mut Rig, message_id: &str, state: NotificationState) {
    // The screen receipt deadline itself is five seconds. Leave room for the
    // resulting journal append and event publication before declaring failure.
    let deadline = Instant::now() + Duration::from_secs(8);
    let transition = loop {
        if let Some(line) = notification_transition(rig, message_id, state) {
            break line;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} did not reach {state:?}: {:#?}",
            workspace_lines(rig)
                .into_iter()
                .filter(|line| line.id == message_id)
                .collect::<Vec<_>>()
        );
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        // The exact-seq wait below is the contract that this transition's
        // event was published.
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
    };

    rig.ev
        .wait_event(Duration::from_secs(5), |event| {
            event["event"] == "messages.changed"
                && event["seq"] == transition.seq
                && event["data"]["changed"]
                    .as_array()
                    .is_some_and(|areas| areas.iter().any(|area| area == "notifications"))
        })
        .await;
}

/// The pasted row carries the sender-authored preview; these tests assert
/// that neither subject nor body ever reaches the pane, so the preview is
/// fixed and content-free.
async fn send_workspace_message(rig: &Rig, client_key: &str, subject: &str, body: &str) -> Value {
    rig.daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": subject,
                "summary": "A message is waiting. Claim it from the mailbox.",
                "body": body,
                "client_key": client_key
            }))
            .unwrap(),
        )
        .await
        .unwrap()
}

async fn send_summarized_workspace_message(
    rig: &Rig,
    client_key: &str,
    subject: &str,
    summary: &str,
    body: &str,
) -> Value {
    rig.daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": subject,
                "summary": summary,
                "body": body,
                "client_key": client_key
            }))
            .unwrap(),
        )
        .await
        .unwrap()
}

struct WaitingPair {
    first: String,
    second: String,
    first_receipt: Value,
    second_receipt: Value,
}

async fn send_waiting_pair(rig: &Rig, key_prefix: &str) -> WaitingPair {
    let first =
        send_workspace_message(rig, &format!("{key_prefix}-first"), "First", "first body").await;
    let second = send_workspace_message(
        rig,
        &format!("{key_prefix}-second"),
        "Second",
        "second body",
    )
    .await;
    assert_eq!(second["deliveries"][0]["notification_state"], "not_started");
    WaitingPair {
        first: first["msg_id"].as_str().unwrap().to_string(),
        second: second["msg_id"].as_str().unwrap().to_string(),
        first_receipt: first["deliveries"][0].clone(),
        second_receipt: second["deliveries"][0].clone(),
    }
}

fn assert_only_oldest_attempt_exists(rig: &Rig, pair: &WaitingPair) {
    assert_eq!(notification_attempts(rig, &pair.first).len(), 1);
    assert!(notification_attempts(rig, &pair.second).is_empty());
}

async fn wait_for_doorbell(rig: &Rig, pane: &str, message_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = rig.tmux.capture(pane);
        if let Some(expected) = current_compact_doorbell(rig, message_id) {
            if screen.contains(&expected) {
                return screen;
            }
        }
        assert!(
            Instant::now() < deadline,
            "doorbell was not shown: {screen}"
        );
        // No event: the row is read from a tmux capture; the journal says it
        // was written, not that it is on screen.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A direct pane read synchronously refreshes this one live pane without the
/// whole-status refresh budget. It proves the ordinary post-paste observation
/// still recognizes the exact doorbell while the old turn is Working,
/// including the `StagedDuringTurn` composer barrier that prompted the
/// final-Enter regression.
async fn refresh_exact_working_doorbell(rig: &mut Rig, pane: &str) -> Value {
    let read = rig
        .ctl
        .request("pane.read", json!({"target": pane, "source": "detection"}))
        .await;
    assert!(read["error"].is_null(), "{read}");
    let detection = &read["result"]["detection"];
    assert_eq!(detection["state"], "working", "{read}");
    assert!(
        detection["readings"].as_array().is_some_and(|readings| {
            readings
                .iter()
                .any(|reading| reading["sensor"] == "screen" && reading["state"] == "working")
        }),
        "the direct read must see the current Working screen row: {read}"
    );
    assert_eq!(
        rig.daemon
            .composer_hold_for_test(0, pane)
            .map(|(hold, _)| hold),
        Some(ComposerHold::StagedDuringTurn),
        "the refreshed Working pane must retain the exact staged doorbell"
    );
    read
}

fn current_compact_doorbell(rig: &Rig, message_id: &str) -> Option<String> {
    current_notification_attempt(&workspace_lines(rig), message_id)
        .map(cyclops_proto::render_doorbell_v3)
}

fn current_notification_attempt(
    lines: &[LedgerLine],
    message_id: &str,
) -> Option<NotificationAttemptId> {
    let mut current_by_recipient = BTreeMap::<RecipientKey, NotificationAttemptId>::new();
    for line in lines.iter().filter(|line| line.id == message_id) {
        let Some(data) = line.data.as_ref() else {
            continue;
        };
        match data["type"].as_str() {
            Some("notification_transition" | "notification_requeued") => {
                let Some(recipient) =
                    serde_json::from_value::<RecipientKey>(data["recipient"].clone()).ok()
                else {
                    continue;
                };
                let Some(attempt_id) = data["attempt_id"]
                    .as_str()
                    .and_then(|value| NotificationAttemptId::parse(value).ok())
                else {
                    continue;
                };
                current_by_recipient.insert(recipient, attempt_id);
            }
            Some("notifications_requeued") => {
                for requeue in data["requeues"].as_array().into_iter().flatten() {
                    let Some(recipient) =
                        serde_json::from_value::<RecipientKey>(requeue["recipient"].clone()).ok()
                    else {
                        continue;
                    };
                    let Some(attempt_id) = requeue["attempt_id"]
                        .as_str()
                        .and_then(|value| NotificationAttemptId::parse(value).ok())
                    else {
                        continue;
                    };
                    current_by_recipient.insert(recipient, attempt_id);
                }
            }
            _ => {}
        }
    }
    let mut attempts = current_by_recipient
        .into_values()
        .collect::<BTreeSet<_>>()
        .into_iter();
    let attempt_id = attempts.next()?;
    if attempts.next().is_some() {
        return None;
    }
    Some(attempt_id)
}

fn compact_doorbell(rig: &Rig, message_id: &str) -> String {
    current_compact_doorbell(rig, message_id)
        .unwrap_or_else(|| panic!("message {message_id} has no notification attempt"))
}

#[test]
fn current_doorbell_follows_the_attempt_owned_by_a_requeue_fact() {
    fn line(seq: u64, message_id: &str, data: Value) -> LedgerLine {
        LedgerLine {
            seq,
            boot_id: "b-test".into(),
            id: message_id.into(),
            ts: seq,
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(data),
        }
    }

    let message_id = "m-requeued-doorbell";
    let prior = "att-00000000-0000-4000-8000-000000000001";
    let current = "att-00000000-0000-4000-8000-000000000002";
    let recipient: RecipientKey =
        "agent:00000000-0000-4000-8000-000000000003/00000000-0000-4000-8000-000000000004/%1"
            .parse()
            .unwrap();
    let lines = vec![
        line(
            1,
            message_id,
            json!({
                "type": "notification_transition",
                "attempt_id": prior,
                "recipient": recipient,
                "state": "attention_required"
            }),
        ),
        line(
            2,
            message_id,
            json!({
                "type": "notification_requeued",
                "prior_attempt_id": prior,
                "attempt_id": current,
                "recipient": recipient
            }),
        ),
    ];

    assert_eq!(
        current_notification_attempt(&lines, message_id),
        Some(NotificationAttemptId::parse(current).unwrap())
    );
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

fn joined_pane_history(rig: &Rig, pane: &str) -> String {
    let output = rig
        .tmux
        .run(&["capture-pane", "-p", "-J", "-S", "-", "-t", pane]);
    assert!(
        output.status.success(),
        "joined capture pane history failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

async fn wait_for_pane_mode(rig: &mut Rig, pane: &str, expected: bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let matches = status["result"]["sessions"][0]["panes"]
            .as_array()
            .expect("pane list")
            .iter()
            .any(|row| row["pane_id"] == pane && row["in_mode"] == expected);
        if matches {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane mode did not become {expected}: {status}"
        );
        // No event: `in_mode` is a pane row field with no announcement of its
        // own.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn resize_pane_and_allow_event(rig: &mut Rig, pane: &str, width: u32) {
    let width_arg = width.to_string();
    rig.tmux
        .run_ok(&["resize-window", "-t", pane, "-x", &width_arg]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let observed = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| session["panes"].as_array())
            .flatten()
            .any(|row| row["pane_id"] == pane && row["width"].as_u64() == Some(u64::from(width)));
        if observed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not observe pane {pane} at width {width}: {status}"
        );
        // No event: width is a pane row field with no announcement.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_human_composer_evidence(rig: &mut Rig, pane: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let held = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| session["panes"].as_array())
            .flatten()
            .any(|row| row["pane_id"] == pane && row["composer"] == "human_draft");
        if held {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane} did not record the human composer evidence: {status}"
        );
        // No event this wait can trust: status blanks `composer` while its
        // live refresh is incomplete.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn pane_pid(rig: &Rig, pane: &str) -> i64 {
    let output = rig
        .tmux
        .run(&["display-message", "-p", "-t", pane, "#{pane_pid}"]);
    assert!(
        output.status.success(),
        "pane pid lookup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .expect("numeric pane pid")
}

async fn wait_for_pane_pid_change(rig: &Rig, pane: &str, prior: i64) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = pane_pid(rig, pane);
        if current != prior {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane} did not acquire a new root process"
        );
        // No event: the root pid is read from tmux.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_pane_observation(rig: &mut Rig, pane: &str, title: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let observed = status["result"]["sessions"][0]["panes"]
            .as_array()
            .expect("pane list")
            .iter()
            .any(|row| row["pane_id"] == pane && row["title"] == title && row["manifest"] == "fix");
        if observed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not observe the auto-detectable replacement: {status}"
        );
        // No event: title and manifest are status fields with no announcement.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn private_body_shapes_never_reach_the_notification_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-private-shapes",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let long_token = "private-long-token".repeat(1_024);
    let body = format!(
        "/approve --all\n```rust\nlet secret = \"not pane content\";\n```\nUnicode: 你好 🌍 café\n{long_token}"
    );
    let result = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Private transport shapes",
                "summary": "A message is waiting. Claim it from the mailbox.",
                "body": body,
                "client_key": "private-shapes"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = result["msg_id"].as_str().unwrap().to_string();

    wait_for_doorbell(&rig, &pane, &message_id).await;
    let screen = joined_pane_history(&rig, &pane);
    for private_text in [
        "Private transport shapes",
        "/approve --all",
        "let secret",
        "你好 🌍 café",
        "private-long-token",
    ] {
        assert!(
            !screen.contains(private_text),
            "private message text reached the pane: {private_text:?}\n{screen}"
        );
    }
    assert!(
        screen.contains(&compact_doorbell(&rig, &message_id)),
        "the fixed notification row was not staged: {screen}"
    );

    let message = workspace_lines(&rig)
        .into_iter()
        .find(|line| line.id == message_id && matches!(line.kind, Kind::Msg))
        .expect("durable message line");
    assert_eq!(message.body.as_deref(), Some(body.as_str()));
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        1
    );
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    assert_eq!(
        snapshot["result"]["rows"][0]["recipients"][0]["mailbox"]["status"],
        "pending"
    );
    assert!(workspace_lines(&rig).iter().all(|line| {
        line.id != message_id
            || line.data.as_ref().is_none_or(|data| {
                !matches!(
                    data["type"].as_str(),
                    Some("message_claimed" | "message_delivered_direct")
                )
            })
    }));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_before_submit_still_submits_the_operator_visible_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "submit = \"Enter\"\n",
        "submit = \"Enter\"\nclear_keys = [\"C-c\"]\n",
    );
    let pane_command = format!("python3 {} --clear-staged", faketui_path());
    let mut rig = Rig::new(
        "workspace-claim-before-submit",
        &manifest,
        &pane_command,
        "receipt_block_ms = 15000\nack_timeout_ms = 15000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_submit" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let first =
        send_workspace_message(&rig, "claim-before-submit-first", "First", "first body").await;
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");
    wait_for_doorbell(&rig, &pane, &first_id).await;

    rig.daemon
        .claim_message_for_test("worker", &first_id)
        .expect("exact recipient claim");
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Notified),
        0,
        "claim before Enter cannot create a notified fact"
    );
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &first_id, NotificationState::Submitted).await;
    wait_for_notification_state(&mut rig, &first_id, NotificationState::Notified).await;
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_row = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(first_id.as_str()))
        .expect("claimed message remains visible");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"], "notified",
        "socket retrieval must not suppress the independent pane notification"
    );
    assert_eq!(
        first_row["recipients"][0]["notification"]["settlement"],
        serde_json::Value::Null
    );
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Submitted),
        1
    );
    assert!(pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &first_id)));
    rig.daemon.shutdown().await;
}

/// An agent-loop pane whose screen can show a modal the manifest does not
/// dismiss. The gate holds on the modal while the loop keeps reading its
/// stdin, so the test can type a socket claim into the pane that the
/// daemon resolves to the pane's own agent identity.
const HELD_AGENT_LOOP_MANIFEST: &str = r#"
[agent]
id = "held-agent-loop"
display_name = "Held agent loop"
process_names = ["Python", "python3"]
argv_basenames = ["cycagent"]

[[rule]]
id = "fake_trust_modal"
state = "blocked_modal"
priority = 1300
region = "bottom_non_empty_lines(8)"
contains = ["FAKE-TRUST-PROMPT"]
auto_dismiss = false

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']
"#;

/// A claim over the socket while the gate holds withdraws the doorbell:
/// the attempt settles as `withdrawn` with cause `claimed_before_write`,
/// the recipient's FIFO moves on, and nothing is ever pasted into the pane.
#[tokio::test(flavor = "multi_thread")]
async fn a_socket_claim_while_the_gate_holds_withdraws_the_doorbell_and_writes_nothing() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let client_dir = crate::body_privacy::named_clients("claim-before-write");
    let pane_command = crate::body_privacy::agent_command_loop(&client_dir);
    let mut rig = Rig::new(
        "workspace-claim-before-write",
        HELD_AGENT_LOOP_MANIFEST,
        &pane_command,
        "",
    )
    .await;
    crate::body_privacy::wait_manifest_bound_to(&mut rig, 1, "held-agent-loop").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // The loop paints the modal; the manifest holds the gate on it.
    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        &pane,
        "/bin/echo FAKE-TRUST-PROMPT",
        "Enter",
    ]);
    rig.tmux.wait_screen(&pane, "FAKE-TRUST-PROMPT");

    let first =
        send_workspace_message(&rig, "claim-before-write-first", "First", "first body").await;
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "gate"
                && event["data"]["id"] == first_id.as_str()
                && event["data"]["action"] == "hold"
        })
        .await;
    let held_doorbell = compact_doorbell(&rig, &first_id);
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Writing),
        0
    );

    // The recipient claims over the socket from inside its own pane.
    let claimed = crate::body_privacy::pane_request(
        &mut rig,
        &client_dir,
        &pane,
        "claim-before-write",
        "inbox.claim",
        json!({"message_id": first_id}),
    )
    .await;
    assert_eq!(claimed["result"]["disposition"], "claimed", "{claimed}");
    assert_eq!(claimed["result"]["message"]["body"], "first body");

    // The claim settled the attempt: withdrawn, because it was claimed
    // before the write. The settlement is projected from the claim fact.
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let notification = &snapshot["result"]["rows"]
        .as_array()
        .expect("snapshot rows")
        .iter()
        .find(|row| row["message_id"] == first_id)
        .expect("claimed message remains visible")["recipients"][0]["notification"];
    assert_eq!(notification["state"], "not_started", "{notification}");
    assert_eq!(notification["settlement"], "withdrawn_by_claim");
    assert_eq!(notification["pre_write_cause"], "claimed_before_write");

    // Clear the modal. The withdrawn attempt has no worker left; the next
    // message proves the pipeline moved past it.
    rig.tmux
        .run_ok(&["send-keys", "-t", &pane, "/usr/bin/clear", "Enter"]);
    let second =
        send_workspace_message(&rig, "claim-before-write-second", "Second", "second body").await;
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    // The loop pane has no hook, so its receipt may read back unverified.
    crate::body_privacy::wait_for_notification_submitted(&mut rig, &second_id).await;

    // Nothing of the first message ever reached the pane.
    let history = joined_pane_history(&rig, &pane);
    assert!(
        !history.contains(&held_doorbell),
        "the withdrawn doorbell was written: {history}"
    );
    assert!(!history.contains("first body"));
    assert!(history.contains(&compact_doorbell(&rig, &second_id)));
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Writing),
        0
    );
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Submitted),
        0
    );
    assert_eq!(notification_attempts(&rig, &first_id).len(), 1);
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_in_the_post_key_gap_settles_before_the_next_notification() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-claim-post-key-gap",
        CAT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 15000\nack_timeout_ms = 15000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (post_submit_tx, mut post_submit_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let post_submit_tx = post_submit_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "post_key" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if phase == "post_submit" {
                    let _ = post_submit_tx.send(());
                    return;
                }
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let first = send_workspace_message(&rig, "post-key-first", "First", "first body").await;
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the post-key pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Submitted),
        1
    );

    rig.daemon
        .claim_message_for_test("worker", &first_id)
        .expect("exact recipient claim");
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_row = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(first_id.as_str()))
        .expect("claimed message remains visible");
    assert_eq!(first_row["recipients"][0]["mailbox"]["status"], "claimed");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"],
        "notified"
    );
    assert_eq!(snapshot["result"]["counts"]["open_attention_entries"], 0);

    release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), post_submit_rx.recv())
            .await
            .is_err(),
        "a claimed attempt must settle before the post-submit receipt path"
    );
    wait_pane_state(&mut rig, "idle").await;
    let second = send_workspace_message(&rig, "post-key-second", "Second", "second body").await;
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &second_id, NotificationState::Writing).await;

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    assert_eq!(snapshot["result"]["counts"]["open_attention_entries"], 0);
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_swallowed_enter_records_notified_without_a_verifier() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-swallowed-compact-doorbell",
        CAT_MANIFEST,
        &swallowing_animated_composer_pane(),
        "receipt_block_ms = 300\nack_timeout_ms = 50\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let sent = send_workspace_message(
        &rig,
        "swallowed-compact-doorbell",
        "Swallowed doorbell",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Notified).await;

    // Changed chrome is not a receipt; neither is the absence of one an
    // alarm. The journal says the doorbell went out and nothing verified it.
    let notified = notification_transition(&rig, &message_id, NotificationState::Notified)
        .expect("durable notified transition");
    assert!(
        notified.data.as_ref().unwrap().get("verified_by").is_none(),
        "{notified:?}"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0
    );
    let screen = rig.tmux.capture(&pane);
    assert!(
        screen.contains(&compact_doorbell(&rig, &message_id)),
        "{screen}"
    );
    assert!(screen.contains("Ctx: 77%"), "{screen}");

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_notification_stages_the_preview_and_exact_claim_without_the_body() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-summary-claim",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    resize_pane_and_allow_event(&mut rig, &pane, 244).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let summary = "This message outlines our Cyclops multi-agent communication test and includes a secret note. Claim the inbox message to read the complete test summary and details.";
    let body = "private implementation details must remain in the mailbox";
    let sent =
        send_summarized_workspace_message(&rig, "summary-claim", "Parser review", summary, body)
            .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");

    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the summary notification has one exact attempt");
    // The row soft-wraps in the fixture pane; read it back joined, the
    // way the daemon's own readback does.
    let screen = joined_pane_history(&rig, &pane);
    assert!(
        screen.contains(summary),
        "summary missing from pane: {screen}"
    );
    assert!(
        screen.contains("[cyclops from admin to worker]"),
        "sender and recipient missing from pane: {screen}"
    );
    assert!(
        screen.contains(&cyclops_proto::render_doorbell_v3(attempt)),
        "claim command missing from pane: {screen}"
    );
    assert!(
        !screen.contains(body),
        "message body leaked into pane: {screen}"
    );

    let writing = notification_transition(&rig, &message_id, NotificationState::Writing)
        .expect("writing transition fixes the selected format");
    assert_eq!(
        writing.data.as_ref().unwrap()["doorbell_format"],
        cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM
    );
    release.add_permits(1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0,
        "a proven one-row summary notification must submit without operator recovery"
    );
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_summary_soft_wraps_without_dropping_the_operator_preview() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-summary-claim-fallback",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    resize_pane_and_allow_event(&mut rig, &pane, 80).await;

    let summary = "This summary cannot fit beside its exact claim at this width. The mailbox still contains every detail.";
    let sent = send_summarized_workspace_message(
        &rig,
        "summary-claim-fallback",
        "Narrow summary",
        summary,
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;

    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the summary notification keeps the same exact attempt");
    let screen = joined_pane_history(&rig, &pane);
    assert!(
        screen.contains(&cyclops_proto::render_doorbell_v3(attempt)),
        "the exact claim did not reach the pane: {screen}"
    );
    assert!(
        screen.contains(summary),
        "the wrapped operator preview was dropped: {screen}"
    );
    let writing = notification_transition(&rig, &message_id, NotificationState::Writing)
        .expect("the summary notification fixes its selected format durably");
    assert_eq!(
        writing.data.as_ref().unwrap()["doorbell_format"],
        cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0
    );
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn format_3_soft_wraps_in_a_narrow_pane_without_a_width_block() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-format3-width-edge",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    resize_pane_and_allow_event(&mut rig, &pane, 59).await;

    let sent = send_workspace_message(&rig, "format3-width", "Width", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the narrow-pane notification has one exact attempt");
    let doorbell = cyclops_proto::render_doorbell_v3(attempt);
    assert!(joined_pane_history(&rig, &pane).contains(&doorbell));
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        0,
        "terminal width must not suppress an operator-visible doorbell"
    );
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn format_3_resize_between_final_read_and_write_still_soft_wraps() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-format3-width-bookend",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_paste" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent =
        send_workspace_message(&rig, "format3-width-bookend", "Width race", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("delivery reached the final pre-write bookend")
        .expect("pause sender stayed open");
    resize_pane_and_allow_event(&mut rig, &pane, 59).await;
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the resized notification has one exact attempt");
    assert!(joined_pane_history(&rig, &pane).contains(&cyclops_proto::render_doorbell_v3(attempt)));
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        0
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_human_draft_holds_one_notification_attempt_until_its_turn_finishes() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-human-draft", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, "human draft stays private"]);
    rig.tmux.wait_screen("main", "human draft stays private");
    wait_for_human_composer_evidence(&mut rig, &pane).await;

    let pair = send_waiting_pair(&rig, "draft").await;

    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "gate"
                && event["data"]["id"] == pair.first.as_str()
                && event["data"]["action"] == "hold"
        })
        .await;
    let held_screen = rig.tmux.capture(&pane);
    assert!(held_screen.contains("human draft stays private"));
    assert!(!held_screen.contains(&compact_doorbell(&rig, &pair.first)));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Writing),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "Enter"]);
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Submitted),
        1
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_visible_human_draft_cleared_by_backspace_releases_the_same_attempt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-backspaced-human-draft",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    // One visible character is the smallest draft that exercises the contract:
    // human input holds the attempt, and Backspace clears that same attempt.
    // A longer draft made the test enqueue many independent tmux commands before
    // reaching this boundary, which tested runner scheduling rather than release.
    let draft = "e";
    rig.tmux.run_ok(&["send-keys", "-l", "-t", &pane, draft]);
    rig.tmux.wait_screen("main", draft);
    wait_for_human_composer_evidence(&mut rig, &pane).await;

    let sent = send_workspace_message(
        &rig,
        "backspaced-human-draft",
        "Backspace release",
        "body stays in the mailbox",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    rig.ev
        .wait_event_named(
            Duration::from_secs(8),
            "initial composer gate hold",
            |event| {
                event["event"] == "gate"
                    && event["data"]["id"] == message_id.as_str()
                    && event["data"]["action"] == "hold"
            },
        )
        .await;
    let attempts_before = notification_attempts(&rig, &message_id);
    assert_eq!(attempts_before.len(), 1);
    assert!(!rig
        .tmux
        .capture(&pane)
        .contains(&compact_doorbell(&rig, &message_id)));

    // Subscribe after the exact gate hold and immediately before Backspace.
    // This is the causal barrier: the connection cannot contain a different
    // gate decision from setup, but it is live before the release edge.
    let mut release_events = TestClient::connect(&rig.daemon.socket_path()).await;
    let subscribed = release_events.request("events.subscribe", json!({})).await;
    assert_eq!(subscribed["result"]["subscribed"], true);
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "BSpace"]);
    // `composer_clean` can precede the positive write-readiness edge while the
    // screen is still settling. The readiness event is the exact contract that
    // wakes or reopens the held attempt, so wait for it rather than polling a
    // weaker projection or guessing how long settlement takes.
    let readiness = release_events
        .wait_event_named(
            Duration::from_secs(8),
            "positive readiness after Backspace",
            |event| {
                event["event"] == "readiness"
                    && event["data"]["session_idx"] == 0
                    && event["data"]["pane_id"] == pane.as_str()
                    && event["data"]["write_ready"] == true
                    && event["data"]["write_block"].is_null()
            },
        )
        .await;
    assert_eq!(readiness["seq"], serde_json::Value::Null, "{readiness}");
    // This real-tmux adapter journey stops at the physical edge it owns.
    // Deterministic tests below it protect the rest of the chain:
    // `causal_route_evidence_survives_an_earlier_tokenless_readiness_observation`
    // carries the source edge, `workspace_messaging_applies_a_readiness_route_observation`
    // crosses the Module boundary, and
    // `blocked_readiness_reopens_once_only_after_positive_exact_route_evidence`
    // reopens this same durable attempt. Waiting here for another ephemeral
    // gate event duplicated those contracts and made runner scheduling part of
    // the proof.
    assert_eq!(notification_attempts(&rig, &message_id), attempts_before);
    assert!(workspace_lines(&rig).iter().all(|line| {
        line.id != message_id
            || line
                .data
                .as_ref()
                .is_none_or(|data| data["type"] != "notification_requeued")
    }));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_mode_holds_one_notification_attempt_until_the_pane_is_write_ready() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-copy-mode", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    rig.tmux.run_ok(&["copy-mode", "-t", &pane]);
    wait_for_pane_mode(&mut rig, &pane, true).await;

    let pair = send_waiting_pair(&rig, "mode").await;

    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "gate"
                && event["data"]["id"] == pair.first.as_str()
                && event["data"]["action"] == "hold"
                && event["data"]["cause"] == "pane_in_mode"
        })
        .await;
    let held_screen = rig.tmux.capture(&pane);
    assert!(!held_screen.contains(&compact_doorbell(&rig, &pair.first)));
    assert!(!held_screen.contains("first body"));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Writing),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "q"]);
    wait_for_pane_mode(&mut rig, &pane, false).await;
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Submitted),
        1
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_human_modal_holds_one_notification_attempt_until_the_prompt_is_cleared() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-human-modal",
        MODAL_MANIFEST,
        &hold_script("FAKE-TRUST-PROMPT"),
        "gate_hold_notify_ms = 200\n",
    )
    .await;
    rig.tmux.wait_screen("main", "FAKE-TRUST-PROMPT");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        Box::pin(async move {
            if phase == "pre_paste" {
                let _ = entered_tx.send(());
            }
        })
    });

    let pair = send_waiting_pair(&rig, "modal").await;

    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "admin-notify"
                && event["data"]["level"] == "action_required"
                && event["data"]["pane_id"] == pane
        })
        .await;
    let held_screen = rig.tmux.capture(&pane);
    assert!(held_screen.contains("FAKE-TRUST-PROMPT"));
    assert!(!held_screen.contains(&compact_doorbell(&rig, &pair.first)));
    assert!(!held_screen.contains("first body"));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Writing),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    rig.tmux.wait_screen("main", "Model x · Ctx: 78%");
    // The clear first has to reach the delivery state machine and pass the
    // gate to the pre-paste boundary, then complete the write. Waiting on
    // these durable latches separates a missed release edge from a
    // pane-paint delay, and leaves the pane history to show the doorbell.
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("reopen must reach the pre-paste execution boundary")
        .expect("pause sender stayed open");
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Writing).await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Submitted).await;
    let released = joined_pane_history(&rig, &pane);
    let expected = current_compact_doorbell(&rig, &pair.first)
        .expect("submitted attempt must have a formatted doorbell");
    assert!(
        released.contains(&expected),
        "doorbell was not shown: {released}"
    );
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Submitted),
        1
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_messages_schedule_only_the_oldest_without_session_message_rows() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-messaging", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let first_request = json!({
        "to": ["worker"],
        "subject": "First private subject",
        "summary": "A message is waiting. Claim it from the mailbox.",
        "body": "First private body",
        "client_key": "first-key"
    });
    let first: MsgSendParams = serde_json::from_value(first_request.clone()).unwrap();
    let first = rig.daemon.msg_send("admin", first).await.unwrap();
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    assert_eq!(first["inserted"], true);
    assert!(matches!(
        first["deliveries"][0]["notification_state"].as_str(),
        Some("queued" | "gating" | "writing" | "staged" | "submitted" | "notified")
    ));
    let first_retry = rig
        .daemon
        .msg_send("admin", serde_json::from_value(first_request).unwrap())
        .await
        .unwrap();
    assert_eq!(first_retry["msg_id"], first_id);
    assert_eq!(first_retry["inserted"], false);

    let second_request = json!({
        "to": ["worker"],
        "subject": "Second private subject",
        "summary": "A message is waiting. Claim it from the mailbox.",
        "body": "Second private body",
        "client_key": "second-key"
    });
    let second = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(second_request.clone()).unwrap(),
        )
        .await
        .unwrap();
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    assert_eq!(second["deliveries"][0]["notification_state"], "not_started");
    assert_eq!(second["deliveries"][0]["position"], 1);

    let retry = rig
        .daemon
        .msg_send("admin", serde_json::from_value(second_request).unwrap())
        .await
        .unwrap();
    assert_eq!(retry["msg_id"], second_id);
    assert_eq!(retry["inserted"], false);
    assert_eq!(retry["deliveries"][0]["notification_state"], "not_started");

    let screen = wait_for_doorbell(&rig, &pane, &first_id).await;
    assert!(!screen.contains("First private subject"));
    assert!(!screen.contains("First private body"));
    assert!(!screen.contains(&second_id));

    let workspace = workspace_lines(&rig);
    let messages: Vec<_> = workspace
        .iter()
        .filter(|line| matches!(line.kind, Kind::Msg | Kind::Fyi))
        .collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].id, first_id);
    assert_eq!(messages[1].id, second_id);
    let first_attempts: Vec<_> = workspace
        .iter()
        .filter_map(|line| {
            let data = line.data.as_ref()?;
            (data["type"] == "notification_transition" && line.id == messages[0].id)
                .then(|| data["attempt_id"].as_str().unwrap())
        })
        .collect();
    assert!(!first_attempts.is_empty());
    assert!(first_attempts
        .iter()
        .all(|attempt| attempt == &first_attempts[0]));
    let session_message_rows = rig
        .ledger_lines()
        .into_iter()
        .filter(|line| matches!(line["kind"].as_str(), Some("msg" | "fyi")))
        .count();
    assert_eq!(session_message_rows, 0);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_notification_resumes_after_restart_without_a_second_attempt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-restart", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_paste" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let result = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Resume",
                "summary": "A message is waiting. Claim it from the mailbox.",
                "body": "Durable body",
                "client_key": "restart-key"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = result["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("notification reached the pre-paste pause")
        .expect("pause sender stayed open");
    assert!(!rig
        .tmux
        .capture(&pane)
        .contains(&compact_doorbell(&rig, &message_id)));

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    let screen = wait_for_doorbell(&rig, &pane, &message_id).await;
    assert!(!screen.contains("Durable body"));

    let attempts: Vec<_> = workspace_lines(&rig)
        .into_iter()
        .filter_map(|line| {
            let data = line.data?;
            (data["type"] == "notification_transition")
                .then(|| data["attempt_id"].as_str().unwrap().to_string())
        })
        .collect();
    assert!(!attempts.is_empty());
    assert!(attempts.iter().all(|attempt| attempt == &attempts[0]));

    drop(hold);
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replacement_recipient_bypasses_a_stale_prewrite_worker() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-recipient-replacement",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let stale_pane = rig.pane_ids().await[0].clone();
    rig.label(&stale_pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_paste" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let stale = send_workspace_message(
        &rig,
        "stale-recipient",
        "Stale recipient",
        "private stale body",
    )
    .await;
    let stale_id = stale["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("stale recipient reached the prewrite pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0
    );

    rig.tmux.simulate_server_loss();
    rig.ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "session" && event["data"]["attached"] == false
        })
        .await;
    rig.tmux.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "160",
        "-y",
        "40",
        &composer_pane(),
    ]);
    let availability = rig
        .ctl
        .request("session.watch", json!({"session": "main"}))
        .await;
    assert_eq!(
        availability["result"]["added"],
        json!(false),
        "{availability}"
    );
    assert_eq!(
        availability["result"]["watching"],
        json!(true),
        "{availability}"
    );
    rig.wait_attached(1).await;
    let replacement_pane = rig.pane_ids().await[0].clone();
    assert_eq!(
        replacement_pane, stale_pane,
        "fixture must reuse the pane id"
    );
    rig.label(&replacement_pane, "worker").await;

    let replacement = send_workspace_message(
        &rig,
        "replacement-recipient",
        "Replacement recipient",
        "private replacement body",
    )
    .await;
    let replacement_id = replacement["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &replacement_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0
    );

    hold.add_permits(1);
    // A bounded window: nothing marks a write that must not happen.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0,
        "the stale route crossed the irreversible write boundary"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn same_route_with_a_new_pane_root_reproves_before_writing() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-pane-root-replacement",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let initial_pid = pane_pid(&rig, &pane);

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_paste" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let stale = send_workspace_message(
        &rig,
        "stale-pane-root",
        "Stale pane root",
        "private stale body",
    )
    .await;
    let stale_id = stale["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("notification reached the prewrite pause")
        .expect("pause sender stayed open");

    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &composer_pane()]);
    let replacement_pid = wait_for_pane_pid_change(&rig, &pane, initial_pid).await;
    assert_ne!(replacement_pid, initial_pid);
    let replacement_title = format!("replacement-root-{replacement_pid}");
    rig.tmux
        .run_ok(&["select-pane", "-T", &replacement_title, "-t", &pane]);
    wait_for_pane_observation(&mut rig, &pane, &replacement_title).await;

    hold.add_permits(1);
    wait_for_notification_state(&mut rig, &stale_id, NotificationState::Writing).await;
    let blocked = notification_transition(&rig, &stale_id, NotificationState::BlockedPreWrite)
        .expect("the stale process proof was refused before the write");
    let writing = notification_transition(&rig, &stale_id, NotificationState::Writing)
        .expect("the replacement process was admitted by a fresh proof");
    let blocked_data = blocked.data.as_ref().expect("blocked transition data");
    let writing_data = writing.data.as_ref().expect("writing transition data");
    assert!(
        blocked.seq < writing.seq,
        "the refusal must be durable before replacement proof authorizes Writing"
    );
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::BlockedPreWrite),
        1,
        "process replacement should produce one bounded refusal"
    );
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        1,
        "the replacement proof should cross the write boundary once"
    );
    assert_eq!(
        blocked_data["attempt_id"], writing_data["attempt_id"],
        "route reconciliation must retain the exact notification attempt"
    );
    assert!(
        matches!(
            blocked_data["pre_write_cause"].as_str(),
            Some("session_unavailable" | "write_readiness_changed")
        ),
        "the original route or process proof must fail closed before reconciliation: {blocked_data}"
    );
    assert_eq!(
        writing_data["binding"]["pane_root"]["pid"], replacement_pid,
        "Writing must bind to the replacement pane process"
    );
    assert_ne!(
        writing_data["binding"]["pane_root"]["pid"], initial_pid,
        "Writing must not reuse the original pane process"
    );
    assert_eq!(
        writing_data["binding"]["manifest"], "fix",
        "the replacement must still match the admitted manifest"
    );

    let status = rig.ctl.request("status", json!({})).await;
    let replacement_row = status["result"]["sessions"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pane_id"] == pane)
        .unwrap();
    assert_eq!(
        replacement_row["agent"], "worker",
        "the logical pane name should survive a same-session process replacement"
    );

    let fresh = send_workspace_message(
        &rig,
        "replacement-pane-root",
        "Replacement pane root",
        "private replacement body",
    )
    .await;
    assert_eq!(fresh["deliveries"][0]["to"], "worker");
    assert_eq!(
        fresh["deliveries"][0]["notification_state"], "not_started",
        "the fresh message is addressable but remains serialized behind the unresolved older one"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_ghost_with_unprovable_binding_blocks_once_and_withdrawal_advances_fifo() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = include_str!("../../../../resources/manifests/codex.toml").replace(
        "process_names = [\"codex\"]",
        "process_names = [\"not-a-real-fixture-process\"]",
    );
    let ghost_pane = concat!(
        r#"python3 -c 'import sys,time; "#,
        r#"sys.stdout.write("\033[1m\033[38;2;255;178;66m›\033[0m "#,
        r#"\033[2mSummarize recent commits\033[0m"); "#,
        r#"sys.stdout.flush(); time.sleep(3600)'"#,
    );
    let mut rig = Rig::new(
        "workspace-unprovable-pinned-binding",
        &manifest,
        ghost_pane,
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    let named = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "codex"}),
        )
        .await;
    assert_eq!(named["result"]["label"], "worker", "{named}");
    assert_eq!(named["result"]["manifest"], "codex", "{named}");
    wait_pane_state(&mut rig, "idle").await;

    let pair = send_waiting_pair(&rig, "unprovable-binding").await;
    assert_eq!(pair.first_receipt["notification_state"], "gating");
    assert_eq!(
        pair.first_receipt["pre_write_cause"], "binding_unprovable",
        "msg.send must return the worker's first durable pre-write disposition"
    );
    assert!(
        pair.second_receipt.get("pre_write_cause").is_none(),
        "the follower must not inherit the head's pre-write block"
    );
    assert!(
        pair.second_receipt.get("wake_block").is_none(),
        "the follower must not inherit the head's scheduler state"
    );
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;
    let gate_trace: Vec<String> = rig
        .ledger_lines()
        .iter()
        .filter(|line| line["kind"] == "gate" && line["id"] == pair.first.as_str())
        .map(|line| {
            format!(
                "{}:{}:{}",
                line["data"]["action"].as_str().unwrap_or(""),
                line["data"]["rule"].as_str().unwrap_or(""),
                line["data"]["cause"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        gate_trace,
        vec!["hold::occupant_unprovable".to_string()],
        "unprovable binding must block before the delivery gate admits a write"
    );
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::BlockedPreWrite),
        1
    );
    let blocked = notification_transition(&rig, &pair.first, NotificationState::BlockedPreWrite)
        .expect("the exact blocked transition is durable");
    let blocked_fact = blocked.data.as_ref().expect("blocked transition has data");
    assert_eq!(
        blocked_fact["pre_write_cause"], "binding_unprovable",
        "the durable reason must name the failed binding proof"
    );
    assert_eq!(
        blocked_fact["pre_write_observation"]["selected_manifest"],
        "codex"
    );
    // A failed OS process observation may not have a pane generation to
    // record. It must never manufacture the complete binding that failed.
    assert!(blocked_fact["pre_write_observation"]
        .get("binding")
        .is_none());
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        assert_eq!(notification_state_count(&rig, &pair.first, state), 0);
    }
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &pair.first)));

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_row = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("blocked message remains visible");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"],
        "gating"
    );
    assert_eq!(
        first_row["recipients"][0]["notification"]["pre_write_cause"],
        "binding_unprovable"
    );
    assert_eq!(
        first_row["recipients"][0]["can_withdraw_notification"],
        true
    );
    let attempt_id = blocked_fact["attempt_id"].clone();
    let recipient = first_row["recipients"][0]["recipient"].clone();

    // A bounded window: nothing marks a retry that must not restart.
    tokio::time::sleep(Duration::from_millis(600)).await;
    for _ in 0..3 {
        let repeated = rig
            .ctl
            .request(
                "pane.label",
                json!({"target": pane, "label": "worker", "manifest": "codex"}),
            )
            .await;
        assert_eq!(repeated["result"]["label"], "worker", "{repeated}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(notification_attempts(&rig, &pair.first).len(), 1);
    assert_eq!(
        rig.ledger_lines()
            .iter()
            .filter(|line| line["kind"] == "gate" && line["id"] == pair.first.as_str())
            .count(),
        gate_trace.len(),
        "unchanged evidence must not restart the bounded retry chain"
    );
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::BlockedPreWrite),
        1,
        "elapsed time and unchanged route evidence restarted the blocked attempt"
    );

    let withdrawn = rig
        .ctl
        .request(
            "notification.withdraw",
            json!({
                "attempt_id": attempt_id,
                "recipient": recipient
            }),
        )
        .await;
    assert!(withdrawn["error"].is_null(), "{withdrawn}");
    assert_eq!(withdrawn["result"]["disposition"], "withdrawn");
    wait_for_notification_state(&mut rig, &pair.second, NotificationState::BlockedPreWrite).await;
    assert_eq!(notification_attempts(&rig, &pair.second).len(), 1);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Writing),
        0
    );
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &pair.second)));

    let after = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_after = after["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("withdrawal keeps the mailbox item visible");
    assert_eq!(
        first_after["recipients"][0]["notification"]["operator_withdrawn"],
        true
    );
    assert_eq!(
        first_after["recipients"][0]["mailbox"]["status"], "pending",
        "withdrawing the wake must not consume the durable message"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_without_composer_ownership_delivers_to_known_route() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST
        .replace("composer_semantic = \"clean\"\n", "")
        .replace("composer_semantic = \"human_input\"\n", "");
    let mut rig = Rig::new(
        "workspace-composer-semantic-missing",
        &manifest,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let pair = send_waiting_pair(&rig, "composer-semantic-missing").await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Submitted).await;
    assert_only_oldest_attempt_exists(&rig, &pair);

    rig.daemon.shutdown().await;
}

/// The 2026-08-30 Cursor repro, generalized: a manifest whose composer rule
/// can only ever say `ambiguous` once left a wake invisibly "checking
/// readiness" for 16+ hours against a visibly idle pane. Ambiguous composer
/// evidence is not a hold: the doorbell goes out once, and the journal
/// records whatever the one read-back could prove.
#[tokio::test(flavor = "multi_thread")]
async fn an_indefinitely_ambiguous_idle_composer_submits_doorbell_once() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "composer_semantic = \"clean\"",
        "composer_semantic = \"ambiguous\"",
    );
    let mut rig = Rig::new(
        "workspace-composer-ambiguous",
        &manifest,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let pair = send_waiting_pair(&rig, "composer-ambiguous").await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Submitted).await;
    assert_only_oldest_attempt_exists(&rig, &pair);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn newly_proven_binding_reopens_the_same_blocked_attempt_once() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "process_names = [\"python3\", \"python\", \"Python\", \"cat\", \"sh\", \"dash\"]",
        "process_names = [\"python3\", \"Python\"]",
    );
    let unrecognized_composer = concat!(
        "sh -c 'printf \"❯\\n",
        "────────────────────────────────────────\\n",
        "Model x · Ctx: 78%\\n\"; exec tail -f /dev/null'"
    );
    let mut rig = Rig::new(
        "workspace-binding-reopen",
        &manifest,
        unrecognized_composer,
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    let named = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(named["result"]["label"], "worker", "{named}");
    wait_pane_state(&mut rig, "idle").await;

    let sent = send_workspace_message(
        &rig,
        "binding-reopen",
        "Binding becomes provable",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::BlockedPreWrite).await;
    let attempt = notification_attempts(&rig, &message_id)
        .into_iter()
        .next()
        .expect("blocked attempt");
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &message_id)));

    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &composer_pane()]);
    let reopened_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if notification_state_count(&rig, &message_id, NotificationState::Gating) >= 2 {
            break;
        }
        assert!(
            Instant::now() < reopened_deadline,
            "new binding did not reopen the attempt: status={} lines={:#?}",
            rig.ctl.request("status", json!({})).await,
            workspace_lines(&rig)
                .into_iter()
                .filter(|line| line.id == message_id)
                .collect::<Vec<_>>()
        );
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        rig.ev
            .wake_on(
                reopened_deadline.saturating_duration_since(Instant::now()),
                |e| e["event"] == "messages.changed",
            )
            .await;
    }
    let screen = wait_for_doorbell(&rig, &pane, &message_id).await;
    assert!(screen.contains(&compact_doorbell(&rig, &message_id)));
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    assert_eq!(
        notification_attempts(&rig, &message_id),
        BTreeSet::from([attempt])
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        1
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Writing),
        1
    );

    rig.daemon.shutdown().await;
}

/// Poll the roster until the pane's fused verdict carries exactly `block`
/// as its write block (`None` for write-allowed or an unstamped refusal).
async fn wait_for_pane_write_block(rig: &mut Rig, pane: &str, block: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let current = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| session["panes"].as_array())
            .flatten()
            .find(|row| row["pane_id"] == pane)
            .map(|row| row["write_block"].as_str().map(str::to_string));
        if current == Some(block.map(str::to_string)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane} never carried write block {block:?}: {status}"
        );
        // No event this wait can trust: `readiness` announces a change, but
        // status blanks `write_block` while its live refresh is incomplete, and
        // that has no event.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One in-process hook report for the labeled `worker` pane.
async fn report_hook(rig: &Rig, event: &str, seq: u64, payload: Value) -> Value {
    rig.daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "worker",
                "event": event,
                "seq": seq,
                "payload": payload
            }))
            .unwrap(),
        )
        .await
        .expect("hook report accepted")
}

/// The liveness fixture, with the Codex-shaped lifecycle key needed to prove
/// that a terminal edge for an older turn cannot veto the current attempt.
fn keyed_liveness_manifest() -> String {
    let marker = "ack_payload_field = \"prompt\"";
    assert_eq!(
        LIVENESS_MANIFEST.matches(marker).count(),
        1,
        "liveness fixture hook shape changed"
    );
    LIVENESS_MANIFEST.replacen(
        marker,
        "ack_payload_field = \"prompt\"\nturn_key_fields = [\"session_id\", \"turn_id\"]",
        1,
    )
}

/// Restart truth: an old SessionStart and an unclosed prompt are never
/// replayed after a daemon restart, so the clean restarted pane is unknown,
/// not idle and not working. Unknown is not a named block: a send there
/// still rings once. A duplicate SessionStart before the restart is
/// duplicate-safe and leaves the pane admitted once.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_pane_is_unknown_and_still_takes_a_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-restart",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    // A clean composer with no admitting edge from this boot is unknown,
    // never idle.
    wait_pane_state(&mut rig, "unknown").await;

    // One SessionStart from this boot admits the pane. The same report
    // again is a duplicate and leaves it admitted exactly as before.
    let first = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(first["applied"], true, "{first}");
    wait_pane_state(&mut rig, "idle").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;
    let duplicate = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(duplicate["applied"], false, "{duplicate}");
    wait_pane_state(&mut rig, "idle").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;

    // A turn opens and never closes before the daemon restarts.
    let prompt = report_hook(
        &rig,
        "UserPromptSubmit",
        2,
        json!({
            "prompt": "a turn that never closes",
            "session_id": "session-1",
            "turn_id": "turn-1"
        }),
    )
    .await;
    assert_eq!(prompt["applied"], true, "{prompt}");
    assert_eq!(prompt["state"], "working", "{prompt}");

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    // Restart truth: neither the old SessionStart nor the unclosed prompt
    // is replayed. The pane is unknown, not idle and not working.
    wait_pane_state(&mut rig, "unknown").await;
    let _before = rig.tmux.capture(&pane);

    let sent =
        send_workspace_message(&rig, "hook-admission-restart", "Restart", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;

    rig.daemon.shutdown().await;
}

/// A configured UserPromptSubmit start records liveness for the pane, but
/// an open turn over a clean composer is Working, not idle, until a valid
/// terminal edge closes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_prompt_submit_records_liveness_but_stays_working_until_a_terminal_edge() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-prompt",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "unknown").await;

    let prompt = report_hook(
        &rig,
        "UserPromptSubmit",
        1,
        json!({
            "prompt": "a real turn",
            "session_id": "session-1",
            "turn_id": "turn-1"
        }),
    )
    .await;
    assert_eq!(prompt["applied"], true, "{prompt}");
    assert_eq!(prompt["state"], "working", "{prompt}");
    wait_pane_state(&mut rig, "working").await;
    // The clean composer under the open turn must not be admitted as idle
    // by the liveness the prompt just recorded.
    // A bounded window: nothing marks an admission that must not happen.
    tokio::time::sleep(Duration::from_millis(300)).await;
    wait_pane_state(&mut rig, "working").await;

    let stop = report_hook(
        &rig,
        "Stop",
        2,
        json!({"session_id": "session-1", "turn_id": "turn-1"}),
    )
    .await;
    assert_eq!(stop["applied"], true, "{stop}");
    // Idle by the confirmed end. This fixture has no lifecycle idle rule,
    // so the hook-derived idle keeps its own named write block; write
    // readiness after a turn is the manifest's contract, not this one.
    wait_pane_state(&mut rig, "idle").await;

    rig.daemon.shutdown().await;
}

/// A working pane may accept a notification only when a fresh screen capture
/// positively proves that its composer is clean. Runtime `Working` is not a
/// blanket refusal, and it is not permission: an ambiguous or missing
/// composer proof still follows the fail-closed path.
#[tokio::test(flavor = "multi_thread")]
async fn a_working_pane_with_a_proven_clean_composer_submits_one_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let event_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "working-clean-composer-submit-event-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&event_dir).unwrap();
    let _event_guard = HomeGuard(event_dir.clone());
    let submit_event_path = event_dir.join("submit.sock");
    let submit_events =
        UnixDatagram::bind(&submit_event_path).expect("bind fake composer submit event socket");
    let pane_command = format!(
        "python3 {} --manual-lifecycle --submit-event-socket {}",
        faketui_path(),
        submit_event_path.display()
    );
    let mut rig = Rig::new(
        "workspace-working-clean-composer",
        LIVENESS_MANIFEST,
        &pane_command,
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let start = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(start["applied"], true, "{start}");
    wait_for_pane_write_block(&mut rig, &pane, None).await;
    wait_pane_state(&mut rig, "idle").await;

    // The fixture keeps its clean prompt visible while exposing a distinct
    // Working row. The screen semantic is therefore positive even though
    // the runtime winner is Working.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-t"]);
    wait_pane_state(&mut rig, "working").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent = send_workspace_message(
        &rig,
        "working-clean-composer",
        "Working clean composer",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    wait_for_doorbell(&rig, &pane, &message_id).await;
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");

    // This is the deterministic post-paste observation that reproduces the
    // final-Enter regression.
    refresh_exact_working_doorbell(&mut rig, &pane).await;
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        1,
        "the exact doorbell must reserve and send only one Enter"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0,
        "a Working pane with a positively clean composer must not become verify_failed"
    );
    let mut event = [0_u8; 16];
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the fake composer did not receive Enter")
        .expect("read fake composer submit event");
    assert_eq!(
        &event[..received],
        b"submit",
        "the fake composer must report the submitted Enter"
    );
    // Shutdown drains or aborts all delivery workers before this test-owned
    // terminal checkpoint. The fixture receives terminal input in order, so a
    // duplicate Enter already queued by Cyclops would report `submit` before
    // the checkpoint. No timer or filesystem poll is needed to prove it.
    rig.daemon.shutdown().await;
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-q"]);
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the fake composer did not acknowledge the terminal checkpoint")
        .expect("read fake composer checkpoint event");
    assert_eq!(
        &event[..received],
        b"checkpoint",
        "the fake composer received an extra Enter before the shutdown checkpoint"
    );
}

/// AGY 1.1.23 wraps an otherwise single-line doorbell into application-owned
/// rows with a two-cell continuation gutter. The gutter is chrome, so the
/// exact proof must still permit the one verified Enter, not hold forever.
#[tokio::test(flavor = "multi_thread")]
async fn agy_indented_wrapped_doorbell_submits_one_enter() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let event_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "agy-indented-doorbell-submit-event-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&event_dir).unwrap();
    let _event_guard = HomeGuard(event_dir.clone());
    let submit_event_path = event_dir.join("submit.sock");
    let submit_events =
        UnixDatagram::bind(&submit_event_path).expect("bind AGY fixture submit event socket");
    let pane_command = format!(
        "python3 {} --agy-layout --agy-content-columns 65 --submit-event-socket {}",
        faketui_path(),
        submit_event_path.display()
    );
    // Preserve the installed 1.1.23 input projection here: its five-row tail
    // loses this four-row prompt below AGY's divider and status chrome. The
    // shipped-manifest tests cover the repaired eight-row rule; this delivery
    // regression proves that an existing installed copy still submits only
    // after exact bytes and binding have been re-proven. The fixture process
    // is deliberately Python, and Rig supplies a private current skill so
    // this is a format-4 mailbox doorbell instead of a legacy direct payload.
    let manifest = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../resources/manifests/agy.toml"
    ))
    .replacen(
        "process_names = [\"agy\"]",
        "process_names = [\"python3\", \"python\", \"Python\"]",
        1,
    )
    .replacen(
        "region = \"bottom_non_empty_lines(8)\"",
        "region = \"bottom_non_empty_lines(5)\"",
        1,
    );
    assert!(
        !manifest.contains("mailbox_capability_file"),
        "Rig must install its private current capability"
    );
    assert!(
        manifest.contains("region = \"bottom_non_empty_lines(5)\""),
        "the regression must retain AGY's previously shipped input projection"
    );
    let mut rig = Rig::new(
        "agy-indented-doorbell-submit",
        &manifest,
        &pane_command,
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    // The live failure was a 70-column AGY pane. Wait for the exact narrow
    // geometry before staging a doorbell whose prompt spans four rows.
    // The isolated server keeps tmux's one-row status line, so a 70x31
    // window produces the observed 70x30 pane.
    rig.tmux
        .run_ok(&["resize-window", "-t", &pane, "-x", "70", "-y", "31"]);
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-l"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let observed = status["result"]["sessions"][0]["panes"]
            .as_array()
            .expect("pane list")
            .iter()
            .any(|row| row["pane_id"] == pane && row["width"] == 70 && row["height"] == 30);
        if observed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the AGY fixture did not reach the live 70x30 pane: {status}"
        );
        // No event: width and height are pane row fields with no announcement.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase == "pre_submit" {
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            }
        })
    });

    let summary = "This concise notice reproduces the narrow AGY four-row doorbell shape. Claim the durable body and continue with the requested work.";
    let sent = send_summarized_workspace_message(
        &rig,
        "agy-indented-doorbell-submit",
        "AGY wrapped doorbell",
        summary,
        "The durable body stays in the mailbox.",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();

    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("AGY doorbell did not reach the pre-submit proof")
        .expect("pre-submit pause sender stayed open");
    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the staged AGY doorbell has one exact attempt");
    let expected = cyclops_proto::render_doorbell_v4("admin", "worker", summary, attempt);
    let screen = rig.tmux.capture(&pane);
    let continuation_rows: Vec<_> = screen.lines().filter(|row| row.starts_with("  ")).collect();
    assert_eq!(
        continuation_rows.len(),
        3,
        "the AGY fixture must expose the live four-row, two-cell-gutter composer:\n{screen}"
    );
    assert!(
        continuation_rows
            .iter()
            .all(|row| row.as_bytes().get(2).is_some_and(|byte| !byte.is_ascii_whitespace())),
        "the fixture rows must contain only the renderer gutter before application content: {continuation_rows:?}"
    );
    assert!(
        expected.contains("cyclops inbox claim m-att_"),
        "the staged row remains the format-4 mailbox doorbell"
    );

    release.add_permits(1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        1,
        "the exact doorbell reserves and sends one Enter"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0,
        "the renderer gutter must not become verify_failed"
    );
    let mut event = [0_u8; 16];
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the AGY fixture did not receive Enter")
        .expect("read AGY fixture submit event");
    assert_eq!(&event[..received], b"submit");

    rig.daemon.shutdown().await;
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-q"]);
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the AGY fixture did not acknowledge the terminal checkpoint")
        .expect("read AGY fixture checkpoint event");
    assert_eq!(
        &event[..received],
        b"checkpoint",
        "the AGY fixture received an extra Enter before the shutdown checkpoint"
    );
}

/// A pane whose liveness no hook has admitted yet is unknown, and unknown
/// is not one of the named blocks: the doorbell still goes out once.
#[tokio::test(flavor = "multi_thread")]
async fn a_pane_without_an_admitting_hook_edge_still_takes_a_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-unknown",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "unknown").await;

    let sent =
        send_workspace_message(&rig, "hook-admission-unknown", "Unknown", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn decoy_replacement_generation_does_not_inherit_prior_occupant_composer_hold() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let ghost = r#"\033[1m\342\200\272\033[0m \033[2mFind and fix a bug in @filename\033[0m"#;
    let typed = r#"\033[1m\342\200\272\033[0m fix the rate limiter in gateway.rs"#;
    let pane_command = format!(
        "sh -c 'printf \"{ghost}\\n\"; read a; printf \"\\033[2J\\033[H\"; \
         printf \"{typed}\\n\"; exec cat'"
    );
    let mut rig = Rig::new(
        "composer-hold-decoy-generation",
        ESC_COMPOSER_MANIFEST,
        &pane_command,
        "receipt_block_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    let initial_pid = pane_pid(&rig, &pane);
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    // First occupant types input, entering staged hold.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    wait_pane_state(&mut rig, "idle_with_input").await;

    let sent = send_workspace_message(
        &rig,
        "composer-hold-decoy-generation",
        "Held for first occupant",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "gate"
                && event["data"]["id"] == message_id.as_str()
                && event["data"]["action"] == "hold"
                && event["data"]["cause"] == "composer_hold"
        })
        .await;
    let attempt_id = notification_attempts(&rig, &message_id)
        .into_iter()
        .next()
        .expect("the held notification has one attempt");
    let attempt_id = NotificationAttemptId::parse(&attempt_id).expect("held attempt id");

    // Withdraw first message using direct resolved-admin seam so FIFO is clear for replacement message.
    let snapshot = json!({
        "result": serde_json::to_value(
            rig.daemon
                .messages_snapshot_for_test("admin", 20)
                .expect("admin messages snapshot"),
        )
        .expect("messages snapshot serializes")
    });
    let first = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(message_id.as_str()))
        .expect("first message exists");
    let recipient_key = serde_json::from_value(first["recipients"][0]["recipient"].clone())
        .expect("held recipient key");
    let withdrawn = serde_json::to_value(
        rig.daemon
            .withdraw_notification_for_test("admin", recipient_key, attempt_id)
            .expect("admin notification withdrawal"),
    )
    .expect("withdrawal result serializes");
    assert_eq!(withdrawn["disposition"], "withdrawn", "{withdrawn}");

    // Replace the occupant with a fresh process tree in the same pane using respawn-pane.
    let clean_pane_command = format!("sh -c 'printf \"{ghost}\\n\"; exec cat'");
    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &clean_pane_command]);
    let replacement_pid = wait_for_pane_pid_change(&rig, &pane, initial_pid).await;
    assert_ne!(replacement_pid, initial_pid);
    wait_pane_state(&mut rig, "idle").await;

    // Send a new message to the replacement occupant.
    let replacement_sent = send_workspace_message(
        &rig,
        "composer-hold-decoy-generation-replacement",
        "For replacement occupant",
        "second private body",
    )
    .await;
    let replacement_id = replacement_sent["msg_id"].as_str().unwrap().to_string();

    // The replacement occupant generation starts with a clean hold and proceeds to write without being blocked by prior occupant's hold.
    wait_for_notification_state(&mut rig, &replacement_id, NotificationState::Writing).await;

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_working_turn_started_pane_submits_doorbell_without_barrier_held() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let event_dir = cyclops_proto::scratch::scratch_dir(&format!("wts-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&event_dir).unwrap();
    let _event_guard = HomeGuard(event_dir.clone());
    let submit_event_path = event_dir.join("submit.sock");
    let submit_events =
        UnixDatagram::bind(&submit_event_path).expect("bind fake composer submit event socket");
    let pane_command = format!(
        "python3 {} --manual-lifecycle --submit-event-socket {}",
        faketui_path(),
        submit_event_path.display()
    );
    let manifest = keyed_liveness_manifest();
    let mut rig = Rig::new(
        "workspace-working-turn-started",
        &manifest,
        &pane_command,
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let start = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(start["applied"], true, "{start}");
    wait_for_pane_write_block(&mut rig, &pane, None).await;

    // Simulate agent turn start (e.g. UserPromptSubmit), putting the pane in TurnStarted hold
    let prompt = report_hook(
        &rig,
        "UserPromptSubmit",
        2,
        json!({
            "prompt": "solve guitar chords",
            "session_id": "session-1",
            "turn_id": "turn-1"
        }),
    )
    .await;
    assert_eq!(prompt["applied"], true, "{prompt}");
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-t"]);
    wait_pane_state(&mut rig, "working").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    // Send a message/reply to this working pane
    let sent = send_workspace_message(
        &rig,
        "working-turn-started",
        "Guitar chord recommendations",
        "E minor and G major chords",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();

    // The message must transition through Writing and be submitted directly to the composer
    // without failing or sticking in barrier_held!
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    wait_for_doorbell(&rig, &pane, &message_id).await;
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");

    refresh_exact_working_doorbell(&mut rig, &pane).await;
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;

    let mut event = [0_u8; 16];
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the fake composer did not receive Enter")
        .expect("read fake composer submit event");
    assert_eq!(
        &event[..received],
        b"submit",
        "the doorbell must be submitted with Enter"
    );

    rig.daemon.shutdown().await;
}

/// The raw transport from the unsafe side: the whole rendered message is
/// pasted and submitted with no composer check, the record names the
/// transport and no binding, and the attempt closes as Notified with no
/// verifier.
#[tokio::test(flavor = "multi_thread")]
async fn a_raw_send_pastes_the_whole_message_and_records_an_unverified_write() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-raw-send", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Raw transport",
                "body": "the raw body reaches the pane",
                "client_key": "raw-send",
                "raw": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Notified).await;

    let writing = notification_transition(&rig, &message_id, NotificationState::Writing)
        .expect("durable writing transition");
    let writing = writing.data.as_ref().unwrap();
    assert_eq!(writing["transport"], "raw", "{writing}");
    assert!(writing.get("binding").is_none(), "{writing}");
    assert!(writing.get("doorbell_format").is_none(), "{writing}");
    let notified = notification_transition(&rig, &message_id, NotificationState::Notified)
        .expect("durable notified transition");
    assert!(
        notified.data.as_ref().unwrap().get("verified_by").is_none(),
        "{notified:?}"
    );
    for state in [
        NotificationState::Submitted,
        NotificationState::SubmittedUnverified,
        NotificationState::AttentionRequired,
    ] {
        assert_eq!(notification_state_count(&rig, &message_id, state), 0);
    }

    let history = joined_pane_history(&rig, &pane);
    assert!(
        history.contains("SUBJECT: Raw transport"),
        "the raw header did not reach the pane: {history}"
    );
    assert!(
        history.contains("the raw body reaches the pane"),
        "the raw body did not reach the pane: {history}"
    );
    assert!(
        history.contains(&format!("[cyclops:end {message_id}]")),
        "the raw sentinel did not reach the pane: {history}"
    );

    rig.daemon.shutdown().await;
}
