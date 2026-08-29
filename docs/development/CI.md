# CI evidence lanes

The workflow keeps six stable check names while the beta CI rework changes
which evidence runs for each change. A required check may later perform a fast
not-applicable pass, but it does not disappear.

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
| `test (macos-latest)` | 8m 58s | Required correctness plus platform evidence during Task 1 |
| `installer (ubuntu-latest)` | 2m 49s | Conditional integration, currently run for every change |
| `installer (macos-latest)` | 3m 27s | Conditional integration, currently run for every change |
| `website` | 18s | Conditional integration, currently run for every change |
| `tmux-head` | 6m 19s | Scheduled evidence, currently advisory on every change |

No release-evidence job exists yet. Task 3 adds an explicit release entrypoint
before moving broad evidence out of the ordinary pull-request path.

## Repeated evidence in the baseline

Each operating-system test job runs the complete Rust evidence, then repeats
the same non-daemon, daemon, and doctest commands under a relocated scratch
root. The second execution attempts to prove only that test scratch paths honor
`CYCLOPS_TEST_TMP`; Task 2 must replace it with focused path evidence before
removing the duplicate run.

The advisory `tmux-head` job builds upstream tmux and repeats nearly the same
complete Rust gate a third time on Linux. Task 3 moves that compatibility risk
to scheduled and path-relevant evidence only after the replacement lane works.

Installer and website jobs do not duplicate the Rust correctness suite, but
they run for unrelated changes. Task 3 makes them path-aware without changing
their stable check names.

## Current lane responsibilities

1. **Required pull-request correctness:** formatting, Clippy, Rust tests,
   documentation paths, and exact-output parity in the two `test` checks.
2. **Conditional integration:** website and installer checks. They remain
   unconditional during Task 1 so this PR changes no coverage.
3. **Scheduled evidence:** tmux HEAD. It remains on every pull request during
   Task 1 until Task 3 supplies the schedule and manual entrypoint.
4. **Release evidence:** not yet implemented. Task 3 must provide the full
   clean-checkout and release-oriented entrypoint before ordinary CI sheds any
   of that evidence.

## Superseded-run cancellation

Pull-request runs share a workflow-and-PR concurrency key and cancel an older
run when a new commit arrives. Push and manual runs use their unique GitHub run
id, so release or operator-triggered evidence cannot cancel another run.
