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
use crate::layout::{layout_dividers, layout_pane_slots, offset_clip};
use crate::model::{PaneSlot, RuntimeRegistry, TabModel, WorkspaceRow};
use crate::resilience::LinkState;
use crate::runtime::{CellGrid, Color, GridCell};
use crate::selection::Selection;
use crate::theme::{self, Paint};

/// Title, optional input buffer, optional hint, and button labels for one
/// dialog.
fn dialog_parts(dialog: &Dialog) -> (&str, Option<&str>, Option<&str>, &'static str) {
    match dialog {
        Dialog::ConfirmClosePane { .. } => (copy::CONFIRM_CLOSE_PANE, None, None, copy::BUTTON_YES),
        Dialog::NewTab { buffer } => (
            copy::NEW_TAB_TITLE,
            Some(buffer),
            Some(copy::NEW_TAB_HINT),
            copy::BUTTON_CREATE,
        ),
        Dialog::RenameTab { buffer, .. } => (
            copy::RENAME_TAB_PROMPT,
            Some(buffer),
            None,
            copy::BUTTON_SAVE,
        ),
        Dialog::ConfirmCloseTab { .. } => (copy::CONFIRM_CLOSE_TAB, None, None, copy::BUTTON_YES),
        Dialog::RenameWorkspace { buffer, .. } => (
            copy::RENAME_WORKSPACE_PROMPT,
            Some(buffer),
            None,
            copy::BUTTON_SAVE,
        ),
        Dialog::ConfirmCloseWorkspace { .. } => {
            (copy::CONFIRM_CLOSE_WORKSPACE, None, None, copy::BUTTON_YES)
        }
    }
}

/// Paint a modal dialog centered in `area`, recording its buttons as hit
/// regions. `hover` is the mouse cell; the button under it highlights.
pub fn paint_dialog(
    dialog: &Dialog,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    let (title, input, hint, confirm_label) = dialog_parts(dialog);
    let cancel_label = if dialog.has_input() {
        copy::BUTTON_CANCEL
    } else {
        copy::BUTTON_NO
    };
    let want_w = (Span::raw(title).width() as u16 + 4).max(40);
    let w = want_w.min(area.width);
    let h = 7u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let dialog_area = Rect::new(x, y, w, h);
    // Clear beneath so pane content doesn't bleed through, then ground.
    for row in dialog_area.y..dialog_area.y + dialog_area.height {
        for col in dialog_area.x..dialog_area.x + dialog_area.width {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.reset();
            }
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::pane_border_focused(paint))
        .style(theme::menu_row(paint));
    let inner = block.inner(dialog_area);
    block.render(dialog_area, buf);

    overlay_text(
        buf,
        inner,
        inner.x + 1,
        inner.y,
        title,
        theme::menu_row(paint),
    );
    if let Some(hint) = hint {
        overlay_text(
            buf,
            inner,
            inner.x + 1,
            inner.y + 1,
            hint,
            theme::sidebar_row(paint).patch(theme::chrome_panel(paint)),
        );
    }
    if let Some(input) = input {
        // The input field: a raised full-width row with a cursor mark.
        let field_y = inner.y + 2;
        if field_y < inner.y + inner.height {
            let field = Rect::new(inner.x + 1, field_y, inner.width.saturating_sub(2), 1);
            buf.set_style(field, theme::menu_row_hover(paint));
            let visible = input_tail(input, field.width.saturating_sub(1) as usize);
            overlay_text(
                buf,
                inner,
                field.x + 1,
                field_y,
                &visible,
                theme::menu_row_hover(paint),
            );
        }
    }
    // Buttons on the last inner row, recorded for the mouse.
    let button_y = inner.y + inner.height.saturating_sub(1);
    let mut bx = inner.x + 1;
    for (label, target) in [
        (confirm_label, HitTarget::DialogConfirm),
        (cancel_label, HitTarget::DialogCancel),
    ] {
        let text = format!("[ {label} ]");
        let bw = Span::raw(text.as_str()).width() as u16;
        let rect = Rect::new(bx, button_y, bw.min(inner.width), 1);
        let hovered =
            hover.is_some_and(|(hc, hr)| hr == rect.y && hc >= rect.x && hc < rect.x + rect.width);
        let style = if hovered {
            theme::pane_border_focused(paint)
                .patch(theme::chrome_raised(paint))
                .add_modifier(Modifier::BOLD)
        } else {
            theme::menu_row_hover(paint)
        };
        overlay_text(buf, inner, bx, button_y, &text, style);
        hits.push(rect, target);
        bx = bx.saturating_add(bw + 2);
    }
}

/// Keep the editing cursor visible when a name is wider than its field.
fn input_tail(input: &str, width: usize) -> String {
    const CURSOR: &str = "▏";
    let cursor_width = Span::raw(CURSOR).width();
    let content_width = width.saturating_sub(cursor_width);
    let mut start = input.len();
    for (index, _) in input.char_indices().rev() {
        if Span::raw(&input[index..]).width() > content_width {
            break;
        }
        start = index;
    }
    format!("{}{CURSOR}", &input[start..])
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
    // A breath under the title separates the header from the rows.
    lines.push(Line::from(""));
    if !decoration.online {
        lines.push(Line::from(Span::styled(
            " cyclopsd offline",
            theme::sidebar_row(paint),
        )));
    }
    let mut active_row = None;
    for (i, ws) in workspaces.iter().enumerate() {
        let marker = if i == active { "●" } else { " " };
        let style = if i == active {
            theme::sidebar_row_active(paint).add_modifier(Modifier::BOLD)
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
                HitTarget::SidebarRow {
                    session: ws.name.clone(),
                },
            );
            if i == active {
                active_row = Some(y);
            }
        }
    }
    Paragraph::new(lines).render(inner, buf);
    // The active row's ground spans the full sidebar width, so the
    // contrast reads as a row, not a word.
    if let Some(y) = active_row {
        buf.set_style(
            Rect::new(inner.x, y, inner.width, 1),
            theme::sidebar_row_active(paint),
        );
    }

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
            theme::tab_active(paint).add_modifier(Modifier::BOLD)
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

/// One cell of breathing room around the pane canvas. With tmux's own
/// one-cell gaps between panes, every pane ends up ringed by gutter
/// cells, which is where the focused pane's ring draws.
pub const PANE_MARGIN: u16 = 1;

/// The rectangle panes actually occupy: the canvas inset by
/// [`PANE_MARGIN`] when there is room. The client size declared to tmux
/// must be this rectangle's size, so panes and gutters agree.
pub fn pane_canvas(canvas: Rect) -> Rect {
    if canvas.width > 2 * PANE_MARGIN && canvas.height > 2 * PANE_MARGIN {
        Rect::new(
            canvas.x + PANE_MARGIN,
            canvas.y + PANE_MARGIN,
            canvas.width - 2 * PANE_MARGIN,
            canvas.height - 2 * PANE_MARGIN,
        )
    } else {
        canvas
    }
}

/// Render every pane of the active window. The gaps tmux leaves between
/// panes — and the margin around them — paint as the chrome ground, so
/// panes read as cards with a gutter, not boxes sharing a line; the
/// focused pane gets an accent ring drawn in its surrounding gutter
/// cells.
pub fn paint_window(
    tab: &TabModel,
    runtimes: &mut RuntimeRegistry,
    canvas: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    if canvas.width == 0 || canvas.height == 0 {
        return;
    }
    // Ground the whole canvas: margins and pane gaps become gutter.
    buf.set_style(canvas, theme::chrome_panel(paint));
    let inner = pane_canvas(canvas);
    let slots = if tab.zoomed {
        vec![PaneSlot {
            pane_id: tab.active_pane.clone(),
            rect: inner,
            focused: true,
        }]
    } else {
        layout_pane_slots(&tab.layout, inner, &tab.active_pane)
    };
    if !tab.zoomed {
        push_divider_hits(tab, inner, ctx.hits);
    }
    if let Some(focused) = slots.iter().find(|s| s.focused) {
        paint_focus_ring(focused.rect, canvas, buf, theme::pane_border_focused(paint));
    }
    for slot in &slots {
        paint_pane_slot(slot, runtimes, buf, paint, ctx);
    }
    if let Some(drag) = ctx.drag.filter(|d| d.is_active()) {
        paint_drag_preview(drag, buf, paint);
    }
}

/// Divider gap cells stay grabbable for resize even though they paint as
/// plain gutter.
fn push_divider_hits(tab: &TabModel, inner: Rect, hits: &mut HitMap) {
    for seg in layout_dividers(&tab.layout) {
        let Some(rect) = offset_clip(
            seg.rect.x,
            seg.rect.y,
            seg.rect.width,
            seg.rect.height,
            inner,
        ) else {
            continue;
        };
        hits.push(
            rect,
            HitTarget::Divider {
                pane_id: seg.pane_id.clone(),
                dir: seg.dir,
            },
        );
    }
}

/// An accent ring in the gutter cells around `rect`, clipped to `bounds`.
/// The margin plus tmux's pane gaps guarantee those cells are gutter,
/// never another pane's content.
fn paint_focus_ring(rect: Rect, bounds: Rect, buf: &mut Buffer, style: Style) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let left = rect.x as i32 - 1;
    let top = rect.y as i32 - 1;
    let right = (rect.x + rect.width) as i32;
    let bottom = (rect.y + rect.height) as i32;
    let mut set = |x: i32, y: i32, sym: &str| {
        if x < bounds.x as i32
            || x >= (bounds.x + bounds.width) as i32
            || y < bounds.y as i32
            || y >= (bounds.y + bounds.height) as i32
        {
            return;
        }
        if let Some(cell) = buf.cell_mut((x as u16, y as u16)) {
            cell.set_symbol(sym);
            cell.set_style(style);
        }
    };
    for x in rect.x as i32..right {
        set(x, top, "─");
        set(x, bottom, "─");
    }
    for y in rect.y as i32..bottom {
        set(left, y, "│");
        set(right, y, "│");
    }
    set(left, top, "╭");
    set(right, top, "╮");
    set(left, bottom, "╰");
    set(right, bottom, "╯");
}

fn paint_pane_slot(
    slot: &PaneSlot,
    runtimes: &mut RuntimeRegistry,
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
    if let Some(runtime) = runtimes.get_mut(&slot.pane_id) {
        let grid = runtime.grid();
        paint_pane_styled(grid.grid, vis, buf, base);
        if let Some(sel) = ctx.selection.filter(|s| s.pane_id == slot.pane_id) {
            paint_selection_overlay(grid.grid, vis, buf, sel, paint);
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
    // controls render on every pane — accent on the focused one, dim on
    // the rest — so any pane can be split without focusing it first;
    // decoration sits to their left.
    let mut right_edge = vis.x + vis.width;
    if vis.width >= 10 {
        let style = if slot.focused {
            theme::pane_border_focused(paint)
        } else {
            theme::pane_border(paint)
        };
        // Rightmost first: split-down button, then split-right to its left.
        for split_down in [true, false] {
            let glyph = if split_down { "[-]" } else { "[|]" };
            let w = glyph.len() as u16;
            right_edge = right_edge.saturating_sub(w);
            let rect = Rect::new(right_edge, vis.y, w, 1);
            overlay_text(buf, vis, rect.x, rect.y, glyph, style);
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
        let available = right_edge.saturating_sub(vis.x);
        if available > 0 {
            let x = right_edge.saturating_sub(w).max(vis.x);
            let bounds = Rect::new(vis.x, vis.y, available, 1);
            overlay_text(
                buf,
                bounds,
                x,
                vis.y,
                &label,
                theme::pane_border_focused(paint),
            );
            if dec.needs_attention {
                ctx.hits.push(
                    Rect::new(x, vis.y, w.min(right_edge - x), 1),
                    HitTarget::AttentionIndicator {
                        pane_id: slot.pane_id.clone(),
                    },
                );
            }
        }
    }
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
        MenuState::TabMenu { .. } => vec![
            (copy::MENU_RENAME_TAB, BindingAction::RenameTab),
            (copy::MENU_CLOSE_TAB, BindingAction::CloseTab),
        ],
        MenuState::WorkspaceMenu { .. } => vec![
            (copy::MENU_RENAME_WORKSPACE, BindingAction::RenameWorkspace),
            (copy::MENU_CLOSE_WORKSPACE, BindingAction::CloseWorkspace),
        ],
    }
}

/// Paint the open menu (if any) and record its item hit regions. Painted
/// last, so its regions shadow everything under it. `hover` is the mouse
/// cell; the item under it paints raised.
pub fn paint_menu(
    menu: &MenuState,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
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
        MenuState::ContextMenu { at, .. }
        | MenuState::TabMenu { at, .. }
        | MenuState::WorkspaceMenu { at, .. } => *at,
        // App menu opens above its bottom-left button.
        _ => (area.x, (area.y + area.height).saturating_sub(h + 1)),
    };
    let x = ax.min((area.x + area.width).saturating_sub(w).max(area.x));
    let y = ay.min((area.y + area.height).saturating_sub(h).max(area.y));
    let menu_area = Rect::new(x, y, w.min(area.width), h.min(area.height));
    // Clear what's underneath so pane content doesn't bleed through.
    for row in menu_area.y..menu_area.y + menu_area.height {
        for col in menu_area.x..menu_area.x + menu_area.width {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.reset();
            }
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::pane_border_focused(paint))
        .style(theme::menu_row(paint));
    let inner = block.inner(menu_area);
    block.render(menu_area, buf);
    for (i, (label, action)) in items.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.width, 1);
        let hovered =
            hover.is_some_and(|(hc, hr)| hr == rect.y && hc >= rect.x && hc < rect.x + rect.width);
        let style = if hovered {
            theme::menu_row_hover(paint)
        } else {
            theme::menu_row(paint)
        };
        buf.set_style(rect, style);
        overlay_text(buf, inner, inner.x, y, &format!(" {label}"), style);
        hits.push(rect, HitTarget::MenuItem { action: *action });
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
                    dst.set_char(ch);
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
            let (ch, style) = if let Some(cell) = grid.cell(col, row) {
                let ch = if cell.wide_spacer || cell.ch == '\0' {
                    ' '
                } else {
                    cell.ch
                };
                (ch, cell_style(cell, base))
            } else {
                (' ', base)
            };
            let x = area.x + col;
            let y = area.y + row;
            if let Some(dst) = buf.cell_mut((x, y)) {
                dst.set_char(ch);
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
    fn gutter_ring_and_tab_bar_render() {
        let tab = two_pane_tab();
        let mut runtimes = RuntimeRegistry::default();

        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            let area = f.area();
            let tab_area = Rect::new(area.x, area.y, area.width, 1);
            let canvas = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
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
            paint_window(
                &tab,
                &mut runtimes,
                canvas,
                f.buffer_mut(),
                &theme,
                &mut ctx,
            );
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
        // Canvas starts at row 1 and panes are inset one margin cell, so
        // the focused pane's ring corners land on the canvas edge rows.
        assert_eq!(buf[(0, 1)].symbol(), "╭", "ring top-left corner");
        assert_eq!(buf[(39, 1)].symbol(), "╮", "ring top-right corner");
        // The pane is 5 rows tall from screen row 2, so the ring's bottom
        // sits on the old divider gap row.
        assert_eq!(buf[(0, 7)].symbol(), "╰", "ring bottom-left corner");
        assert_eq!(buf[(5, 7)].symbol(), "─", "ring bottom edge in the gutter");
        // The gap row is still grabbable for resize.
        assert!(matches!(hits.hit(20, 7), Some(HitTarget::Divider { .. })));
        // Below the second pane the outer margin is plain gutter.
        assert_eq!(buf[(20, 11)].symbol(), " ", "margin paints as gutter");
    }

    #[test]
    fn pane_canvas_insets_by_the_margin() {
        assert_eq!(pane_canvas(Rect::new(0, 1, 40, 11)), Rect::new(1, 2, 38, 9));
        // Too small to inset: the canvas is used as-is.
        assert_eq!(pane_canvas(Rect::new(0, 0, 2, 2)), Rect::new(0, 0, 2, 2));
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
        assert!(matches!(
            hits.hit(1, 0),
            Some(HitTarget::Tab { window_id }) if window_id == "@0"
        ));
        assert!(matches!(
            hits.hit(8, 0),
            Some(HitTarget::Tab { window_id }) if window_id == "@1"
        ));
        // After both labels comes the + button.
        assert!(matches!(hits.hit(15, 0), Some(HitTarget::NewTabButton)));
    }

    #[test]
    fn sidebar_rows_render_and_hit_test_aligned() {
        let workspaces = vec![
            WorkspaceRow {
                session_id: "$0".into(),
                name: "cyclops".into(),
                tab_count: 2,
                active: true,
            },
            WorkspaceRow {
                session_id: "$1".into(),
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
        // The title row is followed by a spacer, then the offline note,
        // so workspaces paint on rows 3 and 4.
        assert!(matches!(
            hits.hit(2, 3),
            Some(HitTarget::SidebarRow { session }) if session == "cyclops"
        ));
        assert!(matches!(
            hits.hit(2, 4),
            Some(HitTarget::SidebarRow { session }) if session == "website"
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
            paint_menu(&menu, f.area(), f.buffer_mut(), &theme, &mut hits, None);
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
    fn hovered_menu_row_paints_raised() {
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let menu = MenuState::ContextMenu {
            pane_id: "%0".into(),
            at: (5, 2),
        };
        term.draw(|f| {
            paint_menu(
                &menu,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                Some((7, 3)),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        // The hovered first row's ground differs from the row below it.
        assert_ne!(
            buf[(7, 3)].bg,
            buf[(7, 4)].bg,
            "hover should raise the row under the mouse"
        );
    }

    #[test]
    fn tab_menu_offers_rename_and_close() {
        let items = menu_items(&MenuState::TabMenu {
            window_id: "@1".into(),
            at: (0, 0),
        });
        let actions: Vec<_> = items.iter().map(|(_, a)| *a).collect();
        assert_eq!(
            actions,
            vec![BindingAction::RenameTab, BindingAction::CloseTab]
        );
        let items = menu_items(&MenuState::WorkspaceMenu {
            session: "cyclops".into(),
            at: (0, 0),
        });
        let actions: Vec<_> = items.iter().map(|(_, a)| *a).collect();
        assert_eq!(
            actions,
            vec![
                BindingAction::RenameWorkspace,
                BindingAction::CloseWorkspace
            ]
        );
    }

    #[test]
    fn new_tab_dialog_renders_input_and_buttons() {
        let backend = TestBackend::new(50, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let dialog = Dialog::NewTab {
            buffer: "revw".into(),
        };
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("New tab"), "title renders: {flat}");
        assert!(flat.contains("revw"), "typed name renders: {flat}");
        assert!(flat.contains("[ Create ]"), "confirm button: {flat}");
        assert!(flat.contains("[ Cancel ]"), "cancel button: {flat}");
        let confirm = hits
            .regions()
            .iter()
            .find(|r| r.target == HitTarget::DialogConfirm)
            .expect("confirm button is clickable");
        assert!(matches!(
            hits.hit(confirm.rect.x, confirm.rect.y),
            Some(HitTarget::DialogConfirm)
        ));
        assert!(hits
            .regions()
            .iter()
            .any(|r| r.target == HitTarget::DialogCancel));
    }

    #[test]
    fn long_dialog_input_keeps_its_tail_and_cursor_visible() {
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let dialog = Dialog::NewTab {
            buffer: "a-very-long-tab-name-that-must-scroll-to-the-visible-tail".into(),
        };
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("visible-tail▏"),
            "tail and cursor render: {flat}"
        );
    }

    #[test]
    fn reconnecting_pane_renders_dimmed_note() {
        let tab = two_pane_tab();
        let mut runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            ctx.link = LinkState::Reconnecting { attempt: 1 };
            paint_window(
                &tab,
                &mut runtimes,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut ctx,
            );
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
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("Close this pane"));
        assert!(flat.contains("[ Yes ]"), "confirm gets buttons too: {flat}");
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
            paint_window(
                &tab,
                &mut runtimes,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut ctx,
            );
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
        let mut runtimes = RuntimeRegistry::default();
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
            paint_window(
                &tab,
                &mut runtimes,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("working"), "badge word should render: {flat}");
    }

    #[test]
    fn narrow_pane_badge_never_overwrites_split_controls() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let node = parse_layout("0000,10x3,0,0,0").unwrap();
        let tab = TabModel {
            window_id: "@0".into(),
            name: "main".into(),
            layout: resolve_layout(&node, &[]).unwrap(),
            active_pane: "%0".into(),
            zoomed: false,
        };
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
                state: AgentState::BlockedPermission,
                needs_attention: true,
            },
        );
        let mut runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(12, 5);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &mut runtimes,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();

        let row: String = (0..12)
            .map(|x| term.backend().buffer()[(x, 1)].symbol().to_string())
            .collect();
        assert!(row.contains("[|][-]"), "controls stay intact: {row:?}");
        assert!(matches!(
            hits.hit(6, 1),
            Some(HitTarget::PaneSplitRight { .. })
        ));
        assert!(matches!(
            hits.hit(9, 1),
            Some(HitTarget::PaneSplitDown { .. })
        ));
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
            paint_window(
                &tab,
                &mut runtimes,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut ctx,
            );
            cursor = ctx.cursor;
        })
        .unwrap();
        // The pane sits one margin cell in from the canvas origin.
        assert_eq!(cursor, Some((3, 1)), "cursor should track the focused pane");
    }

    #[test]
    fn focused_pane_slots_flag() {
        let slots = layout_pane_slots(&two_pane_tab().layout, Rect::new(0, 0, 40, 11), "%0");
        assert_eq!(slots.len(), 2);
        assert!(slots[0].focused);
        assert!(!slots[1].focused);
    }
}
