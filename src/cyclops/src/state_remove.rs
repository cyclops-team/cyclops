//! Explicit, exact removal of the complete current Cyclops state home.
//!
//! This is deliberately separate from both `data forget --all` and the
//! installer uninstall. It has one bounded scope: the state home selected by
//! `CYCLOPS_HOME` (or the default home). Vendor configuration, installed
//! skill files in agent-owned directories (including a Cyclops-seeded copy),
//! binaries, pair stores, and PATH edits have their own owners.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cyclops_state::{
    InspectedEntry, InspectedKind, InspectionLimits, RegularFileEvidence, StateFile,
    StateInspector, StateRoot,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::cleanup::{self, MountIdentity};
use crate::client::{Client, ClientError};

const STATE_REMOVE_SCHEMA: u32 = 1;
const CHECKPOINT: &str = "operations/state-remove.json";
const LEASE: &str = cyclops_proto::DURABLE_RECORD_FORGET_LEASE;
const CONFIRMATION_PREFIX: &str = "remove-cyclops-state:";
const TOMBSTONE_SUFFIX: &str = ".removing";
const CHECKPOINT_BYTES_LIMIT: usize = cyclops_state::INSPECTION_FILE_BYTES_LIMIT_MAX;
const SCOPE: &str = "the complete current Cyclops state home only";
const PRESERVED: &str = "installed binaries, the installer-owned PATH block, vendor hook configuration, and skill files in agent-owned directories (including a Cyclops-seeded copy) remain outside this state-home operation";
const NEXT_STEP_SCOPE: &str = "remove the installed Cyclops binaries, pair store, and installer-owned PATH block; vendor configuration remains separate";
const INSTALLER_UNINSTALL_COMMAND: &str =
    "curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall";

static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

/// Body-free identity evidence for an entry shown by a destructive preview.
///
/// The state crate validates the same facts again through held descriptors
/// before it removes a regular file. No journal bytes are put in a plan,
/// checkpoint, confirmation, or report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EntryEvidence {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    links: u64,
    bytes: u64,
}

impl EntryEvidence {
    fn from_entry(entry: &InspectedEntry) -> Self {
        Self {
            device: entry.device,
            inode: entry.inode,
            mode: entry.mode,
            uid: entry.uid,
            links: entry.links,
            bytes: entry.size,
        }
    }

    fn matches(&self, entry: &InspectedEntry) -> bool {
        self.device == entry.device
            && self.inode == entry.inode
            && self.mode == entry.mode
            && self.uid == entry.uid
            && self.links == entry.links
            && self.bytes == entry.size
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RemovalTarget {
    relative: String,
    bytes: u64,
    evidence: RegularFileEvidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SocketTarget {
    relative: String,
    evidence: EntryEvidence,
}

/// Stable directory identity used after planned children have been removed.
///
/// Link count and allocation size can legitimately change while this operation
/// empties a directory, so they are not part of this comparison. Device,
/// inode, owner, and mode still reject a pathname replacement before the
/// final empty-directory bind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DirectoryEvidence {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
}

impl DirectoryEvidence {
    fn from_entry(entry: &InspectedEntry) -> Self {
        Self {
            device: entry.device,
            inode: entry.inode,
            mode: entry.mode,
            uid: entry.uid,
        }
    }

    fn matches(&self, entry: &InspectedEntry) -> bool {
        self.device == entry.device
            && self.inode == entry.inode
            && self.mode == entry.mode
            && self.uid == entry.uid
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DirectoryTarget {
    relative: String,
    evidence: DirectoryEvidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StateRemovalPlan {
    schema: u32,
    state_root: String,
    tombstone: String,
    /// Identity captured before the original root is renamed. Recovery only
    /// accepts a tombstone that still names this exact directory.
    root_evidence: DirectoryEvidence,
    root_mount: Option<MountIdentity>,
    files: usize,
    directories: Vec<DirectoryTarget>,
    bytes: u64,
    targets: Vec<RemovalTarget>,
    sockets: Vec<SocketTarget>,
    confirmation: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "state", rename_all = "snake_case")]
enum StateRemovalCheckpoint {
    Pending {
        schema: u32,
        operation_id: String,
        plan: StateRemovalPlan,
    },
    TargetsRemoved {
        schema: u32,
        operation_id: String,
        plan: StateRemovalPlan,
        removed_files: usize,
        removed_bytes: u64,
        already_absent_files: usize,
        already_absent_bytes: u64,
    },
}

/// What this invocation could prove about the selected regular files.
///
/// A file that was already absent during recovery is never credited to the
/// current process. That makes interrupted deletion observable rather than
/// presenting a later retry as if it performed every removal itself.
#[derive(Clone, Copy, Debug, Default)]
struct RemovalProgress {
    removed_files: usize,
    removed_bytes: u64,
    already_absent_files: usize,
    already_absent_bytes: u64,
}

/// Descriptor-bound authority for cleanup after the original state root has
/// been isolated. The paths are reporting-only; every deletion below uses the
/// held root or inspector, never the current `$CYCLOPS_HOME` pathname.
struct IsolatedRemoval {
    home: PathBuf,
    tombstone: PathBuf,
    root: StateRoot,
    inspector: StateInspector,
    lease: StateFile,
}

impl RemovalProgress {
    fn removed(&mut self, bytes: u64) -> Result<(), String> {
        self.removed_files = self
            .removed_files
            .checked_add(1)
            .ok_or_else(|| "complete state removal file count overflowed".to_string())?;
        self.removed_bytes = self
            .removed_bytes
            .checked_add(bytes)
            .ok_or_else(|| "complete state removal byte count overflowed".to_string())?;
        Ok(())
    }

    fn already_absent(&mut self, bytes: u64) -> Result<(), String> {
        self.already_absent_files = self
            .already_absent_files
            .checked_add(1)
            .ok_or_else(|| "complete state-removal recovery file count overflowed".to_string())?;
        self.already_absent_bytes = self
            .already_absent_bytes
            .checked_add(bytes)
            .ok_or_else(|| "complete state-removal recovery byte count overflowed".to_string())?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemovalState {
    Preview,
    ConfirmationRequired,
    RecoveryRequired,
    StateRemovalCompleted,
    AlreadyEmpty,
    Partial,
    Refused,
}

#[derive(Clone, Copy, Debug)]
enum RecoveryGuidance {
    None,
    ExactConfirmation,
    FreshPreview,
    ManualProvenance,
}

impl RecoveryGuidance {
    fn json(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::ExactConfirmation => Some(
                "A pending checkpoint authorizes only its original body-free plan. Resolve the reported condition, then rerun its exact confirmation.",
            ),
            Self::FreshPreview => Some(
                "Only the work counted in this report may have completed. Resolve the reported condition, then preview the retained state before any further removal.",
            ),
            Self::ManualProvenance => Some(
                "This tombstone has no durable checkpoint, so no later cyclops remove can safely resume it. Cyclops left it untouched; do not delete it because of its name. Handle it outside Cyclops only if you independently establish what it is.",
            ),
        }
    }

    fn plain(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::ExactConfirmation => Some(
                "resolve that condition, then rerun the exact confirmation from the report; Cyclops will not broaden the state-home scope",
            ),
            Self::FreshPreview => Some(
                "only the work counted above may have completed. Run cyclops remove --all to inspect retained state before any further removal",
            ),
            Self::ManualProvenance => Some(
                "this tombstone has no durable checkpoint, so no later cyclops remove can safely resume it. Cyclops left it untouched; do not delete it because of its name. Handle it outside Cyclops only if you independently establish what it is",
            ),
        }
    }
}

impl RemovalState {
    fn name(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::ConfirmationRequired => "confirmation_required",
            Self::RecoveryRequired => "recovery_required",
            Self::StateRemovalCompleted => "state_removal_completed",
            Self::AlreadyEmpty => "already_empty",
            Self::Partial => "partial",
            Self::Refused => "refused",
        }
    }
}

#[derive(Debug)]
struct RemovalReport {
    mode: &'static str,
    state: RemovalState,
    plan: Option<StateRemovalPlan>,
    operation_id: Option<String>,
    progress: RemovalProgress,
    error: Option<String>,
    recovery_guidance: RecoveryGuidance,
}

impl RemovalReport {
    fn preview(plan: StateRemovalPlan) -> Self {
        Self {
            mode: "preview",
            state: RemovalState::Preview,
            plan: Some(plan),
            operation_id: None,
            progress: RemovalProgress::default(),
            error: None,
            recovery_guidance: RecoveryGuidance::None,
        }
    }

    fn confirmation_required(plan: StateRemovalPlan, error: String) -> Self {
        Self {
            mode: "preview",
            state: RemovalState::ConfirmationRequired,
            plan: Some(plan),
            operation_id: None,
            progress: RemovalProgress::default(),
            error: Some(error),
            recovery_guidance: RecoveryGuidance::ExactConfirmation,
        }
    }

    fn recovery_required(plan: StateRemovalPlan, operation_id: String, error: String) -> Self {
        Self {
            mode: "recovery",
            state: RemovalState::RecoveryRequired,
            plan: Some(plan),
            operation_id: Some(operation_id),
            progress: RemovalProgress::default(),
            error: Some(error),
            recovery_guidance: RecoveryGuidance::ExactConfirmation,
        }
    }

    fn confirmation(&self) -> Option<&str> {
        self.plan
            .as_ref()
            .and_then(|plan| (!plan.confirmation.is_empty()).then_some(plan.confirmation.as_str()))
    }

    fn completed(plan: StateRemovalPlan, operation_id: String, progress: RemovalProgress) -> Self {
        Self {
            mode: "apply",
            state: RemovalState::StateRemovalCompleted,
            plan: Some(plan),
            operation_id: Some(operation_id),
            progress,
            error: None,
            recovery_guidance: RecoveryGuidance::None,
        }
    }

    fn already_empty(home: &Path, tombstone: &Path) -> Self {
        Self {
            mode: "preview",
            state: RemovalState::AlreadyEmpty,
            plan: Some(StateRemovalPlan {
                schema: STATE_REMOVE_SCHEMA,
                state_root: home.display().to_string(),
                tombstone: tombstone.display().to_string(),
                root_evidence: DirectoryEvidence {
                    device: 0,
                    inode: 0,
                    mode: 0,
                    uid: 0,
                },
                root_mount: None,
                files: 0,
                directories: Vec::new(),
                bytes: 0,
                targets: Vec::new(),
                sockets: Vec::new(),
                confirmation: String::new(),
            }),
            operation_id: None,
            progress: RemovalProgress::default(),
            error: None,
            recovery_guidance: RecoveryGuidance::None,
        }
    }

    fn partial(
        plan: StateRemovalPlan,
        operation_id: String,
        progress: RemovalProgress,
        error: String,
    ) -> Self {
        Self {
            mode: "apply",
            state: RemovalState::Partial,
            plan: Some(plan),
            operation_id: Some(operation_id),
            progress,
            error: Some(error),
            recovery_guidance: RecoveryGuidance::FreshPreview,
        }
    }

    fn refused(plan: Option<StateRemovalPlan>, error: String) -> Self {
        Self {
            mode: "preview",
            state: RemovalState::Refused,
            plan,
            operation_id: None,
            progress: RemovalProgress::default(),
            error: Some(error),
            recovery_guidance: RecoveryGuidance::FreshPreview,
        }
    }

    fn checkpoint_free_tombstone(tombstone: &Path) -> Self {
        Self {
            mode: "recovery",
            state: RemovalState::Refused,
            plan: None,
            operation_id: None,
            progress: RemovalProgress::default(),
            error: Some(checkpoint_free_tombstone_error(tombstone)),
            recovery_guidance: RecoveryGuidance::ManualProvenance,
        }
    }

    fn ok(&self) -> bool {
        matches!(
            self.state,
            RemovalState::Preview
                | RemovalState::StateRemovalCompleted
                | RemovalState::AlreadyEmpty
        )
    }
}

/// Preview the exact current state home or remove only that preview while the
/// daemon remains stopped.
pub(crate) fn run(json_output: bool, confirmation: Option<&str>) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let report = match confirmation {
        Some(confirmation) => apply_at(&home, confirmation),
        None => preview_at(&home),
    };
    if json_output {
        println!("{}", render_json(&report));
    } else {
        print!("{}", render_plain(&report));
    }
    i32::from(!report.ok())
}

fn preview_at(home: &Path) -> RemovalReport {
    let tombstone = match tombstone_path(home) {
        Ok(path) => path,
        Err(error) => return RemovalReport::refused(None, error),
    };

    let tombstone_inspector = match open_private_inspector(&tombstone) {
        Ok(inspector) => inspector,
        Err(error) => return RemovalReport::refused(None, error),
    };
    if let Some(inspector) = tombstone_inspector {
        return match read_checkpoint(&inspector, home, &tombstone) {
            Ok(Some(checkpoint)) => {
                let (operation_id, plan) = checkpoint_parts(checkpoint);
                match ensure_tombstone_matches_plan(&inspector, &plan) {
                    Ok(()) => RemovalReport::recovery_required(
                        plan,
                        operation_id,
                        "an isolated prior state-removal checkpoint remains; rerun its exact confirmation after resolving the reported condition".into(),
                    ),
                    Err(error) => RemovalReport::refused(None, error),
                }
            }
            Ok(None) => RemovalReport::checkpoint_free_tombstone(&tombstone),
            Err(error) => RemovalReport::refused(None, error),
        };
    }

    let inspector = match open_private_inspector(home) {
        Ok(Some(inspector)) => inspector,
        Ok(None) => return RemovalReport::already_empty(home, &tombstone),
        Err(error) => return RemovalReport::refused(None, error),
    };
    match read_checkpoint(&inspector, home, &tombstone) {
        Ok(Some(checkpoint)) => {
            let (operation_id, plan) = checkpoint_parts(checkpoint);
            RemovalReport::recovery_required(
                plan,
                operation_id,
                "a prior confirmed state-removal checkpoint remains before isolation; rerun its exact confirmation after resolving the reported condition".into(),
            )
        }
        Ok(None) => match plan_from_inspector(&inspector, home, &tombstone) {
            Ok(plan) => RemovalReport::preview(plan),
            Err(error) => RemovalReport::refused(None, error),
        },
        Err(error) => RemovalReport::refused(None, error),
    }
}

fn apply_at(home: &Path, confirmation: &str) -> RemovalReport {
    apply_at_with(home, confirmation, daemon_is_stopped)
}

fn apply_at_with<F>(home: &Path, confirmation: &str, daemon_stopped: F) -> RemovalReport
where
    F: FnOnce() -> Result<(), String>,
{
    let tombstone = match tombstone_path(home) {
        Ok(path) => path,
        Err(error) => return RemovalReport::refused(None, error),
    };

    let tombstone_inspector = match open_private_inspector(&tombstone) {
        Ok(inspector) => inspector,
        Err(error) => return RemovalReport::refused(None, error),
    };
    if let Some(inspector) = tombstone_inspector {
        return match read_checkpoint(&inspector, home, &tombstone) {
            Ok(Some(_)) => resume_isolated(home, &tombstone, confirmation),
            Ok(None) => RemovalReport::checkpoint_free_tombstone(&tombstone),
            Err(error) => RemovalReport::refused(None, error),
        };
    }

    // This probe happens before rebuilding a complete-tree plan. A daemon
    // socket is itself state, so waiting until after a stale-token comparison
    // would turn a live daemon into a misleading confirmation mismatch. The
    // shared lease below serializes the check-to-isolation gap. Once the old
    // root is isolated, recovery is tombstone-local and never touches a new
    // current state home, so it deliberately does not need this probe.
    if let Err(error) = daemon_stopped() {
        return RemovalReport::refused(None, error);
    }

    let inspector = match open_private_inspector(home) {
        Ok(Some(inspector)) => inspector,
        Ok(None) => return RemovalReport::already_empty(home, &tombstone),
        Err(error) => return RemovalReport::refused(None, error),
    };
    let initial_checkpoint = match read_checkpoint(&inspector, home, &tombstone) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return RemovalReport::refused(None, error),
    };
    match &initial_checkpoint {
        Some(checkpoint) => {
            let (operation_id, plan) = checkpoint_parts_ref(checkpoint);
            if confirmation != plan.confirmation {
                return RemovalReport::recovery_required(
                    plan.clone(),
                    operation_id.to_string(),
                    "the supplied confirmation does not name the pending state-removal plan".into(),
                );
            }
            plan.clone()
        }
        None => match plan_from_inspector(&inspector, home, &tombstone) {
            Ok(plan) => {
                if confirmation != plan.confirmation {
                    return RemovalReport::confirmation_required(
                        plan,
                        "the supplied confirmation does not match the current complete-state preview; preview again before deleting".into(),
                    );
                }
                plan
            }
            Err(error) => return RemovalReport::refused(None, error),
        },
    };
    begin_or_resume_unisolated(home, &tombstone, confirmation)
}

fn begin_or_resume_unisolated(home: &Path, tombstone: &Path, confirmation: &str) -> RemovalReport {
    let (root, inspector, lease) = match lock_root(home) {
        Ok(held) => held,
        Err(error) => return RemovalReport::refused(None, error),
    };
    if tombstone.exists() {
        return RemovalReport::refused(
            None,
            "a state-removal tombstone appeared while Cyclops was acquiring its lease; inspect recovery before trying again".into(),
        );
    }

    match read_checkpoint(&inspector, home, tombstone) {
        Ok(Some(checkpoint)) => {
            let (operation_id, plan) = checkpoint_parts(checkpoint);
            if confirmation != plan.confirmation {
                return RemovalReport::recovery_required(
                    plan,
                    operation_id,
                    "the supplied confirmation does not name the pending state-removal plan".into(),
                );
            }
            if let Err(error) = ensure_pending_source_matches(&inspector, home, tombstone, &plan) {
                return RemovalReport::partial(
                    plan,
                    operation_id,
                    RemovalProgress::default(),
                    error,
                );
            }
            isolate_and_finish(home, tombstone, root, lease, inspector, plan, operation_id)
        }
        Ok(None) => {
            let plan = match plan_from_inspector(&inspector, home, tombstone) {
                Ok(plan) => plan,
                Err(error) => return RemovalReport::refused(None, error),
            };
            if confirmation != plan.confirmation {
                return RemovalReport::confirmation_required(
                    plan,
                    "the supplied confirmation does not match the state home after its removal lease was acquired; preview again".into(),
                );
            }
            let operation_id = next_operation_id();
            let checkpoint = StateRemovalCheckpoint::Pending {
                schema: STATE_REMOVE_SCHEMA,
                operation_id: operation_id.clone(),
                plan: plan.clone(),
            };
            if let Err(error) = write_checkpoint(&root, &checkpoint) {
                return RemovalReport::partial(
                    plan,
                    operation_id,
                    RemovalProgress::default(),
                    error,
                );
            }
            state_remove_boundary(RemovalBoundary::PendingWritten, home);
            isolate_and_finish(home, tombstone, root, lease, inspector, plan, operation_id)
        }
        Err(error) => RemovalReport::refused(None, error),
    }
}

fn isolate_and_finish(
    home: &Path,
    tombstone: &Path,
    _root: StateRoot,
    lease: StateFile,
    inspector: StateInspector,
    plan: StateRemovalPlan,
    operation_id: String,
) -> RemovalReport {
    let name = match tombstone.file_name() {
        Some(name) => name,
        None => {
            return RemovalReport::partial(
                plan,
                operation_id,
                RemovalProgress::default(),
                "the state-removal tombstone has no final path component".into(),
            );
        }
    };
    let isolation = match inspector.isolate_root_to_sibling(name) {
        Ok(isolation) => isolation,
        Err(error) => {
            return RemovalReport::partial(
                plan,
                operation_id,
                RemovalProgress::default(),
                format!("isolate the confirmed state home: {error}"),
            );
        }
    };
    state_remove_boundary(RemovalBoundary::Isolated, home);
    let (tombstone_root, tombstone_inspector) = isolation.into_parts();
    finish_isolated(
        IsolatedRemoval {
            home: home.to_path_buf(),
            tombstone: tombstone.to_path_buf(),
            root: tombstone_root,
            inspector: tombstone_inspector,
            lease,
        },
        plan,
        operation_id,
    )
}

fn resume_isolated(home: &Path, tombstone: &Path, confirmation: &str) -> RemovalReport {
    let (root, inspector) = match open_private_root_and_inspector(tombstone) {
        Ok(Some(held)) => held,
        Ok(None) => {
            return RemovalReport::refused(
                None,
                "the state-removal tombstone disappeared before recovery could inspect it".into(),
            )
        }
        Err(error) => return RemovalReport::refused(None, error),
    };
    let checkpoint = match read_checkpoint(&inspector, home, tombstone) {
        Ok(Some(checkpoint)) => checkpoint,
        Ok(None) => return RemovalReport::checkpoint_free_tombstone(tombstone),
        Err(error) => return RemovalReport::refused(None, error),
    };
    let (operation_id, plan) = checkpoint_parts_ref(&checkpoint);
    if confirmation != plan.confirmation {
        return RemovalReport::recovery_required(
            plan.clone(),
            operation_id.to_string(),
            "the supplied confirmation does not name the pending state-removal plan".into(),
        );
    }
    if let Err(error) = ensure_tombstone_matches_plan(&inspector, plan) {
        return RemovalReport::refused(Some(plan.clone()), error);
    }
    let lease = match lock_held_root(&root, "recover complete state-removal tombstone") {
        Ok(lease) => lease,
        Err(error) => {
            return RemovalReport::recovery_required(plan.clone(), operation_id.to_string(), error);
        }
    };
    let checkpoint = match read_checkpoint(&inspector, home, tombstone) {
        Ok(Some(checkpoint)) => checkpoint,
        Ok(None) => {
            return RemovalReport::recovery_required(
                plan.clone(),
                operation_id.to_string(),
                "the state-removal checkpoint disappeared while its recovery lease was acquired; inspect the tombstone again".into(),
            )
        }
        Err(error) => {
            return RemovalReport::recovery_required(plan.clone(), operation_id.to_string(), error);
        }
    };
    match checkpoint {
        StateRemovalCheckpoint::Pending {
            operation_id, plan, ..
        } => {
            if confirmation != plan.confirmation {
                return RemovalReport::recovery_required(
                    plan,
                    operation_id,
                    "the isolated checkpoint changed while Cyclops was acquiring its recovery lease; inspect it again before retrying".into(),
                );
            }
            if let Err(error) = ensure_tombstone_matches_plan(&inspector, &plan) {
                return RemovalReport::refused(Some(plan), error);
            }
            finish_isolated(
                IsolatedRemoval {
                    home: home.to_path_buf(),
                    tombstone: tombstone.to_path_buf(),
                    root,
                    inspector,
                    lease,
                },
                plan,
                operation_id,
            )
        }
        StateRemovalCheckpoint::TargetsRemoved {
            operation_id,
            plan,
            removed_files,
            removed_bytes,
            already_absent_files,
            already_absent_bytes,
            ..
        } => {
            if confirmation != plan.confirmation {
                return RemovalReport::recovery_required(
                    plan,
                    operation_id,
                    "the isolated completion checkpoint changed while Cyclops was acquiring its recovery lease; inspect it again before retrying".into(),
                );
            }
            if let Err(error) = ensure_tombstone_matches_plan(&inspector, &plan) {
                return RemovalReport::refused(Some(plan), error);
            }
            finalize_tombstone(
                IsolatedRemoval {
                    home: home.to_path_buf(),
                    tombstone: tombstone.to_path_buf(),
                    root,
                    inspector,
                    lease,
                },
                plan,
                operation_id,
                RemovalProgress {
                    removed_files,
                    removed_bytes,
                    already_absent_files,
                    already_absent_bytes,
                },
            )
        }
    }
}

fn finish_isolated(
    isolated: IsolatedRemoval,
    plan: StateRemovalPlan,
    operation_id: String,
) -> RemovalReport {
    if let Err(error) = ensure_tombstone_matches_plan(&isolated.inspector, &plan) {
        return RemovalReport::refused(Some(plan), error);
    }
    let progress = match remove_plan_files(&isolated.inspector, &plan) {
        Ok(progress) => progress,
        Err((progress, error)) => {
            return RemovalReport::partial(plan, operation_id, progress, error);
        }
    };
    if let Err(error) = remove_plan_sockets(&isolated.root, &isolated.inspector, &plan) {
        return RemovalReport::partial(plan, operation_id, progress, error);
    }
    if let Err(error) = remove_empty_directories(&isolated.inspector, &plan) {
        return RemovalReport::partial(plan, operation_id, progress, error);
    }
    let checkpoint = StateRemovalCheckpoint::TargetsRemoved {
        schema: STATE_REMOVE_SCHEMA,
        operation_id: operation_id.clone(),
        plan: plan.clone(),
        removed_files: progress.removed_files,
        removed_bytes: progress.removed_bytes,
        already_absent_files: progress.already_absent_files,
        already_absent_bytes: progress.already_absent_bytes,
    };
    if let Err(error) = write_checkpoint(&isolated.root, &checkpoint) {
        return RemovalReport::partial(plan, operation_id, progress, error);
    }
    state_remove_boundary(RemovalBoundary::TargetsRemoved, &isolated.home);

    finalize_tombstone(isolated, plan, operation_id, progress)
}

fn finalize_tombstone(
    isolated: IsolatedRemoval,
    plan: StateRemovalPlan,
    operation_id: String,
    progress: RemovalProgress,
) -> RemovalReport {
    let IsolatedRemoval {
        home,
        tombstone,
        root,
        inspector,
        lease,
    } = isolated;
    // This root has already been renamed away from `$CYCLOPS_HOME`, so a new
    // daemon cannot discover it through the current-home path. Keep the old
    // lease until its named operation files are gone as well: the final
    // descriptor-bound cleanup then has one continuous ownership interval.
    if let Err(error) = remove_operation_files(&inspector, &plan, &[CHECKPOINT, LEASE]) {
        return RemovalReport::partial(plan, operation_id, progress, error);
    }
    state_remove_boundary(RemovalBoundary::OperationFilesRemoved, &tombstone);
    drop(lease);
    if let Err(error) = remove_operations_directory(&inspector, &plan) {
        return RemovalReport::partial(plan, operation_id, progress, error);
    }
    state_remove_boundary(RemovalBoundary::OperationsDirectoryRemoved, &tombstone);
    if let Err(error) = require_plan_mount(inspector.root(), &plan, "final state-removal tombstone")
    {
        return RemovalReport::partial(plan, operation_id, progress, error);
    }
    drop(inspector);
    if let Err(error) = root.remove_if_empty() {
        return RemovalReport::partial(
            plan,
            operation_id,
            progress,
            format!("remove the empty state-removal tombstone: {error}"),
        );
    }
    state_remove_boundary(RemovalBoundary::TombstoneRemoved, &home);
    if home.exists() {
        return RemovalReport::partial(
            plan,
            operation_id,
            progress,
            "the original state home was removed, but a new state home appeared after isolation and was left untouched; make a fresh preview before removing that new state".into(),
        );
    }
    RemovalReport::completed(plan, operation_id, progress)
}

fn open_private_root_and_inspector(
    path: &Path,
) -> Result<Option<(StateRoot, StateInspector)>, String> {
    let Some(root) = StateRoot::open_existing(path)
        .map_err(|error| format!("open state root {}: {error}", path.display()))?
    else {
        return Ok(None);
    };
    let inspector = root
        .inspector()
        .map_err(|error| format!("inspect held state root {}: {error}", path.display()))?;
    if !inspector
        .private_and_stable()
        .map_err(|error| format!("validate state root {}: {error}", path.display()))?
    {
        return Err(format!(
            "state root {} is not a stable owner-only directory; Cyclops will not remove it",
            path.display()
        ));
    }
    Ok(Some((root, inspector)))
}

fn lock_root(path: &Path) -> Result<(StateRoot, StateInspector, StateFile), String> {
    let Some((root, initial_inspector)) = open_private_root_and_inspector(path)? else {
        return Err("the state root is absent".into());
    };
    // Opening the shared lease can create its `operations` parent on a fresh
    // state root. Refresh the inspector from the *same held root descriptor*
    // afterwards so isolation carries current directory-link evidence rather
    // than treating our own lease setup as a competing mutation.
    drop(initial_inspector);
    let lease = lock_held_root(&root, "confirm complete state removal")?;
    let inspector = root.inspector().map_err(|error| {
        format!(
            "refresh held state-root inspection {}: {error}",
            path.display()
        )
    })?;
    if !inspector
        .private_and_stable()
        .map_err(|error| format!("validate refreshed state root {}: {error}", path.display()))?
    {
        return Err(format!(
            "state root {} changed while its complete-removal lease was acquired",
            path.display()
        ));
    }
    Ok((root, inspector, lease))
}

fn lock_held_root(root: &StateRoot, purpose: &str) -> Result<StateFile, String> {
    let lease = root
        .open_append(Path::new(LEASE))
        .map_err(|error| format!("open state lease to {purpose}: {error}"))?;
    match lease.try_lock() {
        Ok(true) => Ok(lease),
        Ok(false) => Err(
            "cyclopsd or another confirmed removal holds the shared state lease; wait for it to finish"
                .into(),
        ),
        Err(error) => Err(format!("lock state lease to {purpose}: {error}")),
    }
}

fn daemon_is_stopped() -> Result<(), String> {
    match Client::connect() {
        Err(ClientError::NotRunning(_)) => Ok(()),
        Ok(_) => Err(
            "cyclopsd is running; stop it with `cyclops daemon stop` before confirming complete state removal"
                .into(),
        ),
        Err(error) => Err(format!(
            "cannot prove cyclopsd is stopped ({error}); resolve the daemon state before confirming complete state removal"
        )),
    }
}

fn open_private_inspector(path: &Path) -> Result<Option<StateInspector>, String> {
    let Some(inspector) = StateInspector::open_existing(path)
        .map_err(|error| format!("inspect state root {}: {error}", path.display()))?
    else {
        return Ok(None);
    };
    if !inspector
        .private_and_stable()
        .map_err(|error| format!("validate state root {}: {error}", path.display()))?
    {
        return Err(format!(
            "state root {} is not a stable owner-only directory; Cyclops will not remove it",
            path.display()
        ));
    }
    Ok(Some(inspector))
}

fn ensure_tombstone_matches_plan(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
) -> Result<(), String> {
    if !plan.root_evidence.matches(inspector.root()) {
        return Err(
            "the state-removal tombstone does not name the exact root captured before isolation; Cyclops will not resume it"
                .into(),
        );
    }
    require_plan_mount(inspector.root(), plan, "state-removal tombstone")
}

/// A missing checkpoint is never a permission to infer an empty deletion
/// scope. It can mean the original process crashed after deleting its final
/// operation record, and the sibling pathname may have been reused before a
/// later command arrives. The process holding the original descriptor can
/// complete that last `rmdir`; a later invocation leaves the sibling alone.
fn checkpoint_free_tombstone_error(tombstone: &Path) -> String {
    format!(
        "the state-removal tombstone at {} has no durable checkpoint, so no later cyclops remove can safely resume it. Cyclops will not delete it because of its pathname. Handle it outside Cyclops only if you independently establish what it is",
        tombstone.display()
    )
}

fn tombstone_path(home: &Path) -> Result<PathBuf, String> {
    let name = home
        .file_name()
        .ok_or_else(|| "the current Cyclops state home has no final path component".to_string())?;
    let mut bytes = name.as_encoded_bytes().to_vec();
    bytes.extend_from_slice(TOMBSTONE_SUFFIX.as_bytes());
    if bytes.len() > 240 {
        return Err(
            "the current Cyclops state-home name is too long for a safe removal tombstone".into(),
        );
    }
    let tombstone = OsString::from_vec(bytes);
    Ok(home
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(tombstone))
}

fn plan_from_inspector(
    inspector: &StateInspector,
    home: &Path,
    tombstone: &Path,
) -> Result<StateRemovalPlan, String> {
    plan_from_inspector_with_mount_probe(inspector, home, tombstone, cleanup::mount_identity)
}

fn plan_from_inspector_with_mount_probe<F>(
    inspector: &StateInspector,
    home: &Path,
    tombstone: &Path,
    mut mount_probe: F,
) -> Result<StateRemovalPlan, String>
where
    F: FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
{
    let state_root = utf8_path(home, "state home")?;
    let tombstone = utf8_path(tombstone, "state-removal tombstone")?;
    let mut inventory = TreeInventory::default();
    let root = inspect_root_with_remaining(inspector, &inventory)?;
    let root_evidence = DirectoryEvidence::from_entry(&root.directory);
    let root_mount = mount_probe(&root.directory)
        .map_err(|error| format!("inspect complete state-home mount: {error}"))?;
    collect_directory(
        inspector,
        root,
        &mut inventory,
        root_mount,
        &mut mount_probe,
    )?;
    inventory
        .targets
        .sort_by(|left, right| left.relative.cmp(&right.relative));
    inventory
        .directories
        .sort_by(|left, right| left.relative.cmp(&right.relative));
    inventory
        .sockets
        .sort_by(|left, right| left.relative.cmp(&right.relative));

    let files = inventory.targets.len();
    let bytes = inventory.targets.iter().try_fold(0_u64, |total, target| {
        total
            .checked_add(target.bytes)
            .ok_or_else(|| "complete state-removal byte total overflowed".to_string())
    })?;
    let confirmation = confirmation_for(
        &state_root,
        &tombstone,
        &root_evidence,
        Some(root_mount),
        &inventory.targets,
        &inventory.directories,
        &inventory.sockets,
    )?;
    let plan = StateRemovalPlan {
        schema: STATE_REMOVE_SCHEMA,
        state_root,
        tombstone,
        root_evidence,
        root_mount: Some(root_mount),
        files,
        directories: inventory.directories,
        bytes,
        targets: inventory.targets,
        sockets: inventory.sockets,
        confirmation,
    };
    validate_plan(&plan)?;
    checkpoint_fits(&plan)?;
    Ok(plan)
}

#[derive(Default)]
struct TreeInventory {
    entries: usize,
    name_bytes: usize,
    targets: Vec<RemovalTarget>,
    directories: Vec<DirectoryTarget>,
    sockets: Vec<SocketTarget>,
}

fn inspect_root_with_remaining(
    inspector: &StateInspector,
    inventory: &TreeInventory,
) -> Result<cyclops_state::DirectoryInspection, String> {
    let limits = remaining_limits(inventory)?;
    let snapshot = inspector
        .inspect_root(limits)
        .map_err(|error| format!("inspect complete state home: {error}"))?;
    if snapshot.truncated {
        return Err(
            "complete state-removal preview reached its bounded entry or name limit".into(),
        );
    }
    Ok(snapshot)
}

fn collect_directory(
    inspector: &StateInspector,
    snapshot: cyclops_state::DirectoryInspection,
    inventory: &mut TreeInventory,
    allowed_mount: MountIdentity,
    mount_probe: &mut impl FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
) -> Result<(), String> {
    cleanup::require_same_mount(&snapshot.directory, allowed_mount, mount_probe)
        .map_err(|error| format!("complete state-removal {error}"))?;
    for entry in snapshot.entries {
        cleanup::require_same_mount(&entry, allowed_mount, mount_probe)
            .map_err(|error| format!("complete state-removal {error}"))?;
        note_entry(&entry, inventory)?;
        let relative = entry_relative(inspector, &entry)?;
        let relative_text = relative_to_string(&relative)?;
        match entry.kind {
            InspectedKind::RegularFile => {
                if is_operation_infrastructure(&relative_text) {
                    continue;
                }
                require_safe_entry(&entry, "state file")?;
                if entry.links != 1 {
                    return Err(format!(
                        "state file {} has multiple hard links; Cyclops will not remove it",
                        relative_text
                    ));
                }
                let evidence = regular_file_evidence(inspector, &relative, &entry)?;
                inventory.targets.push(RemovalTarget {
                    relative: relative_text,
                    bytes: entry.size,
                    evidence,
                });
            }
            InspectedKind::Directory => {
                require_safe_entry(&entry, "state directory")?;
                let nested = inspector
                    .inspect_bound_directory(&entry, remaining_limits(inventory)?)
                    .map_err(|error| {
                        format!("inspect state directory {}: {error}", relative_text)
                    })?;
                if nested.truncated {
                    return Err(
                        "complete state-removal preview reached its bounded entry or name limit"
                            .into(),
                    );
                }
                collect_directory(inspector, nested, inventory, allowed_mount, mount_probe)?;
                if relative_text != "operations" {
                    inventory.directories.push(DirectoryTarget {
                        relative: relative_text,
                        evidence: DirectoryEvidence::from_entry(&entry),
                    });
                }
            }
            InspectedKind::Socket if relative == Path::new(cyclops_proto::SOCK_NAME) => {
                if entry.uid != effective_uid() || entry.links != 1 {
                    return Err(format!(
                        "state socket {} is not one owned single-link socket",
                        relative_text
                    ));
                }
                inventory.sockets.push(SocketTarget {
                    relative: relative_text,
                    evidence: EntryEvidence::from_entry(&entry),
                });
            }
            InspectedKind::Socket => {
                return Err(format!(
                    "state home contains unsupported socket {}; Cyclops will not remove it",
                    relative_text
                ));
            }
            InspectedKind::Symlink | InspectedKind::Other => {
                return Err(format!(
                    "state home contains unsafe {} {}; Cyclops will not remove it",
                    entry_kind_name(entry.kind),
                    relative_text
                ));
            }
        }
    }
    Ok(())
}

fn note_entry(entry: &InspectedEntry, inventory: &mut TreeInventory) -> Result<(), String> {
    inventory.entries = inventory
        .entries
        .checked_add(1)
        .ok_or_else(|| "complete state-removal entry count overflowed".to_string())?;
    if inventory.entries > cyclops_state::INSPECTION_ENTRY_LIMIT_MAX {
        return Err("complete state-removal preview reached its bounded entry limit".into());
    }
    let name = entry
        .path
        .file_name()
        .ok_or_else(|| "state inspection entry has no final path component".to_string())?;
    inventory.name_bytes = inventory
        .name_bytes
        .checked_add(name.as_encoded_bytes().len())
        .ok_or_else(|| "complete state-removal name-byte count overflowed".to_string())?;
    if inventory.name_bytes > cyclops_state::INSPECTION_NAME_BYTES_LIMIT_MAX {
        return Err("complete state-removal preview reached its bounded name limit".into());
    }
    Ok(())
}

fn remaining_limits(inventory: &TreeInventory) -> Result<InspectionLimits, String> {
    let entries = cyclops_state::INSPECTION_ENTRY_LIMIT_MAX
        .checked_sub(inventory.entries)
        .ok_or_else(|| {
            "complete state-removal preview reached its bounded entry limit".to_string()
        })?;
    let names = cyclops_state::INSPECTION_NAME_BYTES_LIMIT_MAX
        .checked_sub(inventory.name_bytes)
        .ok_or_else(|| {
            "complete state-removal preview reached its bounded name limit".to_string()
        })?;
    InspectionLimits::new(entries, names)
        .map_err(|error| format!("construct complete state-removal inspection bounds: {error}"))
}

fn require_safe_entry(entry: &InspectedEntry, label: &str) -> Result<(), String> {
    if entry.safe_beneath_owner_only_parent() {
        Ok(())
    } else {
        Err(format!(
            "{} {} is unsafe ({:?}); Cyclops will not remove it",
            label,
            entry.path.display(),
            entry.unsafe_reason
        ))
    }
}

/// Capture temporal evidence from the exact regular file selected by the
/// directory walk. The walk and this descriptor-relative read must agree, or
/// the preview refuses instead of blessing a tree that changed under it.
fn regular_file_evidence(
    inspector: &StateInspector,
    relative: &Path,
    walked: &InspectedEntry,
) -> Result<RegularFileEvidence, String> {
    let Some((observed, evidence)) = inspector
        .inspect_file_with(relative, u64::MAX, |file| {
            file.metadata()
                .map(|metadata| RegularFileEvidence::from_metadata(&metadata))
        })
        .map_err(|error| {
            format!(
                "inspect complete state-removal file {}: {error}",
                relative.display()
            )
        })?
    else {
        return Err(format!(
            "state file {} disappeared during complete state-removal preview",
            relative.display()
        ));
    };
    if observed.kind != InspectedKind::RegularFile
        || !same_entry_identity(walked, &observed)
        || !evidence.matches_entry(&observed)
    {
        return Err(format!(
            "state file {} changed during complete state-removal preview",
            relative.display()
        ));
    }
    Ok(evidence)
}

fn same_entry_identity(left: &InspectedEntry, right: &InspectedEntry) -> bool {
    left.kind == right.kind
        && left.device == right.device
        && left.inode == right.inode
        && left.mode == right.mode
        && left.uid == right.uid
        && left.links == right.links
        && left.size == right.size
}

fn entry_relative(inspector: &StateInspector, entry: &InspectedEntry) -> Result<PathBuf, String> {
    entry
        .path
        .strip_prefix(inspector.path())
        .map(PathBuf::from)
        .map_err(|_| "state inspection entry is outside the held state root".to_string())
}

fn relative_to_string(path: &Path) -> Result<String, String> {
    let mut pieces = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err("state-removal plan has a non-normal relative path".into());
        };
        let text = component.to_str().ok_or_else(|| {
            "state-removal plan contains a non-UTF-8 path; Cyclops will not remove it".to_string()
        })?;
        pieces.push(text);
    }
    if pieces.is_empty() {
        return Err("state-removal plan has an empty relative path".into());
    }
    Ok(pieces.join("/"))
}

fn utf8_path(path: &Path, label: &str) -> Result<String, String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{label} is not UTF-8; Cyclops will not remove it"))
}

fn entry_kind_name(kind: InspectedKind) -> &'static str {
    match kind {
        InspectedKind::Directory => "directory",
        InspectedKind::RegularFile => "regular file",
        InspectedKind::Socket => "socket",
        InspectedKind::Symlink => "symbolic link",
        InspectedKind::Other => "unsupported entry",
    }
}

fn is_operation_infrastructure(relative: &str) -> bool {
    relative == CHECKPOINT || relative == LEASE
}

fn confirmation_for(
    state_root: &str,
    tombstone: &str,
    root_evidence: &DirectoryEvidence,
    root_mount: Option<MountIdentity>,
    targets: &[RemovalTarget],
    directories: &[DirectoryTarget],
    sockets: &[SocketTarget],
) -> Result<String, String> {
    #[derive(Serialize)]
    struct Confirmation<'a> {
        scope: &'static str,
        state_root: &'a str,
        tombstone: &'a str,
        root_evidence: &'a DirectoryEvidence,
        root_mount: Option<MountIdentity>,
        targets: &'a [RemovalTarget],
        directories: &'a [DirectoryTarget],
        sockets: &'a [SocketTarget],
    }

    let bytes = serde_json::to_vec(&Confirmation {
        scope: SCOPE,
        state_root,
        tombstone,
        root_evidence,
        root_mount,
        targets,
        directories,
        sockets,
    })
    .map_err(|error| format!("serialize complete state-removal confirmation: {error}"))?;
    Ok(format!("{CONFIRMATION_PREFIX}{:x}", Sha256::digest(bytes)))
}

fn validate_plan(plan: &StateRemovalPlan) -> Result<(), String> {
    if plan.schema != STATE_REMOVE_SCHEMA {
        return Err("state-removal checkpoint has an unsupported schema".into());
    }
    if plan.files != plan.targets.len() {
        return Err("state-removal checkpoint has invalid file counts".into());
    }
    if plan.root_mount.is_none() {
        return Err("state-removal checkpoint has no root mount identity".into());
    }
    if plan.root_evidence.inode == 0 || plan.root_evidence.uid != effective_uid() {
        return Err("state-removal checkpoint has invalid original-root identity".into());
    }
    let mut files = BTreeSet::new();
    let mut bytes = 0_u64;
    for target in &plan.targets {
        validate_relative(&target.relative, "state-removal target")?;
        if is_operation_infrastructure(&target.relative) {
            return Err("state-removal checkpoint names its own operation infrastructure".into());
        }
        if target.bytes != target.evidence.bytes() || !files.insert(&target.relative) {
            return Err("state-removal checkpoint has inconsistent target evidence".into());
        }
        bytes = bytes
            .checked_add(target.bytes)
            .ok_or_else(|| "state-removal checkpoint byte total overflowed".to_string())?;
    }
    if bytes != plan.bytes {
        return Err("state-removal checkpoint has an invalid byte total".into());
    }
    let mut directories = BTreeSet::new();
    for directory in &plan.directories {
        validate_relative(&directory.relative, "state-removal directory")?;
        if directory.relative == "operations" || !directories.insert(&directory.relative) {
            return Err("state-removal checkpoint has invalid directory targets".into());
        }
    }
    if plan.sockets.len() > 1 {
        return Err("state-removal checkpoint has too many socket targets".into());
    }
    for socket in &plan.sockets {
        if socket.relative != cyclops_proto::SOCK_NAME {
            return Err("state-removal checkpoint has an out-of-scope socket target".into());
        }
    }
    let confirmation = confirmation_for(
        &plan.state_root,
        &plan.tombstone,
        &plan.root_evidence,
        plan.root_mount,
        &plan.targets,
        &plan.directories,
        &plan.sockets,
    )?;
    if plan.confirmation != confirmation {
        return Err("state-removal checkpoint confirmation is invalid".into());
    }
    Ok(())
}

fn validate_relative(relative: &str, label: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(relative);
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        && !relative.is_empty()
    {
        Ok(path)
    } else {
        Err(format!("{label} has an unsafe relative path"))
    }
}

fn checkpoint_fits(plan: &StateRemovalPlan) -> Result<(), String> {
    let checkpoint = StateRemovalCheckpoint::Pending {
        schema: STATE_REMOVE_SCHEMA,
        operation_id: "state-remove-preview".into(),
        plan: plan.clone(),
    };
    let bytes = serde_json::to_vec(&checkpoint)
        .map_err(|error| format!("serialize state-removal checkpoint: {error}"))?;
    if bytes.len() > CHECKPOINT_BYTES_LIMIT {
        return Err(format!(
            "complete state-removal plan exceeds its {}-byte checkpoint bound; export needed records and remove this state home in a later supported operation",
            CHECKPOINT_BYTES_LIMIT
        ));
    }
    Ok(())
}

fn ensure_pending_source_matches(
    inspector: &StateInspector,
    home: &Path,
    tombstone: &Path,
    plan: &StateRemovalPlan,
) -> Result<(), String> {
    let current = plan_from_inspector(inspector, home, tombstone)?;
    if &current != plan {
        return Err(
            "the complete state home changed after its pending confirmation; Cyclops left it in place and did not isolate unpreviewed entries"
                .into(),
        );
    }
    Ok(())
}

fn plan_mount(plan: &StateRemovalPlan) -> Result<MountIdentity, String> {
    plan.root_mount
        .ok_or_else(|| "state-removal plan has no root mount identity".to_string())
}

fn require_plan_mount(
    entry: &InspectedEntry,
    plan: &StateRemovalPlan,
    label: &str,
) -> Result<(), String> {
    require_plan_mount_with(entry, plan, label, &mut cleanup::mount_identity)
}

fn require_plan_mount_with(
    entry: &InspectedEntry,
    plan: &StateRemovalPlan,
    label: &str,
    mount_probe: &mut impl FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
) -> Result<(), String> {
    cleanup::require_same_mount(entry, plan_mount(plan)?, mount_probe)
        .map_err(|error| format!("{label} crosses or changed its mount boundary: {error}"))
}

fn inspect_planned_file(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
    target: &RemovalTarget,
) -> Result<Option<InspectedEntry>, String> {
    let relative = validate_relative(&target.relative, "state-removal target")?;
    let Some((entry, evidence)) = inspector
        .inspect_file_with(&relative, u64::MAX, |file| {
            file.metadata()
                .map(|metadata| RegularFileEvidence::from_metadata(&metadata))
        })
        .map_err(|error| {
            format!(
                "inspect confirmed state-removal target {}: {error}",
                target.relative
            )
        })?
    else {
        return Ok(None);
    };
    if entry.kind != InspectedKind::RegularFile {
        return Err(format!(
            "confirmed state-removal target {} is no longer a regular file",
            target.relative
        ));
    }
    require_safe_entry(&entry, "confirmed state file")?;
    require_plan_mount(&entry, plan, "confirmed state-removal target")?;
    if !target.evidence.matches_entry(&entry) || target.evidence != evidence {
        return Err(format!(
            "confirmed state-removal target {} changed after confirmation; Cyclops left it in place",
            target.relative
        ));
    }
    Ok(Some(entry))
}

fn remove_plan_files(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
) -> Result<RemovalProgress, (RemovalProgress, String)> {
    let mut progress = RemovalProgress::default();
    for target in &plan.targets {
        let entry = match inspect_planned_file(inspector, plan, target) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                if let Err(error) = progress.already_absent(target.bytes) {
                    return Err((progress, error));
                }
                continue;
            }
            Err(error) => return Err((progress, error)),
        };
        let bound =
            match inspector.bind_regular_file_for_removal_with_evidence(&entry, &target.evidence) {
                Ok(bound) => bound,
                Err(error) => {
                    return Err((
                        progress,
                        format!(
                            "bind confirmed state-removal target {}: {error}",
                            target.relative
                        ),
                    ));
                }
            };
        match bound.try_lock() {
            Ok(true) => {}
            Ok(false) => {
                return Err((
                    progress,
                    format!(
                        "confirmed state-removal target {} is currently locked by a writer; Cyclops left it in the tombstone",
                        target.relative
                    ),
                ));
            }
            Err(error) => {
                return Err((
                    progress,
                    format!(
                        "lock confirmed state-removal target {}: {error}",
                        target.relative
                    ),
                ));
            }
        }
        // The final temporal recheck in `BoundStateRemoval::remove` happens
        // after this cooperative writer lease is held. Tests use this seam to
        // prove that an in-place same-size rewrite cannot slip through the
        // inspection-to-unlink interval.
        state_remove_boundary(RemovalBoundary::TargetBound, inspector.path());
        if let Err(error) = bound.remove() {
            return Err((
                progress,
                format!(
                    "remove confirmed state-removal target {}: {error}",
                    target.relative
                ),
            ));
        }
        if let Err(error) = progress.removed(target.bytes) {
            return Err((progress, error));
        }
        state_remove_boundary(RemovalBoundary::TargetRemoved, inspector.path());
    }
    Ok(progress)
}

fn remove_plan_sockets(
    root: &StateRoot,
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
) -> Result<(), String> {
    if plan.sockets.is_empty() {
        return Ok(());
    }
    let snapshot = inspector
        .inspect_root(
            InspectionLimits::new(
                cyclops_state::INSPECTION_ENTRY_LIMIT_MAX,
                cyclops_state::INSPECTION_NAME_BYTES_LIMIT_MAX,
            )
            .expect("state-root socket inspection bounds fit hard ceilings"),
        )
        .map_err(|error| format!("inspect confirmed state socket: {error}"))?;
    require_plan_mount(&snapshot.directory, plan, "confirmed state root")?;
    for target in &plan.sockets {
        let Some(expected) = snapshot
            .entries
            .iter()
            .find(|entry| entry.path.file_name() == Some(OsStr::new(&target.relative)))
        else {
            // A crash may happen immediately after unlinking this socket and
            // before the durable TargetsRemoved checkpoint. It is a
            // zero-body, zero-byte planned entry, so an absent socket during
            // tombstone recovery is safely treated as already removed.
            continue;
        };
        if expected.kind != InspectedKind::Socket || !target.evidence.matches(expected) {
            return Err(format!(
                "confirmed state socket {} changed after confirmation; Cyclops left it in the tombstone",
                target.relative
            ));
        }
        require_plan_mount(expected, plan, "confirmed state socket")?;
        root.bind_root_socket_for_removal(expected)
            .map_err(|error| format!("bind confirmed state socket {}: {error}", target.relative))?
            .remove()
            .map_err(|error| {
                format!("remove confirmed state socket {}: {error}", target.relative)
            })?;
        state_remove_boundary(RemovalBoundary::SocketRemoved, inspector.path());
    }
    Ok(())
}

fn remove_empty_directories(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
) -> Result<(), String> {
    let mut directories = plan.directories.clone();
    directories.sort_by(|left, right| {
        right
            .relative
            .matches('/')
            .count()
            .cmp(&left.relative.matches('/').count())
            .then_with(|| right.relative.cmp(&left.relative))
    });
    for directory in directories {
        let relative = validate_relative(&directory.relative, "state-removal directory")?;
        let Some(snapshot) = inspector
            .inspect_directory(
                &relative,
                InspectionLimits::new(1, cyclops_state::INSPECTION_NAME_BYTES_LIMIT_MAX)
                    .expect("empty-directory inspection bounds fit hard ceilings"),
            )
            .map_err(|error| {
                format!(
                    "inspect confirmed state directory {}: {error}",
                    directory.relative
                )
            })?
        else {
            continue;
        };
        require_plan_mount(
            &snapshot.directory,
            plan,
            "confirmed state-removal directory",
        )?;
        if !directory.evidence.matches(&snapshot.directory) {
            return Err(format!(
                "confirmed state directory {} changed after confirmation; Cyclops left the tombstone for recovery",
                directory.relative
            ));
        }
        if snapshot.truncated || !snapshot.entries.is_empty() {
            return Err(format!(
                "confirmed state directory {} is no longer empty; Cyclops left the tombstone for recovery",
                directory.relative
            ));
        }
        inspector
            .bind_empty_directory_for_removal(&snapshot.directory)
            .map_err(|error| format!("bind empty state directory {}: {error}", directory.relative))?
            .remove()
            .map_err(|error| {
                format!(
                    "remove empty state directory {}: {error}",
                    directory.relative
                )
            })?;
    }
    Ok(())
}

fn remove_operation_files(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
    paths: &[&str],
) -> Result<(), String> {
    remove_operation_files_with_mount_probe(inspector, plan, paths, cleanup::mount_identity)
}

fn remove_operation_files_with_mount_probe<F>(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
    paths: &[&str],
    mut mount_probe: F,
) -> Result<(), String>
where
    F: FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
{
    for path in paths {
        let relative = Path::new(path);
        let Some((entry, ())) = inspector
            .inspect_file_with(relative, u64::MAX, |_| Ok(()))
            .map_err(|error| format!("inspect state-removal infrastructure {path}: {error}"))?
        else {
            continue;
        };
        if entry.kind != InspectedKind::RegularFile {
            return Err(format!(
                "state-removal infrastructure {path} is no longer a regular file"
            ));
        }
        require_plan_mount_with(
            &entry,
            plan,
            "state-removal operation infrastructure",
            &mut mount_probe,
        )?;
        inspector
            .bind_regular_file_for_removal(&entry)
            .map_err(|error| format!("bind state-removal infrastructure {path}: {error}"))?
            .remove()
            .map_err(|error| format!("remove state-removal infrastructure {path}: {error}"))?;
    }
    Ok(())
}

fn remove_operations_directory(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
) -> Result<(), String> {
    remove_operations_directory_with_mount_probe(inspector, plan, cleanup::mount_identity)
}

fn remove_operations_directory_with_mount_probe<F>(
    inspector: &StateInspector,
    plan: &StateRemovalPlan,
    mut mount_probe: F,
) -> Result<(), String>
where
    F: FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
{
    let Some(snapshot) = inspector
        .inspect_directory(
            Path::new("operations"),
            InspectionLimits::new(1, cyclops_state::INSPECTION_NAME_BYTES_LIMIT_MAX)
                .expect("operations inspection bounds fit hard ceilings"),
        )
        .map_err(|error| format!("inspect state-removal operation directory: {error}"))?
    else {
        return Ok(());
    };
    if snapshot.truncated || !snapshot.entries.is_empty() {
        return Err(
            "state-removal operation directory contains an unplanned entry; Cyclops left the tombstone for recovery"
                .into(),
        );
    }
    require_plan_mount_with(
        &snapshot.directory,
        plan,
        "state-removal operation directory",
        &mut mount_probe,
    )?;
    inspector
        .bind_empty_directory_for_removal(&snapshot.directory)
        .map_err(|error| format!("bind empty state-removal operation directory: {error}"))?
        .remove()
        .map_err(|error| format!("remove empty state-removal operation directory: {error}"))
}

fn write_checkpoint(root: &StateRoot, checkpoint: &StateRemovalCheckpoint) -> Result<(), String> {
    validate_checkpoint(checkpoint)?;
    let mut bytes = serde_json::to_vec(checkpoint)
        .map_err(|error| format!("serialize state-removal checkpoint: {error}"))?;
    if bytes.len() > CHECKPOINT_BYTES_LIMIT {
        return Err(format!(
            "state-removal checkpoint exceeds its {}-byte bound",
            CHECKPOINT_BYTES_LIMIT
        ));
    }
    bytes.push(b'\n');
    root.replace_file(Path::new(CHECKPOINT), &bytes)
        .map_err(|error| format!("write state-removal checkpoint: {error}"))
}

fn read_checkpoint(
    inspector: &StateInspector,
    home: &Path,
    tombstone: &Path,
) -> Result<Option<StateRemovalCheckpoint>, String> {
    let Some(file) = inspector
        .read_file(Path::new(CHECKPOINT), CHECKPOINT_BYTES_LIMIT)
        .map_err(|error| format!("read state-removal checkpoint: {error}"))?
    else {
        return Ok(None);
    };
    if file.truncated {
        return Err("state-removal checkpoint exceeds its byte bound".into());
    }
    let checkpoint: StateRemovalCheckpoint = serde_json::from_slice(&file.bytes)
        .map_err(|error| format!("parse state-removal checkpoint: {error}"))?;
    validate_checkpoint(&checkpoint)?;
    let (_, plan) = checkpoint_parts_ref(&checkpoint);
    if plan.state_root != home.display().to_string()
        || plan.tombstone != tombstone.display().to_string()
    {
        return Err(
            "state-removal checkpoint names a different state home or tombstone; Cyclops will not resume it"
                .into(),
        );
    }
    Ok(Some(checkpoint))
}

fn validate_checkpoint(checkpoint: &StateRemovalCheckpoint) -> Result<(), String> {
    match checkpoint {
        StateRemovalCheckpoint::Pending {
            schema,
            operation_id,
            plan,
        } => {
            if *schema != STATE_REMOVE_SCHEMA {
                return Err("state-removal checkpoint has an unsupported schema".into());
            }
            validate_operation_id(operation_id)?;
            validate_plan(plan)
        }
        StateRemovalCheckpoint::TargetsRemoved {
            schema,
            operation_id,
            plan,
            removed_files,
            removed_bytes,
            already_absent_files,
            already_absent_bytes,
        } => {
            if *schema != STATE_REMOVE_SCHEMA {
                return Err("state-removal checkpoint has an unsupported schema".into());
            }
            validate_operation_id(operation_id)?;
            validate_plan(plan)?;
            if removed_files.checked_add(*already_absent_files) != Some(plan.files)
                || removed_bytes.checked_add(*already_absent_bytes) != Some(plan.bytes)
            {
                return Err("state-removal checkpoint has invalid recovery totals".into());
            }
            Ok(())
        }
    }
}

fn checkpoint_parts(checkpoint: StateRemovalCheckpoint) -> (String, StateRemovalPlan) {
    match checkpoint {
        StateRemovalCheckpoint::Pending {
            operation_id, plan, ..
        }
        | StateRemovalCheckpoint::TargetsRemoved {
            operation_id, plan, ..
        } => (operation_id, plan),
    }
}

fn checkpoint_parts_ref(checkpoint: &StateRemovalCheckpoint) -> (&str, &StateRemovalPlan) {
    match checkpoint {
        StateRemovalCheckpoint::Pending {
            operation_id, plan, ..
        }
        | StateRemovalCheckpoint::TargetsRemoved {
            operation_id, plan, ..
        } => (operation_id, plan),
    }
}

fn next_operation_id() -> String {
    let sequence = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
    format!(
        "state-remove-{}-{}-{sequence}",
        std::process::id(),
        crate::render::now_ms()
    )
}

fn validate_operation_id(operation_id: &str) -> Result<(), String> {
    if operation_id.len() > 128
        || !operation_id.starts_with("state-remove-")
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("state-removal checkpoint has an invalid operation id".into());
    }
    Ok(())
}

fn render_json(report: &RemovalReport) -> Value {
    let plan = report.plan.as_ref();
    let recovery = report.recovery_guidance.json();
    json!({
        "schema": STATE_REMOVE_SCHEMA,
        "mode": report.mode,
        "state": report.state.name(),
        "scope": SCOPE,
        "preserved": PRESERVED,
        "state_root": plan.map(|plan| &plan.state_root),
        "tombstone": plan.map(|plan| &plan.tombstone),
        "files": plan.map(|plan| plan.files).unwrap_or(0),
        "directories": plan.map(|plan| plan.directories.iter().map(|directory| json!({
            "path": directory.relative,
        })).collect::<Vec<_>>()).unwrap_or_default(),
        "bytes": plan.map(|plan| plan.bytes).unwrap_or(0),
        "targets": plan.map(|plan| plan.targets.iter().map(|target| json!({
            "path": target.relative,
            "bytes": target.bytes,
        })).collect::<Vec<_>>()).unwrap_or_default(),
        "sockets": plan.map(|plan| plan.sockets.iter().map(|socket| json!({
            "path": socket.relative,
        })).collect::<Vec<_>>()).unwrap_or_default(),
        "confirmation": report.confirmation(),
        "operation_id": report.operation_id,
        "removed_files": report.progress.removed_files,
        "removed_bytes": report.progress.removed_bytes,
        "already_absent_files": report.progress.already_absent_files,
        "already_absent_bytes": report.progress.already_absent_bytes,
        "checkpoint": CHECKPOINT,
        "recovery": recovery,
        "next_step": (report.state == RemovalState::StateRemovalCompleted).then_some(json!({
            "command": INSTALLER_UNINSTALL_COMMAND,
            "scope": NEXT_STEP_SCOPE,
        })),
        "error": report.error,
    })
}

fn render_plain(report: &RemovalReport) -> String {
    let mut lines = vec![format!("complete state removal {}", report.state.name())];
    if let Some(plan) = &report.plan {
        lines.push(format!("  scope       {SCOPE}"));
        lines.push(format!("  state home  {}", plan.state_root));
        lines.push(format!(
            "  selected    {} files · {} directories · {} sockets · {} bytes",
            plan.files,
            plan.directories.len(),
            plan.sockets.len(),
            plan.bytes
        ));
        for target in &plan.targets {
            lines.push(format!("    {} · {} bytes", target.relative, target.bytes));
        }
        for directory in &plan.directories {
            lines.push(format!("    {}/", directory.relative));
        }
        for socket in &plan.sockets {
            lines.push(format!("    {} · socket", socket.relative));
        }
        if let Some(confirmation) = report.confirmation() {
            lines.push(format!(
                "  confirm     cyclops remove --all --confirm {}",
                confirmation
            ));
        }
    }
    lines.push(format!("  preserved   {PRESERVED}"));
    if let Some(operation_id) = &report.operation_id {
        lines.push(format!("  operation   {operation_id}"));
    }
    if report.progress.removed_files > 0 {
        lines.push(format!(
            "  removed     {} files · {} bytes",
            report.progress.removed_files, report.progress.removed_bytes
        ));
    }
    if report.progress.already_absent_files > 0 {
        lines.push(format!(
            "  recovered   {} planned files were already absent · {} bytes",
            report.progress.already_absent_files, report.progress.already_absent_bytes
        ));
    }
    match report.state {
        RemovalState::Preview => lines.push(
            "  next        export anything you need, keep cyclopsd stopped, then use the exact confirmation"
                .into(),
        ),
        RemovalState::StateRemovalCompleted => lines.push(format!(
            "  next        {INSTALLER_UNINSTALL_COMMAND} ({NEXT_STEP_SCOPE})"
        )),
        RemovalState::AlreadyEmpty => lines.push("  result      the current Cyclops state home is absent".into()),
        RemovalState::ConfirmationRequired
        | RemovalState::RecoveryRequired
        | RemovalState::Partial
        | RemovalState::Refused => {}
    }
    if let Some(error) = &report.error {
        let outcome = if report.state == RemovalState::Partial {
            "partial"
        } else {
            "refused"
        };
        lines.push(format!("  {outcome:<11}{error}"));
        if let Some(recovery) = report.recovery_guidance.plain() {
            lines.push(format!("  recovery    {recovery}"));
        }
    }
    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemovalBoundary {
    PendingWritten,
    Isolated,
    TargetBound,
    TargetRemoved,
    SocketRemoved,
    TargetsRemoved,
    OperationFilesRemoved,
    OperationsDirectoryRemoved,
    TombstoneRemoved,
}

#[cfg(test)]
type RemovalBoundaryAction = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    static REMOVE_CRASH_BOUNDARY: std::cell::Cell<Option<RemovalBoundary>> = const { std::cell::Cell::new(None) };
    static REMOVE_AFTER_ISOLATION: std::cell::RefCell<Option<RemovalBoundaryAction>> = const { std::cell::RefCell::new(None) };
    static REMOVE_AFTER_TARGET_BOUND: std::cell::RefCell<Option<RemovalBoundaryAction>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn state_remove_boundary(boundary: RemovalBoundary, path: &Path) {
    if boundary == RemovalBoundary::Isolated {
        REMOVE_AFTER_ISOLATION.with(|slot| {
            if let Some(action) = slot.borrow_mut().take() {
                action(path);
            }
        });
    }
    if boundary == RemovalBoundary::TargetBound {
        REMOVE_AFTER_TARGET_BOUND.with(|slot| {
            if let Some(action) = slot.borrow_mut().take() {
                action(path);
            }
        });
    }
    REMOVE_CRASH_BOUNDARY.with(|slot| {
        if slot.get() == Some(boundary) {
            slot.set(None);
            panic!("injected complete state-removal crash at {boundary:?}");
        }
    });
}

#[cfg(not(test))]
#[derive(Clone, Copy)]
enum RemovalBoundary {
    PendingWritten,
    Isolated,
    TargetBound,
    TargetRemoved,
    SocketRemoved,
    TargetsRemoved,
    OperationFilesRemoved,
    OperationsDirectoryRemoved,
    TombstoneRemoved,
}

#[cfg(not(test))]
fn state_remove_boundary(_: RemovalBoundary, _: &Path) {}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and returns this process's uid.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{symlink, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    const PRIVATE_FILE_MODE: u32 = 0o600;

    fn scratch() -> tempfile::TempDir {
        let root = cyclops_proto::scratch::scratch_root();
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("cyclops-state-remove-")
            .tempdir_in(root)
            .unwrap()
    }

    fn private_directory(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE)).unwrap();
    }

    fn state_file(path: &Path, bytes: &[u8]) {
        private_directory(path.parent().unwrap());
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(PRIVATE_FILE_MODE)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    /// Change content without changing its length and set a deliberately
    /// distinct mtime. This avoids timing sleeps while exercising the
    /// preview-to-unlink temporal-evidence check.
    fn rewrite_same_size(path: &Path, bytes: &[u8]) {
        assert_eq!(fs::metadata(path).unwrap().len(), bytes.len() as u64);
        fs::write(path, bytes).unwrap();
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let times = [
            libc::timespec {
                tv_sec: 1,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: 2,
                tv_nsec: 0,
            },
        ];
        // SAFETY: `path` is a valid C string and `times` has the two entries
        // required by utimensat. This test-only helper uses an explicit clock
        // value instead of relying on filesystem timestamp granularity.
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
            0,
            "set deterministic test mtime: {}",
            std::io::Error::last_os_error()
        );
    }

    fn confirmation(home: &Path) -> String {
        let report = preview_at(home);
        assert_eq!(report.state, RemovalState::Preview, "{report:?}");
        report.plan.unwrap().confirmation
    }

    #[test]
    fn preview_is_body_free_and_complete_removal_leaves_unrelated_files_alone() {
        let root = scratch();
        let home = root.path().join("home");
        let outside = root.path().join("outside");
        private_directory(&home);
        private_directory(&outside);
        state_file(&home.join("config.toml"), b"theme = \"light\"\n");
        state_file(
            &home.join("workspaces/main/messages.ndjson"),
            b"{\"body\":\"not-in-plan\"}\n",
        );
        state_file(&outside.join("keep"), b"keep\n");

        let report = preview_at(&home);
        assert_eq!(report.state, RemovalState::Preview, "{report:?}");
        let preview_json = render_json(&report);
        assert!(preview_json["recovery"].is_null());
        let rendered = preview_json.to_string();
        assert!(!rendered.contains("not-in-plan"));
        assert!(!home.join(CHECKPOINT).exists());

        let report = apply_at_with(&home, &report.plan.unwrap().confirmation, || Ok(()));
        assert_eq!(
            report.state,
            RemovalState::StateRemovalCompleted,
            "{report:?}"
        );
        assert!(render_json(&report)["recovery"].is_null());
        assert!(!home.exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep\n");
    }

    #[test]
    fn crash_after_isolation_keeps_a_pending_body_free_checkpoint_and_recovers() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(
            &home.join("workspaces/main/messages.ndjson"),
            b"{\"body\":\"only-in-state\"}\n",
        );
        let confirmation = confirmation(&home);

        REMOVE_CRASH_BOUNDARY.with(|slot| slot.set(Some(RemovalBoundary::Isolated)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let tombstone = tombstone_path(&home).unwrap();
        let checkpoint = fs::read_to_string(tombstone.join(CHECKPOINT)).unwrap();
        assert!(!checkpoint.contains("only-in-state"));
        let pending = preview_at(&home);
        assert_eq!(pending.state, RemovalState::RecoveryRequired, "{pending:?}");

        let resumed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(
            resumed.state,
            RemovalState::StateRemovalCompleted,
            "{resumed:?}"
        );
        assert!(!home.exists());
        assert!(!tombstone.exists());
    }

    #[test]
    fn pending_checkpoint_refuses_an_entry_added_before_isolation() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"before\n");
        let confirmation = confirmation(&home);

        REMOVE_CRASH_BOUNDARY.with(|slot| slot.set(Some(RemovalBoundary::PendingWritten)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let tombstone = tombstone_path(&home).unwrap();
        assert!(home.join(CHECKPOINT).exists());
        assert!(!tombstone.exists());
        state_file(&home.join("added-after-confirmation"), b"keep\n");

        let report = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(report.state, RemovalState::Partial, "{report:?}");
        assert!(report
            .error
            .as_deref()
            .is_some_and(|error| { error.contains("did not isolate unpreviewed entries") }));
        assert_eq!(
            fs::read(home.join("added-after-confirmation")).unwrap(),
            b"keep\n"
        );
        assert!(home.exists());
        assert!(!tombstone.exists());
    }

    #[test]
    fn source_pending_checkpoint_resumes_the_original_confirmation() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"before\n");
        let confirmation = confirmation(&home);

        REMOVE_CRASH_BOUNDARY.with(|slot| slot.set(Some(RemovalBoundary::PendingWritten)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let pending = preview_at(&home);
        assert_eq!(pending.state, RemovalState::RecoveryRequired, "{pending:?}");
        assert_eq!(pending.confirmation(), Some(confirmation.as_str()));
        let resumed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(
            resumed.state,
            RemovalState::StateRemovalCompleted,
            "{resumed:?}"
        );
        assert!(!home.exists());
        assert!(!tombstone_path(&home).unwrap().exists());
    }

    #[test]
    fn same_size_rewrite_after_binding_is_refused_before_unlink() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"before");
        let confirmation = confirmation(&home);

        REMOVE_AFTER_TARGET_BOUND.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|tombstone| {
                rewrite_same_size(&tombstone.join("config.toml"), b"after!");
            }));
        });
        let report = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(report.state, RemovalState::Partial, "{report:?}");
        assert!(report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("changed after its preview")));
        let tombstone = tombstone_path(&home).unwrap();
        assert_eq!(fs::read(tombstone.join("config.toml")).unwrap(), b"after!");
        assert!(!home.exists());
    }

    #[test]
    fn crash_after_one_regular_target_resumes_with_observable_absence() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("a.toml"), b"first\n");
        state_file(&home.join("b.toml"), b"second\n");
        let confirmation = confirmation(&home);

        REMOVE_CRASH_BOUNDARY.with(|slot| slot.set(Some(RemovalBoundary::TargetRemoved)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let tombstone = tombstone_path(&home).unwrap();
        assert!(!tombstone.join("a.toml").exists());
        assert!(tombstone.join("b.toml").exists());
        let resumed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(
            resumed.state,
            RemovalState::StateRemovalCompleted,
            "{resumed:?}"
        );
        assert_eq!(resumed.progress.already_absent_files, 1);
        assert_eq!(resumed.progress.removed_files, 1);
        assert!(!home.exists());
        assert!(!tombstone.exists());
    }

    #[test]
    fn targets_removed_checkpoint_resumes_only_finalization() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"remove\n");
        let confirmation = confirmation(&home);

        REMOVE_CRASH_BOUNDARY.with(|slot| slot.set(Some(RemovalBoundary::TargetsRemoved)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let tombstone = tombstone_path(&home).unwrap();
        assert!(!tombstone.join("config.toml").exists());
        let checkpoint = fs::read_to_string(tombstone.join(CHECKPOINT)).unwrap();
        assert!(checkpoint.contains("targets_removed"));
        let resumed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(
            resumed.state,
            RemovalState::StateRemovalCompleted,
            "{resumed:?}"
        );
        assert_eq!(resumed.progress.removed_files, 1);
        assert_eq!(resumed.progress.already_absent_files, 0);
        assert!(!home.exists());
        assert!(!tombstone.exists());
    }

    #[test]
    fn a_new_home_after_isolation_is_left_untouched_and_reported_partial() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"before\n");
        let confirmation = confirmation(&home);

        let replacement = home.clone();
        REMOVE_AFTER_ISOLATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |_| {
                private_directory(&replacement);
                state_file(&replacement.join("new-state"), b"keep\n");
            }));
        });
        let report = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(report.state, RemovalState::Partial, "{report:?}");
        assert!(report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("new state home appeared")));
        assert_eq!(fs::read(home.join("new-state")).unwrap(), b"keep\n");
        assert!(!tombstone_path(&home).unwrap().exists());
        let rendered = render_json(&report);
        assert!(rendered["recovery"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("preview the retained state")));
        assert!(render_plain(&report).contains("inspect retained state"));
    }

    #[test]
    fn a_stale_root_socket_is_bound_to_the_preview_before_removal() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"keep\n");
        let socket_path = home.join(cyclops_proto::SOCK_NAME);
        let socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(socket);

        let preview = preview_at(&home);
        assert_eq!(preview.state, RemovalState::Preview, "{preview:?}");
        assert_eq!(preview.plan.as_ref().unwrap().sockets.len(), 1);
        let confirmation = preview.plan.unwrap().confirmation;

        let removed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(
            removed.state,
            RemovalState::StateRemovalCompleted,
            "{removed:?}"
        );
        assert!(!home.exists());
    }

    #[test]
    fn crash_after_socket_unlink_recovers_the_original_confirmation() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"keep\n");
        let socket_path = home.join(cyclops_proto::SOCK_NAME);
        let socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(socket);
        let confirmation = confirmation(&home);

        REMOVE_CRASH_BOUNDARY.with(|slot| slot.set(Some(RemovalBoundary::SocketRemoved)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let tombstone = tombstone_path(&home).unwrap();
        assert!(!tombstone.join(cyclops_proto::SOCK_NAME).exists());
        let recovery = preview_at(&home);
        assert_eq!(
            recovery.state,
            RemovalState::RecoveryRequired,
            "{recovery:?}"
        );
        assert_eq!(recovery.confirmation(), Some(confirmation.as_str()));

        let resumed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(
            resumed.state,
            RemovalState::StateRemovalCompleted,
            "{resumed:?}"
        );
        assert!(!home.exists());
        assert!(!tombstone.exists());
    }

    #[test]
    fn checkpoint_free_finalization_tombstone_is_left_for_manual_recovery() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"remove\n");
        let confirmation = confirmation(&home);

        REMOVE_CRASH_BOUNDARY.with(|slot| {
            slot.set(Some(RemovalBoundary::OperationFilesRemoved));
        });
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let tombstone = tombstone_path(&home).unwrap();
        assert!(tombstone.exists());
        assert!(!tombstone.join(CHECKPOINT).exists());
        let displaced = root.path().join("interrupted-original");
        fs::rename(&tombstone, &displaced).unwrap();
        private_directory(&tombstone);
        let replacement_inode = fs::metadata(&tombstone).unwrap().ino();

        let recovery = preview_at(&home);
        assert_eq!(recovery.state, RemovalState::Refused, "{recovery:?}");
        assert!(recovery.plan.is_none());
        assert!(recovery
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no durable checkpoint")));
        let rendered = render_json(&recovery);
        assert!(rendered["confirmation"].is_null());
        assert!(
            rendered["recovery"].as_str().is_some_and(
                |guidance| guidance.contains("no later cyclops remove can safely resume")
            )
        );
        assert!(render_plain(&recovery).contains("do not delete it because of its name"));

        let resumed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(resumed.state, RemovalState::Refused, "{resumed:?}");
        assert!(!home.exists());
        assert!(tombstone.exists());
        assert_eq!(fs::metadata(&tombstone).unwrap().ino(), replacement_inode);
        assert!(displaced.exists());
    }

    #[test]
    fn a_forged_checkpoint_cannot_authorize_a_replacement_tombstone() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"keep\n");
        let plan = preview_at(&home).plan.unwrap();
        let tombstone = tombstone_path(&home).unwrap();
        private_directory(&tombstone);
        let forged = StateRoot::open_existing(&tombstone).unwrap().unwrap();
        write_checkpoint(
            &forged,
            &StateRemovalCheckpoint::Pending {
                schema: STATE_REMOVE_SCHEMA,
                operation_id: "state-remove-forged".into(),
                plan,
            },
        )
        .unwrap();

        let report = preview_at(&home);
        assert_eq!(report.state, RemovalState::Refused, "{report:?}");
        assert!(report.error.as_deref().is_some_and(|error| {
            error.contains("does not name the exact root captured before isolation")
        }));
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), b"keep\n");
        assert!(tombstone.join(CHECKPOINT).exists());
    }

    #[test]
    fn symlinks_and_hard_links_refuse_complete_removal_without_touching_external_data() {
        let root = scratch();
        let home = root.path().join("home");
        let external = root.path().join("external");
        private_directory(&home);
        state_file(&external, b"outside\n");
        symlink(&external, home.join("linked")).unwrap();

        let linked = preview_at(&home);
        assert_eq!(linked.state, RemovalState::Refused, "{linked:?}");
        assert!(linked
            .error
            .as_deref()
            .is_some_and(|error| error.contains("symbolic link")));
        assert_eq!(fs::read(&external).unwrap(), b"outside\n");
        assert!(home.join("linked").is_symlink());

        fs::remove_file(home.join("linked")).unwrap();
        fs::hard_link(&external, home.join("shared")).unwrap();
        let shared = preview_at(&home);
        assert_eq!(shared.state, RemovalState::Refused, "{shared:?}");
        assert!(shared
            .error
            .as_deref()
            .is_some_and(|error| error.contains("multiple hard links")));
        assert_eq!(fs::read(&external).unwrap(), b"outside\n");
        assert!(home.join("shared").exists());
    }

    #[test]
    fn mount_boundaries_are_refused_during_preview_and_operation_finalization() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("nested/record"), b"record\n");
        let tombstone = tombstone_path(&home).unwrap();
        let inspector = open_private_inspector(&home).unwrap().unwrap();

        let preview_error =
            plan_from_inspector_with_mount_probe(&inspector, &home, &tombstone, |entry| {
                let mut identity = cleanup::mount_identity(entry)?;
                if entry.path.file_name() == Some(OsStr::new("nested")) {
                    identity.mount_id = identity.mount_id.wrapping_add(1);
                }
                Ok(identity)
            })
            .unwrap_err();
        assert!(preview_error.contains("mount boundary"));

        let plan = plan_from_inspector(&inspector, &home, &tombstone).unwrap();
        drop(inspector);
        let state = StateRoot::open_existing(&home).unwrap().unwrap();
        write_checkpoint(
            &state,
            &StateRemovalCheckpoint::Pending {
                schema: STATE_REMOVE_SCHEMA,
                operation_id: "state-remove-mount-test".into(),
                plan: plan.clone(),
            },
        )
        .unwrap();
        let inspector = state.inspector().unwrap();

        let operation_file_error =
            remove_operation_files_with_mount_probe(&inspector, &plan, &[CHECKPOINT], |entry| {
                let mut identity = cleanup::mount_identity(entry)?;
                identity.mount_id = identity.mount_id.wrapping_add(1);
                Ok(identity)
            })
            .unwrap_err();
        assert!(operation_file_error.contains("mount boundary"));
        assert!(home.join(CHECKPOINT).exists());

        remove_operation_files(&inspector, &plan, &[CHECKPOINT]).unwrap();
        let operation_directory_error =
            remove_operations_directory_with_mount_probe(&inspector, &plan, |entry| {
                let mut identity = cleanup::mount_identity(entry)?;
                identity.mount_id = identity.mount_id.wrapping_add(1);
                Ok(identity)
            })
            .unwrap_err();
        assert!(operation_directory_error.contains("mount boundary"));
        assert!(home.join("operations").exists());
    }

    #[test]
    fn a_changed_confirmation_never_writes_a_checkpoint() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        state_file(&home.join("config.toml"), b"keep\n");

        let report = apply_at_with(&home, "remove-cyclops-state:not-this-plan", || Ok(()));
        assert_eq!(
            report.state,
            RemovalState::ConfirmationRequired,
            "{report:?}"
        );
        assert!(!home.join(CHECKPOINT).exists());
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), b"keep\n");
    }
}
