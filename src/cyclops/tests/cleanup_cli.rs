use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn command(temp: &tempfile::TempDir) -> Command {
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cyclops"));
    command
        .env("TMPDIR", temp.path())
        .env("CYCLOPS_HOME", temp.path().join("state"));
    command
}

#[test]
fn cleanup_json_and_plain_report_the_same_absent_asset() {
    let temp = tempfile::tempdir().unwrap();
    let json = command(&temp)
        .args(["--json", "cleanup", "build-cache"])
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["mode"], "dry_run");
    assert_eq!(value["candidates"][0]["class"], "build_cache");
    assert_eq!(value["candidates"][0]["state"], "absent");
    assert_eq!(value["excluded"][0]["class"], "state_journals_and_messages");

    let plain = command(&temp)
        .args(["cleanup", "build-cache"])
        .output()
        .unwrap();
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let plain = String::from_utf8(plain.stdout).unwrap();
    for fact in [
        "dry run",
        "build_cache",
        "absent",
        "state_journals_and_messages",
    ] {
        assert!(plain.contains(fact), "{plain}");
    }
}

#[test]
fn cleanup_rejects_paths_and_absent_apply_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let arbitrary = command(&temp).args(["cleanup", "/tmp"]).output().unwrap();
    assert_eq!(arbitrary.status.code(), Some(2));

    for _ in 0..2 {
        let absent = command(&temp)
            .args(["--json", "cleanup", "build-cache", "--apply"])
            .output()
            .unwrap();
        assert!(
            absent.status.success(),
            "{}",
            String::from_utf8_lossy(&absent.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&absent.stdout).unwrap();
        assert_eq!(value["mode"], "apply");
        assert_eq!(value["candidates"][0]["state"], "absent");
    }
}
