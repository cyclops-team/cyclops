//! Read-only installation and state inspection.
//!
//! This command never starts the daemon and never repairs state. Cyclops state
//! is read only through `cyclops-state` held descriptors. PATH and directory
//! inventories have fixed count and byte ceilings.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cyclops_proto::{Hello, ProcessInstanceId, SessionIdentityBinding, StatusResult, WorkspaceId};
use cyclops_state::{InspectedEntry, InspectedKind, InspectionLimits, StateInspector};
use serde_json::{json, Value};

use crate::client::{Client, ClientError};
use crate::hookset::CliKind;
use crate::style::Style;

const PATH_ENTRY_LIMIT: usize = 128;
const PATH_BYTES_LIMIT: usize = 32 * 1_024;
const STATE_ENTRY_LIMIT: usize = 2_048;
const STATE_NAME_BYTES_LIMIT: usize = 128 * 1_024;
const STATE_DEPTH_LIMIT: usize = 8;
const FILE_BYTES_LIMIT: usize = 512 * 1_024;
const PROCESS_BYTES_LIMIT: usize = 2 * 1_024 * 1_024;

#[derive(Clone)]
struct Issue {
    code: &'static str,
    message: String,
    path: Option<PathBuf>,
}

#[derive(Clone)]
struct BinaryResolution {
    name: &'static str,
    path: PathBuf,
    resolved: Option<PathBuf>,
    executable: bool,
    path_index: Option<usize>,
    selected: bool,
}

struct BinaryReport {
    selected_client: PathBuf,
    selected_resolved: Option<PathBuf>,
    selected_daemon: PathBuf,
    selected_daemon_resolved: Option<PathBuf>,
    selected_daemon_ready: bool,
    selected_daemon_build: Option<String>,
    selected_daemon_build_error: Option<String>,
    resolutions: Vec<BinaryResolution>,
    path_truncated: bool,
    path_entries: usize,
    path_bytes: usize,
    shadowed: bool,
}

struct DaemonReport {
    /// A peer completed the socket greeting on this connection.
    running: bool,
    /// The named socket still identifies the endpoint used for the greeting.
    authenticated_socket: bool,
    stale_socket: bool,
    version: Option<String>,
    build: Option<String>,
    executable: Option<PathBuf>,
    boot_id: Option<String>,
    process: Option<ProcessInstanceId>,
    uptime_ms: Option<u64>,
    build_matches_client: Option<bool>,
    status: Option<StatusResult>,
    status_error: Option<String>,
    transport_error: Option<String>,
}

struct DaemonProcess {
    process: ProcessInstanceId,
    command: String,
    selected: bool,
}

struct DaemonProcessReport {
    state: &'static str,
    processes: Vec<DaemonProcess>,
    duplicate: Option<bool>,
    truncated: bool,
    error: Option<String>,
}

struct WorkspaceMappingReport {
    state: &'static str,
    daemon: Option<WorkspaceId>,
    recorded: Option<WorkspaceId>,
    error: Option<String>,
}

struct SessionMappingReport {
    name: String,
    attached: Option<bool>,
    configured: Option<bool>,
    state: &'static str,
    binding: Option<SessionIdentityBinding>,
}

struct SessionConfigReport {
    state: &'static str,
    configured: Vec<String>,
    stale: Vec<String>,
    dynamic: Vec<String>,
    duplicates: Vec<String>,
    error: Option<String>,
}

struct WatcherReport {
    state: &'static str,
    slots: usize,
    duplicate_names: Vec<String>,
}

struct OperationalReport {
    workspace: WorkspaceMappingReport,
    sessions: Vec<SessionMappingReport>,
    config: SessionConfigReport,
    watchers: WatcherReport,
    daemons: DaemonProcessReport,
    session_record_error: Option<String>,
}

struct StateReport {
    present: bool,
    root: Option<InspectedEntry>,
    entries: Vec<InspectedEntry>,
    truncated: bool,
    retained_name_bytes: usize,
    error: Option<String>,
}

struct ConsumerReport {
    id: &'static str,
    name: &'static str,
    installed: bool,
    install_state: &'static str,
    manifest_path: PathBuf,
    manifest_state: &'static str,
    ack_capable: Option<bool>,
    hook_path: Option<PathBuf>,
    hook_state: &'static str,
    skill_path: PathBuf,
    skill_state: &'static str,
    mailbox_transport: Option<&'static str>,
    complete: bool,
}

struct SetupReport {
    home_available: bool,
    complete: bool,
    consumers: Vec<ConsumerReport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExternalPresence {
    Absent,
    Present,
    Unproven,
}

impl ExternalPresence {
    fn word(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Present => "present",
            Self::Unproven => "unproven",
        }
    }

    fn as_optional_bool(self) -> Option<bool> {
        match self {
            Self::Absent => Some(false),
            Self::Present => Some(true),
            Self::Unproven => None,
        }
    }
}

struct ExternalStateReport {
    path: PathBuf,
    presence: ExternalPresence,
    safe: Option<bool>,
    truncated: bool,
    entries: usize,
    error: Option<String>,
    candidates: Vec<ExternalCandidateReport>,
}

struct ExternalCandidateReport {
    class: &'static str,
    path: PathBuf,
    safe: bool,
    truncated: bool,
    entries: usize,
    bytes: u64,
    marker: &'static str,
    lease: &'static str,
    error: Option<String>,
}

struct RollbackReport {
    state: &'static str,
    prefix: Option<PathBuf>,
    selection: Option<PathBuf>,
    active_pair: Option<PathBuf>,
    known_good_pair: Option<PathBuf>,
    active_identity: Option<String>,
    known_good_identity: Option<String>,
    active_build: Option<String>,
    known_good_build: Option<String>,
    active_install_replay: Option<bool>,
    known_good_install_replay: Option<bool>,
    known_good_replay_snapshot: Option<String>,
    candidate_available: Option<bool>,
    install_replay: &'static str,
    journal_replay: &'static str,
    rollback_safe: Option<bool>,
    reason: String,
    error: Option<String>,
}

struct HealthReport {
    binaries: BinaryReport,
    daemon: DaemonReport,
    state: StateReport,
    setup: SetupReport,
    build_cache: ExternalStateReport,
    update_scratch: ExternalStateReport,
    rollback: RollbackReport,
    operational: OperationalReport,
    issues: Vec<Issue>,
}

struct ConsumerSpec {
    id: &'static str,
    name: &'static str,
    kind: CliKind,
    required_receipt_tier: u8,
}

const CONSUMERS: &[ConsumerSpec] = &[
    ConsumerSpec {
        id: "claude",
        name: "Claude Code",
        kind: CliKind::Claude,
        required_receipt_tier: 1,
    },
    ConsumerSpec {
        id: "codex",
        name: "Codex CLI",
        kind: CliKind::Codex,
        required_receipt_tier: 1,
    },
    ConsumerSpec {
        id: "cursor",
        name: "Cursor Agent CLI",
        kind: CliKind::Cursor,
        required_receipt_tier: 1,
    },
    ConsumerSpec {
        id: "agy",
        name: "Antigravity CLI",
        kind: CliKind::Agy,
        required_receipt_tier: 2,
    },
];

pub fn run(json_out: bool) -> i32 {
    let report = collect();
    if json_out {
        println!("{}", report_json(&report));
    } else {
        println!("{}", render_plain(&report, &Style::none()));
    }
    i32::from(!report.issues.is_empty())
}

fn collect() -> HealthReport {
    let home = cyclops_proto::cyclops_home();
    let binaries = inspect_binaries();
    let state = inspect_state(&home);
    let daemon = inspect_daemon(&home, &state);
    let setup = inspect_setup(&home);
    let (build_cache, update_scratch) = inspect_operational_state(&home);
    let rollback = inspect_rollback(&binaries);
    let operational = inspect_operational(&home, &daemon);
    let mut issues = Vec::new();

    if binaries.shadowed {
        issues.push(Issue {
            code: "shadowed_binaries",
            message: "more than one Cyclops installation resolves from PATH".into(),
            path: None,
        });
    }
    if binaries.path_truncated {
        issues.push(Issue {
            code: "path_inventory_truncated",
            message: "PATH inventory reached its count or byte limit".into(),
            path: None,
        });
    }
    if !binaries.selected_daemon_ready {
        issues.push(Issue {
            code: "selected_daemon_unavailable",
            message: format!(
                "the selected client has no executable daemon at {}",
                binaries.selected_daemon.display()
            ),
            path: Some(binaries.selected_daemon.clone()),
        });
    }
    if binaries.selected_daemon_ready && binaries.selected_daemon_resolved.is_none() {
        issues.push(Issue {
            code: "selected_daemon_identity_unproven",
            message: format!(
                "the selected daemon path cannot be resolved at {}",
                binaries.selected_daemon.display()
            ),
            path: Some(binaries.selected_daemon.clone()),
        });
    }
    if let Some(error) = &binaries.selected_daemon_build_error {
        issues.push(Issue {
            code: "selected_daemon_build_unproven",
            message: error.clone(),
            path: Some(binaries.selected_daemon.clone()),
        });
    }
    if binaries
        .selected_daemon_build
        .as_deref()
        .is_some_and(|build| build != crate::BUILD_REF)
    {
        issues.push(Issue {
            code: "selected_daemon_build_mismatch",
            message: format!(
                "client build {} does not match selected adjacent daemon build {}",
                crate::BUILD_REF,
                binaries
                    .selected_daemon_build
                    .as_deref()
                    .unwrap_or("unreported")
            ),
            path: Some(binaries.selected_daemon.clone()),
        });
    }
    if daemon.running && daemon.build.as_deref() != Some(crate::BUILD_REF) {
        issues.push(Issue {
            code: "client_daemon_build_mismatch",
            message: format!(
                "client build {} does not match daemon build {}",
                crate::BUILD_REF,
                daemon.build.as_deref().unwrap_or("unreported")
            ),
            path: None,
        });
    }
    if daemon.running && daemon.process.is_none() {
        issues.push(Issue {
            code: "daemon_process_unproven",
            message: "the authenticated daemon did not report its process generation".into(),
            path: Some(cyclops_proto::socket_path()),
        });
    }
    if daemon.running && daemon.executable.is_none() {
        issues.push(Issue {
            code: "daemon_executable_unproven",
            message: "the authenticated daemon did not report one stable executable path".into(),
            path: Some(cyclops_proto::socket_path()),
        });
    }
    if daemon.running
        && daemon
            .executable
            .as_ref()
            .zip(binaries.selected_daemon_resolved.as_ref())
            .is_some_and(|(running, selected)| running != selected)
    {
        issues.push(Issue {
            code: "daemon_executable_mismatch",
            message: format!(
                "running daemon executable {} does not match selected adjacent daemon {}",
                daemon
                    .executable
                    .as_ref()
                    .expect("checked as present")
                    .display(),
                binaries
                    .selected_daemon_resolved
                    .as_ref()
                    .expect("checked as present")
                    .display()
            ),
            path: daemon.executable.clone(),
        });
    }
    if daemon.stale_socket {
        issues.push(Issue {
            code: "stale_socket",
            message: "a socket entry exists but no authenticated daemon answers".into(),
            path: Some(cyclops_proto::socket_path()),
        });
    }
    if let Some(error) = &daemon.transport_error {
        issues.push(Issue {
            code: "daemon_identity_unavailable",
            message: error.clone(),
            path: Some(cyclops_proto::socket_path()),
        });
    }
    if daemon.running && daemon.status.is_none() {
        issues.push(Issue {
            code: "daemon_status_unavailable",
            message: daemon
                .status_error
                .clone()
                .unwrap_or_else(|| "the authenticated daemon returned no status snapshot".into()),
            path: Some(cyclops_proto::socket_path()),
        });
    }
    if operational.daemons.duplicate == Some(true) {
        issues.push(Issue {
            code: "duplicate_daemons",
            message: "more than one live cyclopsd process belongs to this user".into(),
            path: None,
        });
    }
    if operational.daemons.state == "unproven" {
        issues.push(Issue {
            code: "daemon_process_inventory_unproven",
            message: operational
                .daemons
                .error
                .clone()
                .unwrap_or_else(|| "running daemon process inventory is unproven".into()),
            path: None,
        });
    }
    if operational.watchers.state == "duplicate" {
        issues.push(Issue {
            code: "duplicate_watchers",
            message: format!(
                "duplicate session watcher slots: {}",
                operational.watchers.duplicate_names.join(", ")
            ),
            path: None,
        });
    }
    if operational.config.state == "stale" || operational.config.state == "invalid" {
        issues.push(Issue {
            code: "stale_session_config",
            message: operational.config.error.clone().unwrap_or_else(|| {
                format!(
                    "configured sessions are stale or duplicated: {}",
                    operational.config.stale.join(", ")
                )
            }),
            path: Some(home.join("config.toml")),
        });
    }
    if operational.workspace.state == "invalid"
        || (daemon.running && operational.workspace.state != "current")
    {
        issues.push(Issue {
            code: "workspace_mapping_unproven",
            message: operational
                .workspace
                .error
                .clone()
                .unwrap_or_else(|| "daemon and state workspace identities do not match".into()),
            path: Some(home.join("identity/workspace-id")),
        });
    }
    if let Some(error) = &operational.session_record_error {
        issues.push(Issue {
            code: "session_mapping_record_invalid",
            message: error.clone(),
            path: Some(home.join("identity/sessions.ndjson")),
        });
    }
    for session in operational.sessions.iter().filter(|session| {
        matches!(
            session.state,
            "invalid_record" | "unproven" | "conflict" | "missing_record"
        )
    }) {
        issues.push(Issue {
            code: "session_mapping_unproven",
            message: format!(
                "session {} has {} durable identity mapping",
                session.name, session.state
            ),
            path: Some(home.join("identity/sessions.ndjson")),
        });
    }
    if let Some(error) = &state.error {
        issues.push(Issue {
            code: "state_inspection_failed",
            message: error.clone(),
            path: Some(home.clone()),
        });
    }
    for entry in state
        .root
        .iter()
        .chain(state.entries.iter())
        .filter(|entry| !entry.safe())
    {
        issues.push(Issue {
            code: "unsafe_state_entry",
            message: entry.unsafe_reason.unwrap_or("unsafe state entry").into(),
            path: Some(entry.path.clone()),
        });
    }
    if state.truncated {
        issues.push(Issue {
            code: "state_inventory_truncated",
            message: "state inventory reached its count, byte, or depth limit".into(),
            path: Some(home.clone()),
        });
    }
    if !setup.home_available {
        issues.push(Issue {
            code: "setup_home_unavailable",
            message: "HOME is unavailable, so vendor setup cannot be inspected".into(),
            path: None,
        });
    }
    for consumer in setup
        .consumers
        .iter()
        .filter(|consumer| consumer.installed && !consumer.complete)
    {
        issues.push(Issue {
            code: "consumer_setup_incomplete",
            message: format!(
                "{} setup is incomplete: install {}, manifest {}, hooks {}, skill {}",
                consumer.name,
                consumer.install_state,
                consumer.manifest_state,
                consumer.hook_state,
                consumer.skill_state
            ),
            path: Some(consumer.manifest_path.clone()),
        });
    }
    if build_cache.presence == ExternalPresence::Present && build_cache.safe == Some(false) {
        issues.push(Issue {
            code: "unsafe_build_cache",
            message: build_cache
                .error
                .clone()
                .unwrap_or_else(|| "build cache failed owner-only inspection".into()),
            path: Some(build_cache.path.clone()),
        });
    }
    if build_cache.truncated {
        issues.push(Issue {
            code: "build_cache_inventory_truncated",
            message: "build-cache inventory reached its count or byte limit".into(),
            path: Some(build_cache.path.clone()),
        });
    }
    if let Some(error) = &build_cache.error {
        if build_cache.safe != Some(false) {
            issues.push(Issue {
                code: "build_cache_uninspectable",
                message: error.clone(),
                path: Some(build_cache.path.clone()),
            });
        }
    }
    if update_scratch.presence == ExternalPresence::Present && update_scratch.safe == Some(false) {
        issues.push(Issue {
            code: "unsafe_update_scratch",
            message: update_scratch
                .error
                .clone()
                .unwrap_or_else(|| "update scratch failed owner-only inspection".into()),
            path: Some(update_scratch.path.clone()),
        });
    }
    if update_scratch.truncated {
        issues.push(Issue {
            code: "update_scratch_inventory_truncated",
            message: "update-scratch inventory reached its count or byte limit".into(),
            path: Some(update_scratch.path.clone()),
        });
    }
    if let Some(error) = &update_scratch.error {
        if update_scratch.safe != Some(false) {
            issues.push(Issue {
                code: "update_scratch_uninspectable",
                message: error.clone(),
                path: Some(update_scratch.path.clone()),
            });
        }
    }
    if rollback.state == "concurrent_change" {
        issues.push(Issue {
            code: "rollback_inspection_changed",
            message: rollback
                .error
                .clone()
                .unwrap_or_else(|| "the managed pair changed during health inspection".into()),
            path: rollback.prefix.clone(),
        });
    } else if let Some(error) = &rollback.error {
        issues.push(Issue {
            code: "rollback_proof_invalid",
            message: error.clone(),
            path: rollback.prefix.clone(),
        });
    }
    HealthReport {
        binaries,
        daemon,
        state,
        setup,
        build_cache,
        update_scratch,
        rollback,
        operational,
        issues,
    }
}

fn inspect_binaries() -> BinaryReport {
    let selected_client = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cyclops"));
    let path = std::env::var_os("PATH");
    inspect_binaries_from(path.as_deref(), selected_client)
}

fn inspect_binaries_from(path: Option<&OsStr>, selected_client: PathBuf) -> BinaryReport {
    let selected_resolved = std::fs::canonicalize(&selected_client).ok();
    let mut resolutions = Vec::new();
    let mut path_truncated = false;
    let mut path_entries = 0usize;
    let mut path_bytes = 0usize;

    if let Some(path) = path {
        for (index, directory) in std::env::split_paths(path).enumerate() {
            let bytes = directory.as_os_str().as_bytes().len();
            let Some(next_bytes) = path_bytes.checked_add(bytes) else {
                path_truncated = true;
                break;
            };
            if path_entries >= PATH_ENTRY_LIMIT || next_bytes > PATH_BYTES_LIMIT {
                path_truncated = true;
                break;
            }
            path_entries += 1;
            path_bytes = next_bytes;
            for name in ["cyclops", "cyclopsd"] {
                let candidate = directory.join(name);
                let Ok(_) = std::fs::symlink_metadata(&candidate) else {
                    continue;
                };
                let resolved = std::fs::canonicalize(&candidate).ok();
                let executable = std::fs::metadata(&candidate).ok().is_some_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                });
                let selected = name == "cyclops"
                    && resolved
                        .as_ref()
                        .zip(selected_resolved.as_ref())
                        .is_some_and(|(left, right)| left == right);
                resolutions.push(BinaryResolution {
                    name: if name == "cyclops" {
                        "cyclops"
                    } else {
                        "cyclopsd"
                    },
                    path: candidate,
                    resolved,
                    executable,
                    path_index: Some(index),
                    selected,
                });
            }
        }
    }

    if !resolutions.iter().any(|entry| entry.selected) {
        resolutions.push(BinaryResolution {
            name: "cyclops",
            path: selected_client.clone(),
            resolved: selected_resolved.clone(),
            executable: true,
            path_index: None,
            selected: true,
        });
    }
    if let Some(directory) = selected_client.parent() {
        let daemon = directory.join("cyclopsd");
        if std::fs::symlink_metadata(&daemon).is_ok()
            && !resolutions.iter().any(|entry| entry.path == daemon)
        {
            let executable = std::fs::metadata(&daemon).ok().is_some_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            });
            resolutions.push(BinaryResolution {
                name: "cyclopsd",
                resolved: std::fs::canonicalize(&daemon).ok(),
                path: daemon,
                executable,
                path_index: None,
                selected: false,
            });
        }
    }

    let mut distinct_clients = Vec::<PathBuf>::new();
    let mut distinct_daemons = Vec::<PathBuf>::new();
    for entry in resolutions.iter().filter(|entry| entry.executable) {
        let identity = entry.resolved.clone().unwrap_or_else(|| entry.path.clone());
        let set = if entry.name == "cyclops" {
            &mut distinct_clients
        } else {
            &mut distinct_daemons
        };
        if !set.contains(&identity) {
            set.push(identity);
        }
    }
    let selected_daemon = selected_client.parent().map_or_else(
        || PathBuf::from("cyclopsd"),
        |parent| parent.join("cyclopsd"),
    );
    let selected_daemon_ready = resolutions
        .iter()
        .any(|entry| entry.name == "cyclopsd" && entry.path == selected_daemon && entry.executable);
    let selected_daemon_resolved = std::fs::canonicalize(&selected_daemon).ok();
    let (selected_daemon_build, selected_daemon_build_error) = if selected_daemon_ready {
        match crate::update::candidate_build(&selected_daemon) {
            Ok(build) => (Some(build), None),
            Err(error) => (None, Some(error)),
        }
    } else {
        (None, None)
    };
    let shadowed = distinct_clients.len() > 1 || distinct_daemons.len() > 1;
    BinaryReport {
        selected_client,
        selected_resolved,
        selected_daemon,
        selected_daemon_resolved,
        selected_daemon_ready,
        selected_daemon_build,
        selected_daemon_build_error,
        resolutions,
        path_truncated,
        path_entries,
        path_bytes,
        shadowed,
    }
}

fn selected_public_prefix(binaries: &BinaryReport) -> Option<PathBuf> {
    binaries
        .resolutions
        .iter()
        .find(|entry| {
            entry.name == "cyclops"
                && entry.selected
                && entry.executable
                && entry.path_index.is_some()
        })
        .and_then(|entry| entry.path.parent())
        .map(Path::to_path_buf)
}

fn inspect_rollback(binaries: &BinaryReport) -> RollbackReport {
    inspect_rollback_with(binaries, crate::update::installed_pair_descriptor)
}

fn inspect_rollback_with<F>(binaries: &BinaryReport, inspect: F) -> RollbackReport
where
    F: FnOnce(
        &Path,
    ) -> Result<
        Option<crate::update::InstalledPairDescriptor>,
        crate::update::InstalledPairInspectionError,
    >,
{
    let Some(prefix) = selected_public_prefix(binaries) else {
        return rollback_unproven(
            None,
            "no PATH-resolved selected public client identifies an install prefix",
        );
    };
    match inspect(&prefix) {
        Ok(None) => rollback_unproven(
            Some(prefix),
            "the selected public installation has no managed rollback descriptor",
        ),
        Err(crate::update::InstalledPairInspectionError::ConcurrentChange(error)) => {
            RollbackReport {
                state: "concurrent_change",
                prefix: Some(prefix),
                selection: None,
                active_pair: None,
                known_good_pair: None,
                active_identity: None,
                known_good_identity: None,
                active_build: None,
                known_good_build: None,
                active_install_replay: None,
                known_good_install_replay: None,
                known_good_replay_snapshot: None,
                candidate_available: None,
                install_replay: "unproven",
                journal_replay: "unproven",
                rollback_safe: None,
                reason: "the managed pair changed during read-only inspection".into(),
                error: Some(error),
            }
        }
        Err(crate::update::InstalledPairInspectionError::Invalid(error)) => RollbackReport {
            state: "invalid",
            prefix: Some(prefix),
            selection: None,
            active_pair: None,
            known_good_pair: None,
            active_identity: None,
            known_good_identity: None,
            active_build: None,
            known_good_build: None,
            active_install_replay: None,
            known_good_install_replay: None,
            known_good_replay_snapshot: None,
            candidate_available: None,
            install_replay: "unproven",
            journal_replay: "unproven",
            rollback_safe: None,
            reason: "the managed rollback proof is stale, changed, or unsafe".into(),
            error: Some(error),
        },
        Ok(Some(descriptor)) => {
            let legacy = descriptor.proof_unproven || descriptor.active_identity.is_none();
            let candidate_available = !legacy && descriptor.rollback_safe;
            let state = if legacy {
                "unproven"
            } else if candidate_available {
                "candidate"
            } else {
                "not_available"
            };
            let reason = if legacy {
                "the selected pair predates complete recorded identity; run one update before trusting rollback"
            } else if candidate_available && descriptor.known_good_replay_attested {
                "the known-good pair replayed a recorded install-time snapshot; current journal replay is checked only when rollback runs"
            } else if candidate_available {
                "a distinct known-good pair has a validated binary proof; install-time and current journal replay are unproven"
            } else {
                "the descriptor is valid but has no distinct known-good pair"
            };
            let install_replay = if descriptor.known_good_replay_attested {
                "attested_snapshot"
            } else {
                "unproven"
            };
            RollbackReport {
                state,
                prefix: Some(prefix),
                selection: Some(descriptor.selection),
                active_pair: Some(descriptor.active_pair),
                known_good_pair: Some(descriptor.known_good_pair),
                active_identity: descriptor.active_identity,
                known_good_identity: descriptor.known_good_identity,
                active_build: descriptor.active_build,
                known_good_build: descriptor.known_good_build,
                active_install_replay: Some(descriptor.active_replay_attested),
                known_good_install_replay: Some(descriptor.known_good_replay_attested),
                known_good_replay_snapshot: descriptor.known_good_replay_snapshot,
                candidate_available: Some(candidate_available),
                install_replay,
                journal_replay: "unproven",
                rollback_safe: None,
                reason: reason.into(),
                error: None,
            }
        }
    }
}

fn rollback_unproven(prefix: Option<PathBuf>, reason: impl Into<String>) -> RollbackReport {
    RollbackReport {
        state: "unproven",
        prefix,
        selection: None,
        active_pair: None,
        known_good_pair: None,
        active_identity: None,
        known_good_identity: None,
        active_build: None,
        known_good_build: None,
        active_install_replay: None,
        known_good_install_replay: None,
        known_good_replay_snapshot: None,
        candidate_available: None,
        install_replay: "unproven",
        journal_replay: "unproven",
        rollback_safe: None,
        reason: reason.into(),
        error: None,
    }
}

fn daemon_stopped() -> DaemonReport {
    DaemonReport {
        running: false,
        authenticated_socket: false,
        stale_socket: false,
        version: None,
        build: None,
        executable: None,
        boot_id: None,
        process: None,
        uptime_ms: None,
        build_matches_client: None,
        status: None,
        status_error: None,
        transport_error: None,
    }
}

fn daemon_unproven(reason: impl Into<String>) -> DaemonReport {
    let mut report = daemon_stopped();
    report.transport_error = Some(reason.into());
    report
}

fn daemon_stale_socket() -> DaemonReport {
    let mut report = daemon_stopped();
    report.stale_socket = true;
    report
}

fn inspect_daemon(home: &Path, state: &StateReport) -> DaemonReport {
    if !state.present {
        return daemon_stopped();
    }
    let Some(expected_root) = state.root.as_ref() else {
        return daemon_unproven("daemon not inspected because the state root is not proven");
    };
    if state.error.is_some() || state.truncated || !expected_root.safe() {
        return daemon_unproven("daemon not inspected because the state root is not safely proven");
    }
    let socket_path = cyclops_proto::socket_path();
    let Some(expected_socket) = state.entries.iter().find(|entry| entry.path == socket_path) else {
        return daemon_stopped();
    };
    if expected_socket.kind != InspectedKind::Socket || !expected_socket.safe() {
        return daemon_unproven(
            "daemon not inspected because the socket entry is not safely proven",
        );
    }

    let mut client = match Client::connect() {
        Ok(client) => client,
        Err(ClientError::NotRunning(_)) => return daemon_stale_socket(),
        Err(error) => return daemon_unproven(crate::copy::client_error(&error, None)),
    };
    let hello = client.hello().clone();
    let (status, status_error) = match client.request("health.snapshot", json!({})) {
        Ok(value) => match serde_json::from_value::<StatusResult>(value) {
            Ok(status) => (Some(status), None),
            Err(error) => (None, Some(format!("decode daemon status: {error}"))),
        },
        Err(error) => (None, Some(crate::copy::client_error(&error, None))),
    };
    daemon_from_hello(
        hello,
        socket_identity_is_stable(home, expected_root, expected_socket),
        status,
        status_error,
    )
}

fn daemon_from_hello(
    hello: Hello,
    authenticated_socket: bool,
    status: Option<StatusResult>,
    status_error: Option<String>,
) -> DaemonReport {
    let mut transport_error = None;
    let executable = hello.daemon_executable.map(PathBuf::from);
    let executable = match executable {
        Some(path) if !path.is_absolute() => {
            transport_error = Some("daemon reported a non-absolute executable path".into());
            None
        }
        Some(path) => match std::fs::canonicalize(&path) {
            Ok(path) => Some(path),
            Err(error) => {
                transport_error = Some(format!(
                    "resolve daemon executable {}: {error}",
                    path.display()
                ));
                None
            }
        },
        None => None,
    };
    if !authenticated_socket {
        transport_error = Some("state root or socket changed during daemon inspection".into());
    }
    let uptime_ms = status.as_ref().map(|status| status.uptime_ms);
    DaemonReport {
        running: true,
        authenticated_socket,
        stale_socket: false,
        version: Some(hello.cyclops),
        build_matches_client: Some(hello.build.as_deref() == Some(crate::BUILD_REF)),
        build: hello.build,
        executable,
        boot_id: Some(hello.boot_id),
        process: hello.daemon_process,
        uptime_ms,
        status,
        status_error,
        transport_error,
    }
}

fn inspect_operational(home: &Path, daemon: &DaemonReport) -> OperationalReport {
    let (config_state, configured, config_error) = inspect_configured_sessions(home);
    let configured_set = configured.iter().cloned().collect::<BTreeSet<_>>();
    let (recorded_workspace, workspace_error) = inspect_recorded_workspace(home);
    let (recorded_sessions, session_record_error) = inspect_recorded_sessions(home);
    let status = daemon.status.as_ref();

    let daemon_workspace = status.and_then(|status| status.workspace_id);
    let workspace_state = match (
        daemon_workspace,
        recorded_workspace,
        workspace_error.as_ref(),
    ) {
        (_, _, Some(_)) => "invalid",
        (Some(daemon), Some(recorded), None) if daemon == recorded => "current",
        (Some(_), Some(_), None) => "mismatch",
        (Some(_), None, None) => "missing_record",
        _ => "unproven",
    };
    let workspace_error = workspace_error.or_else(|| match workspace_state {
        "mismatch" => Some("daemon workspace identity differs from the durable record".into()),
        "missing_record" => Some("daemon workspace identity has no durable record".into()),
        "unproven" if daemon.running => {
            Some("daemon status did not carry a durable workspace identity".into())
        }
        _ => None,
    });

    let mut sessions = Vec::new();
    let mut matched_records = BTreeSet::new();
    if let Some(status) = status {
        for session in &status.sessions {
            let state = match (&session.identity, session_record_error.as_ref()) {
                (_, Some(_)) => "invalid_record",
                (None, None) => "unproven",
                (Some(binding), None)
                    if recorded_sessions.iter().any(|recorded| recorded == binding) =>
                {
                    matched_records.insert(binding.session_instance_id());
                    "current"
                }
                (Some(binding), None)
                    if recorded_sessions.iter().any(|recorded| {
                        recorded.live_session_key() == binding.live_session_key()
                            || recorded.session_instance_id() == binding.session_instance_id()
                    }) =>
                {
                    "conflict"
                }
                (Some(_), None) => "missing_record",
            };
            sessions.push(SessionMappingReport {
                name: session.name.clone(),
                attached: Some(session.attached),
                configured: Some(configured_set.contains(&session.name)),
                state,
                binding: session.identity.clone(),
            });
        }
    }
    for binding in recorded_sessions
        .iter()
        .filter(|binding| !matched_records.contains(&binding.session_instance_id()))
    {
        sessions.push(SessionMappingReport {
            name: binding.live_session_key().tmux_session_id().to_string(),
            attached: None,
            configured: None,
            state: "runtime_unproven",
            binding: Some(binding.clone()),
        });
    }

    let runtime_counts = sessions
        .iter()
        .filter(|session| session.attached.is_some())
        .fold(BTreeMap::new(), |mut counts, session| {
            *counts.entry(session.name.clone()).or_insert(0usize) += 1;
            counts
        });
    let duplicate_watchers = runtime_counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let watcher_state = if status.is_none() {
        "unproven"
    } else if duplicate_watchers.is_empty() {
        "current"
    } else {
        "duplicate"
    };

    let mut duplicates = Vec::new();
    let mut seen = BTreeSet::new();
    for name in &configured {
        if !seen.insert(name.clone()) && !duplicates.contains(name) {
            duplicates.push(name.clone());
        }
    }
    let mut stale = Vec::new();
    if status.is_some() {
        for name in &seen {
            let matches = sessions
                .iter()
                .filter(|session| session.attached.is_some() && &session.name == name)
                .collect::<Vec<_>>();
            if matches.is_empty()
                || matches
                    .iter()
                    .all(|session| session.attached == Some(false))
            {
                stale.push(name.clone());
            }
        }
    }
    let dynamic = sessions
        .iter()
        .filter(|session| session.configured == Some(false))
        .map(|session| session.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let config_state = match (config_state, status.is_some()) {
        ("invalid", _) => "invalid",
        (_, false) => "unproven",
        (_, true) if !stale.is_empty() || !duplicates.is_empty() => "stale",
        _ => "current",
    };
    let config_error = config_error.or_else(|| {
        (!duplicates.is_empty()).then(|| {
            format!(
                "sessions contains duplicate names: {}",
                duplicates.join(", ")
            )
        })
    });

    OperationalReport {
        workspace: WorkspaceMappingReport {
            state: workspace_state,
            daemon: daemon_workspace,
            recorded: recorded_workspace,
            error: workspace_error,
        },
        sessions,
        config: SessionConfigReport {
            state: config_state,
            configured,
            stale,
            dynamic,
            duplicates,
            error: config_error,
        },
        watchers: WatcherReport {
            state: watcher_state,
            slots: status.map_or(0, |status| status.sessions.len()),
            duplicate_names: duplicate_watchers,
        },
        daemons: inspect_daemon_processes(daemon.process),
        session_record_error,
    }
}

fn inspect_configured_sessions(home: &Path) -> (&'static str, Vec<String>, Option<String>) {
    match read_asset(home, Path::new("config.toml")) {
        AssetRead::Missing => ("absent", Vec::new(), None),
        AssetRead::Truncated => (
            "invalid",
            Vec::new(),
            Some("config.toml exceeds the bounded health read".into()),
        ),
        AssetRead::Unproven => (
            "invalid",
            Vec::new(),
            Some("config.toml cannot be read through one safe state descriptor".into()),
        ),
        AssetRead::Bytes(bytes) => {
            let parsed = std::str::from_utf8(&bytes)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    text.parse::<toml::Table>()
                        .map_err(|error| error.to_string())
                });
            let table = match parsed {
                Ok(table) => table,
                Err(error) => {
                    return (
                        "invalid",
                        Vec::new(),
                        Some(format!("config.toml is invalid: {error}")),
                    )
                }
            };
            let Some(value) = table.get("sessions") else {
                return ("current", Vec::new(), None);
            };
            let Some(values) = value.as_array() else {
                return (
                    "invalid",
                    Vec::new(),
                    Some("config.toml sessions must be an array of strings".into()),
                );
            };
            let mut sessions = Vec::with_capacity(values.len());
            for value in values {
                let Some(name) = value.as_str() else {
                    return (
                        "invalid",
                        Vec::new(),
                        Some("config.toml sessions must contain only strings".into()),
                    );
                };
                sessions.push(name.to_string());
            }
            ("current", sessions, None)
        }
    }
}

fn inspect_recorded_workspace(home: &Path) -> (Option<WorkspaceId>, Option<String>) {
    match read_asset(home, Path::new("identity/workspace-id")) {
        AssetRead::Missing => (None, None),
        AssetRead::Bytes(bytes) => match std::str::from_utf8(&bytes)
            .map(str::trim)
            .ok()
            .and_then(|value| value.parse().ok())
        {
            Some(id) => (Some(id), None),
            None => (None, Some("identity/workspace-id is invalid".into())),
        },
        AssetRead::Truncated => (
            None,
            Some("identity/workspace-id exceeds the bounded health read".into()),
        ),
        AssetRead::Unproven => (
            None,
            Some("identity/workspace-id cannot be read safely".into()),
        ),
    }
}

fn inspect_recorded_sessions(home: &Path) -> (Vec<SessionIdentityBinding>, Option<String>) {
    match read_asset(home, Path::new("identity/sessions.ndjson")) {
        AssetRead::Missing => (Vec::new(), None),
        AssetRead::Truncated => (
            Vec::new(),
            Some("identity/sessions.ndjson exceeds the bounded health read".into()),
        ),
        AssetRead::Unproven => (
            Vec::new(),
            Some("identity/sessions.ndjson cannot be read safely".into()),
        ),
        AssetRead::Bytes(bytes) => {
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return (
                    Vec::new(),
                    Some("identity/sessions.ndjson is not UTF-8".into()),
                );
            };
            let mut bindings = Vec::new();
            let mut by_live_key = BTreeMap::new();
            let mut by_instance = BTreeMap::new();
            for (index, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<SessionIdentityBinding>(line) {
                    Ok(binding) => {
                        let live_key = binding.live_session_key().clone();
                        let instance = binding.session_instance_id();
                        let duplicate = by_live_key.insert(live_key.clone(), instance).is_some()
                            || by_instance.insert(instance, live_key).is_some();
                        if duplicate {
                            return (
                                Vec::new(),
                                Some(format!(
                                    "identity/sessions.ndjson line {} repeats or conflicts with an earlier binding",
                                    index + 1
                                )),
                            );
                        }
                        bindings.push(binding);
                    }
                    Err(error) => {
                        return (
                            Vec::new(),
                            Some(format!(
                                "identity/sessions.ndjson line {} is invalid: {error}",
                                index + 1
                            )),
                        )
                    }
                }
            }
            (bindings, None)
        }
    }
}

fn inspect_daemon_processes(selected: Option<ProcessInstanceId>) -> DaemonProcessReport {
    let ps = [Path::new("/bin/ps"), Path::new("/usr/bin/ps")]
        .into_iter()
        .find(|path| path.is_file());
    let Some(ps) = ps else {
        return daemon_processes_unproven("no fixed ps executable is available");
    };
    let mut child = match Command::new(ps)
        .args(["-Ao", "pid=,uid=,comm="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return daemon_processes_unproven(format!("start ps: {error}")),
    };
    let Some(stdout) = child.stdout.take() else {
        return daemon_processes_unproven("ps returned no stdout pipe");
    };
    let mut bytes = Vec::new();
    if let Err(error) = stdout
        .take((PROCESS_BYTES_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
    {
        return daemon_processes_unproven(format!("read ps output: {error}"));
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => return daemon_processes_unproven(format!("wait for ps: {error}")),
    };
    if bytes.len() > PROCESS_BYTES_LIMIT {
        let mut report = daemon_processes_unproven("ps output reached its fixed byte limit");
        report.truncated = true;
        return report;
    }
    if !status.success() {
        return daemon_processes_unproven(format!("ps exited with {status}"));
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return daemon_processes_unproven("ps output is not UTF-8"),
    };
    // SAFETY: geteuid reads process credentials and has no pointer arguments.
    parse_daemon_processes(text, unsafe { libc::geteuid() }, selected)
}

fn parse_daemon_processes(
    text: &str,
    uid: libc::uid_t,
    selected: Option<ProcessInstanceId>,
) -> DaemonProcessReport {
    parse_daemon_processes_with(text, uid, selected, crate::daemon::observe_process)
}

fn parse_daemon_processes_with(
    text: &str,
    uid: libc::uid_t,
    selected: Option<ProcessInstanceId>,
    mut observe: impl FnMut(i32) -> Option<ProcessInstanceId>,
) -> DaemonProcessReport {
    let mut processes = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(row_uid), Some(command)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(row_uid)) = (pid.parse::<i32>(), row_uid.parse::<libc::uid_t>()) else {
            continue;
        };
        if row_uid != uid
            || Path::new(command).file_name().and_then(OsStr::to_str) != Some("cyclopsd")
        {
            continue;
        }
        let Some(process) = observe(pid) else {
            continue;
        };
        processes.push(DaemonProcess {
            process,
            command: command.to_string(),
            selected: selected == Some(process),
        });
    }
    processes.sort_by_key(|process| process.process.pid());
    let selected_missing = selected.is_some() && !processes.iter().any(|process| process.selected);
    if selected_missing {
        return daemon_processes_unproven(
            "the authenticated daemon was not stable across the process inventory",
        );
    }
    DaemonProcessReport {
        state: "proven",
        duplicate: Some(processes.len() > 1),
        processes,
        truncated: false,
        error: None,
    }
}

fn daemon_processes_unproven(error: impl Into<String>) -> DaemonProcessReport {
    DaemonProcessReport {
        state: "unproven",
        processes: Vec::new(),
        duplicate: None,
        truncated: false,
        error: Some(error.into()),
    }
}

fn socket_identity_is_stable(
    home: &Path,
    expected_root: &InspectedEntry,
    expected_socket: &InspectedEntry,
) -> bool {
    let Ok(Some(inspector)) = StateInspector::open_existing(home) else {
        return false;
    };
    if inspector.root().device != expected_root.device
        || inspector.root().inode != expected_root.inode
        || inspector.root().uid != expected_root.uid
    {
        return false;
    }
    let Ok(snapshot) = inspector.inspect_root(
        InspectionLimits::new(STATE_ENTRY_LIMIT, STATE_NAME_BYTES_LIMIT)
            .expect("health limits fit state hard ceilings"),
    ) else {
        return false;
    };
    snapshot.entries.iter().any(|entry| {
        entry.path == expected_socket.path
            && entry.kind == expected_socket.kind
            && entry.device == expected_socket.device
            && entry.inode == expected_socket.inode
            && entry.uid == expected_socket.uid
            && entry.links == expected_socket.links
    })
}

fn inspect_state(home: &Path) -> StateReport {
    let inspector = match StateInspector::open_existing(home) {
        Ok(Some(inspector)) => inspector,
        Ok(None) => {
            return StateReport {
                present: false,
                root: None,
                entries: Vec::new(),
                truncated: false,
                retained_name_bytes: 0,
                error: None,
            }
        }
        Err(error) => {
            return StateReport {
                present: true,
                root: None,
                entries: Vec::new(),
                truncated: false,
                retained_name_bytes: 0,
                error: Some(error.to_string()),
            }
        }
    };

    let mut entries = Vec::with_capacity(256);
    let mut pending = VecDeque::<(InspectedEntry, usize)>::new();
    let mut retained_name_bytes = 0usize;
    let mut truncated = false;
    let root_snapshot = match inspector.inspect_root(
        InspectionLimits::new(STATE_ENTRY_LIMIT, STATE_NAME_BYTES_LIMIT)
            .expect("health limits fit state hard ceilings"),
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return StateReport {
                present: true,
                root: Some(inspector.root().clone()),
                entries,
                truncated: false,
                retained_name_bytes,
                error: Some(error.to_string()),
            }
        }
    };
    retained_name_bytes += root_snapshot.retained_name_bytes;
    truncated |= root_snapshot.truncated;
    for entry in root_snapshot.entries {
        if entry.kind == InspectedKind::Directory {
            pending.push_back((entry.clone(), 1));
        }
        entries.push(entry);
    }

    while let Some((directory_entry, depth)) = pending.pop_front() {
        if entries.len() >= STATE_ENTRY_LIMIT
            || retained_name_bytes >= STATE_NAME_BYTES_LIMIT
            || depth >= STATE_DEPTH_LIMIT
        {
            truncated = true;
            break;
        }
        let remaining_entries = STATE_ENTRY_LIMIT - entries.len();
        let remaining_bytes = STATE_NAME_BYTES_LIMIT - retained_name_bytes;
        let snapshot = match inspector.inspect_bound_directory(
            &directory_entry,
            InspectionLimits::new(remaining_entries, remaining_bytes)
                .expect("remaining health limits fit state hard ceilings"),
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return StateReport {
                    present: true,
                    root: Some(inspector.root().clone()),
                    entries,
                    truncated,
                    retained_name_bytes,
                    error: Some(error.to_string()),
                }
            }
        };
        retained_name_bytes += snapshot.retained_name_bytes;
        truncated |= snapshot.truncated;
        for entry in snapshot.entries {
            if entry.kind == InspectedKind::Directory {
                pending.push_back((entry.clone(), depth + 1));
            }
            entries.push(entry);
        }
    }

    match inspector.path_matches_held_root() {
        Ok(true) => {}
        Ok(false) => {
            return StateReport {
                present: true,
                root: Some(inspector.root().clone()),
                entries,
                truncated,
                retained_name_bytes,
                error: Some("state root changed during read-only inspection".into()),
            }
        }
        Err(error) => {
            return StateReport {
                present: true,
                root: Some(inspector.root().clone()),
                entries,
                truncated,
                retained_name_bytes,
                error: Some(error.to_string()),
            }
        }
    }

    StateReport {
        present: true,
        root: Some(inspector.root().clone()),
        entries,
        truncated,
        retained_name_bytes,
        error: None,
    }
}

fn inspect_setup(cyclops_home: &Path) -> SetupReport {
    let Some(user_home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return SetupReport {
            home_available: false,
            complete: false,
            consumers: Vec::new(),
        };
    };
    let state_inspector = StateInspector::open_existing(cyclops_home);
    let mut consumers = Vec::with_capacity(CONSUMERS.len());
    for spec in CONSUMERS {
        let installed_root = crate::consumer::root(spec.kind, &user_home);
        let (installed, install_state) = match StateInspector::open_existing(&installed_root) {
            Ok(Some(_)) => (true, "present"),
            Ok(None) => (false, "absent"),
            Err(_) => (true, "unproven"),
        };
        let manifest_path = crate::manifests::dir(cyclops_home).join(format!("{}.toml", spec.id));
        let manifest_relative = Path::new("manifests").join(format!("{}.toml", spec.id));
        let manifest_file = match &state_inspector {
            Ok(Some(state)) => read_asset_from(state, &manifest_relative),
            Ok(None) => AssetRead::Missing,
            Err(_) => AssetRead::Unproven,
        };
        let (manifest_state, ack_capable, mailbox_capability) = match manifest_file {
            AssetRead::Missing => ("missing", None, None),
            AssetRead::Unproven => ("unproven", None, None),
            AssetRead::Truncated => ("truncated", None, None),
            AssetRead::Bytes(bytes) => {
                let shipped = crate::manifests::shipped_body(spec.id)
                    .expect("shipped consumer manifest")
                    .as_bytes();
                let state = if bytes == shipped {
                    "current"
                } else if crate::manifests::unedited_seed(&bytes) {
                    "outdated"
                } else {
                    "edited"
                };
                let parsed = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|body| cyclops_manifest::Manifest::parse(body, &manifest_path).ok());
                match parsed {
                    Some(parsed) if parsed.agent.id == spec.id => {
                        let ack = parsed.hooks.ack.is_some();
                        (state, Some(ack), parsed.messaging.mailbox_capability_file)
                    }
                    _ => ("invalid", None, None),
                }
            }
        };
        let (hook_root, hook_relative, hook_path) = setup_hook_path(&user_home, spec.kind);
        let hook_state = if !installed {
            "not_installed"
        } else {
            match read_asset(&hook_root, &hook_relative) {
                AssetRead::Missing => "missing",
                AssetRead::Unproven => "unproven",
                AssetRead::Truncated => "truncated",
                AssetRead::Bytes(bytes) => {
                    crate::hookset::inspect_wiring_bytes(spec.kind, &bytes).word()
                }
            }
        };
        let hook_ready = !installed || hook_state == "current";
        let (skill_root, skill_relative) = setup_skill_location(&user_home, spec.id);
        let skill_path = skill_root.join(&skill_relative);
        let (skill_state, skill_ready) = if installed {
            inspect_skill(read_asset(&skill_root, &skill_relative))
        } else {
            ("not_installed", true)
        };
        let mailbox_transport = mailbox_capability
            .as_deref()
            .and_then(|declared| {
                cyclops_manifest::mailbox_capability::resolve_path(declared, &user_home)
            })
            .map(|declared| {
                if declared == skill_path && skill_state == "current" {
                    "doorbell"
                } else {
                    "direct_payload"
                }
            });
        let manifest_ready = matches!(manifest_state, "current" | "edited");
        let receipt_ready =
            !installed || spec.required_receipt_tier != 1 || ack_capable == Some(true);
        let complete = !installed
            || (install_state == "present"
                && manifest_ready
                && hook_ready
                && skill_ready
                && receipt_ready);
        consumers.push(ConsumerReport {
            id: spec.id,
            name: spec.name,
            installed,
            install_state,
            manifest_path,
            manifest_state,
            ack_capable: installed.then_some(ack_capable).flatten(),
            hook_path: Some(hook_path),
            hook_state,
            skill_path,
            skill_state,
            mailbox_transport: installed.then_some(mailbox_transport).flatten(),
            complete,
        });
    }
    let complete = consumers.iter().all(|consumer| consumer.complete);
    SetupReport {
        home_available: true,
        complete,
        consumers,
    }
}

enum AssetRead {
    Missing,
    Bytes(Vec<u8>),
    Truncated,
    Unproven,
}

fn read_asset(root: &Path, relative: &Path) -> AssetRead {
    match StateInspector::open_existing(root) {
        Ok(Some(inspector)) => read_asset_from(&inspector, relative),
        Ok(None) => AssetRead::Missing,
        Err(_) => AssetRead::Unproven,
    }
}

fn read_asset_from(inspector: &StateInspector, relative: &Path) -> AssetRead {
    match inspector.read_file(relative, FILE_BYTES_LIMIT) {
        Ok(Some(file)) if file.truncated => AssetRead::Truncated,
        Ok(Some(file)) if file.entry.mode & 0o022 != 0 => AssetRead::Unproven,
        Ok(Some(file)) => AssetRead::Bytes(file.bytes),
        Ok(None) => AssetRead::Missing,
        Err(_) => AssetRead::Unproven,
    }
}

fn setup_hook_path(home: &Path, kind: CliKind) -> (PathBuf, PathBuf, PathBuf) {
    let (root, relative) = match kind {
        CliKind::Claude => (home.join(".claude"), PathBuf::from("settings.json")),
        CliKind::Codex => (
            crate::consumer::root(kind, home),
            PathBuf::from("hooks.json"),
        ),
        CliKind::Cursor => (home.join(".cursor"), PathBuf::from("hooks.json")),
        CliKind::Agy => (home.join(".agents"), PathBuf::from("hooks.json")),
    };
    let path = root.join(&relative);
    (root, relative, path)
}

fn setup_skill_location(home: &Path, id: &str) -> (PathBuf, PathBuf) {
    match id {
        "claude" => (
            home.join(".claude"),
            PathBuf::from("skills/cyclops/SKILL.md"),
        ),
        "codex" | "cursor" => (
            home.join(".agents"),
            PathBuf::from("skills/cyclops/SKILL.md"),
        ),
        "agy" => (
            home.join(".gemini/antigravity-cli"),
            PathBuf::from("skills/cyclops/SKILL.md"),
        ),
        _ => unreachable!("shipped consumer id"),
    }
}

fn inspect_skill(asset: AssetRead) -> (&'static str, bool) {
    match asset {
        AssetRead::Missing => ("missing", false),
        AssetRead::Truncated => ("truncated", false),
        AssetRead::Unproven => ("unproven", false),
        AssetRead::Bytes(body) if body == crate::skillseed::SHIPPED.as_bytes() => ("current", true),
        AssetRead::Bytes(body) if crate::skillseed::unedited_seed(&body) => ("outdated", false),
        AssetRead::Bytes(_) => ("edited", true),
    }
}

fn inspect_operational_state(home: &Path) -> (ExternalStateReport, ExternalStateReport) {
    let inventory = crate::cleanup::inspect_operational_assets(home);
    let cache_path = crate::update::build_cache(home);
    let mut build_candidates = Vec::new();
    let mut update_candidates = Vec::new();
    for candidate in inventory.candidates {
        let report = ExternalCandidateReport {
            class: candidate.class.name(),
            path: candidate.path,
            safe: candidate.safe,
            truncated: candidate.truncated,
            entries: candidate.entries,
            bytes: candidate.bytes,
            marker: candidate.marker,
            lease: candidate.lease,
            error: candidate.error,
        };
        if report.class == "build_cache" {
            build_candidates.push(report);
        } else {
            update_candidates.push(report);
        }
    }
    let root_error = inventory.error;
    let build_cache = summarize_external(
        cache_path,
        build_candidates,
        inventory.root_safe,
        inventory.truncated,
        root_error.clone(),
    );
    let update_scratch = summarize_external(
        inventory.temp_root,
        update_candidates,
        inventory.root_safe,
        inventory.truncated,
        root_error,
    );
    (build_cache, update_scratch)
}

fn summarize_external(
    path: PathBuf,
    candidates: Vec<ExternalCandidateReport>,
    root_safe: bool,
    root_truncated: bool,
    root_error: Option<String>,
) -> ExternalStateReport {
    let truncated = root_truncated || candidates.iter().any(|candidate| candidate.truncated);
    let presence = if !candidates.is_empty() {
        ExternalPresence::Present
    } else if !root_safe || truncated || root_error.is_some() {
        ExternalPresence::Unproven
    } else {
        ExternalPresence::Absent
    };
    let safe = (presence == ExternalPresence::Present).then(|| {
        root_safe
            && !truncated
            && root_error.is_none()
            && candidates.iter().all(|candidate| candidate.safe)
    });
    let entries = candidates.iter().map(|candidate| candidate.entries).sum();
    let error = root_error.or_else(|| {
        candidates
            .iter()
            .find_map(|candidate| candidate.error.clone())
    });
    let error = if presence == ExternalPresence::Unproven && error.is_none() {
        Some("operational inventory root is not safely proven".into())
    } else {
        error
    };
    ExternalStateReport {
        path,
        presence,
        safe,
        truncated,
        entries,
        error,
        candidates,
    }
}

fn kind_word(kind: InspectedKind) -> &'static str {
    match kind {
        InspectedKind::Directory => "directory",
        InspectedKind::RegularFile => "file",
        InspectedKind::Socket => "socket",
        InspectedKind::Symlink => "symlink",
        InspectedKind::Other => "other",
    }
}

fn entry_json(entry: &InspectedEntry) -> Value {
    json!({
        "path": entry.path.display().to_string(),
        "kind": kind_word(entry.kind),
        "mode": format!("{:04o}", entry.mode),
        "uid": entry.uid,
        "links": entry.links,
        "size": entry.size,
        "device": entry.device,
        "inode": entry.inode,
        "safe": entry.safe(),
        "unsafe_reason": entry.unsafe_reason,
    })
}

fn external_json(report: &ExternalStateReport) -> Value {
    json!({
        "path": report.path.display().to_string(),
        "state": report.presence.word(),
        "present": report.presence.as_optional_bool(),
        "safe": report.safe,
        "entries": report.entries,
        "truncated": report.truncated,
        "error": report.error.as_deref(),
        "candidates": report.candidates.iter().map(|candidate| json!({
            "class": candidate.class,
            "path": candidate.path.display().to_string(),
            "safe": candidate.safe,
            "truncated": candidate.truncated,
            "entries": candidate.entries,
            "bytes": candidate.bytes,
            "marker": candidate.marker,
            "lease": candidate.lease,
            "error": candidate.error.as_deref(),
        })).collect::<Vec<_>>(),
    })
}

fn operational_json(report: &OperationalReport) -> Value {
    json!({
        "workspace_mapping": {
            "state": report.workspace.state,
            "daemon": report.workspace.daemon.map(|id| id.to_string()),
            "recorded": report.workspace.recorded.map(|id| id.to_string()),
            "error": report.workspace.error.as_deref(),
        },
        "session_mappings": report.sessions.iter().map(|session| {
            let binding = session.binding.as_ref();
            let live = binding.map(SessionIdentityBinding::live_session_key);
            json!({
                "name": session.name.as_str(),
                "attached": session.attached,
                "configured": session.configured,
                "state": session.state,
                "session_instance_id": binding.map(SessionIdentityBinding::session_instance_id).map(|id| id.to_string()),
                "workspace_id": live.map(|key| key.workspace_id().to_string()),
                "os_boot_id": live.map(|key| key.os_boot_id().to_string()),
                "tmux_server": live.map(|key| json!({
                    "pid": key.tmux_server().pid(),
                    "birth": key.tmux_server().birth(),
                })),
                "tmux_session_id": live.map(|key| key.tmux_session_id().to_string()),
            })
        }).collect::<Vec<_>>(),
        "session_mapping_record_error": report.session_record_error.as_deref(),
        "session_config": {
            "state": report.config.state,
            "configured": &report.config.configured,
            "stale": &report.config.stale,
            "dynamic": &report.config.dynamic,
            "duplicates": &report.config.duplicates,
            "error": report.config.error.as_deref(),
        },
        "watchers": {
            "state": report.watchers.state,
            "slots": report.watchers.slots,
            "duplicate_names": &report.watchers.duplicate_names,
        },
        "daemon_processes": {
            "state": report.daemons.state,
            "duplicate": report.daemons.duplicate,
            "truncated": report.daemons.truncated,
            "error": report.daemons.error.as_deref(),
            "instances": report.daemons.processes.iter().map(|process| json!({
                "pid": process.process.pid(),
                "birth": process.process.birth(),
                "command": process.command.as_str(),
                "selected": process.selected,
            })).collect::<Vec<_>>(),
        },
    })
}

fn report_json(report: &HealthReport) -> Value {
    let state_files = report
        .state
        .entries
        .iter()
        .filter(|entry| entry.kind == InspectedKind::RegularFile)
        .count();
    let state_directories = report
        .state
        .entries
        .iter()
        .filter(|entry| entry.kind == InspectedKind::Directory)
        .count();
    let journals = report
        .state
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == InspectedKind::RegularFile
                && entry.path.extension().and_then(OsStr::to_str) == Some("ndjson")
        })
        .count();
    let logs = report
        .state
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == InspectedKind::RegularFile
                && entry.path.extension().and_then(OsStr::to_str) == Some("log")
        })
        .count();
    json!({
        "schema": 1,
        "healthy": report.issues.is_empty(),
        "client": {
            "version": crate::VERSION,
            "build": crate::BUILD_REF,
            "selected_executable": report.binaries.selected_client.display().to_string(),
            "selected_resolved": report.binaries.selected_resolved.as_ref().map(|path| path.display().to_string()),
            "selected_daemon": report.binaries.selected_daemon.display().to_string(),
            "selected_daemon_resolved": report.binaries.selected_daemon_resolved.as_ref().map(|path| path.display().to_string()),
            "selected_daemon_ready": report.binaries.selected_daemon_ready,
            "selected_daemon_build": report.binaries.selected_daemon_build.as_deref(),
            "selected_daemon_build_error": report.binaries.selected_daemon_build_error.as_deref(),
            "path": {
                "entries_examined": report.binaries.path_entries,
                "name_bytes_examined": report.binaries.path_bytes,
                "truncated": report.binaries.path_truncated,
            },
            "shadowed": report.binaries.shadowed,
            "resolutions": report.binaries.resolutions.iter().map(|entry| json!({
                "name": entry.name,
                "path": entry.path.display().to_string(),
                "resolved": entry.resolved.as_ref().map(|path| path.display().to_string()),
                "executable": entry.executable,
                "path_index": entry.path_index,
                "selected": entry.selected,
            })).collect::<Vec<_>>(),
        },
        "daemon": {
            "state": if report.daemon.running { "running" } else if report.daemon.stale_socket { "stale_socket" } else if report.daemon.transport_error.is_some() { "unproven" } else { "stopped" },
            "running": report.daemon.running,
            "stale_socket": report.daemon.stale_socket,
            "authenticated_socket": report.daemon.authenticated_socket,
            "version": report.daemon.version.as_deref(),
            "build": report.daemon.build.as_deref(),
            "client_build_matches": report.daemon.build_matches_client,
            "boot_id": report.daemon.boot_id.as_deref(),
            "pid": report.daemon.process.map(ProcessInstanceId::pid),
            "process": {
                "state": if report.daemon.process.is_some() { "proven" } else { "unproven" },
                "pid": report.daemon.process.map(ProcessInstanceId::pid),
                "birth": report.daemon.process.map(ProcessInstanceId::birth),
            },
            "uptime_ms": report.daemon.uptime_ms,
            "executable": {
                "state": if report.daemon.executable.is_some() { "proven" } else { "unproven" },
                "path": report.daemon.executable.as_ref().map(|path| path.display().to_string()),
                "reason": report.daemon.executable.is_none().then_some("the authenticated daemon did not report one stable absolute path"),
            },
            "transport_error": report.daemon.transport_error.as_deref(),
            "status_error": report.daemon.status_error.as_deref(),
        },
        "operational": operational_json(&report.operational),
        "state": {
            "root": cyclops_proto::cyclops_home().display().to_string(),
            "socket": cyclops_proto::socket_path().display().to_string(),
            "present": report.state.present,
            "root_metadata": report.state.root.as_ref().map(entry_json),
            "inventory": {
                "entries": report.state.entries.iter().map(entry_json).collect::<Vec<_>>(),
                "directories": state_directories,
                "files": state_files,
                "journals": journals,
                "logs": logs,
                "retained_name_bytes": report.state.retained_name_bytes,
                "truncated": report.state.truncated,
                "error": report.state.error.as_deref(),
            },
        },
        "setup": {
            "home_available": report.setup.home_available,
            "complete": report.setup.complete,
            "consumers": report.setup.consumers.iter().map(|consumer| json!({
                "id": consumer.id,
                "name": consumer.name,
                "installed": consumer.installed,
                "install_state": consumer.install_state,
                "complete": consumer.complete,
                "manifest": {
                    "path": consumer.manifest_path.display().to_string(),
                    "state": consumer.manifest_state,
                    "ack_capable": consumer.ack_capable,
                },
                "hooks": {
                    "path": consumer.hook_path.as_ref().map(|path| path.display().to_string()),
                    "state": consumer.hook_state,
                },
                "skill": {
                    "path": consumer.skill_path.display().to_string(),
                    "state": consumer.skill_state,
                },
                "mailbox_transport": consumer.mailbox_transport,
            })).collect::<Vec<_>>(),
        },
        "build_cache": external_json(&report.build_cache),
        "update_scratch": external_json(&report.update_scratch),
        "rollback": rollback_json(&report.rollback),
        "limits": {
            "path_entries": PATH_ENTRY_LIMIT,
            "path_bytes": PATH_BYTES_LIMIT,
            "state_entries": STATE_ENTRY_LIMIT,
            "state_name_bytes": STATE_NAME_BYTES_LIMIT,
            "state_depth": STATE_DEPTH_LIMIT,
            "file_bytes": FILE_BYTES_LIMIT,
            "process_bytes": PROCESS_BYTES_LIMIT,
            "inspection_path_components": cyclops_state::INSPECTION_PATH_COMPONENT_LIMIT_MAX,
            "inspection_path_bytes": cyclops_state::INSPECTION_PATH_BYTES_LIMIT_MAX,
            "operational_asset_entries": crate::cleanup::ENTRY_LIMIT,
            "operational_asset_name_bytes": crate::cleanup::NAME_BYTES_LIMIT,
            "operational_asset_depth": crate::cleanup::DEPTH_LIMIT,
        },
        "issues": report.issues.iter().map(|issue| json!({
            "code": issue.code,
            "message": issue.message.as_str(),
            "path": issue.path.as_ref().map(|path| path.display().to_string()),
        })).collect::<Vec<_>>(),
    })
}

fn render_plain(report: &HealthReport, style: &Style) -> String {
    let mut lines = Vec::new();
    let heading = if report.issues.is_empty() {
        "health inspection complete"
    } else {
        "health found problems"
    };
    lines.push(style.bold(heading));
    lines.push(format!(
        "  client   {} · build {}",
        report.binaries.selected_client.display(),
        crate::BUILD_REF
    ));
    if let Some(resolved) = &report.binaries.selected_resolved {
        if resolved != &report.binaries.selected_client {
            lines.push(format!("    resolved {}", resolved.display()));
        }
    }
    for resolution in &report.binaries.resolutions {
        let selected = if resolution.selected {
            " · selected"
        } else {
            ""
        };
        let shadow = if report.binaries.shadowed && !resolution.selected {
            " · shadow candidate"
        } else {
            ""
        };
        let resolved = resolution
            .resolved
            .as_ref()
            .filter(|resolved| *resolved != &resolution.path)
            .map(|resolved| format!(" -> {}", resolved.display()))
            .unwrap_or_default();
        lines.push(format!(
            "    {:8} {}{}{}{}",
            resolution.name,
            resolution.path.display(),
            resolved,
            selected,
            shadow
        ));
    }
    if report.binaries.path_truncated {
        lines.push("    PATH inventory truncated at its fixed limit".into());
    }
    lines.push(format!(
        "    paired daemon {} · {} · build {}",
        report.binaries.selected_daemon.display(),
        if report.binaries.selected_daemon_ready {
            "executable"
        } else {
            "missing or not executable"
        },
        report
            .binaries
            .selected_daemon_build
            .as_deref()
            .unwrap_or("unproven")
    ));
    if let Some(error) = &report.binaries.selected_daemon_build_error {
        lines.push(format!("    adjacent daemon build unavailable · {error}"));
    }
    if report.daemon.running {
        lines.push(format!(
            "  daemon   running · pid {} · version {} · build {} · boot {}",
            report
                .daemon
                .process
                .map(|process| format!("{} birth {}", process.pid(), process.birth()))
                .unwrap_or_else(|| "unreported".into()),
            report.daemon.version.as_deref().unwrap_or("unreported"),
            report.daemon.build.as_deref().unwrap_or("unreported"),
            report.daemon.boot_id.as_deref().unwrap_or("unreported")
        ));
        lines.push(format!(
            "    client build match {}",
            match report.daemon.build_matches_client {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unproven",
            }
        ));
        lines.push(format!(
            "    named socket {}",
            if report.daemon.authenticated_socket {
                "authenticated"
            } else {
                "unproven"
            }
        ));
    } else {
        lines.push(
            if report.daemon.stale_socket {
                "  daemon   stale socket"
            } else if report.daemon.transport_error.is_some() {
                "  daemon   unproven"
            } else {
                "  daemon   stopped"
            }
            .into(),
        );
    }
    lines.push(match report.daemon.executable.as_ref() {
        Some(path) => format!("    executable {}", path.display()),
        None => "    executable unproven · daemon did not report one stable absolute path".into(),
    });
    if let Some(error) = &report.daemon.transport_error {
        lines.push(format!("    identity unavailable · {error}"));
    }
    if let Some(error) = &report.daemon.status_error {
        lines.push(format!("    status unavailable · {error}"));
    }
    lines.push(format!(
        "    daemon processes {} · duplicate {} · {} live",
        report.operational.daemons.state,
        match report.operational.daemons.duplicate {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unproven",
        },
        report.operational.daemons.processes.len()
    ));
    for process in &report.operational.daemons.processes {
        lines.push(format!(
            "      pid {} birth {} · {}{}",
            process.process.pid(),
            process.process.birth(),
            process.command,
            if process.selected { " · selected" } else { "" }
        ));
    }
    if let Some(error) = &report.operational.daemons.error {
        lines.push(format!("      process inventory unavailable · {error}"));
    }
    lines.push(format!(
        "  mapping  workspace {} · daemon {} · recorded {}",
        report.operational.workspace.state,
        report
            .operational
            .workspace
            .daemon
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unproven".into()),
        report
            .operational
            .workspace
            .recorded
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unproven".into())
    ));
    for session in &report.operational.sessions {
        lines.push(format!(
            "    session {} · {} · mapping {} · {}",
            session.name,
            match session.attached {
                Some(true) => "attached",
                Some(false) => "detached",
                None => "runtime unproven",
            },
            session.state,
            match session.configured {
                Some(true) => "configured",
                Some(false) => "dynamic",
                None => "configuration unproven",
            }
        ));
    }
    if let Some(error) = &report.operational.session_record_error {
        lines.push(format!("    session mapping record invalid · {error}"));
    }
    lines.push(format!(
        "    config {} · {} configured · {} stale · {} dynamic",
        report.operational.config.state,
        report.operational.config.configured.len(),
        report.operational.config.stale.len(),
        report.operational.config.dynamic.len()
    ));
    if !report.operational.config.stale.is_empty() {
        lines.push(format!(
            "      stale {}",
            report.operational.config.stale.join(", ")
        ));
    }
    lines.push(format!(
        "    watchers {} · {} slots{}",
        report.operational.watchers.state,
        report.operational.watchers.slots,
        if report.operational.watchers.duplicate_names.is_empty() {
            String::new()
        } else {
            format!(
                " · duplicate {}",
                report.operational.watchers.duplicate_names.join(", ")
            )
        }
    ));
    lines.push(format!(
        "  state    {} · {}",
        cyclops_proto::cyclops_home().display(),
        if report.state.present {
            "present"
        } else {
            "absent"
        }
    ));
    lines.push(format!(
        "    socket {}",
        cyclops_proto::socket_path().display()
    ));
    if report.state.present {
        let unsafe_count = report
            .state
            .root
            .iter()
            .chain(report.state.entries.iter())
            .filter(|entry| !entry.safe())
            .count();
        lines.push(format!(
            "    {} entries · {} unsafe · {} name bytes{}",
            report.state.entries.len(),
            unsafe_count,
            report.state.retained_name_bytes,
            if report.state.truncated {
                " · truncated"
            } else {
                ""
            }
        ));
        let journals = report
            .state
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == InspectedKind::RegularFile
                    && entry.path.extension().and_then(OsStr::to_str) == Some("ndjson")
            })
            .count();
        let logs = report
            .state
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == InspectedKind::RegularFile
                    && entry.path.extension().and_then(OsStr::to_str) == Some("log")
            })
            .count();
        lines.push(format!("    {journals} journals · {logs} logs"));
    }
    lines.push(format!(
        "  setup    {}",
        if report.setup.complete {
            "complete"
        } else {
            "incomplete"
        }
    ));
    for consumer in &report.setup.consumers {
        lines.push(format!(
            "    {:16} install {} · manifest {} · hooks {} · skill {}",
            consumer.name,
            consumer.install_state,
            consumer.manifest_state,
            consumer.hook_state,
            consumer.skill_state
        ));
    }
    lines.push(format!(
        "  cache    {} · {}",
        report.build_cache.path.display(),
        external_plain_state(&report.build_cache)
    ));
    if let Some(error) = &report.build_cache.error {
        lines.push(format!("    cache inspection · {error}"));
    }
    for candidate in &report.build_cache.candidates {
        lines.push(format!(
            "    {} · safe {} · marker {} · lease {} · {} entries · {} bytes",
            candidate.path.display(),
            if candidate.safe { "yes" } else { "no" },
            candidate.marker,
            candidate.lease,
            candidate.entries,
            candidate.bytes
        ));
    }
    lines.push(format!(
        "  update   {} · {} · {} scratch entries{}",
        report.update_scratch.path.display(),
        external_plain_state(&report.update_scratch),
        report.update_scratch.entries,
        if report.update_scratch.truncated {
            " · inventory truncated"
        } else {
            ""
        }
    ));
    if let Some(error) = &report.update_scratch.error {
        lines.push(format!("    update inspection · {error}"));
    }
    for candidate in &report.update_scratch.candidates {
        lines.push(format!(
            "    {} · safe {} · marker {} · lease {} · {} entries · {} bytes",
            candidate.path.display(),
            if candidate.safe { "yes" } else { "no" },
            candidate.marker,
            candidate.lease,
            candidate.entries,
            candidate.bytes
        ));
    }
    lines.extend(render_rollback_plain(&report.rollback));
    if !report.issues.is_empty() {
        lines.push(String::new());
        lines.push("Problems:".into());
        for issue in &report.issues {
            let path = issue
                .path
                .as_ref()
                .map(|path| format!(" · {}", path.display()))
                .unwrap_or_default();
            lines.push(format!("  {} · {}{path}", issue.code, issue.message));
        }
    }
    lines.join("\n")
}

fn external_plain_state(report: &ExternalStateReport) -> &'static str {
    match (report.presence, report.safe) {
        (ExternalPresence::Absent, _) => "absent",
        (ExternalPresence::Unproven, _) => "unproven",
        (ExternalPresence::Present, Some(true)) => "present and owner-only",
        (ExternalPresence::Present, Some(false)) => "present and unsafe",
        (ExternalPresence::Present, None) => "present and unproven",
    }
}

fn rollback_json(report: &RollbackReport) -> Value {
    json!({
        "state": report.state,
        "prefix": report.prefix.as_ref().map(|path| path.display().to_string()),
        "selection": report.selection.as_ref().map(|path| path.display().to_string()),
        "active": {
            "pair": report.active_pair.as_ref().map(|path| path.display().to_string()),
            "identity": report.active_identity.as_deref(),
            "build": report.active_build.as_deref(),
            "install_replay_attested": report.active_install_replay,
        },
        "known_good": {
            "pair": report.known_good_pair.as_ref().map(|path| path.display().to_string()),
            "identity": report.known_good_identity.as_deref(),
            "build": report.known_good_build.as_deref(),
            "install_replay_attested": report.known_good_install_replay,
            "replay_snapshot_sha256": report.known_good_replay_snapshot.as_deref(),
        },
        "candidate_available": report.candidate_available,
        "install_replay": report.install_replay,
        "current_replay": report.journal_replay,
        "journal_replay": report.journal_replay,
        "rollback_safe": report.rollback_safe,
        "reason": report.reason.as_str(),
        "error": report.error.as_deref(),
    })
}

fn render_rollback_plain(report: &RollbackReport) -> Vec<String> {
    let safe = match report.rollback_safe {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unproven",
    };
    let mut lines = vec![format!(
        "  rollback {} · candidate {} · install replay {} · current replay {} · safe {} · {}",
        report.state,
        match report.candidate_available {
            Some(true) => "available",
            Some(false) => "unavailable",
            None => "unproven",
        },
        report.install_replay,
        report.journal_replay,
        safe,
        report.reason
    )];
    if let Some(prefix) = &report.prefix {
        lines.push(format!("    prefix {}", prefix.display()));
    }
    if let Some(selection) = &report.selection {
        lines.push(format!("    selection {}", selection.display()));
    }
    if let Some(pair) = &report.active_pair {
        lines.push(format!(
            "    active {} · identity {} · build {} · install replay {}",
            pair.display(),
            report.active_identity.as_deref().unwrap_or("unproven"),
            report.active_build.as_deref().unwrap_or("unproven"),
            match report.active_install_replay {
                Some(true) => "attested",
                Some(false) => "unproven",
                None => "unproven",
            }
        ));
    }
    if let Some(pair) = &report.known_good_pair {
        lines.push(format!(
            "    known good {} · identity {} · build {} · install replay {}",
            pair.display(),
            report.known_good_identity.as_deref().unwrap_or("unproven"),
            report.known_good_build.as_deref().unwrap_or("unproven"),
            match report.known_good_install_replay {
                Some(true) => "attested",
                Some(false) => "unproven",
                None => "unproven",
            }
        ));
    }
    if let Some(error) = &report.error {
        lines.push(format!("    proof error {error}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn private_scratch() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("cyc-health-")
            .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .unwrap()
    }

    fn binaries_with_public_selector(public: bool) -> BinaryReport {
        BinaryReport {
            selected_client: PathBuf::from("/prefix/.cyclops-pairs/pairs/pair.a/cyclops"),
            selected_resolved: Some(PathBuf::from("/prefix/.cyclops-pairs/pairs/pair.a/cyclops")),
            selected_daemon: PathBuf::from("/prefix/bin/cyclopsd"),
            selected_daemon_resolved: Some(PathBuf::from(
                "/prefix/.cyclops-pairs/pairs/pair.a/cyclopsd",
            )),
            selected_daemon_ready: true,
            selected_daemon_build: Some(crate::BUILD_REF.into()),
            selected_daemon_build_error: None,
            resolutions: vec![BinaryResolution {
                name: "cyclops",
                path: PathBuf::from("/prefix/bin/cyclops"),
                resolved: Some(PathBuf::from("/prefix/.cyclops-pairs/pairs/pair.a/cyclops")),
                executable: true,
                path_index: public.then_some(0),
                selected: true,
            }],
            path_truncated: false,
            path_entries: usize::from(public),
            path_bytes: 0,
            shadowed: false,
        }
    }

    fn descriptor(legacy: bool, rollback_safe: bool) -> crate::update::InstalledPairDescriptor {
        crate::update::InstalledPairDescriptor {
            selection: PathBuf::from("/prefix/bin/.cyclops-pairs/selections/selection.a"),
            active_pair: PathBuf::from("/prefix/bin/.cyclops-pairs/pairs/pair.a"),
            known_good_pair: PathBuf::from("/prefix/bin/.cyclops-pairs/pairs/pair.b"),
            active_identity: (!legacy).then(|| "0.1.0 (active-build)".into()),
            known_good_identity: Some("0.1.0 (known-build)".into()),
            active_build: (!legacy).then(|| "active-build".into()),
            known_good_build: Some("known-build".into()),
            active_replay_attested: false,
            known_good_replay_attested: false,
            known_good_replay_snapshot: None,
            proof_unproven: legacy,
            rollback_safe,
        }
    }

    #[test]
    fn kind_words_are_stable_for_json() {
        assert_eq!(kind_word(InspectedKind::Directory), "directory");
        assert_eq!(kind_word(InspectedKind::RegularFile), "file");
        assert_eq!(kind_word(InspectedKind::Socket), "socket");
        assert_eq!(kind_word(InspectedKind::Symlink), "symlink");
        assert_eq!(kind_word(InspectedKind::Other), "other");
    }

    #[test]
    fn incomplete_external_inventory_never_reports_absent() {
        for report in [
            summarize_external(
                PathBuf::from("/cache"),
                Vec::new(),
                false,
                false,
                Some("inventory failed".into()),
            ),
            summarize_external(PathBuf::from("/cache"), Vec::new(), true, true, None),
        ] {
            assert_eq!(report.presence, ExternalPresence::Unproven);
            assert_eq!(report.presence.as_optional_bool(), None);
            assert_eq!(external_plain_state(&report), "unproven");
            let json = external_json(&report);
            assert_eq!(json["state"], "unproven");
            assert!(json["present"].is_null());
        }

        let absent = summarize_external(PathBuf::from("/cache"), Vec::new(), true, false, None);
        assert_eq!(absent.presence, ExternalPresence::Absent);
        assert_eq!(absent.presence.as_optional_bool(), Some(false));
        assert_eq!(external_plain_state(&absent), "absent");
    }

    #[test]
    fn replacing_the_named_socket_after_hello_revokes_endpoint_authentication() {
        let temp = tempfile::Builder::new()
            .prefix("cyc-health-")
            .tempdir_in(std::fs::canonicalize("/tmp").unwrap())
            .unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let home = base.join("state");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = home.join("sock");
        let original_listener = UnixListener::bind(&socket).unwrap();
        let state = inspect_state(&home);
        let root = state.root.as_ref().unwrap();
        let expected_socket = state
            .entries
            .iter()
            .find(|entry| entry.path == socket)
            .unwrap();
        assert!(socket_identity_is_stable(&home, root, expected_socket));

        let hello = Hello {
            cyclops: "0.1.0".into(),
            build: Some(crate::BUILD_REF.into()),
            daemon_process: Some(ProcessInstanceId::new(4242, 818221).unwrap()),
            daemon_executable: Some("/opt/cyclops/bin/cyclopsd".into()),
            proto: 1,
            boot_id: "health-boot".into(),
        };
        std::fs::rename(&socket, home.join("old.sock")).unwrap();
        let replacement_listener = UnixListener::bind(&socket).unwrap();
        let report = daemon_from_hello(
            hello,
            socket_identity_is_stable(&home, root, expected_socket),
            None,
            None,
        );

        assert!(report.running, "the connected peer answered hello");
        assert!(!report.authenticated_socket);
        assert_eq!(
            report.transport_error.as_deref(),
            Some("state root or socket changed during daemon inspection")
        );
        drop((replacement_listener, original_listener));
    }

    #[test]
    fn operational_health_compares_runtime_and_durable_session_identity() {
        let scratch = private_scratch();
        let home = scratch.path().join("state");
        let root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        let workspace: WorkspaceId = "11111111-1111-4111-8111-111111111111".parse().unwrap();
        let session_id = "22222222-2222-4222-8222-222222222222".parse().unwrap();
        let binding = SessionIdentityBinding::new(
            cyclops_proto::LiveSessionKey::new(
                workspace,
                cyclops_proto::OsBootId::new("boot-test").unwrap(),
                ProcessInstanceId::new(81, 9001).unwrap(),
                "$7".parse().unwrap(),
            ),
            session_id,
        );
        root.replace_file(
            Path::new("identity/workspace-id"),
            format!("{workspace}\n").as_bytes(),
        )
        .unwrap();
        root.replace_file(
            Path::new("identity/sessions.ndjson"),
            format!("{}\n", serde_json::to_string(&binding).unwrap()).as_bytes(),
        )
        .unwrap();
        root.replace_file(
            Path::new("config.toml"),
            b"sessions = [\"main\", \"gone\", \"main\"]\n",
        )
        .unwrap();
        let status: StatusResult = serde_json::from_value(json!({
            "daemon_version": "0.1.0",
            "proto": 1,
            "boot_id": "daemon-boot",
            "uptime_ms": 1,
            "tmux_version": "3.6a",
            "workspace_id": workspace,
            "sessions": [{
                "name": "main",
                "attached": true,
                "identity": binding,
                "panes": [],
            }],
        }))
        .unwrap();
        let mut daemon = daemon_stopped();
        daemon.running = true;
        daemon.status = Some(status);

        let report = inspect_operational(&home, &daemon);
        assert_eq!(report.workspace.state, "current");
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].state, "current");
        assert_eq!(report.sessions[0].attached, Some(true));
        assert_eq!(report.sessions[0].configured, Some(true));
        assert_eq!(report.config.state, "stale");
        assert_eq!(report.config.stale, vec!["gone"]);
        assert_eq!(report.config.duplicates, vec!["main"]);
        assert_eq!(report.watchers.state, "current");
    }

    #[test]
    fn stopped_health_keeps_durable_identity_visible_without_inventing_runtime_state() {
        let scratch = private_scratch();
        let home = scratch.path().join("state");
        let root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        let workspace: WorkspaceId = "11111111-1111-4111-8111-111111111111".parse().unwrap();
        let binding = SessionIdentityBinding::new(
            cyclops_proto::LiveSessionKey::new(
                workspace,
                cyclops_proto::OsBootId::new("boot-test").unwrap(),
                ProcessInstanceId::new(81, 9001).unwrap(),
                "$7".parse().unwrap(),
            ),
            "22222222-2222-4222-8222-222222222222".parse().unwrap(),
        );
        root.replace_file(
            Path::new("identity/workspace-id"),
            format!("{workspace}\n").as_bytes(),
        )
        .unwrap();
        root.replace_file(
            Path::new("identity/sessions.ndjson"),
            format!("{}\n", serde_json::to_string(&binding).unwrap()).as_bytes(),
        )
        .unwrap();

        let report = inspect_operational(&home, &daemon_stopped());

        assert_eq!(report.workspace.state, "unproven");
        assert_eq!(report.workspace.recorded, Some(workspace));
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].name, "$7");
        assert_eq!(report.sessions[0].state, "runtime_unproven");
        assert_eq!(report.sessions[0].attached, None);
        assert_eq!(report.sessions[0].configured, None);
        assert_eq!(report.sessions[0].binding.as_ref(), Some(&binding));
        let json = operational_json(&report);
        assert_eq!(json["session_mappings"][0]["state"], "runtime_unproven");
        assert!(json["session_mappings"][0]["attached"].is_null());
    }

    #[test]
    fn malformed_workspace_identity_is_invalid_even_with_the_daemon_stopped() {
        let scratch = private_scratch();
        let home = scratch.path().join("state");
        let root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        root.replace_file(Path::new("identity/workspace-id"), b"not-a-workspace\n")
            .unwrap();

        let report = inspect_operational(&home, &daemon_stopped());

        assert_eq!(report.workspace.state, "invalid");
        assert_eq!(
            report.workspace.error.as_deref(),
            Some("identity/workspace-id is invalid")
        );
    }

    #[test]
    fn process_inventory_names_duplicate_live_daemons_without_path_guessing() {
        let selected = ProcessInstanceId::new(41, 101).unwrap();
        let report = parse_daemon_processes_with(
            "41 501 /opt/cyclopsd\n42 501 cyclopsd\n43 501 other\n44 502 cyclopsd\n",
            501,
            Some(selected),
            |pid| match pid {
                41 => Some(selected),
                42 => ProcessInstanceId::new(42, 102).ok(),
                _ => None,
            },
        );

        assert_eq!(report.state, "proven");
        assert_eq!(report.duplicate, Some(true));
        assert_eq!(report.processes.len(), 2);
        assert!(report.processes[0].selected);
        assert!(!report.processes[1].selected);
    }

    #[test]
    fn path_inventory_is_bounded_before_candidate_lookups() {
        let path = std::iter::repeat_n("x", PATH_ENTRY_LIMIT + 20)
            .collect::<Vec<_>>()
            .join(":");
        let report = inspect_binaries_from(Some(OsStr::new(&path)), PathBuf::from("/client"));
        assert_eq!(report.path_entries, PATH_ENTRY_LIMIT);
        assert!(report.path_truncated);
    }

    #[test]
    fn direct_invocation_without_a_public_selector_keeps_rollback_unproven() {
        let binaries = binaries_with_public_selector(false);
        let report = inspect_rollback_with(&binaries, |_| {
            panic!("direct invocation must not inspect the immutable pair directory")
        });
        assert_eq!(report.state, "unproven");
        assert_eq!(report.prefix, None);
        assert_eq!(report.rollback_safe, None);
        assert_eq!(report.candidate_available, None);
    }

    #[test]
    fn a_public_legacy_install_without_a_descriptor_is_explicitly_unproven() {
        let binaries = binaries_with_public_selector(true);
        let report = inspect_rollback_with(&binaries, |_| Ok(None));
        assert_eq!(report.state, "unproven");
        assert_eq!(report.prefix, Some(PathBuf::from("/prefix/bin")));
        assert!(report.reason.contains("no managed rollback descriptor"));
    }

    #[test]
    fn a_concurrent_pair_change_is_busy_without_being_called_corruption() {
        let binaries = binaries_with_public_selector(true);
        let report = inspect_rollback_with(&binaries, |_| {
            Err(
                crate::update::InstalledPairInspectionError::ConcurrentChange(
                    "pair selection changed".into(),
                ),
            )
        });

        assert_eq!(report.state, "concurrent_change");
        assert_eq!(report.candidate_available, None);
        assert_eq!(report.error.as_deref(), Some("pair selection changed"));
        assert!(!report.reason.contains("unsafe"));
    }

    #[test]
    fn a_legacy_selection_reports_its_known_good_proof_without_claiming_safety() {
        let binaries = binaries_with_public_selector(true);
        let report = inspect_rollback_with(&binaries, |prefix| {
            assert_eq!(prefix, Path::new("/prefix/bin"));
            Ok(Some(descriptor(true, false)))
        });
        assert_eq!(report.state, "unproven");
        assert_eq!(report.active_identity, None);
        assert_eq!(report.known_good_build.as_deref(), Some("known-build"));
        assert_eq!(report.candidate_available, Some(false));
        assert_eq!(report.rollback_safe, None);
    }

    #[test]
    fn a_valid_distinct_pair_reports_a_candidate_without_claiming_replay_safety() {
        let binaries = binaries_with_public_selector(true);
        let report = inspect_rollback_with(&binaries, |_| Ok(Some(descriptor(false, true))));
        assert_eq!(report.state, "candidate");
        assert_eq!(report.candidate_available, Some(true));
        assert_eq!(report.journal_replay, "unproven");
        assert_eq!(report.install_replay, "unproven");
        assert_eq!(report.rollback_safe, None);
        assert_eq!(report.active_build.as_deref(), Some("active-build"));
        assert_eq!(report.known_good_build.as_deref(), Some("known-build"));
        let json = rollback_json(&report).to_string();
        let plain = render_rollback_plain(&report).join("\n");
        for fact in [
            "candidate",
            "unproven",
            "active-build",
            "known-build",
            "pair.a",
            "pair.b",
        ] {
            assert!(json.contains(fact));
            assert!(plain.contains(fact));
        }
    }

    #[test]
    fn install_replay_attestation_never_claims_current_replay_readiness() {
        let binaries = binaries_with_public_selector(true);
        let mut installed = descriptor(false, true);
        installed.active_replay_attested = true;
        installed.known_good_replay_attested = true;
        installed.known_good_replay_snapshot = Some("a".repeat(64));
        let report = inspect_rollback_with(&binaries, |_| Ok(Some(installed)));

        assert_eq!(report.install_replay, "attested_snapshot");
        assert_eq!(report.journal_replay, "unproven");
        assert_eq!(report.rollback_safe, None);
        let json = rollback_json(&report);
        assert_eq!(json["install_replay"], "attested_snapshot");
        assert_eq!(json["current_replay"], "unproven");
        let snapshot = "a".repeat(64);
        assert_eq!(
            json["known_good"]["replay_snapshot_sha256"].as_str(),
            Some(snapshot.as_str())
        );
    }

    #[test]
    fn a_tampered_pair_proof_is_an_invalid_rollback_report() {
        let binaries = binaries_with_public_selector(true);
        let report = inspect_rollback_with(&binaries, |_| {
            Err(crate::update::InstalledPairInspectionError::Invalid(
                "selected pair changed after its install proof was recorded".into(),
            ))
        });
        assert_eq!(report.state, "invalid");
        assert_eq!(report.rollback_safe, None);
        assert!(report.error.as_deref().unwrap().contains("changed"));
    }
}
