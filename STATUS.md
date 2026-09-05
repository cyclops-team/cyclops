# Status

Updated 2026-09-04. Cyclops is at version `1.1.0`. `main` is the line: pull
requests land on `main`, there is no standing integration branch, and the
release-binaries workflow publishes the matched `cyclops` and `cyclopsd` pair
as release assets when a `v*` tag is pushed. The legacy shell/Python
implementation remains available as the read-only `v1-final` tag.

## Built

- A Rust daemon (`cyclopsd`) watches tmux panes without polling, combines
  manifest and hook signals into agent state, and persists append-only
  NDJSON journals.
- The CLI starts workspaces, names panes, reports state, sends, replies,
  claims, waits on occupant-pinned pane state, reads history and threads,
  manages hooks and themes, and saves or restores workspace layouts.
- Delivery is a durable mailbox plus one doorbell line. A send is fsynced
  before any terminal write. The doorbell is one line, the summary beside the
  exact claim command, written and submitted for a bound, live agent process
  unless a human draft is positively observed or a named block is present
  (modal, permission, quota, dead, copy-mode, a doorbell the recipient has not
  consumed). Ambiguous or absent composer evidence does not hold it. The
  paste is read back once, Enter is pressed once, and the attempt records
  `submitted` or `submitted_unverified`, then `notified` with whatever receipt
  arrived: hook, screen, or none. `attention_required` is written only for a
  physical write failure or a daemon restart. Nothing is retried on a timer.
- `cyclops send --raw` and `cyclops reply --raw` paste the whole message and
  press Enter with no composer check, recorded as an unverified raw write.
- Durable workspace mailboxes expose body-free inbox and notification state,
  require an exact claim for payload access, preserve reply routing across
  label changes, and provide an authenticated administrator inbox.
- Operator recovery is append-only and explicit: withdraw an attempt that has
  not written, requeue an `attention_required` message, and clear alarms by
  exact id or through a confirmed age preview.
- Bare `cyclops` opens the full-screen workspace, seeding the shipped
  themes on the way in. It provides a workspace sidebar, tabs, embedded
  pane terminals, split controls, pane swapping by keyboard or drag,
  text selection with clipboard copy, drag-and-drop workspace ordering,
  mouse support, and the Cyclops theme vocabulary.
  Pane bodies paint the active theme's ground and ANSI-16 palette rather
  than inheriting the host terminal's, and theme switches or edits
  repaint the open workspace without a restart.
- `cyclops watch` opens the stream TUI; `cyclops watch --json` emits NDJSON.
- Twelve detection manifests ship. Five are measured against a live CLI:
  Claude Code, Codex CLI, Antigravity CLI, Cursor Agent CLI, and Kimi Code
  CLI. Seven are written from vendor documentation and marked
  `version_tested = "unverified"`: Gemini CLI, Qwen Code, goose, OpenCode,
  Amp, Crush, and aider. New terminal agents are added with TOML rather than
  Rust code.
- Seventeen themes and four layout presets ship as resources compiled into
  the CLI and seeded into the Cyclops home without overwriting local edits.
  Every theme carries a pane ground and a full ANSI-16 palette; homes
  seeded before those tokens existed resolve them from compiled defaults
  until reseeded.
- The installer downloads a published release pair for the host target when
  one exists (SHA256-verified) and otherwise builds `cyclops` and `cyclopsd`
  from source. It uses a user-writable prefix without sudo, backs up any
  shell profile it edits, and can uninstall its binaries and PATH block. The
  hosted website asset is required to match that tested installer byte for
  byte.

## Verification

The required local gates are:

The paired build is for the explicit real-daemon start assertion in
`workspace_cli::start_starts_a_daemon_when_none_is_running`.
`workspace_boot_sizing`'s sizing assertion does not require a daemon and
tolerates daemon-start failure.
The headless check keeps the no-default-feature CLI free of interactive UI
dependencies while exercising its retained command contracts.

```bash
./tests/e2e/messaging-docs-parity.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
./scripts/check-headless.sh
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
are evidence for those revisions, not a claim that the current head has
already rerun every campaign.

## Known limits

- The human-draft guard is a strong guard, not a guarantee. It depends on the
  manifest recognizing typed text. Unverified manifests never detect a human
  draft; deliveries to those panes are effectively raw, with a receipt.
- A person can type between the final capture and the tmux write; there is
  no input lease across that gap. The result is never silent: an attempt
  whose row did not read back exactly is recorded as `submitted_unverified`.
- `attention_required` needs a human. Inspect the pane, then
  `cyclops requeue <message-id>`. Nothing requeues automatically. A quota
  screen holds the doorbell at the gate until the pane changes; it is never
  retried on a clock.
- `cyclops start` cannot distinguish two otherwise identical live layouts
  until at least one pane has a Cyclops name.
- Pipe orchestration and automatic attention routing are not built. There is
  no `cyclops pipe` subcommand; Clap rejects that spelling as unrecognized.
- Existing v1 state is not migrated automatically. The v1 line is formally
  unsupported; the predecessor implementation is preserved read-only at tag
  `v1-final`.
- A hook-verified receipt requires wiring the generated hook into the vendor
  CLI. Without it a doorbell still settles as `notified`, on screen evidence
  or with no verifier at all.
- Renaming a watched session (folder-following does this) is followed live:
  the watcher matches the rename by the session's stable tmux id and the
  daemon's slot and durable adoption records move to the new name in place,
  so watching continues with no re-registration. The open ledger keeps its
  old-name path for continuity, and the authenticated system rename line is
  replayed at boot only when its old-name chain and persisted live identity
  validate. A restarted daemon then reconnects by stable tmux id without
  rewriting `config.toml` or creating an old-name ghost.
- Agent activity detection is version-specific. The 2026-08-28 evidence
  snapshot for the four manifests measured then is:

  | Vendor | Shipped version claim | Newer live evidence | Remaining gap |
  | --- | --- | --- | --- |
  | Claude Code | 2.1.221 | 2.1.248 discovered on the final host; older composer fixtures and soak remain valid historical evidence | Final live campaign unavailable; current idle, working, staged, modal, and quota matrix |
  | Codex CLI | 0.149.1 | 0.150.1 fresh-pane discovery in the final two-agent exercise; historical fresh, resumed, restart, draft, modal, raw, collapsed, claim, and reply evidence | Full current-version live matrix beyond the exercised cells |
  | Antigravity CLI | 1.1.11 | 1.1.22 live claim, reply, human-draft hold, partial deletion refusal, and final-backspace release; narrow truecolor empty-Context trailer evidence | Full current matrix beyond the exercised cells; automated clear keys remain unavailable |
  | Cursor Agent CLI | 2026.07.23-e383d2b | No installed binary on the evidence host | Installed current binary, full matrix, and paired start and end hook payloads |

  Kimi Code CLI's manifest was measured at 1.0.0 after that snapshot. The
  seven unverified manifests have no live evidence at all: they bind the
  pane and recognize their startup dialogs, nothing more. Manifest updates
  and evidence snapshots are maintained across released vendor updates.

For the repository map and design boundaries, read
[docs/development/HANDOFF.md](docs/development/HANDOFF.md). For user-facing
setup, start with [docs/guides/install.md](docs/guides/install.md) and
[docs/guides/QUICKSTART.md](docs/guides/QUICKSTART.md). Historical milestone
details remain in [CHANGELOG.md](CHANGELOG.md). The stabilization failures,
rejected alternatives, and final evidence are in
[docs/development/archive/STABILIZATION_HISTORY.md](docs/development/archive/STABILIZATION_HISTORY.md);
what is worth doing next is in [docs/development/NEXT.md](docs/development/NEXT.md).
