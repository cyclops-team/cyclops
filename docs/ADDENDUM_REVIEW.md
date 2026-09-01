# Cyclops architecture review addendum

> Supporting design record. The approved
> [Messaging Refactor Charter](development/MESSAGING_REFACTOR_CHARTER.md)
> controls implementation when the documents differ.

- Review date: 2026-08-29
- Reviewed revision: `ead57c1691371a1deca5afeb89e90e8340accb69`
- Status: supporting analysis and design record

This addendum complements the
[primary messaging architecture review](MESSAGING_ARCHITECTURE_REVIEW.md).

It gathers the product goals, candid system assessment, activation and
visibility analysis, CI audit, and implementation-governance decisions in one
coherent record.

The primary review contains the detailed target architecture and code audit.
This addendum explains what the system is trying to become, which current
qualities matter, and how to turn the review into controlled implementation
work.

## How to read this document

Not every statement has the same authority.

- Product and engineering goals are direct user intent.
- Measurements describe the reviewed revision and must be refreshed when the
  worktree changes.
- Code observations are evidence about the current implementation.
- Recommendations remain proposals until an architecture charter approves,
  defers, rejects, or marks them unverified.
- Historical behavior is not automatically desirable architecture.

The current code and tests define a baseline that must be understood. They do
not automatically define the ideal system. A test may protect a real contract,
an accidental behavior, or an obsolete compatibility path.

When this addendum conflicts with current code, `NEXT.md`, or an older design
record, the conflict must be recorded and resolved. It must not be hidden by
silently selecting the first source an engineer read.

## Product and engineering goals

### Two related but independent capabilities

Cyclops has two foundations:

1. A durable messaging and coordination system for agents.
2. An optional human interface for observing and controlling several agents.

They should integrate cleanly, but neither should require the other to
function.

An agent must be able to discover recipients, send, wait, claim, reply, and
recover through Cyclops without opening the workspace UI, sidebar, or Messages
pane.

The UI should make the system easier to understand. It must not become the
transport, durable authority, or only recovery path.

### User experience is part of correctness

A person should be able to understand:

- what happened;
- who communicated with whom;
- what Cyclops has actually proven;
- whether an agent is running, waiting, blocked, or needs the user;
- what remains uncertain;
- what action is safe; and
- how to recover.

A user may want only the agentic CLI panes. They may hide the sidebar and
Messages pane or use native tmux. Cyclops must support that journey
intentionally.

New mail must not force hidden messaging chrome open.

The bounded sender-authored preview exists because it gives a person watching
only the panes a readable trace of who sent work and how to inspect it.
Preserve that purpose.

The exact two-sentence summary grammar may be measured and simplified. The
preview itself should not disappear merely because its current validation is
inconvenient.

Visibility and activation are different. Cyclops must not write into an agent
prompt under a manual activation policy merely to make a message visible.

### Reliability must produce useful and honest progress

Cyclops messaging should be reliable, correct, understandable, and genuinely
well made.

Preserve the valuable foundation:

- durable acceptance before success;
- stable participant and recipient identity;
- per-recipient FIFO;
- idempotent submission;
- authenticated claims;
- durable reply ancestry;
- crash replay and torn-tail handling;
- honest unknown external effects;
- bounded waits without polling;
- recoverable authoritative state;
- body-free invalidation events; and
- human-readable outcomes.

A durable message, human notification, activation request, started model turn,
claim, reply, and completed task are different facts. Never use one silently as
proof of another.

### Less is more

Cyclops should be safe from bugs, easy to understand, and ready for change. It
should also remain understandable to engineers who did not build it.

Use domain-driven design when it clarifies language, responsibility,
ownership, and invariants. Do not manufacture factories, repositories,
services, crates, or event buses merely to appear architected.

The house analogy is useful: each room has one coherent purpose, related work
stays together, and rooms meet through visible doorways. One room should not
store the entire mutable house.

Every abstraction, guard, retry, state, worker, compatibility path, and test
must earn its cost through a concrete contract or credible fault.

Do not simplify by removing correctness. Do not preserve complexity merely
because current tests or implementation already encode it.

### Change should be routine

Adding an ordinary feature should not be scary.

A healthy change should:

- have one obvious domain home;
- cross a small number of explicit seams;
- avoid unrelated daemon, UI, transport, and persistence knowledge;
- gain a readable regression trace at the least expensive honest level; and
- leave the repository coherent if later work never happens.

Cyclops does not need the largest possible test suite. It needs strong primary
proofs for important contracts.

### Performance should compare useful outcomes

Cyclops and raw tmux should be measured at named, comparable milestones.

Raw tmux may win at first visible terminal submission because it provides far
fewer guarantees. That does not make it equivalent to durable acceptance,
authenticated claim, or useful reply.

Measure at least:

- process and connection overhead;
- durable acceptance latency;
- acceptance-to-claim latency;
- acceptance-to-useful-reply latency;
- notification and activation latency;
- concurrency and recipient fairness;
- daemon and runner idle resource use;
- reconnect and replay convergence;
- long-history behavior;
- manual recovery frequency; and
- wrong-target, loss, duplication, and uncertain outcomes.

Optimize for useful, reliable communication and understandable operation, not
one flattering latency number.

## Honest assessment of the current system

Cyclops is not fundamentally chopped. It has a serious reliability core buried
inside an overgrown and partially entangled implementation.

It has real P1 defects and design problems that make future bugs more likely.
It is not finished or fully stable. The evidence does not justify calling the
entire system broadly broken, and it does not justify a rewrite.

### What is genuinely good

Keep these foundations:

- durable acceptance before success;
- append-only journals and strict replay;
- stable recipient identity and recipient-scoped claims;
- idempotency;
- explicit unknown-after-write outcomes;
- separation between message acceptance and terminal notification;
- snapshots, durable follow, and lossy invalidation; and
- existing crash-cut and state-transition evidence that protects real faults.

That is the spine of a serious messaging system. Raw tmux does not provide
those guarantees.

### What is genuinely bad

| Problem | Why it matters | Direction |
|---|---|---|
| Broad `Arc<Inner>` reach-through | Callers can learn unrelated locks, workers, caches, runtime state, mailbox state, and lifecycle behavior | Remove reach-through one path at a time and pass only the needed module or immutable fact |
| Three daemon-client transports | CLI, stream UI, and workspace duplicate greeting, framing, timeout, reconnect, and uncertainty behavior | Build one deep client interface and migrate every official client |
| Current and legacy delivery are interwoven | Current mailbox notification, legacy direct delivery, hook self-test, and replay compatibility share one large implementation | Census callers and durable history, then quarantine proven legacy behavior |
| Presentation knows backend details | UI code knows journal paths, socket behavior, and tmux focus | Give presentation pure snapshots and user-action descriptions |
| Subscribe cursor is misleading | The interface suggests replay that the daemon does not honor | Keep invalidation ephemeral and use snapshots or `messages.follow` for durable recovery |
| Terminal-safety wording is absolute | Tmux cannot eliminate the race between final observation and paste execution | Preserve useful guards but correct the impossible guarantee |
| Official frame limits disagree | The daemon can accept content an official UI refuses to read | Enforce one bounded frame contract everywhere |
| Internal vocabulary leaks outward | Normal users encounter barriers, generations, sealing, and recovery machinery | Lead with accepted, claimed, needs attention, and replied |

### What should not be removed

Some defensive code represents real crash and uncertainty cuts.

Keep the distinctions among `Writing`, `Staged`, `Submitting`, and
`Submitted`. Keep durable intent, idempotency, replay validation, identity
verification, composer checks, and honest ambiguity.

Removing those merely to shorten the implementation would make Cyclops less
reliable, not simpler.

### Overall judgment

| Area | Judgment |
|---|---|
| Core messaging model | Good |
| Persistence and identity | Strong |
| Terminal notification | Useful but inherently fragile |
| Modularity and changeability | Mediocre |
| UI separation | Incomplete |
| Legacy locality | Poor |
| Integration-seam bug risk | High enough to address now |
| Rewrite needed | No |
| Focused simplification needed | Yes |

The right treatment is controlled removal and deepening. Fix verified P1
contracts, consolidate transport, and deepen `WorkspaceMessaging`.

Then shrink shared-state reach-through, quarantine legacy delivery, and remove
backend knowledge from presentation.

## Architecture direction

### Keep a modular monolith

Cyclops should remain one local durable coordinator with replaceable clients,
presentation paths, notification mechanisms, and activation implementations.

Do not add a distributed broker, generic event bus, multi-host mesh, or one
crate per domain without a measured requirement.

### One deep workspace-messaging module

The recommended default is one deep internal `WorkspaceMessaging` module that
owns the atomic durable transition and hides its representation.

It may contain distinct internal concepts for messaging, notification intent,
activation intent, and attention because those facts sometimes change in one
transaction.

The public interface should speak product operations and immutable outcomes.
Callers should not learn journal variants, internal maps, notification cuts,
locks, or compatibility state.

### Internal module before crate extraction

`docs/development/NEXT.md` proposes immediate extraction of a
`cyclops-delivery-core` crate. The newer review recommends proving the internal
module first.

The newer direction is the stronger default. A crate is packaging, not
modularity.

Approve a new crate only if it creates independently valuable build isolation,
release isolation, reuse, failure isolation, or measurable knowledge deletion
from callers.

If callers still understand the same state machine, locks, journal maps, and
recovery order through a crate import, the extraction has only moved the
coupling.

### Domain responsibilities

| Domain | Owns | Does not own |
|---|---|---|
| Directory and identity | Stable participants, recipient generations, labels, and current routes | Message bodies, claims, terminal writes, or rendering |
| Messaging | Acceptance, immutable content, FIFO, claims, replies, idempotency, and threads | Pane readiness, agent execution, or UI layout |
| Human notification | Optional human attention and bounded orientation | Agent activation, claim, or completion |
| Agent activation | Optional request to start or resume one bounded turn | Mailbox truth, private message access, or task graphs |
| Runtime observation | Fresh evidence about panes, processes, composer state, and host state | Durable messaging decisions or recovery policy |
| Attention and recovery | Explainable conditions requiring human judgment | Message truth or automatic certainty |
| Presentation | Reconstructable views and user actions | Journal ownership, socket policy, or tmux execution |

The domains do not each require a public interface, crate, process, or store.
They require clear ownership and explicit syncs.

### Messaging, notification, activation, and execution

These are separate facts:

```text
message accepted
    -> optional human notification
    -> optional activation requested
    -> host accepts or rejects control input
    -> model turn starts
    -> exact recipient claims the message
    -> recipient replies
    -> optional workflow records completion
```

No arrow is automatic proof of the next fact.

## Agent activation and access interfaces

### Why a mailbox listener is not enough

An agent CLI can be absent, idle at its prompt, executing a turn, or waiting
for approval. Cyclops can durably accept a message in every state.

The hard transition is moving an absent or idle host into an executing model
turn.

A background child process may wait for Cyclops mail, but it cannot restart a
completed model turn by itself. It needs a supported queue, resume,
streaming-input, or control interface.

### Preferred activation shape

```text
Cyclops mailbox
    durable acceptance, identity, inbox, claim, reply
                         |
                         v
Optional sleeping runner
    waits without polling and applies recipient activation policy
                         |
                         v
Agent-host implementation
    native queue | background session | ACP | guarded tmux
                         |
                         v
Agent execution
    model turn, tools, approvals, cancellation
                         |
                         v
Observation and optional presentation
    status, transcript, progress, user intervention
```

Messaging remains useful when the runner and every UI are stopped.

The runner should sleep without consuming model tokens. When durable work and
policy allow, it requests one bounded turn and records the host outcome.

### Current host possibilities

At the review date, the installed or documented hosts exposed plausible
control paths:

| Host | Candidate path | What still requires a contract probe |
|---|---|---|
| Codex | Native queue, app-server connection, or session resume | Ordering, idle-session wake, approvals, cancellation, reconnect, and unknown outcomes |
| Claude Code | Background sessions, attachable agent view, and streaming input | Stable inactive-session input, permissions, detach, restart, and cancellation |
| Gemini CLI | ACP JSON-RPC mode, headless execution, and session resume | Identity, concurrency, approval flow, reconnect, and durable correlation |
| Interactive-only CLI | Guarded tmux input | Composer races, wrong mode, ambiguous submission, and weak structured outcomes |

Official references:

- [Claude Code agent view](https://code.claude.com/docs/en/agent-view)
- [Gemini CLI ACP mode](https://geminicli.com/docs/cli/acp-mode/)
- [Agent Client Protocol overview](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/protocol/v2/overview.mdx)

These are capability observations, not proof that Cyclops can depend on them.
Each supported host needs the same bounded contract probe.

### Minimum reliable runner

The smallest useful runner needs:

- one active turn per recipient by default;
- recipient FIFO;
- manual, notify-only, and automatic modes;
- FYI messages that never automatically start work;
- durable activation attempts tied to exact mailbox entries;
- no automatic retry after an unknown host effect;
- visible waiting-for-user and approval states;
- pause, cancel, and bounded cycle limits; and
- explicit identity delegation bound to one exact agent generation.

The runner should not become a universal workflow engine. It schedules one
turn. It does not own task graphs, artifacts, dependencies, or endless agent
conversations.

### MCP is an access adapter, not the messaging system

MCP may expose typed actions to an already-running agent:

- send;
- wait for mail;
- claim;
- reply;
- inspect a thread; and
- inspect status.

MCP does not replace durable acceptance, recipient FIFO, identity, claim,
replay, or terminal-effect recovery.

MCP notifications also do not guarantee that a host starts a new model turn.
That decision belongs to the host.

The useful combination is:

- the native CLI as the universal access path;
- optional stdio MCP for typed access by active agents;
- native host controls for activation; and
- guarded tmux only for hosts without a better control path.

Caller identity is the gate. A central MCP or runner process must never claim
or reply as an arbitrary agent because a request supplied a sender name.

Test identity, retry uncertainty, timeouts, cancellation, and stale process
authority in a throwaway experiment before adopting MCP.

## Visibility without forced messaging chrome

The pane should be an attachable view of an agent, not the transport that keeps
the agent alive.

A user may choose:

- the full Messages pane;
- a compact workspace cue;
- only agent panes;
- a native host transcript;
- native tmux with no Cyclops chrome; or
- explicit inbox commands.

All views must derive from the same durable facts. They must not create
separate unread queues or message truth.

### The preview has a real job

The current Format 4 line carries sender, bounded summary, and exact claim
path. It is for the person watching the pane while the full body remains in the
authenticated mailbox.

That preview is not accidental complexity. It preserves orientation when the
sidebar and Messages pane are hidden.

The coupling is in the terminal implementation: staging the visible line may
also start a model turn. Visibility and activation should remain separate
meanings even when one host action happens to provide both.

### Honest limits

Native tmux with every Cyclops surface hidden has no separate place to display
a rich non-activating cue.

In that mode, immediate rich visibility requires an explicitly enabled system
notification, a host transcript, a terminal activation trace, or deliberate
inbox inspection.

Cyclops should state that limitation. It should not reopen the sidebar, write
into a manual agent prompt, or claim that invisible work was shown to a human.

### Human controls

For background activation, the user should be able to:

- pause automatic activation;
- inspect queued work;
- attach to the native host session;
- answer approvals or questions;
- cancel the current turn;
- stop the runner; and
- understand whether work is accepted, activated, claimed, replied, blocked,
  or uncertain.

## Raw tmux emergency path

Raw tmux must remain an explicit emergency path when Cyclops is confirmed
unavailable or broken.

It is a fail-safe for useful human-controlled progress, not a second normal
message system and not an automatic retry mechanism.

A raw-tmux send means only visible terminal submission. It does not prove:

- durable Cyclops acceptance;
- authenticated retrieval;
- replay;
- recipient FIFO;
- reply ancestry; or
- completion.

It may also interfere with human input.

Do not remove the escape hatch merely to make the architecture look pure. Do
not invoke it automatically after an ambiguous Cyclops effect because that may
duplicate work or overwrite input.

## CI and regression-test audit

The CI audit reviewed revision `ead57c1691371a1deca5afeb89e90e8340accb69`.
Its primary workflow evidence was
[GitHub Actions run 33241643555](https://github.com/cyclops-team/cyclops/actions/runs/33241643555).

The measurements below describe that snapshot. They must be refreshed before
implementation decisions rely on them.

### Audit scope and method

This was a read-only audit of:

1. `.github/workflows/ci.yml`;
2. Cargo and nextest configuration;
3. test counts, file sizes, fixtures, sleeps, polling, and source scans;
4. the completed workflow for the reviewed revision;
5. recent workflow outcomes and failing job names;
6. warm local timings; and
7. live process and tmux state after test activity.

Local timings are diagnostic. GitHub-hosted timings are the decision baseline
for CI cost.

### Executive verdict

Cyclops contains valuable regression evidence, but the CI and test system is
too repetitive and too shaped by the chronology of individual incidents.

The central problem is not simply too many tests. Too many expensive tests
prove nearly the same behavior, while several architecture rules are guarded
by source-text scans that explicitly admit they are not proofs.

The measured pull-request gate took 12 minutes 20 seconds and consumed about
38 runner minutes. It reran the complete Rust evidence under a different
temporary root and ran advisory tmux-HEAD work on every change.

Five of the 14 completed runs before the evidence run failed. Several
follow-up commits repaired fixture synchronization rather than product
behavior.

A good target is not the fewest tests. It is the smallest clear evidence set
that makes a wrong implementation difficult to ship and a correct change easy
to understand.

### Test inventory

| Measure | Observed |
|---|---:|
| Rust test functions | 2,756 |
| `#[cfg(test)]` declarations | 221 |
| Rust lines under integration-test paths | 53,298 |
| Rust lines in `src/cyclopsd/tests` | 24,661 |
| Non-daemon tests selected by the main nextest command | 1,860 |
| Daemon tests listed by Cargo | 888 |
| Doctests | 0 |
| Rust source and test lines inspected | 248,040 |

Test functions by crate:

| Crate | Tests |
|---|---:|
| `cyclopsd` | 888 |
| `cyclops-workspace` | 607 |
| `cyclops` | 476 |
| `cyclops-ui` | 291 |
| `cyclops-tmux` | 158 |
| `cyclops-proto` | 132 |
| `cyclops-state` | 68 |
| `cyclops-manifest` | 65 |
| `cyclops-theme` | 50 |
| `cyclops-ledger` | 13 |
| `cyclops-testrig` | 8 |

The count alone does not prove waste. The stronger signal is how
implementation and tests cluster in the same oversized files.

| File | Total lines | Test module starts | Approximate test lines |
|---|---:|---:|---:|
| `src/cyclopsd/src/mailbox.rs` | 18,050 | 8,055 | 9,996 |
| `src/cyclopsd/src/delivery.rs` | 15,931 | 9,710 | 6,222 |
| `src/cyclopsd/src/fusion.rs` | 11,111 | 4,781 | 6,331 |
| `src/cyclops-workspace/src/app.rs` | 12,716 | 6,935 | 5,782 |
| `src/cyclopsd/src/server.rs` | 4,962 | 2,567 | 2,396 |

This is poor locality. A reader cannot quickly tell which test is primary,
which variants prove distinct faults, or whether a newer contract already
subsumes an incident check.

### Workflow cost

| Job | Total | Expensive step |
|---|---:|---:|
| macOS Rust gate | 12m 16s | Rust tests 6m 30s; relocated rerun 3m 20s |
| Linux Rust gate | 10m 32s | Rust tests 5m 28s; relocated rerun 3m 21s |
| tmux HEAD advisory | 6m 27s | Full Rust gate 5m 32s |
| macOS installer | 4m 58s | Installer trace 4m 23s |
| Linux installer | 3m 14s | Installer trace 2m 55s |
| Website | 21s | Entire job 21s |

Parallel jobs reduce wall-clock waiting but do not remove compute cost, noise,
or failure surface.

### Warm local timings

The local measurements used an Apple M5 Pro, Rust 1.97.1, and tmux 3.6a.
They are diagnostic, not universal performance claims.

| Check | Observed |
|---|---:|
| Non-daemon nextest run, 1,860 tests | 42.9s |
| `cyclopsd` Cargo run, 888 tests, four threads | 2m 44s |
| Performance-contract executable, 7 active tests | 9.1s |
| Parity trace | 47.7s |
| Clippy | 11.9s |
| Rustfmt | 1.0s |
| Documentation-path command | 0.05s |
| Doctest command | 5.7s, 0 tests |

The slowest observed daemon executables included:

- `attention_recovery`: 31.9s;
- `messaging_coordinator`: 15.9s;
- `lifecycle`: 13.0s; and
- daemon library tests: 11.9s.

The daemon suite dominates local feedback. Performance checks also use real
clocks, output floods, and serialized sampling, so they answer a different
risk from ordinary correctness.

### Recent stability evidence

Five of the 14 completed runs before the primary evidence run failed. The
required jobs passed in the evidence run, while the advisory tmux-HEAD job
failed `a_transient_hidden_frame_does_not_bypass_the_active_human_hold`.

Earlier failures appeared in the ordinary Rust gate, duplicate relocated
gate, and advisory tmux-HEAD gate.

Recent commit subjects included:

- `test(ci): remove runtime session startup race`;
- `test(ci): isolate backspace release on tmux head`;
- `test(ci): separate composer release and staging latches`;
- `test(ci): pin final composer deletion boundary`; and
- `test(ci): synchronize rig startup and gate evidence`.

This does not prove every failure was flaky. It shows that fixture and
scheduling behavior consumed a material part of the development loop.

### Current CI findings

#### P1: the complete Rust evidence runs twice

The relocated `CYCLOPS_TEST_TMP` step reruns non-daemon tests, daemon tests,
and doctests after the ordinary gate already ran them.

The property is narrow: scratch paths must honor one configured root. Keep the
exact override test, a representative daemon or tmux journey, and a simple
forbidden-path lint.

Delete the complete relocated rerun only after those smaller checks fail
against a deliberately broken helper.

#### P1: obsolete runs are not cancelled

There is no top-level pull-request concurrency rule. New commits leave old
10-minute runs executing against revisions that cannot merge.

Cancel superseded pull-request runs by workflow and pull request. Keep release
and manually dispatched proof runs in a separate non-cancelled group.

#### P1: fixture cleanup is not interruption-proof

The audit found eight live `cyc-*` tmux servers, eight fake-terminal Python
processes orphaned under PID 1 for more than four hours, and 37 test-named tmux
socket files.

This does not prove the latest successful run created them. A focused rerun of
`crash_after_intent_stays_ambiguous_without_a_second_key` passed without adding
residue. It does prove that `Drop` cleanup cannot cover forced termination.

Give each executable an external cleanup owner and unique run ID. After every
real-tmux executable, assert that it left no owned process, server, socket, or
temporary resource.

Test forced interruption once. Cleanup targets must remain exact and must
never touch the user's tmux server or a broad temporary directory.

#### P1: source-text scans stand in for architecture

`src/cyclops-proto/tests/one_place.rs` scans Rust source for shapes resembling
a duplicate attention rule. Its own documentation says this is not proof and
records a case that defeated the scan.

Some syntactic dependency lints are useful. Semantic source scans should not
be presented as runtime regression evidence.

Expose one deep attention interface and make consumers depend on it. Delete
the semantic scanner only after callers cannot reproduce the rule without
crossing that interface.

#### P1: test organization follows incident chronology

Files such as `m1_fixes.rs`, `m1_blockers.rs`, `m2_history.rs`, and
`gate3_release_proof.rs` preserve implementation history rather than stable
domain language.

Organize tests around durable acceptance, claim, reply, notification,
attention, identity, recovery, and terminal safety. Preserve historical
reasoning in short comments or the stabilization record.

Merge duplicates only after a stronger trace is shown to fail against the
original defect.

#### P1: oversized modules generate oversized test programs

The test problem cannot be solved only in CI. Large modules own too many
decisions, so their tests need wide fixtures and hundreds of interacting
cases.

Deepen the domain modules. Test decisions through narrow interfaces and keep a
small number of integration traces across their seams.

Better modularity should delete repeated setup and assertions, not create more
test scaffolding.

#### P2: different test risks share one lane

Correctness, performance, soak, upstream compatibility, installation, and
portability currently overlap in ordinary pull-request work.

Separate them by the failure they answer:

| Test class | Question | When |
|---|---|---|
| Domain trace | Is the durable fact or transition correct? | Every change |
| Adapter contract | Does one implementation honor its interface? | Relevant changes |
| Process integration | Do daemon, journal, socket, and client agree? | Relevant changes |
| Real tmux journey | Does the actual terminal path work? | Small required set |
| Portability | Does OS-specific behavior work? | Relevant changes plus nightly |
| Performance | Did a measured distribution or resource limit regress? | Scheduled and release |
| Soak and upstream compatibility | Does the system survive duration or upstream change? | Scheduled and manual |
| Installer | Can a clean user install and uninstall safely? | Installer changes and release |

#### P2: macOS duplication is broader than the portability claim

Peer credentials, filesystem behavior, scratch paths, shell behavior, and tmux
integration have real OS differences. Pure parsing and state transitions do
not need complete duplication by default.

Run pure domain tests once. Keep a named macOS portability set and run the
complete operating-system matrix on a schedule and before release until the
narrower set demonstrates equal fault detection.

#### P2: the workflow runs zero doctests

The doctest command performs setup and compilation but Cargo lists zero
doctests.

Remove it from the pull-request gate. Restore it when executable documentation
examples exist. Keep the dedicated documentation parity trace.

#### P2: unrelated jobs run on every change

Website, installer, and tmux-HEAD jobs run regardless of the affected paths.

Make them change-aware:

- website for website, shared public assets, and installer-copy changes;
- installers for installation scripts, packaging, relevant docs, and release
  wiring; and
- tmux HEAD for scheduled runs, manual proof, or tmux-adapter changes.

Keep required status names understandable when a conditional job has no work.

#### P2: CI does not retain useful performance history

The workflow prints timings but does not retain stable machine-readable
results or show which executables own the critical path.

Publish nextest durations and retry-free outcomes. Scheduled performance runs
should record p50, p95, maximum, sample size, OS, Rust version, tmux version,
and revision.

Do not gate ordinary pull requests on tight wall-clock budgets from shared
runners.

## What a real regression test means

A Cyclops regression test should satisfy these conditions:

1. **Names a durable contract.** Describe accepted, claimed, recovered,
   isolated, or rendered behavior rather than a milestone or ticket.
2. **Fails before the fix.** Demonstrate failure for the intended reason.
3. **Uses the least expensive honest seam.** Pure behavior does not boot tmux,
   while tmux behavior is not falsely proved by a fake.
4. **Controls the race.** Use events, barriers, fault injection, or virtual
   time instead of hoping a sleep lands correctly.
5. **Asserts observable facts.** Check durable records, protocol outcomes,
   process ownership, or user-visible output.
6. **Owns its resources.** Leave no process, socket, session, journal, or
   temporary path behind.
7. **Has one reason to fail.** Distinguish setup, performance, and behavior
   failures.
8. **Is not duplicated without a distinct claim.** A repeated journey at
   another seam must prove another risk.

Passing checks are evidence, not architecture. The implementation should make
invalid behavior difficult, while tests demonstrate that design.

## Desired test architecture

### Domain traces

Put mailbox lifecycle, claims, replies, attention, notification, activation
intent, and recovery into deterministic traces without tmux, sockets, shells,
or real clocks.

For each trace, record starting facts, command or event, resulting fact or
refusal, emitted sync, and permitted next commands.

One readable table can replace many tests that repeat the same setup for one
transition.

### Adapter contracts

Give each external implementation a reusable contract:

- journal append and recovery;
- socket request, response, and subscription ordering;
- tmux pane observation and key injection;
- clocks and deadlines;
- process identity and OS credentials; and
- agent-host activation and transcript visibility.

Use deterministic implementations where honest, followed by a smaller real
implementation set. The goal is leverage, not mocking the entire system.

### Focused process traces

Keep a compact real-daemon suite for facts only a process can prove:

- acceptance survives restart;
- exactly one claim wins;
- reply ancestry survives replay;
- unknown terminal effects remain unknown after restart;
- socket authorization follows peer credentials; and
- interruption leaves no owned resources.

Use explicit fault points and events rather than fixed multi-second sleeps.

### Small real-tmux journeys

The required pull-request set should prove a few complete journeys:

1. Send, durable acceptance, notification, and exact claim.
2. Busy recipient, queued notification, and later wake.
3. Staged human text is never casually overwritten.
4. Daemon restart and raw-tmux recovery remain understandable.
5. Narrow or wrapped terminal output remains usable.
6. Cleanup removes only the rig's state.

Domain traces own the combinatorics. Do not multiply real-tmux journeys for
every internal state.

### Scheduled evidence

Run complete OS coverage, performance, soak, tmux HEAD, interruption cleanup,
and release installation on schedules and on demand.

A release candidate should run all of them. An ordinary documentation or pure
domain change should not.

## Proposed CI shape

```text
pull request
    |
    +-- classify changed paths
    +-- static: format, lint, documentation links
    +-- domain: pure tests grouped by ownership
    +-- integration-linux: focused daemon, socket, journal, tmux journeys
    +-- portability-macos: OS-specific contracts
    +-- conditional: website, installer, parity, tmux adapter
    `-- required summary: one stable result

nightly or manual
    |
    +-- complete Linux and macOS suites
    +-- tmux HEAD
    +-- performance history
    +-- soak and interruption cleanup
    `-- installer and release trace
```

This is separation by reason to change and failure mode. It should need only a
small classifier and explicit commands, not a large workflow framework.

## Staged CI improvement plan

### Stage 0: expose the current signal

Do not change coverage yet.

1. Add pull-request concurrency cancellation.
2. Emit nextest JUnit and short-lived artifacts.
3. Print job and executable duration summaries.
4. Record Rust and tmux versions with performance reports.
5. Check exactly for resources owned by the current test run.
6. Give each test class one documented local command.

Exit when one run identifies the slowest tests, failure class, and leaked
resources.

### Stage 1: remove measured duplication

1. Replace the complete relocated-root rerun with focused override evidence.
2. Remove the zero-doctest pull-request step.
3. Move tmux HEAD to scheduled, manual, and relevant changes.
4. Make website and installer work change-aware.
5. Keep the complete OS matrix on a schedule.

The duplicate step cost a measured 3 minutes 21 seconds on Linux and 3 minutes
20 seconds on macOS. Replacing it would remove that measured work.

Remeasure before changing parallelism. The proposed exit is a required path
under nine minutes with the same injected defects detected.

### Stage 2: organize around stable contracts

1. Inventory daemon tests by domain contract and seam.
2. Merge cases that differ only in setup into table-driven traces.
3. Rename milestone and gate files using durable domain language.
4. Replace fixed sleeps with explicit events or virtual time.
5. Move performance and evidence collection out of correctness commands.
6. Delete an incident test only after a stronger trace catches its defect.

Exit when every retained test has a distinct contract, owner, and lane.

### Stage 3: deepen production modules

1. Establish narrow interfaces for messaging, observation, terminal effects,
   client transport, and presentation.
2. Move transition combinatorics into their owning module.
3. Replace semantic source scans with dependency structure or simple lints.
4. Reduce cross-domain fixtures and private-state assertions.

Exit when a behavior change has one implementation home, one primary
regression suite, and one small integration trace.

### Stage 4: set the final budget from evidence

Only after the earlier stages:

- target a five-to-six-minute required pull-request result;
- keep relevant warm pure feedback below one minute;
- retain complete scheduled evidence;
- require zero test-owned resource residue; and
- compare defect detection before and after consolidation.

The timing targets are proposals, not promises. Measure them with the same
workflow and workload.

## Test changes to remove, retain, and add

### Remove after replacement evidence exists

- Complete second Rust run under `CYCLOPS_TEST_TMP`.
- Zero-doctest pull-request command.
- Full tmux-HEAD work on unrelated changes.
- Semantic source scans after a deep interface makes duplication inaccessible.
- Milestone-named cases subsumed by stronger domain traces.
- Fixed sleeps used only to make scheduling more likely.
- Duplicate integrations that assert the same observable contract.

### Retain

- Durable mailbox replay and recovery evidence.
- Exact claim and reply-ancestry checks.
- Crash-point tests for maybe-landed terminal effects.
- Human-input preservation and terminal-safety journeys.
- Real peer-credential and filesystem portability checks.
- Real tmux contracts.
- Documentation and binary parity where exact output is promised.
- Installer and uninstall restoration evidence when relevant.

### Add

- Superseded-run cancellation.
- Per-test duration reporting.
- Exact test-resource leak detection.
- Regression review checklist.
- Domain-to-test ownership map.
- Scheduled performance history with stable workloads.

## Review rule for every new test

Before accepting a test, answer:

1. What user-visible or durable contract failed?
2. Can the failure be reproduced deterministically?
3. What is the least expensive honest seam?
4. Which existing test almost covers it?
5. Can that test be strengthened instead?
6. What would make the new test obsolete?
7. Does it need a real clock, process, filesystem, socket, or tmux server?
8. How is every owned resource removed after interruption?

If those answers are unclear, diagnose the contract before adding another
test.

## Recommendation status before the charter

The following table is a proposed disposition. The architecture charter must
reverify and approve it against current HEAD.

| Recommendation | Proposed status |
|---|---|
| Preserve durable mailbox, identity, FIFO, claims, replay, and uncertainty | Approve |
| Keep a modular monolith and one local coordinator | Approve |
| Establish an internal deep `WorkspaceMessaging` module first | Approve |
| Consolidate official daemon-client transports | Approve after frame-contract verification |
| Remove `Arc<Inner>` reach-through incrementally | Approve |
| Separate pure presentation from journal, socket, terminal, and tmux IO | Approve |
| Treat sidebar and Messages pane as optional views | Approve |
| Preserve bounded pane preview and measure only its grammar | Approve with measurement |
| Correct the absolute terminal-safety wording | Approve after reproducing the residual race |
| Quarantine legacy delivery | Approve after caller and replay census |
| Add a production agent runner | Unverified; run one host pilot first |
| Add a stdio MCP adapter | Unverified; run identity and timeout pilot first |
| Extract `cyclops-delivery-core` immediately | Reject as default; require independent crate value |
| Add a distributed broker or generic event bus | Defer without a multi-host requirement |
| Automatic raw-tmux fallback | Reject |
| Generic workflow management | Reject |
| Broad rewrite | Reject |

## Turning the review into implementation

The primary review and this addendum are strong research inputs. They are not
safe authorization for an open-ended "refactor everything" goal.

### Current authority conflict

`docs/development/NEXT.md` approves immediate delivery-core extraction. The
newer review recommends a deep internal module first.

That conflict must be resolved in one approved charter before production work
begins.

The documentation also contained stale links after the review moved from
`docs/development/` to `docs/`. The exact current link-check result and
worktree state must be refreshed rather than copied from an older conversation.

### Goal 1: make the architecture executable

Use a fresh architecture session to produce a documentation-only refactor
charter.

The charter should:

1. Record the exact reviewed commit and dirty worktree.
2. Reverify every reported P1 against current HEAD.
3. Resolve internal `WorkspaceMessaging` versus immediate crate extraction.
4. Separate messaging architecture from CI architecture.
5. Record behavior and durable semantics that must remain stable.
6. Classify every proposal as approved, deferred, rejected, or unverified.
7. Define independently verifiable milestones.
8. Choose one small first tracer bullet.
9. Specify regression evidence, repository gates, rollback, and stop conditions.
10. Require every extraction to delete knowledge from callers.
11. Repair authority, paths, and indexes only after approval.

No production code should change during this goal.

### Goal 2: implement one approved milestone

After the charter is approved, start another focused session that implements
only Milestone 1.

That session should:

- preserve public behavior, durable history, identity, receipts, and safety;
- route one narrow production path through the approved seam;
- strengthen the smallest honest regression evidence;
- avoid later milestones and unrelated cleanup;
- retain compatibility until its callers and history are known; and
- report behavior preserved, knowledge removed, evidence, and remaining risk.

Each milestone must leave the repository coherent if later work stops.

## Bottom line

Cyclops has a strong messaging spine and too much scar tissue around it.

The right response is not a rewrite, another framework, or a larger test pile.
It is a sequence of small, evidence-backed changes that deepen ownership,
separate views and mechanisms, and delete knowledge from callers.

Messaging must stand without the UI. The UI must make messaging understandable
without owning it. Activation must remain optional.

Raw tmux must remain an honest emergency path. Tests must prove durable
contracts at the cheapest truthful seam.

That direction produces the system the project wants: reliable, correct,
understandable, ready for change, efficient for agent coordination, and useful
to a person supervising the work.
