//! Owner-only state behavior through the daemon's public boot path.

use crate::common;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{symlink, FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use cyclops_state::StateRoot;
use cyclopsd::Config;

use common::{tmux_available, Rig, CAT_MANIFEST};

const CHILD_HOME: &str = "CYCLOPS_PERMISSION_TEST_HOME";

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = cyclops_proto::scratch::scratch_dir(tag);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create permission scratch root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("state metadata")
        .permissions()
        .mode()
        & 0o777
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set fixture mode");
}

fn snapshot(path: &Path) -> (Vec<u8>, u32, u64, u64) {
    let metadata = fs::metadata(path).expect("snapshot metadata");
    (
        fs::read(path).expect("snapshot bytes"),
        mode(path),
        metadata.dev(),
        metadata.ino(),
    )
}

/// Check every entry produced by the current boot instead of a fixed count.
fn assert_owner_only_tree(root: &Path) -> BTreeSet<PathBuf> {
    fn visit(root: &Path, path: &Path, seen: &mut BTreeSet<PathBuf>) {
        let metadata = fs::symlink_metadata(path).expect("state entry metadata");
        let relative = path
            .strip_prefix(root)
            .expect("state descendant")
            .to_path_buf();
        seen.insert(relative);
        if metadata.is_dir() {
            assert_eq!(
                mode(path),
                0o700,
                "directory is not owner-only: {}",
                path.display()
            );
            for entry in fs::read_dir(path).expect("read state directory") {
                visit(root, &entry.expect("state entry").path(), seen);
            }
        } else if metadata.is_file() {
            assert_eq!(
                mode(path),
                0o600,
                "file is not owner-only: {}",
                path.display()
            );
        } else {
            assert!(
                metadata.file_type().is_socket(),
                "unexpected state entry: {}",
                path.display()
            );
        }
    }

    let mut seen = BTreeSet::new();
    visit(root, root, &mut seen);
    seen
}

async fn boot(home: &Path) -> anyhow::Result<cyclopsd::Daemon> {
    cyclopsd::boot(Config::defaults(home)).await
}

async fn assert_boot_refused(home: &Path) {
    match boot(home).await {
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("unsafe state path") || text.contains("open state root"),
                "unexpected boot error: {text}"
            );
        }
        Ok(daemon) => {
            daemon.shutdown().await;
            panic!("daemon accepted unsafe state at {}", home.display());
        }
    }
}

#[test]
fn permissive_umask_child() {
    let Some(home) = std::env::var_os(CHILD_HOME).map(PathBuf::from) else {
        return;
    };
    assert_eq!(
        std::env::var_os("CYCLOPS_HOME"),
        Some(home.clone().into_os_string())
    );

    // This process is dedicated to the test, so changing its umask is isolated.
    unsafe { libc::umask(0) };
    let root = StateRoot::open_or_create(&home).expect("open state root");
    let descendant = Path::new("prewrite/state.ndjson");
    let mut file = root.open_append(descendant).expect("open state file");
    let path = home.join(descendant);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    file.write_all(b"private\n").expect("write state bytes");
    drop((file, root));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    let daemon = runtime.block_on(boot(&home)).expect("boot daemon");
    let seen = assert_owner_only_tree(&home);
    assert!(seen.contains(Path::new("prewrite/state.ndjson")));
    assert!(seen.contains(Path::new("identity/workspace-id")));
    assert!(seen.iter().any(|path| {
        path.starts_with("workspaces")
            && path
                .file_name()
                .is_some_and(|name| name == "messages.ndjson")
    }));
    runtime.block_on(daemon.shutdown());
}

#[test]
fn permissive_umask_creation_is_owner_only_before_state_bytes() {
    let scratch = Scratch::new("cyc-permission-umask");
    let home = scratch.path().join("home");
    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .args(["--exact", "permissive_umask_child", "--nocapture"])
        .env(CHILD_HOME, &home)
        .env("CYCLOPS_HOME", &home)
        .output()
        .expect("run permissive umask child");
    assert!(
        output.status.success(),
        "umask child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn startup_repairs_a_permissive_tree() {
    let scratch = Scratch::new("cyc-permission-repair");
    let home = scratch.path().join("home");
    let legacy = home.join("legacy");
    let file = legacy.join("private.json");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(&file, b"private\n").unwrap();
    set_mode(&home, 0o777);
    set_mode(&legacy, 0o777);
    set_mode(&file, 0o666);

    let daemon = boot(&home).await.expect("boot repairs state");
    assert_eq!(mode(&home), 0o700);
    assert_eq!(mode(&legacy), 0o700);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(fs::read(&file).unwrap(), b"private\n");
    assert_owner_only_tree(&home);
    daemon.shutdown().await;
}

#[tokio::test]
async fn symlinked_root_is_refused_without_mutating_its_target() {
    let scratch = Scratch::new("cyc-permission-root-link");
    let target = scratch.path().join("target");
    let home = scratch.path().join("home");
    fs::create_dir(&target).unwrap();
    set_mode(&target, 0o750);
    let sentinel = target.join("sentinel");
    fs::write(&sentinel, b"outside\n").unwrap();
    set_mode(&sentinel, 0o640);
    let before = snapshot(&sentinel);
    symlink(&target, &home).unwrap();

    assert_boot_refused(&home).await;
    assert_eq!(snapshot(&sentinel), before);
    assert_eq!(mode(&target), 0o750);
    assert!(!target.join(cyclops_proto::SOCK_NAME).exists());
}

#[tokio::test]
async fn dangling_leaf_is_refused_without_creating_its_target() {
    let scratch = Scratch::new("cyc-permission-dangling-link");
    let home = scratch.path().join("home");
    let ledger = home.join("ledger");
    let target = scratch.path().join("outside/missing.ndjson");
    fs::create_dir_all(&ledger).unwrap();
    let leaf = ledger.join("main.ndjson");
    symlink(&target, &leaf).unwrap();

    assert_boot_refused(&home).await;
    assert!(fs::symlink_metadata(&leaf)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!target.exists());
}

#[tokio::test]
async fn linked_directory_component_is_refused_without_external_mutation() {
    let scratch = Scratch::new("cyc-permission-directory-link");
    let home = scratch.path().join("home");
    let external = scratch.path().join("external-ledger");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir(&external).unwrap();
    set_mode(&external, 0o750);
    let sentinel = external.join("sentinel");
    fs::write(&sentinel, b"outside\n").unwrap();
    set_mode(&sentinel, 0o640);
    let before = snapshot(&sentinel);
    symlink(&external, home.join("ledger")).unwrap();

    assert_boot_refused(&home).await;
    assert_eq!(snapshot(&sentinel), before);
    assert_eq!(mode(&external), 0o750);
    assert!(!external.join("main.ndjson").exists());
}

#[tokio::test]
async fn multiply_linked_file_is_refused_without_shared_inode_mutation() {
    let scratch = Scratch::new("cyc-permission-hard-link");
    let home = scratch.path().join("home");
    let external = scratch.path().join("external.ndjson");
    fs::create_dir_all(&home).unwrap();
    fs::write(&external, b"outside\n").unwrap();
    set_mode(&external, 0o640);
    let linked = home.join("linked.ndjson");
    fs::hard_link(&external, &linked).unwrap();
    let before = snapshot(&external);
    let links = fs::metadata(&external).unwrap().nlink();

    assert_boot_refused(&home).await;
    assert_eq!(snapshot(&external), before);
    assert_eq!(fs::metadata(&external).unwrap().nlink(), links);
    assert_eq!(fs::metadata(&linked).unwrap().ino(), before.3);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_identity_record_is_owner_only_and_survives_restart() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("permission-session-identity", CAT_MANIFEST, "cat", "").await;
    let identity_dir = rig.home.join("identity");
    let record = identity_dir.join("sessions.ndjson");
    let before = fs::read(&record).expect("session identity record");
    assert_eq!(mode(&identity_dir), 0o700);
    assert_eq!(mode(&record), 0o600);
    assert_eq!(
        before
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()
            .expect("valid session identity records")
            .len(),
        1
    );

    rig = rig.reboot().await;
    rig.wait_attached(1).await;
    assert_eq!(fs::read(&record).unwrap(), before);
    assert_eq!(mode(&identity_dir), 0o700);
    assert_eq!(mode(&record), 0o600);
    rig.shutdown().await;
}
