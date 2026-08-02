# Cyclops

**One team. Any coding agent.**

Open source coordination for coding agents running in your terminal. Cyclops
gives each tmux pane an identity, delivers structured messages between agents
with verified receipts, and keeps every message and state change on an
append-only record you can audit months later.

If it runs in your terminal, it can run in Cyclops.

```
$ cyclops start
✓ workspace ready — 3 agents

$ cyclops send reviewer --subject "Review the rate limiter"
✓ delivered · verified

$ cyclops list
implementer  active  rate-limiter
reviewer     active  reviewing
tests        active  investigating
```

## Status

Pre-release, under active development. Milestone M0 (shadow daemon) in
progress. See [STATUS.md](STATUS.md) for what works today and
[docs/GOALS.md](docs/GOALS.md) for the quality bar.

Cyclops v2 replaces commPact v1 (bash). The architecture is fixed by
ADR-001 (tmux-backed Rust daemon, sensor-fusion turn detection, append-only
NDJSON ledger) and a 563-delivery validation campaign at zero unrecovered
loss. Detection behavior for each supported agent CLI ships as data in
[manifests/](manifests/), seeded from that campaign's measured evidence.

## Layout

| Crate | What it is |
|---|---|
| `crates/cyclops-proto` | Wire protocol + ledger schema. Data types only. |
| `crates/cyclops-manifest` | Per-CLI detection manifests: schema, loading, rule evaluation. |
| `crates/cyclops-tmux` | The tmux adapter. Every tmux-specific behavior lives here. |
| `crates/cyclopsd` | The daemon: control-mode watcher, fusion, ledger, delivery. |
| `crates/cyclops` | The CLI: thin NDJSON client over the daemon socket. |

## Principles

- Provider-independent: any terminal agent, no wrappers required.
- Terminal-native: tmux keeps owning your panes; a cyclopsd crash loses
  no panes and no history.
- Reliable handoffs: every message ends in a named state; receipts say
  whether delivery was hook-verified or screen-verified.
- Progressive, never prescriptive: valuable with one pane; roles are
  optional labels, never requirements.
