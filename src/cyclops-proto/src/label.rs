//! What a pane may be called, and why three names are not available.
//!
//! An agent's label is how everything addresses it: `cyclops send`, the
//! roster, the border, the record. Three strings already mean something
//! else in that namespace, so a pane wearing one could not be addressed:
//!
//! 1. `admin` is the operator's durable mailbox identity. It is a valid
//!    message address, but no pane may wear it. Only a same-user caller proven
//!    outside every watched pane resolves as `admin`; a pane with that label
//!    would make identity ambiguous.
//! 2. `*` addresses every agent at once.
//! 3. `%0`, `%1` and so on are tmux's own pane ids, which cyclops accepts
//!    anywhere a label is accepted.
//!
//! One home for the rule because three surfaces enforce it: the daemon on
//! `pane.label`, `cyclops hooks install --agent`, and anything that
//! validates before asking. Three copies drifted once already for the
//! attention rule (see [`crate::attention`]), and the failure was the
//! same shape: the surfaces disagreed about what was true.

/// The operator's own identity in the record.
pub const ADMIN: &str = "admin";

/// The broadcast target.
pub const EVERYONE: &str = "*";

/// Why a label cannot be used, with the sentence a person should read.
///
/// The message says what the name already means, because "reserved" on
/// its own tells the reader nothing they can act on. It ends in a name
/// they can use instead.
pub fn refusal(label: &str) -> Option<String> {
    if label.is_empty() {
        return Some("a pane needs a name to be addressed. Pick one, e.g. reviewer.".to_string());
    }
    if label == ADMIN {
        return Some(format!(
            "\"{ADMIN}\" is you. A same-user terminal caller outside every watched \
             pane sends as \"{ADMIN}\", so a pane called that would collide with \
             the operator on the record. Pick another name, e.g. lead."
        ));
    }
    if label == EVERYONE {
        return Some(format!(
            "\"{EVERYONE}\" already addresses every agent at once, so a pane \
             called that would answer to a broadcast. Pick another name, e.g. \
             reviewer."
        ));
    }
    if label.starts_with('%') {
        return Some(format!(
            "\"{label}\" looks like a tmux pane id, and cyclops takes those \
             anywhere it takes a name, so this one could mean two panes. Pick a \
             name that does not start with %, e.g. {}.",
            label.trim_start_matches('%')
        ));
    }
    // A control character cannot survive onto a tmux command line, so the
    // border would wear a different name than the record. One name per
    // pane, on every surface.
    if label.chars().any(char::is_control) {
        return Some(format!(
            "\"{}\" has a control character in it, which tmux would strip from \
             the pane border and cyclops would keep on the record. Use letters, \
             digits, dashes and underscores.",
            label.escape_debug()
        ));
    }
    None
}

/// May a pane be called this?
pub fn is_available(label: &str) -> bool {
    refusal(label).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_reserved_names_are_refused() {
        for l in ["admin", "*", "%0", "%12"] {
            assert!(!is_available(l), "{l} should be refused");
        }
    }

    #[test]
    fn ordinary_names_are_available() {
        for l in ["reviewer", "implementer", "build-2", "worker_a", "admin2"] {
            assert!(is_available(l), "{l} should be available");
        }
    }

    /// The point of the rule is a person knowing what to do next, so every
    /// refusal has to name what the string already means AND offer a way
    /// forward. "reserved" alone sends the reader back to the docs.
    #[test]
    fn every_refusal_says_why_and_offers_a_way_out() {
        for l in ["", "admin", "*", "%1", "bad\nname"] {
            let why = refusal(l).expect("refused");
            assert!(
                why.contains("Pick") || why.contains("Use"),
                "{l:?} refusal gives no way forward: {why}"
            );
            assert!(why.ends_with('.'), "{l:?} refusal is not a sentence: {why}");
        }
    }

    #[test]
    fn a_pane_id_refusal_suggests_the_id_without_its_sigil() {
        let why = refusal("%3").expect("refused");
        assert!(why.contains("e.g. 3."), "{why}");
    }
}
