//! Sensor fusion: title tier plus screen tier over manifest rules, with
//! output activity as a recompute trigger (never a verdict), and the hook
//! sensor fed by agent.state.report (M1).
//!
//! Tier semantics mirror `Manifest::evaluate`: rules are already sorted by
//! priority, the first match in a region class wins that tier, and the
//! fused verdict is whichever tier winner sits earlier in that same order.
//! When both tiers produced a rule and their states differ, the verdict
//! still goes to the higher-priority rule but the disagreement is exposed
//! on the Detection (GOALS: observable, not an error).
//!
//! Screen capture is consulted last (amendment h): when a pane_title rule
//! alone decides the STATE, capture-pane is skipped entirely.
//!
//! That skip is a cost decision about state, and it is never allowed to
//! decide a write. Only the screen sensor can see a composer, so
//! write-readiness needs a positive clean-composer reading from it (rule
//! 12), and a verdict reached without one refuses. Every caller who is
//! about to write, and `pane.read source=detection`, passes
//! `force_screen` for exactly that reason.

use std::collections::BTreeMap;
use std::sync::Arc;

use cyclops_manifest::{strip_csi, CompiledRule, Manifest, Region};
use cyclops_proto::{
    AgentState, ComposerHold, Detection, ProcessInstanceId, RecipientKey, Sensor, SensorReading,
};
use cyclops_tmux::{PaneRow, SessionWatcher, TmuxError};
use tracing::debug;

use crate::{turnkey, unix_ms, DetEntry, Inner, PaneKey};

/// A hook reading older than this is spent: it can no longer decide fused
/// state on its own (a stale edge must not pin a verdict forever). Checked
/// only when a recompute runs anyway; no timer ages anything.
const HOOK_READING_TTL_MS: u64 = 300_000;
/// Consecutive rules-tier verdicts contradicting the hook reading before
/// the reading is invalidated.
const HOOK_DISAGREE_LIMIT: u32 = 3;

/// Stored hook sensor state per pane: the reading plus how many deciding
/// rules-tier recomputes have contradicted it in a row.
pub(crate) struct HookEntry {
    /// The occupant that reported it, and the rules it was read under.
    pane_pid: crate::identity::ProcId,
    manifest: Option<String>,
    pub(crate) reading: SensorReading,
    pub(crate) disagreements: u32,
}

impl HookEntry {
    /// A reading that remembers whose turn it reported.
    ///
    /// A hook edge is a fact about one process read through one set of
    /// rules. Kept unbound, it outlives both: a replacement occupant
    /// inherits the predecessor's "working", and a pane whose manifest
    /// changed keeps being read by rules that no longer apply.
    pub(crate) fn bound(
        pane_pid: crate::identity::ProcId,
        manifest: Option<String>,
        reading: SensorReading,
    ) -> HookEntry {
        HookEntry {
            reading,
            disagreements: 0,
            pane_pid,
            manifest,
        }
    }

    /// Does this reading still describe the pane in front of us?
    ///
    /// Exact equality, with no escape hatch. A zero pid would mean nobody
    /// established whose turn this was, and a reading nobody can attribute
    /// must not be usable as evidence about whoever holds the pane now.
    fn describes(&self, agent: Option<crate::identity::ProcId>, manifest: Option<&str>) -> bool {
        agent == Some(self.pane_pid) && self.manifest.as_deref() == manifest
    }
}

/// Bind a manifest to a pane by its foreground command. Deterministic:
/// manifests iterate in id order.
pub(crate) fn bind_manifest<'a>(
    manifests: &'a BTreeMap<String, Manifest>,
    current_command: &str,
) -> Option<&'a Manifest> {
    manifests
        .values()
        .find(|m| m.agent.process_names.iter().any(|p| p == current_command))
}

/// Bind a manifest to a pane row: the explicit pin first, then the comm
/// name, then the argv-basename fallback.
///
/// The pin is `cyclops name --manifest <id>` and it wins outright. It
/// exists because both automatic routes read what the pane is RUNNING, and
/// a wrapper script, a `sh -c`, or a versioned symlink (F21) can leave a
/// real agent looking like nothing in particular. A person who says which
/// CLI is in the pane is better evidence than a process name.
///
/// pane_current_command is the kernel comm of the RESOLVED executable, so
/// native installs can report a bare version string ("2.1.220", F21) and
/// never bind by comm; the invoked argv[0] basename still says "claude".
/// The fallback resolves argv once per (pane, pid), cached, and matches it
/// against process_names plus argv_basenames.
///
/// The pid it reads is the pane's FOREGROUND process, not `pane_pid`. See
/// [`foreground_pid`]: an agent started by typing its name at a shell
/// prompt is a child of the pane's first process, and reading `pane_pid`
/// binds every such pane to the shell instead of the agent.
pub(crate) fn bind_manifest_for<'a>(
    inner: &'a Inner,
    session_idx: usize,
    row: &PaneRow,
) -> Option<&'a Manifest> {
    let pinned = inner.session(session_idx).and_then(|slot| {
        let session_instance_id = {
            let link = slot.link.lock().expect("session link lock");
            link.identity.as_ref()?.session_instance_id()
        };
        let pane = row.pane_id.parse().ok()?;
        let root = crate::identity::ProcId::of(row.pane_pid)?;
        let pane_root = ProcessInstanceId::new(root.pid, root.birth).ok()?;
        inner
            .adoption_for_observed_route(
                RecipientKey::agent(inner.workspace_id, session_instance_id, pane),
                &row.pane_id,
                pane_root,
            )
            .and_then(|adoption| adoption.manifest.clone())
    });
    if let Some(pinned) = pinned {
        // A pin that names nothing loaded falls through to detection
        // rather than blinding the pane: the manifest set can shrink
        // between the adoption and this recompute.
        if let Some(m) = inner.manifests.get(&pinned) {
            return Some(m);
        }
    }
    if let Some(m) = bind_manifest(&inner.manifests, &row.current_command) {
        return Some(m);
    }
    if row.pane_pid <= 0 {
        return None;
    }
    argv_bound_manifest(
        inner,
        session_idx,
        &row.pane_id,
        foreground_pid(row.pane_pid),
    )
    .map(|(m, _)| m)
}

/// The agent instance a process is working for, proven from the process
/// tree and its argv.
///
/// [`bind_manifest_for`] answers which RULES should read a pane, and an
/// operator's pin is good evidence for that. This answers who is
/// ALLOWED to speak for the pane, and a pin cannot establish that: it is
/// a claim about the pane, and a pane sitting at its shell prompt keeps
/// its pin while anyone at that prompt runs anything they like. So this
/// route reads only live argv, refuses when nothing between `from` and
/// the pane root is a program the daemon ships a manifest for, and
/// refuses when ps cannot be read at all.
pub(crate) fn vendor_between<'a>(
    inner: &'a Inner,
    _session_idx: usize,
    pane_id: &str,
    from: i32,
    root: i32,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    // The binding is built INSIDE the ancestry walk and returned as it
    // stands. Returning a pid for a second lookup would leave a gap where
    // that process exits, its number goes to another vendor, and the
    // second read produces a valid-looking binding for a process this
    // walk never saw.
    crate::identity::vendor_ancestor(from, root, |p| argv_live(inner, pane_id, p))
}

/// The same binding, read LIVE, with no cache consulted.
///
/// The cache is keyed by process identity, which a reused pid cannot
/// forge, but pid and birth both survive an in-place `exec`: a process can
/// bind as a vendor, exec into something else, and keep the identity it
/// was admitted under. Cursor's launcher does exactly that
/// (`exec -a "$0" "$NODE_BIN" ...`), so exec is not a hypothetical here.
///
/// A stale answer on the manifest-binding path costs a wrong rule set
/// until the next probe. On the authentication path it admits a process
/// that is no longer an agent, so that path pays for a fresh read every
/// time.
/// Is this process, right now, one of the vendors this daemon ships
/// rules for?
///
/// Live argv, never the cache. The cache remembers what a pid WAS, and
/// this question is about what it IS: a process that exec'd into or out
/// of a vendor keeps its pid, and the cached answer would outlive the
/// program it described.
///
/// Three answers, because the caller is proving a NEGATIVE with it: an
/// ancestor nobody could read might be the agent, and treating that as
/// "not an agent" is how an orphaned vendor chain borrows the operator's
/// name.
pub(crate) fn is_vendor_now(inner: &Inner, pid: i32) -> crate::identity::Vendorship {
    use crate::identity::Vendorship;
    match vendor_read(inner, pid, argv_basename, crate::identity::proc_facts) {
        VendorRead::Vendor(_, _) => Vendorship::Vendor,
        VendorRead::NotVendor => Vendorship::NotVendor,
        VendorRead::Unprovable => Vendorship::Unprovable,
    }
}

fn argv_live<'a>(
    inner: &'a Inner,
    pane_id: &str,
    pid: i32,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    let _ = pane_id;
    match vendor_read(inner, pid, argv_basename, crate::identity::proc_facts) {
        VendorRead::Vendor(m, proc) => Some((m, proc)),
        VendorRead::NotVendor | VendorRead::Unprovable => None,
    }
}

/// One live read of what a process IS, for everything that needs to know.
///
/// Two answers where a caller wants a binding, three where a caller is
/// proving a negative, and one definition behind both. Two copies of this
/// would be two definitions of "a vendor of ours", and they would drift:
/// one path would admit a process the other refused to classify.
enum VendorRead<'a> {
    Vendor(&'a Manifest, crate::identity::ProcId),
    NotVendor,
    Unprovable,
}

/// The body of [`argv_live`], with both observations injected so a test
/// can prove it never consults the cache.
fn vendor_read<'a, A, F>(inner: &'a Inner, pid: i32, read_argv: A, read_facts: F) -> VendorRead<'a>
where
    A: Fn(i32) -> Option<String>,
    F: Fn(i32) -> Option<(crate::identity::ProcId, u32)>,
{
    // pid 1 is init by definition rather than by observation on macOS:
    // MEASURED on macOS 26.5, `proc_pidinfo` for it is refused to a normal
    // user, so neither its uid nor its argv reads, and it sits at the top
    // of every ancestry walk. Not applied on Linux, where /proc/1 is
    // readable and pid 1 inside a process namespace can be any program,
    // an agent included.
    #[cfg(target_os = "macos")]
    if pid == 1 {
        return VendorRead::NotVendor;
    }
    // Identity and owner together, from one observation. A uid read on
    // its own proves nothing about the process the identity names:
    // credentials can change without the start time moving, and a pid can
    // be handed on between two separate reads.
    let Some(before) = read_facts(pid) else {
        return VendorRead::Unprovable;
    };
    // Every vendor this daemon admits runs as the daemon's own user, so a
    // process owned by anybody else is not one. Structural, and it needs
    // no argv read, which matters because another user's argv is not
    // readable at all.
    if before.1 != unsafe { libc::getuid() } {
        return VendorRead::NotVendor;
    }
    let Some(base) = read_argv(pid) else {
        return VendorRead::Unprovable;
    };
    // Both halves are re-proven across the argv read, for the same reason
    // the cached path re-proves identity: two observations, one moving
    // system. A changed owner refuses here as surely as a changed process.
    if read_facts(pid) != Some(before) {
        return VendorRead::Unprovable;
    }
    let proc = before.0;
    match manifest_for_basename(&inner.manifests, &base) {
        Some(m) => VendorRead::Vendor(m, proc),
        None => VendorRead::NotVendor,
    }
}

/// The agent instance this pane is running right now, by the same rule.
///
/// Starts from the terminal's foreground leader and walks up, so it lands
/// on the agent whether the agent itself holds the tty or has handed it
/// to something it spawned.
pub(crate) fn admitted_vendor<'a>(
    inner: &'a Inner,
    session_idx: usize,
    row: &PaneRow,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    let leader = foreground_pid_checked(row.pane_pid)?;
    vendor_between(inner, session_idx, &row.pane_id, leader, row.pane_pid)
}

/// Everything a write depends on about a pane, read as ONE observation.
///
/// Three facts that have to agree, and are worthless apart: the
/// foreground leader holding the terminal, the agent process the payload
/// belongs to, and the rules that agent is running under RIGHT NOW.
///
/// The manifest is part of it rather than something remembered from the
/// gate because a process can exec in place: same pid, same start time,
/// same identity, different program. Comparing a remembered manifest
/// against a cached verdict would let a payload written for one vendor
/// land in another's composer under the first one's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub(crate) leader: crate::identity::ProcId,
    pub(crate) agent: crate::identity::ProcId,
    pub(crate) manifest: String,
}

/// The current binding of a pane, or None if any part of it could not be
/// proven now.
pub(crate) fn admitted_binding(
    inner: &Inner,
    session_idx: usize,
    row: &PaneRow,
) -> Option<Binding> {
    let leader_pid = foreground_pid_checked(row.pane_pid)?;
    let leader = crate::identity::ProcId::of(leader_pid)?;
    let (manifest, agent) =
        vendor_between(inner, session_idx, &row.pane_id, leader.pid, row.pane_pid)?;
    Some(Binding {
        leader,
        agent,
        manifest: manifest.agent.id.clone(),
    })
}

/// The manifest that claims this argv[0] basename, by either declared name.
pub(crate) fn manifest_for_basename<'a>(
    manifests: &'a BTreeMap<String, Manifest>,
    base: &str,
) -> Option<&'a Manifest> {
    manifests.values().find(|m| {
        m.agent.argv_basenames.iter().any(|name| name == base)
            || m.agent.process_names.iter().any(|name| name == base)
    })
}

/// The pid whose argv says what a pane is RUNNING.
///
/// tmux's `pane_pid` is the pane's FIRST process, which for an interactive
/// pane is the shell and stays the shell for the pane's whole life. An
/// agent the user starts by typing its name at that prompt is a child of
/// it, so `pane_pid` names the shell no matter what is on screen.
/// MEASURED (tmux 3.7b, Claude Code 2.1.222): a pane running Claude Code
/// reports `pane_current_command` "2.1.222", `pane_pid` the zsh, and
/// `ps -o args=` on that pid "-zsh" — nothing in either sensor says
/// "claude", so the pane bound no manifest and carried no state at all.
///
/// The agent instance's identity, and the reason a pane id and a pane pid
/// are not one.
///
/// `pane_pid` is the process tmux spawned, which for an interactive pane
/// is the SHELL, and it does not change for the pane's whole life. Bind
/// safety evidence to it and an agent can exit, another can be launched
/// at the same prompt, and the second inherits everything the first was
/// trusted for: same pane, same root pid, same command, same manifest.
///
/// The tty's foreground process group is the job the terminal is actually
/// talking to, and a process group's id is its leader's pid, so `tpgid`
/// resolves straight to the running agent. A shell idle at its prompt is
/// its own foreground group and resolves back to `pane_pid` unchanged,
/// which is what makes the agent's exit unbind the manifest again.
pub(crate) fn foreground_pid(pane_pid: i32) -> i32 {
    foreground_pid_checked(pane_pid).unwrap_or(pane_pid)
}

/// The same lookup, with the observation failure kept separate from the
/// answer.
///
/// [`foreground_pid`] reports the pane root when `ps` cannot be read. That
/// is right for BINDING a manifest: a pane nobody can observe binds
/// nothing new, and the shell is the honest fallback identity. It is wrong
/// for holding a pin. A caller comparing a stored agent pid against a
/// silently substituted shell pid compares two different domains and gets
/// a confident wrong answer, so a pin resolves through this and treats
/// `None` as the occupant being gone.
/// Is this process still there?
///
/// `kill(pid, 0)` sends nothing: it asks the kernel to resolve the pid and
/// check permission. `ESRCH` is the one answer that means gone; `EPERM`
/// means it exists and belongs to somebody else. Used to tell a pane whose
/// process EXITED apart from one whose process table could not be read,
/// which are the same `None` from a `ps` that failed and must not be the
/// same decision.
pub(crate) fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 delivers nothing and only inspects.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// What one observation could prove about who is in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Occupant {
    /// The foreground process group leader, read now.
    Leader(i32),
    /// The pane's process is gone. Proven, and prior state may retire.
    Gone,
    /// Nothing could be read. Not evidence, and nothing may retire on it.
    Unprovable,
}

/// Read a pane's foreground leader, keeping "gone" apart from "unknown".
pub(crate) fn occupant_of(pane_pid: i32) -> Occupant {
    match foreground_pid_checked(pane_pid) {
        Some(leader) => Occupant::Leader(leader),
        None if !pid_alive(pane_pid) => Occupant::Gone,
        None => Occupant::Unprovable,
    }
}

pub(crate) fn foreground_pid_checked(pane_pid: i32) -> Option<i32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "tpgid=", "-p", &pane_pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tpgid(&String::from_utf8_lossy(&out.stdout))
}

/// Change a pane's hold, but only if a named delivery still owns it.
///
/// Evidence arrives late. A delivery that resolved on screen evidence
/// stays in the acknowledgement registry, its barrier releases, and the
/// NEXT delivery claims the composer. A correlated acknowledgement for
/// the first one can land after all of that, and an unowned mutation
/// would then move a barrier belonging to a delivery it says nothing
/// about, binding or releasing the wrong turn.
///
/// So the token that claimed the barrier is what may change it, and the
/// token is required: a delivery that never claimed one has nothing to
/// settle, and letting it through unowned is the same defect by a
/// shorter route. A receipt whose owner no longer matches still resolves
/// its own delivery; it just does not touch somebody else's composer.
pub(crate) fn set_hold_owned(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    change: impl FnOnce(ComposerHold) -> Option<ComposerHold>,
) -> bool {
    let (prior_ready, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        if entry.hold_owner.as_deref() != Some(owner) {
            return false;
        }
        let Some(hold) = change(entry.hold) else {
            // The caller declined to change it, which is not a failure:
            // the barrier that is already there is the one it wanted.
            return true;
        };
        if hold == entry.hold {
            return true;
        }
        let prior_ready = (
            entry.detection.write_ready,
            entry.detection.write_block.clone(),
        );
        entry.hold = hold;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, hold);
        (prior_ready, entry.detection.clone())
    };
    wake_readiness(inner, session_idx, pane_id, Some(prior_ready), &det);
    true
}

/// Bind an acknowledged turn to the barrier a delivery is holding.
///
/// This is what puts a pane on the exact lifecycle. Until a hold carries
/// a turn key it runs on the screen, where a delayed end from the
/// PREVIOUS turn is indistinguishable from this one's and can release a
/// payload nothing consumed. The key names the turn that took this
/// delivery, so only that turn's own end can end it.
///
/// One transaction over both stores, in this function's usual order,
/// detections then turn ends. The pin has to be in place before the hold
/// starts waiting on the key, or a burst of later ends can evict the one
/// piece of evidence that would release it.
///
/// Refuses without touching anything when the barrier belongs to another
/// delivery, when the pane's binding cannot be named, or when the hold is
/// already waiting on a DIFFERENT turn. Binding the same key again is
/// idempotent: an acknowledgement can arrive more than once.
pub(crate) fn bind_turn(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    turn: turnkey::TurnKey,
    since_ms: u64,
) -> bool {
    let (prior_ready, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let pane = PaneKey::new(session_idx, pane_id);
        let Some(entry) = map.get_mut(&pane) else {
            return false;
        };
        if entry.hold_owner.as_deref() != Some(owner) {
            return false;
        }
        // The end store is keyed on the pane's binding, so an unnamed
        // binding has nothing to key on.
        let (Some(agent), Some(manifest)) = (entry.agent, entry.manifest.clone()) else {
            return false;
        };
        if entry.turn.as_ref().is_some_and(|t| *t != turn) {
            return false;
        }
        if !turnkey::PaneEnds::pin(
            &mut inner.turn_ends.lock().expect("turn ends lock"),
            &pane,
            agent,
            &manifest,
            &turn,
        ) {
            return false;
        }
        entry.turn = Some(turn);
        // Only a hold that is still WAITING takes the mark. One that
        // already carries a witnessed edge has stronger evidence than an
        // acknowledgement's timestamp.
        if entry.hold.is_waiting() {
            entry.hold = ComposerHold::TurnStarted { since_ms };
        }
        let prior_ready = (
            entry.detection.write_ready,
            entry.detection.write_block.clone(),
        );
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (prior_ready, entry.detection.clone())
    };
    wake_readiness(inner, session_idx, pane_id, Some(prior_ready), &det);
    true
}

/// Claim the composer barrier for one delivery attempt, at the write
/// boundary.
///
/// Exactly one attempt owns it at a time, and the owner travels with it
/// so delayed evidence cannot settle somebody else's barrier: a hook
/// upgrade for a delivery that finished long ago must not promote or
/// clear a hold belonging to the payload sitting in the composer now.
///
/// Success means this owner holds it: a fresh claim, or the same owner
/// claiming again. A different owner refuses, and its caller must not
/// write.
pub(crate) fn claim_hold(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: Option<crate::identity::ProcId>,
    manifest: Option<&str>,
) -> bool {
    let (prior_ready, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        // An admitted agent is a POSITIVE prerequisite for a write, not
        // the absence of a mismatch. A manifest can be pinned by an
        // operator, which chooses rules without authenticating anything,
        // so a shell prompt sitting under an always-idle rule set can
        // look write-ready. Comparing two `None`s as equal is how a
        // payload reaches that shell.
        let Some(agent) = agent else {
            return false;
        };
        // And it has to be the binding the caller proved, or this is a
        // different pane occupant than the one it is about to write to.
        if entry.agent != Some(agent) || entry.manifest.as_deref() != manifest {
            return false;
        }
        // A fresh claim requires a composer this daemon believes is
        // EMPTY and unclaimed. An unowned barrier is not free to take: it
        // is what the sensors raised because somebody's text is in there,
        // and a human typing between the last capture and this moment
        // produces exactly that. Only the same owner may re-claim a
        // barrier it already holds.
        match (entry.hold, entry.hold_owner.as_deref()) {
            (ComposerHold::Clear, None) => {}
            (_, Some(held)) if held == owner => {}
            _ => return false,
        }
        let prior_ready = (
            entry.detection.write_ready,
            entry.detection.write_block.clone(),
        );
        entry.hold_owner = Some(owner.to_string());
        entry.hold = ComposerHold::Staged;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (prior_ready, entry.detection.clone())
    };
    wake_readiness(inner, session_idx, pane_id, Some(prior_ready), &det);
    true
}

/// Release this attempt's barrier when its durable write fact could not be recorded.
///
/// The caller has not asked tmux to paste yet. Exact owner and binding checks
/// prevent a failed attempt from clearing a person's draft or another delivery.
pub(crate) fn release_unwritten_hold(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: crate::identity::ProcId,
    manifest: &str,
) -> bool {
    let (prior_ready, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        if entry.hold != ComposerHold::Staged
            || entry.hold_owner.as_deref() != Some(owner)
            || entry.agent != Some(agent)
            || entry.manifest.as_deref() != Some(manifest)
        {
            return false;
        }
        let prior_ready = (
            entry.detection.write_ready,
            entry.detection.write_block.clone(),
        );
        entry.hold = ComposerHold::Clear;
        entry.hold_owner = None;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (prior_ready, entry.detection.clone())
    };
    wake_readiness(inner, session_idx, pane_id, Some(prior_ready), &det);
    true
}

/// Confirm that an operator action still owns the staged composer.
///
/// Exact payload capture proves the bytes. This check proves that no live
/// lifecycle or blocked-state evidence makes a terminal key unsafe.
pub(crate) fn staged_action_ready(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: cyclops_proto::ProcessInstanceId,
    manifest: &str,
) -> bool {
    let expected = crate::identity::ProcId {
        pid: agent.pid(),
        birth: agent.birth(),
    };
    let map = inner.detections.lock().expect("detections lock");
    map.get(&PaneKey::new(session_idx, pane_id))
        .is_some_and(|entry| staged_entry_ready(entry, owner, expected, manifest))
}

fn staged_entry_ready(
    entry: &DetEntry,
    owner: &str,
    agent: crate::identity::ProcId,
    manifest: &str,
) -> bool {
    entry.hold == ComposerHold::Staged
        && entry.hold_owner.as_deref() == Some(owner)
        && entry.agent == Some(agent)
        && entry.manifest.as_deref() == Some(manifest)
        && !entry.in_mode
        && !entry.detection.stale
        && matches!(
            entry.detection.state,
            AgentState::Idle | AgentState::IdleWithInput
        )
        && entry
            .detection
            .readings
            .iter()
            .all(|reading| matches!(reading.state, AgentState::Idle | AgentState::IdleWithInput))
}

/// Release the staged composer barrier after an explicit operator action.
///
/// The caller has already proved the exact composer bytes. This final
/// check keeps the release bound to the process generation and manifest
/// recorded before the paste, so it cannot clear another occupant's hold.
pub(crate) fn resolve_staged_hold(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    owner: &str,
    agent: cyclops_proto::ProcessInstanceId,
    manifest: &str,
) -> bool {
    let expected = crate::identity::ProcId {
        pid: agent.pid(),
        birth: agent.birth(),
    };
    let (prior_ready, det) = {
        let mut map = inner.detections.lock().expect("detections lock");
        let Some(entry) = map.get_mut(&PaneKey::new(session_idx, pane_id)) else {
            return false;
        };
        if entry.hold != ComposerHold::Staged
            || entry.hold_owner.as_deref() != Some(owner)
            || entry.agent != Some(expected)
            || entry.manifest.as_deref() != Some(manifest)
        {
            return false;
        }
        let prior_ready = (
            entry.detection.write_ready,
            entry.detection.write_block.clone(),
        );
        entry.hold = ComposerHold::Clear;
        entry.hold_owner = None;
        entry.turn = None;
        entry.detection = entry.detection.clone().stamped(entry.in_mode, entry.hold);
        (prior_ready, entry.detection.clone())
    };
    wake_readiness(inner, session_idx, pane_id, Some(prior_ready), &det);
    true
}

/// Wake anyone gating on this pane's readiness when the answer moved.
///
/// Broadcast only. Runtime state and write-readiness move independently,
/// so a pane can refuse and then allow with no state edge between: a
/// composer hold lifting is exactly that shape, and a delivery sleeping
/// on the refusal would sleep through its own release. A `state` line
/// would be a transition that never happened, so this is its own event
/// and it names no ledger line.
///
/// `prior` is None on first sight, which is not a change: nothing was
/// waiting on an answer that did not exist yet.
fn wake_readiness(
    inner: &Arc<Inner>,
    session_idx: usize,
    pane_id: &str,
    prior: Option<(bool, Option<String>)>,
    det: &Detection,
) {
    let now = (det.write_ready, det.write_block.clone());
    if prior.is_some_and(|p| p != now) {
        inner.emit(
            "readiness",
            serde_json::json!({
                "pane_id": pane_id,
                "session_idx": session_idx,
                "write_ready": det.write_ready,
                "write_block": det.write_block,
            }),
            None,
        );
    }
}

/// Keep a pane's last verdict after its capture failed, as a refusal.
///
/// Reporting keeps the last known answer; writing must not. Marking it
/// stale is what stops the gate from reading a retained clean composer as
/// permission to paste (rule 12), and the restamp is where that becomes
/// a refusal rather than a fact each reader has to re-derive.
///
/// It is written back, not just returned. The map is what status and
/// every other consumer read, so handing the refusal to the immediate
/// caller alone would leave all of them on the pre-failure record, which
/// still says write_ready. `since` is left alone: the state did not
/// change, only the confidence in it.
fn retain_stale(
    map: &mut std::collections::HashMap<PaneKey, DetEntry>,
    pane: &PaneKey,
    in_mode: bool,
    occupant: Option<i32>,
    manifest: Option<&str>,
) -> Option<Detection> {
    // Exact match on both, and an unprovable occupant matches nothing.
    // A pane id names a place: an agent can exit and another start at the
    // same shell prompt, inheriting the pane id, the root pid and often
    // the manifest too. Retaining on the pane id alone would hand the
    // newcomer a turn its predecessor was having, and the stale flag does
    // not repair that. It blocks the write; the record still names the
    // wrong agent as working.
    let entry = map.get_mut(pane).filter(|e| {
        occupant.is_some() && e.occupant == occupant && e.manifest.as_deref() == manifest
    })?;
    let mut p = entry.detection.clone();
    p.stale = true;
    let p = p.stamped(in_mode, entry.hold);
    entry.detection = p.clone();
    Some(p)
}

/// A `ps -o tpgid=` line as a pid. A pane with no controlling terminal
/// reports -1, which names no process and must not be looked up.
pub(crate) fn parse_tpgid(line: &str) -> Option<i32> {
    let value: i32 = line.trim().parse().ok()?;
    (value > 0).then_some(value)
}

/// Bind a manifest by the argv[0] basename of a pane's foreground process,
/// memoising the reading only once it has actually bound something. The ps
/// spawn runs when comm binding already missed; never on a clock.
///
/// The asymmetry — remember a hit, re-probe a miss — is load-bearing, not
/// an optimisation. Vendor CLIs ship a shell wrapper that re-execs itself
/// in place, so the pid is stable across the exec while argv[0] flips from
/// the wrapper's interpreter to the agent's own name. cursor-agent's
/// wrapper ends in `exec -a "$0" "$NODE_BIN" ... index.js "$@"`, and
/// MEASURED (cursor-agent 2026.07.23-e383d2b) pid 37750 read:
///
/// ```text
/// t+0.00s  ps args = bash /Users/x/.local/bin/agent    -> "bash",  binds nothing
/// t+0.25s  ps args = /Users/x/.local/bin/agent ...     -> "agent", binds cursor
/// ```
///
/// Recomputes are output-driven and typing `agent` at a prompt echoes that
/// line immediately, so the probe lands in the first window often. Keyed on
/// (pane, pid), a cache that remembered "bash" could never correct itself —
/// the pid never changes — and the pane would read unknown, carry no state
/// and refuse delivery for the rest of that process's life. So a basename
/// that binds nothing means "not settled yet", never "no agent here".
///
/// One entry per pane: the foreground pid changes with every job the shell
/// runs, and keeping the losers would grow the map for the pane's whole
/// life without any of them ever being read again.
pub(crate) fn argv_bound_manifest<'a>(
    inner: &'a Inner,
    session_idx: usize,
    pane_id: &str,
    pid: i32,
) -> Option<(&'a Manifest, crate::identity::ProcId)> {
    // Keyed by process IDENTITY, not by the number. A pid is transferable:
    // an agent exits, the kernel hands its number to something unrelated,
    // and a cache keyed on the number alone would answer "claude" for a
    // process that has never been claude. On an authentication path that
    // is not a stale read, it is an admission of the wrong process.
    //
    // The identity read is the same kernel record the ancestry walk uses,
    // so it costs no extra spawn, and a process that has exited cannot be
    // identified at all, which fails closed.
    argv_bound_with(
        inner,
        session_idx,
        pane_id,
        pid,
        argv_basename,
        crate::identity::ProcId::of,
    )
}

/// The body of [`argv_bound_manifest`], with both observations injected so
/// a test can interleave a pid reuse between them.
fn argv_bound_with<'a, A, I>(
    inner: &'a Inner,
    session_idx: usize,
    pane_id: &str,
    pid: i32,
    read_argv: A,
    read_ident: I,
) -> Option<(&'a Manifest, crate::identity::ProcId)>
where
    A: Fn(i32) -> Option<String>,
    I: Fn(i32) -> Option<crate::identity::ProcId>,
{
    let proc = read_ident(pid)?;
    let pane = PaneKey::new(session_idx, pane_id);
    let key = (pane.clone(), proc);
    let cached = inner
        .argv_cache
        .lock()
        .expect("argv cache lock")
        .get(&key)
        .cloned();
    if let Some(base) = cached {
        return manifest_for_basename(&inner.manifests, &base).map(|m| (m, proc));
    }
    // Spawn outside the lock: ps is slower than every other holder of it.
    let base = read_argv(pid)?;
    // The identity was read BEFORE the argv, and they are two separate
    // observations of a system that does not hold still. A process can
    // exit and its number be handed on between them, in which case this
    // argv describes the REPLACEMENT while the key names the predecessor.
    // Filing that would authorize the newcomer under an identity it never
    // had, so the identity is re-proven against the same birth before
    // anything is written down or returned.
    if read_ident(pid) != Some(proc) {
        return None;
    }
    let bound = manifest_for_basename(&inner.manifests, &base)?;
    let mut cache = inner.argv_cache.lock().expect("argv cache lock");
    cache.retain(|(cached_pane, _), _| cached_pane != &pane);
    // One entry per pane, so a reused pid cannot even collide with a
    // stale sibling entry: the pane's previous binding is already gone.
    cache.insert(key, base);
    // The identity is returned WITH the manifest, from this one verified
    // observation. Handing back only the manifest would leave the caller
    // to re-read the identity, and a process replaced between the two
    // reads would pair one process's identity with another's rules.
    Some((bound, proc))
}

/// argv[0] basename of a live pid via `ps -o args=`.
fn argv_basename(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_argv_basename(&String::from_utf8_lossy(&out.stdout))
}

/// First whitespace-separated token of a ps args line, basename only.
pub(crate) fn parse_argv_basename(args_line: &str) -> Option<String> {
    let first = args_line.split_whitespace().next()?;
    let base = first.rsplit('/').next()?;
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// What a recompute should do with the stored hook reading.
#[derive(Debug, PartialEq, Eq)]
enum HookAction {
    /// Reading is live: feed it to fusion.
    Use,
    /// Reading is spent (TTL) or invalidated (repeated rules-tier
    /// contradiction): drop it from the store and fuse without it.
    Drop,
}

/// Age one hook entry against the rules-tier verdict of this recompute.
/// Disagreement only counts when the rules actually decided something;
/// agreement resets the streak.
fn hook_action(entry: &mut HookEntry, rules_state: AgentState, now_ms: u64) -> HookAction {
    if now_ms.saturating_sub(entry.reading.ts) > HOOK_READING_TTL_MS {
        return HookAction::Drop;
    }
    if rules_state == AgentState::Unknown {
        return HookAction::Use;
    }
    if rules_state == entry.reading.state {
        entry.disagreements = 0;
        return HookAction::Use;
    }
    entry.disagreements += 1;
    if entry.disagreements >= HOOK_DISAGREE_LIMIT {
        HookAction::Drop
    } else {
        HookAction::Use
    }
}

/// Highest-priority pane_title rule matching the title.
pub(crate) fn title_winner<'m>(m: &'m Manifest, title: &str) -> Option<&'m CompiledRule> {
    m.rules
        .iter()
        .find(|r| r.region == Region::PaneTitle && r.matches(title, &[title]))
}

/// Highest-priority screen-region rule matching the capture. Region
/// slicing matches `Manifest::evaluate`: bottom N non-empty lines,
/// restored to top-down order. Production goes through
/// [`screen_winner_esc`] (the recompute may carry an escaped capture);
/// this plain form serves the tests that assert single-capture behavior.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn screen_winner<'m>(m: &'m Manifest, screen: &str) -> Option<&'m CompiledRule> {
    screen_winner_esc(m, screen, None)
}

/// [`screen_winner`] with an optional SGR-escaped capture of the same grid
/// (capture-pane -e), so `line_regex_esc` rules can fire. Escaped lines are
/// judged non-empty on their CSI-stripped text, mirroring
/// `Manifest::evaluate_esc`, so both captures slice the same screen rows.
pub(crate) fn screen_winner_esc<'m>(
    m: &'m Manifest,
    screen: &str,
    screen_esc: Option<&str>,
) -> Option<&'m CompiledRule> {
    let non_empty: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let non_empty_esc: Option<Vec<&str>> = screen_esc.map(|s| {
        s.lines()
            .rev()
            .filter(|l| !strip_csi(l).trim().is_empty())
            .collect()
    });
    m.rules.iter().find(|r| match r.region {
        Region::PaneTitle => false,
        Region::BottomNonEmptyLines(n) => {
            let mut sel: Vec<&str> = non_empty.iter().take(n).copied().collect();
            sel.reverse();
            let esc = non_empty_esc.as_ref().map(|ne| {
                let mut sel: Vec<&str> = ne.iter().take(n).copied().collect();
                sel.reverse();
                sel
            });
            r.matches_esc(&sel.join("\n"), &sel, esc.as_deref())
        }
    })
}

/// Capture the sensor set a manifest needs: the plain grid, plus the
/// SGR-escaped grid when any rule carries `line_regex_esc` clauses (codex
/// ghost vs typed text, F19). A failed escaped capture fails the whole
/// read: with the plain capture alone the esc rules fail closed and typed
/// human text reads as idle, which is the injection hazard they exist to
/// prevent. The caller's doubt handling covers both captures.
async fn capture_screens(
    watcher: &SessionWatcher,
    m: &Manifest,
    pane_id: &str,
) -> Result<(String, Option<String>), TmuxError> {
    let plain = watcher.client().capture_pane(pane_id).await?;
    if !m.has_escaped_rules() {
        return Ok((plain, None));
    }
    let esc = watcher.client().capture_pane_escaped(pane_id).await?;
    Ok((plain, Some(esc)))
}

/// Fuse the tier winners into a Detection. Both readings are kept whenever
/// both tiers fired, whatever the verdict.
pub(crate) fn fuse(
    m: &Manifest,
    title: Option<&CompiledRule>,
    screen: Option<&CompiledRule>,
    ts: u64,
) -> Detection {
    let mut readings = Vec::new();
    if let Some(r) = title {
        readings.push(SensorReading {
            sensor: Sensor::Title,
            state: r.state,
            rule: r.id.clone(),
            ts,
        });
    }
    if let Some(r) = screen {
        readings.push(SensorReading {
            sensor: Sensor::Screen,
            state: r.state,
            rule: r.id.clone(),
            ts,
        });
    }
    // First rule in priority order that one of the tiers selected. Compared
    // by address: both winners are references into m.rules.
    let winner = m.rules.iter().find(|r| {
        let rp: *const CompiledRule = *r;
        title.is_some_and(|t| std::ptr::eq(rp, t)) || screen.is_some_and(|s| std::ptr::eq(rp, s))
    });
    match winner {
        Some(w) => Detection {
            state: w.state,
            disagreement: matches!((title, screen), (Some(t), Some(s)) if t.state != s.state),
            decided_by: w.id.clone(),
            stale: false,
            write_ready: false,
            write_block: None,
            readings,
        },
        None => Detection {
            state: AgentState::Unknown,
            readings,
            disagreement: false,
            decided_by: "no_rule".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        },
    }
}

/// Advance one pane's composer hold, and settle the turn key it waits on.
///
/// Which lifecycle this hold runs on belongs to the HOLD, not to the
/// vendor. A hold carrying a bound turn key runs exact: only that turn's
/// own end ends it, matched structurally and never by arrival time,
/// because an end can be observed before the start it belongs to.
///
/// A hold with no bound key runs on the screen, even where the vendor is
/// capable of naming its turns. Reading the lane off the manifest
/// instead would wedge every keyed vendor whose hook was never installed,
/// or whose exact ACK never arrived: the hold would wait on an end that
/// nobody is going to send. A late matching ACK can still upgrade the
/// same owner to the exact lane; what it cannot do is resurrect a hold
/// the screen lane already released, because the owner no longer matches.
///
/// An end is consumed only once it has DONE something: the hold cleared,
/// or new input superseded the old turn and the hold fell back to
/// `Staged`. Leaving the key pinned in either case would refuse the next
/// distinct start as a hijack, and the pane would never take another
/// turn.
///
/// It has NOT done anything while the hold is still `TurnStarted`. That
/// is an end that landed while a sensor still reads the turn as running,
/// and the release is waiting on the clean frame that follows. Taking
/// the end there spends the only proof this turn ever ended, and the
/// clean frame finds nothing to release against: the barrier holds
/// forever.
fn settle_turn(
    ends: &mut turnkey::Ends,
    pane: &PaneKey,
    agent: Option<crate::identity::ProcId>,
    manifest: Option<&str>,
    turn: Option<&turnkey::TurnKey>,
    hold: ComposerHold,
    det: &Detection,
) -> (ComposerHold, bool) {
    let ended = turn.map(|t| {
        turnkey::PaneEnds::holds(
            ends,
            pane,
            agent.expect("a carried turn implies a proven agent"),
            manifest.unwrap_or_default(),
            t,
        )
    });
    let next = hold.advance(det, ended);
    if let (Some(t), Some(agent), Some(id)) = (turn, agent, manifest) {
        match next {
            // Still waiting on this turn's own end.
            ComposerHold::TurnStarted { .. } => {}
            // Released on that end. Consume it, all or nothing.
            ComposerHold::Clear if ended == Some(true) => {
                turnkey::PaneEnds::take(ends, pane, agent, id, t);
            }
            // The hold stopped waiting on this turn WITHOUT its end:
            // new input superseded it, or the pane died. Retire the pin
            // either way, or the next distinct start is refused as a
            // hijack against a turn nobody will ever end.
            _ => {
                turnkey::PaneEnds::retire(ends, pane, agent, id, t);
            }
        }
    }
    // A hold still waiting on an end that the store may have thrown away
    // is waiting on nothing. It stays refused, because releasing on
    // absent evidence is the failure this whole lane exists to prevent,
    // but it stops being an ordinary wait and says so.
    let stranded = ended == Some(false)
        && matches!(next, ComposerHold::TurnStarted { .. })
        && match (turn, agent, manifest) {
            (Some(_), Some(agent), Some(id)) => {
                turnkey::PaneEnds::evidence_lost(ends, pane, agent, id)
            }
            _ => false,
        };
    (next, stranded)
}

/// Recompute one pane's Detection, update the cache, and emit a "state"
/// event when the fused state changed. `force_screen` runs the full sensor
/// set even when a title rule alone would decide (pane.read detection).
/// Returns None when the pane is gone from the table.
///
/// `session_idx` is the caller's stable session-slot index, not re-derived
/// here from `watcher.session()`: see [`crate::emit_state`]'s doc comment
/// for the rename race that distinction closes. Every call site already
/// has one, from wherever it entered the session (an event's own
/// `session_task`, a resolved recipient, a delivery handle).
pub(crate) async fn recompute_pane(
    inner: &Arc<Inner>,
    session_idx: usize,
    watcher: &SessionWatcher,
    pane_id: &str,
    force_screen: bool,
    cause: &str,
) -> Option<Detection> {
    let route = PaneKey::new(session_idx, pane_id);
    let Some(row) = watcher.pane(pane_id) else {
        inner
            .detections
            .lock()
            .expect("detections lock")
            .remove(&route);
        return None;
    };
    let recovery_recipient =
        crate::composer_recovery::exact_recipient(inner, session_idx, watcher, &row);
    let (recovery_records, recovery_store_error) = match recovery_recipient {
        Some(recipient) => match crate::composer_recovery::active_for_recipient(inner, recipient) {
            Ok(records) => (records, None),
            Err(reason) => (Vec::new(), Some(reason)),
        },
        None => (Vec::new(), None),
    };
    let recovering = !recovery_records.is_empty() || recovery_store_error.is_some();
    let manifest = bind_manifest_for(inner, session_idx, &row);
    let manifest_id = manifest.map(|m| m.agent.id.clone());
    // Resolved once, before anything that needs it: the pane's admitted
    // AGENT, which is the domain hook reports are filed under. The
    // foreground leader can be a helper the agent spawned, and the pane
    // root is a shell that outlives every agent run in it.
    //
    // Out here because it can spawn a process, and nothing waits on the
    // detection cache while `ps` runs.
    let seen = occupant_of(row.pane_pid);
    let occupant = match seen {
        Occupant::Leader(leader) => Some(leader),
        Occupant::Gone | Occupant::Unprovable => None,
    };
    let admitted_observation = occupant.and_then(|_| admitted_binding(inner, session_idx, &row));
    let admitted = admitted_observation.as_ref().map(|binding| binding.agent);
    // A process that EXITED is proof: whatever it was holding is gone
    // with it, and prior state retires normally. A process table that
    // could not be READ is not proof of anything. Nothing this pane holds
    // was disproved by a lookup that failed to answer, so the binding,
    // the hold, its owner and the turn it waits on are frozen below
    // rather than recomputed, and the verdict refuses.
    let unobservable = !row.dead && seen == Occupant::Unprovable;
    let quota_screen_clear = inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&route)
        .filter(|entry| entry.agent == admitted && entry.manifest == manifest_id)
        .is_some_and(|entry| entry.quota_screen_clear);

    // Kept for the emitted event: the verdict below consumes manifest_id.
    let source_manifest = manifest_id.clone().unwrap_or_default();
    let ts = unix_ms();

    if !row.dead && row.in_mode && !recovering {
        // Copy-mode and friends gate delivery in M1; they are not agent
        // states. Keep the prior verdict; status exposes in_mode per row.
        //
        // Resolved before the lock: it spawns a process, and nothing else
        // in the daemon should wait on the detection cache while a `ps`
        // runs.
        let occupant = foreground_pid_checked(row.pane_pid);
        let mut map = inner.detections.lock().expect("detections lock");
        let prior_ready = map
            .get(&route)
            .map(|e| (e.detection.write_ready, e.detection.write_block.clone()));
        // Stamped INTO the cache, not just onto the returned copy. The
        // cache is what status and pane.read read, so stamping only the
        // return value would leave every surface reporting the readiness
        // this pane had before the human started scrolling.
        let det = match map.get_mut(&route) {
            Some(e) => {
                let same_binding = e.agent == admitted && e.manifest == manifest_id;
                e.manifest = manifest_id;
                // Only an observation that answered may rewrite the
                // binding: overwriting it with the nothing a failed
                // lookup returned would strand the hold it protects.
                if occupant.is_some() {
                    e.occupant = occupant;
                    e.agent = admitted;
                }
                if !same_binding {
                    e.quota_screen_clear = false;
                }
                e.in_mode = true;
                e.detection = e.detection.clone().stamped(true, e.hold);
                e.detection.clone()
            }
            None => {
                let det = Detection {
                    state: AgentState::Unknown,
                    readings: Vec::new(),
                    disagreement: false,
                    decided_by: "pane_in_mode".into(),
                    stale: false,
                    write_ready: false,
                    write_block: None,
                }
                .stamped(true, ComposerHold::default());
                map.insert(
                    route.clone(),
                    DetEntry {
                        detection: det.clone(),
                        manifest: manifest_id,
                        occupant,
                        agent: admitted,
                        in_mode: true,
                        quota_screen_clear: false,
                        hold: ComposerHold::default(),
                        turn: None,
                        hold_owner: None,
                        since: std::time::Instant::now(),
                    },
                );
                det
            }
        };
        drop(map);
        // Entering a mode refuses a write without touching the runtime
        // state, so this is the wake that has no state edge behind it.
        wake_readiness(inner, session_idx, pane_id, prior_ready, &det);
        return Some(det);
    }

    let mut detection = if row.dead {
        Detection {
            state: AgentState::Dead,
            readings: Vec::new(),
            disagreement: false,
            decided_by: "pane_dead".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
    } else if let Some(m) = manifest {
        let t_rule = title_winner(m, &row.title);
        // One agreeing screen baseline is required per exact occupant so
        // durable quota holds can recover after restart even when a title
        // rule would otherwise skip capture forever. A held quota keeps
        // this false and therefore keeps consulting screen until reset.
        let need_screen = force_screen || recovering || !quota_screen_clear || t_rule.is_none();
        let mut capture_failed = false;
        let (screen, screen_esc) = if need_screen {
            match capture_screens(watcher, m, pane_id).await {
                Ok((s, esc)) => (Some(s), esc),
                Err(e) => {
                    // Sensor failure is doubt, not evidence: keep the prior
                    // verdict rather than flipping state on a broken read.
                    debug!(pane = pane_id, error = %e, "capture failed; keeping prior state");
                    let retained = {
                        let mut map = inner.detections.lock().expect("detections lock");
                        let prior_ready = map
                            .get(&route)
                            .map(|e| (e.detection.write_ready, e.detection.write_block.clone()));
                        retain_stale(
                            &mut map,
                            &route,
                            row.in_mode,
                            foreground_pid_checked(row.pane_pid),
                            manifest_id.as_deref(),
                        )
                        .map(|det| (prior_ready, det))
                    };
                    if let Some((prior_ready, p)) = retained {
                        // The refusal is news like any other: a pane that
                        // was write-ready and is now refused on stale
                        // evidence has to wake whoever was gating on the
                        // old answer.
                        wake_readiness(inner, session_idx, pane_id, prior_ready, &p);
                        return Some(p);
                    }
                    // Nothing cached describes whoever is in the pane now,
                    // so there is nothing to retain. Fall through and let
                    // the title tier answer for the current occupant: a
                    // fresh reading of a different sensor is not
                    // inheritance, it refuses the write on its own (no
                    // screen reading, rule 12), and a verdict with no
                    // reading at all is relabelled sensor_error below.
                    capture_failed = true;
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        let s_rule = screen
            .as_deref()
            .and_then(|s| screen_winner_esc(m, s, screen_esc.as_deref()));
        let mut det = fuse(m, t_rule, s_rule, ts);
        // No prior to fall back on and the screen sensor errored: the rule
        // set was never fully consulted, and the record must not claim it
        // was (GOALS: the record never lies).
        if capture_failed && det.decided_by == "no_rule" {
            det.decided_by = "sensor_error".into();
        }
        det
    } else {
        Detection {
            state: AgentState::Unknown,
            readings: Vec::new(),
            disagreement: false,
            decided_by: "no_manifest".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
    };

    // Hook sensor (agent.state.report): high-precision edges, incomplete
    // coverage. Rules keep the verdict when they produced one; the hook
    // decides only where rules see nothing, and a live disagreement stays
    // observable either way. Blocked states always come from rules, since
    // no tested CLI hooks its modals or quota (amendment h). Readings age
    // out on TTL or repeated rules-tier contradiction so a stale edge
    // cannot pin fused state.
    if !row.dead {
        let hook = {
            let mut map = inner.hook_readings.lock().expect("hook readings lock");
            match map.get_mut(&route) {
                None => None,
                // A reading from a different occupant, or read under rules
                // this pane no longer uses, is not a reading about this
                // pane at all. Dropping it is the point: kept, it would
                // let a predecessor's turn describe its successor.
                // Attribution FAILED, which is doubt, not proof of a
                // different occupant. The reading is not used this round
                // and is not destroyed either: a single transient `ps`
                // failure would otherwise delete a turn-end edge the hold
                // is waiting for, and the hold would then wait forever
                // for evidence that had already arrived and been thrown
                // away.
                Some(_) if admitted.is_none() => None,
                Some(entry) if !entry.describes(admitted, manifest_id.as_deref()) => {
                    map.remove(&route);
                    None
                }
                Some(entry) => match hook_action(entry, detection.state, ts) {
                    HookAction::Use => Some(entry.reading.clone()),
                    HookAction::Drop => {
                        map.remove(&route);
                        None
                    }
                },
            }
        };
        if let Some(reading) = hook {
            let hook_state = reading.state;
            let hook_rule = reading.rule.clone();
            detection.readings.push(reading);
            if detection.state == AgentState::Unknown {
                detection.state = hook_state;
                detection.decided_by = format!("hook:{hook_rule}");
            } else if hook_state != detection.state {
                detection.disagreement = true;
            }
        }
    }

    let recovery_live = recovery_recipient.and_then(|recipient| {
        admitted_observation
            .as_ref()
            .and_then(|binding| crate::composer_recovery::observed_binding(recipient, binding))
    });
    let recovery_clean = recovery_live.as_ref().is_some_and(|binding| {
        crate::composer_recovery::clean_composer_for_binding(
            &detection,
            row.in_mode,
            manifest_id.as_deref(),
            binding,
        )
    });
    let mut recovery_action = if let Some(reason) = recovery_store_error {
        Some(crate::composer_recovery::RecoveryAction::Hold(reason))
    } else {
        inner
            .composer_recovery
            .lock()
            .expect("composer recovery lock")
            .reconcile(&recovery_records, recovery_live.as_ref(), recovery_clean)
    };
    let retired_attempt = match recovery_action.as_ref() {
        Some(action @ crate::composer_recovery::RecoveryAction::Retire { .. }) => {
            match crate::composer_recovery::persist(inner, action) {
                Ok(attempt_id) => {
                    recovery_action = None;
                    Some(attempt_id)
                }
                Err(reason) => {
                    recovery_action = Some(crate::composer_recovery::RecoveryAction::Hold(reason));
                    None
                }
            }
        }
        _ => None,
    };
    if matches!(
        recovery_action,
        Some(crate::composer_recovery::RecoveryAction::Restore(_))
    ) {
        match crate::composer_recovery::retire_exact_lifecycle(
            inner,
            session_idx,
            pane_id,
            recovery_live.as_ref(),
            recovery_clean,
        ) {
            crate::composer_recovery::LifecycleRetirement::NotReady => {}
            crate::composer_recovery::LifecycleRetirement::Durable(_) => {
                // The matching end is still pinned. Normal settlement below
                // may now clear the runtime hold and consume it.
                recovery_action = None;
            }
            crate::composer_recovery::LifecycleRetirement::Blocked(reason) => {
                recovery_action = Some(crate::composer_recovery::RecoveryAction::Hold(reason));
            }
        }
    }

    let (prior, prior_ready, detection, probe_quota_reset) = {
        let mut map = inner.detections.lock().expect("detections lock");
        if matches!(
            recovery_action.as_ref(),
            Some(crate::composer_recovery::RecoveryAction::Hold(
                "composer_recovery_retirement_pending"
            ))
        ) {
            let pending = recovery_records
                .first()
                .and_then(|record| {
                    inner
                        .composer_recovery
                        .lock()
                        .expect("composer recovery lock")
                        .retirement_pending_reason(record.attempt_id)
                })
                .map(crate::composer_recovery::RecoveryAction::Hold);
            recovery_action = pending;
        }
        let prior_entry = map.get(&route);
        let prior = prior_entry.map(|e| e.detection.state);
        let prior_ready =
            prior_entry.map(|e| (e.detection.write_ready, e.detection.write_block.clone()));
        // The hold describes one AGENT's composer, so it is carried on
        // the vendor identity and its rules, never on the foreground
        // group. A vendor that hands the terminal to a tool it spawned
        // and takes it back changes the foreground group twice without
        // ever ceasing to be the agent holding that composer; carrying
        // the hold on the group would clear it twice, and a runtime
        // `working` state would mask that until the pane came back
        // write-ready with the hold gone.
        let carried = prior_entry
            .filter(|e| admitted.is_some() && e.agent == admitted && e.manifest == manifest_id);
        let prior_quota_screen_clear = carried.is_some_and(|entry| entry.quota_screen_clear);
        // Holds carry only across observations of the same cached agent
        // and manifest. First sight of an occupant therefore starts clear.
        let base_hold = carried.map(|entry| entry.hold).unwrap_or_default();
        let mut turn = carried.and_then(|entry| entry.turn.clone());
        let hold_owner = carried.and_then(|entry| entry.hold_owner.clone());
        let (base_hold, hold_owner, clear_turn, recovery_refusal) =
            crate::composer_recovery::merge_barrier(
                recovery_action.as_ref(),
                retired_attempt,
                base_hold,
                hold_owner,
                detection.turn_running_at().is_some(),
            );
        if clear_turn {
            turn = None;
        }
        // Any unresolved recovered action owns the runtime barrier. It may
        // not fall through the ordinary screen lifecycle: ambiguous restart
        // states require an exact post-recovery start and end, and a failed
        // retirement append must keep both the hold and its end reusable.
        //
        // The one safe transition is the bookkeeping step that ends a turn
        // already running when recovery restored the barrier. That turn
        // cannot consume the payload, so the hold becomes Staged and waits
        // for the next exact start.
        let recovered_hold = recovery_hold_before_durable_retirement(
            recovery_action.as_ref(),
            base_hold,
            &detection,
        );
        // `settle_turn` owns the lane rule. Called here, under both
        // locks, because the advance and the consumption of an exact end
        // are one decision: splitting them leaves a window where another
        // route to this pane sees the hold released while the old key is
        // still pinned, and the next bind is refused as a hijack. Lock
        // order is this function's own, detections then turn ends.
        // An observation that did not answer settles nothing. The
        // binding, the hold, its owner and the turn it waits on are
        // carried forward untouched: none of them were disproved, and
        // recomputing from a screen whose process is unproven is how a
        // barrier gets cleared by a failed `ps`. Runtime state still
        // publishes, so liveness and status keep moving; only the write
        // answer becomes a refusal.
        let frozen = unobservable.then_some(prior_entry).flatten().cloned();
        let (hold, stranded, final_turn, final_owner) = match &frozen {
            Some(entry) => (
                entry.hold,
                false,
                entry.turn.clone(),
                entry.hold_owner.clone(),
            ),
            None if recovered_hold.is_some() => (
                recovered_hold.expect("guarded recovered hold"),
                false,
                turn,
                hold_owner,
            ),
            None => {
                let (hold, stranded) = settle_turn(
                    &mut inner.turn_ends.lock().expect("turn ends lock"),
                    &route,
                    admitted,
                    manifest_id.as_deref(),
                    turn.as_ref(),
                    base_hold,
                    &detection,
                );
                let final_turn = matches!(hold, ComposerHold::TurnStarted { .. })
                    .then_some(turn)
                    .flatten();
                let final_owner = (hold != ComposerHold::Clear)
                    .then_some(hold_owner)
                    .flatten();
                (hold, stranded, final_turn, final_owner)
            }
        };
        // Stamped BEFORE it is cached, because the cache is what the gate
        // and every status surface read. Stamping afterwards would leave
        // them all reading a verdict nobody finished.
        let mut detection = detection.stamped(row.in_mode, hold);
        if unobservable {
            detection = detection.occupant_unprovable();
        } else if stranded {
            detection = detection.refused("turn_evidence_lost");
        }
        if let Some(reason) = recovery_refusal {
            detection = detection.refused(reason);
        }
        // A positive screen baseline is enough to discover durable quota
        // holds after restart. Carry that baseline across title-only and
        // hook-only redraws for the same occupant. A positive quota screen
        // clears it and forces screen capture until reset is observed.
        let prior_quota_screen_clear = frozen
            .as_ref()
            .map(|entry| entry.quota_screen_clear)
            .unwrap_or(prior_quota_screen_clear);
        let quota_screen_clear = if unobservable {
            prior_quota_screen_clear
        } else if positive_quota_reset_observation(&detection) {
            true
        } else if detection.state == AgentState::BlockedQuota
            && detection
                .readings
                .iter()
                .any(|reading| reading.sensor == Sensor::Screen)
        {
            false
        } else {
            prior_quota_screen_clear
        };
        let probe_quota_reset =
            !unobservable && quota_reset_probe_needed(prior_quota_screen_clear, &detection);
        // `since` marks the state CHANGING, so a recompute that confirms
        // the same state carries the old mark forward. Without this the
        // elapsed column would reset on every unrelated event.
        let since = match map.get(&route) {
            Some(e) if e.detection.state == detection.state => e.since,
            _ => std::time::Instant::now(),
        };
        map.insert(
            route,
            DetEntry {
                detection: detection.clone(),
                // A binding is rewritten only by an observation that
                // answered. Overwriting it with the nothing a failed
                // lookup returned would leave the next SUCCESSFUL
                // recompute unable to match, and it would drop the very
                // hold this froze to protect.
                manifest: match &frozen {
                    Some(e) => e.manifest.clone(),
                    None => manifest_id,
                },
                occupant: match &frozen {
                    Some(e) => e.occupant,
                    None => occupant,
                },
                agent: match &frozen {
                    Some(e) => e.agent,
                    None => admitted,
                },
                in_mode: row.in_mode,
                quota_screen_clear,
                hold,
                turn: final_turn,
                // The claim is retired with the barrier it protected: a
                // cleared hold owns nothing, so the next attempt is free
                // to take it.
                hold_owner: final_owner,
                since,
            },
        );
        (prior, prior_ready, detection, probe_quota_reset)
    };
    // A readiness change under an UNCHANGED runtime state is still news
    // for anyone gating on it. The hold lifting is the case that matters:
    // the pane reads idle before and after, so no state edge exists, and
    // a delivery sleeping on `not_write_ready:composer_hold` would sleep
    // through its own release. This wake is broadcast only. It is not a
    // state transition and must never be written to the ledger as one.
    wake_readiness(inner, session_idx, pane_id, prior_ready, &detection);
    if probe_quota_reset {
        crate::delivery::observe_quota_reset(inner, session_idx, pane_id);
    }
    // First sight of a pane that reads Unknown is baseline, not a change.
    let changed = prior != Some(detection.state)
        && !(prior.is_none() && detection.state == AgentState::Unknown);
    if changed {
        debug!(
            pane = pane_id,
            state = %detection.state,
            prior = ?prior,
            cause,
            "fused state changed"
        );
        inner.emit_state(
            session_idx,
            pane_id,
            &detection,
            prior,
            cause,
            (admitted, source_manifest.as_str()),
        );
        // The border says what this row says, from the same edge. No
        // timer, no second rule: an adopted pane's chrome moves exactly
        // when the fused state it names moves.
        crate::repaint_chrome(inner, session_idx, watcher, pane_id).await;
    }
    Some(detection)
}

/// Quota is a screen-only fact. Leaving it requires a fresh, agreeing
/// screen classification, not hook-derived idle or an unknown frame.
fn positive_quota_reset_observation(detection: &Detection) -> bool {
    !detection.stale
        && !detection.disagreement
        && matches!(
            detection.state,
            AgentState::Idle
                | AgentState::IdleWithInput
                | AgentState::Working
                | AgentState::BlockedModal
                | AgentState::BlockedPermission
        )
        && detection.readings.iter().any(|reading| {
            reading.sensor == Sensor::Screen
                && reading.state == detection.state
                && reading.state != AgentState::BlockedQuota
        })
}

/// Recheck the cached exact route after a quota hold is made durable.
/// This closes the race where the positive reset edge lands just before
/// the delivery worker appends `QuotaHeld` and therefore finds no target.
pub(crate) fn quota_reset_observed_now(inner: &Inner, session_idx: usize, pane_id: &str) -> bool {
    inner
        .detections
        .lock()
        .expect("detections lock")
        .get(&PaneKey::new(session_idx, pane_id))
        .is_some_and(|entry| entry.quota_screen_clear)
}

fn quota_reset_probe_needed(prior_screen_clear: bool, current: &Detection) -> bool {
    !prior_screen_clear && positive_quota_reset_observation(current)
}

/// Hold recovered runtime state until its retirement fact is durable.
///
/// The only transition allowed before then ends a turn that was already
/// running when the barrier was restored. That turn cannot consume the staged
/// payload, so recovery waits in `Staged` for the next exact start.
fn recovery_hold_before_durable_retirement(
    action: Option<&crate::composer_recovery::RecoveryAction>,
    hold: ComposerHold,
    detection: &Detection,
) -> Option<ComposerHold> {
    let action = action?;
    Some(
        if matches!(action, crate::composer_recovery::RecoveryAction::Restore(_))
            && hold == ComposerHold::StagedDuringTurn
            && detection.turn_running_at().is_none()
        {
            ComposerHold::Staged
        } else {
            hold
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StdMutex;
    use std::collections::HashMap;
    use std::path::Path;

    fn pane() -> PaneKey {
        PaneKey::new(0, "%1")
    }

    const FIXTURE: &str = r#"
[agent]
id = "bash"
display_name = "Bash fixture"
process_names = ["bash"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']

[[rule]]
id = "screen_busy"
state = "working"
priority = 800
region = "bottom_non_empty_lines(3)"
line_regex = ['^FIXPROMPT']
"#;

    fn manifest() -> Manifest {
        Manifest::parse(FIXTURE, Path::new("bash.toml")).unwrap()
    }

    fn quota_detection(sensor: Sensor, state: AgentState) -> Detection {
        Detection {
            state,
            readings: vec![SensorReading {
                sensor,
                state,
                rule: "quota-probe".into(),
                ts: 7,
            }],
            disagreement: false,
            decided_by: "quota-probe".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
    }

    #[test]
    fn quota_reset_store_probe_runs_once_per_positive_screen_edge() {
        let clean = quota_detection(Sensor::Screen, AgentState::Idle);
        let quota = quota_detection(Sensor::Screen, AgentState::BlockedQuota);
        let hook_only = quota_detection(Sensor::Hook, AgentState::Idle);

        assert!(quota_reset_probe_needed(false, &clean));
        assert!(
            !quota_reset_probe_needed(true, &clean),
            "an identical clean redraw repeated quota store work"
        );
        assert!(quota_reset_probe_needed(false, &clean));
        assert!(!quota_reset_probe_needed(false, &hook_only));
        assert!(!quota_reset_probe_needed(true, &quota));

        let mut stale = clean.clone();
        stale.stale = true;
        assert!(!quota_reset_probe_needed(false, &stale));
        let mut disagreeing = clean.clone();
        disagreeing.disagreement = true;
        assert!(!quota_reset_probe_needed(false, &disagreeing));
    }

    #[test]
    fn unresolved_recovery_never_enters_the_ordinary_screen_lifecycle() {
        let attempt_id = cyclops_proto::NotificationAttemptId::generate();
        let restore = crate::composer_recovery::RecoveryAction::Restore(attempt_id);
        let hold =
            crate::composer_recovery::RecoveryAction::Hold("composer_recovery_retirement_failed");
        let working = Detection {
            state: AgentState::Working,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Working,
                rule: "working".into(),
                ts: 8,
            }],
            disagreement: false,
            decided_by: "working".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        };
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 9,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        };

        assert_eq!(
            recovery_hold_before_durable_retirement(Some(&restore), ComposerHold::Staged, &working,),
            Some(ComposerHold::Staged),
            "a screen-only start cannot bind recovered state"
        );
        assert_eq!(
            recovery_hold_before_durable_retirement(
                Some(&restore),
                ComposerHold::StagedDuringTurn,
                &clean,
            ),
            Some(ComposerHold::Staged),
            "the pre-recovery turn may end without consuming the payload"
        );
        assert_eq!(
            recovery_hold_before_durable_retirement(
                Some(&hold),
                ComposerHold::TurnStarted { since_ms: 8 },
                &clean,
            ),
            Some(ComposerHold::TurnStarted { since_ms: 8 }),
            "a failed append cannot release an exact recovered turn"
        );
        assert_eq!(
            recovery_hold_before_durable_retirement(
                None,
                ComposerHold::TurnStarted { since_ms: 8 },
                &clean,
            ),
            None,
            "durable retirement returns control to ordinary settlement"
        );
    }

    /// An exact end is evidence, and evidence is not spent until it is
    /// used.
    ///
    /// The bug this pins: the end was consumed the moment it existed,
    /// including when the screen still painted the turn as running and
    /// the hold stayed `TurnStarted`. The clean frame that arrived next
    /// then found no matching end, and the barrier never released. The
    /// end has to survive until it actually moves the hold.
    #[test]
    fn an_end_is_kept_until_it_can_release_the_hold() {
        let screen = |state, rule: &str, ts| Detection {
            state,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: rule.into(),
                ts,
            }],
            disagreement: false,
            decided_by: rule.into(),
            stale: false,
            write_ready: false,
            write_block: None,
        };
        let working = screen(AgentState::Working, "spinner", 7);
        let clean = screen(AgentState::Idle, "composer_empty", 9);
        let typed = screen(AgentState::IdleWithInput, "composer_text", 9);

        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["s-1", "t-1"]);
        let armed = || {
            let mut ends = turnkey::Ends::new();
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", turn.clone());
            assert!(turnkey::PaneEnds::pin(
                &mut ends,
                &pane(),
                agent,
                "codex",
                &turn
            ));
            ends
        };
        let held =
            |ends: &turnkey::Ends| turnkey::PaneEnds::holds(ends, &pane(), agent, "codex", &turn);
        let started = ComposerHold::TurnStarted { since_ms: 5 };

        // The end lands while a sensor still reads the turn as running.
        // Nothing has released, so nothing is spent.
        let mut ends = armed();
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            started,
            &working,
        )
        .0;
        assert_eq!(hold, started, "a running turn keeps its hold");
        assert!(held(&ends), "the end that has not released yet is kept");

        // The clean frame that follows releases, and consumes it once.
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            hold,
            &clean,
        )
        .0;
        assert_eq!(
            hold,
            ComposerHold::Clear,
            "an ended turn plus a clean composer releases"
        );
        assert!(!held(&ends), "the end that released is consumed");

        // Text in the composer supersedes the old turn: the hold falls
        // back to Staged and the key must not stay pinned, or the next
        // distinct start is refused as a hijack.
        let mut ends = armed();
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            started,
            &typed,
        )
        .0;
        assert_eq!(hold, ComposerHold::Staged);
        assert!(!held(&ends), "a superseded turn releases its key");

        // A hold with no bound key runs on the screen and never touches
        // the end store, even where the vendor can name its turns.
        let mut ends = armed();
        let hold = settle_turn(
            &mut ends,
            &pane(),
            Some(agent),
            Some("codex"),
            None,
            started,
            &clean,
        )
        .0;
        assert_eq!(
            hold,
            ComposerHold::Clear,
            "the screen lane releases on a clean composer"
        );
        assert!(held(&ends), "the screen lane consumes nothing");
    }

    /// A hold waiting on evidence the store threw away says so.
    ///
    /// An end can arrive before the start it belongs to, and nothing
    /// protects such an end from a flood of later ones. When the delayed
    /// start finally binds, "no end for this turn" no longer means "the
    /// turn has not ended": it may mean the proof is gone. Waiting on it
    /// is waiting forever, so the verdict stops being an ordinary hold
    /// and carries a reason a person can read.
    ///
    /// The release rule is deliberately unchanged. Releasing on absent
    /// evidence is the exact failure this lane exists to prevent.
    #[test]
    fn a_stranded_hold_says_so_instead_of_waiting() {
        let screen = |state, rule: &str, ts| Detection {
            state,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: rule.into(),
                ts,
            }],
            disagreement: false,
            decided_by: rule.into(),
            stale: false,
            write_ready: false,
            write_block: None,
        };
        let clean = screen(AgentState::Idle, "composer_empty", 9);

        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let early = turnkey::TurnKey::for_test(&["s-1", "early"]);
        let started = ComposerHold::TurnStarted { since_ms: 5 };
        let step = |ends: &mut turnkey::Ends, manifest: &str, turn: &turnkey::TurnKey, hold| {
            settle_turn(
                ends,
                &pane(),
                Some(agent),
                Some(manifest),
                Some(turn),
                hold,
                &clean,
            )
        };
        // An end, then more distinct ends than the store can hold. The
        // first one was waiting for a start that had not arrived, so
        // nothing protected it.
        let flooded = || {
            let mut ends = turnkey::Ends::new();
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", early.clone());
            for i in 0..turnkey::ENDS_CAP {
                turnkey::PaneEnds::record(
                    &mut ends,
                    &pane(),
                    agent,
                    "codex",
                    turnkey::TurnKey::for_test(&["s-1", &format!("later{i}")]),
                );
            }
            ends
        };

        // The delayed start binds, the composer reads clean, and the end
        // it is waiting on is not there. The hold stands, and it is
        // stranded rather than merely waiting.
        let mut ends = flooded();
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "codex",
            &early
        ));
        assert_eq!(step(&mut ends, "codex", &early, started), (started, true));

        // An unrelated overflow does not stop a turn whose own end IS
        // present from releasing normally.
        let mut ends = flooded();
        let live = turnkey::TurnKey::for_test(&["s-1", "live"]);
        turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", live.clone());
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "codex",
            &live
        ));
        assert_eq!(
            step(&mut ends, "codex", &live, started),
            (ComposerHold::Clear, false),
            "a present end releases, whatever else the store lost"
        );

        // A different rule set on the same pane is a different binding,
        // and a new binding starts with no history and no doubt about it.
        let mut ends = flooded();
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "claude",
            &early
        ));
        assert_eq!(step(&mut ends, "claude", &early, started), (started, false));
    }

    /// A turn the hold stopped waiting on must not stay pinned.
    ///
    /// The bug this pins: the pin was released only by consuming an END.
    /// When new composer input superseded a turn before that turn's end
    /// arrived, the hold moved to `Staged` and dropped the key, but the
    /// pin stayed. Nothing afterwards knew which key to release, so every
    /// later start was refused as a hijack and the pane never took
    /// another turn.
    #[test]
    fn a_superseded_turn_does_not_stay_pinned() {
        let screen = |state, rule: &str, ts| Detection {
            state,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state,
                rule: rule.into(),
                ts,
            }],
            disagreement: false,
            decided_by: rule.into(),
            stale: false,
            write_ready: false,
            write_block: None,
        };
        let clean = screen(AgentState::Idle, "composer_empty", 9);
        let typed = screen(AgentState::IdleWithInput, "composer_text", 9);

        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let t1 = turnkey::TurnKey::for_test(&["s-1", "t-1"]);
        let t2 = turnkey::TurnKey::for_test(&["s-1", "t-2"]);
        let mut ends = turnkey::Ends::new();
        let started = ComposerHold::TurnStarted { since_ms: 5 };
        // These cases are about the pin, so the stranded flag is dropped
        // here: `a_stranded_hold_says_so_instead_of_waiting` is what
        // covers it.
        let step = |ends: &mut turnkey::Ends, turn: &turnkey::TurnKey, hold, det: &Detection| {
            settle_turn(
                ends,
                &pane(),
                Some(agent),
                Some("codex"),
                Some(turn),
                hold,
                det,
            )
            .0
        };

        // A turn is running and no end has arrived for it.
        assert!(turnkey::PaneEnds::pin(
            &mut ends,
            &pane(),
            agent,
            "codex",
            &t1
        ));

        // Somebody types. The hold stops waiting on t1 without ever
        // seeing t1 end.
        assert_eq!(step(&mut ends, &t1, started, &typed), ComposerHold::Staged);

        // The observable consequence: the next distinct turn can take the
        // pin. Before the fix this refused, permanently.
        assert!(
            turnkey::PaneEnds::pin(&mut ends, &pane(), agent, "codex", &t2),
            "a retired pin leaves the next turn free to take it"
        );

        // t1's end finally arrives. It belongs to a turn nobody waits on
        // and must not release t2.
        turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", t1.clone());
        assert_eq!(
            step(&mut ends, &t2, started, &clean),
            started,
            "another turn's end does not end this one"
        );

        // Only t2's own end does.
        turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", t2.clone());
        assert_eq!(step(&mut ends, &t2, started, &clean), ComposerHold::Clear);
        assert!(!turnkey::PaneEnds::holds(
            &ends,
            &pane(),
            agent,
            "codex",
            &t2
        ));
    }

    /// Binding a turn is what puts a pane on the exact lifecycle, and it
    /// is the delivery holding the barrier that may do it.
    ///
    /// Until a hold carries a key it runs on the screen, where an end
    /// delayed from the previous turn is indistinguishable from this
    /// one's and can release a payload nothing consumed. So the bind has
    /// to reach production, and it has to refuse everything that is not
    /// this delivery's own turn.
    ///
    /// Each refusal starts from an EMPTY end store on purpose. A pin
    /// already held refuses a second key on its own, which would let a
    /// missing owner or turn check pass this test for the wrong reason.
    #[test]
    fn only_the_delivery_holding_the_barrier_binds_its_turn() {
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
        .stamped(false, ComposerHold::Clear);
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let inner = inner_with(BTreeMap::new());
        let t1 = turnkey::TurnKey::for_test(&["s-1", "t-1"]);
        let t2 = turnkey::TurnKey::for_test(&["s-1", "t-2"]);

        let entry = |owner: Option<&str>, hold, turn: Option<&turnkey::TurnKey>| DetEntry {
            detection: clean.clone(),
            manifest: Some("codex".into()),
            occupant: Some(71),
            agent: Some(agent),
            turn: turn.cloned(),
            in_mode: false,
            quota_screen_clear: false,
            hold,
            hold_owner: owner.map(str::to_string),
            since: std::time::Instant::now(),
        };
        // Every case starts from a known pane state AND an empty end
        // store, so each refusal below is the guard under test.
        let start = |e: DetEntry| {
            inner
                .detections
                .lock()
                .expect("detections lock")
                .insert(pane(), e);
            *inner.turn_ends.lock().expect("turn ends lock") = turnkey::Ends::new();
        };
        let bound = || {
            let map = inner.detections.lock().expect("detections lock");
            let e = map.get(&pane()).expect("entry");
            (e.hold, e.turn.clone())
        };
        let free = |t: &turnkey::TurnKey| {
            // The pin is observable through its consequence: a turn that
            // is already pinned refuses a different key.
            let mut ends = inner.turn_ends.lock().expect("turn ends lock");
            turnkey::PaneEnds::pin(&mut ends, &pane(), agent, "codex", t)
        };

        // The delivery holding the barrier binds its own turn, and the
        // hold leaves the screen lifecycle for it.
        start(entry(Some("m-1#1"), ComposerHold::Staged, None));
        assert!(bind_turn(&inner, 0, "%1", "m-1#1", t1.clone(), 500));
        assert_eq!(
            bound(),
            (
                ComposerHold::TurnStarted { since_ms: 500 },
                Some(t1.clone())
            )
        );
        assert!(!free(&t2), "the key was not pinned against eviction");

        // Binding the same turn again is idempotent: an acknowledgement
        // can arrive more than once, and the first witnessed edge stands.
        assert!(bind_turn(&inner, 0, "%1", "m-1#1", t1.clone(), 900));
        assert_eq!(
            bound(),
            (
                ComposerHold::TurnStarted { since_ms: 500 },
                Some(t1.clone())
            )
        );

        // A hold already waiting on one turn is not a second turn's to
        // take, even from the delivery that owns the barrier.
        start(entry(
            Some("m-1#1"),
            ComposerHold::TurnStarted { since_ms: 500 },
            Some(&t1),
        ));
        assert!(!bind_turn(&inner, 0, "%1", "m-1#1", t2.clone(), 900));
        assert_eq!(
            bound(),
            (ComposerHold::TurnStarted { since_ms: 500 }, Some(t1))
        );

        // Another delivery's receipt cannot bind a turn to this barrier.
        // That is the late-acknowledgement shape: the first delivery
        // released, the next claimed the composer, and evidence for the
        // first arrives afterwards.
        start(entry(Some("m-2#1"), ComposerHold::Staged, None));
        assert!(!bind_turn(&inner, 0, "%1", "m-1#1", t2.clone(), 900));
        assert_eq!(bound(), (ComposerHold::Staged, None));

        // An unowned barrier is nobody's to bind.
        start(entry(None, ComposerHold::Staged, None));
        assert!(!bind_turn(&inner, 0, "%1", "m-2#1", t2.clone(), 900));
        assert_eq!(bound(), (ComposerHold::Staged, None));

        // And a pane whose binding cannot be named has nothing to key the
        // end store on.
        let mut unbound = entry(Some("m-2#1"), ComposerHold::Staged, None);
        unbound.agent = None;
        start(unbound);
        assert!(!bind_turn(&inner, 0, "%1", "m-2#1", t2, 900));
        assert_eq!(bound(), (ComposerHold::Staged, None));
    }

    /// The composer barrier is not first-come-first-served.
    ///
    /// The bug this pins: the claim checked only whether SOMEBODY ELSE
    /// owned the barrier. A person typing after the last capture raises
    /// an unowned hold, and an unowned hold read as "free" let the next
    /// delivery take it and paste over their text. A fresh claim needs a
    /// composer this daemon believes is empty AND unclaimed; only the
    /// same owner may re-claim what it already holds.
    #[test]
    fn a_fresh_claim_refuses_a_barrier_it_does_not_own() {
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
        .stamped(false, ComposerHold::Clear);
        let admitted = crate::identity::ProcId {
            pid: 4242,
            birth: 7,
        };
        let agent = Some(admitted);
        let entry = |hold, owner: Option<&str>| DetEntry {
            detection: clean.clone(),
            manifest: Some("bash".into()),
            occupant: Some(4242),
            agent,
            turn: None,
            in_mode: false,
            quota_screen_clear: false,
            hold,
            hold_owner: owner.map(str::to_string),
            since: std::time::Instant::now(),
        };

        let inner = inner_with(BTreeMap::new());
        let put = |e: DetEntry| {
            inner
                .detections
                .lock()
                .expect("detections lock")
                .insert(pane(), e);
        };
        let hold_now = || {
            let map = inner.detections.lock().expect("detections lock");
            let e = map.get(&pane()).expect("entry");
            (e.hold, e.hold_owner.clone())
        };

        // Clear and unowned: the only shape a fresh claim may take.
        put(entry(ComposerHold::Clear, None));
        assert!(claim_hold(&inner, 0, "%1", "m-1#1", agent, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Staged, Some("m-1#1".into())));

        // Same owner, already staged: idempotent.
        assert!(claim_hold(&inner, 0, "%1", "m-1#1", agent, Some("bash")));

        // A different delivery may not take a barrier that is held.
        assert!(!claim_hold(&inner, 0, "%1", "m-2#1", agent, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Staged, Some("m-1#1".into())));

        assert!(!release_unwritten_hold(
            &inner, 0, "%1", "m-2#1", admitted, "bash"
        ));
        assert!(!release_unwritten_hold(
            &inner, 0, "%1", "m-1#1", admitted, "codex"
        ));
        assert!(release_unwritten_hold(
            &inner, 0, "%1", "m-1#1", admitted, "bash"
        ));
        assert_eq!(hold_now(), (ComposerHold::Clear, None));
        assert!(!release_unwritten_hold(
            &inner, 0, "%1", "m-1#1", admitted, "bash"
        ));

        let attempt = "att-00000000-0000-4000-8000-000000000001";
        put(entry(ComposerHold::Staged, Some(attempt)));
        let process = cyclops_proto::ProcessInstanceId::new(admitted.pid, admitted.birth).unwrap();
        assert!(staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));
        assert!(!staged_action_ready(
            &inner,
            0,
            "%1",
            "att-00000000-0000-4000-8000-000000000002",
            process,
            "bash"
        ));

        let mut working = entry(ComposerHold::Staged, Some(attempt));
        working.detection.readings.push(SensorReading {
            sensor: Sensor::Hook,
            state: AgentState::Working,
            rule: "turn_start".into(),
            ts: 2,
        });
        put(working);
        assert!(!staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));

        let mut blocked = entry(ComposerHold::Staged, Some(attempt));
        blocked.detection.state = AgentState::BlockedPermission;
        put(blocked);
        assert!(!staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));

        let mut stale = entry(ComposerHold::Staged, Some(attempt));
        stale.detection.stale = true;
        put(stale);
        assert!(!staged_action_ready(
            &inner, 0, "%1", attempt, process, "bash"
        ));

        put(entry(ComposerHold::Staged, Some(attempt)));
        assert!(!resolve_staged_hold(
            &inner,
            0,
            "%1",
            "att-00000000-0000-4000-8000-000000000002",
            process,
            "bash"
        ));
        assert_eq!(hold_now(), (ComposerHold::Staged, Some(attempt.into())));
        assert!(resolve_staged_hold(
            &inner, 0, "%1", attempt, process, "bash"
        ));
        assert_eq!(hold_now(), (ComposerHold::Clear, None));

        // The race this exists for: a person types between the proof and
        // the write, a recompute records the text, and nobody owns it.
        for hold in [
            ComposerHold::Staged,
            ComposerHold::TurnStarted { since_ms: 9 },
        ] {
            put(entry(hold, None));
            assert!(
                !claim_hold(&inner, 0, "%1", "m-3#1", agent, Some("bash")),
                "an unowned {hold:?} is somebody's text, not a free barrier"
            );
            assert_eq!(hold_now(), (hold, None), "a refused claim changes nothing");
        }

        // A proven binding is still required on top of all of that.
        put(entry(ComposerHold::Clear, None));
        assert!(!claim_hold(&inner, 0, "%1", "m-4#1", agent, Some("codex")));
        assert!(!claim_hold(&inner, 0, "%1", "m-4#1", None, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Clear, None));

        // An unauthenticated pane refuses even when the cache agrees that
        // nobody is home. A pinned manifest chooses rules without proving
        // a process, so two absent identities matching would put a
        // payload into a shell prompt.
        let mut unbound = entry(ComposerHold::Clear, None);
        unbound.agent = None;
        put(unbound);
        assert!(!claim_hold(&inner, 0, "%1", "m-5#1", None, Some("bash")));
        assert_eq!(hold_now(), (ComposerHold::Clear, None));
    }

    /// A failed capture has to leave the same refusal everywhere.
    ///
    /// The bug this pins: the retained verdict was returned to the caller
    /// that asked for it and never written back, so `status` and every
    /// other cache reader kept the pre-failure record, which still said
    /// write_ready. Two consumers, two answers, from one observation
    /// failure.
    #[test]
    fn a_failed_capture_refuses_in_the_cache_too() {
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
        .stamped(false, ComposerHold::Clear);
        assert!(clean.write_ready, "fixture must start write-ready");

        let mut map = std::collections::HashMap::new();
        let since = std::time::Instant::now();
        map.insert(
            pane(),
            DetEntry {
                detection: clean,
                manifest: Some("bash".into()),
                occupant: Some(4242),
                agent: None,
                turn: None,
                in_mode: false,
                quota_screen_clear: false,
                hold: ComposerHold::Clear,
                hold_owner: None,
                since,
            },
        );

        let returned = retain_stale(&mut map, &pane(), false, Some(4242), Some("bash"))
            .expect("same occupant");
        let cached = &map[&pane()].detection;
        for (who, det) in [("returned", &returned), ("cached", cached)] {
            assert!(det.stale, "{who} verdict is not marked stale");
            assert!(!det.write_ready, "{who} verdict still authorizes a write");
            assert_eq!(
                det.write_block.as_deref(),
                Some("stale_screen_evidence"),
                "{who} verdict names the wrong reason"
            );
            assert_eq!(det.state, AgentState::Idle, "{who} state must not move");
        }
        assert_eq!(
            map[&pane()].since,
            since,
            "confidence changed, not the state"
        );
    }

    /// The pane id outlives the agent, so a retained verdict must not.
    ///
    /// Shape: agent A runs in a pane and is observed working. A exits back
    /// to the same shell, agent B starts at the same prompt, and B's first
    /// capture fails. Same pane id, same root pid, possibly the same
    /// manifest. Retaining on pane id alone hands B a turn A was having,
    /// and the stale flag does not fix that: it blocks the write, while
    /// the record still says the wrong agent is working.
    #[test]
    fn a_retained_verdict_never_describes_a_replacement_occupant() {
        let working = Detection {
            state: AgentState::Working,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Working,
                rule: "screen_busy".into(),
                ts: 1,
            }],
            disagreement: false,
            decided_by: "screen_busy".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
        .stamped(false, ComposerHold::Clear);
        let entry_a = || DetEntry {
            detection: working.clone(),
            manifest: Some("agent-a".into()),
            occupant: Some(111),
            agent: None,
            in_mode: false,
            quota_screen_clear: false,
            hold: ComposerHold::Clear,
            hold_owner: None,
            turn: None,
            since: std::time::Instant::now(),
        };

        // Each of these is a different occupant from A's: a new leader, a
        // different manifest, and an unprovable foreground.
        for (case, occupant, manifest) in [
            ("agent B took the prompt", Some(222), Some("agent-a")),
            ("the manifest changed", Some(111), Some("agent-b")),
            ("nobody could prove it", None, Some("agent-a")),
        ] {
            let mut map = std::collections::HashMap::new();
            map.insert(pane(), entry_a());
            assert!(
                retain_stale(&mut map, &pane(), false, occupant, manifest).is_none(),
                "{case}: A's verdict was handed to somebody else"
            );
            // Refusing to retain also has to leave A's record alone
            // rather than half-editing it: the caller's fall-through is
            // what replaces it, with readings taken for whoever is there.
            let cached = &map[&pane()];
            assert_eq!(cached.occupant, Some(111), "{case}");
            assert!(!cached.detection.stale, "{case}: A's record was edited");
        }
    }

    #[test]
    fn binding_is_by_process_name_in_id_order() {
        let mut map = BTreeMap::new();
        map.insert("bash".to_string(), manifest());
        assert_eq!(
            bind_manifest(&map, "bash").map(|m| m.agent.id.as_str()),
            Some("bash")
        );
        assert!(bind_manifest(&map, "vim").is_none());
    }

    #[test]
    fn tier_winners() {
        let m = manifest();
        assert_eq!(
            title_winner(&m, "IDLE ready").map(|r| r.id.as_str()),
            Some("title_idle")
        );
        assert!(title_winner(&m, "mac").is_none());
        assert_eq!(
            screen_winner(&m, "junk\nFIXPROMPT ").map(|r| r.id.as_str()),
            Some("screen_busy")
        );
        assert!(screen_winner(&m, "nothing here").is_none());
    }

    #[test]
    fn disagreement_takes_higher_priority_and_keeps_both_readings() {
        let m = manifest();
        let t = title_winner(&m, "IDLE ready");
        let s = screen_winner(&m, "FIXPROMPT ");
        let d = fuse(&m, t, s, 1);
        assert_eq!(d.state, AgentState::Idle);
        assert_eq!(d.decided_by, "title_idle");
        assert!(d.disagreement);
        assert_eq!(d.readings.len(), 2);
        assert_eq!(d.readings[0].sensor, Sensor::Title);
        assert_eq!(d.readings[0].rule, "title_idle");
        assert_eq!(d.readings[1].sensor, Sensor::Screen);
        assert_eq!(d.readings[1].rule, "screen_busy");
    }

    #[test]
    fn single_tier_is_no_disagreement() {
        let m = manifest();
        let s = screen_winner(&m, "FIXPROMPT ");
        let d = fuse(&m, None, s, 1);
        assert_eq!(d.state, AgentState::Working);
        assert_eq!(d.decided_by, "screen_busy");
        assert!(!d.disagreement);
        assert_eq!(d.readings.len(), 1);
    }

    #[test]
    fn no_rule_is_unknown() {
        let m = manifest();
        let d = fuse(&m, None, None, 1);
        assert_eq!(d.state, AgentState::Unknown);
        assert_eq!(d.decided_by, "no_rule");
        assert!(d.readings.is_empty());
    }

    fn entry(state: AgentState, ts: u64) -> HookEntry {
        HookEntry::bound(
            crate::identity::ProcId { pid: 1, birth: 1 },
            None,
            SensorReading {
                sensor: Sensor::Hook,
                state,
                rule: "Stop".into(),
                ts,
            },
        )
    }

    #[test]
    fn hook_reading_ages_out_on_ttl() {
        let mut e = entry(AgentState::Working, 1_000);
        assert_eq!(
            hook_action(&mut e, AgentState::Unknown, 1_000 + HOOK_READING_TTL_MS),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Unknown, 1_001 + HOOK_READING_TTL_MS),
            HookAction::Drop
        );
    }

    #[test]
    fn hook_reading_invalidated_by_repeated_disagreement() {
        let mut e = entry(AgentState::Working, 1_000);
        // Rules see nothing: no evidence against the hook.
        for _ in 0..10 {
            assert_eq!(
                hook_action(&mut e, AgentState::Unknown, 2_000),
                HookAction::Use
            );
        }
        // Two contradictions survive, the third invalidates.
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Drop
        );
    }

    #[test]
    fn hook_agreement_resets_the_disagreement_streak() {
        let mut e = entry(AgentState::Working, 1_000);
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Working, 2_000),
            HookAction::Use
        );
        assert_eq!(e.disagreements, 0);
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
    }

    // The shipped codex esc rules: dim after the glyph is a ghost
    // suggestion (idle), bare text is typed input (idle_with_input), the
    // plain rule is the idle-biased fallback.
    const ESC_FIXTURE: &str = r#"
[agent]
id = "codex"
display_name = "Codex esc fixture"
process_names = ["codex"]

[[rule]]
id = "composer_typed_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+[^\x1b\s]']

[[rule]]
id = "composer_ghost_suggestion"
state = "idle"
priority = 1040
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+\x1b\[2m']

[[rule]]
id = "composer_empty_or_ghost"
state = "idle"
priority = 1000
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*›']
"#;

    #[test]
    fn screen_winner_esc_discriminates_typed_from_ghost() {
        let m = Manifest::parse(ESC_FIXTURE, Path::new("codex.toml")).unwrap();
        let typed_plain = "› fix the rate limiter in gateway.rs";
        let typed_esc = "\u{1b}[1m›\u{1b}[0m fix the rate limiter in gateway.rs";
        let ghost_plain = "› Find and fix a bug in @filename";
        let ghost_esc = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mFind and fix a bug in @filename\u{1b}[0m";

        // With the escaped capture the esc rules decide.
        let r = screen_winner_esc(&m, typed_plain, Some(typed_esc)).unwrap();
        assert_eq!(r.id, "composer_typed_input");
        assert_eq!(r.state, AgentState::IdleWithInput);
        let r = screen_winner_esc(&m, ghost_plain, Some(ghost_esc)).unwrap();
        assert_eq!(r.id, "composer_ghost_suggestion");
        assert_eq!(r.state, AgentState::Idle);

        // Without one the esc rules fail closed: idle-biased fallback,
        // which is exactly the gap the daemon-side capture closes.
        let r = screen_winner(&m, typed_plain).unwrap();
        assert_eq!(r.id, "composer_empty_or_ghost");
        assert_eq!(r.state, AgentState::Idle);

        assert!(m.has_escaped_rules());
        assert!(!manifest().has_escaped_rules());
    }

    #[test]
    fn argv_basename_parses_ps_args_output() {
        assert_eq!(
            parse_argv_basename("/Users/x/.local/bin/claude --continue\n"),
            Some("claude".into())
        );
        assert_eq!(parse_argv_basename("  cat  \n"), Some("cat".into()));
        assert_eq!(parse_argv_basename("\n"), None);
        assert_eq!(parse_argv_basename(""), None);
    }

    const SLEEP_FIXTURE: &str = r#"
[agent]
id = "sleeper"
display_name = "Sleep fixture"
process_names = []
argv_basenames = ["sleep"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']
"#;

    #[test]
    fn a_recovered_exact_end_is_durable_before_runtime_clearance() {
        use crate::mailbox::{MailboxDirectory, MailboxIdentity, MailboxSend, MessageStore};
        use crate::notification_adapter::NotificationContext;
        use cyclops_proto::{
            NotificationAttentionCause, NotificationBinding, NotificationManifestId,
            NotificationTransport, ProcessInstanceId, RecipientKey, SessionInstanceId, TmuxPaneId,
        };

        let mut inner = inner_with(BTreeMap::new());
        let session = "00000000-0000-4000-8000-000000000002"
            .parse::<SessionInstanceId>()
            .unwrap();
        let tmux_pane = "%1".parse::<TmuxPaneId>().unwrap();
        let recipient = RecipientKey::agent(inner.workspace_id, session, tmux_pane);
        let directory = MailboxDirectory::new(
            inner.workspace_id,
            [MailboxIdentity {
                key: recipient,
                label: "codex".into(),
            }],
        )
        .unwrap();
        let store = MessageStore::open(
            &inner.state_root,
            Path::new("workspaces/recovery/messages.ndjson"),
            inner.workspace_id,
            "boot",
        )
        .unwrap();
        let service = Arc::new(crate::mailbox::MailboxService::new(directory, store));
        let accepted = service
            .send(
                service.admin(),
                MailboxSend {
                    addresses: vec!["codex".into()],
                    subject: "recover".into(),
                    body: "body".into(),
                    fyi: false,
                    client_key: None,
                    supersedes: None,
                },
            )
            .unwrap();
        let queued = service
            .prepare_oldest_notification(recipient)
            .unwrap()
            .unwrap();
        let context = NotificationContext::new(
            service.store_handle(),
            accepted.message_id,
            recipient,
            queued.attempt_id,
        );
        context.record_gating().unwrap();
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        context
            .record_writing(
                ProcessInstanceId::new(70, 2).unwrap(),
                ProcessInstanceId::new(agent.pid, agent.birth).unwrap(),
                "codex",
                NotificationTransport::Doorbell,
                None,
            )
            .unwrap();
        context
            .record_attention(NotificationAttentionCause::VerifyFailed)
            .unwrap();
        Arc::get_mut(&mut inner).unwrap().mailbox = Some(Arc::clone(&service));
        *inner.composer_recovery.lock().unwrap() =
            crate::composer_recovery::RecoveryCoordinator::new([queued.attempt_id]);

        let turn = turnkey::TurnKey::for_test(&["session", "turn"]);
        let clean = Detection {
            state: AgentState::Idle,
            readings: vec![SensorReading {
                sensor: Sensor::Screen,
                state: AgentState::Idle,
                rule: "composer_empty".into(),
                ts: 9,
            }],
            disagreement: false,
            decided_by: "composer_empty".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        };
        inner.detections.lock().unwrap().insert(
            pane(),
            DetEntry {
                detection: clean.clone(),
                manifest: Some("codex".into()),
                occupant: Some(70),
                agent: Some(agent),
                in_mode: false,
                quota_screen_clear: false,
                hold: ComposerHold::TurnStarted { since_ms: 8 },
                turn: Some(turn.clone()),
                hold_owner: Some(queued.attempt_id.to_string()),
                since: std::time::Instant::now(),
            },
        );
        {
            let mut ends = inner.turn_ends.lock().unwrap();
            assert!(turnkey::PaneEnds::pin(
                &mut ends,
                &pane(),
                agent,
                "codex",
                &turn
            ));
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", turn.clone());
        }
        let live = NotificationBinding {
            recipient,
            leader: Some(ProcessInstanceId::new(70, 2).unwrap()),
            agent: ProcessInstanceId::new(agent.pid, agent.birth).unwrap(),
            manifest: NotificationManifestId::new("codex").unwrap(),
        };

        assert_eq!(
            crate::composer_recovery::retire_exact_lifecycle(&inner, 0, "%1", Some(&live), true,),
            crate::composer_recovery::LifecycleRetirement::Durable(queued.attempt_id)
        );
        assert!(service.active_notification_barriers().unwrap().is_empty());
        assert!(turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane(),
            agent,
            "codex",
            &turn
        ));

        let (hold, stranded) = settle_turn(
            &mut inner.turn_ends.lock().unwrap(),
            &pane(),
            Some(agent),
            Some("codex"),
            Some(&turn),
            ComposerHold::TurnStarted { since_ms: 8 },
            &clean,
        );
        assert_eq!(hold, ComposerHold::Clear);
        assert!(!stranded);
        assert!(!turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane(),
            agent,
            "codex",
            &turn
        ));
    }

    #[test]
    fn a_failed_recovery_retirement_keeps_the_exact_end_and_hold() {
        let inner = inner_with(BTreeMap::new());
        let attempt_id = cyclops_proto::NotificationAttemptId::generate();
        *inner.composer_recovery.lock().unwrap() =
            crate::composer_recovery::RecoveryCoordinator::new([attempt_id]);
        let agent = crate::identity::ProcId { pid: 71, birth: 3 };
        let turn = turnkey::TurnKey::for_test(&["session", "turn"]);
        inner.detections.lock().unwrap().insert(
            pane(),
            DetEntry {
                detection: Detection {
                    state: AgentState::Idle,
                    readings: Vec::new(),
                    disagreement: false,
                    decided_by: "fixture".into(),
                    stale: false,
                    write_ready: false,
                    write_block: None,
                },
                manifest: Some("codex".into()),
                occupant: Some(70),
                agent: Some(agent),
                in_mode: false,
                quota_screen_clear: false,
                hold: ComposerHold::TurnStarted { since_ms: 8 },
                turn: Some(turn.clone()),
                hold_owner: Some(attempt_id.to_string()),
                since: std::time::Instant::now(),
            },
        );
        {
            let mut ends = inner.turn_ends.lock().unwrap();
            assert!(turnkey::PaneEnds::pin(
                &mut ends,
                &pane(),
                agent,
                "codex",
                &turn
            ));
            turnkey::PaneEnds::record(&mut ends, &pane(), agent, "codex", turn.clone());
        }
        let recipient = RecipientKey::agent(
            inner.workspace_id,
            "00000000-0000-4000-8000-000000000002".parse().unwrap(),
            "%1".parse().unwrap(),
        );
        let live = cyclops_proto::NotificationBinding {
            recipient,
            leader: Some(cyclops_proto::ProcessInstanceId::new(70, 2).unwrap()),
            agent: cyclops_proto::ProcessInstanceId::new(agent.pid, agent.birth).unwrap(),
            manifest: cyclops_proto::NotificationManifestId::new("codex").unwrap(),
        };

        assert_eq!(
            crate::composer_recovery::retire_exact_lifecycle(&inner, 0, "%1", Some(&live), true,),
            crate::composer_recovery::LifecycleRetirement::Blocked(
                "composer_recovery_store_unavailable"
            )
        );
        let entry = inner
            .detections
            .lock()
            .unwrap()
            .get(&pane())
            .unwrap()
            .clone();
        assert_eq!(entry.hold, ComposerHold::TurnStarted { since_ms: 8 });
        assert_eq!(entry.turn.as_ref(), Some(&turn));
        assert!(turnkey::PaneEnds::holds(
            &inner.turn_ends.lock().unwrap(),
            &pane(),
            agent,
            "codex",
            &turn
        ));
    }

    fn inner_with(manifests: BTreeMap<String, Manifest>) -> Arc<Inner> {
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-argv-cache-{}",
            uuid::Uuid::new_v4()
        ));
        let state_root = Arc::new(cyclops_state::StateRoot::open_or_create(&home).unwrap());
        let (registry, _) = crate::registry::Registry::load(Arc::clone(&state_root));
        let workspace_id = crate::workspaceid::load_or_create(&state_root).unwrap();
        let session_identities = crate::sessionstore::SessionIdentities::open(&state_root).unwrap();
        Arc::new(Inner {
            cfg: crate::Config::defaults(&home),
            state_root,
            state_repair: cyclops_state::RepairSummary::default(),
            workspace_id,
            session_identities: StdMutex::new(session_identities),
            mailbox: None,
            composer_recovery: StdMutex::new(
                crate::composer_recovery::RecoveryCoordinator::default(),
            ),
            mailbox_publication: StdMutex::new(()),
            mailbox_publish_pause: StdMutex::new(None),
            boot_id: "b-test".into(),
            started: std::time::Instant::now(),
            tmux_version: "3.6a".into(),
            manifests,
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            events: tokio::sync::broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::new()),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
            turn_ends: StdMutex::new(crate::turnkey::Ends::new()),
            argv_cache: StdMutex::new(HashMap::new()),
            engine: crate::delivery::Engine::new(),
            ack_state: crate::ack::AckState::new(),
            hook_liveness: crate::selftest::HookLiveness::new(),
            inject_pause: StdMutex::new(None),
            fail_chrome_restore: std::sync::atomic::AtomicBool::new(false),
            workspace_ui: StdMutex::new(crate::workspace_ui::WorkspaceUiState::default()),
            stop: tokio::sync::watch::channel(false).1,
            extra_tasks: StdMutex::new(Vec::new()),
        })
    }

    #[test]
    fn exact_route_detection_cache_separates_duplicate_pane_ids() {
        let inner = inner_with(BTreeMap::new());
        let entry = |state| DetEntry {
            detection: Detection {
                state,
                readings: Vec::new(),
                disagreement: false,
                decided_by: "test".into(),
                stale: false,
                write_ready: false,
                write_block: None,
            },
            manifest: None,
            occupant: None,
            agent: None,
            in_mode: false,
            quota_screen_clear: false,
            hold: ComposerHold::Clear,
            turn: None,
            hold_owner: None,
            since: std::time::Instant::now(),
        };
        inner
            .detections
            .lock()
            .unwrap()
            .insert(PaneKey::new(0, "%1"), entry(AgentState::Idle));
        inner
            .detections
            .lock()
            .unwrap()
            .insert(PaneKey::new(1, "%1"), entry(AgentState::Working));

        assert_eq!(inner.cached_state(0, "%1"), AgentState::Idle);
        assert_eq!(inner.cached_state(1, "%1"), AgentState::Working);
    }

    /// A wrapper caught before its `exec` reads as the interpreter and binds
    /// nothing. Remembering that would pin the pane unknown for the life of
    /// the process, because the exec keeps the pid the cache is keyed on.
    #[test]
    fn a_basename_that_binds_nothing_is_re_probed_not_memoised() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        // No manifest claims "sleep": the reading is a miss, and a miss must
        // leave the cache empty so the next recompute probes again.
        let blind = inner_with(BTreeMap::new());
        assert!(argv_bound_manifest(&blind, 0, "%0", pid).is_none());
        assert!(
            blind.argv_cache.lock().unwrap().is_empty(),
            "a non-binding basename must not be memoised"
        );

        // The same pid, once a manifest claims it, binds and is remembered.
        let mut map = BTreeMap::new();
        map.insert(
            "sleeper".to_string(),
            Manifest::parse(SLEEP_FIXTURE, Path::new("sleeper.toml")).unwrap(),
        );
        let bound = inner_with(map);
        assert_eq!(
            argv_bound_manifest(&bound, 0, "%0", pid).map(|(m, _)| m.agent.id.as_str()),
            Some("sleeper")
        );
        let proc = crate::identity::ProcId::of(pid).expect("the child is alive");
        assert_eq!(
            bound
                .argv_cache
                .lock()
                .unwrap()
                .get(&(PaneKey::new(0, "%0"), proc)),
            Some(&"sleep".to_string()),
            "a binding basename is memoised for the pane"
        );

        // The SAME pid with a different birth is a different process, and
        // it reads nothing. That is the pid-reuse case: the number can be
        // handed to anything, and a cache that answered on the number
        // alone would hand the newcomer this agent's identity.
        let impostor = crate::identity::ProcId {
            pid,
            birth: proc.birth + 1,
        };
        assert_eq!(
            bound
                .argv_cache
                .lock()
                .unwrap()
                .get(&(PaneKey::new(0, "%0"), impostor)),
            None,
            "a reused pid inherited a binding it never earned"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A cached positive binding is not authentication evidence.
    ///
    /// pid and birth both survive an in-place `exec`, so a process can
    /// bind as a vendor, exec into something that is not one, and keep
    /// the identity it was admitted under. Cursor's launcher execs in
    /// place, so this is the supported process model, not an edge case.
    /// The authentication path therefore reads argv live every time; only
    /// the manifest-binding path may reuse a cached answer.
    #[test]
    fn a_cached_binding_never_authenticates_after_an_exec() {
        let mut map = BTreeMap::new();
        map.insert(
            "sleeper".to_string(),
            Manifest::parse(SLEEP_FIXTURE, Path::new("sleeper.toml")).unwrap(),
        );
        let inner = inner_with(map);
        let steady = |pid: i32| Some(crate::identity::ProcId { pid, birth: 7 });

        // Seed a positive cache for this exact identity.
        assert!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("sleep".to_string()), steady).is_some(),
            "fixture: the binding has to be cached first"
        );
        assert!(!inner.argv_cache.lock().unwrap().is_empty());

        // Same process, same identity, different program. The cached path
        // still answers from what it remembered, which is exactly why it
        // must not be the authentication route.
        assert!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("bash".to_string()), steady).is_some(),
            "fixture: the cache is expected to answer stale here"
        );
        // The live route is asked about the SAME cached identity, with the
        // post-exec argv. If it ever consulted the cache it would answer
        // "sleeper" here and this would fail.
        let us = unsafe { libc::getuid() };
        let ours = |pid: i32| Some((crate::identity::ProcId { pid, birth: 7 }, us));
        assert!(
            matches!(
                vendor_read(&inner, 4242, |_| Some("bash".to_string()), ours),
                VendorRead::NotVendor
            ),
            "authentication answered from a cached pre-exec binding"
        );
        // And it still binds when the live argv really is the vendor, so
        // the refusal above is the exec and not the plumbing.
        assert!(matches!(
            vendor_read(&inner, 4242, |_| Some("sleep".to_string()), ours),
            VendorRead::Vendor(_, _)
        ));

        // One definition, two callers. A process owned by somebody else
        // is not a vendor of ours whichever route asks, and one nobody
        // could read is doubt rather than a no.
        let theirs = |pid: i32| Some((crate::identity::ProcId { pid, birth: 7 }, us + 1));
        assert!(matches!(
            vendor_read(&inner, 4242, |_| Some("sleep".to_string()), theirs),
            VendorRead::NotVendor
        ));
        for unreadable in [
            vendor_read(&inner, 4242, |_| Some("sleep".to_string()), |_| None),
            vendor_read(&inner, 4242, |_| None, ours),
        ] {
            assert!(matches!(unreadable, VendorRead::Unprovable));
        }

        // The owner is proven ACROSS the argv read, not before it. Both
        // halves come from one observation, and both have to still hold
        // afterwards: credentials can change without the start time
        // moving, and a number can be handed on between two reads.
        let reads = std::sync::atomic::AtomicU64::new(0);
        let owner_changes = |pid: i32| {
            let n = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some((
                crate::identity::ProcId { pid, birth: 7 },
                if n == 0 { us } else { us + 1 },
            ))
        };
        assert!(
            matches!(
                vendor_read(&inner, 4242, |_| Some("sleep".to_string()), owner_changes),
                VendorRead::Unprovable
            ),
            "the owner changed under the probe and it bound anyway"
        );

        let swaps = std::sync::atomic::AtomicU64::new(0);
        let process_changes = |pid: i32| {
            let n = swaps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some((
                crate::identity::ProcId {
                    pid,
                    birth: if n == 0 { 7 } else { 8 },
                },
                us,
            ))
        };
        assert!(
            matches!(
                vendor_read(&inner, 4242, |_| Some("sleep".to_string()), process_changes),
                VendorRead::Unprovable
            ),
            "the process changed under the probe and it bound anyway"
        );
    }

    /// The identity read and the argv read are two observations of a
    /// system that does not hold still, and a pid can change hands
    /// between them.
    ///
    /// Injected rather than raced, because the OS will not reuse a pid on
    /// demand: the argv probe swaps the identity underneath as its side
    /// effect, which is exactly the interleaving. The classification has
    /// to be refused, and nothing may be written down: filing it would
    /// authorize the replacement under the predecessor's identity, and a
    /// cache-hit test cannot see that at all.
    #[test]
    fn a_pid_reused_between_the_two_reads_binds_nothing() {
        let mut map = BTreeMap::new();
        map.insert(
            "sleeper".to_string(),
            Manifest::parse(SLEEP_FIXTURE, Path::new("sleeper.toml")).unwrap(),
        );
        let inner = inner_with(map);

        let reads = std::sync::atomic::AtomicU64::new(0);
        let ident = |pid: i32| {
            // First read one process, every read after it another.
            let n = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(crate::identity::ProcId {
                pid,
                birth: if n == 0 { 100 } else { 200 },
            })
        };
        assert!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("sleep".to_string()), ident).is_none(),
            "a replacement process was classified as its predecessor"
        );
        assert!(
            inner.argv_cache.lock().unwrap().is_empty(),
            "a binding nobody could prove was memoised anyway"
        );

        // The same inputs with a stable identity do bind, so the refusal
        // above is the interleaving and not the fixture.
        let steady = |pid: i32| Some(crate::identity::ProcId { pid, birth: 100 });
        assert_eq!(
            argv_bound_with(&inner, 0, "%0", 4242, |_| Some("sleep".to_string()), steady)
                .map(|(m, _)| m.agent.id.as_str()),
            Some("sleeper")
        );
    }

    #[test]
    fn basename_binding_matches_either_declared_name() {
        let mut map = BTreeMap::new();
        map.insert("bash".to_string(), manifest());
        // process_names
        assert_eq!(
            manifest_for_basename(&map, "bash").map(|m| m.agent.id.as_str()),
            Some("bash")
        );
        // the wrapper's pre-exec interpreter is not a claim on the pane
        assert!(manifest_for_basename(&map, "node").is_none());
        assert!(manifest_for_basename(&BTreeMap::new(), "bash").is_none());
    }

    /// MEASURED 2026-08-06 (Claude Code 2.1.221, tmux 3.6a, live rig): a
    /// pane running a native claude read pane_current_command "2.1.221"
    /// (version symlink, F21), `ps -o args=` on pane_pid "-zsh", and
    /// tpgid " 19989\n" whose args were "claude". Pins every hop of the
    /// binding chain against the shipped manifests on that data alone.
    #[test]
    fn measured_claude_binding_triple_2_1_221() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
        let shipped: BTreeMap<String, Manifest> = cyclops_manifest::load_dir(&dir)
            .unwrap()
            .into_iter()
            .collect();

        // Comm route: nothing claims the bare version string.
        assert!(bind_manifest(&shipped, "2.1.221").is_none());

        // pane_pid's argv is the login shell and binds nothing.
        let shell = parse_argv_basename("-zsh\n").unwrap();
        assert_eq!(shell, "-zsh");
        assert!(manifest_for_basename(&shipped, &shell).is_none());

        // The measured tpgid line resolves to the foreground group leader.
        assert_eq!(parse_tpgid(" 19989\n"), Some(19989));

        // That leader's argv is what binds the claude manifest.
        let agent = parse_argv_basename("claude\n").unwrap();
        assert_eq!(
            manifest_for_basename(&shipped, &agent).map(|m| m.agent.id.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn tpgid_parses_ps_output_and_rejects_no_terminal() {
        assert_eq!(parse_tpgid("  6254\n"), Some(6254));
        // A pane with no controlling terminal: -1 names no process, and 0
        // is not a pid either. Both must fall back to pane_pid rather than
        // send `ps -p` after something that cannot exist.
        assert_eq!(parse_tpgid("   -1\n"), None);
        assert_eq!(parse_tpgid("0\n"), None);
        assert_eq!(parse_tpgid("\n"), None);
        assert_eq!(parse_tpgid("not a pid\n"), None);
    }
}
