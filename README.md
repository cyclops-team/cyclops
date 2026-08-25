<p align="center">
  <img src="assets/cyclops-logo.png" alt="Cyclops" width="128" height="128">
</p>

# Cyclops

**One eye. Many agents. A single coordinated team.**

Cyclops is an open-source coordination layer for coding agents running in
your terminal. Run the agents you already use, watch them from one
workspace, hand work between them through durable mailboxes, and keep it all
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
By default it also merges Cyclops hook entries into installed agent CLIs and
places the Cyclops skill in their supported skill homes. Unrelated vendor
settings are preserved and each vendor file is backed up before its first
edit. Set `CYCLOPS_NO_VENDOR_HOOKS=1` to skip hook and skill wiring. Budget a
few minutes: it is a full release compile. It prints every file it touches,
backs up any shell profile it edits, and never uses sudo.
To install from a clone instead:

```bash
git clone https://github.com/cyclops-team/cyclops.git && cd cyclops
./scripts/install.sh
```

Later, `cyclops update` proves and activates a matched binary pair, and
`cyclops update --rollback` can reactivate a replay-proven retained pair
without reverting state. The
`sh -s -- --uninstall` on the installer takes everything back off.
Details, options, and troubleshooting: [installation guide](docs/guides/install.md).

## Run it

```bash
cyclops
```

One command, from anywhere. It opens the full-screen workspace with your
sessions and agents in a sidebar, tabs, and live panes, starting a tmux
session and the daemon if none is running. Start your coding agents inside
its panes the way you normally would, and talk to them there.

## What using it looks like

You keep talking to your agents in natural language; Cyclops gives them a
shared way to address one another through durable mailboxes:

> Implement the rate limiter change. When you're done, send it to
> reviewer and ask for a review.

The agent runs `cyclops send reviewer ...` from its own pane. The daemon
resolves the sender from the calling process, accepts the message into the
durable workspace mailbox, and queues a content-free wake for the reviewer.
The reviewer claims the exact message before reading its body. You watch the
handoff from the workspace instead of relaying it by hand.

Your agents learn the verbs from one file:
[skills/cyclops/SKILL.md](skills/cyclops/SKILL.md), which the installer
places at the canonical skill destination for every installed supported
consumer. Codex and Cursor share one copy under `~/.agents`; setup never
creates duplicate vendor copies. Scripts use structured command forms with
`--json`; `watch` emits NDJSON, while `update` and `daemon log` remain text.
The commands you will actually type:

| Command | What it does |
|---|---|
| `cyclops` | Open the workspace (starts tmux and the daemon when needed) |
| `cyclops name <pane> <label>` | Name a pane so cyclops can address it |
| `cyclops send <agent> --subject ...` | Accept a durable message and report its wake state |
| `cyclops inbox list`; `cyclops inbox claim <id>` | List pending metadata or claim one exact payload |
| `cyclops reply <message-id>` | Reply using the parent message's route and thread |
| `cyclops messages` | Read body-free mailbox and notification state |
| `cyclops alarm preview --older-than <age>` | Preview exact unresolved alarm ids without changing them |
| `cyclops alarm clear <id>...` | Clear exact alarms; the age-selected form previews and confirms first |
| `cyclops list` | Every named agent, how it is doing, what it is on |
| `cyclops history` | The message record, newest last |
| `cyclops wait <agent> --until idle` | Block on an occupant-pinned pane state; does not prove task completion |
| `cyclops watch` | Live admin, firehose, and Messages views |
| `cyclops update` | Prove and activate a matched pair; restart a running daemon and leave a stopped daemon stopped |
| `cyclops update --rollback` | Reactivate a replay-proven retained pair without reverting state |

Every command explains itself with `--help`. Daemon reads and direct
mutations expose structured `--json` forms except `daemon log`. The guarded
`alarm clear --older-than` form is
interactive by design; scripts preview JSON and pass the returned exact ids
to `alarm clear`. The two-agent review handoff, start to finish, is the
[quickstart](docs/guides/QUICKSTART.md).

## How it works

A Rust daemon (`cyclopsd`) holds one scripted connection to tmux per
watched session, over tmux control mode: cyclops asks, tmux answers, and
tmux keeps owning your panes and layout. Agent state comes from sensor
fusion from vendor hook events, pane titles, output activity, and screen evidence,
with per-CLI detection rules shipped as data in
[`resources/manifests/`](resources/manifests/), not code, so any terminal
agent works without an SDK or a wrapper. Mailbox messages and claims use one
append-only journal per durable workspace. Pane state and legacy direct
delivery remain in separate append-only session ledgers.

## Docs

**Going to work on the code? Start at
[HANDOFF.md](docs/development/HANDOFF.md)**, the map and decisions
behind it. Otherwise, one page per question.

| | |
|---|---|
| [QUICKSTART.md](docs/guides/QUICKSTART.md) | Two agents and a review gate, start to finish |
| [install.md](docs/guides/install.md) | Build it, configure it, update it, run the tests |
| [skills/cyclops/SKILL.md](skills/cyclops/SKILL.md) | Teaching your coding agent to use Cyclops itself |
| [send.md](docs/guides/send.md) | Acceptance, claim, reply, notification, and recovery |
| [history.md](docs/guides/history.md) | Reading the record, threads, paging |
| [wait.md](docs/guides/wait.md) | Waiting on an agent, exit codes |
| [panes.md](docs/guides/panes.md) | Naming, the roster, the tmux border |
| [workspaces.md](docs/guides/workspaces.md) | Presets, save and restore, `cyclops start` |
| [workspace-ui.md](docs/guides/workspace-ui.md) | The full-screen workspace (`cyclops`) |
| [ui.md](docs/guides/ui.md) | The stream and Messages TUI (`cyclops watch`) |
| [themes.md](docs/guides/themes.md) | Semantic color tokens, shipped themes |
| [hooks.md](docs/reference/hooks.md) | Wiring vendor hooks, verifying they fire |
| [MANIFESTS.md](docs/reference/MANIFESTS.md) | Teaching cyclops a new agent CLI |
| [PROTOCOL.md](docs/reference/PROTOCOL.md) | The socket: methods, requests, responses |
| [BENCHMARKS.md](docs/reference/BENCHMARKS.md) | Latency, throughput, and render cost, with sources |
| [troubleshooting.md](docs/guides/troubleshooting.md) | When something is wrong |
| [HANDOFF.md](docs/development/HANDOFF.md) | Start here to work on the codebase |
| [AGENTS.md](AGENTS.md) | The same front door for AI coding agents |
| [ARCHITECTURE.md](docs/development/ARCHITECTURE.md) | How the pieces fit |
| [DELIVERY.md](docs/development/DELIVERY.md) | Legacy direct-delivery compatibility design |
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
