//! The tab bar: a full-width chrome strip listing the active session's
//! tabs. Owns only tab-strip layout and its hit regions; tab actions and
//! reordering are resolved by `action`/`app`, not here.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::decoration::DecorationSnapshot;
use crate::input::mouse::{HitMap, HitTarget};
use crate::model::TabModel;
use crate::theme::{self, Paint};

/// Render the tab bar: a full-width chrome strip with the active tab as a
/// raised chip. Hit regions come from the measured span widths, so clicks
/// land on the tab actually painted there.
pub fn paint_tab_bar(
    tabs: &[TabModel],
    active: usize,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    decoration: &DecorationSnapshot,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The strip's own ground runs the whole width, separating the bar
    // from the canvas below it.
    buf.set_style(area, theme::chrome_panel(paint));
    let mut spans = vec![Span::styled(" ", theme::chrome_panel(paint))];
    let mut x = area.x + 1;
    let right = area.x + area.width;
    for (i, tab) in tabs.iter().enumerate() {
        let attn = if decoration.tab_needs_attention(&tab.window_id) {
            " ◉"
        } else {
            ""
        };
        let style = if i == active {
            theme::tab_active(paint)
        } else {
            theme::tab_inactive(paint)
        };
        let label = format!(" {}{} ", tab.name, attn);
        let w = Span::raw(label.as_str()).width() as u16;
        if x < right {
            hits.push(
                Rect::new(x, area.y, w.min(right - x), area.height.max(1)),
                HitTarget::Tab {
                    window_id: tab.window_id.clone(),
                },
            );
        }
        spans.push(Span::styled(label, style));
        spans.push(Span::styled(" ", theme::chrome_panel(paint)));
        x = x.saturating_add(w + 1);
    }
    let plus = " + ";
    if x < right {
        hits.push(
            Rect::new(
                x,
                area.y,
                (plus.len() as u16).min(right - x),
                area.height.max(1),
            ),
            HitTarget::NewTabButton,
        );
    }
    spans.push(Span::styled(plus, theme::add_button(paint)));
    Paragraph::new(Line::from(spans)).render(area, buf);
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::render::test_support::two_pane_tab;

    #[test]
    fn tab_hit_regions_match_label_widths() {
        let mut tabs = vec![two_pane_tab(), two_pane_tab()];
        tabs[1].window_id = "@1".into();
        tabs[1].name = "logs".into();
        let backend = TestBackend::new(40, 2);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_tab_bar(
                &tabs,
                0,
                Rect::new(0, 0, 40, 1),
                f.buffer_mut(),
                &theme,
                &mut hits,
                &DecorationSnapshot::default(),
            );
        })
        .unwrap();
        // " main " is 6 wide, then a 1-cell separator, then " logs ".
        assert!(matches!(
            hits.hit(1, 0),
            Some(HitTarget::Tab { window_id }) if window_id == "@0"
        ));
        assert!(matches!(
            hits.hit(8, 0),
            Some(HitTarget::Tab { window_id }) if window_id == "@1"
        ));
        let buf = term.backend().buffer();
        assert_ne!(
            buf[(1, 0)].bg,
            buf[(8, 0)].bg,
            "the selected tab needs a materially stronger fill"
        );
        // After both labels comes the + button.
        assert!(matches!(hits.hit(15, 0), Some(HitTarget::NewTabButton)));
        assert_ne!(
            buf[(15, 0)].bg,
            buf[(8, 0)].bg,
            "the add button should read as part of the tab strip"
        );
    }
}
