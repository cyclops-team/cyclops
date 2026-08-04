//! Paint pane grids into a Ratatui buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::layout::layout_pane_slots;
use crate::copy;
use crate::dialog::Dialog;
use crate::model::{PaneSlot, RuntimeRegistry, TabModel, WorkspaceRow};
use crate::runtime::{CellGrid, Color, GridCell};
use crate::theme::{self, Paint};

/// Paint a modal dialog centered in `area`.
pub fn paint_dialog(dialog: &Dialog, area: Rect, buf: &mut Buffer, paint: &Paint) {
    let w = area.width.min(60);
    let h = 3u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let dialog_area = Rect::new(x, y, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::pane_border_focused(paint))
        .style(theme::pane_cell(paint));
    let inner = block.inner(dialog_area);
    block.render(dialog_area, buf);
    let text = match dialog {
        Dialog::ConfirmClosePane { .. } => copy::CONFIRM_CLOSE_PANE.to_string(),
        Dialog::RenameTab { buffer } => format!("{}{}", copy::RENAME_TAB_PROMPT, buffer),
        Dialog::NewWorkspace { buffer } => format!("{}{}", copy::NEW_WORKSPACE_PROMPT, buffer),
        Dialog::RenameWorkspace { buffer } => {
            format!("{}{}", copy::RENAME_WORKSPACE_PROMPT, buffer)
        }
        Dialog::ConfirmCloseWorkspace => copy::CONFIRM_CLOSE_WORKSPACE.to_string(),
    };
    Paragraph::new(text).render(inner, buf);
}

/// Blit a runtime grid into `area` of `buf`.
pub fn paint_pane(grid: &CellGrid, area: Rect, buf: &mut Buffer, paint: &Paint) {
    let base = theme::pane_cell(paint);
    for row in 0..area.height {
        for col in 0..area.width {
            let cell = grid.cell(col, row).cloned().unwrap_or_default();
            let style = cell_style(&cell, base);
            let ch = if cell.wide_spacer || cell.ch == '\0' {
                ' '
            } else {
                cell.ch
            };
            let x = area.x + col;
            let y = area.y + row;
            if let Some(dst) = buf.cell_mut((x, y)) {
                dst.set_symbol(&ch.to_string());
                dst.set_style(style);
            }
        }
    }
}

/// Render the workspace sidebar.
pub fn paint_sidebar(
    workspaces: &[WorkspaceRow],
    active: usize,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
) {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        " Workspaces",
        theme::sidebar_label(paint),
    )));
    for (i, ws) in workspaces.iter().enumerate() {
        let marker = if i == active { "▸" } else { " " };
        let style = if i == active {
            theme::sidebar_row_active(paint)
        } else {
            theme::sidebar_row(paint)
        };
        let label = format!("{marker} {} ({})", ws.name, ws.tab_count);
        lines.push(Line::from(Span::styled(label, style)));
    }
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(theme::pane_border(paint));
    let inner = block.inner(area);
    block.render(area, buf);
    let para = Paragraph::new(lines);
    para.render(inner, buf);
}

/// Render the tab bar for the active session.
pub fn paint_tab_bar(
    tabs: &[TabModel],
    active: usize,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
) {
    let mut spans = Vec::new();
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if i == active {
            theme::tab_active(paint)
        } else {
            theme::tab_inactive(paint)
        };
        spans.push(Span::styled(format!(" {} ", tab.name), style));
    }
    spans.push(Span::raw(" +"));
    let line = Line::from(spans);
    Paragraph::new(line).render(area, buf);
}

/// Render every pane of the active window with borders.
pub fn paint_window(
    tab: &TabModel,
    runtimes: &RuntimeRegistry,
    canvas: Rect,
    buf: &mut Buffer,
    paint: &Paint,
) {
    let slots = layout_pane_slots(&tab.layout, canvas, &tab.active_pane);
    for slot in slots {
        paint_pane_slot(&slot, runtimes, buf, paint);
    }
}

fn paint_pane_slot(slot: &PaneSlot, runtimes: &RuntimeRegistry, buf: &mut Buffer, paint: &Paint) {
    if slot.rect.width == 0 || slot.rect.height == 0 {
        return;
    }
    let border_style = if slot.focused {
        theme::pane_border_focused(paint)
    } else {
        theme::pane_border(paint)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(format!(" {} ", slot.pane_id));
    let inner = block.inner(slot.rect);
    block.render(slot.rect, buf);
    if let Some(runtime) = runtimes.get(&slot.pane_id) {
        let grid = runtime.grid();
        paint_pane(grid.grid, inner, buf, paint);
    }
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
    if cell.attrs.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.attrs.reverse {
        style = style.add_modifier(Modifier::REVERSED);
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::dialog::Dialog;
    use crate::layout::{parse_layout, resolve_layout};
    use crate::model::{RuntimeRegistry, TabModel, WorkspaceRow};
    use crate::runtime::{CellAttrs, GridCell};
    use crate::theme::Paint;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn two_pane_tab() -> TabModel {
        let node = parse_layout("4c3e,319x89,0,0[319x44,0,0,0,319x44,0,45,1]").unwrap();
        let map = HashMap::from([(0, "%0".to_string()), (1, "%1".to_string())]);
        let layout = resolve_layout(&node, &map).unwrap();
        TabModel {
            window_id: "@0".to_string(),
            index: 0,
            name: "main".to_string(),
            layout,
            active_pane: "%0".to_string(),
            zoomed: false,
        }
    }

    #[test]
    fn pane_cells_paint_on_test_backend() {
        let mut grid = CellGrid {
            cols: 5,
            rows: 2,
            cells: vec![GridCell::default(); 10],
        };
        grid.cells[0] = GridCell {
            ch: 'X',
            wide_spacer: false,
            attrs: CellAttrs::default(),
        };
        let backend = TestBackend::new(5, 2);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            paint_pane(&grid, f.area(), f.buffer_mut(), &theme);
        })
        .unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "X");
    }

    #[test]
    fn multi_pane_borders_and_tab_bar_render() {
        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();

        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let area = f.area();
            let tab_area = Rect::new(area.x, area.y, area.width, 1);
            let canvas = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
            paint_tab_bar(std::slice::from_ref(&tab), 0, tab_area, f.buffer_mut(), &theme);
            paint_window(&tab, &runtimes, canvas, f.buffer_mut(), &theme);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row0: String = (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row0.contains("main"),
            "active tab label should render: {row0}"
        );
        let mut saw_border = false;
        for y in 1..buf.area.height {
            for x in 0..buf.area.width {
                if matches!(
                    buf[(x, y)].symbol(),
                    "│" | "─" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼"
                ) {
                    saw_border = true;
                }
            }
        }
        assert!(saw_border, "pane borders should render");
    }

    #[test]
    fn sidebar_rows_render_with_selection() {
        let workspaces = vec![
            WorkspaceRow {
                name: "cyclops".into(),
                tab_count: 2,
                active: true,
            },
            WorkspaceRow {
                name: "website".into(),
                tab_count: 1,
                active: false,
            },
        ];
        let backend = TestBackend::new(20, 6);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            paint_sidebar(&workspaces, 0, f.area(), f.buffer_mut(), &theme);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let flat: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(flat.contains("cyclops"), "sidebar should list workspace: {flat}");
        assert!(flat.contains('▸'), "active row should be marked");
    }

    #[test]
    fn new_workspace_dialog_renders() {
        let backend = TestBackend::new(50, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let dialog = Dialog::NewWorkspace {
            buffer: "/tmp/proj".into(),
        };
        term.draw(|f| paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme))
            .unwrap();
        let buf = term.backend().buffer();
        let flat: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(flat.contains("New workspace folder"));
    }

    #[test]
    fn confirm_close_dialog_renders() {
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let dialog = Dialog::confirm_close("%0");
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let flat: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(flat.contains("Close this pane"));
    }

    #[test]
    fn focused_pane_uses_accent_border() {
        let slots = layout_pane_slots(&two_pane_tab().layout, Rect::new(0, 0, 40, 10), "%0");
        assert_eq!(slots.len(), 2);
        assert!(slots[0].focused);
        assert!(!slots[1].focused);
    }
}
