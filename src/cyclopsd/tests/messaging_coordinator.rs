mod common;

use std::collections::BTreeSet;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    composer_pane, hold_script, tmux_available, wait_pane_state, Rig, CAT_MANIFEST, MODAL_MANIFEST,
};
use cyclops_proto::{Kind, LedgerLine, MessageId, MsgSendParams};
use serde_json::{json, Value};

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

fn notification_state_count(rig: &Rig, message_id: &str, state: &str) -> usize {
    workspace_lines(rig)
        .into_iter()
        .filter(|line| {
            line.id == message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition" && data["state"] == state
                })
        })
        .count()
}

async fn wait_for_notification_state(rig: &mut Rig, message_id: &str, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let transition = loop {
        if let Some(line) = workspace_lines(rig).into_iter().find(|line| {
            line.id == message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition" && data["state"] == state
                })
        }) {
            break line;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} did not reach {state}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
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

async fn wait_for_workspace_fact(rig: &Rig, message_id: &str, fact_type: &str) -> LedgerLine {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(line) = workspace_lines(rig).into_iter().find(|line| {
            line.id == message_id
                && line
                    .data
                    .as_ref()
                    .is_some_and(|data| data["type"] == fact_type)
        }) {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "message {message_id} did not append {fact_type}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn send_workspace_message(rig: &Rig, client_key: &str, subject: &str, body: &str) -> Value {
    rig.daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": subject,
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
    }
}

fn assert_only_oldest_attempt_exists(rig: &Rig, pair: &WaitingPair) {
    assert_eq!(notification_attempts(rig, &pair.first).len(), 1);
    assert!(notification_attempts(rig, &pair.second).is_empty());
}

async fn wait_for_doorbell(rig: &Rig, pane: &str, message_id: &str) -> String {
    let expected = compact_doorbell(message_id);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = rig.tmux.capture(pane);
        if screen.contains(&expected) {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "doorbell was not shown: {screen}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn compact_doorbell(message_id: &str) -> String {
    cyclops_proto::render_doorbell_v1(&MessageId::new(message_id).unwrap())
}

fn legacy_doorbell(message_id: &str) -> String {
    cyclops_proto::render_legacy_doorbell(&MessageId::new(message_id).unwrap())
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
        tokio::time::sleep(Duration::from_millis(50)).await;
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
                "body": body,
                "client_key": "private-shapes"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = result["msg_id"].as_str().unwrap().to_string();

    wait_for_doorbell(&rig, &pane, &message_id).await;
    let screen = pane_history(&rig, &pane);
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
        screen.contains(&cyclops_proto::render_doorbell_v1(
            &cyclops_proto::MessageId::new(&message_id).unwrap()
        )),
        "the fixed notification row was not staged: {screen}"
    );

    let message = workspace_lines(&rig)
        .into_iter()
        .find(|line| line.id == message_id && matches!(line.kind, Kind::Msg))
        .expect("durable message line");
    assert_eq!(message.body.as_deref(), Some(body.as_str()));
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    wait_for_notification_state(&mut rig, &message_id, "staged").await;
    assert_eq!(notification_state_count(&rig, &message_id, "staged"), 1);
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
async fn recipient_without_exact_mailbox_evidence_gets_direct_delivery_without_a_claim() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_without_mailbox_capability(
        "workspace-direct-fallback",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let pair = send_waiting_pair(&rig, "direct-fallback").await;
    wait_for_workspace_fact(&rig, &pair.first, "message_delivered_direct").await;
    wait_for_workspace_fact(&rig, &pair.second, "message_delivered_direct").await;

    let lines = workspace_lines(&rig);
    for message_id in [&pair.first, &pair.second] {
        assert!(lines.iter().any(|line| {
            line.id == *message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition"
                        && data["state"] == "writing"
                        && data["transport"] == "direct_payload"
                })
        }));
        assert!(lines.iter().all(|line| {
            line.id != *message_id
                || line
                    .data
                    .as_ref()
                    .is_none_or(|data| data["type"] != "message_claimed")
        }));
    }
    let screen = pane_history(&rig, &pane);
    assert!(screen.contains("first body"));
    assert!(screen.contains("second body"));
    for message_id in [&pair.first, &pair.second] {
        assert!(!screen.contains(&compact_doorbell(message_id)));
        assert!(!screen.contains(&legacy_doorbell(message_id)));
    }

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let rows = snapshot["result"]["rows"].as_array().unwrap();
    for message_id in [&pair.first, &pair.second] {
        let row = rows
            .iter()
            .find(|row| row["message_id"].as_str() == Some(message_id.as_str()))
            .expect("directly delivered message remains visible in the snapshot");
        assert_eq!(
            row["recipients"][0]["mailbox"]["status"],
            "delivered_direct"
        );
    }

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mailbox_evidence_drift_before_the_write_falls_back_without_a_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-capability-drift",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

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

    let sent = send_workspace_message(
        &rig,
        "capability-drift",
        "Capability changed",
        "direct body after capability drift",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell attempt reached the prewrite pause")
        .expect("pause sender stayed open");
    fs::write(rig.home.join("cyclops-skill.md"), b"outdated skill bytes").unwrap();
    hold.add_permits(1);

    wait_for_workspace_fact(&rig, &message_id, "message_delivered_direct").await;
    let lines = workspace_lines(&rig);
    let writing: Vec<_> = lines
        .iter()
        .filter(|line| {
            line.id == message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition" && data["state"] == "writing"
                })
        })
        .collect();
    assert_eq!(writing.len(), 1);
    assert_eq!(
        writing[0].data.as_ref().unwrap()["transport"],
        "direct_payload"
    );
    assert!(lines.iter().all(|line| {
        line.id != message_id
            || line
                .data
                .as_ref()
                .is_none_or(|data| data["type"] != "message_claimed")
    }));
    let screen = pane_history(&rig, &pane);
    assert!(screen.contains("direct body after capability drift"));
    assert!(!screen.contains(&compact_doorbell(&message_id)));
    assert!(!screen.contains(&legacy_doorbell(&message_id)));

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
    assert!(!held_screen.contains(&cyclops_proto::render_doorbell_v1(
        &cyclops_proto::MessageId::new(&pair.first).unwrap()
    )));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(notification_state_count(&rig, &pair.first, "staged"), 0);

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "Enter"]);
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, "staged").await;
    assert_eq!(notification_state_count(&rig, &pair.first, "staged"), 1);

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
    assert!(!held_screen.contains(&compact_doorbell(&pair.first)));
    assert!(!held_screen.contains("first body"));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(notification_state_count(&rig, &pair.first, "staged"), 0);

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "q"]);
    wait_for_pane_mode(&mut rig, &pane, false).await;
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, "staged").await;
    assert_eq!(notification_state_count(&rig, &pair.first, "staged"), 1);

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
        "",
    )
    .await;
    rig.tmux.wait_screen("main", "FAKE-TRUST-PROMPT");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

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
    assert!(!held_screen.contains(&compact_doorbell(&pair.first)));
    assert!(!held_screen.contains("first body"));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(notification_state_count(&rig, &pair.first, "staged"), 0);

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, "staged").await;
    assert_eq!(notification_state_count(&rig, &pair.first, "staged"), 1);

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
        .contains(&compact_doorbell(&message_id)));

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
    assert_eq!(notification_state_count(&rig, &stale_id, "writing"), 0);

    rig.tmux.run_ok(&["kill-server"]);
    tokio::time::sleep(Duration::from_millis(100)).await;
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
    wait_for_notification_state(&mut rig, &replacement_id, "staged").await;
    assert_eq!(notification_state_count(&rig, &stale_id, "writing"), 0);

    hold.add_permits(1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        notification_state_count(&rig, &stale_id, "writing"),
        0,
        "the stale route crossed the irreversible write boundary"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn same_route_with_a_new_pane_root_never_writes_without_readoption() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-pane-root-replacement",
        CAT_MANIFEST,
        &composer_pane(),
        "",
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
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        notification_state_count(&rig, &stale_id, "writing"),
        0,
        "an auto-detectable replacement crossed Writing without re-adoption"
    );

    rig.daemon.shutdown().await;
}
