# Cyclops beta charter

**Status:** Approved implementation authority

**Approved:** 2026-08-30

**Integration branch:** **beta/messaging-rework**

This charter authorizes the remaining whole-product beta work. It uses the
[whole-system architecture review](../CYCLOPS_SYSTEM_ARCHITECTURE_REVIEW.md)
as supporting evidence without repeating it.
The [Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md) remains the
authority for accepted Track A behavior and compatibility.

## Outcome and user journeys

Cyclops should let one person coordinate terminal agents without surrendering
tmux, trusting invisible delivery, or continuously watching every pane. The
beta is complete only when these representative journeys work:

1. A clean install reaches a first durable two-agent handoff through one clear
   path.
2. Daily workspace use stays responsive while focus, layout, panes, Messages,
   Stream, and files change.
3. Send, claim, reply, wait, and recovery work without either UI; hidden
   Messages still has a body-free cue.
4. Daemon, tmux, hook, update, and partial-write failures remain honest and
   recoverable.
5. Update, rollback, export, uninstall, and deliberate complete removal state
   exactly what changes and what data remains.

## Authority

When documents differ, use this order:

1. Current operator instructions.
2. Current code and focused behavioral evidence.
3. [Invariants](INVARIANTS.md), [delivery](DELIVERY.md),
   [protocol](../reference/PROTOCOL.md), [architecture](ARCHITECTURE.md),
   [goals](GOALS.md), and [style](STYLE.md).
4. The Messaging Refactor Charter for Track A.
5. This charter for the remaining beta scope.
6. [HANDOFF.md](HANDOFF.md), [STATUS.md](../../STATUS.md),
   [NEXT.md](NEXT.md), [CI.md](CI.md), and current guides and reference pages.
7. Architecture reviews, audits, CI reviews, benchmarks, and
   [findings.md](../../findings.md) as revision-bound evidence.
8. Frozen roadmaps, stabilization records, V5, changelogs, and archived demo
   material as history.

`NEXT.md` is a thin queue, not a progress diary. Pull requests and their checks
are the authority for work in flight.

## Responsibility and language

Cyclops remains one local coordinator and a modular monolith. These rooms are
responsibility boundaries, not a requirement for one crate per noun:

| Room | Owns |
|---|---|
| Mail room | Durable message acceptance, ordering, claim, reply, recovery, and notification policy |
| Observation room | Immutable route, process, hook, composer, lifecycle, and freshness evidence |
| Living room | Human interaction with the full-screen workspace |
| Windows | Pure presentation models and rendering requests |
| Utility room | tmux, sockets, files, terminal effects, and other physical adapters |
| Records room | Durable state, append-only journals, replay, and migration |
| Maintenance room | Install, inventory, update, health, repair, rollback, cleanup, and removal plans |
| Agent integration room | Vendor manifests, capabilities, hook facts, and safe setup knowledge |
| Front hall | CLI and product entry, first useful action, and progressive-disclosure guidance |

The glossary in the whole-system review is approved. In particular, a
participant is not necessarily an agent; an agent runtime is one exact process
generation; a route is not a label; a message is not a notification; a claim
is not completion; and an interactive workspace, saved layout, tmux session,
Cyclops home, and coordination identity are different concepts. Adopt these
names when an owning interface is already changing. Do not mechanically rename
wire fields, journals, files, or every `WorkspaceId` use.

## Finding disposition

| Finding | Disposition | Owning work |
|---|---|---|
| F1 whole-beta authority | Resolved by this charter | Track 0 |
| F2 configured tmux focus | Approved for beta | Direct user correctness |
| F3 workspace interaction locality | Approved for beta | Track C |
| F4 presentation-owned mechanisms | Resolved by Track A; re-audit before more extraction | Track D audit |
| F5 headless build dependency | Approved for beta | Track F |
| F6 broad daemon-root reach | Approved; partly reduced by Track A | Track B |
| F7 overloaded `workspace` language | Approved for gradual correction | Every owning track |
| F8 first-run goal versus source build | Approved, measurement first | Track F, then Track G if justified |
| F9 distributed managed-asset knowledge | Approved for beta | Track G |
| F10 inconsistent release identity | Approved for internal correction; public name gated | Direct user correctness |
| F11 split configuration authority | Approved for beta | Track H |
| F12 incomplete data lifecycle | Approved for beta | Track H |
| F13 performance gaps | Unverified; measure before optimizing | Track I |
| F14 weak test locality | Approved after owning production seams exist | Track I and each owning track |
| F15 aging vendor evidence | Approved for beta | Track E |

No reviewed finding is rejected. F13 no longer claims Linux has no evidence;
the missing measurements are cold start, replay scaling, memory, concurrent
messaging, first handoff, update and rollback, comparable cross-platform
performance, and idle wake counts.

## Execution and concurrency

Track A is accepted. The remaining product order is:

1. Direct user correctness: configured tmux focus, internal version identity,
   and representative journeys.
2. Track C workspace interaction and tmux continuity.
3. Track D presentation and user experience, beginning with a fresh audit.
4. Track F CLI, headless use, and the product front door.
5. Track B runtime observation, identity, and attention.
6. Track E agent integration.
7. Track G managed installation, update, health, cleanup, and rollback.
8. Track H configuration, durable state, and data lifecycle.
9. Track I CI, tests, performance, compatibility, and release evidence.
10. A fresh whole-beta responsibility and user-journey audit.

This is a dependency order, not a ban on parallel work. Separate worktrees may
advance non-overlapping rooms at the same time. Work that shares files,
invariants, or an unsettled interface is serialized. Version identity precedes
prebuilt or update work; first-handoff measurements precede a prebuilt-install
decision; production seams precede test-only reorganization.

The primary tracer is **beta/fix/tmux-focus-context**. The independent
**beta/fix/version-identity** slice may proceed in parallel.
After those slices merge, **beta/test/first-run-journeys** assigns the
representative journeys to their cheapest honest evidence lanes before Track C.

## Approved P1 tracers

| Finding | Owner and journey | Smallest tracer | Regression contract | Rollback condition |
|---|---|---|---|---|
| F1 | Front hall; every journey | This charter on **beta/docs/whole-beta-authority** | One hierarchy, one queue, indexed links | Revert docs; Track A stays accepted |
| F2 | Utility room; daily coordination | Semantic configured focus on **beta/fix/tmux-focus-context** | Two isolated tmux servers prove only the displayed server receives focus; failure stays visible | Refuse focus rather than touch a default server |
| F3 | Living room; daily coordination | One focus and pane-movement family on **beta/refactor/workspace-focus-actions** | Keyboard, mouse, and menu produce one legal intent and reconcile authoritative tmux results | Revert on focus, draft, visibility, or failure-honesty drift |
| F5 | Front hall; headless use | Measured seam on **beta/refactor/headless-build-seam** | Send, inbox, reply, status, health, and daemon build and test without full UI implementations | Revert if behavior changes or no measured cost is removed |
| F6 | Observation room; failure and recovery | One immutable family on **beta/refactor/runtime-observation** | Exact route, generation, provenance, freshness, conservative unknown, and no unrelated `Inner` reach | Revert on identity, atomicity, or fail-closed drift |
| F7 | Front hall; all journeys | Approve language here; first adoption in **beta/refactor/workspace-focus-actions** | Public meanings agree across surfaces; wire and journal compatibility stays intact | Keep existing spelling where migration is ambiguous |
| F8 | Front hall; first handoff | Staged measurement on **beta/perf/install-first-handoff** | Each stage is timed; success ends at durable acceptance and exact claim | Keep source install; stop before unapproved distribution |
| F9 | Maintenance room; update and leaving | Read-only inventory on **beta/refactor/managed-asset-inventory** used by two real callers | Current, old, edited, absent, unsafe, partial, and externally owned remain distinct | Revert if legal transitions or user edits are flattened |
| F10 | Maintenance room; update and recovery | Internal authority on **beta/fix/version-identity** | Matching pair agrees; mismatch names both builds; old peers remain explicitly unverified | Stop before tag, Release, publication, or incompatible wire changes |

Every slice uses one focused pull request into **beta/messaging-rework**, removes
caller knowledge when architectural, carries the cheapest honest regression,
and remains an independent rollback point.

## Behavior that must remain unchanged

- Durable acceptance is independent of both UIs and precedes notification.
- Identity, per-recipient FIFO, claim authorization, reply ancestry, replay,
  and honest uncertainty remain intact.
- Every currently readable journal format stays readable. There is no silent
  deletion, truncation, or rewrite; breaking migration requires export or an
  explicit migration path.
- Official clients retain bounded framing and shared certainty semantics.
- Notification and activation remain optional. Stateful collapsed Messages,
  the body-free tmux border count, and intentionally chrome-free pane-only use
  all remain valid.
- Observation reports immutable evidence instead of executing messaging
  policy. Presentation does not own messaging, journals, raw sockets, or tmux
  mechanisms.
- `Daemon::deliver_payload` remains compatibility-sensitive with public support
  status unverified until its caller census permits a change.
- Tmux integration stays event-driven, tests use isolated owned servers, and
  automatic raw-tmux fallback is forbidden.

## Evidence and performance

Use the four lanes in [CI.md](CI.md): required pull-request correctness,
conditional integration, scheduled evidence, and release evidence. Prefer a
pure state transition, then adapter, in-process IO, isolated process, real
tmux, and full journey. Moving or removing evidence requires a named
replacement contract; use a small regression simulation when practical, not a
general mutation system.

Retain measurements with commit, environment, workload, version, and timing.
The beta workloads are clean install to exact claim; daemon cold start; replay
across empty, ordinary, long, linked, and damaged history; concurrent message
acceptance and per-recipient fairness; input and render latency under output;
idle wake counts; memory growth; update and rollback; and comparable Linux and
macOS runs. Optimize only a workload that misses an approved criterion.

## Stop, rollback, and release gates

Stop for operator direction if work would break a public wire or journal
contract; lose durability, identity, FIFO, claim authorization, replay, or
honest uncertainty; require automatic raw-tmux fallback; introduce unresolved
security or data-loss risk; or expand into another daemon, distributed broker,
generic workflow engine, production agent runner, or MCP integration.

Also stop if readable history cannot remain readable or migrate safely, secure
state ownership or user-data recovery cannot be maintained, a consequential
user-experience choice has two materially different unresolved outcomes, or
repository-admin branch protection must change.

Stop before signing credentials, public artifact distribution, release tags,
or a GitHub Release. Do not merge **beta/messaging-rework** into `main`, create or
move tags, choose the final public beta version, or publish a release without
explicit operator approval.

The release gate requires a clean-checkout run of current behavior contracts,
historical journal and migration evidence, installer lifecycle, representative
user journeys, platform and tmux coverage, soak and race evidence, retained
performance comparison, and a fresh architecture audit. A failed slice rolls
back through its own pull request without weakening Track A or another room.

## Exclusions

The beta does not include a distributed broker, multi-host messaging, a generic
workflow engine, a production agent runner, an always-running model process,
MCP, automatic raw-tmux fallback, a database rewrite without measurements, a
new crate for every domain noun, or unrelated website work.
