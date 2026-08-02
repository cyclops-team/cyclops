//! Human-facing copy. Errors follow GOALS.md: what happened, why, next
//! step. Sentence case, plain verbs, no protocol jargon, no apologies.

use crate::client::ClientError;

pub const NOT_RUNNING: &str = "cyclops isn't running. Start it with: cyclopsd &";

pub const CONNECT_TIMEOUT: &str =
    "cyclops didn't accept the connection within 2 seconds. The daemon may be wedged; restart cyclopsd and retry.";

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
        ClientError::ConnectTimeout => CONNECT_TIMEOUT.into(),
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
