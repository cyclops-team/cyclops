//! M2 read side, end to end: two fixture panes exchange real messages
//! through the daemon (isolated tmux server, in-process daemon, NDJSON
//! socket clients), then msg.history and msg.thread reconstruct the
//! conversation from the ledger. Reading never writes.

mod common;

use std::fs;
use std::path::PathBuf;

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

fn workspace_journal(rig: &Rig) -> PathBuf {
    fs::read_dir(rig.home.join("workspaces"))
        .expect("workspace directory")
        .find_map(|entry| {
            let path = entry.ok()?.path().join("messages.ndjson");
            path.is_file().then_some(path)
        })
        .expect("workspace journal")
}

fn workspace_journal_bytes(rig: &Rig) -> Vec<u8> {
    fs::read(workspace_journal(rig)).expect("workspace journal readable")
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

    let msg = workspace_lines(&rig)
        .into_iter()
        .find(|l| matches!(l.kind, cyclops_proto::Kind::Msg) && l.id == msg_id)
        .expect("msg line");
    assert_eq!(msg.to, vec!["canon".to_string()], "{msg:?}");

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
    let tail = line_ids(&resp);
    assert_eq!(tail.len(), 2, "{resp}");
    assert!(resp["result"]["next_cursor"].is_null(), "{resp}");
    assert!(resp["result"]["next_cursor2"].is_string(), "{resp}");

    // The walk: empty cursor2 starts from the beginning; feeding
    // next_cursor2 back covers every message exactly once, in order.
    // Before the fix the equivalent seq walk skipped whichever session's
    // lines hid behind the other's seqs.
    let mut walked: Vec<String> = Vec::new();
    let mut walked_keys: Vec<(u64, u64, String)> = Vec::new();
    let mut cursor2 = json!("");
    loop {
        let resp = rig
            .ctl
            .request("msg.history", json!({"limit": 2, "cursor2": cursor2}))
            .await;
        assert!(resp["error"].is_null(), "{resp}");
        let lines = resp["result"]["lines"]
            .as_array()
            .unwrap_or_else(|| panic!("no lines in {resp}"));
        let ids = line_ids(&resp);
        if ids.is_empty() {
            assert!(resp["result"]["next_cursor2"].is_null(), "{resp}");
            break;
        }
        walked_keys.extend(lines.iter().map(|line| {
            (
                line["ts"].as_u64().expect("history line timestamp"),
                line["seq"].as_u64().expect("history line sequence"),
                line["id"].as_str().expect("history line id").to_string(),
            )
        }));
        walked.extend(ids);
        cursor2 = resp["result"]["next_cursor2"].clone();
        assert!(cursor2.is_string(), "{resp}");
    }
    // The two journals have independent seqs and can receive messages in
    // the same millisecond. History's documented merged order is therefore
    // its deterministic cross-file comparator, not the order these send
    // calls returned. Check the actual contract: every message appears once,
    // and the limited tail is exactly the suffix of that same complete walk.
    let mut expected_ids = sent;
    expected_ids.sort();
    let mut walked_ids = walked.clone();
    walked_ids.sort();
    assert_eq!(
        walked_ids, expected_ids,
        "gapless, dupe-free walk over both sessions"
    );
    assert_eq!(
        tail,
        walked[walked.len().saturating_sub(2)..],
        "the merged tail is the suffix of the same composite walk"
    );
    assert!(
        walked_keys.windows(2).all(|pair| pair[0] <= pair[1]),
        "the composite walk must keep the public merge order: {walked_keys:?}"
    );
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
        .msg_send(
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
        .msg_send(
            "reviewer",
            params(json!({
                "to": [],
                "subject": "Re: Review the rate limiter",
                "body": "Done. One nit in the retry path.",
                "reply_to": m1,
            })),
        )
        .await
        .expect("send 2");
    let m2 = r2["msg_id"].as_str().expect("msg id").to_string();
    // Each recipient claims its head before the next doorbell is scheduled.
    wait_notification_state(&rig, &m1, &["notified", "submitted_unverified"]).await;
    claim(&rig, "reviewer", &m1);
    wait_notification_state(&rig, &m2, &["notified", "submitted_unverified"]).await;
    claim(&rig, "codex", &m2);

    let (r3, _) = rig
        .send(json!({"to": ["codex", "reviewer"], "subject": "Standup in 5", "fyi": true}))
        .await;
    let m3 = r3["msg_id"].as_str().expect("msg id").to_string();
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while notification_states(&rig, &m3)
            .iter()
            .filter(|state| *state == "notified")
            .count()
            < 2
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "broadcast never notified both recipients: {:?}",
                notification_states(&rig, &m3)
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
    claim(&rig, "codex", &m3);
    claim(&rig, "reviewer", &m3);

    let (r4, _) = rig
        .send(json!({"to": ["codex"], "subject": "Only for codex", "body": "b"}))
        .await;
    let m4 = r4["msg_id"].as_str().expect("msg id").to_string();

    let r5 = rig
        .daemon
        .msg_send(
            "codex",
            params(json!({"to": ["admin"], "subject": "Need a decision", "body": "Ship or hold?"})),
        )
        .await
        .expect("send 5");
    let m5 = r5["msg_id"].as_str().expect("msg id").to_string();

    // A send can return while a recipient is still finishing the preceding
    // turn. History is an eventual fold, so settle every background
    // notification before asserting both its result and that reads append
    // nothing. The broadcast carries one notification per recipient.
    for (message, recipients) in [(&m1, 1), (&m2, 1), (&m3, 2), (&m4, 1)] {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let notified = notification_states(&rig, message)
                .iter()
                .filter(|state| *state == "notified")
                .count();
            if notified >= recipients {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{message} never notified {recipients} recipients: {:?}",
                notification_states(&rig, message)
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    // Delivery finality can precede the pane watcher's return to the clean
    // composer. Wait for both panes so the read-only assertion starts from a
    // quiescent ledger rather than racing the final runtime observation.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let all_idle = status["result"]["sessions"][0]["panes"]
            .as_array()
            .is_some_and(|panes| {
                panes.len() == 2 && panes.iter().all(|pane| pane["state"] == "idle")
            });
        if all_idle {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "conversation panes did not become quiescent: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let journal_before = workspace_journal_bytes(&rig);

    // --with reviewer reconstructs the conversation: both directions plus
    // the broadcast, ordered oldest first, nothing else.
    let resp = rig
        .ctl
        .request("msg.history", json!({"with": "reviewer"}))
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(line_ids(&resp), vec![m1.clone(), m2.clone(), m3.clone()]);
    let lines = resp["result"]["lines"].as_array().unwrap();
    // The broadcast reads coherently: ONE msg fact, N delivery badges.
    let cast = lines.iter().find(|l| l["id"] == m3.as_str()).unwrap();
    assert_eq!(cast["kind"], "fyi");
    assert_eq!(cast["to"].as_array().map(Vec::len), Some(2), "{cast}");
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

    // Tail plus cursor walk: the newest limit first, then a gapless,
    // dupe-free forward walk over everything. History reads the workspace
    // journal and the session ledger, so the walk uses the composite
    // cursor2; the per-journal seq cursor is refused with several sources.
    let resp = rig.ctl.request("msg.history", json!({"limit": 2})).await;
    assert_eq!(line_ids(&resp), vec![m4.clone(), m5.clone()]);
    let mut cursor2 = json!("");
    let mut walked: Vec<String> = Vec::new();
    loop {
        let resp = rig
            .ctl
            .request("msg.history", json!({"limit": 2, "cursor2": cursor2}))
            .await;
        assert!(resp["error"].is_null(), "{resp}");
        let ids = line_ids(&resp);
        if ids.is_empty() {
            break;
        }
        walked.extend(ids);
        cursor2 = resp["result"]["next_cursor2"].clone();
        assert!(cursor2.is_string(), "{resp}");
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
    assert_eq!(msg_facts[0]["to"], json!(["reviewer"]), "{resp}");
    assert!(
        lines.iter().any(|l| l["id"] == m2.as_str()),
        "reply missing: {resp}"
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

    // The history API reads the durable message journal without mutating it.
    // Session-state observations may still arrive on the separate pane ledger.
    assert_eq!(workspace_journal_bytes(&rig), journal_before);

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
