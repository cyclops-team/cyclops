# Cyclops beta final acceptance audit

**Status:** Technical beta-acceptance decision complete for the exact
functional candidate reviewed here, not release authorization. A public release
remains an operator decision.

**Functional candidate:** `2c88b823c5ff25a0094a3006f409b0d73de52d86`

**Release-evidence run:**
[33475085967](https://github.com/cyclops-team/cyclops/actions/runs/33475085967),
completed successfully at 2026-09-01T06:01:40Z

This is the current final cross-track acceptance record for the Cyclops Beta
Rework. It supersedes the earlier audit of
`b5f128fa70c67ce4bf9609b188643b71c44d236b` and records the post-audit delta
merged in pull requests #188 through #194: final-audit documentation,
installer lifecycle fixes, force-submit ordering fixes, session-recreation
handling, and full-gate documentation. An independent delta review found no
product, compatibility, or safety blocker. The behavioral contracts remain the
authority when this audit and a contract differ.

The independent architecture, regression and migration, user-journey and
performance, and standards audits identified no unresolved implementation
blocker. This decision does not authorize a merge to `main`, a tag, a version
number, a GitHub Release, or publication. Those remain operator decisions after
release identity is reconciled.

## Decision

The beta has demonstrated the intended product shape:

- durable messaging works without either UI;
- official clients share bounded framing and honest uncertainty semantics;
- notification and activation remain optional;
- readable journal formats remain readable;
- messaging policy is local to `WorkspaceMessaging`, while observation and
  presentation remain separate; and
- required pull-request, scheduled, and release evidence are distinct and
  inspectable.

The remaining gate is release authority, not an unfinished implementation
track. The workspace still declares Cargo version `0.1.0`; the newest remote
tag is `v0.2.0-beta` from 2026-08-27, which peels to historical ancestor
`1155a50ce1db9256b114b1f89d203935324ceb52`; and GitHub has no Release object.
Do not name or publish a beta until an operator reconciles those facts.

## Evidence reviewed

| Area | Evidence | Result |
|---|---|---|
| Clean checkout | Linux and macOS full repository gates in release run 33475085967 | Passed |
| Compatibility | Strict and lenient replay plus daemon historical-journal contracts | Passed |
| Journeys | Linux and macOS installer lifecycle and real user journeys | Passed |
| Reliability | Repeated race, cleanup, soak, and long-history evidence | Passed |
| tmux | Full correctness evidence against tmux HEAD | Passed |
| Performance | Retained `release-performance-2c88b823c5ff25a0094a3006f409b0d73de52d86-1` artifact | Passed with recorded environment and workload metadata |
| Aggregate | `beta release evidence complete` | Passed |

The full release run was a push of the exact candidate SHA to a disposable
beta trigger branch. It completed successfully at 2026-09-01T06:01:40Z and
did not merge, tag, or publish anything.

The performance artifact records the clean commit, Ubuntu 24, x86-64, four
CPUs, Rust 1.98.0, tmux 3.4, and seven bounded workloads. Its results are
evidence for that environment and workload set, not a universal latency or
memory claim. The earlier same-machine comparison retained in
[MESSAGING_BETA_AUDIT.md](MESSAGING_BETA_AUDIT.md) is likewise historical
evidence, not a claim about every host.

## User-journey acceptance

| Journey | Acceptance result |
|---|---|
| First durable handoff | Installer lifecycle reaches durable acceptance and authenticated claim on Linux and macOS. |
| Headless coordination | Send, claim, reply, status, history, and recovery work without the stream or workspace UI. |
| Everyday workspace use | Focus, layout, panes, Messages, Stream, files, and collapsed messaging cues use named interfaces and current daemon projections. |
| Pane-only and hidden-sidebar use | The body-free tmux border count and intentionally chrome-free manual inbox inspection remain available. |
| Failure and recovery | Daemon, tmux, hook, partial-write, and verification failures preserve explicit state and honest uncertainty. |
| Update, repair, and removal | Inventory, health, repair, rollback, cleanup, and removal plans distinguish managed, edited, absent, unsafe, and externally owned assets. |

The three retained visibility choices are therefore preserved: a stateful
collapsed Messages rail, the body-free tmux border count, and a deliberately
chrome-free pane-only workflow.

## Architecture acceptance

The independent architecture and responsibility audit applied the boundaries
in [ARCHITECTURE.md](ARCHITECTURE.md),
[CYCLOPS_BETA_CHARTER.md](CYCLOPS_BETA_CHARTER.md), and the supporting
[whole-system architecture review](../CYCLOPS_SYSTEM_ARCHITECTURE_REVIEW.md),
and found the following rooms coherent:

| Room | Accepted boundary |
|---|---|
| Mail room | `WorkspaceMessaging` owns durable acceptance, FIFO, claims, replies, recovery, and notification policy. Ordinary callers no longer traverse journal variants, projections, locks, worker topology, or post-commit scheduling. |
| Observation room | Fusion and hooks publish immutable evidence. They do not directly apply messaging policy. |
| Living room | The workspace owns human interaction with panes, layout, Messages, and Stream through authenticated daemon projections. |
| Windows | Shared stream and messaging presentation models render supplied data. Presentation owns no socket, journal, tmux, or messaging mechanism. |
| Utility room | tmux and socket effects remain named adapters. The only non-adapter tmux call is the documented boot-time `tmux -V` version probe. |
| Records room | Append-only journals, replay, and migration mechanics remain behind durable-store boundaries. |
| Front hall | The CLI and headless paths use shared client framing; the UI’s shared grid vocabulary is isolated from full terminal implementations. |

The audit identified no material messaging responsibility in the wrong module.
Remaining `Arc<Inner>` use does not let messaging policy reach unrelated daemon
state. This does not require eliminating every `Arc` or extracting a new crate.

## Compatibility, safety, and known limits

- Every journal shape readable at the beginning of the refactor remains
  readable, including original doorbells, Formats 1 and 2, incomplete
  bindings, restricted unknown numeric formats, direct payloads, and historical
  transitions. This is not a promise of indefinite format compatibility.
- Every acknowledged append ends in a newline and is fsynced. Newline-terminated
  records are immutable. An unterminated final tail was never acknowledged:
  lenient replay seals it and retains it when valid, otherwise skips it; strict
  workspace replay removes only that tail, logs a warning, and rejects malformed
  complete lines. No acknowledged record is silently deleted, truncated, or
  rewritten.
- `Daemon::deliver_payload` remains compatibility-sensitive. Its external
  support status is unverified, so it must not be removed or substantially
  changed without a fresh caller census.
- A complete data-lifecycle policy is deliberately deferred. The interim rule
  is still binding: no silent loss, and any breaking migration needs an export
  or migration path.
- Terminal injection is not claimed to be race-free. Ordinary paths require
  fresh positive evidence and record intent before an effect. The separately
  documented default-off administrator force-submit setting never replaces
  bytes, but may submit trailing human input for one exact `verify_failed`
  attempt; it is a bounded liveness tradeoff, not automatic raw-tmux fallback.

## CI and regression evidence

The ordinary pull-request path is measured against the Task 1 baseline in
[CI.md](CI.md): 10m38 wall time and 32m29 runner minutes became 7m47 and
15m16 for the final representative Milestone 7 run, a 26.8% wall-time and
53.0% runner-minute reduction. Required check names remained stable.

The evidence lanes now separate required pull-request correctness,
conditional integration, scheduled evidence, and release evidence. Expensive
tmux HEAD, broad platform, race, soak, long-history, and performance work
still exists; it moved to scheduled or release ownership with explicit gates.
The audit identified no silently removed defect class. The Backspace regression,
path independence, resource ownership, source-boundary lints, and exact-output
documentation checks each have focused evidence at the cheapest honest level.

## Finding disposition

The whole-system review findings have these final dispositions:

| Finding | Final disposition |
|---|---|
| F1 | Resolved by the approved beta authority and one active integration line. |
| F2–F5 | Resolved: configured focus, workspace interaction locality, presentation seams, and headless behavior have focused evidence. |
| F6 | Accepted named daemon-composition boundary. It is intentionally not a claim that every daemon reference disappeared. |
| F7 | Accepted gradual terminology correction; wire and journal compatibility take precedence over mechanical renaming. |
| F8 | Accepted measured source-install path; no unapproved distribution claim is made. |
| F9 | Resolved with managed-asset inventory and lifecycle distinctions. |
| F10 | Operator release gate remains open. |
| F11 | Resolved by one explicit configuration authority and migration-safe behavior. |
| F12 | Accepted beta limitation: inventory, export, preview, and removal work are present; a complete lifecycle policy remains deferred. |
| F13 | Accepted evidence limit: retained measurements support the named workloads only. Broad memory-growth and cross-platform claims remain unmade. |
| F14 | Resolved with contract-focused regression organization and deterministic owned fixtures. |
| F15 | Accepted evidence limit: vendor behavior remains measured, manifest-backed, and explicitly bounded by current evidence. |

## Operator gate

Before any public release action, an operator must decide how to reconcile the
Cargo version, existing beta tag, and absent GitHub Release record. Do not
merge **beta/messaging-rework** into `main`, create or move a tag, choose a beta
version, or publish a Release before that decision.

If a candidate-relevant merge lands after this audit, rerun the complete
release-evidence lane against that exact new SHA. The result of that later run,
not this historical candidate run, controls the updated candidate.
