//! `agent.wait`: server-owned pane-state waits with occupant pinning,
//! against a real tmux server on an isolated `-L` socket (rig in
//! tests/common). Fixture turns are driven through pane titles:
//! `select-pane -T` pushes a subscription change (F13), so every state edge
//! the waits consume is event-driven end to end. Test-side sleeps and screen
//! polls are harness waits, outside the daemon's zero-polling contract.

use crate::common;

use std::time::Duration;

use common::*;
use serde_json::json;

/// Title-tier fixture: WORKING* titles read working, BLOCKED* titles read
/// blocked_permission, anything else reads idle. Injection matches the
/// cat fixture so send-and-wait can deliver first.
const WAIT_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Wait fixture"
process_names = ["python3", "python", "Python", "cat", "sh", "bash", "dash"]

[[rule]]
id = "title_working"
state = "working"
priority = 1000
region = "pane_title"
regex = ['^WORKING']

[[rule]]
id = "title_blocked"
state = "blocked_permission"
priority = 1000
region = "pane_title"
regex = ['^BLOCKED']

[[rule]]
id = "title_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

# Lifecycle rules, matching the shared fixtures: a write needs positive
# clean-composer screen evidence (INVARIANTS rule 12), the staged sentinel
# row is the staging evidence a blank pane can still show, and the
# transient working row is the turn evidence the screen ACK tier needs.
[[rule]]
id = "composer_working"
state = "working"
priority = 300
region = "bottom_non_empty_lines(5)"
line_regex = ['^FAKETUI-WORKING$']

[[rule]]
id = "composer_holds_paste"
state = "idle_with_input"
composer_semantic = "human_input"
priority = 80
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*❯\s+\S']
line_regex_esc = ['^❯']

[[rule]]
id = "composer_empty"
state = "idle"
composer_semantic = "clean"
priority = 90
region = "bottom_non_empty_lines(4)"
line_regex = ['^❯\s*$']

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

/// Run tmux commands against the rig's server from a helper thread after a
/// delay, so a blocking wait request can be driven mid-flight.
fn drive_later(socket: String, steps: Vec<(u64, Vec<String>)>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for (delay_ms, args) in steps {
            std::thread::sleep(Duration::from_millis(delay_ms));
            let _ = std::process::Command::new("tmux")
                .args(["-u", "-L", &socket, "-f", "/dev/null"])
                .args(args.iter().map(String::as_str))
                .status();
        }
    })
}

fn title_step(delay_ms: u64, pane: &str, title: &str) -> (u64, Vec<String>) {
    (
        delay_ms,
        ["select-pane", "-t", pane, "-T", title]
            .into_iter()
            .map(String::from)
            .collect(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_wait_idle_answers_immediately_and_unknown_targets_fail() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("waitnow", WAIT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // Already idle: the wait resolves without consuming the budget.
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "idle", "timeout_ms": 5000}),
        )
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    let r = &resp["result"];
    assert_eq!(r["state"], "idle", "{resp}");
    assert_eq!(r["pane_id"], pane.as_str(), "{resp}");
    assert_eq!(r["until"], "idle", "{resp}");
    // Shape symmetry with send-and-wait entries: success says so.
    assert_eq!(r["outcome"], "reached", "{resp}");
    assert!(r["waited_ms"].as_u64().expect("waited_ms") < 3000, "{resp}");

    // Unknown target: named error, not a hang.
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "ghost", "until": "idle", "timeout_ms": 500}),
        )
        .await;
    assert_eq!(resp["error"]["code"], "no_such_target", "{resp}");
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_wait_turn_ended_resolves_on_the_working_to_idle_edge() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("waitturnend", WAIT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // A current confirmed Working phase followed by Idle satisfies turn-ended.
    rig.tmux
        .run(&["select-pane", "-t", &pane, "-T", "WORKING now"]);
    rig.ev
        .wait_event(Duration::from_secs(5), |e| {
            e["event"] == "state"
                && e["data"]["pane_id"] == pane.as_str()
                && e["data"]["state"] == "working"
        })
        .await;
    let driver = drive_later(
        rig.tmux.socket().to_string(),
        vec![title_step(500, &pane, "READY again")],
    );
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "turn_ended", "timeout_ms": 8000}),
        )
        .await;
    driver.join().expect("driver thread");
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(resp["result"]["state"], "idle", "{resp}");

    // The pane is idle now, so turn-ended must NOT resolve until a Working phase
    // has been observed and the pane returns to Idle. The Working phase must
    // outlive tmux's 1Hz subscription tick or it is invisible (F23).
    let driver = drive_later(
        rig.tmux.socket().to_string(),
        vec![
            title_step(400, &pane, "WORKING again"),
            title_step(2000, &pane, "READY once more"),
        ],
    );
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "turn_ended", "timeout_ms": 8000}),
        )
        .await;
    driver.join().expect("driver thread");
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(resp["result"]["state"], "idle", "{resp}");
    let waited = resp["result"]["waited_ms"].as_u64().expect("waited_ms");
    assert!(
        waited >= 400,
        "turn-ended resolved before the driven turn even started ({waited}ms): {resp}"
    );
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_wait_blocked_resolves_on_a_blocked_state() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("waitblk", WAIT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    let driver = drive_later(
        rig.tmux.socket().to_string(),
        vec![title_step(400, &pane, "BLOCKED on a permission prompt")],
    );
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "blocked", "timeout_ms": 8000}),
        )
        .await;
    driver.join().expect("driver thread");
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(resp["result"]["state"], "blocked_permission", "{resp}");
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_wait_timeout_is_a_wire_error_naming_the_state() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("waitto", WAIT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // Nothing ever works: turn-ended cannot be reached, so the budget expires.
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "turn_ended", "timeout_ms": 600}),
        )
        .await;
    let err = &resp["error"];
    assert_eq!(err["code"], "timeout", "{resp}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("did not reach turn ended"),
        "{resp}"
    );
    // The error data carries the state it was in, so the caller can act.
    assert_eq!(err["data"]["state"], "idle", "{resp}");
    assert_eq!(err["data"]["outcome"], "timeout", "{resp}");
    assert!(
        err["data"]["waited_ms"].as_u64().expect("waited_ms") >= 600,
        "{resp}"
    );
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_wait_pins_the_occupant_and_reports_a_killed_pane() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("waitocc", WAIT_MANIFEST, &composer_pane(), "").await;
    // Keep a second pane so the session survives the kill.
    rig.tmux
        .run_ok(&["split-window", "-d", "-t", "main:0", "cat"]);
    rig.wait_attached(2).await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "mortal").await;

    let driver = drive_later(
        rig.tmux.socket().to_string(),
        vec![(
            400,
            ["kill-pane", "-t", &pane]
                .into_iter()
                .map(String::from)
                .collect(),
        )],
    );
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "mortal", "until": "turn_ended", "timeout_ms": 8000}),
        )
        .await;
    driver.join().expect("driver thread");
    let err = &resp["error"];
    assert_eq!(
        err["code"], "occupant_changed",
        "a dead pane must never read as a wait success: {resp}"
    );
    assert!(
        err["data"]["waited_ms"].as_u64().expect("waited_ms") < 8000,
        "resolved by timeout, not by the pin: {resp}"
    );
    rig.shutdown().await;
}
