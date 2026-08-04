//! Session discovery for workspace attach.

use crate::cmd::run;
use crate::error::TmuxError;

/// One session on a tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub name: String,
    pub attached: bool,
}

/// List sessions on the server `socket` names (None = default server).
pub fn list_sessions(socket: Option<&str>) -> Result<Vec<SessionRow>, TmuxError> {
    let out = run(
        socket,
        None,
        &[
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_attached}",
        ],
    )?;
    let mut rows = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let (name, attached) = line
            .split_once('\t')
            .ok_or_else(|| TmuxError::Protocol(format!("list-sessions line: {line:?}")))?;
        rows.push(SessionRow {
            name: name.to_string(),
            attached: attached == "1",
        });
    }
    Ok(rows)
}

/// Active pane id (`%n`) in a session.
pub fn active_pane(session: &str, socket: Option<&str>) -> Result<String, TmuxError> {
    let target = crate::cmd::session_target(session);
    let out = run(
        socket,
        None,
        &[
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id}\t#{pane_active}",
        ],
    )?;
    for line in out.lines() {
        if let Some((id, active)) = line.split_once('\t') {
            if active == "1" {
                return Ok(id.to_string());
            }
        }
    }
    Err(TmuxError::Protocol(format!(
        "no active pane in session {session}"
    )))
}
