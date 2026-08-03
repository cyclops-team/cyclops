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
    /// Theme name. The daemon never reads this field: `cyclops-theme`
    /// re-reads the same key out of the same file (`select::config_theme`)
    /// and that is what paints every surface, borders included. It is
    /// recognized here so a config carrying it does not warn as an unknown
    /// key, and for nothing else, exactly like `default_workspace`.
    pub theme: Option<String>,
    /// Write `role • state` onto an adopted pane's tmux border. On by
    /// default: a named pane that does not say its name is the whole
    /// feature missing. Off leaves every tmux option untouched.
    pub chrome: bool,
    /// Workspace `cyclops start` opens when it is given no name. The
    /// daemon builds no workspaces and never reads this; like `theme`, it
    /// is recognized here so a config carrying it does not warn as an
    /// unknown key.
    pub default_workspace: Option<String>,
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
            theme: None,
            chrome: true,
            default_workspace: None,
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
                // Recognized, never used, and deliberately silent about a
                // wrong type: cyclops-theme reads this same key out of
                // this same file and warns about it there. Two crates
                // parsing one key printed two warnings in two wordings for
                // one mistake, and the daemon's was the one describing a
                // fallback it does not have (nothing here reads the
                // value).
                "theme" => {
                    if let toml::Value::String(s) = value {
                        cfg.theme = Some(s);
                    }
                }
                "default_workspace" => match value {
                    toml::Value::String(s) => cfg.default_workspace = Some(s),
                    other => warnings.push(format!(
                        "`default_workspace` must be a string, not a {}; `cyclops start` will use the first watched session",
                        other.type_str()
                    )),
                },
                // Words, not a bool: the switch turns a visible thing on
                // and off, and `chrome = "off"` is what a person writes.
                "chrome" => match value.as_str() {
                    Some("on") => cfg.chrome = true,
                    Some("off") => cfg.chrome = false,
                    _ => warnings.push(format!(
                        "`chrome` must be \"on\" or \"off\", not {value}; leaving it {}",
                        if cfg.chrome { "on" } else { "off" }
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
theme = "dark"
default_workspace = "main"
"#;
        let (cfg, warnings) = Config::parse(text, Path::new("/private/tmp/home")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.sessions, vec!["main", "aux"]);
        assert_eq!(cfg.tmux_socket.as_deref(), Some("cyc-test"));
        assert_eq!(cfg.tmux_config.as_deref(), Some(Path::new("/dev/null")));
        assert_eq!(cfg.default_workspace.as_deref(), Some("main"));
        assert_eq!(cfg.theme.as_deref(), Some("dark"));
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
        let (cfg, warnings) = Config::parse(
            "sessions = \"main\"\ntmux_socket = 7\ntheme = 3\n",
            Path::new("/h"),
        )
        .unwrap();
        assert!(cfg.sessions.is_empty());
        assert!(cfg.tmux_socket.is_none());
        // Two, not three: `theme` is silent here on purpose, see below.
        assert!(cfg.theme.is_none());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    /// One mistake, one warning.
    ///
    /// The `theme` key is parsed twice on this machine: here, so a config
    /// carrying it is not an unknown key, and in cyclops-theme, which is
    /// the crate that resolves it and paints from it. Both used to
    /// complain about a wrong type in different words, and the daemon's
    /// version named a fallback it does not have, because nothing in the
    /// daemon reads the value at all.
    #[test]
    fn a_wrong_typed_theme_key_is_the_theme_engine_s_to_complain_about() {
        let home = cyclops_proto::scratch::scratch_dir("cfg-theme-key");
        std::fs::create_dir_all(&home).expect("create scratch home");
        std::fs::write(home.join("config.toml"), "theme = 3\n").expect("write config");

        let (cfg, warnings) = Config::load(&home).expect("load config");
        assert!(cfg.theme.is_none());
        assert!(
            !warnings.iter().any(|w| w.contains("theme")),
            "the daemon complained about a key it never reads: {warnings:?}"
        );

        // The crate that does read it says it, exactly once.
        let sel = cyclops_theme::active_with(None, &home);
        let said: Vec<&String> = sel
            .warnings
            .iter()
            .filter(|w| w.contains("`theme`"))
            .collect();
        assert_eq!(said.len(), 1, "{:?}", sel.warnings);

        std::fs::remove_dir_all(&home).ok();
    }

    /// The order matters and only one of the three is a supported install.
    ///
    /// `cyclops start` seeds `$CYCLOPS_HOME/manifests`, and that is the
    /// only manifest directory an installed pair of binaries has: nothing
    /// writes the config key, and `./manifests` exists only in a clone.
    /// So the home has to win over the working directory, and it has to be
    /// found with no key pointing at it, or a first run detects nothing.
    #[test]
    fn the_home_wins_over_the_working_directory_and_the_key_wins_over_both() {
        let home = cyclops_proto::scratch::scratch_dir("cfg-manifest-dir");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");

        // Nothing seeded yet, and no ./manifests under the test's working
        // directory (the crate root), so there is nothing to find.
        let (cfg, _) = Config::parse("", &home).unwrap();
        assert_eq!(cfg.manifest_dir(), None);

        std::fs::create_dir_all(home.join("manifests")).expect("seed");
        let (cfg, _) = Config::parse("", &home).unwrap();
        assert_eq!(cfg.manifest_dir(), Some(home.join("manifests")));

        // An explicit key still wins: a clone running out of the repo, and
        // every demo script, points at its own directory.
        let text = format!("manifest_dir = \"{}\"\n", home.join("elsewhere").display());
        let (cfg, warnings) = Config::parse(&text, &home).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.manifest_dir(), Some(home.join("elsewhere")));

        let _ = std::fs::remove_dir_all(&home);
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
    fn chrome_is_on_unless_the_file_says_off() {
        let (cfg, warnings) = Config::parse("", Path::new("/h")).unwrap();
        assert!(cfg.chrome, "{warnings:?}");
        let (cfg, warnings) = Config::parse("chrome = \"off\"\n", Path::new("/h")).unwrap();
        assert!(!cfg.chrome);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (cfg, warnings) = Config::parse("chrome = \"on\"\n", Path::new("/h")).unwrap();
        assert!(cfg.chrome);
        assert!(warnings.is_empty(), "{warnings:?}");
        // Anything else warns and changes nothing, including a bool: this
        // key takes words, and silently accepting `true` would leave two
        // spellings of the same switch in circulation.
        let (cfg, warnings) = Config::parse("chrome = true\n", Path::new("/h")).unwrap();
        assert!(cfg.chrome);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
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
