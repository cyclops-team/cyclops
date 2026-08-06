# Post-implementation review findings

Reviewed 2026-08-05 against:

- `../recommendation.md`
- `../implementation-prompt.md`
- implementation range `e2d059d..fc1a810`

## Verdict

The recommendation is not yet complete. The broad architecture, repository
layout, and required gates are in good condition, but the implementation has
two high-severity fidelity/parity defects, two medium-severity UX/migration
defects, and two incomplete validation tasks.

Do not treat a green test suite as resolving the findings below. Several of
the current tests exercise an intermediate representation or synthetic input
instead of the user-visible integration they claim to verify.

## Required findings

### 1. High: terminal rendering loses visual information

The Alacritty-to-Ratatui bridge is still lossy:

- `src/cyclops-workspace/src/runtime/grid.rs:59` stores one `char` in
  `GridCell` rather than the complete cell grapheme.
- `src/cyclops-workspace/src/runtime/pane.rs:257` copies only `cell.c` from
  Alacritty.
- `src/cyclops-workspace/src/render/canvas.rs:546` paints that value with
  `set_char`, so Alacritty's zero-width combining characters never reach the
  Ratatui buffer.
- `src/cyclops-workspace/tests/fidelity.rs:80` checks only that the following
  character remains in column 1. It does not assert that the combining mark is
  visible, so it passes while `e\u{301}` is rendered as plain `e`.
- `src/cyclops-workspace/src/render/mod.rs:155` flattens every underline
  variant to generic `UNDERLINED`.
- Cursor shape is parsed in `src/cyclops-workspace/src/runtime/pane.rs:219`,
  but the render path retains only cursor position at
  `src/cyclops-workspace/src/render/canvas.rs:272` and
  `src/cyclops-workspace/src/app.rs:2163`.

Required outcome:

- Preserve complete visible graphemes through the final Ratatui buffer.
- Either render supported underline and cursor distinctions faithfully or
  explicitly narrow the contract, delete unused distinctions, and document
  the limitation. Prefer faithful rendering where the current stack supports
  it.
- Add end-to-end render-buffer assertions. Tests that inspect only
  `GridCell`, `CellAttrs`, or `CursorState` do not prove bridge fidelity.
- Cover combining marks, emoji sequences, underline variants, cursor
  visibility, and cursor shape at the final rendering boundary.

### 2. High: the workspace event panel does not receive the `cyclops watch` stream

The workspace uses the shared row model but not the same complete stream:

- `src/cyclops-workspace/src/app.rs:616` explicitly omits the ledger-tail
  backfill required by `recommendation.md:395`.
- The live subscription starts at `src/cyclops-workspace/src/app.rs:314`, while
  the status seed is applied later at line 440.
- The workspace does not use `cyclops_ui::Intake`, whose contract in
  `src/cyclops-ui/src/stream.rs:626` orders backfill, the status seed, and live
  events and prevents startup regressions.
- `src/cyclops-workspace/tests/event_stream_parity.rs:65` manually inserts a
  synthetic replayed entry while acknowledging that the real workspace has no
  ledger tail. The test therefore proves shared formatting, not equivalent IO,
  ordering, or records.

Required outcome:

- Feed the workspace record through the same backfill, seed, and live ordering
  contract as `cyclops watch`, including deduplication semantics.
- Reuse the existing workspace subscription where practical; do not introduce
  polling or a competing long-lived state view.
- Replace or extend the parity test so it exercises the real workspace intake
  seam. Given one backfill-plus-live transcript, both consumers must produce
  identical plain rows in identical order.
- Add a regression test in which a live transition arrives during startup and
  cannot be overwritten by an older status snapshot.

### 3. Medium: multi-line event entries can hide the newest event

`src/cyclops-workspace/src/render/event_panel.rs:103` takes the last `height`
entries and only then expands each entry into lines and enables wrapping. When
those entries occupy more than one line, `Paragraph` clips from the start of
the resulting buffer. An older multi-line entry can consume the viewport while
the newest entry is below the clipped region.

Required outcome:

- Compute the visible tail in rendered lines, including explicit line breaks
  and width-dependent wrapping, or use a bottom-aligned/scrolling model that
  guarantees the newest event remains visible.
- Add tests with multi-line and narrow-width entries where the newest event
  must appear in the final buffer.

### 4. Medium: the repository migration left broken and stale paths

At least two runnable demos still refer to directories removed by M1:

- `demos/m5-theme.sh:68` copies from `$REPO/themes/*.toml`; the files now live
  under `resources/themes/`, so the demo exits at this command.
- `demos/m0-status.sh:61` configures `$REPO/manifests`; the shipped manifests
  now live under `resources/manifests/`.

Stale path descriptions also remain in active or onboarding material,
including:

- `STATUS.md:40` and `STATUS.md:85`
- `docs/development/ARCHITECTURE.md:444`
- `docs/guides/send.md:68`
- `docs/guides/install.md:171`
- `docs/reference/PROTOCOL.md:427`
- `.agents/summary/codebase_info.md:79`
- `.agents/summary/components.md:176`
- `.agents/summary/review_notes.md:42`
- `website/src/lib/config.ts:22`

The documentation-path gate does not catch all of these because several paths
are plain prose rather than Markdown links or eligible code spans.

Required outcome:

- Update every moved repository path, including runnable demos, active docs,
  summaries, comments, and website metadata.
- Run the affected demos rather than relying only on `bash -n`.
- Keep `website/` otherwise read-only as required by `AGENTS.md` and the
  implementation prompt. Correcting stale migration metadata is the only
  website work in scope.
- Consider extending the path audit only if it can detect these cases without
  excessive false positives; fixing the references is mandatory either way.

## Incomplete validation work

### 5. The performance contract was only partially measured

`src/cyclops-workspace/tests/baseline.rs` contains hydration, reconciliation,
runtime feed/grid traversal, and resize measurements. It does not fulfill the
complete harness requested at `recommendation.md:639-660`.

Missing evidence includes:

- key-to-control-write p95;
- visible-output frame gaps and starvation behavior;
- queue depth or bounded-memory behavior under sustained output;
- full render duration rather than only cell traversal;
- flow-control pause/resume;
- daemon event bursts;
- input progress during sustained pane output;
- terminal restoration across recoverable exits.

Required outcome:

- Add deterministic measurements for the missing scenarios using
  `cyclops_testrig::TmuxServer` and scratch paths from the repository helpers.
- Exercise one, four, and eight panes and mixed ASCII/wide output where the
  recommendation calls for them.
- Record measured results in the implementation baselines and `findings.md`.
- Do not claim latency or boundedness that the harness does not measure.

### 6. F2 still has three deferred real-output captures

`skills/cyclops/SKILL.md` explicitly defers:

- a verified-tier delivery receipt near line 126;
- a wait that reaches `done` near line 234;
- a genuinely blocked delivery chain near line 286.

The deferral is acknowledged again near line 343 and in
`implementation/progress.md:63`. Therefore F2 is not complete as written.

Required outcome:

- Capture and sanitize these examples from a real hook-wired agent session,
  then replace the placeholders.
- If the environment truly cannot produce them, report F2 as externally
  blocked rather than complete. Do not silently weaken the completion
  condition.

## Verified work that should be preserved

The review found the following work substantially implemented and worth
preserving:

- the intended `src/`, `tests/`, `docs/`, `website/`, `resources/`, `demos/`,
  and `skills/cyclops/` organization;
- one Crossterm version and removal of `vt100`;
- target-bearing action vocabulary and consolidated action execution;
- typed tmux mutations and adapter-owned snapshots;
- stable-ID workspace reordering with a visible insertion rule;
- concurrent hydration, batched reconciliation, and coalesced decoration
  refreshes;
- glyph-only compact statuses and always-visible pane split controls;
- shared semantic event-row formatting;
- attention-rule ownership and the no-polling invariant.

## Independent gate results

The post-implementation review reran these gates from a shell outside tmux:

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | passed |
| `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `CARGO_INCREMENTAL=0 cargo test --workspace --no-fail-fast` | 894 passed, 0 failed |
| `python3 scripts/check-doc-paths.py --selftest` | passed |
| `python3 scripts/check-doc-paths.py` | passed, 28 pages |
| `python3 scripts/commpact-shim/test_shim.py` | 42/42 |
| `./tests/e2e/parity-check.sh` | 115/115 |
| `./tests/e2e/parity-check.sh --with-installer` | 131/131 |
| Full suite with relocated `CYCLOPS_TEST_TMP` | 894 passed, 0 failed |

These green gates establish a stable starting point. They do not override the
integration gaps above.

## Completion criteria for the follow-up

The follow-up is complete only when:

1. All six findings are fixed or, for genuinely external F2 captures, clearly
   reported as blocked rather than complete.
2. New regression tests fail against `fc1a810` and pass with the fixes.
3. The full repository gates pass, including relocated-temp, installer parity,
   and shim tests.
4. The affected demos run successfully on isolated tmux servers.
5. `implementation/progress.md` is corrected to reflect the actual result and
   only claims full completion once these conditions are satisfied.

