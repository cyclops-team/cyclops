mod common;

use std::collections::BTreeSet;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    composer_pane, faketui_path, hold_script, swallowing_animated_composer_pane, tmux_available,
    wait_pane_state, Rig, CAT_MANIFEST, HOOK_MANIFEST, MODAL_MANIFEST,
};
use cyclops_proto::{Kind, LedgerLine, MessageId, MsgSendParams, NotificationState};
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
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Staged),
        1
    );
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
async fn a_doorbell_changed_before_submit_records_verify_attention() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-doorbell-pre-submit-edit",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
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
        "doorbell-pre-submit-edit",
        "Pre-submit edit",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Staged),
        1
    );
    wait_for_doorbell(&rig, &pane, &message_id).await;

    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    hold.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    let attention =
        notification_transition(&rig, &message_id, NotificationState::AttentionRequired)
            .expect("durable attention transition");
    assert_eq!(attention.data.unwrap()["cause"], "verify_failed");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        0
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_start_after_submit_reservation_withholds_enter() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-doorbell-reserved-hook-start",
        HOOK_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "post_submit_reservation" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent = send_workspace_message(
        &rig,
        "doorbell-reserved-hook-start",
        "Reserved hook start",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the post-reservation pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitting),
        1
    );
    wait_for_doorbell(&rig, &pane, &message_id).await;

    // The exact doorbell is still visible, but a confirmed lifecycle edge
    // says this occupant is already running a turn. Exact content alone must
    // not authorize a second terminal key.
    let report = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "worker",
                "event": "UserPromptSubmit",
                "seq": 1,
                "payload": {
                    "prompt": "a different prompt already started this turn",
                    "session_id": "session-1",
                    "turn_id": "turn-1"
                }
            }))
            .unwrap(),
        )
        .await
        .expect("hook report accepted");
    assert_eq!(report["applied"], true, "{report}");
    assert_eq!(report["state"], "working", "{report}");
    hold.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    let attention =
        notification_transition(&rig, &message_id, NotificationState::AttentionRequired)
            .expect("durable attention transition");
    assert_eq!(attention.data.unwrap()["cause"], "verify_failed");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        0,
        "a live Working edge must withhold Enter"
    );
    assert!(
        rig.tmux
            .capture(&pane)
            .contains(&compact_doorbell(&message_id)),
        "the withheld doorbell should remain available for reconciliation"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_before_submit_clears_only_the_exact_doorbell_and_advances_fifo() {
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

    let cleared =
        wait_for_workspace_fact(&rig, &first_id, "notification_claimed_staged_cleared").await;
    rig.ev
        .wait_event(Duration::from_secs(5), |event| {
            event["event"] == "messages.changed"
                && event["seq"] == cleared.seq
                && event["data"]["changed"]
                    .as_array()
                    .is_some_and(|areas| areas.iter().any(|area| area == "notifications"))
        })
        .await;
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_row = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(first_id.as_str()))
        .expect("claimed message remains visible");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"], "staged",
        "the compatibility state preserves that bytes crossed the write boundary"
    );
    assert_eq!(
        first_row["recipients"][0]["notification"]["settlement"],
        "withdrawn_by_claim"
    );
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Submitted),
        0
    );
    assert!(!rig.tmux.capture(&pane).contains(&first_id));

    let second =
        send_workspace_message(&rig, "claim-before-submit-second", "Second", "second body").await;
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &second_id, NotificationState::Writing).await;
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
async fn changed_chrome_does_not_receipt_a_swallowed_compact_doorbell() {
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
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Staged).await;
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;

    let attention =
        notification_transition(&rig, &message_id, NotificationState::AttentionRequired)
            .expect("durable attention transition");
    assert_eq!(attention.data.unwrap()["cause"], "ack_timeout");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Notified),
        0
    );
    let screen = rig.tmux.capture(&pane);
    assert!(screen.contains(&compact_doorbell(&message_id)), "{screen}");
    assert!(screen.contains("Ctx: 77%"), "{screen}");

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
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "Enter"]);
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        1
    );

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
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "q"]);
    wait_for_pane_mode(&mut rig, &pane, false).await;
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
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
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
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
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0
    );

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
    wait_for_notification_state(&mut rig, &replacement_id, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0
    );

    hold.add_permits(1);
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
    assert_eq!(
        blocked_data["pre_write_cause"], "write_readiness_changed",
        "the original process proof must fail closed before reconciliation"
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
async fn an_unprovable_pinned_binding_blocks_once_and_withdrawal_advances_fifo() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "process_names = [\"python3\", \"python\", \"Python\", \"cat\", \"sh\", \"dash\"]",
        "process_names = [\"not-a-real-fixture-process\"]",
    );
    let mut rig = Rig::new(
        "workspace-unprovable-pinned-binding",
        &manifest,
        &composer_pane(),
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
    assert_eq!(named["result"]["manifest"], "fix", "{named}");

    let pair = send_waiting_pair(&rig, "unprovable-binding").await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;
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
        "fix"
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
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&pair.first)));

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

    tokio::time::sleep(Duration::from_millis(600)).await;
    for _ in 0..3 {
        let repeated = rig
            .ctl
            .request(
                "pane.label",
                json!({"target": pane, "label": "worker", "manifest": "fix"}),
            )
            .await;
        assert_eq!(repeated["result"]["label"], "worker", "{repeated}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(notification_attempts(&rig, &pair.first).len(), 1);
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
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&pair.second)));

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
async fn a_manifest_without_composer_ownership_blocks_once_and_withdrawal_advances_fifo() {
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

    let pair = send_waiting_pair(&rig, "composer-semantic-missing").await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;
    assert_only_oldest_attempt_exists(&rig, &pair);
    let blocked = notification_transition(&rig, &pair.first, NotificationState::BlockedPreWrite)
        .expect("the static manifest gap is durable");
    let fact = blocked.data.as_ref().expect("blocked transition has data");
    assert_eq!(fact["pre_write_cause"], "composer_semantic_missing");
    assert_eq!(fact["pre_write_observation"]["selected_manifest"], "fix");
    assert!(fact["pre_write_observation"]["binding"].is_object());
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        assert_eq!(notification_state_count(&rig, &pair.first, state), 0);
    }
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&pair.first)));

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("blocked message remains visible");
    assert_eq!(
        first["recipients"][0]["notification"]["pre_write_cause"],
        "composer_semantic_missing"
    );
    assert_eq!(first["recipients"][0]["can_withdraw_notification"], true);
    let attempt_id = fact["attempt_id"].clone();
    let recipient = first["recipients"][0]["recipient"].clone();

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    assert_eq!(notification_attempts(&rig, &pair.first).len(), 1);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::BlockedPreWrite),
        1,
        "restart duplicated the static pre-write block"
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
    wait_for_notification_state(&mut rig, &pair.second, NotificationState::BlockedPreWrite).await;
    assert_eq!(notification_attempts(&rig, &pair.second).len(), 1);
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&pair.second)));

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
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&message_id)));

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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let screen = wait_for_doorbell(&rig, &pane, &message_id).await;
    assert!(screen.contains(&compact_doorbell(&message_id)));
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
