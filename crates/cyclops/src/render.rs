//! Renderers: strict grid, computed column widths, two-space gutters, no
//! trailing spaces. Pads by display width, not bytes. States always render
//! glyph plus word; color never carries meaning alone.

use std::path::Path;

use serde_json::Value;

use cyclops_proto::{
    AgentState, Delivery, DeliveryReceipt, DeliveryState, Detection, Event, Kind, LedgerLine,
    PaneStatus, Sensor, StatusResult,
};

use crate::style::Style;

/// Display width of one char, covering the glyph set cyclops prints plus
/// the broad wide ranges pane titles can carry (CJK, emoji). Deliberately
/// not a full UAX-11 table: the daemon controls most strings here and the
/// crate stays dependency-free.
pub fn char_width(c: char) -> usize {
    match c {
        // Combining marks occupy no column.
        '\u{0300}'..='\u{036f}' => 0,
        // The one wide glyph in our own set: ⛔ (blocked_quota).
        '\u{26d4}' => 2,
        // Common wide blocks: Hangul jamo, CJK, Hangul syllables,
        // compatibility ideographs, vertical forms, fullwidth forms, emoji.
        '\u{1100}'..='\u{115f}'
        | '\u{2e80}'..='\u{a4cf}'
        | '\u{ac00}'..='\u{d7a3}'
        | '\u{f900}'..='\u{faff}'
        | '\u{fe30}'..='\u{fe4f}'
        | '\u{ff00}'..='\u{ff60}'
        | '\u{ffe0}'..='\u{ffe6}'
        | '\u{1f300}'..='\u{1faff}' => 2,
        _ => 1,
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Pad with spaces to `width` display columns. Never truncates.
pub fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Glyph plus word, the only state encoding. "● working", "○ idle".
pub fn state_cell(s: AgentState) -> String {
    format!("{} {s}", s.glyph())
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
    session: usize,
}

/// Detail column: the pane title when it says something the row doesn't
/// already say, else the running command, else nothing. Hostname-style
/// title noise (F5: agy publishes the hostname) is the daemon's problem to
/// filter; the client only drops exact repeats. The agent binary as a
/// command repeats the manifest, so it is suppressed too.
fn detail_for(p: &PaneStatus, label: &str) -> Option<String> {
    let title = p.title.trim();
    if !title.is_empty() && title != label && title != p.current_command && title != p.window_name {
        return Some(title.to_string());
    }
    let cmd = p.current_command.trim();
    if cmd.is_empty() || cmd == label || Some(cmd) == p.manifest.as_deref() {
        return None;
    }
    Some(cmd.to_string())
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
    let header = format!(
        "{} {} {sep} watching {watching} {sep} tmux {} {sep} up {}",
        style.accent("◉"),
        style.bold("cyclops"),
        res.tmux_version,
        human_duration(res.uptime_ms),
    );

    if res.sessions.is_empty() {
        // Empty state invites the next action instead of erroring.
        return format!(
            "{header}\n\n  No sessions yet. Name one in {} and cyclops will pick it up.",
            config_path.display()
        );
    }

    let mut rows: Vec<Row> = Vec::new();
    for (si, sess) in res.sessions.iter().enumerate() {
        for p in &sess.panes {
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
                session: si,
            });
        }
    }

    let label_w = rows
        .iter()
        .map(|r| display_width(&r.label))
        .max()
        .unwrap_or(0);
    let state_w = rows
        .iter()
        .map(|r| display_width(&state_cell(r.state)))
        .max()
        .unwrap_or(0);

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
            let cell = state_cell(r.state);
            let line = match &r.detail {
                Some(d) => format!("  {label}  {}  {}", pad(&cell, state_w), style.dim(d)),
                None => format!("  {label}  {cell}"),
            };
            out.push(line);
        }
    }
    out.join("\n")
}

/// One receipt badge, the landing-page voice: glyph plus word, qualifier
/// after a dim separator. In-flight pipeline states should not reach a
/// receipt, but every DeliveryState renders rather than panicking on a
/// daemon that answers mid-pipeline.
///
/// The check carries the evidence tier as weight: heavy ✔ for
/// hook-verified, light ✓ for screen-tier. GOALS asks for a hollow check
/// on unverified; no portable hollow check glyph exists in terminal
/// fonts, so weight is the pair (STATUS.md deviations).
pub fn receipt_badge(r: &DeliveryReceipt, style: &Style) -> String {
    let sep = style.dim("·");
    let with = |head: &str, tail: &str| format!("{head} {sep} {}", style.dim(tail));
    match r.state {
        DeliveryState::Queued => match r.position {
            Some(n) => with("● queued", &format!("{n} ahead")),
            None => "● queued".into(),
        },
        DeliveryState::Gating => "● gating".into(),
        DeliveryState::Pasting => "● pasting".into(),
        DeliveryState::Staged => "● staged".into(),
        DeliveryState::Submitted => "● submitted".into(),
        DeliveryState::RetryQueued => "● retrying".into(),
        DeliveryState::DeliveredVerified => with("✔ delivered", "verified"),
        DeliveryState::DeliveredUnverified => with("✓ delivered", "unverified (screen)"),
        DeliveryState::AttentionRequired => match &r.note {
            Some(note) => with("⚠ needs attention", note),
            None => "⚠ needs attention".into(),
        },
        DeliveryState::ParkedBlockedQuota => match &r.note {
            Some(note) => with("⛔ parked", &format!("quota, {note}")),
            None => with("⛔ parked", "quota"),
        },
    }
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
            let head = match state {
                Some(s) => state_cell(s),
                None => "✓ reached".to_string(),
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

/// The `wait` array of a send-and-wait receipt. One recipient renders a
/// bare badge line prefixed `wait:`; a broadcast renders the same aligned
/// grid as receipts.
pub fn render_wait_entries(entries: &[Value], style: &Style) -> String {
    let badge = |e: &Value| {
        wait_badge(
            e["outcome"].as_str().unwrap_or_default(),
            serde_json::from_value::<AgentState>(e["state"].clone()).ok(),
            e["waited_ms"].as_u64(),
            e["delivery"].as_str(),
            style,
        )
    };
    if entries.len() == 1 {
        return format!("wait: {}", badge(&entries[0]));
    }
    let to_of = |e: &Value| e["to"].as_str().unwrap_or_default().to_string();
    let to_w = entries
        .iter()
        .map(|e| display_width(&to_of(e)))
        .max()
        .unwrap_or(0);
    entries
        .iter()
        .map(|e| {
            let to = to_of(e);
            format!("  {}  {}", style.role(&to, &pad(&to, to_w)), badge(e))
        })
        .collect::<Vec<_>>()
        .join("\n")
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

/// Machine causes in user-side words for badges. The generic fallback
/// swaps underscores for spaces, which reads fine for the rest.
fn cause_words(cause: &str) -> String {
    match cause {
        "no_such_pane" => "no pane with that name".into(),
        "daemon_restart" => "daemon restarted mid-delivery".into(),
        _ => cause.replace('_', " "),
    }
}

/// One folded ledger delivery in the M1 badge voice.
pub fn delivery_badge(d: &Delivery, style: &Style) -> String {
    let note = match d.state {
        DeliveryState::AttentionRequired => d.cause.as_deref().map(cause_words),
        _ => None,
    };
    receipt_badge(
        &DeliveryReceipt {
            to: d.to.clone(),
            state: d.state,
            position: None,
            note,
        },
        style,
    )
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

pub fn render_ping(rtt_ms: f64, style: &Style) -> String {
    format!(
        "✓ cyclops is up {} {}",
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
        state_cell(det.state)
    );
    if det.disagreement {
        header.push_str(&format!(" {sep} ⚠ sensors disagree"));
    }
    header.push_str(&format!(
        " {sep} {}",
        style.dim(&format!("decided by {}", det.decided_by))
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
        .map(|r| display_width(&state_cell(r.state)))
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
            pad(&state_cell(r.state), state_w),
            pad(&r.rule, rule_w),
            style.dim(&age(now_ms.saturating_sub(r.ts))),
        ));
    }
    out.join("\n")
}

/// HH:MM:SS in UTC. std ships no timezone database; a local-time gutter
/// needs a tz crate and M0 does not buy one for this.
fn clock_hms(ts_ms: u64) -> String {
    let s = (ts_ms / 1000) % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// One event, one line: timestamp gutter, event name, then whatever the
/// payload can say in user-side words.
pub fn render_event_line(ev: &Event, style: &Style, now_ms: u64) -> String {
    let ts = ev.data.get("ts").and_then(Value::as_u64).unwrap_or(now_ms);
    let mut line = format!("{}  {}", style.dim(&clock_hms(ts)), ev.event);
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
            parts.push(state_cell(st));
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
            width: 120,
            height: 40,
            state,
            hooks_verified: None,
        }
    }

    fn fixture() -> StatusResult {
        StatusResult {
            daemon_version: "0.1.0".into(),
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
        }
    }

    #[test]
    fn status_grid_plain_is_exact() {
        let got = render_status(&fixture(), &Style::none(), Path::new("/x/config.toml"));
        let expected = "◉ cyclops · watching main · tmux 3.6a · up 2m\n\
                        \n\
                        \x20 reviewer     ● working  Run the tests\n\
                        \x20 implementer  ○ idle\n\
                        \x20 %4           ? unknown  vim";
        assert_eq!(got, expected);
    }

    #[test]
    fn status_marks_hook_unverified_panes() {
        let mut res = fixture();
        // reviewer has an informative title: the marker appends after it.
        res.sessions[0].panes[0].hooks_verified = Some(false);
        // implementer has no detail: the marker stands alone.
        res.sessions[0].panes[1].hooks_verified = Some(false);
        let got = render_status(&res, &Style::none(), Path::new("/x/config.toml"));
        let expected = "◉ cyclops · watching main · tmux 3.6a · up 2m\n\
                        \n\
                        \x20 reviewer     ● working  Run the tests · hooks unverified\n\
                        \x20 implementer  ○ idle     hooks unverified\n\
                        \x20 %4           ? unknown  vim";
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
        let expected = "◉ cyclops · watching nothing · tmux 3.6a · up 2m\n\
                        \n\
                        \x20 No sessions yet. Name one in /x/config.toml and cyclops will pick it up.";
        assert_eq!(got, expected);
    }

    fn receipt(state: DeliveryState, position: Option<u32>, note: Option<&str>) -> DeliveryReceipt {
        DeliveryReceipt {
            to: "reviewer".into(),
            state,
            position,
            note: note.map(String::from),
        }
    }

    #[test]
    fn receipt_badges_are_exact_in_plain_mode() {
        use DeliveryState::*;
        let s = Style::none();
        let cases = [
            (
                receipt(DeliveredVerified, None, None),
                "✔ delivered · verified",
            ),
            (
                receipt(DeliveredUnverified, None, None),
                "✓ delivered · unverified (screen)",
            ),
            (receipt(Queued, Some(2), None), "● queued · 2 ahead"),
            (receipt(Queued, None, None), "● queued"),
            (
                receipt(ParkedBlockedQuota, None, Some("resets in 135h")),
                "⛔ parked · quota, resets in 135h",
            ),
            (receipt(ParkedBlockedQuota, None, None), "⛔ parked · quota"),
            (
                receipt(AttentionRequired, None, Some("target pane is gone")),
                "⚠ needs attention · target pane is gone",
            ),
            (receipt(AttentionRequired, None, None), "⚠ needs attention"),
            (receipt(Gating, None, None), "● gating"),
            (receipt(Pasting, None, None), "● pasting"),
            (receipt(Staged, None, None), "● staged"),
            (receipt(Submitted, None, None), "● submitted"),
            (receipt(RetryQueued, None, None), "● retrying"),
        ];
        for (r, want) in &cases {
            assert_eq!(receipt_badge(r, &s), *want);
        }
    }

    #[test]
    fn single_receipt_is_a_bare_badge_line() {
        let rs = [receipt(DeliveryState::DeliveredVerified, None, None)];
        assert_eq!(
            render_receipts(&rs, &Style::none()),
            "✔ delivered · verified"
        );
    }

    #[test]
    fn delivered_check_weight_pairs_heavy_verified_light_unverified() {
        // GOALS "hollow check = unverified", implemented as weight: the
        // two delivered badges must never share a glyph.
        let s = Style::none();
        let verified = receipt_badge(&receipt(DeliveryState::DeliveredVerified, None, None), &s);
        let unverified =
            receipt_badge(&receipt(DeliveryState::DeliveredUnverified, None, None), &s);
        assert!(verified.starts_with('✔'), "{verified}");
        assert!(unverified.starts_with('✓'), "{unverified}");
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
    fn wait_entries_render_single_line_and_grid() {
        let s = Style::none();
        let single = vec![serde_json::json!({
            "to": "reviewer", "outcome": "reached", "state": "idle",
            "waited_ms": 3000, "delivery": "delivered_verified",
        })];
        assert_eq!(render_wait_entries(&single, &s), "wait: ○ idle · waited 3s");
        let multi = vec![
            serde_json::json!({
                "to": "reviewer", "outcome": "reached", "state": "idle",
                "waited_ms": 3000, "delivery": "delivered_verified",
            }),
            serde_json::json!({
                "to": "implementer", "outcome": "timeout", "state": "working",
                "waited_ms": 60_000, "delivery": "delivered_unverified",
            }),
        ];
        let got = render_wait_entries(&multi, &s);
        let expected = "\x20 reviewer     ○ idle · waited 3s\n\
                        \x20 implementer  ⚠ wait timed out · still working";
        assert_eq!(got, expected);
        for line in got.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in: {line:?}");
        }
    }

    #[test]
    fn ping_line_is_exact() {
        assert_eq!(render_ping(0.42, &Style::none()), "✓ cyclops is up · 0.4ms");
        assert_eq!(
            render_ping(12.0, &Style::none()),
            "✓ cyclops is up · 12.0ms"
        );
    }

    #[test]
    fn detection_view_is_exact() {
        let now = 1_754_000_010_000;
        let det = Detection {
            state: AgentState::Working,
            disagreement: true,
            decided_by: "hook:Stop".into(),
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
        let expected = "reviewer · ● working · ⚠ sensors disagree · decided by hook:Stop\n\
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
    fn width_accounts_for_wide_and_combining_chars() {
        assert_eq!(display_width("● working"), 9);
        assert_eq!(display_width("⛔ blocked_quota"), 16);
        assert_eq!(display_width("⚠"), 1);
        // The delivered pair shares one column width, so grids stay aligned.
        assert_eq!(display_width("✔"), 1);
        assert_eq!(display_width("✓"), 1);
        assert_eq!(display_width("日本"), 4);
        // e + combining acute renders in one column.
        assert_eq!(display_width("e\u{0301}"), 1);
        assert_eq!(display_width("🚀"), 2);
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
    fn attention_badge_carries_cause_words() {
        let s = Style::none();
        let d = Delivery {
            to: "reviewer".into(),
            state: DeliveryState::AttentionRequired,
            verified_by: None,
            attempts: 2,
            ts: 0,
            cause: Some("no_such_pane".into()),
        };
        assert_eq!(
            delivery_badge(&d, &s),
            "⚠ needs attention · no pane with that name"
        );
        let parked = Delivery {
            to: "reviewer".into(),
            state: DeliveryState::ParkedBlockedQuota,
            verified_by: None,
            attempts: 0,
            ts: 0,
            cause: Some("blocked_quota".into()),
        };
        assert_eq!(delivery_badge(&parked, &s), "⛔ parked · quota");
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
