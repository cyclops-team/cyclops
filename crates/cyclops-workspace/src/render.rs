//! Paint pane grids and chrome into a Ratatui buffer.
//!
//! Panes render at their tmux cell coordinates 1:1. The workspace subtracts
//! the extra cells used by its separator bands before declaring
//! the client size, then restores those cells only as chrome. Nothing scales;
//! a runtime grid lands on exactly the cells tmux gave the pane.

#![allow(clippy::too_many_arguments, dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::bindings::BindingAction;
use crate::copy;
use crate::decoration::DecorationSnapshot;
use crate::dialog::Dialog;
use crate::drag::{DragState, DragTarget};
use crate::input::mouse::{HitMap, HitTarget, MenuState, PaneGeometry};
use crate::layout::{layout_gap_overhead, layout_geometry, DividerSeg, PaneGaps};
use crate::model::{PaneSlot, RuntimeRegistry, TabModel, WorkspaceRow};
use crate::resilience::LinkState;
use crate::runtime::{CellGrid, Color, GridCell};
use crate::selection::Selection;
use crate::theme::{self, Paint};

/// Title, optional input buffer, optional hint, and button labels for one
/// dialog.
fn dialog_parts(dialog: &Dialog) -> (&str, Option<&str>, Option<&str>, &'static str) {
    match dialog {
        Dialog::ConfirmClosePane { .. } => {
            (copy::CONFIRM_CLOSE_PANE, None, None, copy::BUTTON_CONFIRM)
        }
        Dialog::NewTab { buffer } => (
            copy::NEW_TAB_TITLE,
            Some(buffer),
            Some(copy::NEW_TAB_HINT),
            copy::BUTTON_CREATE,
        ),
        Dialog::NamePane { buffer, .. } => (
            copy::NAME_PANE_TITLE,
            Some(buffer),
            Some(copy::NAME_PANE_HINT),
            copy::BUTTON_SAVE,
        ),
        Dialog::RenameTab { buffer, .. } => (
            copy::RENAME_TAB_PROMPT,
            Some(buffer),
            None,
            copy::BUTTON_SAVE,
        ),
        Dialog::ConfirmCloseTab { .. } => {
            (copy::CONFIRM_CLOSE_TAB, None, None, copy::BUTTON_CONFIRM)
        }
        Dialog::RenameWorkspace { buffer, .. } => (
            copy::RENAME_WORKSPACE_PROMPT,
            Some(buffer),
            None,
            copy::BUTTON_SAVE,
        ),
        Dialog::ConfirmCloseWorkspace { .. } => (
            copy::CONFIRM_CLOSE_WORKSPACE,
            None,
            None,
            copy::BUTTON_CONFIRM,
        ),
        Dialog::Keybinds { .. } => unreachable!("keybinds uses its own dialog renderer"),
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
    if let Dialog::Keybinds { scroll, rows } = dialog {
        paint_keybinds_dialog(*scroll, rows, area, buf, paint, hits, hover);
        return;
    }
    let (title, input, hint, confirm_label) = dialog_parts(dialog);
    let error = match dialog {
        Dialog::NamePane { error, .. } => error.as_deref(),
        _ => None,
    };
    let copy_width = hint
        .map(|hint| Span::raw(hint).width())
        .unwrap_or(0)
        .max(Span::raw(title).width())
        .max(
            error
                .map(|error| Span::raw(error).width().min(68))
                .unwrap_or(0),
        );
    let want_w = (u16::try_from(copy_width)
        .unwrap_or(u16::MAX)
        .saturating_add(4))
    .max(40);
    let w = want_w.min(area.width);
    let error_lines = error
        .map(|error| wrapped_line_count(error, w.saturating_sub(4)))
        .unwrap_or(0);
    let h = u16::try_from(7usize.saturating_add(error_lines))
        .unwrap_or(u16::MAX)
        .min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let dialog_area = Rect::new(x, y, w, h);
    clear_area(buf, dialog_area);
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
            theme::menu_hint(paint),
        );
    }
    if let Some(input) = input {
        // The input field is inset from the dialog, but its editing cursor
        // starts at the field's first cell — no unexplained extra indent.
        let field_y = inner.y + 2;
        if field_y < inner.y + inner.height {
            let field = Rect::new(inner.x + 1, field_y, inner.width.saturating_sub(2), 1);
            buf.set_style(field, theme::dialog_input(paint));
            let visible = input_tail(input, field.width as usize);
            overlay_text(
                buf,
                inner,
                field.x,
                field_y,
                &visible,
                theme::dialog_input(paint),
            );
        }
    }
    if let Some(error) = error {
        let error_area = Rect::new(
            inner.x + 1,
            inner.y + 3,
            inner.width.saturating_sub(2),
            inner.height.saturating_sub(4),
        );
        Paragraph::new(error)
            .style(theme::dialog_error(paint))
            .wrap(Wrap { trim: true })
            .render(error_area, buf);
    }
    // Keyboard-first actions on the last inner row, recorded for the mouse.
    // Enter confirms every modal and Escape cancels it, so one shape covers
    // input dialogs and destructive confirms alike.
    let button_y = inner.y + inner.height.saturating_sub(1);
    let mut bx = inner.x + 1;
    let buttons = [
        (format!("↵ {confirm_label}"), HitTarget::DialogConfirm, true),
        (
            format!("Esc {}", copy::BUTTON_CANCEL),
            HitTarget::DialogCancel,
            false,
        ),
    ];
    for (text, target, primary) in buttons {
        let bw = Span::raw(text.as_str()).width() as u16;
        let available = inner.x.saturating_add(inner.width).saturating_sub(bx);
        let rect = Rect::new(bx, button_y, bw.min(available), 1);
        if rect.width == 0 {
            break;
        }
        let hovered =
            hover.is_some_and(|(hc, hr)| hr == rect.y && hc >= rect.x && hc < rect.x + rect.width);
        let style = if hovered || primary {
            theme::dialog_primary(paint)
        } else {
            theme::dialog_secondary(paint)
        };
        overlay_text(buf, inner, bx, button_y, &text, style);
        hits.push(rect, target);
        bx = bx.saturating_add(bw + 2);
    }
}

fn clear_area(buf: &mut Buffer, area: Rect) {
    for row in area.y..area.y + area.height {
        for col in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.reset();
            }
        }
    }
}

/// Number of trimmed, word-wrapped rows needed at `width`. This mirrors the
/// dialog paragraph closely enough to size it without depending on
/// Ratatui's unstable rendered-line introspection API.
fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = usize::from(width);
    if width == 0 {
        return 0;
    }
    let mut words = text.split_whitespace().peekable();
    if words.peek().is_none() {
        return 0;
    }

    let mut lines = 1usize;
    let mut column = 0usize;
    for word in words {
        let word_width = Span::raw(word).width();
        let separator = usize::from(column > 0);
        if column.saturating_add(separator).saturating_add(word_width) <= width {
            column += separator + word_width;
            continue;
        }
        if column > 0 {
            lines = lines.saturating_add(1);
        }
        lines = lines.saturating_add(word_width.saturating_sub(1) / width);
        column = word_width % width;
        if column == 0 {
            column = width;
        }
    }
    lines
}

/// Large, padded keybinding reference. The dialog owns only a scroll
/// offset; its rows come from the active router map rather than hardcoded
/// documentation.
fn paint_keybinds_dialog(
    scroll: u16,
    rows: &[crate::bindings::BindingHelp],
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    let Some((dialog_area, list_h)) = keybind_dialog_geometry(rows.len(), area) else {
        return;
    };
    clear_area(buf, dialog_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::pane_border_focused(paint))
        .style(theme::menu_row(paint));
    let inner = block.inner(dialog_area);
    block.render(dialog_area, buf);

    let left = inner.x + 2;
    let usable_w = inner.width.saturating_sub(4);
    overlay_text(
        buf,
        inner,
        left,
        inner.y,
        copy::KEYBINDS_TITLE,
        theme::menu_row(paint).add_modifier(Modifier::BOLD),
    );
    overlay_text(
        buf,
        inner,
        left,
        inner.y + 1,
        copy::KEYBINDS_HINT,
        theme::menu_hint(paint),
    );

    let list_y = inner.y + 3;
    let start = if list_h == 0 {
        0
    } else {
        usize::from(scroll.min(keybind_max_scroll(rows.len(), area)))
    };
    let key_w = rows
        .iter()
        .map(|row| Span::raw(row.keys.as_str()).width())
        .max()
        .unwrap_or(0)
        .min(usable_w.saturating_sub(4) as usize) as u16;
    for (line, row) in rows.iter().skip(start).take(list_h as usize).enumerate() {
        let y = list_y + line as u16;
        overlay_text(
            buf,
            inner,
            left,
            y,
            &row.keys,
            theme::pane_border_focused(paint),
        );
        overlay_text(
            buf,
            inner,
            left + key_w + 3,
            y,
            &row.action,
            theme::menu_row(paint),
        );
    }

    let footer_y = inner.y + inner.height.saturating_sub(1);
    let close = format!("↵ / Esc {}", copy::BUTTON_CLOSE);
    let close_w = Span::raw(close.as_str()).width() as u16;
    let close_rect = Rect::new(left, footer_y, close_w.min(usable_w), 1);
    let hovered = hover.is_some_and(|(x, y)| {
        y == close_rect.y && x >= close_rect.x && x < close_rect.x + close_rect.width
    });
    overlay_text(
        buf,
        inner,
        close_rect.x,
        close_rect.y,
        &close,
        if hovered {
            theme::dialog_primary(paint).add_modifier(Modifier::UNDERLINED)
        } else {
            theme::dialog_primary(paint)
        },
    );
    hits.push(close_rect, HitTarget::DialogCancel);

    if !rows.is_empty() && list_h > 0 {
        let end = (start + list_h as usize).min(rows.len());
        let progress = format!("{}–{} / {}", start + 1, end, rows.len());
        let progress_w = Span::raw(progress.as_str()).width() as u16;
        let x = inner
            .x
            .saturating_add(inner.width.saturating_sub(progress_w + 2));
        overlay_text(buf, inner, x, footer_y, &progress, theme::menu_hint(paint));
    }
}

fn keybind_dialog_geometry(row_count: usize, area: Rect) -> Option<(Rect, u16)> {
    if area.width < 8 || area.height < 6 {
        return None;
    }
    let width = area.width.saturating_sub(4).min(72);
    let wanted_height = u16::try_from(row_count.saturating_add(7)).unwrap_or(u16::MAX);
    let height = wanted_height.min(area.height.saturating_sub(2)).max(6);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let list_height = height.saturating_sub(2).saturating_sub(5);
    Some((dialog, list_height))
}

/// Largest meaningful keybinding-list offset for this terminal area.
pub fn keybind_max_scroll(row_count: usize, area: Rect) -> u16 {
    let Some((_, list_height)) = keybind_dialog_geometry(row_count, area) else {
        return 0;
    };
    if list_height == 0 {
        return 0;
    }
    u16::try_from(row_count.saturating_sub(list_height as usize)).unwrap_or(u16::MAX)
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
    active_pane: &str,
    expanded_workspaces: &std::collections::HashSet<String>,
    agent_order: &[String],
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    decoration: &DecorationSnapshot,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    buf.set_style(area, theme::chrome_panel(paint));
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(theme::pane_border(paint))
        .style(theme::chrome_panel(paint));
    let inner = block.inner(area);
    block.render(area, buf);
    hits.push(
        Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        ),
        HitTarget::SidebarDivider,
    );

    // Two cells of breathing room keep workspace and agent names away from
    // the outer edge and the resize border.
    let pad = 2.min(inner.width / 2);
    let content = Rect::new(
        inner.x + pad,
        inner.y,
        inner.width.saturating_sub(pad.saturating_mul(2)),
        inner.height,
    );
    let eye = if decoration.workspace_needs_attention() {
        " ◉"
    } else {
        ""
    };
    overlay_text(
        buf,
        content,
        content.x,
        content.y,
        "Workspaces",
        theme::sidebar_label(paint).add_modifier(Modifier::BOLD),
    );
    overlay_text(
        buf,
        content,
        content.x + "Workspaces".len() as u16,
        content.y,
        eye,
        theme::attention_eye(paint).patch(paint.bg_token(cyclops_theme::tokens::CHROME_PANEL)),
    );
    let mut y = content.y + 2;
    if !decoration.online {
        overlay_text(
            buf,
            content,
            content.x,
            y,
            "cyclopsd offline",
            theme::sidebar_row(paint),
        );
        y += 1;
    }
    let footer_y = inner.y + inner.height.saturating_sub(1);
    for (i, ws) in workspaces.iter().enumerate() {
        if y >= footer_y {
            break;
        }
        let expanded = expanded_workspaces.contains(&ws.session_id);
        let marker = if expanded { "▾" } else { "▸" };
        let style = if i == active {
            theme::sidebar_workspace_active(paint)
        } else {
            theme::sidebar_workspace(paint)
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        buf.set_style(row, style);
        overlay_text(
            buf,
            content,
            content.x,
            y,
            &format!("{marker} {} ({})", ws.name, ws.tab_count),
            style,
        );
        hits.push(
            row,
            HitTarget::SidebarRow {
                session_id: ws.session_id.clone(),
                session: ws.name.clone(),
            },
        );
        hits.push(
            Rect::new(content.x, y, 1.min(content.width), 1),
            HitTarget::SidebarDisclosure {
                session_id: ws.session_id.clone(),
            },
        );
        y += 1;

        if !expanded {
            continue;
        }
        for agent in decoration.agent_rows_for_window_ids(&ws.window_ids, agent_order) {
            if y >= footer_y {
                break;
            }
            let selected = i == active && agent.pane_id == active_pane;
            let row_style = if selected {
                theme::sidebar_row_active(paint)
            } else {
                theme::sidebar_row(paint)
            };
            let row = Rect::new(inner.x, y, inner.width, 1);
            buf.set_style(row, row_style);
            let name = DecorationSnapshot::sidebar_name(agent);
            let name_style = if agent.label.is_some() {
                row_style.patch(paint.role(name))
            } else {
                row_style
            };
            // Status leads the row, matching the compact roster shape
            // (`● Claude Code`). Unknown deliberately contributes no text.
            let mut x = content.x.saturating_add(3);
            if let Some(status) = DecorationSnapshot::primary_status(agent) {
                let status_style = if status.glyph == "⚠" {
                    theme::attention_eye(paint)
                } else {
                    paint.state(status.color_state)
                };
                overlay_text(buf, content, x, y, status.glyph, status_style);
                x = x.saturating_add(2);
            }
            overlay_text(buf, content, x, y, name, name_style);
            let order_key = DecorationSnapshot::agent_order_key(agent);
            hits.push(
                row,
                HitTarget::SidebarAgent {
                    workspace_id: ws.session_id.clone(),
                    pane_id: agent.pane_id.clone(),
                    order_key,
                },
            );
            y += 1;
        }
    }

    // Application menu at left; a matching compact create button anchors the
    // hierarchy at bottom-right without stealing the rest of the footer row.
    if inner.height >= 2 {
        let menu_row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let menu_width = u16::try_from(Span::raw(copy::APP_MENU_BUTTON).width())
            .unwrap_or(u16::MAX)
            .min(content.width);
        overlay_text(
            buf,
            content,
            content.x,
            menu_row.y,
            copy::APP_MENU_BUTTON,
            theme::sidebar_label(paint),
        );
        hits.push(
            Rect::new(content.x, menu_row.y, menu_width, 1),
            HitTarget::AppMenu,
        );
        let plus = " + ";
        let plus_width = u16::try_from(Span::raw(plus).width())
            .unwrap_or(u16::MAX)
            .min(content.width);
        let plus_x = content
            .x
            .saturating_add(content.width.saturating_sub(plus_width));
        overlay_text(
            buf,
            content,
            plus_x,
            menu_row.y,
            plus,
            theme::add_button(paint),
        );
        hits.push(
            Rect::new(plus_x, menu_row.y, plus_width, 1),
            HitTarget::NewWorkspaceButton,
        );
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

/// One cell of breathing room around the pane canvas. Compact separator
/// bands preserve one border for each sibling pane.
pub const PANE_MARGIN: u16 = 1;

/// Compact pane separation: each sibling keeps its own border, with no extra
/// blank cell between them. Both split directions therefore use two cells,
/// one less than the previous border/blank/border band.
pub const PANE_GAPS: PaneGaps = PaneGaps {
    columns: 2,
    rows: 2,
};

/// The rectangle occupied by pane content plus internal separators: the
/// canvas inset by [`PANE_MARGIN`] when there is room.
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

/// Grid size declared to tmux for the active tab. Separator overhead is
/// removed here and restored only as chrome by [`paint_window`].
pub fn tmux_client_size(canvas: Rect, tab: &TabModel) -> (u16, u16) {
    let inner = pane_canvas(canvas);
    let (gap_width, gap_height) = if tab.zoomed {
        (0, 0)
    } else {
        layout_gap_overhead(&tab.layout, PANE_GAPS)
    };
    (
        inner.width.saturating_sub(gap_width),
        inner.height.saturating_sub(gap_height),
    )
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
    let (slots, dividers) = if tab.zoomed {
        (
            vec![PaneSlot {
                pane_id: tab.active_pane.clone(),
                rect: inner,
                focused: true,
            }],
            Vec::new(),
        )
    } else {
        let geometry = layout_geometry(&tab.layout, inner, &tab.active_pane, PANE_GAPS);
        (geometry.slots, geometry.dividers)
    };
    for slot in &slots {
        paint_pane_slot(slot, runtimes, buf, paint, ctx);
    }
    // Every pane owns a quiet boundary. Paint the focused pane last so its
    // accent wins where nested borders intersect.
    for slot in slots.iter().filter(|slot| !slot.focused) {
        paint_pane_frame(slot, canvas, buf, paint, ctx);
    }
    for slot in slots.iter().filter(|slot| slot.focused) {
        paint_pane_frame(slot, canvas, buf, paint, ctx);
    }
    // Shared pane borders are resize handles. Put divider regions above the
    // generic frame regions, then restore the visibly overlaid controls as
    // the most specific hit targets.
    push_divider_hits(&dividers, ctx.hits);
    for slot in &slots {
        push_pane_overlay_hits(slot, canvas, ctx.decoration, ctx.hits);
    }
    if let Some(drag) = ctx.drag.filter(|d| d.is_active()) {
        paint_drag_preview(drag, buf, paint);
    }
}

/// Divider gap cells stay grabbable for resize even though they paint as
/// plain gutter.
fn push_divider_hits(dividers: &[DividerSeg], hits: &mut HitMap) {
    for seg in dividers {
        hits.push(
            seg.rect,
            HitTarget::Divider {
                pane_id: seg.pane_id.clone(),
                dir: seg.dir,
            },
        );
    }
}

/// A pane border in the gutter cells around `rect`, clipped to `bounds`.
/// The margin and explicit separators guarantee those cells are chrome,
/// never another pane's content.
fn paint_pane_border(rect: Rect, bounds: Rect, buf: &mut Buffer, style: Style) {
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
}

/// Paint one pane's border, optional named-agent chrome, and split controls.
/// Unnamed panes stay textually quiet; their muted boundary still makes the
/// layout legible.
fn paint_pane_frame(
    slot: &PaneSlot,
    bounds: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    let vis = slot.rect;
    let border_style = if slot.focused {
        theme::pane_border_focused(paint)
    } else {
        theme::pane_border(paint)
    };
    paint_pane_border(vis, bounds, buf, border_style);

    let left = vis.x.saturating_sub(1).max(bounds.x);
    let top = vis.y.saturating_sub(1).max(bounds.y);
    let right = (vis.x + vis.width).min(bounds.x + bounds.width - 1);
    let bottom = (vis.y + vis.height).min(bounds.y + bounds.height - 1);
    ctx.hits.push(
        Rect::new(left, top, right.saturating_sub(left).saturating_add(1), 1),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push(
        Rect::new(
            left,
            bottom,
            right.saturating_sub(left).saturating_add(1),
            1,
        ),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push(
        Rect::new(left, top, 1, bottom.saturating_sub(top).saturating_add(1)),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push(
        Rect::new(right, top, 1, bottom.saturating_sub(top).saturating_add(1)),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );

    // Controls live in the border instead of overwriting the first row of
    // the child TUI. They remain available on unfocused panes.
    let controls = pane_controls(slot, bounds);
    let control_left = controls.map_or(right, |controls| controls.split_right.x);
    if let Some(controls) = controls {
        overlay_text(
            buf,
            bounds,
            controls.split_right.x,
            controls.split_right.y,
            "[|]",
            border_style,
        );
        overlay_text(
            buf,
            bounds,
            controls.split_down.x,
            controls.split_down.y,
            "[-]",
            border_style,
        );
    }

    let Some(decoration) = ctx
        .decoration
        .pane(&slot.pane_id)
        .filter(|decoration| decoration.label.is_some() || decoration.manifest.is_some())
    else {
        return;
    };
    let label = DecorationSnapshot::sidebar_name(decoration);
    let Some(title_bounds) = pane_title_rect(slot, bounds, control_left) else {
        return;
    };
    let status = DecorationSnapshot::primary_status(decoration);
    let shown_state = status.map(|status| {
        let full = format!("{} {}", status.glyph, status.word);
        let full_suffix = 3usize.saturating_add(Span::raw(full.as_str()).width());
        if slot.focused
            && Span::raw(label).width().saturating_add(full_suffix)
                <= usize::from(title_bounds.width)
        {
            full
        } else {
            status.glyph.to_string()
        }
    });
    let suffix_width = shown_state
        .as_ref()
        .map(|state| {
            3u16.saturating_add(u16::try_from(Span::raw(state).width()).unwrap_or(u16::MAX))
        })
        .unwrap_or(0);
    let label_width = u16::try_from(Span::raw(label).width()).unwrap_or(u16::MAX);
    let label_budget = if suffix_width > 0 && title_bounds.width > suffix_width {
        label_width.min(title_bounds.width - suffix_width)
    } else {
        label_width.min(title_bounds.width)
    };
    let label_style = if decoration.label.is_some() {
        paint.role(label).add_modifier(Modifier::BOLD)
    } else {
        border_style.add_modifier(Modifier::BOLD)
    };
    let label_bounds = Rect::new(title_bounds.x, top, label_budget, 1);
    overlay_text(buf, label_bounds, label_bounds.x, top, label, label_style);
    let Some(shown_state) = shown_state.filter(|_| title_bounds.width > suffix_width) else {
        return;
    };
    let mut x = title_bounds.x.saturating_add(label_budget);
    overlay_text(buf, title_bounds, x, top, " · ", border_style);
    x = x.saturating_add(3);
    overlay_text(
        buf,
        title_bounds,
        x,
        top,
        &shown_state,
        if shown_state.starts_with('⚠') {
            theme::attention_eye(paint)
        } else {
            paint.state(
                status
                    .map(|status| status.color_state)
                    .unwrap_or(decoration.state),
            )
        },
    );
}

fn pane_title_rect(slot: &PaneSlot, bounds: Rect, control_left: u16) -> Option<Rect> {
    let top = slot.rect.y.saturating_sub(1).max(bounds.y);
    let title_left = slot.rect.x.saturating_add(1);
    (title_left < control_left).then(|| Rect::new(title_left, top, control_left - title_left, 1))
}

#[derive(Debug, Clone, Copy)]
struct PaneControls {
    split_right: Rect,
    split_down: Rect,
}

fn pane_controls(slot: &PaneSlot, bounds: Rect) -> Option<PaneControls> {
    let vis = slot.rect;
    let left = vis.x.saturating_sub(1).max(bounds.x);
    let top = vis.y.saturating_sub(1).max(bounds.y);
    let right = (vis.x + vis.width).min(bounds.x + bounds.width - 1);
    if right.saturating_sub(left) < 8 {
        return None;
    }

    let split_down = Rect::new(right.saturating_sub(3), top, 3, 1);
    let split_right = Rect::new(split_down.x.saturating_sub(3), top, 3, 1);
    Some(PaneControls {
        split_right,
        split_down,
    })
}

fn push_pane_overlay_hits(
    slot: &PaneSlot,
    bounds: Rect,
    decoration: &DecorationSnapshot,
    hits: &mut HitMap,
) {
    let right = (slot.rect.x + slot.rect.width).min(bounds.x + bounds.width - 1);
    let controls = pane_controls(slot, bounds);
    let control_left = controls.map_or(right, |controls| controls.split_right.x);
    if let Some((pane, rect)) = decoration
        .pane(&slot.pane_id)
        .filter(|pane| pane.label.is_some())
        .zip(pane_title_rect(slot, bounds, control_left))
    {
        let target = if pane.needs_attention {
            HitTarget::AttentionIndicator {
                pane_id: slot.pane_id.clone(),
            }
        } else {
            HitTarget::PaneFrame {
                pane_id: slot.pane_id.clone(),
            }
        };
        hits.push(rect, target);
    }
    if let Some(controls) = controls {
        hits.push(
            controls.split_right,
            HitTarget::PaneSplitRight {
                pane_id: slot.pane_id.clone(),
            },
        );
        hits.push(
            controls.split_down,
            HitTarget::PaneSplitDown {
                pane_id: slot.pane_id.clone(),
            },
        );
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
            (copy::MENU_KEYBINDS, BindingAction::ShowKeybinds),
            (copy::MENU_DETACH, BindingAction::Detach),
        ],
        MenuState::ContextMenu { .. } => vec![
            (copy::MENU_NAME_PANE, BindingAction::NamePane),
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
    clear_area(buf, menu_area);
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
        DragTarget::Workspace { .. } | DragTarget::Agent { .. } => "⇅",
        DragTarget::Sidebar => "↔",
    };
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(hint);
        cell.set_style(style);
    }
}

/// Slide-out event stream from daemon attention items.
pub fn paint_event_stream(lines: &[String], area: Rect, buf: &mut Buffer, paint: &Paint) {
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
    let text: Vec<Line> = if lines.is_empty() {
        vec![Line::from(Span::styled(
            copy::EVENT_STREAM_EMPTY,
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

    /// Two stacked panes whose tmux grid plus compact divider fills the
    /// 38x9 pane canvas used by the frame tests.
    fn two_pane_tab() -> TabModel {
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
        // Each stacked pane keeps its border, but the old blank row between
        // those borders is gone.
        assert_eq!(buf[(0, 6)].symbol(), "╰", "first pane bottom corner");
        assert_eq!(buf[(5, 6)].symbol(), "─", "first pane bottom border");
        assert_eq!(buf[(5, 7)].symbol(), "─", "second pane top border");
        assert_ne!(
            buf[(5, 6)].fg,
            buf[(5, 7)].fg,
            "focused and inactive borders use distinct theme tokens"
        );
        // The compact two-cell band stays grabbable for resize.
        assert!(matches!(hits.hit(20, 6), Some(HitTarget::Divider { .. })));
        assert!(matches!(hits.hit(20, 7), Some(HitTarget::Divider { .. })));
        // The bottom margin carries the second pane's muted border.
        assert_eq!(buf[(20, 11)].symbol(), "─", "every pane has a border");
    }

    #[test]
    fn pane_canvas_insets_by_the_margin() {
        assert_eq!(pane_canvas(Rect::new(0, 1, 40, 11)), Rect::new(1, 2, 38, 9));
        // Too small to inset: the canvas is used as-is.
        assert_eq!(pane_canvas(Rect::new(0, 0, 2, 2)), Rect::new(0, 0, 2, 2));
    }

    #[test]
    fn client_size_reserves_the_compact_internal_gutter() {
        let tab = two_pane_tab();
        assert_eq!(tmux_client_size(Rect::new(0, 1, 40, 11), &tab), (38, 8));
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

    #[test]
    fn sidebar_rows_render_and_hit_test_aligned() {
        let workspaces = vec![
            WorkspaceRow {
                session_id: "$0".into(),
                name: "cyclops".into(),
                tab_count: 2,
                active: true,
                window_ids: vec!["@0".into()],
            },
            WorkspaceRow {
                session_id: "$1".into(),
                name: "website".into(),
                tab_count: 1,
                active: false,
                window_ids: vec!["@1".into()],
            },
        ];
        let backend = TestBackend::new(20, 8);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &[],
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
        assert!(flat.contains('▾'), "active row should be expanded");
        assert_eq!(buf[(0, 3)].symbol(), " ", "left padding cell one");
        assert_eq!(buf[(1, 3)].symbol(), " ", "left padding cell two");
        // The title row is followed by a spacer, then the offline note,
        // so workspaces paint on rows 3 and 4.
        assert!(matches!(
            hits.hit(3, 3),
            Some(HitTarget::SidebarRow { session, .. }) if session == "cyclops"
        ));
        assert!(matches!(
            hits.hit(3, 4),
            Some(HitTarget::SidebarRow { session, .. }) if session == "website"
        ));
        assert!(matches!(
            hits.hit(2, 3),
            Some(HitTarget::SidebarDisclosure { session_id }) if session_id == "$0"
        ));
        // Bottom row carries distinct menu and create buttons.
        assert!(matches!(hits.hit(2, 7), Some(HitTarget::AppMenu)));
        assert!(matches!(
            hits.hit(15, 7),
            Some(HitTarget::NewWorkspaceButton)
        ));
        assert!(flat.contains("menu"), "menu button should render: {flat}");
        assert!(flat.contains('+'), "create button should render: {flat}");
    }

    #[test]
    fn sidebar_agents_use_hierarchy_display_names_and_compact_status() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            active: true,
            window_ids: vec!["@0".into()],
        }];
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
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Unknown,
                needs_attention: false,
            },
        );
        decoration.panes.insert(
            "%1".into(),
            PaneDecoration {
                pane_id: "%1".into(),
                window_id: "@0".into(),
                label: None,
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Working,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(28, 8);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        term.draw(|frame| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &["pane:%1".into(), "name:reviewer".into()],
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut hits,
                &decoration,
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("● Claude Code"), "detected agent: {flat}");
        assert!(flat.contains("reviewer"), "named pane: {flat}");
        assert!(
            !flat.contains("unknown"),
            "unknown stays diagnostic: {flat}"
        );
        assert!(!flat.contains("? reviewer"), "unknown has no glyph: {flat}");
        assert!(
            matches!(
                hits.hit(6, 3),
                Some(HitTarget::SidebarAgent { pane_id, .. }) if pane_id == "%1"
            ),
            "persisted agent order should put Claude first"
        );
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
            hits.hit(7, 4),
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
        assert_ne!(
            buf[(7, 4)].bg,
            RtColor::Reset,
            "menu surface should use the theme background"
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
        assert!(flat.contains("↵ Create"), "confirm affordance: {flat}");
        assert!(flat.contains("Esc Cancel"), "cancel affordance: {flat}");
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
    fn empty_dialog_cursor_starts_at_the_input_fields_first_cell() {
        let backend = TestBackend::new(50, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|frame| {
            paint_dialog(
                &Dialog::NewTab {
                    buffer: String::new(),
                },
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut hits,
                None,
            );
        })
        .unwrap();

        let width = (Span::raw(copy::NEW_TAB_HINT).width() as u16 + 4).max(40);
        let left = (50 - width) / 2;
        // Border, then one intentional field inset. There is no second,
        // accidental indent inside the input field.
        assert_eq!(term.backend().buffer()[(left + 2, 4)].symbol(), "▏");
    }

    #[test]
    fn pane_name_errors_wrap_without_hiding_the_actionable_ending() {
        let error = cyclops_proto::label::refusal("admin").expect("reserved label");
        let backend = TestBackend::new(72, 15);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|frame| {
            paint_dialog(
                &Dialog::NamePane {
                    pane_id: "%0".into(),
                    buffer: "admin".into(),
                    error: Some(error.clone()),
                },
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut hits,
                None,
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("Pick another name, e.g. lead."),
            "the useful end of the refusal should remain visible: {flat}"
        );
        assert!(
            hits.regions()
                .iter()
                .any(|region| region.target == HitTarget::DialogConfirm),
            "wrapping must leave the save button reachable"
        );
    }

    #[test]
    fn keybind_dialog_is_padded_themed_and_scrolls_to_the_last_row() {
        let rows: Vec<_> = (0..20)
            .map(|index| crate::bindings::BindingHelp {
                keys: format!("Ctrl+{index}"),
                action: format!("Action {index}"),
            })
            .collect();
        let backend = TestBackend::new(50, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|frame| {
            paint_dialog(
                &Dialog::Keybinds {
                    scroll: u16::MAX,
                    rows: rows.clone(),
                },
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut hits,
                None,
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        let flat = flatten(buf);
        assert!(
            flat.contains("Action 19"),
            "last row should be reachable: {flat}"
        );
        assert!(
            !flat.contains("Action 0"),
            "scroll should move the first row away"
        );
        assert_ne!(buf[(4, 3)].bg, RtColor::Reset, "modal owns a themed ground");
        assert!(matches!(
            hits.regions().last().map(|region| &region.target),
            Some(HitTarget::DialogCancel)
        ));
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
        assert!(flat.contains("↵ Confirm"), "confirm key is visible: {flat}");
        assert!(
            flat.contains("Esc Cancel"),
            "cancel action is visible: {flat}"
        );
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
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Working,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(40, 12);
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
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("working"), "badge word should render: {flat}");
        assert!(matches!(
            hits.hit(3, 0),
            Some(HitTarget::PaneFrame { pane_id }) if pane_id == "%0"
        ));
    }

    #[test]
    fn unknown_is_not_painted_in_primary_pane_chrome() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let node = parse_layout("0000,30x5,0,0,0").unwrap();
        let tab = TabModel {
            window_id: "@0".into(),
            name: "1".into(),
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
                label: Some("planner".into()),
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Unknown,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(32, 7);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|frame| {
            let mut runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &mut runtimes,
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("planner"));
        assert!(!flat.contains("unknown"));
        assert!(!flat.contains("? "));
    }

    #[test]
    fn inactive_pane_chrome_keeps_status_compact() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let tab = two_pane_tab();
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%1".into(),
            PaneDecoration {
                pane_id: "%1".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Idle,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|frame| {
            let mut runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &mut runtimes,
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("reviewer · ○"), "compact chrome: {flat}");
        assert!(!flat.contains("reviewer · ○ idle"), "inactive word: {flat}");
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
                manifest_display_name: Some("Claude Code".into()),
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
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(row.contains("[|][-]"), "controls stay intact: {row:?}");
        assert!(matches!(
            hits.hit(6, 0),
            Some(HitTarget::PaneSplitRight { .. })
        ));
        assert!(matches!(
            hits.hit(9, 0),
            Some(HitTarget::PaneSplitDown { .. })
        ));
    }

    #[test]
    fn compact_pane_title_keeps_the_state_glyph_visible() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let node = parse_layout("0000,18x3,0,0,0").unwrap();
        let tab = TabModel {
            window_id: "@0".into(),
            name: "1".into(),
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
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::BlockedPermission,
                needs_attention: true,
            },
        );
        let backend = TestBackend::new(20, 5);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|frame| {
            let mut runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &mut runtimes,
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();

        let row: String = (0..20)
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row.contains(" · ⚠"),
            "compact state remains visible: {row:?}"
        );
        assert!(row.contains("[|][-]"), "controls remain intact: {row:?}");
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
        let slots = layout_geometry(
            &two_pane_tab().layout,
            Rect::new(0, 0, 40, 11),
            "%0",
            PANE_GAPS,
        )
        .slots;
        assert_eq!(slots.len(), 2);
        assert!(slots[0].focused);
        assert!(!slots[1].focused);
    }
}
