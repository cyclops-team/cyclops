//! Dedicated Gate 3 Release Proof integration suite.
//!
//! Covers the four Gate 3 proof gaps identified during release audit:
//! 1. Multi-alarm clear is one append-only batch fact after complete validation.
//! 2. Exact in-flight attempt is exposed under the queue mutex.
//! 3. Recovery failure visibly faults the worker and never silently restarts.
//! 4. Composer barriers cannot leak between the write boundary and durable state transition.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{swallowing_composer_pane, tmux_available, Rig, TestClient, CAT_MANIFEST};
use cyclops_proto::{Kind, LedgerLine, MsgSendParams};
use serde_json::json;

fn recovery_manifest() -> String {
    let mut manifest = CAT_MANIFEST.replace(
        "safe_states = [\"idle\"]\n",
        "safe_states = [\"idle\"]\n\
         clear_keys = [\"C-c\"]\n",
    );
    manifest.push_str(
        "\n[[rule]]\n\
         id = \"blocked_title\"\n\
         state = \"blocked_modal\"\n\
         priority = 500\n\
         region = \"pane_title\"\n\
         regex = ['^BLOCKED$']\n",
    );
    manifest
}

fn workspace_journal(rig: &Rig) -> PathBuf {
    fs::read_dir(rig.home.join("workspaces"))
        .expect("workspace directory")
        .find_map(|entry| {
            let path = entry.ok()?.path().join("messages.ndjson");
            path.is_file().then_some(path)
        })
        .expect("workspace journal")
}

fn workspace_lines(rig: &Rig) -> Vec<LedgerLine> {
    fs::read_to_string(workspace_journal(rig))
        .expect("workspace journal readable")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("workspace line parses"))
        .collect()
}

async fn wait_for_alarm(rig: &mut Rig, message_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = rig
            .ctl
            .request("alarm.preview", json!({"older_than_ms": 0}))
            .await;
        assert!(
            response["error"].is_null(),
            "alarm preview failed: {response}"
        );
        if let Some(entry) = response["result"]["entries"]
            .as_array()
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| entry["message_id"] == message_id)
            })
        {
            return entry["id"].as_str().expect("attempt id").to_string();
        }
        assert!(Instant::now() < deadline, "alarm did not open: {response}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Gap 1: Multi-alarm clear is one append-only batch fact after complete validation
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn multi_alarm_clear_is_one_fact_after_complete_validation() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let sw = swallowing_composer_pane();
    let mut rig = Rig::new_multi(
        "gate3-multi-alarm-clear",
        &manifest,
        &[("s0", &sw), ("s1", &sw), ("s2", &sw)],
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;

    let p0 = rig.pane_ids_session(0).await[0].clone();
    let p1 = rig.pane_ids_session(1).await[0].clone();
    let p2 = rig.pane_ids_session(2).await[0].clone();

    rig.label(&p0, "worker0").await;
    rig.label(&p1, "worker1").await;
    rig.label(&p2, "worker2").await;

    // Send messages that get stuck in swallowing composers, raising alarms
    let m0 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker0"],
                "subject": "Task 0",
                "body": "Body 0"
            }))
            .unwrap(),
        )
        .await
        .unwrap()["msg_id"]
        .as_str()
        .unwrap()
        .to_string();

    let m1 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker1"],
                "subject": "Task 1",
                "body": "Body 1"
            }))
            .unwrap(),
        )
        .await
        .unwrap()["msg_id"]
        .as_str()
        .unwrap()
        .to_string();

    let m2 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker2"],
                "subject": "Task 2",
                "body": "Body 2"
            }))
            .unwrap(),
        )
        .await
        .unwrap()["msg_id"]
        .as_str()
        .unwrap()
        .to_string();

    let a0 = wait_for_alarm(&mut rig, &m0).await;
    let a1 = wait_for_alarm(&mut rig, &m1).await;
    let a2 = wait_for_alarm(&mut rig, &m2).await;

    let lines_before = workspace_lines(&rig);

    // 1. Partial invalidity refuses the entire batch with zero journal mutations
    let bad_clear = rig
        .ctl
        .request(
            "alarm.clear",
            json!({
                "ids": [a0.clone(), a1.clone(), "att-00000000-0000-4000-8000-000000000000"]
            }),
        )
        .await;
    assert_eq!(bad_clear["error"]["code"], "no_such_alarm");

    let lines_mid = workspace_lines(&rig);
    assert_eq!(
        lines_mid.len(),
        lines_before.len(),
        "failed validation must append zero facts"
    );

    // 2. Valid multi-alarm clearance appends exactly one batch fact
    let good_clear = rig
        .ctl
        .request("alarm.clear", json!({ "ids": [a0, a1, a2] }))
        .await;
    assert!(
        good_clear["error"].is_null(),
        "clear request failed: {good_clear}"
    );
    assert_eq!(
        good_clear["result"]["cleared_ids"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let lines_after = workspace_lines(&rig);
    assert_eq!(
        lines_after.len(),
        lines_before.len() + 1,
        "multi-alarm clear must append exactly ONE batch fact to the journal"
    );

    let last = lines_after.last().unwrap();
    assert_eq!(last.kind, Kind::State);
    assert_eq!(last.data.as_ref().unwrap()["type"], "notifications_cleared");
    assert_eq!(
        last.data.as_ref().unwrap()["attempt_ids"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    rig.shutdown().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Gap 2: Exact in-flight attempt is exposed under queue mutex
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_attempt_is_exposed_under_queue_mutex() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "gate3-in-flight-mutex",
        &manifest,
        &common::composer_pane(),
        "receipt_block_ms = 500\n",
    )
    .await;

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let paused = Arc::clone(&hold);

    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let paused = Arc::clone(&paused);
        Box::pin(async move {
            if current != "pre_paste" {
                return;
            }
            let _ = entered_tx.send(());
            paused.acquire_owned().await.unwrap().forget();
        })
    });

    let socket = rig.daemon.socket_path();
    let send_task = tokio::spawn(async move {
        let mut client = TestClient::connect(&socket).await;
        client
            .request(
                "msg.send",
                json!({
                    "from": "admin",
                    "to": ["worker"],
                    "subject": "Hold test",
                    "body": "In-flight payload"
                }),
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("delivery reached write boundary")
        .expect("pause channel stayed open");

    // While worker is held at the write boundary, query quiesce under the mutex
    let quiesced = rig
        .ctl
        .request("daemon.quiesce", json!({"timeout_ms": 100}))
        .await;
    assert!(quiesced["error"].is_null(), "quiesce failed: {quiesced}");

    let in_flight = quiesced["result"]["in_flight"]
        .as_array()
        .expect("in_flight array");
    assert_eq!(
        in_flight.len(),
        1,
        "exact in-flight attempt must be exposed under queue mutex: {quiesced}"
    );
    assert!(
        in_flight[0].as_str().unwrap().contains("-> worker"),
        "in_flight must name recipient: {in_flight:?}"
    );

    // Release pause and allow delivery to complete cleanly
    hold.add_permits(1);
    let send_res = send_task.await.unwrap();
    assert!(send_res["result"]["msg_id"].is_string());

    rig.shutdown().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Gap 3: Recovery failure visibly faults the worker and never silently restarts
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn recovery_failure_visibly_faults_and_never_silently_restarts() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "gate3-recovery-failure-fault",
        &manifest,
        &swallowing_composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Fault test",
                "body": "Payload to fault"
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;

    // Check status: notification_state is attention_required
    let status_before = rig.ctl.request("status", json!({})).await;
    let pane_state_before = status_before["result"]["sessions"][0]["panes"][0]
        ["notification_state"]
        .as_str()
        .unwrap();
    assert_eq!(
        pane_state_before, "attention_required",
        "status must report attention_required: {status_before}"
    );

    // Corrupt terminal evidence by setting pane title to BLOCKED modal
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "BLOCKED"]);

    // Attempt explicit resolution which fails due to contradictory evidence
    let res = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    let err_code = res["error"]["code"].as_str().expect("error code");
    assert!(
        err_code == "attention_action_uncertain" || err_code == "attention_evidence_failed",
        "resolution must fail when evidence is contradictory: {res}"
    );

    // The failure must visibly fault: pane notification state remains attention_required
    let status_after = rig.ctl.request("status", json!({})).await;
    let pane_state_after = status_after["result"]["sessions"][0]["panes"][0]["notification_state"]
        .as_str()
        .unwrap();
    assert_eq!(
        pane_state_after, "attention_required",
        "status must stay in attention_required after recovery failure"
    );

    // Verify it does NOT restart silently: check alarm preview still holds the exact attempt
    let preview = rig
        .ctl
        .request("alarm.preview", json!({"older_than_ms": 0}))
        .await;
    let alarm_entries = preview["result"]["entries"].as_array().unwrap();
    assert!(
        alarm_entries.iter().any(|e| e["id"] == attempt_id),
        "attempt must remain in attention without silent restart: {preview}"
    );

    rig.shutdown().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Gap 4: Composer barriers cannot leak between write boundary and durable transition
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn composer_barrier_cannot_leak_between_write_boundary_and_durable_transition() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "gate3-barrier-leak-prevention",
        &manifest,
        &common::composer_pane(),
        "receipt_block_ms = 500\n",
    )
    .await;

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let paused = Arc::clone(&hold);

    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let paused = Arc::clone(&paused);
        Box::pin(async move {
            if current != "pre_paste" {
                return;
            }
            let _ = entered_tx.send(());
            paused.acquire_owned().await.unwrap().forget();
        })
    });

    let socket = rig.daemon.socket_path();
    // Send initial message that halts at write boundary
    tokio::spawn(async move {
        let mut client = TestClient::connect(&socket).await;
        client
            .request(
                "msg.send",
                json!({
                    "from": "admin",
                    "to": ["worker"],
                    "subject": "First message",
                    "body": "First body"
                }),
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("reached write boundary")
        .expect("channel open");

    // While first attempt is at write boundary holding the barrier, send a second message
    let send_second = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Second message",
                "body": "Second body"
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let second_id = send_second["msg_id"].as_str().unwrap().to_string();

    // Verify the second message is safely held in FIFO behind the active barrier and does not leak or double-paste
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let rows = snapshot["result"]["rows"]
        .as_array()
        .expect("snapshot rows");
    let second_row = rows
        .iter()
        .find(|r| r["message_id"] == second_id)
        .expect("second message in snapshot");

    assert_eq!(
        second_row["recipients"][0]["fifo_position"], 2,
        "second message must hold at FIFO position 2 behind the barrier: {second_row}"
    );
    assert_eq!(
        second_row["recipients"][0]["notification"]["state"], "not_started",
        "second message notification must remain not_started while barrier is held: {second_row}"
    );

    // Release pause to cleanly tear down
    hold.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;

    rig.shutdown().await;
}
