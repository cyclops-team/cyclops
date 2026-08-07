//! session.watch: making a running daemon watch a tmux session it was not
//! booted with.
//!
//! `sessions` in config.toml is the boot set (see docs/guides/install.md); this
//! is the runtime path the terminal workspace UI uses when it creates a
//! tmux session on the fly and wants the daemon to see it. Same isolated
//! tmux rig as the other integration tests (tests/common); the bounded
//! poll loops here are test-side waits, explicitly outside the daemon's
//! zero-polling contract.

mod common;

use common::*;
use serde_json::json;
use std::time::{Duration, Instant};

/// Boot with nothing configured, watch a session created afterwards, and
/// see it show up on status with its pane table -- the whole point of the
/// verb. A second watch of the same name is a no-op, not a duplicate, and
/// an empty name is refused before it ever reaches tmux.
#[tokio::test(flavor = "multi_thread")]
async fn watching_a_session_after_boot_makes_status_see_it() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Empty `sessions`: the daemon boots watching nothing at all.
    let mut rig = Rig::new_multi("session-watch", CAT_MANIFEST, &[], "").await;
    assert_eq!(
        rig.ctl.request("status", json!({})).await["result"]["sessions"],
        json!([]),
        "an empty boot set watches nothing"
    );

    // A session the workspace UI created at runtime, on the same tmux
    // server the daemon is configured for.
    rig.tmux.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "extra",
        "-x",
        "160",
        "-y",
        "40",
        "cat",
    ]);

    let watch = rig
        .ctl
        .request("session.watch", json!({"session": "extra"}))
        .await;
    assert_eq!(watch["result"]["session"], json!("extra"), "{watch}");
    assert_eq!(watch["result"]["watching"], json!(true), "{watch}");
    assert_eq!(watch["result"]["added"], json!(true), "{watch}");

    // The new session is watched off the reactor: poll status the same
    // bounded way every other test waits for an attach, rather than a bare
    // sleep.
    rig.wait_attached_session(0, 1).await;
    let status = rig.ctl.request("status", json!({})).await;
    let sessions = status["result"]["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1, "{status}");
    assert_eq!(sessions[0]["name"], json!("extra"), "{status}");
    assert_eq!(sessions[0]["attached"], json!(true), "{status}");
    assert_eq!(
        sessions[0]["panes"].as_array().expect("panes").len(),
        1,
        "{status}"
    );

    // Watching the same session again is idempotent: no second ledger, no
    // second task, no duplicate row in status.
    let again = rig
        .ctl
        .request("session.watch", json!({"session": "extra"}))
        .await;
    assert_eq!(again["result"]["added"], json!(false), "{again}");
    assert_eq!(again["result"]["watching"], json!(true), "{again}");
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        status["result"]["sessions"]
            .as_array()
            .expect("sessions")
            .len(),
        1,
        "a repeat watch must not duplicate the session: {status}"
    );

    // An empty session name never reaches watch_session at all.
    let empty = rig
        .ctl
        .request("session.watch", json!({"session": ""}))
        .await;
    assert_eq!(empty["error"]["code"], json!("bad_request"), "{empty}");
    let whitespace = rig
        .ctl
        .request("session.watch", json!({"session": "   "}))
        .await;
    assert_eq!(
        whitespace["error"]["code"],
        json!("bad_request"),
        "{whitespace}"
    );
    let absent = rig.ctl.request("session.watch", json!({})).await;
    assert_eq!(absent["error"]["code"], json!("bad_request"), "{absent}");

    rig.shutdown().await;
}

/// End-to-end rename contract: the live watcher keeps feeding the same
/// daemon slot, a workspace re-registering the new name dedups, adoptions
/// move with the session, and new state facts keep appending to the ledger
/// that was already open under the old name.
#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_watched_session_keeps_one_slot_registry_and_ledger() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("session-rename", CAT_MANIFEST, "cat", "").await;
    let original_pane = rig.pane_ids().await.remove(0);
    rig.label(&original_pane, "implementer").await;

    rig.tmux
        .run_ok(&["rename-session", "-t", "=main", "renamed"]);

    let deadline = Instant::now() + Duration::from_secs(10);
    let renamed_status = loop {
        let status = rig.ctl.request("status", json!({})).await;
        let sessions = status["result"]["sessions"].as_array().expect("sessions");
        if sessions.len() == 1
            && sessions[0]["name"] == json!("renamed")
            && sessions[0]["attached"] == json!(true)
        {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never followed the rename: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        renamed_status["result"]["sessions"][0]["panes"][0]["agent"],
        json!("implementer"),
        "the adopted pane stays named after the session rename"
    );

    let rewatch = rig
        .ctl
        .request("session.watch", json!({"session": "renamed"}))
        .await;
    assert_eq!(rewatch["result"]["added"], json!(false), "{rewatch}");
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        status["result"]["sessions"]
            .as_array()
            .expect("sessions")
            .len(),
        1,
        "re-registering the new name must not duplicate the slot: {status}"
    );

    let registry: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(rig.home.join("registry.json")).expect("registry readable"),
    )
    .expect("registry JSON");
    assert_eq!(registry["panes"][0]["session"], json!("renamed"));
    assert_eq!(registry["windows"][0]["session"], json!("renamed"));

    rig.tmux
        .run_ok(&["split-window", "-t", "renamed", "/bin/sh"]);
    rig.wait_attached(2).await;
    let pane_ids = rig.pane_ids().await;
    let added_pane = pane_ids
        .iter()
        .find(|pane| **pane != original_pane)
        .expect("new pane");

    let lines = rig.ledger_lines();
    let rename_at = lines
        .iter()
        .position(|line| {
            line["data"]["event"] == json!("renamed")
                && line["data"]["old_name"] == json!("main")
                && line["data"]["new_name"] == json!("renamed")
        })
        .expect("rename fact in old-name ledger");
    assert!(
        lines.iter().skip(rename_at + 1).any(|line| {
            line["kind"] == json!("state") && line["data"]["pane_id"] == json!(added_pane)
        }),
        "pane events after the rename must keep appending to the open ledger"
    );
    assert!(
        !rig.ledger_path_for("renamed").exists(),
        "a live rename keeps one record instead of splitting it across files"
    );

    rig.shutdown().await;
}
