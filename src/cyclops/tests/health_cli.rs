//! Read-only health command coverage with isolated homes and no tmux.

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

fn run_health(home: &Path, user: &Path, temp: &Path, path: &str) -> Output {
    run_health_binary(
        Path::new(env!("CARGO_BIN_EXE_cyclops")),
        home,
        user,
        temp,
        path,
    )
}

fn run_health_binary(binary: &Path, home: &Path, user: &Path, temp: &Path, path: &str) -> Output {
    Command::new(binary)
        .env("CYCLOPS_HOME", home)
        .env("HOME", user)
        .env("TMPDIR", temp)
        .env("PATH", path)
        .env_remove("CODEX_HOME")
        .args(["--json", "health"])
        .output()
        .expect("run cyclops health")
}

fn make_executable(path: &Path) {
    fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// Copy the built client into an isolated executable pair.
///
/// The daemon stand-in only needs to answer the version query; the client
/// binary already provides that behavior with the exact build under test.
fn paired_client(directory: &Path) -> (PathBuf, PathBuf) {
    fs::create_dir(directory).unwrap();
    let source = Path::new(env!("CARGO_BIN_EXE_cyclops"));
    let client = directory.join("cyclops");
    let daemon = directory.join("cyclopsd");
    for destination in [&client, &daemon] {
        fs::copy(source, destination).unwrap();
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
    }
    (
        fs::canonicalize(client).unwrap(),
        fs::canonicalize(daemon).unwrap(),
    )
}

fn scratch(tag: &str) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = tempfile::Builder::new().prefix(tag).tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let home = base.join("state");
    let user = base.join("user");
    let runtime_temp = base.join("temp");
    fs::create_dir(&user).unwrap();
    fs::create_dir(&runtime_temp).unwrap();
    fs::set_permissions(&user, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&runtime_temp, fs::Permissions::from_mode(0o700)).unwrap();
    (temp, home, user, runtime_temp)
}

fn parse(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "health JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn answer_health_snapshot(stream: UnixStream) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    assert!(reader.read_line(&mut request).unwrap() > 0);
    let request: Value = serde_json::from_str(request.trim()).unwrap();
    assert_eq!(request["method"], "health.snapshot");
    assert_eq!(request["params"], json!({}));
    writeln!(
        reader.get_mut(),
        "{}",
        json!({
            "id": request["id"],
            "result": {
                "daemon_version": "0.1.0",
                "proto": 1,
                "boot_id": "health-boot",
                "uptime_ms": 123,
                "tmux_version": "3.6a",
                "sessions": [],
            }
        })
    )
    .unwrap();
}

#[test]
fn health_works_with_no_daemon_and_does_not_create_state() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-absent");
    let mut before = fs::read_dir(home.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    before.sort();
    let output = run_health(&home, &user, &runtime_temp, "");
    let report = parse(&output);
    // Process inventory is intentionally host-wide: another same-user daemon
    // may make the overall report unhealthy even though this isolated state
    // root has no daemon. This test owns the offline/read-only facts below,
    // while exit semantics and duplicate-daemon reporting have focused tests.
    assert_eq!(report["daemon"]["running"], false);
    assert_eq!(report["state"]["present"], false);
    assert_eq!(report["state"]["root"], home.display().to_string());
    assert_eq!(
        report["state"]["socket"],
        home.join("sock").display().to_string()
    );
    assert_eq!(report["client"]["build"], env!("CYCLOPS_BUILD_REF"));
    assert!(report["client"]["selected_executable"]
        .as_str()
        .is_some_and(|path| !path.is_empty()));
    assert!(report["client"]["resolutions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == "cyclops" && entry["selected"] == true));
    assert_eq!(report["daemon"]["executable"]["state"], "unproven");
    assert!(!home.exists(), "health created the missing state root");
    let mut after = fs::read_dir(home.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    after.sort();
    assert_eq!(after, before, "health changed the scratch home");
}

#[test]
fn health_fails_when_the_selected_client_has_no_daemon_sibling() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-unpaired");
    let bin = user.join("bin");
    fs::create_dir(&bin).unwrap();
    let client = bin.join("cyclops");
    fs::copy(env!("CARGO_BIN_EXE_cyclops"), &client).unwrap();
    fs::set_permissions(&client, fs::Permissions::from_mode(0o700)).unwrap();

    let output = run_health_binary(&client, &home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    assert_eq!(
        report["client"]["selected_daemon"],
        bin.join("cyclopsd").display().to_string()
    );
    assert_eq!(report["client"]["selected_daemon_ready"], false);
    assert!(report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|issue| issue["code"] == "selected_daemon_unavailable"));
}

#[test]
fn health_names_a_stale_socket_without_starting_a_daemon() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-stale-socket");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(home.join("sock")).unwrap();
    drop(listener);

    let output = run_health(&home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    assert_eq!(report["daemon"]["state"], "stale_socket");
    assert_eq!(report["daemon"]["running"], false);
    assert!(report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|issue| issue["code"] == "stale_socket"));
}

#[test]
fn health_names_every_shadowed_path_resolution() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-path");
    let bin_a = user.join("bin-a");
    let bin_b = user.join("bin-b");
    fs::create_dir(&bin_a).unwrap();
    fs::create_dir(&bin_b).unwrap();
    for directory in [&bin_a, &bin_b] {
        make_executable(&directory.join("cyclops"));
        make_executable(&directory.join("cyclopsd"));
    }
    let path = format!("{}:{}", bin_a.display(), bin_b.display());
    let output = run_health(&home, &user, &runtime_temp, &path);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    assert_eq!(report["client"]["shadowed"], true);
    let paths = report["client"]["resolutions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect::<Vec<_>>();
    for expected in [
        bin_a.join("cyclops"),
        bin_a.join("cyclopsd"),
        bin_b.join("cyclops"),
        bin_b.join("cyclopsd"),
    ] {
        assert!(paths.contains(&expected.to_str().unwrap()), "{report}");
    }
    assert!(report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|issue| issue["code"] == "shadowed_binaries"));
}

#[test]
fn health_reports_linked_state_without_following_or_repairing_it() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-links");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let external = user.join("external");
    fs::write(&external, b"outside\n").unwrap();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&external, home.join("linked")).unwrap();
    fs::hard_link(&external, home.join("shared")).unwrap();

    let output = run_health(&home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    let unsafe_entries = report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| issue["code"] == "unsafe_state_entry")
        .count();
    assert_eq!(unsafe_entries, 2, "{report}");
    assert_eq!(fs::read(&external).unwrap(), b"outside\n");
    assert_eq!(
        fs::metadata(&external).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn health_does_not_follow_linked_hook_or_skill_files() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-setup-links");
    let claude = user.join(".claude");
    let skill_dir = claude.join("skills/cyclops");
    fs::create_dir_all(&skill_dir).unwrap();
    let external_hook = user.join("external-hook.json");
    let external_skill = user.join("external-skill.md");
    fs::write(&external_hook, b"{}\n").unwrap();
    fs::write(&external_skill, b"external\n").unwrap();
    symlink(&external_hook, claude.join("settings.json")).unwrap();
    symlink(&external_skill, skill_dir.join("SKILL.md")).unwrap();

    let output = run_health(&home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    let claude = report["setup"]["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|consumer| consumer["id"] == "claude")
        .unwrap();
    assert_eq!(claude["hooks"]["state"], "unproven");
    assert_eq!(claude["skill"]["state"], "unproven");
    assert_eq!(fs::read(&external_hook).unwrap(), b"{}\n");
    assert_eq!(fs::read(&external_skill).unwrap(), b"external\n");
}

#[test]
fn health_never_accepts_writable_hook_or_skill_bytes_as_current() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-writable-setup");
    let claude = user.join(".claude");
    let skill_dir = claude.join("skills/cyclops");
    fs::create_dir_all(&skill_dir).unwrap();
    let hook = claude.join("settings.json");
    let skill = skill_dir.join("SKILL.md");
    fs::write(&hook, b"{}\n").unwrap();
    fs::write(&skill, b"operator edit\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o666)).unwrap();
    fs::set_permissions(&skill, fs::Permissions::from_mode(0o666)).unwrap();

    let output = run_health(&home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    let claude = report["setup"]["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|consumer| consumer["id"] == "claude")
        .unwrap();
    assert_eq!(claude["hooks"]["state"], "unproven");
    assert_eq!(claude["skill"]["state"], "unproven");
    assert_eq!(
        fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
        0o666
    );
    assert_eq!(
        fs::metadata(&skill).unwrap().permissions().mode() & 0o777,
        0o666
    );
}

#[test]
fn health_marks_an_installed_consumer_with_missing_setup_incomplete() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-incomplete-setup");
    fs::create_dir(user.join(".claude")).unwrap();

    let output = run_health(&home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    let claude = report["setup"]["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|consumer| consumer["id"] == "claude")
        .unwrap();
    assert_eq!(claude["installed"], true);
    assert_eq!(claude["complete"], false);
    assert!(report["issues"].as_array().unwrap().iter().any(|issue| {
        issue["code"] == "consumer_setup_incomplete"
            && issue["message"]
                .as_str()
                .is_some_and(|message| message.contains("Claude Code setup is incomplete"))
    }));
}

#[test]
fn health_accepts_cargo_modes_beneath_a_private_build_cache() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-cache-cargo-modes");
    let baseline = parse(&run_health(&home, &user, &runtime_temp, ""));
    let cache = PathBuf::from(baseline["build_cache"]["path"].as_str().unwrap());
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(cache.join(".lease"), b"").unwrap();
    fs::set_permissions(cache.join(".lease"), fs::Permissions::from_mode(0o600)).unwrap();
    let dist = cache.join("dist/debug");
    fs::create_dir_all(&dist).unwrap();
    fs::set_permissions(cache.join("dist"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&dist, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dist.join("artifact.rlib"), b"cargo artifact").unwrap();
    fs::set_permissions(
        dist.join("artifact.rlib"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::write(dist.join("cyclops"), b"executable artifact").unwrap();
    fs::set_permissions(dist.join("cyclops"), fs::Permissions::from_mode(0o755)).unwrap();

    let report = parse(&run_health(&home, &user, &runtime_temp, ""));
    assert_eq!(report["build_cache"]["safe"], true, "{report}");
    assert!(report["build_cache"]["entries"].as_u64().unwrap() > 0);
    assert!(
        report["build_cache"]["candidates"][0]["bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(!report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|issue| issue["code"] == "unsafe_build_cache"));
}

#[test]
fn health_recursively_refuses_a_link_inside_the_build_cache() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-cache-link");
    let baseline = parse(&run_health(&home, &user, &runtime_temp, ""));
    let cache = PathBuf::from(baseline["build_cache"]["path"].as_str().unwrap());
    fs::create_dir(&cache).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(cache.join(".lease"), b"").unwrap();
    fs::set_permissions(cache.join(".lease"), fs::Permissions::from_mode(0o600)).unwrap();
    let nested = cache.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
    let external = user.join("external-cache-bytes");
    fs::write(&external, b"outside").unwrap();
    symlink(&external, nested.join("linked")).unwrap();

    let output = run_health(&home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    assert_eq!(report["build_cache"]["safe"], false);
    assert_eq!(report["build_cache"]["candidates"][0]["safe"], false);
    assert_eq!(fs::read(&external).unwrap(), b"outside");
    assert!(report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|issue| issue["code"] == "unsafe_build_cache"));
}

#[test]
fn health_reports_each_update_scratch_marker_and_lease() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-scratch-marker");
    let nonce = "0123456789abcdef0123456789abcdef";
    let scratch = runtime_temp.join(format!("cyclops-update.{nonce}"));
    fs::create_dir(&scratch).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(scratch.join(".lease"), b"").unwrap();
    fs::set_permissions(scratch.join(".lease"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(scratch.join("payload"), b"temporary").unwrap();

    let output = run_health(&home, &user, &runtime_temp, "");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    let candidates = report["update_scratch"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["path"], scratch.display().to_string());
    assert_eq!(candidates[0]["marker"], "unproven");
    assert_eq!(candidates[0]["lease"], "current");
    assert_eq!(candidates[0]["safe"], false);
    assert!(report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|issue| issue["code"] == "unsafe_update_scratch"));
}

#[test]
fn health_reports_hello_identity_with_one_read_only_snapshot_request() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-daemon");
    let (client, daemon) = paired_client(&user.join("bin"));
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(home.join("sock")).unwrap();
    let expected_daemon = daemon.display().to_string();
    let daemon_wire = expected_daemon.clone();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream;
        writeln!(
            writer,
            "{}",
            json!({
                "cyclops": "0.1.0",
                "build": env!("CYCLOPS_BUILD_REF"),
                "daemon_process": { "pid": 4242, "birth": 818221 },
                "daemon_executable": daemon_wire,
                "proto": 1,
                "boot_id": "health-boot"
            })
        )
        .unwrap();
        answer_health_snapshot(writer);
    });

    let output = run_health_binary(&client, &home, &user, &runtime_temp, "");
    server.join().unwrap();
    let report = parse(&output);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    for code in [
        "daemon_process_inventory_unproven",
        "workspace_mapping_unproven",
    ] {
        assert!(
            report["issues"]
                .as_array()
                .unwrap()
                .iter()
                .any(|issue| issue["code"] == code),
            "{report}"
        );
    }
    assert_eq!(report["daemon"]["state"], "running");
    assert_eq!(report["daemon"]["running"], true);
    assert_eq!(report["daemon"]["authenticated_socket"], true);
    assert_eq!(report["daemon"]["pid"], 4242);
    assert_eq!(report["daemon"]["process"]["state"], "proven");
    assert_eq!(report["daemon"]["process"]["birth"], 818221);
    assert_eq!(report["daemon"]["boot_id"], "health-boot");
    assert_eq!(report["daemon"]["uptime_ms"], 123);
    assert_eq!(report["daemon"]["client_build_matches"], true);
    assert_eq!(report["daemon"]["executable"]["state"], "proven");
    assert_eq!(report["daemon"]["executable"]["path"], expected_daemon);
}

#[test]
fn health_refuses_a_non_absolute_hello_executable() {
    let (_temp, home, user, runtime_temp) = scratch("cyclops-health-daemon-path");
    let (client, _daemon) = paired_client(&user.join("bin"));
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(home.join("sock")).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = stream;
        writeln!(
            writer,
            "{}",
            json!({
                "cyclops": "0.1.0",
                "build": env!("CYCLOPS_BUILD_REF"),
                "daemon_process": { "pid": 4242, "birth": 818221 },
                "daemon_executable": "relative/cyclopsd",
                "proto": 1,
                "boot_id": "health-boot"
            })
        )
        .unwrap();
        answer_health_snapshot(writer);
    });

    let output = run_health_binary(&client, &home, &user, &runtime_temp, "");
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report = parse(&output);
    assert_eq!(report["daemon"]["executable"]["state"], "unproven");
    assert!(
        report["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["code"] == "daemon_executable_unproven"),
        "{report}"
    );
    assert!(
        report["issues"].as_array().unwrap().iter().any(|issue| {
            issue["code"] == "daemon_identity_unavailable"
                && issue["message"] == "daemon reported a non-absolute executable path"
        }),
        "{report}"
    );
}
