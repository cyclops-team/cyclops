//! Forced-interruption evidence for the external tmux cleanup owner.
//!
//! `Drop` already proves normal return and panic unwind. This executable kills
//! an exact child test process, then observes that the child's server, socket,
//! shell, and session are gone while a neighboring server remains live.
//!
//! This test becomes obsolete if the test runner itself owns exact external
//! resources and proves their cleanup after forced termination.

use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};

const CHILD_ENV: &str = "CYCLOPS_TESTRIG_INTERRUPTED_OWNER";
const MARKER: &str = "CYCLOPS_OWNED_TMUX";

fn server_is_up(socket: &str) -> bool {
    Command::new("tmux")
        .args(["-L", socket, "list-sessions"])
        .env_remove("TMUX")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
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

    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "interrupted_fixture_child", "--nocapture"])
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start exact interrupted fixture");
    let stdout = child.stdout.take().expect("capture child ownership marker");
    let mut owned = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("read child marker");
        let Some(fields) = line.strip_prefix(&format!("{MARKER}\t")) else {
            continue;
        };
        let (socket, path) = fields.split_once('\t').expect("socket and path fields");
        owned = Some((socket.to_string(), PathBuf::from(path)));
        break;
    }
    let (socket, socket_path) = owned.expect("child published its exact owned resources");
    assert!(server_is_up(&socket), "child server never became live");
    assert!(socket_path.exists(), "child socket never appeared");

    child.kill().expect("force-stop fixture before Drop");
    child.wait().expect("reap interrupted fixture");

    let deadline = Instant::now() + Duration::from_secs(5);
    while (server_is_up(&socket) || socket_path.exists()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !server_is_up(&socket),
        "external owner left child server live"
    );
    assert!(
        !socket_path.exists(),
        "external owner left child socket {socket_path:?}"
    );
    assert!(
        server_is_up(neighbor.socket()),
        "child cleanup touched a neighboring tmux server"
    );
}
