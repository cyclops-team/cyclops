//! Durable-record commands run offline and leave the source journals untouched.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;

use serde_json::{json, Value};

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
    assert_eq!(inventory["forget"]["command"], "cyclops data forget --all");

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

#[test]
fn forget_previews_the_exact_record_scope_then_requires_its_token() {
    let root = scratch();
    let home = root.path().join("home");
    private_directory(&home);
    let session = home.join("ledger/main.ndjson");
    let workspace = home.join("workspaces/alpha/messages.ndjson");
    let config = home.join("config.toml");
    record(&session, b"{\"seq\":1}\n");
    record(&workspace, b"{\"seq\":1,\"body\":\"keep\"}\n");
    record(&config, b"theme = \"light\"\n");

    let preview = run(&home, &["--json", "data", "forget", "--all"]);
    assert!(preview.status.success(), "{preview:?}");
    let preview = json(&preview);
    assert_eq!(preview["state"], "preview");
    assert_eq!(preview["files"], 2);
    assert_eq!(preview["targets"].as_array().unwrap().len(), 2);
    let confirmation = preview["confirmation"]
        .as_str()
        .expect("preview emits its exact confirmation")
        .to_string();
    assert!(confirmation.starts_with("forget-durable-records:"));
    assert_eq!(fs::read(&session).unwrap(), b"{\"seq\":1}\n");
    assert_eq!(
        fs::read(&workspace).unwrap(),
        b"{\"seq\":1,\"body\":\"keep\"}\n"
    );
    assert!(!home.join("operations").exists(), "preview created state");

    let wrong = run(
        &home,
        &[
            "--json",
            "data",
            "forget",
            "--all",
            "--confirm",
            "forget-durable-records:not-the-preview",
        ],
    );
    assert!(!wrong.status.success(), "{wrong:?}");
    assert_eq!(json(&wrong)["state"], "confirmation_required");
    assert!(
        !home.join("operations").exists(),
        "wrong token created state"
    );
    assert!(session.exists());
    assert!(workspace.exists());

    let applied = run(
        &home,
        &[
            "--json",
            "data",
            "forget",
            "--all",
            "--confirm",
            &confirmation,
        ],
    );
    assert!(applied.status.success(), "{applied:?}");
    let applied = json(&applied);
    assert_eq!(applied["state"], "completed");
    assert_eq!(applied["removed_files"], 2);
    assert!(!session.exists());
    assert!(!workspace.exists());
    assert_eq!(fs::read(&config).unwrap(), b"theme = \"light\"\n");
}

#[test]
fn forget_refuses_to_apply_while_an_authenticated_daemon_answers() {
    let root = scratch();
    let home = root.path().join("home");
    private_directory(&home);
    let journal = home.join("ledger/main.ndjson");
    record(&journal, b"{\"seq\":1}\n");

    let preview = json(&run(&home, &["--json", "data", "forget", "--all"]));
    let confirmation = preview["confirmation"]
        .as_str()
        .expect("preview emits confirmation")
        .to_string();

    let listener = UnixListener::bind(home.join(cyclops_proto::SOCK_NAME))
        .expect("bind scratch daemon socket");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept data removal check");
        writeln!(
            stream,
            "{}",
            json!({
                "cyclops": "0.1.0",
                "build": cyclops_proto::BUILD_REF,
                "proto": cyclops_proto::PROTOCOL_VERSION,
                "boot_id": "data-forget-live",
            })
        )
        .expect("write daemon hello");
    });

    let applied = run(
        &home,
        &[
            "--json",
            "data",
            "forget",
            "--all",
            "--confirm",
            &confirmation,
        ],
    );
    server.join().expect("daemon check server exits");
    assert!(!applied.status.success(), "{applied:?}");
    let applied = json(&applied);
    assert_eq!(applied["state"], "refused");
    assert!(applied["error"]
        .as_str()
        .is_some_and(|error| error.contains("cyclopsd is running")));
    assert_eq!(fs::read(&journal).unwrap(), b"{\"seq\":1}\n");
    assert!(
        !home.join("operations").exists(),
        "a live daemon refusal created removal state"
    );
}
