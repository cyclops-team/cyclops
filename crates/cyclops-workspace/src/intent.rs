//! UI intents mapped to tmux operations. The model updates only from
//! reconciliation after tmux replies and notifications — never here.

use std::path::Path;

use cyclops_tmux::{quote_arg, session_target, ControlClient, TmuxError};

/// Structural workspace actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Select a tab by tmux window id (`@n`) — robust to index gaps.
    SelectTabId(String),
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    SplitRight,
    SplitDown,
    ClosePane,
    ZoomPane,
    SwitchWorkspace(String),
}

/// Issue one intent against tmux. Does not mutate the workspace model.
pub async fn execute(
    client: &ControlClient,
    intent: Intent,
    active_pane: &str,
) -> Result<(), TmuxError> {
    match intent {
        Intent::SelectTabId(id) => {
            client
                .command(&format!("select-window -t {}", quote_arg(&id)))
                .await?;
        }
        Intent::FocusLeft => {
            client.command("select-pane -L").await?;
        }
        Intent::FocusRight => {
            client.command("select-pane -R").await?;
        }
        Intent::FocusUp => {
            client.command("select-pane -U").await?;
        }
        Intent::FocusDown => {
            client.command("select-pane -D").await?;
        }
        Intent::SplitRight => {
            split(client, active_pane, true).await?;
        }
        Intent::SplitDown => {
            split(client, active_pane, false).await?;
        }
        Intent::ClosePane => {
            client
                .command(&format!("kill-pane -t {}", quote_arg(active_pane)))
                .await?;
        }
        Intent::ZoomPane => {
            client
                .command(&format!("resize-pane -Z -t {}", quote_arg(active_pane)))
                .await?;
        }
        Intent::SwitchWorkspace(name) => {
            client
                .command(&format!(
                    "switch-client -t {}",
                    quote_arg(&session_target(&name))
                ))
                .await?;
        }
    }
    Ok(())
}

/// Select a pane and, when it lives on another tab, select that window
/// first. The two commands stay explicit so each control-mode reply remains
/// correctly correlated.
pub async fn execute_focus_pane(
    client: &ControlClient,
    window_id: Option<&str>,
    pane_id: &str,
) -> Result<(), TmuxError> {
    if let Some(window_id) = window_id {
        client
            .command(&format!("select-window -t {}", quote_arg(window_id)))
            .await?;
    }
    client
        .command(&format!("select-pane -t {}", quote_arg(pane_id)))
        .await?;
    Ok(())
}

/// Create a tab, optionally named and rooted in `cwd`, and return its window
/// id. One command owns creation plus naming, so the UI never exposes an
/// intermediate default name.
pub async fn execute_new_tab(
    client: &ControlClient,
    cwd: Option<&str>,
    name: Option<&str>,
) -> Result<String, TmuxError> {
    let mut cmd = format!("new-window -P -F {}", quote_arg("#{window_id}"));
    if let Some(name) = name.filter(|name| !name.is_empty()) {
        cmd.push_str(&format!(" -n {}", quote_arg(name)));
    }
    if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
        cmd.push_str(&format!(" -c {}", quote_arg(dir)));
    }
    let out = client.command(&cmd).await?;
    Ok(out
        .first()
        .map(|s| s.trim().to_string())
        .unwrap_or_default())
}

/// A workspace this call just created: its name plus the tmux session id
/// that identifies it once the name is gone. A folder-following workspace
/// gets renamed later, and the id is the only handle that survives that.
#[derive(Debug, Clone)]
pub struct NewWorkspace {
    /// The production caller only needs `session_id` to start following the
    /// folder; `name` is exercised by the test below, which asserts creation
    /// still names the session after the folder.
    #[cfg_attr(not(test), allow(dead_code))]
    pub name: String,
    pub session_id: String,
}

/// Create a workspace (tmux session) from a project folder and switch to
/// it. The name is the folder's basename, sanitized for tmux and made
/// unique against `taken`, so "create a workspace here" never collides.
pub async fn execute_new_workspace(
    client: &ControlClient,
    folder: &Path,
    taken: &[String],
) -> Result<NewWorkspace, TmuxError> {
    let name = unique_session_name(&session_name_from_folder(folder), taken);
    let path = folder.to_string_lossy();
    let out = client
        .command(&format!(
            "new-session -d -P -F {} -s {} -n {} -c {}",
            quote_arg("#{session_id}"),
            quote_arg(&name),
            quote_arg("1"),
            quote_arg(path.as_ref())
        ))
        .await?;
    let session_id = out
        .first()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    client
        .command(&format!(
            "switch-client -t {}",
            quote_arg(&session_target(&name))
        ))
        .await?;
    Ok(NewWorkspace { name, session_id })
}

/// Rename one session.
pub async fn execute_rename_workspace(
    client: &ControlClient,
    session: &str,
    name: &str,
) -> Result<(), TmuxError> {
    client
        .command(&format!(
            "rename-session -t {} {}",
            quote_arg(&session_target(session)),
            quote_arg(name)
        ))
        .await?;
    Ok(())
}

/// Close one session. When it owns this control client, switch the client
/// to `fallback` first so closing one workspace does not strand the UI while
/// other sessions still exist.
pub async fn execute_close_workspace(
    client: &ControlClient,
    session: &str,
    fallback: Option<&str>,
) -> Result<(), TmuxError> {
    if let Some(fallback) = fallback {
        client
            .command(&format!(
                "switch-client -t {}",
                quote_arg(&session_target(fallback))
            ))
            .await?;
    }
    client
        .command(&format!(
            "kill-session -t {}",
            quote_arg(&session_target(session))
        ))
        .await?;
    Ok(())
}

/// A folder basename as a tmux session name. tmux reserves `.` and `:` in
/// targets, so they become `-`; an unusable basename falls back to
/// "workspace".
pub fn session_name_from_folder(folder: &Path) -> String {
    let name: String = folder
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim()
        .chars()
        .map(|c| {
            if c == '.' || c == ':' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = name.trim_matches('-');
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `base`, or the first `base-N` (N from 2) not in `taken`.
pub fn unique_session_name(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("some suffix is always free")
}

/// The name a folder-following workspace should wear, or `None` when it
/// already wears it. `taken` is every OTHER workspace's name, so the suffix
/// rule that keeps `execute_new_workspace` collision-free keeps this rename
/// collision-free too.
///
/// This is a pure function — no tmux, no session lookup — because it *is*
/// the whole follow-the-folder rule: every case is decided from `current`,
/// `cwd`, and `taken` alone. That's what lets a caller run it on every
/// render tick without a round trip, and what lets it be tested exhaustively
/// without a tmux server.
pub fn folder_rename(current: &str, cwd: &str, taken: &[String]) -> Option<String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return None;
    }
    let base = session_name_from_folder(Path::new(cwd));
    let next = unique_session_name(&base, taken);
    (next != current).then_some(next)
}

/// Switch to adjacent workspace by index delta.
pub async fn execute_switch_workspace_by_delta(
    client: &ControlClient,
    workspaces: &[crate::model::WorkspaceRow],
    active: usize,
    delta: isize,
) -> Result<(), TmuxError> {
    if workspaces.is_empty() {
        return Ok(());
    }
    let len = workspaces.len() as isize;
    let next = (active as isize + delta).rem_euclid(len) as usize;
    let name = workspaces[next].name.clone();
    execute(client, Intent::SwitchWorkspace(name), "").await
}

/// Rename one window after the user supplies a name.
pub async fn execute_rename_tab(
    client: &ControlClient,
    window_id: &str,
    name: &str,
) -> Result<(), TmuxError> {
    client
        .command(&format!(
            "rename-window -t {} {}",
            quote_arg(window_id),
            quote_arg(name)
        ))
        .await?;
    Ok(())
}

/// Close one window.
pub async fn execute_close_tab(client: &ControlClient, window_id: &str) -> Result<(), TmuxError> {
    client
        .command(&format!("kill-window -t {}", quote_arg(window_id)))
        .await?;
    Ok(())
}

/// Resize a split divider by coalesced steps.
pub async fn resize_divider(
    client: &ControlClient,
    pane: &str,
    dir: crate::layout::SplitDir,
    steps: i32,
) -> Result<(), TmuxError> {
    if steps == 0 {
        return Ok(());
    }
    let flag = match dir {
        crate::layout::SplitDir::Horizontal => {
            if steps > 0 {
                "-R"
            } else {
                "-L"
            }
        }
        crate::layout::SplitDir::Vertical => {
            if steps > 0 {
                "-D"
            } else {
                "-U"
            }
        }
    };
    let n = steps.unsigned_abs();
    client
        .command(&format!(
            "resize-pane -t {} {} {}",
            quote_arg(pane),
            flag,
            n
        ))
        .await?;
    Ok(())
}

async fn split(client: &ControlClient, pane: &str, horizontal: bool) -> Result<(), TmuxError> {
    let path = client
        .display(pane, "#{pane_current_path}")
        .await?
        .trim()
        .to_string();
    let flag = if horizontal { "-h" } else { "-v" };
    client
        .command(&format!(
            "split-window {flag} -d -c {} -t {}",
            quote_arg(&path),
            quote_arg(pane)
        ))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use cyclops_testrig::{tmux_available, TmuxServer};
    use cyclops_tmux::ControlClient;

    use super::*;

    async fn rig_client(server: &TmuxServer, session: &str) -> ControlClient {
        let cfg = cyclops_tmux::ControlConfig::attach(session)
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        ControlClient::spawn(cfg).await.expect("attach").0
    }

    #[tokio::test]
    async fn new_workspace_sets_name_and_directory() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("ws-create");
        let folder = cyclops_proto::scratch::scratch_dir("cyclops-ws-create");
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).expect("folder");
        server.run_ok(&["new-session", "-d", "-s", "host", "/bin/sh"]);
        let client = rig_client(&server, "host").await;
        let created = execute_new_workspace(&client, &folder, &[])
            .await
            .expect("create");
        let name = created.name;
        assert_eq!(
            name,
            folder.file_name().unwrap().to_string_lossy(),
            "the session name comes from the scratch folder"
        );
        let out = server.run(&["display-message", "-p", "-t", &name, "#{session_id}"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            created.session_id,
            "the returned session id should identify the created session"
        );
        let out = server.run(&["display-message", "-p", "-t", &name, "#{session_path}"]);
        assert_eq!(
            std::fs::canonicalize(String::from_utf8_lossy(&out.stdout).trim()).unwrap(),
            std::fs::canonicalize(&folder).unwrap(),
            "session default directory should match folder"
        );
        let out = server.run(&["list-windows", "-t", &name, "-F", "#{window_name}"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "1",
            "a workspace's first tab uses the numeric sequence"
        );
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&folder);
    }

    fn pane_ids(server: &TmuxServer, target: &str) -> Vec<String> {
        let out = server.run(&["list-panes", "-t", target, "-F", "#{pane_id}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn split_right_opens_in_source_pane_path() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-split");
        let src = cyclops_proto::scratch::scratch_dir("cyclops-split-src");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(&src).expect("split src dir");
        server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-c",
            src.to_str().expect("UTF-8 scratch path"),
            "/bin/sh",
        ]);
        let client = rig_client(&server, "s").await;
        let before = pane_ids(&server, "s");
        let pane = before[0].clone();
        execute(&client, Intent::SplitRight, &pane)
            .await
            .expect("split");
        assert_eq!(pane_ids(&server, "s").len(), 2);
        let after = pane_ids(&server, "s");
        let new_pane = after
            .iter()
            .find(|p| !before.contains(p))
            .expect("new pane");
        let path = client
            .display(new_pane, "#{pane_current_path}")
            .await
            .expect("path");
        let expected = std::fs::canonicalize(&src).expect("canonical src");
        let actual = std::fs::canonicalize(path.trim()).expect("canonical pane path");
        assert_eq!(
            actual, expected,
            "new split pane should inherit source pane_current_path"
        );
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn split_down_increases_pane_count() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-split-d");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let client = rig_client(&server, "s").await;
        let pane = pane_ids(&server, "s")[0].clone();
        execute(&client, Intent::SplitDown, &pane)
            .await
            .expect("split");
        assert_eq!(pane_ids(&server, "s").len(), 2);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn close_pane_removes_it() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-close");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "s"]);
        let client = rig_client(&server, "s").await;
        let pane = pane_ids(&server, "s")[0].clone();
        execute(&client, Intent::ClosePane, &pane)
            .await
            .expect("close");
        assert_eq!(pane_ids(&server, "s").len(), 1);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn rename_tab_targets_the_named_window() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-rename");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        // A second window is active, so an untargeted rename would hit it
        // instead of the first — the id must carry the target.
        server.run_ok(&["new-window", "-t", "s", "-n", "active", "/bin/sh"]);
        let out = server.run(&["list-windows", "-t", "s", "-F", "#{window_id}"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let first = stdout.lines().next().expect("first window id").to_string();
        let client = rig_client(&server, "s").await;
        execute_rename_tab(&client, &first, "review")
            .await
            .expect("rename");
        let out = server.run(&["list-windows", "-t", "s", "-F", "#{window_name}"]);
        let names = String::from_utf8_lossy(&out.stdout);
        let names: Vec<_> = names.lines().collect();
        assert_eq!(names, vec!["review", "active"]);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn focusing_a_pane_switches_to_its_window_first() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-focus-window");
        server.run_ok(&["new-session", "-d", "-s", "s", "-n", "one", "/bin/sh"]);
        server.run_ok(&["new-window", "-d", "-t", "s", "-n", "two", "/bin/sh"]);
        let client = rig_client(&server, "s").await;
        let out = server.run(&[
            "list-panes",
            "-a",
            "-F",
            "#{window_name}\t#{window_id}\t#{pane_id}",
        ]);
        let rows = String::from_utf8_lossy(&out.stdout);
        let target = rows
            .lines()
            .find(|line| line.starts_with("two\t"))
            .expect("second window pane");
        let mut fields = target.split('\t');
        let _name = fields.next();
        let window = fields.next().expect("window id");
        let pane = fields.next().expect("pane id");

        execute_focus_pane(&client, Some(window), pane)
            .await
            .expect("focus pane");

        let active = server.run(&[
            "display-message",
            "-p",
            "-t",
            "s",
            "#{window_name}\t#{pane_id}",
        ]);
        assert_eq!(
            String::from_utf8_lossy(&active.stdout).trim(),
            format!("two\t{pane}")
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn focusing_a_background_workspace_agent_selects_its_window() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-focus-background-workspace");
        server.run_ok(&["new-session", "-d", "-s", "alpha", "/bin/sh"]);
        server.run_ok(&["new-session", "-d", "-s", "beta", "-n", "one", "/bin/sh"]);
        server.run_ok(&["new-window", "-d", "-t", "beta", "-n", "two", "/bin/sh"]);
        let client = rig_client(&server, "alpha").await;
        let out = server.run(&[
            "list-panes",
            "-t",
            "beta:two",
            "-F",
            "#{window_id}\t#{pane_id}",
        ]);
        let target = String::from_utf8_lossy(&out.stdout);
        let (window, pane) = target.trim().split_once('\t').expect("window and pane ids");

        execute(&client, Intent::SwitchWorkspace("beta".into()), "")
            .await
            .expect("switch workspace");
        execute_focus_pane(&client, Some(window), pane)
            .await
            .expect("focus background agent");

        assert_eq!(
            client
                .command("display-message -p '#{session_name}\t#{window_name}\t#{pane_id}'")
                .await
                .expect("active target"),
            vec![format!("beta\ttwo\t{pane}")]
        );
        client.shutdown().await;
    }

    #[test]
    fn folder_names_sanitize_for_tmux() {
        use std::path::Path;
        assert_eq!(session_name_from_folder(Path::new("/a/cyclops")), "cyclops");
        // `.` and `:` are tmux target syntax; a dotfile folder must not
        // produce a name tmux reads as "window of the empty session".
        assert_eq!(
            session_name_from_folder(Path::new("/a/my.project")),
            "my-project"
        );
        assert_eq!(session_name_from_folder(Path::new("/a/.config")), "config");
        assert_eq!(
            session_name_from_folder(Path::new("/a/line\nbreak")),
            "line-break"
        );
        assert_eq!(session_name_from_folder(Path::new("/")), "workspace");
    }

    #[test]
    fn duplicate_workspace_names_get_a_suffix() {
        let taken = vec!["cyclops".to_string(), "cyclops-2".to_string()];
        assert_eq!(unique_session_name("cyclops", &taken), "cyclops-3");
        assert_eq!(unique_session_name("fresh", &taken), "fresh");
    }

    #[test]
    fn folder_rename_targets_the_new_folders_basename() {
        assert_eq!(
            folder_rename("old", "/a/cyclops", &[]),
            Some("cyclops".to_string())
        );
    }

    #[test]
    fn folder_rename_is_none_once_the_name_already_matches() {
        assert_eq!(folder_rename("cyclops", "/a/cyclops", &[]), None);
    }

    #[test]
    fn folder_rename_ignores_an_empty_or_blank_cwd() {
        assert_eq!(folder_rename("cyclops", "", &[]), None);
        assert_eq!(folder_rename("cyclops", "   ", &[]), None);
    }

    #[test]
    fn folder_rename_lands_on_a_suffix_and_then_holds_still() {
        let taken = vec!["cyclops".to_string()];
        let next = folder_rename("old", "/a/cyclops", &taken).expect("collision suffix");
        assert_eq!(next, "cyclops-2");
        // Probing again with the suffixed name as `current` must return
        // None — otherwise a folder-following workspace that collided once
        // would rename itself on every subsequent probe.
        assert_eq!(folder_rename(&next, "/a/cyclops", &taken), None);
    }

    #[test]
    fn folder_rename_sanitizes_like_session_name_from_folder() {
        assert_eq!(
            folder_rename("old", "/a/my.project", &[]),
            Some("my-project".to_string())
        );
    }

    #[tokio::test]
    async fn close_tab_removes_window() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-close-tab");
        server.run_ok(&["new-session", "-d", "-s", "closetab", "/bin/sh"]);
        server.run_ok(&[
            "new-window",
            "-d",
            "-t",
            "closetab",
            "-n",
            "extra",
            "/bin/sh",
        ]);
        let client = rig_client(&server, "closetab").await;
        client.command("select-window -t :1").await.expect("focus");
        execute_close_tab(&client, "@1").await.expect("close tab");
        let out = server.run(&["list-windows", "-t", "closetab", "-F", "#{window_name}"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let names: Vec<_> = stdout.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(names.len(), 1);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn stale_session_target_never_falls_through_to_a_prefix_match() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-exact-session");
        server.run_ok(&["new-session", "-d", "-s", "host", "/bin/sh"]);
        server.run_ok(&["new-session", "-d", "-s", "proj", "/bin/sh"]);
        server.run_ok(&["new-session", "-d", "-s", "project", "/bin/sh"]);
        let client = rig_client(&server, "host").await;
        server.run_ok(&["kill-session", "-t", "=proj"]);

        assert!(
            execute_close_workspace(&client, "proj", None)
                .await
                .is_err(),
            "a vanished exact target must fail instead of matching `project`"
        );
        let sessions = server.run(&["list-sessions", "-F", "#{session_name}"]);
        let sessions = String::from_utf8_lossy(&sessions.stdout);
        assert!(
            sessions.lines().any(|name| name == "project"),
            "the prefix-neighbor session must survive: {sessions}"
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn closing_the_attached_workspace_moves_to_a_survivor_first() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-close-active-session");
        server.run_ok(&["new-session", "-d", "-s", "alpha", "/bin/sh"]);
        server.run_ok(&["new-session", "-d", "-s", "beta", "/bin/sh"]);
        let client = rig_client(&server, "alpha").await;

        execute_close_workspace(&client, "alpha", Some("beta"))
            .await
            .expect("switch then close");

        assert_eq!(
            client
                .command("display-message -p '#{session_name}'")
                .await
                .expect("client remains live"),
            vec!["beta"]
        );
        let sessions = server.run(&["list-sessions", "-F", "#{session_name}"]);
        let sessions = String::from_utf8_lossy(&sessions.stdout);
        assert!(!sessions.lines().any(|name| name == "alpha"));
        assert!(sessions.lines().any(|name| name == "beta"));
        client.shutdown().await;
    }

    #[tokio::test]
    async fn zoom_toggles_tmux_zoom_flag() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-zoom");
        server.run_ok(&["new-session", "-d", "-s", "z", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "z"]);
        let client = rig_client(&server, "z").await;
        let pane = pane_ids(&server, "z")[0].clone();
        execute(&client, Intent::ZoomPane, &pane)
            .await
            .expect("zoom");
        let out = server.run(&["list-windows", "-t", "z", "-F", "#{window_zoomed_flag}"]);
        let zoomed = String::from_utf8_lossy(&out.stdout);
        assert_eq!(zoomed.trim(), "1", "window should be zoomed with 2+ panes");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_split_from_second_client_converges() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-concur");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let client = rig_client(&server, "s").await;
        server.run_ok(&["split-window", "-h", "-t", "s"]);
        let before = pane_ids(&server, "s").len();
        server.run_ok(&["split-window", "-v", "-t", "s"]);
        let after = pane_ids(&server, "s").len();
        assert_eq!(after, before + 1);
        let model = crate::sync::fetch_session_model("s", Some(server.socket())).expect("model");
        assert_eq!(
            crate::layout::pane_ids_in_layout(&model.active_tab().layout).len(),
            after
        );
        let _ = client;
    }
}
