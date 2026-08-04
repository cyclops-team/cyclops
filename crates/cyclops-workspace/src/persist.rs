#![allow(dead_code)]

//! Durable workspace UI preferences and last-active state.

use std::path::Path;

/// User intent persisted under `[workspace]` in config.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePrefs {
    pub sidebar_visible: bool,
    pub sidebar_width: u16,
    pub workspace_order: Vec<String>,
}

impl Default for WorkspacePrefs {
    fn default() -> Self {
        WorkspacePrefs {
            sidebar_visible: true,
            sidebar_width: 20,
            workspace_order: Vec::new(),
        }
    }
}

/// Volatile last-active workspace/tab from the daemon wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastActive {
    pub session: String,
    pub window_id: String,
}

/// Where to land on reopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReopenTarget {
    LastActive { session: String, window_id: String },
    DefaultWorkspace(String),
    First(String),
    OfferCreate,
}

/// Read `[workspace]` keys from `<home>/config.toml`. Unknown keys are ignored.
pub fn load_prefs(home: &Path) -> WorkspacePrefs {
    let path = home.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return WorkspacePrefs::default();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return WorkspacePrefs::default();
    };
    let Some(workspace) = table.get("workspace").and_then(|v| v.as_table()) else {
        return WorkspacePrefs::default();
    };
    let mut prefs = WorkspacePrefs::default();
    if let Some(v) = workspace.get("sidebar_visible").and_then(|v| v.as_bool()) {
        prefs.sidebar_visible = v;
    }
    if let Some(v) = workspace
        .get("sidebar_width")
        .and_then(|v| v.as_integer())
        .and_then(|v| u16::try_from(v).ok())
    {
        prefs.sidebar_width = v;
    }
    if let Some(arr) = workspace.get("workspace_order").and_then(|v| v.as_array()) {
        prefs.workspace_order = arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    prefs
}

/// Merge `[workspace]` keys into config.toml, preserving other sections.
pub fn save_prefs(home: &Path, prefs: &WorkspacePrefs) -> std::io::Result<()> {
    let path = home.join("config.toml");
    let mut table: toml::Table = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or_default()
    } else {
        toml::Table::new()
    };
    let order = toml::Value::Array(
        prefs
            .workspace_order
            .iter()
            .map(|s| toml::Value::String(s.clone()))
            .collect(),
    );
    let mut workspace = toml::Table::new();
    workspace.insert(
        "sidebar_visible".into(),
        toml::Value::Boolean(prefs.sidebar_visible),
    );
    workspace.insert(
        "sidebar_width".into(),
        toml::Value::Integer(prefs.sidebar_width as i64),
    );
    workspace.insert("workspace_order".into(), order);
    table.insert("workspace".into(), toml::Value::Table(workspace));
    std::fs::write(path, toml::to_string_pretty(&table).unwrap_or_default())
}

/// Query last-active state from cyclopsd. Returns None when the daemon is
/// offline or the method is absent (version skew).
pub fn get_last_active(_home: &Path) -> Option<LastActive> {
    let sock = cyclops_proto::socket_path();
    if !sock.exists() {
        return None;
    }
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        return None;
    };
    use std::io::{BufRead, Write};
    let req = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"workspace_ui.get\",\"params\":{{\"protocol_version\":{}}}}}\n",
        cyclops_proto::PROTOCOL_VERSION
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return None;
    }
    let mut line = String::new();
    let mut reader = std::io::BufReader::new(stream);
    if reader.read_line(&mut line).is_err() {
        return None;
    }
    let Ok(resp) = serde_json::from_str::<serde_json::Value>(&line) else {
        return None;
    };
    if resp.get("error").is_some() {
        return None;
    }
    let result = resp.get("result")?;
    let session = result.get("last_active_session")?.as_str()?.to_string();
    let window_id = result.get("last_active_window")?.as_str()?.to_string();
    Some(LastActive { session, window_id })
}

/// Persist last-active workspace/tab through cyclopsd.
pub fn set_last_active(_home: &Path, session: &str, window_id: &str) {
    let sock = cyclops_proto::socket_path();
    if !sock.exists() {
        return;
    }
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        return;
    };
    use std::io::Write;
    let body = serde_json::json!({
        "session": session,
        "window_id": window_id,
        "protocol_version": cyclops_proto::PROTOCOL_VERSION,
    });
    let req = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"workspace_ui.set\",\"params\":{body}}}\n"
    );
    let _ = stream.write_all(req.as_bytes());
}

/// Reopen fallback: last-active → default_workspace → first → offer.
pub fn reopen_fallback(
    session_names: &[String],
    last_active: Option<&LastActive>,
    default_workspace: Option<&str>,
    workspace_order: &[String],
) -> ReopenTarget {
    if let Some(la) = last_active {
        if session_names.iter().any(|s| s == &la.session) {
            return ReopenTarget::LastActive {
                session: la.session.clone(),
                window_id: la.window_id.clone(),
            };
        }
    }
    if let Some(default) = default_workspace {
        if session_names.iter().any(|s| s == default) {
            return ReopenTarget::DefaultWorkspace(default.to_string());
        }
    }
    if let Some(first_ordered) = workspace_order
        .iter()
        .find(|name| session_names.iter().any(|s| s == *name))
    {
        return ReopenTarget::First(first_ordered.clone());
    }
    if let Some(first) = session_names.first() {
        return ReopenTarget::First(first.clone());
    }
    ReopenTarget::OfferCreate
}

/// Apply workspace_order to a session list.
pub fn order_workspaces(mut names: Vec<String>, order: &[String]) -> Vec<String> {
    if order.is_empty() {
        return names;
    }
    let mut out = Vec::new();
    for name in order {
        if let Some(pos) = names.iter().position(|n| n == name) {
            out.push(names.remove(pos));
        }
    }
    out.extend(names);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::scratch::scratch_dir;

    #[test]
    fn prefs_round_trip_under_scratch_home() {
        let home = scratch_dir("ws-prefs");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch");
        let prefs = WorkspacePrefs {
            sidebar_visible: false,
            sidebar_width: 28,
            workspace_order: vec!["beta".into(), "alpha".into()],
        };
        save_prefs(&home, &prefs).expect("save");
        let loaded = load_prefs(&home);
        assert_eq!(loaded, prefs);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn unknown_config_keys_do_not_break_load() {
        let home = scratch_dir("ws-prefs-unknown");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch");
        std::fs::write(
            home.join("config.toml"),
            "[workspace]\nsidebar_visible = true\nfuture_key = 42\n",
        )
        .expect("write");
        let loaded = load_prefs(&home);
        assert!(loaded.sidebar_visible);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fallback_chain_each_link() {
        let sessions = vec!["a".into(), "b".into()];
        let last = LastActive {
            session: "b".into(),
            window_id: "@1".into(),
        };
        assert_eq!(
            reopen_fallback(&sessions, Some(&last), None, &[]),
            ReopenTarget::LastActive {
                session: "b".into(),
                window_id: "@1".into(),
            }
        );
        assert_eq!(
            reopen_fallback(&sessions, None, Some("a"), &[]),
            ReopenTarget::DefaultWorkspace("a".into())
        );
        assert_eq!(
            reopen_fallback(&sessions, None, None, &["b".into(), "a".into()]),
            ReopenTarget::First("b".into())
        );
        assert_eq!(
            reopen_fallback(&[], None, None, &[]),
            ReopenTarget::OfferCreate
        );
    }

    #[test]
    fn workspace_order_sorts_known_sessions() {
        let names = vec!["alpha".into(), "beta".into(), "gamma".into()];
        let ordered = order_workspaces(names, &["gamma".into(), "alpha".into()]);
        assert_eq!(ordered, vec!["gamma", "alpha", "beta"]);
    }
}
