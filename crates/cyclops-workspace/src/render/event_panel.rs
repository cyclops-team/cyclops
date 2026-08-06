//! The slide-out event panel: the shared `cyclops watch` stream model's
//! admitted rows (E2), clipped to this narrower viewport. Row content,
//! order, and filtering come from `cyclops_ui::Record`; this only turns
//! its rows into a painted, backend-neutral text panel.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

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
        EntryKind::Msg { .. }
        | EntryKind::Gate { .. }
        | EntryKind::Session { .. }
        | EntryKind::PaneGone { .. }
        | EntryKind::Other { .. } => theme::sidebar_row(paint),
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

    // The viewport clip the spec allows: the tail the panel can plausibly
    // show, oldest-of-that-window first. Ratatui's own `Wrap` handles the
    // narrower-width wrapping; this only bounds how many entries it gets.
    let cap = (inner.height as usize).max(1);
    let visible = &rows[rows.len().saturating_sub(cap)..];
    let mut lines: Vec<Line> = Vec::new();
    for row in visible {
        let style = entry_row_style(&row.entry.kind, paint);
        for text in &row.lines {
            lines.push(Line::from(Span::styled(text.clone(), style)));
        }
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, buf);
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
    #[test]
    fn event_panel_renders_the_calm_views_glyph_and_word() {
        let mut record = Record::new();
        record.live(state_entry(
            1_000,
            "reviewer",
            "%1",
            cyclops_proto::AgentState::BlockedPermission,
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
        let word = cyclops_proto::AgentState::BlockedPermission.to_string();
        assert!(text.contains(&word), "{text:?}");
    }

    /// Rule 11, mechanically: turn color off and read the same line. The
    /// event panel's rows carry their glyph and word regardless of
    /// `paint.colors_enabled`; only the `Style` painted over them changes.
    #[test]
    fn event_panel_rows_read_the_same_with_color_off() {
        let mut record = Record::new();
        record.live(state_entry(
            1_000,
            "reviewer",
            "%1",
            cyclops_proto::AgentState::BlockedPermission,
        ));

        let render_with = |paint: &Paint| -> ratatui::buffer::Buffer {
            let backend = TestBackend::new(40, 6);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| {
                paint_event_stream(&record, f.area(), f.buffer_mut(), paint);
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let colored = render_with(&Paint::for_test());
        let plain = render_with(&Paint::without_color_for_test());
        assert_eq!(
            flatten(&colored),
            flatten(&plain),
            "the words and glyphs must not depend on color"
        );
        // Column 0 is the panel's left border; the row text itself starts
        // one cell in.
        assert_ne!(
            colored[(1, 0)].fg,
            RtColor::Reset,
            "color on must actually paint the row"
        );
        assert_eq!(
            plain[(1, 0)].fg,
            RtColor::Reset,
            "NO_COLOR must leave no color behind"
        );
    }
}
