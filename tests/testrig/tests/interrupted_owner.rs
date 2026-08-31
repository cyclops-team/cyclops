//! Forced-interruption evidence for the external tmux cleanup owner.
//!
//! `Drop` already proves normal return and panic unwind. This executable kills
//! an exact child test process, then observes that the child's server, socket,
//! and session are gone while a neighboring server remains live.
//!
//! This test becomes obsolete if the test runner itself owns exact external
//! resources and proves their cleanup after forced termination.

use std::io::{self, BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};

const CHILD_ENV: &str = "CYCLOPS_TESTRIG_INTERRUPTED_OWNER";
const MARKER: &str = "CYCLOPS_OWNED_TMUX";
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_REAP_RECHECK: Duration = Duration::from_millis(10);

fn server_is_up(socket: &str) -> bool {
    Command::new("tmux")
        .args(["-L", socket, "list-sessions"])
        .env_remove("TMUX")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn assert_owned_resources_are_gone(socket: &str, socket_path: &PathBuf) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while (server_is_up(socket) || socket_path.exists()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !server_is_up(socket),
        "external owner left child server live on {socket}"
    );
    assert!(
        !socket_path.exists(),
        "external owner left child socket {socket_path:?}"
    );
}

#[derive(Clone, Copy)]
enum ChildStop {
    Exact,
    #[cfg(unix)]
    ExactProcessGroup,
}

/// Own the deliberately parked child until the test kills and reaps it.
///
/// Assertions before the intended interruption still need to leave the child
/// and its exact registered tmux resources behind cleanly.
struct ParkedFixtureChild {
    child: Option<Child>,
    stop: ChildStop,
}

impl ParkedFixtureChild {
    fn start(stop: ChildStop) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args(["--exact", "interrupted_fixture_child", "--nocapture"])
            .env(CHILD_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        if matches!(stop, ChildStop::ExactProcessGroup) {
            use std::os::unix::process::CommandExt;
            // This child becomes its own group leader. The test can then
            // reproduce runner cancellation without signalling itself.
            command.process_group(0);
        }
        let child = command.spawn().expect("start exact interrupted fixture");
        Self {
            child: Some(child),
            stop,
        }
    }

    fn ownership(&mut self) -> (String, PathBuf) {
        let stdout = self
            .child
            .as_mut()
            .expect("parked child is still owned")
            .stdout
            .take()
            .expect("capture child ownership marker");
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("read child marker");
            let Some(fields) = line.strip_prefix(&format!("{MARKER}\t")) else {
                continue;
            };
            let (socket, path) = fields.split_once('\t').expect("socket and path fields");
            return (socket.to_string(), PathBuf::from(path));
        }
        panic!("child did not publish exact owned resources");
    }

    fn stop_and_reap(&mut self) -> io::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let signal = signal_child(self.stop, child);
        let reaped = wait_for_child_exit(child);
        if reaped.is_ok() {
            self.child.take();
        }
        match (signal, reaped) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(signal), Ok(())) => Err(signal),
            (Ok(()), Err(reaped)) => Err(reaped),
            (Err(signal), Err(reaped)) => Err(io::Error::other(format!(
                "stopping fixture child failed: {signal}; child also did not exit: {reaped}"
            ))),
        }
    }
}

impl Drop for ParkedFixtureChild {
    fn drop(&mut self) {
        let _ = self.stop_and_reap();
    }
}

/// Stop only the child test executable and the helpers that inherited its
/// process group. The external cleanup owner must be outside that group: a
/// runner can end a cancelled test this way, after which Rust `Drop` cannot
/// run.
#[cfg(unix)]
fn kill_process_group(leader: u32) -> io::Result<()> {
    let status = Command::new("/bin/kill")
        // A negative target names a process group. End option parsing first:
        // GNU kill otherwise treats the group id as another option and can
        // report success without signalling the parked fixture.
        .args(["-KILL", "--", &format!("-{leader}")])
        .status()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("force-stop child process group {leader}: {error}"),
            )
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "kill child process group {leader} returned {status}"
        )))
    }
}

fn signal_child(stop: ChildStop, child: &mut Child) -> io::Result<()> {
    match stop {
        ChildStop::Exact => child.kill(),
        #[cfg(unix)]
        ChildStop::ExactProcessGroup => match kill_process_group(child.id()) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Group cancellation could not be sent, but the RAII guard
                // still owns this one child and must try to release the
                // registration pipe before it reports the original failure.
                match child.kill() {
                    Ok(()) => Err(error),
                    Err(fallback) => Err(io::Error::other(format!(
                        "process-group signal failed: {error}; exact-child fallback failed: {fallback}"
                    ))),
                }
            }
        },
    }
}

/// Wait only for the exact child process this guard owns. This is test-side
/// process observation, not a scheduling guess: `try_wait` is the event and
/// the deadline prevents a broken signal path from hanging the test runner.
fn wait_for_child_exit(child: &mut Child) -> io::Result<()> {
    let pid = child.id();
    let deadline = Instant::now() + CHILD_REAP_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("fixture child {pid} did not exit within {CHILD_REAP_TIMEOUT:?}"),
            ));
        }
        std::thread::sleep(CHILD_REAP_RECHECK);
    }
}

#[test]
fn interrupted_fixture_child() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
        return;
    }

    let server = TmuxServer::new("interrupted-child");
    server.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "owned",
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ]);
    let socket_path = server
        .socket_path()
        .expect("live child server reports its socket");
    println!("{MARKER}\t{}\t{}", server.socket(), socket_path.display());
    std::io::stdout()
        .flush()
        .expect("publish owned resource ids");

    // The parent kills this process. Parking avoids a guessed lifetime and
    // makes forced termination, not normal return, the only exit edge.
    loop {
        std::thread::park();
    }
}

#[test]
fn killing_an_owner_removes_only_its_exact_tmux_resources() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }

    let neighbor = TmuxServer::new("interrupted-neighbor");
    neighbor.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "neighbor",
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ]);

    let mut child = ParkedFixtureChild::start(ChildStop::Exact);
    let (socket, socket_path) = child.ownership();
    assert!(server_is_up(&socket), "child server never became live");
    assert!(socket_path.exists(), "child socket never appeared");

    child
        .stop_and_reap()
        .expect("force-stop fixture before Drop");

    assert_owned_resources_are_gone(&socket, &socket_path);
    assert!(
        server_is_up(neighbor.socket()),
        "child cleanup touched a neighboring tmux server"
    );
}

#[cfg(unix)]
#[test]
fn killing_a_fixture_process_group_removes_only_its_exact_tmux_resources() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }

    let neighbor = TmuxServer::new("interrupted-group-neighbor");
    neighbor.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "neighbor",
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ]);

    let mut child = ParkedFixtureChild::start(ChildStop::ExactProcessGroup);
    let (socket, socket_path) = child.ownership();
    assert!(server_is_up(&socket), "child server never became live");
    assert!(socket_path.exists(), "child socket never appeared");

    child
        .stop_and_reap()
        .expect("force-stop fixture process group before Drop");

    assert_owned_resources_are_gone(&socket, &socket_path);
    assert!(
        server_is_up(neighbor.socket()),
        "child cleanup touched a neighboring tmux server"
    );
}

#[cfg(unix)]
#[test]
fn a_group_fixture_assertion_unwind_reaps_its_exact_tmux_resources() {
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }

    let mut child = ParkedFixtureChild::start(ChildStop::ExactProcessGroup);
    let (socket, socket_path) = child.ownership();
    assert!(server_is_up(&socket), "child server never became live");
    assert!(socket_path.exists(), "child socket never appeared");

    let unwound = std::panic::catch_unwind(move || {
        let _owned_until_unwind = child;
        panic!("expected panic: prove parked child guard reaps on assertion unwind");
    });
    assert!(unwound.is_err(), "the fixture guard probe must panic");

    assert_owned_resources_are_gone(&socket, &socket_path);
}
