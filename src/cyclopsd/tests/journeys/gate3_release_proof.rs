//! Dedicated Gate 3 Release Proof integration suite.
//!
//! Covers the four Gate 3 proof invariants:
//! 1. Multi-alarm clear is one append-only batch fact after complete validation.
//! 2. Mailbox notification workers expose an exact in-flight attempt under
//!    the queue mutex.
//! 3. Supervised child recovery failure visibly faults the worker and never silently restarts.
//! 4. Composer barriers cannot leak between the write boundary and durable state transition.

use crate::common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    composer_pane, swallowing_composer_pane, tmux_available, Rig, TestClient, CAT_MANIFEST,
};
use cyclops_proto::{Kind, LedgerLine, MsgSendParams, NotificationAttemptId};
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
        // An alarm is a durable attention fact, so its append publishes
        // `messages.changed`; wake on it and ask the preview again.
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
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

    // An alarm is a physical post-write failure. Park every doorbell after
    // its paste, swap each pane's occupant for a shell, and release: the
    // pre-Enter occupant check refuses and each attempt closes to attention.
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let release_seam = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let release = Arc::clone(&release_seam);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            release
                .acquire_owned()
                .await
                .expect("seam release")
                .forget();
        })
    });

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

    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
            .await
            .expect("every doorbell reached the pre-submit seam")
            .expect("seam channel open");
    }
    for pane in [&p0, &p1, &p2] {
        rig.tmux.run_ok(&["respawn-pane", "-k", "-t", pane, "sh"]);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let swapped = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| session["panes"].as_array())
            .flatten()
            .filter(|row| {
                ["sh", "bash", "dash"].contains(&row["current_command"].as_str().unwrap_or(""))
            })
            .count();
        if swapped == 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the shells never took the panes: {status}"
        );
        // No event: `current_command` is a status field with no announcement.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    release.add_permits(3);

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
// Invariant 2A: Exact in-flight mailbox attempt is exposed under queue mutex
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn in_flight_mailbox_attempt_is_exposed_under_queue_mutex_with_decoy_isolation() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let sw = composer_pane();
    let mut rig = Rig::new_multi(
        "gate3-in-flight-mailbox-decoy",
        &manifest,
        &[("s0", &sw), ("s1", &sw)],
        "receipt_block_ms = 500\n",
    )
    .await;

    let p0 = rig.pane_ids_session(0).await[0].clone();
    let p1 = rig.pane_ids_session(1).await[0].clone();
    rig.label(&p0, "worker").await;
    rig.label(&p1, "worker_decoy").await;

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

    // 2. Query worker registry with decoy isolation: ignoring RecipientKey must fail
    assert_eq!(
        rig.daemon.mailbox_worker_current_for_test("worker_decoy"),
        None,
        "decoy recipient worker must not report any in-flight attempt"
    );
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

// ─────────────────────────────────────────────────────────────────────────────
// Invariant 2B: Exact in-flight legacy direct-delivery worker is exposed under queue mutex
// ─────────────────────────────────────────────────────────────────────────────

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
    let sw = composer_pane();
    let mut rig = Rig::new_multi(
        "gate3-recovery-failure-fault",
        &manifest,
        &[("s0", &sw), ("s1", &sw)],
        "receipt_block_ms = 500\n",
    )
    .await;

    let pane = rig.pane_ids_session(0).await[0].clone();
    let pane_unrelated = rig.pane_ids_session(1).await[0].clone();
    rig.label(&pane, "worker").await;
    rig.label(&pane_unrelated, "worker_unrelated").await;

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

    // ── Non-Target Append Targeting Proof ──
    // Prove that arming the fault for `target_attempt` does NOT fail an unrelated / non-target notification append.
    let unrelated_send = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker_unrelated"],
                "subject": "Unrelated notification append",
                "body": "Unrelated payload",
                "client_key": "unrelated-append-job"
            }))
            .unwrap(),
        )
        .await
        .expect("non-target notification append must succeed while target recovery fault is armed");
    let unrelated_id = unrelated_send["msg_id"].as_str().unwrap().to_string();

    // Verify unrelated append committed to journal with its own distinct attempt id
    let lines = workspace_lines(&rig);
    let unrelated_lines: Vec<_> = lines.iter().filter(|l| l.id == unrelated_id).collect();
    assert!(
        !unrelated_lines.is_empty(),
        "unrelated message must have journal entries"
    );
    for line in &unrelated_lines {
        if let Some(data) = &line.data {
            if let Some(attempt_str) = data.get("attempt_id").and_then(|a| a.as_str()) {
                assert_ne!(
                    attempt_str,
                    target_attempt.to_string(),
                    "unrelated notification attempt must differ from target_attempt"
                );
            }
        }
    }

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
                assert_eq!(
                    diag["notification_attempt"],
                    target_attempt.to_string(),
                    "fault diagnostic must name the exact target_attempt"
                );
                fault_msg_id = diag["message_id"].as_str().unwrap().to_string();
                fault_attempt_id = diag["notification_attempt"].as_str().unwrap().to_string();
                found_diagnostic = true;
                break;
            }
        }
        // No event: status diagnostics are computed on request.
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
    // A bounded window: nothing marks a notification that must not start.
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

    tokio::time::timeout(Duration::from_secs(15), async {
        let manifest = recovery_manifest();
        let sw = composer_pane();
        let mut rig = Rig::new_multi(
            "gate3-barrier-leak-prevention",
            &manifest,
            &[("s0", &sw), ("s1", &sw)],
            "receipt_block_ms = 500\n",
        )
        .await;

    let p0 = rig.pane_ids_session(0).await[0].clone();
    let p1 = rig.pane_ids_session(1).await[0].clone();
    rig.label(&p0, "worker").await;
    rig.label(&p1, "worker_unrelated").await;

    // Pause once at pre_paste on the target worker, learn the attempt id,
    // arm the exact attempt scoped fault, then release.
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);

    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let mut release_rx = release_rx.clone();
        Box::pin(async move {
            if current == "pre_paste" {
                let _ = entered_tx.send(());
                let _ = release_rx.wait_for(|&ready| ready).await;
            }
        })
    });

    // The join below holds the daemon and the rig immutably, so the wake
    // inside it comes from a subscription of its own rather than `rig.ev`.
    let mut probe = TestClient::connect(&rig.daemon.socket_path()).await;
    let ack = probe.request("events.subscribe", json!({})).await;
    assert_eq!(ack["result"]["subscribed"], true);
    let (send_res, target_attempt) = tokio::join!(
        rig.daemon.msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Pre-durable exit",
                "body": "First payload",
                "client_key": "barrier-msg1"
            }))
            .unwrap(),
        ),
        async {
            tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
                .await
                .expect("reached pre_paste")
                .expect("channel open");

            // Inspect the exact in-flight attempt owned by the worker
            let (_, in_flight_attempt) = rig
                .daemon
                .mailbox_worker_current_for_test("worker")
                .expect("in-flight worker job");
            let target_attempt = in_flight_attempt.expect("in-flight attempt id");

            // Arm the exact attempt scoped failure: panic after latch_hold claims composer barrier but before record_writing
            rig.daemon.fail_pre_record_writing_for_attempt(target_attempt);

            // Clear the pause hook so recovery retry and subsequent messages proceed unpaused
            rig.daemon.clear_inject_pause();

            // ── Concurrent Unrelated Message Non-Interference Proof ──
            // WHILE target_attempt is still paused, send an active message to `worker_unrelated`.
            // Proves that arming `target_attempt` cannot be consumed by or interfere with a concurrent
            // unrelated mailbox attempt executing while the seam is armed.
            let unrelated_send = rig
                .daemon
                .msg_send(
                    "admin",
                    serde_json::from_value::<MsgSendParams>(json!({
                        "to": ["worker_unrelated"],
                        "subject": "Unrelated message",
                        "body": "Unrelated payload",
                        "client_key": "unrelated-msg"
                    }))
                    .unwrap(),
                )
                .await
                .expect("unrelated message must deliver cleanly while target attempt is paused and armed");
            let unrelated_id = unrelated_send["msg_id"].as_str().unwrap().to_string();

            // Assert unrelated registry ownership and transitions
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = rig
                    .daemon
                    .messages_snapshot_for_test("admin", 10)
                    .expect("snapshot");
                if let Some(unrelated_row) = snap
                    .rows
                    .iter()
                    .find(|r| r.message_id.as_str() == unrelated_id)
                {
                    if unrelated_row.recipients[0].notification.state
                        == cyclops_proto::MessageNotificationState::Notified
                    {
                        break;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "unrelated message did not reach notified"
                );
                // Every journal append publishes `messages.changed`; wake on it and read
                // the journal again rather than sleep.
                probe
                    .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                        e["event"] == "messages.changed"
                    })
                    .await;
            }

            // Assert unrelated attempt != target_attempt
            let lines = workspace_lines(&rig);
            let mut unrelated_attempt_id = String::new();
            for line in lines.iter().filter(|l| l.id == unrelated_id) {
                if let Some(data) = &line.data {
                    if let Some(attempt_str) = data.get("attempt_id").and_then(|a| a.as_str()) {
                        assert_ne!(
                            attempt_str,
                            target_attempt.to_string(),
                            "unrelated attempt must differ from target_attempt"
                        );
                        unrelated_attempt_id = attempt_str.to_string();
                    }
                }
            }
            assert!(
                !unrelated_attempt_id.is_empty(),
                "unrelated attempt id must be present in journal"
            );
            let unrelated_attempt = NotificationAttemptId::parse(&unrelated_attempt_id)
                .expect("parse unrelated attempt id");
            let unrelated_locator =
                cyclops_proto::notification_attempt_claim_locator(unrelated_attempt);

            // Assert exactly one execution on unrelated pane p1
            let p1_out = rig
                .tmux
                .run(&["capture-pane", "-p", "-S", "-", "-t", &p1]);
            let p1_content = String::from_utf8_lossy(&p1_out.stdout);
            assert!(
                p1_content.contains(unrelated_locator.as_str()),
                "unrelated doorbell locator must appear on p1: {p1_content}"
            );
            assert_eq!(
                p1_content.matches("FAKETUI-WORKING").count(),
                1,
                "unrelated pane must execute exactly once: {p1_content}"
            );

            // Assert that the fault remains armed for target_attempt and was NOT consumed by the unrelated message
            assert_eq!(
                rig.daemon.fail_pre_record_writing_target_for_test(),
                Some(target_attempt),
                "fault must remain armed exclusively for target_attempt"
            );

            // Release target worker into execution (will trigger fail_pre_record_writing on target_attempt)
            release_tx.send(true).expect("release target worker");
            target_attempt
        }
    );
    let send1 = send_res.unwrap();
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
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
    }

    // Assert that target_attempt execution consumed and cleared the armed fault
    assert_eq!(
        rig.daemon.fail_pre_record_writing_target_for_test(),
        None,
        "fault must be cleared after target_attempt executes"
    );

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
    assert_eq!(
        first_attempt_id,
        target_attempt.to_string(),
        "re-queued attempt must preserve exact target_attempt identity"
    );

    // Observe hold status for the target pane
    let (hold_state, _) = rig
        .daemon
        .composer_hold_for_test(0, &p0)
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
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        rig.ev
            .wake_on(deadline2.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
    }

    // Verify journal transitions for first_id: exactly one writing, one submitted
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
    let second_attempt = NotificationAttemptId::parse(&second_attempt_id)
        .expect("parse second attempt id");
    let second_locator = cyclops_proto::notification_attempt_claim_locator(second_attempt);

    // Capture full pane history to assert exact doorbell markers and executions
    let history = String::from_utf8_lossy(
        &rig.tmux
            .run(&["capture-pane", "-p", "-S", "-", "-t", &p0])
            .stdout,
    )
    .to_string();

    let first_attempt = NotificationAttemptId::parse(&first_attempt_id)
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
    })
    .await
    .expect("test completed within 15s");
}
