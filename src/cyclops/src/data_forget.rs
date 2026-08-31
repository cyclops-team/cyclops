//! Explicit removal of the durable records `cyclops data inventory` reports.
//!
//! This is deliberately narrower than uninstalling Cyclops. It removes the
//! append-only workspace and session journals only after a confirmation from a
//! preview made while the daemon is stopped. Preferences, layouts, setup,
//! logs, sockets, managed assets, installed binaries, and vendor configuration
//! remain outside the scope.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cyclops_state::{InspectedEntry, InspectedKind, StateInspector, StateRoot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::client::{Client, ClientError};
use crate::data::{self, RecordEvidence};

const FORGET_SCHEMA: u32 = 1;
const CHECKPOINT: &str = "operations/data-forget.json";
const LEASE: &str = cyclops_proto::DURABLE_RECORD_FORGET_LEASE;
const CHECKPOINT_BYTES_LIMIT: usize = cyclops_state::INSPECTION_FILE_BYTES_LIMIT_MAX;
const CONFIRMATION_PREFIX: &str = "forget-durable-records:";
const SCOPE: &str =
    "all workspace and session NDJSON journals in the current durable-record inventory";
const PRESERVED: &str = "preferences, layouts, setup files, logs, sockets, managed assets, installed binaries, and vendor configuration";

static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ForgetTarget {
    relative: String,
    bytes: u64,
    evidence: RecordEvidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ForgetPlan {
    schema: u32,
    state_root: String,
    files: usize,
    bytes: u64,
    targets: Vec<ForgetTarget>,
    confirmation: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "state", rename_all = "snake_case")]
enum ForgetCheckpoint {
    Pending {
        schema: u32,
        operation_id: String,
        plan: ForgetPlan,
    },
    Completed {
        schema: u32,
        operation_id: String,
        plan: ForgetPlan,
        removed_files: usize,
        removed_bytes: u64,
        already_absent_files: usize,
        already_absent_bytes: u64,
    },
}

/// What this invocation could observe about each planned file.
///
/// An already-absent file is not credited to the current invocation. It can
/// be the result of an interrupted earlier attempt, or an external removal;
/// the final report keeps that uncertainty explicit.
#[derive(Clone, Copy, Debug, Default)]
struct RemovalProgress {
    removed_files: usize,
    removed_bytes: u64,
    already_absent_files: usize,
    already_absent_bytes: u64,
}

impl RemovalProgress {
    fn removed(&mut self, bytes: u64) -> Result<(), String> {
        self.removed_files = self
            .removed_files
            .checked_add(1)
            .ok_or_else(|| "durable-record removal file count overflowed".to_string())?;
        self.removed_bytes = self
            .removed_bytes
            .checked_add(bytes)
            .ok_or_else(|| "durable-record removal byte count overflowed".to_string())?;
        Ok(())
    }

    fn already_absent(&mut self, bytes: u64) -> Result<(), String> {
        self.already_absent_files = self
            .already_absent_files
            .checked_add(1)
            .ok_or_else(|| "durable-record recovery file count overflowed".to_string())?;
        self.already_absent_bytes = self
            .already_absent_bytes
            .checked_add(bytes)
            .ok_or_else(|| "durable-record recovery byte count overflowed".to_string())?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForgetState {
    Preview,
    ConfirmationRequired,
    RecoveryRequired,
    Completed,
    AlreadyEmpty,
    Partial,
    Refused,
}

impl ForgetState {
    fn name(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::ConfirmationRequired => "confirmation_required",
            Self::RecoveryRequired => "recovery_required",
            Self::Completed => "completed",
            Self::AlreadyEmpty => "already_empty",
            Self::Partial => "partial",
            Self::Refused => "refused",
        }
    }
}

#[derive(Debug)]
struct ForgetReport {
    mode: &'static str,
    state: ForgetState,
    plan: Option<ForgetPlan>,
    operation_id: Option<String>,
    removed_files: usize,
    removed_bytes: u64,
    already_absent_files: usize,
    already_absent_bytes: u64,
    error: Option<String>,
}

impl ForgetReport {
    fn preview(plan: ForgetPlan) -> Self {
        Self {
            mode: "preview",
            state: ForgetState::Preview,
            plan: Some(plan),
            operation_id: None,
            removed_files: 0,
            removed_bytes: 0,
            already_absent_files: 0,
            already_absent_bytes: 0,
            error: None,
        }
    }

    fn confirmation_required(plan: ForgetPlan, error: String) -> Self {
        Self {
            mode: "preview",
            state: ForgetState::ConfirmationRequired,
            plan: Some(plan),
            operation_id: None,
            removed_files: 0,
            removed_bytes: 0,
            already_absent_files: 0,
            already_absent_bytes: 0,
            error: Some(error),
        }
    }

    fn recovery_required(plan: ForgetPlan, operation_id: String, error: String) -> Self {
        Self {
            mode: "recovery",
            state: ForgetState::RecoveryRequired,
            plan: Some(plan),
            operation_id: Some(operation_id),
            removed_files: 0,
            removed_bytes: 0,
            already_absent_files: 0,
            already_absent_bytes: 0,
            error: Some(error),
        }
    }

    fn completed(plan: ForgetPlan, operation_id: String, progress: RemovalProgress) -> Self {
        Self {
            mode: "apply",
            state: ForgetState::Completed,
            plan: Some(plan),
            operation_id: Some(operation_id),
            removed_files: progress.removed_files,
            removed_bytes: progress.removed_bytes,
            already_absent_files: progress.already_absent_files,
            already_absent_bytes: progress.already_absent_bytes,
            error: None,
        }
    }

    fn already_empty(root: &Path) -> Self {
        Self {
            mode: "preview",
            state: ForgetState::AlreadyEmpty,
            plan: Some(ForgetPlan {
                schema: FORGET_SCHEMA,
                state_root: root.display().to_string(),
                files: 0,
                bytes: 0,
                targets: Vec::new(),
                confirmation: String::new(),
            }),
            operation_id: None,
            removed_files: 0,
            removed_bytes: 0,
            already_absent_files: 0,
            already_absent_bytes: 0,
            error: None,
        }
    }

    fn partial(
        plan: ForgetPlan,
        operation_id: String,
        progress: RemovalProgress,
        error: String,
    ) -> Self {
        Self {
            mode: "apply",
            state: ForgetState::Partial,
            plan: Some(plan),
            operation_id: Some(operation_id),
            removed_files: progress.removed_files,
            removed_bytes: progress.removed_bytes,
            already_absent_files: progress.already_absent_files,
            already_absent_bytes: progress.already_absent_bytes,
            error: Some(error),
        }
    }

    fn refused(plan: ForgetPlan, error: String) -> Self {
        Self {
            mode: "preview",
            state: ForgetState::Refused,
            plan: Some(plan),
            operation_id: None,
            removed_files: 0,
            removed_bytes: 0,
            already_absent_files: 0,
            already_absent_bytes: 0,
            error: Some(error),
        }
    }

    fn ok(&self) -> bool {
        matches!(
            self.state,
            ForgetState::Preview | ForgetState::Completed | ForgetState::AlreadyEmpty
        )
    }
}

/// Preview the complete durable-record scope or, with its exact unchanged
/// token, remove it while the daemon remains stopped.
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

fn preview_at(home: &Path) -> ForgetReport {
    let source = match data::inspect_records(home) {
        Ok(source) => source,
        Err(error) => return failed_preview(home, error),
    };
    let Some(inspector) = source.inspector.as_ref() else {
        return ForgetReport::already_empty(home);
    };

    match read_checkpoint(inspector) {
        Ok(Some(ForgetCheckpoint::Pending {
            operation_id, plan, ..
        })) => ForgetReport::recovery_required(
            plan,
            operation_id,
            "a prior confirmed record removal did not record completion; rerun its exact confirmation to recover".into(),
        ),
        Ok(Some(ForgetCheckpoint::Completed { .. })) | Ok(None) => match plan_from_source(&source) {
            Ok(Some(plan)) => ForgetReport::preview(plan),
            Ok(None) => ForgetReport::already_empty(home),
            Err(error) => failed_preview(home, error),
        },
        Err(error) => failed_preview(home, error),
    }
}

fn failed_preview(home: &Path, error: String) -> ForgetReport {
    ForgetReport::refused(empty_plan(home), error)
}

fn apply_at(home: &Path, confirmation: &str) -> ForgetReport {
    apply_at_with(home, confirmation, daemon_is_stopped)
}

fn apply_at_with<F>(home: &Path, confirmation: &str, daemon_stopped: F) -> ForgetReport
where
    F: FnOnce() -> Result<(), String>,
{
    let initial = match data::inspect_records(home) {
        Ok(source) => source,
        Err(error) => return failed_preview(home, error),
    };
    let Some(initial_inspector) = initial.inspector.as_ref() else {
        return ForgetReport::already_empty(home);
    };
    let initial_checkpoint = match read_checkpoint(initial_inspector) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return failed_preview(home, error),
    };
    match &initial_checkpoint {
        Some(ForgetCheckpoint::Pending {
            operation_id, plan, ..
        }) if confirmation != plan.confirmation => {
            return ForgetReport::recovery_required(
                plan.clone(),
                operation_id.clone(),
                "the supplied confirmation does not name the pending removal plan".into(),
            )
        }
        Some(ForgetCheckpoint::Pending { .. }) => {}
        Some(ForgetCheckpoint::Completed { .. }) | None => {
            let Some(plan) = (match plan_from_source(&initial) {
                Ok(plan) => plan,
                Err(error) => return failed_preview(home, error),
            }) else {
                return ForgetReport::already_empty(home);
            };
            if confirmation != plan.confirmation {
                return ForgetReport::confirmation_required(
                    plan,
                    "the supplied confirmation does not match the current inventory; preview again before deleting".into(),
                );
            }
        }
    }
    if let Err(error) = daemon_stopped() {
        return refused_apply(initial_checkpoint, &initial, error);
    }

    let root = match StateRoot::open_existing(home) {
        Ok(Some(root)) => root,
        Ok(None) => return ForgetReport::already_empty(home),
        Err(error) => {
            return failed_preview(
                home,
                format!("open durable-record state root for removal: {error}"),
            )
        }
    };
    let lease = match root.open_append(Path::new(LEASE)) {
        Ok(lease) => lease,
        Err(error) => {
            return refused_apply(
                initial_checkpoint,
                &initial,
                format!("open durable-record removal lease: {error}"),
            )
        }
    };
    match lease.try_lock() {
        Ok(true) => {}
        Ok(false) => {
            return refused_apply(
                initial_checkpoint,
                &initial,
                "cyclopsd or another confirmed durable-record removal holds the journal lease; wait for it to finish"
                    .into(),
            )
        }
        Err(error) => {
            return refused_apply(
                initial_checkpoint,
                &initial,
                format!("lock durable-record removal lease: {error}"),
            )
        }
    }

    // The lease is the serialization point. Rebuild every fact after holding
    // it, so an earlier preview can never authorize a changed record set.
    let source = match data::inspect_records(home) {
        Ok(source) => source,
        Err(error) => return failed_preview(home, error),
    };
    let Some(inspector) = source.inspector.as_ref() else {
        return ForgetReport::already_empty(home);
    };
    let checkpoint = match read_checkpoint(inspector) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return failed_preview(home, error),
    };

    let (operation_id, plan, pending) = match checkpoint {
        Some(ForgetCheckpoint::Pending {
            operation_id, plan, ..
        }) => {
            if confirmation != plan.confirmation {
                return ForgetReport::recovery_required(
                    plan,
                    operation_id,
                    "the supplied confirmation does not name the pending removal plan".into(),
                );
            }
            (operation_id, plan, true)
        }
        Some(ForgetCheckpoint::Completed { .. }) | None => {
            let Some(plan) = (match plan_from_source(&source) {
                Ok(plan) => plan,
                Err(error) => return failed_preview(home, error),
            }) else {
                return ForgetReport::already_empty(home);
            };
            if confirmation != plan.confirmation {
                return ForgetReport::confirmation_required(
                    plan,
                    "the supplied confirmation does not match the current inventory; preview again before deleting".into(),
                );
            }
            let operation_id = next_operation_id();
            let checkpoint = ForgetCheckpoint::Pending {
                schema: FORGET_SCHEMA,
                operation_id: operation_id.clone(),
                plan: plan.clone(),
            };
            if let Err(error) = write_checkpoint(&root, &checkpoint) {
                return ForgetReport::partial(
                    plan,
                    operation_id,
                    RemovalProgress::default(),
                    error,
                );
            }
            forget_boundary(ForgetBoundary::PendingWritten);
            (operation_id, plan, false)
        }
    };

    let progress = match remove_targets(inspector, &plan) {
        Ok(progress) => progress,
        Err((progress, error)) => {
            return ForgetReport::partial(plan, operation_id, progress, error)
        }
    };
    if pending {
        // A resumed operation can reach this point with every target already
        // absent. The completed checkpoint makes that exact recovery visible.
        forget_boundary(ForgetBoundary::RecoveryFinished);
    } else {
        forget_boundary(ForgetBoundary::TargetsRemoved);
    }
    let completed = ForgetCheckpoint::Completed {
        schema: FORGET_SCHEMA,
        operation_id: operation_id.clone(),
        plan: plan.clone(),
        removed_files: progress.removed_files,
        removed_bytes: progress.removed_bytes,
        already_absent_files: progress.already_absent_files,
        already_absent_bytes: progress.already_absent_bytes,
    };
    if let Err(error) = write_checkpoint(&root, &completed) {
        return ForgetReport::partial(plan, operation_id, progress, error);
    }
    ForgetReport::completed(plan, operation_id, progress)
}

fn refused_apply(
    checkpoint: Option<ForgetCheckpoint>,
    source: &data::RecordSource,
    error: String,
) -> ForgetReport {
    match checkpoint {
        Some(ForgetCheckpoint::Pending {
            operation_id, plan, ..
        }) => ForgetReport::recovery_required(plan, operation_id, error),
        Some(ForgetCheckpoint::Completed { .. }) | None => match plan_from_source(source) {
            Ok(Some(plan)) => ForgetReport::refused(plan, error),
            Ok(None) => ForgetReport::already_empty(&source.home),
            Err(error) => failed_preview(&source.home, error),
        },
    }
}

fn daemon_is_stopped() -> Result<(), String> {
    match Client::connect() {
        Err(ClientError::NotRunning(_)) => Ok(()),
        Ok(_) => Err(
            "cyclopsd is running; stop it with `cyclops daemon stop` before confirming durable-record removal"
                .into(),
        ),
        Err(error) => Err(format!(
            "cannot prove cyclopsd is stopped ({error}); resolve the daemon state before confirming durable-record removal"
        )),
    }
}

fn plan_from_source(source: &data::RecordSource) -> Result<Option<ForgetPlan>, String> {
    if !source.inventory.complete() {
        return Err(
            "durable-record inventory is incomplete; Cyclops will not delete a partial scope"
                .into(),
        );
    }
    if source.inventory.files() == 0 {
        return Ok(None);
    }
    let mut targets = source
        .inventory
        .records()
        .map(|record| ForgetTarget {
            relative: record.path.clone(),
            bytes: record.bytes,
            evidence: record.evidence.clone(),
        })
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| left.relative.cmp(&right.relative));
    let bytes = source.inventory.bytes()?;
    let state_root = source.home.display().to_string();
    let confirmation = confirmation_for(&state_root, &targets)?;
    let plan = ForgetPlan {
        schema: FORGET_SCHEMA,
        state_root,
        files: targets.len(),
        bytes,
        targets,
        confirmation,
    };
    validate_plan(&plan)?;
    let checkpoint_bytes = serde_json::to_vec(&ForgetCheckpoint::Pending {
        schema: FORGET_SCHEMA,
        operation_id: "data-forget-preview".into(),
        plan: plan.clone(),
    })
    .map_err(|error| format!("serialize durable-record removal plan: {error}"))?;
    if checkpoint_bytes.len() > CHECKPOINT_BYTES_LIMIT {
        return Err(format!(
            "durable-record removal plan exceeds its {}-byte checkpoint bound; export records and remove them in a later supported migration",
            CHECKPOINT_BYTES_LIMIT
        ));
    }
    Ok(Some(plan))
}

fn empty_plan(home: &Path) -> ForgetPlan {
    ForgetPlan {
        schema: FORGET_SCHEMA,
        state_root: home.display().to_string(),
        files: 0,
        bytes: 0,
        targets: Vec::new(),
        confirmation: String::new(),
    }
}

fn confirmation_for(state_root: &str, targets: &[ForgetTarget]) -> Result<String, String> {
    #[derive(Serialize)]
    struct Confirmation<'a> {
        scope: &'static str,
        state_root: &'a str,
        targets: &'a [ForgetTarget],
    }

    let bytes = serde_json::to_vec(&Confirmation {
        scope: SCOPE,
        state_root,
        targets,
    })
    .map_err(|error| format!("serialize durable-record removal confirmation: {error}"))?;
    Ok(format!("{CONFIRMATION_PREFIX}{:x}", Sha256::digest(bytes)))
}

fn validate_plan(plan: &ForgetPlan) -> Result<(), String> {
    if plan.schema != FORGET_SCHEMA {
        return Err("durable-record removal checkpoint has an unsupported schema".into());
    }
    if plan.files != plan.targets.len() || plan.targets.is_empty() {
        return Err("durable-record removal checkpoint has invalid target counts".into());
    }
    if plan.targets.len() > cyclops_state::INSPECTION_ENTRY_LIMIT_MAX {
        return Err("durable-record removal checkpoint exceeds the inspection entry limit".into());
    }
    let mut paths = BTreeSet::new();
    let mut bytes = 0_u64;
    for target in &plan.targets {
        target_relative_path(target)?;
        if !paths.insert(&target.relative) {
            return Err("durable-record removal checkpoint names one record twice".into());
        }
        if target.bytes != target.evidence.bytes {
            return Err("durable-record removal checkpoint has inconsistent record bytes".into());
        }
        bytes = bytes
            .checked_add(target.bytes)
            .ok_or_else(|| "durable-record removal checkpoint byte count overflowed".to_string())?;
    }
    if bytes != plan.bytes {
        return Err("durable-record removal checkpoint has an invalid byte total".into());
    }
    if confirmation_for(&plan.state_root, &plan.targets)? != plan.confirmation {
        return Err("durable-record removal checkpoint confirmation is invalid".into());
    }
    Ok(())
}

fn target_relative_path(target: &ForgetTarget) -> Result<PathBuf, String> {
    let relative = PathBuf::from(&target.relative);
    let components = relative.components().collect::<Vec<_>>();

    fn normal<'a>(component: &Component<'a>) -> Option<&'a str> {
        match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        }
    }

    let allowed = match components.as_slice() {
        [first, second] if normal(first) == Some("ledger") => {
            normal(second).is_some_and(|name| name.ends_with(".ndjson") && name != ".ndjson")
        }
        [first, workspace, file]
            if normal(first) == Some("workspaces") && normal(workspace).is_some() =>
        {
            normal(file) == Some("messages.ndjson")
        }
        _ => false,
    };
    if !allowed {
        return Err(format!(
            "durable-record removal checkpoint has an out-of-scope target: {}",
            target.relative
        ));
    }
    Ok(relative)
}

fn remove_targets(
    inspector: &StateInspector,
    plan: &ForgetPlan,
) -> Result<RemovalProgress, (RemovalProgress, String)> {
    if let Err(error) = validate_plan(plan) {
        return Err((RemovalProgress::default(), error));
    }
    if inspector.path().display().to_string() != plan.state_root {
        return Err((
            RemovalProgress::default(),
            "the durable-record state root changed spelling since confirmation; preview again from the same state root".into(),
        ));
    }
    let mut progress = RemovalProgress::default();
    for target in &plan.targets {
        let relative = match target_relative_path(target) {
            Ok(relative) => relative,
            Err(error) => return Err((progress, error)),
        };
        let entry = match inspected_target(inspector, &relative, target) {
            Ok(entry) => entry,
            Err(error) => return Err((progress, error)),
        };
        let Some(entry) = entry else {
            if let Err(error) = progress.already_absent(target.bytes) {
                return Err((progress, error));
            }
            continue;
        };
        let bound = match inspector.bind_regular_file_for_removal(&entry) {
            Ok(bound) => bound,
            Err(error) => {
                return Err((
                    progress,
                    format!("bind confirmed durable record {}: {error}", target.relative),
                ))
            }
        };
        // The shared daemon/removal lease closes the current daemon's startup
        // race. This exact-file lease also protects a still-running older
        // daemon that predates that global lease.
        let locked = match bound.try_lock() {
            Ok(locked) => locked,
            Err(error) => {
                return Err((
                    progress,
                    format!("lock confirmed durable record {}: {error}", target.relative),
                ))
            }
        };
        if !locked {
            return Err((
                progress,
                format!(
                    "confirmed durable record {} is currently locked by a writer; Cyclops left it in place",
                    target.relative
                ),
            ));
        }
        if let Err(error) = bound.remove() {
            return Err((progress, format!("remove {}: {error}", target.relative)));
        }
        if let Err(error) = progress.removed(target.bytes) {
            return Err((progress, error));
        }
        forget_boundary(ForgetBoundary::TargetRemoved);
    }
    Ok(progress)
}

fn inspected_target(
    inspector: &StateInspector,
    relative: &Path,
    target: &ForgetTarget,
) -> Result<Option<InspectedEntry>, String> {
    let inspected = inspector
        .inspect_file_with(relative, u64::MAX, |file| {
            file.metadata()
                .map(|metadata| RecordEvidence::from_metadata(&metadata))
        })
        .map_err(|error| {
            format!(
                "inspect confirmed durable record {}: {error}",
                target.relative
            )
        })?;
    let Some((entry, evidence)) = inspected else {
        return Ok(None);
    };
    if entry.kind != InspectedKind::RegularFile {
        return Err(format!(
            "confirmed durable record {} is no longer a regular file",
            target.relative
        ));
    }
    data::require_safe(&entry, "confirmed durable record")?;
    if !target.evidence.matches_entry(&entry) || target.evidence != evidence {
        return Err(format!(
            "confirmed durable record {} changed after confirmation; Cyclops left it in place",
            target.relative
        ));
    }
    Ok(Some(entry))
}

fn read_checkpoint(inspector: &StateInspector) -> Result<Option<ForgetCheckpoint>, String> {
    let Some(file) = inspector
        .read_file(Path::new(CHECKPOINT), CHECKPOINT_BYTES_LIMIT)
        .map_err(|error| format!("read durable-record removal checkpoint: {error}"))?
    else {
        return Ok(None);
    };
    if file.truncated {
        return Err("durable-record removal checkpoint exceeds its byte bound".into());
    }
    let checkpoint: ForgetCheckpoint = serde_json::from_slice(&file.bytes)
        .map_err(|error| format!("parse durable-record removal checkpoint: {error}"))?;
    validate_checkpoint(&checkpoint)?;
    let plan = match &checkpoint {
        ForgetCheckpoint::Pending { plan, .. } | ForgetCheckpoint::Completed { plan, .. } => plan,
    };
    if plan.state_root != inspector.path().display().to_string() {
        return Err(
            "durable-record removal checkpoint names a different state root; Cyclops will not resume it"
                .into(),
        );
    }
    Ok(Some(checkpoint))
}

fn validate_checkpoint(checkpoint: &ForgetCheckpoint) -> Result<(), String> {
    match checkpoint {
        ForgetCheckpoint::Pending {
            schema,
            operation_id,
            plan,
        } => {
            if *schema != FORGET_SCHEMA {
                return Err("durable-record removal checkpoint has an unsupported schema".into());
            }
            validate_operation_id(operation_id)?;
            validate_plan(plan)?;
        }
        ForgetCheckpoint::Completed {
            schema,
            operation_id,
            plan,
            removed_files,
            removed_bytes,
            already_absent_files,
            already_absent_bytes,
        } => {
            if *schema != FORGET_SCHEMA {
                return Err("durable-record removal checkpoint has an unsupported schema".into());
            }
            validate_operation_id(operation_id)?;
            validate_plan(plan)?;
            if removed_files.checked_add(*already_absent_files) != Some(plan.files)
                || removed_bytes.checked_add(*already_absent_bytes) != Some(plan.bytes)
            {
                return Err(
                    "durable-record removal checkpoint has invalid completion totals".into(),
                );
            }
        }
    }
    Ok(())
}

fn write_checkpoint(root: &StateRoot, checkpoint: &ForgetCheckpoint) -> Result<(), String> {
    validate_checkpoint(checkpoint)?;
    let mut bytes = serde_json::to_vec(checkpoint)
        .map_err(|error| format!("serialize durable-record removal checkpoint: {error}"))?;
    if bytes.len() > CHECKPOINT_BYTES_LIMIT {
        return Err(format!(
            "durable-record removal checkpoint exceeds its {}-byte bound",
            CHECKPOINT_BYTES_LIMIT
        ));
    }
    bytes.push(b'\n');
    root.replace_file(Path::new(CHECKPOINT), &bytes)
        .map_err(|error| format!("write durable-record removal checkpoint: {error}"))
}

fn next_operation_id() -> String {
    let sequence = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
    format!(
        "data-forget-{}-{}-{sequence}",
        std::process::id(),
        crate::render::now_ms()
    )
}

fn validate_operation_id(operation_id: &str) -> Result<(), String> {
    if operation_id.len() > 128
        || !operation_id.starts_with("data-forget-")
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("durable-record removal checkpoint has an invalid operation id".into());
    }
    Ok(())
}

fn render_json(report: &ForgetReport) -> Value {
    let plan = report.plan.as_ref();
    json!({
        "schema": FORGET_SCHEMA,
        "mode": report.mode,
        "state": report.state.name(),
        "scope": SCOPE,
        "preserved": PRESERVED,
        "state_root": plan.map(|plan| &plan.state_root),
        "files": plan.map(|plan| plan.files).unwrap_or(0),
        "bytes": plan.map(|plan| plan.bytes).unwrap_or(0),
        "targets": plan.map(|plan| plan.targets.iter().map(|target| json!({
            "path": target.relative,
            "bytes": target.bytes,
        })).collect::<Vec<_>>()).unwrap_or_default(),
        "confirmation": plan.and_then(|plan| (!plan.confirmation.is_empty()).then_some(&plan.confirmation)),
        "operation_id": report.operation_id,
        "removed_files": report.removed_files,
        "removed_bytes": report.removed_bytes,
        "already_absent_files": report.already_absent_files,
        "already_absent_bytes": report.already_absent_bytes,
        "checkpoint": CHECKPOINT,
        "recovery": "A pending checkpoint never authorizes new records. Rerun the exact confirmation only after resolving the reported condition.",
        "error": report.error,
    })
}

fn render_plain(report: &ForgetReport) -> String {
    let mut lines = vec![format!("durable record forget {}", report.state.name())];
    if let Some(plan) = &report.plan {
        lines.push(format!("  scope       {SCOPE}"));
        lines.push(format!("  state root  {}", plan.state_root));
        lines.push(format!(
            "  selected    {} files · {} bytes",
            plan.files, plan.bytes
        ));
        for target in &plan.targets {
            lines.push(format!("    {} · {} bytes", target.relative, target.bytes));
        }
        if !plan.confirmation.is_empty() {
            lines.push(format!(
                "  confirm     cyclops data forget --all --confirm {}",
                plan.confirmation
            ));
        }
    }
    lines.push(format!("  preserved   {PRESERVED}"));
    if let Some(operation_id) = &report.operation_id {
        lines.push(format!("  operation   {operation_id}"));
    }
    if report.removed_files > 0 {
        lines.push(format!(
            "  removed     {} files · {} bytes",
            report.removed_files, report.removed_bytes
        ));
    }
    if report.already_absent_files > 0 {
        lines.push(format!(
            "  recovered   {} planned files were already absent before this invocation · {} bytes",
            report.already_absent_files, report.already_absent_bytes
        ));
    }
    match report.state {
        ForgetState::Preview => lines.push(
            "  next        keep cyclopsd stopped; if it was running, stop it and preview again before using a confirmation"
                .into(),
        ),
        ForgetState::Completed => lines.push(
            "  result      every planned journal is absent; empty parent directories and records created after the preview may remain"
                .into(),
        ),
        ForgetState::AlreadyEmpty => lines.push("  result      no retained durable journals are present".into()),
        ForgetState::ConfirmationRequired
        | ForgetState::RecoveryRequired
        | ForgetState::Partial
        | ForgetState::Refused => {}
    }
    if let Some(error) = &report.error {
        lines.push(format!("  refused     {error}"));
        lines.push(
            "  recovery    resolve that condition, then rerun the exact confirmation shown above; Cyclops will not add new records to a pending plan"
                .into(),
        );
    }
    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForgetBoundary {
    PendingWritten,
    TargetRemoved,
    TargetsRemoved,
    RecoveryFinished,
}

#[cfg(test)]
thread_local! {
    static FORGET_CRASH_BOUNDARY: std::cell::Cell<Option<ForgetBoundary>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn forget_boundary(boundary: ForgetBoundary) {
    FORGET_CRASH_BOUNDARY.with(|slot| {
        if slot.get() == Some(boundary) {
            slot.set(None);
            panic!("injected durable-record removal crash at {boundary:?}");
        }
    });
}

#[cfg(not(test))]
fn forget_boundary(_: ForgetBoundary) {}

#[cfg(not(test))]
#[derive(Clone, Copy)]
enum ForgetBoundary {
    PendingWritten,
    TargetRemoved,
    TargetsRemoved,
    RecoveryFinished,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    const PRIVATE_FILE_MODE: u32 = 0o600;

    fn scratch() -> tempfile::TempDir {
        let root = cyclops_proto::scratch::scratch_root();
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("cyclops-data-forget-")
            .tempdir_in(root)
            .unwrap()
    }

    fn private_directory(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE)).unwrap();
    }

    fn record(path: &Path, bytes: &[u8]) {
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

    fn planned_confirmation(home: &Path) -> String {
        let report = preview_at(home);
        assert_eq!(report.state, ForgetState::Preview, "{report:?}");
        report.plan.unwrap().confirmation
    }

    #[test]
    fn preview_is_read_only_and_confirmation_binds_the_current_record_set() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let workspace = home.join("workspaces/alpha/messages.ndjson");
        let session = home.join("ledger/main.ndjson");
        record(&workspace, b"workspace\n");
        record(&session, b"session\n");

        let report = preview_at(&home);
        assert_eq!(report.state, ForgetState::Preview);
        let plan = report.plan.unwrap();
        assert_eq!(plan.files, 2);
        assert!(plan.confirmation.starts_with(CONFIRMATION_PREFIX));
        assert_eq!(fs::read(&workspace).unwrap(), b"workspace\n");
        assert_eq!(fs::read(&session).unwrap(), b"session\n");
        assert!(!home.join(CHECKPOINT).exists());
        assert!(!home.join(LEASE).exists());

        let mismatch = apply_at_with(&home, "forget-durable-records:not-this-plan", || Ok(()));
        assert_eq!(mismatch.state, ForgetState::ConfirmationRequired);
        assert_eq!(fs::read(&workspace).unwrap(), b"workspace\n");
        assert_eq!(fs::read(&session).unwrap(), b"session\n");
        assert!(!home.join(CHECKPOINT).exists());
    }

    #[test]
    fn journal_change_between_preview_and_confirmation_refuses_the_stale_token() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let session = home.join("ledger/main.ndjson");
        record(&session, b"before graceful stop\n");
        let confirmation = planned_confirmation(&home);

        // A graceful daemon stop can append its final ledger facts. Simulate
        // that exact post-preview mutation without starting a daemon here.
        fs::write(&session, b"before graceful stop\nafter graceful stop\n").unwrap();

        let report = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(
            report.state,
            ForgetState::ConfirmationRequired,
            "{report:?}"
        );
        assert!(report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("does not match the current inventory")));
        assert_eq!(
            fs::read(&session).unwrap(),
            b"before graceful stop\nafter graceful stop\n"
        );
        assert!(
            !home.join(CHECKPOINT).exists(),
            "a stale confirmation must not create a removal checkpoint"
        );
    }

    #[test]
    fn confirmed_forget_removes_only_inventory_journals_and_leaves_other_state() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let workspace = home.join("workspaces/alpha/messages.ndjson");
        let session = home.join("ledger/main.ndjson");
        let config = home.join("config.toml");
        record(&workspace, b"workspace\n");
        record(&session, b"session\n");
        record(&config, b"theme = \"light\"\n");
        let confirmation = planned_confirmation(&home);

        let report = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(report.state, ForgetState::Completed, "{report:?}");
        assert_eq!(report.removed_files, 2);
        assert!(!workspace.exists());
        assert!(!session.exists());
        assert_eq!(fs::read(&config).unwrap(), b"theme = \"light\"\n");
        assert!(matches!(
            read_checkpoint(
                data::inspect_records(&home)
                    .unwrap()
                    .inspector
                    .as_ref()
                    .unwrap()
            )
            .unwrap(),
            Some(ForgetCheckpoint::Completed { .. })
        ));
    }

    #[test]
    fn confirmed_forget_refuses_while_the_daemon_journal_lease_is_held() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let session = home.join("ledger/main.ndjson");
        record(&session, b"session\n");
        let confirmation = planned_confirmation(&home);

        let state_root = StateRoot::open_or_create(&home).unwrap();
        let daemon_lease = state_root.open_append(Path::new(LEASE)).unwrap();
        assert!(daemon_lease.try_lock().unwrap());

        let report = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(report.state, ForgetState::Refused, "{report:?}");
        assert!(report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("holds the journal lease")));
        assert_eq!(fs::read(&session).unwrap(), b"session\n");
        assert!(!home.join(CHECKPOINT).exists());
    }

    #[test]
    fn locked_journal_is_left_in_place_until_its_writer_quiesces() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let session = home.join("ledger/main.ndjson");
        record(&session, b"session\n");

        // Model an older daemon that already opened and locked the journal
        // before the operator made the preview.
        let state_root = StateRoot::open_existing(&home).unwrap().unwrap();
        let writer = state_root
            .open_append(Path::new("ledger/main.ndjson"))
            .unwrap();
        assert!(writer.try_lock().unwrap());
        let confirmation = planned_confirmation(&home);

        let blocked = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(blocked.state, ForgetState::Partial, "{blocked:?}");
        assert!(blocked
            .error
            .as_deref()
            .is_some_and(|error| error.contains("currently locked")));
        assert_eq!(fs::read(&session).unwrap(), b"session\n");

        writer.unlock().unwrap();
        drop(writer);
        let recovered = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(recovered.state, ForgetState::Completed, "{recovered:?}");
        assert!(!session.exists());
    }

    #[test]
    fn crash_after_one_removal_keeps_a_pending_plan_that_resumes_exactly() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let workspace = home.join("workspaces/alpha/messages.ndjson");
        let session = home.join("ledger/main.ndjson");
        record(&workspace, b"{\"body\":\"only-in-journal\"}\n");
        record(&session, b"session\n");
        let confirmation = planned_confirmation(&home);

        FORGET_CRASH_BOUNDARY.with(|slot| slot.set(Some(ForgetBoundary::TargetRemoved)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());

        let checkpoint = fs::read_to_string(home.join(CHECKPOINT)).unwrap();
        assert!(
            !checkpoint.contains("only-in-journal"),
            "the recovery checkpoint must not copy a journal body"
        );

        let pending = preview_at(&home);
        assert_eq!(pending.state, ForgetState::RecoveryRequired, "{pending:?}");
        let resumed = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(resumed.state, ForgetState::Completed, "{resumed:?}");
        assert_eq!(resumed.removed_files, 1);
        assert_eq!(resumed.already_absent_files, 1);
        assert!(!workspace.exists());
        assert!(!session.exists());
    }

    #[test]
    fn recovery_refuses_a_record_that_changed_after_the_confirmed_plan() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let session = home.join("ledger/main.ndjson");
        record(&session, b"before\n");
        let confirmation = planned_confirmation(&home);

        FORGET_CRASH_BOUNDARY.with(|slot| slot.set(Some(ForgetBoundary::PendingWritten)));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let _ = apply_at_with(&home, &confirmation, || Ok(()));
        }))
        .is_err());
        fs::write(&session, b"after\n").unwrap();

        let report = apply_at_with(&home, &confirmation, || Ok(()));
        assert_eq!(report.state, ForgetState::Partial, "{report:?}");
        assert!(report.error.unwrap().contains("changed after confirmation"));
        assert_eq!(fs::read(&session).unwrap(), b"after\n");
    }
}
