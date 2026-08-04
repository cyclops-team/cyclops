//! UI intents mapped to tmux operations. The model updates only from
//! reconciliation after tmux replies and notifications — never here.

#![allow(dead_code)]

use std::path::Path;

use cyclops_tmux::{quote_arg, ControlClient, TmuxError};

/// Structural workspace actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    NextTab,
    PrevTab,
    SelectTab(usize),
    NewTab,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    SplitRight,
    SplitDown,
    ClosePane,
    ZoomPane,
    RenameTab,
    CloseTab,
    SwitchWorkspace(String),
    NewWorkspace,
    RenameWorkspace,
    CloseWorkspace,
}

/// Issue one intent against tmux. Does not mutate the workspace model.
pub async fn execute(
    client: &ControlClient,
    intent: Intent,
    active_pane: &str,
) -> Result<(), TmuxError> {
    match intent {
        Intent::NextTab => {
            client.command("next-window").await?;
        }
        Intent::PrevTab => {
            client.command("previous-window").await?;
        }
        Intent::SelectTab(n) => {
            let idx = n.saturating_sub(1);
            client.command(&format!("select-window -t :{idx}")).await?;
        }
        Intent::NewTab => {
            client.command("new-window -d").await?;
            client.command("select-window -t :+").await?;
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
        Intent::RenameTab => {}
        Intent::CloseTab => {
            client.command("kill-window").await?;
        }
        Intent::SwitchWorkspace(name) => {
            client
                .command(&format!("switch-client -t {}", quote_arg(&name)))
                .await?;
        }
        Intent::NewWorkspace | Intent::RenameWorkspace | Intent::CloseWorkspace => {}
    }
    Ok(())
}

/// Create a workspace (tmux session) from a project folder.
pub async fn execute_new_workspace(
    client: &ControlClient,
    folder: &Path,
) -> Result<String, TmuxError> {
    let name = session_name_from_folder(folder);
    let path = folder.to_string_lossy();
    client
        .command(&format!(
            "new-session -d -s {} -c {}",
            quote_arg(&name),
            quote_arg(path.as_ref())
        ))
        .await?;
    client
        .command(&format!("switch-client -t {}", quote_arg(&name)))
        .await?;
    Ok(name)
}

/// Rename the attached session.
pub async fn execute_rename_workspace(client: &ControlClient, name: &str) -> Result<(), TmuxError> {
    client
        .command(&format!("rename-session {}", quote_arg(name)))
        .await?;
    Ok(())
}

/// Close the attached session.
pub async fn execute_close_workspace(client: &ControlClient) -> Result<(), TmuxError> {
    client.command("kill-session").await?;
    Ok(())
}

fn session_name_from_folder(folder: &Path) -> String {
    folder
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string()
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

/// Rename the active window after the user supplies a name.
pub async fn execute_rename(client: &ControlClient, name: &str) -> Result<(), TmuxError> {
    client
        .command(&format!("rename-window {}", quote_arg(name)))
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

impl From<crate::bindings::BindingAction> for Intent {
    fn from(action: crate::bindings::BindingAction) -> Self {
        use crate::bindings::BindingAction::*;
        match action {
            NextTab => Intent::NextTab,
            PrevTab => Intent::PrevTab,
            SelectTab(n) => Intent::SelectTab(n),
            NewTab => Intent::NewTab,
            FocusLeft => Intent::FocusLeft,
            FocusRight => Intent::FocusRight,
            FocusUp => Intent::FocusUp,
            FocusDown => Intent::FocusDown,
            SplitRight => Intent::SplitRight,
            SplitDown => Intent::SplitDown,
            ClosePane => Intent::ClosePane,
            ZoomPane => Intent::ZoomPane,
            RenameTab => Intent::RenameTab,
            CloseTab => Intent::CloseTab,
            NextWorkspace | PrevWorkspace | NewWorkspace | RenameWorkspace | CloseWorkspace => {
                unreachable!("workspace actions use dedicated handlers")
            }
            ToggleEventPanel | Detach => unreachable!("handled in app"),
        }
    }
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
        let folder = "/tmp/cyclops-ws-create";
        std::fs::create_dir_all(folder).expect("folder");
        server.run_ok(&["new-session", "-d", "-s", "host", "/bin/sh"]);
        let client = rig_client(&server, "host").await;
        let name = execute_new_workspace(&client, std::path::Path::new(folder))
            .await
            .expect("create");
        assert_eq!(name, "cyclops-ws-create");
        let out = server.run(&["display-message", "-p", "-t", &name, "#{session_path}"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            folder,
            "session default directory should match folder"
        );
        client.shutdown().await;
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
        let src = "/tmp/cyclops-split-src";
        std::fs::create_dir_all(src).expect("split src dir");
        server.run_ok(&["new-session", "-d", "-s", "s", "-c", src, "/bin/sh"]);
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
        assert_eq!(
            path.trim(),
            src,
            "new split pane should inherit source pane_current_path"
        );
        client.shutdown().await;
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
    async fn rename_tab_updates_window_name() {
        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("intent-rename");
        server.run_ok(&["new-session", "-d", "-s", "s", "/bin/sh"]);
        let client = rig_client(&server, "s").await;
        execute_rename(&client, "review").await.expect("rename");
        let out = server.run(&["list-windows", "-t", "s", "-F", "#{window_name}"]);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "review");
        client.shutdown().await;
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
        let pane = pane_ids(&server, "closetab:1")[0].clone();
        execute(&client, Intent::CloseTab, &pane)
            .await
            .expect("close tab");
        let out = server.run(&["list-windows", "-t", "closetab", "-F", "#{window_name}"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let names: Vec<_> = stdout.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(names.len(), 1);
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
