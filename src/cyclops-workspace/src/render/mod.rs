//! Frame composition and render-derived hit geometry: painting panes and
//! chrome into a Ratatui buffer, and the top-level chrome layout every
//! surface below is painted into.
//!
//! Panes render at their tmux cell coordinates 1:1. The workspace subtracts
//! the extra cells used by its separator bands before declaring
//! the client size, then restores those cells only as chrome. Nothing scales;
//! a runtime grid lands on exactly the cells tmux gave the pane.
//!
//! This module does not own persistence, daemon queries, or attention
//! predicates — it reads whatever state its callers hand it and paints or
//! measures, nothing more. Each surface below has clear seams (sidebar,
//! pane canvas, tab bar, dialogs/menus, event panel) and lives in its own
//! file; this file owns only what those surfaces share: the top-level
//! chrome split (`chrome_areas_for`), the cell-to-style bridge
//! (`cell_style`/`rt_color`), and the one text primitive
//! (`overlay_text`) every surface paints through.

#![allow(clippy::too_many_arguments)]

mod canvas;
mod event_panel;
mod overlay;
mod sidebar;
mod tab_bar;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};

use crate::drag::{DragState, DragTarget};
use crate::runtime::{Color, GridCell};

pub use canvas::{paint_window, tmux_client_size, HostCursor, WindowPaintCtx};
pub use event_panel::{event_stream_rows, paint_event_stream, EventRow};
pub use overlay::{keybind_max_scroll, paint_dialog, paint_menu};
pub use sidebar::paint_sidebar;
pub use tab_bar::paint_tab_bar;

/// A chrome region's height in the tab bar. It never grows: the row is a
/// strip, not a panel.
const TAB_BAR_HEIGHT: u16 = 1;
/// Fixed width of the slide-out event panel.
const EVENT_STREAM_WIDTH: u16 = 40;
/// Narrowest a readable sidebar can be: below this, workspace and agent
/// names truncate into noise.
pub(crate) const SIDEBAR_MIN_WIDTH: u16 = 22;
/// Widest a sidebar may grow before it starts crowding the pane canvas it
/// exists to introduce.
const SIDEBAR_MAX_WIDTH: u16 = 42;

/// Chrome rectangles for one frame.
pub struct ChromeAreas {
    pub sidebar: Option<Rect>,
    pub panel: Option<Rect>,
    pub tab_bar: Rect,
    pub canvas: Rect,
}

/// Split one frame into the sidebar, event panel, tab bar, and pane canvas
/// — the top-level chrome composition every painted surface below sits
/// inside. `app` decides visibility and width; this only turns those
/// decisions into rectangles.
pub fn chrome_areas_for(
    area: Rect,
    sidebar_visible: bool,
    sidebar_width: u16,
    panel_open: bool,
) -> ChromeAreas {
    let mut main = area;
    let sidebar = if sidebar_visible && main.width > 4 {
        let w = clamp_sidebar_width(sidebar_width, main.width);
        let s = Rect::new(main.x, main.y, w, main.height);
        main = Rect::new(main.x + w, main.y, main.width - w, main.height);
        Some(s)
    } else {
        None
    };
    let panel = if panel_open && main.width > EVENT_STREAM_WIDTH + 4 {
        let p = Rect::new(
            main.x + main.width - EVENT_STREAM_WIDTH,
            main.y,
            EVENT_STREAM_WIDTH,
            main.height,
        );
        main = Rect::new(main.x, main.y, main.width - EVENT_STREAM_WIDTH, main.height);
        Some(p)
    } else {
        None
    };
    let bar_h = TAB_BAR_HEIGHT.min(main.height);
    let tab_bar = Rect::new(main.x, main.y, main.width, bar_h);
    let canvas = Rect::new(
        main.x,
        main.y + bar_h,
        main.width,
        main.height.saturating_sub(bar_h),
    );
    ChromeAreas {
        sidebar,
        panel,
        tab_bar,
        canvas,
    }
}

/// Bound a requested sidebar width to what stays readable without eating
/// more than half the terminal.
pub fn clamp_sidebar_width(requested: u16, terminal_width: u16) -> u16 {
    let max = SIDEBAR_MAX_WIDTH.min(terminal_width / 2).max(1);
    let min = SIDEBAR_MIN_WIDTH.min(max);
    requested.clamp(min, max)
}

/// The sidebar width a live drag to `column` would commit, bounded the same
/// way a resting preference is.
pub fn sidebar_width_for_column(column: u16, terminal_width: u16) -> u16 {
    clamp_sidebar_width(column.saturating_add(1), terminal_width)
}

/// The width to restore when a sidebar-resize drag is cancelled: `None` for
/// every other drag target, which has nothing here to restore.
pub fn sidebar_width_on_cancel(drag: &DragState, terminal_width: u16) -> Option<u16> {
    matches!(&drag.target, DragTarget::Sidebar)
        .then(|| sidebar_width_for_column(drag.start.0, terminal_width))
}

/// Write `text` onto one row, clipped to `bounds`.
fn overlay_text(buf: &mut Buffer, bounds: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y < bounds.y || y >= bounds.y + bounds.height || x < bounds.x || x >= bounds.x + bounds.width
    {
        return;
    }
    let width = (bounds.x + bounds.width - x) as usize;
    buf.set_stringn(x, y, text, width, style);
}

fn cell_style(cell: &GridCell, base: Style) -> Style {
    let mut style = base;
    if let Some(fg) = rt_color(cell.attrs.fg) {
        style = style.fg(fg);
    }
    if let Some(bg) = rt_color(cell.attrs.bg) {
        style = style.bg(bg);
    }
    if cell.attrs.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.attrs.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.attrs.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.attrs.underline.is_underlined() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.attrs.reverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if cell.attrs.strikeout {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if cell.attrs.hidden {
        style = style.add_modifier(Modifier::HIDDEN);
    }
    style
}

fn rt_color(c: Color) -> Option<RtColor> {
    match c {
        Color::Default => None,
        Color::Indexed(i) => Some(RtColor::Indexed(i)),
        Color::Rgb(r, g, b) => Some(RtColor::Rgb(r, g, b)),
    }
}

/// Test fixtures shared by more than one surface's test module. Kept here,
/// rather than duplicated, because both `canvas` and `sidebar` (the two
/// glyph-stability tests) and both `canvas` and `tab_bar` (the two-pane
/// frame fixture) exercise the identical setup.
#[cfg(test)]
pub(crate) mod test_support {
    use ratatui::buffer::Buffer;

    use crate::layout::{parse_layout, resolve_layout};
    use crate::model::TabModel;
    use crate::theme::Paint;

    /// Two stacked panes whose tmux grid plus compact divider fills the
    /// 38x9 pane canvas used by the frame tests.
    pub(crate) fn two_pane_tab() -> TabModel {
        let node = parse_layout("4c3e,38x8,0,0[38x4,0,0,0,38x3,0,5,1]").unwrap();
        let layout = resolve_layout(&node, &[]).unwrap();
        TabModel {
            window_id: "@0".to_string(),
            name: "main".to_string(),
            layout,
            active_pane: "%0".to_string(),
            zoomed: false,
        }
    }

    pub(crate) fn flatten(buf: &Buffer) -> String {
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect()
    }

    /// A theme deliberately unlike the default on every token the compact
    /// state cell can paint through — both `[state]` (idle/working/dead)
    /// and `[eye]` (the attention glyph) — so a color match against the
    /// default theme in a caller's test would mean its glyph check was
    /// vacuous. Shared by the two glyph-stability tests below.
    pub(crate) fn alt_test_theme_paint() -> Paint {
        let (theme, warnings) = cyclops_theme::Theme::parse(
            "name = \"alt-test\"\n\
             [state]\n\
             healthy = \"#123456\"\n\
             quiet = \"#234567\"\n\
             dead = \"#345678\"\n\
             [eye]\n\
             alert = \"#456789\"\n",
            "alt-test",
        )
        .expect("valid test theme");
        assert!(
            warnings.is_empty(),
            "unexpected theme warnings: {warnings:?}"
        );
        let mut paint = Paint::for_test();
        paint.theme = theme;
        paint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_resize_is_bounded_by_readability_and_half_the_terminal() {
        assert_eq!(clamp_sidebar_width(1, 200), SIDEBAR_MIN_WIDTH);
        assert_eq!(clamp_sidebar_width(100, 200), SIDEBAR_MAX_WIDTH);
        assert_eq!(clamp_sidebar_width(100, 50), 25);
        assert_eq!(sidebar_width_for_column(30, 50), 25);
    }

    #[test]
    fn chrome_canvas_excludes_sidebar_and_tab_bar() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), true, 22, false);
        assert_eq!(areas.sidebar, Some(Rect::new(0, 0, 22, 50)));
        assert_eq!(areas.tab_bar, Rect::new(22, 0, 178, 1));
        assert_eq!(areas.canvas, Rect::new(22, 1, 178, 49));
        assert_eq!(areas.panel, None);
    }

    #[test]
    fn chrome_canvas_shrinks_for_event_stream() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), true, 22, true);
        assert_eq!(areas.panel, Some(Rect::new(160, 0, 40, 50)));
        assert_eq!(areas.canvas, Rect::new(22, 1, 138, 49));
    }

    #[test]
    fn cancelling_a_sidebar_drag_restores_its_starting_width() {
        let mut drag = DragState::on_down(DragTarget::Sidebar, 27, 5);
        drag.on_move(38, 5);
        assert_eq!(sidebar_width_on_cancel(&drag, 100), Some(28));

        let tab = DragState::on_down(
            DragTarget::Tab {
                window_id: "@0".into(),
            },
            27,
            5,
        );
        assert_eq!(sidebar_width_on_cancel(&tab, 100), None);
    }
}
