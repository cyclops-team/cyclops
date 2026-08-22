//! What `cyclops send` answers with at shipped defaults.
//!
//! Both tests here are the same failure one step past the M6 setup work:
//! every earlier surface reported success, and the receipt is where the
//! user finds out that nothing happened. They differ in what the daemon
//! knew at the time.
//!
//! The rig boots on `cfg_extra = ""` on purpose. Every other delivery test
//! in this crate raises `ack_timeout_ms` or `receipt_block_ms` to make its
//! transcript reproducible, which is exactly how a receipt that cannot
//! resolve at the shipped numbers stayed invisible for six milestones.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;

/// A named pane nothing detects, at shipped defaults.
///
/// The gate refuses this delivery: no manifest binds the pane, so nothing
/// can be typed into it. The receipt has to carry that verdict, because
/// the message is spent at this moment and a script branching on exit 0
/// must not read it as delivered.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_to_a_pane_nothing_detects_says_so_on_the_receipt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // The fixture binds cat/sh/bash/dash. `sleep` binds nothing, which is
    // the state a real pane is in before its agent CLI starts.
    let mut rig = Rig::new("undetected", CAT_MANIFEST, "sleep 300", "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (result, elapsed) = rig
        .send(json!({"to": ["worker"], "subject": "Review the rate limiter"}))
        .await;
    let msg_id = result["msg_id"].as_str().expect("msg id").to_string();
    let d = &result["deliveries"][0];

    assert_eq!(
        d["state"], "attention_required",
        "the receipt reported a state the delivery never reached: {result}"
    );
    // The cause travels as the machine token the ledger records, and the
    // pane as data. Wording either one is the reader's surface's job
    // (cyclops_ui::grid::cause_words), so the daemon ships neither.
    assert_eq!(d["note"], json!("no_manifest"), "{result}");
    assert_eq!(
        d["pane"],
        json!(pane),
        "the receipt cannot name the pane the fix applies to: {result}"
    );
    assert!(
        elapsed < Duration::from_millis(2500),
        "the verdict was already known; the receipt waited {elapsed:?}"
    );

    // The record agrees with the receipt, with the gate's own cause on it.
    assert_eq!(
        rig.final_state(&msg_id, "worker").as_deref(),
        Some("attention_required")
    );
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// The first message a new user sends, at shipped defaults.
///
/// The manifest declares hooks, as every shipped manifest for a real CLI
/// does, and no hook is wired, as no first run has. The delivery is
/// screen-tier and it lands; the question is whether the receipt says so
/// before it is printed.
#[tokio::test(flavor = "multi_thread")]
async fn an_unhooked_agent_still_gets_a_delivery_badge_at_shipped_defaults() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("unhooked", HOOK_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (result, elapsed) = rig
        .send(json!({"to": ["worker"], "subject": "hello"}))
        .await;
    let d = &result["deliveries"][0];
    assert_eq!(
        d["state"], "delivered_unverified",
        "the first message printed a non-delivery badge after {elapsed:?}: {result}"
    );
    rig.shutdown().await;
}
