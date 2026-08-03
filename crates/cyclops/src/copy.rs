//! Human-facing copy. Errors follow GOALS.md: what happened, why, next
//! step. Sentence case, plain verbs, no protocol jargon, no apologies.

use std::time::Duration;

use crate::client::ClientError;

pub const NOT_RUNNING: &str = "cyclops isn't running. Start it with: cyclopsd &";

pub const NO_RECIPIENT: &str =
    "no recipient. Name one (cyclops send reviewer --subject \"...\"), or pass --to or --all.";

/// Empty roster invites the next action, and names the command that fills
/// it. `cyclops status` is the way to find the pane id to hand it.
pub const NO_AGENTS: &str =
    "No agents yet. Name a pane: cyclops name %1 reviewer  (cyclops status lists the panes)";

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

/// Empty history invites the next action. A filtered query names the agent
/// it was scoped to so the suggested send goes somewhere real.
pub fn no_messages(target: Option<&str>) -> String {
    match target {
        Some(t) => format!("No messages with {t} yet. Send one: cyclops send {t} --subject ..."),
        None => "No messages yet. Send one: cyclops send <target> --subject ...".to_string(),
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

pub fn bad_duration(input: &str) -> String {
    format!("can't read \"{input}\" as a duration. Use forms like 90s, 2m, 1m30s, or 500ms.")
}

/// Wait timed out (exit 2). Names what was waited for, how long, the state
/// the target was last seen in, and the next step.
pub fn wait_timeout(target: &str, until: &str, d: Duration, state: Option<&str>) -> String {
    let last = match state {
        Some(s) => format!(" Last state: {s}."),
        None => String::new(),
    };
    format!(
        "{target} didn't reach {until} within {}.{last} Give it more time with --timeout, or look in with cyclops status.",
        timeout_words(d)
    )
}

/// The pinned pane died or changed occupant mid-wait (exit 3).
pub fn wait_occupant_changed(target: &str) -> String {
    format!(
        "{target}'s pane died or changed occupant while waiting, so the wait can't answer for the agent you asked about. Check cyclops status and relabel the pane if a new agent owns it."
    )
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
            ..
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
            data: serde_json::Value::Null,
        };
        assert_eq!(client_error(&e, None), "reviewer declined the message");
        let bare = ClientError::Server {
            code: "denied".into(),
            message: String::new(),
            targets: vec![],
            data: serde_json::Value::Null,
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
    fn empty_history_copy_invites_a_send() {
        assert_eq!(
            no_messages(None),
            "No messages yet. Send one: cyclops send <target> --subject ..."
        );
        assert_eq!(
            no_messages(Some("reviewer")),
            "No messages with reviewer yet. Send one: cyclops send reviewer --subject ..."
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
            data: serde_json::Value::Null,
        };
        assert_eq!(
            client_error(&e, Some("ghost")),
            "no agent or pane called \"ghost\". Cyclops knows: reviewer."
        );
        // Without an asked name there is nothing to blame; daemon copy wins.
        assert_eq!(client_error(&e, None), "server words");
    }

    #[test]
    fn bad_duration_names_the_input_and_the_forms() {
        assert_eq!(
            bad_duration("soon"),
            "can't read \"soon\" as a duration. Use forms like 90s, 2m, 1m30s, or 500ms."
        );
    }

    #[test]
    fn wait_timeout_copy_names_state_and_next_step() {
        assert_eq!(
            wait_timeout("reviewer", "done", Duration::from_secs(60), Some("working")),
            "reviewer didn't reach done within 60 seconds. Last state: working. Give it more time with --timeout, or look in with cyclops status."
        );
        assert_eq!(
            wait_timeout("reviewer", "idle", Duration::from_secs(5), None),
            "reviewer didn't reach idle within 5 seconds. Give it more time with --timeout, or look in with cyclops status."
        );
    }

    #[test]
    fn occupant_changed_copy_is_exact() {
        assert_eq!(
            wait_occupant_changed("reviewer"),
            "reviewer's pane died or changed occupant while waiting, so the wait can't answer for the agent you asked about. Check cyclops status and relabel the pane if a new agent owns it."
        );
    }
}
