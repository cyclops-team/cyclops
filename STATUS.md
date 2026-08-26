# Status

Updated 2026-08-26. Cyclops is pre-release software at version `0.1.0`.
The Rust implementation on `main` is the current product; the shell/Python
implementation remains available as the read-only `v1` branch and
`v1-final` tag.

## Built

- A Rust daemon (`cyclopsd`) watches tmux panes without polling, combines
  manifest and hook signals into agent state, and persists an append-only
  NDJSON ledger.
- The CLI starts workspaces, names panes, reports state, sends messages,
  waits on occupant-pinned pane state, reads history and threads, manages hooks and themes,
  and saves or restores workspace layouts.
- Message delivery has per-recipient ordering, gate decisions, occupant
  checks, verified or screen-inferred receipts, retry limits, and ledgered
  causes.
- Durable workspace mailboxes accept messages before any terminal write,
  expose body-free inbox and notification state, require an exact claim for
  payload access, preserve reply routing across label changes, and provide an
  authenticated administrator inbox.
- Notification recovery is append-only and operator explicit. Eligible
  messages can be requeued, and alarms can be cleared by exact id or through
  an age preview whose exact id set is confirmed before mutation.
- Bare `cyclops` opens the full-screen workspace, seeding the shipped
  themes on the way in. It provides a workspace sidebar, tabs, embedded
  pane terminals, split controls, pane swapping by keyboard or drag,
  text selection with clipboard copy, drag-and-drop workspace ordering,
  mouse support, and the Cyclops theme vocabulary.
  Pane bodies paint the active theme's ground and ANSI-16 palette rather
  than inheriting the host terminal's, and theme switches or edits
  repaint the open workspace without a restart.
- `cyclops watch` opens the stream TUI; `cyclops watch --json` emits NDJSON.
  The old `cyclops ui` spelling remains only as a deprecated compatibility
  alias.
- Four detection manifests ship for Claude Code, Codex CLI, Antigravity CLI,
  and Cursor Agent CLI. New terminal agents are added with TOML rather than
  Rust code.
- Seventeen themes and four layout presets ship as resources compiled into
  the CLI and seeded into the Cyclops home without overwriting local edits.
  Every theme carries a pane ground and a full ANSI-16 palette; homes
  seeded before those tokens existed resolve them from compiled defaults
  until reseeded.
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

The parity walk currently contains 123 checks before the optional installer
exercise. Test counts are intentionally not pinned here because they change
whenever coverage grows.

## Known limits

- A quota-held mailbox notification never retries automatically. After fresh
  screen evidence records that quota has reset, the workspace administrator
  must run `cyclops requeue <message-id>`. Legacy direct-delivery quota parks
  remain terminal and require a fresh send.
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
- Agent activity detection remains version-specific and conservative. Unknown
  chrome or a vendor version without sufficient current evidence holds terminal
  writes instead of guessing. The 2026-08-25 evidence snapshot is:

  | Vendor | Shipped version claim | Newer live evidence | Remaining gap |
  | --- | --- | --- | --- |
  | Claude Code | 2.1.221 | 2.1.239 composer extraction, clearing, and stage-and-clear soak | Current idle, working, staged, modal, and quota matrix |
  | Codex CLI | 0.147.0 | 0.149.1 occupied-prompt and no-color trailer structure; 0.149.0 clearing, title spinner, and stage-and-clear soak | Current full matrix, fresh and resumed delivery, and live hook payload capture |
  | Antigravity CLI | 1.1.21 | 1.1.21 exact composer and file-access permission; 1.1.18 stage-and-clear soak | Current full matrix beyond the measured composer and permission states, plus lifecycle evidence |
  | Cursor Agent CLI | 2026.07.23-e383d2b | No installed binary on the evidence host | Installed current binary, full matrix, and paired start and end hook payloads |

  The soak proves staging verification and cleanup only. It does not promote a
  whole manifest to the newer version. Detection gaps remain tracked in
  [issue #7](https://github.com/cyclops-team/cyclops/issues/7).

For the repository map and design boundaries, read
[docs/development/HANDOFF.md](docs/development/HANDOFF.md). For user-facing
setup, start with [docs/guides/install.md](docs/guides/install.md) and
[docs/guides/QUICKSTART.md](docs/guides/QUICKSTART.md). Historical milestone
details remain in [CHANGELOG.md](CHANGELOG.md).
