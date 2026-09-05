# Cyclops whole-system architecture review

> Supporting design record. This review covers the whole Cyclops product.
> It does not replace current behavior contracts or authorize implementation.
> The approved Messaging Refactor Charter controls accepted Track A behavior.
> The approved [Cyclops beta charter](CYCLOPS_BETA_CHARTER.md)
> controls the remaining beta work when this review and the charter differ.

**Current-state note, 2026-08-30:** The findings were reverified against
integration revision `9ad18c288fd8081ea986300fe597089e8a645fca`. Track A is
accepted. F4's original presentation-mechanism finding was resolved by Track A;
the remaining focus defect is F2 in the launcher adapter. F13 no longer implies
that Linux has no evidence, but its named cold-start, replay, memory,
concurrency, first-handoff, rollback, cross-platform, and wake-count
measurements remain open. The charter records current dispositions and
sequencing. The reviewed revision and historical measurements below are kept
unchanged as the evidence basis for this record.

- Review date: 2026-08-30
- Reviewed branch: **beta/messaging-rework**
- Reviewed revision: `1d2a6b54c0704ca5cf2cd3797873d27f8551c168`
- Pending work excluded from the reviewed revision: PR #113
- Primary principles: Unified Context, domain-driven responsibility, simple
  modular design, reliable systems behavior, and user experience as correctness

## Executive verdict

Cyclops is not a bad system. Its strongest mechanisms are better than the
average pre-release agent tool:

- durable acceptance precedes success;
- identities are tied to processes and exact pane routes;
- messages are ordered per recipient;
- claims and replies are authenticated;
- journals are append-only and replayable;
- terminal writes require fresh positive evidence;
- uncertain external effects remain uncertain;
- tmux integration is event-driven instead of polled;
- queues and UI ingress paths are bounded;
- user-visible state avoids color-only meaning; and
- the CLI, daemon, native tmux path, watch UI, and full workspace can operate in
  useful combinations.

Cyclops is also architecturally uneven. The messaging rework is repairing one
important vertical slice, but it is not a whole-product refactor. The remaining
concentrations of responsibility make ordinary changes harder than they should
be:

1. The full workspace application owns interaction, rendering coordination,
   tmux continuity, files, Messages, Stream, persistence, and recovery state.
2. The daemon composition state remains reachable by observation, delivery,
   presentation support, lifecycle, and compatibility implementations.
3. Reusable presentation code still performs socket, ledger, filesystem, and
   tmux work.
4. The public `cyclops` binary depends on both full-screen UI crates, so
   headless operation is runtime-independent but not build-independent.
5. Setup, seeding, update, rollback, health, cleanup, and vendor wiring repeat
   ownership and lifecycle knowledge.
6. The word `workspace` names several different domain concepts.
7. The source-only install path conflicts with the stated sixty-second
   first-delivery goal.
8. Long-history replay, cold start, concurrent messaging saturation, Linux
   performance, and idle wake counts do not have current measurements.

The correct response is not a rewrite and not a new crate for every noun.
Cyclops should remain a modular monolith with one local coordinator. The beta
should deepen a small number of domain modules, remove knowledge from callers,
and make the user journey the acceptance test.

The most urgent verified user-facing defect found in this review is narrower
than the large refactors: the standalone watch UI jumps to a pane with
`cyclops_tmux::focus_pane(None, None, target)`. It ignores configured
`tmux_socket` and `tmux_config` values. A user watching a non-default tmux
server can see the correct daemon state and then have the focus action target
the wrong server or fail. Presentation should request a semantic focus action
through an owner that knows the active tmux context.

## Authority and scope

Current behavior remains authoritative in this order:

1. Current code and focused regression evidence.
2. [Invariants](../INVARIANTS.md),
   [delivery](../DELIVERY.md), and
   [protocol](../../reference/PROTOCOL.md) contracts.
3. [Engineering map](../HANDOFF.md) and [status](../../../STATUS.md).
4. The approved
   [Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md) for
   the active messaging track.
5. Supporting architecture and CI reviews as design evidence.

This document is a proposal. It deliberately does not change `NEXT.md`, approve
new production work, or broaden the current messaging pull requests.

The review covers:

- messaging and coordination;
- runtime observation and attention;
- workspace interaction and tmux control;
- presentation and user experience;
- agent manifests, hooks, skills, and runtime integration;
- CLI entrypoints and headless use;
- installation, update, rollback, health, and cleanup;
- configuration, persistent state, history, and user data;
- CI, tests, performance, release evidence, and documentation authority; and
- the syncs connecting those responsibilities.

It does not approve MCP, a background model runner, multi-host messaging, a
distributed broker, automatic raw-tmux fallback, or a generic workflow engine.

## Review method

The review follows Unified Context rather than treating current code as the
ideal:

1. Define the user outcomes without referring to the implementation.
2. Walk representative user journeys, including setup, failure, recovery, and
   leaving the system.
3. Identify domain responsibilities, their state, invariants, failure modes,
   dependencies, and reasons to change.
4. Apply the house test: one room has one coherent purpose, and interaction
   happens through visible doorways rather than shared drawers.
5. Inspect code ownership, dependency direction, mutable state, IO, tests, and
   current documentation against that model.
6. Apply the deletion test to proposed modules: a module earns its existence
   only when deleting it would spread knowledge back across callers.
7. Measure before recommending performance work.
8. Separate verified findings, inferences, proposals, and unverified risks.

No `CONTEXT.md` or architecture decision record directory exists in the
reviewed tree. The domain language below is therefore derived from current
contracts, code, guides, and direct product intent. It should be approved
before a broad naming migration.

## The product outcome

Cyclops should let one person coordinate several terminal agents without
requiring them to surrender tmux, trust invisible delivery, or monitor every
pane continuously.

The product succeeds when:

- an agent can send, wait, claim, reply, and recover without opening a UI;
- a person can understand who is doing what and what Cyclops has actually
  proven;
- hidden sidebars or pane-only use do not make messaging disappear;
- input remains responsive while agents produce output;
- accepted work survives daemon, UI, or terminal failure;
- one broken integration does not make durable messaging unavailable;
- setup and recovery explain what happened and the next safe action;
- an engineer can find the one room that owns a rule; and
- performance is measured against user-visible workloads rather than guessed.

## User journeys that define correctness

### Journey 1: first install and first handoff

1. The user installs Cyclops.
2. Cyclops explains every external change and asks before vendor wiring where
   consent is required.
3. The user opens a workspace without learning daemon, journal, manifest, and
   tmux internals.
4. Cyclops explains an unknown agent with one direct repair action.
5. Two agents exchange a durable message.
6. The person can tell accepted, notified, claimed, replied, and completed
   apart.

Current strengths:

- installation avoids `sudo`;
- edits are backed up or preserved;
- `setup check`, `health`, and direct error copy expose repair actions;
- bare `cyclops` is the recommended front door; and
- presets provide a low-decision starting shape.

Current friction:

- the installer builds from source and documents that the build takes minutes;
- Rust is a product dependency for installation;
- the stated goal is sixty seconds from install to first delivery;
- hook and skill wiring spans several vendor homes and consent states; and
- version identity is split between Cargo `0.1.0`, tag `v0.2.0-beta`, and no
  GitHub Release object at review time.

### Journey 2: daily supervised coordination

1. The user opens the workspace.
2. Panes stay responsive while several agents emit output.
3. The user changes focus, layout, Messages visibility, and workspace tabs.
4. A message arrives while Messages is hidden.
5. The user understands the cue without being forced into messaging chrome.
6. A reconnect restores current state without duplicating messages or losing
   drafts.

Current strengths:

- rendering, input, hydration, and reconciliation are fast in measured paths;
- the event loop uses bounded priority lanes;
- render and decoration bursts coalesce without starvation;
- the workspace retains user choices for sidebar, tabs, Messages, and files;
- uncertain message sends preserve idempotency keys and drafts; and
- terminal ownership is restored on exit.

Current friction:

- the workspace application holds many unrelated kinds of state;
- the missing collapsed-workspace cue is still an approved pending milestone;
- workspace, watch, and CLI presentation share models but not one clean
  projection seam; and
- a feature can require coordinated edits across application state, tmux
  effects, render code, persistence, daemon calls, and large regression files.

### Journey 3: headless or native tmux use

1. The user starts or adopts a tmux session.
2. Agents use Cyclops messaging without the full workspace.
3. The user may watch only the panes or open `cyclops watch` separately.
4. Closing every UI leaves the mailbox and daemon useful.
5. Raw tmux remains a deliberate emergency action only after confirmed Cyclops
   failure.

Current strengths:

- this journey works at runtime;
- the daemon has no production dependency on either UI;
- pane previews and unread chrome support pane-only orientation; and
- durable acceptance never depends on a UI or model runner.

Current friction:

- the public CLI still compiles and links both UI crates;
- watch UI focus bypasses configured tmux context; and
- reusable UI startup code can read journals and the filesystem directly.

### Journey 4: failure and recovery

1. A daemon request times out after a write may have occurred.
2. The UI reports the outcome as unknown rather than failed or successful.
3. The user retries with the same idempotency key or performs authoritative
   recovery.
4. The daemon restarts and replays valid state.
5. Torn tails, process replacement, stale routes, and missing hooks are visible
   without inventing certainty.

This is Cyclops's strongest journey. Its contracts are unusually explicit.
The beta should simplify how those guarantees are implemented without reducing
them.

### Journey 5: update, rollback, and leaving

1. The user inspects the current installation.
2. An update stages and proves a matched CLI and daemon pair.
3. Operator-edited assets survive.
4. A failed update can roll back without touching durable history.
5. Uninstall removes owned executables and PATH changes.
6. The user can export or deliberately remove retained data.

Current strengths:

- update and uninstall are conservative;
- managed binaries use a validated pair and rollback design;
- cleanup defaults to dry-run;
- user edits outrank shipped guesses; and
- uninstall preserves durable history by default.

Current friction:

- lifecycle rules are distributed across setup, seed, update, health, cleanup,
  installer shell, and vendor wiring modules;
- complete removal requires manual vendor edits and deleting the entire
  Cyclops home; and
- no product-level export, retention, or explicit forget journey exists.

## DDD and the house test

DDD is useful here as a responsibility and language discipline, not as a reason
to create factories, repositories, or public interfaces for every noun.

The house story is the practical test:

- The mail room accepts, stores, orders, and releases messages.
- The observation room watches panes and reports evidence.
- The living room is where the person interacts with workspaces and agents.
- The windows present what is happening without owning the machinery outside.
- The utility room operates tmux, sockets, files, and terminal effects.
- The records room owns durable state and replay.
- The maintenance room installs, updates, inspects, rolls back, and removes
  managed assets.

Some rooms interact constantly. That does not make them one room. A doorway
passes an explicit fact or request. It does not let one room search another
room's drawers.

Two current examples make the analogy concrete:

- `cyclops-ui` directly invoking tmux focus is utility machinery stored in the
  presentation room.
- the workspace `App` holding pane runtime, view state, file browser state,
  message state, terminal state, persistence state, reconnect state, and effect
  workers resembles storing most of the house in the living room.

The opposite mistake is also possible. Splitting every field into a new module
would create a hallway of tiny closets. Keep behavior together when one
invariant requires it to move atomically.

## Proposed ubiquitous language

The current language is strongest around messaging and weakest around
`workspace`.

| Term | Proposed meaning | Current ambiguity to retire gradually |
|---|---|---|
| Participant | A human or agent identity that can author or receive durable communication | Often called agent even when it is the admin |
| Agent runtime | One vendor CLI process generation occupying a pane | Agent can also mean label, manifest, pane, or role |
| Route | The exact tmux session, pane, process generation, and recipient binding used for an effect | Display labels are sometimes treated as if they were routes |
| Message | Durable authored communication | Must not mean terminal notification |
| Notification attempt | Optional content-limited effort to make a recipient notice a message | Legacy direct delivery also uses delivery language |
| Claim | Authenticated retrieval of one message body | Must not imply model execution or task completion |
| Reply | Durable response linked to message ancestry | Must not be inferred from pane state |
| Attention item | Exact unresolved condition requiring a human decision | Must not be recomputed independently by every surface |
| Runtime observation | Immutable evidence about a pane, process, composer, hook, or terminal state | Must not perform messaging policy |
| Tmux session | The host runtime collection owned by tmux | Sometimes called a workspace |
| Interactive workspace | The full-screen human interface containing tabs, panes, Messages, Stream, and files | Also used for saved layouts and durable mailbox identity |
| Saved layout | Persisted desired tmux arrangement and naming hints | Currently called a workspace file |
| Cyclops home | The validated state root containing configuration, identities, journals, assets, and operational files | Sometimes confused with a workspace namespace |
| Coordination identity | The durable identity currently represented by `WorkspaceId` | The current name sounds like the interactive workspace or saved layout |

Do not begin with a mechanical rename. First approve these meanings, add a
small glossary, and rename only when a responsibility-changing milestone is
already touching the relevant interface.

## Target system shape

Cyclops should remain one local coordinator and a modular monolith:

```text
Human or agent intent
        |
        v
CLI / watch / workspace adapters
        |
        v
Daemon client interface
        |
        v
Local coordinator
  +-- messaging
  +-- runtime observation
  +-- session and participant directory
  +-- attention and recovery
  +-- presentation projections
        |
        +-- durable state adapter
        +-- tmux adapter
        +-- vendor hook adapter
        +-- terminal notification adapter
```

Installation and maintenance form a separate local module around the shipped
artifacts and state root. They do not belong inside messaging or presentation.

The target does not require:

- another daemon;
- a distributed broker;
- a generic internal event framework;
- a new crate for every domain room;
- replacing NDJSON with a database before measurements demand it;
- an always-running model process;
- MCP in the beta; or
- automatic raw-tmux fallback.

## Current architecture strengths to preserve

### Deep tmux adapter

`cyclops-tmux` owns product tmux invocation, control-mode correlation, flow
control, snapshots, hydration, layout, sizing, focus, and watcher behavior.
The interface gives high leverage and keeps vendor and messaging meaning out of
tmux mechanics.

The watch focus defect is not evidence against this adapter. It is evidence
that presentation chooses the adapter without the correct tmux context.

### Deep durable state primitives

`cyclops-state` provides descriptor-anchored, owner-only state access,
replacement durability uncertainty, bounded inspection, safe removal, and
socket cleanup. This complexity answers concrete security and data-loss risks.
It should not be simplified into ordinary path operations.

### Append-only ledger and honest replay

`cyclops-ledger` has a narrow responsibility: append, sync, sequence, cursor,
and torn-tail handling. Domain meaning remains outside. This is a good deep
module.

### Data-driven agent manifests

Vendor process names, visual rules, composer regions, launch commands, hook
capability, and priority are data. Adding a supported agent should remain a
manifest change whenever the existing manifest language can express it.

### Semantic themes and redundant meaning

Renderers consume semantic tokens. State meaning is not color-only. Plain mode
and glyph-plus-word presentation are product correctness, not decoration.

### Event-driven coordination

One long-lived tmux control client per watched session, daemon event
subscriptions, bounded channels, one-shot deadlines, and level-triggered
reconciliation avoid idle polling and repeated process startup.

### Messaging guarantees

Durability, exact identity, FIFO, idempotency, claims, reply ancestry, strict
replay, body-free invalidations, and honest uncertainty should remain release
gates.

## Severity model

- **P0:** known data loss, security violation, or product-wide correctness
  failure requiring immediate stop.
- **P1:** whole-product beta work. The product can run, but the defect or design
  concentration materially harms correctness, user experience, or safe change.
- **P2:** important follow-up that should be scheduled after the P1 seam exists
  or after measurement confirms the need.
- **Keep:** current design that has earned its complexity.

No P0 was verified in this review.

## Whole-system findings

| ID | Priority | Finding | Evidence | User consequence |
|---|---|---|---|---|
| F1 | P1 | The approved beta queue is a messaging track, not the whole Cyclops beta | `NEXT.md` explicitly excludes UI redesign and broad rewriting | Finishing seven milestones could be misreported as whole-product completion |
| F2 | P1 | Watch focus ignores configured tmux context | `cyclops-ui` calls `focus_pane(None, None, target)` | Correct state can be displayed while a click targets the wrong tmux server |
| F3 | P1 | Workspace interaction has low locality | `src/cyclops-workspace/src/app.rs` coordinates nearly every interactive concern | Small UX features cross many mechanisms and regression surfaces |
| F4 | P1 | Presentation still owns backend mechanisms | `cyclops-ui` reads bounded ledger tails, walks directories, opens daemon sockets, and invokes tmux focus | Views cannot stand alone as pure projections and can drift across surfaces |
| F5 | P1 | Headless operation is not build-independent | `cyclops` depends on `cyclops-ui` and `cyclops-workspace` | Headless users compile the full UI and installation pays unnecessary build cost |
| F6 | P1 | Daemon domains still reach broad shared state | `Inner` holds messaging, observation, registry, hooks, lifecycle, theme, chrome, workers, and workspace UI state | A change in one daemon room requires knowledge of unrelated locks and invariants |
| F7 | P1 | Domain language overloads `workspace` | The term names UI, saved layout, state identity, mailbox namespace, and tmux-oriented models | Humans and agents can place behavior in the wrong room while using locally reasonable names |
| F8 | P1 | First-run performance contradicts the product goal | Source build takes minutes; goals promise first delivery in sixty seconds | The first user experience fails before runtime performance can matter |
| F9 | P1 | Managed installation knowledge is distributed | setup, manifests, themes, skills, sounds, hooks, update, health, cleanup, and installer code each know ownership rules | Changes to shipped assets are difficult to prove consistently |
| F10 | P1 | Release identity is inconsistent | Cargo reports `0.1.0`, remote tag is `v0.2.0-beta`, and no GitHub Release object exists | Users and diagnostics cannot name one authoritative installed version line |
| F11 | P2 | Configuration authority is split | daemon config parses UI keys it does not use; workspace has its own settings and preference stores | New settings can acquire multiple parsers and unclear live-update semantics |
| F12 | P2 | User data has preservation but not a complete lifecycle | uninstall preserves state; complete export and deliberate forget remain manual | Users can keep data safely but cannot manage its full lifecycle through Cyclops |
| F13 | P2 | Important performance questions are unmeasured | no current cold-start, long-history, concurrent-message, Linux, or wake-count evidence | Optimization and capacity decisions would be guesses |
| F14 | P2 | Test locality follows large implementation modules | several source files contain thousands of lines of implementation and tests; some source-shape tripwires remain | Compile cost and failure interpretation stay broader than the domain contract |
| F15 | P2 | Vendor support evidence decays outside Cyclops releases | status records partial current-version evidence and untested Cursor state | A syntactically valid manifest can become operationally stale as vendor UI changes |

## Track A: messaging and coordination

### Current state

The active beta track has already merged:

- the bounded frame contract;
- a shared daemon client;
- the first `WorkspaceMessaging` seam;
- observation-to-messaging separation for quota-reset consequences;
- legacy compatibility quarantine;
- additional read, claim, requeue, and withdrawal responsibility moves; and
- CI foundation, deterministic fixtures, and evidence lanes.

PR #113 was still pending and is not included in the reviewed revision.

### Assessment

The direction is correct. `WorkspaceMessaging` is becoming a deep internal
module rather than a renamed file collection. The remaining work should finish
the approved deletion test:

- ordinary socket handlers should not understand mailbox projection variants,
  journal locks, worker topology, or post-commit schedules;
- observation should return immutable evidence;
- messaging should decide durable consequences;
- notification and activation should remain optional effects; and
- compatibility should remain explicit until caller and replay evidence permit
  removal.

### Beta requirements

- Finish the existing messaging milestones without adding whole-product work to
  their pull requests.
- Complete presentation seams for messaging state.
- Complete the collapsed Messages cue.
- Preserve readable journals and durable meaning.
- Do not create `cyclops-delivery-core`; it was an earlier name for this same
  modularity goal, not a separate product.

## Track B: runtime observation, identity, and attention

### Current state

`fusion.rs` combines pane-table evidence, process identity, manifests, hooks,
screen captures, composer ownership, lifecycle settlement, and cached
detections. It contains strong conservative rules, but parts of observation
still know messaging and recovery structures.

`Inner` holds detection caches, hook readings, hook lifecycle, turn ends,
process-name caches, recompute gates, lifecycle tasks, registry, session slots,
and messaging state together.

### Desired responsibility

The runtime observation module should answer:

> What immutable evidence exists about this exact pane route and process
> generation now, and how fresh and trustworthy is it?

It should not decide:

- whether a message is accepted;
- whether a notification is queued, withdrawn, or requeued;
- whether an attention item is cleared;
- how a UI renders the evidence; or
- which user-facing recovery action is chosen.

### Recommended deepening

1. Finish converting observation outputs into typed immutable evidence.
2. Group runtime caches and lifecycle task ownership behind one internal
   interface instead of passing broad daemon state.
3. Keep process-generation checks and conservative unknown states together.
4. Let attention consume facts rather than inspect fusion internals.
5. Keep screen capture as last-resort evidence and skip it when a cheaper sensor
   already decides.

### What not to do

- Do not build a generic sensor framework.
- Do not make one public interface per sensor.
- Do not move process or composer invariants away from the operation that must
  decide them atomically.

## Track C: workspace interaction and tmux continuity

### Current state

`src/cyclops-workspace/src/app.rs` is 12,589 lines in the reviewed tree; tests begin
around line 6,829. Line count is not proof of bad design, but the state and
call graph show responsibility concentration.

The application coordinates:

- workspace and tab models;
- pane runtimes and terminal cells;
- keyboard, paste, mouse, selection, drag, and menus;
- dialogs, settings, themes, sound, and notices;
- sidebar, file trees, Stream, Messages, composer, and detail state;
- daemon requests and event subscriptions;
- tmux control, sizing, hydration, focus, and reconnect;
- persistence and live preference changes; and
- rendering deadlines, coalescing, backpressure, and terminal restoration.

Many individual mechanisms are well designed. The problem is their locality.

### Desired responsibility

The interactive workspace should be understood as four cooperating internal
rooms:

1. **Workspace state:** tabs, panes, selected workspace, layout, and user
   preferences.
2. **Interaction:** user intent, focus, selection, drag, dialogs, and action
   legality.
3. **Runtime continuity:** tmux connection, hydration, sizing, flow control,
   reconnect, and authoritative reconciliation.
4. **Presentation:** pure view state for rendering workspace chrome and embedded
   panes.

The application loop remains the composition root. It may coordinate these
rooms, but it should not contain their policy.

### Recommended tracer sequence

1. Pick one user action family, such as pane focus and movement.
2. Express legality and resulting workspace state without tmux IO or rendering.
3. Return explicit effects for the tmux adapter.
4. Reconcile the authoritative result back into workspace state.
5. Render from the resulting view state.
6. Prove the old callers lose tmux and state-transition knowledge.

Repeat for layout mutation, workspace lifecycle, and reconnect only when each
previous slice is independently coherent.

### User experience acceptance

- A key or click has immediate visible acknowledgment.
- Failed tmux effects leave the workspace honest and recoverable.
- Reconnect preserves focus, layout intent, drafts, and visibility choices.
- Hidden Messages and sidebar modes remain understandable.
- The same action has the same name in menus, help, errors, and docs.
- Narrow terminals show what a destructive action will do before accepting it.

## Track D: presentation and user experience

### Current state

`cyclops-ui` contains valuable shared models for Stream, Messages, attention,
grid layout, detail, composers, and avatars. It also contains:

- daemon connection and subscription logic;
- bounded ledger backfill and filesystem traversal;
- action request IO;
- terminal lifecycle; and
- direct tmux focus.

The full workspace embeds many of the models but maintains its own application
state and additional daemon workers.

### Desired responsibility

Presentation modules should transform authorized state into human-readable view
state and user intent. They should not know:

- where journals live;
- how a socket greets;
- which tmux socket is configured;
- how a pane is captured or focused;
- how durable mutation is committed; or
- how a retry is made safe.

IO remains necessary. It should live in adapters beside the application using
the presentation model, not inside the reusable presentation model.

### Correctness finding: focus context

The watch UI's focus worker passes no tmux socket or config. Fix this before
calling the watch experience correct for supported non-default configurations.

The smallest correct design is a semantic focus request whose adapter owns the
configured tmux context. Do not expose raw socket/config details to the view.

### Offline history

If viewing bounded history while the daemon is unavailable is a deliberate
feature, preserve it. Move the filesystem and ledger reader into an explicit
offline-history adapter and label the resulting state as historical. Do not
remove the feature merely to make the presentation module pure.

### User experience priorities

1. One clear first-run path.
2. One consistent vocabulary for state and actions.
3. Persistent visibility choices without forced chrome.
4. Honest loading, stale, disconnected, uncertain, and partial states.
5. Keyboard, mouse, narrow terminal, plain mode, and screen-reader journeys.
6. No destructive action whose target is hidden by the current terminal size.
7. Recovery text that states condition, consequence, and next action.

## Track E: agent integration

### Current state

Agent integration spans:

- manifest schema and evaluation;
- shipped manifest data;
- process and argv binding;
- hook templates and hook report authentication;
- vendor configuration merging;
- skill seeding and capability checks;
- setup inspection; and
- notification choice between doorbell and direct compatibility behavior.

The manifest evaluator is a good deep module. Installation and capability
policy around it are more distributed.

### Desired responsibility

One agent-integration module should answer:

- Is this consumer installed?
- Which exact process generation and manifest bind this pane?
- What lifecycle evidence can this consumer provide?
- Is its Cyclops skill current, known old, or operator-edited?
- Which notification capability is proven?
- What setup action is safe and authorized?

Runtime observation still owns current pane evidence. Managed installation
still owns file mutation. Agent integration supplies the vendor-specific facts
both need.

### Reliability rule

A manifest version claim is evidence, not permanent truth. Keep the current
fail-closed behavior when vendor chrome changes. Improve the user experience by
making unsupported, unverified, and partially verified states specific and
actionable.

### Beta scope

- Keep the four current vendor integrations correct.
- Add current-version evidence before strengthening capability claims.
- Do not require a production background agent runner.
- Do not add MCP in this beta.
- Do not infer completion from an idle pane.

## Track F: CLI, headless use, and product front door

### Current state

The CLI offers a strong machine-readable surface and a broad human command
surface. The top-level help currently lists more than twenty command groups,
including ordinary, diagnostic, compatibility, administrative, and maintenance
verbs together.

The `cyclops` package describes itself as a thin client but depends on:

- protocol, state, manifest, theme, and tmux crates;
- `cyclops-ui`; and
- `cyclops-workspace`.

### Desired responsibility

The product front door should provide progressive disclosure:

- **Everyday:** `cyclops`, `send`, `inbox`, `reply`, `status`, `health`.
- **Workspace construction:** `start`, saved layouts, sizing, naming.
- **Operations:** daemon, update, cleanup, hooks, theme.
- **Diagnosis and compatibility:** read, history, attention, alarm,
  notification, and deprecated spellings.

Existing command spellings may remain compatible. The help and docs should
lead with the smallest useful set.

### Headless build seam

Headless commands should compile and test without the workspace and watch UI
implementations. Establish the internal seam first. Split binaries or Cargo
features only if the seam produces measured build, install, test, or failure
isolation value.

### Installation speed

Prebuilt matched CLI and daemon binaries are the highest-leverage user speedup.
They should be considered a beta goal if the release process can provide:

- platform and architecture identity;
- checksums and provenance;
- a matched pair guarantee;
- atomic activation and rollback;
- a source-build fallback; and
- honest unsupported-platform errors.

If Cyclops intentionally remains source-only, revise the sixty-second first
delivery goal. Both claims cannot remain true.

## Track G: installation, update, health, cleanup, and managed assets

### Current state

The relevant implementations are individually careful but collectively broad:

- `update.rs`: about 4,900 lines;
- `health.rs`: about 2,950 lines;
- `cleanup.rs`: about 2,930 lines;
- `hookset.rs`: about 2,300 lines;
- separate manifest, theme, sound, and skill seed modules; and
- the shell installer and hosted byte-identical copy.

Repeated concepts include:

- shipped artifact inventory;
- historical shipped hashes;
- operator edit authority;
- current, old, edited, absent, unsafe, and partial states;
- safe creation and replacement;
- matched binary pair identity;
- rollback ownership;
- read-only inspection; and
- bounded safe removal.

### Desired responsibility

A deep managed-assets module should own the lifecycle vocabulary and planning
for shipped artifacts. Its interface should support:

1. inventory;
2. classify ownership and drift;
3. produce an explicit plan;
4. apply authorized changes;
5. verify the result; and
6. report rollback or repair actions.

Commands remain responsible for user intent, confirmation, rendering, and exit
status. The state module remains responsible for safe filesystem mechanics.
Vendor integration remains responsible for vendor-specific facts.

### Deletion test

The module earns its depth only if setup, update, health, cleanup, and installer
callers can delete repeated knowledge about shipped history, operator edits,
ownership, and lifecycle states.

Do not create one generic asset abstraction if manifests, themes, skills,
hooks, sounds, and binaries do not actually share the same legal transitions.
Share the lifecycle facts they truly have in common and preserve specialized
rules internally.

## Track H: configuration, durable state, and data lifecycle

### Current state

The secure state-root implementation is strong. Ownership above it is less
uniform:

- daemon configuration owns runtime and delivery settings;
- the same parser recognizes `theme` and `default_workspace` without using
  them;
- workspace code parses overlapping settings;
- workspace preferences live separately;
- daemon memory stores last-active workspace UI state;
- saved layouts, identities, mailbox history, session history, assets, logs,
  and update state have distinct formats and lifecycles.

### Desired responsibility

Separate configuration by reason to change:

- **Coordinator configuration:** watched sessions, tmux connection, messaging
  timing, notification policy, and daemon chrome.
- **Interactive preferences:** theme, sound, sidebar, Messages, tab bar, files,
  motion, density, and local view state.
- **Saved layouts:** desired tmux arrangement and naming hints.
- **Operational state:** daemon identity, live session mapping, update pair,
  logs, and cleanup checkpoints.
- **Durable records:** messages, claims, replies, attention actions, and session
  facts.

One file may still carry several categories for compatibility. One owner should
parse and validate each key, and live-update behavior must be explicit.

### Minimum data lifecycle for beta

Cyclops does not need an automatic retention engine before measurement. It does
need user control:

- inventory what durable data exists and its size;
- export records without copying secrets Cyclops never stored;
- explain what uninstall preserves;
- provide an explicit, previewed forget or complete-removal journey;
- keep migration and replay compatibility explicit; and
- refuse silent truncation or deletion.

Indexing and compaction should wait for measured long-history thresholds.

## Track I: CI, tests, performance, and release evidence

### Current state

The CI redesign is a real improvement, not merely documentation:

- superseded pull-request runs cancel;
- required, conditional, scheduled, and release evidence are distinct;
- relocated-root evidence no longer repeats the full suite;
- real tmux fixtures have exact external cleanup ownership;
- path-aware lanes preserve stable check names;
- zero-value doctest execution became documentation compilation;
- performance workloads moved out of ordinary correctness; and
- representative runner time fell from 32m29s to 16m15s, a recorded 50%
  reduction.

### Remaining architecture issue

Large implementation files contain large internal test suites, and several
integration files still follow milestone or incident chronology. This is
partly a consequence of production modules without deep interfaces.

Do not reorganize tests first. Deepen the production module, then move
regression evidence to the interface that owns the contract.

### Required evidence hierarchy

Prefer:

1. pure state transition or domain trace;
2. adapter contract;
3. in-process persistence or socket trace;
4. isolated process trace;
5. real tmux journey; and
6. full user journey.

Use the cheapest level that can honestly fail for the defect. Preserve real
tmux evidence for terminal behavior, process identity, lifecycle, and cleanup.

### Test removal rule

Remove a test only after identifying:

- the durable contract it protects;
- the original defect class;
- the replacement evidence;
- why the replacement can fail for that contract; and
- which evidence lane now owns it.

## Cross-track syncs

The system becomes understandable when the doorways between rooms are explicit.

### Message acceptance and notification

| Trigger | Participants | Data crossing the seam | Result |
|---|---|---|---|
| Authenticated send | directory, messaging, durable record | exact sender, recipients, body, idempotency key | one atomic durable message and recipient entries |
| Durable commit | messaging, notification adapter | body-free notification intent and exact recipient route | optional wake scheduled after acceptance |
| Pane observation | runtime observation, messaging | immutable route and composer evidence | messaging decides durable attempt consequences |
| Claim | messaging, attention, notification | exact caller and message or attempt token | body returned once authorized; independent wake state updated explicitly |

### Workspace interaction

| Trigger | Participants | Data crossing the seam | Result |
|---|---|---|---|
| Key, mouse, or menu action | interaction, workspace state | semantic user intent | legal state transition plus explicit effects |
| Tmux effect requested | workspace state, tmux adapter | exact route and operation | host mutation or named failure |
| Host mutation settles | tmux adapter, runtime continuity | authoritative snapshot or event | workspace state reconciles before rendering |
| State changes | presentation | immutable view state | one coalesced frame |

### Install and update

| Trigger | Participants | Data crossing the seam | Result |
|---|---|---|---|
| Setup or update requested | command, managed assets, agent integration | intent, current inventory, vendor facts | previewable plan |
| Plan authorized | managed assets, state adapter | exact owned paths and expected identities | bounded mutation with rollback evidence |
| Mutation completes | health, presentation | verified inventory and warnings | one user-facing result and repair path |

### Reconnect and recovery

| Trigger | Participants | Data crossing the seam | Result |
|---|---|---|---|
| Subscription gap or daemon restart | client adapter, coordinator, presentation | gap classification, fresh snapshot, cursor | authorized projection rebuilt without silent loss |
| Tmux control reconnect | runtime continuity, workspace state | fresh workspace snapshot and pane hydration | stale local geometry and cells replaced |
| Unknown mutation outcome | client adapter, domain module, user | idempotency key and authoritative snapshot | retry or reconciliation without duplicate effects |

## User experience audit

### What already works well

- The system distinguishes accepted, notified, claimed, replied, and completed.
- Human input protection is treated as correctness.
- Errors often state what happened, why, and the next action.
- Unknown agent states are explained rather than hidden.
- Plain mode and redundant state encoding exist.
- Hidden UI is a supported operating mode.
- Message drafts survive uncertain transport outcomes.
- Destructive Messages actions are constrained by what the frame can display.
- The workspace keeps rendering and daemon IO off the same blocking path.

### What needs product work

#### First-run elapsed time

Runtime is fast, but installation dominates the first experience. Optimize the
journey before micro-optimizing the renderer.

#### Progressive disclosure

Keep advanced commands, but stop presenting every maintenance and compatibility
verb as equal to the six ordinary actions. Help, docs, and empty states should
teach the next useful action.

#### Consistent terminology

Resolve `workspace`, `agent`, `delivery`, `notification`, and `attention` in a
small domain glossary. User-facing terms should remain stable across CLI,
workspace, watch, errors, and docs.

#### Visibility without forced chrome

Complete the collapsed Messages cue. Test full, compact, hidden, pane-only,
detached, and reconnected journeys. A cue should indicate that work exists
without exposing message bodies or reopening a surface the user hid.

#### Honest stale and partial state

Every UI should distinguish:

- loading;
- current;
- historical;
- disconnected;
- partial after a gap;
- action refused;
- request known not sent; and
- outcome unknown after send.

#### Leaving the product

Add a safe export and complete-removal journey. Preserving data by default is
correct, but preservation without a product-level exit path is incomplete user
control.

## Performance audit

### Current measured strengths

The following measurements were refreshed on the reviewed revision on
2026-08-30 using macOS 26.5.2, Apple M5 Pro, 18 logical CPUs, Rust 1.97.1,
and tmux 3.6a.

#### Watch UI frame construction

At a 10,000-entry record:

| Open attention items | Firehose frame | Admin frame |
|---:|---:|---:|
| 0 | 0.079ms | 0.048ms |
| 100 | 0.288ms | 0.133ms |
| 400 | 0.290ms | 0.193ms |
| 1,000 | 0.441ms | 0.345ms |

All remain far below the existing 16ms contract.

#### Workspace input and output

- Idle key-to-control-write: p50 0.7us, p95 3.3us, max 551us.
- Under output flood: p50 18.9us, p95 33.3us, max 98.1us.
- The flood run delivered about 6.3MB during sampling.
- A sustained-output test drained about 6.9MB with data in every scheduled
  drain and one observed continuity gap.

The isolated idle maximum is higher than the flood maximum, which is why tail
timings on shared machines must be interpreted as measurements, not universal
budgets.

#### Reconciliation and hydration

| Shape | 1 | 4 | 8 |
|---|---:|---:|---:|
| Concurrent pane hydration | 0.16ms | 0.25ms | 0.36ms |
| Fixed-command workspace snapshot | 0.16ms | 0.23ms | 0.34ms |
| Historical fan-out reconciliation | 11.18ms | 17.91ms | 27.53ms |

The current fixed-command snapshot is roughly 81 times faster than the old
eight-window fan-out in this run.

#### Terminal runtime

- 80x24 feed throughput: 162.3MB/s.
- 200x50 feed throughput: 93.2MB/s.
- Average direct cell walk: 7.17us and 36.50us per frame respectively.
- Fifty resizes with 2,000 lines of scrollback averaged 514us and reached
  1.085ms maximum.

These results support the current render cadence, bounded batching, concurrent
hydration, and resize coalescing.

### Existing transport measurements

The repository's frozen mailbox benchmark records:

- persistent socket ping p50 0.012ms;
- connect and greeting p50 0.019ms;
- `cyclops send` p50 10.991ms;
- peer CLI send through exact claim p50 27.033ms;
- raw tmux fire-and-forget p50 3.945ms; and
- raw tmux write plus capture p50 8.014ms.

Raw tmux remains cheaper because it proves far less. It has no durable
acceptance, authenticated claim, FIFO, reply ancestry, or recovery record.

### Highest-value optimization priorities

1. **Prebuilt matched binaries:** remove minutes from installation and update.
2. **Headless build seam:** stop compiling full UI implementations for
   headless-only work when measurement confirms the savings.
3. **Cold-start and replay benchmark:** measure daemon boot and UI startup over
   realistic record sizes before adding indexes.
4. **Long-history memory benchmark:** measure projection and UI memory at
   bounded histories and after days of use.
5. **Concurrent messaging saturation:** measure several senders, multiple
   recipients, FIFO fairness, tail latency, journal sync cost, and overload
   behavior.
6. **Truecolor cell path:** isolate the reported per-cell contrast cost, then
   cache or precompute only if the benchmark shows meaningful frame cost.
7. **Linux evidence:** record the same retained workloads on the supported
   Linux path.
8. **Idle wake count:** prove zero-polling behavior with wake and capture counts,
   not CPU time alone.

### Optimizations not currently justified

- A persistent socket for operator-paced detail actions: connection cost is
  already tiny compared with human pacing.
- A database or history index without a measured replay or query threshold.
- Renderer micro-optimization on the measured 256-color path.
- More worker concurrency without a saturation workload and FIFO fairness
  evidence.
- Removing safety observations to match raw tmux latency.
- A distributed pub-sub broker for one local coordinator.

## Reliability, operability, and security

### Reliability promises and owners

| Promise | Owning room | Required evidence |
|---|---|---|
| Accepted means durable | messaging and durable record | crash trace around commit point |
| Retry does not duplicate | messaging | stable idempotency trace |
| Recipient order holds | messaging | multi-message per-recipient trace |
| A replaced process receives nothing | identity, observation, notification adapter | process-generation replacement trace |
| Human input is not knowingly overwritten | observation and terminal notification | real vendor composer journeys |
| Unknown external effect stays unknown | daemon client and effect owner | post-write disconnect trace |
| UI gap does not become false current state | client adapter and presentation | lag, reconnect, and snapshot trace |
| Update activates a matched pair | managed installation | crash and rollback trace |
| User edits survive | managed assets | old seed, current seed, edited copy trace |

### Operability strengths

- `health`, `status`, daemon logs, and append-only records expose state.
- attention states and exact causes are user-visible.
- bounded daemon logs prevent unbounded operational growth.
- explicit requeue and alarm actions avoid silent retries.
- scheduled and release workflows retain reliability and performance evidence.

### Operability improvements

- Give each background worker a named responsibility in one engineering map.
- Expose queue depth, lag, dropped event counts, replay duration, and last
  successful reconciliation where they answer a user recovery question.
- Keep metrics local and bounded; Cyclops does not need telemetry to support
  diagnosis.
- Ensure every timeout reports the operation, consequence, and safe next action.

### Security strengths

- Unix socket peer credentials and process ancestry authenticate callers.
- process birth and executable changes are checked.
- state access is descriptor-relative and owner-only.
- message bodies are not broadcast in invalidation events.
- claims require exact recipient identity.
- secrets are excluded from the record by contract.

### Security scope

Cyclops is a same-user local tool. It should state that trust model plainly.
Reliability must not be described as authorization. Future MCP or remote access
would require a separate threat model and must not reuse local process
attribution without proof.

## Simplification and removal candidates

Remove only after replacement evidence exists:

1. Direct tmux focus from `cyclops-ui`.
2. Direct ledger and filesystem knowledge from reusable presentation modules.
3. Full UI dependencies from the headless client path.
4. Ordinary daemon callers' reach into unrelated `Inner` fields.
5. Repeated managed-asset ownership and shipped-history logic in commands.
6. Duplicate configuration parsing for the same key.
7. Legacy compatibility writers after caller, replay, import, and recovery
   evidence approve removal.
8. Chronology-shaped regression collections after stable domain interfaces own
   the contracts.
9. Historical architecture pages from active navigation after they are clearly
   archived and no live reference depends on them.

Do not remove:

- guarded composer and occupant evidence;
- explicit uncertainty;
- exact identity and claim authorization;
- append-only replay and torn-tail handling;
- zero-polling design;
- the tmux adapter;
- data-driven manifests;
- semantic themes and redundant encoding;
- raw tmux as a human-authorized emergency lane; or
- real tmux tests for behavior that only tmux can prove.

## Sequenced whole-product beta plan

### Phase 0: establish one beta authority

1. Keep the current messaging branch and PR sequence running.
2. Record the seven existing milestones as Track A, not the entire beta.
3. Approve, defer, reject, or mark unverified every finding in this review.
4. Create one whole-product beta charter and integration authority.
5. Keep CI, performance, and release evidence as a cross-cutting track.

### Phase 1: close direct user correctness gaps

1. Fix watch focus for configured tmux context.
2. Finish the collapsed Messages cue.
3. Reconcile Cargo, tag, installer, daemon greeting, and release version
   identity.
4. Decide between prebuilt beta artifacts and an honest source-install timing
   goal.
5. Add focused journeys for non-default tmux socket, hidden Messages, and
   first-run setup.

### Phase 2: deepen workspace interaction

1. Extract one coherent action family behind a state transition interface.
2. Return explicit tmux effects rather than performing them inside view logic.
3. Reconcile authoritative host results.
4. Repeat for layout, workspace lifecycle, and reconnect only after the first
   slice passes the deletion test.

### Phase 3: complete presentation and headless independence

1. Separate pure presentation models from socket, journal, filesystem, terminal,
   and tmux adapters.
2. Preserve offline history through an explicit adapter if it remains a product
   feature.
3. Make headless commands build and test without full-screen UI implementations.
4. Use one state and action vocabulary across CLI, watch, workspace, and docs.

### Phase 4: deepen daemon runtime locality

1. Finish immutable observation evidence.
2. Group session runtime, observation caches, lifecycle tasks, and attention
   facts behind narrow internal interfaces.
3. Reduce `Arc<Inner>` reach-through one operation family at a time.
4. Preserve atomic invariants and avoid a daemon rewrite.

### Phase 5: unify managed installation and configuration ownership

1. Define shared artifact lifecycle states.
2. Build one inventory and plan model used by setup, update, health, and cleanup.
3. Keep operator edit authority and rollback evidence.
4. Assign each configuration key one owner and explicit live-update semantics.
5. Add export and deliberate complete-removal journeys.

### Phase 6: optimize and release from evidence

1. Measure cold start, replay, memory, concurrency, Linux, truecolor, and idle
   wake counts.
2. Optimize only workloads that miss approved user criteria.
3. Run migration, historical replay, real user journey, platform, soak,
   reliability, and performance evidence.
4. Conduct a fresh responsibility audit.
5. Stop for operator approval before merging the whole beta into `main` or
   publishing a release.

## Pull request and milestone rules

- One coherent vertical slice per pull request.
- Every architectural extraction must delete knowledge from callers.
- Preserve current user behavior unless a milestone explicitly changes it.
- Add the least expensive honest regression evidence.
- Do not combine workspace, daemon, installer, and CI restructuring in one
  pull request.
- Keep rollback points independent.
- Do not create a crate merely to move files.
- Stop if a change risks durable meaning, identity, FIFO, authorization,
  replay, or user data without a migration path.

## Whole-product beta acceptance criteria

### Architecture

- Every product responsibility has one documented home.
- `workspace` terminology no longer silently crosses distinct meanings.
- UI modules do not own messaging durability or raw tmux configuration.
- Headless commands do not require full UI implementations.
- The daemon composition root coordinates deep modules without ordinary
  callers reaching through it.
- Managed asset ownership and lifecycle states have one authority.

### User experience

- A new user reaches a first durable handoff through one clear path.
- Install duration is measured and consistent with the stated goal.
- Hidden, compact, pane-only, and full workspace modes all expose new work
  without forced chrome.
- Loading, historical, disconnected, partial, refused, not-sent, and unknown
  states remain distinct.
- A non-default tmux socket works across status, watch, focus, workspace, and
  messaging.
- Update, rollback, export, uninstall, and complete removal are understandable.

### Reliability and security

- Existing messaging invariants remain green.
- Historical journals remain readable or have an approved import path.
- Process replacement, reconnect, crash, and post-write uncertainty journeys
  are retained.
- State ownership and permissions remain descriptor-anchored.
- No compatibility or fallback path is silent.

### Performance

- Ordinary input and rendering meet approved interaction criteria.
- Cold start and replay have measured workloads.
- Concurrent messaging has bounded overload and fairness evidence.
- Linux and macOS results are retained with environment metadata.
- Installation and update time are reported alongside runtime metrics.
- No optimization weakens correctness to improve a number.

### Change safety

- Regression evidence is organized by durable contract.
- Required pull-request evidence remains deterministic and focused.
- Scheduled and release lanes retain broader race, platform, tmux, soak,
  migration, and performance evidence.
- A feature can be assigned to one primary room before implementation starts.

## Blunt final assessment

Cyclops is not low-quality at its core. The durable messaging, state safety,
tmux control, event-driven operation, and uncertainty semantics show careful
systems engineering.

Cyclops is chopped at the product composition level. The workspace application,
daemon shared state, presentation IO, installation lifecycle, and domain
language carry too much cross-room knowledge. That makes the system harder to
explain and feature work scarier than the underlying mechanisms justify.

The correct beta is therefore larger than the seven messaging milestones but
smaller than a rewrite. Finish messaging as Track A. Then deepen workspace,
presentation, daemon runtime, headless packaging, managed installation, and
state ownership through focused vertical slices. Keep the fast paths and the
reliability guarantees. Remove knowledge, not merely files.

That path produces the product originally intended: durable agent coordination
that works without a UI, an optional workspace that makes the system easier to
understand, and an architecture an engineer outside the project can learn room
by room.

## Verification and limitations

Verified during this review:

- current branch, revision, tags, and open beta pull request state;
- repository dependency graph and source responsibility map;
- current CLI help surface;
- direct watch UI tmux focus behavior;
- workspace, daemon, presentation, state, agent integration, installation, and
  CI code paths;
- current CI timing and routing records;
- watch UI performance tests;
- workspace performance contract tests; and
- workspace baseline throughput, hydration, snapshot, and resize tests.

Not verified:

- a fresh user study;
- current live vendor behavior beyond the repository's recorded evidence;
- release transport performance on the reviewed revision;
- cold startup and long-history replay;
- concurrent message saturation;
- Linux performance numbers;
- a truecolor-specific cell benchmark;
- idle wake counts; and
- PR #113, which was pending and not part of the reviewed revision.

These gaps are explicit measurement work, not permission to invent numbers or
build speculative infrastructure.
