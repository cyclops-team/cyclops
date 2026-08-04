# AGENTS.md

Orientation for AI coding agents working on this repo. The human-written map
is [docs/HANDOFF.md](docs/HANDOFF.md) — read it before any non-trivial
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
| `crates/cyclops-proto` | Wire + ledger types, delivery state machine, attention rule | Shared *rules* live here once; no IO, no tmux, no rendering |
| `crates/cyclops-tmux` | The tmux adapter | **Every tmux invocation in the product is in this crate** (one exception: the daemon's boot-time `tmux -V`) |
| `crates/cyclops-manifest` | Detection-manifest schema and evaluation | Vendor CLI behavior is TOML data in `manifests/`, never Rust |
| `crates/cyclops-ledger` | Append-only NDJSON writer/reader | Never rewritten; corrections are new lines |
| `crates/cyclops-theme` | Semantic color tokens | Renderers use tokens, never raw colors |
| `crates/cyclopsd` | The daemon: fusion, delivery, socket, identity | Library + thin binary so tests boot it in-process |
| `crates/cyclops` | The CLI | Thin client; business rules stay in proto/daemon. User-facing sentences live in `crates/cyclops/src/copy.rs` |
| `crates/cyclops-ui` | The stream TUI | Its `grid` module is the one rendering vocabulary; the CLI shares it |
| `crates/cyclops-testrig` | Test-only isolated tmux server | The only way tests may touch tmux |
| `manifests/`, `themes/`, `layouts/`, `hooks/` | Data, not code paths | `manifests/` and `layouts/` are compiled into the CLI with include_str and seeded to the user home on first run |
| `demos/` | Runnable end-to-end scripts on throwaway tmux servers | `demos/parity-check.sh` is a CI gate, not a demo |
| `frontend/` | SvelteKit landing page for usecyclops.dev | Outside the Cargo workspace, ignored by CI, **read-only** — never modify without an explicit request |
| `findings.md` | Measured facts (F13+), each with its probe | Docs and comments cite these F-numbers |

## The gates a change must pass

Same four CI runs, in order (full detail: [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md)):

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
python3 scripts/check-doc-paths.py
./demos/parity-check.sh
```

- `--no-fail-fast` is not optional: cargo stops at the first failing test
  *binary* and hides everything after it (F24).
- Touching `scripts/install.sh` adds `./demos/parity-check.sh --with-installer`.
- CI also reruns the whole suite with `CYCLOPS_TEST_TMP` relocated, runs the
  v1 shim tests (`scripts/commpact-shim/test_shim.py`), and has an advisory
  job against tmux built from master.

## Rules that are unusual for this repo

1. **Docs are CI-verified against the binaries.** Never hand-write output
   into a doc: `demos/parity-check.sh` re-runs every command shape the
   README and `docs/` quote and fails when a line drifts. If you change
   output a doc quotes, copy the new output from the parity transcript into
   the page in the same commit.
2. **Every path a doc quotes is checked**, including in this file:
   `scripts/check-doc-paths.py` fails CI when a markdown link or a
   slash-containing code span in any root or `docs/` page names a file that
   does not exist, and when a page is not reachable from `README.md` or
   `docs/HANDOFF.md`.
3. **Tests never touch the user's tmux server or real home.** Use
   `cyclops_testrig::TmuxServer` (Drop is the teardown) and point
   `CYCLOPS_HOME` at scratch. Scratch paths come from
   `cyclops_proto::scratch::scratch_dir`, never `/tmp` literals or
   `std::env::temp_dir()` (F24). Guard tests enforce both; shell goes
   through `demos/lib.sh`.
4. **No polling.** If you are reaching for an interval timer, you have not
   found the event yet (`docs/INVARIANTS.md`, rule 9).
5. **Wire changes are additive.** New fields optional; unknown fields
   ignored in both directions; version mismatch warns, never rejects.
6. **The pane title is a sensor — never write it.** Adoption decoration goes
   on the pane border only.
7. **A behavior fix needs a test that fails before it**, and docs ship in
   the same commit as the behavior.
8. **[docs/STYLE.md](docs/STYLE.md) is binding** on code, comments, and
   docs. Write for a tired engineer who has never seen this repo.
9. **Record what you measured.** A learned fact about tmux, a vendor CLI, or
   a platform goes in `findings.md` with the probe that proved it.
10. **Before touching delivery, the ledger, or anything that renders**, read
    [docs/INVARIANTS.md](docs/INVARIANTS.md). The delivery spec is
    [docs/DELIVERY.md](docs/DELIVERY.md); the legal transitions are
    `DeliveryState::can_transition_to` in
    `crates/cyclops-proto/src/ledger.rs`.

## Fast navigation

- How a message becomes a verified receipt: [docs/DELIVERY.md](docs/DELIVERY.md),
  then `crates/cyclopsd/src/delivery.rs` in call order
  (`msg_send` → `worker_loop` → `process` → `gate` → `attempt_delivery`).
- What state a pane is in and why: `crates/cyclopsd/src/fusion.rs`;
  per-sensor readings via `cyclops read <agent> --source detection`.
- "What needs a human" (the eye): one owner,
  `crates/cyclops-proto/src/attention.rs` — never recompute it elsewhere.
- Debugging a stuck delivery: the ledger is the debugger; every gate
  decision is a line with a cause. See the cause table in
  [docs/HANDOFF.md](docs/HANDOFF.md).
- Adding an agent CLI: one TOML file, no code —
  [docs/MANIFESTS.md](docs/MANIFESTS.md), fixtures in
  `crates/cyclops-manifest/tests/fixtures/`.
- What is built vs. planned: [STATUS.md](STATUS.md). Two non-bugs to know:
  a quota park has no re-queue verb (by design), and `cyclops start` cannot
  tell two same-shaped layouts apart until one pane is named.

## Custom Instructions

<!-- This section is maintained by developers and agents during day-to-day work.
     It is NOT auto-generated by codebase-summary and MUST be preserved during refreshes.
     Add project-specific conventions, gotchas, and workflow requirements here. -->
