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
use std::io::Write as _;
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

    // Keep the isolated tmux server reachable after the runtime session goes
    // away. The removal assertion below needs tmux's named-missing reply;
    // if `extra` were its last session, a missing socket would be honest
    // uncertainty and must retain the runtime slot instead of retiring it.
    rig.tmux
        .run_ok(&["new-session", "-d", "-s", "anchor", "cat"]);

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

    // Once a runtime session that was live is positively removed, it must
    // disappear from the public projection. Reusing the display name later
    // creates a fresh watcher rather than reviving the dead row.
    rig.tmux.run_ok(&["kill-session", "-t", "=extra"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        if status["result"]["sessions"] == json!([]) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "status retained the removed runtime session: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

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
    let replacement = rig
        .ctl
        .request("session.watch", json!({"session": "extra"}))
        .await;
    assert_eq!(replacement["result"]["added"], json!(true), "{replacement}");
    rig.wait_attached_session(0, 1).await;
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(status["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(status["result"]["sessions"][0]["name"], json!("extra"));

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

/// Losing the whole tmux server is not proof that a runtime session was
/// removed. Keep its durable slot readable until the creator rebuilds tmux
/// and sends the same explicit availability edge that created the watch.
#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_runtime_watch_waits_for_an_explicit_availability_edge() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_multi("runtime-watch-unavailable", CAT_MANIFEST, &[], "").await;
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
    let first_watch = rig
        .ctl
        .request("session.watch", json!({"session": "extra"}))
        .await;
    assert_eq!(first_watch["result"]["added"], json!(true), "{first_watch}");
    rig.wait_attached_session(0, 1).await;
    let before_loss = rig.ctl.request("status", json!({})).await;
    let before_identity = before_loss["result"]["sessions"][0]["identity"].clone();

    rig.tmux.simulate_server_loss();
    rig.ev
        .wait_event(Duration::from_secs(10), |event| {
            event["event"] == "session"
                && event["data"]["name"] == "extra"
                && event["data"]["attached"] == false
        })
        .await;
    let detached = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        detached["result"]["sessions"].as_array().map(Vec::len),
        Some(1),
        "an unavailable tmux server does not retire the runtime slot: {detached}"
    );
    assert_eq!(detached["result"]["sessions"][0]["name"], json!("extra"));
    assert_eq!(detached["result"]["sessions"][0]["attached"], json!(false));
    assert_eq!(
        detached["result"]["sessions"][0]["identity"], before_identity,
        "uncertainty keeps the runtime slot's last observed identity"
    );

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
    let still_detached = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        still_detached["result"]["sessions"][0]["attached"],
        json!(false),
        "recreating tmux alone is not an availability edge"
    );

    let availability = rig
        .ctl
        .request("session.watch", json!({"session": "extra"}))
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
    rig.wait_attached_session(0, 1).await;

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
        lines[rename_at]["data"]["identity"].is_object(),
        "rename recovery requires an identity-bound system fact"
    );
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

/// A followed rename must remain the boot identity after a clean daemon
/// restart. The config intentionally still names `main`: runtime following
/// must persist the stable session identity without rewriting that file.
#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_watched_session_is_followed_after_daemon_restart() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let rig = Rig::new("session-rename-restart", CAT_MANIFEST, "cat", "").await;
    rig.tmux
        .run_ok(&["rename-session", "-t", "=main", "renamed"]);

    let mut rig = rig;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let sessions = status["result"]["sessions"].as_array().expect("sessions");
        if sessions.len() == 1
            && sessions[0]["name"] == json!("renamed")
            && sessions[0]["attached"] == json!(true)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never followed the runtime rename: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let config = std::fs::read_to_string(rig.home.join("config.toml")).expect("config readable");
    assert!(
        config.contains("sessions = [\"main\"]"),
        "rename rewrote user config"
    );

    let mut rebooted = rig.reboot().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let status = rebooted.ctl.request("status", json!({})).await;
        let sessions = status["result"]["sessions"].as_array().expect("sessions");
        if sessions.len() == 1 && sessions[0]["attached"] == json!(true) {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not reattach the renamed session: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let sessions = status["result"]["sessions"].as_array().expect("sessions");
    assert_eq!(sessions[0]["name"], json!("renamed"), "{status}");
    assert_ne!(
        sessions[0]["name"],
        json!("main"),
        "old name is a ghost: {status}"
    );
    assert!(rebooted.ledger_path_for("main").exists());
    assert!(!rebooted.ledger_path_for("renamed").exists());

    rebooted.shutdown().await;
}

/// A syntactically valid rename fact with an identity that was never
/// persisted by this daemon must not retarget boot to the renamed session.
/// The configured root remains the safe fallback while the live session is
/// still available under its configured name.
#[tokio::test(flavor = "multi_thread")]
async fn an_unpersisted_rename_identity_cannot_retarget_boot() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("session-rename-unpersisted", CAT_MANIFEST, "cat", "").await;
    let initial_status = rig.ctl.request("status", json!({})).await;
    let live_identity = initial_status["result"]["sessions"][0]["identity"].clone();
    assert!(live_identity.is_object(), "initial live identity");
    let lines = rig.ledger_lines();
    let mut forged = lines
        .iter()
        .find(|line| line["data"]["event"] == json!("boot"))
        .cloned()
        .expect("boot fact");
    let next_seq = lines
        .iter()
        .filter_map(|line| line["seq"].as_u64())
        .max()
        .expect("ledger sequence")
        + 1;
    forged["seq"] = json!(next_seq);
    forged["id"] = json!("e-forged-rename");
    forged["data"] = json!({
        "event": "renamed",
        "old_name": "main",
        "new_name": "forged",
        "identity": live_identity,
    });
    forged["data"]["identity"]["session_instance_id"] =
        json!("00000000-0000-0000-0000-000000000003");
    let mut ledger = std::fs::OpenOptions::new()
        .append(true)
        .open(rig.ledger_path())
        .expect("ledger append");
    writeln!(
        ledger,
        "{}",
        serde_json::to_string(&forged).expect("forged JSON")
    )
    .expect("append forged rename");

    let mut rebooted = rig.reboot().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let status = rebooted.ctl.request("status", json!({})).await;
        let sessions = status["result"]["sessions"].as_array().expect("sessions");
        if sessions.len() == 1
            && sessions[0]["name"] == json!("main")
            && sessions[0]["attached"] == json!(true)
        {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "forged identity retargeted boot: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_ne!(
        status["result"]["sessions"][0]["name"],
        json!("forged"),
        "{status}"
    );

    rebooted.shutdown().await;
}

/// Two configured roots that resolve to the same verified tmux session must
/// settle to one canonical live slot rather than two watcher routes.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_configured_roots_coalesce_by_verified_session_identity() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let rig = Rig::new("session-duplicate-root", CAT_MANIFEST, "cat", "").await;
    rig.tmux
        .run_ok(&["rename-session", "-t", "=main", "renamed"]);

    let mut rig = rig;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        if status["result"]["sessions"][0]["name"] == json!("renamed") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never followed rename: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    rig.sessions.push("renamed".into());
    rig.rewrite_config("");
    // Give both configured roots identity-bound rename evidence. The second
    // root uses the same live binding under its own configured old name, so
    // this exercises boot-time coalescing rather than runtime repair.
    let mut duplicate_fact = rig
        .ledger_lines()
        .into_iter()
        .find(|line| line["data"]["event"] == json!("renamed"))
        .expect("runtime rename fact");
    duplicate_fact["data"]["old_name"] = json!("renamed");
    duplicate_fact["data"]["new_name"] = json!("renamed");
    std::fs::write(
        rig.ledger_path_for("renamed"),
        format!(
            "{}\n",
            serde_json::to_string(&duplicate_fact).expect("duplicate rename JSON")
        ),
    )
    .expect("write duplicate identity-bound history");
    let mut rebooted = rig.reboot().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let status = rebooted.ctl.request("status", json!({})).await;
        let sessions = status["result"]["sessions"].as_array().expect("sessions");
        if sessions.len() == 1 && sessions[0]["attached"] == json!(true) {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "duplicate roots did not coalesce: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let sessions = status["result"]["sessions"].as_array().expect("sessions");
    assert_eq!(sessions[0]["name"], json!("renamed"), "{status}");
    assert!(rebooted.ledger_path_for("renamed").exists());
    assert!(
        rebooted
            .ledger_lines_for("renamed")
            .iter()
            .all(|line| line["data"]["event"] != json!("session_slot_aliased")),
        "boot identity coalescing should retire the duplicate before a watcher starts"
    );
    rebooted.shutdown().await;
}
