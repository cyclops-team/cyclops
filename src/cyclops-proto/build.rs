//! Stamp one source identity for every Cyclops component.

use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let full_sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let short_sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty());

    let build = if dirty && short_sha != "unknown" {
        format!("{short_sha}.dirty")
    } else {
        short_sha
    };
    let build_id = if dirty && full_sha != "unknown" {
        format!("{full_sha}.dirty")
    } else {
        full_sha
    };
    println!("cargo:rustc-env=CYCLOPS_BUILD_REF={build}");
    println!("cargo:rustc-env=CYCLOPS_BUILD_ID={build_id}");

    // `rerun-if-changed` replaces Cargo's default package-wide watch set.
    // The stamp describes every shipped binary, so changes to sibling crates,
    // workspace dependency selection, or embedded product resources must
    // recompute dirty/clean state too. Do not watch the workspace root: its
    // `target/` directory would make this script trigger itself forever.
    for input in [
        "..",
        "../../Cargo.toml",
        "../../Cargo.lock",
        "../../resources",
        "../../skills",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }

    // Linked worktrees keep HEAD and the branch ref outside this crate.
    // Watching both prevents Cargo from reusing a stamp after a commit.
    if let Some(head) = git(&["rev-parse", "--path-format=absolute", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &reference,
        ]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
