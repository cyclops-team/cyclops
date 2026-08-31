//! Daemon configuration: `$CYCLOPS_HOME/config.toml`.
//!
//! Data-only parse (v1 keeper): the file is TOML values, never code.
//! Unknown keys and wrong-typed coordinator values warn and are ignored so an
//! old daemon keeps booting against a newer config file. A missing file is a
//! valid empty config: the daemon watches nothing and status still answers.

use std::io::Read as _;
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
    /// Cap on observing the first durable disposition of a head whose cached
    /// pane verdict says the worker can decide immediately.
    pub receipt_block_ms: u64,
    /// One admin notification when a delivery has been held in gating this
    /// long (working pane, human typing, detached session). Visibility for
    /// wedged holds; the delivery itself keeps waiting for events.
    pub gate_hold_notify_ms: u64,
    /// One optional, bounded reminder for a doorbell that remains unclaimed.
    ///
    /// `None` is the shipped default. A configured positive duration arms one
    /// exact-attempt timer after the first proven doorbell notification.
    pub unclaimed_reminder_ms: Option<u64>,
    /// Opt-in escape hatch for a notification already pasted into a composer
    /// but left in verify-failed attention. The daemon may press Enter once
    /// after this delay without exact composer-content proof.
    pub force_notification_submit: bool,
    /// Delay retained while the escape hatch is off, so toggling it back on
    /// restores the operator's chosen position.
    pub force_notification_submit_delay_ms: u64,
    /// Write `role • state` onto an adopted pane's tmux border. On by
    /// default: a named pane that does not say its name is the whole
    /// feature missing. Off leaves every tmux option untouched.
    pub chrome: bool,
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
            unclaimed_reminder_ms: None,
            force_notification_submit: false,
            force_notification_submit_delay_ms: 5_000,
            chrome: true,
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
                "unclaimed_reminder_ms" => match ms_value(&value) {
                    Some(0) => cfg.unclaimed_reminder_ms = None,
                    Some(v) => cfg.unclaimed_reminder_ms = Some(v),
                    None => warnings.push(format!(
                        "`unclaimed_reminder_ms` must be a non-negative integer, not a {}; reminders remain off",
                        value.type_str()
                    )),
                },
                "force_notification_submit" => match value.as_str() {
                    Some("on") => cfg.force_notification_submit = true,
                    Some("off") => cfg.force_notification_submit = false,
                    _ => warnings.push(format!(
                        "`force_notification_submit` must be \"on\" or \"off\", not {value}; leaving it off"
                    )),
                },
                "force_notification_submit_delay_ms" => match ms_value(&value) {
                    Some(v) if v <= 20_000 => cfg.force_notification_submit_delay_ms = v,
                    Some(_) => warnings.push(
                        "`force_notification_submit_delay_ms` must be between 0 and 20000; using 5000"
                            .to_string(),
                    ),
                    None => warnings.push(format!(
                        "`force_notification_submit_delay_ms` must be a non-negative integer, not a {}; using 5000",
                        value.type_str()
                    )),
                },
                // One physical TOML file carries settings for several
                // product owners. The daemon acknowledges these top-level
                // keys so a shared file stays quiet, but must not interpret
                // or validate them: cyclops-theme owns `theme`, and the
                // workspace launcher owns `default_workspace`.
                "theme" | "default_workspace" => {}
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
                // Owned by cyclops-workspace. Recognize the table so one
                // shared config file does not make the daemon warn about
                // another shipped binary's settings.
                "workspace" => {}
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

/// Persist the operator's force-submit choice without disturbing any other
/// daemon or workspace setting. The caller updates the live runtime only
/// after this atomic replacement succeeds.
pub(crate) fn save_force_notification_submit(
    home: &Path,
    enabled: bool,
    delay_ms: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(delay_ms <= 20_000, "force-submit delay exceeds 20 seconds");
    let root = cyclops_state::StateRoot::open_or_create(home)?;
    let mut table: toml::Table = match root.open_read(Path::new("config.toml"))? {
        Some(mut file) => {
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            text.parse()?
        }
        None => toml::Table::new(),
    };
    table.insert(
        "force_notification_submit".into(),
        toml::Value::String(if enabled { "on" } else { "off" }.into()),
    );
    table.insert(
        "force_notification_submit_delay_ms".into(),
        toml::Value::Integer(i64::try_from(delay_ms).expect("20 seconds fits i64")),
    );
    let mut body = toml::to_string_pretty(&table)?;
    if !body.ends_with('\n') {
        body.push('\n');
    }
    root.replace_file(Path::new("config.toml"), body.as_bytes())?;
    Ok(())
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
[workspace]
sidebar_width = 28
[workspace.bindings]
next_tab = "Alt+n"
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
    fn client_owned_keys_are_not_daemon_configuration() {
        let (_, warnings) =
            Config::parse("theme = 3\ndefault_workspace = 3\n", Path::new("/h")).unwrap();

        assert!(
            warnings.is_empty(),
            "the daemon must leave client-owned keys to their owners: {warnings:?}"
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
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    /// One mistake, one warning.
    ///
    /// The daemon acknowledges `theme` only because it shares a file with
    /// the theme engine. It never gives the key a daemon meaning.
    #[test]
    fn a_wrong_typed_theme_key_is_the_theme_engine_s_to_complain_about() {
        let home = cyclops_proto::scratch::scratch_dir("cfg-theme-key");
        std::fs::create_dir_all(&home).expect("create scratch home");
        std::fs::write(home.join("config.toml"), "theme = 3\n").expect("write config");

        let (_, warnings) = Config::load(&home).expect("load config");
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
        assert_eq!(cfg.unclaimed_reminder_ms, None);
        assert!(!cfg.force_notification_submit);
        assert_eq!(cfg.force_notification_submit_delay_ms, 5_000);

        let text = "ack_timeout_ms = 200\ndelivery_retry_max = 0\nreceipt_block_ms = 900\ngate_hold_notify_ms = 300\nunclaimed_reminder_ms = 45000\nforce_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 0\n";
        let (cfg, warnings) = Config::parse(text, Path::new("/h")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.ack_timeout_ms, 200);
        assert_eq!(cfg.delivery_retry_max, 0);
        assert_eq!(cfg.receipt_block_ms, 900);
        assert_eq!(cfg.gate_hold_notify_ms, 300);
        assert_eq!(cfg.unclaimed_reminder_ms, Some(45_000));
        assert!(cfg.force_notification_submit);
        assert_eq!(cfg.force_notification_submit_delay_ms, 0);

        let (cfg, warnings) =
            Config::parse("unclaimed_reminder_ms = 0\n", Path::new("/h")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.unclaimed_reminder_ms, None);
    }

    #[test]
    fn force_submit_delay_refuses_values_above_the_ui_contract() {
        let (cfg, warnings) = Config::parse(
            "force_notification_submit = \"on\"\nforce_notification_submit_delay_ms = 20001\n",
            Path::new("/h"),
        )
        .unwrap();
        assert!(cfg.force_notification_submit);
        assert_eq!(cfg.force_notification_submit_delay_ms, 5_000);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn saving_force_submit_preserves_unrelated_operator_settings() {
        let home = cyclops_proto::scratch::scratch_dir("cfg-force-submit-save");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        std::fs::write(
            home.join("config.toml"),
            "theme = \"sorbet\"\n[workspace]\nsidebar_width = 31\n",
        )
        .expect("write config");

        save_force_notification_submit(&home, true, 20_000).expect("save force-submit setting");
        let saved = std::fs::read_to_string(home.join("config.toml")).expect("read saved config");
        assert!(saved.contains("theme = \"sorbet\""), "{saved}");
        assert!(saved.contains("sidebar_width = 31"), "{saved}");
        let (cfg, warnings) = Config::load(&home).expect("reload config");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(cfg.force_notification_submit);
        assert_eq!(cfg.force_notification_submit_delay_ms, 20_000);

        let _ = std::fs::remove_dir_all(&home);
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
