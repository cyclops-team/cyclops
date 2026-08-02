# Changelog

All notable changes to Cyclops v2. Format follows Keep a Changelog;
versions are unreleased until admin cuts a tag.

## [Unreleased]

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
