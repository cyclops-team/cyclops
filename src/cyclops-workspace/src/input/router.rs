//! Keyboard router: prefix state machine and binding resolution.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::bindings::{binding_help, BindingAction, BindingChord, BindingHelp};

/// What the router does with one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouterResult {
    /// Arm the prefix; consume the key.
    PrefixArmed,
    /// Resolved workspace action.
    Action(BindingAction),
    /// Forward to the focused pane via send-keys.
    PassThrough(KeyEvent),
    /// Consumed without action (unknown prefix key).
    Consumed,
}

/// Prefix-first keyboard router with rebindable chords.
pub struct Router {
    prefix_armed: bool,
    bindings: HashMap<BindingAction, BindingChord>,
}

impl Router {
    pub fn new(bindings: HashMap<BindingAction, BindingChord>) -> Self {
        Router {
            prefix_armed: false,
            bindings,
        }
    }

    /// Whether the prefix chord is armed for the next key. Only a test
    /// checks this directly; product code reads the `RouterResult` `route`
    /// returns instead.
    #[cfg(test)]
    fn prefix_armed(&self) -> bool {
        self.prefix_armed
    }

    pub fn route(&mut self, key: KeyEvent) -> RouterResult {
        if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.prefix_armed = true;
            return RouterResult::PrefixArmed;
        }

        if self.prefix_armed {
            self.prefix_armed = false;
            if let Some(action) = self.match_prefix(key.code) {
                return RouterResult::Action(action);
            }
            return RouterResult::Consumed;
        }

        if let Some(action) = self.match_direct(&key) {
            return RouterResult::Action(action);
        }

        RouterResult::PassThrough(key)
    }

    pub fn help(&self) -> Vec<BindingHelp> {
        binding_help(&self.bindings)
    }

    /// The chord bound to `action`, for callers that need to know how a
    /// match was made: prefix chords are matched by key code alone, so a
    /// modifier on the event is extra information there and nowhere else.
    pub fn chord(&self, action: BindingAction) -> Option<&BindingChord> {
        self.bindings.get(&action)
    }

    fn match_prefix(&self, code: KeyCode) -> Option<BindingAction> {
        for (action, chord) in &self.bindings {
            if matches!(chord, BindingChord::Prefix(c) if *c == code) {
                return Some(*action);
            }
        }
        None
    }

    fn match_direct(&self, key: &KeyEvent) -> Option<BindingAction> {
        for (action, chord) in &self.bindings {
            if let BindingChord::Direct(ev) = chord {
                if ev.code == key.code && ev.modifiers == key.modifiers {
                    return Some(*action);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::default_bindings;

    fn ctrl_b() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
    }

    #[test]
    fn prefix_arms_then_resolves_detach() {
        let mut router = Router::new(default_bindings());
        assert_eq!(router.route(ctrl_b()), RouterResult::PrefixArmed);
        assert!(router.prefix_armed());
        assert_eq!(
            router.route(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty())),
            RouterResult::Action(BindingAction::Detach)
        );
    }

    #[test]
    fn unreserved_keys_pass_through() {
        let mut router = Router::new(default_bindings());
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(router.route(key), RouterResult::PassThrough(key));
    }

    #[test]
    fn unknown_prefix_key_is_consumed() {
        let mut router = Router::new(default_bindings());
        router.route(ctrl_b());
        assert_eq!(
            router.route(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty())),
            RouterResult::Consumed
        );
    }

    #[test]
    fn direct_chord_skips_prefix() {
        let mut bindings = default_bindings();
        bindings.insert(
            BindingAction::NextTab,
            BindingChord::Direct(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
        );
        let mut router = Router::new(bindings);
        assert_eq!(
            router.route(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            RouterResult::Action(BindingAction::NextTab)
        );
    }

    #[test]
    fn tab_number_after_prefix() {
        let mut router = Router::new(default_bindings());
        router.route(ctrl_b());
        assert_eq!(
            router.route(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::empty())),
            RouterResult::Action(BindingAction::SelectTab(2))
        );
    }
}
