# Stabilization history

This document records the stabilization run from PR 56 through PR 95. It is a
technical history, not a list of release claims. A merged fix, a fixture proof,
a live vendor observation, and an opt-in evidence component are different kinds
of evidence and are named separately below.

The run began with messaging that was durable but still coupled too tightly to
terminal wake, UI projections that exposed or obscured the wrong information,
and integration tests whose timing could hide the actual boundary under test.
It ended at `0492d7a` with a matched CLI and daemon, body-free notification,
exact claims, stable identity across rename and restart, guarded composer
writes, deterministic crash-boundary tests, and a live two-agent proof of the
visible-input contract.

## What changed

### Durable messaging became independent of terminal wake

The most consequential change was separating acceptance from notification.
`cyclops send` now succeeds when the canonical mailbox facts have been appended
and synced. A blocked or unavailable pane cannot revoke that acceptance.
Terminal notification is asynchronous and content-free. Callers that truly
need the stronger wake boundary opt into `--require-wake`.

The recipient claims an exact `m-att_...` attempt before the daemon releases the
authorized message envelope. Replies route on durable recipient identity, not
mutable display labels. Shared Stream events and resting rows carry metadata,
not bodies. The authorized thread and claim paths remain the only readers of
message content.

Rejected alternatives included treating an injected string as receipt,
returning success only after tmux decoration, copying bodies into broadcast
events, and automatically falling back to raw `tmux send-keys`. Each would have
made a fast or visible path more authoritative than the durable record.

### Composer safety became its own state axis

Agent activity answers what the human sees: idle, working, blocked, or failed.
Composer evidence answers whether Cyclops may write: clean, withInput, or
ambiguous. A working pane with a proven clean composer can accept a wake. An idle
pane with human input cannot.

Human input is now a temporal hold, not a one-frame classification. Hiding a
draft does not release it. Transient process-identity loss carries the hold only
for the same live occupant, never across a replacement generation. The final
stabilization defect was the opposite edge: a user could visibly erase every
character but the unowned human hold remained forever. PR 95 releases only that
hold after a settled, exact empty-composer observation, then sends the same
attempt back through the ordinary gate. Partial deletion, stale or ambiguous
frames, modals, replacement occupants, notification-owned holds, and recovery
barriers remain blocked.

Rejected alternatives included a fixed timeout, two generic blank frames, a
pane-wide unblock, and treating idle as permission. Those mechanisms cannot
distinguish genuine deletion from a transient redraw or hidden input.

### Identity, recovery, and lifecycle became durable boundaries

Session names and process IDs are observations, not identity. Stable tmux
session IDs, process birth information, durable endpoint keys, and exact
notification attempt IDs now survive rename, replacement, restart, and replay.
Missing lifecycle ends emit one bounded diagnostic only after positive visual
contradiction. Pre-write crashes roll back the unwritten in-memory hold.
Post-write uncertainty faults visibly and does not silently restart or paste a
second copy.

Queue and worker tests now prove exact ownership under their mutexes, atomic
multi-alarm facts, one-shot reopens, per-recipient FIFO, and independent mailbox
and legacy worker registries. Cleanup and update operations revalidate held
descriptor identity and record content-free facts instead of trusting a
re-resolved path.

### The UI became a projection instead of a second authority

Messages is a bordered peer pane rather than an overlay. It hydrates authorized
thread bodies from the daemon, suppresses reply subjects from the durable
`reply_to` fact, collapses transport status to one line, and decays attention
from recipient wake age in both normal and narrow layouts. Stream rows are
body-free by type.

Workspace sizing now distinguishes the authoritative owner from followers,
deliberate minimization from resize compression, and local Messages space from
shared tmux geometry. Session rename recovery follows stable identity rather
than creating an old-name ghost. Slack-first Messages sizing consumes existing
right-side slack before shrinking an agent card and installs a fresh post-resize
model before returning.

### Tests became event-driven and CI work became narrower

Many failures initially described as load were phase-order races. Tests waited
on command echoes, fixed sleeps, shared retry channels, pending-count
projections, or screen polling while production was still between states.
Stabilization replaced those waits with exact attempt latches, durable Writing
and Staged transitions, watcher events, bounded field publication, and explicit
pre-tmux boundaries. Mutation checks proved that the new gates fail when their
production boundary is removed.

CI installer jobs stopped rebuilding the workspace and repeating the full
parity walk. Source scans and paste parsing became linear. Two proposed speedups
were intentionally not merged: reduced debug information lacked sufficient
causal hosted evidence, and bounded nextest concurrency was slower under the
real process-isolation workload.

## Pull request ledger

| PR | Outcome |
|---|---|
| 56 | Replaced coarse working-pane refusal with fresh clean-composer proof. |
| 57 | Ran the local gate under Bash so its own process name no longer matched a fixture agent. |
| 58 | Added authorized thread bodies, reply-aware subjects, compact held rows, and wake-age attention decay. |
| 59 | Removed message bodies from shared push events and resting Stream rows at the type boundary. |
| 60 | Added offline operational truth, descriptor-relative cleanup, rollback attestation, and per-field revalidation tests. |
| 61 | Added an opt-in frozen transport benchmark with bounded pipe draining and honest build identity. |
| 62 | Proved detach and reattach, rename reply routing, exact lifecycle ends, and bounded missing-end diagnostics. |
| 63 | Decoupled durable mailbox acceptance from terminal wake and best-effort unread chrome. |
| 64 | Made a successful claim return a self-addressing, authorized envelope. |
| 65 | Added `--require-wake` without resending an already accepted message or overclaiming receipt. |
| 66 | Made Messages a local peer pane rather than a session-wide geometry mutation. |
| 67 | Added Gate 3 proofs for atomic clearance, exact workers, recovery faults, and write-boundary crashes. |
| 68 | Added owner transfer, pinned-window sizing, post-resize snapshots, and minimization provenance. |
| 69 | Formally retired the commPact v1 shim, installer, runbook, tests, CI stages, and active documentation. |
| 70 | Added one exact-attempt, body-free unclaimed reminder with replay and obsolescence bounds. |
| 71 | Reconciled the authoritative pane table before name fallback to prevent `no_such_target`. |
| 72 | Added installer-only parity mode while preserving the real installer lifecycle and relocated-root gate. |
| 73 | Closed without merge after the reduced-debug CI experiment lacked sufficient hosted causal evidence. |
| 74 | Latched visible human input so a hidden draft could not be mistaken for a manual clear. |
| 75 | Removed hydration marker command-echo races by waiting for actual output. |
| 76 | Recorded the frozen roadmap authority and baseline in the repository. |
| 77 | Waited for asynchronous tmux current-directory publication instead of reading it immediately. |
| 78 | Waited for the exact visible working doorbell rather than capturing immediately after Writing. |
| 79 | Added an opt-in Gate 7 stage-and-clear component with explicit limitations, not full certification. |
| 80 | Made the one-place architecture source scan linear. |
| 81 | Made bracketed-paste inspection single-pass while preserving terminator overlap. |
| 82 | Closed without merge after bounded nextest concurrency was slower in hosted process-isolated runs. |
| 83 | Isolated retry phases and tolerated the explicit incomplete-status intermediate state. |
| 84 | Made unread-worker shutdown prove the pre-tmux boundary before asserting the join. |
| 85 | Paused externally rate-limited Vercel Git deployment status while retaining repository-owned website checks. |
| 86 | Made naming fallback and claim-ordering CI tests force their intended production boundaries. |
| 87 | Aligned release schemas, exact Format 3 wake and claim latches, build stamps, and deadlock diagnostics. |
| 90 | Added slack-first Messages sizing and fresh owner reconcile convergence. |
| 91 | Recovered renamed sessions by stable tmux identity across daemon restart. |
| 92 | Replaced modal-clear screen polling with exact pre-write, Writing, and Staged latches. |
| 93 | Preserved composer holds across transient identity loss but never across a replacement generation. |
| 94 | Added narrow AGY 1.1.22 empty-Context and truecolor trailer evidence. |
| 95 | Released the same held attempt after settled visible input became exactly empty. |

PR 88 and PR 89 do not exist. PR 73 and PR 82 were measured negative
experiments and were closed without merge. That distinction matters: the
history includes what was rejected so those ideas do not return as folklore.

## Final evidence at `0492d7a`

The merged PR 95 head passed the complete fast repository gate: formatting,
strict clippy, documentation paths, 1,848 nextest tests, the daemon integration
suites, and doctests.

A matched `cyclops` and `cyclopsd` pair built from `0492d7a` was installed and
restarted through the authenticated daemon command. A fresh Codex 0.150.1 pane
and AGY 1.1.22 pane were discovered. The live two-agent exercise proved:

- durable send and Format 3 wake;
- exact AGY claim and reply;
- visible human input holding a second notification;
- twelve of thirteen backspaces preserving the same held attempt;
- the final backspace releasing that same attempt through Writing, Staged,
  Submitted, and Notified;
- exact claim and reply after release.

The opt-in frozen transport component also passed on the exact SHA. Across 20
samples it measured separate CLI startup, socket RPC, durable acceptance,
notification, and claim distributions. These are local measurements, not a
universal performance guarantee.

The Gate 7 stage-and-clear component executed honestly but returned
`Limitation`, not `Passed`: vendor authentication was not provisioned for its
Codex and Claude subprocess trials, AGY clear keys are manual-only, and Cursor
was absent. Claude was not available for the final live exercise. The direct
Codex and AGY acceptance run above is real live evidence, but it does not turn
unexecuted vendor cells into passes.

## What remains

The current product behavior is stable enough to become the baseline for
future work. The highest-priority follow-up is the behavior-preserving
delivery-core extraction in [NEXT.md](NEXT.md). Large-fleet UI, Cursor live
coverage, raw-tmux fallback polish, and a generic administrator hold override
are deferred and are not hidden release blockers.
