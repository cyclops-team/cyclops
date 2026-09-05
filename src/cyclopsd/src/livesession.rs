//! Read the live session key [`crate::sessionid`] mints durable names
//! from: workspace, OS boot, tmux server process, tmux session id.
//!
//! Every field is read from a system that does not hold still, so the
//! reading is bracketed and a reading that moved is refused rather than
//! assembled from two. No persistence and no daemon state here.

use cyclops_proto::{
    IdentityError, LiveSessionKey, OsBootId, ProcessInstanceId, TmuxSessionId, WorkspaceId,
};

/// Why an observation could not be trusted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ObserveError {
    #[error("the OS boot token could not be read")]
    NoBootToken,
    #[error("tmux did not answer")]
    NoTmux,
    #[error("tmux answered {0:?}, which is not a server pid and session id")]
    BadTmuxReply(String),
    #[error("the tmux server process could not be identified")]
    NoServerProcess,
    #[error("the tmux server changed while it was being read")]
    ServerChanged,
    #[error("the machine rebooted while the session was being read")]
    BootChanged,
    #[error(transparent)]
    Invalid(#[from] IdentityError),
}

/// The readings an observation needs. Injected so the race cases can be
/// driven exactly; production reads through [`observe_watched`].
#[cfg(test)]
pub(crate) trait LiveSource {
    /// A token that changes when the machine reboots.
    fn boot_token(&self) -> Option<String>;
    /// One reply naming the tmux server pid and the session id. One
    /// command, because asked separately the pid can belong to a server
    /// that no longer owns the session.
    fn tmux_facts(&self) -> Option<String>;
    /// Kernel start time, in the platform units `ProcessInstanceId`
    /// documents.
    fn birth_of(&self, pid: i32) -> Option<u64>;
}

/// What the readers answered, decided on by [`assemble`]. Separate so
/// the injected and watched paths gather differently and decide
/// identically.
struct Reading {
    boot_before: Option<String>,
    tmux_first: Option<String>,
    birth: Option<u64>,
    tmux_second: Option<String>,
    boot_after: Option<String>,
}

/// One reading to one key, or a refusal.
fn assemble(
    reading: Reading,
    workspace: WorkspaceId,
    expected_session: TmuxSessionId,
) -> Result<LiveSessionKey, ObserveError> {
    let boot_before = reading.boot_before.ok_or(ObserveError::NoBootToken)?;
    let (server_pid, session_id) =
        parse_tmux_facts(&reading.tmux_first.ok_or(ObserveError::NoTmux)?)?;
    if session_id != expected_session {
        return Err(ObserveError::ServerChanged);
    }
    let birth = reading.birth.ok_or(ObserveError::NoServerProcess)?;
    // Re-read across the birth lookup: without it the birth can belong
    // to whatever inherited the pid after the server exited.
    let (again, session_again) =
        parse_tmux_facts(&reading.tmux_second.ok_or(ObserveError::NoTmux)?)?;
    if again != server_pid || session_again != expected_session {
        return Err(ObserveError::ServerChanged);
    }
    let boot_after = reading.boot_after.ok_or(ObserveError::NoBootToken)?;
    // Read on both sides: a reboot between the last tmux reply and a
    // single trailing read would file a dead server's pid under the new
    // boot.
    let boot = normalize_boot_token(&boot_before);
    if boot != normalize_boot_token(&boot_after) {
        return Err(ObserveError::BootChanged);
    }
    Ok(LiveSessionKey::new(
        workspace,
        OsBootId::new(boot)?,
        ProcessInstanceId::new(server_pid, birth)?,
        session_id,
    ))
}

/// Read one live session through an injected source.
#[cfg(test)]
pub(crate) fn observe(
    source: &impl LiveSource,
    workspace: WorkspaceId,
    expected_session: TmuxSessionId,
) -> Result<LiveSessionKey, ObserveError> {
    let boot_before = source.boot_token();
    let tmux_first = source.tmux_facts();
    let birth = tmux_first
        .as_deref()
        .and_then(|reply| parse_tmux_facts(reply).ok())
        .and_then(|(pid, _)| source.birth_of(pid));
    let tmux_second = source.tmux_facts();
    let boot_after = source.boot_token();
    assemble(
        Reading {
            boot_before,
            tmux_first,
            birth,
            tmux_second,
            boot_after,
        },
        workspace,
        expected_session,
    )
}

/// `"<pid> $<n>"`, as `display-message -p '#{pid} #{session_id}'` prints
/// it. Strict: guessing at a partial reply is how a session id becomes a
/// pid.
fn parse_tmux_facts(reply: &str) -> Result<(i32, TmuxSessionId), ObserveError> {
    let bad = || ObserveError::BadTmuxReply(reply.to_string());
    let mut parts = reply.split_whitespace();
    let (Some(pid), Some(session), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    let pid: i32 = pid.parse().map_err(|_| bad())?;
    if pid <= 0 {
        return Err(bad());
    }
    Ok((pid, session.parse().map_err(|_| bad())?))
}

/// Lowercased: macOS prints its boot UUID uppercase and Linux lowercase,
/// and a token that changed case between two readings would read as a
/// different boot.
fn normalize_boot_token(token: &str) -> String {
    token.trim().to_ascii_lowercase()
}

/// The current OS boot, as the durable identity a headless registration is
/// filed under. A process birth is comparable only within one boot, so a
/// registration from another boot names nothing and is dropped at boot
/// reverification.
pub(crate) fn current_os_boot_id() -> Option<OsBootId> {
    OsBootId::new(normalize_boot_token(&boot_token()?)).ok()
}

/// The OS boot token. MEASURED on macOS 26.5: `kern.bootsessionuuid` is
/// a per-boot UUID, and not `kern.boottime`, which NTP adjusts while the
/// machine runs.
fn boot_token() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|token| !token.is_empty())
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Read one watched session over its existing control connection.
///
/// The stable session id is the target. A pane can move to another session
/// while its old watcher row is still cached, so a pane target can name the
/// wrong session. The existing connection keeps the read on the configured
/// tmux socket.
///
/// `birth` is the caller's, so the daemon keeps one reader for process
/// start times.
pub(crate) async fn observe_watched(
    client: &cyclops_tmux::ControlClient,
    expected_session: TmuxSessionId,
    workspace: WorkspaceId,
    birth: impl Fn(i32) -> Option<u64>,
) -> Result<LiveSessionKey, ObserveError> {
    // A trailing colon gives display-message a pane context inside this
    // exact session without borrowing a pane id from the watcher cache.
    let target = format!("{expected_session}:");
    let facts = || async {
        client
            .display(&target, "#{pid} #{session_id}")
            .await
            .ok()
            .map(|reply| reply.trim().to_string())
    };
    let boot_before = boot_token();
    let tmux_first = facts().await;
    let birth = tmux_first
        .as_deref()
        .and_then(|reply| parse_tmux_facts(reply).ok())
        .and_then(|(pid, _)| birth(pid));
    let tmux_second = facts().await;
    let boot_after = boot_token();
    assemble(
        Reading {
            boot_before,
            tmux_first,
            birth,
            tmux_second,
            boot_after,
        },
        workspace,
        expected_session,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn workspace() -> WorkspaceId {
        "11111111-1111-4111-8111-111111111111"
            .parse()
            .expect("workspace id")
    }

    fn session() -> TmuxSessionId {
        "$1".parse().expect("session id")
    }

    /// Answers from a script, so a reading that changes between calls is
    /// arranged rather than raced for.
    struct Scripted {
        boots: RefCell<Vec<Option<String>>>,
        tmux: RefCell<Vec<Option<String>>>,
        birth: Option<u64>,
    }

    impl Scripted {
        fn steady(reply: &str) -> Scripted {
            Scripted {
                boots: RefCell::new(vec![Some("25899552-CA6F-424A-B5EE-A4C80827B373".into()); 2]),
                tmux: RefCell::new(vec![Some(reply.into()); 2]),
                birth: Some(4242),
            }
        }
    }

    impl LiveSource for Scripted {
        fn boot_token(&self) -> Option<String> {
            let mut queued = self.boots.borrow_mut();
            if queued.is_empty() {
                return None;
            }
            queued.remove(0)
        }
        fn tmux_facts(&self) -> Option<String> {
            let mut queued = self.tmux.borrow_mut();
            if queued.is_empty() {
                return None;
            }
            queued.remove(0)
        }
        fn birth_of(&self, _pid: i32) -> Option<u64> {
            self.birth
        }
    }

    #[test]
    fn one_steady_reading_names_one_live_session() {
        let key = observe(&Scripted::steady("65411 $1"), workspace(), session()).expect("observed");
        assert_eq!(key.workspace_id(), workspace());
        assert_eq!(key.tmux_server().pid(), 65411);
        assert_eq!(key.tmux_server().birth(), 4242);
        assert_eq!(key.tmux_session_id().to_string(), "$1");
        assert_eq!(
            key.os_boot_id().as_str(),
            "25899552-ca6f-424a-b5ee-a4c80827b373"
        );
    }

    #[test]
    fn only_a_server_pid_and_a_session_id_are_accepted() {
        for reply in [
            "",
            "65411",
            "$1",
            "65411 $1 extra",
            "65411 1",
            "65411 $",
            "notapid $1",
            "0 $1",
            "-1 $1",
            "65411 %1",
        ] {
            assert!(
                matches!(
                    observe(&Scripted::steady(reply), workspace(), session()),
                    Err(ObserveError::BadTmuxReply(_))
                ),
                "accepted {reply:?}"
            );
        }
        // Surrounding whitespace is the terminal's, not content.
        assert!(observe(&Scripted::steady("  65411 $1\n"), workspace(), session()).is_ok());
    }

    /// Otherwise the key can pair one server or session with another.
    #[test]
    fn a_different_server_or_session_is_refused() {
        for (case, replies) in [
            ("the server restarted", vec!["65411 $1", "70000 $1"]),
            ("the session was recreated", vec!["65411 $1", "65411 $2"]),
            (
                "the first reply named another session",
                vec!["65411 $2", "65411 $1"],
            ),
            (
                "both replies named another session",
                vec!["65411 $2", "65411 $2"],
            ),
            ("both changed", vec!["65411 $1", "70000 $2"]),
        ] {
            let source = Scripted {
                tmux: RefCell::new(replies.into_iter().map(|r| Some(r.into())).collect()),
                ..Scripted::steady("unused")
            };
            assert_eq!(
                observe(&source, workspace(), session()),
                Err(ObserveError::ServerChanged),
                "{case}"
            );
        }
    }

    /// A single trailing boot read would miss a reboot landing between
    /// the last tmux reply and it.
    #[test]
    fn a_reboot_under_the_reading_is_refused() {
        let rebooted = Scripted {
            boots: RefCell::new(vec![Some("boot-a".into()), Some("boot-b".into())]),
            ..Scripted::steady("65411 $1")
        };
        assert_eq!(
            observe(&rebooted, workspace(), session()),
            Err(ObserveError::BootChanged)
        );

        let steady = Scripted {
            boots: RefCell::new(vec![Some("boot-a".into()); 2]),
            ..Scripted::steady("65411 $1")
        };
        assert!(observe(&steady, workspace(), session()).is_ok());

        // Normalized on both sides.
        let cased = Scripted {
            boots: RefCell::new(vec![Some("BOOT-A".into()), Some("boot-a".into())]),
            ..Scripted::steady("65411 $1")
        };
        assert!(observe(&cased, workspace(), session()).is_ok());
    }

    /// Each reading fails on its own terms rather than defaulting.
    #[test]
    fn a_reading_that_did_not_answer_refuses() {
        let gone = Scripted {
            birth: None,
            ..Scripted::steady("65411 $1")
        };
        assert_eq!(
            observe(&gone, workspace(), session()),
            Err(ObserveError::NoServerProcess)
        );

        let no_tmux = Scripted {
            tmux: RefCell::new(vec![None]),
            ..Scripted::steady("unused")
        };
        assert_eq!(
            observe(&no_tmux, workspace(), session()),
            Err(ObserveError::NoTmux)
        );

        // tmux died between the two reads.
        let died = Scripted {
            tmux: RefCell::new(vec![Some("65411 $1".into()), None]),
            ..Scripted::steady("unused")
        };
        assert_eq!(
            observe(&died, workspace(), session()),
            Err(ObserveError::NoTmux)
        );

        let no_boot = Scripted {
            boots: RefCell::new(vec![None, None]),
            ..Scripted::steady("65411 $1")
        };
        assert_eq!(
            observe(&no_boot, workspace(), session()),
            Err(ObserveError::NoBootToken)
        );

        // The SECOND read vanishing is the half a trailing read misses.
        let boot_gone = Scripted {
            boots: RefCell::new(vec![Some("boot-a".into()), None]),
            ..Scripted::steady("65411 $1")
        };
        assert_eq!(
            observe(&boot_gone, workspace(), session()),
            Err(ObserveError::NoBootToken)
        );

        let blank_boot = Scripted {
            boots: RefCell::new(vec![Some("   ".into()); 2]),
            ..Scripted::steady("65411 $1")
        };
        assert!(matches!(
            observe(&blank_boot, workspace(), session()),
            Err(ObserveError::Invalid(_))
        ));
    }

    /// A key carrying a zero birth compares equal to every other unread
    /// process.
    #[test]
    fn an_unreadable_start_time_is_not_a_process() {
        let source = Scripted {
            birth: Some(0),
            ..Scripted::steady("65411 $1")
        };
        assert!(matches!(
            observe(&source, workspace(), session()),
            Err(ObserveError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn a_moved_pane_cannot_redirect_the_session_read() {
        if !cyclops_testrig::tmux_available() {
            return;
        }
        let tmux = cyclops_testrig::TmuxServer::new("live-session-target");
        tmux.run_ok(&["new-session", "-d", "-s", "alpha"]);
        tmux.run_ok(&["new-window", "-d", "-t", "alpha"]);
        tmux.run_ok(&["new-session", "-d", "-s", "beta"]);

        let pane_reply = tmux.run(&["display-message", "-p", "-t", "alpha:0", "#{pane_id}"]);
        assert!(pane_reply.status.success());
        let pane_id = String::from_utf8_lossy(&pane_reply.stdout)
            .trim()
            .to_string();

        let cfg = cyclops_tmux::ControlConfig::attach("alpha")
            .on_socket(tmux.socket())
            .with_config_file("/dev/null");
        let (client, _notifications) = cyclops_tmux::ControlClient::spawn(cfg)
            .await
            .expect("control client");
        let expected: TmuxSessionId = client
            .display("=alpha:", "#{session_id}")
            .await
            .expect("alpha id")
            .parse()
            .expect("session id");

        tmux.run_ok(&["link-window", "-s", "alpha:0", "-t", "beta:1"]);
        tmux.run_ok(&["unlink-window", "-t", "alpha:0"]);
        let moved: TmuxSessionId = client
            .display(&pane_id, "#{session_id}")
            .await
            .expect("moved pane remains live")
            .parse()
            .expect("session id");
        assert_ne!(moved, expected, "the stale pane still named alpha");

        let key = observe_watched(&client, expected, workspace(), |_| Some(4242))
            .await
            .expect("alpha remains observable");
        assert_eq!(key.tmux_session_id(), expected);
        client.shutdown().await;
    }
}
