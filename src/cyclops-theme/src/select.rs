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
//! (mtime, length) stamps and reloads on change. Long-lived renderers call
//! it when an event already woke them, so a change applies on the next
//! render. Chosen over an fs watch to avoid a watcher thread and
//! platform-specific backends; the zero-polling contract holds because no
//! timer exists, the stat rides the event that is about to repaint anyway.
//! One-shot commands reload by construction.
//!
//! What is watched is the SELECTION, both halves of it: the config key
//! that picks a theme and the file that theme is. `cyclops theme <name>`
//! writes the key, so a watch that only knew its own file would leave a
//! running `cyclops ui` on the old palette forever.
//!
//! ```mermaid
//! flowchart TD
//!   R[refresh] --> C{config.toml stamp moved?}
//!   C -- yes --> S[re-resolve the selection, load it] --> T[new palette]
//!   C -- no --> F{theme file stamp moved?}
//!   F -- no --> K[nothing to do]
//!   F -- yes --> L{loads, and still sets every token it set before?}
//!   L -- yes --> T
//!   L -- no --> W[keep the colors on screen, one warning]
//! ```

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{tokens, Theme, ThemeError};

/// Environment override for the active theme: a name or a .toml path.
pub const THEME_ENV: &str = "CYCLOPS_THEME";

/// The shipped default.
const DEFAULT_THEME: &str = "dark";

/// Said on every reload a watch refuses. One phrase, because a reader who
/// sees it twice for two different reasons should recognize what it means
/// both times: nothing on screen moved.
const KEPT: &str = "Keeping the colors on screen.";

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
    load(choose(env_override, home), home)
}

/// Which file the selection names, before anything is read from it.
///
/// Split out from the load because a watch has a question the load cannot
/// answer without doing the work twice: has the config key started naming
/// a DIFFERENT file? Nothing here touches the theme file, so the warnings
/// it carries are about the choice and never about the file's contents.
struct Choice {
    name: String,
    path: Option<PathBuf>,
    /// True when a person named this theme (the env override or the config
    /// key) rather than it being the shipped default. Only an explicit
    /// choice that resolves nowhere is worth saying anything about.
    explicit: bool,
    warnings: Vec<String>,
}

fn choose(env_override: Option<&str>, home: &Path) -> Choice {
    let mut warnings = Vec::new();
    let env = env_override.map(str::trim).filter(|s| !s.is_empty());
    let (name, explicit) = match env {
        Some(s) => (s.to_string(), true),
        None => match config_theme(home, &mut warnings) {
            Some(s) => (s, true),
            None => (DEFAULT_THEME.to_string(), false),
        },
    };
    let path = path_for(&name, home);
    Choice {
        name,
        path,
        explicit,
        warnings,
    }
}

/// Read the chosen file. Only IO and TOML syntax are hard failures here;
/// everything softer becomes a warning and resolves through the compiled
/// defaults.
fn load(choice: Choice, home: &Path) -> Selection {
    let Choice {
        name,
        path,
        explicit,
        mut warnings,
    } = choice;
    let theme = match path.as_deref().filter(|p| p.is_file()) {
        Some(p) => match Theme::load(p) {
            Ok((t, mut w)) => {
                warnings.append(&mut w);
                t
            }
            Err(e) => {
                warnings.push(format!(
                    "{e}, so cyclops is using built-in colors until you fix it."
                ));
                Theme::default()
            }
        },
        None => {
            // The default name missing its file is a bare install, not a
            // problem. An explicit choice that resolves nowhere gets said.
            if explicit {
                warnings.push(match &path {
                    Some(p) => format!(
                        "no theme called \"{name}\" (looked for {}), so cyclops is using built-in colors; run cyclops theme to see what is there.",
                        p.display()
                    ),
                    None => format!(
                        "no theme called \"{name}\" (no themes directory at {} or ./themes), so cyclops is using built-in colors; run cyclops theme to see what is there.",
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

/// A theme value to the file it names: a bare name against the themes
/// directory, a path used as written.
///
/// Public because `cyclops theme <name>` checks a theme before writing the
/// config key that selects it, and a second resolver would let the command
/// approve one file while selection loaded another.
pub fn path_for(name: &str, home: &Path) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) || name.ends_with(".toml") {
        return Some(PathBuf::from(name));
    }
    themes_dir(home).map(|d| d.join(format!("{name}.toml")))
}

/// The `theme` key from `<home>/config.toml`, read tolerantly: the daemon
/// owns real config validation, so a missing or unparseable file stays
/// quiet here and only a wrong-typed `theme` key gets a warning.
fn config_theme(home: &Path, warnings: &mut Vec<String>) -> Option<String> {
    let text = std::fs::read_to_string(config_path(home)).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    match table.get("theme") {
        None => None,
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(other) => {
            warnings.push(format!(
                "`theme` in {} must be a string, not a {}; using the default \"{DEFAULT_THEME}\"",
                config_path(home).display(),
                other.type_str()
            ));
            None
        }
    }
}

fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// Set `theme = "<name>"` in the config, leaving the rest of the file
/// exactly as the person who wrote it left it.
///
/// A rewrite through a TOML serializer would cost the file its comments
/// and its key order, so this edits one line and adds nothing else. The
/// CLI's `cyclops theme <name>` and the workspace's theme picker both call
/// this rather than each carrying their own writer, which is how the two
/// used to drift from each other one edit at a time.
///
/// Steps:
/// 1. Read what is there. No file means one is created holding this key
///    and nothing else.
/// 2. Find a top-level `theme =` line, which means before the first
///    `[table]` header, and replace its value in place.
/// 3. With no such line, insert one at the end of the top-level keys.
/// 4. Write through a temp file and rename, so a crash mid-write cannot
///    leave a half-written config where a whole one was.
pub fn set_config_theme(home: &Path, name: &str) -> Result<(), String> {
    use std::io::Write;

    let path = config_path(home);
    let key = format!("theme = \"{name}\"");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let top = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    match lines[..top].iter().position(|l| is_theme_key(l)) {
        Some(i) => lines[i] = key,
        None => lines.insert(top, key),
    }
    let mut body = lines.join("\n");
    body.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::File::create(&tmp).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    f.write_all(body.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("write {}: {e}", path.display()))
}

/// A line that assigns the top-level `theme` key. Not a substring match:
/// `default_workspace = "theme"` and a commented-out `# theme = "dark"`
/// both contain the word and neither one sets anything.
fn is_theme_key(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("theme")
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

/// (mtime, length) stamp for change detection. None when the file is not
/// there, which is a stamp too: it is how a watch notices a delete.
fn stamp_of(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The active theme held by a long-lived renderer, reloading on change.
/// See the module doc for what is watched and the reload rule.
pub struct ThemeWatch {
    /// The `$CYCLOPS_THEME` override this watch was built with. Fixed for
    /// the life of the process: the environment does not change under a
    /// running program, so re-reading it per refresh would only be a way
    /// to disagree with the value the process started on.
    env: Option<String>,
    home: PathBuf,
    config: Option<(SystemTime, u64)>,
    path: Option<PathBuf>,
    file: Option<(SystemTime, u64)>,
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
        ThemeWatch {
            env: env_override.map(str::to_string),
            home: home.to_path_buf(),
            config: stamp_of(&config_path(home)),
            file: sel.path.as_deref().and_then(stamp_of),
            path: sel.path,
            theme: sel.theme,
            warnings: sel.warnings,
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Warnings since the last take, and the only way to read them.
    ///
    /// Draining rather than accumulating is what makes "one warning line"
    /// true: a caller logs what it takes, and a refresh that changes
    /// nothing hands back nothing, so a broken file is said once per edit
    /// instead of once per repaint.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Re-check the selection and reload it when something moved. True
    /// when the palette changed, which is when a renderer has to repaint.
    ///
    /// Steps, in the order that makes each one answerable:
    ///
    /// 1. The config first, because it decides WHICH file the rest of this
    ///    is about. A key naming a different file is a switch, and a
    ///    switch applies whole (see [`Self::switch`]).
    /// 2. Otherwise the theme file itself, by stamp. Unmoved means done.
    /// 3. A moved file has to load, and has to still set every token it
    ///    set before, or the colors on screen stay (see [`Self::adopt`]).
    ///
    /// Step 1 asks whether the FILE changed, not whether the config did.
    /// The two are not the same question, and answering the easy one cost
    /// the rule in step 3: `cyclops theme <name>` rewrites the key every
    /// time, so re-selecting on any config write turned every edit made
    /// while that command ran into an exempt switch. Caught by
    /// demos/m5-theme.sh, where a half-written file reached a real border.
    pub fn refresh(&mut self) -> bool {
        let config = stamp_of(&config_path(&self.home));
        if config != self.config {
            self.config = config;
            let choice = choose(self.env.as_deref(), &self.home);
            if choice.path != self.path {
                return self.switch(choice);
            }
            // The key still names the file already on screen. A complaint
            // about the KEY (wrong type, say) is worth saying once even
            // so; a complaint about the file is step 3's to make, and
            // `choose` has not read the file, so none can leak in here.
            if !choice.warnings.is_empty() {
                self.warnings = choice.warnings;
            }
        }
        let Some(path) = self.path.clone() else {
            return false;
        };
        let file = stamp_of(&path);
        if file == self.file {
            return false;
        }
        self.file = file;
        match Theme::load(&path) {
            Ok((theme, warnings)) => self.adopt(&path, theme, warnings),
            // A file that is not there cannot be fixed by saving it, so
            // the remedy names how a theme file comes back instead.
            Err(ThemeError::Read { path, source })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                self.warnings = vec![format!(
                    "{} does not exist. {KEPT} Run cyclops start to seed the \
                     shipped themes, or cyclops theme <name> to pick an \
                     existing one.",
                    path.display()
                )];
                false
            }
            Err(e) => {
                // Same next step as the lost-token refusal below, because
                // the reader's move is the same: the file is wrong, the
                // screen is not, and saving it again is what applies.
                self.warnings = vec![format!("{e}. {KEPT} Fix the file and save again.")];
                false
            }
        }
    }

    /// The config key now names a different file. A switch applies whole,
    /// with the tolerance a fresh start gives: the user asked for this
    /// palette, so a theme that sets only a few tokens takes the compiled
    /// defaults for the rest instead of being refused.
    fn switch(&mut self, choice: Choice) -> bool {
        let sel = load(choice, &self.home);
        self.file = sel.path.as_deref().and_then(stamp_of);
        self.path = sel.path;
        self.theme = sel.theme;
        self.warnings = sel.warnings;
        true
    }

    /// Take a reloaded palette, or refuse it and say why in one line.
    ///
    /// The refusal exists for one failure, and it is a common one: editors
    /// save by truncating and rewriting, so a stat that lands mid-save
    /// reads a SHORTER file that is still valid TOML. Loading it paints
    /// every token it lost out of the compiled default table, whose
    /// lightness has nothing to do with the theme on screen, so a light
    /// terminal gets a handful of dark-theme cells for as long as the save
    /// takes. A typo in a token name does the same thing and stays.
    ///
    /// Refusing costs a deliberate edit that drops a token nothing but a
    /// warning: the next save with the token gone is the same file again
    /// and applies, because the theme on screen no longer sets it either.
    fn adopt(&mut self, path: &Path, next: Theme, warnings: Vec<String>) -> bool {
        let lost: Vec<&str> = tokens::ALL
            .into_iter()
            .filter(|t| self.theme.defines(t) && !next.defines(t))
            .collect();
        let Some(first) = lost.first() else {
            self.theme = next;
            self.warnings = warnings;
            return true;
        };
        let more = match lost.len() - 1 {
            0 => String::new(),
            n => format!(" and {n} more"),
        };
        self.warnings = vec![format!(
            "{} stopped setting `{first}`{more}. {KEPT} Fix the file and save again.",
            path.display()
        )];
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_theme(dir: &Path, name: &str, body: &str) -> PathBuf {
        let themes = dir.join("themes");
        std::fs::create_dir_all(&themes).expect("mkdir themes");
        let path = themes.join(format!("{name}.toml"));
        std::fs::write(&path, body).expect("write theme");
        path
    }

    /// A theme file that sets the whole vocabulary, every token the same
    /// color. Reload rules are about which tokens a file SETS, so the
    /// values do not matter and one hex keeps the fixtures readable.
    fn whole(hex: &str) -> String {
        let mut out = String::from("name = \"dark\"\n");
        for group in [
            "role", "surface", "eye", "state", "badge", "chrome", "palette",
        ] {
            out.push_str(&format!("[{group}]\n"));
            for token in tokens::ALL {
                let (g, key) = token.split_once('.').expect("group.key");
                if g == group {
                    out.push_str(&format!("{key} = \"{hex}\"\n"));
                }
            }
        }
        out
    }

    /// Force a file's mtime forward so a rewrite of the same length cannot
    /// hide inside the filesystem's timestamp granularity.
    fn touch(path: &Path) {
        let f = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for touch");
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(2))
            .expect("bump mtime");
    }

    /// `set_config_theme`'s home-relative path, read back after a write.
    fn config_after(before: Option<&str>, name: &str) -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(b) = before {
            std::fs::write(dir.path().join("config.toml"), b).expect("seed config");
        }
        set_config_theme(dir.path(), name).expect("write theme key");
        std::fs::read_to_string(dir.path().join("config.toml")).expect("read config")
    }

    /// The switch edits one line. Comments, key order, and every other key
    /// survive it, because the file belongs to whoever wrote it.
    #[test]
    fn the_config_key_is_edited_not_rewritten() {
        let before = "# my config\nsessions = [\"main\"]\ntheme = \"dark\"\nchrome = \"off\"\n";
        assert_eq!(
            config_after(Some(before), "light"),
            "# my config\nsessions = [\"main\"]\ntheme = \"light\"\nchrome = \"off\"\n"
        );
        // No key yet: added after the top-level keys.
        assert_eq!(
            config_after(Some("sessions = [\"main\"]\n"), "light"),
            "sessions = [\"main\"]\ntheme = \"light\"\n"
        );
        // No file yet.
        assert_eq!(config_after(None, "light"), "theme = \"light\"\n");
    }

    /// A `theme` key has to be a key. A comment and a value that happens to
    /// contain the word are neither, and rewriting one would silently
    /// change something else or leave the real key alone.
    #[test]
    fn only_a_real_top_level_key_is_rewritten() {
        assert_eq!(
            config_after(
                Some("# theme = \"dark\"\ndefault_workspace = \"theme\"\n"),
                "light"
            ),
            "# theme = \"dark\"\ndefault_workspace = \"theme\"\ntheme = \"light\"\n"
        );
        // A key under a table is not the top-level key, so the new one
        // goes above the table where the daemon will read it.
        assert_eq!(
            config_after(
                Some("sessions = [\"main\"]\n[other]\ntheme = \"x\"\n"),
                "light"
            ),
            "sessions = [\"main\"]\ntheme = \"light\"\n[other]\ntheme = \"x\"\n"
        );
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

    /// The command that writes the config key and the selection that reads
    /// it must name the same file, or `cyclops theme` approves one theme
    /// and the renderers load another.
    #[test]
    fn path_for_is_the_one_resolver() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "");
        let dir = themes_dir(home.path()).expect("themes dir");
        assert_eq!(path_for("dark", home.path()), Some(dir.join("dark.toml")));
        assert_eq!(
            path_for("/tmp/x.toml", home.path()),
            Some(PathBuf::from("/tmp/x.toml"))
        );
        // The path a bare name resolves to is the path selection loads.
        std::fs::write(home.path().join("config.toml"), "theme = \"dark\"\n").expect("config");
        assert_eq!(
            active_with(None, home.path()).path,
            path_for("dark", home.path())
        );
    }

    #[test]
    fn explicit_missing_theme_warns_and_falls_back() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "");
        let sel = active_with(Some("ghost"), home.path());
        assert_eq!(sel.theme.name(), "built-in");
        assert_eq!(sel.warnings.len(), 1, "{:?}", sel.warnings);
        assert!(sel.warnings[0].contains("no theme called \"ghost\""));
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
        let path = write_theme(home.path(), "dark", "[surface\n");
        let sel = active_with(None, home.path());
        assert_eq!(sel.theme.name(), "built-in");
        assert_eq!(sel.warnings.len(), 1, "{:?}", sel.warnings);
        assert_eq!(
            sel.warnings[0],
            format!(
                "{} isn't valid TOML (line 1, column 9: invalid table header; \
                 expected `.`, `]`), so cyclops is using built-in colors until \
                 you fix it.",
                path.display()
            )
        );
    }

    /// Every warning is one line, whatever went wrong inside it.
    ///
    /// `toml::de::Error`'s Display is a five-line diagnostic with a caret
    /// diagram. Quoting it inside a sentence printed a block and left the
    /// tail of the sentence alone on the last line, which is not something
    /// a reader can act on. The position and what the parser wanted are.
    #[test]
    fn a_warning_is_one_line_and_never_stutters_the_word_theme() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "[surface\n");
        write_theme(home.path(), "typo", "[surfac]\ndim = \"#111111\"\n");
        write_theme(home.path(), "empty", "");
        std::fs::write(home.path().join("config.toml"), "theme = \"dark\"\n").expect("config");

        // Both halves of the selection, and both halves of a reload: every
        // path that composes a line for a human is here.
        let mut said: Vec<String> = active_with(None, home.path()).warnings;
        said.extend(active_with(Some("typo"), home.path()).warnings);
        said.extend(active_with(Some("ghost"), home.path()).warnings);
        let mut watch = ThemeWatch::with_env(Some("typo"), home.path());
        said.extend(watch.take_warnings());
        write_theme(home.path(), "typo", "[surface\n");
        assert!(!watch.refresh());
        said.extend(watch.take_warnings());
        std::fs::remove_file(home.path().join("themes/typo.toml")).expect("remove theme");
        assert!(!watch.refresh());
        said.extend(watch.take_warnings());

        assert!(said.len() >= 5, "{said:?}");
        for w in &said {
            assert_eq!(w.lines().count(), 1, "not one line: {w:?}");
            // Every printer prefixes "theme: ", so a message that names the
            // word again reads "theme: theme /path/...".
            assert!(!w.starts_with("theme "), "stutters: {w:?}");
        }
    }

    #[test]
    fn refresh_applies_edits_and_ignores_no_change() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = write_theme(home.path(), "dark", "[surface]\ndim = \"#111111\"\n");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (17, 17, 17));
        // Untouched file: no change.
        assert!(!watch.refresh());
        // A length-changing edit applies on the next refresh.
        std::fs::write(&path, "name = \"dark\"\n[surface]\ndim = \"#222222\"\n")
            .expect("rewrite theme");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (34, 34, 34));
        assert!(!watch.refresh());
        // A same-length edit is caught through the mtime half of the stamp.
        std::fs::write(&path, "name = \"dark\"\n[surface]\ndim = \"#333333\"\n")
            .expect("rewrite theme");
        touch(&path);
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (51, 51, 51));
        assert!(watch.take_warnings().is_empty());
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
        // but the file itself arrives later. Nothing is on screen yet that
        // a partial file could break, so the arrival applies.
        std::fs::create_dir_all(home.path().join("themes")).expect("mkdir themes");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert_eq!(watch.theme().name(), "built-in");
        assert!(!watch.refresh());
        write_theme(home.path(), "dark", "[surface]\ndim = \"#555555\"\n");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (85, 85, 85));
    }

    /// The `cyclops theme <name>` path: the file on disk never changes,
    /// only the key that says which file to read. A watch that stat'ed one
    /// path would sit on the old palette for the life of the process.
    #[test]
    fn the_config_key_switches_a_running_watch() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", "[surface]\ndim = \"#111111\"\n");
        write_theme(home.path(), "light", "[surface]\ndim = \"#eeeeee\"\n");
        let config = home.path().join("config.toml");
        std::fs::write(&config, "theme = \"dark\"\n").expect("write config");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (17, 17, 17));

        std::fs::write(&config, "theme = \"light\"\n").expect("switch theme");
        assert!(watch.refresh());
        assert_eq!(watch.theme().name(), "light");
        assert_eq!(
            watch.theme().resolve(tokens::SURFACE_DIM).rgb,
            (238, 238, 238)
        );
        assert!(watch.take_warnings().is_empty());
        assert!(!watch.refresh());

        // The new file is now the watched one: editing it applies, and
        // editing the one that was left behind does not.
        std::fs::write(
            home.path().join("themes/light.toml"),
            "[surface]\ndim = \"#dddddd\"\n",
        )
        .expect("edit light");
        assert!(watch.refresh());
        assert_eq!(
            watch.theme().resolve(tokens::SURFACE_DIM).rgb,
            (221, 221, 221)
        );
        std::fs::write(
            home.path().join("themes/dark.toml"),
            "[surface]\ndim = \"#000000\"\n",
        )
        .expect("edit dark");
        assert!(!watch.refresh());
        assert_eq!(
            watch.theme().resolve(tokens::SURFACE_DIM).rgb,
            (221, 221, 221)
        );
    }

    /// A switch the user asked for applies whole, even to a sparse theme
    /// that leaves most tokens on the compiled defaults. The token rule
    /// below is about an EDIT to the file already on screen, and holding a
    /// deliberate `cyclops theme mine` to it would refuse to switch.
    #[test]
    fn a_selection_change_may_set_fewer_tokens() {
        let home = tempfile::tempdir().expect("tempdir");
        write_theme(home.path(), "dark", &whole("#111111"));
        write_theme(home.path(), "mine", "[role]\n1 = \"#abcdef\"\n");
        let config = home.path().join("config.toml");
        std::fs::write(&config, "theme = \"dark\"\n").expect("write config");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert!(watch.theme().defines(tokens::STATE_DEAD));

        std::fs::write(&config, "theme = \"mine\"\n").expect("switch theme");
        assert!(watch.refresh());
        assert_eq!(watch.theme().name(), "mine");
        assert!(watch.take_warnings().is_empty());
        assert_eq!(watch.theme().resolve("role.1").rgb, (0xab, 0xcd, 0xef));
        // Everything it does not set comes off the compiled table, which
        // is the documented tolerance and not a reload failure.
        assert!(!watch.theme().defines(tokens::STATE_DEAD));
    }

    /// Malformed-reload recovery, all four shapes an edit goes wrong in.
    /// Every one keeps the colors already on screen and says so once.
    #[test]
    fn a_broken_edit_keeps_the_colors_and_warns_once() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = write_theme(home.path(), "dark", &whole("#111111"));
        let mut watch = ThemeWatch::with_env(None, home.path());
        let good = watch.theme().clone();
        assert!(watch.take_warnings().is_empty());

        // 1. Broken TOML: the whole file is unreadable.
        std::fs::write(&path, "[surface\n").expect("break theme");
        assert!(!watch.refresh());
        assert_eq!(watch.theme().overrides, good.overrides);
        let w = watch.take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("isn't valid TOML"), "{w:?}");
        assert!(w[0].contains(KEPT), "{w:?}");
        // A file that exists and does not parse is fixed by saving it.
        assert!(w[0].contains("Fix the file and save again"), "{w:?}");

        // 2. Truncated mid-save: valid TOML, most of the palette gone.
        //    This is the one that used to paint half a theme.
        std::fs::write(&path, "[surface]\ndim = \"#111111\"\n").expect("truncate theme");
        assert!(!watch.refresh());
        assert_eq!(watch.theme().overrides, good.overrides);
        let w = watch.take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("stopped setting"), "{w:?}");
        assert!(w[0].contains("more"), "{w:?}");

        // 3. One token misspelled: it loads, warns as unknown, and would
        //    have left that one cell on the compiled default.
        let typo = whole("#111111").replace("\ndim =", "\ndimm =");
        std::fs::write(&path, &typo).expect("typo theme");
        assert!(!watch.refresh());
        assert_eq!(watch.theme().overrides, good.overrides);
        let w = watch.take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`surface.dim`"), "{w:?}");
        assert!(!w[0].contains("more"), "{w:?}");

        // 4. Deleted outright. Saving a file that is not there fixes
        //    nothing, so the remedy names the commands that bring one back.
        std::fs::remove_file(&path).expect("remove theme");
        assert!(!watch.refresh());
        assert_eq!(watch.theme().overrides, good.overrides);
        let w = watch.take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains(KEPT), "{w:?}");
        assert!(w[0].contains("does not exist"), "{w:?}");
        assert!(w[0].contains("cyclops start"), "{w:?}");
        assert!(!w[0].contains("Fix the file"), "{w:?}");

        // A refusal is said once, not once per repaint.
        assert!(!watch.refresh());
        assert!(watch.take_warnings().is_empty());

        // And the file coming back good applies, with nothing left over.
        std::fs::write(&path, whole("#222222")).expect("restore theme");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (34, 34, 34));
        assert!(watch.take_warnings().is_empty());
    }

    /// An edit that keeps every token it had is a normal edit, whatever
    /// else it does: the rule is about tokens LOST, not about the file
    /// growing or shrinking.
    #[test]
    fn an_edit_that_keeps_its_tokens_applies() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = write_theme(home.path(), "dark", &whole("#111111"));
        let mut watch = ThemeWatch::with_env(None, home.path());
        // Same tokens, new values, and a shorter file (the values lost
        // their explicit fallbacks).
        let smaller = whole("#222222").replace("\n\n", "\n");
        std::fs::write(&path, &smaller).expect("rewrite theme");
        touch(&path);
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (34, 34, 34));
        assert!(watch.take_warnings().is_empty());
    }

    /// The regression demos/m5-theme.sh found, and the reason step 1 asks
    /// about the file and not about the config.
    ///
    /// `cyclops theme <name>` rewrites the key every time, even to the
    /// value it already had. A refresh that treated any config write as a
    /// switch made that command an exemption from the token rule, so a
    /// file caught mid-save while it ran reached the screen: on a real rig
    /// the pane borders went to the compiled default table.
    #[test]
    fn rewriting_the_key_with_the_same_value_is_not_a_switch() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = write_theme(home.path(), "light", &whole("#eeeeee"));
        let config = home.path().join("config.toml");
        std::fs::write(&config, "theme = \"light\"\n").expect("write config");
        let mut watch = ThemeWatch::with_env(None, home.path());
        let good = watch.theme().clone();

        // The shape a save leaves behind, and the command rewriting the
        // key it already holds, in either order.
        std::fs::write(&path, "").expect("truncate light");
        std::fs::write(&config, "theme = \"light\"\n").expect("rewrite the same key");
        touch(&config);
        assert!(!watch.refresh());
        assert_eq!(watch.theme().overrides, good.overrides);
        let w = watch.take_warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("stopped setting"), "{w:?}");
    }

    /// A config write while the theme file is already broken must not say
    /// "Using built-in colors", because it is not using them: the colors
    /// on screen were kept. That copy belongs to a fresh start, which has
    /// nothing to keep, and it reached the config path when one function
    /// resolved the choice and read the file together.
    #[test]
    fn a_config_write_never_reports_the_fresh_start_fallback() {
        let home = tempfile::tempdir().expect("tempdir");
        let path = write_theme(home.path(), "dark", &whole("#111111"));
        let config = home.path().join("config.toml");
        std::fs::write(&config, "theme = \"dark\"\n").expect("write config");
        let mut watch = ThemeWatch::with_env(None, home.path());
        let good = watch.theme().clone();

        std::fs::write(&path, "[surface\n").expect("break theme");
        assert!(!watch.refresh());
        assert_eq!(watch.take_warnings().len(), 1);

        // The file is still broken; now the config moves for a reason of
        // its own. Nothing new happened to the theme, so nothing is said.
        std::fs::write(&config, "theme = \"dark\"\nsessions = [\"main\"]\n").expect("edit config");
        assert!(!watch.refresh());
        assert_eq!(watch.theme().overrides, good.overrides);
        assert!(watch.take_warnings().is_empty());
    }

    /// A watch that starts on the built-in table has no palette to
    /// protect, so a file arriving with a token subset applies. The rule
    /// keeps what is ON SCREEN; the compiled table is the floor, not a
    /// theme someone chose.
    #[test]
    fn the_built_in_floor_is_not_a_palette_to_keep() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(home.path().join("themes")).expect("mkdir themes");
        let mut watch = ThemeWatch::with_env(None, home.path());
        assert_eq!(watch.theme().name(), "built-in");
        write_theme(home.path(), "dark", "[surface]\ndim = \"#555555\"\n");
        assert!(watch.refresh());
        assert_eq!(watch.theme().resolve(tokens::SURFACE_DIM).rgb, (85, 85, 85));
        assert!(watch.take_warnings().is_empty());
    }
}
