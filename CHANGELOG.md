# Changelog

All notable changes to Cyclops v2. Format follows Keep a Changelog;
versions are unreleased until admin cuts a tag.

## [Unreleased]

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

- Delivery state watch used watch::Sender::send, which drops the value when
  no receiver is subscribed; broadcast receipts subscribed late and waited
  out the full receipt cap on already-resolved deliveries. send_replace
  stores unconditionally; broadcast receipts return as soon as every
  delivery resolves.

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
