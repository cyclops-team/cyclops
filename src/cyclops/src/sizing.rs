//! The operator's way out of Cyclops window sizing.
//!
//! A workspace pins the windows it sizes to `window-size manual` and puts
//! them back when it quits. A workspace that is killed hard does not get to
//! put anything back, and while any later workspace repairs that (the
//! record of what each window was lives in the tmux server, not in the
//! workspace), an operator who is finished with Cyclops has no workspace
//! coming. This is the command for that case, so the answer is never "know
//! these two option names and unset them yourself".
//!
//! It refuses more often than it acts, and that is the design. It refuses
//! while the session's owner is still running, because recovery is for a
//! workspace that is gone. It refuses on a record it cannot read, because
//! the original policy is then unknowable and guessing one would invent
//! state the operator never set. A refusal changes nothing at all and says
//! exactly what to do by hand.

use crate::style::Style;
use cyclops_tmux::{layout::Server, ReleaseOutcome, Restored};

/// Exit code for a refusal: nothing was read, nothing was written, and the
/// operator has something to decide.
const EXIT_REFUSED: i32 = 3;

/// Resolve the session an operator asked to release.
///
/// An explicit server cannot safely inherit the shell's session name: that
/// shell may be attached to a different tmux server. The caller must name the
/// recovery target in that case.
pub(crate) fn resolve_session(
    requested: Option<String>,
    server: &Server,
) -> Result<String, String> {
    if let Some(session) = requested {
        return Ok(session);
    }
    if server.socket.is_some() {
        return Err(
            "Cyclops is configured for a named tmux server; pass --session <name> so recovery does not use this shell's different server"
                .to_string(),
        );
    }
    cyclops_tmux::current_session(None)
        .ok_or_else(|| "not inside tmux: name the session with --session <name>".to_string())
}

/// Undo Cyclops sizing on `session`, printing what it did per window.
///
/// The server comes from the same `tmux_socket` the rest of the client
/// reads, so recovery lands on the server the daemon watches rather than on
/// whichever one tmux would pick by default.
pub fn run_release(server: &Server, session: &str, json: bool, style: &Style) -> i32 {
    let outcome = match cyclops_tmux::release_session_sizing(session, server.socket.as_deref()) {
        Ok(outcome) => outcome,
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "session": session, "error": error.to_string()})
                );
            } else {
                eprintln!("{}", style.bold(&format!("{session}: {error}")));
            }
            return 1;
        }
    };

    let released = match outcome {
        ReleaseOutcome::RefusedLiveOwner { marker } => {
            return refuse_live_owner(session, &marker, json, style)
        }
        ReleaseOutcome::Released(released) => released,
    };

    let restored = released
        .iter()
        .filter(|w| w.outcome == Restored::Exactly)
        .count();
    let malformed: Vec<&str> = released
        .iter()
        .filter(|w| w.outcome == Restored::Malformed)
        .map(|w| w.window_id.as_str())
        .collect();
    let untouched = released.len() - restored - malformed.len();
    let refused = !malformed.is_empty();

    if json {
        println!(
            "{}",
            serde_json::json!({
                // False on a refusal: some windows are still pinned, the
                // records are still there, and the session is still owned.
                "ok": !refused,
                "released": !refused,
                "refused": refused.then_some("unreadable_record"),
                "session": session,
                "restored": restored,
                "malformed": malformed,
                "untouched": untouched,
                "windows": released
                    .iter()
                    .map(|w| serde_json::json!({
                        "window": w.window_id,
                        "outcome": match w.outcome {
                            Restored::Exactly => "restored",
                            Restored::Malformed => "malformed",
                            Restored::Nothing => "untouched",
                        }
                    }))
                    .collect::<Vec<_>>(),
            })
        );
        return if refused { EXIT_REFUSED } else { 0 };
    }

    if refused {
        // The headline says refused, before any count. A partial release
        // that announced itself as a release is how an operator walks away
        // from a session that is still pinned and still owned.
        eprintln!(
            "{}",
            style.bold(&format!(
                "{session}: refused. {} window(s) carry a record cyclops cannot read, so nothing about them was changed",
                malformed.len()
            ))
        );
    } else {
        println!(
            "{}",
            style.accent(&format!("{session}: cyclops sizing released"))
        );
    }
    println!("  {restored} window(s) put back on their original policy");
    if untouched > 0 {
        println!("  {untouched} window(s) cyclops never sized, left alone");
    }
    if !refused {
        return 0;
    }
    for window in &malformed {
        eprintln!("  {window} is still on manual and still owned. Read its record with:");
        eprintln!("    tmux show-options -w -t {window} @cyclops_prior_window_size");
        eprintln!("  then set window-size yourself and clear the record with:");
        eprintln!("    tmux set-option -w -t {window} window-size <policy>");
        eprintln!("    tmux set-option -w -t {window} -u @cyclops_prior_window_size");
    }
    eprintln!("  finally, release the session:");
    eprintln!("    tmux set-option -t {session} -u @cyclops_window_driver");
    EXIT_REFUSED
}

/// Refuse while the owner is still running, and say who it is.
fn refuse_live_owner(session: &str, marker: &str, json: bool, style: &Style) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": false,
                "released": false,
                "refused": "live_owner",
                "session": session,
                "owner": marker,
            })
        );
        return EXIT_REFUSED;
    }
    eprintln!(
        "{}",
        style.bold(&format!(
            "{session}: refused. A running workspace still owns this session's sizing"
        ))
    );
    eprintln!("  owner: {marker}");
    eprintln!("  Nothing was changed. That workspace puts these windows back when it quits.");
    eprintln!("  Quit it, then run this again.");
    EXIT_REFUSED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_server_requires_an_explicit_recovery_session() {
        let server = Server {
            socket: Some("cyclops-test".to_string()),
            config_file: None,
        };

        let error = resolve_session(None, &server).expect_err("named servers need a named session");
        assert!(error.contains("--session <name>"));
    }

    #[test]
    fn an_explicit_session_is_valid_for_a_configured_server() {
        let server = Server {
            socket: Some("cyclops-test".to_string()),
            config_file: None,
        };

        assert_eq!(
            resolve_session(Some("recover-me".to_string()), &server),
            Ok("recover-me".to_string())
        );
    }
}
