# Cyclops

**One team. Any coding agent.**

Open source coordination for coding agents running in your terminal. Cyclops
gives each tmux pane an identity, delivers structured messages between agents
with verified receipts, and keeps everything on an append-only record you can
audit months later. If it runs in your terminal, it can run in Cyclops.

Pre-release, and honest about it: [STATUS.md](STATUS.md) says what is built.
Everything below is built and tested.

## Install

Needs tmux 3.2+ (developed on 3.6a) and a Rust toolchain.

```bash
git clone https://github.com/cyclops-team/cyclops.git && cd cyclops
cargo build --release
```

Binaries land in `target/release/`: `cyclopsd` (the daemon) and `cyclops`
(the CLI). Put them on your PATH. More: [docs/install.md](docs/install.md).

Building from source is how you run this implementation. The one-line
installer on usecyclops.dev installs the previous shell implementation,
which is a separate program sharing the name; see [Versions](#versions).

## Start here

One rung at a time. Each is useful on its own, and you can stop at any of
them.

Every block below is real output, captured by
[`demos/parity-check.sh`](demos/parity-check.sh) on a throwaway tmux server.
Three things are edited and nothing else: the home directory is shortened to
`~/.cyclops`, color is off, and a `Next:` block already shown once is left
out the second time. That script re-runs the whole walk and fails if a line
here stops matching what the binaries print.

Some of it follows your machine rather than the run it was captured from:
message ids and clock values, the pane ids tmux hands out (`%1`), the shell
a status row names for a pane running one (`bash` below, `zsh` on plenty of
machines), and the tmux version and loaded-manifest list the daemon
reports. Everything else should match line for line.

### 1. One pane

`cyclops start` opens a workspace and tells you what is left to do.

```
$ cyclops start
✓ workspace ready · 1 agent
  wrote ~/.cyclops/config.toml

Next:
  1  cyclopsd &                                  start the daemon
  2  tmux attach -t main                         open the workspace and start your agents
  3  cyclops send implementer --subject "hello"  send the first message
```

Do them in order. Start the daemon, attach, run your agent CLI in the pane
the way you normally would. Then send it something.

```
$ cyclops send implementer --subject "Review the rate limiter" --body "gateway.rs:120 drops the burst path"
✓ delivered · unverified (screen)
```

The receipt names its evidence. The light check means the message landed
and cyclops confirmed it on the screen, which is what you get before the
agent's own hooks are wired. Wire them once
([hooks.md](docs/hooks.md)) and the same send earns the heavy check,
`✔ delivered · verified`, meaning the recipient's own hook confirmed this
exact message rather than cyclops typing hopefully.

Either way it lands on the record:

```
$ cyclops history
  0s  admin → implementer  Review the rate limiter  ✓ delivered · unverified (screen)
```

The record is a file, so it outlives the daemon. Kill `cyclopsd`, start it
again, and the record and the roster are where you left them:

```
$ cyclops history
  0s  admin → implementer  Review the rate limiter  ✔ delivered · verified

$ cyclops list
  implementer  ○ idle
```

That is the whole product at n=1: one pane, one name, a message you can
prove arrived, and a record that survives.

### 2. Name panes

Split a second pane and start another agent in it. `cyclops status` lists
every pane cyclops watches; the ones with no name yet are listed by pane id.

```
$ cyclops status
‿ cyclops · watching main · tmux 3.6a · up 0s

  implementer  ○ idle  bash · hooks unverified
  %1           ○ idle  bash
```

The closed eye means nothing needs you. `hooks unverified` means no hook has
reported from that pane this run ([docs/hooks.md](docs/hooks.md)). A name is
an address, so give it one:

```
$ cyclops name %1 reviewer
✔ named reviewer · %1

$ cyclops list
  implementer  ● working  Implementing rate limiter
  reviewer     ○ idle
```

Three columns: the name, how the agent is doing, what it is on. A named pane
also says so on its own tmux border, and `cyclops name reviewer --clear`
gives the border back. Details: [docs/panes.md](docs/panes.md).

### 3. Any terminal agent

Cyclops has no SDK and no wrapper. Everything it knows about an agent CLI is
one TOML file: which processes it runs as, how to read working from idle off
the pane, how to type into it. Three ship in [`manifests/`](manifests/):
Claude Code, Codex CLI, Antigravity CLI.

The panes in this walk are none of those. They run a shell script, and
cyclops addresses them because `demos/parity-check.sh` adds a fourth
manifest. That is the promise demonstrated rather than claimed:

```
$ cyclops read reviewer --source detection
reviewer · ○ idle · decided by title_idle

  title  ○ idle  title_idle  just now
```

`decided by title_idle` names the rule in the manifest that produced the
verdict, so a wrong reading is one file to fix and no code to change.

```
$ cyclops name %1 reviewer --manifest cluade
no manifest "cluade"; loaded: agy, claude, codex, demo
```

Writing one: [docs/MANIFESTS.md](docs/MANIFESTS.md).

### 4. Layouts

The shape of a session is a file: panes, sizes, names, working directories.

```
$ cyclops workspace save
✔ workspace saved · main · 2 panes · 2 agents · ~/.cyclops/workspaces/main.toml
```

Lose the session and take it back. `cyclops start` is safe to run twice; the
first run rebuilds the panes, and the second puts the names on once the
daemon has the session again.

```
$ tmux kill-session -t main

$ cyclops start
✓ workspace ready · 2 agents

Next:
  1  tmux attach -t main                         open the workspace and start your agents
  2  cyclops send implementer --subject "hello"  send the first message

$ cyclops start
✔ workspace ready · 2 agents

$ cyclops list
  implementer  ○ idle
  reviewer     ○ idle
```

The light `✓` is `start` declining to claim a roster it could not read; the
heavy `✔` is the daemon confirming it.

Four arrangements ship, each the one before it plus a pane: `solo`, `duo`,
`quad`, `ops`.

```
$ cyclops start --workspace ops --session ops --preset ops
✓ workspace ready · 3 agents
  cyclopsd won't watch "ops" until it's listed in ~/.cyclops/config.toml. Add it to sessions there, then restart cyclopsd.
```

More: [docs/workspaces.md](docs/workspaces.md).

### 5. Structured messages

A message has a subject, a body, a sender, and an id. This is what the
recipient's model actually reads:

```
$ tmux capture-pane -p -t %1
[cyclops m-2f304e] FROM: admin  SUBJECT: Review the rate limiter
gateway.rs:120 drops the burst path
Reply with: cyclops send admin --subject "..." [--body ... | --body-file -]
```

The daemon builds that header from the sender's real identity, resolved from
the process behind the socket. Nothing in a body can forge it.

Replies chain, and a thread reads back whole:

```
$ cyclops thread m-2f304e
  0s  admin → reviewer  Review the rate limiter  ✔ delivered · verified
      gateway.rs:120 drops the burst path
```

A broadcast is one message with one delivery per recipient, each advancing on
its own:

```
$ cyclops send --all --subject "Standup in 5" --fyi
  implementer  ✔ delivered · verified
  reviewer     ● queued · 1 ahead
```

A receipt is the state at the instant you asked. `● queued · 1 ahead` means
that recipient was still finishing the delivery before it; the message goes
in as soon as the pane can take it, in order. The record is where each one
ended up:

```
$ cyclops history
  16s  admin → implementer       Review the rate limiter  ✔ delivered · verified
   5s  admin → reviewer          Review the rate limiter  ✔ delivered · verified
   2s  admin → 2 agents     fyi  Standup in 5
       implementer  ✔ delivered · verified
       reviewer     ✔ delivered · verified
```

Nothing here is ever in limbo: every delivery is in one of ten named states,
and the badges above are live reads of the record, not what was true when the
send returned.

Waiting is a command, not a sleep. `cyclops wait reviewer --until done`
returns when the turn ends; `cyclops send ... --wait done` does both.

```
$ cyclops wait reviewer --until idle
○ idle · waited 0s
```

Errors say what happened and what to do next:

```
$ cyclops send --subject nobody
no recipient. Name one (cyclops send reviewer --subject "..."), or pass --to or --all.
```

More: [docs/send.md](docs/send.md), [docs/history.md](docs/history.md),
[docs/wait.md](docs/wait.md). The two-agent review handoff, start to finish:
[docs/QUICKSTART.md](docs/QUICKSTART.md).

### 6. Pipe output, coming in M6

`cyclops pipe <from> <to>` will take the tail of one agent's pane and deliver
it to another as a normal message. It is not built:

```
$ cyclops pipe implementer reviewer
error: unrecognized subcommand 'pipe'

Usage: cyclops [OPTIONS] <COMMAND>

For more information, try '--help'.
```

`demos/parity-check.sh` asserts that, so this paragraph cannot outlive it.

What scripts can do today is everything the UI does. Every command takes
`--json`:

```
$ cyclops --json history | jq -r '.lines[] | "\(.from) -> \(.to[0])  \(.subject)"'
admin -> implementer  Review the rate limiter
admin -> reviewer  Review the rate limiter
admin -> implementer  Standup in 5
```

And the record underneath is a plain text file, one JSON object per line,
readable with no daemon running at all:

```
$ jq -c 'select(.kind == "msg") | {ts, from, to, subject}' ~/.cyclops/ledger/main.ndjson
{"ts":1785735600663,"from":"admin","to":["implementer"],"subject":"Review the rate limiter"}
{"ts":1785735611792,"from":"admin","to":["reviewer"],"subject":"Review the rate limiter"}
```

The socket the CLI speaks is documented and open:
[docs/PROTOCOL.md](docs/PROTOCOL.md).

## Watch it live

`cyclops ui` turns the terminal into the stream: messages and state changes
as they happen, calm by default, firehose one keypress away, the eye in the
header. [docs/ui.md](docs/ui.md).

Colors are semantic tokens, never raw values in code, so a theme file changes
every surface at once, including the pane borders:

```
$ cyclops theme
▸ dark           ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  high-contrast  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  light          ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉

  cyclops theme <name> to switch
```

Every state pairs a glyph with a word, so `NO_COLOR` and `--plain` lose
nothing. [docs/themes.md](docs/themes.md).

Every demo in [`demos/`](demos/) runs the real binaries against an isolated
tmux server and touches nothing of yours. `demos/m2-conversation.sh` is two
agents exchanging a reviewed message and a reply;
`demos/m4-workspace.sh` builds a session, saves it, destroys it, and brings
it back; `demos/m5-theme.sh` switches themes under a live pane border and
then catches a theme file mid-save; `demos/parity-check.sh` is the walk
above.

## Commands

| Command | What it does |
|---|---|
| `cyclops start` | Open the default workspace: restore it, or build it from a preset. Safe to run twice |
| `cyclops workspace save\|restore` | The shape of a session as a file: panes, sizes, names, directories |
| `cyclops name <pane> <label>` | Name a pane so cyclops can address it; the pane's tmux border says so |
| `cyclops list` | The roster: every named agent, how it is doing, what it is on |
| `cyclops status` | Every watched pane with its fused state, and the eye |
| `cyclops send <agent> --subject ...` | Deliver a message with a verified receipt (`--wait done` blocks until the turn it starts ends) |
| `cyclops wait <agent> --until idle\|done\|blocked` | Block until an agent is ready, finishes a turn, or needs a human |
| `cyclops history --with <agent>` | The message record, newest last, with each delivery's current badge |
| `cyclops thread <id>` | One message plus its replies and delivery record |
| `cyclops hooks install <cli> --agent ...` | Render a vendor hook config plus wiring instructions |
| `cyclops hooks verify\|selftest <agent>` | Hook liveness, and one no-op delivery that proves the ack fires |
| `cyclops ui` | The live stream: calm admin view, firehose one keypress away, jump-to-pane |
| `cyclops theme [name]` | Switch themes, or list them with a preview of each. A switch is live at once |
| `cyclops read <agent> --source detection` | Per-sensor readings behind a state verdict |
| `cyclops watch` | Live event stream |
| `cyclops ping` | Daemon round trip |

All of them take `--json` and `--plain`, and honor `NO_COLOR`. (`ui` has no
`--json`; the machine stream is `cyclops watch --json`.)

## How it works

A Rust daemon (`cyclopsd`) holds one scripted connection to tmux per watched
session, over the interface tmux calls control mode: cyclops asks and tmux
answers, and tmux keeps owning your panes, layout, and attach. A daemon crash
loses nothing. Agent state comes from sensor fusion: vendor hook events, pane
titles, output activity, and screen evidence as a last resort, with per-CLI
detection rules shipped as data in [`manifests/`](manifests/), not code.
Every message and state change lands in a ledger that is only ever appended
to, one JSON object per line, so you can `jq` it.

The crate-by-crate map, the delivery state machine, and the gate order are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Docs

**Going to work on the code? Start at
[HANDOFF.md](docs/HANDOFF.md).** It is the front door for a newcomer: where
everything lives, what to read for the job you have been handed, and which
decisions were deliberate so you do not spend a day undoing one.

Otherwise, one page per question.

| | |
|---|---|
| [HANDOFF.md](docs/HANDOFF.md) | Start here to work on the codebase: the map, and the decisions behind it |
| [install.md](docs/install.md) | Build it, configure it, run the tests |
| [QUICKSTART.md](docs/QUICKSTART.md) | Two agents and a review gate, start to finish |
| [send.md](docs/send.md) | Sending, receipts, broadcast, quota parking |
| [history.md](docs/history.md) | Reading the record, threads, paging |
| [wait.md](docs/wait.md) | Waiting on an agent, exit codes |
| [panes.md](docs/panes.md) | Naming, the roster, the tmux border |
| [workspaces.md](docs/workspaces.md) | Presets, save and restore, `cyclops start` |
| [ui.md](docs/ui.md) | The stream: keys, filters, the eye |
| [themes.md](docs/themes.md) | Semantic color tokens, shipped themes |
| [hooks.md](docs/hooks.md) | Wiring vendor hooks, verifying they fire |
| [MANIFESTS.md](docs/MANIFESTS.md) | Teaching cyclops a new agent CLI |
| [PROTOCOL.md](docs/PROTOCOL.md) | The socket: methods, requests, responses |
| [troubleshooting.md](docs/troubleshooting.md) | When something is wrong |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | How the pieces fit |
| [DELIVERY.md](docs/DELIVERY.md) | The delivery spec: states, evidence tiers, ordering |
| [CONTRIBUTING.md](docs/CONTRIBUTING.md) | The development loop, the demos, and the gates a change must pass |
| [INVARIANTS.md](docs/INVARIANTS.md) | Eleven rules a change must never break, and what breaks otherwise |
| [findings.md](findings.md) | The measurements the design rests on, F13 onward, each with the probe that proved it |
| [GOALS.md](docs/GOALS.md) | The quality bar every milestone is reviewed against |
| [STYLE.md](docs/STYLE.md) | How this codebase is written, binding on every change |

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
