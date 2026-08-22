//! Explicit recovery of a staged mailbox notification through daemon dispatch.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{faketui_path, tmux_available, Rig, CAT_MANIFEST};
use cyclops_proto::{Kind, LedgerLine, MessageId, MsgSendParams, NotificationResolution};
use serde_json::json;

fn recovery_manifest() -> String {
    let mut manifest = CAT_MANIFEST.replace(
        "safe_states = [\"idle\"]\n",
        "safe_states = [\"idle\"]\n\
         clear_keys = [\"C-c\"]\n\
         composer_prompt_regex = '^❯ (?P<content>.*)$'\n\
         composer_continuation_regex = '^(?P<content>.*)$'\n",
    );
    manifest.push_str(
        "\n[[rule]]\n\
         id = \"blocked_title\"\n\
         state = \"blocked_modal\"\n\
         priority = 500\n\
         region = \"pane_title\"\n\
         regex = ['^BLOCKED$']\n",
    );
    assert_ne!(manifest, CAT_MANIFEST, "fixture injection block changed");
    manifest
}

fn recovery_composer_pane() -> String {
    format!("python3 {} --swallow-once --clear-staged", faketui_path())
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

async fn wait_for_empty_composer(rig: &Rig, pane: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = rig.tmux.capture(pane);
        if screen.lines().any(|line| line.trim_end() == "❯") {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "composer did not become empty: {screen}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn active_composer_line(screen: &str) -> Option<&str> {
    let lines: Vec<_> = screen.lines().collect();
    let trailer = lines.iter().position(|line| line.starts_with('─'))?;
    trailer
        .checked_sub(1)
        .and_then(|index| lines.get(index))
        .copied()
}

fn resolution_lines<'a>(lines: &'a [LedgerLine], attempt_id: &str) -> Vec<&'a LedgerLine> {
    lines
        .iter()
        .filter(|line| {
            line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_resolved" && data["attempt_id"] == attempt_id
            })
        })
        .collect()
}

fn intent_lines<'a>(lines: &'a [LedgerLine], attempt_id: &str) -> Vec<&'a LedgerLine> {
    lines
        .iter()
        .filter(|line| {
            line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_resolution_intent" && data["attempt_id"] == attempt_id
            })
        })
        .collect()
}

fn withdrawn_intent_lines<'a>(lines: &'a [LedgerLine], attempt_id: &str) -> Vec<&'a LedgerLine> {
    lines
        .iter()
        .filter(|line| {
            line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_resolution_intent_withdrawn"
                    && data["attempt_id"] == attempt_id
            })
        })
        .collect()
}

fn assert_content_free_resolution(line: &LedgerLine, resolution: NotificationResolution) {
    assert_eq!(line.kind, Kind::State);
    assert!(line.subject.is_none());
    assert!(line.body.is_none());
    let data = line.data.as_ref().expect("resolution data");
    assert_eq!(
        data["resolution"],
        serde_json::to_value(resolution).unwrap()
    );
    let mut keys: Vec<_> = data
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "attempt_id",
            "message_id",
            "recipient",
            "record_version",
            "resolution",
            "type",
        ]
    );
}

async fn assert_snapshot_resolved(
    rig: &mut Rig,
    message_id: &str,
    resolution: NotificationResolution,
) -> u64 {
    let snapshot = rig
        .ctl
        .request("messages.snapshot", json!({"recent_settled": 20}))
        .await;
    assert!(snapshot["error"].is_null(), "snapshot failed: {snapshot}");
    let row = snapshot["result"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["message_id"] == message_id))
        .unwrap_or_else(|| panic!("message missing from snapshot: {snapshot}"));
    assert_eq!(row["needs_action"], false, "resolved row stayed in Work");
    assert_eq!(row["recipients"][0]["needs_action"], false);
    assert_eq!(
        row["recipients"][0]["notification"]["resolution"],
        serde_json::to_value(resolution).unwrap()
    );
    assert_eq!(snapshot["result"]["counts"]["work_messages"], 0);
    assert_eq!(snapshot["result"]["counts"]["open_attention_entries"], 0);
    snapshot["result"]["workspace_seq"]
        .as_u64()
        .expect("workspace sequence")
}

async fn exercise_resolution(resolution: NotificationResolution, direct_fallback: bool) {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let tag = match (resolution, direct_fallback) {
        (NotificationResolution::Complete, false) => "attention-complete",
        (NotificationResolution::Discard, false) => "attention-discard",
        (NotificationResolution::Complete, true) => "attention-direct-complete",
        (NotificationResolution::Discard, true) => "attention-direct-discard",
    };
    let pane_command = recovery_composer_pane();
    let mut rig = if direct_fallback {
        Rig::new_without_mailbox_capability(
            tag,
            &manifest,
            &pane_command,
            "receipt_block_ms = 100\nack_timeout_ms = 300\n",
        )
        .await
    } else {
        Rig::new(
            tag,
            &manifest,
            &pane_command,
            "receipt_block_ms = 100\nack_timeout_ms = 300\n",
        )
        .await
    };
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Recover staged notification",
                "body": "private body",
                "client_key": format!("recovery-{resolution:?}")
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().expect("message id").to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    let doorbell = cyclops_proto::render_doorbell_v1(&MessageId::new(&message_id).unwrap());
    let expected = if direct_fallback {
        cyclopsd::render_payload(
            &message_id,
            "admin",
            "Recover staged notification",
            "private body",
            false,
        )
    } else {
        doorbell.clone()
    };
    let staged = rig.tmux.capture(&pane);
    assert!(
        staged.contains(expected.lines().next().unwrap()),
        "exact notification is not staged: {staged}"
    );
    let before_action = workspace_lines(&rig);
    let writing = before_action
        .iter()
        .find(|line| {
            line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_transition"
                    && data["attempt_id"] == attempt_id
                    && data["state"] == "writing"
            })
        })
        .expect("writing boundary fact");
    let binding = &writing.data.as_ref().unwrap()["binding"];
    assert!(binding["leader"]["pid"].as_i64().is_some_and(|pid| pid > 0));
    assert!(binding["leader"]["birth"]
        .as_u64()
        .is_some_and(|birth| birth > 0));
    assert!(binding["agent"]["pid"].as_i64().is_some_and(|pid| pid > 0));
    assert!(binding["agent"]["birth"]
        .as_u64()
        .is_some_and(|birth| birth > 0));
    assert!(binding.get("transport").is_none());
    assert_eq!(
        writing.data.as_ref().unwrap()["transport"],
        if direct_fallback {
            "direct_payload"
        } else {
            "doorbell"
        }
    );
    assert_eq!(
        writing.data.as_ref().unwrap()["doorbell_format"],
        if direct_fallback {
            serde_json::Value::Null
        } else {
            json!(cyclops_proto::DOORBELL_FORMAT_COMPACT_CLAIM)
        }
    );

    let shown = rig
        .ctl
        .request("attention.show", json!({"id": attempt_id, "diff": true}))
        .await;
    assert!(shown["error"].is_null(), "attention show failed: {shown}");
    assert_eq!(shown["result"]["checks"]["notification_exact"], true);
    assert_eq!(shown["result"]["checks"]["trailer_anchored"], true);
    assert_eq!(shown["result"]["checks"]["process_matches"], true);
    assert_eq!(shown["result"]["checks"]["manifest_matches"], true);
    assert_eq!(shown["result"]["checks"]["terminal_action_safe"], true);
    assert_eq!(shown["result"]["expected"], expected);
    assert_eq!(shown["result"]["observed"], expected);

    let before = fs::read_to_string(workspace_journal(&rig)).unwrap();
    let method = match resolution {
        NotificationResolution::Complete => "attention.complete",
        NotificationResolution::Discard => "attention.discard",
    };
    let resolved = rig.ctl.request(method, json!({"id": attempt_id})).await;
    assert!(resolved["error"].is_null(), "resolution failed: {resolved}");
    assert_eq!(resolved["result"]["attempt_id"], attempt_id);
    assert_eq!(
        resolved["result"]["resolution"],
        serde_json::to_value(resolution).unwrap()
    );

    let screen = wait_for_empty_composer(&rig, &pane).await;
    match resolution {
        NotificationResolution::Complete => assert!(
            screen.contains(expected.lines().next().unwrap())
                && active_composer_line(&screen) == Some("❯"),
            "complete did not submit the staged notification: {screen}"
        ),
        NotificationResolution::Discard => assert!(
            !screen.contains(expected.lines().next().unwrap()),
            "discard did not clear the staged notification: {screen}"
        ),
    }

    let lines = workspace_lines(&rig);
    let facts = resolution_lines(&lines, &attempt_id);
    assert_eq!(facts.len(), 1, "resolution fact count changed");
    let intents = intent_lines(&lines, &attempt_id);
    assert_eq!(intents.len(), 1, "resolution intent count changed");
    assert!(withdrawn_intent_lines(&lines, &attempt_id).is_empty());
    assert_content_free_resolution(facts[0], resolution);
    assert!(intents[0].subject.is_none());
    assert!(intents[0].body.is_none());
    let after = fs::read_to_string(workspace_journal(&rig)).unwrap();
    let appended = after
        .strip_prefix(&before)
        .expect("resolution only appends to the journal");
    assert_eq!(appended.lines().count(), 2);
    assert!(!appended.contains("private body"));
    assert!(!appended.contains("expected"));
    assert!(!appended.contains("observed"));

    let resolved_seq = facts[0].seq;
    let event = rig
        .ev
        .wait_event(Duration::from_secs(5), |event| {
            let changed = event["data"]["changed"].as_array();
            event["event"] == "messages.changed"
                && event["seq"] == resolved_seq
                && changed.is_some_and(|areas| {
                    areas.len() == 2
                        && areas.iter().any(|area| area == "attention")
                        && areas.iter().any(|area| area == "notifications")
                })
        })
        .await;
    assert_eq!(event["seq"], resolved_seq);
    assert_eq!(event["data"]["workspace_seq"], resolved_seq);
    assert_eq!(
        assert_snapshot_resolved(&mut rig, &message_id, resolution).await,
        resolved_seq
    );

    let preview = rig
        .ctl
        .request("alarm.preview", json!({"older_than_ms": 0}))
        .await;
    assert!(preview["result"]["entries"].as_array().unwrap().is_empty());
    let repeated = rig.ctl.request(method, json!({"id": attempt_id})).await;
    assert_eq!(repeated["error"]["code"], "conflict");
    assert_eq!(
        resolution_lines(&workspace_lines(&rig), &attempt_id).len(),
        1
    );
    assert_eq!(intent_lines(&workspace_lines(&rig), &attempt_id).len(), 1);

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    assert_snapshot_resolved(&mut rig, &message_id, resolution).await;
    let preview = rig
        .ctl
        .request("alarm.preview", json!({"older_than_ms": 0}))
        .await;
    assert!(preview["result"]["entries"].as_array().unwrap().is_empty());
    assert_eq!(
        resolution_lines(&workspace_lines(&rig), &attempt_id).len(),
        1
    );
    assert_eq!(intent_lines(&workspace_lines(&rig), &attempt_id).len(), 1);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn complete_submits_one_exact_staged_notification_and_replays_resolution() {
    exercise_resolution(NotificationResolution::Complete, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_clears_one_exact_staged_notification_and_replays_resolution() {
    exercise_resolution(NotificationResolution::Discard, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_fallback_complete_submits_the_exact_canonical_payload() {
    exercise_resolution(NotificationResolution::Complete, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_fallback_discard_clears_the_exact_canonical_payload() {
    exercise_resolution(NotificationResolution::Discard, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_diff_exposes_only_the_content_free_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let pane = recovery_composer_pane();
    let mut rig = Rig::new_multi(
        "attention-body-privacy",
        &manifest,
        &[("sender", &pane), ("recipient", &pane)],
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let sender = rig.pane_ids_session(0).await[0].clone();
    let recipient = rig.pane_ids_session(1).await[0].clone();
    rig.label(&sender, "sender").await;
    rig.label(&recipient, "recipient").await;

    let sent = rig
        .daemon
        .msg_send(
            "sender",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["recipient"],
                "subject": "Agent private subject",
                "body": "agent private body",
                "client_key": "agent-private-attention"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().expect("message id").to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;

    let shown = rig
        .ctl
        .request("attention.show", json!({"id": attempt_id, "diff": true}))
        .await;
    assert!(shown["error"].is_null(), "attention show failed: {shown}");
    assert_eq!(shown["result"]["checks"]["notification_exact"], true);
    assert_eq!(shown["result"]["checks"]["trailer_anchored"], true);
    assert_eq!(shown["result"]["checks"]["process_matches"], true);
    assert_eq!(shown["result"]["checks"]["manifest_matches"], true);
    assert_eq!(shown["result"]["checks"]["terminal_action_safe"], true);
    let doorbell = cyclops_proto::render_doorbell_v1(&MessageId::new(&message_id).unwrap());
    assert_eq!(shown["result"]["expected"], doorbell);
    assert_eq!(shown["result"]["observed"], doorbell);
    let encoded = shown.to_string();
    assert!(
        !encoded.contains("Agent private subject"),
        "subject leaked: {shown}"
    );
    assert!(
        !encoded.contains("agent private body"),
        "body leaked: {shown}"
    );
    assert_eq!(
        resolution_lines(&workspace_lines(&rig), &attempt_id).len(),
        0
    );
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_fallback_refuses_a_doorbell_staged_for_the_same_message() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new_without_mailbox_capability(
        "attention-direct-cross-transport",
        &manifest,
        &recovery_composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Direct transport",
                "body": "private direct body",
                "client_key": "direct-cross-transport"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    let expected = cyclopsd::render_payload(
        &message_id,
        "admin",
        "Direct transport",
        "private direct body",
        false,
    );
    let doorbell = cyclops_proto::render_doorbell_v1(&MessageId::new(&message_id).unwrap());

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-c"]);
    wait_for_empty_composer(&rig, &pane).await;
    rig.tmux
        .run_ok(&["send-keys", "-t", &pane, "-l", &doorbell]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rig.tmux.capture(&pane).contains(&doorbell) {
        assert!(Instant::now() < deadline, "doorbell was not staged");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let shown = rig
        .ctl
        .request("attention.show", json!({"id": attempt_id, "diff": true}))
        .await;
    assert!(shown["error"].is_null(), "attention show failed: {shown}");
    assert_eq!(shown["result"]["checks"]["trailer_anchored"], true);
    assert_eq!(shown["result"]["checks"]["notification_exact"], false);
    assert_eq!(shown["result"]["expected"], expected);
    assert_eq!(shown["result"]["observed"], doorbell);

    let refused = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    assert_eq!(refused["error"]["code"], "attention_evidence_failed");
    assert!(intent_lines(&workspace_lines(&rig), &attempt_id).is_empty());
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_fallback_refuses_an_altered_canonical_payload() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new_without_mailbox_capability(
        "attention-direct-altered",
        &manifest,
        &recovery_composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Altered direct transport",
                "body": "private direct body",
                "client_key": "direct-altered"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    let expected = cyclopsd::render_payload(
        &message_id,
        "admin",
        "Altered direct transport",
        "private direct body",
        false,
    );

    rig.tmux
        .run_ok(&["send-keys", "-t", &pane, "-l", "trailing human input"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rig.tmux.capture(&pane).contains("trailing human input") {
        assert!(Instant::now() < deadline, "trailing input was not staged");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let shown = rig
        .ctl
        .request("attention.show", json!({"id": attempt_id, "diff": true}))
        .await;
    assert!(shown["error"].is_null(), "attention show failed: {shown}");
    assert_eq!(shown["result"]["checks"]["trailer_anchored"], true);
    assert_eq!(shown["result"]["checks"]["notification_exact"], false);
    assert_eq!(shown["result"]["expected"], expected);
    assert_eq!(
        shown["result"]["observed"],
        format!("{expected}trailing human input")
    );

    let refused = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    assert_eq!(refused["error"]["code"], "attention_evidence_failed");
    assert!(intent_lines(&workspace_lines(&rig), &attempt_id).is_empty());
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_returns_only_the_trailer_bound_composer_candidate() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "attention-unsafe-diff",
        &manifest,
        &recovery_composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Durable secret subject",
                "body": "Durable secret body",
                "client_key": "unsafe-diff"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    rig.tmux
        .run_ok(&["send-keys", "-t", &pane, "-l", "trailing human input"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rig.tmux.capture(&pane).contains("trailing human input") {
        assert!(Instant::now() < deadline, "trailing input was not painted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let shown = rig
        .ctl
        .request("attention.show", json!({"id": attempt_id, "diff": true}))
        .await;
    let doorbell = cyclops_proto::render_doorbell_v1(&MessageId::new(&message_id).unwrap());
    assert_eq!(shown["result"]["expected"], doorbell);
    assert_eq!(
        shown["result"]["observed"],
        format!("{doorbell}trailing human input")
    );
    assert_eq!(shown["result"]["checks"]["trailer_anchored"], true);
    assert_eq!(shown["result"]["checks"]["notification_exact"], false);
    let encoded = shown.to_string();
    assert!(!encoded.contains("Durable secret subject"));
    assert!(!encoded.contains("Durable secret body"));
    assert!(intent_lines(&workspace_lines(&rig), &attempt_id).is_empty());
    rig.shutdown().await;
}

async fn exercise_resolution_crash_window(phase: &'static str, final_fact_expected: bool) {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        phase,
        &manifest,
        &recovery_composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Crash boundary secret",
                "body": "Crash boundary body",
                "client_key": phase
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let paused = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let paused = Arc::clone(&paused);
        Box::pin(async move {
            if current != phase {
                return;
            }
            let _ = entered_tx.send(());
            paused.acquire_owned().await.unwrap().forget();
        })
    });

    let socket = rig.daemon.socket_path();
    let requested_attempt = attempt_id.clone();
    let request = tokio::spawn(async move {
        let mut client = common::TestClient::connect(&socket).await;
        client
            .request("attention.complete", json!({"id": requested_attempt}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("resolution reached crash boundary")
        .expect("pause sender stayed open");

    let lines = workspace_lines(&rig);
    let intents = intent_lines(&lines, &attempt_id);
    assert_eq!(intents.len(), 1);
    let intent_seq = intents[0].seq;
    assert_eq!(
        resolution_lines(&lines, &attempt_id).len(),
        usize::from(final_fact_expected)
    );
    assert!(withdrawn_intent_lines(&lines, &attempt_id).is_empty());
    let intent_event = rig
        .ev
        .wait_event(Duration::from_secs(5), |event| {
            let changed = event["data"]["changed"].as_array();
            event["event"] == "messages.changed"
                && event["seq"] == intent_seq
                && changed.is_some_and(|areas| {
                    areas.len() == 2
                        && areas.iter().any(|area| area == "attention")
                        && areas.iter().any(|area| area == "notifications")
                })
        })
        .await;
    assert_eq!(intent_event["data"]["workspace_seq"], intent_seq);
    let journal = fs::read_to_string(workspace_journal(&rig)).unwrap();
    assert_eq!(journal.matches("Crash boundary secret").count(), 1);
    assert_eq!(journal.matches("Crash boundary body").count(), 1);

    request.abort();
    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    let repeated = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    assert_eq!(repeated["error"]["code"], "conflict", "{repeated}");
    let replayed = workspace_lines(&rig);
    assert_eq!(intent_lines(&replayed, &attempt_id).len(), 1);
    assert_eq!(
        resolution_lines(&replayed, &attempt_id).len(),
        usize::from(final_fact_expected)
    );
    assert!(withdrawn_intent_lines(&replayed, &attempt_id).is_empty());
    if final_fact_expected {
        assert_snapshot_resolved(&mut rig, &message_id, NotificationResolution::Complete).await;
    } else {
        let snapshot = rig
            .ctl
            .request("messages.snapshot", json!({"recent_settled": 20}))
            .await;
        let row = snapshot["result"]["rows"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["message_id"] == message_id))
            .unwrap_or_else(|| panic!("message missing after replay: {snapshot}"));
        assert_eq!(
            row["recipients"][0]["notification"]["resolution_intent"],
            "complete"
        );
        assert!(row["recipients"][0]["notification"]
            .get("resolution")
            .is_none());
        assert_eq!(row["recipients"][0]["needs_action"], true);
    }
    drop(hold);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_intent_is_ambiguous_and_non_repeatable() {
    exercise_resolution_crash_window("attention_after_intent", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_terminal_key_is_ambiguous_and_non_repeatable() {
    exercise_resolution_crash_window("attention_after_key", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_final_fact_replays_the_resolution() {
    exercise_resolution_crash_window("attention_after_resolution", true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn process_replacement_after_intent_refuses_the_terminal_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "attention-process-replaced",
        &manifest,
        &recovery_composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Replace owner",
                "body": "Private replacement body",
                "client_key": "replace-owner"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let paused = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let paused = Arc::clone(&paused);
        Box::pin(async move {
            if current != "attention_after_intent" {
                return;
            }
            let _ = entered_tx.send(());
            paused.acquire_owned().await.unwrap().forget();
        })
    });
    let socket = rig.daemon.socket_path();
    let requested_attempt = attempt_id.clone();
    let request = tokio::spawn(async move {
        let mut client = common::TestClient::connect(&socket).await;
        client
            .request("attention.complete", json!({"id": requested_attempt}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("resolution reached intent boundary")
        .expect("pause sender stayed open");

    let replacement = recovery_composer_pane();
    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &replacement]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let shown = rig
            .ctl
            .request("attention.show", json!({"id": attempt_id, "diff": false}))
            .await;
        if shown["result"]["checks"]["process_matches"] == false {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon kept the replaced process binding"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    hold.add_permits(1);
    let response = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("resolution answered")
        .expect("request task joined");
    assert_eq!(response["error"]["code"], "attention_evidence_failed");
    assert!(!rig
        .tmux
        .capture(&pane)
        .contains(&cyclops_proto::render_doorbell_v1(
            &MessageId::new(&message_id).unwrap()
        )));
    let lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&lines, &attempt_id).len(), 1);
    assert!(resolution_lines(&lines, &attempt_id).is_empty());
    let withdrawn = withdrawn_intent_lines(&lines, &attempt_id);
    assert_eq!(withdrawn.len(), 1);
    let withdrawn_seq = withdrawn[0].seq;
    let withdrawal_event = rig
        .ev
        .wait_event(Duration::from_secs(5), |event| {
            let changed = event["data"]["changed"].as_array();
            event["event"] == "messages.changed"
                && event["seq"] == withdrawn_seq
                && changed.is_some_and(|areas| {
                    areas.len() == 2
                        && areas.iter().any(|area| area == "attention")
                        && areas.iter().any(|area| area == "notifications")
                })
        })
        .await;
    assert_eq!(withdrawal_event["data"]["workspace_seq"], withdrawn_seq);
    let repeated = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    assert_eq!(repeated["error"]["code"], "attention_evidence_failed");
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_blocked_capture_refuses_the_terminal_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "attention-fresh-blocked",
        &manifest,
        &recovery_composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Fresh blocked state",
                "body": "Private blocked body",
                "client_key": "fresh-blocked"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let paused = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let paused = Arc::clone(&paused);
        Box::pin(async move {
            if current != "attention_after_intent" {
                return;
            }
            let _ = entered_tx.send(());
            paused.acquire_owned().await.unwrap().forget();
        })
    });
    let socket = rig.daemon.socket_path();
    let requested_attempt = attempt_id.clone();
    let request = tokio::spawn(async move {
        let mut client = common::TestClient::connect(&socket).await;
        client
            .request("attention.complete", json!({"id": requested_attempt}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("resolution reached intent boundary")
        .expect("pause sender stayed open");

    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "BLOCKED"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let shown = rig
            .ctl
            .request("attention.show", json!({"id": attempt_id, "diff": false}))
            .await;
        if shown["result"]["checks"]["terminal_action_safe"] == false
            && shown["result"]["checks"]["notification_exact"] == true
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "blocked title was not classified: {shown}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    hold.add_permits(1);
    let response = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("resolution answered")
        .expect("request task joined");
    assert_eq!(response["error"]["code"], "attention_evidence_failed");
    assert_eq!(
        response["error"]["data"]["checks"]["terminal_action_safe"],
        false
    );
    let lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&lines, &attempt_id).len(), 1);
    assert!(resolution_lines(&lines, &attempt_id).is_empty());
    assert_eq!(withdrawn_intent_lines(&lines, &attempt_id).len(), 1);
    rig.shutdown().await;
}
