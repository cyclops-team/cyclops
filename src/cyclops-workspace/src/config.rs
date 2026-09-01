//! Read the tmux coordinator settings from the shared config boundary.

use std::path::{Path, PathBuf};

/// Tmux server connection from config.toml.
#[derive(Debug, Clone, Default)]
pub struct TmuxConfig {
    pub socket: Option<String>,
    pub config_file: Option<PathBuf>,
}

/// Load `tmux_socket` and `tmux_config` from `<home>/config.toml`.
///
/// The full-screen workspace starts a control-mode tmux client, so it must
/// refuse an unsafe coordinator config before it can fall back to tmux's
/// ambient server.
pub fn load_tmux_config(home: &Path) -> Result<TmuxConfig, String> {
    let config = cyclops_state::load_coordinator_config(home).map_err(|error| {
        format!(
            "read {}: {error}",
            home.join(cyclops_state::COORDINATOR_CONFIG_FILE).display()
        )
    })?;
    Ok(TmuxConfig {
        socket: config.coordinator.tmux_socket,
        config_file: config.coordinator.tmux_config,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn valid_nondefault_config_reaches_the_workspace_control_client() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-tmux-config-valid");
        let _ = std::fs::remove_dir_all(&home);
        cyclops_state::StateRoot::open_or_create(&home)
            .expect("create safe home")
            .replace_file(
                Path::new("config.toml"),
                b"tmux_socket = \"workspace-test\"\ntmux_config = \"/dev/null\"\n",
            )
            .expect("write safe config");

        let config = load_tmux_config(&home).expect("valid config loads");
        assert_eq!(config.socket.as_deref(), Some("workspace-test"));
        assert_eq!(config.config_file.as_deref(), Some(Path::new("/dev/null")));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn malformed_or_linked_config_refuses_before_workspace_can_default_tmux() {
        let malformed = cyclops_proto::scratch::scratch_dir("workspace-tmux-config-malformed");
        let _ = std::fs::remove_dir_all(&malformed);
        cyclops_state::StateRoot::open_or_create(&malformed)
            .expect("create safe home")
            .replace_file(Path::new("config.toml"), b"tmux_config = [")
            .expect("write malformed config");
        assert!(load_tmux_config(&malformed).is_err());

        let linked = cyclops_proto::scratch::scratch_dir("workspace-tmux-config-linked");
        let external = cyclops_proto::scratch::scratch_dir("workspace-tmux-config-external");
        for path in [&linked, &external] {
            let _ = std::fs::remove_dir_all(path);
            std::fs::create_dir_all(path).expect("create scratch directory");
        }
        std::fs::write(external.join("config.toml"), "tmux_socket = \"foreign\"\n")
            .expect("write external config");
        symlink(external.join("config.toml"), linked.join("config.toml")).expect("link config");
        assert!(load_tmux_config(&linked).is_err());

        let _ = std::fs::remove_dir_all(&malformed);
        let _ = std::fs::remove_dir_all(&linked);
        let _ = std::fs::remove_dir_all(&external);
    }
}
