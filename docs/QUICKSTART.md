# Two agents and a review gate

Wire two agents, pass reviewed work between them, and read it back months
later. About ten minutes, most of it waiting for agents.

The README ladder is the short version. This is the same walk with the
handoff in the middle, which is the thing Cyclops is for.

Output here is real, captured by
[`demos/parity-check.sh`](../demos/parity-check.sh) on a throwaway tmux
server. Three things are edited and nothing else: the home directory is
shortened to `~/.cyclops`, color is off, and a `Next:` block already shown
once is left out the second time. Message ids and clock values belong to
that run; yours will differ there and nowhere else.

## 1. Open the workspace

`duo` is two panes side by side, one implementer and one reviewer.

```
$ cyclops start --preset duo
✓ workspace ready · 2 agents
  wrote ~/.cyclops/config.toml

Next:
  1  cyclopsd &                                  start the daemon
  2  tmux attach -t main                         open the workspace and start your agents
  3  cyclops send implementer --subject "hello"  send the first message
```

Do those in order. `cyclopsd &` from anywhere; it reads
`~/.cyclops/config.toml`, which `start` just wrote. Then attach, and start
one agent CLI in each pane the way you normally would.

Run `cyclops start` again once the agents are up. The second run is where
the names go on the panes, because only a running daemon can adopt one:

```
$ cyclops start
✔ workspace ready · 2 agents
```

The heavy check means the daemon confirmed the roster. A light `✓` means it
could not be asked and the count came from the workspace file.

## 2. Check the roster

```
$ cyclops list
  implementer  ● working  Implementing rate limiter
  reviewer     ○ idle
```

Three columns: the name, how the agent is doing, and what it is on. Both
names came from the preset. `duo` carries them, and `cyclops start` put them
on the panes it built.

If a pane reads `? unknown`, no manifest matched the process in it. Pin one
by hand with `--manifest claude` when you name it, or write one:
[MANIFESTS.md](MANIFESTS.md).

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

## 3. Wire the hooks

Skip this and everything still works; deliveries land verified by screen
evidence instead of by the agent's own hook. Wiring takes a minute and
upgrades every receipt:

```
cyclops hooks install claude --agent reviewer   # renders config, prints wiring
cyclops hooks selftest reviewer                 # proves the hooks fire
```

Install never writes into a vendor config directory. It prepares files under
`~/.cyclops/hooks/` and tells you the one command to wire each CLI. The codex
directory-trust trap and the agy caveat are in [hooks.md](hooks.md).

## 4. The handoff

The implementer finishes something and hands it to the reviewer. This is run
in the implementer's own pane, by you or by the agent itself, and its receipt
prints there:

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

## 5. Make the gate blocking

The handoff above is fire and forget. To hold until the reviewer has
actually finished the turn your message started, wait for it:

```
cyclops send reviewer --subject "Review the burst path fix" --wait done --timeout 5m
```

`--wait done` returns on the working-to-idle edge of the turn your delivery
started, never a turn that predates it, and it is pinned to the pane
occupant the message was submitted to. Exit codes and the pinning rule:
[wait.md](wait.md).

A script gate around it:

```bash
cyclops send reviewer --subject "Review $BRANCH" --body-file diff.txt --wait done --timeout 10m
cyclops --json history --with reviewer --limit 1 | jq -r '.lines[0].subject'
```

The exit code follows the delivery, not the wait: 0 delivered or queued, 1
parked or needing a human, 2 a usage error. Scripts that branch on the wait
outcome read `--json`, where every wait entry carries `{outcome, state,
waited_ms, delivery}`.

## 6. Audit it later

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

- [send.md](send.md) for receipts, broadcast, and quota parking
- [history.md](history.md) for filters, threads, and paging a long record
- [ui.md](ui.md) for the live stream
- [workspaces.md](workspaces.md) for presets and saving your own arrangement
- [troubleshooting.md](troubleshooting.md) when something is wrong
