# Recommendation implementation — progress

Recovery run resumed 2026-08-05 (took over from the run that paused on a
full disk). Working from
`.agents/planning/2026-08-03-cyclops-workspace-tui/implementation-prompt.md`.

## G0 baseline

Merge commit **`e2d059d`** on `main` (the bug-fix run's nine commits merged
on the user's decision). Baseline gates at G0: fmt clean, clippy clean,
`cargo test --workspace --no-fail-fast` 768 passed / 1 failed — the one
failure is the contention-sensitive `cyclops-ui` perf test, which passes in
isolation (see Gotchas).

## Disk blocker — resolved

The previous run stopped at ~117 MiB free. Recovery: verified no
cargo/rustc/clippy/rustdoc process and no open files under the inactive,
already-merged worktree's target, then `cargo clean --manifest-path
~/Desktop/Code/cyclops-workspace/Cargo.toml` (dry-run first) reclaimed
13.2 GiB → 10 GiB free. The worktree's untracked
`handoff-visual-polish.md` was preserved (it is historical: that pass was
committed to the PR branch and is part of G0 via the merge).

## Completed and committed

| Task | Commit | What | Tests run |
|---|---|---|---|
| B1 | `5d39e49` | One crossterm (0.29), vt100 + comparison tests deleted | `cargo test -p cyclops-workspace`, fmt, clippy |
| fixes | `9584252` | Alternate-screen hydration order inverted (F38); DECTCEM hidden cursor painted. Field renamed `alternate` → `saved_primary`. `PaneRuntime::snapshot()` added for tests | 154 passed incl. new rig test `hydrating_a_pane_in_the_alternate_screen_restores_what_the_user_sees` |
| A1b | `c541d10` | 20 bridge-fidelity fixtures (`tests/fidelity.rs`); CellAttrs gains `hidden`, `strikeout`, `Underline` enum wired to Ratatui modifiers; `at_tail()` derived from display offset (pulled forward from R1); F39 recorded | 174 passed, fmt, clippy |
| D1 | `dfb089f` | `cyclops-tmux::ops`: typed select/split/close/zoom/resize/create/rename/swap/move on `ControlClient`, exact-match `=` targets, id replies | `cargo test -p cyclops-tmux` (rig-backed ops tests), fmt, clippy |

Findings written this run: **F38** (`capture-pane -a` reads the saved
primary screen, never the alternate screen) and **F39** (alacritty 0.26:
VS16 does not widen; bare SGR 21 is bold-off). F36/F37 were already
allocated — the previous run's code comments citing F36 were renumbered
to F38 before committing.

## Audit notes on the recovered work

The previous run's uncommitted tree was audited change-by-change against
the recommendation and invariants before committing: all of it was sound.
The C1/E1/F1 subagents it left "running" died on ENOSPC without writing
any files (no stray `mod action;` was left in `lib.rs`). D1's output was
complete, not partial, and passed its rig tests unmodified.

## Active delegations (shared worktree, hard file boundaries)

| Task | Boundary | State |
|---|---|---|
| A1a | NEW `crates/cyclops-workspace/tests/baseline.rs` + `implementation/baselines.md` only | running |
| C1 | NEW `src/action.rs` (+ optional `src/input/route.rs`); 1-line `lib.rs` mod add; visibility-only tweaks | running |
| E1 | `crates/cyclops-ui/` only — extract the backend-neutral stream model | running |
| F1 | NEW files under `skills/cyclops/` only | running |

Subagents do not commit; the lead reviews every diff, runs targeted tests,
and commits per task ID. `app.rs`, `render.rs`, and `Cargo.lock` have one
owner (the lead).

## Remaining

R1 (lead, starts after A1a's baseline lands), C2 (after C1+D1), D2 (after
A1a+D1), then W1 → L1 → E2 → U1 → S1 → Q1 → M1 → F2 → Q2 per the
recommendation's dependency graph.

## Gotchas worth keeping

- `cyclops-ui/tests/perf.rs::frame_build_stays_under_16ms_at_10k_entries`
  fails under parallel CPU load (24.8 ms vs 16 ms budget) and passes in
  isolation. Re-run it alone before believing it.
- Two tmux-rig test binaries running concurrently can interfere; rerun a
  failed rig test alone before believing it.
- Do not pipe `cargo test` through `tail` — the pipeline's exit code is
  `tail`'s, which masks failures. Redirect to a file and check `$?`.
- Disk floor: check free space before cargo commands; below 10 GiB do not
  start new builds (policy in `implementation-prompt.md`). Currently at
  the floor — the only remaining reclaimable artifact set is this repo's
  own active `target/` (~6 GiB), which stays.
