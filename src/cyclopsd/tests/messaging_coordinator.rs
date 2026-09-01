mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    composer_pane, faketui_path, hold_script, swallowing_animated_composer_pane, tmux_available,
    wait_pane_state, HomeGuard, Rig, TestClient, CAT_MANIFEST, HOOK_MANIFEST, LIVENESS_MANIFEST,
    MODAL_MANIFEST, QUOTA_MANIFEST,
};
use cyclops_proto::{
    ComposerHold, Kind, LedgerLine, MessageId, MsgSendParams, NotificationAttemptId,
    NotificationState, RecipientKey,
};
use serde_json::{json, Value};
use tokio::net::UnixDatagram;

/// The escaped Codex composer distinction from the live `/model` incident:
/// dim text is a ghost suggestion, while bare text is a human draft. The
/// fixture deliberately returns to a ghost without emitting a turn, which is
/// enough to prove this must become a durable pre-write block rather than an
/// in-memory wait.
const ESC_COMPOSER_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Escaped composer fixture"
process_names = ["python3", "python", "Python", "cat", "sh", "bash", "dash"]

[[rule]]
id = "title_working"
state = "working"
priority = 1200
region = "pane_title"
regex = ['^WORKING']

[[rule]]
id = "composer_typed_input"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+[^\x1b\s]']

[[rule]]
id = "composer_ghost_suggestion"
state = "idle"
composer_semantic = "ghost_suggestion"
priority = 1040
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+\x1b\[2m']

[[rule]]
id = "composer_empty_or_ghost"
state = "idle"
composer_semantic = "ambiguous"
priority = 1000
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*›']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
"#;

fn workspace_lines(rig: &Rig) -> Vec<LedgerLine> {
    let workspace = fs::read_dir(rig.home.join("workspaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = fs::read_to_string(workspace.join("messages.ndjson")).unwrap();
    // Ignore only a final line that a concurrent writer has not terminated yet.
    let complete = if raw.ends_with('\n') {
        raw.as_str()
    } else {
        raw.rsplit_once('\n').map_or("", |(lines, _)| lines)
    };
    complete
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn notification_attempts(rig: &Rig, message_id: &str) -> BTreeSet<String> {
    workspace_lines(rig)
        .into_iter()
        .filter_map(|line| {
            let data = line.data?;
            (line.id == message_id && data["type"] == "notification_transition")
                .then(|| data["attempt_id"].as_str().unwrap().to_string())
        })
        .collect()
}

fn notification_transition(
    rig: &Rig,
    message_id: &str,
    state: NotificationState,
) -> Option<LedgerLine> {
    workspace_lines(rig).into_iter().find(|line| {
        line.id == message_id
            && line.data.as_ref().is_some_and(|data| {
                data["type"] == "notification_transition"
                    && serde_json::from_value::<NotificationState>(data["state"].clone())
                        .is_ok_and(|actual| actual == state)
            })
    })
}

fn notification_state_count(rig: &Rig, message_id: &str, state: NotificationState) -> usize {
    workspace_lines(rig)
        .into_iter()
        .filter(|line| {
            line.id == message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition"
                        && serde_json::from_value::<NotificationState>(data["state"].clone())
                            .is_ok_and(|actual| actual == state)
                })
        })
        .count()
}

async fn wait_for_notification_state(rig: &mut Rig, message_id: &str, state: NotificationState) {
    // The screen receipt deadline itself is five seconds. Leave room for the
    // resulting journal append and event publication before declaring failure.
    let deadline = Instant::now() + Duration::from_secs(8);
    let transition = loop {
        if let Some(line) = notification_transition(rig, message_id, state) {
            break line;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} did not reach {state:?}: {:#?}",
            workspace_lines(rig)
                .into_iter()
                .filter(|line| line.id == message_id)
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    rig.ev
        .wait_event(Duration::from_secs(5), |event| {
            event["event"] == "messages.changed"
                && event["seq"] == transition.seq
                && event["data"]["changed"]
                    .as_array()
                    .is_some_and(|areas| areas.iter().any(|area| area == "notifications"))
        })
        .await;
}

async fn wait_for_workspace_fact(rig: &Rig, message_id: &str, fact_type: &str) -> LedgerLine {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(line) = workspace_lines(rig).into_iter().find(|line| {
            line.id == message_id
                && line
                    .data
                    .as_ref()
                    .is_some_and(|data| data["type"] == fact_type)
        }) {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "message {message_id} did not append {fact_type}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn send_workspace_message(rig: &Rig, client_key: &str, subject: &str, body: &str) -> Value {
    rig.daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": subject,
                "body": body,
                "client_key": client_key
            }))
            .unwrap(),
        )
        .await
        .unwrap()
}

async fn send_summarized_workspace_message(
    rig: &Rig,
    client_key: &str,
    subject: &str,
    summary: &str,
    body: &str,
) -> Value {
    rig.daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": subject,
                "summary": summary,
                "body": body,
                "client_key": client_key
            }))
            .unwrap(),
        )
        .await
        .unwrap()
}

// Obsolete if a mailbox quota reset no longer requires a fresh pane
// observation followed by explicit operator requeue.
#[tokio::test(flavor = "multi_thread")]
async fn a_positive_quota_observation_commits_before_cancelled_chrome_repaint() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let script = "sh -c 'echo \"Individual quota reached. Please upgrade your subscription.\"; read x; printf \"\\033[2J\\033[H\"; exec cat'";
    let mut rig = Rig::new(
        "workspace-quota-reset-observation",
        QUOTA_MANIFEST,
        script,
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "blocked_quota").await;

    let sent = send_workspace_message(
        &rig,
        "quota-reset-observation",
        "Held during quota",
        "body stays durable",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let held_notice = rig
        .ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "admin-notify"
                && event["data"]["level"] == "urgent"
                && event["data"]["id"] == message_id.as_str()
        })
        .await;
    assert!(held_notice["data"]["body"]
        .as_str()
        .is_some_and(|body| body.contains("will not resume automatically")));
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::QuotaHeld),
        1
    );
    let attempts_before = notification_attempts(&rig, &message_id);

    let repaint = rig.daemon.pause_next_chrome_repaint_for_test();
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    tokio::time::timeout(Duration::from_secs(8), repaint.wait_until_entered())
        .await
        .expect("quota reset did not reach the post-commit chrome boundary");
    let reset_notice = rig
        .ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "admin-notify"
                && event["data"]["level"] == "action_required"
                && event["data"]["id"] == message_id.as_str()
        })
        .await;
    assert_eq!(
        reset_notice["data"]["subject"],
        "quota reset observed for worker"
    );
    assert!(reset_notice["data"]["body"]
        .as_str()
        .is_some_and(|body| body.contains(&format!("cyclops requeue {message_id}"))));
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::QuotaResetObserved),
        1
    );
    assert_eq!(notification_attempts(&rig, &message_id), attempts_before);
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Writing),
        0,
        "positive reset observation must not write or requeue"
    );
    let screen = rig.tmux.capture(&pane);
    assert!(!screen.contains(&compact_doorbell(&rig, &message_id)));
    assert!(!screen.contains("body stays durable"));

    // Leave the repaint stalled. Shutdown cancels the owning daemon task,
    // proving presentation cancellation cannot consume the already-committed
    // reset edge.
    rig.daemon.shutdown().await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::QuotaResetObserved),
        1
    );
}

struct WaitingPair {
    first: String,
    second: String,
    first_receipt: Value,
    second_receipt: Value,
}

async fn send_waiting_pair(rig: &Rig, key_prefix: &str) -> WaitingPair {
    let first =
        send_workspace_message(rig, &format!("{key_prefix}-first"), "First", "first body").await;
    let second = send_workspace_message(
        rig,
        &format!("{key_prefix}-second"),
        "Second",
        "second body",
    )
    .await;
    assert_eq!(second["deliveries"][0]["notification_state"], "not_started");
    WaitingPair {
        first: first["msg_id"].as_str().unwrap().to_string(),
        second: second["msg_id"].as_str().unwrap().to_string(),
        first_receipt: first["deliveries"][0].clone(),
        second_receipt: second["deliveries"][0].clone(),
    }
}

fn assert_only_oldest_attempt_exists(rig: &Rig, pair: &WaitingPair) {
    assert_eq!(notification_attempts(rig, &pair.first).len(), 1);
    assert!(notification_attempts(rig, &pair.second).is_empty());
}

async fn wait_for_doorbell(rig: &Rig, pane: &str, message_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let screen = rig.tmux.capture(pane);
        if let Some(expected) = current_compact_doorbell(rig, message_id) {
            if screen.contains(&expected) {
                return screen;
            }
        }
        assert!(
            Instant::now() < deadline,
            "doorbell was not shown: {screen}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `status` synchronously refreshes the live pane. This proves the ordinary
/// post-paste observation still recognizes the exact doorbell while the old
/// turn is Working, including the `StagedDuringTurn` composer barrier that
/// prompted the final-Enter regression.
async fn refresh_exact_working_doorbell(rig: &mut Rig, pane: &str) {
    let status = rig.ctl.request("status", json!({})).await;
    assert!(status["error"].is_null(), "{status}");
    let pane_status = status["result"]["sessions"][0]["panes"]
        .as_array()
        .and_then(|panes| {
            panes
                .iter()
                .find(|row| row["pane_id"].as_str() == Some(pane))
        })
        .expect("status contains the refreshed working pane");
    assert_eq!(pane_status["state"], "working", "{status}");
    assert_eq!(
        pane_status["composer"], "cyclops_notification_staged",
        "{status}"
    );
    assert_eq!(
        pane_status["composer_proof"], "exact_notification",
        "{status}"
    );
    assert_eq!(pane_status["notification_state"], "staged", "{status}");
    assert_eq!(pane_status["next_action"], "automatic_submit", "{status}");
    assert_eq!(
        rig.daemon
            .composer_hold_for_test(0, pane)
            .map(|(hold, _)| hold),
        Some(ComposerHold::StagedDuringTurn),
        "the refreshed Working pane must retain the exact staged doorbell"
    );
}

fn current_compact_doorbell(rig: &Rig, message_id: &str) -> Option<String> {
    current_notification_attempt(&workspace_lines(rig), message_id)
        .map(cyclops_proto::render_doorbell_v3)
}

fn current_notification_attempt(
    lines: &[LedgerLine],
    message_id: &str,
) -> Option<NotificationAttemptId> {
    let mut current_by_recipient = BTreeMap::<RecipientKey, NotificationAttemptId>::new();
    for line in lines.iter().filter(|line| line.id == message_id) {
        let Some(data) = line.data.as_ref() else {
            continue;
        };
        match data["type"].as_str() {
            Some("notification_transition" | "notification_requeued") => {
                let Some(recipient) =
                    serde_json::from_value::<RecipientKey>(data["recipient"].clone()).ok()
                else {
                    continue;
                };
                let Some(attempt_id) = data["attempt_id"]
                    .as_str()
                    .and_then(|value| NotificationAttemptId::parse(value).ok())
                else {
                    continue;
                };
                current_by_recipient.insert(recipient, attempt_id);
            }
            Some("notifications_requeued") => {
                for requeue in data["requeues"].as_array().into_iter().flatten() {
                    let Some(recipient) =
                        serde_json::from_value::<RecipientKey>(requeue["recipient"].clone()).ok()
                    else {
                        continue;
                    };
                    let Some(attempt_id) = requeue["attempt_id"]
                        .as_str()
                        .and_then(|value| NotificationAttemptId::parse(value).ok())
                    else {
                        continue;
                    };
                    current_by_recipient.insert(recipient, attempt_id);
                }
            }
            _ => {}
        }
    }
    let mut attempts = current_by_recipient
        .into_values()
        .collect::<BTreeSet<_>>()
        .into_iter();
    let attempt_id = attempts.next()?;
    if attempts.next().is_some() {
        return None;
    }
    Some(attempt_id)
}

fn compact_doorbell(rig: &Rig, message_id: &str) -> String {
    current_compact_doorbell(rig, message_id)
        .unwrap_or_else(|| panic!("message {message_id} has no notification attempt"))
}

fn legacy_doorbell(message_id: &str) -> String {
    cyclops_proto::render_legacy_doorbell(&MessageId::new(message_id).unwrap())
}

#[test]
fn current_doorbell_follows_the_attempt_owned_by_a_requeue_fact() {
    fn line(seq: u64, message_id: &str, data: Value) -> LedgerLine {
        LedgerLine {
            seq,
            boot_id: "b-test".into(),
            id: message_id.into(),
            ts: seq,
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: Vec::new(),
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(data),
        }
    }

    let message_id = "m-requeued-doorbell";
    let prior = "att-00000000-0000-4000-8000-000000000001";
    let current = "att-00000000-0000-4000-8000-000000000002";
    let recipient: RecipientKey =
        "agent:00000000-0000-4000-8000-000000000003/00000000-0000-4000-8000-000000000004/%1"
            .parse()
            .unwrap();
    let lines = vec![
        line(
            1,
            message_id,
            json!({
                "type": "notification_transition",
                "attempt_id": prior,
                "recipient": recipient,
                "state": "attention_required"
            }),
        ),
        line(
            2,
            message_id,
            json!({
                "type": "notification_requeued",
                "prior_attempt_id": prior,
                "attempt_id": current,
                "recipient": recipient
            }),
        ),
    ];

    assert_eq!(
        current_notification_attempt(&lines, message_id),
        Some(NotificationAttemptId::parse(current).unwrap())
    );
}

fn pane_history(rig: &Rig, pane: &str) -> String {
    let output = rig.tmux.run(&["capture-pane", "-p", "-S", "-", "-t", pane]);
    assert!(
        output.status.success(),
        "capture pane history failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn joined_pane_history(rig: &Rig, pane: &str) -> String {
    let output = rig
        .tmux
        .run(&["capture-pane", "-p", "-J", "-S", "-", "-t", pane]);
    assert!(
        output.status.success(),
        "joined capture pane history failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

async fn wait_for_pane_mode(rig: &mut Rig, pane: &str, expected: bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let matches = status["result"]["sessions"][0]["panes"]
            .as_array()
            .expect("pane list")
            .iter()
            .any(|row| row["pane_id"] == pane && row["in_mode"] == expected);
        if matches {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane mode did not become {expected}: {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn resize_pane_and_allow_event(rig: &mut Rig, pane: &str, width: u32) {
    let width_arg = width.to_string();
    rig.tmux
        .run_ok(&["resize-window", "-t", pane, "-x", &width_arg]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let observed = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| session["panes"].as_array())
            .flatten()
            .any(|row| row["pane_id"] == pane && row["width"].as_u64() == Some(u64::from(width)));
        if observed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not observe pane {pane} at width {width}: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_human_composer_evidence(rig: &mut Rig, pane: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let held = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| session["panes"].as_array())
            .flatten()
            .any(|row| row["pane_id"] == pane && row["composer"] == "human_draft");
        if held {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane} did not record the human composer evidence: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn pane_pid(rig: &Rig, pane: &str) -> i64 {
    let output = rig
        .tmux
        .run(&["display-message", "-p", "-t", pane, "#{pane_pid}"]);
    assert!(
        output.status.success(),
        "pane pid lookup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .expect("numeric pane pid")
}

async fn wait_for_pane_pid_change(rig: &Rig, pane: &str, prior: i64) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = pane_pid(rig, pane);
        if current != prior {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane} did not acquire a new root process"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_pane_observation(rig: &mut Rig, pane: &str, title: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let observed = status["result"]["sessions"][0]["panes"]
            .as_array()
            .expect("pane list")
            .iter()
            .any(|row| row["pane_id"] == pane && row["title"] == title && row["manifest"] == "fix");
        if observed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not observe the auto-detectable replacement: {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn private_body_shapes_never_reach_the_notification_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-private-shapes",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let long_token = "private-long-token".repeat(1_024);
    let body = format!(
        "/approve --all\n```rust\nlet secret = \"not pane content\";\n```\nUnicode: 你好 🌍 café\n{long_token}"
    );
    let result = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Private transport shapes",
                "body": body,
                "client_key": "private-shapes"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = result["msg_id"].as_str().unwrap().to_string();

    wait_for_doorbell(&rig, &pane, &message_id).await;
    let screen = joined_pane_history(&rig, &pane);
    for private_text in [
        "Private transport shapes",
        "/approve --all",
        "let secret",
        "你好 🌍 café",
        "private-long-token",
    ] {
        assert!(
            !screen.contains(private_text),
            "private message text reached the pane: {private_text:?}\n{screen}"
        );
    }
    assert!(
        screen.contains(&compact_doorbell(&rig, &message_id)),
        "the fixed notification row was not staged: {screen}"
    );

    let message = workspace_lines(&rig)
        .into_iter()
        .find(|line| line.id == message_id && matches!(line.kind, Kind::Msg))
        .expect("durable message line");
    assert_eq!(message.body.as_deref(), Some(body.as_str()));
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Staged),
        1
    );
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        1
    );
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    assert_eq!(
        snapshot["result"]["rows"][0]["recipients"][0]["mailbox"]["status"],
        "pending"
    );
    assert!(workspace_lines(&rig).iter().all(|line| {
        line.id != message_id
            || line.data.as_ref().is_none_or(|data| {
                !matches!(
                    data["type"].as_str(),
                    Some("message_claimed" | "message_delivered_direct")
                )
            })
    }));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_doorbell_changed_before_submit_records_verify_attention() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-doorbell-pre-submit-edit",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent = send_workspace_message(
        &rig,
        "doorbell-pre-submit-edit",
        "Pre-submit edit",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Staged),
        1
    );
    wait_for_doorbell(&rig, &pane, &message_id).await;

    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    hold.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    let attention =
        notification_transition(&rig, &message_id, NotificationState::AttentionRequired)
            .expect("durable attention transition");
    assert_eq!(attention.data.unwrap()["cause"], "verify_failed");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        0
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn force_submit_sends_one_enter_for_the_exact_verify_failed_attempt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-once",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\nforce_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel();
    let (force_tx, mut force_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_release = Arc::new(tokio::sync::Semaphore::new(0));
    let force_release = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pre = Arc::new(AtomicBool::new(true));
    let first_force = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let pre_release = Arc::clone(&pre_release);
        let force_release = Arc::clone(&force_release);
        let first_pre = Arc::clone(&first_pre);
        let first_force = Arc::clone(&first_force);
        move |phase| {
            let pre_tx = pre_tx.clone();
            let force_tx = force_tx.clone();
            let pre_release = Arc::clone(&pre_release);
            let force_release = Arc::clone(&force_release);
            let pause_pre = phase == "pre_submit" && first_pre.swap(false, Ordering::SeqCst);
            let pause_force =
                phase == "force_submit_after_intent" && first_force.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if pause_pre {
                    let _ = pre_tx.send(());
                    pre_release.acquire_owned().await.unwrap().forget();
                } else if pause_force {
                    let _ = force_tx.send(());
                    force_release.acquire_owned().await.unwrap().forget();
                }
            })
        }
    });

    let sent =
        send_workspace_message(&rig, "force-submit-once", "Force submit", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), pre_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    pre_release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    tokio::time::timeout(Duration::from_secs(5), force_rx.recv())
        .await
        .expect("force-submit recorded intent")
        .expect("force-submit sender stayed open");

    // Replay and a live Settings update can both rescan while the first timer
    // owns the attempt. Durable intent must still elect one terminal key.
    rig.daemon.schedule_force_submit_candidates_for_test();
    rig.daemon.schedule_force_submit_candidates_for_test();
    tokio::time::sleep(Duration::from_millis(50)).await;
    force_release.add_permits(1);

    let intent = wait_for_workspace_fact(&rig, &message_id, "notification_resolution_intent").await;
    assert_eq!(intent.data.as_ref().unwrap()["forced"], true);
    wait_for_workspace_fact(&rig, &message_id, "notification_resolution_action_accepted").await;
    rig.tmux.wait_screen("main", "FAKETUI-WORKING");
    tokio::time::sleep(Duration::from_millis(250)).await;

    let lines = workspace_lines(&rig);
    for fact_type in [
        "notification_resolution_intent",
        "notification_resolution_action_accepted",
    ] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.id == message_id
                        && line
                            .data
                            .as_ref()
                            .is_some_and(|data| data["type"] == fact_type)
                })
                .count(),
            1,
            "the exact attempt appended {fact_type} more than once: {lines:#?}"
        );
    }
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        1,
        "the escape hatch pressed Enter more than once"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn force_submit_rearms_an_existing_exact_candidate_after_boot_routes_its_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-boot-recovery",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_release = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pre = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let pre_release = Arc::clone(&pre_release);
        let first_pre = Arc::clone(&first_pre);
        move |phase| {
            let pre_tx = pre_tx.clone();
            let pre_release = Arc::clone(&pre_release);
            let pause_pre = phase == "pre_submit" && first_pre.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if pause_pre {
                    let _ = pre_tx.send(());
                    pre_release.acquire_owned().await.unwrap().forget();
                }
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "force-submit-boot-recovery",
        "Force submit after boot",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), pre_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    pre_release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        0,
        "force-submit is off before the reboot"
    );

    rig.rewrite_config(
        "delivery_retry_max = 0\nforce_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 0\n",
    );
    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;

    let intent = wait_for_workspace_fact(&rig, &message_id, "notification_resolution_intent").await;
    assert_eq!(intent.data.as_ref().unwrap()["forced"], true);
    wait_for_workspace_fact(&rig, &message_id, "notification_resolution_action_accepted").await;
    rig.tmux.wait_screen("main", "FAKETUI-WORKING");

    let lines = workspace_lines(&rig);
    for fact_type in [
        "notification_resolution_intent",
        "notification_resolution_action_accepted",
    ] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.id == message_id
                        && line
                            .data
                            .as_ref()
                            .is_some_and(|data| data["type"] == fact_type)
                })
                .count(),
            1,
            "the recovered candidate appended {fact_type} more than once: {lines:#?}"
        );
    }
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        1,
        "the recovered candidate pressed Enter more than once"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn force_submit_waits_for_the_exact_reservation_release_before_revalidating() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-resolution-release",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reservation_tx, mut reservation_rx) = tokio::sync::mpsc::unbounded_channel();
    let (force_waiting_tx, mut force_waiting_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_release = Arc::new(tokio::sync::Semaphore::new(0));
    let reservation_release = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pre = Arc::new(AtomicBool::new(true));
    let first_reservation = Arc::new(AtomicBool::new(true));
    let first_force_wait = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let pre_release = Arc::clone(&pre_release);
        let reservation_release = Arc::clone(&reservation_release);
        let first_pre = Arc::clone(&first_pre);
        let first_reservation = Arc::clone(&first_reservation);
        let first_force_wait = Arc::clone(&first_force_wait);
        move |phase| {
            let pre_tx = pre_tx.clone();
            let reservation_tx = reservation_tx.clone();
            let force_waiting_tx = force_waiting_tx.clone();
            let pre_release = Arc::clone(&pre_release);
            let reservation_release = Arc::clone(&reservation_release);
            let pause_pre = phase == "pre_submit" && first_pre.swap(false, Ordering::SeqCst);
            let pause_reservation = phase == "attention_after_reservation"
                && first_reservation.swap(false, Ordering::SeqCst);
            let report_force_wait = phase == "force_submit_waiting_for_resolution_release"
                && first_force_wait.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if pause_pre {
                    let _ = pre_tx.send(());
                    pre_release.acquire_owned().await.unwrap().forget();
                } else if pause_reservation {
                    let _ = reservation_tx.send(());
                    reservation_release.acquire_owned().await.unwrap().forget();
                } else if report_force_wait {
                    let _ = force_waiting_tx.send(());
                }
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "force-submit-resolution-release",
        "Force submit waits for release",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), pre_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    pre_release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    tokio::time::timeout(Duration::from_secs(5), reservation_rx.recv())
        .await
        .expect("ordinary resolver acquired the exact reservation")
        .expect("ordinary resolver sender stayed open");

    let enabled = rig
        .ctl
        .request(
            "notification.force_submit.set",
            json!({"enabled": true, "delay_seconds": 0}),
        )
        .await;
    assert_eq!(enabled["result"]["enabled"], true, "{enabled}");
    tokio::time::timeout(Duration::from_secs(5), force_waiting_rx.recv())
        .await
        .expect("force-submit observed the conflicting reservation")
        .expect("force-submit wait sender stayed open");
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        0,
        "force-submit pressed Enter while another resolver still owned the attempt"
    );

    reservation_release.add_permits(1);
    let intent = wait_for_workspace_fact(&rig, &message_id, "notification_resolution_intent").await;
    assert_eq!(intent.data.as_ref().unwrap()["forced"], true);
    wait_for_workspace_fact(&rig, &message_id, "notification_resolution_action_accepted").await;
    rig.tmux.wait_screen("main", "FAKETUI-WORKING");

    let lines = workspace_lines(&rig);
    for fact_type in [
        "notification_resolution_intent",
        "notification_resolution_action_accepted",
    ] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.id == message_id
                        && line
                            .data
                            .as_ref()
                            .is_some_and(|data| data["type"] == fact_type)
                })
                .count(),
            1,
            "the exact release handoff appended {fact_type} more than once: {lines:#?}"
        );
    }
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        1,
        "the release handoff pressed Enter more than once"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn force_submit_waits_through_successive_reservations_until_one_forced_action_can_run() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-successive-reservations",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel();
    let (a_reserved_tx, mut a_reserved_rx) = tokio::sync::mpsc::unbounded_channel();
    let (b_reserved_tx, mut b_reserved_rx) = tokio::sync::mpsc::unbounded_channel();
    let (force_waiting_tx, mut force_waiting_rx) = tokio::sync::mpsc::unbounded_channel();
    let (after_a_release_tx, mut after_a_release_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_release = Arc::new(tokio::sync::Semaphore::new(0));
    let a_release = Arc::new(tokio::sync::Semaphore::new(0));
    let b_release = Arc::new(tokio::sync::Semaphore::new(0));
    let force_after_a_release = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pre = Arc::new(AtomicBool::new(true));
    let reservation_count = Arc::new(AtomicUsize::new(0));
    let first_after_a_release = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let pre_release = Arc::clone(&pre_release);
        let a_release = Arc::clone(&a_release);
        let b_release = Arc::clone(&b_release);
        let force_after_a_release = Arc::clone(&force_after_a_release);
        let first_pre = Arc::clone(&first_pre);
        let reservation_count = Arc::clone(&reservation_count);
        let first_after_a_release = Arc::clone(&first_after_a_release);
        move |phase| {
            let pre_tx = pre_tx.clone();
            let a_reserved_tx = a_reserved_tx.clone();
            let b_reserved_tx = b_reserved_tx.clone();
            let force_waiting_tx = force_waiting_tx.clone();
            let after_a_release_tx = after_a_release_tx.clone();
            let pre_release = Arc::clone(&pre_release);
            let a_release = Arc::clone(&a_release);
            let b_release = Arc::clone(&b_release);
            let force_after_a_release = Arc::clone(&force_after_a_release);
            let pause_pre = phase == "pre_submit" && first_pre.swap(false, Ordering::SeqCst);
            let reservation = (phase == "attention_after_reservation")
                .then(|| reservation_count.fetch_add(1, Ordering::SeqCst));
            let pause_after_a_release = phase == "force_submit_after_resolution_release"
                && first_after_a_release.swap(false, Ordering::SeqCst);
            let force_waiting = phase == "force_submit_waiting_for_resolution_release";
            Box::pin(async move {
                if pause_pre {
                    let _ = pre_tx.send(());
                    pre_release.acquire_owned().await.unwrap().forget();
                } else if let Some(0) = reservation {
                    let _ = a_reserved_tx.send(());
                    a_release.acquire_owned().await.unwrap().forget();
                } else if let Some(1) = reservation {
                    let _ = b_reserved_tx.send(());
                    b_release.acquire_owned().await.unwrap().forget();
                } else if force_waiting {
                    let _ = force_waiting_tx.send(());
                } else if pause_after_a_release {
                    let _ = after_a_release_tx.send(());
                    force_after_a_release
                        .acquire_owned()
                        .await
                        .unwrap()
                        .forget();
                }
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "force-submit-successive-reservations",
        "Force submit survives successor reservations",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), pre_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    pre_release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    let attempt_id = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("attention-required message has one current attempt");
    tokio::time::timeout(Duration::from_secs(5), a_reserved_rx.recv())
        .await
        .expect("automatic resolver A reserved the attempt")
        .expect("resolver A sender stayed open");

    let enabled = rig
        .ctl
        .request(
            "notification.force_submit.set",
            json!({"enabled": true, "delay_seconds": 0}),
        )
        .await;
    assert_eq!(enabled["result"]["enabled"], true, "{enabled}");
    tokio::time::timeout(Duration::from_secs(5), force_waiting_rx.recv())
        .await
        .expect("force-submit waited for resolver A")
        .expect("force-submit wait sender stayed open");

    // Hold force-submit after A releases, then let explicit resolver B acquire
    // the same exact reservation. This reproduces the successor race without
    // guessing at task scheduling.
    a_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), after_a_release_rx.recv())
        .await
        .expect("force-submit received resolver A's release")
        .expect("force-submit release sender stayed open");
    let socket = rig.daemon.socket_path();
    let requested_attempt = attempt_id.to_string();
    let resolver_b = tokio::spawn(async move {
        let mut client = TestClient::connect(&socket).await;
        client
            .request("attention.complete", json!({"id": requested_attempt}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), b_reserved_rx.recv())
        .await
        .expect("explicit resolver B reserved after A")
        .expect("resolver B sender stayed open");

    force_after_a_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), force_waiting_rx.recv())
        .await
        .expect("force-submit waited for resolver B instead of exiting")
        .expect("force-submit second wait sender stayed open");
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        0,
        "force-submit pressed Enter while either resolver still owned the attempt"
    );

    b_release.add_permits(1);
    let b_result = tokio::time::timeout(Duration::from_secs(5), resolver_b)
        .await
        .expect("resolver B returned after its reservation released")
        .expect("resolver B task joined");
    assert_eq!(
        b_result["error"]["code"], "attention_evidence_failed",
        "resolver B must cancel before recording any terminal intent: {b_result}"
    );

    let intent = wait_for_workspace_fact(&rig, &message_id, "notification_resolution_intent").await;
    assert_eq!(intent.data.as_ref().unwrap()["forced"], true);
    wait_for_workspace_fact(&rig, &message_id, "notification_resolution_action_accepted").await;
    rig.tmux.wait_screen("main", "FAKETUI-WORKING");

    let lines = workspace_lines(&rig);
    for fact_type in [
        "notification_resolution_intent",
        "notification_resolution_action_accepted",
    ] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.id == message_id
                        && line
                            .data
                            .as_ref()
                            .is_some_and(|data| data["type"] == fact_type)
                })
                .count(),
            1,
            "successive reservations appended {fact_type} more than once: {lines:#?}"
        );
    }
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        1,
        "successive reservations allowed more than one forced Enter"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claiming_the_message_before_the_timer_cancels_force_submit() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-claim-cancel",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\nforce_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 500\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_submit" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if should_pause {
                    let _ = entered_tx.send(());
                    pause.acquire_owned().await.unwrap().forget();
                }
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "force-submit-claim-cancel",
        "Claim cancels",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    release.add_permits(1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;

    rig.daemon
        .claim_message_for_test("worker", &message_id)
        .expect("recipient claims the exact message");
    tokio::time::sleep(Duration::from_millis(750)).await;
    let lines = workspace_lines(&rig);
    assert!(
        lines.iter().all(|line| {
            line.id != message_id
                || line.data.as_ref().is_none_or(|data| {
                    !matches!(
                        data["type"].as_str(),
                        Some(
                            "notification_resolution_intent"
                                | "notification_resolution_action_accepted"
                        )
                    )
                })
        }),
        "a claimed message reached the force-submit boundary: {lines:#?}"
    );
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        0,
        "claim cancellation still pressed Enter"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn claiming_after_forced_intent_withholds_the_submit_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-claim-after-intent",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\nforce_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel();
    let (intent_tx, mut intent_rx) = tokio::sync::mpsc::unbounded_channel();
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_release = Arc::new(tokio::sync::Semaphore::new(0));
    let intent_release = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pre = Arc::new(AtomicBool::new(true));
    let first_intent = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let pre_release = Arc::clone(&pre_release);
        let intent_release = Arc::clone(&intent_release);
        let first_pre = Arc::clone(&first_pre);
        let first_intent = Arc::clone(&first_intent);
        move |phase| {
            let pre_tx = pre_tx.clone();
            let intent_tx = intent_tx.clone();
            let key_tx = key_tx.clone();
            let pre_release = Arc::clone(&pre_release);
            let intent_release = Arc::clone(&intent_release);
            let pause_pre = phase == "pre_submit" && first_pre.swap(false, Ordering::SeqCst);
            let pause_intent =
                phase == "force_submit_after_intent" && first_intent.swap(false, Ordering::SeqCst);
            let sent_key = phase == "force_submit_after_key_before_accepted";
            Box::pin(async move {
                if pause_pre {
                    let _ = pre_tx.send(());
                    pre_release.acquire_owned().await.unwrap().forget();
                } else if pause_intent {
                    let _ = intent_tx.send(());
                    intent_release.acquire_owned().await.unwrap().forget();
                } else if sent_key {
                    let _ = key_tx.send(());
                }
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "force-submit-claim-after-intent",
        "Claim after forced intent",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), pre_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    pre_release.add_permits(1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;

    tokio::time::timeout(Duration::from_secs(5), intent_rx.recv())
        .await
        .expect("force-submit recorded durable intent")
        .expect("intent sender stayed open");
    rig.daemon
        .claim_message_for_test("worker", &message_id)
        .expect("recipient claims the exact message after forced intent");
    intent_release.add_permits(1);

    let withdrawn = wait_for_workspace_fact(
        &rig,
        &message_id,
        "notification_resolution_intent_withdrawn",
    );
    tokio::pin!(withdrawn);
    tokio::select! {
        _ = key_rx.recv() => panic!(
            "a claim after forced intent still reached the terminal submit key"
        ),
        _ = &mut withdrawn => {}
    }

    let lines = workspace_lines(&rig);
    for fact_type in [
        "notification_resolution_action_reserved",
        "notification_resolution_action_accepted",
    ] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.id == message_id
                        && line
                            .data
                            .as_ref()
                            .is_some_and(|data| data["type"] == fact_type)
                })
                .count(),
            0,
            "claim cancellation must happen before {fact_type}: {lines:#?}"
        );
    }
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        0,
        "claim cancellation still pressed Enter"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_force_submit_before_the_forced_key_reservation_withholds_the_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-disable-before-reservation",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\nforce_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel();
    let (setting_tx, mut setting_rx) = tokio::sync::mpsc::unbounded_channel();
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_release = Arc::new(tokio::sync::Semaphore::new(0));
    let setting_release = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pre = Arc::new(AtomicBool::new(true));
    let first_setting = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let pre_release = Arc::clone(&pre_release);
        let setting_release = Arc::clone(&setting_release);
        let first_pre = Arc::clone(&first_pre);
        let first_setting = Arc::clone(&first_setting);
        move |phase| {
            let pre_tx = pre_tx.clone();
            let setting_tx = setting_tx.clone();
            let key_tx = key_tx.clone();
            let pre_release = Arc::clone(&pre_release);
            let setting_release = Arc::clone(&setting_release);
            let pause_pre = phase == "pre_submit" && first_pre.swap(false, Ordering::SeqCst);
            let pause_setting = phase == "force_submit_after_setting_check_before_reservation"
                && first_setting.swap(false, Ordering::SeqCst);
            let sent_key = phase == "force_submit_after_key_before_accepted";
            Box::pin(async move {
                if pause_pre {
                    let _ = pre_tx.send(());
                    pre_release.acquire_owned().await.unwrap().forget();
                } else if pause_setting {
                    let _ = setting_tx.send(());
                    setting_release.acquire_owned().await.unwrap().forget();
                } else if sent_key {
                    let _ = key_tx.send(());
                }
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "force-submit-disable-before-reservation",
        "Disable before forced key reservation",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), pre_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    pre_release.add_permits(1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;

    tokio::time::timeout(Duration::from_secs(5), setting_rx.recv())
        .await
        .expect("force-submit read the enabled setting before reservation")
        .expect("setting-check sender stayed open");
    let disabled = rig
        .ctl
        .request(
            "notification.force_submit.set",
            json!({"enabled": false, "delay_seconds": 0}),
        )
        .await;
    assert_eq!(disabled["result"]["enabled"], false, "{disabled}");
    setting_release.add_permits(1);

    let withdrawn = wait_for_workspace_fact(
        &rig,
        &message_id,
        "notification_resolution_intent_withdrawn",
    );
    tokio::pin!(withdrawn);
    tokio::select! {
        _ = key_rx.recv() => panic!(
            "a successful force-submit disable still reached the terminal key"
        ),
        _ = &mut withdrawn => {}
    }

    let lines = workspace_lines(&rig);
    for fact_type in [
        "notification_resolution_action_reserved",
        "notification_resolution_action_accepted",
    ] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| {
                    line.id == message_id
                        && line
                            .data
                            .as_ref()
                            .is_some_and(|data| data["type"] == fact_type)
                })
                .count(),
            0,
            "disable must withdraw before {fact_type}: {lines:#?}"
        );
    }
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        0,
        "a successful disable still pressed Enter"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn later_disable_or_claim_cannot_revoke_the_forced_key_reservation() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-force-submit-events-after-reservation",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\nforce_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (pre_tx, mut pre_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reservation_tx, mut reservation_rx) = tokio::sync::mpsc::unbounded_channel();
    let pre_release = Arc::new(tokio::sync::Semaphore::new(0));
    let reservation_release = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pre = Arc::new(AtomicBool::new(true));
    let first_reservation = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let pre_release = Arc::clone(&pre_release);
        let reservation_release = Arc::clone(&reservation_release);
        let first_pre = Arc::clone(&first_pre);
        let first_reservation = Arc::clone(&first_reservation);
        move |phase| {
            let pre_tx = pre_tx.clone();
            let reservation_tx = reservation_tx.clone();
            let pre_release = Arc::clone(&pre_release);
            let reservation_release = Arc::clone(&reservation_release);
            let pause_pre = phase == "pre_submit" && first_pre.swap(false, Ordering::SeqCst);
            let pause_reservation = phase == "force_submit_after_terminal_key_reservation"
                && first_reservation.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if pause_pre {
                    let _ = pre_tx.send(());
                    pre_release.acquire_owned().await.unwrap().forget();
                } else if pause_reservation {
                    let _ = reservation_tx.send(());
                    reservation_release.acquire_owned().await.unwrap().forget();
                }
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "force-submit-events-after-reservation",
        "Events after forced key reservation",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), pre_rx.recv())
        .await
        .expect("doorbell reached pre-submit")
        .expect("pre-submit sender stayed open");
    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    pre_release.add_permits(1);
    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;

    tokio::time::timeout(Duration::from_secs(5), reservation_rx.recv())
        .await
        .expect("forced key reservation is durable before terminal IO")
        .expect("reservation sender stayed open");
    // A completed settings request affects future timers. It cannot revoke
    // this key because the durable reservation is already the commit point.
    let disabled = rig
        .ctl
        .request(
            "notification.force_submit.set",
            json!({"enabled": false, "delay_seconds": 0}),
        )
        .await;
    assert_eq!(disabled["result"]["enabled"], false, "{disabled}");
    rig.daemon
        .claim_message_for_test("worker", &message_id)
        .expect("recipient claims after the forced key reservation");
    let lines_after_claim = workspace_lines(&rig);
    let reservation_seq = lines_after_claim
        .iter()
        .find(|line| {
            line.id == message_id
                && line
                    .data
                    .as_ref()
                    .is_some_and(|data| data["type"] == "notification_resolution_action_reserved")
        })
        .expect("one durable forced key reservation")
        .seq;
    let claim_seq = lines_after_claim
        .iter()
        .find(|line| {
            line.id == message_id
                && line
                    .data
                    .as_ref()
                    .is_some_and(|data| data["type"] == "message_claimed")
        })
        .expect("recipient claim fact")
        .seq;
    assert!(
        reservation_seq < claim_seq,
        "the regression must exercise a claim ordered after the reservation: {lines_after_claim:#?}"
    );

    reservation_release.add_permits(1);
    wait_for_workspace_fact(&rig, &message_id, "notification_resolution_action_accepted").await;
    rig.tmux.wait_screen("main", "FAKETUI-WORKING");

    let lines = workspace_lines(&rig);
    assert_eq!(
        lines
            .iter()
            .filter(|line| {
                line.id == message_id
                    && line.data.as_ref().is_some_and(|data| {
                        data["type"] == "notification_resolution_action_reserved"
                    })
            })
            .count(),
        1,
        "exactly one forced key may be reserved: {lines:#?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| {
                line.id == message_id
                    && line.data.as_ref().is_some_and(|data| {
                        data["type"] == "notification_resolution_action_accepted"
                    })
            })
            .count(),
        1,
        "the ordered reservation must still send exactly one key: {lines:#?}"
    );
    assert!(
        lines.iter().all(|line| {
            line.id != message_id
                || line
                    .data
                    .as_ref()
                    .is_none_or(|data| data["type"] != "notification_resolution_intent_withdrawn")
        }),
        "events after reservation must not revoke its forced key: {lines:#?}"
    );
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        1,
        "the reservation boundary must admit exactly one terminal Enter"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_start_after_submit_reservation_withholds_enter() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-doorbell-reserved-hook-start",
        HOOK_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "post_submit_reservation" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent = send_workspace_message(
        &rig,
        "doorbell-reserved-hook-start",
        "Reserved hook start",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the post-reservation pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitting),
        1
    );
    wait_for_doorbell(&rig, &pane, &message_id).await;

    // The exact doorbell is still visible, but a confirmed lifecycle edge
    // says this occupant is already running a turn. Exact content alone must
    // not authorize a second terminal key.
    let report = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "worker",
                "event": "UserPromptSubmit",
                "seq": 1,
                "payload": {
                    "prompt": "a different prompt already started this turn",
                    "session_id": "session-1",
                    "turn_id": "turn-1"
                }
            }))
            .unwrap(),
        )
        .await
        .expect("hook report accepted");
    assert_eq!(report["applied"], true, "{report}");
    assert_eq!(report["state"], "working", "{report}");
    hold.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    let attention =
        notification_transition(&rig, &message_id, NotificationState::AttentionRequired)
            .expect("durable attention transition");
    assert_eq!(attention.data.unwrap()["cause"], "verify_failed");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        0,
        "a live Working edge must withhold Enter"
    );
    assert!(
        rig.tmux
            .capture(&pane)
            .contains(&compact_doorbell(&rig, &message_id)),
        "the withheld doorbell should remain available for reconciliation"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_before_submit_still_submits_the_operator_visible_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "submit = \"Enter\"\n",
        "submit = \"Enter\"\nclear_keys = [\"C-c\"]\n",
    );
    let pane_command = format!("python3 {} --clear-staged", faketui_path());
    let mut rig = Rig::new(
        "workspace-claim-before-submit",
        &manifest,
        &pane_command,
        "receipt_block_ms = 15000\nack_timeout_ms = 15000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_submit" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let first =
        send_workspace_message(&rig, "claim-before-submit-first", "First", "first body").await;
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");
    wait_for_doorbell(&rig, &pane, &first_id).await;

    rig.daemon
        .claim_message_for_test("worker", &first_id)
        .expect("exact recipient claim");
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Notified),
        0,
        "claim before Enter cannot create a notified fact"
    );
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &first_id, NotificationState::Submitted).await;
    wait_for_notification_state(&mut rig, &first_id, NotificationState::Notified).await;
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_row = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(first_id.as_str()))
        .expect("claimed message remains visible");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"], "notified",
        "socket retrieval must not suppress the independent pane notification"
    );
    assert_eq!(
        first_row["recipients"][0]["notification"]["settlement"],
        serde_json::Value::Null
    );
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Submitted),
        1
    );
    assert!(pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &first_id)));
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_in_the_post_key_gap_settles_before_the_next_notification() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-claim-post-key-gap",
        CAT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 15000\nack_timeout_ms = 15000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (post_submit_tx, mut post_submit_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let post_submit_tx = post_submit_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "post_key" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if phase == "post_submit" {
                    let _ = post_submit_tx.send(());
                    return;
                }
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let first = send_workspace_message(&rig, "post-key-first", "First", "first body").await;
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the post-key pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &first_id, NotificationState::Submitted),
        1
    );

    rig.daemon
        .claim_message_for_test("worker", &first_id)
        .expect("exact recipient claim");
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_row = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(first_id.as_str()))
        .expect("claimed message remains visible");
    assert_eq!(first_row["recipients"][0]["mailbox"]["status"], "claimed");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"],
        "notified"
    );
    assert_eq!(snapshot["result"]["counts"]["open_attention_entries"], 0);

    release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), post_submit_rx.recv())
            .await
            .is_err(),
        "a claimed attempt must settle before the post-submit receipt path"
    );
    wait_pane_state(&mut rig, "idle").await;
    let second = send_workspace_message(&rig, "post-key-second", "Second", "second body").await;
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &second_id, NotificationState::Writing).await;

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    assert_eq!(snapshot["result"]["counts"]["open_attention_entries"], 0);
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_chrome_does_not_receipt_a_swallowed_compact_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-swallowed-compact-doorbell",
        CAT_MANIFEST,
        &swallowing_animated_composer_pane(),
        "receipt_block_ms = 300\nack_timeout_ms = 50\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let sent = send_workspace_message(
        &rig,
        "swallowed-compact-doorbell",
        "Swallowed doorbell",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Staged).await;
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;

    let attention =
        notification_transition(&rig, &message_id, NotificationState::AttentionRequired)
            .expect("durable attention transition");
    assert_eq!(attention.data.unwrap()["cause"], "ack_timeout");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Notified),
        0
    );
    let screen = rig.tmux.capture(&pane);
    assert!(
        screen.contains(&compact_doorbell(&rig, &message_id)),
        "{screen}"
    );
    assert!(screen.contains("Ctx: 77%"), "{screen}");

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_notification_stages_the_preview_and_exact_claim_without_the_body() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-summary-claim",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    resize_pane_and_allow_event(&mut rig, &pane, 244).await;

    let summary = "This message outlines our Cyclops multi-agent communication test and includes a secret note. Claim the inbox message to read the complete test summary and details.";
    let body = "private implementation details must remain in the mailbox";
    let sent =
        send_summarized_workspace_message(&rig, "summary-claim", "Parser review", summary, body)
            .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Staged).await;

    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the summary notification has one exact attempt");
    let screen = rig.tmux.capture(&pane);
    assert!(
        screen.contains(summary),
        "summary missing from pane: {screen}"
    );
    assert!(
        screen.contains("[cyclops from admin]"),
        "sender missing from pane: {screen}"
    );
    assert!(
        screen.contains(&cyclops_proto::render_doorbell_v3(attempt)),
        "claim command missing from pane: {screen}"
    );
    assert!(
        !screen.contains(body),
        "message body leaked into pane: {screen}"
    );

    let writing = notification_transition(&rig, &message_id, NotificationState::Writing)
        .expect("writing transition fixes the selected format");
    assert_eq!(
        writing.data.as_ref().unwrap()["doorbell_format"],
        cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM
    );
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0,
        "a proven one-row summary notification must submit without operator recovery"
    );
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_summary_soft_wraps_without_dropping_the_operator_preview() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-summary-claim-fallback",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    resize_pane_and_allow_event(&mut rig, &pane, 80).await;

    let summary = "This summary cannot fit beside its exact claim at this width. The mailbox still contains every detail.";
    let sent = send_summarized_workspace_message(
        &rig,
        "summary-claim-fallback",
        "Narrow summary",
        summary,
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;

    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the summary notification keeps the same exact attempt");
    let screen = joined_pane_history(&rig, &pane);
    assert!(
        screen.contains(&cyclops_proto::render_doorbell_v3(attempt)),
        "the exact claim did not reach the pane: {screen}"
    );
    assert!(
        screen.contains(summary),
        "the wrapped operator preview was dropped: {screen}"
    );
    let writing = notification_transition(&rig, &message_id, NotificationState::Writing)
        .expect("the summary notification fixes its selected format durably");
    assert_eq!(
        writing.data.as_ref().unwrap()["doorbell_format"],
        cyclops_proto::DOORBELL_FORMAT_SUMMARY_CLAIM
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0
    );
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn format_3_soft_wraps_in_a_narrow_pane_without_a_width_block() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-format3-width-edge",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    resize_pane_and_allow_event(&mut rig, &pane, 59).await;

    let sent = send_workspace_message(&rig, "format3-width", "Width", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the narrow-pane notification has one exact attempt");
    let doorbell = cyclops_proto::render_doorbell_v3(attempt);
    assert!(joined_pane_history(&rig, &pane).contains(&doorbell));
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        0,
        "terminal width must not suppress an operator-visible doorbell"
    );
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn format_3_resize_between_final_read_and_write_still_soft_wraps() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-format3-width-bookend",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "post_final_prewrite" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent =
        send_workspace_message(&rig, "format3-width-bookend", "Width race", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("delivery reached the final pre-write bookend")
        .expect("pause sender stayed open");
    resize_pane_and_allow_event(&mut rig, &pane, 59).await;
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    let attempt = current_notification_attempt(&workspace_lines(&rig), &message_id)
        .expect("the resized notification has one exact attempt");
    assert!(joined_pane_history(&rig, &pane).contains(&cyclops_proto::render_doorbell_v3(attempt)));
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        0
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exact_attempt_ack_timeout_claim_clears_then_advances_the_fifo() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let log_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "claimed-ack-timeout-submit-log-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&log_dir).unwrap();
    let submit_log = log_dir.join("submits.txt");
    let pane_command = format!(
        "python3 {} --swallow-once --clear-staged --submit-log {}",
        faketui_path(),
        submit_log.display()
    );
    let manifest = HOOK_MANIFEST.replace(
        "submit = \"Enter\"\n",
        "submit = \"Enter\"\nclear_keys = [\"C-c\"]\n",
    );
    let mut rig = Rig::new(
        "workspace-claimed-v3-ack-timeout",
        &manifest,
        &pane_command,
        "delivery_retry_max = 0\nreceipt_block_ms = 300\nack_timeout_ms = 50\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let pair = send_waiting_pair(&rig, "claimed-v3-ack-timeout").await;
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Staged).await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Submitted).await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::AttentionRequired).await;
    let attention =
        notification_transition(&rig, &pair.first, NotificationState::AttentionRequired)
            .expect("durable ACK-timeout transition");
    assert_eq!(attention.data.as_ref().unwrap()["cause"], "ack_timeout");
    let attempt_id = attention.data.as_ref().unwrap()["attempt_id"]
        .as_str()
        .unwrap();
    let expected =
        cyclops_proto::render_doorbell_v3(NotificationAttemptId::parse(attempt_id).unwrap());
    assert!(rig.tmux.capture(&pane).contains(&expected));
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);

    let (settlement_tx, mut settlement_rx) = tokio::sync::mpsc::unbounded_channel();
    let release_settlement = Arc::new(tokio::sync::Semaphore::new(0));
    let (refusal_tx, mut refusal_rx) = tokio::sync::mpsc::unbounded_channel();
    let release_refusal = Arc::new(tokio::sync::Semaphore::new(0));
    let (second_staged_tx, mut second_staged_rx) = tokio::sync::mpsc::unbounded_channel();
    let release_second = Arc::new(tokio::sync::Semaphore::new(0));
    let settlement_pause = Arc::clone(&release_settlement);
    let refusal_pause = Arc::clone(&release_refusal);
    let second_pause = Arc::clone(&release_second);
    rig.daemon.set_inject_pause(move |phase| {
        let settlement_tx = settlement_tx.clone();
        let refusal_tx = refusal_tx.clone();
        let second_staged_tx = second_staged_tx.clone();
        let settlement_pause = Arc::clone(&settlement_pause);
        let refusal_pause = Arc::clone(&refusal_pause);
        let second_pause = Arc::clone(&second_pause);
        Box::pin(async move {
            match phase {
                "post_claimed_notification_refusal" => {
                    let _ = refusal_tx.send(());
                    refusal_pause.acquire_owned().await.unwrap().forget();
                }
                "pre_claimed_notification_settlement" => {
                    let _ = settlement_tx.send(());
                    settlement_pause.acquire_owned().await.unwrap().forget();
                }
                "pre_submit" => {
                    let _ = second_staged_tx.send(());
                    second_pause.acquire_owned().await.unwrap().forget();
                }
                _ => {}
            }
        })
    });

    let started = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "worker",
                "event": "UserPromptSubmit",
                "seq": 1,
                "payload": {
                    "prompt": "a different prompt is already running",
                    "session_id": "claimed-ack-timeout-session",
                    "turn_id": "claimed-ack-timeout-turn"
                }
            }))
            .unwrap(),
        )
        .await
        .expect("turn start accepted");
    assert_eq!(started["applied"], true, "{started}");
    assert_eq!(started["state"], "working", "{started}");
    wait_pane_state(&mut rig, "working").await;

    rig.daemon
        .claim_message_for_test("worker", &pair.first)
        .expect("exact recipient claim");
    tokio::time::timeout(Duration::from_secs(5), refusal_rx.recv())
        .await
        .expect("claimed recovery reached its unsafe-action decision")
        .expect("refusal sender stayed open");
    assert!(settlement_rx.try_recv().is_err());
    assert!(
        rig.tmux.capture(&pane).contains(&expected),
        "the exact doorbell must remain staged while terminal action is unsafe"
    );
    assert!(workspace_lines(&rig).iter().all(|line| {
        line.id != pair.first
            || line
                .data
                .as_ref()
                .is_none_or(|data| data["type"] != "notification_claimed_ack_timeout_reconciled")
    }));

    let stopped = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "worker",
                "event": "Stop",
                "seq": 2,
                "payload": {
                    "session_id": "claimed-ack-timeout-session",
                    "turn_id": "claimed-ack-timeout-turn"
                }
            }))
            .unwrap(),
        )
        .await
        .expect("turn end accepted");
    assert_eq!(stopped["applied"], true, "{stopped}");
    assert_eq!(stopped["state"], "idle", "{stopped}");
    wait_pane_state(&mut rig, "idle").await;
    release_refusal.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), settlement_rx.recv())
        .await
        .expect("claimed notification reached the settlement boundary")
        .expect("settlement sender stayed open");
    let claim = wait_for_workspace_fact(&rig, &pair.first, "message_claimed").await;
    assert!(workspace_lines(&rig).iter().all(|line| {
        line.id != pair.first
            || line
                .data
                .as_ref()
                .is_none_or(|data| data["type"] != "notification_claimed_ack_timeout_reconciled")
    }));
    assert!(notification_attempts(&rig, &pair.second).is_empty());
    let claimed = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_before_settlement = claimed["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("claimed message remains visible before settlement");
    assert_eq!(
        first_before_settlement["recipients"][0]["mailbox"]["status"],
        "claimed"
    );
    assert_eq!(
        first_before_settlement["recipients"][0]["notification"]["state"],
        "attention_required"
    );
    assert_eq!(
        first_before_settlement["recipients"][0]["notification"]["cause"],
        "ack_timeout"
    );
    assert_eq!(claimed["result"]["counts"]["open_attention_entries"], 1);
    assert!(
        !rig.tmux.capture(&pane).contains(&expected),
        "exact clear must finish before durable settlement"
    );
    release_settlement.add_permits(1);
    let settled = wait_for_workspace_fact(
        &rig,
        &pair.first,
        "notification_claimed_ack_timeout_reconciled",
    )
    .await;
    assert!(claim.seq < settled.seq, "claim must precede reconciliation");
    assert!(settled.subject.is_none());
    assert!(settled.body.is_none());
    assert!(settled.reply_to.is_none());
    assert!(settled.deliveries.is_empty());
    let data = settled.data.as_ref().unwrap().as_object().unwrap();
    assert_eq!(
        data.keys().cloned().collect::<BTreeSet<_>>(),
        [
            "attempt_id".to_string(),
            "message_id".to_string(),
            "recipient".to_string(),
            "record_version".to_string(),
            "type".to_string(),
        ]
        .into_iter()
        .collect()
    );
    assert!(data.get("composer").is_none());
    assert!(data.get("diff").is_none());
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Notified),
        0,
        "only the dedicated reconciliation fact may project Notified"
    );
    rig.ev
        .wait_event(Duration::from_secs(5), |event| {
            event["event"] == "messages.changed"
                && event["seq"] == settled.seq
                && event["data"]["changed"].as_array().is_some_and(|areas| {
                    areas.iter().any(|area| area == "notifications")
                        && areas.iter().any(|area| area == "attention")
                })
        })
        .await;

    tokio::time::timeout(Duration::from_secs(5), second_staged_rx.recv())
        .await
        .expect("next FIFO notification reached pre-submit")
        .expect("pre-submit sender stayed open");
    let second_writing = notification_transition(&rig, &pair.second, NotificationState::Writing)
        .expect("next FIFO notification crossed the write boundary");
    assert!(
        settled.seq < second_writing.seq,
        "next FIFO notification advanced before reconciliation"
    );
    assert_eq!(notification_attempts(&rig, &pair.first).len(), 1);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Writing),
        1,
        "reconciliation must not paste the first doorbell again"
    );
    assert_eq!(fs::read_to_string(&submit_log).unwrap().lines().count(), 1);
    assert!(!rig.tmux.capture(&pane).contains(&expected));

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("claimed message remains visible");
    assert_eq!(first["recipients"][0]["mailbox"]["status"], "claimed");
    assert_eq!(first["recipients"][0]["notification"]["state"], "notified");
    assert_eq!(snapshot["result"]["counts"]["open_attention_entries"], 0);

    release_second.add_permits(1);
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exact_submitted_claim_needs_fresh_clean_composer_then_wakes_fifo() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "submit = \"Enter\"\n",
        "submit = \"Enter\"\nclear_keys = [\"C-c\"]\n",
    );
    let pane_command = format!("python3 {} --swallow-once --clear-staged", faketui_path());
    let mut rig = Rig::new(
        "workspace-claimed-submitted-clean",
        &manifest,
        &pane_command,
        "receipt_block_ms = 15000\nack_timeout_ms = 15000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (submitted_tx, mut submitted_rx) = tokio::sync::mpsc::unbounded_channel();
    let release_submitted = Arc::new(tokio::sync::Semaphore::new(0));
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let release_submitted = Arc::clone(&release_submitted);
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let submitted_tx = submitted_tx.clone();
            let release_submitted = Arc::clone(&release_submitted);
            let should_pause = phase == "post_submit" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = submitted_tx.send(());
                release_submitted.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let pair = send_waiting_pair(&rig, "claimed-submitted-clean").await;
    assert_only_oldest_attempt_exists(&rig, &pair);
    tokio::time::timeout(Duration::from_secs(5), submitted_rx.recv())
        .await
        .expect("first notification reached Submitted")
        .expect("post-submit sender stayed open");
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Submitted).await;
    let submitted = notification_transition(&rig, &pair.first, NotificationState::Submitted)
        .expect("durable Submitted transition");
    let attempt_id = submitted.data.as_ref().unwrap()["attempt_id"]
        .as_str()
        .unwrap()
        .to_string();
    let expected =
        cyclops_proto::render_doorbell_v3(NotificationAttemptId::parse(&attempt_id).unwrap());
    assert!(rig.tmux.capture(&pane).contains(&expected));

    rig.daemon
        .claim_message_for_test("worker", &pair.first)
        .expect("exact recipient claim");
    let claim = wait_for_workspace_fact(&rig, &pair.first, "message_claimed").await;

    // A fresh observation that still sees staged input must preserve the
    // exact owner. Retrieval alone is never composer-clearance evidence.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-l"]);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(rig.tmux.capture(&pane).contains(&expected));
    assert!(workspace_lines(&rig).iter().all(|line| {
        line.id != pair.first
            || line
                .data
                .as_ref()
                .is_none_or(|data| data["type"] != "notification_barrier_retired")
    }));
    assert_eq!(
        notification_state_count(&rig, &pair.second, NotificationState::Writing),
        0,
        "claim alone released the FIFO"
    );

    release_submitted.add_permits(1);
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-c"]);
    let retired = wait_for_workspace_fact(&rig, &pair.first, "notification_barrier_retired").await;
    let retired_data = retired.data.as_ref().unwrap();
    assert_eq!(retired_data["attempt_id"], attempt_id);
    assert_eq!(retired_data["cause"], "composer_observed_clear");
    assert!(claim.seq < retired.seq);

    wait_for_notification_state(&mut rig, &pair.second, NotificationState::Writing).await;
    let second_writing = notification_transition(&rig, &pair.second, NotificationState::Writing)
        .expect("next FIFO notification crossed the write boundary");
    assert!(retired.seq < second_writing.seq);
    assert_eq!(notification_attempts(&rig, &pair.first).len(), 1);
    assert_eq!(notification_attempts(&rig, &pair.second).len(), 1);
    assert!(!rig.tmux.capture(&pane).contains(&expected));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recipient_without_exact_mailbox_evidence_gets_direct_delivery_without_a_claim() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_without_mailbox_capability(
        "workspace-direct-fallback",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let pair = send_waiting_pair(&rig, "direct-fallback").await;
    wait_for_workspace_fact(&rig, &pair.first, "message_delivered_direct").await;
    wait_for_workspace_fact(&rig, &pair.second, "message_delivered_direct").await;

    let lines = workspace_lines(&rig);
    for message_id in [&pair.first, &pair.second] {
        assert!(lines.iter().any(|line| {
            line.id == *message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition"
                        && data["state"] == "writing"
                        && data["transport"] == "direct_payload"
                })
        }));
        assert!(lines.iter().all(|line| {
            line.id != *message_id
                || line
                    .data
                    .as_ref()
                    .is_none_or(|data| data["type"] != "message_claimed")
        }));
    }
    let screen = pane_history(&rig, &pane);
    assert!(screen.contains("first body"));
    assert!(screen.contains("second body"));
    for message_id in [&pair.first, &pair.second] {
        assert!(!screen.contains(&compact_doorbell(&rig, message_id)));
        assert!(!screen.contains(&legacy_doorbell(message_id)));
    }

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let rows = snapshot["result"]["rows"].as_array().unwrap();
    for message_id in [&pair.first, &pair.second] {
        let row = rows
            .iter()
            .find(|row| row["message_id"].as_str() == Some(message_id.as_str()))
            .expect("directly delivered message remains visible in the snapshot");
        assert_eq!(
            row["recipients"][0]["mailbox"]["status"],
            "delivered_direct"
        );
    }

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mailbox_evidence_drift_before_the_write_falls_back_without_a_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-capability-drift",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_paste" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let sent = send_workspace_message(
        &rig,
        "capability-drift",
        "Capability changed",
        "direct body after capability drift",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell attempt reached the prewrite pause")
        .expect("pause sender stayed open");
    fs::write(rig.home.join("cyclops-skill.md"), b"outdated skill bytes").unwrap();
    hold.add_permits(1);

    wait_for_workspace_fact(&rig, &message_id, "message_delivered_direct").await;
    let lines = workspace_lines(&rig);
    let writing: Vec<_> = lines
        .iter()
        .filter(|line| {
            line.id == message_id
                && line.data.as_ref().is_some_and(|data| {
                    data["type"] == "notification_transition" && data["state"] == "writing"
                })
        })
        .collect();
    assert_eq!(writing.len(), 1);
    assert_eq!(
        writing[0].data.as_ref().unwrap()["transport"],
        "direct_payload"
    );
    assert!(lines.iter().all(|line| {
        line.id != message_id
            || line
                .data
                .as_ref()
                .is_none_or(|data| data["type"] != "message_claimed")
    }));
    let screen = pane_history(&rig, &pane);
    assert!(screen.contains("direct body after capability drift"));
    assert!(!screen.contains(&compact_doorbell(&rig, &message_id)));
    assert!(!screen.contains(&legacy_doorbell(&message_id)));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_human_draft_holds_one_notification_attempt_until_its_turn_finishes() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-human-draft", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, "human draft stays private"]);
    rig.tmux.wait_screen("main", "human draft stays private");
    wait_for_human_composer_evidence(&mut rig, &pane).await;

    let pair = send_waiting_pair(&rig, "draft").await;

    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "gate"
                && event["data"]["id"] == pair.first.as_str()
                && event["data"]["action"] == "hold"
        })
        .await;
    let held_screen = rig.tmux.capture(&pane);
    assert!(held_screen.contains("human draft stays private"));
    assert!(!held_screen.contains(&compact_doorbell(&rig, &pair.first)));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "Enter"]);
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        1
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_visible_human_draft_cleared_by_backspace_releases_the_same_attempt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-backspaced-human-draft",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    // One visible character is the smallest draft that exercises the contract:
    // human input holds the attempt, and Backspace clears that same attempt.
    // A longer draft made the test enqueue many independent tmux commands before
    // reaching this boundary, which tested runner scheduling rather than release.
    let draft = "e";
    rig.tmux.run_ok(&["send-keys", "-l", "-t", &pane, draft]);
    rig.tmux.wait_screen("main", draft);
    wait_for_human_composer_evidence(&mut rig, &pane).await;

    let sent = send_workspace_message(
        &rig,
        "backspaced-human-draft",
        "Backspace release",
        "body stays in the mailbox",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    rig.ev
        .wait_event_named(
            Duration::from_secs(8),
            "initial composer gate hold",
            |event| {
                event["event"] == "gate"
                    && event["data"]["id"] == message_id.as_str()
                    && event["data"]["action"] == "hold"
            },
        )
        .await;
    let attempts_before = notification_attempts(&rig, &message_id);
    assert_eq!(attempts_before.len(), 1);
    assert!(!rig
        .tmux
        .capture(&pane)
        .contains(&compact_doorbell(&rig, &message_id)));

    // Subscribe after the exact gate hold and immediately before Backspace.
    // This is the causal barrier: the connection cannot contain a different
    // gate decision from setup, but it is live before the release edge.
    let mut release_events = TestClient::connect(&rig.daemon.socket_path()).await;
    let subscribed = release_events.request("events.subscribe", json!({})).await;
    assert_eq!(subscribed["result"]["subscribed"], true);
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "BSpace"]);
    // `composer_clean` can precede the positive write-readiness edge while the
    // screen is still settling. The readiness event is the exact contract that
    // wakes or reopens the held attempt, so wait for it rather than polling a
    // weaker projection or guessing how long settlement takes.
    let readiness = release_events
        .wait_event_named(
            Duration::from_secs(8),
            "positive readiness after Backspace",
            |event| {
                event["event"] == "readiness"
                    && event["data"]["session_idx"] == 0
                    && event["data"]["pane_id"] == pane.as_str()
                    && event["data"]["write_ready"] == true
                    && event["data"]["write_block"].is_null()
            },
        )
        .await;
    assert_eq!(readiness["seq"], serde_json::Value::Null, "{readiness}");
    // This real-tmux adapter journey stops at the physical edge it owns.
    // Deterministic tests below it protect the rest of the chain:
    // `causal_route_evidence_survives_an_earlier_tokenless_readiness_observation`
    // carries the source edge, `workspace_messaging_applies_a_readiness_route_observation`
    // crosses the Module boundary, and
    // `blocked_readiness_reopens_once_only_after_positive_exact_route_evidence`
    // reopens this same durable attempt. Waiting here for another ephemeral
    // gate event duplicated those contracts and made runner scheduling part of
    // the proof.
    assert_eq!(notification_attempts(&rig, &message_id), attempts_before);
    assert!(workspace_lines(&rig).iter().all(|line| {
        line.id != message_id
            || line
                .data
                .as_ref()
                .is_none_or(|data| data["type"] != "notification_requeued")
    }));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transient_hidden_frame_does_not_bypass_the_active_human_hold() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest =
        CAT_MANIFEST.replacen("lifecycle_evidence = false", "lifecycle_evidence = true", 1);
    assert_ne!(manifest, CAT_MANIFEST, "fixture lifecycle override applies");
    let mut rig = Rig::new(
        "workspace-hidden-human-draft",
        &manifest,
        &composer_pane(),
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, "human draft stays private"]);
    rig.tmux.wait_screen("main", "human draft stays private");
    wait_for_human_composer_evidence(&mut rig, &pane).await;
    wait_pane_state(&mut rig, "idle_with_input").await;

    // Establish the durable barrier before hiding the text. The previous test
    // sent only after C-g, so a loaded runner could cross the 300ms output
    // settlement boundary first and legitimately release the unowned hold.
    // That tested scheduler speed instead of the promised transient-frame
    // behavior.
    let pair = send_waiting_pair(&rig, "hidden-draft").await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;

    // The fixture keeps the bytes staged but stops drawing them. This one
    // clean-looking frame cannot bypass the already durable hold.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-g"]);
    assert!(!rig
        .tmux
        .capture(&pane)
        .contains("human draft stays private"));

    let held = notification_transition(&rig, &pair.first, NotificationState::BlockedPreWrite)
        .expect("the transient hidden frame remains a durable pre-write block");
    let held = held.data.as_ref().expect("blocked transition data");
    assert_eq!(
        held["pre_write_observation"]["write_block"], "composer_hold",
        "a clean-looking frame released hidden human input: {held}"
    );
    assert!(!rig
        .tmux
        .capture(&pane)
        .contains(&compact_doorbell(&rig, &pair.first)));
    assert_only_oldest_attempt_exists(&rig, &pair);

    // Submit proves the apparently empty composer still held the draft. Only
    // that real turn may release the barrier and admit the waiting doorbell.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "Enter"]);
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(released.contains("human draft stays private"));
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_mode_holds_one_notification_attempt_until_the_pane_is_write_ready() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-copy-mode", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    rig.tmux.run_ok(&["copy-mode", "-t", &pane]);
    wait_for_pane_mode(&mut rig, &pane, true).await;

    let pair = send_waiting_pair(&rig, "mode").await;

    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "gate"
                && event["data"]["id"] == pair.first.as_str()
                && event["data"]["action"] == "hold"
                && event["data"]["cause"] == "pane_in_mode"
        })
        .await;
    let held_screen = rig.tmux.capture(&pane);
    assert!(!held_screen.contains(&compact_doorbell(&rig, &pair.first)));
    assert!(!held_screen.contains("first body"));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "q"]);
    wait_for_pane_mode(&mut rig, &pane, false).await;
    let released = wait_for_doorbell(&rig, &pane, &pair.first).await;
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        1
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_human_modal_holds_one_notification_attempt_until_the_prompt_is_cleared() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-human-modal",
        MODAL_MANIFEST,
        &hold_script("FAKE-TRUST-PROMPT"),
        "",
    )
    .await;
    rig.tmux.wait_screen("main", "FAKE-TRUST-PROMPT");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        Box::pin(async move {
            if phase == "post_final_prewrite" {
                let _ = entered_tx.send(());
            }
        })
    });

    let pair = send_waiting_pair(&rig, "modal").await;

    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "admin-notify"
                && event["data"]["level"] == "action_required"
                && event["data"]["pane_id"] == pane
        })
        .await;
    let held_screen = rig.tmux.capture(&pane);
    assert!(held_screen.contains("FAKE-TRUST-PROMPT"));
    assert!(!held_screen.contains(&compact_doorbell(&rig, &pair.first)));
    assert!(!held_screen.contains("first body"));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        0
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    rig.tmux.wait_screen("main", "Model x · Ctx: 78%");
    // The clear first has to reach the delivery state machine and pass all
    // pre-write checks to the post_final_prewrite boundary, then complete
    // staging. Waiting on these durable latches separates a missed release
    // edge from a pane-paint delay, and leaves the screen capture to assert
    // the final verified doorbell.
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("reopen must reach the post_final_prewrite execution boundary")
        .expect("pause sender stayed open");
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Writing).await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::Staged).await;
    let released = rig.tmux.capture(&pane);
    let expected = current_compact_doorbell(&rig, &pair.first)
        .expect("staged attempt must have a formatted doorbell");
    assert!(
        released.contains(&expected),
        "doorbell was not shown: {released}"
    );
    assert!(!released.contains("first body"));
    assert!(!released.contains(&pair.second));
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Staged),
        1
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_messages_schedule_only_the_oldest_without_session_message_rows() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-messaging", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let first_request = json!({
        "to": ["worker"],
        "subject": "First private subject",
        "body": "First private body",
        "client_key": "first-key"
    });
    let first: MsgSendParams = serde_json::from_value(first_request.clone()).unwrap();
    let first = rig.daemon.msg_send("admin", first).await.unwrap();
    let first_id = first["msg_id"].as_str().unwrap().to_string();
    assert_eq!(first["inserted"], true);
    assert!(matches!(
        first["deliveries"][0]["notification_state"].as_str(),
        Some("queued" | "gating" | "writing" | "staged" | "submitted" | "notified")
    ));
    let first_retry = rig
        .daemon
        .msg_send("admin", serde_json::from_value(first_request).unwrap())
        .await
        .unwrap();
    assert_eq!(first_retry["msg_id"], first_id);
    assert_eq!(first_retry["inserted"], false);

    let second_request = json!({
        "to": ["worker"],
        "subject": "Second private subject",
        "body": "Second private body",
        "client_key": "second-key"
    });
    let second = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(second_request.clone()).unwrap(),
        )
        .await
        .unwrap();
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    assert_eq!(second["deliveries"][0]["notification_state"], "not_started");
    assert_eq!(second["deliveries"][0]["position"], 1);

    let retry = rig
        .daemon
        .msg_send("admin", serde_json::from_value(second_request).unwrap())
        .await
        .unwrap();
    assert_eq!(retry["msg_id"], second_id);
    assert_eq!(retry["inserted"], false);
    assert_eq!(retry["deliveries"][0]["notification_state"], "not_started");

    let screen = wait_for_doorbell(&rig, &pane, &first_id).await;
    assert!(!screen.contains("First private subject"));
    assert!(!screen.contains("First private body"));
    assert!(!screen.contains(&second_id));

    let workspace = workspace_lines(&rig);
    let messages: Vec<_> = workspace
        .iter()
        .filter(|line| matches!(line.kind, Kind::Msg | Kind::Fyi))
        .collect();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].id, first_id);
    assert_eq!(messages[1].id, second_id);
    let first_attempts: Vec<_> = workspace
        .iter()
        .filter_map(|line| {
            let data = line.data.as_ref()?;
            (data["type"] == "notification_transition" && line.id == messages[0].id)
                .then(|| data["attempt_id"].as_str().unwrap())
        })
        .collect();
    assert!(!first_attempts.is_empty());
    assert!(first_attempts
        .iter()
        .all(|attempt| attempt == &first_attempts[0]));
    let session_message_rows = rig
        .ledger_lines()
        .into_iter()
        .filter(|line| matches!(line["kind"].as_str(), Some("msg" | "fyi")))
        .count();
    assert_eq!(session_message_rows, 0);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_notification_resumes_after_restart_without_a_second_attempt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("workspace-restart", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_paste" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let result = rig
        .daemon
        .msg_send(
            "admin",
            serde_json::from_value(json!({
                "to": ["worker"],
                "subject": "Resume",
                "body": "Durable body",
                "client_key": "restart-key"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let message_id = result["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("notification reached the pre-paste pause")
        .expect("pause sender stayed open");
    assert!(!rig
        .tmux
        .capture(&pane)
        .contains(&compact_doorbell(&rig, &message_id)));

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    let screen = wait_for_doorbell(&rig, &pane, &message_id).await;
    assert!(!screen.contains("Durable body"));

    let attempts: Vec<_> = workspace_lines(&rig)
        .into_iter()
        .filter_map(|line| {
            let data = line.data?;
            (data["type"] == "notification_transition")
                .then(|| data["attempt_id"].as_str().unwrap().to_string())
        })
        .collect();
    assert!(!attempts.is_empty());
    assert!(attempts.iter().all(|attempt| attempt == &attempts[0]));

    drop(hold);
    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replacement_recipient_bypasses_a_stale_prewrite_worker() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-recipient-replacement",
        CAT_MANIFEST,
        &composer_pane(),
        "",
    )
    .await;
    let stale_pane = rig.pane_ids().await[0].clone();
    rig.label(&stale_pane, "worker").await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_paste" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let stale = send_workspace_message(
        &rig,
        "stale-recipient",
        "Stale recipient",
        "private stale body",
    )
    .await;
    let stale_id = stale["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("stale recipient reached the prewrite pause")
        .expect("pause sender stayed open");
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0
    );

    rig.tmux.simulate_server_loss();
    rig.ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "session" && event["data"]["attached"] == false
        })
        .await;
    rig.tmux.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "160",
        "-y",
        "40",
        &composer_pane(),
    ]);
    let availability = rig
        .ctl
        .request("session.watch", json!({"session": "main"}))
        .await;
    assert_eq!(
        availability["result"]["added"],
        json!(false),
        "{availability}"
    );
    assert_eq!(
        availability["result"]["watching"],
        json!(true),
        "{availability}"
    );
    rig.wait_attached(1).await;
    let replacement_pane = rig.pane_ids().await[0].clone();
    assert_eq!(
        replacement_pane, stale_pane,
        "fixture must reuse the pane id"
    );
    rig.label(&replacement_pane, "worker").await;

    let replacement = send_workspace_message(
        &rig,
        "replacement-recipient",
        "Replacement recipient",
        "private replacement body",
    )
    .await;
    let replacement_id = replacement["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &replacement_id, NotificationState::Staged).await;
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0
    );

    hold.add_permits(1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        0,
        "the stale route crossed the irreversible write boundary"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn same_route_with_a_new_pane_root_reproves_before_writing() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-pane-root-replacement",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let initial_pid = pane_pid(&rig, &pane);

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    let first_pause = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_pause = Arc::clone(&first_pause);
        move |phase| {
            let entered_tx = entered_tx.clone();
            let pause = Arc::clone(&pause);
            let should_pause = phase == "pre_paste" && first_pause.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = entered_tx.send(());
                pause.acquire_owned().await.unwrap().forget();
            })
        }
    });

    let stale = send_workspace_message(
        &rig,
        "stale-pane-root",
        "Stale pane root",
        "private stale body",
    )
    .await;
    let stale_id = stale["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("notification reached the prewrite pause")
        .expect("pause sender stayed open");

    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &composer_pane()]);
    let replacement_pid = wait_for_pane_pid_change(&rig, &pane, initial_pid).await;
    assert_ne!(replacement_pid, initial_pid);
    let replacement_title = format!("replacement-root-{replacement_pid}");
    rig.tmux
        .run_ok(&["select-pane", "-T", &replacement_title, "-t", &pane]);
    wait_for_pane_observation(&mut rig, &pane, &replacement_title).await;

    hold.add_permits(1);
    wait_for_notification_state(&mut rig, &stale_id, NotificationState::Writing).await;
    let blocked = notification_transition(&rig, &stale_id, NotificationState::BlockedPreWrite)
        .expect("the stale process proof was refused before the write");
    let writing = notification_transition(&rig, &stale_id, NotificationState::Writing)
        .expect("the replacement process was admitted by a fresh proof");
    let blocked_data = blocked.data.as_ref().expect("blocked transition data");
    let writing_data = writing.data.as_ref().expect("writing transition data");
    assert!(
        blocked.seq < writing.seq,
        "the refusal must be durable before replacement proof authorizes Writing"
    );
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::BlockedPreWrite),
        1,
        "process replacement should produce one bounded refusal"
    );
    assert_eq!(
        notification_state_count(&rig, &stale_id, NotificationState::Writing),
        1,
        "the replacement proof should cross the write boundary once"
    );
    assert_eq!(
        blocked_data["attempt_id"], writing_data["attempt_id"],
        "route reconciliation must retain the exact notification attempt"
    );
    assert!(
        matches!(
            blocked_data["pre_write_cause"].as_str(),
            Some("session_unavailable" | "write_readiness_changed")
        ),
        "the original route or process proof must fail closed before reconciliation: {blocked_data}"
    );
    assert_eq!(
        writing_data["binding"]["pane_root"]["pid"], replacement_pid,
        "Writing must bind to the replacement pane process"
    );
    assert_ne!(
        writing_data["binding"]["pane_root"]["pid"], initial_pid,
        "Writing must not reuse the original pane process"
    );
    assert_eq!(
        writing_data["binding"]["manifest"], "fix",
        "the replacement must still match the admitted manifest"
    );

    let status = rig.ctl.request("status", json!({})).await;
    let replacement_row = status["result"]["sessions"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pane_id"] == pane)
        .unwrap();
    assert_eq!(
        replacement_row["agent"], "worker",
        "the logical pane name should survive a same-session process replacement"
    );

    let fresh = send_workspace_message(
        &rig,
        "replacement-pane-root",
        "Replacement pane root",
        "private replacement body",
    )
    .await;
    assert_eq!(fresh["deliveries"][0]["to"], "worker");
    assert_eq!(
        fresh["deliveries"][0]["notification_state"], "not_started",
        "the fresh message is addressable but remains serialized behind the unresolved older one"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_ghost_binding_failure_reopens_only_after_new_route_evidence() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = include_str!("../../../resources/manifests/codex.toml").replace(
        "process_names = [\"codex\"]",
        "process_names = [\"Python\", \"python3\"]",
    );
    let ghost_pane = concat!(
        r#"python3 -c 'import sys,time; "#,
        r#"sys.stdout.write("\033[1m\033[38;2;255;178;66m›\033[0m "#,
        r#"\033[2mSummarize recent commits\033[0m"); "#,
        r#"sys.stdout.flush(); time.sleep(3600)'"#,
    );
    let mut rig = Rig::new("codex-final-binding-unprovable", &manifest, ghost_pane, "").await;
    let pane = rig.pane_ids().await[0].clone();
    let named = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "codex"}),
        )
        .await;
    assert_eq!(named["result"]["label"], "worker", "{named}");
    assert_eq!(named["result"]["manifest"], "codex", "{named}");
    wait_pane_state(&mut rig, "idle").await;

    // The OS observation used for the final write-boundary binding proof is
    // an external boundary. Fail that one observation after the real screen
    // capture has admitted the measured Codex ghost suggestion.
    rig.daemon.fail_next_final_binding_observation();
    let sent = send_workspace_message(
        &rig,
        "codex-final-binding-unprovable",
        "Final binding",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::BlockedPreWrite).await;

    let gate_trace: Vec<String> = rig
        .ledger_lines()
        .iter()
        .filter(|line| line["kind"] == "gate" && line["id"] == message_id)
        .map(|line| {
            format!(
                "{}:{}:{}",
                line["data"]["action"].as_str().unwrap_or(""),
                line["data"]["rule"].as_str().unwrap_or(""),
                line["data"]["cause"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        gate_trace,
        vec![
            "proceed:composer_ghost_suggestion:".to_string(),
            "rebound::binding_unprovable".to_string(),
        ]
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        1
    );
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        assert_eq!(notification_state_count(&rig, &message_id, state), 0);
    }
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &message_id)));

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Gating),
        1,
        "the synthetic post-block reconciliation reopened unchanged evidence"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        1,
        "elapsed time repeated the terminal block"
    );

    // A later explicit route observation is causal evidence even when the
    // proven process binding is unchanged. Reopen the exact attempt once and
    // fail its terminal observation again so the bounded result stays
    // inspectable without writing the notification.
    rig.daemon.fail_next_final_binding_observation();
    let observed = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "codex"}),
        )
        .await;
    assert_eq!(observed["result"]["label"], "worker", "{observed}");
    let reopened_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite) >= 2 {
            break;
        }
        assert!(
            Instant::now() < reopened_deadline,
            "new same-binding route evidence did not reopen the attempt: {:#?}",
            workspace_lines(&rig)
                .into_iter()
                .filter(|line| line.id == message_id)
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    for _ in 0..3 {
        let repeated = rig
            .ctl
            .request(
                "pane.label",
                json!({"target": pane, "label": "worker", "manifest": "codex"}),
            )
            .await;
        assert_eq!(repeated["result"]["label"], "worker", "{repeated}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    assert_eq!(
        rig.ledger_lines()
            .iter()
            .filter(|line| line["kind"] == "gate" && line["id"] == message_id)
            .count(),
        gate_trace.len() * 2,
        "route evidence reopened more than once"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        2,
        "repeated evidence escaped the one-reopen bound"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Gating),
        2,
        "repeated evidence reopened the exact attempt again"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Writing),
        0
    );
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &message_id)));

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn current_command_exec_in_place_reopens_blocked_binding() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let fixture_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "current-command-evidence-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&fixture_dir).unwrap();
    let release_fifo = fixture_dir.join("release.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&release_fifo)
        .status()
        .unwrap();
    assert!(status.success());
    let pane_command = format!(
        "sh -c 'printf \"❯\\n\"; read release < {}; exec cat'",
        release_fifo.display()
    );
    let mut rig = Rig::new(
        "workspace-current-command-evidence",
        CAT_MANIFEST,
        &pane_command,
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;
    let pane_root = pane_pid(&rig, &pane);

    let (prewrite_tx, mut prewrite_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resumed_tx, mut resumed_rx) = tokio::sync::mpsc::unbounded_channel();
    let prewrite_release = Arc::new(tokio::sync::Semaphore::new(0));
    let prewrite_release_seam = Arc::clone(&prewrite_release);
    let first_prewrite = Arc::new(AtomicBool::new(true));
    rig.daemon.set_inject_pause({
        let first_prewrite = Arc::clone(&first_prewrite);
        move |phase| {
            let prewrite_tx = prewrite_tx.clone();
            let resumed_tx = resumed_tx.clone();
            let prewrite_release = Arc::clone(&prewrite_release_seam);
            let should_pause = phase == "pre_paste" && first_prewrite.swap(false, Ordering::SeqCst);
            Box::pin(async move {
                if !should_pause {
                    return;
                }
                let _ = prewrite_tx.send(());
                prewrite_release
                    .acquire_owned()
                    .await
                    .expect("release initial prewrite")
                    .forget();
                let _ = resumed_tx.send(());
            })
        }
    });
    let sent = send_workspace_message(
        &rig,
        "current-command-binding",
        "Current command binding",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(8), prewrite_rx.recv())
        .await
        .expect("initial binding reached the final prewrite path")
        .expect("prewrite pause sender stayed open");
    rig.daemon.fail_next_final_binding_observation();
    prewrite_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(8), resumed_rx.recv())
        .await
        .expect("initial prewrite resumed")
        .expect("prewrite resume sender stayed open");
    rig.daemon.clear_inject_pause();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::BlockedPreWrite).await;
    let blocked = notification_transition(&rig, &message_id, NotificationState::BlockedPreWrite)
        .expect("the terminal binding observation failed closed");
    let blocked_data = blocked.data.as_ref().expect("blocked transition data");
    assert_eq!(blocked_data["pre_write_cause"], "binding_unprovable");
    assert_eq!(
        blocked_data["pre_write_observation"]["binding"]["pane_root"]["pid"],
        pane_root
    );

    fs::write(&release_fifo, b"exec\n").unwrap();
    assert_eq!(
        pane_pid(&rig, &pane),
        pane_root,
        "exec-in-place must preserve the pane process generation"
    );

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    let writing = notification_transition(&rig, &message_id, NotificationState::Writing)
        .expect("CurrentCommand evidence reopened the exact attempt");
    let writing_data = writing.data.as_ref().expect("writing transition data");
    assert_eq!(writing_data["binding"]["pane_root"]["pid"], pane_root);
    assert_eq!(
        writing_data["binding"]["agent"]["pid"],
        blocked_data["pre_write_observation"]["binding"]["agent"]["pid"],
        "the causal edge changed the command without changing process identity"
    );
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);

    rig.daemon.shutdown().await;
    fs::remove_dir_all(fixture_dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_composer_hold_is_a_durable_prewrite_block_until_a_real_turn() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let ghost = r#"\033[1m\342\200\272\033[0m \033[2mFind and fix a bug in @filename\033[0m"#;
    let typed = r#"\033[1m\342\200\272\033[0m fix the rate limiter in gateway.rs"#;
    let pane_command = format!(
        "sh -c 'printf \"{ghost}\\n\"; read a; printf \"\\033[2J\\033[H\"; \
         printf \"{typed}\\n\"; read b; printf \"\\033[2J\\033[H\"; \
         printf \"{ghost}\\n\"; read c; printf \"\\033[2J\\033[H\"; \
         printf \"{ghost}\\n\"; exec cat'"
    );
    let mut rig = Rig::new(
        "composer-hold-durable-prewrite",
        ESC_COMPOSER_MANIFEST,
        &pane_command,
        "receipt_block_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    // Human text, not Cyclops input, creates the hold.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    wait_pane_state(&mut rig, "idle_with_input").await;

    let sent = send_workspace_message(
        &rig,
        "composer-hold-durable-prewrite",
        "Held until a real turn",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    assert_eq!(
        sent["deliveries"][0]["notification_state"], "gating",
        "{sent}"
    );
    assert_eq!(
        sent["deliveries"][0]["pre_write_cause"], "write_readiness_changed",
        "the sender receives the durable refusal rather than a generic queue receipt: {sent}"
    );

    wait_for_notification_state(&mut rig, &message_id, NotificationState::BlockedPreWrite).await;
    let blocked = notification_transition(&rig, &message_id, NotificationState::BlockedPreWrite)
        .expect("typed composer hold is persisted");
    let data = blocked.data.as_ref().expect("blocked transition data");
    assert_eq!(data["pre_write_cause"], "write_readiness_changed");
    assert_eq!(
        data["pre_write_observation"]["write_block"],
        "composer_hold"
    );
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        assert_eq!(notification_state_count(&rig, &message_id, state), 0);
    }
    assert!(!pane_history(&rig, &pane).contains("[cyclops"));

    // This fixture matches `bash`/`sh`; a socket client inherits the shell
    // that starts cargo and therefore has no stable mailbox identity on
    // every runner. Socket identity is covered separately. This delivery
    // test uses the same resolved-admin seam as its send and claim setup to
    // assert the public projection deterministically.
    let snapshot = json!({
        "result": serde_json::to_value(
            rig.daemon
                .messages_snapshot_for_test("admin", 20)
                .expect("admin messages snapshot"),
        )
        .expect("messages snapshot serializes")
    });
    let row = snapshot["result"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["message_id"] == message_id))
        .unwrap_or_else(|| panic!("blocked mailbox row: {snapshot:#}"));
    assert_eq!(row["needs_action"], true, "{row}");
    assert_eq!(row["recipients"][0]["fifo_position"], 1, "{row}");
    assert_eq!(
        row["recipients"][0]["notification"]["pre_write_block"], "composer_hold",
        "{row}"
    );
    assert_eq!(
        row["recipients"][0]["can_withdraw_notification"], true,
        "{row}"
    );

    let status = rig.ctl.request("status", json!({})).await;
    let status_row = status["result"]["blocked_notifications"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["message_id"] == message_id))
        .expect("blocked wake in status");
    assert_eq!(
        status_row["recipient"]["notification"]["pre_write_block"],
        "composer_hold"
    );
    assert_eq!(status_row["recipient"]["current_route"]["pane_id"], pane);
    assert!(
        status_row["waiting_age_ms"].is_u64(),
        "blocked wake exposes its durable age: {status_row}"
    );
    assert_eq!(status_row["next_action"], "withdraw_notification");

    // Pull remains available while automatic input is held. Claiming this
    // exact mailbox entry does not cancel the operator-visible notification.
    rig.daemon
        .claim_message_for_test("worker", &message_id)
        .expect("a held message remains claimable");
    let claimed = json!({
        "result": serde_json::to_value(
            rig.daemon
                .messages_snapshot_for_test("admin", 20)
                .expect("admin messages snapshot after claim"),
        )
        .expect("messages snapshot serializes")
    });
    let claimed_row = claimed["result"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["message_id"] == message_id))
        .expect("claimed message remains visible");
    assert_eq!(claimed_row["recipients"][0]["mailbox"]["status"], "claimed");
    assert_eq!(
        claimed_row["recipients"][0]["notification"]["pre_write_block"],
        "composer_hold"
    );

    let first_withdrawn = rig
        .daemon
        .withdraw_notification_for_test(
            "admin",
            serde_json::from_value(row["recipients"][0]["recipient"].clone())
                .expect("blocked recipient key"),
            serde_json::from_value(data["attempt_id"].clone()).expect("blocked attempt id"),
        )
        .expect("admin withdraws the claimed operator notification");
    assert_eq!(
        first_withdrawn.disposition,
        cyclops_proto::NotificationWithdrawDisposition::Withdrawn
    );

    // Withdrawal is exact, pre-write, and leaves the message pullable. It
    // releases only the FIFO head, so the next message inherits the same
    // truthful block rather than bypassing the composer hold.
    let second = send_workspace_message(
        &rig,
        "composer-hold-durable-prewrite-second",
        "Second held message",
        "second private body",
    )
    .await;
    let second_id = second["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &second_id, NotificationState::BlockedPreWrite).await;
    let second_blocked =
        notification_transition(&rig, &second_id, NotificationState::BlockedPreWrite)
            .expect("second held wake is persisted");
    let second_data = second_blocked
        .data
        .as_ref()
        .expect("second blocked transition data");
    let third = send_workspace_message(
        &rig,
        "composer-hold-durable-prewrite-third",
        "Third held message",
        "third private body",
    )
    .await;
    let third_id = third["msg_id"].as_str().unwrap().to_string();
    assert!(notification_attempts(&rig, &third_id).is_empty());
    let withdrawn = serde_json::to_value(
        rig.daemon
            .withdraw_notification_for_test(
                "admin",
                serde_json::from_value(row["recipients"][0]["recipient"].clone())
                    .expect("blocked recipient key"),
                serde_json::from_value(second_data["attempt_id"].clone())
                    .expect("blocked attempt id"),
            )
            .expect("admin notification withdrawal"),
    )
    .expect("withdrawal result serializes");
    assert_eq!(withdrawn["disposition"], "withdrawn", "{withdrawn}");
    wait_for_notification_state(&mut rig, &third_id, NotificationState::BlockedPreWrite).await;
    let after_withdrawal = json!({
        "result": serde_json::to_value(
            rig.daemon
                .messages_snapshot_for_test("admin", 20)
                .expect("admin messages snapshot after withdrawal"),
        )
        .expect("messages snapshot serializes")
    });
    let second_row = after_withdrawal["result"]["rows"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|candidate| candidate["message_id"] == second_id)
        })
        .expect("withdrawn message remains pullable");
    assert_eq!(second_row["recipients"][0]["mailbox"]["status"], "pending");
    assert_eq!(
        second_row["recipients"][0]["notification"]["operator_withdrawn"],
        true
    );
    assert!(
        second_row["recipients"][0]["fifo_position"].is_null(),
        "a wake withdrawn by the operator remains pullable but no longer occupies notification FIFO: {second_row}"
    );
    let third_row = after_withdrawal["result"]["rows"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|candidate| candidate["message_id"] == third_id)
        })
        .expect("the actionable successor remains visible");
    assert_eq!(
        third_row["recipients"][0]["fifo_position"],
        1,
        "the scheduler skips a withdrawn wake, so the next actionable notification is first: {third_row}"
    );
    let status_after_withdrawal = rig.ctl.request("status", json!({})).await;
    let third_status = status_after_withdrawal["result"]["blocked_notifications"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|candidate| candidate["message_id"] == third_id)
        })
        .expect("the successor's durable pre-write block is visible in status");
    assert_eq!(
        third_status["recipient"]["fifo_position"], 1,
        "status uses the same actionable notification FIFO as Messages: {third_status}"
    );

    // A ghost redraw alone cannot clear the durable block, even across a
    // transient process-table identity lapse during the shell transition.
    // Subscribe at this causal boundary so the assertion cannot run against
    // the prior idle-with-input cache while the redraw is still in flight.
    let mut ghost_events = TestClient::connect(&rig.daemon.socket_path()).await;
    let subscribed = ghost_events.request("events.subscribe", json!({})).await;
    assert_eq!(subscribed["result"]["subscribed"], true);
    rig.daemon.fail_next_admitted_binding_observation();
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    ghost_events
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "state"
                && event["data"]["pane_id"] == pane.as_str()
                && event["data"]["state"] == "idle"
        })
        .await;
    assert_eq!(
        notification_state_count(&rig, &third_id, NotificationState::Gating),
        1,
        "a ghost redraw must not reopen the attempt: {:#?}",
        workspace_lines(&rig)
            .into_iter()
            .filter(|line| line.id == third_id)
            .collect::<Vec<_>>()
    );

    // A genuine start and end is the only causal edge that retires the hold.
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "WORKING the draft"]);
    wait_pane_state(&mut rig, "working").await;
    rig.tmux.run_ok(&["select-pane", "-t", &pane, "-T", "done"]);
    // A real agent turn also produces output. That output is the causal
    // route edge which lets the mailbox reconsider its durable block.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    wait_pane_state(&mut rig, "idle").await;
    wait_for_notification_state(&mut rig, &third_id, NotificationState::Writing).await;
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    assert_eq!(notification_attempts(&rig, &second_id).len(), 1);
    assert_eq!(notification_attempts(&rig, &third_id).len(), 1);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_ghost_with_unprovable_binding_blocks_once_and_withdrawal_advances_fifo() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = include_str!("../../../resources/manifests/codex.toml").replace(
        "process_names = [\"codex\"]",
        "process_names = [\"not-a-real-fixture-process\"]",
    );
    let ghost_pane = concat!(
        r#"python3 -c 'import sys,time; "#,
        r#"sys.stdout.write("\033[1m\033[38;2;255;178;66m›\033[0m "#,
        r#"\033[2mSummarize recent commits\033[0m"); "#,
        r#"sys.stdout.flush(); time.sleep(3600)'"#,
    );
    let mut rig = Rig::new(
        "workspace-unprovable-pinned-binding",
        &manifest,
        ghost_pane,
        "",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    let named = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "codex"}),
        )
        .await;
    assert_eq!(named["result"]["label"], "worker", "{named}");
    assert_eq!(named["result"]["manifest"], "codex", "{named}");
    wait_pane_state(&mut rig, "idle").await;

    let pair = send_waiting_pair(&rig, "unprovable-binding").await;
    assert_eq!(pair.first_receipt["notification_state"], "gating");
    assert_eq!(
        pair.first_receipt["pre_write_cause"], "binding_unprovable",
        "msg.send must return the worker's first durable pre-write disposition"
    );
    assert!(
        pair.second_receipt.get("pre_write_cause").is_none(),
        "the follower must not inherit the head's pre-write block"
    );
    assert!(
        pair.second_receipt.get("wake_block").is_none(),
        "the follower must not inherit the head's scheduler state"
    );
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;
    let gate_trace: Vec<String> = rig
        .ledger_lines()
        .iter()
        .filter(|line| line["kind"] == "gate" && line["id"] == pair.first.as_str())
        .map(|line| {
            format!(
                "{}:{}:{}",
                line["data"]["action"].as_str().unwrap_or(""),
                line["data"]["rule"].as_str().unwrap_or(""),
                line["data"]["cause"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        gate_trace,
        vec!["hold::occupant_unprovable".to_string()],
        "unprovable binding must block before the delivery gate admits a write"
    );
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::BlockedPreWrite),
        1
    );
    let blocked = notification_transition(&rig, &pair.first, NotificationState::BlockedPreWrite)
        .expect("the exact blocked transition is durable");
    let blocked_fact = blocked.data.as_ref().expect("blocked transition has data");
    assert_eq!(
        blocked_fact["pre_write_cause"], "binding_unprovable",
        "the durable reason must name the failed binding proof"
    );
    assert_eq!(
        blocked_fact["pre_write_observation"]["selected_manifest"],
        "codex"
    );
    // A failed OS process observation may not have a pane generation to
    // record. It must never manufacture the complete binding that failed.
    assert!(blocked_fact["pre_write_observation"]
        .get("binding")
        .is_none());
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        assert_eq!(notification_state_count(&rig, &pair.first, state), 0);
    }
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &pair.first)));

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_row = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("blocked message remains visible");
    assert_eq!(
        first_row["recipients"][0]["notification"]["state"],
        "gating"
    );
    assert_eq!(
        first_row["recipients"][0]["notification"]["pre_write_cause"],
        "binding_unprovable"
    );
    assert_eq!(
        first_row["recipients"][0]["can_withdraw_notification"],
        true
    );
    let attempt_id = blocked_fact["attempt_id"].clone();
    let recipient = first_row["recipients"][0]["recipient"].clone();

    tokio::time::sleep(Duration::from_millis(600)).await;
    for _ in 0..3 {
        let repeated = rig
            .ctl
            .request(
                "pane.label",
                json!({"target": pane, "label": "worker", "manifest": "codex"}),
            )
            .await;
        assert_eq!(repeated["result"]["label"], "worker", "{repeated}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(notification_attempts(&rig, &pair.first).len(), 1);
    assert_eq!(
        rig.ledger_lines()
            .iter()
            .filter(|line| line["kind"] == "gate" && line["id"] == pair.first.as_str())
            .count(),
        gate_trace.len(),
        "unchanged evidence must not restart the bounded retry chain"
    );
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::BlockedPreWrite),
        1,
        "elapsed time and unchanged route evidence restarted the blocked attempt"
    );

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
    assert_eq!(withdrawn["result"]["disposition"], "withdrawn");
    wait_for_notification_state(&mut rig, &pair.second, NotificationState::BlockedPreWrite).await;
    assert_eq!(notification_attempts(&rig, &pair.second).len(), 1);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::Writing),
        0
    );
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &pair.second)));

    let after = rig.ctl.request("messages.snapshot", json!({})).await;
    let first_after = after["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("withdrawal keeps the mailbox item visible");
    assert_eq!(
        first_after["recipients"][0]["notification"]["operator_withdrawn"],
        true
    );
    assert_eq!(
        first_after["recipients"][0]["mailbox"]["status"], "pending",
        "withdrawing the wake must not consume the durable message"
    );

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_without_composer_ownership_blocks_once_and_withdrawal_advances_fifo() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST
        .replace("composer_semantic = \"clean\"\n", "")
        .replace("composer_semantic = \"human_input\"\n", "");
    let mut rig = Rig::new(
        "workspace-composer-semantic-missing",
        &manifest,
        &composer_pane(),
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let pair = send_waiting_pair(&rig, "composer-semantic-missing").await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;
    assert_only_oldest_attempt_exists(&rig, &pair);
    let blocked = notification_transition(&rig, &pair.first, NotificationState::BlockedPreWrite)
        .expect("the static manifest gap is durable");
    let fact = blocked.data.as_ref().expect("blocked transition has data");
    assert_eq!(fact["pre_write_cause"], "composer_semantic_missing");
    assert_eq!(fact["pre_write_observation"]["selected_manifest"], "fix");
    assert!(fact["pre_write_observation"]["binding"].is_object());
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        assert_eq!(notification_state_count(&rig, &pair.first, state), 0);
    }
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &pair.first)));

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("blocked message remains visible");
    assert_eq!(
        first["recipients"][0]["notification"]["pre_write_cause"],
        "composer_semantic_missing"
    );
    assert_eq!(first["recipients"][0]["can_withdraw_notification"], true);
    let attempt_id = fact["attempt_id"].clone();
    let recipient = first["recipients"][0]["recipient"].clone();

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    assert_eq!(notification_attempts(&rig, &pair.first).len(), 1);
    assert_eq!(
        notification_state_count(&rig, &pair.first, NotificationState::BlockedPreWrite),
        1,
        "restart duplicated the static pre-write block"
    );

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
    wait_for_notification_state(&mut rig, &pair.second, NotificationState::BlockedPreWrite).await;
    assert_eq!(notification_attempts(&rig, &pair.second).len(), 1);
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &pair.second)));

    rig.daemon.shutdown().await;
}

/// The 2026-08-30 Cursor repro, generalized: a manifest whose composer rule
/// can only ever say `ambiguous` left a wake invisibly "checking readiness"
/// for 16+ hours against a visibly idle pane. Ambiguity that outlives the
/// settle window must settle as a durable `composer_semantic_ambiguous`
/// pre-write block — named, operator-visible, withdrawable — and the gate
/// must first hold under its own named cause, never paste, and never ring
/// the doorbell. Mid-turn ambiguity is exempt by construction (the Working
/// arm owns those frames); this test locks the idle path.
#[tokio::test(flavor = "multi_thread")]
async fn an_indefinitely_ambiguous_idle_composer_settles_as_a_durable_pre_write_block() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "composer_semantic = \"clean\"",
        "composer_semantic = \"ambiguous\"",
    );
    let mut rig = Rig::new(
        "workspace-composer-ambiguous",
        &manifest,
        &composer_pane(),
        "delivery_retry_max = 0\nambiguous_composer_settle_ms = 300",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let pair = send_waiting_pair(&rig, "composer-ambiguous").await;
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;
    assert_only_oldest_attempt_exists(&rig, &pair);
    let blocked = notification_transition(&rig, &pair.first, NotificationState::BlockedPreWrite)
        .expect("settled ambiguity is durable");
    let fact = blocked.data.as_ref().expect("blocked transition has data");
    assert_eq!(fact["pre_write_cause"], "write_readiness_changed");
    assert_eq!(fact["pre_write_observation"]["selected_manifest"], "fix");
    assert!(fact["pre_write_observation"]["binding"].is_object());
    assert_eq!(
        fact["pre_write_observation"]["write_block"],
        "composer_semantic_ambiguous"
    );
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitting,
        NotificationState::Submitted,
    ] {
        assert_eq!(notification_state_count(&rig, &pair.first, state), 0);
    }
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &pair.first)));

    // The block was an escalation, not a first answer: the gate held under
    // its own named cause for the settle window before settling.
    assert!(
        rig.ledger_lines().iter().any(|line| {
            line["kind"] == "gate"
                && line["id"] == pair.first.as_str()
                && line["data"]["action"] == "hold"
                && line["data"]["cause"] == "not_write_ready:composer_semantic_ambiguous"
        }),
        "the settle window must be visible as a named gate hold"
    );

    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let first = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(pair.first.as_str()))
        .expect("blocked message remains visible");
    assert_eq!(
        first["recipients"][0]["notification"]["pre_write_cause"],
        "write_readiness_changed"
    );
    assert_eq!(
        first["recipients"][0]["notification"]["pre_write_block"],
        "composer_semantic_ambiguous"
    );
    assert_eq!(first["recipients"][0]["can_withdraw_notification"], true);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn newly_proven_binding_reopens_the_same_blocked_attempt_once() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let manifest = CAT_MANIFEST.replace(
        "process_names = [\"python3\", \"python\", \"Python\", \"cat\", \"sh\", \"dash\"]",
        "process_names = [\"python3\", \"Python\"]",
    );
    let unrecognized_composer = concat!(
        "sh -c 'printf \"❯\\n",
        "────────────────────────────────────────\\n",
        "Model x · Ctx: 78%\\n\"; exec tail -f /dev/null'"
    );
    let mut rig = Rig::new(
        "workspace-binding-reopen",
        &manifest,
        unrecognized_composer,
        "delivery_retry_max = 0",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    let named = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "worker", "manifest": "fix"}),
        )
        .await;
    assert_eq!(named["result"]["label"], "worker", "{named}");
    wait_pane_state(&mut rig, "idle").await;

    let sent = send_workspace_message(
        &rig,
        "binding-reopen",
        "Binding becomes provable",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::BlockedPreWrite).await;
    let attempt = notification_attempts(&rig, &message_id)
        .into_iter()
        .next()
        .expect("blocked attempt");
    assert!(!pane_history(&rig, &pane).contains(&compact_doorbell(&rig, &message_id)));

    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &composer_pane()]);
    let reopened_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if notification_state_count(&rig, &message_id, NotificationState::Gating) >= 2 {
            break;
        }
        assert!(
            Instant::now() < reopened_deadline,
            "new binding did not reopen the attempt: status={} lines={:#?}",
            rig.ctl.request("status", json!({})).await,
            workspace_lines(&rig)
                .into_iter()
                .filter(|line| line.id == message_id)
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let screen = wait_for_doorbell(&rig, &pane, &message_id).await;
    assert!(screen.contains(&compact_doorbell(&rig, &message_id)));
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    assert_eq!(
        notification_attempts(&rig, &message_id),
        BTreeSet::from([attempt])
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        1
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Writing),
        1
    );

    rig.daemon.shutdown().await;
}

/// Poll the roster until the pane's fused verdict carries exactly `block`
/// as its write block (`None` for write-allowed or an unstamped refusal).
async fn wait_for_pane_write_block(rig: &mut Rig, pane: &str, block: Option<&str>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let current = status["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|session| session["panes"].as_array())
            .flatten()
            .find(|row| row["pane_id"] == pane)
            .map(|row| row["write_block"].as_str().map(str::to_string));
        if current == Some(block.map(str::to_string)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane} never carried write block {block:?}: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One in-process hook report for the labeled `worker` pane.
async fn report_hook(rig: &Rig, event: &str, seq: u64, payload: Value) -> Value {
    rig.daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "worker",
                "event": event,
                "seq": seq,
                "payload": payload
            }))
            .unwrap(),
        )
        .await
        .expect("hook report accepted")
}

/// Hook admission recovery, restart truth: an old SessionStart and an
/// unclosed prompt are never replayed after a daemon restart; the clean
/// restarted pane is unknown under `hook_admission_unproven`; a send there
/// records one named durable pre-write block with zero writing facts and
/// zero pane bytes; and one SessionStart from the current boot reopens that
/// exact attempt exactly once. A duplicate SessionStart before the restart
/// is duplicate-safe and leaves the pane admitted once.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_pane_parks_the_wake_as_hook_admission_unproven_until_a_current_boot_edge() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-restart",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    // A clean composer with no admitting edge from this boot is unknown
    // under a named block, never idle.
    wait_for_pane_write_block(&mut rig, &pane, Some("hook_admission_unproven")).await;
    wait_pane_state(&mut rig, "unknown").await;

    // One SessionStart from this boot admits the pane. The same report
    // again is a duplicate and leaves it admitted exactly as before.
    let first = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(first["applied"], true, "{first}");
    wait_pane_state(&mut rig, "idle").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;
    let duplicate = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(duplicate["applied"], false, "{duplicate}");
    wait_pane_state(&mut rig, "idle").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;

    // A turn opens and never closes before the daemon restarts.
    let prompt = report_hook(
        &rig,
        "UserPromptSubmit",
        2,
        json!({
            "prompt": "a turn that never closes",
            "session_id": "session-1",
            "turn_id": "turn-1"
        }),
    )
    .await;
    assert_eq!(prompt["applied"], true, "{prompt}");
    assert_eq!(prompt["state"], "working", "{prompt}");

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    // Restart truth: neither the old SessionStart nor the unclosed prompt
    // is replayed. The pane is unknown under the block, not idle and not
    // working.
    wait_for_pane_write_block(&mut rig, &pane, Some("hook_admission_unproven")).await;
    wait_pane_state(&mut rig, "unknown").await;
    let before = rig.tmux.capture(&pane);

    let sent =
        send_workspace_message(&rig, "hook-admission-restart", "Restart", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::BlockedPreWrite).await;
    let blocked = notification_transition(&rig, &message_id, NotificationState::BlockedPreWrite)
        .expect("the hook admission block is durable");
    let fact = blocked.data.as_ref().expect("blocked transition data");
    assert_eq!(fact["pre_write_cause"], "write_readiness_changed");
    assert_eq!(
        fact["pre_write_observation"]["write_block"],
        "hook_admission_unproven"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Writing),
        0
    );
    assert_eq!(
        rig.tmux.capture(&pane),
        before,
        "a hook admission block writes zero pane bytes"
    );
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let notification = &snapshot["result"]["rows"][0]["recipients"][0]["notification"];
    // The wire spells a blocked pre-write as gating plus its cause fields.
    assert_eq!(notification["state"], "gating");
    assert_eq!(notification["pre_write_block"], "hook_admission_unproven");
    assert_eq!(notification["pre_write_cause"], "write_readiness_changed");

    // A SessionStart from the current boot reopens that exact attempt once.
    let again = report_hook(&rig, "SessionStart", 3, json!({"session_id": "session-2"})).await;
    assert_eq!(again["applied"], true, "{again}");
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Gating),
        2,
        "one admitting edge must reopen the same attempt exactly once"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        1
    );
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    wait_for_doorbell(&rig, &pane, &message_id).await;

    rig.daemon.shutdown().await;
}

/// A configured UserPromptSubmit start records liveness for the pane, but
/// an open turn over a clean composer is Working, not idle, until a valid
/// terminal edge closes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_prompt_submit_records_liveness_but_stays_working_until_a_terminal_edge() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-prompt",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_for_pane_write_block(&mut rig, &pane, Some("hook_admission_unproven")).await;

    let prompt = report_hook(
        &rig,
        "UserPromptSubmit",
        1,
        json!({
            "prompt": "a real turn",
            "session_id": "session-1",
            "turn_id": "turn-1"
        }),
    )
    .await;
    assert_eq!(prompt["applied"], true, "{prompt}");
    assert_eq!(prompt["state"], "working", "{prompt}");
    wait_pane_state(&mut rig, "working").await;
    // The clean composer under the open turn must not be admitted as idle
    // by the liveness the prompt just recorded.
    tokio::time::sleep(Duration::from_millis(300)).await;
    wait_pane_state(&mut rig, "working").await;

    let stop = report_hook(
        &rig,
        "Stop",
        2,
        json!({"session_id": "session-1", "turn_id": "turn-1"}),
    )
    .await;
    assert_eq!(stop["applied"], true, "{stop}");
    // Idle by the confirmed end. This fixture has no lifecycle idle rule,
    // so the hook-derived idle keeps its own named write block; write
    // readiness after a turn is the manifest's contract, not this one.
    wait_pane_state(&mut rig, "idle").await;

    rig.daemon.shutdown().await;
}

/// A working pane may accept a notification only when a fresh screen capture
/// positively proves that its composer is clean. Runtime `Working` is not a
/// blanket refusal, and it is not permission: an ambiguous or missing
/// composer proof still follows the fail-closed path.
#[tokio::test(flavor = "multi_thread")]
async fn a_working_pane_with_a_proven_clean_composer_submits_one_doorbell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let event_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "working-clean-composer-submit-event-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&event_dir).unwrap();
    let _event_guard = HomeGuard(event_dir.clone());
    let submit_event_path = event_dir.join("submit.sock");
    let submit_events =
        UnixDatagram::bind(&submit_event_path).expect("bind fake composer submit event socket");
    let pane_command = format!(
        "python3 {} --manual-lifecycle --submit-event-socket {}",
        faketui_path(),
        submit_event_path.display()
    );
    let mut rig = Rig::new(
        "workspace-working-clean-composer",
        LIVENESS_MANIFEST,
        &pane_command,
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let start = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(start["applied"], true, "{start}");
    wait_for_pane_write_block(&mut rig, &pane, None).await;
    wait_pane_state(&mut rig, "idle").await;

    // The fixture keeps its clean prompt visible while exposing a distinct
    // Working row. The screen semantic is therefore positive even though
    // the runtime winner is Working.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-t"]);
    wait_pane_state(&mut rig, "working").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent = send_workspace_message(
        &rig,
        "working-clean-composer",
        "Working clean composer",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    wait_for_doorbell(&rig, &pane, &message_id).await;
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");

    // This is the deterministic post-paste observation that reproduces the
    // final-Enter regression.
    refresh_exact_working_doorbell(&mut rig, &pane).await;
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Submitted).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        1,
        "the exact doorbell must reserve and send only one Enter"
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::AttentionRequired),
        0,
        "a Working pane with a positively clean composer must not become verify_failed"
    );
    let mut event = [0_u8; 16];
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the fake composer did not receive Enter")
        .expect("read fake composer submit event");
    assert_eq!(
        &event[..received],
        b"submit",
        "the fake composer must report the submitted Enter"
    );
    // Shutdown drains or aborts all delivery workers before this test-owned
    // terminal checkpoint. The fixture receives terminal input in order, so a
    // duplicate Enter already queued by Cyclops would report `submit` before
    // the checkpoint. No timer or filesystem poll is needed to prove it.
    rig.daemon.shutdown().await;
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-q"]);
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the fake composer did not acknowledge the terminal checkpoint")
        .expect("read fake composer checkpoint event");
    assert_eq!(
        &event[..received],
        b"checkpoint",
        "the fake composer received an extra Enter before the shutdown checkpoint"
    );
}

/// The Working exception belongs only to the exact pre-admitted doorbell. If
/// a person changes those bytes after fusion has re-observed them under the
/// running turn, the final exact capture must still withhold Enter.
#[tokio::test(flavor = "multi_thread")]
async fn a_human_edit_after_a_working_clean_stage_withholds_enter() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let event_dir = cyclops_proto::scratch::scratch_dir(&format!(
        "working-clean-composer-edit-event-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&event_dir).unwrap();
    let _event_guard = HomeGuard(event_dir.clone());
    let submit_event_path = event_dir.join("submit.sock");
    let submit_events =
        UnixDatagram::bind(&submit_event_path).expect("bind fake composer submit event socket");
    let pane_command = format!(
        "python3 {} --manual-lifecycle --submit-event-socket {}",
        faketui_path(),
        submit_event_path.display()
    );
    let mut rig = Rig::new(
        "workspace-working-clean-composer-edit",
        LIVENESS_MANIFEST,
        &pane_command,
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let start = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(start["applied"], true, "{start}");
    wait_for_pane_write_block(&mut rig, &pane, None).await;
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-t"]);
    wait_pane_state(&mut rig, "working").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent = send_workspace_message(
        &rig,
        "working-clean-composer-edit",
        "Working clean composer edit",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    wait_for_doorbell(&rig, &pane, &message_id).await;
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");

    // Re-observe the untouched exact doorbell first, so this test exercises
    // the same StagedDuringTurn state as the Working success path.
    refresh_exact_working_doorbell(&mut rig, &pane).await;

    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, " trailing input"]);
    rig.tmux.wait_screen("main", "trailing input");
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    let attention =
        notification_transition(&rig, &message_id, NotificationState::AttentionRequired)
            .expect("durable attention transition");
    assert_eq!(attention.data.unwrap()["cause"], "verify_failed");
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        0,
        "a human edit must withhold Enter"
    );

    // After shutdown has drained the worker, a checkpoint as the first fake
    // terminal event proves the changed composer never received Enter.
    rig.daemon.shutdown().await;
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-q"]);
    let mut event = [0_u8; 16];
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the fake composer did not acknowledge the terminal checkpoint")
        .expect("read fake composer checkpoint event");
    assert_eq!(
        &event[..received],
        b"checkpoint",
        "the fake composer received Enter before the human-edit checkpoint"
    );
}

/// A current screen Working row cannot mask a late lifecycle end. The staged
/// doorbell is still exact, but the retained Hook Idle says this is no longer
/// the conflict-free Working frame that admitted the ordinary Enter.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_stop_after_a_working_clean_stage_withholds_enter() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let event_dir = cyclops_proto::scratch::scratch_dir(&format!("wcs-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&event_dir).unwrap();
    let _event_guard = HomeGuard(event_dir.clone());
    let submit_event_path = event_dir.join("submit.sock");
    let submit_events =
        UnixDatagram::bind(&submit_event_path).expect("bind fake composer submit event socket");
    let pane_command = format!(
        "python3 {} --manual-lifecycle --submit-event-socket {}",
        faketui_path(),
        submit_event_path.display()
    );
    let mut rig = Rig::new(
        "workspace-working-clean-late-stop",
        LIVENESS_MANIFEST,
        &pane_command,
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let start = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(start["applied"], true, "{start}");
    wait_for_pane_write_block(&mut rig, &pane, None).await;
    let prompt = report_hook(
        &rig,
        "UserPromptSubmit",
        2,
        json!({
            "prompt": "the existing turn",
            "session_id": "session-1",
            "turn_id": "turn-1"
        }),
    )
    .await;
    assert_eq!(prompt["applied"], true, "{prompt}");
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-t"]);
    wait_pane_state(&mut rig, "working").await;
    wait_for_pane_write_block(&mut rig, &pane, None).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_submit" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent =
        send_workspace_message(&rig, "working-clean-late-stop", "Late stop", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("doorbell reached the pre-submit pause")
        .expect("pause sender stayed open");
    wait_for_doorbell(&rig, &pane, &message_id).await;

    // Force the same post-paste Working observation that the success test
    // proves. A later Stop must still make Enter unsafe after the barrier
    // becomes StagedDuringTurn.
    refresh_exact_working_doorbell(&mut rig, &pane).await;

    // Keep the fixture's screen row Working, but publish an exact Stop for
    // the active turn. Fusion must retain that Hook Idle as a conflict rather
    // than trusting the raw screen winner alone.
    let stop = report_hook(
        &rig,
        "Stop",
        3,
        json!({"session_id": "session-1", "turn_id": "turn-1"}),
    )
    .await;
    assert_eq!(stop["applied"], true, "{stop}");
    assert_eq!(
        stop["state"], "idle",
        "the hook end must reach fusion: {stop}"
    );
    assert!(
        rig.tmux.capture(&pane).contains("FAKETUI-WORKING"),
        "the fixture must keep the raw screen Working while fusion retains Hook Idle"
    );
    release.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::AttentionRequired).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Submitted),
        0,
        "a retained hook Stop must withhold Enter"
    );
    assert!(
        rig.tmux
            .capture(&pane)
            .contains(&compact_doorbell(&rig, &message_id)),
        "the withheld doorbell remains available for inspection"
    );

    // The fixture parses Ctrl-Q in terminal order. Once daemon shutdown has
    // drained delivery workers, a checkpoint as the first event proves the
    // fake composer received no Enter before it.
    rig.daemon.shutdown().await;
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-q"]);
    let mut event = [0_u8; 16];
    let received = tokio::time::timeout(Duration::from_secs(5), submit_events.recv(&mut event))
        .await
        .expect("the fake composer did not acknowledge the terminal checkpoint")
        .expect("read fake composer checkpoint event");
    assert_eq!(
        &event[..received],
        b"checkpoint",
        "the fake composer received Enter before the withheld-delivery checkpoint"
    );
}

/// An opt-in stale reminder is another run of the ordinary notification
/// worker, not a second injection path. It reuses the exact attempt locator,
/// passes through the same composer gate, and spends one durable allowance.
#[tokio::test(flavor = "multi_thread")]
async fn an_unclaimed_doorbell_reminds_once_through_the_ordinary_gate() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-unclaimed-reminder",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\nunclaimed_reminder_ms = 100\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let sent = send_workspace_message(
        &rig,
        "unclaimed-reminder",
        "Reminder envelope",
        "private body never reaches the pane",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    let doorbell = compact_doorbell(&rig, &message_id);

    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let lines = workspace_lines(&rig);
        let reminder_facts = lines
            .iter()
            .filter(|line| {
                line.id == message_id
                    && line.data.as_ref().is_some_and(|data| {
                        data["type"] == "notification_unclaimed_reminder_queued"
                    })
            })
            .count();
        let notified = notification_state_count(&rig, &message_id, NotificationState::Notified);
        if reminder_facts == 1 && notified == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "one reminder did not complete: reminder_facts={reminder_facts}, notified={notified}, lines={lines:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let history = pane_history(&rig, &pane);
    assert!(history.contains(&doorbell), "{history}");
    assert_eq!(
        history.matches("FAKETUI-WORKING").count(),
        2,
        "the original doorbell and its reminder must each execute once: {history}"
    );
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    for state in [
        NotificationState::Writing,
        NotificationState::Staged,
        NotificationState::Submitted,
        NotificationState::Notified,
    ] {
        assert_eq!(
            notification_state_count(&rig, &message_id, state),
            2,
            "both writes must stay on the exact attempt and complete {state:?} once"
        );
    }
    assert!(!history.contains("Reminder envelope"), "{history}");
    assert!(
        !history.contains("private body never reaches the pane"),
        "{history}"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        workspace_lines(&rig)
            .iter()
            .filter(|line| {
                line.id == message_id
                    && line.data.as_ref().is_some_and(|data| {
                        data["type"] == "notification_unclaimed_reminder_queued"
                    })
            })
            .count(),
        1,
        "the exact attempt has one durable reminder allowance"
    );
    assert_eq!(
        pane_history(&rig, &pane).matches("FAKETUI-WORKING").count(),
        2
    );

    rig.daemon.shutdown().await;
}

/// A human draft is a hold, never a hook admission block, and an admitting
/// edge arriving over the draft writes nothing and touches nothing. The
/// human's own submit is what frees the composer; the edge already
/// recorded then admits the clean composer.
#[tokio::test(flavor = "multi_thread")]
async fn a_human_draft_is_untouched_by_hook_admission_recovery() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-draft",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_for_pane_write_block(&mut rig, &pane, Some("hook_admission_unproven")).await;

    rig.tmux
        .run_ok(&["send-keys", "-l", "-t", &pane, "human draft stays private"]);
    rig.tmux.wait_screen("main", "human draft stays private");
    wait_for_human_composer_evidence(&mut rig, &pane).await;

    let sent = send_workspace_message(&rig, "hook-admission-draft", "Draft", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    rig.ev
        .wait_event(Duration::from_secs(8), |event| {
            event["event"] == "gate"
                && event["data"]["id"] == message_id.as_str()
                && event["data"]["action"] == "hold"
        })
        .await;

    let start = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(start["applied"], true, "{start}");
    // Hook reporting completes its route-evidence reconciliation before it
    // returns. The pane-level refusal remains the human-input verdict here;
    // `composer_hold` belongs to a durable blocked notification, not this
    // still-gating attempt.
    let held = rig.tmux.capture(&pane);
    assert!(held.contains("human draft stays private"));
    assert!(!held.contains(&compact_doorbell(&rig, &message_id)));
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Writing),
        0
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        0,
        "a human draft is a hold, never a hook admission block"
    );

    rig.tmux.run_ok(&["send-keys", "-t", &pane, "Enter"]);
    wait_for_pane_write_block(&mut rig, &pane, None).await;
    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    let released = wait_for_doorbell(&rig, &pane, &message_id).await;
    assert!(!released.contains("private body"));
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);

    rig.daemon.shutdown().await;
}

/// An admitting edge that lands after the gate refused and before the
/// block is durable is reconciled once the append exists: the attempt
/// reopens once and is never stranded behind a block nothing will clear.
#[tokio::test(flavor = "multi_thread")]
async fn an_admitting_edge_racing_the_block_append_does_not_strand_the_attempt() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-race",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_for_pane_write_block(&mut rig, &pane, Some("hook_admission_unproven")).await;

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&hold);
    rig.daemon.set_inject_pause(move |phase| {
        let entered_tx = entered_tx.clone();
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if phase != "pre_prewrite_block" {
                return;
            }
            let _ = entered_tx.send(());
            pause.acquire_owned().await.unwrap().forget();
        })
    });

    let sent = send_workspace_message(&rig, "hook-admission-race", "Race", "private body").await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("the refused attempt reached the pre-append pause")
        .expect("pause sender stayed open");

    // The edge lands while the block is decided but not yet durable.
    let start = report_hook(&rig, "SessionStart", 1, json!({"session_id": "session-1"})).await;
    assert_eq!(start["applied"], true, "{start}");
    wait_pane_state(&mut rig, "idle").await;
    hold.add_permits(1);

    wait_for_notification_state(&mut rig, &message_id, NotificationState::Writing).await;
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::BlockedPreWrite),
        1
    );
    assert_eq!(
        notification_state_count(&rig, &message_id, NotificationState::Gating),
        2,
        "the racing edge must reopen the same attempt exactly once"
    );
    assert_eq!(notification_attempts(&rig, &message_id).len(), 1);
    wait_for_doorbell(&rig, &pane, &message_id).await;

    rig.daemon.shutdown().await;
}

/// A recipient claim leaves the operator notification at the FIFO head. An
/// exact administrator withdrawal releases it without changing the claimed
/// mailbox message, then the next pending notification may advance.
#[tokio::test(flavor = "multi_thread")]
async fn claim_preserves_and_exact_withdrawal_releases_a_hook_admission_notification() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "workspace-hook-admission-release",
        LIVENESS_MANIFEST,
        &composer_pane(),
        "delivery_retry_max = 0\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_for_pane_write_block(&mut rig, &pane, Some("hook_admission_unproven")).await;

    let pair = send_waiting_pair(&rig, "hook-admission-release").await;
    let third =
        send_workspace_message(&rig, "hook-admission-release-third", "Third", "third body").await;
    let third_id = third["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &pair.first, NotificationState::BlockedPreWrite).await;
    assert_only_oldest_attempt_exists(&rig, &pair);
    assert!(notification_attempts(&rig, &third_id).is_empty());

    // The recipient claims the backend payload, but the independent operator
    // notification remains the exact FIFO owner.
    rig.daemon
        .claim_message_for_test("worker", &pair.first)
        .expect("exact recipient claim");
    assert!(notification_attempts(&rig, &pair.second).is_empty());
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let rows = snapshot["result"]["rows"].as_array().unwrap();
    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row["message_id"] == id)
            .expect("message row survives")
    };
    let first_row = row_of(&pair.first);
    assert_eq!(first_row["recipients"][0]["mailbox"]["status"], "claimed");
    assert_eq!(
        first_row["recipients"][0]["notification"]["settlement"],
        serde_json::Value::Null
    );
    assert_eq!(
        first_row["recipients"][0]["notification"]["pre_write_block"],
        "hook_admission_unproven"
    );
    let first_recipient = first_row["recipients"][0]["recipient"].clone();
    let first_blocked =
        notification_transition(&rig, &pair.first, NotificationState::BlockedPreWrite)
            .expect("the first block is durable");
    let first_attempt = first_blocked
        .data
        .as_ref()
        .expect("blocked transition data")["attempt_id"]
        .clone();
    let first_withdrawn = rig
        .ctl
        .request(
            "notification.withdraw",
            json!({"attempt_id": first_attempt, "recipient": first_recipient}),
        )
        .await;
    assert!(first_withdrawn["error"].is_null(), "{first_withdrawn}");

    wait_for_notification_state(&mut rig, &pair.second, NotificationState::BlockedPreWrite).await;
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let rows = snapshot["result"]["rows"].as_array().unwrap();
    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row["message_id"] == id)
            .expect("message row survives")
    };
    let second_row = row_of(&pair.second);
    // The wire spells a blocked pre-write as gating plus its cause fields.
    assert_eq!(
        second_row["recipients"][0]["notification"]["state"],
        "gating"
    );
    assert_eq!(
        second_row["recipients"][0]["notification"]["pre_write_block"],
        "hook_admission_unproven"
    );
    let recipient = second_row["recipients"][0]["recipient"].clone();
    assert!(notification_attempts(&rig, &third_id).is_empty());

    // An administrator withdraws the exact blocked attempt: the message
    // stays pending, that attempt is withdrawn, and the FIFO moves on.
    let blocked = notification_transition(&rig, &pair.second, NotificationState::BlockedPreWrite)
        .expect("the second block is durable");
    let attempt_id = blocked.data.as_ref().expect("blocked transition data")["attempt_id"].clone();
    let withdrawn = rig
        .ctl
        .request(
            "notification.withdraw",
            json!({"attempt_id": attempt_id, "recipient": recipient}),
        )
        .await;
    assert!(withdrawn["error"].is_null(), "{withdrawn}");
    assert_eq!(
        withdrawn["result"]["disposition"], "withdrawn",
        "{withdrawn}"
    );
    wait_for_notification_state(&mut rig, &third_id, NotificationState::BlockedPreWrite).await;
    let snapshot = rig.ctl.request("messages.snapshot", json!({})).await;
    let rows = snapshot["result"]["rows"].as_array().unwrap();
    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row["message_id"] == id)
            .expect("message row survives")
    };
    let second_row = row_of(&pair.second);
    assert_eq!(second_row["recipients"][0]["mailbox"]["status"], "pending");
    // The wire spells an operator withdrawal as not_started plus the flag.
    assert_eq!(
        second_row["recipients"][0]["notification"]["state"],
        "not_started"
    );
    assert_eq!(
        second_row["recipients"][0]["notification"]["operator_withdrawn"],
        true
    );
    let third_row = row_of(&third_id);
    assert_eq!(
        third_row["recipients"][0]["notification"]["pre_write_block"],
        "hook_admission_unproven"
    );
    assert_eq!(notification_attempts(&rig, &pair.second).len(), 1);
    assert_eq!(notification_attempts(&rig, &third_id).len(), 1);

    rig.daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn decoy_replacement_generation_does_not_inherit_prior_occupant_composer_hold() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let ghost = r#"\033[1m\342\200\272\033[0m \033[2mFind and fix a bug in @filename\033[0m"#;
    let typed = r#"\033[1m\342\200\272\033[0m fix the rate limiter in gateway.rs"#;
    let pane_command = format!(
        "sh -c 'printf \"{ghost}\\n\"; read a; printf \"\\033[2J\\033[H\"; \
         printf \"{typed}\\n\"; exec cat'"
    );
    let mut rig = Rig::new(
        "composer-hold-decoy-generation",
        ESC_COMPOSER_MANIFEST,
        &pane_command,
        "receipt_block_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    let initial_pid = pane_pid(&rig, &pane);
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    // First occupant types input, entering staged hold.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    wait_pane_state(&mut rig, "idle_with_input").await;

    let sent = send_workspace_message(
        &rig,
        "composer-hold-decoy-generation",
        "Held for first occupant",
        "private body",
    )
    .await;
    let message_id = sent["msg_id"].as_str().unwrap().to_string();
    wait_for_notification_state(&mut rig, &message_id, NotificationState::BlockedPreWrite).await;
    let blocked = notification_transition(&rig, &message_id, NotificationState::BlockedPreWrite)
        .expect("notification reached BlockedPreWrite");
    let blocked_data = blocked.data.as_ref().expect("blocked transition data");

    // Withdraw first message using direct resolved-admin seam so FIFO is clear for replacement message.
    let snapshot = json!({
        "result": serde_json::to_value(
            rig.daemon
                .messages_snapshot_for_test("admin", 20)
                .expect("admin messages snapshot"),
        )
        .expect("messages snapshot serializes")
    });
    let first = snapshot["result"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["message_id"].as_str() == Some(message_id.as_str()))
        .expect("first message exists");
    let recipient_key = serde_json::from_value(first["recipients"][0]["recipient"].clone())
        .expect("blocked recipient key");
    let attempt_id =
        serde_json::from_value(blocked_data["attempt_id"].clone()).expect("blocked attempt id");
    let withdrawn = serde_json::to_value(
        rig.daemon
            .withdraw_notification_for_test("admin", recipient_key, attempt_id)
            .expect("admin notification withdrawal"),
    )
    .expect("withdrawal result serializes");
    assert_eq!(withdrawn["disposition"], "withdrawn", "{withdrawn}");

    // Replace the occupant with a fresh process tree in the same pane using respawn-pane.
    let clean_pane_command = format!("sh -c 'printf \"{ghost}\\n\"; exec cat'");
    rig.tmux
        .run_ok(&["respawn-pane", "-k", "-t", &pane, &clean_pane_command]);
    let replacement_pid = wait_for_pane_pid_change(&rig, &pane, initial_pid).await;
    assert_ne!(replacement_pid, initial_pid);
    wait_pane_state(&mut rig, "idle").await;

    // Send a new message to the replacement occupant.
    let replacement_sent = send_workspace_message(
        &rig,
        "composer-hold-decoy-generation-replacement",
        "For replacement occupant",
        "second private body",
    )
    .await;
    let replacement_id = replacement_sent["msg_id"].as_str().unwrap().to_string();

    // The replacement occupant generation starts with a clean hold and proceeds to write without being blocked by prior occupant's hold.
    wait_for_notification_state(&mut rig, &replacement_id, NotificationState::Writing).await;

    rig.daemon.shutdown().await;
}
