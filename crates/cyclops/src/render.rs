//! Renderers: strict grid, computed column widths, two-space gutters, no
//! trailing spaces. Pads by display width, not bytes. States always render
//! glyph plus word; color never carries meaning alone.

use std::path::Path;

use serde_json::Value;

use cyclops_proto::{AgentState, Detection, Event, PaneStatus, Sensor, StatusResult};

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
            let detail = detail_for(p, &label);
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
