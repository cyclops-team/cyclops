//! Peer credentials for Unix socket connections.
//!
//! M0 only logs who connected; identity enforcement (fail-closed ACL,
//! sender resolution through pid ancestry) lands in M1/M2. The plumbing is
//! here now so the enforcement change is local to the dispatch layer.
//!
//! macOS: LOCAL_PEERCRED (uid) plus LOCAL_PEERPID (pid) via getsockopt.
//! Linux: SO_PEERCRED (uid and pid in one struct). Constants are spelled
//! locally where libc's coverage has historically drifted.

use tokio::net::UnixStream;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PeerCreds {
    pub uid: u32,
    pub pid: Option<i32>,
}

#[cfg(target_os = "macos")]
pub(crate) fn peer_creds(stream: &UnixStream) -> Option<PeerCreds> {
    use std::os::fd::AsRawFd;

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
        return None;
    }
    let mut pid: libc::pid_t = 0;
    let mut pid_len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let pid_rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            std::ptr::addr_of_mut!(pid).cast(),
            &mut pid_len,
        )
    };
    Some(PeerCreds {
        uid: cred.cr_uid,
        pid: (pid_rc == 0).then_some(pid),
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn peer_creds(stream: &UnixStream) -> Option<PeerCreds> {
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
        return None;
    }
    Some(PeerCreds {
        uid: cred.uid,
        pid: Some(cred.pid),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn peer_creds(_stream: &UnixStream) -> Option<PeerCreds> {
    None
}
