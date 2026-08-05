//! User-facing sentences for the workspace UI.

pub const NO_TMUX_SERVER: &str = "No tmux server is running. Start one with: tmux new -s main";

pub const DETACHED: &str = "Detached from workspace.";

pub const HELP_HINT: &str = "Run cyclops --help for commands.";

pub const CONFIRM_CLOSE_PANE: &str = "Close this pane? An agent may be running. [y/N]";

pub const RENAME_TAB_PROMPT: &str = "Rename tab: ";

pub const NEW_WORKSPACE_PROMPT: &str = "New workspace folder: ";

pub const RENAME_WORKSPACE_PROMPT: &str = "Rename workspace: ";

pub const CONFIRM_CLOSE_WORKSPACE: &str = "Close this workspace? [y/N]";

pub const RECONNECTING_NOTE: &str = "reconnecting…";

pub const PAUSED_NOTE: &str = "paused";

pub const SERVER_GONE_OFFER: &str =
    "tmux server is gone. Start a new session with: tmux new -s main";

pub const EVENT_PANEL_EMPTY: &str = "No events yet.";

pub const APP_MENU_BUTTON: &str = "☰ menu";

pub const MENU_SPLIT_RIGHT: &str = "Split right";

pub const MENU_SPLIT_DOWN: &str = "Split down";

pub const MENU_ZOOM_PANE: &str = "Zoom pane";

pub const MENU_CLOSE_PANE: &str = "Close pane";

pub const MENU_NEW_TAB: &str = "New tab";

pub const MENU_NEW_WORKSPACE: &str = "New workspace…";

pub const MENU_TOGGLE_EVENTS: &str = "Events panel";

pub const MENU_DETACH: &str = "Detach";
