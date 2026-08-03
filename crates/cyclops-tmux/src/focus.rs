//! One-shot focus jump: put a pane in front of the human.
//!
//! Not control mode. A focus jump is a user gesture from the stream UI,
//! not daemon state, and it may target a server no control client is
//! attached to. A short-lived `tmux -u` invocation is the whole cost;
//! routing it through this crate keeps the adapter-only rule intact
//! (no tmux calls outside cyclops-tmux). `-u` always: F14.

use std::path::Path;
use std::process::Command;

use crate::error::TmuxError;

/// Select the window holding `pane_id`, then the pane itself. The window
/// step comes first: `select-pane` alone leaves a pane in an unselected
/// window invisible.
///
/// `socket` is a `tmux -L` socket name (tests use their isolated servers);
/// None uses the caller's default server. `config_file` mirrors `-f`.
pub fn focus_pane(
    socket: Option<&str>,
    config_file: Option<&Path>,
    pane_id: &str,
) -> Result<(), TmuxError> {
    // Only real pane ids ("%" + digits) go on the command line. Labels
    // must be resolved by the caller; anything else is a protocol misuse
    // worth a loud error, not a tmux guess.
    if !pane_id.starts_with('%')
        || pane_id.len() < 2
        || !pane_id[1..].bytes().all(|b| b.is_ascii_digit())
    {
        return Err(TmuxError::Protocol(format!(
            "focus_pane needs a pane id like %3, got {pane_id:?}"
        )));
    }
    run(socket, config_file, &["select-window", "-t", pane_id])?;
    run(socket, config_file, &["select-pane", "-t", pane_id])
}

fn run(socket: Option<&str>, config_file: Option<&Path>, args: &[&str]) -> Result<(), TmuxError> {
    let mut cmd = Command::new("tmux");
    cmd.arg("-u");
    if let Some(sock) = socket {
        // An explicit socket targets exactly that server; the inherited
        // $TMUX must not redirect it.
        cmd.args(["-L", sock]).env_remove("TMUX");
    }
    if let Some(cfg) = config_file {
        cmd.arg("-f").arg(cfg);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|e| TmuxError::Spawn(e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(TmuxError::Command(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_anything_that_is_not_a_pane_id() {
        for bad in ["reviewer", "", "%", "%3a", "3", "%-1", "% 3"] {
            match focus_pane(Some("cyclops-no-such-server"), None, bad) {
                Err(TmuxError::Protocol(_)) => {}
                other => panic!("{bad:?} should be a protocol error, got {other:?}"),
            }
        }
    }
}
