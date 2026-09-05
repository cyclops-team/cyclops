//! End-to-end test proving the real 3-agent journey contract:
//!
//! Codey sends -> Gemmy and Claudey receive visible submitted pane notifications
//! -> both claim and reply -> Codey receives both direct pane notifications while working.

use crate::common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::Rig;
use serde_json::{json, Value};

const THREE_AGENT_MANIFEST: &str = r#"
[agent]
id = "three-agent-fixture"
display_name = "Three Agent Fixture"
process_names = ["Python", "python3"]
argv_basenames = ["cycagent"]

[[rule]]
id = "pane_title_working"
state = "working"
priority = 1000
region = "pane_title"
contains = ["BUSY"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']
"#;

fn client_path() -> String {
    format!(
        "{}/tests/common/socket_client.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn named_clients(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}-bin"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let output = Command::new("sh")
        .args(["-c", "command -v python3"])
        .output()
        .expect("look up python3");
    let python = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(!python.is_empty(), "python3 must be on PATH");
    for name in ["cycagent", "cycclient"] {
        let link = dir.join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).expect("remove stale client link");
        }
        std::os::unix::fs::symlink(&python, link).expect("symlink python3");
    }
    dir
}

fn agent_command_loop(client_dir: &Path) -> String {
    format!(
        "{}/cycagent -u -c 'import shlex,subprocess,sys\nfor line in sys.stdin:\n try: subprocess.run(shlex.split(line))\n except Exception: pass'",
        client_dir.display()
    )
}

fn workspace_lines(rig: &Rig) -> Vec<Value> {
    let workspace = fs::read_dir(rig.home.join("workspaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = fs::read_to_string(workspace.join("messages.ndjson")).unwrap();
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

async fn wait_for_notification_submitted(rig: &mut Rig, message_id: &str, expected_count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count = workspace_lines(rig)
            .into_iter()
            .filter(|line| {
                line["id"] == message_id
                    && line["data"]["type"] == "notification_transition"
                    && matches!(
                        line["data"]["state"].as_str(),
                        Some("submitted" | "submitted_unverified" | "notified")
                    )
            })
            .count();
        if count >= expected_count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "notification {message_id} was not submitted (got {count}/{expected_count}): {:#?}",
            workspace_lines(rig)
                .into_iter()
                .filter(|line| line["id"] == message_id)
                .collect::<Vec<_>>()
        );
        // Every journal append publishes `messages.changed`; wake on it and read
        // the journal again rather than sleep.
        rig.ev
            .wake_on(deadline.saturating_duration_since(Instant::now()), |e| {
                e["event"] == "messages.changed"
            })
            .await;
    }
}

async fn response(rig: &Rig, pane: &str, out: &Path) -> Value {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(out) {
            if !text.trim().is_empty() {
                return serde_json::from_str(&text).expect("client response parses");
            }
        }
        // No event: the fixture answers by writing a file.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let cap = rig.tmux.capture(pane);
    panic!(
        "client never answered: {}; pane capture:\n{cap}",
        out.display()
    );
}

async fn pane_request(
    rig: &mut Rig,
    client_dir: &Path,
    pane: &str,
    tag: &str,
    method: &str,
    params: Value,
) -> Value {
    let out = rig.home.join(format!("{tag}.json"));
    rig.tmux.run_ok(&["send-keys", "-t", pane, "C-u"]);
    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        pane,
        &format!(
            "{}/cycclient {} {} {} {} '{}'",
            client_dir.display(),
            client_path(),
            rig.daemon.socket_path().display(),
            out.display(),
            method,
            params
        ),
        "Enter",
    ]);
    response(rig, pane, &out).await
}

async fn wait_manifest_bound(rig: &mut Rig, pane_count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let panes = status["result"]["sessions"][0]["panes"]
            .as_array()
            .unwrap_or_else(|| panic!("no panes in {status}"));
        if panes.len() == pane_count
            && panes
                .iter()
                .all(|pane| pane["manifest"] == "three-agent-fixture")
        {
            return;
        }
        assert!(Instant::now() < deadline, "manifest never bound: {status}");
        // No event: binding a manifest publishes nothing, and status blanks
        // `manifest` while its live refresh is incomplete.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_pane_text(rig: &Rig, pane_id: &str, text: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let capture = rig.tmux.capture(pane_id);
        if capture.contains(text) {
            return;
        }
        // No event: the screen is read from a tmux capture.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let capture = rig.tmux.capture(pane_id);
    panic!("pane {pane_id} never contained {text:?}; capture:\n{capture}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_three_agent_journey() {
    println!("\n===============================================================================");
    println!("             REAL 3-AGENT JOURNEY END-TO-END VERIFICATION                      ");
    println!("===============================================================================\n");

    let client_dir = named_clients("three-agent-journey");
    let pane_command = agent_command_loop(&client_dir);

    let mut rig = Rig::new("threeagent", THREE_AGENT_MANIFEST, &pane_command, "").await;
    let first = rig.pane_ids().await[0].clone();
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", &first, &pane_command]);
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", &first, &pane_command]);
    rig.wait_attached(3).await;
    wait_manifest_bound(&mut rig, 3).await;

    let panes = rig.pane_ids().await;
    let codey_pane = panes[0].clone();
    let gemmy_pane = panes[1].clone();
    let claudey_pane = panes[2].clone();

    rig.label(&codey_pane, "codey").await;
    rig.label(&gemmy_pane, "gemmy").await;
    rig.label(&claudey_pane, "claudey").await;

    println!("Agents adopted & labelled: codey ({codey_pane}), gemmy ({gemmy_pane}), claudey ({claudey_pane})");

    // -----------------------------------------------------------------------
    // Step 1: Codey sends message to Gemmy and Claudey
    // -----------------------------------------------------------------------
    println!("\n--- Step 1: Codey sends broadcast message to Gemmy and Claudey ---");
    let broadcast_secret = "Task: Review PR #219 architecture changes";
    let sent = pane_request(
        &mut rig,
        &client_dir,
        &codey_pane,
        "codey-send",
        "msg.send",
        json!({
            "to": ["gemmy", "claudey"],
            "subject": "PR #219 Review Request",
            "summary": "A review request is waiting. Claim it from the mailbox.",
            "body": broadcast_secret,
            "client_key": "broadcast-codey-1",
        }),
    )
    .await;
    assert!(
        sent["error"].is_null(),
        "Codey broadcast send failed: {sent}"
    );
    let broadcast_id = sent["result"]["msg_id"].as_str().unwrap().to_string();
    println!("Broadcast message accepted with id {broadcast_id}");

    // -----------------------------------------------------------------------
    // Step 2: Gemmy and Claudey receive visible submitted pane notifications
    // -----------------------------------------------------------------------
    println!(
        "\n--- Step 2: Waiting for Gemmy and Claudey to receive visible pane notifications ---"
    );
    wait_for_notification_submitted(&mut rig, &broadcast_id, 2).await;
    wait_for_pane_text(
        &rig,
        &gemmy_pane,
        "cyclops inbox claim",
        Duration::from_secs(5),
    )
    .await;
    wait_for_pane_text(
        &rig,
        &claudey_pane,
        "cyclops inbox claim",
        Duration::from_secs(5),
    )
    .await;

    let gemmy_cap = rig.tmux.capture(&gemmy_pane);
    let claudey_cap = rig.tmux.capture(&claudey_pane);
    assert!(gemmy_cap.contains("cyclops inbox claim"));
    assert!(claudey_cap.contains("cyclops inbox claim"));
    assert!(
        !gemmy_cap.contains(broadcast_secret),
        "Payload body must not leak to pane screen"
    );
    assert!(
        !claudey_cap.contains(broadcast_secret),
        "Payload body must not leak to pane screen"
    );
    println!("✓ Both Gemmy and Claudey received visible doorbell notifications with private body preserved");

    // A bounded window: the fixture's command loop publishes nothing that
    // marks the doorbell turn finishing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // -----------------------------------------------------------------------
    // Step 3: Both Gemmy and Claudey claim the message from inside their panes
    // -----------------------------------------------------------------------
    println!(
        "\n--- Step 3: Gemmy and Claudey claim the broadcast message from inside their panes ---"
    );
    let gemmy_claimed = pane_request(
        &mut rig,
        &client_dir,
        &gemmy_pane,
        "gemmy-claim",
        "inbox.claim",
        json!({"message_id": broadcast_id}),
    )
    .await;
    assert!(
        gemmy_claimed["error"].is_null(),
        "Gemmy claim failed: {gemmy_claimed}"
    );
    assert_eq!(gemmy_claimed["result"]["disposition"], "claimed");
    assert_eq!(gemmy_claimed["result"]["message"]["body"], broadcast_secret);

    let claudey_claimed = pane_request(
        &mut rig,
        &client_dir,
        &claudey_pane,
        "claudey-claim",
        "inbox.claim",
        json!({"message_id": broadcast_id}),
    )
    .await;
    assert!(
        claudey_claimed["error"].is_null(),
        "Claudey claim failed: {claudey_claimed}"
    );
    assert_eq!(claudey_claimed["result"]["disposition"], "claimed");
    assert_eq!(
        claudey_claimed["result"]["message"]["body"],
        broadcast_secret
    );
    println!("✓ Both agents successfully claimed and authenticated private message body");

    // -----------------------------------------------------------------------
    // Step 4: Codey enters WORKING state
    // -----------------------------------------------------------------------
    println!("\n--- Step 4: Setting Codey to WORKING state (busy) ---");
    rig.tmux
        .run(&["select-pane", "-t", &codey_pane, "-T", "BUSY-CODING"]);
    // Verify Codey is observed as working: the fused verdict, woken by its
    // `state` event and read from the ledger.
    common::wait_pane_id_state(&mut rig, &codey_pane, "working").await;
    println!("✓ Codey confirmed in WORKING state");

    // -----------------------------------------------------------------------
    // Step 5: Both Gemmy and Claudey reply while Codey is working
    // -----------------------------------------------------------------------
    println!("\n--- Step 5: Gemmy and Claudey reply to Codey while Codey is working ---");
    let gemmy_reply = pane_request(
        &mut rig,
        &client_dir,
        &gemmy_pane,
        "gemmy-reply",
        "msg.send",
        json!({
            "to": [],
            "subject": "Re: PR #219 Review Request",
            "body": "Gemmy approval: Architecture looks clean and minimal!",
            "client_key": "gemmy-reply-1",
            "reply_to": broadcast_id,
        }),
    )
    .await;
    assert!(
        gemmy_reply["error"].is_null(),
        "Gemmy reply failed: {gemmy_reply}"
    );
    let gemmy_reply_id = gemmy_reply["result"]["msg_id"]
        .as_str()
        .unwrap()
        .to_string();

    let claudey_reply = pane_request(
        &mut rig,
        &client_dir,
        &claudey_pane,
        "claudey-reply",
        "msg.send",
        json!({
            "to": [],
            "subject": "Re: PR #219 Review Request",
            "body": "Claudey approval: Verified safe from bugs and ready for change!",
            "client_key": "claudey-reply-1",
            "reply_to": broadcast_id,
        }),
    )
    .await;
    assert!(
        claudey_reply["error"].is_null(),
        "Claudey reply failed: {claudey_reply}"
    );
    let claudey_reply_id = claudey_reply["result"]["msg_id"]
        .as_str()
        .unwrap()
        .to_string();

    println!("✓ Gemmy reply {gemmy_reply_id} sent");
    println!("✓ Claudey reply {claudey_reply_id} sent");

    // -----------------------------------------------------------------------
    // Step 6: Codey receives direct pane notifications while working!
    // -----------------------------------------------------------------------
    println!("\n--- Step 6: Verifying Codey receives direct pane notifications while working ---");
    // Under our new contract: Working state does not silently prevent submission!
    // Codey receives Gemmy's reply notification in pane while working
    wait_for_notification_submitted(&mut rig, &gemmy_reply_id, 1).await;
    wait_for_pane_text(
        &rig,
        &codey_pane,
        "cyclops inbox claim",
        Duration::from_secs(5),
    )
    .await;
    println!("✓ Codey received Gemmy's reply notification in pane while working");

    // Codey claims Gemmy's reply from inside its pane
    let codey_claim_1 = pane_request(
        &mut rig,
        &client_dir,
        &codey_pane,
        "codey-claim-1",
        "inbox.claim",
        json!({"message_id": gemmy_reply_id}),
    )
    .await;
    assert!(
        codey_claim_1["error"].is_null(),
        "Codey claim 1 failed: {codey_claim_1}"
    );
    assert_eq!(codey_claim_1["result"]["disposition"], "claimed");
    assert_eq!(
        codey_claim_1["result"]["message"]["body"],
        "Gemmy approval: Architecture looks clean and minimal!"
    );
    println!("✓ Codey successfully claimed Gemmy's reply");

    // Once the first message is claimed, the next queued message for Codey (Claudey's reply)
    // is scheduled and delivered to Codey while still working!
    wait_for_notification_submitted(&mut rig, &claudey_reply_id, 1).await;
    println!("✓ Codey received Claudey's reply notification in pane while working");

    // Codey claims Claudey's reply from inside its pane
    let codey_claim_2 = pane_request(
        &mut rig,
        &client_dir,
        &codey_pane,
        "codey-claim-2",
        "inbox.claim",
        json!({"message_id": claudey_reply_id}),
    )
    .await;
    assert!(
        codey_claim_2["error"].is_null(),
        "Codey claim 2 failed: {codey_claim_2}"
    );
    assert_eq!(codey_claim_2["result"]["disposition"], "claimed");
    assert_eq!(
        codey_claim_2["result"]["message"]["body"],
        "Claudey approval: Verified safe from bugs and ready for change!"
    );
    println!("✓ Codey successfully claimed Claudey's reply");

    println!("\n===============================================================================");
    println!("   REAL 3-AGENT JOURNEY PROOF: 100% SUCCESSFUL END-TO-END EXECUTION!           ");
    println!("===============================================================================\n");

    rig.shutdown().await;
}
