//! Quoting for tmux command arguments.
//!
//! Control mode is line based: one command per line, tokenized with
//! shell-like quoting. Everything the daemon interpolates into a command
//! (pane ids, session names, buffer names, literal keys, format strings) is
//! untrusted and goes through [`quote_arg`]. There is exactly one quoting
//! rule in the codebase; do not hand-roll another.

/// Wrap one argument in single quotes, escaping embedded single quotes with
/// the `'\''` close-escape-reopen idiom tmux's parser understands.
///
/// Newline, carriage return, and NUL cannot be represented on a control-mode
/// command line at all, so they are stripped rather than escaped. Payloads
/// that need those bytes travel through `load-buffer`, which reads from a
/// file and has no quoting problem.
pub fn quote_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\n' | '\r' | '\0' => {}
            '\'' => out.push_str("'\\''"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_empty() {
        assert_eq!(quote_arg("hello"), "'hello'");
        assert_eq!(quote_arg(""), "''");
        assert_eq!(quote_arg("%3"), "'%3'");
    }

    #[test]
    fn embedded_single_quote() {
        assert_eq!(quote_arg("it's"), "'it'\\''s'");
        assert_eq!(quote_arg("''"), "''\\'''\\'''");
    }

    #[test]
    fn spaces_semicolons_and_format_braces_are_inert() {
        assert_eq!(quote_arg("a b; kill-server"), "'a b; kill-server'");
        assert_eq!(quote_arg("#{pane_title}"), "'#{pane_title}'");
    }

    #[test]
    fn line_breakers_are_stripped() {
        assert_eq!(quote_arg("a\nb\rc\0d"), "'abcd'");
    }
}
