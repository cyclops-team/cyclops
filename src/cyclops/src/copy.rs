//! Human-facing copy. Errors follow GOALS.md: what happened, why, next
//! step. Sentence case, plain verbs, no protocol jargon, no apologies.

use std::time::Duration;

use crate::client::ClientError;

/// A down daemon, and the one command that fixes it.
///
/// Re-exported rather than written here: the stream carries the same
/// sentence, and cyclops_proto is the one place both can read it from.
pub use cyclops_proto::NOT_RUNNING;

/// `cyclops daemon status` with nothing to report on.
pub const DAEMON_DOWN: &str = "○ cyclopsd is not running · start it with: cyclops start";

pub const NO_RECIPIENT: &str =
    "no recipient. Name one (cyclops send reviewer --subject \"...\"), or pass --to or --all.";

/// Empty roster invites the next action, and names the command that fills
/// it. `cyclops status` is the way to find the pane id to hand it.
pub const NO_AGENTS: &str =
    "No agents yet. Name a pane: cyclops name %1 reviewer  (cyclops status lists the panes)";

/// Why a pane reads `? unknown`, and the next step.
///
/// The label alone has never been enough, because two different problems
/// wear it and their fixes are nothing alike. cyclopsd holding no manifests
/// at all is a broken install: nothing on the machine can be detected or
/// delivered to, and it is fixed once for every pane. A full manifest set
/// that binds nothing in one pane is one CLI cyclops has not been taught,
/// and it is fixed for that pane. A delivery to an unknown pane ends in
/// attention_required either way, which is why this is said before a
/// message is sent rather than half a minute after.
///
/// `loaded` is the daemon's manifest ids: None from a daemon too old to
/// say, which is not the same as an empty set and must not be reported as
/// one.
pub fn unknown_panes(
    n: usize,
    pane_id: &str,
    label: Option<&str>,
    loaded: Option<&[String]>,
) -> String {
    let head = if n == 1 {
        "1 pane reads unknown".to_string()
    } else {
        format!("{n} panes read unknown")
    };
    let (why, next) = match loaded {
        Some([]) => (
            "cyclopsd loaded no detection manifests.".to_string(),
            "Install them and restart: cyclops start, then restart cyclopsd.".to_string(),
        ),
        Some(ids) => (
            format!("none of {} matches what is running there.", ids.join(", ")),
            pin_a_manifest(pane_id, label),
        ),
        None => (
            "no manifest matches what is running there.".to_string(),
            pin_a_manifest(pane_id, label),
        ),
    };
    format!("{head}: {why} Nothing can be delivered to an unknown pane. {next}")
}

/// The command that teaches cyclops what runs in a pane.
///
/// One home, because three surfaces print it: the status grid explaining
/// an unknown pane, `cyclops name` warning about the pane it just named,
/// and a send refused for that pane. The sentences around it differ
/// because they answer different questions; the command may not, or a
/// reader who has now seen it twice has to compare them word by word.
///
/// The pane is the target and the label is the name it already answers to.
/// Passing the label as the target would rename an adopted pane to a
/// placeholder, which is the one way to get this command wrong.
fn pin_command(pane: &str, label: &str) -> String {
    format!("cyclops name {pane} {label} --manifest <id>")
}

/// The next step for a pane no manifest binds: pin one, or write one.
/// `label` is the name the pane already answers to, so the command can be
/// pasted whole instead of renaming the pane to a placeholder.
fn pin_a_manifest(pane: &str, label: Option<&str>) -> String {
    format!(
        "Pin one: {}. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md.",
        pin_command(pane, label.unwrap_or("<label>"))
    )
}

/// Said right after `cyclops name` when nothing binds the pane it just
/// named. The name went on the roster and the border, and no message can
/// reach it, which the receipt would otherwise report half a minute later.
pub fn named_but_undetected(pane: &str, label: &str) -> String {
    format!(
        "nothing detects {pane} yet, so {label} can't receive a message. cyclops status names the manifests that are loaded; pin one with: {}",
        pin_command(pane, label)
    )
}

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

/// The gate cause a receipt carries when nothing detects the pane. It is a
/// protocol token, not prose: the words a reader sees are
/// `cyclops_ui::grid::cause_words`'s, and this is only how the CLI knows
/// which follow-up to print under the badge.
pub const CAUSE_NO_MANIFEST: &str = "no_manifest";

/// Follow-up for a receipt that needs a human, said after the badge.
///
/// The badge names the state and the reason; a reader still has to be told
/// what became of the message, because "⚠ needs attention" on its own does
/// not say whether it was delivered. It was not. It is on the record, and
/// `cyclops status` is where every waiting item is listed with what to do
/// about it.
///
/// `pane` is set on the one cause whose fix is a command: a pane no
/// manifest binds. It carries the pin, with the name the pane already
/// answers to in it, so the line can be pasted whole. Passing the label as
/// the target would rename an adopted pane to a placeholder, which is why
/// `cyclops name` takes both and why both are here.
pub fn needs_attention(to: &str, pane: Option<&str>) -> String {
    match pane {
        Some(pane) => format!(
            "{to} did not get this message; it is on the record and needs attention. Teach cyclops what runs in {pane}: {}. cyclops status names the manifests that are loaded, and docs/reference/MANIFESTS.md is how to write one.",
            pin_command(pane, to)
        ),
        None => format!(
            "{to} did not get this message. It is on the record and needs attention; cyclops status lists what is waiting on you and what to do about each one."
        ),
    }
}

/// Follow-up for a receipt taken while the delivery is still in flight.
///
/// The payload is in the pane and cyclops is waiting on the evidence that
/// the recipient took it. That is a real state and it gets its own word,
/// so the sender is not told the message is queued when the pane already
/// has it. What is missing is the confirmation, and it lands on the record
/// with or without a reader, which is what this points at.
pub fn in_flight(to: &str) -> String {
    format!(
        "the message is in {to}'s pane; cyclops is still waiting for the confirmation. It lands on the record either way: cyclops history shows the badge."
    )
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

/// Said after a switch no daemon confirmed. The config is written either
/// way, so this is a "when", not a "but".
pub const THEME_NEXT_COMMAND: &str = "the next command picks it up";

/// Said after a switch a running daemon did NOT take. Not a "when": the
/// config is written and every one-shot command is already on the new
/// theme, but the pane borders and any open `cyclops ui` are cyclopsd's to
/// repaint and it is painting something else. Nothing that happens on this
/// side moves them.
///
/// Two ways the daemon lands somewhere else, and the next step covers
/// both. `CYCLOPS_THEME` in its environment beats the config key and is
/// fixed for the life of the process, and a bare theme name resolves
/// against `./themes` relative to the daemon's working directory when
/// `$CYCLOPS_HOME/themes` does not exist, which need not be the directory
/// this command just read.
pub fn theme_not_live(painting: Option<&str>) -> String {
    match painting {
        Some(now) => format!(
            "cyclopsd is still painting {now}, so pane borders did not change. Check CYCLOPS_THEME and the themes directory where cyclopsd runs, then restart it."
        ),
        None => "cyclopsd didn't take the switch, so pane borders did not change. Restart cyclopsd to pick it up.".to_string(),
    }
}

/// A theme file that parses but sets no token in the vocabulary.
///
/// Refused for the same reason a file that will not parse is refused, and
/// it is the reason the listing leaves both out: every token would resolve
/// off the compiled default table, so the switch would report a change
/// that no surface can show.
pub fn theme_sets_no_colors(name: &str, path: &std::path::Path) -> String {
    format!(
        "can't use theme \"{name}\": {} sets no colors, so switching to it would change nothing on screen. Nothing was changed. Pick another with cyclops theme.",
        path.display()
    )
}

/// No themes directory anywhere. Names both places one is looked for, and
/// the colors that render until one exists.
pub fn no_themes(home: &std::path::Path) -> String {
    format!(
        "No themes found. Cyclops looks in {} and ./themes, and renders in built-in colors until one of them has a .toml in it.",
        home.join("themes").display()
    )
}

/// The active theme is not in the listing, which happens when it was
/// chosen by path or by CYCLOPS_THEME. The listing marks nothing, so this
/// says what is actually on.
pub fn active_elsewhere(sel: &cyclops_theme::Selection) -> String {
    match &sel.path {
        Some(p) => format!("on now: {} ({})", sel.theme.name(), p.display()),
        None => format!("on now: {} (built-in colors)", sel.theme.name()),
    }
}

/// A named theme that will not load. The key is not written: a config
/// pointing at this file would render built-in colors and say why only at
/// the next command.
///
/// `cause` is a `cyclops_theme::ThemeError`, which names the file itself,
/// so nothing here repeats the path.
pub fn theme_unusable(name: &str, cause: &str) -> String {
    format!("can't use theme \"{name}\": {cause}. Nothing was changed. Pick another with cyclops theme.")
}

/// The config could not be written, so nothing switched.
pub fn theme_not_saved(path: &std::path::Path, cause: &str) -> String {
    format!(
        "can't save the theme: {cause}. Nothing was changed. Check that you can write {}.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three causes, three sentences, and the difference between them is
    /// the whole point: an empty manifest set is fixed once for the
    /// machine, a set that binds nothing in one pane is fixed for that
    /// pane, and a daemon too old to say must be reported as neither.
    #[test]
    fn unknown_pane_copy_names_the_cause_and_the_fix() {
        let ids = vec!["agy".to_string(), "claude".to_string()];
        assert_eq!(
            unknown_panes(1, "%4", None, Some(&[])),
            "1 pane reads unknown: cyclopsd loaded no detection manifests. Nothing can be delivered to an unknown pane. Install them and restart: cyclops start, then restart cyclopsd."
        );
        assert_eq!(
            unknown_panes(1, "%4", None, Some(&ids)),
            "1 pane reads unknown: none of agy, claude matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %4 <label> --manifest <id>. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md."
        );
        // A pane that already answers to a name keeps it, so the command
        // is one a reader can paste instead of edit.
        assert_eq!(
            unknown_panes(2, "%4", Some("reviewer"), None),
            "2 panes read unknown: no manifest matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %4 reviewer --manifest <id>. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md."
        );
    }

    /// Three surfaces, one command, byte for byte. A reader who has seen
    /// it on the status grid must not have to compare it with the one a
    /// refused send prints.
    #[test]
    fn every_surface_prints_the_same_pin_command() {
        let want = "cyclops name %4 reviewer --manifest <id>";
        for said in [
            unknown_panes(1, "%4", Some("reviewer"), None),
            named_but_undetected("%4", "reviewer"),
            needs_attention("reviewer", Some("%4")),
        ] {
            assert!(said.contains(want), "{said}");
        }
    }

    #[test]
    fn a_named_pane_nothing_detects_says_it_cannot_receive_yet() {
        assert_eq!(
            named_but_undetected("%0", "implementer"),
            "nothing detects %0 yet, so implementer can't receive a message. cyclops status names the manifests that are loaded; pin one with: cyclops name %0 implementer --manifest <id>"
        );
    }

    /// The sentence is cyclops_proto's, and the stream prints the same
    /// one. What this pins is the property that survives a rewording: it
    /// names a command, and that command is one that exists and starts a
    /// daemon. It said `cyclopsd &` until `cyclops start` took the job
    /// over, and a frozen literal here would have had to be edited in
    /// two crates on the same day to stay true in one.
    #[test]
    fn not_running_names_the_command_that_fixes_it() {
        assert_eq!(NOT_RUNNING, cyclops_proto::NOT_RUNNING);
        assert!(NOT_RUNNING.contains("cyclops start"), "{NOT_RUNNING}");
        assert!(
            !NOT_RUNNING.contains("cyclopsd &"),
            "the daemon is not started by hand any more: {NOT_RUNNING}"
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

    /// A badge that says a human is needed still has to say the message
    /// did not arrive, and the one cause a command fixes has to carry the
    /// command. Pasteable means both ids in it: the pane, and the name the
    /// pane already answers to.
    #[test]
    fn attention_copy_says_the_message_did_not_arrive_and_carries_the_fix() {
        assert_eq!(
            needs_attention("worker", Some("%1")),
            "worker did not get this message; it is on the record and needs attention. Teach cyclops what runs in %1: cyclops name %1 worker --manifest <id>. cyclops status names the manifests that are loaded, and docs/reference/MANIFESTS.md is how to write one."
        );
        // Every other cause: no pin command, because a manifest does not
        // fix a dead pane or a name nobody answers to.
        assert_eq!(
            needs_attention("ghost", None),
            "ghost did not get this message. It is on the record and needs attention; cyclops status lists what is waiting on you and what to do about each one."
        );
    }

    #[test]
    fn in_flight_copy_names_what_is_missing_and_where_to_look() {
        assert_eq!(
            in_flight("worker"),
            "the message is in worker's pane; cyclops is still waiting for the confirmation. It lands on the record either way: cyclops history shows the badge."
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
