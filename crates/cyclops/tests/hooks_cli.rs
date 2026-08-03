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
    // claude: settings fragment, --settings wiring.
    let out = run(
        &home,
        &["hooks", "install", "claude", "--agent", "reviewer"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = home.join("hooks/reviewer/settings.json");
    let text = fs::read_to_string(&path).expect("rendered settings.json");
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert!(v["hooks"]["UserPromptSubmit"].is_array(), "{text}");
    assert!(text.contains("--agent reviewer"), "{text}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Wrote"), "{stdout}");
    assert!(stdout.contains("--settings"), "{stdout}");
    assert!(stdout.contains("hooks selftest reviewer"), "{stdout}");

    // codex: same dest dir, hooks.json, and the F1 caveats printed, not
    // applied: CODEX_HOME copy, the trust seed line, and the flag that
    // does NOT work.
    let out = run(&home, &["hooks", "install", "codex", "--agent", "reviewer"]);
    assert!(out.status.success());
    let path = home.join("hooks/reviewer/hooks.json");
    let text = fs::read_to_string(&path).expect("rendered hooks.json");
    serde_json::from_str::<serde_json::Value>(&text).expect("valid JSON");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CODEX_HOME"), "{stdout}");
    assert!(stdout.contains("trust_level = \"trusted\""), "{stdout}");
    assert!(
        stdout.contains("--dangerously-bypass-hook-trust does NOT"),
        "{stdout}"
    );

    // agy: .agents/hooks.json placement instructions.
    let out = run(&home, &["hooks", "install", "agy", "--agent", "reviewer"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(".agents/hooks.json"), "{stdout}");
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
        let expected = format!(
            "--agent needs a real label; {reserved:?} is reserved. Name the pane \
             first: cyclops status shows every pane and its label, and {} \
             names the watched sessions. Then rerun cyclops hooks install \
             with that label.",
            home.join("config.toml").display()
        );
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), expected);
        assert!(out.stdout.is_empty(), "refusal must not print wiring");
        assert!(!home.join("hooks").exists(), "refusal must not write");
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn install_refuses_vendor_dot_dirs() {
    let home = scratch_home("hiv");
    for vendor in [".claude", ".codex", ".gemini", ".agents"] {
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
