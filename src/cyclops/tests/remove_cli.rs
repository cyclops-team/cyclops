//! The complete state-removal journey runs against a scratch home only.
//!
//! `cyclops data forget --all` remains the narrow, journal-only operation.
//! This test protects the separate, explicit operation that can remove the
//! complete Cyclops state home without reaching into unrelated user files.

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
        .prefix("cyclops-remove-cli-")
        .tempdir_in(root)
        .expect("create owned test scratch root")
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).expect("create private directory");
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
        .expect("protect private directory");
}

fn state_file(path: &Path, bytes: &[u8]) {
    private_directory(path.parent().expect("state file has a parent"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)
        .expect("create private state file");
    file.write_all(bytes).expect("write private state file");
    file.sync_all().expect("sync private state file");
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env_remove("CODEX_HOME")
        .args(args)
        .output()
        .expect("run cyclops remove command")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "remove command JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

#[test]
fn remove_previews_the_complete_state_home_then_requires_its_exact_confirmation() {
    let root = scratch();
    let home = root.path().join("home");
    let unrelated = root.path().join("outside-cyclops");
    private_directory(&home);
    private_directory(&unrelated);

    state_file(&home.join("config.toml"), b"theme = \"light\"\n");
    state_file(
        &home.join("workspaces/alpha/messages.ndjson"),
        b"{\"seq\":1,\"body\":\"private journal body\"}\n",
    );
    state_file(
        &home.join("layouts/focus.toml"),
        b"[layout]\nname = \"focus\"\n",
    );
    state_file(&home.join("logs/daemon.log"), b"diagnostic\n");
    state_file(&unrelated.join("keep.txt"), b"not Cyclops state\n");

    let preview = run(&home, &["--json", "remove", "--all"]);
    assert!(preview.status.success(), "{preview:?}");
    let preview = json(&preview);
    assert_eq!(preview["state"], "preview");
    assert!(preview["recovery"].is_null());
    assert_eq!(
        preview["scope"],
        "the complete current Cyclops state home only"
    );
    assert_eq!(preview["files"], 4);
    assert_eq!(preview["targets"].as_array().unwrap().len(), 4);
    assert!(
        preview["targets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|target| target.get("body").is_none()),
        "a destructive preview must stay body-free: {preview}"
    );
    let confirmation = preview["confirmation"]
        .as_str()
        .expect("preview emits exact confirmation")
        .to_string();
    assert!(confirmation.starts_with("remove-cyclops-state:"));
    assert!(home.join("config.toml").exists(), "preview changed state");
    assert!(
        !root.path().join("home.removing").exists(),
        "preview created a removal tombstone"
    );

    let wrong = run(
        &home,
        &[
            "--json",
            "remove",
            "--all",
            "--confirm",
            "remove-cyclops-state:not-the-preview",
        ],
    );
    assert!(!wrong.status.success(), "{wrong:?}");
    assert_eq!(json(&wrong)["state"], "confirmation_required");
    assert!(
        home.join("config.toml").exists(),
        "wrong token changed state"
    );

    let removed = run(
        &home,
        &["--json", "remove", "--all", "--confirm", &confirmation],
    );
    assert!(removed.status.success(), "{removed:?}");
    let removed = json(&removed);
    assert_eq!(removed["state"], "state_removal_completed");
    assert!(removed["recovery"].is_null());
    assert_eq!(removed["removed_files"], 4);
    assert_eq!(
        removed["next_step"]["command"],
        "curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall"
    );
    assert!(
        removed["next_step"]["scope"]
            .as_str()
            .is_some_and(|scope| scope.contains("binaries")),
        "the result must distinguish state removal from installer cleanup: {removed}"
    );
    assert!(
        !home.exists(),
        "the complete state home remains after success"
    );
    assert!(
        !root.path().join("home.removing").exists(),
        "the completed state-removal tombstone remains"
    );
    assert_eq!(
        fs::read(unrelated.join("keep.txt")).unwrap(),
        b"not Cyclops state\n",
        "complete state removal reached outside the state home"
    );
}

#[test]
fn plain_remove_preview_lists_a_stale_state_socket() {
    let root = scratch();
    let home = root.path().join("home");
    private_directory(&home);
    state_file(&home.join("config.toml"), b"theme = \"light\"\n");

    let socket_path = home.join(cyclops_proto::SOCK_NAME);
    let socket = UnixListener::bind(&socket_path).expect("bind scratch stale socket");
    drop(socket);

    let preview = run(&home, &["remove", "--all"]);
    assert!(preview.status.success(), "{preview:?}");
    let plain = String::from_utf8(preview.stdout).expect("plain remove preview is UTF-8");
    assert!(
        plain.contains(&format!("{} · socket", cyclops_proto::SOCK_NAME)),
        "plain preview must list the stale socket it would remove: {plain}"
    );
    assert!(home.join(cyclops_proto::SOCK_NAME).exists());
    assert!(!root.path().join("home.removing").exists());
}

#[test]
fn remove_refuses_while_an_authenticated_daemon_answers_and_leaves_state_intact() {
    let root = scratch();
    let home = root.path().join("home");
    private_directory(&home);
    state_file(&home.join("config.toml"), b"theme = \"dark\"\n");

    let preview = json(&run(&home, &["--json", "remove", "--all"]));
    let confirmation = preview["confirmation"]
        .as_str()
        .expect("preview emits exact confirmation")
        .to_string();

    let listener = UnixListener::bind(home.join(cyclops_proto::SOCK_NAME))
        .expect("bind scratch daemon socket");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept removal daemon check");
        writeln!(
            stream,
            "{}",
            json!({
                "cyclops": "0.1.0",
                "build": cyclops_proto::BUILD_REF,
                "proto": cyclops_proto::PROTOCOL_VERSION,
                "boot_id": "state-remove-live",
            })
        )
        .expect("write daemon hello");
    });

    let removed = run(
        &home,
        &["--json", "remove", "--all", "--confirm", &confirmation],
    );
    server.join().expect("daemon check server exits");
    assert!(!removed.status.success(), "{removed:?}");
    let removed = json(&removed);
    assert_eq!(removed["state"], "refused");
    assert!(removed["error"]
        .as_str()
        .is_some_and(|error| error.contains("cyclopsd is running")));
    assert_eq!(
        fs::read(home.join("config.toml")).unwrap(),
        b"theme = \"dark\"\n"
    );
    assert!(
        !root.path().join("home.removing").exists(),
        "a live daemon refusal created a tombstone"
    );
}
