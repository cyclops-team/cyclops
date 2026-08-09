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
    /// Address a message from inside the workspace: `@reviewer ship it`.
    ///
    /// One line, because the point is to be faster than leaving for a
    /// shell. The recipient is part of the text rather than a separate
    /// field so the whole thing can be typed without a Tab, and so a
    /// prefilled `@name ` can be overtyped when it is the wrong name.
    Compose {
        buffer: String,
        /// What the send said, once it has said anything. The dialog stays
        /// open across the send: it runs off this thread and can take
        /// seconds, so it reports here instead of the workspace freezing.
        /// None while composing, Some once it has been answered.
        status: Option<String>,
        /// A send is in flight. Enter is ignored while this is set, so a
        /// second press cannot put the same message on the record twice.
        sending: bool,
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
                | Dialog::Compose { .. }
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
        | Dialog::RenameWorkspace { buffer, .. }
        | Dialog::Compose { buffer, .. } => Some(buffer),
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

/// What a composer line says, once it says something addressable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Read `@reviewer ship the rate limiter fix` into a recipient and a
/// message, or say what is missing.
///
/// The whole grammar is: an `@name`, then the rest. Free text after the
/// name is taken literally and never re-split, because a message is prose
/// and the moment this starts interpreting `&&` or `#` it becomes a shell
/// that only looks like one. That failure has a history here: an earlier
/// design put this in the user's shell as a function, where `fix issue #42`
/// silently became `fix issue` and `run make && test` ran `test`.
///
/// The subject is the first line's worth of the message, because the record
/// and every list are keyed on subjects and a blank one reads as a message
/// with nothing in it. Long messages keep the whole text as the body, so
/// nothing typed is lost to the summary.
pub fn parse_compose(input: &str) -> Result<Composed, &'static str> {
    let text = input.trim();
    let Some(rest) = text.strip_prefix('@') else {
        return Err("start with @name, as in @reviewer take a look");
    };
    let mut parts = rest.splitn(2, char::is_whitespace);
    let to = parts.next().unwrap_or_default().trim();
    if to.is_empty() {
        return Err("who is it for? @name, as in @reviewer take a look");
    }
    let message = parts.next().unwrap_or_default().trim();
    if message.is_empty() {
        return Err("nothing to send yet");
    }
    // A subject is a handle, not the message. Cut on the first line break
    // so a pasted paragraph does not become a subject nobody can read in a
    // list, and cap the rest at a width a roster row can hold.
    const SUBJECT_MAX: usize = 72;
    let first_line = message.lines().next().unwrap_or(message).trim();
    let subject: String = match first_line.char_indices().nth(SUBJECT_MAX) {
        None => first_line.to_string(),
        Some((cut, _)) => format!("{}…", first_line[..cut].trim_end()),
    };
    Ok(Composed {
        to: to.to_string(),
        subject,
        body: message.to_string(),
    })
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

    /// The composer takes free text and must never interpret it.
    ///
    /// These are the exact strings that broke the shell-function design
    /// this replaced: a `#` started a comment, `&&` ran the tail as a
    /// second command, and `*` globbed against the working directory.
    /// Everything after the name has to arrive at the recipient byte for
    /// byte.
    #[test]
    fn a_message_is_prose_and_is_never_re_split() {
        for text in [
            "@reviewer fix issue #42",
            "@reviewer run make && test",
            "@reviewer why is x*y broken?",
            "@reviewer use `cyclops send` for this",
            "@reviewer $HOME is not expanded; neither is $(date)",
            "@reviewer \"quoted\" and 'quoted' both survive",
        ] {
            let got = parse_compose(text).expect(text);
            assert_eq!(got.to, "reviewer");
            let want = text.trim_start_matches("@reviewer ");
            assert_eq!(got.body, want, "body was altered for {text:?}");
        }
    }

    #[test]
    fn a_composer_line_says_what_is_missing() {
        assert!(parse_compose("").is_err());
        assert!(parse_compose("   ").is_err());
        // No name at all.
        assert!(parse_compose("just some words").is_err());
        // A name and nothing to say.
        assert!(parse_compose("@reviewer").is_err());
        assert!(parse_compose("@reviewer   ").is_err());
        // An @ with no name behind it.
        assert!(parse_compose("@ reviewer hello").is_err());
        // Leading and trailing space around the whole line is not input.
        let got = parse_compose("  @reviewer  ship it  ").expect("addressed");
        assert_eq!(got.to, "reviewer");
        assert_eq!(got.body, "ship it");
    }

    /// The subject is a handle for a list row; the body keeps everything.
    #[test]
    fn a_long_message_keeps_its_body_and_shortens_only_the_subject() {
        let long = "x".repeat(200);
        let got = parse_compose(&format!("@reviewer {long}")).expect("addressed");
        assert_eq!(got.body, long, "the body must keep every character");
        assert!(got.subject.chars().count() <= 73, "{}", got.subject);
        assert!(got.subject.ends_with('…'));

        // A pasted paragraph gets a first-line subject, not a wall of text.
        let got = parse_compose("@reviewer first line\nsecond line").expect("addressed");
        assert_eq!(got.subject, "first line");
        assert_eq!(got.body, "first line\nsecond line");
    }

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
