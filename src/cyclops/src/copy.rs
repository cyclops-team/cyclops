//! Human-facing copy. Errors follow GOALS.md: what happened, why, next
//! step. Sentence case, plain verbs, no protocol jargon, no apologies.

use std::fmt::Write as _;
use std::time::Duration;

use crate::client::ClientError;

/// A down daemon, and the one command that fixes it.
///
/// Re-exported rather than written here: the stream carries the same
/// sentence, and cyclops_proto is the one place both can read it from.
pub use cyclops_proto::NOT_RUNNING;

/// `cyclops daemon status` with nothing to report on.
pub const DAEMON_DOWN: &str = "○ cyclopsd is not running · start it with: cyclops start";

#[cfg(not(feature = "full-ui"))]
pub const WORKSPACE_NOT_INCLUDED: &str = "the full-screen workspace is not included in this build. Run cyclops --help for headless commands, or install a full Cyclops build.";

#[cfg(not(feature = "full-ui"))]
pub const WATCH_NOT_INCLUDED: &str = "interactive watch is not included in this build. Use cyclops watch --json for the headless event stream, or install a full Cyclops build.";

/// Said when the running daemon is too old to answer the restart
/// handshake. Retrying this verb can only fail the same way, so the copy
/// names the pair of commands that do cross, once.
pub const RESTART_PREDATES_FIX: &str =
    "Stop and start it once by hand: cyclops daemon stop, then cyclops start.";

pub const NO_RECIPIENT: &str =
    "no recipient. Name one (cyclops send reviewer --subject \"...\" --summary \"First sentence. Second sentence.\"), or pass --to or --all.";

pub const ATTENTION_DIFF_UNAVAILABLE: &str =
    "diff unavailable: exact visible composer extraction failed";

pub const ALARM_CLEAR_JSON_REQUIRES_CONFIRMATION: &str = "alarm clear --older-than requires interactive confirmation; use alarm preview --json, then alarm clear with its exact ids";

pub const ALARM_CLEAR_TERMINAL_REQUIRED: &str = "alarm clear --older-than requires an interactive terminal; use alarm preview, then alarm clear with its exact ids";

pub const ALARM_CLEARANCE_CANCELLED: &str = "alarm clearance cancelled";

pub const SETUP_HOME_UNAVAILABLE: &str = "HOME is not set, so setup paths cannot be inspected";

/// Explain why a notification cannot fit without changing its recorded cause.
pub fn pane_too_narrow(observed: u32, required: u32) -> String {
    format!("pane too narrow ({observed}, requires {required})")
}

pub fn alarm_clear_confirmation(count: usize, older_than: &str) -> String {
    format!("Clear {count} alarms selected by --older-than {older_than}? Type clear to confirm: ")
}

/// What `alarm clear` did and did not do. Clearance acknowledges an alarm;
/// it retires nothing. The attempt keeps its state, the message keeps its
/// place, and a pending head keeps holding its recipient's queue.
pub fn alarm_cleared_consequence(
    id: &str,
    message_id: &str,
    recipient: &str,
    state: &str,
    cause: &str,
) -> String {
    format!(
        "  acknowledged only · at clearance, attempt {id} was {state} ({cause}) · clearance did not change message {message_id} to {recipient}; while pending, it can hold that recipient's queue · next: recipient retrieves the durable payload with cyclops inbox claim {message_id} · admin may inspect current state with cyclops attention show {id} --diff, then complete or discard when its checks authorize the action · neither clearance nor payload retrieval alone proves a post-write composer barrier retired"
    )
}

/// Mixed-version fallback when an older daemon returns cleared ids without
/// the additive locked summaries. The command must still state that alarm
/// clearance does not resolve the notification or message.
pub fn alarm_cleared_without_summary(id: &str) -> String {
    format!(
        "  acknowledged only · the daemon did not return the locked summary for attempt {id} · inspect current state with cyclops attention show {id} --diff, or update and restart the matched Cyclops pair"
    )
}

/// A claim by id took a later message; the oldest pending one still holds
/// this recipient's queue head and its wake.
pub fn claim_skipped_oldest(oldest: &str) -> String {
    format!("skipped oldest pending {oldest} · it still holds your queue head · claim it, or use inbox next for oldest-first")
}

pub fn no_unresolved_alarms(older_than: &str) -> String {
    format!("no unresolved alarms selected by --older-than {older_than}")
}

pub fn alarm_clear_confirmation_unreadable(error: &std::io::Error) -> String {
    format!("could not read alarm clearance confirmation: {error}")
}

pub fn attention_resolution_verb(
    resolution: cyclops_proto::NotificationResolution,
) -> &'static str {
    match resolution {
        cyclops_proto::NotificationResolution::Complete => "submitted",
        cyclops_proto::NotificationResolution::Discard => "discarded",
    }
}

pub fn attention_check_rows(checks: &cyclops_proto::AttentionChecks) -> [(&'static str, bool); 5] {
    [
        ("notification exact", checks.notification_exact),
        ("trailer anchored", checks.trailer_anchored),
        ("process binding matches", checks.process_matches),
        ("manifest matches", checks.manifest_matches),
        ("terminal action safe", checks.terminal_action_safe),
    ]
}

pub fn attention_check_value(passed: bool) -> &'static str {
    if passed {
        "yes"
    } else {
        "no"
    }
}

pub fn attention_action_uncertain(
    resolution: cyclops_proto::NotificationResolution,
    attempt_id: cyclops_proto::NotificationAttemptId,
) -> String {
    let (action, command) = match resolution {
        cyclops_proto::NotificationResolution::Complete => ("submit", "complete"),
        cyclops_proto::NotificationResolution::Discard => ("discard", "discard"),
    };
    format!(
        "{action} action outcome uncertain; safe reconcile: cyclops attention {command} {attempt_id}; this rechecks without sending a second key"
    )
}

/// Empty roster invites the next action, and names the command that fills
/// it. `cyclops status` is the way to find the pane id to hand it.
pub const NO_AGENTS: &str =
    "No agents yet. Name a pane: cyclops name %1 reviewer  (cyclops status lists the panes)";

/// The complete command map behind the everyday top-level help. The
/// descriptions come from clap's command help, so discovery and detailed
/// help cannot quietly describe the same spelling two different ways.
pub const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    (
        "Everyday",
        &[
            "send", "inbox", "reply", "clear", "status", "health", "stop",
        ],
    ),
    (
        "Workspace",
        &["start", "workspace", "sizing", "name", "list", "watch"],
    ),
    (
        "Operations",
        &[
            "setup", "hooks", "theme", "daemon", "update", "cleanup", "data", "remove", "reset",
            "flush",
        ],
    ),
    (
        "Diagnosis and compatibility",
        &[
            "ping",
            "read",
            "messages",
            "requeue",
            "notification",
            "alarm",
            "attention",
            "history",
            "thread",
            "wait",
            "ui",
            "hook",
        ],
    ),
];

pub fn command_catalog(mut about: impl FnMut(&str) -> String) -> String {
    let width = COMMAND_GROUPS
        .iter()
        .flat_map(|(_, commands)| commands.iter())
        .map(|command| command.len())
        .max()
        .expect("command catalog is not empty");
    let mut out = String::new();

    for (group_index, (heading, commands)) in COMMAND_GROUPS.iter().enumerate() {
        if group_index > 0 {
            out.push('\n');
        }
        writeln!(out, "{heading}").expect("write command catalog heading");
        for command in *commands {
            let description = about(command);
            let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut first_sentence = description
                .split_once(". ")
                .map(|(sentence, _)| format!("{sentence}."))
                .unwrap_or(description);
            if !first_sentence.ends_with(['.', '?', '!']) {
                first_sentence.push('.');
            }
            writeln!(out, "  {command:width$}  {first_sentence}")
                .expect("write command catalog entry");
        }
    }

    out.push_str("\nRun cyclops <command> --help for details.");
    out
}

pub const EMPTY_INBOX: &str = "No pending messages. Wait for one: cyclops inbox next --timeout 30s";

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
/// not say whether it was delivered. Proven pre-write failures say that it
/// was not; ambiguous after-write failures direct the reader to inspect
/// before resending. Both are on the record, and `cyclops status` is where
/// every waiting item is listed with what to do about it.
///
/// `pane` is set on the one cause whose fix is a command: a pane no
/// manifest binds. It carries the pin, with the name the pane already
/// answers to in it, so the line can be pasted whole. Passing the label as
/// the target would rename an adopted pane to a placeholder, which is why
/// `cyclops name` takes both and why both are here.
/// Follow-up for an unresolved delivery with its exact machine cause. A
/// failure after the irreversible boundary must not be described as a
/// proven non-delivery: the operator inspects the named pane before sending
/// the same logical message again.
pub fn needs_attention_for(to: &str, pane: Option<&str>, cause: Option<&str>) -> String {
    if cause.is_some_and(cyclops_ui::grid::is_after_write_cause) {
        let reason = cause
            .map(cyclops_ui::grid::cause_words)
            .unwrap_or_else(|| "outcome unknown".to_string());
        return match pane {
            Some(pane) => format!(
                "{to}'s delivery {reason}. Inspect {pane} and its composer before resending; it is on the record and needs attention."
            ),
            None => format!(
                "{to}'s delivery {reason}. Inspect the recipient pane before resending; it is on the record and needs attention."
            ),
        };
    }
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
        Some(t) => format!(
            "No messages with {t} yet. Send one: cyclops send {t} --subject ... --summary \"First sentence. Second sentence.\""
        ),
        None => "No messages yet. Send one: cyclops send <target> --subject ... --summary \"First sentence. Second sentence.\"".to_string(),
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
pub const UNKNOWN_WAKE_RECEIPT: &str = "wake receipt state is unknown to this client";

pub fn broken(cause: &str) -> String {
    format!(
        "lost the connection to cyclops: {cause}. The request may already have landed. Check that cyclopsd is running and inspect current state. Only repeat a send or reply with the same explicit --client-key."
    )
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

/// Persistent notice for a daemon that is not the same runtime identity as
/// this CLI. Classification stays in `cyclops-client`; this module owns the
/// command and recovery wording shown to a person.
pub fn hello_compatibility_notice(
    compatibility: &cyclops_client::HelloCompatibility,
) -> Option<String> {
    use cyclops_client::HelloCompatibility;

    match compatibility {
        HelloCompatibility::Current { .. } => None,
        HelloCompatibility::Mismatch { client, daemon } => Some(format!(
            "version/build mismatch: cyclops {}, cyclopsd {}. Continuing; run cyclops daemon restart. If they still differ, update or reinstall the older side.",
            client.description(),
            daemon.description()
        )),
        HelloCompatibility::UnverifiedDaemon { client, daemon } => Some(format!(
            "daemon identity unverified: cyclops {}, cyclopsd {}. Continuing; run cyclops daemon restart. If it remains unverified, update or reinstall the daemon.",
            client.description(),
            daemon.description()
        )),
    }
}

pub fn bad_duration(input: &str) -> String {
    format!("can't read \"{input}\" as a duration. Use forms like 90s, 2m, 1m30s, or 500ms.")
}

pub fn inbox_next_timeout(d: Duration) -> String {
    format!(
        "no pending message arrived within {}. Increase --timeout or inspect the queue with cyclops inbox list.",
        timeout_words(d)
    )
}

pub fn inbox_claim_outcome_unknown(message_id: &str) -> String {
    format!(
        "cyclops sent the claim for {message_id}, but no usable answer arrived. The message may already be claimed. Inspect it with cyclops thread {message_id} or cyclops inbox list before retrying."
    )
}

pub const INBOX_SENDER_FILTER_UNAVAILABLE: &str = "the daemon did not prove the sender endpoint on its inbox answer, so cyclops refused to claim a possibly different message. Update cyclopsd or remove --from.";

pub const WATCH_JSON_FILTER_UNSUPPORTED: &str = "--from, --to, and --with filter the interactive TUI and are not available with --json. Use --kinds for the event stream or cyclops inbox next --from <recipient-key> for bounded receive automation.";

#[cfg(feature = "full-ui")]
pub fn unknown_watch_filters(asked: &[&str]) -> String {
    let names = asked
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let label = if asked.len() == 1 {
        "unknown active display label"
    } else {
        "unknown active display labels"
    };
    format!(
        "{label} {names}. Run cyclops list --all to discover active labels. Watch resolves each label once to its durable endpoint, so renaming it later does not retarget or strand the filter. For automation, use cyclops inbox next --timeout 30s."
    )
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
        ClientError::NotRunning(_) => NOT_RUNNING.into(),
        ClientError::ConnectTimeout(d) => connect_timeout(*d),
        ClientError::HelloTimeout(d) => broken(&format!("no answer within {}", timeout_words(*d))),
        ClientError::ReadTimeout(d) => broken(&format!("no answer within {}", timeout_words(*d))),
        ClientError::RequestFrameTooLarge => {
            format!("{}. Nothing was sent.", frame_too_large("request"))
        }
        ClientError::DaemonFrameTooLarge => broken(&frame_too_large("daemon frame")),
        // The daemon deliberately supplied this complete sentence because the
        // real response would not fit. Preserve its honest uncertainty copy.
        ClientError::OversizedResponse(message) => message.clone(),
        ClientError::InvalidHello(_) => broken("the hello line didn't parse"),
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
        ClientError::NotSent(cause) | ClientError::Unknown(cause) | ClientError::Gap(cause) => {
            broken(cause)
        }
    }
}

pub fn frame_too_large(subject: &str) -> String {
    format!(
        "{subject} exceeds the {}-byte JSON frame limit (newline excluded)",
        cyclops_proto::FrameContract::MAX_JSON_BYTES
    )
}

/// Under a scoped roster's header: the watched sessions the caller is
/// not in, and the way to see them. Scoping without this line would be
/// the old "whose roster is this" defect in a new place: sessions
/// quietly missing instead of sessions quietly mixed in.
pub fn also_watching(sessions: &[String]) -> String {
    format!(
        "also watching {} · cyclops list --all to see every session",
        sessions.join(", ")
    )
}

/// `cyclops ui` refuses --json: the machine stream lives on `watch`, and
/// pointing there beats emitting a shape nothing should rely on.
#[cfg(feature = "full-ui")]
pub const UI_NO_JSON: &str =
    "cyclops ui has no --json form. The machine stream is: cyclops watch --json";

/// Said on stderr every `cyclops ui` run, so scripts keep working while
/// their authors learn the verb that replaced it.
#[cfg(feature = "full-ui")]
pub const UI_DEPRECATED: &str = "cyclops ui is deprecated; use cyclops watch";

/// `cyclops daemon log` with no log file. Not an error state: it means no
/// detached daemon has ever run from this home, and the sentence says what
/// makes one appear.
pub fn no_daemon_log(log: &std::path::Path) -> String {
    format!(
        "no daemon log at {}. One appears the first time `cyclops start` starts a daemon for you.",
        log.display()
    )
}

/// `cyclops name --self` outside tmux. The flag reads $TMUX_PANE, so a
/// shell with none set cannot mean any pane; the sentence hands over the
/// by-id spelling instead.
pub fn self_outside_tmux(name: &str) -> String {
    format!(
        "--self names the pane this command is running in, and this shell is not in one. \
         Run it inside tmux, or name the pane by id: cyclops name %0 {name}."
    )
}

/// `--raw` beside a non-detection source. The other sources ARE the raw
/// capture, so the flag there is a misunderstanding worth a sentence
/// rather than a silent no-op.
pub const RAW_NEEDS_DETECTION: &str =
    "--raw pairs with --source detection: it adds the capture the sensors read to the \
     detection view. This source is already the raw capture.";

/// `cyclops update` refuses --json: the bulk of its output is the
/// installer's own stream, and a JSON wrapper around a build log would be
/// a shape nothing could rely on.
pub const UPDATE_NO_JSON: &str =
    "cyclops update has no --json form: its output is the installer's stream. The machine-readable build is: cyclops --version";

/// Said before an update from a build with edited sources. The freshness
/// check is skipped rather than faked: no commit can ever match a .dirty
/// build ref, and "an update exists" would be a guess.
pub fn update_dirty(build_ref: &str) -> String {
    format!(
        "this build is {build_ref}, built from edited sources; no commit can match it, so the freshness check is skipped."
    )
}

/// The same skip for a build stamped outside git (a source tarball).
pub const UPDATE_UNKNOWN: &str =
    "this build is unknown, built outside git; the freshness check is skipped.";

/// The freshness check could not read the remote, so nothing was fetched
/// and nothing was changed.
pub fn update_unreachable(repo: &str, reff: &str, cause: &str) -> String {
    format!(
        "can't read {reff} from {repo}: {cause}. Check the network, or point CYCLOPS_REPO/CYCLOPS_REF at a repo and ref that exist."
    )
}

/// The clone failed, so there is no source to build and nothing was
/// changed. Same next step as the installer's own clone failure.
/// Where the incremental build cache lives.
///
/// Printed because a directory this tool creates, that grows to gigabytes,
/// and that the operator never asked for, is one they should hear about at
/// the time rather than discover while hunting for disk.
pub fn update_build_cache(dir: &std::path::Path) -> String {
    format!(
        "building in {} (delete it any time; it only costs a slow rebuild)",
        dir.display()
    )
}

/// The cache could not be made. Names the cost, which is time and nothing
/// else: the update still runs, it just runs the way it used to.
pub fn update_cache_unusable(dir: &std::path::Path, cause: &std::io::Error) -> String {
    format!(
        "no build cache at {} ({cause}); this update rebuilds from scratch",
        dir.display()
    )
}

pub fn update_clone_failed(repo: &str, reff: &str, cause: &str) -> String {
    format!(
        "could not clone {repo} at {reff}: {cause}. Check the network, or set CYCLOPS_REF to a branch that exists."
    )
}

/// The installer stopped partway. It owns the explanation (it prints what
/// went wrong and the fix as it dies), so this only points back up.
/// `cause` is set when the installer could not be started at all.
pub fn update_install_failed(cause: Option<&str>) -> String {
    match cause {
        Some(c) => format!("could not run the installer: {c}."),
        None => "the installer did not finish; its output above says how far it got.".to_string(),
    }
}

/// The update installed, and the new binary could not be found to answer
/// for itself. The installer's own report is the fallback authority.
pub const UPDATE_UNRESOLVED: &str =
    "can't find the new cyclops on PATH to report its build; the installer's report above names it.";

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

    #[test]
    fn attention_copy_owns_resolution_and_check_vocabulary() {
        assert_eq!(
            attention_resolution_verb(cyclops_proto::NotificationResolution::Complete),
            "submitted"
        );
        assert_eq!(
            attention_resolution_verb(cyclops_proto::NotificationResolution::Discard),
            "discarded"
        );
        let checks = cyclops_proto::AttentionChecks {
            notification_exact: true,
            trailer_anchored: true,
            process_matches: true,
            manifest_matches: true,
            terminal_action_safe: true,
        };
        let labels: Vec<_> = attention_check_rows(&checks)
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(
            labels,
            [
                "notification exact",
                "trailer anchored",
                "process binding matches",
                "manifest matches",
                "terminal action safe",
            ]
        );
        assert_eq!(attention_check_value(true), "yes");
        assert_eq!(attention_check_value(false), "no");
        let attempt =
            cyclops_proto::NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001")
                .unwrap();
        assert_eq!(
            attention_action_uncertain(
                cyclops_proto::NotificationResolution::Complete,
                attempt
            ),
            "submit action outcome uncertain; safe reconcile: cyclops attention complete att-00000000-0000-4000-8000-000000000001; this rechecks without sending a second key"
        );
        assert_eq!(
            attention_action_uncertain(
                cyclops_proto::NotificationResolution::Discard,
                attempt
            ),
            "discard action outcome uncertain; safe reconcile: cyclops attention discard att-00000000-0000-4000-8000-000000000001; this rechecks without sending a second key"
        );
    }

    /// The surfaces that still name the pin command say it byte for byte.
    /// A reader who has seen it once must not have to compare spellings.
    ///
    /// The status grid used to be in this list. It no longer offers a pin,
    /// because it no longer guesses that a missing manifest is the cause:
    /// the daemon says why a pane is unknown, and only some of those
    /// reasons are fixed by pinning one.
    #[test]
    fn every_surface_prints_the_same_pin_command() {
        let want = "cyclops name %4 reviewer --manifest <id>";
        for said in [
            named_but_undetected("%4", "reviewer"),
            needs_attention_for("reviewer", Some("%4"), None),
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
    fn runtime_identity_notice_names_both_sides_and_an_existing_recovery_command() {
        let compatibility = cyclops_client::HelloCompatibility::between(
            cyclops_client::RuntimeIdentity::new("0.1.0", Some("client-new")),
            cyclops_client::RuntimeIdentity::new("0.0.9", Some("daemon-old")),
        );
        assert_eq!(
            hello_compatibility_notice(&compatibility).as_deref(),
            Some(
                "version/build mismatch: cyclops 0.1.0 (client-new), cyclopsd 0.0.9 (daemon-old). Continuing; run cyclops daemon restart. If they still differ, update or reinstall the older side."
            )
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
    fn oversized_response_preserves_the_daemons_uncertainty_sentence() {
        // Obsolete when the daemon no longer substitutes a bounded uncertainty
        // response for a result that exceeds the official frame contract.
        let message = "daemon response was too large; request outcome is unknown";
        let error = ClientError::OversizedResponse(message.into());
        assert_eq!(client_error(&error, None), message);
    }

    #[test]
    fn hello_timeout_keeps_the_existing_read_timeout_sentence() {
        // Obsolete when CLI presentation intentionally distinguishes the
        // Hello read from later bounded daemon reads.
        let waited = Duration::from_secs(5);
        assert_eq!(
            client_error(&ClientError::HelloTimeout(waited), None),
            client_error(&ClientError::ReadTimeout(waited), None)
        );
    }

    #[test]
    fn a_lost_response_never_recommends_an_unkeyed_retry() {
        let answer = broken("timed out waiting for a reply");
        assert!(answer.contains("may already have landed"), "{answer}");
        assert!(answer.contains("same explicit --client-key"), "{answer}");
        assert!(!answer.ends_with("then retry."), "{answer}");
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
            needs_attention_for("worker", Some("%1"), None),
            "worker did not get this message; it is on the record and needs attention. Teach cyclops what runs in %1: cyclops name %1 worker --manifest <id>. cyclops status names the manifests that are loaded, and docs/reference/MANIFESTS.md is how to write one."
        );
        // Every other cause: no pin command, because a manifest does not
        // fix a dead pane or a name nobody answers to.
        assert_eq!(
            needs_attention_for("ghost", None, None),
            "ghost did not get this message. It is on the record and needs attention; cyclops status lists what is waiting on you and what to do about each one."
        );
    }

    #[test]
    fn after_write_attention_copy_requires_inspection_before_resend() {
        let copy = needs_attention_for("worker", Some("%1"), Some("verify_failed"));
        assert!(copy.contains("outcome unknown"), "{copy}");
        assert!(
            copy.contains("Inspect %1 and its composer before resending"),
            "{copy}"
        );
        assert!(!copy.contains("did not get this message"), "{copy}");
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
            "No messages yet. Send one: cyclops send <target> --subject ... --summary \"First sentence. Second sentence.\""
        );
        assert_eq!(
            no_messages(Some("reviewer")),
            "No messages with reviewer yet. Send one: cyclops send reviewer --subject ... --summary \"First sentence. Second sentence.\""
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
            wait_timeout(
                "reviewer",
                "turn ended",
                Duration::from_secs(60),
                Some("working")
            ),
            "reviewer didn't reach turn ended within 60 seconds. Last state: working. Give it more time with --timeout, or look in with cyclops status."
        );
        assert_eq!(
            wait_timeout("reviewer", "idle", Duration::from_secs(5), None),
            "reviewer didn't reach idle within 5 seconds. Give it more time with --timeout, or look in with cyclops status."
        );
    }

    /// The note under a scoped roster has both halves or it is useless:
    /// what was elided, and the command that shows it anyway.
    #[test]
    fn scoped_roster_note_names_the_sessions_and_the_way_out() {
        assert_eq!(
            also_watching(&["ops".into()]),
            "also watching ops · cyclops list --all to see every session"
        );
        assert_eq!(
            also_watching(&["ops".into(), "dev".into()]),
            "also watching ops, dev · cyclops list --all to see every session"
        );
    }

    /// Update's skip notes must be honest about WHY there is no check,
    /// and its failure copy must name where the answer is: the repo, the
    /// ref, or the installer's own stream.
    #[test]
    fn update_copy_names_the_cause_and_the_next_step() {
        assert_eq!(
            update_dirty("e610afc.dirty"),
            "this build is e610afc.dirty, built from edited sources; no commit can match it, so the freshness check is skipped."
        );
        assert_eq!(
            update_unreachable("https://x.example/r.git", "main", "no route"),
            "can't read main from https://x.example/r.git: no route. Check the network, or point CYCLOPS_REPO/CYCLOPS_REF at a repo and ref that exist."
        );
        assert_eq!(
            update_clone_failed("https://x.example/r.git", "nope", "not found"),
            "could not clone https://x.example/r.git at nope: not found. Check the network, or set CYCLOPS_REF to a branch that exists."
        );
        assert_eq!(
            update_install_failed(None),
            "the installer did not finish; its output above says how far it got."
        );
        assert_eq!(
            update_install_failed(Some("sh: not found")),
            "could not run the installer: sh: not found."
        );
        // The refusal points at the machine-readable alternative, and the
        // no-json sentence must not read as an error in the update itself.
        assert!(
            UPDATE_NO_JSON.contains("cyclops --version"),
            "{UPDATE_NO_JSON}"
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
