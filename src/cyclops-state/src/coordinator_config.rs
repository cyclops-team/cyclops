//! The shared safety boundary for coordinator settings in `config.toml`.
//!
//! These settings select the tmux server before any Cyclops process can safely
//! use its default. Every reader therefore comes through this module: it opens
//! the state root without following links, reads the config through a held
//! descriptor, and refuses malformed or ambiguous coordinator values instead
//! of silently choosing the ambient tmux server.

use std::path::{Path, PathBuf};

use crate::{StateError, StateRoot};

/// The one shared Cyclops configuration file.
pub const COORDINATOR_CONFIG_FILE: &str = "config.toml";

/// Configuration is small data, not a journal. Bound one read before parsing.
const COORDINATOR_CONFIG_MAX_BYTES: usize = 1024 * 1024;

/// The settings that choose what the coordinator will watch or control.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct CoordinatorConfig {
    /// tmux sessions the daemon watches, in the operator's declared order.
    pub sessions: Vec<String>,
    /// Optional tmux socket name. `None` deliberately means the normal server
    /// only after this configuration boundary has accepted the file.
    pub tmux_socket: Option<String>,
    /// Optional tmux client configuration file.
    pub tmux_config: Option<PathBuf>,
}

/// A safely read configuration document. Other product owners may interpret
/// their own keys from `table`; this module owns only [`CoordinatorConfig`].
#[derive(Debug, Clone)]
pub struct CoordinatorConfigDocument {
    pub coordinator: CoordinatorConfig,
    pub table: toml::Table,
    pub exists: bool,
}

impl CoordinatorConfigDocument {
    fn missing() -> Self {
        Self {
            coordinator: CoordinatorConfig::default(),
            table: toml::Table::new(),
            exists: false,
        }
    }
}

/// Why a coordinator configuration could not safely choose a tmux server.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorConfigError {
    #[error("could not safely read coordinator config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: StateError,
    },
    #[error("coordinator config {path} is not owner-readable")]
    Unreadable { path: PathBuf },
    #[error("coordinator config {path} exceeds the {COORDINATOR_CONFIG_MAX_BYTES}-byte limit")]
    TooLarge { path: PathBuf },
    #[error("coordinator config {path} is not valid UTF-8")]
    InvalidUtf8 { path: PathBuf },
    #[error("coordinator config {path} is malformed: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("coordinator config {path}: `{key}` must be {expected}, not {actual}")]
    WrongType {
        path: PathBuf,
        key: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
}

/// Safely load the coordinator settings. A missing state root or config file
/// is a valid empty configuration; every present file either parses fully or
/// refuses before a caller can issue a tmux command.
pub fn load_coordinator_config(
    home: &Path,
) -> Result<CoordinatorConfigDocument, CoordinatorConfigError> {
    let path = home.join(COORDINATOR_CONFIG_FILE);
    let Some(root) =
        StateRoot::open_existing(home).map_err(|source| read_error(path.clone(), source))?
    else {
        return Ok(CoordinatorConfigDocument::missing());
    };
    let inspector = root
        .inspector()
        .map_err(|source| read_error(path.clone(), source))?;
    let Some(file) = inspector
        .read_file(
            Path::new(COORDINATOR_CONFIG_FILE),
            COORDINATOR_CONFIG_MAX_BYTES,
        )
        .map_err(|source| read_error(path.clone(), source))?
    else {
        return Ok(CoordinatorConfigDocument::missing());
    };

    // StateRoot owns a private parent. A mode such as 0644 is still safe
    // beneath it, but links, foreign ownership, hard links, and unexpected
    // file types are never configuration input.
    if !file.entry.safe_beneath_owner_only_parent() {
        return Err(CoordinatorConfigError::Read {
            path,
            source: StateError::UnsafePath {
                path: file.entry.path,
                reason: file
                    .entry
                    .unsafe_reason
                    .unwrap_or("coordinator config is unsafe"),
            },
        });
    }
    if file.entry.mode & 0o400 == 0 {
        return Err(CoordinatorConfigError::Unreadable { path });
    }
    if file.truncated {
        return Err(CoordinatorConfigError::TooLarge { path });
    }
    let text = std::str::from_utf8(&file.bytes)
        .map_err(|_| CoordinatorConfigError::InvalidUtf8 { path: path.clone() })?;
    parse_coordinator_document(text, &path)
}

fn read_error(path: PathBuf, source: StateError) -> CoordinatorConfigError {
    if matches!(
        &source,
        StateError::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied
    ) {
        CoordinatorConfigError::Unreadable { path }
    } else {
        CoordinatorConfigError::Read { path, source }
    }
}

/// Parse one already-safely-read configuration document.
///
/// This is public for the daemon's in-memory parser so its coordinator
/// semantics stay identical to the descriptor-backed loader above.
pub fn parse_coordinator_document(
    text: &str,
    path: &Path,
) -> Result<CoordinatorConfigDocument, CoordinatorConfigError> {
    let table: toml::Table =
        toml::from_str(text).map_err(|source| CoordinatorConfigError::Malformed {
            path: path.to_path_buf(),
            source,
        })?;
    let coordinator = parse_coordinator_values(&table, path)?;
    Ok(CoordinatorConfigDocument {
        coordinator,
        table,
        exists: true,
    })
}

/// Parse the coordinator-owned keys in an already parsed table.
pub fn parse_coordinator_values(
    table: &toml::Table,
    path: &Path,
) -> Result<CoordinatorConfig, CoordinatorConfigError> {
    let sessions = match table.get("sessions") {
        None => Vec::new(),
        Some(value) => {
            let Some(items) = value.as_array() else {
                return Err(wrong_type(path, "sessions", "an array of strings", value));
            };
            let mut sessions = Vec::with_capacity(items.len());
            for item in items {
                let Some(session) = item.as_str() else {
                    return Err(wrong_type(
                        path,
                        "sessions",
                        "an array whose entries are strings",
                        item,
                    ));
                };
                sessions.push(session.to_string());
            }
            sessions
        }
    };
    let tmux_socket = optional_string(table, path, "tmux_socket")?;
    let tmux_config = optional_string(table, path, "tmux_config")?.map(PathBuf::from);
    Ok(CoordinatorConfig {
        sessions,
        tmux_socket,
        tmux_config,
    })
}

fn optional_string(
    table: &toml::Table,
    path: &Path,
    key: &'static str,
) -> Result<Option<String>, CoordinatorConfigError> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(str::to_string)
        .map(Some)
        .ok_or_else(|| wrong_type(path, key, "a string", value))
}

fn wrong_type(
    path: &Path,
    key: &'static str,
    expected: &'static str,
    value: &toml::Value,
) -> CoordinatorConfigError {
    CoordinatorConfigError::WrongType {
        path: path.to_path_buf(),
        key,
        expected,
        actual: value.type_str(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    use super::*;

    fn home(name: &str) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("create scratch home");
        let path = std::fs::canonicalize(home.path()).expect("canonicalize scratch home");
        StateRoot::open_or_create(&path).expect("create safe state root");
        (home, path)
    }

    fn write_config(home: &Path, body: &str) {
        StateRoot::open_existing(home)
            .expect("open state root")
            .expect("state root exists")
            .replace_file(Path::new(COORDINATOR_CONFIG_FILE), body.as_bytes())
            .expect("write safe config");
    }

    #[test]
    fn missing_config_uses_explicit_defaults() {
        let home = tempfile::tempdir().expect("create scratch home");
        let path = std::fs::canonicalize(home.path()).expect("canonicalize scratch home");

        let config = load_coordinator_config(&path).expect("missing config is valid");
        assert!(!config.exists);
        assert_eq!(config.coordinator, CoordinatorConfig::default());
    }

    #[test]
    fn valid_nondefault_config_is_shared_exactly() {
        let (_home, home) = home("coordinator-config-valid");
        write_config(
            &home,
            "sessions = [\"main\", \"review\"]\ntmux_socket = \"cyclops-test\"\ntmux_config = \"/dev/null\"\ndefault_workspace = \"main\"\n",
        );

        let config = load_coordinator_config(&home).expect("valid config loads");
        assert!(config.exists);
        assert_eq!(config.coordinator.sessions, ["main", "review"]);
        assert_eq!(
            config.coordinator.tmux_socket.as_deref(),
            Some("cyclops-test")
        );
        assert_eq!(
            config.coordinator.tmux_config.as_deref(),
            Some(Path::new("/dev/null"))
        );
        assert_eq!(config.table["default_workspace"].as_str(), Some("main"));
    }

    #[test]
    fn malformed_config_refuses_instead_of_defaulting_tmux() {
        let (_home, home) = home("coordinator-config-malformed");
        write_config(&home, "sessions = [");

        assert!(matches!(
            load_coordinator_config(&home),
            Err(CoordinatorConfigError::Malformed { .. })
        ));
    }

    #[test]
    fn wrongly_typed_coordinator_values_refuse_instead_of_defaulting_tmux() {
        let (_home, home) = home("coordinator-config-wrong-type");
        write_config(&home, "tmux_socket = 7");

        assert!(matches!(
            load_coordinator_config(&home),
            Err(CoordinatorConfigError::WrongType {
                key: "tmux_socket",
                ..
            })
        ));
    }

    #[test]
    fn linked_config_refuses_without_reading_the_target() {
        let (_home, home) = home("coordinator-config-link");
        let target_root = tempfile::tempdir().expect("create target directory");
        let target = target_root.path().join("config.toml");
        std::fs::write(&target, "tmux_socket = \"foreign\"\n").expect("write target");
        symlink(&target, home.join(COORDINATOR_CONFIG_FILE)).expect("link config");

        assert!(matches!(
            load_coordinator_config(&home),
            Err(CoordinatorConfigError::Read { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&target).expect("target remains readable"),
            "tmux_socket = \"foreign\"\n"
        );
    }

    #[test]
    fn unreadable_config_refuses_before_any_permission_repair() {
        let (_home, home) = home("coordinator-config-unreadable");
        write_config(&home, "tmux_socket = \"configured\"\n");
        let path = home.join(COORDINATOR_CONFIG_FILE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("make config unreadable");

        assert!(matches!(
            load_coordinator_config(&home),
            Err(CoordinatorConfigError::Unreadable { .. })
        ));
        assert_eq!(
            std::fs::metadata(&path)
                .expect("config remains in place")
                .permissions()
                .mode()
                & 0o777,
            0o000,
            "the safety read must not repair a file it refuses"
        );
    }
}
