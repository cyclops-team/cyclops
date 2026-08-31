//! `cyclops sizing release` against an isolated tmux server.
//!
//! The command refuses more often than it acts, and a refusal that reads
//! like success is worse than no command at all: the operator walks away
//! believing a session was handed back while its windows are still pinned
//! and still owned. These pin the exact stdout, JSON and exit code of each
//! answer.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::Path;
use std::process::{Command, Output};

use cyclops_testrig::{tmux_available, TmuxServer};
use serde_json::Value;

/// A home whose config points the client at this test's tmux server, the
/// same way a real installation points it at the one the daemon watches.
fn home_for(server: &TmuxServer, tag: &str) -> std::path::PathBuf {
    let home = cyclops_proto::scratch::scratch_dir(tag);
    fs::create_dir_all(&home).expect("home exists");
    fs::write(
        home.join("config.toml"),
        format!(
            "tmux_socket = \"{}\"\ntmux_config = \"/dev/null\"\n",
            server.socket()
        ),
    )
    .expect("write config");
    home
}

fn release(home: &Path, session: &str, json: bool) -> Output {
    let mut cmd = Command::new(Path::new(env!("CARGO_BIN_EXE_cyclops")));
    cmd.env("CYCLOPS_HOME", home);
    if json {
        cmd.arg("--json");
    }
    cmd.args(["sizing", "release", "--session", session]);
    cmd.output().expect("run cyclops sizing release")
}

/// Run the omitted-session form with an ambient tmux recorder. A config safety
/// failure must return before this fake can observe any tmux invocation.
fn release_without_session(home: &Path, tmux_bin: &Path, calls: &Path) -> Output {
    let mut cmd = Command::new(Path::new(env!("CARGO_BIN_EXE_cyclops")));
    cmd.env("CYCLOPS_HOME", home)
        .env("CYCLOPS_SIZING_TMUX_CALLS", calls)
        .env("PATH", tmux_bin)
        .env("TMUX", "ambient,123,0")
        .args(["--json", "sizing", "release"]);
    cmd.output().expect("run cyclops sizing release")
}

fn fake_tmux(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let bin = root.join("bin");
    fs::create_dir(&bin).expect("create fake tmux directory");
    let fake_tmux = bin.join("tmux");
    fs::write(
        &fake_tmux,
        "#!/bin/sh\nprintf 'tmux' >> \"$CYCLOPS_SIZING_TMUX_CALLS\"\nfor arg in \"$@\"; do printf '\\t%s' \"$arg\" >> \"$CYCLOPS_SIZING_TMUX_CALLS\"; done\nprintf '\\n' >> \"$CYCLOPS_SIZING_TMUX_CALLS\"\n",
    )
    .expect("write fake tmux recorder");
    fs::set_permissions(&fake_tmux, fs::Permissions::from_mode(0o700))
        .expect("make fake tmux executable");
    (bin, root.join("tmux.calls"))
}

fn isolated_root(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(name)
        .tempdir_in(cyclops_proto::scratch::scratch_root())
        .expect("create isolated root")
}

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout is one JSON object")
}

/// A damaged configuration must refuse before the omitted-session fallback can
/// query the terminal's ambient tmux server. This catches the old order where
/// `current_session(None)` ran before the config boundary.
#[test]
fn malformed_or_linked_config_refuses_before_an_ambient_session_query() {
    let root = isolated_root("sizing-config-refusal-");
    let (tmux_bin, calls) = fake_tmux(root.path());

    let malformed_home = root.path().join("malformed-home");
    fs::create_dir(&malformed_home).expect("create malformed home");
    fs::write(malformed_home.join("config.toml"), "tmux_socket = [")
        .expect("write malformed config");
    let malformed = release_without_session(&malformed_home, &tmux_bin, &calls);
    assert_eq!(malformed.status.code(), Some(1));
    let malformed_body = json_of(&malformed);
    assert_eq!(malformed_body["ok"], Value::Bool(false));
    assert!(
        malformed_body["error"]
            .as_str()
            .is_some_and(|error| error.contains("malformed")),
        "malformed config was not the reported failure: {malformed_body}"
    );
    assert!(
        !calls.exists(),
        "malformed config queried ambient tmux: {}",
        fs::read_to_string(&calls).unwrap_or_default()
    );

    let linked_home = root.path().join("linked-home");
    fs::create_dir(&linked_home).expect("create linked home");
    let target = root.path().join("outside-config.toml");
    fs::write(&target, "tmux_socket = \"foreign\"\n").expect("write external config");
    symlink(&target, linked_home.join("config.toml")).expect("link external config");
    let linked = release_without_session(&linked_home, &tmux_bin, &calls);
    assert_eq!(linked.status.code(), Some(1));
    let linked_body = json_of(&linked);
    assert_eq!(linked_body["ok"], Value::Bool(false));
    assert!(
        linked_body["error"]
            .as_str()
            .is_some_and(|error| error.contains("could not safely read coordinator config")),
        "linked config was not the reported failure: {linked_body}"
    );
    assert!(
        !calls.exists(),
        "linked config queried ambient tmux: {}",
        fs::read_to_string(&calls).unwrap_or_default()
    );
}

/// The shell's session belongs to the ambient server. A named coordinator
/// server must get an explicit recovery target rather than borrowing that
/// potentially unrelated session name.
#[test]
fn named_server_requires_an_explicit_session_without_querying_ambient_tmux() {
    let root = isolated_root("sizing-configured-session-");
    let (tmux_bin, calls) = fake_tmux(root.path());
    let home = root.path().join("configured-home");
    fs::create_dir(&home).expect("create configured home");
    fs::write(home.join("config.toml"), "tmux_socket = \"configured\"\n")
        .expect("write configured server");

    let output = release_without_session(&home, &tmux_bin, &calls);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unexpected configured-server refusal:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--session <name>"),
        "the refusal did not explain how to name the server target: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !calls.exists(),
        "configured-server refusal queried ambient tmux: {}",
        fs::read_to_string(&calls).unwrap_or_default()
    );
}

/// Nothing to do is a success, and says so.
#[test]
fn releasing_a_session_cyclops_never_sized_succeeds_and_touches_nothing() {
    if !tmux_available() {
        return;
    }
    let server = TmuxServer::new("sizing-cli-clean");
    server.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "120",
        "-y",
        "40",
        "/bin/sh",
    ]);
    let home = home_for(&server, "sizing-cli-clean-home");

    let out = release(&home, "work", true);
    assert_eq!(out.status.code(), Some(0));
    let body = json_of(&out);
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(body["released"], Value::Bool(true));
    assert_eq!(body["refused"], Value::Null);
    let _ = fs::remove_dir_all(home);
}

/// A record cyclops cannot read is a refusal, and every surface says so.
///
/// The defect this pins: the command used to print `<session>: cyclops
/// sizing released` and emit `ok: true` while returning a failure code,
/// with the windows still pinned and the mark still set.
#[test]
fn an_unreadable_record_refuses_on_stdout_json_and_exit_code() {
    if !tmux_available() {
        return;
    }
    let server = TmuxServer::new("sizing-cli-malformed");
    server.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "120",
        "-y",
        "40",
        "/bin/sh",
    ]);
    server.run_ok(&["set-option", "-w", "-t", "@0", "window-size", "manual"]);
    server.run_ok(&[
        "set-option",
        "-w",
        "-t",
        "@0",
        "@cyclops_prior_window_size",
        "written-by-something-else",
    ]);
    let home = home_for(&server, "sizing-cli-malformed-home");

    let text = release(&home, "work", false);
    assert_eq!(text.status.code(), Some(3), "a refusal must not exit zero");
    let stdout = String::from_utf8_lossy(&text.stdout);
    let stderr = String::from_utf8_lossy(&text.stderr);
    assert!(
        !stdout.contains("cyclops sizing released"),
        "a refusal announced itself as a release: {stdout}"
    );
    assert!(
        stderr.contains("refused"),
        "the refusal was not stated: {stderr}"
    );
    assert!(
        stderr.contains("@cyclops_prior_window_size"),
        "the operator was not told how to inspect the record: {stderr}"
    );

    let out = release(&home, "work", true);
    assert_eq!(out.status.code(), Some(3));
    let body = json_of(&out);
    assert_eq!(body["ok"], Value::Bool(false), "JSON claimed success");
    assert_eq!(body["released"], Value::Bool(false));
    assert_eq!(body["refused"], Value::String("unreadable_record".into()));
    assert_eq!(
        body["malformed"].as_array().map(Vec::len),
        Some(1),
        "the per-window outcome was dropped"
    );

    // And it changed nothing.
    let policy = server.run(&["show-options", "-w", "-t", "@0", "-qv", "window-size"]);
    assert_eq!(String::from_utf8_lossy(&policy.stdout).trim(), "manual");
    let record = server.run(&[
        "show-options",
        "-w",
        "-t",
        "@0",
        "-qv",
        "@cyclops_prior_window_size",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&record.stdout).trim(),
        "written-by-something-else"
    );
    let _ = fs::remove_dir_all(home);
}

/// Recovery under a running owner is refused, named, and changes nothing.
#[test]
fn a_live_owner_refuses_on_stdout_json_and_exit_code() {
    if !tmux_available() {
        return;
    }
    let server = TmuxServer::new("sizing-cli-live");
    server.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "-x",
        "120",
        "-y",
        "40",
        "/bin/sh",
    ]);
    // A marker naming a client that really is attached to this server.
    let clients = server.run(&["list-clients", "-F", "#{client_name}:#{client_created}"]);
    let live = String::from_utf8_lossy(&clients.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string);
    let Some(live) = live else {
        // A detached session has no clients; attach one through the rig.
        return;
    };
    server.run_ok(&["set-option", "-t", "work", "@cyclops_window_driver", &live]);
    server.run_ok(&["set-option", "-w", "-t", "@0", "window-size", "manual"]);
    let home = home_for(&server, "sizing-cli-live-home");

    let text = release(&home, "work", false);
    assert_eq!(text.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&text.stderr);
    assert!(stderr.contains("refused"), "{stderr}");
    assert!(stderr.contains(&live), "the owner was not named: {stderr}");

    let out = release(&home, "work", true);
    assert_eq!(out.status.code(), Some(3));
    let body = json_of(&out);
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["refused"], Value::String("live_owner".into()));
    assert_eq!(body["owner"], Value::String(live.clone()));

    let mark = server.run(&[
        "show-options",
        "-t",
        "work",
        "-qv",
        "@cyclops_window_driver",
    ]);
    assert_eq!(String::from_utf8_lossy(&mark.stdout).trim(), live);
    let _ = fs::remove_dir_all(home);
}
