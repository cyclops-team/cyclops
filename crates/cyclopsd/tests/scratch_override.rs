//! The one test that can fail when `CYCLOPS_TEST_TMP` stops working.
//!
//! The suite's scratch paths all come from `cyclops_proto::scratch`, and
//! three tests used to "cover" that by asserting a path built from
//! `scratch_dir` starts with `scratch_root`, which is true by
//! construction and cannot fail for any reason. Nothing exercised the
//! override branch at all, so the rule F24 cost two milestones to learn
//! could have rotted with the suite still green.
//!
//! This file holds exactly one test on purpose: it sets a process-global
//! environment variable, and cargo runs the tests inside one binary on
//! threads.

use cyclops_proto::scratch::{scratch_dir, scratch_root, SCRATCH_ENV};

#[test]
fn the_override_moves_the_scratch_root_and_everything_built_on_it() {
    // CI runs this suite twice, once with the override already set, so
    // the baseline is measured with it out of the way and put back after.
    let inherited = std::env::var_os(SCRATCH_ENV);
    std::env::remove_var(SCRATCH_ENV);
    let platform_default = scratch_root();

    // Somewhere real, and deliberately not the platform default, so a
    // root that ignored the override cannot pass by coincidence.
    let relocated = std::env::temp_dir().join(format!("cyc-relocated-{}", std::process::id()));
    std::fs::create_dir_all(&relocated).expect("create the relocated root");
    assert_ne!(
        relocated, platform_default,
        "the fixture must differ from the default root to prove anything"
    );

    std::env::set_var(SCRATCH_ENV, &relocated);
    assert_eq!(
        scratch_root(),
        relocated,
        "{SCRATCH_ENV} did not move the root"
    );

    // And everything the suite builds on it moves too, which is the part
    // that matters: this is where sockets and scratch homes get created.
    let dir = scratch_dir("cyc-override-probe");
    assert!(
        dir.starts_with(&relocated),
        "{dir:?} ignored the relocated root {relocated:?}"
    );
    std::fs::create_dir_all(&dir).expect("the relocated root must be usable");
    std::fs::write(dir.join("probe"), b"x").expect("write under the relocated root");
    let _ = std::fs::remove_dir_all(&dir);

    // Exported-but-empty is how a shell passes "unset", and reading it as
    // a path would put every socket in the current directory.
    std::env::set_var(SCRATCH_ENV, "");
    assert_eq!(
        scratch_root(),
        platform_default,
        "an empty {SCRATCH_ENV} must read as unset"
    );

    let _ = std::fs::remove_dir_all(&relocated);
    match inherited {
        Some(v) => std::env::set_var(SCRATCH_ENV, v),
        None => std::env::remove_var(SCRATCH_ENV),
    }
}
