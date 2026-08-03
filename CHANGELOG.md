# Changelog

All notable changes to Cyclops v2. Format follows Keep a Changelog;
versions are unreleased until admin cuts a tag.

## [Unreleased]

### Added (M3: the stream UI, cyclops ui)

- crates/cyclops-ui plus the `cyclops ui` verb (dispatch-only wiring in
  the CLI): the live stream. Admin view by default and deliberately calm:
  only messages addressed to admin, deliveries whose latest state is
  attention_required or parked, agents entering a blocked_* state, gate
  holds whose cause names a blocked pane, and every admin ping
  (hook-unverified notices arrive as pings). A delivery held merely
  because the recipient is mid-turn is routine and stays in the firehose.
  The firehose (tab) shows every message, delivery transition, state
  change, gate decision, and session event; a message to admin appears in
  both views.
- THE EYE in the header: `‿` closed when calm, `◑` opening at one
  attention item, `◉` open with the count beside it (glyph set documented
  with the theme tokens in cyclops-ui/src/theme.rs; colors ride eye.calm
  and eye.alert). Attention items are currently-blocked agents plus
  deliveries sitting in attention_required or parked_blocked_quota, keyed
  per (recipient, message) so a later message to the same agent can never
  clear an earlier one's item: only that delivery's own next transition
  does, and both those states are terminal until an operator requeues. The
  eye ticks through at most one intermediate frame per change on a single
  one-shot timer; nothing animates continuously, nothing blinks. --plain
  prints it as a word line ("eye opening · 1 needs attention").
- Rendering on the GOALS grid: an aligned HH:MM:SS gutter with hanging
  indents at the content column, role color and state glyph as the only
  meaning-carrying encodings, delivery badges byte-identical to M1
  receipts (pinned by tests against the CLI's exact strings), density
  modes (c: comfortable with body lines and breathing room, compact one
  line per entry). No reflow on arrival: autowrap is off (long lines clip,
  never wrap), pinned-to-tail scrolls, and an unpinned viewport anchors to
  an entry uid so arrivals append below it. Keys: tab view, w/f/t filter
  input mirroring the history flags, up/down/end scroll and repin, enter
  jump, c density, ? cheatsheet, q quit.
- Data: events.subscribe live push plus a one-time ledger-tail backfill
  (default 200 lines, --backfill N), merged behind a buffering intake
  that dedupes by ledger seq when one session file exists. One status
  request at startup seeds the label-to-pane map and current states.
  All IO on separate tasks feeding one channel; the event loop never
  blocks on the daemon, keypresses are handled between IO batches.
- Fluidity, measured: 10,000-entry ring with windowed rendering; frame
  build at 220x60 over the full ring is 0.33 to 0.35ms median in a debug
  build across three runs, and 0.12ms for the admin view, which filters
  the whole ring every frame; ingesting all 10,000 entries takes 3.2ms.
  Budget is 16ms, one 60Hz frame; tests/perf.rs asserts and prints both.
- Jump-to-pane: enter resolves the entry's agent through the harvested
  pane map and calls the new cyclops-tmux `focus_pane` helper (one-shot
  `tmux -u select-window` + `select-pane`, adapter-only rule intact,
  proven against an isolated tmux server).
- --plain, or a non-terminal stdin or stdout, degrades to a line-oriented
  follow mode: backfill first, then each admitted event, eye word lines,
  standard connection-loss copy and exit 1 when the daemon goes away.
  Plain mode carries the same content as the sighted comfortable view,
  message body lines included: it is the screen-reader path, so it is an
  accessibility peer rather than a reduced view.
  NO_COLOR is a color preference, not a mode: it keeps the full stream UI
  and turns the color off. Every state pairs a glyph with a word, so the
  UI is legible with no color at all, and conflating the two would have
  cost a NO_COLOR user the eye, the filters, scrolling and the jump.
  `cyclops ui --json` refuses and points at `cyclops watch --json`.
- The TUI terminal layer is hand-rolled (termios raw mode, alternate
  screen, single-write frames with per-line clears) behind a pure frame
  builder: the offline build environment carries no TUI crates, so
  ratatui/crossterm were not used; the backend is a thin seam if that
  changes.
- Tests: 42 cyclops-ui unit tests (classification, filters, eye, ring,
  selection, exact frame strings at fixed sizes, badge-voice parity with
  the CLI), the 10k fluidity measurement, 2 focus helper integration tests
  on an isolated tmux server, and 5 headless end-to-end tests driving
  `cyclops ui --plain` against a canned daemon over a scratch socket with
  a fixture ledger (calm admin stream, firehose, filter, dedupe, honest
  endings).
- Docs: docs/ui.md; README ui row and crate row; ARCHITECTURE crate map
  and zero-polling notes updated to the shipped M3 client.

### Added (M3: theme engine)

- crates/cyclops-theme: every color is a semantic token (role.1-8,
  surface.dim, surface.accent, eye.calm, eye.alert, five state.* and four
  badge.*, plus surface.fg as the engine's fallback for an
  out-of-vocabulary name). Themes are data-only
  TOML: values are "#rrggbb" or { hex, c256 }; an omitted 256-color
  fallback is derived (nearest cube-or-ramp xterm entry, documented and
  tested), unknown tokens warn, missing tokens fall back to a compiled
  default table (the pre-M3 CLI palette), only broken TOML rejects a file.
- The vocabulary is exactly what the renderers paint, and that now
  includes state and badge color. GOALS says color must never be the only
  encoding, which requires it to be REDUNDANT with the glyph and the word,
  not absent. M3 first read that as "states are never colored" and dropped
  the tokens; that reading was wrong and is reversed here. States and
  badges resolve five state.* and four badge.* tokens grouped by what a
  reader needs to tell apart, not one hue per state: healthy (working,
  delivered), needs-you (blocked_modal, blocked_permission, attention),
  terminal (blocked_quota, parked, the states that never retry
  themselves), quiet (idle, queued, unknown) and a dimmer dead. Role hues
  stay on the agent name alone, so the two encodings never share a cell.
  Color stays redundant and is measured that way: under NO_COLOR, --plain
  or Theme::none every state still carries its glyph and its word and
  renders byte-identically. The CLI and the stream paint from the same
  tokens through the same code, so the two surfaces cannot drift.
  stream.* (3) and surface.bg stayed dropped: nothing paints a ground, and
  the stream's gutter resolves surface.dim like every other detail column.
  Naming a dropped token warns and is skipped.
- themes/: dark (the shipped default; maps the usecyclops.dev terminal
  identity, sage and mauve leading a muted eight-slot role wheel), light
  (the site's light page palette at ink strength), high-contrast (white
  and saturated grid-exact hues on the terminal's black; every value
  clears WCAG AA against it, the dimmest at 7.5:1). Each file header
  documents every mapping choice and why the absent groups are absent.
- Selection: `theme = "name"` in config.toml, `CYCLOPS_THEME` env wins
  over it; both accept a name in the themes dir (`~/.cyclops/themes`,
  falling back to `./themes`) or a direct .toml path. Hot reload for
  long-lived renderers is ThemeWatch: a (mtime, length) stat when an
  event already woke the renderer, no watcher thread, no timer; edits to
  the active theme apply on the next render.
- cyclops/src/style.rs resolves through the theme engine; its public
  surface (detect, none, role, accent, dim, bold, role_color) is
  unchanged and every CLI render test passes untouched. Role labels now
  hash into 8 palette slots instead of 6, so agents may land on different
  colors than before (slot count is part of visual stability going
  forward). cyclopsd recognizes the `theme` config key so a themed
  config file does not warn.
- Tests: 21 cyclops-theme unit tests (vocabulary and default table agree
  and every token resolves to its own default, every documented token is
  one a renderer paints, dropped tokens warn when named, 256-color
  derivation, parse
  tolerance, selection precedence, hot reload) plus 5 shipped-file tests
  (the three themes load with zero warnings and cover every token, role
  fallbacks stay pairwise distinct, non-role fallbacks match the
  documented derivation, high-contrast is grid-exact throughout, and
  docs/themes.md's token table is pinned to the vocabulary).
- Docs: docs/themes.md; install.md theme key; ARCHITECTURE.md crate map.

### Added (M3: integration)

- demos/m3-stream.sh: the M3 surface live in one isolated rig: three
  fixture panes (implementer, reviewer, builder), two `cyclops ui
  --plain` followers capturing the admin stream and the firehose while
  the panes generate an agent-to-agent review request, a title-driven
  blocked_permission and its clear (the eye opening and closing as word
  lines), and a message to admin that lands in both views with its
  honest attention_required delivery and admin ping. A late viewer then
  backfills from the ledger tail with --with filtering, and stopping the
  daemon proves the connection-loss copy and exit 1. Twelve checks pin
  the contract in the captured logs; the full-screen TUI is the manual
  half, printed as a command to try in a real terminal.

### Added (M2: messaging read side, history + thread)

- Daemon msg.history: filter the message record (with = from-or-to,
  from/to one direction each, limit, cursor), newest last, returning
  {lines, next_cursor}. Lines are the ledger's msg/fyi facts with their
  delivery chains folded in at read time: one msg fact, N current badges;
  the files are never rewritten. Cross-session broadcasts dedupe to one
  fact with each hosting file's chain. Reading is free (any same-uid
  caller may query the whole record) and reading never writes; the name
  "me" in any filter resolves through the caller's identity envelope with
  the same fail-closed peer-credential walk msg.send uses. Reader is
  cyclops-ledger's existing read_after full scan; no indexed reader was
  added (a 10k-line ledger parses in single-digit ms, no measured need).
- Daemon msg.thread: id -> the folded msg line, every state/gate line
  sharing the id (cross-file duplicates collapse), and every msg whose
  reply_to chains to it, transitively, ordered oldest first. Unknown ids
  answer no_such_message, not an empty page.
- cyclops history [--with X | --from X --to Y | --to me] [--limit N]
  [--cursor S] and cyclops thread <id>: strict-grid rendering with a
  timestamp gutter (relative under 24h, UTC date beyond), role-colored
  from -> to, a distinct fyi column, and per-delivery badges in the M1
  receipt voice (broadcasts hang N badges under one fact line; thread
  adds bodies). --json passes the raw folded lines through; empty states
  invite the next send.
- Tests: 12 daemon unit tests over a checked-in fixture ledger covering
  every line kind (tests/fixtures/history.ndjson), 6 CLI e2e tests
  against the canned daemon, and an integration test (m2_history.rs)
  where two fixture panes exchange real messages through the daemon and
  history --with reconstructs the conversation, including the
  one-fact-N-badges broadcast read, me-resolution over the socket,
  gapless cursor walk, thread chain order, and a reboot replay.
- Docs: docs/history.md; README history/thread rows.

### Added (M2: agent.wait, server-owned, plus send-and-wait)

- agent.wait rebuilt as a server-owned wait with occupant pinning:
  (pane_id, pane_pid) recorded at wait start; the pane vanishing, dying,
  or changing root pid resolves a wire error occupant_changed instead of a
  false success. Timeout is now a wire error too (code timeout), and both
  errors carry {state, waited_ms, target, until} in the new optional
  WireError.data field (additive; old clients ignore it). done tightened
  to the working -> idle edge: the current or next turn ending satisfies
  it; a blocked state mid-turn keeps waiting instead of passing as done.
  Waits are event-driven off the fusion broadcast plus the watcher stream;
  the deadline is the only timer.
- msg.send send-and-wait entries now carry {outcome, state, waited_ms,
  delivery} per recipient (outcome: reached | timeout | occupant_changed |
  not_delivered), replacing the boolean timed_out shape.
- cyclops wait <target> --until idle|done|blocked [--timeout 60s]: human
  durations (90s, 2m, 1m30s, 500ms; max 10m), badge output on reached,
  exit 0 reached / 2 timeout / 3 occupant changed. cyclops send gained
  --wait idle|done|blocked with --timeout passthrough and a wait line
  under the receipt.
- F23 (findings.md): tmux evaluates format subscriptions on a 1Hz tick;
  a title state that appears and disappears within the same second never
  produces %subscription-changed, so the title sensor's resolution is one
  second. m2_wait fixtures hold driven states across the tick.
- Tests: 6 fixture-pane integration tests (m2_wait.rs) covering each until
  mode including both done edges, timeout data, kill-pane occupant
  pinning, and a send --wait done round trip; 6 CLI e2e tests for badges,
  copy, exit codes, --json error objects, and the --wait passthrough.
- Docs: docs/wait.md; send.md --wait section; README wait row.

### Added (M2: hooks install + startup self-test, amendment c)

- Hook config templates under hooks/<cli>/ with the measured vendor
  schemas: claude settings fragment (UserPromptSubmit, Stop, Notification,
  PermissionRequest), codex hooks.json (PascalCase; no Notification event
  exists on codex), agy .agents/hooks.json (named-hooks schema, every
  event registered with a distinct self-tagging command, F7). Templates
  carry {label}/{cyclops_bin} placeholders and comment headers naming the
  trust caveats; comments are stripped at render.
- cyclops hooks install <cli> --agent <label> [--dry-run] [--dest <dir>]:
  renders to $CYCLOPS_HOME/hooks/<label>/ and prints copy-pasteable wiring
  instructions (claude --settings path; codex CODEX_HOME copy or the
  config.toml trust seed line, printed not applied, F1; agy .agents
  placement). Refuses vendor dot-dirs (.claude/.codex/.gemini/.agents)
  even via --dest.
- Daemon hook liveness (per adopted pane whose manifest declares hooks):
  every agent.state.report records a per-event last-seen edge. PaneStatus
  gained additive optional hooks_verified (skip-serialized None; old
  daemons omit it, old clients ignore it); cyclops status renders
  "hooks unverified". New socket verbs hooks.verify (tier plus last-seen
  edge ages) and hooks.selftest (one fyi marker through the normal
  delivery pipeline, subject "[cyclops] hook self-test", reporting whether
  the ack hook fired with the marker; costs one trivial turn; result is a
  ledger system line).
- F1 downgrade visibility: the first delivery that times out its tier-1
  ack window on a pane with zero hook edges ever seen emits one
  admin.notify action_required naming the likely cause (codex directory
  trust); the delivery itself resolves on screen evidence as before.
- Tests: template golden files, install e2e (dry-run, default dest, vendor
  dot-dir refusal, json mode), selftest integration with a simulated hook
  edge, and the F1 regression shape (zero-edge tier-1 pane downgrades
  cleanly, notifies once, loses nothing).

### Added (M2: commPact v1 cutover prep; prepared, never installed)

- scripts/commpact-shim/commPact: the v1 calling surface served by
  cyclops. send/read/list/resolve/doctor forward to the cyclops CLI,
  id/hash/version stay local with v1 behavior, verbs with no v2
  equivalent (type, keys, message, name) refuse honestly with exit 2,
  and a one-line deprecation note prints to stderr once per day per user
  via a stamp under $CYCLOPS_HOME.
- scripts/commpact-shim/install.sh: the guarded installer only the admin
  runs: refuses without CYCLOPS_CUTOVER_ACK=yes, moves the v1 binary to
  commPact.v1.bak (the backup IS the original), symlinks the shim, prints
  rollback, refuses on existing backups or foreign symlinks. Nothing in
  the repo executes it.
- docs/CUTOVER.md: the runbook: verb map, honest differences,
  preconditions, admin-only install steps, parallel window, verification
  checklist over the COORDINATION.md messaging patterns, and rollback;
  ends in ADMIN_ACTION_REQUIRED.
- scripts/commpact-shim/test_shim.py: 42 checks running the shim against
  a canned daemon on a sandbox socket, asserting verb mapping, refusals
  never reaching the daemon, stamp behavior, installer guards, and that
  the real ~/.commPact stays untouched. Python, outside cargo test; run
  python3 scripts/commpact-shim/test_shim.py.

### Added (M2: integration)

- demos/m2-conversation.sh: the whole M2 surface in one isolated rig: two
  fixture panes acting like hook-wired CLIs (acks travel through the real
  cyclops hook receiver), a send whose identity resolves from the sending
  pane, a --reply-to reply, a broadcast fyi, history --with and thread
  reconstructing the conversation, wait --until idle, hooks verify, hooks
  selftest, and jq over the session ledger.

### Fixed (M2)

- pane.read resolved strict pane ids only: cyclops read <label> answered
  no_such_target while the CLI promised "label or pane id", and the v1
  shim maps commPact read <label> onto exactly that call. The resolver
  now goes through the adoption registry first, like every other verb.

### Added (M1: delivery pipeline)

- cyclopsd delivery core per docs/DELIVERY.md: per-recipient FIFO workers,
  spec-order gate (no_such_pane, pane_dead, pane_in_mode, quota park-all,
  manifest modal decline or hold+notify, working/idle_with_input hold, idle
  proceeds with a forced recompute before pasting), unique cyc-<pid>-<seq>
  buffers from a 0700 spool, paste-buffer -p -d, composer verification with
  <message_id> substitution, submit, two ACK tiers (hook payload match with
  dedupe and late upgrade; screen evidence), one bounded retry, then
  attention_required plus admin notify. blocked_quota parks and never
  auto-retries.
- Ledger wired in: cyclops-ledger adopted into the workspace; one ledger per
  watched session at $CYCLOPS_HOME/ledger/<session>.ndjson. Boot, attach and
  detach, pane labeling, and admin notifications are system lines; every
  fused state change and delivery transition is a state line; gate decisions
  carry rule ids and causes only, never screen text.
- Fail-closed sender identity: socket peer (uid, pid) via LOCAL_PEERCRED or
  SO_PEERCRED, pid-ancestry walk to a watched pane_pid (labeled pane, pane
  id, or admin); nothing in a request body overrides it. cyclops-tmux pane
  rows gained pane_pid.
- New socket verbs: msg.send (receipts block up to receipt_block_ms on the
  idle path, immediate queued/parked otherwise; broadcast is one msg line
  with N delivery records), admin.notify, agent.wait, pane.label (adoption
  registry), agent.state.report (AckMatcher; unmatched reports feed fusion
  as the hook sensor).
- cyclops send: positional target merged with --to, --all, --fyi,
  --reply-to, --body/--body-file (- reads stdin); badge receipts, broadcast
  grid, exit 1 on parked/attention, 2 on usage errors. cyclops hook: silent
  exit-0 receiver posting agent.state.report with flock-serialized per-agent
  seq; failures log to $CYCLOPS_HOME/hook-errors.log.
- Config: ack_timeout_ms (1500), delivery_retry_max (1), receipt_block_ms
  (2500); unknown keys still warn, never fail.
- demos/m1-send.sh: isolated end-to-end send demo (two labeled cat panes,
  single delivery, broadcast, jq over the session ledger).
- Tests: 43 cyclopsd unit plus 9 delivery scenarios on isolated tmux -u -L
  servers validating full-ledger legality; identity unit and integration
  tests; 16 cyclops e2e covering send receipts, exit codes, and the hook
  budget.

### Fixed (M1)

- Codex idle_with_input discrimination was data-only: the manifest's
  line_regex_esc rules (typed text is bare, ghost suggestions are SGR-dim,
  F19) could never fire because nothing supplied an escaped capture, so
  typed human text read as idle and was safe to paste over. cyclops-tmux
  gained ControlClient::capture_pane_escaped (capture-pane -e), and fusion
  recompute (which the gate's fresh pre-paste evaluation runs through)
  now takes both captures whenever the bound manifest carries esc rules.
  A failed escaped capture is doubt, same as a failed plain capture,
  never an idle-biased fallback.
- No pane-rebind re-check existed between the gate's admitting recompute
  and paste/submit: a pane whose occupant changed after admit (agent
  exited to a shell, another CLI took over) got pasted into and
  Enter-submitted, and a shell occupant would EXECUTE the message text.
  The inject path now re-reads the pane immediately before the paste and
  again immediately before the submit key, requiring the pane to exist,
  be alive, keep its admitted pane_pid, and bind the admitted manifest;
  any mismatch goes to retry_queued (cause: pane_rebound) with a gate
  ledger line and the submit key is never sent (DELIVERY.md v1.1
  amendment 3).
- Deadline expiry could stand on an evidence pass that never looked: when
  the watcher was already cleared (a detach removes it before the
  lifecycle event is broadcast) or the capture failed, the tier-2 pass
  silently reported "no evidence" and an expired ACK clock returned
  Timeout, burning the attempt. Unobservable passes now freeze the
  AckClock (doubt, mirroring fusion's capture-failure handling); a
  session edge, pane activity, or a lag reconcile unfreezes it.
- A lone exact repost of an out-of-order older hook seq wiped the dedupe
  window (any replayed below-max seq read as a counter reset). Only a
  small replayed seq (<= 8, the hook restarts at 1) or three consecutive
  below-max replays read as a reset now; anything else is a duplicate.
  The (session_id, turn_id, event) dedupe stays as the backstop.
- send-and-wait omitted pane-less recipients from the wait array while
  DELIVERY.md says every recipient reports. They now get a wait entry
  carrying the resolved delivery state (attention_required) and a null
  agent state.
- Restart-limbo closure only seeded chains from msg lines via the hosted
  field, so ledgers written before that field existed (old single-file
  daemons) never closed a delivery that died before its first state line.
  A msg line with no hosted list now hosts every recipient it names.
- tests/harness/tuikit.py ran tmux without -u and without -f /dev/null
  (F14 discipline: a harness server could load the user's tmux config and
  sanitize control replies), and its dismiss_modal sent 2 to the codex
  update dialog whose measured decline is 3 (Skip until next version,
  F3). Both ported from tests/m1_soak.py; test_vocab.py locks the codex
  decline.
- Detach-blind ACKs (the soak's duplicate delivery): ACK deadlines now
  freeze while a session's control connection is down and extend by the
  outage duration on reattach; reattach runs an evidence pass before any
  deadline can expire, so a delivery that landed during the outage
  resolves instead of being resubmitted; and agent.state.report resolves
  against the session's last-known pane table while detached, so hook
  ACKs no longer bounce with session_detached.
- send-and-wait ordering: the wait now starts only after the delivery
  reaches a resolved state, and until=done counts only working phases
  observed at or after this delivery's submit. Wait entries carry the
  resolved delivery state; a non-delivered resolution reports it instead
  of a fabricated wait result.
- Post-paste verification could pass on stale screen text: a generic
  verify pattern ("Pasted text") anywhere in the 15-line window, even
  from a PREVIOUS message. Generic patterns now count only on a manifest
  composer line; the substituted message id still counts anywhere.
- Tier-2 screen ACK accepted a changed composer window alone as delivery
  evidence. Per DELIVERY.md v1.1: a changed window counts only when
  verification demonstrably staged the id pattern; otherwise working or
  output evidence is required.
- Restart limbo: deliveries left in flight by a daemon stop are closed at
  the next boot as attention_required (cause: daemon_restart) with one
  aggregated admin notification. msg lines now carry a `hosted` recipient
  list so cross-session chains close only where they are hosted.
- Manifest binding silently failed on native installs whose
  pane_current_command is a bare version string (F21): binding now falls
  back to the argv[0] basename of pane_pid (ps, cached per pane+pid)
  matched against process_names plus agent.argv_basenames.
- Modal decline TOCTOU: multi-key declines re-capture the screen before
  the final confirming key and abort back to the gate loop (gate line
  decline_aborted) when the same rule no longer matches, so the confirm
  can never land in whatever replaced the dialog.
- Hook seq counter resets (the hook restarts at 1 after file loss) no
  longer eat the agent's real reports as duplicates: a replayed
  below-max seq clears that agent's dedupe window.
- A stale hook reading can no longer pin fused state: readings age out
  (5 min TTL, checked at recompute time) and are invalidated after three
  consecutive contradicting rules-tier verdicts.
- Deliveries held in gating past gate_hold_notify_ms (new config knob,
  default 120000) ping the admin once so a wedged hold is visible.
- Unresolvable-recipient state lines went to session 0 regardless of the
  sessions carrying the msg line; they now land in every involved session
  file, keeping each per-session ledger a complete stream.
- A loaded tmux buffer lingered server-global (payload included) when
  paste-buffer failed after load-buffer succeeded; it is now deleted best
  effort. cyclopsd also retired its duplicated spool logic for the
  adapter's ControlClient::load_buffer spool path under
  $CYCLOPS_HOME/spool.
- Event subscribers were dropped after ~2.5s of stall at soak rate (1024
  buffer): the event buffer is now 8192 so briefly-stalled clients
  survive; truly wedged clients still lag out and are dropped.
- Amendment i landed: injection is behind the `Injector` trait
  (paste/submit/capture) with the tmux paste path as its first
  implementation, so a headless protocol backend can slot in per agent
  without touching the gate, verification, or ACK layers.
- Delivery state watch used watch::Sender::send, which drops the value when
  no receiver is subscribed; broadcast receipts subscribed late and waited
  out the full receipt cap on already-resolved deliveries. send_replace
  stores unconditionally; broadcast receipts return as soon as every
  delivery resolves.
- tmux control connection dropped under a busy Claude TUI (8x in 80s in the
  m1 soak, blinding both ACK tiers each time): the control reader decoded
  the stream as UTF-8 lines, but pane bytes >= 0x80 ride %output verbatim
  and a split multi-byte character makes single lines invalid UTF-8 (F22).
  The reader now reads byte lines end to end; %output/%extended-output
  data is byte-faithful, reply-block text degrades lossily, and reply
  timeouts stay command-level failures that never tear the connection
  down. Regression: cyclops-tmux tests/control_load.rs holds zero
  Disconnected events through a 60 s braille/title-churn/split-sequence
  soak with concurrent command traffic.
- Control client shutdown could silently skip detach-client on an
  already-closed pipe and then wait a blind 2 s grace for a child that was
  wedged flushing stdout. The detach write is now bounded, and a child
  that never got the detach is killed without the grace wait.
- ControlClient::load_buffer wrote payload files with default permissions
  in the shared system temp dir. Spool files are now exclusive-create
  0600, optionally under a caller-supplied 0o700 spool dir
  (ControlConfig::with_buffer_spool_dir), so cyclopsd can retire its
  duplicated spool logic.

### Added (M0: shadow daemon)

- cyclops-tmux: control-mode client with FIFO reply correlation, pause-after
  flow control at attach, and a zero-polling reconciling pane watcher built
  on refresh-client -B subscriptions (probed on tmux 3.6a). All tmux access
  passes -u after finding F14.
- cyclopsd: read-only shadow daemon: config, sensor fusion over manifest
  rules (title + screen, observable disagreement), NDJSON socket server with
  ping/status/pane.read/events.subscribe, peer-credential capture, clean
  signal shutdown.
- cyclops: status/ping/read/watch with strict-grid rendering, semantic color
  slots with truecolor/256 fallback, NO_COLOR and --plain support.
- cyclops-ledger: crash-safe append-only writer (fsync, torn-tail sealing,
  monotonic seq across restarts) and cursor replay reader.
- Python probe harness ported from the validation campaign; demos/m0-status.sh
  end-to-end demo; docs/ARCHITECTURE.md, docs/DELIVERY.md, docs/GOALS.md.
- Milestone workflow queue (.claude/workflows/m1..m6) with preflight gates.
- findings.md F13-F18 (subscription probe, tmux -u locale sanitization,
  %extended-output switch, %begin flags correlation, bracketed-paste
  conditionality, macOS SO_RCVTIMEO EINVAL).

### Added (scaffold)

- Workspace scaffold: cyclops-proto (protocol v1 + ledger schema),
  cyclops-manifest (detection manifests with modal decline actions),
  cyclops-tmux (version probe), cyclopsd and cyclops binary stubs.
- Shipped detection manifests for Claude Code, Codex CLI, and Antigravity
  CLI, seeded from the 2026-08-01 validation campaign.
- CI: fmt, clippy, tests on ubuntu/macos, advisory tmux-HEAD job.
- docs/GOALS.md: the admin-set quality bar.
