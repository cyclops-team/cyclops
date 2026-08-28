//! Stamp the commit into the binary so an installed cyclopsd can say
//! which build it is. The boot log line carries it; a build outside a
//! git checkout (a source tarball) stamps "unknown".

use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let full_sha = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let build_ref = if dirty && sha != "unknown" {
        format!("{sha}.dirty")
    } else {
        sha
    };
    println!("cargo:rustc-env=CYCLOPS_BUILD_REF={build_ref}");
    let build_id = if dirty && full_sha != "unknown" {
        format!("{full_sha}.dirty")
    } else {
        full_sha
    };
    println!("cargo:rustc-env=CYCLOPS_BUILD_ID={build_id}");

    // A linked worktree's ../../.git is a pointer file, not the HEAD whose
    // contents advance. Watch both the actual HEAD and its symbolic target so
    // a new commit cannot reuse build metadata cached for its predecessor.
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
