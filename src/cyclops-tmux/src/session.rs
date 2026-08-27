//! Session discovery for workspace attach.

use crate::cmd::{run, session_target};
use crate::error::TmuxError;

/// One session on a tmux server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// Stable tmux session id, e.g. `$0`.
    pub id: String,
    pub name: String,
    pub attached: bool,
    /// Number of windows (tabs) in the session.
    pub tab_count: usize,
}

/// One window in a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRow {
    /// Window id, e.g. `@0`.
    pub id: String,
    /// Zero-based window index in the session.
    pub index: usize,
    pub name: String,
    /// Raw tmux layout string (`#{window_layout}`).
    pub layout: String,
    pub active: bool,
    pub zoomed: bool,
}

/// One pane in a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowPaneRow {
    /// Pane id, e.g. `%0`.
    pub id: String,
    /// Zero-based pane index in the window.
    pub index: usize,
    pub active: bool,
}

/// Membership of one globally identified window in a session. A single
/// `list-windows -a` supplies the sidebar hierarchy without one process per
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMembership {
    pub session_id: String,
    pub window_id: String,
}

/// List sessions on the server `socket` names (None = default server).
/// The session the calling shell is sitting in, or `None` outside tmux.
///
/// `$TMUX` is the guard rather than the answer: it proves there is a server
/// and a client to ask, and tmux itself resolves which session that client
/// is currently showing, which is not derivable from the variable.
pub fn current_session(socket: Option<&str>) -> Option<String> {
    std::env::var_os("TMUX")?;
    let out = run(socket, None, &["display-message", "-p", "#{session_name}"]).ok()?;
    let name = out.trim().to_string();
    (!name.is_empty()).then_some(name)
}

pub fn list_sessions(socket: Option<&str>) -> Result<Vec<SessionRow>, TmuxError> {
    let out = run(
        socket,
        None,
        &[
            "list-sessions",
            "-F",
            "#{session_id}\t#{session_name}\t#{session_attached}\t#{session_windows}",
        ],
    )?;
    let mut rows = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 4 {
            return Err(TmuxError::Protocol(format!("list-sessions line: {line:?}")));
        }
        rows.push(SessionRow {
            id: parts[0].to_string(),
            name: parts[1].to_string(),
            attached: parts[2] == "1",
            tab_count: parts[3]
                .parse()
                .map_err(|e| TmuxError::Protocol(format!("session_windows: {e}")))?,
        });
    }
    Ok(rows)
}

/// List windows in a session.
pub fn list_windows(session: &str, socket: Option<&str>) -> Result<Vec<WindowRow>, TmuxError> {
    let target = session_target(session);
    let out = run(
        socket,
        None,
        &[
            "list-windows",
            "-t",
            &target,
            "-F",
            "#{window_id}\t#{window_index}\t#{window_name}\t#{window_layout}\t#{window_active}\t#{window_zoomed_flag}",
        ],
    )?;
    let mut rows = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 6 {
            return Err(TmuxError::Protocol(format!("list-windows line: {line:?}")));
        }
        rows.push(WindowRow {
            id: parts[0].to_string(),
            index: parts[1]
                .parse()
                .map_err(|e| TmuxError::Protocol(format!("window_index: {e}")))?,
            name: parts[2].to_string(),
            layout: parts[3].to_string(),
            active: parts[4] == "1",
            zoomed: parts[5] == "1",
        });
    }
    Ok(rows)
}

/// List every session-to-window edge on the server in one tmux invocation.
pub fn list_window_memberships(socket: Option<&str>) -> Result<Vec<WindowMembership>, TmuxError> {
    let out = run(
        socket,
        None,
        &["list-windows", "-a", "-F", "#{session_id}\t#{window_id}"],
    )?;
    let mut rows = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((session_id, window_id)) = line.split_once('\t') else {
            return Err(TmuxError::Protocol(format!(
                "list-windows membership line: {line:?}"
            )));
        };
        rows.push(WindowMembership {
            session_id: session_id.to_string(),
            window_id: window_id.to_string(),
        });
    }
    Ok(rows)
}

/// List panes in one window (`@n` or `session:index`).
pub fn list_panes(window: &str, socket: Option<&str>) -> Result<Vec<WindowPaneRow>, TmuxError> {
    let out = run(
        socket,
        None,
        &[
            "list-panes",
            "-t",
            window,
            "-F",
            "#{pane_id}\t#{pane_index}\t#{pane_active}",
        ],
    )?;
    let mut rows = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            return Err(TmuxError::Protocol(format!("list-panes line: {line:?}")));
        }
        rows.push(WindowPaneRow {
            id: parts[0].to_string(),
            index: parts[1]
                .parse()
                .map_err(|e| TmuxError::Protocol(format!("pane_index: {e}")))?,
            active: parts[2] == "1",
        });
    }
    Ok(rows)
}

/// Active pane id (`%n`) in a session.
pub fn active_pane(session: &str, socket: Option<&str>) -> Result<String, TmuxError> {
    let target = session_target(session);
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
