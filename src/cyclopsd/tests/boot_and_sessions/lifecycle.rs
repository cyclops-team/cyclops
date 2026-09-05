//! Turn lifecycle against the real report path.
//!
//! Hook reports arrive over a socket from a separate process, so nothing
//! orders them. A vendor that names its turns lets the daemon match an end
//! to the turn it belongs to; what it cannot do is guarantee the end
//! arrives after the start. These tests drive that disorder through the
//! daemon's own ingestion path rather than through the pieces underneath
//! it.

use crate::common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::json;

/// The shared fixture, plus the one thing these tests need: a vendor that
/// NAMES its turns, so an end can be matched to the turn it belongs to.
///
/// Derived rather than copied so the screen rules stay identical to every
/// other test's. Start and acknowledgement remain the same event, which
/// is the shipped shape: taking the prompt is both the receipt and the
/// beginning of the turn.
fn codex_lifecycle_manifest() -> String {
    let shipped = cyclops_manifest::Manifest::parse(
        include_str!("../../../../resources/manifests/codex.toml"),
        std::path::Path::new("resources/manifests/codex.toml"),
    )
    .expect("shipped Codex manifest");
    assert_eq!(
        shipped.hooks.turn_start.as_deref(),
        Some("UserPromptSubmit")
    );
    assert_eq!(shipped.hooks.turn_end.as_deref(), Some("Stop"));
    assert_eq!(shipped.hooks.ack.as_deref(), Some("UserPromptSubmit"));
    assert_eq!(shipped.hooks.ack_payload_field.as_deref(), Some("prompt"));
    assert_eq!(
        shipped.hooks.turn_key_fields,
        ["session_id", "turn_id"],
        "the integration fixture must follow the shipped Codex lifecycle key"
    );

    let hooks = "ack_payload_field = \"prompt\"";
    assert!(HOOK_MANIFEST.contains(hooks), "fixture shape changed");
    HOOK_MANIFEST.replace(
        hooks,
        &format!("{hooks}\nturn_key_fields = [\"session_id\", \"turn_id\"]"),
    )
}

fn claude_candidate_manifest() -> String {
    // Synthetic keyed candidate lifecycle. Claude does not expose this key;
    // these fixtures exercise the generic candidate-end machinery.
    let marker = "ack_payload_field = \"prompt\"";
    assert!(HOOK_MANIFEST.contains(marker), "fixture shape changed");
    HOOK_MANIFEST
        .replace(
            "turn_start_evidence = \"confirmed\"",
            "turn_start_evidence = \"candidate\"",
        )
        .replace(
            "turn_end_evidence = \"confirmed\"",
            "turn_end_evidence = \"candidate\"",
        )
        .replace(
            marker,
            concat!(
                "ack_payload_field = \"prompt\"\n",
                "turn_end_settle_ms = 3000\n",
                "turn_end_confirmed = [\"StopFailure\"]\n",
                "ack_evidence = \"dispatch\"\n",
                "turn_key_fields = [\"session_id\", \"prompt_id\"]"
            ),
        )
}

fn claude_unkeyed_dispatch_manifest() -> String {
    // Mirror the shipped Claude hook contract while retaining the fake
    // agent process and screen rules needed by this integration fixture.
    let lifecycle = concat!(
        "turn_start = \"UserPromptSubmit\"\n",
        "turn_start_evidence = \"confirmed\"\n",
        "turn_end = \"Stop\"\n",
        "turn_end_evidence = \"confirmed\"\n",
        "ack = \"UserPromptSubmit\"\n"
    );
    assert!(HOOK_MANIFEST.contains(lifecycle), "fixture shape changed");
    HOOK_MANIFEST.replace(
        lifecycle,
        concat!(
            "turn_start = \"UserPromptSubmit\"\n",
            "turn_start_evidence = \"candidate\"\n",
            "ack = \"UserPromptSubmit\"\n",
            "ack_evidence = \"dispatch\"\n"
        ),
    )
}

fn send_fixture_key(rig: &Rig, pane: &str, key: &str) {
    rig.tmux.run_ok(&["send-keys", "-t", pane, key]);
}

async fn report(rig: &Rig, event: &str, payload: serde_json::Value) -> serde_json::Value {
    rig.daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "keyed",
                "event": event,
                "payload": payload,
            }))
            .expect("report params"),
        )
        .await
        .expect("report ok")
}

async fn wait_submitted(rig: &mut Rig, id: &str) {
    wait_delivery_state(rig, id, "submitted").await;
}

/// Wait for the durable notification record. `delivered_verified` is the
/// receipt vocabulary these tests keep: on the record it is `notified`,
/// reached from the exact hook acknowledgement each test reports.
async fn wait_delivery_state(rig: &mut Rig, id: &str, state: &str) {
    let states: &[&str] = match state {
        "submitted" => &["submitted", "submitted_unverified", "notified"],
        "delivered_verified" => &["notified"],
        "gating" => &["gating"],
        other => panic!("no durable notification state for {other}"),
    };
    wait_notification_state(rig, id, states).await;
}

async fn acknowledge_codex_turn(rig: &mut Rig, subject: &str, session: &str, turn: &str) -> String {
    let (result, _) = rig
        .send(json!({"to": ["keyed"], "subject": subject, "body": "b"}))
        .await;
    let id = result["msg_id"].as_str().expect("msg id").to_string();
    wait_submitted(rig, &id).await;
    let ack = report(
        rig,
        "UserPromptSubmit",
        json!({
            "session_id": session,
            "turn_id": turn,
            "prompt": doorbell_for(rig, &id),
        }),
    )
    .await;
    assert_eq!(ack["matched"], true, "{ack}");
    wait_notification_state(rig, &id, &["notified"]).await;
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_unkeyed_dispatch_publishes_provisional_working_then_visual_receipt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_unkeyed_dispatch_manifest();
    let mut rig = Rig::new(
        "claude-unkeyed-receipt",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (sent, _) = rig
        .send(json!({"to": ["keyed"], "subject": "unkeyed", "body": "body"}))
        .await;
    let id = sent["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &id).await;
    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-1",
            "prompt": doorbell_for(&rig, &id),
        }),
    )
    .await;

    assert_eq!(start["matched"], true, "{start}");
    assert_eq!(start["state"], "working", "{start}");
    let provisional_state = rig
        .ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "state"
                && event["data"]["pane_id"] == pane.as_str()
                && event["data"]["state"] == "working"
                && event["data"]["working_confirmed"] == false
        })
        .await;
    assert_eq!(
        provisional_state["data"]["working_confirmed"], false,
        "{provisional_state}"
    );
    assert_ne!(
        notification_state(&rig, &id).as_deref(),
        Some("notified"),
        "the hook dispatch became a receipt before visual acceptance"
    );

    // Status forces a clean visual recompute. A clean composer is neutral
    // while Claude's prompt-submit edge remains provisional and output has
    // not appeared yet.
    let status = rig.ctl.request("status", json!({})).await;
    let pane_status = status["result"]["sessions"][0]["panes"]
        .as_array()
        .and_then(|panes| panes.iter().find(|row| row["pane_id"] == pane.as_str()))
        .unwrap_or_else(|| panic!("pane missing after provisional start: {status}"));
    assert_eq!(pane_status["state"], "working", "{status}");
    assert_ne!(
        notification_state(&rig, &id).as_deref(),
        Some("notified"),
        "status promoted the provisional hook dispatch into a receipt"
    );

    let repaint = rig.daemon.pause_next_chrome_repaint_for_test();
    send_fixture_key(&rig, &pane, "C-t");
    tokio::time::timeout(Duration::from_secs(10), repaint.wait_until_entered())
        .await
        .expect("visual acceptance did not reach the post-commit chrome boundary");
    assert_eq!(
        notification_state(&rig, &id).as_deref(),
        Some("notified"),
        "presentation began before the returned dispatch ACK was durable"
    );
    repaint.release();
    rig.ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "state"
                && event["data"]["pane_id"] == pane.as_str()
                && event["data"]["state"] == "working"
                && event["data"]["working_confirmed"] == true
        })
        .await;
    wait_notification_state(&mut rig, &id, &["notified"]).await;

    // Claude has no exact Stop key. Its lifecycle returns to idle from a
    // fresh clean visual frame, without inventing cross-event correlation.
    send_fixture_key(&rig, &pane, "C-y");
    wait_pane_state(&mut rig, "idle").await;
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unconfirmed_dispatch_cannot_be_revived_by_a_later_human_prompt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_unkeyed_dispatch_manifest();
    let mut rig = Rig::new(
        "claude-unconfirmed-dispatch",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (sent, _) = rig
        .send(json!({"to": ["keyed"], "subject": "unconfirmed", "body": "body"}))
        .await;
    let id = sent["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &id).await;
    let dispatch = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "cyclops-session",
            "prompt": doorbell_for(&rig, &id),
        }),
    )
    .await;
    assert_eq!(dispatch["matched"], true, "{dispatch}");
    assert_eq!(dispatch["state"], "working", "{dispatch}");
    rig.ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "state"
                && event["data"]["pane_id"] == pane.as_str()
                && event["data"]["state"] == "working"
                && event["data"]["working_confirmed"] == false
        })
        .await;

    // No visual Working frame follows. A clean frame cannot reject the hook
    // edge based only on age, so runtime remains conservatively Working.
    // A bounded window: nothing publishes the absence of a frame.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let status = rig.ctl.request("status", json!({})).await;
    let pane_status = status["result"]["sessions"][0]["panes"]
        .as_array()
        .and_then(|panes| panes.iter().find(|row| row["pane_id"] == pane.as_str()))
        .unwrap_or_else(|| panic!("pane missing after unconfirmed dispatch: {status}"));
    assert_eq!(pane_status["state"], "working", "{status}");
    assert_eq!(pane_status["working_confirmed"], false, "{status}");
    assert_ne!(
        notification_state(&rig, &id).as_deref(),
        Some("notified"),
        "a stable clean frame verified an unaccepted dispatch"
    );

    let human = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "later-human-session",
            "prompt": "later unrelated human prompt",
        }),
    )
    .await;
    assert_eq!(human["matched"], false, "{human}");
    send_fixture_key(&rig, &pane, "C-t");
    rig.ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "state"
                && event["data"]["pane_id"] == pane.as_str()
                && event["data"]["state"] == "working"
                && event["data"]["working_confirmed"] == true
        })
        .await;
    assert_ne!(
        notification_state(&rig, &id).as_deref(),
        Some("notified"),
        "a later human turn revived the retired Cyclops receipt candidate"
    );

    send_fixture_key(&rig, &pane, "C-y");
    wait_pane_state(&mut rig, "idle").await;
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_dispatch_waits_for_visual_working_before_it_verifies() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new(
        "claude-candidate",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (sent, _) = rig
        .send(json!({"to": ["keyed"], "subject": "candidate", "body": "body"}))
        .await;
    let id = sent["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &id).await;
    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-1",
            "prompt_id": "prompt-1",
            "prompt": doorbell_for(&rig, &id),
        }),
    )
    .await;
    assert_eq!(start["matched"], true, "{start}");
    assert!(
        start["state"].is_null(),
        "candidate claimed runtime state: {start}"
    );
    assert_ne!(
        notification_state(&rig, &id).as_deref(),
        Some("notified"),
        "dispatch alone became a receipt"
    );
    send_fixture_key(&rig, &pane, "C-t");

    rig.ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "state"
                && event["data"]["pane_id"] == pane.as_str()
                && event["data"]["state"] == "working"
        })
        .await;
    wait_notification_state(&mut rig, &id, &["notified"]).await;

    let end = report(
        &rig,
        "Stop",
        json!({"session_id": "session-1", "prompt_id": "prompt-1"}),
    )
    .await;
    assert_eq!(end["applied"], true, "{end}");
    assert!(end["state"].is_null(), "candidate Stop claimed idle: {end}");
    send_fixture_key(&rig, &pane, "C-y");
    wait_pane_state(&mut rig, "idle").await;

    let working_ts = rig
        .ledger_lines()
        .iter()
        .find(|line| line["kind"] == "state" && line["data"]["state"] == "working")
        .and_then(|line| line["ts"].as_u64())
        .expect("Working state line");
    let notified_ts = workspace_lines(&rig)
        .into_iter()
        .find(|line| {
            line.id == id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition" && data["state"] == "notified"
                })
        })
        .map(|line| line.ts)
        .expect("notified transition");
    assert!(
        working_ts <= notified_ts,
        "receipt published before accepted Working"
    );
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_blocked_dispatch_never_becomes_a_verified_receipt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new(
        "claude-blocked",
        &manifest,
        &swallowing_animated_composer_pane(),
        "ack_timeout_ms = 200\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;

    let (sent, _) = rig
        .send(json!({"to": ["keyed"], "subject": "blocked", "body": "body"}))
        .await;
    let id = sent["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &id).await;
    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-1",
            "prompt_id": "prompt-1",
            "prompt": doorbell_for(&rig, &id),
        }),
    )
    .await;
    assert_eq!(start["matched"], true, "{start}");
    assert!(
        start["state"].is_null(),
        "candidate claimed runtime state: {start}"
    );
    send_fixture_key(&rig, &pane, "C-l");

    // No receipt is not an alarm: the record closes as Notified with no
    // verifier, and the unconfirmed dispatch never became that verifier.
    wait_notification_state(&mut rig, &id, &["notified"]).await;
    let notified = workspace_lines(&rig)
        .into_iter()
        .find(|line| {
            line.id == id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition" && data["state"] == "notified"
                })
        })
        .expect("notified transition");
    assert!(
        notified.data.as_ref().unwrap().get("verified_by").is_none(),
        "a blocked dispatch became a verified receipt: {notified:?}"
    );
    let screen = rig.tmux.capture(&pane);
    assert!(
        screen.contains(&doorbell_for(&rig, &id)),
        "blocked doorbell left the composer: {screen}"
    );
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_blocked_stop_can_be_retried_for_the_same_prompt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new("claude-stop", &manifest, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-1",
            "prompt_id": "prompt-1",
            "prompt": "manual prompt",
        }),
    )
    .await;
    assert!(start["state"].is_null(), "{start}");
    send_fixture_key(&rig, &pane, "C-t");
    wait_pane_state(&mut rig, "working").await;

    let first_stop = report(
        &rig,
        "Stop",
        json!({"session_id": "session-1", "prompt_id": "prompt-1"}),
    )
    .await;
    assert_eq!(first_stop["applied"], true, "{first_stop}");
    assert!(
        first_stop["state"].is_null(),
        "candidate Stop claimed idle: {first_stop}"
    );
    let read = rig
        .ctl
        .request(
            "pane.read",
            json!({"target": "keyed", "source": "detection"}),
        )
        .await;
    assert_eq!(read["result"]["detection"]["state"], "working", "{read}");

    let second_stop = report(
        &rig,
        "Stop",
        json!({"session_id": "session-1", "prompt_id": "prompt-1"}),
    )
    .await;
    assert_eq!(second_stop["applied"], true, "{second_stop}");
    assert_ne!(
        second_stop["duplicate"], true,
        "later Stop was deduplicated: {second_stop}"
    );
    send_fixture_key(&rig, &pane, "C-y");
    wait_pane_state(&mut rig, "idle").await;
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_stopfailure_confirms_a_short_turn_without_late_working() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new(
        "claude-stopfailure-short",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (sent, _) = rig
        .send(json!({"to": ["keyed"], "subject": "short", "body": "body"}))
        .await;
    let id = sent["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &id).await;
    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-short",
            "prompt_id": "prompt-short",
            "prompt": doorbell_for(&rig, &id),
        }),
    )
    .await;
    assert_eq!(start["matched"], true, "{start}");
    assert!(start["state"].is_null(), "{start}");

    let end = report(
        &rig,
        "StopFailure",
        json!({"session_id": "session-short", "prompt_id": "prompt-short"}),
    )
    .await;
    assert_eq!(end["state"], "idle", "{end}");
    wait_delivery_state(&mut rig, &id, "delivered_verified").await;
    let read = rig
        .ctl
        .request(
            "pane.read",
            json!({"target": "keyed", "source": "detection"}),
        )
        .await;
    assert_eq!(read["result"]["detection"]["state"], "idle", "{read}");
    assert_eq!(read["result"]["detection"]["write_ready"], true, "{read}");
    assert!(!rig
        .ledger_lines()
        .iter()
        .any(|line| line["kind"] == "state" && line["data"]["state"] == "working"));
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_stopfailure_before_dispatch_still_confirms_the_exact_message() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new(
        "claude-stopfailure-before-dispatch",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (sent, _) = rig
        .send(json!({"to": ["keyed"], "subject": "reordered", "body": "body"}))
        .await;
    let id = sent["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &id).await;

    let end = report(
        &rig,
        "StopFailure",
        json!({"session_id": "session-reordered", "prompt_id": "prompt-reordered"}),
    )
    .await;
    assert!(
        end["state"].is_null(),
        "an end without its start claimed state: {end}"
    );

    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-reordered",
            "prompt_id": "prompt-reordered",
            "prompt": doorbell_for(&rig, &id),
        }),
    )
    .await;
    assert_eq!(start["matched"], true, "{start}");
    wait_delivery_state(&mut rig, &id, "delivered_verified").await;

    let read = rig
        .ctl
        .request(
            "pane.read",
            json!({"target": "keyed", "source": "detection"}),
        )
        .await;
    assert_eq!(read["result"]["detection"]["state"], "idle", "{read}");
    assert_eq!(read["result"]["detection"]["write_ready"], true, "{read}");
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_stopfailure_confirms_a_superseded_exact_dispatch() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new(
        "claude-stopfailure-superseded",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (sent, _) = rig
        .send(json!({"to": ["keyed"], "subject": "first", "body": "body"}))
        .await;
    let id = sent["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &id).await;
    let first = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-superseded",
            "prompt_id": "prompt-a",
            "prompt": doorbell_for(&rig, &id),
        }),
    )
    .await;
    assert_eq!(first["matched"], true, "{first}");

    let second = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-superseded",
            "prompt_id": "prompt-b",
            "prompt": "a later human prompt",
        }),
    )
    .await;
    assert_eq!(second["matched"], false, "{second}");
    let end_first = report(
        &rig,
        "StopFailure",
        json!({"session_id": "session-superseded", "prompt_id": "prompt-a"}),
    )
    .await;
    assert_eq!(end_first["applied"], true, "{end_first}");
    wait_delivery_state(&mut rig, &id, "delivered_verified").await;

    send_fixture_key(&rig, &pane, "C-t");
    wait_pane_state(&mut rig, "working").await;
    let active = rig
        .ctl
        .request(
            "pane.read",
            json!({"target": "keyed", "source": "detection"}),
        )
        .await;
    assert_eq!(
        active["result"]["detection"]["state"], "working",
        "{active}"
    );

    let end_second = report(
        &rig,
        "StopFailure",
        json!({"session_id": "session-superseded", "prompt_id": "prompt-b"}),
    )
    .await;
    assert_eq!(end_second["state"], "idle", "{end_second}");
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_stopfailure_does_not_override_visual_working_or_release_a_dirty_barrier() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new(
        "claude-stopfailure-active",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (first, _) = rig
        .send(json!({"to": ["keyed"], "subject": "first", "body": "body"}))
        .await;
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &first_id).await;
    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-active",
            "prompt_id": "prompt-active",
            "prompt": doorbell_for(&rig, &first_id),
        }),
    )
    .await;
    assert_eq!(start["matched"], true, "{start}");
    send_fixture_key(&rig, &pane, "C-t");
    wait_pane_state(&mut rig, "working").await;
    wait_delivery_state(&mut rig, &first_id, "delivered_verified").await;
    claim(&rig, "keyed", &first_id);

    let (second, _) = rig
        .send(json!({"to": ["keyed"], "subject": "second", "body": "body"}))
        .await;
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    wait_delivery_state(&mut rig, &second_id, "gating").await;

    let mismatch = report(
        &rig,
        "StopFailure",
        json!({"session_id": "session-active", "prompt_id": "other-prompt"}),
    )
    .await;
    assert!(mismatch["state"].is_null(), "{mismatch}");
    wait_pane_state(&mut rig, "working").await;

    let end = report(
        &rig,
        "StopFailure",
        json!({"session_id": "session-active", "prompt_id": "prompt-active"}),
    )
    .await;
    assert_eq!(end["state"], "idle", "{end}");
    let dirty = rig
        .ctl
        .request(
            "pane.read",
            json!({"target": "keyed", "source": "detection"}),
        )
        .await;
    assert_eq!(dirty["result"]["detection"]["state"], "working", "{dirty}");
    assert_eq!(
        dirty["result"]["detection"]["write_ready"], false,
        "{dirty}"
    );
    assert_eq!(
        notification_state(&rig, &second_id).as_deref(),
        Some("gating")
    );

    send_fixture_key(&rig, &pane, "C-y");
    wait_submitted(&mut rig, &second_id).await;
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_interrupt_updates_status_without_fabricating_an_exact_end() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = claude_candidate_manifest();
    let mut rig = Rig::new(
        "claude-interrupt",
        &manifest,
        &manual_lifecycle_composer_pane(),
        "ack_timeout_ms = 3000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let (first, _) = rig
        .send(json!({"to": ["keyed"], "subject": "interrupt", "body": "body"}))
        .await;
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    wait_submitted(&mut rig, &first_id).await;
    report(
        &rig,
        "UserPromptSubmit",
        json!({
            "session_id": "session-interrupt",
            "prompt_id": "prompt-interrupt",
            "prompt": doorbell_for(&rig, &first_id),
        }),
    )
    .await;
    send_fixture_key(&rig, &pane, "C-t");
    wait_pane_state(&mut rig, "working").await;
    wait_delivery_state(&mut rig, &first_id, "delivered_verified").await;
    claim(&rig, "keyed", &first_id);

    let (second, _) = rig
        .send(json!({"to": ["keyed"], "subject": "held", "body": "body"}))
        .await;
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    wait_delivery_state(&mut rig, &second_id, "gating").await;
    send_fixture_key(&rig, &pane, "Escape");
    wait_pane_state(&mut rig, "working").await;
    let interrupted = rig
        .ctl
        .request(
            "pane.read",
            json!({"target": "keyed", "source": "detection"}),
        )
        .await;
    assert_eq!(
        interrupted["result"]["detection"]["state"], "working",
        "visual idle cannot erase an exact active start: {interrupted}"
    );
    assert_eq!(
        interrupted["result"]["detection"]["write_ready"], false,
        "visual idle must not fabricate the exact end: {interrupted}"
    );
    assert_eq!(
        notification_state(&rig, &second_id).as_deref(),
        Some("gating")
    );

    let end = report(
        &rig,
        "StopFailure",
        json!({"session_id": "session-interrupt", "prompt_id": "prompt-interrupt"}),
    )
    .await;
    assert_eq!(end["applied"], true, "{end}");
    assert_eq!(end["state"], "idle", "{end}");
    wait_pane_state(&mut rig, "idle").await;
    wait_submitted(&mut rig, &second_id).await;
    rig.shutdown().await;
}

struct HeldTmuxSocket {
    original: PathBuf,
    held: PathBuf,
}

impl HeldTmuxSocket {
    fn disconnect(rig: &Rig, session: &str) -> HeldTmuxSocket {
        let original = rig.tmux.socket_path().expect("tmux socket path");
        let held = original.with_extension(format!("cyclops-held-{}", std::process::id()));
        std::fs::rename(&original, &held).expect("hold tmux socket path");
        let socket = HeldTmuxSocket { original, held };

        // Address the server through the held pathname and close its control
        // client. The daemon still dials the original pathname, so it cannot
        // reconnect until restore() puts the socket back. Renaming a session
        // is not equivalent: the watcher follows that event and reconnects
        // under the new name.
        let output = std::process::Command::new("tmux")
            .args(["-u", "-S"])
            .arg(&socket.held)
            .args(["-f", "/dev/null", "detach-client", "-s", session])
            .env_remove("TMUX")
            .output()
            .expect("detach tmux control client");
        assert!(
            output.status.success(),
            "tmux detach failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        socket
    }

    fn restore(mut self) {
        self.restore_inner().expect("restore tmux socket path");
    }

    fn restore_inner(&mut self) -> std::io::Result<()> {
        if self.held.exists() {
            std::fs::rename(&self.held, &self.original)?;
        }
        Ok(())
    }
}

impl Drop for HeldTmuxSocket {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

/// An authenticated start is the earliest reliable signal that Claude
/// accepted a prompt. The title and screen can still show the idle composer
/// until the first output frame, so those frames must not keep runtime state
/// at idle or evict the start after repeated recomputes.
#[tokio::test(flavor = "multi_thread")]
async fn an_authenticated_start_reports_working_before_visual_output() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("preoutput", HOOK_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;
    wait_pane_state(&mut rig, "idle").await;

    let before = rig.tmux.capture(&pane);
    assert!(
        !before.contains("FAKETUI-WORKING"),
        "fixture already showed visual work: {before}"
    );

    let start = report(
        &rig,
        "UserPromptSubmit",
        json!({"prompt": "a human prompt with no Cyclops message"}),
    )
    .await;
    assert_eq!(start["state"], "working", "{start}");

    // Every read forces the visual sensors. Their repeated idle verdicts
    // must remain observable disagreement without erasing the lifecycle.
    for round in 0..6 {
        let read = rig
            .ctl
            .request(
                "pane.read",
                json!({"target": "keyed", "source": "detection"}),
            )
            .await;
        let detection = &read["result"]["detection"];
        assert_eq!(detection["state"], "working", "round {round}: {read}");
        assert_eq!(detection["disagreement"], true, "round {round}: {read}");
        assert_eq!(detection["write_ready"], false, "round {round}: {read}");
    }

    let after = rig.tmux.capture(&pane);
    assert!(
        !after.contains("FAKETUI-WORKING"),
        "the fixture produced output instead of testing the pre-output gap: {after}"
    );

    let end = report(&rig, "Stop", json!({})).await;
    assert_eq!(end["state"], "idle", "{end}");
    wait_pane_state(&mut rig, "idle").await;

    rig.shutdown().await;
}

/// A start for a turn that has already ended is not a turn running, and
/// the pane it names must not be left holding because of it.
///
/// The order hook reports arrive in is not the vendor's contract. When
/// the end lands first, a later start naming that same turn describes
/// something already over. Publishing `working` for it leaves the runtime
/// saying so with nothing left to correct it, because the turn is
/// finished and no further report is coming: the composer hold waits on a
/// clean screen it can never be released against, and the next delivery
/// to that pane never happens.
///
/// The whole sequence runs through the daemon's own ingestion: a real
/// delivery takes the composer, its end and its acknowledgement arrive
/// out of order, and a second delivery has to follow it.
#[tokio::test(flavor = "multi_thread")]
async fn an_out_of_order_turn_does_not_strand_the_composer() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = codex_lifecycle_manifest();
    let mut rig = Rig::new(
        "keyend",
        &manifest,
        &composer_pane(),
        "receipt_block_ms = 2000\nack_timeout_ms = 8000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;

    macro_rules! submitted {
        ($id:expr) => {{
            let id: String = $id;
            wait_notification_state(
                &mut rig,
                &id,
                &["submitted", "submitted_unverified", "notified"],
            )
            .await;
        }};
    }

    // One real delivery takes the composer.
    let (result, _) = rig
        .send(json!({"to": ["keyed"], "subject": "first", "body": "b"}))
        .await;
    let first = result["msg_id"].as_str().expect("msg id").to_string();
    submitted!(first.clone());

    // Its END arrives before its acknowledgement. Both name the same
    // turn, which is the only thing that makes them the same turn.
    let end = json!({
        "agent": "keyed",
        "event": "Stop",
        "payload": {"session_id": "s1", "turn_id": "t1"},
    });
    let resp = rig
        .daemon
        .report_state(serde_json::from_value(end).expect("end params"))
        .await
        .expect("report ok");
    assert_eq!(resp["applied"], true, "{resp}");

    // Then the acknowledgement for that same turn, carrying the exact
    // payload this delivery sent, which is what binds it.
    let ack = json!({
        "agent": "keyed",
        "event": "UserPromptSubmit",
        "payload": {
            "session_id": "s1",
            "turn_id": "t1",
            "prompt": doorbell_for(&rig, &first),
        },
    });
    let resp = rig
        .daemon
        .report_state(serde_json::from_value(ack).expect("ack params"))
        .await
        .expect("report ok");
    assert_eq!(resp["applied"], true, "{resp}");

    // The turn is over and the composer is free. A delivery queued behind
    // it has to reach the pane; before the fix it waited on a turn that
    // had already ended and nothing was left to say so.
    claim(&rig, "keyed", &first);
    let (result, _) = rig
        .send(json!({"to": ["keyed"], "subject": "second", "body": "b"}))
        .await;
    let second = result["msg_id"].as_str().expect("msg id").to_string();
    submitted!(second);

    rig.shutdown().await;
}

/// A turn end must match every field in the shipped Codex key. Sharing
/// either the session or the turn is not enough to release the composer.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_codex_turn_match_keeps_the_composer_held() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = codex_lifecycle_manifest();
    let mut rig = Rig::new(
        "keymismatch",
        &manifest,
        &composer_pane(),
        "receipt_block_ms = 2000\nack_timeout_ms = 8000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;

    let first = acknowledge_codex_turn(&mut rig, "first", "s1", "t1").await;
    claim(&rig, "keyed", &first);

    let (result, _) = rig
        .send(json!({"to": ["keyed"], "subject": "second", "body": "b"}))
        .await;
    let second = result["msg_id"].as_str().expect("msg id").to_string();
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == second.as_str()
                && e["data"]["action"] == "hold"
        })
        .await;

    for payload in [
        json!({"session_id": "other", "turn_id": "t1"}),
        json!({"session_id": "s1", "turn_id": "other"}),
    ] {
        let end = report(&rig, "Stop", payload).await;
        assert_eq!(end["applied"], true, "{end}");
        let read = rig
            .ctl
            .request(
                "pane.read",
                json!({"target": "keyed", "source": "detection"}),
            )
            .await;
        assert_eq!(
            read["result"]["detection"]["state"], "working",
            "another turn's end cleared the active lifecycle: {read}"
        );
    }
    assert_eq!(
        notification_state(&rig, &second).as_deref(),
        Some("gating"),
        "a partial turn-key match released the next delivery"
    );
    assert!(
        !rig.tmux
            .capture(&pane)
            .contains(&format!("[cyclops:end {second}]")),
        "the held delivery reached the composer"
    );

    let end = report(&rig, "Stop", json!({"session_id": "s1", "turn_id": "t1"})).await;
    assert_eq!(end["applied"], true, "{end}");
    wait_submitted(&mut rig, &second).await;

    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// A matching end received while detached is lifecycle evidence, not a
/// write authorization. It is retained until reattach supplies a current
/// screen capture that proves the composer is clean.
#[tokio::test(flavor = "multi_thread")]
async fn a_detached_codex_end_releases_only_after_a_fresh_clean_capture() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = codex_lifecycle_manifest();
    let mut rig = Rig::new(
        "keydetach",
        &manifest,
        &composer_pane(),
        "receipt_block_ms = 2000\nack_timeout_ms = 8000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "keyed").await;

    let first = acknowledge_codex_turn(&mut rig, "first", "s1", "t1").await;
    claim(&rig, "keyed", &first);

    let (result, _) = rig
        .send(json!({"to": ["keyed"], "subject": "second", "body": "b"}))
        .await;
    let second = result["msg_id"].as_str().expect("msg id").to_string();
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == second.as_str()
                && e["data"]["action"] == "hold"
        })
        .await;

    // An end from another turn changes neither the active runtime start nor
    // the exact hold. The following detach therefore exercises stored
    // matching lifecycle evidence rather than a transient sensor conflict.
    let mismatch = report(
        &rig,
        "Stop",
        json!({"session_id": "s1", "turn_id": "other"}),
    )
    .await;
    assert_eq!(mismatch["applied"], true, "{mismatch}");
    let read = rig
        .ctl
        .request(
            "pane.read",
            json!({"target": "keyed", "source": "detection"}),
        )
        .await;
    assert_eq!(read["result"]["detection"]["state"], "working", "{read}");

    // Whatever the second doorbell recorded before the outage stands; the
    // detached end must add nothing. A write needs a live watcher, and
    // stored lifecycle evidence is not one.
    let held_socket = HeldTmuxSocket::disconnect(&rig, "main");
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "session" && e["data"]["attached"] == false
        })
        .await;
    let before_detached_end = notification_states(&rig, &second);
    let end = report(&rig, "Stop", json!({"session_id": "s1", "turn_id": "t1"})).await;
    assert_eq!(end["applied"], true, "{end}");
    assert_eq!(end["live"], false, "{end}");
    // A bounded window: nothing publishes the absence of an append.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        notification_states(&rig, &second),
        before_detached_end,
        "stored lifecycle evidence authorized a write while detached"
    );

    held_socket.restore();
    let availability = rig
        .ctl
        .request("session.watch", json!({"session": "main"}))
        .await;
    assert_eq!(
        availability["result"]["added"],
        json!(false),
        "{availability}"
    );
    rig.ev
        .wait_event(Duration::from_secs(15), |e| {
            e["event"] == "session" && e["data"]["attached"] == true
        })
        .await;
    wait_submitted(&mut rig, &second).await;

    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// Install an inject-pause seam that parks a delivery at `phase`; the
/// returned semaphore releases one pass.
fn park_at(
    rig: &Rig,
    phase: &'static str,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
    Arc<tokio::sync::Semaphore>,
) {
    let (entered_tx, entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let release_seam = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |p| {
        let entered_tx = entered_tx.clone();
        let release = Arc::clone(&release_seam);
        Box::pin(async move {
            if p != phase {
                return;
            }
            let _ = entered_tx.send(p);
            release
                .acquire_owned()
                .await
                .expect("seam release")
                .forget();
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    });
    (entered_rx, release)
}

/// An acknowledgement is never lost to the moment it arrives in.
///
/// The worker moves a delivery from staged to submitted and then looks
/// for an acknowledgement that landed early. A report arriving in that
/// gap used to read the state, see one thing, and write to the other:
/// classification and installation were separate, so an acknowledgement
/// could be recorded just after the only read of it, or resolve nothing
/// because the delivery had already moved on.
///
/// Both sides of the gap are forced here rather than raced for. The seam
/// parks the worker at a known point and the report is posted while it
/// waits, so each interleaving happens every run.
#[tokio::test(flavor = "multi_thread")]
async fn an_acknowledgement_in_the_submit_gap_still_resolves() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // `post_key` parks after the submit key and the binding, with the
    // delivery still staged, so the report has to INSTALL and the worker
    // has to find it. `post_submit` parks after the submitted line and
    // before the record is read, so the report finds it submitted and has
    // to resolve the delivery itself. Between them they are both sides of
    // the gap.
    for phase in ["post_key", "post_submit"] {
        let manifest = codex_lifecycle_manifest();
        let mut rig = Rig::new(
            "ackgap",
            &manifest,
            &composer_pane(),
            "receipt_block_ms = 100\nack_timeout_ms = 8000\n",
        )
        .await;
        let pane = rig.pane_ids().await[0].clone();
        rig.label(&pane, "keyed").await;
        let (mut entered, release) = park_at(&rig, phase);

        let (result, _) = rig
            .send(json!({"to": ["keyed"], "subject": "gap", "body": "b"}))
            .await;
        let msg_id = result["msg_id"].as_str().expect("msg id").to_string();

        tokio::time::timeout(Duration::from_secs(10), entered.recv())
            .await
            .unwrap_or_else(|_| panic!("{phase} seam not reached within 10s"))
            .expect("seam channel open");

        let ack = json!({
            "agent": "keyed",
            "event": "UserPromptSubmit",
            "payload": {
                "session_id": "s1",
                "turn_id": "t1",
                "prompt": doorbell_for(&rig, &msg_id),
            },
        });
        let resp = rig
            .daemon
            .report_state(serde_json::from_value(ack).expect("ack params"))
            .await
            .expect("report ok");
        assert_eq!(resp["applied"], true, "{phase}: {resp}");
        release.add_permits(1);

        wait_notification_state(&mut rig, &msg_id, &["notified"]).await;
        rig.shutdown().await;
    }
}
