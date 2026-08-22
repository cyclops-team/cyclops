//! Turn lifecycle against the real report path.
//!
//! Hook reports arrive over a socket from a separate process, so nothing
//! orders them. A vendor that names its turns lets the daemon match an end
//! to the turn it belongs to; what it cannot do is guarantee the end
//! arrives after the start. These tests drive that disorder through the
//! daemon's own ingestion path rather than through the pieces underneath
//! it.

mod common;

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
        include_str!("../../../resources/manifests/codex.toml"),
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
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == id
                && e["data"]["to_state"] == "submitted"
        })
        .await;
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
            "prompt": cyclopsd::render_payload(&id, "admin", subject, "b", false),
        }),
    )
    .await;
    assert_eq!(ack["matched"], true, "{ack}");
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == id.as_str()
                && e["data"]["to_state"] == "delivered_verified"
        })
        .await;
    id
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
            rig.ev
                .wait_event(Duration::from_secs(10), |e| {
                    e["event"] == "delivery-state"
                        && e["data"]["id"] == id.as_str()
                        && e["data"]["to_state"] == "submitted"
                })
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
            "prompt": cyclopsd::render_payload(&first, "admin", "first", "b", false),
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

    let _first = acknowledge_codex_turn(&mut rig, "first", "s1", "t1").await;

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
    }
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == second.as_str()
                && e["data"]["cause"] == "not_write_ready:composer_hold"
        })
        .await;
    assert_eq!(
        rig.final_state(&second, "keyed").as_deref(),
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

    let _first = acknowledge_codex_turn(&mut rig, "first", "s1", "t1").await;

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

    // Put the runtime reading back at idle with an end from another turn.
    // The exact hold remains, which makes the following detach exercise
    // stored lifecycle evidence rather than a transient sensor conflict.
    let mismatch = report(
        &rig,
        "Stop",
        json!({"session_id": "s1", "turn_id": "other"}),
    )
    .await;
    assert_eq!(mismatch["applied"], true, "{mismatch}");
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == second.as_str()
                && e["data"]["cause"] == "not_write_ready:composer_hold"
        })
        .await;

    let held_socket = HeldTmuxSocket::disconnect(&rig, "main");
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "session" && e["data"]["attached"] == false
        })
        .await;
    let end = report(&rig, "Stop", json!({"session_id": "s1", "turn_id": "t1"})).await;
    assert_eq!(end["applied"], true, "{end}");
    assert_eq!(end["live"], false, "{end}");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        rig.final_state(&second, "keyed").as_deref(),
        Some("gating"),
        "stored lifecycle evidence authorized a write while detached"
    );

    held_socket.restore();
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
                "prompt": cyclopsd::render_payload(&msg_id, "admin", "gap", "b", false),
            },
        });
        let resp = rig
            .daemon
            .report_state(serde_json::from_value(ack).expect("ack params"))
            .await
            .expect("report ok");
        assert_eq!(resp["applied"], true, "{phase}: {resp}");
        release.add_permits(1);

        rig.ev
            .wait_event(Duration::from_secs(10), |e| {
                e["event"] == "delivery-state"
                    && e["data"]["id"] == msg_id.as_str()
                    && e["data"]["to_state"] == "delivered_verified"
            })
            .await;
        rig.shutdown().await;
    }
}
