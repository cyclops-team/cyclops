//! Human-facing copy. Errors follow GOALS.md: what happened, why, next
//! step. Sentence case, plain verbs, no protocol jargon, no apologies.

use std::time::Duration;

use crate::client::ClientError;

pub const NOT_RUNNING: &str = "cyclops isn't running. Start it with: cyclopsd &";

pub const NO_RECIPIENT: &str =
    "no recipient. Name one (cyclops send reviewer --subject \"...\"), or pass --to or --all.";

/// Humane duration for timeout copy: whole seconds as words, else ms.
pub fn timeout_words(d: Duration) -> String {
    let ms = d.as_millis();
    if ms >= 1000 && ms.is_multiple_of(1000) {
        let s = ms / 1000;
        if s == 1 {
            "1 second".into()
        } else {
            format!("{s} seconds")
        }
    } else {
        format!("{ms}ms")
    }
}

pub fn connect_timeout(d: Duration) -> String {
    format!(
        "cyclops didn't accept the connection within {}. The daemon may be wedged; restart cyclopsd and retry.",
        timeout_words(d)
    )
}

/// Follow-up for a parked receipt. Quota parks are terminal until an
/// operator re-queues (never auto-retried), so the next step says so and
/// carries the reset hint from the receipt note.
pub fn parked(to: &str, note: Option<&str>) -> String {
    match note {
        Some(n) => format!(
            "{to} is out of quota, {n}. The message is kept as parked; requeue it once the quota resets."
        ),
        None => format!(
            "{to} is out of quota. The message is kept as parked; requeue it once the quota resets."
        ),
    }
}

pub fn body_file_unreadable(path: &str, cause: &str) -> String {
    let src = if path == "-" {
        "stdin".to_string()
    } else {
        format!("\"{path}\"")
    };
    format!("can't read the message body from {src}: {cause}. Check the file and resend.")
}

pub const UNREADABLE_ANSWER: &str =
    "cyclops answered in a shape this client doesn't understand. The daemon and CLI are probably far apart in version; update the older one.";

pub fn broken(cause: &str) -> String {
    format!("lost the connection to cyclops: {cause}. Check that cyclopsd is still running, then retry.")
}

pub fn unknown_target(asked: &str, known: &[String]) -> String {
    if known.is_empty() {
        format!("no agent or pane called \"{asked}\". Run cyclops status to see what cyclops is watching.")
    } else {
        format!(
            "no agent or pane called \"{asked}\". Cyclops knows: {}.",
            known.join(", ")
        )
    }
}

pub fn proto_mismatch(server: u32, client: u32) -> String {
    format!("note: cyclopsd speaks protocol {server}, this cyclops speaks {client}. Continuing; update the older side.")
}

/// One place turns transport errors into copy. `asked` names the target the
/// user typed, when the command had one.
pub fn client_error(e: &ClientError, asked: Option<&str>) -> String {
    match e {
        ClientError::NotRunning => NOT_RUNNING.into(),
        ClientError::ConnectTimeout(d) => connect_timeout(*d),
        ClientError::Server {
            code,
            message,
            targets,
        } => {
            if code == "no_such_target" {
                if let Some(asked) = asked {
                    return unknown_target(asked, targets);
                }
            }
            // The daemon owns its own error copy; pass it through.
            if message.is_empty() {
                format!("cyclops refused: {code}")
            } else {
                message.clone()
            }
        }
        ClientError::Broken(cause) => broken(cause),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_running_copy_is_exact() {
        assert_eq!(
            NOT_RUNNING,
            "cyclops isn't running. Start it with: cyclopsd &"
        );
    }

    #[test]
    fn unknown_target_names_ask_and_lists_known() {
        assert_eq!(
            unknown_target("ghost", &["reviewer".into(), "implementer".into()]),
            "no agent or pane called \"ghost\". Cyclops knows: reviewer, implementer."
        );
        assert_eq!(
            unknown_target("ghost", &[]),
            "no agent or pane called \"ghost\". Run cyclops status to see what cyclops is watching."
        );
    }

    #[test]
    fn proto_mismatch_names_both_sides() {
        assert_eq!(
            proto_mismatch(2, 1),
            "note: cyclopsd speaks protocol 2, this cyclops speaks 1. Continuing; update the older side."
        );
    }

    #[test]
    fn server_error_falls_back_to_daemon_message() {
        let e = ClientError::Server {
            code: "denied".into(),
            message: "reviewer declined the message".into(),
            targets: vec![],
        };
        assert_eq!(client_error(&e, None), "reviewer declined the message");
        let bare = ClientError::Server {
            code: "denied".into(),
            message: String::new(),
            targets: vec![],
        };
        assert_eq!(client_error(&bare, None), "cyclops refused: denied");
    }

    #[test]
    fn timeout_words_cover_seconds_and_millis() {
        assert_eq!(timeout_words(Duration::from_secs(5)), "5 seconds");
        assert_eq!(timeout_words(Duration::from_secs(1)), "1 second");
        assert_eq!(timeout_words(Duration::from_millis(500)), "500ms");
        assert_eq!(
            connect_timeout(Duration::from_secs(2)),
            "cyclops didn't accept the connection within 2 seconds. The daemon may be wedged; restart cyclopsd and retry."
        );
    }

    #[test]
    fn parked_copy_carries_the_reset_hint_and_next_step() {
        assert_eq!(
            parked("reviewer", Some("resets in 135h")),
            "reviewer is out of quota, resets in 135h. The message is kept as parked; requeue it once the quota resets."
        );
        assert_eq!(
            parked("reviewer", None),
            "reviewer is out of quota. The message is kept as parked; requeue it once the quota resets."
        );
    }

    #[test]
    fn body_file_copy_names_the_source() {
        assert_eq!(
            body_file_unreadable("notes.md", "No such file or directory (os error 2)"),
            "can't read the message body from \"notes.md\": No such file or directory (os error 2). Check the file and resend."
        );
        assert!(
            body_file_unreadable("-", "x").starts_with("can't read the message body from stdin: x")
        );
    }

    #[test]
    fn unknown_target_route_needs_the_asked_name() {
        let e = ClientError::Server {
            code: "no_such_target".into(),
            message: "server words".into(),
            targets: vec!["reviewer".into()],
        };
        assert_eq!(
            client_error(&e, Some("ghost")),
            "no agent or pane called \"ghost\". Cyclops knows: reviewer."
        );
        // Without an asked name there is nothing to blame; daemon copy wins.
        assert_eq!(client_error(&e, None), "server words");
    }
}
