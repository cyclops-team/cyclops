//! Guard: no interval timers and no direct tmux invocation in the workspace crate.

use std::path::{Path, PathBuf};

fn read_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            read_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_interval_timers_in_workspace_src() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    read_rs_files(&src, &mut files);
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            !text.contains("time::interval(") && !text.contains("tokio::time::interval"),
            "interval timer found in {}",
            path.display()
        );
    }
}

#[test]
fn no_direct_tmux_invocation_in_workspace() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    read_rs_files(&src, &mut files);
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            !text.contains("Command::new(\"tmux\")"),
            "tmux spawn found in {}",
            path.display()
        );
    }
}

/// This syntactic tripwire checks only the spellings listed below. It does
/// not prove Files behavior or exclude every filesystem API; review enforces
/// the broader ownership rule.
#[test]
fn file_tree_model_does_not_name_listed_filesystem_spellings() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/files.rs");
    let text = std::fs::read_to_string(&path).expect("read file tree model");
    let production = text
        .split_once("#[cfg(test)]")
        .map_or(text.as_str(), |(production, _)| production);
    for forbidden in [
        "std::fs::",
        "std::time::UNIX_EPOCH",
        "read_dir(",
        ".metadata(",
    ] {
        assert!(
            !production.contains(forbidden),
            "filesystem operation {forbidden:?} found in {}",
            path.display()
        );
    }
}
