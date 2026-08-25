//! M2 agent.wait: server-owned waits with occupant pinning, against a real
//! tmux server on an isolated `-L` socket (rig in tests/common). Fixture
//! turns are driven through pane titles: `select-pane -T` pushes a
//! subscription change (F13), so every state edge the waits consume is
//! event-driven end to end. Test-side sleeps and screen polls are harness
//! waits, outside the daemon's zero-polling contract.

mod common;

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
async fn agent_wait_done_resolves_on_the_working_to_idle_edge() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("waitdone", WAIT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // CURRENT turn: put the pane mid-turn, then wait; the turn ending
    // satisfies done.
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
            json!({"target": "worker", "until": "done", "timeout_ms": 8000}),
        )
        .await;
    driver.join().expect("driver thread");
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(resp["result"]["state"], "idle", "{resp}");

    // NEXT turn: the pane is idle now, so done must NOT resolve until a
    // working phase has been observed AND ended. The working phase must
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
            json!({"target": "worker", "until": "done", "timeout_ms": 8000}),
        )
        .await;
    driver.join().expect("driver thread");
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(resp["result"]["state"], "idle", "{resp}");
    let waited = resp["result"]["waited_ms"].as_u64().expect("waited_ms");
    assert!(
        waited >= 400,
        "done resolved before the driven turn even started ({waited}ms): {resp}"
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

    // Nothing ever works: done cannot be reached, so the budget expires.
    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "done", "timeout_ms": 600}),
        )
        .await;
    let err = &resp["error"];
    assert_eq!(err["code"], "timeout", "{resp}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("did not reach done"),
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
            json!({"target": "mortal", "until": "done", "timeout_ms": 8000}),
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

/// LOW: send-and-wait pins the occupant the delivery was SUBMITTED to.
/// The occupant is swapped between delivery resolution and wait start (the
/// pre_wait seam); the wait must answer occupant_changed instead of
/// reporting about the impostor. Before the fix the wait pinned whoever
/// lived in the pane at wait start, and `until: idle` here "reached"
/// instantly on the impostor.
#[tokio::test(flavor = "multi_thread")]
async fn send_wait_pins_the_submitted_occupant_not_the_impostor() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "waitpin",
        WAIT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 5000\n",
    )
    .await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // Park the send at the pre_wait seam: delivery resolved, wait not yet
    // started.
    let (entered_tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let release_seam = std::sync::Arc::clone(&release);
    rig.daemon.set_inject_pause(move |p| {
        let entered_tx = entered_tx.clone();
        let release = std::sync::Arc::clone(&release_seam);
        Box::pin(async move {
            if p != "pre_wait" {
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

    let daemon = &rig.daemon;
    let ctl = &mut rig.ctl;
    let tmux = &rig.tmux;
    let params: cyclops_proto::MsgSendParams = serde_json::from_value(json!({
        "to": ["worker"],
        "subject": "for the first occupant",
        "body": "b",
        "wait": {"until": "idle", "timeout_ms": 8000},
    }))
    .expect("send params");
    let (result, ()) = tokio::join!(daemon.deliver_payload("admin", params), async {
        entered
            .recv()
            .await
            .expect("send reached the pre_wait seam");
        // Swap the occupant (cat -> sh) and wait until the watcher table
        // saw it: command and pid travel in the same subscription push.
        // The impostor is `cat` on purpose. The first occupant is the
        // composer, which is a shell, so respawning another shell would
        // leave "the command changed" true from the start and the seam
        // would resume against the very row it is supposed to catch
        // changing. The pane pid would be the exact discriminator, but
        // status does not carry it, so the command name has to differ.
        tmux.run_ok(&["respawn-pane", "-k", "-t", &pane, "cat"]);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let status = ctl.request("status", json!({})).await;
            let cmd = status["result"]["sessions"][0]["panes"][0]["current_command"].clone();
            if cmd == "cat" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher never saw the occupant swap: {status}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        release.add_permits(1);
    });
    let result = result.expect("send-and-wait answers");
    assert_eq!(
        result["deliveries"][0]["state"], "delivered_unverified",
        "the delivery itself landed on the first occupant: {result}"
    );
    let wait = &result["wait"][0];
    assert_eq!(
        wait["outcome"], "occupant_changed",
        "the wait must refuse to answer for the impostor: {result}"
    );
    assert_eq!(wait["delivery"], "delivered_unverified", "{result}");
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn send_wait_done_round_trip() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("sendwait", WAIT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;

    // Fixture turn: once the payload lands in the pane, run one
    // working-then-idle title cycle, as an agent CLI would.
    let socket = rig.tmux.socket().to_string();
    let pane_for_driver = pane.clone();
    let driver = std::thread::spawn(move || {
        let tmux = |args: &[&str]| {
            let _ = std::process::Command::new("tmux")
                .args(["-u", "-L", &socket, "-f", "/dev/null"])
                .args(args)
                .status();
        };
        let capture = || {
            std::process::Command::new("tmux")
                .args(["-u", "-L", &socket, "-f", "/dev/null"])
                .args(["capture-pane", "-p", "-t", &pane_for_driver])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !capture().contains("[cyclops") && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(200));
        tmux(&[
            "select-pane",
            "-t",
            &pane_for_driver,
            "-T",
            "WORKING the turn",
        ]);
        // Outlive the 1Hz subscription tick (F23) so the working phase is
        // observable before the turn "ends".
        std::thread::sleep(Duration::from_millis(2000));
        tmux(&["select-pane", "-t", &pane_for_driver, "-T", "READY done"]);
    });

    let (result, _) = rig
        .send(json!({
            "to": ["worker"],
            "subject": "run this",
            "body": "a\nb",
            "wait": {"until": "done", "timeout_ms": 10000},
        }))
        .await;
    driver.join().expect("driver thread");

    let msg_id = result["msg_id"].as_str().expect("msg id").to_string();
    assert_eq!(
        result["deliveries"][0]["state"], "delivered_unverified",
        "{result}"
    );
    let wait = &result["wait"][0];
    assert_eq!(wait["to"], "worker", "{result}");
    assert_eq!(wait["outcome"], "reached", "{result}");
    assert_eq!(wait["state"], "idle", "{result}");
    assert_eq!(wait["delivery"], "delivered_unverified", "{result}");
    assert!(wait["waited_ms"].is_u64(), "{result}");

    assert_eq!(
        rig.final_state(&msg_id, "worker").as_deref(),
        Some("delivered_unverified")
    );
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// The pane shape an adopted agent actually has: tmux spawned the SHELL,
/// and the agent is a job that shell put in the terminal's foreground.
///
/// A delivery pins the foreground leader, because that is the process the
/// message went to and the one whose exit invalidates the receipt. The
/// wait pinned the pane root. On every other fixture in this file those
/// are the same number, because tmux execs the fake TUI directly, so the
/// two domains could be compared for free and nothing ever noticed. Here
/// they differ, and comparing them made `send --wait` answer
/// occupant_changed the instant it started, on every interactive pane in
/// the fleet.
#[tokio::test]
async fn send_wait_holds_when_the_agent_is_a_child_of_the_pane_shell() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    if std::process::Command::new("zsh")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: no zsh to be the pane's shell");
        return;
    }
    // An INTERACTIVE shell, because job control is what puts the child in
    // its own process group and makes it the tty's foreground. A `sh -c`
    // would leave the child in the shell's group and collapse the two
    // identities all over again.
    let mut rig = Rig::new("waitchild", WAIT_MANIFEST, "zsh -f", "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("exec 2>/dev/null; python3 {}", faketui_path()),
        "Enter",
    ]);
    rig.tmux.wait_screen(&pane, "❯");
    rig.label(&pane, "worker").await;

    // The premise, asserted rather than assumed: if this platform ever
    // stops splitting the two, the test below would pass while proving
    // nothing.
    let root = String::from_utf8_lossy(
        &rig.tmux
            .run(&["display-message", "-p", "-t", &pane, "#{pane_pid}"])
            .stdout,
    )
    .trim()
    .parse::<i32>()
    .expect("pane_pid");
    let fg = String::from_utf8_lossy(
        &std::process::Command::new("ps")
            .args(["-o", "tpgid=", "-p", &root.to_string()])
            .output()
            .expect("ps")
            .stdout,
    )
    .trim()
    .parse::<i32>()
    .expect("tpgid");
    assert_ne!(
        root, fg,
        "the pane root and its foreground leader collapsed; this fixture no longer \
         reproduces the shape it exists for"
    );

    let socket = rig.tmux.socket().to_string();
    let pane_for_driver = pane.clone();
    let driver = std::thread::spawn(move || {
        let tmux = |args: &[&str]| {
            let _ = std::process::Command::new("tmux")
                .args(["-u", "-L", &socket, "-f", "/dev/null"])
                .args(args)
                .status();
        };
        let capture = || {
            std::process::Command::new("tmux")
                .args(["-u", "-L", &socket, "-f", "/dev/null"])
                .args(["capture-pane", "-p", "-t", &pane_for_driver])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !capture().contains("[cyclops") && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(200));
        tmux(&[
            "select-pane",
            "-t",
            &pane_for_driver,
            "-T",
            "WORKING the turn",
        ]);
        std::thread::sleep(Duration::from_millis(2000));
        tmux(&["select-pane", "-t", &pane_for_driver, "-T", "READY done"]);
    });

    let (result, _) = rig
        .send(json!({
            "to": ["worker"],
            "subject": "run this",
            "body": "a\nb",
            "wait": {"until": "done", "timeout_ms": 10000},
        }))
        .await;
    driver.join().expect("driver thread");

    let wait = &result["wait"][0];
    assert_ne!(
        wait["outcome"], "occupant_changed",
        "the agent never went anywhere: {result}"
    );
    assert_eq!(wait["outcome"], "reached", "{result}");
    assert_eq!(wait["state"], "idle", "{result}");
    rig.shutdown().await;
}

#[tokio::test]
async fn a_same_command_respawn_wakes_a_pinned_wait_as_occupant_changed() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("wait-respawn", WAIT_MANIFEST, &composer_pane(), "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "worker").await;
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "WORKING before respawn"]);
    wait_pane_state(&mut rig, "working").await;

    let socket = rig.tmux.socket().to_string();
    let pane_for_driver = pane.clone();
    let replacement = composer_pane();
    let driver = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let output = std::process::Command::new("tmux")
            .args(["-u", "-L", &socket, "-f", "/dev/null"])
            .args(["respawn-pane", "-k", "-t", &pane_for_driver, &replacement])
            .output()
            .expect("respawn replacement agent");
        assert!(
            output.status.success(),
            "respawn failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    let response = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "done", "timeout_ms": 6000}),
        )
        .await;
    driver.join().expect("driver thread");
    assert_eq!(
        response["error"]["data"]["outcome"], "occupant_changed",
        "same-command respawn must wake the pinned wait: {response}"
    );
    assert!(
        response["error"]["data"]["waited_ms"]
            .as_u64()
            .expect("waited_ms")
            < 6000,
        "the wait ended by timeout instead of the pid edge: {response}"
    );
    rig.shutdown().await;
}

/// The other half of the same binding: when the agent exits and its shell
/// survives, the wait must say occupant_changed, not sit out its budget.
///
/// The pane root never moves here, because the root IS the shell, so the
/// root-pid comparison sees nothing. What changes is the process in front
/// of the tty, and the answer has to come from that domain.
#[tokio::test]
async fn a_foreground_agent_exiting_ends_the_wait_even_though_the_shell_survives() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    if std::process::Command::new("zsh")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: no zsh to be the pane's shell");
        return;
    }
    let mut rig = Rig::new("waitexit", WAIT_MANIFEST, "zsh -f", "").await;
    let pane = rig.pane_ids().await[0].clone();
    rig.tmux.run_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("exec 2>/dev/null; python3 {}", faketui_path()),
        "Enter",
    ]);
    rig.tmux.wait_screen(&pane, "❯");
    rig.label(&pane, "worker").await;
    // A turn is running, so nothing about the fused state will satisfy
    // the wait on its own: only the occupant check can end it.
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "WORKING the turn"]);
    wait_pane_state(&mut rig, "working").await;

    let socket = rig.tmux.socket().to_string();
    let pane_for_driver = pane.clone();
    let driver = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        // Ctrl-C: the fake TUI exits and zsh, which tmux spawned and
        // which is still very much alive, comes back to the foreground.
        let _ = std::process::Command::new("tmux")
            .args(["-u", "-L", &socket, "-f", "/dev/null"])
            .args(["send-keys", "-t", &pane_for_driver, "C-c"])
            .status();
    });

    let resp = rig
        .ctl
        .request(
            "agent.wait",
            json!({"target": "worker", "until": "done", "timeout_ms": 6000}),
        )
        .await;
    driver.join().expect("driver thread");

    assert_eq!(
        resp["error"]["data"]["outcome"], "occupant_changed",
        "the agent it was waiting on is gone: {resp}"
    );
    rig.shutdown().await;
}
