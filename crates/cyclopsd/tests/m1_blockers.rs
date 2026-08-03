//! Final M1 gate blockers: the codex escaped-capture discriminator wired
//! into fusion and the gate, the pane-rebind re-check between gate admit
//! and paste/submit, send-and-wait entries for pane-less recipients, and
//! restart closure of pre-hosted-field ledgers. Same isolated-tmux rig as
//! m1.rs (tests/common).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

/// The shipped codex esc rules, bound to the sh/cat fixture panes: dim
/// text after the composer glyph is a ghost suggestion (idle), bare text
/// is typed input (idle_with_input), the plain rule is the idle-biased
/// fallback that used to be the only thing that could fire.
const ESC_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Codex esc fixture"
process_names = ["cat", "sh", "bash", "dash"]

[[rule]]
id = "composer_typed_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+[^\x1b\s]']

[[rule]]
id = "composer_ghost_suggestion"
state = "idle"
priority = 1040
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+\x1b\[2m']

[[rule]]
id = "composer_empty_or_ghost"
state = "idle"
priority = 1000
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*›']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
"#;

/// Poll status until the pane's fused state reads `want` (test-side wait,
/// outside the daemon's zero-polling contract).
async fn wait_pane_state(rig: &mut Rig, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = rig.ctl.request("status", json!({})).await;
        let state = resp["result"]["sessions"][0]["panes"][0]["state"].clone();
        if state == json!(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane never reached state {want}: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll status until the pane's foreground command is no longer `old`
/// (macOS reports /bin/sh's comm as "bash", so the new name is not
/// portable). The subscription push that carries the command change
/// carries the new pane_pid too, so this doubles as "the watcher table
/// saw the occupant swap".
async fn wait_pane_command_away_from(rig: &mut Rig, old: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = rig.ctl.request("status", json!({})).await;
        let cmd = resp["result"]["sessions"][0]["panes"][0]["current_command"].clone();
        if cmd != json!(old) && !cmd.is_null() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pane command never left {old}: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// HIGH 1: the daemon supplies escaped captures wherever the bound
/// manifest carries line_regex_esc rules, so codex typed text finally
/// reads as idle_with_input (F19) and the gate holds instead of pasting
/// into a human's draft. The pane renders the EXACT measured codex
/// composer lines (bold glyph, dim ghost vs bare typed text) with real
/// ESC bytes via printf.
#[tokio::test(flavor = "multi_thread")]
async fn escaped_capture_flips_typed_text_to_idle_with_input_and_gates() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // Measured composer lines from the F19 ghost probe (fixtures in
    // crates/cyclops-manifest/tests/fixtures/): ESC[1m glyph ESC[0m, then
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

    // Ghost suggestion on screen: idle, safe. Before the fix this is the
    // best codex could ever report.
    wait_pane_state(&mut rig, "idle").await;

    // The human "types": the composer line becomes bare text. Only the
    // escaped capture can tell this apart from the ghost (the plain
    // captures are identical), so before the fix this stayed idle.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    wait_pane_state(&mut rig, "idle_with_input").await;

    // A delivery against typed text must hold in gating: human wins.
    let (result, _) = rig
        .send(json!({"to": ["codexy"], "subject": "hold this", "body": "payload-body"}))
        .await;
    let msg_id = result["msg_id"].as_str().unwrap().to_string();
    assert_eq!(result["deliveries"][0]["state"], "queued", "{result}");
    let hold = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "hold"
        })
        .await;
    assert_eq!(hold["data"]["cause"], "idle_with_input", "{hold}");
    // Nothing was typed into the human's draft.
    assert!(
        !rig.tmux.capture(&pane).contains("[cyclops"),
        "payload pasted over typed human text"
    );

    // The typed text goes away, only ghost text remains: idle again, and
    // the held delivery proceeds on the state change.
    rig.tmux.run_ok(&["send-keys", "-t", &pane, "x", "Enter"]);
    rig.ev
        .wait_event(Duration::from_secs(10), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "delivered_unverified"
        })
        .await;
    assert!(rig.tmux.capture(&pane).contains(&msg_id));

    // Screen text stays out of the ledger.
    rig.assert_ledger_legal(&["gateway.rs", "Find and fix a bug"]);
    rig.shutdown().await;
}

/// Manifest binding only cat: after the occupant swap to a plain sh the
/// gate can no longer bind, so the retry ends in attention_required
/// instead of re-admitting.
const CAT_ONLY_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Cat-only fixture"
process_names = ["cat"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
"#;

/// Install an inject-pause seam that parks the delivery at `phase` and
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

/// HIGH 2: the pane occupant changes between the gate's admit and the
/// paste. The re-check must catch the rebind and never paste: on a shell
/// occupant the pasted message text would be EXECUTED.
#[tokio::test(flavor = "multi_thread")]
async fn pane_rebound_before_paste_never_pastes_into_the_new_occupant() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "rbpaste",
        CAT_ONLY_MANIFEST,
        "cat",
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

    // The gate admitted and the paste path is parked at the seam. Replace
    // the occupant with a plain shell and wait until the watcher table
    // saw the swap (command and pid travel in the same push).
    entered.recv().await.expect("paste path reached the seam");
    rig.tmux.run_ok(&["respawn-pane", "-k", "-t", &pane, "sh"]);
    wait_pane_command_away_from(&mut rig, "cat").await;
    release.add_permits(1);

    // The re-check caught the rebind: gate line, retry, and the retry's
    // gate pass cannot bind the shell, so the delivery needs a human.
    let rebound = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "rebound"
        })
        .await;
    assert_eq!(rebound["data"]["cause"], "pane_pid_changed", "{rebound}");
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "retry_queued"
                && e["data"]["cause"] == "pane_rebound"
        })
        .await;
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "attention_required"
        })
        .await;

    // THE assertion: nothing was ever typed into the shell.
    let screen = rig.tmux.capture(&pane);
    assert!(
        !screen.contains("[cyclops") && !screen.contains("rm -rf"),
        "payload reached the rebound shell:\n{screen}"
    );
    let pastes = rig
        .ledger_lines()
        .iter()
        .filter(|l| {
            l["kind"] == "state" && l["id"] == msg_id.as_str() && l["data"]["to_state"] == "staged"
        })
        .count();
    assert_eq!(pastes, 0, "a rebound pane still staged a paste");
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// HIGH 2, submit side: the occupant changes after the paste verified but
/// before the submit key. Enter must never reach the new occupant.
#[tokio::test(flavor = "multi_thread")]
async fn pane_rebound_before_submit_withholds_the_submit_key() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new(
        "rbsub",
        CAT_ONLY_MANIFEST,
        "cat",
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

    // Paste and verification ran; the path is parked just before Enter.
    entered.recv().await.expect("submit path reached the seam");
    rig.tmux.run_ok(&["respawn-pane", "-k", "-t", &pane, "sh"]);
    wait_pane_command_away_from(&mut rig, "cat").await;
    release.add_permits(1);

    let rebound = rig
        .ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "gate"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["action"] == "rebound"
        })
        .await;
    assert_eq!(rebound["data"]["cause"], "pane_pid_changed", "{rebound}");
    rig.ev
        .wait_event(Duration::from_secs(8), |e| {
            e["event"] == "delivery-state"
                && e["data"]["id"] == msg_id.as_str()
                && e["data"]["to_state"] == "attention_required"
        })
        .await;

    // The chain records the staged paste but never a submit.
    let lines = rig.ledger_lines();
    assert!(
        lines.iter().any(|l| {
            l["kind"] == "state" && l["id"] == msg_id.as_str() && l["data"]["to_state"] == "staged"
        }),
        "the pre-swap paste never staged"
    );
    let submits = lines
        .iter()
        .filter(|l| {
            l["kind"] == "state"
                && l["id"] == msg_id.as_str()
                && l["data"]["to_state"] == "submitted"
        })
        .count();
    assert_eq!(submits, 0, "the submit key was sent to a rebound pane");
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// MINOR 7: send-and-wait reports EVERY recipient. A pane-less recipient
/// has no pane to watch: its wait entry carries the resolved delivery
/// state (attention_required) and a null agent state instead of being
/// silently omitted from the wait array.
#[tokio::test(flavor = "multi_thread")]
async fn send_and_wait_reports_paneless_recipients() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("waitless", CAT_MANIFEST, "cat", "receipt_block_ms = 100\n").await;
    let (result, _) = rig
        .send(json!({
            "to": ["ghost"],
            "subject": "nobody home",
            "body": "b",
            "wait": {"until": "idle", "timeout_ms": 500},
        }))
        .await;
    assert_eq!(
        result["deliveries"][0]["state"], "attention_required",
        "{result}"
    );
    let waits = result["wait"].as_array().expect("wait array present");
    assert_eq!(waits.len(), 1, "pane-less recipient omitted: {result}");
    assert_eq!(waits[0]["to"], "ghost", "{result}");
    assert_eq!(waits[0]["delivery"], "attention_required", "{result}");
    assert!(waits[0]["state"].is_null(), "{result}");
    assert_eq!(waits[0]["outcome"], "not_delivered", "{result}");
    assert_eq!(waits[0]["waited_ms"], 0, "{result}");
    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// MINOR 8: msg lines written before the `hosted` field existed (old
/// single-file daemons) must still seed restart-limbo chains: a delivery
/// with no state lines in such a ledger closes at boot instead of
/// dangling forever.
#[tokio::test(flavor = "multi_thread")]
async fn restart_closes_pre_hosted_field_ledger_chains() {
    let home = cyclops_proto::scratch::scratch_dir("cyc-m1-oldhost");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("ledger")).expect("scratch home");
    let _guard = HomeGuard(home.clone());
    // A watched session that never attaches: limbo closure runs at boot
    // from the replayed ledger alone, no tmux needed. The attach retries
    // still autostart a server on this socket, so it goes through the rig
    // like every other server: unique name, and teardown on drop rather
    // than at the end of the body, where a failed assertion skips it.
    let tmux = TmuxGuard::new("oldhost-none");
    let socket = tmux.socket();
    std::fs::write(
        home.join("config.toml"),
        format!("sessions = [\"main\"]\ntmux_socket = \"{socket}\"\ntmux_config = \"/dev/null\"\n"),
    )
    .expect("config");
    // Old-daemon msg line: no data, no hosted list, delivery still queued.
    std::fs::write(
        home.join("ledger/main.ndjson"),
        concat!(
            r#"{"seq":1,"boot_id":"old","id":"m-old001","ts":1,"kind":"msg","from":"admin","#,
            r#""to":["worker"],"subject":"stuck","deliveries":"#,
            r#"[{"to":"worker","state":"queued","attempts":0,"ts":1}]}"#,
            "\n"
        ),
    )
    .expect("seed old ledger");

    let (cfg, _) = cyclopsd::Config::load(&home).expect("config loads");
    let daemon = cyclopsd::boot(cfg).await.expect("daemon boots");
    let text = std::fs::read_to_string(home.join("ledger/main.ndjson")).expect("ledger readable");
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("ledger line parses"))
        .collect();
    let closure = lines
        .iter()
        .find(|l| l["kind"] == "state" && l["id"] == "m-old001" && l["data"]["to"] == "worker")
        .expect("un-hosted chain was not closed at boot");
    assert_eq!(
        closure["data"]["to_state"], "attention_required",
        "{closure}"
    );
    assert_eq!(closure["data"]["cause"], "daemon_restart", "{closure}");
    assert!(
        lines.iter().any(|l| l["kind"] == "system"
            && l["subject"]
                .as_str()
                .is_some_and(|s| s.contains("interrupted by daemon restart"))),
        "no aggregated restart notification"
    );
    daemon.shutdown().await;
}
