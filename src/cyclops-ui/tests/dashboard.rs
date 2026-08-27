//! The dashboard half of the stream: the agents panel beside the window,
//! and the mouse. Everything here goes through the public surface the
//! runtime uses: seed, live, build, handle_key.

use cyclops_ui::{build, App, Command, Entry, EntryKind, Filter, Key, RowTarget, View};

use cyclops_proto::{AgentState, Event, PaneStatus, SessionStatus, StatusResult};
use cyclops_ui::{RosterSeed, StatusSeed, Theme};

const WIDE: usize = 120;
const NARROW: usize = 80;
const H: usize = 24;

fn seeded_app() -> App {
    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    app.seed_status(StatusSeed {
        watched: vec!["main".into()],
        panes: Vec::new(),
        open: Vec::new(),
        admin_unread: 0,
        mailbox_routes: Vec::new(),
        roster: vec![
            RosterSeed {
                session_idx: 0,
                pane_id: "%0".into(),
                name: "implementer".into(),
                state: AgentState::Working,
                manifest: Some("claude".into()),
                state_ms: Some(5_000),
            },
            RosterSeed {
                session_idx: 0,
                pane_id: "%1".into(),
                name: "reviewer".into(),
                state: AgentState::Idle,
                manifest: None,
                state_ms: None,
            },
        ],
        mailbox: Vec::new(),
    });
    app
}

fn state_event(uid_ts: u64, name: &str, pane: &str, state: AgentState) -> Entry {
    Entry {
        uid: 0,
        ts: uid_ts,
        seq: None,
        id: Some("e-t".into()),
        kind: EntryKind::State {
            target: name.into(),
            recipient: None,
            session_idx: 0,
            pane_id: Some(pane.into()),
            state,
        },
    }
}

/// The panel: who, which CLI, where they stand, for how long. Glyph plus
/// word for every state, elapsed only where somebody has said, and the
/// whole thing absent below the width that can afford it.
#[test]
fn the_panel_shows_each_agent_and_only_on_a_wide_terminal() {
    let mut app = seeded_app();
    let wide = build(&mut app, WIDE, H);
    let joined = wide.join("\n");
    assert!(joined.contains("agents"), "{joined}");
    assert!(joined.contains("implementer · claude"), "{joined}");
    assert!(joined.contains("● working · 5s"), "{joined}");
    assert!(joined.contains("reviewer"), "{joined}");
    // Nobody said how long reviewer has been idle, so nothing counts.
    assert!(joined.contains("○ idle"), "{joined}");
    assert!(!joined.contains("○ idle ·"), "{joined}");
    assert!(joined.contains("│"), "no separator: {joined}");
    assert!(app.sidebar_w > 0);

    let narrow = build(&mut app, NARROW, H);
    let joined = narrow.join("\n");
    assert!(!joined.contains("agents\n"), "{joined}");
    assert!(!joined.contains("│"), "{joined}");
    assert_eq!(app.sidebar_w, 0);

    // `a` gives the width back on demand.
    let mut app = seeded_app();
    app.handle_key(Key::Char('a'));
    let rows = build(&mut app, WIDE, H);
    assert!(!rows.join("\n").contains("│"));
}

/// A click is the panel's whole point: the fastest route to the pane that
/// needs you. The frame wrote what each cell means; x picks the half.
#[test]
fn clicks_land_on_what_the_frame_drew() {
    let mut app = seeded_app();
    // One entry the ADMIN view can reach: routine working/idle traffic is
    // firehose-only, and a click needs a row to land on.
    app.live(state_event(
        43_480_000,
        "implementer",
        "%0",
        AgentState::BlockedPermission,
    ));
    let _ = build(&mut app, WIDE, H);

    // Find the implementer row in the panel and click its left half.
    let agent_row = app
        .row_targets
        .iter()
        .position(|(side, _)| *side == RowTarget::Agent("%0".into()))
        .expect("an agent row in the panel");
    let cmd = app.handle_key(Key::Click {
        x: 2,
        y: agent_row as u16,
    });
    assert_eq!(cmd, Some(Command::Focus("%0".into())));

    // A stream entry row, clicked right of the divider: selection, no jump.
    let entry_row = app
        .row_targets
        .iter()
        .find_map(|(_, stream)| match stream {
            RowTarget::Entry(uid) => Some(*uid),
            RowTarget::Nothing => None,
            RowTarget::Agent(_) => None,
        })
        .expect("a stream entry row");
    let y = app
        .row_targets
        .iter()
        .position(|(_, s)| *s == RowTarget::Entry(entry_row))
        .unwrap();
    let cmd = app.handle_key(Key::Click {
        x: (app.sidebar_w + 5) as u16,
        y: y as u16,
    });
    assert_eq!(cmd, None);
    assert_eq!(app.selected, Some(entry_row));
    assert!(!app.pinned);

    // Dead space does nothing at all.
    let cmd = app.handle_key(Key::Click { x: 0, y: 1 });
    assert_eq!(cmd, None);
}

/// The elapsed clock restarts on a TRANSITION and survives confirmation.
/// The daemon re-emits state on unrelated recomputes; a clock that reset
/// on those would show every long-running agent as seconds old.
#[test]
fn elapsed_survives_confirmation_and_restarts_on_transition() {
    let mut app = seeded_app();
    // Confirmed: same state again. The 5s the daemon reported stands.
    app.live(state_event(1, "implementer", "%0", AgentState::Working));
    let row = app.roster().find(|r| r.pane_id == "%0").unwrap();
    assert!(row.elapsed_ms().unwrap() >= 5_000, "the clock reset");

    // A real transition: the clock starts over from what we saw.
    app.live(state_event(2, "implementer", "%0", AgentState::Idle));
    let row = app.roster().find(|r| r.pane_id == "%0").unwrap();
    assert!(row.elapsed_ms().unwrap() < 5_000, "the clock carried over");
}

/// A pane leaving the table leaves the panel: tmux retired the id, and a
/// row nothing can click is a row that lies.
#[test]
fn a_gone_pane_leaves_the_panel() {
    let mut app = seeded_app();
    assert_eq!(app.roster_len(), 2);
    app.live(Entry {
        uid: 0,
        ts: 3,
        seq: None,
        id: None,
        kind: EntryKind::PaneGone {
            session_idx: 0,
            pane_id: "%0".into(),
            physical_gone: true,
        },
    });
    assert_eq!(app.roster_len(), 1);
    let rows = build(&mut app, WIDE, H);
    assert!(!rows.join("\n").contains("implementer"));
}

/// A transfer is reported by two independently scheduled session watchers.
/// The destination state can arrive before the source removal, and that late
/// source edge must not erase the pane at its new route.
#[test]
fn a_late_source_removal_keeps_the_transferred_pane_focusable() {
    let pane = PaneStatus {
        pane_id: "%1".into(),
        window_id: "@1".into(),
        window_name: "source-window".into(),
        agent: Some("reviewer".into()),
        manifest: Some("claude".into()),
        title: String::new(),
        current_command: "claude".into(),
        dead: false,
        in_mode: false,
        write_ready: false,
        write_block: None,
        composer: cyclops_proto::ComposerState::ComposerAmbiguous,
        composer_proof: cyclops_proto::ComposerProof::Unprovable,
        notification_attempt: None,
        composer_reason: None,
        composer_candidates: 0,
        notification_state: None,
        message_state: None,
        next_action: None,
        width: 120,
        height: 40,
        state: AgentState::Idle,
        state_ms: Some(5_000),
        working_confirmed: None,
        hooks_verified: None,
        manifest_display_name: None,
        unread: None,
        unknown_reason: None,
    };
    let status = StatusResult {
        daemon_version: "0.1.0".into(),
        daemon_build: None,
        daemon_process: None,
        daemon_executable: None,
        proto: 1,
        boot_id: "boot".into(),
        uptime_ms: 1_000,
        tmux_version: "3.6a".into(),
        sessions: vec![
            SessionStatus {
                name: "source".into(),
                attached: true,
                panes: vec![pane],
            },
            SessionStatus {
                name: "destination".into(),
                attached: true,
                panes: Vec::new(),
            },
        ],
        mailbox_routes: Vec::new(),
        admin_unread: 0,
        open_deliveries: Vec::new(),
        diagnostics: Vec::new(),
        blocked_notifications: Vec::new(),
        blocked_notifications_total: 0,
        manifests: None,
        pid: None,
        mailbox_attention: Vec::new(),
    };
    let mut app = App::new(Theme::none(), View::Firehose, Filter::default());
    app.seed_status(StatusSeed::from_status(&status));

    app.live(Entry::from_event(
        &Event {
            event: "state".into(),
            data: serde_json::json!({
                "target": "reviewer",
                "pane_id": "%1",
                "session_idx": 1,
                "state": "blocked_permission",
            }),
            seq: None,
        },
        2,
    ));
    app.live(Entry::from_event(
        &Event {
            event: "pane-removed".into(),
            data: serde_json::json!({
                "session": "source",
                "session_idx": 0,
                "pane_id": "%1",
                "physical_gone": false,
            }),
            seq: None,
        },
        3,
    ));

    assert_eq!(app.roster_len(), 1);
    let row = app
        .roster()
        .find(|row| row.pane_id == "%1")
        .expect("the destination route remains in the roster");
    assert_eq!(row.state, AgentState::BlockedPermission);
    assert_eq!(row.session_idx, 1);
    assert_eq!(app.attention_count(), 1);

    let state_uid = app
        .entries()
        .find(|entry| {
            matches!(
                &entry.kind,
                EntryKind::State {
                    target,
                    session_idx: 1,
                    pane_id: Some(pane_id),
                    ..
                } if target == "reviewer" && pane_id == "%1"
            )
        })
        .expect("the destination state remains in the stream")
        .uid;
    app.selected = Some(state_uid);
    app.pinned = false;
    assert_eq!(
        app.handle_key(Key::Enter),
        Some(Command::Focus("%1".into()))
    );

    app.live(Entry {
        uid: 0,
        ts: 4,
        seq: None,
        id: Some("m-route".into()),
        kind: EntryKind::Msg {
            from: "reviewer".into(),
            to: vec!["admin".into()],
            endpoints: None,
            subject: "route check".into(),
            body: None,
            fyi: false,
        },
    });
    let message_uid = app
        .entries()
        .find(|entry| entry.id.as_deref() == Some("m-route"))
        .expect("the label-targeted entry is in the stream")
        .uid;
    app.selected = Some(message_uid);
    assert_eq!(
        app.handle_key(Key::Enter),
        Some(Command::Focus("%1".into()))
    );
}

/// The uncolored dashboard carries everything the colored one does: the
/// panel is glyph-plus-word like every other surface, so NO_COLOR costs
/// paint and nothing else.
#[test]
fn an_uncolored_dashboard_emits_no_escapes() {
    let mut app = seeded_app();
    let rows = build(&mut app, WIDE, H);
    assert!(
        rows.iter().all(|r| !r.contains('\x1b')),
        "escape bytes in an uncolored frame"
    );
}

/// The wheel scrolls like the arrows, three rows a notch.
#[test]
fn the_wheel_scrolls_and_unpins() {
    let mut app = seeded_app();
    // The firehose: routine state traffic is visible there, and the wheel
    // needs a list longer than the window to scroll in.
    app.handle_key(Key::Tab);
    for n in 0..12 {
        app.live(state_event(
            43_480_000 + n,
            "implementer",
            "%0",
            if n % 2 == 0 {
                AgentState::Working
            } else {
                AgentState::Idle
            },
        ));
    }
    let _ = build(&mut app, WIDE, 10);
    assert!(app.pinned);
    app.handle_key(Key::WheelUp);
    assert!(!app.pinned, "a wheel notch should unpin like ↑ does");
    app.handle_key(Key::End);
    assert!(app.pinned);
}
