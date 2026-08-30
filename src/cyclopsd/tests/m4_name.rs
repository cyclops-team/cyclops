//! Naming a pane, and the border it wears afterwards.
//!
//! What has to be true, and none of it can be checked by reading code: the
//! adoption reaches the roster and the record, it survives a daemon
//! restart, an explicit `--manifest` binds at once, the tmux writes land
//! on the pane and window cyclops claims and nowhere else, they follow the
//! pane between windows, and every one of them comes back on `--clear`, at
//! shutdown, and when `chrome = "off"` says not to write them at all.
//!
//! Every check asks tmux rather than the daemon. The daemon believing it
//! wrote a border is exactly the thing under test.
//!
//! Same isolated-tmux rig as the m1 tests (tests/common); the bounded
//! waits here are test-side and outside the daemon's zero-polling
//! contract.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

/// Title-tier fixture: a pane whose title starts with CYC-BUSY reads
/// working, anything else reads idle.
///
/// The title tier and not the screen tier, for two reasons. It is the
/// sensor a background recompute actually consults (screen capture is
/// evidence of last resort, amendment h: a title rule that matched means
/// no capture runs), and driving it from OUTSIDE the pane is the exact
/// thing cyclops must never do itself: the border can follow the title
/// sensor only because chrome does not write the title (F26).
const CHROME_MANIFEST: &str = r#"
[agent]
id = "fix"
display_name = "Chrome fixture"
process_names = ["cat", "sh", "bash", "dash"]

[[rule]]
id = "title_working"
state = "working"
priority = 1100
region = "pane_title"
regex = ['^CYC-BUSY']

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^']

[injection]
submit = "Enter"
verify_before_submit = true
verify_pattern = ["<message_id>"]
"#;

/// One tmux option at one scope, as the server reports it ("name value").
/// Empty means the option is not set AT THAT SCOPE, which is the state
/// every one of these has to come back to.
fn option(rig: &Rig, scope: &str, target: &str, option: &str) -> String {
    let out = rig.tmux.run(&["show-options", scope, "-t", target, option]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The raw format string cyclops installed on a pane, styling included.
fn border_format(rig: &Rig, pane: &str) -> String {
    let out = rig
        .tmux
        .run(&["show-options", "-p", "-t", pane, "-v", "pane-border-format"]);
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// The `#[fg=...]` color immediately in front of a reference, read off the
/// raw format. The expansion carries the text; the format carries the
/// styling, and this is the styling half.
fn fg_before(format: &str, reference: &str) -> String {
    let head = format
        .split(reference)
        .next()
        .expect("split yields a head")
        .to_string();
    let at = head
        .rfind("#[fg=")
        .unwrap_or_else(|| panic!("no color in front of {reference} in {format:?}"));
    head[at + 5..]
        .split(']')
        .next()
        .expect("a closed style run")
        .to_string()
}

/// What a pane's border actually renders, with every format directive and
/// option reference already expanded by tmux.
fn border_text(rig: &Rig, pane: &str) -> String {
    let out = rig.tmux.run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{E:pane-border-format}",
    ]);
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// Bounded test-side wait for the border to say something.
fn wait_border(rig: &Rig, pane: &str, needle: &str) -> String {
    let t = Instant::now();
    loop {
        let got = border_text(rig, pane);
        if got.contains(needle) {
            return got;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "border never said {needle:?}; it says {got:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn pane_labeled_lines(rig: &Rig) -> Vec<Value> {
    rig.ledger_lines()
        .into_iter()
        .filter(|l| l["data"]["event"] == json!("pane_labeled"))
        .collect()
}

/// A label typo cannot become valid through pane-table reconciliation. It
/// must fail from the registry fast path even when an unrelated watched
/// session's control client is alive but no longer making progress.
#[tokio::test(flavor = "multi_thread")]
async fn a_typo_fails_honestly_without_waiting_for_an_unrelated_session() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_multi(
        "m4name-typo-bound",
        CHROME_MANIFEST,
        &[("unrelated", "cat"), ("target", "cat")],
        "",
    )
    .await;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_name_reconcile_pause(move |session| {
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if session == "unrelated" {
                pause.acquire().await.expect("pause remains open").forget();
            }
        })
    });

    let response = tokio::time::timeout(
        Duration::from_millis(500),
        rig.ctl.request(
            "pane.label",
            json!({"target": "not-a-real-label", "label": "nobody"}),
        ),
    )
    .await;
    release.add_permits(1);
    rig.daemon.clear_name_reconcile_pause();
    let response = response.expect("a typo must not await an unrelated watcher");
    assert_eq!(
        response["error"]["code"],
        json!("no_such_target"),
        "{response}"
    );

    rig.shutdown().await;
}

/// A raw pane id identifies one pane on one tmux server. If that pane is new
/// enough to miss the cache, naming must refresh only its owning session; a
/// stalled unrelated watcher cannot sit in front of it.
#[tokio::test(flavor = "multi_thread")]
async fn naming_a_new_pane_does_not_touch_or_await_an_unrelated_session() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_multi(
        "m4name-one-owner",
        CHROME_MANIFEST,
        &[("unrelated", "cat"), ("target", "cat")],
        "",
    )
    .await;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_name_reconcile_pause(move |session| {
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            if session == "unrelated" {
                pause.acquire().await.expect("pause remains open").forget();
            }
        })
    });

    let split = rig.tmux.run(&[
        "split-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "target",
        "cat",
    ]);
    assert!(split.status.success(), "split failed: {split:?}");
    let pane = String::from_utf8_lossy(&split.stdout).trim().to_string();

    let response = tokio::time::timeout(
        Duration::from_secs(1),
        rig.ctl.request(
            "pane.label",
            json!({"target": pane, "label": "fresh-reviewer"}),
        ),
    )
    .await;
    release.add_permits(1);
    rig.daemon.clear_name_reconcile_pause();
    let response = response.expect("the target session must bypass an unrelated watcher");
    assert_eq!(
        response["result"]["label"],
        json!("fresh-reviewer"),
        "{response}"
    );

    rig.shutdown().await;
}

/// Even the one owning watcher is an external dependency. If its explicit
/// reconcile stops making progress, the socket request still gets a bounded,
/// truthful answer instead of inheriting the watcher's unbounded queue wait.
#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_owner_reconcile_cannot_make_naming_unbounded() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4name-owner-bound", CHROME_MANIFEST, "cat", "").await;
    let entered = Arc::new(AtomicBool::new(false));
    let entered_pause = Arc::clone(&entered);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let pause = Arc::clone(&release);
    rig.daemon.set_name_reconcile_pause(move |_| {
        entered_pause.store(true, Ordering::SeqCst);
        let pause = Arc::clone(&pause);
        Box::pin(async move {
            pause.acquire().await.expect("pause remains open").forget();
        })
    });

    let split = rig.tmux.run(&[
        "split-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "main",
        "cat",
    ]);
    assert!(split.status.success(), "split failed: {split:?}");
    let pane = String::from_utf8_lossy(&split.stdout).trim().to_string();
    rig.wait_attached(2).await;

    let started = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        rig.ctl.request(
            "pane.label",
            json!({"target": pane, "label": "bounded-reviewer"}),
        ),
    )
    .await;
    release.add_permits(1);
    rig.daemon.clear_name_reconcile_pause();
    let response = response.expect("the whole fallback must have an outer bound");
    assert!(
        entered.load(Ordering::SeqCst),
        "the request never exercised the stuck owner fallback: {response}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the naming fallback exceeded its outer bound: {response}"
    );
    assert!(
        response["result"]["label"] == json!("bounded-reviewer")
            || response["error"]["code"] == json!("no_such_target"),
        "the bounded response must reflect the live cache honestly: {response}"
    );

    rig.shutdown().await;
}

/// Naming is an explicit request about tmux's current pane population. A
/// structural notification only arms the watcher's 30ms reconcile, so a
/// pane created immediately before this request is real before it appears in
/// the cached table. The request must close that gap itself instead of making
/// the operator wait and retry.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_pane_can_be_named_before_the_structural_debounce_fires() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4name-fresh-pane", CHROME_MANIFEST, "cat", "").await;

    let split = rig.tmux.run(&[
        "split-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "main",
        "cat",
    ]);
    assert!(split.status.success(), "split failed: {split:?}");
    let pane = String::from_utf8_lossy(&split.stdout).trim().to_string();
    assert!(
        pane.starts_with('%'),
        "split did not print a pane id: {split:?}"
    );

    let response = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "fresh-reviewer"}),
        )
        .await;
    assert_eq!(
        response["result"]["label"],
        json!("fresh-reviewer"),
        "{response}"
    );

    rig.shutdown().await;
}

/// The round trip the verb promises: name it, see it on the roster, find
/// the fact on the record, clear it, and get tmux back unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn naming_a_pane_adopts_it_records_it_and_clearing_puts_tmux_back() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4name", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let window = {
        let out = rig
            .tmux
            .run(&["display-message", "-p", "-t", &pane, "#{window_id}"]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // What tmux looked like before cyclops touched it. Both are unset at
    // the scope cyclops writes, which is the case restore has to
    // reproduce by unsetting rather than by writing a value back.
    assert_eq!(option(&rig, "-p", &pane, "pane-border-format"), "");
    assert_eq!(option(&rig, "-w", &window, "pane-border-status"), "");
    let before = border_text(&rig, &pane);

    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "reviewer", "manifest": "fix"}),
        )
        .await;
    assert_eq!(resp["result"]["label"], json!("reviewer"), "{resp}");
    assert_eq!(resp["result"]["manifest"], json!("fix"), "{resp}");
    assert_eq!(resp["result"]["pane_id"], json!(pane), "{resp}");

    // 1. The roster carries it, and the label resolves as a target.
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        status["result"]["sessions"][0]["panes"][0]["agent"],
        json!("reviewer"),
        "{status}"
    );
    let read = rig
        .ctl
        .request("pane.read", json!({"target": "reviewer"}))
        .await;
    assert_eq!(read["result"]["pane_id"], json!(pane), "{read}");

    // 2. The record says it happened, with the pin on it.
    let named = pane_labeled_lines(&rig);
    assert_eq!(named.len(), 1, "{named:?}");
    assert_eq!(named[0]["kind"], json!("system"));
    assert_eq!(named[0]["data"]["label"], json!("reviewer"));
    assert_eq!(named[0]["data"]["pane_id"], json!(pane));
    assert_eq!(named[0]["data"]["manifest"], json!("fix"));

    // 3. The border says who and how, and the writes are where cyclops
    //    said they would be: this pane, this window, nothing global.
    let text = wait_border(&rig, &pane, "reviewer");
    assert!(text.contains('•'), "{text:?}");
    assert!(text.contains("idle"), "{text:?}");
    assert_eq!(
        option(&rig, "-w", &window, "pane-border-status"),
        "pane-border-status top"
    );
    assert!(
        option(&rig, "-p", &pane, "@cyclops_role").contains("reviewer"),
        "the label rides a pane option, not the format string"
    );
    // The server-global options are the ones a daemon must never touch:
    // they would follow the user into every other session on this server.
    let global = rig.tmux.run(&["show-options", "-g", "pane-border-status"]);
    assert!(
        String::from_utf8_lossy(&global.stdout).contains("off"),
        "chrome reached the server-global scope"
    );

    // 4. Clearing gives all of it back.
    let resp = rig
        .ctl
        .request("pane.label", json!({"target": "reviewer", "label": null}))
        .await;
    assert!(resp["result"]["label"].is_null(), "{resp}");
    assert_eq!(option(&rig, "-p", &pane, "pane-border-format"), "");
    assert_eq!(option(&rig, "-w", &window, "pane-border-status"), "");
    assert_eq!(option(&rig, "-p", &pane, "@cyclops_role"), "");
    assert_eq!(option(&rig, "-p", &pane, "@cyclops_state"), "");
    assert_eq!(border_text(&rig, &pane), before);

    let named = pane_labeled_lines(&rig);
    assert_eq!(named.len(), 2, "the clear is on the record too");
    assert!(named[1]["data"]["label"].is_null());

    rig.assert_ledger_legal(&[]);
    rig.shutdown().await;
}

/// The border is a view of the fused state, so it moves when the state
/// moves and it moves on that edge alone. Nothing here pokes the chrome:
/// the pane is driven, fusion notices, and tmux is asked what it renders.
#[tokio::test(flavor = "multi_thread")]
async fn a_state_change_rewrites_the_border() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4chrome", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "implementer").await;
    let idle = wait_border(&rig, &pane, "○ idle");
    assert!(idle.contains("implementer"), "{idle:?}");
    let idle_fmt = border_format(&rig, &pane);

    // The pane goes busy the way a real agent does it: by publishing a
    // title. tmux re-evaluates the subscription on its own tick (F23), so
    // the edge arrives within a second or so.
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "CYC-BUSY"]);
    let busy = wait_border(&rig, &pane, "● working");
    assert!(busy.contains("implementer"), "{busy:?}");
    let busy_fmt = border_format(&rig, &pane);

    // The state color moves with the state: quiet and healthy are
    // different groups in every shipped theme, and the border reads the
    // same tokens the grid does.
    assert_ne!(
        fg_before(&idle_fmt, "#{@cyclops_state}"),
        fg_before(&busy_fmt, "#{@cyclops_state}"),
        "the state cell kept one color across a group change:\n{idle_fmt:?}\n{busy_fmt:?}"
    );
    // The name keeps its own color through the change: the two encodings
    // never share a cell (GOALS).
    assert_eq!(
        fg_before(&idle_fmt, "#{@cyclops_role}"),
        fg_before(&busy_fmt, "#{@cyclops_role}")
    );

    rig.shutdown().await;
}

/// One pane observation owns its chrome projection through the last tmux
/// write. A concurrent refresh must report honest incompleteness rather than
/// overtake a stalled older repaint and leave mixed or stale border options.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_state_observation_waits_for_prior_chrome_repaint() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4chrome-order", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "implementer").await;
    wait_border(&rig, &pane, "○ idle");

    let repaint = rig.daemon.pause_next_chrome_repaint_for_test();
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "CYC-BUSY"]);
    tokio::time::timeout(Duration::from_secs(8), repaint.wait_until_entered())
        .await
        .expect("working observation did not reach the chrome boundary");

    // Queue a newer idle edge, then ask a separate socket task to observe the
    // same pane while the prior repaint is still held. The named status budget
    // must expire on the retained pane gate; without that gate the newer
    // observation overtakes this repaint and answers from interleaved chrome.
    rig.tmux
        .run_ok(&["select-pane", "-t", &pane, "-T", "CYC-IDLE"]);
    let blocked = rig.ctl.request("status", json!({})).await;
    let blocked_pane = blocked["result"]["sessions"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["pane_id"] == pane)
        .unwrap();
    assert_eq!(
        blocked_pane["write_block"], "status_refresh_incomplete",
        "a concurrent observation overtook the retained repaint: {blocked}"
    );

    repaint.release();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let current = status["result"]["sessions"][0]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["pane_id"] == pane)
            .unwrap();
        if current["state"] == "idle" && current["write_block"] != "status_refresh_incomplete" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "newest idle observation never completed: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let idle = wait_border(&rig, &pane, "○ idle");
    assert!(idle.contains("implementer"), "{idle:?}");

    rig.shutdown().await;
}

/// A restart must not unname the team. The registry is a file, and the
/// pane it points at has to still be the same pane.
#[tokio::test(flavor = "multi_thread")]
async fn adoptions_survive_a_daemon_restart_and_the_border_comes_back() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4restart", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "reviewer").await;
    wait_border(&rig, &pane, "reviewer");

    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        status["result"]["sessions"][0]["panes"][0]["agent"],
        json!("reviewer"),
        "the name did not survive the restart: {status}"
    );
    wait_border(&rig, &pane, "reviewer");
    rig.shutdown().await;
}

/// Shutdown is not a crash: the daemon takes its decoration off on the way
/// out, because a border claiming a live state with nobody watching it is
/// the record lying. The adoption itself stays.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_takes_the_chrome_back_off() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4shutdown", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let window = {
        let out = rig
            .tmux
            .run(&["display-message", "-p", "-t", &pane, "#{window_id}"]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    rig.label(&pane, "reviewer").await;
    wait_border(&rig, &pane, "reviewer");

    rig.daemon.shutdown().await;
    assert_eq!(option(&rig, "-p", &pane, "pane-border-format"), "");
    assert_eq!(option(&rig, "-w", &window, "pane-border-status"), "");
}

/// `chrome = "off"` means no tmux option is written at all. The adoption
/// still happens: the switch is about decoration, not about the roster.
#[tokio::test(flavor = "multi_thread")]
async fn chrome_off_writes_no_tmux_option() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4off", CHROME_MANIFEST, "cat", "chrome = \"off\"\n").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let window = {
        let out = rig
            .tmux
            .run(&["display-message", "-p", "-t", &pane, "#{window_id}"]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let before = border_text(&rig, &pane);
    rig.label(&pane, "reviewer").await;
    // Give a chrome write time to happen if it were going to.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        status["result"]["sessions"][0]["panes"][0]["agent"],
        json!("reviewer"),
        "the switch turned off the roster, not just the paint: {status}"
    );
    assert_eq!(option(&rig, "-p", &pane, "pane-border-format"), "");
    assert_eq!(option(&rig, "-w", &window, "pane-border-status"), "");
    assert_eq!(option(&rig, "-p", &pane, "@cyclops_role"), "");
    assert_eq!(border_text(&rig, &pane), before);

    rig.shutdown().await;
}

/// Names are addresses, and a manifest pin has to name something real.
/// Both refusals happen before anything is written.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_name_writes_nothing() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4refuse", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    rig.tmux.run_ok(&["split-window", "-t", "main", "cat"]);
    rig.wait_attached(2).await;
    let panes = rig.pane_ids().await;
    rig.label(&panes[0], "reviewer").await;

    // The same name on a second pane.
    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": panes[1], "label": "reviewer"}),
        )
        .await;
    assert_eq!(resp["error"]["code"], json!("bad_request"), "{resp}");
    // The refusal names the holder and the way out. "already taken"
    // alone once had an operator distrusting the roster: the words must
    // say which pane wears the name, where, and how to free it.
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("already taken"), "{resp}");
    assert!(msg.contains(&panes[0]), "no holder pane in: {msg}");
    assert!(
        msg.contains("in session main"),
        "no holder session in: {msg}"
    );
    assert!(msg.contains("--clear"), "no remedy in: {msg}");

    // A manifest that was never loaded, named out loud with the ones that were.
    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": panes[1], "label": "tests", "manifest": "nope"}),
        )
        .await;
    assert_eq!(resp["error"]["code"], json!("bad_request"), "{resp}");
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("nope") && msg.contains("fix"), "{msg}");

    // A reserved name.
    for reserved in ["admin", "*", "%9"] {
        let resp = rig
            .ctl
            .request("pane.label", json!({"target": panes[1], "label": reserved}))
            .await;
        assert_eq!(resp["error"]["code"], json!("bad_request"), "{resp}");
    }

    // Nothing above named the second pane, and nothing above is on the record.
    let status = rig.ctl.request("status", json!({})).await;
    assert!(
        status["result"]["sessions"][0]["panes"][1]["agent"].is_null(),
        "{status}"
    );
    assert_eq!(pane_labeled_lines(&rig).len(), 1);

    rig.shutdown().await;
}

/// Adoption ends with the pane (M1 rule), so the name is free the moment
/// the pane dies. The live-use bug this pins against: a label "already
/// taken" by a holder no roster shows.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_panes_label_is_free_to_claim() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4panegone", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    rig.tmux.run_ok(&["split-window", "-t", "main", "cat"]);
    rig.wait_attached(2).await;
    let panes = rig.pane_ids().await;
    rig.label(&panes[1], "human").await;

    let victim = panes[1].clone();
    rig.tmux.run_ok(&["kill-pane", "-t", &victim]);
    rig.ev
        .wait_event(Duration::from_secs(10), |v| {
            v["event"] == json!("pane-removed") && v["data"]["pane_id"] == json!(victim)
        })
        .await;

    rig.label(&panes[0], "human").await;
    rig.shutdown().await;
}

/// Killing a whole session sends no PaneRemoved for its panes: the
/// control connection just drops (F25 covers pane death, not session
/// death). The labels are released where the daemon learns the session is
/// gone, the attach loop, or they stay claimed forever while `cyclops
/// list` says there are no agents at all.
#[tokio::test(flavor = "multi_thread")]
async fn killing_a_watched_session_releases_its_labels() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_multi(
        "m4sesskill",
        CHROME_MANIFEST,
        &[("main", "cat"), ("aux", "cat")],
        "",
    )
    .await;
    let aux_pane = rig.pane_ids_session(1).await[0].clone();
    rig.label(&aux_pane, "human").await;
    rig.tmux.run_ok(&["kill-session", "-t", "aux"]);

    // Test-side bounded wait: the daemon notices on a reconnect attempt,
    // and reconnects start at 200ms.
    let main_pane = rig.pane_ids().await[0].clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = rig
            .ctl
            .request(
                "pane.label",
                json!({"target": &main_pane, "label": "human"}),
            )
            .await;
        if resp["result"]["label"] == json!("human") {
            break;
        }
        let msg = resp["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("already taken"), "unexpected refusal: {resp}");
        assert!(
            Instant::now() < deadline,
            "the killed session's label was never released: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    rig.shutdown().await;
}

/// A restart re-verifies what it resurrects from the registry file. A
/// session the new run does not watch never attaches, so the attach
/// reconcile can never prune its entries: while the session lives its
/// names are kept (and the refusal says who holds them and where), and
/// once it is gone the next boot releases them.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_reverifies_labels_from_sessions_it_no_longer_watches() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new_multi(
        "m4unwatched",
        CHROME_MANIFEST,
        &[("main", "cat"), ("aux", "cat")],
        "",
    )
    .await;
    let aux_pane = rig.pane_ids_session(1).await[0].clone();
    rig.label(&aux_pane, "human").await;

    // The restart drops aux from the watched set: the shape of every
    // runtime-watched workspace session after a daemon restart, because
    // session.watch does not rewrite config.toml.
    rig.sessions = vec!["main".to_string()];
    rig.rewrite_config("");
    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;

    // aux still exists, so its pane still wears the name.
    let main_pane = rig.pane_ids().await[0].clone();
    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": &main_pane, "label": "human"}),
        )
        .await;
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("already taken"), "{resp}");
    assert!(msg.contains(&aux_pane), "no holder pane in: {msg}");
    assert!(msg.contains("aux"), "no holder session in: {msg}");

    // The session dies while nothing watches it: no subscription, no
    // reconcile. The next boot is the only verifier left.
    rig.tmux.run_ok(&["kill-session", "-t", "aux"]);
    let mut rig = rig.reboot().await;
    rig.wait_attached(1).await;
    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": &main_pane, "label": "human"}),
        )
        .await;
    assert_eq!(
        resp["result"]["label"],
        json!("human"),
        "the dead session's label was not released at boot: {resp}"
    );
    rig.shutdown().await;
}

/// A manifest nothing autodetects: the process name matches no pane the
/// rig can start, so this manifest binds only when a person pins it.
const PINNED_MANIFEST: &str = r#"
[agent]
id = "pinned"
display_name = "Pin fixture"
process_names = ["cyclops-no-such-command"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^']

[injection]
submit = "Enter"
"#;

/// `--manifest` is for the panes detection cannot work out on its own, so
/// it has to bind AT THE MOMENT OF NAMING. Pinning it and finding out at
/// the next unrelated event would be the same silence it exists to fix.
#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_manifest_binds_as_soon_as_the_pane_is_named() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4pin", PINNED_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();

    // Nothing binds by process, so nothing decides.
    let before = rig
        .ctl
        .request("pane.read", json!({"target": pane, "source": "detection"}))
        .await;
    assert_eq!(
        before["result"]["detection"]["decided_by"],
        json!("no_manifest"),
        "{before}"
    );

    let resp = rig
        .ctl
        .request(
            "pane.label",
            json!({"target": pane, "label": "reviewer", "manifest": "pinned"}),
        )
        .await;
    assert!(resp["error"].is_null(), "{resp}");

    // The very next question, with nothing else touching the pane.
    let status = rig.ctl.request("status", json!({})).await;
    let row = &status["result"]["sessions"][0]["panes"][0];
    assert_eq!(row["manifest"], json!("pinned"), "{status}");
    assert_eq!(row["state"], json!("idle"), "{status}");
    assert!(wait_border(&rig, &pane, "○ idle").contains("reviewer"));

    // And clearing the name gives detection back to the process.
    let resp = rig
        .ctl
        .request("pane.label", json!({"target": "reviewer", "label": null}))
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    let status = rig.ctl.request("status", json!({})).await;
    assert!(
        status["result"]["sessions"][0]["panes"][0]["manifest"].is_null(),
        "{status}"
    );

    rig.shutdown().await;
}

/// The manifest's display name is daemon identity data: the daemon loaded
/// it off the manifest TOML at boot, same as the id, so a pane bound to a
/// manifest carries it in status. This is what lets a client render the
/// name without re-parsing manifest files itself (the ownership cleanup in
/// `.agents/planning/2026-08-03-cyclops-workspace-tui/recommendation.md`).
#[tokio::test(flavor = "multi_thread")]
async fn a_bound_manifest_s_display_name_rides_along_in_status() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    // CAT_MANIFEST binds by process name alone (id "fix", display name
    // "Cat fixture"), so no explicit pane.label is needed to bind it.
    let mut rig = Rig::new("m4dispname", CAT_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rig.ctl.request("status", json!({})).await;
        let row = &status["result"]["sessions"][0]["panes"][0];
        if row["manifest"] == json!("fix") {
            assert_eq!(
                row["manifest_display_name"],
                json!("Cat fixture"),
                "{status}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "manifest never bound: {status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    rig.shutdown().await;
}

/// Border text is a window setting with no pane scope, so it does not
/// travel with a pane the way the pane's own options do. A named pane that
/// moves has to take the border text with it, or the window it left keeps
/// showing border text over nothing named.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_pane_that_changes_window_takes_its_border_with_it() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4move", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let source = window_of(&rig, &pane);
    rig.label(&pane, "reviewer").await;
    wait_border(&rig, &pane, "reviewer");
    assert_eq!(
        option(&rig, "-w", &source, "pane-border-status"),
        "pane-border-status top"
    );

    // A second window with a pane to join onto, then move the named pane
    // into it. break-pane/join-pane is the tmux gesture this covers.
    rig.tmux.run_ok(&["new-window", "-t", "main", "-d", "cat"]);
    rig.wait_attached(2).await;
    let other = rig
        .pane_ids()
        .await
        .into_iter()
        .find(|p| *p != pane)
        .expect("a second pane");
    let destination = window_of(&rig, &other);
    assert_ne!(source, destination);
    rig.tmux
        .run_ok(&["join-pane", "-s", &pane, "-t", &other, "-d"]);

    // The destination lights up and the source goes dark, both without
    // anyone naming or renaming anything.
    let t = Instant::now();
    loop {
        let dest_on = option(&rig, "-w", &destination, "pane-border-status").contains("top");
        let source_off = option(&rig, "-w", &source, "pane-border-status").is_empty();
        if dest_on && source_off {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "border did not follow the pane: destination={:?} source={:?}",
            option(&rig, "-w", &destination, "pane-border-status"),
            option(&rig, "-w", &source, "pane-border-status")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(wait_border(&rig, &pane, "reviewer").contains('•'));

    // Shutdown gives the destination back too, so the move left nothing
    // behind at either end. `daemon.shutdown` rather than `rig.shutdown`
    // because the tmux server has to outlive the daemon to be asked.
    rig.daemon.shutdown().await;
    assert_eq!(option(&rig, "-w", &destination, "pane-border-status"), "");
}

/// `chrome = "off"` turns writing off, not reading.
///
/// A named pane that moves into a window cyclops has never looked at still
/// has to leave that window's own border setting recorded. Otherwise the
/// day chrome comes back on, taking the name off unsets an option cyclops
/// never read, and the user's own setting is gone. Same rule as the
/// snapshot at adoption, same reason.
#[tokio::test(flavor = "multi_thread")]
async fn chrome_off_still_records_the_window_a_named_pane_moves_into() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4offmove", CHROME_MANIFEST, "cat", "chrome = \"off\"\n").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();

    // A second window, wearing a border setting of the user's own. This is
    // the value the restore has to reproduce.
    rig.tmux.run_ok(&["new-window", "-t", "main", "-d", "cat"]);
    rig.wait_attached(2).await;
    let other = rig
        .pane_ids()
        .await
        .into_iter()
        .find(|p| *p != pane)
        .expect("a second pane");
    let destination = window_of(&rig, &other);
    rig.tmux.run_ok(&[
        "set-option",
        "-w",
        "-t",
        &destination,
        "pane-border-status",
        "bottom",
    ]);

    rig.label(&pane, "reviewer").await;
    rig.tmux
        .run_ok(&["join-pane", "-s", &pane, "-t", &other, "-d"]);
    wait_registry_window(&rig, &pane, &destination);

    // Chrome back on, same registry: the daemon now paints, and the name
    // comes off the way a user takes it off.
    rig.rewrite_config("");
    let mut rig = rig.reboot().await;
    rig.wait_attached(2).await;
    wait_border(&rig, &pane, "reviewer");
    let resp = rig
        .ctl
        .request("pane.label", json!({"target": "reviewer", "label": null}))
        .await;
    assert!(resp["error"].is_null(), "{resp}");

    assert_eq!(
        option(&rig, "-w", &destination, "pane-border-status"),
        "pane-border-status bottom",
        "the clear unset a window option cyclops never read"
    );
    rig.shutdown().await;
}

/// The raw value of a window's `pane-border-status`, as tmux holds it.
fn border_status(rig: &Rig, window: &str) -> String {
    let out = rig.tmux.run(&[
        "show-options",
        "-w",
        "-t",
        window,
        "-v",
        "pane-border-status",
    ]);
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// The registry file's own copy of the pre-cyclops chrome: the pane's
/// `pane-border-format` and its window's `pane-border-status`, as they
/// were snapshotted at adoption.
///
/// Read off the file rather than asked of the daemon, because the file is
/// the only thing that survives the daemon, and it is the only copy of
/// those values left once tmux is wearing cyclops's.
fn registry_snapshot(rig: &Rig, pane: &str, window: &str) -> (Option<String>, Option<String>) {
    let text = std::fs::read_to_string(rig.home.join("registry.json")).unwrap_or_default();
    let file: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let format = file["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["pane_id"] == json!(pane))
        .and_then(|p| p["border_format"].as_str())
        .map(str::to_string);
    let status = file["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| w["window_id"] == json!(window))
        .and_then(|w| w["border_status"].as_str())
        .map(str::to_string);
    (format, status)
}

/// The user's own chrome, written before cyclops has ever looked at this
/// pane. Distinctive on purpose: every assertion below is about getting
/// exactly these two values back and not cyclops's.
const MINE_FORMAT: &str = "MINE-BORDER";
const MINE_STATUS: &str = "bottom";

/// A `--clear` whose chrome restore fails must not take the snapshot with
/// it.
///
/// By the time `--clear` runs, tmux is wearing cyclops's border and the
/// registry entry holds the only copy of what the user had. Committing the
/// removal before the restore lands throws both away in one step: the pane
/// keeps cyclops's decoration, and nothing on the machine knows what was
/// under it. So the entry is the last thing to go.
#[tokio::test(flavor = "multi_thread")]
async fn a_clear_whose_restore_fails_keeps_the_name_and_the_snapshot() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4clearfail", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let window = window_of(&rig, &pane);

    // The settings the user owns. Nothing else on the machine records them.
    rig.tmux.run_ok(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "pane-border-format",
        MINE_FORMAT,
    ]);
    rig.tmux.run_ok(&[
        "set-option",
        "-w",
        "-t",
        &window,
        "pane-border-status",
        MINE_STATUS,
    ]);

    rig.label(&pane, "reviewer").await;
    wait_border(&rig, &pane, "reviewer");
    assert_eq!(
        registry_snapshot(&rig, &pane, &window),
        (Some(MINE_FORMAT.to_string()), Some(MINE_STATUS.to_string())),
        "adoption did not record what the user had"
    );

    // tmux refuses the restore. The verb has to fail rather than half-run.
    rig.daemon.fail_chrome_restore(true);
    let resp = rig
        .ctl
        .request("pane.label", json!({"target": "reviewer", "label": null}))
        .await;
    assert_eq!(
        resp["error"]["code"],
        json!("chrome_not_restored"),
        "{resp}"
    );
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    // What happened, what is still cyclops's, and how to retry.
    assert!(msg.contains(&pane), "{msg}");
    assert!(msg.contains("reviewer"), "{msg}");
    assert!(msg.contains("pane-border-format"), "{msg}");
    assert!(msg.contains("pane-border-status"), "{msg}");
    assert!(msg.contains(&window), "{msg}");
    assert!(
        msg.contains(&format!("cyclops name {pane} --clear")),
        "{msg}"
    );

    // Nothing was given up. The name is still on the roster, the snapshot
    // is still in the file, and the clear is not on the record.
    let status = rig.ctl.request("status", json!({})).await;
    assert_eq!(
        status["result"]["sessions"][0]["panes"][0]["agent"],
        json!("reviewer"),
        "the name went even though the border did not: {status}"
    );
    assert_eq!(
        registry_snapshot(&rig, &pane, &window),
        (Some(MINE_FORMAT.to_string()), Some(MINE_STATUS.to_string())),
        "the only copy of the user's border settings was deleted by a failed clear"
    );
    assert_eq!(
        pane_labeled_lines(&rig).len(),
        1,
        "a clear that did not happen is on the record"
    );
    // And tmux is untouched by the failed attempt: still cyclops's border.
    assert!(border_format(&rig, &pane).contains("@cyclops_role"));

    rig.shutdown().await;
}

/// The retry, which is the half a lost snapshot makes impossible.
///
/// Two ways a user gets here after a failed clear: they run `--clear`
/// again, or they rename the pane first and clear later. Both have to end
/// on the values tmux had before cyclops, never on cyclops's own format
/// re-snapshotted as if it were the user's.
#[tokio::test(flavor = "multi_thread")]
async fn a_clear_retried_after_a_failure_restores_the_users_own_border() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("m4clearretry", CHROME_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    let pane = rig.pane_ids().await[0].clone();
    let window = window_of(&rig, &pane);
    rig.tmux.run_ok(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "pane-border-format",
        MINE_FORMAT,
    ]);
    rig.tmux.run_ok(&[
        "set-option",
        "-w",
        "-t",
        &window,
        "pane-border-status",
        MINE_STATUS,
    ]);
    rig.label(&pane, "reviewer").await;
    wait_border(&rig, &pane, "reviewer");

    // One failed clear.
    rig.daemon.fail_chrome_restore(true);
    let resp = rig
        .ctl
        .request("pane.label", json!({"target": "reviewer", "label": null}))
        .await;
    assert_eq!(
        resp["error"]["code"],
        json!("chrome_not_restored"),
        "{resp}"
    );

    // A rename in between, which is where the poisoning would happen: an
    // adoption that finds no snapshot on file reads the pane, and the pane
    // is wearing cyclops's format by now.
    let resp = rig
        .ctl
        .request("pane.label", json!({"target": &pane, "label": "tests"}))
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    wait_border(&rig, &pane, "tests");
    assert_eq!(
        registry_snapshot(&rig, &pane, &window),
        (Some(MINE_FORMAT.to_string()), Some(MINE_STATUS.to_string())),
        "renaming re-snapshotted cyclops's own border as the thing to restore"
    );

    // tmux answers again, and the retry lands on the user's values.
    rig.daemon.fail_chrome_restore(false);
    let resp = rig
        .ctl
        .request("pane.label", json!({"target": "tests", "label": null}))
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(
        border_format(&rig, &pane),
        MINE_FORMAT,
        "the retry put something other than the user's own format back"
    );
    assert_eq!(border_status(&rig, &window), MINE_STATUS);
    assert_eq!(option(&rig, "-p", &pane, "@cyclops_role"), "");
    assert_eq!(option(&rig, "-p", &pane, "@cyclops_state"), "");
    let status = rig.ctl.request("status", json!({})).await;
    assert!(
        status["result"]["sessions"][0]["panes"][0]["agent"].is_null(),
        "{status}"
    );

    rig.shutdown().await;
}

/// Bounded test-side wait for the registry to record a pane's window. The
/// registry file is where the move is durable, and the only place the
/// destination snapshot can be seen before anything paints.
fn wait_registry_window(rig: &Rig, pane: &str, window: &str) {
    let t = Instant::now();
    loop {
        let text = std::fs::read_to_string(rig.home.join("registry.json")).unwrap_or_default();
        let file: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let got = file["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["pane_id"] == json!(pane))
            .and_then(|p| p["window_id"].as_str())
            .unwrap_or_default()
            .to_string();
        if got == window {
            return;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "the registry never moved {pane} to {window}; it says {got:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The window a pane sits in, as tmux reports it.
fn window_of(rig: &Rig, pane: &str) -> String {
    let out = rig
        .tmux
        .run(&["display-message", "-p", "-t", pane, "#{window_id}"]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
