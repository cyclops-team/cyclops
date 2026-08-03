//! The calm view never shows an alarm with nothing under it that answers.
//!
//! A line reaches the admin stream because it says a human is needed. The
//! transition that ENDS that condition is ordinary traffic, so it stayed
//! in the firehose and the calm view kept the alarm alone: a closed eye
//! directly above "⚠ blocked_permission", with the line that resolved it
//! one keypress away in a view the reader is not looking at.
//!
//! The record does not retract, so the alarm line stays. What was missing
//! is the second line. The register emits a clearance for the transition
//! that resolved an item (`cyclops_proto::attention`), and both lines ride
//! the calm view, so the reader sees the alarm, sees it end, and the
//! closed eye is self-evidently right.
//!
//! ## The sweep
//!
//! [`no_calm_frame_leaves_an_alarm_unanswered`] is the measurement that
//! found the defect, checked in: every sequence of up to four events over
//! the alphabet in [`Step`], each rendered in both views, at both
//! densities, under four filters and at four heights. One assertion per
//! frame, and it is the whole rule: under a calm eye, every row that says
//! a human is needed has a row below it that answers it.
//!
//! Live events only. The register takes the daemon's snapshot and the
//! live push and nothing else, and a replayed tail feeds it nothing by
//! design (`cyclops_proto::attention`, "what may feed the register"), so
//! the live path is the one where a transition can resolve anything. The
//! snapshot half of the same rule is driven end to end against a real
//! daemon in cyclopsd's `restart_eye`.

use std::collections::{HashMap, HashSet};

use cyclops_proto::{AgentState, DeliveryState, Event};
use cyclops_ui::{build, grid, App, Density, Entry, Filter, Theme, View};
use serde_json::{json, Value};

const BASE: u64 = 43_471_000;

// ---------------------------------------------------------------------------
// The alphabet
// ---------------------------------------------------------------------------

/// One daemon event the sweep can put in a sequence.
///
/// Nine, and every one of them earns its place: two shapes of agent alarm
/// on one pane, the two ways an agent alarm can end, both shapes of
/// delivery alarm on one message, the way a delivery alarm ends, the gate
/// hold that says a human is needed without being a register item of its
/// own, and the ping that points at one. Ordinary chatter is not here: a
/// message says nothing about a human being needed, so it can only pad the
/// window, and the four heights already pad it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// %1 stops on a permission prompt.
    Prompt,
    /// %1 stops on an exhausted quota, a second alarm on the same pane.
    Quota,
    /// %1 takes a turn again: the transition that answers either alarm.
    Working,
    /// %1 leaves the tmux table. The pane's last transition, and the other
    /// way its alarm can end.
    PaneGone,
    /// m-1 parks on a quota, which never auto-retries (GOALS, F11).
    Parked,
    /// m-1 runs out of redelivery, the other terminal-in-the-record state.
    NeedsHuman,
    /// m-1 is requeued: the transition that answers either delivery alarm.
    Requeued,
    /// The gate holds m-1 because %1 is blocked. Emitted only while %1
    /// actually is, which is the only way the daemon writes it: the cause
    /// IS the pane's fused state (cyclopsd/src/delivery.rs).
    Held,
    /// The daemon pings about %1's prompt.
    Ping,
}

const ALPHABET: [Step; 9] = [
    Step::Prompt,
    Step::Quota,
    Step::Working,
    Step::PaneGone,
    Step::Parked,
    Step::NeedsHuman,
    Step::Requeued,
    Step::Held,
    Step::Ping,
];

/// Longest sequence swept. Four is the shortest length that can raise two
/// alarms and answer both, which is the state the eye closes in.
const MAX_LEN: usize = 4;

/// The pane, the agent that wears it, and the one message in flight. Two
/// names so the filters below have something to separate.
const PANE: &str = "%1";
const AGENT: &str = "reviewer";
const RECIPIENT: &str = "implementer";
const MSG: &str = "m-1";

/// The event one step feeds the app, or None when the daemon would not
/// have written it here.
fn event_for(step: Step, ts: u64, blocked: bool) -> Option<Entry> {
    let (name, data) = match step {
        Step::Prompt => (
            "state",
            json!({"target": AGENT, "pane_id": PANE, "state": "blocked_permission"}),
        ),
        Step::Quota => (
            "state",
            json!({"target": AGENT, "pane_id": PANE, "state": "blocked_quota"}),
        ),
        Step::Working => (
            "state",
            json!({"target": AGENT, "pane_id": PANE, "state": "working"}),
        ),
        Step::PaneGone => ("pane-removed", json!({"session": "main", "pane_id": PANE})),
        Step::Parked => (
            "delivery-state",
            json!({"id": MSG, "to": RECIPIENT, "to_state": "parked_blocked_quota"}),
        ),
        Step::NeedsHuman => (
            "delivery-state",
            json!({"id": MSG, "to": RECIPIENT, "to_state": "attention_required"}),
        ),
        Step::Requeued => (
            "delivery-state",
            json!({"id": MSG, "to": RECIPIENT, "to_state": "queued"}),
        ),
        Step::Held if blocked => (
            "gate",
            json!({"id": MSG, "to": AGENT, "action": "hold", "cause": "blocked:trust_dialog"}),
        ),
        Step::Held => return None,
        Step::Ping => (
            "admin-notify",
            json!({
                "id": MSG, "level": "action_required", "pane_id": PANE,
                "subject": "reviewer: a prompt is waiting"
            }),
        ),
    };
    Some(event(name, data, ts))
}

/// Does this step leave %1 blocked? The gate reads the pane's fused state,
/// so the sweep has to track it to write the hold the daemon would write.
fn leaves_pane_blocked(step: Step, was: bool) -> bool {
    match step {
        Step::Prompt | Step::Quota => true,
        Step::Working | Step::PaneGone => false,
        _ => was,
    }
}

fn event(name: &str, mut data: Value, ts: u64) -> Entry {
    if let Some(map) = data.as_object_mut() {
        map.insert("ts".into(), json!(ts));
    }
    Entry::from_event(
        &Event {
            event: name.into(),
            data,
            seq: None,
        },
        ts,
    )
}

// ---------------------------------------------------------------------------
// Reading a frame the way a reader does
// ---------------------------------------------------------------------------

/// Which item a row is about, at the resolution a rendered row carries:
/// the name in the gutter, plus which half the cell belongs to. One pane
/// and one message per name in this sweep, so this is the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Half {
    /// A pane's own state.
    Agent,
    /// A delivery to that recipient.
    Delivery,
    /// A gate hold. Not a register item: it says a human is needed because
    /// a PANE is blocked, so the pane's clearance is what answers it.
    Held,
}

/// What one stream row says about a human being needed.
enum Says<'a> {
    /// This row is an alarm about (name, half), wearing this cell.
    Alarm(&'a str, Half, &'a str),
    /// This row answers an alarm about `name`, naming the cell it wore.
    Answers(&'a str, &'a str),
    /// A ping claiming a human is needed. It names no register item a row
    /// can show, so nothing in the body can answer it.
    Claim,
    Nothing,
}

/// Every cell the calm view can wear because it says a human is needed.
///
/// Built from the rule's own states and rendered through the same code the
/// stream renders them with, so a change to either shows up here rather
/// than drifting past a hardcoded string.
struct Vocabulary {
    agent: Vec<String>,
    delivery: Vec<String>,
    held: String,
}

impl Vocabulary {
    fn build() -> Vocabulary {
        // Every state, filtered by the rule. Listing the blocked ones here
        // would be a second copy of the rule
        // (crates/cyclops-proto/tests/one_place.rs).
        let every_agent_state = [
            AgentState::Unknown,
            AgentState::Idle,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
            AgentState::Dead,
        ];
        let every_delivery_state = [
            DeliveryState::Queued,
            DeliveryState::Gating,
            DeliveryState::Pasting,
            DeliveryState::Staged,
            DeliveryState::Submitted,
            DeliveryState::DeliveredVerified,
            DeliveryState::DeliveredUnverified,
            DeliveryState::RetryQueued,
            DeliveryState::AttentionRequired,
            DeliveryState::ParkedBlockedQuota,
        ];
        // The gate's calm-view cell, taken off a rendered line rather than
        // copied: the hold is the one gate line that reaches this view and
        // its copy lives in cyclops-ui `Entry::content`.
        let held = event(
            "gate",
            json!({"to": AGENT, "action": "hold", "cause": "blocked:trust_dialog"}),
            BASE,
        );
        // The frame puts a two-column marker in front of every stream row;
        // `Entry::lines` renders the row without it.
        let held = format!("  {}", held.lines(&Theme::none(), false).remove(0));
        let (_, held) = split_row(&held).expect("the hold renders a name and a cell");
        Vocabulary {
            agent: every_agent_state
                .into_iter()
                .filter(|s| s.is_blocked())
                .map(|s| grid::state_cell(s, &grid::Plain))
                .collect(),
            delivery: every_delivery_state
                .into_iter()
                .filter(|s| cyclops_proto::delivery_needs_human(*s))
                .map(|s| grid::delivery_badge(RECIPIENT, s, None, &grid::Plain))
                .collect(),
            held: held.to_string(),
        }
    }

    fn reads<'a>(&self, row: &'a str) -> Says<'a> {
        let Some((name, cell)) = split_row(row) else {
            return Says::Nothing;
        };
        // A clearance names the cell it answers after "was", so the two
        // rows match by sight and this matches them the same way.
        if let Some(was) = cell.strip_prefix('✔').and_then(|c| c.split_once(" · was ")) {
            return Says::Answers(name, was.1);
        }
        if name.is_empty() {
            // No name in the gutter: the row is a ping, which is content
            // with no item beside it.
            return if PING_CLAIMS.iter().any(|c| cell.starts_with(c)) {
                Says::Claim
            } else {
                Says::Nothing
            };
        }
        if self.agent.iter().any(|a| cell.starts_with(a.as_str())) {
            return Says::Alarm(name, Half::Agent, cell);
        }
        // The badge carries the cause after the state, so a park written
        // by the daemon is the badge plus a qualifier.
        if self.delivery.iter().any(|d| cell.starts_with(d.as_str())) {
            return Says::Alarm(name, Half::Delivery, cell);
        }
        if cell == self.held {
            return Says::Alarm(name, Half::Held, cell);
        }
        Says::Nothing
    }
}

/// The two ping levels that claim a human is needed (cyclops-ui
/// `Entry::content`). An fyi claims nothing.
const PING_CLAIMS: [&str; 2] = ["⚠ action required", "⚠ urgent"];

/// One stream row split into the name in the gutter and the cell after it:
/// "  12:04:31  reviewer  ⚠ blocked_permission". The name is empty for a
/// row that carries content without one, which is what a ping is.
fn split_row(row: &str) -> Option<(&str, &str)> {
    let rest = row.strip_prefix("  ").or_else(|| row.strip_prefix("▸ "))?;
    let (clock, rest) = rest.split_once("  ")?;
    if clock.len() != 8 || !clock.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(match rest.split_once("  ") {
        Some((name, cell)) => (name, cell),
        None => ("", rest),
    })
}

/// Every alarm row this frame leaves with nothing under it that answers.
///
/// Read top to bottom, the way the reader reads it. A later alarm about
/// the same item supersedes an earlier one: one pane has one current
/// state, and one message has one, so the newest row is the claim standing
/// and the older one is the record of how it got there. A clearance
/// answers the item it names, and a pane's clearance answers the gate hold
/// that was waiting on that pane, which has no item of its own.
///
/// Pings count in the calm view only. The firehose keeps every ping the
/// daemon sent (docs/ui.md) and shows every transition beside it, so a
/// ping there is history with its own answer already on screen; in the
/// calm view a ping stands only while the register backs it, and one that
/// slipped through is exactly the contradiction this sweep is about.
fn unanswered(vocab: &Vocabulary, rows: &[String], view: View) -> Vec<String> {
    let mut open: HashMap<(String, Half), String> = HashMap::new();
    let mut claims = Vec::new();
    for row in rows {
        match vocab.reads(row) {
            Says::Alarm(name, half, cell) => {
                open.insert((name.to_string(), half), cell.to_string());
            }
            Says::Answers(name, was) => {
                let half = if vocab.agent.iter().any(|a| was.starts_with(a.as_str())) {
                    Half::Agent
                } else {
                    Half::Delivery
                };
                open.remove(&(name.to_string(), half));
                if half == Half::Agent {
                    open.remove(&(name.to_string(), Half::Held));
                }
            }
            Says::Claim if view == View::Admin => claims.push(row.clone()),
            Says::Claim | Says::Nothing => {}
        }
    }
    let mut out: Vec<String> = open.into_values().chain(claims).collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// The minimal case
// ---------------------------------------------------------------------------

/// Two live events on an empty ledger with default flags, which is the
/// most ordinary path in the product: a pane stops on a permission prompt
/// and then takes a turn again.
///
/// The alarm line is admin-visible and the line that ends it is not, so
/// the calm view showed the alarm and never the clearance. Both lines
/// belong here.
#[test]
fn the_calm_view_shows_the_alarm_and_the_line_that_ends_it() {
    let mut app = App::new(Theme::none(), View::Admin, Filter::default());
    app.live(event_for(Step::Prompt, BASE, false).expect("a prompt event"));
    app.live(event_for(Step::Working, BASE + 1000, true).expect("a working event"));
    while app.tick_eye() {}

    let rows = build(&mut app, 8);
    assert_eq!(
        rows,
        vec![
            "‿ cyclops · admin stream".to_string(),
            String::new(),
            "  12:04:31  reviewer  ⚠ blocked_permission".to_string(),
            String::new(),
            "  12:04:32  reviewer  ✔ cleared · was ⚠ blocked_permission".to_string(),
            String::new(),
            String::new(),
            "? keys · tab view · c density · w/f/t filter · enter jump · q quit".to_string(),
        ]
    );
}

/// The delivery half of the same two lines, and the pane's other ending.
/// A delivery that ran out of redelivery is requeued by an operator; a
/// blocked pane can also end by the pane going away, and the reader is
/// owed the difference between "someone answered it" and "the window
/// closed".
#[test]
fn a_clearance_names_what_it_resolved_on_both_halves() {
    let lines = |steps: &[Step]| {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        let mut blocked = false;
        for (i, step) in steps.iter().enumerate() {
            if let Some(e) = event_for(*step, BASE + i as u64 * 1000, blocked) {
                app.live(e);
            }
            blocked = leaves_pane_blocked(*step, blocked);
        }
        let rows = build(&mut app, 20);
        rows.into_iter()
            .filter_map(|r| split_row(&r).map(|(n, c)| format!("{n}|{c}")))
            .collect::<Vec<String>>()
    };

    assert_eq!(
        lines(&[Step::NeedsHuman, Step::Requeued]),
        vec![
            "implementer|⚠ needs attention",
            "implementer|✔ cleared · was ⚠ needs attention",
        ]
    );
    assert_eq!(
        lines(&[Step::Parked, Step::Requeued]),
        vec![
            "implementer|⊘ parked · quota",
            "implementer|✔ cleared · was ⊘ parked · quota",
        ]
    );
    assert_eq!(
        lines(&[Step::Prompt, Step::PaneGone]),
        vec![
            "reviewer|⚠ blocked_permission",
            "reviewer|✔ pane closed · was ⚠ blocked_permission",
        ]
    );
    // A hold waits on the pane, so the pane's clearance is what ends it.
    // Nothing else in the calm view speaks for a gate line.
    assert_eq!(
        lines(&[Step::Prompt, Step::Held, Step::Working]),
        vec![
            "reviewer|⚠ blocked_permission",
            "reviewer|⚠ held · a prompt in this pane needs you",
            "reviewer|✔ cleared · was ⚠ blocked_permission",
        ]
    );
    // One pane, two alarms, one clearance: the newest alarm is the one
    // standing, and the clearance names it.
    assert_eq!(
        lines(&[Step::Prompt, Step::Quota, Step::Working]),
        vec![
            "reviewer|⚠ blocked_permission",
            "reviewer|⊘ blocked_quota",
            "reviewer|✔ cleared · was ⊘ blocked_quota",
        ]
    );
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Every filter the UI can be run under, plus none. `with` is either
/// direction and `from`/`to` are one each, so the three of them reach the
/// alarm rows by different paths and a clearance has to pass whichever the
/// alarm passed.
fn filters() -> Vec<Filter> {
    vec![
        Filter::default(),
        Filter {
            with: Some(AGENT.into()),
            ..Filter::default()
        },
        Filter {
            from: Some(AGENT.into()),
            ..Filter::default()
        },
        Filter {
            to: Some(RECIPIENT.into()),
            ..Filter::default()
        },
    ]
}

/// Terminal heights: a sliver that fits nothing, a short window that cuts
/// the top of the stream, and two that hold the whole sequence.
const HEIGHTS: [usize; 4] = [4, 7, 14, 28];

#[test]
fn no_calm_frame_leaves_an_alarm_unanswered() {
    let vocab = Vocabulary::build();
    let filters = filters();
    let mut frames = 0usize;
    let mut sequences = 0usize;

    for len in 1..=MAX_LEN {
        for seq in sequences_of_length(len) {
            sequences += 1;
            // One app per sequence: the view, the density, the filter and
            // the height are all rendering, so the same run answers for
            // every combination of them.
            let mut app = App::new(Theme::none(), View::Admin, Filter::default());
            let mut blocked = false;
            for (i, step) in seq.iter().enumerate() {
                if let Some(e) = event_for(*step, BASE + i as u64 * 1000, blocked) {
                    app.live(e);
                }
                blocked = leaves_pane_blocked(*step, blocked);
            }
            // The eye ticks one frame per change; settle it, because the
            // glyph a settled frame draws is the one this is about.
            while app.tick_eye() {}

            for view in [View::Admin, View::Firehose] {
                for density in [Density::Comfortable, Density::Compact] {
                    for filter in &filters {
                        for height in HEIGHTS {
                            let how = Render {
                                view,
                                density,
                                filter: filter.clone(),
                                height,
                            };
                            app.view = how.view;
                            app.density = how.density;
                            app.filter = how.filter.clone();
                            let rows = build(&mut app, how.height);
                            frames += 1;
                            check(&vocab, &app, &rows, &seq, &how);
                        }
                    }
                }
            }
        }
    }

    // The sweep is only worth its runtime if it actually ran: the numbers
    // are the measurement this test is a checked-in copy of.
    let expected: usize = (1..=MAX_LEN).map(|n| ALPHABET.len().pow(n as u32)).sum();
    assert_eq!(sequences, expected, "the generator lost sequences");
    assert_eq!(
        frames,
        expected * 2 * 2 * filters.len() * HEIGHTS.len(),
        "the render matrix lost frames"
    );
    println!("swept {frames} frames over {sequences} sequences");
}

/// One way of rendering the same run. Every field is presentation, which
/// is why one sequence answers for all of them at once.
#[derive(Debug)]
struct Render {
    view: View,
    density: Density,
    filter: Filter,
    height: usize,
}

/// One frame, one assertion: a calm eye never sits above a row saying a
/// human is needed that nothing under it answers.
fn check(vocab: &Vocabulary, app: &App, rows: &[String], seq: &[Step], how: &Render) {
    let calm = app.attention().header_drawn(app.eye()).calm;
    let says_calm = rows.iter().any(|r| r.contains("All calm"));
    if !calm && !says_calm {
        return;
    }
    let left = unanswered(vocab, rows, how.view);
    assert!(
        left.is_empty(),
        "a calm frame left {left:?} with nothing under it that answers.\n\
         sequence {seq:?}, {how:?}\n{rows:#?}"
    );
    if says_calm {
        // The copy is stronger than the glyph: it says there is nothing to
        // look at, so the frame may carry no alarm row at all.
        let alarms: Vec<&String> = rows
            .iter()
            .filter(|r| matches!(vocab.reads(r), Says::Alarm(..)))
            .collect();
        assert!(
            alarms.is_empty(),
            "\"All calm\" over {alarms:?}\nsequence {seq:?}, {how:?}\n{rows:#?}"
        );
    }
    // A calm eye and a count are one value; a frame that draws one without
    // the other is the same contradiction read from the header alone.
    if calm {
        assert_eq!(app.attention_count(), 0, "a calm eye over a count");
    }
}

/// Every sequence of exactly `len` steps, in odometer order.
fn sequences_of_length(len: usize) -> Vec<Vec<Step>> {
    let mut out = Vec::new();
    let mut idx = vec![0usize; len];
    loop {
        out.push(idx.iter().map(|i| ALPHABET[*i]).collect());
        let mut at = len;
        loop {
            if at == 0 {
                return out;
            }
            at -= 1;
            idx[at] += 1;
            if idx[at] < ALPHABET.len() {
                break;
            }
            idx[at] = 0;
        }
    }
}

/// The sweep reads rows, so it can only see what a reader sees. This holds
/// it to that: every kind of row it classifies is a row the stream really
/// renders, and a row it calls nothing really says nothing.
#[test]
fn the_row_reader_recognises_what_the_stream_renders() {
    let vocab = Vocabulary::build();
    let seen: HashSet<&str> = ["alarm", "answers", "claim", "nothing"]
        .into_iter()
        .collect();
    let mut found = HashSet::new();
    let mut app = App::new(Theme::none(), View::Firehose, Filter::default());
    for (i, step) in [
        Step::Prompt,
        Step::Held,
        Step::Ping,
        Step::Working,
        Step::Parked,
        Step::Requeued,
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(e) = event_for(step, BASE + i as u64 * 1000, i > 0) {
            app.live(e);
        }
    }
    for row in build(&mut app, 30) {
        found.insert(match vocab.reads(&row) {
            Says::Alarm(..) => "alarm",
            Says::Answers(..) => "answers",
            Says::Claim => "claim",
            Says::Nothing => "nothing",
        });
    }
    assert_eq!(found, seen, "the reader never saw one of its own cases");
}
