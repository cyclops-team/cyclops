# Cyclops

**One team. Any coding agent.**

Open source coordination for coding agents running in your terminal. Cyclops
gives each tmux pane an identity, delivers structured messages between agents
with verified receipts, and keeps everything on an append-only record you can
audit months later. If it runs in your terminal, it can run in Cyclops.

Pre-release. Today the daemon watches sessions and reads agent state
(milestone M0); message delivery is next. [STATUS.md](STATUS.md) has the
live picture.

## Try it now

Requires tmux 3.2+ and Rust.

```bash
git clone <this repo> && cd clops && cargo build --release
```

See the whole stack run against an isolated tmux server (nothing touches
your own sessions):

```bash
./demos/m0-status.sh
```

Or run it against a real session:

```bash
mkdir -p ~/.cyclops
printf 'sessions = ["main"]\nmanifest_dir = "%s/manifests"\n' "$PWD" > ~/.cyclops/config.toml
./target/release/cyclopsd &
./target/release/cyclops status
```

What works today:

| Command | What it does |
|---|---|
| `cyclops status` | Every watched pane with its fused state (idle, working, blocked) |
| `cyclops ping` | Daemon round trip |
| `cyclops read <pane> --source detection` | Per-sensor readings behind a state verdict |
| `cyclops watch` | Live event stream |

All commands take `--json` (scripts can do anything the UI does) and
`--plain`, and honor `NO_COLOR`.

The messaging surface (`send`, `history`, `wait`, `ui`, `start`) ships
milestone by milestone; [STATUS.md](STATUS.md) tracks what is real.

## How it works

A Rust daemon (`cyclopsd`) holds one tmux control-mode connection per
watched session. tmux keeps owning your panes, layout, and attach; a daemon
crash loses nothing. Agent state comes from sensor fusion: vendor hook
events, pane titles, output activity, and screen evidence as last resort,
with per-CLI detection rules shipped as data in [manifests/](manifests/),
not code. Every message and state change lands in an append-only NDJSON
ledger you can `jq`.

| Crate | What it is |
|---|---|
| `crates/cyclops-proto` | Wire protocol + ledger schema. Data types only. |
| `crates/cyclops-manifest` | Per-CLI detection manifests: schema, loading, rule evaluation. |
| `crates/cyclops-tmux` | The tmux adapter. Every tmux-specific behavior lives here. |
| `crates/cyclopsd` | The daemon: watcher, fusion, socket API, ledger, delivery. |
| `crates/cyclops` | The CLI: thin NDJSON client over the daemon socket. |
| `crates/cyclops-ledger` | Crash-safe append-only ledger writer and reader. |

More: [docs/install.md](docs/install.md), [docs/send.md](docs/send.md),
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/GOALS.md](docs/GOALS.md).

## Principles

- Provider-independent: any terminal agent, no wrappers required.
- Terminal-native: tmux keeps your panes; restart reconciles silently.
- Reliable handoffs: every message ends in a named state; receipts say how
  delivery was verified.
- Progressive, never prescriptive: valuable with one pane; roles are
  optional labels, never requirements.
