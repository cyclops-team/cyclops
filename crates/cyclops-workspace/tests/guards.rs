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
