//! One adapter-owned snapshot of every session, window, and pane on the
//! server, gathered with a small fixed number of control-mode commands
//! instead of one one-shot tmux process per window.
//!
//! `src/cyclops-workspace/src/sync.rs`'s `fetch_workspace_model` used to
//! build the workspace model from `list-sessions`, an all-window membership
//! query, `list-windows`, and one `list-panes` call *per window* — `W + 3`
//! one-shot tmux processes for a session with `W` windows (MEASURED in
//! `src/cyclops-workspace/tests/baseline.rs`: 10-15ms per extra window).
//! [`ControlClient::workspace_snapshot`] replaced that fan-out with three
//! formatted commands over the control client that is already connected,
//! none of which scales with window or pane count.
//!
//! ## Why three commands, not one
//!
//! Every tmux session has at least one window, and every window at least one
//! pane — VERIFIED directly against tmux 3.7b: killing a session's only pane
//! does not leave an empty session, it leaves no server at all ("no server
//! running on ..."). So a single `list-panes -a` (every pane, every session)
//! is structurally enough to discover every session, window, and pane that
//! exists; nothing needs a per-window follow-up.
//!
//! The reason for additional commands is escaping, not reachability. Session
//! names and window names are both arbitrary human text, and both can appear
//! on the same `list-panes -a` line. This crate's strongest precedent for a
//! free-text field ([`crate::watcher`]'s `PANE_FORMAT`, which carries
//! `pane_title`) makes exactly one field per line safe against an embedded
//! tab: position it last, so `splitn` hands back everything after the last
//! *known-safe* tab as one piece, remainder tabs included. Two independent
//! arbitrary fields cannot both be "last" on the same line — whichever one
//! sits earlier is still exposed to a tab inside it shifting every field
//! after it. Rather than accept that exposure for both names, `window_name`
//! keeps the safe last slot on the `list-panes -a` line, and `session_name`
//! gets its own `list-sessions` line where it is the only, and therefore
//! safely last, free-text field. Pane option `@cyclops_pane_minimized_v1`
//! carries arbitrary option bytes that may also contain tabs, and is safely
//! queried in a dedicated second `list-panes -a` command as the trailing field.
//! All three commands are bounded by session count and window count respectively:
//! as *data volume*, which was never the problem; the baseline's `W + 3` was
//! `W + 3` one-shot tmux *processes*, and this is three, regardless of `W`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::control::ControlClient;
use crate::error::TmuxError;
use crate::quote::quote_arg;

/// Recorded pre-minimize provenance for a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneMinimizationProvenance {
    /// No minimization option set on this pane.
    None,
    /// Deliberately minimized pane with verified pre-collapse height.
    Minimized { original_height: u16 },
    /// Non-empty option payload present but corrupted or invalid.
    Malformed(String),
}

impl PaneMinimizationProvenance {
    /// Parse from raw tmux option string with byte-exact validation (no trimming).
    pub fn parse(raw: &str) -> Self {
        if raw.is_empty() {
            return Self::None;
        }
        if let Some(rest) = raw.strip_prefix("v1:") {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(height) = rest.parse::<u16>() {
                    if height >= 2 {
                        return Self::Minimized {
                            original_height: height,
                        };
                    }
                }
            }
            return Self::Malformed(raw.to_string());
        }
        Self::Malformed(raw.to_string())
    }

    pub fn is_minimized(&self) -> bool {
        matches!(self, Self::Minimized { .. })
    }

    pub fn is_malformed(&self) -> bool {
        matches!(self, Self::Malformed(_))
    }

    pub fn original_height(&self) -> Option<u16> {
        match self {
            Self::Minimized { original_height } => Some(*original_height),
            _ => None,
        }
    }
}

/// One pane on a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPane {
    pub id: String,
    /// Zero-based pane index in the window.
    pub index: usize,
    pub active: bool,
    pub width: u32,
    pub height: u32,
    /// Recorded pre-minimize provenance if deliberately minimized by operator.
    pub minimization: PaneMinimizationProvenance,
}

/// One window in a [`SnapshotSession`], with its panes in pane-index order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotWindow {
    /// Window id, e.g. `@1`.
    pub id: String,
    /// Zero-based window index in the session.
    pub index: usize,
    pub name: String,
    /// Raw tmux layout string (`#{window_layout}`).
    pub layout: String,
    pub active: bool,
    pub zoomed: bool,
    pub panes: Vec<SnapshotPane>,
}

/// One session in a [`WorkspaceSnapshot`], with its windows in window-index
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSession {
    /// Stable tmux session id, e.g. `$0`.
    pub id: String,
    pub name: String,
    pub attached: bool,
    pub windows: Vec<SnapshotWindow>,
}

/// Every session, window, and pane on the server, as of one
/// [`ControlClient::workspace_snapshot`] call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceSnapshot {
    /// Sessions in `list-sessions` order (tmux's own session order); a
    /// session that only `list-panes -a` saw — created or destroyed in the
    /// gap between the two commands — is appended at the end rather than
    /// dropped, so a race costs ordering, never data. The next reconcile
    /// (structural notifications remain the trigger) settles it.
    pub sessions: Vec<SnapshotSession>,
}

/// `list-panes -a` format for structural layout. Every field except the last
/// is a tmux-generated id, index, flag, or layout string. None of those can
/// contain a tab. `window_name` is deliberately last: `splitn` with the exact
/// field count hands back everything past the final known-safe tab as one
/// piece, so a tab embedded in a window name lands inside the name instead of
/// corrupting fields.
const SNAPSHOT_PANE_FORMAT: &str = "#{session_id}\t#{session_attached}\t#{window_id}\t#{window_index}\t#{window_active}\t#{window_zoomed_flag}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_width}\t#{pane_height}\t#{window_layout}\t#{window_name}";

/// Number of tab-separated fields in [`SNAPSHOT_PANE_FORMAT`].
const SNAPSHOT_PANE_FIELDS: usize = 13;

/// `list-panes -a` format for pane options. `pane_id` is first (contains no tabs),
/// and `@cyclops_pane_minimized_v1` is last, so arbitrary option values containing
/// literal tabs swallow into the option payload without shifting structural fields.
const SNAPSHOT_PANE_OPTIONS_FORMAT: &str = "#{pane_id}\t#{@cyclops_pane_minimized_v1}";

/// `list-sessions` format for names only. `session_id` and `session_attached`
/// already travel on every `SNAPSHOT_PANE_FORMAT` line; this command exists
/// solely to carry `session_name` without putting two arbitrary-text fields
/// on one `list-panes -a` line (see the module doc). `session_name` is last,
/// so it is the one field this line lets swallow an embedded tab safely.
const SNAPSHOT_SESSION_NAME_FORMAT: &str = "#{session_id}\t#{session_name}";

impl ControlClient {
    /// Build the whole server's session/window/pane tree with formatted
    /// commands over this already-connected client.
    ///
    /// Neither command's cost scales with window or pane count: each is one
    /// control-mode round trip regardless of how many lines it returns.
    pub async fn workspace_snapshot(&self) -> Result<WorkspaceSnapshot, TmuxError> {
        let pane_lines = self
            .command(&format!(
                "list-panes -a -F {}",
                quote_arg(SNAPSHOT_PANE_FORMAT)
            ))
            .await?;
        let option_lines = self
            .command(&format!(
                "list-panes -a -F {}",
                quote_arg(SNAPSHOT_PANE_OPTIONS_FORMAT)
            ))
            .await?;
        let session_lines = self
            .command(&format!(
                "list-sessions -F {}",
                quote_arg(SNAPSHOT_SESSION_NAME_FORMAT)
            ))
            .await?;

        let options_by_pane = parse_pane_options(&option_lines);
        let (session_order, session_names) = parse_session_names(&session_lines)?;

        // Fold every pane line into its window, and every window into its
        // session. list-panes -a repeats window_id/window_name/layout once
        // per pane in that window; the first line seen for a window sets
        // the window-level fields, later lines for the same window only add
        // another pane.
        let mut attached: HashMap<String, bool> = HashMap::new();
        let mut windows_by_session: HashMap<String, HashMap<String, SnapshotWindow>> =
            HashMap::new();
        for line in pane_lines {
            if line.is_empty() {
                continue;
            }
            let raw = parse_pane_line(&line)?;
            attached.insert(raw.session_id.clone(), raw.session_attached);
            let window = windows_by_session
                .entry(raw.session_id.clone())
                .or_default()
                .entry(raw.window_id.clone())
                .or_insert_with(|| SnapshotWindow {
                    id: raw.window_id,
                    index: raw.window_index,
                    name: raw.window_name,
                    layout: raw.window_layout,
                    active: raw.window_active,
                    zoomed: raw.window_zoomed,
                    panes: Vec::new(),
                });
            let minimization = options_by_pane
                .get(&raw.pane_id)
                .cloned()
                .unwrap_or(PaneMinimizationProvenance::None);
            window.panes.push(SnapshotPane {
                id: raw.pane_id,
                index: raw.pane_index,
                active: raw.pane_active,
                width: raw.pane_width,
                height: raw.pane_height,
                minimization,
            });
        }

        // Assemble in list-sessions order; a session list-panes -a saw but
        // list-sessions did not (the race the module doc names) is appended
        // rather than dropped.
        let mut order = session_order;
        for id in windows_by_session.keys() {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }

        let mut sessions = Vec::with_capacity(order.len());
        for id in order {
            let mut windows: Vec<SnapshotWindow> = windows_by_session
                .remove(&id)
                .map(|by_id| by_id.into_values().collect())
                .unwrap_or_default();
            windows.sort_by_key(|w| w.index);
            for window in &mut windows {
                window.panes.sort_by_key(|p| p.index);
            }
            let name = session_names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| id.clone());
            let session_attached = attached.get(&id).copied().unwrap_or(false);
            sessions.push(SnapshotSession {
                id,
                name,
                attached: session_attached,
                windows,
            });
        }

        Ok(WorkspaceSnapshot { sessions })
    }
}

/// One parsed `SNAPSHOT_PANE_FORMAT` line.
struct RawPaneLine {
    session_id: String,
    session_attached: bool,
    window_id: String,
    window_index: usize,
    window_active: bool,
    window_zoomed: bool,
    pane_id: String,
    pane_index: usize,
    pane_active: bool,
    pane_width: u32,
    pane_height: u32,
    window_layout: String,
    window_name: String,
}

fn parse_pane_options(lines: &[String]) -> HashMap<String, PaneMinimizationProvenance> {
    let mut map = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((pane_id, opt)) = line.split_once('\t') {
            map.insert(pane_id.to_string(), PaneMinimizationProvenance::parse(opt));
        } else {
            map.insert(line.clone(), PaneMinimizationProvenance::None);
        }
    }
    map
}

fn parse_pane_line(line: &str) -> Result<RawPaneLine, TmuxError> {
    let mut fields = line.splitn(SNAPSHOT_PANE_FIELDS, '\t');
    let mut next = || {
        fields
            .next()
            .ok_or_else(|| TmuxError::Protocol(format!("workspace snapshot pane line: {line:?}")))
    };
    let session_id = next()?.to_string();
    let session_attached = next()? == "1";
    let window_id = next()?.to_string();
    let window_index = parse_usize(next()?, "window_index")?;
    let window_active = next()? == "1";
    let window_zoomed = next()? == "1";
    let pane_id = next()?.to_string();
    let pane_index = parse_usize(next()?, "pane_index")?;
    let pane_active = next()? == "1";
    let pane_width = parse_u32(next()?, "pane_width")?;
    let pane_height = parse_u32(next()?, "pane_height")?;
    let window_layout = next()?.to_string();
    let window_name = next()?.to_string();
    Ok(RawPaneLine {
        session_id,
        session_attached,
        window_id,
        window_index,
        window_active,
        window_zoomed,
        pane_id,
        pane_index,
        pane_active,
        pane_width,
        pane_height,
        window_layout,
        window_name,
    })
}

/// Parse every `SNAPSHOT_SESSION_NAME_FORMAT` line, returning (session ids in
/// the order tmux listed them, id -> name).
fn parse_session_names(
    lines: &[String],
) -> Result<(Vec<String>, HashMap<String, String>), TmuxError> {
    let mut order = Vec::with_capacity(lines.len());
    let mut names = HashMap::with_capacity(lines.len());
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (id, name) = line
            .split_once('\t')
            .ok_or_else(|| TmuxError::Protocol(format!("list-sessions line: {line:?}")))?;
        order.push(id.to_string());
        names.insert(id.to_string(), name.to_string());
    }
    Ok((order, names))
}

fn parse_usize(field: &str, what: &str) -> Result<usize, TmuxError> {
    field
        .parse()
        .map_err(|e| TmuxError::Protocol(format!("workspace snapshot {what}: {e}")))
}

fn parse_u32(field: &str, what: &str) -> Result<u32, TmuxError> {
    field
        .parse()
        .map_err(|e| TmuxError::Protocol(format!("workspace snapshot {what}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_line_parses_and_keeps_a_tab_inside_the_trailing_window_name() {
        let line = "$0\t1\t@1\t2\t1\t0\t%3\t1\t1\t80\t24\t8205,80x24,0,0{...}\tname\twith\ttabs";
        let raw = parse_pane_line(line).expect("parses");
        assert_eq!(raw.session_id, "$0");
        assert!(raw.session_attached);
        assert_eq!(raw.window_id, "@1");
        assert_eq!(raw.window_index, 2);
        assert!(raw.window_active);
        assert!(!raw.window_zoomed);
        assert_eq!(raw.pane_id, "%3");
        assert_eq!(raw.pane_index, 1);
        assert!(raw.pane_active);
        assert_eq!(raw.pane_width, 80);
        assert_eq!(raw.pane_height, 24);
        assert_eq!(raw.window_layout, "8205,80x24,0,0{...}");
        // The trailing field swallows every tab after the 12th: a window
        // name that happens to contain literal tabs still parses whole
        // instead of corrupting fields that do not exist after it.
        assert_eq!(raw.window_name, "name\twith\ttabs");
    }

    #[test]
    fn pane_options_parse_and_keeps_tabs_inside_malformed_provenance() {
        let lines = vec![
            "%1\tv1:23".to_string(),
            "%2\tcorrupted\twith\ttabs".to_string(),
            "%3\t".to_string(),
            "%4".to_string(),
        ];
        let map = parse_pane_options(&lines);
        assert_eq!(
            map.get("%1"),
            Some(&PaneMinimizationProvenance::Minimized {
                original_height: 23
            })
        );
        assert_eq!(
            map.get("%2"),
            Some(&PaneMinimizationProvenance::Malformed(
                "corrupted\twith\ttabs".to_string()
            ))
        );
        assert_eq!(map.get("%3"), Some(&PaneMinimizationProvenance::None));
        assert_eq!(map.get("%4"), Some(&PaneMinimizationProvenance::None));
    }

    #[test]
    fn pane_line_rejects_too_few_fields() {
        assert!(matches!(
            parse_pane_line("$0\t1\t@1"),
            Err(TmuxError::Protocol(_))
        ));
    }

    #[test]
    fn session_names_preserve_order_and_last_field_swallows_tabs() {
        let lines = vec![
            "$1\tbeta session".to_string(),
            "$0\tname\twith\ttab".to_string(),
        ];
        let (order, names) = parse_session_names(&lines).expect("parses");
        assert_eq!(order, vec!["$1", "$0"]);
        assert_eq!(names.get("$1").map(String::as_str), Some("beta session"));
        assert_eq!(names.get("$0").map(String::as_str), Some("name\twith\ttab"));
    }

    #[test]
    fn byte_exact_provenance_parsing_rejects_whitespace_and_trailing_characters() {
        assert_eq!(
            PaneMinimizationProvenance::parse(""),
            PaneMinimizationProvenance::None
        );
        assert_eq!(
            PaneMinimizationProvenance::parse("v1:24"),
            PaneMinimizationProvenance::Minimized {
                original_height: 24
            }
        );
        assert_eq!(
            PaneMinimizationProvenance::parse("v1:24\t"),
            PaneMinimizationProvenance::Malformed("v1:24\t".to_string())
        );
        assert_eq!(
            PaneMinimizationProvenance::parse(" v1:24 "),
            PaneMinimizationProvenance::Malformed(" v1:24 ".to_string())
        );
        assert_eq!(
            PaneMinimizationProvenance::parse("v1:1"),
            PaneMinimizationProvenance::Malformed("v1:1".to_string())
        );
        assert_eq!(
            PaneMinimizationProvenance::parse("   "),
            PaneMinimizationProvenance::Malformed("   ".to_string())
        );
    }
}
