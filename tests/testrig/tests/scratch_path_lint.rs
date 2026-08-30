//! Syntactic lint for real-tmux fixture scratch ownership.
//!
//! A test source that opens a `cyclops-testrig` server must derive filesystem
//! scratch from `cyclops_proto::scratch`, never the platform temp API or a
//! hardcoded temp literal. This is intentionally a prohibited-source lint, not
//! runtime evidence that paths work. A non-filesystem fixture value may carry
//! `scratch-path-lint:` on its line to state why it is not an owned path. The
//! focused relocated daemon journey owns the behavioral proof.
//!
//! This lint becomes obsolete when the tmux rig constructor requires a typed
//! scratch owner, making an unrelocatable fixture impossible to compile.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn real_tmux_fixtures_do_not_bypass_the_scratch_root() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tests/testrig has two parents");
    let this_file = Path::new(file!());
    let mut files = Vec::new();
    rust_files(repo, &mut files);
    let mut offenders = Vec::new();
    let forbidden = [
        "temp_dir()",
        "tempdir()",
        "tempfile::tempdir()",
        ".tempdir()",
        "TempDir::new()",
        "\"/tmp",
        "\"/private/tmp",
    ];

    for path in files {
        if path.ends_with(this_file) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let owns_tmux = source.contains("cyclops_testrig")
            || source.contains("TmuxServer")
            || source.contains("TmuxGuard");
        if !owns_tmux {
            continue;
        }
        for (index, line) in source.lines().enumerate() {
            if line.contains("scratch-path-lint:") {
                continue;
            }
            for pattern in forbidden {
                if line.contains(pattern) {
                    offenders.push(format!(
                        "{}:{} contains {pattern:?}",
                        path.display(),
                        index + 1
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "real-tmux fixtures bypass the relocatable scratch root:\n{}",
        offenders.join("\n")
    );
}
