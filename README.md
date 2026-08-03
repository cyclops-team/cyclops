# Cyclops

**One team. Any coding agent.**

Open source coordination for coding agents running in your terminal. Cyclops
gives each tmux pane an identity, delivers structured messages between agents
with verified receipts, and keeps everything on an append-only record you can
audit months later. If it runs in your terminal, it can run in Cyclops.

Pre-release. Today the daemon watches sessions, reads agent state,
delivers messages with verified receipts, reconstructs the record on
demand, and streams it live (milestones M0 through M3), and `cyclops
start` opens a saved workspace. [STATUS.md](STATUS.md) has the live
picture.

## Try it now

Requires tmux 3.2+ and Rust.

```bash
git clone https://github.com/cyclops-team/cyclops.git && cd cyclops
cargo build --release
```

Building from source is how you run this implementation. The one-line
installer advertised on usecyclops.dev installs the previous shell
implementation, which is a separate program that shares the name; see
[Versions](#versions).

See the whole stack run against an isolated tmux server (nothing touches
your own sessions): two panes exchange a reviewed message and a reply,
history reconstructs the thread, and the hook self-test proves the ack
round trip.

```bash
./demos/m2-conversation.sh
```

`demos/` also keeps the m0 (status) and m1 (send) walkthroughs,
`demos/m3-stream.sh` shows the stream UI following the same rig in
plain mode (the full-screen `cyclops ui` is worth a real terminal), and
`demos/m4-name.sh` names three panes, prints the roster, and shows the
border a named pane wears and how `--clear` gives tmux back.
`demos/m4-workspace.sh` is the whole of M4 in one run: it builds the `duo`
arrangement, names both panes, saves the workspace, kills the session
outright, brings the panes and the roster back, and then diffs what came
back against what was there.

Or run it for real. `start` builds the workspace and writes
`~/.cyclops/config.toml` on a first run, then tells you what is left:

```bash
./target/release/cyclops start   # ✔ workspace ready · 1 agent
./target/release/cyclopsd &      # from the repo root, so it finds manifests/
./target/release/cyclops status
```

`cyclops start --preset ops` gives you three agents and the stream docked
under them. [docs/workspaces.md](docs/workspaces.md) has the rest.

What works today:

| Command | What it does |
|---|---|
| `cyclops start` | Open the default workspace: restore it, or build it from a preset. Safe to run twice |
| `cyclops workspace save\|restore` | The shape of a session as a file: panes, sizes, names, directories |
| `cyclops name <pane> <label>` | Name a pane so cyclops can address it; the pane's tmux border says so |
| `cyclops list` | The roster: every named agent, how it is doing, what it is on |
| `cyclops status` | Every watched pane with its fused state (idle, working, blocked) |
| `cyclops send <agent> --subject ...` | Deliver a message with a verified receipt (`--wait done` blocks until the turn it starts ends) |
| `cyclops wait <agent> --until idle\|done\|blocked` | Block until an agent is ready, finishes a turn, or needs a human |
| `cyclops history --with <agent>` | The message record, newest last, with each delivery's current badge |
| `cyclops thread <id>` | One message plus its replies and delivery record |
| `cyclops hooks install <cli> --agent ...` | Render a vendor hook config plus wiring instructions |
| `cyclops hooks verify <agent>` | Hook liveness: tier and last-seen edge ages |
| `cyclops hooks selftest <agent>` | Prove the ack hook fires, via one no-op delivery |
| `cyclops ui` | The live stream: calm admin view, firehose one keypress away, the eye, jump-to-pane |
| `cyclops ping` | Daemon round trip |
| `cyclops read <agent> --source detection` | Per-sensor readings behind a state verdict |
| `cyclops watch` | Live event stream |

All commands take `--json` (scripts can do anything the UI does) and
`--plain`, and honor `NO_COLOR`. (`ui` has no `--json`; the machine
stream is `cyclops watch --json`.)

The surface ships milestone by milestone; [STATUS.md](STATUS.md) tracks
what is real.

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
| `crates/cyclops-ui` | The stream UI behind `cyclops ui`: admin view, firehose, the eye. |
| `crates/cyclops-theme` | Semantic color tokens and the shipped themes. Data, not code. |
| `crates/cyclops-ledger` | Crash-safe append-only ledger writer and reader. |

More: [docs/install.md](docs/install.md), [docs/send.md](docs/send.md),
[docs/history.md](docs/history.md),
[docs/wait.md](docs/wait.md), [docs/hooks.md](docs/hooks.md),
[docs/ui.md](docs/ui.md), [docs/panes.md](docs/panes.md),
[docs/workspaces.md](docs/workspaces.md),
[docs/themes.md](docs/themes.md),
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/GOALS.md](docs/GOALS.md),
[docs/STYLE.md](docs/STYLE.md) (how this codebase is written, binding on
every change).

## Principles

- Provider-independent: any terminal agent, no wrappers required.
- Terminal-native: tmux keeps your panes; restart reconciles silently.
- Reliable handoffs: every message ends in a named state; receipts say how
  delivery was verified.
- Progressive, never prescriptive: valuable with one pane; roles are
  optional labels, never requirements.

## Versions

Cyclops has been rewritten. This tree is the Rust implementation: a
daemon on tmux control mode, an append-only ledger, verified receipts.

The previous implementation was a shell and Python toolkit built around
`bin/commPact`. It still exists and still works: branch
[`v1`](https://github.com/cyclops-team/cyclops/tree/v1), tag `v1-final`.
It is a read-only reference now, and it is what the usecyclops.dev
one-line installer currently fetches. Nothing here migrates your v1 state
automatically; [docs/CUTOVER.md](docs/CUTOVER.md) is the runbook when you
choose to move.

## License

MIT, see [LICENSE](LICENSE). Upstream attribution for the v1 lineage is
in [NOTICE](NOTICE).
