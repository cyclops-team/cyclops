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

use super::PANE_GRIP;
use crate::bindings::BindingAction;
use crate::copy;
use crate::dialog::{
    Dialog, SettingsSection, SoundPicker, SoundRow, ThemePicker, ViewRow, ViewSwitches,
};
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

/// Widest the settings card gets. Wider than the plain dialogs so the
/// sound section's one-line explanation and the theme hint each fit on
/// one row at an ordinary terminal width; a narrower terminal wraps them
/// rather than losing them.
const SETTINGS_MAX_WIDTH: u16 = 64;

/// Widest the keybinds card grows: room for a chord column and the
/// longest action name beside it, and no wider.
const KEYBINDS_MAX_WIDTH: u16 = 72;

/// Columns between two buttons on the action row.
const DIALOG_BUTTON_GAP: u16 = 2;

/// Rows a multi-line field grows to before it starts scrolling its tail.
/// Six is a short paragraph: enough that a message reads as one thought,
/// and not so many that the card takes the terminal it floats over.
const FIELD_MAX_ROWS: u16 = 6;

/// The rows a floating box is dragged by: its top border, and the row
/// under it. One border row is a hard target for a pointer, and the row
/// under it carries no control of its own (the hint on the keybinds
/// sheet, the question on a plain one), so claiming it costs nothing and
/// doubles the reach. The settings card's section chips are the one
/// control on that row; they are pushed after the strip so each wins its
/// own cells and the rest of the row still drags.
const DIALOG_GRAB_ROWS: u16 = 2;

/// Move a centered box by however far the operator has dragged it, and keep
/// it whole and on screen.
///
/// A box that could be dragged off the edge is a box whose action row can be
/// put somewhere nobody can click, and Escape is then the only way out of a
/// dialog that also has a Cancel button. So the offset stops at the edge
/// rather than the box leaving through it.
fn shift_on_screen(rect: Rect, area: Rect, offset: (i16, i16)) -> Rect {
    let axis = |start: u16, span: u16, bound_start: u16, bound_span: u16, delta: i16| {
        let slack = i32::from(bound_span.saturating_sub(span));
        let low = i32::from(bound_start);
        (i32::from(start) + i32::from(delta)).clamp(low, low + slack) as u16
    };
    Rect::new(
        axis(rect.x, rect.width, area.x, area.width, offset.0),
        axis(rect.y, rect.height, area.y, area.height, offset.1),
        rect.width,
        rect.height,
    )
}

/// Name a floating box in its top border, set the drag grip at the
/// border's far end, and record the rows that pick the box up.
///
/// The border reads `╭─ Themes ──────[⠿]─╮`, or `╭──────[⠿]─╮` for a
/// card whose title is its question and stays inside. The grip is
/// [`PANE_GRIP`], the handle every pane frame wears, so a card is dragged
/// by the glyph a pane is; and it is always painted, because a handle that
/// only appears under the mouse has to be found by accident. Nothing here
/// answers the pointer: the whole strip drags and the grip says so at
/// rest. An earlier version lit the border under the mouse, and a bar
/// that lit read as a selection rather than a handle.
fn paint_title_bar(
    buf: &mut Buffer,
    dialog_area: Rect,
    title: Option<&str>,
    paint: &Paint,
    hits: &mut HitMap,
) {
    let grab = Rect::new(
        dialog_area.x,
        dialog_area.y,
        dialog_area.width,
        DIALOG_GRAB_ROWS.min(dialog_area.height),
    );
    // Between the corners, which stay the frame's, one border cell in
    // from each so the name and the grip sit in the line rather than
    // against a corner.
    let bar = Rect::new(
        dialog_area.x.saturating_add(1),
        dialog_area.y,
        dialog_area.width.saturating_sub(2),
        1,
    );
    let style = theme::dialog_title(paint);
    if let Some(title) = title {
        let name = format!(" {title} ");
        super::overlay_text(buf, bar, bar.x.saturating_add(1), bar.y, &name, style);
    }
    let grip_x = (bar.x + bar.width).saturating_sub(text_width(PANE_GRIP) + 1);
    if grip_x > bar.x {
        super::overlay_text(buf, bar, grip_x, bar.y, PANE_GRIP, style);
    }
    hits.push(grab, HitTarget::DialogTitleBar);
}

/// `text` broken into the visual lines a field `width` columns wide shows,
/// honouring the line breaks the composer's own chord inserts.
///
/// The one wrap the field has, so the rows the card is sized for and the
/// rows it paints cannot disagree. [`wrapped_line_count`] cannot serve:
/// it treats a newline as one more space, so a three-line message would
/// size a one-line field.
fn wrap_field(text: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let mut rest = line;
        loop {
            let (take, skip) = wrap_head(rest, usize::from(width));
            lines.push(rest[..take].to_string());
            rest = &rest[take + skip..];
            if rest.is_empty() {
                break;
            }
        }
    }
    lines
}

/// Rows a field needs to show `text` whole at `width`.
fn field_rows(text: &str, width: u16) -> usize {
    wrap_field(text, width).len().max(1)
}

/// The last `rows` visual lines of `text` wrapped to `width`, with the
/// editing cursor on the final one.
///
/// The tail rather than the head: the cursor is at the end of the buffer,
/// and a field that scrolled off the thing being typed would be worse than
/// no field at all.
fn field_tail(text: &str, width: u16, rows: u16) -> Vec<String> {
    let rows = usize::from(rows.max(1));
    let mut lines = wrap_field(text, width);
    if lines.len() > rows {
        lines.drain(..lines.len() - rows);
    }
    // The cursor rides the last line, and `input_tail` is what keeps it
    // visible when that line alone is wider than the field.
    if let Some(last) = lines.pop() {
        lines.push(input_tail(&last, usize::from(width)));
    }
    lines
}

/// The byte length of the longest prefix of `text` that fits in `width`
/// display columns. Always at least one character, so a glyph wider than
/// the whole field advances instead of looping forever.
fn input_head(text: &str, width: usize) -> usize {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if Span::raw(&text[..next]).width() > width && end > 0 {
            return end;
        }
        end = next;
    }
    end
}

/// Where to break `text` for a field `width` columns wide: how many bytes
/// go on this line, and how many to skip before the next one.
///
/// Words are kept whole, because the field holds prose and a message split
/// as `da / emon` reads as a rendering fault rather than as a wrap. A
/// single word wider than the field has no break to take and gets the hard
/// one; the skipped byte is the space the break replaced, which would
/// otherwise open the next line with an indent.
fn wrap_head(text: &str, width: usize) -> (usize, usize) {
    let hard = input_head(text, width);
    if hard == text.len() {
        return (hard, 0);
    }
    if text[hard..].starts_with(' ') {
        return (hard, 1);
    }
    match text[..hard].rfind(' ') {
        Some(space) => (space, 1),
        None => (hard, 0),
    }
}

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
        Dialog::Compose { send, .. } if send.is_confirming_abandon() => (
            copy::COMPOSE_ABANDON_TITLE,
            None,
            Some(copy::COMPOSE_ABANDON_HINT),
            copy::BUTTON_ABANDON,
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
        Dialog::Settings { .. } => unreachable!("settings uses its own dialog renderer"),
    }
}

/// Where an open dialog's box lands before the operator's drag is applied.
///
/// The one dispatch from a dialog to its geometry, so the bound a drag is
/// held to and the rect the paint uses cannot disagree about which shape a
/// dialog has. `paint_dialog` reaches the same three functions for the
/// extra row counts it also needs;
/// `every_dialog_is_dragged_to_where_it_is_painted` is what keeps the two
/// matches saying the same thing.
pub fn dialog_rect(dialog: &Dialog, area: Rect) -> Option<Rect> {
    match dialog {
        Dialog::Keybinds { rows, .. } => keybind_dialog_geometry(rows.len(), area).map(|(r, _)| r),
        // Sized for every section at once, so Tab never moves the box.
        Dialog::Settings {
            themes,
            view,
            sound,
            ..
        } => {
            let (rows, footer_lines) = settings_frame(themes, view, sound, area);
            settings_dialog_geometry(rows, footer_lines, area).map(|(r, _)| r)
        }
        _ => {
            let (title, input, hint, confirm_label) = dialog_parts(dialog);
            let error = match dialog {
                Dialog::NamePane { error, .. } => error.as_deref(),
                _ => None,
            };
            plain_dialog_geometry(
                title,
                hint,
                error,
                confirm_label,
                input.filter(|_| dialog.is_multiline()),
                area,
            )
            .map(|(r, _)| r)
        }
    }
}

/// Hold a dialog drag to the offsets that actually move the box.
///
/// Past the screen edge [`shift_on_screen`] clamps the paint anyway, and an
/// offset that kept accumulating out there would have to be dragged all the
/// way back before the box moved again. A dialog too big for the terminal
/// to host has nowhere to go, so its offset is zero.
pub fn clamp_dialog_offset(dialog: &Dialog, area: Rect, offset: (i16, i16)) -> (i16, i16) {
    let Some(rect) = dialog_rect(dialog, area) else {
        return (0, 0);
    };
    let moved = shift_on_screen(rect, area, offset);
    (
        i16::try_from(i32::from(moved.x) - i32::from(rect.x)).unwrap_or(0),
        i16::try_from(i32::from(moved.y) - i32::from(rect.y)).unwrap_or(0),
    )
}

/// Paint a modal dialog centered in `area`, recording its buttons and its
/// title bar as hit regions. `hover` is the mouse cell; the button under it
/// highlights. `offset` is how far the operator has dragged the box off
/// center by its title bar.
pub fn paint_dialog(
    dialog: &Dialog,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
    offset: (i16, i16),
) {
    if let Dialog::Keybinds { scroll, rows } = dialog {
        paint_keybinds_dialog(*scroll, rows, area, buf, paint, hits, hover, offset);
        return;
    }
    if let Dialog::Settings {
        section,
        themes,
        view,
        sound,
    } = dialog
    {
        paint_settings_dialog(
            *section, themes, view, sound, area, buf, paint, hits, hover, offset,
        );
        return;
    }
    let (title, input, hint, confirm_label) = dialog_parts(dialog);
    let error = match dialog {
        Dialog::NamePane { error, .. } => error.as_deref(),
        _ => None,
    };
    let multiline = dialog.is_multiline();
    let Some((dialog_area, field_h)) = plain_dialog_geometry(
        title,
        hint,
        error,
        confirm_label,
        input.filter(|_| multiline),
        area,
    ) else {
        return;
    };
    let dialog_area = shift_on_screen(dialog_area, area, offset);
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
        let field = Rect::new(left, field_y, usable_w, field_h);
        buf.set_style(field, theme::dialog_input(paint));
        for (row, line) in field_tail(input, field.width, field_h).iter().enumerate() {
            super::overlay_text(
                buf,
                inner,
                field.x,
                field_y + row as u16,
                line,
                theme::dialog_input(paint),
            );
        }
    }
    if let Some(error) = error {
        let error_y = inner.y + 2 + field_h;
        let error_area = Rect::new(
            left,
            error_y,
            usable_w,
            inner.height.saturating_sub(error_y - inner.y + 1),
        );
        Paragraph::new(error)
            .style(theme::dialog_error(paint))
            .wrap(Wrap { trim: true })
            .render(error_area, buf);
    }
    paint_dialog_buttons(buf, inner, paint, hits, hover, confirm_label);
    paint_title_bar(buf, dialog_area, None, paint, hits);
}

/// Where a plain dialog lands and how many rows its field gets, or `None`
/// when the terminal cannot host it.
///
/// Two pieces of copy are hard-clipped by `overlay_text` and so set the
/// floor: the title, which is the dialog's question, and the action row,
/// which is how it gets answered. Cut either and the box misstates what it
/// is asking, so under that width nothing is painted at all. The hint and
/// the error never gate the box, because neither has to be cut: the hint
/// drops, the error wraps.
///
/// `grow_for` is the buffer of a multi-line field, and is `None` for every
/// single-line dialog. The field grows with what has been typed rather than
/// opening at full height, so a one-line message still gets a one-line
/// card.
fn plain_dialog_geometry(
    title: &str,
    hint: Option<&str>,
    error: Option<&str>,
    confirm_label: &str,
    grow_for: Option<&str>,
    area: Rect,
) -> Option<(Rect, u16)> {
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
    let usable_w = w.saturating_sub(DIALOG_CHROME_WIDTH);
    // 3. The field and the error slot both grow the card downward, as far as
    //    the terminal allows and no further: the card itself never leaves
    //    the screen. The field is served first — it is what is being typed.
    let mut slack = area.height - base_h;
    let field_h = grow_for
        .map(|text| u16::try_from(field_rows(text, usable_w)).unwrap_or(u16::MAX))
        .unwrap_or(1)
        .clamp(1, FIELD_MAX_ROWS)
        .min(slack + 1);
    slack -= field_h - 1;
    let error_lines = error
        .map(|error| wrapped_line_count(error, usable_w))
        .unwrap_or(0);
    let error_h = u16::try_from(error_lines).unwrap_or(u16::MAX).min(slack);
    let h = base_h + (field_h - 1) + error_h;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    Some((Rect::new(x, y, w, h), field_h))
}

/// The two actions every dialog offers, in paint order. One list, so the
/// width a dialog reserves and the row it paints cannot disagree.
fn dialog_buttons(confirm_label: &str) -> [(String, HitTarget, bool); 2] {
    // One space of padding inside each label, because the style fills the
    // label's own cells: without it the filled block ends on the glyph and
    // the button reads as highlighted text rather than as a button. A real
    // border was the other option and costs two rows on a one-row action
    // row, which the shortest dialogs do not have to give.
    [
        (
            format!(" ↵ {confirm_label} "),
            HitTarget::DialogConfirm,
            true,
        ),
        (
            format!(" Esc {} ", copy::BUTTON_CANCEL),
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

/// The settings card: section chips across the top, the showing
/// section's list under them with the saved row checked and the selected
/// row raised, and a muted line under the list saying what the list is
/// for. Applying a theme repaints the whole workspace, so its rows need
/// no swatch; the CLI's listing is the visual preview. The keybinds
/// section lists the active bindings instead, read-only, behind a
/// viewport ([`paint_keybind_rows`]), with a close control where the
/// other sections have Apply.
///
/// The saved marker is [`copy::MENU_CHECK`], the same glyph the app
/// menu's toggle uses, because a theme being on is the same kind of fact
/// as the drawer being open and should not need a second vocabulary. It rides
/// beside the selection highlight, so "which one is saved" and "which
/// one is the cursor on" stay separable without color.
#[allow(clippy::too_many_arguments)]
fn paint_settings_dialog(
    section: SettingsSection,
    themes: &ThemePicker,
    view: &ViewSwitches,
    sound: &SoundPicker,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
    offset: (i16, i16),
) {
    let footer = settings_footer(section, themes);
    let (frame_rows, footer_lines) = settings_frame(themes, view, sound, area);
    let Some((dialog_area, list_h)) = settings_dialog_geometry(frame_rows, footer_lines, area)
    else {
        return;
    };
    let dialog_area = shift_on_screen(dialog_area, area, offset);
    clear_area(buf, dialog_area);
    let block = overlay_block(paint);
    let inner = block.inner(dialog_area);
    block.render(dialog_area, buf);

    let left = inner.x + DIALOG_INSET;
    let usable_w = inner.width.saturating_sub(2 * DIALOG_INSET);
    let list = Rect::new(left, inner.y + 2, usable_w, list_h);

    let lines = settings_lines(section, themes, view, sound);
    let selected = match section {
        SettingsSection::Theme => themes.selected,
        SettingsSection::View => view.selected,
        SettingsSection::Sound => sound.selected,
    };
    paint_settings_rows(buf, inner, list, &lines, selected, paint, hits);

    let footer_y = list.y + list.height + 1;
    let bottom = inner.y + inner.height;
    if footer_y < bottom {
        let footer_area = Rect::new(
            left,
            footer_y,
            usable_w,
            (footer_lines as u16).min(bottom - footer_y),
        );
        Paragraph::new(footer)
            .style(theme::menu_hint(paint))
            .wrap(Wrap { trim: true })
            .render(footer_area, buf);
    }

    match section {
        // Enter flips the row under the cursor and the card stays, so
        // there is nothing to apply and Esc is the one key that leaves.
        SettingsSection::View => paint_close_row(buf, inner, paint, hits, hover, "Esc", None, 0),
        SettingsSection::Theme | SettingsSection::Sound => {
            paint_dialog_buttons(buf, inner, paint, hits, hover, copy::BUTTON_APPLY)
        }
    }
    paint_title_bar(buf, dialog_area, Some(copy::SETTINGS_TITLE), paint, hits);
    // After the title bar on purpose: the chips share its second row, and
    // `HitMap::hit` answers with the last region pushed over a cell, so a
    // chip wins its own cells and the rest of the row still drags.
    paint_section_chips(buf, inner, left, usable_w, section, paint, hits, hover);
}

/// A picking section's lines: the cursor's row raised, the checked row
/// marked, muted text where the section explains itself, and every row
/// a click target by its picker index.
///
/// The selected row stays visible: the window slides down only once the
/// arrows walk past its last visible line. Lines, not rows: a gap
/// between the sound section's groups takes a line no row is on.
fn paint_settings_rows(
    buf: &mut Buffer,
    inner: Rect,
    list: Rect,
    lines: &[SettingsLine<'_>],
    selected: usize,
    paint: &Paint,
    hits: &mut HitMap,
) {
    let list_h = usize::from(list.height);
    let cursor_line = lines
        .iter()
        .position(|line| matches!(line, SettingsLine::Row { index, .. } if *index == selected))
        .unwrap_or(0);
    let start = if list_h == 0 || cursor_line < list_h {
        0
    } else {
        cursor_line + 1 - list_h
    };
    for (offset, line) in lines[start..lines.len().min(start + list_h)]
        .iter()
        .enumerate()
    {
        let y = list.y + offset as u16;
        let SettingsLine::Row {
            index,
            label,
            checked,
        } = *line
        else {
            match *line {
                SettingsLine::Text(text) => {
                    super::overlay_text(buf, inner, list.x, y, text, theme::menu_hint(paint));
                }
                // Under the label, not the check: the note belongs to
                // the row above it and reads as its second line.
                SettingsLine::Note(text) => {
                    super::overlay_text(buf, inner, list.x + 2, y, text, theme::menu_hint(paint));
                }
                _ => {}
            }
            continue;
        };
        let style = if index == selected {
            theme::menu_row_hover(paint)
        } else {
            theme::menu_row(paint)
        };
        let rect = Rect::new(list.x, y, list.width, 1);
        buf.set_style(rect, style);
        let mark = if checked { copy::MENU_CHECK } else { " " };
        super::overlay_text(buf, inner, list.x, y, &format!("{mark} {label}"), style);
        hits.push(rect, HitTarget::SettingsRow { index });
    }
}

/// The keybinds card's rows: the chord in the focus color and the
/// action beside it, from the row the card is scrolled to. Read-only,
/// so no row is raised and none is a target. Returns the rows showing
/// (for the `first–last / all` count), or `None` when none fit.
fn paint_keybind_rows(
    buf: &mut Buffer,
    inner: Rect,
    list: Rect,
    scroll: u16,
    rows: &[crate::bindings::BindingHelp],
    paint: &Paint,
) -> Option<(usize, usize)> {
    let list_h = usize::from(list.height);
    if list_h == 0 || rows.is_empty() {
        return None;
    }
    let start = usize::from(scroll).min(rows.len().saturating_sub(list_h));
    let end = (start + list_h).min(rows.len());
    let key_w = rows
        .iter()
        .map(|row| Span::raw(row.keys.as_str()).width())
        .max()
        .unwrap_or(0)
        .min(usize::from(list.width.saturating_sub(4))) as u16;
    for (line, row) in rows[start..end].iter().enumerate() {
        let y = list.y + line as u16;
        super::overlay_text(
            buf,
            inner,
            list.x,
            y,
            &row.keys,
            theme::pane_border_focused(paint),
        );
        super::overlay_text(
            buf,
            inner,
            list.x + key_w + 3,
            y,
            &row.action,
            theme::menu_row(paint),
        );
    }
    Some((start, end))
}

/// The action row of a card, or a section, with nothing to apply: one
/// control closes it, named for the `keys` that do it (both keys on the
/// read-only keybinds card, Esc alone on the view section, whose Enter
/// is taken), and the rows `showing` are counted at the right,
/// the way the sidebar says `+3 more` for what is below its fold. Same
/// row and inset as the Apply row of the other sections.
#[allow(clippy::too_many_arguments)]
fn paint_close_row(
    buf: &mut Buffer,
    inner: Rect,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
    keys: &str,
    showing: Option<(usize, usize)>,
    total: usize,
) {
    let row_y = inner.y + inner.height.saturating_sub(1);
    let left = inner.x + DIALOG_INSET;
    let usable_w = inner.width.saturating_sub(2 * DIALOG_INSET);
    let close = format!(" {keys} {} ", copy::BUTTON_CLOSE);
    let close_w = text_width(&close);
    if close_w > usable_w {
        return;
    }
    let rect = Rect::new(left, row_y, close_w, 1);
    let hovered =
        hover.is_some_and(|(hc, hr)| hr == rect.y && hc >= rect.x && hc < rect.x + rect.width);
    let style = if hovered {
        theme::dialog_primary(paint).add_modifier(Modifier::UNDERLINED)
    } else {
        theme::dialog_primary(paint)
    };
    super::overlay_text(buf, inner, rect.x, rect.y, &close, style);
    hits.push(rect, HitTarget::DialogCancel);

    let Some((start, end)) = showing else {
        return;
    };
    let progress = format!("{}–{} / {}", start + 1, end, total);
    let progress_w = text_width(&progress);
    let x = left.saturating_add(usable_w.saturating_sub(progress_w));
    if x >= rect.x + rect.width + DIALOG_BUTTON_GAP {
        super::overlay_text(buf, inner, x, row_y, &progress, theme::menu_hint(paint));
    }
}

/// The section chips, Tab's targets made visible: every section named,
/// the showing one filled the way the primary button is, the others
/// muted until the pointer is over them.
#[allow(clippy::too_many_arguments)]
fn paint_section_chips(
    buf: &mut Buffer,
    inner: Rect,
    left: u16,
    usable_w: u16,
    showing: SettingsSection,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    let right = left.saturating_add(usable_w);
    let mut x = left;
    for section in SettingsSection::ALL {
        let text = format!(" {} ", section.label());
        let w = text_width(&text);
        if w > right.saturating_sub(x) {
            break;
        }
        let rect = Rect::new(x, inner.y, w, 1);
        let hovered =
            hover.is_some_and(|(hc, hr)| hr == rect.y && hc >= rect.x && hc < rect.x + rect.width);
        let style = if section == showing {
            theme::dialog_primary(paint)
        } else if hovered {
            theme::menu_row_hover(paint)
        } else {
            theme::menu_hint(paint)
        };
        super::overlay_text(buf, inner, x, inner.y, &text, style);
        hits.push(rect, HitTarget::SettingsSection { section });
        x = x.saturating_add(w + DIALOG_BUTTON_GAP);
    }
}

fn settings_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).min(SETTINGS_MAX_WIDTH)
}

/// One line of a settings list: a row the cursor can be on, or the
/// blank line that keeps the sound section's two groups apart.
#[derive(Debug, Clone, Copy)]
enum SettingsLine<'a> {
    Row {
        /// The picker's row index: what the cursor and a click name.
        index: usize,
        label: &'a str,
        /// Whether the check is on this row: the choice Enter would
        /// save, which follows the cursor within its group.
        checked: bool,
    },
    /// A muted line the cursor skips: an explanation, or a heading.
    Text(&'a str),
    /// A muted line under a row, indented to its label: what the row
    /// above is. The cursor skips it the way it skips `Text`.
    Note(&'a str),
    Gap,
}

/// The lines the showing section lists.
fn settings_lines<'a>(
    section: SettingsSection,
    themes: &'a ThemePicker,
    view: &'a ViewSwitches,
    sound: &'a SoundPicker,
) -> Vec<SettingsLine<'a>> {
    match section {
        SettingsSection::Theme => theme_lines(themes),
        SettingsSection::View => view_lines(view),
        SettingsSection::Sound => sound_lines(sound),
    }
}

/// Each surface's row, checked while it shows, with one muted line
/// under it saying what the surface is, so a name like "Files" does
/// not have to be recognised to be chosen. A blank line between the
/// pairs keeps each note with its own row.
fn view_lines(view: &ViewSwitches) -> Vec<SettingsLine<'static>> {
    let mut lines = Vec::with_capacity(view.len() * 3);
    for index in 0..view.len() {
        let Some(row) = view.row(index) else {
            break;
        };
        let (label, note) = match row {
            ViewRow::TabBar => (copy::VIEW_TAB_BAR, copy::VIEW_TAB_BAR_NOTE),
            ViewRow::Files => (copy::VIEW_FILES, copy::VIEW_FILES_NOTE),
        };
        if index > 0 {
            lines.push(SettingsLine::Gap);
        }
        lines.push(SettingsLine::Row {
            index,
            label,
            checked: view.is_checked(index),
        });
        lines.push(SettingsLine::Note(note));
    }
    lines
}

/// One list, one check, and it is on the cursor: the theme under it is
/// the one Enter would apply, and is already live as a preview.
fn theme_lines(themes: &ThemePicker) -> Vec<SettingsLine<'_>> {
    themes
        .names
        .iter()
        .enumerate()
        .map(|(index, name)| SettingsLine::Row {
            index,
            label: name.as_str(),
            checked: index == themes.selected,
        })
        .collect()
}

/// What the switch is for, the switch's rows, then the sounds under
/// their own heading: two groups that each check their own row, kept
/// apart so two checks read as two answers and not a contradiction.
fn sound_lines(sound: &SoundPicker) -> Vec<SettingsLine<'_>> {
    let mut lines = Vec::with_capacity(sound.len() + 5);
    lines.push(SettingsLine::Text(copy::SOUND_INTRO));
    lines.push(SettingsLine::Gap);
    for index in 0..sound.len() {
        let Some(row) = sound.row(index) else {
            break;
        };
        if index == SoundPicker::SWITCH_ROWS {
            lines.push(SettingsLine::Gap);
            lines.push(SettingsLine::Text(copy::SOUND_LIST_HEADING));
        }
        let label = match row {
            SoundRow::Switch(true) => copy::SOUND_NOTIFS_ON,
            SoundRow::Switch(false) => copy::SOUND_NOTIFS_OFF,
            SoundRow::Sound(name) if name == crate::sound::SYSTEM => copy::SOUND_SYSTEM,
            SoundRow::Sound(name) => name,
        };
        lines.push(SettingsLine::Row {
            index,
            label,
            checked: sound.is_checked(index),
        });
    }
    lines
}

/// The muted line under the list. The theme section's says how to drive
/// the card, or what an apply that could not go live had to say, or
/// where themes come from when there are none to list. The view
/// section's says its rows flip at once, the one way it differs from
/// the pickers. The sound section says its piece at the top
/// ([`sound_lines`]) and has none.
fn settings_footer(section: SettingsSection, themes: &ThemePicker) -> &str {
    match section {
        SettingsSection::Theme if themes.names.is_empty() => copy::THEMES_EMPTY,
        SettingsSection::Theme => themes.notice.as_deref().unwrap_or(copy::THEMES_HINT),
        SettingsSection::View => copy::VIEW_HINT,
        SettingsSection::Sound => "",
    }
}

/// The list rows and footer lines the card is sized for: the most any
/// section needs, so one card fits every section and Tab never resizes
/// it. Every footer any section can show is counted, so an apply notice
/// arriving does not move the box either.
fn settings_frame(
    themes: &ThemePicker,
    view: &ViewSwitches,
    sound: &SoundPicker,
    area: Rect,
) -> (usize, usize) {
    let text_width = settings_width(area).saturating_sub(DIALOG_CHROME_WIDTH);
    let rows = theme_lines(themes)
        .len()
        .max(view_lines(view).len())
        .max(sound_lines(sound).len());
    let footers = [
        copy::THEMES_HINT,
        copy::THEMES_EMPTY,
        copy::VIEW_HINT,
        themes.notice.as_deref().unwrap_or(""),
    ];
    let footer_lines = footers
        .iter()
        .map(|footer| wrapped_line_count(footer, text_width))
        .max()
        .unwrap_or(0);
    (rows, footer_lines)
}

/// Fixed rows (chips, one blank before the list, one after, the footer,
/// the action row) around a list that shrinks before the chrome does.
fn settings_dialog_geometry(
    row_count: usize,
    footer_lines: usize,
    area: Rect,
) -> Option<(Rect, u16)> {
    if area.width < 8 || area.height < 6 {
        return None;
    }
    let width = settings_width(area);
    // Chips, one blank line before the list, one after it, and the action
    // row: 4 fixed rows around the list, plus the footer. The title is in
    // the border (`paint_title_bar`).
    let wanted_height =
        u16::try_from(row_count.saturating_add(footer_lines).saturating_add(6)).unwrap_or(u16::MAX);
    let height = wanted_height.min(area.height.saturating_sub(2)).max(6);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let list_height = height
        .saturating_sub(2)
        .saturating_sub(4)
        .saturating_sub(u16::try_from(footer_lines).unwrap_or(u16::MAX));
    Some((dialog, list_height))
}

/// The keybinding reference, a card of its own: one muted line saying
/// how to move, the chord and action of every active binding behind a
/// viewport, and one close control with the rows showing counted beside
/// it. The card owns only a scroll offset; its rows come from the active
/// router map rather than hardcoded documentation.
#[allow(clippy::too_many_arguments)]
fn paint_keybinds_dialog(
    scroll: u16,
    rows: &[crate::bindings::BindingHelp],
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
    offset: (i16, i16),
) {
    let Some((dialog_area, list_h)) = keybind_dialog_geometry(rows.len(), area) else {
        return;
    };
    let dialog_area = shift_on_screen(dialog_area, area, offset);
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
        copy::KEYBINDS_HINT,
        theme::menu_hint(paint),
    );
    let list = Rect::new(left, inner.y + 2, usable_w, list_h);
    let showing = paint_keybind_rows(buf, inner, list, scroll, rows, paint);
    paint_close_row(
        buf,
        inner,
        paint,
        hits,
        hover,
        "↵ / Esc",
        showing,
        rows.len(),
    );
    paint_title_bar(buf, dialog_area, Some(copy::KEYBINDS_TITLE), paint, hits);
}

/// Tallest the keybinds card may grow: thirteen rows of bindings behind
/// the chrome. The list scrolls, so the card stays a card rather than
/// growing into a wall for forty bindings.
const KEYBINDS_MAX_HEIGHT: u16 = 19;

/// The card and its list height. The hint, one blank line before the
/// list, one after it, and the close row are 4 fixed rows around a list
/// that shrinks before the chrome does; the title is in the border
/// (`paint_title_bar`).
fn keybind_dialog_geometry(row_count: usize, area: Rect) -> Option<(Rect, u16)> {
    if area.width < 8 || area.height < 6 {
        return None;
    }
    let width = area.width.saturating_sub(4).min(KEYBINDS_MAX_WIDTH);
    let wanted_height = u16::try_from(row_count.saturating_add(6)).unwrap_or(u16::MAX);
    let height = wanted_height
        .min(area.height.saturating_sub(2))
        .clamp(6, KEYBINDS_MAX_HEIGHT);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let list_height = height.saturating_sub(2).saturating_sub(4);
    Some((dialog, list_height))
}

/// Largest meaningful keybinds-card offset for this terminal area: the
/// rows past the list the card has room for.
pub fn keybind_max_scroll(row_count: usize, area: Rect) -> u16 {
    let Some((_, list_height)) = keybind_dialog_geometry(row_count, area) else {
        return 0;
    };
    if list_height == 0 {
        return 0;
    }
    u16::try_from(row_count.saturating_sub(usize::from(list_height))).unwrap_or(u16::MAX)
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
    /// Whether the right messages drawer is open.
    pub messages: bool,
}

/// One menu row: its label, what clicking it does, and whether it is a
/// setting that is currently on.
///
/// `None` is "not a toggle", which is different from `Some(false)`: an
/// unchecked row still aligns with the check column while any check in the
/// menu is lit, and a menu showing no lit check reserves nothing.
pub type MenuRow = (&'static str, BindingAction, Option<bool>);

/// Menu items for one open menu.
pub fn menu_items(menu: &MenuState, checks: MenuChecks) -> Vec<MenuRow> {
    match menu {
        MenuState::None => Vec::new(),
        MenuState::AppMenu => vec![
            // New tab and new session are not here: the tab strip's `+`
            // and the sidebar's session button already carry them, and
            // the menu is for what has no chrome of its own. Rule 2 in
            // `render/mod.rs`: a surface that is put away has no chrome
            // left to host its own control, so the menu carries it. The
            // composer's `@` button is painted in the sidebar footer,
            // which a collapsed sidebar does not have, and the one-column
            // rail only has room for the chevron. Without this row the
            // composer is keyboard-only the moment the sidebar is closed.
            (copy::MENU_COMPOSE, BindingAction::Compose, None),
            (
                copy::MENU_TOGGLE_MESSAGES,
                BindingAction::ToggleMessages,
                Some(checks.messages),
            ),
            // Settings opens a card rather than flipping anything, so it
            // carries no check of its own; the card marks the theme, the
            // surfaces showing (its View section holds the tab strip's
            // and file panel's switches, which have no chords, and the
            // file panel's seam can close it with no seam left to grab)
            // and the sound row that are on with the same glyph.
            (copy::MENU_SETTINGS, BindingAction::ShowSettings, None),
            // The keybinding reference is a card of its own: nothing on
            // it is a setting, and "what are the keys" is the first thing
            // a new operator asks. This row opens it, as `Ctrl+B ?` does.
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
    // labels line up down the list — but only while some check is actually
    // lit. A column of blanks held for checks that are all off pushed every
    // label three cells off the border for nothing. Alignment cannot break
    // under the operator's pointer: a click always closes the menu
    // (`app::handle_mouse`), so the gutter is decided once per open.
    let gutter: u16 = if items.iter().any(|(_, _, c)| *c == Some(true)) {
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
        let on = MenuChecks { messages: true };
        let rows = menu_items(&MenuState::AppMenu, on);
        let checked = |rows: &[MenuRow], label: &str| {
            rows.iter()
                .find(|(l, _, _)| *l == label)
                .unwrap_or_else(|| panic!("{label} is not in the app menu"))
                .2
        };

        assert_eq!(checked(&rows, copy::MENU_TOGGLE_MESSAGES), Some(true));
        // Not a toggle: opening the settings card is not a setting, and
        // reserving a check for it would imply it could be on.
        assert_eq!(checked(&rows, copy::MENU_COMPOSE), None);
        assert_eq!(checked(&rows, copy::MENU_SETTINGS), None);
        assert_eq!(checked(&rows, copy::MENU_KEYBINDS), None);
        assert_eq!(checked(&rows, copy::MENU_DETACH), None);

        let off = MenuChecks::default();
        let rows = menu_items(&MenuState::AppMenu, off);
        assert_eq!(checked(&rows, copy::MENU_TOGGLE_MESSAGES), Some(false));
    }

    /// The check column is reserved for the whole menu or none of it, so
    /// labels do not step sideways while a check shows — and it exists only
    /// while one does. A menu with no toggles, or whose toggles are all
    /// off, pays nothing for a gutter no check is using.
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

        let app_on = render(&mut term, MenuState::AppMenu, MenuChecks { messages: true });
        let messages = app_on
            .iter()
            .find(|line| line.contains(copy::MENU_TOGGLE_MESSAGES))
            .expect("a Messages row");
        let settings = app_on
            .iter()
            .find(|line| line.contains(copy::MENU_SETTINGS))
            .expect("a Settings row");
        assert!(
            messages.contains(&format!(
                "{} {}",
                copy::MENU_CHECK,
                copy::MENU_TOGGLE_MESSAGES
            )),
            "an on toggle carries the check: {messages}"
        );
        assert!(
            !settings.contains(copy::MENU_CHECK),
            "a row that is not a toggle carries no mark of its own: {settings}"
        );
        // Column, not byte offset: the check is three bytes and a space is
        // one, so `find` alone reports a two-byte gap that is not on screen.
        let column_of = |line: &str, needle: &str| {
            line.find(needle)
                .map(|byte| line[..byte].chars().count())
                .expect("the label is on this line")
        };
        assert_eq!(
            column_of(messages, copy::MENU_TOGGLE_MESSAGES),
            column_of(settings, copy::MENU_SETTINGS),
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

        // And so does a menu whose toggles are all off: a column of blanks
        // held for checks nobody lit is exactly the three-cell indent this
        // rule exists to prevent. Same anchor as `app_on`, so the columns
        // are comparable.
        let app_off = render(&mut term, MenuState::AppMenu, MenuChecks::default());
        let settings_off = app_off
            .iter()
            .find(|line| line.contains(copy::MENU_SETTINGS))
            .expect("a Settings row");
        assert_eq!(
            column_of(settings, copy::MENU_SETTINGS) - column_of(settings_off, copy::MENU_SETTINGS),
            2,
            "an all-off menu drops the two-cell check gutter"
        );
    }

    /// The app menu is the visible route to what has no chrome of its
    /// own and no home elsewhere. Everything else stays out: new tab and
    /// new session have their own buttons, the tab strip's and file
    /// panel's switches live on the settings card's View section, and
    /// the event stream is off (`persist::STREAM_TAB`), so a row for it
    /// would promise a surface the sidebar will not paint.
    #[test]
    fn app_menu_lists_only_what_has_no_chrome_of_its_own() {
        let actions: Vec<_> = menu_items(&MenuState::AppMenu, MenuChecks::default())
            .iter()
            .map(|(_, action, _)| *action)
            .collect();
        assert_eq!(
            actions,
            vec![
                // The composer's `@` button is painted in the sidebar
                // footer, and a collapsed sidebar keeps only the
                // one-column rail, so without this row the composer is
                // keyboard-only the moment the sidebar is closed. Rule 2
                // in `render/mod.rs` puts exactly that case here.
                BindingAction::Compose,
                BindingAction::ToggleMessages,
                BindingAction::ShowSettings,
                // The keybinding reference is its own card, and the
                // first thing a new operator reaches for.
                BindingAction::ShowKeybinds,
                BindingAction::Detach,
            ]
        );
    }

    /// The settings card open on its theme section.
    fn theme_card(
        names: Vec<String>,
        selected: usize,
        active: Option<usize>,
        notice: Option<String>,
    ) -> Dialog {
        Dialog::Settings {
            section: SettingsSection::Theme,
            themes: ThemePicker {
                names,
                selected,
                active,
                notice,
            },
            view: ViewSwitches::new(true, true),
            sound: SoundPicker::new(
                false,
                vec!["bow-ripple".into(), crate::sound::SYSTEM.into()],
                "bow-ripple",
            ),
        }
    }

    /// The keybinds card with `count` generated rows scrolled to `scroll`.
    fn keybinds_card(count: usize, scroll: u16) -> Dialog {
        Dialog::Keybinds {
            scroll,
            rows: (0..count)
                .map(|index| crate::bindings::BindingHelp {
                    keys: format!("Ctrl+{}", index % 26),
                    action: format!("Action {index}"),
                })
                .collect(),
        }
    }

    /// Paint one dialog into a fixed terminal at a given drag offset.
    fn draw_dialog(dialog: &Dialog, offset: (i16, i16)) -> (Buffer, HitMap) {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_dialog(
                dialog,
                f.area(),
                f.buffer_mut(),
                &Paint::for_test(),
                &mut hits,
                None,
                offset,
            );
        })
        .unwrap();
        (term.backend().buffer().clone(), hits)
    }

    fn composer(buffer: &str) -> Dialog {
        Dialog::Compose {
            buffer: buffer.into(),
            status: None,
            send: crate::dialog::ComposeSendState::Ready,
        }
    }

    #[test]
    fn abandon_confirmation_names_the_safe_retry_loss() {
        let message = crate::dialog::parse_compose("@reviewer ship it").expect("message");
        let dialog = Dialog::Compose {
            buffer: "@reviewer ship it".into(),
            status: Some("sending to reviewer…".into()),
            send: crate::dialog::ComposeSendState::ConfirmAbandon {
                attempt: crate::dialog::ComposeAttempt {
                    message,
                    client_key: "stable-key".into(),
                },
                resume: crate::dialog::ComposeResume::Sending,
            },
        };

        assert_eq!(
            dialog_parts(&dialog),
            (
                copy::COMPOSE_ABANDON_TITLE,
                None,
                Some(copy::COMPOSE_ABANDON_HINT),
                copy::BUTTON_ABANDON,
            )
        );
    }

    /// One line of message gets one row; a paragraph grows the field and
    /// the card under it. A composer that opened at full height would make
    /// the common case look like a form.
    #[test]
    fn the_composer_field_grows_with_what_has_been_typed() {
        let area = Rect::new(0, 0, 80, 24);
        let rows_for = |text: &str| {
            let (_, field_h) = plain_dialog_geometry(
                copy::COMPOSE_TITLE,
                Some(copy::COMPOSE_HINT),
                None,
                copy::BUTTON_SEND,
                Some(text),
                area,
            )
            .expect("the composer fits");
            field_h
        };
        assert_eq!(rows_for("@reviewer ship it"), 1);
        assert_eq!(rows_for("@reviewer one\ntwo\nthree"), 3);
        assert_eq!(
            rows_for(&format!("@reviewer{}", "\nx".repeat(40))),
            FIELD_MAX_ROWS,
            "past the cap the field scrolls instead of taking the terminal"
        );

        // And the card grew with it, rather than the field painting over
        // the action row.
        let (short, _) = draw_dialog(&composer("@reviewer ship it"), (0, 0));
        let (tall, _) = draw_dialog(&composer("@reviewer one\ntwo\nthree"), (0, 0));
        assert_eq!(
            row_of(&tall, "↵ Send") - row_of(&short, "↵ Send"),
            1,
            "two extra field rows push the action row down by two on a \
             centered card, which moves its top up by one"
        );
    }

    /// The field shows the end of the message, because that is where the
    /// cursor is. A field that scrolled off what was being typed would be
    /// worse than no field.
    #[test]
    fn a_long_message_shows_its_tail() {
        let lines: Vec<String> = (0..12).map(|n| format!("line{n}")).collect();
        let (buf, _) = draw_dialog(&composer(&format!("@rev {}", lines.join("\n"))), (0, 0));
        let flat = flatten(&buf);
        assert!(flat.contains("line11"), "the last line must be on screen");
        assert!(
            !flat.contains("line0 "),
            "the head scrolled off, not the tail: {flat}"
        );
    }

    /// Every dialog is picked up by its top border, moves with the pointer,
    /// and stops at the screen edge with its action row still reachable.
    #[test]
    fn a_dialog_moves_by_its_title_bar_and_stops_at_the_edge() {
        let area = Rect::new(0, 0, 80, 24);
        for dialog in [
            composer("@reviewer ship it"),
            Dialog::confirm_close("%0"),
            theme_card(vec!["dark".into(), "light".into()], 0, Some(0), None),
            keybinds_card(40, 0),
        ] {
            let rest = dialog_rect(&dialog, area).expect("a full terminal hosts it");
            let (_, hits) = draw_dialog(&dialog, (0, 0));
            assert!(
                matches!(
                    hits.hit(rest.x + rest.width / 2, rest.y),
                    Some(HitTarget::DialogTitleBar)
                ),
                "{dialog:?}: the top border must pick the box up"
            );

            // A modest drag lands exactly where it was dragged to. One cell,
            // a drag every card has room for in an 80-column terminal.
            assert_eq!(clamp_dialog_offset(&dialog, area, (1, 1)), (1, 1));

            // A drag off the edge stops at it, so the offset the app keeps
            // is the one that still moves the box. Anything larger would
            // have to be dragged back before the box budged.
            let (dx, dy) = clamp_dialog_offset(&dialog, area, (999, 999));
            let landed = shift_on_screen(rest, area, (dx, dy));
            assert_eq!(landed.x + landed.width, area.width);
            assert_eq!(landed.y + landed.height, area.height);
            assert_eq!(
                clamp_dialog_offset(&dialog, area, (i16::MIN, i16::MIN)),
                (
                    -i16::try_from(rest.x).unwrap(),
                    -i16::try_from(rest.y).unwrap()
                ),
                "{dialog:?}: the far corner is the origin, not off screen"
            );
        }
    }

    /// `dialog_rect` and `paint_dialog` each match on the dialog to pick a
    /// geometry. This is what keeps the two matches saying the same thing:
    /// a variant added to one and missed in the other drags a box that is
    /// painted somewhere else.
    #[test]
    fn every_dialog_is_dragged_to_where_it_is_painted() {
        let area = Rect::new(0, 0, 80, 24);
        for dialog in [
            Dialog::confirm_close("%0"),
            Dialog::NewTab {
                buffer: "review".into(),
            },
            Dialog::NamePane {
                pane_id: "%0".into(),
                buffer: "rev".into(),
                error: Some("that name is taken".into()),
            },
            Dialog::RenameTab {
                window_id: "@0".into(),
                buffer: "build".into(),
            },
            Dialog::ConfirmCloseTab {
                window_id: "@0".into(),
            },
            Dialog::RenameWorkspace {
                session: "main".into(),
                buffer: "main".into(),
            },
            Dialog::ConfirmCloseWorkspace {
                session: "main".into(),
            },
            composer("@reviewer one\ntwo\nthree"),
            keybinds_card(40, 5),
            theme_card(Vec::new(), 0, None, None),
            theme_card(
                vec!["dark".into()],
                0,
                Some(0),
                Some("the daemon is not running".into()),
            ),
        ] {
            let rect = dialog_rect(&dialog, area).expect("a full terminal hosts it");
            let (_, hits) = draw_dialog(&dialog, (0, 0));
            let painted = hits
                .regions()
                .iter()
                .find(|region| region.target == HitTarget::DialogTitleBar)
                .map(|region| region.rect)
                .unwrap_or_else(|| panic!("{dialog:?} painted no title bar"));
            assert_eq!(
                (painted.x, painted.y, painted.width),
                (rect.x, rect.y, rect.width),
                "{dialog:?}: dragged from one rect, painted at another"
            );
        }
    }

    /// The name sits at the left of the top border and the grip at its
    /// right, and neither answers the pointer: the strip reads as a header
    /// rather than a control, and the grip alone says the card drags.
    #[test]
    fn the_title_bar_names_the_card_left_and_wears_the_grip_right() {
        let theme = Paint::for_test();
        let dialog = theme_card(vec!["dark".into()], 0, Some(0), None);
        let draw = |hover: Option<(u16, u16)>| -> (Buffer, HitMap) {
            let mut term = Terminal::new(TestBackend::new(72, 24)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_dialog(
                    &dialog,
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    hover,
                    (0, 0),
                );
            })
            .unwrap();
            (term.backend().buffer().clone(), hits)
        };

        let (rest, hits) = draw(None);
        let (cx, cy) = box_corner(&rest);
        let width = hits
            .regions()
            .iter()
            .find(|region| region.target == HitTarget::DialogTitleBar)
            .map(|region| region.rect.width)
            .expect("the header drags");
        let top: String = (cx..cx + width).map(|x| rest[(x, cy)].symbol()).collect();
        assert!(top.starts_with("╭─ Settings ─"), "the name leads: {top}");
        assert!(
            top.ends_with(&format!("{PANE_GRIP}─╮")),
            "the grip trails: {top}"
        );
        assert!(
            rest[(cx + 3, cy)].modifier.contains(Modifier::BOLD),
            "the name is bold at rest"
        );

        // The second row's probe is past the chips, which win their own
        // cells (`settings_card_offers_its_sections_as_chips_over_the_grab_strip`).
        for hover in [(cx + 3, cy), (cx + width / 2, cy), (cx + width - 2, cy + 1)] {
            assert!(
                matches!(hits.hit(hover.0, hover.1), Some(HitTarget::DialogTitleBar)),
                "the whole header drags"
            );
            let (pointed, _) = draw(Some(hover));
            for x in cx..cx + width {
                assert_eq!(
                    pointed[(x, cy)],
                    rest[(x, cy)],
                    "the border does not answer the pointer at {hover:?}"
                );
            }
        }
    }

    /// A plain dialog's title is its question and stays inside the card;
    /// its border carries the grip alone.
    #[test]
    fn a_plain_dialog_keeps_its_question_inside_and_the_grip_in_the_border() {
        let (buf, hits) = draw_dialog(&Dialog::confirm_close("%0"), (0, 0));
        let (cx, cy) = box_corner(&buf);
        let width = hits
            .regions()
            .iter()
            .find(|region| region.target == HitTarget::DialogTitleBar)
            .map(|region| region.rect.width)
            .expect("the header drags");
        let top: String = (cx..cx + width).map(|x| buf[(x, cy)].symbol()).collect();
        assert!(top.starts_with("╭───"), "{top}");
        assert!(top.ends_with(&format!("{PANE_GRIP}─╮")), "{top}");
        assert_eq!(row_of(&buf, copy::CONFIRM_CLOSE_PANE), cy + 1);
    }

    #[test]
    fn settings_card_checks_and_raises_the_selected_row_and_offers_apply() {
        let backend = TestBackend::new(60, 16);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let dialog = theme_card(
            vec!["dark".into(), "light".into(), "solar".into()],
            1,
            Some(0),
            None,
        );
        term.draw(|f| {
            paint_dialog(
                &dialog,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
                (0, 0),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        let flat = flatten(buf);
        assert!(flat.contains("Settings"), "title renders: {flat}");
        assert!(
            flat.contains("✓ light") && !flat.contains("✓ dark"),
            "the check is on the cursor, not the saved theme: {flat}"
        );
        assert!(flat.contains("dark") && flat.contains("solar"), "{flat}");
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
        let dialog = theme_card(
            vec!["dark".into()],
            0,
            Some(0),
            Some(copy::THEME_SAVED_NO_DAEMON.into()),
        );
        term.draw(|f| {
            paint_dialog(
                &dialog,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
                (0, 0),
            );
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("The next command picks it up."),
            "the saved story stays visible: {flat}"
        );
    }

    /// Tab's targets made visible: both sections named on the row under
    /// the border, the showing one lit, each a click target that wins its
    /// cells from the grab strip while the rest of that row still drags.
    #[test]
    fn settings_card_offers_its_sections_as_chips_over_the_grab_strip() {
        let (buf, hits) = draw_dialog(&theme_card(vec!["dark".into()], 0, Some(0), None), (0, 0));
        let flat = flatten(&buf);
        assert!(
            flat.contains(copy::SETTINGS_SECTION_THEME)
                && flat.contains(copy::SETTINGS_SECTION_VIEW)
                && flat.contains(copy::SETTINGS_SECTION_SOUND),
            "every section is named: {flat}"
        );
        let chip = |section: SettingsSection| {
            hits.regions()
                .iter()
                .find(|region| region.target == HitTarget::SettingsSection { section })
                .map(|region| region.rect)
                .unwrap_or_else(|| panic!("{section:?} has no chip"))
        };
        let theme_chip = chip(SettingsSection::Theme);
        let view_chip = chip(SettingsSection::View);
        let sound_chip = chip(SettingsSection::Sound);
        assert_eq!(theme_chip.y, view_chip.y, "one row of chips");
        assert_eq!(theme_chip.y, sound_chip.y, "one row of chips");
        assert!(
            view_chip.x > theme_chip.x + theme_chip.width
                && sound_chip.x > view_chip.x + view_chip.width,
            "in Tab order"
        );
        assert_eq!(
            hits.hit(sound_chip.x, sound_chip.y),
            Some(&HitTarget::SettingsSection {
                section: SettingsSection::Sound
            }),
            "a chip wins its own cells"
        );
        assert!(
            matches!(
                hits.hit(sound_chip.x + sound_chip.width + 4, sound_chip.y),
                Some(HitTarget::DialogTitleBar)
            ),
            "past the chips the row still drags"
        );
        assert_ne!(
            buf[(theme_chip.x + 1, theme_chip.y)].bg,
            buf[(sound_chip.x + 1, sound_chip.y)].bg,
            "the showing section's chip is lit"
        );
    }

    /// Every listed row is a click target naming its index, the mouse's
    /// half of the arrows, in both sections.
    #[test]
    fn settings_rows_are_click_targets() {
        let (_, hits) = draw_dialog(
            &theme_card(vec!["dark".into(), "light".into()], 0, Some(0), None),
            (0, 0),
        );
        let row = |index: usize| {
            hits.regions()
                .iter()
                .find(|region| region.target == HitTarget::SettingsRow { index })
                .map(|region| region.rect)
                .unwrap_or_else(|| panic!("row {index} is not a target"))
        };
        let (dark, light) = (row(0), row(1));
        assert_eq!(light.y, dark.y + 1, "one row each, in list order");
        assert_eq!(
            hits.hit(light.x + light.width - 1, light.y),
            Some(&HitTarget::SettingsRow { index: 1 }),
            "the whole row answers, not just its text"
        );
        assert!(
            !hits
                .regions()
                .iter()
                .any(|region| region.target == HitTarget::SettingsRow { index: 2 }),
            "no target for a row the list does not have"
        );

        let mut sound = theme_card(vec!["dark".into()], 0, Some(0), None);
        if let Dialog::Settings { section, .. } = &mut sound {
            *section = SettingsSection::Sound;
        }
        let (buf, hits) = draw_dialog(&sound, (0, 0));
        let off = row_of(&buf, copy::SOUND_NOTIFS_OFF);
        assert!(
            matches!(
                hits.hit(buf.area.width / 2, off),
                Some(HitTarget::SettingsRow { index: 1 })
            ),
            "the switch's rows are targets too"
        );
    }

    /// One card for every section: switching to the shorter list does
    /// not shrink the box, and an apply notice arriving does not move
    /// it. The empty space under a short list is the price of a box
    /// that holds still under Tab.
    #[test]
    fn the_settings_card_keeps_one_size_across_sections() {
        let area = Rect::new(0, 0, 80, 24);
        let names: Vec<String> = [
            "dark", "light", "solar", "mono", "paper", "sage", "ink", "dusk", "dawn", "fog",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let theme = theme_card(names.clone(), 0, Some(0), None);
        let mut sound = theme_card(names.clone(), 0, Some(0), None);
        if let Dialog::Settings { section, .. } = &mut sound {
            *section = SettingsSection::Sound;
        }
        let noticed = theme_card(
            vec!["dark".into()],
            0,
            Some(0),
            Some(copy::THEME_SAVED_NO_DAEMON.into()),
        );
        let plain = theme_card(vec!["dark".into()], 0, Some(0), None);

        let rect = |dialog: &Dialog| dialog_rect(dialog, area).expect("fits");
        assert_eq!(rect(&theme), rect(&sound), "Tab does not resize the card");
        assert_eq!(
            rect(&plain).height,
            rect(&noticed).height,
            "the apply notice was sized for before it arrived"
        );
        assert!(
            rect(&theme).height > rect(&plain).height,
            "the list still sizes the card: ten themes need more than the sound list"
        );
        let (buf, _) = draw_dialog(&sound, (0, 0));
        let flat = flatten(&buf);
        assert!(flat.contains(copy::SOUND_NOTIFS_ON) && flat.contains(copy::SOUND_LIST_HEADING));
    }

    /// The view section: the surfaces the app menu used to switch, each
    /// row checked while its surface shows, one muted line under each
    /// saying what the surface is, and no Apply: the rows flip at once,
    /// the footer says so, and Esc is the way out.
    #[test]
    fn view_section_lists_the_surfaces_with_their_notes() {
        let mut dialog = theme_card(vec!["dark".into()], 0, Some(0), None);
        if let Dialog::Settings { section, view, .. } = &mut dialog {
            *section = SettingsSection::View;
            *view = ViewSwitches::new(true, false);
        }
        let (buf, hits) = draw_dialog(&dialog, (0, 0));
        let flat = flatten(&buf);
        assert!(
            flat.contains(&format!("{} {}", copy::MENU_CHECK, copy::VIEW_TAB_BAR)),
            "a showing surface wears the check: {flat}"
        );
        assert!(
            flat.contains(&format!("  {}", copy::VIEW_FILES))
                && !flat.contains(&format!("{} {}", copy::MENU_CHECK, copy::VIEW_FILES)),
            "a hidden one does not: {flat}"
        );
        assert!(
            !flat.contains("dark"),
            "the theme list is not showing: {flat}"
        );

        // Row, note, blank line, row, note: each note sits under its own
        // row, starting where the label starts, and neither note is a
        // row itself (no target).
        let tab_bar = row_of(&buf, copy::VIEW_TAB_BAR);
        let tab_note = row_of(&buf, copy::VIEW_TAB_BAR_NOTE);
        let files = row_of(&buf, copy::VIEW_FILES);
        let files_note = row_of(&buf, copy::VIEW_FILES_NOTE);
        assert_eq!(tab_note, tab_bar + 1, "the note sits under its row");
        assert_eq!(files, tab_note + 2, "a blank line, then the next pair");
        assert_eq!(files_note, files + 1);
        assert!(hits.hit(buf.area.width / 2, tab_note).is_none());
        assert!(hits.hit(buf.area.width / 2, files_note).is_none());
        assert_eq!(
            hits.hit(buf.area.width / 2, tab_bar),
            Some(&HitTarget::SettingsRow { index: 0 })
        );
        assert_eq!(
            hits.hit(buf.area.width / 2, files),
            Some(&HitTarget::SettingsRow { index: 1 })
        );
        let column_of = |row: u16, needle: &str| {
            let line: String = (0..buf.area.width)
                .map(|x| buf[(x, row)].symbol())
                .collect();
            line.find(needle)
                .map(|byte| line[..byte].chars().count())
                .unwrap_or_else(|| panic!("{needle} is not on row {row}"))
        };
        let label_x = column_of(tab_bar, copy::VIEW_TAB_BAR);
        assert_eq!(
            column_of(tab_note, copy::VIEW_TAB_BAR_NOTE),
            label_x,
            "the note aligns under the label, not the check"
        );
        // Muted: the note's ink differs from its row's, the same hint
        // color the sound section's explanation and the footer wear.
        let paint = Paint::for_test();
        let note_fg = Some(buf[(label_x as u16, tab_note)].fg);
        assert_eq!(
            note_fg,
            theme::menu_hint(&paint).fg,
            "the note is in the hint color"
        );
        assert_ne!(
            note_fg,
            theme::menu_row(&paint).fg,
            "and not in the row color"
        );

        assert!(
            flat.contains(copy::VIEW_HINT),
            "the footer says rows flip at once: {flat}"
        );
        assert!(
            flat.contains(&format!("Esc {}", copy::BUTTON_CLOSE)) && !flat.contains("Apply"),
            "nothing to apply; Esc leaves: {flat}"
        );
        assert!(
            !hits
                .regions()
                .iter()
                .any(|region| region.target == HitTarget::DialogConfirm),
            "no Apply button to click"
        );
    }

    /// The sound section: the two rows of the switch, the saved one
    /// checked and the cursor's raised, the theme list gone, and the
    /// muted line under them saying what the switch is for.
    #[test]
    fn sound_section_lists_the_switch_with_its_explanation() {
        let dialog = Dialog::Settings {
            section: SettingsSection::Sound,
            themes: ThemePicker {
                names: vec!["dark".into()],
                selected: 0,
                active: Some(0),
                notice: None,
            },
            view: ViewSwitches::new(true, true),
            sound: SoundPicker::new(
                false,
                vec!["bow-ripple".into(), crate::sound::SYSTEM.into()],
                crate::sound::SYSTEM,
            ),
        };
        let (buf, hits) = draw_dialog(&dialog, (0, 0));
        let flat = flatten(&buf);
        assert!(
            flat.contains(&format!("{} {}", copy::MENU_CHECK, copy::SOUND_NOTIFS_OFF)),
            "the checked row wears the mark: {flat}"
        );
        assert!(flat.contains(copy::SOUND_NOTIFS_ON), "{flat}");
        // The explanation opens the section, a blank line above the
        // switch, and neither it nor the heading is a row: no target, and
        // the cursor's raised ground never lands on them.
        let intro = row_of(&buf, copy::SOUND_INTRO);
        let on_row = row_of(&buf, copy::SOUND_NOTIFS_ON);
        assert_eq!(on_row, intro + 2, "the explanation is first: {flat}");
        assert!(hits.hit(buf.area.width / 2, intro).is_none());
        // The sounds: under their heading, the shipped cue by name, the
        // bell by its label, the saved one checked, a blank line between
        // the groups, and each a click target by its picker index.
        assert!(flat.contains("  bow-ripple"), "the installed sound: {flat}");
        assert!(
            flat.contains(&format!("{} {}", copy::MENU_CHECK, copy::SOUND_SYSTEM)),
            "the checked cue wears the mark: {flat}"
        );
        let off_row = row_of(&buf, copy::SOUND_NOTIFS_OFF);
        let heading = row_of(&buf, copy::SOUND_LIST_HEADING);
        let ripple = row_of(&buf, "bow-ripple");
        assert_eq!(heading, off_row + 2, "a blank line, then the heading");
        assert_eq!(ripple, heading + 1, "the sounds sit under it");
        assert!(hits.hit(buf.area.width / 2, heading).is_none());
        let gap: String = (0..buf.area.width)
            .map(|x| buf[(x, off_row + 1)].symbol())
            .collect();
        assert!(
            gap.trim_matches(|c| c == ' ' || c == '│' || c == '║')
                .is_empty(),
            "{gap:?}"
        );
        assert_eq!(
            hits.hit(buf.area.width / 2, ripple),
            Some(&HitTarget::SettingsRow { index: 2 })
        );
        assert_eq!(
            hits.hit(buf.area.width / 2, ripple + 1),
            Some(&HitTarget::SettingsRow { index: 3 })
        );
        assert!(
            !flat.contains("dark"),
            "the theme list is not showing: {flat}"
        );
        let on = row_of(&buf, copy::SOUND_NOTIFS_ON);
        let off = row_of(&buf, copy::SOUND_NOTIFS_OFF);
        let x = (0..buf.area.width)
            .find(|col| buf[(*col, on)].symbol() == "S")
            .expect("row text");
        assert_ne!(
            buf[(x, on)].bg,
            buf[(x, off)].bg,
            "the row the arrows are on is raised"
        );
    }

    #[test]
    fn empty_themes_dialog_says_where_themes_come_from() {
        let backend = TestBackend::new(60, 16);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let dialog = theme_card(Vec::new(), 0, None, None);
        term.draw(|f| {
            paint_dialog(
                &dialog,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
                (0, 0),
            );
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
            paint_dialog(
                &dialog,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
                (0, 0),
            );
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
                (0, 0),
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
                (0, 0),
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("Pick") && flat.contains("another name, e.g. lead."),
            "the useful end of the refusal should remain visible: {flat}"
        );
        assert!(
            hits.regions()
                .iter()
                .any(|region| region.target == HitTarget::DialogConfirm),
            "wrapping must leave the save button reachable"
        );
    }

    /// The keybinds card: its own title, the hint, then the chord and
    /// the action of every active binding behind a viewport, scrolled to
    /// the row asked for and no further, the count of what is showing at
    /// the right, and one close control that answers the mouse. No row
    /// is a target: there is nothing on one to choose.
    #[test]
    fn the_keybinds_card_scrolls_to_the_last_row_and_closes() {
        let area = Rect::new(0, 0, 60, 24);
        let dialog = keybinds_card(20, u16::MAX);
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|frame| {
            paint_dialog(
                &dialog,
                frame.area(),
                frame.buffer_mut(),
                &Paint::for_test(),
                &mut hits,
                None,
                (0, 0),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let flat = flatten(buf);
        assert!(
            flat.contains(copy::KEYBINDS_TITLE),
            "the card's title: {flat}"
        );
        assert!(
            !flat.contains(copy::SETTINGS_TITLE) && !flat.contains(copy::SETTINGS_SECTION_THEME),
            "not the settings card: {flat}"
        );
        assert!(flat.contains(copy::KEYBINDS_HINT), "how to move: {flat}");
        assert!(
            flat.contains("Action 19"),
            "the last row is reachable: {flat}"
        );
        assert!(
            !flat.contains("Action 0 "),
            "scrolling moved the first row away: {flat}"
        );
        assert!(flat.contains("Ctrl+19"), "the chord beside it: {flat}");
        let max = keybind_max_scroll(20, area);
        assert!(max > 0 && max < 20, "some rows are below the fold: {max}");
        assert!(
            flat.contains(&format!("{}–20 / 20", usize::from(max) + 1)),
            "the rows showing are counted: {flat}"
        );
        assert!(
            !flat.contains("Apply") && flat.contains("Esc Close"),
            "nothing to apply, one control closes: {flat}"
        );
        let close = hits
            .regions()
            .iter()
            .find(|region| region.target == HitTarget::DialogCancel)
            .map(|region| region.rect)
            .expect("the section registers its close control");
        assert_eq!(hits.hit(close.x, close.y), Some(&HitTarget::DialogCancel));
        assert!(
            !hits
                .regions()
                .iter()
                .any(|region| matches!(region.target, HitTarget::SettingsRow { .. })),
            "no row is a target"
        );
        let action = row_of(buf, "Action 19");
        assert_ne!(
            buf[(4, action)].bg,
            RtColor::Reset,
            "modal owns a themed ground"
        );
        assert_eq!(
            buf[(4, action)].bg,
            buf[(4, action - 1)].bg,
            "no row is raised"
        );
    }

    /// The keybinds card caps its height rather than growing: a hundred
    /// bindings size the list like thirteen, and the rest scroll. A few
    /// bindings make a shorter card, and a terminal too short for the
    /// cap gets what fits.
    #[test]
    fn the_keybinds_card_stays_a_card_and_scrolls_the_rest() {
        let area = Rect::new(0, 0, 80, 50);
        let rect = |dialog: &Dialog| dialog_rect(dialog, area).expect("fits");
        let hundred = keybinds_card(100, 0);
        let thirteen = keybinds_card(13, 0);
        assert_eq!(
            rect(&hundred).height,
            rect(&thirteen).height,
            "the list is capped; the card is not a wall"
        );
        assert_eq!(rect(&hundred).height, KEYBINDS_MAX_HEIGHT);
        assert!(
            rect(&hundred).height < area.height / 2,
            "a card, in a tall terminal: {:?}",
            rect(&hundred)
        );
        assert_eq!(
            keybind_max_scroll(100, area),
            100 - 13,
            "everything past the cap is reachable by scrolling"
        );
        assert_eq!(keybind_max_scroll(13, area), 0);
        assert!(
            rect(&keybinds_card(2, 0)).height < rect(&thirteen).height,
            "a short sheet is a shorter card"
        );
        let short = Rect::new(0, 0, 80, 12);
        assert!(
            dialog_rect(&hundred, short).expect("fits").height <= short.height - 2,
            "a short terminal still hosts it"
        );
        assert!(keybind_max_scroll(100, short) > keybind_max_scroll(100, area));
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
            paint_dialog(
                &dialog,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
                (0, 0),
            );
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
            paint_dialog(
                &dialog,
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
                (0, 0),
            );
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
            ("keybinds", Some(keybinds_card(1, 0)), MenuState::None),
            (
                "themes",
                Some(theme_card(vec!["dark".into()], 0, Some(0), None)),
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
                    paint_dialog(
                        dialog,
                        f.area(),
                        f.buffer_mut(),
                        &theme,
                        &mut hits,
                        None,
                        (0, 0),
                    );
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
                &theme_card(vec!["dark".into(), "light".into()], 0, Some(0), None),
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                None,
                (0, 0),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let (corner_x, corner_y) = box_corner(buf);
        let content_x = corner_x + 1 + DIALOG_INSET;
        assert_eq!(
            buf[(corner_x + 3, corner_y)].symbol(),
            "S",
            "the settings title rides the border"
        );
        let chip = hits
            .regions()
            .iter()
            .find(|region| {
                region.target
                    == HitTarget::SettingsSection {
                        section: SettingsSection::Theme,
                    }
            })
            .expect("the theme chip is clickable");
        assert_eq!(
            chip.rect.x, content_x,
            "the section chips start at the inset, like the action row"
        );
        assert_eq!(
            buf[(content_x, row_of(buf, "Pick with"))].symbol(),
            "P",
            "the theme hint starts at the inset"
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
                (0, 0),
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
                    (0, 0),
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
                (0, 0),
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
                    (0, 0),
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
        let (card, field_h) = plain_dialog_geometry(
            copy::NEW_TAB_TITLE,
            Some(copy::NEW_TAB_HINT),
            None,
            copy::BUTTON_CREATE,
            None,
            area,
        )
        .expect("a full terminal hosts the card");
        assert_eq!(field_h, 1, "a single-line dialog keeps its one-row field");
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
                None,
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
                None,
                Rect::new(0, 0, 80, DIALOG_INNER_ROWS + 1)
            )
            .is_none(),
            "a terminal shorter than the card refuses the box"
        );

        // A long error grows the card downward, never past the terminal.
        let error = "x ".repeat(200);
        let (card, _) = plain_dialog_geometry(
            copy::NAME_PANE_TITLE,
            Some(copy::NAME_PANE_HINT),
            Some(error.as_str()),
            copy::BUTTON_SAVE,
            None,
            Rect::new(0, 0, 80, 12),
        )
        .expect("the card still fits");
        assert_eq!(card.height, 12, "the error stops at the terminal edge");
    }
}
