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
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::copy;
use crate::hash::fnv64;
use crate::render;
use crate::style::Style;

pub(crate) mod pair_store;
#[cfg(test)]
mod tests;

pub(crate) use pair_store::*;

/// The installer's defaults (scripts/install.sh:26-27), restated because
/// the binary cannot read that file at runtime: change them together.
const DEFAULT_REPO: &str = "https://github.com/cyclops-team/cyclops.git";
const DEFAULT_REF: &str = "main";

/// The commit baked into every Cyclops component by the shared build stamp.
const BUILD_REF: &str = cyclops_proto::BUILD_REF;

/// What the baked build ref can say about a freshness check.
#[derive(Debug, PartialEq)]
pub(crate) enum LocalBuild {
    /// A clean commit: comparable to the remote by sha prefix.
    Sha(String),
    /// Built from edited sources; no remote commit can match it.
    Dirty(String),
    /// Built outside git (a source tarball); there is nothing to compare.
    Unknown,
}

/// build.rs stamps exactly three shapes: `<short-sha>`, `<short-sha>.dirty`,
/// or the literal `unknown`.
pub(crate) fn classify(build_ref: &str) -> LocalBuild {
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
pub(crate) fn is_current(local_short: &str, remote_sha: &str) -> bool {
    !local_short.is_empty()
        && remote_sha.len() >= local_short.len()
        && remote_sha.starts_with(local_short)
}

/// The commit `reff` names at `repo`, asked of the remote itself. One
/// cheap round trip, no clone, no checkout.
pub(crate) fn ls_remote(repo: &str, reff: &str) -> Result<String, String> {
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
pub(crate) fn clone(repo: &str, reff: &str, dest: &Path) -> Result<(), String> {
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

/// The visible, user-owned root for retained update build artifacts.
///
/// macOS has a per-process temporary directory under `/private/var`; it is a
/// bad home for a multi-gigabyte cache that persists after an update. Keep the
/// cache in the platform cache location instead, where an operator can inspect
/// The visible, user-owned root for retained update build artifacts.
///
/// macOS has a per-process temporary directory under `/private/var`; it is a
/// bad home for a multi-gigabyte cache that persists after an update. Keep the
/// cache in the platform cache location instead, where an operator can inspect
/// or remove it without hunting through system temporary storage.
pub(crate) fn build_cache_parent(home: &Path) -> PathBuf {
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| home.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| home.to_path_buf());
    #[cfg(target_os = "macos")]
    {
        user_home.join("Library/Caches/Cyclops")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home.join(".cache"))
            .join("cyclops")
    }
}

/// Where update keeps its build cache, so a rebuild is incremental.
///
/// Outside the state root because Cargo writes executable build artifacts.
pub(crate) fn build_cache(home: &Path) -> PathBuf {
    let home_key = fnv64(home.as_os_str().as_bytes());
    build_cache_parent(home).join(format!("build-{home_key}"))
}

/// Open the dedicated cache parent before its build-cache child. The standard
/// platform cache directories may not exist on a minimal account, so create
/// their ordinary ancestors first, then let `StateRoot` verify and own the
/// Cyclops leaf without following links.
pub(crate) fn open_build_cache(home: &Path) -> Result<cyclops_state::StateRoot, String> {
    let cache = build_cache(home);
    let parent = cache
        .parent()
        .ok_or_else(|| format!("build cache {} has no parent", cache.display()))?;
    let ancestors = parent
        .parent()
        .ok_or_else(|| format!("build cache parent {} has no parent", parent.display()))?;
    std::fs::create_dir_all(ancestors)
        .map_err(|error| format!("create build-cache parent {}: {error}", ancestors.display()))?;
    cyclops_state::StateRoot::open_or_create(parent)
        .map_err(|error| format!("open build-cache parent {}: {error}", parent.display()))?;
    cyclops_state::StateRoot::open_or_create(&cache)
        .map_err(|error| format!("open build cache {}: {error}", cache.display()))
}

/// The build-cache lease shared by update and cleanup.
pub(crate) const BUILD_CACHE_LEASE: &str = ".lease";

/// A held advisory lock that is released before its descriptor is closed.
pub(crate) struct ExclusiveLease(pub(crate) File);

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
pub(crate) struct Scratch {
    path: PathBuf,
    marker: String,
    device: u64,
    inode: u64,
    marker_device: u64,
    marker_inode: u64,
    _lease: ExclusiveLease,
}

impl Scratch {
    pub(crate) fn create() -> Result<Self, String> {
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

    pub(crate) fn path(&self) -> &Path {
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

pub(crate) fn random_hex() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("read system randomness: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
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
pub(crate) fn installed_cyclops() -> Option<PathBuf> {
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
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    which_in(name, &path)
}

pub(crate) fn which_in(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|d| d.join(name))
        .find(|p| is_executable_file(p))
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
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
pub(crate) fn install_prefix() -> Option<PathBuf> {
    installed_cyclops()?.parent().map(Path::to_path_buf)
}

/// `<bin> --version` with the leading command name stripped, so the
/// report reads `0.1.0 (e610afc)` on both sides of the arrow.
pub(crate) fn version_of(bin: &Path) -> Option<String> {
    let out = version_output(bin).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(version_words(&text))
}

pub(crate) fn version_words(version_line: &str) -> String {
    version_line
        .strip_prefix("cyclops ")
        .unwrap_or(version_line)
        .to_string()
}

/// The already-current badge. Heavy check by render::check's rule: the
/// remote owns the fact and just answered for it.
pub(crate) fn current_badge(reff: &str, style: &Style) -> String {
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
pub(crate) fn updated_badge(old: &str, new: &str, style: &Style) -> String {
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
#[allow(clippy::too_many_arguments)]
pub fn run(
    json: bool,
    style: &Style,
    rollback: bool,
    install_pair: Option<&Path>,
    remove_pair_store: bool,
    stop_selected_daemon: bool,
    remove_integrations: bool,
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
    if stop_selected_daemon {
        let Some(prefix) = prefix else {
            eprintln!("--stop-selected-daemon requires --prefix");
            return crate::EXIT_USAGE;
        };
        return run_stop_selected_daemon(prefix);
    }
    if remove_integrations {
        let Some(prefix) = prefix else {
            eprintln!("--remove-integrations requires --prefix");
            return crate::EXIT_USAGE;
        };
        return run_remove_integrations(prefix);
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
        match open_build_cache(&cyclops_proto::cyclops_home()) {
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

pub(crate) fn run_remove_pair_store(prefix: &Path) -> i32 {
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

/// Stop the selected daemon without changing the selected pair. The installer
/// uses this before invoking the separately journaled complete-state remover;
/// it must keep the client executable available until that remover finishes.
pub(crate) fn run_stop_selected_daemon(prefix: &Path) -> i32 {
    let daemon = match validate_uninstall_pair(prefix) {
        Ok(daemon) => daemon,
        Err(error) => {
            eprintln!("uninstall refused: {error}");
            return 1;
        }
    };
    match crate::daemon::stop_selected_for_pair_change(&daemon) {
        Ok(Some(pid)) => {
            println!("stopped selected cyclopsd pid {pid}");
            0
        }
        Ok(None) => {
            println!("no selected cyclopsd was running");
            0
        }
        Err(refusal) => {
            eprintln!("uninstall refused: {}", refusal.why());
            1
        }
    }
}

/// Remove only the vendor configuration and agent instructions that Cyclops
/// itself can prove it owns. This is intentionally an installer-only step:
/// state removal remains a separately confirmed operation.
pub(crate) fn run_remove_integrations(prefix: &Path) -> i32 {
    if let Err(error) = validate_uninstall_pair(prefix) {
        eprintln!("uninstall refused: {error}");
        return 1;
    }
    let mut failed = false;
    for kind in [
        crate::hookset::CliKind::Claude,
        crate::hookset::CliKind::Codex,
        crate::hookset::CliKind::Agy,
        crate::hookset::CliKind::Cursor,
    ] {
        match crate::hookset::remove_vendor_wiring(kind) {
            Ok(Some(result)) if result.removed => {
                println!(
                    "removed Cyclops {} hooks from {}",
                    result.vendor,
                    result.path.display()
                );
            }
            Ok(_) => {}
            Err(error) => {
                failed = true;
                eprintln!("uninstall left vendor configuration unchanged: {error}");
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        for result in crate::skillseed::remove_owned(&home) {
            match result.outcome {
                crate::skillseed::RemovalOutcome::Removed => {
                    println!(
                        "removed Cyclops {} skill from {}",
                        result.consumer,
                        result.path.display()
                    );
                }
                crate::skillseed::RemovalOutcome::Problem(detail) => {
                    failed = true;
                    eprintln!("uninstall left skill unchanged: {detail}");
                }
                crate::skillseed::RemovalOutcome::Missing
                | crate::skillseed::RemovalOutcome::Kept => {}
            }
        }
    }
    i32::from(failed)
}

/// Prove that the internal uninstall command is running from the selected
/// prefix and that both public names resolve to one owner-controlled build.
pub(crate) fn validate_uninstall_pair(prefix: &Path) -> Result<PathBuf, String> {
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
    let client_identity = candidate_identity(&client, "cyclops")?;
    let daemon_identity = candidate_identity(&daemon, "cyclopsd")?;
    if client_identity != daemon_identity {
        return Err(format!(
            "selected client identity {} does not match daemon identity {}",
            client_identity.description(),
            daemon_identity.description()
        ));
    }
    Ok(daemon)
}

pub(crate) fn restart_pre_activation_pair(
    store: &PairStore,
    prefix: &Path,
    selected: Option<&Selection>,
) -> Result<(), String> {
    if selected.is_some() {
        start_and_prove_selected(store)
    } else {
        start_pair_daemon(&prefix.join("cyclopsd"))
    }
}

/// What recovery actually completed after a pair-change error.
/// The caller reports these facts beside the original error instead of
/// collapsing a visible selector change into a generic install failure.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PairChangeRecovery {
    pub(crate) prior_selector_restored: bool,
    pub(crate) prior_daemon_restarted: bool,
}

/// Restore the exact prior selector before any daemon restart. A failed
/// directory sync after rename leaves the selector observable but uncertain;
/// in that case a second selector change must be confirmed before a daemon is
/// allowed to start.
pub(crate) fn recover_prior_pair_after_change_failure(
    store: &PairStore,
    fallback_previous: Option<&Selection>,
    failure: &PairChangeError,
    daemon_was_stopped: bool,
    restart: impl FnOnce() -> Result<(), String>,
) -> Result<PairChangeRecovery, String> {
    let prior_selector_restored = if failure.selector_is_visible() {
        let previous = failure.previous().or(fallback_previous).ok_or_else(|| {
            "the new selector is visible, but there is no earlier selection to restore; do not start a daemon automatically"
                .to_string()
        })?;
        store
            .restore_selection(previous)
            .map_err(|error| format!("selector recovery held: {error}"))?;
        true
    } else {
        false
    };
    let prior_daemon_restarted = if daemon_was_stopped {
        restart().map_err(|error| format!("previous daemon restart failed: {error}"))?;
        true
    } else {
        false
    };
    Ok(PairChangeRecovery {
        prior_selector_restored,
        prior_daemon_restarted,
    })
}

pub(crate) fn report_pair_change_recovery(recovery: &PairChangeRecovery) {
    match (
        recovery.prior_selector_restored,
        recovery.prior_daemon_restarted,
    ) {
        (true, true) => {
            eprintln!("  restored the prior selector and restarted its exact daemon");
        }
        (true, false) => {
            eprintln!("  restored the prior selector; no daemon had been running");
        }
        (false, true) => {
            eprintln!("  the selector was unchanged; restarted its exact daemon");
        }
        (false, false) => {}
    }
}

pub(crate) fn discard_unselected_candidate(store: &PairStore, candidate: &str) {
    if let Err(error) = store.discard(candidate) {
        eprintln!("  staged candidate cleanup held: {error}");
    }
}

pub(crate) fn run_install_pair(source: &Path, prefix: &Path, style: &Style) -> i32 {
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
    let replay = match prove_candidate_replay(&pair, &cyclops_proto::cyclops_home(), &scratch) {
        Ok(replay) => replay,
        Err(error) => {
            if stopped.is_some() {
                if let Err(restart_error) =
                    restart_pre_activation_pair(&store, prefix, before_migration.as_ref())
                {
                    eprintln!("  previous daemon restart failed: {restart_error}");
                }
            }
            let _ = store.discard(&candidate);
            eprintln!("install failed: candidate replay proof failed: {error}");
            return 1;
        }
    };
    let build = match identity_build(&replay.pair.identity) {
        Ok(build) => build,
        Err(error) => {
            if stopped.is_some() {
                if let Err(restart_error) =
                    restart_pre_activation_pair(&store, prefix, before_migration.as_ref())
                {
                    eprintln!("  previous daemon restart failed: {restart_error}");
                }
            }
            let _ = store.discard(&candidate);
            eprintln!("install failed: candidate replay proof failed: {error}");
            return 1;
        }
    };
    if let Err(error) = store.migrate_direct_pair(&candidate) {
        eprintln!("install did not finish during direct-pair migration: {error}");
        match recover_prior_pair_after_change_failure(
            &store,
            before_migration.as_ref(),
            &error,
            stopped.is_some(),
            || restart_pre_activation_pair(&store, prefix, before_migration.as_ref()),
        ) {
            Ok(recovery) => {
                report_pair_change_recovery(&recovery);
                discard_unselected_candidate(&store, &candidate);
            }
            Err(recovery_error) => {
                eprintln!("  recovery held: {recovery_error}");
                eprintln!("  retained staged candidate {candidate} for inspection");
            }
        }
        return 1;
    }
    let previous = match store.selection() {
        Ok(previous) => previous,
        Err(error) => {
            if stopped.is_some() {
                if let Err(restart_error) =
                    restart_pre_activation_pair(&store, prefix, before_migration.as_ref())
                {
                    eprintln!("  previous daemon restart failed: {restart_error}");
                }
            }
            let _ = store.discard(&candidate);
            eprintln!("install failed: {error}");
            return 1;
        }
    };
    if let Err(error) = store.activate(&candidate, replay) {
        eprintln!("install did not finish: {error}");
        match recover_prior_pair_after_change_failure(
            &store,
            previous.as_ref(),
            &error,
            stopped.is_some(),
            || restart_pre_activation_pair(&store, prefix, previous.as_ref()),
        ) {
            Ok(recovery) => {
                report_pair_change_recovery(&recovery);
                discard_unselected_candidate(&store, &candidate);
            }
            Err(recovery_error) => {
                eprintln!("  recovery held: {recovery_error}");
                eprintln!("  retained staged candidate {candidate} for inspection");
            }
        }
        return 1;
    }
    if stopped.is_some() {
        let started = start_and_prove_selected(&store);
        if let Err(error) = started {
            eprintln!("install did not finish: candidate daemon did not take over: {error}");
            let candidate_stopped = match store.active_binary("cyclopsd") {
                Ok(daemon) => crate::daemon::stop_selected_for_pair_change(&daemon),
                Err(error) => Err(crate::daemon::RestartRefusal::Failed(error)),
            };
            if matches!(candidate_stopped, Ok(Some(_)) | Ok(None)) {
                match store.selection() {
                    Ok(Some(selection)) => {
                        let failure = PairChangeError::after_visible_selector(
                            previous.clone(),
                            selection,
                            format!("candidate daemon did not take over: {error}"),
                        );
                        match recover_prior_pair_after_change_failure(
                            &store,
                            previous.as_ref(),
                            &failure,
                            true,
                            || restart_pre_activation_pair(&store, prefix, previous.as_ref()),
                        ) {
                            Ok(recovery) => report_pair_change_recovery(&recovery),
                            Err(recovery_error) => {
                                eprintln!("  automatic rollback held: {recovery_error}");
                            }
                        }
                    }
                    Ok(None) => {
                        eprintln!("  automatic rollback held: the active selector disappeared");
                    }
                    Err(selection_error) => {
                        eprintln!(
                            "  automatic rollback held: inspect active selector: {selection_error}"
                        );
                    }
                }
            }
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

/// Prove the selected known-good pair can replay the quiesced daemon inputs
/// before any selector change. The pair-store lease keeps this selection
/// stable until rollback either commits or returns.
pub(crate) fn prove_selected_rollback_replay(
    store: &PairStore,
    source_home: &Path,
    scratch: &Scratch,
) -> Result<ReplayAttestation, String> {
    let selection = store.rollback_selection()?;
    let proof = selection.known_good_proof.as_ref().ok_or_else(|| {
        "the known-good pair has no recorded build identity; run one update before rollback"
            .to_string()
    })?;
    let pair = store.pair_path(&selection.known_good)?;
    verify_recorded_pair(&pair, proof)?;
    let replay = prove_candidate_replay(&pair, source_home, scratch)
        .map_err(|error| format!("known-good journal replay failed: {error}"))?;
    if replay.pair.identity != proof.identity {
        return Err(format!(
            "known-good replay reported identity {:?}, expected {:?}",
            replay.pair.identity, proof.identity
        ));
    }
    Ok(replay)
}

pub(crate) fn run_rollback(style: &Style) -> i32 {
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
    let replay =
        match prove_selected_rollback_replay(&store, &cyclops_proto::cyclops_home(), &scratch) {
            Ok(replay) => replay,
            Err(error) => {
                if stopped.is_some() {
                    if let Err(restart_error) = start_and_prove_selected(&store) {
                        eprintln!("  previous daemon restart failed: {restart_error}");
                    }
                }
                eprintln!("rollback failed: {error}");
                return 1;
            }
        };
    let (prior, restored) = match store.rollback(replay) {
        Ok(swapped) => swapped,
        Err(error) => {
            eprintln!("rollback did not finish: {error}");
            match recover_prior_pair_after_change_failure(
                &store,
                None,
                &error,
                stopped.is_some(),
                || start_and_prove_selected(&store),
            ) {
                Ok(recovery) => report_pair_change_recovery(&recovery),
                Err(recovery_error) => eprintln!("  recovery held: {recovery_error}"),
            }
            return 1;
        }
    };
    if stopped.is_some() {
        let started = start_and_prove_selected(&store);
        if let Err(error) = started {
            eprintln!("rollback did not finish: restored daemon did not start: {error}");
            let restored_stopped = match store.active_binary("cyclopsd") {
                Ok(daemon) => crate::daemon::stop_selected_for_pair_change(&daemon),
                Err(error) => Err(crate::daemon::RestartRefusal::Failed(error)),
            };
            if matches!(restored_stopped, Ok(Some(_)) | Ok(None)) {
                match store.selection() {
                    Ok(Some(selection)) => {
                        let failure = PairChangeError::after_visible_selector(
                            Some(prior.clone()),
                            selection,
                            format!("restored daemon did not start: {error}"),
                        );
                        match recover_prior_pair_after_change_failure(
                            &store,
                            Some(&prior),
                            &failure,
                            true,
                            || start_and_prove_selected(&store),
                        ) {
                            Ok(recovery) => report_pair_change_recovery(&recovery),
                            Err(recovery_error) => {
                                eprintln!("  selector restoration held: {recovery_error}");
                            }
                        }
                    }
                    Ok(None) => {
                        eprintln!("  selector restoration held: the active selector disappeared");
                    }
                    Err(selection_error) => {
                        eprintln!("  selector restoration held: inspect active selector: {selection_error}");
                    }
                }
            }
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

pub(crate) fn start_pair_daemon(daemon: &Path) -> Result<(), String> {
    let directory = daemon
        .parent()
        .ok_or_else(|| format!("selected daemon {} has no pair directory", daemon.display()))?;
    let identity = prove_pair_identity(directory)?;
    let identity = cyclops_client::RuntimeIdentity::parse(&identity)
        .ok_or_else(|| "selected pair has an invalid runtime identity".to_string())?;
    start_pair_daemon_with_identity(daemon, &identity)
}

pub(crate) fn start_pair_daemon_with_identity(
    daemon: &Path,
    identity: &cyclops_client::RuntimeIdentity,
) -> Result<(), String> {
    match crate::daemon::start_and_prove_from(&cyclops_proto::cyclops_home(), daemon, identity)? {
        crate::daemon::Started::Spawned => Ok(()),
        crate::daemon::Started::AlreadyRunning => {
            Err("another daemon answered before the selected pair started".to_string())
        }
    }
}

pub(crate) fn start_and_prove_selected(store: &PairStore) -> Result<(), String> {
    let cli = store.active_binary("cyclops")?;
    let daemon = store.active_binary("cyclopsd")?;
    let cli_identity = candidate_identity(&cli, "cyclops")?;
    let daemon_identity = candidate_identity(&daemon, "cyclopsd")?;
    if cli_identity != daemon_identity {
        return Err(format!(
            "selected CLI identity {} does not match daemon identity {}",
            cli_identity.description(),
            daemon_identity.description()
        ));
    }
    start_pair_daemon_with_identity(&daemon, &cli_identity)
}

const GENERATION_MIGRATION: &str =
    "the old daemon lacks process-generation identity; run `cyclops daemon stop` with the old CLI, then rerun update";

/// The one thing the update never restarts, and how to bring it over.
const WORKSPACE_NOTE: &str =
    "an open workspace stays on the old build until you detach (ctrl+b d) and run cyclops again";

pub(crate) fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}
