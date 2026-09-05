//! Hook liveness and self-test: hooks.verify, hooks.selftest, the
//! hooks_verified status bit, and the F1 regression shape (a tier-1 pane
//! with zero hook edges downgrades cleanly and notifies, no hang, no
//! loss). Isolated tmux rig from tests/common; hook edges are simulated
//! through the trusted in-process Daemon::report_state path, because the
//! socket path is pinned to the reporting pane's process ancestry and this
//! test process lives outside every pane (that pinning has its own test
//! below).

use crate::common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};

const SELFTEST_SUBJECT: &str = "[cyclops] hook self-test";

/// Pane row for the rig's single pane out of a status result.
fn status_pane(status: &Value) -> Value {
    status["result"]["sessions"][0]["panes"][0].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn selftest_verifies_with_simulated_hook_edge() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "sthook",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 300\nack_timeout_ms = 1500\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    // Adoption/boot: tier-1 pane, zero edges ever seen. Status says so.
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(status_pane(&status)["hooks_verified"], false, "{status}");

    // hooks.verify agrees and names the declared events with no ages.
    let verify = rig
        .ctl
        .request("hooks.verify", json!({"target": "hooky"}))
        .await;
    let v = &verify["result"];
    assert_eq!(v["tier"], 1, "{verify}");
    assert_eq!(v["hooks_verified"], false, "{verify}");
    let events = v["events"].as_array().expect("events array");
    let names: Vec<&str> = events.iter().filter_map(|e| e["event"].as_str()).collect();
    assert_eq!(names, vec!["UserPromptSubmit", "Stop"], "{verify}");
    for e in events {
        assert!(e["last_seen_ms_ago"].is_null(), "never seen yet: {verify}");
    }

    // Self-test with the hook edge simulated at submit time: the exact
    // doorbell round trip resolves delivered_verified and reports hook_ack.
    let daemon = &rig.daemon;
    let params: cyclops_proto::HooksSelftestParams =
        serde_json::from_value(json!({"target": "hooky"})).unwrap();
    // The join below holds the daemon and the rig immutably, so the wakes
    // come from a subscription of their own rather than `rig.ev`.
    let mut probe = TestClient::connect(&daemon.socket_path()).await;
    let ack = probe.request("events.subscribe", json!({})).await;
    assert_eq!(ack["result"]["subscribed"], true);
    let (selftest, msg_id) = tokio::join!(daemon.hooks_selftest(params), async {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let msg_id = loop {
            let found = workspace_lines(&rig).into_iter().find(|line| {
                matches!(line.kind, cyclops_proto::Kind::Fyi)
                    && line.subject.as_deref() == Some(SELFTEST_SUBJECT)
            });
            if let Some(line) = found {
                break line.id;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the self-test message was never accepted"
            );
            // Acceptance is a journal append, which publishes
            // `messages.changed`; wake on it and read the journal again.
            probe
                .wake_on(
                    deadline.saturating_duration_since(std::time::Instant::now()),
                    |e| e["event"] == "messages.changed",
                )
                .await;
        };
        wait_notification_state_on(
            &mut probe,
            &rig.home,
            &msg_id,
            &["submitted", "submitted_unverified"],
        )
        .await;
        // The simulated ACK hook: the trusted in-process report path,
        // carrying the exact row the daemon pasted.
        let resp = daemon
            .report_state(
                serde_json::from_value(json!({
                    "agent": "hooky",
                    "event": "UserPromptSubmit",
                    "seq": 1,
                    "payload": {"prompt": doorbell_for(&rig, &msg_id)},
                }))
                .unwrap(),
            )
            .await
            .expect("report ok");
        assert_eq!(resp["applied"], true, "{resp}");
        msg_id
    });
    let result = selftest.expect("selftest ok");
    assert_eq!(result["hook_ack"], true, "{result}");
    assert_eq!(result["state"], "delivered_verified", "{result}");
    assert_eq!(result["tier"], 1, "{result}");
    assert_eq!(result["msg_id"], msg_id.as_str(), "{result}");
    // The bound manifest id rides along so CLI failure copy can print a
    // runnable `cyclops hooks install <manifest> --agent <target>`.
    assert_eq!(result["manifest"], "fix", "{result}");

    // Liveness flipped: the cached status projection and verify both see the
    // edge now. Use the in-process status projection here because the socket
    // `status` command first performs a separately bounded live tmux refresh.
    // An overloaded runner may honestly refuse that refresh with
    // `status_refresh_incomplete`; that says nothing about whether the hook
    // edge was recorded.
    let status = json!({"result": rig.daemon.status(false)});
    assert_eq!(status_pane(&status)["hooks_verified"], true, "{status}");
    let verify = rig
        .ctl
        .request("hooks.verify", json!({"target": "hooky"}))
        .await;
    let ups = &verify["result"]["events"][0];
    assert_eq!(ups["event"], "UserPromptSubmit", "{verify}");
    assert!(ups["last_seen_ms_ago"].is_u64(), "{verify}");

    // The result is a recorded system fact, and the fyi message is a
    // durable mailbox record with the self-test subject and body.
    let sys = rig
        .ledger_lines()
        .into_iter()
        .find(|l| l["kind"] == "system" && l["data"]["event"] == "hook_selftest")
        .expect("hook_selftest system line");
    assert_eq!(sys["id"], msg_id.as_str());
    assert_eq!(sys["data"]["hook_ack"], true);
    assert_eq!(sys["data"]["state"], "delivered_verified");
    let msg = workspace_lines(&rig)
        .into_iter()
        .find(|l| matches!(l.kind, cyclops_proto::Kind::Fyi) && l.id == msg_id)
        .expect("self-test fyi message");
    assert_eq!(msg.subject.as_deref(), Some(SELFTEST_SUBJECT));
    assert_eq!(msg.body.as_deref(), Some("Reply not needed."));
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn f1_zero_edge_tier1_downgrades_notifies_once_and_loses_nothing() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Short tier-1 window: the F1 shape is a hook config that never fires.
    let mut rig = Rig::new(
        "f1",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 2500\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    // Self-test with NO hook edge: the unverified path. No hang: the
    // request resolves inside the receipt/evidence budget; no loss: the
    // delivery lands as delivered_unverified (screen tier).
    let selftest = rig
        .ctl
        .request("hooks.selftest", json!({"target": "hooky"}))
        .await;
    let r = &selftest["result"];
    assert_eq!(r["hook_ack"], false, "{selftest}");
    assert_eq!(r["state"], "delivered_unverified", "{selftest}");
    assert_eq!(r["tier"], 1, "{selftest}");
    assert_eq!(r["manifest"], "fix", "{selftest}");
    let m1 = r["msg_id"].as_str().expect("msg id").to_string();

    // The tier-1 timeout on a zero-edge pane names the likely F1 cause.
    let notify = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "admin-notify"
                && e["data"]["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("hooks configured but never seen"))
        })
        .await;
    assert_eq!(notify["data"]["level"], "action_required", "{notify}");
    let body = notify["data"]["body"].as_str().unwrap();
    assert!(body.contains(&m1), "names the delivery: {body}");
    assert!(body.contains("screen"), "names the downgrade: {body}");
    // And names it structurally too. That delivery resolved as
    // delivered_unverified, which the rule says nobody must clear, so
    // this is exactly the ping that used to sit under a calm eye.
    assert_eq!(notify["data"]["to"], "hooky", "{notify}");
    assert_eq!(notify["data"]["id"], m1.as_str(), "{notify}");

    // Still unverified in status; the payload really reached the pane. The
    // screen receipt can resolve while the fixture's short Working frame is
    // still visible. Wait for its clean write-ready frame before exercising
    // F1 notification deduplication, which is independent of queue wakeup.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let pane_status = status_pane(&status);
        if pane_status["write_block"] == "status_refresh_incomplete" {
            assert!(
                std::time::Instant::now() < deadline,
                "first delivery status refresh never completed: {status}"
            );
            // No event: `status_refresh_incomplete` is status refusing to answer
            // inside its own budget, and it blanks `write_ready` with it.
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }
        assert_eq!(pane_status["hooks_verified"], false, "{status}");
        if pane_status["write_ready"] == true {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first delivery never returned to a clean write-ready frame: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        rig.tmux.capture(&pane).contains(&doorbell_for(&rig, &m1)),
        "doorbell not in pane"
    );

    // A second notification downgrades the same way but does NOT
    // re-notify: one F1 ping per pane. The recipient claims the first
    // message so the next doorbell is scheduled.
    claim(&rig, "hooky", &m1);
    let (r2, _) = rig
        .send(json!({"to": ["hooky"], "subject": "after", "body": "b"}))
        .await;
    let m2 = r2["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&mut rig, &m2, &["notified"]).await;
    let f1_notifies = rig
        .ledger_lines()
        .iter()
        .filter(|l| {
            l["kind"] == "system"
                && l["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("hooks configured but never seen"))
        })
        .count();
    assert_eq!(f1_notifies, 1, "F1 notify must fire once per pane");

    assert_eq!(notification_state(&rig, &m1).as_deref(), Some("notified"));
    assert_eq!(notification_state(&rig, &m2).as_deref(), Some("notified"));
    rig.shutdown().await;
}

/// HIGH: agent.state.report over the socket is pinned to the reporting
/// pane. This test process is same-uid but lives outside every pane, so
/// its forged report must be DENIED and ingest nothing: no hook liveness,
/// no tier-1 ACK match. Before the pin, this exact call forged
/// hooks_verified AND upgraded the delivery to "delivered · verified",
/// making the record lie.
#[tokio::test(flavor = "multi_thread")]
async fn forged_report_over_the_socket_is_denied_and_ingests_nothing() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "forge",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 1200\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "forge me", "body": "a\nb"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&mut rig, &msg_id, &["submitted", "submitted_unverified"]).await;

    // The forged tier-1 ACK, row and all, from outside the pane.
    let resp = rig
        .ctl
        .request(
            "agent.state.report",
            json!({
                "agent": "hooky",
                "event": "UserPromptSubmit",
                "seq": 1,
                "payload": {"prompt": format!("staged text {msg_id} etc")},
            }),
        )
        .await;
    assert_eq!(resp["error"]["code"], "denied", "{resp}");
    // Naming the pane by id instead of label changes nothing.
    let resp = rig
        .ctl
        .request(
            "agent.state.report",
            json!({
                "agent": pane.as_str(),
                "event": "UserPromptSubmit",
                "seq": 2,
                "payload": {"prompt": format!("staged text {msg_id} etc")},
            }),
        )
        .await;
    assert_eq!(resp["error"]["code"], "denied", "{resp}");

    // Nothing was ingested: the notification resolves on the SCREEN tier
    // when the real hook window times out.
    wait_notification_state(&mut rig, &msg_id, &["notified"]).await;
    // No liveness recorded: the pane still reads hooks unverified.
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(status_pane(&status)["hooks_verified"], false, "{status}");
    rig.shutdown().await;
}

/// MEDIUM (F1 stale liveness): hook edges belong to the occupant that
/// produced them. After an occupant swap (respawn without hooks),
/// hooks_verified must revert, and the next tier-1 delivery must fire the
/// F1 downgrade notification for the NEW occupant instead of hiding behind
/// the old one's edges.
#[tokio::test(flavor = "multi_thread")]
async fn occupant_swap_preserves_the_name_and_renews_the_f1_ping() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "occf1",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 2500\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    // The first occupant proves its hooks: one edge, hooks_verified true.
    // (PostToolUse is not a turn-boundary event, so it feeds liveness
    // without moving the fused state.)
    let resp = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "hooky",
                "event": "PostToolUse",
                "seq": 1,
                "payload": {},
            }))
            .unwrap(),
        )
        .await
        .expect("report ok");
    assert_eq!(resp["applied"], true, "{resp}");
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(status_pane(&status)["hooks_verified"], true, "{status}");

    // Occupant swap: same logical pane and name, new process. The durable
    // route moves to the replacement, but hook proof stays with the process
    // that emitted it and must disappear.
    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &composer_pane()]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let pane_status = status_pane(&status);
        if pane_status["agent"] == "hooky" && pane_status["hooks_verified"] == false {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the replacement did not keep the name with fresh hook state: {status}"
        );
        // No event: the respawned occupant's binding and hook state reach
        // status without an announcement.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Tier-1 delivery to the hookless new occupant: screen downgrade plus
    // the F1 notification, which the old occupant's edges used to suppress.
    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "after swap", "body": "b"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&mut rig, &msg_id, &["notified"]).await;
    let notify = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "admin-notify"
                && e["data"]["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("hooks configured but never seen"))
        })
        .await;
    assert!(
        notify["data"]["body"].as_str().unwrap().contains(&msg_id),
        "{notify}"
    );
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn screen_tier_pane_reports_tier2_and_no_verified_bit() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // CAT_MANIFEST declares no hooks at all: hooks_verified stays absent
    // (configuration cannot be unverified when nothing is configured) and
    // verify reports the screen tier with no events.
    let mut rig = Rig::new("t2", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "screeny").await;

    let status = rig.ctl.request("status", json!({})).await;
    assert!(status_pane(&status)["hooks_verified"].is_null(), "{status}");
    let verify = rig
        .ctl
        .request("hooks.verify", json!({"target": "screeny"}))
        .await;
    let v = &verify["result"];
    assert_eq!(v["tier"], 2, "{verify}");
    assert!(v["hooks_verified"].is_null(), "{verify}");
    assert_eq!(v["events"], json!([]), "{verify}");

    // Unknown targets are a named error, not a hang.
    let missing = rig
        .ctl
        .request("hooks.verify", json!({"target": "ghost"}))
        .await;
    assert_eq!(missing["error"]["code"], "no_such_target", "{missing}");
    rig.shutdown().await;
}
