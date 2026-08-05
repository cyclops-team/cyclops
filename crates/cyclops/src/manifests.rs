//! The detection manifests that ship with the binary, and getting them
//! into the cyclops home.
//!
//! A manifest is everything cyclops knows about one agent CLI: which
//! processes it runs as, how to read busy from idle off the pane, how to
//! type into it (docs/MANIFESTS.md). Without one, a pane reads `? unknown`
//! and a delivery to it ends in attention_required. So a machine with no
//! manifests has a cyclops that starts, watches, reports, and cannot
//! deliver a single message.
//!
//! They live in the repo at `manifests/` and are compiled in here, the same
//! way `workspace.rs` compiles in the layout presets and for the same
//! reason: an install is two binaries, and it has to work before it has
//! files. `cyclops start` writes them into `$CYCLOPS_HOME/manifests`, which
//! is where cyclopsd looks when `manifest_dir` is unset.
//!
//! Writing never overwrites. These files are meant to be edited, a rule
//! that has already been measured against a real CLI is worth more than the
//! shipped guess, and a seed that clobbered would throw that away on every
//! run. The cost is that a shipped file whose contents change does not
//! reach a home that already has that name; the fix is deleting your copy
//! and running `cyclops start` again, which is documented where it is felt
//! (docs/install.md).

use std::path::{Path, PathBuf};

/// Every manifest the binary carries, by file name.
const SHIPPED: &[(&str, &str)] = &[
    ("agy.toml", include_str!("../../../manifests/agy.toml")),
    (
        "claude.toml",
        include_str!("../../../manifests/claude.toml"),
    ),
    ("codex.toml", include_str!("../../../manifests/codex.toml")),
    (
        "cursor.toml",
        include_str!("../../../manifests/cursor.toml"),
    ),
];

/// Where manifests go, and where cyclopsd looks with no `manifest_dir` set.
pub fn dir(home: &Path) -> PathBuf {
    home.join("manifests")
}

/// What one [`seed`] run did.
pub struct Seeded {
    pub dir: PathBuf,
    /// Shipped files written this run.
    pub written: Vec<String>,
    /// Shipped files already there, left exactly as they were.
    pub kept: Vec<String>,
    /// Why a file could not be written, one sentence each.
    pub problems: Vec<String>,
}

impl Seeded {
    /// True when the directory holds none of the shipped manifests and
    /// nothing could be written, which is the broken install: cyclopsd will
    /// bind no pane and deliver nothing.
    pub fn none_installed(&self) -> bool {
        self.written.is_empty() && self.kept.is_empty()
    }
}

/// Put the shipped manifests in `<home>/manifests`, keeping every file that
/// is already there.
///
/// Runs on every `cyclops start`, not only the first: an upgrade that adds
/// a manifest lands on the next start, and a home that predates this seed
/// gets one without a reinstall. Files already present are never read,
/// compared, or rewritten, so an edited copy survives every run.
pub fn seed(home: &Path) -> Seeded {
    let dir = dir(home);
    let mut out = Seeded {
        dir: dir.clone(),
        written: Vec::new(),
        kept: Vec::new(),
        problems: Vec::new(),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        out.problems.push(format!("create {}: {e}", dir.display()));
        return out;
    }
    for (name, body) in SHIPPED {
        let path = dir.join(name);
        if path.exists() {
            out.kept.push((*name).to_string());
            continue;
        }
        match std::fs::write(&path, body) {
            Ok(()) => out.written.push((*name).to_string()),
            Err(e) => out.problems.push(format!("write {}: {e}", path.display())),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

/// The line `cyclops start` prints when it installed manifests.
///
/// Only when it actually wrote something. A run that found all of them
/// already there says nothing: a note repeated on every start is noise, and
/// the reader is looking for what changed.
pub fn installed(seeded: &Seeded) -> String {
    let n = seeded.written.len();
    let thing = if n == 1 { "manifest" } else { "manifests" };
    format!("wrote {n} detection {thing} to {}", seeded.dir.display())
}

/// The install is broken and nothing that follows can work.
///
/// Said in `cyclops start`'s own output, because every other line it prints
/// reports success and this is the one that says the success is empty.
pub fn nothing_installed(seeded: &Seeded) -> String {
    let why = match seeded.problems.first() {
        Some(p) => format!(": {p}"),
        None => String::new(),
    };
    format!(
        "no detection manifests in {}{why}. Cyclops can't tell what is running in a pane without them, so every pane reads unknown and no message can be delivered. Check you can write that directory, then run cyclops start again.",
        seeded.dir.display()
    )
}

/// Some manifests landed and some did not. Cyclops works for the CLIs that
/// got through, so this is a note beside a real ready line and not the
/// refusal above.
pub fn partly_installed(seeded: &Seeded) -> String {
    let cli = if seeded.problems.len() == 1 {
        "That agent CLI"
    } else {
        "Those agent CLIs"
    };
    format!(
        "{}. {cli} won't be detected; the manifests that landed are in {}.",
        seeded.problems.join("; "),
        seeded.dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled-in copies have to be the files in the repo, and they
    /// have to be manifests cyclops can actually load. A binary shipping a
    /// manifest that does not parse would seed a home where every pane
    /// reads unknown, which is the failure this whole module exists to
    /// prevent.
    #[test]
    fn the_shipped_set_is_the_repo_and_it_parses() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../manifests");
        let on_disk: Vec<String> = std::fs::read_dir(&repo)
            .expect("the repo manifests directory")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".toml"))
            .collect();
        assert_eq!(
            on_disk.len(),
            SHIPPED.len(),
            "manifests/ holds {on_disk:?}, the binary carries {:?}",
            SHIPPED.iter().map(|(n, _)| *n).collect::<Vec<_>>()
        );
        for (name, body) in SHIPPED {
            let text = std::fs::read_to_string(repo.join(name)).expect("shipped file exists");
            assert_eq!(&text, body, "{name} in the binary is not {name} on disk");
            cyclops_manifest::Manifest::parse(body, Path::new(name))
                .unwrap_or_else(|e| panic!("shipped {name} does not parse: {e}"));
        }
    }

    /// The rule the whole module turns on: a seed installs what is missing
    /// and touches nothing else. An edited manifest is a measurement
    /// somebody took against a real CLI, and the shipped file is a guess
    /// next to it.
    #[test]
    fn a_second_seed_keeps_the_edits_the_first_one_left_behind() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-seed-keep");
        let _ = std::fs::remove_dir_all(&home);

        let first = seed(&home);
        assert_eq!(first.written.len(), SHIPPED.len(), "{:?}", first.problems);
        assert!(first.kept.is_empty());
        assert!(!first.none_installed());

        // An edit, and a manifest of the reader's own.
        let edited = dir(&home).join("claude.toml");
        let mine = dir(&home).join("mycli.toml");
        std::fs::write(&edited, "# measured on 2.1.220\n").expect("edit");
        std::fs::write(&mine, "# mine\n").expect("write mine");

        let second = seed(&home);
        assert!(second.written.is_empty(), "{:?}", second.written);
        assert_eq!(second.kept.len(), SHIPPED.len());
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "# measured on 2.1.220\n",
            "a later start overwrote an edited manifest"
        );
        assert!(mine.exists(), "a later start removed a manifest of my own");

        // A shipped manifest deleted by hand comes back, which is how a set
        // that gains a file reaches a home that already exists.
        std::fs::remove_file(dir(&home).join("agy.toml")).expect("remove");
        let third = seed(&home);
        assert_eq!(third.written, vec!["agy.toml"]);
        assert_eq!(
            installed(&third),
            format!("wrote 1 detection manifest to {}", dir(&home).display())
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The middle case, which the seed reaches when one file fails and
    /// others do not. Cyclops still works for the CLIs that landed, so it
    /// is a note beside a real ready line, and it has to name what was
    /// lost rather than claim the install is dead.
    #[test]
    fn a_partial_install_names_what_did_not_land() {
        let dir = PathBuf::from("/h/manifests");
        let one = Seeded {
            dir: dir.clone(),
            written: vec!["agy.toml".into()],
            kept: Vec::new(),
            problems: vec!["write /h/manifests/claude.toml: Permission denied".into()],
        };
        assert!(!one.none_installed());
        assert_eq!(
            partly_installed(&one),
            "write /h/manifests/claude.toml: Permission denied. That agent CLI won't be detected; the manifests that landed are in /h/manifests."
        );
        let two = Seeded {
            problems: vec!["write a: no".into(), "write b: no".into()],
            ..one
        };
        assert!(partly_installed(&two).contains("Those agent CLIs won't be detected"));
    }

    /// A directory that cannot be written is the broken install, and the
    /// line says which directory, why, and what to do.
    #[test]
    fn a_seed_that_writes_nothing_says_the_install_cannot_work() {
        let root = cyclops_proto::scratch::scratch_dir("cyc-seed-blocked");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        // A file where the manifests directory has to go: create_dir_all
        // fails, and nothing can be written under it.
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::write(dir(&home), "not a directory").expect("occupy the path");

        let seeded = seed(&home);
        assert!(seeded.none_installed());
        let words = nothing_installed(&seeded);
        assert!(words.contains("no detection manifests in"), "{words}");
        assert!(words.contains("no message can be delivered"), "{words}");
        assert!(words.contains("cyclops start again"), "{words}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
