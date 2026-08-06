//! Read workspace-relevant keys from config.toml.

use std::path::{Path, PathBuf};

/// Tmux server connection from config.toml.
#[derive(Debug, Clone, Default)]
pub struct TmuxConfig {
    pub socket: Option<String>,
    pub config_file: Option<PathBuf>,
}

/// Load `tmux_socket` and `tmux_config` from `<home>/config.toml`.
pub fn load_tmux_config(home: &Path) -> TmuxConfig {
    let path = home.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return TmuxConfig::default();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return TmuxConfig::default();
    };
    TmuxConfig {
        socket: table
            .get("tmux_socket")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        config_file: table
            .get("tmux_config")
            .and_then(|v| v.as_str())
            .map(PathBuf::from),
    }
}
