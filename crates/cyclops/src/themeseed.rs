//! The theme files that ship with the binary, and getting them installed.
//!
//! Same shape as [`crate::manifests`], same reason, one difference in
//! stakes: a home without manifests delivers nothing, a home without
//! themes just renders in the engine's built-in colors. So the seed is
//! quiet on success and nothing checks `none_installed` here.
//!
//! Until this existed the themes were only in the repo, and `cyclops
//! theme` on an installed machine listed nothing: the docs told people to
//! copy the files in by hand, which is exactly the friction the installer
//! exists to remove.

use std::path::{Path, PathBuf};

/// Every theme the binary carries, by file name.
const SHIPPED: &[(&str, &str)] = &[
    (
        "catppuccin.toml",
        include_str!("../../../themes/catppuccin.toml"),
    ),
    ("dark.toml", include_str!("../../../themes/dark.toml")),
    ("gruvbox.toml", include_str!("../../../themes/gruvbox.toml")),
    (
        "high-contrast.toml",
        include_str!("../../../themes/high-contrast.toml"),
    ),
    ("light.toml", include_str!("../../../themes/light.toml")),
    ("nord.toml", include_str!("../../../themes/nord.toml")),
    (
        "tokyo-night.toml",
        include_str!("../../../themes/tokyo-night.toml"),
    ),
];

/// Where themes go: the first directory `cyclops_theme::themes_dir` looks.
pub fn dir(home: &Path) -> PathBuf {
    home.join("themes")
}

/// What one [`seed`] run did.
pub struct Seeded {
    pub dir: PathBuf,
    /// Shipped files written this run.
    pub written: Vec<String>,
    /// Why a file could not be written, one sentence each.
    pub problems: Vec<String>,
}

/// Put the shipped themes in `<home>/themes`, keeping every file that is
/// already there.
///
/// Runs on every `cyclops start`, not only the first: an upgrade that
/// adds a theme lands on the next start, and a home that predates this
/// seed gets the set without a reinstall. Files already present are never
/// read, compared, or rewritten, so an edited copy survives every run.
pub fn seed(home: &Path) -> Seeded {
    let dir = dir(home);
    let mut out = Seeded {
        dir: dir.clone(),
        written: Vec::new(),
        problems: Vec::new(),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        out.problems.push(format!("create {}: {e}", dir.display()));
        return out;
    }
    for (name, body) in SHIPPED {
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        match std::fs::write(&path, body) {
            Ok(()) => out.written.push((*name).to_string()),
            Err(e) => out.problems.push(format!("write {}: {e}", path.display())),
        }
    }
    out
}

/// The line `cyclops start` prints when it installed themes. Only when it
/// actually wrote something, for the same reason the manifest seed only
/// speaks then: a note repeated on every start is noise.
pub fn installed(seeded: &Seeded) -> String {
    let n = seeded.written.len();
    let thing = if n == 1 { "theme" } else { "themes" };
    format!("wrote {n} {thing} to {}", seeded.dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    #[test]
    fn seeds_every_shipped_theme_once_and_keeps_edits() {
        let home = scratch("cyc-themeseed");
        let first = seed(&home);
        assert_eq!(first.written.len(), SHIPPED.len(), "{:?}", first.problems);
        assert!(home.join("themes/catppuccin.toml").is_file());

        // An edited copy survives every later run, byte for byte.
        std::fs::write(home.join("themes/nord.toml"), "name = \"mine\"\n").unwrap();
        let again = seed(&home);
        assert!(again.written.is_empty(), "{:?}", again.written);
        assert_eq!(
            std::fs::read_to_string(home.join("themes/nord.toml")).unwrap(),
            "name = \"mine\"\n"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every shipped theme loads in the real engine with ZERO warnings.
    /// The engine parses tolerantly and paints unknown tokens from the
    /// built-in table, so a misspelled token would ship as a theme that
    /// half-works and looks fine in the diff; the warning is the only
    /// place the typo shows, so the warning is what fails the build.
    #[test]
    fn every_shipped_theme_loads_clean_in_the_engine() {
        let home = scratch("cyc-themeload");
        seed(&home);
        for (name, _) in SHIPPED {
            let path = dir(&home).join(name);
            let (_, warnings) = cyclops_theme::Theme::load(&path)
                .unwrap_or_else(|e| panic!("{name} does not load: {e}"));
            assert!(warnings.is_empty(), "{name} warns: {warnings:?}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
