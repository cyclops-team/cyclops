//! Headless agents: an agent process with no pane, reachable over the
//! socket only.
//!
//! A pane agent is identified by the pane its process tree reaches. A
//! headless agent has no pane, so its registration is one more root row:
//! the nearest vendor process at or above the registering peer, on a path
//! proven current to the top of the tree. Nothing is asserted by the
//! request; the label is bound to that exact process generation, and every
//! later caller descending from it resolves to the label exactly the way a
//! pane's helpers resolve to the pane (INVARIANTS rule 5).
//!
//! Delivery to a headless recipient is mailbox-only: no terminal exists,
//! so the attempt moves `queued -> notified` with `transport: mailbox` and
//! the agent reads over the socket (`inbox next --wait`).
//!
//! The label is released when the process exits. The exit is a named
//! one-shot event, never a poll (rule 8): `kqueue` `EVFILT_PROC NOTE_EXIT`
//! on macOS and `pidfd_open` on Linux, each wrapped in an `AsyncFd`. On a
//! platform with neither, retirement happens the next time the root is
//! observed dead during a resolution, and at boot reverification.

use std::sync::Arc;

use cyclops_proto::{AgentInstanceId, ProcessInstanceId, RecipientKey, WireError};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::identity::{self, HeadlessRoot, ProcId};
use crate::registry::{HeadlessAdoption, LabelHolder};
use crate::server::Peer;
use crate::Inner;

/// `headless.register` params.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct HeadlessRegisterParams {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) manifest: Option<String>,
}

/// `headless.clear` params. With no label the caller clears its own
/// registration; with a label only the operator may clear it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub(crate) struct HeadlessClearParams {
    #[serde(default)]
    pub(crate) label: Option<String>,
}

fn bad_request(message: String) -> WireError {
    WireError {
        code: "bad_request".to_string(),
        message,
        data: None,
    }
}

fn denied(message: String) -> WireError {
    WireError {
        code: "denied".to_string(),
        message,
        data: None,
    }
}

fn internal(message: String) -> WireError {
    WireError {
        code: "internal".to_string(),
        message,
        data: None,
    }
}

fn process_instance(process: ProcId) -> Option<ProcessInstanceId> {
    ProcessInstanceId::new(process.pid, process.birth).ok()
}

pub(crate) fn proc_id(root: ProcessInstanceId) -> ProcId {
    ProcId {
        pid: root.pid(),
        birth: root.birth(),
    }
}

/// Register the calling process tree's nearest agent under `label`.
pub(crate) async fn register(
    inner: &Arc<Inner>,
    peer: Peer,
    params: HeadlessRegisterParams,
) -> Result<Value, WireError> {
    let (_uid, pid) = crate::server::daemon_peer(peer)?;
    let label = params.label.trim().to_string();
    if let Some(why) = cyclops_proto::label::refusal(&label) {
        return Err(bad_request(why));
    }
    if let Some(pin) = &params.manifest {
        if !inner.manifests.contains_key(pin) {
            let known: Vec<&str> = inner.manifests.keys().map(String::as_str).collect();
            return Err(bad_request(if known.is_empty() {
                format!("no manifest {pin:?}; this daemon loaded none at all")
            } else {
                format!("no manifest {pin:?}; loaded: {}", known.join(", "))
            }));
        }
    }

    // The walk is the whole authentication: nothing in the request names
    // a process, and a shell that is not below an agent cannot register.
    let pane_roots = crate::server::watched_pane_roots(inner);
    let root = match identity::headless_root(pid, &pane_roots, |process| {
        crate::fusion::is_vendor_now(inner, process)
    }) {
        HeadlessRoot::Vendor(root) => root,
        HeadlessRoot::InsidePane { pane_id } => {
            return Err(WireError {
                code: "use_pane".to_string(),
                message: format!(
                    "this process is inside watched pane {pane_id}; a pane agent is named \
                     with `cyclops name {label} --self` from that pane, and a headless \
                     registration is only for a process that has no pane at all"
                ),
                data: None,
            });
        }
        HeadlessRoot::NoVendor => {
            return Err(denied(
                "no agent process is above this one; a headless registration binds the \
                 nearest agent cyclops has a manifest for, and the operator's own shell \
                 cannot register"
                    .to_string(),
            ));
        }
        HeadlessRoot::Unprovable => {
            return Err(denied(
                "the process tree above this peer could not be proven current; nothing was \
                 registered"
                    .to_string(),
            ));
        }
    };
    let detected = crate::fusion::vendor_manifest_now(inner, root.pid)
        .filter(|(_, process)| *process == root)
        .map(|(manifest, _)| manifest);
    if let (Some(pin), Some(found)) = (&params.manifest, &detected) {
        if pin != found {
            return Err(bad_request(format!(
                "this process is pinned to {pin:?} but {found:?} is what is running as it"
            )));
        }
    }
    let manifest = params.manifest.clone().or_else(|| detected.clone());
    let root_instance = process_instance(root)
        .ok_or_else(|| denied("the agent process has no valid process identity".to_string()))?;
    let os_boot_id = crate::livesession::current_os_boot_id()
        .ok_or_else(|| internal("cannot read the OS boot token".to_string()))?;

    let (recipient, fresh) = crate::with_messaging_publication(inner, |messaging| {
        let mut registry = inner.registry.lock().expect("registry lock");
        // The same root registering again keeps its key: an agent that
        // re-runs its startup keeps the mailbox it already has.
        let (recipient, fresh) = match registry.headless_for_root(root_instance) {
            Some(existing) => (existing.recipient, false),
            None => (
                RecipientKey::headless(
                    inner.workspace_id,
                    AgentInstanceId::from_uuid(uuid::Uuid::new_v4()).expect("non-nil UUID"),
                ),
                true,
            ),
        };
        if let Some(holder) = registry.label_holder(&label, Some(recipient)) {
            return Err(bad_request(match holder {
                LabelHolder::Pane(holder) => crate::label_taken_words(inner, &label, &holder),
                LabelHolder::Headless(holder) => format!(
                    "label {label:?} is already taken by a headless agent (pid {}). \
                     Pick another name.",
                    holder.root.pid()
                ),
            }));
        }
        registry
            .adopt_headless(HeadlessAdoption {
                label: label.clone(),
                recipient,
                root: root_instance,
                os_boot_id,
                manifest: manifest.clone(),
            })
            .map_err(|error| internal(format!("cannot record the registration: {error}")))?;
        drop(registry);
        if messaging
            .is_some_and(|messaging| !crate::refresh_mailbox_directory_published(inner, messaging))
        {
            return Err(internal(
                "the name was recorded but its mailbox route could not be published".to_string(),
            ));
        }
        Ok((recipient, fresh))
    })?;
    inner.emit("messages.route_changed", json!({}), None);
    crate::apply_messaging_availability_change(inner);
    info!(label = %label, pid = root.pid, recipient = %recipient, "headless agent registered");

    if fresh {
        arm_exit_watcher(inner, recipient, root_instance);
    }
    // Armed, then re-read: a root that exited between the walk and the
    // watch never answers to its label.
    if !root.still_live() {
        retire(inner, recipient);
        return Err(denied(
            "the agent process exited during registration; nothing is registered".to_string(),
        ));
    }
    Ok(json!({
        "label": label,
        "recipient": recipient,
        "agent_instance_id": recipient.agent_instance_id(),
        "manifest": params.manifest,
        "detects_as": detected,
        "headless": true,
        "pid": root.pid,
    }))
}

/// Release a headless label: the caller's own, or one named by the operator.
pub(crate) fn clear(
    inner: &Arc<Inner>,
    peer: Peer,
    params: HeadlessClearParams,
) -> Result<Value, WireError> {
    let Some((_, caller)) = crate::server::workspace_messaging_caller_if_available(inner, peer)?
    else {
        return Err(WireError {
            code: "mailbox_unavailable".to_string(),
            message: "durable workspace identity is not connected".to_string(),
            data: None,
        });
    };
    let recipient = match params.label {
        Some(label) => {
            if !caller.key.is_admin() {
                return Err(denied(
                    "clearing another agent's headless label requires the workspace \
                     administrator"
                        .to_string(),
                ));
            }
            let registry = inner.registry.lock().expect("registry lock");
            match registry.label_holder(&label, None) {
                Some(LabelHolder::Headless(holder)) => holder.recipient,
                Some(LabelHolder::Pane(_)) => {
                    return Err(bad_request(format!(
                        "{label:?} names a pane, not a headless agent; take it back with \
                         cyclops name {label} --clear"
                    )));
                }
                None => {
                    return Err(WireError {
                        code: "no_such_target".to_string(),
                        message: format!("no headless agent {label:?}"),
                        data: None,
                    });
                }
            }
        }
        None if caller.key.is_headless() => caller.key,
        None => {
            return Err(denied(
                "this process is not registered headless; name the label to clear as the \
                 workspace administrator"
                    .to_string(),
            ));
        }
    };
    match retire(inner, recipient) {
        Some(gone) => Ok(json!({"label": gone.label, "cleared": true})),
        None => Err(WireError {
            code: "no_such_target".to_string(),
            message: "the headless registration is already gone".to_string(),
            data: None,
        }),
    }
}

/// Retire one headless registration and publish the route change.
///
/// After this the label is unaddressable and the recipient key resolves
/// nothing; pending mailbox entries stay pending under their key, where
/// the operator can still read them with `msg.read`.
pub(crate) fn retire(inner: &Arc<Inner>, recipient: RecipientKey) -> Option<HeadlessAdoption> {
    let gone = crate::with_messaging_publication(inner, |messaging| {
        let gone = inner
            .registry
            .lock()
            .expect("registry lock")
            .retire_headless(recipient)?;
        if let Some(messaging) = messaging {
            crate::refresh_mailbox_directory_published(inner, messaging);
        }
        Some(gone)
    })?;
    inner.emit("messages.route_changed", json!({}), None);
    crate::apply_messaging_availability_change(inner);
    info!(label = %gone.label, pid = gone.root.pid(), "headless agent retired");
    Some(gone)
}

/// Retire every registration whose root is observed dead right now.
///
/// The exit watcher is the ordinary path. This is the fallback named under
/// INVARIANTS rule 8 for a platform without one, and a second chance on
/// the platforms with one: an observation that already read the process
/// table costs nothing more to act on.
pub(crate) fn retire_dead(inner: &Arc<Inner>, dead: Vec<RecipientKey>) {
    for recipient in dead {
        retire(inner, recipient);
    }
}

/// Arm the one-shot exit event for a registration's root process.
pub(crate) fn arm_exit_watcher(
    inner: &Arc<Inner>,
    recipient: RecipientKey,
    root: ProcessInstanceId,
) {
    match exit_watch::arm(root.pid()) {
        Ok(watch) => {
            let daemon = Arc::clone(inner);
            let handle = tokio::spawn(async move {
                watch.wait().await;
                debug!(pid = root.pid(), %recipient, "headless root exited");
                retire(&daemon, recipient);
            });
            inner
                .extra_tasks
                .lock()
                .expect("extra tasks lock")
                .push(handle);
        }
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            warn!(
                pid = root.pid(),
                "no process exit event on this platform; the headless label is released at \
                 the next resolution that finds the process gone, and at boot"
            );
        }
        Err(error) => {
            // ESRCH: the process is already gone. Anything else: nothing to
            // wait on, so the lazy path applies.
            debug!(pid = root.pid(), %error, "cannot watch the headless root; retiring");
            retire(inner, recipient);
        }
    }
}

/// Drop registrations from another boot or a dead process, and arm the
/// exit watcher for every survivor. Boot calls this once after the
/// registry loads and before the first directory is published.
pub(crate) fn reverify_at_boot(inner: &Arc<Inner>) {
    let boot = crate::livesession::current_os_boot_id();
    let dropped = inner
        .registry
        .lock()
        .expect("registry lock")
        .retain_headless(|adoption| {
            boot.as_ref() == Some(&adoption.os_boot_id) && proc_id(adoption.root).still_live()
        });
    for gone in dropped {
        info!(label = %gone.label, pid = gone.root.pid(), "headless registration did not survive the restart");
    }
    let survivors = inner
        .registry
        .lock()
        .expect("registry lock")
        .headless_adoptions();
    for adoption in survivors {
        arm_exit_watcher(inner, adoption.recipient, adoption.root);
    }
}

/// The platform's process-exit event, wrapped so the daemon awaits it once.
#[cfg(target_os = "macos")]
mod exit_watch {
    use std::os::fd::{FromRawFd, OwnedFd};

    use tokio::io::unix::AsyncFd;

    pub(super) struct ProcessExit {
        queue: AsyncFd<OwnedFd>,
    }

    /// `kqueue` with one `EVFILT_PROC` `NOTE_EXIT` entry for `pid`. The
    /// queue descriptor itself becomes readable when the event fires, so
    /// tokio can wait on it like any other descriptor: one wake, no timer.
    pub(super) fn arm(pid: i32) -> std::io::Result<ProcessExit> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Owned from here on: an early return below closes it.
        let queue = unsafe { OwnedFd::from_raw_fd(kq) };
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let rc = unsafe { libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if rc < 0 {
            // ESRCH: already gone. The caller retires on the spot.
            return Err(std::io::Error::last_os_error());
        }
        // Read interest only: a kqueue descriptor answers EVFILT_READ and
        // refuses EVFILT_WRITE with EINVAL, and the default registration
        // asks for both.
        Ok(ProcessExit {
            queue: AsyncFd::with_interest(queue, tokio::io::Interest::READABLE)?,
        })
    }

    impl ProcessExit {
        /// Resolve once the process has exited. A readable queue is
        /// confirmed by draining it; a spurious wake clears the readiness
        /// and waits again, still with no timer.
        pub(super) async fn wait(self) {
            loop {
                let Ok(mut guard) = self.queue.readable().await else {
                    return;
                };
                let mut out: libc::kevent = unsafe { std::mem::zeroed() };
                let zero = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                let rc = unsafe {
                    libc::kevent(
                        std::os::fd::AsRawFd::as_raw_fd(self.queue.get_ref()),
                        std::ptr::null(),
                        0,
                        &mut out,
                        1,
                        &zero,
                    )
                };
                if rc != 0 {
                    // The event, or a queue that can no longer be read:
                    // either way the wait is over.
                    return;
                }
                guard.clear_ready();
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod exit_watch {
    use std::os::fd::{FromRawFd, OwnedFd};

    use tokio::io::unix::AsyncFd;

    pub(super) struct ProcessExit {
        pidfd: AsyncFd<OwnedFd>,
    }

    /// `pidfd_open(2)`: the descriptor becomes readable when the process
    /// exits. Linux 5.3 and later; an older kernel answers `ENOSYS`, which
    /// is reported as unsupported so the lazy path applies.
    pub(super) fn arm(pid: i32) -> std::io::Result<ProcessExit> {
        let fd =
            unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOSYS) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "pidfd_open is unavailable on this kernel",
                ));
            }
            return Err(error);
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        Ok(ProcessExit {
            pidfd: AsyncFd::with_interest(pidfd, tokio::io::Interest::READABLE)?,
        })
    }

    impl ProcessExit {
        pub(super) async fn wait(self) {
            let _ = self.pidfd.readable().await;
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod exit_watch {
    pub(super) struct ProcessExit;

    pub(super) fn arm(_pid: i32) -> std::io::Result<ProcessExit> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no process exit event on this platform",
        ))
    }

    impl ProcessExit {
        pub(super) async fn wait(self) {}
    }
}
