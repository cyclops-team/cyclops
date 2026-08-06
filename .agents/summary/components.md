# Components

Every major component, what it owns, and where its key types live.

## Rust crates

### cyclops-proto (`src/cyclops-proto`)

The foundation: data types only, no IO. Modules re-exported flat from
`lib.rs`.

- `state.rs` — `AgentState` (`Unknown, Idle, IdleWithInput, Working,
  BlockedModal, BlockedPermission, BlockedQuota, Dead`) with
  `safe_to_inject()` (only `Idle`), `glyph()`, `state_words()`; `Sensor`
  (`Hook, Title, Output, Screen`), `SensorReading`, `Detection`.
- `wire.rs` — the NDJSON socket protocol: `Hello`, `Request`, `Response`,
  `WireError`, `Event`, and every method's param/result types (see
  interfaces.md). Compatibility rule: unknown fields tolerated both ways;
  version mismatch warns, never rejects.
- `ledger.rs` — `LedgerLine`, `Kind` (`Msg, Fyi, System, State, Gate`),
  `Delivery`, `VerifiedBy` (`Hook|Screen`), and `DeliveryState`, the 10-state
  machine with `can_transition_to()` encoding every legal move.
- `attention.rs` — the one home of "what needs a human": the `Attention`
  register, `AttentionItem`, `Eye` (`Closed ‿ / Opening ◑ / Open ◉`),
  `EyeHeader`. Every surface (status, stream header, `--plain`) reads this;
  none recomputes it.
- `label.rs` — reserved-name rule (`admin`, `*`, `%`-prefixed refused, with
  the reason spelled out).
- `scratch.rs` — test scratch paths (`CYCLOPS_TEST_TMP` override;
  `/private/tmp` on macOS for short socket paths).
- Also: `PROTOCOL_VERSION = 1`, `cyclops_home()` (`$CYCLOPS_HOME` or
  `~/.cyclops`), `socket_path()`.

### cyclops-manifest (`src/cyclops-manifest`)

Single `lib.rs`. Parses, validates, compiles, and evaluates per-CLI detection
manifests (TOML). Key types: `Manifest` (`AgentMeta`, `Hooks`,
`Vec<CompiledRule>`, `Injection`), `Region` (`PaneTitle` |
`BottomNonEmptyLines(n)`), `CompiledMatcher` (contains / regex / line_regex /
`line_regex_esc` for SGR-escaped captures — esc rules fail closed without an
escaped capture). `Manifest::evaluate()` walks rules highest-priority-first;
first match wins. `load_dir()` loads a directory at daemon boot — no hot
reload. It does *not* decide pane state (fusion does) or capture screens
(the tmux crate does).

### cyclops-tmux (`src/cyclops-tmux`)

The adapter and "blast wall": nothing outside this crate speaks to tmux.

- `control.rs` — `ControlClient`: one `tmux -C` child per watched session,
  FIFO reply correlation over `%begin/%end/%error`, flow control via
  `pause-after` with auto-resume, typed helpers (`capture_pane`,
  `send_keys`, `load_buffer` spooled 0600, `paste_buffer`).
- `watcher.rs` — `SessionWatcher`: zero-polling reconciling pane table.
  Per-pane `refresh-client -B` subscriptions push field changes; a
  session-wide subscription catches pane death (a per-pane one cannot, F25);
  structural notifications trigger a 30ms-debounced reconcile against
  `list-panes`. Emits `PaneEvent` over `broadcast`.
- `notify.rs` — parses every `%`-prefixed control-mode line into
  `Notification`; unknown lines are data (`Other`), never errors; `%output`
  payloads are octal-unescaped bytes, not guaranteed UTF-8 (F22).
- `layout.rs` — workspace layouts: `Layout → Window → Row → Pane`, a
  grid-of-rows model; `capture()` refuses non-grid windows, `apply()` refuses
  existing sessions.
- `version.rs` — `TmuxVersion` with named predicates
  (`has_bracket_paste_flag()` ≥3.8, `has_pause_after()` ≥3.2); version checks
  are never call-site comparisons.
- `quote.rs`, `focus.rs`, `error.rs`, private `cmd.rs` (always `-u`, F14).

### cyclops-ledger (`src/cyclops-ledger`)

`LedgerWriter`: `open()` seals a torn final line and continues seq numbering
across restarts (seq monotonic per file; `boot_id` marks which run wrote a
line); `append()` fsyncs before returning. Readers: `read_after(path,
cursor)` full scan, invalid lines skipped with a warning. Deliberately no
index. Filtering/folding lives in `src/cyclopsd/src/history.rs`.

### cyclops-theme (`src/cyclops-theme`)

22 semantic tokens (`role.1..8`, `surface.*`, `eye.*`, `state.*`,
`badge.*`); `state_token(AgentState)` and `delivery_token(DeliveryState)`
group mappings; `Color` with hex parse and nearest-xterm-256 derivation;
tolerant `Theme::parse` (unknown tokens warn and fall back — resolution is
total); `select.rs` — selection order `CYCLOPS_THEME` env → `theme` key in
config → `dark`; `ThemeWatch` hot reload as a *stat* (mtime+len stamps
checked when a repaint is already due), never a watcher thread or timer.

### cyclops-ui (`src/cyclops-ui`)

The stream TUI behind `cyclops ui`. `run(UiOptions)` builds a current-thread
runtime; non-tty forces the line-oriented `plain` mode. `app.rs` is pure UI
state (entry ring of 10k, Admin/Firehose views, attention delegated entirely
to proto); `data.rs` does IO (connect, one `status` call, subscribe, one
ledger backfill); `entry.rs` normalizes events and ledger lines;
`frame.rs` builds frames as pure functions; `term.rs` is a hand-rolled
terminal layer (termios via libc — no TUI crates, offline build);
`grid.rs` is the product's one voice for state cells, badges, clock gutters,
and cause words — the CLI renders through it rather than holding a copy.

### cyclopsd (`src/cyclopsd`)

Library + thin binary (all logic in the library so tests boot the daemon
in-process).

- `lib.rs` — shared `Inner` state (config, manifests, sessions, event
  broadcast, detection cache, registry, theme watch, delivery engine),
  the public `Daemon` handle, `boot()`, session attach/pump/reattach loop,
  pane-event handling, output-settle debounce (300ms).
- `config.rs` — `Config` from `$CYCLOPS_HOME/config.toml`; tolerant parse;
  missing file is a valid empty config.
- `fusion.rs` — sensor fusion: manifest binding (pin → comm → argv basename,
  F21), title tier decides when possible, screen capture is evidence of last
  resort, capture failure keeps the prior verdict, hook readings age out
  (TTL 300s, contradiction limit 3). Emits state ledger lines + events.
- `delivery.rs` — the delivery pipeline (spec: `docs/development/DELIVERY.md`): one FIFO
  worker per target pane; gate → paste → verify → submit → ACK; every
  transition ledger-logged; quota parks are terminal; occupant pid re-checked
  before paste *and* before submit.
- `ack.rs` — `agent.state.report` ingestion, dedupe, ACK matching.
- `history.rs` — read model for `msg.history` / `msg.thread`: folds delivery
  state lines back into msg lines at read time; disk never rewritten.
- `identity.rs` — fail-closed sender identity from socket peer credentials,
  walking process ancestry to a watched pane pid.
- `registry.rs` — durable adoption roster in `$CYCLOPS_HOME/registry.json`;
  restore prunes entries whose pane id or root pid no longer match.
- `chrome.rs` — pane border decoration, written on exactly eight named edges;
  `chrome = "off"` gates all writes here and nowhere else.
- `selftest.rs` — hook liveness keyed to occupant pid; `hooks.verify` and
  `hooks.selftest` (one fyi marker through the real pipeline).
- `server.rs` — Unix socket: stale-socket protocol, hello first, dispatch,
  event pump after `events.subscribe`, 5s write timeout drops wedged clients.

### cyclops (`src/cyclops`)

The CLI binary ("One eye on every agent").

- `client.rs` — synchronous NDJSON client; reads `Hello` first.
- `main.rs` — clap subcommands (see interfaces.md); global `--json` and
  `--plain`; exit-code conventions.
- `workspace.rs` — `cyclops start` (seed config/resources/manifests/themes, ensure
  daemon, restore-or-build preset, name panes, attach), save/restore.
  Presets are embedded from `resources/layouts/` with `include_str!` so a fresh
  install works with no files.
- `daemon.rs` — `ensure_running` (spawn detached `cyclopsd`, log to
  `$CYCLOPS_HOME/cyclopsd.log`, wait ≤10s for the socket), stop, log.
- `hook.rs` — the receiver vendor hooks invoke: fast, silent, exit 0 always,
  3s budget; failures append to `$CYCLOPS_HOME/hook-errors.log`.
- `hookset.rs` — renders vendor hook configs (claude/codex/agy); never
  writes into vendor dot-dirs.
- `render.rs` — layout for status/list/history/thread/receipts, reading
  proto's attention register and the UI's `grid` rather than deciding
  anything. `copy.rs` holds every user-facing sentence in one place.
- `theme.rs`, `manifests.rs`, `themeseed.rs` — theme switching and seeding
  shipped data into `$CYCLOPS_HOME`.

### cyclops-testrig (`tests/testrig`)

Test-only, zero dependencies. `TmuxServer::new(tag)` reserves a private
`-L cyc-<tag>-<pid>` socket; every command applies `-u -L <sock> -f
/dev/null` and unsets `TMUX`; `Drop` kills the server *and unlinks the
socket file* (stopping the server alone does not unlink — measured). `tmux_available()`
lets suites skip cleanly. Two guard tests keep the rule from being copied
back out: `teardown_has_one_home.rs` (no other Rust file may start or kill a
tmux server) and `shell_teardown.rs` (holds `tests/e2e/lib/lib.sh` to the same
contract).

## Non-crate components

| Component | What it is |
|---|---|
| `website/` | SvelteKit 2 / Svelte 5 (runes) static marketing site for usecyclops.dev. 19 components, one route, plain CSS design tokens, no backend communication except a GitHub star-count fetch. Excluded from the workspace; read-only branding reference |
| `resources/manifests/` | Shipped detection manifests: `agy.toml`, `claude.toml`, `codex.toml`, `cursor.toml`. Compiled into the CLI with `include_str!` and seeded to `$CYCLOPS_HOME/manifests` on first `cyclops start`, never overwritten after |
| `resources/themes/` | 7 themes: dark, light (derived from the frontend's CSS tokens), catppuccin, gruvbox, nord, tokyo-night, high-contrast |
| `resources/layouts/` | Presets `solo`, `duo`, `quad`, `ops` — each the previous plus a pane |
| `resources/hooks/` | Vendor hook config templates per CLI (agy, claude, codex) rendered by `cyclops hooks install` |
| `demos/` | Seven runnable end-to-end scripts on isolated tmux servers; `tests/e2e/parity-check.sh` is the CI gate that asserts docs and binaries agree |
| `scripts/` | `install.sh` (POSIX source installer: builds, places binaries, edits profile with backup, `--uninstall` restores), `check-doc-paths.py` (doc-path + orphan gate), `commpact-shim/` (v1 compatibility shim + tests) |
| `tests/` | Python soak gate `m1_soak.py` and probe harness (`tests/e2e/lib/`) |
