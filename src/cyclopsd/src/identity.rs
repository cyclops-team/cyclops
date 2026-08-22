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
    /// A same-uid process proven to sit outside every watched pane: the
    /// human. Proven means the walk reached the top of the process tree
    /// without meeting one, not that it stopped looking.
    Admin,
    /// The ancestry could not be walked to an answer. A missing parent, a
    /// cycle, or a tree deeper than the cap.
    ///
    /// Distinct from `Admin` because they arrive the same way and mean
    /// opposite things. Collapsing them stamps the operator's name on a
    /// message from a process nobody could place, and the operator is a
    /// trusted sender.
    Unprovable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOrigin {
    Admin,
    Pane {
        pane_id: String,
        label: Option<String>,
        pane_root: ProcId,
        /// True when the peer reached this pane through a vendor process.
        /// A bare shell in an unassigned pane is the local operator, not an
        /// anonymous agent.
        vendor_below: bool,
    },
    Unprovable,
}

/// A connection's peer, as the kernel attested it.
///
/// A pid names a slot in the process table, and the kernel hands that slot
/// on when the process using it exits. A connection outlives one request,
/// so a peer identified by pid alone can be a different program by the
/// time a later request on the same connection is answered.
///
/// What this carries beyond the number is a generation: something that
/// changes when the process behind the pid does. On macOS the kernel
/// attests it directly, and it is an EXECUTION rather than a process, so a
/// peer that execs across a live connection stops matching. That is
/// stronger than a start time, which an exec does not move.
///
/// Deliberately NOT a claim about who wrote any particular byte. A
/// descriptor can be inherited or passed, so socket credentials identify
/// the process that CONNECTED, and nothing here can tell you which of its
/// children wrote a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentity {
    pub uid: u32,
    pub pid: i32,
    exec: PeerExec,
}

/// What tells one execution from another behind the same pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerExec {
    /// The kernel's own audit token. MEASURED on macOS 26.5: reading
    /// `LOCAL_PEERTOKEN` returns 32 bytes whose pid, pidversion and euid
    /// match the connector, the pidversion INCREMENTS across an exec, a
    /// re-read on the same descriptor reflects the new execution, and
    /// `proc_pidpath_audittoken` answers ESRCH for a token whose execution
    /// is gone.
    #[cfg(target_os = "macos")]
    Token([u32; 8]),
    /// No attested generation available. The start time is the closest
    /// stand-in, and it is weaker: an exec keeps it.
    #[cfg(not(target_os = "macos"))]
    Birth(u64),
}

/// A connection and the peer that opened it.
///
/// The descriptor is kept so the question can be asked AGAIN. A
/// connection outlives one request, and authority derived from it has to
/// be checked at the moment it is used rather than the moment the socket
/// was accepted. Borrowed, not owned: both halves of the connection
/// outlive every request answered on it.
#[derive(Debug, Clone, Copy)]
pub struct PeerConn {
    pub id: PeerIdentity,
    pub fd: std::os::fd::RawFd,
}

impl PeerConn {
    /// The peer, if it is still the one that opened this connection.
    ///
    /// None means the process behind the socket has changed or gone: a
    /// different program, the same program re-executed, or a number
    /// handed on to somebody else. Any of those makes the credentials
    /// this connection was accepted with somebody else's.
    pub fn current(&self) -> Option<PeerIdentity> {
        self.id.still_current(self.fd).then_some(self.id)
    }
}

impl PeerIdentity {
    /// Is the peer on this connection still the one that opened it?
    ///
    /// Asked again at every request that turns a connection into
    /// authority, because a connection is long-lived and the process
    /// behind it need not be.
    pub fn still_current(&self, fd: std::os::fd::RawFd) -> bool {
        match peer_identity_fd(fd) {
            Ok(now) => now == *self && self.alive(),
            Err(_) => false,
        }
    }

    #[cfg(target_os = "macos")]
    fn alive(&self) -> bool {
        let PeerExec::Token(token) = self.exec;
        // Proves the EXECUTION is still there, which a re-read of the
        // socket option cannot: the option answers for whatever holds the
        // pid now, and this answers for the one that connected.
        let mut path = [0i8; 4096];
        let rc = unsafe {
            proc_pidpath_audittoken(
                std::ptr::addr_of!(token).cast(),
                path.as_mut_ptr().cast(),
                path.len() as u32,
            )
        };
        rc > 0
    }

    #[cfg(not(target_os = "macos"))]
    fn alive(&self) -> bool {
        matches!(self.exec, PeerExec::Birth(b) if birth_of(self.pid) == Some(b))
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn proc_pidpath_audittoken(
        token: *const libc::c_void,
        buffer: *mut libc::c_void,
        buffersize: u32,
    ) -> libc::c_int;
}

/// The peer of a connected Unix socket, identified rather than numbered.
pub fn peer_identity(stream: &UnixStream) -> std::io::Result<PeerIdentity> {
    use std::os::fd::AsRawFd;
    peer_identity_fd(stream.as_raw_fd())
}

/// The same question asked of a descriptor, so a connection that has been
/// split into halves can still be asked who is on the other end.
#[cfg(target_os = "macos")]
pub fn peer_identity_fd(fd: std::os::fd::RawFd) -> std::io::Result<PeerIdentity> {
    // Spelled locally: libc's coverage of these has historically drifted.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERTOKEN: libc::c_int = 0x006;

    let mut token = [0u32; 8];
    let mut len = std::mem::size_of_val(&token) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERTOKEN,
            token.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of_val(&token) {
        return Err(std::io::Error::other("short audit token"));
    }
    // Field positions are libbsm's accessors, MEASURED against
    // audit_token_to_euid, audit_token_to_pid and
    // audit_token_to_pidversion on macOS 26.5.
    Ok(PeerIdentity {
        uid: token[1],
        pid: token[5] as i32,
        exec: PeerExec::Token(token),
    })
}

/// Same, where no attested peer identity is available.
///
/// Fails closed on a peer whose start time cannot be read: a process this
/// daemon cannot pin is not one it can attribute anything to.
#[cfg(target_os = "linux")]
pub fn peer_identity_fd(fd: std::os::fd::RawFd) -> std::io::Result<PeerIdentity> {
    let (uid, pid) = linux_peer_of_fd(fd)?;
    let birth = birth_of(pid).ok_or_else(|| std::io::Error::other("peer process unreadable"))?;
    Ok(PeerIdentity {
        uid,
        pid,
        exec: PeerExec::Birth(birth),
    })
}

/// Unsupported platform: no peer credentials, so a connection cannot become
/// an authenticated mailbox caller.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn peer_identity_fd(_fd: std::os::fd::RawFd) -> std::io::Result<PeerIdentity> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "peer credentials unsupported on this platform",
    ))
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

    linux_peer_of_fd(stream.as_raw_fd())
}

#[cfg(target_os = "linux")]
fn linux_peer_of_fd(fd: std::os::fd::RawFd) -> std::io::Result<(u32, i32)> {
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
pub fn resolve_sender<V: Fn(i32) -> Vendorship>(
    uid: u32,
    pid: i32,
    panes: &[(String, Option<String>, i32)],
    is_vendor: V,
) -> Sender {
    let _ = uid;
    match observe_peer_origin(pid, panes, is_vendor) {
        PeerOrigin::Admin => Sender::Admin,
        PeerOrigin::Pane { pane_id, label, .. } => match label {
            Some(label) => Sender::Agent(label),
            None => Sender::Pane(pane_id),
        },
        PeerOrigin::Unprovable => Sender::Unprovable,
    }
}

pub fn resolve_peer_origin<V: Fn(i32) -> Vendorship>(
    uid: u32,
    pid: i32,
    panes: &[(String, Option<String>, i32)],
    is_vendor: V,
) -> PeerOrigin {
    let _ = uid;
    observe_peer_origin(pid, panes, is_vendor)
}

/// Resolve a peer against pane roots whose process generations were already observed.
pub fn resolve_peer_origin_observed<V: Fn(i32) -> Vendorship>(
    uid: u32,
    pid: i32,
    panes: &[(String, Option<String>, ProcId)],
    is_vendor: V,
) -> PeerOrigin {
    let _ = uid;
    let Some(start) = ProcId::of(pid) else {
        return PeerOrigin::Unprovable;
    };
    resolve_peer_origin_with(
        start,
        panes,
        |process| is_vendor(process.pid),
        origin_parent_ident,
        ProcId::still_live,
    )
}

fn observe_peer_origin<V: Fn(i32) -> Vendorship>(
    pid: i32,
    panes: &[(String, Option<String>, i32)],
    is_vendor: V,
) -> PeerOrigin {
    let Some(start) = ProcId::of(pid) else {
        return PeerOrigin::Unprovable;
    };
    let pane_roots: Vec<_> = panes
        .iter()
        .filter_map(|(pane_id, label, root_pid)| {
            ProcId::of(*root_pid).map(|root| (pane_id.clone(), label.clone(), root))
        })
        .collect();
    resolve_peer_origin_with(
        start,
        &pane_roots,
        |process| is_vendor(process.pid),
        origin_parent_ident,
        ProcId::still_live,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginParent {
    Process(ProcId),
    Top,
}

fn resolve_peer_origin_with<V, P, L>(
    start: ProcId,
    panes: &[(String, Option<String>, ProcId)],
    is_vendor: V,
    parent: P,
    is_live: L,
) -> PeerOrigin
where
    V: Fn(ProcId) -> Vendorship,
    P: Fn(ProcId) -> Option<OriginParent>,
    L: Fn(&ProcId) -> bool,
{
    let mut current = start;
    let mut path = Vec::new();
    let mut vendor_below = false;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current.pid <= 0 || path.contains(&current) {
            return PeerOrigin::Unprovable;
        }
        path.push(current);
        if let Some((pane_id, label, pane_root)) =
            panes.iter().find(|(_, _, pane_root)| *pane_root == current)
        {
            let vendor_below = match is_vendor(current) {
                Vendorship::Vendor => true,
                Vendorship::NotVendor => vendor_below,
                Vendorship::Unprovable => return PeerOrigin::Unprovable,
            };
            return if path_is_current(&path, &parent, &is_live) {
                PeerOrigin::Pane {
                    pane_id: pane_id.clone(),
                    label: label.clone(),
                    pane_root: *pane_root,
                    vendor_below,
                }
            } else {
                PeerOrigin::Unprovable
            };
        }
        match is_vendor(current) {
            Vendorship::Vendor => vendor_below = true,
            Vendorship::NotVendor => {}
            Vendorship::Unprovable => return PeerOrigin::Unprovable,
        }
        match parent(current) {
            Some(OriginParent::Process(next)) => current = next,
            Some(OriginParent::Top) => {
                if vendor_below || !path_is_current_to_top(&path, &parent, &is_live) {
                    return PeerOrigin::Unprovable;
                }
                return PeerOrigin::Admin;
            }
            None => return PeerOrigin::Unprovable,
        }
    }
    PeerOrigin::Unprovable
}

fn path_is_current<P, L>(path: &[ProcId], parent: &P, is_live: &L) -> bool
where
    P: Fn(ProcId) -> Option<OriginParent>,
    L: Fn(&ProcId) -> bool,
{
    path.first().is_some_and(is_live)
        && path
            .windows(2)
            .all(|pair| parent(pair[0]) == Some(OriginParent::Process(pair[1])))
}

fn path_is_current_to_top<P, L>(path: &[ProcId], parent: &P, is_live: &L) -> bool
where
    P: Fn(ProcId) -> Option<OriginParent>,
    L: Fn(&ProcId) -> bool,
{
    path_is_current(path, parent, is_live)
        && path
            .last()
            .is_some_and(|process| parent(*process) == Some(OriginParent::Top))
}

/// Resolve a process to the exact watched pane row in its ancestry.
pub fn resolve_pane_origin(
    uid: u32,
    pid: i32,
    panes: &[(String, Option<String>, i32)],
) -> Option<(String, Option<String>, i32)> {
    match resolve_peer_origin(uid, pid, panes, |_| Vendorship::NotVendor) {
        PeerOrigin::Pane {
            pane_id,
            label,
            pane_root,
        } => Some((pane_id, label, pane_root.pid)),
        PeerOrigin::Admin | PeerOrigin::Unprovable => None,
    }
}

/// Resolve a process to a pane without re-observing stored pane-root PIDs.
pub fn resolve_pane_origin_observed(
    uid: u32,
    pid: i32,
    panes: &[(String, Option<String>, ProcId)],
) -> Option<(String, Option<String>, i32)> {
    match resolve_peer_origin_observed(uid, pid, panes, |_| Vendorship::NotVendor) {
        PeerOrigin::Pane {
            pane_id,
            label,
            pane_root,
        } => Some((pane_id, label, pane_root.pid)),
        PeerOrigin::Admin | PeerOrigin::Unprovable => None,
    }
}

/// The first process at or above `pid`, stopping at `root`, that
/// `is_vendor` accepts.
///
/// This answers "which agent instance is this process working for",
/// which is a different question from "which pane is it in". A hook
/// helper is a child of the agent that ran it, so the agent is found by
/// walking up from the helper; whoever currently holds the terminal has
/// no bearing on it. The walk stops at the pane root because nothing
/// above the pane is the pane's business.
pub fn vendor_ancestor<T, F: Fn(i32) -> Option<T>>(pid: i32, root: i32, classify: F) -> Option<T> {
    // Both ends are pinned to an identity before the walk starts. A bare
    // pid names a place in the table, not a process, and this walk is an
    // authentication: every link it follows has to be the one it read.
    let start = ProcId::of(pid)?;
    let root = ProcId::of(root)?;
    vendor_ancestor_with(start, root, |p: ProcId| classify(p.pid), parent_ident)
}

/// The parent of a process, as an identity rather than a number.
///
/// Two reads of the child's own identity bracket the parent lookup, so a
/// pid handed to another process between them is caught instead of
/// followed: the number would be the same and the process would not.
/// The parent comes back identified for the same reason.
fn parent_ident(child: ProcId) -> Option<ProcId> {
    if ProcId::of(child.pid) != Some(child) {
        return None;
    }
    let ppid = parent_of(child.pid)?;
    if ProcId::of(child.pid) != Some(child) {
        return None;
    }
    ProcId::of(ppid)
}

fn origin_parent_ident(child: ProcId) -> Option<OriginParent> {
    if ProcId::of(child.pid) != Some(child) {
        return None;
    }
    let ppid = parent_of(child.pid)?;
    if ProcId::of(child.pid) != Some(child) {
        return None;
    }
    if ppid <= 1 {
        Some(OriginParent::Top)
    } else {
        ProcId::of(ppid).map(OriginParent::Process)
    }
}

/// The classification is produced INSIDE the walk and returned as it
/// stands. Returning a bare pid for the caller to look up again would
/// leave a gap: the process can exit and its number be handed to another
/// vendor between the two, and the caller would then hold a binding for a
/// process that was never in this ancestry at all.
fn vendor_ancestor_with<T, F, P>(start: ProcId, root: ProcId, classify: F, parent: P) -> Option<T>
where
    F: Fn(ProcId) -> Option<T>,
    P: Fn(ProcId) -> Option<ProcId>,
{
    let mut current = start;
    let mut seen: HashSet<ProcId> = HashSet::new();
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current.pid <= 0 || !seen.insert(current) {
            break;
        }
        if let Some(found) = classify(current) {
            // The chain was read one link at a time, and the processes on
            // it can exit while it is being read. A parent that exits
            // orphans its child, and its number can be handed to another
            // vendor before this loop gets there: the classification would
            // then be sound about a process that was never on this chain.
            //
            // So the chain is walked again, and the process just admitted
            // has to still be reachable from the same start before the
            // same root. A chain that moved under us refuses instead of
            // admitting whoever happens to be standing there now.
            return reaches(start, current, root, &parent).then_some(found);
        }
        // The root is examined, never passed: the pane's own shell is the
        // last honest candidate, and its parent is tmux.
        if current == root {
            break;
        }
        match parent(current) {
            Some(p) => current = p,
            None => break,
        }
    }
    None
}

/// Is `target` on the chain from `from` up to `root`, right now, with
/// `root` still at the top of it?
///
/// Both halves are required. Stopping at `target` proves only that the
/// helper is below the agent; it says nothing about whether the agent is
/// still in this PANE. A pane root that exits leaves the helper and the
/// agent linked to each other under a reaper, and a walk that returned as
/// soon as it saw the agent would accept a report from a process that has
/// left the pane entirely. So the walk continues past the target and
/// answers true only when the same root is reached.
fn reaches<P: Fn(ProcId) -> Option<ProcId>>(
    from: ProcId,
    target: ProcId,
    root: ProcId,
    parent: &P,
) -> bool {
    let mut current = from;
    let mut seen: HashSet<ProcId> = HashSet::new();
    let mut saw_target = false;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current.pid <= 0 || !seen.insert(current) {
            return false;
        }
        if current == target {
            saw_target = true;
        }
        if current == root {
            return saw_target;
        }
        match parent(current) {
            Some(p) => current = p,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
fn resolve_with<F, V>(
    pid: i32,
    panes: &[(String, Option<String>, i32)],
    parent: F,
    is_vendor: V,
) -> Sender
where
    F: Fn(i32) -> Option<i32>,
    V: Fn(i32) -> Vendorship,
{
    if pid <= 0 {
        return Sender::Unprovable;
    }
    let ident = |process: i32| ProcId {
        pid: process,
        birth: process.unsigned_abs() as u64 + 1,
    };
    let pane_roots: Vec<_> = panes
        .iter()
        .map(|(pane_id, label, root)| (pane_id.clone(), label.clone(), ident(*root)))
        .collect();
    match resolve_peer_origin_with(
        ident(pid),
        &pane_roots,
        |process| is_vendor(process.pid),
        |process| match parent(process.pid) {
            Some(parent) if parent <= 0 => Some(OriginParent::Top),
            Some(parent) => Some(OriginParent::Process(ident(parent))),
            None if process.pid == 1 => Some(OriginParent::Top),
            None => None,
        },
        |_| true,
    ) {
        PeerOrigin::Admin => Sender::Admin,
        PeerOrigin::Pane { pane_id, label, .. } => match label {
            Some(label) => Sender::Agent(label),
            None => Sender::Pane(pane_id),
        },
        PeerOrigin::Unprovable => Sender::Unprovable,
    }
}

/// What one live observation could prove about a process on a walk.
///
/// Three answers, because two of them are not the same. A bool collapses
/// "read it, and it is not an agent" with "could not read it", and the
/// second one is doubt. Proving the sender is the OPERATOR means proving
/// no agent is in the chain, and an ancestor nobody could read leaves
/// exactly that unproven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendorship {
    /// Read, and it is one of the vendors this daemon ships rules for.
    Vendor,
    /// Read, and it is not.
    NotVendor,
    /// Not read.
    Unprovable,
}

/// A process, identified so that a reused pid is a DIFFERENT process.
///
/// A bare pid is transferable: an agent exits, the kernel hands its
/// number to something unrelated, and anything keyed on the number alone
/// silently transfers to the newcomer. That is an authorization defect
/// wherever the pid stands for "the agent we admitted", which is hook
/// liveness, ACK authority and the argv cache.
///
/// The second half is the process's start time, which the kernel assigns
/// once and never reuses within a boot. Two processes can share a pid;
/// they cannot share a pid AND a birth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcId {
    pub pid: i32,
    /// Kernel start time, in whatever unit the platform reports. Compared,
    /// never interpreted.
    pub birth: u64,
}

impl ProcId {
    /// Read a live process's identity. None when it is gone or cannot be
    /// observed, which is doubt and never a match.
    pub fn of(pid: i32) -> Option<ProcId> {
        (pid > 0).then_some(())?;
        Some(ProcId {
            pid,
            birth: birth_of(pid)?,
        })
    }

    /// Is this still the same process it was?
    ///
    /// Cheap and non-destructive: re-reads the birth and compares. A pid
    /// that now belongs to somebody else answers false, which is the
    /// whole point.
    pub fn still_live(&self) -> bool {
        birth_of(self.pid) == Some(self.birth)
    }
}

/// Kernel start time of a process, the half of [`ProcId`] a pid cannot
/// fake. None when the process is gone or unreadable.
#[cfg(target_os = "macos")]
fn birth_of(pid: i32) -> Option<u64> {
    bsd_info(pid).map(|(_, birth, _)| birth)
}

/// One kernel read, both facts. The parent and the start time come out of
/// the same `proc_bsdinfo` record, so asking for identity costs nothing
/// beyond what the ancestry walk was already paying.
#[cfg(target_os = "macos")]
fn bsd_info(pid: i32) -> Option<(i32, u64, u32)> {
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
    // Seconds and microseconds together: two processes started in the same
    // second are still told apart.
    let birth = info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec;
    Some((info.pbi_ppid as i32, birth, info.pbi_uid))
}

#[cfg(target_os = "linux")]
fn birth_of(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22 (starttime), counting from after the comm field, which may
    // itself contain spaces and parens.
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn birth_of(_pid: i32) -> Option<u64> {
    None
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
    bsd_info(pid).map(|(ppid, _, _)| ppid)
}

/// A process's identity and its owner, from ONE observation.
///
/// The pair has to come from one snapshot. Credentials can change without
/// the start time moving, and a pid can be handed on between two separate
/// reads, so a uid read on its own proves nothing about the process the
/// identity names. On macOS the kernel returns both in a single call; on
/// Linux the two reads are bracketed by a third so a change between them
/// refuses.
#[cfg(target_os = "macos")]
pub fn proc_facts(pid: i32) -> Option<(ProcId, u32)> {
    (pid > 0).then_some(())?;
    let (_, birth, uid) = bsd_info(pid)?;
    Some((ProcId { pid, birth }, uid))
}

#[cfg(target_os = "linux")]
pub fn proc_facts(pid: i32) -> Option<(ProcId, u32)> {
    let before = ProcId::of(pid)?;
    let uid = uid_of(pid)?;
    if ProcId::of(pid) != Some(before) {
        return None;
    }
    Some((before, uid))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn proc_facts(_pid: i32) -> Option<(ProcId, u32)> {
    None
}

/// The uid that owns a process, or None when it could not be read.
///
/// Used to exclude an ancestor structurally: every vendor this daemon
/// admits runs as the daemon's own user, so a process owned by anybody
/// else is not one, and saying so needs no argv read. That matters
/// because the argv of a root-owned process is not readable by a normal
/// user at all, and init sits at the top of every walk.
#[cfg(target_os = "macos")]
pub fn uid_of(pid: i32) -> Option<u32> {
    bsd_info(pid).map(|(_, _, uid)| uid)
}

/// The owner of /proc/<pid> is the process's uid.
#[cfg(target_os = "linux")]
pub fn uid_of(pid: i32) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}"))
        .ok()
        .map(|m| m.uid())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn uid_of(_pid: i32) -> Option<u32> {
    None
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

    /// The common case for the sender walk: every hop was read, and none
    /// of them is an agent, so only the pane rows decide.
    fn no_vendors(_: i32) -> Vendorship {
        Vendorship::NotVendor
    }

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

    /// A pid as an identity, with a birth derived from the number so two
    /// different pids are always two different processes.
    fn id(pid: i32) -> ProcId {
        ProcId {
            pid,
            birth: pid as u64 * 1000,
        }
    }

    /// The same synthetic tree, walked as identities. Every process keeps
    /// the birth `id` gives it, which is the stable case; the reuse tests
    /// below supply their own parent function.
    fn id_tree(edges: &[(i32, i32)]) -> impl Fn(ProcId) -> Option<ProcId> + '_ {
        let map: HashMap<i32, i32> = edges.iter().copied().collect();
        move |p: ProcId| {
            // A link is only followed from the process that was read.
            (p == id(p.pid)).then(|| map.get(&p.pid).copied().map(id))?
        }
    }

    fn origin_tree(edges: &[(i32, i32)]) -> impl Fn(ProcId) -> Option<OriginParent> + '_ {
        let parent = id_tree(edges);
        move |process| match parent(process) {
            Some(parent) if parent.pid <= 1 => Some(OriginParent::Top),
            Some(parent) => Some(OriginParent::Process(parent)),
            None if process.pid == 1 => Some(OriginParent::Top),
            None => None,
        }
    }

    #[test]
    fn peer_origin_is_one_bookended_ancestry_observation() {
        let pane_rows = vec![("%1".to_string(), Some("codex".to_string()), id(200))];
        let no_vendor = |_| Vendorship::NotVendor;

        assert!(matches!(
            resolve_peer_origin_with(id(200), &pane_rows, no_vendor, origin_tree(&[(200, 1)]), |_| true),
            PeerOrigin::Pane { pane_id, .. } if pane_id == "%1"
        ));
        assert!(matches!(
            resolve_peer_origin_with(
                id(500),
                &pane_rows,
                no_vendor,
                origin_tree(&[(500, 400), (400, 200), (200, 1)]),
                |_| true,
            ),
            PeerOrigin::Pane { pane_id, .. } if pane_id == "%1"
        ));
        assert_eq!(
            resolve_peer_origin_with(
                id(500),
                &pane_rows,
                no_vendor,
                origin_tree(&[(500, 1)]),
                |_| true,
            ),
            PeerOrigin::Admin
        );

        let replacement_rows = vec![(
            "%1".to_string(),
            Some("codex".to_string()),
            ProcId {
                pid: 200,
                birth: 999,
            },
        )];
        assert_eq!(
            resolve_peer_origin_with(
                id(500),
                &replacement_rows,
                |process| {
                    if process.pid == 200 {
                        Vendorship::Vendor
                    } else {
                        Vendorship::NotVendor
                    }
                },
                origin_tree(&[(500, 200), (200, 1)]),
                |_| true,
            ),
            PeerOrigin::Unprovable
        );

        let calls = std::cell::Cell::new(0);
        let changing_parent = |process: ProcId| {
            let call = calls.get();
            calls.set(call + 1);
            match (process.pid, call) {
                (500, 0) => Some(OriginParent::Process(id(200))),
                (500, _) => Some(OriginParent::Process(ProcId {
                    pid: 200,
                    birth: 999,
                })),
                (200, _) => Some(OriginParent::Top),
                _ => None,
            }
        };
        assert_eq!(
            resolve_peer_origin_with(id(500), &pane_rows, no_vendor, changing_parent, |_| true),
            PeerOrigin::Unprovable
        );
    }

    #[test]
    fn pane_origin_keeps_vendor_ancestry_at_the_pane_root() {
        let pane_rows = vec![("%1".to_string(), Some("codex".to_string()), id(200))];
        let origin = resolve_peer_origin_with(
            id(500),
            &pane_rows,
            |process| {
                (process.pid == 200)
                    .then_some(Vendorship::Vendor)
                    .unwrap_or(Vendorship::NotVendor)
            },
            origin_tree(&[(500, 200), (200, 1)]),
            |_| true,
        );
        assert!(matches!(
            origin,
            PeerOrigin::Pane {
                vendor_below: true,
                ..
            }
        ));
    }

    /// The pane's own shell prompt is the attack surface: an adopted pane
    /// keeps its label, its adoption and its manifest pin while its agent
    /// is not running, and anyone at that prompt can start anything.
    ///
    /// So being inside the pane is not enough, and neither is holding the
    /// terminal: a hand-started helper holds it while it runs. What
    /// admits a report is descent from an agent process.
    #[test]
    fn a_helper_nobody_started_from_an_agent_is_not_admitted() {
        // 100 is the pane's zsh. The operator typed the helper at its
        // prompt, so 500's only ancestor is the shell.
        let edges = &[(500, 100), (100, 1)];
        let is_agent = |pid: i32| (pid == 300).then_some(pid);
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| is_agent(p.pid),
                id_tree(edges)
            ),
            None,
            "a pane pinned to an agent still admitted a process the agent never started"
        );
    }

    #[test]
    fn a_helper_the_agent_started_is_admitted_as_that_agent() {
        // cyclops hook (500) under a shell (400) under claude (300) under
        // the pane's zsh (100), which is how a vendor runs a hook.
        let edges = &[(500, 400), (400, 300), (300, 100), (100, 1)];
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 300).then_some(p.pid),
                id_tree(edges),
            ),
            Some(300),
            "the report belongs to the agent that ran the hook"
        );
    }

    #[test]
    fn a_pane_that_execs_its_agent_directly_is_admitted_at_the_root() {
        // tmux spawned the agent itself, so the pane root IS the agent
        // and the walk has to examine it rather than stop before it.
        let edges = &[(500, 100), (100, 1)];
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 100).then_some(p.pid),
                id_tree(edges),
            ),
            Some(100)
        );
    }

    #[test]
    fn the_walk_stops_at_the_pane_root() {
        // 1 is above the pane. Whatever it is, it is not this pane's
        // agent, and admitting it would let a process outside the pane
        // vouch for one inside it.
        let edges = &[(500, 100), (100, 1)];
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 1).then_some(p.pid),
                id_tree(edges),
            ),
            None
        );
    }

    /// A link can rot while the chain is being read.
    ///
    /// Shape: the helper's parent is 300, which is agent A. A exits and
    /// orphans the helper, and pid 300 goes to vendor B before the walk
    /// asks about it. B classifies as an agent, and it is a real agent,
    /// but it was never this helper's parent: admitting it would file a
    /// report under a process that has no relationship to the reporter.
    #[test]
    fn a_link_reused_mid_walk_is_not_admitted() {
        // The second walk sees the truth: the helper is orphaned, so 300
        // is no longer reachable from it.
        let calls = std::cell::Cell::new(0);
        let parent = move |p: ProcId| {
            let n = calls.get();
            calls.set(n + 1);
            match (p.pid, n) {
                // First read of the helper still shows the old parent.
                (500, 0) => Some(id(300)),
                // By the replay, the helper is orphaned.
                (500, _) => Some(id(100)),
                (300, _) => Some(id(100)),
                (100, _) => Some(id(1)),
                _ => None,
            }
        };
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 300).then_some(p.pid),
                parent,
            ),
            None,
            "a process that is no longer on this chain was admitted"
        );
    }

    /// The replay has to follow the same PROCESSES, not the same numbers.
    ///
    /// The bug this pins: the walk and its replay both compared bare pids.
    /// A pid is a slot in a table, and the kernel hands it on when the
    /// process using it exits. An intermediate link, or the pane root
    /// itself, could therefore be a different program by the time the
    /// replay reached it, and the chain still looked identical: same
    /// numbers, same shape, different processes. That is an
    /// authentication walk admitting whoever happens to be standing in
    /// the right place.
    #[test]
    fn a_reused_number_is_not_the_same_link() {
        let agent = |p: ProcId| (p.pid == 300).then_some(p);
        let chain = &[(500, 300), (300, 100), (100, 1)];

        // The stable tree admits, which is what the cases below vary.
        assert_eq!(
            vendor_ancestor_with(id(500), id(100), agent, id_tree(chain)),
            Some(id(300)),
            "the honest chain must still be admitted"
        );

        // The INTERMEDIATE keeps its number and becomes another process
        // between the walk and the replay.
        let calls = std::cell::Cell::new(0);
        let reborn_middle = move |p: ProcId| {
            let n = calls.get();
            calls.set(n + 1);
            match p.pid {
                // First pass hands back the agent this walk classified;
                // by the replay, that number belongs to something else.
                500 if n == 0 => Some(id(300)),
                500 => Some(ProcId {
                    pid: 300,
                    birth: 999,
                }),
                300 => Some(id(100)),
                100 => Some(id(1)),
                _ => None,
            }
        };
        assert_eq!(
            vendor_ancestor_with(id(500), id(100), agent, reborn_middle),
            None,
            "the number was the same and the process was not"
        );

        // The PANE ROOT keeps its number and becomes another process. The
        // chain still runs to something called 100; it is no longer this
        // pane.
        let reborn_root = |p: ProcId| match p.pid {
            500 => Some(id(300)),
            300 => Some(ProcId {
                pid: 100,
                birth: 777,
            }),
            100 => Some(id(1)),
            _ => None,
        };
        assert_eq!(
            vendor_ancestor_with(id(500), id(100), agent, reborn_root),
            None,
            "a replacement at the pane root is not the pane root"
        );

        // And a link that cannot be read at all refuses, rather than
        // being skipped.
        let unreadable = |p: ProcId| (p.pid != 300).then(|| id(1));
        assert_eq!(
            vendor_ancestor_with(id(500), id(100), agent, unreadable),
            None
        );
    }

    /// Proving helper -> agent is not proving agent -> this pane.
    ///
    /// The bug this pins: the replay returned as soon as it saw the
    /// admitted process. When the pane's own root exits, the helper and
    /// the agent stay linked to each other under a reaper while the chain
    /// above them now runs to init instead of the pane. The replay saw
    /// the agent, said yes, and a report from a process that had left the
    /// pane was accepted as coming from inside it.
    #[test]
    fn an_agent_that_left_the_pane_is_not_reachable_from_it() {
        // 500 -> 300 -> 100 (the pane root) -> 1, and the pane root has
        // gone: 300 is reparented straight to init.
        let edges = &[(500, 300), (300, 1), (1, 0)];
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 300).then_some(p.pid),
                id_tree(edges),
            ),
            None,
            "the chain no longer runs through this pane"
        );

        // The same shape with the root still in place is admitted.
        let edges = &[(500, 300), (300, 100), (100, 1)];
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 300).then_some(p.pid),
                id_tree(edges),
            ),
            Some(300)
        );

        // The pane root may itself be the vendor: it is examined, and it
        // is its own top.
        let edges = &[(500, 100), (100, 1)];
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 100).then_some(p.pid),
                id_tree(edges),
            ),
            Some(100)
        );
    }

    #[test]
    fn a_cycle_in_the_tree_ends_the_vendor_walk() {
        let edges = &[(500, 400), (400, 500)];
        assert_eq!(
            vendor_ancestor_with(
                id(500),
                id(100),
                |p: ProcId| (p.pid == 300).then_some(p.pid),
                id_tree(edges),
            ),
            None
        );
    }

    #[test]
    fn walk_reaches_labeled_pane_through_ancestors() {
        // 500 -> 400 -> 200 (pane %1, labeled codex)
        let edges = &[(500, 400), (400, 200), (200, 1)];
        assert_eq!(
            resolve_with(500, &panes(), tree(edges), no_vendors),
            Sender::Agent("codex".to_string())
        );
    }

    #[test]
    fn walk_reaches_unlabeled_pane_and_reports_pane_id() {
        let edges = &[(600, 300)];
        assert_eq!(
            resolve_with(600, &panes(), tree(edges), no_vendors),
            Sender::Pane("%2".to_string())
        );
    }

    #[test]
    fn starting_pid_itself_matches_without_a_hop() {
        // No parent edges at all: the match must come from pid 200 itself.
        assert_eq!(
            resolve_with(200, &panes(), tree(&[]), no_vendors),
            Sender::Agent("codex".to_string())
        );
    }

    /// Only a COMPLETE walk that met no pane and no agent is the human.
    ///
    /// The bug this pins: every way of failing to answer produced
    /// `Admin`. A parent that could not be read, a tree that lied, a
    /// chain deeper than the cap, and a nonsense peer pid all stamped the
    /// operator's name on a message from a process nobody could place,
    /// and the operator is the most trusted sender there is.
    #[test]
    fn only_a_complete_walk_out_of_the_tree_is_the_operator() {
        // The real thing: a shell of the operator's, walked all the way
        // to the kernel without meeting a pane or an agent.
        let edges = &[(900, 800), (800, 1), (1, 0)];
        assert_eq!(
            resolve_with(900, &panes(), tree(edges), no_vendors),
            Sender::Admin
        );

        // 700's parent cannot be read, so everything above it is unknown,
        // and one of those unknowns may be a watched pane.
        assert_eq!(
            resolve_with(700, &panes(), tree(&[]), no_vendors),
            Sender::Unprovable
        );

        // A tree that lied, usually a pid reused mid-walk.
        let cycle = tree(&[(10, 20), (20, 10)]);
        assert_eq!(
            resolve_with(10, &panes(), cycle, no_vendors),
            Sender::Unprovable
        );

        // Reaching init IS the top of the tree: MEASURED on macOS, a
        // normal user cannot read pid 1's own parent, so a walk that
        // required pid 0 would deny every message the operator sends.
        let to_init = tree(&[(900, 800), (800, 1)]);
        assert_eq!(
            resolve_with(900, &panes(), to_init, no_vendors),
            Sender::Admin
        );

        // An ancestor nobody could classify leaves the operator's name
        // unproven: that ancestor may be the agent.
        let outside = &[(900, 800), (800, 1)];
        assert_eq!(
            resolve_with(900, &panes(), tree(outside), |p| if p == 800 {
                Vendorship::Unprovable
            } else {
                Vendorship::NotVendor
            }),
            Sender::Unprovable
        );

        // A peer pid that is not a process at all.
        for pid in [0, -1] {
            assert_eq!(
                resolve_with(pid, &panes(), tree(&[]), no_vendors),
                Sender::Unprovable
            );
        }
    }

    /// Reaching the top of the tree proves the caller is outside every
    /// watched pane. It does not prove the caller is a person.
    ///
    /// An agent, or a helper it spawned, outlives a pane root that exited
    /// and is reparented to init. The walk out then looks exactly like a
    /// shell the operator is typing in, and the message would be minted
    /// under the operator's name.
    #[test]
    fn an_orphaned_agent_is_not_the_operator() {
        // 950 (helper) -> 940 (the agent) -> 1 -> 0. The pane root that
        // used to sit between them has exited.
        let orphan = &[(950, 940), (940, 1), (1, 0)];
        assert_eq!(
            resolve_with(950, &panes(), tree(orphan), |p| if p == 940 {
                Vendorship::Vendor
            } else {
                Vendorship::NotVendor
            }),
            Sender::Unprovable
        );
        // The same chain with nothing on it that is an agent is the
        // operator, which is the case this must not break.
        assert_eq!(
            resolve_with(950, &panes(), tree(orphan), no_vendors),
            Sender::Admin
        );
        // The terminator is classified like any other hop, so a
        // classifier that cannot answer for it refuses too. The live one
        // never has to: init is init by definition, and it says so
        // without reading anything.
        assert_eq!(
            resolve_with(950, &panes(), tree(orphan), |p| if p == 1 {
                Vendorship::Unprovable
            } else {
                Vendorship::NotVendor
            }),
            Sender::Unprovable
        );

        // And an agent still INSIDE a watched pane is that pane's agent:
        // the pane row wins before the classifier is ever consulted.
        let inside = tree(&[(500, 400), (400, 200), (200, 1)]);
        assert_eq!(
            resolve_with(500, &panes(), inside, |p| if p == 400 {
                Vendorship::Vendor
            } else {
                Vendorship::NotVendor
            }),
            Sender::Agent("codex".to_string())
        );
    }

    #[test]
    fn depth_cap_bounds_the_walk() {
        // Chain 1000 -> 999 -> ... with the pane pid deeper than the cap.
        let edges: Vec<(i32, i32)> = (0..60).map(|i| (1000 - i, 999 - i)).collect();
        let deep_panes = vec![("%9".to_string(), None, 1000 - 40)];
        assert_eq!(
            resolve_with(1000, &deep_panes, tree(&edges), no_vendors),
            Sender::Unprovable,
            "the rest of the chain was never examined"
        );
        // Same chain with the pane inside the cap resolves.
        let near_panes = vec![("%9".to_string(), None, 1000 - 20)];
        assert_eq!(
            resolve_with(1000, &near_panes, tree(&edges), no_vendors),
            Sender::Pane("%9".to_string())
        );
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
        assert_eq!(resolve_sender(uid, me, &[], no_vendors), Sender::Admin);

        // Our own parent as a watched pane: one real ancestry hop.
        let panes = vec![("%7".to_string(), Some("claude".to_string()), ppid)];
        assert_eq!(
            resolve_sender(uid, me, &panes, no_vendors),
            Sender::Agent("claude".to_string())
        );
    }
}
