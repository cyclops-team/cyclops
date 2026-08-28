//! One-shot tmux commands: spawn `tmux`, run one command, take its stdout.
//!
//! Control mode ([`crate::control`]) is the daemon's long-lived connection
//! per watched session. A one-shot is the right shape for a user gesture
//! that may target a server nobody is attached to: focusing a pane
//! ([`crate::focus`]), building a workspace ([`crate::layout`]). Both go
//! through here so the invocation rules are written once.
//!
//! `-u` always (F14): with no UTF-8 locale in the environment, tmux
//! rewrites tabs and non-ASCII in its reply text to `_`, which destroys
//! every tab-separated format this crate parses.

use std::path::Path;
use std::process::Command;

use tokio::process::Command as TokioCommand;

use crate::error::TmuxError;

/// Run one tmux command and return its stdout, trailing newline trimmed.
///
/// `socket` is a `tmux -L` socket name (tests always pass their isolated
/// server); None uses the server the caller's environment points at.
/// `config_file` mirrors `-f`.
pub(crate) fn run(
    socket: Option<&str>,
    config_file: Option<&Path>,
    args: &[&str],
) -> Result<String, TmuxError> {
    let mut cmd = Command::new("tmux");
    cmd.arg("-u");
    if let Some(sock) = socket {
        // An explicit socket targets exactly that server; the inherited
        // $TMUX must not redirect it. With no explicit socket the
        // inherited $TMUX is right: a user running this from inside tmux
        // means the server they are looking at.
        cmd.args(["-L", sock]).env_remove("TMUX");
    }
    if let Some(cfg) = config_file {
        cmd.arg("-f").arg(cfg);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|e| TmuxError::Spawn(e.to_string()))?;
    if out.status.success() {
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        Ok(text)
    } else {
        Err(TmuxError::Command(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// Async one-shot tmux command. Dropping the future kills the child, which
/// lets a caller put one bound around a larger operation that includes this
/// authoritative probe and any work selected by its result.
pub(crate) async fn run_async(
    socket: Option<&str>,
    config_file: Option<&Path>,
    args: &[&str],
) -> Result<String, TmuxError> {
    let mut cmd = TokioCommand::new("tmux");
    cmd.arg("-u");
    if let Some(sock) = socket {
        cmd.args(["-L", sock]).env_remove("TMUX");
    }
    if let Some(cfg) = config_file {
        cmd.arg("-f").arg(cfg);
    }
    cmd.args(args).kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .map_err(|error| TmuxError::Spawn(error.to_string()))?;
    if out.status.success() {
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        Ok(text)
    } else {
        Err(TmuxError::Command(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// Exact-match session target (`=name`).
///
/// Without the `=`, tmux accepts a prefix match, so a command aimed at
/// session "main" lands on "mainly" when "main" is gone. Every command
/// here that names a session names it this way.
pub fn session_target(session: &str) -> String {
    format!("={session}")
}
