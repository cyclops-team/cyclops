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

pub const APP_MENU_BUTTON: &str = "☰ menu";

/// Named beside the sidebar's create button while the mouse rests on it. A
/// bare glyph does not say what it makes, and the sidebar is too narrow to
/// carry the whole phrase at rest.
pub const NEW_WORKSPACE_HINT: &str = "new";

pub const MENU_SPLIT_RIGHT: &str = "Split right";

pub const MENU_SPLIT_DOWN: &str = "Split down";

pub const MENU_ZOOM_PANE: &str = "Zoom pane";

pub const MENU_NAME_PANE: &str = "Name pane";

pub const MENU_CLOSE_PANE: &str = "Close pane";

pub const MENU_NEW_TAB: &str = "New tab";

pub const MENU_NEW_WORKSPACE: &str = "New workspace";

pub const MENU_TOGGLE_EVENTS: &str = "Event stream";

pub const MENU_KEYBINDS: &str = "Keybinds";

pub const MENU_DETACH: &str = "Detach";

pub const MENU_RENAME_TAB: &str = "Rename tab";

pub const MENU_CLOSE_TAB: &str = "Close tab";

pub const MENU_RENAME_WORKSPACE: &str = "Rename workspace";

pub const MENU_CLOSE_WORKSPACE: &str = "Close workspace";
