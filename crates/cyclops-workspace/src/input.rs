//! Map crossterm keys to tmux `send-keys` arguments.

pub mod mouse;
pub mod router;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode one key event for passthrough to the focused pane.
pub fn encode_send_keys(ev: &KeyEvent) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(name) = tmux_key_name(ev) {
        out.push(name);
        return out;
    }
    if let KeyCode::Char(c) = ev.code {
        if ev.modifiers.is_empty() || ev.modifiers == KeyModifiers::SHIFT {
            out.push(c.to_string());
            return out;
        }
    }
    out
}

fn tmux_key_name(ev: &KeyEvent) -> Option<String> {
    use KeyCode::*;
    // Crossterm normally reports Shift+Tab as BackTab, but accepting the
    // shifted Tab shape too keeps the passthrough correct across terminals.
    if ev.code == BackTab || (ev.code == Tab && ev.modifiers.contains(KeyModifiers::SHIFT)) {
        // BTab already implies Shift. Preserve any additional modifiers
        // without redundantly producing `S-BTab`.
        let prefix = modifier_prefix(ev.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT));
        return Some(format!("{prefix}BTab"));
    }
    let prefix = modifier_prefix(ev.modifiers);
    let name = match ev.code {
        Enter => "Enter",
        Backspace => "BSpace",
        Left => "Left",
        Right => "Right",
        Up => "Up",
        Down => "Down",
        Home => "Home",
        End => "End",
        PageUp => "PPage",
        PageDown => "NPage",
        Tab => "Tab",
        Esc => "Escape",
        Delete => "DC",
        Insert => "IC",
        F(n) => return Some(format!("{prefix}F{n}")),
        Char(c)
            if ev
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            return Some(format!("{prefix}{}", c.to_ascii_lowercase()));
        }
        _ => return None,
    };
    Some(format!("{prefix}{name}"))
}

fn modifier_prefix(modifiers: KeyModifiers) -> String {
    let mut prefix = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        prefix.push_str("C-");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        prefix.push_str("M-");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        prefix.push_str("S-");
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_encode() {
        let ev = KeyEvent::new(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(encode_send_keys(&ev), vec!["Up".to_string()]);
    }

    #[test]
    fn ctrl_combo_encodes() {
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_send_keys(&ev), vec!["C-c".to_string()]);
    }

    #[test]
    fn plain_char_literal() {
        let ev = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(encode_send_keys(&ev), vec!["a".to_string()]);
    }

    #[test]
    fn shift_tab_reaches_agent_tuis_as_backtab() {
        for ev in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            assert_eq!(encode_send_keys(&ev), vec!["BTab".to_string()]);
        }
    }

    #[test]
    fn backtab_preserves_additional_modifiers() {
        let ev = KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert_eq!(encode_send_keys(&ev), vec!["C-M-BTab".to_string()]);
    }

    #[test]
    fn combined_modifiers_are_not_silently_discarded() {
        let ev = KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert_eq!(encode_send_keys(&ev), vec!["C-M-S-Left".to_string()]);
    }
}
