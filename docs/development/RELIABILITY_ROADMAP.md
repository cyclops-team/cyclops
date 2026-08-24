# Cyclops reliability roadmap

Status: current normative implementation roadmap for the reliability release.

This document defines the remaining production-hardening work for Cyclops. It
does not replace the stable contracts in:

- `docs/development/ARCHITECTURE.md`
- `docs/development/DELIVERY.md`
- `docs/development/INVARIANTS.md`
- `docs/reference/PROTOCOL.md`

Those files own current behavior. This roadmap owns release order, acceptance
gates, and the boundary between release work and later architecture work.

This roadmap supersedes `PLAN.md` revision 17 from the August 2026 messaging
redesign task as the current implementation authority. That plan, its earlier
revisions, `OPTIMALITY.md`, `OPTIMAL_MESSAGING_HORIZONS.md`, discussion notes,
reviews, and handoff records remain historical evidence. They do not add current
scope or override this roadmap.

The release program has exactly four implementation groups followed by one
frozen validation campaign:

1. Queue recovery and operator atomicity
2. Status, watch, and wait truth
3. Bounded interfaces and operator actions
4. Health, update, and cleanup
5. Frozen release validation

The product model and target production architecture below are non-normative
orientation. They describe the required release end state, not proof that every
unfinished gate ships today. Exact vocabulary and current behavior come from
the four contract owners listed above.

## Product model

Cyclops has four separate records. They must not be collapsed into one status.

1. A message is durable payload and routing metadata.
2. A notification is one wake attempt for one message recipient.
3. A mailbox entry records whether that recipient can claim the message.
4. A conversation links replies without pretending that a task is complete.

The default messaging path is mailbox plus one safe notification. The payload
remains in the mailbox until an authenticated recipient claims it. An ambiguous
notification does not retry automatically. It records attention and leaves the
payload claimable.

The supported compatibility path can still write a complete payload into a
terminal composer. It must obey the same identity, readiness, structural
verification, and ambiguity rules. It is not equivalent to the mailbox path and
must not be described as one.

The user-visible state model is also separated:

- Runtime state says whether an agent appears idle, working, or blocked.
- Write readiness says whether the terminal composer is safe to change.
- Mailbox state records pending, claimed, delivered-direct, or superseded
  delivery and claim outcomes.
- Notification state records the wake lifecycle, including claim-driven
  withdrawal before a terminal write.
- Conversation state says which message another message replies to.

No receipt may claim more than the durable fact proves. A notification is not a
read receipt. A claim is not task completion. Pane idle is not message
completion.

## Target production architecture

### Identity

Durable routing uses workspace, session instance, endpoint, and process
generation identities. Display names are aliases only. Renaming a pane cannot
change an existing message route or reply destination.

Every process identity is a PID plus a kernel-observed start time. Authentication
rechecks ancestry and generation at the operation boundary. Cached display data
is never authentication evidence.

### Delivery and readiness

Each recipient owns one FIFO notification stream. A message can be accepted
while its notification waits behind another item. The daemon serializes terminal
writes and never lets two senders share one composer at the same time.

Terminal writing requires all of the following at the write boundary:

- the same admitted endpoint and process generation
- the same manifest
- a current, positive clean-composer reading
- no pane mode
- no live working, blocked, input-present, or conflicting sensor reading
- a structural proof that the exact staged notification is complete

A lifecycle hook reports a runtime edge. It never proves that the composer is
safe. A clean composer reading never proves that a turn ended. These signals may
cooperate, but they do not replace each other.

Raw wrapping, collapsed-paste chips, and terminal chrome are representations,
not vendor identities. Generic transport code owns completeness and terminal
anchoring. Manifests own representation-specific vocabulary. Production code
must not branch on vendor names.

### Claim settlement

A successful claim settles notification work according to the write boundary:

- A notification that has not written is withdrawn.
- A staged or submitted doorbell is recorded as notified and keeps its composer
  barrier until the terminal is reconciled.
- A direct payload that crossed the write boundary is not withdrawn.
- An attention record after the write boundary stays open and clearable.

The claim fact is the only durable owner of this settlement. Replay derives the
notification projection from that fact. Cyclops must not append a second fact
that can disagree with the claim.

### Status and pull receive

Status is a projection of durable and live owners. It must not infer one state
from another. A waiting row reports a content-free reason, age, FIFO position,
and next action from the daemon.

Pull receive claims a message through the socket and writes no terminal bytes.
It can therefore make progress while the recipient is working. Watch filters
resolve an alias once to a durable recipient identity. An unknown alias fails
immediately. A rename does not silently retarget or strand an active watch.

The system reports a content-free deadlock risk when a recipient is blocked in a
receive tool while a terminal notification to that recipient is held. This is a
diagnostic, not permission to bypass write readiness.

### Operator control

Local health and cleanup do not require an agent mailbox identity. Workspace
administration is authenticated separately from agent messaging. A human can
open a fresh terminal and inspect or administer the workspace without pretending
to be an agent pane.

Alarm preview is body-free. Alarm resolution uses stable notification attempt
identities. Multi-alarm clearance validates every target and appends one durable
batch fact so an I/O failure cannot expose a half-applied command. Historical
single-clear facts remain replayable.

### Human interfaces

The Messages interface is one queue with stable recipient-specific targets.
Attention is pinned above ordinary inbox work. FIFO order remains visible within
each band. Selection follows a durable target, never a row index. Every mutation
freezes the target and snapshot watermark before confirmation. If the target
changes or disappears, the action refuses by identity instead of moving to a
neighbor.

Rows never contain message bodies. Authorized detail loading is explicit. The
detail view preserves drafts across refused operations and treats an uncertain
outcome as uncertain rather than offering an unsafe retry.

Both shipped full-screen interfaces, `cyclops-ui` and `cyclops-workspace`, use
bounded input paths. Keyboard input, ordered stream events, refresh invalidation,
snapshots, and action results have separate capacity and coalescing policies. A
slow daemon or event burst cannot grow memory without bound or make keypresses
wait behind history.

## Release-blocking Codex 0.149.1 incident

A reported Codex 0.149.1 failure repeatedly records a safe visual gate followed
by `binding_unprovable`, leaves the notification at the head of the recipient
FIFO, and gives the sender no surface-level explanation beyond durable
acceptance. The report is release-blocking, but its manifest-change hypothesis
is not accepted without reproduction. The observed sequence points to a
pre-write binding failure and must be traced to the exact write boundary before
any terminal rule changes.

The correction has four parts:

1. Reproduce fresh and resumed Codex 0.149.1 process trees and screen
   representations. Capture identity, ancestry, manifest, route, rule winners,
   readiness, write boundary, and hook behavior without recording terminal
   content in journals.
2. Bound repeated identical pre-write failure by evidence changes. After the
   bounded evaluation, retain one named visible blocked notification. Do not
   retry on a timer and do not touch the composer.
3. Permit an operator to withdraw one exact recipient notification only while
   its mailbox entry is unclaimed and the daemon proves no terminal write may
   have occurred. The append-only withdrawal retains message history and
   releases that recipient FIFO. Unknown or post-write boundaries remain
   attention work.
4. Keep authenticated socket claim usable while terminal wake is blocked. A
   claim settles the blocked pre-write notification and admits the next FIFO
   item under the existing claim-settlement rules.

The sender and every operator surface must distinguish durable acceptance from
`wake blocked before write: binding_unprovable`. The incident closes only when
fresh and resumed Codex 0.149.1 sessions either complete the normal notification,
claim, and reply path, or stop once in the visible blocked state without writing
terminal bytes.

## Release gates

The release is complete only when all seven gates pass on one frozen candidate.
An earlier focused pass does not substitute for the frozen run.

### Gate 1: process generation and route correctness

- Pane discovery distinguishes shell root, admitted agent, foreground tool, and
  hook helper.
- Direct-child and sibling hook helpers authenticate through the admitted agent
  ancestry.
- A retained connection from a replaced process generation is denied.
- Watcher rows reconcile process death, replacement, detach, reattach, and pane
  removal without duplicate live entries.
- Durable replies route to the original endpoint after an alias rename.
- Unknown aliases fail before a blocking watch begins.

### Gate 2: lifecycle, readiness, and submit correctness

- An authenticated prompt-start edge marks a supported pane working before
  output appears.
- A matching end edge cannot release a composer barrier until a fresh clean
  composer reading agrees.
- A missing or mismatched end edge holds and emits one bounded diagnostic.
- A lifecycle cache eviction cannot erase a durable turn edge.
- Capture failure, pane mode, stale observation, identity change, and sensor
  conflict all refuse terminal writing.
- Raw-wrap and collapsed-chip staging prove complete notification structure.
- A changed, truncated, echoed, or followed trailer fails closed.
- Submit success requires proof that the staged notification was consumed.
- A pasted but unsent notification remains recoverable through attention.

### Gate 3: durable queue and recovery

- Each message-recipient pair owns one mailbox entry and one FIFO notification
  stream.
- Claim settlement follows the write-boundary rules and replays identically.
- Broadcast recipients remain distinct through queueing, selection, claim, reply,
  alarm, rename, and restart.
- Multi-alarm clear is one append-only batch fact after complete validation.
- Normal worker retirement removes its registry entry.
- Both mailbox notification workers and supported legacy direct-delivery workers
  expose an exact in-flight attempt under the queue mutex.
- An unexpected worker exit classifies the in-flight attempt from durable state:
  unwritten work resumes as the same attempt, uncertain post-write work becomes
  attention, and terminal work remains terminal.
- Recovery failure visibly faults the worker. It cannot restart silently.
- Composer barriers cannot leak if a worker exits between the write boundary and
  durable state transition.
- Repeated identical `binding_unprovable` evidence settles into one visible
  pre-write blocked notification and performs no timer retry.
- Recipient-scoped withdrawal accepts only an exact, unclaimed, provably
  unwritten attempt, appends one durable fact, and releases the next FIFO item.
- A socket claim remains available while terminal wake is blocked and settles
  the blocked pre-write attempt without a second terminal write.
- Session churn keeps worker, route, and queue counts bounded.

### Gate 4: identity, status, wait, and watch truth

- A fresh human terminal can perform authorized workspace administration without
  becoming a mailbox endpoint.
- Status has one live row per current session and separates runtime, readiness,
  mailbox, notification, and attention facts.
- Waiting rows show daemon-owned reason, age, FIFO position, and next action.
  FIFO position already exists and must remain the daemon's answer.
- Pull receive can claim while the pane is working and writes zero terminal
  bytes.
- A receive-tool cycle reports a content-free deadlock risk.
- Watch binds to a durable identity and reports rename, replacement, or invalid
  route explicitly.
- Send returns durable acceptance. Pane wait remains pane-scoped. The CLI must not
  advertise message completion that Cyclops cannot prove.
- Send, status, Messages, and operator detail expose a pre-write blocked wake,
  its reason, age, FIFO position, current route, and exact next action.

### Gate 5: bounded and stable interfaces

- Messages shows one row per message-recipient pair and never duplicates an open
  alarm as a second piece of work.
- Attention, inbox, outbound, and observed rows use distinct state words.
- The selected target survives insertion, rename, reorder, broadcast fan-out,
  and viewport movement.
- A removed target refuses rather than acting on its replacement.
- Narrow and short terminals retain the selection marker and both state words.
- Authorized detail is body-bearing; resting rows, preview, logs, and diagnostics
  stay body-free.
- `cyclops-ui` and `cyclops-workspace` bound keyboard, stream, refresh, snapshot,
  and action ingress by items and bytes.
- Stream gaps and dropped invalidations become explicit stale state followed by
  one whole-snapshot reconciliation.
- Draw time follows visible rows, not total mailbox size. Performance budgets are
  measured on a quiet machine and enforced in tests.

### Gate 6: health, update, and cleanup

- `cyclops health` performs read-only, descriptor-relative, no-follow inspection
  and works with the daemon stopped.
- Health reports every installed binary found on PATH, daemon version, setup,
  hooks, manifests, skills, permissions, journals, caches, logs, update scratch,
  and rollback state.
- Update uses randomized owner-only scratch with an ownership marker and a
  kernel-held lease.
- One canonical cache is shared by update and cleanup. Legacy caches are named
  migration candidates, not silently reused.
- Candidate binaries prove version identity and journal replay before install.
- Installation replaces the CLI and daemon as one recoverable pair and retains
  one known-good pair.
- Cleanup is dry-run by default, accepts asset classes rather than arbitrary
  paths, revalidates every target, and never touches durable journals.
- Cleanup never kills processes. Update and rollback may stop only the exact
  daemon instance they authenticated and own.
- Logs use bounded writers. Linked files, unsafe ownership, changed inodes, and
  active leases fail closed.

### Gate 7: frozen release proof

- Formatting, strict lint, documentation parity, protocol compatibility, focused
  suites, and the full workspace suite pass on the same commit.
- Current-version live fixtures cover every supported vendor for idle, working,
  long staged input, modal state, quota state, raw wrap, and collapsed chip where
  the representation exists.
- Codex 0.149.1 evidence covers clean idle, ghost suggestion, typed text, slash
  command, working, tool execution, approval, raw and collapsed staging,
  submission, hook receipt, claim, reply, restart, and both fresh and resumed
  process structures.
- Missing vendor software is an explicit release limitation or an administrator
  decision, not fixture evidence presented as a live pass.
- Deterministic process, lifecycle, queue, restart, permissions, symlink,
  hard-link, update-crash, and UI-boundary tests pass.
- Live stage-and-clear evidence is opt-in and dated. Default tests never launch
  vendor TUIs or rewrite evidence artifacts.
- Transport benchmarks separate persistent socket cost, CLI startup, durable
  acceptance, notification, claim, and agent response. Fire-and-forget terminal
  input is not presented as verified delivery.

## Implementation order

Parallel work is safe only where ownership does not overlap.

1. Close the Codex 0.149.1 pre-write rebound, then complete worker supervision
   and atomic alarm clearance in the same queue-recovery group.
2. Complete daemon-owned wait detail, durable watch binding, and honest wait CLI
   wording.
3. Bound ingress in both full-screen interfaces and finish Messages actions.
4. Build read-only health, transactional update, bounded logs, and safe cleanup.
5. Freeze one candidate, run the full gates once, then perform adversarial review
   against the frozen bytes.

Protocol changes land before their producers and consumers. Durable facts land
with replay support in the same change. UI actions land only after the daemon
operation has stable identity and idempotency semantics.

## Evidence model

Evidence is stored outside this roadmap because it changes every run. A release
record names:

- exact commit
- exact commands and exit codes
- test totals
- live vendor versions and fixture provenance
- dated opt-in soak artifacts
- performance distributions and machine conditions
- known limitations and explicit administrator decisions

No command is reported green through a pipeline that can hide its exit code. A
test that proves only a fixture is labeled fixture evidence. A live check proves
only the exact version and representation that ran.

## Engineering rules

- One owner per durable fact, state transition, identity, and cleanup policy.
- State machines expose semantic predicates instead of repeated string matching
  or duplicated enum lists.
- Security decisions use current kernel evidence and stable identities.
- No lock is held across filesystem, process, socket, terminal, or journal I/O.
- Queue mutation and worker registry operations use one documented lock order.
- Every retry is backed by an idempotency key or an explicit refusal.
- Ambiguity is recorded and surfaced. It is never converted into success.
- Production comments explain behavior and non-obvious safety decisions. They do
  not narrate implementation history.
- Public documentation uses product language, not internal review shorthand.
- Compatibility behavior stays explicit until it is intentionally removed with
  migration evidence.

## Later architecture work

These changes may improve ownership after the release gates pass. They are not
reasons to delay a correct release or to rewrite working code during hardening.

- Deepen pane runtime ownership so discovery, lifecycle, readiness, and process
  generation have one narrow public interface.
- Separate durable endpoint adoption from live tmux route lookup.
- Deepen terminal writing behind a representation-neutral staging and submit
  interface.
- Retire legacy direct-payload delivery only after supported vendors have a
  proven mailbox wake path and operators have a migration path.
- Add a general multi-fact journal transaction only when another operation needs
  it. Atomic alarm clearance needs one batch fact, not a transaction framework.
- Group protocol handlers by product domain after behavior and wire shapes are
  stable.
- Add concurrency only after queue-depth and latency measurements identify a
  real bottleneck.

## Deferred product work

- Native vendor protocol injectors
- MCP as a wake transport
- Automatic notification retries
- General cancellation
- Broadcast supersession
- Artifact storage and retention
- A task-completion receipt without an explicit agent-side completion contract

These ideas require separate product decisions. None may weaken mailbox
durability, terminal readiness, stable identity, or receipt honesty.

## Definition of done

Cyclops is ready to recommend for daily use when:

1. Sending records a durable message immediately.
2. Pull receive works while an agent is busy and touches no composer.
3. A terminal notification is written and submitted only with current structural
   and readiness proof.
4. Pasted-but-unsent and uncertain post-write outcomes remain recoverable.
5. Rename, restart, detach, replacement, broadcast, and worker failure preserve
   exact ownership.
6. Status and Messages show one coherent, bounded, identity-stable view.
7. A fresh terminal can inspect health and perform authorized operator work.
8. Update and cleanup are repeatable, bounded, recoverable, and safe around
   durable history.
9. The frozen release candidate passes every gate with dated evidence.

Until all nine statements are true on one frozen commit, the tree is a release
candidate, not a finished production release.
