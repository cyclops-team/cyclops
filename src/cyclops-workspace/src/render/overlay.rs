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
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::bindings::BindingAction;
use crate::copy;
use crate::dialog::Dialog;
use crate::input::mouse::{HitMap, HitTarget, MenuState};
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

    super::overlay_text(
        buf,
        inner,
        inner.x + 1,
        inner.y,
        title,
        theme::menu_row(paint),
    );
    if let Some(hint) = hint {
        super::overlay_text(
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
            super::overlay_text(
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
        super::overlay_text(buf, inner, bx, button_y, &text, style);
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
        super::overlay_text(buf, inner, inner.x, y, &format!(" {label}"), style);
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
}
