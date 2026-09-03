use super::pair_store::*;
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

#[cfg(target_os = "linux")]
#[test]
fn version_probe_recovers_after_a_transient_text_busy_result() {
    let scratch = Scratch::create().unwrap();
    let binary = scratch.path().join("cyclops");
    write_new(
        &binary,
        b"#!/bin/sh\necho 'cyclops 0.1.0 (busy-test)'\n",
        0o755,
    )
    .unwrap();
    let writer = OpenOptions::new().write(true).open(&binary).unwrap();
    let initial = Command::new(&binary).arg("--version").output().unwrap_err();
    assert_eq!(initial.raw_os_error(), Some(libc::ETXTBSY));

    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(writer);
    });
    let output = version_output(&binary).unwrap();
    release.join().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "cyclops 0.1.0 (busy-test)"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn text_busy_retry_is_bounded_and_specific() {
    let mut transient_attempts = 0;
    let recovered = retry_text_busy(|| {
        transient_attempts += 1;
        if transient_attempts == 1 {
            Err(std::io::Error::from_raw_os_error(libc::ETXTBSY))
        } else {
            Ok(7)
        }
    })
    .unwrap();
    assert_eq!(recovered, 7);
    assert_eq!(transient_attempts, 2);

    let mut busy_attempts = 0;
    let busy = retry_text_busy(|| {
        busy_attempts += 1;
        Err::<(), _>(std::io::Error::from_raw_os_error(libc::ETXTBSY))
    })
    .unwrap_err();
    assert_eq!(busy.raw_os_error(), Some(libc::ETXTBSY));
    assert_eq!(busy_attempts, TEXT_BUSY_RETRY_DELAYS_MS.len() + 1);

    let mut denied_attempts = 0;
    let denied = retry_text_busy(|| {
        denied_attempts += 1;
        Err::<(), _>(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    })
    .unwrap_err();
    assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(denied_attempts, 1);
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

fn recorded_replay(store: &PairStore, pair: &str, nonce: u8) -> ReplayAttestation {
    ReplayAttestation {
        schema: 1,
        pair: store.pair_proof(pair).unwrap(),
        snapshot_sha256: format!("{nonce:064x}"),
        snapshot_entries: u64::from(nonce),
        snapshot_bytes: u64::from(nonce),
    }
}

fn crash_at(boundary: UpdateBoundary, operation: impl FnOnce()) {
    CRASH_AT_UPDATE_BOUNDARY.with(|selected| selected.set(Some(boundary)));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
    CRASH_AT_UPDATE_BOUNDARY.with(|selected| selected.set(None));
    let payload = outcome.expect_err("the selected update boundary was not reached");
    assert_eq!(payload.downcast_ref::<UpdateBoundary>(), Some(&boundary));
}

fn assert_selected_pair_is_matched(store: &PairStore, allowed_builds: &[&str]) {
    let selection = store.selection().unwrap().unwrap();
    let client = store.active_binary("cyclops").unwrap();
    let daemon = store.active_binary("cyclopsd").unwrap();
    assert_eq!(client.parent(), daemon.parent());
    let client_build = candidate_build(&client).unwrap();
    let daemon_build = candidate_build(&daemon).unwrap();
    assert_eq!(client_build, daemon_build);
    assert!(allowed_builds.contains(&client_build.as_str()));
    assert_eq!(
        std::fs::read_link(store.prefix.join("cyclops")).unwrap(),
        PathBuf::from(PAIR_ROOT)
            .join(ACTIVE_SELECTOR)
            .join("cyclops")
    );
    assert_eq!(
        std::fs::read_link(store.prefix.join("cyclopsd")).unwrap(),
        PathBuf::from(PAIR_ROOT)
            .join(ACTIVE_SELECTOR)
            .join("cyclopsd")
    );
    assert!(store.root.join(selection.active).is_dir());
    assert!(store.root.join(selection.known_good).is_dir());
}

fn tree_signature(path: &Path) -> Vec<(PathBuf, u8, Vec<u8>)> {
    fn walk(root: &Path, path: &Path, rows: &mut Vec<(PathBuf, u8, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
            let relative = entry.path().strip_prefix(root).unwrap().to_path_buf();
            if metadata.file_type().is_symlink() {
                rows.push((
                    relative,
                    b'l',
                    std::fs::read_link(entry.path())
                        .unwrap()
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                ));
            } else if metadata.is_dir() {
                rows.push((relative, b'd', Vec::new()));
                walk(root, &entry.path(), rows);
            } else {
                rows.push((relative, b'f', std::fs::read(entry.path()).unwrap()));
            }
        }
    }

    let mut rows = Vec::new();
    walk(path, path, &mut rows);
    rows
}

fn replay_failure_pair(path: &Path, build: &str, hello: &str, cli: &[u8]) {
    directory(path);
    write_new(&path.join("cyclops"), cli, 0o755).unwrap();
    let hello = serde_json::to_string(hello).unwrap();
    let script = format!(
            "#!/usr/bin/env python3\nimport os, socket, sys, time\nif len(sys.argv) > 1 and sys.argv[1] == '--version':\n    print('cyclopsd 0.1.0 ({build})')\n    sys.exit(0)\nhome = os.environ['CYCLOPS_HOME']\npath = os.path.join(home, '{}')\ntry:\n    os.unlink(path)\nexcept FileNotFoundError:\n    pass\ns = socket.socket(socket.AF_UNIX)\ns.bind(path)\nwith open(os.path.join(home, 'probe.pid'), 'w') as f:\n    f.write(str(os.getpid()))\ns.listen(1)\nc, _ = s.accept()\nc.sendall(({} + '\\n').encode())\ntime.sleep(60)\n",
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
    replay_failure_pair(
            &pair,
            "build",
            "{malformed",
            b"#!/bin/sh\n[ \"$1\" = \"--version\" ] && { echo 'cyclops 0.1.0 (build)'; exit 0; }\nexit 1\n",
        );
    let error =
        prove_candidate_replay(&pair, &scratch.path().join("absent"), &scratch).unwrap_err();
    assert!(
        error.contains("decode candidate hello"),
        "unexpected replay failure: {error}"
    );
    assert_replay_probe_reaped(&scratch);
}

#[test]
fn candidate_replay_refuses_a_greeting_with_the_same_build_but_another_version() {
    let scratch = Scratch::create().unwrap();
    let pair = scratch.path().join("pair-version-mismatch");
    let hello = serde_json::to_string(&cyclops_proto::Hello {
        cyclops: "0.0.9".to_string(),
        build: Some("same-build".to_string()),
        daemon_process: None,
        daemon_executable: None,
        proto: cyclops_proto::PROTOCOL_VERSION,
        boot_id: "probe-version-mismatch".to_string(),
    })
    .unwrap();
    replay_failure_pair(
            &pair,
            "same-build",
            &hello,
            b"#!/bin/sh\n[ \"$1\" = \"--version\" ] && { echo 'cyclops 0.1.0 (same-build)'; exit 0; }\nexit 1\n",
        );

    let error =
        prove_candidate_replay(&pair, &scratch.path().join("absent"), &scratch).unwrap_err();
    assert!(error.contains("0.1.0 (same-build)"), "{error}");
    assert!(error.contains("0.0.9 (same-build)"), "{error}");
    assert_replay_probe_reaped(&scratch);
}

#[test]
fn failed_candidate_cli_identity_reaps_the_private_daemon() {
    let scratch = Scratch::create().unwrap();
    let pair = scratch.path().join("pair");
    let marker = scratch.path().join("r/probe.pid");
    let cli = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n    [ -f '{}' ] && exit 1\n    echo 'cyclops 0.1.0 (build)'\n    exit 0\nfi\nexit 1\n",
            marker.display()
        );
    replay_failure_pair(&pair, "build", &valid_probe_hello("build"), cli.as_bytes());
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
            "build",
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
    let error = lock_build_cache(&root)
        .err()
        .expect("a second build cache lease must be refused");
    assert!(error.contains("in use"));
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
    let replay = recorded_replay(&store, &candidate, 1);
    assert!(store.activate(&candidate, replay).unwrap().is_none());
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

    let candidate_replay = recorded_replay(&store, &candidate, 2);
    assert_eq!(
        store.activate(&candidate, candidate_replay).unwrap(),
        Some(old.clone())
    );
    let active = store.selection().unwrap().unwrap();
    assert_eq!(active.active, candidate);
    assert_eq!(active.known_good, old.active);

    let rollback_replay = recorded_replay(&store, &active.known_good, 3);
    let (prior, restored) = store.rollback(rollback_replay).unwrap();
    assert_eq!(prior, active);
    assert_eq!(restored.active, old.active);
    assert_eq!(restored.known_good, candidate);
    assert_eq!(store.selection().unwrap(), Some(restored));
}

#[test]
fn reinstall_recovers_owned_public_links_after_the_pair_store_is_removed() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    directory(&prefix);
    let old_source = scratch.path().join("old");
    let candidate_source = scratch.path().join("candidate");
    pair_source(&old_source, "old-build");
    pair_source(&candidate_source, "candidate-build");

    let store = PairStore::open(&prefix).unwrap();
    let old = store.stage(&old_source).unwrap();
    store
        .activate(&old, recorded_replay(&store, &old, 1))
        .unwrap();
    std::fs::remove_dir_all(&store.root).unwrap();

    let recovered = PairStore::open(&prefix).unwrap();
    let candidate = recovered.stage(&candidate_source).unwrap();
    recovered.migrate_direct_pair(&candidate).unwrap();
    recovered
        .activate(&candidate, recorded_replay(&recovered, &candidate, 2))
        .unwrap();

    recovered.require_public_links().unwrap();
    assert_eq!(recovered.selection().unwrap().unwrap().active, candidate);
}

#[test]
fn activation_reports_a_visible_selector_when_its_directory_sync_fails() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let old_source = scratch.path().join("old");
    let candidate_source = scratch.path().join("candidate");
    directory(&prefix);
    pair_source(&old_source, "old-build");
    pair_source(&candidate_source, "candidate-build");
    let store = PairStore::open(&prefix).unwrap();
    let old = store.stage(&old_source).unwrap();
    store
        .activate(&old, recorded_replay(&store, &old, 20))
        .unwrap();
    let before = store.selection().unwrap().unwrap();
    let candidate = store.stage(&candidate_source).unwrap();

    FAIL_NEXT_SELECTOR_DIRECTORY_SYNC.with(|fail| fail.set(true));
    let error = store
        .activate(&candidate, recorded_replay(&store, &candidate, 21))
        .unwrap_err();

    assert!(matches!(
        &error,
        PairChangeError::SelectorVisible { selection, .. } if selection.active == candidate
    ));
    assert!(error
        .to_string()
        .contains("selector durability confirmation failed"));
    assert_eq!(store.selection().unwrap().unwrap().active, candidate);
    assert_eq!(error.previous(), Some(&before));

    let recovery =
        recover_prior_pair_after_change_failure(&store, Some(&before), &error, false, || Ok(()))
            .unwrap();
    assert_eq!(
        recovery,
        PairChangeRecovery {
            prior_selector_restored: true,
            prior_daemon_restarted: false,
        }
    );
    assert_eq!(store.selection().unwrap(), Some(before));
}

#[test]
fn rollback_reports_a_visible_selector_when_its_directory_sync_fails() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let old_source = scratch.path().join("old");
    let candidate_source = scratch.path().join("candidate");
    directory(&prefix);
    pair_source(&old_source, "old-build");
    pair_source(&candidate_source, "candidate-build");
    let store = PairStore::open(&prefix).unwrap();
    let old = store.stage(&old_source).unwrap();
    store
        .activate(&old, recorded_replay(&store, &old, 22))
        .unwrap();
    let candidate = store.stage(&candidate_source).unwrap();
    store
        .activate(&candidate, recorded_replay(&store, &candidate, 23))
        .unwrap();
    let before = store.selection().unwrap().unwrap();

    FAIL_NEXT_SELECTOR_DIRECTORY_SYNC.with(|fail| fail.set(true));
    let error = store
        .rollback(recorded_replay(&store, &before.known_good, 24))
        .unwrap_err();

    assert!(matches!(
        &error,
        PairChangeError::SelectorVisible { selection, .. } if selection.active == before.known_good
    ));
    assert!(error
        .to_string()
        .contains("selector durability confirmation failed"));
    assert_eq!(
        store.selection().unwrap().unwrap().active,
        before.known_good
    );
    assert_eq!(error.previous(), Some(&before));

    recover_prior_pair_after_change_failure(&store, None, &error, false, || Ok(())).unwrap();
    assert_eq!(store.selection().unwrap(), Some(before));
}

#[test]
fn recovery_does_not_hide_a_restart_error_or_start_after_an_unconfirmed_restore() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let old_source = scratch.path().join("old");
    let candidate_source = scratch.path().join("candidate");
    directory(&prefix);
    pair_source(&old_source, "old-build");
    pair_source(&candidate_source, "candidate-build");
    let store = PairStore::open(&prefix).unwrap();
    let old = store.stage(&old_source).unwrap();
    store
        .activate(&old, recorded_replay(&store, &old, 25))
        .unwrap();
    let before = store.selection().unwrap().unwrap();
    let candidate = store.stage(&candidate_source).unwrap();

    FAIL_NEXT_SELECTOR_DIRECTORY_SYNC.with(|fail| fail.set(true));
    let error = store
        .activate(&candidate, recorded_replay(&store, &candidate, 26))
        .unwrap_err();
    let restart_error =
        recover_prior_pair_after_change_failure(&store, Some(&before), &error, true, || {
            Err("injected restart refusal".to_string())
        })
        .unwrap_err();
    assert!(restart_error.contains("previous daemon restart failed: injected restart refusal"));
    assert_eq!(store.selection().unwrap(), Some(before.clone()));

    FAIL_NEXT_SELECTOR_DIRECTORY_SYNC.with(|fail| fail.set(true));
    let error = store
        .activate(&candidate, recorded_replay(&store, &candidate, 27))
        .unwrap_err();
    FAIL_NEXT_SELECTOR_DIRECTORY_SYNC.with(|fail| fail.set(true));
    let restarted = std::cell::Cell::new(false);
    let restore_error =
        recover_prior_pair_after_change_failure(&store, Some(&before), &error, true, || {
            restarted.set(true);
            Ok(())
        })
        .unwrap_err();
    assert!(restore_error.contains("selector recovery held"));
    assert!(!restarted.get());
    assert_eq!(store.selection().unwrap(), Some(before));
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
    let old_replay = recorded_replay(&store, &old, 7);
    let old_selection = store
        .prepare_selection_with_replays(&old, &old, Some(old_replay.clone()), Some(old_replay))
        .unwrap();
    store.select(&old_selection).unwrap();
    store.replace_public_link("cyclopsd").unwrap();
    store.replace_public_link("cyclops").unwrap();
    let replay = recorded_replay(&store, &new, 4);
    store.activate(&new, replay).unwrap();
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

    let replay = recorded_replay(&store, &candidate, 5);
    store.activate(&candidate, replay).unwrap();
    let active = store.selection().unwrap().unwrap();
    assert!(!active.legacy_active);
    assert_eq!(active.active, candidate);
    assert_eq!(active.known_good, candidate);
}

#[test]
fn visible_legacy_migration_retains_its_known_good_candidate_for_recovery() {
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
    let store = PairStore::open(&prefix).unwrap();
    let source = scratch.path().join("candidate");
    pair_source(&source, "candidate-build");
    let candidate = store.stage(&source).unwrap();

    FAIL_NEXT_SELECTOR_DIRECTORY_SYNC.with(|fail| fail.set(true));
    let error = store.migrate_direct_pair(&candidate).unwrap_err();

    let visible = match &error {
        PairChangeError::SelectorVisible { selection, .. } => selection,
        PairChangeError::SelectorUnchanged(error) => {
            panic!("migration never made its selector visible: {error}")
        }
    };
    assert!(visible.legacy_active);
    assert_eq!(visible.known_good, candidate);
    assert_eq!(store.selection().unwrap(), Some((**visible).clone()));
    let recovery = recover_prior_pair_after_change_failure(&store, None, &error, false, || Ok(()))
        .unwrap_err();
    assert!(recovery.contains("no earlier selection to restore"));
    assert!(store.root.join(&candidate).is_dir());
    assert!(store.discard(&candidate).unwrap_err().contains("selected"));
}

#[test]
fn every_pair_commit_boundary_recovers_to_one_matched_pair() {
    let boundaries = [
        UpdateBoundary::PairDirectoryCreated,
        UpdateBoundary::ClientCopied,
        UpdateBoundary::DaemonCopied,
        UpdateBoundary::PairPublished,
        UpdateBoundary::SelectionDirectoryCreated,
        UpdateBoundary::ClientSelectionLinked,
        UpdateBoundary::DaemonSelectionLinked,
        UpdateBoundary::SelectionDescriptorWritten,
        UpdateBoundary::SelectionPublished,
        UpdateBoundary::SelectorTemporaryCreated,
        UpdateBoundary::SelectorCommitted,
        UpdateBoundary::SelectorPublished,
    ];
    for boundary in boundaries {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        let old_source = scratch.path().join("old");
        let new_source = scratch.path().join("new");
        let journal = scratch.path().join("journal.ndjson");
        directory(&prefix);
        pair_source(&old_source, "old-build");
        pair_source(&new_source, "new-build");
        write_new(&journal, b"durable-journal\n", 0o600).unwrap();
        let journal_before = std::fs::symlink_metadata(&journal).unwrap();

        let store = PairStore::open(&prefix).unwrap();
        let old = store.stage(&old_source).unwrap();
        let old_replay = recorded_replay(&store, &old, 10);
        let selected = store
            .prepare_selection_with_replays(&old, &old, Some(old_replay.clone()), Some(old_replay))
            .unwrap();
        store.select(&selected).unwrap();
        store.replace_public_link("cyclopsd").unwrap();
        store.replace_public_link("cyclops").unwrap();

        if matches!(
            boundary,
            UpdateBoundary::PairDirectoryCreated
                | UpdateBoundary::ClientCopied
                | UpdateBoundary::DaemonCopied
                | UpdateBoundary::PairPublished
        ) {
            crash_at(boundary, || {
                let _ = store.stage(&new_source);
            });
        } else {
            let candidate = store.stage(&new_source).unwrap();
            let replay = recorded_replay(&store, &candidate, 11);
            crash_at(boundary, || {
                let _ = store.activate(&candidate, replay);
            });
        }
        drop(store);

        let recovered = PairStore::open(&prefix).unwrap();
        assert_selected_pair_is_matched(&recovered, &["old-build", "new-build"]);
        assert_eq!(std::fs::read(&journal).unwrap(), b"durable-journal\n");
        let journal_after = std::fs::symlink_metadata(&journal).unwrap();
        assert_eq!(journal_after.ino(), journal_before.ino());
        assert_eq!(
            journal_after.permissions().mode(),
            journal_before.permissions().mode()
        );
    }
}

#[test]
fn every_pair_store_initialization_boundary_recovers_on_the_next_open() {
    for boundary in [
        UpdateBoundary::PairStoreRootCreated,
        UpdateBoundary::PairStoreOwnerWritten,
        UpdateBoundary::PairStoreLeaseCreated,
        UpdateBoundary::PairStorePairsCreated,
        UpdateBoundary::PairStoreSelectionsCreated,
    ] {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        directory(&prefix);

        crash_at(boundary, || {
            let _ = PairStore::open(&prefix);
        });

        let recovered = PairStore::open(&prefix).unwrap();
        recovered.require_root().unwrap();
        require_exact_entries(
            &recovered.root,
            &[PAIR_OWNER, PAIR_LEASE, PAIRS_DIR, SELECTIONS_DIR],
        )
        .unwrap();
    }
}

#[test]
fn production_pair_changes_stop_the_daemon_before_replay_snapshot() {
    let source = include_str!("mod.rs");
    for (function, stop_call, replay_call) in [
        (
            "fn run_install_pair(",
            "let stop_result =",
            "prove_candidate_replay(&pair",
        ),
        (
            "fn run_rollback(",
            "stop_selected_for_pair_change(&selected_daemon)",
            "prove_selected_rollback_replay(&store",
        ),
    ] {
        let body = source
            .split_once(function)
            .expect("production pair-change function")
            .1
            .split_once("\n}\n")
            .expect("production pair-change body")
            .0;
        let stopped = body.find(stop_call).expect("exact daemon stop");
        let replayed = body.find(replay_call).expect("private replay proof");
        assert!(
            stopped < replayed,
            "{function} copied live journals before stopping the daemon"
        );
    }
}

#[test]
fn every_public_pair_replacement_boundary_repairs_without_a_split_pair() {
    for boundary in [
        UpdateBoundary::PublicDaemonTemporaryCreated,
        UpdateBoundary::PublicDaemonCommitted,
        UpdateBoundary::PublicDaemonPublished,
        UpdateBoundary::PublicClientTemporaryCreated,
        UpdateBoundary::PublicClientCommitted,
        UpdateBoundary::PublicClientPublished,
    ] {
        let scratch = Scratch::create().unwrap();
        let prefix = scratch.path().join("bin");
        let candidate_source = scratch.path().join("candidate");
        directory(&prefix);
        pair_source(&prefix, "old-build");
        pair_source(&candidate_source, "new-build");
        let store = PairStore::open(&prefix).unwrap();
        let candidate = store.stage(&candidate_source).unwrap();

        crash_at(boundary, || {
            let _ = store.migrate_direct_pair(&candidate);
        });
        drop(store);

        let recovered = PairStore::open(&prefix).unwrap();
        assert_selected_pair_is_matched(&recovered, &["old-build"]);
        assert!(read_directory(&prefix, "install prefix")
            .unwrap()
            .iter()
            .all(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !name.starts_with(".cyclops.") && !name.starts_with(".cyclopsd.")
            }));
    }
}

#[test]
fn a_concurrent_updater_is_refused_before_any_store_mutation() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let source = scratch.path().join("source");
    directory(&prefix);
    pair_source(&source, "build");
    let store = PairStore::open(&prefix).unwrap();
    let pair = store.stage(&source).unwrap();
    let replay = recorded_replay(&store, &pair, 12);
    store.activate(&pair, replay).unwrap();
    let before = tree_signature(&prefix);

    let error = PairStore::open(&prefix)
        .err()
        .expect("a concurrent updater must be refused");
    assert!(error.contains("another Cyclops update"), "{error}");
    assert_eq!(tree_signature(&prefix), before);

    drop(store);
    let reopened = PairStore::open(&prefix).unwrap();
    assert_selected_pair_is_matched(&reopened, &["build"]);
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
    let old_replay = recorded_replay(&store, &old, 7);
    let old_selection = store
        .prepare_selection_with_replays(&old, &old, Some(old_replay.clone()), Some(old_replay))
        .unwrap();
    store.select(&old_selection).unwrap();
    store.replace_public_link("cyclopsd").unwrap();
    store.replace_public_link("cyclops").unwrap();
    let replay = recorded_replay(&store, &new, 6);
    store.activate(&new, replay).unwrap();
    std::fs::remove_file(&old_probe).unwrap();
    std::fs::remove_file(&new_probe).unwrap();

    let descriptor = installed_pair_descriptor(&prefix).unwrap().unwrap();
    assert!(descriptor.rollback_safe);
    assert_eq!(
        descriptor.active_identity.as_deref(),
        Some("0.1.0 (new-build)")
    );
    assert_eq!(
        descriptor.known_good_identity.as_deref(),
        Some("0.1.0 (old-build)")
    );
    assert_eq!(descriptor.active_build.as_deref(), Some("new-build"));
    assert_eq!(descriptor.known_good_build.as_deref(), Some("old-build"));
    assert!(descriptor.active_replay_attested);
    assert!(descriptor.known_good_replay_attested);
    let old_snapshot = format!("{:064x}", 7_u8);
    assert_eq!(
        descriptor.known_good_replay_snapshot.as_deref().unwrap(),
        old_snapshot.as_str()
    );
    assert!(descriptor.selection.is_dir());
    assert!(descriptor.active_pair.is_dir());
    assert!(descriptor.known_good_pair.is_dir());
    assert!(
        !old_probe.exists(),
        "health proof executed the known-good pair"
    );
    assert!(!new_probe.exists(), "health proof executed the active pair");
    drop(store);
}

#[test]
fn selector_change_during_read_only_inspection_is_typed_as_concurrent() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let source = scratch.path().join("candidate");
    directory(&prefix);
    pair_source(&source, "build");
    let store = PairStore::open(&prefix).unwrap();
    let pair = store.stage(&source).unwrap();
    let replay = recorded_replay(&store, &pair, 9);
    let first = store
        .prepare_selection_with_replays(&pair, &pair, Some(replay.clone()), Some(replay.clone()))
        .unwrap();
    let second = store
        .prepare_selection_with_replays(&pair, &pair, Some(replay.clone()), Some(replay))
        .unwrap();
    store.select(&first).unwrap();
    store.replace_public_link("cyclopsd").unwrap();
    store.replace_public_link("cyclops").unwrap();
    let root = store.root.clone();
    let next = second.id.clone();
    drop(store);

    BEFORE_PAIR_INSPECTION_RECHECK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let temporary = root.join(".active.health-race");
            std::os::unix::fs::symlink(next, &temporary).unwrap();
            std::fs::rename(temporary, root.join(ACTIVE_SELECTOR)).unwrap();
        }));
    });
    let error = installed_pair_descriptor(&prefix).unwrap_err();

    assert!(matches!(
        error,
        InstalledPairInspectionError::ConcurrentChange(_)
    ));
}

#[test]
fn health_pair_inspection_does_not_open_or_lock_the_updater_lease() {
    let source = include_str!("pair_store.rs");
    let body = source
        .split_once("pub(crate) fn installed_pair_descriptor(")
        .expect("health pair inspector")
        .1
        .split_once("\n}\n\nfn inspect_installed_pair_snapshot")
        .expect("health pair inspector body")
        .0;

    assert!(!body.contains("PairStore::open_existing"));
    assert!(!body.contains("LOCK_EX"));
    assert!(!body.contains("flock"));
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
    assert!(matches!(&error, InstalledPairInspectionError::Invalid(_)));
    assert!(
        error
            .to_string()
            .contains("changed after its install proof"),
        "{error}"
    );

    std::fs::write(&pair_path, std::fs::read(source.join("cyclops")).unwrap()).unwrap();
    let mut descriptor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&descriptor_path).unwrap()).unwrap();
    descriptor["known_good_proof"] = serde_json::Value::Null;
    std::fs::write(&descriptor_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
    let error = installed_pair_descriptor(&prefix).unwrap_err();
    assert!(matches!(&error, InstalledPairInspectionError::Invalid(_)));
    assert!(
        error
            .to_string()
            .contains("missing a recorded build identity"),
        "{error}"
    );
}

#[test]
fn schema_two_selection_remains_readable_without_replay_evidence() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let source = scratch.path().join("candidate");
    directory(&prefix);
    pair_source(&source, "build");
    let store = PairStore::open(&prefix).unwrap();
    let pair = store.stage(&source).unwrap();
    let replay = recorded_replay(&store, &pair, 13);
    let selection = store
        .prepare_selection_with_replays(&pair, &pair, Some(replay.clone()), Some(replay))
        .unwrap();
    store.select(&selection).unwrap();
    store.replace_public_link("cyclopsd").unwrap();
    store.replace_public_link("cyclops").unwrap();
    let descriptor_path = store.root.join(&selection.id).join(PAIR_DESCRIPTOR);
    let mut descriptor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&descriptor_path).unwrap()).unwrap();
    descriptor["schema"] = serde_json::json!(2);
    descriptor.as_object_mut().unwrap().remove("active_replay");
    descriptor
        .as_object_mut()
        .unwrap()
        .remove("known_good_replay");
    std::fs::write(&descriptor_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();

    let previous = store.selection().unwrap().unwrap();
    assert_eq!(previous.active, pair);
    assert_eq!(previous.active_replay, None);
    assert_eq!(previous.known_good_replay, None);
}

#[test]
fn schema_one_selection_is_reported_as_unproven_instead_of_invalid() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let source = scratch.path().join("candidate");
    directory(&prefix);
    pair_source(&source, "build");
    let store = PairStore::open(&prefix).unwrap();
    let pair = store.stage(&source).unwrap();
    let replay = recorded_replay(&store, &pair, 16);
    let selection = store
        .prepare_selection_with_replays(&pair, &pair, Some(replay.clone()), Some(replay))
        .unwrap();
    store.select(&selection).unwrap();
    store.replace_public_link("cyclopsd").unwrap();
    store.replace_public_link("cyclops").unwrap();
    let descriptor_path = store.root.join(&selection.id).join(PAIR_DESCRIPTOR);
    let mut descriptor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&descriptor_path).unwrap()).unwrap();
    descriptor["schema"] = serde_json::json!(1);
    for field in [
        "active_proof",
        "known_good_proof",
        "active_replay",
        "known_good_replay",
    ] {
        descriptor.as_object_mut().unwrap().remove(field);
    }
    std::fs::write(&descriptor_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();
    drop(store);

    let inspected = installed_pair_descriptor(&prefix).unwrap().unwrap();

    assert!(inspected.proof_unproven);
    assert!(!inspected.rollback_safe);
    assert_eq!(inspected.active_identity, None);
    assert_eq!(inspected.known_good_identity, None);
}

#[test]
fn replay_attestation_is_bound_to_pair_digests_and_snapshot_identity() {
    let scratch = Scratch::create().unwrap();
    let prefix = scratch.path().join("bin");
    let source = scratch.path().join("candidate");
    directory(&prefix);
    pair_source(&source, "build");
    let store = PairStore::open(&prefix).unwrap();
    let pair = store.stage(&source).unwrap();
    let mut replay = recorded_replay(&store, &pair, 14);
    replay.pair.cyclops_sha256 = "0".repeat(64);
    let error = store.activate(&pair, replay).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not name the recorded pair"),
        "{error}"
    );

    let mut replay = recorded_replay(&store, &pair, 15);
    replay.snapshot_sha256 = "not-a-digest".to_string();
    let error = store.activate(&pair, replay).unwrap_err();
    assert!(
        error.to_string().contains("invalid snapshot identity"),
        "{error}"
    );
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
        .to_string()
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
        .to_string()
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
fn same_build_different_versions_are_not_a_matched_pair() {
    let scratch = Scratch::create().unwrap();
    let source = scratch.path().join("mixed-version");
    pair_source(&source, "same-build");
    std::fs::remove_file(source.join("cyclopsd")).unwrap();
    write_new(
        &source.join("cyclopsd"),
        b"#!/bin/sh\n[ \"$1\" = \"--version\" ] && echo 'cyclopsd 0.0.9 (same-build)'\n",
        0o755,
    )
    .unwrap();

    let error = prove_pair_identity(&source).unwrap_err();
    assert!(error.contains("0.1.0 (same-build)"), "{error}");
    assert!(error.contains("0.0.9 (same-build)"), "{error}");
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
