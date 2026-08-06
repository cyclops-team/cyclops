# Codebase Info

Basic facts about the Cyclops repository, gathered by static analysis.

## What Cyclops is

Cyclops is open-source coordination for coding agents running in a terminal.
A Rust daemon (`cyclopsd`) watches tmux sessions that hold AI-agent CLIs
(Claude Code, Codex CLI, Antigravity CLI, or anything a manifest describes),
fuses sensors to decide what each agent pane is doing, delivers structured
messages between agents by pasting into panes at safe moments, records every
fact in append-only NDJSON ledgers, and exposes everything over a Unix-socket
NDJSON protocol consumed by the CLI, the stream TUI (`cyclops watch`), and
the full-screen workspace (bare `cyclops`).
The signature device is "the eye": a header glyph that opens when something
needs a human.

## Languages and toolchains

| Language | Where | Notes |
|---|---|---|
| Rust (edition 2021) | `src/` — the product | Cargo workspace, resolver 2, all crates version 0.1.0. Stable toolchain; no `rustfmt.toml`, `clippy.toml`, or `deny.toml` (defaults are the standard) |
| TypeScript / Svelte 5 | `website/` | SvelteKit 2 marketing site for usecyclops.dev. Excluded from the Cargo workspace and checked by its own CI job |
| Python 3 | `scripts/check-doc-paths.py`, `scripts/commpact-shim/`, `tests/e2e/` | Doc-path gate, v1 compatibility shim tests, soak + probe harness |
| POSIX shell | `demos/`, `scripts/install.sh`, `tests/e2e/lib/lib.sh`, `resources/hooks/` | Runnable narrative demos, source installer, shared test machinery, vendor hook templates |

## Repository layout

```
cyclops/
├── Cargo.toml            # workspace root; excludes website/
├── src/                  # 9 product crates, one directory per crate
├── tests/
│   ├── testrig/          # cyclops-testrig: the isolated tmux server, workspace member
│   └── e2e/              # cross-crate/soak/parity tests; lib/ holds shared machinery
├── resources/
│   ├── manifests/        # per-CLI detection manifests (TOML, data not code)
│   ├── themes/           # 7 semantic-token color themes (TOML)
│   ├── layouts/          # workspace presets: solo, duo, quad, ops (TOML)
│   └── hooks/            # vendor hook config templates (agy, claude, codex, cursor)
├── docs/                 # guides/, reference/, development/, public/ (published)
├── website/              # SvelteKit landing page (outside the workspace)
├── demos/                # narrative isolated-tmux demos (not CI gates)
├── scripts/              # install.sh, check-doc-paths.py, commpact-shim/
├── .github/workflows/    # ci.yml (the only workflow)
├── README.md             # user front door; output blocks CI-verified
├── CONTRIBUTING.md       # development loop, demos, the gates a change must pass
├── STATUS.md             # maintained backlog, risks, known floors
├── findings.md           # measured facts (F13+) each with the probe that proved it
└── CHANGELOG.md          # per-milestone history
```

## The ten workspace crates

| Crate | Role |
|---|---|
| `cyclops-proto` | Wire protocol + ledger schema, delivery state machine, attention rule. Data types only, no IO |
| `cyclops-manifest` | Per-CLI detection manifests: TOML schema, regex compilation, rule evaluation |
| `cyclops-tmux` | The tmux adapter — every tmux invocation in the product lives here |
| `cyclops-ledger` | Append-only NDJSON ledger: fsynced writer, cursor-replayable reader |
| `cyclops-theme` | Semantic color tokens, theme TOML, 256-color fallback, stat-based hot reload |
| `cyclops-ui` | The live stream TUI behind `cyclops watch`, plus `grid`, the CLI/stream renderer vocabulary |
| `cyclops-workspace` | The full-screen workspace behind bare `cyclops`: Ratatui/Crossterm chrome and embedded pane VT runtimes |
| `cyclopsd` | The daemon: watcher, sensor fusion, ledger writing, delivery pipeline, socket API |
| `cyclops` | The CLI: thin NDJSON socket client, rendering, workspace verbs, hook receiver |
| `cyclops-testrig` | Test-only isolated tmux server with Drop-based teardown (`publish = false`) |

## Runtime footprint

- Binaries: `cyclopsd` (daemon) and `cyclops` (CLI). Nothing else ships.
- All runtime state lives under `$CYCLOPS_HOME` (default `~/.cyclops`):
  `config.toml`, `sock`, `ledger/<session>.ndjson`, `manifests/`, `themes/`,
  `workspaces/`, `registry.json`, `cyclopsd.log`, `spool/`, `hook-errors.log`.
- Requires tmux ≥ 3.2 (developed on 3.6a; CI also tests tmux built from master).

## License and provenance

MIT (`LICENSE`); upstream attribution for the v1 lineage in `NOTICE`.
This tree is a Rust rewrite. The previous shell/Python implementation lives on
branch `v1` (tag `v1-final`). The usecyclops.dev one-line installer builds the
current Rust implementation from `main`; `docs/development/CUTOVER.md` is the
optional v1 migration runbook.

## Version status

Pre-release (0.1.0). `STATUS.md` is the maintained statement of what is built;
`CHANGELOG.md` preserves the milestone history.
