//! Paint pane grids and chrome into a Ratatui buffer.
//!
//! Panes render at their tmux cell coordinates 1:1. The workspace subtracts
//! the extra cells used by its separator bands before declaring
//! the client size, then restores those cells only as chrome. Nothing scales;
//! a runtime grid lands on exactly the cells tmux gave the pane.

#![allow(clippy::too_many_arguments)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use cyclops_ui::{Entry, EntryKind, Record};

use crate::bindings::BindingAction;
use crate::copy;
use crate::decoration::DecorationSnapshot;
use crate::dialog::Dialog;
use crate::drag::{DragState, DragTarget};
use crate::input::mouse::{HitMap, HitTarget, MenuState, PaneGeometry};
use crate::layout::{layout_gap_overhead, layout_geometry, DividerSeg, PaneGaps};
use crate::model::{PaneSlot, RuntimeRegistry, TabModel, WorkspaceRow};
use crate::resilience::LinkState;
use crate::runtime::{CellPos, Color, GridCell, PaneRuntime};
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
    // Rule 11's compact surfaces (sidebar rows, inactive pane borders) show
    // the glyph alone; this dialog is the one place that spells the whole
    // vocabulary out, once, so a reader never has to guess what a bare ●
    // or ✕ meant.
    overlay_text(
        buf,
        inner,
        left,
        inner.y + 2,
        copy::STATE_GLYPH_LEGEND,
        theme::menu_hint(paint),
    );

    let list_y = inner.y + 4;
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
    // Title, hint, glyph legend, one blank line before the list, one blank
    // line after it, and the footer: 6 fixed rows around the scrollable list.
    let wanted_height = u16::try_from(row_count.saturating_add(8)).unwrap_or(u16::MAX);
    let height = wanted_height.min(area.height.saturating_sub(2)).max(6);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let list_height = height.saturating_sub(2).saturating_sub(6);
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
    hover: Option<(u16, u16)>,
    drag: Option<&DragState>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // A live workspace-row drag: which row is grabbed (dimmed in the loop
    // below) and, once the pointer is actually over this sidebar, which
    // slot it currently previews.
    let dragging_session = drag
        .filter(|d| d.is_active())
        .and_then(|d| match &d.target {
            DragTarget::Workspace { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        });
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
        // The color cue (dim) is redundant with a non-color one (the grip
        // glyph prefix) — see rule 11 and `theme::sidebar_row_dragging`.
        let dragging = dragging_session == Some(ws.session_id.as_str());
        let style = if dragging {
            theme::sidebar_row_dragging(paint)
        } else if i == active {
            theme::sidebar_workspace_active(paint)
        } else {
            theme::sidebar_workspace(paint)
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        buf.set_style(row, style);
        let grip = if dragging { "⇅ " } else { "" };
        overlay_text(
            buf,
            content,
            content.x,
            y,
            &format!("{grip}{marker} {} ({})", ws.name, ws.tab_count),
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

    // The live drop preview: a full-width rule at the boundary the drag
    // currently previews, painted only once the pointer is actually over
    // this sidebar — a pointer that has strayed elsewhere (a pane, the tab
    // bar) shows no rule, matching that a release there leaves order
    // unchanged. Terminal rows have no sub-row resolution, so "between two
    // rows" is approximated as the row itself, repainted as a rule for as
    // long as the drag stays live.
    if let Some(drag) = drag.filter(|d| d.is_active() && dragging_session.is_some()) {
        if area.contains(ratatui::layout::Position::from(drag.current)) {
            let blocks = hits.workspace_blocks();
            let slot = crate::drag::slot_for_row(&blocks, drag.current.1);
            if let Some(rule_y) = crate::drag::boundary_row(&blocks, slot) {
                paint_insertion_rule(buf, inner, rule_y, paint);
            }
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
        // The button keeps one width whether or not it is pointed at, so
        // the target never moves out from under the mouse that found it.
        let hovered = hover.is_some_and(|(hover_col, hover_row)| {
            hover_row == menu_row.y
                && hover_col >= plus_x
                && hover_col < plus_x.saturating_add(plus_width)
        });
        if hovered {
            // Say what it makes, in the gutter the footer already leaves
            // between the menu label and the button. Skipped rather than
            // truncated when the sidebar is too narrow: half a word next to
            // a lit button teaches nothing.
            let hint_width =
                u16::try_from(Span::raw(copy::NEW_WORKSPACE_HINT).width()).unwrap_or(u16::MAX);
            let gutter = plus_x.saturating_sub(content.x.saturating_add(menu_width));
            if hint_width < gutter {
                overlay_text(
                    buf,
                    content,
                    plus_x.saturating_sub(hint_width),
                    menu_row.y,
                    copy::NEW_WORKSPACE_HINT,
                    theme::sidebar_label(paint),
                );
            }
        }
        overlay_text(
            buf,
            content,
            plus_x,
            menu_row.y,
            plus,
            if hovered {
                theme::add_button_hover(paint)
            } else {
                theme::add_button(paint)
            },
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
    runtimes: &RuntimeRegistry,
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
    let frames = outer_frames(&slots);
    // Every pane owns a quiet boundary. Paint the focused pane last so its
    // accent wins where nested borders intersect.
    for (slot, frame) in slots.iter().zip(&frames).filter(|(slot, _)| !slot.focused) {
        paint_pane_frame(slot, *frame, canvas, buf, paint, ctx);
    }
    for (slot, frame) in slots.iter().zip(&frames).filter(|(slot, _)| slot.focused) {
        paint_pane_frame(slot, *frame, canvas, buf, paint, ctx);
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

/// The border box for each slot: its own rect, grown to the shared outer
/// edge on any axis where no sibling lies beyond it.
///
/// The pane area has to read as one rectangle, and without this it does
/// not. tmux gives every column the same height, but a column expands one
/// screen cell per split into two (`PANE_GAPS`), so a column carrying three
/// panes needs more chrome rows than a column carrying one.
/// [`layout_gap_overhead`] reserves that difference from the deepest branch,
/// which leaves every shallower branch finishing that many cells short and
/// the deepest one looking like it overhangs the rest.
///
/// Only the border moves. Leaf content rects stay exactly the size tmux
/// reported, because a live child terminal maps one runtime cell to one
/// screen cell and cropping or stretching it would corrupt what it drew —
/// the surplus simply becomes gutter inside the box, which is already how
/// this renderer treats every cell tmux does not own.
fn outer_frames(slots: &[PaneSlot]) -> Vec<Rect> {
    let edge_right = slots
        .iter()
        .map(|slot| slot.rect.x + slot.rect.width)
        .max()
        .unwrap_or(0);
    let edge_bottom = slots
        .iter()
        .map(|slot| slot.rect.y + slot.rect.height)
        .max()
        .unwrap_or(0);
    slots
        .iter()
        .map(|slot| {
            let own = slot.rect;
            let shares_rows =
                |other: &Rect| other.y < own.y + own.height && own.y < other.y + other.height;
            let shares_cols =
                |other: &Rect| other.x < own.x + own.width && own.x < other.x + other.width;
            let has_pane_below = slots
                .iter()
                .any(|o| o.rect.y >= own.y + own.height && shares_cols(&o.rect));
            let has_pane_beyond = slots
                .iter()
                .any(|o| o.rect.x >= own.x + own.width && shares_rows(&o.rect));
            Rect::new(
                own.x,
                own.y,
                if has_pane_beyond {
                    own.width
                } else {
                    edge_right.saturating_sub(own.x)
                },
                if has_pane_below {
                    own.height
                } else {
                    edge_bottom.saturating_sub(own.y)
                },
            )
        })
        .collect()
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
        let selection = ctx.selection.filter(|s| s.pane_id == slot.pane_id);
        paint_pane_cells(
            runtime,
            selection,
            vis,
            buf,
            base,
            theme::selection_highlight(paint),
        );
        if slot.focused {
            let cur = runtime.cursor();
            if cur.visible && runtime.at_tail() && cur.col < vis.width && cur.row < vis.height {
                ctx.cursor = Some((vis.x + cur.col, vis.y + cur.row));
            }
        }
    } else {
        fill_blank(vis, buf, base);
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
    frame: Rect,
    bounds: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    // The frame, not the content rect: a box grown out to the shared edge
    // must stay grabbable over the whole boundary it actually draws.
    let vis = frame;
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

/// One pass from engine cells to buffer cells: the R1 design. There is no
/// intermediate full-grid copy, and the selection highlight is decided per
/// cell in the same visit instead of a second walk over a mirrored grid.
fn paint_pane_cells(
    runtime: &PaneRuntime,
    selection: Option<&Selection>,
    area: Rect,
    buf: &mut Buffer,
    base: Style,
    highlight: Style,
) {
    let range = selection.map(|sel| {
        if sel.from.row < sel.to.row || (sel.from.row == sel.to.row && sel.from.col <= sel.to.col) {
            (sel.from, sel.to)
        } else {
            (sel.to, sel.from)
        }
    });
    runtime.for_each_visible_cell(|col, row, cell| {
        if col >= area.width || row >= area.height {
            return;
        }
        let ch = if cell.wide_spacer || cell.ch == '\0' {
            ' '
        } else {
            cell.ch
        };
        let style = match range {
            Some((from, to)) if in_selection(col, row, from, to) => highlight,
            _ => cell_style(&cell, base),
        };
        if let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) {
            dst.set_char(ch);
            dst.set_style(style);
        }
    });
    // The grid can be smaller than the slot during a resize transient, and
    // every slot cell must repaint over the previous frame.
    let (gcols, grows) = runtime.size();
    for row in 0..area.height {
        for col in 0..area.width {
            if col >= gcols || row >= grows {
                if let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) {
                    dst.set_char(' ');
                    dst.set_style(base);
                }
            }
        }
    }
}

/// Whether one cell sits inside a normalized selection range. Middle rows
/// select edge to edge; the end rows clip at the anchor columns.
fn in_selection(col: u16, row: u16, from: CellPos, to: CellPos) -> bool {
    (row > from.row || (row == from.row && col >= from.col))
        && (row < to.row || (row == to.row && col <= to.col))
}

fn fill_blank(area: Rect, buf: &mut Buffer, base: Style) {
    for row in 0..area.height {
        for col in 0..area.width {
            if let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) {
                dst.set_char(' ');
                dst.set_style(base);
            }
        }
    }
}

/// The workspace-reorder drop indicator: a full-width accent rule at row
/// `y`, spanning `area`'s usable width. Called only while a workspace-row
/// drag is live and the pointer sits over the sidebar — see the call site
/// in [`paint_sidebar`].
fn paint_insertion_rule(buf: &mut Buffer, area: Rect, y: u16, paint: &Paint) {
    if area.width == 0 || y < area.y || y >= area.y + area.height {
        return;
    }
    let style = theme::drag_insertion_rule(paint);
    let rule: String = "─".repeat(area.width as usize);
    buf.set_stringn(area.x, y, &rule, area.width as usize, style);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoration::DecorationSnapshot;
    use crate::dialog::Dialog;
    use crate::layout::{parse_layout, resolve_layout};
    use crate::model::{RuntimeRegistry, TabModel, WorkspaceRow};
    use crate::resilience::LinkState;
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
        let mut rt = crate::runtime::PaneRuntime::new(5, 2);
        rt.feed(b"X");
        let backend = TestBackend::new(5, 2);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            paint_pane_cells(
                &rt,
                None,
                f.area(),
                f.buffer_mut(),
                theme::pane_cell(&theme),
                theme::selection_highlight(&theme),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "X");
        assert_eq!(buf[(1, 0)].symbol(), " ", "blank cells repaint as spaces");
    }

    #[test]
    fn gutter_ring_and_tab_bar_render() {
        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();

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

    fn frame_slot(pane_id: &str, rect: Rect) -> PaneSlot {
        PaneSlot {
            pane_id: pane_id.into(),
            rect,
            focused: false,
        }
    }

    /// A column split three ways spends more rows on separator bands than
    /// its single-pane sibling, so tmux's equal columns paint unequal boxes
    /// and the deep one reads as overhanging the rest.
    #[test]
    fn outermost_boxes_close_on_one_shared_bottom_edge() {
        let slots = vec![
            frame_slot("%0", Rect::new(0, 0, 40, 18)),
            frame_slot("%1", Rect::new(42, 0, 40, 5)),
            frame_slot("%2", Rect::new(42, 7, 40, 5)),
            frame_slot("%3", Rect::new(42, 14, 40, 6)),
        ];
        let frames = outer_frames(&slots);

        let shallow = frames[0].y + frames[0].height;
        let deep = frames[3].y + frames[3].height;
        assert_eq!(
            shallow, deep,
            "the single-pane column must close on the same row as the three-pane column"
        );
        assert_eq!(
            frames[0].width, 40,
            "a pane with a sibling beyond it keeps its own right edge"
        );
        assert_eq!(
            frames[1].height, 5,
            "a pane with a sibling below it keeps its own bottom edge"
        );
        for (slot, frame) in slots.iter().zip(&frames) {
            assert!(
                frame.width >= slot.rect.width && frame.height >= slot.rect.height,
                "a frame only ever grows past its content, never crops it"
            );
        }
    }

    /// The identical bug on the other axis: a row split three ways beside a
    /// single pane must still close on one right edge.
    #[test]
    fn outermost_boxes_close_on_one_shared_right_edge() {
        let slots = vec![
            frame_slot("%0", Rect::new(0, 0, 18, 40)),
            frame_slot("%1", Rect::new(0, 42, 5, 40)),
            frame_slot("%2", Rect::new(7, 42, 5, 40)),
            frame_slot("%3", Rect::new(14, 42, 6, 40)),
        ];
        let frames = outer_frames(&slots);

        assert_eq!(
            frames[0].x + frames[0].width,
            frames[3].x + frames[3].width,
            "both rows must end on the same column"
        );
        assert_eq!(
            frames[1].width, 5,
            "a pane with a sibling beyond it keeps its own right edge"
        );
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

    /// The create button is a bare glyph at rest, so the mouse has to be
    /// what explains it: pointing at it fills the button and names what it
    /// makes, and the target must not move while being pointed at.
    #[test]
    fn sidebar_create_button_answers_the_mouse() {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            active: true,
            window_ids: vec!["@0".into()],
        }];
        let theme = Paint::for_test();
        let expanded = std::collections::HashSet::from(["$0".to_string()]);

        let draw = |hover: Option<(u16, u16)>| {
            let mut term = Terminal::new(TestBackend::new(20, 8)).unwrap();
            let mut hits = HitMap::default();
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
                    hover,
                    None,
                );
            })
            .unwrap();
            let buf = term.backend().buffer().clone();
            (buf, hits)
        };

        let (rest_buf, rest_hits) = draw(None);
        let plus = (0..rest_buf.area.width)
            .flat_map(|x| (0..rest_buf.area.height).map(move |y| (x, y)))
            .find(|&(x, y)| matches!(rest_hits.hit(x, y), Some(HitTarget::NewWorkspaceButton)))
            .expect("the sidebar paints a create button");

        let (hot_buf, hot_hits) = draw(Some(plus));
        assert_eq!(
            hot_hits.hit(plus.0, plus.1).cloned(),
            rest_hits.hit(plus.0, plus.1).cloned(),
            "the button must not move out from under the mouse that found it"
        );
        assert_ne!(
            hot_buf[plus].style(),
            rest_buf[plus].style(),
            "pointing at the create button must change how it paints"
        );
        assert!(
            flatten(&hot_buf).contains(copy::NEW_WORKSPACE_HINT),
            "hovering should name what the button makes: {}",
            flatten(&hot_buf)
        );
        assert!(
            !flatten(&rest_buf).contains(copy::NEW_WORKSPACE_HINT),
            "the hint belongs to hover, not to the resting sidebar"
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
                None,
                None,
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

    /// A live workspace-row drag must (1) mark the grabbed row with a
    /// non-color cue on top of the color one, and (2) paint a full-width
    /// rule at the exact boundary the drag currently previews — the same
    /// slot math `commit_drag_drop` uses to resolve the drop, so what the
    /// user watches while dragging is what actually lands.
    #[test]
    fn dragging_a_workspace_row_dims_it_and_paints_the_previewed_rule() {
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
        // Both collapsed: rows 3 ($0) and 4 ($1), matching
        // `sidebar_rows_render_and_hit_test_aligned`.
        let expanded = std::collections::HashSet::new();
        let theme = Paint::for_test();

        let render = |drag: Option<&DragState>| {
            let backend = TestBackend::new(20, 8);
            let mut term = Terminal::new(backend).unwrap();
            let mut hits = HitMap::default();
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
                    None,
                    drag,
                );
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let at_rest = render(None);

        // $1 (website, row 4) is picked up and dragged onto $0's row (3) —
        // the rule should preview inserting before $0.
        let mut drag = DragState::on_down(
            DragTarget::Workspace {
                session_id: "$1".into(),
                session: "website".into(),
            },
            3,
            4,
        );
        drag.on_move(3, 3);
        assert!(drag.is_active(), "past the 1-cell sidebar row threshold");
        let dragging = render(Some(&drag));

        // (1) The grabbed row keeps its name but gains a non-color grip
        // glyph and a materially different style than at rest — color
        // alone never carries this.
        let row4 = |buf: &Buffer| {
            (0..buf.area.width)
                .map(|x| buf[(x, 4)].symbol().to_string())
                .collect::<String>()
        };
        assert!(
            row4(&dragging).contains('⇅'),
            "the grabbed row shows a non-color marker glyph: {}",
            row4(&dragging)
        );
        assert!(
            row4(&dragging).contains("website"),
            "the grabbed row's own name stays visible while dragging"
        );
        assert_ne!(
            dragging[(2, 4)].style(),
            at_rest[(2, 4)].style(),
            "the grabbed row's style must change while dragging"
        );

        // (2) The rule paints across the sidebar's usable width at row 3 —
        // the previewed boundary — and nowhere else.
        let inner_width = 19; // area width 20 minus the 1-cell right border
        for x in 0..inner_width {
            assert_eq!(
                dragging[(x, 3)].symbol(),
                "─",
                "the rule should span the full sidebar width at column {x}"
            );
        }
        assert_ne!(
            dragging[(inner_width, 3)].symbol(),
            "─",
            "the rule must not paint over the sidebar's own border column"
        );
        // Rows other than the previewed boundary are unaffected by the
        // rule (row 4 still reads as the grabbed row's own text, not a
        // second copy of the line).
        assert_ne!(dragging[(0, 4)].symbol(), "─");
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
                None,
                None,
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

    /// A theme deliberately unlike the default on every token the compact
    /// state cell can paint through — both `[state]` (idle/working/dead)
    /// and `[eye]` (the attention glyph) — so a color match against the
    /// default theme in a caller's test would mean its glyph check was
    /// vacuous. Shared by the two glyph-stability tests below.
    fn alt_test_theme_paint() -> Paint {
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

    /// Rule 11's compact glyph vocabulary is a fixed mapping from
    /// `AgentState`/attention to one of four characters (`○`, `●`, `⚠`,
    /// `✕`) — it is not a color swatch. Feed the sidebar's status cell two
    /// materially different themes plus `NO_COLOR` and the glyph at the
    /// same cell must read identically every time; only the `Style`
    /// painted under it may change (and must, on the two colored runs, or
    /// this proves nothing).
    #[test]
    fn sidebar_state_glyph_is_stable_across_theme_and_no_color() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            active: true,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::from(["$0".to_string()]);

        let render_with = |paint: &Paint, state: AgentState, needs_attention: bool| -> Buffer {
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
                    manifest: None,
                    manifest_display_name: None,
                    state,
                    needs_attention,
                },
            );
            let backend = TestBackend::new(24, 8);
            let mut term = Terminal::new(backend).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%9",
                    &expanded,
                    &[],
                    f.area(),
                    f.buffer_mut(),
                    paint,
                    &mut hits,
                    &decoration,
                    None,
                    None,
                );
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let alt_paint = alt_test_theme_paint();
        let default_paint = Paint::for_test();
        let plain_paint = Paint::without_color_for_test();

        // Column 5, row 3: one expanded, online workspace puts its first
        // agent row at y = 3 (title, blank, workspace row), and the status
        // glyph lands 3 cells past the 2-cell sidebar pad
        // (`content.x.saturating_add(3)` in `paint_sidebar`).
        let (gx, gy) = (5, 3);
        for (state, needs_attention, glyph) in [
            (AgentState::Idle, false, "○"),
            (AgentState::Working, false, "●"),
            (AgentState::BlockedPermission, true, "⚠"),
            (AgentState::Dead, false, "✕"),
        ] {
            let default_buf = render_with(&default_paint, state, needs_attention);
            let alt_buf = render_with(&alt_paint, state, needs_attention);
            let plain_buf = render_with(&plain_paint, state, needs_attention);

            assert_eq!(
                default_buf[(gx, gy)].symbol(),
                glyph,
                "default theme glyph for {state}"
            );
            assert_eq!(
                alt_buf[(gx, gy)].symbol(),
                glyph,
                "an unrelated theme must not change the glyph for {state}"
            );
            assert_eq!(
                plain_buf[(gx, gy)].symbol(),
                glyph,
                "NO_COLOR must not change the glyph for {state}"
            );
            assert_ne!(
                default_buf[(gx, gy)].fg,
                alt_buf[(gx, gy)].fg,
                "the theme change must actually repaint the color for {state}, \
                 or the glyph check above proves nothing"
            );
            assert_eq!(
                plain_buf[(gx, gy)].fg,
                RtColor::Reset,
                "NO_COLOR must leave no color behind for {state}, confirming \
                 this compact cell does not depend on color to read"
            );
        }
    }

    /// The same guarantee as the sidebar test above, for the other compact
    /// surface rule 11 names explicitly: an inactive pane's border, which
    /// (per `inactive_pane_chrome_keeps_status_compact`) paints the glyph
    /// alone with no word to fall back on.
    #[test]
    fn inactive_pane_border_glyph_is_stable_across_theme_and_no_color() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let render_with = |paint: &Paint, state: AgentState, needs_attention: bool| -> Buffer {
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
                    manifest: None,
                    manifest_display_name: None,
                    state,
                    needs_attention,
                },
            );
            let backend = TestBackend::new(40, 12);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|frame| {
                let runtimes = RuntimeRegistry::default();
                let mut hits = HitMap::default();
                let paused = std::collections::HashSet::new();
                let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
                paint_window(
                    &tab,
                    &runtimes,
                    frame.area(),
                    frame.buffer_mut(),
                    paint,
                    &mut ctx,
                );
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        fn find_glyph(buf: &Buffer, glyph: &str) -> (u16, u16) {
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    if buf[(x, y)].symbol() == glyph {
                        return (x, y);
                    }
                }
            }
            panic!("glyph {glyph:?} was not painted anywhere: {}", flatten(buf));
        }

        let alt_paint = alt_test_theme_paint();
        let default_paint = Paint::for_test();
        let plain_paint = Paint::without_color_for_test();

        for (state, needs_attention, glyph) in [
            (AgentState::Idle, false, "○"),
            (AgentState::Working, false, "●"),
            (AgentState::BlockedPermission, true, "⚠"),
            (AgentState::Dead, false, "✕"),
        ] {
            let default_buf = render_with(&default_paint, state, needs_attention);
            let alt_buf = render_with(&alt_paint, state, needs_attention);
            let plain_buf = render_with(&plain_paint, state, needs_attention);

            let pos = find_glyph(&default_buf, glyph);
            assert_eq!(
                alt_buf[pos].symbol(),
                glyph,
                "an unrelated theme must not move or change the glyph for {state}"
            );
            assert_eq!(
                plain_buf[pos].symbol(),
                glyph,
                "NO_COLOR must not move or change the glyph for {state}"
            );
            assert_ne!(
                default_buf[pos].fg, alt_buf[pos].fg,
                "the theme change must actually repaint the color for {state}, \
                 or the glyph check above proves nothing"
            );
            assert_eq!(
                plain_buf[pos].fg,
                RtColor::Reset,
                "NO_COLOR must leave no color behind for {state}, confirming \
                 this compact cell does not depend on color to read"
            );
        }
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
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
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
            let runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &runtimes,
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
            let runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &runtimes,
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
        let runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(12, 5);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
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
            let runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &runtimes,
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
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
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
        assert!(text.contains("blocked_permission"), "{text:?}");
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
