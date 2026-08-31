# CI evidence lanes and measured baseline

**Status:** Current CI contract

Cyclops separates fast pull-request correctness from conditional integration,
scheduled reliability, and release evidence. A lane may be cheaper than the
one that preceded it, but it must still name the durable contract it protects.
Replacement evidence protects the original defect or durable contract. A small
local regression simulation is useful when practical; Cyclops does not maintain
a general mutation-testing framework.

## Task 1 baseline

The baseline is GitHub Actions run
[33275472898](https://github.com/cyclops-team/cyclops/actions/runs/33275472898),
first attempt, at commit `b9543a417262015950fd63ec41463fb9e2618397`.
Reproduce the measurements with:

```bash
python3 scripts/ci-baseline.py 33275472898 --attempt 1 --sample 30
```

| Measure | Baseline |
|---|---:|
| Pull-request wall time | 10m 38s |
| Total runner time | 32m 29s |
| Recent pull-request runs sampled | 30 |
| Failed runs | 14 (46.7%) |
| Explicitly rerun runs | 1 (3.3%) |
| Cancelled runs | 0 |

Failure frequency is a workflow outcome, not a claim that every failure was a
flake. Explicit reruns are GitHub rerun attempts; follow-up commits are separate
runs and are not counted as reruns.

| Stable check | Baseline duration | Responsibility |
|---|---:|---|
| `test (ubuntu-latest)` | 10m 38s | Required pull-request correctness |
| `test (macos-latest)` | 8m 58s | Conditional platform and tmux evidence |
| `installer (ubuntu-latest)` | 2m 49s | Conditional installer integration |
| `installer (macos-latest)` | 3m 27s | Conditional installer integration |
| `website` | 18s | Conditional website integration |
| `tmux-head` | 6m 19s | Conditional focused tmux HEAD evidence |

The Task 1 workflow ran every check for every change. The two operating-system
test jobs each ran the complete Rust suite, then repeated it under a relocated
scratch root. The advisory tmux HEAD job built upstream tmux and repeated nearly
the same complete Rust gate a third time on Linux. No scheduled or release
workflow existed.

## Task 2 deterministic replacement evidence

The relocated-root step no longer repeats the complete Rust suite. It now runs
`scripts/test-relocated-scratch.sh`, which carries three distinct proofs:

1. `scratch_override` proves `CYCLOPS_TEST_TMP` selects the root and that empty
   values retain the platform default.
2. `scratch_path_lint` is explicitly syntactic. It rejects direct platform-temp
   calls and hardcoded `/tmp` or `/private/tmp` literals in any Rust fixture that
   owns a real `cyclops-testrig` server.
3. `m0_shadow_daemon_end_to_end` runs one real tmux server, in-process daemon,
   Unix socket, and scratch home under the relocated root.

The former relocated step cost 3m21s on Linux and 3m20s on macOS. In Task 2 it
cost 2 seconds on each platform. The complete pull-request run changed from
10m38s wall time and 32m29s runner time to 8m28s and 27m23s. That is 2m10s
(20.4%) less wall time and 5m06s (15.7%) fewer runner minutes without removing a
named defect class.

Tmux server names now include the executable pid and an in-process sequence.
One external owner per test executable records only those exact names. The
`interrupted_owner` regression kills a fixture before Rust destructors run,
observes that its server and socket disappear, and proves a neighboring server
survives. Ordinary return, already-gone server, and panic cleanup remain covered
by the existing teardown tests.

`scratch_path_lint`, `teardown_has_one_home`, and the workspace guards enforce
literal prohibited calls or ownership boundaries. They are syntactic
architecture lints. `src/cyclops-proto/tests/one_place.rs` remains a semantic
tripwire whose own header states what source shapes it cannot recognize. It does
not replace domain tests or review.

The named backspace regression waits for the positive `write_ready` event that
actually wakes the held attempt. Waiting only for the weaker `composer_clean`
projection failed once under the release suite because screen settlement had
not yet produced write permission. A fresh event subscription is established
after the gate reports the hold and immediately before Backspace, so setup
events cannot satisfy the release condition. The test then waits on explicit
injected pre-write and staged phase events.

That evidence exposed a second product race under the complete Linux suite.
The delivery gate recreated its readiness receiver between re-evaluations. An
early pane event could trigger one re-evaluation, then the settled positive
readiness edge could land in the gap before the next receiver existed. The gate
now owns one receiver for its full lifetime, so events published during or
between re-evaluations remain buffered. The focused Backspace regression passed
40 bounded repetitions and the complete 40-test messaging coordinator passed
10 bounded four-thread repetitions after the repair. This synchronization
contains no timing sleep; the 20-second phase timeout only bounds a missing
contract event under four concurrent isolated rigs on a cold shared runner.

A later required Linux run exposed a distinct ordering race in the same
regression. A tokenless observer could publish the positive readiness tuple
before the serialized source recompute carrying the Backspace route token. The
source recompute then saw an unchanged tuple and suppressed reconciliation,
leaving the durable attempt held forever. The wake plan now preserves positive
write-ready or owned-staged causal route evidence while tokenless observers
remain observational and unchanged negative observations remain quiet. A
focused two-observation unit regression simulates that ordering without
timing, tmux, or a mutation framework. After the repair, the real Backspace
journey passed 40 bounded concurrent repetitions and the current 41-test
messaging coordinator passed 10 bounded four-thread repetitions.

A follow-up required run
[33323429830](https://github.com/cyclops-team/cyclops/actions/runs/33323429830)
showed that the regression still waited beyond the contract it named. After
observing the positive readiness edge, it also waited for the worker to reach
the final pre-write boundary, stage the terminal doorbell, and pause before
submit. Those later phases prove terminal injection and presentation, not that
Backspace released the held attempt, and the pre-write wait exhausted its
20-second bound under runner load. The regression now stops at the exact
message-specific `gate: proceed` event produced by that readiness edge and
asserts that the attempt identity did not change. Focused tests separately
protect staged notification bytes, body-free doorbells, and reopened-attempt
injection.

This follow-up removed two injected phase semaphores and one terminal capture;
it added no sleep and did not increase a timeout. Disabling the production rule
that retires an unowned human-input hold after settled visible emptiness made
the narrowed regression fail on the missing readiness event, then restoring
the rule made it pass. The restored test passed 20 serial repetitions, 80
focused repetitions at eight-way process concurrency, and ten complete
four-thread coordinator repetitions. The complete coordinator averaged 15.17s
before and 15.42s after locally, which is no meaningful wall-time change; the
gain is removal of a false 20-second terminal-scheduling dependency. No defect
class moved to scheduled or release evidence.

Milestone 7 then reran the same daemon code in required run
[33325229205](https://github.com/cyclops-team/cyclops/actions/runs/33325229205)
and exposed one remaining false dependency. The narrowed regression still
required `gate: proceed`, but that decision also requires a fresh foreground
process binding after the composer hold has already been released. Binding
doubt may honestly replace the composer-era hold with `occupant_unprovable`;
it does not mean Backspace failed to release the same attempt.

Required Linux run
[33327467320](https://github.com/cyclops-team/cyclops/actions/runs/33327467320)
showed that the replacement gate decision was still a false dependency. The
real-tmux path had already observed the positive Backspace readiness edge, then
waited for an ephemeral delivery-worker decision whose durable consequences are
protected more cheaply below that adapter.

The final proof is split at the actual ownership boundaries. The real-tmux
journey establishes a fresh event subscription after the initial exact gate
hold, sends Backspace, requires the positive readiness edge, and verifies that
the attempt identity did not change or requeue. The deterministic
`causal_route_evidence_survives_an_earlier_tokenless_readiness_observation`
test protects the source edge,
`workspace_messaging_applies_a_readiness_route_observation` protects the Module
boundary, and
`blocked_readiness_reopens_once_only_after_positive_exact_route_evidence`
protects one-time reopening of the same durable attempt. Event waits now name
their boundary if either physical event is absent.

A small local regression simulation removed the rule that preserves positive
causal route evidence after an earlier tokenless readiness observation. The
fusion test failed at that exact lost-edge assertion, then passed when the rule
was restored. The real-tmux journey passed 200 bounded focused repetitions at
16-way process concurrency, and the complete 41-test coordinator passed ten
bounded four-thread repetitions. No timeout increased, no sleep was added, and
no defect class moved to scheduled or release evidence.

Required macOS runs
[33322158633](https://github.com/cyclops-team/cyclops/actions/runs/33322158633)
and
[33326783742](https://github.com/cyclops-team/cyclops/actions/runs/33326783742)
exposed a separate false dependency in the real workspace boot-sizing test.
That regression already records the first target-side resize in an immutable
tmux hook, but it waited for unrelated terminal text before reading the hook.
The same test passed on adjacent macOS runs and in 20 focused local repetitions,
confirming that paint scheduling did not honestly prove the sizing contract.

The test now waits directly for the recorded cold-boot resize and asserts both
that first event and the converged window size. It no longer treats a rendered
`Chat 0 !0` line, alternate-screen state, or terminal teardown as sizing
evidence. Changing the persisted preference to `messages_visible = false` in a
local regression simulation made the replacement fail with the collapsed
width `(95, 26)` instead of the required open width `(72, 26)`. Restored, the
focused test passed 50 bounded repetitions. No timeout increased, no sleep was
added, and renderer coverage remains in the renderer and workspace journey
suites that own it.

Required macOS runs
[33326744941](https://github.com/cyclops-team/cyclops/actions/runs/33326744941)
and
[33329043584](https://github.com/cyclops-team/cyclops/actions/runs/33329043584)
exposed a second repeated false dependency in
`selftest_verifies_with_simulated_hook_edge`. The self-test had already
recorded and verified the simulated hook edge, but its final assertion called
the socket `status` command. That command first attempts a separately bounded
250 ms live tmux refresh and correctly returns
`status_refresh_incomplete` when the refresh cannot finish. Treating that
honest uncertainty as a missing hook edge coupled the regression to unrelated
runner scheduling.

The regression now reads the daemon's cached status projection directly for
the `hooks_verified` assertion. The existing `hooks.verify` request still
checks the public hook-liveness path. A small local simulation that forced the
cached projection to report `hooks_verified = false` made the replacement
fail at the intended assertion. Restored, the focused self-test passed 100
bounded runs at 16-way process concurrency, and the complete five-test hook
suite passed 20 bounded runs at four-way process concurrency. No production
code changed, no timeout increased, and no sleep was added.

## Task 3 representative pull-request comparison

Messaging Milestone 3 provided the first post-merge product change whose diff
did not alter CI control files. GitHub Actions run
[33286752920](https://github.com/cyclops-team/cyclops/actions/runs/33286752920),
first attempt, at commit `3774217fd544add6800fb7c22e6812d3116d5895`,
is the representative routed result. Reproduce it with:

```bash
python3 scripts/ci-baseline.py 33286752920 --attempt 1
```

| Measure | Task 1 baseline | Task 3 routed result | Change |
|---|---:|---:|---:|
| Pull-request wall time | 10m 38s | 8m 46s | 1m 52s less (17.6%) |
| Total runner time | 32m 29s | 16m 15s | 16m 14s less (50.0%) |

The required Ubuntu lane ran for 8m36s. The conditional website, tmux HEAD,
and installer checks each returned an explicit successful not-applicable result
in three to five seconds. The macOS correctness lane ran for 7m16s because the
workflow still allocated a macOS runner before evaluating applicability. The
follow-up runner expression preserves the stable `test (macos-latest)` and
`installer (macos-latest)` names while routing their not-applicable steps to
Linux; substantive macOS evidence remains on macOS.

## Normal pull-request workflow

`.github/workflows/ci.yml` keeps the six stable check names in the baseline.
`scripts/ci-paths.py` classifies the complete pull-request diff. Workflow and
classifier changes select every lane so routing changes prove themselves.
Unknown manual history also selects every lane and fails safe.

| Stable check | Runs substantive evidence when |
|---|---|
| `test (ubuntu-latest)` | Always checks documentation paths; adds Rust, documentation, and exact-output evidence when their inputs change |
| `test (macos-latest)` | A named platform or tmux risk changes |
| `installer (ubuntu-latest)` | Installer, packaged assets, or install docs change |
| `installer (macos-latest)` | Installer, packaged assets, or install docs change |
| `website` | Website, hosted installer, or README-facing inputs change |
| `tmux-head` | The tmux adapter, testrig, manifests, layouts, or Cargo graph changes |

Every check always exists. An unrelated change receives an explicit successful
not-applicable step, so branch protection never depends on a disappearing job.
The documentation-path check is the deliberate exception to conditional work:
it runs for every pull request because deleting or renaming any quoted target
can break a page even when no Markdown file changed. If classification itself
fails, each stable check fails instead of silently skipping.
Pull-request runs share a workflow-and-PR concurrency key and cancel an older
revision. Push and manual evidence use unique keys and never cancel each other.

The Ubuntu correctness lane owns formatting, Clippy, parallel-safe tests,
daemon tests, Rust documentation compilation, documentation paths,
exact-output parity, and focused relocated-root evidence. The normal nextest
filter excludes the six performance executables. Those workloads, plus the
staged install-to-first-durable-handoff journey, retain their own metadata and
history in scheduled and release evidence. The handoff journey never runs in a
pull-request job.

Immediately before its filtered nextest run, the correctness lane builds the
matching `cyclops`/`cyclopsd` pair for
`workspace_cli::start_starts_a_daemon_when_none_is_running`, the explicit
real-daemon start assertion. `workspace_boot_sizing`'s sizing assertion does
not require a daemon and tolerates daemon-start failure.

The old `cargo test --workspace --doc` command compiled every workspace crate
and executed zero doctests. The replacement
`cargo doc --workspace --no-deps` directly protects the intended
documentation-compilation contract. It does not turn pre-existing rustdoc
warnings into a new blocking policy.

### Simplification ledger

| Work removed from every pull request | Contract it attempted to protect | Replacement evidence |
|---|---|---|
| Complete macOS Rust suite | Named portability and tmux behavior | Conditional macOS suite for platform or tmux paths; scheduled full matrix for all other cross-platform drift |
| Website install, check, and build | Hosted installer parity and website type/build correctness | Stable `website` check runs the same commands only for website-facing inputs |
| Installer lifecycle on both platforms | Installation, profile restoration, seeded assets, and uninstall | Both stable installer checks run the same lifecycle only for installer-owned inputs; release evidence repeats it with user journeys |
| Complete Rust gate against tmux HEAD | Upstream tmux adapter compatibility | Pull requests run `cyclops-tmux`, `cyclops-workspace`, and the M0 tmux/daemon/socket journey; the retained `watcher_modes` contract still catches F25; scheduled evidence runs the full fast gate against tmux HEAD |
| Six performance test binaries inside ordinary nextest, plus staged install-to-first-durable-handoff | Frame, queue, control-write, flood, terminal-restoration, daemon cold-boot/replay, concurrent durable mailbox acceptance, quiet-pane observation work, and the installed durable-handoff journey | Scheduled and release runs execute the same binaries and staged journey through `scripts/ci-performance.py` and retain comparable metadata |
| Zero-test doctest execution | Rust documentation compilation | `cargo doc` builds every workspace page; pre-existing warnings remain visible without becoming a new blocking policy |

Path-classifier self-tests simulate unrelated website, installer, daemon,
platform-client, documentation, domain-only, and workflow changes. They assert
both selected and not-applicable lanes. The F25 test remains in the focused
tmux HEAD package, and the performance script has been run locally through all
seven retained workloads with their JSON metadata contracts checked. These are
small contract simulations, not mutation infrastructure.

Run the complete normal gate locally with:

```bash
./scripts/check.sh
```

Run the path classifier's contract examples with:

```bash
python3 scripts/ci-paths.py --selftest
```

## Scheduled evidence

`.github/workflows/scheduled-evidence.yml` runs after every merge to
**beta/messaging-rework**, runs nightly once the workflow reaches GitHub's
default branch, and can be dispatched by lane. GitHub only fires cron workflows
from the default branch, so the beta integration trigger prevents a dormant
replacement lane during the rework. The workflow owns the complete Linux and
macOS matrix, the full fast gate against tmux master, retained performance
workloads, repeated race evidence, forced-cleanup evidence, soak tests, and
long-history workloads.

While the workflow exists only on the beta branch, merging into
**beta/messaging-rework** triggers every scheduled lane. GitHub registers
`workflow_dispatch` only after the workflow reaches the default branch. After
that final integration, dispatch all lanes manually with:

```bash
gh workflow run scheduled-evidence.yml \
  --ref beta/messaging-rework \
  -f lane=all
```

For a focused post-main manual run, replace `all` with `matrix`, `tmux-head`,
`performance`, or `reliability`. The reliability command is also runnable
locally with a bounded repeat count:

```bash
CYCLOPS_CI_REPEAT=10 ./scripts/ci-reliability.sh
```

`scripts/ci-performance.py` records the commit, dirty state, Cyclops version,
operating system, architecture, CPU count, Rust and Cargo versions, tmux
version, runner image, workload command, result, output, and duration. The
`daemon-cold-start-replay` workload records three in-process daemon boot samples
after 0, 1,000, and 10,000 operator-addressed FYI messages, alongside the
workspace journal's byte and line counts. Each timed boot starts from a
validated configuration after a clean daemon shutdown, then a separate
body-free snapshot verifies replayed visibility. It is not a whole executable
startup, client-connect, or terminal-notification measurement. The runner
rejects a zero-exit daemon test that does not emit exactly one complete
`CYCLOPS_DAEMON_COLD_START_REPLAY_JSON` report with the expected schema,
kind, workload, and three replay measurements. Those records cover the 0,
1,000, and 10,000-message journals, each with matching journal counts and
three boot timings; the workload records the body-free replay check. A skipped,
malformed, or incomplete measurement is failed evidence, not a successful
performance artifact. Change that runner only with its focused contract check:

```bash
python3 scripts/ci-performance.py --selftest
```

That fast check also rejects a fixture name response that does not confirm the
exact manifest pin before a retained handoff record can claim it.

GitHub retains the JSON artifact, including failed-evidence diagnostics, for 90
days under a commit-specific name.

The required Ubuntu pull-request check runs that fast self-test when the
performance runner, its path classifier, or its PR workflow wiring changes. It
does not run the performance workloads on ordinary pull requests.

The `concurrent-mailbox-acceptance` workload runs three isolated samples with
four concurrent callers and 32 messages per caller. Each caller waits for its
own durable acceptance before submitting the next message. The retained record
contains the raw per-request and per-workload timings, exact per-caller durable
sequence order, a body-free mailbox snapshot, and observed global interleaving.
It deliberately excludes socket authentication, agent routing, notification,
terminal injection, and end-to-end user-journey timing. Its summaries describe
the retained raw samples; they make no universal latency or fairness bound.

The runner rejects a zero-exit concurrent workload unless its marker identifies
the exact workload shape and contains every expected sequence, body-free
snapshot, caller timing, and internally consistent timing/interleaving summary.

The `idle-observation-counts` workload uses two sequential isolated tmux
fixtures, each with one screen-tier `CAT_MANIFEST` pane running `cat`. The
first sends one literal line to `cat`. That positive control must raise the
watcher-event, recompute, and capture counters. A fresh second fixture then
completes attachment and readiness checks, resets its counters, and observes a
fixed one-second quiet window with no client request or pane output. The
retained marker records the positive-control counts and requires zero
application-level watcher-event wakes, observation-recompute starts, and
state-observation `capture-pane` requests in the quiet window. The counts
deliberately exclude daemon boot, fixture attachment, the separate control
fixture, terminal-delivery and composer-recovery captures, tmux internals, and
operating-system scheduler wakeups. The window is a bounded measurement period,
not a retry used to make the fixture pass. A zero-exit test without the exact
marker, positive control, and final zero counts is failed evidence.

The `install-first-durable-handoff` workload adds one structured measurement
to that artifact. It uses the public source installer from the checked-out
tree with a fresh private prefix, home, tmux server, daemon, fixture agents,
journal, and Cargo target directory. It separately records source build, pair
activation, setup, daemon readiness, session adoption, explicit fixture-manifest
binding, durable send, and authenticated claim. The workload is a staged local-source
install: its Cargo registry and toolchain may be warm, and it does not measure
network download or Rust installation. Its synthetic two-agent fixture is
reported as a separate setup phase and is explicitly bound through the public
`cyclops name --manifest` command. That pin selects fixture rules; the later
send and claim still require live process-ancestry authentication. Readiness
and fixture completion have a
bounded 50 ms test-rig probe interval, so those small phase values are not
latency claims. One staged sample is retained per run, so its p50, p95, and
maximum are the same observed sample rather than a statistical confidence
claim. Compare only artifacts with the same staged workload and recorded
environment. In particular, it makes no sixty-second product claim.

The runner rejects a zero-exit handoff journey unless it emits exactly one
complete `CYCLOPS_INSTALL_FIRST_HANDOFF_JSON` report. That report must identify
the checked-out commit, dirty state, environment, installed matched pair,
staged workload limits, all named phases, durable journal acceptance, and the
recipient's authenticated claim. The fast runner self-test rejects missing
markers, phases, durable proofs, and inconsistent one-sample summaries without
running the performance workloads on a pull request.

## Release evidence

`.github/workflows/release-evidence.yml` does not merge, tag, or publish
anything. It owns full clean-checkout validation on Linux and macOS,
strict and lenient journal replay, daemon historical replay, installer
lifecycle, real parity journeys, and a retained performance comparison.

GitHub registers `workflow_dispatch` only from the default branch. Until this
beta workflow reaches `main`, exercise the release lane by creating its
disposable beta trigger branch at the exact integration commit:

```bash
git fetch origin beta/messaging-rework
release_sha="$(git rev-parse refs/remotes/origin/beta/messaging-rework)"
git push origin "$release_sha":refs/heads/beta/test/release-evidence
```

Identify, watch, and inspect the resulting `release evidence` run before
removing the trigger branch:

```bash
release_run="$(gh run list \
  --branch beta/test/release-evidence \
  --limit 20 \
  --json databaseId,headSha,workflowName \
  --jq "[.[] | select(.workflowName == \"release evidence\" and .headSha == \"$release_sha\")][0].databaseId")"
if [ -z "$release_run" ]; then
  echo "release evidence for $release_sha is not registered yet" >&2
  exit 1
fi
gh run watch "$release_run" --exit-status
gh run view "$release_run"
git push origin --delete beta/test/release-evidence
```

After the workflow reaches the default branch, use the ordinary manual form:

```bash
gh workflow run release-evidence.yml --ref beta/messaging-rework
```

The final `beta release evidence complete` job becomes green only when every
release responsibility succeeds. Operator approval is still required before
merging **beta/messaging-rework** into **main** or publishing a release.

## Final comparison record

The representative comparison above fulfills the final Task 3 measurement: it
uses the first post-merge messaging pull request whose diff did not change CI
control files. An earlier workflow-control run at commit `5f96cc4`,
[33288631738](https://github.com/cyclops-team/cyclops/actions/runs/33288631738)
correctly selected every lane because it changed the workflow itself. That
control run completed in 6m44s wall time and 21m13s runner time, but it is proof
of the routing expressions rather than the ordinary path-routed comparison.

No defect class is silently discarded. Performance, soak, repeated-race, full
matrix, and full tmux HEAD evidence move to scheduled or release ownership.
Ordinary pull requests keep the cheapest evidence that can honestly fail for
their changed contract.

The final Messaging Milestone 7 pull-request run
[33329987959](https://github.com/cyclops-team/cyclops/actions/runs/33329987959)
completed in 7m47s wall time and 15m16s runner time. Against the Task 1
baseline, that is 26.8% less wall time and 53.0% fewer runner minutes. The
post-merge scheduled run
[33330374062](https://github.com/cyclops-team/cyclops/actions/runs/33330374062)
then passed the full platform matrix, tmux HEAD, performance, repeated-race,
soak, cleanup, and long-history lanes at exact integration commit `7de5ac8a`.
The [Messaging Beta audit](MESSAGING_BETA_AUDIT.md) records the final product
and release evidence.
