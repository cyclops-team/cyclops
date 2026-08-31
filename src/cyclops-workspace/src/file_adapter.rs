//! Local filesystem adapter for the workspace Files panel.
//!
//! [`crate::files::FileTree`] owns navigation and rendered rows. This module
//! owns the operating-system read that satisfies its query, including bounded
//! traversal, metadata inspection, and unreadable-directory facts.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::files::{FileRow, FileTree, RowKind, TreeQuery, TreeSnapshot};

/// Entries read from any one directory. A directory with more than this gets
/// a truncated listing and says so, which beats either freezing the frame or
/// silently showing part of a folder as if it were all of it.
pub(crate) const MAX_ENTRIES_PER_DIR: usize = 500;

/// Depth the adapter will descend to. Reached only by opening folders one at
/// a time, so this is a backstop against a pathological tree, not a limit
/// anyone navigates into.
const MAX_DEPTH: u16 = 16;

/// Move to a new root and install its current local filesystem snapshot.
pub(crate) fn reroot(tree: &mut FileTree, root: impl Into<PathBuf>) -> bool {
    if !tree.reroot(root) {
        return false;
    }
    refresh(tree);
    true
}

/// Replay one previously visited root and install its current snapshot.
pub(crate) fn go_back(tree: &mut FileTree) -> bool {
    if !tree.go_back() {
        return false;
    }
    refresh(tree);
    true
}

/// Replay one forward root and install its current snapshot.
pub(crate) fn go_forward(tree: &mut FileTree) -> bool {
    if !tree.go_forward() {
        return false;
    }
    refresh(tree);
    true
}

/// Change one directory's expansion and install the resulting snapshot.
pub(crate) fn toggle(tree: &mut FileTree, path: &Path) {
    tree.toggle(path);
    refresh(tree);
}

/// Re-read the current bounded tree and say whether anything visible moved.
///
/// The caller decides when an event warrants this request. This adapter never
/// arms a timer or turns an unchanged frame into a redraw.
pub(crate) fn refresh(tree: &mut FileTree) -> bool {
    if !tree.has_root() {
        return false;
    }
    tree.install(read_tree(tree.query()))
}

fn read_tree(query: TreeQuery) -> TreeSnapshot {
    let mut rows = Vec::new();
    let mut stamp = Fnv::new();
    stamp.write(query.root.to_string_lossy().as_bytes());
    walk(&query.root, 0, &query.expanded, &mut rows, &mut stamp);
    TreeSnapshot {
        rows,
        stamp: stamp.finish(),
    }
}

fn walk(
    dir: &Path,
    depth: u16,
    expanded_paths: &BTreeSet<PathBuf>,
    rows: &mut Vec<FileRow>,
    stamp: &mut Fnv,
) {
    if depth >= MAX_DEPTH {
        return;
    }
    let Some((entries, hidden)) = read_dir_sorted(dir) else {
        // Unreadable: no rows, and the failure is part of the stamp so a
        // later explicit refresh notices when permission comes back.
        stamp.write(b"\0unreadable\0");
        stamp.write(dir.to_string_lossy().as_bytes());
        return;
    };
    for entry in entries {
        stamp.write(entry.name.as_bytes());
        stamp.write(&[u8::from(entry.is_dir)]);
        // Size and mtime for files only. A directory row shows a name and a
        // chevron, and its own mtime bumps on every write inside it. An open
        // directory's visible descendants are stamped by the recursion below.
        if !entry.is_dir {
            stamp.write(&entry.size.to_le_bytes());
            stamp.write(&entry.modified.to_le_bytes());
        }

        let expanded = entry.is_dir && expanded_paths.contains(&entry.path);
        rows.push(FileRow {
            name: entry.name,
            depth,
            kind: if entry.is_dir {
                RowKind::Dir { expanded }
            } else {
                RowKind::File
            },
            path: entry.path.clone(),
        });
        if expanded {
            walk(&entry.path, depth + 1, expanded_paths, rows, stamp);
        }
    }
    if hidden > 0 {
        stamp.write(&hidden.to_le_bytes());
        rows.push(FileRow {
            path: dir.to_path_buf(),
            name: String::new(),
            depth,
            kind: RowKind::Truncated { hidden },
        });
    }
}

/// One directory entry, reduced to what the adapter sorts and stamps on.
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    /// Seconds since the epoch, or 0 when the platform will not say.
    modified: u64,
}

/// Read one directory: directories first, then files, each group ordered
/// case-insensitively by name. Returns the entries and how many were cut.
///
/// `.git` is skipped. It is a directory of thousands of files that nobody
/// navigates to and no agent should be handed a path into. Every other dotfile
/// is shown. Symlinks are leaves whatever they point at, so a link to `..`
/// cannot turn a view refresh into an infinite walk.
fn read_dir_sorted(dir: &Path) -> Option<(Vec<Entry>, usize)> {
    let reader = std::fs::read_dir(dir).ok()?;
    let mut entries: Vec<Entry> = Vec::new();
    for entry in reader.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        // `symlink_metadata` does not follow: a link reports as a link.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let is_symlink = meta.is_symlink();
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entries.push(Entry {
            path: entry.path(),
            name,
            is_dir: meta.is_dir() && !is_symlink,
            size: meta.len(),
            modified,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    let hidden = entries.len().saturating_sub(MAX_ENTRIES_PER_DIR);
    entries.truncate(MAX_ENTRIES_PER_DIR);
    Some((entries, hidden))
}

/// FNV-1a, the same 64-bit hash the manifest seeder uses. It lets a refresh
/// answer "did this visible tree change" in one comparison instead of diffing
/// two row lists.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x1000_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
