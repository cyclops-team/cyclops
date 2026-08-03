//! Which theme is active, and when it changes.
//!
//! Selection order: `$CYCLOPS_THEME`, then the `theme` key in
//! `$CYCLOPS_HOME/config.toml`, then the default "dark". A bare name
//! resolves to `<themes dir>/<name>.toml`; a value containing a path
//! separator or ending in `.toml` is used as a path directly. When nothing
//! loads, the compiled default table renders (silently for the default
//! name, with a warning when the choice was explicit).
//!
//! Hot reload is a stat, not a watcher: [`ThemeWatch::refresh`] compares
//! the file's (mtime, length) stamp and reloads on change. Long-lived
//! renderers call it when an event already woke them, so an edit to the
//! active theme applies on the next render. Chosen over an fs watch to
//! avoid a watcher thread and platform-specific backends; the zero-polling
//! contract holds because no timer exists, the stat rides the event that
//! is about to repaint anyway. One-shot commands reload by construction.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::Theme;

/// Environment override for the active theme: a name or a .toml path.
pub const THEME_ENV: &str = "CYCLOPS_THEME";

/// The shipped default.
const DEFAULT_THEME: &str = "dark";

/// The resolved active theme.
pub struct Selection {
    pub theme: Theme,
    /// Where the theme file is or would be. None only when a bare name had
    /// no themes directory to resolve against. A watcher keeps watching
    /// this path even when the file does not exist yet.
    pub path: Option<PathBuf>,
    /// Human-readable load warnings, for the caller to surface once.
    pub warnings: Vec<String>,
}

/// Resolve and load the active theme, reading `$CYCLOPS_THEME`.
pub fn active(home: &Path) -> Selection {
    let env = std::env::var(THEME_ENV).ok();
    active_with(env.as_deref(), home)
}

/// [`active`] with the environment override passed in. Tests use this to
/// stay deterministic without touching process-global env state.
pub fn active_with(env_override: Option<&str>, home: &Path) -> Selection {
    let mut warnings = Vec::new();
    let env = env_override.map(str::trim).filter(|s| !s.is_empty());
    let (name, explicit) = match env {
        Some(s) => (s.to_string(), true),
        None => match config_theme(home, &mut warnings) {
            Some(s) => (s, true),
            None => (DEFAULT_THEME.to_string(), false),
        },
    };
    let path = resolve_path(&name, home);
    let theme = match path.as_deref().filter(|p| p.is_file()) {
        Some(p) => match Theme::load(p) {
            Ok((t, mut w)) => {
                warnings.append(&mut w);
                t
            }
            Err(e) => {
                warnings.push(format!("{e}. Using built-in colors."));
                Theme::default()
            }
        },
        None => {
            // The default name missing its file is a bare install, not a
            // problem. An explicit choice that resolves nowhere gets said.
            if explicit {
                warnings.push(match &path {
                    Some(p) => format!(
                        "theme \"{name}\" not found (looked for {}). Using built-in colors.",
                        p.display()
                    ),
                    None => format!(
                        "theme \"{name}\" not found (no themes directory at {} or ./themes). Using built-in colors.",
                        home.join("themes").display()
                    ),
                });
            }
            Theme::default()
        }
    };
    Selection {
        theme,
        path,
        warnings,
    }
}

/// The themes directory: `<home>/themes` if it exists, else `./themes`
/// relative to the working directory (the repo layout), else nothing.
/// Mirrors the manifest directory fallback in the daemon config.
pub fn themes_dir(home: &Path) -> Option<PathBuf> {
    let d = home.join("themes");
    if d.is_dir() {
        return Some(d);
    }
    let cwd = PathBuf::from("themes");
    if cwd.is_dir() {
        return Some(cwd);
    }
    None
}

/// A theme value to the file it names.
fn resolve_path(name: &str, home: &Path) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) || name.ends_with(".toml") {
        return Some(PathBuf::from(name));
    }
    themes_dir(home).map(|d| d.join(format!("{name}.toml")))
}

/// The `theme` key from `<home>/config.toml`, read tolerantly: the daemon
/// owns real config validation, so a missing or unparseable file stays
/// quiet here and only a wrong-typed `theme` key gets a warning.
fn config_theme(home: &Path, warnings: &mut Vec<String>) -> Option<String> {
    let path = home.join("config.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    match table.get("theme") {
        None => None,
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(other) => {
            warnings.push(format!(
                "`theme` in {} must be a string, not a {}; using the default \"{DEFAULT_THEME}\"",
                path.display(),
                other.type_str()
            ));
            None
        }
    }
}

/// (mtime, length) stamp for change detection.
fn stamp_of(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The active theme held by a long-lived renderer, reloading on change.
/// See the module doc for the reload contract.
pub struct ThemeWatch {
    path: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    theme: Theme,
    warnings: Vec<String>,
}

impl ThemeWatch {
    /// Watch the active theme selection, reading `$CYCLOPS_THEME`.
    pub fn new(home: &Path) -> ThemeWatch {
        let env = std::env::var(THEME_ENV).ok();
        ThemeWatch::with_env(env.as_deref(), home)
    }

    /// [`ThemeWatch::new`] with the environment override passed in.
    pub fn with_env(env_override: Option<&str>, home: &Path) -> ThemeWatch {
        let sel = active_with(env_override, home);
        let stamp = sel.path.as_deref().and_then(stamp_of);
        ThemeWatch {
            path: sel.path,
            stamp,
            theme: sel.theme,
            warnings: sel.warnings,
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Warnings from the most recent load, for surfacing once per change.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Re-check the watched file and reload it when its stamp moved.
    /// Returns true when the palette changed (including falling back to
    /// built-in colors after the file disappeared). A file rewritten with
    /// identical length inside the filesystem's mtime granularity is
    /// caught on the first refresh after the clock advances. A file that
    /// changed into invalid TOML keeps the previous colors and says so in
    /// [`ThemeWatch::warnings`].
    pub fn refresh(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        match stamp_of(&path) {
            None => {
                if self.stamp.take().is_none() {
                    return false;
                }
                self.theme = Theme::default();
                self.warnings.clear();
                true
            }
            Some(s) if Some(s) == self.stamp => false,
            Some(s) => {
                self.stamp = Some(s);
                match Theme::load(&path) {
                    Ok((theme, warnings)) => {
                        self.theme = theme;
                        self.warnings = warnings;
                        true
                    }
                    Err(e) => {
                        self.warnings = vec![format!("{e}. Keeping the previous colors.")];
                        false
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens;

    fn write_theme(dir: &Path, name: &str, body: &str) -> PathBuf {
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).expect("mkdir themes");
        let path = themes.join(format!("{name}.toml"));
        std::fs::write(&path, body).expect("write theme");
        path
    }

    #[test]
    fn default_selection_loads_dark_when_present() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "[surface]\ndim = \"#111111\"\n");
        let sel = active_with(None, home.path());
        assert!(sel.warnings.is_empty(), "{:?}", sel.warnings);
        assert_eq!(sel.theme.name(), "dark");
        assert_eq!(sel.theme.resolve(tokens::SURFACE_DIM).rgb, (17, 17, 17));
    }

    #[test]
    fn bare_home_is_quietly_built_in() {
        let home = tempfile::tempdir().expect("tempdir");
        let sel = active_with(None, home.path());
        assert!(sel.warnings.is_empty(), "{:?}", sel.warnings);
        assert_eq!(sel.theme.name(), "built-in");
        assert_eq!(sel.theme.resolve(tokens::SURFACE_DIM).rgb, (128, 128, 128));
    }

    #[test]
    fn env_beats_config_beats_default() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "[surface]\ndim = \"#111111\"\n");
        write_theme(home.path(), "solar", "[surface]\ndim = \"#222222\"\n");
        write_theme(home.path(), "lunar", "[surface]\ndim = \"#333333\"\n");
        std::fs::write(home.path().join("config.toml"), "theme = \"solar\"\n")
            .expect("write config");
        // Config picks solar over the default dark.
        let sel = active_with(None, home.path());
        assert_eq!(sel.theme.name(), "solar");
        // Env picks lunar over both.
        let sel = active_with(Some("lunar"), home.path());
        assert_eq!(sel.theme.name(), "lunar");
        // Blank env falls through to the config.
        let sel = active_with(Some("  "), home.path());
        assert_eq!(sel.theme.name(), "solar");
    }

    #[test]
    fn env_accepts_a_direct_path() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = home.path().join("elsewhere.toml");
        std::fs::write(&path, "[surface]\ndim = \"#444444\"\n").expect("write theme");
        let sel = active_with(Some(path.to_str().expect("utf8 path")), home.path());
        assert!(sel.warnings.is_empty(), "{:?}", sel.warnings);
        assert_eq!(sel.theme.name(), "elsewhere");
        assert_eq!(sel.theme.resolve(tokens::SURFACE_DIM).rgb, (68, 68, 68));
    }

    #[test]
    fn explicit_missing_theme_warns_and_falls_back() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "");
        let sel = active_with(Some("ghost"), home.path());
        assert_eq!(sel.theme.name(), "built-in");
        assert_eq!(sel.warnings.len(), 1, "{:?}", sel.warnings);
        assert!(sel.warnings[0].contains("theme \"ghost\" not found"));
        assert!(sel.warnings[0].contains("ghost.toml"));
    }

    #[test]
    fn wrong_typed_config_key_warns_and_uses_the_default() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "[surface]\ndim = \"#111111\"\n");
        std::fs::write(home.path().join("config.toml"), "theme = 3\n").expect("write config");
        let sel = active_with(None, home.path());
        assert_eq!(sel.theme.name(), "dark");
        assert_eq!(sel.warnings.len(), 1, "{:?}", sel.warnings);
        assert!(sel.warnings[0].contains("`theme`"));
    }

    #[test]
    fn broken_theme_file_warns_and_falls_back() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "[surface\n");
        let sel = active_with(None, home.path());
        assert_eq!(sel.theme.name(), "built-in");
        assert_eq!(sel.warnings.len(), 1, "{:?}", sel.warnings);
        assert!(sel.warnings[0].contains("isn't valid TOML"));
    }

    #[test]
    fn refresh_applies_edits_ignores_no_change_and_survives_bad_toml() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = write_theme(home.path(), "dark", "[surface]\ndim = \"#111111\"\n");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (17, 17, 17));
        // Untouched file: no change.
        assert!(!watch.refresh());
        // A length-changing edit applies on the next refresh.
        std::fs::write(&path, "[surface]\ndim = \"#222222\"\nname = \"dark\"\n")
            .expect("rewrite theme");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (34, 34, 34));
        assert!(!watch.refresh());
        // A same-length edit is caught through the mtime half of the stamp;
        // the timestamp is forced forward so granularity cannot hide it.
        std::fs::write(&path, "[surface]\ndim = \"#333333\"\nname = \"dark\"\n")
            .expect("rewrite theme");
        let f = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open theme");
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(2))
            .expect("bump mtime");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (51, 51, 51));
        // Broken TOML keeps the previous colors and records a warning.
        std::fs::write(&path, "[surface\n").expect("break theme");
        assert!(!watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (51, 51, 51));
        assert_eq!(watch.warnings().len(), 1, "{:?}", watch.warnings());
        // Deleting the file falls back to built-in colors, once.
        std::fs::remove_file(&path).expect("remove theme");
        assert!(watch.refresh());
        assert_eq!(
            watch.theme().resolve(tokens::SURFACE_DIM).rgb,
            (128, 128, 128)
        );
        assert!(!watch.refresh());
        // The path stays watched: a recreated file loads again.
        std::fs::write(&path, "[surface]\ndim = \"#444444\"\n").expect("recreate theme");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (68, 68, 68));
    }

    #[test]
    fn watch_on_a_bare_home_never_changes() {
        let home = tempfile::tempdir().expect("tempdir");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert_eq!(watch.theme().name(), "built-in");
        assert!(!watch.refresh());
    }

    #[test]
    fn watch_picks_up_a_theme_created_after_start() {
        let home = tempfile::tempdir().expect("tempdir");
        // The themes dir exists so the default name resolves to a path,
        // but the file itself arrives later.
        std::fs::create_dir_all(home.path().join("themes")).expect("mkdir themes");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert_eq!(watch.theme().name(), "built-in");
        assert!(!watch.refresh());
        write_theme(home.path(), "dark", "[surface]\ndim = \"#555555\"\n");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (85, 85, 85));
    }
}
