//! The gate, the write boundary, and the receipt against a real tmux server
//! on an isolated `-L` socket (rig in tests/common).
//!
//! Every send here goes through the durable mailbox as the operator; the
//! pane sees one Format 4 doorbell row. What these tests protect is the
//! terminal safety contract around that row: who may be written to, when
//! the submit key may follow, and what proves the row was consumed.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

const ESC_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Codex esc fixture"
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

/// Binds ONLY the composer process, so an occupant swap to a plain shell
/// leaves the gate unable to bind at all. That is the point of it: the
/// rebind tests need a manifest that stops matching the moment the pane
/// changes hands.
const COMPOSER_ONLY_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Fake-TUI fixture, single binding"
process_names = ["python3", "python", "Python"]

[[rule]]
id = "composer_has_input"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 200
region = "bottom_non_empty_lines(8)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
composer_trailer_regex = ['^─+$', '^Model \S+ · Ctx: \d+%$']
composer_trailer_regex_esc = ['^\x1b\[38;5;244m─', '^\x1b\[38;5;152mModel\b']
composer_trailer_required_prefix = 2
composer_prompt_regex = '^❯ ?(?P<content>.*)$'
composer_continuation_regex = '^(?P<content>.*)$'
"#;

/// Install an inject-pause seam that parks the worker at `phase` and
/// reports each entry; the returned semaphore releases one pass.
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

async fn wait_pane_command_is(rig: &mut Rig, want: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = rig.ctl.request("status", json!({})).await;
        let cmd = resp["result"]["sessions"][0]["panes"][0]["current_command"].clone();
        if want.iter().any(|w| cmd == json!(w)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane command never became one of {want:?}: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Rename the session away and detach the daemon's control client: a
/// scripted, held-open detach. Rename back to let it reattach.
fn scripted_detach(rig: &Rig, session: &str, hidden: &str) {
    rig.tmux.run_ok(&["rename-session", "-t", session, hidden]);
    rig.tmux.run_ok(&["detach-client", "-s", hidden]);
}

fn scripted_reattach(rig: &Rig, hidden: &str, session: &str) {
    rig.tmux.run_ok(&["rename-session", "-t", hidden, session]);
}

/// The exact hook acknowledgement for one message: the Format 4 row the
/// daemon pasted, reported through the trusted in-process path because the
/// socket path is pinned to the pane's process ancestry and this test
/// process lives outside every pane.
async fn report_ack(rig: &Rig, agent: &str, message_id: &str, seq: u64, turn: &str) -> Value {
    rig.daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": agent,
                "event": "UserPromptSubmit",
                "seq": seq,
                "payload": {
                    "prompt": doorbell_for(rig, message_id),
                    "session_id": "s1",
                    "turn_id": turn,
                },
            }))
            .unwrap(),
        )
        .await
        .expect("report ok")
}

/// Tier 1: the hook acknowledgement inside the window resolves the
/// notification; a late one after a screen receipt still counts, and an
/// exact duplicate is deduplicated.
#[tokio::test(flavor = "multi_thread")]
async fn tier1_hook_ack_and_late_upgrade() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "hook",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 300\nack_timeout_ms = 800\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "one", "body": "a\nb\nc"}))
        .await;
    let m1 = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&rig, &m1, &["submitted", "submitted_unverified"]).await;
    let resp = report_ack(&rig, "hooky", &m1, 1, "t1").await;
    assert_eq!(resp["applied"], true, "{resp}");
    assert_eq!(resp["matched"], true, "{resp}");
    wait_notification_state(&rig, &m1, &["notified"]).await;

    // Exact duplicate (same session, turn, event): deduped.
    let dup = report_ack(&rig, "hooky", &m1, 2, "t1").await;
    assert_eq!(dup["duplicate"], true, "{dup}");

    // Close the turn the acknowledgement opened before the next message,
    // so the next gate does not race a hook reading against an idle screen.
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
    // The recipient claims the first message; only then is the next
    // doorbell for this mailbox scheduled.
    claim(&rig, "hooky", &m1);

    // Message 2: no hook inside the window, so the screen tier resolves
    // it; a late matching acknowledgement still matches the exact row.
    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "two", "body": "a\nb\nc"}))
        .await;
    let m2 = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&rig, &m2, &["notified"]).await;
    let resp = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "hooky",
                "event": "user_prompt_submit",
                "seq": 4,
                "payload": {
                    "prompt": doorbell_for(&rig, &m2),
                    "session_id": "s1",
                    "turn_id": "t2",
                },
            }))
            .unwrap(),
        )
        .await
        .expect("report ok");
    assert_eq!(resp["matched"], true, "{resp}");
    assert_eq!(
        notification_states(&rig, &m2)
            .iter()
            .filter(|state| *state == "writing")
            .count(),
        1,
        "the late acknowledgement must not write again"
    );
    rig.shutdown().await;
}

/// The screen is re-read before the FINAL confirming key of a multi-key
/// decline; if the modal changed under the sequence the confirm is
/// withheld and the gate re-evaluates. The pane clears its dialog the
/// instant the first key arrives, so an unchecked sequence would type
/// Enter into whatever replaced it.
#[tokio::test(flavor = "multi_thread")]
async fn decline_aborts_when_the_modal_changes_between_keys() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Raw tty so dd returns on exactly one byte (the first decline key
    // "3"); the dialog then vanishes and the pane becomes the composer.
    let tail = composer_tail();
    let script = &format!("sh -c 'echo FAKE-UPDATE-AVAILABLE; stty -icanon -echo min 1 time 0; dd bs=1 count=1 >/dev/null 2>&1; stty sane; printf \"\\033[2J\\033[H\"; {tail}'");
    let mut rig = Rig::new("toctou", MODAL_MANIFEST, script, "receipt_block_ms = 200\n").await;
    rig.tmux.wait_screen("main", "FAKE-UPDATE-AVAILABLE");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "flappy").await;

    let (result, _) = rig
        .send(json!({"to": ["flappy"], "subject": "hello", "body": "x\ny"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();

    let decline = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "decline"
        })
        .await;
    assert_eq!(decline["data"]["rule"], "fake_update_modal");
    // The confirming Enter was withheld and the abort is on the record.
    let aborted = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "decline_aborted"
        })
        .await;
    assert_eq!(aborted["data"]["cause"], "modal_changed", "{aborted}");
    // The gate re-read reality (the pane is now a clean composer) and
    // the doorbell landed.
    wait_notification_state(&rig, &msg_id, &["notified", "submitted_unverified"]).await;
    rig.assert_ledger_legal(&["FAKE-UPDATE-AVAILABLE"]);
    rig.shutdown().await;
}

/// A notification held in gating past `gate_hold_notify_ms` pings the
/// admin exactly once; the hold itself keeps waiting on events. A running
/// turn admits a doorbell, so the hold that lasts here is copy-mode: the
/// human is reading and nothing may be written until they leave it.
#[tokio::test(flavor = "multi_thread")]
async fn long_gate_hold_notifies_the_admin_once() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "holdn",
        CAT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 100\ngate_hold_notify_ms = 300\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "held").await;
    wait_pane_state(&mut rig, "idle").await;
    rig.tmux.run_ok(&["copy-mode", "-t", &pane]);
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "readiness"
                && e["data"]["pane_id"] == pane.as_str()
                && e["data"]["write_block"] == "pane_in_mode"
        })
        .await;

    let (result, _) = rig
        .send(json!({"to": ["held"], "subject": "wedged", "body": "b"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    let notify = rig
        .ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "admin-notify"
                && e["data"]["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("held in gating"))
        })
        .await;
    assert_eq!(notify["data"]["level"], "action_required", "{notify}");
    assert!(notify["data"]["body"].as_str().unwrap().contains(&msg_id));
    assert_eq!(notify["data"]["pane_id"], pane.as_str(), "{notify}");
    assert_eq!(notify["data"]["id"], msg_id.as_str(), "{notify}");

    // Release: the held notification proceeds on the pane edge.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "q"]);
    wait_notification_state(&rig, &msg_id, &["notified", "submitted_unverified"]).await;
    let pings = rig
        .ledger_lines()
        .iter()
        .filter(|l| {
            l["kind"] == "system"
                && l["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("held in gating"))
        })
        .count();
    assert_eq!(pings, 1, "the wedged hold pinged more than once");
    rig.shutdown().await;
}

/// A hook report arriving while the session is detached is still accepted
/// (the pane exists in the last-known table) and resolves the in-flight
/// notification, so the recipient never sees two copies of the same row.
#[tokio::test(flavor = "multi_thread")]
async fn hook_ack_during_detach_resolves_the_notification() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "dethook",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 8000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "hooky").await;

    let (result, _) = rig
        .send(json!({"to": ["hooky"], "subject": "over the gap", "body": "a\nb"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&rig, &msg_id, &["submitted", "submitted_unverified"]).await;

    scripted_detach(&rig, "main", "hidden-dethook");
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "session" && e["data"]["attached"] == false
        })
        .await;

    // The recipient's hook fires DURING the outage. The report needs no
    // tmux connection; rejecting it was the duplicate-delivery bug.
    let resp = report_ack(&rig, "hooky", &msg_id, 1, "t").await;
    assert_eq!(resp["applied"], true, "{resp}");
    assert_eq!(resp["matched"], true, "{resp}");
    assert_eq!(resp["live"], false, "{resp}");
    wait_notification_state(&rig, &msg_id, &["notified"]).await;

    scripted_reattach(&rig, "hidden-dethook", "main");
    rig.ev
        .wait_event(Duration::from_secs(15), |e| {
            e["event"] == "session" && e["data"]["attached"] == true
        })
        .await;

    // One paste, one notification. No duplicate after reattach.
    assert_eq!(
        notification_states(&rig, &msg_id)
            .iter()
            .filter(|state| *state == "writing")
            .count(),
        1,
        "the row was pasted more than once"
    );
    rig.shutdown().await;
}

/// ACK deadlines freeze while the session is detached and a reattach runs
/// an evidence pass before any deadline fires. A detach that outlives the
/// whole screen-ACK window must NOT burn the attempt: the notification
/// resolves from post-reattach evidence with a single paste.
#[tokio::test(flavor = "multi_thread")]
async fn ack_deadlines_freeze_across_detach() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "detfrz",
        HOOK_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 100\nack_timeout_ms = 2000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "frozen").await;

    let (result, _) = rig
        .send(json!({"to": ["frozen"], "subject": "freeze me", "body": "a\nb"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&rig, &msg_id, &["submitted", "submitted_unverified"]).await;

    // Detach immediately after submit and hold the outage PAST the entire
    // 5s screen-ACK deadline.
    scripted_detach(&rig, "main", "hidden-detfrz");
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "session" && e["data"]["attached"] == false
        })
        .await;
    tokio::time::sleep(Duration::from_millis(5500)).await;
    scripted_reattach(&rig, "hidden-detfrz", "main");
    rig.ev
        .wait_event(Duration::from_secs(15), |e| {
            e["event"] == "session" && e["data"]["attached"] == true
        })
        .await;

    // Reattach evidence pass: the row demonstrably arrived (staged, then
    // the composer moved on), so the notification resolves without a
    // resubmit and never records an ack timeout.
    let state = wait_notification_state(&rig, &msg_id, &["notified", "attention_required"]).await;
    assert_eq!(
        state,
        "notified",
        "{:?}",
        notification_states(&rig, &msg_id)
    );
    assert_eq!(
        notification_states(&rig, &msg_id)
            .iter()
            .filter(|state| *state == "writing")
            .count(),
        1,
        "the detach burned the ACK window and forced a duplicate paste"
    );
    rig.shutdown().await;
}

/// Two messages back to back through one recipient. Cyclops latches its
/// OWN paste: after the row lands, the pane is holding text exactly the
/// way a person's draft holds it, so the second notification must not
/// paste until a turn has consumed the first, and the turn then has to
/// release it.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_message_waits_for_the_first_turn_and_then_lands() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("fifohold", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let mut ids = Vec::new();
    for subject in ["first", "second"] {
        let (result, _) = rig
            .send(json!({"to": ["worker"], "subject": subject, "body": "a\nb"}))
            .await;
        ids.push(result["msg_id"].as_str().expect("msg id").to_string());
    }

    // Both resolve, in order. The second one landing at all is the
    // assertion that matters: it can only have gated after the first turn
    // released the hold this notification's own paste latched. The
    // recipient claims the first before the second may be scheduled.
    wait_notification_state(&rig, &ids[0], &["notified", "submitted_unverified"]).await;
    claim(&rig, "worker", &ids[0]);
    wait_notification_state(&rig, &ids[1], &["notified", "submitted_unverified"]).await;
    // A regate may admit the same attempt again; what may not happen is
    // the second admitted before the first, or a third message invented.
    let mut proceeds: Vec<String> = rig
        .ledger_lines()
        .iter()
        .filter(|l| l["kind"] == "gate" && l["data"]["action"] == "proceed")
        .filter_map(|l| l["id"].as_str().map(str::to_string))
        .collect();
    proceeds.dedup();
    assert_eq!(proceeds, ids, "admitted in order: {proceeds:?}");
    rig.shutdown().await;
}

/// Write-readiness and runtime state move independently, so a pane can
/// refuse and then allow with no state edge between. Anything waiting on
/// the refusal has to be woken by something, and a `state` line would be
/// a transition that never happened.
#[tokio::test(flavor = "multi_thread")]
async fn a_readiness_change_with_no_state_change_is_still_broadcast() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("readywake", CAT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    // Copy mode refuses the write without touching the runtime state: the
    // pane is idle before and idle after.
    rig.tmux.run_ok(&["copy-mode", "-t", &pane]);
    let ev = rig
        .ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "readiness"
                && e["data"]["session_idx"] == 0
                && e["data"]["pane_id"] == pane.as_str()
                && e["data"]["write_block"] == "pane_in_mode"
        })
        .await;
    assert_eq!(ev["data"]["write_ready"], false, "{ev}");
    assert!(
        ev["seq"].is_null(),
        "a readiness wake names no ledger line: {ev}"
    );
    let status = rig.ctl.request("status", json!({})).await;
    let pane_status = &status["result"]["sessions"][0]["panes"][0];
    assert_eq!(pane_status["state"], "idle", "{status}");
    assert_eq!(pane_status["in_mode"], true, "{status}");
    assert_eq!(pane_status["write_ready"], false, "{status}");
    assert_eq!(pane_status["write_block"], "pane_in_mode", "{status}");

    // And the record does not gain a transition for it.
    assert!(
        !rig.ledger_lines()
            .iter()
            .any(|l| l["kind"] == "state" && l["data"]["cause"] == "pane_in_mode"),
        "copy mode wrote a state transition"
    );
    rig.shutdown().await;
}

/// A subscriber that stalls briefly while events burst past the buffer
/// survives and reads the whole stream; only a truly wedged client is
/// dropped.
#[tokio::test(flavor = "multi_thread")]
async fn briefly_stalled_subscriber_survives_event_burst() {
    // No tmux needed: a daemon with zero sessions still serves events.
    let home = cyclops_proto::scratch::scratch_dir("cyc-gate-subbuf");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("scratch home");
    let _guard = HomeGuard(home.clone());
    std::fs::write(home.join("config.toml"), "sessions = []\n").expect("config");
    let (cfg, _) = cyclopsd::Config::load(&home).expect("config loads");
    let daemon = cyclopsd::boot(cfg).await.expect("daemon boots");
    let sock = daemon.socket_path();
    let mut ctl = TestClient::connect(&sock).await;
    let mut ev = TestClient::connect(&sock).await;
    let ack = ev.request("events.subscribe", json!({})).await;
    assert_eq!(ack["result"]["subscribed"], true);

    // 1600 events while the subscriber reads nothing: beyond a 1024-event
    // buffer plus whatever the socket absorbed.
    let filler = "x".repeat(400);
    for n in 0..1600 {
        let resp = ctl
            .request(
                "admin.notify",
                json!({"level": "fyi", "subject": format!("burst-{n}"), "body": filler}),
            )
            .await;
        assert!(resp["error"].is_null(), "{resp}");
    }
    ev.wait_event(Duration::from_secs(20), |e| {
        e["event"] == "admin-notify" && e["data"]["subject"] == "burst-1599"
    })
    .await;
    daemon.shutdown().await;
}

/// Only the escaped capture can tell typed text from a ghost suggestion.
/// Typed text holds the gate (human wins); one clean frame afterwards is
/// not admission; a turn is what releases the hold.
#[tokio::test(flavor = "multi_thread")]
async fn escaped_capture_flips_typed_text_to_idle_with_input_and_gates() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Measured composer lines from the F19 ghost probe (fixtures in
    // src/cyclops-manifest/tests/fixtures/): ESC[1m glyph ESC[0m, then
    // dim-wrapped ghost text or bare typed text.
    let ghost = r#"\033[1m\342\200\272\033[0m \033[2mFind and fix a bug in @filename\033[0m"#;
    let typed = r#"\033[1m\342\200\272\033[0m fix the rate limiter in gateway.rs"#;
    let script = format!(
        "sh -c 'printf \"{ghost}\\n\"; read a; printf \"\\033[2J\\033[H\"; \
         printf \"{typed}\\n\"; read b; printf \"\\033[2J\\033[H\"; \
         printf \"{ghost}\\n\"; exec cat'"
    );
    let mut rig = Rig::new("esc", ESC_MANIFEST, &script, "receipt_block_ms = 300\n").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "codexy").await;

    wait_pane_state(&mut rig, "idle").await;
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    wait_pane_state(&mut rig, "idle_with_input").await;

    // A notification against typed text is a durable pre-write block:
    // human wins, and the refusal is recorded rather than waited out.
    let (result, _) = rig
        .send(json!({"to": ["codexy"], "subject": "hold this", "body": "payload-body"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&rig, &msg_id, &["blocked_pre_write"]).await;
    assert!(
        !rig.tmux.capture(&pane).contains("[cyclops"),
        "row pasted over typed human text"
    );

    // The typed text goes away and only ghost text remains, so the pane
    // reads idle again. That is NOT admission: one clean frame does not
    // prove the draft left.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    wait_pane_state(&mut rig, "idle").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !rig.tmux.capture(&pane).contains("[cyclops"),
        "row pasted over a composer still under hold"
    );
    assert_eq!(
        notification_state(&rig, &msg_id).as_deref(),
        Some("blocked_pre_write")
    );

    // A turn is the evidence the hold waits for. Running one, then coming
    // back to a clean composer, releases the hold and admits the row.
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "WORKING the draft"]);
    wait_pane_state(&mut rig, "working").await;
    rig.tmux.run_ok(&["select-pane", "-t", &pane, "-T", "done"]);
    let proceed = rig
        .ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "proceed"
        })
        .await;
    assert_eq!(proceed["data"]["to"], "codexy", "{proceed}");

    // Screen text stays out of the ledger.
    rig.assert_ledger_legal(&["gateway.rs", "Find and fix a bug"]);
    rig.shutdown().await;
}

/// The occupant changes between the gate's admit and the paste. The
/// re-check must catch the rebind and never paste: on a shell occupant
/// the pasted row would be EXECUTED.
#[tokio::test(flavor = "multi_thread")]
async fn pane_rebound_before_paste_never_pastes_into_the_new_occupant() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "rbpaste",
        COMPOSER_ONLY_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 100\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let (mut entered, release) = park_at(&rig, "pre_paste");

    let (result, _) = rig
        .send(json!({"to": ["worker"], "subject": "danger", "body": "rm -rf nothing"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();

    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("paste path reached the seam within 10s")
        .expect("seam channel open");
    rig.tmux.run_ok(&["respawn-pane", "-k", "-t", &pane, "sh"]);
    wait_pane_command_is(&mut rig, &["sh", "bash", "dash"]).await;
    release.add_permits(1);

    let rebound = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "rebound"
        })
        .await;
    assert!(
        matches!(
            rebound["data"]["cause"].as_str(),
            Some("pane_pid_changed" | "route_binding_changed" | "binding_unprovable" | "pane_gone")
        ),
        "{rebound}"
    );

    // THE assertion: nothing was ever typed into the shell, and the
    // durable record never crossed the write boundary.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let screen = rig.tmux.capture(&pane);
    assert!(
        !screen.contains("[cyclops") && !screen.contains("rm -rf"),
        "row reached the rebound shell:\n{screen}"
    );
    assert!(
        !notification_states(&rig, &msg_id)
            .iter()
            .any(|state| state == "writing" || state == "staged"),
        "a rebound pane still took a write: {:?}",
        notification_states(&rig, &msg_id)
    );
    rig.shutdown().await;
}

/// The occupant changes after the paste verified but before the submit
/// key. Enter must never reach the new occupant.
#[tokio::test(flavor = "multi_thread")]
async fn pane_rebound_before_submit_withholds_the_submit_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "rbsub",
        COMPOSER_ONLY_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 100\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let (mut entered, release) = park_at(&rig, "pre_submit");

    let (result, _) = rig
        .send(json!({"to": ["worker"], "subject": "danger", "body": "echo pwned"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();

    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("submit path reached the seam within 10s")
        .expect("seam channel open");
    assert!(
        notification_states(&rig, &msg_id)
            .iter()
            .any(|state| state == "staged"),
        "the pre-swap paste never staged: {:?}",
        notification_states(&rig, &msg_id)
    );

    // Terminal IO must not retain the journal lock or block inspection.
    let status = tokio::time::timeout(Duration::from_secs(2), rig.ctl.request("status", json!({})))
        .await
        .expect("status answers while terminal submit is paused");
    assert_eq!(status["error"], Value::Null, "{status}");

    // Replace the occupant with a plain shell. The daemon re-proves the
    // binding immediately before Enter, so the swap only has to be real
    // by the time the seam releases.
    rig.tmux.run_ok(&["respawn-pane", "-k", "-t", &pane, "sh"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = rig
            .tmux
            .run(&["display", "-p", "-t", &pane, "#{pane_current_command}"]);
        let command = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if ["sh", "bash", "dash"].contains(&command.as_str()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the shell never took the pane: {command}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    release.add_permits(1);

    // THE assertion: the submit key never went out, and the replacement
    // shell never saw the row. The record keeps the staged paste and
    // settles without a submit; whether it lands in attention now or stays
    // held for the daemon's reconnect is not the contract.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let states = notification_states(&rig, &msg_id);
    assert!(
        !states.iter().any(|state| state == "submitted"
            || state == "submitted_unverified"
            || state == "notified"),
        "the submit key was sent to a rebound pane: {states:?}"
    );
    let screen = rig.tmux.capture(&pane);
    assert!(
        !screen.contains("[cyclops") && !screen.contains("echo pwned"),
        "the row reached the replacement shell:\n{screen}"
    );
    rig.shutdown().await;
}

/// The human scrolls (copy-mode) after the gate admitted: the pre-paste
/// re-check refuses and nothing reaches the pane.
#[tokio::test(flavor = "multi_thread")]
async fn pane_mode_entered_after_admission_withholds_the_paste() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "modepaste",
        COMPOSER_ONLY_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 100\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    let (mut entered, release) = park_at(&rig, "pre_paste");

    let (result, _) = rig
        .send(json!({"to": ["worker"], "subject": "scrolling", "body": "not while reading"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();

    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("paste path reached the seam within 10s")
        .expect("seam channel open");
    rig.tmux.run_ok(&["copy-mode", "-t", &pane]);

    // Wait for the DAEMON to see it, not tmux: the pre-paste re-check
    // reads the watcher's pane table.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = rig.ctl.request("status", json!({})).await;
        let seen = resp["result"]["sessions"][0]["panes"]
            .as_array()
            .expect("panes array")
            .iter()
            .any(|p| p["pane_id"] == pane.as_str() && p["in_mode"] == json!(true));
        if seen {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never saw the pane enter copy-mode: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    release.add_permits(1);

    let held = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && matches!(e["data"]["action"].as_str(), Some("rebound" | "hold"))
                && e["data"]["cause"]
                    .as_str()
                    .is_some_and(|c| c.contains("pane_in_mode"))
        })
        .await;
    assert!(
        held["data"]["cause"]
            .as_str()
            .unwrap()
            .contains("pane_in_mode"),
        "{held}"
    );

    // Taken while copy-mode is still up, which is when a stray paste
    // would show.
    let screen = rig.tmux.capture(&pane);
    assert!(
        !screen.contains("[cyclops") && !screen.contains("not while reading"),
        "row reached a pane in copy-mode:\n{screen}"
    );
    assert!(
        !notification_states(&rig, &msg_id)
            .iter()
            .any(|state| state == "staged"),
        "a pane in copy-mode still staged a paste"
    );
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "q"]);
    rig.shutdown().await;
}

/// `daemon.quiesce` is quiet over pre-paste notifications: they are
/// durably queued and the next boot schedules them again.
#[tokio::test(flavor = "multi_thread")]
async fn quiesce_is_quiet_over_prepaste_notifications() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "quiesce",
        BUSY_MANIFEST,
        &hold_script("BUSY-MARKER"),
        "receipt_block_ms = 200\n",
    )
    .await;
    rig.tmux.wait_screen("main", "BUSY-MARKER");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "held").await;

    let (result, _) = rig
        .send(json!({"to": ["held"], "subject": "s", "body": "b"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    wait_notification_state(&rig, &msg_id, &["gating"]).await;

    let resp = rig.ctl.request("daemon.quiesce", json!({})).await;
    assert_eq!(resp["result"]["quiet"], true, "{resp}");
    assert!(
        resp["result"]["in_flight"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{resp}"
    );
    rig.shutdown().await;
}

/// The fake TUI's stream parser owns the one thing a PTY makes hard:
/// telling a pasted payload from the submit key that follows it. Its
/// regressions live with it, and this runs them.
#[test]
fn faketui_stream_parser_selftest() {
    let out = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/common/faketui.py"
        ))
        .arg("--selftest")
        .output()
        .expect("run faketui selftest");
    assert!(
        out.status.success(),
        "parser regressions failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
