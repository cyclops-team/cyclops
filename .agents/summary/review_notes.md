# Review Notes

Consistency and completeness review of the generated documentation set and
of the repo's own docs, plus decisions made during consolidation.

## Consistency check

- **Generated files vs. repo docs.** The repo already maintains
  authoritative prose docs (`docs/ARCHITECTURE.md`, `docs/PROTOCOL.md`,
  `docs/DELIVERY.md`, `docs/HANDOFF.md`, and one page per user question).
  The `.agents/summary/` files summarize and cross-reference them rather
  than restate them, and defer to them wherever they conflict. Two
  mechanical gates keep the repo docs honest — `scripts/check-doc-paths.py`
  (every quoted path must exist; every page must be reachable from a front
  door) and `demos/parity-check.sh` (every quoted output block must match
  what the binaries print) — so the repo docs should be treated as the
  source of truth over this summary when they diverge.
- **Terminology.** These summaries use the repo's own vocabulary (adoption,
  the eye, the gate, tiers, parking, chrome) as defined in
  `docs/HANDOFF.md` and `crates/cyclops-proto`. No new terms were coined.
- **No contradictions found** between the crate-level analysis and the repo
  docs during generation.

## Completeness check — known gaps

- **ADR-001 is not in this repo.** The formal decision record lives in a
  separate `cyclops-arch` design repo. `docs/HANDOFF.md` carries the parts a
  newcomer needs; the summaries repeat those. Anything deeper requires the
  other repo.
- **The delivery pipeline is summarized, not specified, here.**
  `crates/cyclopsd/src/delivery.rs` is ~3400 lines with many timing
  constants; the authoritative spec is `docs/DELIVERY.md` plus
  `DeliveryState::can_transition_to` in
  `crates/cyclops-proto/src/ledger.rs`.
- **Frontend coverage is intentionally shallow.** `frontend/` is a read-only
  branding reference outside the workspace, never modified without an
  explicit admin request; the summary records that boundary and its shape,
  not component-by-component detail.
- **Shell and Python are lightly covered by tooling.** `demos/*.sh`,
  `scripts/install.sh`, and the Python harness have no linter in CI beyond
  `bash -n` (demos) and their own self-tests; documentation for them lives
  mostly in headers and `tests/harness/README.md`.
- **Volatile documents.** `STATUS.md` (backlog, risks, floors) and
  `findings.md` (measured facts) change with every milestone. The summaries
  point at them rather than quoting them.

## Observations that may warrant maintainer attention

- The one-line installer served from usecyclops.dev
  (`frontend/static/install.sh`) deliberately installs the v1 shell
  implementation (pinned to tag `v1-final`), while this tree is the Rust
  rewrite installed via `scripts/install.sh`. `README.md` documents this
  under "Versions"; `frontend/src/lib/config.ts` carries a TODO about
  confirming the deployment before shipping the copy button.
- The frontend has no tests, no ESLint/Prettier; `svelte-check` is the only
  static check. Consistent with its read-only status, but worth knowing
  before touching it.
- No `rustfmt.toml`/`clippy.toml` exist — default formatting and lints, with
  clippy at `-D warnings` in CI. That appears deliberate (no note suggests
  otherwise).

## Consolidation decisions (deviations from the default skill behavior)

1. **README.md was preserved, not regenerated.** The existing README is a
   hand-crafted, complete user front door whose output blocks are captured
   by and asserted against `demos/parity-check.sh` in CI ("never hand-write
   output into a doc"). Regenerating it would break the parity gate and
   discard CI-verified content. The only change made: one row added to its
   docs table linking `AGENTS.md`, which the orphan rule in
   `scripts/check-doc-paths.py` requires for any new root-level page.
2. **CONTRIBUTING.md stays at `docs/CONTRIBUTING.md` and was preserved.**
   The skill default places consolidated files at the repo root, but this
   repo's convention, front-door links, and doc gates are built around
   `docs/`. The existing page already covers the full contributor loop
   (build, the four gates, testrig and scratch rules, demos, CI, house
   rules) and is itself CI-enforced. A duplicate root file would add a page
   the front doors must link and content that would drift.
3. **AGENTS.md was written to be doc-gate-clean**: every inline path
   resolves from the repo root (the checker validates code spans containing
   slashes) and it is linked from `README.md` so the orphan check passes.

## Recommendations

- Re-run this summary generation after each milestone lands, since
  `STATUS.md`, `CHANGELOG.md`, and the crate surfaces move together.
- If `AGENTS.md` grows project conventions over time, keep them in its
  Custom Instructions section so regeneration preserves them.
- Run `python3 scripts/check-doc-paths.py` after editing any root or
  `docs/` markdown file — including `AGENTS.md`.
