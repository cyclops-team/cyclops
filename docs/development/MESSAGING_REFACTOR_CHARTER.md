# Messaging refactor charter

**Status:** Approved implementation authority

**Approved:** 2026-08-29

**Integration branch:** **beta/messaging-rework**

**Reviewed revision:** `ead57c1691371a1deca5afeb89e90e8340accb69`

**Review date:** 2026-08-29

This charter controls the Cyclops Messaging Beta Rework. Approval authorizes
the documented sequence and boundaries, not a broad rewrite. Each production
milestone still requires its own branch, pull request, regression evidence,
review, and rollback point. The documentation authority change precedes all
production work.

## 1. Scope

This charter defines the smallest evidence-backed path from the current
messaging implementation to a system whose responsibilities are easier to
understand and change. It resolves the immediate packaging conflict, records
the behavior that must survive, classifies the review proposals, separates CI
work from messaging work, and specifies one later production tracer bullet.

The first approved phase changes documentation plus the shipped Cyclops skill
and its required current-body seeding hash. It does not change messaging
runtime behavior, tests, the public command surface, durable data, or the
running system. It approves and repairs this charter, rewrites
[NEXT.md](NEXT.md), updates [HANDOFF.md](HANDOFF.md), repairs documentation
authority and links, makes the shipped skill the emergency-doctrine source of
truth, and synchronizes the installed copy. It does not create
`cyclops-delivery-core`, a runner, a host adapter, or an MCP adapter.

The direct user goal is a messaging system that works without either UI, stays
understandable to pane-only users, keeps activation optional, reports only facts
it can prove, and becomes easier to change through small Modules that delete
knowledge from callers.

## 2. Authority hierarchy

When sources conflict, use this order:

1. Current behavior contracts in [ARCHITECTURE.md](ARCHITECTURE.md),
   [DELIVERY.md](DELIVERY.md), [INVARIANTS.md](INVARIANTS.md),
   [PROTOCOL.md](../reference/PROTOCOL.md), [GOALS.md](GOALS.md), and
   [STYLE.md](STYLE.md), except where current evidence proves that wording
   overstates implementation.
2. This approved charter as implementation authority for the Messaging Beta
   Rework.
3. [NEXT.md](NEXT.md) as the thin current execution queue.
4. The
   [messaging architecture review](../MESSAGING_ARCHITECTURE_REVIEW.md) and
   [addendum](../ADDENDUM_REVIEW.md) as supporting design records.
5. The [CI review](CI_TEST_ARCHITECTURE_REVIEW.md) as a separate active
   proposal, not a messaging milestone.
6. Historical records and superseded plans, which are never current
   implementation authority.

Repository rules in `AGENTS.md` remain binding throughout. The direct operator
decision controls where this hierarchy does not answer a question.

The contracts remain authoritative for behavior that exists and is internally
consistent. A demonstrated mismatch is classified here rather than concealed
by choosing either prose or code uncritically.

## 3. Exact reviewed commit and dirty-worktree state

The review and all current-state probes used this exact commit:

```text
ead57c1691371a1deca5afeb89e90e8340accb69
```

Before this charter was created, the worktree was:

```text
## main...origin/main
 M docs/development/HANDOFF.md
?? docs/ADDENDUM_REVIEW.md
?? docs/MESSAGING_ARCHITECTURE_REVIEW.md
?? docs/development/CI_TEST_ARCHITECTURE_REVIEW.md
?? dump.md
```

Those were pre-existing user changes at review start. The approved
documentation pass now incorporates the review documents, repairs
`HANDOFF.md`, and moves the unrelated `dump.md` out of this repository. This
provenance record does not describe the later branch state.

## 4. Independent target synthesis

The following target was written before reading `NEXT.md`, `ARCHITECTURE.md`,
`GOALS.md`, or the CI review. Later inspection refined its evidence but did not
change its basic shape.

Cyclops should remain one local durable coordinator and a modular monolith.
Messaging truth must survive without any UI, pane, runner, or model process.
Human notification, agent activation, terminal execution, retrieval, reply, and
completion are separate facts. No one fact silently proves the next.

An internal, deep `WorkspaceMessaging` Module should own the durable messaging
decisions that require one transaction. It should hide journal variants,
projection maps, locks, worker topology, crash cuts, and compatibility state
from ordinary callers. A Participant Directory supplies exact identities. A
Pane Observer supplies immutable evidence. Optional runners and host adapters
execute bounded activation work only after separate probes justify them. One
Daemon Client contract supplies every official client with the same framing,
correlation, timeout, uncertainty, gap, reconnect, and recovery semantics.
Presentation derives authorized views without knowing journal paths, socket
framing, tmux commands, or daemon locks.

This target keeps complexity that answers a named failure: durable acceptance,
idempotency, exact identity, recipient FIFO, strict replay, guarded external
effects, and honest uncertainty. It rejects complexity that has no current
requirement: a distributed broker, generic event bus, generic workflow engine,
multi-host mesh, automatic raw-tmux fallback, broad rewrite, or a public
Interface for every noun.

A refactor counts as progress only when the caller loses knowledge. Moving a
field, file, or state name while the same callers still understand its locks,
ordering, storage variants, and crash cuts fails this deletion test.

## 5. User journeys

1. **Agent-only messaging.** An agent discovers an exact recipient, durably
   sends, waits without polling, claims, and replies through the CLI while all
   Cyclops UIs and runners are stopped.
2. **Human-supervised workspace.** A person can use the full workspace, stream,
   compact cue, pane borders, or explicit inbox commands. Hiding a view never
   changes messaging or activation policy and never forces the view open.
3. **Pane-only supervision.** The sender and bounded preview orient the person,
   and an exact command reaches authorized content. The terminal trace is not a
   claim, reply, or completion proof.
4. **Optional activation.** An opted-in sleeping runner may request one bounded
   turn through a host-supported adapter. Mailbox truth remains correct when
   the runner or host is absent, stopped, cancelled, or uncertain.
5. **Crash and recovery.** Committed facts replay. Clients recover from an
   authoritative snapshot or durable follow cursor. An ambiguous external
   effect remains ambiguous and is not repeated automatically.
6. **Confirmed coordinator failure.** Normal communication stays on Cyclops. A
   human may explicitly authorize an exact, labeled, unrecorded raw-tmux send
   only after confirming the daemon is unavailable or broken.
7. **Larger local group.** Each recipient retains independent FIFO and exact
   identity. A slow subscriber or blocked recipient cannot stall unrelated
   recipients or the durable coordinator.

## 6. Target domain responsibilities

These are responsibility decisions, not permission to create a crate or public
type for every row.

| Module or role | Owns | Must not own |
|---|---|---|
| Participant Directory | Stable participant keys, recipient generations, labels, routes, and route freshness | Messages, claims, notification policy, or terminal effects |
| `WorkspaceMessaging` | Acceptance, mailbox entries, idempotency, FIFO, claims, replies, ancestry, notification intent, activation intent, attention and recovery facts, replay projection, and atomic durable transitions | Tmux commands, screen capture, renderer state, or host-specific execution |
| Durable Store | Append, sync, strict replay, torn-tail handling, and migration mechanics requested by `WorkspaceMessaging` | Messaging policy or user-facing outcomes |
| Pane Observer | Immutable, time-scoped runtime, process, manifest, hook, screen, composer, and disagreement evidence with provenance | Durable messaging consequences or terminal activation policy |
| Notification adapter | One guarded human-visible notification attempt and its explicit outcome | Mailbox truth, claim truth, model execution, or completion |
| Agent Runner | Optional acquisition of body-free activation work and one bounded turn request | Message bodies, mailbox policy, caller identity invention, retry policy, or completion inference |
| Agent Host Adapter | Translation of an exact activation request into one real host control path and only the outcomes and visibility that host proves | Durable policy, claims, replies, or generic workflow management |
| Daemon Client | Connection, greeting, frame contract, correlation, timeout classes, known-not-sent versus unknown-after-send, gaps, reconnect, and authoritative recovery | Domain policy or presentation |
| Presentation | Reconstructable view models, explicit actions, full and compact views, and host-visible orientation | Journal paths, socket framing, tmux commands, daemon locks, or storage topology |
| Raw emergency lane | Explicit operator-directed, exact-pane, unrecorded recovery outside the Cyclops contract | Automatic fallback, receipts, ordering, replay, claim, or completion |

Notification and activation are separate semantic concepts now. Notification is
already public behavior. Activation remains an internal target concept until a
host pilot proves that a useful public outcome vocabulary can be supported.
Execution details stay inside adapters.

## 7. Explicit cross-domain syncs

| Trigger | Synchronous decision | Later effect or projection |
|---|---|---|
| Send or reply | Directory resolves exact recipients; `WorkspaceMessaging` commits the message and mailbox entries atomically | Body-free invalidation and independently scheduled notification or activation intent |
| Claim | `WorkspaceMessaging` authenticates and commits exact retrieval and ancestry state | Withdraw pre-effect activation or record retrieval beside a post-effect attempt; refresh attention and unread projections |
| Fresh pane observation | Pane Observer returns immutable evidence | One `WorkspaceMessaging` operation decides durable consequences, then returns explicit effects |
| Notification intent | Durable identity, FIFO, composer ownership, and safety policy select one exact attempt | Notification adapter reports known-not-written, written, receipt-proven, or uncertain |
| Activation intent | Exact recipient and host generation plus opt-in policy select one bounded attempt | Runner and host adapter report only proven host outcomes |
| Recovery action | Intent is recorded before any external effect | Evidence settles the same attempt without erasing its history or inventing success |
| Durable change | Commit remains authoritative | A body-free invalidation may wake clients; authorized snapshots or durable follow return truth |
| Subscriber gap or reconnect | Client marks its projection stale | Daemon Client obtains an authoritative snapshot or durable follow page before re-enabling mutation |

These syncs allow one transaction where invariants require one. They do not
require a distributed transaction protocol or generic event bus.

## 8. Verified current-state findings

All findings below were checked at the reviewed revision.

| Finding | Current evidence | Classification |
|---|---|---|
| Official frame limits disagree | At the reviewed revision, the then-current UI wire module capped encoded and decoded frames at 1,048,576 JSON-object bytes. That module was consolidated and now lives in `src/cyclops-client/src/lib.rs`. `src/cyclopsd/src/server.rs` then used unbounded `lines()` and unbounded response/event serialization. `src/cyclops/src/client.rs` then used unbounded `read_line`, and `src/cyclops/src/main.rs` read complete stdin or files before validation. | Verified P1 interoperability and local resource defect |
| Terminal non-interference was overstated | At the reviewed revision, `INVARIANTS.md` said “Human typing always wins.” `src/cyclopsd/src/delivery.rs` documents an irreducible final proof-to-paste command interval and exposes `post_final_prewrite`. | Verified contract defect; this documentation pass corrects the wording while preserving useful guards |
| Raw emergency doctrine is fragmented | `README.md` and `DELIVERY.md` already make raw tmux manual, unrecorded, and non-automatic. The active skill forbids autonomous bypass but does not state the operator-authorized confirmed-failure exception. | Verified authority and recovery gap; the literal contradiction was overstated |
| Subscribe, snapshot, and follow roles are explicit | `events.subscribe` is an ephemeral push and accepts its old cursor only as compatibility input. `events.backfill` is a bounded body-free connection-epoch projection, current views use snapshots, and `messages.follow` owns durable mailbox progress. | Typed-contract mismatch corrected by Milestone 6 without removing the legacy input |
| Daemon state has low locality | `cyclopsd::Inner` has 46 fields spanning mailbox, events, observation, registry, delivery, hooks, workspace, lifecycle, and fault controls. `Arc<Inner>` reaches through server, messaging, fusion, delivery, and recovery paths. | Verified structural problem |
| Current acceptance is already durable-first | `server::msg_send` authenticates, then `WorkspaceMessaging::send` calls `MailboxService`; the store prepares, appends, and commits its projection before the Module schedules notification, unread, and receipt consequences through a narrow effects capability. | Verified behavior preserved by Milestone 3 |
| Claim coordination is behind the messaging Interface | `WorkspaceMessaging::claim` selects literal versus retained locator claims, commits through `MailboxService`, then owns notification settlement, delivery cancellation, attention reconciliation, FIFO scheduling, and unread invalidation through its effects capability. Socket and in-process callers receive the wire result without projection, worker, lock, or post-commit knowledge. | Verified behavior preserved by the Milestone 3 completion pass |
| Requeue and pre-write withdrawal coordination are behind the messaging Interface | `WorkspaceMessaging::requeue` and `WorkspaceMessaging::withdraw_notification` own the durable mutation and the resulting notification scheduling, exact cancellation, FIFO advance, and unread invalidation. Socket and in-process callers receive only protocol outcomes and do not coordinate the publication lock or workers. | Verified behavior preserved by the Milestone 3 mutation-family pass |
| Alarm administration and attention selection are behind the messaging Interface | `WorkspaceMessaging` owns body-free alarm projection, administrator clearance, exact target selection, ambiguity, and recipient privacy. The terminal-resolution mechanism receives the selected attempt through an internal handoff; socket callers receive protocol outcomes without mailbox records or lookup policy. | Verified behavior preserved by the Milestone 3 attention-family pass |
| Daemon status consumes one body-free messaging projection | `WorkspaceMessaging::status_snapshot` owns mailbox route fallback, unread counts, held attention, the bounded blocked-wake sample, active composer-candidate cardinality, durable binding comparison, mailbox-state mapping, recovery policy, and the finished next action. It also joins durable gating records to current route and working-state evidence before exposing body-free foreground-watch candidates. Status composition supplies current content-free pane evidence, retains the legacy session-ledger fold separately, and no longer reads mailbox projections, recovery variants, notification indexes, worker ownership, route lookup, or manifest activation policy directly. The process diagnostic knows only candidate and operating-system process facts. | Verified behavior preserved by the Milestone 3 status-family, status-composer, and diagnostics passes |
| Durable operations cannot construct or traverse the daemon root | The `cyclopsd` composition root constructs `WorkspaceMessaging` and implements its named post-commit effects capability. Daemon-root notification scheduling and terminal recovery are isolated in `messaging_runtime.rs`. The operation Module contains no `Inner`, pane-cache access, task spawning, delivery enqueueing, or pane observation, and a whole-file syntactic lint protects that boundary. | Verified behavior preserved by the Milestone 3 runtime-locality pass |
| External settlement follow-up is behind the messaging Interface | Delivery and attention mechanisms report a changed durable notification head to `WorkspaceMessaging`. The Module owns recipient FIFO continuation and the unread-before-schedule ordering for direct delivery; those mechanisms no longer receive `MailboxService` merely to call `schedule_recipient`. | Verified behavior preserved by the Milestone 3 runtime-host pass |
| Runtime evidence and post-commit work enter through the messaging Interface | Fusion, authenticated ACK handling, tmux event sources, and daemon lifecycle publish immutable route, pane-size, or availability evidence to `WorkspaceMessaging`. Delivery reports durable pre-write, attention, and notified outcomes, and the socket server reports an enabled force-submit setting. The Module owns pending-recipient, width-block, replay-reminder, and force-submit candidate selection and decides route reconciliation, reminder, exact-attention reconciliation, and force-submit consequences. A syntactic lint prevents ordinary callers from invoking those messaging schedulers directly; the composition adapter still hosts the retained mechanisms. | Verified behavior preserved by the Milestone 3 runtime-evidence pass and Milestones 3–4 responsibility-audit correction |
| Participant directory publication is behind the messaging Interface | Adoption, clear, attach, rebind, and process-replacement code supplies observed participant identities through an atomic `WorkspaceMessaging` publication boundary. The Module owns durable directory replacement and the synchronization shared with authenticated reads; ordinary participant callers no longer access `MailboxService` or `mailbox_publication`. Force-submit settings use the same authenticated Module caller. Existing concurrent-publication, duplicate-label, and stale-snapshot regressions plus deletion lints protect the boundary. | Verified behavior preserved by the Milestones 3–4 participant-directory correction |
| Authenticated consumption evidence enters through the messaging Interface | Hook handling publishes one immutable observation containing authenticated route, process binding, manifest, prompt, and causal time. `WorkspaceMessaging` owns exact registered-candidate lookup, durable binding and payload comparison, and the one-shot signal. Hook handling no longer accesses `MailboxService` or candidate storage, and a deletion lint protects the boundary. | Verified behavior preserved by the Milestone 3 audit correction pass |
| Durable pre-write block policy is behind the messaging Interface | Delivery supplies immutable physical evidence and receives a body-free recorded-or-obsolete result. `WorkspaceMessaging` owns publication synchronization, content-free readiness-route baseline synthesis, wake-block mapping, the durable transition, benign obsolete classification, the exhausted-supervisor transition, and the first post-commit route reconciliation. Delivery retains physical re-observation and worker fault ownership without reading journal variants or choosing messaging policy, and a deletion lint protects the boundary. | Verified behavior preserved by the Milestone 3 pre-write correction pass |
| Durable composer recovery is behind the messaging Interface | Fusion supplies immutable physical binding, screen, and exact-turn evidence and carries one opaque recovery plan. `WorkspaceMessaging` owns active-barrier lookup, exact claim comparison, recovery coordination, retirement policy and persistence, writer-uncertainty handling, and lifecycle, replacement, and pane-loss settlement. Delivery, runtime, and hook adapters can only track, bind, or settle an exact attempt through named Module operations, and a deletion lint prevents recovery records, variants, or coordinator state from returning to those callers. | Verified behavior preserved by the Milestone 3 recovery correction pass |
| Runtime composer projection is behind the messaging Interface | Fusion captures exact composer content between stable process bookends and supplies immutable semantic, route, binding, and safety facts through an opaque probe. `WorkspaceMessaging` owns active-candidate lookup and cardinality, durable attempt and binding joins, payload reconstruction, submission-state interpretation, and the finished body-free ownership projection. A deletion lint prevents raw candidate records, journal-state reasons, or payload reconstruction from returning to fusion. | Verified behavior preserved by the Milestone 3 composer-projection correction pass |
| Durable attention outcomes are behind the messaging Interface | The terminal-resolution mechanism retains exact route proof, capture, key execution, and evidence waiting. Resolution reservation, operator, automatic, and forced intent, accepted-action and consumption facts, final settlement, no-key discard, pre-key withdrawal, reservation release, and FIFO continuation are `WorkspaceMessaging` operations. A syntactic lint prevents terminal code from committing those mailbox mutations directly. | Verified behavior preserved by the Milestone 3 attention-commit pass |
| Exact-attention worker policy is behind the messaging Interface | `WorkspaceMessaging` selects active exact-owned candidates, owns evidence coalescing, worker election, automatic resolution choice, conflict parking, and re-election. The composition adapter hosts the elected task and asks the terminal mechanism to perform one exact action; terminal code no longer scans messaging projections, manipulates reconciliation locks, or spawns workers. | Verified behavior preserved by the Milestone 3 attention-runtime pass |
| Terminal attention support is behind the messaging Interface | `WorkspaceMessaging` rebuilds the expected payload, resolves the current route through its effects adapter, selects runtime targets, and owns boot-local consumption registration and deterministic cleanup. The terminal mechanism receives named results and no longer receives `MailboxService`, reads message rows, or owns consumption-candidate storage. | Verified behavior preserved by the Milestone 3 terminal-support pass |
| Exact-owned pane evidence leaves observation as immutable data | A state or composer edge now appends a typed exact-owned observation to the same ordered collection that can carry quota-reset evidence. The daemon composition root applies every item through `WorkspaceMessaging` before presentation. Fusion no longer selects exact-owned candidates, elects a worker, or invokes that consequence directly, and a deletion lint protects the boundary. | Verified behavior preserved by the Milestone 4 exact-owned evidence correction |
| Readiness route consequences leave observation as immutable data | Source recomputes append typed causal route evidence to the pane result before exact-owned and quota-reset observations. Cache-only hold mutations produce the same type and hand it to the composition root immediately. `WorkspaceMessaging` owns reconciliation; fusion no longer invokes it directly, and a deletion lint protects the boundary. | Verified behavior preserved by the Milestone 4 route-evidence correction |
| Composer recovery crosses an immutable evidence boundary | Fusion supplies binding, clean-composer, legacy-readiness, and recovered lifecycle-start evidence to a narrow composition-root boundary. The adapter delegates durable probing, reconciliation, retirement, and coordinator decisions to `WorkspaceMessaging`, then returns only the body-free barrier update needed by the serialized cache commit. Fusion and the physical recovery helper can no longer obtain the messaging Module or invoke its policy methods, and a deletion lint protects that boundary. | Verified behavior preserved by the Milestone 4 composer-recovery evidence correction |
| Dispatch ACK confirmation leaves observation as immutable data | Fusion returns exact route, process, manifest, turn, and causal-time evidence after the cache commit and any state event. The composition root applies that body-free evidence through the retained delivery mechanism before ordered messaging observations and presentation. Fusion can no longer access delivery handles or confirm their state, and a deletion lint protects the boundary. | Verified behavior and post-commit ordering preserved by the Milestone 4 dispatch-ACK evidence correction |
| Current and legacy delivery still coexist | Normal `msg.send` uses the mailbox path. Hook self-test and `Daemon::deliver_payload` cross the explicit compatibility boundary before the retained direct writer. | Repository and local-install census complete; external embedder use unverified |
| Historical replay has real obligations | Formats 1 and 2, original doorbells, incomplete bindings, legacy direct payloads, and the replay-only historical `Staged` to `Submitted` transition remain readable under explicit restrictions. | Verified compatibility obligation |
| Official transports share uncertainty knowledge | `cyclops-client` owns greeting, bounded frames, response correlation, timeout classes, refusal decoding, post-write uncertainty, and stream gaps for CLI, stream UI, and workspace callers. | Verified behavior preserved by Milestone 2 and physically isolated in Milestone 6 |
| Headless runtime and build independence differ | `cyclopsd` has no production UI dependency. The public `cyclops` binary depends on both UI crates. | Runtime independence verified; build independence incomplete |
| Reusable presentation consumes named adapters | `cyclops-client` owns Unix-socket mechanics, `events.backfill` keeps journal traversal in the daemon, and the launcher supplies the stream UI's optional pane-focus effect. `cyclops-ui` has no production dependency on `cyclops-ledger`, `cyclops-state`, or `cyclops-tmux`; dependency and source lints protect the boundary. | Verified presentation seam corrected by Milestone 6 |
| Hidden-view visibility is mixed | The full-screen workspace defers message snapshots while hidden and its collapsed rail is only a toggle. Adopted tmux panes can already show a body-free `✉ N` border count. A deliberately chrome-free native view has no such cue. | Verified full-workspace cue gap; broad “no compact cue” claim contradicted |
| Preview and terminal activation are coupled | Format 4 stages sender, preview, and claim locator, then submits that terminal payload. | Verified current behavior; preview-only activation is not built |
| Caller identity is process-derived | Kernel peer credentials and process ancestry resolve a vendor caller to one exact recipient, a proven outside shell to admin, and unprovable callers to denial. Requests carry no arbitrary sender. | Verified current security property |
| Production runner and MCP adapter do not exist | No production Agent Runner or messaging MCP adapter is present. Current identity does not prove host-specific ancestry through a future adapter. | Verified absence; compatibility unverified |
| Data lifecycle is incomplete | Current authority specifies append, replay, compatibility, and content-free privacy rules, but no complete retention, export, restore, deletion, or compaction product policy. | Verified contract gap, not permission to invent policy |
| CI concerns are separate | The three authorized CI tasks added cancellation and a reproducible baseline, deterministic ownership and focused relocated-root evidence, then explicit required, conditional, scheduled, and release lanes with retained performance metadata. | Corrected by the integrated CI workstream; messaging milestones still do not absorb broad CI redesign |

## 9. Corrected, fixed, contradicted, and unverified review findings

| Review finding | Current disposition | Correction or remaining uncertainty |
|---|---|---|
| Frame-size P1 | Corrected by Milestone 1 | Official daemon ingress and egress plus blocking and async clients share the 1,048,576-byte JSON-object limit, excluding the newline. Historical oversized rows remain readable. |
| Terminal-safety P1 | Contract wording corrected in this documentation pass | The existing late proof, occupant checks, composer hold, durable intent, and conservative recovery remain. No test currently inserts typing in the final pause, so no absolute exclusion claim is made. |
| Raw-tmux P1 | Authority gap corrected in this documentation pass | Product docs and the active skill now use one role-aware confirmation and authorization doctrine. No transport behavior changed. |
| Subscribe cursor | Corrected by Milestone 6 with compatibility preserved | The legacy input remains accepted but explicitly promises no replay. Current stream projections use daemon-owned `events.backfill`; mailbox progress uses snapshots and `messages.follow`. |
| Direct delivery is retired | Contradicted | Hook self-test and `Daemon::deliver_payload` remain live repository Interfaces. External embedder use is unknown. |
| Pane-only mode has no compact visibility | Contradicted broadly | Adopted tmux panes already have a body-free unread border count. The hidden full-workspace rail and explicitly chrome-free native journey still lack a compact cue. |
| Semantic source scans generally prove architecture | Corrected | The attention equivalence scanner admits it is not proof. Simple forbidden-dependency and ownership scans remain useful syntactic lints. |
| Tests are wholly chronology-shaped | Corrected | Chronology-named suites remain, but domain-named suites also exist. A contract census must precede moves or deletion. |
| Soak and performance all run in the correctness lane | Corrected | Some active measurement executables run there; true long soaks and frozen benchmarks are ignored or opt-in. |
| Current CI timing and residue counts | Superseded by the CI baseline and final comparison | The CI workstream recorded reproducible per-job and runner-minute evidence and retained performance metadata. A new run may differ with workload and GitHub capacity. |
| Preview grammar causes material rejection friction | Unverified | No rejection, edit, or quality evidence exists. Preserve the preview and measure before changing grammar. |
| Native host activation is reliable enough to productize | Unverified | No current probe proves prompt, progress, approval, cancellation, detach, reconnect, transcript, or unknown-outcome semantics. |
| Stdio MCP preserves exact pane identity | Unverified | A central process authenticates as itself. Per-agent process ancestry, stable idempotency keys, wait timeouts, and cancellation need a throwaway host-specific probe. |
| Long-history indexing is needed | Unverified | No refreshed cold replay, resident memory, snapshot, or follow-page measurement justifies an index. |
| P1 disposition after approval | All three approved corrections are integrated | Terminal wording and raw-tmux authority were repaired in the documentation authority PR; the bounded official frame contract shipped in Milestone 1. |

## 10. Resolution of WorkspaceMessaging versus immediate crate extraction

**Approved decision:** establish a deep internal `WorkspaceMessaging` Module
before considering a `cyclops-delivery-core` crate.

`cyclops-delivery-core` was earlier shorthand for the modular core that decides
how messages are accepted, stored, routed, claimed, replied to, recovered, and
prepared for notification. It and `WorkspaceMessaging` name the same modularity
goal at different stages, not competing product ideas. `WorkspaceMessaging` is
the clearer implementation name because “delivery” can blur durable message
acceptance, human notification, agent activation, and terminal effects.

The requirement is a deep messaging Module that removes messaging knowledge
from callers. It does not require a crate with the old name, a second messaging
system, or extraction before the responsibilities are understood. Do not spend
implementation time disproving or defending the old name.

Current `Inner`, fusion, mailbox projection, delivery engine, attention,
publication, and tmux effects are too entangled to define an honest independent
crate Interface today. Immediate extraction would either export daemon internals
or replace direct coupling with callback-heavy shallow Interfaces.

The useful part of [NEXT.md](NEXT.md) remains: pure decisions should converge on
the sans-IO shape `(state, input) -> (state, effects)`. That shape should first
live behind the internal `WorkspaceMessaging` Interface, where one durable
transaction and current behavior can remain intact. The Interface must not be a
new name around `Arc<Inner>`.

A later crate extraction requires separate evidence of at least one of these:

- independent build or release isolation;
- reuse by a second real consumer without exporting daemon internals;
- a failure-isolation boundary that changes recovery behavior usefully;
- a dependency graph that is materially simpler; or
- additional caller knowledge deletion that an internal Module cannot provide.

Packaging alone is not evidence. Until one criterion is demonstrated, crate
extraction is not authorized. An internal Module comes first. A crate requires
separate approval after evidence shows that it deletes additional caller
knowledge or provides measurable isolation that the internal Module cannot.

Before structural refactoring, the approved work repairs documentation and
unifies the raw emergency doctrine. The bounded frame correction is then the
first production milestone and is a reliability prerequisite, not the
`WorkspaceMessaging` extraction. These corrections do not authorize a delivery
redesign.

## 11. Preserved-behavior ledger

| Behavior or guarantee | Authority | Current evidence | Must remain? | Allowed implementation change |
|---|---|---|---|---|
| Durable acceptance before success | `DELIVERY.md`, `PROTOCOL.md` | Store append and sync precede projection commit and acceptance response | Yes | Store representation may change only with equivalent crash evidence and migration |
| Idempotent retry | `PROTOCOL.md` | Stable client keys converge; conflicting reuse is refused | Yes | Key storage and lookup may move behind `WorkspaceMessaging` |
| Exact participant and recipient identity | `INVARIANTS.md`, `PROTOCOL.md` | Stable recipient key plus process generation and route binding | Yes | Directory representation may change; display labels may not become authority |
| Recipient FIFO | `DELIVERY.md` | One exact recipient’s notification attempts retain order | Yes | Worker topology may change if observable FIFO and independence remain |
| Authenticated claim | `PROTOCOL.md` | Kernel peer and process ancestry authorize exact retrieval | Yes | Authentication mechanics may be wrapped, never replaced by a request sender field |
| Reply ancestry | `PROTOCOL.md` | Reply derives recipient and thread from the durable parent | Yes | Internal indexing may change without changing ancestry |
| Replay and torn-tail behavior | `INVARIANTS.md`, `DELIVERY.md` | Strict replay, sealed torn final line, visible mid-history corruption | Yes | Checkpoints or indexes require measurement and replay equivalence |
| Honest unknown external effects | `INVARIANTS.md`, `DELIVERY.md` | Intent precedes write; ambiguous post-write outcomes do not auto-retry | Yes | State names may hide behind a deep Interface; uncertainty cuts must remain distinct |
| Body-free invalidation | `PROTOCOL.md` | `messages.changed` carries projection identity and sequence, not content | Yes | Event transport may change; bodies and previews stay out of generic invalidations |
| Authoritative snapshot and follow recovery | `PROTOCOL.md` | Snapshots reconstruct current state; durable follow uses its own cursor | Yes | Client recovery code may consolidate; event hints must not become truth |
| UI-independent messaging | Direct user goal, `DELIVERY.md` | Daemon and CLI mailbox path operate without either UI | Yes | Public packaging may improve; UI cannot become a runtime dependency |
| Bounded sender-authored preview | `DELIVERY.md`, direct user goal | Format 4 stores and stages the sender’s bounded two-sentence summary | Yes | Exact grammar may change only after measurement; purpose and bound remain |
| Operator-controlled raw-tmux emergency | `README.md`, `DELIVERY.md` | Manual path is outside receipts and never automatic after uncertainty | Yes | One role-aware procedure may unify docs and skill; no synthetic facts |
| Process-derived caller identity | `INVARIANTS.md`, `PROTOCOL.md` | Peer credentials and ancestry decide vendor, admin, or denial | Yes | Adapters may inherit identity only after host proof; no claimed sender input |
| `Daemon::deliver_payload` compatibility seam | Milestone 5 repository and local-install census complete; external callers remain unverified | Nine repository test/support call sites and possible embedders may use the in-process bypass | Compatibility-sensitive; support status unverified | Preserve the exact seam; a later removal needs separate external-support evidence and approval |
| Compatibility obligations | `DELIVERY.md`, `PROTOCOL.md` | Old doorbells, Formats 1 and 2, incomplete bindings, direct payloads, and replay-only transitions remain readable | Preserve every currently readable journal format throughout this refactor; this is not an indefinite promise | Old writers may later be quarantined or retired; readers and fixtures remain until a separately approved history boundary |
| Data lifecycle | Interim operator rule | Append and replay are specified; full retention, export, restore, deletion, and migration policy is deferred | No silent deletion, truncation, or rewriting | A breaking migration requires an explicit export or migration path; a complete lifecycle policy needs separate approval |
| Human-input safety | `INVARIANTS.md`, `DELIVERY.md` | Fresh positive composer and occupant evidence, late proof, durable intent, exact verification, conservative recovery | Concrete goal and guards: yes. Absolute “always”: no | Correct wording and add characterization; absolute safety needs cooperative input ownership |
| Zero polling | `INVARIANTS.md`, `ARCHITECTURE.md` | Product transitions ride events; timers are bounded one-shots and reconnects | Yes | New waits or reconciliation need a named event or measured exception |
| Content privacy in generic evidence | `DELIVERY.md`, `PROTOCOL.md` | Message bodies are durable product data; raw screen captures and bodies do not enter invalidations | Yes | Authorized projections may change; broadcast content and captured screens remain forbidden |
| Compact public milestones | Current protocol and target synthesis | Accepted, notification outcome, attention, claimed, and replied are distinct | Yes | Internal states may remain detailed; public states must not invent execution or completion |
| Messaging visibility choices | Direct operator decision | Full workspace rail, adopted-tmux body-free count, and manual inbox inspection serve different journeys | Preserve all three: a stateful collapsed Messages rail, existing body-free tmux border count, and intentionally chrome-free mode | Improve one journey without forcing chrome or deleting another |

## 12. Proposal disposition table

| Proposal | Status | Current evidence | Rationale | Prerequisites | Revisit condition |
|---|---|---|---|---|---|
| Preserve the durable messaging spine | Approved | Durable-first send, exact claim, reply, replay, and uncertainty are verified | These are product value, not scar tissue | Preserved-behavior tests | Only with a separately approved product change |
| Keep one local coordinator and modular monolith | Approved | Current local transaction and event model is appropriate | Smallest shape satisfying durability and recovery | Deep internal Interfaces | A measured multi-host requirement |
| Internal deep `WorkspaceMessaging` first | Approved | `Arc<Inner>` reach-through and cross-domain post-commit work are verified | Deletes knowledge before packaging | P1 corrections; one operation family at a time | Failure of the deletion test |
| Separate notification and activation semantics | Approved | Current notification does not prove claim or execution; activation is absent | Prevents false progress claims | Keep public activation vocabulary unbuilt until host proof | A host pilot with proven outcomes |
| Shared Daemon Client | Approved | Three official transports duplicate framing and uncertainty | One deep Interface prevents further drift | Frame tracer bullet | If blocking and async requirements cannot share semantics honestly |
| Incremental removal of `Arc<Inner>` reach-through | Approved | Server, messaging, fusion, and delivery receive broad daemon state | Locality improves only one path at a time | Narrow Interfaces and unchanged traces | If a proposed slice exports equivalent internals |
| Pure presentation models | Approved | UI code knows socket, journal, terminal, and tmux mechanisms | Views should reconstruct from authorized projections | Honest snapshot/follow/event contracts | If a mechanism is proven to be an unavoidable view responsibility |
| Honest subscribe, follow, and snapshot contracts | Approved | Subscribe cursor is ignored; follow and snapshots already have distinct roles | Hints, durable progress, and truth need separate Interfaces | Cursor compatibility decision | A real heterogeneous replay journey |
| Frame-size correction | Approved | Accepted data can exceed an official UI’s envelope | Verified P1 and first production tracer | Shared limit is 1,048,576 JSON-object bytes, excluding the newline | Measurement supports a separately approved bounded ceiling |
| Correct terminal-safety wording | Implemented in the documentation prerequisite | Code documents the residual proof-to-paste interval | Truthful wording strengthens rather than weakens safety | Preserve current guards; characterize the residual race before stronger claims | Cooperative queue or lease eliminates the interval |
| One raw-tmux doctrine | Implemented in the documentation prerequisite | README, delivery contract, and active skill now state the same operator exception | One role-aware recovery contract removes ambiguity | No production transport change | A recorded emergency transport is designed separately |
| Legacy compatibility quarantine | Approved | Self-test, embedder seam, old writers, and replay obligations coexist | Make compatibility visible before deleting anything | Repository and installed-caller census; replay fixtures | Census proves a writer or reader obsolete |
| Pane-only visibility | Approved | Workspace rail lacks state; adopted tmux border already has unread count | Preserve a stateful collapsed Messages rail, existing body-free tmux border count, and intentionally chrome-free mode with manual inbox inspection | Authorized snapshot-derived cue design | A separately approved product change |
| Preserve preview; measure grammar | Approved | Preview serves orientation; friction evidence is absent | Keep purpose while testing solution friction | Validation-failure and edit measurement | Evidence favors another bounded grammar |
| Headless client build independence | Approved | Runtime is independent; public binary build is not | Useful only after transport consolidation | Shared Daemon Client; packaging evidence | Build isolation has no measurable value |
| Production Agent Runner | Unverified | No runner exists and activation outcomes are unproved | Optional execution must not be guessed into the mailbox | One bounded host pilot and human visibility trace | Pilot proves identity, cancellation, reconnect, and uncertainty |
| Production host adapters | Unverified | Tmux notification is not a general host activation contract | A seam needs real implementations, not a hypothetical callback set | One tmux and one native-host probe | Two paths share a stable, smaller Interface |
| Stdio MCP adapter | Unverified | Native identity is proven; adapter ancestry and host timeouts are not | MCP may be an access adapter, never the messaging system | Throwaway identity, idempotency, wait, timeout, and cancellation probe | Every supported host attributes the exact caller |
| Keep current hybrid pub-sub | Approved | Requests, durable journal, body-free hints, durable follow, and snapshots already separate concerns | Generic replay bus adds no current value | Honest Interface cleanup | A measured requirement the hybrid cannot satisfy |
| History index or checkpoint | Deferred | Current long-history cost was not refreshed | Optimize only after user-visible measurement | Cold replay, memory, snapshot, and follow measurements | A bound is exceeded |
| Progressive onboarding rewrite | Approved | Current reference material is comprehensive but large | A short golden path can reuse the same authority | Architecture approval, no aspirational behavior | User-facing journey changes |
| CI superseded-run cancellation | Approved | No workflow concurrency rule exists | Independent cost and feedback improvement | Preserve manual and release proof runs | Workflow ownership changes |
| Remove duplicate relocated full suite | Approved after replacement evidence | Full Rust evidence reruns under `CYCLOPS_TEST_TMP`; focused override test exists | Narrow property should have narrow proof | Broken-helper mutation must be caught by focused tests and lint | Replacement misses a protected defect class |
| Replace semantic source scans | Approved after Interface proof | Attention scan admits it is not runtime proof; some simple scans are valid lints | Architecture should make duplication inaccessible | Deep Interface and stronger domain trace | A syntactic rule still needs a simple lint |
| Reorganize tests by durable domain | Approved as later migration | Both chronology and domain suites exist | Names and moves alone do not prove consolidation | Contract and seam census; original-defect comparison | Stronger trace subsumes an old case |
| Retained performance history | Approved as scheduled evidence | Current CI prints measurements but retains no stable report | Trends need revision and environment facts | Stable workload and artifact format | Measurements show no decision value |
| Immediate `cyclops-delivery-core` extraction | Not authorized | Current dependency shape would export daemon coupling | Internal depth must precede packaging; the old term names the same modularity goal | Additional caller-knowledge deletion or measurable isolation plus separate approval | The approved criteria are demonstrated |
| Distributed broker | Deferred | No multi-host requirement | Adds operations and failure modes without value | Measured cross-host need | One coordinator cannot meet a real journey |
| Generic event bus | Rejected for current scope | Current hybrid already handles commands, hints, and durable recovery | Generic topics would blur truth and hints | None | New charter with a concrete missing journey |
| Automatic raw-tmux fallback | Rejected | It can duplicate ambiguous writes and invent authority | Unsafe and dishonest | None | Not revisited without an atomic external protocol |
| Generic workflow engine | Rejected | Messaging needs coordination milestones, not orchestration graphs | Expands the product without a user requirement | None | Separate product proposal |
| Broad rewrite | Rejected | Current durable spine is valuable and heavily evidenced | Small slices preserve behavior and reveal real seams | None | Not a valid fallback for difficult refactoring |

## 13. Legacy caller and replay questions

### Verified current census

- Normal socket `msg.send`, `msg.reply`, and claim operations use
  `MailboxService` and the current messaging coordinator.
- Hook self-test bypasses the mailbox through
  `compatibility::deliver_payload`. A private boundary capability prevents it
  or any new caller from reaching the retained direct writer accidentally.
- `Daemon::deliver_payload` remains an in-process direct-delivery Interface for
  tests and possible embedders. The repository has nine call sites across seven
  integration-test or test-support files and no production caller of that
  public method. A read-only search under `/Users/yahirh/projects` and
  `/Users/yahirh/Documents` found only Cyclops worktrees and one archived
  Cyclops source snapshot. An exact public GitHub code search found no other
  Rust definition. Those negative results do not prove that no external
  embedder exists, so public support status remains unverified and the method
  is preserved unchanged.
- Summaryless wire clients can still select Format 3 or canonical direct
  fallback. Current CLI sends Format 4.
- Formats 1 and 2, original doorbells, incomplete historical bindings, unknown
  numeric formats with restricted authority, legacy direct payloads, and the
  historical direct `Staged` to `Submitted` edge remain replay obligations.
- Session-ledger recovery still folds legacy delivery chains independently from
  workspace-owned notification attempts.

### Read-only retained-format census

Measured on 2026-08-29 across the seven NDJSON or JSONL files under the local
Cyclops state root, including its retained archive:

- all 575 lines parsed as JSON;
- the largest JSON object was 1,781 bytes, excluding its newline;
- kinds were 394 `state`, 135 `system`, 22 `gate`, 14 `msg`, and 10 records
  without a `kind` field;
- retained notification transitions included five Format 3 and eight Format 4
  writing facts;
- 130 records carried record version 1 and three resolution records carried
  proof version 1; and
- the census emitted only structural metadata. It did not emit, copy, or store
  message bodies.

This is evidence about the inspected local state, not permission to reject a
larger historical object or delete another readable shape. Formats 1 and 2,
original and summaryless doorbells, incomplete bindings, restricted unknown
numeric formats, direct payloads, historical `Staged` to `Submitted`, linked
session journals, and workspace-journal versions remain covered by their
existing readers and fixtures.

### Questions that block deletion or substantial compatibility changes

1. Are any installed clients still sending summaryless messages?
2. Does any external embedder call `Daemon::deliver_payload`?
3. Can hook verification use the current mailbox notification path without
   changing what it proves?
4. Which historical write formats should remain supported after this refactor,
   and which may later become import-only fixtures?
5. Which old states must remain readable but never writable?
6. Do any user journals contain frames above the proposed official envelope,
   and what read/export path must remain available for them?

Repository search cannot answer installed-client or retained-history questions.
A read-only census of local format metadata is permitted, but it must not
collect message bodies. Every format that is readable at the start of this
refactor remains readable throughout it. That preservation does not promise
indefinite compatibility. No legacy writer, reader, fixture, state, or
`Daemon::deliver_payload` behavior may be deleted or substantially changed
until its caller and history boundary are demonstrated.

## 14. Separate CI and test workstream

No broad CI redesign is a prerequisite for the first messaging milestone. That
milestone needs only its deterministic contract evidence and the existing
repository gates. CI changes remain separately reviewable.

| CI or test finding | Current classification | Decision for Milestone 1 |
|---|---|---|
| Duplicate full suite under `CYCLOPS_TEST_TMP` | Independently actionable after replacement mutation evidence | Out of scope; do not delete the duplicate leg yet |
| Superseded-run cancellation | Independently actionable CI improvement | Out of scope; may ship separately |
| Interrupted fixture cleanup | Independently actionable CI improvement | Out of scope; add exact run-id ownership separately, never broad cleanup |
| Semantic source-text scans | Later domain-test migration | Preserve simple syntactic lints; replace semantic scans only after an Interface makes duplication inaccessible |
| Chronology-shaped test files | Later domain-test migration | No moves, merges, or deletion based on names |
| Performance and soak in correctness lanes | Later migration plus scheduled evidence | Distinguish active correctness from measurement and true opt-in soak before moving anything |
| Full macOS matrix | Measurement required | Preserve until a named portability set proves equal fault detection |
| Zero doctests | Independently actionable CI improvement | Live bounded probe listed zero doctests; removal is still a separate gate/docs change |
| Unconditional website, installer, and tmux-HEAD jobs | Independently actionable path-selectivity improvement | Out of scope; keep stable required status semantics if changed later |
| Missing retained performance history | Scheduled evidence | Out of scope; add stable workload and environment facts later |

Tests may be removed only when a stronger, cheaper proof is demonstrated against
the original defect or durable contract. Count, line count, and naming are not
deletion evidence.

## 15. Sequenced independently verifiable milestones

Each milestone must leave the repository coherent if all later work stops.

| Sequence | Outcome | Independent exit evidence |
|---|---|---|
| Documentation prerequisite | Repair authority, navigation, terminal-safety wording, and the raw emergency doctrine without production changes | Documentation checker reports zero broken references and zero unindexed retained pages; review proves no production file changed |
| Milestone 1 | Enforce one bounded frame contract at every official ingress and egress | Fully specified in section 16 |
| Milestone 2 | Consolidate greeting, framing, correlation, timeout, uncertainty, gap, and recovery semantics behind one Daemon Client Interface | Blocking and async contract suites plus unchanged CLI, stream, and workspace journeys |
| Milestone 3 | Put one current durable operation family behind an internal `WorkspaceMessaging` Interface | Caller loses journal, projection, worker, and post-commit scheduling knowledge; current durable trace remains byte- and outcome-equivalent |
| Milestone 4 | Move one observation-to-messaging consequence family out of fusion | Pane Observer returns immutable evidence; a domain trace proves the same durable consequences and tmux behavior |
| Milestone 5 | Quarantine proven legacy writers and readers behind an explicit compatibility path | Caller census, replay fixtures, and restart traces define the supported history boundary |
| Milestone 6 | Complete honest snapshot/follow/event and pure presentation seams | UI rebuilds authorized state without reusable presentation code reading journals or tmux |
| Milestone 7 | Complete the missing full-workspace collapsed cue while preserving existing pane border and chrome-free choices | Full, compact, hidden, adopted-tmux, detach, and reconnect journeys agree without forced chrome or broadcast content |

### Completion rule for Milestones 3 and 4

The first operation family proves that a seam can carry real behavior. It does
not complete the responsibility migration by itself. Continue with additional
focused pull requests before Milestone 6 until:

- `WorkspaceMessaging` owns the durable messaging decisions assigned to it by
  this charter;
- ordinary callers no longer understand journal variants, projection internals,
  messaging locks, worker topology, or post-commit scheduling;
- observation returns immutable evidence instead of executing messaging policy;
- remaining `Arc<Inner>` use cannot let messaging code reach unrelated daemon
  state merely for convenience; and
- a fresh architecture audit finds no material messaging responsibility in the
  wrong module.

This is a responsibility and locality gate, not an instruction to remove every
`Arc`, invent a perfect abstraction, or move unrelated daemon behavior. Each
additional pull request remains a focused, independently reviewable slice.
Presentation ownership of sockets, journals, tmux, or messaging mechanisms is
the corresponding exit gate for Milestone 6.

Runner, host-adapter, and MCP work are research gates, not production milestones
in this sequence. They enter a later charter only if their probes pass.

### Branch and pull-request workflow

The beta rework is developed through the existing repository and must not be
developed directly on `main`. **beta/messaging-rework** is the remote integration
branch. Routine milestone commits do not go directly to that branch.

1. **docs/messaging-refactor-authority** was the completed bootstrap exception
   that approved and repaired this charter in PR #101.
2. **beta/fix/frame-contract** implements only Milestone 1.
3. **beta/refactor/daemon-client** consolidates official transport semantics only
   after Milestone 1 is accepted.
4. **beta/refactor/workspace-messaging** introduces one narrow
   `WorkspaceMessaging` operation family and proves that callers lose
   knowledge. It does not extract a crate without separate approval.
5. **beta/refactor/observation-messaging** separates observation from messaging
   responsibility.
6. **beta/refactor/legacy-compatibility** quarantines compatibility-sensitive
   legacy paths after their caller census.
7. **beta/refactor/presentation-seams** makes snapshot, follow, event, and
   presentation seams explicit.
8. **beta/feat/collapsed-messages-cue** adds the missing collapsed-workspace
   messaging cue.

Each milestone gets its own pull request into **beta/messaging-rework**,
regression evidence, review, and rollback point. Do not begin a later milestone
inside an earlier pull request. Keep the integration branch synchronized with
`main` through reviewed merges. Milestone pull requests may merge when their
required evidence, review, and CI are green. Do not merge the beta integration
branch into `main` or publish a release without operator approval.

After the approved beta scope is complete, run fresh architecture, regression,
performance, migration, and user-journey audits. Then open one final pull
request from **beta/messaging-rework** into `main`.

### Beta release intent and naming gate

This is the Cyclops Messaging Beta Rework, not a broad rewrite. The beta must
demonstrate that:

- messaging works without either UI;
- existing journals remain readable;
- official clients share framing and uncertainty semantics;
- notification and activation remain optional;
- pane-only and hidden-sidebar journeys remain understandable;
- preserved messaging contracts have focused regression evidence;
- performance and reliability are measured against the existing system; and
- the modular structure is easier to explain and change.

Release identity is unresolved. Verified on 2026-08-29, the newest remote tag
is `v0.2.0-beta`, created 2026-08-27; GitHub has no Release objects; and the
workspace version in the repository-root `Cargo.toml` is `0.1.0`. Reconcile the
version, tag, and release authorities before assigning or publishing a final
beta version number.

## 16. One fully specified first tracer bullet

### Milestone 1: bound the official daemon frame contract end to end

**Purpose.** Ensure every newly accepted official message and every official
daemon response is representable by every official client, without unbounded
line allocation.

**Current problem.** The UI accepts at most 1,048,576 bytes per JSON object,
excluding its newline. The daemon and blocking CLI do not share or enforce that
envelope. A new message can be accepted and journaled even though an official
UI will reject the resulting frame. Request and response reads can also
allocate without a protocol bound.

**Owning domain.** Daemon Client framing and the socket adapter. This milestone
does not move mailbox or delivery policy.

**Likely files.**

- `src/cyclops-proto/src/wire.rs` for one pure public size rule and error
  vocabulary;
- the then-current UI wire module, now
  `src/cyclops-client/src/lib.rs`, to consume that rule rather than own a
  local number;
- `src/cyclops/src/client.rs` and the body-source handling in
  `src/cyclops/src/main.rs` for bounded pre-write encoding and bounded reads;
- `src/cyclopsd/src/server.rs` for bounded ingress and egress before unbounded
  allocation or durable acceptance;
- `src/cyclops-workspace/src/app.rs` for the same exact event-frame envelope;
- the narrow existing protocol, client, server, and UI test homes; and
- `PROTOCOL.md` plus any exact-output documentation affected by the new usage
  error.

No new crate is justified for this milestone. A pure shared rule can remove the
limit and edge semantics from callers now. Milestone 2 can later consolidate the
remaining blocking and async reader implementations.

**Behavior that remains unchanged.**

- All currently valid in-envelope requests, responses, events, snapshots,
  sends, claims, replies, and follow pages keep their wire meaning and output.
- Durable acceptance, idempotency, identity, FIFO, claims, replies, replay,
  notification, and attention behavior do not change.
- The approved shared limit is 1,048,576 JSON-object bytes, excluding the
  newline.
- Existing journal rows are never rewritten or deleted. Internal replay remains
  able to diagnose historical oversized rows even when an official projection
  cannot emit them in one frame.
- A pre-write size refusal is known-not-sent. A disconnect or write failure
  after bytes may have left the client remains outcome-unknown under the current
  rule.

The only intentional behavior correction is above the official envelope: new
oversized requests are rejected before durable acceptance, official clients
name the bounded usage error before writing when possible, raw oversized socket
input fails closed without unbounded buffering, and oversized egress never asks
an official client to accept a frame beyond the same contract.

**Interface introduced or deepened.** One protocol-level `FrameContract` rule
owns the maximum JSON-object bytes, newline treatment, exact-bound acceptance,
oversize classification, and user-facing limit value. Socket readers and
writers remain adapters.

**Caller knowledge removed.**

- the UI-local `MAX_FRAME_BYTES` authority;
- the workspace’s independent interpretation of whether the newline counts;
- the CLI assumption that a whole stdin or file is safe to allocate and encode;
- the daemon assumption that any complete line is a supported request; and
- per-client invention of the numeric limit and exact-bound behavior.

**Deterministic regression evidence.**

1. A pure table proves one byte below, exactly at, and one byte above the JSON
   object bound, with newline treatment stated once.
2. CLI request encoding refuses above-bound input before any socket write and
   classifies it as known-not-sent.
3. Daemon ingress reads incrementally, accepts the exact bound, closes or
   returns a bounded refusal above it, and appends no message fact for the
   refused request.
4. Daemon egress never emits an oversized response or event; its bounded error
   or connection outcome is explicit and does not invent a successful result.
5. UI and workspace readers accept and reject the same boundary bytes.
6. A restart/replay fixture containing an old oversized row remains diagnosable
   and is not rewritten.

These tests use memory buffers or an isolated in-process Unix socket. They do
not boot tmux, run a model, use a real clock, or add a full-product journey.

**Narrow verification.** Run the named frame-contract tests in
`cyclops-proto`, `cyclops`, `cyclops-ui`, `cyclops-workspace`, and `cyclopsd`,
then the existing protocol/client tests that exercise hello, request, response,
event, snapshot, and follow framing. Demonstrate that the new tests fail against
the old uncapped daemon and CLI for the intended reason.

**Repository gates.** After narrow evidence is green, run the existing five
gates in repository order:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast
cargo test -p cyclopsd --all-targets --no-fail-fast
cargo test --workspace --doc
python3 scripts/check-doc-paths.py
./tests/e2e/parity-check.sh
```

The listed commands reflect the repository’s current split gate even though it
is commonly described as five core gates. Installer parity is required only if
an installer changes, which this milestone forbids.

**Rollback condition.** Revert the complete milestone if any in-envelope
official request changes meaning, an accepted row becomes unreadable after
restart, exact-bound behavior differs across clients, or the daemon cannot
enforce ingress before unbounded allocation.

**Stop conditions.** Stop and return to design if:

- retained histories contain oversized rows with no safe diagnostic or export
  path;
- the proposed rule would make `cyclops-proto` own socket or filesystem IO;
- an egress refusal cannot preserve honest response uncertainty;
- a new crate or public command becomes necessary;
- the change expands into general Daemon Client consolidation; or
- deterministic coverage requires real tmux or broad test restructuring.

**Risks.** The JSON object versus newline boundary can drift; JSON overhead can
make a body smaller than the final request still too large; responses and
snapshots can exceed the bound even when each message does not; and a historical
oversized record needs honest recovery rather than silent truncation.

**Explicit non-goals.** This milestone does not consolidate transports, create
`WorkspaceMessaging`, change the preview grammar, change durable formats,
quarantine legacy delivery, add an index, alter UI layout, redesign CI, create a
crate, or implement a runner, host adapter, or MCP adapter.

## 17. Regression evidence and repository gates

Every later milestone must name one durable or user-visible contract, show the
new evidence failing against the pre-change implementation for the intended
reason, and use the least expensive honest seam. Pure decisions do not boot
tmux. Tmux claims use an isolated real tmux server. Race claims use existing
fault cuts or explicit events rather than sleeps.

The minimum evidence stack is:

1. a domain trace for the changed decision;
2. an adapter contract only when an external mechanism changed;
3. one focused process trace when durability, credentials, socket ordering, or
   crash recovery changed;
4. one real-tmux journey only when terminal behavior changed; and
5. existing documentation parity when exact output or command shapes changed.

Each retained test must have one reason to fail, own every resource it creates,
and state what would make it obsolete. A test is not deleted until its
replacement is demonstrated against the original defect.

After narrow tests, production milestones run the repository gates quoted in
section 16 with `--no-fail-fast`. Tmux tests run outside tmux. A change to either
installer also requires byte-identical installer copies and parity with
`--with-installer`. Website checks apply only to an explicitly approved website
change.

No full product suite was run to produce this charter. The only live Cargo probe
listed doctests and reported zero across the workspace.

## 18. Rollback and stop conditions

Every implementation milestone must be revertible as one coherent slice. It may
add optional fields or readers before changing writers, but it may not create a
half-migrated durable state that requires the next milestone to work.

Rollback the active milestone when preserved public behavior, replay, identity,
FIFO, claim authorization, uncertainty, human-input guards, or UI independence
regresses. Restore the previous production path and keep any additive reader
that is required to read facts already written.

Stop the active milestone and request a new decision when:

- the deletion test fails and callers still know the moved internals;
- a supposedly internal Interface requires a breaking wire or journal change;
- compatibility callers or retained histories cannot be enumerated safely;
- a new crate, daemon, process, generic framework, or automatic fallback becomes
  necessary;
- deterministic evidence cannot distinguish the intended behavior from fixture
  timing;
- the change needs unrelated CI, test, UI, website, installer, or cleanup work;
- host evidence cannot distinguish known-not-executed from outcome unknown; or
- production code would need to outrun this charter’s approved milestone.

## 19. Deferred work and non-goals

Deferred pending measurement or a separate approval:

- long-history indexes and checkpoints;
- preview grammar changes;
- headless packaging after Daemon Client consolidation;
- a production Agent Runner;
- tmux and native Agent Host implementations;
- a stdio MCP adapter;
- data retention, export, restore, deletion, and migration policy;
- full CI lane redesign, matrix narrowing, and test consolidation;
- retained performance infrastructure;
- large-fleet and multi-host coordination; and
- low-frequency health reconciliation without a current event.

Explicit non-goals:

- a distributed broker or multi-host mesh;
- a generic event bus or generic replay log;
- a generic workflow engine;
- automatic raw-tmux fallback;
- retry after an ambiguous external write;
- forced messaging chrome;
- a public Interface per domain noun;
- immediate `cyclops-delivery-core` extraction;
- broad file movement or rewrite;
- deletion based on module size or test count; and
- weakening identity, durability, replay, FIFO, claims, uncertainty, or
  human-input guards to simplify code.

## 20. Documentation authority repair before Milestone 1

At approval, the documentation checker reported 11 broken references and four
unindexed pages. Authority and navigation must be coherent before production
implementation. The authority prerequisite is:

1. record the approval decisions in this charter;
2. fix heading, fact, and status inconsistencies;
3. rewrite `NEXT.md` as the thin current execution queue;
4. make `HANDOFF.md` the single documentation front door;
5. restore the general architecture review method;
6. label supporting, proposed, historical, and superseded documents clearly;
7. move unrelated Research Synthesis and Research Library material out of this
   repository;
8. correct terminal-safety wording, publish one raw-tmux emergency doctrine in
   the shipped Cyclops skill, and synchronize the separately installed copy;
9. run the documentation checker until it reports zero broken references and
   zero unindexed retained pages; and
10. review and commit the authority and shipped-skill change.

The architecture reviews remain supporting design records because they explain
the reasoning behind this charter. Historical status metadata is preferred to
broad physical movement when moving a page would create link churn. Git already
preserves the superseded contents of `NEXT.md`.

`ARCHITECTURE.md` remains the current architecture contract. Update it only
when an approved production change alters current behavior, including any later
subscribe-cursor or target-architecture implementation.

## 21. Recorded operator decisions

1. `WorkspaceMessaging` precedes crate extraction. Do not create
   `cyclops-delivery-core` unless the internal Module later proves that a crate
   deletes additional caller knowledge or provides measurable isolation, and
   the extraction receives separate approval.
2. Milestone 1 is the bounded official daemon frame contract with a shared
   limit of 1,048,576 JSON-object bytes, excluding the newline. It is a
   reliability prerequisite, not the `WorkspaceMessaging` extraction.
3. Preserve `Daemon::deliver_payload`, label it compatibility-sensitive with
   unverified support status, and do not remove or substantially change it
   before the caller census finishes.
4. Preserve every currently readable journal format throughout this refactor.
   Do not promise indefinite compatibility. A read-only census of local format
   metadata is allowed and must not collect message bodies.
5. Preserve a stateful collapsed Messages rail, the existing body-free tmux
   border count, and an intentionally chrome-free mode with manual inbox
   inspection.
6. Complete the documentation authority repair before Milestone 1.
7. Defer the complete data-lifecycle policy. Until it is approved, allow no
   silent deletion, truncation, or rewriting, and require an explicit export or
   migration path for breaking migrations.

Start Milestone 1 in a fresh implementation session with this narrow prompt:

> Implement only Milestone 1 from the approved Messaging Refactor Charter: the
> end-to-end bounded official daemon frame contract. Do not begin Daemon Client
> consolidation, WorkspaceMessaging extraction, crate extraction, UI redesign,
> CI restructuring, legacy deletion, MCP work, or later milestones. Preserve
> historical replay and honest uncertainty. Stop if any charter stop condition
> is encountered.
