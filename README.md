<p align="center">
  <img src="assets/cyclops-logo.png" alt="Cyclops" width="128" height="128">
</p>

# Cyclops

**One eye. Many agents. A single coordinated team.**

Cyclops is an open-source coordination layer for coding agents running in
your terminal. Run the agents you already use, watch them from one
workspace, hand work between them with verified delivery, and keep it all
on an append-only record you can audit later. If it runs in your terminal,
it can run in Cyclops.

[usecyclops.dev](https://www.usecyclops.dev) · [quickstart](docs/guides/QUICKSTART.md) · [the docs, one page per question](#docs)

Pre-release, and honest about it: [STATUS.md](STATUS.md) says what is built.

## Install

Needs tmux 3.2+, curl, and Git. Cyclops builds from source; if Rust is
missing, the installer installs it with [rustup](https://rustup.rs) and
continues (`CYCLOPS_NO_RUSTUP=1` declines).

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh
```

That builds both binaries, puts them on your PATH, and writes the config.
Budget a few minutes: it is a full release compile. It prints every file
it touches, backs up any shell profile it edits, and never uses sudo.
To install from a clone instead:

```bash
git clone https://github.com/cyclops-team/cyclops.git && cd cyclops
./scripts/install.sh
```

Later, `cyclops update` rebuilds from the latest source in place, and
`sh -s -- --uninstall` on the installer takes everything back off.
Details, options, and troubleshooting: [installation guide](docs/guides/install.md).

## Run it

```bash
cyclops
```

One command, from anywhere. It opens the full-screen workspace — your
sessions and agents in a sidebar, tabs, live panes — starting a tmux
session and the daemon if none is running. Start your coding agents inside
its panes the way you normally would, and talk to them there.

## What using it looks like

You keep talking to your agents in natural language; Cyclops gives them a
shared way to address one another and prove delivery:

> Implement the rate limiter change. When you're done, send it to
> reviewer and ask for a review.

The agent runs the handoff itself — `cyclops send reviewer …` from its own
pane. The daemon resolves the sender from the pane the message really came
from, delivers it with an evidence-labeled receipt, and appends it to a
plain-text record you can read with `jq`. You watch it happen from the
workspace instead of relaying messages by hand.

Your agents learn the verbs from one file —
[skills/cyclops/SKILL.md](skills/cyclops/SKILL.md), which the installer
places for Claude Code automatically — and scripts use the same CLI with
`--json`. The commands you will actually type:

| Command | What it does |
|---|---|
| `cyclops` | Open the workspace (starts tmux and the daemon when needed) |
| `cyclops name <pane> <label>` | Name a pane so cyclops can address it |
| `cyclops send <agent> --subject ...` | Deliver a message with a receipt naming its evidence |
| `cyclops list` | Every named agent, how it is doing, what it is on |
| `cyclops history` | The message record, newest last |
| `cyclops wait <agent> --until idle` | Block until an agent is ready, done, or needs a human |
| `cyclops watch` | The live event stream |
| `cyclops update` | Rebuild from the latest source; config and record untouched |

Every command explains itself with `--help` and takes `--json`. The
two-agent review handoff, start to finish, is the
[quickstart](docs/guides/QUICKSTART.md).

## How it works

A Rust daemon (`cyclopsd`) holds one scripted connection to tmux per
watched session, over tmux control mode: cyclops asks, tmux answers, and
tmux keeps owning your panes and layout. Agent state comes from sensor
fusion — vendor hook events, pane titles, output activity, screen evidence —
with per-CLI detection rules shipped as data in
[`resources/manifests/`](resources/manifests/), not code, so any terminal
agent works without an SDK or a wrapper. Every message and state change
lands in an append-only ledger, one JSON object per line.

## Docs

**Going to work on the code? Start at
[HANDOFF.md](docs/development/HANDOFF.md)** — the map, and the decisions
behind it. Otherwise, one page per question.

| | |
|---|---|
| [QUICKSTART.md](docs/guides/QUICKSTART.md) | Two agents and a review gate, start to finish |
| [install.md](docs/guides/install.md) | Build it, configure it, update it, run the tests |
| [skills/cyclops/SKILL.md](skills/cyclops/SKILL.md) | Teaching your coding agent to use Cyclops itself |
| [send.md](docs/guides/send.md) | Sending, receipts, broadcast, quota parking |
| [history.md](docs/guides/history.md) | Reading the record, threads, paging |
| [wait.md](docs/guides/wait.md) | Waiting on an agent, exit codes |
| [panes.md](docs/guides/panes.md) | Naming, the roster, the tmux border |
| [workspaces.md](docs/guides/workspaces.md) | Presets, save and restore, `cyclops start` |
| [workspace-ui.md](docs/guides/workspace-ui.md) | The full-screen workspace (`cyclops`) |
| [ui.md](docs/guides/ui.md) | The stream TUI (`cyclops watch`) |
| [themes.md](docs/guides/themes.md) | Semantic color tokens, shipped themes |
| [hooks.md](docs/reference/hooks.md) | Wiring vendor hooks, verifying they fire |
| [MANIFESTS.md](docs/reference/MANIFESTS.md) | Teaching cyclops a new agent CLI |
| [PROTOCOL.md](docs/reference/PROTOCOL.md) | The socket: methods, requests, responses |
| [BENCHMARKS.md](docs/reference/BENCHMARKS.md) | Latency, throughput, and render cost, with sources |
| [troubleshooting.md](docs/guides/troubleshooting.md) | When something is wrong |
| [HANDOFF.md](docs/development/HANDOFF.md) | Start here to work on the codebase |
| [AGENTS.md](AGENTS.md) | The same front door for AI coding agents |
| [ARCHITECTURE.md](docs/development/ARCHITECTURE.md) | How the pieces fit |
| [DELIVERY.md](docs/development/DELIVERY.md) | The delivery spec: states, evidence tiers, ordering |
| [INVARIANTS.md](docs/development/INVARIANTS.md) | Rules a change must never break |
| [CONTRIBUTING.md](CONTRIBUTING.md) | The development loop and the gates a change must pass |
| [SECURITY.md](SECURITY.md) | Reporting a vulnerability privately |
| [STATUS.md](STATUS.md) | What is built, milestone by milestone |
| [CHANGELOG.md](CHANGELOG.md) | What each milestone changed |
| [findings.md](findings.md) | The measurements the design rests on |
| [V5.md](docs/development/V5.md) | What the v5 line is for |
| [GOALS.md](docs/development/GOALS.md) | The quality bar every milestone is reviewed against |
| [STYLE.md](docs/development/STYLE.md) | How this codebase is written |
| [CUTOVER.md](docs/development/CUTOVER.md) | Migrating from the v1 shell toolkit |

## Versions

This tree is the Rust implementation. The previous shell-and-Python
toolkit lives on branch
[`v1`](https://github.com/cyclops-team/cyclops/tree/v1) as a read-only
reference; nothing migrates automatically, and
[the cutover runbook](docs/development/CUTOVER.md) covers moving off it.

## License

MIT, see [LICENSE](LICENSE). Upstream attribution for the v1 lineage is
in [NOTICE](NOTICE).
