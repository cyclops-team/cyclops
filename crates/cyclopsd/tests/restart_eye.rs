//! The eye against a real daemon restart.
//!
//! The restart-limbo closure is the one place the daemon pings about many
//! things at once (delivery.rs `close_limbo`), and it is the ping a reader
//! sees most: every restart with anything in flight emits it. The rule the
//! whole milestone is about is that in a single frame the eye's glyph and
//! the body cannot contradict each other, so this drives the real path,
//! writes a real ledger, and reads it back through the real stream reader.
//!
//! A hand-built ping proves nothing here. What the daemon writes at boot
//! has to be what the surface drawing the eye can hold to the register, so
//! the ping under test is the one `cyclopsd::boot` actually wrote.

mod common;

use common::{HomeGuard, TmuxGuard};
use cyclops_proto::StatusResult;
use cyclops_ui::{build, App, Entry, EntryKind, Eye, Filter, Intake, StatusSeed, Theme, View};
use serde_json::Value;

/// A previous run's ledger with `chains` deliveries still in flight: msg
/// lines whose delivery records never left `queued`, which is exactly what
/// a daemon killed mid-send leaves behind.
///
/// Written by hand because the point is the state of the record at boot,
/// and a real kill would leave the same four fields. Everything read back
/// below is the NEW daemon's own output.
fn ledger_with_deliveries_in_flight(home: &std::path::Path, chains: &[(&str, &str)]) {
    let lines: String = chains
        .iter()
        .enumerate()
        .map(|(i, (id, to))| {
            format!(
                "{}\n",
                serde_json::json!({
                    "seq": i + 1, "boot_id": "previous-run", "id": id, "ts": 1000 + i,
                    "kind": "msg", "from": "admin", "to": [to],
                    "subject": "in flight when the daemon died",
                    "deliveries": [{"to": to, "state": "queued", "attempts": 0, "ts": 1000 + i}],
                })
            )
        })
        .collect();
    std::fs::write(home.join("ledger/main.ndjson"), lines).expect("seed the previous run's ledger");
}

/// The UI's own startup, in the one order it can happen: the daemon's
/// answer, then the replayed tail, then the seed over it (cyclops-ui
/// `Intake`). `answer` is what `status` returned for this run.
fn ui_after_startup(home: &std::path::Path, answer: &StatusResult) -> App {
    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    let mut intake = Intake::new();
    let seed = StatusSeed::from_status(answer);
    let watched = seed.watched.clone();
    assert!(intake.status(Box::new(seed)).is_none());
    let (entries, max_seq) = cyclops_ui::read_backfill(&home.join("ledger"), 500, Some(&watched));
    let landed = intake.backfill(entries, max_seq);
    for e in landed.replayed {
        app.replay(e);
    }
    for e in app.seed_status(*landed.seed.expect("the seed came back")) {
        app.replay(e);
    }
    for e in landed.live {
        app.live(e);
    }
    // Settle the eye: the drawn glyph is what a frame shows.
    while app.tick_eye() {}
    app.tick_eye();
    app
}

/// Every clearance row in a frame, gutter stripped, so the assertions read
/// as the reader reads them: who, and what it ended.
fn cleared_rows(rows: &[String]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.contains("✔ cleared"))
        .map(|r| {
            r.trim_start()
                .split_once("  ")
                .map_or(String::new(), |(_clock, rest)| rest.into())
        })
        .collect()
}

/// Where the row for `who` that says `what` sits in the frame.
fn row_index(rows: &[String], who: &str, what: &str) -> usize {
    rows.iter()
        .position(|r| r.contains(who) && r.contains(what))
        .unwrap_or_else(|| panic!("no row for {who} saying {what}: {rows:#?}"))
}

/// The restart ping as the stream reader holds it, from the real ledger.
fn restart_ping(app: &App) -> &Entry {
    app.entries()
        .find(|e| {
            matches!(&e.kind, EntryKind::Notify { subject, .. }
                if subject.contains("interrupted by daemon restart"))
        })
        .expect("the daemon wrote no restart ping")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_restart_ping_never_outlives_the_deliveries_it_names() {
    let home = cyclops_proto::scratch::scratch_dir("cyc-restart-eye");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("ledger")).expect("scratch home");
    let _guard = HomeGuard(home.clone());
    // A watched session that never attaches: the limbo closure runs at
    // boot from the replayed ledger alone, no tmux needed. The attach
    // retries still autostart a server on this socket, so it goes through
    // the rig like every other server (unique name, teardown on drop).
    let tmux = TmuxGuard::new("restart-eye-none");
    let socket = tmux.socket();
    std::fs::write(
        home.join("config.toml"),
        format!("sessions = [\"main\"]\ntmux_socket = \"{socket}\"\ntmux_config = \"/dev/null\"\n"),
    )
    .expect("config");
    ledger_with_deliveries_in_flight(&home, &[("m-alpha", "reviewer"), ("m-beta", "implementer")]);

    let (cfg, _) = cyclopsd::Config::load(&home).expect("config loads");
    let daemon = cyclopsd::boot(cfg).await.expect("daemon boots");

    // 1. The daemon closed both chains and pinged once, naming both. The
    //    names are the register's key for a delivery: recipient, message.
    let ledger: Vec<Value> = std::fs::read_to_string(home.join("ledger/main.ndjson"))
        .expect("ledger readable")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("ledger line parses"))
        .collect();
    let pings: Vec<&Value> = ledger
        .iter()
        .filter(|l| {
            l["kind"] == "system"
                && l["subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("interrupted by daemon restart"))
        })
        .collect();
    assert_eq!(pings.len(), 1, "want one aggregated ping: {pings:?}");
    assert_eq!(
        pings[0]["data"]["deliveries"],
        serde_json::json!([
            {"to": "reviewer", "id": "m-alpha"},
            {"to": "implementer", "id": "m-beta"},
        ]),
        "the ping named something other than what it closed: {}",
        pings[0]
    );

    // 2. The honest case, and the reason the ping is not simply dropped:
    //    while the daemon still lists both, the eye is open, the calm view
    //    carries the ping, and the two agree.
    let answer = daemon.status(true);
    assert_eq!(answer.open_deliveries.len(), 2, "{:?}", answer);
    let mut app = ui_after_startup(&home, &answer);
    assert_eq!(app.attention_count(), 2);
    assert_eq!(app.eye(), Eye::Open);
    let rows = build(&mut app, 20);
    assert!(rows[0].contains("2 need attention"), "{:?}", rows[0]);
    assert!(
        rows.iter().any(|r| r.contains("action required")),
        "the calm view dropped a ping the register backs: {rows:#?}"
    );

    // 3. One of the two is dealt with. The ping named both, so it still
    //    stands: the other has not been dealt with, and the eye says so.
    let mut half_done = daemon.status(true);
    half_done.open_deliveries.retain(|d| d.to == "reviewer");
    let mut app = ui_after_startup(&home, &half_done);
    assert_eq!(app.attention_count(), 1);
    let rows = build(&mut app, 20);
    assert!(rows[0].contains("1 needs attention"), "{:?}", rows[0]);
    assert!(
        rows.iter().any(|r| r.contains("action required")),
        "one item left and the ping vanished: {rows:#?}"
    );
    // And the one that WAS dealt with says so, next to its own closure
    // line. The other has nothing to say yet.
    assert_eq!(
        cleared_rows(&rows),
        vec!["implementer  ✔ cleared · was ⚠ needs attention"]
    );

    // 4. Both dealt with: a later run's answer no longer lists either, and
    //    the ping is still in the ledger every run replays. A closed eye
    //    over "⚠ action required" is the contradiction this whole
    //    milestone is about, so the calm view may not take it.
    let mut cleared = daemon.status(true);
    cleared.open_deliveries.clear();
    let mut app = ui_after_startup(&home, &cleared);
    assert_eq!(app.attention_count(), 0);
    assert_eq!(app.eye(), Eye::Closed);
    let ping = restart_ping(&app).clone();
    assert!(
        !app.admits_in_view(&ping),
        "--plain would print this ping and can never take it back"
    );
    let rows = build(&mut app, 20);
    assert!(rows[0].starts_with("‿ cyclops"), "{:?}", rows[0]);
    assert!(
        !rows.iter().any(|r| r.contains("action required")),
        "a calm eye over a line saying a human is needed: {rows:#?}"
    );
    // The closure lines themselves stay: they are stamped transitions in
    // the record, and the record does not retract. What they no longer do
    // is stand alone. Each of them is a line saying a human is needed, and
    // under a closed eye each one is answered by the clearance the
    // register wrote when the answer stopped counting it
    // (cyclops_proto::attention, rule 3). Two lines that tell the whole
    // story; this used to be one line telling half of it.
    assert_eq!(
        rows.iter()
            .filter(|r| r.contains("daemon restarted mid-delivery"))
            .count(),
        2,
        "{rows:#?}"
    );
    assert_eq!(
        cleared_rows(&rows),
        vec![
            "implementer  ✔ cleared · was ⚠ needs attention",
            "reviewer  ✔ cleared · was ⚠ needs attention",
        ],
        "a closed eye over two rows saying a human is needed: {rows:#?}"
    );
    // Order is the reader's: the alarm, then the line that ends it.
    for who in ["reviewer", "implementer"] {
        let alarm = row_index(&rows, who, "daemon restarted mid-delivery");
        let cleared = row_index(&rows, who, "✔ cleared");
        assert!(alarm < cleared, "{who}'s clearance came first: {rows:#?}");
    }

    // 5. Nothing is dropped from the record or from the firehose, in
    //    either surface: the ping is history, and history is complete.
    app.view = View::Firehose;
    assert!(app.admits_in_view(&ping), "the firehose lost a daemon ping");
    let rows = build(&mut app, 20);
    assert!(
        rows.iter().any(|r| r.contains("action required")),
        "{rows:#?}"
    );

    daemon.shutdown().await;
}
