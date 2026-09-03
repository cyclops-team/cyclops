//! Explicit recovery of a staged mailbox notification through daemon dispatch.

mod common;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{faketui_path, tmux_available, Rig, CAT_MANIFEST};
use cyclops_proto::{
    Kind, LedgerLine, MsgSendParams, NotificationResolution, NOTIFICATION_RESOLUTION_PROOF_VERSION,
};
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
    let deadline = Instant::now() + Duration::from_secs(30);
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

async fn wait_for_resolution(rig: &Rig, attempt_id: &str, resolution: NotificationResolution) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let lines = workspace_lines(rig);
        if resolution_lines(&lines, attempt_id).iter().any(|line| {
            line.data
                .as_ref()
                .is_some_and(|data| data["resolution"] == serde_json::to_value(resolution).unwrap())
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "automatic {resolution:?} did not settle for {attempt_id}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_inject_signal(
    entered: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    phase: &'static str,
) {
    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .unwrap_or_else(|_| panic!("inject phase {phase} was not reached"))
        .expect("inject phase sender stayed open");
}

async fn assert_alarm_cause(rig: &mut Rig, attempt_id: &str, cause: &str) {
    let response = rig
        .ctl
        .request("alarm.preview", json!({"older_than_ms": 0}))
        .await;
    let entry = response["result"]["entries"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["id"] == attempt_id))
        .unwrap_or_else(|| panic!("alarm disappeared before cause check: {response}"));
    assert_eq!(entry["cause"], cause, "{response}");
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

fn no_key_resolution_lines<'a>(lines: &'a [LedgerLine], attempt_id: &str) -> Vec<&'a LedgerLine> {
    lines
        .iter()
        .filter(|line| {
            line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_resolved_without_terminal_action"
                    && data["attempt_id"] == attempt_id
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

fn accepted_action_lines<'a>(lines: &'a [LedgerLine], attempt_id: &str) -> Vec<&'a LedgerLine> {
    lines
        .iter()
        .filter(|line| {
            line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_resolution_action_accepted"
                    && data["attempt_id"] == attempt_id
            })
        })
        .collect()
}

fn consumption_lines<'a>(lines: &'a [LedgerLine], attempt_id: &str) -> Vec<&'a LedgerLine> {
    lines
        .iter()
        .filter(|line| {
            line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_resolution_consumption_observed"
                    && data["attempt_id"] == attempt_id
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
    assert_eq!(data["proof_version"], NOTIFICATION_RESOLUTION_PROOF_VERSION);
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
            "proof_version",
            "recipient",
            "record_version",
            "resolution",
            "type",
        ]
    );
}

fn assert_content_free_action_accepted(line: &LedgerLine, resolution: NotificationResolution) {
    assert_eq!(line.kind, Kind::State);
    assert!(line.subject.is_none());
    assert!(line.body.is_none());
    let data = line.data.as_ref().expect("action-accepted data");
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

fn assert_content_free_consumption(line: &LedgerLine) {
    assert_eq!(line.kind, Kind::State);
    assert!(line.subject.is_none());
    assert!(line.body.is_none());
    let data = line.data.as_ref().expect("consumption data");
    assert!(matches!(
        data["evidence"].as_str(),
        Some("exact_hook_prompt" | "authenticated_claim")
    ));
    assert!(data["observed_at_ms"]
        .as_u64()
        .is_some_and(|observed| observed > 0));
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
            "evidence",
            "message_id",
            "observed_at_ms",
            "recipient",
            "record_version",
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
    let doorbell = cyclops_proto::render_doorbell_v3(
        cyclops_proto::NotificationAttemptId::parse(&attempt_id).unwrap(),
    );
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
            json!(cyclops_proto::DOORBELL_FORMAT_ATTEMPT_ONLY_CLAIM)
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
    let resolved = if resolution == NotificationResolution::Complete {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let hold = Arc::new(tokio::sync::Semaphore::new(0));
        let paused = Arc::clone(&hold);
        rig.daemon.set_inject_pause(move |current| {
            let entered_tx = entered_tx.clone();
            let paused = Arc::clone(&paused);
            Box::pin(async move {
                if current != "attention_after_action_accepted" {
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
            .expect("Complete reached its accepted boundary")
            .expect("pause sender stayed open");
        rig.daemon
            .claim_message_for_test("worker", &message_id)
            .expect("exact recipient claim");
        hold.add_permits(1);
        request.await.unwrap()
    } else {
        rig.ctl.request(method, json!({"id": attempt_id})).await
    };
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
    let accepted = accepted_action_lines(&lines, &attempt_id);
    assert_eq!(accepted.len(), 1, "action-accepted fact count changed");
    let consumptions = consumption_lines(&lines, &attempt_id);
    assert_eq!(
        consumptions.len(),
        usize::from(resolution == NotificationResolution::Complete),
        "consumption fact count changed"
    );
    assert!(withdrawn_intent_lines(&lines, &attempt_id).is_empty());
    match resolution {
        NotificationResolution::Complete => {
            assert!(
                intents[0].seq < accepted[0].seq
                    && accepted[0].seq < consumptions[0].seq
                    && consumptions[0].seq < facts[0].seq,
                "Complete boundaries are out of order"
            );
            assert_content_free_consumption(consumptions[0]);
        }
        NotificationResolution::Discard => assert!(
            intents[0].seq < accepted[0].seq && accepted[0].seq < facts[0].seq,
            "Discard boundaries are out of order"
        ),
    }
    assert_content_free_action_accepted(accepted[0], resolution);
    assert_content_free_resolution(facts[0], resolution);
    assert!(intents[0].subject.is_none());
    assert!(intents[0].body.is_none());
    let after = fs::read_to_string(workspace_journal(&rig)).unwrap();
    let appended = after
        .strip_prefix(&before)
        .expect("resolution only appends to the journal");
    assert_eq!(
        appended.lines().count(),
        if resolution == NotificationResolution::Complete {
            5
        } else {
            3
        }
    );
    assert!(!appended.contains("private body"));
    assert!(!appended.contains("\"expected\":"));
    assert!(!appended.contains("\"observed\":"));

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
    assert_eq!(
        accepted_action_lines(&workspace_lines(&rig), &attempt_id).len(),
        1
    );

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
    assert_eq!(
        accepted_action_lines(&workspace_lines(&rig), &attempt_id).len(),
        1
    );
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
async fn pending_exact_owned_doorbell_submits_once_without_operator_input() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let log_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "attention-automatic-complete-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&log_dir).unwrap();
    let submit_log = log_dir.join("submits.txt");
    let pane_command = format!(
        "python3 {} --clear-staged --submit-log {}",
        faketui_path(),
        submit_log.display()
    );
    let mut rig = Rig::new(
        "attention-automatic-complete",
        &recovery_manifest(),
        &pane_command,
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (pre_submit_tx, mut pre_submit_rx) = tokio::sync::mpsc::unbounded_channel();
    let (automatic_tx, mut automatic_rx) = tokio::sync::mpsc::unbounded_channel();
    let (after_action_tx, mut after_action_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_submit = Arc::new(tokio::sync::Semaphore::new(0));
    let automatic = Arc::new(tokio::sync::Semaphore::new(0));
    let after_action = Arc::new(tokio::sync::Semaphore::new(0));
    let paused_pre_submit = Arc::clone(&pre_submit);
    let paused_automatic = Arc::clone(&automatic);
    let paused_after_action = Arc::clone(&after_action);
    rig.daemon.set_inject_pause(move |phase| {
        let pause = match phase {
            "pre_submit" => Some((pre_submit_tx.clone(), Arc::clone(&paused_pre_submit))),
            "automatic_attention_before_resolve" => {
                Some((automatic_tx.clone(), Arc::clone(&paused_automatic)))
            }
            "attention_after_action_accepted" => {
                Some((after_action_tx.clone(), Arc::clone(&paused_after_action)))
            }
            _ => None,
        };
        Box::pin(async move {
            if let Some((entered, paused)) = pause {
                let _ = entered.send(());
                paused.acquire_owned().await.unwrap().forget();
            }
        })
    });

    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Automatic exact submit",
                "body": "private body",
                "client_key": "automatic-exact-submit"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_inject_signal(&mut pre_submit_rx, "pre_submit").await;
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "BLOCKED"]);
    common::wait_pane_state(&mut rig, "blocked_modal").await;
    pre_submit.add_permits(1);

    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    assert_alarm_cause(&mut rig, &attempt_id, "verify_failed").await;
    assert!(!submit_log.exists(), "failed delivery sent a submit key");
    wait_for_inject_signal(&mut automatic_rx, "automatic_attention_before_resolve").await;
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "worker"]);
    common::wait_pane_state(&mut rig, "unknown").await;

    automatic.add_permits(32);
    wait_for_inject_signal(&mut after_action_rx, "attention_after_action_accepted").await;
    rig.daemon
        .claim_message_for_test("worker", &message_id)
        .expect("claim after automatic action acceptance");
    after_action.add_permits(32);

    wait_for_resolution(&rig, &attempt_id, NotificationResolution::Complete).await;
    wait_for_empty_composer(&rig, &pane).await;
    let lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(accepted_action_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(consumption_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(resolution_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);

    rig.shutdown().await;
    fs::remove_dir_all(log_dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn working_pre_submit_recheck_submits_the_exact_doorbell_after_fresh_quiet_evidence() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let log_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "attention-working-recovery-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&log_dir).unwrap();
    let submit_log = log_dir.join("submits.txt");
    let pane_command = format!(
        "python3 {} --clear-staged --submit-log {}",
        faketui_path(),
        submit_log.display()
    );
    let mut rig = Rig::new(
        "attention-working-recovery",
        &recovery_manifest(),
        &pane_command,
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (pre_submit_tx, mut pre_submit_rx) = tokio::sync::mpsc::unbounded_channel();
    let (automatic_tx, mut automatic_rx) = tokio::sync::mpsc::unbounded_channel();
    let (after_action_tx, mut after_action_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_submit = Arc::new(tokio::sync::Semaphore::new(0));
    let automatic = Arc::new(tokio::sync::Semaphore::new(0));
    let after_action = Arc::new(tokio::sync::Semaphore::new(0));
    let paused_pre_submit = Arc::clone(&pre_submit);
    let paused_automatic = Arc::clone(&automatic);
    let paused_after_action = Arc::clone(&after_action);
    rig.daemon.set_inject_pause(move |phase| {
        let pause = match phase {
            "pre_submit" => Some((pre_submit_tx.clone(), Arc::clone(&paused_pre_submit))),
            "automatic_attention_before_resolve" => {
                Some((automatic_tx.clone(), Arc::clone(&paused_automatic)))
            }
            "attention_after_action_accepted" => {
                Some((after_action_tx.clone(), Arc::clone(&paused_after_action)))
            }
            _ => None,
        };
        Box::pin(async move {
            if let Some((entered, paused)) = pause {
                let _ = entered.send(());
                paused.acquire_owned().await.unwrap().forget();
            }
        })
    });

    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Working composer recovery",
                "body": "private body",
                "client_key": "working-composer-recovery"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_inject_signal(&mut pre_submit_rx, "pre_submit").await;

    // The recipient begins a turn after the doorbell has been staged but
    // before Cyclops may send Enter. The final proof must withhold that key.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-t"]);
    common::wait_pane_state(&mut rig, "working").await;
    pre_submit.add_permits(1);

    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    assert_alarm_cause(&mut rig, &attempt_id, "verify_failed").await;
    assert!(!submit_log.exists(), "working recheck sent a submit key");
    let expected_doorbell = cyclops_proto::render_doorbell_v3(
        cyclops_proto::NotificationAttemptId::parse(&attempt_id).unwrap(),
    );
    assert!(
        rig.tmux.capture(&pane).contains(&expected_doorbell),
        "working recheck changed the exact staged doorbell"
    );
    let lines = workspace_lines(&rig);
    assert!(intent_lines(&lines, &attempt_id).is_empty());
    assert!(accepted_action_lines(&lines, &attempt_id).is_empty());
    assert!(consumption_lines(&lines, &attempt_id).is_empty());
    assert!(resolution_lines(&lines, &attempt_id).is_empty());

    // A fresh screen/composer observation restores the same exact doorbell
    // without a running turn. It may re-elect normal reconciliation; no
    // timer, poll, or force-submit path participates.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-y"]);
    common::wait_pane_state(&mut rig, "unknown").await;
    wait_for_inject_signal(&mut automatic_rx, "automatic_attention_before_resolve").await;
    automatic.add_permits(32);
    wait_for_inject_signal(&mut after_action_rx, "attention_after_action_accepted").await;
    rig.daemon
        .claim_message_for_test("worker", &message_id)
        .expect("claim after automatic action acceptance");
    after_action.add_permits(32);

    wait_for_resolution(&rig, &attempt_id, NotificationResolution::Complete).await;
    wait_for_empty_composer(&rig, &pane).await;
    let lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(accepted_action_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(consumption_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(resolution_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);

    rig.shutdown().await;
    fs::remove_dir_all(log_dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn claimed_exact_owned_doorbell_clears_without_another_submit() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let log_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "attention-automatic-discard-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&log_dir).unwrap();
    let submit_log = log_dir.join("submits.txt");
    let pane_command = format!(
        "python3 {} --clear-staged --submit-log {}",
        faketui_path(),
        submit_log.display()
    );
    let mut rig = Rig::new(
        "attention-automatic-discard",
        &recovery_manifest(),
        &pane_command,
        "receipt_block_ms = 100\nack_timeout_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (pre_submit_tx, mut pre_submit_rx) = tokio::sync::mpsc::unbounded_channel();
    let (automatic_tx, mut automatic_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_submit = Arc::new(tokio::sync::Semaphore::new(0));
    let automatic = Arc::new(tokio::sync::Semaphore::new(0));
    let paused_pre_submit = Arc::clone(&pre_submit);
    let paused_automatic = Arc::clone(&automatic);
    rig.daemon.set_inject_pause(move |phase| {
        let pause = match phase {
            "pre_submit" => Some((pre_submit_tx.clone(), Arc::clone(&paused_pre_submit))),
            "automatic_attention_before_resolve" => {
                Some((automatic_tx.clone(), Arc::clone(&paused_automatic)))
            }
            _ => None,
        };
        Box::pin(async move {
            if let Some((entered, paused)) = pause {
                let _ = entered.send(());
                paused.acquire_owned().await.unwrap().forget();
            }
        })
    });

    let sent = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value::<MsgSendParams>(json!({
                "to": ["worker"],
                "subject": "Automatic exact clear",
                "body": "private body",
                "client_key": "automatic-exact-clear"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_inject_signal(&mut pre_submit_rx, "pre_submit").await;
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "BLOCKED"]);
    common::wait_pane_state(&mut rig, "blocked_modal").await;
    pre_submit.add_permits(1);

    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    assert_alarm_cause(&mut rig, &attempt_id, "verify_failed").await;
    assert!(!submit_log.exists(), "failed delivery sent a submit key");
    wait_for_inject_signal(&mut automatic_rx, "automatic_attention_before_resolve").await;
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "worker"]);
    common::wait_pane_state(&mut rig, "unknown").await;
    rig.daemon
        .claim_message_for_test("worker", &message_id)
        .expect("claim before automatic resolution");
    automatic.add_permits(32);

    wait_for_resolution(&rig, &attempt_id, NotificationResolution::Discard).await;
    wait_for_empty_composer(&rig, &pane).await;
    let lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&lines, &attempt_id).len(), 1);
    assert_eq!(accepted_action_lines(&lines, &attempt_id).len(), 1);
    assert!(consumption_lines(&lines, &attempt_id).is_empty());
    assert_eq!(resolution_lines(&lines, &attempt_id).len(), 1);
    assert!(!submit_log.exists(), "claimed recovery sent a submit key");

    rig.shutdown().await;
    fs::remove_dir_all(log_dir).unwrap();
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
    let doorbell = cyclops_proto::render_doorbell_v3(
        cyclops_proto::NotificationAttemptId::parse(&attempt_id).unwrap(),
    );
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
    let doorbell = cyclops_proto::render_doorbell_v3(
        cyclops_proto::NotificationAttemptId::parse(&attempt_id).unwrap(),
    );

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
    let doorbell = cyclops_proto::render_doorbell_v3(
        cyclops_proto::NotificationAttemptId::parse(&attempt_id).unwrap(),
    );
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

async fn exercise_resolution_crash_window(
    phase: &'static str,
    accepted_fact_before_crash: bool,
    consumption_fact_before_crash: bool,
    resolved_before_crash: bool,
    resolves_on_retry: bool,
) {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let manifest = recovery_manifest();
    let log_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "attention-crash-submit-log-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&log_dir).unwrap();
    let submit_log = log_dir.join("submits.txt");
    let pane_command = format!(
        "python3 {} --swallow-once --clear-staged --submit-log {}",
        faketui_path(),
        submit_log.display()
    );
    let mut rig = Rig::new(
        phase,
        &manifest,
        &pane_command,
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
    if consumption_fact_before_crash {
        let deadline = Instant::now() + Duration::from_secs(5);
        while accepted_action_lines(&workspace_lines(&rig), &attempt_id).is_empty() {
            assert!(
                Instant::now() < deadline,
                "Complete did not reach action acceptance before claim"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        rig.daemon
            .claim_message_for_test("worker", &message_id)
            .expect("exact post-action recipient claim");
    }
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("resolution reached crash boundary")
        .expect("pause sender stayed open");

    let lines = workspace_lines(&rig);
    let intents = intent_lines(&lines, &attempt_id);
    assert_eq!(intents.len(), 1);
    let intent_seq = intents[0].seq;
    let accepted = accepted_action_lines(&lines, &attempt_id);
    assert_eq!(
        accepted.len(),
        usize::from(accepted_fact_before_crash),
        "durable action-accepted boundary changed at {phase}"
    );
    if let Some(accepted) = accepted.first() {
        assert!(intent_seq < accepted.seq);
        assert_content_free_action_accepted(accepted, NotificationResolution::Complete);
    }
    let consumptions = consumption_lines(&lines, &attempt_id);
    assert_eq!(
        consumptions.len(),
        usize::from(consumption_fact_before_crash),
        "durable consumption boundary changed at {phase}"
    );
    if let Some(consumption) = consumptions.first() {
        let accepted = accepted
            .first()
            .expect("consumption requires an action-accepted boundary");
        assert!(accepted.seq < consumption.seq);
        assert_content_free_consumption(consumption);
    }
    assert_eq!(
        resolution_lines(&lines, &attempt_id).len(),
        usize::from(resolved_before_crash)
    );
    if let Some(resolved) = resolution_lines(&lines, &attempt_id).first() {
        let consumption = consumptions
            .first()
            .expect("a Complete resolution requires a consumption boundary");
        assert!(consumption.seq < resolved.seq);
    }
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
    // tmux acknowledges send-keys before the pane process consumes the input.
    // Observe that first key before reboot so the later count proves no retry.
    let expected_keys_before_reboot = 1 + usize::from(phase != "attention_after_intent");
    let key_deadline = Instant::now() + Duration::from_secs(5);
    let keys_before_reboot = loop {
        let observed = fs::read_to_string(&submit_log).unwrap().lines().count();
        assert!(
            observed <= expected_keys_before_reboot,
            "unexpected terminal-key count at {phase}: {observed}"
        );
        if observed == expected_keys_before_reboot {
            break observed;
        }
        assert!(
            Instant::now() < key_deadline,
            "terminal fixture did not consume the issued key at {phase}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    request.abort();
    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    let repeated = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    if resolved_before_crash {
        assert_eq!(repeated["error"]["code"], "conflict", "{repeated}");
    } else if resolves_on_retry {
        assert!(repeated["error"].is_null(), "{repeated}");
        assert_eq!(repeated["result"]["resolution"], "complete");
    } else {
        assert_eq!(
            repeated["error"]["code"], "attention_action_uncertain",
            "{repeated}"
        );
    }
    let replayed = workspace_lines(&rig);
    assert_eq!(intent_lines(&replayed, &attempt_id).len(), 1);
    assert_eq!(
        accepted_action_lines(&replayed, &attempt_id).len(),
        usize::from(accepted_fact_before_crash)
    );
    assert_eq!(
        consumption_lines(&replayed, &attempt_id).len(),
        usize::from(consumption_fact_before_crash)
    );
    assert_eq!(
        resolution_lines(&replayed, &attempt_id).len(),
        usize::from(resolved_before_crash || resolves_on_retry)
    );
    assert_eq!(
        fs::read_to_string(&submit_log).unwrap().lines().count(),
        keys_before_reboot,
        "recovery sent a second terminal key"
    );
    assert!(withdrawn_intent_lines(&replayed, &attempt_id).is_empty());
    if resolved_before_crash || resolves_on_retry {
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
        assert_eq!(
            row["recipients"][0]["notification"]
                .get("resolution_action_accepted")
                .and_then(serde_json::Value::as_str),
            accepted_fact_before_crash.then_some("complete")
        );
        assert_eq!(
            row["recipients"][0]["notification"]
                .get("resolution_consumption_observed")
                .and_then(|value| value.get("evidence"))
                .and_then(serde_json::Value::as_str),
            consumption_fact_before_crash.then_some("authenticated_claim")
        );
        assert!(row["recipients"][0]["notification"]
            .get("resolution")
            .is_none());
        assert_eq!(row["recipients"][0]["needs_action"], true);
    }
    drop(hold);
    rig.shutdown().await;
    fs::remove_dir_all(log_dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_intent_stays_ambiguous_without_a_second_key() {
    exercise_resolution_crash_window("attention_after_intent", false, false, false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_terminal_key_before_acceptance_stays_ambiguous_without_a_second_key() {
    exercise_resolution_crash_window(
        "attention_after_key_before_accepted",
        false,
        false,
        false,
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_action_acceptance_without_consumption_stays_ambiguous() {
    exercise_resolution_crash_window("attention_after_action_accepted", true, false, false, false)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_consumption_reconciles_exact_clean_state_without_a_second_key() {
    exercise_resolution_crash_window(
        "attention_after_consumption_observed",
        true,
        true,
        false,
        true,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_final_fact_replays_the_resolution() {
    exercise_resolution_crash_window("attention_after_resolution", true, true, true, false).await;
}

async fn exercise_no_key_discard_crash(phase: &'static str, resolved_before_crash: bool) {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let log_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "attention-no-key-log-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&log_dir).unwrap();
    let submit_log = log_dir.join("submits.txt");
    let pane_command = format!(
        "python3 {} --swallow-once --clear-staged --submit-log {}",
        faketui_path(),
        submit_log.display()
    );
    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        phase,
        &manifest,
        &pane_command,
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
                "subject": "Atomic no-key Discard",
                "body": "Never journal these private bytes twice",
                "client_key": phase
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);

    // The operator independently cleared the exact staged notification. This
    // is the no-key Discard route: two fresh empty observations, then one
    // atomic resolution fact and no terminal action intent.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-c"]);
    wait_for_empty_composer(&rig, &pane).await;

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
            .request("attention.discard", json!({"id": requested_attempt}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("no-key Discard reached crash boundary")
        .expect("pause sender stayed open");

    let lines = workspace_lines(&rig);
    assert!(intent_lines(&lines, &attempt_id).is_empty());
    assert!(accepted_action_lines(&lines, &attempt_id).is_empty());
    assert!(consumption_lines(&lines, &attempt_id).is_empty());
    assert!(resolution_lines(&lines, &attempt_id).is_empty());
    assert_eq!(
        no_key_resolution_lines(&lines, &attempt_id).len(),
        usize::from(resolved_before_crash)
    );
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);

    request.abort();
    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    let repeated = rig
        .ctl
        .request("attention.discard", json!({"id": attempt_id}))
        .await;
    if resolved_before_crash {
        assert_eq!(repeated["error"]["code"], "conflict", "{repeated}");
    } else {
        assert!(repeated["error"].is_null(), "{repeated}");
        assert_eq!(repeated["result"]["resolution"], "discard");
    }
    let replayed = workspace_lines(&rig);
    assert!(intent_lines(&replayed, &attempt_id).is_empty());
    assert_eq!(no_key_resolution_lines(&replayed, &attempt_id).len(), 1);
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);
    assert_snapshot_resolved(&mut rig, &message_id, NotificationResolution::Discard).await;
    drop(hold);
    rig.shutdown().await;
    fs::remove_dir_all(log_dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_before_no_key_discard_fact_retries_without_a_terminal_key() {
    exercise_no_key_discard_crash("attention_before_no_key_resolution", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_no_key_discard_fact_replays_without_a_terminal_key() {
    exercise_no_key_discard_crash("attention_after_no_key_resolution", true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn swallowed_complete_stays_uncertain_and_reconciliation_sends_no_second_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let log_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "attention-submit-log-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&log_dir).unwrap();
    let submit_log = log_dir.join("submits.txt");
    let pane_command = format!(
        "python3 {} --swallow-submit --clear-staged --submit-log {}",
        faketui_path(),
        submit_log.display()
    );
    let manifest = recovery_manifest();
    let mut rig = Rig::new(
        "attention-complete-swallowed",
        &manifest,
        &pane_command,
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
                "subject": "Swallowed terminal key",
                "body": "Keep the exact staged bytes",
                "client_key": "attention-complete-swallowed"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let attempt_id = wait_for_alarm(&mut rig, &message_id).await;
    let expected = cyclops_proto::render_doorbell_v3(
        cyclops_proto::NotificationAttemptId::parse(&attempt_id).unwrap(),
    );
    assert!(rig.tmux.capture(&pane).contains(&expected));
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let paused = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |current| {
        let entered_tx = entered_tx.clone();
        let paused = Arc::clone(&paused);
        Box::pin(async move {
            if current != "attention_after_action_accepted" {
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
        .expect("swallowed Complete reached action acceptance")
        .expect("pause sender stayed open");

    // A separate turn-like screen edge on the same binding cannot identify
    // this message. It may wake reconciliation, but it must not become a
    // consumption fact.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-t"]);
    let working_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let working = status["result"]["sessions"][0]["panes"]
            .as_array()
            .and_then(|panes| panes.iter().find(|row| row["pane_id"] == pane))
            .is_some_and(|row| row["state"] == "working");
        if working {
            break;
        }
        assert!(
            Instant::now() < working_deadline,
            "unrelated Working edge was not observed: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-y"]);
    hold.add_permits(1);
    let first = request.await.unwrap();
    assert_eq!(
        first["error"]["code"], "attention_action_uncertain",
        "{first}"
    );
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 2);
    assert!(rig.tmux.capture(&pane).contains(&expected));

    let repeated = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    assert_eq!(
        repeated["error"]["code"], "attention_action_uncertain",
        "{repeated}"
    );
    assert_eq!(
        fs::read_to_string(&submit_log).unwrap().lines().count(),
        2,
        "reconcile-only must not send another terminal key"
    );
    assert!(rig.tmux.capture(&pane).contains(&expected));

    let lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&lines, &attempt_id).len(), 1);
    let accepted = accepted_action_lines(&lines, &attempt_id);
    assert_eq!(accepted.len(), 1);
    assert_content_free_action_accepted(accepted[0], NotificationResolution::Complete);
    assert!(consumption_lines(&lines, &attempt_id).is_empty());
    assert!(resolution_lines(&lines, &attempt_id).is_empty());

    // Clearing the composer independently is not evidence that the swallowed
    // Complete action started the attempt's turn. Exact empty pixels alone
    // must not settle a Complete action that produced neither an exact hook
    // prompt nor an exact recipient claim after action acceptance.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-c"]);
    wait_for_empty_composer(&rig, &pane).await;
    let after_independent_clear = rig
        .ctl
        .request("attention.complete", json!({"id": attempt_id}))
        .await;
    assert_eq!(
        after_independent_clear["error"]["code"], "attention_action_uncertain",
        "{after_independent_clear}"
    );
    assert_eq!(
        fs::read_to_string(&submit_log).unwrap().lines().count(),
        2,
        "independent clear recovery sent another terminal key"
    );
    let after_clear_lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&after_clear_lines, &attempt_id).len(), 1);
    assert_eq!(
        accepted_action_lines(&after_clear_lines, &attempt_id).len(),
        1
    );
    assert!(consumption_lines(&after_clear_lines, &attempt_id).is_empty());
    assert!(resolution_lines(&after_clear_lines, &attempt_id).is_empty());
    rig.shutdown().await;
    fs::remove_dir_all(log_dir).unwrap();
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
        .contains(&cyclops_proto::render_doorbell_v3(
            cyclops_proto::NotificationAttemptId::parse(&attempt_id).unwrap(),
        )));
    let lines = workspace_lines(&rig);
    assert_eq!(intent_lines(&lines, &attempt_id).len(), 1);
    assert!(accepted_action_lines(&lines, &attempt_id).is_empty());
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
    assert!(accepted_action_lines(&lines, &attempt_id).is_empty());
    assert!(resolution_lines(&lines, &attempt_id).is_empty());
    assert_eq!(withdrawn_intent_lines(&lines, &attempt_id).len(), 1);
    rig.shutdown().await;
}
