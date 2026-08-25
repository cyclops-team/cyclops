//! Bounded cleanup for rebuildable assets with an explicit ownership contract.
//!
//! The command never accepts an arbitrary path. Inventory and removal stay
//! beneath held directory descriptors, and removal is always an explicit
//! second step after a dry-run report.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use cyclops_state::{
    BoundStateRemoval, InspectedEntry, InspectedKind, InspectionLimits, StateInspector,
};
use serde_json::{json, Value};

use crate::update::{BUILD_CACHE_LEASE, SCRATCH_LEASE, SCRATCH_MARKER};

pub(crate) const ENTRY_LIMIT: usize = 4_096;
pub(crate) const NAME_BYTES_LIMIT: usize = 256 * 1_024;
pub(crate) const DEPTH_LIMIT: usize = 32;
const MARKER_BYTES_LIMIT: usize = 64;
const UPDATE_PREFIX: &str = crate::update::SCRATCH_PREFIX;
const LEGACY_UPDATE_PREFIX: &str = "cyclops-update.";
const CLEANUP_BUILD_PREFIX: &str = ".cyclops-cleanup.build-cache.";
const CLEANUP_UPDATE_PREFIX: &str = ".cyclops-cleanup.update-scratch.";
const NONCE_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum AssetClass {
    /// Cargo output held under Cyclops' shared build-cache lease.
    BuildCache,
    /// Completed update workspaces with a matching owner marker and lease.
    UpdateScratch,
}

impl AssetClass {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::BuildCache => "build_cache",
            Self::UpdateScratch => "update_scratch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateState {
    Absent,
    Ready,
    Removed,
    Active,
    Unmarked,
    Unsafe,
    Failed,
    Unsupported,
}

impl CandidateState {
    fn name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Ready => "ready",
            Self::Removed => "removed",
            Self::Active => "active",
            Self::Unmarked => "unmarked",
            Self::Unsafe => "unsafe",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
        }
    }

    fn is_problem(self) -> bool {
        matches!(
            self,
            Self::Active | Self::Unmarked | Self::Unsafe | Self::Failed | Self::Unsupported
        )
    }
}

#[derive(Debug)]
struct CandidateReport {
    class: AssetClass,
    path: PathBuf,
    state: CandidateState,
    entries: usize,
    bytes: u64,
    reason: String,
}

impl CandidateReport {
    fn new(
        class: AssetClass,
        path: PathBuf,
        state: CandidateState,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            class,
            path,
            state,
            entries: 0,
            bytes: 0,
            reason: reason.into(),
        }
    }
}

#[derive(Debug)]
struct ExcludedClass {
    class: &'static str,
    state: &'static str,
    reason: &'static str,
}

#[derive(Debug)]
struct CleanupReport {
    apply: bool,
    temp_root: PathBuf,
    temp_root_state: &'static str,
    candidates: Vec<CandidateReport>,
    excluded: Vec<ExcludedClass>,
    issues: Vec<String>,
}

struct Inventory {
    entries: Vec<InspectedEntry>,
    directories: BTreeMap<PathBuf, InspectedEntry>,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MountIdentity {
    device: u64,
    mount_id: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CleanupTombstone {
    class: AssetClass,
    marker: Option<String>,
    cache_key: Option<String>,
    device: u64,
    inode: u64,
}

struct PreparedAsset<'a> {
    temp: &'a StateInspector,
    temp_entry: InspectedEntry,
    inspector: StateInspector,
    lease: Option<BoundStateRemoval>,
    inventory: Inventory,
    allowed_mount: MountIdentity,
}

pub(crate) struct OperationalAssetCandidate {
    pub(crate) class: AssetClass,
    pub(crate) path: PathBuf,
    pub(crate) safe: bool,
    pub(crate) truncated: bool,
    pub(crate) entries: usize,
    pub(crate) bytes: u64,
    pub(crate) marker: &'static str,
    pub(crate) lease: &'static str,
    pub(crate) error: Option<String>,
}

pub(crate) struct OperationalAssetInventory {
    pub(crate) temp_root: PathBuf,
    pub(crate) root_safe: bool,
    pub(crate) truncated: bool,
    pub(crate) candidates: Vec<OperationalAssetCandidate>,
    pub(crate) error: Option<String>,
}

#[cfg(test)]
type BeforeApply = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
type AfterIsolate = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
type AfterOperationalInventory = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    static BEFORE_APPLY: std::cell::RefCell<Option<BeforeApply>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_ISOLATE: std::cell::RefCell<Option<AfterIsolate>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_OPERATIONAL_INVENTORY: std::cell::RefCell<Option<AfterOperationalInventory>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn run(json_output: bool, classes: &[AssetClass], apply: bool) -> i32 {
    let temp_root = platform_temp_root();
    let cache = crate::update::build_cache(&cyclops_proto::cyclops_home());
    let report = collect_at(&temp_root, &cache, classes, apply);
    if json_output {
        println!("{}", render_json(&report));
    } else {
        print!("{}", render_plain(&report));
    }
    i32::from(!report.issues.is_empty())
}

/// Inspect cleanup-managed operational assets without locking or changing them.
pub(crate) fn inspect_operational_assets(home: &Path) -> OperationalAssetInventory {
    let temp_root = platform_temp_root();
    let cache = crate::update::build_cache(home);
    let temp = match open_private_temp_root(&temp_root) {
        Ok(temp) => temp,
        Err(error) => {
            return OperationalAssetInventory {
                temp_root,
                root_safe: false,
                truncated: false,
                candidates: Vec::new(),
                error: Some(error),
            }
        }
    };
    let snapshot = match temp.inspect_root(limits()) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return OperationalAssetInventory {
                temp_root,
                root_safe: false,
                truncated: false,
                candidates: Vec::new(),
                error: Some(error.to_string()),
            }
        }
    };
    if snapshot.truncated {
        return OperationalAssetInventory {
            temp_root,
            root_safe: true,
            truncated: true,
            candidates: Vec::new(),
            error: Some("temporary-root inventory reached its fixed limit".into()),
        };
    }
    let mut candidates = Vec::new();
    for entry in &snapshot.entries {
        if entry.path == cache
            || cleanup_tombstone(&entry.path).is_some_and(|tombstone| {
                tombstone.class == AssetClass::BuildCache
                    && tombstone.cache_key.as_deref() == cache_key(&cache)
            })
        {
            candidates.push(inspect_operational_candidate(
                &temp,
                entry,
                AssetClass::BuildCache,
                None,
            ));
            continue;
        }
        let marker = update_nonce(&entry.path).map(str::to_owned).or_else(|| {
            cleanup_tombstone(&entry.path).and_then(|tombstone| {
                (tombstone.class == AssetClass::UpdateScratch)
                    .then_some(tombstone.marker)
                    .flatten()
            })
        });
        if let Some(marker) = marker {
            candidates.push(inspect_operational_candidate(
                &temp,
                entry,
                AssetClass::UpdateScratch,
                Some(&marker),
            ));
        } else if entry
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(UPDATE_PREFIX) || name.starts_with(LEGACY_UPDATE_PREFIX)
            })
        {
            candidates.push(OperationalAssetCandidate {
                class: AssetClass::UpdateScratch,
                path: entry.path.clone(),
                safe: false,
                truncated: false,
                entries: 0,
                bytes: 0,
                marker: "invalid_name",
                lease: "unproven",
                error: Some("update scratch name has no exact 32-hex nonce".into()),
            });
        }
    }
    OperationalAssetInventory {
        temp_root,
        root_safe: true,
        truncated: false,
        candidates,
        error: None,
    }
}

fn inspect_operational_candidate(
    temp: &StateInspector,
    entry: &InspectedEntry,
    class: AssetClass,
    marker: Option<&str>,
) -> OperationalAssetCandidate {
    let mut report = OperationalAssetCandidate {
        class,
        path: entry.path.clone(),
        safe: false,
        truncated: false,
        entries: 0,
        bytes: 0,
        marker: if marker.is_some() {
            "unproven"
        } else {
            "not_required"
        },
        lease: "unproven",
        error: None,
    };
    let inspected = (|| -> Result<(), String> {
        if entry.kind != InspectedKind::Directory || entry.mode != 0o700 || !entry.safe() {
            return Err("asset root is not one owner-only 0700 directory".into());
        }
        let inspector = StateInspector::open_existing(&entry.path)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "asset root changed before inspection".to_string())?;
        if !same_exact(inspector.root(), entry)
            || !inspector
                .path_matches_held_root()
                .map_err(|error| error.to_string())?
        {
            return Err("asset-root identity changed before inspection".into());
        }
        let first = inspector
            .inspect_root(limits())
            .map_err(|error| error.to_string())?;
        if first.truncated {
            return Err(
                "asset inventory reached its fixed limit before the lease was found".into(),
            );
        }
        let lease_path = inspector.path().join(match class {
            AssetClass::BuildCache => BUILD_CACHE_LEASE,
            AssetClass::UpdateScratch => SCRATCH_LEASE,
        });
        let lease = first
            .entries
            .iter()
            .find(|candidate| candidate.path == lease_path);
        let mut validation_error = match lease {
            None => Some("asset has no shared cleanup lease".to_string()),
            Some(lease)
                if lease.kind != InspectedKind::RegularFile
                    || lease.mode != 0o600
                    || !lease.safe() =>
            {
                Some("asset lease is not one owner-only single-link regular file".into())
            }
            Some(_) => {
                report.lease = "current";
                None
            }
        };
        if let Some(expected) = marker {
            match validate_update_marker(&inspector, expected) {
                Ok(()) => report.marker = "current",
                Err((_, error)) => {
                    validation_error = Some(match validation_error {
                        Some(existing) => format!("{existing}; {error}"),
                        None => error,
                    });
                }
            }
        }
        let allowed_mount = mount_identity(temp.root())?;
        let inventory =
            inventory_on_mount(&inspector, allowed_mount).map_err(|(_, error)| error)?;
        if let Some(error) = validation_error {
            return Err(error);
        }
        #[cfg(test)]
        AFTER_OPERATIONAL_INVENTORY.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook(&entry.path);
            }
        });
        revalidate_named_root(entry, allowed_mount)?;
        report.safe = true;
        report.entries = inventory.entries.len();
        report.bytes = inventory.bytes;
        Ok(())
    })();
    if let Err(error) = inspected {
        report.truncated = error.contains("limit");
        report.error = Some(error);
    }
    report
}

fn collect_at(
    temp_root: &Path,
    cache: &Path,
    classes: &[AssetClass],
    apply: bool,
) -> CleanupReport {
    let classes = deduplicate_classes(classes);
    let mut report = CleanupReport {
        apply,
        temp_root: temp_root.to_path_buf(),
        temp_root_state: "unsupported",
        candidates: Vec::new(),
        excluded: excluded_classes(),
        issues: Vec::new(),
    };

    let temp = match open_private_temp_root(temp_root) {
        Ok(temp) => temp,
        Err(reason) => {
            report.issues.push(reason.clone());
            for class in classes {
                report.candidates.push(CandidateReport::new(
                    class,
                    class_placeholder(class, temp_root, cache),
                    CandidateState::Unsupported,
                    reason.clone(),
                ));
            }
            return report;
        }
    };
    report.temp_root_state = "private_owner_root";

    let snapshot = match temp.inspect_root(limits()) {
        Ok(snapshot) if !snapshot.truncated => snapshot,
        Ok(_) => {
            let reason = "temporary-root inventory reached its fixed limit".to_string();
            report.issues.push(reason.clone());
            for class in classes {
                report.candidates.push(CandidateReport::new(
                    class,
                    class_placeholder(class, temp_root, cache),
                    CandidateState::Unsupported,
                    reason.clone(),
                ));
            }
            return report;
        }
        Err(error) => {
            let reason = format!("temporary-root inventory is unavailable: {error}");
            report.issues.push(reason.clone());
            for class in classes {
                report.candidates.push(CandidateReport::new(
                    class,
                    class_placeholder(class, temp_root, cache),
                    CandidateState::Unsupported,
                    reason.clone(),
                ));
            }
            return report;
        }
    };

    for class in classes {
        match class {
            AssetClass::BuildCache => {
                inspect_build_cache(&temp, &snapshot.entries, cache, apply, &mut report)
            }
            AssetClass::UpdateScratch => {
                inspect_update_scratch(&temp, &snapshot.entries, temp_root, apply, &mut report)
            }
        }
    }

    for candidate in &report.candidates {
        if candidate.state.is_problem() {
            report.issues.push(format!(
                "{} {}: {}",
                candidate.class.name(),
                candidate.path.display(),
                candidate.reason
            ));
        }
    }
    report
}

fn deduplicate_classes(classes: &[AssetClass]) -> Vec<AssetClass> {
    let mut build_cache = false;
    let mut update_scratch = false;
    for class in classes {
        match class {
            AssetClass::BuildCache => build_cache = true,
            AssetClass::UpdateScratch => update_scratch = true,
        }
    }
    let mut result = Vec::with_capacity(2);
    if build_cache {
        result.push(AssetClass::BuildCache);
    }
    if update_scratch {
        result.push(AssetClass::UpdateScratch);
    }
    result
}

fn excluded_classes() -> Vec<ExcludedClass> {
    vec![
        ExcludedClass {
            class: "state_journals_and_messages",
            state: "excluded",
            reason: "durable state is never a cleanup asset",
        },
        ExcludedClass {
            class: "rollback_pair_store",
            state: "excluded",
            reason: "rollback pairs are managed only by the transactional updater",
        },
        ExcludedClass {
            class: "historical_test_scratch_and_loose_logs",
            state: "unproven",
            reason: "historical assets have no owner marker and active lease contract",
        },
        ExcludedClass {
            class: "processes",
            state: "excluded",
            reason: "cleanup never signals or kills a process",
        },
    ]
}

fn class_placeholder(class: AssetClass, temp_root: &Path, cache: &Path) -> PathBuf {
    match class {
        AssetClass::BuildCache => cache.to_path_buf(),
        AssetClass::UpdateScratch => temp_root.join("cycu.<32-hex>"),
    }
}

fn platform_temp_root() -> PathBuf {
    let raw = std::env::temp_dir();
    #[cfg(target_os = "macos")]
    {
        if raw == Path::new("/tmp") {
            return PathBuf::from("/private/tmp");
        }
        if let Ok(rest) = raw.strip_prefix("/var") {
            return PathBuf::from("/private/var").join(rest);
        }
    }
    raw
}

fn open_private_temp_root(path: &Path) -> Result<StateInspector, String> {
    let inspector = StateInspector::open_existing(path)
        .map_err(|error| format!("unsupported_temp_root: {error}"))?
        .ok_or_else(|| "unsupported_temp_root: temporary root is absent".to_string())?;
    let root = inspector.root();
    if root.kind != InspectedKind::Directory || root.mode != 0o700 || !root.safe() {
        return Err(
            "unsupported_temp_root: cleanup requires a current-user 0700 temporary root"
                .to_string(),
        );
    }
    if !inspector
        .path_matches_held_root()
        .map_err(|error| format!("unsupported_temp_root: {error}"))?
    {
        return Err("unsupported_temp_root: temporary-root identity changed".to_string());
    }
    Ok(inspector)
}

fn inspect_build_cache(
    temp: &StateInspector,
    entries: &[InspectedEntry],
    cache: &Path,
    apply: bool,
    report: &mut CleanupReport,
) {
    if cache.parent() != Some(temp.path()) {
        report.candidates.push(CandidateReport::new(
            AssetClass::BuildCache,
            cache.to_path_buf(),
            CandidateState::Unsupported,
            "build-cache path is outside the validated temporary root",
        ));
        return;
    }
    let mut matched = false;
    for entry in entries.iter().filter(|entry| {
        entry.path == cache
            || cleanup_tombstone(&entry.path).is_some_and(|tombstone| {
                tombstone.class == AssetClass::BuildCache
                    && tombstone.cache_key.as_deref() == cache_key(cache)
            })
    }) {
        matched = true;
        inspect_candidate(temp, entry, AssetClass::BuildCache, None, apply, report);
    }
    if !matched {
        report.candidates.push(CandidateReport::new(
            AssetClass::BuildCache,
            cache.to_path_buf(),
            CandidateState::Absent,
            "no managed build cache is present",
        ));
    }
}

fn inspect_update_scratch(
    temp: &StateInspector,
    entries: &[InspectedEntry],
    temp_root: &Path,
    apply: bool,
    report: &mut CleanupReport,
) {
    let mut matched = false;
    for entry in entries {
        let nonce = update_nonce(&entry.path).map(str::to_owned).or_else(|| {
            cleanup_tombstone(&entry.path).and_then(|tombstone| {
                (tombstone.class == AssetClass::UpdateScratch)
                    .then_some(tombstone.marker)
                    .flatten()
            })
        });
        let Some(nonce) = nonce else { continue };
        matched = true;
        inspect_candidate(
            temp,
            entry,
            AssetClass::UpdateScratch,
            Some(&nonce),
            apply,
            report,
        );
    }
    if !matched {
        report.candidates.push(CandidateReport::new(
            AssetClass::UpdateScratch,
            temp_root.join("cycu.<32-hex>"),
            CandidateState::Absent,
            "no marked update scratch is present",
        ));
    }
}

fn update_nonce(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let nonce = name
        .strip_prefix(UPDATE_PREFIX)
        .or_else(|| name.strip_prefix(LEGACY_UPDATE_PREFIX))?;
    (nonce.len() == NONCE_BYTES
        && nonce
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)))
    .then_some(nonce)
}

fn cleanup_tombstone(path: &Path) -> Option<CleanupTombstone> {
    let name = path.file_name()?.to_str()?;
    if let Some(rest) = name.strip_prefix(CLEANUP_BUILD_PREFIX) {
        let mut parts = rest.split('.');
        let key = parts.next()?;
        let device = parts.next()?;
        let inode = parts.next()?;
        let (device, inode) = parse_cleanup_identity(device, inode)?;
        if parts.next().is_some()
            || key.is_empty()
            || !key.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        return Some(CleanupTombstone {
            class: AssetClass::BuildCache,
            marker: None,
            cache_key: Some(key.to_string()),
            device,
            inode,
        });
    }
    let rest = name.strip_prefix(CLEANUP_UPDATE_PREFIX)?;
    let (nonce, identity) = rest.split_once('.')?;
    let (device, inode) = identity.split_once('.')?;
    let (device, inode) = parse_cleanup_identity(device, inode)?;
    if nonce.len() != NONCE_BYTES
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(CleanupTombstone {
        class: AssetClass::UpdateScratch,
        marker: Some(nonce.to_string()),
        cache_key: None,
        device,
        inode,
    })
}

fn is_verified_tombstone(
    entry: &InspectedEntry,
    class: AssetClass,
    marker: Option<&str>,
) -> Result<bool, (CandidateState, String)> {
    let Some(tombstone) = cleanup_tombstone(&entry.path) else {
        return Ok(false);
    };
    if tombstone.class != class
        || tombstone.marker.as_deref() != marker
        || tombstone.device != entry.device
        || tombstone.inode != entry.inode
    {
        return Err((
            CandidateState::Unsafe,
            "cleanup tombstone identity does not match its directory".to_string(),
        ));
    }
    Ok(true)
}

fn cache_key(path: &Path) -> Option<&str> {
    path.file_name()?
        .to_str()?
        .strip_prefix("cyclops-build-cache-")
        .filter(|key| !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_digit()))
}

fn parse_cleanup_identity(device: &str, inode: &str) -> Option<(u64, u64)> {
    if ![device, inode]
        .into_iter()
        .all(|part| part.len() == 16 && part.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return None;
    }
    Some((
        u64::from_str_radix(device, 16).ok()?,
        u64::from_str_radix(inode, 16).ok()?,
    ))
}

fn cleanup_tombstone_name(
    class: AssetClass,
    marker: Option<&str>,
    entry: &InspectedEntry,
) -> String {
    match class {
        AssetClass::BuildCache => format!(
            "{CLEANUP_BUILD_PREFIX}{}.{:016x}.{:016x}",
            cache_key(&entry.path).expect("build cache carries its home key"),
            entry.device,
            entry.inode
        ),
        AssetClass::UpdateScratch => format!(
            "{CLEANUP_UPDATE_PREFIX}{}.{:016x}.{:016x}",
            marker.expect("update scratch carries its nonce"),
            entry.device,
            entry.inode
        ),
    }
}

fn inspect_candidate(
    temp: &StateInspector,
    temp_entry: &InspectedEntry,
    class: AssetClass,
    marker: Option<&str>,
    apply: bool,
    report: &mut CleanupReport,
) {
    let path = temp_entry.path.clone();
    let prepared = prepare_asset(temp, temp_entry, class, marker);
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err((state, reason)) => {
            report
                .candidates
                .push(CandidateReport::new(class, path, state, reason));
            return;
        }
    };
    let entries = prepared.inventory.entries.len();
    let bytes = prepared.inventory.bytes;
    if !apply {
        report.candidates.push(CandidateReport {
            class,
            path,
            state: CandidateState::Ready,
            entries,
            bytes,
            reason: "dry run only; pass --apply to remove this exact asset".to_string(),
        });
        return;
    }

    #[cfg(test)]
    BEFORE_APPLY.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(&path);
        }
    });

    let outcome = remove_prepared(prepared, class, marker);
    match outcome {
        Ok(()) => report.candidates.push(CandidateReport {
            class,
            path,
            state: CandidateState::Removed,
            entries,
            bytes,
            reason: "removed after descriptor-bound revalidation".to_string(),
        }),
        Err(reason) => report.candidates.push(CandidateReport {
            class,
            path,
            state: CandidateState::Failed,
            entries,
            bytes,
            reason,
        }),
    }
}

fn prepare_asset<'a>(
    temp: &'a StateInspector,
    temp_entry: &InspectedEntry,
    class: AssetClass,
    marker: Option<&str>,
) -> Result<PreparedAsset<'a>, (CandidateState, String)> {
    if temp_entry.kind != InspectedKind::Directory || temp_entry.mode != 0o700 || !temp_entry.safe()
    {
        return Err((
            CandidateState::Unsafe,
            "asset root is not one owner-only 0700 directory".to_string(),
        ));
    }
    let inspector = StateInspector::open_existing(&temp_entry.path)
        .map_err(|error| (CandidateState::Unsafe, error.to_string()))?
        .ok_or_else(|| {
            (
                CandidateState::Unsafe,
                "asset root changed before inspection".to_string(),
            )
        })?;
    if !same_exact(inspector.root(), temp_entry)
        || !inspector
            .path_matches_held_root()
            .map_err(|error| (CandidateState::Unsafe, error.to_string()))?
    {
        return Err((
            CandidateState::Unsafe,
            "asset-root identity changed before inspection".to_string(),
        ));
    }
    let recovering = is_verified_tombstone(temp_entry, class, marker)?;

    let first = inspector
        .inspect_root(limits())
        .map_err(|error| (CandidateState::Unsafe, error.to_string()))?;
    if first.truncated {
        return Err((
            CandidateState::Unsafe,
            "asset inventory reached its fixed limit before the lease was found".to_string(),
        ));
    }
    let lease_path = inspector.path().join(match class {
        AssetClass::BuildCache => BUILD_CACHE_LEASE,
        AssetClass::UpdateScratch => SCRATCH_LEASE,
    });
    let lease = match first.entries.iter().find(|entry| entry.path == lease_path) {
        None if recovering => None,
        None => {
            return Err((
                CandidateState::Unmarked,
                "asset has no shared cleanup lease".to_string(),
            ));
        }
        Some(lease_entry)
            if lease_entry.kind != InspectedKind::RegularFile
                || lease_entry.mode != 0o600
                || !lease_entry.safe() =>
        {
            return Err((
                CandidateState::Unsafe,
                "asset lease is not one owner-only single-link regular file".to_string(),
            ));
        }
        Some(lease_entry) => {
            let lease = inspector
                .bind_regular_file_for_removal(lease_entry)
                .map_err(|error| (CandidateState::Unsafe, error.to_string()))?;
            match lease.try_lock() {
                Ok(true) => {}
                Ok(false) => {
                    return Err((
                        CandidateState::Active,
                        "asset lease is held by an active writer".to_string(),
                    ));
                }
                Err(error) => {
                    return Err((
                        CandidateState::Unsafe,
                        format!("asset lease could not be checked: {error}"),
                    ));
                }
            }
            Some(lease)
        }
    };

    if let Some(expected) = marker {
        let present = validate_update_marker_if_present(&inspector, expected)?;
        if !present && !recovering {
            return Err((
                CandidateState::Unmarked,
                "update scratch has no owner marker".to_string(),
            ));
        }
    }
    let allowed_mount =
        mount_identity(temp.root()).map_err(|error| (CandidateState::Unsafe, error))?;
    let inventory = inventory_on_mount(&inspector, allowed_mount)?;
    if lease.is_none() && !inventory.entries.is_empty() {
        return Err((
            CandidateState::Unsafe,
            "interrupted cleanup lost its lease before payload retirement".to_string(),
        ));
    }
    revalidate_named_root(temp_entry, allowed_mount)
        .map_err(|error| (CandidateState::Unsafe, error))?;
    Ok(PreparedAsset {
        temp,
        temp_entry: temp_entry.clone(),
        inspector,
        lease,
        inventory,
        allowed_mount,
    })
}

fn validate_update_marker(
    inspector: &StateInspector,
    expected: &str,
) -> Result<(), (CandidateState, String)> {
    if !validate_update_marker_if_present(inspector, expected)? {
        return Err((
            CandidateState::Unmarked,
            "update scratch has no owner marker".to_string(),
        ));
    }
    Ok(())
}

fn validate_update_marker_if_present(
    inspector: &StateInspector,
    expected: &str,
) -> Result<bool, (CandidateState, String)> {
    let marker = inspector
        .read_file(Path::new(SCRATCH_MARKER), MARKER_BYTES_LIMIT)
        .map_err(|error| (CandidateState::Unsafe, error.to_string()))?;
    let Some(marker) = marker else {
        return Ok(false);
    };
    if marker.entry.mode != 0o600 || !marker.entry.safe() {
        return Err((
            CandidateState::Unsafe,
            "update owner marker is not one owner-only single-link regular file".to_string(),
        ));
    }
    if marker.truncated || marker.bytes != expected.as_bytes() {
        return Err((
            CandidateState::Unmarked,
            "update owner marker does not exactly match its directory nonce".to_string(),
        ));
    }
    Ok(true)
}

fn inventory_on_mount(
    inspector: &StateInspector,
    allowed_mount: MountIdentity,
) -> Result<Inventory, (CandidateState, String)> {
    inventory_with_mount_probe(inspector, allowed_mount, mount_identity)
}

fn inventory_with_mount_probe<F>(
    inspector: &StateInspector,
    allowed_mount: MountIdentity,
    mut mount_probe: F,
) -> Result<Inventory, (CandidateState, String)>
where
    F: FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
{
    let root = inspector
        .inspect_root(limits())
        .map_err(|error| (CandidateState::Unsafe, error.to_string()))?;
    if root.truncated || !same_exact(&root.directory, inspector.root()) {
        return Err((
            CandidateState::Unsafe,
            "asset changed at the inventory boundary".to_string(),
        ));
    }
    require_same_mount(&root.directory, allowed_mount, &mut mount_probe)?;
    let mut entries = Vec::with_capacity(root.entries.len().min(ENTRY_LIMIT));
    let mut directories = BTreeMap::new();
    directories.insert(inspector.path().to_path_buf(), root.directory.clone());
    let mut queue = VecDeque::new();
    let mut name_bytes = 0usize;
    let mut bytes = 0u64;
    add_entries(
        root.entries,
        1,
        &mut entries,
        &mut directories,
        &mut queue,
        &mut name_bytes,
        &mut bytes,
        allowed_mount,
        &mut mount_probe,
    )?;

    while let Some((directory, depth)) = queue.pop_front() {
        if depth >= DEPTH_LIMIT {
            let snapshot = inspector
                .inspect_bound_directory(&directory, InspectionLimits::new(1, 256).unwrap())
                .map_err(|error| (CandidateState::Unsafe, error.to_string()))?;
            require_same_mount(&snapshot.directory, allowed_mount, &mut mount_probe)?;
            if snapshot.truncated || !snapshot.entries.is_empty() {
                return Err((
                    CandidateState::Unsafe,
                    "asset inventory reached its fixed depth limit".to_string(),
                ));
            }
            continue;
        }
        let remaining_entries = ENTRY_LIMIT.saturating_sub(entries.len());
        let remaining_names = NAME_BYTES_LIMIT.saturating_sub(name_bytes);
        let snapshot = inspector
            .inspect_bound_directory(
                &directory,
                InspectionLimits::new(remaining_entries, remaining_names).unwrap(),
            )
            .map_err(|error| (CandidateState::Unsafe, error.to_string()))?;
        require_same_mount(&snapshot.directory, allowed_mount, &mut mount_probe)?;
        if snapshot.truncated || !same_exact(&snapshot.directory, &directory) {
            return Err((
                CandidateState::Unsafe,
                "asset changed or reached its fixed inventory limit".to_string(),
            ));
        }
        add_entries(
            snapshot.entries,
            depth + 1,
            &mut entries,
            &mut directories,
            &mut queue,
            &mut name_bytes,
            &mut bytes,
            allowed_mount,
            &mut mount_probe,
        )?;
    }
    Ok(Inventory {
        entries,
        directories,
        bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn add_entries(
    discovered: Vec<InspectedEntry>,
    depth: usize,
    entries: &mut Vec<InspectedEntry>,
    directories: &mut BTreeMap<PathBuf, InspectedEntry>,
    queue: &mut VecDeque<(InspectedEntry, usize)>,
    name_bytes: &mut usize,
    bytes: &mut u64,
    allowed_mount: MountIdentity,
    mount_probe: &mut impl FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
) -> Result<(), (CandidateState, String)> {
    for entry in discovered {
        if entries.len() >= ENTRY_LIMIT {
            return Err((
                CandidateState::Unsafe,
                "asset inventory reached its fixed entry limit".to_string(),
            ));
        }
        let leaf_bytes = entry
            .path
            .file_name()
            .map(|name| name.as_bytes().len())
            .unwrap_or_default();
        *name_bytes = name_bytes.checked_add(leaf_bytes).ok_or_else(|| {
            (
                CandidateState::Unsafe,
                "asset inventory name-byte count overflowed".to_string(),
            )
        })?;
        if *name_bytes > NAME_BYTES_LIMIT {
            return Err((
                CandidateState::Unsafe,
                "asset inventory reached its fixed name-byte limit".to_string(),
            ));
        }
        match entry.kind {
            InspectedKind::Directory if entry.mode == 0o700 && entry.safe() => {
                require_same_mount(&entry, allowed_mount, mount_probe)?;
                directories.insert(entry.path.clone(), entry.clone());
                queue.push_back((entry.clone(), depth));
            }
            InspectedKind::RegularFile if entry.safe_beneath_owner_only_parent() => {
                require_same_mount(&entry, allowed_mount, mount_probe)?;
                *bytes = bytes.checked_add(entry.size).ok_or_else(|| {
                    (
                        CandidateState::Unsafe,
                        "asset inventory byte count overflowed".to_string(),
                    )
                })?;
            }
            _ => {
                return Err((
                    CandidateState::Unsafe,
                    format!("unsafe asset entry: {}", entry.path.display()),
                ));
            }
        }
        entries.push(entry);
    }
    Ok(())
}

fn require_same_mount(
    entry: &InspectedEntry,
    allowed: MountIdentity,
    mount_probe: &mut impl FnMut(&InspectedEntry) -> Result<MountIdentity, String>,
) -> Result<(), (CandidateState, String)> {
    let current = mount_probe(entry).map_err(|error| (CandidateState::Unsafe, error))?;
    if current != allowed {
        return Err((
            CandidateState::Unsafe,
            format!("asset crosses a mount boundary: {}", entry.path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn mount_identity(entry: &InspectedEntry) -> Result<MountIdentity, String> {
    Ok(MountIdentity {
        device: entry.device,
        mount_id: entry.device,
    })
}

#[cfg(target_os = "linux")]
fn mount_identity(entry: &InspectedEntry) -> Result<MountIdentity, String> {
    let path = std::ffi::CString::new(entry.path.as_os_str().as_bytes())
        .map_err(|_| format!("asset path contains a null byte: {}", entry.path.display()))?;
    // SAFETY: statx initializes the provided structure before a successful return.
    let mut metadata: libc::statx = unsafe { std::mem::zeroed() };
    let requested = libc::STATX_BASIC_STATS | libc::STATX_MNT_ID;
    // SAFETY: path is a valid C string and metadata points to writable storage.
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            requested,
            &mut metadata,
        )
    };
    if result != 0 {
        return Err(format!(
            "read mount identity for {}: {}",
            entry.path.display(),
            std::io::Error::last_os_error()
        ));
    }
    if metadata.stx_mask & requested != requested {
        return Err(format!(
            "kernel did not report a complete mount identity for {}",
            entry.path.display()
        ));
    }
    let kind = match u32::from(metadata.stx_mode) & libc::S_IFMT {
        value if value == libc::S_IFDIR => InspectedKind::Directory,
        value if value == libc::S_IFREG => InspectedKind::RegularFile,
        value if value == libc::S_IFSOCK => InspectedKind::Socket,
        value if value == libc::S_IFLNK => InspectedKind::Symlink,
        _ => InspectedKind::Other,
    };
    let device = libc::makedev(metadata.stx_dev_major, metadata.stx_dev_minor);
    if kind != entry.kind
        || device != entry.device
        || metadata.stx_ino != entry.inode
        || metadata.stx_uid != entry.uid
        || u32::from(metadata.stx_mode) & 0o7777 != entry.mode
    {
        return Err(format!(
            "asset entry changed while reading its mount identity: {}",
            entry.path.display()
        ));
    }
    Ok(MountIdentity {
        device,
        mount_id: metadata.stx_mnt_id,
    })
}

fn remove_prepared(
    prepared: PreparedAsset<'_>,
    class: AssetClass,
    marker: Option<&str>,
) -> Result<(), String> {
    let PreparedAsset {
        temp,
        temp_entry,
        inspector,
        lease,
        inventory,
        allowed_mount,
    } = prepared;
    revalidate_root(temp, &temp_entry, allowed_mount)?;
    let isolated_root =
        if is_verified_tombstone(&temp_entry, class, marker).map_err(|(_, error)| error)? {
            temp_entry.clone()
        } else {
            let name = cleanup_tombstone_name(class, marker, &temp_entry);
            temp.isolate_direct_child_directory(&temp_entry, std::ffi::OsStr::new(&name))
                .map_err(|error| format!("isolate {}: {error}", temp_entry.path.display()))?
        };

    #[cfg(test)]
    AFTER_ISOLATE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(&temp_entry.path);
        }
    });

    let lease_path = lease.as_ref().map(|lease| lease.path().to_path_buf());
    let marker_path = marker.map(|_| inspector.path().join(SCRATCH_MARKER));
    let mut files = inventory
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == InspectedKind::RegularFile
                && lease_path.as_ref() != Some(&entry.path)
                && marker_path.as_ref() != Some(&entry.path)
        })
        .cloned()
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in files {
        inspector
            .bind_regular_file_for_removal(&entry)
            .and_then(BoundStateRemoval::remove)
            .map_err(|error| format!("remove {}: {error}", entry.path.display()))?;
    }

    let mut directories = inventory
        .directories
        .values()
        .filter(|entry| entry.path != inspector.path())
        .cloned()
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        path_depth(&right.path)
            .cmp(&path_depth(&left.path))
            .then_with(|| right.path.cmp(&left.path))
    });
    for original in directories {
        let current = refresh_entry(&inspector, &inventory.directories, &original)?;
        inspector
            .bind_empty_directory_for_removal(&current)
            .and_then(BoundStateRemoval::remove)
            .map_err(|error| format!("remove {}: {error}", current.path.display()))?;
    }

    if let Some(marker_path) = marker_path {
        if let Some(marker_entry) = inventory
            .entries
            .iter()
            .find(|entry| entry.path == marker_path)
        {
            inspector
                .bind_regular_file_for_removal(marker_entry)
                .and_then(BoundStateRemoval::remove)
                .map_err(|error| format!("remove {}: {error}", marker_path.display()))?;
        }
    }

    if let Some(lease) = lease {
        let lease_path = lease.path().to_path_buf();
        lease
            .remove()
            .map_err(|error| format!("remove {}: {error}", lease_path.display()))?;
    }

    let current_root = refresh_entry(
        temp,
        &BTreeMap::from([(temp.path().to_path_buf(), temp.root().clone())]),
        &isolated_root,
    )?;
    temp.bind_empty_directory_for_removal(&current_root)
        .and_then(BoundStateRemoval::remove)
        .map_err(|error| format!("remove {}: {error}", current_root.path.display()))
}

fn refresh_entry(
    inspector: &StateInspector,
    directories: &BTreeMap<PathBuf, InspectedEntry>,
    original: &InspectedEntry,
) -> Result<InspectedEntry, String> {
    let parent_path = original
        .path
        .parent()
        .ok_or_else(|| format!("{} has no parent", original.path.display()))?;
    let expected_parent = directories
        .get(parent_path)
        .ok_or_else(|| format!("{} has no inventoried parent", original.path.display()))?;
    let snapshot = if parent_path == inspector.path() {
        inspector.inspect_root(limits())
    } else {
        let relative = parent_path
            .strip_prefix(inspector.path())
            .map_err(|_| format!("{} is outside the asset", parent_path.display()))?;
        inspector
            .inspect_directory(relative, limits())
            .and_then(|snapshot| {
                snapshot.ok_or_else(|| cyclops_state::StateError::UnsafePath {
                    path: parent_path.to_path_buf(),
                    reason: "asset parent changed before removal",
                })
            })
    }
    .map_err(|error| error.to_string())?;
    if snapshot.truncated || !same_stable(&snapshot.directory, expected_parent) {
        return Err(format!(
            "asset parent changed before removal: {}",
            parent_path.display()
        ));
    }
    let current = snapshot
        .entries
        .into_iter()
        .find(|entry| entry.path == original.path)
        .ok_or_else(|| {
            format!(
                "asset entry changed before removal: {}",
                original.path.display()
            )
        })?;
    if !same_stable(&current, original) {
        return Err(format!(
            "asset entry changed before removal: {}",
            original.path.display()
        ));
    }
    Ok(current)
}

fn revalidate_root(
    temp: &StateInspector,
    original: &InspectedEntry,
    allowed_mount: MountIdentity,
) -> Result<(), String> {
    if !temp
        .path_matches_held_root()
        .map_err(|error| error.to_string())?
    {
        return Err("temporary-root identity changed before apply".to_string());
    }
    let directories = BTreeMap::from([(temp.path().to_path_buf(), temp.root().clone())]);
    let current = refresh_entry(temp, &directories, original)?;
    if !same_exact(&current, original) || mount_identity(&current)? != allowed_mount {
        return Err("asset-root metadata changed before apply".to_string());
    }
    Ok(())
}

fn revalidate_named_root(
    original: &InspectedEntry,
    allowed_mount: MountIdentity,
) -> Result<(), String> {
    let current = StateInspector::open_existing(&original.path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "asset root changed during inventory".to_string())?;
    if !same_exact(current.root(), original)
        || !current
            .path_matches_held_root()
            .map_err(|error| error.to_string())?
        || mount_identity(current.root())? != allowed_mount
    {
        return Err("asset-root identity or metadata changed during inventory".to_string());
    }
    Ok(())
}

fn same_exact(left: &InspectedEntry, right: &InspectedEntry) -> bool {
    same_stable(left, right) && left.links == right.links && left.size == right.size
}

fn same_stable(left: &InspectedEntry, right: &InspectedEntry) -> bool {
    left.kind == right.kind
        && left.device == right.device
        && left.inode == right.inode
        && left.uid == right.uid
        && left.mode == right.mode
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

fn limits() -> InspectionLimits {
    InspectionLimits::new(ENTRY_LIMIT, NAME_BYTES_LIMIT)
        .expect("cleanup limits fit state-inspection hard ceilings")
}

fn render_json(report: &CleanupReport) -> Value {
    json!({
        "schema": 1,
        "mode": if report.apply { "apply" } else { "dry_run" },
        "ok": report.issues.is_empty(),
        "temporary_root": {
            "path": report.temp_root.display().to_string(),
            "state": report.temp_root_state,
        },
        "eligible": ["build_cache", "update_scratch"],
        "candidates": report.candidates.iter().map(|candidate| json!({
            "class": candidate.class.name(),
            "path": candidate.path.display().to_string(),
            "state": candidate.state.name(),
            "entries": candidate.entries,
            "bytes": candidate.bytes,
            "reason": candidate.reason,
        })).collect::<Vec<_>>(),
        "excluded": report.excluded.iter().map(|excluded| json!({
            "class": excluded.class,
            "state": excluded.state,
            "reason": excluded.reason,
        })).collect::<Vec<_>>(),
        "limits": {
            "entries": ENTRY_LIMIT,
            "name_bytes": NAME_BYTES_LIMIT,
            "depth": DEPTH_LIMIT,
            "marker_bytes": MARKER_BYTES_LIMIT,
        },
        "issues": report.issues,
    })
}

fn render_plain(report: &CleanupReport) -> String {
    let mut lines = vec![format!(
        "cleanup {} · temporary root {} · {}",
        if report.apply { "apply" } else { "dry run" },
        report.temp_root.display(),
        report.temp_root_state
    )];
    lines.push("  eligible  build_cache, update_scratch".to_string());
    for candidate in &report.candidates {
        lines.push(format!(
            "  {}  {}  {} entries · {} bytes",
            candidate.class.name(),
            candidate.state.name(),
            candidate.entries,
            candidate.bytes
        ));
        lines.push(format!("    {}", candidate.path.display()));
        lines.push(format!("    {}", candidate.reason));
    }
    lines.push("  excluded".to_string());
    for excluded in &report.excluded {
        lines.push(format!(
            "    {}  {}  {}",
            excluded.class, excluded.state, excluded.reason
        ));
    }
    if !report.issues.is_empty() {
        lines.push("  issues".to_string());
        for issue in &report.issues {
            lines.push(format!("    {issue}"));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::rc::Rc;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn private_temp() -> tempfile::TempDir {
        let temp_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("cyclops-cleanup-")
            .tempdir_in(temp_root)
            .unwrap();
        set_mode(temp.path(), 0o700);
        temp
    }

    fn build_cache(temp: &Path) -> PathBuf {
        let path = temp.join("cyclops-build-cache-1234");
        fs::create_dir(&path).unwrap();
        set_mode(&path, 0o700);
        fs::write(path.join(BUILD_CACHE_LEASE), b"").unwrap();
        set_mode(&path.join(BUILD_CACHE_LEASE), 0o600);
        fs::create_dir(path.join("nested")).unwrap();
        set_mode(&path.join("nested"), 0o700);
        fs::write(path.join("nested/output"), b"rebuildable").unwrap();
        path
    }

    fn isolated_build_cache(temp: &Path) -> PathBuf {
        fs::read_dir(temp)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(CLEANUP_BUILD_PREFIX))
            })
            .expect("failed cleanup retains the isolated build cache")
    }

    fn operational_candidate(
        temp: &Path,
        path: &Path,
        class: AssetClass,
        marker: Option<&str>,
    ) -> OperationalAssetCandidate {
        let inspector = StateInspector::open_existing(temp)
            .unwrap()
            .expect("private temporary root");
        let snapshot = inspector.inspect_root(limits()).unwrap();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        inspect_operational_candidate(&inspector, entry, class, marker)
    }

    fn update_scratch(temp: &Path, nonce: &str) -> PathBuf {
        let path = temp.join(format!("{UPDATE_PREFIX}{nonce}"));
        fs::create_dir(&path).unwrap();
        set_mode(&path, 0o700);
        fs::write(path.join(SCRATCH_MARKER), nonce.as_bytes()).unwrap();
        set_mode(&path.join(SCRATCH_MARKER), 0o600);
        fs::write(path.join(SCRATCH_LEASE), b"").unwrap();
        set_mode(&path.join(SCRATCH_LEASE), 0o600);
        fs::write(path.join("payload"), b"temporary").unwrap();
        path
    }

    #[test]
    fn dry_run_and_json_plain_rendering_use_one_report() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], false);

        assert!(report.issues.is_empty());
        assert!(cache.exists());
        assert_eq!(report.candidates[0].state, CandidateState::Ready);
        let json = render_json(&report).to_string();
        let plain = render_plain(&report);
        for fact in [
            "build_cache",
            "update_scratch",
            "ready",
            cache.to_str().unwrap(),
        ] {
            assert!(json.contains(fact));
            assert!(plain.contains(fact));
        }
        assert!(json.contains("state_journals_and_messages"));
        assert!(plain.contains("state_journals_and_messages"));
    }

    #[test]
    fn marked_assets_are_removed_and_absence_is_idempotent() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let nonce = "0123456789abcdef0123456789abcdef";
        let scratch = update_scratch(temp.path(), nonce);
        let classes = [AssetClass::BuildCache, AssetClass::UpdateScratch];

        let first = collect_at(temp.path(), &cache, &classes, true);
        assert!(first.issues.is_empty(), "{:?}", first.issues);
        assert!(!cache.exists());
        assert!(!scratch.exists());
        assert!(first
            .candidates
            .iter()
            .all(|candidate| candidate.state == CandidateState::Removed));

        let second = collect_at(temp.path(), &cache, &classes, true);
        assert!(second.issues.is_empty());
        assert!(second
            .candidates
            .iter()
            .all(|candidate| candidate.state == CandidateState::Absent));
    }

    #[test]
    fn an_active_lease_refuses_cleanup() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let lease = fs::File::open(cache.join(BUILD_CACHE_LEASE)).unwrap();
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Active);
        assert!(cache.exists());
        assert!(!report.issues.is_empty());
    }

    #[test]
    fn namespace_is_isolated_before_a_replacement_writer_can_enter() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let replacement_lease = Rc::new(RefCell::new(None));
        let lease_for_hook = Rc::clone(&replacement_lease);
        AFTER_ISOLATE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |original| {
                fs::create_dir(original).unwrap();
                set_mode(original, 0o700);
                let lease_path = original.join(BUILD_CACHE_LEASE);
                fs::write(&lease_path, b"").unwrap();
                set_mode(&lease_path, 0o600);
                let lease = fs::File::open(&lease_path).unwrap();
                assert_eq!(
                    unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
                fs::write(original.join("new-output"), b"new writer").unwrap();
                *lease_for_hook.borrow_mut() = Some(lease);
            }));
        });

        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(report.candidates[0].state, CandidateState::Removed);
        assert_eq!(fs::read(cache.join("new-output")).unwrap(), b"new writer");
        assert!(replacement_lease.borrow().is_some());
        assert!(!fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .any(|name| name.to_string_lossy().starts_with(CLEANUP_BUILD_PREFIX)));
    }

    #[test]
    fn an_interrupted_isolated_asset_is_discovered_on_the_next_run() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let inspector = StateInspector::open_existing(temp.path())
            .unwrap()
            .expect("private temporary root");
        let snapshot = inspector.inspect_root(limits()).unwrap();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == cache)
            .unwrap();
        let name = cleanup_tombstone_name(AssetClass::BuildCache, None, entry);
        let isolated = inspector
            .isolate_direct_child_directory(entry, std::ffi::OsStr::new(&name))
            .unwrap();
        assert!(!cache.exists());
        assert!(isolated.path.exists());

        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(report.candidates[0].state, CandidateState::Removed);
        assert!(!isolated.path.exists());
    }

    #[test]
    fn interrupted_cleanup_keeps_authority_through_tombstone_retirement() {
        let temp = private_temp();
        let nonce = "0123456789abcdef0123456789abcdef";
        let scratch = update_scratch(temp.path(), nonce);
        let temp_inspector = StateInspector::open_existing(temp.path())
            .unwrap()
            .expect("private temporary root");
        let scratch_entry = temp_inspector
            .inspect_root(limits())
            .unwrap()
            .entries
            .into_iter()
            .find(|entry| entry.path == scratch)
            .unwrap();
        let scratch_name =
            cleanup_tombstone_name(AssetClass::UpdateScratch, Some(nonce), &scratch_entry);
        let isolated_scratch = temp_inspector
            .isolate_direct_child_directory(&scratch_entry, std::ffi::OsStr::new(&scratch_name))
            .unwrap();
        fs::remove_file(isolated_scratch.path.join(SCRATCH_MARKER)).unwrap();

        let absent_cache = temp.path().join("cyclops-build-cache-77");
        let report = collect_at(
            temp.path(),
            &absent_cache,
            &[AssetClass::UpdateScratch],
            true,
        );
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(report.candidates[0].state, CandidateState::Removed);
        assert!(!isolated_scratch.path.exists());

        let cache = build_cache(temp.path());
        let cache_entry = temp_inspector
            .inspect_root(limits())
            .unwrap()
            .entries
            .into_iter()
            .find(|entry| entry.path == cache)
            .unwrap();
        let cache_name = cleanup_tombstone_name(AssetClass::BuildCache, None, &cache_entry);
        let isolated_cache = temp_inspector
            .isolate_direct_child_directory(&cache_entry, std::ffi::OsStr::new(&cache_name))
            .unwrap();
        fs::remove_file(isolated_cache.path.join("nested/output")).unwrap();
        fs::remove_dir(isolated_cache.path.join("nested")).unwrap();
        fs::remove_file(isolated_cache.path.join(BUILD_CACHE_LEASE)).unwrap();

        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(report.candidates[0].state, CandidateState::Removed);
        assert!(!isolated_cache.path.exists());
    }

    #[test]
    fn a_tombstone_must_encode_the_directory_identity() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let inspector = StateInspector::open_existing(temp.path())
            .unwrap()
            .expect("private temporary root");
        let entry = inspector
            .inspect_root(limits())
            .unwrap()
            .entries
            .into_iter()
            .find(|entry| entry.path == cache)
            .unwrap();
        let wrong_name = format!(
            "{CLEANUP_BUILD_PREFIX}{}.{:016x}.{:016x}",
            cache_key(&cache).unwrap(),
            entry.device,
            entry.inode.wrapping_add(1)
        );
        let isolated = inspector
            .isolate_direct_child_directory(&entry, std::ffi::OsStr::new(&wrong_name))
            .unwrap();

        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Unsafe);
        assert!(report.candidates[0]
            .reason
            .contains("tombstone identity does not match"));
        assert!(isolated.path.join("nested/output").is_file());
    }

    #[test]
    fn a_lease_less_tombstone_cannot_retire_payload() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let inspector = StateInspector::open_existing(temp.path())
            .unwrap()
            .expect("private temporary root");
        let entry = inspector
            .inspect_root(limits())
            .unwrap()
            .entries
            .into_iter()
            .find(|entry| entry.path == cache)
            .unwrap();
        let name = cleanup_tombstone_name(AssetClass::BuildCache, None, &entry);
        let isolated = inspector
            .isolate_direct_child_directory(&entry, std::ffi::OsStr::new(&name))
            .unwrap();
        fs::remove_file(isolated.path.join(BUILD_CACHE_LEASE)).unwrap();

        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Unsafe);
        assert!(report.candidates[0]
            .reason
            .contains("before payload retirement"));
        assert!(isolated.path.join("nested/output").is_file());
    }

    #[test]
    fn inventory_refuses_root_and_descendant_mount_boundaries() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let inspector = StateInspector::open_existing(&cache)
            .unwrap()
            .expect("build cache");
        let allowed = MountIdentity {
            device: inspector.root().device,
            mount_id: 41,
        };
        let root_error = inventory_with_mount_probe(&inspector, allowed, |entry| {
            Ok(MountIdentity {
                device: entry.device,
                mount_id: 42,
            })
        })
        .err()
        .expect("a candidate-root mount boundary must fail closed");
        assert_eq!(root_error.0, CandidateState::Unsafe);
        assert!(root_error.1.contains("mount boundary"));

        let nested = cache.join("nested");
        let nested_error = inventory_with_mount_probe(&inspector, allowed, |entry| {
            Ok(MountIdentity {
                device: entry.device,
                mount_id: if entry.path == nested { 42 } else { 41 },
            })
        })
        .err()
        .expect("a same-device descendant mount boundary must fail closed");
        assert_eq!(nested_error.0, CandidateState::Unsafe);
        assert!(nested_error.1.contains("mount boundary"));
        assert!(cache.join("nested/output").is_file());
    }

    #[test]
    fn links_and_unsafe_modes_refuse_without_touching_external_bytes() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        set_mode(&outside, 0o640);
        symlink(&outside, cache.join("linked")).unwrap();

        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Unsafe);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert_eq!(mode(&outside), 0o640);
        assert!(cache.join("linked").is_symlink());

        fs::remove_file(cache.join("linked")).unwrap();
        fs::hard_link(&outside, cache.join("linked")).unwrap();
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Unsafe);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");

        set_mode(&cache, 0o755);
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Unsafe);
        assert!(cache.exists());
    }

    #[test]
    fn operational_inventory_refuses_nested_links_and_unsafe_modes() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let outside = temp.path().join("outside-operational");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, cache.join("nested/linked")).unwrap();
        let report = operational_candidate(temp.path(), &cache, AssetClass::BuildCache, None);
        assert!(!report.safe);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");

        fs::remove_file(cache.join("nested/linked")).unwrap();
        fs::hard_link(&outside, cache.join("nested/linked")).unwrap();
        let report = operational_candidate(temp.path(), &cache, AssetClass::BuildCache, None);
        assert!(!report.safe);
        assert_eq!(fs::read(&outside).unwrap(), b"outside");

        fs::remove_file(cache.join("nested/linked")).unwrap();
        set_mode(&cache.join("nested"), 0o755);
        let report = operational_candidate(temp.path(), &cache, AssetClass::BuildCache, None);
        assert!(!report.safe);
    }

    #[test]
    fn operational_inventory_revalidates_the_named_root_after_recursion() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let displaced = temp.path().join("displaced-operational-cache");
        AFTER_OPERATIONAL_INVENTORY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |path| {
                fs::rename(path, &displaced).unwrap();
                fs::create_dir(path).unwrap();
                set_mode(path, 0o700);
            }));
        });

        let report = operational_candidate(temp.path(), &cache, AssetClass::BuildCache, None);
        assert!(!report.safe);
        assert!(report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("changed during inventory")));
        assert!(temp
            .path()
            .join("displaced-operational-cache/nested/output")
            .is_file());
    }

    #[test]
    fn operational_update_scratch_requires_its_exact_marker_and_lease() {
        let temp = private_temp();
        let nonce = "0123456789abcdef0123456789abcdef";
        let scratch = update_scratch(temp.path(), nonce);
        let current = operational_candidate(
            temp.path(),
            &scratch,
            AssetClass::UpdateScratch,
            Some(nonce),
        );
        assert!(current.safe);
        assert_eq!(current.marker, "current");
        assert_eq!(current.lease, "current");

        fs::remove_file(scratch.join(SCRATCH_MARKER)).unwrap();
        let missing_marker = operational_candidate(
            temp.path(),
            &scratch,
            AssetClass::UpdateScratch,
            Some(nonce),
        );
        assert!(!missing_marker.safe);
        assert_eq!(missing_marker.marker, "unproven");

        fs::write(scratch.join(SCRATCH_MARKER), nonce).unwrap();
        set_mode(&scratch.join(SCRATCH_MARKER), 0o600);
        fs::remove_file(scratch.join(SCRATCH_LEASE)).unwrap();
        let missing_lease = operational_candidate(
            temp.path(),
            &scratch,
            AssetClass::UpdateScratch,
            Some(nonce),
        );
        assert!(!missing_lease.safe);
        assert_eq!(missing_lease.lease, "unproven");
    }

    #[test]
    fn inode_and_mode_changes_after_inventory_refuse_apply() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        let original = temp.path().join("original-cache");
        let original_for_hook = original.clone();
        let replacement = cache.clone();
        BEFORE_APPLY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |path| {
                fs::rename(path, &original_for_hook).unwrap();
                fs::create_dir(&replacement).unwrap();
                set_mode(&replacement, 0o700);
            }));
        });
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Failed);
        assert!(cache.is_dir());
        assert!(original.join("nested/output").is_file());

        fs::remove_dir(&cache).unwrap();
        fs::rename(&original, &cache).unwrap();
        BEFORE_APPLY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|path| set_mode(path, 0o755)));
        });
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Failed);
        assert!(cache.exists());
        assert_eq!(mode(&cache), 0o755);
    }

    #[test]
    fn nested_regular_file_changes_after_inventory_refuse_apply() {
        let temp = private_temp();
        let cache = build_cache(temp.path());
        BEFORE_APPLY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|path| {
                set_mode(&path.join("nested/output"), 0o666)
            }));
        });
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Failed);
        assert_eq!(
            mode(&isolated_build_cache(temp.path()).join("nested/output")),
            0o666
        );

        let temp = private_temp();
        let cache = build_cache(temp.path());
        let outside = temp.path().join("outside-hardlink");
        BEFORE_APPLY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |path| {
                fs::hard_link(path.join("nested/output"), &outside).unwrap();
            }));
        });
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Failed);
        let isolated = isolated_build_cache(temp.path());
        assert!(isolated.join("nested/output").is_file());
        assert_eq!(
            fs::symlink_metadata(isolated.join("nested/output"))
                .unwrap()
                .nlink(),
            2
        );

        let temp = private_temp();
        let cache = build_cache(temp.path());
        BEFORE_APPLY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|path| {
                let output = path.join("nested/output");
                fs::rename(&output, path.join("nested/displaced")).unwrap();
                fs::write(&output, b"replacement").unwrap();
            }));
        });
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.candidates[0].state, CandidateState::Failed);
        let isolated = isolated_build_cache(temp.path());
        assert_eq!(
            fs::read(isolated.join("nested/output")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            fs::read(isolated.join("nested/displaced")).unwrap(),
            b"rebuildable"
        );
    }

    #[test]
    fn a_writable_update_marker_is_unsafe() {
        let temp = private_temp();
        let cache = temp.path().join("absent-cache");
        let nonce = "0123456789abcdef0123456789abcdef";
        let scratch = update_scratch(temp.path(), nonce);
        set_mode(&scratch.join(SCRATCH_MARKER), 0o666);

        let report = collect_at(temp.path(), &cache, &[AssetClass::UpdateScratch], true);
        assert_eq!(report.candidates[0].state, CandidateState::Unsafe);
        assert!(scratch.exists());
        assert_eq!(mode(&scratch.join(SCRATCH_MARKER)), 0o666);
    }

    #[test]
    fn foreign_or_shared_temporary_roots_are_unsupported() {
        let temp = tempfile::tempdir().unwrap();
        set_mode(temp.path(), 0o1777);
        let cache = temp.path().join("cyclops-build-cache-test");
        let report = collect_at(temp.path(), &cache, &[AssetClass::BuildCache], true);
        assert_eq!(report.temp_root_state, "unsupported");
        assert_eq!(report.candidates[0].state, CandidateState::Unsupported);
        assert!(!cache.exists());
    }
}
