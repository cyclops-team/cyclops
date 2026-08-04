//! Paint pane grids into a Ratatui buffer.

#![allow(clippy::too_many_arguments, dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::copy;
use crate::decoration::DecorationSnapshot;
use crate::dialog::Dialog;
use crate::drag::{DragState, DragTarget};
use crate::input::mouse::{HitMap, HitTarget, PaneGeometry};
use crate::layout::{layout_pane_slots, SplitDir};
use crate::model::{PaneSlot, RuntimeRegistry, TabModel, WorkspaceRow};
use crate::resilience::LinkState;
use crate::runtime::{CellGrid, Color, GridCell};
use crate::selection::Selection;
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
    hits: &mut HitMap,
    decoration: &DecorationSnapshot,
) {
    let mut lines = Vec::new();
    let eye = if decoration.workspace_needs_attention() {
        " ◉"
    } else {
        ""
    };
    lines.push(Line::from(vec![
        Span::styled(" Workspaces", theme::sidebar_label(paint)),
        Span::styled(eye, theme::attention_eye(paint)),
    ]));
    if !decoration.online {
        lines.push(Line::from(Span::styled(
            " cyclopsd offline",
            theme::sidebar_row(paint),
        )));
    }
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
    let row_h = inner.height.saturating_sub(1) / workspaces.len().max(1) as u16;
    for (i, _) in workspaces.iter().enumerate() {
        let y = inner.y + 1 + (i as u16) * row_h;
        if y < inner.y + inner.height {
            hits.push(
                Rect::new(inner.x, y, inner.width, row_h.max(1)),
                HitTarget::SidebarRow { index: i },
            );
        }
    }
    hits.push(
        Rect::new(
            area.x,
            area.y + area.height.saturating_sub(1),
            area.width,
            1,
        ),
        HitTarget::AppMenu,
    );
}

/// Render the tab bar for the active session.
pub fn paint_tab_bar(
    tabs: &[TabModel],
    active: usize,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    decoration: &DecorationSnapshot,
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
        let attn = if decoration.tab_needs_attention(&tab.window_id) {
            " ◉"
        } else {
            ""
        };
        spans.push(Span::styled(format!(" {} {attn}", tab.name), style));
    }
    spans.push(Span::raw(" +"));
    let line = Line::from(spans);
    Paragraph::new(line).render(area, buf);
    let tab_w = (area.width / tabs.len().max(1) as u16).max(4);
    for (i, _) in tabs.iter().enumerate() {
        hits.push(
            Rect::new(area.x + (i as u16) * tab_w, area.y, tab_w, area.height),
            HitTarget::Tab { index: i },
        );
    }
}

/// Chrome state passed through the window paint pass.
pub struct WindowPaintCtx<'a> {
    pub link: LinkState,
    pub paused: &'a std::collections::HashSet<String>,
    pub hits: &'a mut HitMap,
    pub decoration: &'a DecorationSnapshot,
    pub selection: Option<&'a Selection>,
    pub drag: Option<&'a DragState>,
}

/// Render every pane of the active window with borders.
pub fn paint_window(
    tab: &TabModel,
    runtimes: &RuntimeRegistry,
    canvas: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    let slots = layout_pane_slots(&tab.layout, canvas, &tab.active_pane);
    record_divider_hits(&slots, ctx.hits);
    for slot in &slots {
        paint_pane_slot(slot, runtimes, buf, paint, ctx);
    }
    if let Some(drag) = ctx.drag.filter(|d| d.is_active()) {
        paint_drag_preview(drag, buf, paint);
    }
}

fn paint_pane_slot(
    slot: &PaneSlot,
    runtimes: &RuntimeRegistry,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    if slot.rect.width == 0 || slot.rect.height == 0 {
        return;
    }
    let mut border_style = if slot.focused {
        theme::pane_border_focused(paint)
    } else {
        theme::pane_border(paint)
    };
    if matches!(ctx.link, LinkState::Reconnecting { .. }) {
        border_style = border_style.add_modifier(Modifier::DIM);
    }
    let dec = ctx.decoration.pane(&slot.pane_id);
    let badge = dec
        .map(|d| DecorationSnapshot::state_badge(d.state))
        .unwrap_or_default();
    let attn = dec
        .filter(|d| d.needs_attention)
        .map(|_| " ◉")
        .unwrap_or_default();
    let mut title = format!(" {} · {badge}{attn} ", slot.pane_id);
    if matches!(ctx.link, LinkState::Reconnecting { .. }) {
        title = format!(" {} · {} ", slot.pane_id, copy::RECONNECTING_NOTE);
    } else if ctx.paused.contains(&slot.pane_id) {
        title = format!(" {} · {} ", slot.pane_id, copy::PAUSED_NOTE);
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let inner = block.inner(slot.rect);
    block.render(slot.rect, buf);
    ctx.hits.push(
        Rect::new(slot.rect.x, slot.rect.y, slot.rect.width, 1),
        HitTarget::PaneBorder {
            pane_id: slot.pane_id.clone(),
        },
    );
    if dec.is_some_and(|d| d.needs_attention) {
        ctx.hits.push(
            Rect::new(slot.rect.x + 1, slot.rect.y, 3, 1),
            HitTarget::AttentionIndicator {
                pane_id: slot.pane_id.clone(),
            },
        );
    }
    ctx.hits.push(
        inner,
        HitTarget::PaneBody {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push_geometry(PaneGeometry {
        pane_id: slot.pane_id.clone(),
        inner,
        cols: inner.width,
        rows: inner.height,
    });
    if inner.width >= 4 && inner.height >= 1 {
        let ctrl = Rect::new(inner.x + inner.width.saturating_sub(3), inner.y, 3, 1);
        ctx.hits.push(
            ctrl,
            HitTarget::PaneSplitRight {
                pane_id: slot.pane_id.clone(),
            },
        );
        ctx.hits.push(
            Rect::new(ctrl.x.saturating_sub(2), inner.y, 2, 1),
            HitTarget::PaneSplitDown {
                pane_id: slot.pane_id.clone(),
            },
        );
    }
    if let Some(runtime) = runtimes.get(&slot.pane_id) {
        let grid = runtime.grid();
        let mut base = theme::pane_cell(paint);
        if matches!(ctx.link, LinkState::Reconnecting { .. }) {
            base = base.add_modifier(Modifier::DIM);
        }
        paint_pane_dim(grid.grid, inner, buf, base);
        if let Some(sel) = ctx.selection.filter(|s| s.pane_id == slot.pane_id) {
            paint_selection_overlay(grid.grid, inner, buf, sel, paint);
        }
    }
}

fn record_divider_hits(slots: &[PaneSlot], hits: &mut HitMap) {
    for i in 0..slots.len() {
        for j in (i + 1)..slots.len() {
            let a = &slots[i];
            let b = &slots[j];
            if a.rect.y == b.rect.y && a.rect.height == b.rect.height {
                let x = a.rect.x + a.rect.width;
                if x == b.rect.x {
                    hits.push(
                        Rect::new(x.saturating_sub(1), a.rect.y, 2, a.rect.height),
                        HitTarget::Divider {
                            pane_id: a.pane_id.clone(),
                            dir: SplitDir::Horizontal,
                        },
                    );
                }
            }
            if a.rect.x == b.rect.x && a.rect.width == b.rect.width {
                let y = a.rect.y + a.rect.height;
                if y == b.rect.y {
                    hits.push(
                        Rect::new(a.rect.x, y.saturating_sub(1), a.rect.width, 2),
                        HitTarget::Divider {
                            pane_id: a.pane_id.clone(),
                            dir: SplitDir::Vertical,
                        },
                    );
                }
            }
        }
    }
}

fn paint_selection_overlay(
    grid: &CellGrid,
    area: Rect,
    buf: &mut Buffer,
    sel: &Selection,
    paint: &Paint,
) {
    let (from, to) = if sel.from.row < sel.to.row
        || (sel.from.row == sel.to.row && sel.from.col <= sel.to.col)
    {
        (sel.from, sel.to)
    } else {
        (sel.to, sel.from)
    };
    let hi = theme::selection_highlight(paint);
    for row in from.row..=to.row {
        let col_start = if row == from.row { from.col } else { 0 };
        let col_end = if row == to.row {
            to.col
        } else {
            area.width.saturating_sub(1)
        };
        for col in col_start..=col_end.min(area.width.saturating_sub(1)) {
            if let Some(cell) = grid.cell(col, row) {
                let ch = if cell.wide_spacer || cell.ch == '\0' {
                    ' '
                } else {
                    cell.ch
                };
                let x = area.x + col;
                let y = area.y + row;
                if let Some(dst) = buf.cell_mut((x, y)) {
                    dst.set_symbol(&ch.to_string());
                    dst.set_style(hi);
                }
            }
        }
    }
}

fn paint_drag_preview(drag: &DragState, buf: &mut Buffer, paint: &Paint) {
    let style = theme::pane_border_focused(paint);
    let (x, y) = drag.current;
    let hint = match &drag.target {
        DragTarget::Divider { .. } => "↔",
        DragTarget::Tab { .. } => "⇄",
        DragTarget::TabToWorkspace { .. } => "⇢",
        DragTarget::Pane { .. } => "⤢",
    };
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(hint);
        cell.set_style(style);
    }
}

/// Slide-out event panel from daemon attention items.
pub fn paint_event_panel(lines: &[String], area: Rect, buf: &mut Buffer, paint: &Paint) {
    let w = area.width.min(40);
    let panel = Rect::new(
        area.x + area.width.saturating_sub(w),
        area.y,
        w,
        area.height,
    );
    let block = Block::default()
        .borders(Borders::LEFT)
        .title(" Events ")
        .border_style(theme::pane_border_focused(paint));
    let inner = block.inner(panel);
    block.render(panel, buf);
    let text: Vec<Line> = if lines.is_empty() {
        vec![Line::from(Span::styled(
            copy::EVENT_PANEL_EMPTY,
            theme::sidebar_row(paint),
        ))]
    } else {
        lines
            .iter()
            .take(inner.height as usize)
            .map(|l| Line::from(Span::raw(l.as_str())))
            .collect()
    };
    Paragraph::new(text).render(inner, buf);
}

fn paint_pane_dim(grid: &CellGrid, area: Rect, buf: &mut Buffer, base: Style) {
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
    use crate::decoration::DecorationSnapshot;
    use crate::dialog::Dialog;
    use crate::layout::{parse_layout, resolve_layout};
    use crate::model::{RuntimeRegistry, TabModel, WorkspaceRow};
    use crate::resilience::LinkState;
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
            let mut hits = HitMap::default();
            paint_tab_bar(
                std::slice::from_ref(&tab),
                0,
                tab_area,
                f.buffer_mut(),
                &theme,
                &mut hits,
                &DecorationSnapshot::default(),
            );
            let mut ctx = WindowPaintCtx {
                link: LinkState::Live,
                paused: &std::collections::HashSet::new(),
                hits: &mut hits,
                decoration: &DecorationSnapshot::default(),
                selection: None,
                drag: None,
            };
            paint_window(&tab, &runtimes, canvas, f.buffer_mut(), &theme, &mut ctx);
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
            let mut hits = HitMap::default();
            paint_sidebar(
                &workspaces,
                0,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                &DecorationSnapshot::default(),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let flat: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(
            flat.contains("cyclops"),
            "sidebar should list workspace: {flat}"
        );
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
    fn reconnecting_pane_renders_dimmed_note() {
        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = WindowPaintCtx {
                link: LinkState::Reconnecting { attempt: 1 },
                paused: &paused,
                hits: &mut hits,
                decoration: &dec,
                selection: None,
                drag: None,
            };
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let flat: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(
            flat.contains("reconnecting"),
            "border should note reconnect: {flat}"
        );
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
    fn selection_highlight_renders_on_test_backend() {
        use crate::runtime::CellPos;
        use crate::selection::Selection;

        let tab = two_pane_tab();
        let mut runtimes = RuntimeRegistry::default();
        let mut rt = crate::runtime::PaneRuntime::new(5, 2);
        rt.feed(b"hello\r\n");
        runtimes.insert("%0".into(), rt);
        let sel = Selection {
            pane_id: "%0".into(),
            from: CellPos { col: 0, row: 0 },
            to: CellPos { col: 2, row: 0 },
        };
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = WindowPaintCtx {
                link: LinkState::Live,
                paused: &paused,
                hits: &mut hits,
                decoration: &dec,
                selection: Some(&sel),
                drag: None,
            };
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut saw_highlight = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].bg != RtColor::Reset {
                    saw_highlight = true;
                }
            }
        }
        assert!(saw_highlight, "selection should paint a highlight");
    }

    #[test]
    fn agent_badge_renders_on_pane_border() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%0".into(),
            PaneDecoration {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: Some("claude".into()),
                state: AgentState::Working,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = WindowPaintCtx {
                link: LinkState::Live,
                paused: &paused,
                hits: &mut hits,
                decoration: &decoration,
                selection: None,
                drag: None,
            };
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let flat: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(flat.contains("working"), "badge word should render: {flat}");
    }

    #[test]
    fn focused_pane_uses_accent_border() {
        let slots = layout_pane_slots(&two_pane_tab().layout, Rect::new(0, 0, 40, 10), "%0");
        assert_eq!(slots.len(), 2);
        assert!(slots[0].focused);
        assert!(!slots[1].focused);
    }
}
