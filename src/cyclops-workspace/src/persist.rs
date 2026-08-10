//! Durable workspace UI preferences and last-active state, including
//! keeping a persisted order list correct when the identity it was keyed
//! on renames or is replaced.

use std::path::Path;

use crate::decoration::DecorationSnapshot;

/// Which surface the sidebar shows: the session list or the event stream.
/// Persisted under `[workspace] sidebar_tab` as `"sessions"`/`"stream"`;
/// an unknown value loads as the default, so a config written by a newer
/// build cannot break an older one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SidebarTab {
    #[default]
    Sessions,
    Stream,
}

impl SidebarTab {
    fn as_str(self) -> &'static str {
        match self {
            SidebarTab::Sessions => "sessions",
            SidebarTab::Stream => "stream",
        }
    }

    fn parse(s: &str) -> Option<SidebarTab> {
        match s {
            "sessions" => Some(SidebarTab::Sessions),
            "stream" => Some(SidebarTab::Stream),
            _ => None,
        }
    }
}

/// User intent persisted under `[workspace]` in config.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePrefs {
    pub sidebar_visible: bool,
    pub sidebar_width: u16,
    /// The sidebar tab last selected, restored on reopen like visibility
    /// and width.
    pub sidebar_tab: SidebarTab,
    /// Whether the tab strip shows. Visible is the default and hiding it
    /// is the operator's explicit choice, so the `+` that makes tabs is on
    /// screen from a fresh install; restored on reopen like the sidebar's
    /// own visibility.
    pub tab_bar_visible: bool,
    /// Whether chrome fades between states (`crate::animate`). On by
    /// default: the four fades are short, none moves anything, and each
    /// one carries information the snap carried too. Off is stored rather
    /// than inferred, because the capability gates around it (no color, no
    /// truecolor, a terminal that cannot keep up) all answer "can we",
    /// while this answers "should we", and an operator who finds motion
    /// distracting must not have to re-answer it every launch.
    pub motion: bool,
    pub workspace_order: Vec<String>,
    /// Stable labels, with pane-id fallbacks for unnamed detected agents.
    pub agent_order: Vec<String>,
    /// tmux session ids (`$n`) of workspaces that follow their pane's
    /// folder. The id, not the name, is what's tracked here: following is
    /// what renames the name, so the name can't be the thing that survives
    /// the rename.
    pub folder_tracked: Vec<String>,
}

impl Default for WorkspacePrefs {
    fn default() -> Self {
        WorkspacePrefs {
            sidebar_visible: true,
            // Matches `render::SIDEBAR_DEFAULT_WIDTH`: the width a fresh
            // install opens at. Not the clamp floor any more — an operator
            // can drag well below this — so a fresh config should not
            // start pinned to the minimum just because it once matched it.
            sidebar_width: crate::render::SIDEBAR_DEFAULT_WIDTH,
            sidebar_tab: SidebarTab::default(),
            tab_bar_visible: true,
            motion: true,
            workspace_order: Vec::new(),
            agent_order: Vec::new(),
            folder_tracked: Vec::new(),
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
    if let Some(v) = workspace
        .get("sidebar_tab")
        .and_then(|v| v.as_str())
        .and_then(SidebarTab::parse)
    {
        prefs.sidebar_tab = v;
    }
    if let Some(v) = workspace.get("motion").and_then(|v| v.as_bool()) {
        prefs.motion = v;
    }
    if let Some(v) = workspace.get("tab_bar_visible").and_then(|v| v.as_bool()) {
        prefs.tab_bar_visible = v;
    }
    if let Some(arr) = workspace.get("workspace_order").and_then(|v| v.as_array()) {
        prefs.workspace_order = arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    if let Some(arr) = workspace.get("agent_order").and_then(|v| v.as_array()) {
        prefs.agent_order = arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    if let Some(arr) = workspace.get("folder_tracked").and_then(|v| v.as_array()) {
        prefs.folder_tracked = arr
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
        std::fs::read_to_string(&path)?
            .parse()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
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
    let agent_order = toml::Value::Array(
        prefs
            .agent_order
            .iter()
            .map(|s| toml::Value::String(s.clone()))
            .collect(),
    );
    let folder_tracked = toml::Value::Array(
        prefs
            .folder_tracked
            .iter()
            .map(|s| toml::Value::String(s.clone()))
            .collect(),
    );
    let mut workspace = match table.remove("workspace") {
        Some(toml::Value::Table(workspace)) => workspace,
        _ => toml::Table::new(),
    };
    workspace.insert(
        "sidebar_visible".into(),
        toml::Value::Boolean(prefs.sidebar_visible),
    );
    workspace.insert(
        "sidebar_width".into(),
        toml::Value::Integer(prefs.sidebar_width as i64),
    );
    workspace.insert(
        "sidebar_tab".into(),
        toml::Value::String(prefs.sidebar_tab.as_str().into()),
    );
    workspace.insert("motion".into(), toml::Value::Boolean(prefs.motion));
    workspace.insert(
        "tab_bar_visible".into(),
        toml::Value::Boolean(prefs.tab_bar_visible),
    );
    workspace.insert("workspace_order".into(), order);
    workspace.insert("agent_order".into(), agent_order);
    workspace.insert("folder_tracked".into(), folder_tracked);
    table.insert("workspace".into(), toml::Value::Table(workspace));
    let mut body = toml::to_string_pretty(&table)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if !body.ends_with('\n') {
        body.push('\n');
    }

    // A sidebar drag must never leave a torn shared config behind. The pid
    // keeps simultaneous workspace processes from sharing a temp file; the
    // final rename is atomic on the Unix platforms this crate supports. An
    // existing symlink is resolved so a dotfiles-managed link stays a link.
    std::fs::create_dir_all(home)?;
    let destination = if path.exists() {
        std::fs::canonicalize(&path)?
    } else {
        path.clone()
    };
    let tmp = destination.with_extension(format!("toml.workspace-{}.tmp", std::process::id()));
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        if let Ok(metadata) = std::fs::metadata(&path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, destination)
}

/// Query last-active state from cyclopsd. Returns None when the daemon is
/// offline or the method is absent (version skew).
pub fn get_last_active(home: &Path) -> Option<LastActive> {
    let result = crate::daemon::request(
        home,
        "workspace_ui.get",
        serde_json::json!({"protocol_version": cyclops_proto::PROTOCOL_VERSION}),
    )
    .ok()?;
    let session = result.get("last_active_session")?.as_str()?.to_string();
    let window_id = result.get("last_active_window")?.as_str()?.to_string();
    Some(LastActive { session, window_id })
}

/// Persist last-active workspace/tab through cyclopsd.
pub fn set_last_active(home: &Path, session: &str, window_id: &str) {
    // Wait for the acknowledgement so two rapid switches cannot be
    // processed out of order on separate daemon connection tasks.
    let _ = crate::daemon::request(
        home,
        "workspace_ui.set",
        serde_json::json!({
            "session": session,
            "window_id": window_id,
            "protocol_version": cyclops_proto::PROTOCOL_VERSION,
        }),
    );
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

/// Move one item to the index occupied by another. Downward drags land after
/// the target and upward drags before it, matching the direction of motion.
/// Keep a persisted row in place when its identity-bearing label changes.
/// If a stale copy of the new key already exists, remove the old entry rather
/// than creating a duplicate.
pub fn migrate_order_entry(order: &mut Vec<String>, old: &str, new: &str) -> bool {
    if old == new {
        return false;
    }
    let Some(old_index) = order.iter().position(|entry| entry == old) else {
        return false;
    };
    if order.iter().any(|entry| entry == new) {
        order.remove(old_index);
    } else {
        order[old_index] = new.to_string();
    }
    true
}

/// Preserve a pane's position when a daemon event reports an external rename
/// or clear. Pane ids bind the before/after snapshots without guessing from
/// display text.
pub fn migrate_agent_order_entries(
    order: &mut Vec<String>,
    previous: &DecorationSnapshot,
    next: &DecorationSnapshot,
) -> bool {
    let mut changed = false;
    for pane in next.panes.values() {
        let Some(old_pane) = previous.pane(&pane.pane_id) else {
            continue;
        };
        let old_key = DecorationSnapshot::agent_order_key(old_pane);
        let new_key = DecorationSnapshot::agent_order_key(pane);
        changed |= migrate_order_entry(order, &old_key, &new_key);
    }
    changed
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
            sidebar_tab: SidebarTab::Stream,
            tab_bar_visible: false,
            motion: false,
            workspace_order: vec!["beta".into(), "alpha".into()],
            agent_order: vec!["name:reviewer".into(), "pane:%7".into()],
            folder_tracked: vec!["$3".into(), "$7".into()],
        };
        save_prefs(&home, &prefs).expect("save");
        let loaded = load_prefs(&home);
        assert_eq!(loaded, prefs);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn folder_tracked_round_trips_by_session_id() {
        let home = scratch_dir("ws-prefs-folder-tracked");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch");
        let prefs = WorkspacePrefs {
            folder_tracked: vec!["$3".into()],
            ..WorkspacePrefs::default()
        };
        save_prefs(&home, &prefs).expect("save");
        let loaded = load_prefs(&home);
        // Tracked entries are session ids, not names — the id is what
        // survives a folder-follow rename, so it's the id that must persist.
        assert_eq!(loaded.folder_tracked, vec!["$3".to_string()]);
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
            "[workspace]\nsidebar_visible = true\nfuture_key = 42\nsidebar_tab = \"conga\"\n",
        )
        .expect("write");
        let loaded = load_prefs(&home);
        assert!(loaded.sidebar_visible);
        // A tab value this build does not know falls back to the default
        // instead of failing the whole load.
        assert_eq!(loaded.sidebar_tab, SidebarTab::Sessions);
        // The key a config written before this build simply lacks: the
        // strip shows, which is what a fresh install gets too.
        assert!(loaded.tab_bar_visible);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every preference belongs under `[workspace]`, and a key written to
    /// the root table instead would still round-trip through `save` and
    /// `load` in the same process while silently never reaching the
    /// section a human edits. So this asserts the section, not the value.
    #[test]
    fn every_saved_preference_lands_under_the_workspace_section() {
        let home = scratch_dir("ws-prefs-section");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch");

        save_prefs(&home, &WorkspacePrefs::default()).expect("save");
        let text = std::fs::read_to_string(home.join("config.toml")).expect("read");
        let table: toml::Table = text.parse().expect("parse");
        let workspace = table
            .get("workspace")
            .and_then(|v| v.as_table())
            .expect("a [workspace] section");

        for key in [
            "sidebar_visible",
            "sidebar_width",
            "sidebar_tab",
            "tab_bar_visible",
            "motion",
            "workspace_order",
            "agent_order",
            "folder_tracked",
        ] {
            assert!(
                workspace.contains_key(key),
                "{key} is missing from [workspace]"
            );
            assert!(
                !table.contains_key(key),
                "{key} was written to the root table as well as, or instead of, [workspace]"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn saving_prefs_preserves_bindings_and_future_workspace_keys() {
        let home = scratch_dir("ws-prefs-preserve");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch");
        std::fs::write(
            home.join("config.toml"),
            "[workspace]\nfuture_key = 42\n[workspace.bindings]\nnext_tab = 'Alt+n'\n[theme]\nname = 'sage'\n",
        )
        .expect("write");

        save_prefs(
            &home,
            &WorkspacePrefs {
                sidebar_visible: true,
                sidebar_width: 31,
                sidebar_tab: SidebarTab::Sessions,
                tab_bar_visible: true,
                motion: true,
                workspace_order: vec!["cyclops".into()],
                agent_order: vec!["name:implementer".into()],
                folder_tracked: vec![],
            },
        )
        .expect("save");

        let saved = std::fs::read_to_string(home.join("config.toml")).expect("saved config");
        let table = saved.parse::<toml::Table>().expect("valid TOML");
        let workspace = table["workspace"].as_table().expect("workspace table");
        assert_eq!(workspace["future_key"].as_integer(), Some(42));
        assert_eq!(
            workspace["bindings"]
                .as_table()
                .and_then(|bindings| bindings["next_tab"].as_str()),
            Some("Alt+n")
        );
        assert_eq!(
            table["theme"]
                .as_table()
                .and_then(|theme| theme["name"].as_str()),
            Some("sage")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn saving_prefs_never_overwrites_an_invalid_shared_config() {
        let home = scratch_dir("ws-prefs-invalid");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch");
        let original = "theme = [this is not valid TOML\n";
        std::fs::write(home.join("config.toml"), original).expect("write invalid config");

        let error = save_prefs(&home, &WorkspacePrefs::default()).expect_err("must fail closed");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).expect("original remains"),
            original
        );
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
    fn renamed_sidebar_entries_keep_their_persisted_position() {
        let mut order = vec!["alpha".into(), "beta".into(), "gamma".into()];
        assert!(migrate_order_entry(&mut order, "beta", "renamed"));
        assert_eq!(order, vec!["alpha", "renamed", "gamma"]);

        let mut stale = vec!["old".into(), "new".into()];
        assert!(migrate_order_entry(&mut stale, "old", "new"));
        assert_eq!(stale, vec!["new"]);
    }

    #[test]
    fn external_agent_rename_keeps_its_persisted_position() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let snapshot = |label: &str| {
            let mut snapshot = DecorationSnapshot::default();
            snapshot.panes.insert(
                "%7".into(),
                PaneDecoration {
                    pane_id: "%7".into(),
                    window_id: "@2".into(),
                    label: Some(label.into()),
                    manifest: Some("claude".into()),
                    manifest_display_name: Some("Claude Code".into()),
                    state: AgentState::Idle,
                    needs_attention: false,
                },
            );
            snapshot
        };
        let mut order = vec!["name:reviewer".into(), "name:implementer".into()];
        assert!(migrate_agent_order_entries(
            &mut order,
            &snapshot("reviewer"),
            &snapshot("auditor")
        ));
        assert_eq!(order, vec!["name:auditor", "name:implementer"]);
    }
}
