//! The demos' daemon-stop helper has one home, `tests/e2e/lib/lib.sh`.
//!
//! It had three. `stop_daemon` was pasted byte for byte into
//! `demos/m4-name.sh`, `demos/m5-theme.sh` and `tests/e2e/parity-check.sh`
//! (then `demos/parity-check.sh`) while the file whose header says "source
//! it, do not paste" sat next to them. Nothing was wrong with any copy,
//! which is the problem: the next change to the rule fixes one of three and
//! leaves two behind, each green on its own. Same shape as
//! `tests/testrig/tests/teardown_has_one_home.rs`, which guards the tmux
//! half of the same file, and it lives in this crate because the process
//! being stopped is cyclopsd.
//!
//! Scope, stated because the check is narrower than it could be: this
//! catches a helper FUNCTION, which is what a new demo copies when it is
//! written from an old one. Four demos still stop their daemon inline in
//! `cleanup` (`m0-status`, `m1-send`, `m2-conversation`, `m4-workspace`,
//! and `m3-stream` over a pid list), and two of those are a deliberate
//! variant: they run cyclopsd under `cargo run`, so they `pkill` the child
//! before the parent. Folding a variant into the shared rule needs the
//! variant understood first, and that is not this change.

use std::path::{Path, PathBuf};

/// Repo root: this crate sits at `<root>/src/cyclopsd`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("src/<crate> has two parents")
        .to_path_buf()
}

/// Every regular file under `dir`, recursively. A missing directory yields
/// nothing.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Captured vendor screens: any text at all can be in them.
            if path.file_name().and_then(|n| n.to_str()) == Some("raw") {
                continue;
            }
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// What the copies looked like, and what a demo written from one of them
/// would carry.
const DEFINES_A_STOPPER: &str = "stop_daemon() {";

/// The one home, and the name every demo calls.
const THE_HOME: &str = "cyc_stop_daemon() {";

#[test]
fn stopping_the_daemon_has_exactly_one_home() {
    let root = repo_root();
    let lib = root.join("tests/e2e/lib/lib.sh");
    let mut files = Vec::new();
    // The two places a shell or python rig lives. A Rust rig stops the
    // daemon by dropping it, which is a different rule in a different
    // language with its own home.
    files_under(&root.join("demos"), &mut files);
    files_under(&root.join("tests"), &mut files);

    let mut offenders = Vec::new();
    for path in files {
        if path == lib {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.contains(DEFINES_A_STOPPER) {
            offenders.push(path);
        }
    }
    assert!(
        offenders.is_empty(),
        "these files define their own daemon stop instead of calling \
         cyc_stop_daemon from tests/e2e/lib/lib.sh, which is where the wait \
         and the safe-to-call-twice rule live: {offenders:#?}"
    );

    let text = std::fs::read_to_string(&lib).expect("read tests/e2e/lib/lib.sh");
    assert!(
        text.contains(THE_HOME),
        "tests/e2e/lib/lib.sh no longer defines the rule it is the home of"
    );
}
