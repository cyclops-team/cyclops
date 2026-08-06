# Interfaces

Every boundary a client, agent, or contributor programs against. The
authoritative pages are `docs/reference/PROTOCOL.md` (socket) and `docs/reference/MANIFESTS.md`
(manifest schema); this file is the summary plus the surfaces those pages do
not cover.

## The socket protocol (NDJSON over Unix socket)

Socket: `$CYCLOPS_HOME/sock`, 0700 home. One JSON object per line. The daemon
writes a `Hello { cyclops, proto, boot_id }` line first on every connection.
Then `Request { id, method, params }` / `Response { id, result | error }`.
After `events.subscribe`, the connection also carries pushed
`Event { event, data, seq }` lines. Compatibility: unknown fields are
tolerated in both directions; new fields are optional; a protocol version
mismatch warns and never rejects. Types live in
`src/cyclops-proto/src/wire.rs`.

| Method | Purpose |
|---|---|
| `ping` | Round trip |
| `status` | Sessions, panes with fused state, open deliveries, manifests, pid — the one answer that seeds every client's attention register |
| `pane.read` | Pane content: `visible`, `recent`, or `detection` (per-sensor readings behind the verdict) |
| `msg.send` | Deliver a message; optional `wait` (until idle/done/blocked) |
| `msg.history` / `msg.thread` | The record, with delivery chains folded in at read time |
| `agent.state.report` | Posted by `cyclops hook` when a vendor hook fires; feeds fusion and ACK matching |
| `agent.wait` | Block until an agent reaches a state; pinned to the occupant pid |
| `pane.label` | Adopt (name) or clear a pane |
| `hooks.verify` / `hooks.selftest` | Hook liveness; one no-op delivery proving the ack fires |
| `admin.notify` | Ping the admin (fyi / action-required / urgent) |
| `theme.reload` | Re-stat the theme, repaint borders, emit a `theme` event |
| `events.subscribe` | Switch the connection to the push stream (with kinds filter and cursor) |

Error codes are stable strings: `unknown_method`, `denied`, `no_such_target`,
`bad_request`, `timeout`, `occupant_changed`, `internal`, and friends.

Identity is fail-closed: peer credentials (uid, pid) are read from the
socket; `msg.send` sender identity is resolved by walking process ancestry to
a watched pane pid (`src/cyclopsd/src/identity.rs`), so nothing in a
message body can forge the FROM header.

## The CLI (`cyclops`)

All verbs take `--json` (raw socket answer) and `--plain`, and honor
`NO_COLOR` (`ui` has no `--json`; the machine stream is `cyclops watch
--json`).

| Verb | Purpose |
|---|---|
| `start` | Open/restore the default workspace, seed config+manifests+themes, start the daemon. Safe to run twice |
| `workspace save\|restore` | Session shape to/from `$CYCLOPS_HOME/workspaces` |
| `name <pane> <label>` | Adopt a pane (`--manifest` to pin, `--clear` to release) |
| `status` / `list` / `ping` | Roster and health |
| `send <agent> --subject … [--body …] [--all] [--fyi] [--reply-to] [--wait …]` | Deliver with a receipt; exit 0 delivered/queued, 1 parked/attention |
| `wait <agent> --until idle\|done\|blocked` | Exit 0 reached, 2 timeout, 3 occupant changed/died |
| `history` / `thread <id>` | The record |
| `read <agent> --source visible\|recent\|detection` | Pane content / sensor readings |
| `watch` | Live event stream, one JSON line each |
| `ui` | The stream TUI (firehose, filters, the eye, jump-to-pane) |
| `hook <event>` | Vendor-hook receiver: silent, exit 0 always, 3s budget |
| `hooks install\|verify\|selftest` | Render vendor hook config; prove liveness |
| `theme [name]` | List with previews, or switch (live at once) |
| `daemon status\|stop\|log` | Daemon lifecycle |

Usage errors exit 2.

## The vendor hook contract

A vendor CLI's hook config (templates in `resources/hooks/`, rendered by
`cyclops hooks install <cli> --agent <label>`) invokes `cyclops hook <event>`
with the payload on stdin. The receiver's contract is strict: fast, silent,
exit 0 always, 3 seconds total budget; agent identity from `--agent` or
`$CYCLOPS_AGENT`; it posts `agent.state.report` and appends failures to
`$CYCLOPS_HOME/hook-errors.log`. A hook ACK carrying the message id inside
the ACK window is what upgrades a delivery to `delivered_verified`.

## The manifest schema (TOML)

One file per agent CLI in `resources/manifests/` (page: `docs/reference/MANIFESTS.md`):

- `[agent]` — `id`, `display_name`, `process_names` (binds a pane by
  foreground command), `argv_basenames` (needed when the CLI installs as a
  versioned symlink, F21).
- `[[rule]]` — state detection off the pane title or bottom screen lines,
  priority-ordered; matchers are contains/regex/line_regex, with escaped
  (`_esc`) variants for SGR-colored captures; `decline_keys` and
  `auto_dismiss` drive modal handling.
- `[injection]` — paste method, submit key, `verify_pattern` (must contain
  `<message_id>` — that substitution proves the composer staged *this*
  message), safe/unsafe states.
- `[hooks]` — how the CLI's hook mechanism is configured and which event acks.

Unknown keys are tolerated so authors keep evidence next to the rule.

## File formats on disk

| File | Format |
|---|---|
| `$CYCLOPS_HOME/ledger/<session>.ndjson` | Append-only; one `LedgerLine` JSON object per line; readable with no daemon running |
| `$CYCLOPS_HOME/config.toml` | `sessions`, `theme`, `chrome`, timing knobs, `default_workspace`; unknown keys warn; missing file is a valid empty config |
| `$CYCLOPS_HOME/registry.json` | Versioned adoption roster, written whole on every change |
| `$CYCLOPS_HOME/workspaces/*.toml` | Saved session shapes (layout grammar in `src/cyclops-tmux/src/layout.rs`) |
| `resources/themes/*.toml` | `[colors]` token → hex; unknown tokens warn and fall back |

## Environment variables

| Variable | Effect |
|---|---|
| `CYCLOPS_HOME` | Runtime home (default `~/.cyclops`) |
| `CYCLOPS_THEME` | Theme override (beats the config key) |
| `CYCLOPS_LOG` | Daemon tracing filter (EnvFilter syntax) |
| `CYCLOPS_AGENT` | Agent identity for `cyclops hook` |
| `CYCLOPS_TEST_TMP` | Relocates the test scratch root (F24) |
| `NO_COLOR` | Color off everywhere; nothing is lost — every state pairs glyph + word |
