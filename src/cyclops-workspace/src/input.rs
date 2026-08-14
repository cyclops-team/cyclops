//! Device-event decoding: turning a raw crossterm key or mouse event into
//! something routing can act on, before any tmux call or app-state mutation.
//! `mouse` owns hit regions and menu state; `router` owns the prefix-chord
//! state machine. This top level owns what is left: encoding a key for pane
//! passthrough, and deciding whether a key preempts routing entirely (an
//! Escape that cancels a chrome drag or selection). No tmux IO, no App
//! state — a decision here is a pure function of the event and, at most,
//! the couple of booleans the caller already has in hand.

pub mod mouse;
pub mod router;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode one key event for passthrough to the focused pane.
pub fn encode_send_keys(ev: &KeyEvent) -> Vec<String> {
    // Editing chords no pane can receive natively, translated to the
    // readline vocabulary every composer in a pane already understands.
    // A terminal has no byte sequence for Ctrl+Backspace or the Cmd key,
    // so forwarding them literally sends either nothing or a plain
    // Backspace; what the operator meant is the edit, not the chord.
    // C-w is delete-word-back in readline shells, vim's insert mode, and
    // the Claude/Codex composers; C-u is kill-to-line-start in the same
    // places, which is what Cmd+Backspace does in every macOS text field.
    // Exact modifiers, not contains: Ctrl+Alt+Backspace is not this chord,
    // and translating it anyway would send an edit nobody asked for.
    if ev.code == KeyCode::Backspace && ev.modifiers == KeyModifiers::CONTROL {
        return vec!["C-w".to_string()];
    }
    if ev.code == KeyCode::Backspace && ev.modifiers == KeyModifiers::SUPER {
        return vec!["C-u".to_string()];
    }
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

/// Cmd+A over a pane, resolved against what the operator does next.
///
/// A terminal composer has no selection to show, so "select all" cannot
/// mean a highlight. What it can honestly mean is the GUI gesture people
/// actually perform: Cmd+A then a delete clears the whole line. The arm
/// is pane-scoped and spent by the next key forwarded to a pane: a
/// delete clears the line, anything else forwards normally and the arm
/// is forgotten. (Chrome events between the two — a menu, a mouse move —
/// leave it standing, same as a GUI selection surviving them.) Only a
/// delete acts on the arm — in a GUI, typing over a selection replaces
/// it, but with no visible selection that behavior would silently
/// destroy a half-written prompt on a mistyped chord.
#[derive(Default)]
pub struct SelectAll {
    armed_for: Option<String>,
}

/// What the key means, given the arm state.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectAllOutcome {
    /// Cmd+A: remember the pane, send nothing.
    Armed,
    /// A delete while armed for this pane: clear the whole input line
    /// (the caller sends `C-e C-u`, cursor to end then kill to start,
    /// so the cursor's position cannot leave a tail behind).
    ClearLine,
    /// Any other key: forward as usual.
    Forward,
}

impl SelectAll {
    pub fn on_key(&mut self, pane: &str, ev: &KeyEvent) -> SelectAllOutcome {
        if ev.code == KeyCode::Char('a') && ev.modifiers == KeyModifiers::SUPER {
            self.armed_for = Some(pane.to_string());
            return SelectAllOutcome::Armed;
        }
        // One key resolves the arm either way, and an arm from another
        // pane is stale, not actionable: the delete the operator is
        // pressing belongs to the pane they are looking at now.
        let armed = self.armed_for.take().is_some_and(|p| p == pane);
        let is_delete = matches!(ev.code, KeyCode::Backspace | KeyCode::Delete);
        if armed && is_delete {
            SelectAllOutcome::ClearLine
        } else {
            SelectAllOutcome::Forward
        }
    }
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

/// Whether this key decodes to "cancel the in-progress chrome operation"
/// before any routing happens — a text selection drag or a chrome drag both
/// answer to Escape ahead of the router and the focused pane, so the app
/// loop checks this first and consumes the key rather than forwarding it.
pub fn escape_cancels_visual_state(
    code: crossterm::event::KeyCode,
    selection_active: bool,
    drag_active: bool,
) -> bool {
    code == crossterm::event::KeyCode::Esc && (selection_active || drag_active)
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

    #[test]
    fn editing_chords_translate_to_the_readline_vocabulary() {
        // Ctrl+Backspace is delete-word-back everywhere a pane composer
        // reads keys, and C-w is how that edit is spelled on the wire.
        let ctrl = KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL);
        assert_eq!(encode_send_keys(&ctrl), vec!["C-w".to_string()]);
        // Cmd+Backspace is kill-to-line-start in every macOS text field.
        let cmd = KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER);
        assert_eq!(encode_send_keys(&cmd), vec!["C-u".to_string()]);
        // A plain Backspace stays itself; the translation must not widen.
        let plain = KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty());
        assert_eq!(encode_send_keys(&plain), vec!["BSpace".to_string()]);
    }

    #[test]
    fn cmd_a_arms_one_delete_and_only_for_its_own_pane() {
        let cmd_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER);
        let bspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty());
        let letter = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty());

        // Arm then delete: the line clears.
        let mut s = SelectAll::default();
        assert_eq!(s.on_key("%1", &cmd_a), SelectAllOutcome::Armed);
        assert_eq!(s.on_key("%1", &bspace), SelectAllOutcome::ClearLine);
        // The arm is spent: a second delete is an ordinary key.
        assert_eq!(s.on_key("%1", &bspace), SelectAllOutcome::Forward);

        // Arm then type: forwarded, and the arm is forgotten.
        assert_eq!(s.on_key("%1", &cmd_a), SelectAllOutcome::Armed);
        assert_eq!(s.on_key("%1", &letter), SelectAllOutcome::Forward);
        assert_eq!(s.on_key("%1", &bspace), SelectAllOutcome::Forward);

        // Arm in one pane, delete in another: the stale arm must not
        // clear a line the operator never selected.
        assert_eq!(s.on_key("%1", &cmd_a), SelectAllOutcome::Armed);
        assert_eq!(s.on_key("%2", &bspace), SelectAllOutcome::Forward);
    }

    #[test]
    fn escape_is_consumed_when_it_cancels_a_chrome_operation() {
        assert!(escape_cancels_visual_state(KeyCode::Esc, true, false));
        assert!(escape_cancels_visual_state(KeyCode::Esc, false, true));
        assert!(!escape_cancels_visual_state(KeyCode::Esc, false, false));
        assert!(!escape_cancels_visual_state(KeyCode::Char('x'), true, true));
    }
}
