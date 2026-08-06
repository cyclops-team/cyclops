//! Workspace naming policy: pure functions, no tmux, no IO.
//!
//! A workspace (tmux session) is named after the folder it was created in,
//! sanitized for tmux target syntax and made unique against whatever else is
//! already open. This is the whole rule; the executor's `NewWorkspace` arm
//! and the folder-following probe (`app::follow_workspace_folder`) are both
//! callers, never re-implementers, of it.

use std::path::Path;

/// A folder basename as a tmux session name. tmux reserves `.` and `:` in
/// targets, so they become `-`; an unusable basename falls back to
/// "workspace".
pub fn session_name_from_folder(folder: &Path) -> String {
    let name: String = folder
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim()
        .chars()
        .map(|c| {
            if c == '.' || c == ':' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = name.trim_matches('-');
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `base`, or the first `base-N` (N from 2) not in `taken`.
pub fn unique_session_name(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("some suffix is always free")
}

/// The name a folder-following workspace should wear, or `None` when it
/// already wears it. `taken` is every OTHER workspace's name, so the suffix
/// rule that keeps a new workspace's name collision-free keeps this rename
/// collision-free too.
///
/// This is a pure function — no tmux, no session lookup — because it *is*
/// the whole follow-the-folder rule: every case is decided from `current`,
/// `cwd`, and `taken` alone. That's what lets a caller run it on every
/// render tick without a round trip, and what lets it be tested exhaustively
/// without a tmux server.
pub fn folder_rename(current: &str, cwd: &str, taken: &[String]) -> Option<String> {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return None;
    }
    let base = session_name_from_folder(Path::new(cwd));
    let next = unique_session_name(&base, taken);
    (next != current).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_sanitize_for_tmux() {
        assert_eq!(session_name_from_folder(Path::new("/a/cyclops")), "cyclops");
        // `.` and `:` are tmux target syntax; a dotfile folder must not
        // produce a name tmux reads as "window of the empty session".
        assert_eq!(
            session_name_from_folder(Path::new("/a/my.project")),
            "my-project"
        );
        assert_eq!(session_name_from_folder(Path::new("/a/.config")), "config");
        assert_eq!(
            session_name_from_folder(Path::new("/a/line\nbreak")),
            "line-break"
        );
        assert_eq!(session_name_from_folder(Path::new("/")), "workspace");
    }

    #[test]
    fn duplicate_workspace_names_get_a_suffix() {
        let taken = vec!["cyclops".to_string(), "cyclops-2".to_string()];
        assert_eq!(unique_session_name("cyclops", &taken), "cyclops-3");
        assert_eq!(unique_session_name("fresh", &taken), "fresh");
    }

    #[test]
    fn folder_rename_targets_the_new_folders_basename() {
        assert_eq!(
            folder_rename("old", "/a/cyclops", &[]),
            Some("cyclops".to_string())
        );
    }

    #[test]
    fn folder_rename_is_none_once_the_name_already_matches() {
        assert_eq!(folder_rename("cyclops", "/a/cyclops", &[]), None);
    }

    #[test]
    fn folder_rename_ignores_an_empty_or_blank_cwd() {
        assert_eq!(folder_rename("cyclops", "", &[]), None);
        assert_eq!(folder_rename("cyclops", "   ", &[]), None);
    }

    #[test]
    fn folder_rename_lands_on_a_suffix_and_then_holds_still() {
        let taken = vec!["cyclops".to_string()];
        let next = folder_rename("old", "/a/cyclops", &taken).expect("collision suffix");
        assert_eq!(next, "cyclops-2");
        // Probing again with the suffixed name as `current` must return
        // None — otherwise a folder-following workspace that collided once
        // would rename itself on every subsequent probe.
        assert_eq!(folder_rename(&next, "/a/cyclops", &taken), None);
    }

    #[test]
    fn folder_rename_sanitizes_like_session_name_from_folder() {
        assert_eq!(
            folder_rename("old", "/a/my.project", &[]),
            Some("my-project".to_string())
        );
    }
}
