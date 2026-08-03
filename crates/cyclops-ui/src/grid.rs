//! The strict grid: the timestamp gutter, the cause vocabulary, the state
//! cells and the badge voice. The stream, receipts, and history are one
//! product surface, so this is one module and not two.
//!
//! The CLI (crates/cyclops) renders the same voice on receipts, history,
//! and the status grid. It already depends on this crate to run
//! `cyclops ui`, so it calls these functions. It used to hold a
//! hand-written copy of them, justified by the CLI being a binary, with a
//! suite of "badge drift" parity tests policing the copy; the parity tests
//! themselves imported this module, which is what showed the justification
//! was never true.
//!
//! Color is the one thing the two surfaces do differently: the CLI holds a
//! `Style` and the stream a `Theme`. Both resolve the same cyclops-theme
//! tokens, so a cell takes the caller's [`Paint`] and phrases everything
//! else itself.
//!
//! The width helpers are not the CLI's padding helpers. This crate never
//! pads or truncates: autowrap is off and the terminal clips its own edge
//! (frame.rs). `display_width` stays because the eye's glyphs have to
//! measure one column or the header grid loses its alignment (theme.rs
//! pins that), and the CLI's `pad`/`pad_left` measure through it too, so
//! the two surfaces cannot disagree about how wide a badge is.

use cyclops_proto::{AgentState, AttentionItem, Clearance, DeliveryReceipt, DeliveryState};

/// The caller's color, for the cells this module composes.
///
/// The one thing a cell cannot decide for itself: the CLI paints through
/// `Style`, the stream through `cyclops_ui::Theme`, and both resolve the
/// same cyclops-theme tokens. Three implementors and the reason is drift,
/// not symmetry: the state cell and the delivery badge used to be painted
/// at the call site, so the CLI colored them and the stream rendered the
/// same cell bare against the same theme.
///
/// Color is strictly redundant (GOALS). Every cell here carries its glyph
/// and its word first, so [`Plain`] loses nothing.
pub trait Paint {
    /// The detail after a badge's separator: `surface.dim`.
    fn dim(&self, text: &str) -> String;
    /// One agent state, in its group's color (`state.*`).
    fn state(&self, state: AgentState, text: &str) -> String;
    /// One delivery state, in its group's color (`badge.*`).
    fn badge(&self, state: DeliveryState, text: &str) -> String;
}

/// The painter for a surface with no color at all: `--plain`, the
/// attention band's item phrase, and every exact-string test. Named once
/// so no caller has to write three closures that say nothing.
pub struct Plain;

impl Paint for Plain {
    fn dim(&self, text: &str) -> String {
        text.to_string()
    }

    fn state(&self, _state: AgentState, text: &str) -> String {
        text.to_string()
    }

    fn badge(&self, _state: DeliveryState, text: &str) -> String {
        text.to_string()
    }
}

/// Display width of one char, covering the glyph set cyclops prints plus
/// the broad wide ranges pane-derived strings can carry.
///
/// Private: [`display_width`] is the whole public question, and every
/// surface asks it. A second caller measuring char by char would be
/// deciding its own column arithmetic.
///
/// Cyclops's own glyphs are all one column and none of them is listed
/// here. The ranges are for text that comes off a pane: CJK, fullwidth
/// forms, and the emoji planes. `AgentState::glyph` used to return U+26D4,
/// which is East Asian Wide and outside every range below, and it carried
/// a hardcoded entry of its own; the glyph is now U+2298 and the entry is
/// gone with it.
fn char_width(c: char) -> usize {
    match c {
        '\u{0300}'..='\u{036f}' => 0,
        '\u{1100}'..='\u{115f}'
        | '\u{2e80}'..='\u{a4cf}'
        | '\u{ac00}'..='\u{d7a3}'
        | '\u{f900}'..='\u{faff}'
        | '\u{fe30}'..='\u{fe4f}'
        | '\u{ff00}'..='\u{ff60}'
        | '\u{ffe0}'..='\u{ffe6}'
        | '\u{1f300}'..='\u{1faff}' => 2,
        _ => 1,
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// HH:MM:SS in UTC, the stream gutter. Same clock as `cyclops watch`.
pub fn clock_hms(ts_ms: u64) -> String {
    let s = (ts_ms / 1000) % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Glyph plus word, in the state's group color. "● working", "○ idle".
///
/// The two encodings GOALS allows are role color and the state glyph, and
/// they never share a cell: the role hue paints the agent NAME, this
/// paints the cell. The color is redundant with the glyph and the word,
/// which is why [`Plain`] is a complete rendering and not a degraded one.
pub fn state_cell(s: AgentState, paint: &dyn Paint) -> String {
    paint.state(s, &state_words(s))
}

/// The state cell's words alone, for a caller that must MEASURE the cell
/// before painting it. The CLI's status grid pads the column to its widest
/// cell, and padding after the paint would count escape bytes as columns.
///
/// Only that caller: everything else takes the painted cell, or the same
/// words through [`Plain`]. The words themselves are
/// [`cyclops_proto::state_words`], because the daemon writes the same cell
/// onto tmux pane borders and does not link this crate.
pub use cyclops_proto::state_words;

/// Machine causes in user-side words. GOALS: no `pane_id`, no `NDJSON`,
/// nothing a newcomer has to look up, on any surface that shows a cause.
///
/// This is the only place a delivery cause becomes English. Receipts,
/// history and the stream all reach it, which is the point: the same cause
/// said two ways on two surfaces is a rule with two homes, and one of them
/// drifts. "no manifest" was that drift, and it also failed the GOALS test
/// on its own, because a reader hitting it for the first time has no idea
/// what a manifest is.
pub fn cause_words(cause: &str) -> String {
    match cause {
        "no_such_pane" => "no pane with that name".into(),
        "no_manifest" => "nothing detects its pane".into(),
        "daemon_restart" => "daemon restarted mid-delivery".into(),
        _ => cause.replace('_', " "),
    }
}

/// [`cause_words`] for the one surface that knows which pane: a receipt.
///
/// The record does not carry a pane on a delivery, so history and the
/// stream name what they have and stop. A receipt is answering a send that
/// resolved to a pane, and naming it is what makes the fix pasteable.
/// Both spellings live here, next to each other, so neither can move
/// without the other.
pub fn cause_words_for(cause: &str, pane: Option<&str>) -> String {
    match (cause, pane) {
        ("no_manifest", Some(pane)) => format!("nothing detects {pane}"),
        _ => cause_words(cause),
    }
}

/// One delivery badge, the exact M1 voice: glyph plus word, qualifier
/// after a dim separator, all of it in the delivery state's group color.
/// Worn by receipts, history, the status grid, and the stream.
///
/// The group color opens around the whole composed badge and the dim runs
/// inside it close themselves, which leaves the glyph and the word in the
/// group color and the qualifier dim. That holds only because a badge
/// never puts an unpainted word AFTER a dim run.
pub fn receipt_badge(r: &DeliveryReceipt, paint: &dyn Paint) -> String {
    let sep = paint.dim("·");
    let with = |head: &str, tail: &str| format!("{head} {sep} {}", paint.dim(tail));
    let words = match r.state {
        DeliveryState::Queued => match r.position {
            Some(n) => with("● queued", &format!("{n} ahead")),
            None => "● queued".into(),
        },
        DeliveryState::Gating => "● gating".into(),
        DeliveryState::Pasting => "● pasting".into(),
        DeliveryState::Staged => "● staged".into(),
        DeliveryState::Submitted => "● submitted".into(),
        DeliveryState::RetryQueued => "● retrying".into(),
        DeliveryState::DeliveredVerified => with("✔ delivered", "verified"),
        DeliveryState::DeliveredUnverified => with("✓ delivered", "unverified (screen)"),
        // The qualifier is the gate's cause worded here, never at the
        // daemon: a receipt that arrived carrying a sentence keeps it
        // (cause_words leaves anything it does not know alone), and one
        // that arrived carrying a cause gets the same words history does.
        DeliveryState::AttentionRequired => match &r.note {
            Some(note) => with(
                "⚠ needs attention",
                &cause_words_for(note, r.pane.as_deref()),
            ),
            None => "⚠ needs attention".into(),
        },
        DeliveryState::ParkedBlockedQuota => match &r.note {
            Some(note) => with("⊘ parked", &format!("quota, {note}")),
            None => with("⊘ parked", "quota"),
        },
    };
    paint.badge(r.state, &words)
}

/// Badge for a delivery transition, folded from the recorded state and
/// cause: the same badge a receipt wears, fed from a record line instead
/// of a send.
pub fn delivery_badge(
    to: &str,
    state: DeliveryState,
    cause: Option<&str>,
    paint: &dyn Paint,
) -> String {
    let note = match state {
        DeliveryState::AttentionRequired => cause.map(String::from),
        _ => None,
    };
    receipt_badge(
        &DeliveryReceipt {
            to: to.to_string(),
            state,
            position: None,
            note,
            // A record line names the recipient, not the pane it lived in.
            pane: None,
        },
        paint,
    )
}

/// One attention item said the way the stream says it: the name, then the
/// same cell or badge its own line carries. The cause stays off it; the
/// item names what needs doing, and the line in the stream carries why.
pub fn attention_phrase(item: &AttentionItem, paint: &dyn Paint) -> String {
    format!("{}  {}", item.name(), item_cell(item, paint))
}

/// The cell an item's own line wears: the state cell for a pane, the badge
/// for a delivery.
fn item_cell(item: &AttentionItem, paint: &dyn Paint) -> String {
    match item {
        AttentionItem::Agent { state, .. } => state_cell(*state, paint),
        AttentionItem::Delivery { to, state, .. } => delivery_badge(to, *state, None, paint),
    }
}

/// The cell a clearance line wears: what happened, then the alarm it
/// answers in the exact words that alarm's own row wore.
///
/// Quoting the alarm rather than renaming it is the whole point. The
/// reader scanning the calm view sees "⚠ blocked_permission" on one row
/// and "was ⚠ blocked_permission" on the next, and needs nothing from the
/// firehose to connect them.
pub fn cleared_cell(was: &AttentionItem, how: Clearance, paint: &dyn Paint) -> String {
    let head = match how {
        Clearance::Moved => "✔ cleared",
        // Nobody answered it. Saying "cleared" would claim otherwise.
        Clearance::PaneGone => "✔ pane closed",
    };
    format!("{head} {} was {}", paint.dim("·"), item_cell(was, paint))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(state: DeliveryState, position: Option<u32>, note: Option<&str>) -> DeliveryReceipt {
        DeliveryReceipt {
            to: "reviewer".into(),
            state,
            position,
            note: note.map(String::from),
            pane: None,
        }
    }

    /// The badge voice, pinned once. Receipts, history, the status grid
    /// and the stream all read this table, so these strings are the whole
    /// vocabulary and there is nothing left to keep in step with it.
    #[test]
    fn the_badge_voice_is_exact() {
        use DeliveryState::*;
        let cases = [
            (
                receipt(DeliveredVerified, None, None),
                "✔ delivered · verified",
            ),
            (
                receipt(DeliveredUnverified, None, None),
                "✓ delivered · unverified (screen)",
            ),
            (receipt(Queued, Some(2), None), "● queued · 2 ahead"),
            (receipt(Queued, None, None), "● queued"),
            (
                receipt(ParkedBlockedQuota, None, Some("resets in 135h")),
                "⊘ parked · quota, resets in 135h",
            ),
            (receipt(ParkedBlockedQuota, None, None), "⊘ parked · quota"),
            (
                receipt(AttentionRequired, None, Some("target pane is gone")),
                "⚠ needs attention · target pane is gone",
            ),
            (receipt(AttentionRequired, None, None), "⚠ needs attention"),
            (receipt(Gating, None, None), "● gating"),
            (receipt(Pasting, None, None), "● pasting"),
            (receipt(Staged, None, None), "● staged"),
            (receipt(Submitted, None, None), "● submitted"),
            (receipt(RetryQueued, None, None), "● retrying"),
        ];
        for (r, want) in &cases {
            assert_eq!(receipt_badge(r, &Plain), *want);
        }
    }

    #[test]
    fn a_recorded_delivery_folds_its_cause_into_the_badge() {
        assert_eq!(
            delivery_badge(
                "reviewer",
                DeliveryState::AttentionRequired,
                Some("no_such_pane"),
                &Plain
            ),
            "⚠ needs attention · no pane with that name"
        );
        assert_eq!(
            delivery_badge(
                "reviewer",
                DeliveryState::ParkedBlockedQuota,
                Some("blocked_quota"),
                &Plain
            ),
            "⊘ parked · quota"
        );
    }

    #[test]
    fn the_clock_gutter_and_the_width_table_are_exact() {
        assert_eq!(clock_hms(43_471_000), "12:04:31");
        assert_eq!(clock_hms(0), "00:00:00");
        assert_eq!(display_width("● working"), 9);
        assert_eq!(display_width("⊘ blocked_quota"), 15);
        assert_eq!(display_width("✔"), 1);
        assert_eq!(display_width("日本"), 4);
    }

    /// Every state glyph occupies exactly one column. The grid is strict
    /// (GOALS): one wide glyph puts every cell after it a column out, and
    /// a glyph the terminal draws in its emoji font takes no theme color.
    /// blocked_quota shipped as U+26D4, which is both.
    #[test]
    fn every_state_glyph_is_one_column() {
        for s in [
            AgentState::Unknown,
            AgentState::Idle,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
            AgentState::Dead,
        ] {
            assert_eq!(display_width(s.glyph()), 1, "{s} is not one column");
        }
    }

    #[test]
    fn state_cells_pair_glyph_and_word() {
        assert_eq!(state_cell(AgentState::Working, &Plain), "● working");
        assert_eq!(state_cell(AgentState::Idle, &Plain), "○ idle");
        assert_eq!(
            state_cell(AgentState::BlockedQuota, &Plain),
            "⊘ blocked_quota"
        );
        // The words the CLI measures before it pads are the same words.
        assert_eq!(state_words(AgentState::Working), "● working");
    }

    #[test]
    fn attention_phrases_wear_the_stream_voice() {
        let agent = AttentionItem::Agent {
            pane_id: "%1".into(),
            name: "reviewer".into(),
            state: AgentState::BlockedPermission,
        };
        assert_eq!(
            attention_phrase(&agent, &Plain),
            "reviewer  ⚠ blocked_permission"
        );
        let delivery = AttentionItem::Delivery {
            to: "implementer".into(),
            id: "m-1".into(),
            state: DeliveryState::ParkedBlockedQuota,
        };
        assert_eq!(
            attention_phrase(&delivery, &Plain),
            "implementer  ⊘ parked · quota"
        );
    }
}
