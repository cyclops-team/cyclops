# Two agents and a review gate

Install cyclops, wire two agents, pass reviewed work between them, and read
it back months later. About ten minutes, most of it waiting for agents.

Day to day, you will not type most of these commands. You open the
workspace with bare `cyclops`, start your agents in its panes, and tell
them things like "when you're done, send it to reviewer". An agent that
knows Cyclops ([one file teaches it](../../skills/cyclops/SKILL.md)) runs
the handoff itself. This page walks the layer underneath, command by
command, so you can see what your agents do and drive every step yourself
when you want to.

The README ladder is the short version. This is the same walk from a bare
machine, with the handoff in the middle, which is the thing Cyclops is for.

The command shapes here are exercised by
[`tests/e2e/parity-check.sh`](../../tests/e2e/parity-check.sh) on a throwaway tmux
server. The examples abbreviate full UUID-based message ids for readability.
Color, paths, clocks, pane ids, and notification timing vary by run.

Other values also follow the machine: the shell a status row names, the tmux
version, and the loaded-manifest list. Treat the examples as stable output
shapes, not byte-for-byte transcripts.

## 1. Install

```bash
curl -fsSL https://www.usecyclops.dev/install.sh | sh
```

Building from source needs tmux 3.2+ and a Rust toolchain; [install.md](install.md)
covers getting Rust with rustup when `cargo` is missing.

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
the lifecycle hooks used by state detection and notification safety.

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

Standard messaging prefers a content-free doorbell when the exact claim skill
is installed. Without that capability proof it safely delivers the full
payload instead. Hooks report authenticated lifecycle edges that help Cyclops
distinguish a running turn from a clean composer on either path.

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
ask for a review", and the implementer runs the command itself:

```
$ cyclops send reviewer --subject "Burst path fix, ready for review" --body "gateway.rs:120. Tests pass."
accepted m-be0129
✓ accepted · wake queued
```

Nothing in that command says who is sending. The daemon resolves it by
walking the calling process up to a watched pane, so the record names the
pane rather than anything the request claimed. The body is durable, but the
reviewer's pane receives only a one-line wake:

```text
cyclops inbox claim m-be0129
```

The reviewer lists metadata, claims that exact id, and then replies:

```console
$ cyclops inbox list
m-be0129 implementer · Burst path fix, ready for review

$ cyclops inbox claim m-be0129
[cyclops m-be0129] FROM: implementer  SUBJECT: Burst path fix, ready for review
gateway.rs:120. Tests pass.
Reply: cyclops reply m-be0129 --body "..."

$ cyclops reply m-be0129 --body "Approved. One nit in the retry path."
accepted m-a94c10
```

A claim is atomic and recipient-authenticated. Repeating it returns the same
payload without creating a second task. A reply is the durable review verdict;
pane state is not.

From the reviewer pane, the thread reads back whole, oldest first. That caller
may see the request body because it claimed the message and the reply body
because it authored the reply:

```
$ cyclops thread m-be0129
  5s  implementer → reviewer  Burst path fix, ready for review
      gateway.rs:120. Tests pass.

  0s  reviewer → implementer  Re: Burst path fix, ready for review
      Approved. One nit in the retry path.
```

In this review workflow, the durable reply is the gate: work does not move
until a message with a verdict moves with it. Cyclops records the facts but
does not enforce that project policy.

`admin` uses the same mailbox. An agent can run `cyclops send admin ...`.
No pane wake is attempted; `cyclops status` shows the pending admin count and
an operator caller proven outside every watched pane uses `cyclops inbox list`
and `cyclops inbox claim <id>`. A shell inside a watched pane retains that
pane's agent identity.

## 6. Observe pane activity

`cyclops wait` blocks on a pane state edge without polling:

```
cyclops wait reviewer --until done --timeout 5m
```

`done` means a turn ran on that pane and reached idle while the same
process occupied it. It does not identify which message or task the turn
handled. Use the reviewer's reply from section 5 as the durable verdict.
Do not treat a pane state transition as proof that a specific review
finished. Exit codes and the occupant pinning rule are in [wait.md](wait.md).

## 7. Audit it later

The mailbox journal is one append-only file per durable workspace. Ask the
body-free snapshot for its id instead of guessing a directory name:

```bash
workspace_id=$(cyclops --json messages | jq -r .workspace_id)
cyclops_home="${CYCLOPS_HOME:-$HOME/.cyclops}"
jq -c 'select(.kind == "msg") | {from, to, subject, reply_to}' \
  "$cyclops_home/workspaces/$workspace_id/messages.ndjson" | tail -2
```

The immutable body, mailbox mutations, and content-free notification facts
are append-only. Treat the journal as sensitive owner-only state. Use the CLI
when caller-scoped body access matters. Session records under
`$CYCLOPS_HOME/ledger/` cover pane state and legacy direct delivery; they are
not the canonical mailbox journal.

Reading either journal is free and never writes.

## What to read next

- [skills/cyclops/SKILL.md](../../skills/cyclops/SKILL.md) to teach your
  agent the verbs and safety rules, so the handoff above is something you
  say rather than type
- [install.md](install.md) for PATH, config keys, and the manifests
- [send.md](send.md) for acceptance, claim, reply, and attention recovery
- [history.md](history.md) for filters, threads, and paging a long record
- [ui.md](ui.md) for the live stream
- [workspaces.md](workspaces.md) for presets and saving your own arrangement
- [troubleshooting.md](troubleshooting.md) when something is wrong
