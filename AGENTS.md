# AGENTS.md

Orientation for AI coding agents working on this repo. The human-written map
is [docs/development/HANDOFF.md](docs/development/HANDOFF.md). Read it before any non-trivial
change; this page is the condensed agent view plus the gates that are easy
to trip.

Cyclops coordinates terminal coding agents already running in tmux: a Rust
daemon (`cyclopsd`) watches panes, fuses sensors into an agent state,
delivers messages between agents with verified receipts, and appends every
fact to an NDJSON ledger. A generated knowledge base with diagrams lives at
`.agents/summary/index.md`.

## Where things live

| Path | What | The rule that matters |
|---|---|---|
| `src/cyclops-proto` | Wire + ledger types, delivery state machine, attention rule | Shared *rules* live here once; no IO, no tmux, no rendering |
| `src/cyclops-tmux` | The tmux adapter | **Every tmux invocation in the product is in this crate** (one exception: the daemon's boot-time `tmux -V`) |
| `src/cyclops-manifest` | Detection-manifest schema and evaluation | Vendor CLI behavior is TOML data in `resources/manifests/`, never Rust |
| `src/cyclops-ledger` | Append-only NDJSON writer/reader | Never rewritten; corrections are new lines |
| `src/cyclops-theme` | Semantic color tokens | Renderers use tokens, never raw colors |
| `src/cyclopsd` | The daemon: fusion, delivery, socket, identity | Library + thin binary so tests boot it in-process |
| `src/cyclops` | The CLI | Thin client; business rules stay in proto/daemon. User-facing sentences live in `src/cyclops/src/copy.rs` |
| `src/cyclops-ui` | The stream TUI (`cyclops watch`) | Its `grid` module is the CLI/stream rendering vocabulary |
| `src/cyclops-workspace` | The full-screen workspace (`cyclops`) | Ratatui/Crossterm chrome; pane VT runtimes; shares `cyclops-theme` tokens |
| `tests/testrig` | Test-only isolated tmux server | The only way tests may touch tmux |
| `resources/manifests/`, `resources/themes/`, `resources/layouts/`, `resources/hooks/` | Data, not code paths | `resources/manifests/` and `resources/layouts/` are compiled into the CLI with include_str and seeded to the user home on first run |
| `demos/` | Runnable end-to-end scripts on throwaway tmux servers | `tests/e2e/parity-check.sh` is a CI gate, not a demo |
| `website/` | SvelteKit landing page for usecyclops.dev | Outside the Cargo workspace; modify only on explicit request. CI checks it and requires its installer to match `scripts/install.sh` |
| `findings.md` | Measured facts (F13+), each with its probe | Docs and comments cite these F-numbers |

## The gates a change must pass

The complete local gate is documented in [CONTRIBUTING.md](CONTRIBUTING.md).
`./scripts/check.sh` runs it cheapest first; `--fast` stops after Rust
correctness and documentation compilation. Performance executables run in the
scheduled and release lanes, not as ordinary correctness tests.

```bash
./tests/e2e/messaging-docs-parity.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace -E 'not (package(cyclopsd) | binary_id(=cyclops-ui::perf) | binary_id(=cyclops-ui::queue_perf) | binary_id(=cyclops-workspace::perf_contract))' --no-fail-fast
cargo test -p cyclopsd --all-targets --no-fail-fast
cargo doc --workspace --no-deps
python3 scripts/check-doc-paths.py
./tests/e2e/parity-check.sh
```

- `--no-fail-fast` is not optional: nextest must keep scheduling so one run
  reports every failing test instead of hiding the remaining failures.
- Touching either installer requires keeping `scripts/install.sh` and
  `website/static/install.sh` byte-for-byte identical, then running
  `./tests/e2e/parity-check.sh --with-installer`.
- CI runs focused root-selection, source-boundary, and tmux/daemon/socket
  evidence with `CYCLOPS_TEST_TMP` relocated. Website, installer, tmux HEAD,
  macOS, and other platform evidence run on pull requests only when their owned
  inputs change. Full matrix, tmux HEAD, reliability, performance, and release
  evidence have explicit workflows described in
  [CI.md](docs/development/CI.md).

## Rules that are unusual for this repo

1. **Docs are CI-verified against the binaries.** Never hand-write output
   into a doc: `tests/e2e/parity-check.sh` re-runs every command shape the
   README and `docs/` quote and fails when a line drifts. If you change
   output a doc quotes, copy the new output from the parity transcript into
   the page in the same commit.
2. **Every path a doc quotes is checked**, including in this file:
   `scripts/check-doc-paths.py` fails CI when a markdown link or a
   slash-containing code span in any root or `docs/` page names a file that
   does not exist, and when a page is not reachable from `README.md` or
   `docs/development/HANDOFF.md`.
3. **Tests never touch the user's tmux server or real home.** Use
   `cyclops_testrig::TmuxServer` (Drop is the teardown) and point
   `CYCLOPS_HOME` at scratch. Scratch paths come from
   `cyclops_proto::scratch::scratch_dir`, never `/tmp` literals or
   `std::env::temp_dir()` (F24). Guard tests enforce both; shell goes
   through `tests/e2e/lib/lib.sh`.
4. **No polling.** If you are reaching for an interval timer, you have not
   found the event yet (`docs/development/INVARIANTS.md`, rule 9).
5. **Wire changes are additive.** New fields optional; unknown fields
   ignored in both directions; version mismatch warns, never rejects.
6. **The pane title is a sensor. Never write it.** Adoption decoration goes
   on the pane border only.
7. **A behavior fix needs a test that fails before it**, and docs ship in
   the same commit as the behavior.
8. **[docs/development/STYLE.md](docs/development/STYLE.md) is binding** on code, comments, and
   docs. Write for a tired engineer who has never seen this repo.
9. **Record what you measured.** A learned fact about tmux, a vendor CLI, or
   a platform goes in `findings.md` with the probe that proved it.
10. **Before touching delivery, the ledger, or anything that renders**, read
    [docs/development/INVARIANTS.md](docs/development/INVARIANTS.md). The delivery spec is
    [docs/development/DELIVERY.md](docs/development/DELIVERY.md); the legal transitions are
    `DeliveryState::can_transition_to` in
    `src/cyclops-proto/src/ledger.rs`.

## Fast navigation

- How a message becomes a verified receipt: [docs/development/DELIVERY.md](docs/development/DELIVERY.md),
  then `src/cyclopsd/src/delivery.rs` in call order
  (`msg_send` → `worker_loop` → `process` → `gate` → `attempt_delivery`).
- What state a pane is in and why: `src/cyclopsd/src/fusion.rs`;
  per-sensor readings via `cyclops read <agent> --source detection`.
- "What needs a human" (the eye): one owner,
  `src/cyclops-proto/src/attention.rs`. Never recompute it elsewhere.
- Debugging a stuck delivery: the ledger is the debugger; every gate
  decision is a line with a cause. See the cause table in
  [docs/development/HANDOFF.md](docs/development/HANDOFF.md).
- Adding an agent CLI: one TOML file, no code.
  [docs/reference/MANIFESTS.md](docs/reference/MANIFESTS.md), fixtures in
  `src/cyclops-manifest/tests/fixtures/`.
- What is built vs. planned: [STATUS.md](STATUS.md). Two non-bugs to know:
  a legacy direct-delivery quota park has no requeue verb, while a mailbox
  quota hold requires observed reset plus explicit admin requeue; and
  `cyclops start` cannot tell two same-shaped layouts apart until one pane is
  named.

## Custom Instructions

<!-- This section is maintained by developers and agents during day-to-day work.
     It is NOT auto-generated by codebase-summary and MUST be preserved during refreshes.
     Add project-specific conventions, gotchas, and workflow requirements here. -->

## Cursor Cloud specific instructions

Cloud VMs start with Rust 1.83 pinned as `rustup` default; this tree's
`Cargo.lock` needs 1.85+ (`edition2024`). The session update script runs
`rustup update stable` and `rustup default stable`. If a build fails with
"feature `edition2024` is required", re-run `rustup default stable`.

Standard lint/test/build/run commands are in this file and
[CONTRIBUTING.md](CONTRIBUTING.md). Follow those. Non-obvious
caveats for this environment:

- **Run the Rust tests (nextest and `cargo test --doc`) from a plain shell, not inside tmux.** The e2e tests
  inherit the caller's environment; with `$TMUX` / `$TMUX_PANE` set,
  `src/cyclops/tests/e2e.rs`'s
  `self_names_the_calling_pane_and_says_so_when_there_is_none` fails
  because it asserts the outside-tmux path. Outside tmux it passes.
- **Two `cyclops-theme` tests are timing-sensitive on this VM.**
  `the_config_key_switches_a_running_watch` and
  `a_selection_change_may_set_fewer_tokens` in
  `src/cyclops-theme/src/select.rs` rewrite a file to the same length
  and rely on mtime advancing between writes microseconds apart. This
  kernel's per-tick inode timestamps are coarse (confirmed on overlayfs
  and tmpfs), so the theme watcher can miss the change. Not a code
  defect. Do not "fix" it. Everything else in the suite passes.
- For a self-contained end-to-end delivery demo that provisions its own
  detectable pane, run `./demos/m1-send.sh` (isolated tmux server +
  throwaway home). A live `cyclops start` pane reads `? unknown` unless
  a shipped manifest matches the program in it.
- `website/` serves on port 5173 with `npm run dev`. Modify it only on an
  explicit request; when you do, run `npm run check` and `npm run build`.
