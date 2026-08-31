//! Durable-record commands run offline and leave the source journals untouched.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

fn scratch() -> tempfile::TempDir {
    let root = cyclops_proto::scratch::scratch_root();
    fs::create_dir_all(&root).expect("create shared test scratch root");
    tempfile::Builder::new()
        .prefix("cyclops-data-cli-")
        .tempdir_in(root)
        .expect("create owned test scratch root")
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).expect("create private directory");
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
        .expect("protect private directory");
}

fn record(path: &Path, bytes: &[u8]) {
    private_directory(path.parent().expect("record has parent"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)
        .expect("create private record");
    file.write_all(bytes).expect("write private record");
    file.sync_all().expect("sync private record");
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env_remove("CODEX_HOME")
        .args(args)
        .output()
        .expect("run cyclops data command")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "data command JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn inventory_with_no_state_home_reports_empty_and_creates_nothing() {
    let root = scratch();
    let home = root.path().join("missing-home");

    let inventory = run(&home, &["--json", "data", "inventory"]);
    assert!(inventory.status.success(), "{inventory:?}");
    let inventory = json(&inventory);
    assert_eq!(inventory["complete"], true);
    assert_eq!(inventory["categories"][0]["files"], 0);
    assert_eq!(inventory["categories"][1]["files"], 0);
    assert!(!home.exists(), "inventory created a missing state home");
}

#[test]
fn inventory_and_export_work_without_a_daemon_and_preserve_raw_journals() {
    let root = scratch();
    let home = root.path().join("home");
    private_directory(&home);
    let session = b"{\"seq\":1}\nunterminated tail";
    let workspace = b"{\"seq\":1,\"body\":\"keep every byte\"}\n";
    record(&home.join("ledger/main.ndjson"), session);
    record(&home.join("workspaces/alpha/messages.ndjson"), workspace);

    let inventory = run(&home, &["--json", "data", "inventory"]);
    assert!(inventory.status.success(), "{inventory:?}");
    let inventory = json(&inventory);
    assert_eq!(inventory["complete"], true);
    assert_eq!(inventory["categories"][0]["files"], 1);
    assert_eq!(inventory["categories"][1]["files"], 1);
    assert_eq!(
        inventory["export"]["command"],
        "cyclops data export --to <new-directory>"
    );

    let destination = root.path().join("export");
    let destination_arg = destination.to_str().expect("export path is UTF-8");
    let export = run(
        &home,
        &["--json", "data", "export", "--to", destination_arg],
    );
    assert!(export.status.success(), "{export:?}");
    let export = json(&export);
    assert_eq!(export["exported"], true);
    assert_eq!(export["source_mutated_by_export"], false);
    assert_eq!(export["source_final_recheck"], "matched");
    assert!(
        export["snapshot"]
            .as_str()
            .is_some_and(|snapshot| snapshot.contains("not an atomic daemon snapshot")),
        "export must not present a live source as a daemon-paused snapshot: {export}"
    );
    assert_eq!(
        fs::read(destination.join("records/ledger/main.ndjson")).expect("read exported session"),
        session
    );
    assert_eq!(
        fs::read(destination.join("records/workspaces/alpha/messages.ndjson"))
            .expect("read exported workspace"),
        workspace
    );
    assert_eq!(
        fs::read(home.join("ledger/main.ndjson")).expect("read source session"),
        session
    );
    assert_eq!(
        fs::read(home.join("workspaces/alpha/messages.ndjson")).expect("read source workspace"),
        workspace
    );
    assert!(!destination.join("INCOMPLETE").exists());
}
