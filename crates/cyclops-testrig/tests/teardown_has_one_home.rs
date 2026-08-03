//! The structural half of the teardown fix: the rule may not be copied.
//!
//! Named for what it guards, not for its shape. Its sibling under
//! `cyclops-proto/tests/` guards a different rule with different
//! machinery, and when both were called `one_place.rs` a reader who had
//! read one assumed they had read the other.
//!
//! Three review rounds fixed tmux teardown at the site the reviewer named
//! and left an identical copy in the next file, each copy missing the
//! unlink. Behavior tests could not catch that, because every copy was
//! green on its own. This test catches it: a second home for the rule is
//! the failure, whether or not that copy happens to be correct today.
//!
//! Two languages, two homes. Rust goes through `cyclops-testrig`; shell,
//! python and prose go through `demos/lib.sh`.
//!
//! Both scans start at the REPO ROOT and skip only what holds no source.
//! The first version of this file scanned `crates`, `demos` and `scripts`,
//! and a fifth home survived it in `tests/m1_soak.py`, with a sixth in
//! `tests/harness/README.md` teaching the leak to every probe written from
//! it. A guard that looks only where the known copies live cannot find the
//! next one, so the scan owns the tree and the exceptions are named here.

use std::path::{Path, PathBuf};

/// Repo root: this crate sits at `<root>/crates/cyclops-testrig`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has two parents")
        .to_path_buf()
}

/// Every regular file under `dir`, skipping directories that hold no
/// source. Missing directories yield nothing.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if holds_no_source(&path) {
                continue;
            }
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Build output, git internals, python bytecode, the vendored landing
/// page, the local agent workflow scripts, and the soak's raw artifacts.
/// The last one matters: those are captured vendor screens and can hold
/// any text at all, including a command a probe typed into a pane.
fn holds_no_source(dir: &Path) -> bool {
    matches!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("target" | ".git" | "__pycache__" | "frontend" | ".claude")
    ) || dir.ends_with("tests/raw")
}

/// Every source file in the tree.
fn tree() -> Vec<PathBuf> {
    let mut files = Vec::new();
    files_under(&repo_root(), &mut files);
    files
}

#[test]
fn tmux_teardown_has_exactly_one_home_in_rust() {
    // This crate is the one home: its source states the rule and its tests
    // quote it. Everything else may drive tmux but may not kill it.
    let rig = repo_root().join("crates/cyclops-testrig");
    let mut offenders = Vec::new();
    for path in tree() {
        if path.starts_with(&rig) || path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // The quoted literal is the command being passed to tmux; prose
        // about the rule (this crate's doc comments, and the tests that
        // point at it) is not a second home for it.
        if text.contains("Command::new(\"tmux\")") && text.contains("\"kill-server\"") {
            offenders.push(path);
        }
    }
    assert!(
        offenders.is_empty(),
        "these files kill a tmux server themselves instead of dropping a \
         cyclops_testrig::TmuxServer, which is how the unlink gets forgotten \
         again: {offenders:#?}"
    );
}

/// The other half, and the one that had the blind spot. Outside Rust there
/// is no compiler to route callers through the rig, so the check is the
/// command name itself: writing it down is how a copy starts, and that goes
/// for a README example as much as for a script.
#[test]
fn tmux_teardown_has_exactly_one_home_outside_rust() {
    let lib = repo_root().join("demos/lib.sh");
    let mut offenders = Vec::new();
    for path in tree() {
        if path == lib || path.extension().is_some_and(|e| e == "rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.contains("kill-server") {
            offenders.push(path);
        }
    }
    assert!(
        offenders.is_empty(),
        "these files stop a tmux server themselves instead of calling \
         cyc_tmux_teardown from demos/lib.sh, which is where the unlink and \
         the already-gone fallback live (python sources it through bash, see \
         tests/m1_soak.py): {offenders:#?}"
    );
}
