# Recommendation implementation — progress

Recovery run resumed 2026-08-05. Working from
`.agents/planning/2026-08-03-cyclops-workspace-tui/implementation-prompt.md`.
G0 baseline: merge `e2d059d`. Disk blocker resolved same day (13.2 GiB
reclaimed from the merged worktree's target via its manifest, dry-run
first; `handoff-visual-polish.md` preserved).

## Completed and committed

| Task | Commit | What |
|---|---|---|
| B1 | `5d39e49` | One crossterm (0.29); vt100 + comparison tests deleted |
| fixes | `9584252` | Alternate-screen hydration order (F38); DECTCEM cursor |
| A1b | `c541d10` | 20 bridge-fidelity fixtures; attrs bridged; F39 |
| D1 | `dfb089f` | Typed structural ops on ControlClient |
| A1a | `dc20c4c` | Recorded perf baselines + harness |
| D1+ | `870efea` | switch/new/kill session ops |
| F1 | `54f9a38` | skills/cyclops/SKILL.md from real captures |
| C1 | `036d60b` | 26-variant Action vocabulary + pure routing |
| R1 | `71d2e35` | Runtime collapsed; grid mirror deleted; measured |
| D2 | `ad07de7` | Adapter snapshot (2 cmds flat) + concurrent hydration; F40/F41 |
| E1 | `7239beb` | Backend-neutral stream model in cyclops-ui |
| C2 | `ab739b0` | One executor owns every action; intent.rs deleted; app.rs 3166→2532 |
| W1 | `f58e0cc` | Live insertion rule; preview == drop by construction |
| L1 | `ceb98da` | Reconcile 2 cmds (~69x at 8 windows); concurrent tab hydration; coalesced decoration |
| E2 | `7cec8e1` | Event panel renders the shared watch stream; parity test |
| U1 | `250c28e` | Attention has one owner; glyph contract + invariant 11 amended; dead-code allowances gone |
| — | `c21acd1` | doc-paths gate exempts the maintainer's notebook.md |
| — | `5ec068d` | Status carries manifest_display_name; workspace manifest scan deleted |
| — | `a9b91a6` | Attention tripwire green again (U1 fallout caught by cross-crate test run) |
| S1 | `61158d2` | render/ split by surface; app.rs sheds non-owned code; moves only, 203 tests before and after |

Also: `cba276b` (tmux snapshot comments as history), `17c632f`/`0f6aa92`
(progress records). Findings this run: F38-F41, all indexed.

## In progress

Q1 (lead-owned). Docs drift fix delegated first: ~29 `cyclops ui`
mentions across 9 pages updated to `cyclops watch` with real re-captured
output, parity-check and doc-paths as its gates. Full workspace gates
(fmt, clippy, full test suite, doc-paths, parity) run after it lands.
No concurrent behavior edits during Q1, per the plan.

## Remaining

M1 (repository migration, one owner, behavior frozen) → F2 (skill
validation against the final tree; three deferred capture blocks need a
live hook-wired vendor CLI) → Q2 (final gates incl. relocated-temp,
shim, installer variants; docs/findings from real output).

## Gotchas worth keeping

- `cyclops-ui/tests/perf.rs` 16ms assertion flakes under load; rerun alone.
- Two tmux-rig test binaries running concurrently can interfere.
- Never pipe `cargo test` through `tail`; redirect to a file.
- Perf numbers during macOS storage-daemon churn read ~20% worse on
  unchanged code; trust quiet-machine runs only (baselines.md).
- Disk floor 10 GiB: target/debug/incremental was 1.5 GiB of dead weight
  (all builds run CARGO_INCREMENTAL=0) and was reclaimed before Q1.
- The attention tripwire (cyclops-proto/tests/one_place.rs) scans file
  text repo-wide: enumerating the blocked states or string-matching state
  words anywhere — tests included — trips it. Ask the owner
  (is_blocked, Display) instead. Targeted crate gates don't run it;
  cross-crate work does.
