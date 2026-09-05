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

use crate::bindings::BindingHelp;
use crate::copy;

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
    /// Rename one stable workspace identity (tmux session).
    RenameWorkspace { session_id: String, buffer: String },
    /// Confirm closing one stable workspace identity.
    ConfirmCloseWorkspace { session_id: String },
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
    /// The settings card: one section showing at a time, Tab walks them.
    /// Every section keeps its own list state while another is showing,
    /// so a Tab away and back lands where the arrows were.
    Settings {
        section: SettingsSection,
        themes: ThemePicker,
        view: ViewSwitches,
        sound: SoundPicker,
    },
    /// The keybinding reference: every active binding, read-only, behind
    /// a viewport. A card of its own beside the settings card: nothing on
    /// it is chosen, so it has no Apply and shares nothing with the
    /// sections that do. Enter closes it like Esc.
    Keybinds {
        /// The first row showing.
        scroll: u16,
        /// Every active binding, generated from the router's map rather
        /// than written down, so a rebinding in config.toml shows here.
        rows: Vec<BindingHelp>,
    },
}

/// The settings card's sections, in the order Tab walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Theme,
    View,
    Sound,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 3] = [
        SettingsSection::Theme,
        SettingsSection::View,
        SettingsSection::Sound,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Theme => copy::SETTINGS_SECTION_THEME,
            SettingsSection::View => copy::SETTINGS_SECTION_VIEW,
            SettingsSection::Sound => copy::SETTINGS_SECTION_SOUND,
        }
    }

    /// The section `delta` steps along, wrapping at both ends: Tab from
    /// the last section is the first one, so the key never dead-ends.
    pub fn step(self, delta: i16) -> Self {
        let len = Self::ALL.len() as i32;
        let at = Self::ALL.iter().position(|s| *s == self).unwrap_or(0) as i32;
        Self::ALL[(at + i32::from(delta)).rem_euclid(len) as usize]
    }
}

/// Pick a theme; Enter applies it exactly like `cyclops theme <name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemePicker {
    /// Loadable theme names, the same rows `cyclops theme` lists.
    pub names: Vec<String>,
    /// The row the arrow keys are on.
    pub selected: usize,
    /// The row of the active theme, when it is one of the rows at all
    /// (a path or CYCLOPS_THEME selection is not).
    pub active: Option<usize>,
    /// What an apply that could not go live has to say (daemon down,
    /// or painting something else). Same slot NamePane's error uses.
    pub notice: Option<String>,
}

/// The view section: the chrome an operator can put away, one row per
/// surface. Unlike the pickers beside it, nothing here waits for Enter
/// to save: a row flips its surface the moment it is chosen (Enter or a
/// click), the way the app menu's toggles did before they moved here,
/// and the checks read back what the workspace is showing right now
/// (`exec::sync_view_switches` keeps them honest while the card is up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewSwitches {
    /// The row the arrow keys are on.
    pub selected: usize,
    /// Whether the tab strip shows (`WorkspacePrefs::tab_bar_visible`).
    pub tab_bar: bool,
    /// Whether the sidebar's file panel shows (`files_rows > 0`).
    pub files: bool,
}

/// One row of the view section, by the surface it switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewRow {
    TabBar,
    Files,
}

impl ViewSwitches {
    /// The rows, in list order.
    pub const ROWS: [ViewRow; 2] = [ViewRow::TabBar, ViewRow::Files];

    /// Opens on the first row, reading the surfaces as they are.
    pub fn new(tab_bar: bool, files: bool) -> Self {
        ViewSwitches {
            selected: 0,
            tab_bar,
            files,
        }
    }

    /// How many rows the arrows can land on.
    pub fn len(&self) -> usize {
        Self::ROWS.len()
    }

    pub fn row(&self, index: usize) -> Option<ViewRow> {
        Self::ROWS.get(index).copied()
    }

    /// What Enter would flip: the row the cursor is on.
    pub fn selected_row(&self) -> Option<ViewRow> {
        self.row(self.selected)
    }

    /// Whether row `index` wears a check: its surface is showing.
    pub fn is_checked(&self, index: usize) -> bool {
        match self.row(index) {
            Some(ViewRow::TabBar) => self.tab_bar,
            Some(ViewRow::Files) => self.files,
            None => false,
        }
    }
}

/// The sound section as one list the arrows walk: the switch's two rows
/// first, then every sound there is to choose from. One cursor and one
/// Enter for both, so it reads and moves like the theme picker beside
/// it; each group keeps its own saved row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundPicker {
    /// The row the arrow keys are on.
    pub selected: usize,
    /// The switch's checked side: the saved one when the card opens,
    /// the last switch row landed on since. Enter saves it.
    pub on: bool,
    /// The sounds on offer, `crate::sound::choices` order: installed
    /// stems, then the system alert.
    pub sounds: Vec<String>,
    /// Index into `sounds` of the checked cue, the same way: saved on
    /// open, then following the cursor. `None` when the saved name is
    /// not on offer (its file went away), so no row wears its check.
    pub active_sound: Option<usize>,
    /// Where the cursor was when a sound last played for it, so a redraw
    /// does not replay and a return to the same row does.
    pub previewed: Option<usize>,
}

/// One row of the sound list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundRow<'a> {
    /// The switch: `true` is the "on" row.
    Switch(bool),
    /// A sound to choose, by the name `[workspace] sound` saves.
    Sound(&'a str),
}

impl SoundPicker {
    /// The switch's rows, ahead of the sounds. On first: it is what the
    /// switch is for.
    pub const SWITCH_ROWS: usize = 2;

    /// Opens on the saved switch, the cursor on its row.
    pub fn new(on: bool, sounds: Vec<String>, chosen: &str) -> Self {
        let active_sound = sounds.iter().position(|name| name == chosen);
        SoundPicker {
            selected: Self::row_for(on),
            on,
            sounds,
            active_sound,
            previewed: None,
        }
    }

    pub fn row_for(on: bool) -> usize {
        if on {
            0
        } else {
            1
        }
    }

    /// How many rows the arrows can land on.
    pub fn len(&self) -> usize {
        Self::SWITCH_ROWS + self.sounds.len()
    }

    pub fn row(&self, index: usize) -> Option<SoundRow<'_>> {
        match index {
            0 => Some(SoundRow::Switch(true)),
            1 => Some(SoundRow::Switch(false)),
            _ => self
                .sounds
                .get(index - Self::SWITCH_ROWS)
                .map(|name| SoundRow::Sound(name)),
        }
    }

    /// What Enter would save: the row the cursor is on.
    pub fn selected_row(&self) -> Option<SoundRow<'_>> {
        self.row(self.selected)
    }

    /// Whether row `index` wears its group's check: the switch's checked
    /// side, or the checked cue.
    pub fn is_checked(&self, index: usize) -> bool {
        match self.row(index) {
            Some(SoundRow::Switch(on)) => on == self.on,
            Some(SoundRow::Sound(_)) => Some(index - Self::SWITCH_ROWS) == self.active_sound,
            None => false,
        }
    }

    /// Move the cursor's group's check to the cursor: the switch reads
    /// as flipped, or the row becomes the cue to be. Nothing is saved
    /// until Enter reads the checks. Whether a check moved.
    pub fn check_selected(&mut self) -> bool {
        let index = self.selected.checked_sub(Self::SWITCH_ROWS);
        match self.selected_row() {
            Some(SoundRow::Switch(on)) => {
                let changed = self.on != on;
                self.on = on;
                changed
            }
            Some(SoundRow::Sound(_)) => {
                let changed = self.active_sound != index;
                self.active_sound = index;
                changed
            }
            None => false,
        }
    }

    /// The checked cue's name, when one is on offer.
    pub fn checked_sound(&self) -> Option<&str> {
        self.active_sound
            .and_then(|index| self.sounds.get(index))
            .map(String::as_str)
    }

    /// The sound to play because the cursor just arrived on it: a sound
    /// row the cursor was not on at the last call. Leaving for another
    /// row and coming back plays again; a redraw with the cursor still
    /// does not.
    pub fn arrived_on(&mut self) -> Option<&str> {
        if self.previewed == Some(self.selected) {
            return None;
        }
        self.previewed = Some(self.selected);
        match self.selected_row() {
            Some(SoundRow::Sound(name)) => Some(name),
            _ => None,
        }
    }
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
    DeleteWord,
    Clear,
    Append(char),
    /// Break the line in a multi-line field. Never a confirm: the only
    /// dialogs that produce this are the ones Enter alone still sends.
    Newline,
    Scroll(i16),
    ScrollStart,
    ScrollEnd,
    /// Tab (forward) or Shift+Tab (back) across the settings sections.
    SwitchSection(i16),
    Ignore,
}

/// Remove the trailing word from an editable buffer.
pub fn delete_word(buffer: &mut String) {
    while buffer.ends_with(|c: char| c.is_whitespace()) {
        buffer.pop();
    }
    while let Some(c) = buffer.chars().last() {
        if c.is_whitespace() {
            break;
        }
        buffer.pop();
    }
    while buffer.ends_with(|c: char| c.is_whitespace()) {
        buffer.pop();
    }
}

/// Resolve dialog keys without mutating application state. Every modal
/// confirms on Enter and cancels on Escape, so one key means the same thing
/// in every dialog. The read-only keybinds card has nothing to confirm, so
/// its Enter dismisses it (`action::route_dialog_confirm` agrees; the key
/// is read as a cancel here so the close path is one path).
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
    // The settings card scrolls a selection, not a viewport, and unlike
    // the keybinds card its Enter has something to confirm. Tab is the one
    // key it has that no other dialog does: none of them takes a literal
    // tab, so nothing is lost by claiming it here.
    if matches!(dialog, Dialog::Settings { .. }) {
        return match key.code {
            KeyCode::Esc => DialogKeyAction::Cancel,
            KeyCode::Enter | KeyCode::Char(' ') => DialogKeyAction::Confirm,
            KeyCode::Up => DialogKeyAction::Scroll(-1),
            KeyCode::Down => DialogKeyAction::Scroll(1),
            KeyCode::PageUp => DialogKeyAction::Scroll(-8),
            KeyCode::PageDown => DialogKeyAction::Scroll(8),
            KeyCode::Home => DialogKeyAction::ScrollStart,
            KeyCode::End => DialogKeyAction::ScrollEnd,
            KeyCode::Tab => DialogKeyAction::SwitchSection(1),
            KeyCode::BackTab => DialogKeyAction::SwitchSection(-1),
            _ => DialogKeyAction::Ignore,
        };
    }
    if dialog.has_input() {
        // Word delete: Ctrl+Backspace, Alt+Backspace, or Ctrl+W
        if (key.code == KeyCode::Backspace
            && (key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)))
            || (key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return DialogKeyAction::DeleteWord;
        }
        // Clear all / delete all: Ctrl+U, Cmd+A, or Cmd+Backspace
        if (key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Char('a')
                && (key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::SUPER)))
            || (key.code == KeyCode::Backspace && key.modifiers.contains(KeyModifiers::SUPER))
        {
            return DialogKeyAction::Clear;
        }
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

/// Resolve a [`DialogKeyAction::Scroll`] delta against the keybinds
/// card's bound: moves immediately, and never past either end.
pub fn move_keybind_scroll(current: u16, delta: i16, max: u16) -> u16 {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs()).min(max)
    } else {
        current.saturating_add(delta as u16).min(max)
    }
}

/// Resolve a [`DialogKeyAction::Scroll`] delta against a list's rows:
/// clamped to the ends, same rule as [`move_keybind_scroll`].
pub fn move_selection(current: usize, delta: i16, len: usize) -> usize {
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

/// Move the settings card's cursor `delta` rows in whichever section is
/// showing. The one place that knows which list the arrows are on, so
/// the key path and the wheel path cannot disagree about it.
pub fn move_settings_selection(dialog: &mut Dialog, delta: i16) {
    let Dialog::Settings {
        section,
        themes,
        view,
        sound,
    } = dialog
    else {
        return;
    };
    match section {
        SettingsSection::Theme => {
            themes.selected = move_selection(themes.selected, delta, themes.names.len());
        }
        SettingsSection::View => {
            view.selected = move_selection(view.selected, delta, view.len());
        }
        SettingsSection::Sound => {
            sound.selected = move_selection(sound.selected, delta, sound.len());
        }
    }
}

/// Home and End: the showing section's first row, or its last.
pub fn jump_settings_selection(dialog: &mut Dialog, to_end: bool) {
    let Dialog::Settings {
        section,
        themes,
        view,
        sound,
    } = dialog
    else {
        return;
    };
    let last = |len: usize| if to_end { len.saturating_sub(1) } else { 0 };
    match section {
        SettingsSection::Theme => themes.selected = last(themes.names.len()),
        SettingsSection::View => view.selected = last(view.len()),
        SettingsSection::Sound => sound.selected = last(sound.len()),
    }
}

/// How many rows one wheel notch moves in an open dialog: a picker's
/// notch moves the selection one row, the keybinds card's moves its
/// viewport three, the way a wheel over any list of text does.
pub fn dialog_wheel_rows(dialog: &Dialog) -> i16 {
    match dialog {
        Dialog::Keybinds { .. } => 3,
        _ => 1,
    }
}

/// Put the showing section's cursor on row `index` (a click). A row the
/// list does not have leaves the cursor where it was.
pub fn select_settings_row(dialog: &mut Dialog, index: usize) {
    let Dialog::Settings {
        section,
        themes,
        view,
        sound,
        ..
    } = dialog
    else {
        return;
    };
    match section {
        SettingsSection::Theme if index < themes.names.len() => themes.selected = index,
        SettingsSection::View if index < view.len() => view.selected = index,
        SettingsSection::Sound if index < sound.len() => sound.selected = index,
        _ => {}
    }
}

/// Show the section `delta` steps along (Tab, Shift+Tab, a chip click).
pub fn switch_settings_section(dialog: &mut Dialog, delta: i16) {
    if let Dialog::Settings { section, .. } = dialog {
        *section = section.step(delta);
    }
}

/// Show one section outright (a chip click names it).
pub fn show_settings_section(dialog: &mut Dialog, wanted: SettingsSection) {
    if let Dialog::Settings { section, .. } = dialog {
        *section = wanted;
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

    fn settings(section: SettingsSection) -> Dialog {
        Dialog::Settings {
            section,
            themes: ThemePicker {
                names: vec!["dark".into(), "light".into()],
                selected: 0,
                active: Some(0),
                notice: None,
            },
            view: ViewSwitches::new(true, true),
            sound: SoundPicker::new(
                false,
                vec!["bow-ripple".into(), "system".into()],
                "bow-ripple",
            ),
        }
    }

    fn keybinds() -> Dialog {
        Dialog::Keybinds {
            scroll: 0,
            rows: (0..20)
                .map(|index| BindingHelp {
                    keys: format!("Ctrl+B {index}"),
                    action: format!("Action {index}"),
                })
                .collect(),
        }
    }

    #[test]
    fn settings_keys_move_switch_confirm_and_cancel() {
        let dialog = settings(SettingsSection::Theme);
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
            dialog_key_action(&dialog, &key(KeyCode::Char(' '))),
            DialogKeyAction::Confirm
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Esc)),
            DialogKeyAction::Cancel
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::PageDown)),
            DialogKeyAction::Scroll(8)
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Home)),
            DialogKeyAction::ScrollStart
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::End)),
            DialogKeyAction::ScrollEnd
        );
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Tab)),
            DialogKeyAction::SwitchSection(1)
        );
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
            ),
            DialogKeyAction::SwitchSection(-1)
        );
        // No text input: a typed character must not become an append.
        assert_eq!(
            dialog_key_action(&dialog, &key(KeyCode::Char('d'))),
            DialogKeyAction::Ignore
        );
    }

    #[test]
    fn list_selection_clamps_at_both_ends() {
        assert_eq!(move_selection(0, -1, 3), 0);
        assert_eq!(move_selection(0, 1, 3), 1);
        assert_eq!(move_selection(2, 1, 3), 2);
        assert_eq!(move_selection(0, 1, 0), 0);
    }

    /// A click lands the cursor on the row it names, in the showing
    /// section only, and a row past the end is not a jump to nowhere.
    #[test]
    fn a_row_click_moves_the_showing_sections_cursor() {
        let mut dialog = settings(SettingsSection::Theme);
        select_settings_row(&mut dialog, 1);
        select_settings_row(&mut dialog, 7);
        switch_settings_section(&mut dialog, 1);
        select_settings_row(&mut dialog, 1);
        select_settings_row(&mut dialog, 2);
        switch_settings_section(&mut dialog, 1);
        select_settings_row(&mut dialog, 1);
        let Dialog::Settings {
            themes,
            view,
            sound,
            ..
        } = &dialog
        else {
            unreachable!()
        };
        assert_eq!(
            themes.selected, 1,
            "the theme click landed; the far one did not"
        );
        assert_eq!(
            view.selected, 1,
            "the view click landed; the far one did not"
        );
        assert_eq!(view.selected_row(), Some(ViewRow::Files));
        assert_eq!(
            sound.selected, 1,
            "the sound click landed in its own section"
        );
        assert_eq!(sound.selected_row(), Some(SoundRow::Switch(false)));
    }

    /// The view section's rows are switches, not choices: each wears
    /// its check when its surface is showing, independent of the
    /// cursor, and there is nothing to preview or save on landing.
    #[test]
    fn the_view_switches_check_what_is_showing() {
        let view = ViewSwitches::new(true, false);
        assert_eq!(view.selected_row(), Some(ViewRow::TabBar));
        assert!(view.is_checked(0), "the tab bar is showing");
        assert!(!view.is_checked(1), "the file panel is not");
        assert!(!view.is_checked(2), "no row past the last");
        assert_eq!(view.len(), ViewSwitches::ROWS.len());
    }

    /// One list, two groups: the arrows walk from the switch onto the
    /// sounds, each group checks its own saved row, and a sound plays
    /// once per arrival of the cursor.
    #[test]
    fn the_sound_list_follows_the_switch_and_plays_on_arrival() {
        let mut picker =
            SoundPicker::new(true, vec!["bow-ripple".into(), "system".into()], "system");
        assert_eq!(picker.len(), 4);
        assert_eq!(picker.selected_row(), Some(SoundRow::Switch(true)));
        assert!(
            picker.is_checked(0) && !picker.is_checked(1),
            "the switch's saved side"
        );
        assert!(
            !picker.is_checked(2) && picker.is_checked(3),
            "the saved cue"
        );
        assert_eq!(picker.row(3), Some(SoundRow::Sound("system")));
        assert_eq!(picker.row(4), None);

        assert_eq!(picker.arrived_on(), None, "opening plays nothing");
        picker.selected = 2;
        assert_eq!(picker.arrived_on(), Some("bow-ripple"));
        assert_eq!(picker.arrived_on(), None, "a redraw does not replay");
        picker.selected = 1;
        assert_eq!(picker.arrived_on(), None, "the switch is silent");
        picker.selected = 2;
        assert_eq!(picker.arrived_on(), Some("bow-ripple"), "coming back plays");

        let gone = SoundPicker::new(false, vec!["system".into()], "bow-ripple");
        assert_eq!(gone.active_sound, None, "a saved cue that is not on offer");
        assert!(!gone.is_checked(2));
        assert_eq!(gone.checked_sound(), None);
    }

    /// Landing on a row is checking it: the check moves to the row in
    /// its own group, the other group's check stays, and landing where
    /// the check already is moves nothing.
    #[test]
    fn landing_on_a_row_moves_its_groups_check() {
        let mut picker = SoundPicker::new(
            true,
            vec!["bow-ripple".into(), "system".into()],
            "bow-ripple",
        );
        assert!(!picker.check_selected(), "opening on the checked row");

        picker.selected = 1;
        assert!(picker.check_selected());
        assert!(!picker.on && picker.is_checked(1) && !picker.is_checked(0));
        assert!(picker.is_checked(2), "the cue's check stayed put");

        picker.selected = 3;
        assert!(picker.check_selected());
        assert_eq!(picker.checked_sound(), Some("system"));
        assert!(picker.is_checked(3) && !picker.is_checked(2));
        assert!(picker.is_checked(1), "the switch's check stayed put");
        assert!(!picker.check_selected(), "landing again moves nothing");
    }

    /// Tab wraps, and each section keeps its own cursor across a switch.
    #[test]
    fn sections_wrap_and_keep_their_own_selection() {
        let mut dialog = settings(SettingsSection::Theme);
        move_settings_selection(&mut dialog, 1);
        switch_settings_section(&mut dialog, 1);
        move_settings_selection(&mut dialog, 1);
        switch_settings_section(&mut dialog, 1);
        move_settings_selection(&mut dialog, -1);
        switch_settings_section(&mut dialog, 1);
        let Dialog::Settings {
            section,
            themes,
            view,
            sound,
            ..
        } = &dialog
        else {
            unreachable!()
        };
        assert_eq!(*section, SettingsSection::Theme, "Tab past the last wraps");
        assert_eq!(themes.selected, 1, "the theme cursor survived the trip");
        assert_eq!(view.selected, 1, "the view cursor moved on its own");
        assert_eq!(sound.selected, 0, "the sound cursor moved on its own");
        assert_eq!(sound.selected_row(), Some(SoundRow::Switch(true)));

        let mut back = settings(SettingsSection::Theme);
        switch_settings_section(&mut back, -1);
        assert!(matches!(
            back,
            Dialog::Settings {
                section: SettingsSection::Sound,
                ..
            }
        ));
        show_settings_section(&mut back, SettingsSection::Theme);
        assert!(matches!(
            back,
            Dialog::Settings {
                section: SettingsSection::Theme,
                ..
            }
        ));
    }

    /// The keybinds card is a viewport, not a picker: a wheel notch
    /// scrolls text, Enter closes it like Esc since nothing on it is
    /// chosen, the paging keys are its scroll keys, and the settings
    /// card's helpers leave it alone. Home and End in a picking section
    /// move its cursor to the ends.
    #[test]
    fn the_keybinds_card_is_a_viewport_and_confirms_nothing() {
        let card = keybinds();
        assert_eq!(dialog_wheel_rows(&card), 3, "a wheel notch scrolls text");
        assert_eq!(
            dialog_wheel_rows(&settings(SettingsSection::Sound)),
            1,
            "a wheel notch moves a picker one row"
        );
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(
            dialog_key_action(&card, &key(KeyCode::Enter)),
            DialogKeyAction::Cancel,
            "Enter closes: there is nothing to confirm"
        );
        assert_eq!(
            dialog_key_action(&card, &key(KeyCode::Esc)),
            DialogKeyAction::Cancel
        );
        assert_eq!(
            dialog_key_action(&card, &key(KeyCode::PageDown)),
            DialogKeyAction::Scroll(8)
        );
        assert_eq!(
            dialog_key_action(&card, &key(KeyCode::End)),
            DialogKeyAction::ScrollEnd
        );
        assert_eq!(
            dialog_key_action(&card, &key(KeyCode::Tab)),
            DialogKeyAction::Ignore,
            "no sections to walk"
        );
        let mut untouched = keybinds();
        move_settings_selection(&mut untouched, 3);
        jump_settings_selection(&mut untouched, true);
        select_settings_row(&mut untouched, 3);
        switch_settings_section(&mut untouched, 1);
        assert_eq!(untouched, card, "the settings helpers are no-ops on it");

        let mut picker = settings(SettingsSection::Theme);
        jump_settings_selection(&mut picker, true);
        switch_settings_section(&mut picker, 1);
        jump_settings_selection(&mut picker, true);
        switch_settings_section(&mut picker, 1);
        jump_settings_selection(&mut picker, true);
        let Dialog::Settings {
            themes,
            view,
            sound,
            ..
        } = &picker
        else {
            unreachable!()
        };
        assert_eq!(themes.selected, 1, "End is the last theme");
        assert_eq!(view.selected, view.len() - 1, "End is the last view row");
        assert_eq!(sound.selected, sound.len() - 1, "End is the last sound");
    }

    #[test]
    fn the_sound_picker_opens_on_the_saved_row() {
        let on = SoundPicker::new(true, vec!["system".into()], "system");
        let off = SoundPicker::new(false, vec!["system".into()], "system");
        assert_eq!(on.selected, 0);
        assert_eq!(off.selected, 1);
        assert_eq!(off.selected_row(), Some(SoundRow::Switch(false)));
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

    #[test]
    fn delete_word_removes_trailing_token_and_whitespace() {
        let mut buf = "hello world foo".to_string();
        delete_word(&mut buf);
        assert_eq!(buf, "hello world");
        delete_word(&mut buf);
        assert_eq!(buf, "hello");
        delete_word(&mut buf);
        assert_eq!(buf, "");
    }

    #[test]
    fn input_dialog_supports_word_delete_and_clear_chords() {
        let dialog = Dialog::NewTab {
            buffer: "hello world".into(),
        };
        // Ctrl+Backspace
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)
            ),
            DialogKeyAction::DeleteWord
        );
        // Alt+Backspace
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)
            ),
            DialogKeyAction::DeleteWord
        );
        // Ctrl+W
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
            ),
            DialogKeyAction::DeleteWord
        );
        // Ctrl+U
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
            ),
            DialogKeyAction::Clear
        );
        // Cmd+A / Ctrl+A
        assert_eq!(
            dialog_key_action(
                &dialog,
                &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)
            ),
            DialogKeyAction::Clear
        );
    }
}
