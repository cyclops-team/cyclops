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
    hover: Option<(u16, u16)>,
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
    let mut plus_hovered = false;
    if x < right {
        let rect = Rect::new(
            x,
            area.y,
            (plus.len() as u16).min(right - x),
            area.height.max(1),
        );
        // The `+` lights under the mouse like every other chrome control
        // (render/mod.rs rule 1). Without this the strip's own button was
        // the one control in the language that never answered the pointer.
        plus_hovered = hover.is_some_and(|(col, row)| {
            col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
        });
        hits.push(rect, HitTarget::NewTabButton);
    }
    let plus_style = if plus_hovered {
        theme::add_button_hover(paint)
    } else {
        theme::add_button(paint)
    };
    spans.push(Span::styled(plus, plus_style));

    // The `+` is the last thing on this strip. A compose button used to
    // follow it, as the mouse's half of Ctrl+B @, from before the sidebar
    // footer carried one. Two copies of one control on screen at once is
    // one too many, and the footer's is the one that belongs: the roster it
    // writes to sits directly above it, where this strip is a list of tabs
    // and has nothing to do with addressing an agent. The chord and the
    // footer button both still reach the composer.
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
                None,
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

    /// The strip carries no compose button, and the `+` is still a real
    /// control on it.
    ///
    /// The compose button used to sit here as the mouse's half of Ctrl+B @,
    /// from before the sidebar footer carried one. Two of them on screen at
    /// once is one too many. This pins the removal rather than merely
    /// dropping the old test, because the strip is where someone reaching
    /// for "a visible route to the composer" would put one back.
    ///
    /// The `+` half is what the old test was really guarding: rule 1 in
    /// render/mod.rs promises every chrome control lights under the
    /// pointer, and the strip's own button once shipped taking no hover at
    /// all. That half is kept.
    #[test]
    fn the_strip_ends_at_the_plus_and_that_plus_answers_the_pointer() {
        let paint_at = |hover: Option<(u16, u16)>| {
            let mut term = Terminal::new(TestBackend::new(40, 2)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_tab_bar(
                    &[two_pane_tab()],
                    0,
                    Rect::new(0, 0, 40, 1),
                    f.buffer_mut(),
                    &Paint::for_test(),
                    &mut hits,
                    &DecorationSnapshot::default(),
                    hover,
                );
            })
            .unwrap();
            (term.backend().buffer().clone(), hits)
        };

        let (cold, hits) = paint_at(None);
        let plus = (0..40)
            .find(|x| matches!(hits.hit(*x, 0), Some(HitTarget::NewTabButton)))
            .expect("the + is on the strip");

        assert!(
            (0..40).all(|x| !matches!(hits.hit(x, 0), Some(HitTarget::ComposeButton))),
            "the strip must not carry a second compose button"
        );
        let painted: String = (0..40).map(|x| cold[(x, 0)].symbol()).collect();
        assert!(
            !painted.contains('@'),
            "and must not paint its sigil either: {painted:?}"
        );

        let (lit, _) = paint_at(Some((plus, 0)));
        assert_ne!(
            cold[(plus, 0)].bg,
            lit[(plus, 0)].bg,
            "the + did not light under the pointer"
        );
        let (elsewhere, _) = paint_at(Some((0, 0)));
        assert_eq!(cold[(plus, 0)].bg, elsewhere[(plus, 0)].bg);
    }

    #[test]
    fn the_add_button_lights_under_the_mouse() {
        let paint_at = |hover: Option<(u16, u16)>| {
            let mut term = Terminal::new(TestBackend::new(40, 2)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_tab_bar(
                    &[two_pane_tab()],
                    0,
                    Rect::new(0, 0, 40, 1),
                    f.buffer_mut(),
                    &Paint::for_test(),
                    &mut hits,
                    &DecorationSnapshot::default(),
                    hover,
                );
            })
            .unwrap();
            (term.backend().buffer().clone(), hits)
        };

        let (cold, hits) = paint_at(None);
        let plus = (0..40)
            .find(|x| matches!(hits.hit(*x, 0), Some(HitTarget::NewTabButton)))
            .expect("the strip paints a + button");
        let (lit, _) = paint_at(Some((plus, 0)));
        assert_ne!(
            cold[(plus, 0)].bg,
            lit[(plus, 0)].bg,
            "hovering the + has to change the cell under the pointer"
        );

        // And only under the pointer: a hover somewhere else on the strip
        // must leave the button alone.
        let (elsewhere, _) = paint_at(Some((1, 0)));
        assert_eq!(cold[(plus, 0)].bg, elsewhere[(plus, 0)].bg);

        // The hover above is only reachable if the event loop delivers the
        // motion that drives it. `AppMsg::Mouse` drops bare `Moved` unless
        // `motion_touches_hover_button` admits it, and the button was
        // missing from that list, so the strip painted a state no pointer
        // could ever reach. Both edges: arriving lights it, leaving puts
        // it out.
        use crate::input::mouse::motion_touches_hover_button;
        assert!(
            motion_touches_hover_button(&hits, None, plus, 0),
            "arriving on the + must wake the renderer"
        );
        assert!(
            motion_touches_hover_button(&hits, Some((plus, 0)), 1, 0),
            "leaving the + must wake the renderer too, or it stays lit"
        );
    }
}
