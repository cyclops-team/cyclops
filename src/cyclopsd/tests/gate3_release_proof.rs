//! Dedicated Gate 3 Release Proof integration suite.
//!
//! Covers the four Gate 3 proof invariants:
//! 1. Multi-alarm clear is one append-only batch fact after complete validation.
//! 2. Both mailbox notification workers and supported legacy direct-delivery workers
//!    expose an exact in-flight attempt under the queue mutex.
//! 3. Supervised child recovery failure visibly faults the worker and never silently restarts.
//! 4. Composer barriers cannot leak between the write boundary and durable state transition.

mod common;

use std::fs;
use std::path::PathBuf;
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
// Invariant 1: Multi-alarm clear is one append-only batch fact after complete validation
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
// Invariant 2: Exact in-flight attempt is exposed under queue mutex
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

        // 1. Query quiesce under the mutex boundary
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

        // 2. Query worker registry and WorkerState.current directly under the queue mutex
        let (current_msg_id, current_attempt_id) = rig
            .daemon
            .mailbox_worker_current_for_test("worker")
            .expect("mailbox worker must own exact in-flight attempt under queue mutex");
        assert_eq!(
            current_msg_id, expected_msg_id,
            "worker current job msg_id must match"
        );

        // 3. Query messages snapshot under mutex
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
        let expected_attempt = current_attempt_id.expect("active attempt id must be Some");
        assert_eq!(
            recip.notification.attempt_id,
            Some(expected_attempt),
            "worker current job attempt_id must match snapshot attempt_id"
        );

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

        let (send_res, expected_legacy_id) = tokio::join!(
            rig.daemon.deliver_payload(
                "admin",
                serde_json::from_value::<MsgSendParams>(json!({
                    "to": ["legacy_worker"],
                    "subject": "Legacy hold",
                    "body": "Legacy payload",
                    "client_key": "legacy-in-flight"
                }))
                .unwrap(),
            ),
            async {
                tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
                    .await
                    .expect("legacy delivery reached write boundary")
                    .expect("pause channel stayed open");

                // 1. Query quiesce under the mutex boundary for legacy worker
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
                let expected_legacy_id = in_flight_str.split(" -> ").next().unwrap().to_string();

                // 2. Query legacy worker registry with exact session and pane key
                let decoy_pane = "%99".to_string();
                assert_eq!(
                    rig.daemon.legacy_worker_current_for_test(0, &decoy_pane),
                    None,
                    "decoy pane must not match any worker current job"
                );
                let current_legacy_msg_id = rig
                    .daemon
                    .legacy_worker_current_for_test(0, &pane)
                    .expect("legacy worker must own exact in-flight job under queue mutex");
                assert_eq!(
                    current_legacy_msg_id, expected_legacy_id,
                    "legacy worker current job must match in-flight handle"
                );

                // Release pause and finish
                hold.add_permits(1);
                expected_legacy_id
            }
        );
        let send_res = send_res.expect("deliver payload success");
        assert_eq!(send_res["msg_id"], expected_legacy_id);

        rig.shutdown().await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Invariant 3: Supervised child recovery failure visibly faults the worker and never silently restarts
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn supervised_child_recovery_failure_visibly_faults_and_never_restarts() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "gate3-recovery-failure-fault",
        &manifest,
        &composer_pane(),
        "receipt_block_ms = 500\n",
    )
    .await;

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // Pause once at pre_submit (when state is Staged), signal the test,
    // arm the exact recovery append failure, then release into panic.
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));

    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let release_rx = Arc::clone(&release_rx);
        Box::pin(async move {
            if current == "pre_submit" {
                let _ = entered_tx.send(());
                let rx = release_rx.lock().await.take();
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
                panic!("injected supervised child crash at pre_submit");
            }
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
                    "subject": "Crash test",
                    "body": "Payload to crash",
                    "client_key": "crash-job"
                }),
            )
            .await
    });

    // Wait until delivery reaches pre_submit (current state is Staged)
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("reached pre_submit")
        .expect("channel open");

    // Inspect the exact in-flight attempt owned by the worker
    let (_, in_flight_attempt) = rig
        .daemon
        .mailbox_worker_current_for_test("worker")
        .expect("in-flight worker job");
    let target_attempt = in_flight_attempt.expect("in-flight attempt id");

    // Arm the exact-attempt recovery append failure directly on the store
    rig.daemon.fail_notification_recovery_append(target_attempt);

    // Release pause into panic: child crashes, supervisor catches it, recover_failed_job tries to
    // record attention in journal for target_attempt, store append fails, recover_failed_job returns false,
    // and supervisor sets Worker::set_fault.
    let _ = release_tx.send(());
    let _ = send_task.await;

    // Wait until daemon status reflects the worker fault with exact code notification_recovery_storage_failed
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found_diagnostic = false;
    let mut fault_msg_id = String::new();
    let mut fault_attempt_id = String::new();
    while Instant::now() < deadline {
        let status = rig.ctl.request("status", json!({})).await;
        if let Some(diagnostics) = status["result"]["diagnostics"].as_array() {
            if let Some(diag) = diagnostics
                .iter()
                .find(|d| d["recipient_label"] == "worker")
            {
                assert_eq!(
                    diag["code"], "notification_recovery_storage_failed",
                    "diagnostic code must be exact"
                );
                assert_eq!(diag["recipient_label"], "worker");
                assert_eq!(diag["pane_id"], pane);
                assert!(diag["notification_attempt"].is_string());
                fault_msg_id = diag["message_id"].as_str().unwrap().to_string();
                fault_attempt_id = diag["notification_attempt"].as_str().unwrap().to_string();
                found_diagnostic = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found_diagnostic,
        "status diagnostics must contain visible worker fault for recipient worker"
    );

    // Send follower message to the same worker
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

    // Trigger a later pane/route edge
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "idle_new_edge"]);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Assert follower remains NotStarted
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
        "follower must remain NotStarted under permanently faulted worker"
    );

    // Assert diagnostic persists
    let status_after = rig.ctl.request("status", json!({})).await;
    let diags_after = status_after["result"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(
        diags_after
            .iter()
            .any(|d| d["code"] == "notification_recovery_storage_failed"
                && d["message_id"] == fault_msg_id
                && d["notification_attempt"] == fault_attempt_id),
        "diagnostic must persist across subsequent route edges: {diags_after:?}"
    );

    // Assert no follower bytes or doorbell markers ever reach the pane history
    let history = String::from_utf8_lossy(
        &rig.tmux
            .run(&["capture-pane", "-p", "-S", "-", "-t", &pane])
            .stdout,
    )
    .to_string();
    assert!(
        !history.contains(msg2_id),
        "no follower bytes or markers may reach the pane: {history}"
    );

    rig.shutdown().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Invariant 4: Composer barriers cannot leak between write boundary and durable transition
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

    // Arm one-shot seam: panic after latch_hold claims composer barrier but before record_writing
    rig.daemon.fail_pre_record_writing();

    let send1 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Pre-durable exit",
                "body": "First payload",
                "client_key": "barrier-msg1"
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let first_id = send1["msg_id"].as_str().unwrap().to_string();

    // Because the worker exited before the first durable transition, UnwrittenHold dropped and rolled back
    // the in-memory hold. recover_failed_job requeued the same attempt cleanly.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snap = rig
            .daemon
            .messages_snapshot_for_test("admin", 10)
            .expect("snapshot");
        let row = snap
            .rows
            .iter()
            .find(|r| r.message_id.as_str() == first_id)
            .expect("first message in snapshot");
        if row.recipients[0].notification.state == cyclops_proto::MessageNotificationState::Notified
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "same-attempt recovery did not reach notified"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Collect every notification fact attempt_id for first message and assert one unique id across crash/recovery
    let lines = workspace_lines(&rig);
    let mut first_attempt_ids = std::collections::HashSet::new();
    for line in &lines {
        if line.id == first_id {
            if let Some(data) = &line.data {
                if let Some(attempt) = data.get("attempt_id").and_then(|a| a.as_str()) {
                    first_attempt_ids.insert(attempt.to_string());
                }
            }
        }
    }
    assert_eq!(
        first_attempt_ids.len(),
        1,
        "all notification facts for first message must share one unique attempt_id: {first_attempt_ids:?}"
    );
    let first_attempt_id = first_attempt_ids.into_iter().next().unwrap();

    // Observe hold status for the pane
    let (hold_state, _) = rig
        .daemon
        .composer_hold_for_test(0, &pane)
        .expect("detection entry for pane");
    assert!(
        matches!(
            hold_state,
            cyclops_proto::ComposerHold::Clear | cyclops_proto::ComposerHold::TurnStarted { .. }
        ),
        "composer barrier must be clear or turn started: {hold_state:?}"
    );

    // Now send a follower message to prove the composer barrier was not leaked
    let send2 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Second message",
                "body": "Second payload",
                "client_key": "barrier-msg2"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let second_id = send2["msg_id"].as_str().unwrap().to_string();

    // Settle the first notification by claiming it
    rig.daemon
        .claim_message_for_test("worker", &first_id)
        .expect("claim first message");

    // Once first is claimed, the follower proceeds cleanly through FIFO without barrier leaks
    let deadline2 = Instant::now() + Duration::from_secs(10);
    loop {
        let snap = rig
            .daemon
            .messages_snapshot_for_test("admin", 10)
            .expect("snapshot");
        let row2 = snap
            .rows
            .iter()
            .find(|r| r.message_id.as_str() == second_id)
            .expect("second message in snapshot");
        if row2.recipients[0].notification.state
            == cyclops_proto::MessageNotificationState::Notified
        {
            break;
        }
        assert!(
            Instant::now() < deadline2,
            "second message did not reach notified"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Verify journal transitions for first_id: exactly one writing, one staged, one submitted
    let writing_count = lines
        .iter()
        .filter(|l| {
            l.id == first_id
                && l.data
                    .as_ref()
                    .is_some_and(|d| d.get("state") == Some(&json!("writing")))
        })
        .count();
    assert_eq!(
        writing_count, 1,
        "exactly one writing transition in journal"
    );
    let staged_count = lines
        .iter()
        .filter(|l| {
            l.id == first_id
                && l.data
                    .as_ref()
                    .is_some_and(|d| d.get("state") == Some(&json!("staged")))
        })
        .count();
    assert_eq!(staged_count, 1, "exactly one staged transition in journal");
    let submitted_count = lines
        .iter()
        .filter(|l| {
            l.id == first_id
                && l.data
                    .as_ref()
                    .is_some_and(|d| d.get("state") == Some(&json!("submitted")))
        })
        .count();
    assert_eq!(
        submitted_count, 1,
        "exactly one submitted transition in journal"
    );

    // Follower marker also appears in journal with unique attempt_id
    let lines_after = workspace_lines(&rig);
    let mut second_attempt_ids = std::collections::HashSet::new();
    for line in &lines_after {
        if line.id == second_id {
            if let Some(data) = &line.data {
                if let Some(attempt) = data.get("attempt_id").and_then(|a| a.as_str()) {
                    second_attempt_ids.insert(attempt.to_string());
                }
            }
        }
    }
    assert_eq!(second_attempt_ids.len(), 1);
    let second_attempt_id = second_attempt_ids.into_iter().next().unwrap();
    let second_attempt = cyclops_proto::NotificationAttemptId::parse(&second_attempt_id)
        .expect("parse second attempt id");
    let second_locator = cyclops_proto::notification_attempt_claim_locator(second_attempt);

    // Capture full pane history to assert exact doorbell markers and executions
    let history = String::from_utf8_lossy(
        &rig.tmux
            .run(&["capture-pane", "-p", "-S", "-", "-t", &pane])
            .stdout,
    )
    .to_string();

    let first_attempt = cyclops_proto::NotificationAttemptId::parse(&first_attempt_id)
        .expect("parse first attempt id");
    let first_locator = cyclops_proto::notification_attempt_claim_locator(first_attempt);

    // Assert both doorbells were delivered and executed exactly once in the pane
    assert!(
        history.contains(first_locator.as_str()),
        "first doorbell marker must appear in pane history: {history}"
    );
    assert!(
        history.contains(second_locator.as_str()),
        "second doorbell marker must appear in pane history: {history}"
    );
    assert_eq!(
        history.matches("FAKETUI-WORKING").count(),
        2,
        "exact two submits must execute across the two messages: {history}"
    );

    rig.shutdown().await;
}
