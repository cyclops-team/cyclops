//! The eye against a real ledger: `--backfill` decides what is DISPLAYED
//! and never what is COUNTED, for both halves of the count, and every
//! number the header shows has a line the reader can reach.
//!
//! Attention was once derived from the windowed entry stream, so the same
//! ledger read with `--backfill 200` and `--backfill 400` gave a closed
//! eye and an open one. Round two moved the delivery half onto the
//! daemon's answer and left the agent half on the stream, which kept the
//! same bug alive on the other side and added a worse one: a blocked pane
//! that later disappeared stayed counted with nothing able to clear it.
//! The register now takes the daemon's snapshot and live events only
//! (`cyclops_proto::attention`), and these tests are what holds it there.

use cyclops_proto::{AgentState, Delivery, DeliveryState, Kind, LedgerLine, OpenDelivery};
use cyclops_ui::{
    build, App, Entry, EntryKind, Eye, Filter, Intake, PaneSnapshot, StatusSeed, Theme, View,
};

const BASE: u64 = 43_000_000;

/// A ledger whose oldest lines are the two shapes of attention, buried
/// under enough later traffic to fall out of a 200-line tail:
///
/// - a delivery parked on a quota, which GOALS makes the one state that
///   never auto-retries, so it is the item most likely to sit for hours;
/// - a pane blocked on a prompt, whose pane is gone by the time the UI
///   starts, so nothing will ever report it again.
fn ledger_with_a_buried_park(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("ledger dir");
    let w = cyclops_ledger::LedgerWriter::open(&dir.join("main.ndjson"), "b-test")
        .expect("ledger opens");
    let line = |id: &str, ts: u64, kind: Kind| LedgerLine {
        seq: 0,
        boot_id: String::new(),
        id: id.into(),
        ts,
        kind,
        from: "codex".into(),
        to: vec!["implementer".into()],
        subject: Some("Rerun the quota-heavy leg".into()),
        body: None,
        reply_to: None,
        deliveries: Vec::new(),
        data: None,
    };
    w.append(line("m-park", BASE, Kind::Msg)).expect("append");
    let mut parked = line("m-park", BASE + 100, Kind::State);
    parked.from = "cyclopsd".into();
    parked.subject = None;
    parked.deliveries = vec![Delivery {
        to: "implementer".into(),
        state: DeliveryState::ParkedBlockedQuota,
        verified_by: None,
        attempts: 1,
        ts: BASE + 100,
        cause: Some("blocked_quota".into()),
    }];
    parked.data = Some(serde_json::json!({
        "to": "implementer", "from": "queued",
        "to_state": "parked_blocked_quota", "cause": "blocked_quota"
    }));
    w.append(parked).expect("append");
    // The pane half of the same problem: %9 sat on a permission prompt,
    // and its window was closed before this UI ever started.
    let mut blocked = line("e-ghost", BASE + 200, Kind::State);
    blocked.from = "cyclopsd".into();
    blocked.subject = None;
    blocked.data = Some(serde_json::json!({
        "target": "ghost", "pane_id": "%9", "state": "blocked_permission"
    }));
    w.append(blocked).expect("append");
    // Ordinary traffic on top, enough of it that both fall out of a
    // 200-line tail and stay inside a 400-line one.
    for i in 0..209u64 {
        w.append(line(&format!("m-{i:04}"), BASE + 1000 + i, Kind::Msg))
            .expect("append");
    }
}

/// Everything a UI run holds after startup, for one `--backfill` value:
/// the replayed tail, the daemon's answer reconciled over it, then any
/// live entries that queued behind the two.
fn started(dir: &std::path::Path, backfill: usize, seed: StatusSeed) -> App {
    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    let mut intake = Intake::new();
    // The seed arrives before the backfill; Intake orders it between the
    // replayed tail and the live backlog.
    let watched = seed.watched.clone();
    assert!(intake.status(Box::new(seed)).is_none());
    let (entries, max_seq) = cyclops_ui::read_backfill(dir, backfill, Some(&watched));
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
    app
}

/// The daemon's answer for this ledger: the panes it currently sees, and
/// the backlog its fold found, whatever its age. %9 is not among them,
/// because %9 is gone.
fn daemon_answer() -> StatusSeed {
    StatusSeed {
        watched: vec!["main".into()],
        panes: vec![
            PaneSnapshot {
                name: "reviewer".into(),
                pane_id: "%1".into(),
                state: AgentState::BlockedModal,
            },
            PaneSnapshot {
                name: "implementer".into(),
                pane_id: "%2".into(),
                state: AgentState::Idle,
            },
        ],
        open: vec![OpenDelivery {
            id: "m-park".into(),
            to: "implementer".into(),
            state: DeliveryState::ParkedBlockedQuota,
            ts: BASE + 100,
            cause: Some("blocked_quota".into()),
        }],
    }
}

#[test]
fn backfill_decides_what_is_displayed_never_what_is_counted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("ledger");
    ledger_with_a_buried_park(&dir);
    let watched = ["main".to_string()];

    // The tail genuinely misses both. This is the shape of the bug: the
    // stream alone can only answer for what it happens to be holding.
    let (tail, _) = cyclops_ui::read_backfill(&dir, 200, Some(&watched));
    assert_eq!(tail.len(), 200);
    assert!(!tail.iter().any(is_the_park), "the fixture buried nothing");
    assert!(!tail.iter().any(is_the_ghost), "the fixture buried nothing");
    let (wide, _) = cyclops_ui::read_backfill(&dir, 400, Some(&watched));
    assert!(wide.iter().any(is_the_park), "the wide tail lost the park");
    assert!(wide.iter().any(is_the_ghost), "the wide tail lost the pane");

    // Every backfill value agrees, because none of them is asked. Zero is
    // in the list on purpose: with no tail at all, the daemon's answer is
    // the entire count.
    for backfill in [0, 200, 400] {
        let app = started(&dir, backfill, daemon_answer());
        assert_eq!(
            app.attention_count(),
            2,
            "backfill {backfill} changed the count"
        );
        assert_eq!(app.eye_target(), Eye::Open, "backfill {backfill}");

        // Both halves are the daemon's: the park it folded from the whole
        // record, and the pane it currently sees blocked.
        let items = app.attention_items();
        assert_eq!(
            items,
            vec!["implementer  ⊘ parked · quota", "reviewer  ⚠ blocked_modal"],
            "backfill {backfill}"
        );

        // Exactly one line stands behind the park, whether it came from
        // the replayed tail or from the reconciliation.
        let parked: Vec<&Entry> = app.entries().filter(|e| is_the_park(e)).collect();
        assert_eq!(parked.len(), 1, "backfill {backfill} doubled the line");
        assert_eq!(
            parked[0].lines(&Theme::none(), false),
            vec!["11:56:40  implementer  ⊘ parked · quota"],
            "backfill {backfill}"
        );

        // The pane that disappeared is counted by nobody, at any backfill,
        // even where its blocked line is on screen.
        assert!(
            !app.attention_items().iter().any(|i| i.starts_with("ghost")),
            "backfill {backfill} counted a pane that no longer exists"
        );
    }

    // And the same run at 400 shows the ghost's line without counting it:
    // the record keeps what happened, the count says what is true now.
    let app = started(&dir, 400, daemon_answer());
    assert!(app.entries().any(is_the_ghost));
    assert_eq!(app.attention_count(), 2);
}

/// The snapshot half of rule 3 (`cyclops_proto::attention`): the tail can
/// put an alarm on screen whose item the answer does not count, and the
/// transition that ended it is older than anything replayed. The reader
/// gets the ending anyway, written where the answer said so.
///
/// %9's line says a human is needed and the daemon does not list %9 at
/// all, which is the shape that used to leave a closed eye directly over
/// "⚠ blocked_permission" on every startup with a wide enough tail.
#[test]
fn a_replayed_alarm_the_answer_no_longer_counts_gets_its_clearance() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("ledger");
    ledger_with_a_buried_park(&dir);

    let mut app = started(&dir, 400, daemon_answer());
    let rows: Vec<String> = build(&mut app, 40);
    let ghost = |what: &str| {
        rows.iter()
            .position(|r| r.contains("ghost") && r.contains(what))
            .unwrap_or_else(|| panic!("no ghost row saying {what}: {rows:#?}"))
    };
    // The pane is gone, so nobody answered the prompt and the line says
    // that rather than claiming somebody did.
    assert!(
        ghost("⚠ blocked_permission") < ghost("✔ pane closed · was ⚠ blocked_permission"),
        "the clearance came before the alarm: {rows:#?}"
    );
    // The count is untouched: a clearance is a line, not an item.
    assert_eq!(app.attention_count(), 2);

    // With no tail there is no alarm on screen, so there is nothing to
    // answer and nothing is written.
    let mut app = started(&dir, 0, daemon_answer());
    let rows = build(&mut app, 40);
    assert!(
        !rows.iter().any(|r| r.contains("✔ pane closed")),
        "a clearance with no alarm above it: {rows:#?}"
    );
}

/// A pane the daemon stops reporting stops counting. Nothing else can
/// clear it: no event arrives for a pane that is gone, so if the answer
/// does not, the item outlives the process.
#[test]
fn a_blocked_pane_that_disappears_stops_being_counted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("ledger");
    ledger_with_a_buried_park(&dir);
    let mut app = started(&dir, 400, daemon_answer());
    assert_eq!(app.attention_count(), 2);

    // The next answer no longer lists %1: the pane went away while the
    // stream was running, and it was blocked when it did.
    let mut later = daemon_answer();
    later.panes.retain(|p| p.pane_id != "%1");
    app.seed_status(later);
    assert_eq!(app.attention_count(), 1, "a vanished pane still counted");
    assert_eq!(app.attention_items(), vec!["implementer  ⊘ parked · quota"]);
}

/// The snapshot above is taken ONCE, at startup. Nothing re-takes it: a
/// repeating status request would be polling, which this UI does not do.
/// So while the process runs, the pane's own removal edge is the only
/// thing that can drop its item, and the daemon emits one.
#[test]
fn a_pane_removed_mid_stream_drops_its_item_without_a_second_snapshot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("ledger");
    ledger_with_a_buried_park(&dir);
    let mut app = started(&dir, 400, daemon_answer());
    assert_eq!(app.attention_count(), 2);
    assert_eq!(app.eye_target(), Eye::Open);

    // %1 leaves the tmux table. One live event, no second status.
    app.live(pane_removed("%1"));
    assert_eq!(
        app.attention_count(),
        1,
        "the item outlived the pane it was about"
    );
    assert_eq!(app.attention_items(), vec!["implementer  ⊘ parked · quota"]);
    assert_eq!(app.eye_target(), Eye::Opening);

    // The header may not point at a line for a pane that is gone either:
    // the delivery is the only thing left to name.
    let rows = build(&mut app, 12);
    assert!(rows[0].contains("1 needs attention"), "{:?}", rows[0]);
    assert!(!rows[0].contains("2 need"), "{:?}", rows[0]);

    // The delivery half is untouched by a pane leaving, and a pane nobody
    // counted removes cleanly.
    app.live(pane_removed("%404"));
    assert_eq!(app.attention_count(), 1);
}

/// One `pane-removed` event as the daemon emits it (cyclopsd/src/lib.rs).
fn pane_removed(pane_id: &str) -> Entry {
    Entry::from_event(
        &cyclops_proto::Event {
            event: "pane-removed".into(),
            data: serde_json::json!({
                "ts": BASE + 5000, "session": "main", "pane_id": pane_id
            }),
            seq: None,
        },
        BASE + 5000,
    )
}

/// The header and the body answer the same question, in every view, under
/// every filter, whether or not the window has anything in it. The band
/// used to be reachable only from the empty window, so a count under a
/// filter over a busy stream had nothing behind it at all.
#[test]
fn every_count_has_a_line_the_reader_can_reach() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("ledger");
    ledger_with_a_buried_park(&dir);
    let mut app = started(&dir, 200, daemon_answer());

    // Nothing hidden: both lines are in the view, so the frame says
    // nothing extra and the stream carries them.
    let rows = build(&mut app, 12);
    assert!(rows[0].contains("2 need attention"), "{:?}", rows[0]);
    assert!(!rows[2].contains("Hidden by the"), "{rows:?}");
    assert!(
        rows.iter().any(|r| r.contains("⊘ parked · quota")),
        "{rows:?}"
    );

    // A filter that hides one of the two, over a window that still has
    // the other. The header keeps both, so the frame has to name the one
    // it cannot show.
    app.filter.with = Some("reviewer".into());
    let rows = build(&mut app, 12);
    assert!(rows[0].contains("2 need attention"), "{:?}", rows[0]);
    assert!(!app.visible().is_empty(), "the window is not empty here");
    assert_eq!(
        rows[2],
        "  implementer  ⊘ parked · quota. Hidden by the with filter · \
         press w, empty the line, then enter."
    );
    assert!(rows[3].contains("⚠ blocked_modal"), "{rows:?}");

    // A filter that hides both names them both, one line, plural counted.
    app.filter.with = Some("nobody".into());
    let rows = build(&mut app, 12);
    assert!(app.visible().is_empty());
    assert_eq!(
        rows[2],
        "  implementer  ⊘ parked · quota and 1 more. Hidden by the with filter · \
         press w, empty the line, then enter."
    );
    assert!(!rows.iter().any(|r| r.contains("All calm")), "{rows:?}");

    // The firehose is a different view and answers the same way.
    app.view = View::Firehose;
    let rows = build(&mut app, 12);
    assert!(rows[2].contains("Hidden by the with filter"), "{rows:?}");

    // Filter cleared: the lines are reachable and the band goes away.
    app.filter.with = None;
    let rows = build(&mut app, 12);
    assert!(!rows.iter().any(|r| r.contains("Hidden by the")));

    // Nothing outstanding: calm is calm again.
    let mut calm = App::new(Theme::none(), View::Admin, Filter::default());
    let rows = build(&mut calm, 10);
    assert_eq!(rows[2], "  All calm. tab shows the firehose.");
}

/// One `admin-notify` event as the daemon emits it (cyclopsd
/// delivery.rs): the level, what it says, and the register item it is
/// about when it has one.
fn ping(level: &str, subject: &str, about: serde_json::Value) -> Entry {
    let mut data = serde_json::json!({
        "ts": BASE + 6000, "level": level, "subject": subject, "body": ""
    });
    let (Some(map), serde_json::Value::Object(about)) = (data.as_object_mut(), about) else {
        panic!("both are objects");
    };
    map.extend(about);
    Entry::from_event(
        &cyclops_proto::Event {
            event: "admin-notify".into(),
            data,
            seq: None,
        },
        BASE + 6000,
    )
}

/// A ping POINTS AT something that needs a human; it is not itself a
/// state, so nothing ever transitions it and the register cannot hold it.
/// Admitting every ping to the calm view put "⚠ action required" directly
/// under a closed eye in steady state, because the daemon pings about
/// conditions the rule says nobody must clear: a delivery that downgraded
/// to screen evidence, a gate hold that has waited too long.
#[test]
fn a_ping_the_register_cannot_back_stays_out_of_the_calm_view() {
    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    app.live(ping(
        "action_required",
        "reviewer: hooks configured but never seen",
        serde_json::json!({"id": "m-1", "to": "reviewer"}),
    ));
    while app.tick_eye() {}
    assert_eq!(app.attention_count(), 0, "a ping moved the count");

    let rows = build(&mut app, 12);
    assert!(rows[0].starts_with("‿ cyclops"), "{:?}", rows[0]);
    assert!(
        !rows.iter().any(|r| r.contains("action required")),
        "a calm eye over a line saying a human is needed: {rows:#?}"
    );
    // Nothing is dropped: the firehose keeps every ping the daemon sent.
    app.view = View::Firehose;
    let rows = build(&mut app, 12);
    assert!(
        rows.iter().any(|r| r.contains("action required")),
        "{rows:#?}"
    );
}

/// The other side of the same rule: a ping whose item the register does
/// hold stands, on both halves. The eye is open, the line says why, and
/// the two agree.
#[test]
fn a_ping_about_a_live_item_reaches_the_calm_view() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("ledger");
    ledger_with_a_buried_park(&dir);
    let mut app = started(&dir, 0, daemon_answer());
    assert_eq!(app.attention_count(), 2);

    // The delivery half: m-park is in the register under implementer.
    app.live(ping(
        "urgent",
        "implementer parked: quota exhausted",
        serde_json::json!({"id": "m-park", "to": "implementer"}),
    ));
    // The agent half: %1 is blocked on a modal, and the gate names the
    // pane rather than the delivery it is holding.
    app.live(ping(
        "action_required",
        "reviewer blocked: trust_dialog",
        serde_json::json!({"id": "m-9", "pane_id": "%1"}),
    ));
    // An operator's own ping names no item at all; nothing may drop it.
    app.live(ping(
        "action_required",
        "look at the gateway rollout",
        serde_json::json!({"id": "e-op"}),
    ));
    // An fyi claims nothing, so nothing about it can contradict the eye.
    app.live(ping(
        "fyi",
        "nightly finished",
        serde_json::json!({"id": "m-old", "to": "nobody"}),
    ));

    let rows = build(&mut app, 20);
    assert!(rows[0].contains("2 need attention"), "{:?}", rows[0]);
    for said in [
        "implementer parked: quota exhausted",
        "reviewer blocked: trust_dialog",
        "look at the gateway rollout",
        "nightly finished",
    ] {
        assert!(
            rows.iter().any(|r| r.contains(said)),
            "the calm view dropped {said:?}: {rows:#?}"
        );
    }

    // The delivery moves on. Its ping goes with it: the item is gone, so
    // the line claiming a human is needed for it may not stand alone.
    app.live(Entry::from_event(
        &cyclops_proto::Event {
            event: "delivery-state".into(),
            data: serde_json::json!({
                "ts": BASE + 7000, "id": "m-park",
                "to": "implementer", "to_state": "queued"
            }),
            seq: None,
        },
        BASE + 7000,
    ));
    let rows = build(&mut app, 20);
    assert!(rows[0].contains("1 needs attention"), "{:?}", rows[0]);
    assert!(
        !rows.iter().any(|r| r.contains("quota exhausted")),
        "the ping outlived the item it was about: {rows:#?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("trust_dialog")),
        "the pane is still blocked: {rows:#?}"
    );
}

/// One ping over a BATCH: the daemon's restart closure ends every
/// delivery a dead run left in flight and pings once about all of them
/// (cyclopsd delivery.rs `close_limbo`). It claims a human is needed, so
/// it is held to the register like any other ping, and it stands while ANY
/// item it names does. Naming nothing put "⚠ action required" under a
/// closed eye on every restart once the batch had been dealt with.
///
/// The daemon side is driven end to end in cyclopsd's `restart_eye`; this
/// pins the reader's half of the rule on its own.
#[test]
fn a_batch_ping_stands_while_any_item_it_names_does() {
    let batch = |ids: serde_json::Value| {
        ping(
            "action_required",
            "deliveries interrupted by daemon restart",
            serde_json::json!({"id": "e-restart", "deliveries": ids}),
        )
    };
    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    app.live(batch(serde_json::json!([
        {"to": "reviewer", "id": "m-1"},
        {"to": "implementer", "id": "m-2"},
    ])));
    // Nothing in the register yet: the ping names two items and holds
    // neither, so the calm view may not show it.
    let rows = build(&mut app, 12);
    assert!(rows[0].starts_with("‿ cyclops"), "{:?}", rows[0]);
    assert!(
        !rows.iter().any(|r| r.contains("action required")),
        "{rows:#?}"
    );

    // One of the two lands. The ping stands on it alone.
    app.live(Entry::from_event(
        &cyclops_proto::Event {
            event: "delivery-state".into(),
            data: serde_json::json!({
                "ts": BASE + 8000, "id": "m-2", "to": "implementer",
                "to_state": "attention_required"
            }),
            seq: None,
        },
        BASE + 8000,
    ));
    let rows = build(&mut app, 12);
    assert!(rows[0].contains("1 needs attention"), "{:?}", rows[0]);
    assert!(
        rows.iter().any(|r| r.contains("action required")),
        "{rows:#?}"
    );

    // That one moves on: nothing it names stands, and neither does it.
    app.live(Entry::from_event(
        &cyclops_proto::Event {
            event: "delivery-state".into(),
            data: serde_json::json!({
                "ts": BASE + 9000, "id": "m-2", "to": "implementer", "to_state": "queued"
            }),
            seq: None,
        },
        BASE + 9000,
    ));
    while app.tick_eye() {}
    let rows = build(&mut app, 12);
    assert!(rows[0].starts_with("‿ cyclops"), "{:?}", rows[0]);
    assert!(
        !rows.iter().any(|r| r.contains("action required")),
        "{rows:#?}"
    );

    // A daemon that predates the list sends the ping without it. Nothing
    // here can prove that one stale, so it is shown: dropping a ping the
    // reader cannot judge is the worse failure (tolerant protocol).
    let mut old = App::new(Theme::none(), View::Admin, Filter::default());
    old.live(ping(
        "action_required",
        "deliveries interrupted by daemon restart",
        serde_json::json!({"id": "e-restart"}),
    ));
    let rows = build(&mut old, 12);
    assert!(
        rows.iter().any(|r| r.contains("action required")),
        "{rows:#?}"
    );
}

fn is_the_park(e: &Entry) -> bool {
    matches!(
        &e.kind,
        EntryKind::Delivery {
            state: DeliveryState::ParkedBlockedQuota,
            ..
        }
    )
}

fn is_the_ghost(e: &Entry) -> bool {
    matches!(
        &e.kind,
        EntryKind::State {
            target,
            state: AgentState::BlockedPermission,
            ..
        } if target == "ghost"
    )
}
