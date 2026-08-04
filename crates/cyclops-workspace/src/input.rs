//! Map crossterm keys to tmux `send-keys` arguments.

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
    let prefix = if ev.modifiers.contains(KeyModifiers::CONTROL) {
        "C-"
    } else if ev.modifiers.contains(KeyModifiers::ALT) {
        "M-"
    } else {
        ""
    };
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
        Char(c) if ev.modifiers.contains(KeyModifiers::CONTROL) => {
            return Some(format!("C-{}", c.to_ascii_lowercase()));
        }
        Char(c) if ev.modifiers.contains(KeyModifiers::ALT) => {
            return Some(format!("M-{c}"));
        }
        _ => return None,
    };
    Some(format!("{prefix}{name}"))
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
}
