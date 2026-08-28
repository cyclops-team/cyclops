//! Stamp the commit into the binary so an installed cyclops can say
//! which build it is. `cyclops --version` prints it; a build outside a
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
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let build_ref = if dirty && sha != "unknown" {
        format!("{sha}.dirty")
    } else {
        sha
    };
    println!("cargo:rustc-env=CYCLOPS_BUILD_REF={build_ref}");

    // Cargo resolves rerun paths from this crate, while git normally prints
    // them relative to the repository. Ask for absolute paths so both a
    // normal checkout and a linked worktree follow the real HEAD and branch
    // ref instead of a nonexistent src/cyclops/.git path.
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
