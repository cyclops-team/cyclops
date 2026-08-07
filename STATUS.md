# Status

Updated 2026-08-06. Cyclops is pre-release software at version `0.1.0`.
The Rust implementation on `main` is the current product; the shell/Python
implementation remains available as the read-only `v1` branch and
`v1-final` tag.

## Built

- A Rust daemon (`cyclopsd`) watches tmux panes without polling, combines
  manifest and hook signals into agent state, and persists an append-only
  NDJSON ledger.
- The CLI starts workspaces, names panes, reports state, sends messages,
  waits for completion, reads history and threads, manages hooks and themes,
  and saves or restores workspace layouts.
- Message delivery has per-recipient ordering, gate decisions, occupant
  checks, verified or screen-inferred receipts, retry limits, and ledgered
  causes.
- Bare `cyclops` opens the full-screen workspace. It provides a workspace
  sidebar, tabs, embedded pane terminals, split controls, drag-and-drop
  workspace ordering, mouse support, and the Cyclops theme vocabulary.
- `cyclops watch` opens the stream TUI; `cyclops watch --json` emits NDJSON.
  The old `cyclops ui` spelling remains only as a deprecated compatibility
  alias.
- Four detection manifests ship for Claude Code, Codex CLI, Antigravity CLI,
  and Cursor Agent CLI. New terminal agents are added with TOML rather than
  Rust code.
- Seven themes and four layout presets ship as resources compiled into the
  CLI and seeded into the Cyclops home without overwriting local edits.
- The source installer builds `cyclops` and `cyclopsd`, uses a user-writable
  prefix without sudo, backs up any shell profile it edits, and can uninstall
  its binaries and PATH block. The hosted website asset is required to match
  that tested installer byte for byte.

## Verification

The required local gates are:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
python3 scripts/check-doc-paths.py
./tests/e2e/parity-check.sh
```

CI runs those checks on Linux and macOS, reruns the suite with relocated
scratch storage, exercises the v1 compatibility shim, verifies the installer
with `./tests/e2e/parity-check.sh --with-installer`, and builds and checks the
website. An advisory job also tests against tmux built from its current
development branch.

The parity walk currently contains 115 checks, or 131 when installer checks
are included. Test counts are intentionally not pinned here because they
change whenever coverage grows.

## Known limits

- Quota-parked deliveries have no requeue verb by design.
- `cyclops start` cannot distinguish two otherwise identical live layouts
  until at least one pane has a Cyclops name.
- Pipe orchestration and automatic attention routing are not built. There is
  no `cyclops pipe` subcommand; Clap rejects that spelling as unrecognized.
- Existing v1 state is not migrated automatically. Use
  [the cutover runbook](docs/development/CUTOVER.md) if you still run v1.
- Hook-backed verification requires wiring the generated hook into the
  vendor CLI; without it, delivery can still finish with screen evidence and
  an explicitly unverified receipt.
- Renaming a watched session (folder-following does this) is followed live:
  the watcher matches the rename by the session's stable tmux id and the
  daemon's slot and durable adoption records move to the new name in place,
  so watching continues with no re-registration. Two edges remain: the open
  ledger file keeps its old-name path for that daemon run (a system line
  records the rename), and `config.toml`'s `sessions` list is not rewritten,
  so a restarted daemon waits on the old name until something re-registers
  the new one.

For the repository map and design boundaries, read
[docs/development/HANDOFF.md](docs/development/HANDOFF.md). For user-facing
setup, start with [docs/guides/install.md](docs/guides/install.md) and
[docs/guides/QUICKSTART.md](docs/guides/QUICKSTART.md). Historical milestone
details remain in [CHANGELOG.md](CHANGELOG.md).
