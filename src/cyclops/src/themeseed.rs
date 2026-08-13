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

use crate::hash::fnv64;

/// Every theme the binary carries, by file name. Alphabetical, and
/// `the_shipped_list_is_the_themes_directory` holds it to every file in
/// resources/themes/ so a new theme cannot be embedded halfway.
const SHIPPED: &[(&str, &str)] = &[
    (
        "blossom.toml",
        include_str!("../../../resources/themes/blossom.toml"),
    ),
    (
        "buttercream.toml",
        include_str!("../../../resources/themes/buttercream.toml"),
    ),
    (
        "catppuccin.toml",
        include_str!("../../../resources/themes/catppuccin.toml"),
    ),
    (
        "dark.toml",
        include_str!("../../../resources/themes/dark.toml"),
    ),
    (
        "ember.toml",
        include_str!("../../../resources/themes/ember.toml"),
    ),
    (
        "forest.toml",
        include_str!("../../../resources/themes/forest.toml"),
    ),
    (
        "gruvbox.toml",
        include_str!("../../../resources/themes/gruvbox.toml"),
    ),
    (
        "high-contrast.toml",
        include_str!("../../../resources/themes/high-contrast.toml"),
    ),
    (
        "light.toml",
        include_str!("../../../resources/themes/light.toml"),
    ),
    (
        "meadow.toml",
        include_str!("../../../resources/themes/meadow.toml"),
    ),
    (
        "midnight.toml",
        include_str!("../../../resources/themes/midnight.toml"),
    ),
    (
        "nord.toml",
        include_str!("../../../resources/themes/nord.toml"),
    ),
    (
        "obsidian.toml",
        include_str!("../../../resources/themes/obsidian.toml"),
    ),
    (
        "periwinkle.toml",
        include_str!("../../../resources/themes/periwinkle.toml"),
    ),
    (
        "seafoam.toml",
        include_str!("../../../resources/themes/seafoam.toml"),
    ),
    (
        "sorbet.toml",
        include_str!("../../../resources/themes/sorbet.toml"),
    ),
    (
        "tokyo-night.toml",
        include_str!("../../../resources/themes/tokyo-night.toml"),
    ),
];

/// Where themes go: the first directory `cyclops_theme::themes_dir` looks.
pub fn dir(home: &Path) -> PathBuf {
    home.join("themes")
}

/// FNV-1a 64 of every theme file body this project has ever shipped, the
/// current ones included. A file on disk whose hash is in this list is a
/// seed the user never edited, so a newer shipped version may replace it;
/// any other content is the user's and is never touched. Measured cost of
/// not having this: a light.toml seeded before `surface.bg` existed kept
/// resolving a dark ground under a current binary, on two real machines,
/// through every reinstall.
///
/// Regenerate when a shipped theme changes: hash every historical blob of
/// resources/themes/ (and the old themes/ path) across all refs; the test
/// below fails until the current bodies are listed.
const EVER_SHIPPED_FNV64: &[&str] = &[
    "079d603c4110e97a",
    "0b4e9e0c1faf543b",
    "103a71725e5f7e0e",
    "16659b363c97515a",
    "1c6dd03958d24189",
    "2805d33948df7087",
    "302ae5a539549eb3",
    "3043a40966256b08",
    "31d3f87d51f24bdb",
    "3d0aad5d9b63f1d3",
    "4148e7e8ffaff233",
    "438eef9420321e42",
    "43e559f60ee17dfd",
    "45df4664bb804c5d",
    "4bd5ba7fc0eb9cfc",
    "52c5205d7f6637f0",
    "4e04410bb72e4c83",
    "590416905c96bb96",
    "5ffdc0c45a777472",
    "6040fe6db2bbabbc",
    "608d06f121bee9bf",
    "65f6c922b4cfa360",
    "7b1f9cf910687f09",
    "7cd90c5fd2fabde5",
    "847e29dfa0cbd4dc",
    "85bf980022c0549d",
    "88387fdff1488ef2",
    "94301aec753053fb",
    "95223edd9843dcf8",
    "9ef86f48ff5d4311",
    "a96073b373240177",
    "aaa456eea2091dc9",
    "ac1368318fbcdf8e",
    "af572f0c4cb4ced1",
    "bfc17f3f626168c0",
    "c2f4f150f84798d0",
    "c776b954bfa78c16",
    "c915108a16b358cd",
    "cc7d478d863ef7dc",
    "d71f99395f3a548b",
    "dc469fc78811f02b",
    "df7a68ea06240a04",
    "dfa5b169d8b1a128",
    "e070908e2221ea5b",
    "e26522f98f021dd7",
    "e2960fc8ce169c7f",
    "e5531f9b02b29c11",
    "ec8713a84becce80",
];

/// What one [`seed`] run did.
pub struct Seeded {
    pub dir: PathBuf,
    /// Shipped files written this run, fresh seeds and upgrades alike.
    pub written: Vec<String>,
    /// Why a file could not be written, one sentence each.
    pub problems: Vec<String>,
}

/// Put the shipped themes in `<home>/themes`, keeping every file the
/// user edited.
///
/// Runs on every `cyclops start`, not only the first: an upgrade that
/// adds a theme lands on the next start, and a home that predates this
/// seed gets the set without a reinstall. A file already present is
/// rewritten only when its content is byte-identical to a version this
/// project shipped ([`EVER_SHIPPED_FNV64`]): that file is a seed nobody
/// touched, and leaving it stale is how a pre-`surface.bg` light.toml
/// kept a dark ground under a current binary. Anything else on disk is
/// the user's and survives every run.
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
        if let Ok(existing) = std::fs::read(&path) {
            let unedited = EVER_SHIPPED_FNV64.contains(&fnv64(&existing).as_str());
            if existing == body.as_bytes() || !unedited {
                continue;
            }
        } else if path.exists() {
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

    /// The stale-seed trap, closed: a file identical to a version this
    /// project once shipped is a seed nobody edited, and a newer shipped
    /// body replaces it on the next run. The pre-ground light.toml kept
    /// a dark ground under a current binary on two real machines.
    #[test]
    fn an_unedited_old_seed_upgrades_and_an_edited_one_never_does() {
        let home = scratch("cyc-themeup");
        seed(&home);
        // Stand in for an old shipped version: any body whose hash is in
        // the ever-shipped list but is not the current one. The v2-era
        // light.toml is blob-exact in git history; reproduce its effect
        // with a truncated current body registered nowhere, then prove
        // the negative first: unknown content stays.
        let light = home.join("themes/light.toml");
        std::fs::write(&light, "name = \"light\"\n").unwrap();
        let kept = seed(&home);
        assert!(kept.written.is_empty(), "{:?}", kept.written);

        // Now the positive: plant a body that hashes into the shipped
        // list (the current dark.toml body under light's name is exactly
        // that: shipped bytes, wrong file, user never typed them).
        let dark_body = SHIPPED
            .iter()
            .find(|(n, _)| *n == "dark.toml")
            .map(|(_, b)| *b)
            .unwrap();
        std::fs::write(&light, dark_body).unwrap();
        let upgraded = seed(&home);
        assert_eq!(upgraded.written, vec!["light.toml".to_string()]);
        let now = std::fs::read_to_string(&light).unwrap();
        assert!(now.contains("[palette]"), "upgraded to the current body");
        assert_ne!(now, dark_body, "and it is light.toml's body, not dark's");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every current shipped body must be in the ever-shipped hash list,
    /// which is what keeps the list honest: whoever changes a theme sees
    /// this fail and appends the new hash, so the version they replaced
    /// stays recognizable as an unedited seed forever after.
    #[test]
    fn the_ever_shipped_list_contains_the_current_bodies() {
        for (name, body) in SHIPPED {
            assert!(
                EVER_SHIPPED_FNV64.contains(&fnv64(body.as_bytes()).as_str()),
                "{name}'s current body is not in EVER_SHIPPED_FNV64; add {}",
                fnv64(body.as_bytes())
            );
        }
    }

    /// resources/themes/ is the source of truth for what ships, and this
    /// list is one of two that have to match it. The other is `SHIPPED`
    /// in `src/cyclops-theme/tests/shipped.rs`, held to the same
    /// directory by its own test; anchoring both to the directory is
    /// what keeps two lists in two crates from drifting apart.
    ///
    /// The failure this closes: a theme added here but forgotten there
    /// seeds onto every machine without one structural gate ever loading
    /// it, so nothing checks its token coverage, its role fallbacks or
    /// its palette until a user sees the colors.
    #[test]
    fn the_shipped_list_is_the_themes_directory() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/themes");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("resources/themes")
            .map(|e| {
                e.expect("theme dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|n| n.ends_with(".toml"))
            .collect();
        on_disk.sort();

        let listed: Vec<String> = SHIPPED.iter().map(|(n, _)| (*n).to_string()).collect();
        for name in &on_disk {
            assert!(
                listed.contains(name),
                "resources/themes/{name} exists but nothing seeds it; add it to \
                 SHIPPED in src/cyclops/src/themeseed.rs"
            );
        }
        // A SHIPPED entry naming a file that is not there cannot compile,
        // so what is left to check is order and duplicates.
        assert_eq!(
            listed, on_disk,
            "SHIPPED is not resources/themes/ in alphabetical order"
        );
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
