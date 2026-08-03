# When something is wrong

Every message below is real output. Find yours, do the next step.

## "cyclops isn't running. Start it with: cyclops start"

```
$ cyclops status
cyclops isn't running. Start it with: cyclops start
```

Nothing is listening on `$CYCLOPS_HOME/sock`. `cyclops start` starts one,
and if starting it fails it says why rather than leaving you here.

If one was running and died, `cyclops daemon log` is where it wrote its
reason. `CYCLOPS_LOG=debug` says more on the next run.

The workspace commands (`start --no-daemon`, `workspace save|restore`)
work without a daemon, they just cannot name panes or count agents. That
is what a light `✓` on their output line means.

## "lost the connection to cyclops: path must be shorter than SUN_LEN"

```
$ cyclops status
lost the connection to cyclops: path must be shorter than SUN_LEN. Check that cyclopsd is still running, then retry.
```

Ignore the next step on that line; no restart can help. `$CYCLOPS_HOME` is
deep enough that `$CYCLOPS_HOME/sock` does not fit in a Unix socket
address, so `cyclopsd` never bound and never will. Run it in the foreground
once and it says the same thing about itself:

```
boot failed: bind /some/very/deep/path/.cyclops/sock: path must be shorter than SUN_LEN
```

Put the home somewhere shorter and start the daemon again. The cap and the
byte counts are in [install.md](install.md).

## A pane reads `? unknown`

```
$ cyclops status
‿ cyclops · watching main · tmux 3.6a · up 2s

  %0  ? unknown  mac
```

No manifest matched the process in that pane, so cyclops has no rules for
reading it and will not deliver to it. Three causes, in order of likelihood:

1. **The pane is running a shell**, not an agent. Expected. Start the agent.
2. **The process name is not what the manifest expects.** A wrapper script,
   a `sh -c`, or a versioned install whose binary reports a version number
   instead of a name. Pin it: `cyclops name %0 reviewer --manifest claude`.
   The pin wins over detection and sticks with the name.
3. **No manifests loaded at all.** Then every pane reads unknown. Check the
   daemon's log for `manifests loaded dir=... count=N`. Count 0, or no line
   at all, means the directory was empty, missing, or one file failed to
   parse and took the rest with it. See [MANIFESTS.md](MANIFESTS.md).

## A send to a pane nothing detects

```
$ cyclops send ghost --subject "hello"
⚠ needs attention · nothing detects %1
ghost did not get this message; it is on the record and needs attention.
Teach cyclops what runs in %1: cyclops name %1 ghost --manifest <id>.
cyclops status names the manifests that are loaded, and docs/MANIFESTS.md
is how to write one.
```

Exit code 1, and the message is on the record rather than lost. Cyclops
will not type into a pane it cannot read, so the delivery never starts.
Fix the binding, not the receipt: `cyclops status` names what is loaded and
what to pin.

Hooks are a different axis and do not help here. They upgrade a delivered
receipt from screen evidence to hook-verified, and a pane nothing detects
never gets that far.

## A send says "● queued"

```
$ cyclops send implementer --subject "hello"
● queued · 1 ahead
```

Queued means one thing: nothing has been typed into the pane yet. The
number is a queue position, so `1 ahead` means one message is in front of
this one. You see this when the recipient already has a delivery moving,
or when it could not take input at the instant you asked.

This one clears itself. Past the paste, a receipt reports the state it is
actually in rather than calling it queued, so a receipt that still says
queued is telling you the truth about where the message stands.

Every other reason clears itself. A recipient mid-turn, a human typing in
that pane, or a dialog waiting on a human decision all take the message in
order once the pane is ready. A receipt is the state at the instant you
asked; `cyclops history` is where each delivery actually ended up.
[send.md](send.md).

## "no manifest \"cluade\"; loaded: agy, claude, codex"

A typo, or a manifest the daemon has not read. The list is what it has.
Adding a file needs a `cyclopsd` restart: manifests are read once at boot.

## The eye is open and something is "waiting on you"

```
$ cyclops status
◑ 1 cyclops · watching main · tmux 3.6a · up 19s · 1 needs attention

  implementer  ○ idle  bash
  reviewer     ○ idle  bash

  waiting on you
  ghost  ⚠ needs attention
```

The eye opens for exactly two things: a pane in a blocked state, and a
delivery that cannot move without you. The block under the roster names
them, and the header counts them. Nothing closes it but the thing itself
clearing, and a clearance line follows the alarm in `cyclops ui` so a closed
eye can never sit over a stale warning.

## A send says "needs attention"

```
$ cyclops send ghost --subject "Review this"
⚠ needs attention · no pane for "ghost"
```

The qualifier is the whole diagnosis. `no pane for "x"` means no agent by
that name; `cyclops list` shows the ones that exist. The others are a dead
pane, no matching manifest, and two failed delivery attempts.

Exit code 1. The message is kept on the record either way.

## A send parks on quota

```
reviewer is out of quota, resets in 135h. The message is kept as parked; requeue it once the quota resets.
```

Parked deliveries are never retried automatically, and new sends to that
recipient park immediately behind them. That is deliberate: a retry loop
against an exhausted quota burns the reset. Wait it out, or send to a
different agent now. [send.md](send.md).

## Deliveries always land unverified

```
✓ delivered · unverified (screen)
```

The message arrived. What is missing is the recipient's own hook confirming
it, so the evidence is screen-tier: the paste left the composer and a turn
started.

```
cyclops hooks verify reviewer    # which edges have ever arrived
cyclops hooks selftest reviewer  # one no-op delivery, proves the ack fires
```

Most common cause by far is Codex CLI in an untrusted directory: it silently
loads zero hooks and `--dangerously-bypass-hook-trust` does not fix it.
Wiring per CLI, including that one: [hooks.md](hooks.md). Antigravity has no
payload-matchable ack at all, so its deliveries are screen-tier by design.

## `hooks unverified` on a pane that was fine a minute ago

Liveness belongs to the pane's current occupant, not the pane. Restarting
the CLI in a pane resets it until the new process fires an edge; a
predecessor's hooks never vouch for its replacement.

## A wait times out

```
reviewer didn't reach done within 60 seconds. Last state: working. Give it more time with --timeout, or look in with cyclops status.
```

Exit code 2. Usually the turn is simply longer than the budget.

One real limit: on an agent detected only by pane title or screen rules,
tmux re-evaluates what cyclops subscribed to once per second, so a turn that
starts and ends inside the same second is invisible to `--until done`.
Hook-wired agents report their edges directly and are not subject to that.

## A wait exits 3

```
reviewer's pane died or changed occupant while waiting, so the wait can't answer for the agent you asked about.
```

The wait pins the pane and its process when it starts. If either changes it
refuses to answer for whoever lives there now, rather than reporting a state
that belongs to a different program. Check `cyclops status` and rename the
pane if a new agent owns it.

## `cyclops start` says the daemon will not watch a session

```
$ cyclops start --workspace ops --session ops --preset ops
✓ workspace ready · 3 agents
  cyclopsd won't watch "ops" until it's listed in ~/.cyclops/config.toml. Add it to sessions there, then restart cyclopsd.
```

The daemon's session list is fixed when it boots. Add the session to
`sessions` in the config and restart `cyclopsd`. Cyclops writes the config
on a first run and never edits one you wrote, so this line is what you get
instead.

## `cyclops start` renamed nothing and says why

`start` refuses to put a workspace's names on a session it cannot match,
because a name is what every later send resolves through. It compares the
live grid to the workspace, position for position, plus the names the daemon
already holds. Any mismatch renames nothing at all and the line says which
difference stopped it. Rearrange back, or save the new shape:
`cyclops workspace save`.

Its floor, documented rather than hidden: if cyclopsd holds no names for the
session at all, two arrangements of the same shape are indistinguishable and
names go on by position. Naming one pane closes that.

## `cyclops name --clear` failed and kept the name

Deliberate. That record holds the only copy of your own pane border
settings; tmux is wearing cyclops's by then, and nothing else wrote yours
down. So the clear puts the border back first and drops the name second, and
a failed restore keeps both. Run it again once tmux is answering.

## Nothing is wrong, but the panes look squashed

A workspace built at one size and looked at from another drifts, because
tmux hands a resize out evenly rather than in proportion. `cyclops start`
and `cyclops workspace restore` build at the size of the terminal they were
run from, so this shows up when you build in a script or a small pane and
attach from somewhere else. Rebuild from the terminal you will work in.
[workspaces.md](workspaces.md).

## Border text ate a row of my pane

That is tmux drawing the border where the text goes, and it is the visible
price of the `role • state` chrome. Turn it off in
`~/.cyclops/config.toml`:

```toml
chrome = "off"
```

Off means cyclops writes no tmux option at all. Naming still works; the pane
just does not say so. [panes.md](panes.md).

## Colors are wrong, or absent

`NO_COLOR` and `--plain` win over every theme and are read before one is
even loaded. Both print the same words a colored terminal does.

A theme file broken mid-edit falls back to the built-in colors rather than
crashing, and a running `cyclops ui` keeps the colors it already has.
[themes.md](themes.md).

## I switched theme and the pane borders did not change

Look at the first line the switch printed. It says which of the three
happened, and only `✔` claims the borders moved:

```
⚠ theme light · saved, not live
  light  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  cyclopsd is still painting dark, so pane borders did not change. Check CYCLOPS_THEME and the themes directory where cyclopsd runs, then restart it.
```

The daemon resolves the theme itself rather than taking colors off the
wire, so it can end up somewhere the config key does not point. Two ways,
and a restart clears both once you have fixed the cause:

- `CYCLOPS_THEME` is set where cyclopsd runs. It beats the config key, and
  it is read once at startup, so nothing you do afterwards outvotes it.
- cyclopsd cannot see the themes directory this shell can. With no
  `~/.cyclops/themes`, a bare name resolves against `./themes` relative to
  the working directory, and the daemon's need not be yours.

`✓` on that line is the other case entirely: no daemon was running, so
there were no borders to repaint, and the next command picks the theme up.
[themes.md](themes.md).

## Tests fail on a machine where the code is fine

Run them with `--no-fail-fast`. Cargo stops at the first failing test binary
and hides every binary after it, which is how one portability bug looked
like a green build for two milestones:

```bash
cargo test --workspace --no-fail-fast
```

If the failure is a permission error under `/private/tmp`, relocate the
scratch root: `CYCLOPS_TEST_TMP=/some/writable/dir`. [install.md](install.md).

## Still stuck

`cyclops read <agent> --source detection` shows every sensor reading behind
a state verdict and names the rule that decided it; add `--raw` and the
same answer carries the pane capture those sensors read, so the evidence
and the verdict are one moment. `cyclops watch` streams what the daemon is
seeing, live. `CYCLOPS_LOG=debug cyclopsd` says the rest.

The record never lies and never needs the daemon:

```bash
jq -c 'select(.id == "m-914b34")' ~/.cyclops/ledger/main.ndjson
```
