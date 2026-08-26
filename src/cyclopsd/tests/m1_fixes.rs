//! M1 gate-blocker regressions: send-and-wait ordering, restart-limbo
//! closure, detach-aware ACKs, argv-fallback manifest binding, decline
//! TOCTOU, cross-session ledger completeness, gate-hold visibility, and
//! subscriber buffering. Same isolated-tmux rig as m1.rs (tests/common).

mod common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};

/// Fix A: send-and-wait must start the wait only after the delivery
/// resolves, and `turn_ended` must not count a working phase that predates the
/// delivery. A busy pane released mid-wait used to satisfy `turn_ended` off the
/// PRE-delivery busy phase, with the delivery still unresolved.
#[tokio::test(flavor = "multi_thread")]
async fn send_and_wait_starts_after_delivery_resolution() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "waita",
        BUSY_MANIFEST,
        &hold_then_manual_lifecycle_script("BUSY-MARKER"),
        "receipt_block_ms = 200\n",
    )
    .await;
    rig.tmux.wait_screen("main", "BUSY-MARKER");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "waity").await;

    // Release the pane 400ms in, while msg.send is blocked on the wait.
    let socket = rig.tmux.socket().to_string();
    let pane_for_release = pane.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let _ = std::process::Command::new("tmux")
            .args(["-u", "-L", &socket, "-f", "/dev/null"])
            .args(["send-keys", "-t", &pane_for_release, "x", "Enter"])
            .status();
    });

    let (result, _) = rig
        .send(json!({
            "to": ["waity"],
            "subject": "wait for me",
            "body": "a\nb\nc",
            "wait": {"until": "turn_ended", "timeout_ms": 2500},
        }))
        .await;
    releaser.join().expect("releaser thread");
    let msg_id = result["msg_id"].as_str().unwrap().to_string();

    // The receipt phase capped out while busy: queued is honest.
    assert_eq!(result["deliveries"][0]["state"], "queued", "{result}");

    // The wait ran only after the delivery resolved: the entry carries the
    // resolved delivery state, and `turn_ended` was NOT satisfied by the
    // pre-delivery busy phase. The manual-lifecycle composer consumes this
    // delivery but emits no post-submit Working edge, so a correctly anchored
    // `turn_ended` times out.
    let wait = &result["wait"][0];
    assert_eq!(wait["to"], "waity", "{result}");
    assert_eq!(wait["delivery"], "delivered_unverified", "{result}");
    assert_eq!(
        wait["outcome"], "timeout",
        "turn_ended was satisfied by a working phase that predates the delivery: {result}"
    );

    // By response time the ledger agrees the delivery resolved.
    assert_eq!(
        rig.final_state(&msg_id, "waity").as_deref(),
        Some("delivered_unverified")
    );

    // Fix G (spool switch): payloads spooled under the 0700 home, cleaned
    // up after use.
    let spool = rig.home.join("spool");
    assert!(spool.is_dir(), "spool dir missing under home");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&spool).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "spool dir must be owner-only");
    }
    assert_eq!(
        std::fs::read_dir(&spool).unwrap().count(),
        0,
        "payload spool files must not linger"
    );

    rig.assert_ledger_legal(&["BUSY-MARKER"]);
    rig.shutdown().await;
}

/// Fix C, amended by the restart requeue: a daemon restart must leave no
/// delivery in limbo, and the pre-write boundary decides how each chain
/// ends. This one was GATING when the daemon died — nothing had touched
/// the pane — so the reboot requeues it (retry_queued, cause
/// daemon_restart), it re-enters the gate on the new run, and no human is
/// summoned for a message that was never at risk. The closure path for
/// chains past the paste, and for recipients no pane answers to, is
/// pinned by `restart_closes_pre_hosted_field_ledger_chains`
/// (m1_blockers) and restart_eye.
#[tokio::test(flavor = "multi_thread")]
async fn restart_requeues_prepaste_deliveries() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "limbo",
        BUSY_MANIFEST,
        &hold_script("BUSY-MARKER"),
        "receipt_block_ms = 200\n",
    )
    .await;
    rig.tmux.wait_screen("main", "BUSY-MARKER");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "limboed").await;

    let (result, _) = rig
        .send(json!({"to": ["limboed"], "subject": "stuck", "body": "b"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    assert_eq!(result["deliveries"][0]["state"], "queued", "{result}");
    rig.ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "gating"
        })
        .await;

    // Stop mid-hold (the delivery is gating, non-terminal) and reboot on
    // the same home.
    let rig = rig.reboot().await;

    // The chain went back in the queue with the restart named as cause
    // (written by close_limbo inside boot, so it is on disk already)…
    let lines = rig.ledger_lines();
    let new_boot_at = lines
        .iter()
        .rposition(|line| line["data"]["event"] == "boot")
        .expect("new boot fact");
    let new_boot = lines[new_boot_at]["boot_id"].clone();
    assert!(
        lines
            .iter()
            .skip(new_boot_at)
            .all(|line| line["boot_id"] == new_boot),
        "an old daemon appended after the replacement booted: {lines:#?}"
    );
    let requeue = lines
        .iter()
        .find(|l| {
            l["kind"] == "state"
                && l["id"] == msg_id.as_str()
                && l["data"]["to_state"] == "retry_queued"
        })
        .expect("no requeue line after reboot");
    assert_eq!(requeue["data"]["cause"], "daemon_restart", "{requeue}");
    // …and no attention closure exists for it: nobody is summoned.
    assert!(
        !lines.iter().any(|l| l["kind"] == "state"
            && l["id"] == msg_id.as_str()
            && l["data"]["to_state"] == "attention_required"),
        "requeue must not also close the chain"
    );
    assert!(
        !lines.iter().any(|l| l["kind"] == "system"
            && l["subject"]
                .as_str()
                .is_some_and(|s| s.contains("interrupted by daemon restart"))),
        "no action-required restart ping for a requeued chain"
    );

    // One FYI names the requeue instead.
    let fyis: Vec<&Value> = lines
        .iter()
        .filter(|l| {
            l["kind"] == "system"
                && l["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("requeued after daemon restart"))
        })
        .collect();
    assert_eq!(fyis.len(), 1, "want one requeue FYI: {fyis:?}");
    assert!(
        fyis[0]["body"].as_str().unwrap().contains(&msg_id),
        "{fyis:?}"
    );

    // The requeued delivery re-enters the gate on the new run. Polled off
    // the ledger, not the event stream: the transition can race the
    // rebooted rig's subscribe, and events do not replay.
    let requeue_seq = requeue["seq"].as_u64().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let lines = rig.ledger_lines();
        let re_entered = lines.iter().any(|l| {
            l["kind"] == "state"
                && l["id"] == msg_id.as_str()
                && l["data"]["to_state"] == "gating"
                && l["seq"].as_u64().unwrap_or(0) > requeue_seq
        });
        if re_entered {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "requeued delivery never re-entered the gate: {lines:#?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    rig.assert_ledger_legal(&["BUSY-MARKER"]);
    rig.shutdown().await;
}

/// `daemon.quiesce` is quiet over pre-paste deliveries: they ride a
/// restart (the requeue above), so only a delivery past the paste may
/// hold up a stop. The one here is held at the gate by a busy pane —
/// mid-flight forever from a sender's view, and still no reason to
/// refuse.
#[tokio::test(flavor = "multi_thread")]
async fn quiesce_is_quiet_over_prepaste_deliveries() {
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
    rig.ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "gating"
        })
        .await;

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

/// Rename the session away and detach the daemon's control client: a
/// scripted, held-open detach. Rename back to let it reattach.
fn scripted_detach(rig: &Rig, session: &str, hidden: &str) {
    rig.tmux.run_ok(&["rename-session", "-t", session, hidden]);
    rig.tmux.run_ok(&["detach-client", "-s", hidden]);
}

fn scripted_reattach(rig: &Rig, hidden: &str, session: &str) {
    rig.tmux.run_ok(&["rename-session", "-t", hidden, session]);
}

/// Fix E part 3: a hook report arriving while the session is detached must
/// still be accepted (the pane exists in the last-known table) and must
/// resolve the in-flight delivery, so the recipient never answers two
/// copies of the same message.
#[tokio::test(flavor = "multi_thread")]
async fn hook_ack_during_detach_resolves_the_delivery() {
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
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "submitted"
        })
        .await;

    scripted_detach(&rig, "main", "hidden-dethook");
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "session" && e["data"]["attached"] == false
        })
        .await;

    // The recipient's hook fires DURING the outage. The report needs no
    // tmux connection; rejecting it was the soak's duplicate-delivery bug.
    // Posted through the trusted in-process path: the socket path is
    // pinned to the pane's process ancestry and this test process lives
    // outside the pane.
    let resp = rig
        .daemon
        .report_state(
            serde_json::from_value(json!({
                "agent": "hooky",
                "event": "UserPromptSubmit",
                "seq": 1,
                // The exact payload: a hook acknowledgement verifies the
                // bytes this delivery sent, or it verifies nothing.
                "payload": {
                    "prompt": cyclopsd::render_payload(&msg_id, "admin", "over the gap", "a\nb", false),
                    "session_id": "s",
                    "turn_id": "t",
                },
            }))
            .unwrap(),
        )
        .await
        .expect("report ok");
    assert_eq!(resp["applied"], true, "{resp}");
    assert_eq!(resp["matched"], true, "{resp}");
    assert_eq!(resp["live"], false, "{resp}");

    let done = rig
        .ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_verified"
        })
        .await;
    assert_eq!(done["data"]["verified_by"], "hook");

    scripted_reattach(&rig, "hidden-dethook", "main");
    rig.ev
        .wait_event(Duration::from_secs(15), |e| {
            e["event"] == "session" && e["data"]["attached"] == true
        })
        .await;

    // One paste, one delivery. No duplicate after reattach.
    let pastes = rig
        .ledger_lines()
        .iter()
        .filter(|l| {
            l["kind"] == "state" && l["id"] == msg_id.as_str() && l["data"]["to_state"] == "pasting"
        })
        .count();
    assert_eq!(pastes, 1, "delivery was pasted more than once");
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// Fix E parts 1 and 2: ACK deadlines freeze while the session is detached
/// and a reattach runs an evidence pass before any retry. A detach that
/// outlives the whole 5s ACK window must NOT burn the attempt: the
/// delivery resolves from post-reattach screen evidence with a single
/// paste (before the fix: timeout during the outage, retry after
/// reattach, second paste).
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
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "submitted"
        })
        .await;

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

    // Reattach evidence pass: the payload demonstrably arrived (id staged,
    // composer moved on), so the delivery resolves without a resubmit.
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;

    let lines = rig.ledger_lines();
    let pastes = lines
        .iter()
        .filter(|l| {
            l["kind"] == "state" && l["id"] == msg_id.as_str() && l["data"]["to_state"] == "pasting"
        })
        .count();
    assert_eq!(
        pastes, 1,
        "the detach burned the ACK window and forced a duplicate paste"
    );
    let final_line = lines
        .iter()
        .rev()
        .find(|l| l["kind"] == "state" && l["id"] == msg_id.as_str())
        .expect("final state line");
    assert_eq!(final_line["deliveries"][0]["attempts"], 1, "{final_line}");
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// Fix F: when pane_current_command matches no manifest (native installs
/// report a bare version string, F21), binding falls back to the argv
/// basename of pane_pid matched against process_names + argv_basenames.
#[tokio::test(flavor = "multi_thread")]
async fn binding_falls_back_to_argv_basename() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // A renamed binary: the pane runs .../claude (a symlink to cat), so
    // the kernel comm never matches the manifest's process_names.
    let bin_dir = cyclops_proto::scratch::scratch_dir("cyc-argv");
    let _ = std::fs::remove_dir_all(&bin_dir);
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let link = bin_dir.join("claude");
    std::os::unix::fs::symlink("/bin/cat", &link).expect("symlink");

    const ARGV_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Argv fixture"
process_names = ["no-such-comm-name"]
argv_basenames = ["claude"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 70
region = "bottom_non_empty_lines(4)"
# The fixture screen is the idle authority: a blank or plain pane is idle,
# and this rule is the lifecycle evidence that confirms it.
lifecycle_evidence = true
regex = ['^']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
"#;
    let mut rig = Rig::new("argv", ARGV_MANIFEST, link.to_str().unwrap(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "native").await;

    // The claim is binding, so the assertion is binding: status names the
    // manifest the pane resolved to. It used to assert a delivered
    // receipt instead, which proved binding only as a side effect and
    // could not survive a pane that cannot model a composer. This pane
    // deliberately cannot: it IS the symlinked `cat` whose argv basename
    // is the thing under test.
    let resp = rig.ctl.request("status", json!({})).await;
    let bound = &resp["result"]["sessions"][0]["panes"][0]["manifest"];
    assert_eq!(bound, "fix", "manifest never bound: {resp}");

    // And the gate agrees: a bound manifest holds on write-readiness,
    // where an unbound one dead-ends in attention_required instead.
    let (result, _) = rig
        .send(json!({"to": ["native"], "subject": "bind me", "body": "a\nb"}))
        .await;
    assert_eq!(
        result["deliveries"][0]["held_by"], "not_write_ready",
        "an unbound pane would have failed differently: {result}"
    );
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
    let _ = std::fs::remove_dir_all(&bin_dir);
}

/// Fix G (decline TOCTOU): the screen is re-read before the FINAL
/// confirming key of a multi-key decline; if the modal changed under the
/// sequence the confirm is withheld and the gate re-evaluates. The pane
/// clears its dialog the instant the first key arrives, so before the fix
/// the daemon typed Enter into whatever replaced it.
#[tokio::test(flavor = "multi_thread")]
async fn decline_aborts_when_the_modal_changes_between_keys() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Raw tty so dd returns on exactly one byte (the first decline key
    // "3"); the dialog then vanishes and the pane becomes a plain cat.
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
    // The gate re-read reality (pane is now an idle cat) and delivered.
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;
    rig.assert_ledger_legal(&["FAKE-UPDATE-AVAILABLE"]);
    rig.shutdown().await;
}

/// Fix G (cross-session consistency): the unresolvable-recipient state
/// line lands in the session files that carry the msg line, never a
/// bystander session's file. Each per-session ledger is a complete stream
/// for the deliveries it hosts.
#[tokio::test(flavor = "multi_thread")]
async fn cross_session_ledgers_are_complete_streams() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_multi(
        "xsess",
        CAT_MANIFEST,
        &[("main", "cat"), ("aux", &composer_pane())],
        "receipt_block_ms = 4000\n",
    )
    .await;
    let aux_pane = rig.pane_ids_session(1).await[0].clone();
    rig.label(&aux_pane, "auxw").await;

    // Recipients: one hosted in aux, one unresolvable. Session main is a
    // bystander: not involved at all.
    let (result, _) = rig
        .send(json!({"to": ["auxw", "ghost"], "subject": "split", "body": "a\nb"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();

    // aux hosts everything: msg line, the auxw chain, and the ghost
    // resolution.
    let aux_lines = rig.ledger_lines_for("aux");
    assert!(
        aux_lines
            .iter()
            .any(|l| l["kind"] == "msg" && l["id"] == msg_id.as_str()),
        "msg line missing from the involved session"
    );
    assert_eq!(
        rig.final_state_in("aux", &msg_id, "ghost").as_deref(),
        Some("attention_required"),
        "ghost chain missing from the involved session file"
    );
    assert_eq!(
        rig.final_state_in("aux", &msg_id, "auxw").as_deref(),
        Some("delivered_unverified")
    );

    // The bystander file carries no msg or delivery-state lines for this
    // message (admin notifications fan out everywhere by design).
    let main_lines = rig.ledger_lines_for("main");
    assert!(
        !main_lines
            .iter()
            .any(|l| (l["kind"] == "msg" || l["kind"] == "state") && l["id"] == msg_id.as_str()),
        "bystander session file carries pieces of another session's delivery"
    );

    rig.assert_ledger_legal_for("aux", &[]);
    rig.assert_ledger_legal_for("main", &[]);
    rig.shutdown().await;
}

/// Fix G (gate-hold visibility): a delivery held in gating past the
/// configured threshold pings the admin exactly once, while the hold keeps
/// waiting on events.
#[tokio::test(flavor = "multi_thread")]
async fn long_gate_hold_notifies_the_admin_once() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "holdn",
        BUSY_MANIFEST,
        &hold_script("BUSY-MARKER"),
        "receipt_block_ms = 100\ngate_hold_notify_ms = 300\n",
    )
    .await;
    rig.tmux.wait_screen("main", "BUSY-MARKER");
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "held").await;

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
    // It names the delivery it is about, which is still gating and so is
    // nobody's to clear: a reader's calm view keeps this out until the
    // delivery itself lands in a state the rule counts.
    assert_eq!(notify["data"]["to"], "held", "{notify}");
    assert_eq!(notify["data"]["id"], msg_id.as_str(), "{notify}");

    // Release: the held delivery proceeds on the state change.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;
    rig.assert_ledger_legal(&["BUSY-MARKER"]);
    rig.shutdown().await;
}

/// Fix G (subscriber buffer): a subscriber that stalls briefly while
/// events burst past the OLD 1024-event buffer survives and reads the
/// whole stream; only a truly wedged client is dropped.
#[tokio::test(flavor = "multi_thread")]
async fn briefly_stalled_subscriber_survives_event_burst() {
    // No tmux needed: a daemon with zero sessions still serves events.
    let home = cyclops_proto::scratch::scratch_dir("cyc-m1-subbuf");
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

    // 1600 events while the subscriber reads nothing: beyond the old 1024
    // buffer plus whatever the socket absorbed. The old daemon lagged the
    // receiver out and dropped the connection.
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
    // The stalled-but-alive subscriber catches up on the full stream.
    ev.wait_event(Duration::from_secs(20), |e| {
        e["event"] == "admin-notify" && e["data"]["subject"] == "burst-1599"
    })
    .await;
    daemon.shutdown().await;
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
    assert_eq!(ev["data"]["write_block"], "pane_in_mode", "{ev}");
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
    let states: Vec<&Value> = rig
        .ledger_lines()
        .iter()
        .filter(|l| l["kind"] == "state" && l["data"]["cause"] == "pane_in_mode")
        .cloned()
        .collect::<Vec<Value>>()
        .leak()
        .iter()
        .collect();
    assert!(states.is_empty(), "copy mode wrote a state transition");
    rig.shutdown().await;
}

/// Two messages back to back through one recipient, which is where a
/// hold that never releases would show up as a wedge rather than as a
/// refusal.
///
/// Cyclops latches its OWN paste: after the payload lands, the pane is
/// holding text exactly the way a person's draft holds it, so the second
/// delivery must not paste until a turn has consumed the first. The turn
/// then has to release it, or the recipient takes one message and never
/// another.
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
    // assertion that matters: it can only have gated after the first
    // turn released the hold this delivery's own paste latched.
    for id in &ids {
        let done = rig
            .ev
            .wait_event(Duration::from_secs(30), |e| {
                e["event"] == "delivery-state"
                    && e["data"]["id"] == id.as_str()
                    && e["data"]["to_state"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("delivered_"))
            })
            .await;
        assert_eq!(done["data"]["to"], "worker", "{done}");
    }

    // Nothing was pasted on top of anything: each payload got its own
    // gate/proceed, and they are ordered.
    let lines = rig.ledger_lines();
    let proceeds: Vec<String> = lines
        .iter()
        .filter(|l| l["kind"] == "gate" && l["data"]["action"] == "proceed")
        .filter_map(|l| l["id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(proceeds, ids, "one proceed each, in order: {proceeds:?}");
    rig.shutdown().await;
}

/// Submit command accepted, composer NOT consumed: the staged-never-sent
/// class c, which is what a modal or a mode does to an Enter.
///
/// `send-keys` succeeds here, so anything that treated that success as
/// proof of a turn would promote the hold and let the next delivery paste
/// over a payload that never went anywhere. The delivery has to end in
/// attention with the payload intact, and the pane has to stay refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_swallowed_submit_keeps_the_hold_and_the_payload() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "swallow",
        CAT_MANIFEST,
        &swallowing_composer_pane(),
        "receipt_block_ms = 200\nack_timeout_ms = 800\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    wait_pane_state(&mut rig, "idle").await;

    let (result, _) = rig
        .send(json!({"to": ["worker"], "subject": "one", "body": "a\nb"}))
        .await;
    let first = result["msg_id"].as_str().expect("msg id").to_string();

    // The pipeline gets as far as submitted and no further: nothing ever
    // proves consumption, so the chain closes for a human.
    let closed = rig
        .ev
        .wait_event(Duration::from_secs(20), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == first.as_str()
                && e["data"]["to_state"] == "attention_required"
        })
        .await;
    assert_eq!(closed["data"]["from"], "submitted", "{closed}");

    // The payload is still sitting in the composer, which is the whole
    // point: nothing consumed it.
    let screen = rig.tmux.capture(&pane);
    assert!(
        screen.contains(&format!("[cyclops:end {first}]")),
        "the payload left a composer nothing consumed: {screen}"
    );

    // Now the payload stops being VISIBLE without being consumed, which
    // is the wrapped-payload shape: it is really there and the screen
    // rules cannot see it. Done after the first delivery has already
    // closed, so this redraw cannot be mistaken for its screen receipt.
    // With the composer reading clean and the sensors agreeing, the hold
    // is the only thing left that can refuse.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "C-g"]);
    wait_pane_state(&mut rig, "idle").await;
    let screen = rig.tmux.capture(&pane);
    assert!(
        !screen.contains(&format!("[cyclops:end {first}]")),
        "the fixture is still drawing the payload: {screen}"
    );

    let (second, _) = rig
        .send(json!({"to": ["worker"], "subject": "two", "body": "c"}))
        .await;
    let second_id = second["msg_id"].as_str().expect("msg id").to_string();
    let held = rig
        .ev
        .wait_event(Duration::from_secs(15), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == second_id.as_str()
                && e["data"]["action"] == "hold"
        })
        .await;
    // Exactly the hold. The screen reads clean and the sensors agree, so
    // every other refusal is out of the way: if the receipt had wrongly
    // promoted this hold, the paste would have gone in.
    assert_eq!(
        held["data"]["cause"], "not_write_ready:composer_hold",
        "the second delivery gated on something other than the hold: {held}"
    );
    let screen = rig.tmux.capture(&pane);
    assert!(
        !screen.contains(&format!("[cyclops:end {second_id}]")),
        "a second payload was pasted over the first: {screen}"
    );
    rig.shutdown().await;
}
