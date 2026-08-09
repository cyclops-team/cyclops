//! Workspace key bindings from config.toml.

use std::collections::HashMap;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Named workspace actions the keyboard router resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingAction {
    Detach,
    NextTab,
    PrevTab,
    SelectTab(usize),
    NewTab,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    /// No default chords: the pane context menu reaches these, and the
    /// keyboard's Shift upgrade of the focus chords covers the common
    /// case; bindable via config for anyone who wants dedicated keys.
    SwapPaneLeft,
    SwapPaneRight,
    SwapPaneUp,
    SwapPaneDown,
    SplitRight,
    SplitDown,
    ClosePane,
    ZoomPane,
    NamePane,
    RenameTab,
    CloseTab,
    NextWorkspace,
    PrevWorkspace,
    NewWorkspace,
    RenameWorkspace,
    CloseWorkspace,
    /// Collapse or reopen the sidebar; the state persists across restarts.
    ToggleSidebar,
    /// Show or hide the tab strip; the state persists across restarts.
    ///
    /// No default chord. The strip ships visible and the app menu's "Tab
    /// bar" item is the way it goes away and comes back, so hiding it can
    /// never be something that happened by itself; bindable via config for
    /// anyone who wants a key.
    ToggleTabBar,
    ToggleMotion,
    /// Show the event stream on the sidebar's Stream tab, or hide the
    /// sidebar when the stream is what's showing. The name predates the
    /// merge of the old right-hand panel into the sidebar; the config key
    /// (`toggle_event_panel`) keeps working unchanged.
    ToggleEventPanel,
    ShowKeybinds,
    /// Open the composer: address a message without leaving the workspace.
    Compose,
    /// No default chord: reached from the app menu, bindable via config.
    ShowThemes,
}

/// One human-readable row in the in-app keybinding reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingHelp {
    pub keys: String,
    pub action: String,
}

/// One binding: either after prefix or a direct chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingChord {
    Prefix(KeyCode),
    Direct(KeyEvent),
}

/// Default prefix-first bindings (tmux/Herdr shape).
pub fn default_bindings() -> HashMap<BindingAction, BindingChord> {
    HashMap::from([
        (
            BindingAction::Detach,
            BindingChord::Prefix(KeyCode::Char('d')),
        ),
        (
            BindingAction::NextTab,
            BindingChord::Prefix(KeyCode::Char('n')),
        ),
        (
            BindingAction::PrevTab,
            BindingChord::Prefix(KeyCode::Char('p')),
        ),
        (
            BindingAction::NewTab,
            BindingChord::Prefix(KeyCode::Char('c')),
        ),
        (
            BindingAction::FocusLeft,
            BindingChord::Prefix(KeyCode::Left),
        ),
        (
            BindingAction::FocusRight,
            BindingChord::Prefix(KeyCode::Right),
        ),
        (BindingAction::FocusUp, BindingChord::Prefix(KeyCode::Up)),
        (
            BindingAction::FocusDown,
            BindingChord::Prefix(KeyCode::Down),
        ),
        (
            BindingAction::SelectTab(1),
            BindingChord::Prefix(KeyCode::Char('1')),
        ),
        (
            BindingAction::SelectTab(2),
            BindingChord::Prefix(KeyCode::Char('2')),
        ),
        (
            BindingAction::SelectTab(3),
            BindingChord::Prefix(KeyCode::Char('3')),
        ),
        (
            BindingAction::SelectTab(4),
            BindingChord::Prefix(KeyCode::Char('4')),
        ),
        (
            BindingAction::SelectTab(5),
            BindingChord::Prefix(KeyCode::Char('5')),
        ),
        (
            BindingAction::SelectTab(6),
            BindingChord::Prefix(KeyCode::Char('6')),
        ),
        (
            BindingAction::SelectTab(7),
            BindingChord::Prefix(KeyCode::Char('7')),
        ),
        (
            BindingAction::SelectTab(8),
            BindingChord::Prefix(KeyCode::Char('8')),
        ),
        (
            BindingAction::SelectTab(9),
            BindingChord::Prefix(KeyCode::Char('9')),
        ),
        (
            BindingAction::SplitRight,
            BindingChord::Prefix(KeyCode::Char('%')),
        ),
        (
            BindingAction::SplitDown,
            BindingChord::Prefix(KeyCode::Char('\"')),
        ),
        (
            BindingAction::ClosePane,
            BindingChord::Prefix(KeyCode::Char('x')),
        ),
        (
            BindingAction::ZoomPane,
            BindingChord::Prefix(KeyCode::Char('z')),
        ),
        (
            BindingAction::NamePane,
            BindingChord::Prefix(KeyCode::Char('m')),
        ),
        (
            BindingAction::RenameTab,
            BindingChord::Prefix(KeyCode::Char(',')),
        ),
        (
            BindingAction::CloseTab,
            BindingChord::Prefix(KeyCode::Char('&')),
        ),
        (
            BindingAction::NextWorkspace,
            BindingChord::Prefix(KeyCode::Char(']')),
        ),
        (
            BindingAction::PrevWorkspace,
            BindingChord::Prefix(KeyCode::Char('[')),
        ),
        (
            BindingAction::NewWorkspace,
            BindingChord::Prefix(KeyCode::Char('w')),
        ),
        (
            BindingAction::RenameWorkspace,
            BindingChord::Prefix(KeyCode::Char('W')),
        ),
        (
            BindingAction::CloseWorkspace,
            BindingChord::Prefix(KeyCode::Char('K')),
        ),
        (
            BindingAction::ToggleSidebar,
            BindingChord::Prefix(KeyCode::Char('b')),
        ),
        (
            BindingAction::ToggleEventPanel,
            BindingChord::Prefix(KeyCode::Char('e')),
        ),
        (
            BindingAction::ShowKeybinds,
            BindingChord::Prefix(KeyCode::Char('?')),
        ),
        (
            BindingAction::Compose,
            BindingChord::Prefix(KeyCode::Char('@')),
        ),
    ])
}

/// Load bindings from `<home>/config.toml`, falling back to defaults.
pub fn load_bindings(home: &Path) -> HashMap<BindingAction, BindingChord> {
    let path = home.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return default_bindings();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return default_bindings();
    };
    let Some(workspace) = table.get("workspace").and_then(|v| v.as_table()) else {
        return default_bindings();
    };
    let Some(bindings) = workspace.get("bindings").and_then(|v| v.as_table()) else {
        return default_bindings();
    };
    let mut out = default_bindings();
    for (key, value) in bindings {
        let Some(spec) = value.as_str() else {
            continue;
        };
        let Some(action) = action_from_key(key) else {
            continue;
        };
        if let Some(chord) = parse_binding_spec(spec) {
            out.insert(action, chord);
        }
    }
    out
}

fn action_from_key(key: &str) -> Option<BindingAction> {
    match key {
        "detach" => Some(BindingAction::Detach),
        "next_tab" => Some(BindingAction::NextTab),
        "prev_tab" => Some(BindingAction::PrevTab),
        "new_tab" => Some(BindingAction::NewTab),
        "focus_left" => Some(BindingAction::FocusLeft),
        "focus_right" => Some(BindingAction::FocusRight),
        "focus_up" => Some(BindingAction::FocusUp),
        "focus_down" => Some(BindingAction::FocusDown),
        "swap_left" => Some(BindingAction::SwapPaneLeft),
        "swap_right" => Some(BindingAction::SwapPaneRight),
        "swap_up" => Some(BindingAction::SwapPaneUp),
        "swap_down" => Some(BindingAction::SwapPaneDown),
        "split_right" => Some(BindingAction::SplitRight),
        "split_down" => Some(BindingAction::SplitDown),
        "close_pane" => Some(BindingAction::ClosePane),
        "zoom_pane" => Some(BindingAction::ZoomPane),
        "name_pane" => Some(BindingAction::NamePane),
        "rename_tab" => Some(BindingAction::RenameTab),
        "close_tab" => Some(BindingAction::CloseTab),
        "next_workspace" => Some(BindingAction::NextWorkspace),
        "prev_workspace" => Some(BindingAction::PrevWorkspace),
        "new_workspace" => Some(BindingAction::NewWorkspace),
        "rename_workspace" => Some(BindingAction::RenameWorkspace),
        "close_workspace" => Some(BindingAction::CloseWorkspace),
        "toggle_sidebar" => Some(BindingAction::ToggleSidebar),
        "toggle_tab_bar" => Some(BindingAction::ToggleTabBar),
        "toggle_motion" => Some(BindingAction::ToggleMotion),
        "toggle_event_panel" => Some(BindingAction::ToggleEventPanel),
        "show_keybinds" => Some(BindingAction::ShowKeybinds),
        "compose" => Some(BindingAction::Compose),
        "show_themes" => Some(BindingAction::ShowThemes),
        s if s.starts_with("tab_") => s
            .strip_prefix("tab_")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|&n| (1..=9).contains(&n))
            .map(BindingAction::SelectTab),
        _ => None,
    }
}

/// Stable, complete help rows for the active binding map. The reference is
/// generated from the same map the router uses, so a config rebind cannot
/// leave the modal teaching the old key.
pub fn binding_help(bindings: &HashMap<BindingAction, BindingChord>) -> Vec<BindingHelp> {
    let mut rows: Vec<_> = bindings
        .iter()
        .map(|(action, chord)| {
            (
                *action,
                BindingHelp {
                    keys: chord_words(chord),
                    action: action_words(*action),
                },
            )
        })
        .collect();
    rows.sort_by_key(|(action, _)| help_rank(*action));
    // The pane swap chords are Shift upgrades of the focus chords, not
    // entries in the map ([`crate::action::route_binding_shifted`]), and
    // only a prefix chord can carry the upgrade: a direct chord's
    // modifiers are matched exactly, so Shift there is a different
    // binding. Deriving the rows from the live chords keeps a config
    // rebind from leaving the sheet teaching the old key.
    let swaps: Vec<BindingHelp> = [
        (BindingAction::FocusLeft, "Swap pane left"),
        (BindingAction::FocusRight, "Swap pane right"),
        (BindingAction::FocusUp, "Swap pane above"),
        (BindingAction::FocusDown, "Swap pane below"),
    ]
    .into_iter()
    .filter_map(|(focus, words)| match bindings.get(&focus) {
        Some(BindingChord::Prefix(code)) => Some(BindingHelp {
            keys: format!("Ctrl+B Shift+{}", key_words(*code)),
            action: words.into(),
        }),
        _ => None,
    })
    .collect();
    let at = rows
        .iter()
        .rposition(|(action, _)| {
            matches!(
                action,
                BindingAction::FocusLeft
                    | BindingAction::FocusRight
                    | BindingAction::FocusUp
                    | BindingAction::FocusDown
            )
        })
        .map_or(rows.len(), |index| index + 1);
    let mut rows: Vec<BindingHelp> = rows.into_iter().map(|(_, row)| row).collect();
    for (offset, row) in swaps.into_iter().enumerate() {
        rows.insert(at + offset, row);
    }
    // The grip, the sidebar chevron, copy, and paste are fixed gestures,
    // not bindings, but this reference is the one place a user looks for
    // them. Each row quotes the painted glyph so the sheet cannot drift
    // from the chrome.
    rows.push(BindingHelp {
        keys: format!("Drag {}", crate::render::PANE_GRIP),
        action: "Swap panes (bottom-right grip)".into(),
    });
    rows.push(BindingHelp {
        keys: format!(
            "Click {} / {}",
            crate::render::SIDEBAR_COLLAPSE,
            crate::render::SIDEBAR_EXPAND
        ),
        action: "Collapse / reopen the sidebar".into(),
    });
    // The tab strip's switch ships as a menu item and no default chord,
    // so nothing in the bindings map would mention it. An operator who
    // hid the strip has to be able to find the way back here.
    rows.push(BindingHelp {
        keys: "Menu".into(),
        action: "Tab bar: show or hide the strip".into(),
    });
    rows.push(BindingHelp {
        keys: "Drag".into(),
        action: "Select text, copy on release".into(),
    });
    rows.push(BindingHelp {
        keys: "Double / triple click".into(),
        action: "Copy word / line".into(),
    });
    rows.push(BindingHelp {
        keys: "Terminal paste".into(),
        action: "Paste into focused pane".into(),
    });
    rows
}

fn action_words(action: BindingAction) -> String {
    match action {
        BindingAction::Detach => "Detach".into(),
        BindingAction::NextTab => "Next tab".into(),
        BindingAction::PrevTab => "Previous tab".into(),
        BindingAction::SelectTab(index) => format!("Select tab {index}"),
        BindingAction::NewTab => "New tab".into(),
        BindingAction::FocusLeft => "Focus pane left".into(),
        BindingAction::FocusRight => "Focus pane right".into(),
        BindingAction::FocusUp => "Focus pane above".into(),
        BindingAction::FocusDown => "Focus pane below".into(),
        BindingAction::SwapPaneLeft => "Swap pane left".into(),
        BindingAction::SwapPaneRight => "Swap pane right".into(),
        BindingAction::SwapPaneUp => "Swap pane above".into(),
        BindingAction::SwapPaneDown => "Swap pane below".into(),
        BindingAction::SplitRight => "Split pane right".into(),
        BindingAction::SplitDown => "Split pane down".into(),
        BindingAction::ClosePane => "Close pane".into(),
        BindingAction::ZoomPane => "Zoom pane".into(),
        BindingAction::NamePane => "Name pane / agent".into(),
        BindingAction::RenameTab => "Rename tab".into(),
        BindingAction::CloseTab => "Close tab".into(),
        BindingAction::NextWorkspace => "Next workspace".into(),
        BindingAction::PrevWorkspace => "Previous workspace".into(),
        BindingAction::NewWorkspace => "New workspace here".into(),
        BindingAction::RenameWorkspace => "Rename workspace".into(),
        BindingAction::CloseWorkspace => "Close workspace".into(),
        BindingAction::ToggleSidebar => "Toggle sidebar".into(),
        BindingAction::ToggleTabBar => "Toggle tab bar".into(),
        BindingAction::ToggleMotion => "Toggle motion".into(),
        BindingAction::ToggleEventPanel => "Toggle event stream".into(),
        BindingAction::ShowKeybinds => "Keybinds".into(),
        BindingAction::Compose => "Send a message".into(),
        BindingAction::ShowThemes => "Themes".into(),
    }
}

fn help_rank(action: BindingAction) -> (u8, usize) {
    match action {
        BindingAction::FocusLeft => (10, 0),
        BindingAction::FocusRight => (11, 0),
        BindingAction::FocusUp => (12, 0),
        BindingAction::FocusDown => (13, 0),
        BindingAction::SwapPaneLeft => (13, 1),
        BindingAction::SwapPaneRight => (13, 2),
        BindingAction::SwapPaneUp => (13, 3),
        BindingAction::SwapPaneDown => (13, 4),
        BindingAction::SplitRight => (14, 0),
        BindingAction::SplitDown => (15, 0),
        BindingAction::ZoomPane => (16, 0),
        BindingAction::NamePane => (17, 0),
        BindingAction::ClosePane => (18, 0),
        BindingAction::NewTab => (30, 0),
        BindingAction::NextTab => (31, 0),
        BindingAction::PrevTab => (32, 0),
        BindingAction::SelectTab(index) => (33, index),
        BindingAction::RenameTab => (34, 0),
        BindingAction::CloseTab => (35, 0),
        BindingAction::NewWorkspace => (40, 0),
        BindingAction::NextWorkspace => (41, 0),
        BindingAction::PrevWorkspace => (42, 0),
        BindingAction::RenameWorkspace => (43, 0),
        BindingAction::CloseWorkspace => (44, 0),
        BindingAction::ToggleSidebar => (49, 0),
        BindingAction::ToggleTabBar => (49, 1),
        BindingAction::ToggleMotion => (49, 2),
        BindingAction::ToggleEventPanel => (50, 0),
        BindingAction::Compose => (50, 1),
        BindingAction::ShowKeybinds => (51, 0),
        BindingAction::ShowThemes => (52, 0),
        BindingAction::Detach => (53, 0),
    }
}

fn chord_words(chord: &BindingChord) -> String {
    match chord {
        BindingChord::Prefix(code) => format!("Ctrl+B {}", key_words(*code)),
        BindingChord::Direct(event) => {
            let mut parts = Vec::new();
            if event.modifiers.contains(KeyModifiers::CONTROL) {
                parts.push("Ctrl".to_string());
            }
            if event.modifiers.contains(KeyModifiers::ALT) {
                parts.push("Alt".to_string());
            }
            if event.modifiers.contains(KeyModifiers::SHIFT) {
                parts.push("Shift".to_string());
            }
            parts.push(key_words(event.code));
            parts.join("+")
        }
    }
}

fn key_words(code: KeyCode) -> String {
    match code {
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(character) => character.to_string(),
        other => format!("{other:?}"),
    }
}

/// Parse `prefix n` or `ctrl+alt+]`.
pub fn parse_binding_spec(spec: &str) -> Option<BindingChord> {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("prefix ") {
        return parse_prefix_key(rest.trim());
    }
    if spec.contains('+') {
        return parse_direct_chord(spec);
    }
    parse_prefix_key(spec)
}

fn parse_prefix_key(s: &str) -> Option<BindingChord> {
    let code = match s {
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        c if c.len() == 1 => KeyCode::Char(c.chars().next()?),
        _ => return None,
    };
    Some(BindingChord::Prefix(code))
}

fn parse_direct_chord(s: &str) -> Option<BindingChord> {
    let mut mods = KeyModifiers::empty();
    let mut key_part = s;
    for part in s.split('+') {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            _ => key_part = part,
        }
    }
    let code = match key_part {
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        c if c.len() == 1 => KeyCode::Char(c.chars().next()?),
        _ => return None,
    };
    Some(BindingChord::Direct(KeyEvent::new(code, mods)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_chord_rebinds_next_tab() {
        let chord = parse_binding_spec("ctrl+alt+]").expect("parse");
        assert!(matches!(chord, BindingChord::Direct(_)));
    }

    #[test]
    fn prefix_key_parses() {
        let chord = parse_binding_spec("prefix n").expect("parse");
        assert_eq!(chord, BindingChord::Prefix(KeyCode::Char('n')));
    }

    #[test]
    fn help_rows_come_from_the_active_bindings() {
        let mut bindings = default_bindings();
        bindings.insert(
            BindingAction::NamePane,
            BindingChord::Direct(KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
        );

        let rows = binding_help(&bindings);
        let naming = rows
            .iter()
            .find(|row| row.action == "Name pane / agent")
            .expect("naming row");
        assert_eq!(naming.keys, "Ctrl+Alt+m");
        assert!(rows.iter().any(|row| row.action == "Keybinds"));
        assert!(rows.iter().any(|row| row.action == "Detach"));
    }

    /// Both sidebar chords ship by default and reach the help sheet from
    /// the live map, so the sheet teaches the collapse toggle alongside
    /// the stream toggle it inherited.
    #[test]
    fn help_teaches_the_sidebar_and_stream_toggles() {
        let rows = binding_help(&default_bindings());
        let sidebar = rows
            .iter()
            .find(|row| row.action == "Toggle sidebar")
            .expect("sidebar row");
        assert_eq!(sidebar.keys, "Ctrl+B b");
        let stream = rows
            .iter()
            .find(|row| row.action == "Toggle event stream")
            .expect("stream row");
        assert_eq!(stream.keys, "Ctrl+B e");
    }

    #[test]
    fn help_documents_copy_and_paste() {
        let rows = binding_help(&default_bindings());
        assert!(rows
            .iter()
            .any(|row| row.action == "Select text, copy on release"));
        assert!(rows.iter().any(|row| row.action == "Copy word / line"));
        assert!(rows
            .iter()
            .any(|row| row.action == "Paste into focused pane"));
    }

    /// The grip is a fixed gesture like copy and paste, and its row quotes
    /// the glyph the frame actually paints, so a glyph change cannot leave
    /// the sheet teaching a handle that no longer exists.
    #[test]
    fn help_teaches_the_grip_swap_with_the_painted_glyph() {
        let rows = binding_help(&default_bindings());
        let grip = rows
            .iter()
            .find(|row| row.action == "Swap panes (bottom-right grip)")
            .expect("grip row");
        assert_eq!(grip.keys, format!("Drag {}", crate::render::PANE_GRIP));
    }

    /// The chevron is a fixed gesture like the grip, and its row quotes
    /// the glyphs the chrome actually paints, so a glyph change cannot
    /// leave the sheet teaching a control that is no longer there.
    #[test]
    fn help_teaches_the_sidebar_chevron_with_the_painted_glyphs() {
        let rows = binding_help(&default_bindings());
        let chevron = rows
            .iter()
            .find(|row| row.action == "Collapse / reopen the sidebar")
            .expect("chevron row");
        assert_eq!(
            chevron.keys,
            format!(
                "Click {} / {}",
                crate::render::SIDEBAR_COLLAPSE,
                crate::render::SIDEBAR_EXPAND
            )
        );
    }

    /// The swap rows derive from the live focus chords, right after them,
    /// so a rebind cannot leave the sheet teaching the old key.
    #[test]
    fn help_derives_the_swap_chords_from_the_focus_chords() {
        let rows = binding_help(&default_bindings());
        let focus_down = rows
            .iter()
            .position(|row| row.action == "Focus pane below")
            .expect("focus rows present");
        let swaps: Vec<_> = rows[focus_down + 1..focus_down + 5]
            .iter()
            .map(|row| (row.keys.as_str(), row.action.as_str()))
            .collect();
        assert_eq!(
            swaps,
            vec![
                ("Ctrl+B Shift+Left", "Swap pane left"),
                ("Ctrl+B Shift+Right", "Swap pane right"),
                ("Ctrl+B Shift+Up", "Swap pane above"),
                ("Ctrl+B Shift+Down", "Swap pane below"),
            ]
        );

        // A rebound prefix chord changes the taught key with it.
        let mut bindings = default_bindings();
        bindings.insert(
            BindingAction::FocusLeft,
            BindingChord::Prefix(KeyCode::Char('h')),
        );
        let rows = binding_help(&bindings);
        assert!(rows
            .iter()
            .any(|row| row.action == "Swap pane left" && row.keys == "Ctrl+B Shift+h"));

        // A direct chord matches its modifiers exactly, so no Shift
        // upgrade exists and no swap row is taught for it.
        bindings.insert(
            BindingAction::FocusLeft,
            BindingChord::Direct(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)),
        );
        let rows = binding_help(&bindings);
        assert!(!rows.iter().any(|row| row.action == "Swap pane left"));
    }
}
