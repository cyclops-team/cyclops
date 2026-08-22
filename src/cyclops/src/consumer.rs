//! Canonical config roots for supported agent consumers.

use std::path::{Path, PathBuf};

use crate::hookset::CliKind;

/// Resolve the config root that proves a consumer is installed.
///
/// Codex alone supports an environment override. The override changes its
/// hook config root, not the shared Cyclops skill destination.
pub(crate) fn root(kind: CliKind, user_home: &Path) -> PathBuf {
    match kind {
        CliKind::Claude => user_home.join(".claude"),
        CliKind::Codex => std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home.join(".codex")),
        CliKind::Cursor => user_home.join(".cursor"),
        CliKind::Agy => user_home.join(".gemini/antigravity-cli"),
    }
}
