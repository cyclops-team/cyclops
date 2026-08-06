# Codebase Info

Basic facts about the Cyclops repository, gathered by static analysis.

## What Cyclops is

Cyclops is open-source coordination for coding agents running in a terminal.
A Rust daemon (`cyclopsd`) watches tmux sessions that hold AI-agent CLIs
(Claude Code, Codex CLI, Antigravity CLI, or anything a manifest describes),
fuses sensors to decide what each agent pane is doing, delivers structured
messages between agents by pasting into panes at safe moments, records every
fact in append-only NDJSON ledgers, and exposes everything over a Unix-socket
NDJSON protocol consumed by a CLI (`cyclops`) and a terminal UI (`cyclops ui`).
The signature device is "the eye": a header glyph that opens when something
needs a human.

## Languages and toolchains

| Language | Where | Notes |
|---|---|---|
| Rust (edition 2021) | `src/` — the product | Cargo workspace, resolver 2, all crates version 0.1.0. Stable toolchain; no `rustfmt.toml`, `clippy.toml`, or `deny.toml` (defaults are the standard) |
| TypeScript / Svelte 5 | `website/` | SvelteKit 2 marketing site for usecyclops.dev. **Excluded from the Cargo workspace**; treated as a read-only branding reference |
| Python 3 | `scripts/check-doc-paths.py`, `scripts/commpact-shim/`, `tests/e2e/` | Doc-path gate, v1 compatibility shim tests, soak + probe harness |
| POSIX shell | `demos/`, `scripts/install.sh`, `tests/e2e/lib/lib.sh`, `resources/hooks/` | Runnable narrative demos, source installer, shared test machinery, vendor hook templates |

## Repository layout

```
cyclops/
├── Cargo.toml            # workspace root; excludes website/
├── src/                  # 9 Cargo packages (the product), one dir per crate
├── tests/
│   ├── testrig/          # cyclops-testrig: the isolated tmux server, workspace member
│   └── e2e/              # cross-crate/soak/parity tests; lib/ holds shared machinery
├── resources/
│   ├── manifests/        # per-CLI detection manifests (TOML, data not code)
│   ├── themes/           # 7 semantic-token color themes (TOML)
│   ├── layouts/          # workspace presets: solo, duo, quad, ops (TOML)
│   └── hooks/            # vendor hook config templates (agy, claude, codex)
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

## The nine crates

| Crate | Role |
|---|---|
| `cyclops-proto` | Wire protocol + ledger schema, delivery state machine, attention rule. Data types only, no IO |
| `cyclops-manifest` | Per-CLI detection manifests: TOML schema, regex compilation, rule evaluation |
| `cyclops-tmux` | The tmux adapter — every tmux invocation in the product lives here |
| `cyclops-ledger` | Append-only NDJSON ledger: fsynced writer, cursor-replayable reader |
| `cyclops-theme` | Semantic color tokens, theme TOML, 256-color fallback, stat-based hot reload |
| `cyclops-ui` | The live stream TUI behind `cyclops ui`, plus `grid`, the shared renderer vocabulary |
| `cyclopsd` | The daemon: watcher, sensor fusion, ledger writing, delivery pipeline, socket API |
| `cyclops` | The CLI: thin NDJSON socket client, rendering, workspace verbs, hook receiver |
| `cyclops-testrig` | Test-only isolated tmux server with Drop-based teardown (`publish = false`) |

## Runtime footprint

- Binaries: `cyclopsd` (daemon) and `cyclops` (CLI). Nothing else ships.
- All runtime state lives under `$CYCLOPS_HOME` (default `~/.cyclops`):
  `config.toml`, `sock`, `ledger/<session>.ndjson`, `resources/manifests/`, `resources/themes/`,
  `workspaces/`, `registry.json`, `cyclopsd.log`, `spool/`, `hook-errors.log`.
- Requires tmux ≥ 3.2 (developed on 3.6a; CI also tests tmux built from master).

## License and provenance

MIT (`LICENSE`); upstream attribution for the v1 lineage in `NOTICE`.
This tree is a Rust rewrite. The previous shell/Python implementation lives on
branch `v1` (tag `v1-final`) and is what the usecyclops.dev one-line installer
currently fetches. `docs/development/CUTOVER.md` is the migration runbook.

## Version status

Pre-release (0.1.0). `STATUS.md` is the maintained statement of what is built;
`CHANGELOG.md` records what each milestone (M0–M6 so far) shipped.
