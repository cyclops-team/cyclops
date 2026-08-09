//! Modal dialogs and popup menus: the workspace's floating chrome. Both
//! center or anchor a bordered box over the frame already painted, clear
//! the cells underneath first, and record their own hit regions last so
//! they shadow whatever is under them. Dialog *content* (what a dialog is,
//! how a key edits its buffer) belongs to `crate::dialog`; this only paints
//! whatever state it is handed.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};

use crate::bindings::BindingAction;
use crate::copy;
use crate::dialog::Dialog;
use crate::input::mouse::{HitMap, HitTarget, MenuState};
use crate::theme::{self, Paint};

/// Columns between a box's border and its content, on both sides. One
/// number for the plain dialogs, the two list dialogs, and the action row
/// they all share, so a button never lands a column off the rows above it.
const DIALOG_INSET: u16 = 2;

/// What a box costs before any copy fits inside it: a border column plus
/// the inset, on each side.
const DIALOG_CHROME_WIDTH: u16 = 2 + 2 * DIALOG_INSET;

/// Narrowest a plain dialog gets when its copy would allow less. Short
/// prompts still read as a dialog rather than as a tooltip.
const DIALOG_MIN_WIDTH: u16 = 40;

/// The plain dialog card: title, hint, input, a blank row, the action row.
/// Fixed, so a dialog holds its shape whichever parts it has; only the
/// error slot under the input grows it.
const DIALOG_INNER_ROWS: u16 = 5;

/// Widest an error is allowed to make a dialog. Past this it wraps instead
/// of stretching the box across the terminal.
const DIALOG_ERROR_MAX_WIDTH: u16 = 68;

/// Columns between two buttons on the action row.
const DIALOG_BUTTON_GAP: u16 = 2;

/// Display columns `text` occupies.
fn text_width(text: &str) -> u16 {
    u16::try_from(Span::raw(text).width()).unwrap_or(u16::MAX)
}

/// The frame every floating box wears. Rounded, because the pane frames
/// are rounded (`canvas.rs` draws ╭ ╮ ╰ ╯ itself) and the workspace speaks
/// one shape language.
fn overlay_block(paint: &Paint) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::pane_border_focused(paint))
        .style(theme::menu_row(paint))
}

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
        Dialog::Compose { buffer, status, .. } => (
            copy::COMPOSE_TITLE,
            Some(buffer),
            // The receipt takes the hint's place once there is one. By then
            // the reader has evidently worked out the grammar, and what
            // happened to their message is the more useful line to hold.
            status.as_deref().or(Some(copy::COMPOSE_HINT)),
            copy::BUTTON_SEND,
        ),
        Dialog::Keybinds { .. } => unreachable!("keybinds uses its own dialog renderer"),
        Dialog::Themes { .. } => unreachable!("themes uses its own dialog renderer"),
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
    if let Dialog::Themes {
        names,
        selected,
        active,
        notice,
    } = dialog
    {
        paint_themes_dialog(
            names,
            *selected,
            *active,
            notice.as_deref(),
            area,
            buf,
            paint,
            hits,
            hover,
        );
        return;
    }
    let (title, input, hint, confirm_label) = dialog_parts(dialog);
    let error = match dialog {
        Dialog::NamePane { error, .. } => error.as_deref(),
        _ => None,
    };
    let Some(dialog_area) = plain_dialog_geometry(title, hint, error, confirm_label, area) else {
        return;
    };
    clear_area(buf, dialog_area);
    let block = overlay_block(paint);
    let inner = block.inner(dialog_area);
    block.render(dialog_area, buf);

    let left = inner.x + DIALOG_INSET;
    let usable_w = inner.width.saturating_sub(2 * DIALOG_INSET);

    super::overlay_text(buf, inner, left, inner.y, title, theme::menu_row(paint));
    // A hint that will not fit whole is not painted. Half a sentence reads
    // as corruption rather than as help, the rule `canvas::paint_notice`
    // follows on the pane border. The dialog still opens: the hint is the
    // part a narrow terminal loses, not the prompt.
    if let Some(hint) = hint.filter(|hint| text_width(hint) <= usable_w) {
        super::overlay_text(buf, inner, left, inner.y + 1, hint, theme::menu_hint(paint));
    }
    if let Some(input) = input {
        // The input field is inset from the dialog, but its editing cursor
        // starts at the field's first cell. No second, accidental indent.
        let field_y = inner.y + 2;
        let field = Rect::new(left, field_y, usable_w, 1);
        buf.set_style(field, theme::dialog_input(paint));
        let visible = input_tail(input, field.width as usize);
        super::overlay_text(
            buf,
            inner,
            field.x,
            field_y,
            &visible,
            theme::dialog_input(paint),
        );
    }
    if let Some(error) = error {
        let error_area = Rect::new(left, inner.y + 3, usable_w, inner.height.saturating_sub(4));
        Paragraph::new(error)
            .style(theme::dialog_error(paint))
            .wrap(Wrap { trim: true })
            .render(error_area, buf);
    }
    paint_dialog_buttons(buf, inner, paint, hits, hover, confirm_label);
}

/// Where a plain dialog lands, or `None` when the terminal cannot host it.
///
/// Two pieces of copy are hard-clipped by `overlay_text` and so set the
/// floor: the title, which is the dialog's question, and the action row,
/// which is how it gets answered. Cut either and the box misstates what it
/// is asking, so under that width nothing is painted at all. The hint and
/// the error never gate the box, because neither has to be cut: the hint
/// drops, the error wraps.
fn plain_dialog_geometry(
    title: &str,
    hint: Option<&str>,
    error: Option<&str>,
    confirm_label: &str,
    area: Rect,
) -> Option<Rect> {
    // 1. The floor: copy that cannot be cut, plus the box around it, and
    //    the card's own rows. Under either, refuse.
    let need_w = text_width(title)
        .max(dialog_buttons_width(confirm_label))
        .saturating_add(DIALOG_CHROME_WIDTH);
    let base_h = DIALOG_INNER_ROWS.saturating_add(2);
    if need_w > area.width || base_h > area.height {
        return None;
    }
    // 2. What the copy would like: the hint and the error widen the box
    //    when there is room for them.
    let want_w = text_width(title)
        .max(hint.map(text_width).unwrap_or(0))
        .max(
            error
                .map(|error| text_width(error).min(DIALOG_ERROR_MAX_WIDTH))
                .unwrap_or(0),
        )
        .saturating_add(DIALOG_CHROME_WIDTH);
    // The floor wins over the terminal cap, which step 1 proved it fits in.
    let w = want_w.max(DIALOG_MIN_WIDTH).min(area.width).max(need_w);
    // 3. The error slot grows the card downward, as far as the terminal
    //    allows and no further: the card itself never leaves the screen.
    let error_lines = error
        .map(|error| wrapped_line_count(error, w.saturating_sub(DIALOG_CHROME_WIDTH)))
        .unwrap_or(0);
    let error_h = u16::try_from(error_lines)
        .unwrap_or(u16::MAX)
        .min(area.height - base_h);
    let h = base_h + error_h;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    Some(Rect::new(x, y, w, h))
}

/// The two actions every dialog offers, in paint order. One list, so the
/// width a dialog reserves and the row it paints cannot disagree.
fn dialog_buttons(confirm_label: &str) -> [(String, HitTarget, bool); 2] {
    [
        (format!("↵ {confirm_label}"), HitTarget::DialogConfirm, true),
        (
            format!("Esc {}", copy::BUTTON_CANCEL),
            HitTarget::DialogCancel,
            false,
        ),
    ]
}

/// Columns the action row needs to show both buttons whole, gap included.
fn dialog_buttons_width(confirm_label: &str) -> u16 {
    let mut width = 0u16;
    for (index, (text, _, _)) in dialog_buttons(confirm_label).iter().enumerate() {
        if index > 0 {
            width = width.saturating_add(DIALOG_BUTTON_GAP);
        }
        width = width.saturating_add(text_width(text));
    }
    width
}

/// Keyboard-first actions on the last inner row, recorded for the mouse.
/// Enter confirms every modal and Escape cancels it, so one shape covers
/// input dialogs, destructive confirms, and the theme picker alike. The row
/// starts at `DIALOG_INSET`, the same column the rows above it start at.
fn paint_dialog_buttons(
    buf: &mut Buffer,
    inner: Rect,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
    confirm_label: &str,
) {
    let button_y = inner.y + inner.height.saturating_sub(1);
    let right = inner.x + inner.width.saturating_sub(DIALOG_INSET);
    let mut bx = inner.x + DIALOG_INSET;
    for (text, target, primary) in dialog_buttons(confirm_label) {
        let bw = text_width(text.as_str());
        // A button cut in half is a worse affordance than no button, and it
        // would claim a hit region for a label nobody can read. The key it
        // names works either way.
        if bw > right.saturating_sub(bx) {
            break;
        }
        let rect = Rect::new(bx, button_y, bw, 1);
        let hovered =
            hover.is_some_and(|(hc, hr)| hr == rect.y && hc >= rect.x && hc < rect.x + rect.width);
        let style = if hovered || primary {
            theme::dialog_primary(paint)
        } else {
            theme::dialog_secondary(paint)
        };
        super::overlay_text(buf, inner, bx, button_y, &text, style);
        hits.push(rect, target);
        bx = bx.saturating_add(bw + DIALOG_BUTTON_GAP);
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
    let block = overlay_block(paint);
    let inner = block.inner(dialog_area);
    block.render(dialog_area, buf);

    let left = inner.x + DIALOG_INSET;
    let usable_w = inner.width.saturating_sub(2 * DIALOG_INSET);
    super::overlay_text(
        buf,
        inner,
        left,
        inner.y,
        copy::KEYBINDS_TITLE,
        theme::menu_row(paint).add_modifier(Modifier::BOLD),
    );
    super::overlay_text(
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
    super::overlay_text(
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
        super::overlay_text(
            buf,
            inner,
            left,
            y,
            &row.keys,
            theme::pane_border_focused(paint),
        );
        super::overlay_text(
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
    super::overlay_text(
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
        super::overlay_text(buf, inner, x, footer_y, &progress, theme::menu_hint(paint));
    }
}

/// Tallest the keybinds dialog may grow. The list scrolls, so the dialog
/// stays a card rather than a wall.
const KEYBINDS_MAX_HEIGHT: u16 = 20;

fn keybind_dialog_geometry(row_count: usize, area: Rect) -> Option<(Rect, u16)> {
    if area.width < 8 || area.height < 6 {
        return None;
    }
    let width = area.width.saturating_sub(4).min(72);
    // Title, hint, glyph legend, one blank line before the list, one blank
    // line after it, and the footer: 6 fixed rows around the scrollable list.
    let wanted_height = u16::try_from(row_count.saturating_add(8)).unwrap_or(u16::MAX);
    let height = wanted_height
        .min(area.height.saturating_sub(2))
        .clamp(6, KEYBINDS_MAX_HEIGHT);
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

/// The theme picker: a titled list with the active row checked and the
/// selected row raised. Applying repaints the whole workspace, so the
/// rows need no swatch; the CLI's listing is the visual preview.
///
/// The active marker is [`copy::MENU_CHECK`], the same glyph the app
/// menu's toggles use, because a theme being on is the same kind of fact
/// as motion being on and should not need a second vocabulary. It rides
/// beside the selection highlight, so "which one is applied" and "which
/// one is the cursor on" stay separable without color.
fn paint_themes_dialog(
    names: &[String],
    selected: usize,
    active: Option<usize>,
    notice: Option<&str>,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    // An empty listing has no rows to explain the hint against; the
    // notice slot wraps, so the where-themes-come-from line goes there.
    let notice = if names.is_empty() {
        Some(copy::THEMES_EMPTY)
    } else {
        notice
    };
    let width = area.width.saturating_sub(4).min(48);
    let notice_lines = notice
        .map(|text| wrapped_line_count(text, width.saturating_sub(DIALOG_CHROME_WIDTH)))
        .unwrap_or(0);
    let Some((dialog_area, list_h)) = themes_dialog_geometry(names.len(), notice_lines, area)
    else {
        return;
    };
    clear_area(buf, dialog_area);
    let block = overlay_block(paint);
    let inner = block.inner(dialog_area);
    block.render(dialog_area, buf);

    let left = inner.x + DIALOG_INSET;
    let usable_w = inner.width.saturating_sub(2 * DIALOG_INSET);
    super::overlay_text(
        buf,
        inner,
        left,
        inner.y,
        copy::THEMES_TITLE,
        theme::menu_row(paint).add_modifier(Modifier::BOLD),
    );
    if !names.is_empty() {
        super::overlay_text(
            buf,
            inner,
            left,
            inner.y + 1,
            copy::THEMES_HINT,
            theme::menu_hint(paint),
        );
    }

    // The selected row stays visible: the window slides down only once
    // the arrows walk past its last visible row.
    let list_y = inner.y + 3;
    let list_h = usize::from(list_h);
    let start = if list_h == 0 || selected < list_h {
        0
    } else {
        selected + 1 - list_h
    };
    for (line, index) in (start..names.len().min(start + list_h)).enumerate() {
        let y = list_y + line as u16;
        let style = if index == selected {
            theme::menu_row_hover(paint)
        } else {
            theme::menu_row(paint)
        };
        buf.set_style(Rect::new(left, y, usable_w, 1), style);
        let mark = if Some(index) == active {
            copy::MENU_CHECK
        } else {
            " "
        };
        super::overlay_text(
            buf,
            inner,
            left,
            y,
            &format!("{mark} {}", names[index]),
            style,
        );
    }

    if let Some(text) = notice {
        let notice_y = list_y + list_h as u16 + 1;
        let bottom = inner.y + inner.height;
        if notice_y < bottom {
            let notice_area = Rect::new(
                left,
                notice_y,
                usable_w,
                (notice_lines as u16).min(bottom - notice_y),
            );
            Paragraph::new(text)
                .style(theme::menu_hint(paint))
                .wrap(Wrap { trim: true })
                .render(notice_area, buf);
        }
    }

    paint_dialog_buttons(buf, inner, paint, hits, hover, copy::BUTTON_APPLY);
}

/// Same shape as [`keybind_dialog_geometry`]: fixed rows (title, hint,
/// one blank before the list, one after, the footer) around a list that
/// shrinks before the chrome does.
fn themes_dialog_geometry(
    row_count: usize,
    notice_lines: usize,
    area: Rect,
) -> Option<(Rect, u16)> {
    if area.width < 8 || area.height < 6 {
        return None;
    }
    let width = area.width.saturating_sub(4).min(48);
    let wanted_height =
        u16::try_from(row_count.saturating_add(notice_lines).saturating_add(7)).unwrap_or(u16::MAX);
    let height = wanted_height.min(area.height.saturating_sub(2)).max(6);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let list_height = height
        .saturating_sub(2)
        .saturating_sub(5)
        .saturating_sub(u16::try_from(notice_lines).unwrap_or(u16::MAX));
    Some((dialog, list_height))
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

/// What the app menu's toggles currently read, so their rows can say so.
///
/// Passed in rather than reached for: this module paints and owns no
/// state, and a menu that guessed at a preference would be a second place
/// the answer lives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuChecks {
    pub tab_bar: bool,
    pub motion: bool,
    /// Whether the sidebar is showing the event stream rather than the
    /// session tree. The item toggles between the two, so a check here
    /// means "the stream is what you are looking at".
    pub stream: bool,
}

/// One menu row: its label, what clicking it does, and whether it is a
/// setting that is currently on.
///
/// `None` is "not a toggle", which is different from `Some(false)`: an
/// unchecked row still reserves the check column so labels line up, and a
/// menu with no toggles at all reserves nothing.
pub type MenuRow = (&'static str, BindingAction, Option<bool>);

/// Menu items for one open menu.
pub fn menu_items(menu: &MenuState, checks: MenuChecks) -> Vec<MenuRow> {
    match menu {
        MenuState::None => Vec::new(),
        MenuState::AppMenu => vec![
            (copy::MENU_NEW_TAB, BindingAction::NewTab, None),
            (copy::MENU_NEW_WORKSPACE, BindingAction::NewWorkspace, None),
            (
                copy::MENU_TOGGLE_EVENTS,
                BindingAction::ToggleEventPanel,
                Some(checks.stream),
            ),
            // The tab strip's only visible switch. It sits beside the
            // stream toggle because both answer the same question: which
            // surfaces this workspace shows.
            (
                copy::MENU_TAB_BAR,
                BindingAction::ToggleTabBar,
                Some(checks.tab_bar),
            ),
            // Same reason the tab strip's switch is here: a preference
            // with no chord needs one place a mouse can reach it.
            (
                copy::MENU_MOTION,
                BindingAction::ToggleMotion,
                Some(checks.motion),
            ),
            // Themes opens a picker rather than flipping anything, so it
            // carries no check of its own; the picker marks the theme
            // that is on with the same glyph.
            (copy::MENU_THEMES, BindingAction::ShowThemes, None),
            (copy::MENU_KEYBINDS, BindingAction::ShowKeybinds, None),
            (copy::MENU_DETACH, BindingAction::Detach, None),
        ],
        MenuState::ContextMenu { .. } => vec![
            (copy::MENU_NAME_PANE, BindingAction::NamePane, None),
            (copy::MENU_SPLIT_RIGHT, BindingAction::SplitRight, None),
            (copy::MENU_SPLIT_DOWN, BindingAction::SplitDown, None),
            (copy::MENU_SWAP_LEFT, BindingAction::SwapPaneLeft, None),
            (copy::MENU_SWAP_RIGHT, BindingAction::SwapPaneRight, None),
            (copy::MENU_SWAP_UP, BindingAction::SwapPaneUp, None),
            (copy::MENU_SWAP_DOWN, BindingAction::SwapPaneDown, None),
            (copy::MENU_ZOOM_PANE, BindingAction::ZoomPane, None),
            (copy::MENU_CLOSE_PANE, BindingAction::ClosePane, None),
        ],
        MenuState::TabMenu { .. } => vec![
            (copy::MENU_RENAME_TAB, BindingAction::RenameTab, None),
            (copy::MENU_CLOSE_TAB, BindingAction::CloseTab, None),
        ],
        MenuState::WorkspaceMenu { .. } => vec![
            (
                copy::MENU_RENAME_WORKSPACE,
                BindingAction::RenameWorkspace,
                None,
            ),
            (
                copy::MENU_CLOSE_WORKSPACE,
                BindingAction::CloseWorkspace,
                None,
            ),
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
    checks: MenuChecks,
) {
    let items = menu_items(menu, checks);
    if items.is_empty() {
        return;
    }
    // The check column is reserved for the whole menu or for none of it, so
    // labels line up down the list rather than stepping in and out as
    // settings flip. A menu with no toggles pays nothing for it.
    let gutter: u16 = if items.iter().any(|(_, _, c)| c.is_some()) {
        2
    } else {
        0
    };
    let w = (items
        .iter()
        .map(|(label, _, _)| Span::raw(*label).width())
        .max()
        .unwrap_or(0) as u16)
        .saturating_add(4)
        .saturating_add(gutter);
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
    let block = overlay_block(paint);
    let inner = block.inner(menu_area);
    block.render(menu_area, buf);
    for (i, (label, action, checked)) in items.iter().enumerate() {
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
        // An off toggle pays a blank, not a second glyph. A row reading
        // "✗ Motion" beside "✓ Tab bar" is two marks to tell apart at a
        // glance; presence against absence is one.
        let mark = match (gutter, checked) {
            (0, _) => String::new(),
            (_, Some(true)) => format!("{} ", copy::MENU_CHECK),
            (_, _) => "  ".into(),
        };
        super::overlay_text(buf, inner, inner.x, y, &format!(" {mark}{label}"), style);
        hits.push(rect, HitTarget::MenuItem { action: *action });
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color as RtColor;
    use ratatui::Terminal;

    use super::*;
    use crate::render::test_support::flatten;

    #[test]
    fn context_menu_paints_items_with_hits() {
        let backend = TestBackend::new(40, 16);
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
                None,
                MenuChecks::default(),
            );
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
        let backend = TestBackend::new(40, 16);
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
                MenuChecks::default(),
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
        let items = menu_items(
            &MenuState::TabMenu {
                window_id: "@1".into(),
                at: (0, 0),
            },
            MenuChecks::default(),
        );
        let actions: Vec<_> = items.iter().map(|(_, a, _)| *a).collect();
        assert_eq!(
            actions,
            vec![BindingAction::RenameTab, BindingAction::CloseTab]
        );
        let items = menu_items(
            &MenuState::WorkspaceMenu {
                session: "cyclops".into(),
                at: (0, 0),
            },
            MenuChecks::default(),
        );
        let actions: Vec<_> = items.iter().map(|(_, a, _)| *a).collect();
        assert_eq!(
            actions,
            vec![
                BindingAction::RenameWorkspace,
                BindingAction::CloseWorkspace
            ]
        );
    }

    /// A toggle's row says what the toggle currently reads. The check is
    /// the only difference between on and off, so the test asserts both
    /// directions: a mark that never cleared would look identical to a
    /// correct one on the frame where the setting is on.
    #[test]
    fn a_menu_toggle_is_checked_exactly_when_its_setting_is_on() {
        let on = MenuChecks {
            tab_bar: true,
            motion: true,
            stream: false,
        };
        let rows = menu_items(&MenuState::AppMenu, on);
        let checked = |rows: &[MenuRow], label: &str| {
            rows.iter()
                .find(|(l, _, _)| *l == label)
                .unwrap_or_else(|| panic!("{label} is not in the app menu"))
                .2
        };

        assert_eq!(checked(&rows, copy::MENU_TAB_BAR), Some(true));
        assert_eq!(checked(&rows, copy::MENU_MOTION), Some(true));
        assert_eq!(checked(&rows, copy::MENU_TOGGLE_EVENTS), Some(false));
        // Not a toggle: opening a picker is not a setting, and reserving a
        // check for it would imply it could be on.
        assert_eq!(checked(&rows, copy::MENU_THEMES), None);
        assert_eq!(checked(&rows, copy::MENU_DETACH), None);

        let off = MenuChecks::default();
        let rows = menu_items(&MenuState::AppMenu, off);
        assert_eq!(checked(&rows, copy::MENU_TAB_BAR), Some(false));
        assert_eq!(checked(&rows, copy::MENU_MOTION), Some(false));
    }

    /// The check column is reserved for the whole menu or none of it, so
    /// labels do not step sideways as settings flip, and a menu with no
    /// toggles does not pay two columns for a gutter nothing uses.
    #[test]
    fn the_check_column_is_all_or_nothing_per_menu() {
        let backend = TestBackend::new(60, 20);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();

        let render = |term: &mut Terminal<TestBackend>, menu: MenuState, checks: MenuChecks| {
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_menu(
                    &menu,
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    None,
                    checks,
                );
            })
            .unwrap();
            let buf = term.backend().buffer();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        let app_on = render(
            &mut term,
            MenuState::AppMenu,
            MenuChecks {
                tab_bar: true,
                motion: false,
                stream: false,
            },
        );
        let tab_bar = app_on
            .iter()
            .find(|line| line.contains(copy::MENU_TAB_BAR))
            .expect("a Tab bar row");
        let motion = app_on
            .iter()
            .find(|line| line.contains(copy::MENU_MOTION))
            .expect("a Motion row");
        assert!(
            tab_bar.contains(&format!("{} {}", copy::MENU_CHECK, copy::MENU_TAB_BAR)),
            "an on toggle carries the check: {tab_bar}"
        );
        assert!(
            !motion.contains(copy::MENU_CHECK),
            "an off toggle carries no mark of its own: {motion}"
        );
        // Column, not byte offset: the check is three bytes and a space is
        // one, so `find` alone reports a two-byte gap that is not on screen.
        let column_of = |line: &str, needle: &str| {
            line.find(needle)
                .map(|byte| line[..byte].chars().count())
                .expect("the label is on this line")
        };
        assert_eq!(
            column_of(tab_bar, copy::MENU_TAB_BAR),
            column_of(motion, copy::MENU_MOTION),
            "checked and unchecked labels start in the same column"
        );

        // A menu with no toggles at all reserves nothing for them.
        let ctx = render(
            &mut term,
            MenuState::ContextMenu {
                pane_id: "%1".into(),
                at: (0, 0),
            },
            MenuChecks::default(),
        );
        let zoom = ctx
            .iter()
            .find(|line| line.contains(copy::MENU_ZOOM_PANE))
            .expect("a Zoom pane row");
        let gutterless = format!(" {}", copy::MENU_ZOOM_PANE);
        assert!(
            zoom.contains(&gutterless),
            "a menu with no toggles indents by one, not three: {zoom}"
        );
    }

    /// The app menu is the visible route to everything that has no chrome
    /// of its own, and the tab strip is now one of those: hidden, its item
    /// here is the only way back, so it has to be in this list.
    #[test]
    fn app_menu_offers_the_surface_toggles_between_new_and_keybinds() {
        let actions: Vec<_> = menu_items(&MenuState::AppMenu, MenuChecks::default())
            .iter()
            .map(|(_, action, _)| *action)
            .collect();
        assert_eq!(
            actions,
            vec![
                BindingAction::NewTab,
                BindingAction::NewWorkspace,
                BindingAction::ToggleEventPanel,
                BindingAction::ToggleTabBar,
                // Motion ships with no chord either, so the menu is its
                // only switch, for the same reason the tab strip's is.
                BindingAction::ToggleMotion,
                BindingAction::ShowThemes,
                BindingAction::ShowKeybinds,
                BindingAction::Detach,
            ]
        );
    }

    #[test]
    fn themes_dialog_marks_active_raises_selected_and_offers_apply() {
        let backend = TestBackend::new(60, 16);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let dialog = Dialog::Themes {
            names: vec!["dark".into(), "light".into(), "solar".into()],
            selected: 1,
            active: Some(0),
            notice: None,
        };
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
        })
        .unwrap();

        let buf = term.backend().buffer();
        let flat = flatten(buf);
        assert!(flat.contains("Themes"), "title renders: {flat}");
        assert!(flat.contains("✓ dark"), "active row is checked: {flat}");
        assert!(flat.contains("light") && flat.contains("solar"), "{flat}");
        assert!(flat.contains("↵ Apply"), "confirm affordance: {flat}");
        assert!(flat.contains("Esc Cancel"), "cancel affordance: {flat}");
        assert!(hits
            .regions()
            .iter()
            .any(|region| region.target == HitTarget::DialogConfirm));
        assert!(hits
            .regions()
            .iter()
            .any(|region| region.target == HitTarget::DialogCancel));
        // The selected row's ground differs from its neighbours'.
        let row_of = |needle: &str| {
            (0..16)
                .find(|row| {
                    let line: String = (0..60).map(|col| buf[(col, *row)].symbol()).collect();
                    line.contains(needle)
                })
                .unwrap_or_else(|| panic!("{needle} not painted"))
        };
        let x = (0..60)
            .find(|col| buf[(*col, row_of("light"))].symbol() == "l")
            .expect("row text");
        assert_ne!(
            buf[(x, row_of("light"))].bg,
            buf[(x, row_of("solar"))].bg,
            "selection should raise the row the arrows are on"
        );
    }

    #[test]
    fn themes_dialog_shows_the_apply_notice() {
        let backend = TestBackend::new(60, 16);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let dialog = Dialog::Themes {
            names: vec!["dark".into()],
            selected: 0,
            active: Some(0),
            notice: Some(copy::THEME_SAVED_NO_DAEMON.into()),
        };
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("The next command picks it up."),
            "the saved story stays visible: {flat}"
        );
    }

    #[test]
    fn empty_themes_dialog_says_where_themes_come_from() {
        let backend = TestBackend::new(60, 16);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let dialog = Dialog::Themes {
            names: Vec::new(),
            selected: 0,
            active: None,
            notice: None,
        };
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("cyclops start"),
            "the empty state names the seeding command: {flat}"
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

        let width = (text_width(copy::NEW_TAB_HINT) + DIALOG_CHROME_WIDTH).max(DIALOG_MIN_WIDTH);
        let left = (50 - width) / 2;
        // Border, then the one intentional inset every row shares. There is
        // no second, accidental indent inside the input field.
        assert_eq!(
            term.backend().buffer()[(left + 1 + DIALOG_INSET, 4)].symbol(),
            "▏"
        );
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
    fn keybind_dialog_stays_a_card_in_tall_terminals() {
        let rows: Vec<_> = (0..100)
            .map(|i| crate::bindings::BindingHelp {
                keys: format!("Ctrl+{}", i % 26),
                action: format!("Action {i}"),
            })
            .collect();
        let area = Rect::new(0, 0, 80, 50);
        let (dialog, list_h) = keybind_dialog_geometry(rows.len(), area)
            .expect("should produce geometry for large area");
        assert_eq!(
            dialog.height, 20,
            "dialog should be capped at 20 rows even with many bindings"
        );
        assert_eq!(
            list_h, 12,
            "list height = dialog height - borders - padding"
        );
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
    fn confirm_close_dialog_renders() {
        // 50 columns: the question is 41 wide and the box costs six more.
        let backend = TestBackend::new(50, 10);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let dialog = Dialog::confirm_close("%0");
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_dialog(&dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains(copy::CONFIRM_CLOSE_PANE), "{flat}");
        assert!(flat.contains("↵ Confirm"), "confirm key is visible: {flat}");
        assert!(
            flat.contains("Esc Cancel"),
            "cancel action is visible: {flat}"
        );
    }

    /// Top-left cell of the only box on screen. The tests measure insets
    /// from the frame the code actually painted rather than recomputing
    /// geometry beside it.
    fn box_corner(buf: &Buffer) -> (u16, u16) {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].symbol() == "╭" {
                    return (x, y);
                }
            }
        }
        panic!("no rounded box was painted");
    }

    /// First row whose text contains `needle`.
    fn row_of(buf: &Buffer, needle: &str) -> u16 {
        (0..buf.area.height)
            .find(|y| {
                let line: String = (0..buf.area.width).map(|x| buf[(x, *y)].symbol()).collect();
                line.contains(needle)
            })
            .unwrap_or_else(|| panic!("{needle} was not painted"))
    }

    /// The pane frames are rounded (`canvas.rs` draws ╭ ╮ ╰ ╯ itself). Every
    /// box that floats over them is too, or the app speaks two shapes.
    #[test]
    fn every_floating_box_wears_the_rounded_frame() {
        let theme = Paint::for_test();
        let cases: Vec<(&str, Option<Dialog>, MenuState)> = vec![
            (
                "new tab",
                Some(Dialog::NewTab {
                    buffer: String::new(),
                }),
                MenuState::None,
            ),
            (
                "keybinds",
                Some(Dialog::Keybinds {
                    scroll: 0,
                    rows: vec![crate::bindings::BindingHelp {
                        keys: "Ctrl+A".into(),
                        action: "Attach".into(),
                    }],
                }),
                MenuState::None,
            ),
            (
                "themes",
                Some(Dialog::Themes {
                    names: vec!["dark".into()],
                    selected: 0,
                    active: Some(0),
                    notice: None,
                }),
                MenuState::None,
            ),
            ("app menu", None, MenuState::AppMenu),
        ];
        for (name, dialog, menu) in cases {
            let backend = TestBackend::new(72, 24);
            let mut term = Terminal::new(backend).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                if let Some(dialog) = &dialog {
                    paint_dialog(dialog, f.area(), f.buffer_mut(), &theme, &mut hits, None);
                }
                paint_menu(
                    &menu,
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    None,
                    MenuChecks::default(),
                );
            })
            .unwrap();
            let flat = flatten(term.backend().buffer());
            assert!(
                flat.contains('╭') && flat.contains('╯'),
                "{name} should be rounded like the pane frames: {flat}"
            );
            assert!(
                !flat.contains('┌'),
                "{name} still paints a square corner: {flat}"
            );
        }
    }

    /// The action row and every row above it start at the same column, in
    /// the plain dialogs and in the theme picker alike.
    #[test]
    fn every_dialog_row_starts_at_the_same_inset() {
        let theme = Paint::for_test();

        let backend = TestBackend::new(72, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_dialog(
                &Dialog::Themes {
                    names: vec!["dark".into(), "light".into()],
                    selected: 0,
                    active: Some(0),
                    notice: None,
                },
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let (corner_x, corner_y) = box_corner(buf);
        let content_x = corner_x + 1 + DIALOG_INSET;
        assert_eq!(
            buf[(content_x, corner_y + 1)].symbol(),
            "T",
            "the themes title starts at the inset"
        );
        assert_eq!(
            buf[(content_x, row_of(buf, "✓ dark"))].symbol(),
            copy::MENU_CHECK,
            "the active marker starts at the inset"
        );
        let confirm = hits
            .regions()
            .iter()
            .find(|region| region.target == HitTarget::DialogConfirm)
            .expect("apply button is clickable");
        assert_eq!(
            confirm.rect.x, content_x,
            "the action row must not sit a column left of the rows above it"
        );

        let backend = TestBackend::new(72, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_dialog(
                &Dialog::NewTab {
                    buffer: "revw".into(),
                },
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let (corner_x, corner_y) = box_corner(buf);
        let content_x = corner_x + 1 + DIALOG_INSET;
        assert_eq!(
            buf[(content_x, corner_y + 1)].symbol(),
            "N",
            "the title starts at the inset"
        );
        assert_eq!(
            buf[(content_x, corner_y + 3)].symbol(),
            "r",
            "the input field starts at the inset"
        );
        let confirm = hits
            .regions()
            .iter()
            .find(|region| region.target == HitTarget::DialogConfirm)
            .expect("create button is clickable");
        assert_eq!(confirm.rect.x, content_x, "the action row shares the inset");
    }

    /// The hint is the line a narrow terminal loses. The prompt, the field,
    /// and the actions stay, because a dialog that opens with three of its
    /// four parts is still usable and half a sentence is not.
    #[test]
    fn a_hint_that_would_be_cut_is_dropped_and_returns_when_it_fits() {
        let theme = Paint::for_test();
        let paint_at = |width: u16| {
            let backend = TestBackend::new(width, 12);
            let mut term = Terminal::new(backend).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_dialog(
                    &Dialog::NamePane {
                        pane_id: "%0".into(),
                        buffer: "rev".into(),
                        error: None,
                    },
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    None,
                );
            })
            .unwrap();
            flatten(term.backend().buffer())
        };

        // The hint is 55 columns; 50 cannot hold it whole.
        let narrow = paint_at(50);
        assert!(
            narrow.contains(copy::NAME_PANE_TITLE),
            "the prompt still opens: {narrow}"
        );
        assert!(
            narrow.contains("↵ Save"),
            "the actions are still offered: {narrow}"
        );
        assert!(
            !narrow.contains("Used to identify"),
            "no fragment of the hint should be painted: {narrow}"
        );

        let wide = paint_at(70);
        assert!(
            wide.contains(copy::NAME_PANE_HINT),
            "the whole hint returns once it fits: {wide}"
        );
    }

    /// The question and the action row are the dialog. Neither can be cut,
    /// so a terminal too narrow for them gets no box at all.
    #[test]
    fn a_dialog_too_narrow_for_its_question_is_not_painted() {
        // The close prompt is 41 columns and the box costs six more.
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_dialog(
                &Dialog::confirm_close("%0"),
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
            );
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(
            flat.trim().is_empty(),
            "a question that cannot be read whole is not asked: {flat}"
        );
        assert!(
            hits.regions().is_empty(),
            "an unpainted dialog claims no buttons"
        );
    }

    /// The card is five inner rows. A terminal that cannot hold them gets
    /// nothing, rather than a button row painted over what was typed.
    #[test]
    fn a_short_terminal_refuses_the_card_instead_of_stacking_it() {
        let theme = Paint::for_test();
        let draw_at = |height: u16| {
            let backend = TestBackend::new(60, height);
            let mut term = Terminal::new(backend).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_dialog(
                    &Dialog::NewTab {
                        buffer: "revw".into(),
                    },
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    None,
                );
            })
            .unwrap();
            (term, hits)
        };

        let (term, hits) = draw_at(5);
        let flat = flatten(term.backend().buffer());
        assert!(
            flat.trim().is_empty(),
            "five rows cannot hold the card: {flat}"
        );
        assert!(
            hits.regions().is_empty(),
            "nothing painted, nothing clickable"
        );

        // Seven rows is the exact fit: the typed name keeps its own row and
        // the actions stay below it instead of landing on top of it.
        let (term, _) = draw_at(7);
        let buf = term.backend().buffer();
        assert!(
            row_of(buf, "↵ Create") > row_of(buf, "revw"),
            "the action row must stay below the input row"
        );
    }

    /// The geometry answers all three width questions in one place: refuse,
    /// fit snug, or grow for the copy.
    #[test]
    fn plain_dialog_geometry_refuses_before_it_cuts() {
        let area = Rect::new(0, 0, 80, 24);
        let card = plain_dialog_geometry(
            copy::NEW_TAB_TITLE,
            Some(copy::NEW_TAB_HINT),
            None,
            copy::BUTTON_CREATE,
            area,
        )
        .expect("a full terminal hosts the card");
        assert_eq!(
            card.height,
            DIALOG_INNER_ROWS + 2,
            "no error, so the card is exactly its five rows plus borders"
        );
        assert_eq!(
            card.width,
            text_width(copy::NEW_TAB_HINT) + DIALOG_CHROME_WIDTH,
            "the box grows to hold the hint whole"
        );

        assert!(
            plain_dialog_geometry(
                copy::CONFIRM_CLOSE_PANE,
                None,
                None,
                copy::BUTTON_CONFIRM,
                Rect::new(0, 0, 40, 24)
            )
            .is_none(),
            "a title that cannot be read whole refuses the box"
        );
        assert!(
            plain_dialog_geometry(
                copy::NEW_TAB_TITLE,
                Some(copy::NEW_TAB_HINT),
                None,
                copy::BUTTON_CREATE,
                Rect::new(0, 0, 80, DIALOG_INNER_ROWS + 1)
            )
            .is_none(),
            "a terminal shorter than the card refuses the box"
        );

        // A long error grows the card downward, never past the terminal.
        let error = "x ".repeat(200);
        let card = plain_dialog_geometry(
            copy::NAME_PANE_TITLE,
            Some(copy::NAME_PANE_HINT),
            Some(error.as_str()),
            copy::BUTTON_SAVE,
            Rect::new(0, 0, 80, 12),
        )
        .expect("the card still fits");
        assert_eq!(card.height, 12, "the error stops at the terminal edge");
    }
}
