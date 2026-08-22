//! The line-oriented follow mode: `--plain`, or no terminal to take over.
//!
//! NO_COLOR does not land here. It is a color preference, and the full UI
//! is legible without color, so it turns the paint off and nothing else
//! (lib.rs `wants_plain`).
//!
//! No raw mode, no frames, no color: the backfill tail prints first, then
//! the startup reconciliation, then every admitted event as it arrives, in
//! the same voice and at the same content as the full UI's comfortable
//! density (a message prints its body's first line under it). The eye
//! degrades to a plain word line whenever what it counts changes, naming
//! every item. Ctrl-C exits; a dead daemon connection ends the follow with
//! the standard copy on stderr and exit 1.

use std::io::Write;
use std::path::Path;

use crate::app::{App, View};
use crate::data::{self, UiMsg};
use crate::stream::{Entry, Intake};
use crate::theme::Theme;
use crate::UiOptions;

pub async fn run(opts: &UiOptions, home: &Path) -> i32 {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    data::spawn_io(&tx, home, opts.backfill);
    let view = if opts.firehose {
        View::Firehose
    } else {
        View::Admin
    };
    let mut app = App::new(Theme::none(), view, opts.filter());
    let mut intake = Intake::new();
    // The last eye line printed, so a change prints exactly once.
    let mut eye_printed: Option<Vec<String>> = None;
    // A lost connection ends the follow, but only after the backfill has
    // flushed: buffered lines must land before the exit.
    let mut lost: Option<String> = None;
    let mut stdout = std::io::stdout();

    while let Some(msg) = rx.recv().await {
        match msg {
            // Plain output has no messages surface yet. Dropping the
            // snapshot keeps this stream line-oriented rather than
            // half-rendering a list it cannot navigate.
            UiMsg::Messages { .. }
            | UiMsg::MessagesFailed { .. }
            | UiMsg::MessagesChanged(_)
            | UiMsg::MessagesRouteChanged
            | UiMsg::Subscribed
            | UiMsg::ActionDone { .. } => {}
            UiMsg::Entry(e) => {
                for e in intake.entry(*e) {
                    live(&mut app, e, &mut stdout);
                }
            }
            UiMsg::Backfill { entries, max_seq } => {
                // The startup order, and it is the whole correctness
                // story: the replayed tail is history, the seed is the
                // daemon's answer about now, and the live entries that
                // queued behind them are newer than both.
                let landed = intake.backfill(entries, max_seq);
                for e in landed.replayed {
                    replay(&mut app, e, &mut stdout);
                }
                if let Some(seed) = landed.seed {
                    seed_status(&mut app, *seed, &mut stdout);
                }
                for e in landed.live {
                    live(&mut app, e, &mut stdout);
                }
            }
            UiMsg::Status(seed) => {
                if let Some(seed) = intake.status(seed) {
                    seed_status(&mut app, *seed, &mut stdout);
                }
            }
            UiMsg::ConnLost(text) => lost = Some(text),
            // TUI-only traffic. Plain mode holds no theme at all
            // (`Theme::none`, this is the screen-reader mode), so a theme
            // switch has nothing here to move.
            UiMsg::Key(_) | UiMsg::Notice(_) | UiMsg::EyeTick | UiMsg::ThemeChanged => {}
        }
        eye_line(&app, &mut eye_printed, &mut stdout);
        let _ = stdout.flush();
        if let Some(text) = &lost {
            if intake.is_backfilled() {
                eprintln!("{text}");
                return 1;
            }
        }
    }
    0
}

/// Apply the startup reconciliation, printing the lines it wrote for items
/// the replayed tail does not already carry. A count with no line under it
/// is exactly what plain mode cannot afford: it has no header to point at.
fn seed_status(app: &mut App, seed: crate::stream::StatusSeed, out: &mut impl Write) {
    for e in app.seed_status(seed) {
        replay(app, e, out);
    }
}

/// One replayed line: print it if the view admits it, then hand it to the
/// app as history.
fn replay(app: &mut App, e: Entry, out: &mut impl Write) {
    print_line(app, &e, out);
    app.replay(e);
}

/// One live event: print it if the view admits it, then hand it to the app
/// as the transition it is. Hidden entries still move the count.
///
/// A transition that ended something needing a human hands back a
/// clearance, and that line prints too. It is the only news the calm view
/// gets that the alarm it printed is over, and this mode cannot go back
/// and amend a line it already wrote.
fn live(app: &mut App, e: Entry, out: &mut impl Write) {
    print_line(app, &e, out);
    if let Some(cleared) = app.live(e) {
        print_line(app, &cleared, out);
    }
}

/// Plain mode is the screen-reader view, so it carries what the sighted
/// comfortable density carries: the entry line, plus the message body's
/// first line hanging at the content column. Never less.
fn print_line(app: &App, e: &Entry, out: &mut impl Write) {
    // The same admission the full UI's window applies, asked of the same
    // app: this mode prints one line at a time and has no frame to
    // reconcile, so a line it prints is a line it can never take back.
    let admitted = app.admits_in_view(e) && app.filter.matches(e);
    if admitted {
        for line in e.lines(&app.theme, true) {
            let _ = writeln!(out, "{line}");
        }
    }
}

/// The eye as a plain word: the position, the count, and what it counts.
/// The initial closed state prints nothing.
///
/// The position and the count words are the register's
/// ([`cyclops_proto::EyeHeader::spoken`]), the same composition the full
/// UI's header and `cyclops status` wear. This mode restated them, so a
/// change to the header's words left plain saying the old ones with
/// nothing able to fail. All plain adds is the item list.
///
/// Printed whenever the ITEM SET changes, not merely when the count does.
/// Naming the items is this mode's whole answer to "every count has a line
/// behind it": there is no header to point at and no band, and an item's
/// own line may be filtered out or older than the follow. One item
/// swapping for another leaves the count alone, so a count-only trigger
/// left the newcomer with nothing said about it anywhere.
fn eye_line(app: &App, printed: &mut Option<Vec<String>>, out: &mut impl Write) {
    let items = app.attention_items();
    if printed.as_ref() == Some(&items) || (printed.is_none() && items.is_empty()) {
        return;
    }
    let spoken = app.attention().header().spoken;
    let line = if items.is_empty() {
        spoken
    } else {
        format!("{spoken} · {}", items.join(" · "))
    };
    let _ = writeln!(out, "{line}");
    *printed = Some(items);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{EntryKind, Filter};
    use cyclops_proto::AgentState;

    fn state(target: &str, s: AgentState) -> Entry {
        Entry {
            uid: 0,
            ts: 43_471_000,
            seq: None,
            id: Some("e-1".into()),
            kind: EntryKind::State {
                target: target.into(),
                session_idx: 0,
                pane_id: None,
                state: s,
            },
        }
    }

    #[test]
    fn eye_words_print_on_change_only() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        let mut printed = None;
        let mut out = Vec::new();
        // Calm start: nothing printed.
        eye_line(&app, &mut printed, &mut out);
        assert!(out.is_empty());
        // One blocked agent: opening.
        app.live(state("reviewer", AgentState::BlockedPermission));
        eye_line(&app, &mut printed, &mut out);
        // Same state again: no repeat.
        eye_line(&app, &mut printed, &mut out);
        // A second: open. Then both clear: closed.
        app.live(state("implementer", AgentState::BlockedQuota));
        eye_line(&app, &mut printed, &mut out);
        app.live(state("reviewer", AgentState::Idle));
        app.live(state("implementer", AgentState::Idle));
        eye_line(&app, &mut printed, &mut out);
        // The count names what it counts: this mode has no header to
        // point at, and the item can be older than anything printed.
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "eye opening · 1 needs attention · reviewer  ⚠ blocked_permission\n\
             eye open · 2 need attention · implementer  ⊘ blocked_quota · \
             reviewer  ⚠ blocked_permission\n\
             eye closed\n"
        );
    }

    fn msg(from: &str, to: &[&str], subject: &str, body: Option<&str>) -> Entry {
        Entry {
            uid: 0,
            ts: 43_471_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Msg {
                from: from.into(),
                to: to.iter().map(|t| t.to_string()).collect(),
                subject: subject.into(),
                body: body.map(String::from),
                fyi: false,
            },
        }
    }

    /// --plain is the screen-reader mode, so it must never carry less
    /// than the sighted view: the body line prints, hanging at the same
    /// content column the TUI uses.
    #[test]
    fn plain_prints_the_body_line_like_comfortable_density() {
        let mut app = App::new(Theme::none(), View::Firehose, Filter::default());
        let mut out = Vec::new();
        live(
            &mut app,
            msg(
                "codex",
                &["reviewer"],
                "Review the rate limiter",
                Some("gateway.rs:120 drops the burst path\nsecond line stays off"),
            ),
            &mut out,
        );
        // A message with no body is still one line.
        live(&mut app, msg("codex", &["admin"], "done", None), &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "12:04:31  codex → reviewer  Review the rate limiter\n\
             \x20         gateway.rs:120 drops the burst path\n\
             12:04:31  codex → admin  done\n"
        );
    }

    /// The startup reconciliation prints. A count that moved with nothing
    /// printed under it is exactly what this mode cannot afford: it has no
    /// header to point at, and the item can predate the whole follow.
    #[test]
    fn the_seed_prints_the_line_behind_the_count() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        let mut out = Vec::new();
        let mut printed = None;
        seed_status(
            &mut app,
            crate::stream::StatusSeed {
                watched: vec!["main".into()],
                panes: Vec::new(),
                roster: Vec::new(),
                admin_unread: 0,
                open: vec![cyclops_proto::OpenDelivery {
                    id: "m-park".into(),
                    to: "implementer".into(),
                    state: cyclops_proto::DeliveryState::ParkedBlockedQuota,
                    ts: 43_471_000,
                    cause: Some("blocked_quota".into()),
                }],
            },
            &mut out,
        );
        eye_line(&app, &mut printed, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "12:04:31  implementer  ⊘ parked · quota\n\
             eye opening · 1 needs attention · implementer  ⊘ parked · quota\n"
        );
    }

    /// This mode prints a line and can never take it back, so a ping the
    /// register cannot back must not be printed at all: the eye line that
    /// follows it would be the contradiction, one line down.
    #[test]
    fn a_ping_the_register_cannot_back_never_prints() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        let mut out = Vec::new();
        let mut printed = None;
        live(&mut app, ping("m-1", Some("reviewer")), &mut out);
        eye_line(&app, &mut printed, &mut out);
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));

        // The same ping while its delivery is in the register: it prints,
        // and the eye line under it agrees.
        app.live(delivery_needing_a_human());
        live(&mut app, ping("m-1", Some("reviewer")), &mut out);
        eye_line(&app, &mut printed, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "12:04:31  ⚠ action required · delivery to reviewer needs attention\n\
             eye opening · 1 needs attention · reviewer  ⚠ needs attention\n"
        );
    }

    fn ping(id: &str, to: Option<&str>) -> Entry {
        Entry {
            uid: 0,
            ts: 43_471_000,
            seq: None,
            id: Some(id.into()),
            kind: EntryKind::Notify {
                level: cyclops_proto::NotifyLevel::ActionRequired,
                subject: "delivery to reviewer needs attention".into(),
                pane_id: None,
                to: to.map(String::from),
                deliveries: Vec::new(),
            },
        }
    }

    fn delivery_needing_a_human() -> Entry {
        Entry {
            uid: 0,
            ts: 43_471_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Delivery {
                to: "reviewer".into(),
                state: cyclops_proto::DeliveryState::AttentionRequired,
                cause: None,
            },
        }
    }

    /// This mode prints a line and can never take it back, so the line
    /// that ENDS an alarm has to print exactly as the alarm did. Without
    /// it the follow printed "reviewer ⚠ blocked_permission" and then
    /// "eye closed" with nothing in between, and the reader's only account
    /// of what happened was a number going down.
    #[test]
    fn the_line_that_ends_an_alarm_prints_under_it() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        let mut out = Vec::new();
        let mut printed = None;
        live(
            &mut app,
            state("reviewer", AgentState::BlockedPermission),
            &mut out,
        );
        eye_line(&app, &mut printed, &mut out);
        // The transition itself is not admin-visible and does not print;
        // the clearance it produced is, and does.
        live(&mut app, state("reviewer", AgentState::Working), &mut out);
        eye_line(&app, &mut printed, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "12:04:31  reviewer  ⚠ blocked_permission\n\
             eye opening · 1 needs attention · reviewer  ⚠ blocked_permission\n\
             12:04:31  reviewer  ✔ cleared · was ⚠ blocked_permission\n\
             eye closed\n"
        );
    }

    #[test]
    fn hidden_entries_still_move_the_eye() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        let mut out = Vec::new();
        // working is not admin-visible, but must still land in the app.
        live(&mut app, state("reviewer", AgentState::Working), &mut out);
        assert!(out.is_empty(), "working printed in the admin view");
        assert_eq!(app.len(), 1);
        // blocked is admin-visible and prints.
        live(
            &mut app,
            state("reviewer", AgentState::BlockedPermission),
            &mut out,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "12:04:31  reviewer  ⚠ blocked_permission\n"
        );
        assert_eq!(app.attention_count(), 1);
    }
}
