# Status

Updated 2026-08-31. Cyclops is pre-release software; the Cargo workspace
currently declares `0.1.0`. `main` is the stable line and
**beta/messaging-rework** is the active beta integration line. The shell/Python
implementation remains available as the read-only `v1-final` tag.

Track A, the Messaging Beta Rework, is accepted. `WorkspaceMessaging` now owns
current durable messaging policy. Retained session-journal discovery and replay
go through the narrow compatibility-history adapter.

The whole-product implementation tracks are integrated. The
[Cyclops Beta Charter](docs/development/CYCLOPS_BETA_CHARTER.md) now governs
final acceptance corrections and release evidence. No beta version or release
name has been assigned: the Cargo version, existing `v0.2.0-beta` tag, and
absence of GitHub Release objects still require operator-directed reconciliation.

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
- Agent activity and composer safety are separate. Visible human input holds a
  notification even when the agent is idle. Partial deletion remains held; a
  settled exact empty composer releases the same unowned human attempt through
  the ordinary gate. Hidden, ambiguous, stale, modal, replacement, and
  recovery-owned holds remain blocked.
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

The paired build is for the explicit real-daemon start assertion in
`workspace_cli::start_starts_a_daemon_when_none_is_running`.
`workspace_boot_sizing`'s sizing assertion does not require a daemon and
tolerates daemon-start failure.

```bash
./tests/e2e/messaging-docs-parity.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
python3 scripts/check-doc-paths.py
cargo build -p cyclops -p cyclopsd --bins
cargo nextest run --workspace -E 'not (package(cyclopsd) | binary_id(=cyclops-ui::perf) | binary_id(=cyclops-ui::queue_perf) | binary_id(=cyclops-workspace::perf_contract))' --no-fail-fast
cargo test -p cyclopsd --all-targets --no-fail-fast
cargo doc --workspace --no-deps
./tests/e2e/parity-check.sh
```

The six stable pull-request check names always report. Path classification
decides whether macOS, installer, website, and tmux HEAD checks run substantive
evidence or return an explicit successful not-applicable result. The required
Ubuntu lane runs the commands above plus focused relocated-root evidence.
Scheduled and release workflows own the full platform matrix, complete tmux
HEAD evidence, reliability repetitions, soak, performance history, historical
replay, installer lifecycle, and release journeys. The current routing,
commands, and measured baseline live in
[docs/development/CI.md](docs/development/CI.md).

Historical stabilization, CI, release, performance, and live-vendor results
remain tied to their named revisions in the records that produced them. They
are evidence for those revisions, not a claim that the current integration
head has already rerun every campaign.

## Known limits

- A quota-held mailbox notification never retries automatically. After fresh
  screen evidence records that quota has reset, the workspace administrator
  must run `cyclops requeue <message-id>`. Legacy direct-delivery quota parks
  remain terminal and require a fresh send.
- `cyclops start` cannot distinguish two otherwise identical live layouts
  until at least one pane has a Cyclops name.
- Pipe orchestration and automatic attention routing are not built. There is
  no `cyclops pipe` subcommand; Clap rejects that spelling as unrecognized.
- Existing v1 state is not migrated automatically. The v1 line is formally
  unsupported; the predecessor implementation is preserved read-only at tag
  `v1-final`.
- Hook-backed verification requires wiring the generated hook into the
  vendor CLI; without it, delivery can still finish with screen evidence and
  an explicitly unverified receipt.
- Renaming a watched session (folder-following does this) is followed live:
  the watcher matches the rename by the session's stable tmux id and the
  daemon's slot and durable adoption records move to the new name in place,
  so watching continues with no re-registration. The open ledger keeps its
  old-name path for continuity, and the authenticated system rename line is
  replayed at boot only when its old-name chain and persisted live identity
  validate. A restarted daemon then reconnects by stable tmux id without
  rewriting `config.toml` or creating an old-name ghost.
- Agent activity detection remains version-specific and conservative. Unknown
  chrome or a vendor version without sufficient current evidence holds terminal
  writes instead of guessing. The 2026-08-28 evidence snapshot is:

  | Vendor | Shipped version claim | Newer live evidence | Remaining gap |
  | --- | --- | --- | --- |
  | Claude Code | 2.1.221 | 2.1.248 discovered on the final host; older composer fixtures and soak remain valid historical evidence | Final live campaign unavailable; current idle, working, staged, modal, and quota matrix |
  | Codex CLI | 0.149.1 | 0.150.1 fresh-pane discovery in the final two-agent exercise; historical fresh, resumed, restart, draft, modal, raw, collapsed, claim, and reply evidence | Full current-version live matrix beyond the exercised cells |
  | Antigravity CLI | 1.1.11 | 1.1.22 live Format 3 claim, reply, human-draft hold, partial deletion refusal, and final-backspace release; narrow truecolor empty-Context trailer evidence | Full current matrix beyond the exercised cells; automated clear keys remain unavailable |
  | Cursor Agent CLI | 2026.07.23-e383d2b | No installed binary on the evidence host | Installed current binary, full matrix, and paired start and end hook payloads |

  The soak proves staging verification and cleanup only. It does not promote a
  whole manifest to the newer version. Track E owns the remaining current
  vendor-capability evidence and conservative manifest updates.

For the repository map and design boundaries, read
[docs/development/HANDOFF.md](docs/development/HANDOFF.md). For user-facing
setup, start with [docs/guides/install.md](docs/guides/install.md) and
[docs/guides/QUICKSTART.md](docs/guides/QUICKSTART.md). Historical milestone
details remain in [CHANGELOG.md](CHANGELOG.md). The stabilization failures,
rejected alternatives, and final evidence are in
[docs/development/STABILIZATION_HISTORY.md](docs/development/STABILIZATION_HISTORY.md);
prioritized architecture follow-up is in
[docs/development/NEXT.md](docs/development/NEXT.md).
