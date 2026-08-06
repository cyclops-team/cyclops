//! Fail-closed sender identity from socket peer credentials.
//!
//! The envelope a recipient sees is daemon-built from who actually
//! connected, never from request text (ADR-001: spoof stripping made
//! structural). Two steps:
//!
//! 1. [`peer_of`] reads the kernel's (uid, pid) for the connected peer.
//!    It only reports. The dispatch layer denies any peer whose uid is not
//!    the daemon's uid; that check is the ACL, not this module.
//! 2. [`resolve_sender`] walks the peer pid's process ancestry until a pid
//!    equals the `pane_pid` of a watched pane. Labeled pane: the sender is
//!    that agent. Unlabeled pane: the sender is the pane id. No pane in
//!    the ancestry: the sender is Admin, because a same-uid shell outside
//!    every watched pane is the human (COORDINATION: Admin may talk from
//!    any shell).
//!
//! The walk is bounded (depth cap, visited set) and never blocks: one
//! proc_pidinfo call (macOS) or one /proc read (Linux) per hop.

use std::collections::HashSet;

use tokio::net::UnixStream;

/// Ancestry hops examined per resolution, starting pid included. Agent
/// CLIs sit a handful of forks under their pane shell; 32 is far past any
/// real chain and keeps a pathological tree cheap.
const MAX_ANCESTRY_DEPTH: usize = 32;

/// Who a request came from, as the daemon will stamp it on the envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    /// A watched pane with a cyclops label; carries the label.
    Agent(String),
    /// A watched pane without a label; carries the tmux pane id.
    Pane(String),
    /// A same-uid process outside every watched pane: the human.
    Admin,
}

/// (uid, pid) of the peer on a connected Unix socket.
///
/// Reports only; it never denies. The caller enforces the fail-closed ACL:
/// a uid other than the daemon's own uid is rejected before any request is
/// processed.
#[cfg(target_os = "macos")]
pub fn peer_of(stream: &UnixStream) -> std::io::Result<(u32, i32)> {
    use std::os::fd::AsRawFd;

    // Spelled locally: libc's coverage of these has historically drifted.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERCRED: libc::c_int = 0x001;
    const LOCAL_PEERPID: libc::c_int = 0x002;

    let fd = stream.as_raw_fd();
    let mut cred: libc::xucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::xucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut pid: libc::pid_t = 0;
    let mut pid_len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            std::ptr::addr_of_mut!(pid).cast(),
            &mut pid_len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((cred.cr_uid, pid))
}

/// (uid, pid) of the peer on a connected Unix socket.
///
/// Reports only; it never denies. The caller enforces the fail-closed ACL:
/// a uid other than the daemon's own uid is rejected before any request is
/// processed.
#[cfg(target_os = "linux")]
pub fn peer_of(stream: &UnixStream) -> std::io::Result<(u32, i32)> {
    use std::os::fd::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((cred.uid, cred.pid))
}

/// Unsupported platform: no peer credentials, so the caller's fail-closed
/// ACL denies every connection.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn peer_of(_stream: &UnixStream) -> std::io::Result<(u32, i32)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "peer credentials unsupported on this platform",
    ))
}

/// Resolve a peer (uid, pid) to a sender against the watched panes, given
/// as (pane_id, label, pane_pid) rows.
///
/// `uid` is part of the identity contract but plays no role in the walk:
/// the caller must already have denied any peer whose uid differs from the
/// daemon uid. By the time this runs, every candidate is the same human's
/// process; the question is only which pane, if any, it lives in.
pub fn resolve_sender(uid: u32, pid: i32, panes: &[(String, Option<String>, i32)]) -> Sender {
    let _ = uid;
    resolve_with(pid, panes, parent_of)
}

/// The walk, with the parent lookup injected so tests drive it over
/// synthetic process trees.
fn resolve_with<F>(pid: i32, panes: &[(String, Option<String>, i32)], parent: F) -> Sender
where
    F: Fn(i32) -> Option<i32>,
{
    let mut current = pid;
    let mut seen: HashSet<i32> = HashSet::new();
    for _ in 0..MAX_ANCESTRY_DEPTH {
        // pid 0 is the kernel; a repeat means the tree lied (pid reuse
        // mid-walk). Both end the walk.
        if current <= 0 || !seen.insert(current) {
            break;
        }
        if let Some((pane_id, label, _)) = panes.iter().find(|(_, _, pp)| *pp == current) {
            return match label {
                Some(l) => Sender::Agent(l.clone()),
                None => Sender::Pane(pane_id.clone()),
            };
        }
        match parent(current) {
            Some(p) => current = p,
            None => break,
        }
    }
    Sender::Admin
}

/// Parent pid of a process. None when the process is gone.
///
/// Not the sysctl KERN_PROC_PID route the design sketch named: libc
/// 0.2.189 defines no kinfo_proc for apple targets, and hand-spelling that
/// struct's layout (extern_proc plus eproc) is exactly the fragile unsafe
/// this crate avoids. proc_pidinfo(PROC_PIDTBSDINFO) reads the same kernel
/// record through a struct libc does define.
#[cfg(target_os = "macos")]
fn parent_of(pid: i32) -> Option<i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    // Returns bytes written; anything short of the full struct (missing
    // pid, zombie, permission) is no usable answer.
    if rc != size {
        return None;
    }
    Some(info.pbi_ppid as i32)
}

/// Parent pid from /proc/<pid>/stat field 4. None when the process is gone.
#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is in parens and may contain spaces or parens; the
    // fixed fields start after the last ')': state, then ppid.
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Unsupported platform: no ancestry, so only a direct pane_pid match can
/// resolve to a pane.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_of(_pid: i32) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn panes() -> Vec<(String, Option<String>, i32)> {
        vec![
            ("%1".to_string(), Some("codex".to_string()), 200),
            ("%2".to_string(), None, 300),
        ]
    }

    /// Synthetic tree as a child -> parent map; absent pids have no parent.
    fn tree(edges: &[(i32, i32)]) -> impl Fn(i32) -> Option<i32> + '_ {
        let map: HashMap<i32, i32> = edges.iter().copied().collect();
        move |pid| map.get(&pid).copied()
    }

    #[test]
    fn walk_reaches_labeled_pane_through_ancestors() {
        // 500 -> 400 -> 200 (pane %1, labeled codex)
        let parent = tree(&[(500, 400), (400, 200), (200, 1)]);
        assert_eq!(
            resolve_with(500, &panes(), parent),
            Sender::Agent("codex".to_string())
        );
    }

    #[test]
    fn walk_reaches_unlabeled_pane_and_reports_pane_id() {
        let parent = tree(&[(600, 300)]);
        assert_eq!(
            resolve_with(600, &panes(), parent),
            Sender::Pane("%2".to_string())
        );
    }

    #[test]
    fn starting_pid_itself_matches_without_a_hop() {
        // No parent edges at all: the match must come from pid 200 itself.
        assert_eq!(
            resolve_with(200, &panes(), tree(&[])),
            Sender::Agent("codex".to_string())
        );
    }

    #[test]
    fn no_pane_in_ancestry_is_admin() {
        let parent = tree(&[(900, 800), (800, 1), (1, 0)]);
        assert_eq!(resolve_with(900, &panes(), parent), Sender::Admin);
    }

    #[test]
    fn missing_parent_ends_walk_as_admin() {
        // 700's parent is unknown (process exited mid-walk).
        assert_eq!(resolve_with(700, &panes(), tree(&[])), Sender::Admin);
    }

    #[test]
    fn cycle_terminates_as_admin() {
        let parent = tree(&[(10, 20), (20, 10)]);
        assert_eq!(resolve_with(10, &panes(), parent), Sender::Admin);
    }

    #[test]
    fn depth_cap_bounds_the_walk() {
        // Chain 1000 -> 999 -> ... with the pane pid deeper than the cap.
        let edges: Vec<(i32, i32)> = (0..60).map(|i| (1000 - i, 999 - i)).collect();
        let deep_panes = vec![("%9".to_string(), None, 1000 - 40)];
        assert_eq!(resolve_with(1000, &deep_panes, tree(&edges)), Sender::Admin);
        // Same chain with the pane inside the cap resolves.
        let near_panes = vec![("%9".to_string(), None, 1000 - 20)];
        assert_eq!(
            resolve_with(1000, &near_panes, tree(&edges)),
            Sender::Pane("%9".to_string())
        );
    }

    #[test]
    fn nonpositive_pid_is_admin() {
        assert_eq!(resolve_with(0, &panes(), tree(&[])), Sender::Admin);
        assert_eq!(resolve_with(-1, &panes(), tree(&[])), Sender::Admin);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn real_parent_of_agrees_with_getppid() {
        let me = std::process::id() as i32;
        let ppid = unsafe { libc::getppid() };
        assert_eq!(parent_of(me), Some(ppid));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn resolve_sender_walks_the_real_tree() {
        let uid = unsafe { libc::getuid() };
        let me = std::process::id() as i32;
        let ppid = unsafe { libc::getppid() };

        // Nothing watched: everything same-uid is the human.
        assert_eq!(resolve_sender(uid, me, &[]), Sender::Admin);

        // Our own parent as a watched pane: one real ancestry hop.
        let panes = vec![("%7".to_string(), Some("claude".to_string()), ppid)];
        assert_eq!(
            resolve_sender(uid, me, &panes),
            Sender::Agent("claude".to_string())
        );
    }
}
