mod common;

use std::time::{Duration, Instant};

use common::{tmux_available, Rig, CAT_MANIFEST};
use serde_json::json;

fn option(rig: &Rig, scope: &str, target: &str, option: &str) -> String {
    let out = rig.tmux.run(&["show-options", scope, "-t", target, option]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn border_text(rig: &Rig, pane: &str) -> String {
    let out = rig.tmux.run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{E:pane-border-format}",
    ]);
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

async fn wait_for_option(rig: &Rig, pane: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let opt = option(rig, "-p", pane, "@cyclops_unread");
        let val = if opt.is_empty() {
            ""
        } else {
            opt.split_whitespace().nth(1).unwrap_or("")
        };
        if val == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let actual = option(rig, "-p", pane, "@cyclops_unread");
    panic!("timed out waiting for @cyclops_unread to become {expected:?}; got {actual:?}");
}

async fn wait_for_border_needle(rig: &Rig, pane: &str, needle: &str, should_contain: bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let text = border_text(rig, pane);
        if text.contains(needle) == should_contain {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let text = border_text(rig, pane);
    panic!(
        "timed out waiting for border text (should_contain: {should_contain}) for {needle:?}; got {text:?}"
    );
}

async fn wait_for_pending_unread_projection_count(rig: &Rig, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if rig.daemon.pending_unread_projection_count_for_test() == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "timed out waiting for {expected} pending unread recipient(s); got {}",
        rig.daemon.pending_unread_projection_count_for_test()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_acceptance_never_waits_on_tmux_unread_chrome() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let mut rig = Rig::new(
        "s1unread-acceptance",
        CAT_MANIFEST,
        "cat",
        "receipt_block_ms = 100\n",
    )
    .await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let response = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(response["result"]["label"], "worker", "{response}");

    let unread_chrome = rig.daemon.hold_unread_projection_for_test().await;
    let send = tokio::time::timeout(
        Duration::from_secs(1),
        rig.daemon.msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Mailbox truth",
                "body": "Chrome may be wedged",
                "client_key": "s1-unread-does-not-block"
            }))
            .unwrap(),
        ),
    )
    .await
    .expect("durable acceptance waited on tmux unread chrome")
    .unwrap();
    let message_id = send["msg_id"].as_str().unwrap();
    rig.daemon
        .claim_message_for_test("worker", message_id)
        .expect("the accepted message is immediately claimable");
    drop(unread_chrome);
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_projection_coalesces_a_burst_without_dropping_the_newest_count() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let mut rig = Rig::new(
        "s1unread-coalesce",
        CAT_MANIFEST,
        "cat",
        "receipt_block_ms = 100\n",
    )
    .await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let response = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(response["result"]["label"], "worker", "{response}");

    let first_write = rig.daemon.pause_next_unread_projection_for_test();
    for index in 0..3 {
        rig.daemon
            .msg_send(
                "admin",
                serde_json::from_value(json!({
                    "to": ["worker"],
                    "subject": format!("Burst {index}"),
                    "body": "The durable count must win",
                    "client_key": format!("s1-unread-coalesce-{index}")
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        if index == 0 {
            tokio::time::timeout(Duration::from_secs(2), first_write.wait_until_derived())
                .await
                .expect("the first unread pass never reached its derived-count boundary");
        }
    }
    wait_for_pending_unread_projection_count(&rig, 1).await;

    // Let the stale count of one reach tmux, then stop the second pass after
    // it derives three. This pins the interleaving that a try-lock/drop design
    // loses: the newest fact arrived after derivation but before the old write.
    let second_write = rig.daemon.pause_next_unread_projection_for_test();
    first_write.release();
    tokio::time::timeout(Duration::from_secs(2), second_write.wait_until_derived())
        .await
        .expect("the dirty recipient was dropped after the stale tmux write");
    assert_eq!(
        option(&rig, "-p", &pane, "@cyclops_unread")
            .split_whitespace()
            .nth(1),
        Some("1"),
        "the first tmux write must carry the stale pre-burst count"
    );
    second_write.release();

    wait_for_option(&rig, &pane, "3").await;
    wait_for_pending_unread_projection_count(&rig, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unadopt_is_the_last_chrome_writer_after_an_in_flight_unread_projection() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let mut rig = Rig::new(
        "s1unread-unadopt",
        CAT_MANIFEST,
        "cat",
        "receipt_block_ms = 100\n",
    )
    .await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let response = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(response["result"]["label"], "worker", "{response}");

    let unread_chrome = rig.daemon.hold_unread_projection_for_test().await;
    rig.daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Clear wins",
                "body": "No late badge may repaint the pane",
                "client_key": "s1-unread-unadopt"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    wait_for_pending_unread_projection_count(&rig, 0).await;
    tokio::task::yield_now().await;

    let response = {
        let clear = rig
            .ctl
            .request("pane.label", json!({"target": "worker", "label": null}));
        tokio::pin!(clear);
        tokio::select! {
            response = &mut clear => panic!("unadopt bypassed the unread projection gate: {response}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        drop(unread_chrome);
        clear.await
    };
    assert_eq!(response["result"]["label"], serde_json::Value::Null);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        option(&rig, "-p", &pane, "@cyclops_unread").is_empty(),
        "an unread projection repainted the pane after unadopt"
    );
    wait_for_border_needle(&rig, &pane, "✉", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_joins_the_unread_worker_before_restoring_user_chrome() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let mut rig = Rig::new(
        "s1unread-shutdown",
        CAT_MANIFEST,
        "cat",
        "receipt_block_ms = 100\n",
    )
    .await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let response = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(response["result"]["label"], "worker", "{response}");

    let unread_pause = rig.daemon.pause_next_unread_projection_for_test();
    rig.daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Shutdown order",
                "body": "The user's chrome must be last",
                "client_key": "s1-unread-shutdown"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    unread_pause.wait_until_derived().await;

    let shutdown = rig.daemon.shutdown();
    tokio::pin!(shutdown);
    tokio::select! {
        () = &mut shutdown => panic!("shutdown bypassed the in-flight unread projection"),
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    unread_pause.release();
    shutdown.await;

    assert!(
        option(&rig, "-p", &pane, "@cyclops_unread").is_empty(),
        "the unread worker repainted Cyclops chrome after shutdown restore"
    );
    assert!(!border_text(&rig, &pane).contains("✉"));
}

#[tokio::test(flavor = "multi_thread")]
async fn slice1_unread_lifecycle_and_reconstruction() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let mut rig = Rig::new("s1unread", CAT_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();

    // Adopt pane as "worker".
    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(resp["result"]["label"], "worker", "{resp}");

    // Initial state: no unread option set, no unread icon on border.
    let initial_opt = option(&rig, "-p", &pane, "@cyclops_unread");
    assert!(initial_opt.is_empty(), "initial unread should be unset");
    assert!(!border_text(&rig, &pane).contains("✉"));

    // 1. Accept increments unread and sets @cyclops_unread on the pane border.
    let send1 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "First",
                "body": "First body",
                "client_key": "s1-first"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let msg1_id = send1["msg_id"].as_str().unwrap().to_string();

    wait_for_option(&rig, &pane, "1").await;
    wait_for_border_needle(&rig, &pane, "✉ 1", true).await;

    // Send second message: unread becomes 2.
    let send2 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Second",
                "body": "Second body",
                "client_key": "s1-second"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let msg2_id = send2["msg_id"].as_str().unwrap().to_string();

    wait_for_option(&rig, &pane, "2").await;
    wait_for_border_needle(&rig, &pane, "✉ 2", true).await;

    // 2. Claim decrements and eventually clears unread projection.
    rig.daemon
        .claim_message_for_test("worker", &msg1_id)
        .unwrap();
    wait_for_option(&rig, &pane, "1").await;
    wait_for_border_needle(&rig, &pane, "✉ 1", true).await;

    rig.daemon
        .claim_message_for_test("worker", &msg2_id)
        .unwrap();
    wait_for_option(&rig, &pane, "").await;
    wait_for_border_needle(&rig, &pane, "✉", false).await;

    // 3. Send third message, then test daemon reboot reconstruction.
    let send3 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Third",
                "body": "Third body",
                "client_key": "s1-third"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let _msg3_id = send3["msg_id"].as_str().unwrap().to_string();

    wait_for_option(&rig, &pane, "1").await;
    wait_for_border_needle(&rig, &pane, "✉ 1", true).await;

    // Reboot daemon: reconstruction must restore @cyclops_unread from durable journal.
    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;

    wait_for_option(&rig, &pane, "1").await;
    wait_for_border_needle(&rig, &pane, "✉ 1", true).await;

    // 4. Status and messages.snapshot expose pending unread even when query has no live pane.
    let status = rig.ctl.request("status", json!({})).await;
    let worker_route = status["result"]["mailbox_routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "worker")
        .expect("worker route must exist in status");
    assert_eq!(worker_route["unread"], 1);

    let snapshot = rig
        .ctl
        .request("messages.snapshot", json!({"recent_settled": 0}))
        .await;
    assert!(
        snapshot["result"]["counts"]["pending_entries"]
            .as_u64()
            .unwrap()
            >= 1,
        "pending entries in snapshot must be >= 1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn slice1_supersession_and_unroutable_unread() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let mut rig = Rig::new("s1super", CAT_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();

    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(resp["result"]["label"], "worker", "{resp}");

    // Send a message to worker with client_key.
    let send1 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Initial",
                "body": "Initial body",
                "client_key": "s1-key-1"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let msg1_id = send1["msg_id"].as_str().unwrap().to_string();
    wait_for_option(&rig, &pane, "1").await;

    // Send superseding message to worker referencing msg1_id.
    let send2 = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Superseding",
                "body": "Superseding body",
                "supersedes": msg1_id,
                "client_key": "s1-key-2"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let _msg2_id = send2["msg_id"].as_str().unwrap().to_string();

    // The superseded message is no longer pending, and the new message is pending, so unread remains 1 (not 2).
    wait_for_option(&rig, &pane, "1").await;

    // Now unadopt the pane (remove label) so the pane has no live route/adoption.
    let unadopt = rig
        .ctl
        .request("pane.label", json!({"target": "worker", "label": null}))
        .await;
    assert_eq!(
        unadopt["result"]["label"],
        serde_json::Value::Null,
        "{unadopt}"
    );

    // Unadopting unsets the pane option.
    let opt_after_unadopt = option(&rig, "-p", &pane, "@cyclops_unread");
    assert!(
        opt_after_unadopt.is_empty(),
        "option must be cleared on unadopt"
    );

    // But status and messages.snapshot STILL expose the unread count for worker!
    let status = rig.ctl.request("status", json!({})).await;
    let worker_route = status["result"]["mailbox_routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "worker")
        .expect("worker route must remain in status mailbox_routes");
    assert_eq!(worker_route["unread"], 1);

    let snapshot = rig
        .ctl
        .request("messages.snapshot", json!({"recent_settled": 0}))
        .await;
    assert_eq!(
        snapshot["result"]["counts"]["pending_entries"]
            .as_u64()
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn slice1_withdrawal_leaves_message_pending_and_unread() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let mut rig = Rig::new("s1withd", CAT_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();

    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(resp["result"]["label"], "worker", "{resp}");
    // Copy-mode holds the wake in the gate, which is the one place a
    // withdrawal can still take it.
    rig.tmux.run_ok(&["copy-mode", "-t", &pane]);
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "readiness"
                && e["data"]["pane_id"] == pane.as_str()
                && e["data"]["write_block"] == "pane_in_mode"
        })
        .await;

    let send = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Withdrawal Test",
                "body": "Withdrawal body",
                "client_key": "s1-withdraw"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let msg_id = send["msg_id"].as_str().unwrap().to_string();
    wait_for_option(&rig, &pane, "1").await;

    // Read snapshot to get attempt_id and recipient
    let snap = rig
        .ctl
        .request("messages.snapshot", json!({"recent_settled": 0}))
        .await;
    let row = snap["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["message_id"].as_str() == Some(msg_id.as_str()))
        .unwrap();
    let recipient = row["recipients"][0]["recipient"].clone();
    let attempt_id = row["recipients"][0]["notification"]["attempt_id"].clone();

    if !attempt_id.is_null() {
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
    }

    // Withdrawal of the wake leaves the mailbox message pending: unread remains 1!
    wait_for_option(&rig, &pane, "1").await;
    wait_for_border_needle(&rig, &pane, "✉ 1", true).await;

    let status = rig.ctl.request("status", json!({})).await;
    let worker_route = status["result"]["mailbox_routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "worker")
        .unwrap();
    assert_eq!(worker_route["unread"], 1);
}
