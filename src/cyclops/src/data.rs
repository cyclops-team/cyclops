//! Read-only durable-record inventory and export.
//!
//! The command deliberately covers the append-only workspace and session
//! journals, not every mutable preference or managed installation artifact.
//! It gives a person an exact, portable copy of the records Cyclops retains
//! today without repairing, compacting, or changing the source state.

use std::ffi::{CString, OsStr};
#[cfg(test)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
#[cfg(test)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use cyclops_state::{InspectedEntry, InspectedKind, InspectionLimits, StateInspector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const INVENTORY_SCHEMA: u32 = 1;
const EXPORT_SCHEMA: u32 = 1;
const EXPORT_RECORDS_DIRECTORY: &str = "records";
const EXPORT_MANIFEST: &str = "manifest.json";
const EXPORT_INCOMPLETE_MARKER: &str = "INCOMPLETE";
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

const OWNERSHIP: &str =
    "Cyclops owns these append-only journals below its state home; workspace journals can contain message bodies.";
const RETENTION: &str =
    "Cyclops preserves these records until an explicit confirmed forget operation. Inventory and export never delete, truncate, rewrite, or repair them.";
const SCOPE: &str =
    "workspace and session NDJSON journals only; preferences, setup files, and managed installation assets are outside this export.";
const SNAPSHOT: &str =
    "each selected file matched its identity and modification evidence at copy time and at the final recheck; the live daemon is not paused, so this is not an atomic daemon snapshot.";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RecordCategory {
    WorkspaceJournals,
    SessionJournals,
}

impl RecordCategory {
    fn name(self) -> &'static str {
        match self {
            Self::WorkspaceJournals => "workspace_journals",
            Self::SessionJournals => "session_journals",
        }
    }

    fn words(self) -> &'static str {
        match self {
            Self::WorkspaceJournals => "workspace journals",
            Self::SessionJournals => "session journals",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecordFile {
    category: RecordCategory,
    pub(crate) relative: PathBuf,
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) evidence: RecordEvidence,
}

/// Stable evidence captured before an export reads one live journal.
///
/// Device and inode detect a replacement. Size, modification, and change
/// times detect an in-place rewrite that preserves the path and byte count.
/// This is not a daemon epoch: a live writer can still change a record after
/// the final recheck and before the command returns.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordEvidence {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) links: u64,
    pub(crate) bytes: u64,
    pub(crate) modified_seconds: i64,
    pub(crate) modified_nanoseconds: i64,
    pub(crate) changed_seconds: i64,
    pub(crate) changed_nanoseconds: i64,
}

impl RecordEvidence {
    pub(crate) fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.permissions().mode() & 0o7777,
            uid: metadata.uid(),
            links: metadata.nlink(),
            bytes: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    pub(crate) fn matches_entry(&self, entry: &InspectedEntry) -> bool {
        self.device == entry.device
            && self.inode == entry.inode
            && self.mode == entry.mode
            && self.uid == entry.uid
            && self.links == entry.links
            && self.bytes == entry.size
    }

    pub(crate) fn matches_metadata(&self, metadata: &fs::Metadata) -> bool {
        self == &Self::from_metadata(metadata)
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct RecordGroup {
    records: Vec<RecordFile>,
    bytes: u64,
    truncated: bool,
}

impl RecordGroup {
    fn add(&mut self, record: RecordFile) -> Result<(), String> {
        self.bytes = self
            .bytes
            .checked_add(record.bytes)
            .ok_or_else(|| "durable record inventory byte count overflowed".to_string())?;
        self.records.push(record);
        Ok(())
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RecordInventory {
    workspace: RecordGroup,
    session: RecordGroup,
}

impl RecordInventory {
    pub(crate) fn complete(&self) -> bool {
        !self.workspace.truncated && !self.session.truncated
    }

    pub(crate) fn records(&self) -> impl Iterator<Item = &RecordFile> {
        self.workspace.records.iter().chain(&self.session.records)
    }

    pub(crate) fn files(&self) -> usize {
        self.records().count()
    }

    pub(crate) fn bytes(&self) -> Result<u64, String> {
        self.workspace
            .bytes
            .checked_add(self.session.bytes)
            .ok_or_else(|| "durable record export byte count overflowed".to_string())
    }
}

pub(crate) struct RecordSource {
    pub(crate) home: PathBuf,
    pub(crate) inspector: Option<StateInspector>,
    pub(crate) inventory: RecordInventory,
}

#[derive(Serialize)]
struct ExportManifest {
    schema: u32,
    kind: &'static str,
    format: &'static str,
    scope: &'static str,
    ownership: &'static str,
    retention: &'static str,
    snapshot: &'static str,
    records: Vec<ExportedRecord>,
}

#[derive(Serialize)]
struct ExportedRecord {
    category: &'static str,
    path: String,
    bytes: u64,
}

#[derive(Debug)]
struct ExportResult {
    destination: PathBuf,
    files: usize,
    bytes: u64,
}

#[derive(Debug)]
enum ExportFailure {
    Incomplete(String),
    CompletionUncertain(String),
}

impl ExportFailure {
    #[cfg(test)]
    fn message(&self) -> &str {
        match self {
            Self::Incomplete(message) | Self::CompletionUncertain(message) => message,
        }
    }
}

/// A directory reached through an already-held descriptor.
///
/// Export writes only through these descriptors. A later pathname replacement
/// cannot redirect a journal copy into another directory.
#[derive(Debug)]
struct HeldDirectory {
    file: File,
    path: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
}

impl DirectoryIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.permissions().mode() & 0o7777,
            uid: metadata.uid(),
        }
    }

    fn matches_directory_stat(&self, metadata: &libc::stat) -> bool {
        metadata.st_mode & libc::S_IFMT == libc::S_IFDIR
            && stat_device(metadata) == Some(self.device)
            && stat_inode(metadata) == self.inode
            && stat_mode(metadata).is_some_and(|mode| mode & 0o7777 == self.mode)
            && metadata.st_uid == self.uid
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RegularFileIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    links: u64,
    bytes: u64,
}

impl RegularFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.permissions().mode() & 0o7777,
            uid: metadata.uid(),
            links: metadata.nlink(),
            bytes: metadata.len(),
        }
    }

    fn matches_regular_stat(&self, metadata: &libc::stat) -> bool {
        metadata.st_mode & libc::S_IFMT == libc::S_IFREG
            && stat_device(metadata) == Some(self.device)
            && stat_inode(metadata) == self.inode
            && stat_mode(metadata).is_some_and(|mode| mode & 0o7777 == self.mode)
            && metadata.st_uid == self.uid
            && stat_links(metadata) == Some(self.links)
            && u64::try_from(metadata.st_size).ok() == Some(self.bytes)
    }
}

/// One new export directory and the parent descriptor that names it.
///
/// The immediate parent must be owned by the operator and not writable by
/// other users. That makes the held-descriptor guarantee meaningful for the
/// private export while still allowing ordinary user-owned directories such as
/// a home-folder export directory.
#[derive(Debug)]
struct ExportDestination {
    display: PathBuf,
    parent_path: PathBuf,
    parent: File,
    parent_identity: DirectoryIdentity,
    name: CString,
    directory: HeldDirectory,
    identity: DirectoryIdentity,
    incomplete_marker: RegularFileIdentity,
}

struct ExportTarget {
    parent: HeldDirectory,
    name: CString,
    path: PathBuf,
}

/// Print an inventory without contacting the daemon or changing durable state.
pub(crate) fn run_inventory(json_output: bool) -> i32 {
    match inspect_records(&cyclops_proto::cyclops_home()) {
        Ok(source) => {
            let inventory = source.inventory;
            if json_output {
                println!("{}", inventory_json(&inventory));
            } else {
                print!("{}", inventory_plain(&inventory));
            }
            i32::from(!inventory.complete())
        }
        Err(error) => {
            print_error(json_output, "data_inventory_failed", &error);
            1
        }
    }
}

/// Copy durable records into one new user-selected directory.
pub(crate) fn run_export(json_output: bool, destination: &Path) -> i32 {
    match export_at(&cyclops_proto::cyclops_home(), destination) {
        Ok(result) => {
            if json_output {
                println!(
                    "{}",
                    json!({
                        "schema": EXPORT_SCHEMA,
                        "exported": true,
                        "destination": result.destination.display().to_string(),
                        "files": result.files,
                        "bytes": result.bytes,
                        "source_mutated_by_export": false,
                        "source_final_recheck": "matched",
                        "snapshot": SNAPSHOT,
                    })
                );
            } else {
                println!(
                    "exported {} durable record files ({} bytes) to {}",
                    result.files,
                    result.bytes,
                    result.destination.display()
                );
                println!("  Cyclops did not change source records");
                println!("  source remained live; the final recheck matched before completion");
                println!("  raw NDJSON is under records/; manifest.json describes the export");
            }
            0
        }
        Err(error) => {
            print_error(json_output, "data_export_failed", &error);
            1
        }
    }
}

fn print_error(json_output: bool, code: &str, error: &str) {
    if json_output {
        println!(
            "{}",
            json!({"schema": INVENTORY_SCHEMA, "code": code, "message": error})
        );
    } else {
        eprintln!("{code}: {error}");
    }
}

pub(crate) fn inspect_records(home: &Path) -> Result<RecordSource, String> {
    let Some(inspector) = StateInspector::open_existing(home)
        .map_err(|error| format!("inspect Cyclops state at {}: {error}", home.display()))?
    else {
        return Ok(RecordSource {
            home: home.to_path_buf(),
            inspector: None,
            inventory: RecordInventory::default(),
        });
    };

    require_owner_only_root(inspector.root())?;

    let workspace = inspect_workspace_journals(&inspector)?;
    let session = inspect_session_journals(&inspector)?;
    if !inspector
        .path_matches_held_root()
        .map_err(|error| format!("recheck Cyclops state root: {error}"))?
    {
        return Err("Cyclops state root changed during durable-record inspection".into());
    }

    Ok(RecordSource {
        home: home.to_path_buf(),
        inspector: Some(inspector),
        inventory: RecordInventory { workspace, session },
    })
}

fn inspect_workspace_journals(inspector: &StateInspector) -> Result<RecordGroup, String> {
    let Some(workspaces) = inspector
        .inspect_directory(Path::new("workspaces"), inspection_limits())
        .map_err(|error| format!("inspect workspace journals: {error}"))?
    else {
        return Ok(RecordGroup::default());
    };
    require_safe(&workspaces.directory, "workspace journal directory")?;

    let mut group = RecordGroup {
        truncated: workspaces.truncated,
        ..RecordGroup::default()
    };
    for workspace in workspaces.entries {
        if workspace.kind == InspectedKind::Symlink {
            return Err(format!(
                "workspace journal directory {} is unsafe: symbolic links are not accepted",
                workspace.path.display()
            ));
        }
        if workspace.kind != InspectedKind::Directory {
            continue;
        }
        require_safe(&workspace, "workspace journal directory")?;
        let contents = inspector
            .inspect_bound_directory(&workspace, inspection_limits())
            .map_err(|error| format!("inspect workspace journal directory: {error}"))?;
        require_safe(&contents.directory, "workspace journal directory")?;
        group.truncated |= contents.truncated;

        for entry in contents.entries {
            if entry.path.file_name() == Some(OsStr::new("messages.ndjson")) {
                group.add(record_file(
                    inspector,
                    entry,
                    RecordCategory::WorkspaceJournals,
                )?)?;
            }
        }
    }
    group
        .records
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(group)
}

fn inspect_session_journals(inspector: &StateInspector) -> Result<RecordGroup, String> {
    let Some(ledger) = inspector
        .inspect_directory(Path::new("ledger"), inspection_limits())
        .map_err(|error| format!("inspect session journals: {error}"))?
    else {
        return Ok(RecordGroup::default());
    };
    require_safe(&ledger.directory, "session journal directory")?;

    let mut group = RecordGroup {
        truncated: ledger.truncated,
        ..RecordGroup::default()
    };
    for entry in ledger.entries {
        if entry.path.extension() == Some(OsStr::new("ndjson")) {
            group.add(record_file(
                inspector,
                entry,
                RecordCategory::SessionJournals,
            )?)?;
        }
    }
    group
        .records
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(group)
}

pub(crate) fn inspection_limits() -> InspectionLimits {
    InspectionLimits::new(
        cyclops_state::INSPECTION_ENTRY_LIMIT_MAX,
        cyclops_state::INSPECTION_NAME_BYTES_LIMIT_MAX,
    )
    .expect("the state inspector's hard limits are valid inspection limits")
}

fn record_file(
    inspector: &StateInspector,
    entry: InspectedEntry,
    category: RecordCategory,
) -> Result<RecordFile, String> {
    if entry.kind != InspectedKind::RegularFile {
        return Err(format!(
            "{} must be an owner-only regular file, not {:?}",
            entry.path.display(),
            entry.kind
        ));
    }
    require_safe(&entry, "durable record")?;
    let relative = entry.path.strip_prefix(inspector.path()).map_err(|_| {
        format!(
            "durable record {} is outside the held Cyclops state root",
            entry.path.display()
        )
    })?;
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "durable record {} has an unsafe relative path",
            entry.path.display()
        ));
    }
    let Some(path) = relative.to_str() else {
        return Err(format!(
            "durable record {} has a non-UTF-8 path that this export cannot describe",
            entry.path.display()
        ));
    };
    let relative = relative.to_path_buf();
    let (confirmed, evidence) = inspector
        .inspect_file_with(&relative, u64::MAX, |file| {
            file.metadata()
                .map(|metadata| RecordEvidence::from_metadata(&metadata))
        })
        .map_err(|error| format!("inspect durable record {}: {error}", entry.path.display()))?
        .ok_or_else(|| {
            format!(
                "durable record {} disappeared during inspection",
                entry.path.display()
            )
        })?;
    if !evidence.matches_entry(&entry) || !evidence.matches_entry(&confirmed) {
        return Err(format!(
            "durable record {} changed during inspection",
            entry.path.display()
        ));
    }
    require_safe(&confirmed, "durable record")?;

    Ok(RecordFile {
        category,
        relative,
        path: path.to_string(),
        bytes: evidence.bytes,
        evidence,
    })
}

pub(crate) fn require_safe(entry: &InspectedEntry, what: &str) -> Result<(), String> {
    if entry.safe_beneath_owner_only_parent() {
        return Ok(());
    }
    Err(format!(
        "{} {} is unsafe: {}",
        what,
        entry.path.display(),
        entry
            .unsafe_reason
            .unwrap_or("state entry cannot be certified")
    ))
}

/// The held state root is the trust boundary for every descendant inspection.
/// A broad descendant can be safe under this private root, but a broad root
/// itself lets another local user replace or add records during the export.
fn require_owner_only_root(root: &InspectedEntry) -> Result<(), String> {
    if root.safe() {
        return Ok(());
    }
    Err(format!(
        "Cyclops state root {} is unsafe: {}",
        root.path.display(),
        root.unsafe_reason
            .unwrap_or("state root cannot be certified")
    ))
}

fn inventory_json(inventory: &RecordInventory) -> Value {
    json!({
        "schema": INVENTORY_SCHEMA,
        "scope": SCOPE,
        "complete": inventory.complete(),
        "ownership": OWNERSHIP,
        "retention": RETENTION,
        "categories": [
            group_json(RecordCategory::WorkspaceJournals, &inventory.workspace),
            group_json(RecordCategory::SessionJournals, &inventory.session),
        ],
        "export": {
            "command": "cyclops data export --to <new-directory>",
            "format": "raw NDJSON files plus manifest.json",
            "snapshot": SNAPSHOT,
        },
        "forget": {
            "command": "cyclops data forget --all",
            "scope": "the exact journal inventory shown by its preview",
            "requires": "a daemon stopped before preview and kept stopped through exact confirmation",
        },
    })
}

fn group_json(category: RecordCategory, group: &RecordGroup) -> Value {
    json!({
        "category": category.name(),
        "files": group.records.len(),
        "bytes": group.bytes,
        "truncated": group.truncated,
        "records": group.records.iter().map(|record| json!({
            "path": record.path,
            "bytes": record.bytes,
        })).collect::<Vec<_>>(),
    })
}

fn inventory_plain(inventory: &RecordInventory) -> String {
    let mut output = String::from("durable record inventory\n");
    for (category, group) in [
        (RecordCategory::WorkspaceJournals, &inventory.workspace),
        (RecordCategory::SessionJournals, &inventory.session),
    ] {
        output.push_str(&format!(
            "  {}  {} files · {} bytes{}\n",
            category.words(),
            group.records.len(),
            group.bytes,
            if group.truncated {
                " · incomplete"
            } else {
                ""
            }
        ));
    }
    output.push_str(&format!("  scope      {SCOPE}\n"));
    output.push_str(&format!("  ownership  {OWNERSHIP}\n"));
    output.push_str(&format!("  retention  {RETENTION}\n"));
    if !inventory.complete() {
        output.push_str("  export     refused until a complete inventory is available\n");
    } else {
        output.push_str("  export     cyclops data export --to <new-directory>\n");
        output.push_str("  forget     cyclops data forget --all\n");
    }
    output
}

fn export_at(home: &Path, destination: &Path) -> Result<ExportResult, String> {
    let source = inspect_records(home)?;
    if !source.inventory.complete() {
        return Err(
            "durable record inventory is incomplete; export is refused rather than omit records"
                .into(),
        );
    }

    let destination = ExportDestination::create(destination)?;

    let result = export_into(&source, &destination);
    match result {
        Ok(result) => Ok(result),
        Err(ExportFailure::Incomplete(error)) => Err(format!(
            "{error}; the incomplete marker remains in the held export directory and Cyclops did not change source records"
        )),
        Err(ExportFailure::CompletionUncertain(error)) => Err(format!(
            "{error}; Cyclops did not change source records, but completion is uncertain. Inspect the held export directory before relying on it"
        )),
    }
}

fn export_into(
    source: &RecordSource,
    destination: &ExportDestination,
) -> Result<ExportResult, ExportFailure> {
    let records_root = destination
        .directory
        .open_or_create_private_child(OsStr::new(EXPORT_RECORDS_DIRECTORY))
        .map_err(ExportFailure::Incomplete)?;

    for record in source.inventory.records() {
        let target =
            export_target(&records_root, &record.relative).map_err(ExportFailure::Incomplete)?;
        copy_record(source.inspector.as_ref(), record, &target)
            .map_err(ExportFailure::Incomplete)?;
    }

    ensure_source_still_matches_inventory(source).map_err(ExportFailure::Incomplete)?;

    let manifest = ExportManifest {
        schema: EXPORT_SCHEMA,
        kind: "cyclops_durable_record_export",
        format: "raw_ndjson",
        scope: SCOPE,
        ownership: OWNERSHIP,
        retention: RETENTION,
        snapshot: SNAPSHOT,
        records: source
            .inventory
            .records()
            .map(|record| ExportedRecord {
                category: record.category.name(),
                path: record.path.clone(),
                bytes: record.bytes,
            })
            .collect(),
    };
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        ExportFailure::Incomplete(format!("serialize durable record export manifest: {error}"))
    })?;
    manifest_bytes.push(b'\n');
    destination
        .write_private_file(EXPORT_MANIFEST, &manifest_bytes)
        .map_err(ExportFailure::Incomplete)?;
    destination.complete()?;

    Ok(ExportResult {
        destination: destination.display.clone(),
        files: source.inventory.files(),
        bytes: source
            .inventory
            .bytes()
            .map_err(ExportFailure::Incomplete)?,
    })
}

/// A live daemon can append between file copies. Refuse a completed marker if
/// the selected source set no longer matches the initial inventory.
fn ensure_source_still_matches_inventory(source: &RecordSource) -> Result<(), String> {
    if let Some(inspector) = &source.inspector {
        if !inspector
            .path_matches_held_root()
            .map_err(|error| format!("recheck Cyclops state root: {error}"))?
        {
            return Err("Cyclops state root changed during durable-record export".into());
        }
    }

    let current = inspect_records(&source.home)?;
    if current.inspector.is_some() != source.inspector.is_some()
        || current.inventory != source.inventory
    {
        return Err(
            "durable records changed during export; retry with a new destination to obtain a complete snapshot"
                .into(),
        );
    }
    Ok(())
}

fn export_target(records_root: &HeldDirectory, relative: &Path) -> Result<ExportTarget, String> {
    let Some(parent) = relative.parent() else {
        return Err("durable record export path has no parent directory".into());
    };
    let mut destination = records_root.try_clone()?;
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err("durable record export path contains a non-normal component".into());
        };
        destination = destination.open_or_create_private_child(name)?;
    }
    let Some(file_name) = relative.file_name() else {
        return Err("durable record export path has no file name".into());
    };
    let name = component_name(file_name, &destination.path)
        .map_err(|error| format!("name export record {}: {error}", relative.display()))?;
    Ok(ExportTarget {
        path: destination.path.join(file_name),
        parent: destination,
        name,
    })
}

fn copy_record(
    inspector: Option<&StateInspector>,
    record: &RecordFile,
    target: &ExportTarget,
) -> Result<(), String> {
    let Some(inspector) = inspector else {
        return Err(format!(
            "record {} disappeared before the export could open its state root",
            record.path
        ));
    };
    let mut output = target
        .parent
        .create_private_file(&target.name)
        .map_err(|error| format!("create export record {}: {error}", target.path.display()))?;
    let copied = inspector
        .inspect_file_with(&record.relative, u64::MAX, |input| {
            let metadata = input.metadata()?;
            if !record.evidence.matches_metadata(&metadata) {
                return Err(io::Error::other("record changed before export"));
            }
            let expected_bytes = metadata.len();
            let copied = io::copy(&mut input.take(expected_bytes), &mut output)?;
            if copied != expected_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "record ended before its inspected size",
                ));
            }
            output.sync_all()?;
            Ok(copied)
        })
        .map_err(|error| format!("copy durable record {}: {error}", record.path))?
        .ok_or_else(|| format!("durable record {} disappeared during export", record.path))?;
    if !record.evidence.matches_entry(&copied.0) || copied.1 != record.bytes {
        return Err(format!(
            "durable record {} changed during export",
            record.path
        ));
    }
    target.parent.file.sync_all().map_err(|error| {
        format!(
            "sync export directory {}: {error}",
            target.parent.path.display()
        )
    })?;
    Ok(())
}

impl HeldDirectory {
    fn try_clone(&self) -> Result<Self, String> {
        let file = self.file.try_clone().map_err(|error| {
            format!(
                "clone held export directory {}: {error}",
                self.path.display()
            )
        })?;
        Ok(Self {
            file,
            path: self.path.clone(),
        })
    }

    fn open_or_create_private_child(&self, name: &OsStr) -> Result<Self, String> {
        let child_path = self.path.join(name);
        let name = component_name(name, &child_path)
            .map_err(|error| format!("name export directory {}: {error}", child_path.display()))?;
        let created = match create_directory_at(&self.file, &name, PRIVATE_DIRECTORY_MODE) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => {
                return Err(format!(
                    "create export directory {}: {error}",
                    child_path.display()
                ));
            }
        };
        let file = open_directory_at(&self.file, &name)
            .map_err(|error| format!("open export directory {}: {error}", child_path.display()))?;
        if created {
            set_file_mode(&file, PRIVATE_DIRECTORY_MODE).map_err(|error| {
                format!("protect export directory {}: {error}", child_path.display())
            })?;
        }
        require_private_directory(&file, &child_path)?;
        if created {
            file.sync_all().map_err(|error| {
                format!("sync export directory {}: {error}", child_path.display())
            })?;
            self.file.sync_all().map_err(|error| {
                format!(
                    "sync export directory parent {}: {error}",
                    self.path.display()
                )
            })?;
        }
        Ok(Self {
            file,
            path: child_path,
        })
    }

    fn create_private_file(&self, name: &CString) -> io::Result<File> {
        let file = create_private_file_at(&self.file, name, PRIVATE_FILE_MODE)?;
        set_file_mode(&file, PRIVATE_FILE_MODE)?;
        Ok(file)
    }
}

impl ExportDestination {
    /// Establish a new, private export directory before copying any source
    /// record. The incomplete marker file and its directory are both synced
    /// before this function returns, so every later failure has a durable
    /// non-completion state to leave behind.
    fn create(display: &Path) -> Result<Self, String> {
        let (parent_path, name) = export_parent_and_name(display)?;
        let parent = open_directory_path(&parent_path).map_err(|error| {
            format!(
                "open export destination parent {}: {error}",
                parent_path.display()
            )
        })?;
        let parent_identity = require_export_parent(&parent, &parent_path)?;
        create_directory_at(&parent, &name, PRIVATE_DIRECTORY_MODE).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                format!(
                    "export destination {} already exists; choose a new directory",
                    display.display()
                )
            } else {
                format!("create export destination {}: {error}", display.display())
            }
        })?;
        let directory_file = open_directory_at(&parent, &name)
            .map_err(|error| format!("open export destination {}: {error}", display.display()))?;
        set_file_mode(&directory_file, PRIVATE_DIRECTORY_MODE).map_err(|error| {
            format!("protect export destination {}: {error}", display.display())
        })?;
        let identity = require_private_directory(&directory_file, display)?;
        let directory = HeldDirectory {
            file: directory_file,
            path: display.to_path_buf(),
        };
        directory
            .file
            .sync_all()
            .map_err(|error| format!("sync export destination {}: {error}", display.display()))?;
        parent.sync_all().map_err(|error| {
            format!(
                "sync export destination parent {}: {error}",
                parent_path.display()
            )
        })?;

        let incomplete_marker =
            establish_incomplete_marker(&directory, &parent, display, &parent_path)?;
        Ok(Self {
            display: display.to_path_buf(),
            parent_path,
            parent,
            parent_identity,
            name,
            directory,
            identity,
            incomplete_marker,
        })
    }

    fn write_private_file(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.display.join(name);
        let name = component_name(OsStr::new(name), &path)
            .map_err(|error| format!("name export file {}: {error}", path.display()))?;
        let mut file = self
            .directory
            .create_private_file(&name)
            .map_err(|error| format!("create export file {}: {error}", path.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("write export file {}: {error}", path.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync export file {}: {error}", path.display()))?;
        require_private_file(&file, &path)?;
        self.directory
            .file
            .sync_all()
            .map_err(|error| format!("sync export directory {}: {error}", self.display.display()))
    }

    /// Commit a complete export by removing the already-durable marker through
    /// the held directory descriptor and syncing that directory. A failed
    /// post-removal sync has an explicitly uncertain outcome: the command must
    /// not claim either a completed export or a retained marker.
    fn complete(&self) -> Result<(), ExportFailure> {
        self.directory.file.sync_all().map_err(|error| {
            ExportFailure::Incomplete(format!(
                "sync completed export directory {}: {error}",
                self.display.display()
            ))
        })?;
        self.parent.sync_all().map_err(|error| {
            ExportFailure::Incomplete(format!(
                "sync completed export parent {}: {error}",
                self.parent_path.display()
            ))
        })?;
        if !self
            .path_still_names_held_directory()
            .map_err(ExportFailure::Incomplete)?
        {
            return Err(ExportFailure::Incomplete(format!(
                "export destination {} changed during export; its incomplete marker was not removed",
                self.display.display()
            )));
        }

        let marker_path = self.display.join(EXPORT_INCOMPLETE_MARKER);
        let marker_name = component_name(OsStr::new(EXPORT_INCOMPLETE_MARKER), &marker_path)
            .map_err(|error| {
                ExportFailure::Incomplete(format!(
                    "name incomplete export marker {}: {error}",
                    marker_path.display()
                ))
            })?;
        let current_marker = stat_at_optional(&self.directory.file, &marker_name)
            .map_err(|error| {
                ExportFailure::Incomplete(format!(
                    "inspect incomplete export marker {}: {error}",
                    marker_path.display()
                ))
            })?
            .ok_or_else(|| {
                ExportFailure::Incomplete(format!(
                    "incomplete export marker {} disappeared",
                    marker_path.display()
                ))
            })?;
        if !self.incomplete_marker.matches_regular_stat(&current_marker) {
            return Err(ExportFailure::Incomplete(format!(
                "incomplete export marker {} changed; it was not removed",
                marker_path.display()
            )));
        }
        remove_file_at(&self.directory.file, &marker_name).map_err(|error| {
            ExportFailure::Incomplete(format!(
                "remove incomplete export marker {}: {error}",
                marker_path.display()
            ))
        })?;
        sync_marker_removal(&self.directory.file).map_err(|error| {
            ExportFailure::CompletionUncertain(format!(
                "sync completed export directory {}: {error}",
                self.display.display()
            ))
        })?;
        completion_after_marker_removal();
        if !self
            .path_still_names_held_directory()
            .map_err(ExportFailure::CompletionUncertain)?
        {
            return Err(ExportFailure::CompletionUncertain(format!(
                "export destination {} changed after marker removal; the completed export is held at the original directory, not the replacement path",
                self.display.display()
            )));
        }
        Ok(())
    }

    fn path_still_names_held_directory(&self) -> Result<bool, String> {
        let current_parent = open_directory_path(&self.parent_path).map_err(|error| {
            format!(
                "recheck export destination parent {}: {error}",
                self.parent_path.display()
            )
        })?;
        let parent_metadata = current_parent.metadata().map_err(|error| {
            format!(
                "recheck export destination parent {}: {error}",
                self.parent_path.display()
            )
        })?;
        if DirectoryIdentity::from_metadata(&parent_metadata) != self.parent_identity {
            return Ok(false);
        }
        let Some(current) = stat_at_optional(&current_parent, &self.name).map_err(|error| {
            format!(
                "recheck export destination {}: {error}",
                self.display.display()
            )
        })?
        else {
            return Ok(false);
        };
        Ok(self.identity.matches_directory_stat(&current))
    }
}

fn establish_incomplete_marker(
    directory: &HeldDirectory,
    parent: &File,
    display: &Path,
    parent_path: &Path,
) -> Result<RegularFileIdentity, String> {
    let marker_path = display.join(EXPORT_INCOMPLETE_MARKER);
    let marker_name =
        component_name(OsStr::new(EXPORT_INCOMPLETE_MARKER), &marker_path).map_err(|error| {
            format!(
                "name incomplete export marker {}: {error}",
                marker_path.display()
            )
        })?;
    let mut marker = directory
        .create_private_file(&marker_name)
        .map_err(|error| {
            format!(
                "create incomplete export marker {}: {error}",
                marker_path.display()
            )
        })?;
    marker
        .write_all(
            b"This export is incomplete. Do not rely on it until this marker is gone. Source records were not modified.\n",
        )
        .map_err(|error| format!("write incomplete export marker {}: {error}", marker_path.display()))?;
    marker.sync_all().map_err(|error| {
        format!(
            "sync incomplete export marker {}: {error}",
            marker_path.display()
        )
    })?;
    let identity = require_private_file(&marker, &marker_path)?;
    directory.file.sync_all().map_err(|error| {
        format!(
            "sync incomplete export directory {}: {error}",
            display.display()
        )
    })?;
    parent.sync_all().map_err(|error| {
        format!(
            "sync export destination parent {}: {error}",
            parent_path.display()
        )
    })?;
    Ok(identity)
}

fn export_parent_and_name(path: &Path) -> Result<(PathBuf, CString), String> {
    let Some(Component::Normal(name)) = path.components().next_back() else {
        return Err(format!(
            "export destination {} must name one new directory",
            path.display()
        ));
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_path = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve current directory for export: {error}"))?
            .join(parent)
    };
    let name = component_name(name, path)
        .map_err(|error| format!("name export destination {}: {error}", path.display()))?;
    Ok((parent_path, name))
}

fn require_export_parent(file: &File, path: &Path) -> Result<DirectoryIdentity, String> {
    let metadata = file.metadata().map_err(|error| {
        format!(
            "inspect export destination parent {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "export destination parent {} is not a directory",
            path.display()
        ));
    }
    if metadata.uid() != effective_uid() {
        return Err(format!(
            "export destination parent {} belongs to another user",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(format!(
            "export destination parent {} is writable by another user; choose a parent controlled only by you",
            path.display()
        ));
    }
    Ok(DirectoryIdentity::from_metadata(&metadata))
}

fn require_private_directory(file: &File, path: &Path) -> Result<DirectoryIdentity, String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect export directory {}: {error}", path.display()))?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() {
        return Err(format!(
            "export directory {} is not one private owner directory",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE {
        return Err(format!(
            "export directory {} is not mode 0700",
            path.display()
        ));
    }
    Ok(DirectoryIdentity::from_metadata(&metadata))
}

fn require_private_file(file: &File, path: &Path) -> Result<RegularFileIdentity, String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect export file {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.nlink() != 1 {
        return Err(format!(
            "export file {} is not one private owner regular file",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE {
        return Err(format!("export file {} is not mode 0600", path.display()));
    }
    Ok(RegularFileIdentity::from_metadata(&metadata))
}

fn open_directory_path(path: &Path) -> io::Result<File> {
    let mut directory = File::open("/")?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} contains an unresolved parent component", path.display()),
                ));
            }
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a Unix export path", path.display()),
                ));
            }
        };
        let name = component_name(name, path)?;
        directory = open_directory_at(&directory, &name)?;
    }
    Ok(directory)
}

fn component_name(name: &OsStr, path: &Path) -> io::Result<CString> {
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not one normal path component", path.display()),
        ));
    }
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} contains a NUL byte", path.display()),
        )
    })
}

fn create_directory_at(parent: &File, name: &CString, mode: u32) -> io::Result<()> {
    let mode = libc::mode_t::try_from(mode)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory mode"))?;
    // SAFETY: `parent` is held, `name` is a valid C string, and `mode` is a Unix mode.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn open_directory_at(parent: &File, name: &CString) -> io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    // SAFETY: `parent` is held and `name` is a valid C string.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh successful `openat` result owned by this function.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn create_private_file_at(parent: &File, name: &CString, mode: u32) -> io::Result<File> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    // SAFETY: `parent` is held, `name` is a valid C string, and `mode` is a Unix mode.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh successful `openat` result owned by this function.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    let mode = libc::mode_t::try_from(mode)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file mode"))?;
    // SAFETY: `file` is a valid held descriptor and `mode` is a Unix mode.
    let result = unsafe { libc::fchmod(file.as_raw_fd(), mode) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn stat_at_optional(parent: &File, name: &CString) -> io::Result<Option<libc::stat>> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `parent` and `name` are valid, and `metadata` is writable storage.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: the successful `fstatat` initialized `metadata`.
        return Ok(Some(unsafe { metadata.assume_init() }));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(error)
    }
}

fn remove_file_at(parent: &File, name: &CString) -> io::Result<()> {
    // SAFETY: `parent` is held and `name` is a valid C string.
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn sync_marker_removal(directory: &File) -> io::Result<()> {
    #[cfg(test)]
    {
        if COMPLETION_MARKER_SYNC_FAILURE.with(|slot| slot.replace(false)) {
            return Err(io::Error::other(
                "injected marker-removal directory sync failure",
            ));
        }
    }
    directory.sync_all()
}

fn completion_after_marker_removal() {
    #[cfg(test)]
    COMPLETION_AFTER_MARKER_REMOVAL.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` reads process credentials and has no pointer arguments.
    unsafe { libc::geteuid() }
}

/// Widen `ino_t` on 32-bit Linux while staying a no-op on 64-bit targets.
#[allow(clippy::unnecessary_cast)]
fn stat_inode(metadata: &libc::stat) -> u64 {
    metadata.st_ino as u64
}

/// Normalize the platform-specific `dev_t` without treating a negative value
/// as the identity we captured from `std::fs::Metadata`.
///
/// Linux already exposes this as `u64`; Darwin uses a narrower signed type.
/// Keeping the checked conversion preserves the same fail-closed comparison on
/// both platforms.
#[allow(
    clippy::unnecessary_fallible_conversions,
    clippy::useless_conversion,
    reason = "the libc aliases differ between Linux and Darwin; the checked conversion is required to keep one fail-closed comparison"
)]
fn stat_device(metadata: &libc::stat) -> Option<u64> {
    u64::try_from(metadata.st_dev).ok()
}

/// Normalize the platform-specific `mode_t` for the permission comparison.
#[allow(
    clippy::unnecessary_fallible_conversions,
    clippy::useless_conversion,
    reason = "the libc aliases differ between Linux and Darwin; the checked conversion is required to keep one fail-closed comparison"
)]
fn stat_mode(metadata: &libc::stat) -> Option<u32> {
    u32::try_from(metadata.st_mode).ok()
}

/// Normalize the platform-specific `nlink_t` for the identity comparison.
#[allow(
    clippy::unnecessary_fallible_conversions,
    clippy::useless_conversion,
    reason = "the libc aliases differ between Linux and Darwin; the checked conversion is required to keep one fail-closed comparison"
)]
fn stat_links(metadata: &libc::stat) -> Option<u64> {
    u64::try_from(metadata.st_nlink).ok()
}

#[cfg(test)]
thread_local! {
    static COMPLETION_MARKER_SYNC_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static COMPLETION_AFTER_MARKER_REMOVAL: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn inject_completion_marker_sync_failure() {
    COMPLETION_MARKER_SYNC_FAILURE.with(|slot| slot.set(true));
}

#[cfg(test)]
fn inject_completion_after_marker_removal(action: impl FnOnce() + 'static) {
    COMPLETION_AFTER_MARKER_REMOVAL.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "one completion-after-marker-removal action is already installed"
        );
        *slot.borrow_mut() = Some(Box::new(action));
    });
}

#[cfg(test)]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch() -> tempfile::TempDir {
        let root = cyclops_proto::scratch::scratch_root();
        std::fs::create_dir_all(&root).expect("create shared test scratch root");
        tempfile::Builder::new()
            .prefix("cyclops-data-")
            .tempdir_in(root)
            .expect("create owned test scratch root")
    }

    fn private_directory(path: &Path) {
        std::fs::create_dir_all(path).expect("create private directory");
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .expect("protect private directory");
    }

    fn record(path: &Path, bytes: &[u8]) {
        private_directory(path.parent().expect("record has parent"));
        let mut file = create_private_file(path).expect("create private record");
        file.write_all(bytes).expect("write private record");
        file.sync_all().expect("sync private record");
    }

    #[test]
    fn inventory_groups_the_two_journal_families_and_leaves_other_state_out_of_scope() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        record(&home.join("ledger/main.ndjson"), b"{\"seq\":1}\n");
        record(
            &home.join("workspaces/alpha/messages.ndjson"),
            b"{\"seq\":1,\"body\":\"durable\"}\n",
        );
        record(
            &home.join("identity/workspace-id"),
            b"not an exported journal\n",
        );

        let source = inspect_records(&home).expect("inspect durable records");
        assert!(source.inventory.complete());
        assert_eq!(source.inventory.workspace.records.len(), 1);
        assert_eq!(source.inventory.session.records.len(), 1);
        assert_eq!(
            source.inventory.workspace.records[0].path,
            "workspaces/alpha/messages.ndjson"
        );
        assert_eq!(
            source.inventory.session.records[0].path,
            "ledger/main.ndjson"
        );
        assert_eq!(source.inventory.files(), 2);
        assert!(inventory_plain(&source.inventory).contains("preferences, setup files"));
    }

    #[test]
    fn unsafe_state_root_is_refused_before_inventory_or_export() {
        let root = scratch();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
            .expect("make state root parent publicly traversable");
        let home = root.path().join("home");
        private_directory(&home);
        record(&home.join("ledger/main.ndjson"), b"{\"seq\":1}\n");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o777))
            .expect("make state root writable by another user");

        let inspection = match inspect_records(&home) {
            Ok(_) => panic!("inventory must not trust a state root another user can mutate"),
            Err(error) => error,
        };
        assert!(inspection.contains("Cyclops state root"), "{inspection}");
        assert!(inspection.contains("permissions"), "{inspection}");

        let destination = root.path().join("export");
        let export = export_at(&home, &destination)
            .expect_err("export must not copy from a mutable state root");
        assert!(export.contains("Cyclops state root"), "{export}");
        assert!(
            !destination.exists(),
            "a refused source must not create an export destination"
        );
    }

    #[test]
    fn export_preserves_raw_journal_bytes_without_changing_the_source() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let session = b"{\"seq\":1}\nunterminated tail";
        let workspace = b"{\"seq\":1,\"body\":\"keep every byte\"}\n";
        record(&home.join("ledger/main.ndjson"), session);
        record(&home.join("workspaces/alpha/messages.ndjson"), workspace);
        let destination = root.path().join("export");

        let result = export_at(&home, &destination).expect("export durable records");
        assert_eq!(result.files, 2);
        assert_eq!(
            std::fs::read(destination.join("records/ledger/main.ndjson"))
                .expect("read exported session journal"),
            session
        );
        assert_eq!(
            std::fs::read(destination.join("records/workspaces/alpha/messages.ndjson"))
                .expect("read exported workspace journal"),
            workspace
        );
        assert_eq!(
            std::fs::read(home.join("ledger/main.ndjson")).expect("read source session journal"),
            session
        );
        assert_eq!(
            std::fs::read(home.join("workspaces/alpha/messages.ndjson"))
                .expect("read source workspace journal"),
            workspace
        );
        assert!(!destination.join(EXPORT_INCOMPLETE_MARKER).exists());
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(destination.join(EXPORT_MANIFEST)).expect("read export manifest"),
        )
        .expect("parse export manifest");
        assert_eq!(manifest["format"], "raw_ndjson");
        assert_eq!(manifest["records"].as_array().unwrap().len(), 2);
        assert!(
            manifest["snapshot"]
                .as_str()
                .is_some_and(|snapshot| snapshot.contains("not an atomic daemon snapshot")),
            "export manifest must state the live-source boundary: {manifest}"
        );
    }

    #[test]
    fn export_refuses_an_existing_destination_without_overwriting_it() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        record(&home.join("ledger/main.ndjson"), b"{\"seq\":1}\n");
        let destination = root.path().join("already-there");
        private_directory(&destination);
        record(&destination.join("keep"), b"operator data");

        let error = match export_at(&home, &destination) {
            Ok(_) => panic!("existing destination must refuse"),
            Err(error) => error,
        };
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read(destination.join("keep")).expect("read preserved destination"),
            b"operator data"
        );
    }

    #[test]
    fn incomplete_marker_is_established_before_copy_and_retained_after_copy_failure() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let journal = home.join("ledger/main.ndjson");
        record(&journal, b"{\"seq\":1}\n");
        let source = inspect_records(&home).expect("inspect durable records");
        let destination_path = root.path().join("export");
        let destination =
            ExportDestination::create(&destination_path).expect("establish incomplete export");
        let marker = destination_path.join(EXPORT_INCOMPLETE_MARKER);
        assert!(marker.is_file(), "marker exists before any journal copy");
        assert_eq!(
            std::fs::metadata(&marker)
                .expect("inspect marker")
                .permissions()
                .mode()
                & 0o777,
            PRIVATE_FILE_MODE
        );

        std::fs::remove_file(&journal).expect("simulate a source disappearing before copy");
        let error = export_into(&source, &destination)
            .expect_err("a failed copy must not complete the export");
        assert!(error.message().contains("disappeared"), "{error:?}");
        assert!(
            marker.exists(),
            "failed export retained its incomplete marker"
        );
        assert!(!destination_path.join(EXPORT_MANIFEST).exists());
    }

    #[test]
    fn completion_does_not_remove_a_replaced_incomplete_marker() {
        let root = scratch();
        let destination_path = root.path().join("export");
        let destination =
            ExportDestination::create(&destination_path).expect("establish incomplete export");
        let marker = destination_path.join(EXPORT_INCOMPLETE_MARKER);
        std::fs::remove_file(&marker).expect("replace marker in local fault simulation");
        std::fs::create_dir(&marker).expect("make replacement marker directory");

        let error = destination
            .complete()
            .expect_err("completion must not remove a different marker entry");
        assert!(matches!(&error, ExportFailure::Incomplete(_)), "{error:?}");
        assert!(error.message().contains("changed"), "{error:?}");
        assert!(marker.is_dir(), "replacement marker stays for inspection");
    }

    #[test]
    fn post_removal_sync_failure_reports_completion_as_uncertain() {
        let root = scratch();
        let destination_path = root.path().join("export");
        let destination =
            ExportDestination::create(&destination_path).expect("establish incomplete export");
        inject_completion_marker_sync_failure();

        let error = destination
            .complete()
            .expect_err("failed post-removal sync must not report a completed export");
        assert!(
            matches!(&error, ExportFailure::CompletionUncertain(_)),
            "{error:?}"
        );
        assert!(
            !destination_path.join(EXPORT_INCOMPLETE_MARKER).exists(),
            "the command must admit that durable marker removal is uncertain"
        );
    }

    #[test]
    fn destination_replacement_after_marker_removal_reports_completion_as_uncertain() {
        let root = scratch();
        let destination_path = root.path().join("export");
        let destination =
            ExportDestination::create(&destination_path).expect("establish incomplete export");
        let held_path = root.path().join("held-export");
        let outside = root.path().join("outside");
        private_directory(&outside);
        let destination_for_swap = destination_path.clone();
        let held_for_swap = held_path.clone();
        let outside_for_swap = outside.clone();
        inject_completion_after_marker_removal(move || {
            std::fs::rename(&destination_for_swap, &held_for_swap)
                .expect("move completed directory after its marker is removed");
            symlink(&outside_for_swap, &destination_for_swap)
                .expect("replace completed destination with a symlink");
        });

        let error = destination
            .complete()
            .expect_err("a replaced completion path must not report success");
        assert!(
            matches!(&error, ExportFailure::CompletionUncertain(_)),
            "{error:?}"
        );
        assert!(
            !held_path.join(EXPORT_INCOMPLETE_MARKER).exists(),
            "the marker was removed from the held completed directory"
        );
        assert!(
            !outside.join(EXPORT_INCOMPLETE_MARKER).exists(),
            "the replacement symlink never received the held directory's entries"
        );
    }

    #[test]
    fn destination_symlink_swap_cannot_redirect_export_or_report_success() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        record(&home.join("ledger/main.ndjson"), b"{\"seq\":1}\n");
        let source = inspect_records(&home).expect("inspect durable records");
        let destination_path = root.path().join("export");
        let destination =
            ExportDestination::create(&destination_path).expect("establish incomplete export");
        let held_path = root.path().join("held-export");
        let outside = root.path().join("outside");
        private_directory(&outside);
        std::fs::rename(&destination_path, &held_path)
            .expect("move named destination after descriptor is held");
        symlink(&outside, &destination_path).expect("replace export path with a symlink");

        let error = export_into(&source, &destination)
            .expect_err("changed destination path must not report a completed export");
        assert!(error.message().contains("destination"), "{error:?}");
        assert!(
            !outside.join(EXPORT_RECORDS_DIRECTORY).exists(),
            "held-descriptor writes never followed the replacement symlink"
        );
        assert!(
            held_path.join(EXPORT_INCOMPLETE_MARKER).exists(),
            "the original held directory retained the incomplete state"
        );
    }

    #[test]
    fn export_refuses_a_destination_parent_writable_by_other_users() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        record(&home.join("ledger/main.ndjson"), b"{\"seq\":1}\n");
        let parent = root.path().join("other-writable");
        private_directory(&parent);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777))
            .expect("make local fault parent writable by other users");
        let destination = parent.join("export");

        let error = export_at(&home, &destination)
            .expect_err("private export must reject a mutable destination parent");
        assert!(error.contains("writable by another user"), "{error}");
        assert!(!destination.exists());
    }

    #[test]
    fn export_refuses_a_symlinked_destination_parent() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        record(&home.join("ledger/main.ndjson"), b"{\"seq\":1}\n");
        let parent = root.path().join("physical-parent");
        private_directory(&parent);
        let alias = root.path().join("parent-alias");
        symlink(&parent, &alias).expect("make local symlinked parent simulation");
        let destination = alias.join("export");

        let _error = export_at(&home, &destination)
            .expect_err("export location must not be hidden behind a parent symlink");
        assert!(!parent.join("export").exists());
    }

    #[test]
    fn source_recheck_refuses_a_record_added_during_export() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        record(&home.join("ledger/main.ndjson"), b"{\"seq\":1}\n");
        let source = inspect_records(&home).expect("inspect durable records");
        record(&home.join("ledger/later.ndjson"), b"{\"seq\":2}\n");

        let error = ensure_source_still_matches_inventory(&source)
            .expect_err("a later durable record must not be silently omitted");
        assert!(error.contains("changed during export"), "{error}");
    }

    #[test]
    fn source_recheck_refuses_a_same_length_atomic_replacement() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let journal = home.join("ledger/main.ndjson");
        record(&journal, b"{\"seq\":1}\n");
        let source = inspect_records(&home).expect("inspect durable records");
        let replacement = home.join("ledger/replacement.ndjson");
        record(&replacement, b"{\"seq\":2}\n");
        std::fs::rename(&replacement, &journal).expect("replace journal atomically");

        let error = ensure_source_still_matches_inventory(&source)
            .expect_err("a same-length replacement must not be called one snapshot");
        assert!(error.contains("changed during export"), "{error}");
    }

    #[test]
    fn source_recheck_refuses_a_same_length_in_place_rewrite() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        let journal = home.join("ledger/main.ndjson");
        record(&journal, b"{\"seq\":1}\n");
        let source = inspect_records(&home).expect("inspect durable records");

        let mut rewrite = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&journal)
            .expect("open journal for local rewrite simulation");
        rewrite
            .write_all(b"{\"seq\":2}\n")
            .expect("rewrite journal at the same length");
        rewrite.sync_all().expect("sync local rewrite simulation");

        let error = ensure_source_still_matches_inventory(&source)
            .expect_err("a same-length rewrite must not be called one snapshot");
        assert!(error.contains("changed during export"), "{error}");
    }

    #[test]
    fn linked_journals_are_refused_without_reading_their_target() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        private_directory(&home.join("ledger"));
        let outside = root.path().join("outside.ndjson");
        record(&outside, b"outside record");
        symlink(&outside, home.join("ledger/linked.ndjson")).expect("link outside record");

        let error = match inspect_records(&home) {
            Ok(_) => panic!("linked journal must refuse"),
            Err(error) => error,
        };
        assert!(error.contains("regular file"), "{error}");
        assert_eq!(
            std::fs::read(&outside).expect("read outside record"),
            b"outside record"
        );
    }

    #[test]
    fn linked_workspace_journal_directories_are_refused_without_reading_their_target() {
        let root = scratch();
        let home = root.path().join("home");
        private_directory(&home);
        private_directory(&home.join("workspaces"));
        let outside = root.path().join("outside-workspace");
        record(
            &outside.join("messages.ndjson"),
            b"outside workspace record",
        );
        symlink(&outside, home.join("workspaces/alpha")).expect("link outside workspace");

        let error = match inspect_records(&home) {
            Ok(_) => panic!("linked workspace must refuse"),
            Err(error) => error,
        };
        assert!(error.contains("symbolic links"), "{error}");
        assert_eq!(
            std::fs::read(outside.join("messages.ndjson")).expect("read outside workspace"),
            b"outside workspace record"
        );
    }
}
