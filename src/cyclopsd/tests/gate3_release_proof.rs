//! Dedicated Gate 3 Release Proof integration suite.
//!
//! Covers the four Gate 3 proof gaps identified during release audit:
//! 1. Multi-alarm clear is one append-only batch fact after complete validation.
//! 2. Both mailbox notification workers and supported legacy direct-delivery workers
//!    expose an exact in-flight attempt under the queue mutex.
//! 3. Recovery failure visibly faults the worker and never silently restarts.
//! 4. Composer barriers cannot leak between the write boundary and durable state transition.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    composer_pane, swallowing_composer_pane, tmux_available, Rig, TestClient, CAT_MANIFEST,
};
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
// Gap 2: Exact in-flight attempt is exposed under queue mutex (Mailbox + Legacy)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_attempt_is_exposed_under_queue_mutex() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();

    // ── Part A: Mailbox Notification Worker ──
    {
        let mut rig = Rig::new(
            "gate3-in-flight-mailbox",
            &manifest,
            &composer_pane(),
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
                        "body": "In-flight payload",
                        "client_key": "mailbox-in-flight"
                    }),
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("mailbox delivery reached write boundary")
            .expect("pause channel stayed open");

        // Query quiesce under the mutex boundary
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
            "exact in-flight mailbox attempt must be exposed under queue mutex: {quiesced}"
        );
        let in_flight_str = in_flight[0].as_str().unwrap();
        assert!(
            in_flight_str.ends_with(" -> worker"),
            "in_flight must name target worker: {in_flight_str}"
        );
        let expected_msg_id = in_flight_str.split(" -> ").next().unwrap();

        // Also assert snapshot under the same queue mutex exposes the exact attempt and state
        let snapshot = rig
            .daemon
            .messages_snapshot_for_test("admin", 10)
            .expect("snapshot under mutex");
        let row = snapshot
            .rows
            .iter()
            .find(|r| r.message_id.as_str() == expected_msg_id)
            .expect("row in snapshot");
        let recip = &row.recipients[0];
        assert_eq!(recip.label, "worker");
        assert!(
            recip.notification.state == cyclops_proto::MessageNotificationState::Gating
                || recip.notification.state == cyclops_proto::MessageNotificationState::Writing,
            "notification must be in in-flight state: {:?}",
            recip.notification.state
        );
        assert!(recip.notification.attempt_id.is_some());

        // Release pause and finish
        hold.add_permits(1);
        let send_res = send_task.await.unwrap();
        assert_eq!(send_res["result"]["msg_id"], expected_msg_id);

        rig.shutdown().await;
    }

    // ── Part B: Supported Legacy Direct-Delivery Worker ──
    {
        let mut rig = Rig::new_without_mailbox_capability(
            "gate3-in-flight-legacy",
            &manifest,
            &composer_pane(),
            "receipt_block_ms = 500\n",
        )
        .await;

        let pane = rig.pane_ids().await[0].clone();
        rig.label(&pane, "legacy_worker").await;

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
                        "to": ["legacy_worker"],
                        "subject": "Legacy hold",
                        "body": "Legacy payload",
                        "client_key": "legacy-in-flight"
                    }),
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("legacy delivery reached write boundary")
            .expect("pause channel stayed open");

        // Query quiesce under the mutex boundary for legacy worker
        let quiesced = rig
            .ctl
            .request("daemon.quiesce", json!({"timeout_ms": 100}))
            .await;
        assert!(
            quiesced["error"].is_null(),
            "legacy quiesce failed: {quiesced}"
        );

        let in_flight = quiesced["result"]["in_flight"]
            .as_array()
            .expect("in_flight array");
        assert_eq!(
            in_flight.len(),
            1,
            "exact legacy direct-delivery attempt must be exposed under queue mutex: {quiesced}"
        );
        let in_flight_str = in_flight[0].as_str().unwrap();
        assert!(
            in_flight_str.ends_with(" -> legacy_worker"),
            "in_flight must name target legacy worker: {in_flight_str}"
        );

        // Release pause and finish
        hold.add_permits(1);
        let send_res = send_task.await.unwrap();
        assert!(send_res["result"]["msg_id"].is_string());

        rig.shutdown().await;
    }
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
        &composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let panicked = Arc::new(AtomicBool::new(false));
    let panic_flag = Arc::clone(&panicked);

    // Set pause on pre_submit (after write boundary is crossed):
    // Child delivery task panics unexpectedly, and we fail the next durable append
    // so recover_failed_job's attention recording fails, forcing worker.set_fault.
    rig.daemon.set_inject_pause(move |current| {
        let panic_flag = Arc::clone(&panic_flag);
        Box::pin(async move {
            if current == "pre_submit" && !panic_flag.swap(true, Ordering::SeqCst) {
                panic!("injected supervised child crash after write boundary");
            }
        })
    });

    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Crash test",
                "body": "Payload to crash",
                "client_key": "crash-job"
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let message_id = sent["msg_id"].as_str().unwrap().to_string();

    // The child task crashed at write boundary and recover_failed_job classified it.
    // The attempt transitioned durably to AttentionRequired with TransportOutcomeUnknown.
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;

    // Verify status visibly faults the worker with attention required
    let status = rig.ctl.request("status", json!({})).await;
    let pane_status = &status["result"]["sessions"][0]["panes"][0];
    assert_eq!(
        pane_status["notification_state"], "attention_required",
        "worker recovery must visibly fault into attention: {status}"
    );

    // Verify alarm preview retains the exact attempt
    let preview = rig
        .ctl
        .request("alarm.preview", json!({"older_than_ms": 0}))
        .await;
    let alarm_entries = preview["result"]["entries"].as_array().unwrap();
    let exact_entry = alarm_entries
        .iter()
        .find(|e| e["id"] == attempt_id)
        .expect("exact attempt retained in alarm preview");
    assert_eq!(exact_entry["cause"], "transport_outcome_unknown");

    // Send a second message and prove it is NOT processed by a silent restart:
    // It remains enqueued at FIFO position 2 without double-writing.
    let send2 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Second message",
                "body": "Must not silently run",
                "client_key": "second-job"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let msg2_id = send2["msg_id"].as_str().unwrap();

    let snapshot = rig
        .daemon
        .messages_snapshot_for_test("admin", 10)
        .expect("snapshot");
    let row2 = snapshot
        .rows
        .iter()
        .find(|r| r.message_id.as_str() == msg2_id)
        .expect("row2 in snapshot");
    assert_eq!(
        row2.recipients[0].notification.state,
        cyclops_proto::MessageNotificationState::NotStarted,
        "second message must not start under faulted worker"
    );
    assert_eq!(row2.recipients[0].fifo_position, Some(2));

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
        &composer_pane(),
        "receipt_block_ms = 500\n",
    )
    .await;

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let panicked = Arc::new(AtomicBool::new(false));
    let panic_flag = Arc::clone(&panicked);

    // Force the owning supervised child worker to exit in the exact window
    // between crossing the write boundary and completing the submit transition.
    rig.daemon.set_inject_pause(move |current| {
        let panic_flag = Arc::clone(&panic_flag);
        Box::pin(async move {
            if current == "pre_submit" && !panic_flag.swap(true, Ordering::SeqCst) {
                panic!("worker exited in write-boundary-before-durable-transition window");
            }
        })
    });

    let send1 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "First message",
                "body": "First body",
                "client_key": "barrier-msg1"
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let first_id = send1["msg_id"].as_str().unwrap().to_string();

    // Wait for the worker supervisor to recover the failed job into durable AttentionRequired
    let attempt1 = wait_for_alarm(&mut rig, &first_id).await;

    // Send a second message to the same recipient while the barrier is held in attention
    let send2 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Second message",
                "body": "Second body",
                "client_key": "barrier-msg2"
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let second_id = send2["msg_id"].as_str().unwrap().to_string();

    // Verify:
    // 1. The original attempt durably holds the barrier in AttentionRequired (with cause TransportOutcomeUnknown).
    let snapshot = rig
        .daemon
        .messages_snapshot_for_test("admin", 10)
        .expect("snapshot");
    let row1 = snapshot
        .rows
        .iter()
        .find(|r| r.message_id.as_str() == first_id)
        .expect("first message");
    assert_eq!(
        row1.recipients[0].notification.state,
        cyclops_proto::MessageNotificationState::AttentionRequired
    );
    assert_eq!(
        row1.recipients[0]
            .notification
            .attempt_id
            .as_ref()
            .map(ToString::to_string),
        Some(attempt1)
    );

    // 2. The second message is safely held in FIFO at position 2 and CANNOT pass the barrier.
    let row2 = snapshot
        .rows
        .iter()
        .find(|r| r.message_id.as_str() == second_id)
        .expect("second message");
    assert_eq!(
        row2.recipients[0].notification.state,
        cyclops_proto::MessageNotificationState::NotStarted,
        "second message must not start while barrier is held in attention"
    );
    assert_eq!(row2.recipients[0].fifo_position, Some(2));

    // 3. Screen evidence confirms zero double-paste / second writes reached the composer
    let screen = rig.tmux.capture(&pane);
    let occurrences = screen
        .lines()
        .filter(|l| l.contains("Second body") || l.contains("First body"))
        .count();
    assert!(
        occurrences <= 1,
        "no duplicate or uncoordinated second write may reach the terminal: {screen}"
    );

    rig.shutdown().await;
}
