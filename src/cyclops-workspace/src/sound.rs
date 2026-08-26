//! Sound notifications: one cue when an agent the operator is not looking
//! at gives them a reason to look.
//!
//! The thesis: a sound is worth its interruption only when attention
//! should move. An agent that finished its turn (working to idle), one
//! that needs a human (attention raised, or blocked on a prompt), and
//! one that died all qualify. An agent starting to work does not: the
//! operator just told it to.
//!
//! The switch is `WorkspacePrefs::sound_notifs` and the cue is
//! `WorkspacePrefs::sound`, both set in the settings dialog's Sound
//! section. The cue names a file under `<home>/sounds/` by stem (the
//! shipped `bow-ripple` and `glass-ping`, or anything the operator drops in) or the
//! system's own alert sound ([`SYSTEM`]). Every path ends in a noise: a
//! named file that is not there, or no player for it, falls through to
//! the system alert, and that falls through to the terminal bell, so the
//! switch always does something. Deciding *whether* a snapshot earns a
//! cue is pure ([`background_state_changed`]); making the noise is the
//! one side effect here ([`play`]).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cyclops_proto::AgentState;

use crate::decoration::{DecorationSnapshot, PrimaryStatus};

/// The stem of the default cue, `bow-ripple.wav`, which `cyclops start`
/// seeds into `<home>/sounds/` (src/cyclops/src/soundseed.rs), and so
/// the default `[workspace] sound`. Sounds are named by stem rather than
/// file name so a re-encoded copy still plays.
pub const DEFAULT: &str = "bow-ripple";

/// The `[workspace] sound` value that means the system's alert sound
/// rather than a file: whatever System Settings plays for an alert on
/// macOS, the desktop's sound theme on Linux. Not the terminal bell,
/// which is a setting many terminals ship turned off. Reserved: a file
/// with this stem is not listed.
pub const SYSTEM: &str = "system";

/// What a stopped agent is stopped as. Coarser than [`PrimaryStatus`] on
/// purpose: idle and idle-with-input paint the same glyph and word, and a
/// composer being cleared while idle is not news.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    Idle,
    Attention,
    Dead,
}

/// How a status reads as a stop, or `None` while it reads as a turn in
/// progress.
///
/// A blocked state counts as attention whether or not the daemon's
/// register has flagged it. The chrome's ⚠ stays the register's alone
/// (`decoration.rs` says why), but a permission prompt is a reason to
/// switch attention the moment it appears, and a cue that waited for the
/// register to catch up would ring after the operator already noticed.
fn stopped(status: PrimaryStatus) -> Option<Stopped> {
    if status.attention || status.color_state.is_blocked() {
        return Some(Stopped::Attention);
    }
    match status.color_state {
        AgentState::Idle | AgentState::IdleWithInput => Some(Stopped::Idle),
        AgentState::Dead => Some(Stopped::Dead),
        _ => None,
    }
}

/// Whether the change from `before` to `after` earns a cue.
///
/// Decided per snapshot, not per pane: a split or a reconcile moves
/// several panes through cyclopsd at once, and one cue for the batch is
/// what an operator can act on. A pane earns it when, in order:
///
/// 1. Both snapshots know it. A pane that just appeared has no state to
///    have changed from, and a pane that vanished is a close, which the
///    operator did.
/// 2. Its state was known before. Unknown resolves to something on every
///    boot, and a ring per pane while the sensors settle would teach the
///    operator to turn the switch off.
/// 3. It stopped, and differently from how it was stopped before: idle
///    after working (done), needing attention or blocked after either,
///    dead after anything. Starting to work is the operator's doing (they
///    just sent something) and is not news.
/// 4. It is in the background: not the focused pane, or any pane while
///    the terminal's focus is elsewhere. The focused pane's border is
///    under the operator's eyes; a sound for it is noise.
pub fn background_state_changed(
    before: &DecorationSnapshot,
    after: &DecorationSnapshot,
    focused_pane: &str,
    window_focused: bool,
) -> bool {
    after.panes.values().any(|now| {
        let Some(was) = before.pane(&now.pane_id) else {
            return false;
        };
        let Some(was) = DecorationSnapshot::primary_status(was) else {
            return false;
        };
        let now_stopped = DecorationSnapshot::primary_status(now).and_then(stopped);
        if now_stopped.is_none() || now_stopped == stopped(was) {
            return false;
        }
        !window_focused || now.pane_id != focused_pane
    })
}

/// Make the noise `name` asks for: the system alert, or the installed
/// file of that stem through the platform's player, falling through to
/// the system alert when the file is gone or there is no player for it.
pub fn play(home: &Path, name: &str) {
    let played = name != SYSTEM
        && installed_sound(home, name)
            .and_then(|path| player_command(&path))
            .is_some_and(spawn_detached);
    if !played {
        play_system_alert();
    }
}

/// The system's alert sound, and the terminal bell when the system has
/// no way to play one. On macOS `osascript -e beep` is NSBeep: the alert
/// the operator chose in System Settings, at the alert volume, from any
/// process. On Linux the freedesktop sound theme's bell through the
/// player; a desktop without one falls to BEL, which is the terminal's
/// call.
fn play_system_alert() {
    let played = system_alert_command().is_some_and(spawn_detached);
    if !played {
        ring_bell();
    }
}

fn system_alert_command() -> Option<Command> {
    if cfg!(target_os = "macos") {
        let mut command = Command::new(find_on_path("osascript")?);
        command.args(["-e", "beep"]);
        return Some(command);
    }
    let theme = [
        "/usr/share/sounds/freedesktop/stereo/bell.oga",
        "/usr/share/sounds/freedesktop/stereo/complete.oga",
    ]
    .into_iter()
    .map(Path::new)
    .find(|path| path.is_file())?;
    player_command(theme)
}

/// What the settings card offers: every installed sound's stem, sorted
/// and deduplicated, then the system alert, so the list is never empty
/// and the shipped cue sorts first on a fresh install. A read failure
/// lists the system alert alone: the switch still has a sound to make.
pub fn choices(home: &Path) -> Vec<String> {
    let mut stems: Vec<String> = std::fs::read_dir(home.join("sounds"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_string))
        .filter(|stem| stem != SYSTEM)
        .collect();
    stems.sort();
    stems.dedup();
    stems.push(SYSTEM.to_string());
    stems
}

/// The installed file with stem `name`, when there is one. Sorted so two
/// files that differ only by extension resolve the same way every time.
pub fn installed_sound(home: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(home.join("sounds")).ok()?;
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.file_stem().is_some_and(|stem| stem == name))
        .collect();
    files.sort();
    files.into_iter().next()
}

/// Start the noise and get out of its way: whether the child started.
/// It runs on its own; a thread waits on it so the process table is not
/// left holding a zombie per cue, and nothing here blocks the input
/// loop.
fn spawn_detached(mut command: Command) -> bool {
    let spawned = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// The platform's stock command-line player, ready to play `path`. macOS
/// ships `afplay`; on Linux the first of PulseAudio's, ALSA's and
/// ffmpeg's players on PATH. None when none is: the caller falls
/// through instead of reporting an error nobody can act on mid-session.
fn player_command(path: &Path) -> Option<Command> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["afplay"]
    } else {
        &["paplay", "aplay", "ffplay"]
    };
    for name in candidates {
        let Some(binary) = find_on_path(name) else {
            continue;
        };
        let mut command = Command::new(binary);
        if *name == "ffplay" {
            command.args(["-nodisp", "-autoexit", "-loglevel", "quiet"]);
        }
        command.arg(path);
        return Some(command);
    }
    None
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// BEL to the terminal the workspace draws on, the last resort: the
/// terminal decides what a bell is (a sound, a flash, nothing), and
/// many ship with the sound off, which is why it is not the default.
fn ring_bell() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoration::PaneDecoration;

    fn pane(id: &str, state: AgentState, needs_attention: bool) -> PaneDecoration {
        PaneDecoration {
            pane_id: id.into(),
            window_id: "@0".into(),
            label: None,
            manifest: None,
            manifest_display_name: None,
            state,
            needs_attention,
        }
    }

    fn snapshot(panes: &[PaneDecoration]) -> DecorationSnapshot {
        DecorationSnapshot {
            panes: panes
                .iter()
                .map(|pane| (pane.pane_id.clone(), pane.clone()))
                .collect(),
            online: true,
            ..DecorationSnapshot::default()
        }
    }

    /// The reason the switch exists: a pane the operator is not watching
    /// finishes its turn.
    #[test]
    fn a_background_pane_going_idle_earns_a_cue() {
        let before = snapshot(&[pane("%1", AgentState::Working, false)]);
        let after = snapshot(&[pane("%1", AgentState::Idle, false)]);
        assert!(background_state_changed(&before, &after, "%0", true));
    }

    #[test]
    fn attention_blocks_and_death_earn_a_cue_starting_work_does_not() {
        let idle = snapshot(&[pane("%1", AgentState::Idle, false)]);
        let blocked = snapshot(&[pane("%1", AgentState::BlockedPermission, true)]);
        let unflagged = snapshot(&[pane("%1", AgentState::BlockedModal, false)]);
        let dead = snapshot(&[pane("%1", AgentState::Dead, false)]);
        let working = snapshot(&[pane("%1", AgentState::Working, false)]);
        assert!(background_state_changed(&idle, &blocked, "%0", true));
        assert!(
            background_state_changed(&working, &unflagged, "%0", true),
            "a prompt is a reason to look before the register says so"
        );
        assert!(
            !background_state_changed(&blocked, &unflagged, "%0", true),
            "flagged or not, a block is one kind of stop"
        );
        assert!(background_state_changed(&working, &dead, "%0", true));
        assert!(
            !background_state_changed(&idle, &working, "%0", true),
            "going to work is the operator's doing"
        );
    }

    /// The focused pane is under the operator's eyes, unless the terminal
    /// itself is not.
    #[test]
    fn the_focused_pane_is_silent_while_the_window_is_focused() {
        let before = snapshot(&[pane("%0", AgentState::Working, false)]);
        let after = snapshot(&[pane("%0", AgentState::Idle, false)]);
        assert!(!background_state_changed(&before, &after, "%0", true));
        assert!(
            background_state_changed(&before, &after, "%0", false),
            "an unfocused terminal makes every pane a background pane"
        );
    }

    /// Nothing rings on boot, on a new pane, or on a change the chrome
    /// would not show either.
    #[test]
    fn unknown_new_and_same_vocabulary_changes_are_silent() {
        let unknown = snapshot(&[pane("%1", AgentState::Unknown, false)]);
        let idle = snapshot(&[pane("%1", AgentState::Idle, false)]);
        let idle_input = snapshot(&[pane("%1", AgentState::IdleWithInput, false)]);
        assert!(
            !background_state_changed(&unknown, &idle, "%0", true),
            "sensors settling on boot"
        );
        assert!(
            !background_state_changed(&snapshot(&[]), &idle, "%0", true),
            "a pane with no before"
        );
        assert!(
            !background_state_changed(&idle, &idle_input, "%0", true),
            "idle either way"
        );
    }

    /// A sound is its stem: the extension is the encoding, not the name.
    #[test]
    fn an_installed_sound_is_found_by_stem_whatever_its_extension() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-sound-installed");
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(installed_sound(&home, DEFAULT), None, "no sounds directory");
        std::fs::create_dir_all(home.join("sounds")).expect("sounds dir");
        assert_eq!(installed_sound(&home, DEFAULT), None, "an empty directory");
        std::fs::write(home.join("sounds/other.wav"), b"").expect("write");
        assert_eq!(
            installed_sound(&home, DEFAULT),
            None,
            "the asked-for stem only"
        );
        std::fs::write(home.join("sounds/bow-ripple.aiff"), b"").expect("write");
        assert_eq!(
            installed_sound(&home, DEFAULT),
            Some(home.join("sounds/bow-ripple.aiff"))
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The list is the folder's stems in order and the system alert
    /// last: one row per sound however many encodings it has, the
    /// alert's name kept for the alert, and the alert alone when there
    /// is no folder.
    #[test]
    fn the_choices_are_the_installed_stems_then_the_system_alert() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-sound-choices");
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            choices(&home),
            vec![SYSTEM.to_string()],
            "no folder: the system alert"
        );
        std::fs::create_dir_all(home.join("sounds/not-a-file.wav")).expect("dir");
        for name in [
            "chime.wav",
            "bow-ripple.wav",
            "bow-ripple.aiff",
            "system.wav",
        ] {
            std::fs::write(home.join("sounds").join(name), b"").expect("write");
        }
        assert_eq!(
            choices(&home),
            vec![DEFAULT.to_string(), "chime".to_string(), SYSTEM.to_string()]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The built-in cue has a command on this platform, so the switch
    /// makes a noise on a machine with no sounds folder at all.
    #[test]
    fn the_system_alert_has_a_command_here() {
        let command = system_alert_command().expect("a way to play the system alert");
        let program = command.get_program().to_string_lossy().into_owned();
        if cfg!(target_os = "macos") {
            assert!(program.ends_with("osascript"), "{program}");
        }
    }
}
