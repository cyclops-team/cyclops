//! Structural operations: the commands a workspace UI issues on a gesture.
//!
//! Split this pane, close that tab, drag a window onto another workspace.
//! A caller names the operation and its targets in this crate's vocabulary;
//! the command spelling, the quoting, the exact-match `=session` target and
//! the reply parsing are this crate's, per the rule in the crate header.
//!
//! None of these update a caller's model. Every one changes the tmux
//! session, and tmux answers with the structural notification that drives
//! reconciliation ([`crate::watcher`]); a UI that also mutated its own model
//! here would be guessing at state tmux is about to report, and the two
//! guesses disagree the moment a second client is attached.

use std::path::Path;

use crate::cmd::session_target;
use crate::control::ControlClient;
use crate::error::TmuxError;
use crate::quote::quote_arg;

/// Which way a split cuts the source pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// `-h`: the new pane sits beside the source, sharing its rows.
    Horizontal,
    /// `-v`: the new pane sits below the source, sharing its columns.
    Vertical,
}

impl SplitDirection {
    fn flag(self) -> &'static str {
        match self {
            SplitDirection::Horizontal => "-h",
            SplitDirection::Vertical => "-v",
        }
    }
}

/// A direction on the pane grid: which neighbour to select, or which way to
/// push a pane's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

impl PaneDirection {
    fn flag(self) -> &'static str {
        match self {
            PaneDirection::Left => "-L",
            PaneDirection::Right => "-R",
            PaneDirection::Up => "-U",
            PaneDirection::Down => "-D",
        }
    }

    /// The neighbour-of-current target mnemonic, for commands that take a
    /// pane target rather than a directional flag.
    fn neighbour_target(self) -> &'static str {
        match self {
            PaneDirection::Left => "{left-of}",
            PaneDirection::Right => "{right-of}",
            PaneDirection::Up => "{up-of}",
            PaneDirection::Down => "{down-of}",
        }
    }
}

impl ControlClient {
    /// Focus the neighbouring pane in `direction`, starting from whichever
    /// pane is current. tmux picks the neighbour from the live layout, so a
    /// caller never has to work out which pane is to the left of which.
    pub async fn select_pane_toward(&self, direction: PaneDirection) -> Result<(), TmuxError> {
        self.command(&format!("select-pane {}", direction.flag()))
            .await
            .map(|_| ())
    }

    /// Focus one pane by id.
    ///
    /// A pane on a window that is not current stays invisible until that
    /// window is selected too — call [`ControlClient::select_window`] first
    /// in that case. The two stay separate commands so each control-mode
    /// reply is still correlated to the command that caused it.
    pub async fn select_pane(&self, pane_id: &str) -> Result<(), TmuxError> {
        self.command(&format!("select-pane -t {}", quote_arg(pane_id)))
            .await
            .map(|_| ())
    }

    /// Split `pane_id` and leave tmux focused on the new pane.
    ///
    /// Two commands underneath. `split-window` inherits the *session's*
    /// default directory, not the source pane's, so this reads the pane's
    /// `#{pane_current_path}` first and passes it as `-c`: a split from a
    /// pane the human has `cd`ed somewhere else must open where they are
    /// looking, not where the session started.
    ///
    /// No `-d`, deliberately: a split exists to be typed into, so the new
    /// pane becomes current in tmux immediately.
    pub async fn split_window(
        &self,
        pane_id: &str,
        direction: SplitDirection,
    ) -> Result<(), TmuxError> {
        let path = self.display(pane_id, "#{pane_current_path}").await?;
        let path = path.trim();
        let mut cmd = format!("split-window {}", direction.flag());
        if !path.is_empty() {
            cmd.push_str(&format!(" -c {}", quote_arg(path)));
        }
        cmd.push_str(&format!(" -t {}", quote_arg(pane_id)));
        self.command(&cmd).await.map(|_| ())
    }

    /// Close one pane.
    pub async fn kill_pane(&self, pane_id: &str) -> Result<(), TmuxError> {
        self.command(&format!("kill-pane -t {}", quote_arg(pane_id)))
            .await
            .map(|_| ())
    }

    /// Push `pane_id`'s edge `cells` cells in `direction`.
    ///
    /// Direction and magnitude are separate because tmux's flag is the
    /// direction: a caller holding a signed delta (a drag) decides the sign
    /// once, here it is already decided.
    pub async fn resize_pane(
        &self,
        pane_id: &str,
        direction: PaneDirection,
        cells: u32,
    ) -> Result<(), TmuxError> {
        self.command(&format!(
            "resize-pane -t {} {} {cells}",
            quote_arg(pane_id),
            direction.flag()
        ))
        .await
        .map(|_| ())
    }

    /// Set `pane_id` to exactly `rows` rows high (`resize-pane -y`).
    ///
    /// Absolute, unlike [`Self::resize_pane`], which pushes an edge by a
    /// delta. Minimize and restore both know the height they want, and
    /// expressing that as a delta would mean reading the current height
    /// first and racing every other client against the answer.
    ///
    /// tmux clamps to its own minimum and refuses a pane that has no room
    /// to give, so a pane spanning the window's full height simply does not
    /// move. Callers that must not offer a control which does nothing check
    /// that before painting it.
    pub async fn resize_pane_height(&self, pane_id: &str, rows: u16) -> Result<(), TmuxError> {
        self.command(&format!("resize-pane -t {} -y {rows}", quote_arg(pane_id)))
            .await
            .map(|_| ())
    }

    /// Toggle zoom on the window holding `pane_id` (`resize-pane -Z`).
    ///
    /// tmux owns the flag, so this is a toggle rather than a set: asking for
    /// a state would mean reading `#{window_zoomed_flag}` first and racing
    /// every other client against the answer.
    pub async fn toggle_pane_zoom(&self, pane_id: &str) -> Result<(), TmuxError> {
        self.command(&format!("resize-pane -Z -t {}", quote_arg(pane_id)))
            .await
            .map(|_| ())
    }

    /// Create a window in the attached session and return its window id.
    ///
    /// One command owns creation plus naming plus directory, so no caller
    /// ever sees the intermediate default name. The id comes back from
    /// `-P -F '#{window_id}'` because an index is not a handle: indexes
    /// shift as windows come and go, `@n` does not.
    ///
    /// An empty `name` is treated as no name, an empty `cwd` as no
    /// directory; passing either through to tmux would create a nameless
    /// window or fail on a directory that is not there.
    pub async fn new_window(
        &self,
        name: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<String, TmuxError> {
        let mut cmd = format!("new-window -P -F {}", quote_arg("#{window_id}"));
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            cmd.push_str(&format!(" -n {}", quote_arg(name)));
        }
        if let Some(cwd) = cwd.filter(|d| !d.as_os_str().is_empty()) {
            cmd.push_str(&format!(" -c {}", quote_arg(&cwd.to_string_lossy())));
        }
        let out = self.command(&cmd).await?;
        first_field(&out, "new-window")
    }

    /// Rename one window. The id carries the target, so this never depends
    /// on which window happens to be current.
    pub async fn rename_window(&self, window_id: &str, name: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "rename-window -t {} {}",
            quote_arg(window_id),
            quote_arg(name)
        ))
        .await
        .map(|_| ())
    }

    /// Rename one session.
    ///
    /// The target is `=name`: without the `=`, tmux accepts a prefix match,
    /// so a rename aimed at a session that has just closed silently lands on
    /// a neighbour whose name merely starts the same way.
    pub async fn rename_session(&self, session: &str, name: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "rename-session -t {} {}",
            quote_arg(&session_target(session)),
            quote_arg(name)
        ))
        .await
        .map(|_| ())
    }

    /// Exchange the positions of two windows, which is how a UI reorders
    /// tabs: tmux has no "insert at index" for a window that already exists.
    pub async fn swap_window(
        &self,
        src_window_id: &str,
        dst_window_id: &str,
    ) -> Result<(), TmuxError> {
        self.command(&format!(
            "swap-window -s {} -t {}",
            quote_arg(src_window_id),
            quote_arg(dst_window_id)
        ))
        .await
        .map(|_| ())
    }

    /// Swap two panes by id. tmux focuses `-t` after a swap, so
    /// `dst_pane_id` ends up focused in its new slot.
    pub async fn swap_pane(&self, src_pane_id: &str, dst_pane_id: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "swap-pane -s {} -t {}",
            quote_arg(src_pane_id),
            quote_arg(dst_pane_id)
        ))
        .await
        .map(|_| ())
    }

    /// Swap the current pane with its neighbour in `direction`, which tmux
    /// resolves from live layout via its `{left-of}` target family. The
    /// implied `-t` is the current pane, and tmux focuses `-t` after a
    /// swap, so focus follows the pane being moved.
    pub async fn swap_pane_toward(&self, direction: PaneDirection) -> Result<(), TmuxError> {
        self.command(&format!(
            "swap-pane -s {}",
            quote_arg(direction.neighbour_target())
        ))
        .await
        .map(|_| ())
    }

    /// Move one window to another session.
    ///
    /// The target is `=session:` — the trailing colon names the session's
    /// window list rather than a window in it, so tmux appends at the next
    /// free index instead of colliding with whatever holds that index. The
    /// `=` is the same exact-match rule as [`ControlClient::rename_session`].
    pub async fn move_window_to_session(
        &self,
        window_id: &str,
        session: &str,
    ) -> Result<(), TmuxError> {
        self.command(&format!(
            "move-window -s {} -t {}",
            quote_arg(window_id),
            quote_arg(&format!("{}:", session_target(session)))
        ))
        .await
        .map(|_| ())
    }

    /// Make one window current.
    pub async fn select_window(&self, window_id: &str) -> Result<(), TmuxError> {
        self.command(&format!("select-window -t {}", quote_arg(window_id)))
            .await
            .map(|_| ())
    }

    /// Close one window and every pane in it.
    pub async fn kill_window(&self, window_id: &str) -> Result<(), TmuxError> {
        self.command(&format!("kill-window -t {}", quote_arg(window_id)))
            .await
            .map(|_| ())
    }

    /// Switch this control client to another session. The `=` exact-match
    /// rule is the same as [`ControlClient::rename_session`].
    pub async fn switch_to_session(&self, session: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "switch-client -t {}",
            quote_arg(&session_target(session))
        ))
        .await
        .map(|_| ())
    }

    /// Create a detached session rooted in `cwd` and return its session id.
    ///
    /// The id comes back from `-P -F '#{session_id}'` because the name is
    /// not a stable handle: a folder-following workspace gets renamed later
    /// and the `$n` id is what survives that. The caller owns name policy
    /// (sanitization, uniqueness); this owns the command. The first window
    /// is named "1" so tab naming starts numeric, matching the workspace's
    /// automatic tab names.
    pub async fn new_session(&self, name: &str, cwd: &Path) -> Result<String, TmuxError> {
        let out = self
            .command(&format!(
                "new-session -d -P -F {} -s {} -n {} -c {}",
                quote_arg("#{session_id}"),
                quote_arg(name),
                quote_arg("1"),
                quote_arg(&cwd.to_string_lossy())
            ))
            .await?;
        first_field(&out, "new-session")
    }

    /// Close one session and every window in it. Exact-match `=` target, so
    /// a kill aimed at a vanished session fails instead of landing on a
    /// prefix neighbour.
    pub async fn kill_session(&self, session: &str) -> Result<(), TmuxError> {
        self.command(&format!(
            "kill-session -t {}",
            quote_arg(&session_target(session))
        ))
        .await
        .map(|_| ())
    }
}

/// The single value a `-P -F` reply carries.
///
/// An empty reply means tmux did the work but told us nothing, and the id is
/// the caller's only handle on what was created. Failing here beats handing
/// back an empty string that fails later as a target nobody can trace.
fn first_field(reply: &[String], what: &str) -> Result<String, TmuxError> {
    reply
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .ok_or_else(|| TmuxError::Protocol(format!("{what}: reply carried no id")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_direction_flags() {
        assert_eq!(SplitDirection::Horizontal.flag(), "-h");
        assert_eq!(SplitDirection::Vertical.flag(), "-v");
        assert_eq!(PaneDirection::Left.flag(), "-L");
        assert_eq!(PaneDirection::Right.flag(), "-R");
        assert_eq!(PaneDirection::Up.flag(), "-U");
        assert_eq!(PaneDirection::Down.flag(), "-D");
    }

    #[test]
    fn id_reply_skips_blank_lines() {
        assert_eq!(
            first_field(&["".into(), " @7 ".into()], "new-window").unwrap(),
            "@7"
        );
    }

    #[test]
    fn empty_id_reply_is_a_protocol_error() {
        match first_field(&[], "new-window") {
            Err(TmuxError::Protocol(msg)) => assert!(msg.contains("new-window")),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }
}
