# Cyclops

**One eye. Many agents. A single coordinated team.**

Cyclops is an open-source coordination layer for coding agents running in
your terminal. Run the agents you already use, watch what each one is doing
from one workspace, hand work between them, verify message delivery, and
keep the whole workflow on an append-only record you can audit months later.
If it runs in your terminal, it can run in Cyclops.

[usecyclops.dev](https://www.usecyclops.dev) · [quickstart](docs/guides/QUICKSTART.md) · [the docs, one page per question](#docs)

Pre-release, and honest about it: [STATUS.md](STATUS.md) says what is built.
Everything below is built and tested.

## Install

Needs tmux 3.2+, curl, Git, and a Rust toolchain. The public installer builds
the current Rust implementation from source:

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh
```

That builds both binaries, puts them where your shell looks, and writes
the config and detection manifests. It prints every file it touches, backs
up any shell profile it edits, and never uses sudo. `--prefix DIR` picks
where the binaries go, `--no-path` leaves your profile alone, and
`--uninstall` takes it all back off. To install a clone instead:

```bash
git clone https://github.com/cyclops-team/cyclops.git && cd cyclops
./scripts/install.sh
```

More: [installation guide](docs/guides/install.md).

Then one command, from anywhere:

```bash
cyclops
```

Bare `cyclops` opens the full-screen workspace: your sessions and agents in
a sidebar, tabs, and live panes. With no tmux session running it starts
one, and it starts the daemon too, so there is no second command and no tab
to keep open. Start the coding agents you already use inside its panes, the
way you normally would, and talk to them there.
[The workspace guide](docs/guides/workspace-ui.md) is the tour.

## Update

```bash
cyclops update
```

It says which build you are running, checks the source for a newer
commit (already-current stops right there), reruns the installer from a
fresh clone, and reports old build to new plus the three restart steps.
Your config, themes, manifests and record are untouched. Details:
[installation guide](docs/guides/install.md).

## Uninstall

A buggy install rarely needs this: `cyclops update` (or the installer
run again) overwrites in place. To actually remove cyclops:

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh -s -- --uninstall
```

That stops the daemon, removes both binaries, and takes the installer's
PATH block back out of your shell profile, backing the file up first.
Quit any open workspace too (`q`); a running one keeps using the deleted
binary until it exits. Your state (agent names, message record, themes,
config) stays at `~/.cyclops` so a later reinstall picks up where you
left off. To leave nothing behind:

```bash
rm -rf ~/.cyclops
```

## Talk to your agents, not to Cyclops

Cyclops gives the agents in your workspace a shared way to identify one
another, exchange structured handoffs, and prove delivery. You keep talking
to your agents the way you already do:

> Implement the rate limiter change. When you're done, send it to reviewer
> and ask for a review.

An agent that knows Cyclops runs the handoff itself — `cyclops send
reviewer …` from its own pane. The reviewer receives a structured message
whose sender the daemon resolved from the pane it really came from, with a
reply hint at the bottom. The delivery lands on the record with its
evidence, and you watch it happen from the workspace instead of relaying
messages by hand.

Three interfaces, one system:

1. **You** run `cyclops`, arrange agents, and speak to them in natural
   language.
2. **Your agents** use the `cyclops` CLI underneath — send, reply, wait,
   check delivery. Teaching an agent the verbs and the safety rules is one
   file: [skills/cyclops/SKILL.md](skills/cyclops/SKILL.md). Nothing
   installs it for you yet: copy it where your agent looks for
   instructions (for Claude Code, `~/.claude/skills/cyclops/SKILL.md`), or
   just tell the agent to use cyclops — every command explains itself with
   `--help`, and every delivered message carries its own reply
   instructions.
3. **Scripts and CI** use the same CLI with `--json`. The ladder below is
   that layer, rung by rung.

You never have to relay a handoff by hand, but the same commands are yours
whenever you want them — that is all the ladder below is.

## The CLI, one rung at a time

Everything above runs on these commands: they are what your agents and
scripts use, and the fastest way to understand the system. Each rung is
useful on its own, and you can stop at any of them.

Every block below is real output, captured by
[`tests/e2e/parity-check.sh`](tests/e2e/parity-check.sh) on a throwaway tmux server.
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

One command. It opens the workspace, starts the daemon if none is
running, names the panes, and tells you what is left to do.

```
$ cyclops start
✔ workspace ready · 1 agent
  started cyclopsd, logging to ~/.cyclops/cyclopsd.log

Next:
  1  tmux attach -t main                         open the workspace and start your agents
  2  cyclops send implementer --subject "hello"  send the first message
```

The heavy check means cyclopsd confirmed the roster: one pane it will
deliver to, not one name in a file.

The daemon it started outlives the shell you typed in, so there is no tab
to keep open. `cyclops daemon status` says whether one is running,
`cyclops daemon stop` takes it down, and `cyclops daemon log` is where it
writes. `cyclops start --no-daemon` leaves that to you.

Attach, and run your agent CLI in the pane the way you normally would.
Then send it something.

```
$ cyclops send implementer --subject "Review the rate limiter" --body "gateway.rs:120 drops the burst path"
✓ delivered · unverified (screen)
```

The receipt names its evidence. The light check means the message landed
and cyclops confirmed it on the screen, which is what you get before the
agent's own hooks are wired. Wire them once
([hooks.md](docs/reference/hooks.md)) and the same send earns the heavy check,
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
watching main · home ~/.cyclops

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
reported from that pane this run ([hooks guide](docs/reference/hooks.md)). A name is
an address, so give it one:

```
$ cyclops name %1 reviewer
✔ named reviewer · %1

$ cyclops list
watching main · home ~/.cyclops

  implementer  ● working  Implementing rate limiter
  reviewer     ○ idle
```

Three columns: the name, how the agent is doing, what it is on. A named pane
also says so on its own tmux border, and `cyclops name reviewer --clear`
gives the border back.

The roster is scoped to where you ask from: inside tmux, when the daemon
watches more than one session, `cyclops list` shows only the session you
are sitting in, with a dim line naming what it left out. `cyclops list
--all` is every watched session. Details: [pane guide](docs/guides/panes.md).

### 3. Any terminal agent

Cyclops has no SDK and no wrapper. Everything it knows about an agent CLI is
one TOML file: which processes it runs as, how to read working from idle off
the pane, how to type into it. Four ship in [`resources/manifests/`](resources/manifests/):
Claude Code, Codex CLI, Antigravity CLI, Cursor Agent.

The panes in this walk are none of those. They run a shell script, and
cyclops addresses them because `tests/e2e/parity-check.sh` adds a fifth
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
no manifest "cluade"; loaded: agy, claude, codex, cursor, demo
```

Writing one: [manifest reference](docs/reference/MANIFESTS.md).

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
watching main · home ~/.cyclops

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

A preset ships names and shapes, never CLIs: which agent belongs in which
pane is yours to say. `--agents` says it, by manifest id, one per named
pane in layout order.

```
cyclops start --preset duo --agents claude,codex
```

That starts them in this run. A workspace cyclops builds from a preset is
written down as it builds it, so the file carries the fleet too; a
workspace you saved yourself is never rewritten behind you. Either way the
next run starts nothing on its own: a command in a file is a suggestion,
and replaying it stays a `--launch` you type each time. An id cyclops has
no manifest for, or more CLIs than the preset has named panes, is refused
before a single pane is built.

The CLIs come up bare. Hook wiring is `cyclops hooks install`, so receipts
from a freshly spun fleet are screen-tier until you wire them.

More: [workspace guide](docs/guides/workspaces.md).

### 5. Structured messages

A message has a subject, a body, a sender, and an id. This is what the
recipient's model actually reads:

```
$ tmux capture-pane -p -t %1
[cyclops m-2f304e] FROM: admin  SUBJECT: Review the rate limiter
gateway.rs:120 drops the burst path
Reply: cyclops send admin --subject "..."
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

More: [send guide](docs/guides/send.md), [history guide](docs/guides/history.md),
and [wait guide](docs/guides/wait.md). The two-agent review handoff, start to
finish: [quickstart](docs/guides/QUICKSTART.md).

### 6. Pipe output is not built

`cyclops pipe <from> <to>` will take the tail of one agent's pane and deliver
it to another as a normal message. It is not built:

```
$ cyclops pipe implementer reviewer
error: unrecognized subcommand 'pipe'

Usage: cyclops [OPTIONS] [COMMAND]

For more information, try '--help'.
```

`tests/e2e/parity-check.sh` asserts that, so this paragraph cannot outlive it.

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
[protocol reference](docs/reference/PROTOCOL.md).

## Watch it live

`cyclops watch` turns the terminal into the stream: messages and state
changes as they happen, calm by default, firehose one keypress away, the
eye in the header. [stream UI guide](docs/guides/ui.md).

Colors are semantic tokens, never raw values in code, so a theme file changes
every surface at once, including the pane borders:

```
$ cyclops theme
  blossom        ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  buttercream    ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  catppuccin     ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
▸ dark           ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  ember          ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  forest         ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  gruvbox        ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  high-contrast  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  light          ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  meadow         ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  midnight       ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  nord           ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  obsidian       ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  periwinkle     ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  seafoam        ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  sorbet         ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  tokyo-night    ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉

  cyclops theme <name> to switch
```

Every state glyph has one fixed meaning under every theme and remains visible
under `NO_COLOR`; roomy surfaces pair it with a word. [theme guide](docs/guides/themes.md).

Every demo in [`demos/`](demos/) runs the real binaries against an isolated
tmux server and touches nothing of yours. `demos/m2-conversation.sh` is two
agents exchanging a reviewed message and a reply;
`demos/m4-workspace.sh` builds a session, saves it, destroys it, and brings
it back; `demos/m5-theme.sh` switches themes under a live pane border and
then catches a theme file mid-save; `tests/e2e/parity-check.sh` is the walk
above.

## Commands

| Command | What it does |
|---|---|
| `cyclops` | The workspace: sidebar, tabs, live panes. Starts a session and the daemon when none is running |
| `cyclops start` | Build or restore the default workspace from a preset, without opening the UI. Safe to run twice |
| `cyclops workspace save\|restore` | The shape of a session as a file: panes, sizes, names, directories |
| `cyclops name <pane> <label>` | Name a pane so cyclops can address it; the pane's tmux border says so |
| `cyclops list` | The roster: every named agent, how it is doing, what it is on. Inside tmux it scopes to your session; `--all` is every watched session |
| `cyclops status` | Every watched pane with its fused state, and the eye |
| `cyclops send <agent> --subject ...` | Deliver a message with an evidence-labeled receipt (`--wait done` blocks until the turn it starts ends) |
| `cyclops wait <agent> --until idle\|done\|blocked` | Block until an agent is ready, finishes a turn, or needs a human |
| `cyclops history --with <agent>` | The message record, newest last, with each delivery's current badge |
| `cyclops thread <id>` | One message plus its replies and delivery record |
| `cyclops hooks install <cli> --agent ...` | Render a vendor hook config plus wiring instructions |
| `cyclops hooks verify\|selftest <agent>` | Hook liveness, and one no-op delivery that proves the ack fires |
| `cyclops watch` | The live stream: calm admin view, firehose one keypress away, jump-to-pane |
| `cyclops theme [name]` | Switch themes, or list them with a preview of each. A switch is live at once |
| `cyclops update` | Rebuild from the latest source and replace the installed binaries; config and record untouched |
| `cyclops read <agent> --source detection` | Per-sensor readings behind a state verdict |
| `cyclops ping` | Daemon round trip |

All of them take `--json` and `--plain`, and honor `NO_COLOR`. (Two
exceptions have no `--json`: the deprecated `cyclops ui` alias, whose
machine stream is `cyclops watch --json`, and `cyclops update`, whose
output is the installer's stream.)

## How it works

A Rust daemon (`cyclopsd`) holds one scripted connection to tmux per watched
session, over the interface tmux calls control mode: cyclops asks and tmux
answers, and tmux keeps owning your panes, layout, and attach. A daemon crash
loses nothing. Agent state comes from sensor fusion: vendor hook events, pane
titles, output activity, and screen evidence as a last resort, with per-CLI
detection rules shipped as data in [`resources/manifests/`](resources/manifests/), not code.
Every message and state change lands in a ledger that is only ever appended
to, one JSON object per line, so you can `jq` it.

The crate-by-crate map, the delivery state machine, and the gate order are in
[architecture guide](docs/development/ARCHITECTURE.md).

## Docs

**Going to work on the code? Start at
[HANDOFF.md](docs/development/HANDOFF.md).** It is the front door for a newcomer: where
everything lives, what to read for the job you have been handed, and which
decisions were deliberate so you do not spend a day undoing one.

Otherwise, one page per question.

| | |
|---|---|
| [HANDOFF.md](docs/development/HANDOFF.md) | Start here to work on the codebase: the map, and the decisions behind it |
| [AGENTS.md](AGENTS.md) | The same front door for AI coding agents: the map condensed, and the gates a change must pass |
| [install.md](docs/guides/install.md) | Build it, configure it, run the tests |
| [QUICKSTART.md](docs/guides/QUICKSTART.md) | Two agents and a review gate, start to finish |
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
| [troubleshooting.md](docs/guides/troubleshooting.md) | When something is wrong |
| [ARCHITECTURE.md](docs/development/ARCHITECTURE.md) | How the pieces fit |
| [DELIVERY.md](docs/development/DELIVERY.md) | The delivery spec: states, evidence tiers, ordering |
| [CONTRIBUTING.md](CONTRIBUTING.md) | The development loop, the demos, and the gates a change must pass |
| [SECURITY.md](SECURITY.md) | Reporting a vulnerability privately |
| [INVARIANTS.md](docs/development/INVARIANTS.md) | Eleven rules a change must never break, and what breaks otherwise |
| [findings.md](findings.md) | The measurements the design rests on, F13 onward, each with the probe that proved it |
| [CHANGELOG.md](CHANGELOG.md) | What each milestone changed, in the order it shipped |
| [GOALS.md](docs/development/GOALS.md) | The quality bar every milestone is reviewed against |
| [STYLE.md](docs/development/STYLE.md) | How this codebase is written, binding on every change |

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
It is a read-only reference now. The usecyclops.dev installer follows this
tree's current `main` branch and installs the Rust implementation. Nothing
automatically migrates v1 state; [the cutover runbook](docs/development/CUTOVER.md)
is available if you still use the previous implementation.

## License

MIT, see [LICENSE](LICENSE). Upstream attribution for the v1 lineage is
in [NOTICE](NOTICE).
