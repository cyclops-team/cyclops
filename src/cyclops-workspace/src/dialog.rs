//! Modal dialogs in the workspace UI: what a dialog is, and how a key or a
//! paste edits its own text-input or scroll state.
//!
//! Every dialog carries its own target (a pane, window or session id), so
//! a dialog opened from a right-click on a background tab acts on that
//! tab, not on whatever is active by the time the user confirms. Deciding
//! what a *resolved* [`DialogKeyAction`] does to the rest of the app
//! (dispatching an [`crate::action::Action`], closing the dialog, cancelling
//! a drag) is `app`'s job, not this module's: everything here is a pure
//! function of a `Dialog` plus, at most, the key or text that just arrived.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Active modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Confirm closing a pane that may host a live agent.
    ConfirmClosePane { pane_id: String },
    /// Name a new tab before it is created; blank uses the next number.
    NewTab { buffer: String },
    /// Assign the pane's Cyclops identity and message address.
    NamePane {
        pane_id: String,
        buffer: String,
        error: Option<String>,
    },
    /// Rename one tab; buffer holds the edited name.
    RenameTab { window_id: String, buffer: String },
    /// Confirm closing a whole tab (kills every pane in it).
    ConfirmCloseTab { window_id: String },
    /// Rename one workspace (tmux session).
    RenameWorkspace { session: String, buffer: String },
    /// Confirm closing a workspace that may host agents.
    ConfirmCloseWorkspace { session: String },
    /// Read-only, scrollable reference generated from the active bindings.
    Keybinds {
        scroll: u16,
        rows: Vec<crate::bindings::BindingHelp>,
    },
    /// Pick a theme; Enter applies it exactly like `cyclops theme <name>`.
    Themes {
        /// Loadable theme names, the same rows `cyclops theme` lists.
        names: Vec<String>,
        /// The row the arrow keys are on.
        selected: usize,
        /// The row of the active theme, when it is one of the rows at all
        /// (a path or CYCLOPS_THEME selection is not).
        active: Option<usize>,
        /// What an apply that could not go live has to say (daemon down,
        /// or painting something else). Same slot NamePane's error uses.
        notice: Option<String>,
    },
}

impl Dialog {
    pub fn confirm_close(pane_id: impl Into<String>) -> Self {
        Dialog::ConfirmClosePane {
            pane_id: pane_id.into(),
        }
    }

    /// Whether the dialog takes typed input (vs a yes/no confirm).
    pub fn has_input(&self) -> bool {
        matches!(
            self,
            Dialog::NewTab { .. }
                | Dialog::NamePane { .. }
                | Dialog::RenameTab { .. }
                | Dialog::RenameWorkspace { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKeyAction {
    Confirm,
    Cancel,
    Backspace,
    Append(char),
    Scroll(i16),
    ScrollStart,
    ScrollEnd,
    Ignore,
}

/// Resolve dialog keys without mutating application state. Every modal
/// confirms on Enter and cancels on Escape, so one key means the same thing
/// in every dialog. The read-only keybinds sheet has nothing to confirm, so
/// Enter dismisses it.
pub fn dialog_key_action(dialog: &Dialog, key: &KeyEvent) -> DialogKeyAction {
    if matches!(dialog, Dialog::Keybinds { .. }) {
        return match key.code {
            KeyCode::Esc | KeyCode::Enter => DialogKeyAction::Cancel,
            KeyCode::Up => DialogKeyAction::Scroll(-1),
            KeyCode::Down => DialogKeyAction::Scroll(1),
            KeyCode::PageUp => DialogKeyAction::Scroll(-8),
            KeyCode::PageDown => DialogKeyAction::Scroll(8),
            KeyCode::Home => DialogKeyAction::ScrollStart,
            KeyCode::End => DialogKeyAction::ScrollEnd,
            _ => DialogKeyAction::Ignore,
        };
    }
    // The picker scrolls a selection, not a viewport, and unlike the
    // keybinds sheet its Enter has something to confirm.
    if matches!(dialog, Dialog::Themes { .. }) {
        return match key.code {
            KeyCode::Esc => DialogKeyAction::Cancel,
            KeyCode::Enter => DialogKeyAction::Confirm,
            KeyCode::Up => DialogKeyAction::Scroll(-1),
            KeyCode::Down => DialogKeyAction::Scroll(1),
            _ => DialogKeyAction::Ignore,
        };
    }
    let text_key = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
    match key.code {
        KeyCode::Esc => DialogKeyAction::Cancel,
        KeyCode::Enter => DialogKeyAction::Confirm,
        KeyCode::Backspace if dialog.has_input() => DialogKeyAction::Backspace,
        KeyCode::Char(c) if dialog.has_input() && text_key => DialogKeyAction::Append(c),
        _ => DialogKeyAction::Ignore,
    }
}

/// The editable buffer of an input dialog, if this dialog has one.
pub fn dialog_buffer_mut(dialog: &mut Dialog) -> Option<&mut String> {
    match dialog {
        Dialog::NewTab { buffer }
        | Dialog::NamePane { buffer, .. }
        | Dialog::RenameTab { buffer, .. }
        | Dialog::RenameWorkspace { buffer, .. } => Some(buffer),
        _ => None,
    }
}

/// Add printable pasted text to an input dialog. Line controls belong to a
/// pane paste, never to a tmux tab or session name.
pub fn append_dialog_text(dialog: Option<&mut Dialog>, text: &str) -> bool {
    let Some(dialog) = dialog else {
        return false;
    };
    if let Dialog::NamePane { error, .. } = dialog {
        *error = None;
    }
    let Some(buffer) = dialog_buffer_mut(dialog) else {
        return false;
    };
    let before = buffer.len();
    buffer.extend(text.chars().filter(|ch| !ch.is_control()));
    buffer.len() != before
}

/// Resolve a [`DialogKeyAction::Scroll`] delta against the keybinds sheet's
/// bound: moves immediately, and never past either end.
pub fn move_keybind_scroll(current: u16, delta: i16, max: u16) -> u16 {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs()).min(max)
    } else {
        current.saturating_add(delta as u16).min(max)
    }
}

/// Resolve a [`DialogKeyAction::Scroll`] delta against the theme picker's
/// rows: clamped to the ends, same rule as [`move_keybind_scroll`].
pub fn move_theme_selection(current: usize, delta: i16, len: usize) -> usize {
    let Some(last) = len.checked_sub(1) else {
        return 0;
    };
    if delta.is_negative() {
        current
            .saturating_sub(delta.unsigned_abs() as usize)
            .min(last)
    } else {
        current.saturating_add(delta as usize).min(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybind_scroll_moves_immediately_after_end_and_never_overshoots() {
        assert_eq!(move_keybind_scroll(4, 20, 10), 10);
        assert_eq!(move_keybind_scroll(10, -1, 10), 9);
        assert_eq!(move_keybind_scroll(0, -8, 10), 0);
    }

    #[test]
    fn enter_confirms_a_destructive_dialog() {
        let dialog = Dialog::ConfirmCloseTab {
            window_id: "@1".into(),
        };
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(dialog_key_action(&dialog, &enter), DialogKeyAction::Confirm);
    }

    #[test]
    fn enter_submits_an_input_dialog() {
        let dialog = Dialog::NewTab {
            buffer: "review".into(),
        };
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(dialog_key_action(&dialog, &enter), DialogKeyAction::Confirm);
    }

    #[test]
    fn modified_characters_do_not_leak_into_dialog_text() {
        let dialog = Dialog::NewTab {
            buffer: String::new(),
        };
        let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            dialog_key_action(&dialog, &control_c),
            DialogKeyAction::Ignore
        );
    }

    #[test]
    fn theme_picker_keys_move_confirm_and_cancel() {
        let dialog = Dialog::Themes {
            names: vec!["dark".into(), "light".into()],
            selected: 0,
            active: Some(0),
            notice: None,
        };
        let key = |code| KeyEvent::new(code, KeyModifiers::empty());
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Down)),
            DialogKeyAction::Scroll(1)
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Up)),
            DialogKeyAction::Scroll(-1)
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Enter)),
            DialogKeyAction::Confirm
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Esc)),
            DialogKeyAction::Cancel
        );
        // No text input: a typed character must not become an append.
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Char('d'))),
            DialogKeyAction::Ignore
        );
    }

    #[test]
    fn theme_selection_clamps_at_both_ends() {
        assert_eq!(move_theme_selection(0, -1, 3), 0);
        assert_eq!(move_theme_selection(0, 1, 3), 1);
        assert_eq!(move_theme_selection(2, 1, 3), 2);
        assert_eq!(move_theme_selection(0, 1, 0), 0);
    }

    #[test]
    fn dialog_paste_keeps_text_and_drops_line_controls() {
        let mut dialog = Dialog::NewTab {
            buffer: "review".into(),
        };
        assert!(append_dialog_text(Some(&mut dialog), "-api\n\t"));
        assert_eq!(
            dialog,
            Dialog::NewTab {
                buffer: "review-api".into()
            }
        );
    }
}
