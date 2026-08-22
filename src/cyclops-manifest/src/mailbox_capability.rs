//! Exact installed-skill evidence for the mailbox doorbell transport.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Skill bytes that define the claim-capable mailbox contract.
pub const SHIPPED_SKILL: &[u8] = include_bytes!("../../../skills/cyclops/SKILL.md");

/// Expand a manifest capability path against the user's home directory.
pub fn resolve_path(declared: &Path, user_home: &Path) -> Option<PathBuf> {
    if declared.is_absolute() {
        return Some(declared.to_path_buf());
    }
    let rest = declared.strip_prefix("~").ok()?;
    Some(user_home.join(rest))
}

/// Digest the opened regular file without following a final symlink.
pub fn file_digest(path: &Path) -> Option<[u8; 32]> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() != SHIPPED_SKILL.len() as u64 {
        return None;
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            return Some(digest.finalize().into());
        }
        digest.update(&buffer[..read]);
    }
}

/// Digest of the claim-capable skill compiled into this release.
pub fn shipped_digest() -> [u8; 32] {
    Sha256::digest(SHIPPED_SKILL).into()
}

/// Whether a path currently proves the exact claim-capable skill contract.
pub fn is_current(path: &Path) -> bool {
    file_digest(path) == Some(shipped_digest())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_skill_requires_exact_regular_file_bytes() {
        let root = cyclops_proto::scratch::scratch_dir("manifest-mailbox-capability");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("SKILL.md");
        std::fs::write(&path, SHIPPED_SKILL).unwrap();
        assert!(is_current(&path));

        std::fs::write(&path, b"operator edit").unwrap();
        assert!(!is_current(&path));
        assert!(!is_current(&root));
        assert!(!is_current(&root.join("missing")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let target = root.join("target-skill.md");
            let link = root.join("linked-skill.md");
            std::fs::write(&target, SHIPPED_SKILL).unwrap();
            symlink(&target, &link).unwrap();
            assert!(!is_current(&link));
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tilde_paths_resolve_only_against_the_supplied_home() {
        let home = Path::new("/users/test");
        assert_eq!(
            resolve_path(Path::new("~/.agents/skills/cyclops/SKILL.md"), home),
            Some(home.join(".agents/skills/cyclops/SKILL.md"))
        );
        assert_eq!(
            resolve_path(Path::new("/opt/cyclops/SKILL.md"), home),
            Some(PathBuf::from("/opt/cyclops/SKILL.md"))
        );
        assert_eq!(resolve_path(Path::new("relative/SKILL.md"), home), None);
    }
}
