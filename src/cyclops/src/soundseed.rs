//! The sound files that ship with the binary, and getting them installed.
//!
//! Same shape as [`crate::themeseed`], smaller stakes still: a home
//! without sounds falls back to the terminal bell (the workspace's
//! `sound` module), so the seed is quiet on success and a problem is a
//! note. Simpler too: nobody edits a sound the way they edit a theme, so
//! a file already present is left alone whatever its bytes. Replacing a
//! shipped cue means shipping it under a new name.

use std::path::{Path, PathBuf};

use cyclops_state::{CreateFileOutcome, StateRoot};

/// Every sound the binary carries, by file name. Alphabetical, and
/// `the_shipped_list_is_the_sounds_directory` holds it to every file in
/// resources/sounds/ so a new sound cannot be embedded halfway.
const SHIPPED: &[(&str, &[u8])] = &[
    (
        "bow-ripple.wav",
        include_bytes!("../../../resources/sounds/bow-ripple.wav"),
    ),
    (
        "glass-ping.wav",
        include_bytes!("../../../resources/sounds/glass-ping.wav"),
    ),
];

/// Where sounds go: the directory the workspace's `sound::installed_sound`
/// reads.
pub fn dir(home: &Path) -> PathBuf {
    home.join("sounds")
}

/// What one [`seed`] run did.
pub struct Seeded {
    pub dir: PathBuf,
    /// Shipped files written this run.
    pub written: Vec<String>,
    /// Why a file could not be written, one sentence each.
    pub problems: Vec<String>,
}

/// Put the shipped sounds in `<home>/sounds`, never touching a file that
/// is already there. Runs on every `cyclops start` and every bare
/// `cyclops`, so a home that predates the seed gets the set on its next
/// run without a reinstall.
pub fn seed(home: &Path) -> Seeded {
    let dir = dir(home);
    let mut out = Seeded {
        dir: dir.clone(),
        written: Vec::new(),
        problems: Vec::new(),
    };
    let root = match StateRoot::open_or_create(home) {
        Ok(root) => root,
        Err(e) => {
            out.problems.push(format!("create {}: {e}", dir.display()));
            return out;
        }
    };
    for (name, body) in SHIPPED {
        let path = dir.join(name);
        let descendant = Path::new("sounds").join(name);
        match root.create_file_once(&descendant, body) {
            Ok(CreateFileOutcome::Created) => out.written.push((*name).to_string()),
            Ok(CreateFileOutcome::AlreadyExists) => {}
            Err(e) => out.problems.push(format!("write {}: {e}", path.display())),
        }
    }
    out
}

/// The line `cyclops start` prints when it installed sounds. Only when it
/// wrote something: a note repeated on every start is noise.
pub fn installed(seeded: &Seeded) -> String {
    let n = seeded.written.len();
    let thing = if n == 1 { "sound" } else { "sounds" };
    format!("wrote {n} {thing} to {}", seeded.dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn scratch(tag: &str) -> PathBuf {
        let d = cyclops_proto::scratch::scratch_dir(tag);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    #[test]
    fn seeds_every_shipped_sound_once_and_never_rewrites() {
        let home = scratch("cyc-soundseed");
        let first = seed(&home);
        assert_eq!(first.written.len(), SHIPPED.len(), "{:?}", first.problems);
        for (name, body) in SHIPPED {
            let path = home.join("sounds").join(name);
            assert_eq!(std::fs::read(&path).unwrap(), *body, "{name} byte-exact");
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let cue = home.join("sounds/bow-ripple.wav");
        assert_eq!(
            installed(&first),
            format!("wrote 2 sounds to {}", dir(&home).display())
        );

        // Whatever is there stays, byte for byte, on every later run.
        std::fs::write(&cue, b"mine").unwrap();
        let again = seed(&home);
        assert!(again.written.is_empty(), "{:?}", again.written);
        assert!(again.problems.is_empty(), "{:?}", again.problems);
        assert_eq!(std::fs::read(&cue).unwrap(), b"mine");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// resources/sounds/ is the source of truth for what ships; a file
    /// added there but not here would never reach a home.
    #[test]
    fn the_shipped_list_is_the_sounds_directory() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/sounds");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("resources/sounds")
            .map(|e| {
                e.expect("sound dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|n| !n.starts_with('.'))
            .collect();
        on_disk.sort();
        let listed: Vec<String> = SHIPPED.iter().map(|(n, _)| (*n).to_string()).collect();
        assert_eq!(
            listed, on_disk,
            "SHIPPED is not resources/sounds/ in alphabetical order; add the \
             missing file to SHIPPED in src/cyclops/src/soundseed.rs"
        );
    }
}
