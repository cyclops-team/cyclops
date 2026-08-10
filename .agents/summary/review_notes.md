# Review Notes

Consistency and completeness review for the **2026-08-09** knowledge-base
refresh under `.agents/summary/`. Parameters for this run:

| Parameter | Value |
|---|---|
| `codebase_path` | `/Users/briosolivares/Desktop/Code/cyclops` |
| `output_dir` | `.agents/summary` |
| `consolidate` | **false** |
| `check_consistency` | true |
| `check_completeness` | true |

Because `consolidate` is false, `consolidate_targets` /
`consolidate_prompt` are ignored. **No consolidated root files were
written or modified** — `AGENTS.md`, `README.md`, and `CONTRIBUTING.md`
were left untouched.

## What this refresh completed

A prior pass had already rewritten (2026-08-09 ~17:17–17:19):

- `index.md`, `architecture.md`, `codebase_info.md`, `components.md`,
  `interfaces.md`, `data_models.md`, `workflows.md`

This finishing pass:

1. Fully refreshed the two remaining stale files (`dependencies.md`,
   `review_notes.md` — previously dated ~2026-08-06).
2. Spot-checked the already-updated siblings against the tree
   (`Cargo.toml` members, `resources/themes/` count, wire method list,
   theme token count, `cyclops watch` vs deprecated `cyclops ui`, `src/`
   layout).
3. Applied one small accuracy patch in `data_models.md` (delivery-machine
   diagram edges that `can_transition_to` allows but the diagram omitted).

## Consistency check

Checked across the summary set and against authoritative sources
(`docs/development/HANDOFF.md`, `STATUS.md`, root/`src/*/Cargo.toml`,
`AGENTS.md`, `CONTRIBUTING.md`).

| Claim | Verdict |
|---|---|
| Crates live under `src/` (not `crates/`) | Consistent across the set |
| Stream TUI is `cyclops watch`; `cyclops ui` is deprecated alias only | Consistent (`architecture.md`, `interfaces.md`, `workflows.md`, `STATUS.md`) |
| Seventeen themes + 42 semantic tokens | Consistent with `resources/themes/` (17 TOMLs) and `cyclops-theme` `ALL: [&str; 42]` |
| Four manifests / four layout presets / seven demos | Consistent with `resources/` and `demos/` |
| Protocol v1 = 17 methods | Matches `PROTOCOL_V1` in `src/cyclopsd/src/server.rs` |
| Delivery machine = 10 states; legal moves in proto | Matches `DeliveryState` / `can_transition_to` |
| Zero polling; no file-watcher / interval-poller crates | Matches architecture decision and the refreshed `dependencies.md` "deliberately absent" list |
| Workspace = Ratatui/Crossterm/Alacritty; stream UI hand-rolls termios | Consistent across architecture / components / dependencies |
| Ten workspace crates (9 product + testrig) | Matches root `Cargo.toml` `members` |
| Terminology (adoption, eye, gate, tiers, parking, chrome) | Uses repo vocabulary from HANDOFF / proto; no coined jargon |

**Corrections made during this finishing pass**

- `data_models.md`: delivery state diagram now includes `Queued` →
  `AttentionRequired` / `ParkedBlockedQuota`, and `Gating` →
  `RetryQueued`, which `can_transition_to` allows but the prior diagram
  omitted. Prose about terminal parks and the future operator-clear path
  was already accurate.

No other contradictions were found between the Aug-9 sibling summaries and
the current tree. Where summaries and hand-written docs could diverge in
the future, **repo docs win**: they are CI-enforced by
`scripts/check-doc-paths.py` and `tests/e2e/parity-check.sh`.

## Completeness check — known gaps

These are intentional or structural limits of the summary set, not bugs
in the refresh:

- **ADR-001 is not in this repo.** The formal decision record lives in a
  separate `cyclops-arch` design repo. `docs/development/HANDOFF.md`
  carries what a newcomer needs; summaries repeat that level only.
- **Delivery is summarized, not specified.** The authoritative path is
  `docs/development/DELIVERY.md` plus
  `DeliveryState::can_transition_to` in
  `src/cyclops-proto/src/ledger.rs`. `src/cyclopsd/src/delivery.rs` is
  large and timing-heavy; do not treat the summary sequence diagram as
  exhaustive of every retry / hold branch.
- **Website coverage is intentionally shallow.** `website/` is a separate
  SvelteKit build outside Cargo. Its CI contract is type-check, build, and
  exact installer parity with `scripts/install.sh`.
- **Shell / Python tooling is lightly covered.** Demos, the installer, and
  the e2e harness live mostly in headers and `tests/e2e/lib/README.md`;
  CI lint for demos is primarily `bash -n`.
- **Volatile documents are pointed at, not restated.** `STATUS.md`
  (backlog, risks, floors) and `findings.md` (F-numbers with probes)
  change with milestones. Summaries link them rather than quote counts
  that would stale immediately (parity-check count, test count, etc.).
- **Per-vendor detection quirks** (e.g. Codex idle↔working / Cursor
  typing-as-working) are mentioned in `workflows.md` and tracked in
  `STATUS.md` / GitHub — not expanded into a per-CLI matrix here.
- **No language-support gaps from the skill's multi-language analyzer:**
  the product is Rust + a thin TypeScript site + shell/Python tooling;
  all of those surfaces are named in `codebase_info.md` /
  `dependencies.md`.

## Consolidation status

**Skipped** (`consolidate: false`).

- `AGENTS.md` — untouched (including its human-maintained
  `Custom Instructions` block).
- `README.md` — untouched (CI-verified user front door; output blocks
  asserted by parity-check).
- `CONTRIBUTING.md` — untouched (gates and testrig rules remain the
  contributor source of truth).

A future run with `consolidate: true` must preserve `AGENTS.md`'s
Custom Instructions verbatim and must not regenerate README/CONTRIBUTING
output that would break the parity gate.

## Observations for maintainers (unchanged facts)

- Hosted installer `website/static/install.sh` must remain identical to
  `scripts/install.sh`; both parity and website CI enforce this.
- No `rustfmt.toml` / `clippy.toml` / `deny.toml` — defaults, with clippy
  at `-D warnings` in CI.
- Website has no browser test suite or ESLint/Prettier config; CI runs
  `svelte-check` and the production build.

## Recommendations

- Re-run this summary generation after each milestone lands —
  `STATUS.md`, `CHANGELOG.md`, crate surfaces, theme/manifest counts, and
  wire methods move together.
- Keep using `index.md` as the primary AI context entry; pull sibling
  files only as the query requires.
- If consolidation is ever enabled again, keep project conventions in
  `AGENTS.md` Custom Instructions so regeneration preserves them, and
  run `python3 scripts/check-doc-paths.py` after any root/docs markdown
  edit.
- Prefer pointing agents at `docs/development/HANDOFF.md` and
  `docs/development/INVARIANTS.md` before changing delivery, ledger, or
  render paths — the summaries are an index, not a substitute for those
  pages.
