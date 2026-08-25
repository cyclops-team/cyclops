#![cfg_attr(
    target_os = "linux",
    allow(clippy::unnecessary_cast, clippy::useless_conversion)
)]

//! Owner-only state paths anchored beneath one validated directory descriptor.
//!
//! A [`StateRoot`] is the only path-based entry point. Every later lookup is
//! relative to its open descriptor, refuses links and unexpected file types,
//! and validates ownership before changing permissions or exposing bytes.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const REPLACEMENT_TEMP_PREFIX: &str = ".cyclops-state-replace-";
const REPLACEMENT_TEMP_ATTEMPTS: usize = 128;
const BOUNDED_APPEND_LIMIT_MAX: usize = 16 * 1024 * 1024;

static REPLACEMENT_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
type RepairAfterInspect = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
type InspectAfterRead = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static REPAIR_AFTER_INSPECT: std::cell::RefCell<Option<RepairAfterInspect>> =
        const { std::cell::RefCell::new(None) };
    static INSPECT_AFTER_READ: std::cell::RefCell<Option<InspectAfterRead>> =
        const { std::cell::RefCell::new(None) };
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state io at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("unsafe state path {path}: {reason}")]
    UnsafePath { path: PathBuf, reason: &'static str },
    #[error("state replacement is visible at {path}, but directory sync failed: {source}")]
    ReplacementDurabilityUnknown {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("state creation is visible at {path}, but directory sync failed: {source}")]
    CreationDurabilityUnknown {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("state removal is visible at {path}, but directory sync failed: {source}")]
    RemovalDurabilityUnknown {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Result of publishing a file without replacing an existing entry.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CreateFileOutcome {
    Created,
    AlreadyExists,
}

/// Totals from one recursive state permission repair.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct RepairSummary {
    pub directories: usize,
    pub regular_files: usize,
    pub live_socket_preserved: bool,
}

/// Hard ceiling for one read-only directory inspection.
pub const INSPECTION_ENTRY_LIMIT_MAX: usize = 4_096;

/// Hard ceiling for names retained by one read-only directory inspection.
pub const INSPECTION_NAME_BYTES_LIMIT_MAX: usize = 256 * 1_024;

/// Hard ceiling for bytes returned by one read-only file inspection.
pub const INSPECTION_FILE_BYTES_LIMIT_MAX: usize = 1_024 * 1_024;

/// Hard ceiling for components resolved by one read-only inspection lookup.
pub const INSPECTION_PATH_COMPONENT_LIMIT_MAX: usize = 256;

/// Hard ceiling for one read-only inspection path.
pub const INSPECTION_PATH_BYTES_LIMIT_MAX: usize = 64 * 1_024;

/// Caller-selected bounds for one read-only directory inspection.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct InspectionLimits {
    pub max_entries: usize,
    pub max_name_bytes: usize,
}

impl InspectionLimits {
    /// Validate explicit limits before allocating or reading directory entries.
    pub fn new(max_entries: usize, max_name_bytes: usize) -> Result<Self, StateError> {
        if max_entries > INSPECTION_ENTRY_LIMIT_MAX {
            return Err(StateError::UnsafePath {
                path: PathBuf::from("<inspection>"),
                reason: "inspection entry limit exceeds the hard ceiling",
            });
        }
        if max_name_bytes > INSPECTION_NAME_BYTES_LIMIT_MAX {
            return Err(StateError::UnsafePath {
                path: PathBuf::from("<inspection>"),
                reason: "inspection name-byte limit exceeds the hard ceiling",
            });
        }
        Ok(Self {
            max_entries,
            max_name_bytes,
        })
    }
}

impl Default for InspectionLimits {
    fn default() -> Self {
        Self {
            max_entries: 1_024,
            max_name_bytes: 64 * 1_024,
        }
    }
}

/// File type observed without following the named entry.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum InspectedKind {
    Directory,
    RegularFile,
    Socket,
    Symlink,
    Other,
}

/// Descriptor-relative metadata for one state entry.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InspectedEntry {
    pub path: PathBuf,
    pub kind: InspectedKind,
    pub mode: u32,
    pub uid: u32,
    pub links: u64,
    pub size: u64,
    pub device: u64,
    pub inode: u64,
    /// None means the entry passed the checks available to this snapshot.
    pub unsafe_reason: Option<&'static str>,
}

impl InspectedEntry {
    pub fn safe(&self) -> bool {
        self.unsafe_reason.is_none()
    }

    /// Whether an owner-only parent contains every reported risk.
    ///
    /// Broader leaf mode bits are harmless behind a 0700 parent. Links,
    /// foreign ownership, and unsupported file types remain unsafe.
    pub fn safe_beneath_owner_only_parent(&self) -> bool {
        self.safe()
            || self.unsafe_reason == Some("state entry permissions grant access beyond the owner")
    }
}

/// One bounded directory snapshot.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DirectoryInspection {
    pub directory: InspectedEntry,
    pub entries: Vec<InspectedEntry>,
    pub retained_name_bytes: usize,
    pub truncated: bool,
}

/// One bounded regular-file snapshot from a held descriptor.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FileInspection {
    pub entry: InspectedEntry,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Held read-only authority for an existing Cyclops state root.
///
/// Unlike [`StateRoot`], this type never creates or repairs anything. All
/// descendant lookups remain relative to the held root descriptor.
#[derive(Debug)]
pub struct StateInspector {
    directory: File,
    path: PathBuf,
    owner: u32,
    root: InspectedEntry,
}

/// One exact inspected file or empty directory bound for explicit removal.
/// Dropping the handle without calling [`BoundStateRemoval::remove`] changes nothing.
pub struct BoundStateRemoval {
    parent: File,
    target: UnlockOnDropFile,
    name: CString,
    path: PathBuf,
    expected: InspectedEntry,
    kind: RemovalKind,
}

/// An open, owner-only Cyclops state directory.
#[derive(Debug)]
pub struct StateRoot {
    directory: File,
    path: PathBuf,
    owner: u32,
}

/// Cleanup authority for one validated root-level Unix socket.
pub struct BoundSocketCleanup {
    directory: File,
    name: CString,
    path: PathBuf,
    identity: SocketIdentity,
    armed: bool,
}

/// One published state file removed through its held parent on drop.
#[derive(Debug)]
pub struct TransientStateFile {
    directory: File,
    name: CString,
    path: PathBuf,
    identity: RegularIdentity,
    armed: bool,
}

impl StateInspector {
    /// Open an existing state root without creating or repairing it.
    ///
    /// A missing root is a normal absent result. A linked component, foreign
    /// owner, or unstable root identity is an error.
    pub fn open_existing(path: &Path) -> Result<Option<Self>, StateError> {
        let parts = inspection_root_parts(path)?;
        let owner = effective_uid();
        let mut directory = open_start(path)?;

        for (index, part) in parts.iter().enumerate() {
            if index + 1 == parts.len() {
                validate_root_parent(&directory, path)?;
            }
            let name = c_name(path, part)?;
            directory = match open_directory_at(&directory, &name) {
                Ok(next) => next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(path_error(
                        path,
                        error,
                        "state root contains a linked or non-directory component",
                    ));
                }
            };
        }

        let metadata = directory
            .metadata()
            .map_err(|source| io_error(path, source))?;
        if metadata.uid() != owner {
            return Err(unsafe_path(path, "state directory belongs to another user"));
        }
        let root = inspected_from_metadata(path.to_path_buf(), &metadata, owner);
        Ok(Some(Self {
            directory,
            path: path.to_path_buf(),
            owner,
            root,
        }))
    }

    /// Display path only. Inspection authority stays on the held descriptor.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Metadata for the held root descriptor.
    pub fn root(&self) -> &InspectedEntry {
        &self.root
    }

    /// Check that a fresh no-follow lookup still names this held root.
    pub fn path_matches_held_root(&self) -> Result<bool, StateError> {
        let parts = inspection_root_parts(&self.path)?;
        let mut directory = open_start(&self.path)?;
        for part in parts {
            let name = c_name(&self.path, part)?;
            directory = match open_directory_at(&directory, &name) {
                Ok(next) => next,
                Err(_) => return Ok(false),
            };
        }
        let current = directory
            .metadata()
            .map_err(|source| io_error(&self.path, source))?;
        Ok(current.dev() == self.root.device
            && current.ino() == self.root.inode
            && current.uid() == self.owner
            && current.file_type().is_dir())
    }

    /// Inspect the state root's direct children within explicit hard bounds.
    pub fn inspect_root(
        &self,
        limits: InspectionLimits,
    ) -> Result<DirectoryInspection, StateError> {
        inspect_directory_descriptor(
            clone_file(&self.directory, &self.path)?,
            self.path.clone(),
            self.owner,
            self.root.clone(),
            limits,
        )
    }

    /// Inspect direct children of one existing descendant directory.
    ///
    /// Missing is distinct from an empty directory. Every path component is
    /// opened relative to the held root and symbolic links are never followed.
    pub fn inspect_directory(
        &self,
        descendant: &Path,
        limits: InspectionLimits,
    ) -> Result<Option<DirectoryInspection>, StateError> {
        let Some((directory, display_path, metadata)) =
            self.open_descendant_directory(descendant)?
        else {
            return Ok(None);
        };
        let inspected = inspected_from_metadata(display_path.clone(), &metadata, self.owner);
        inspect_directory_descriptor(directory, display_path, self.owner, inspected, limits)
            .map(Some)
    }

    /// Inspect a directory entry returned by an earlier snapshot.
    ///
    /// The path must still name the same device, inode, type, owner, and link
    /// count. This is the recursive-walk form because it cannot cross from an
    /// enumerated directory into a replacement directory.
    pub fn inspect_bound_directory(
        &self,
        expected: &InspectedEntry,
        limits: InspectionLimits,
    ) -> Result<DirectoryInspection, StateError> {
        if expected.kind != InspectedKind::Directory {
            return Err(unsafe_path(
                &expected.path,
                "bound inspection target is not a directory",
            ));
        }
        let descendant = expected.path.strip_prefix(&self.path).map_err(|_| {
            unsafe_path(
                &expected.path,
                "bound inspection target is outside the state root",
            )
        })?;
        let Some((directory, display_path, metadata)) =
            self.open_descendant_directory(descendant)?
        else {
            return Err(unsafe_path(
                &expected.path,
                "state entry changed during read-only inspection",
            ));
        };
        let current = inspected_from_metadata(display_path.clone(), &metadata, self.owner);
        if current.device != expected.device
            || current.inode != expected.inode
            || current.kind != expected.kind
            || current.uid != expected.uid
            || current.links != expected.links
            || current.mode != expected.mode
            || current.size != expected.size
        {
            return Err(unsafe_path(
                &expected.path,
                "state entry changed during read-only inspection",
            ));
        }
        inspect_directory_descriptor(directory, display_path, self.owner, current, limits)
    }

    fn open_descendant_directory(
        &self,
        descendant: &Path,
    ) -> Result<Option<(File, PathBuf, std::fs::Metadata)>, StateError> {
        let parts = inspection_descendant_parts(descendant)?;
        let display_path = self.path.join(descendant);
        let mut directory = clone_file(&self.directory, &display_path)?;
        for part in parts {
            let name = c_name(&display_path, part)?;
            directory = match open_directory_at(&directory, &name) {
                Ok(next) => next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(path_error(
                        &display_path,
                        error,
                        "state path contains a linked or non-directory component",
                    ));
                }
            };
            let metadata = directory
                .metadata()
                .map_err(|source| io_error(&display_path, source))?;
            if metadata.uid() != self.owner {
                return Err(unsafe_path(
                    &display_path,
                    "state directory belongs to another user",
                ));
            }
        }
        let metadata = directory
            .metadata()
            .map_err(|source| io_error(&display_path, source))?;
        Ok(Some((directory, display_path, metadata)))
    }

    /// Read one existing regular state file through a held descriptor.
    ///
    /// The byte limit is checked before allocation. The file identity is
    /// compared before and after the read, and links are refused.
    pub fn read_file(
        &self,
        descendant: &Path,
        max_bytes: usize,
    ) -> Result<Option<FileInspection>, StateError> {
        if max_bytes > INSPECTION_FILE_BYTES_LIMIT_MAX {
            return Err(unsafe_path(
                &self.path.join(descendant),
                "inspection file-byte limit exceeds the hard ceiling",
            ));
        }
        let parts = inspection_descendant_parts(descendant)?;
        let display_path = self.path.join(descendant);
        let mut directory = clone_file(&self.directory, &display_path)?;
        for part in &parts[..parts.len() - 1] {
            let name = c_name(&display_path, part)?;
            directory = match open_directory_at(&directory, &name) {
                Ok(next) => next,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(path_error(
                        &display_path,
                        error,
                        "state path contains a linked or non-directory component",
                    ));
                }
            };
            let metadata = directory
                .metadata()
                .map_err(|source| io_error(&display_path, source))?;
            if metadata.uid() != self.owner {
                return Err(unsafe_path(
                    &display_path,
                    "state directory belongs to another user",
                ));
            }
        }

        let leaf = c_name(
            &display_path,
            parts.last().expect("descendant has one component"),
        )?;
        let before = match stat_at_optional(&directory, &leaf, &display_path)? {
            Some(metadata) => metadata,
            None => return Ok(None),
        };
        validate_regular_stat(&before, &display_path, self.owner)?;
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        // SAFETY: directory and leaf are held descriptors and a valid C name.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), leaf.as_ptr(), flags) };
        if fd < 0 {
            return Err(path_error(
                &display_path,
                std::io::Error::last_os_error(),
                "state file could not be opened for read-only inspection",
            ));
        }
        // SAFETY: fd is a fresh successful openat result owned here.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file
            .metadata()
            .map_err(|source| io_error(&display_path, source))?;
        if !metadata_matches_stat(&metadata, &before) {
            return Err(unsafe_path(
                &display_path,
                "state entry changed during read-only inspection",
            ));
        }

        let read_limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1_024));
        let mut limited = (&file).take(read_limit);
        limited
            .read_to_end(&mut bytes)
            .map_err(|source| io_error(&display_path, source))?;
        let truncated = bytes.len() > max_bytes;
        if truncated {
            bytes.truncate(max_bytes);
        }
        inspect_after_read(&display_path);
        let descriptor_after = file
            .metadata()
            .map_err(|source| io_error(&display_path, source))?;
        if !metadata_stable_during_read(&metadata, &descriptor_after) {
            return Err(unsafe_path(
                &display_path,
                "state entry changed during read-only inspection",
            ));
        }
        let after = stat_at(&directory, &leaf, &display_path)?;
        if !same_stat_identity(&before, &after) {
            return Err(unsafe_path(
                &display_path,
                "state entry changed during read-only inspection",
            ));
        }
        let entry = inspected_from_stat(display_path, &after, self.owner);
        Ok(Some(FileInspection {
            entry,
            bytes,
            truncated,
        }))
    }

    /// Bind one inspected regular file for one explicit removal attempt.
    pub fn bind_regular_file_for_removal(
        &self,
        expected: &InspectedEntry,
    ) -> Result<BoundStateRemoval, StateError> {
        if expected.kind != InspectedKind::RegularFile
            || expected.uid != self.owner
            || expected.links != 1
        {
            return Err(unsafe_path(
                &expected.path,
                "removal target is not one owned single-link regular file",
            ));
        }
        self.bind_for_removal(expected, RemovalKind::RegularFile)
    }

    /// Bind one inspected empty directory for one explicit removal attempt.
    pub fn bind_empty_directory_for_removal(
        &self,
        expected: &InspectedEntry,
    ) -> Result<BoundStateRemoval, StateError> {
        if expected.kind != InspectedKind::Directory || expected.uid != self.owner {
            return Err(unsafe_path(
                &expected.path,
                "removal target is not one owned directory",
            ));
        }
        self.bind_for_removal(expected, RemovalKind::EmptyDirectory)
    }

    /// Atomically move one exact direct child directory to a private name.
    ///
    /// The move never replaces an existing entry. Callers can keep an
    /// in-directory lease held while removing the isolated namespace.
    pub fn isolate_direct_child_directory(
        &self,
        expected: &InspectedEntry,
        isolated_name: &OsStr,
    ) -> Result<InspectedEntry, StateError> {
        if expected.kind != InspectedKind::Directory
            || expected.uid != self.owner
            || expected.path.parent() != Some(self.path.as_path())
        {
            return Err(unsafe_path(
                &expected.path,
                "isolation target is not one owned direct child directory",
            ));
        }
        let isolated_path = self.path.join(isolated_name);
        let isolated_parts = inspection_descendant_parts(Path::new(isolated_name))?;
        if isolated_parts.len() != 1 || isolated_name.as_bytes().len() > 240 {
            return Err(unsafe_path(
                &isolated_path,
                "isolation name is not one bounded path component",
            ));
        }
        let old_name = c_name(
            &expected.path,
            expected
                .path
                .file_name()
                .expect("direct child has one file name"),
        )?;
        let new_name = c_name(&isolated_path, isolated_name)?;
        let before =
            stat_at_optional(&self.directory, &old_name, &expected.path)?.ok_or_else(|| {
                unsafe_path(&expected.path, "isolation target changed before binding")
            })?;
        validate_removal_stat(&before, expected, RemovalKind::EmptyDirectory)?;
        let target = open_directory_at(&self.directory, &old_name).map_err(|error| {
            path_error(
                &expected.path,
                error,
                "isolation target changed before binding",
            )
        })?;
        validate_removal_descriptor(&target, expected, RemovalKind::EmptyDirectory)?;
        if stat_at_optional(&self.directory, &new_name, &isolated_path)?.is_some() {
            return Err(unsafe_path(
                &isolated_path,
                "isolation target already exists",
            ));
        }

        // SAFETY: both names are bounded direct children of the held parent.
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                self.directory.as_raw_fd(),
                old_name.as_ptr(),
                self.directory.as_raw_fd(),
                new_name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        // SAFETY: both names are bounded direct children of the held parent.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                self.directory.as_raw_fd(),
                old_name.as_ptr(),
                self.directory.as_raw_fd(),
                new_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(path_error(
                &isolated_path,
                std::io::Error::last_os_error(),
                "could not isolate state directory without replacement",
            ));
        }
        let after = stat_at(&self.directory, &new_name, &isolated_path)?;
        validate_removal_stat(&after, expected, RemovalKind::EmptyDirectory).map_err(|_| {
            unsafe_path(
                &isolated_path,
                "isolated state directory changed during rename",
            )
        })?;
        sync_directory(&self.directory).map_err(|source| StateError::RemovalDurabilityUnknown {
            path: isolated_path.clone(),
            source,
        })?;
        let mut isolated = expected.clone();
        isolated.path = isolated_path;
        Ok(isolated)
    }

    fn bind_for_removal(
        &self,
        expected: &InspectedEntry,
        kind: RemovalKind,
    ) -> Result<BoundStateRemoval, StateError> {
        let descendant = expected
            .path
            .strip_prefix(&self.path)
            .map_err(|_| unsafe_path(&expected.path, "removal target is outside the state root"))?;
        let parts = inspection_descendant_parts(descendant)?;
        let mut parent = clone_file(&self.directory, &expected.path)?;
        for part in &parts[..parts.len() - 1] {
            let name = c_name(&expected.path, part)?;
            parent = open_directory_at(&parent, &name).map_err(|error| {
                path_error(
                    &expected.path,
                    error,
                    "removal target parent is linked or changed",
                )
            })?;
            let metadata = parent
                .metadata()
                .map_err(|source| io_error(&expected.path, source))?;
            if metadata.uid() != self.owner {
                return Err(unsafe_path(
                    &expected.path,
                    "removal target parent belongs to another user",
                ));
            }
        }
        let name = c_name(
            &expected.path,
            parts.last().expect("descendant has one component"),
        )?;
        let before = stat_at_optional(&parent, &name, &expected.path)?
            .ok_or_else(|| unsafe_path(&expected.path, "removal target changed before binding"))?;
        validate_removal_stat(&before, expected, kind)?;
        let target = match kind {
            RemovalKind::RegularFile => {
                open_lockable_regular_at(&parent, &name).map_err(|error| {
                    path_error(
                        &expected.path,
                        error,
                        "removal target changed before binding",
                    )
                })?
            }
            RemovalKind::EmptyDirectory => open_directory_at(&parent, &name).map_err(|error| {
                path_error(
                    &expected.path,
                    error,
                    "removal target changed before binding",
                )
            })?,
        };
        validate_removal_descriptor(&target, expected, kind)?;
        let after = stat_at_optional(&parent, &name, &expected.path)?
            .ok_or_else(|| unsafe_path(&expected.path, "removal target changed before binding"))?;
        validate_removal_stat(&after, expected, kind)?;

        if matches!(kind, RemovalKind::EmptyDirectory) {
            let snapshot = inspect_directory_descriptor(
                clone_file(&target, &expected.path)?,
                expected.path.clone(),
                self.owner,
                expected.clone(),
                InspectionLimits::new(1, INSPECTION_NAME_BYTES_LIMIT_MAX)
                    .expect("removal inspection limits fit hard ceilings"),
            )?;
            if snapshot.truncated || !snapshot.entries.is_empty() {
                return Err(unsafe_path(
                    &expected.path,
                    "removal target directory is not empty",
                ));
            }
        }

        Ok(BoundStateRemoval {
            parent,
            target: UnlockOnDropFile::new(target),
            name,
            path: expected.path.clone(),
            expected: expected.clone(),
            kind,
        })
    }
}

struct SocketIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
    owner: libc::uid_t,
    links: libc::nlink_t,
}

#[derive(Debug)]
struct RegularIdentity {
    device: u64,
    inode: u64,
    owner: u32,
    links: u64,
}

#[derive(Clone, Copy)]
enum RemovalKind {
    RegularFile,
    EmptyDirectory,
}

impl StateRoot {
    /// Open or create the state root without following any path component.
    pub fn open_or_create(path: &Path) -> Result<Self, StateError> {
        Self::open_root(path, true)?.ok_or_else(|| StateError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        })
    }

    /// Open an existing state root. A missing root is not an error.
    pub fn open_existing(path: &Path) -> Result<Option<Self>, StateError> {
        Self::open_root(path, false)
    }

    fn open_root(path: &Path, create: bool) -> Result<Option<Self>, StateError> {
        let parts = root_parts(path)?;
        let owner = effective_uid();
        let mut directory = open_start(path)?;

        // 1. Walk existing ancestors without following links. Ancestors are
        // outside Cyclops ownership, so they are inspected but never chmodded.
        for part in &parts[..parts.len() - 1] {
            let name = c_name(path, part)?;
            directory = match open_directory_at(&directory, &name) {
                Ok(next) => next,
                Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => {
                    return Err(path_error(
                        path,
                        error,
                        "state root contains a linked or non-directory component",
                    ));
                }
            };
        }

        // 2. A mutable non-sticky parent could replace a restrictive-mode
        // root between inspection and repair. Refuse that race surface.
        validate_root_parent(&directory, path)?;

        // 3. Open or create the root leaf. Only this leaf belongs to Cyclops;
        // its parent can be a normal user home and must remain unchanged.
        let leaf = c_name(path, parts.last().expect("root has one component"))?;
        let root = match owned_directory_at(&directory, &leaf, path, owner, create)? {
            Some(root) => root,
            None => return Ok(None),
        };

        // 4. Keep the descriptor as the authority for all descendant access.
        Ok(Some(Self {
            directory: root,
            path: path.to_path_buf(),
            owner,
        }))
    }

    /// Open or create an appendable regular file beneath this root.
    pub fn open_append(&self, descendant: &Path) -> Result<StateFile, StateError> {
        Ok(self
            .open_file(descendant, true)?
            .expect("create=true returns an opened state file"))
    }

    /// Open a regular file beneath this root for reading.
    pub fn open_read(&self, descendant: &Path) -> Result<Option<StateFile>, StateError> {
        self.open_file(descendant, false)
    }

    /// Atomically create or replace a regular file beneath this root.
    pub fn replace_file(&self, descendant: &Path, contents: &[u8]) -> Result<(), StateError> {
        let parts = descendant_parts(descendant)?;
        let display_path = self.path.join(descendant);
        let mut directory = clone_file(&self.directory, &display_path)?;

        // 1. Resolve or create each parent beneath the held root.
        for part in &parts[..parts.len() - 1] {
            let name = c_name(&display_path, part)?;
            directory = owned_directory_at(&directory, &name, &display_path, self.owner, true)?
                .expect("create=true returns an opened state directory");
        }

        let leaf = c_name(
            &display_path,
            parts.last().expect("descendant has one component"),
        )?;
        // 2. Secure a named temporary file before writing any bytes.
        let mut temporary = ReplacementTemp::create(&directory, &display_path, self.owner)?;
        let result = (|| {
            // 3. The target must stay the same validated entry during the write.
            let target_before =
                replacement_target_at(&directory, &leaf, &display_path, self.owner)?;
            temporary
                .file
                .write_all(contents)
                .map_err(|source| io_error(&temporary.path, source))?;
            temporary
                .file
                .sync_all()
                .map_err(|source| io_error(&temporary.path, source))?;

            let target_after = replacement_target_at(&directory, &leaf, &display_path, self.owner)?;
            validate_replacement_target(
                target_before.as_ref(),
                target_after.as_ref(),
                &display_path,
            )?;
            // 4. Rename within the held parent, then persist its directory entry.
            temporary.rename_to(&leaf, &display_path, self.owner)?;
            sync_directory(&directory).map_err(|source| {
                StateError::ReplacementDurabilityUnknown {
                    path: display_path.clone(),
                    source,
                }
            })?;
            Ok(())
        })();

        match result {
            Ok(()) => Ok(()),
            Err(error) if temporary.live => {
                temporary.remove()?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Publish a new regular file without replacing an existing entry.
    pub fn create_file_once(
        &self,
        descendant: &Path,
        contents: &[u8],
    ) -> Result<CreateFileOutcome, StateError> {
        let parts = descendant_parts(descendant)?;
        let display_path = self.path.join(descendant);
        let mut directory = clone_file(&self.directory, &display_path)?;

        for part in &parts[..parts.len() - 1] {
            let name = c_name(&display_path, part)?;
            directory = owned_directory_at(&directory, &name, &display_path, self.owner, true)?
                .expect("create=true returns an opened state directory");
        }

        let leaf = c_name(
            &display_path,
            parts.last().expect("descendant has one component"),
        )?;
        let mut temporary = ReplacementTemp::create(&directory, &display_path, self.owner)?;
        let result = (|| {
            temporary
                .file
                .write_all(contents)
                .map_err(|source| io_error(&temporary.path, source))?;
            temporary
                .file
                .sync_all()
                .map_err(|source| io_error(&temporary.path, source))?;
            if !temporary.rename_once_to(&leaf, &display_path, self.owner)? {
                temporary.remove()?;
                sync_directory(&directory).map_err(|source| io_error(&display_path, source))?;
                return Ok(CreateFileOutcome::AlreadyExists);
            }
            temporary.validate_published(&leaf, &display_path, self.owner)?;
            sync_directory(&directory).map_err(|source| StateError::CreationDurabilityUnknown {
                path: display_path.clone(),
                source,
            })?;
            Ok(CreateFileOutcome::Created)
        })();

        match result {
            Ok(outcome) => Ok(outcome),
            Err(error) if temporary.live => {
                temporary.remove()?;
                sync_directory(&directory).map_err(|source| io_error(&display_path, source))?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Publish an owner-only file and remove that exact inode on drop.
    pub fn create_transient_file(
        &self,
        descendant: &Path,
        contents: &[u8],
    ) -> Result<TransientStateFile, StateError> {
        let parts = descendant_parts(descendant)?;
        let display_path = self.path.join(descendant);
        let mut directory = clone_file(&self.directory, &display_path)?;

        for part in &parts[..parts.len() - 1] {
            let name = c_name(&display_path, part)?;
            directory = owned_directory_at(&directory, &name, &display_path, self.owner, true)?
                .expect("create=true returns an opened state directory");
        }

        let leaf = c_name(
            &display_path,
            parts.last().expect("descendant has one component"),
        )?;
        let mut temporary = ReplacementTemp::create(&directory, &display_path, self.owner)?;
        let result = (|| {
            temporary
                .file
                .write_all(contents)
                .map_err(|source| io_error(&temporary.path, source))?;
            temporary
                .file
                .sync_all()
                .map_err(|source| io_error(&temporary.path, source))?;
            let cleanup_directory = clone_file(&directory, &display_path)?;
            let metadata = temporary
                .file
                .metadata()
                .map_err(|source| io_error(&display_path, source))?;

            if !temporary.rename_once_to(&leaf, &display_path, self.owner)? {
                return Err(io_error(
                    &display_path,
                    std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                ));
            }
            temporary.validate_published(&leaf, &display_path, self.owner)?;
            Ok(TransientStateFile {
                directory: cleanup_directory,
                name: leaf.clone(),
                path: display_path.clone(),
                identity: RegularIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    owner: metadata.uid(),
                    links: metadata.nlink(),
                },
                armed: true,
            })
        })();

        match result {
            Ok(file) => Ok(file),
            Err(error) if temporary.live => {
                temporary.remove()?;
                sync_directory(&directory).map_err(|source| io_error(&display_path, source))?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// List direct regular-file names in an existing directory beneath this root.
    pub fn regular_file_names(&self, descendant: &Path) -> Result<Vec<OsString>, StateError> {
        let parts = descendant_parts(descendant)?;
        let display_path = self.path.join(descendant);
        let mut directory = clone_file(&self.directory, &display_path)?;

        for part in parts {
            let name = c_name(&display_path, part)?;
            directory = owned_directory_at(&directory, &name, &display_path, self.owner, false)?
                .ok_or_else(|| {
                    io_error(
                        &display_path,
                        std::io::Error::from(std::io::ErrorKind::NotFound),
                    )
                })?;
        }

        read_regular_file_names(&directory, &display_path)
    }

    /// Repair every owned directory and regular file beneath this root.
    ///
    /// `live_socket_leaf` may name one root-level socket already bound by
    /// the daemon. It is validated but never opened or chmodded.
    pub fn repair_descendant_permissions(
        &self,
        live_socket_leaf: Option<&OsStr>,
    ) -> Result<RepairSummary, StateError> {
        validate_directory(&self.directory, &self.path, self.owner)?;
        repair_descriptor_permissions(&self.directory, &self.path, DIRECTORY_MODE)?;
        validate_directory(&self.directory, &self.path, self.owner)?;

        repair_directory_tree(
            clone_file(&self.directory, &self.path)?,
            self.path.clone(),
            self.owner,
            live_socket_leaf,
        )
    }

    /// Check that a fresh no-follow lookup still names this held root.
    pub fn path_matches_held_root(&self) -> Result<bool, StateError> {
        let parts = root_parts(&self.path)?;
        let mut directory = open_start(&self.path)?;
        for part in parts {
            let name = c_name(&self.path, part)?;
            directory = match open_directory_at(&directory, &name) {
                Ok(next) => next,
                Err(_) => return Ok(false),
            };
        }

        let held = self
            .directory
            .metadata()
            .map_err(|source| io_error(&self.path, source))?;
        let fresh = directory
            .metadata()
            .map_err(|source| io_error(&self.path, source))?;
        Ok(fresh.file_type().is_dir()
            && fresh.dev() == held.dev()
            && fresh.ino() == held.ino()
            && fresh.uid() == self.owner)
    }

    /// Capture cleanup authority for one validated root-level socket.
    pub fn bound_socket_cleanup(&self, leaf: &OsStr) -> Result<BoundSocketCleanup, StateError> {
        let descendant = Path::new(leaf);
        let parts = descendant_parts(descendant)?;
        if parts.len() != 1 {
            return Err(unsafe_path(
                &self.path.join(descendant),
                "state socket must be a root-level entry",
            ));
        }
        let path = self.path.join(descendant);
        let name = c_name(&path, parts[0])?;
        let metadata = stat_at(&self.directory, &name, &path)?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            return Err(unsafe_path(&path, "state socket entry is not a socket"));
        }
        if metadata.st_uid != self.owner {
            return Err(unsafe_path(&path, "state socket belongs to another user"));
        }
        if metadata.st_nlink != 1 {
            return Err(unsafe_path(&path, "state socket has multiple hard links"));
        }
        Ok(BoundSocketCleanup {
            directory: clone_file(&self.directory, &path)?,
            name,
            path,
            identity: SocketIdentity {
                device: metadata.st_dev,
                inode: metadata.st_ino,
                owner: metadata.st_uid,
                links: metadata.st_nlink,
            },
            armed: true,
        })
    }

    /// Display path only. Security decisions use the held descriptor.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open_file(&self, descendant: &Path, create: bool) -> Result<Option<StateFile>, StateError> {
        let parts = descendant_parts(descendant)?;
        let display_path = self.path.join(descendant);
        let mut directory = clone_file(&self.directory, &display_path)?;

        // 1. Resolve each parent relative to the validated root. Every owned
        // directory is checked before its mode is repaired.
        for part in &parts[..parts.len() - 1] {
            let name = c_name(&display_path, part)?;
            directory =
                match owned_directory_at(&directory, &name, &display_path, self.owner, create)? {
                    Some(next) => next,
                    None => return Ok(None),
                };
        }

        // 2. Open the leaf without following links. O_NONBLOCK prevents a
        // special file from blocking before its type is rejected.
        let leaf = c_name(
            &display_path,
            parts.last().expect("descendant has one component"),
        )?;
        let access = if create {
            libc::O_RDWR | libc::O_APPEND | libc::O_CREAT
        } else {
            libc::O_RDONLY
        };
        let flags = access | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        let mut expected_inode = None;
        // SAFETY: directory and leaf are valid descriptors/C strings; openat
        // returns a new descriptor that this function takes ownership of.
        let mut fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                leaf.as_ptr(),
                flags,
                FILE_MODE as libc::c_uint,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if !create && error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            if error.kind() != std::io::ErrorKind::PermissionDenied {
                return Err(path_error(
                    &display_path,
                    error,
                    "state leaf is linked or not a regular file",
                ));
            }

            // A prior run can leave an owner file with mode 000. The parent is
            // already 0700, so recover relative to it and bind the retry to the
            // same inspected inode.
            let before = stat_at(&directory, &leaf, &display_path)?;
            validate_regular_stat(&before, &display_path, self.owner)?;
            chmod_at(&directory, &leaf, &display_path, FILE_MODE, &before)?;
            let after = stat_at(&directory, &leaf, &display_path)?;
            if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
                return Err(unsafe_path(
                    &display_path,
                    "state file changed during validation",
                ));
            }
            validate_regular_stat(&after, &display_path, self.owner)?;
            expected_inode = Some((after.st_dev as u64, after.st_ino));
            // SAFETY: the same validated parent/name pair is retried after
            // owner-only mode repair; a successful fd is owned here.
            fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    leaf.as_ptr(),
                    flags,
                    FILE_MODE as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(path_error(
                    &display_path,
                    std::io::Error::last_os_error(),
                    "state file changed during validation",
                ));
            }
        }
        // SAFETY: fd is a fresh successful openat result owned by this scope.
        let file = unsafe { File::from_raw_fd(fd) };

        // 3. Validate type, owner, and link count before chmod or byte access.
        validate_regular(&file, &display_path, self.owner)?;
        if let Some((device, inode)) = expected_inode {
            let metadata = file
                .metadata()
                .map_err(|source| io_error(&display_path, source))?;
            if metadata.dev() != device || metadata.ino() != inode {
                return Err(unsafe_path(
                    &display_path,
                    "state file changed during validation",
                ));
            }
        }

        // 4. Repair through the validated descriptor before exposing bytes.
        repair_descriptor_permissions(&file, &display_path, FILE_MODE)?;
        validate_regular(&file, &display_path, self.owner)?;
        sync_create_capable_entry(&directory, &file, &display_path, create)?;

        Ok(Some(StateFile {
            file: UnlockOnDropFile::new(file),
            path: display_path,
        }))
    }
}

impl BoundSocketCleanup {
    /// Remove the captured socket if the held root still names it exactly.
    pub fn remove(mut self) -> Result<(), StateError> {
        self.remove_inner()
    }

    fn remove_inner(&mut self) -> Result<(), StateError> {
        if !self.armed {
            return Ok(());
        }
        let Some(metadata) = stat_at_optional(&self.directory, &self.name, &self.path)? else {
            self.armed = false;
            return Ok(());
        };
        let same_socket = metadata.st_mode & libc::S_IFMT == libc::S_IFSOCK
            && metadata.st_dev == self.identity.device
            && metadata.st_ino == self.identity.inode
            && metadata.st_uid == self.identity.owner
            && metadata.st_nlink == self.identity.links;
        if !same_socket {
            self.armed = false;
            return Err(unsafe_path(
                &self.path,
                "state socket changed before cleanup",
            ));
        }

        // SAFETY: the held root and validated name identify the captured socket.
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                self.armed = false;
                return Ok(());
            }
            return Err(path_error(
                &self.path,
                error,
                "could not remove state socket",
            ));
        }
        self.armed = false;
        Ok(())
    }
}

impl BoundStateRemoval {
    /// Display path only. Removal and locking use the held descriptors.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Try to hold this exact regular file against another lease holder.
    pub fn try_lock(&self) -> std::io::Result<bool> {
        if !matches!(self.kind, RemovalKind::RegularFile) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "only a regular-file removal target can be locked",
            ));
        }
        // SAFETY: target is a held regular-file descriptor. Update uses the
        // same flock contract, so both sides serialize on the same inode.
        if unsafe { libc::flock(self.target.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(error)
        }
    }

    /// Remove the exact bound entry and durably record the parent change.
    pub fn remove(self) -> Result<(), StateError> {
        validate_removal_descriptor(&self.target, &self.expected, self.kind)
            .map_err(|_| unsafe_path(&self.path, "bound removal target changed before removal"))?;
        let named = stat_at_optional(&self.parent, &self.name, &self.path)?.ok_or_else(|| {
            unsafe_path(&self.path, "bound removal target changed before removal")
        })?;
        validate_removal_stat(&named, &self.expected, self.kind)
            .map_err(|_| unsafe_path(&self.path, "bound removal target changed before removal"))?;

        let flags = if matches!(self.kind, RemovalKind::EmptyDirectory) {
            libc::AT_REMOVEDIR
        } else {
            0
        };
        // SAFETY: the held parent and immediately revalidated name identify
        // the descriptor-bound entry. Directory removal is never recursive.
        let result = unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), flags) };
        if result != 0 {
            return Err(path_error(
                &self.path,
                std::io::Error::last_os_error(),
                "could not remove bound state entry",
            ));
        }
        sync_directory(&self.parent).map_err(|source| StateError::RemovalDurabilityUnknown {
            path: self.path,
            source,
        })
    }
}

impl TransientStateFile {
    /// Display path for the external reader that consumes this file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the published file if its held name still identifies it.
    pub fn remove(mut self) -> Result<(), StateError> {
        self.remove_inner()
    }

    fn remove_inner(&mut self) -> Result<(), StateError> {
        if !self.armed {
            return Ok(());
        }
        let Some(metadata) = stat_at_optional(&self.directory, &self.name, &self.path)? else {
            self.armed = false;
            return Ok(());
        };
        let same_file = metadata.st_mode & libc::S_IFMT == libc::S_IFREG
            && u64::try_from(metadata.st_dev).ok() == Some(self.identity.device)
            && metadata.st_ino == self.identity.inode
            && metadata.st_uid == self.identity.owner
            && u64::from(metadata.st_nlink) == self.identity.links;
        if !same_file {
            self.armed = false;
            return Err(unsafe_path(
                &self.path,
                "transient state file changed before cleanup",
            ));
        }

        // SAFETY: the held parent and validated name identify this file.
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                self.armed = false;
                return Ok(());
            }
            return Err(path_error(
                &self.path,
                error,
                "could not remove transient state file",
            ));
        }
        self.armed = false;
        sync_directory(&self.directory).map_err(|source| io_error(&self.path, source))
    }
}

impl Drop for BoundSocketCleanup {
    fn drop(&mut self) {
        let _ = self.remove_inner();
    }
}

impl Drop for TransientStateFile {
    fn drop(&mut self) {
        let _ = self.remove_inner();
    }
}

/// A validated regular state file.
pub struct StateFile {
    file: UnlockOnDropFile,
    path: PathBuf,
}

/// A descriptor that explicitly releases any held lock before closing.
struct UnlockOnDropFile(Option<File>);

impl UnlockOnDropFile {
    fn new(file: File) -> Self {
        Self(Some(file))
    }

    fn into_inner(mut self) -> File {
        self.0.take().expect("state descriptor is present")
    }
}

impl std::ops::Deref for UnlockOnDropFile {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("state descriptor is present")
    }
}

impl std::ops::DerefMut for UnlockOnDropFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("state descriptor is present")
    }
}

impl Drop for UnlockOnDropFile {
    fn drop(&mut self) {
        if let Some(file) = self.0.as_ref() {
            let _ = file.unlock();
        }
    }
}

struct DirectoryStream(*mut libc::DIR);

struct ReplacementTemp {
    parent: File,
    file: File,
    name: CString,
    path: PathBuf,
    live: bool,
}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: fdopendir returned this stream and ownership is held here.
        unsafe { libc::closedir(self.0) };
    }
}

impl ReplacementTemp {
    fn create(parent: &File, target_path: &Path, owner: u32) -> Result<Self, StateError> {
        let parent = clone_file(parent, target_path)?;
        for _ in 0..REPLACEMENT_TEMP_ATTEMPTS {
            let sequence =
                REPLACEMENT_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = CString::new(format!(
                "{REPLACEMENT_TEMP_PREFIX}{}-{sequence}",
                std::process::id()
            ))
            .expect("temporary state name has no NUL byte");
            if target_path
                .file_name()
                .is_some_and(|target| target.as_bytes() == name.as_bytes())
            {
                continue;
            }
            let path = target_path
                .parent()
                .expect("state descendant has a parent")
                .join(OsStr::from_bytes(name.as_bytes()));
            let flags =
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
            // SAFETY: parent and name are valid. O_EXCL gives this call sole
            // ownership of a new directory entry.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    flags,
                    FILE_MODE as libc::c_uint,
                )
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(path_error(
                    &path,
                    error,
                    "could not create temporary state file",
                ));
            }

            // SAFETY: fd is a fresh successful openat result owned here.
            let file = unsafe { File::from_raw_fd(fd) };
            let mut temporary = Self {
                parent,
                file,
                name,
                path,
                live: true,
            };
            let result = (|| {
                validate_regular(&temporary.file, &temporary.path, owner)?;
                repair_descriptor_permissions(&temporary.file, &temporary.path, FILE_MODE)?;
                temporary.validate_named(owner)
            })();
            return match result {
                Ok(()) => Ok(temporary),
                Err(error) => {
                    temporary.remove()?;
                    Err(error)
                }
            };
        }

        Err(io_error(
            target_path,
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not reserve a temporary state file",
            ),
        ))
    }

    fn validate_named(&self, owner: u32) -> Result<(), StateError> {
        validate_regular(&self.file, &self.path, owner)?;
        let descriptor = self
            .file
            .metadata()
            .map_err(|source| io_error(&self.path, source))?;
        let named = stat_at(&self.parent, &self.name, &self.path)?;
        validate_regular_stat(&named, &self.path, owner)?;
        if descriptor.dev() != named.st_dev as u64 || descriptor.ino() != named.st_ino {
            return Err(unsafe_path(
                &self.path,
                "temporary state file changed before replacement",
            ));
        }
        Ok(())
    }

    fn rename_to(
        &mut self,
        target: &CString,
        target_path: &Path,
        owner: u32,
    ) -> Result<(), StateError> {
        self.validate_named(owner)?;
        // SAFETY: both names are valid and resolve beneath the held parent.
        let result = unsafe {
            libc::renameat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                self.parent.as_raw_fd(),
                target.as_ptr(),
            )
        };
        if result != 0 {
            return Err(path_error(
                target_path,
                std::io::Error::last_os_error(),
                "state replacement target changed before rename",
            ));
        }
        self.live = false;
        Ok(())
    }

    fn rename_once_to(
        &mut self,
        target: &CString,
        target_path: &Path,
        owner: u32,
    ) -> Result<bool, StateError> {
        self.validate_named(owner)?;
        // SAFETY: both names resolve beneath the same held parent. The flags
        // make publication fail instead of replacing an existing entry.
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                self.parent.as_raw_fd(),
                target.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                self.parent.as_raw_fd(),
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            self.live = false;
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Ok(false);
        }
        Err(path_error(
            target_path,
            error,
            "could not publish state file without replacement",
        ))
    }

    fn validate_published(
        &self,
        target: &CString,
        target_path: &Path,
        owner: u32,
    ) -> Result<(), StateError> {
        validate_regular(&self.file, target_path, owner)?;
        let descriptor = self
            .file
            .metadata()
            .map_err(|source| io_error(target_path, source))?;
        let named = stat_at(&self.parent, target, target_path)?;
        validate_regular_stat(&named, target_path, owner)?;
        if descriptor.dev() != named.st_dev as u64 || descriptor.ino() != named.st_ino {
            return Err(unsafe_path(
                target_path,
                "published state file changed before validation",
            ));
        }
        Ok(())
    }

    fn remove(&mut self) -> Result<(), StateError> {
        if !self.live {
            return Ok(());
        }
        let descriptor = self
            .file
            .metadata()
            .map_err(|source| io_error(&self.path, source))?;
        let Some(named) = stat_at_optional(&self.parent, &self.name, &self.path)? else {
            self.live = false;
            return Ok(());
        };
        if descriptor.dev() != named.st_dev as u64 || descriptor.ino() != named.st_ino {
            return Err(unsafe_path(
                &self.path,
                "temporary state file changed before cleanup",
            ));
        }

        // SAFETY: the held parent and verified name identify the temporary
        // file created by this replacement.
        let result = unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                self.live = false;
                return Ok(());
            }
            return Err(path_error(
                &self.path,
                error,
                "could not remove temporary state file",
            ));
        }
        self.live = false;
        Ok(())
    }
}

impl Drop for ReplacementTemp {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

impl StateFile {
    /// Display path only. I/O stays bound to the held descriptor.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lock this held file for exclusive cross-process access.
    pub fn lock(&self) -> std::io::Result<()> {
        self.file.lock()
    }

    /// Try to lock this held file without waiting for another process.
    pub fn try_lock(&self) -> std::io::Result<bool> {
        match self.file.try_lock() {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// Release an exclusive lock held by this descriptor.
    pub fn unlock(&self) -> std::io::Result<()> {
        self.file.unlock()
    }

    /// Append one record while keeping the file within a fixed byte limit.
    ///
    /// Rotation keeps recent complete lines from the old tail. A record larger
    /// than the whole limit keeps its final bytes. The held descriptor remains
    /// the only authority and the file is never replaced by path.
    pub fn append_bounded(
        &mut self,
        record: &[u8],
        max_bytes: usize,
        retain_bytes: usize,
    ) -> std::io::Result<()> {
        if max_bytes == 0 || max_bytes > BOUNDED_APPEND_LIMIT_MAX || retain_bytes >= max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bounded append limits are invalid",
            ));
        }

        self.file.lock()?;
        let result = self.append_bounded_locked(record, max_bytes, retain_bytes);
        let unlock = self.file.unlock();
        match result {
            Err(error) => Err(error),
            Ok(()) => unlock,
        }
    }

    /// Try one bounded append without waiting for another file writer.
    pub fn try_append_bounded(
        &mut self,
        record: &[u8],
        max_bytes: usize,
        retain_bytes: usize,
    ) -> std::io::Result<bool> {
        if max_bytes == 0 || max_bytes > BOUNDED_APPEND_LIMIT_MAX || retain_bytes >= max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bounded append limits are invalid",
            ));
        }
        if !self.try_lock()? {
            return Ok(false);
        }
        let result = self.append_bounded_locked(record, max_bytes, retain_bytes);
        let unlock = self.file.unlock();
        match result {
            Err(error) => Err(error),
            Ok(()) => unlock.map(|()| true),
        }
    }

    fn append_bounded_locked(
        &mut self,
        record: &[u8],
        max_bytes: usize,
        retain_bytes: usize,
    ) -> std::io::Result<()> {
        if record.len() >= max_bytes {
            self.file.set_len(0)?;
            self.file
                .write_all(&record[record.len().saturating_sub(max_bytes)..])?;
            return self.file.flush();
        }

        let length = self.file.seek(SeekFrom::End(0))?;
        if length.saturating_add(record.len() as u64) <= max_bytes as u64 {
            self.file.write_all(record)?;
            return self.file.flush();
        }

        let available = max_bytes - record.len();
        let keep = retain_bytes
            .min(available)
            .min(usize::try_from(length).unwrap_or(usize::MAX));
        let start = length.saturating_sub(keep as u64);
        self.file.seek(SeekFrom::Start(start))?;
        let mut tail = Vec::with_capacity(keep);
        Read::by_ref(&mut *self.file)
            .take(keep as u64)
            .read_to_end(&mut tail)?;
        if start > 0 {
            let cut = tail
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(tail.len(), |position| position + 1);
            tail.drain(..cut);
        }

        self.file.set_len(0)?;
        self.file.write_all(&tail)?;
        self.file.write_all(record)?;
        self.file.flush()
    }

    /// Change the length of this held file.
    pub fn set_len(&self, size: u64) -> std::io::Result<()> {
        self.file.set_len(size)
    }

    pub fn sync_data(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }

    /// Return the validated descriptor as a standard file handle.
    pub fn into_file(self) -> File {
        self.file.into_inner()
    }
}

impl Read for StateFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Write for StateFile {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Seek for StateFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

fn sync_create_capable_entry(
    parent: &File,
    entry: &File,
    path: &Path,
    create: bool,
) -> Result<(), StateError> {
    if !create {
        return Ok(());
    }

    // A retry after an uncertain parent sync must reestablish durability.
    entry.sync_all().map_err(|source| io_error(path, source))?;
    sync_directory(parent).map_err(|source| io_error(path, source))
}

fn sync_directory(directory: &File) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_DIRECTORY_SYNC.with(|fail| fail.replace(false)) {
        return Err(std::io::Error::other("injected directory sync failure"));
    }

    directory.sync_all()
}

#[cfg(test)]
fn inject_next_directory_sync_failure() {
    FAIL_NEXT_DIRECTORY_SYNC.with(|fail| fail.set(true));
}

/// Open, validate, and repair one owned directory relative to `parent`.
fn owned_directory_at(
    parent: &File,
    name: &CString,
    path: &Path,
    owner: u32,
    create: bool,
) -> Result<Option<File>, StateError> {
    // 1. mkdirat makes creation relative to the trusted parent. EEXIST is
    // expected and always flows through the same validation as a raced entry.
    if create {
        // SAFETY: parent and name are valid; mkdirat does not retain pointers.
        let result = unsafe {
            libc::mkdirat(
                parent.as_raw_fd(),
                name.as_ptr(),
                DIRECTORY_MODE as libc::mode_t,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(path_error(path, error, "could not create state directory"));
            }
        }
    }

    // 2. Prefer a descriptor open. It gives chmod a race-free target when
    // the directory is searchable under its current mode.
    match open_directory_at(parent, name) {
        Ok(directory) => {
            validate_directory(&directory, path, owner)?;
            repair_descriptor_permissions(&directory, path, DIRECTORY_MODE)?;
            validate_directory(&directory, path, owner)?;
            sync_create_capable_entry(parent, &directory, path, create)?;
            return Ok(Some(directory));
        }
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) if error.kind() != std::io::ErrorKind::PermissionDenied => {
            return Err(path_error(
                path,
                error,
                "state path contains a linked or non-directory component",
            ));
        }
        Err(_) => {}
    }

    // 3. A restrictive umask can create mode 000. Inspect without following,
    // repair relative to the parent, then prove the same inode was repaired.
    let before = stat_at(parent, name, path)?;
    validate_directory_stat(&before, path, owner)?;
    chmod_at(parent, name, path, DIRECTORY_MODE, &before)?;
    let after = stat_at(parent, name, path)?;
    if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
        return Err(unsafe_path(
            path,
            "state directory changed during validation",
        ));
    }
    validate_directory_stat(&after, path, owner)?;

    // 4. The recovered directory must now open and match the inspected inode.
    let directory = open_directory_at(parent, name)
        .map_err(|error| path_error(path, error, "state directory changed during validation"))?;
    let metadata = directory
        .metadata()
        .map_err(|source| io_error(path, source))?;
    if metadata.dev() != after.st_dev as u64 || metadata.ino() != after.st_ino {
        return Err(unsafe_path(
            path,
            "state directory changed during validation",
        ));
    }
    validate_directory(&directory, path, owner)?;
    repair_descriptor_permissions(&directory, path, DIRECTORY_MODE)?;
    validate_directory(&directory, path, owner)?;
    sync_create_capable_entry(parent, &directory, path, create)?;
    Ok(Some(directory))
}

fn validate_directory(file: &File, path: &Path, owner: u32) -> Result<(), StateError> {
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_dir() {
        return Err(unsafe_path(path, "state path component is not a directory"));
    }
    if metadata.uid() != owner {
        return Err(unsafe_path(path, "state directory belongs to another user"));
    }
    Ok(())
}

fn validate_directory_stat(
    metadata: &libc::stat,
    path: &Path,
    owner: u32,
) -> Result<(), StateError> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(unsafe_path(path, "state path component is not a directory"));
    }
    if metadata.st_uid != owner {
        return Err(unsafe_path(path, "state directory belongs to another user"));
    }
    Ok(())
}

fn validate_regular(file: &File, path: &Path, owner: u32) -> Result<(), StateError> {
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() {
        return Err(unsafe_path(path, "state leaf is not a regular file"));
    }
    if metadata.uid() != owner {
        return Err(unsafe_path(path, "state file belongs to another user"));
    }
    if metadata.nlink() != 1 {
        return Err(unsafe_path(path, "state file has multiple hard links"));
    }
    Ok(())
}

fn validate_regular_stat(metadata: &libc::stat, path: &Path, owner: u32) -> Result<(), StateError> {
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(unsafe_path(path, "state leaf is not a regular file"));
    }
    if metadata.st_uid != owner {
        return Err(unsafe_path(path, "state file belongs to another user"));
    }
    if metadata.st_nlink != 1 {
        return Err(unsafe_path(path, "state file has multiple hard links"));
    }
    Ok(())
}

fn validate_root_parent(file: &File, path: &Path) -> Result<(), StateError> {
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    let mode = metadata.mode();
    let writable_by_others = mode & 0o022 != 0;
    let sticky = mode & libc::S_ISVTX as u32 != 0;
    if writable_by_others && !sticky {
        return Err(unsafe_path(
            path,
            "state root parent is writable by other users without sticky protection",
        ));
    }
    Ok(())
}

fn open_start(path: &Path) -> Result<File, StateError> {
    File::open(if path.is_absolute() { "/" } else { "." }).map_err(|source| io_error(path, source))
}

fn open_directory_at(parent: &File, name: &CString) -> std::io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    // SAFETY: parent and name are valid; a successful descriptor is returned
    // with unique ownership to this function.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: fd is a fresh successful openat result owned by this scope.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn validate_inspection_limits(limits: InspectionLimits, path: &Path) -> Result<(), StateError> {
    if limits.max_entries > INSPECTION_ENTRY_LIMIT_MAX {
        return Err(unsafe_path(
            path,
            "inspection entry limit exceeds the hard ceiling",
        ));
    }
    if limits.max_name_bytes > INSPECTION_NAME_BYTES_LIMIT_MAX {
        return Err(unsafe_path(
            path,
            "inspection name-byte limit exceeds the hard ceiling",
        ));
    }
    Ok(())
}

fn inspected_kind(mode: u32) -> InspectedKind {
    match mode & libc::S_IFMT as u32 {
        value if value == libc::S_IFDIR as u32 => InspectedKind::Directory,
        value if value == libc::S_IFREG as u32 => InspectedKind::RegularFile,
        value if value == libc::S_IFSOCK as u32 => InspectedKind::Socket,
        value if value == libc::S_IFLNK as u32 => InspectedKind::Symlink,
        _ => InspectedKind::Other,
    }
}

fn inspection_safety(
    kind: InspectedKind,
    mode: u32,
    uid: u32,
    links: u64,
    owner: u32,
) -> Option<&'static str> {
    if uid != owner {
        return Some("state entry belongs to another user");
    }
    match kind {
        InspectedKind::Symlink => return Some("state entry is a symbolic link"),
        InspectedKind::Other => return Some("state entry has an unsupported file type"),
        InspectedKind::RegularFile if links != 1 => {
            return Some("state file has multiple hard links")
        }
        InspectedKind::Socket if links != 1 => return Some("state socket has multiple hard links"),
        _ => {}
    }
    let permissions = mode & 0o7777;
    let allowed = match kind {
        InspectedKind::Directory => DIRECTORY_MODE,
        InspectedKind::RegularFile => FILE_MODE,
        // A socket is protected by its owner-only parent. Socket mode is
        // platform-managed and therefore reported without imposing 0600.
        InspectedKind::Socket => return None,
        InspectedKind::Symlink | InspectedKind::Other => unreachable!("handled above"),
    };
    if permissions & !allowed != 0 {
        Some("state entry permissions grant access beyond the owner")
    } else {
        None
    }
}

fn inspected_from_metadata(
    path: PathBuf,
    metadata: &std::fs::Metadata,
    owner: u32,
) -> InspectedEntry {
    let mode = metadata.mode();
    let kind = inspected_kind(mode);
    let uid = metadata.uid();
    let links = metadata.nlink();
    InspectedEntry {
        path,
        kind,
        mode: mode & 0o7777,
        uid,
        links,
        size: metadata.size(),
        device: metadata.dev(),
        inode: metadata.ino(),
        unsafe_reason: inspection_safety(kind, mode, uid, links, owner),
    }
}

/// Widen `ino_t` on 32-bit Linux while staying a no-op on 64-bit targets.
#[allow(clippy::unnecessary_cast)]
fn stat_inode(metadata: &libc::stat) -> u64 {
    metadata.st_ino as u64
}

fn inspected_from_stat(path: PathBuf, metadata: &libc::stat, owner: u32) -> InspectedEntry {
    let mode = metadata.st_mode as u32;
    let kind = inspected_kind(mode);
    let uid = metadata.st_uid;
    let links = metadata.st_nlink as u64;
    InspectedEntry {
        path,
        kind,
        mode: mode & 0o7777,
        uid,
        links,
        size: u64::try_from(metadata.st_size).unwrap_or_default(),
        device: metadata.st_dev as u64,
        inode: stat_inode(metadata),
        unsafe_reason: inspection_safety(kind, mode, uid, links, owner),
    }
}

fn same_stat_identity(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && left.st_mode & libc::S_IFMT == right.st_mode & libc::S_IFMT
        && left.st_uid == right.st_uid
        && left.st_nlink == right.st_nlink
}

fn metadata_matches_stat(metadata: &std::fs::Metadata, inspected: &libc::stat) -> bool {
    metadata.dev() == inspected.st_dev as u64
        && metadata.ino() == stat_inode(inspected)
        && metadata.mode() & libc::S_IFMT as u32 == inspected.st_mode as u32 & libc::S_IFMT as u32
        && metadata.uid() == inspected.st_uid
        && metadata.nlink() == inspected.st_nlink as u64
}

fn metadata_stable_during_read(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.mode() & libc::S_IFMT as u32 == after.mode() & libc::S_IFMT as u32
        && before.uid() == after.uid()
        && before.nlink() == after.nlink()
        && before.size() == after.size()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn removal_kind(kind: RemovalKind) -> InspectedKind {
    match kind {
        RemovalKind::RegularFile => InspectedKind::RegularFile,
        RemovalKind::EmptyDirectory => InspectedKind::Directory,
    }
}

fn validate_removal_stat(
    metadata: &libc::stat,
    expected: &InspectedEntry,
    kind: RemovalKind,
) -> Result<(), StateError> {
    let same = inspected_kind(metadata.st_mode as u32) == removal_kind(kind)
        && metadata.st_dev as u64 == expected.device
        && stat_inode(metadata) == expected.inode
        && metadata.st_uid == expected.uid
        && metadata.st_nlink as u64 == expected.links
        && metadata.st_mode as u32 & 0o7777 == expected.mode
        && u64::try_from(metadata.st_size).unwrap_or_default() == expected.size;
    if same {
        Ok(())
    } else {
        Err(unsafe_path(
            &expected.path,
            "removal target changed during binding",
        ))
    }
}

fn validate_removal_descriptor(
    target: &File,
    expected: &InspectedEntry,
    kind: RemovalKind,
) -> Result<(), StateError> {
    let metadata = target
        .metadata()
        .map_err(|source| io_error(&expected.path, source))?;
    let type_matches = match kind {
        RemovalKind::RegularFile => metadata.file_type().is_file(),
        RemovalKind::EmptyDirectory => metadata.file_type().is_dir(),
    };
    let same = type_matches
        && metadata.dev() == expected.device
        && metadata.ino() == expected.inode
        && metadata.uid() == expected.uid
        && metadata.nlink() == expected.links
        && metadata.mode() & 0o7777 == expected.mode
        && metadata.size() == expected.size;
    if same {
        Ok(())
    } else {
        Err(unsafe_path(
            &expected.path,
            "removal target changed during binding",
        ))
    }
}

fn open_inspection_at(
    parent: &File,
    name: &CString,
    kind: InspectedKind,
) -> std::io::Result<Option<File>> {
    if !matches!(kind, InspectedKind::Directory | InspectedKind::RegularFile) {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = match kind {
        InspectedKind::Directory => libc::O_SEARCH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        InspectedKind::RegularFile => {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
        }
        _ => unreachable!("filtered above"),
    };
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: fd is a fresh successful openat result owned here.
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    }
}

fn open_lockable_regular_at(parent: &File, name: &CString) -> std::io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: parent and name are held values. The returned descriptor is
    // retained through removal and supports flock on Linux, unlike O_PATH.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: fd is a fresh successful openat result owned here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn inspect_directory_descriptor(
    directory: File,
    path: PathBuf,
    owner: u32,
    inspected_directory: InspectedEntry,
    limits: InspectionLimits,
) -> Result<DirectoryInspection, StateError> {
    validate_inspection_limits(limits, &path)?;
    let stream_file = open_directory_for_enumeration(&directory, &path)?;
    // SAFETY: stream_file is an open directory descriptor. fdopendir takes
    // ownership only when it returns a non-null stream.
    let stream = unsafe { libc::fdopendir(stream_file.as_raw_fd()) };
    if stream.is_null() {
        return Err(io_error(&path, std::io::Error::last_os_error()));
    }
    std::mem::forget(stream_file);
    let stream = DirectoryStream(stream);
    let mut entries = Vec::with_capacity(limits.max_entries.min(256));
    let mut retained_name_bytes = 0usize;
    let mut truncated = false;

    loop {
        clear_errno();
        // SAFETY: stream remains live until the guard is dropped.
        let raw = unsafe { libc::readdir(stream.0) };
        if raw.is_null() {
            let error = current_errno();
            if error == 0 {
                break;
            }
            return Err(io_error(&path, std::io::Error::from_raw_os_error(error)));
        }
        // SAFETY: readdir returned a live dirent with a NUL-terminated name.
        let name = unsafe { CStr::from_ptr((*raw).d_name.as_ptr()) };
        let name_bytes = name.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let Some(next_name_bytes) = retained_name_bytes.checked_add(name_bytes.len()) else {
            truncated = true;
            break;
        };
        if entries.len() >= limits.max_entries || next_name_bytes > limits.max_name_bytes {
            truncated = true;
            break;
        }

        let name = OsStr::from_bytes(name_bytes).to_os_string();
        let entry_path = path.join(&name);
        inspect_after_read(&entry_path);
        let c_name = c_name(&entry_path, &name)?;
        let before = stat_at_optional(&directory, &c_name, &entry_path)?.ok_or_else(|| {
            unsafe_path(
                &entry_path,
                "state entry changed during read-only inspection",
            )
        })?;
        // d_ino binds the enumeration to the metadata lookup. Missing or
        // different identity cannot support a safe snapshot.
        let read_inode = unsafe { (*raw).d_ino as u64 };
        if read_inode == 0 || read_inode != before.st_ino as u64 {
            return Err(unsafe_path(
                &entry_path,
                "state entry changed during read-only inspection",
            ));
        }
        let kind = inspected_kind(before.st_mode as u32);
        let pinned = open_inspection_at(&directory, &c_name, kind);
        let after = stat_at_optional(&directory, &c_name, &entry_path)?.ok_or_else(|| {
            unsafe_path(
                &entry_path,
                "state entry changed during read-only inspection",
            )
        })?;
        if !same_stat_identity(&before, &after) {
            return Err(unsafe_path(
                &entry_path,
                "state entry changed during read-only inspection",
            ));
        }

        let mut entry = inspected_from_stat(entry_path.clone(), &after, owner);
        match pinned {
            Ok(Some(file)) => {
                let metadata = file
                    .metadata()
                    .map_err(|source| io_error(&entry_path, source))?;
                if !metadata_matches_stat(&metadata, &after) {
                    return Err(unsafe_path(
                        &entry_path,
                        "state entry changed during read-only inspection",
                    ));
                }
            }
            Ok(None) => {}
            Err(_) if entry.unsafe_reason.is_some() => {}
            Err(_) => {
                entry.unsafe_reason = Some("state entry could not be pinned for inspection");
            }
        }
        retained_name_bytes = next_name_bytes;
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(DirectoryInspection {
        directory: inspected_directory,
        entries,
        retained_name_bytes,
        truncated,
    })
}

#[cfg(test)]
fn inspect_after_read(path: &Path) {
    INSPECT_AFTER_READ.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action(path);
        }
    });
}

#[cfg(not(test))]
fn inspect_after_read(_: &Path) {}

#[cfg(test)]
fn inject_inspect_after_read(action: impl FnOnce(&Path) + 'static) {
    INSPECT_AFTER_READ.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

fn read_regular_file_names(directory: &File, path: &Path) -> Result<Vec<OsString>, StateError> {
    let mut names = Vec::new();
    for name in read_entry_names(directory, path)? {
        let entry_path = path.join(&name);
        let c_name = c_name(&entry_path, &name)?;
        let metadata = stat_at(directory, &c_name, &entry_path)?;
        if metadata.st_mode & libc::S_IFMT == libc::S_IFREG {
            names.push(name);
        }
    }
    Ok(names)
}

fn read_entry_names(directory: &File, path: &Path) -> Result<Vec<OsString>, StateError> {
    let stream_file = open_directory_for_enumeration(directory, path)?;
    // SAFETY: stream_file is an open directory descriptor. fdopendir takes
    // ownership only when it returns a non-null stream.
    let stream = unsafe { libc::fdopendir(stream_file.as_raw_fd()) };
    if stream.is_null() {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    std::mem::forget(stream_file);
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();

    loop {
        clear_errno();
        // SAFETY: stream remains live until the guard is dropped.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = current_errno();
            if error == 0 {
                break;
            }
            return Err(io_error(path, std::io::Error::from_raw_os_error(error)));
        }

        // SAFETY: readdir returned a live dirent with a NUL-terminated name.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        names.push(OsStr::from_bytes(name.to_bytes()).to_os_string());
    }

    Ok(names)
}

fn open_directory_for_enumeration(directory: &File, path: &Path) -> Result<File, StateError> {
    let held = directory
        .metadata()
        .map_err(|source| io_error(path, source))?;
    // Duplicated directory descriptors share a cursor. Opening dot relative to
    // the held directory creates an independent cursor without reopening a path.
    let dot = CString::new(".").expect("dot is one valid directory component");
    let opened = open_directory_at(directory, &dot)
        .map_err(|error| path_error(path, error, "state directory changed before enumeration"))?;
    let current = opened.metadata().map_err(|source| io_error(path, source))?;
    let same = held.file_type().is_dir()
        && current.file_type().is_dir()
        && held.dev() == current.dev()
        && held.ino() == current.ino()
        && held.uid() == current.uid()
        && held.nlink() == current.nlink()
        && held.mode() == current.mode()
        && held.size() == current.size();
    if !same {
        return Err(unsafe_path(
            path,
            "state directory changed before enumeration",
        ));
    }
    Ok(opened)
}

fn repair_directory_tree(
    root: File,
    root_path: PathBuf,
    owner: u32,
    live_socket_leaf: Option<&OsStr>,
) -> Result<RepairSummary, StateError> {
    let mut summary = RepairSummary::default();
    let mut pending = vec![(root, root_path, true)];

    while let Some((directory, directory_path, is_root)) = pending.pop() {
        for name in read_entry_names(&directory, &directory_path)? {
            let path = directory_path.join(&name);
            let c_name = c_name(&path, &name)?;
            let inspected = stat_at(&directory, &c_name, &path)?;
            let file_type = inspected.st_mode & libc::S_IFMT;
            repair_after_inspect(&path);

            if file_type == libc::S_IFDIR {
                let child = repair_directory_at(&directory, &c_name, &path, owner, &inspected)?;
                summary.directories += 1;
                pending.push((child, path, false));
                continue;
            }
            if file_type == libc::S_IFREG {
                repair_regular_at(&directory, &c_name, &path, owner, &inspected)?;
                summary.regular_files += 1;
                continue;
            }
            if file_type == libc::S_IFSOCK
                && is_root
                && live_socket_leaf.is_some_and(|socket| socket == name)
            {
                if inspected.st_uid != owner {
                    return Err(unsafe_path(&path, "state socket belongs to another user"));
                }
                summary.live_socket_preserved = true;
                continue;
            }
            if file_type == libc::S_IFLNK {
                return Err(unsafe_path(&path, "state tree contains a symbolic link"));
            }
            return Err(unsafe_path(
                &path,
                "state tree contains an unsupported entry",
            ));
        }
    }

    Ok(summary)
}

fn repair_directory_at(
    parent: &File,
    name: &CString,
    path: &Path,
    owner: u32,
    inspected: &libc::stat,
) -> Result<File, StateError> {
    validate_directory_stat(inspected, path, owner)?;
    let handle = match open_directory_repair_handle(parent, name) {
        Ok(handle) => handle,
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && is_secure_restrictive_mode(inspected, DIRECTORY_MODE) =>
        {
            return Err(unsafe_path(
                path,
                "owner-only state directory cannot be inspected",
            ));
        }
        Err(error) => {
            return Err(path_error(
                path,
                error,
                "state directory cannot be safely opened for recursive repair",
            ));
        }
    };
    validate_descriptor_identity(&handle.file, inspected, path)?;
    validate_directory(&handle.file, path, owner)?;
    repair_handle_permissions(&handle, path, DIRECTORY_MODE)?;
    validate_directory(&handle.file, path, owner)?;

    // O_SEARCH and O_PATH descriptors pin the inode but cannot enumerate it.
    let directory = open_directory_at(parent, name).map_err(|error| {
        path_error(
            path,
            error,
            "state directory changed during recursive repair",
        )
    })?;
    validate_descriptor_identity(&directory, inspected, path)?;
    validate_directory(&directory, path, owner)?;
    Ok(directory)
}

fn repair_regular_at(
    parent: &File,
    name: &CString,
    path: &Path,
    owner: u32,
    inspected: &libc::stat,
) -> Result<(), StateError> {
    validate_regular_stat(inspected, path, owner)?;
    let handle = match open_regular_repair_handle(parent, name) {
        Ok(handle) => handle,
        Err(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied
                && is_secure_restrictive_mode(inspected, FILE_MODE) =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(path_error(
                path,
                error,
                "state file cannot be safely opened for recursive repair",
            ));
        }
    };
    validate_descriptor_identity(&handle.file, inspected, path)?;
    validate_regular(&handle.file, path, owner)?;
    repair_handle_permissions(&handle, path, FILE_MODE)?;
    validate_regular(&handle.file, path, owner)
}

struct RepairHandle {
    file: File,
    path_only: bool,
}

fn open_directory_repair_handle(parent: &File, name: &CString) -> std::io::Result<RepairHandle> {
    match open_directory_at(parent, name) {
        Ok(file) => {
            return Ok(RepairHandle {
                file,
                path_only: false,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error),
    }

    #[cfg(target_os = "macos")]
    let flags = libc::O_SEARCH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let file = open_repair_at(parent, name, flags)?;
    Ok(RepairHandle {
        file,
        path_only: cfg!(target_os = "linux"),
    })
}

fn open_regular_repair_handle(parent: &File, name: &CString) -> std::io::Result<RepairHandle> {
    for access in [libc::O_RDONLY, libc::O_WRONLY] {
        let flags = access | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        match open_repair_at(parent, name, flags) {
            Ok(file) => {
                return Ok(RepairHandle {
                    file,
                    path_only: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }

    #[cfg(target_os = "macos")]
    let flags = libc::O_EXEC | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    #[cfg(target_os = "linux")]
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let file = open_repair_at(parent, name, flags)?;
    Ok(RepairHandle {
        file,
        path_only: cfg!(target_os = "linux"),
    })
}

fn open_repair_at(parent: &File, name: &CString, flags: libc::c_int) -> std::io::Result<File> {
    // SAFETY: parent and name are valid. A successful descriptor is owned here.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: fd is a fresh successful openat result owned by this scope.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn repair_handle_permissions(
    handle: &RepairHandle,
    path: &Path,
    mode: u32,
) -> Result<(), StateError> {
    if handle.path_only {
        #[cfg(target_os = "linux")]
        return chmod_pinned_descriptor(&handle.file, path, mode);
        #[cfg(not(target_os = "linux"))]
        unreachable!("path-only repair handles are Linux-only");
    }
    repair_descriptor_permissions(&handle.file, path, mode)
}

fn is_secure_restrictive_mode(metadata: &libc::stat, desired: u32) -> bool {
    let permissions = metadata.st_mode as u32 & 0o7777;
    permissions & !desired == 0
}

#[cfg(test)]
fn repair_after_inspect(path: &Path) {
    REPAIR_AFTER_INSPECT.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action(path);
        }
    });
}

#[cfg(not(test))]
fn repair_after_inspect(_: &Path) {}

#[cfg(test)]
fn inject_repair_after_inspect(action: impl FnOnce(&Path) + 'static) {
    REPAIR_AFTER_INSPECT.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

fn validate_descriptor_identity(
    file: &File,
    inspected: &libc::stat,
    path: &Path,
) -> Result<(), StateError> {
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if metadata.dev() != inspected.st_dev as u64
        || metadata.ino() != inspected.st_ino
        || metadata.mode() & libc::S_IFMT as u32 != inspected.st_mode as u32 & libc::S_IFMT as u32
        || metadata.uid() != inspected.st_uid
        || metadata.nlink() != inspected.st_nlink as u64
    {
        return Err(unsafe_path(
            path,
            "state entry changed during recursive repair",
        ));
    }
    Ok(())
}

fn stat_at(parent: &File, name: &CString, path: &Path) -> Result<libc::stat, StateError> {
    stat_at_optional(parent, name, path)?
        .ok_or_else(|| io_error(path, std::io::Error::from(std::io::ErrorKind::NotFound)))
}

fn stat_at_optional(
    parent: &File,
    name: &CString,
    path: &Path,
) -> Result<Option<libc::stat>, StateError> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to writable storage and fstatat initializes it
    // on success. AT_SYMLINK_NOFOLLOW preserves the link boundary.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(path_error(
            path,
            error,
            "state entry changed during validation",
        ));
    }
    // SAFETY: the successful fstatat call initialized every field.
    Ok(Some(unsafe { metadata.assume_init() }))
}

fn replacement_target_at(
    parent: &File,
    name: &CString,
    path: &Path,
    owner: u32,
) -> Result<Option<libc::stat>, StateError> {
    let target = stat_at_optional(parent, name, path)?;
    if let Some(target) = &target {
        validate_regular_stat(target, path, owner)?;
    }
    Ok(target)
}

fn validate_replacement_target(
    before: Option<&libc::stat>,
    after: Option<&libc::stat>,
    path: &Path,
) -> Result<(), StateError> {
    let unchanged = match (before, after) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            before.st_dev == after.st_dev
                && before.st_ino == after.st_ino
                && before.st_mode & libc::S_IFMT == after.st_mode & libc::S_IFMT
                && before.st_uid == after.st_uid
                && before.st_nlink == after.st_nlink
        }
        _ => false,
    };
    if unchanged {
        Ok(())
    } else {
        Err(unsafe_path(
            path,
            "state replacement target changed during write",
        ))
    }
}

fn chmod_at(
    parent: &File,
    name: &CString,
    path: &Path,
    mode: u32,
    inspected: &libc::stat,
) -> Result<(), StateError> {
    // SAFETY: parent and name are valid. AT_SYMLINK_NOFOLLOW prevents the
    // permission repair from reaching a linked external target.
    let result = unsafe {
        libc::fchmodat(
            parent.as_raw_fd(),
            name.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    #[cfg(target_os = "linux")]
    if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
        return chmod_pinned_at(parent, name, path, mode, inspected);
    }

    let _ = inspected;
    Err(path_error(
        path,
        error,
        "could not repair state entry permissions",
    ))
}

#[cfg(target_os = "linux")]
fn chmod_pinned_at(
    parent: &File,
    name: &CString,
    path: &Path,
    mode: u32,
    inspected: &libc::stat,
) -> Result<(), StateError> {
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: parent and name are valid. O_PATH pins the entry without
    // requiring its current permissions or following a final link.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(path_error(
            path,
            std::io::Error::last_os_error(),
            "state entry changed during permission repair",
        ));
    }
    // SAFETY: fd is a fresh successful openat result owned by this scope.
    let pinned = unsafe { File::from_raw_fd(fd) };
    let pinned_stat = stat_pinned(&pinned, path)?;
    validate_repair_target(inspected, &pinned_stat, path)?;

    chmod_pinned_descriptor(&pinned, path, mode)
}

#[cfg(target_os = "linux")]
fn chmod_pinned_descriptor(file: &File, path: &Path, mode: u32) -> Result<(), StateError> {
    let proc_path = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .expect("descriptor path has no NUL byte");
    // SAFETY: proc_path names the descriptor held above. The descriptor stays
    // open through chmod, so the path cannot be redirected to another inode.
    let result = unsafe { libc::chmod(proc_path.as_ptr(), mode as libc::mode_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(path_error(
            path,
            std::io::Error::last_os_error(),
            "could not repair state entry permissions",
        ))
    }
}

#[cfg(target_os = "linux")]
fn stat_pinned(file: &File, path: &Path) -> Result<libc::stat, StateError> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to writable storage. AT_EMPTY_PATH inspects the
    // held O_PATH descriptor and initializes metadata on success.
    let result = unsafe {
        libc::fstatat(
            file.as_raw_fd(),
            c"".as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result != 0 {
        return Err(path_error(
            path,
            std::io::Error::last_os_error(),
            "could not inspect pinned state entry",
        ));
    }
    // SAFETY: the successful fstatat call initialized every field.
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(any(target_os = "linux", test))]
fn validate_repair_target(
    inspected: &libc::stat,
    pinned: &libc::stat,
    path: &Path,
) -> Result<(), StateError> {
    let inspected_type = inspected.st_mode & libc::S_IFMT;
    let supported_type = inspected_type == libc::S_IFDIR || inspected_type == libc::S_IFREG;
    if !supported_type
        || pinned.st_dev != inspected.st_dev
        || pinned.st_ino != inspected.st_ino
        || pinned.st_mode & libc::S_IFMT != inspected_type
        || pinned.st_uid != inspected.st_uid
        || pinned.st_nlink != inspected.st_nlink
    {
        return Err(unsafe_path(
            path,
            "state entry changed during permission repair",
        ));
    }
    Ok(())
}

fn repair_descriptor_permissions(file: &File, path: &Path, mode: u32) -> Result<(), StateError> {
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|source| io_error(path, source))?;

    #[cfg(target_os = "macos")]
    remove_extended_acl(file, path)?;

    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_extended_acl(file: &File, path: &Path) -> Result<(), StateError> {
    let acl = unsafe { acl_init(0) };
    let acl = std::ptr::NonNull::new(acl)
        .map(ExtendedAcl)
        .ok_or_else(|| io_error(path, std::io::Error::last_os_error()))?;

    // SAFETY: file remains open and acl owns valid empty ACL storage.
    if unsafe { acl_set_fd(file.as_raw_fd(), acl.0.as_ptr()) } != 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
struct ExtendedAcl(std::ptr::NonNull<libc::c_void>);

#[cfg(target_os = "macos")]
impl Drop for ExtendedAcl {
    fn drop(&mut self) {
        // SAFETY: this pointer came from acl_init and is freed exactly once.
        let _ = unsafe { acl_free(self.0.as_ptr()) };
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd(fd: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

fn clone_file(file: &File, path: &Path) -> Result<File, StateError> {
    file.try_clone().map_err(|source| io_error(path, source))
}

#[cfg(target_os = "macos")]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: __error returns this thread's errno storage.
    unsafe { libc::__error() }
}

#[cfg(target_os = "linux")]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: __errno_location returns this thread's errno storage.
    unsafe { libc::__errno_location() }
}

fn clear_errno() {
    // SAFETY: errno_location returns writable thread-local storage.
    unsafe { *errno_location() = 0 };
}

fn current_errno() -> libc::c_int {
    // SAFETY: errno_location returns readable thread-local storage.
    unsafe { *errno_location() }
}

fn validate_inspection_path(path: &Path) -> Result<(), StateError> {
    if path.as_os_str().as_bytes().len() > INSPECTION_PATH_BYTES_LIMIT_MAX {
        return Err(unsafe_path(
            path,
            "inspection path exceeds the hard byte ceiling",
        ));
    }
    Ok(())
}

fn inspection_root_parts(path: &Path) -> Result<Vec<&OsStr>, StateError> {
    validate_inspection_path(path)?;
    let mut parts = Vec::with_capacity(16);
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => {
                if parts.len() >= INSPECTION_PATH_COMPONENT_LIMIT_MAX {
                    return Err(unsafe_path(
                        path,
                        "inspection path exceeds the hard component ceiling",
                    ));
                }
                parts.push(part);
            }
            Component::ParentDir => {
                return Err(unsafe_path(path, "state root contains parent traversal"));
            }
            Component::Prefix(_) => {
                return Err(unsafe_path(path, "state root has an unsupported prefix"));
            }
        }
    }
    if parts.is_empty() {
        return Err(unsafe_path(path, "state root must name a directory"));
    }
    Ok(parts)
}

fn inspection_descendant_parts(path: &Path) -> Result<Vec<&OsStr>, StateError> {
    validate_inspection_path(path)?;
    if path.is_absolute() {
        return Err(unsafe_path(path, "state descendant must be relative"));
    }
    let mut parts = Vec::with_capacity(8);
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if parts.len() >= INSPECTION_PATH_COMPONENT_LIMIT_MAX {
                    return Err(unsafe_path(
                        path,
                        "inspection path exceeds the hard component ceiling",
                    ));
                }
                parts.push(part);
            }
            Component::ParentDir => {
                return Err(unsafe_path(
                    path,
                    "state descendant contains parent traversal",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path(path, "state descendant must be relative"));
            }
            Component::CurDir => {}
        }
    }
    if parts.is_empty() {
        return Err(unsafe_path(path, "state descendant must name an entry"));
    }
    Ok(parts)
}

fn root_parts(path: &Path) -> Result<Vec<&OsStr>, StateError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                return Err(unsafe_path(path, "state root contains parent traversal"));
            }
            Component::Prefix(_) => {
                return Err(unsafe_path(path, "state root has an unsupported prefix"));
            }
        }
    }
    if parts.is_empty() {
        return Err(unsafe_path(path, "state root must name a directory"));
    }
    Ok(parts)
}

fn descendant_parts(path: &Path) -> Result<Vec<&OsStr>, StateError> {
    if path.is_absolute() {
        return Err(unsafe_path(path, "state descendant must be relative"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                return Err(unsafe_path(
                    path,
                    "state descendant contains parent traversal",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path(path, "state descendant must be relative"));
            }
            Component::CurDir => {}
        }
    }
    if parts.is_empty() {
        return Err(unsafe_path(path, "state descendant must name an entry"));
    }
    Ok(parts)
}

fn c_name(path: &Path, name: &OsStr) -> Result<CString, StateError> {
    CString::new(name.as_bytes()).map_err(|_| unsafe_path(path, "state path contains a NUL byte"))
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and reads process credentials.
    unsafe { libc::geteuid() }
}

fn path_error(path: &Path, source: std::io::Error, unsafe_reason: &'static str) -> StateError {
    if matches!(source.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
        unsafe_path(path, unsafe_reason)
    } else {
        io_error(path, source)
    }
}

fn io_error(path: &Path, source: std::io::Error) -> StateError {
    StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn unsafe_path(path: &Path, reason: &'static str) -> StateError {
    StateError::UnsafePath {
        path: path.to_path_buf(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    const CHILD_ROOT: &str = "CYCLOPS_STATE_TEST_ROOT";
    const CHILD_UMASK: &str = "CYCLOPS_STATE_TEST_UMASK";

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn snapshot(path: &Path) -> (Vec<u8>, u32) {
        (fs::read(path).unwrap(), mode(path))
    }

    fn entry_names(path: &Path) -> Vec<OsString> {
        let mut names = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[cfg(target_os = "macos")]
    fn acl_entries(path: &Path) -> Vec<String> {
        let output = Command::new("/bin/ls")
            .arg("-lde")
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not inspect ACL for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .map(|line| line.trim().to_owned())
            .collect()
    }

    fn chown(path: &Path, owner: u32) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is a valid C string and this helper runs only as root.
        assert_eq!(unsafe { libc::chown(path.as_ptr(), owner, u32::MAX) }, 0);
    }

    fn base(temp: &tempfile::TempDir) -> PathBuf {
        fs::canonicalize(temp.path()).unwrap()
    }

    fn root_in(temp: &tempfile::TempDir) -> StateRoot {
        StateRoot::open_or_create(&base(temp).join("state")).unwrap()
    }

    #[test]
    fn read_only_inspection_reports_without_repairing_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let ledger = root_path.join("ledger");
        let journal = ledger.join("main.ndjson");
        fs::create_dir(&root_path).unwrap();
        fs::create_dir(&ledger).unwrap();
        fs::write(&journal, b"private\n").unwrap();
        set_mode(&root_path, 0o755);
        set_mode(&ledger, 0o750);
        set_mode(&journal, 0o640);

        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        assert_eq!(inspector.root().mode, 0o755);
        assert_eq!(
            inspector.root().unsafe_reason,
            Some("state entry permissions grant access beyond the owner")
        );
        assert!(inspector.root().safe_beneath_owner_only_parent());
        let root = inspector.inspect_root(InspectionLimits::default()).unwrap();
        assert_eq!(root.entries.len(), 1);
        assert_eq!(root.entries[0].kind, InspectedKind::Directory);
        assert_eq!(root.entries[0].mode, 0o750);
        let ledger_snapshot = inspector
            .inspect_directory(Path::new("ledger"), InspectionLimits::default())
            .unwrap()
            .expect("ledger directory");
        assert_eq!(ledger_snapshot.entries.len(), 1);
        assert_eq!(ledger_snapshot.entries[0].kind, InspectedKind::RegularFile);
        assert_eq!(ledger_snapshot.entries[0].mode, 0o640);
        assert_eq!(fs::read(&journal).unwrap(), b"private\n");
        assert_eq!(mode(&root_path), 0o755);
        assert_eq!(mode(&ledger), 0o750);
        assert_eq!(mode(&journal), 0o640);
    }

    #[test]
    fn read_only_inspection_treats_a_missing_root_as_absent() {
        let temp = tempfile::tempdir().unwrap();
        let path = base(&temp).join("missing");
        assert!(StateInspector::open_existing(&path).unwrap().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn read_only_inspection_reports_links_without_following_them() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let root_path = base.join("state");
        let external = base.join("external");
        fs::create_dir(&root_path).unwrap();
        fs::write(&external, b"outside\n").unwrap();
        set_mode(&external, 0o644);
        symlink(&external, root_path.join("linked")).unwrap();
        fs::hard_link(&external, root_path.join("shared")).unwrap();

        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let linked = snapshot
            .entries
            .iter()
            .find(|entry| entry.path.ends_with("linked"))
            .unwrap();
        assert_eq!(linked.kind, InspectedKind::Symlink);
        assert_eq!(linked.unsafe_reason, Some("state entry is a symbolic link"));
        let shared = snapshot
            .entries
            .iter()
            .find(|entry| entry.path.ends_with("shared"))
            .unwrap();
        assert_eq!(shared.kind, InspectedKind::RegularFile);
        assert_eq!(
            shared.unsafe_reason,
            Some("state file has multiple hard links")
        );
        assert!(!shared.safe_beneath_owner_only_parent());
        assert_eq!(fs::read(&external).unwrap(), b"outside\n");
        assert_eq!(mode(&external), 0o644);
    }

    #[test]
    fn read_only_inspection_stops_at_explicit_limits() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        fs::create_dir(&root_path).unwrap();
        for name in ["one", "two", "three"] {
            fs::write(root_path.join(name), name).unwrap();
        }
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector
            .inspect_root(InspectionLimits::new(1, 64).unwrap())
            .unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        assert!(snapshot.truncated);
        assert!(snapshot.retained_name_bytes <= 64);

        let no_names = inspector
            .inspect_root(InspectionLimits::new(3, 0).unwrap())
            .unwrap();
        assert!(no_names.entries.is_empty());
        assert!(no_names.truncated);
        assert!(InspectionLimits::new(INSPECTION_ENTRY_LIMIT_MAX + 1, 1).is_err());
        assert!(InspectionLimits::new(1, INSPECTION_NAME_BYTES_LIMIT_MAX + 1).is_err());
    }

    #[test]
    fn read_only_inspection_restarts_each_directory_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        fs::create_dir(&root_path).unwrap();
        for name in ["one", "two", "three"] {
            fs::write(root_path.join(name), name).unwrap();
        }
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");

        let first = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let bounded = inspector
            .inspect_root(InspectionLimits::new(1, 64).unwrap())
            .unwrap();
        let second = inspector.inspect_root(InspectionLimits::default()).unwrap();

        assert!(bounded.truncated);
        assert_eq!(bounded.entries.len(), 1);
        assert_eq!(first, second);
        assert_eq!(second.entries.len(), 3);
    }

    #[test]
    fn read_only_inspection_bounds_lookup_paths_before_opening_them() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        fs::create_dir(&root_path).unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");

        let mut components = PathBuf::new();
        for _ in 0..=INSPECTION_PATH_COMPONENT_LIMIT_MAX {
            components.push("x");
        }
        assert!(inspector
            .inspect_directory(&components, InspectionLimits::default())
            .unwrap_err()
            .to_string()
            .contains("component ceiling"));

        let long = PathBuf::from("x".repeat(INSPECTION_PATH_BYTES_LIMIT_MAX + 1));
        assert!(inspector
            .read_file(&long, 1)
            .unwrap_err()
            .to_string()
            .contains("byte ceiling"));
    }

    #[test]
    fn read_only_file_inspection_is_bounded_and_keeps_mode() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let file = root_path.join("manifest.toml");
        fs::create_dir(&root_path).unwrap();
        fs::write(&file, b"0123456789").unwrap();
        set_mode(&file, 0o640);
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector
            .read_file(Path::new("manifest.toml"), 4)
            .unwrap()
            .expect("manifest");
        assert_eq!(snapshot.bytes, b"0123");
        assert!(snapshot.truncated);
        assert_eq!(snapshot.entry.mode, 0o640);
        assert_eq!(mode(&file), 0o640);
        assert!(inspector
            .read_file(
                Path::new("manifest.toml"),
                INSPECTION_FILE_BYTES_LIMIT_MAX + 1
            )
            .is_err());
    }

    #[test]
    fn read_only_file_inspection_refuses_a_same_inode_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let file = root_path.join("manifest.toml");
        fs::create_dir(&root_path).unwrap();
        fs::write(&file, b"old bytes").unwrap();
        let original = fs::symlink_metadata(&file).unwrap();
        let rewrite = file.clone();
        inject_inspect_after_read(move |_| fs::write(&rewrite, b"new bytes").unwrap());

        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let error = inspector
            .read_file(Path::new("manifest.toml"), 64)
            .unwrap_err();
        let changed = fs::symlink_metadata(&file).unwrap();
        assert_eq!(original.dev(), changed.dev());
        assert_eq!(original.ino(), changed.ino());
        assert!(error
            .to_string()
            .contains("changed during read-only inspection"));
    }

    #[test]
    fn read_only_inspection_refuses_an_entry_replaced_after_readdir() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let entry = root_path.join("entry");
        let displaced = root_path.join("displaced");
        fs::create_dir(&root_path).unwrap();
        fs::write(&entry, b"old").unwrap();
        let replace_entry = entry.clone();
        inject_inspect_after_read(move |_| {
            fs::rename(&replace_entry, &displaced).unwrap();
            fs::write(&replace_entry, b"new").unwrap();
        });

        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let error = inspector
            .inspect_root(InspectionLimits::default())
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed during read-only inspection"));
    }

    #[test]
    fn bound_directory_inspection_refuses_a_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let child = root_path.join("child");
        fs::create_dir(&root_path).unwrap();
        fs::create_dir(&child).unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let root = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let expected = root
            .entries
            .iter()
            .find(|entry| entry.path == child)
            .unwrap();
        fs::rename(&child, root_path.join("old-child")).unwrap();
        fs::create_dir(&child).unwrap();
        let error = inspector
            .inspect_bound_directory(expected, InspectionLimits::default())
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed during read-only inspection"));
    }

    #[test]
    fn read_only_inspection_detects_a_replaced_root_path() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let root_path = base.join("state");
        fs::create_dir(&root_path).unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        fs::rename(&root_path, base.join("old-state")).unwrap();
        fs::create_dir(&root_path).unwrap();
        assert!(!inspector.path_matches_held_root().unwrap());
    }

    #[test]
    fn bound_removal_rejects_symbolic_and_multiply_linked_files() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let root_path = base.join("state");
        let external = base.join("external");
        fs::create_dir(&root_path).unwrap();
        fs::write(&external, b"outside\n").unwrap();
        symlink(&external, root_path.join("linked")).unwrap();
        fs::hard_link(&external, root_path.join("shared")).unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();

        let linked = snapshot
            .entries
            .iter()
            .find(|entry| entry.path.ends_with("linked"))
            .unwrap();
        assert!(inspector.bind_regular_file_for_removal(linked).is_err());
        let shared = snapshot
            .entries
            .iter()
            .find(|entry| entry.path.ends_with("shared"))
            .unwrap();
        assert!(inspector.bind_regular_file_for_removal(shared).is_err());
        assert_eq!(fs::read(&external).unwrap(), b"outside\n");
        assert!(root_path.join("linked").is_symlink());
        assert!(root_path.join("shared").exists());
    }

    #[test]
    fn bound_regular_removal_refuses_an_inode_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let path = root_path.join("entry");
        fs::create_dir(&root_path).unwrap();
        fs::write(&path, b"original").unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let removal = inspector
            .bind_regular_file_for_removal(&snapshot.entries[0])
            .unwrap();

        let displaced = root_path.join("displaced");
        fs::rename(&path, &displaced).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(removal.remove().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(fs::read(&displaced).unwrap(), b"original");
    }

    #[test]
    fn bound_regular_removal_refuses_a_mode_change() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let path = root_path.join("entry");
        fs::create_dir(&root_path).unwrap();
        fs::write(&path, b"original").unwrap();
        set_mode(&path, 0o600);
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let removal = inspector
            .bind_regular_file_for_removal(&snapshot.entries[0])
            .unwrap();

        set_mode(&path, 0o666);
        fs::write(&path, b"modified").unwrap();
        assert!(removal.remove().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"modified");
        assert_eq!(mode(&path), 0o666);
    }

    #[test]
    fn bound_regular_removal_refuses_a_size_change() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let path = root_path.join("entry");
        fs::create_dir(&root_path).unwrap();
        fs::write(&path, b"original").unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let removal = inspector
            .bind_regular_file_for_removal(&snapshot.entries[0])
            .unwrap();

        fs::write(&path, b"changed size").unwrap();
        assert!(removal.remove().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"changed size");
    }

    #[test]
    fn bound_regular_removal_removes_only_the_inspected_file() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let path = root_path.join("entry");
        let keep = root_path.join("keep");
        fs::create_dir(&root_path).unwrap();
        fs::write(&path, b"remove").unwrap();
        fs::write(&keep, b"keep").unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();

        inspector
            .bind_regular_file_for_removal(entry)
            .unwrap()
            .remove()
            .unwrap();

        assert!(!path.exists());
        assert_eq!(fs::read(&keep).unwrap(), b"keep");
    }

    #[test]
    fn bound_regular_removal_retains_a_lockable_descriptor() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let path = root_path.join("entry");
        fs::create_dir(&root_path).unwrap();
        fs::write(&path, b"lease").unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let removal = inspector
            .bind_regular_file_for_removal(&snapshot.entries[0])
            .unwrap();

        assert!(removal.try_lock().unwrap());
        let competing = fs::File::open(&path).unwrap();
        assert_ne!(
            unsafe { libc::flock(competing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "the bound removal must retain the lock"
        );
    }

    #[test]
    fn directory_isolation_preserves_identity_and_never_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let asset = root_path.join("asset");
        let isolated = root_path.join(".isolated");
        fs::create_dir(&root_path).unwrap();
        fs::create_dir(&asset).unwrap();
        fs::write(asset.join("payload"), b"keep").unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let expected = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == asset)
            .unwrap();

        let moved = inspector
            .isolate_direct_child_directory(expected, OsStr::new(".isolated"))
            .unwrap();
        assert_eq!(moved.path, isolated);
        assert_eq!(moved.device, expected.device);
        assert_eq!(moved.inode, expected.inode);
        assert!(!asset.exists());
        assert_eq!(fs::read(isolated.join("payload")).unwrap(), b"keep");

        fs::create_dir(&asset).unwrap();
        fs::create_dir(root_path.join(".occupied")).unwrap();
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let replacement = snapshot
            .entries
            .iter()
            .find(|entry| entry.path == asset)
            .unwrap();
        let error = inspector
            .isolate_direct_child_directory(replacement, OsStr::new(".occupied"))
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert!(asset.is_dir());
        assert!(root_path.join(".occupied").is_dir());
    }

    #[test]
    fn bound_empty_directory_removal_is_explicit_and_non_recursive() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let empty = root_path.join("empty");
        let mode_changed = root_path.join("mode-changed");
        let links_changed = root_path.join("links-changed");
        let nonempty = root_path.join("nonempty");
        fs::create_dir(&root_path).unwrap();
        fs::create_dir(&empty).unwrap();
        fs::create_dir(&mode_changed).unwrap();
        fs::create_dir(&links_changed).unwrap();
        fs::create_dir(&nonempty).unwrap();
        set_mode(&mode_changed, 0o700);
        fs::write(nonempty.join("keep"), b"keep").unwrap();
        let inspector = StateInspector::open_existing(&root_path)
            .unwrap()
            .expect("existing state root");
        let snapshot = inspector.inspect_root(InspectionLimits::default()).unwrap();
        let entry = |name: &str| {
            snapshot
                .entries
                .iter()
                .find(|entry| entry.path.ends_with(name))
                .unwrap()
        };

        inspector
            .bind_empty_directory_for_removal(entry("empty"))
            .unwrap()
            .remove()
            .unwrap();
        assert!(!empty.exists());

        let removal = inspector
            .bind_empty_directory_for_removal(entry("mode-changed"))
            .unwrap();
        set_mode(&mode_changed, 0o755);
        let error = removal.remove().unwrap_err();
        assert!(error.to_string().contains("changed before removal"));
        assert!(mode_changed.is_dir());
        assert_eq!(mode(&mode_changed), 0o755);

        let removal = inspector
            .bind_empty_directory_for_removal(entry("links-changed"))
            .unwrap();
        fs::create_dir(links_changed.join("late")).unwrap();
        let error = removal.remove().unwrap_err();
        assert!(error.to_string().contains("changed before removal"));
        assert!(links_changed.join("late").is_dir());

        assert!(inspector
            .bind_empty_directory_for_removal(entry("nonempty"))
            .is_err());
        assert_eq!(fs::read(nonempty.join("keep")).unwrap(), b"keep");
    }

    #[test]
    fn bounded_append_keeps_the_latest_complete_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let path = root.path().join("cyclopsd.log");
        let mut file = root.open_append(Path::new("cyclopsd.log")).unwrap();

        file.append_bounded(b"first failure context\n", 64, 32)
            .unwrap();
        file.append_bounded(b"second failure context\n", 64, 32)
            .unwrap();
        file.append_bounded(b"latest failure context\n", 64, 32)
            .unwrap();

        let bytes = fs::read(path).unwrap();
        assert!(bytes.len() <= 64);
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("first failure context"));
        assert!(text.contains("second failure context"));
        assert!(text.contains("latest failure context"));
    }

    #[test]
    fn bounded_append_caps_one_oversized_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let path = root.path().join("cyclopsd.log");
        let mut file = root.open_append(Path::new("cyclopsd.log")).unwrap();
        let mut record = vec![b'x'; 96];
        record[64..].copy_from_slice(b"latest-32-bytes-stay-in-the-log!");

        file.append_bounded(&record, 32, 16).unwrap();

        assert_eq!(fs::read(path).unwrap(), &record[64..]);
    }

    #[test]
    fn state_file_try_lock_never_waits_for_an_existing_writer() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let first = root.open_append(Path::new("hook-errors.log")).unwrap();
        let second = root.open_append(Path::new("hook-errors.log")).unwrap();

        first.lock().unwrap();
        let inherited = first.file.try_clone().unwrap();
        assert!(!second.try_lock().unwrap());
        drop(first);
        assert!(second.try_lock().unwrap());
        drop(inherited);
    }

    #[test]
    fn bounded_try_append_never_waits_for_an_existing_writer() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let path = root.path().join("cyclopsd.log");
        let first = root.open_append(Path::new("cyclopsd.log")).unwrap();
        let mut second = root.open_append(Path::new("cyclopsd.log")).unwrap();

        first.lock().unwrap();
        assert!(!second.try_append_bounded(b"blocked\n", 64, 32).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"");
        drop(first);
        assert!(second.try_append_bounded(b"written\n", 64, 32).unwrap());
        assert_eq!(fs::read(path).unwrap(), b"written\n");
    }

    #[test]
    fn umask_child() {
        let Some(root) = std::env::var_os(CHILD_ROOT).map(PathBuf::from) else {
            return;
        };
        let mask =
            u32::from_str_radix(&std::env::var(CHILD_UMASK).expect("child umask"), 8).unwrap();
        // SAFETY: this dedicated child owns the process-wide umask until exit.
        unsafe { libc::umask(mask as libc::mode_t) };
        let state = StateRoot::open_or_create(&root).unwrap();
        let mut file = state.open_append(Path::new("ledger/main.ndjson")).unwrap();
        file.write_all(b"line\n").unwrap();
        state
            .replace_file(Path::new("config/settings.json"), b"atomic\n")
            .unwrap();
        assert_eq!(
            state
                .create_file_once(Path::new("identity/workspace-id"), b"stable\n")
                .unwrap(),
            CreateFileOutcome::Created
        );
    }

    #[test]
    fn creation_is_owner_only_under_permissive_and_restrictive_umasks() {
        for mask in ["000", "777"] {
            let temp = tempfile::tempdir().unwrap();
            let root = base(&temp).join(format!("state-{mask}"));
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::umask_child", "--nocapture"])
                .env(CHILD_ROOT, &root)
                .env(CHILD_UMASK, mask)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "umask {mask} child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(mode(&root), DIRECTORY_MODE);
            assert_eq!(mode(&root.join("ledger")), DIRECTORY_MODE);
            assert_eq!(mode(&root.join("ledger/main.ndjson")), FILE_MODE);
            assert_eq!(mode(&root.join("config")), DIRECTORY_MODE);
            assert_eq!(mode(&root.join("config/settings.json")), FILE_MODE);
            assert_eq!(mode(&root.join("identity")), DIRECTORY_MODE);
            assert_eq!(mode(&root.join("identity/workspace-id")), FILE_MODE);
            assert_eq!(
                fs::read(root.join("config/settings.json")).unwrap(),
                b"atomic\n"
            );
            assert_eq!(
                fs::read(root.join("identity/workspace-id")).unwrap(),
                b"stable\n"
            );
        }
    }

    #[test]
    fn concurrent_create_once_publishers_agree_on_one_complete_file() {
        const CREATORS: usize = 8;

        let temp = tempfile::tempdir().unwrap();
        let root = std::sync::Arc::new(root_in(&temp));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(CREATORS));
        let mut threads = Vec::new();
        for index in 0..CREATORS {
            let root = std::sync::Arc::clone(&root);
            let barrier = std::sync::Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                let contents = format!("workspace-{index}\n").into_bytes();
                barrier.wait();
                let outcome = root
                    .create_file_once(Path::new("workspace-id"), &contents)
                    .unwrap();
                (outcome, contents)
            }));
        }

        let mut created = Vec::new();
        let mut existing = 0;
        for thread in threads {
            let (outcome, contents) = thread.join().unwrap();
            match outcome {
                CreateFileOutcome::Created => created.push(contents),
                CreateFileOutcome::AlreadyExists => existing += 1,
            }
        }

        assert_eq!(created.len(), 1);
        assert_eq!(existing, CREATORS - 1);
        assert_eq!(
            fs::read(root.path().join("workspace-id")).unwrap(),
            created[0]
        );
        let metadata = fs::metadata(root.path().join("workspace-id")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, FILE_MODE);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(
            entry_names(root.path()),
            vec![OsString::from("workspace-id")]
        );
    }

    #[test]
    fn transient_file_is_owner_only_and_removed_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let file = root
            .create_transient_file(Path::new("spool/payload"), b"secret")
            .unwrap();
        let path = file.path().to_path_buf();

        assert_eq!(fs::read(&path).unwrap(), b"secret");
        assert_eq!(mode(&path), FILE_MODE);
        assert_eq!(mode(path.parent().unwrap()), DIRECTORY_MODE);
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn transient_cleanup_refuses_a_replaced_entry() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let file = root
            .create_transient_file(Path::new("spool/payload"), b"secret")
            .unwrap();
        let path = file.path().to_path_buf();
        let displaced = path.with_extension("displaced");
        fs::rename(&path, &displaced).unwrap();
        fs::write(&path, b"replacement").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let error = file.remove().unwrap_err();
        assert!(error.to_string().contains("changed before cleanup"));
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert_eq!(mode(&path), 0o640);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inherited_acls_are_removed_before_state_bytes_are_read() {
        const INHERITED_ACL: &str =
            "everyone allow list,add_file,search,add_subdirectory,file_inherit,directory_inherit";

        let temp = tempfile::tempdir().unwrap();
        let parent = base(&temp);
        let status = Command::new("/bin/chmod")
            .args(["+a", INHERITED_ACL])
            .arg(&parent)
            .status()
            .unwrap();
        assert!(status.success());
        let parent_acl = acl_entries(&parent);
        assert!(!parent_acl.is_empty());

        let root_path = parent.join("state");
        let ledger_path = root_path.join("ledger");
        let file_path = ledger_path.join("main.ndjson");
        fs::create_dir(&root_path).unwrap();
        fs::create_dir(&ledger_path).unwrap();
        fs::write(&file_path, b"private state\n").unwrap();
        for path in [&root_path, &ledger_path, &file_path] {
            assert!(!acl_entries(path).is_empty());
        }

        let root = StateRoot::open_or_create(&root_path).unwrap();
        let mut file = root
            .open_read(Path::new("ledger/main.ndjson"))
            .unwrap()
            .unwrap();
        assert_eq!(acl_entries(&parent), parent_acl);
        for path in [&root_path, &ledger_path, &file_path] {
            assert!(
                acl_entries(path).is_empty(),
                "{} kept an ACL",
                path.display()
            );
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"private state\n");
    }

    #[test]
    fn permission_repair_requires_the_inspected_identity_and_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let path = base.join("entry");
        fs::write(&path, b"state").unwrap();
        let parent = File::open(&base).unwrap();
        let name = c_name(&path, OsStr::new("entry")).unwrap();
        let inspected = stat_at(&parent, &name, &path).unwrap();

        validate_repair_target(&inspected, &inspected, &path).unwrap();

        let mut wrong_device = stat_at(&parent, &name, &path).unwrap();
        wrong_device.st_dev = wrong_device.st_dev.wrapping_add(1);
        assert!(validate_repair_target(&inspected, &wrong_device, &path).is_err());

        let mut wrong_inode = stat_at(&parent, &name, &path).unwrap();
        wrong_inode.st_ino = wrong_inode.st_ino.wrapping_add(1);
        assert!(validate_repair_target(&inspected, &wrong_inode, &path).is_err());

        let mut wrong_type = stat_at(&parent, &name, &path).unwrap();
        wrong_type.st_mode = (wrong_type.st_mode & !libc::S_IFMT) | libc::S_IFDIR;
        assert!(validate_repair_target(&inspected, &wrong_type, &path).is_err());

        let mut wrong_owner = stat_at(&parent, &name, &path).unwrap();
        wrong_owner.st_uid = wrong_owner.st_uid.wrapping_add(1);
        assert!(validate_repair_target(&inspected, &wrong_owner, &path).is_err());

        let mut wrong_links = stat_at(&parent, &name, &path).unwrap();
        wrong_links.st_nlink = wrong_links.st_nlink.wrapping_add(1);
        assert!(validate_repair_target(&inspected, &wrong_links, &path).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pinned_fallback_repairs_inspected_directory_and_file() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let parent = File::open(&base).unwrap();

        let directory_path = base.join("directory");
        fs::create_dir(&directory_path).unwrap();
        set_mode(&directory_path, 0o000);
        let directory_name = c_name(&directory_path, OsStr::new("directory")).unwrap();
        let directory_stat = stat_at(&parent, &directory_name, &directory_path).unwrap();
        chmod_pinned_at(
            &parent,
            &directory_name,
            &directory_path,
            DIRECTORY_MODE,
            &directory_stat,
        )
        .unwrap();
        assert_eq!(mode(&directory_path), DIRECTORY_MODE);

        let file_path = base.join("file");
        fs::write(&file_path, b"state").unwrap();
        set_mode(&file_path, 0o000);
        let file_name = c_name(&file_path, OsStr::new("file")).unwrap();
        let file_stat = stat_at(&parent, &file_name, &file_path).unwrap();
        chmod_pinned_at(&parent, &file_name, &file_path, FILE_MODE, &file_stat).unwrap();
        assert_eq!(mode(&file_path), FILE_MODE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pinned_fallback_refuses_a_replaced_inode_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let parent = File::open(&base).unwrap();
        let path = base.join("entry");
        fs::write(&path, b"original").unwrap();
        set_mode(&path, 0o000);
        let name = c_name(&path, OsStr::new("entry")).unwrap();
        let inspected = stat_at(&parent, &name, &path).unwrap();

        fs::rename(&path, base.join("original")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        set_mode(&path, 0o640);
        let replacement = snapshot(&path);

        assert!(chmod_pinned_at(&parent, &name, &path, FILE_MODE, &inspected).is_err());
        assert_eq!(snapshot(&path), replacement);
        assert_eq!(mode(&base.join("original")), 0o000);
    }

    #[test]
    fn permissive_existing_paths_are_repaired_before_read() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");
        let root = StateRoot::open_or_create(&root_path).unwrap();
        fs::create_dir(root.path().join("ledger")).unwrap();
        let leaf = root.path().join("ledger/main.ndjson");
        fs::write(&leaf, b"keep").unwrap();
        set_mode(&leaf, 0o666);
        set_mode(&root.path().join("ledger"), 0o777);
        drop(root);
        set_mode(&root_path, 0o777);

        let root = StateRoot::open_or_create(&root_path).unwrap();
        let _ = root
            .open_read(Path::new("ledger/main.ndjson"))
            .unwrap()
            .unwrap();
        assert_eq!(mode(&root_path), DIRECTORY_MODE);
        assert_eq!(mode(&root_path.join("ledger")), DIRECTORY_MODE);
        assert_eq!(mode(&leaf), FILE_MODE);
        assert_eq!(fs::read(leaf).unwrap(), b"keep");
    }

    #[test]
    fn recursive_repair_returns_one_aggregate_summary() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let ledger = root.path().join("ledger");
        let archive = ledger.join("archive");
        fs::create_dir(&ledger).unwrap();
        fs::create_dir(&archive).unwrap();
        fs::write(ledger.join("main.ndjson"), b"main\n").unwrap();
        fs::write(archive.join("old.ndjson"), b"old\n").unwrap();
        set_mode(root.path(), 0o777);
        set_mode(&ledger, 0o777);
        set_mode(&archive, 0o100);
        set_mode(&ledger.join("main.ndjson"), 0o666);
        set_mode(&archive.join("old.ndjson"), 0o200);

        let summary = root.repair_descendant_permissions(None).unwrap();

        assert_eq!(
            summary,
            RepairSummary {
                directories: 2,
                regular_files: 2,
                live_socket_preserved: false,
            }
        );
        assert_eq!(mode(root.path()), DIRECTORY_MODE);
        assert_eq!(mode(&ledger), DIRECTORY_MODE);
        assert_eq!(mode(&archive), DIRECTORY_MODE);
        assert_eq!(mode(&ledger.join("main.ndjson")), FILE_MODE);
        assert_eq!(mode(&archive.join("old.ndjson")), FILE_MODE);
    }

    #[test]
    fn recursive_repair_preserves_only_the_named_live_socket() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let socket_path = root.path().join("sock");
        let socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let before = fs::symlink_metadata(&socket_path).unwrap();
        set_mode(root.path(), 0o777);

        let summary = root
            .repair_descendant_permissions(Some(OsStr::new("sock")))
            .unwrap();
        let after = fs::symlink_metadata(&socket_path).unwrap();

        assert!(summary.live_socket_preserved);
        assert_eq!(mode(root.path()), DIRECTORY_MODE);
        assert_eq!(after.dev(), before.dev());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        drop(socket);
    }

    #[test]
    fn recursive_repair_refuses_an_unexpected_socket_without_chmod() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let socket_path = root.path().join("unexpected.sock");
        let socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let before = fs::symlink_metadata(&socket_path).unwrap();

        assert!(root
            .repair_descendant_permissions(Some(OsStr::new("sock")))
            .is_err());

        let after = fs::symlink_metadata(&socket_path).unwrap();
        assert_eq!(after.dev(), before.dev());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.mode(), before.mode());
        drop(socket);
    }

    #[test]
    fn recursive_repair_refuses_a_symlink_without_mutating_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let external = base(&temp).join("external");
        fs::create_dir(&external).unwrap();
        set_mode(&external, 0o750);
        let external_file = external.join("keep.ndjson");
        fs::write(&external_file, b"keep\n").unwrap();
        set_mode(&external_file, 0o640);
        let before = snapshot(&external_file);
        let linked = root.path().join("linked");
        symlink(&external, &linked).unwrap();

        assert!(root.repair_descendant_permissions(None).is_err());

        assert_eq!(mode(&external), 0o750);
        assert_eq!(snapshot(&external_file), before);
        assert!(fs::symlink_metadata(linked)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn recursive_repair_refuses_a_hard_link_without_mutating_shared_inode() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let external = base(&temp).join("external.ndjson");
        fs::write(&external, b"keep\n").unwrap();
        set_mode(&external, 0o640);
        let before = snapshot(&external);
        fs::hard_link(&external, root.path().join("linked.ndjson")).unwrap();

        assert!(root.repair_descendant_permissions(None).is_err());

        assert_eq!(snapshot(&external), before);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recursive_repair_refuses_an_opaque_directory_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let directory = root.path().join("opaque");
        fs::create_dir(&directory).unwrap();
        let child = directory.join("hidden.ndjson");
        fs::write(&child, b"hidden\n").unwrap();
        set_mode(&child, 0o666);
        let directory_file = File::open(&directory).unwrap();
        let child_file = File::open(&child).unwrap();
        set_mode(&directory, 0o000);

        assert!(root.repair_descendant_permissions(None).is_err());

        assert_eq!(
            directory_file.metadata().unwrap().permissions().mode() & 0o777,
            0o000
        );
        assert_eq!(
            child_file.metadata().unwrap().permissions().mode() & 0o777,
            0o666
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recursive_repair_uses_a_pinned_handle_for_an_opaque_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let directory = root.path().join("opaque");
        fs::create_dir(&directory).unwrap();
        let child = directory.join("hidden.ndjson");
        fs::write(&child, b"hidden\n").unwrap();
        set_mode(&child, 0o666);
        set_mode(&directory, 0o000);

        let summary = root.repair_descendant_permissions(None).unwrap();

        assert_eq!(summary.directories, 1);
        assert_eq!(summary.regular_files, 1);
        assert_eq!(mode(&directory), DIRECTORY_MODE);
        assert_eq!(mode(&child), FILE_MODE);
    }

    #[test]
    fn recursive_repair_does_not_chmod_a_swapped_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let leaf = root.path().join("state.ndjson");
        fs::write(&leaf, b"inside\n").unwrap();
        set_mode(&leaf, 0o666);
        let external = base(&temp).join("external.ndjson");
        fs::write(&external, b"outside\n").unwrap();
        set_mode(&external, 0o640);
        let external_file = File::open(&external).unwrap();
        let replacement = external.clone();
        let destination = leaf.clone();
        inject_repair_after_inspect(move |inspected| {
            assert_eq!(inspected, destination);
            fs::rename(replacement, destination).unwrap();
        });

        assert!(root.repair_descendant_permissions(None).is_err());

        assert_eq!(
            external_file.metadata().unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read(leaf).unwrap(), b"outside\n");
    }

    #[test]
    fn recursive_repair_does_not_chmod_a_swapped_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let leaf = root.path().join("ledger");
        fs::create_dir(&leaf).unwrap();
        set_mode(&leaf, 0o777);
        let external = base(&temp).join("external");
        fs::create_dir(&external).unwrap();
        set_mode(&external, 0o750);
        let external_directory = File::open(&external).unwrap();
        let replacement = external.clone();
        let destination = leaf.clone();
        inject_repair_after_inspect(move |inspected| {
            assert_eq!(inspected, destination);
            fs::rename(replacement, destination).unwrap();
        });

        assert!(root.repair_descendant_permissions(None).is_err());

        assert_eq!(
            external_directory.metadata().unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[test]
    fn atomic_replace_creates_and_replaces_an_owner_only_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let descendant = Path::new("config/settings.json");
        let path = root.path().join(descendant);

        root.replace_file(descendant, b"first\n").unwrap();
        set_mode(&path, 0o640);
        let first_inode = fs::metadata(&path).unwrap().ino();
        let mut first_file = File::open(&path).unwrap();
        root.replace_file(descendant, b"second\n").unwrap();

        let metadata = fs::metadata(&path).unwrap();
        let mut first_contents = Vec::new();
        first_file.read_to_end(&mut first_contents).unwrap();
        assert_ne!(metadata.ino(), first_inode);
        assert_eq!(first_contents, b"first\n");
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(mode(&path), FILE_MODE);
        assert_eq!(fs::read(path).unwrap(), b"second\n");
    }

    #[test]
    fn post_rename_sync_failure_reports_a_visible_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let descendant = Path::new("settings.json");
        let path = root.path().join(descendant);
        root.replace_file(descendant, b"first\n").unwrap();
        let entries = entry_names(path.parent().unwrap());

        inject_next_directory_sync_failure();
        let error = root.replace_file(descendant, b"second\n").unwrap_err();

        match error {
            StateError::ReplacementDurabilityUnknown {
                path: error_path, ..
            } => assert_eq!(error_path, path),
            other => panic!("unexpected replacement error: {other}"),
        }
        assert_eq!(fs::read(&path).unwrap(), b"second\n");
        assert_eq!(entry_names(path.parent().unwrap()), entries);
    }

    #[test]
    fn pre_rename_sync_failure_preserves_the_target_and_reports_io() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let descendant = Path::new("config/settings.json");
        let path = root.path().join(descendant);
        root.replace_file(descendant, b"first\n").unwrap();
        let entries = entry_names(path.parent().unwrap());

        inject_next_directory_sync_failure();
        let error = root.replace_file(descendant, b"second\n").unwrap_err();

        assert!(matches!(error, StateError::Io { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"first\n");
        assert_eq!(entry_names(path.parent().unwrap()), entries);
    }

    #[test]
    fn root_creation_fails_before_return_when_parent_sync_fails() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = base(&temp).join("state");

        inject_next_directory_sync_failure();
        let result = StateRoot::open_or_create(&root_path);

        assert!(matches!(result, Err(StateError::Io { .. })));
        assert_eq!(mode(&root_path), DIRECTORY_MODE);
    }

    #[test]
    fn intermediate_creation_fails_before_leaf_open_when_parent_sync_fails() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);

        inject_next_directory_sync_failure();
        let result = root.open_append(Path::new("ledger/main.ndjson"));

        assert!(matches!(result, Err(StateError::Io { .. })));
        assert_eq!(mode(&root.path().join("ledger")), DIRECTORY_MODE);
        assert!(!root.path().join("ledger/main.ndjson").exists());
    }

    #[test]
    fn file_creation_returns_no_handle_when_parent_sync_fails() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let path = root.path().join("main.ndjson");

        inject_next_directory_sync_failure();
        let result = root.open_append(Path::new("main.ndjson"));

        assert!(matches!(result, Err(StateError::Io { .. })));
        assert_eq!(mode(&path), FILE_MODE);
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
    }

    #[test]
    fn atomic_replace_refuses_a_symlinked_component_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let external = base(&temp).join("external");
        fs::create_dir(&external).unwrap();
        set_mode(&external, 0o750);
        let existing = external.join("keep.json");
        fs::write(&existing, b"keep\n").unwrap();
        set_mode(&existing, 0o640);
        let before = snapshot(&existing);
        symlink(&external, root.path().join("config")).unwrap();

        assert!(root
            .replace_file(Path::new("config/settings.json"), b"replace\n")
            .is_err());
        assert_eq!(snapshot(&existing), before);
        assert_eq!(mode(&external), 0o750);
        assert!(!external.join("settings.json").exists());
    }

    #[test]
    fn atomic_replace_refuses_a_dangling_leaf_link_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let directory = root.path().join("config");
        fs::create_dir(&directory).unwrap();
        let external = base(&temp).join("outside/missing.json");
        let leaf = directory.join("settings.json");
        symlink(&external, &leaf).unwrap();
        let before = entry_names(&directory);

        assert!(root
            .replace_file(Path::new("config/settings.json"), b"replace\n")
            .is_err());
        assert_eq!(entry_names(&directory), before);
        assert!(fs::symlink_metadata(&leaf)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!external.exists());
    }

    #[test]
    fn atomic_replace_refuses_a_hard_link_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let directory = root.path().join("config");
        fs::create_dir(&directory).unwrap();
        let external = base(&temp).join("outside.json");
        fs::write(&external, b"keep\n").unwrap();
        set_mode(&external, 0o640);
        let leaf = directory.join("settings.json");
        fs::hard_link(&external, &leaf).unwrap();
        let before = snapshot(&external);
        let before_entries = entry_names(&directory);
        let before_links = fs::metadata(&external).unwrap().nlink();

        assert!(root
            .replace_file(Path::new("config/settings.json"), b"replace\n")
            .is_err());
        assert_eq!(snapshot(&external), before);
        assert_eq!(fs::metadata(&external).unwrap().nlink(), before_links);
        assert_eq!(entry_names(&directory), before_entries);
    }

    #[test]
    fn failed_atomic_replace_removes_its_temporary_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let directory = root.path().join("config");
        fs::create_dir(&directory).unwrap();
        let leaf = directory.join("settings.json");
        fs::create_dir(&leaf).unwrap();
        set_mode(&leaf, 0o750);
        let before = entry_names(&directory);

        assert!(root
            .replace_file(Path::new("config/settings.json"), b"replace\n")
            .is_err());
        assert_eq!(entry_names(&directory), before);
        assert_eq!(mode(&leaf), 0o750);
    }

    #[test]
    fn regular_file_names_include_only_direct_regular_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let mut file = root.open_append(Path::new("ledger/main.ndjson")).unwrap();
        file.write_all(b"line\n").unwrap();

        let ledger = root.path().join("ledger");
        fs::create_dir(ledger.join("nested")).unwrap();
        fs::write(ledger.join("nested/child.ndjson"), b"nested\n").unwrap();
        let external = base(&temp).join("external.ndjson");
        fs::write(&external, b"external\n").unwrap();
        symlink(&external, ledger.join("linked.ndjson")).unwrap();

        let mut names = root.regular_file_names(Path::new("ledger")).unwrap();
        names.sort();
        assert_eq!(names, vec![OsString::from("main.ndjson")]);
    }

    #[test]
    fn directory_enumeration_refuses_unsafe_components_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let external = base(&temp).join("external-ledger");
        fs::create_dir(&external).unwrap();
        set_mode(&external, 0o750);
        let external_file = external.join("main.ndjson");
        fs::write(&external_file, b"external\n").unwrap();
        set_mode(&external_file, 0o640);
        let before = snapshot(&external_file);
        symlink(&external, root.path().join("ledger")).unwrap();

        assert!(root.regular_file_names(Path::new("ledger")).is_err());
        assert_eq!(snapshot(&external_file), before);
        assert_eq!(mode(&external), 0o750);
        assert!(fs::symlink_metadata(root.path().join("ledger"))
            .unwrap()
            .file_type()
            .is_symlink());

        assert!(root.regular_file_names(Path::new("missing")).is_err());

        let file_component = root.path().join("not-a-directory");
        fs::write(&file_component, b"keep").unwrap();
        set_mode(&file_component, 0o640);
        let before = snapshot(&file_component);
        assert!(root
            .regular_file_names(Path::new("not-a-directory"))
            .is_err());
        assert_eq!(snapshot(&file_component), before);
    }

    #[test]
    fn descendants_refuse_absolute_and_parent_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let external = base(&temp).join("outside");
        fs::write(&external, b"keep").unwrap();
        set_mode(&external, 0o640);
        let before = snapshot(&external);

        assert!(root.open_append(&external).is_err());
        assert!(root.open_append(Path::new("ledger/../../outside")).is_err());
        assert_eq!(snapshot(&external), before);
    }

    #[test]
    fn symlinked_root_is_refused_without_mutating_target() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        set_mode(&target, 0o750);
        let link = base.join("state");
        symlink(&target, &link).unwrap();

        assert!(StateRoot::open_or_create(&link).is_err());
        assert_eq!(mode(&target), 0o750);
    }

    #[test]
    fn symlinked_root_component_is_refused_without_mutating_target() {
        let temp = tempfile::tempdir().unwrap();
        let base = base(&temp);
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        set_mode(&target, 0o750);
        let link = base.join("linked-parent");
        symlink(&target, &link).unwrap();

        assert!(StateRoot::open_or_create(&link.join("state")).is_err());
        assert_eq!(mode(&target), 0o750);
        assert!(!target.join("state").exists());
    }

    #[test]
    fn symlinked_component_is_refused_without_mutating_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let target = base(&temp).join("target");
        fs::create_dir(&target).unwrap();
        set_mode(&target, 0o750);
        symlink(&target, root.path().join("ledger")).unwrap();

        assert!(root.open_append(Path::new("ledger/main.ndjson")).is_err());
        assert_eq!(mode(&target), 0o750);
        assert!(!target.join("main.ndjson").exists());
    }

    #[test]
    fn symlinked_leaf_is_refused_without_mutating_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        fs::create_dir(root.path().join("ledger")).unwrap();
        let target = base(&temp).join("target");
        fs::write(&target, b"keep").unwrap();
        set_mode(&target, 0o640);
        let before = snapshot(&target);
        symlink(&target, root.path().join("ledger/main.ndjson")).unwrap();

        assert!(root.open_append(Path::new("ledger/main.ndjson")).is_err());
        assert_eq!(snapshot(&target), before);
    }

    #[test]
    fn dangling_leaf_link_is_refused_without_creating_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        fs::create_dir(root.path().join("ledger")).unwrap();
        let target = base(&temp).join("outside/missing.ndjson");
        let leaf = root.path().join("ledger/main.ndjson");
        symlink(&target, &leaf).unwrap();

        assert!(root.open_append(Path::new("ledger/main.ndjson")).is_err());
        assert!(fs::symlink_metadata(leaf).unwrap().file_type().is_symlink());
        assert!(!target.exists());
    }

    #[test]
    fn hard_link_is_refused_without_mutating_shared_inode() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        fs::create_dir(root.path().join("ledger")).unwrap();
        let target = base(&temp).join("target");
        fs::write(&target, b"keep").unwrap();
        set_mode(&target, 0o640);
        let before = snapshot(&target);
        fs::hard_link(&target, root.path().join("ledger/main.ndjson")).unwrap();

        assert!(root.open_append(Path::new("ledger/main.ndjson")).is_err());
        assert_eq!(snapshot(&target), before);
    }

    #[test]
    fn non_regular_leaf_is_refused_without_changing_its_mode() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        fs::create_dir(root.path().join("ledger")).unwrap();
        let leaf = root.path().join("ledger/main.ndjson");
        fs::create_dir(&leaf).unwrap();
        set_mode(&leaf, 0o750);

        assert!(root.open_append(Path::new("ledger/main.ndjson")).is_err());
        assert_eq!(mode(&leaf), 0o750);
    }

    #[test]
    fn mutable_non_sticky_root_parent_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let parent = base(&temp).join("shared");
        fs::create_dir(&parent).unwrap();
        set_mode(&parent, 0o777);

        assert!(StateRoot::open_or_create(&parent.join("state")).is_err());
        assert_eq!(mode(&parent), 0o777);
        assert!(!parent.join("state").exists());
    }

    #[test]
    fn existing_unsearchable_directory_is_revalidated_before_repair() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        let ledger = root.path().join("ledger");
        fs::create_dir(&ledger).unwrap();
        set_mode(&ledger, 0o000);

        let _ = root.open_append(Path::new("ledger/main.ndjson")).unwrap();
        assert_eq!(mode(&ledger), DIRECTORY_MODE);
    }

    #[test]
    fn existing_unreadable_file_is_revalidated_before_repair() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        fs::create_dir(root.path().join("ledger")).unwrap();
        let leaf = root.path().join("ledger/main.ndjson");
        fs::write(&leaf, b"keep").unwrap();
        set_mode(&leaf, 0o000);

        let mut file = root
            .open_read(Path::new("ledger/main.ndjson"))
            .unwrap()
            .unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"keep");
        assert_eq!(mode(&leaf), FILE_MODE);
    }

    #[test]
    fn missing_read_does_not_create_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        assert!(root
            .open_read(Path::new("missing/main.ndjson"))
            .unwrap()
            .is_none());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn wrong_owner_root_and_leaf_are_refused_when_the_test_can_create_them() {
        if effective_uid() != 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = root_in(&temp);
        fs::create_dir(root.path().join("ledger")).unwrap();
        let leaf = root.path().join("ledger/main.ndjson");
        fs::write(&leaf, b"keep").unwrap();
        set_mode(&leaf, 0o640);
        chown(&leaf, 1);
        let before = snapshot(&leaf);
        assert!(root.open_append(Path::new("ledger/main.ndjson")).is_err());
        assert_eq!(snapshot(&leaf), before);

        let other = base(&temp).join("foreign-state");
        fs::create_dir(&other).unwrap();
        set_mode(&other, 0o750);
        chown(&other, 1);
        assert!(StateRoot::open_or_create(&other).is_err());
        assert_eq!(mode(&other), 0o750);
    }

    #[test]
    fn relocated_nested_root_uses_the_same_descriptor_rules() {
        let temp = tempfile::tempdir().unwrap();
        let relocation = base(&temp).join("relocated");
        fs::create_dir(&relocation).unwrap();
        let root_path = relocation.join("state");
        let root = StateRoot::open_or_create(&root_path).unwrap();
        let mut file = root.open_append(Path::new("ledger/main.ndjson")).unwrap();
        file.write_all(b"ok\n").unwrap();
        assert_eq!(
            fs::read(root_path.join("ledger/main.ndjson")).unwrap(),
            b"ok\n"
        );
    }
}
