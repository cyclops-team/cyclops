# Status

Updated 2026-08-02. M0 complete and audited; M1 launched.

## Done

- **M0 shadow daemon, end to end.** `demos/m0-status.sh` runs the whole
  stack: isolated tmux server, cyclopsd attached in control mode, `cyclops
  ping` (0.1ms round trip) and `cyclops status` rendering the branded grid.
- `cyclops-proto`: protocol v1, ledger schema, delivery state machine. 7 tests.
- `cyclops-manifest`: rule engine + shipped claude/codex/agy manifests with
  modal decline actions (amendment g). 7 tests, hazard screens locked.
- `cyclops-tmux`: control client (FIFO reply correlation via %begin flags,
  pause-after at attach per amendment a, %pause auto-resume, octal-safe
  %output), zero-polling SessionWatcher on refresh-client -B subscriptions
  (probed working on 3.6a, F13) with hint-debounced list-panes reconcile,
  typed helpers incl. byte-exact bracketed paste. 29 tests.
- `cyclopsd` M0 scope: config, boot, fusion (title+screen, disagreement
  observable), socket server (hello, ping/status/pane.read/events.subscribe,
  peer credentials), signal shutdown. Integration-tested against a live
  isolated tmux server.
- `cyclops` CLI: status/ping/read/watch, strict-grid rendering, semantic
  style module (truecolor + 256, NO_COLOR, --plain), GOALS-grade error copy.
  26 tests.
- `cyclops-ledger` (early M1): crash-safe appender (fsync per line, torn-tail
  sealing, monotonic seq across restarts), cursor replay. 5 tests. Standalone
  until M1 wires it in.
- Harness ported (`tests/harness/`), CI authored, `docs/ARCHITECTURE.md`,
  `docs/DELIVERY.md` (M1 spec), `docs/GOALS.md` (admin quality bar).
- Milestone queue authored as named workflows in `.claude/workflows/`
  (m1-delivery ... m6-flow), each preflight-gated on a green committed base.
- New findings F13-F18 in `findings.md`. Standouts: F13 (subscriptions work
  on 3.6a, zero-polling is real), F14 (tmux sanitizes control-mode replies
  without `-u` in non-UTF-8 environments, which would have silently
  destroyed the Claude title sensor under launchd; Python probes could never
  have seen it).

## Next

- M1 via the `m1-delivery` workflow: ledger wiring, msg.send end to end,
  ACK tiers, quota parking, modal declines, 100-message mini-soak per
  available CLI as the regression gate.

## Audit note

The M0 cyclopsd implementation agent died mid-run (API error) after its code
was complete but before self-reporting. An independent audit substituted:
verdict complete-to-spec, zero-polling verified across every timer in the
workspace, startup measured at 107ms against the 300ms bar. Its three minor
gaps are fixed: tests now pass tmux -u (F14 discipline), an unexpected
socket state at boot fails loudly instead of reclaiming a possibly-live
daemon's socket, and a capture failure with no prior verdict reports
decided_by "sensor_error" instead of masquerading as a consulted rule set.

## Risks

- CI authored but unexercised (no remote; pushes are admin-only).
- The M1 mini-soak spends real vendor tokens (cheapest models, trivial
  prompts, quota parking honored as a valid outcome).

## Open questions

- License file before anything publishes (admin decision).
- Local-time vs UTC gutter in `cyclops watch` (needs a tz dependency; UTC
  for now, one-line decision later).

## Deviations from the brief

- None in behavior. One bookkeeping fix: docs/ARCHITECTURE.md initially
  carried a derived amendment lettering; corrected to the brief's (a)-(i).
