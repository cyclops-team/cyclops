//! `cyclops hooks install` end to end: the real binary, a scratch
//! CYCLOPS_HOME, no daemon (install renders and instructs without one).
//! No vendor dot-dir is ever touched: HOME is pointed at scratch for the
//! refusal test, and the default dest lives under CYCLOPS_HOME.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Scratch home unique per test and process, under the relocatable
/// scratch root rather than the OS temp dir (F24), so CYCLOPS_TEST_TMP
/// moves this suite's state with the rest of the workspace. That the root
/// really relocates is proven once, in cyclopsd's scratch_override test;
/// restating it here as a starts_with could not fail.
fn scratch_home(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch home");
    dir
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env("HOME", home)
        .args(args)
        .output()
        .expect("run cyclops binary")
}

#[test]
fn install_dry_run_prints_everything_and_writes_nothing() {
    let home = scratch_home("hidry");
    let out = run(
        &home,
        &[
            "hooks",
            "install",
            "claude",
            "--agent",
            "reviewer",
            "--dry-run",
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Would write"), "{stdout}");
    assert!(
        stdout.contains("hook UserPromptSubmit --agent reviewer"),
        "{stdout}"
    );
    assert!(
        stdout.contains("--settings"),
        "wiring instructions: {stdout}"
    );
    assert!(
        !home.join("hooks").exists(),
        "dry run must not write anything"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn install_writes_the_default_dest_and_prints_the_wiring() {
    let home = scratch_home("hiw");
    let vendors = [
        ("claude", "settings.json"),
        ("codex", "hooks.json"),
        ("agy", "hooks.json"),
        ("cursor", "hooks.json"),
    ];
    let mut artifacts = Vec::new();

    for (vendor, file) in vendors {
        let out = run(&home, &["hooks", "install", vendor, "--agent", "reviewer"]);
        assert!(
            out.status.success(),
            "{vendor} stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let path = home.join(format!("hooks/{vendor}/reviewer/{file}"));
        let text = fs::read_to_string(&path).expect("rendered hook artifact");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(value.is_object(), "{vendor}: {text}");
        assert!(text.contains("--agent reviewer"), "{vendor}: {text}");
        artifacts.push((path, text));

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stdout_lower = stdout.to_ascii_lowercase();
        assert!(stdout.contains("Wrote"), "{vendor}: {stdout}");
        assert!(stdout_lower.contains("merge"), "{vendor}: {stdout}");
        assert!(stdout_lower.contains("preserv"), "{vendor}: {stdout}");
        assert!(stdout.contains("hooks verify") || stdout.contains("hooks selftest"));
        assert!(
            !stdout.contains(" cp "),
            "{vendor} prints an overwrite-shaped cp"
        );
        if vendor == "codex" {
            assert!(stdout.contains("CODEX_HOME"), "{stdout}");
            assert!(stdout.contains("trust_level = \"trusted\""), "{stdout}");
            assert!(stdout.contains("/hooks"), "{stdout}");
            assert!(stdout.contains("Reload behavior depends"), "{stdout}");
            assert!(
                stdout.contains("--dangerously-bypass-hook-trust does NOT"),
                "{stdout}"
            );
        }
    }

    // Same-label artifacts are isolated by vendor, even when their filenames
    // collide. A second prepare is byte-for-byte stable and leaves no temp
    // artifacts behind.
    let paths: Vec<_> = artifacts.iter().map(|(path, _)| path).collect();
    assert_eq!(paths.len(), 4);
    for left in 0..paths.len() {
        for right in (left + 1)..paths.len() {
            assert_ne!(paths[left], paths[right]);
        }
    }
    for (vendor, file) in vendors {
        let out = run(&home, &["hooks", "install", vendor, "--agent", "reviewer"]);
        assert!(out.status.success(), "{vendor} second run failed");
        let path = home.join(format!("hooks/{vendor}/reviewer/{file}"));
        let before = artifacts
            .iter()
            .find(|(candidate, _)| candidate == &path)
            .map(|(_, text)| text)
            .expect("first artifact");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            *before,
            "{vendor} changed"
        );
        let temp_files: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            temp_files.is_empty(),
            "{vendor} left temp files: {temp_files:?}"
        );
    }

    // Explicit --dest remains an exact destination directory, rather than
    // gaining the implicit vendor/label suffix.
    let explicit = home.join("prepared");
    let out = run(
        &home,
        &[
            "hooks",
            "install",
            "codex",
            "--agent",
            "reviewer",
            "--dest",
            explicit.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    assert!(explicit.join("hooks.json").is_file());
    assert!(!explicit.join("codex/reviewer/hooks.json").exists());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn install_json_mode_is_machine_clean() {
    let home = scratch_home("hij");
    let out = run(
        &home,
        &[
            "hooks",
            "install",
            "codex",
            "--agent",
            "rev",
            "--dry-run",
            "--json",
        ],
    );
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("one JSON line");
    assert_eq!(v["cli"], "codex");
    assert_eq!(v["written"], false);
    assert!(v["content"].as_str().unwrap().contains("--agent rev"));
    assert!(
        out.stderr.is_empty(),
        "json mode stays machine-clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn install_reserved_label_names_the_naming_step() {
    let home = scratch_home("hirl");
    for reserved in ["%4", "*", "admin"] {
        let out = run(&home, &["hooks", "install", "claude", "--agent", reserved]);
        assert_eq!(out.status.code(), Some(2), "{reserved} must be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        // The wording belongs to cyclops_proto::label, which has its own
        // tests for saying why and offering a way out. What this asserts
        // is that the refusal reaches the operator through this verb, and
        // carries the step that gets them out of it.
        assert!(
            err.contains(&cyclops_proto::label::refusal(reserved).expect("refused")),
            "{reserved} refusal did not carry the shared reason: {err}"
        );
        assert!(err.contains("cyclops status"), "{reserved}: {err}");
        assert!(out.stdout.is_empty(), "refusal must not print wiring");
        assert!(!home.join("hooks").exists(), "refusal must not write");
    }
    let _ = fs::remove_dir_all(&home);
}

/// FNV-1a 64, hex. Restated here rather than reached for: the point is to
/// hash the artifact independently of the code that recorded the hash.
fn fnv64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The receipt is what lets a later `cyclops start` refresh this artifact
/// instead of leaving it pointed at a binary that moved. Without it every
/// prepared file is unmanaged forever.
#[test]
fn install_records_a_receipt_beside_the_artifact() {
    let home = scratch_home("hircpt");
    let out = run(&home, &["hooks", "install", "codex", "--agent", "reviewer"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dir = home.join("hooks/codex/reviewer");
    let artifact = fs::read(dir.join("hooks.json")).expect("rendered hook artifact");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join(".cyclops-prepared.json")).unwrap())
            .expect("receipt is JSON");

    assert_eq!(receipt["vendor"], "codex");
    assert_eq!(receipt["agent"], "reviewer");
    assert_eq!(receipt["file"], "hooks.json");
    assert_eq!(receipt["rendered_fnv"], fnv64(&artifact));
    // The path the commands actually bake, so a later run can tell a
    // prefix move from a template change.
    assert_eq!(receipt["bin"], env!("CARGO_BIN_EXE_cyclops"));
    assert!(
        String::from_utf8_lossy(&artifact).contains(env!("CARGO_BIN_EXE_cyclops")),
        "the receipt must name the path the artifact carries"
    );
    assert!(receipt["written_ms"].as_u64().unwrap_or(0) > 0);

    // An explicit dest outside the refreshed tree says so, because
    // nothing will ever repoint it.
    let explicit = home.join("elsewhere");
    let out = run(
        &home,
        &[
            "hooks",
            "install",
            "codex",
            "--agent",
            "reviewer",
            "--dest",
            explicit.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("will not refresh it"), "{stdout}");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn install_refuses_vendor_dot_dirs() {
    let home = scratch_home("hiv");
    for vendor in [".claude", ".codex", ".gemini", ".agents", ".cursor"] {
        let dest = home.join(vendor).join("sub");
        let out = run(
            &home,
            &[
                "hooks",
                "install",
                "claude",
                "--agent",
                "r",
                "--dest",
                dest.to_str().unwrap(),
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{vendor} must be refused");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("vendor config directory"), "{stderr}");
        assert!(
            !home.join(vendor).exists(),
            "{vendor} must not be created by the refusal"
        );
    }
    let _ = fs::remove_dir_all(&home);
}
