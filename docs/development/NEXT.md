# Current execution queue

**Status:** Current execution queue

**Implementation authority:**
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md)

**Integration branch:** **beta/messaging-rework**

Cyclops is executing the Messaging Beta Rework as independently verifiable
milestones. This page states what runs next. The charter owns rationale,
preserved behavior, stop conditions, and rollback requirements.

## Current milestone

The next focused completion pass runs on
**beta/refactor/observation-messaging-evidence**. The first Milestone 3
family proved the internal seam. The read-and-claim pass then moved inbox listing,
claiming, message snapshots, and durable follow pages behind
`WorkspaceMessaging`; the Module owns retained claim-locator interpretation,
publication synchronization, claim result mapping, and the post-claim
scheduling sequence. The mutation pass moved requeue and exact pre-write
withdrawal behind the same boundary, including durable mutation, notification
scheduling, cancellation, FIFO advance, and unread invalidation. Socket
handlers and in-process seams no longer coordinate those mechanisms. The
attention pass moved body-free alarm projection, administrator clearance,
exact attention-target selection, ambiguity, and recipient privacy behind the
Module; the socket layer no longer receives mailbox records for that family.
The status pass moved mailbox route fallback, unread counts, held attention,
and the bounded blocked-wake sample behind one body-free Module projection;
daemon status retains the legacy session-ledger fold separately and no longer
reads mailbox projections directly. The runtime-effects boundary pass moved
`WorkspaceMessaging` construction and its daemon adapter into the composition
root; the operation Module now sees only named capabilities and a dependency
lint prevents it from recovering `Inner`. The runtime-host pass then moved
recipient FIFO continuation and direct-delivery unread ordering behind the
Module, so delivery and attention mechanisms no longer receive the mailbox
service merely to call the recipient scheduler. The runtime-evidence pass then
moved route observations from fusion and authenticated ACK handling through an
immutable evidence type, and moved durable pre-write, attention, notified, and
force-submit-setting consequences behind named Module operations. Those
callers no longer choose messaging schedulers. The attention-commit pass then
moved resolution reservation, durable intent, accepted-action, consumption,
settlement, no-key discard, and pre-key withdrawal behind `WorkspaceMessaging`;
the terminal mechanism retains exact terminal proof and action. The
attention-runtime pass then moved exact-owned candidate selection, automatic
policy, evidence coalescing, worker election, conflict parking, and re-election
behind the Module. The composition adapter hosts elected tasks, and the terminal
mechanism performs one requested exact action without scanning messaging
projections or spawning workers. The terminal-support pass then moved expected
payload reconstruction, current route lookup, runtime target selection,
consumption registration, and deterministic registration cleanup behind named
Module operations; the terminal mechanism no longer receives `MailboxService`.
The status-composer pass then moved active candidate cardinality, durable
binding comparison, mailbox-state mapping, recovery variants, worker ownership,
manifest clear support, and next-action policy behind the body-free Module
status operation. Daemon status now supplies immutable pane evidence and copies
one finished decision instead of reconstructing messaging policy. The
runtime-locality pass then moved daemon-root notification scheduling and
terminal recovery into a physically separate composition adapter. A whole-file
lint prevents the durable Module from recovering `Inner`, the pane cache, task
spawning, delivery enqueueing, or pane observation. The diagnostics pass then
moved the foreground-watch diagnostic's durable gating projection, route
resolution, and working-state join behind `WorkspaceMessaging`. Deadlock
detection now consumes body-free candidates and inspects only operating-system
process state; a deletion lint prevents it from recovering mailbox, route, or
daemon-root knowledge. The fresh Milestone 3 responsibility and locality audit
found that authenticated hook handling still reached the mailbox service to
match boot-local attention candidates. The first audit correction now sends
one immutable authenticated observation through `WorkspaceMessaging`; the
Module owns exact registered-candidate lookup, payload and durable-binding
comparison, and the one-shot signal. The pre-write correction moved
publication synchronization, readiness-route baseline synthesis, wake-block
policy, the durable block transition, benign terminal-state classification,
the exhausted-supervisor transition, and the first post-commit route
consequence behind the Module. Delivery now retains physical route observation
and worker fault ownership while receiving only a body-free
recorded-or-obsolete result. The recovery correction then moved active-barrier
lookup, exact claim comparison, recovery coordination, retirement policy and
persistence, writer-uncertainty handling, and lifecycle, replacement, and
pane-loss settlement behind `WorkspaceMessaging`. Fusion now supplies physical
binding, screen, and exact-turn evidence and carries one opaque recovery plan.
Runtime adapters can track or settle only an exact attempt through named
Module operations. The composer-projection correction then moved fusion's
remaining active-candidate lookup, durable cardinality and binding join,
notification payload reconstruction, and submission-state interpretation
behind an opaque `WorkspaceMessaging` probe. Fusion now supplies only
immutable terminal facts and receives one body-free projection; deletion
evidence prevents raw candidate records or durable projection policy from
returning. The observation-evidence correction now returns a state or composer
edge as typed exact-owned evidence and applies it at the composition root. Pane
observation carries an ordered collection so a simultaneous quota-reset fact is
not dropped, while a deletion lint prevents fusion from invoking exact-owned
candidate selection or worker election directly. The fresh follow-up audit
found that the readiness helper still invoked route reconciliation while
observation was running. The route-evidence correction now returns that causal
token as the first typed item in the ordered pane result. Cache-only hold
mutations produce the same type and pass it through the composition root, while
tokenless and unchanged negative observations remain quiet. A deletion lint
prevents fusion from invoking route or exact-owned policy directly. The
composer-recovery correction on
**beta/refactor/observation-messaging-recovery-evidence** now passes immutable
binding, clean-composer, legacy-readiness, and recovered-turn evidence through
a narrow composition-root boundary. Only
`WorkspaceMessaging` reads durable probes or decides recovery, retirement, and
coordinator policy; fusion receives a body-free barrier update inside the same
serialized cache transaction. A deletion lint prevents fusion and the physical
recovery helper from recovering the concrete messaging Module. The dispatch
ACK correction on **beta/refactor/observation-messaging-ack-evidence** now
returns exact route, process, manifest, turn, and causal-time evidence after
the cache commit and any state event. The composition root confirms the
retained delivery before ordered messaging observations or presentation, and
a stalled chrome repaint cannot consume that durable receipt. A deletion lint
prevents fusion from calling the ACK mechanism directly. A fresh Milestone 4
responsibility audit is next; use
**beta/refactor/observation-messaging-audit** only when that audit
finds a material misplaced responsibility requiring a focused correction.
Additional narrowly named completion branches remain allowed when one pull
request would become broad. Milestone 5 put
retained direct-delivery entry points, restart settlement, and session-journal
traversal behind
`src/cyclopsd/src/compatibility.rs`; its census preserves
`Daemon::deliver_payload` with support status unverified and preserves every
currently readable journal shape. Milestone 4 moved positive quota-reset
consequences out of fusion: pane observation now returns immutable evidence,
while `WorkspaceMessaging` commits the durable reset facts and decides the
explicit administrator notices that the daemon composition root commits in PR
#109. Milestone 3 put send and reply
acceptance behind the internal `WorkspaceMessaging` Module. Milestone 2
consolidated official transport semantics in PR #104, and Milestone 1 merged in
PR #103 after the documentation authority repair in PR #101.

Exit evidence:

- `WorkspaceMessaging` owns the charter-assigned durable messaging decisions;
- ordinary callers do not understand journal variants, projection internals,
  messaging locks, worker topology, or post-commit scheduling;
- remaining `Arc<Inner>` use cannot give messaging code unrelated daemon state
  merely for convenience;
- focused contract evidence protects each moved family; and
- a fresh responsibility audit identifies no material messaging decision left
  in an ordinary caller before observation completion begins.

## Milestone queue

1. **beta/fix/frame-contract**: implement only the end-to-end bounded official
   daemon frame contract. The shared limit is 1,048,576 JSON-object bytes,
   excluding the newline. This is a reliability prerequisite, not the
   `WorkspaceMessaging` extraction.
2. **beta/refactor/daemon-client**: consolidate official framing, correlation,
   timeout, uncertainty, gap, reconnect, and recovery semantics only after
   Milestone 1 is accepted.
3. **beta/refactor/workspace-messaging**: introduce one narrow internal
   `WorkspaceMessaging` operation family and prove that callers lose messaging
   knowledge.
4. **beta/refactor/observation-messaging**: separate observation and messaging
   responsibilities without changing durable behavior.
5. **beta/refactor/legacy-compatibility**: quarantine compatibility-sensitive
   legacy paths after the caller census and focused regression evidence.
   Before Milestone 6, use focused
   **beta/refactor/workspace-messaging-completion** and
   **beta/refactor/observation-messaging-completion** pull requests, with a
   descriptive suffix for any additional slice, until the charter's
   responsibility audit is clean.
6. **beta/refactor/presentation-seams**: make snapshot, follow, event, and
   presentation seams honest and explicit.
7. **beta/feat/collapsed-messages-cue**: add the missing collapsed-workspace
   messaging cue while preserving all three approved visibility choices.

`cyclops-delivery-core` was an earlier name for the same modularity goal now
called `WorkspaceMessaging`. It does not authorize a crate, a second messaging
system, or an extraction debate. Do not create a crate unless the internal
Module later proves that a crate deletes additional caller knowledge or
provides measurable isolation, and the operator separately approves it.

## Branch and review rules

- Do not develop the beta rework directly on `main`.
- Do not make routine implementation commits directly on
  **beta/messaging-rework**.
- Give each milestone its own branch, pull request into the integration branch,
  regression evidence, review, and rollback point.
- Do not begin a later milestone inside an earlier pull request.
- Keep the integration branch synchronized with `main` through reviewed merges.
- Merge milestone pull requests when their required evidence, review, and CI
  are green. Do not merge the beta integration branch into `main` or publish a
  release without operator approval.
- After the approved beta scope, run fresh architecture, regression,
  performance, migration, and user-journey audits before the final pull request
  from **beta/messaging-rework** into `main`.

## WorkspaceMessaging completion session boundary

Use this scope:

> Continue Milestone 3 from the approved Messaging Refactor Charter after its
> first tracer family. Move one additional coherent family of durable messaging
> decisions or post-commit coordination behind `WorkspaceMessaging`, and prove
> the ordinary caller loses journal, projection, lock, worker, or scheduling
> knowledge. Keep the pull request focused and repeat through later focused
> branches until the responsibility audit is clean. Do not change wire meaning,
> readable journals, UI behavior, observation policy, presentation, broad CI,
> MCP, or later milestones. Preserve honest uncertainty and stop if any charter
> stop condition is encountered.

## Release naming gate

Verified on 2026-08-29: the newest remote tag is `v0.2.0-beta`, created
2026-08-27; GitHub has no Release objects; and the repository-root `Cargo.toml`
declares `0.1.0`. Reconcile the version, tag, and release authorities before
assigning or publishing the final beta version number.

## Not authorized by this queue

No current messaging milestone authorizes UI redesign, broad CI restructuring, legacy
deletion, runner or host-adapter production work, MCP work, broad rewriting,
automatic raw-tmux fallback, a complete data-lifecycle policy, or release
publication. The separately authorized CI workstream remains isolated on its
own branches and pull requests. Preserve every currently readable journal
format throughout this refactor. Until a complete lifecycle policy is approved,
allow no silent deletion, truncation, or rewriting; a breaking migration
requires an explicit export or migration path.
