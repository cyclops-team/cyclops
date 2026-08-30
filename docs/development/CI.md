# Current CI evidence and baseline

The workflow exposes six stable check names. This page records the measured
baseline and the responsibility of the evidence that runs today.

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
| `test (macos-latest)` | 8m 58s | Required correctness plus platform evidence |
| `installer (ubuntu-latest)` | 2m 49s | Conditional integration, currently run for every change |
| `installer (macos-latest)` | 3m 27s | Conditional integration, currently run for every change |
| `website` | 18s | Conditional integration, currently run for every change |
| `tmux-head` | 6m 19s | Scheduled evidence, currently advisory on every change |

No release-evidence job exists in the current workflow.

## Repeated evidence in the Task 1 baseline

Each operating-system test job runs the complete Rust evidence, then repeats
the same non-daemon, daemon, and doctest commands under a relocated scratch
root. The second execution attempts to prove only that test scratch paths honor
`CYCLOPS_TEST_TMP`.

The advisory `tmux-head` job builds upstream tmux and repeats nearly the same
complete Rust gate a third time on Linux.

Installer and website jobs do not duplicate the Rust correctness suite, but
they currently run for unrelated changes.

## Current lane responsibilities

1. **Required pull-request correctness:** formatting, Clippy, Rust tests,
   documentation paths, and exact-output parity in the two `test` checks.
2. **Conditional integration:** website and installer checks. They currently
   run for every change.
3. **Scheduled evidence:** tmux HEAD. It currently runs on every pull request.
4. **Release evidence:** no explicit release lane is implemented.

## Superseded-run cancellation

Pull-request runs share a workflow-and-PR concurrency key and cancel an older
run when a new commit arrives. Push and manual runs use their unique GitHub run
id, so release or operator-triggered evidence cannot cancel another run.

## Task 2 deterministic replacement evidence

The relocated-root step no longer repeats the complete Rust suite. It now runs
`scripts/test-relocated-scratch.sh`, which carries three distinct proofs:

1. `scratch_override` proves `CYCLOPS_TEST_TMP` selects the root and that empty
   values retain the platform default.
2. `scratch_path_lint` is explicitly syntactic. It rejects direct platform-temp
   calls in any Rust fixture that owns a real `cyclops-testrig` server.
3. `m0_shadow_daemon_end_to_end` runs one real tmux server, in-process daemon,
   Unix socket, and scratch home under the relocated root.

The former duplicate step cost 3m21s on Linux and 3m20s on macOS in the CI
architecture review's evidence run. The Task 2 pull request records the same
step's replacement duration and the resulting job and runner-time delta.

Tmux server names now include the executable pid and an in-process sequence.
One external owner per test executable records only those exact names. The
`interrupted_owner` regression kills a fixture before Rust destructors run,
observes that its server and socket disappear, and proves a neighboring server
survives. Ordinary return, already-gone server, and panic cleanup remain covered
by the existing teardown tests.

### Source checks are not one evidence class

- `scratch_path_lint`, `teardown_has_one_home`, and the workspace guards enforce
  literal prohibited calls or ownership boundaries. They are syntactic
  architecture lints.
- `src/cyclops-proto/tests/one_place.rs` remains a semantic tripwire. Its own
  header documents the shapes it cannot recognize. A green result is not
  runtime proof and does not replace the attention domain tests or review.

No chronology-named regression was deleted or consolidated in Task 2. The
required durable-contract and original-defect census did not establish a safe
replacement for any of them. Task 2 corrected the named backspace regression's
observable boundary: runtime-idle can coexist with a human draft, so the test
now waits for the distinct `composer_clean` projection before expecting the
attempt to reopen. It passed 30 focused local repetitions without an added
sleep.
