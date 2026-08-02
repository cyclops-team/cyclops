# Architecture

Cyclops v2 is a tmux-backed coordination daemon for terminal coding agents.
The architecture is frozen by ADR-001 (`cyclops-arch/deliverables/`) plus the
validation campaign's amendments (`cyclops-arch/validation-report.md`,
section 8). This document maps those decisions to code. Parts marked M2 or
later do not exist yet; everything else is in the tree today.

## Crate map

| Crate | Role | Status |
|---|---|---|
| `crates/cyclops-proto` | Wire protocol v1, ledger schema, delivery state machine, agent state model. Data types only, no IO. Daemon and every client compile against it. | Done |
| `crates/cyclops-manifest` | Per-CLI detection manifests: TOML schema, compiled rules, region parsing, priority evaluation, modal decline actions. Loads `manifests/*.toml`. | Done |
| `crates/cyclops-tmux` | The tmux adapter. Every tmux-specific behavior lives here: version probe with feature gates, control-mode client (FIFO reply correlation, pause-after flow control), zero-polling reconciling pane table on `refresh-client -B` subscriptions. Pane rows carry `pane_pid` for sender identity. | Done (M1 scope) |
| `crates/cyclopsd` | The daemon: control-mode watcher, sensor fusion (title + screen + hook reports), socket server, per-session ledger writer, delivery pipeline with per-recipient FIFO workers, fail-closed sender identity, pane adoption registry. | Done (M1 scope) |
| `crates/cyclops` | The CLI: thin NDJSON client over the daemon socket. `ping`, `status`, `read`, `watch`, `send`, and the `hook` receiver exist; `history` and a wait verb land in M2. | Done (M1 scope) |
| `crates/cyclops-ledger` | Crash-safe append-only NDJSON ledger writer and cursor reader. Workspace member; `cyclopsd` writes one ledger per watched session through it. | Done |

Non-crate directories: `manifests/` (shipped detection data for claude,
codex, agy, seeded from the campaign), `tests/harness/` (Python probe
harness, the regression seed), `demos/` (runnable end-to-end scripts),
`themes/` (semantic color tokens, consumed from M3), `frontend/` (the
production landing page for usecyclops.dev; read-only branding reference,
outside the Cargo workspace, ignored by Rust CI, never modified without an
explicit admin request).

## Data flow

M0 core (the reconciling read side):

```
tmux control mode (one tmux -C client per watched session)
      |  %output, %pane-title-changed, refresh-client -B subscriptions
      v
change hints ---> reconcile on doubt (list-panes, display-message,
      |           capture-pane; authoritative queries, event-triggered)
      v
pane state table (pane_id keyed; dead, in_mode, title, command, size, pid)
      |
      v
fusion (manifest rules over title + screen; unmatched hook reports join
      |            as the hook sensor via agent.state.report)
      |            per-sensor readings kept, disagreement observable
      v
AgentState per pane ---> socket: status, pane.read, events.subscribe
```

Notifications are hints, not truth. The daemon reconciles derived state
against authoritative queries whenever a hint arrives or a doubt exists.
Missed events degrade freshness, never correctness (ADR revision 1,
level-triggered core).

The M1 delivery pipeline rides the same state, one FIFO worker per
recipient pane:

```
msg.send -> ledger (queued) -> gate (fused idle, pane_dead, pane_in_mode,
no modal) -> load-buffer + paste-buffer -p -d (unique buffer name) -> verify
composer staged it -> Enter -> ACK (per-agent tier: hook payload match, or
screen evidence) -> delivered_verified | delivered_unverified
```

Every transition is a ledger line. Failures retry once, then queue or park;
they never drop and never loop. Receipts on the idle path block up to
`receipt_block_ms` (default 2500); busy targets answer queued with a
position immediately, parked targets answer parked with the reset hint.

## Where each frozen decision lives

| ADR-001 decision | Lives at | Status |
|---|---|---|
| Single daemon, one `tmux -C` client per session (T3 scoping) | `crates/cyclops-tmux/src/control.rs`, owned by `cyclopsd` | Done |
| Level-triggered reconciling core, not an event mirror (revision 1, C2) | `crates/cyclops-tmux/src/watcher.rs` | Done |
| Sensor fusion with per-sensor readings and observable disagreement (revision 2) | Types: `cyclops-proto/src/state.rs` (`Sensor`, `SensorReading`, `Detection`). Engine: `cyclopsd/src/fusion.rs`; hook sensor fed from `cyclopsd/src/ack.rs` | Done |
| Detection rules are per-CLI data, not code (herdr manifest style, H2) | `crates/cyclops-manifest`, `manifests/{claude,codex,agy}.toml` | Done |
| NDJSON Unix socket, hello line first, version mismatch warns never rejects (S2) | `cyclops-proto/src/wire.rs` (`Hello`, `PROTOCOL_VERSION`); server in `cyclopsd/src/server.rs` | Done |
| Append-only NDJSON ledger, monotonic seq plus boot_id, replayable by cursor (C6) | Schema: `cyclops-proto/src/ledger.rs`. Writer: `crates/cyclops-ledger`; `cyclopsd` writes `$CYCLOPS_HOME/ledger/<session>.ndjson` per watched session | Done (cursor replay over the socket lands with the M3 stream client) |
| Delivery pipeline: queue, gate, paste, verify, submit, ACK; failures are queued states | State machine: `cyclops-proto/src/ledger.rs` (`DeliveryState::can_transition_to`). Pipeline: `cyclopsd/src/delivery.rs` | Done |
| Turn detection from hooks via a `cyclops hook` receiver | `wire.rs` (`agent.state.report` params); receiver in `crates/cyclops/src/hook.rs`, matcher and fusion input in `cyclopsd/src/ack.rs` | Done |
| Agent surface: thin CLI speaking NDJSON to the socket | `crates/cyclops` | ping/status/read/watch/send/hook done; history and a wait verb M2 |
| MCP front-door on the same daemon (option D absorbed) | Planned addition, not a dependency | M2+ |
| v1 keepers: fail-closed ACL, data-only config, explicit pane adoption, identity from socket peer | `cyclopsd/src/identity.rs` (peer creds + pid ancestry walk to a watched pane), `pane.label` adoption registry | Done |
| tmux specifics confined to one adapter, version-gated, CI against tmux HEAD | `crates/cyclops-tmux`; advisory tmux-HEAD CI job | Done (probe), ongoing |
| Rollout: shadow mode first, cutover gated on soak | M0 was the shadow daemon; M1 adds the write path (delivery), cutover is admin's call | In progress |

## Validation amendments (a)-(i)

Letters follow the admin's build brief, the authoritative list. The related
change number from `validation-report.md` section 8 is in parentheses. The
report's change 1 (per-agent ACK capability tiers) is frozen decision 6 in
the brief rather than a lettered amendment; it lives in `cyclops-manifest`
`Hooks.ack: Option` (None = screen tier), `ledger.rs` `DeliveredUnverified`
plus `VerifiedBy`, and the manifests (`agy.toml` declares no ack); the
pipeline honors it from M1.

| | Amendment | Lives at | Status |
|---|---|---|---|
| a | `pause-after` set on the control connection at attach (2) | `cyclops-tmux/src/control.rs` attach handshake; findings F15 covers the %extended-output consequence | Done |
| b | `bracket_paste_flag` unavailable through tmux 3.6a; post-paste composer verification is the gate (3) | `cyclops-tmux/src/version.rs` `has_bracket_paste_flag`; verification with `<message_id>` substitution in `cyclopsd/src/delivery.rs` | Done |
| c | Daemon startup self-test proving hooks actually fire, F1: Codex loads zero hooks in untrusted dirs, silently (4) | `cyclopsd` hook liveness tracking, result logged as a `system` ledger line | M2 |
| d | Dedupe hook events on (session_id, turn_id, event), F2: Codex double-fires across config layers (5) | `cyclopsd/src/ack.rs` (plus reporter seq) | Done |
| e | Unique tmux buffer name per delivery, F4: named buffers are global, concurrent senders race (6) | `cyclopsd/src/delivery.rs`: `cyc-<pid>-<seq>` buffers loaded from a 0700 spool file, `paste-buffer -p -d` | Done |
| f | Terminal `blocked_quota` state: park and alert, never auto-retry, F11: quota exhaustion passes every liveness check (9) | `state.rs` `AgentState::BlockedQuota`, `ledger.rs` `ParkedBlockedQuota` (only exit: operator requeue, M2), parking + urgent notify with reset hint in `cyclopsd/src/delivery.rs` | Done |
| g | Modal vocabulary is per-CLI manifest data with explicit decline options, never generic Enter/Escape, F3, F12 (8) | `cyclops-manifest` `decline_keys` + `auto_dismiss`; `manifests/*.toml` (codex update dialog declines "3" Enter, agy survey "0", trust prompts never auto-dismiss) | Done |
| h | Fusion documented as rare-blocked-state coverage, not steady-state accuracy (7) | `cyclops-proto/src/state.rs` module doc; fusion engine ordering in `cyclopsd` | Done |
| i | Delivery behind a trait so per-agent backends can swap to headless protocol drive without touching layers above | `cyclopsd/src/delivery.rs`: the `Injector` trait (paste / submit / capture) with `TmuxInjector` (load-buffer + paste-buffer + send-keys) as the M1 backend; gate, verify, and ACK layers call through the seam only | Done |

## The zero-polling contract

Idle CPU near zero is a hard goal (GOALS.md). Concretely:

- No interval timers re-querying tmux, panes, or files. State changes arrive
  as control-mode notifications and `refresh-client -B` subscription
  reports; hook reports arrive as socket requests.
- Reconciliation is event-triggered: a notification, a socket request, or a
  delivery gate check may trigger authoritative queries. A clock may not.
- Screen capture (the heuristic sensor) runs only at gate time or when a
  hint puts fused state in doubt, never on a schedule.
- Daemon stalls are in-band, not watchdog-polled: `pause-after` converts
  falling behind into `%pause` / `%continue` notifications (amendment a).
- Clients never poll the daemon: `events.subscribe` pushes. The subscribe
  cursor for post-disconnect replay is accepted but ignored until the M3
  stream client needs it.

Two sanctioned exceptions, neither in the product: the Python probe harness
(`tests/harness/`) polls because it is a measuring instrument, and demo
scripts may wait in a bounded loop for process startup.
