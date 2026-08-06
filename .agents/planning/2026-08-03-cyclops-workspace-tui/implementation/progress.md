# Recommendation implementation — complete

Recovery run, resumed and finished 2026-08-05. Every task in the
recommendation's dependency graph (G0 → foundation → core → integration →
S1 → Q1 → M1 → F2 → Q2) is implemented, committed in logical increments,
and verified. Working from
`.agents/planning/2026-08-03-cyclops-workspace-tui/implementation-prompt.md`;
G0 baseline `e2d059d`.

## Final gates (Q2, run on the migrated tree)

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace --no-fail-fast` | 894 passed / 0 failed (768/1 at G0) |
| `scripts/check-doc-paths.py` | 28 pages, every path resolves, every page indexed |
| `tests/e2e/parity-check.sh` | 115/115 — docs and binaries agree |
| `tests/e2e/parity-check.sh --with-installer` | 131/131 |
| Relocated `CYCLOPS_TEST_TMP` full suite | 894 / 0 |
| `scripts/commpact-shim/test_shim.py` | 42/42 |

## Task commits, in order

| Task | Commit |
|---|---|
| B1 dependency cleanup | `5d39e49` |
| Hydration + DECTCEM fixes (F38) | `9584252` |
| A1b fidelity floor (F39) | `c541d10` |
| D1 typed tmux ops | `dfb089f` |
| A1a perf baselines | `dc20c4c` |
| D1 session-lifecycle ops | `870efea` |
| F1 skill draft | `54f9a38` |
| C1 action vocabulary | `036d60b` |
| R1 runtime collapse (measured) | `71d2e35` |
| D2 adapter snapshot + concurrent hydration (F40/F41) | `ad07de7` |
| E1 shared stream model | `7239beb` |
| C2 one executor, intent.rs deleted | `ab739b0` |
| W1 reorder insertion rule | `f58e0cc` |
| L1 latency integration (measured ~69x reconcile) | `ceb98da` |
| E2 event panel on the watch stream | `7cec8e1` |
| U1 attention owner + glyph contract | `250c28e` |
| display-name ownership cleanup | `5ec068d` |
| attention tripwire repair | `a9b91a6` |
| S1 modules own their rules | `61158d2` |
| Q1 gates green (parity manifest fix) | `b408618` |
| M1 repository migration | `b2d5bf7` |
| F2 skill validation | `8d991fe` |

Supporting commits: `cba276b`, `c21acd1`, and the `cyclops watch` docs
accuracy pass. Findings recorded: F38–F41, indexed.

## Product decisions preserved (verified in code and tests)

Glyph-only compact statuses (invariant 11 amended, theme/NO_COLOR
stability tests); always-visible split controls; the event panel renders
the same stream model as `cyclops watch` (parity test); the live
horizontal insertion rule during workspace reordering (preview == drop
by construction); zero polling throughout (every new deadline is
one-shot and event-armed); the attention rule has one owner and a green
tripwire.

## Deliberately not done, with reasons

- The skill's three deferred capture blocks (verified-tier receipt,
  reached wait, genuinely blocked chain) need a live hook-wired vendor
  CLI no isolated fixture can produce; each is marked in place.
- `CursorShape` stays computed-but-unrendered: it is the A1b
  bridge-fidelity contract, an obligation rather than dead code.
- `./manifests` / `./themes` cwd-relative fallbacks left as behavior
  (frozen during M1); the doc-paths gate knows they are not repo paths.

## Gotchas for whoever comes next

- `cyclops-ui/tests/perf.rs` 16ms assertion flakes under CPU load;
  rerun alone before believing it.
- Two tmux-rig test binaries running concurrently can interfere.
- Never pipe `cargo test` through `tail`; redirect to a file.
- Perf numbers during macOS storage-daemon churn read ~20% worse on
  unchanged code (baselines.md quantifies it).
- The attention tripwire scans file text repo-wide, tests included:
  ask the owner (`is_blocked`, `Display`) instead of enumerating states
  or string-matching state words. Targeted crate gates don't run it.
- Build with `CARGO_INCREMENTAL=0`; the incremental cache is dead
  weight here and has eaten gigabytes before.
