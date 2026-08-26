//! Renderers: strict grid, computed column widths, two-space gutters, no
//! trailing spaces. Pads by display width, not bytes. States always render
//! glyph plus word, and the group color on top of them is redundant: turn
//! color off and the same words are still there.
//!
//! Padding happens before painting everywhere below, so escape bytes never
//! reach the width table.
//!
//! The voice itself is not here. The width table, the clock gutter, the
//! state cells, the cause vocabulary and the badges live in
//! `cyclops_ui::grid`, which this crate already links to run `cyclops ui`.
//! They were copied here once, justified by this crate being a binary, and
//! the parity tests written to police the copy imported the very module
//! the copy was supposed to be impossible to import. What stays here is
//! this surface's own layout: padding, column widths, and the grids.

use std::path::Path;

use serde_json::Value;

use cyclops_proto::{
    AgentState, Attention, AttentionItem, ComposerMessageState, ComposerNextAction, ComposerProof,
    ComposerState, Delivery, DeliveryReceipt, DeliveryState, Detection, Event, Kind, LedgerLine,
    NotificationState, PaneStatus, Sensor, StatusResult,
};
use cyclops_ui::grid;

use crate::copy;
use crate::style::Style;

pub use cyclops_ui::grid::{display_width, state_words};

/// Pad with spaces to `width` display columns. Never truncates.
///
/// Padding is this crate's alone: the stream never pads, because autowrap
/// is off there and the terminal clips its own edge.
pub fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Max display width across a column's cells; an empty column costs 0.
fn column_width<S: AsRef<str>>(cells: impl Iterator<Item = S>) -> usize {
    cells.map(|s| display_width(s.as_ref())).max().unwrap_or(0)
}

/// Compact humane duration: "42s", "2m", "3h 12m", "5d 2h".
pub fn human_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        let h = s / 3600;
        let m = (s % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        let d = s / 86_400;
        let h = (s % 86_400) / 3600;
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d {h}h")
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn age(delta_ms: u64) -> String {
    if delta_ms < 1000 {
        "just now".into()
    } else {
        format!("{} ago", human_duration(delta_ms))
    }
}

/// One status row, resolved before layout so widths come from content.
struct Row {
    label: String,
    adopted: bool,
    state: AgentState,
    detail: Option<String>,
    facts: Vec<String>,
    session: usize,
}

fn composer_words(state: ComposerState) -> &'static str {
    match state {
        ComposerState::ComposerClean => "clean",
        ComposerState::HumanDraft => "human draft",
        ComposerState::VendorGhostSuggestion => "vendor ghost suggestion",
        ComposerState::CyclopsNotificationStaged => "Cyclops notification staged",
        ComposerState::CyclopsNotificationSubmitted => "Cyclops notification submitted",
        ComposerState::ComposerAmbiguous => "ambiguous",
    }
}

fn notification_words(state: NotificationState) -> &'static str {
    match state {
        NotificationState::Queued => "queued",
        NotificationState::Gating => "checking readiness",
        NotificationState::BlockedPreWrite => "blocked before write",
        NotificationState::QuotaHeld => "quota held",
        NotificationState::QuotaResetObserved => "quota reset observed",
        NotificationState::Writing => "writing",
        NotificationState::Staged => "waiting for submit",
        NotificationState::Submitting => "submit intent recorded",
        NotificationState::Submitted => "submitted",
        NotificationState::Notified => "notified",
        NotificationState::AttentionRequired => "needs attention",
        NotificationState::Withdrawn => "withdrawn",
        NotificationState::WithdrawnAfterStaging => "withdrawn after staging",
        NotificationState::WithdrawnByOperator => "withdrawn by operator",
        NotificationState::Superseded => "superseded",
    }
}

fn message_words(state: ComposerMessageState) -> &'static str {
    match state {
        ComposerMessageState::Pending => "pending",
        ComposerMessageState::Claimed => "claimed",
        ComposerMessageState::DeliveredDirect => "delivered direct",
        ComposerMessageState::Superseded => "superseded",
    }
}

fn next_action_words(pane: &PaneStatus, action: ComposerNextAction) -> String {
    match action {
        ComposerNextAction::AutomaticSubmit => "automatic submit".into(),
        ComposerNextAction::AutomaticReconcile => "automatic reconcile".into(),
        ComposerNextAction::InspectAttention => pane
            .notification_attempt
            .map(|attempt| format!("workspace admin: cyclops attention show {attempt} --diff"))
            .unwrap_or_else(|| "cyclops messages".into()),
        ComposerNextAction::InspectMessages => "cyclops messages".into(),
        ComposerNextAction::CheckHealth => "cyclops health".into(),
    }
}

/// Additional status facts when runtime state alone would hide useful truth.
fn status_fact_rows(pane: &PaneStatus, style: &Style) -> Vec<String> {
    let composer_is_observed = pane.composer_proof != ComposerProof::Unprovable
        || pane.notification_attempt.is_some()
        || pane.composer_reason.is_some();
    let composer_needs_detail = pane.composer != ComposerState::ComposerClean
        && pane.composer != ComposerState::VendorGhostSuggestion;
    let show_composer = composer_is_observed
        && (composer_needs_detail
            || pane.composer == ComposerState::VendorGhostSuggestion
            || pane.notification_state.is_some());
    let show_readiness = show_composer || pane.write_block.is_some();
    let show_runtime_certainty =
        pane.state == AgentState::Working && pane.working_confirmed.is_some();
    if !show_composer
        && !show_readiness
        && pane.notification_state.is_none()
        && !show_runtime_certainty
    {
        return Vec::new();
    }

    let mut facts = Vec::new();
    if show_runtime_certainty {
        facts.push(format!(
            "runtime working: {}",
            if pane.working_confirmed == Some(true) {
                "confirmed"
            } else {
                "provisional"
            }
        ));
    }
    if show_composer {
        facts.push(format!("composer {}", composer_words(pane.composer)));
    }
    if let Some(reason) = pane.composer_reason.as_deref() {
        facts.push(format!("composer reason {reason}"));
    }
    if pane.composer_candidates > 1 {
        facts.push(format!(
            "{} active notification barriers",
            pane.composer_candidates
        ));
    }
    if show_readiness {
        facts.push(match (&pane.write_block, pane.write_ready) {
            (_, true) => "write readiness ready".to_string(),
            (Some(reason), false) => format!("write readiness held: {reason}"),
            (None, false) => "write readiness held: evidence unavailable".to_string(),
        });
    }
    if let Some(state) = pane.notification_state {
        facts.push(format!("notification {}", notification_words(state)));
    }
    if let Some(state) = pane.message_state {
        facts.push(format!("message {}", message_words(state)));
    }
    if let Some(action) = pane.next_action {
        facts.push(format!("next action {}", next_action_words(pane, action)));
    }
    if let Some(attempt) = pane.notification_attempt {
        facts.push(format!("attempt {attempt}"));
    }

    vec![format!("    {}", style.dim(&facts.join(" · ")))]
}

/// The pane title when it says something the row does not already say.
///
/// This is the "what is it on" column: agent CLIs publish the current task
/// there ("Implementing rate limiter"), and tmux publishes noise there
/// (the hostname, F5; the window name; the command). Exact repeats of
/// anything already on the row are dropped, and what is left is either a
/// task hint or nothing.
fn title_hint(p: &PaneStatus, label: &str) -> Option<String> {
    let title = p.title.trim();
    if !title.is_empty() && title != label && title != p.current_command && title != p.window_name {
        return Some(title.to_string());
    }
    None
}

/// Detail column for `status`: the informative title, else the running
/// command, else nothing. `status` answers "what is in this pane" for
/// every pane including unnamed ones, so a bare command is worth showing
/// there; `cyclops list` asks a narrower question and takes the title
/// alone. The agent binary as a command repeats the manifest, so it is
/// suppressed too.
fn detail_for(p: &PaneStatus, label: &str) -> Option<String> {
    if let Some(t) = title_hint(p, label) {
        return Some(t);
    }
    let cmd = p.current_command.trim();
    if cmd.is_empty() || cmd == label || Some(cmd) == p.manifest.as_deref() {
        return None;
    }
    Some(cmd.to_string())
}

/// One agent row: the name cell, already painted by the caller, then the
/// state cell, then an optional detail after a dim separator.
///
/// The state cell is padded only when a detail follows it. The padding
/// lives INSIDE the color run, so a trailing-space trim could not reach it
/// afterwards: a row with nothing to say takes the unpadded cell instead,
/// leaving no invisible column for a trim to miss.
fn agent_row(
    painted_name: &str,
    state: AgentState,
    detail: Option<&str>,
    state_w: usize,
    style: &Style,
) -> String {
    match detail {
        Some(d) => format!(
            "  {painted_name}  {}  {}",
            style.state(state, &pad(&state_words(state), state_w)),
            style.dim(d)
        ),
        None => format!("  {painted_name}  {}", grid::state_cell(state, style)),
    }
}

pub fn render_status(res: &StatusResult, style: &Style, config_path: &Path) -> String {
    let sep = style.dim("·");
    let names: Vec<String> = res
        .sessions
        .iter()
        .map(|s| {
            if s.attached {
                s.name.clone()
            } else {
                format!("{} {}", s.name, style.dim("(reconnecting)"))
            }
        })
        .collect();
    let watching = if names.is_empty() {
        "nothing".to_string()
    } else {
        names.join(", ")
    };
    // Normal status reports the live pane fleet. Durable delivery alarms
    // remain in the mailbox, alarm, and stream surfaces that can act on
    // them. The named constructor makes that scope an explicit decision.
    let attention = Attention::from_live_status(res);
    let eye = attention.header();
    let mut header = format!(
        "{} {} {sep} watching {watching} {sep} tmux {} {sep} up {}",
        style.eye(eye.calm, &eye.cell),
        style.bold("cyclops"),
        res.tmux_version,
        human_duration(res.uptime_ms),
    );
    if let Some(tail) = &eye.tail {
        header.push_str(&format!(" {sep} {tail}"));
    }
    if res.admin_unread > 0 {
        header.push_str(&format!(" {sep} admin inbox {}", res.admin_unread));
    }

    if res.sessions.is_empty() {
        // Empty state invites the next action instead of erroring. The
        // backlog rows still ride along: no branch of this function may
        // print a count with nothing behind it.
        let mut out = vec![
            header,
            String::new(),
            format!(
                "  No sessions yet. Name one in {} and cyclops will pick it up.",
                config_path.display()
            ),
        ];
        out.extend(blocked_notification_rows(res, style));
        out.extend(diagnostic_rows(res, style));
        out.extend(waiting_rows(&attention, res, style));
        return out.join("\n");
    }

    let mut rows: Vec<Row> = Vec::new();
    for (si, sess) in res.sessions.iter().enumerate() {
        for p in &sess.panes {
            if unmanaged_shell(p) {
                continue;
            }
            let (label, adopted) = match &p.agent {
                Some(a) => (a.clone(), true),
                None => (p.pane_id.clone(), false),
            };
            let mut detail = detail_for(p, &label);
            // Amendment c: a hook config that never fired is a silent
            // verification downgrade; the grid says so out loud.
            if p.hooks_verified == Some(false) {
                detail = Some(match detail {
                    Some(d) => format!("{d} · hooks unverified"),
                    None => "hooks unverified".to_string(),
                });
            }
            rows.push(Row {
                label,
                adopted,
                state: p.state,
                detail,
                facts: status_fact_rows(p, style),
                session: si,
            });
        }
    }

    let label_w = column_width(rows.iter().map(|r| r.label.as_str()));
    let state_w = column_width(rows.iter().map(|r| state_words(r.state)));

    let multi = res.sessions.len() > 1;
    let mut out = vec![header, String::new()];
    for (si, sess) in res.sessions.iter().enumerate() {
        if multi {
            if si > 0 {
                out.push(String::new());
            }
            out.push(format!("  {}", style.dim(&sess.name)));
        }
        for r in rows.iter().filter(|r| r.session == si) {
            let label = if r.adopted {
                style.role(&r.label, &pad(&r.label, label_w))
            } else {
                style.dim(&pad(&r.label, label_w))
            };
            out.push(agent_row(
                &label,
                r.state,
                r.detail.as_deref(),
                state_w,
                style,
            ));
            out.extend(r.facts.iter().cloned());
        }
    }
    out.extend(unknown_rows(res, style));
    out.extend(blocked_notification_rows(res, style));
    out.extend(diagnostic_rows(res, style));
    out.extend(waiting_rows(&attention, res, style));
    out.join("\n")
}

fn blocked_notification_rows(res: &StatusResult, style: &Style) -> Vec<String> {
    if res.blocked_notifications.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![
        String::new(),
        format!("  {}", style.dim("wake blocked before write")),
    ];
    for item in &res.blocked_notifications {
        let reason = item
            .recipient
            .notification
            .pane_width_block()
            .map(|(observed, required)| {
                format!("pane too narrow ({observed}, requires {required})")
            })
            .or_else(|| {
                item.recipient
                    .notification
                    .pre_write_cause
                    .map(|cause| cause.label().to_string())
            })
            .unwrap_or_else(|| "reason unavailable".to_string());
        let position = item
            .recipient
            .fifo_position
            .map(|position| format!("FIFO {position}"))
            .unwrap_or_else(|| "FIFO position unavailable".into());
        let route = item
            .recipient
            .current_route
            .as_ref()
            .map(|route| format!("{} ({})", route.label, route.pane_id))
            .unwrap_or_else(|| "route unavailable".into());
        rows.push(format!(
            "  {} {} · {reason} · waited {} · {position} · {route}",
            style.role(&item.recipient.label, &item.recipient.label),
            style.accent(item.message_id.as_str()),
            human_duration(item.waiting_age_ms)
        ));
        let next = match item.next_action {
            Some(cyclops_proto::StatusNextAction::WithdrawNotification) => format!(
                "workspace admin: cyclops notification withdraw {} --recipient {}",
                item.notification_attempt, item.recipient.recipient
            ),
            None => "recipient claim or cyclops messages inspection".into(),
        };
        rows.push(format!(
            "  {}",
            style.dim(&format!(
                "Attempt {} · next: {next}",
                item.notification_attempt
            ))
        ));
        rows.push(format!(
            "  {}",
            style.dim(&format!(
                "Message remains claimable from its recipient pane: cyclops inbox claim {}",
                item.message_id
            ))
        ));
    }
    let omitted = res
        .blocked_notifications_total
        .saturating_sub(res.blocked_notifications.len() as u64);
    if omitted > 0 {
        rows.push(format!(
            "  {}",
            style.dim(&format!(
                "{omitted} more blocked wakes · next: cyclops messages"
            ))
        ));
    }
    rows
}

fn diagnostic_rows(res: &StatusResult, style: &Style) -> Vec<String> {
    let risks: Vec<_> = res
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "deadlock_risk")
        .collect();
    let worker_failures: Vec<_> = res
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "notification_worker_failed")
        .collect();
    let settlement_failures: Vec<_> = res
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "notification_settlement_storage_failed")
        .collect();
    let recovery_failures: Vec<_> = res
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "notification_recovery_storage_failed")
        .collect();
    if risks.is_empty()
        && worker_failures.is_empty()
        && settlement_failures.is_empty()
        && recovery_failures.is_empty()
    {
        return Vec::new();
    }

    let mut rows = Vec::new();
    if !risks.is_empty() {
        rows.extend([String::new(), format!("  {}", style.dim("deadlock risk"))]);
    }
    for risk in risks {
        rows.push(format!(
            "  {} {} {} · cyclops watch holds this pane working while its notification is gated",
            style.role(&risk.recipient_label, &risk.recipient_label),
            style.dim(&risk.pane_id),
            style.accent(risk.message_id.as_str())
        ));
        rows.push(format!(
            "  {}",
            style.dim(&format!(
                "In {} ({}), interrupt cyclops watch, then run from that pane: cyclops inbox next --timeout 30s",
                risk.recipient_label, risk.pane_id
            ))
        ));
    }
    if !worker_failures.is_empty() {
        rows.extend([
            String::new(),
            format!("  {}", style.dim("notification worker failure")),
        ]);
        for failure in worker_failures {
            rows.push(format!(
                "  {} {} {} · delivery stopped and will not restart automatically",
                style.role(&failure.recipient_label, &failure.recipient_label),
                style.dim(&failure.pane_id),
                style.accent(failure.message_id.as_str())
            ));
        }
    }
    if !settlement_failures.is_empty() {
        rows.extend([
            String::new(),
            format!("  {}", style.dim("notification settlement blocked")),
        ]);
        for failure in settlement_failures {
            rows.push(format!(
                "  {} {} {} {} · state storage refused the final settlement",
                style.role(&failure.recipient_label, &failure.recipient_label),
                style.dim(&failure.pane_id),
                style.accent(failure.message_id.as_str()),
                style.accent(&failure.notification_attempt.to_string())
            ));
            rows.push(format!(
                "  {}",
                style.dim(&format!(
                    "Run cyclops health, repair state storage, then run cyclops daemon restart to reconcile {}",
                    failure.notification_attempt
                ))
            ));
        }
    }
    if !recovery_failures.is_empty() {
        rows.extend([
            String::new(),
            format!("  {}", style.dim("notification recovery blocked")),
        ]);
        for failure in recovery_failures {
            rows.push(format!(
                "  {} {} {} {} · state storage refused recovery; this attempt still owns the FIFO",
                style.role(&failure.recipient_label, &failure.recipient_label),
                style.dim(&failure.pane_id),
                style.accent(failure.message_id.as_str()),
                style.accent(&failure.notification_attempt.to_string())
            ));
            rows.push(format!(
                "  {}",
                style.dim(&format!(
                    "Run cyclops health, repair state storage, then run cyclops daemon restart to reconcile {}",
                    failure.notification_attempt
                ))
            ));
        }
    }
    rows
}

/// The sentence under the grid explaining every `? unknown` row on it.
///
/// A state cell says WHAT cyclops read. Unknown is the one cell where that
/// is not enough: it is not a state the agent is in, it is cyclops unable
/// to read one, and the pane it names can receive nothing until that is
/// fixed. The grid keeps its one-line rows and the reason goes underneath,
/// once, however many rows there are.
fn unknown_rows(res: &StatusResult, style: &Style) -> Vec<String> {
    let unknown: Vec<&PaneStatus> = res
        .sessions
        .iter()
        .flat_map(|s| s.panes.iter())
        .filter(|p| p.state == AgentState::Unknown && !unmanaged_shell(p))
        .collect();
    let Some(first) = unknown.first() else {
        return Vec::new();
    };
    // The pin command names the pane by its tmux id and keeps the name it
    // already answers to, so it can be pasted whole: `cyclops name` takes
    // a target and a label, and passing the label as the target would
    // rename an adopted pane to a placeholder.
    let loaded = res.manifests.as_ref().map(|m| m.ids.as_slice());
    vec![
        String::new(),
        format!(
            "  {}",
            style.dim(&copy::unknown_panes(
                unknown.len(),
                &first.pane_id,
                first.agent.as_deref(),
                loaded
            ))
        ),
    ]
}

/// An ordinary terminal in a watched tmux session is not an unconfigured
/// agent. Keep it out of the agent roster and its setup guidance.
fn unmanaged_shell(pane: &PaneStatus) -> bool {
    pane.agent.is_none()
        && pane.manifest.is_none()
        && pane.state == AgentState::Unknown
        && matches!(
            pane.current_command.as_str(),
            "bash" | "fish" | "sh" | "zsh"
        )
}

/// The roster: one row per named agent, on the same grid as `status`.
///
///     watching main · home /Users/x/.cyclops
///
///     implementer  ● working  Implementing rate limiter
///     reviewer     ○ idle
///
/// The dim header says whose roster this is: the watched session(s), and
/// the home whose socket answered. Two daemons on two homes each answer
/// `cyclops list` with a plausible roster, and without the header the one
/// in a fresh terminal tab was anyone's guess.
///
/// Three columns and no column header. The name wears its role color, the
/// state cell wears its group color on top of the glyph and the word, and
/// the task hint is dim. Turn color off and every one of those still reads.
///
/// Sessions are not a column: a label is unique across every watched
/// session (the daemon refuses a duplicate), so naming the session would
/// add a column that never disambiguates anything. The header names them
/// once instead.
///
/// `also_watching` is the sessions a scoped roster left out (cmd_list's
/// caller-session rule). Non-empty, it earns one dim line under the
/// header naming them and the way out, so the header never claims the
/// daemon watches less than it does. Empty, the output is byte for byte
/// the unscoped grid.
pub fn render_list(
    res: &StatusResult,
    style: &Style,
    home: &Path,
    also_watching: &[String],
) -> String {
    let mut header = style.dim(&format!(
        "watching {} · home {}",
        watching_words(res),
        home.display()
    ));
    if !also_watching.is_empty() {
        header.push_str(&format!(
            "\n  {}",
            style.dim(&copy::also_watching(also_watching))
        ));
    }
    let rows: Vec<(String, AgentState, Option<String>)> = res
        .sessions
        .iter()
        .flat_map(|s| s.panes.iter())
        .filter_map(|p| {
            let label = p.agent.clone()?;
            let hint = title_hint(p, &label);
            Some((label, p.state, hint))
        })
        .collect();
    if rows.is_empty() {
        // The empty roster keeps inviting the next action, under the same
        // header and on the grid's indent, exactly as `status` lays out
        // its own empty state.
        return format!("{header}\n\n  {}", copy::NO_AGENTS);
    }
    let label_w = column_width(rows.iter().map(|(l, _, _)| l.as_str()));
    let state_w = column_width(rows.iter().map(|(_, s, _)| state_words(*s)));
    let grid = rows
        .iter()
        .map(|(label, state, hint)| {
            let name = style.role(label, &pad(label, label_w));
            agent_row(&name, *state, hint.as_deref(), state_w, style)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}\n\n{grid}")
}

/// The check glyph, and the one rule for it. Every surface that prints a
/// check goes through here.
///
/// The weight says WHO answered for the fact beside it. Heavy ✔ means the
/// component that owns the fact confirmed it: a vendor hook for a
/// delivery, cyclopsd for a name, a roster, or its own liveness. Light ✓
/// means nobody could be asked, and the line carries the best statement
/// there is: a read off the screen, or an agent count taken off a
/// workspace file.
///
/// So the same words take either glyph, and the glyph is the difference:
/// `✔ workspace ready · 3 agents` is three panes cyclopsd will deliver to,
/// `✓ workspace ready · 3 agents` is three names in a file with no daemon
/// running to put them on anything.
///
/// GOALS asks for a hollow check on unverified; no portable hollow check
/// glyph exists in terminal fonts, so weight is the pair (STATUS.md
/// deviations).
pub fn check(confirmed: bool) -> &'static str {
    if confirmed {
        "✔"
    } else {
        "✓"
    }
}

/// `cyclops daemon status` when one is answering.
///
/// The same facts as the `cyclops status` header minus the roster, in the
/// same order, because a reader who knows one should not have to learn
/// the other. No eye: this line is about the process, and the eye is
/// about the agents.
pub fn daemon_running(res: &StatusResult, style: &Style) -> String {
    let sep = style.dim("·");
    let pid = match res.pid {
        Some(p) => format!(" {sep} pid {p}"),
        None => String::new(),
    };
    format!(
        "{} {sep} up {}{pid} {sep} watching {}",
        style.bold("● cyclopsd is running"),
        human_duration(res.uptime_ms),
        watching_words(res),
    )
}

/// What the daemon has been told to watch, for a one-line summary.
fn watching_words(res: &StatusResult) -> String {
    if res.sessions.is_empty() {
        return "nothing".to_string();
    }
    res.sessions
        .iter()
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `cyclops daemon stop`, in the badge voice.
///
/// It names what was NOT touched, because stopping the thing that watches
/// your agents reads like it might take the agents with it. It does not:
/// tmux keeps running and the record is on disk.
pub fn daemon_stopped(pid: u32, style: &Style) -> String {
    let sep = style.dim("·");
    format!(
        "{} {sep} {}",
        style.bold(&format!("{} stopped cyclopsd", check(true))),
        style.dim(&format!(
            "pid {pid}, your tmux panes and the record are untouched"
        ))
    )
}

pub fn daemon_restarted(old_pid: u32, style: &Style) -> String {
    let sep = style.dim("·");
    format!(
        "{} {sep} {}",
        style.bold(&format!("{} restarted cyclopsd", check(true))),
        style.dim(&format!(
            "was pid {old_pid}, now on the installed binaries; queued messages rode through"
        ))
    )
}

/// The answer to `cyclops name`, in the badge voice: what happened, then
/// the detail after a dim separator.
///
/// Heavy check: this is the daemon's answer to its own write. It put the
/// pane in the registry and the line on the ledger before replying, so
/// there is nothing left to confirm.
///
/// A name is an address, and an address nothing can be delivered to is
/// half a name. So a pane no manifest binds gets a second line saying so
/// here, where the reader is, rather than at the receipt of the first
/// message half a minute later.
pub fn render_named(result: &Value, style: &Style) -> String {
    let sep = style.dim("·");
    let ok = check(true);
    let pane = result["pane_id"].as_str().unwrap_or_default();
    let manifest = result["manifest"].as_str();
    match result["label"].as_str() {
        Some(label) => {
            let tail = match manifest {
                Some(m) => format!("{pane}, detects as {m}"),
                None => pane.to_string(),
            };
            let mut out = format!(
                "{ok} named {} {sep} {}",
                style.role(label, label),
                style.dim(&tail)
            );
            // `manifest` is the pin the caller asked for; `detects_as` is
            // what binds the pane now. A daemon that predates the field
            // says nothing, and nothing is what gets printed.
            if result["detects_as"].is_null() && result.get("detects_as").is_some() {
                out.push('\n');
                out.push_str(&format!(
                    "  {}",
                    style.dim(&copy::named_but_undetected(pane, label))
                ));
            }
            out
        }
        None => format!(
            "{ok} cleared {sep} {}",
            style.dim(&format!("{pane} is unnamed"))
        ),
    }
}

/// Status is a current operational surface, not an alarm archive.
///
/// Keep the newest alarms visible and point to the operator tools for the
/// rest. The header retains the full count, so the summary never hides that
/// work remains.
const STATUS_DELIVERY_ROW_LIMIT: usize = 8;

fn waiting_rows(attention: &Attention, res: &StatusResult, style: &Style) -> Vec<String> {
    let mut open: Vec<(String, String, DeliveryState, Option<String>, u64)> = attention
        .items()
        .into_iter()
        .filter_map(|i| match i {
            AttentionItem::Delivery { to, id, state } => {
                let (cause, ts) = res
                    .open_deliveries
                    .iter()
                    .find(|d| d.to == to && d.id == id)
                    .map(|d| (d.cause.clone(), d.ts))
                    .unwrap_or((None, 0));
                Some((to, id, state, cause, ts))
            }
            AttentionItem::Agent { .. } => None,
        })
        .collect();
    if open.is_empty() {
        return Vec::new();
    }

    open.sort_by(|left, right| {
        right
            .4
            .cmp(&left.4)
            .then_with(|| left.0.cmp(&right.0))
            .then_with(|| left.1.cmp(&right.1))
    });
    let hidden = open.len().saturating_sub(STATUS_DELIVERY_ROW_LIMIT);
    let shown = &open[..open.len().min(STATUS_DELIVERY_ROW_LIMIT)];
    let to_w = shown
        .iter()
        .map(|(to, _, _, _, _)| display_width(to))
        .max()
        .unwrap_or(0);
    let mut out = vec![String::new(), format!("  {}", style.dim("waiting on you"))];
    out.extend(shown.iter().map(|(to, id, state, cause, ts)| {
        let badge = receipt_badge(
            &DeliveryReceipt {
                to: to.clone(),
                state: *state,
                notification_state: None,
                quota_state: None,
                notification_settlement: None,
                wake_block: None,
                position: None,
                note: None,
                // The eye counts items from the record, which names the
                // recipient and not the pane the delivery went to.
                pane: None,
                held_by: None,
            },
            style,
        );
        let cause = cause
            .as_deref()
            .map(grid::cause_words)
            .unwrap_or_else(|| "cause unknown".into());
        let when = if *ts == 0 {
            "time unknown".into()
        } else {
            age(now_ms().saturating_sub(*ts))
        };
        let detail = style.dim(&format!("{cause} · {id} · {when}"));
        format!("  {}  {badge} · {detail}", style.role(to, &pad(to, to_w)))
    }));
    if hidden > 0 {
        out.push(format!(
            "  {}",
            style.dim(&format!(
                "{hidden} older alarms not shown · inspect or clear: cyclops alarm preview --older-than <age>"
            ))
        ));
    }
    out
}

/// One receipt badge in this surface's paint. Words, glyph and color all
/// come from `cyclops_ui::grid`; the CLI supplies only the painter.
///
/// Its checks follow [`check`]: hook-verified is confirmed and takes the
/// heavy one, screen-tier is not and takes the light one. The parity test
/// below reads both from [`check`] so the two cannot drift.
pub fn receipt_badge(r: &DeliveryReceipt, style: &Style) -> String {
    grid::receipt_badge(r, style)
}

/// Receipts as send shows them: one delivery is a bare badge line, a
/// broadcast is a grid of role-colored recipients and badges.
pub fn render_receipts(rs: &[DeliveryReceipt], style: &Style) -> String {
    if rs.len() == 1 {
        return receipt_badge(&rs[0], style);
    }
    let to_w = rs.iter().map(|r| display_width(&r.to)).max().unwrap_or(0);
    rs.iter()
        .map(|r| {
            format!(
                "  {}  {}",
                style.role(&r.to, &pad(&r.to, to_w)),
                receipt_badge(r, style)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One wait outcome as a badge, same voice as receipts: glyph plus word,
/// qualifier after a dim separator. Also the whole output of a reached
/// `cyclops wait`.
pub fn wait_badge(
    outcome: &str,
    state: Option<AgentState>,
    waited_ms: Option<u64>,
    delivery: Option<&str>,
    style: &Style,
) -> String {
    let sep = style.dim("·");
    let with = |head: &str, tail: &str| format!("{head} {sep} {}", style.dim(tail));
    match outcome {
        "reached" => {
            // Reached-with-a-state IS a state cell, so it wears the state
            // cell's group color. The outcomes below are wait vocabulary
            // rather than agent or delivery states and take no group.
            let head = match state {
                Some(s) => grid::state_cell(s, style),
                // Light check by [`check`]'s rule: the daemon said the
                // wait reached its target and named no state to show for
                // it, so there is nothing here that anybody confirmed.
                None => format!("{} reached", check(false)),
            };
            match waited_ms {
                Some(ms) => with(&head, &format!("waited {}", human_duration(ms))),
                None => head,
            }
        }
        "timeout" => match state {
            Some(s) => with("⚠ wait timed out", &format!("still {s}")),
            None => "⚠ wait timed out".to_string(),
        },
        "occupant_changed" => with("✗ pane changed occupant", "wait abandoned"),
        "not_delivered" => match delivery {
            Some(d) => with("⚠ not delivered", d),
            None => "⚠ not delivered".to_string(),
        },
        other => other.to_string(),
    }
}

/// Pad with spaces on the left to `width` display columns (right-aligned
/// gutter cells). Never truncates.
pub fn pad_left(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - w))
    }
}

/// Proleptic Gregorian civil date from unix ms, UTC (Howard Hinnant's
/// days-from-civil inverse). std ships no timezone database; the record's
/// date gutter is UTC like the rest of the CLI's clocks.
fn civil_from_ms(ms: u64) -> (i64, u32, u32) {
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Date cell for the history gutter: "Jul 28", plus the year when it is
/// not the current one.
fn date_cell(ts: u64, now: u64) -> String {
    let (y, m, d) = civil_from_ms(ts);
    let (now_y, _, _) = civil_from_ms(now);
    let mon = MONTHS[(m - 1) as usize];
    if y == now_y {
        format!("{mon} {d}")
    } else {
        format!("{mon} {d} {y}")
    }
}

/// Timestamp gutter for the record: relative age under 24h, date beyond.
pub fn history_gutter(ts: u64, now: u64) -> String {
    let delta = now.saturating_sub(ts);
    if delta < 86_400_000 {
        human_duration(delta)
    } else {
        date_cell(ts, now)
    }
}

/// One folded ledger delivery in the M1 badge voice, painted like a
/// receipt badge (same words, same painter).
pub fn delivery_badge(d: &Delivery, style: &Style) -> String {
    grid::delivery_badge(&d.to, d.state, d.cause.as_deref(), style)
}

/// One message resolved for the grid before layout.
struct MsgRow<'a> {
    gutter: String,
    who_plain: String,
    who_colored: String,
    fyi: bool,
    subject: &'a str,
    /// Inline badge for a single-recipient message.
    badge: Option<String>,
    /// (recipient, badge) grid for a broadcast: one msg fact, N badges.
    subs: Vec<(String, String)>,
    body: Option<&'a str>,
}

fn msg_rows<'a>(lines: &'a [LedgerLine], style: &Style, now: u64) -> Vec<MsgRow<'a>> {
    lines
        .iter()
        .filter(|l| matches!(l.kind, Kind::Msg | Kind::Fyi))
        .map(|l| {
            let (who_plain, who_colored) = match l.to.as_slice() {
                [to] => (
                    format!("{} → {to}", l.from),
                    format!("{} → {}", style.role(&l.from, &l.from), style.role(to, to)),
                ),
                [] => (l.from.clone(), style.role(&l.from, &l.from)),
                many => (
                    format!("{} → {} agents", l.from, many.len()),
                    format!("{} → {} agents", style.role(&l.from, &l.from), many.len()),
                ),
            };
            let (badge, subs) = if l.to.len() > 1 {
                (
                    None,
                    l.deliveries
                        .iter()
                        .map(|d| (d.to.clone(), delivery_badge(d, style)))
                        .collect(),
                )
            } else {
                (
                    l.deliveries.first().map(|d| delivery_badge(d, style)),
                    Vec::new(),
                )
            };
            MsgRow {
                gutter: history_gutter(l.ts, now),
                who_plain,
                who_colored,
                fyi: l.kind == Kind::Fyi,
                subject: l.subject.as_deref().unwrap_or(""),
                badge,
                subs,
                body: l.body.as_deref(),
            }
        })
        .collect()
}

/// The shared message grid: aligned gutter, from → to, an fyi column when
/// any announcement is present, subject, badge. Broadcasts hang their
/// per-recipient badges under the fact line; thread mode adds bodies with
/// a hanging indent and a blank line between messages.
fn render_messages(lines: &[LedgerLine], style: &Style, now: u64, bodies: bool) -> String {
    let rows = msg_rows(lines, style, now);
    let g_w = rows
        .iter()
        .map(|r| display_width(&r.gutter))
        .max()
        .unwrap_or(0);
    let who_w = rows
        .iter()
        .map(|r| display_width(&r.who_plain))
        .max()
        .unwrap_or(0);
    let tag_w = if rows.iter().any(|r| r.fyi) { 3 } else { 0 };
    let subj_w = rows
        .iter()
        .map(|r| display_width(r.subject))
        .max()
        .unwrap_or(0);
    let indent = " ".repeat(2 + g_w + 2);

    let mut blocks: Vec<Vec<String>> = Vec::new();
    for r in &rows {
        let mut block = Vec::new();
        let mut line = format!("  {}  ", style.dim(&pad_left(&r.gutter, g_w)));
        line.push_str(&r.who_colored);
        line.push_str(&" ".repeat(who_w - display_width(&r.who_plain)));
        if tag_w > 0 {
            let tag = if r.fyi { "fyi" } else { "" };
            line.push_str("  ");
            line.push_str(&style.dim(tag));
            line.push_str(&" ".repeat(tag_w - tag.len()));
        }
        line.push_str("  ");
        line.push_str(&pad(r.subject, subj_w));
        if let Some(badge) = &r.badge {
            line.push_str("  ");
            line.push_str(badge);
        }
        block.push(line.trim_end().to_string());
        if !r.subs.is_empty() {
            let to_w = r
                .subs
                .iter()
                .map(|(to, _)| display_width(to))
                .max()
                .unwrap_or(0);
            for (to, badge) in &r.subs {
                block.push(format!(
                    "{indent}{}  {badge}",
                    style.role(to, &pad(to, to_w))
                ));
            }
        }
        if bodies {
            if let Some(body) = r.body {
                for body_line in body.trim_end_matches('\n').lines() {
                    block.push(format!("{indent}{body_line}").trim_end().to_string());
                }
            }
        }
        blocks.push(block);
    }
    let sep = if bodies { "\n\n" } else { "\n" };
    blocks
        .iter()
        .map(|b| b.join("\n"))
        .collect::<Vec<_>>()
        .join(sep)
}

/// The record as `cyclops history` shows it: newest last.
pub fn render_history(lines: &[LedgerLine], style: &Style, now: u64) -> String {
    render_messages(lines, style, now, false)
}

/// One thread as `cyclops thread` shows it: messages with bodies, oldest
/// first. State and gate lines ride along in --json only; the human view
/// keeps the badges, which already summarize them.
pub fn render_thread(lines: &[LedgerLine], style: &Style, now: u64) -> String {
    render_messages(lines, style, now, true)
}

/// Heavy check by [`check`]'s rule: the daemon answered this round trip
/// itself, which is the whole of what the line claims.
pub fn render_ping(rtt_ms: f64, style: &Style) -> String {
    format!(
        "{} cyclops is up {} {}",
        check(true),
        style.dim("·"),
        style.dim(&format!("{rtt_ms:.1}ms"))
    )
}

fn sensor_name(s: Sensor) -> &'static str {
    match s {
        Sensor::Hook => "hook",
        Sensor::Title => "title",
        Sensor::Output => "output",
        Sensor::Screen => "screen",
    }
}

/// Detection view: fused verdict on top, one row per sensor beneath.
/// Disagreement is an observable state and gets named in the header.
pub fn render_detection(target: &str, det: &Detection, style: &Style, now_ms: u64) -> String {
    let sep = style.dim("·");
    let mut header = format!(
        "{} {sep} {}",
        style.role(target, target),
        grid::state_cell(det.state, style)
    );
    if det.disagreement {
        header.push_str(&format!(" {sep} ⚠ sensors disagree"));
    }
    header.push_str(&format!(
        " {sep} {}",
        style.dim(&format!("decided by {}", det.decided_by))
    ));
    // Runtime state and write-readiness are two answers, and the diagnostic
    // shows both (rule 12). The verdict comes from the one owner of the
    // rule, never recomputed here, so the surface cannot drift from the
    // gate's decision.
    header.push_str(&format!(
        " {sep} {}",
        match det.write_block.as_deref() {
            None if det.write_ready => style.dim("write-ready"),
            // An older daemon stamps neither field. Absent evidence is
            // not permission, so it reads as refused rather than ready.
            None => style.dim("not write-ready: unstamped"),
            Some(reason) => style.dim(&format!("not write-ready: {reason}")),
        }
    ));
    if det.readings.is_empty() {
        return header;
    }

    let name_w = det
        .readings
        .iter()
        .map(|r| display_width(sensor_name(r.sensor)))
        .max()
        .unwrap_or(0);
    let state_w = det
        .readings
        .iter()
        .map(|r| display_width(&state_words(r.state)))
        .max()
        .unwrap_or(0);
    let rule_w = det
        .readings
        .iter()
        .map(|r| display_width(&r.rule))
        .max()
        .unwrap_or(0);

    let mut out = vec![header, String::new()];
    for r in &det.readings {
        out.push(format!(
            "  {}  {}  {}  {}",
            pad(sensor_name(r.sensor), name_w),
            style.state(r.state, &pad(&state_words(r.state), state_w)),
            pad(&r.rule, rule_w),
            style.dim(&age(now_ms.saturating_sub(r.ts))),
        ));
    }
    out.join("\n")
}

/// One event, one line: timestamp gutter, event name, then whatever the
/// payload can say in user-side words.
pub fn render_event_line(ev: &Event, style: &Style, now_ms: u64) -> String {
    let ts = ev.data.get("ts").and_then(Value::as_u64).unwrap_or(now_ms);
    let mut line = format!("{}  {}", style.dim(&grid::clock_hms(ts)), ev.event);
    let summary = event_summary(&ev.data, style);
    if !summary.is_empty() {
        line.push_str("  ");
        line.push_str(&summary);
    }
    line
}

/// Pull the fields worth a human's eye out of an event payload. Unknown
/// vocabularies still render, as dimmed compact JSON minus the timestamp.
fn event_summary(data: &Value, style: &Style) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in ["agent", "target"] {
        if let Some(v) = data.get(key).and_then(Value::as_str) {
            parts.push(style.role(v, v));
        }
    }
    if let Some(from) = data.get("from").and_then(Value::as_str) {
        let to = match data.get("to") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            _ => None,
        };
        match to {
            Some(t) if !t.is_empty() => {
                parts.push(format!("{} → {t}", style.role(from, from)));
            }
            _ => parts.push(style.role(from, from)),
        }
    }
    if let Some(v) = data.get("state") {
        if let Ok(st) = serde_json::from_value::<AgentState>(v.clone()) {
            parts.push(grid::state_cell(st, style));
        } else if let Some(s) = v.as_str() {
            parts.push(s.to_string());
        }
    }
    if let Some(s) = data.get("subject").and_then(Value::as_str) {
        parts.push(style.dim(s));
    }
    if parts.is_empty() {
        if let Value::Object(map) = data {
            let rest: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(k, _)| k.as_str() != "ts")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !rest.is_empty() {
                parts.push(style.dim(&Value::Object(rest).to_string()));
            }
        }
    }
    parts.join(&format!(" {} ", style.dim("·")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{SensorReading, SessionStatus};
    use serde_json::json;

    fn pane(
        pane_id: &str,
        agent: Option<&str>,
        manifest: Option<&str>,
        title: &str,
        cmd: &str,
        state: AgentState,
    ) -> PaneStatus {
        PaneStatus {
            pane_id: pane_id.into(),
            window_id: "@1".into(),
            window_name: "agents".into(),
            agent: agent.map(String::from),
            manifest: manifest.map(String::from),
            title: title.into(),
            current_command: cmd.into(),
            dead: false,
            in_mode: false,
            write_ready: false,
            write_block: None,
            composer: cyclops_proto::ComposerState::ComposerAmbiguous,
            composer_proof: cyclops_proto::ComposerProof::Unprovable,
            notification_attempt: None,
            composer_reason: None,
            composer_candidates: 0,
            notification_state: None,
            message_state: None,
            next_action: None,
            width: 120,
            height: 40,
            state,
            state_ms: None,
            working_confirmed: None,
            hooks_verified: None,
            manifest_display_name: None,
        }
    }

    /// The block [`fixture`]'s one unknown pane earns, appended to every
    /// golden below. Written once because it is one sentence with one
    /// wording, and a golden that spelled it again would be a second copy
    /// of the copy.
    const UNKNOWN_NOTE: &str = "\n\n  1 pane reads unknown: none of agy, claude, codex matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %4 <label> --manifest <id>. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md.";

    fn open_delivery(id: &str, to: &str, state: DeliveryState) -> cyclops_proto::OpenDelivery {
        cyclops_proto::OpenDelivery {
            id: id.into(),
            to: to.into(),
            recipient: None,
            state,
            ts: 0,
            cause: Some(
                match state {
                    DeliveryState::ParkedBlockedQuota => "blocked_quota",
                    _ => "verify_failed",
                }
                .into(),
            ),
        }
    }

    fn fixture() -> StatusResult {
        StatusResult {
            daemon_version: "0.1.0".into(),
            daemon_build: None,
            daemon_process: None,
            daemon_executable: None,
            proto: 1,
            boot_id: "b-test".into(),
            uptime_ms: 2 * 60 * 1000,
            tmux_version: "3.6a".into(),
            sessions: vec![SessionStatus {
                name: "main".into(),
                attached: true,
                panes: vec![
                    pane(
                        "%1",
                        Some("reviewer"),
                        Some("claude"),
                        "Run the tests",
                        "claude",
                        AgentState::Working,
                    ),
                    // Title repeats the label, command repeats the manifest:
                    // the detail column stays empty.
                    pane(
                        "%2",
                        Some("implementer"),
                        Some("claude"),
                        "implementer",
                        "claude",
                        AgentState::Idle,
                    ),
                    // Unadopted pane: labelled by pane id, command as detail.
                    pane("%4", None, None, "", "vim", AgentState::Unknown),
                ],
            }],
            mailbox_routes: Vec::new(),
            admin_unread: 0,
            // The daemon serves this half only when the caller asks for
            // it. An answer without it counts panes alone, which is what
            // the calm-rig cases below pin.
            open_deliveries: Vec::new(),
            diagnostics: Vec::new(),
            blocked_notifications: Vec::new(),
            blocked_notifications_total: 0,
            manifests: Some(cyclops_proto::Manifests {
                ids: vec!["agy".into(), "claude".into(), "codex".into()],
                dir: Some("/x/manifests".into()),
            }),
            pid: Some(4242),
        }
    }

    #[test]
    fn status_grid_plain_is_exact_on_a_calm_rig() {
        // Nothing blocked, so the header must not wear the alarm eye. The
        // shipped themes give surface.accent and eye.alert the same hex
        // and the same 256 fallback, so an accent-painted ◉ here was
        // byte- and color-identical to the stream's "two or more
        // attention items" mark on a system with nothing wrong.
        let got = render_status(&fixture(), &Style::none(), Path::new("/x/config.toml"));
        let expected = format!(
            "‿ cyclops · watching main · tmux 3.6a · up 2m\n\
             \n\
             \x20 reviewer     ● working  Run the tests\n\
             \x20 implementer  ○ idle\n\
             \x20 %4           ? unknown  vim{UNKNOWN_NOTE}"
        );
        assert_eq!(got, expected);
    }

    #[test]
    fn status_header_reports_admin_unread_messages() {
        let mut status = fixture();
        status.admin_unread = 3;
        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(rendered.lines().next().unwrap().contains("admin inbox 3"));
    }

    #[test]
    fn status_does_not_hide_an_owned_notification_behind_runtime_idle() {
        let mut status = fixture();
        let pane = &mut status.sessions[0].panes[1];
        pane.write_block = Some("composer_hold".into());
        pane.composer = ComposerState::CyclopsNotificationStaged;
        pane.composer_proof = ComposerProof::ExactNotification;
        pane.notification_attempt =
            Some("att-00000000-0000-4000-8000-000000000001".parse().unwrap());
        pane.notification_state = Some(NotificationState::Staged);
        pane.message_state = Some(ComposerMessageState::Pending);
        pane.next_action = Some(ComposerNextAction::AutomaticSubmit);

        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(rendered.contains("implementer  ○ idle"), "{rendered}");
        assert!(
            rendered.contains("composer Cyclops notification staged"),
            "{rendered}"
        );
        assert!(
            rendered.contains("write readiness held: composer_hold"),
            "{rendered}"
        );
        assert!(
            rendered.contains("notification waiting for submit"),
            "{rendered}"
        );
        assert!(rendered.contains("message pending"), "{rendered}");
        assert!(
            rendered.contains("next action automatic submit"),
            "{rendered}"
        );
    }

    #[test]
    fn status_distinguishes_provisional_working_and_names_ambiguity_recovery() {
        let mut status = fixture();
        let pane = &mut status.sessions[0].panes[0];
        pane.working_confirmed = Some(false);
        pane.composer = ComposerState::ComposerAmbiguous;
        pane.composer_proof = ComposerProof::Unprovable;
        pane.notification_attempt =
            Some("att-00000000-0000-4000-8000-000000000001".parse().unwrap());
        pane.composer_reason = Some("composer_capture_unprovable".into());
        pane.composer_candidates = 1;
        pane.notification_state = Some(NotificationState::Submitting);
        pane.message_state = Some(ComposerMessageState::Pending);
        pane.next_action = Some(ComposerNextAction::CheckHealth);

        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(
            rendered.contains("runtime working: provisional"),
            "{rendered}"
        );
        assert!(
            rendered.contains("composer reason composer_capture_unprovable"),
            "{rendered}"
        );
        assert!(
            rendered.contains("notification submit intent recorded"),
            "{rendered}"
        );
        assert!(
            rendered.contains("next action cyclops health"),
            "{rendered}"
        );
    }

    #[test]
    fn status_offers_attention_inspection_only_for_an_attention_attempt() {
        let mut status = fixture();
        {
            let pane = &mut status.sessions[0].panes[1];
            pane.notification_attempt =
                Some("att-00000000-0000-4000-8000-000000000001".parse().unwrap());
            pane.notification_state = Some(NotificationState::AttentionRequired);
            pane.next_action = Some(ComposerNextAction::InspectAttention);
        }

        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(
            rendered.contains(
                "next action workspace admin: cyclops attention show att-00000000-0000-4000-8000-000000000001 --diff"
            ),
            "{rendered}"
        );

        status.sessions[0].panes[1].notification_attempt = None;
        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(
            rendered.contains("next action cyclops messages"),
            "{rendered}"
        );
        assert!(!rendered.contains("cyclops attention show"), "{rendered}");
    }

    #[test]
    fn status_names_a_blocked_wake_without_exposing_message_content() {
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient =
            cyclops_proto::RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let attempt = "att-00000000-0000-4000-8000-000000000001".parse().unwrap();
        let mut status = fixture();
        status
            .blocked_notifications
            .push(cyclops_proto::StatusBlockedNotification {
                message_id: "m-blocked".parse().unwrap(),
                notification_attempt: attempt,
                recipient: cyclops_proto::MessageRecipientSummary {
                    recipient,
                    label: "reviewer".into(),
                    direction: cyclops_proto::MessageDirection::Outbound,
                    needs_action: true,
                    can_manage_attention: false,
                    can_withdraw_notification: true,
                    current_route: Some(cyclops_proto::MessageRecipientRoute {
                        label: "reviewer-now".into(),
                        pane_id: "%1".parse().unwrap(),
                    }),
                    available: true,
                    mailbox: cyclops_proto::MailboxEntryState::Pending,
                    fifo_position: Some(2),
                    notification: cyclops_proto::MessageNotificationSummary {
                        state: cyclops_proto::MessageNotificationState::Gating,
                        quota_state: None,
                        settlement: None,
                        operator_withdrawn: None,
                        attempt_id: Some(attempt),
                        cause: None,
                        pre_write_cause: Some(
                            cyclops_proto::NotificationPreWriteCause::BindingUnprovable,
                        ),
                        pre_write_pane_width: None,
                        pre_write_required_pane_width: None,
                        attention_cleared: None,
                        resolution: None,
                        resolution_intent: None,
                        resolution_action_accepted: None,
                        resolution_consumption_observed: None,
                        updated_at: Some(1),
                    },
                },
                waiting_age_ms: 61_000,
                next_action: Some(cyclops_proto::StatusNextAction::WithdrawNotification),
            });
        status.blocked_notifications_total = 5;

        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(rendered.contains("wake blocked before write"), "{rendered}");
        assert!(rendered.contains("binding unprovable"), "{rendered}");
        assert!(rendered.contains("FIFO 2"), "{rendered}");
        assert!(rendered.contains("reviewer-now (%1)"), "{rendered}");
        assert!(
            rendered.contains(&format!(
                "workspace admin: cyclops notification withdraw {attempt} --recipient {recipient}"
            )),
            "{rendered}"
        );
        assert!(
            rendered.contains("cyclops inbox claim m-blocked"),
            "{rendered}"
        );
        assert!(!rendered.contains("secret body"), "{rendered}");
        assert!(rendered.contains("4 more blocked wakes"), "{rendered}");

        status.blocked_notifications[0]
            .recipient
            .notification
            .pre_write_cause =
            Some(cyclops_proto::NotificationPreWriteCause::WriteReadinessChanged);
        status.blocked_notifications[0]
            .recipient
            .notification
            .pre_write_pane_width = Some(59);
        status.blocked_notifications[0]
            .recipient
            .notification
            .pre_write_required_pane_width = Some(60);
        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(
            rendered.contains("pane too narrow (59, requires 60)"),
            "{rendered}"
        );

        status.blocked_notifications[0]
            .recipient
            .notification
            .pre_write_cause = Some(cyclops_proto::NotificationPreWriteCause::WorkerFailed);
        status.blocked_notifications[0]
            .recipient
            .notification
            .pre_write_pane_width = None;
        status.blocked_notifications[0]
            .recipient
            .notification
            .pre_write_required_pane_width = None;
        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(rendered.contains("worker failed"), "{rendered}");
        assert!(rendered.contains("waited 1m"), "{rendered}");
        assert!(rendered.contains("reviewer-now (%1)"), "{rendered}");
        assert!(
            rendered.contains(&format!(
                "workspace admin: cyclops notification withdraw {attempt} --recipient {recipient}"
            )),
            "{rendered}"
        );

        status.blocked_notifications[0].next_action = None;
        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(!rendered.contains("notification withdraw"), "{rendered}");
        assert!(
            rendered.contains("recipient claim or cyclops messages inspection"),
            "{rendered}"
        );

        status.sessions.clear();
        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(rendered.contains("wake blocked before write"), "{rendered}");
    }

    #[test]
    fn status_names_a_content_free_watch_deadlock_and_the_pull_exit() {
        let mut status = fixture();
        status.diagnostics.push(cyclops_proto::StatusDiagnostic {
            code: "deadlock_risk".into(),
            message_id: "m-startup".parse().unwrap(),
            notification_attempt: "att-00000000-0000-4000-8000-000000000001".parse().unwrap(),
            recipient: serde_json::from_value(serde_json::json!({
                "kind": "agent",
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "session_instance_id": "00000000-0000-0000-0000-000000000002",
                "pane_id": "%1"
            }))
            .unwrap(),
            recipient_label: "reviewer".into(),
            pane_id: "%1".into(),
        });

        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(rendered.contains("deadlock risk"), "{rendered}");
        assert!(rendered.contains("reviewer %1 m-startup"), "{rendered}");
        assert!(
            rendered.contains(
                "In reviewer (%1), interrupt cyclops watch, then run from that pane: cyclops inbox next --timeout 30s"
            ),
            "{rendered}"
        );
        assert!(!rendered.contains("secret body"), "{rendered}");
    }

    #[test]
    fn status_names_the_exact_settlement_fault_and_recovery() {
        let mut status = fixture();
        let attempt = "att-00000000-0000-4000-8000-000000000009";
        status.diagnostics.push(cyclops_proto::StatusDiagnostic {
            code: "notification_settlement_storage_failed".into(),
            message_id: "m-settlement".parse().unwrap(),
            notification_attempt: attempt.parse().unwrap(),
            recipient: serde_json::from_value(serde_json::json!({
                "kind": "agent",
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "session_instance_id": "00000000-0000-0000-0000-000000000002",
                "pane_id": "%1"
            }))
            .unwrap(),
            recipient_label: "reviewer".into(),
            pane_id: "%1".into(),
        });

        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(
            rendered.contains("notification settlement blocked"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("reviewer %1 m-settlement {attempt}")),
            "{rendered}"
        );
        assert!(rendered.contains("Run cyclops health"), "{rendered}");
        assert!(rendered.contains("cyclops daemon restart"), "{rendered}");
        assert!(!rendered.contains("secret body"), "{rendered}");
    }

    #[test]
    fn status_keeps_a_failed_recovery_bound_to_its_fifo_owner() {
        let mut status = fixture();
        let attempt = "att-00000000-0000-4000-8000-000000000010";
        status.diagnostics.push(cyclops_proto::StatusDiagnostic {
            code: "notification_recovery_storage_failed".into(),
            message_id: "m-recovery".parse().unwrap(),
            notification_attempt: attempt.parse().unwrap(),
            recipient: serde_json::from_value(serde_json::json!({
                "kind": "agent",
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "session_instance_id": "00000000-0000-0000-0000-000000000002",
                "pane_id": "%1"
            }))
            .unwrap(),
            recipient_label: "reviewer".into(),
            pane_id: "%1".into(),
        });

        let rendered = render_status(&status, &Style::none(), Path::new("/x/config.toml"));
        assert!(
            rendered.contains("notification recovery blocked"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("reviewer %1 m-recovery {attempt}")),
            "{rendered}"
        );
        assert!(rendered.contains("still owns the FIFO"), "{rendered}");
        assert!(rendered.contains("Run cyclops health"), "{rendered}");
        assert!(rendered.contains("cyclops daemon restart"), "{rendered}");
        assert!(!rendered.contains("secret body"), "{rendered}");
    }

    /// An unknown pane is the one cell on the grid that is not a state the
    /// agent is in: it is cyclops unable to read one, and the pane can
    /// receive nothing until it is fixed. Two causes wear that same label
    /// and their fixes are nothing alike, so the daemon's manifest set
    /// decides which sentence a reader gets. A daemon too old to say is a
    /// third answer, not the empty set.
    #[test]
    fn an_unknown_pane_gets_the_reason_its_cause_earns() {
        let mut res = fixture();

        // Nothing loaded: the install is broken for every pane at once, and
        // no per-pane pin would help.
        res.manifests = Some(cyclops_proto::Manifests {
            ids: Vec::new(),
            dir: Some("/x/manifests".into()),
        });
        let got = render_status(&res, &Style::none(), Path::new("/x"));
        assert!(
            got.contains("1 pane reads unknown: cyclopsd loaded no detection manifests."),
            "{got}"
        );
        assert!(
            got.contains("Nothing can be delivered to an unknown pane"),
            "{got}"
        );
        assert!(
            got.contains("cyclops start, then restart cyclopsd"),
            "{got}"
        );
        assert!(!got.contains("--manifest"), "nothing to pin: {got}");

        // A daemon that predates the field says neither, so neither is
        // claimed: the pin is still the next step and no id list is quoted.
        res.manifests = None;
        let got = render_status(&res, &Style::none(), Path::new("/x"));
        assert!(
            got.contains("no manifest matches what is running there"),
            "{got}"
        );
        assert!(
            got.contains("cyclops name %4 <label> --manifest <id>"),
            "{got}"
        );
        assert!(!got.contains("loaded no detection manifests"), "{got}");

        // Two unknown panes take one sentence, counted and pluralized, and
        // the command names the first of them.
        res.manifests = Some(cyclops_proto::Manifests {
            ids: vec!["claude".into()],
            dir: None,
        });
        res.sessions[0].panes[0].state = AgentState::Unknown;
        let got = render_status(&res, &Style::none(), Path::new("/x"));
        assert!(got.contains("2 panes read unknown"), "{got}");
        // The first unknown row is reviewer, which already answers to a
        // name: the command keeps it rather than renaming the pane.
        assert!(
            got.contains("cyclops name %1 reviewer --manifest <id>"),
            "{got}"
        );
        assert_eq!(
            got.matches("panes read unknown").count(),
            1,
            "one sentence however many rows: {got}"
        );
    }

    /// And a grid with nothing unknown on it says nothing at all. The
    /// explanation is not a footer.
    #[test]
    fn a_grid_with_no_unknown_pane_stays_quiet() {
        let mut res = fixture();
        res.sessions[0].panes.pop();
        let got = render_status(&res, &Style::none(), Path::new("/x"));
        assert!(!got.contains("unknown"), "{got}");
    }

    #[test]
    fn an_unmanaged_shell_is_not_an_unknown_agent() {
        let mut res = fixture();
        let shell = res.sessions[0]
            .panes
            .last_mut()
            .expect("fixture has a shell");
        shell.current_command = "zsh".into();
        let got = render_status(&res, &Style::none(), Path::new("/x"));
        assert!(!got.contains("%4"), "{got}");
        assert!(!got.contains("reads unknown"), "{got}");
    }

    /// The roster grid, pinned exactly. This is the shape the landing page
    /// promises: name, how it is doing, what it is on, aligned, under one
    /// header line saying whose roster it is.
    #[test]
    fn list_grid_plain_is_exact() {
        let got = render_list(&fixture(), &Style::none(), Path::new("/x"), &[]);
        let expected = "watching main · home /x\n\
                        \n\
                        \x20 reviewer     ● working  Run the tests\n\
                        \x20 implementer  ○ idle";
        assert_eq!(got, expected);
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in: {line:?}");
        }
    }

    /// The header answers the question the rows cannot: WHICH rig this
    /// roster came from. Two daemons on two homes both answer `cyclops
    /// list` with a plausible roster, so the header has to name every
    /// watched session and the home whose socket answered.
    #[test]
    fn list_header_names_the_sessions_and_the_home() {
        let mut res = fixture();
        res.sessions.push(SessionStatus {
            name: "ops".into(),
            attached: true,
            panes: Vec::new(),
        });
        let got = render_list(&res, &Style::none(), Path::new("/second/.cyclops"), &[]);
        assert!(
            got.starts_with("watching main, ops · home /second/.cyclops\n"),
            "{got}"
        );

        // A daemon watching nothing says so in status's word for it.
        res.sessions.clear();
        let got = render_list(&res, &Style::none(), Path::new("/second/.cyclops"), &[]);
        assert!(
            got.starts_with("watching nothing · home /second/.cyclops\n"),
            "{got}"
        );
    }

    /// A scoped roster's header keeps two promises at once: it names only
    /// the session the rows come from, and the dim line under it names
    /// every session that was elided plus the command that shows them.
    /// This is the exact shape, on the grid's own indent.
    #[test]
    fn a_scoped_list_names_what_it_left_out_and_the_way_back() {
        let got = render_list(
            &fixture(),
            &Style::none(),
            Path::new("/x"),
            &["ops".into(), "dev".into()],
        );
        let expected = "watching main · home /x\n\
                        \x20 also watching ops, dev · cyclops list --all to see every session\n\
                        \n\
                        \x20 reviewer     ● working  Run the tests\n\
                        \x20 implementer  ○ idle";
        assert_eq!(got, expected);
    }

    /// And a roster that was not scoped says nothing about scoping: the
    /// note is for elision, not decoration.
    #[test]
    fn an_unscoped_list_carries_no_scoping_note() {
        let got = render_list(&fixture(), &Style::none(), Path::new("/x"), &[]);
        assert!(!got.contains("also watching"), "{got}");
        assert!(!got.contains("--all"), "{got}");
    }

    /// Painted, the grid still ends where the words end.
    ///
    /// A plain golden cannot see this one. The state cell is padded to its
    /// column and then colored, so the padding sits INSIDE the escape run
    /// and a `trim_end` over the finished line walks straight past it. A
    /// row with no hint has to take the unpadded cell instead, which is
    /// what `status` does and what this pins.
    #[test]
    fn list_rows_carry_no_padding_a_reader_cannot_see() {
        let mut res = fixture();
        res.sessions[0].panes[1].state = AgentState::BlockedPermission;
        let painted = render_list(
            &res,
            &Style::with_theme(cyclops_theme::Theme::default(), true),
            Path::new("/x"),
            &[],
        );
        for line in painted.lines() {
            let stripped: String = {
                // Drop every CSI run, leaving the words the row prints.
                let mut out = String::new();
                let mut chars = line.chars();
                while let Some(c) = chars.next() {
                    if c == '\x1b' {
                        for c in chars.by_ref() {
                            if c == 'm' {
                                break;
                            }
                        }
                    } else {
                        out.push(c);
                    }
                }
                out
            };
            assert_eq!(
                stripped,
                stripped.trim_end(),
                "painted row ends in space: {line:?}"
            );
        }
    }

    /// Unnamed panes are `status`'s business. `list` answers "who is on
    /// this team", and a pane nobody named is not on it. The empty roster
    /// keeps the invitation, under the same header.
    #[test]
    fn list_shows_named_panes_only_and_invites_the_first_one() {
        let got = render_list(&fixture(), &Style::none(), Path::new("/x"), &[]);
        assert!(!got.contains("%4"), "{got}");

        let mut res = fixture();
        for p in &mut res.sessions[0].panes {
            p.agent = None;
        }
        assert_eq!(
            render_list(&res, &Style::none(), Path::new("/x"), &[]),
            format!("watching main · home /x\n\n  {}", copy::NO_AGENTS)
        );
    }

    /// The state cell carries glyph and word on this grid too, and every
    /// state is spelled the way the stream and `status` spell it.
    #[test]
    fn list_states_read_as_glyph_and_word() {
        let mut res = fixture();
        res.sessions[0].panes[1].state = AgentState::BlockedQuota;
        res.sessions[0].panes[1].title = String::new();
        let got = render_list(&res, &Style::none(), Path::new("/x"), &[]);
        assert_eq!(
            got,
            "watching main · home /x\n\
             \n\
             \x20 reviewer     ● working        Run the tests\n\
             \x20 implementer  ⊘ blocked_quota"
        );
    }

    /// `cyclops name` answers in the badge voice: what happened, then the
    /// detail after a dim separator. A pin is named out loud, because the
    /// whole point of passing it was to stop guessing.
    #[test]
    fn the_name_verb_says_what_it_did() {
        let s = Style::none();
        assert_eq!(
            render_named(
                &json!({"pane_id": "%3", "label": "reviewer", "manifest": null}),
                &s
            ),
            "✔ named reviewer · %3"
        );
        assert_eq!(
            render_named(
                &json!({"pane_id": "%3", "label": "reviewer", "manifest": "claude"}),
                &s
            ),
            "✔ named reviewer · %3, detects as claude"
        );
        // The clear badge answers for the name and nothing else. Whether
        // the pane's own border format came back is cyclopsd's to report,
        // and this line must never stand in for it.
        assert_eq!(
            render_named(&json!({"pane_id": "%3", "label": null}), &s),
            "✔ cleared · %3 is unnamed"
        );
    }

    /// A name is an address. `cyclops name` used to report success for a
    /// pane nothing binds, and the first message to it died in the gate
    /// half a minute later with the reason nowhere near the command that
    /// caused it.
    #[test]
    fn naming_a_pane_nothing_detects_says_so_on_the_spot() {
        let s = Style::none();
        let got = render_named(
            &json!({"pane_id": "%0", "label": "implementer",
                    "manifest": null, "detects_as": null}),
            &s,
        );
        assert_eq!(
            got,
            "✔ named implementer · %0\n  nothing detects %0 yet, so implementer can't receive a message. cyclops status names the manifests that are loaded; pin one with: cyclops name %0 implementer --manifest <id>"
        );
        // Bound, so the badge stands alone exactly as it always has.
        let got = render_named(
            &json!({"pane_id": "%0", "label": "implementer",
                    "manifest": null, "detects_as": "claude"}),
            &s,
        );
        assert_eq!(got, "✔ named implementer · %0");
        // A daemon too old to answer the question says nothing, and a
        // clear was never about detection at all.
        let got = render_named(&json!({"pane_id": "%0", "label": "implementer"}), &s);
        assert_eq!(got, "✔ named implementer · %0");
        let got = render_named(
            &json!({"pane_id": "%0", "label": null, "detects_as": null}),
            &s,
        );
        assert_eq!(got, "✔ cleared · %0 is unnamed");
    }

    #[test]
    fn status_header_eye_opens_with_the_blocked_agent_count() {
        let mut res = fixture();
        res.sessions[0].panes[1].state = AgentState::BlockedPermission;
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        let expected = format!(
            "◑ 1 cyclops · watching main · tmux 3.6a · up 2m · 1 needs attention\n\
             \n\
             \x20 reviewer     ● working             Run the tests\n\
             \x20 implementer  ⚠ blocked_permission\n\
             \x20 %4           ? unknown             vim{UNKNOWN_NOTE}"
        );
        assert_eq!(got, expected);
        // Two blocked agents open the eye fully and take the plural.
        res.sessions[0].panes[0].state = AgentState::BlockedQuota;
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        assert_eq!(
            got.lines().next().unwrap_or_default(),
            "◉ 2 cyclops · watching main · tmux 3.6a · up 2m · 2 need attention"
        );
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in: {line:?}");
        }
    }

    #[test]
    fn status_eye_glyphs_come_from_the_stream_vocabulary() {
        // One glyph table, not two: the header must read the same device
        // the stream header and docs/guides/ui.md describe.
        let mut res = fixture();
        assert!(render_status(&res, &Style::none(), Path::new("/x"))
            .starts_with(cyclops_proto::Eye::Closed.glyph()));
        res.sessions[0].panes[0].state = AgentState::BlockedModal;
        assert!(render_status(&res, &Style::none(), Path::new("/x"))
            .starts_with(cyclops_proto::Eye::Opening.glyph()));
        res.sessions[0].panes[1].state = AgentState::BlockedModal;
        assert!(render_status(&res, &Style::none(), Path::new("/x"))
            .starts_with(cyclops_proto::Eye::Open.glyph()));
    }

    /// Both live-pane surfaces use the shared eye vocabulary.
    #[test]
    fn live_status_uses_the_shared_eye_vocabulary() {
        let cases = [
            (AgentState::Working, Vec::new()),
            (AgentState::BlockedPermission, Vec::new()),
        ];
        for (state, open) in cases {
            let mut res = fixture();
            res.sessions[0].panes[1].state = state;
            res.open_deliveries = open;

            // What the register says the header must carry.
            let attention = cyclops_proto::Attention::from_status(&res);
            let eye = attention.header();

            // Surface one: the CLI grid.
            let status = render_status(&res, &Style::none(), Path::new("/x"));
            let status_head = status.lines().next().expect("a header line");

            // Surface two: the stream header, from the same answer.
            let mut app = cyclops_ui::App::new(
                cyclops_ui::Theme::none(),
                cyclops_ui::View::Admin,
                cyclops_ui::Filter::default(),
            );
            for e in app.seed_status(cyclops_ui::StatusSeed::from_status(&res)) {
                app.replay(e);
            }
            while app.tick_eye() {}
            let stream_head = cyclops_ui::build(&mut app, 80, 12).remove(0);

            let lead = format!("{} cyclops", eye.cell);
            assert!(
                status_head.starts_with(&lead),
                "status header lost the composed eye cell: {status_head:?}"
            );
            assert!(
                stream_head.starts_with(&lead),
                "stream header lost the composed eye cell: {stream_head:?}"
            );
            match &eye.tail {
                Some(tail) => {
                    assert!(status_head.ends_with(tail), "{status_head:?}");
                    assert!(stream_head.ends_with(tail), "{stream_head:?}");
                }
                None => {
                    assert!(!status_head.contains("attention"), "{status_head:?}");
                    assert!(!stream_head.contains("attention"), "{stream_head:?}");
                }
            }
        }
    }

    #[test]
    fn status_reports_live_panes_without_durable_delivery_alarms() {
        let mut res = fixture();
        res.open_deliveries = vec![
            open_delivery("m-1", "implementer", DeliveryState::ParkedBlockedQuota),
            open_delivery("m-2", "reviewer", DeliveryState::AttentionRequired),
        ];
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        let expected = format!(
            "‿ cyclops · watching main · tmux 3.6a · up 2m\n\
             \n\
             \x20 reviewer     ● working  Run the tests\n\
             \x20 implementer  ○ idle\n\
             \x20 %4           ? unknown  vim{UNKNOWN_NOTE}"
        );
        assert_eq!(got, expected);
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in: {line:?}");
        }

        // A blocked pane still opens the live status eye by itself.
        res.sessions[0].panes[1].state = AgentState::BlockedQuota;
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        assert!(
            got.lines()
                .next()
                .unwrap_or_default()
                .contains("1 needs attention"),
            "{got}"
        );
    }

    #[test]
    fn status_never_renders_historical_delivery_rows() {
        let mut res = fixture();
        res.open_deliveries = (0..10)
            .map(|index| {
                let mut delivery = open_delivery(
                    &format!("m-{index:02}"),
                    "reviewer",
                    DeliveryState::AttentionRequired,
                );
                delivery.ts = index + 1;
                delivery
            })
            .collect();

        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        assert!(!got.contains("waiting on you"), "{got}");
        assert!(!got.contains("m-"), "{got}");
    }

    #[test]
    fn status_marks_hook_unverified_panes() {
        let mut res = fixture();
        // reviewer has an informative title: the marker appends after it.
        res.sessions[0].panes[0].hooks_verified = Some(false);
        // implementer has no detail: the marker stands alone.
        res.sessions[0].panes[1].hooks_verified = Some(false);
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        let expected = format!(
            "‿ cyclops · watching main · tmux 3.6a · up 2m\n\
             \n\
             \x20 reviewer     ● working  Run the tests · hooks unverified\n\
             \x20 implementer  ○ idle     hooks unverified\n\
             \x20 %4           ? unknown  vim{UNKNOWN_NOTE}"
        );
        assert_eq!(got, expected);
        // Verified and undeclared panes carry no marker.
        res.sessions[0].panes[0].hooks_verified = Some(true);
        res.sessions[0].panes[1].hooks_verified = None;
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        assert!(!got.contains("hooks unverified"));
    }

    #[test]
    fn status_rows_have_no_trailing_spaces() {
        let got = render_status(&fixture(), &Style::none(), Path::new("/x/config.toml"));
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in: {line:?}");
        }
    }

    #[test]
    fn status_empty_state_invites_config() {
        let mut res = fixture();
        res.sessions.clear();
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        // No panes means nothing blocked: the empty rig is calm too.
        let expected = "‿ cyclops · watching nothing · tmux 3.6a · up 2m\n\
                        \n\
                        \x20 No sessions yet. Name one in /x/config.toml and cyclops will pick it up.";
        assert_eq!(got, expected);
    }

    fn receipt(state: DeliveryState, position: Option<u32>, note: Option<&str>) -> DeliveryReceipt {
        DeliveryReceipt {
            to: "reviewer".into(),
            state,
            notification_state: None,
            quota_state: None,
            notification_settlement: None,
            wake_block: None,
            position,
            note: note.map(String::from),
            pane: None,
            held_by: None,
        }
    }

    /// Every agent state and every delivery state, so a variant added
    /// later cannot skip the color-off check below.
    const EVERY_AGENT_STATE: [AgentState; 8] = [
        AgentState::Unknown,
        AgentState::Idle,
        AgentState::IdleWithInput,
        AgentState::Working,
        AgentState::BlockedModal,
        AgentState::BlockedPermission,
        AgentState::BlockedQuota,
        AgentState::Dead,
    ];

    const EVERY_DELIVERY_STATE: [DeliveryState; 10] = [
        DeliveryState::Queued,
        DeliveryState::Gating,
        DeliveryState::Pasting,
        DeliveryState::Staged,
        DeliveryState::Submitted,
        DeliveryState::DeliveredVerified,
        DeliveryState::DeliveredUnverified,
        DeliveryState::RetryQueued,
        DeliveryState::AttentionRequired,
        DeliveryState::ParkedBlockedQuota,
    ];

    /// The reason state color is allowed to exist: it is redundant. With
    /// color off, every badge is byte-identical to the words grid
    /// composed, and every status row still carries its state's glyph and
    /// word. `NO_COLOR` and `--plain` lose nothing.
    #[test]
    fn color_off_is_byte_identical_to_the_grid_words() {
        let s = Style::none();
        for state in EVERY_DELIVERY_STATE {
            assert_eq!(
                receipt_badge(&receipt(state, None, None), &s),
                grid::receipt_badge(&receipt(state, None, None), &grid::Plain),
                "{state:?}"
            );
        }
        for state in EVERY_AGENT_STATE {
            let mut res = fixture();
            res.sessions[0].panes[0].state = state;
            let got = render_status(&res, &s, Path::new("/x/config.toml"));
            assert!(
                got.contains(&state_words(state)),
                "{state} lost its glyph and word:\n{got}"
            );
        }
    }

    /// SGR runs removed, so a colored render can be compared to the plain
    /// one it has to lay out identically.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The status grid paints its state cells through the group tokens,
    /// and pads before painting. Both halves matter: an unpainted cell
    /// drops the second encoding's color half, and a cell measured for
    /// width AFTER painting counts escape bytes as columns and tears the
    /// grid apart.
    #[test]
    fn status_grid_paints_state_cells_through_their_group() {
        let (theme, warnings) = cyclops_theme::Theme::parse(
            concat!(
                "[state]\n",
                "healthy = { hex = \"#010203\", c256 = 31 }\n",
                "quiet = { hex = \"#040506\", c256 = 32 }\n",
                "needs_you = { hex = \"#070809\", c256 = 33 }\n",
            ),
            "test",
        )
        .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let s = Style::with_theme(theme, false);
        let mut res = fixture();
        res.sessions[0].panes[1].state = AgentState::BlockedPermission;
        let got = render_status(&res, &s, Path::new("/x/config.toml"));

        // The widest cell sets the column, so the other two are padded
        // inside their own paint.
        let w = display_width(&state_words(AgentState::BlockedPermission));
        for (state, code) in [
            (AgentState::Working, 31),
            (AgentState::Unknown, 32),
            (AgentState::BlockedPermission, 33),
        ] {
            let cell = state_words(state);
            let cell = if state == AgentState::BlockedPermission {
                cell
            } else {
                pad(&cell, w)
            };
            assert!(
                got.contains(&format!("\x1b[38;5;{code}m{cell}\x1b[0m")),
                "{state} did not paint through its group:\n{got}"
            );
        }

        // Padding before painting, stated as a layout invariant: strip the
        // color and the grid is the plain grid, byte for byte.
        let plain = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        assert_eq!(strip_sgr(&got), plain);
    }

    /// The badge's glyph and word wear the delivery's group color; the
    /// qualifier after the separator stays dim. Two painters compose one
    /// string (grid dims the qualifier, this crate wraps the whole badge),
    /// so the byte layout that produces is pinned here. A grid badge that
    /// ever put an unpainted word AFTER a dim run would lose its color
    /// silently, and this is what catches it.
    #[test]
    fn badge_paint_colors_the_head_and_dims_the_qualifier() {
        let (theme, warnings) = cyclops_theme::Theme::parse(
            concat!(
                "[badge]\n",
                "terminal = { hex = \"#010203\", c256 = 21 }\n",
                "quiet = { hex = \"#040506\", c256 = 23 }\n",
                "[surface]\n",
                "dim = { hex = \"#070809\", c256 = 22 }\n",
            ),
            "test",
        )
        .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let s = Style::with_theme(theme, false);

        // Head in the group color, separator and qualifier dim.
        let got = receipt_badge(&receipt(DeliveryState::ParkedBlockedQuota, None, None), &s);
        let head = grid::receipt_badge(
            &receipt(DeliveryState::ParkedBlockedQuota, None, None),
            &grid::Plain,
        );
        let head = head
            .split(" · ")
            .next()
            .expect("a head before the separator");
        assert_eq!(
            got,
            format!("\x1b[38;5;21m{head} \x1b[38;5;22m·\x1b[0m \x1b[38;5;22mquota\x1b[0m\x1b[0m")
        );

        // No qualifier: the whole badge is one run of the group color.
        let got = receipt_badge(&receipt(DeliveryState::Gating, None, None), &s);
        assert_eq!(got, "\x1b[38;5;23m● gating\x1b[0m");
    }

    /// The badge words live in cyclops_ui::grid and are pinned there.
    /// What this crate owns is the paint, so this pins the paint: a badge
    /// with color off is the words and not one escape byte.
    #[test]
    fn badges_paint_through_this_surfaces_dim() {
        let plain = receipt_badge(
            &receipt(DeliveryState::ParkedBlockedQuota, None, None),
            &Style::none(),
        );
        assert_eq!(plain, "⊘ parked · quota");
        let painted = receipt_badge(
            &receipt(DeliveryState::ParkedBlockedQuota, None, None),
            &Style::detect(false),
        );
        // Style::detect off a pipe is colorless, which is the machine
        // path; the colored form is pinned in style.rs.
        assert_eq!(painted, plain);
    }

    #[test]
    fn single_receipt_is_a_bare_badge_line() {
        let rs = [receipt(DeliveryState::DeliveredVerified, None, None)];
        assert_eq!(
            render_receipts(&rs, &Style::none()),
            "✔ delivered · verified"
        );
    }

    /// [`check`] is the one place the rule lives, so every surface that
    /// prints a check reads its glyph from there. This test is what makes
    /// that true of the surfaces whose glyph comes from somewhere else:
    /// the delivery badges are `cyclops_ui::grid`'s, and a hook-verified
    /// one has to be the confirmed glyph while a screen-tier one has to
    /// be the other.
    #[test]
    fn every_check_on_every_surface_reads_the_same_rule() {
        let s = Style::none();
        assert_ne!(check(true), check(false), "the weights have to differ");

        let verified = receipt_badge(&receipt(DeliveryState::DeliveredVerified, None, None), &s);
        let unverified =
            receipt_badge(&receipt(DeliveryState::DeliveredUnverified, None, None), &s);
        assert!(verified.starts_with(check(true)), "{verified}");
        assert!(unverified.starts_with(check(false)), "{unverified}");

        // The daemon wrote the registry and the ledger before answering.
        let named = render_named(&json!({"pane_id": "%3", "label": "reviewer"}), &s);
        assert!(named.starts_with(check(true)), "{named}");
        let cleared = render_named(&json!({"pane_id": "%3", "label": null}), &s);
        assert!(cleared.starts_with(check(true)), "{cleared}");

        // The daemon answered this round trip itself.
        assert!(render_ping(0.4, &s).starts_with(check(true)));

        // A wait that reached with no state to show for it: nothing there
        // was confirmed by anyone.
        let reached = wait_badge("reached", None, None, None, &s);
        assert!(reached.starts_with(check(false)), "{reached}");
    }

    /// One cause, one wording, on both surfaces that show it.
    ///
    /// The receipt used to be worded by the daemon and history by
    /// `cause_words`, so the same refused delivery read "nothing detects
    /// %1" while the record line under it read "no manifest". Two homes,
    /// and the drift was visible in one screenful. Both now come out of
    /// `cyclops_ui::grid`, and the only difference left is the identifier
    /// a receipt has and a folded record line does not.
    #[test]
    fn a_refused_delivery_reads_the_same_on_the_receipt_and_the_record() {
        let s = Style::none();
        let mut r = receipt(DeliveryState::AttentionRequired, None, Some("no_manifest"));
        r.pane = Some("%1".into());
        assert_eq!(
            receipt_badge(&r, &s),
            "⚠ needs attention · nothing detects %1"
        );

        let recorded = Delivery {
            to: "reviewer".into(),
            state: DeliveryState::AttentionRequired,
            verified_by: None,
            attempts: 1,
            ts: 0,
            cause: Some("no_manifest".into()),
        };
        assert_eq!(
            delivery_badge(&recorded, &s),
            "⚠ needs attention · nothing detects its pane"
        );

        // The machine cause never faces a reader on either surface.
        assert!(!receipt_badge(&r, &s).contains("no_manifest"));
        assert!(!delivery_badge(&recorded, &s).contains("no_manifest"));
    }

    #[test]
    fn broadcast_grid_is_exact_and_aligned() {
        let mut a = receipt(DeliveryState::DeliveredVerified, None, None);
        a.to = "reviewer".into();
        let mut b = receipt(DeliveryState::Queued, Some(2), None);
        b.to = "implementer".into();
        let got = render_receipts(&[a, b], &Style::none());
        let expected = "\x20 reviewer     ✔ delivered · verified\n\
                        \x20 implementer  ● queued · 2 ahead";
        assert_eq!(got, expected);
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in: {line:?}");
        }
    }

    #[test]
    fn wait_badges_are_exact_in_plain_mode() {
        let s = Style::none();
        let cases = [
            (
                ("reached", Some(AgentState::Idle), Some(3000u64), None),
                "○ idle · waited 3s",
            ),
            (
                (
                    "reached",
                    Some(AgentState::BlockedPermission),
                    Some(10_000),
                    None,
                ),
                "⚠ blocked_permission · waited 10s",
            ),
            (
                ("timeout", Some(AgentState::Working), Some(60_000), None),
                "⚠ wait timed out · still working",
            ),
            (("timeout", None, None, None), "⚠ wait timed out"),
            (
                ("occupant_changed", None, Some(1200), None),
                "✗ pane changed occupant · wait abandoned",
            ),
            (
                ("not_delivered", None, Some(0), Some("attention_required")),
                "⚠ not delivered · attention_required",
            ),
        ];
        for ((outcome, state, ms, delivery), want) in cases {
            assert_eq!(wait_badge(outcome, state, ms, delivery, &s), want);
        }
    }

    #[test]
    fn ping_line_is_exact() {
        assert_eq!(render_ping(0.42, &Style::none()), "✔ cyclops is up · 0.4ms");
        assert_eq!(
            render_ping(12.0, &Style::none()),
            "✔ cyclops is up · 12.0ms"
        );
    }

    #[test]
    fn detection_view_is_exact() {
        let now = 1_754_000_010_000;
        let det = Detection {
            state: AgentState::Working,
            disagreement: true,
            decided_by: "hook:Stop".into(),
            stale: false,
            write_ready: false,
            write_block: None,
            composer_semantic: None,
            readings: vec![
                SensorReading {
                    sensor: Sensor::Hook,
                    state: AgentState::Working,
                    rule: "Stop".into(),
                    ts: now - 2000,
                },
                SensorReading {
                    sensor: Sensor::Title,
                    state: AgentState::Working,
                    rule: "spinner".into(),
                    ts: now - 2000,
                },
                SensorReading {
                    sensor: Sensor::Screen,
                    state: AgentState::Idle,
                    rule: "prompt_visible".into(),
                    ts: now - 8000,
                },
            ],
        };
        let got = render_detection("reviewer", &det, &Style::none(), now);
        let expected = "reviewer · ● working · ⚠ sensors disagree · decided by hook:Stop · not write-ready: unstamped\n\
             \n\
             \x20 hook    ● working  Stop            2s ago\n\
             \x20 title   ● working  spinner         2s ago\n\
             \x20 screen  ○ idle     prompt_visible  8s ago";
        assert_eq!(got, expected);
    }

    #[test]
    fn event_line_gutter_and_summary() {
        let ev = Event {
            event: "agent.state".into(),
            data: json!({"ts": 43_471_000u64, "agent": "reviewer", "state": "working"}),
            seq: None,
        };
        assert_eq!(
            render_event_line(&ev, &Style::none(), 0),
            "12:04:31  agent.state  reviewer · ● working"
        );
    }

    #[test]
    fn event_line_unknown_payload_falls_back_to_json() {
        let ev = Event {
            event: "daemon.selftest".into(),
            data: json!({"ts": 0u64, "passed": true}),
            seq: None,
        };
        assert_eq!(
            render_event_line(&ev, &Style::none(), 0),
            "00:00:00  daemon.selftest  {\"passed\":true}"
        );
        let bare = Event {
            event: "daemon.reconciled".into(),
            data: Value::Null,
            seq: None,
        };
        assert_eq!(
            render_event_line(&bare, &Style::none(), 3_600_000),
            "01:00:00  daemon.reconciled"
        );
    }

    #[test]
    fn pad_pads_by_display_width_not_bytes() {
        // "日" is 3 bytes but 2 columns; pad to 4 adds 2 spaces.
        assert_eq!(pad("日", 4), "日  ");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcd", 2), "abcd");
    }

    #[test]
    fn durations_read_like_a_human_wrote_them() {
        assert_eq!(human_duration(42_000), "42s");
        assert_eq!(human_duration(120_000), "2m");
        assert_eq!(human_duration(3_600_000), "1h");
        assert_eq!(human_duration(11_520_000), "3h 12m");
        assert_eq!(human_duration(180_000_000), "2d 2h");
    }

    fn ledger_msg(
        id: &str,
        ts: u64,
        kind: Kind,
        from: &str,
        to: &[&str],
        subject: &str,
        deliveries: &[(&str, DeliveryState, Option<&str>)],
    ) -> LedgerLine {
        LedgerLine {
            seq: 1,
            boot_id: "b".into(),
            id: id.into(),
            ts,
            kind,
            from: from.into(),
            to: to.iter().map(|t| t.to_string()).collect(),
            subject: Some(subject.into()),
            body: None,
            reply_to: None,
            deliveries: deliveries
                .iter()
                .map(|(to, state, cause)| Delivery {
                    to: to.to_string(),
                    state: *state,
                    verified_by: None,
                    attempts: 1,
                    ts,
                    cause: cause.map(String::from),
                })
                .collect(),
            data: None,
        }
    }

    #[test]
    fn history_gutter_is_relative_under_24h_then_a_date() {
        let now = 100 * 86_400_000; // 1970-04-11 UTC
        assert_eq!(history_gutter(now - 42_000, now), "42s");
        assert_eq!(history_gutter(now - 125 * 60_000, now), "2h 5m");
        assert_eq!(history_gutter(now - 86_399_999, now), "23h 59m");
        // 24h exactly tips into the date form; same year drops the year.
        assert_eq!(history_gutter(now - 86_400_000, now), "Apr 10");
        assert_eq!(history_gutter(now - 3 * 86_400_000, now), "Apr 8");
        // A different year names it.
        let next_year = now + 366 * 86_400_000;
        assert_eq!(history_gutter(0, next_year), "Jan 1 1970");
        // Clock skew (future ts) reads as just written, never a date.
        assert_eq!(history_gutter(now + 5_000, now), "0s");
    }

    #[test]
    fn history_grid_plain_is_exact() {
        let now = 100 * 86_400_000;
        let lines = vec![
            ledger_msg(
                "m-ccc",
                now - 3 * 86_400_000,
                Kind::Msg,
                "reviewer",
                &["codex"],
                "Re: rate limiter",
                &[("codex", DeliveryState::DeliveredUnverified, None)],
            ),
            ledger_msg(
                "m-bbb",
                now - 120_000,
                Kind::Fyi,
                "admin",
                &["reviewer", "implementer"],
                "Standup in 5",
                &[
                    ("reviewer", DeliveryState::DeliveredVerified, None),
                    ("implementer", DeliveryState::Queued, None),
                ],
            ),
            ledger_msg(
                "m-aaa",
                now - 42_000,
                Kind::Msg,
                "codex",
                &["reviewer"],
                "Review the rate limiter",
                &[("reviewer", DeliveryState::DeliveredVerified, None)],
            ),
        ];
        let got = render_history(&lines, &Style::none(), now);
        let expected = "\x20 Apr 8  reviewer → codex       Re: rate limiter         ✓ delivered · unverified (screen)\n\
                        \x20    2m  admin → 2 agents  fyi  Standup in 5\n\
                        \x20        reviewer     ✔ delivered · verified\n\
                        \x20        implementer  ● queued\n\
                        \x20   42s  codex → reviewer       Review the rate limiter  ✔ delivered · verified";
        assert_eq!(got, expected);
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in: {line:?}");
        }
    }

    #[test]
    fn history_without_fyi_lines_drops_the_tag_column() {
        let now = 100 * 86_400_000;
        let lines = vec![ledger_msg(
            "m-aaa",
            now - 42_000,
            Kind::Msg,
            "codex",
            &["reviewer"],
            "Ping",
            &[("reviewer", DeliveryState::DeliveredVerified, None)],
        )];
        assert_eq!(
            render_history(&lines, &Style::none(), now),
            "\x20 42s  codex → reviewer  Ping  ✔ delivered · verified"
        );
    }

    #[test]
    fn thread_plain_is_exact_and_skips_chain_lines() {
        let now = 100 * 86_400_000;
        let mut state_line = ledger_msg(
            "m-aaa",
            now - 3_000_000,
            Kind::State,
            "cyclopsd",
            &["reviewer"],
            "",
            &[("reviewer", DeliveryState::Gating, None)],
        );
        state_line.subject = None;
        let mut ask = ledger_msg(
            "m-aaa",
            now - 3_600_000,
            Kind::Msg,
            "codex",
            &["reviewer"],
            "Review the rate limiter",
            &[("reviewer", DeliveryState::DeliveredVerified, None)],
        );
        ask.body = Some("gateway.rs:120 drops the burst path".into());
        let mut reply = ledger_msg(
            "m-ccc",
            now - 120_000,
            Kind::Msg,
            "reviewer",
            &["codex"],
            "Re: Review the rate limiter",
            &[("codex", DeliveryState::DeliveredVerified, None)],
        );
        reply.body = Some("Done. One nit.".into());
        let lines = vec![ask, state_line, reply];
        let got = render_thread(&lines, &Style::none(), now);
        let expected = "\x20 1h  codex → reviewer  Review the rate limiter      ✔ delivered · verified\n\
                        \x20     gateway.rs:120 drops the burst path\n\
                        \n\
                        \x20 2m  reviewer → codex  Re: Review the rate limiter  ✔ delivered · verified\n\
                        \x20     Done. One nit.";
        assert_eq!(got, expected);
    }

    #[test]
    fn detail_prefers_informative_title_then_command() {
        let p = pane(
            "%9",
            Some("x"),
            Some("claude"),
            "Fix the bug",
            "claude",
            AgentState::Idle,
        );
        assert_eq!(detail_for(&p, "x"), Some("Fix the bug".into()));
        let p = pane(
            "%9",
            Some("x"),
            Some("claude"),
            "x",
            "claude",
            AgentState::Idle,
        );
        assert_eq!(detail_for(&p, "x"), None);
        let p = pane("%9", None, None, "", "htop", AgentState::Unknown);
        assert_eq!(detail_for(&p, "%9"), Some("htop".into()));
        // Title matching the window name is tmux default noise.
        let mut p = pane("%9", None, None, "agents", "zsh", AgentState::Unknown);
        p.window_name = "agents".into();
        assert_eq!(detail_for(&p, "%9"), Some("zsh".into()));
    }
}
