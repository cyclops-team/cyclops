//! Scratch paths for tests, demos, and probe rigs.
//!
//! Nothing at runtime uses this: the daemon and the CLI keep their state
//! under CYCLOPS_HOME. It lives here because every crate already depends on
//! cyclops-proto, so the rule below is written once.

use std::path::PathBuf;

/// Environment variable overriding the scratch root.
pub const SCRATCH_ENV: &str = "CYCLOPS_TEST_TMP";

/// Root directory for throwaway test state.
///
/// The root must be short. A unix socket path caps near 104 bytes on macOS,
/// and the system temp dir there is a long private/var/folders path, so a
/// daemon socket created under it fails to bind. `/private/tmp` is short and
/// always present on macOS.
///
/// It does not exist on Linux, and the parent is not writable, so hardcoding
/// it fails every Linux test that creates a directory (F24). There the system
/// temp dir is `/tmp`, already short.
///
/// `CYCLOPS_TEST_TMP` overrides both. Running the suite with the root
/// relocated is how this choice is proven free of a hardcoded path.
pub fn scratch_root() -> PathBuf {
    if let Ok(p) = std::env::var(SCRATCH_ENV) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if cfg!(target_os = "macos") {
        return PathBuf::from("/private/tmp");
    }
    std::env::temp_dir()
}

/// Scratch directory for one test, named `<root>/<tag>-<pid>`.
///
/// The pid keeps concurrent test binaries off each other's state.
pub fn scratch_dir(tag: &str) -> PathBuf {
    scratch_root().join(format!("{tag}-{}", std::process::id()))
}
