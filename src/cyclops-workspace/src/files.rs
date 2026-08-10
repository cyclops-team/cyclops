//! The sidebar's file tree: what is on disk under one root, flattened into
//! the rows a panel paints.
//!
//! Everything here is a pure function of a path and a set of expanded
//! directories. It reads the filesystem and nothing else: no tmux, no
//! daemon, no terminal. Painting belongs to `render::files`, and deciding
//! what a click on a row DOES belongs to `app`.
//!
//! Reads are lazy. Only the root and the directories the operator has
//! opened are listed, so the cost of the panel is the cost of what is on
//! screen rather than the cost of the project. A repository with a hundred
//! thousand files under it costs one `read_dir` until someone opens a
//! folder.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Entries read from any one directory. A directory with more than this
/// gets a truncated listing and says so, which beats either freezing the
/// frame or silently showing part of a folder as if it were all of it.
const MAX_ENTRIES_PER_DIR: usize = 500;

/// Depth the tree will descend to. Reached only by opening folders one at
/// a time, so this is a backstop against a pathological tree, not a limit
/// anyone navigates into.
const MAX_DEPTH: u16 = 16;

/// Folders remembered on either side of the current one. Deep enough that
/// no real session reaches the end of it, bounded so a workspace left open
/// for a week does not accumulate a list nobody will ever step back through.
const MAX_HISTORY: usize = 64;

/// One painted line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: PathBuf,
    /// The entry's own name, not its path: the row shows a name and its
    /// indent, the way a file tree reads.
    pub name: String,
    /// Indent level; the root's children are 0.
    pub depth: u16,
    pub kind: RowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A directory, and whether it is open. Clicking toggles it.
    Dir { expanded: bool },
    /// Anything that is not a directory the tree will descend into:
    /// regular files, and symlinks of every kind. Clicking sends its path.
    File,
    /// The tail of a directory whose listing was cut at
    /// [`MAX_ENTRIES_PER_DIR`]. Not a path; clicking does nothing.
    Truncated { hidden: usize },
}

impl FileRow {
    /// The type tag this row leads with, lowercased: `md` for `HELLO.md`.
    ///
    /// It exists to survive a clip. The panel is narrow and long names get
    /// cut, and a row cut to `(md) HELL…` still says what the thing is,
    /// where `HELLO.m…` says nothing. That is the whole reason the type
    /// moved to the front of the row instead of staying on the end of the
    /// name where it cannot be relied on.
    ///
    /// `Path::extension` decides what counts rather than a split on '.',
    /// because it already answers the cases a hand-rolled one gets wrong: a
    /// dotfile is its own name and not an extension of something, so
    /// `.zshrc` has no tag; `archive.tar.gz` is a `gz`; `Makefile` has
    /// none. A name ending in a dot yields an empty extension, which is not
    /// a tag anyone can read, so it is dropped here.
    ///
    /// Directories have none. Their chevron already says what they are, and
    /// a folder named `my.folder` is not a `folder` file.
    pub fn type_tag(&self) -> Option<String> {
        if !matches!(self.kind, RowKind::File) {
            return None;
        }
        let ext = Path::new(&self.name).extension()?.to_string_lossy();
        (!ext.is_empty()).then(|| ext.to_lowercase())
    }
}

/// The sidebar's view of one directory tree.
#[derive(Debug, Clone, Default)]
pub struct FileTree {
    root: PathBuf,
    /// The folder a reference is written relative to, which is NOT the
    /// folder being browsed.
    ///
    /// Clicking a folder moves the view. The agent this panel is writing to
    /// has not moved with it: after walking into `src`, a click on
    /// `main.rs` still has to send `src/main.rs`, because the recipient
    /// resolves what it is sent against its own working directory. Set by
    /// the pane probe, never by browsing.
    anchor: PathBuf,
    rows: Vec<FileRow>,
    /// Directories the operator has opened. Kept even when the tree is
    /// rerooted or a directory disappears, so re-entering a folder that
    /// came back finds it as it was left.
    expanded: BTreeSet<PathBuf>,
    /// First visible row, moved by the wheel.
    scroll: usize,
    /// Folders stood in before this one, oldest first. Pushed by every
    /// move the operator makes, including the climb out through `..`.
    back: Vec<PathBuf>,
    /// Folders stepped back out of, most recent last. Cleared the moment
    /// the operator navigates somewhere new, because that move is a new
    /// branch and what they had stepped out of is no longer ahead of them.
    /// Session state only: where someone browsed is not worth restoring on
    /// the next launch, so [`crate::persist`] never sees these.
    forward: Vec<PathBuf>,
    /// What the last read saw, for [`FileTree::refresh`] to compare
    /// against. Not a hash of file CONTENT: this panel reports what is in
    /// a folder, so a write that leaves name, size and mtime alone is not
    /// something it has anything to say about.
    stamp: u64,
}

impl FileTree {
    /// An empty tree rooted nowhere. Paints as the "no folder yet" state
    /// until [`FileTree::reroot`] gives it somewhere to look.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rows(&self) -> &[FileRow] {
        &self.rows
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn has_root(&self) -> bool {
        !self.root.as_os_str().is_empty()
    }

    /// Point the tree at a different directory and read it.
    ///
    /// The scroll goes back to the top, because the row that was under the
    /// eye belonged to the old folder. `expanded` is kept: it is keyed by
    /// absolute path, so it only ever matches folders under a root that
    /// contains them, and walking up and back down again should not close
    /// everything on the way.
    pub fn reroot(&mut self, root: impl Into<PathBuf>) {
        let root = root.into();
        if root == self.root {
            return;
        }
        // Standing somewhere is what makes it worth remembering. The first
        // reroot of a session moves off no folder at all, and a history
        // entry for nowhere is a back button that empties the panel.
        if self.has_root() {
            self.push_back(self.root.clone());
        }
        self.forward.clear();
        self.land_on(root);
    }

    /// Step back to the folder before this one. False when there is none,
    /// so a caller can paint the control as unavailable rather than
    /// offering a click that does nothing.
    pub fn go_back(&mut self) -> bool {
        let Some(previous) = self.back.pop() else {
            return false;
        };
        self.forward.push(self.root.clone());
        self.land_on(previous);
        true
    }

    /// Step forward again, undoing a [`FileTree::go_back`].
    pub fn go_forward(&mut self) -> bool {
        let Some(next) = self.forward.pop() else {
            return false;
        };
        self.push_back(self.root.clone());
        self.land_on(next);
        true
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Take up a new root and read it. The scroll goes back to the top for
    /// the reason [`FileTree::reroot`] gives; history is the caller's to
    /// record, because stepping through history must not record itself.
    fn land_on(&mut self, root: PathBuf) {
        self.root = root;
        self.scroll = 0;
        self.read();
    }

    /// Oldest entry falls off the front. Dropping the far end of a long
    /// trail costs the operator nothing; growing without bound does.
    fn push_back(&mut self, path: PathBuf) {
        self.back.push(path);
        if self.back.len() > MAX_HISTORY {
            self.back.remove(0);
        }
    }

    /// The directory above the current root, if there is one and it is
    /// readable. `None` at a filesystem root, which is where climbing
    /// stops.
    pub fn parent(&self) -> Option<PathBuf> {
        self.root.parent().map(Path::to_path_buf)
    }

    /// Open or close `path`, then re-read. A directory that is not in the
    /// tree is ignored rather than added: the caller passes rows it was
    /// painted, and a row that has since gone is not something to open.
    pub fn toggle(&mut self, path: &Path) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
        self.read();
    }

    /// Re-read the tree and say whether anything a reader would see moved.
    ///
    /// This is the poll's whole job. It costs one `read_dir` per open
    /// directory, and returns false almost every time, which is what lets
    /// it run on a timer without turning the workspace into a spinning
    /// redraw.
    pub fn refresh(&mut self) -> bool {
        if !self.has_root() {
            return false;
        }
        let before = self.stamp;
        self.read();
        self.stamp != before
    }

    /// Scroll the visible window, clamped so the list cannot be scrolled
    /// past its own end or above its start. `height` is the number of rows
    /// the panel can show.
    pub fn scroll_by(&mut self, delta: i16, height: usize) {
        let max = self.rows.len().saturating_sub(height);
        let next = if delta.is_negative() {
            self.scroll
                .saturating_sub(usize::from(delta.unsigned_abs()))
        } else {
            self.scroll.saturating_add(delta as usize)
        };
        self.scroll = next.min(max);
    }

    /// Hold the scroll inside a list that may have shrunk since the wheel
    /// last moved. Called by the painter, which is the only place that
    /// knows how tall the panel actually is.
    pub fn clamp_scroll(&mut self, height: usize) {
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(height));
    }

    /// `path` written the way a message should carry it: relative to the
    /// tree's root when it is under it, absolute when it is not.
    ///
    /// Relative because the recipient of `@src/main.rs` is an agent whose
    /// working directory is this root, and an absolute path from another
    /// machine's home directory is noise in a transcript.
    pub fn reference(&self, path: &Path) -> String {
        path.strip_prefix(self.reference_root())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// Point references at the focused pane's own folder. Called by the
    /// pane probe, which is the only thing that knows where the agent
    /// actually is.
    pub fn anchor_at(&mut self, path: impl Into<PathBuf>) {
        self.anchor = path.into();
    }

    /// Empty until the first probe answers. Falling back to the browsing
    /// root keeps a hand-built tree writing relative paths instead of
    /// absolute ones, which is what every test that builds one expects.
    fn reference_root(&self) -> &Path {
        if self.anchor.as_os_str().is_empty() {
            &self.root
        } else {
            &self.anchor
        }
    }

    fn read(&mut self) {
        let mut rows = Vec::new();
        let mut stamp = Fnv::new();
        stamp.write(self.root.to_string_lossy().as_bytes());
        self.walk(&self.root.clone(), 0, &mut rows, &mut stamp);
        self.rows = rows;
        self.stamp = stamp.finish();
    }

    fn walk(&self, dir: &Path, depth: u16, rows: &mut Vec<FileRow>, stamp: &mut Fnv) {
        if depth >= MAX_DEPTH {
            return;
        }
        let Some((entries, hidden)) = read_dir_sorted(dir) else {
            // Unreadable: no rows, and the failure is part of the stamp so
            // the poll notices when permission comes back.
            stamp.write(b"\0unreadable\0");
            stamp.write(dir.to_string_lossy().as_bytes());
            return;
        };
        for entry in entries {
            stamp.write(entry.name.as_bytes());
            stamp.write(&[u8::from(entry.is_dir)]);
            // Size and mtime for files only. A directory row shows a name
            // and a chevron, and its own mtime bumps every time anything
            // is written anywhere inside it — stamping that would report a
            // change for a closed folder whose row did not move a pixel,
            // and the poll would redraw the workspace on every write in
            // the project. An OPEN folder's contents are stamped by the
            // recursion below, so nothing visible goes unwatched.
            if !entry.is_dir {
                stamp.write(&entry.size.to_le_bytes());
                stamp.write(&entry.modified.to_le_bytes());
            }

            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
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
                self.walk(&entry.path, depth + 1, rows, stamp);
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
}

/// One directory entry, reduced to what the tree sorts and stamps on.
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
/// navigates to and no agent should be handed a path into, and leaving it
/// in would put a folder that dwarfs the project at the top of every tree.
/// Every other dotfile is shown: they are the config an operator actually
/// edits, and a tree that hides them is a tree you cannot trust.
///
/// Symlinks are leaves whatever they point at. Following them is how a
/// tree walk finds a cycle, and one symlink to `..` would otherwise hang
/// the frame that painted it.
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

/// FNV-1a, the same 64-bit hash the manifest seeder uses. Not a checksum
/// of anything anyone stores: it exists so a poll can answer "did this
/// change" in one comparison instead of diffing two row lists.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = cyclops_proto::scratch::scratch_dir(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            Scratch(dir)
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(&path).expect("mkdir");
            path
        }

        fn file(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir -p");
            }
            std::fs::write(&path, body).expect("write");
            path
        }

        fn tree(&self) -> FileTree {
            let mut tree = FileTree::new();
            tree.reroot(&self.0);
            tree
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn names(tree: &FileTree) -> Vec<String> {
        tree.rows().iter().map(|r| r.name.clone()).collect()
    }

    /// Directories first, then files, each case-insensitively by name. A
    /// tree that ordered by raw bytes would file every capitalised name
    /// before every lowercase one, which is not how anyone reads a folder.
    #[test]
    fn a_folder_lists_directories_first_then_files_ignoring_case() {
        let s = Scratch::new("files-order");
        s.file("zebra.rs", "");
        s.file("Apple.rs", "");
        s.file("mango.rs", "");
        s.dir("src");
        s.dir("Docs");

        assert_eq!(
            names(&s.tree()),
            vec!["Docs", "src", "Apple.rs", "mango.rs", "zebra.rs"]
        );
    }

    /// Nothing below the root is read until a folder is opened. That is the
    /// whole reason this panel can sit over a large repository.
    #[test]
    fn children_appear_only_once_their_folder_is_opened() {
        let s = Scratch::new("files-lazy");
        s.file("src/main.rs", "");
        s.file("src/lib.rs", "");
        s.file("README.md", "");
        let mut tree = s.tree();

        assert_eq!(names(&tree), vec!["src", "README.md"]);

        tree.toggle(&s.0.join("src"));
        assert_eq!(names(&tree), vec!["src", "lib.rs", "main.rs", "README.md"]);
        assert_eq!(
            tree.rows()[1].depth,
            1,
            "a child indents one level under its folder"
        );
        assert!(matches!(
            tree.rows()[0].kind,
            RowKind::Dir { expanded: true }
        ));

        tree.toggle(&s.0.join("src"));
        assert_eq!(names(&tree), vec!["src", "README.md"]);
    }

    /// The poll's contract: cheap, and false unless a reader would see
    /// something different.
    #[test]
    fn refresh_reports_only_changes_a_reader_would_see() {
        let s = Scratch::new("files-refresh");
        s.file("a.txt", "one");
        let mut tree = s.tree();

        assert!(!tree.refresh(), "an untouched folder reports no change");

        s.file("b.txt", "two");
        assert!(tree.refresh(), "a new file is a change");
        assert_eq!(names(&tree), vec!["a.txt", "b.txt"]);
        assert!(!tree.refresh(), "and only once");

        std::fs::remove_file(s.0.join("a.txt")).expect("rm");
        assert!(tree.refresh(), "a deleted file is a change");
        assert_eq!(names(&tree), vec!["b.txt"]);

        // A folder nobody opened is not read, so churn inside it is not a
        // change this panel has anything to say about.
        s.dir("closed");
        assert!(tree.refresh(), "the new folder itself shows");
        s.file("closed/deep.txt", "");
        assert!(
            !tree.refresh(),
            "but a file inside a closed folder is not on screen"
        );
    }

    /// A file written inside an OPEN folder is on screen, so it counts.
    #[test]
    fn refresh_sees_into_the_folders_that_are_open() {
        let s = Scratch::new("files-refresh-open");
        s.dir("src");
        let mut tree = s.tree();
        tree.toggle(&s.0.join("src"));
        assert!(!tree.refresh());

        s.file("src/new.rs", "");
        assert!(tree.refresh());
        assert_eq!(names(&tree), vec!["src", "new.rs"]);
    }

    /// A symlink is a leaf whatever it points at. The self-referential case
    /// is the one that matters: followed, it is an infinite walk, and the
    /// frame that painted it never returns.
    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_cannot_hang_the_walk() {
        let s = Scratch::new("files-symlink");
        s.dir("real");
        std::os::unix::fs::symlink(&s.0, s.0.join("real/loop")).expect("symlink");
        let mut tree = s.tree();
        tree.toggle(&s.0.join("real"));

        // Terminates, and the link is a leaf rather than a door.
        let rows = tree.rows();
        let link = rows
            .iter()
            .find(|r| r.name == "loop")
            .expect("the link is listed");
        assert_eq!(link.kind, RowKind::File, "a link is never descended into");
    }

    /// `.git` is skipped; every other dotfile is shown. Hiding the ones an
    /// operator edits would make the tree lie about what is in the folder.
    #[test]
    fn the_tree_hides_git_and_nothing_else() {
        let s = Scratch::new("files-dotfiles");
        s.file(".git/HEAD", "ref: refs/heads/main");
        s.file(".gitignore", "target");
        s.file(".env", "");
        s.file("Cargo.toml", "");

        assert_eq!(names(&s.tree()), vec![".env", ".gitignore", "Cargo.toml"]);
    }

    /// A folder too big to paint is cut, and says how much it cut. Silence
    /// here would show part of a folder as though it were all of it.
    #[test]
    fn an_enormous_folder_is_cut_and_admits_it() {
        let s = Scratch::new("files-huge");
        for n in 0..MAX_ENTRIES_PER_DIR + 25 {
            s.file(&format!("f{n:05}.txt"), "");
        }
        let tree = s.tree();

        assert_eq!(tree.rows().len(), MAX_ENTRIES_PER_DIR + 1);
        assert_eq!(
            tree.rows().last().map(|r| r.kind),
            Some(RowKind::Truncated { hidden: 25 })
        );
    }

    /// The path a message carries is relative to the root, because the
    /// recipient's working directory is that root.
    #[test]
    fn a_reference_is_relative_to_the_root() {
        let s = Scratch::new("files-reference");
        s.file("src/main.rs", "");
        let tree = s.tree();

        assert_eq!(tree.reference(&s.0.join("src/main.rs")), "src/main.rs");
        // Something outside the root keeps its absolute path: a relative
        // one would resolve to the wrong file at the other end.
        let outside = PathBuf::from("/etc/hosts");
        assert_eq!(tree.reference(&outside), "/etc/hosts");
    }

    /// Walking into a folder moves the view, not the agent.
    ///
    /// The reference is still written from where the pane is. Without this
    /// separation, drilling into `src` and clicking `main.rs` sends
    /// `@main.rs` to an agent whose working directory is the project root,
    /// and it resolves to a file that is not there.
    #[test]
    fn browsing_into_a_folder_does_not_move_what_a_reference_is_relative_to() {
        let s = Scratch::new("files-anchor");
        s.file("src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(&s.0);
        tree.anchor_at(&s.0);
        assert_eq!(tree.reference(&s.0.join("src/main.rs")), "src/main.rs");

        tree.reroot(s.0.join("src"));
        assert_eq!(
            tree.reference(&s.0.join("src/main.rs")),
            "src/main.rs",
            "the recipient's working directory did not move with the view"
        );

        // Climbing back out is the same story from the other side.
        tree.reroot(&s.0);
        assert_eq!(tree.reference(&s.0.join("src/main.rs")), "src/main.rs");
    }

    /// An unreadable folder is empty, not fatal, and comes back on its own
    /// when permission does.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_paints_empty_and_recovers() {
        use std::os::unix::fs::PermissionsExt;

        let s = Scratch::new("files-denied");
        s.file("locked/secret.txt", "");
        let locked = s.0.join("locked");
        let mut tree = s.tree();
        tree.toggle(&locked);
        assert_eq!(names(&tree), vec!["locked", "secret.txt"]);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        // Running as root defeats the point of the test; skip rather than
        // assert something the environment cannot produce.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("un");
            return;
        }
        assert!(tree.refresh(), "losing access is a change");
        assert_eq!(names(&tree), vec!["locked"], "the folder is still listed");

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("unlock");
        assert!(tree.refresh(), "regaining access is a change too");
        assert_eq!(names(&tree), vec!["locked", "secret.txt"]);
    }

    /// Reopening a folder finds it as it was left. `expanded` is keyed by
    /// absolute path, so climbing out of a tree and back in does not close
    /// everything on the way.
    #[test]
    fn walking_out_and_back_keeps_the_folders_that_were_open() {
        let s = Scratch::new("files-reroot");
        s.file("project/src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(s.0.join("project"));
        tree.toggle(&s.0.join("project/src"));
        assert_eq!(names(&tree), vec!["src", "main.rs"]);

        tree.reroot(tree.parent().expect("a parent above project"));
        assert_eq!(names(&tree), vec!["project"]);

        tree.reroot(s.0.join("project"));
        assert_eq!(
            names(&tree),
            vec!["src", "main.rs"],
            "src was left open and is still open"
        );
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let s = Scratch::new("files-scroll");
        for n in 0..10 {
            s.file(&format!("f{n}.txt"), "");
        }
        let mut tree = s.tree();

        tree.scroll_by(-1, 4);
        assert_eq!(tree.scroll(), 0, "cannot scroll above the first row");
        tree.scroll_by(3, 4);
        assert_eq!(tree.scroll(), 3);
        tree.scroll_by(100, 4);
        assert_eq!(tree.scroll(), 6, "the last row stays on screen");
        // A panel taller than the list has nothing to scroll.
        tree.clamp_scroll(40);
        assert_eq!(tree.scroll(), 0);
    }

    /// The type tag is the part of a row that has to survive being cut, so
    /// what counts as one matters. The cases here are the ones a split on
    /// '.' gets wrong.
    #[test]
    fn a_type_tag_is_only_the_part_that_names_a_kind() {
        let row = |name: &str, kind: RowKind| FileRow {
            path: PathBuf::from(name),
            name: name.to_string(),
            depth: 0,
            kind,
        };
        let tag = |name: &str| row(name, RowKind::File).type_tag();

        assert_eq!(tag("HELLO.md").as_deref(), Some("md"));
        assert_eq!(tag("main.rs").as_deref(), Some("rs"));
        assert_eq!(tag("Cargo.toml").as_deref(), Some("toml"));
        // Case is display, not identity: .MD and .md are one kind.
        assert_eq!(tag("READ.MD").as_deref(), Some("md"));
        // The last extension wins; a tarball is a gz.
        assert_eq!(tag("archive.tar.gz").as_deref(), Some("gz"));

        // A dotfile is its own name. `.zshrc` is not a `zshrc` file, and
        // tagging it as one would put a nonsense badge on every config an
        // operator edits.
        assert_eq!(tag(".zshrc"), None);
        assert_eq!(tag(".gitignore"), None);
        assert_eq!(tag(".env"), None);
        // But a dotfile that really does carry one keeps it.
        assert_eq!(tag(".eslintrc.json").as_deref(), Some("json"));

        assert_eq!(tag("Makefile"), None);
        assert_eq!(tag("LICENSE"), None);
        // A trailing dot leaves an empty extension, which is not a tag.
        assert_eq!(tag("weird."), None);

        // Folders are told apart by their chevron. `my.folder` is not a
        // `folder` file, and a truncation notice is not a thing at all.
        assert_eq!(
            row("my.folder", RowKind::Dir { expanded: false }).type_tag(),
            None
        );
        assert_eq!(
            row("x.rs", RowKind::Truncated { hidden: 3 }).type_tag(),
            None
        );
    }

    /// Walking into folders and back out again retraces exactly the path
    /// taken, and going somewhere new from the middle of a trail drops what
    /// was ahead, the way a browser does it.
    #[test]
    fn history_retraces_the_walk_and_a_new_move_forks_it() {
        let s = Scratch::new("files-history");
        s.file("a/deep/leaf.txt", "");
        s.dir("b");
        let mut tree = FileTree::new();

        // The first root is where the session starts, not somewhere it
        // came back from.
        tree.reroot(&s.0);
        assert!(!tree.can_go_back(), "nothing precedes the first folder");
        assert!(!tree.can_go_forward());

        tree.reroot(s.0.join("a"));
        tree.reroot(s.0.join("a/deep"));
        assert!(tree.can_go_back());

        assert!(tree.go_back());
        assert_eq!(tree.root(), s.0.join("a"));
        assert!(tree.can_go_forward(), "and forward is now open");
        assert!(tree.go_back());
        assert_eq!(tree.root(), s.0);
        assert!(!tree.can_go_back(), "back stops at the first folder");
        assert!(
            !tree.go_back(),
            "and says so rather than doing nothing quietly"
        );

        assert!(tree.go_forward());
        assert_eq!(tree.root(), s.0.join("a"));

        // A new move from here forks: `a/deep` was ahead and is not anymore.
        tree.reroot(s.0.join("b"));
        assert!(!tree.can_go_forward(), "a new branch drops what was ahead");
        assert!(tree.go_back());
        assert_eq!(
            tree.root(),
            s.0.join("a"),
            "back still retraces the new trail"
        );
    }

    /// Climbing out through `..` is a move like any other, so it is
    /// something back can undo. An operator who overshoots the folder they
    /// wanted needs one click to return, not a walk back down the tree.
    #[test]
    fn climbing_out_is_a_step_back_can_undo() {
        let s = Scratch::new("files-history-up");
        s.file("project/src/main.rs", "");
        let mut tree = FileTree::new();
        tree.reroot(s.0.join("project/src"));

        tree.reroot(tree.parent().expect("a parent above src"));
        assert_eq!(tree.root(), s.0.join("project"));
        assert!(tree.go_back(), "the climb is undoable");
        assert_eq!(tree.root(), s.0.join("project/src"));
    }

    /// Re-rooting where the tree already stands is not a move. Without this
    /// the pane-follow probe, which re-asserts the same folder on a timer,
    /// would fill the back stack with copies of the current folder and the
    /// back button would appear to do nothing.
    #[test]
    fn standing_still_is_not_history() {
        let s = Scratch::new("files-history-still");
        s.dir("here");
        let mut tree = FileTree::new();
        tree.reroot(s.0.join("here"));
        for _ in 0..5 {
            tree.reroot(s.0.join("here"));
        }
        assert!(!tree.can_go_back());
    }

    #[test]
    fn a_tree_with_no_root_reads_nothing() {
        let mut tree = FileTree::new();
        assert!(!tree.has_root());
        assert!(tree.rows().is_empty());
        assert!(!tree.refresh(), "nothing to poll until there is a root");
    }
}
