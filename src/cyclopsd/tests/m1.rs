//! M1 end-to-end: the delivery pipeline against a real tmux server on an
//! isolated `-L` socket, the daemon booted in-process, NDJSON clients on
//! the Unix socket. The rig lives in tests/common.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn screen_tier_delivery_end_to_end() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("cat", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (result, elapsed) = rig
        .send(json!({
            "to": ["worker"],
            "subject": "Review the rate limiter",
            "body": "line one\nline two\nline three",
        }))
        .await;
    let msg_id = result["msg_id"].as_str().expect("msg id").to_string();
    assert!(msg_id.starts_with("m-"), "{msg_id}");

    // Idle path: the receipt blocks until resolution, under the 2.5s cap,
    // and reports the screen tier honestly.
    assert!(
        elapsed < Duration::from_millis(2500),
        "receipt took {elapsed:?}"
    );
    let d = &result["deliveries"][0];
    assert_eq!(d["to"], "worker");
    assert_eq!(d["state"], "delivered_unverified", "{result}");

    // The payload is in the pane: daemon-built envelope and body.
    let screen = rig.tmux.capture(&pane);
    assert!(
        screen.contains(&format!(
            "[cyclops {msg_id}] FROM: admin  SUBJECT: Review the rate limiter"
        )),
        "payload not on screen:\n{screen}"
    );
    assert!(screen.contains("line two"), "{screen}");
    // And no reply line, because this one is from admin. That name is
    // reserved, so `cyclops send admin` answers no_such_target and an
    // agent obeying the hint would file a failed delivery and raise
    // attention for it. `in_process_send_with_custom_sender` holds the
    // agent-to-agent case, where the hint still names a real target.
    assert!(
        !screen.contains("Reply: cyclops send admin"),
        "pasted a reply command that cannot run:\n{screen}"
    );

    // Events arrived as they happened: msg, then the delivery-state walk.
    rig.ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "msg" && e["data"]["id"] == msg_id.as_str()
        })
        .await;
    for expect in [
        "gating",
        "pasting",
        "staged",
        "submitted",
        "delivered_unverified",
    ] {
        rig.ev
            .wait_event(Duration::from_secs(5), |e| {
                e["event"] == "delivery-state"
                    && e["data"]["id"] == msg_id.as_str()
                    && e["data"]["to_state"] == expect
            })
            .await;
    }
    let gate = rig
        .ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "gate" && e["data"]["id"] == msg_id.as_str()
        })
        .await;
    assert_eq!(gate["data"]["action"], "proceed");

    // Ledger: one msg line carrying subject and body, legal transitions,
    // verified_by=screen on the final record.
    let lines = rig.ledger_lines();
    let msg_lines: Vec<&Value> = lines
        .iter()
        .filter(|l| l["kind"] == "msg" && l["id"] == msg_id.as_str())
        .collect();
    assert_eq!(msg_lines.len(), 1);
    assert_eq!(msg_lines[0]["from"], "admin");
    assert_eq!(msg_lines[0]["subject"], "Review the rate limiter");
    assert_eq!(msg_lines[0]["body"], "line one\nline two\nline three");
    let final_line = lines
        .iter()
        .rev()
        .find(|l| l["kind"] == "state" && l["id"] == msg_id.as_str())
        .expect("final state line");
    assert_eq!(final_line["deliveries"][0]["verified_by"], "screen");
    assert_eq!(
        rig.final_state(&msg_id, "worker").as_deref(),
        Some("delivered_unverified")
    );
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tier1_hook_ack_and_late_upgrade() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Short receipt block so the test can post hook reports while the
    // delivery waits in its ACK window.
    let mut rig = Rig::new(
        "hook",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 300\nack_timeout_ms = 800\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    // Message 1: hook ACK inside the window -> delivered_verified.
    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "one", "body": "a\nb\nc"}))
        .await;
    let m1 = result["msg_id"].as_str().unwrap().to_string();
    rig.ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == m1.as_str()
                && e["data"]["to_state"] == "submitted"
        })
        .await;
    // Hook reports post through the trusted in-process path: the socket
    // path is pinned to the pane's process ancestry, and this test process
    // lives outside the pane (see m2_hooks for the denial test).
    let resp = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "hooky",
                "event": "UserPromptSubmit",
                "seq": 1,
                "payload": {"prompt": format!("staged text {m1} etc"), "session_id": "s1", "turn_id": "t1"},
            }))
            .unwrap(),
        )
        .await
        .expect("report ok");
    assert_eq!(resp["applied"], true, "{resp}");
    let done = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == m1.as_str()
                && e["data"]["to_state"] == "delivered_verified"
        })
        .await;
    assert_eq!(done["data"]["verified_by"], "hook");

    // Exact duplicate (same session, turn, event): deduped (amendment d).
    let dup = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "hooky",
                "event": "UserPromptSubmit",
                "seq": 2,
                "payload": {"prompt": format!("staged text {m1} etc"), "session_id": "s1", "turn_id": "t1"},
            }))
            .unwrap(),
        )
        .await
        .expect("report ok");
    assert_eq!(dup["duplicate"], true, "{dup}");

    // The turn that UserPromptSubmit opened has to be closed before the
    // next message, and closing it is the agent's job to report. Until a
    // turn-end edge arrives the hook still reads working, and a working
    // hook against an idle screen is a contradiction: rule 12 holds the
    // write rather than picking the reading it likes. Waiting for the two
    // sensors to agree is what makes the next send deterministic instead
    // of a race against a hook reading ageing out.
    rig.daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "hooky",
                "event": "Stop",
                "seq": 3,
                "payload": {"session_id": "s1", "turn_id": "t1"},
            }))
            .unwrap(),
        )
        .await
        .expect("report ok");
    wait_pane_state(&mut rig, "idle").await;

    // Message 2: no hook inside the window -> screen tier resolves it,
    // then a late matching ACK upgrades it (receipts stay honest).
    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "two", "body": "a\nb\nc"}))
        .await;
    let m2 = result["msg_id"].as_str().unwrap().to_string();
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == m2.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;
    let resp = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "hooky",
                "event": "user_prompt_submit",
                "seq": 4,
                "payload": {"prompt": format!("late but matching {m2}"), "session_id": "s1", "turn_id": "t2"},
            }))
            .unwrap(),
        )
        .await
        .expect("report ok");
    assert_eq!(resp["matched"], true, "{resp}");
    let upgraded = rig
        .ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == m2.as_str()
                && e["data"]["to_state"] == "delivered_verified"
        })
        .await;
    assert_eq!(upgraded["data"]["from"], "delivered_unverified");
    assert_eq!(upgraded["data"]["verified_by"], "hook");

    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn modal_declined_with_manifest_keys() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "modal",
        MODAL_MANIFEST,
        &hold_script("FAKE-UPDATE-AVAILABLE"),
        "",
    )
    .await;
    rig.tmux.wait_screen("main", "FAKE-UPDATE-AVAILABLE");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "moody").await;

    let (result, _) = rig
        .send(json!({"to": ["moody"], "subject": "hello", "body": "x\ny\nz"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();

    // The gate declined via the manifest rule's keys, then delivered.
    let decline = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "decline"
        })
        .await;
    assert_eq!(decline["data"]["rule"], "fake_update_modal");
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;

    // The decline keys landed in the pane (the script consumed them), and
    // the raw modal text never entered the ledger.
    rig.assert_ledger_legal(&["FAKE-UPDATE-AVAILABLE"]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn modal_without_auto_dismiss_holds_and_notifies() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "trust",
        MODAL_MANIFEST,
        &hold_script("FAKE-TRUST-PROMPT"),
        "receipt_block_ms = 600\n",
    )
    .await;
    rig.tmux.wait_screen("main", "FAKE-TRUST-PROMPT");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "trusty").await;

    let (result, _) = rig
        .send(json!({"to": ["trusty"], "subject": "held", "body": "x\ny\nz"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    // Unresolved inside the receipt window: reported queued, never a lie.
    assert_eq!(result["deliveries"][0]["state"], "queued", "{result}");
    assert_eq!(result["deliveries"][0]["held_by"], "blocked", "{result}");
    assert!(
        !result["deliveries"][0]["held_by"]
            .as_str()
            .unwrap_or_default()
            .contains("fake_trust_modal"),
        "manifest rule ids must not cross the receipt boundary: {result}"
    );

    // The hold is announced to the admin, naming the rule, and the
    // delivery stays in gating (no decline keys are ever sent).
    let notify = rig
        .ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "admin-notify" && e["data"]["level"] == "action_required"
        })
        .await;
    assert!(
        notify["data"]["subject"]
            .as_str()
            .unwrap()
            .contains("fake_trust_modal"),
        "{notify}"
    );
    // A ping points at something; this one points at the pane, because
    // the prompt on the pane is what a human clears (the delivery is only
    // gating). A reader's calm view holds the ping to that item.
    assert_eq!(notify["data"]["pane_id"], pane.as_str(), "{notify}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let lines = rig.ledger_lines();
    let last = lines
        .iter()
        .rev()
        .find(|l| l["kind"] == "state" && l["id"] == msg_id.as_str())
        .expect("state line");
    assert_eq!(last["data"]["to_state"], "gating", "held, not dismissed");
    // The pane still shows the modal: nothing was typed into it.
    assert!(rig.tmux.capture(&pane).contains("FAKE-TRUST-PROMPT"));

    // The human clears the modal; the held delivery proceeds on the state
    // change, no timer involved.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;
    rig.assert_ledger_legal(&["FAKE-TRUST-PROMPT"]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_parks_everything_and_never_retries() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let script = "sh -c 'echo \"Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 135h57m42s.\"; read x; printf \"\\033[2J\\033[H\"; exec cat'";
    let mut rig = Rig::new("quota", QUOTA_MANIFEST, script, "receipt_block_ms = 2000\n").await;
    rig.tmux.wait_screen("main", "Individual quota reached");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "agy").await;

    // First send: the gate discovers the quota wall and parks, with the
    // parsed reset hint in the receipt note.
    let (r1, _) = rig
        .send(json!({"to": ["agy"], "subject": "first", "body": "b"}))
        .await;
    let m1 = r1["msg_id"].as_str().unwrap().to_string();
    assert_eq!(r1["deliveries"][0]["state"], "parked_blocked_quota", "{r1}");
    assert_eq!(r1["deliveries"][0]["note"], "resets in 135h57m42s", "{r1}");
    let notify = rig
        .ev
        .wait_event(Duration::from_secs(5), |e| e["event"] == "admin-notify")
        .await;
    assert_eq!(notify["data"]["level"], "urgent", "{notify}");
    assert!(notify["data"]["body"]
        .as_str()
        .unwrap()
        .contains("resets in 135h57m42s"));

    // Second send: the parked recipient answers immediately, still parked.
    let (r2, elapsed) = rig
        .send(json!({"to": ["agy"], "subject": "second", "body": "b"}))
        .await;
    let m2 = r2["msg_id"].as_str().unwrap().to_string();
    assert_eq!(r2["deliveries"][0]["state"], "parked_blocked_quota", "{r2}");
    assert!(
        elapsed < Duration::from_millis(500),
        "parked receipt blocked {elapsed:?}"
    );

    // Even when the pane recovers, NOTHING auto-retries: re-queue is an
    // explicit operator action (amendment f).
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        rig.final_state(&m1, "agy").as_deref(),
        Some("parked_blocked_quota")
    );
    assert_eq!(
        rig.final_state(&m2, "agy").as_deref(),
        Some("parked_blocked_quota")
    );
    let screen = rig.tmux.capture(&pane);
    assert!(
        !screen.contains("[cyclops"),
        "a parked delivery was pasted:\n{screen}"
    );

    // The raw quota banner text stays out of the ledger; only the parsed
    // hint travels.
    rig.assert_ledger_legal(&["upgrade your subscription"]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fifo_ordering_after_busy_target_goes_idle() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // The receipt window is deliberately far larger than the thing being
    // measured. This test's timing assertion separates two outcomes: a
    // busy target answering AT ONCE with a queue position, and a
    // regression that holds the receipt open for the whole window. At
    // `receipt_block_ms = 400` against a 300ms threshold those two are
    // 100ms apart, which a loaded runner crosses without anything being
    // wrong: tmux-head measured 330ms on a genuinely immediate answer and
    // failed, while the same commit passed on a quieter runner minutes
    // earlier. A window ten times the threshold cannot be crossed by load.
    let mut rig = Rig::new(
        "fifo",
        BUSY_MANIFEST,
        &hold_script("BUSY-MARKER"),
        "receipt_block_ms = 4000\n",
    )
    .await;
    rig.tmux.wait_screen("main", "BUSY-MARKER");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "busy").await;

    let mut ids = Vec::new();
    let mut positions = Vec::new();
    for n in 0..3 {
        let (r, elapsed) = rig
            .send(json!({"to": ["busy"], "subject": format!("msg {n}"), "body": "a\nb\nc"}))
            .await;
        ids.push(r["msg_id"].as_str().unwrap().to_string());
        assert_eq!(r["deliveries"][0]["state"], "queued", "{r}");
        positions.push(r["deliveries"][0]["position"].as_u64());
        if n == 0 {
            assert_eq!(r["deliveries"][0]["held_by"], "working", "{r}");
        } else {
            assert!(r["deliveries"][0]["held_by"].is_null(), "{r}");
        }
        if n > 0 {
            // Busy path: immediate queued receipt with the queue depth.
            // Well under the 4000ms window above, so a pass means the
            // receipt did not wait for it and a failure means it did.
            assert!(
                elapsed < Duration::from_millis(1500),
                "busy receipt blocked {elapsed:?}, so it waited for the receipt window \
                 instead of answering at once with a queue position"
            );
        }
    }
    assert_eq!(positions, vec![Some(0), Some(1), Some(2)]);

    // Release the pane; queued deliveries land FIFO on the state change.
    let released_at = Instant::now();
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    let mut delivered_order = Vec::new();
    for _ in 0..3 {
        let e = rig
            .ev
            .wait_event(Duration::from_secs(15), |e| {
                e["event"] == "delivery-state" && e["data"]["to_state"] == "delivered_unverified"
            })
            .await;
        delivered_order.push(e["data"]["id"].as_str().unwrap().to_string());
    }
    assert_eq!(delivered_order, ids, "deliveries arrived out of order");
    assert!(
        released_at.elapsed() < Duration::from_secs(10),
        "queued deliveries took {:?} after release",
        released_at.elapsed()
    );
    rig.assert_ledger_legal(&["BUSY-MARKER"]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn broadcast_fans_out_and_missing_recipient_needs_attention() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Generous receipt cap: this test asserts fan-out semantics, not the
    // 2.5s budget. Under full-workspace parallel load the second screen-tier
    // delivery can outlast the default cap, and a queued receipt is legal.
    let mut rig = Rig::new(
        "cast",
        CAT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 10000\n",
    )
    .await;
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", "main:0", &composer_pane()]);
    rig.wait_attached(2).await;
    let panes = rig.pane_ids().await;
    rig.label(&panes[0], "impl").await;
    rig.label(&panes[1], "rev").await;

    // Broadcast: one msg ledger fact, N delivery records advancing
    // independently.
    let (result, _) = rig
        .send(json!({"to": ["impl", "rev"], "subject": "cast", "body": "a\nb\nc"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    let deliveries = result["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 2);
    for d in deliveries {
        assert_eq!(d["state"], "delivered_unverified", "{result}");
    }
    let lines = rig.ledger_lines();
    let msg_lines: Vec<&Value> = lines
        .iter()
        .filter(|l| l["kind"] == "msg" && l["id"] == msg_id.as_str())
        .collect();
    assert_eq!(msg_lines.len(), 1, "broadcast is ONE msg line");
    assert_eq!(msg_lines[0]["deliveries"].as_array().unwrap().len(), 2);
    assert!(rig.tmux.capture(&panes[0]).contains(&msg_id));
    assert!(rig.tmux.capture(&panes[1]).contains(&msg_id));

    // Unresolvable recipient: attention_required with a cause, plus an
    // admin notification. Limbo is a bug; this is a named state.
    let (result, _) = rig
        .send(json!({"to": ["ghost"], "subject": "lost", "body": "b"}))
        .await;
    assert_eq!(
        result["deliveries"][0]["state"], "attention_required",
        "{result}"
    );
    rig.ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "admin-notify" && e["data"]["subject"].as_str().unwrap().contains("ghost")
        })
        .await;

    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn verification_failure_after_a_successful_paste_never_repastes() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "retry",
        BAD_VERIFY_MANIFEST,
        // Deliberately non-verifying: echo stays off so the paste never
        // shows, which is the point. It still needs a clean screen to be
        // admitted at all, so the prompt is painted first.
        "sh -c 'printf \"CYCFIX>\\n\"; stty -echo; exec cat'",
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "flaky").await;

    let (result, _) = rig
        .send(json!({"to": ["flaky"], "subject": "will fail", "body": "b"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    assert_eq!(
        result["deliveries"][0]["state"], "attention_required",
        "{result}"
    );

    // Verification follows a successful paste. Even with one configured
    // retry, the write's terminal outcome is ambiguous and must not repeat.
    let lines = rig.ledger_lines();
    let pastes = lines
        .iter()
        .filter(|l| {
            l["kind"] == "state" && l["id"] == msg_id.as_str() && l["data"]["to_state"] == "pasting"
        })
        .count();
    assert_eq!(pastes, 1, "an ambiguous write must never be pasted twice");
    let screen = rig.tmux.capture(&pane);
    assert_eq!(
        screen
            .matches(&format!("[cyclops {msg_id}] FROM: admin"))
            .count(),
        1,
        "screen after one paste:\n{screen}"
    );
    let final_line = lines
        .iter()
        .rev()
        .find(|l| l["kind"] == "state" && l["id"] == msg_id.as_str())
        .expect("final state line");
    assert_eq!(final_line["data"]["cause"], "verify_failed");
    assert_eq!(final_line["deliveries"][0]["attempts"], 1);
    assert_eq!(
        rig.final_state(&msg_id, "flaky").as_deref(),
        Some("attention_required")
    );
    rig.ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "admin-notify" && e["data"]["level"] == "action_required"
        })
        .await;
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// The in-process embedding path: a caller with an already-resolved sender
/// (the socket path resolves identity first, tested above via "admin").
#[tokio::test(flavor = "multi_thread")]
async fn in_process_send_with_custom_sender() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("embed", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "peer").await;

    let params: cyclops_proto::MsgSendParams = serde_json::from_value(json!({
        "to": ["peer"],
        "subject": "from a labeled agent",
        "body": "a\nb\nc",
    }))
    .unwrap();
    let result = rig.daemon.msg_send("codex", params).await.expect("send ok");
    assert_eq!(
        result["deliveries"][0]["state"], "delivered_unverified",
        "{result}"
    );
    let screen = rig.tmux.capture(&pane);
    assert!(screen.contains("FROM: codex"), "{screen}");
    assert!(screen.contains("Reply: cyclops send codex"), "{screen}");
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}
