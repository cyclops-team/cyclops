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
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
