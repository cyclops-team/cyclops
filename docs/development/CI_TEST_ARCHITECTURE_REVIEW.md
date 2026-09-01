# Cyclops CI and test architecture review

**Audit date:** 2026-08-29

**Code reviewed:** `ead57c1691371a1deca5afeb89e90e8340accb69`

**Status:** Supporting design record. The implemented [CI contract](CI.md)
controls current behavior; this review remains evidence and rationale, not
messaging authority.

**Primary evidence run:** [GitHub Actions run 33241643555](https://github.com/cyclops-team/cyclops/actions/runs/33241643555)

## Executive verdict

Cyclops has valuable regression evidence, but its CI and test system is too
large, too repetitive, and too shaped by the history of individual fixes.
The central problem is not simply "too many tests." It is that too many tests
prove nearly the same behavior through expensive process and tmux rigs, while
several architectural rules are guarded by source-text scans that openly admit
they are not proofs.

The result is a 12-minute pull-request gate that consumes about 38 runner
minutes per change, frequently reruns the same Rust suite under a different
temporary directory, runs an advisory tmux build against every change, and
performs zero doctests after building the documentation harness. Of the 14
completed runs immediately before the primary evidence run, 5 failed. Several
follow-up commits are named for stabilizing CI fixtures rather than product
behavior.

This is not a reason to weaken correctness. It is a reason to make each test
prove one durable contract at the least expensive honest seam, and to reserve
real tmux, cross-process, performance, portability, installer, and soak work
for the changes and schedules that need them.

The blunt assessment is:

- The suite contains real, important regression tests.
- The suite also contains repeated, chronology-shaped, and patch-shaped tests.
- CI spends substantial time re-proving facts that an earlier step already
  proved.
- Green source scans can create confidence without proving the runtime
  property they name.
- Test fixture ownership is not yet trustworthy after interrupted runs.
- The current layout makes failures harder to interpret than they should be.

A good target is not the fewest tests. It is the smallest set of clear tests
that makes a wrong implementation difficult to ship and a correct change easy
to understand.

## Principles used

This review applies `UNIFIED-CONTEXT.md` directly:

- Safe from bugs, easy to understand, ready for change.
- Use the simplest design that satisfies a real contract.
- Add a regression check when it reproduces the defect reliably and protects
  an important contract without disproportionate cost.
- Prefer another verification method when an automated check would be brittle,
  misleading, or weaker.
- Match verification depth to risk and reach.
- Measure optimization before and after, including correctness and reliability.
- Do not create tests, abstractions, or artifacts merely to demonstrate
  compliance.

The software-design test is locality: when a messaging rule changes, an
engineer should know which domain module owns it, which small test set proves
it, and which user journey confirms its integration. A change should not
require editing or rerunning unrelated rooms of the house.

## Scope and method

This was a read-only audit. It covered:

1. The only GitHub Actions workflow, `.github/workflows/ci.yml`.
2. Cargo and nextest configuration.
3. Test counts, file sizes, names, fixtures, sleeps, polling, process ownership,
   and architectural scans.
4. The completed workflow for the exact reviewed revision.
5. Recent workflow outcomes and failing job names.
6. Warm local timings on macOS with an Apple M5 Pro, Rust 1.97.1, and tmux
   3.6a.
7. Live process and tmux state after test activity.

Local timings are diagnostic measurements, not claims about GitHub-hosted
hardware. The GitHub timings below are the decision-grade baseline for CI.

## Measured baseline

### Current test inventory

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

The count alone does not prove waste. The stronger evidence is where the tests
live. Several implementation files have become implementation plus a very
large embedded test program:

| File | Total lines | Embedded test module starts | Approximate test-module lines |
|---|---:|---:|---:|
| `src/cyclopsd/src/mailbox.rs` | 18,050 | 8,055 | 9,996 |
| `src/cyclopsd/src/delivery.rs` | 15,931 | 9,710 | 6,222 |
| `src/cyclopsd/src/fusion.rs` | 11,111 | 4,781 | 6,331 |
| `src/cyclops-workspace/src/app.rs` | 12,716 | 6,935 | 5,782 |
| `src/cyclopsd/src/server.rs` | 4,962 | 2,567 | 2,396 |

This is poor locality. A reader cannot quickly tell whether a behavior is
proved once, which variants are distinct, or whether an old incident check has
been superseded.

### Current workflow cost

The primary evidence run took 12m 20s wall-clock. Its jobs consumed about 38
runner minutes in total. The required jobs passed; the advisory tmux-HEAD job
failed:

| Job | Total | Expensive step |
|---|---:|---:|
| macOS Rust gate | 12m 16s | Rust tests 6m 30s; relocated rerun 3m 20s |
| Linux Rust gate | 10m 32s | Rust tests 5m 28s; relocated rerun 3m 21s |
| tmux HEAD advisory | 6m 27s | full Rust gate 5m 32s |
| macOS installer | 4m 58s | installer trace 4m 23s |
| Linux installer | 3m 14s | installer trace 2m 55s |
| Website | 21s | entire job 21s |

The critical path in this run was the macOS Rust job. Parallel jobs hide cost
from the person waiting, but they do not remove compute, noise, or failure
surface.

### Warm local timings

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

The daemon suite dominates local feedback. Within it, the slowest observed
executables were `attention_recovery` at 31.9s,
`messaging_coordinator` at 15.9s, `lifecycle` at 13.0s, and the daemon library
tests at 11.9s.

The performance checks are also deliberately serialized in nextest and include
real output floods and paced sampling. One active check takes about six
seconds. Those checks have value, but they measure a different risk from
ordinary correctness.

### Recent CI stability signal

Five of the 14 completed workflow runs immediately before the primary evidence
run failed. The primary run's required result was green while its advisory
tmux-HEAD job failed
`a_transient_hidden_frame_does_not_bypass_the_active_human_hold`: the pane still
displayed the staged human draft after the fixture sent the key that was
expected to hide that frame. Earlier failures
landed in the ordinary Rust gate, the duplicate relocated gate, and the
advisory tmux-HEAD gate. Recent commit subjects include:

- `test(ci): remove runtime session startup race`
- `test(ci): isolate backspace release on tmux head`
- `test(ci): separate composer release and staging latches`
- `test(ci): pin final composer deletion boundary`
- `test(ci): synchronize rig startup and gate evidence`

This does not prove every failure was a flaky test. It does prove that fixture
and scheduling behavior is consuming a significant part of the development
loop. A gate that frequently needs its own repair is not giving clean product
signal.

## Findings

### P1: CI executes the complete Rust evidence twice on both operating systems

The `CYCLOPS_TEST_TMP` step reruns non-daemon tests, all daemon tests, and
doctests after the ordinary gate already ran them. On the evidence run, this
cost 3m 21s on Linux and 3m 20s on macOS.

The property under test is narrow: all scratch paths must honor one configured
root. The repository already has `src/cyclopsd/tests/scratch_override.rs`, whose
documentation calls itself the one test that can fail when the override stops
working. Running UI rendering, pure state transitions, parsing, and every
unrelated process trace again is not proportional verification.

**Recommendation:** keep the exact override test, add one representative
socket/daemon/tmux journey under the relocated root, and add a small static
architecture lint that rejects test scratch creation outside the canonical
helper. Delete the complete relocated rerun after proving those checks fail
against a deliberately broken helper.

### P1: CI has no cancellation for obsolete runs

There is no top-level `concurrency` rule. A new commit does not cancel the old
run for the same pull request, so several 10-minute workflows can continue
proving revisions nobody can merge.

**Recommendation:** cancel superseded pull-request runs by workflow and pull
request number. Keep release and manually dispatched proof runs in a separate
group that is never cancelled.

This change improves feedback and cost without changing test coverage.

### P1: Fixture cleanup is source-checked but not interruption-proof

The audit found 8 live `cyc-*` tmux servers and 8 fake-terminal Python
processes orphaned with parent PID 1 for more than four hours, plus 37
test-named tmux socket files. The live names came from attention recovery
scenarios. They are evidence of interrupted or abnormal test runs, not proof
that the latest successful run leaked them. A focused rerun of
`crash_after_intent_stays_ambiguous_without_a_second_key` passed and added no
new server or socket.

`cyclops-testrig::TmuxServer` has a careful `Drop` implementation, and a source
scan requires tmux teardown to have one Rust home. That protects normal return
and panic unwinding. It cannot protect process termination before destructors
run. The live resources demonstrate that the operational property is stronger
than the source rule.

**Recommendation:**

1. Give each test executable an external cleanup owner that can remove only
   resources bearing that executable's unique run id.
2. After every real-tmux executable, assert that it added no surviving tmux
   server, socket, shell, or fake-terminal process.
3. Run the same check after a deliberately interrupted fixture in one focused
   regression trace.
4. Keep cleanup targets exact. Never clean all tmux state or a broad temporary
   directory.

This is a real regression contract: after success, failure, panic, or forced
interruption, the test's resources are gone and the user's resources are
untouched.

### P1: Architecture is sometimes enforced by semantic source-text scans

`src/cyclops-proto/tests/one_place.rs` is more than 1,100 lines of Rust that scans
Rust source for shapes resembling a duplicated attention rule. Its own
documentation correctly says it is not proof, lists known holes, and records
that a previous verifier defeated it while every gate stayed green.

This is a costly symptom of shallow module interfaces. Callers can still
reimplement the rule, so a second test program tries to recognize equivalent
source text. That test is brittle by construction and makes ordinary refactors
look architecturally dangerous.

`tests/testrig/tests/teardown_has_one_home.rs` and
`src/cyclops-workspace/tests/guards.rs` use similar source scans. Some static
architecture lints are useful, but they must be named and reported as lints,
not as runtime regression evidence.

**Recommendation:** expose a deep, canonical interface for the attention
decision and make every consumer depend on it. Once call sites cannot obtain
the required answer without that interface, delete the semantic scanner. Keep
only simple import or forbidden-dependency lints where the rule is actually
syntactic.

### P1: Test organization follows incident chronology instead of the domain

Daemon integration files include `m0.rs`, `m1.rs`, `m1_fixes.rs`,
`m1_blockers.rs`, `m2_history.rs`, `m2_hooks.rs`, `m2_wait.rs`, `m4_name.rs`,
`m5_theme.rs`, `m6_manifests.rs`, `m6_receipts.rs`,
`gate3_release_proof.rs`, and `stage1_unread_projection.rs`.

Those names preserve development history, but they do not tell a new engineer
which domain owns the behavior. Milestones end. Delivery acceptance, claims,
notification, attention recovery, session identity, and terminal injection
remain.

Chronological files make it easy to add the next fix beside the last fix. They
make it hard to see that a newer table or trace already subsumes the old case.

**Recommendation:** organize regression tests by stable domain behavior and
user-visible contract. Move historical reasoning into short comments or the
stabilization record. Rename before adding more tests, then merge duplicate
fixtures and cases while preserving the strongest assertion.

### P1: Oversized implementation modules generate oversized test programs

The test problem cannot be solved only in CI. `mailbox.rs`, `delivery.rs`,
`fusion.rs`, `app.rs`, and `server.rs` each own too many decisions. Their test
sections then need hundreds of cases because every change interacts with a
wide internal representation.

This is the house problem in code: the rooms exist, but too much plumbing and
furniture have accumulated in a few of them. Adding another test to the living
room does not make the toilet belong there.

**Recommendation:** deepen the domain modules described in the messaging
architecture review. Keep state transitions, durable facts, coordination, tmux
transport, and presentation decisions behind narrow interfaces. Test each
implementation through its contract. Keep a small number of integration
traces across seams. Better modularity should delete test setup and repeated
assertions, not create more abstractions.

### P2: Correctness, performance, soak, and compatibility risks share one lane

The default nextest configuration serializes performance executables because
they use real clocks and output floods. The tmux-HEAD job builds upstream tmux
and reruns nearly the complete Rust gate on every push and pull request even
though the job is advisory. Soak and release-benchmark files sit beside normal
regression files, with active helper checks and ignored long scenarios mixed
together.

**Recommendation:** separate test classes by the risk they answer:

| Test class | Question | When |
|---|---|---|
| Domain trace | Is the state transition or durable fact correct? | Every change |
| Adapter contract | Does an implementation honor a narrow interface? | Every relevant change |
| Process integration | Do daemon, journal, socket, and client agree? | Every relevant change |
| Real tmux journey | Does the actual terminal path work? | Small required set |
| Portability | Does OS-specific behavior work? | Relevant changes plus nightly |
| Performance | Did a measured distribution or resource limit regress? | Scheduled and before release |
| Soak and upstream compatibility | Does it survive duration or upstream tmux churn? | Scheduled and manual |
| Installer | Can a clean user install and uninstall safely? | Installer changes and release |

The classes should have separate commands and reports. A failure should say
what kind of product risk was found.

### P2: Full macOS duplication is broader than the portability claim

The workflow comment correctly names peer credentials and scratch paths as
real OS differences. It then runs almost everything on both systems, including
pure parsing, pure state transitions, rendering calculations, and source
scans.

**Recommendation:** run pure domain tests once. Run a named macOS portability
set for credentials, filesystem semantics, scratch location, tmux adapter
behavior, shell profiles, and terminal behavior. Run the complete macOS set on
a schedule and before release until the narrower set has demonstrated equal
fault detection.

### P2: The workflow runs zero doctests

The current doctest command builds and launches documentation tests across the
workspace, but Cargo lists zero doctests.

**Recommendation:** remove the command from the pull-request gate. Restore it
when the repository contains executable documentation examples. Existing
documentation parity is better protected by the dedicated parity trace.

### P2: Website, installer, and tmux HEAD run without change awareness

The website is cheap but unrelated to most Rust changes. Installer jobs cost
2m 52s and 4m 40s. The advisory tmux-HEAD job costs 6m 18s. All run for every
change.

**Recommendation:**

- Run website checks when website, installer-copy, or shared public assets
  change.
- Run installer checks when installer scripts, installation docs, packaged
  resources, Cargo metadata, or release wiring change.
- Run tmux HEAD nightly, manually, and when the tmux adapter or its manifests
  change.
- Keep a required status with a stable name even when a conditional job has no
  work, so branch rules stay understandable.

### P2: CI does not publish a useful performance history

The repository prints performance numbers but does not retain a machine-
readable report, compare a stable workload with a baseline, or show which tests
own the critical path. There is no nextest JUnit report or timing artifact in
the workflow.

**Recommendation:** publish per-test duration and retry-free outcome data from
nextest, plus explicit benchmark results from the scheduled performance lane.
Track p50, p95, maximum, sample size, operating system, Rust version, tmux
version, and revision. Do not gate ordinary pull requests on tight wall-clock
budgets from shared runners.

## What a real regression test means

A Cyclops regression test should meet all of these conditions:

1. **Names a durable contract.** The name describes accepted, claimed,
   recovered, isolated, or rendered behavior, not a milestone or ticket.
2. **Fails before the fix.** The author demonstrates that the old
   implementation fails for the intended reason.
3. **Uses the least expensive honest seam.** A pure transition test does not
   boot tmux. A tmux behavior test does not pretend a fake proves tmux.
4. **Controls the race.** Faults and ordering are injected through events,
   barriers, or virtual time instead of hoping a sleep lands correctly.
5. **Asserts observable facts.** It checks durable records, protocol answers,
   process ownership, or user-visible output rather than private call order.
6. **Owns its resources.** It leaves no process, socket, session, journal, or
   temporary path behind.
7. **Has one reason to fail.** Setup failure, performance drift, and behavior
   failure are distinguishable.
8. **Is not duplicated without a distinct claim.** Repeating a scenario at
   another seam is justified only when it proves a different risk.

Passing checks are evidence, not the architecture. The implementation should
make invalid behavior difficult; tests should demonstrate that design, not
compensate indefinitely for its absence.

## Desired test architecture

### 1. Domain traces

Put mailbox lifecycle, claims, replies, attention, delivery state, and recovery
rules into deterministic tables or traces with no tmux, shell, socket, or real
clock. These should be the majority of cases and complete in seconds.

For a trace, record:

- starting durable facts;
- command or event;
- resulting fact or refusal;
- emitted coordination event;
- permitted next commands.

One table can replace many tests that each spell a single transition with
nearly identical setup.

### 2. Adapter contract suites

Each external implementation should have a reusable contract:

- journal append and recovery;
- socket request and subscription ordering;
- tmux pane observation and key injection;
- clock and deadline behavior;
- process identity and OS credential lookup.

Run the contract against a deterministic in-memory implementation where that
is honest, then against the real implementation in a smaller set. The goal is
leverage, not a mock of everything.

### 3. Focused process traces

Keep a compact daemon suite for the contracts only a real process can prove:

- acceptance survives restart;
- exactly one claim wins;
- reply ancestry survives replay;
- an uncertain terminal action remains uncertain after restart;
- socket authorization follows real peer credentials;
- shutdown and interruption leave no owned resources.

Use injected pauses already present in the code to choose the exact crash
point. Avoid fixed multi-second sleeps when the test can await an explicit
event.

### 4. Small real-tmux user journeys

The required pull-request set should prove a few complete journeys:

1. send, durable acceptance, notification, exact claim;
2. busy recipient, queued notification, later wake;
3. staged human text is never overwritten;
4. daemon restart and raw-tmux recovery remain understandable;
5. narrow and wrapped terminal output remains usable;
6. cleanup preserves the user's tmux state and removes the rig's state.

Do not multiply these journeys for every internal state. Domain traces own the
combinatorics.

### 5. Scheduled evidence

Run full OS coverage, performance, soak, tmux HEAD, and release installation on
a schedule and on demand. A release candidate should run all of them. An
ordinary documentation or pure-domain change should not.

## Proposed CI shape

```text
pull request
    |
    +-- classify changed paths
    |
    +-- static: fmt, clippy, documentation links
    |
    +-- domain: pure tests, sharded by crate ownership
    |
    +-- integration-linux: focused daemon, socket, journal, tmux journeys
    |
    +-- portability-macos: OS-specific contracts only
    |
    +-- conditional: website, installer, parity, tmux adapter
    |
    `-- required summary: one stable pass/fail result

nightly or manual
    |
    +-- complete Linux and macOS suites
    +-- tmux HEAD
    +-- performance history
    +-- soak and interruption cleanup
    `-- installer and release trace
```

This is separation by reason to change and failure mode. It is not a request
for more workflow machinery. One small change classifier and a few explicit
commands are enough.

## Staged improvement plan

### Stage 0: make the existing signal visible

No coverage changes:

1. Add pull-request concurrency cancellation.
2. Emit nextest JUnit and upload it as a short-lived artifact.
3. Print job and executable duration summaries.
4. Record Rust and tmux versions with every performance report.
5. Add an exact post-test check for resources created by the current run id.
6. Give each test class one documented local command.

Exit criterion: the team can identify the slowest 20 tests, the failure class,
and any leaked test resource from one run.

### Stage 1: remove measured redundant execution

1. Replace the complete relocated-root rerun with the override test, a helper
   lint, and one representative real journey.
2. Remove the zero-doctest step.
3. Move tmux HEAD to nightly/manual and tmux-adapter changes.
4. Make website and installer work change-aware.
5. Keep one full operating-system matrix on a nightly schedule.

The first item alone removes 3m 21s from Linux and 3m 20s from the measured
macOS critical path. After this stage, remeasure before changing test
parallelism.

Exit criterion: the required pull-request critical path is below nine minutes
on the same workload, with the same deliberately injected faults detected.

### Stage 2: consolidate around stable domain contracts

1. Inventory every daemon integration test by domain contract and seam.
2. Merge cases that differ only in setup into table-driven traces.
3. Rename milestone and gate files by durable domain language.
4. Replace fixed sleeps with injected events or virtual time where possible.
5. Move performance and evidence-collection scenarios out of default
   correctness commands.
6. Delete old incident tests only after a stronger trace is shown to fail
   against the original defect.

Exit criterion: every retained test has a distinct contract, owner, and reason
to run in its assigned lane.

### Stage 3: deepen the production modules

1. Give messaging acceptance, mailbox projection, notification coordination,
   attention recovery, and terminal transport narrow interfaces.
2. Move transition combinatorics into their owning domain modules.
3. Replace semantic source scans with dependency structure or simple static
   lints.
4. Reduce cross-domain fixtures and duplicated private-state assertions.

Exit criterion: a behavior change has one obvious implementation home, one
primary regression suite, and a small integration trace.

### Stage 4: set and validate the final budget

Only after the earlier stages:

- target a required pull-request result in five to six minutes;
- keep pure local feedback under one minute when relevant artifacts are warm;
- retain complete scheduled evidence;
- require zero test-owned process and socket residue;
- compare defect detection before and after consolidation.

The five-to-six-minute target is a proposal, not a measured promise. The first
stage has a measured saving; later savings must be demonstrated with the same
workflow and workload.

## What to remove, retain, and add

### Remove after replacement evidence exists

- The complete second Rust run under `CYCLOPS_TEST_TMP`.
- The zero-doctest pull-request command.
- Full tmux-HEAD validation on every unrelated change.
- Semantic source-text scanners once interfaces make the duplicate rule
  inaccessible or a simple static lint can enforce the dependency.
- Milestone-named regression cases subsumed by stronger domain traces.
- Fixed sleeps used only to make scheduling more likely.
- Duplicate integration scenarios that assert the same observable contract.

### Retain

- Durable mailbox replay and recovery evidence.
- Exact claim and reply ancestry checks.
- Injected crash-point tests for maybe-landed terminal actions.
- Human-input preservation and terminal-safety journeys.
- Real peer-credential and filesystem portability checks.
- Real tmux adapter contracts.
- Documentation and binary parity traces where documentation promises exact
  output.
- Installer and uninstall restoration evidence for relevant changes and
  releases.

### Add

- Superseded-run cancellation.
- Per-test duration reporting.
- Exact test-resource leak detection.
- A regression-test review checklist using the eight conditions above.
- A domain-to-test map that names the primary suite for each durable contract.
- Scheduled performance history with stable workloads and environment facts.

## Review rule for every new test

Before accepting another test, answer:

1. What user-visible or durable contract failed?
2. Can the failure be reproduced deterministically?
3. What is the least expensive honest seam?
4. Which existing test almost covers it?
5. Can that test be strengthened instead of adding another?
6. What would make this new test obsolete?
7. Does it use a real clock, process, filesystem, socket, or tmux server? If
   so, why is that dependency necessary?
8. How is every owned resource removed after interruption?

If those answers are unclear, the right next action is diagnosis, not another
test.

## Bottom line

Cyclops needs fewer repeated proofs and stronger primary proofs.

The fastest safe improvement is to stop running the complete suite twice,
cancel obsolete workflows, move advisory and performance work to appropriate
schedules, and detect leaked resources directly. The deeper improvement is to
organize implementation and tests around stable messaging domains so a bug fix
strengthens one understandable contract instead of adding another historical
test island.

That produces the outcome the project actually wants: a smaller CI story,
faster feedback, clearer failures, safer changes, and higher confidence in the
agent communication path.
