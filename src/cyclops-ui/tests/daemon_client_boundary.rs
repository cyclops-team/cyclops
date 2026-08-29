//! Syntactic guard for the shared official daemon connection boundary.

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
    let shared_client = workspace_src.join("cyclops-ui/src/daemon_client.rs");
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
