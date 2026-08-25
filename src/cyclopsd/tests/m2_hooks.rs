//! M2 hook liveness and self-test: hooks.verify, hooks.selftest, the
//! hooks_verified status bit, and the F1 regression shape (a tier-1 pane
//! with zero hook edges downgrades cleanly and notifies, no hang, no
//! loss). Isolated tmux rig from tests/common; hook edges are simulated
//! through the trusted in-process Daemon::report_state path, because the
//! socket path is pinned to the reporting pane's process ancestry and this
//! test process lives outside every pane (that pinning has its own test
//! below).

mod common;

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

    // Self-test with the hook edge simulated at submit time: the marker
    // round trip resolves delivered_verified and reports hook_ack.
    let daemon = &rig.daemon;
    let ev = &mut rig.ev;
    let params: cyclops_proto::HooksSelftestParams =
        serde_json::from_value(json!({"target": "hooky"})).unwrap();
    let (selftest, msg_id) = tokio::join!(daemon.hooks_selftest(params), async {
        let msg = ev
            .wait_event(Duration::from_secs(8), |e| {
                e["event"] == "msg" && e["data"]["subject"] == SELFTEST_SUBJECT
            })
            .await;
        let msg_id = msg["data"]["id"].as_str().expect("msg id").to_string();
        assert_eq!(msg["data"]["fyi"], true, "self-test is flagged fyi");
        assert_eq!(msg["data"]["body"], "Reply not needed.", "{msg}");
        ev.wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "submitted"
        })
        .await;
        // The simulated ACK hook: the trusted in-process report path,
        // payload carrying the marker (the message id).
        let resp = daemon
            .report_state(
                serde_json::from_value(json!({
                    "agent": "hooky",
                    "event": "UserPromptSubmit",
                    "seq": 1,
                    // The exact payload: a hook acknowledgement verifies
                    // the bytes this delivery sent, or nothing. The
                    // self-test sends as `cyclopsd` with fyi set, so
                    // there is no reply hint.
                    "payload": {"prompt": cyclopsd::render_payload(
                        &msg_id,
                        "cyclopsd",
                        SELFTEST_SUBJECT,
                        "Reply not needed.",
                        true,
                    )},
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

    // Liveness flipped: status and verify both see the edge now.
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(status_pane(&status)["hooks_verified"], true, "{status}");
    let verify = rig
        .ctl
        .request("hooks.verify", json!({"target": "hooky"}))
        .await;
    let ups = &verify["result"]["events"][0];
    assert_eq!(ups["event"], "UserPromptSubmit", "{verify}");
    assert!(ups["last_seen_ms_ago"].is_u64(), "{verify}");

    // The result is a recorded system fact, and the fyi msg line carries
    // the self-test subject and body.
    let lines = rig.ledger_lines();
    let sys = lines
        .iter()
        .find(|l| l["kind"] == "system" && l["data"]["event"] == "hook_selftest")
        .expect("hook_selftest system line");
    assert_eq!(sys["id"], msg_id.as_str());
    assert_eq!(sys["data"]["hook_ack"], true);
    assert_eq!(sys["data"]["state"], "delivered_verified");
    let msg = lines
        .iter()
        .find(|l| l["kind"] == "fyi" && l["id"] == msg_id.as_str())
        .expect("self-test fyi msg line");
    assert_eq!(msg["subject"], SELFTEST_SUBJECT);
    assert_eq!(msg["body"], "Reply not needed.");
    rig.assert_ledger_legal(&[]);
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
    assert!(rig.tmux.capture(&pane).contains(&m1), "marker not in pane");

    // A second delivery downgrades the same way but does NOT re-notify:
    // one F1 ping per pane.
    let (r2, _) = rig
        .send(json!({"to": ["hooky"], "subject": "after", "body": "b"}))
        .await;
    let m2 = r2["msg_id"].as_str().unwrap().to_string();
    assert_eq!(
        r2["deliveries"][0]["state"], "delivered_unverified",
        "second delivery left a positively write-ready frame: {r2}"
    );
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

    assert_eq!(
        rig.final_state(&m1, "hooky").as_deref(),
        Some("delivered_unverified")
    );
    assert_eq!(
        rig.final_state(&m2, "hooky").as_deref(),
        Some("delivered_unverified")
    );
    rig.assert_ledger_legal(&[]);
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
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "submitted"
        })
        .await;

    // The forged tier-1 ACK, marker and all, from outside the pane.
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

    // Nothing was ingested: the delivery resolves on the SCREEN tier when
    // the real hook window times out, and no verified transition exists.
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;
    assert_eq!(
        rig.final_state(&msg_id, "hooky").as_deref(),
        Some("delivered_unverified"),
        "the forged ACK must not verify the delivery"
    );
    assert!(
        !rig.ledger_lines().iter().any(|l| {
            l["kind"] == "state"
                && l["id"] == msg_id.as_str()
                && l["data"]["to_state"] == "delivered_verified"
        }),
        "a delivered_verified line reached the ledger from a forged report"
    );
    // No liveness recorded: the pane still reads hooks unverified.
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(status_pane(&status)["hooks_verified"], false, "{status}");
    rig.assert_ledger_legal(&[]);
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Tier-1 delivery to the hookless new occupant: screen downgrade plus
    // the F1 notification, which the old occupant's edges used to suppress.
    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "after swap", "body": "b"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    assert_eq!(
        result["deliveries"][0]["state"], "delivered_unverified",
        "{result}"
    );
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
    rig.assert_ledger_legal(&[]);
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
