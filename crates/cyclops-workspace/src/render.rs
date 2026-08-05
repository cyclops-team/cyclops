//! Paint pane grids and chrome into a Ratatui buffer.
//!
//! Panes render at their tmux cell coordinates 1:1 — the client size the
//! workspace declares is the pane canvas, so tmux's own one-cell gaps
//! between panes are where dividers draw. Nothing scales; a runtime grid
//! lands on exactly the cells tmux gave the pane.

#![allow(clippy::too_many_arguments, dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::bindings::BindingAction;
use crate::copy;
use crate::decoration::DecorationSnapshot;
use crate::dialog::Dialog;
use crate::drag::{DragState, DragTarget};
use crate::input::mouse::{HitMap, HitTarget, MenuState, PaneGeometry};
use crate::layout::{layout_dividers, layout_pane_slots, offset_clip, SplitDir};
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
    paint_pane_styled(grid, area, buf, theme::pane_cell(paint));
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
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(theme::pane_border(paint));
    let inner = block.inner(area);
    block.render(area, buf);

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
        let marker = if i == active { "●" } else { " " };
        let style = if i == active {
            theme::sidebar_row_active(paint)
        } else {
            theme::sidebar_row(paint)
        };
        // The row's paragraph line index is lines.len() right now, so its
        // screen row is knowable before pushing — hit rows always match
        // painted rows.
        let y = inner.y + lines.len() as u16;
        lines.push(Line::from(Span::styled(
            format!("{marker} {} ({})", ws.name, ws.tab_count),
            style,
        )));
        if y < inner.y + inner.height {
            hits.push(
                Rect::new(inner.x, y, inner.width, 1),
                HitTarget::SidebarRow { index: i },
            );
        }
    }
    Paragraph::new(lines).render(inner, buf);

    // Application-menu button on the bottom row.
    if inner.height >= 2 {
        let menu_row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", copy::APP_MENU_BUTTON),
            theme::sidebar_label(paint),
        )))
        .render(menu_row, buf);
        hits.push(menu_row, HitTarget::AppMenu);
    }
}

/// Render the tab bar. Hit regions come from the measured span widths, so
/// clicks land on the tab actually painted there.
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
    let mut x = area.x;
    let right = area.x + area.width;
    for (i, tab) in tabs.iter().enumerate() {
        let attn = if decoration.tab_needs_attention(&tab.window_id) {
            " ◉"
        } else {
            ""
        };
        let style = if i == active {
            theme::tab_active(paint).add_modifier(Modifier::BOLD)
        } else {
            theme::tab_inactive(paint)
        };
        let label = format!(" {}{} ", tab.name, attn);
        let w = Span::raw(label.as_str()).width() as u16;
        if x < right {
            hits.push(
                Rect::new(x, area.y, w.min(right - x), area.height.max(1)),
                HitTarget::Tab { index: i },
            );
        }
        spans.push(Span::styled(label, style));
        spans.push(Span::styled("│", theme::pane_border(paint)));
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
    spans.push(Span::styled(plus, theme::tab_inactive(paint)));
    Paragraph::new(Line::from(spans)).render(area, buf);
}

/// Chrome state passed through the window paint pass.
pub struct WindowPaintCtx<'a> {
    pub link: LinkState,
    pub paused: &'a std::collections::HashSet<String>,
    pub hits: &'a mut HitMap,
    pub decoration: &'a DecorationSnapshot,
    pub selection: Option<&'a Selection>,
    pub drag: Option<&'a DragState>,
    /// Screen cell for the hardware cursor when the focused pane shows one.
    pub cursor: Option<(u16, u16)>,
}

/// Render every pane of the active window plus the dividers between them.
pub fn paint_window(
    tab: &TabModel,
    runtimes: &RuntimeRegistry,
    canvas: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    if canvas.width == 0 || canvas.height == 0 {
        return;
    }
    let slots = if tab.zoomed {
        vec![PaneSlot {
            pane_id: tab.active_pane.clone(),
            rect: canvas,
            focused: true,
        }]
    } else {
        layout_pane_slots(&tab.layout, canvas, &tab.active_pane)
    };
    if !tab.zoomed {
        paint_dividers(tab, &slots, canvas, buf, paint, ctx.hits);
    }
    for slot in &slots {
        paint_pane_slot(slot, runtimes, buf, paint, ctx);
    }
    if let Some(drag) = ctx.drag.filter(|d| d.is_active()) {
        paint_drag_preview(drag, buf, paint);
    }
}

fn paint_dividers(
    tab: &TabModel,
    slots: &[PaneSlot],
    canvas: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
) {
    let focused_rect = slots.iter().find(|s| s.focused).map(|s| s.rect);
    for seg in layout_dividers(&tab.layout) {
        let Some(rect) = offset_clip(
            seg.rect.x,
            seg.rect.y,
            seg.rect.width,
            seg.rect.height,
            canvas,
        ) else {
            continue;
        };
        let touches_focus = focused_rect.is_some_and(|f| rects_touch(f, rect));
        let style = if touches_focus {
            theme::pane_border_focused(paint)
        } else {
            theme::pane_border(paint)
        };
        let glyph = match seg.dir {
            SplitDir::Horizontal => "│",
            SplitDir::Vertical => "─",
        };
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(glyph);
                    cell.set_style(style);
                }
            }
        }
        hits.push(
            rect,
            HitTarget::Divider {
                pane_id: seg.pane_id.clone(),
                dir: seg.dir,
            },
        );
    }
}

/// Whether two non-overlapping rectangles share an edge.
fn rects_touch(a: Rect, b: Rect) -> bool {
    let h_adjacent = (a.x + a.width == b.x || b.x + b.width == a.x)
        && a.y < b.y + b.height
        && b.y < a.y + a.height;
    let v_adjacent = (a.y + a.height == b.y || b.y + b.height == a.y)
        && a.x < b.x + b.width
        && b.x < a.x + a.width;
    h_adjacent || v_adjacent
}

fn paint_pane_slot(
    slot: &PaneSlot,
    runtimes: &RuntimeRegistry,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    let vis = slot.rect;
    if vis.width == 0 || vis.height == 0 {
        return;
    }
    let mut base = theme::pane_cell(paint);
    if matches!(ctx.link, LinkState::Reconnecting { .. }) {
        base = base.add_modifier(Modifier::DIM);
    }
    if let Some(runtime) = runtimes.get(&slot.pane_id) {
        paint_pane_styled(runtime.grid().grid, vis, buf, base);
        if let Some(sel) = ctx.selection.filter(|s| s.pane_id == slot.pane_id) {
            paint_selection_overlay(runtime.grid().grid, vis, buf, sel, paint);
        }
        if slot.focused {
            let cur = runtime.cursor();
            if cur.visible && runtime.at_tail() && cur.col < vis.width && cur.row < vis.height {
                ctx.cursor = Some((vis.x + cur.col, vis.y + cur.row));
            }
        }
    } else {
        let empty = CellGrid {
            cols: 0,
            rows: 0,
            cells: Vec::new(),
        };
        paint_pane_styled(&empty, vis, buf, base);
    }

    ctx.hits.push(
        vis,
        HitTarget::PaneBody {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push_geometry(PaneGeometry {
        pane_id: slot.pane_id.clone(),
        inner: vis,
        cols: vis.width,
        rows: vis.height,
    });

    // Transient link state, top-left overlay.
    let note = if matches!(ctx.link, LinkState::Reconnecting { .. }) {
        Some(copy::RECONNECTING_NOTE)
    } else if ctx.paused.contains(&slot.pane_id) {
        Some(copy::PAUSED_NOTE)
    } else {
        None
    };
    if let Some(note) = note {
        overlay_text(
            buf,
            vis,
            vis.x,
            vis.y,
            &format!(" {note} "),
            theme::pane_border_focused(paint).add_modifier(Modifier::DIM),
        );
    }

    // Agent decoration and split controls, top-right overlay. Split
    // controls render on the focused pane only; decoration sits to their
    // left. Plain undetected panes get no chrome text at all.
    let mut right_edge = vis.x + vis.width;
    if slot.focused && vis.width >= 10 {
        // Rightmost first: split-down button, then split-right to its left.
        for split_down in [true, false] {
            let glyph = if split_down { "[-]" } else { "[|]" };
            let w = glyph.len() as u16;
            right_edge = right_edge.saturating_sub(w);
            let rect = Rect::new(right_edge, vis.y, w, 1);
            overlay_text(
                buf,
                vis,
                rect.x,
                rect.y,
                glyph,
                theme::pane_border_focused(paint),
            );
            let target = if split_down {
                HitTarget::PaneSplitDown {
                    pane_id: slot.pane_id.clone(),
                }
            } else {
                HitTarget::PaneSplitRight {
                    pane_id: slot.pane_id.clone(),
                }
            };
            ctx.hits.push(rect, target);
        }
    }
    if let Some(dec) = ctx.decoration.pane(&slot.pane_id) {
        let attn = if dec.needs_attention { " ◉" } else { "" };
        let label = format!(" {}{} ", DecorationSnapshot::state_badge(dec.state), attn);
        let w = Span::raw(label.as_str()).width() as u16;
        let x = right_edge.saturating_sub(w).max(vis.x);
        overlay_text(
            buf,
            vis,
            x,
            vis.y,
            &label,
            theme::pane_border_focused(paint),
        );
        if dec.needs_attention {
            ctx.hits.push(
                Rect::new(x, vis.y, w, 1),
                HitTarget::AttentionIndicator {
                    pane_id: slot.pane_id.clone(),
                },
            );
        }
    }
}

/// Write `text` onto one row, clipped to `bounds`.
fn overlay_text(buf: &mut Buffer, bounds: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y < bounds.y || y >= bounds.y + bounds.height {
        return;
    }
    for (cx, ch) in (x..).zip(text.chars()) {
        if cx < bounds.x || cx >= bounds.x + bounds.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((cx, y)) {
            cell.set_symbol(&ch.to_string());
            cell.set_style(style);
        }
    }
}

/// Menu items for one open menu.
pub fn menu_items(menu: &MenuState) -> Vec<(&'static str, BindingAction)> {
    match menu {
        MenuState::None => Vec::new(),
        MenuState::AppMenu => vec![
            (copy::MENU_NEW_TAB, BindingAction::NewTab),
            (copy::MENU_NEW_WORKSPACE, BindingAction::NewWorkspace),
            (copy::MENU_TOGGLE_EVENTS, BindingAction::ToggleEventPanel),
            (copy::MENU_DETACH, BindingAction::Detach),
        ],
        MenuState::ContextMenu { .. } => vec![
            (copy::MENU_SPLIT_RIGHT, BindingAction::SplitRight),
            (copy::MENU_SPLIT_DOWN, BindingAction::SplitDown),
            (copy::MENU_ZOOM_PANE, BindingAction::ZoomPane),
            (copy::MENU_CLOSE_PANE, BindingAction::ClosePane),
        ],
    }
}

/// Paint the open menu (if any) and record its item hit regions. Painted
/// last, so its regions shadow everything under it.
pub fn paint_menu(
    menu: &MenuState,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
) {
    let items = menu_items(menu);
    if items.is_empty() {
        return;
    }
    let w = (items
        .iter()
        .map(|(label, _)| Span::raw(*label).width())
        .max()
        .unwrap_or(0) as u16)
        .saturating_add(4);
    let h = items.len() as u16 + 2;
    let (ax, ay) = match menu {
        MenuState::ContextMenu { at, .. } => *at,
        // App menu opens above its bottom-left button.
        _ => (area.x, (area.y + area.height).saturating_sub(h + 1)),
    };
    let x = ax.min((area.x + area.width).saturating_sub(w).max(area.x));
    let y = ay.min((area.y + area.height).saturating_sub(h).max(area.y));
    let menu_area = Rect::new(x, y, w.min(area.width), h.min(area.height));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::pane_border_focused(paint));
    let inner = block.inner(menu_area);
    // Clear what's underneath so pane content doesn't bleed through.
    for row in menu_area.y..menu_area.y + menu_area.height {
        for col in menu_area.x..menu_area.x + menu_area.width {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.reset();
            }
        }
    }
    block.render(menu_area, buf);
    let mut lines = Vec::new();
    for (i, (label, action)) in items.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            format!(" {label}"),
            theme::sidebar_row(paint),
        )));
        let y = inner.y + i as u16;
        if y < inner.y + inner.height {
            hits.push(
                Rect::new(inner.x, y, inner.width, 1),
                HitTarget::MenuItem { action: *action },
            );
        }
    }
    Paragraph::new(lines).render(inner, buf);
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

fn paint_pane_styled(grid: &CellGrid, area: Rect, buf: &mut Buffer, base: Style) {
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

    /// Two stacked panes filling a 40x11 window, one divider row at y=5.
    fn two_pane_tab() -> TabModel {
        let node = parse_layout("4c3e,40x11,0,0[40x5,0,0,0,40x5,0,6,1]").unwrap();
        let layout = resolve_layout(&node, &[]).unwrap();
        TabModel {
            window_id: "@0".to_string(),
            index: 0,
            name: "main".to_string(),
            layout,
            active_pane: "%0".to_string(),
            zoomed: false,
        }
    }

    fn ctx_defaults<'a>(
        hits: &'a mut HitMap,
        paused: &'a std::collections::HashSet<String>,
        decoration: &'a DecorationSnapshot,
    ) -> WindowPaintCtx<'a> {
        WindowPaintCtx {
            link: LinkState::Live,
            paused,
            hits,
            decoration,
            selection: None,
            drag: None,
            cursor: None,
        }
    }

    fn flatten(buf: &Buffer) -> String {
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect()
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
    fn divider_and_tab_bar_render() {
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
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
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
        // The divider row is window y=5, canvas offset 1 → screen row 6.
        let divider_row: String = (0..buf.area.width)
            .map(|x| buf[(x, 6)].symbol().to_string())
            .collect();
        assert!(
            divider_row.chars().all(|c| c == '─'),
            "gap row should be a divider line: {divider_row:?}"
        );
    }

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
        assert!(matches!(hits.hit(1, 0), Some(HitTarget::Tab { index: 0 })));
        assert!(matches!(hits.hit(8, 0), Some(HitTarget::Tab { index: 1 })));
        // After both labels comes the + button.
        assert!(matches!(hits.hit(15, 0), Some(HitTarget::NewTabButton)));
    }

    #[test]
    fn sidebar_rows_render_and_hit_test_aligned() {
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
        let backend = TestBackend::new(20, 8);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
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
        let flat = flatten(buf);
        assert!(
            flat.contains("cyclops"),
            "sidebar should list workspace: {flat}"
        );
        assert!(flat.contains('●'), "active row should be marked");
        // Offline note occupies row 1, so workspaces paint on rows 2 and 3.
        assert!(matches!(
            hits.hit(2, 2),
            Some(HitTarget::SidebarRow { index: 0 })
        ));
        assert!(matches!(
            hits.hit(2, 3),
            Some(HitTarget::SidebarRow { index: 1 })
        ));
        // Bottom row is the app-menu button.
        assert!(matches!(hits.hit(2, 7), Some(HitTarget::AppMenu)));
        assert!(flat.contains("menu"), "menu button should render: {flat}");
    }

    #[test]
    fn context_menu_paints_items_with_hits() {
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let menu = MenuState::ContextMenu {
            pane_id: "%0".into(),
            at: (5, 2),
        };
        term.draw(|f| {
            paint_menu(&menu, f.area(), f.buffer_mut(), &theme, &mut hits);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("Split right"), "menu items render: {flat}");
        assert!(matches!(
            hits.hit(7, 3),
            Some(HitTarget::MenuItem {
                action: BindingAction::SplitRight
            })
        ));
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
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("New workspace folder"));
    }

    #[test]
    fn reconnecting_pane_renders_dimmed_note() {
        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            ctx.link = LinkState::Reconnecting { attempt: 1 };
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("reconnecting"),
            "pane should note reconnect: {flat}"
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
        let flat = flatten(term.backend().buffer());
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
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            ctx.selection = Some(&sel);
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
    fn agent_badge_renders_on_pane_overlay() {
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
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("working"), "badge word should render: {flat}");
    }

    #[test]
    fn focused_pane_reports_cursor_position() {
        let tab = two_pane_tab();
        let mut runtimes = RuntimeRegistry::default();
        let mut rt = crate::runtime::PaneRuntime::new(40, 5);
        rt.feed(b"$ ");
        runtimes.insert("%0".into(), rt);
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut cursor = None;
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
            cursor = ctx.cursor;
        })
        .unwrap();
        assert_eq!(cursor, Some((2, 0)), "cursor should track the focused pane");
    }

    #[test]
    fn focused_pane_slots_flag() {
        let slots = layout_pane_slots(&two_pane_tab().layout, Rect::new(0, 0, 40, 11), "%0");
        assert_eq!(slots.len(), 2);
        assert!(slots[0].focused);
        assert!(!slots[1].focused);
    }
}
