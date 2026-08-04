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
    SplitRight,
    SplitDown,
    ClosePane,
    ZoomPane,
    RenameTab,
    CloseTab,
    NextWorkspace,
    PrevWorkspace,
    NewWorkspace,
    RenameWorkspace,
    CloseWorkspace,
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
        "split_right" => Some(BindingAction::SplitRight),
        "split_down" => Some(BindingAction::SplitDown),
        "close_pane" => Some(BindingAction::ClosePane),
        "zoom_pane" => Some(BindingAction::ZoomPane),
        "rename_tab" => Some(BindingAction::RenameTab),
        "close_tab" => Some(BindingAction::CloseTab),
        "next_workspace" => Some(BindingAction::NextWorkspace),
        "prev_workspace" => Some(BindingAction::PrevWorkspace),
        "new_workspace" => Some(BindingAction::NewWorkspace),
        "rename_workspace" => Some(BindingAction::RenameWorkspace),
        "close_workspace" => Some(BindingAction::CloseWorkspace),
        s if s.starts_with("tab_") => s
            .strip_prefix("tab_")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|&n| (1..=9).contains(&n))
            .map(BindingAction::SelectTab),
        _ => None,
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
}
