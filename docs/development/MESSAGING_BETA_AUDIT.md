# Messaging Beta Rework audit

**Status:** Track A accepted

**Audit date:** 2026-08-30

**Historical product revision reviewed:** `a1dfa125419bb59a6d2434e30bfed9a5449a615e`

**History-seam base:** `f8946a5bee00df27ad7b4368db129737abd09e5f`

**Stable comparison revision:** `8e40102a538ba1f6364df093658cb5cdd25286de`

This audit originally reported the implementation program authorized by the
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md) complete. An
independent acceptance review found narrower evidence, responsibility, and
documentation gaps. This revision preserves the named historical evidence,
records those gaps, and separates current regression coverage from retained
fail-before evidence.

**Current release status:** This is a Track A acceptance record, not the live
release queue. The current candidate uses `0.1.1-beta` / `v0.1.1-beta`; no new
tag or GitHub Release has been created. See [NEXT.md](NEXT.md) for the current
exact-SHA evidence and operator-approval boundary.

## Verdict

The seven messaging implementation milestones and the three-task CI workstream
are integrated. Their durable frame, transport, mailbox, observation,
compatibility, presentation, and visibility behavior remains in place.
Track A is accepted. Named `WorkspaceMessaging` operations now own current
history and threads, visibility and body release for authenticated readers,
and current-over-legacy ID collision rules.

Retained cross-journal discovery and rename-linked replay use
`CompatibilityHistoryAdapter`. It captures only the state root and per-session
journal references needed for replay.

The correction pass repaired source-boundary lints, clarified client and
application recovery ownership, added collapsed-cue coverage across detach and
reattach, closed the current-history seam, and aligned the execution docs.

`WorkspaceMessaging` still cannot traverse `Inner`; retained daemon-root access
belongs to composition and physical notification, terminal, and lifecycle
adapters.

Whole-product beta implementation is now authorized, but this remains no
authorization to merge to `main`, create or move tags, or publish a release.

## Evidence vocabulary

- **Verified** means checked in the named source revision or by a focused
  regression whose present scope is stated.
- **Measured** means produced by a named run with a commit and environment.
- **Historical** means retained evidence tied to a named revision and not rerun
  for the current acceptance correction.
- **Unverified** means the repository cannot currently prove the claim.
- **Deferred** means intentionally outside the approved beta scope.

## Audit basis

The original focused architecture, protocol, replay, and UI regressions ran at
product milestone head `7de5ac8ae215f2b8f418f43483ef8a68ff7f3cc2`. PR #140 then changed only
documentation and demo scripts, and its pull-request checks passed. The
historical integration revision reviewed by that audit is PR #140's merge
commit, `a1dfa125419bb59a6d2434e30bfed9a5449a615e`. Those results remain
evidence for those named revisions rather than implied results for the current
integration head.

The focused local audit included:

```bash
cargo test -p cyclops-client --all-targets --no-fail-fast
cargo test -p cyclopsd --lib messaging::tests:: -- --nocapture
cargo test -p cyclops-ui --test daemon_client_boundary
cargo test -p cyclops-ledger --all-targets --no-fail-fast
cargo test -p cyclopsd --test m2_history --no-fail-fast
cargo test -p cyclopsd --test attention_recovery --no-fail-fast
./tests/e2e/messaging-docs-parity.sh
./tests/e2e/parity-check.sh
```

Additional exact tests exercised ordered immutable observations, handler and
Module deletion boundaries, historical doorbell and rename-linked replay,
collapsed-rail rendering, hidden invalidation refresh, lost-response
uncertainty, and oversized-response classification.

The acceptance correction adds focused present evidence. A small
source simulation fails against the old first-`#[cfg(test)]` boundary because
that boundary hides the inserted forbidden reference. It passes with the main
test-module boundary, which keeps the later production region in the audited
files in view. A focused tmux regression detaches the first control client,
drops its `App`, reloads the saved collapsed preference into a fresh `App`,
reattaches, and drives the actual `AppMsg::DaemonReconnected` dispatch. The
resulting authenticated body-free snapshot restores Work and attention counts
without opening the Messages pane. That regression protects current behavior;
no historical fail-before result is claimed for it.

The history-seam change includes an architecture guard that was confirmed to
fail before implementation. Its focused tests exercise the real
`WorkspaceMessaging` history and thread operations.

Together with the retained regressions, the tests cover current-over-legacy
collision rules, body release after an exact claim, unrelated-reader
redaction, metadata-less compatibility records, legacy-only cursors,
linked-journal ordering, and read-loss reporting.

## Milestone disposition

| Milestone | Integrated pull requests | Audit result |
|---|---|---|
| Documentation authority | #101 | Verified. The charter, authority hierarchy, execution queue, raw-tmux doctrine, and shipped skill agree. |
| 1. Bounded official frames | #103 | Verified. Official ingress, egress, blocking clients, and async clients share a 1,048,576-byte JSON-object limit, excluding the newline. Historical journal readers are not subject to the live frame ceiling. |
| 2. Shared Daemon Client | #104 | Verified with corrected ownership. `cyclops-client` owns connection facts, greeting, framing, correlation, shared timeout defaults and certainty, refusal decoding, post-write uncertainty, and stream gaps. Callers choose or accept those deadlines; applications own retry schedules and projection restoration. |
| 3. `WorkspaceMessaging` | #107, #111-#127, #133 plus the final history seam | Verified. Mutation families, projections, worker selection, recovery policy, post-commit scheduling, current history and thread composition, body redaction, and current collision ownership live behind the Module. Cross-journal discovery and replay remain an explicit compatibility concern. |
| 4. Observation separation | #109, #128-#132 | Verified. Fusion returns ordered immutable route, composer, ownership, recovery, and ACK evidence. The composition root applies that evidence after observation instead of letting observation execute messaging policy. |
| 5. Compatibility quarantine | #110 | Verified. Current mailbox work and retained direct delivery cross distinct paths; replay readers and fixtures remain. `Daemon::deliver_payload` is preserved as compatibility-sensitive with public support status unverified. |
| 6. Honest seams | #134 | Verified. Snapshot, ephemeral events, durable follow, gaps, and recovery have distinct contracts. Reusable presentation does not own daemon framing, journal traversal, or tmux effects. |
| 7. Collapsed messaging cue | #135 | Verified with acceptance-correction evidence. The collapsed workspace rail reports body-free Work, attention, and stale or unknown state without opening the pane; the detach/reattach regression reloads the saved collapsed choice and rebuilds those counts through the reconnect dispatch. Adopted tmux keeps its body-free border count, and chrome-free mode keeps manual inbox inspection. |
| Post-audit demo repair | #140 | Verified. The maintained demo proves durable acceptance and body-free visibility without either UI. Obsolete alpha M2 and M3 scripts remain in Git history instead of being presented as beta behavior. |

## Architecture and deletion test

### Verified ownership

- `WorkspaceMessaging` owns `MailboxService`, the publication boundary, and a
  narrow post-commit effects capability. It does not own or receive `Inner`.
- Normal mutation and projection handlers authenticate and call named
  `WorkspaceMessaging` operations. They do not coordinate worker topology,
  schedule post-commit work, read the current message journal, interpret body
  release, or inspect current collision ownership. `msg.history` and
  `msg.thread` receive authenticated identities and immutable compatibility
  sources; current messaging policy remains inside the Module.
- `CompatibilityHistoryAdapter` captures only the validated state root and
  ordered session-journal references. It owns legacy source discovery,
  rename-linked traversal, stable cursor source names, and read-loss counts.
  It cannot traverse live panes or current mailbox state after capture.
- Delivery and attention mechanisms report physical or durable outcomes to
  `WorkspaceMessaging`; they do not decide the next durable messaging policy.
- Fusion returns typed immutable observations. It cannot obtain the messaging
  Module or run its consequences directly.
- `cyclops-ui` consumes `cyclops-client`, daemon-owned backfill, and a launcher
  focus capability. Its production dependency lints protect journal,
  state-store, tmux, and raw socket ownership.
- Repaired daemon source-boundary lints scan to the main test module instead of
  stopping at the first test-only item. They remain syntactic lints, not proof
  of runtime behavior.
- `cyclops-workspace` owns presentation and explicit user actions. Its hidden
  rail is an authenticated projection, not a second unread queue.

### Remaining daemon-root access

`WorkspaceMessaging` owns no `Inner`. `DaemonWorkspaceMessagingEffects` holds a
`Weak<Inner>` at the composition root so it can invoke retained notification,
terminal, and lifecycle mechanisms. Delivery and attention adapters also
retain daemon-root access for physical pane and terminal work, but they report
outcomes through named `WorkspaceMessaging` operations instead of reading
durable messaging internals or choosing the next durable transition. Syntactic
boundary lints protect against daemon-root, pane-cache, task-spawn,
delivery-enqueue, or raw scheduling knowledge returning to the Module. The
charter required coherent ownership, not removal of every `Arc`.

## Preserved behavior and journeys

| Contract or journey | Audit evidence | Result |
|---|---|---|
| Messaging without either UI | Updated `demos/m1-send.sh`; full parity journey | Verified. Durable acceptance, optional notification failure, body-free listing, and workspace-journal metadata do not require a UI. |
| Exact claim and reply | Full parity journey plus mailbox coordinator tests | Verified. Recipient authorization, idempotent repeat claim, reply ancestry, and durable `reply_to` linkage remain. |
| Durability and replay | `cyclops-ledger`, `m2_history`, and `attention_recovery` suites | Verified. Strict and lenient replay, torn-tail handling, restart settlement, and historical state transitions remain covered. |
| Honest uncertainty | `cyclops-client` contract tests and workspace lost-response regressions | Verified. Known refusal, known-not-sent, and unknown-after-write outcomes remain distinct; uncertain effects are not retried automatically. |
| Recipient FIFO and claims | `WorkspaceMessaging` domain and coordinator suites | Verified. Exact recipient ordering, claim cancellation, requeue, and one-time continuation remain behind the Module. |
| Historical formats | Versioned doorbell, rename-linked journal, configured-alias, history, and recovery fixtures | Verified for every format readable at the start of the refactor. This does not promise indefinite compatibility. |
| Collapsed workspace | Hidden-invalidation, detach/reattach, collapsed-rail, lost-acceptance, and oversized-response regressions | Verified. Hidden refresh is body-free, does not open the panel, retains honest stale state, and the reattachment regression rebuilds current counts without changing the saved visibility choice. |
| Adopted tmux and chrome-free use | Existing border and manual-inbox contracts | Verified. Neither journey was replaced by the collapsed rail. |

The named historical command-output parity run passed. The updated mailbox
demo passed three consecutive complete runs. No arbitrary sleep was added to
make either journey pass. Current parity is rerun through the repository gate;
this audit does not pin its evolving check count.

## CI and reliability

The CI workstream shipped through #102, #105, #106, #108, and the deterministic
follow-ups #136-#139. Superseded pull-request revisions cancel; stable check
names remain; conditional checks return a successful not-applicable result;
and scheduled and release workflows own the evidence removed from ordinary
pull requests.

The final Milestone 7 pull-request run
[33329987959](https://github.com/cyclops-team/cyclops/actions/runs/33329987959)
passed all seven checks. Compared with the Task 1 baseline:

| Measure | Task 1 baseline | Final milestone run | Change |
|---|---:|---:|---:|
| Pull-request wall time | 10m 38s | 7m 47s | 2m 51s less, 26.8% |
| Total runner time | 32m 29s | 15m 16s | 17m 13s less, 53.0% |

These are measured GitHub runs, not a guarantee that queueing and runner speed
will stay constant. The final run preserved all six stable branch-protection
check names plus path classification.

Scheduled run
[33330374062](https://github.com/cyclops-team/cyclops/actions/runs/33330374062)
passed at exact integration commit
`7de5ac8ae215f2b8f418f43483ef8a68ff7f3cc2`. It covered the full Linux and
macOS matrix, tmux HEAD, retained performance workloads, repeated race
evidence, forced cleanup, soak, and long-history work. The retained performance
artifact identifies the commit, dirty state, version, runner image, operating
system, architecture, CPU count, Rust, Cargo, tmux, workload, timing, and
result.

The historical Track A release-evidence run
[33331719737](https://github.com/cyclops-team/cyclops/actions/runs/33331719737)
targeted exact post-demo-repair integration commit
`a1dfa125419bb59a6d2434e30bfed9a5449a615e`. The final whole-product
disposition is recorded in [CYCLOPS_BETA_FINAL_AUDIT.md](CYCLOPS_BETA_FINAL_AUDIT.md).

## Performance comparison

The frozen transport benchmark ran serially on the same Apple M5 Pro with 30
samples per commit, macOS 26.5.2, tmux 3.6a, and Rust 1.97.1. It compared
`main` commit `8e40102a` with beta product commit `7de5ac8a`. Each run used a
clean checkout at its named commit and verified the candidate binary build SHA
before timing it. Times are microseconds.

| Phase | `main` p50 / p95 | Beta p50 / p95 | Measured change |
|---|---:|---:|---:|
| CLI version process startup | 1,536 / 1,704 | 1,580 / 1,981 | 2.9% / 16.3% slower |
| Persistent socket round trip | 12 / 18 | 10 / 14 | 16.7% / 22.2% faster |
| Durable acceptance RPC | 8,990 / 10,083 | 8,003 / 8,834 | 11.0% / 12.4% faster |
| Claim RPC | 8,990 / 10,139 | 8,034 / 8,941 | 10.6% / 11.8% faster |
| Notification pipeline | 543,942 / 562,338 | 550,823 / 568,820 | 1.3% / 1.2% slower |

The beta improves the durable mailbox RPCs and keeps the much
larger notification pipeline within 1.3% in this sample. Its CLI-startup tail
is slower. The sample is sufficient to reject a claim of universal speedup,
but too small to label the startup difference a user-visible regression. It is
retained here so later runs can compare rather than rediscover it.

The scheduled Linux performance artifact also passed every retained budget at
`7de5ac8a`: the 10,000-event stream frame remained under 16 ms, 10,000-message
queue operations remained under 9 ms, workspace control-write and flood
contracts passed, terminal restoration passed, and sustained-backlog
continuity passed.

## Compatibility and migration

- No journal writer, reader, record, or fixture covered by the charter was
  removed. Old doorbells, Formats 1 and 2, incomplete bindings, restricted
  unknown numeric formats, direct payloads, and historical transitions remain
  readable.
- The approved local metadata census remains historical evidence: 575 retained
  lines parsed as JSON, and no message body was collected by the census.
- `Daemon::deliver_payload` remains present and delegates through the explicit
  compatibility module. One retained self-test caller and nine test call sites
  are known. External embedder use remains unverified.
- The interim lifecycle rule remains in force: every acknowledged append ends
  in a newline and is fsynced, and newline-terminated records are immutable.
  A final unterminated tail was never acknowledged: lenient replay retains it
  when valid and skips it otherwise, while strict replay removes only that
  tail. No acknowledged record is silently deleted, truncated, or rewritten.
  A breaking migration requires an explicit export or migration path.

## Historical Track A release evidence

This is retained historical Track A evidence. Run 33331719737 completed
successfully at exact integration commit
`a1dfa125419bb59a6d2434e30bfed9a5449a615e`:

- full clean-checkout repository gates passed on Linux and macOS;
- strict, lenient, and daemon historical-journal compatibility passed;
- installer lifecycle and real user journeys passed on Linux and macOS;
- all three retained performance workloads passed and produced a 90-day
  artifact; and
- the aggregate `beta release evidence complete` gate passed.

The performance artifact identifies the exact clean commit, Cyclops 0.1.0,
Ubuntu 24, x86-64, four CPUs, Rust 1.98.0, tmux 3.4, and all workload statuses
as zero. The workflow did not merge, tag, name, or publish anything.

The completed whole-product acceptance decision is recorded in
[CYCLOPS_BETA_FINAL_AUDIT.md](CYCLOPS_BETA_FINAL_AUDIT.md). It cites the final
functional candidate and its exact release-evidence run. This section remains
as Track A provenance rather than the current release gate.

## Open gates and explicit non-results

1. **Release identity was unresolved when this audit was written.** The newest
   remote tag by creator date was `v0.2.0-beta` from 2026-08-27, the repository
   had no GitHub Release objects, and the workspace version was `0.1.0`. The
   current selected identity is `0.1.1-beta` / `v0.1.1-beta`; it still requires
   exact-SHA evidence and subsequent explicit operator approval before a tag,
   GitHub Release, or publication.
2. **External support for `Daemon::deliver_payload` is unverified.** This blocks
   deletion or substantial change, not the completed internal quarantine.
3. **A complete data-lifecycle policy is deferred.** The interim no-silent-loss
   rule remains binding.
4. **Agent Runner, host-adapter, MCP, distributed broker, and generic workflow
   work is not implemented.** Those were research gates or explicit non-goals,
   not missing beta milestones.
5. **No claim is made that terminal injection can be absolutely race-free.**
   The final pre-write check still leaves a small interval before the terminal
   write; composer ownership, durable intent, and conservative recovery remain
   the protections around that interval.

## Current acceptance and release boundary

Track A continues to satisfy the corrected acceptance criteria. The
whole-product implementation tracks are integrated on the beta branch, and the
technical beta-acceptance decision for the audited candidate, not release
authorization, is [CYCLOPS_BETA_FINAL_AUDIT.md](CYCLOPS_BETA_FINAL_AUDIT.md).
Release-identity reconciliation remains a separate operator gate. See
[NEXT.md](NEXT.md) for the current queue.

Keep **beta/messaging-rework** as the integration branch. Explicit operator
approval is still required before merging it into `main`, creating or moving a
release tag, assigning the final beta version, or publishing a release.
