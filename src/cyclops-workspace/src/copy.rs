//! User-facing sentences for the workspace UI.

/// Session bare `cyclops` creates when the server has nothing to attach to.
pub const DEFAULT_SESSION_NAME: &str = "main";

pub const DETACHED: &str = "Detached from workspace.";

pub const HELP_HINT: &str = "Run cyclops --help for commands.";

pub const CONFIRM_CLOSE_PANE: &str = "Close this pane? An agent may be running.";

pub const CONFIRM_CLOSE_TAB: &str = "Close this tab? Its panes may host agents.";

pub const RENAME_TAB_PROMPT: &str = "Rename tab";

pub const NEW_TAB_TITLE: &str = "New tab";

pub const NEW_TAB_HINT: &str = "Opens in the current pane's directory.";

pub const NAME_PANE_TITLE: &str = "Name pane";

pub const NAME_PANE_HINT: &str = "Used to identify and message this agent, e.g. reviewer.";

pub const KEYBINDS_TITLE: &str = "Keybinds";

pub const KEYBINDS_HINT: &str = "Scroll with ↑/↓, PgUp/PgDn, or the mouse wheel.";

/// The compact state vocabulary (rule 11), spelled out once for the
/// keybinds dialog since sidebar rows and inactive pane borders show only
/// the glyph half of it.
pub const STATE_GLYPH_LEGEND: &str = "Status:  ○ idle   ● working   ⚠ needs attention   ✕ dead";

pub const RENAME_WORKSPACE_PROMPT: &str = "Rename workspace";

pub const CONFIRM_CLOSE_WORKSPACE: &str = "Close this workspace?";

pub const BUTTON_CREATE: &str = "Create";

pub const BUTTON_SAVE: &str = "Save";

pub const BUTTON_CANCEL: &str = "Cancel";

pub const BUTTON_CONFIRM: &str = "Confirm";

pub const BUTTON_CLOSE: &str = "Close";

pub const RECONNECTING_NOTE: &str = "reconnecting…";

pub const PAUSED_NOTE: &str = "paused";

pub const SERVER_GONE_OFFER: &str = "tmux server is gone. Run cyclops again to start a fresh one.";

pub const EVENT_STREAM_EMPTY: &str = "No events yet.";

// No space after the glyph: ☰ is ambiguous-width and most fonts already
// draw it with a right shoulder, so a literal space read as a two-cell gap.
pub const APP_MENU_BUTTON: &str = "☰menu";

/// The sidebar's tab chips. They name what the body below them shows and
/// take the place of the plain title the session tree used to carry.
pub const SIDEBAR_TAB_SESSIONS: &str = "Workspaces";

pub const SIDEBAR_TAB_STREAM: &str = "Stream";

/// Named beside the sidebar's create button while the mouse rests on it. A
/// bare glyph does not say what it makes, and the sidebar is too narrow to
/// carry the whole phrase at rest.
pub const NEW_WORKSPACE_HINT: &str = "new";

/// Said on the notice line after a selection reaches the clipboard. The
/// count is the whole point: a bare "copied" cannot tell a whole line from
/// the one word a mis-aimed double click took, and the clipboard write
/// itself can never report back (OSC 52 is fire and forget), so what this
/// reports is what the selection actually extracted.
pub fn copied(text: &str) -> String {
    let lines = text.lines().count();
    if lines > 1 {
        return format!("copied {lines} lines");
    }
    match text.chars().count() {
        1 => "copied 1 character".to_string(),
        n => format!("copied {n} characters"),
    }
}

pub const MENU_SPLIT_RIGHT: &str = "Split right";

pub const MENU_SPLIT_DOWN: &str = "Split down";

pub const MENU_ZOOM_PANE: &str = "Zoom pane";

pub const MENU_NAME_PANE: &str = "Name pane";

pub const MENU_SWAP_LEFT: &str = "Swap left";

pub const MENU_SWAP_RIGHT: &str = "Swap right";

pub const MENU_SWAP_UP: &str = "Swap up";

pub const MENU_SWAP_DOWN: &str = "Swap down";

pub const MENU_CLOSE_PANE: &str = "Close pane";

pub const MENU_NEW_TAB: &str = "New tab";

pub const MENU_NEW_WORKSPACE: &str = "New workspace";

pub const MENU_TOGGLE_EVENTS: &str = "Event stream";

/// Marks a menu row whose setting is on, and the active row in the theme
/// picker. One cell in every monospace font, chosen by state and never by
/// theme, so it reads under `NO_COLOR` (rule 11).
pub const MENU_CHECK: &str = "✓";

pub const MENU_TAB_BAR: &str = "Tab bar";
/// The only visible switch for chrome fades (`crate::animate`).
pub const MENU_MOTION: &str = "Motion";

pub const MENU_THEMES: &str = "Themes";

pub const MENU_KEYBINDS: &str = "Keybinds";

pub const MENU_DETACH: &str = "Detach";

pub const MENU_RENAME_TAB: &str = "Rename tab";

pub const MENU_CLOSE_TAB: &str = "Close tab";

pub const MENU_RENAME_WORKSPACE: &str = "Rename workspace";

pub const MENU_CLOSE_WORKSPACE: &str = "Close workspace";

pub const THEMES_TITLE: &str = "Themes";

pub const THEMES_HINT: &str = "Pick with ↑/↓, Enter applies everywhere.";

pub const THEMES_EMPTY: &str = "No themes found. Run cyclops start to seed the shipped ones.";

pub const BUTTON_APPLY: &str = "Apply";

/// Said in the picker after a switch no daemon confirmed. The config is
/// written either way, so this is a "when", not a "but"; the CLI says the
/// same after `cyclops theme <name>` (its `THEME_NEXT_COMMAND`).
pub const THEME_SAVED_NO_DAEMON: &str = "Saved. The next command picks it up.";

/// Said after a switch a running daemon did not take: the config is
/// written and this workspace repaints from it, but pane borders are
/// cyclopsd's to repaint and it is painting something else. Mirrors the
/// CLI's `theme_not_live` story, sentence for sentence.
pub fn theme_not_live(painting: Option<&str>) -> String {
    match painting {
        Some(now) => format!(
            "Saved, but cyclopsd is still painting {now}, so pane borders did not change. Check CYCLOPS_THEME and the themes directory where cyclopsd runs, then restart it."
        ),
        None => "Saved, but cyclopsd didn't take the switch, so pane borders did not change. Restart cyclopsd to pick it up.".to_string(),
    }
}

/// A picked theme whose file stopped loading between the listing and the
/// apply. Nothing was written; the CLI refuses the same file by name.
pub fn theme_unusable(name: &str, cause: &str) -> String {
    format!("can't use theme \"{name}\": {cause}. Nothing was changed.")
}

/// A picked theme whose file now parses but sets no token, so switching
/// to it would repaint nothing. Nothing was written.
pub fn theme_sets_no_colors(name: &str) -> String {
    format!(
        "can't use theme \"{name}\": it sets no colors, so switching to it would change nothing on screen. Nothing was changed."
    )
}

/// The config could not be written, so nothing switched.
pub fn theme_not_saved(cause: &str) -> String {
    format!("can't save the theme: {cause}. Nothing was changed.")
}

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------

/// The composer's own title. The grammar is in the title because it is the
/// whole interface: there is no second field to Tab into.
pub const COMPOSE_TITLE: &str = "Send a message";
/// The sidebar footer's hover note. Short because it shares one slot with
/// the create button's hint and the "+N more" count, and a phrase that has
/// to be truncated teaches nothing.
pub const SIDEBAR_COMPOSE_HINT: &str = "send";
/// The grammar, then the two keys that decide what Enter does.
///
/// This line is the widest thing on the card and so sets its width, which
/// is why it dropped "Esc closes" to make room: the action row two rows
/// below paints `Esc Cancel`, so the key is still on screen, and keeping
/// both would have grown the box by 21 columns.
///
/// Alt+Enter stands for all three newline chords. Naming the fallbacks
/// would cost more width than it teaches, and a reader who reaches for
/// Shift+Enter or Ctrl+J finds it works anyway.
pub const COMPOSE_HINT: &str = "@name then your message · Enter sends · Alt+Enter new line";
pub const BUTTON_SEND: &str = "Send";

/// In flight. Named, because a workspace with several agents open should
/// not make the reader work out which one is being written to.
pub fn compose_sending(to: &str) -> String {
    format!("sending to {to}…")
}

/// The send answered. The receipt is already in the vocabulary `cyclops
/// send` prints, so it is passed through rather than re-worded here.
pub fn compose_sent(receipt: &str) -> String {
    receipt.to_string()
}

/// Nothing reached the record. Distinct from a delivery that was attempted
/// and failed: this one is not somewhere waiting to be found, so the
/// operator's next move is to retry rather than to go looking.
pub fn compose_failed(to: &str, cause: &str) -> String {
    format!("nothing sent to {to}: {cause}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three shapes a copy takes: a dragged block, a triple-clicked
    /// line, and a double-clicked word. Each says what it took, because
    /// the count is the only thing that distinguishes them on screen.
    #[test]
    fn copied_counts_what_the_selection_took() {
        assert_eq!(copied("one\ntwo\nthree"), "copied 3 lines");
        assert_eq!(copied("cargo test"), "copied 10 characters");
        assert_eq!(copied("x"), "copied 1 character");
        // Column-accurate, not byte-accurate: a wide glyph is one thing
        // the operator selected, not three bytes.
        assert_eq!(copied("你好"), "copied 2 characters");
    }
}
