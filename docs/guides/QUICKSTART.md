# Two agents and a review gate

Install cyclops, wire two agents, pass reviewed work between them, and read
it back months later. About ten minutes, most of it waiting for agents.

Day to day, you will not type most of these commands. You open the
workspace with bare `cyclops`, start your agents in its panes, and tell
them things like "when you're done, send it to reviewer" — an agent that
knows Cyclops ([one file teaches it](../../skills/cyclops/SKILL.md)) runs
the handoff itself. This page walks the layer underneath, command by
command, so you can see what your agents do and drive every step yourself
when you want to.

The README ladder is the short version. This is the same walk from a bare
machine, with the handoff in the middle, which is the thing Cyclops is for.

Output here is real, captured by
[`tests/e2e/parity-check.sh`](../../tests/e2e/parity-check.sh) on a throwaway tmux
server. Three things are edited and nothing else: the home directory is
shortened to `~/.cyclops`, color is off, and a `Next:` block already shown
once is left out the second time.

Some of it follows your machine rather than that run: message ids and clock
values, the pane ids tmux hands out (`%0`, `%1`), the home directory in
`command -v` output, the shell a status row names for a pane running one
(`bash` below, `zsh` on plenty of machines), and the tmux version and
loaded-manifest list the daemon reports in `cyclops status`. Everything
else should match line for line.

## 1. Install

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh
```

It builds both binaries, puts them where your shell looks, and writes the
config and detection manifests. It ends by telling you whether this shell
can already find them:

```
✔ cyclops 0.1.0 is installed
  cyclops    /Users/you/.local/bin/cyclops
  cyclopsd   /Users/you/.local/bin/cyclopsd
  home       /Users/you/.cyclops

Next:
  1  exec /bin/zsh -l  so your shell can find cyclops
  2  cyclops           open your workspace and start your agents
```

Step 1 is there only when the installer had to add a line to your shell
profile. It prints that line, backs the file up first, and the uninstall
command in the [installation guide](install.md#uninstall) takes it back out.
[install.md](install.md) covers `--prefix`, `--no-path`, and installing
with cargo instead.

The home it set up holds two things, both needed. `config.toml` says which
tmux session to watch. The manifests are how cyclops tells what is running
in a pane, and nothing can be delivered to a pane it cannot read. Your
edits to them survive later runs; [install.md](install.md) covers what
happens when the shipped set gains one.

Nothing below needs the repo. Run it from wherever you work.

## 2. Open the workspace

One command. It builds the session, starts the daemon if none is running,
waits for it to reach the session, and names the panes.

`duo` is two panes side by side, one implementer and one reviewer.

```
$ cyclops start --preset duo
✔ workspace ready · 2 agents

Next:
  1  tmux attach -t main                         open the workspace and start your agents
  2  cyclops send implementer --subject "hello"  send the first message
```

The heavy check means the daemon confirmed the roster: two panes it will
deliver to, not two names in a file. A light `✓` means it could not be
asked, and the line under it says so.

The daemon outlives the shell you typed in. `cyclops daemon status` says
whether one is running, `cyclops daemon stop` takes it down, and
`cyclops daemon log` is where a detached one writes.

Attach, and start one agent CLI in each pane the way you normally would.

Or have cyclops start them. `--agents` names them by manifest id, one per
named pane in layout order:

```
cyclops start --preset duo --agents claude,codex
```

Typing the names is the decision to run them, so they start in that run
with no second flag. What it does not do is change the rule for later runs:
the commands go into the workspace file cyclops writes from the preset, and
a file naming a command is a suggestion, so the next `cyclops start` opens
shells unless you pass `--launch`. An id with no manifest, or a count that
does not fit the preset's named panes, is refused before anything is built.

The CLIs start bare, exactly as if you had typed their names. Step 4 wires
the hooks; until then their receipts are screen-tier.

## 3. Check the roster

```
$ cyclops list
watching main · home ~/.cyclops

  implementer  ● working  Implementing rate limiter
  reviewer     ○ idle
```

Three columns: the name, how the agent is doing, and what it is on. Both
names came from the preset. `duo` carries them, and `cyclops start` put them
on the panes it built.

When the daemon watches more than one session and you ask from inside
tmux, the roster is your session's alone, and a dim line under the
header names what was left out. `cyclops list --all` is every watched
session.

### When a pane reads unknown

`? unknown` is not a state an agent is in. It is cyclops unable to read one,
and a message to that pane will end up needing a human. `cyclops status`
says which of the two causes it is:

```
$ cyclops status
‿ cyclops · watching main · tmux 3.6a · up 2s

  %0  ? unknown  bash
  %1  ? unknown  bash

  2 panes read unknown: none of agy, claude, codex, cursor matches what is running there. Nothing can be delivered to an unknown pane. Pin one: cyclops name %0 <label> --manifest <id>. Teaching cyclops a new CLI is one file: docs/reference/MANIFESTS.md.
```

Manifests loaded and none of them matching, as here, is one CLI cyclops has
not been taught: pin one, or write one ([MANIFESTS.md](../reference/MANIFESTS.md)). The
other sentence, `cyclopsd loaded no detection manifests`, is the whole
install and is fixed once, with bare `cyclops` (or `cyclops start`) and a
daemon restart.

### Naming a pane yourself

Presets are one way in. The other is a session you arranged, where cyclops
watches panes it did not build. `cyclops status` shows all of them, and the
ones with no name are listed by pane id:

```
$ cyclops status
‿ cyclops · watching main · tmux 3.6a · up 0s

  implementer  ○ idle  bash · hooks unverified
  %1           ○ idle  bash
```

The closed eye means nothing needs you. `hooks unverified` means no hook has
reported from that pane this run, which the next step fixes. Take the pane
id and name it:

```
$ cyclops name %1 reviewer
✔ named reviewer · %1
```

Names are addresses, so they are unique across every watched session.
Naming is always explicit: cyclops never adopts a pane because it looks like
an agent. [panes.md](panes.md).

## 4. Wire the hooks

Skip this and everything still works. Without hooks a delivery is confirmed
by what cyclops can see on the screen, and the receipt says so in those
words:

```
✓ delivered · unverified (screen)
```

The light check is not a lesser delivery. It means the message landed and
the evidence is screen-tier, which is the honest thing to say when the
agent itself has not confirmed. Wiring hooks takes a minute and upgrades
every receipt to the heavy check, `✔ delivered · verified`, where the
recipient's own hook confirmed this exact message:

```
cyclops hooks install claude --agent reviewer   # renders config, prints wiring
cyclops hooks selftest reviewer                 # proves the hooks fire
```

Install never writes into a vendor config directory. It prepares files under
`~/.cyclops/hooks/` and tells you the one command to wire each CLI. The codex
directory-trust trap and the agy caveat are in [hooks.md](../reference/hooks.md).

## 5. The handoff

The implementer finishes something and hands it to the reviewer. In the
natural-language flow this is the moment you said "send it to reviewer and
ask for a review", and the implementer runs the command itself. The command
is the same either way — run in the implementer's own pane, by the agent or
by you, and its receipt prints there:

```
$ cyclops send reviewer --subject "Burst path fix, ready for review" --body "gateway.rs:120. Tests pass."
```

Nothing in that command says who is sending. The daemon resolves it by
walking the calling process up to a watched pane, so the record names the
pane rather than anything the request claimed:

```
$ cyclops history --with reviewer --limit 1
  4s  implementer → reviewer  Burst path fix, ready for review  ✔ delivered · verified
```

The reviewer replies, chaining to the message id:

```
$ cyclops send implementer --reply-to m-be0129 --subject "Re: Burst path fix" --body "Approved. One nit in the retry path."
```

And the thread reads back whole, oldest first:

```
$ cyclops thread m-be0129
  5s  implementer → reviewer  Burst path fix, ready for review  ✔ delivered · verified
      gateway.rs:120. Tests pass.

  0s  reviewer → implementer  Re: Burst path fix                ✔ delivered · verified
      Approved. One nit in the retry path.
```

That is the gate: work does not move until a message with a verdict moves
with it, and both halves are on the record with the delivery evidence
attached.

## 6. Make the gate blocking

The handoff above is fire and forget. To hold until the reviewer has
actually finished the turn your message started, wait for it:

```
cyclops send reviewer --subject "Review the burst path fix" --wait done --timeout 5m
```

`--wait done` returns on the working-to-idle edge of the turn your delivery
started, never a turn that predates it, and it is pinned to the pane
occupant the message was submitted to. Exit codes and the pinning rule:
[wait.md](wait.md).

A script gate around it has to read two answers, because the exit code is
only the first one:

```bash
set -e
cyclops send reviewer --subject "Review $BRANCH" --body-file diff.txt \
  --wait done --timeout 10m --json > receipt.json
jq -e '.wait[0].outcome == "reached"' receipt.json > /dev/null
```

The exit code follows the delivery, not the wait: 0 delivered or queued, 1
parked or needing a human, 2 a usage error. So a message that landed and
then ran out of wait budget still exits 0, and `set -e` alone lets the
script march on as if the review had happened. The `jq -e` line is the half
that gates: it stops on anything but a finished turn.

There is one wait entry per recipient, each `{to, outcome, state,
waited_ms, delivery}`. `outcome` is `reached`, `timeout`,
`occupant_changed` when the pane's occupant changed mid-wait, or
`not_delivered` when the delivery never got far enough to start a turn. A
gate that wants to tell those apart branches on that field instead of
testing it against one value.

## 7. Audit it later

The record is a file per watched session, one JSON object per line, never
rewritten. It does not need a daemon to read:

```
$ jq -c 'select(.kind == "msg") | {from, to, subject, reply_to}' ~/.cyclops/ledger/main.ndjson | tail -2
{"from":"implementer","to":["reviewer"],"subject":"Burst path fix, ready for review","reply_to":null}
{"from":"reviewer","to":["implementer"],"subject":"Re: Burst path fix","reply_to":"m-be0129"}
```

Every delivery attempt, gate decision, state change and ack is a line in the
same file, sharing the message id, so one `jq` filter reconstructs a message
and everything that happened to it. Secrets never enter it.

Back it up by copying `~/.cyclops/ledger/`. Reading is free and never
writes: any agent may query the whole record.

## What to read next

- [skills/cyclops/SKILL.md](../../skills/cyclops/SKILL.md) to teach your
  agent the verbs and safety rules, so the handoff above is something you
  say rather than type
- [install.md](install.md) for PATH, config keys, and the manifests
- [send.md](send.md) for receipts, broadcast, and quota parking
- [history.md](history.md) for filters, threads, and paging a long record
- [ui.md](ui.md) for the live stream
- [workspaces.md](workspaces.md) for presets and saving your own arrangement
- [troubleshooting.md](troubleshooting.md) when something is wrong
