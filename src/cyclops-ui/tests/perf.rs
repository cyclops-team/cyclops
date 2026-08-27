//! Fluidity past 10k entries (GOALS smoothness bar): fill the ring to its
//! 10,000-entry cap with synthetic traffic, carry a REAL attention backlog
//! alongside it, then measure one full frame build at a realistic terminal
//! size. The 16ms budget is one 60Hz frame. The measured numbers print for
//! the milestone report.
//!
//! The backlog is the point. The frame's newest code path is the attention
//! band, which asks `App::unreachable` on every frame, and the previous
//! fixture's attention count was exactly zero: `i % 4 == 1` and
//! `i % 50 == 0` have no common solution, and neither do `i % 4 == 2` and
//! `i % 40 == 0`, so no synthetic entry ever raised an item and the band
//! never ran. A budget certified by a fixture that avoids the code path is
//! not a budget, so `backlog_is_real` fails outright if that happens again.

use std::time::Instant;

use cyclops_proto::{AgentState, DeliveryState, OpenDelivery};
use cyclops_ui::{build, App, Entry, EntryKind, Filter, PaneSnapshot, StatusSeed, Theme, View};

/// Ordinary stream traffic: messages, delivery transitions, state changes
/// and gate lines across seven agents. None of it needs a human; the
/// backlog is seeded separately so its size is a parameter and not an
/// accident of arithmetic.
fn synthetic(i: u64) -> Entry {
    let kind = match i % 5 {
        // Admin pings. The calm view asks the register whether each one
        // still stands (`App::admits`), so they are the one entry kind
        // whose admission costs a register lookup per frame, and a stream
        // with none of them measures a frame the check never touches.
        4 => EntryKind::Notify {
            level: cyclops_proto::NotifyLevel::ActionRequired,
            subject: format!("delivery to agent{} needs attention", i % 7),
            pane_id: None,
            to: Some(format!("agent{}", i % 7)),
            recipient: None,
            deliveries: Vec::new(),
        },
        0 => EntryKind::Msg {
            from: format!("agent{}", i % 7),
            to: vec![format!("agent{}", (i + 1) % 7)],
            endpoints: None,
            subject: format!("message number {i} with a realistic subject line"),
            fyi: i.is_multiple_of(8),
        },
        1 => EntryKind::Delivery {
            to: format!("agent{}", i % 7),
            recipient: None,
            state: DeliveryState::DeliveredVerified,
            cause: None,
        },
        // Alternated rather than constant: consecutive entries for the same
        // pane land 35 apart (lcm(5, 7)), an odd step, so `i % 2` flips
        // between them every time and none of the 10,000 is a same-state
        // repeat `Record::ingest` would now dedupe out of the ring.
        2 => EntryKind::State {
            target: format!("agent{}", i % 7),
            recipient: None,
            session_idx: 0,
            pane_id: Some(format!("%{}", i % 7)),
            state: if i.is_multiple_of(2) {
                AgentState::Working
            } else {
                AgentState::IdleWithInput
            },
        },
        _ => EntryKind::Gate {
            to: format!("agent{}", i % 7),
            recipient: None,
            action: "proceed".into(),
            detail: Some("prompt_visible".into()),
        },
    };
    Entry {
        uid: 0,
        ts: 1_754_000_000_000 + i * 250,
        seq: Some(i + 1),
        id: Some(format!("m-{i}")),
        kind,
    }
}

/// The daemon's answer for a rig with `open` deliveries parked on a quota
/// and one blocked pane once there is any backlog at all. A quota weekend
/// leaves hundreds of these; every one of them is an item the band has to
/// answer for on every frame.
fn backlog(open: usize) -> StatusSeed {
    StatusSeed {
        watched: vec!["main".into()],
        admin_unread: 0,
        mailbox_routes: Vec::new(),
        roster: Vec::new(),
        panes: if open == 0 {
            Vec::new()
        } else {
            vec![PaneSnapshot {
                name: "agent0".into(),
                pane_id: "%0".into(),
                state: AgentState::BlockedPermission,
            }]
        },
        open: (0..open.saturating_sub(usize::from(open > 0)))
            .map(|n| OpenDelivery {
                id: format!("m-park-{n:04}"),
                to: format!("agent{}", n % 7),
                recipient: None,
                state: DeliveryState::ParkedBlockedQuota,
                ts: 1_754_000_000_000,
                cause: Some("blocked_quota".into()),
                attempt_id: None,
            })
            .collect(),
        mailbox: Vec::new(),
    }
}

/// A full ring plus a backlog of exactly `open` items, reconciled the way
/// a real startup reconciles it (the seed writes a line for every item the
/// replayed stream does not already carry, and those lines go in the ring).
fn rig(view: View, open: usize) -> App {
    let mut app = App::new(Theme::none(), view, Filter::default());
    for i in 0..10_000u64 {
        app.live(synthetic(i));
    }
    for e in app.seed_status(backlog(open)) {
        app.replay(e);
    }
    assert_eq!(app.len(), 10_000);
    assert_eq!(app.attention_count(), open, "the fixture lost its backlog");
    app
}

/// Median of five builds at a 220x60 terminal, pinned to the tail like a
/// live stream. One warm build first.
fn median_us(app: &mut App) -> u128 {
    let _ = build(app, 80, 60);
    let mut samples: Vec<u128> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let rows = build(app, 80, 60);
            let d = t.elapsed().as_micros();
            assert_eq!(rows.len(), 60);
            d
        })
        .collect();
    samples.sort_unstable();
    samples[2]
}

/// The fixture carries what it claims to carry. Without this the whole
/// budget below can go green while measuring a frame the eye never
/// touches, which is how the band's cost went unmeasured for a round.
#[test]
fn backlog_is_real() {
    let app = rig(View::Firehose, 400);
    assert_eq!(app.attention_count(), 400);
    // And the band's own input is non-empty work: every item is checked
    // against the window, not skipped by an early return.
    let visible = app.visible();
    assert_eq!(visible.len(), 10_000);
    assert!(
        app.unreachable(&visible).len() < 400,
        "the seeded lines never made it into the stream"
    );
}

#[test]
fn frame_build_stays_under_16ms_at_10k_entries() {
    for open in [0, 100, 400, 1000] {
        let mut app = rig(View::Firehose, open);
        let us = median_us(&mut app);
        println!(
            "10k-entry frame build, {open} open items: {:.3}ms",
            us as f64 / 1000.0
        );
        assert!(
            us < 16_000,
            "frame build with {open} open items took {us}us, budget is 16ms"
        );

        // The admin view filters the whole ring per frame; it must hold
        // the same budget with the same backlog.
        let mut admin = rig(View::Admin, open);
        let us = median_us(&mut admin);
        println!(
            "10k-entry admin-view frame build, {open} open items: {:.3}ms",
            us as f64 / 1000.0
        );
        assert!(
            us < 16_000,
            "admin frame build with {open} open items took {us}us, budget is 16ms"
        );
    }
}
