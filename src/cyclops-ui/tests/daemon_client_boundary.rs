//! Structural guards for transport and reusable-presentation ownership.
//!
//! `cyclops-client` owns connection facts, framing, correlation, timeout
//! classification, refusal decoding, post-write uncertainty, and gap signals.
//! The applications retain retry schedules and projection restoration because
//! those decisions depend on the state each application presents.
//!
//! This guard becomes obsolete when module or crate visibility makes the raw
//! daemon socket constructor unreachable to official callers, so a second
//! connection path is impossible to compile instead of merely linted.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn official_callers_do_not_open_daemon_sockets_directly() {
    let workspace_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate lives under workspace src");
    let shared_client = workspace_src.join("cyclops-client/src/lib.rs");
    let mut files = Vec::new();
    for crate_name in ["cyclops", "cyclops-ui", "cyclops-workspace"] {
        rust_files(&workspace_src.join(crate_name).join("src"), &mut files);
    }

    for path in files {
        if path == shared_client {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        assert!(
            !source.contains("UnixStream::connect("),
            "raw daemon socket connection found outside shared client: {}",
            path.display()
        );
    }
}

#[test]
fn reusable_presentation_has_no_journal_or_tmux_dependency() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        std::fs::read_to_string(crate_root.join("Cargo.toml")).expect("read cyclops-ui manifest");
    let production = manifest
        .split_once("[dev-dependencies]")
        .map_or(manifest.as_str(), |(production, _)| production);
    for forbidden in ["cyclops-ledger", "cyclops-state", "cyclops-tmux"] {
        assert!(
            !production.contains(forbidden),
            "presentation recovered a production {forbidden} dependency"
        );
    }

    let mut files = Vec::new();
    rust_files(&crate_root.join("src"), &mut files);
    for path in files {
        let source = std::fs::read_to_string(&path).expect("read presentation source");
        for forbidden in ["UnixStream::connect(", "join(\"ledger\")", "focus_pane("] {
            assert!(
                !source.contains(forbidden),
                "presentation mechanism {forbidden:?} found in {}",
                path.display()
            );
        }
    }
}
