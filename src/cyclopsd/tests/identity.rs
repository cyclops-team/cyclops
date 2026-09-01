//! Sender identity against real processes.
//!
//! Two proofs the unit tests cannot give: peer_of reads this process's own
//! (uid, pid) back over a real socketpair, and the ancestry walk climbs
//! from a real child process spawned inside an isolated tmux pane to that
//! pane's pane_pid.
//!
//! Never touches the user's tmux: every tmux call carries
//! `-L cyc-id-<pid>-<sequence> -f /dev/null` through cyclops-testrig, which kills
//! the server and unlinks its socket on drop.
//! Skips cleanly when tmux is not on PATH. The retry loops are test-side
//! waits for process spawns, outside the daemon's zero-polling contract.

use std::process::Command;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::{os::fd::AsRawFd, path::Path, process::Child};

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::{ControlConfig, SessionWatcher};
#[cfg(target_os = "macos")]
use cyclopsd::identity::peer_identity;
use cyclopsd::identity::{peer_of, resolve_sender, Sender, Vendorship};
#[cfg(target_os = "macos")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "macos")]
use tokio::net::UnixListener;
use tokio::net::UnixStream;

/// Nothing in these synthetic trees is an agent process, and every hop
/// reads: the pane rows are what decide, which is what these tests are
/// about.
fn no_vendors(_: i32) -> Vendorship {
    Vendorship::NotVendor
}

#[tokio::test]
async fn peer_of_reports_own_uid_and_pid_over_socketpair() {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let uid = unsafe { libc::getuid() };
    let me = std::process::id() as i32;
    // Both ends belong to this process; each must see the other as us.
    for end in [&a, &b] {
        let (peer_uid, peer_pid) = peer_of(end).expect("peer_of");
        assert_eq!(peer_uid, uid);
        assert_eq!(peer_pid, me);
    }
}

#[cfg(target_os = "macos")]
fn compile_peer_helper(root: &Path) -> std::path::PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/peer_identity_client.rs");
    let binary = root.join("peer-identity-client");
    let output = Command::new("rustc")
        .args([
            "--edition=2021",
            "-Dwarnings",
            source.to_str().expect("source path is UTF-8"),
            "-o",
            binary.to_str().expect("binary path is UTF-8"),
        ])
        .output()
        .expect("compile peer identity fixture");
    assert!(
        output.status.success(),
        "peer identity fixture compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

#[cfg(target_os = "macos")]
async fn connected_peer(tag: &str, mode: &str) -> (std::path::PathBuf, UnixStream, Child) {
    let root = cyclops_proto::scratch::scratch_dir(tag);
    std::fs::create_dir_all(&root).expect("peer fixture directory");
    let socket = root.join("peer.sock");
    let listener = UnixListener::bind(&socket).expect("bind peer fixture socket");
    let binary = compile_peer_helper(&root);
    let child = Command::new(&binary)
        .args([socket.to_str().expect("socket path is UTF-8"), mode])
        .spawn()
        .expect("spawn peer identity fixture");
    let (mut peer, _) = listener.accept().await.expect("accept fixture peer");
    let mut ready = [0u8; 1];
    peer.read_exact(&mut ready)
        .await
        .expect("read fixture ready byte");
    assert_eq!(ready, *b"R");
    (root, peer, child)
}

/// Removing a running executable's directory entry must not revoke the
/// process's already-proven socket authority. Managed updates may prune an
/// old pair while its workspace process is still connected.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_unlinked_live_executable_keeps_its_peer_identity() {
    let (root, mut peer, mut child) = connected_peer("cyc-peer-unlinked", "hold").await;
    let binary = root.join("peer-identity-client");
    let identity = peer_identity(&peer).expect("capture live peer identity");

    std::fs::remove_file(&binary).expect("unlink running helper executable");
    assert!(
        identity.still_current(peer.as_raw_fd()),
        "peer liveness must not depend on an executable pathname"
    );

    peer.write_all(b"X").await.expect("release fixture");
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join fixture wait")
        .expect("wait for fixture");
    assert!(status.success());
    assert!(
        !identity.still_current(peer.as_raw_fd()),
        "the same identity must stop matching after process exit"
    );
    drop(peer);
    std::fs::remove_dir_all(root).expect("remove peer fixture directory");
}

/// A descendant may inherit the connected descriptor, but it never inherits
/// the opener's authority after that opener exits.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_inherited_descriptor_does_not_outlive_its_authenticated_opener() {
    let (root, mut peer, mut opener) = connected_peer("cyc-peer-inherited", "inherit").await;
    let identity = peer_identity(&peer).expect("capture opener identity");

    peer.write_all(b"F").await.expect("ask fixture to fork");
    let status = tokio::task::spawn_blocking(move || opener.wait())
        .await
        .expect("join opener wait")
        .expect("wait for opener");
    assert!(status.success());
    let mut inherited = [0u8; 1];
    peer.read_exact(&mut inherited)
        .await
        .expect("the descendant retained the descriptor");
    assert_eq!(inherited, *b"I");
    assert!(
        !identity.still_current(peer.as_raw_fd()),
        "a descriptor holder is not the process that opened the connection"
    );

    peer.write_all(b"X").await.expect("release descendant");
    let mut eof = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(10), peer.read(&mut eof))
        .await
        .expect("descendant did not exit")
        .expect("read descendant exit");
    assert_eq!(read, 0, "descendant kept the fixture socket open");
    drop(peer);
    std::fs::remove_dir_all(root).expect("remove peer fixture directory");
}

/// Child pids of `pid` via pgrep -P. Empty when it has none.
fn children(pid: i32) -> Vec<i32> {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Executable name of `pid` ("sleep", possibly path-prefixed on macOS).
fn comm(pid: i32) -> String {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Breadth-first pgrep -P chain from the pane pid downward: find the
/// `sleep` process wherever the shell put it (direct child, or under an
/// intermediate sh).
fn find_sleep_below(pane_pid: i32) -> Option<i32> {
    let mut frontier = vec![pane_pid];
    for _ in 0..3 {
        let mut next = Vec::new();
        for pid in frontier {
            for child in children(pid) {
                let name = comm(child);
                if name == "sleep" || name.ends_with("/sleep") {
                    return Some(child);
                }
                next.push(child);
            }
        }
        frontier = next;
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn ancestry_walk_resolves_real_child_in_pane() {
    if !tmux_available() {
        eprintln!("skipping identity integration test: tmux not on PATH");
        return;
    }
    let tmux = TmuxServer::new("id");
    tmux.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "idsess",
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ]);

    // The watcher supplies pane_id and pane_pid exactly the way the daemon
    // will feed them to resolve_sender.
    let cfg = ControlConfig::attach("idsess")
        .on_socket(tmux.socket().to_string())
        .with_config_file("/dev/null");
    let w = SessionWatcher::connect(cfg).await.expect("connect watcher");
    let row = w.snapshot().into_iter().next().expect("one pane");
    assert!(row.pane_pid > 0, "pane_pid must be real: {row:?}");

    // Long enough to survive a loaded CI box; teardown reaps it early on
    // every normal run.
    tmux.run_ok(&["send-keys", "-t", "idsess", "sleep 30", "Enter"]);

    // Bounded wait for the shell to spawn sleep (test-side only).
    let mut sleep_pid = None;
    for _ in 0..100 {
        sleep_pid = find_sleep_below(row.pane_pid);
        if sleep_pid.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sleep_pid = sleep_pid.expect("sleep never appeared under the pane pid");
    assert_ne!(
        sleep_pid, row.pane_pid,
        "the walk must cross at least one hop"
    );

    let uid = unsafe { libc::getuid() };

    // Labeled pane: the child resolves to the label.
    let labeled = vec![(row.pane_id.clone(), Some("codex".to_string()), row.pane_pid)];
    assert_eq!(
        resolve_sender(uid, sleep_pid, &labeled, no_vendors),
        Sender::Agent("codex".to_string())
    );

    // Unlabeled pane: the child resolves to the pane id.
    let unlabeled = vec![(row.pane_id.clone(), None, row.pane_pid)];
    assert_eq!(
        resolve_sender(uid, sleep_pid, &unlabeled, no_vendors),
        Sender::Pane(row.pane_id.clone())
    );

    // This test process lives outside the tmux tree: same uid, no pane in
    // its ancestry, so it is the human.
    assert_eq!(
        resolve_sender(uid, std::process::id() as i32, &labeled, no_vendors),
        Sender::Admin
    );

    w.shutdown().await;
}
