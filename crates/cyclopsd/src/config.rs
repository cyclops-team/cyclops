//! Daemon configuration: `$CYCLOPS_HOME/config.toml`.
//!
//! Data-only parse (v1 keeper): the file is TOML values, never code.
//! Unknown keys and wrong-typed values warn and are ignored so an old
//! daemon keeps booting against a newer config file. A missing file is a
//! valid empty config: the daemon watches nothing and status still answers.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

#[derive(Debug, Clone)]
pub struct Config {
    /// Cyclops home directory. The socket and default manifest dir live here.
    pub home: PathBuf,
    /// tmux sessions to watch. Empty means watch nothing.
    pub sessions: Vec<String>,
    /// `-L` socket name for every tmux interaction. None uses the default
    /// server. Tests and demos always set this.
    pub tmux_socket: Option<String>,
    /// `-f` config file for tmux clients. Tests point this at /dev/null.
    pub tmux_config: Option<PathBuf>,
    /// Explicit manifest directory. None falls back, see [`Config::manifest_dir`].
    pub manifest_dir: Option<PathBuf>,
    /// Tier-1 ACK window: how long a delivery waits for the manifest hook
    /// ACK before falling back to screen evidence.
    pub ack_timeout_ms: u64,
    /// Redelivery attempts after the first failure. The soak needed zero;
    /// one bounded retry is the ceiling, never a loop.
    pub delivery_retry_max: u32,
    /// Cap on how long msg.send blocks for a receipt on the idle path.
    pub receipt_block_ms: u64,
    /// One admin notification when a delivery has been held in gating this
    /// long (working pane, human typing, detached session). Visibility for
    /// wedged holds; the delivery itself keeps waiting for events.
    pub gate_hold_notify_ms: u64,
}

impl Config {
    /// Empty config rooted at `home`.
    pub fn defaults(home: &Path) -> Config {
        Config {
            home: home.to_path_buf(),
            sessions: Vec::new(),
            tmux_socket: None,
            tmux_config: None,
            manifest_dir: None,
            ack_timeout_ms: 1500,
            delivery_retry_max: 1,
            receipt_block_ms: 2500,
            gate_hold_notify_ms: 120_000,
        }
    }

    /// Load `<home>/config.toml`. A missing file yields the defaults.
    /// Returns the config plus human-readable warnings for the caller to log.
    pub fn load(home: &Path) -> anyhow::Result<(Config, Vec<String>)> {
        let path = home.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Config::defaults(home), Vec::new()));
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Config::parse(&text, home).with_context(|| format!("parse {}", path.display()))
    }

    /// Parse config text. Only TOML syntax errors fail; everything else
    /// degrades to a warning.
    pub fn parse(text: &str, home: &Path) -> anyhow::Result<(Config, Vec<String>)> {
        let table: toml::Table = toml::from_str(text)?;
        let mut cfg = Config::defaults(home);
        let mut warnings = Vec::new();
        for (key, value) in table {
            match key.as_str() {
                "sessions" => match value {
                    toml::Value::Array(items) => {
                        for item in items {
                            match item {
                                toml::Value::String(s) => cfg.sessions.push(s),
                                other => warnings.push(format!(
                                    "`sessions` entries must be strings; skipped a {}",
                                    other.type_str()
                                )),
                            }
                        }
                    }
                    other => warnings.push(format!(
                        "`sessions` must be an array of strings, not a {}; watching nothing",
                        other.type_str()
                    )),
                },
                "tmux_socket" => match value {
                    toml::Value::String(s) => cfg.tmux_socket = Some(s),
                    other => warnings.push(format!(
                        "`tmux_socket` must be a string, not a {}; using the default tmux server",
                        other.type_str()
                    )),
                },
                "tmux_config" => match value {
                    toml::Value::String(s) => cfg.tmux_config = Some(PathBuf::from(s)),
                    other => warnings.push(format!(
                        "`tmux_config` must be a string path, not a {}; ignored",
                        other.type_str()
                    )),
                },
                "manifest_dir" => match value {
                    toml::Value::String(s) => cfg.manifest_dir = Some(PathBuf::from(s)),
                    other => warnings.push(format!(
                        "`manifest_dir` must be a string path, not a {}; using the default",
                        other.type_str()
                    )),
                },
                "ack_timeout_ms" => match ms_value(&value) {
                    Some(v) => cfg.ack_timeout_ms = v,
                    None => warnings.push(format!(
                        "`ack_timeout_ms` must be a non-negative integer, not a {}; using {}",
                        value.type_str(),
                        cfg.ack_timeout_ms
                    )),
                },
                "delivery_retry_max" => match ms_value(&value) {
                    Some(v) => cfg.delivery_retry_max = v.min(u32::MAX as u64) as u32,
                    None => warnings.push(format!(
                        "`delivery_retry_max` must be a non-negative integer, not a {}; using {}",
                        value.type_str(),
                        cfg.delivery_retry_max
                    )),
                },
                "receipt_block_ms" => match ms_value(&value) {
                    Some(v) => cfg.receipt_block_ms = v,
                    None => warnings.push(format!(
                        "`receipt_block_ms` must be a non-negative integer, not a {}; using {}",
                        value.type_str(),
                        cfg.receipt_block_ms
                    )),
                },
                "gate_hold_notify_ms" => match ms_value(&value) {
                    Some(v) => cfg.gate_hold_notify_ms = v,
                    None => warnings.push(format!(
                        "`gate_hold_notify_ms` must be a non-negative integer, not a {}; using {}",
                        value.type_str(),
                        cfg.gate_hold_notify_ms
                    )),
                },
                unknown => warnings.push(format!("unknown config key `{unknown}` ignored")),
            }
        }
        Ok((cfg, warnings))
    }

    /// Resolve the manifest directory: explicit config value first, then
    /// `<home>/manifests` if it exists, then `./manifests` relative to the
    /// working directory. None when nothing is found.
    pub fn manifest_dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.manifest_dir {
            return Some(d.clone());
        }
        let default = self.home.join("manifests");
        if default.is_dir() {
            return Some(default);
        }
        let cwd = PathBuf::from("manifests");
        if cwd.is_dir() {
            return Some(cwd);
        }
        None
    }
}

/// Non-negative integer TOML value for the timing/count knobs.
fn ms_value(value: &toml::Value) -> Option<u64> {
    match value {
        toml::Value::Integer(n) if *n >= 0 => Some(*n as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_config_parses() {
        let text = r#"
sessions = ["main", "aux"]
tmux_socket = "cyc-test"
tmux_config = "/dev/null"
manifest_dir = "/private/tmp/manifests"
"#;
        let (cfg, warnings) = Config::parse(text, Path::new("/private/tmp/home")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.sessions, vec!["main", "aux"]);
        assert_eq!(cfg.tmux_socket.as_deref(), Some("cyc-test"));
        assert_eq!(cfg.tmux_config.as_deref(), Some(Path::new("/dev/null")));
        assert_eq!(
            cfg.manifest_dir(),
            Some(PathBuf::from("/private/tmp/manifests"))
        );
    }

    #[test]
    fn unknown_keys_warn_but_parse() {
        let (cfg, warnings) =
            Config::parse("sessions = [\"main\"]\nfuture_key = 3\n", Path::new("/h")).unwrap();
        assert_eq!(cfg.sessions, vec!["main"]);
        assert!(
            warnings.iter().any(|w| w.contains("future_key")),
            "{warnings:?}"
        );
    }

    #[test]
    fn wrong_types_warn_and_fall_back() {
        let (cfg, warnings) =
            Config::parse("sessions = \"main\"\ntmux_socket = 7\n", Path::new("/h")).unwrap();
        assert!(cfg.sessions.is_empty());
        assert!(cfg.tmux_socket.is_none());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn syntax_error_is_an_error() {
        assert!(Config::parse("sessions = [", Path::new("/h")).is_err());
    }

    #[test]
    fn delivery_knobs_default_and_override() {
        let (cfg, warnings) = Config::parse("", Path::new("/h")).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(cfg.ack_timeout_ms, 1500);
        assert_eq!(cfg.delivery_retry_max, 1);
        assert_eq!(cfg.receipt_block_ms, 2500);
        assert_eq!(cfg.gate_hold_notify_ms, 120_000);

        let text = "ack_timeout_ms = 200\ndelivery_retry_max = 0\nreceipt_block_ms = 900\ngate_hold_notify_ms = 300\n";
        let (cfg, warnings) = Config::parse(text, Path::new("/h")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.ack_timeout_ms, 200);
        assert_eq!(cfg.delivery_retry_max, 0);
        assert_eq!(cfg.receipt_block_ms, 900);
        assert_eq!(cfg.gate_hold_notify_ms, 300);
    }

    #[test]
    fn delivery_knobs_wrong_types_warn_and_keep_defaults() {
        let text = "ack_timeout_ms = \"soon\"\ndelivery_retry_max = -2\n";
        let (cfg, warnings) = Config::parse(text, Path::new("/h")).unwrap();
        assert_eq!(cfg.ack_timeout_ms, 1500);
        assert_eq!(cfg.delivery_retry_max, 1);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
