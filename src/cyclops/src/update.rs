//! Transactional installation, update, and rollback for the Cyclops binary pair.
//!
//! `cyclops` and `cyclopsd` are always selected together. Each release lives in
//! an immutable pair directory. One atomic selector points at an immutable
//! record containing both the active and known-good pair identities. The two
//! public binary names resolve through that selector, so they cannot observe
//! different releases during activation or rollback.
//!
//! The source is a fresh clone of `CYCLOPS_REPO` at `CYCLOPS_REF` (the
//! installer's own overrides, same defaults), never the hosted
//! install.sh: the hosted copy is only as fresh as the last website
//! deploy, and a clone always runs the installer that matches the code
//! it builds.
//!
//! Before activation, the candidate pair proves source identity and boots
//! against a private journal snapshot. A running daemon is quiesced and stopped
//! only after one authenticated connection proves its exact process generation.
//! Failed activation restores the prior selector and daemon. An open workspace
//! keeps executing its current process until the operator detaches it.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _,
    PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::copy;
use crate::hash::fnv64;
use crate::render;
use crate::style::Style;

/// The installer's defaults (scripts/install.sh:26-27), restated because
/// the binary cannot read that file at runtime: change them together.
const DEFAULT_REPO: &str = "https://github.com/cyclops-team/cyclops.git";
const DEFAULT_REF: &str = "main";

/// The commit baked into this binary by build.rs.
const BUILD_REF: &str = env!("CYCLOPS_BUILD_REF");

/// What the baked build ref can say about a freshness check.
#[derive(Debug, PartialEq)]
enum LocalBuild {
    /// A clean commit: comparable to the remote by sha prefix.
    Sha(String),
    /// Built from edited sources; no remote commit can match it.
    Dirty(String),
    /// Built outside git (a source tarball); there is nothing to compare.
    Unknown,
}

/// build.rs stamps exactly three shapes: `<short-sha>`, `<short-sha>.dirty`,
/// or the literal `unknown`.
fn classify(build_ref: &str) -> LocalBuild {
    if build_ref == "unknown" {
        LocalBuild::Unknown
    } else if build_ref.ends_with(".dirty") {
        // The full form is kept: the note quotes it, and the bare sha is
        // never compared (nothing can match an edited tree).
        LocalBuild::Dirty(build_ref.to_string())
    } else {
        LocalBuild::Sha(build_ref.to_string())
    }
}

/// The remote already carries the running commit. Prefix match, because
/// build.rs stamps `git rev-parse --short`, whose length grows when short
/// shas collide, and the remote answers with the full 40.
fn is_current(local_short: &str, remote_sha: &str) -> bool {
    !local_short.is_empty()
        && remote_sha.len() >= local_short.len()
        && remote_sha.starts_with(local_short)
}

/// The commit `reff` names at `repo`, asked of the remote itself. One
/// cheap round trip, no clone, no checkout.
fn ls_remote(repo: &str, reff: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["ls-remote", repo, reff])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().next() {
        Some(sha) if !sha.is_empty() => Ok(sha.to_string()),
        _ => Err(format!("{reff} names nothing there")),
    }
}

/// Shallow clone of `repo` at `reff` into `dest`, quiet on success.
///
/// `HEAD` cannot go through `--branch` (it is not a branch), so it takes
/// the bare clone, which lands on whatever the repo has checked out. That
/// is the form that lets a local mirror with no named branches serve as
/// an update source.
fn clone(repo: &str, reff: &str, dest: &Path) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth", "1"]);
    if reff != "HEAD" {
        cmd.args(["--branch", reff]);
    }
    let out = cmd
        .arg(repo)
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Where update keeps its build cache, so a rebuild is incremental.
///
/// Outside the state root because Cargo writes executable build artifacts.
pub(crate) fn build_cache(home: &Path) -> PathBuf {
    let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    let home_key = fnv64(home.as_os_str().as_bytes());
    temp.join(format!("cyclops-build-cache-{home_key}"))
}

/// The build-cache lease shared by update and cleanup.
pub(crate) const BUILD_CACHE_LEASE: &str = ".lease";

/// A held advisory lock that is released before its descriptor is closed.
pub(crate) struct ExclusiveLease(File);

impl Drop for ExclusiveLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Hold the build cache against cleanup for the lifetime of the returned lease.
pub(crate) fn lock_build_cache(root: &cyclops_state::StateRoot) -> Result<ExclusiveLease, String> {
    let lease = root
        .open_append(Path::new(BUILD_CACHE_LEASE))
        .map_err(|error| format!("open build-cache lease: {error}"))?
        .into_file();
    if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!(
            "the Cyclops build cache is in use: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(ExclusiveLease(lease))
}

pub(crate) const SCRATCH_MARKER: &str = ".cyclops-update-owner";
pub(crate) const SCRATCH_LEASE: &str = ".lease";
pub(crate) const SCRATCH_PREFIX: &str = "cycu.";

/// Random owner-only update workspace with an exclusive kernel lease.
struct Scratch {
    path: PathBuf,
    marker: String,
    device: u64,
    inode: u64,
    marker_device: u64,
    marker_inode: u64,
    _lease: ExclusiveLease,
}

impl Scratch {
    fn create() -> Result<Self, String> {
        let temp = std::fs::canonicalize(std::env::temp_dir())
            .map_err(|error| format!("resolve temporary directory: {error}"))?;
        for _ in 0..32 {
            let nonce = random_hex()?;
            // Keep the replay socket below the Unix sun_path limit even in
            // macOS's long per-user temporary directory.
            let path = temp.join(format!("{SCRATCH_PREFIX}{nonce}"));
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    let metadata = std::fs::symlink_metadata(&path)
                        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
                    if !metadata.is_dir()
                        || metadata.file_type().is_symlink()
                        || metadata.uid() != unsafe { libc::geteuid() }
                        || metadata.permissions().mode() & 0o777 != 0o700
                    {
                        return Err(format!(
                            "update scratch {} is not an owner-only directory",
                            path.display()
                        ));
                    }
                    write_new(&path.join(SCRATCH_MARKER), nonce.as_bytes(), 0o600)?;
                    let marker_metadata = std::fs::symlink_metadata(path.join(SCRATCH_MARKER))
                        .map_err(|error| format!("inspect update owner marker: {error}"))?;
                    if marker_metadata.file_type().is_symlink()
                        || !marker_metadata.is_file()
                        || marker_metadata.nlink() != 1
                        || marker_metadata.uid() != unsafe { libc::geteuid() }
                        || marker_metadata.permissions().mode() & 0o777 != 0o600
                    {
                        return Err("update owner marker is not an owner-only file".to_string());
                    }
                    let lease = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(path.join(SCRATCH_LEASE))
                        .map_err(|error| format!("create update lease: {error}"))?;
                    let locked =
                        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                    if locked != 0 {
                        return Err(format!(
                            "lock update lease: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                    return Ok(Self {
                        path,
                        marker: nonce,
                        device: metadata.dev(),
                        inode: metadata.ino(),
                        marker_device: marker_metadata.dev(),
                        marker_inode: marker_metadata.ino(),
                        _lease: ExclusiveLease(lease),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create update scratch: {error}")),
            }
        }
        Err("could not mint a unique update scratch directory".to_string())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let unchanged = std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        });
        let marker_path = self.path.join(SCRATCH_MARKER);
        let marker_unchanged = std::fs::symlink_metadata(&marker_path).is_ok_and(|metadata| {
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o777 == 0o600
                && metadata.dev() == self.marker_device
                && metadata.ino() == self.marker_inode
        });
        let marker_matches = marker_unchanged
            && std::fs::read_to_string(marker_path).is_ok_and(|marker| marker == self.marker);
        if unchanged && marker_matches {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("read system randomness: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// The installed binary, resolved the way a shell resolves it.
///
/// Never this process, for two different reasons. Reporting the new
/// version: this process is still the old build, and self-reporting is how
/// an update that silently failed would go unnoticed. Picking the prefix:
/// the copy a shell runs is the copy an update has to land on, and this
/// process may be a build directory nobody installed from.
fn installed_cyclops() -> Option<PathBuf> {
    if let Some(p) = which("cyclops") {
        return Some(p);
    }
    // Nothing on PATH: the installer's own prefix candidates, in its
    // order, for the case where it just created the directory and only a
    // fresh shell will see it.
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [".local/bin", "bin", ".cargo/bin"]
        .iter()
        .map(|d| home.join(d).join("cyclops"))
        .find(|p| is_executable_file(p))
}

/// First match for a bare name on PATH. daemon.rs holds the same six
/// lines for finding cyclopsd; the two resolve different binaries for
/// different reasons and neither is a library.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    which_in(name, &path)
}

fn which_in(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|d| d.join(name))
        .find(|p| is_executable_file(p))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    metadata.is_file() && unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0
}

/// Where the installer must write, resolved before anything is replaced.
///
/// None when no cyclops resolves at all, which is the machine that has
/// never been installed to; there the installer's own `pick_prefix` is the
/// only answer and it makes it.
fn install_prefix() -> Option<PathBuf> {
    installed_cyclops()?.parent().map(Path::to_path_buf)
}

const PAIR_ROOT: &str = ".cyclops-pairs";
const PAIRS_DIR: &str = "pairs";
const SELECTIONS_DIR: &str = "selections";
const ACTIVE_SELECTOR: &str = "active";
const PAIR_DESCRIPTOR: &str = "state.json";
const PAIR_OWNER: &str = ".owner";
const PAIR_LEASE: &str = ".lease";

struct PairStore {
    prefix: PathBuf,
    root: PathBuf,
    root_device: u64,
    root_inode: u64,
    owner_device: u64,
    owner_inode: u64,
    lease_device: u64,
    lease_inode: u64,
    _lease: ExclusiveLease,
}

// Owner-only directories separate local accounts. The kernel lease and inode
// rechecks prevent cooperating same-user operations from racing one another.
// A hostile process already running as the same uid is outside this boundary.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Selection {
    id: String,
    active: String,
    known_good: String,
    legacy_active: bool,
    active_proof: Option<PairProof>,
    known_good_proof: Option<PairProof>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PairProof {
    identity: String,
    cyclops_sha256: String,
    cyclopsd_sha256: String,
}

/// Read-only rollback proof consumed by health reporting.
#[derive(Debug)]
pub(crate) struct InstalledPairDescriptor {
    pub(crate) selection: PathBuf,
    pub(crate) active_pair: PathBuf,
    pub(crate) known_good_pair: PathBuf,
    pub(crate) active_identity: Option<String>,
    pub(crate) known_good_identity: String,
    pub(crate) active_build: Option<String>,
    pub(crate) known_good_build: String,
    pub(crate) rollback_safe: bool,
}

/// Inspect the selected pair under the same kernel lease update uses.
pub(crate) fn installed_pair_descriptor(
    prefix: &Path,
) -> Result<Option<InstalledPairDescriptor>, String> {
    let Some(store) = PairStore::open_existing(prefix)? else {
        return Ok(None);
    };
    let selection = store
        .selection_descriptor()?
        .ok_or_else(|| "the managed pair store has no active selector".to_string())?;
    let active_pair = store.root.join(&selection.active);
    let known_good_pair = store.root.join(&selection.known_good);
    let active_proof = selection.active_proof.clone();
    let known_good_proof = selection.known_good_proof.clone().ok_or_else(|| {
        "the selected pair predates recorded build identity; run one update before trusting rollback"
            .to_string()
    })?;
    if !selection.legacy_active && active_proof.is_none() {
        return Err(
            "the active pair does not record its build identity; rollback is unproven".to_string(),
        );
    }
    if let Some(proof) = active_proof.as_ref() {
        verify_recorded_pair(&active_pair, proof)?;
    }
    verify_recorded_pair(&known_good_pair, &known_good_proof)?;
    let active_identity = active_proof.as_ref().map(|proof| proof.identity.clone());
    let known_good_identity = known_good_proof.identity;
    let active_build = active_identity.as_deref().map(identity_build).transpose()?;
    let known_good_build = identity_build(&known_good_identity)?;
    Ok(Some(InstalledPairDescriptor {
        selection: store.root.join(&selection.id),
        active_pair,
        known_good_pair,
        active_identity,
        known_good_identity,
        active_build,
        known_good_build,
        rollback_safe: !selection.legacy_active && selection.active != selection.known_good,
    }))
}

impl PairStore {
    fn open(prefix: &Path) -> Result<Self, String> {
        Self::open_inner(prefix, true)?.ok_or_else(|| "pair store was not created".to_string())
    }

    fn open_existing(prefix: &Path) -> Result<Option<Self>, String> {
        Self::open_inner(prefix, false)
    }

    fn open_inner(prefix: &Path, create: bool) -> Result<Option<Self>, String> {
        let prefix = std::fs::canonicalize(prefix)
            .map_err(|error| format!("resolve install prefix {}: {error}", prefix.display()))?;
        let root = prefix.join(PAIR_ROOT);
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
                write_new(
                    &root.join(PAIR_OWNER),
                    unsafe { libc::geteuid() }.to_string().as_bytes(),
                    0o600,
                )?;
            }
            Err(error) => return Err(format!("inspect pair store {}: {error}", root.display())),
        }
        require_owner_directory(&root)?;
        let owner_marker = root.join(PAIR_OWNER);
        require_owner_regular_file(&owner_marker, 0o600)?;
        let owner = std::fs::read_to_string(&owner_marker)
            .map_err(|error| format!("read pair owner marker: {error}"))?;
        if owner != unsafe { libc::geteuid() }.to_string() {
            return Err("pair store ownership marker does not match this user".to_string());
        }
        let lease_path = root.join(PAIR_LEASE);
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lease_path)
            .map_err(|error| format!("open pair update lease: {error}"))?;
        require_owner_regular_file(&lease_path, 0o600)?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "another Cyclops update holds the pair store lease: {}",
                std::io::Error::last_os_error()
            ));
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
            }
            require_owner_directory(&directory)?;
        }
        Ok(Some(Self {
            prefix,
            root,
            root_device: root_metadata.dev(),
            root_inode: root_metadata.ino(),
            owner_device: owner_metadata.dev(),
            owner_inode: owner_metadata.ino(),
            lease_device: lease_metadata.dev(),
            lease_inode: lease_metadata.ino(),
            _lease: lease,
        }))
    }

    fn require_root(&self) -> Result<(), String> {
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

    fn stage(&self, source: &Path) -> Result<String, String> {
        self.require_root()?;
        let pair_id = format!("pair.{}", random_hex()?);
        let destination = self.root.join(PAIRS_DIR).join(&pair_id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&destination)
            .map_err(|error| format!("create staged pair: {error}"))?;
        let staged = (|| {
            for name in ["cyclops", "cyclopsd"] {
                copy_executable(&source.join(name), &destination.join(name))?;
            }
            prove_pair(&destination)?;
            sync_directory(&destination)
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
    fn stage_legacy(&self, source: &Path) -> Result<String, String> {
        self.require_root()?;
        let pair_id = format!("pair.{}", random_hex()?);
        let destination = self.root.join(PAIRS_DIR).join(&pair_id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&destination)
            .map_err(|error| format!("create legacy migration pair: {error}"))?;
        let staged = (|| {
            for name in ["cyclops", "cyclopsd"] {
                copy_executable(&source.join(name), &destination.join(name))?;
            }
            sync_directory(&destination)
        })();
        match staged {
            Ok(()) => Ok(format!("{PAIRS_DIR}/{pair_id}")),
            Err(error) => match remove_pair_residue_directory(&destination) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; legacy pair cleanup refused: {cleanup}")),
            },
        }
    }

    fn pair_path(&self, target: &str) -> Result<PathBuf, String> {
        self.require_root()?;
        self.require_pair(target)?;
        Ok(self.root.join(target))
    }

    fn active_binary(&self, name: &str) -> Result<PathBuf, String> {
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
    fn migrate_direct_pair(&self, candidate: &str) -> Result<(), String> {
        self.require_root()?;
        self.require_pair(candidate)?;
        if self.selection()?.is_some() {
            return self.repair_public_links();
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
            return Err("managed Cyclops links have no active selector".to_string());
        }
        match (cli_meta, daemon_meta) {
            (None, None) => Ok(()),
            (Some(cli_meta), Some(daemon_meta)) if cli_meta.is_file() && daemon_meta.is_file() => {
                let source = self.prefix.clone();
                let matched = prove_pair_identity(&source).is_ok();
                let old = if matched {
                    self.stage(&source)?
                } else {
                    self.stage_legacy(&source)?
                };
                let selection = if matched {
                    self.prepare_selection(&old, &old)?
                } else {
                    self.prepare_legacy_selection(&old, candidate)?
                };
                self.select(&selection)?;
                // The daemon path moves first but still resolves to the same
                // copied bytes as the direct CLI. The second move completes
                // the stable indirection without a mixed pair window.
                self.replace_public_link("cyclopsd")?;
                self.replace_public_link("cyclops")?;
                Ok(())
            }
            _ => Err(
                "the install prefix contains only one Cyclops binary or an unsupported file type"
                    .to_string(),
            ),
        }
    }

    fn activate(&self, candidate: &str) -> Result<Option<Selection>, String> {
        self.require_root()?;
        self.require_pair(candidate)?;
        let previous = self.selection()?;
        if previous
            .as_ref()
            .is_some_and(|value| value.active == candidate)
        {
            self.require_public_links()?;
            return Ok(previous);
        }
        let known_good = previous
            .as_ref()
            .map(|value| {
                if value.legacy_active {
                    value.known_good.as_str()
                } else {
                    value.active.as_str()
                }
            })
            .unwrap_or(candidate);
        let selection = self.prepare_selection(candidate, known_good)?;
        self.select(&selection)?;
        if previous.is_none() {
            self.replace_public_link("cyclopsd")?;
            self.replace_public_link("cyclops")?;
        } else {
            self.require_public_links()?;
        }
        Ok(previous)
    }

    fn rollback(&self) -> Result<(Selection, Selection), String> {
        let current = self.rollback_selection()?;
        let restored = self.prepare_selection(&current.known_good, &current.active)?;
        self.select(&restored)?;
        Ok((current, restored))
    }

    /// Validate the selected rollback relationship without changing it.
    fn rollback_selection(&self) -> Result<Selection, String> {
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

    fn restore_selection(&self, selection: &Selection) -> Result<(), String> {
        self.require_root()?;
        self.require_selection(selection)?;
        self.select(selection)
    }

    fn discard(&self, pair: &str) -> Result<(), String> {
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
    fn remove_managed(self) -> Result<(), String> {
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

    fn validate_managed_schema(&self) -> Result<(), String> {
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

    fn prune(&self) -> Result<(), String> {
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

    fn selection(&self) -> Result<Option<Selection>, String> {
        let selection = self.selection_descriptor()?;
        if let Some(selection) = selection.as_ref() {
            self.require_selection(selection)?;
        }
        Ok(selection)
    }

    /// Read selected paths and recorded identities without executing a binary.
    fn selection_descriptor(&self) -> Result<Option<Selection>, String> {
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

    fn select(&self, selection: &Selection) -> Result<(), String> {
        self.require_root()?;
        self.require_selection(selection)?;
        let temporary = self
            .root
            .join(format!(".{ACTIVE_SELECTOR}.{}", random_hex()?));
        std::os::unix::fs::symlink(&selection.id, &temporary)
            .map_err(|error| format!("create selector {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, self.root.join(ACTIVE_SELECTOR))
            .map_err(|error| format!("activate pair selector: {error}"))?;
        sync_directory(&self.root)
    }

    fn prepare_selection(&self, active: &str, known_good: &str) -> Result<Selection, String> {
        self.prepare_selection_with_trust(active, known_good, false)
    }

    fn prepare_legacy_selection(
        &self,
        active: &str,
        known_good: &str,
    ) -> Result<Selection, String> {
        self.prepare_selection_with_trust(active, known_good, true)
    }

    fn prepare_selection_with_trust(
        &self,
        active: &str,
        known_good: &str,
        legacy_active: bool,
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
        let id = format!("{SELECTIONS_DIR}/selection.{}", random_hex()?);
        let directory = self.root.join(&id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&directory)
            .map_err(|error| format!("create selection record: {error}"))?;
        let selection = Selection {
            id,
            active: active.to_string(),
            known_good: known_good.to_string(),
            legacy_active,
            active_proof,
            known_good_proof,
        };
        for name in ["cyclops", "cyclopsd"] {
            let target = PathBuf::from("../..").join(active).join(name);
            std::os::unix::fs::symlink(target, directory.join(name))
                .map_err(|error| format!("write selection binary {name}: {error}"))?;
        }
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": 2,
            "active": active,
            "known_good": known_good,
            "legacy_active": legacy_active,
            "active_proof": selection.active_proof.as_ref(),
            "known_good_proof": selection.known_good_proof.as_ref(),
        }))
        .map_err(|error| format!("encode pair descriptor: {error}"))?;
        write_new(&directory.join(PAIR_DESCRIPTOR), &body, 0o600)?;
        sync_directory(&directory)?;
        self.require_selection(&selection)?;
        Ok(selection)
    }

    fn read_selection(&self, id: &str) -> Result<Selection, String> {
        self.require_root()?;
        validate_selection_target(id)?;
        let directory = self.root.join(id);
        require_owner_directory(&directory)?;
        let descriptor = directory.join(PAIR_DESCRIPTOR);
        require_owner_regular_file(&descriptor, 0o600)?;
        let body = std::fs::read(&descriptor)
            .map_err(|error| format!("read selected pair descriptor: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("decode selected pair descriptor: {error}"))?;
        let schema = value
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "selected pair descriptor has no schema".to_string())?;
        if !matches!(schema, 1 | 2) {
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
        if schema == 2 && (known_good_proof.is_none() || (!legacy_active && active_proof.is_none()))
        {
            return Err(
                "selected pair descriptor is missing a recorded build identity".to_string(),
            );
        }
        Ok(Selection {
            id: id.to_string(),
            active: active.to_string(),
            known_good: known_good.to_string(),
            legacy_active,
            active_proof,
            known_good_proof,
        })
    }

    fn require_selection(&self, selection: &Selection) -> Result<(), String> {
        self.require_selection_layout(selection)?;
        if !selection.legacy_active {
            let proof = self.pair_proof(&selection.active)?;
            if selection
                .active_proof
                .as_ref()
                .is_some_and(|recorded| recorded != &proof)
            {
                return Err("active pair identity does not match its selection record".to_string());
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
        Ok(())
    }

    fn require_selection_layout(&self, selection: &Selection) -> Result<(), String> {
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

    fn require_pair(&self, target: &str) -> Result<(), String> {
        self.pair_proof(target).map(|_| ())
    }

    fn pair_proof(&self, target: &str) -> Result<PairProof, String> {
        self.require_root()?;
        validate_pair_target(target)?;
        let directory = self.root.join(target);
        require_pair_directory(&directory)?;
        prove_pair(&directory)
    }

    fn replace_public_link(&self, name: &str) -> Result<(), String> {
        self.require_root()?;
        let temporary = self.prefix.join(format!(".{name}.{}", random_hex()?));
        let target = PathBuf::from(PAIR_ROOT).join(ACTIVE_SELECTOR).join(name);
        std::os::unix::fs::symlink(&target, &temporary)
            .map_err(|error| format!("create public {name} selector: {error}"))?;
        std::fs::rename(&temporary, self.prefix.join(name))
            .map_err(|error| format!("publish {name}: {error}"))?;
        sync_directory(&self.prefix)
    }

    fn require_public_links(&self) -> Result<(), String> {
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

    fn repair_public_links(&self) -> Result<(), String> {
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

fn validate_pair_target(target: &str) -> Result<(), String> {
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

fn validate_selection_target(target: &str) -> Result<(), String> {
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

fn valid_random_name(name: &std::ffi::OsStr, prefix: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(nonce) = name.strip_prefix(prefix) else {
        return false;
    };
    nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn require_owner_directory(path: &Path) -> Result<(), String> {
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

fn require_install_prefix(path: &Path) -> Result<(), String> {
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

fn require_unlinked_regular_file(path: &Path) -> Result<(), String> {
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

fn require_owner_regular_file(path: &Path, mode: u32) -> Result<(), String> {
    require_unlinked_regular_file(path)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.permissions().mode() & 0o777 != mode {
        return Err(format!("{} does not have mode {mode:o}", path.display()));
    }
    Ok(())
}

fn require_executable(path: &Path) -> Result<(), String> {
    require_unlinked_regular_file(path)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.permissions().mode() & 0o100 == 0 {
        return Err(format!("{} is not executable by its owner", path.display()));
    }
    Ok(())
}

fn regular_files_equal(left: &Path, right: &Path) -> Result<bool, String> {
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

fn metadata_unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && after.nlink() == 1
}

fn read_directory(path: &Path, kind: &str) -> Result<Vec<std::fs::DirEntry>, String> {
    std::fs::read_dir(path)
        .map_err(|error| format!("read {kind}: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {kind} entry: {error}"))
}

fn require_exact_entries(directory: &Path, allowed: &[&str]) -> Result<(), String> {
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

fn require_pair_directory(directory: &Path) -> Result<(), String> {
    require_owner_directory(directory)?;
    require_exact_entries(directory, &["cyclops", "cyclopsd"])?;
    for name in ["cyclops", "cyclopsd"] {
        require_executable(&directory.join(name))?;
    }
    Ok(())
}

/// Validate an unselected staging residue without requiring both binaries.
fn validate_pair_residue_directory(directory: &Path) -> Result<(), String> {
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

fn remove_pair_residue_directory(directory: &Path) -> Result<(), String> {
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

fn remove_pair_directory(directory: &Path) -> Result<(), String> {
    require_pair_directory(directory)?;
    remove_pair_residue_directory(directory)
}

fn validate_selection_directory(directory: &Path) -> Result<(), String> {
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

fn remove_selection_directory(directory: &Path) -> Result<(), String> {
    validate_selection_directory(directory)?;
    for name in ["cyclops", "cyclopsd"] {
        let path = directory.join(name);
        std::fs::remove_file(&path)
            .map_err(|error| format!("remove stale selection {}: {error}", path.display()))?;
    }
    let descriptor = directory.join(PAIR_DESCRIPTOR);
    std::fs::remove_file(&descriptor)
        .map_err(|error| format!("remove stale selection descriptor: {error}"))?;
    std::fs::remove_dir(directory)
        .map_err(|error| format!("remove stale selection directory: {error}"))
}

fn copy_executable(source: &Path, destination: &Path) -> Result<(), String> {
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

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

const MAX_PAIR_BINARY_BYTES: u64 = 256 * 1024 * 1024;

fn prove_pair(directory: &Path) -> Result<PairProof, String> {
    let identity = prove_pair_identity(directory)?;
    Ok(PairProof {
        identity,
        cyclops_sha256: executable_sha256(&directory.join("cyclops"))?,
        cyclopsd_sha256: executable_sha256(&directory.join("cyclopsd"))?,
    })
}

/// Validate the recorded install proof without executing either binary.
fn verify_recorded_pair(directory: &Path, proof: &PairProof) -> Result<(), String> {
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

fn executable_sha256(path: &Path) -> Result<String, String> {
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

fn prove_pair_identity(directory: &Path) -> Result<String, String> {
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

fn binary_identity(binary: &Path, expected_name: &str) -> Result<String, String> {
    require_executable(binary)?;
    let output = Command::new(binary)
        .arg("--version")
        .output()
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
fn replay_log_tail(root_path: &Path, descendant: &Path) -> Option<Vec<u8>> {
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
fn sanitize_replay_diagnostic(line: &str) -> Option<String> {
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

fn daemon_replay_failure_line(bytes: &[u8]) -> Option<String> {
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

fn captured_replay_failure_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .find_map(sanitize_replay_diagnostic)
}

/// Prefer the daemon's boot log. Diagnostic read failures never replace the
/// child exit status, and captured stdout or stderr remains the fallback.
fn candidate_replay_failure_detail(probe_home: &Path, scratch_root: &Path) -> String {
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
fn prove_candidate_replay(
    pair: &Path,
    source_home: &Path,
    scratch: &Scratch,
) -> Result<String, String> {
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
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(&socket) {
            let mut line = String::new();
            let mut reader = std::io::BufReader::new(stream);
            std::io::BufRead::read_line(&mut reader, &mut line)
                .map_err(|error| format!("read candidate hello: {error}"))?;
            break serde_json::from_str::<cyclops_proto::Hello>(line.trim())
                .map_err(|error| format!("decode candidate hello: {error}"))?;
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
    let build = candidate_build(&pair.join("cyclops"))?;
    if hello.build.as_deref() != Some(build.as_str()) {
        return Err("candidate CLI and daemon source builds do not match".to_string());
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
    Ok(build)
}

/// Reap a private replay daemon on every return path.
struct ReplayChild(Option<std::process::Child>);

impl ReplayChild {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut std::process::Child {
        self.0.as_mut().expect("replay child is armed")
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
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
const MAX_REPLAY_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPLAY_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Copy only state that daemon boot reads. Logs, caches, saved workspace
/// layouts, sockets, and update artifacts cannot affect journal replay.
fn copy_replay_state(source: &Path, destination: &Path) -> Result<(), String> {
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

pub(crate) fn candidate_build(binary: &Path) -> Result<String, String> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|error| format!("run {} --version: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!("{} --version failed", binary.display()));
    }
    identity_build(String::from_utf8_lossy(&output.stdout).trim())
        .map_err(|_| format!("{} did not report a source build", binary.display()))
}

fn identity_build(identity: &str) -> Result<String, String> {
    let build = identity
        .strip_suffix(')')
        .and_then(|line| line.rsplit_once(" (").map(|(_, build)| build))
        .filter(|build| !build.is_empty())
        .ok_or_else(|| "recorded pair identity has no source build".to_string())?;
    Ok(build.to_string())
}

fn copy_state_tree(
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

fn copy_regular_file(source: &Path, destination: &Path, mode: u32) -> Result<(), String> {
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

fn isolate_probe_config(home: &Path) -> Result<(), String> {
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

/// `<bin> --version` with the leading command name stripped, so the
/// report reads `0.1.0 (e610afc)` on both sides of the arrow.
fn version_of(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(version_words(&text))
}

fn version_words(version_line: &str) -> String {
    version_line
        .strip_prefix("cyclops ")
        .unwrap_or(version_line)
        .to_string()
}

/// The already-current badge. Heavy check by render::check's rule: the
/// remote owns the fact and just answered for it.
fn current_badge(reff: &str, style: &Style) -> String {
    format!(
        "{} {} {}",
        style.bold(&format!(
            "{} already the latest {reff}",
            render::check(true)
        )),
        style.dim("·"),
        style.dim("nothing to update")
    )
}

/// The old-to-new report. Heavy check: the new binary on disk answered
/// `--version` itself.
fn updated_badge(old: &str, new: &str, style: &Style) -> String {
    format!(
        "{} {} {old} → {new}",
        style.bold(&format!("{} updated", render::check(true))),
        style.dim("·")
    )
}

/// `cyclops update`. Steps, in the order that keeps a no-op cheap:
///
/// 1. Say which build is running and where the update comes from.
/// 2. Freshness check: `git ls-remote` against the baked sha. Already
///    current says so and stops; a build that cannot be compared says why
///    and goes on.
/// 3. Clone and run the clone's installer, streaming its output, at the
///    prefix the current install already uses, and building into a cache
///    that outlives the clone so the compile is incremental. Its last step
///    is `cyclops start --setup-only --wire-hooks`, which writes the hook
///    config each installed agent CLI reads and repoints the prepared
///    artifacts at the new binary.
/// 4. The candidate installer validates the matched pair, proves journal replay,
///    quiesces the exact daemon generation, changes one selector, and restarts.
/// 5. Report old to new from the selected binary's own `--version`.
pub fn run(
    json: bool,
    style: &Style,
    rollback: bool,
    install_pair: Option<&Path>,
    remove_pair_store: bool,
    prefix: Option<&Path>,
) -> i32 {
    if json {
        eprintln!("{}", copy::UPDATE_NO_JSON);
        return crate::EXIT_USAGE;
    }
    if rollback {
        return run_rollback(style);
    }
    if remove_pair_store {
        let Some(prefix) = prefix else {
            eprintln!("--remove-pair-store requires --prefix");
            return crate::EXIT_USAGE;
        };
        return run_remove_pair_store(prefix);
    }
    if let Some(source) = install_pair {
        let Some(prefix) = prefix else {
            eprintln!("--install-pair requires --prefix");
            return crate::EXIT_USAGE;
        };
        return run_install_pair(source, prefix, style);
    }
    let repo = env_or("CYCLOPS_REPO", DEFAULT_REPO);
    let reff = env_or("CYCLOPS_REF", DEFAULT_REF);

    // 1.
    println!("cyclops {}", crate::VERSION);
    println!("  {}", style.dim(&format!("source {repo} at {reff}")));

    // 2.
    match classify(BUILD_REF) {
        LocalBuild::Sha(sha) => match ls_remote(&repo, &reff) {
            Ok(remote) if is_current(&sha, &remote) => {
                println!("{}", current_badge(&reff, style));
                return 0;
            }
            Ok(_) => {}
            Err(cause) => {
                eprintln!("{}", copy::update_unreachable(&repo, &reff, &cause));
                return 1;
            }
        },
        LocalBuild::Dirty(build_ref) => {
            println!("  {}", style.dim(&copy::update_dirty(&build_ref)));
        }
        LocalBuild::Unknown => println!("  {}", style.dim(copy::UPDATE_UNKNOWN)),
    }

    // 3. Runtime path, not test scratch, same as the installer's own
    //    mktemp under ${TMPDIR:-/tmp}.
    let scratch = match Scratch::create() {
        Ok(scratch) => scratch,
        Err(error) => {
            eprintln!("{}", copy::update_clone_failed(&repo, &reff, &error));
            return 1;
        }
    };
    if !scratch.path().is_dir() {
        eprintln!(
            "{}",
            copy::update_clone_failed(&repo, &reff, "secure update scratch disappeared")
        );
        return 1;
    }
    let src = scratch.path().join("cyclops");
    if let Err(cause) = clone(&repo, &reff, &src) {
        eprintln!("{}", copy::update_clone_failed(&repo, &reff, &cause));
        return 1;
    }
    // The clone's installer, streamed: it prints every file it touches,
    // and it is the one implementation of where binaries and home live.
    //
    // --prefix pins it to the directory the install already uses. With no
    // --prefix the installer re-picks from the current PATH, so a PATH
    // that changed since install writes the new build somewhere else,
    // leaves the old one in place, and every hook already wired keeps
    // invoking it. A stale build answering hooks is worse than a missing
    // one: the edges keep arriving, from a build the daemon no longer
    // matches. Where it lands is printed, because a prefix is the kind of
    // thing an operator only discovers is wrong much later.
    let mut installer = Command::new("sh");
    installer.arg(src.join("scripts").join("install.sh"));
    // Build into a directory that outlives the clone.
    //
    // The clone is a fresh temp dir every run, so cargo's target/ starts
    // empty and a one-commit change pays for the whole dependency tree,
    // roughly 130 crates. Then `Scratch` deletes the clone on the way out,
    // taking the build with it, so the next update starts cold as well.
    // install.sh already reads CARGO_TARGET_DIR; nothing ever set it.
    //
    // Pointing it somewhere persistent makes cargo do what cargo does and
    // recompile only what moved. An operator who set their own target dir
    // keeps it: they have already decided where builds go.
    let cache = build_cache(&cyclops_proto::cyclops_home());
    let mut held_cache = None;
    let mut held_cache_lease = None;
    if std::env::var_os("CARGO_TARGET_DIR").is_none() {
        match cyclops_state::StateRoot::open_or_create(&cache) {
            Ok(root) => match lock_build_cache(&root) {
                Ok(lease) => {
                    installer.env("CARGO_TARGET_DIR", root.path());
                    println!("  {}", style.dim(&copy::update_build_cache(root.path())));
                    held_cache_lease = Some(lease);
                    held_cache = Some(root);
                }
                Err(error) => {
                    eprintln!("  {}", style.dim(&format!("build cache held: {error}")));
                }
            },
            // Not fatal. A cache that cannot be made costs a slow build,
            // which is what every update did before this existed.
            Err(e) => {
                let error = std::io::Error::other(e.to_string());
                eprintln!(
                    "  {}",
                    style.dim(&copy::update_cache_unusable(&cache, &error))
                );
            }
        }
    }
    if let Some(prefix) = install_prefix() {
        println!(
            "  {}",
            style.dim(&format!("installing over {}", prefix.display()))
        );
        installer.arg("--prefix").arg(prefix);
    }
    // Pair activation is the installer's commit boundary. If later home
    // setup needs repair, the installer must report that committed state
    // instead of returning a generic failure that implies no update landed.
    installer.env("CYCLOPS_UPDATE_DRIVER", "1");
    match installer.status() {
        Ok(s) if s.success() => {}
        Ok(_) => {
            eprintln!("{}", copy::update_install_failed(None));
            return 1;
        }
        Err(e) => {
            eprintln!("{}", copy::update_install_failed(Some(&e.to_string())));
            return 1;
        }
    }
    drop(held_cache_lease);
    drop(held_cache);

    // 4.
    println!();
    match installed_cyclops().and_then(|p| version_of(&p)) {
        Some(new) => println!("{}", updated_badge(crate::VERSION, &new, style)),
        None => println!("  {}", style.dim(copy::UPDATE_UNRESOLVED)),
    }
    println!();

    println!("  {}", style.dim(WORKSPACE_NOTE));
    0
}

fn run_remove_pair_store(prefix: &Path) -> i32 {
    let daemon = match validate_uninstall_pair(prefix) {
        Ok(daemon) => daemon,
        Err(error) => {
            eprintln!("uninstall refused: {error}");
            return 1;
        }
    };
    let store = match PairStore::open_existing(prefix) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("uninstall refused: {error}");
            return 1;
        }
    };
    if let Err(refusal) = crate::daemon::stop_selected_for_pair_change(&daemon) {
        eprintln!("uninstall refused: {}", refusal.why());
        return 1;
    }
    if let Some(store) = store {
        if let Err(error) = store.remove_managed() {
            eprintln!("managed pair removal refused: {error}");
            1
        } else {
            0
        }
    } else {
        0
    }
}

/// Prove that the internal uninstall command is running from the selected
/// prefix and that both public names resolve to one owner-controlled build.
fn validate_uninstall_pair(prefix: &Path) -> Result<PathBuf, String> {
    let prefix = std::fs::canonicalize(prefix)
        .map_err(|error| format!("resolve install prefix {}: {error}", prefix.display()))?;
    require_install_prefix(&prefix)?;
    let public_client = prefix.join("cyclops");
    let public_daemon = prefix.join("cyclopsd");
    let client = std::fs::canonicalize(&public_client)
        .map_err(|error| format!("resolve {}: {error}", public_client.display()))?;
    let daemon = std::fs::canonicalize(&public_daemon)
        .map_err(|error| format!("resolve {}: {error}", public_daemon.display()))?;
    if !client.starts_with(&prefix) || !daemon.starts_with(&prefix) {
        return Err(format!(
            "the selected public binaries do not both belong to {}",
            prefix.display()
        ));
    }
    require_executable(&client)?;
    require_executable(&daemon)?;
    let running_client = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("resolve running cyclops executable: {error}"))?;
    if running_client != client {
        return Err(format!(
            "the validating client {} is not the selected {}",
            running_client.display(),
            client.display()
        ));
    }
    let client_build = candidate_build(&client)?;
    let daemon_build = candidate_build(&daemon)?;
    if client_build != daemon_build {
        return Err(format!(
            "selected client build {client_build} does not match daemon build {daemon_build}"
        ));
    }
    Ok(daemon)
}

fn run_install_pair(source: &Path, prefix: &Path, style: &Style) -> i32 {
    let scratch = match Scratch::create() {
        Ok(scratch) => scratch,
        Err(error) => {
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    let store = match PairStore::open(prefix) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    let candidate = match store.stage(source) {
        Ok(candidate) => candidate,
        Err(error) => {
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    let pair = match store.pair_path(&candidate) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    let build = match prove_candidate_replay(&pair, &cyclops_proto::cyclops_home(), &scratch) {
        Ok(build) => build,
        Err(error) => {
            let _ = store.discard(&candidate);
            eprintln!("install failed: candidate replay proof failed: {error}");
            return 1;
        }
    };
    let before_migration = match store.selection() {
        Ok(selection) => selection,
        Err(error) => {
            let _ = store.discard(&candidate);
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    let stop_result = match before_migration.as_ref() {
        Some(_) => match store.active_binary("cyclopsd") {
            Ok(daemon) => crate::daemon::stop_selected_for_pair_change(&daemon),
            Err(error) => Err(crate::daemon::RestartRefusal::Failed(error)),
        },
        None if prefix.join("cyclopsd").exists() => {
            crate::daemon::stop_selected_for_pair_change(&prefix.join("cyclopsd"))
        }
        None if crate::daemon::is_up() => Err(crate::daemon::RestartRefusal::Failed(
            "a daemon is running but this install prefix has no selected pair; nothing was stopped"
                .to_string(),
        )),
        None => Ok(None),
    };
    let stopped = match stop_result {
        Ok(stopped) => stopped,
        Err(refusal) => {
            let _ = store.discard(&candidate);
            eprintln!("install failed: {}", refusal.why());
            if matches!(refusal, crate::daemon::RestartRefusal::Predates) {
                eprintln!("  {GENERATION_MIGRATION}");
            }
            return 1;
        }
    };
    if let Err(error) = store.migrate_direct_pair(&candidate) {
        if stopped.is_some() {
            let restart = match before_migration.as_ref() {
                Some(_) => start_and_prove_selected(&store),
                None => start_pair_daemon(&prefix.join("cyclopsd")),
            };
            if let Err(restart_error) = restart {
                eprintln!("  previous daemon restart failed: {restart_error}");
            }
        }
        let _ = store.discard(&candidate);
        eprintln!("install failed during direct-pair migration: {error}");
        return 1;
    }
    let previous = match store.selection() {
        Ok(previous) => previous,
        Err(error) => {
            let _ = store.discard(&candidate);
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    if let Err(error) = store.activate(&candidate) {
        if let Some(previous) = previous.as_ref() {
            let _ = store.restore_selection(previous);
        }
        if stopped.is_some() {
            let _ = start_and_prove_selected(&store);
        }
        let _ = store.discard(&candidate);
        eprintln!("install failed: {error}");
        return 1;
    }
    if stopped.is_some() {
        let started = start_and_prove_selected(&store);
        if let Err(error) = started {
            let candidate_stopped = match store.active_binary("cyclopsd") {
                Ok(daemon) => crate::daemon::stop_selected_for_pair_change(&daemon),
                Err(error) => Err(crate::daemon::RestartRefusal::Failed(error)),
            };
            if matches!(candidate_stopped, Ok(Some(_)) | Ok(None)) {
                if let Some(previous) = previous.as_ref() {
                    let _ = store.restore_selection(previous);
                    let _ = start_and_prove_selected(&store);
                }
            }
            eprintln!("install failed: candidate daemon did not take over: {error}");
            if let Err(ref refusal) = candidate_stopped {
                eprintln!("  automatic rollback held: {}", refusal.why());
            }
            return 1;
        }
    }
    if let Err(error) = store.prune() {
        eprintln!(
            "  {}",
            style.dim(&format!("old pair cleanup held: {error}"))
        );
    }
    println!(
        "  {}",
        style.dim(&format!("activated matched pair {build}"))
    );
    if stopped.is_some() {
        println!(
            "  {}",
            style.dim("restarted the exact selected daemon on the new pair")
        );
    } else {
        println!(
            "  {}",
            style.dim("no daemon was running; the selected pair remains stopped")
        );
    }
    0
}

/// Prove the selected known-good pair can replay current daemon inputs before
/// any daemon stop or selector change. The pair-store lease keeps this
/// selection stable until rollback either commits or returns.
fn prove_selected_rollback_replay(
    store: &PairStore,
    source_home: &Path,
    scratch: &Scratch,
) -> Result<String, String> {
    let selection = store.rollback_selection()?;
    let proof = selection.known_good_proof.as_ref().ok_or_else(|| {
        "the known-good pair has no recorded build identity; run one update before rollback"
            .to_string()
    })?;
    let pair = store.pair_path(&selection.known_good)?;
    verify_recorded_pair(&pair, proof)?;
    let expected_build = identity_build(&proof.identity)?;
    let replayed_build = prove_candidate_replay(&pair, source_home, scratch)
        .map_err(|error| format!("known-good journal replay failed: {error}"))?;
    if replayed_build != expected_build {
        return Err(format!(
            "known-good replay reported build {replayed_build}, expected {expected_build}"
        ));
    }
    Ok(replayed_build)
}

fn run_rollback(style: &Style) -> i32 {
    let Some(prefix) = install_prefix() else {
        eprintln!("rollback failed: cyclops is not installed on PATH");
        return 1;
    };
    let store = match PairStore::open(&prefix) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("rollback failed: {error}");
            return 1;
        }
    };
    let scratch = match Scratch::create() {
        Ok(scratch) => scratch,
        Err(error) => {
            eprintln!("rollback failed: {error}");
            return 1;
        }
    };
    if let Err(error) =
        prove_selected_rollback_replay(&store, &cyclops_proto::cyclops_home(), &scratch)
    {
        eprintln!("rollback failed: {error}");
        return 1;
    }
    let selected_daemon = match store.active_binary("cyclopsd") {
        Ok(daemon) => daemon,
        Err(error) => {
            eprintln!("rollback failed: {error}");
            return 1;
        }
    };
    let stopped = match crate::daemon::stop_selected_for_pair_change(&selected_daemon) {
        Ok(stopped) => stopped,
        Err(refusal) => {
            eprintln!("rollback failed: {}", refusal.why());
            return 1;
        }
    };
    let (prior, restored) = match store.rollback() {
        Ok(swapped) => swapped,
        Err(error) => {
            if stopped.is_some() {
                let _ = start_and_prove_selected(&store);
            }
            eprintln!("rollback failed: {error}");
            return 1;
        }
    };
    if stopped.is_some() {
        let started = start_and_prove_selected(&store);
        if let Err(error) = started {
            let restored_stopped = match store.active_binary("cyclopsd") {
                Ok(daemon) => crate::daemon::stop_selected_for_pair_change(&daemon),
                Err(error) => Err(crate::daemon::RestartRefusal::Failed(error)),
            };
            if matches!(restored_stopped, Ok(Some(_)) | Ok(None)) {
                let _ = store.restore_selection(&prior);
                let _ = start_and_prove_selected(&store);
            }
            eprintln!("rollback failed: restored daemon did not start: {error}");
            if let Err(ref refusal) = restored_stopped {
                eprintln!("  selector restoration held: {}", refusal.why());
            }
            return 1;
        }
    }
    println!(
        "{}",
        style.bold(&format!("{} rolled back", render::check(true)))
    );
    println!("  {}", style.dim(&format!("active {}", restored.active)));
    println!("  {}", style.dim(&format!("known-good {}", prior.active)));
    0
}

fn start_pair_daemon(daemon: &Path) -> Result<(), String> {
    let build = candidate_build(daemon)?;
    start_pair_daemon_with_build(daemon, &build)
}

fn start_pair_daemon_with_build(daemon: &Path, build: &str) -> Result<(), String> {
    match crate::daemon::start_and_prove_from(&cyclops_proto::cyclops_home(), daemon, build)? {
        crate::daemon::Started::Spawned => Ok(()),
        crate::daemon::Started::AlreadyRunning => {
            Err("another daemon answered before the selected pair started".to_string())
        }
    }
}

fn start_and_prove_selected(store: &PairStore) -> Result<(), String> {
    let daemon = store.active_binary("cyclopsd")?;
    let cli_build = candidate_build(&store.active_binary("cyclops")?)?;
    let daemon_build = candidate_build(&daemon)?;
    if cli_build != daemon_build {
        return Err("selected CLI and daemon source builds do not match".to_string());
    }
    start_pair_daemon_with_build(&daemon, &cli_build)
}

const GENERATION_MIGRATION: &str =
    "the old daemon lacks process-generation identity; run `cyclops daemon stop` with the old CLI, then rerun update";

/// The one thing the update never restarts, and how to bring it over.
const WORKSPACE_NOTE: &str =
    "an open workspace stays on the old build until you detach (ctrl+b d) and run cyclops again";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(path: &Path) {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700).recursive(true);
        builder.create(path).unwrap();
    }

    fn pair_source(path: &Path, build: &str) {
        directory(path);
        write_new(
            &path.join("cyclops"),
            format!("#!/bin/sh\n[ \"$1\" = \"--version\" ] && echo 'cyclops 0.1.0 ({build})'\n")
                .as_bytes(),
            0o755,
        )
        .unwrap();
        write_new(
            &path.join("cyclopsd"),
            format!("#!/bin/sh\n[ \"$1\" = \"--version\" ] && echo 'cyclopsd 0.1.0 ({build})'\n")
                .as_bytes(),
            0o755,
        )
        .unwrap();
    }

    fn replay_rejecting_pair(path: &Path, build: &str, rejected_state: &str) {
        directory(path);
        write_new(
            &path.join("cyclops"),
            format!("#!/bin/sh\n[ \"$1\" = \"--version\" ] && echo 'cyclops 0.1.0 ({build})'\n")
                .as_bytes(),
            0o755,
        )
        .unwrap();
        write_new(
            &path.join("cyclopsd"),
            format!(
                "#!/usr/bin/env python3\nimport os, sys\nif len(sys.argv) > 1 and sys.argv[1] == '--version':\n    print('cyclopsd 0.1.0 ({build})')\n    sys.exit(0)\nhome = os.environ['CYCLOPS_HOME']\nif os.path.exists(os.path.join(home, '{rejected_state}')):\n    sys.exit(42)\nsys.exit(0)\n"
            )
            .as_bytes(),
            0o755,
        )
        .unwrap();
    }

    fn replay_exiting_pair(path: &Path, build: &str, body: &str) {
        directory(path);
        write_new(
            &path.join("cyclops"),
            format!("#!/bin/sh\n[ \"$1\" = \"--version\" ] && echo 'cyclops 0.1.0 ({build})'\n")
                .as_bytes(),
            0o755,
        )
        .unwrap();
        write_new(
            &path.join("cyclopsd"),
            format!(
                "#!/usr/bin/env python3\nimport os, sys\nif len(sys.argv) > 1 and sys.argv[1] == '--version':\n    print('cyclopsd 0.1.0 ({build})')\n    sys.exit(0)\n{body}\n"
            )
            .as_bytes(),
            0o755,
        )
        .unwrap();
    }

    fn pair_source_with_execution_probe(path: &Path, build: &str, probe: &Path) {
        directory(path);
        for name in ["cyclops", "cyclopsd"] {
            write_new(
                &path.join(name),
                format!(
                    "#!/bin/sh\ntouch '{}'\n[ \"$1\" = \"--version\" ] && echo '{name} 0.1.0 ({build})'\n",
                    probe.display()
                )
                .as_bytes(),
                0o755,
            )
            .unwrap();
        }
    }

    fn interrupted_pair(store: &PairStore, nonce: u8) -> PathBuf {
        let path = store
            .root
            .join(PAIRS_DIR)
            .join(format!("pair.{nonce:032x}"));
        directory(&path);
        path
    }

    fn replay_failure_pair(path: &Path, hello: &str, cli: &[u8]) {
        directory(path);
        write_new(&path.join("cyclops"), cli, 0o755).unwrap();
        let hello = serde_json::to_string(hello).unwrap();
        let script = format!(
            "#!/usr/bin/env python3\nimport os, socket, time\nhome = os.environ['CYCLOPS_HOME']\npath = os.path.join(home, '{}')\ntry:\n    os.unlink(path)\nexcept FileNotFoundError:\n    pass\ns = socket.socket(socket.AF_UNIX)\ns.bind(path)\nwith open(os.path.join(home, 'probe.pid'), 'w') as f:\n    f.write(str(os.getpid()))\ns.listen(1)\nc, _ = s.accept()\nc.sendall(({} + '\\n').encode())\nc.close()\ntime.sleep(60)\n",
            cyclops_proto::SOCK_NAME,
            hello
        );
        write_new(&path.join("cyclopsd"), script.as_bytes(), 0o755).unwrap();
    }

    fn assert_replay_probe_reaped(scratch: &Scratch) {
        let pid: i32 = std::fs::read_to_string(scratch.path().join("r/probe.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if alive {
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(!alive, "private replay daemon {pid} was not reaped");
    }

    fn valid_probe_hello(build: &str) -> String {
        serde_json::to_string(&cyclops_proto::Hello {
            cyclops: "0.1.0".to_string(),
            build: Some(build.to_string()),
            daemon_process: None,
            daemon_executable: None,
            proto: cyclops_proto::PROTOCOL_VERSION,
            boot_id: "probe-boot".to_string(),
        })
        .unwrap()
    }

    #[test]
    fn malformed_replay_hello_reaps_the_private_daemon() {
        let scratch = Scratch::create().unwrap();
        let pair = scratch.path().join("pair");
        replay_failure_pair(&pair, "{malformed", b"#!/bin/sh\nexit 1\n");
        let error =
            prove_candidate_replay(&pair, &scratch.path().join("absent"), &scratch).unwrap_err();
        assert!(
            error.contains("decode candidate hello"),
            "unexpected replay failure: {error}"
        );
        assert_replay_probe_reaped(&scratch);
    }

    #[test]
    fn failed_candidate_cli_identity_reaps_the_private_daemon() {
        let scratch = Scratch::create().unwrap();
        let pair = scratch.path().join("pair");
        replay_failure_pair(&pair, &valid_probe_hello("build"), b"#!/bin/sh\nexit 1\n");
        let error =
            prove_candidate_replay(&pair, &scratch.path().join("absent"), &scratch).unwrap_err();
        assert!(
            error.contains("--version failed"),
            "unexpected replay failure: {error}"
        );
        assert_replay_probe_reaped(&scratch);
    }

    #[test]
    fn failed_replay_stop_command_reaps_the_private_daemon() {
        let scratch = Scratch::create().unwrap();
        let pair = scratch.path().join("pair");
        replay_failure_pair(
            &pair,
            &valid_probe_hello("build"),
            b"#!/bin/sh\n[ \"$1\" = \"--version\" ] && { echo 'cyclops 0.1.0 (build)'; exit 0; }\nexit 1\n",
        );
        let error =
            prove_candidate_replay(&pair, &scratch.path().join("absent"), &scratch).unwrap_err();
        assert!(
            error.contains("could not stop"),
            "unexpected replay failure: {error}"
        );
        assert_replay_probe_reaped(&scratch);
    }

    #[test]
    fn candidate_replay_failure_surfaces_the_bounded_daemon_boot_cause() {
        let scratch = Scratch::create().unwrap();
        let pair = scratch.path().join("pair");
        replay_exiting_pair(
            &pair,
            "build",
            r#"home = os.environ['CYCLOPS_HOME']
path = os.path.join(home, 'cyclopsd.log')
descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(descriptor, 'wb') as log:
    log.write(b'x' * 9000 + b'\n')
    log.write(b'ERROR earlier-secret-must-not-surface\n')
    log.write(b'2026 ERROR cyclopsd: boot failed: replay-sentinel\x1b[31m\tunsafe\rfield ' + b'z' * 700 + b'\n')
sys.exit(42)"#,
        );

        let error =
            prove_candidate_replay(&pair, &scratch.path().join("absent"), &scratch).unwrap_err();

        assert!(error.contains("exit status: 42"), "{error}");
        assert!(error.contains("replay-sentinel"), "{error}");
        assert!(
            !error.contains("earlier-secret-must-not-surface"),
            "{error}"
        );
        assert!(!error.contains("no log output"), "{error}");
        assert!(
            !error
                .chars()
                .any(|character| matches!(character, '\u{1b}' | '\r' | '\t')),
            "{error:?}"
        );
        assert!(error.chars().count() <= 600, "diagnostic was not bounded");
    }

    #[test]
    fn candidate_replay_failure_falls_back_when_the_daemon_log_is_unreadable() {
        let scratch = Scratch::create().unwrap();
        let pair = scratch.path().join("pair");
        replay_exiting_pair(
            &pair,
            "build",
            r#"home = os.environ['CYCLOPS_HOME']
os.mkdir(os.path.join(home, 'cyclopsd.log'), 0o700)
sys.stderr.write('captured-replay-fallback\x1b[31m\tunsafe\rfield\n')
sys.exit(43)"#,
        );

        let error =
            prove_candidate_replay(&pair, &scratch.path().join("absent"), &scratch).unwrap_err();

        assert!(error.contains("exit status: 43"), "{error}");
        assert!(error.contains("captured-replay-fallback"), "{error}");
        assert!(!error.contains("no log output"), "{error}");
        assert!(
            !error
                .chars()
                .any(|character| matches!(character, '\u{1b}' | '\r' | '\t')),
            "{error:?}"
        );
    }

    #[test]
    fn path_resolution_skips_a_nonexecutable_shadow() {
        let scratch = Scratch::create().unwrap();
        let first = scratch.path().join("first");
        let second = scratch.path().join("second");
        directory(&first);
        directory(&second);
        write_new(&first.join("cyclops"), b"not executable\n", 0o600).unwrap();
        write_new(&second.join("cyclops"), b"#!/bin/sh\n", 0o700).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(which_in("cyclops", &path), Some(second.join("cyclops")));
    }

    #[test]
    fn build_cache_lease_is_exclusive_on_the_held_root() {
        let scratch = Scratch::create().unwrap();
        let cache = scratch.path().join("cache");
        let root = cyclops_state::StateRoot::open_or_create(&cache).unwrap();
        let held = lock_build_cache(&root).unwrap();
        let inherited = held.0.try_clone().unwrap();
        assert!(lock_build_cache(&root).unwrap_err().contains("in use"));
        drop(held);
        assert!(lock_build_cache(&root).is_ok());
        drop(inherited);
    }

    #[test]
    fn pair_store_drop_unlocks_an_inherited_descriptor() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let inherited = store._lease.0.try_clone().unwrap();

        drop(store);
        assert!(PairStore::open_existing(&prefix).unwrap().is_some());

        drop(inherited);
    }

    #[test]
    fn a_normal_stage_failure_removes_its_partial_pair_immediately() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("incomplete-source");
        directory(&source);
        write_new(&source.join("cyclops"), b"#!/bin/sh\n", 0o755).unwrap();

        assert!(store.stage(&source).is_err());
        assert!(read_directory(&store.root.join(PAIRS_DIR), "pair store")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_next_update_removes_empty_and_one_file_crash_residue() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let empty = interrupted_pair(&store, 1);
        let one_file = interrupted_pair(&store, 2);
        write_new(&one_file.join("cyclops"), b"#!/bin/sh\n", 0o755).unwrap();

        let source = scratch.path().join("candidate");
        pair_source(&source, "build");
        let candidate = store.stage(&source).unwrap();
        assert!(store.activate(&candidate).unwrap().is_none());
        store.prune().unwrap();

        assert!(!empty.exists());
        assert!(!one_file.exists());
        assert!(store.root.join(candidate).is_dir());
        assert!(store.active_binary("cyclops").unwrap().is_file());
    }

    #[test]
    fn unsafe_staging_residue_refuses_prune_before_safe_residue_changes() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("candidate");
        pair_source(&source, "build");
        let candidate = store.stage(&source).unwrap();
        let selection = store.prepare_selection(&candidate, &candidate).unwrap();
        store.select(&selection).unwrap();

        let safe = interrupted_pair(&store, 3);
        let linked = interrupted_pair(&store, 4);
        let outside = scratch.path().join("outside");
        write_new(&outside, b"outside\n", 0o755).unwrap();
        std::os::unix::fs::symlink(&outside, linked.join("cyclops")).unwrap();

        assert!(store.prune().unwrap_err().contains("linked"));
        assert!(safe.is_dir());
        assert!(linked.is_dir());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside\n");
    }

    #[test]
    fn multiply_linked_staging_residue_refuses_prune_without_mutation() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("candidate");
        pair_source(&source, "build");
        let candidate = store.stage(&source).unwrap();
        let selection = store.prepare_selection(&candidate, &candidate).unwrap();
        store.select(&selection).unwrap();

        let residue = interrupted_pair(&store, 8);
        let outside = scratch.path().join("outside-hard-link");
        write_new(&outside, b"outside\n", 0o755).unwrap();
        std::fs::hard_link(&outside, residue.join("cyclops")).unwrap();

        assert!(store.prune().unwrap_err().contains("linked"));
        assert!(residue.is_dir());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside\n");
        assert_eq!(std::fs::symlink_metadata(&outside).unwrap().nlink(), 2);
    }

    #[test]
    fn extra_staging_content_refuses_prune_without_removal() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("candidate");
        pair_source(&source, "build");
        let candidate = store.stage(&source).unwrap();
        let selection = store.prepare_selection(&candidate, &candidate).unwrap();
        store.select(&selection).unwrap();

        let residue = interrupted_pair(&store, 5);
        write_new(&residue.join("unexpected"), b"keep\n", 0o600).unwrap();
        assert!(store.prune().unwrap_err().contains("unmanaged entry"));
        assert_eq!(
            std::fs::read(residue.join("unexpected")).unwrap(),
            b"keep\n"
        );
    }

    #[test]
    fn managed_pair_removal_removes_valid_crash_residue() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("candidate");
        pair_source(&source, "build");
        let candidate = store.stage(&source).unwrap();
        let selection = store.prepare_selection(&candidate, &candidate).unwrap();
        store.select(&selection).unwrap();
        let empty = interrupted_pair(&store, 6);
        let one_file = interrupted_pair(&store, 7);
        write_new(&one_file.join("cyclopsd"), b"#!/bin/sh\n", 0o700).unwrap();
        let root = store.root.clone();

        store.remove_managed().unwrap();
        assert!(!empty.exists());
        assert!(!one_file.exists());
        assert!(!root.exists());
    }

    #[test]
    fn update_scratch_is_random_owner_only_marked_and_leased() {
        let first = Scratch::create().unwrap();
        let second = Scratch::create().unwrap();
        assert_ne!(first.path(), second.path());
        let metadata = std::fs::symlink_metadata(first.path()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::read_to_string(first.path().join(SCRATCH_MARKER)).unwrap(),
            first.marker
        );
        let competing = File::open(first.path().join(SCRATCH_LEASE)).unwrap();
        assert_ne!(
            unsafe { libc::flock(competing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "a second updater must not acquire the live lease"
        );
    }

    #[test]
    fn one_selector_activates_and_rolls_back_a_matched_pair() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("candidate");
        pair_source(&source, "new-build");
        let candidate = store.stage(&source).unwrap();
        store.migrate_direct_pair(&candidate).unwrap();
        let old = store.selection().unwrap().unwrap();
        assert_eq!(old.active, old.known_good);
        store.require_public_links().unwrap();

        assert_eq!(store.activate(&candidate).unwrap(), Some(old.clone()));
        let active = store.selection().unwrap().unwrap();
        assert_eq!(active.active, candidate);
        assert_eq!(active.known_good, old.active);

        let (prior, restored) = store.rollback().unwrap();
        assert_eq!(prior, active);
        assert_eq!(restored.active, old.active);
        assert_eq!(restored.known_good, candidate);
        assert_eq!(store.selection().unwrap(), Some(restored));
    }

    #[test]
    fn rollback_refuses_incompatible_current_journals_before_selector_change() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let old_source = scratch.path().join("old");
        let new_source = scratch.path().join("new");
        replay_rejecting_pair(&old_source, "old-build", "ledger/incompatible");
        pair_source(&new_source, "new-build");
        let old = store.stage(&old_source).unwrap();
        let new = store.stage(&new_source).unwrap();
        let old_selection = store.prepare_selection(&old, &old).unwrap();
        store.select(&old_selection).unwrap();
        store.replace_public_link("cyclopsd").unwrap();
        store.replace_public_link("cyclops").unwrap();
        store.activate(&new).unwrap();
        let before = store.selection().unwrap().unwrap();

        let home = scratch.path().join("current-home");
        let ledger = home.join("ledger");
        directory(&ledger);
        write_new(&ledger.join("incompatible"), b"journal generation\n", 0o600).unwrap();
        let replay_scratch = Scratch::create().unwrap();
        let error = prove_selected_rollback_replay(&store, &home, &replay_scratch).unwrap_err();

        assert!(error.contains("known-good journal replay failed"));
        assert!(error.contains("exit status: 42"));
        assert_eq!(store.selection().unwrap(), Some(before));
    }

    #[test]
    fn legacy_direct_pair_stays_executable_but_is_not_retained_as_known_good() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        std::fs::remove_file(prefix.join("cyclopsd")).unwrap();
        write_new(
            &prefix.join("cyclopsd"),
            b"#!/bin/sh\n[ \"$1\" = \"--version\" ] && echo 'cyclopsd 0.1.0'\n",
            0o755,
        )
        .unwrap();
        let old_cli = std::fs::read(prefix.join("cyclops")).unwrap();
        let old_daemon = std::fs::read(prefix.join("cyclopsd")).unwrap();
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("candidate");
        pair_source(&source, "new-build");
        let candidate = store.stage(&source).unwrap();

        store.migrate_direct_pair(&candidate).unwrap();
        let migrated = store.selection().unwrap().unwrap();
        assert!(migrated.legacy_active);
        assert_eq!(migrated.known_good, candidate);
        assert_eq!(std::fs::read(prefix.join("cyclops")).unwrap(), old_cli);
        assert_eq!(std::fs::read(prefix.join("cyclopsd")).unwrap(), old_daemon);

        store.activate(&candidate).unwrap();
        let active = store.selection().unwrap().unwrap();
        assert!(!active.legacy_active);
        assert_eq!(active.active, candidate);
        assert_eq!(active.known_good, candidate);
    }

    #[test]
    fn a_crash_between_known_good_and_active_keeps_the_old_pair_executable() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let old_source = scratch.path().join("old");
        let new_source = scratch.path().join("new");
        pair_source(&old_source, "old-build");
        pair_source(&new_source, "new-build");
        let old = store.stage(&old_source).unwrap();
        let new = store.stage(&new_source).unwrap();
        let old_selection = store.prepare_selection(&old, &old).unwrap();
        store.select(&old_selection).unwrap();

        let prepared = store.prepare_selection(&new, &old).unwrap();
        assert_eq!(store.selection().unwrap(), Some(old_selection));
        store.select(&prepared).unwrap();
        assert_eq!(store.selection().unwrap(), Some(prepared));
    }

    #[test]
    fn read_only_descriptor_proves_the_selected_rollback_pair() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let old_source = scratch.path().join("old");
        let new_source = scratch.path().join("new");
        let old_probe = scratch.path().join("old-executed");
        let new_probe = scratch.path().join("new-executed");
        pair_source_with_execution_probe(&old_source, "old-build", &old_probe);
        pair_source_with_execution_probe(&new_source, "new-build", &new_probe);
        let old = store.stage(&old_source).unwrap();
        let new = store.stage(&new_source).unwrap();
        let old_selection = store.prepare_selection(&old, &old).unwrap();
        store.select(&old_selection).unwrap();
        store.replace_public_link("cyclopsd").unwrap();
        store.replace_public_link("cyclops").unwrap();
        store.activate(&new).unwrap();
        drop(store);
        std::fs::remove_file(&old_probe).unwrap();
        std::fs::remove_file(&new_probe).unwrap();

        let descriptor = installed_pair_descriptor(&prefix).unwrap().unwrap();
        assert!(descriptor.rollback_safe);
        assert_eq!(
            descriptor.active_identity.as_deref(),
            Some("0.1.0 (new-build)")
        );
        assert_eq!(descriptor.known_good_identity, "0.1.0 (old-build)");
        assert_eq!(descriptor.active_build.as_deref(), Some("new-build"));
        assert_eq!(descriptor.known_good_build, "old-build");
        assert!(descriptor.selection.is_dir());
        assert!(descriptor.active_pair.is_dir());
        assert!(descriptor.known_good_pair.is_dir());
        assert!(
            !old_probe.exists(),
            "health proof executed the known-good pair"
        );
        assert!(!new_probe.exists(), "health proof executed the active pair");
    }

    #[test]
    fn read_only_descriptor_refuses_changed_pair_bytes_and_missing_proof() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();
        let source = scratch.path().join("candidate");
        pair_source(&source, "build");
        let pair = store.stage(&source).unwrap();
        let selection = store.prepare_selection(&pair, &pair).unwrap();
        store.select(&selection).unwrap();
        let pair_path = store.root.join(&pair).join("cyclops");
        let descriptor_path = store.root.join(&selection.id).join(PAIR_DESCRIPTOR);
        drop(store);

        OpenOptions::new()
            .append(true)
            .open(&pair_path)
            .unwrap()
            .write_all(b"# changed\n")
            .unwrap();
        let error = installed_pair_descriptor(&prefix).unwrap_err();
        assert!(error.contains("changed after its install proof"), "{error}");

        std::fs::write(&pair_path, std::fs::read(source.join("cyclops")).unwrap()).unwrap();
        let mut descriptor: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&descriptor_path).unwrap()).unwrap();
        descriptor["known_good_proof"] = serde_json::Value::Null;
        std::fs::write(&descriptor_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
        assert!(installed_pair_descriptor(&prefix)
            .unwrap_err()
            .contains("missing a recorded build identity"));
    }

    #[test]
    fn interrupted_direct_migration_repairs_only_matching_public_bytes() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        let store = PairStore::open(&prefix).unwrap();
        let old = store.stage(&prefix).unwrap();
        let selection = store.prepare_selection(&old, &old).unwrap();
        store.select(&selection).unwrap();

        store.replace_public_link("cyclopsd").unwrap();
        assert!(std::fs::symlink_metadata(prefix.join("cyclopsd"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(prefix.join("cyclops"))
            .unwrap()
            .is_file());

        store.migrate_direct_pair(&old).unwrap();
        store.require_public_links().unwrap();
        assert_eq!(store.selection().unwrap(), Some(selection));
    }

    #[test]
    fn interrupted_migration_refuses_an_unproven_regular_binary() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        let store = PairStore::open(&prefix).unwrap();
        let old = store.stage(&prefix).unwrap();
        let selection = store.prepare_selection(&old, &old).unwrap();
        store.select(&selection).unwrap();
        store.replace_public_link("cyclopsd").unwrap();
        std::fs::remove_file(prefix.join("cyclops")).unwrap();
        write_new(&prefix.join("cyclops"), b"#!/bin/sh\necho hostile\n", 0o700).unwrap();

        assert!(store
            .migrate_direct_pair(&old)
            .unwrap_err()
            .contains("does not match"));
        assert_eq!(
            std::fs::read(prefix.join("cyclops")).unwrap(),
            b"#!/bin/sh\necho hostile\n"
        );
        assert!(std::fs::symlink_metadata(prefix.join("cyclops"))
            .unwrap()
            .is_file());
    }

    #[test]
    fn interrupted_migration_refuses_an_external_public_link() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        let store = PairStore::open(&prefix).unwrap();
        let old = store.stage(&prefix).unwrap();
        let selection = store.prepare_selection(&old, &old).unwrap();
        store.select(&selection).unwrap();
        store.replace_public_link("cyclopsd").unwrap();
        let outside = scratch.path().join("outside");
        write_new(&outside, b"outside\n", 0o700).unwrap();
        std::fs::remove_file(prefix.join("cyclops")).unwrap();
        std::os::unix::fs::symlink(&outside, prefix.join("cyclops")).unwrap();

        assert!(store
            .migrate_direct_pair(&old)
            .unwrap_err()
            .contains("outside the pair store"));
        assert_eq!(std::fs::read_link(prefix.join("cyclops")).unwrap(), outside);
    }

    #[test]
    fn managed_pair_removal_refuses_unknown_entries_before_mutating() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        let store = PairStore::open(&prefix).unwrap();
        let candidate = store.stage(&prefix).unwrap();
        store.migrate_direct_pair(&candidate).unwrap();
        write_new(&store.root.join("unknown"), b"keep\n", 0o600).unwrap();
        let root = store.root.clone();

        assert!(store
            .remove_managed()
            .unwrap_err()
            .contains("unmanaged entry"));
        assert_eq!(std::fs::read(root.join("unknown")).unwrap(), b"keep\n");
        assert!(root.exists());
    }

    #[test]
    fn managed_pair_removal_deletes_only_the_validated_schema() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        let store = PairStore::open(&prefix).unwrap();
        let candidate = store.stage(&prefix).unwrap();
        store.migrate_direct_pair(&candidate).unwrap();
        let root = store.root.clone();

        store.remove_managed().unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn staged_pairs_refuse_symlinks_and_multiply_linked_binaries() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let store = PairStore::open(&prefix).unwrap();

        let symlinked = scratch.path().join("symlinked");
        pair_source(&symlinked, "link-build");
        std::fs::remove_file(symlinked.join("cyclops")).unwrap();
        std::os::unix::fs::symlink("cyclopsd", symlinked.join("cyclops")).unwrap();
        assert!(store.stage(&symlinked).unwrap_err().contains("linked"));

        let hardlinked = scratch.path().join("hardlinked");
        pair_source(&hardlinked, "hard-build");
        let alias = scratch.path().join("outside-link");
        std::fs::hard_link(hardlinked.join("cyclops"), &alias).unwrap();
        assert!(store.stage(&hardlinked).unwrap_err().contains("linked"));

        let writable = scratch.path().join("writable");
        pair_source(&writable, "writable-build");
        std::fs::set_permissions(
            writable.join("cyclopsd"),
            std::fs::Permissions::from_mode(0o775),
        )
        .unwrap();
        assert!(store.stage(&writable).unwrap_err().contains("linked"));

        let owner_not_executable = scratch.path().join("owner-not-executable");
        pair_source(&owner_not_executable, "mode-build");
        std::fs::set_permissions(
            owner_not_executable.join("cyclopsd"),
            std::fs::Permissions::from_mode(0o055),
        )
        .unwrap();
        assert!(store
            .stage(&owner_not_executable)
            .unwrap_err()
            .contains("not executable by its owner"));
    }

    #[test]
    fn same_version_different_builds_are_not_a_matched_pair() {
        let scratch = Scratch::create().unwrap();
        let source = scratch.path().join("mixed");
        pair_source(&source, "cli-build");
        std::fs::remove_file(source.join("cyclopsd")).unwrap();
        write_new(
            &source.join("cyclopsd"),
            b"#!/bin/sh\n[ \"$1\" = \"--version\" ] && echo 'cyclopsd 0.1.0 (daemon-build)'\n",
            0o755,
        )
        .unwrap();

        let error = prove_pair_identity(&source).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        assert!(error.contains("cli-build"), "{error}");
        assert!(error.contains("daemon-build"), "{error}");
    }

    #[test]
    fn pair_store_refuses_linked_roots_markers_and_external_selectors() {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);
        let external = scratch.path().join("external-store");
        directory(&external);
        std::os::unix::fs::symlink(&external, prefix.join(PAIR_ROOT)).unwrap();
        assert!(PairStore::open(&prefix)
            .err()
            .unwrap()
            .contains("owner-only"));
        std::fs::remove_file(prefix.join(PAIR_ROOT)).unwrap();

        let store = PairStore::open(&prefix).unwrap();
        let marker_alias = scratch.path().join("marker-alias");
        std::fs::hard_link(store.root.join(PAIR_OWNER), &marker_alias).unwrap();
        assert!(store.require_root().unwrap_err().contains("linked"));
        std::fs::remove_file(marker_alias).unwrap();

        let source = scratch.path().join("candidate");
        pair_source(&source, "build");
        let pair = store.stage(&source).unwrap();
        let selection = store.prepare_selection(&pair, &pair).unwrap();
        store.select(&selection).unwrap();
        std::fs::remove_file(store.root.join(ACTIVE_SELECTOR)).unwrap();
        std::os::unix::fs::symlink("../outside", store.root.join(ACTIVE_SELECTOR)).unwrap();
        assert!(store
            .selection()
            .unwrap_err()
            .contains("invalid pair selection"));
    }

    #[test]
    fn replay_snapshot_omits_non_boot_artifacts_and_refuses_oversized_state() {
        let scratch = Scratch::create().unwrap();
        let source = scratch.path().join("state");
        let destination = scratch.path().join("copy");
        directory(&source);
        write_new(&source.join("config.toml"), b"sessions = []\n", 0o600).unwrap();
        write_new(&source.join("cyclopsd.log"), b"private log\n", 0o600).unwrap();
        directory(&source.join("cache"));
        write_new(&source.join("cache/artifact"), b"build output\n", 0o600).unwrap();

        copy_replay_state(&source, &destination).unwrap();
        assert!(destination.join("config.toml").is_file());
        assert!(!destination.join("cyclopsd.log").exists());
        assert!(!destination.join("cache").exists());

        let oversized_source = scratch.path().join("oversized");
        let oversized_copy = scratch.path().join("oversized-copy");
        directory(&oversized_source.join("ledger"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(oversized_source.join("ledger/main.ndjson"))
            .unwrap();
        file.set_len(MAX_REPLAY_FILE_BYTES + 1).unwrap();
        assert!(copy_replay_state(&oversized_source, &oversized_copy)
            .unwrap_err()
            .contains("byte bound"));
    }

    /// The three shapes build.rs stamps, and nothing else. `.dirty` and
    /// `unknown` must never reach the sha compare: the first can never
    /// match and the second has nothing to match with.
    #[test]
    fn build_refs_classify_into_the_three_stamped_shapes() {
        assert_eq!(classify("e610afc"), LocalBuild::Sha("e610afc".into()));
        assert_eq!(
            classify("e610afc.dirty"),
            LocalBuild::Dirty("e610afc.dirty".into())
        );
        assert_eq!(classify("unknown"), LocalBuild::Unknown);
    }

    /// Prefix match against the remote's full sha, because the baked side
    /// is `--short` and its length is git's choice, not ours.
    #[test]
    fn currency_is_a_prefix_match_on_the_short_sha() {
        let remote = "e610afc0123456789abcdef0123456789abcdef0";
        assert!(is_current("e610afc", remote));
        assert!(is_current("e610afc012", remote));
        assert!(!is_current("a1b2c3d", remote));
        // A sha longer than the remote's cannot match, and an empty local
        // sha must never read as current.
        assert!(!is_current(&"e".repeat(41), remote));
        assert!(!is_current("", remote));
        // A .dirty ref never reaches this compare, but if one did the
        // suffix keeps it from matching.
        assert!(!is_current("e610afc.dirty", remote));
    }

    #[test]
    fn the_report_strips_the_command_name_from_a_version_line() {
        assert_eq!(version_words("cyclops 0.1.0 (e610afc)"), "0.1.0 (e610afc)");
        // A shape from some other build is passed through rather than
        // half-eaten.
        assert_eq!(version_words("0.2.0 (abc1234)"), "0.2.0 (abc1234)");
    }

    #[test]
    fn the_badges_read_plain() {
        let plain = Style::none();
        assert_eq!(
            current_badge("main", &plain),
            "✔ already the latest main · nothing to update"
        );
        assert_eq!(
            updated_badge("0.1.0 (a1b2c3d)", "0.1.0 (e4f5a6b)", &plain),
            "✔ updated · 0.1.0 (a1b2c3d) → 0.1.0 (e4f5a6b)"
        );
    }
}
