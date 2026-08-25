//! M2 read side, end to end: two fixture panes exchange real messages
//! through the daemon (isolated tmux server, in-process daemon, NDJSON
//! socket clients), then msg.history and msg.thread reconstruct the
//! conversation from the ledger. Reading never writes.

mod common;

use common::*;
use cyclops_proto::MsgSendParams;
use serde_json::{json, Value};

fn params(v: Value) -> MsgSendParams {
    serde_json::from_value(v).expect("send params")
}

fn line_ids(resp: &Value) -> Vec<String> {
    resp["result"]["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("no lines in {resp}"))
        .iter()
        .map(|l| l["id"].as_str().expect("line id").to_string())
        .collect()
}

/// LOW: recipients are ledgered under their canonical name (the pane's
/// label, or the pane id when unlabeled) however the sender addressed
/// them. Before the fix a send to "%N" of a labeled pane recorded "%N",
/// and `history --with <label>` silently missed the message.
#[tokio::test(flavor = "multi_thread")]
async fn recipients_are_ledgered_by_label_however_addressed() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "canon",
        CAT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 10000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "canon").await;

    // Addressed by pane id; ledgered and receipted under the label.
    let (result, _) = rig
        .send(json!({"to": [pane.as_str()], "subject": "aliased", "body": "b"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    assert_eq!(result["deliveries"][0]["to"], "canon", "{result}");

    let msg = rig
        .ledger_lines()
        .into_iter()
        .find(|l| l["kind"] == "msg" && l["id"] == msg_id.as_str())
        .expect("msg line");
    assert_eq!(msg["to"], json!(["canon"]), "{msg}");

    // History matches naturally on the label now.
    let resp = rig
        .ctl
        .request("msg.history", json!({"with": "canon"}))
        .await;
    assert_eq!(line_ids(&resp), vec![msg_id.clone()], "{resp}");

    // Addressing the label AND the pane id in one send is ONE recipient.
    let (result, _) = rig
        .send(json!({"to": ["canon", pane.as_str()], "subject": "dedupe", "body": "b"}))
        .await;
    assert_eq!(
        result["deliveries"].as_array().unwrap().len(),
        1,
        "alias forms must collapse to one delivery: {result}"
    );
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// MEDIUM: cursor paging across several watched sessions. The per-file u64
/// seq is ambiguous there (before the fix it silently skipped messages),
/// so it is refused, and the opaque composite cursor2 walks the merged
/// record without gaps or duplicates.
#[tokio::test(flavor = "multi_thread")]
async fn multi_session_paging_is_gapless_and_refuses_the_raw_seq_cursor() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_multi(
        "m2page",
        CAT_MANIFEST,
        &[("alpha", &composer_pane()), ("beta", &composer_pane())],
        "receipt_block_ms = 10000\n",
    )
    .await;
    let left = rig.pane_ids_session(0).await[0].clone();
    let right = rig.pane_ids_session(1).await[0].clone();
    rig.label(&left, "left").await;
    rig.label(&right, "right").await;

    // Alternating recipients interleave msg lines across the two session
    // files with clashing per-file seqs.
    let mut sent: Vec<String> = Vec::new();
    for (i, to) in ["left", "right", "left", "right", "left"]
        .iter()
        .enumerate()
    {
        let (r, _) = rig
            .send(json!({"to": [to], "subject": format!("page {i}"), "body": "b"}))
            .await;
        sent.push(r["msg_id"].as_str().unwrap().to_string());
    }

    // The raw seq cursor is refused with the workaround named.
    let resp = rig.ctl.request("msg.history", json!({"cursor": 0})).await;
    assert_eq!(resp["error"]["code"], "bad_request", "{resp}");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cursor2"),
        "{resp}"
    );

    // Tail: newest limit, no u64 cursor, a composite cursor instead.
    let resp = rig.ctl.request("msg.history", json!({"limit": 2})).await;
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(line_ids(&resp), sent[3..].to_vec(), "{resp}");
    assert!(resp["result"]["next_cursor"].is_null(), "{resp}");
    assert!(resp["result"]["next_cursor2"].is_string(), "{resp}");

    // The walk: empty cursor2 starts from the beginning; feeding
    // next_cursor2 back covers every message exactly once, in order.
    // Before the fix the equivalent seq walk skipped whichever session's
    // lines hid behind the other's seqs.
    let mut walked: Vec<String> = Vec::new();
    let mut cursor2 = json!("");
    loop {
        let resp = rig
            .ctl
            .request("msg.history", json!({"limit": 2, "cursor2": cursor2}))
            .await;
        assert!(resp["error"].is_null(), "{resp}");
        let ids = line_ids(&resp);
        if ids.is_empty() {
            assert!(resp["result"]["next_cursor2"].is_null(), "{resp}");
            break;
        }
        walked.extend(ids);
        cursor2 = resp["result"]["next_cursor2"].clone();
        assert!(cursor2.is_string(), "{resp}");
    }
    assert_eq!(walked, sent, "gapless, dupe-free walk over both sessions");
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn history_reconstructs_a_two_pane_conversation() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Generous receipt cap, as in the m1 fan-out test: this asserts read
    // semantics, not the 2.5s budget, and parallel-workspace load can push
    // the second screen-tier delivery past the default cap.
    let mut rig = Rig::new(
        "m2hist",
        CAT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 10000\n",
    )
    .await;
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", "main:0", &composer_pane()]);
    rig.wait_attached(2).await;
    let panes = rig.pane_ids().await;
    rig.label(&panes[0], "codex").await;
    rig.label(&panes[1], "reviewer").await;

    // The conversation: codex asks, reviewer replies (reply_to), admin
    // broadcasts an fyi to both. Plus two lines outside it: admin to codex
    // only, and codex to admin (no admin pane: attention_required).
    let r1 = rig
        .daemon
        .deliver_payload(
            "codex",
            params(json!({
                "to": ["reviewer"],
                "subject": "Review the rate limiter",
                "body": "gateway.rs:120 drops the burst path",
            })),
        )
        .await
        .expect("send 1");
    let m1 = r1["msg_id"].as_str().expect("msg id").to_string();

    let r2 = rig
        .daemon
        .deliver_payload(
            "reviewer",
            params(json!({
                "to": ["codex"],
                "subject": "Re: Review the rate limiter",
                "body": "Done. One nit in the retry path.",
                "reply_to": m1,
            })),
        )
        .await
        .expect("send 2");
    let m2 = r2["msg_id"].as_str().expect("msg id").to_string();

    let (r3, _) = rig
        .send(json!({"to": ["codex", "reviewer"], "subject": "Standup in 5", "fyi": true}))
        .await;
    let m3 = r3["msg_id"].as_str().expect("msg id").to_string();

    let (r4, _) = rig
        .send(json!({"to": ["codex"], "subject": "Only for codex", "body": "b"}))
        .await;
    let m4 = r4["msg_id"].as_str().expect("msg id").to_string();

    let r5 = rig
        .daemon
        .deliver_payload(
            "codex",
            params(json!({"to": ["admin"], "subject": "Need a decision", "body": "Ship or hold?"})),
        )
        .await
        .expect("send 5");
    let m5 = r5["msg_id"].as_str().expect("msg id").to_string();
    assert_eq!(r5["deliveries"][0]["state"], "attention_required", "{r5}");

    // A send can return while a recipient is still finishing the preceding
    // turn. History is an eventual ledger fold, so settle every background
    // delivery before asserting both its result and that reads append nothing.
    for (message, recipient) in [(&m3, "codex"), (&m3, "reviewer"), (&m4, "codex")] {
        if rig.final_state(message, recipient).as_deref() != Some("delivered_unverified") {
            rig.ev
                .wait_event(std::time::Duration::from_secs(10), |event| {
                    event["event"] == "delivery-state"
                        && event["data"]["id"] == message.as_str()
                        && event["data"]["to"] == recipient
                        && event["data"]["to_state"] == "delivered_unverified"
                })
                .await;
        }
    }

    let ledger_before = rig.ledger_lines().len();

    // --with reviewer reconstructs the conversation: both directions plus
    // the broadcast, ordered oldest first, nothing else.
    let resp = rig
        .ctl
        .request("msg.history", json!({"with": "reviewer"}))
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(line_ids(&resp), vec![m1.clone(), m2.clone(), m3.clone()]);
    let lines = resp["result"]["lines"].as_array().unwrap();
    for l in lines {
        for d in l["deliveries"].as_array().expect("deliveries") {
            assert_eq!(d["state"], "delivered_unverified", "folded state in {l}");
        }
    }
    // The broadcast reads coherently: ONE msg fact, N delivery badges.
    let cast = lines.iter().find(|l| l["id"] == m3.as_str()).unwrap();
    assert_eq!(cast["kind"], "fyi");
    assert_eq!(cast["deliveries"].as_array().unwrap().len(), 2);
    assert_eq!(cast["subject"], "Standup in 5");

    // Direction filters narrow one side each.
    let resp = rig
        .ctl
        .request("msg.history", json!({"from": "codex", "to": "reviewer"}))
        .await;
    assert_eq!(line_ids(&resp), vec![m1.clone()]);

    // "me" resolves through the caller's identity envelope: this client is
    // a same-uid process outside every watched pane, the human.
    let resp = rig.ctl.request("msg.history", json!({"from": "me"})).await;
    assert_eq!(line_ids(&resp), vec![m3.clone(), m4.clone()]);
    let resp = rig.ctl.request("msg.history", json!({"to": "me"})).await;
    assert_eq!(line_ids(&resp), vec![m5.clone()]);
    let to_admin = &resp["result"]["lines"][0]["deliveries"][0];
    assert_eq!(to_admin["state"], "attention_required");
    assert_eq!(to_admin["cause"], "no_such_pane");

    // Tail plus cursor walk: the newest limit first, then a gapless,
    // dupe-free forward walk over everything.
    let resp = rig.ctl.request("msg.history", json!({"limit": 2})).await;
    assert_eq!(line_ids(&resp), vec![m4.clone(), m5.clone()]);
    let mut cursor = json!(0);
    let mut walked: Vec<String> = Vec::new();
    loop {
        let resp = rig
            .ctl
            .request("msg.history", json!({"limit": 2, "cursor": cursor}))
            .await;
        let ids = line_ids(&resp);
        if ids.is_empty() {
            break;
        }
        walked.extend(ids);
        cursor = resp["result"]["next_cursor"].clone();
        assert!(cursor.is_u64(), "{resp}");
    }
    assert_eq!(
        walked,
        vec![m1.clone(), m2.clone(), m3.clone(), m4.clone(), m5.clone()]
    );

    // The thread of m1: one folded msg fact, its delivery chain, the reply.
    let resp = rig.ctl.request("msg.thread", json!({"id": m1})).await;
    assert!(resp["error"].is_null(), "{resp}");
    let lines = resp["result"]["lines"].as_array().unwrap();
    let msg_facts: Vec<&Value> = lines
        .iter()
        .filter(|l| l["kind"] == "msg" && l["id"] == m1.as_str())
        .collect();
    assert_eq!(msg_facts.len(), 1, "one msg fact: {resp}");
    assert_eq!(
        msg_facts[0]["deliveries"][0]["state"],
        "delivered_unverified"
    );
    assert!(
        lines.iter().any(|l| l["id"] == m2.as_str()),
        "reply missing: {resp}"
    );
    let chain: Vec<&str> = lines
        .iter()
        .filter(|l| l["kind"] == "state" && l["id"] == m1.as_str())
        .filter_map(|l| l["data"]["to_state"].as_str())
        .collect();
    assert_eq!(
        chain,
        vec![
            "gating",
            "pasting",
            "staged",
            "submitted",
            "delivered_unverified"
        ],
        "{resp}"
    );
    for other in [&m3, &m4, &m5] {
        assert!(
            !lines.iter().any(|l| l["id"] == other.as_str()),
            "{other} leaked into the thread"
        );
    }

    // Unknown ids answer with a named error, not an empty page.
    let resp = rig
        .ctl
        .request("msg.thread", json!({"id": "m-nope00"}))
        .await;
    assert_eq!(resp["error"]["code"], "no_such_message", "{resp}");

    // Reading is free and reading never writes.
    assert_eq!(rig.ledger_lines().len(), ledger_before);
    rig.assert_ledger_legal(&[]);

    // The record survives the daemon: a fresh boot on the same home
    // answers the same conversation from the replayed ledger.
    let mut rig = rig.reboot().await;
    let resp = rig
        .ctl
        .request("msg.history", json!({"with": "reviewer"}))
        .await;
    assert_eq!(line_ids(&resp), vec![m1, m2, m3]);
    rig.shutdown().await;
}
