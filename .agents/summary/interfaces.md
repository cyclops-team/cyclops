# Interfaces

Every boundary a client, agent, or contributor programs against. The
authoritative pages are `docs/reference/PROTOCOL.md` (socket),
`docs/reference/MANIFESTS.md` (manifest schema), and
`docs/reference/hooks.md` (vendor hooks); this file is the summary plus the
surfaces those pages do not cover.

## The socket protocol (NDJSON over Unix socket)

Socket: `$CYCLOPS_HOME/sock`, 0700 home. One JSON object per line. The daemon
writes a `Hello { cyclops, proto, boot_id }` line first on every connection.
Then `Request { id, method, params }` / `Response { id, result | error }`.
After `events.subscribe`, the connection also carries pushed
`Event { event, data, seq }` lines. Compatibility: unknown fields are
tolerated in both directions; new fields are optional; a protocol version
mismatch warns and never rejects. Types live in
`src/cyclops-proto/src/wire.rs`. Protocol v1 answers these 17 methods
(asserted in `src/cyclopsd/src/server.rs`):

| Method | Purpose |
|---|---|
| `ping` | Round trip |
| `status` | Sessions, panes with fused state, open deliveries, manifests, pid — the one answer that seeds every client's attention register |
| `pane.read` | Pane content: `visible`, `recent`, or `detection` (per-sensor readings behind the verdict) |
| `pane.label` | Adopt (name) or clear a pane |
| `session.watch` | Start watching a tmux session the daemon was not booted with |
| `msg.send` | Deliver a message; optional `wait` (until idle/done/blocked) |
| `msg.history` / `msg.thread` | The record, with delivery chains folded in at read time |
| `agent.state.report` | Posted by `cyclops hook` when a vendor hook fires; feeds fusion and ACK matching |
| `agent.wait` | Block until an agent reaches a state; pinned to the occupant pid |
| `hooks.verify` / `hooks.selftest` | Hook liveness; one no-op delivery proving the ack fires |
| `admin.notify` | Ping the admin (fyi / action-required / urgent) |
| `theme.reload` | Re-stat the theme, repaint borders, emit a `theme` event |
| `workspace_ui.get` / `workspace_ui.set` | Volatile last-active session/window for the workspace UI |
| `events.subscribe` | Switch the connection to the push stream (with kinds filter and cursor) |

Error codes are stable strings: `unknown_method`, `denied`, `no_such_target`,
`bad_request`, `timeout`, `occupant_changed`, `internal`, and friends.

Identity is fail-closed: peer credentials (uid, pid) are read from the
socket; `msg.send` sender identity is resolved by walking process ancestry to
a watched pane pid (`src/cyclopsd/src/identity.rs`), so nothing in a
message body can forge the FROM header.

## The CLI (`cyclops`)

CLI subcommands accept the global `--json` and `--plain` forms where they
apply, and honor `NO_COLOR`. Bare `cyclops` opens the full-screen workspace
(TTY required; seeds themes/manifests on the way in). The deprecated `ui`
alias has no JSON form.

| Verb | Purpose |
|---|---|
| *(none)* | Open the full-screen workspace |
| `start` | Open/restore the default workspace, seed config+manifests+themes, start the daemon. Safe to run twice. Notable flags: `--preset`, `--agents <ids>`, `--launch`, `--setup-only`, `--wire-hooks` (requires `--setup-only`; installer opt-in), `--no-daemon` |
| `workspace save\|restore` | Session shape to/from `$CYCLOPS_HOME/workspaces` |
| `name <pane> <label>` | Adopt a pane (`--manifest` to pin, `--clear` to release, `--self` for the calling pane) |
| `status` / `list` / `ping` | Roster and health (`list` scopes to the caller's tmux session; `--all` for every watched session) |
| `send <agent> --subject … [--body …] [--all] [--fyi] [--reply-to] [--wait …]` | Deliver with a receipt; exit 0 delivered/queued, 1 parked/attention |
| `wait <agent> --until idle\|done\|blocked` | Exit 0 reached, 2 timeout, 3 occupant changed/died |
| `history` / `thread <id>` | The record |
| `read <agent> --source visible\|recent\|detection` | Pane content / sensor readings (`--raw` with detection) |
| `watch` | Stream TUI by default; live NDJSON with `--json` |
| `ui` | Deprecated compatibility alias for the stream TUI |
| `hook <event>` | Vendor-hook receiver: silent, exit 0 always, 3s budget |
| `hooks install\|verify\|selftest` | Render vendor hook config under `$CYCLOPS_HOME/hooks/`; prove liveness |
| `theme [name]` | List with previews, or switch (live at once) |
| `update` | Fetch source, rebuild, replace binaries; refresh already-installed hooks; print restart steps |
| `daemon status\|stop\|log` | Daemon lifecycle |

Usage errors exit 2. Confirm flags with `cyclops --help` / `cyclops <cmd> --help`
on the machine — docs and the agent skill can drift.

## The vendor hook contract

Authoritative page: `docs/reference/hooks.md`.

A vendor CLI's hook config (templates in `resources/hooks/`) invokes
`cyclops hook <event>` with the payload on stdin. The receiver's contract is
strict: fast, silent, exit 0 always, 3 seconds total budget; agent identity
from `--agent` or `$CYCLOPS_AGENT`; it posts `agent.state.report` and appends
failures to `$CYCLOPS_HOME/hook-errors.log`. A hook ACK carrying the message
id inside the ACK window upgrades a delivery to `delivered_verified`.

Two install paths:

1. **`cyclops hooks install <cli> --agent <label>`** — stages under
   `$CYCLOPS_HOME/hooks/<label>/` and prints wiring instructions; does not
   write vendor dot-dirs by itself.
2. **Installer wiring (`--setup-only --wire-hooks`)** — merges into the
   paths each CLI actually reads (Claude: per-pane settings launch path;
   Codex: `$CODEX_HOME/hooks.json`; Antigravity: `~/.agents/hooks.json`;
   Cursor: `~/.cursor/hooks.json`), with backup `*.before-cyclops` on first
   edit. Opt-in only; `CYCLOPS_NO_VENDOR_HOOKS=1` declines. `cyclops update`
   refreshes artifacts it already installed so absolute binary paths stay
   current.

`cyclops start --agents` sets `CYCLOPS_AGENT` per pane via tmux `-e` so a
shared vendor hooks file can still report the right label.

## The manifest schema (TOML)

One file per agent CLI in `resources/manifests/` (page: `docs/reference/MANIFESTS.md`):

- `[agent]` — `id`, `display_name`, `process_names` (binds a pane by
  foreground command), `argv_basenames` (needed when the CLI installs as a
  versioned symlink, F21), launch command fields used by `--agents`.
- `[[rule]]` — state detection off the pane title or bottom screen lines,
  priority-ordered; matchers are contains/regex/line_regex, with escaped
  (`_esc`) variants for SGR-colored captures; `decline_keys` and
  `auto_dismiss` drive modal handling. First match after descending priority
  wins — idle rules must not outrank working rules when the composer stays
  painted during a turn.
- `[injection]` — paste method, submit key, `verify_pattern` (must contain
  `<message_id>` — that substitution proves the composer staged *this*
  message), safe/unsafe states.
- `[hooks]` — how the CLI's hook mechanism is configured and which event acks.

Unknown keys are tolerated so authors keep evidence next to the rule.

## File formats on disk

| File | Format |
|---|---|
| `$CYCLOPS_HOME/ledger/<session>.ndjson` | Append-only; one `LedgerLine` JSON object per line; readable with no daemon running |
| `$CYCLOPS_HOME/config.toml` | `sessions`, `theme`, `chrome`, timing knobs, `default_workspace`, `[workspace]` UI prefs; unknown keys warn; missing file is a valid empty config |
| `$CYCLOPS_HOME/registry.json` | Versioned adoption roster, written whole on every change |
| `$CYCLOPS_HOME/workspaces/*.toml` | Saved session shapes (layout grammar in `src/cyclops-tmux/src/layout.rs`) |
| `$CYCLOPS_HOME/hooks/<label>/` | Staged vendor hook configs + install receipts |
| `resources/themes/*.toml` | `[colors]` token → hex; unknown tokens warn and fall back |

## Environment variables

| Variable | Effect |
|---|---|
| `CYCLOPS_HOME` | Runtime home (default `~/.cyclops`) |
| `CYCLOPS_THEME` | Theme override (beats the config key) |
| `CYCLOPS_LOG` | Daemon tracing filter (EnvFilter syntax) |
| `CYCLOPS_AGENT` | Agent identity for `cyclops hook` (also set per pane by `start --agents`) |
| `CYCLOPS_TEST_TMP` | Relocates the test scratch root (F24) |
| `CYCLOPS_MOTION` | Workspace motion override (`0` forces off) |
| `CYCLOPS_REPO` / `CYCLOPS_REF` | Source for `cyclops update` / installer (defaults: GitHub cyclops `main`) |
| `CYCLOPS_NO_VENDOR_HOOKS` | When set, declines installer `--wire-hooks` |
| `NO_COLOR` | Color off everywhere; nothing is lost — every state pairs glyph + word |

## Agent skill

`skills/cyclops/SKILL.md` teaches other coding agents how to discover peers
and send via the CLI. Treat live `--help` as authoritative over the skill
text when they disagree.
