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
    /// The recipient is part of the text rather than a separate field so
    /// the whole thing can be typed without a Tab, and so a prefilled
    /// `@name ` can be overtyped when it is the wrong name.
    ///
    /// The buffer may hold newlines ([`Dialog::is_multiline`]). Enter still
    /// sends, because that is what the button says and what the muscle
    /// expects from a one-line composer; a paragraph is reached through
    /// [`newline_chord`] or by pasting one.
    Compose {
        buffer: String,
        /// What the send said, once it has said anything. The dialog stays
        /// open across the send: it runs off this thread and can take
        /// seconds, so it reports here instead of the workspace freezing.
        /// None while composing, Some once it has been answered.
        status: Option<String>,
        /// The request identity and the only valid transitions around it.
        send: ComposeSendState,
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
        match self {
            Dialog::NewTab { .. }
            | Dialog::NamePane { .. }
            | Dialog::RenameTab { .. }
            | Dialog::RenameWorkspace { .. } => true,
            Dialog::Compose { send, .. } => !send.is_confirming_abandon(),
            _ => false,
        }
    }

    /// Whether this dialog's buffer may hold more than one line.
    ///
    /// Only the composer. The others name a tab, a pane or a workspace, and
    /// tmux takes those as a single line: a newline in one is not a longer
    /// name, it is a name with a control character in it.
    pub fn is_multiline(&self) -> bool {
        matches!(
            self,
            Dialog::Compose { send, .. } if !send.is_confirming_abandon()
        )
    }
}

/// Whether this key asks a multi-line field for a line break rather than a
/// send.
///
/// Three chords, because no one of them survives every terminal. Alt+Enter
/// arrives as ESC CR nearly everywhere. Shift+Enter is only distinguishable
/// under the kitty keyboard protocol, and folds back into a plain Enter
/// (a send) where it is not, which is the safe direction. Ctrl+J is the
/// oldest of the three and the one that works over a bare tty; terminals
/// that report its 0x0A as a plain Enter also fold back into a send.
fn newline_chord(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT),
        KeyCode::Char('j') | KeyCode::Char('J') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKeyAction {
    Confirm,
    Cancel,
    Backspace,
    Append(char),
    /// Break the line in a multi-line field. Never a confirm: the only
    /// dialogs that produce this are the ones Enter alone still sends.
    Newline,
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
        // Ahead of both Enter and Char, or Ctrl+J would append a 'j' and
        // Alt+Enter would send.
        _ if dialog.is_multiline() && newline_chord(key) => DialogKeyAction::Newline,
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
        Dialog::Compose { buffer, send, .. } if !send.is_confirming_abandon() => Some(buffer),
        _ => None,
    }
}

/// Add pasted text to an input dialog.
///
/// Line breaks survive into a multi-line field and are dropped everywhere
/// else: a tab or session name is one line to tmux, so a newline in one is
/// not a longer name but a name with a control character in it. Every other
/// control character is dropped in both cases. CRLF and a bare CR both
/// normalise to `\n`, so a paste from a Windows editor does not arrive as a
/// blank line between every line.
pub fn append_dialog_text(dialog: Option<&mut Dialog>, text: &str) -> bool {
    let Some(dialog) = dialog else {
        return false;
    };
    if let Dialog::NamePane { error, .. } = dialog {
        *error = None;
    }
    let multiline = dialog.is_multiline();
    let Some(buffer) = dialog_buffer_mut(dialog) else {
        return false;
    };
    let before = buffer.len();
    if multiline {
        let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
        buffer.extend(
            normalised
                .chars()
                .filter(|ch| *ch == '\n' || !ch.is_control()),
        );
    } else {
        buffer.extend(text.chars().filter(|ch| !ch.is_control()));
    }
    buffer.len() != before
}

/// What a composer line says, once it says something addressable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// One semantic message and the idempotency key bound to its send attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeAttempt {
    pub message: Composed,
    pub client_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeResume {
    Sending,
    Retryable,
}

/// Lifecycle of the process-local idempotency key owned by one composer.
/// Explicit abandon may drop it; process restart remains unrecoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeSendState {
    Ready,
    Sending(ComposeAttempt),
    Retryable(ComposeAttempt),
    ConfirmAbandon {
        attempt: ComposeAttempt,
        resume: ComposeResume,
    },
}

impl ComposeSendState {
    pub fn attempt(&self) -> Option<&ComposeAttempt> {
        match self {
            ComposeSendState::Ready => None,
            ComposeSendState::Sending(attempt)
            | ComposeSendState::Retryable(attempt)
            | ComposeSendState::ConfirmAbandon { attempt, .. } => Some(attempt),
        }
    }

    pub fn is_sending(&self) -> bool {
        matches!(self, ComposeSendState::Sending(_))
    }

    pub fn is_confirming_abandon(&self) -> bool {
        matches!(self, ComposeSendState::ConfirmAbandon { .. })
    }
}

/// Whether Esc may close the dialog without losing a retry key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeCancel {
    Close,
    KeepOpen,
}

pub fn request_compose_cancel(dialog: &mut Dialog) -> ComposeCancel {
    let Dialog::Compose { send, .. } = dialog else {
        return ComposeCancel::Close;
    };
    let current = std::mem::replace(send, ComposeSendState::Ready);
    *send = match current {
        ComposeSendState::Ready => return ComposeCancel::Close,
        ComposeSendState::Sending(attempt) => ComposeSendState::ConfirmAbandon {
            attempt,
            resume: ComposeResume::Sending,
        },
        ComposeSendState::Retryable(attempt) => ComposeSendState::ConfirmAbandon {
            attempt,
            resume: ComposeResume::Retryable,
        },
        ComposeSendState::ConfirmAbandon { attempt, resume } => match resume {
            ComposeResume::Sending => ComposeSendState::Sending(attempt),
            ComposeResume::Retryable => ComposeSendState::Retryable(attempt),
        },
    };
    ComposeCancel::KeepOpen
}

/// Start a send, reusing an uncertain attempt only when its message is exact.
pub fn begin_compose_send(
    dialog: Option<&mut Dialog>,
    message: Composed,
    new_client_key: impl FnOnce() -> String,
) -> Option<ComposeAttempt> {
    let Some(Dialog::Compose { send, .. }) = dialog else {
        return None;
    };

    let selected = match send {
        ComposeSendState::Ready => ComposeAttempt {
            message,
            client_key: new_client_key(),
        },
        ComposeSendState::Retryable(existing) if existing.message == message => existing.clone(),
        ComposeSendState::Retryable(_) => ComposeAttempt {
            message,
            client_key: new_client_key(),
        },
        ComposeSendState::Sending(_) | ComposeSendState::ConfirmAbandon { .. } => return None,
    };
    *send = ComposeSendState::Sending(selected.clone());
    Some(selected)
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

    fn composer(buffer: &str) -> Dialog {
        Dialog::Compose {
            buffer: buffer.into(),
            status: None,
            send: ComposeSendState::Ready,
        }
    }

    /// Enter sends and the newline chords break the line. Which one of the
    /// three a terminal reports is not something the composer can choose,
    /// so all three have to mean the same thing.
    #[test]
    fn the_composer_breaks_its_line_without_giving_up_enter() {
        let dialog = composer("@reviewer ship it");
        let key = |code, mods| KeyEvent::new(code, mods);

        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Enter, KeyModifiers::empty())),
            DialogKeyAction::Confirm,
            "a plain Enter still sends"
        );
        for mods in [KeyModifiers::ALT, KeyModifiers::SHIFT] {
            assert_eq!(
                dialog_key_action(&dialog, &key(KeyCode::Enter, mods)),
                DialogKeyAction::Newline,
                "{mods:?}+Enter should break the line"
            );
        }
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            DialogKeyAction::Newline,
            "Ctrl+J is the chord that survives a bare tty"
        );
    }

    #[test]
    fn abandon_confirmation_keeps_the_draft_read_only() {
        let mut dialog = composer("@reviewer ship it");
        let message = parse_compose("@reviewer ship it").expect("message");
        begin_compose_send(Some(&mut dialog), message, || "stable-key".into()).expect("attempt");
        assert_eq!(request_compose_cancel(&mut dialog), ComposeCancel::KeepOpen);

        assert!(!dialog.has_input());
        assert!(!append_dialog_text(Some(&mut dialog), " changed"));
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty())
            ),
            DialogKeyAction::Ignore
        );
        let Dialog::Compose { buffer, send, .. } = dialog else {
            unreachable!()
        };
        assert_eq!(buffer, "@reviewer ship it");
        assert_eq!(
            send.attempt().map(|attempt| attempt.client_key.as_str()),
            Some("stable-key")
        );
    }

    /// A name is one line to tmux. The chords that break a composer line
    /// must do nothing in the dialogs that name something.
    #[test]
    fn a_name_field_has_no_newline_chord() {
        let dialog = Dialog::NewTab {
            buffer: "review".into(),
        };
        assert_eq!(
            dialog_key_action(&dialog, &KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            DialogKeyAction::Confirm,
        );
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            DialogKeyAction::Ignore,
        );
    }

    /// A pasted paragraph reaches the recipient as a paragraph. Only the
    /// line breaks survive: a paste carrying a tab or an escape is still
    /// text the field has no way to show.
    #[test]
    fn a_composer_paste_keeps_its_paragraph() {
        let mut dialog = composer("@reviewer ");
        assert!(append_dialog_text(
            Some(&mut dialog),
            "first line\r\nsecond line\rthird\tline\u{7}"
        ));
        let Dialog::Compose { buffer, .. } = &dialog else {
            unreachable!()
        };
        assert_eq!(buffer, "@reviewer first line\nsecond line\nthirdline");

        let parsed = parse_compose(buffer).expect("addressed");
        assert_eq!(
            parsed.subject, "first line",
            "the subject is the first line"
        );
        assert_eq!(parsed.body, "first line\nsecond line\nthirdline");
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
