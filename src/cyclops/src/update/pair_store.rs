//! Binary pairing, verification, and transactional selection store.

use std::io::Seek as _;
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use cyclops_state::{
    DirectoryInspection, InspectedEntry, InspectedKind, InspectionLimits, LinkInspection,
    StateInspector,
};

use super::*;

pub(crate) const PAIR_ROOT: &str = ".cyclops-pairs";
pub(crate) const PAIRS_DIR: &str = "pairs";
pub(crate) const SELECTIONS_DIR: &str = "selections";
pub(crate) const ACTIVE_SELECTOR: &str = "active";
pub(crate) const PAIR_DESCRIPTOR: &str = "state.json";
pub(crate) const PAIR_OWNER: &str = ".owner";
pub(crate) const PAIR_LEASE: &str = ".lease";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateBoundary {
    PairStoreRootCreated,
    PairStoreOwnerWritten,
    PairStoreLeaseCreated,
    PairStorePairsCreated,
    PairStoreSelectionsCreated,
    PairDirectoryCreated,
    ClientCopied,
    DaemonCopied,
    PairPublished,
    SelectionDirectoryCreated,
    ClientSelectionLinked,
    DaemonSelectionLinked,
    SelectionDescriptorWritten,
    SelectionPublished,
    SelectorTemporaryCreated,
    SelectorCommitted,
    SelectorPublished,
    PublicDaemonTemporaryCreated,
    PublicDaemonCommitted,
    PublicDaemonPublished,
    PublicClientTemporaryCreated,
    PublicClientCommitted,
    PublicClientPublished,
}

#[cfg(test)]
thread_local! {
    pub(crate) static CRASH_AT_UPDATE_BOUNDARY: std::cell::Cell<Option<UpdateBoundary>> = const {
        std::cell::Cell::new(None)
    };
    pub(crate) static FAIL_NEXT_SELECTOR_DIRECTORY_SYNC: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

pub(crate) fn crossed_update_boundary(boundary: UpdateBoundary) {
    #[cfg(test)]
    CRASH_AT_UPDATE_BOUNDARY.with(|selected| {
        if selected.get() == Some(boundary) {
            std::panic::panic_any(boundary);
        }
    });
    #[cfg(not(test))]
    let _ = boundary;
}

pub(crate) struct PairStore {
    pub(crate) prefix: PathBuf,
    pub(crate) root: PathBuf,
    pub(crate) root_device: u64,
    pub(crate) root_inode: u64,
    pub(crate) owner_device: u64,
    pub(crate) owner_inode: u64,
    pub(crate) lease_device: u64,
    pub(crate) lease_inode: u64,
    pub(crate) _lease: ExclusiveLease,
}

// Owner-only directories separate local accounts. The kernel lease and inode
// rechecks prevent cooperating same-user operations from racing one another.
// A hostile process already running as the same uid is outside this boundary.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) id: String,
    pub(crate) active: String,
    pub(crate) known_good: String,
    pub(crate) legacy_active: bool,
    pub(crate) active_proof: Option<PairProof>,
    pub(crate) known_good_proof: Option<PairProof>,
    pub(crate) active_replay: Option<ReplayAttestation>,
    pub(crate) known_good_replay: Option<ReplayAttestation>,
}

/// The selector rename either has not become visible yet, or it has. Callers
/// must not flatten those states into one generic failure: after the rename,
/// the pair the public links resolve through may already have changed.
#[derive(Debug)]
pub(crate) enum SelectorPublicationError {
    BeforeVisible(String),
    Visible {
        selection: Box<Selection>,
        error: String,
    },
}

impl SelectorPublicationError {
    pub(crate) fn before(error: impl Into<String>) -> Self {
        Self::BeforeVisible(error.into())
    }

    pub(crate) fn visible(selection: Selection, error: impl Into<String>) -> Self {
        Self::Visible {
            selection: Box::new(selection),
            error: error.into(),
        }
    }
}

impl std::fmt::Display for SelectorPublicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeVisible(error) => formatter.write_str(error),
            Self::Visible { selection, error } => write!(
                formatter,
                "the active selector now names {}, but its durability confirmation failed: {error}",
                selection.active
            ),
        }
    }
}

/// A public pair-change failure that keeps selector visibility explicit. A
/// caller must restore a prior selection before starting a daemon when this
/// operation made a new selector visible.
#[derive(Debug)]
pub(crate) enum PairChangeError {
    SelectorUnchanged(String),
    SelectorVisible {
        previous: Option<Box<Selection>>,
        selection: Box<Selection>,
        error: String,
    },
}

impl PairChangeError {
    pub(crate) fn unchanged(error: impl Into<String>) -> Self {
        Self::SelectorUnchanged(error.into())
    }

    pub(crate) fn after_selector_publication(
        previous: Option<Selection>,
        error: SelectorPublicationError,
    ) -> Self {
        match error {
            SelectorPublicationError::BeforeVisible(error) => Self::SelectorUnchanged(error),
            SelectorPublicationError::Visible { selection, error } => Self::SelectorVisible {
                previous: previous.map(Box::new),
                selection,
                error: format!("selector durability confirmation failed: {error}"),
            },
        }
    }

    pub(crate) fn after_visible_selector(
        previous: Option<Selection>,
        selection: Selection,
        error: impl Into<String>,
    ) -> Self {
        Self::SelectorVisible {
            previous: previous.map(Box::new),
            selection: Box::new(selection),
            error: error.into(),
        }
    }

    pub(crate) fn selector_is_visible(&self) -> bool {
        matches!(self, Self::SelectorVisible { .. })
    }

    pub(crate) fn previous(&self) -> Option<&Selection> {
        match self {
            Self::SelectorUnchanged(_) => None,
            Self::SelectorVisible { previous, .. } => previous.as_deref(),
        }
    }
}

impl std::fmt::Display for PairChangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SelectorUnchanged(error) => formatter.write_str(error),
            Self::SelectorVisible {
                selection, error, ..
            } => write!(
                formatter,
                "the active selector now names {}, but the pair change did not finish: {error}",
                selection.active
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PairProof {
    pub(crate) identity: String,
    pub(crate) cyclops_sha256: String,
    pub(crate) cyclopsd_sha256: String,
}

/// Durable evidence that one exact pair booted one private state snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ReplayAttestation {
    pub(crate) schema: u32,
    pub(crate) pair: PairProof,
    pub(crate) snapshot_sha256: String,
    pub(crate) snapshot_entries: u64,
    pub(crate) snapshot_bytes: u64,
}

/// Read-only rollback proof consumed by health reporting.
#[derive(Debug)]
pub(crate) struct InstalledPairDescriptor {
    pub(crate) selection: PathBuf,
    pub(crate) active_pair: PathBuf,
    pub(crate) known_good_pair: PathBuf,
    pub(crate) active_identity: Option<String>,
    pub(crate) known_good_identity: Option<String>,
    pub(crate) active_build: Option<String>,
    pub(crate) known_good_build: Option<String>,
    pub(crate) active_replay_attested: bool,
    pub(crate) known_good_replay_attested: bool,
    pub(crate) known_good_replay_snapshot: Option<String>,
    pub(crate) proof_unproven: bool,
    pub(crate) rollback_safe: bool,
}

#[derive(Debug)]
pub(crate) enum InstalledPairInspectionError {
    ConcurrentChange(String),
    Invalid(String),
}

impl From<String> for InstalledPairInspectionError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl std::fmt::Display for InstalledPairInspectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConcurrentChange(message) | Self::Invalid(message) => {
                formatter.write_str(message)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum PairStoreOpenError {
    UpdateActive(String),
    Invalid(String),
}

impl From<String> for PairStoreOpenError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl std::fmt::Display for PairStoreOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpdateActive(message) | Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_PAIR_INSPECTION_RECHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn before_pair_inspection_recheck() {
    BEFORE_PAIR_INSPECTION_RECHECK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
pub(crate) fn before_pair_inspection_recheck() {}

/// Inspect one immutable selected-pair snapshot without taking the updater lease.
pub(crate) fn installed_pair_descriptor(
    prefix: &Path,
) -> Result<Option<InstalledPairDescriptor>, InstalledPairInspectionError> {
    let root = prefix.join(PAIR_ROOT);
    let Some(inspector) = StateInspector::open_existing(&root)
        .map_err(|error| InstalledPairInspectionError::Invalid(error.to_string()))?
    else {
        return Ok(None);
    };
    let selector = inspector
        .read_link(Path::new(ACTIVE_SELECTOR), 512)
        .map_err(|error| InstalledPairInspectionError::Invalid(error.to_string()))?
        .ok_or_else(|| {
            InstalledPairInspectionError::Invalid(
                "the managed pair store has no active selector".to_string(),
            )
        })?;
    let result = inspect_installed_pair_snapshot(&inspector, &selector);
    before_pair_inspection_recheck();
    let root_matches = inspector.path_matches_held_root().unwrap_or(false);
    let selector_matches = inspector
        .read_link(Path::new(ACTIVE_SELECTOR), 512)
        .ok()
        .flatten()
        .as_ref()
        == Some(&selector);
    if !root_matches || !selector_matches {
        return Err(InstalledPairInspectionError::ConcurrentChange(
            "the managed pair selection changed during health inspection".to_string(),
        ));
    }
    result
        .map(Some)
        .map_err(InstalledPairInspectionError::Invalid)
}

fn inspect_installed_pair_snapshot(
    inspector: &StateInspector,
    selector: &LinkInspection,
) -> Result<InstalledPairDescriptor, String> {
    require_inspected_directory(inspector.root(), 0o700, "pair store")?;
    let owner = inspector
        .read_file(Path::new(PAIR_OWNER), 64)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the pair store has no ownership marker".to_string())?;
    require_inspected_regular(&owner.entry, 0o600, "pair owner marker")?;
    let owner = std::str::from_utf8(&owner.bytes)
        .map_err(|_| "the pair owner marker is not UTF-8".to_string())?;
    if owner != unsafe { libc::geteuid() }.to_string() {
        return Err("pair store ownership marker does not match this user".to_string());
    }
    let lease = inspector
        .read_file(Path::new(PAIR_LEASE), 1)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the pair store has no updater lease file".to_string())?;
    require_inspected_regular(&lease.entry, 0o600, "pair updater lease")?;

    let target = selector
        .target
        .to_str()
        .ok_or_else(|| "the active selector is not UTF-8".to_string())?;
    validate_selection_target(target)?;
    let selection_directory = inspector
        .inspect_directory(Path::new(target), InspectionLimits::default())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the active selection directory is missing".to_string())?;
    require_selection_snapshot_layout(&selection_directory)?;
    let descriptor_relative = Path::new(target).join(PAIR_DESCRIPTOR);
    let descriptor = inspector
        .read_file(&descriptor_relative, 1024 * 1024)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the selected pair descriptor is missing".to_string())?;
    require_inspected_regular(&descriptor.entry, 0o600, "selected pair descriptor")?;
    if descriptor.truncated {
        return Err("the selected pair descriptor exceeds its read bound".to_string());
    }
    let selection = decode_selection(target, &descriptor.bytes)?;
    for name in ["cyclops", "cyclopsd"] {
        let selected = inspector
            .read_link(&Path::new(target).join(name), 512)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("selected {name} link is missing"))?;
        let expected = PathBuf::from("../..").join(&selection.active).join(name);
        if selected.target != expected {
            return Err(format!("selected {name} does not name the active pair"));
        }
    }

    let active_pair = inspector.path().join(&selection.active);
    let known_good_pair = inspector.path().join(&selection.known_good);
    let active_proof = selection.active_proof.clone();
    let known_good_proof = selection.known_good_proof.clone();
    let proof_unproven =
        known_good_proof.is_none() || (!selection.legacy_active && active_proof.is_none());
    if let Some(proof) = active_proof.as_ref() {
        verify_recorded_pair_snapshot(inspector, &selection.active, proof)?;
    }
    if let Some(proof) = known_good_proof.as_ref() {
        verify_recorded_pair_snapshot(inspector, &selection.known_good, proof)?;
    }
    if let Some(attestation) = selection.active_replay.as_ref() {
        let proof = active_proof.as_ref().ok_or_else(|| {
            "the active replay attestation has no recorded pair identity".to_string()
        })?;
        verify_replay_attestation(attestation, proof)?;
    }
    if let Some(attestation) = selection.known_good_replay.as_ref() {
        let proof = known_good_proof.as_ref().ok_or_else(|| {
            "the known-good replay attestation has no recorded pair identity".to_string()
        })?;
        verify_replay_attestation(attestation, proof)?;
    }
    let active_identity = active_proof.as_ref().map(|proof| proof.identity.clone());
    let known_good_identity = known_good_proof.map(|proof| proof.identity);
    let active_build = active_identity.as_deref().map(identity_build).transpose()?;
    let known_good_build = known_good_identity
        .as_deref()
        .map(identity_build)
        .transpose()?;
    inspector
        .inspect_bound_directory(&selection_directory.directory, InspectionLimits::default())
        .map_err(|error| error.to_string())?;
    Ok(InstalledPairDescriptor {
        selection: inspector.path().join(&selection.id),
        active_pair,
        known_good_pair,
        active_identity,
        known_good_identity,
        active_build,
        known_good_build,
        active_replay_attested: selection.active_replay.is_some(),
        known_good_replay_attested: selection.known_good_replay.is_some(),
        known_good_replay_snapshot: selection
            .known_good_replay
            .map(|attestation| attestation.snapshot_sha256),
        proof_unproven,
        rollback_safe: !proof_unproven
            && !selection.legacy_active
            && selection.active != selection.known_good,
    })
}

pub(crate) fn require_selection_snapshot_layout(
    snapshot: &DirectoryInspection,
) -> Result<(), String> {
    require_inspected_directory(&snapshot.directory, 0o700, "selection directory")?;
    if snapshot.truncated || snapshot.entries.len() != 3 {
        return Err("the selected pair directory has unexpected entries".to_string());
    }
    for name in ["cyclops", "cyclopsd", PAIR_DESCRIPTOR] {
        if !snapshot
            .entries
            .iter()
            .any(|entry| entry.path.file_name() == Some(std::ffi::OsStr::new(name)))
        {
            return Err(format!("the selected pair directory is missing {name}"));
        }
    }
    Ok(())
}

pub(crate) fn verify_recorded_pair_snapshot(
    inspector: &StateInspector,
    target: &str,
    proof: &PairProof,
) -> Result<(), String> {
    validate_pair_target(target)?;
    let snapshot = inspector
        .inspect_directory(Path::new(target), InspectionLimits::default())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!(
                "selected pair {} is missing",
                inspector.path().join(target).display()
            )
        })?;
    require_inspected_directory(&snapshot.directory, 0o700, "selected pair")?;
    if snapshot.truncated || snapshot.entries.len() != 2 {
        return Err(format!(
            "selected pair {} has unexpected entries",
            inspector.path().join(target).display()
        ));
    }
    let mut digests = Vec::with_capacity(2);
    for name in ["cyclops", "cyclopsd"] {
        let expected = snapshot
            .entries
            .iter()
            .find(|entry| entry.path.file_name() == Some(std::ffi::OsStr::new(name)))
            .ok_or_else(|| format!("selected pair is missing {name}"))?;
        require_inspected_executable(expected, name)?;
        let relative = Path::new(target).join(name);
        let (observed, digest) = inspector
            .inspect_file_with(&relative, MAX_PAIR_BINARY_BYTES, |file| {
                let mut hasher = Sha256::new();
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let read = file.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                }
                Ok(format!("{:x}", hasher.finalize()))
            })
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("selected pair is missing {name}"))?;
        if &observed != expected {
            return Err(format!("selected pair {name} changed during inspection"));
        }
        digests.push(digest);
    }
    inspector
        .inspect_bound_directory(&snapshot.directory, InspectionLimits::default())
        .map_err(|error| error.to_string())?;
    if digests[0] != proof.cyclops_sha256 || digests[1] != proof.cyclopsd_sha256 {
        return Err(format!(
            "selected pair {} changed after its install proof was recorded",
            inspector.path().join(target).display()
        ));
    }
    identity_build(&proof.identity)?;
    Ok(())
}

pub(crate) fn require_inspected_directory(
    entry: &InspectedEntry,
    mode: u32,
    kind: &str,
) -> Result<(), String> {
    if entry.kind != InspectedKind::Directory
        || entry.uid != unsafe { libc::geteuid() }
        || entry.mode & 0o777 != mode
    {
        return Err(format!(
            "{} is not an owner-only {kind}",
            entry.path.display()
        ));
    }
    Ok(())
}

pub(crate) fn require_inspected_regular(
    entry: &InspectedEntry,
    mode: u32,
    kind: &str,
) -> Result<(), String> {
    if entry.kind != InspectedKind::RegularFile
        || entry.uid != unsafe { libc::geteuid() }
        || entry.links != 1
        || entry.mode & 0o777 != mode
    {
        return Err(format!(
            "{} is not one owner-controlled {kind}",
            entry.path.display()
        ));
    }
    Ok(())
}

pub(crate) fn require_inspected_executable(
    entry: &InspectedEntry,
    name: &str,
) -> Result<(), String> {
    if entry.kind != InspectedKind::RegularFile
        || entry.uid != unsafe { libc::geteuid() }
        || entry.links != 1
        || entry.mode & 0o100 == 0
        || entry.mode & 0o022 != 0
    {
        return Err(format!(
            "{} is not one owner-controlled executable {name}",
            entry.path.display()
        ));
    }
    Ok(())
}

impl PairStore {
    pub(crate) fn open(prefix: &Path) -> Result<Self, String> {
        Self::open_inner(prefix, true)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "pair store was not created".to_string())
    }

    pub(crate) fn open_existing(prefix: &Path) -> Result<Option<Self>, PairStoreOpenError> {
        Self::open_inner(prefix, false)
    }

    pub(crate) fn open_inner(
        prefix: &Path,
        create: bool,
    ) -> Result<Option<Self>, PairStoreOpenError> {
        let prefix = std::fs::canonicalize(prefix)
            .map_err(|error| format!("resolve install prefix {}: {error}", prefix.display()))?;
        let root = prefix.join(PAIR_ROOT);
        let mut root_created = false;
        match std::fs::symlink_metadata(&root) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder
                    .create(&root)
                    .map_err(|error| format!("create pair store {}: {error}", root.display()))?;
                root_created = true;
                sync_directory(&prefix)?;
                crossed_update_boundary(UpdateBoundary::PairStoreRootCreated);
            }
            Err(error) => {
                return Err(format!("inspect pair store {}: {error}", root.display()).into())
            }
        }
        require_owner_directory(&root)?;
        let owner_marker = root.join(PAIR_OWNER);
        if create
            && std::fs::symlink_metadata(&owner_marker)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            if !read_directory(&root, "unfinished pair store")?.is_empty() {
                return Err("an unfinished pair store has unexpected entries"
                    .to_string()
                    .into());
            }
            write_new(
                &owner_marker,
                unsafe { libc::geteuid() }.to_string().as_bytes(),
                0o600,
            )?;
            sync_directory(&root)?;
            crossed_update_boundary(UpdateBoundary::PairStoreOwnerWritten);
        } else if root_created {
            return Err("new pair store did not create its owner marker"
                .to_string()
                .into());
        }
        require_owner_regular_file(&owner_marker, 0o600)?;
        let owner = std::fs::read_to_string(&owner_marker)
            .map_err(|error| format!("read pair owner marker: {error}"))?;
        if owner != unsafe { libc::geteuid() }.to_string() {
            return Err("pair store ownership marker does not match this user"
                .to_string()
                .into());
        }
        let lease_path = root.join(PAIR_LEASE);
        let lease_missing = std::fs::symlink_metadata(&lease_path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lease_path)
            .map_err(|error| format!("open pair update lease: {error}"))?;
        require_owner_regular_file(&lease_path, 0o600)?;
        if lease_missing {
            sync_directory(&root)?;
            crossed_update_boundary(UpdateBoundary::PairStoreLeaseCreated);
        }
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(PairStoreOpenError::UpdateActive(format!(
                "another Cyclops update holds the pair store lease: {}",
                std::io::Error::last_os_error()
            )));
        }
        let lease = ExclusiveLease(lease);
        let lease_metadata = lease
            .0
            .metadata()
            .map_err(|error| format!("inspect pair update lease: {error}"))?;
        let root_metadata = std::fs::symlink_metadata(&root)
            .map_err(|error| format!("inspect pair store: {error}"))?;
        let owner_metadata = std::fs::symlink_metadata(&owner_marker)
            .map_err(|error| format!("inspect pair owner marker: {error}"))?;
        for name in [PAIRS_DIR, SELECTIONS_DIR] {
            let directory = root.join(name);
            if create && !directory.exists() {
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&directory).map_err(|error| {
                    format!(
                        "create pair store directory {}: {error}",
                        directory.display()
                    )
                })?;
                sync_directory(&root)?;
                crossed_update_boundary(if name == PAIRS_DIR {
                    UpdateBoundary::PairStorePairsCreated
                } else {
                    UpdateBoundary::PairStoreSelectionsCreated
                });
            }
            require_owner_directory(&directory)?;
        }
        let store = Self {
            prefix,
            root,
            root_device: root_metadata.dev(),
            root_inode: root_metadata.ino(),
            owner_device: owner_metadata.dev(),
            owner_inode: owner_metadata.ino(),
            lease_device: lease_metadata.dev(),
            lease_inode: lease_metadata.ino(),
            _lease: lease,
        };
        if create {
            store.recover_interrupted_update()?;
        }
        Ok(Some(store))
    }

    /// Finish or remove only validated residue from an interrupted update.
    pub(crate) fn recover_interrupted_update(&self) -> Result<(), String> {
        self.require_root()?;
        self.remove_temporary_selectors()?;
        let selected = self.selection()?;
        if selected.is_some() {
            self.repair_public_links()?;
        }
        let active = selected.as_ref().map(|selection| selection.active.as_str());
        let known_good = selected
            .as_ref()
            .map(|selection| selection.known_good.as_str());
        let selected_id = selected.as_ref().map(|selection| selection.id.as_str());

        for entry in read_directory(&self.root.join(SELECTIONS_DIR), "selection store")? {
            let name = entry.file_name();
            if !valid_random_name(&name, "selection.") {
                return Err(format!(
                    "invalid selection directory {}",
                    entry.path().display()
                ));
            }
            let target = format!("{SELECTIONS_DIR}/{}", name.to_string_lossy());
            if selected_id != Some(target.as_str()) {
                remove_selection_residue_directory(&entry.path())?;
            }
        }
        for entry in read_directory(&self.root.join(PAIRS_DIR), "pair store")? {
            let name = entry.file_name();
            if !valid_random_name(&name, "pair.") {
                return Err(format!("invalid pair directory {}", entry.path().display()));
            }
            let target = format!("{PAIRS_DIR}/{}", name.to_string_lossy());
            if active != Some(target.as_str()) && known_good != Some(target.as_str()) {
                remove_pair_residue_directory(&entry.path())?;
            }
        }
        Ok(())
    }

    pub(crate) fn remove_temporary_selectors(&self) -> Result<(), String> {
        for entry in read_directory(&self.root, "pair store")? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(nonce) = name.strip_prefix(".active.") else {
                continue;
            };
            if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!(
                    "invalid temporary selector {}",
                    entry.path().display()
                ));
            }
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| format!("inspect temporary selector: {error}"))?;
            if !metadata.file_type().is_symlink() {
                return Err(format!(
                    "temporary selector {} is not a symlink",
                    entry.path().display()
                ));
            }
            let target = std::fs::read_link(entry.path())
                .map_err(|error| format!("read temporary selector: {error}"))?;
            let target = target
                .to_str()
                .ok_or_else(|| "temporary selector target is not UTF-8".to_string())?;
            validate_selection_target(target)?;
            std::fs::remove_file(entry.path())
                .map_err(|error| format!("remove temporary selector: {error}"))?;
        }
        for binary in ["cyclopsd", "cyclops"] {
            let prefix = format!(".{binary}.");
            for entry in read_directory(&self.prefix, "install prefix")? {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let Some(nonce) = name.strip_prefix(&prefix) else {
                    continue;
                };
                if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(format!(
                        "invalid temporary public selector {}",
                        entry.path().display()
                    ));
                }
                let metadata = std::fs::symlink_metadata(entry.path())
                    .map_err(|error| format!("inspect temporary public selector: {error}"))?;
                let expected = PathBuf::from(PAIR_ROOT).join(ACTIVE_SELECTOR).join(binary);
                if !metadata.file_type().is_symlink()
                    || std::fs::read_link(entry.path())
                        .map_err(|error| format!("read temporary public selector: {error}"))?
                        != expected
                {
                    return Err(format!(
                        "temporary public selector {} is not managed",
                        entry.path().display()
                    ));
                }
                std::fs::remove_file(entry.path())
                    .map_err(|error| format!("remove temporary public selector: {error}"))?;
            }
        }
        sync_directory(&self.root)?;
        sync_directory(&self.prefix)
    }

    pub(crate) fn require_root(&self) -> Result<(), String> {
        require_owner_directory(&self.root)?;
        let root = std::fs::symlink_metadata(&self.root)
            .map_err(|error| format!("recheck pair store: {error}"))?;
        if root.dev() != self.root_device || root.ino() != self.root_inode {
            return Err("pair store directory changed during this operation".to_string());
        }
        let marker = self.root.join(PAIR_OWNER);
        require_owner_regular_file(&marker, 0o600)?;
        let marker = std::fs::symlink_metadata(&marker)
            .map_err(|error| format!("recheck pair owner marker: {error}"))?;
        if marker.dev() != self.owner_device || marker.ino() != self.owner_inode {
            return Err("pair store owner marker changed during this operation".to_string());
        }
        let lease_path = self.root.join(PAIR_LEASE);
        require_owner_regular_file(&lease_path, 0o600)?;
        let lease = std::fs::symlink_metadata(&lease_path)
            .map_err(|error| format!("recheck pair update lease: {error}"))?;
        if lease.dev() != self.lease_device || lease.ino() != self.lease_inode {
            return Err("pair store lease changed during this operation".to_string());
        }
        Ok(())
    }

    pub(crate) fn stage(&self, source: &Path) -> Result<String, String> {
        self.require_root()?;
        let pair_id = format!("pair.{}", random_hex()?);
        let destination = self.root.join(PAIRS_DIR).join(&pair_id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&destination)
            .map_err(|error| format!("create staged pair: {error}"))?;
        crossed_update_boundary(UpdateBoundary::PairDirectoryCreated);
        let staged = (|| {
            for name in ["cyclops", "cyclopsd"] {
                copy_executable(&source.join(name), &destination.join(name))?;
                crossed_update_boundary(if name == "cyclops" {
                    UpdateBoundary::ClientCopied
                } else {
                    UpdateBoundary::DaemonCopied
                });
            }
            prove_pair(&destination)?;
            sync_directory(&destination)?;
            sync_directory(&self.root.join(PAIRS_DIR))?;
            crossed_update_boundary(UpdateBoundary::PairPublished);
            Ok(())
        })();
        match staged {
            Ok(()) => Ok(format!("{PAIRS_DIR}/{pair_id}")),
            Err(error) => match remove_pair_residue_directory(&destination) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; staged pair cleanup refused: {cleanup}")),
            },
        }
    }

    /// Copy a direct legacy pair only to keep its current bytes executable
    /// during selector migration. It is never retained as known-good.
    pub(crate) fn stage_legacy(&self, source: &Path) -> Result<String, String> {
        self.require_root()?;
        let pair_id = format!("pair.{}", random_hex()?);
        let destination = self.root.join(PAIRS_DIR).join(&pair_id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&destination)
            .map_err(|error| format!("create legacy migration pair: {error}"))?;
        crossed_update_boundary(UpdateBoundary::PairDirectoryCreated);
        let staged = (|| {
            for name in ["cyclops", "cyclopsd"] {
                copy_executable(&source.join(name), &destination.join(name))?;
                crossed_update_boundary(if name == "cyclops" {
                    UpdateBoundary::ClientCopied
                } else {
                    UpdateBoundary::DaemonCopied
                });
            }
            sync_directory(&destination)?;
            sync_directory(&self.root.join(PAIRS_DIR))?;
            crossed_update_boundary(UpdateBoundary::PairPublished);
            Ok(())
        })();
        match staged {
            Ok(()) => Ok(format!("{PAIRS_DIR}/{pair_id}")),
            Err(error) => match remove_pair_residue_directory(&destination) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; legacy pair cleanup refused: {cleanup}")),
            },
        }
    }

    pub(crate) fn pair_path(&self, target: &str) -> Result<PathBuf, String> {
        self.require_root()?;
        self.require_pair(target)?;
        Ok(self.root.join(target))
    }

    pub(crate) fn active_binary(&self, name: &str) -> Result<PathBuf, String> {
        self.require_root()?;
        let active = self
            .selection()?
            .ok_or_else(|| "no active Cyclops pair is recorded".to_string())?;
        if active.legacy_active {
            require_pair_directory(&self.root.join(&active.active))?;
        } else {
            self.require_pair(&active.active)?;
        }
        Ok(self.root.join(active.active).join(name))
    }

    /// Move an existing direct installation behind the selector without ever
    /// changing the bytes either public name resolves to.
    pub(crate) fn migrate_direct_pair(&self, candidate: &str) -> Result<(), PairChangeError> {
        self.require_root().map_err(PairChangeError::unchanged)?;
        self.require_pair(candidate)
            .map_err(PairChangeError::unchanged)?;
        if self
            .selection()
            .map_err(PairChangeError::unchanged)?
            .is_some()
        {
            return self
                .repair_public_links()
                .map_err(PairChangeError::unchanged);
        }
        let cli = self.prefix.join("cyclops");
        let daemon = self.prefix.join("cyclopsd");
        let cli_meta = std::fs::symlink_metadata(&cli).ok();
        let daemon_meta = std::fs::symlink_metadata(&daemon).ok();
        if cli_meta
            .as_ref()
            .is_some_and(|m| m.file_type().is_symlink())
            || daemon_meta
                .as_ref()
                .is_some_and(|m| m.file_type().is_symlink())
        {
            let expected_cli = PathBuf::from(PAIR_ROOT)
                .join(ACTIVE_SELECTOR)
                .join("cyclops");
            let expected_daemon = PathBuf::from(PAIR_ROOT)
                .join(ACTIVE_SELECTOR)
                .join("cyclopsd");
            if std::fs::read_link(&cli).ok().as_ref() == Some(&expected_cli)
                && std::fs::read_link(&daemon).ok().as_ref() == Some(&expected_daemon)
            {
                // A complete-state removal may have removed the selector store
                // while leaving these two owned public links behind. There is
                // no executable pair to preserve, so activation below may
                // safely replace both links with the verified candidate.
                return Ok(());
            }
            return Err(PairChangeError::unchanged(
                "managed Cyclops links have no active selector",
            ));
        }
        match (cli_meta, daemon_meta) {
            (None, None) => Ok(()),
            (Some(cli_meta), Some(daemon_meta)) if cli_meta.is_file() && daemon_meta.is_file() => {
                let source = self.prefix.clone();
                let matched = prove_pair_identity(&source).is_ok();
                let old = if matched {
                    self.stage(&source).map_err(PairChangeError::unchanged)?
                } else {
                    self.stage_legacy(&source)
                        .map_err(PairChangeError::unchanged)?
                };
                let selection = if matched {
                    self.prepare_selection(&old, &old)
                        .map_err(PairChangeError::unchanged)?
                } else {
                    self.prepare_legacy_selection(&old, candidate)
                        .map_err(PairChangeError::unchanged)?
                };
                self.select(&selection)
                    .map_err(|error| PairChangeError::after_selector_publication(None, error))?;
                // The daemon path moves first but still resolves to the same
                // copied bytes as the direct CLI. The second move completes
                // the stable indirection without a mixed pair window.
                self.replace_public_link("cyclopsd").map_err(|error| {
                    PairChangeError::after_visible_selector(None, selection.clone(), error)
                })?;
                self.replace_public_link("cyclops").map_err(|error| {
                    PairChangeError::after_visible_selector(None, selection.clone(), error)
                })?;
                Ok(())
            }
            _ => Err(PairChangeError::unchanged(
                "the install prefix contains only one Cyclops binary or an unsupported file type",
            )),
        }
    }

    pub(crate) fn activate(
        &self,
        candidate: &str,
        replay: ReplayAttestation,
    ) -> Result<Option<Selection>, PairChangeError> {
        self.require_root().map_err(PairChangeError::unchanged)?;
        let candidate_proof = self
            .pair_proof(candidate)
            .map_err(PairChangeError::unchanged)?;
        verify_replay_attestation(&replay, &candidate_proof).map_err(PairChangeError::unchanged)?;
        let previous = self.selection().map_err(PairChangeError::unchanged)?;
        if previous.as_ref().is_some_and(|value| {
            value.active == candidate && value.active_replay == Some(replay.clone())
        }) {
            self.require_public_links()
                .map_err(PairChangeError::unchanged)?;
            return Ok(previous);
        }
        let (known_good, known_good_replay) = previous
            .as_ref()
            .map(|value| {
                if value.legacy_active {
                    (
                        value.known_good.clone(),
                        if value.known_good == candidate {
                            Some(replay.clone())
                        } else {
                            value.known_good_replay.clone()
                        },
                    )
                } else {
                    (value.active.clone(), value.active_replay.clone())
                }
            })
            .unwrap_or_else(|| (candidate.to_string(), Some(replay.clone())));
        let selection = self
            .prepare_selection_with_replays(candidate, &known_good, Some(replay), known_good_replay)
            .map_err(PairChangeError::unchanged)?;
        self.select(&selection).map_err(|error| {
            PairChangeError::after_selector_publication(previous.clone(), error)
        })?;
        if previous.is_none() {
            self.replace_public_link("cyclopsd").map_err(|error| {
                PairChangeError::after_visible_selector(previous.clone(), selection.clone(), error)
            })?;
            self.replace_public_link("cyclops").map_err(|error| {
                PairChangeError::after_visible_selector(previous.clone(), selection.clone(), error)
            })?;
        } else {
            self.require_public_links().map_err(|error| {
                PairChangeError::after_visible_selector(previous.clone(), selection.clone(), error)
            })?;
        }
        Ok(previous)
    }

    pub(crate) fn rollback(
        &self,
        restored_replay: ReplayAttestation,
    ) -> Result<(Selection, Selection), PairChangeError> {
        let current = self
            .rollback_selection()
            .map_err(PairChangeError::unchanged)?;
        let known_good_proof = current
            .known_good_proof
            .as_ref()
            .ok_or_else(|| "the known-good pair has no recorded identity".to_string())
            .map_err(PairChangeError::unchanged)?;
        verify_replay_attestation(&restored_replay, known_good_proof)
            .map_err(PairChangeError::unchanged)?;
        let restored = self
            .prepare_selection_with_replays(
                &current.known_good,
                &current.active,
                Some(restored_replay),
                current.active_replay.clone(),
            )
            .map_err(PairChangeError::unchanged)?;
        self.select(&restored).map_err(|error| {
            PairChangeError::after_selector_publication(Some(current.clone()), error)
        })?;
        Ok((current, restored))
    }

    /// Validate the selected rollback relationship without changing it.
    pub(crate) fn rollback_selection(&self) -> Result<Selection, String> {
        self.require_root()?;
        let current = self
            .selection()?
            .ok_or_else(|| "no active Cyclops pair is recorded".to_string())?;
        if current.legacy_active {
            return Err(
                "legacy direct-pair migration is incomplete; rerun the interrupted update"
                    .to_string(),
            );
        }
        self.require_pair(&current.active)?;
        self.require_pair(&current.known_good)?;
        if current.active == current.known_good {
            return Err("the active and known-good selectors name the same pair".to_string());
        }
        Ok(current)
    }

    pub(crate) fn restore_selection(
        &self,
        selection: &Selection,
    ) -> Result<(), SelectorPublicationError> {
        self.require_root()
            .map_err(SelectorPublicationError::before)?;
        self.require_selection(selection)
            .map_err(SelectorPublicationError::before)?;
        self.select(selection)
    }

    pub(crate) fn discard(&self, pair: &str) -> Result<(), String> {
        self.require_root()?;
        self.require_pair(pair)?;
        if self
            .selection()?
            .is_some_and(|selection| selection.active == pair || selection.known_good == pair)
        {
            return Err("refusing to remove a selected Cyclops pair".to_string());
        }
        let directory = self.root.join(pair);
        remove_pair_directory(&directory)?;
        sync_directory(&self.root.join(PAIRS_DIR))
    }

    /// Remove only a fully validated managed pair store.
    pub(crate) fn remove_managed(self) -> Result<(), String> {
        self.require_root()?;
        self.validate_managed_schema()?;

        let active = self.root.join(ACTIVE_SELECTOR);
        match std::fs::symlink_metadata(&active) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                std::fs::remove_file(&active)
                    .map_err(|error| format!("remove active pair selector: {error}"))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err("active pair selector is not a symlink".to_string()),
            Err(error) => return Err(format!("inspect active pair selector: {error}")),
        }

        for entry in read_directory(&self.root.join(SELECTIONS_DIR), "selection store")? {
            remove_selection_directory(&entry.path())?;
        }
        for entry in read_directory(&self.root.join(PAIRS_DIR), "pair store")? {
            remove_pair_residue_directory(&entry.path())?;
        }
        std::fs::remove_dir(self.root.join(SELECTIONS_DIR))
            .map_err(|error| format!("remove selection store: {error}"))?;
        std::fs::remove_dir(self.root.join(PAIRS_DIR))
            .map_err(|error| format!("remove pair store: {error}"))?;

        self.require_root()?;
        let owner = self.root.join(PAIR_OWNER);
        let lease = self.root.join(PAIR_LEASE);
        std::fs::remove_file(&owner)
            .map_err(|error| format!("remove pair owner marker: {error}"))?;
        std::fs::remove_file(&lease)
            .map_err(|error| format!("remove pair update lease: {error}"))?;
        let prefix = self.prefix.clone();
        let root = self.root.clone();
        let PairStore { _lease, .. } = self;
        drop(_lease);
        std::fs::remove_dir(&root)
            .map_err(|error| format!("remove managed pair store: {error}"))?;
        sync_directory(&prefix)
    }

    pub(crate) fn validate_managed_schema(&self) -> Result<(), String> {
        self.require_root()?;
        let allowed = [
            PAIR_OWNER,
            PAIR_LEASE,
            PAIRS_DIR,
            SELECTIONS_DIR,
            ACTIVE_SELECTOR,
        ];
        for entry in read_directory(&self.root, "pair store")? {
            let name = entry.file_name();
            if !allowed
                .iter()
                .any(|allowed| name == std::ffi::OsStr::new(allowed))
            {
                return Err(format!(
                    "pair store contains unmanaged entry {}",
                    entry.path().display()
                ));
            }
        }
        let selection = self.selection()?;
        for entry in read_directory(&self.root.join(PAIRS_DIR), "pair store")? {
            let name = entry.file_name();
            if !valid_random_name(&name, "pair.") {
                return Err(format!("invalid pair directory {}", entry.path().display()));
            }
            let target = format!("{PAIRS_DIR}/{}", name.to_string_lossy());
            if selection.as_ref().is_some_and(|selection| {
                selection.active == target || selection.known_good == target
            }) {
                require_pair_directory(&entry.path())?;
            } else {
                validate_pair_residue_directory(&entry.path())?;
            }
        }
        for entry in read_directory(&self.root.join(SELECTIONS_DIR), "selection store")? {
            let name = entry.file_name();
            if !valid_random_name(&name, "selection.") {
                return Err(format!(
                    "invalid selection directory {}",
                    entry.path().display()
                ));
            }
            let target = format!("{SELECTIONS_DIR}/{}", name.to_string_lossy());
            let selection = self.read_selection(&target)?;
            self.require_selection(&selection)?;
            require_exact_entries(&entry.path(), &["cyclops", "cyclopsd", PAIR_DESCRIPTOR])?;
        }
        Ok(())
    }

    pub(crate) fn prune(&self) -> Result<(), String> {
        self.require_root()?;
        let selection = self.selection()?;
        let active = selection.as_ref().map(|value| value.active.as_str());
        let known = selection.as_ref().map(|value| value.known_good.as_str());
        let selected_id = selection.as_ref().map(|value| value.id.as_str());
        let selections = self.root.join(SELECTIONS_DIR);
        let stale_selections = std::fs::read_dir(&selections)
            .map_err(|error| format!("read selection store: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read selection entry: {error}"))?;
        let mut removable_selections = Vec::new();
        for entry in stale_selections {
            let target = format!("{SELECTIONS_DIR}/{}", entry.file_name().to_string_lossy());
            if selected_id == Some(target.as_str()) {
                continue;
            }
            validate_selection_directory(&entry.path())?;
            removable_selections.push(entry.path());
        }
        let pairs = self.root.join(PAIRS_DIR);
        let stale_pairs = std::fs::read_dir(&pairs)
            .map_err(|error| format!("read pair store: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read pair entry: {error}"))?;
        let mut removable_pairs = Vec::new();
        for entry in stale_pairs {
            if !valid_random_name(&entry.file_name(), "pair.") {
                return Err(format!("invalid pair directory {}", entry.path().display()));
            }
            let target = format!("{PAIRS_DIR}/{}", entry.file_name().to_string_lossy());
            if active == Some(target.as_str()) || known == Some(target.as_str()) {
                continue;
            }
            validate_pair_residue_directory(&entry.path())?;
            removable_pairs.push(entry.path());
        }
        for selection in removable_selections {
            remove_selection_directory(&selection)?;
        }
        for pair in removable_pairs {
            remove_pair_residue_directory(&pair)?;
        }
        Ok(())
    }

    pub(crate) fn selection(&self) -> Result<Option<Selection>, String> {
        let selection = self.selection_descriptor()?;
        if let Some(selection) = selection.as_ref() {
            self.require_selection(selection)?;
        }
        Ok(selection)
    }

    /// Read selected paths and recorded identities without executing a binary.
    pub(crate) fn selection_descriptor(&self) -> Result<Option<Selection>, String> {
        self.require_root()?;
        let path = self.root.join(ACTIVE_SELECTOR);
        let target = match std::fs::read_link(&path) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("read selector {}: {error}", path.display())),
        };
        let target = target
            .to_str()
            .ok_or_else(|| format!("selector {} is not UTF-8", path.display()))?;
        validate_selection_target(target)?;
        let selection = self.read_selection(target)?;
        self.require_selection_layout(&selection)?;
        Ok(Some(selection))
    }

    pub(crate) fn select(&self, selection: &Selection) -> Result<(), SelectorPublicationError> {
        self.require_root()
            .map_err(SelectorPublicationError::before)?;
        self.require_selection(selection)
            .map_err(SelectorPublicationError::before)?;
        let temporary = self.root.join(format!(
            ".{ACTIVE_SELECTOR}.{}",
            random_hex().map_err(SelectorPublicationError::before)?
        ));
        std::os::unix::fs::symlink(&selection.id, &temporary).map_err(|error| {
            SelectorPublicationError::before(format!(
                "create selector {}: {error}",
                temporary.display()
            ))
        })?;
        crossed_update_boundary(UpdateBoundary::SelectorTemporaryCreated);
        std::fs::rename(&temporary, self.root.join(ACTIVE_SELECTOR)).map_err(|error| {
            SelectorPublicationError::before(format!("activate pair selector: {error}"))
        })?;
        crossed_update_boundary(UpdateBoundary::SelectorCommitted);
        sync_selector_directory(&self.root)
            .map_err(|error| SelectorPublicationError::visible(selection.clone(), error))?;
        crossed_update_boundary(UpdateBoundary::SelectorPublished);
        Ok(())
    }

    pub(crate) fn prepare_selection(
        &self,
        active: &str,
        known_good: &str,
    ) -> Result<Selection, String> {
        self.prepare_selection_with_trust(active, known_good, false, None, None)
    }

    pub(crate) fn prepare_selection_with_replays(
        &self,
        active: &str,
        known_good: &str,
        active_replay: Option<ReplayAttestation>,
        known_good_replay: Option<ReplayAttestation>,
    ) -> Result<Selection, String> {
        self.prepare_selection_with_trust(
            active,
            known_good,
            false,
            active_replay,
            known_good_replay,
        )
    }

    pub(crate) fn prepare_legacy_selection(
        &self,
        active: &str,
        known_good: &str,
    ) -> Result<Selection, String> {
        self.prepare_selection_with_trust(active, known_good, true, None, None)
    }

    pub(crate) fn prepare_selection_with_trust(
        &self,
        active: &str,
        known_good: &str,
        legacy_active: bool,
        active_replay: Option<ReplayAttestation>,
        known_good_replay: Option<ReplayAttestation>,
    ) -> Result<Selection, String> {
        self.require_root()?;
        let active_proof = if legacy_active {
            validate_pair_target(active)?;
            require_pair_directory(&self.root.join(active))?;
            None
        } else {
            Some(self.pair_proof(active)?)
        };
        let known_good_proof = Some(self.pair_proof(known_good)?);
        if let Some(attestation) = active_replay.as_ref() {
            let proof = active_proof
                .as_ref()
                .ok_or_else(|| "a legacy active pair cannot carry replay evidence".to_string())?;
            verify_replay_attestation(attestation, proof)?;
        }
        if let Some(attestation) = known_good_replay.as_ref() {
            verify_replay_attestation(
                attestation,
                known_good_proof
                    .as_ref()
                    .expect("known-good proof is always recorded"),
            )?;
        }
        let id = format!("{SELECTIONS_DIR}/selection.{}", random_hex()?);
        let directory = self.root.join(&id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&directory)
            .map_err(|error| format!("create selection record: {error}"))?;
        crossed_update_boundary(UpdateBoundary::SelectionDirectoryCreated);
        let selection = Selection {
            id,
            active: active.to_string(),
            known_good: known_good.to_string(),
            legacy_active,
            active_proof,
            known_good_proof,
            active_replay,
            known_good_replay,
        };
        for name in ["cyclops", "cyclopsd"] {
            let target = PathBuf::from("../..").join(active).join(name);
            std::os::unix::fs::symlink(target, directory.join(name))
                .map_err(|error| format!("write selection binary {name}: {error}"))?;
            crossed_update_boundary(if name == "cyclops" {
                UpdateBoundary::ClientSelectionLinked
            } else {
                UpdateBoundary::DaemonSelectionLinked
            });
        }
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": 3,
            "active": active,
            "known_good": known_good,
            "legacy_active": legacy_active,
            "active_proof": selection.active_proof.as_ref(),
            "known_good_proof": selection.known_good_proof.as_ref(),
            "active_replay": selection.active_replay.as_ref(),
            "known_good_replay": selection.known_good_replay.as_ref(),
        }))
        .map_err(|error| format!("encode pair descriptor: {error}"))?;
        write_new(&directory.join(PAIR_DESCRIPTOR), &body, 0o600)?;
        crossed_update_boundary(UpdateBoundary::SelectionDescriptorWritten);
        sync_directory(&directory)?;
        sync_directory(&self.root.join(SELECTIONS_DIR))?;
        crossed_update_boundary(UpdateBoundary::SelectionPublished);
        self.require_selection(&selection)?;
        Ok(selection)
    }

    pub(crate) fn read_selection(&self, id: &str) -> Result<Selection, String> {
        self.require_root()?;
        validate_selection_target(id)?;
        let directory = self.root.join(id);
        require_owner_directory(&directory)?;
        let descriptor = directory.join(PAIR_DESCRIPTOR);
        require_owner_regular_file(&descriptor, 0o600)?;
        let body = std::fs::read(&descriptor)
            .map_err(|error| format!("read selected pair descriptor: {error}"))?;
        decode_selection(id, &body)
    }

    pub(crate) fn require_selection(&self, selection: &Selection) -> Result<(), String> {
        self.require_selection_layout(selection)?;
        if selection.legacy_active && selection.active_replay.is_some() {
            return Err("a legacy active pair cannot carry replay evidence".to_string());
        }
        if !selection.legacy_active {
            let proof = self.pair_proof(&selection.active)?;
            if selection
                .active_proof
                .as_ref()
                .is_some_and(|recorded| recorded != &proof)
            {
                return Err("active pair identity does not match its selection record".to_string());
            }
            if let Some(attestation) = selection.active_replay.as_ref() {
                verify_replay_attestation(attestation, &proof)?;
            }
        }
        let proof = self.pair_proof(&selection.known_good)?;
        if selection
            .known_good_proof
            .as_ref()
            .is_some_and(|recorded| recorded != &proof)
        {
            return Err("known-good pair identity does not match its selection record".to_string());
        }
        if let Some(attestation) = selection.known_good_replay.as_ref() {
            verify_replay_attestation(attestation, &proof)?;
        }
        Ok(())
    }

    pub(crate) fn require_selection_layout(&self, selection: &Selection) -> Result<(), String> {
        self.require_root()?;
        validate_selection_target(&selection.id)?;
        validate_pair_target(&selection.active)?;
        validate_pair_target(&selection.known_good)?;
        let directory = self.root.join(&selection.id);
        require_owner_directory(&directory)?;
        require_pair_directory(&self.root.join(&selection.active))?;
        require_pair_directory(&self.root.join(&selection.known_good))?;
        for name in ["cyclops", "cyclopsd"] {
            let expected = PathBuf::from("../..").join(&selection.active).join(name);
            let actual = std::fs::read_link(directory.join(name))
                .map_err(|error| format!("read selected {name}: {error}"))?;
            if actual != expected {
                return Err(format!("selected {name} does not name the active pair"));
            }
        }
        Ok(())
    }

    pub(crate) fn require_pair(&self, target: &str) -> Result<(), String> {
        self.pair_proof(target).map(|_| ())
    }

    pub(crate) fn pair_proof(&self, target: &str) -> Result<PairProof, String> {
        self.require_root()?;
        validate_pair_target(target)?;
        let directory = self.root.join(target);
        require_pair_directory(&directory)?;
        prove_pair(&directory)
    }

    pub(crate) fn replace_public_link(&self, name: &str) -> Result<(), String> {
        self.require_root()?;
        let temporary = self.prefix.join(format!(".{name}.{}", random_hex()?));
        let target = PathBuf::from(PAIR_ROOT).join(ACTIVE_SELECTOR).join(name);
        std::os::unix::fs::symlink(&target, &temporary)
            .map_err(|error| format!("create public {name} selector: {error}"))?;
        crossed_update_boundary(if name == "cyclopsd" {
            UpdateBoundary::PublicDaemonTemporaryCreated
        } else {
            UpdateBoundary::PublicClientTemporaryCreated
        });
        std::fs::rename(&temporary, self.prefix.join(name))
            .map_err(|error| format!("publish {name}: {error}"))?;
        crossed_update_boundary(if name == "cyclopsd" {
            UpdateBoundary::PublicDaemonCommitted
        } else {
            UpdateBoundary::PublicClientCommitted
        });
        sync_directory(&self.prefix)?;
        crossed_update_boundary(if name == "cyclopsd" {
            UpdateBoundary::PublicDaemonPublished
        } else {
            UpdateBoundary::PublicClientPublished
        });
        Ok(())
    }

    pub(crate) fn require_public_links(&self) -> Result<(), String> {
        self.require_root()?;
        for name in ["cyclops", "cyclopsd"] {
            let expected = PathBuf::from(PAIR_ROOT).join(ACTIVE_SELECTOR).join(name);
            let actual = std::fs::read_link(self.prefix.join(name))
                .map_err(|error| format!("read public {name} selector: {error}"))?;
            if actual != expected {
                return Err(format!(
                    "public {name} selector points outside the pair store"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn repair_public_links(&self) -> Result<(), String> {
        self.require_root()?;
        let selection = self
            .selection()?
            .ok_or_else(|| "managed Cyclops links have no active selector".to_string())?;
        for name in ["cyclopsd", "cyclops"] {
            let public = self.prefix.join(name);
            let expected = PathBuf::from(PAIR_ROOT).join(ACTIVE_SELECTOR).join(name);
            match std::fs::symlink_metadata(&public) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let actual = std::fs::read_link(&public)
                        .map_err(|error| format!("read public {name}: {error}"))?;
                    if actual != expected {
                        return Err(format!(
                            "public {name} selector points outside the pair store"
                        ));
                    }
                }
                Ok(metadata) if metadata.is_file() => {
                    let selected = self.root.join(&selection.active).join(name);
                    if !regular_files_equal(&public, &selected)? {
                        return Err(format!(
                            "direct {name} does not match the selected pair; refusing migration"
                        ));
                    }
                    self.replace_public_link(name)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.replace_public_link(name)?;
                }
                Ok(_) => return Err(format!("public {name} has an unsupported file type")),
                Err(error) => return Err(format!("inspect public {name}: {error}")),
            }
        }
        self.require_public_links()
    }
}

pub(crate) fn decode_selection(id: &str, body: &[u8]) -> Result<Selection, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("decode selected pair descriptor: {error}"))?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "selected pair descriptor has no schema".to_string())?;
    if !matches!(schema, 1..=3) {
        return Err("selected pair descriptor has an unsupported schema".to_string());
    }
    let active = value
        .get("active")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "selected pair descriptor has no active pair".to_string())?;
    let known_good = value
        .get("known_good")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "selected pair descriptor has no known-good pair".to_string())?;
    let legacy_active = value
        .get("legacy_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let active_proof = value
        .get("active_proof")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<PairProof>(value.clone()))
        .transpose()
        .map_err(|error| format!("decode active pair proof: {error}"))?;
    let known_good_proof = value
        .get("known_good_proof")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<PairProof>(value.clone()))
        .transpose()
        .map_err(|error| format!("decode known-good pair proof: {error}"))?;
    let active_replay = value
        .get("active_replay")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<ReplayAttestation>(value.clone()))
        .transpose()
        .map_err(|error| format!("decode active replay attestation: {error}"))?;
    let known_good_replay = value
        .get("known_good_replay")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<ReplayAttestation>(value.clone()))
        .transpose()
        .map_err(|error| format!("decode known-good replay attestation: {error}"))?;
    if schema >= 2 && (known_good_proof.is_none() || (!legacy_active && active_proof.is_none())) {
        return Err("selected pair descriptor is missing a recorded build identity".to_string());
    }
    Ok(Selection {
        id: id.to_string(),
        active: active.to_string(),
        known_good: known_good.to_string(),
        legacy_active,
        active_proof,
        known_good_proof,
        active_replay,
        known_good_replay,
    })
}

pub(crate) fn validate_pair_target(target: &str) -> Result<(), String> {
    let path = Path::new(target);
    let mut components = path.components();
    let first = components.next();
    let second = components.next();
    if first
        != Some(std::path::Component::Normal(std::ffi::OsStr::new(
            PAIRS_DIR,
        )))
        || !matches!(second, Some(std::path::Component::Normal(name)) if valid_random_name(name, "pair."))
        || components.next().is_some()
    {
        return Err(format!("invalid pair selector target {target:?}"));
    }
    Ok(())
}

pub(crate) fn validate_selection_target(target: &str) -> Result<(), String> {
    let path = Path::new(target);
    let mut components = path.components();
    let first = components.next();
    let second = components.next();
    if first
        != Some(std::path::Component::Normal(std::ffi::OsStr::new(
            SELECTIONS_DIR,
        )))
        || !matches!(second, Some(std::path::Component::Normal(name)) if valid_random_name(name, "selection."))
        || components.next().is_some()
    {
        return Err(format!("invalid pair selection target {target:?}"));
    }
    Ok(())
}

pub(crate) fn valid_random_name(name: &std::ffi::OsStr, prefix: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(nonce) = name.strip_prefix(prefix) else {
        return false;
    };
    nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn require_owner_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(format!("{} is not an owner-only directory", path.display()));
    }
    Ok(())
}

pub(crate) fn require_install_prefix(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect install prefix {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(format!(
            "install prefix {} is not an owner-controlled directory",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn require_unlinked_regular_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(format!(
            "{} is linked or not a regular file",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn require_owner_regular_file(path: &Path, mode: u32) -> Result<(), String> {
    require_unlinked_regular_file(path)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.permissions().mode() & 0o777 != mode {
        return Err(format!("{} does not have mode {mode:o}", path.display()));
    }
    Ok(())
}

pub(crate) fn require_executable(path: &Path) -> Result<(), String> {
    require_unlinked_regular_file(path)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.permissions().mode() & 0o100 == 0 {
        return Err(format!("{} is not executable by its owner", path.display()));
    }
    Ok(())
}

pub(crate) fn regular_files_equal(left: &Path, right: &Path) -> Result<bool, String> {
    require_unlinked_regular_file(left)?;
    require_unlinked_regular_file(right)?;
    let left_before = std::fs::symlink_metadata(left)
        .map_err(|error| format!("inspect {}: {error}", left.display()))?;
    let right_before = std::fs::symlink_metadata(right)
        .map_err(|error| format!("inspect {}: {error}", right.display()))?;
    let mut left = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(left)
        .map_err(|error| format!("open {}: {error}", left.display()))?;
    let mut right = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(right)
        .map_err(|error| format!("open {}: {error}", right.display()))?;
    if left.metadata().map_err(|error| error.to_string())?.len()
        != right.metadata().map_err(|error| error.to_string())?.len()
    {
        return Ok(false);
    }
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left
            .read(&mut left_buffer)
            .map_err(|error| format!("read direct Cyclops binary: {error}"))?;
        let right_read = right
            .read(&mut right_buffer)
            .map_err(|error| format!("read selected Cyclops binary: {error}"))?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            let left_after = left
                .metadata()
                .map_err(|error| format!("recheck direct Cyclops binary: {error}"))?;
            let right_after = right
                .metadata()
                .map_err(|error| format!("recheck selected Cyclops binary: {error}"))?;
            return Ok(metadata_unchanged(&left_before, &left_after)
                && metadata_unchanged(&right_before, &right_after));
        }
    }
}

pub(crate) fn metadata_unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && after.nlink() == 1
}

pub(crate) fn read_directory(path: &Path, kind: &str) -> Result<Vec<std::fs::DirEntry>, String> {
    std::fs::read_dir(path)
        .map_err(|error| format!("read {kind}: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {kind} entry: {error}"))
}

pub(crate) fn require_exact_entries(directory: &Path, allowed: &[&str]) -> Result<(), String> {
    for entry in read_directory(directory, "managed directory")? {
        let name = entry.file_name();
        if !allowed
            .iter()
            .any(|allowed| name == std::ffi::OsStr::new(allowed))
        {
            return Err(format!("unmanaged entry {}", entry.path().display()));
        }
    }
    Ok(())
}

pub(crate) fn require_pair_directory(directory: &Path) -> Result<(), String> {
    require_owner_directory(directory)?;
    require_exact_entries(directory, &["cyclops", "cyclopsd"])?;
    for name in ["cyclops", "cyclopsd"] {
        require_executable(&directory.join(name))?;
    }
    Ok(())
}

/// Validate an unselected staging residue without requiring both binaries.
pub(crate) fn validate_pair_residue_directory(directory: &Path) -> Result<(), String> {
    require_owner_directory(directory)?;
    for entry in read_directory(directory, "staged pair residue")? {
        let name = entry.file_name();
        if name != std::ffi::OsStr::new("cyclops") && name != std::ffi::OsStr::new("cyclopsd") {
            return Err(format!(
                "staged pair residue contains unmanaged entry {}",
                entry.path().display()
            ));
        }
        require_executable(&entry.path())?;
    }
    Ok(())
}

pub(crate) fn remove_pair_residue_directory(directory: &Path) -> Result<(), String> {
    validate_pair_residue_directory(directory)?;
    let parent = directory
        .parent()
        .ok_or_else(|| "staged pair residue has no parent directory".to_string())?;
    let entries = read_directory(directory, "staged pair residue")?;
    for entry in entries {
        require_executable(&entry.path())?;
        std::fs::remove_file(entry.path())
            .map_err(|error| format!("remove staged pair residue: {error}"))?;
    }
    std::fs::remove_dir(directory)
        .map_err(|error| format!("remove staged pair residue directory: {error}"))?;
    sync_directory(parent)
}

pub(crate) fn remove_pair_directory(directory: &Path) -> Result<(), String> {
    require_pair_directory(directory)?;
    remove_pair_residue_directory(directory)
}

pub(crate) fn validate_selection_directory(directory: &Path) -> Result<(), String> {
    require_owner_directory(directory)?;
    require_exact_entries(directory, &["cyclops", "cyclopsd", PAIR_DESCRIPTOR])?;
    for name in ["cyclops", "cyclopsd"] {
        let path = directory.join(name);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect stale selection {}: {error}", path.display()))?;
        if !metadata.file_type().is_symlink() {
            return Err(format!(
                "stale selection {} is not a symlink",
                path.display()
            ));
        }
    }
    require_owner_regular_file(&directory.join(PAIR_DESCRIPTOR), 0o600)
}

pub(crate) fn remove_selection_directory(directory: &Path) -> Result<(), String> {
    validate_selection_directory(directory)?;
    remove_selection_residue_directory(directory)
}

pub(crate) fn remove_selection_residue_directory(directory: &Path) -> Result<(), String> {
    require_owner_directory(directory)?;
    let entries = read_directory(directory, "selection residue")?;
    for entry in &entries {
        let name = entry.file_name();
        if name == std::ffi::OsStr::new("cyclops") || name == std::ffi::OsStr::new("cyclopsd") {
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| format!("inspect selection residue: {error}"))?;
            if !metadata.file_type().is_symlink() {
                return Err(format!(
                    "selection residue {} is not a symlink",
                    entry.path().display()
                ));
            }
        } else if name == std::ffi::OsStr::new(PAIR_DESCRIPTOR) {
            require_owner_regular_file(&entry.path(), 0o600)?;
        } else {
            return Err(format!(
                "selection residue contains unmanaged entry {}",
                entry.path().display()
            ));
        }
    }
    for entry in entries {
        std::fs::remove_file(entry.path())
            .map_err(|error| format!("remove selection residue: {error}"))?;
    }
    let parent = directory
        .parent()
        .ok_or_else(|| "selection residue has no parent directory".to_string())?;
    std::fs::remove_dir(directory)
        .map_err(|error| format!("remove selection residue directory: {error}"))?;
    sync_directory(parent)
}

pub(crate) fn copy_executable(source: &Path, destination: &Path) -> Result<(), String> {
    require_executable(source)?;
    let before = std::fs::symlink_metadata(source)
        .map_err(|error| format!("inspect {}: {error}", source.display()))?;
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(|error| format!("open {}: {error}", source.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(destination)
        .map_err(|error| format!("create {}: {error}", destination.display()))?;
    let copied = std::io::copy(&mut input, &mut output)
        .map_err(|error| format!("copy {}: {error}", source.display()))?;
    output
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", destination.display()))?;
    let after = input
        .metadata()
        .map_err(|error| format!("recheck {}: {error}", source.display()))?;
    if copied != before.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || after.nlink() != 1
    {
        return Err(format!("{} changed while it was copied", source.display()));
    }
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

/// The selector is already observable after its rename. Keep the directory
/// sync separate so tests can exercise the only error path where a pair change
/// is visible but not yet confirmed durable.
pub(crate) fn sync_selector_directory(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_NEXT_SELECTOR_DIRECTORY_SYNC.with(|fail| fail.replace(false)) {
        return Err("injected selector directory sync failure".to_string());
    }
    sync_directory(path)
}

const MAX_PAIR_BINARY_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn prove_pair(directory: &Path) -> Result<PairProof, String> {
    let identity = prove_pair_identity(directory)?;
    Ok(PairProof {
        identity,
        cyclops_sha256: executable_sha256(&directory.join("cyclops"))?,
        cyclopsd_sha256: executable_sha256(&directory.join("cyclopsd"))?,
    })
}

/// Validate the recorded install proof without executing either binary.
pub(crate) fn verify_recorded_pair(directory: &Path, proof: &PairProof) -> Result<(), String> {
    require_pair_directory(directory)?;
    let cyclops = executable_sha256(&directory.join("cyclops"))?;
    let cyclopsd = executable_sha256(&directory.join("cyclopsd"))?;
    if cyclops != proof.cyclops_sha256 || cyclopsd != proof.cyclopsd_sha256 {
        return Err(format!(
            "selected pair {} changed after its install proof was recorded",
            directory.display()
        ));
    }
    identity_build(&proof.identity)?;
    Ok(())
}

pub(crate) fn verify_replay_attestation(
    attestation: &ReplayAttestation,
    proof: &PairProof,
) -> Result<(), String> {
    if attestation.schema != 1 {
        return Err("replay attestation has an unsupported schema".to_string());
    }
    if &attestation.pair != proof {
        return Err("replay attestation does not name the recorded pair".to_string());
    }
    if attestation.snapshot_sha256.len() != 64
        || !attestation
            .snapshot_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("replay attestation has an invalid snapshot identity".to_string());
    }
    if attestation.snapshot_entries > MAX_REPLAY_ENTRIES as u64
        || attestation.snapshot_bytes > MAX_REPLAY_TOTAL_BYTES
    {
        return Err("replay attestation exceeds the replay snapshot bounds".to_string());
    }
    Ok(())
}

pub(crate) fn executable_sha256(path: &Path) -> Result<String, String> {
    require_executable(path)?;
    let before = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if before.len() > MAX_PAIR_BINARY_BYTES {
        return Err(format!(
            "{} exceeds the {} byte pair-binary bound",
            path.display(),
            MAX_PAIR_BINARY_BYTES
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|error| format!("recheck {}: {error}", path.display()))?;
    if !metadata_unchanged(&before, &after) {
        return Err(format!("{} changed while it was hashed", path.display()));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn prove_pair_identity(directory: &Path) -> Result<String, String> {
    let cli = binary_identity(&directory.join("cyclops"), "cyclops")?;
    let daemon = binary_identity(&directory.join("cyclopsd"), "cyclopsd")?;
    if cli != daemon {
        return Err(format!(
            "CLI identity {cli:?} does not match daemon identity {daemon:?}"
        ));
    }
    if !cli.contains(" (") || !cli.ends_with(')') {
        return Err("the pair does not report a source build identity".to_string());
    }
    Ok(cli)
}

// A concurrent fork can briefly inherit a newly staged executable's writable
// descriptor. Linux rejects execution until that child reaches exec and
// closes the descriptor, so retry only that transient kernel result.
const TEXT_BUSY_RETRY_DELAYS_MS: [u64; 3] = [100, 200, 400];

pub(crate) fn retry_text_busy<T>(
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut delays = TEXT_BUSY_RETRY_DELAYS_MS.into_iter();
    loop {
        match operation() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                let Some(delay_ms) = delays.next() else {
                    return Err(error);
                };
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            result => return result,
        }
    }
}

pub(crate) fn version_output(binary: &Path) -> std::io::Result<Output> {
    retry_text_busy(|| Command::new(binary).arg("--version").output())
}

pub(crate) fn binary_identity(binary: &Path, expected_name: &str) -> Result<String, String> {
    require_executable(binary)?;
    let output = version_output(binary)
        .map_err(|error| format!("run {} --version: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!("{} --version failed", binary.display()));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let (name, identity) = line
        .trim()
        .split_once(' ')
        .ok_or_else(|| format!("{} returned an invalid version line", binary.display()))?;
    if name != expected_name || identity.is_empty() {
        return Err(format!(
            "{} did not identify itself as {expected_name}",
            binary.display()
        ));
    }
    Ok(identity.to_string())
}

const REPLAY_DIAGNOSTIC_TAIL_BYTES: u64 = 8 * 1024;
const REPLAY_DIAGNOSTIC_MAX_CHARS: usize = 512;

/// Read one bounded tail through the state root's held descriptors.
pub(crate) fn replay_log_tail(root_path: &Path, descendant: &Path) -> Option<Vec<u8>> {
    let root = cyclops_state::StateRoot::open_existing(root_path)
        .ok()
        .flatten()?;
    let mut file = root.open_read(descendant).ok().flatten()?;
    let length = file.seek(std::io::SeekFrom::End(0)).ok()?;
    file.seek(std::io::SeekFrom::Start(
        length.saturating_sub(REPLAY_DIAGNOSTIC_TAIL_BYTES),
    ))
    .ok()?;
    let mut bytes =
        Vec::with_capacity(usize::try_from(length.min(REPLAY_DIAGNOSTIC_TAIL_BYTES)).unwrap_or(0));
    file.take(REPLAY_DIAGNOSTIC_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

/// Keep a terminal-safe, single-line diagnostic from an untrusted child log.
pub(crate) fn sanitize_replay_diagnostic(line: &str) -> Option<String> {
    let clean: String = line
        .trim()
        .chars()
        .take(REPLAY_DIAGNOSTIC_MAX_CHARS)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.to_string())
}

pub(crate) fn daemon_replay_failure_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .find_map(|raw| {
            let line = sanitize_replay_diagnostic(raw)?;
            if !line.contains("ERROR")
                && !line.contains("boot failed")
                && !line.contains("cyclopsd panic")
            {
                return None;
            }
            let detail = line
                .split_once("boot failed: ")
                .map(|(_, detail)| detail)
                .or_else(|| line.split_once("ERROR ").map(|(_, detail)| detail))
                .unwrap_or(&line);
            sanitize_replay_diagnostic(detail)
        })
}

pub(crate) fn captured_replay_failure_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .find_map(sanitize_replay_diagnostic)
}

/// Prefer the daemon's boot log. Diagnostic read failures never replace the
/// child exit status, and captured stdout or stderr remains the fallback.
pub(crate) fn candidate_replay_failure_detail(probe_home: &Path, scratch_root: &Path) -> String {
    replay_log_tail(probe_home, Path::new("cyclopsd.log"))
        .as_deref()
        .and_then(daemon_replay_failure_line)
        .or_else(|| {
            replay_log_tail(scratch_root, Path::new("candidate-replay.log"))
                .as_deref()
                .and_then(captured_replay_failure_line)
        })
        .unwrap_or_else(|| "no log output".to_string())
}

/// Boot the candidate against a private copy of current state. A ready hello
/// proves that its real daemon startup replayed every configured journal.
pub(crate) fn prove_candidate_replay(
    pair: &Path,
    source_home: &Path,
    scratch: &Scratch,
) -> Result<ReplayAttestation, String> {
    let pair_proof = prove_pair(pair)?;
    let probe_home = scratch.path().join("r");
    if source_home.exists() {
        copy_replay_state(source_home, &probe_home)?;
    } else {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&probe_home)
            .map_err(|error| format!("create replay home: {error}"))?;
    }
    isolate_probe_config(&probe_home)?;
    let snapshot = replay_snapshot_identity(&probe_home)?;
    let log_path = scratch.path().join("candidate-replay.log");
    let stdout = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|error| format!("create candidate replay log: {error}"))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| format!("clone candidate replay log: {error}"))?;
    let child = Command::new(pair.join("cyclopsd"))
        .env("CYCLOPS_HOME", &probe_home)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|error| format!("start candidate replay: {error}"))?;
    let mut child = ReplayChild::new(child);
    let socket = probe_home.join(cyclops_proto::SOCK_NAME);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let hello = loop {
        match crate::client::Client::connect_path(
            &socket,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_secs(2),
        ) {
            Ok(client) => break client.hello().clone(),
            Err(
                crate::client::ClientError::NotRunning(_)
                | crate::client::ClientError::ConnectTimeout(_),
            ) => {}
            Err(error) => {
                return Err(match error {
                    crate::client::ClientError::InvalidHello(cause) => {
                        format!("decode candidate hello: {cause}")
                    }
                    error => format!(
                        "read candidate hello: {}",
                        crate::copy::client_error(&error, None)
                    ),
                });
            }
        }
        if let Some(status) = child
            .child_mut()
            .try_wait()
            .map_err(|error| format!("wait for candidate replay: {error}"))?
        {
            let detail = candidate_replay_failure_detail(&probe_home, scratch.path());
            return Err(format!(
                "candidate journal replay exited with {status}: {detail}"
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err("candidate journal replay did not become ready within 10s".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let client_identity_text = binary_identity(&pair.join("cyclops"), "cyclops")?;
    if client_identity_text != pair_proof.identity {
        return Err(format!(
            "candidate CLI identity changed during replay: staged {:?}, running {:?}",
            pair_proof.identity, client_identity_text
        ));
    }
    let client_identity = cyclops_client::RuntimeIdentity::parse(&client_identity_text)
        .ok_or_else(|| "candidate pair has an invalid runtime identity".to_string())?;
    let daemon_identity = cyclops_client::RuntimeIdentity::from_hello(&hello);
    if client_identity != daemon_identity {
        return Err(format!(
            "candidate CLI identity {} does not match daemon greeting {}",
            client_identity.description(),
            daemon_identity.description()
        ));
    }
    let stopped = Command::new(pair.join("cyclops"))
        .args(["daemon", "stop"])
        .env("CYCLOPS_HOME", &probe_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("stop candidate replay daemon: {error}"))?;
    if !stopped.success() {
        return Err("candidate replay daemon could not stop its authenticated generation".into());
    }
    let status = child
        .wait()
        .map_err(|error| format!("join candidate replay daemon: {error}"))?;
    if !status.success() {
        return Err(format!("candidate replay daemon stopped with {status}"));
    }
    let attestation = ReplayAttestation {
        schema: 1,
        pair: pair_proof,
        snapshot_sha256: snapshot.sha256,
        snapshot_entries: snapshot.entries,
        snapshot_bytes: snapshot.bytes,
    };
    verify_replay_attestation(&attestation, &attestation.pair)?;
    Ok(attestation)
}

/// Reap a private replay daemon on every return path.
pub(crate) struct ReplayChild(Option<std::process::Child>);

impl ReplayChild {
    pub(crate) fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    pub(crate) fn child_mut(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("replay child is armed")
    }

    pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child_mut().wait()?;
        self.0.take();
        Ok(status)
    }
}

impl Drop for ReplayChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

const MAX_REPLAY_ENTRIES: usize = 50_000;
pub(crate) const MAX_REPLAY_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPLAY_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Copy only state that daemon boot reads. Logs, caches, saved workspace
/// layouts, sockets, and update artifacts cannot affect journal replay.
pub(crate) fn copy_replay_state(source: &Path, destination: &Path) -> Result<(), String> {
    require_owner_directory(source)?;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(destination)
        .map_err(|error| format!("create replay home: {error}"))?;
    let mut entries = 0;
    let mut bytes = 0;
    for name in [
        "config.toml",
        "registry.json",
        "identity",
        "ledger",
        "workspaces",
        "manifests",
        "themes",
    ] {
        let child = source.join(name);
        match std::fs::symlink_metadata(&child) {
            Ok(_) => copy_state_tree(&child, &destination.join(name), 0, &mut entries, &mut bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("inspect replay state {}: {error}", child.display())),
        }
    }
    Ok(())
}

pub(crate) struct ReplaySnapshotIdentity {
    pub(crate) sha256: String,
    pub(crate) entries: u64,
    pub(crate) bytes: u64,
}

/// Hash the exact private boot inputs without retaining names or contents.
pub(crate) fn replay_snapshot_identity(root: &Path) -> Result<ReplaySnapshotIdentity, String> {
    require_owner_directory(root)?;
    let mut hasher = Sha256::new();
    hasher.update(b"cyclops-replay-snapshot-v1\0");
    let mut entries = 0_u64;
    let mut bytes = 0_u64;
    hash_replay_tree(root, Path::new(""), &mut hasher, &mut entries, &mut bytes)?;
    Ok(ReplaySnapshotIdentity {
        sha256: format!("{:x}", hasher.finalize()),
        entries,
        bytes,
    })
}

pub(crate) fn hash_replay_tree(
    directory: &Path,
    relative: &Path,
    hasher: &mut Sha256,
    entries: &mut u64,
    bytes: &mut u64,
) -> Result<(), String> {
    require_owner_directory(directory)?;
    let mut children = read_directory(directory, "replay snapshot")?;
    children.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    for child in children {
        *entries = entries
            .checked_add(1)
            .ok_or_else(|| "replay snapshot entry count overflowed".to_string())?;
        if *entries > MAX_REPLAY_ENTRIES as u64 {
            return Err("state snapshot exceeds replay probe bounds".to_string());
        }
        let child_relative = relative.join(child.file_name());
        let path_bytes = child_relative.as_os_str().as_bytes();
        let metadata = std::fs::symlink_metadata(child.path())
            .map_err(|error| format!("inspect replay snapshot entry: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "state snapshot refuses symlink {}",
                child.path().display()
            ));
        }
        if metadata.is_dir() {
            hash_field(hasher, b"d");
            hash_field(hasher, path_bytes);
            hash_replay_tree(&child.path(), &child_relative, hasher, entries, bytes)?;
            continue;
        }
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(format!(
                "state snapshot refuses linked or special file {}",
                child.path().display()
            ));
        }
        require_unlinked_regular_file(&child.path())?;
        if metadata.len() > MAX_REPLAY_FILE_BYTES {
            return Err(format!(
                "replay file {} exceeds the {} byte bound",
                child.path().display(),
                MAX_REPLAY_FILE_BYTES
            ));
        }
        *bytes = bytes
            .checked_add(metadata.len())
            .ok_or_else(|| "replay snapshot byte count overflowed".to_string())?;
        if *bytes > MAX_REPLAY_TOTAL_BYTES {
            return Err(format!(
                "replay state exceeds the {} byte total bound",
                MAX_REPLAY_TOTAL_BYTES
            ));
        }
        hash_field(hasher, b"f");
        hash_field(hasher, path_bytes);
        hash_field(hasher, &metadata.len().to_be_bytes());
        hash_regular_file(&child.path(), hasher)?;
    }
    Ok(())
}

pub(crate) fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

pub(crate) fn hash_regular_file(path: &Path, hasher: &mut Sha256) -> Result<(), String> {
    let before = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|error| format!("recheck {}: {error}", path.display()))?;
    if !metadata_unchanged(&before, &after) {
        return Err(format!("{} changed while it was hashed", path.display()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn candidate_build(binary: &Path) -> Result<String, String> {
    let output = version_output(binary)
        .map_err(|error| format!("run {} --version: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!("{} --version failed", binary.display()));
    }
    identity_build(String::from_utf8_lossy(&output.stdout).trim())
        .map_err(|_| format!("{} did not report a source build", binary.display()))
}

pub(crate) fn candidate_identity(
    binary: &Path,
    expected_name: &str,
) -> Result<cyclops_client::RuntimeIdentity, String> {
    let identity = binary_identity(binary, expected_name)?;
    cyclops_client::RuntimeIdentity::parse(&identity).ok_or_else(|| {
        format!(
            "{} did not report a complete runtime identity",
            binary.display()
        )
    })
}

pub(crate) fn identity_build(identity: &str) -> Result<String, String> {
    let build = identity
        .strip_suffix(')')
        .and_then(|line| line.rsplit_once(" (").map(|(_, build)| build))
        .filter(|build| !build.is_empty())
        .ok_or_else(|| "recorded pair identity has no source build".to_string())?;
    Ok(build.to_string())
}

pub(crate) fn copy_state_tree(
    source: &Path,
    destination: &Path,
    depth: usize,
    entries: &mut usize,
    bytes: &mut u64,
) -> Result<(), String> {
    if depth > 16 || *entries > MAX_REPLAY_ENTRIES {
        return Err("state snapshot exceeds replay probe bounds".to_string());
    }
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("inspect {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "state snapshot refuses symlink {}",
            source.display()
        ));
    }
    if metadata.is_dir() {
        require_owner_directory(source)?;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(destination)
            .map_err(|error| format!("create {}: {error}", destination.display()))?;
        for child in std::fs::read_dir(source)
            .map_err(|error| format!("read {}: {error}", source.display()))?
        {
            let child = child.map_err(|error| format!("read state entry: {error}"))?;
            *entries += 1;
            if child.file_name() == std::ffi::OsStr::new(cyclops_proto::SOCK_NAME) {
                continue;
            }
            copy_state_tree(
                &child.path(),
                &destination.join(child.file_name()),
                depth + 1,
                entries,
                bytes,
            )?;
        }
        return Ok(());
    }
    if metadata.file_type().is_socket() {
        return Ok(());
    }
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(format!(
            "state snapshot refuses linked or special file {}",
            source.display()
        ));
    }
    require_unlinked_regular_file(source)?;
    if metadata.len() > MAX_REPLAY_FILE_BYTES {
        return Err(format!(
            "replay file {} exceeds the {} byte bound",
            source.display(),
            MAX_REPLAY_FILE_BYTES
        ));
    }
    *bytes = bytes
        .checked_add(metadata.len())
        .ok_or_else(|| "replay snapshot byte count overflowed".to_string())?;
    if *bytes > MAX_REPLAY_TOTAL_BYTES {
        return Err(format!(
            "replay state exceeds the {} byte total bound",
            MAX_REPLAY_TOTAL_BYTES
        ));
    }
    let mode = metadata.permissions().mode() & 0o700;
    let mode = if mode == 0 { 0o600 } else { mode };
    copy_regular_file(source, destination, mode)
}

pub(crate) fn copy_regular_file(
    source: &Path,
    destination: &Path,
    mode: u32,
) -> Result<(), String> {
    let before = std::fs::symlink_metadata(source)
        .map_err(|error| format!("inspect {}: {error}", source.display()))?;
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)
        .map_err(|error| format!("open {}: {error}", source.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(destination)
        .map_err(|error| format!("create {}: {error}", destination.display()))?;
    let copied = std::io::copy(&mut input, &mut output)
        .map_err(|error| format!("copy {}: {error}", source.display()))?;
    output
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", destination.display()))?;
    let after = input
        .metadata()
        .map_err(|error| format!("recheck {}: {error}", source.display()))?;
    if copied != before.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || after.nlink() != 1
    {
        return Err(format!(
            "{} changed during the replay snapshot",
            source.display()
        ));
    }
    Ok(())
}

pub(crate) fn isolate_probe_config(home: &Path) -> Result<(), String> {
    let path = home.join("config.toml");
    let mut config = match std::fs::read_to_string(&path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map_err(|error| format!("parse replay config: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(format!("read replay config: {error}")),
    };
    config.insert(
        "tmux_socket".to_string(),
        toml::Value::String(format!("cyclops-update-probe-{}", random_hex()?)),
    );
    let bytes =
        toml::to_string(&config).map_err(|error| format!("encode replay config: {error}"))?;
    let temporary = home.join(format!(".config.toml.{}", random_hex()?));
    write_new(&temporary, bytes.as_bytes(), 0o600)?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("publish replay config: {error}"))?;
    Ok(())
}
