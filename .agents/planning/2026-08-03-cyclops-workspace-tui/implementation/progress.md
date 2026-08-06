# Recommendation implementation — progress

Recovery run resumed 2026-08-05 (took over from the run that paused on a
full disk). Working from
`.agents/planning/2026-08-03-cyclops-workspace-tui/implementation-prompt.md`.

## G0 baseline

Merge commit **`e2d059d`** on `main`. Baseline gates at G0: fmt clean,
clippy clean, `cargo test --workspace --no-fail-fast` 768 passed / 1
failed — the one failure is the contention-sensitive `cyclops-ui` perf
test, which passes in isolation (see Gotchas).

## Disk blocker — resolved

The previous run stopped at ~117 MiB free. Verified no cargo/rustc
process and no open files under the inactive, already-merged worktree's
target, then `cargo clean --manifest-path
~/Desktop/Code/cyclops-workspace/Cargo.toml` (dry-run first) reclaimed
13.2 GiB → 10 GiB free. `handoff-visual-polish.md` preserved (historical;
that work is in G0 via the merge).

## Completed and committed

| Task | Commit | What | Tests |
|---|---|---|---|
| B1 | `5d39e49` | One crossterm (0.29); vt100 + comparison tests deleted | workspace crate, fmt, clippy |
| fixes | `9584252` | Alternate-screen hydration order (F38); DECTCEM hidden cursor. `alternate` → `saved_primary` | 154 incl. new rig test |
| A1b | `c541d10` | 20 bridge-fidelity fixtures; `hidden`/`strikeout`/`Underline` bridged; `at_tail` derived; F39 | 174 |
| D1 | `dfb089f` | `cyclops-tmux::ops`: typed select/split/close/zoom/resize/create/rename/swap/move | tmux crate rig tests |
| A1a | `dc20c4c` | Recorded baselines: hydration, W+3 reconcile fan-out, feed/grid throughput, resize cost (`tests/baseline.rs` + `baselines.md`) | 4 recording tests |
| D1+ | `870efea` | switch_to_session / new_session / kill_session — the last adapter gaps blocking intent.rs deletion | ops tests (21) |
| F1 | `54f9a38` | `skills/cyclops/SKILL.md` from real captured runs; 3 blocks deferred to F2 | doc links verified |
| C1 | `036d60b` | 26-variant stable-target `Action` vocabulary + pure device routing, 39 tests. Temporary module dead_code allowance until C2 | 217 |
| R1 | `71d2e35` | PaneRuntime/AlacrittyVt collapsed; grid mirror deleted; direct engine-cell rendering with selection folded into the one pass; word-select CJK column fix. Measured: frame walk 150.75us vs 165.79us mirror build + paint walk (baselines.md post-R1 section) | 218 |
| D2 | `ad07de7` | Adapter workspace_snapshot (2 commands flat vs W+3; 0.36ms vs 39.61ms at 8 windows); hydrate_panes concurrent (0.5-0.7ms vs 1.4-1.6ms at 8 panes); commands_issued counter; F40/F41 | tmux crate (101) |
| E1 | `7239beb` | Backend-neutral stream model (`cyclops-ui::stream::Record`/`Intake`/`Entry`); watch is renderer #1; transcript seam test for E2 parity | ui 81 + cli 189 |

Findings written this run: F38, F39, F40, F41 (indexed).

## Active delegation

C2 (action-executor integration; sole owner of `crates/cyclops-workspace`,
deletes intent.rs and the duplicate device execution branches, moves
naming policy to a workspace module, removes C1's temporary allowance).

## Remaining

W1 (after C2) → L1 (integrates D2's snapshot + concurrent hydration into
sync.rs; coalesced decoration refreshes) → E2 (event panel on the shared
stream model) → U1 (glyph-only compact statuses; delete the
`state.is_blocked()` attention fallback) → S1 → Q1 → M1 → F2 → Q2.
Serialized: they converge on app.rs/render.rs.

Also queued for pre-Q1: docs still name `cyclops ui` as current in
README/HANDOFF/troubleshooting/STATUS though the binary ships it only as
a deprecated alias for `cyclops watch` (found by F1; parity transcripts
must be regenerated with the fix).

## Gotchas worth keeping

- `cyclops-ui/tests/perf.rs::frame_build_stays_under_16ms_at_10k_entries`
  flakes under parallel CPU load; passes alone.
- Two tmux-rig test binaries running concurrently can interfere; rerun a
  failed rig test alone before believing it.
- Do not pipe `cargo test` through `tail`; redirect to a file.
- Perf numbers taken during macOS storage-daemon churn (after mass file
  deletion) read ~20% worse across the board, including on unchanged
  code. Trust only quiet-machine runs (see baselines.md).
- Disk floor: below 10 GiB free, no new builds. Currently ~10 GiB; the
  only remaining reclaimable artifact set is the active repo's own
  target/, which stays.
