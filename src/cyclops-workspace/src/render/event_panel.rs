//! The slide-out event panel: the shared `cyclops watch` stream model's
//! admitted rows (E2), clipped to this narrower viewport. Row content,
//! order, and filtering come from `cyclops_ui::Record`; this only turns
//! its rows into a painted, backend-neutral text panel.

use std::collections::VecDeque;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthChar;

use cyclops_ui::{Entry, EntryKind, Record};

use crate::copy;
use crate::theme::{self, Paint};

/// One event-panel row: the entry it came from (for [`entry_row_style`])
/// and the plain line(s) `cyclops watch`'s follow mode would print for it
/// ([`Entry::lines`], comfortable density — a message's body hangs its
/// first line under the subject there too).
pub struct EventRow<'a> {
    pub entry: &'a Entry,
    pub lines: Vec<String>,
}

/// The event panel's row-producing path: every entry the record's own
/// calm-view decision admits ([`Record::admits`]), oldest first, each
/// turned into the exact words and glyphs [`cyclops_ui::Entry::lines`]
/// gives any renderer. Pure and backend-neutral — no Ratatui, no color —
/// so [`paint_event_stream`] and the cross-crate parity test read one
/// path instead of two that can drift.
///
/// Nothing here re-sorts, re-filters beyond `admits`, or rewords a line:
/// the model owns the order and the content, this only asks for it. A
/// caller that needs fewer rows than the record holds (the panel's
/// viewport) clips the RESULT, never this call.
pub fn event_stream_rows(record: &Record) -> Vec<EventRow<'_>> {
    let plain = cyclops_ui::Theme::none();
    record
        .entries()
        .filter(|e| record.admits(e))
        .map(|entry| EventRow {
            entry,
            lines: entry.lines(&plain, true),
        })
        .collect()
}

/// Which cyclops-theme token colors one event-panel row. This only picks
/// the token; the glyph and the word are already in the text
/// [`event_stream_rows`] produced, so `NO_COLOR` (`paint.colors_enabled`
/// false, `theme::style_token` folds to no style) leaves the row exactly
/// as legible (rule 11).
fn entry_row_style(kind: &EntryKind, paint: &Paint) -> Style {
    match kind {
        EntryKind::State { state, .. } => paint.state(*state),
        EntryKind::Delivery { state, .. } => paint.delivery(*state),
        EntryKind::Cleared { was, .. } => match was {
            cyclops_proto::AttentionItem::Agent { state, .. } => paint.state(*state),
            cyclops_proto::AttentionItem::Delivery { state, .. } => paint.delivery(*state),
        },
        // The daemon's own ping at a human: the eye's alert token, not a
        // state or badge group.
        EntryKind::Notify { .. } => theme::attention_eye(paint),
        // Plain body rows: chrome text on the panel's own raised ground,
        // never the terminal's foreground, which a themed ground could
        // render unreadable on a light host.
        EntryKind::Msg { .. }
        | EntryKind::Gate { .. }
        | EntryKind::Session { .. }
        | EntryKind::PaneGone { .. }
        | EntryKind::Other { .. } => theme::menu_row(paint),
    }
}

/// Slide-out event panel: the shared `cyclops watch` stream model's
/// admitted rows (E2), clipped to this narrower viewport. See
/// [`event_stream_rows`] for the row content and ordering guarantee, and
/// `crate::app::App::record`'s doc for what feeds it.
pub fn paint_event_stream(record: &Record, area: Rect, buf: &mut Buffer, paint: &Paint) {
    let w = area.width.min(40);
    let panel = Rect::new(
        area.x + area.width.saturating_sub(w),
        area.y,
        w,
        area.height,
    );
    let block = Block::default()
        .borders(Borders::LEFT)
        .title(" Event stream ")
        .border_style(theme::pane_border_focused(paint))
        .style(theme::menu_row(paint));
    let inner = block.inner(panel);
    block.render(panel, buf);

    let rows = event_stream_rows(record);
    if rows.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            copy::EVENT_STREAM_EMPTY,
            theme::sidebar_row(paint),
        )))
        .render(inner, buf);
        return;
    }

    // An admitted row can be several lines (a message's body) and any one
    // of those can be wider than the panel, so "row" and "rendered line"
    // are different units. Capping by row count let one old, wide row's
    // wrap push the newest row's lines past the bottom of the viewport,
    // where `Paragraph` silently drops them (it fills from its first line
    // and stops, so overflow is lost off the end, not the start). The
    // panel's guarantee is the last `inner.height` RENDERED lines, so this
    // wraps to `inner.width` itself and windows the result, rather than
    // asking `Paragraph::wrap` for a word-wrapped line count it does not
    // expose.
    let lines = visible_window(&rows, inner.width as usize, inner.height as usize, paint);
    Paragraph::new(lines).render(inner, buf);
}

/// Wraps `text` at `width` display columns, matching how a narrower
/// terminal would actually break the run of glyphs `text` already is (no
/// re-flow, no trimming — the panel's other rows are pre-formatted text,
/// not prose).
///
/// A char whose width would overflow the open segment starts a new one; a
/// zero-width char (a combining mark) always joins whatever segment is
/// open, even an empty one, so it can never anchor a line by itself.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut segment = String::new();
    let mut segment_width = 0usize;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if w == 0 {
            segment.push(ch);
            continue;
        }
        if segment_width + w > width && !segment.is_empty() {
            out.push(std::mem::take(&mut segment));
            segment_width = 0;
        }
        segment.push(ch);
        segment_width += w;
    }
    out.push(segment);
    out
}

/// The last `height` rendered lines of `rows`, newest last.
///
/// 1. Walk `rows` newest first, wrapping each row's lines at `width` and
///    prepending them, so a record holding thousands of admitted rows only
///    ever wraps the screenful this renders rather than the whole ring.
/// 2. Stop as soon as `height` lines have accumulated, then keep exactly
///    the last `height` of them. A row taller than the panel is trimmed
///    from its own front, the same as the window as a whole, so it shows
///    its tail rather than disappearing under an older row's lines.
///
/// Order matters here: walking oldest first and trimming the same way
/// would keep the OLDEST lines instead, which is the defect this replaces.
fn visible_window(
    rows: &[EventRow<'_>],
    width: usize,
    height: usize,
    paint: &Paint,
) -> Vec<Line<'static>> {
    let mut visible: VecDeque<Line<'static>> = VecDeque::new();
    for row in rows.iter().rev() {
        let style = entry_row_style(&row.entry.kind, paint);
        let mut wrapped = Vec::new();
        for text in &row.lines {
            for segment in wrap_to_width(text, width) {
                wrapped.push(Line::from(Span::styled(segment, style)));
            }
        }
        for line in wrapped.into_iter().rev() {
            visible.push_front(line);
        }
        if visible.len() >= height {
            break;
        }
    }
    while visible.len() > height {
        visible.pop_front();
    }
    visible.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color as RtColor;
    use ratatui::Terminal;

    use super::*;
    use crate::render::test_support::flatten;

    fn state_entry(
        ts: u64,
        target: &str,
        pane_id: &str,
        state: cyclops_proto::AgentState,
    ) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq: None,
            id: None,
            kind: EntryKind::State {
                target: target.into(),
                pane_id: Some(pane_id.into()),
                state,
            },
        }
    }

    /// E2's row-producing path applies exactly one filter — the record's
    /// own calm-view decision — and preserves its order. A routine
    /// `working` transition never reaches the calm view; the
    /// `blocked_permission` one before it does, and stays first.
    #[test]
    fn event_stream_rows_follow_the_records_own_admission_and_order() {
        let mut record = Record::new();
        record.live(state_entry(
            1_000,
            "reviewer",
            "%1",
            cyclops_proto::AgentState::BlockedPermission,
        ));
        record.live(state_entry(
            2_000,
            "codex",
            "%2",
            cyclops_proto::AgentState::Working,
        ));

        let rows = event_stream_rows(&record);
        assert_eq!(rows.len(), 1, "the routine transition must not surface");
        assert_eq!(
            rows[0].lines,
            vec!["00:00:01  reviewer  ⚠ blocked_permission"]
        );
    }

    /// The panel renders the model's own glyph and word — the same
    /// content `cyclops watch` shows for the identical entry — through
    /// Ratatui rather than a private debug-formatted projection.
    ///
    /// A state whose word plus "reviewer" comes in under this backend's
    /// content width: the panel caps its own width at 40 regardless of
    /// the terminal, so the row this fixture renders has to fit inside
    /// that fixed 39-column budget to test the glyph and word rather than
    /// this module's own hard wrap (covered separately, below).
    #[test]
    fn event_panel_renders_the_calm_views_glyph_and_word() {
        let mut record = Record::new();
        record.live(state_entry(
            1_000,
            "reviewer",
            "%1",
            cyclops_proto::AgentState::BlockedModal,
        ));

        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).unwrap();
        let paint = Paint::for_test();
        term.draw(|f| {
            paint_event_stream(&record, f.area(), f.buffer_mut(), &paint);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let text = flatten(buf);
        assert!(text.contains("reviewer"), "{text:?}");
        assert!(text.contains("⚠"), "{text:?}");
        // The expected word comes from the vocabulary's owner, not a
        // literal: if the state's words change, this follows.
        let word = cyclops_proto::AgentState::BlockedModal.to_string();
        assert!(text.contains(&word), "{text:?}");
    }

    /// Rule 11, mechanically: turn color off and read the same lines. The
    /// event panel's rows carry their glyph and word regardless of
    /// `paint.colors_enabled`; only the `Style` painted over them changes.
    /// The plain body row also proves the themed side of the contract:
    /// chrome text on the panel ground, never the terminal's foreground.
    #[test]
    fn event_panel_rows_read_the_same_with_color_off() {
        let mut record = Record::new();
        // A short target keeps the state line inside the panel's 39-column
        // inner width, so each entry is exactly one rendered row.
        record.live(state_entry(
            1_000,
            "rev",
            "%1",
            cyclops_proto::AgentState::BlockedPermission,
        ));
        record.live(Entry {
            uid: 0,
            ts: 2_000,
            seq: None,
            id: None,
            kind: EntryKind::Msg {
                from: "reviewer".into(),
                to: vec!["admin".into()],
                subject: "ping".into(),
                body: None,
                fyi: false,
            },
        });

        let render_with = |paint: &Paint| -> ratatui::buffer::Buffer {
            let backend = TestBackend::new(40, 6);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| {
                paint_event_stream(&record, f.area(), f.buffer_mut(), paint);
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let paint = Paint::for_test();
        let colored = render_with(&paint);
        let plain = render_with(&Paint::without_color_for_test());
        assert_eq!(
            flatten(&colored),
            flatten(&plain),
            "the words and glyphs must not depend on color"
        );
        // Column 0 is the panel's left border and row 0 its title; the
        // state entry renders on row 1, the message on row 2.
        assert_ne!(
            colored[(1, 1)].fg,
            RtColor::Reset,
            "color on must actually paint the state row"
        );
        assert_eq!(
            colored[(1, 2)].fg,
            theme::menu_row(&paint).fg.expect("chrome text color"),
            "a plain body row wears chrome text, not the terminal's own"
        );
        assert_eq!(
            plain[(1, 1)].fg,
            RtColor::Reset,
            "NO_COLOR must leave no color behind"
        );
        assert_eq!(
            plain[(1, 2)].fg,
            RtColor::Reset,
            "NO_COLOR must leave the plain row uncolored too"
        );
    }

    /// The defect this module exists to fix: an older row's target is long
    /// enough that its own wrapped lines alone exceed the panel's height,
    /// so with only entries (not rendered lines) capped, Ratatui's `Wrap`
    /// renders from the first line onward and never reaches the newer
    /// row. The oldest lines survive; the newest entry is cut off.
    #[test]
    fn newest_entrys_lines_survive_when_an_older_entry_wraps_past_the_window() {
        let mut record = Record::new();
        record.live(state_entry(
            1_000,
            &"a".repeat(200),
            "%1",
            cyclops_proto::AgentState::BlockedModal,
        ));
        record.live(state_entry(
            2_000,
            "z",
            "%2",
            cyclops_proto::AgentState::BlockedQuota,
        ));

        let backend = TestBackend::new(31, 3);
        let mut term = Terminal::new(backend).unwrap();
        let paint = Paint::for_test();
        term.draw(|f| {
            paint_event_stream(&record, f.area(), f.buffer_mut(), &paint);
        })
        .unwrap();
        let text = flatten(term.backend().buffer());

        let newest_word = cyclops_proto::AgentState::BlockedQuota.to_string();
        assert!(text.contains(&newest_word), "{text:?}");
    }

    /// A single entry taller than the panel keeps bottom-aligned
    /// semantics: its own trailing lines are what the reader sees, never
    /// its opening ones.
    ///
    /// The target's length is picked so the row's word lands exactly on a
    /// wrapped line's start and fits inside one: `flatten` reads the
    /// panel's left border once per row, so a word straddling two wrapped
    /// lines would read back with that border cell spliced into the
    /// middle of it, which is a property of the test's own flattening and
    /// not of the panel.
    #[test]
    fn a_newest_entry_taller_than_the_panel_shows_its_own_tail() {
        let mut record = Record::new();
        let target = format!("head{}", "m".repeat(302));
        record.live(state_entry(
            1_000,
            &target,
            "%1",
            cyclops_proto::AgentState::BlockedPermission,
        ));

        let backend = TestBackend::new(21, 6);
        let mut term = Terminal::new(backend).unwrap();
        let paint = Paint::for_test();
        term.draw(|f| {
            paint_event_stream(&record, f.area(), f.buffer_mut(), &paint);
        })
        .unwrap();
        let text = flatten(term.backend().buffer());

        assert!(!text.contains("head"), "{text:?}");
        let word = cyclops_proto::AgentState::BlockedPermission.to_string();
        assert!(text.contains(&word), "{text:?}");
    }
}
