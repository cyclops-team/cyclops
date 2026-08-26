# When something is wrong

Examples below show the stable output shape. Dynamic ids, clocks, paths, and
notification timing vary. Find the matching condition, then take the next step.

## "cyclops isn't running. Start it with: cyclops start"

```
$ cyclops status
cyclops isn't running. Start it with: cyclops start
```

Nothing is listening on `$CYCLOPS_HOME/sock`. `cyclops start` starts one,
and so does bare `cyclops`; if starting it fails either says why rather
than leaving you here.

If one was running and died, `cyclops daemon log` is where it wrote its
reason. `CYCLOPS_LOG=debug` says more on the next run.

The workspace commands (`start --no-daemon`, `workspace save|restore`)
work without a daemon, they just cannot name panes or count agents. That
is what a light `✓` on their output line means.

## Codex warns that `TERM` is `dumb`

Interactive agent CLIs need a terminal description that supports cursor
movement and full-screen drawing. If Codex prints this warning, do not continue
in that shell until the launch environment is understood:

```text
WARNING: TERM is set to "dumb". Codex's interactive TUI may not work in this terminal.
Continue anyway? [y/N]:
```

Check the exact shell that will launch the agent:

```bash
printf '%s\n' "$TERM"
```

That value is the authoritative terminal type for the process launched from
that pane. `tmux show-environment -g TERM` reports the tmux server's saved
client environment, not the value already assigned to an existing pane.

Inside tmux, Cyclops panes normally report `tmux-256color`. A regular terminal
outside tmux commonly reports `xterm-256color`. Cyclops does not set
`TERM=dumb`; it leaves pane terminal selection to tmux. Check that selection
with:

```bash
tmux show-options -gv default-terminal
tmux show-environment -g TERM
```

Use the first command to inspect the terminal type assigned to new panes. The
second is contrast-only evidence about the tmux server's saved client
environment; it does not predict a pane's `TERM` value.

Remove an explicit `export TERM=dumb` from the shell profile, launcher, or
automation that created the shell. If the shell is attached to a real terminal
emulator but inherited the wrong value, create a new tmux pane or restart the
pane shell and agent after correcting that configuration. Existing processes
keep the old environment. Do not set `xterm-256color` merely to silence the
warning in a non-interactive pipe or log runner; the claimed capabilities must
be real.

Codex 0.149.1 also refuses startup when its input or error stream is not attached
to a TTY. That is a separate launch error. Run the interactive TUI in a real
terminal pane; use a noninteractive Codex interface for pipes and automation.

Headless tmux control clients can themselves report `dumb`. That is expected and
does not change the terminal type inside an agent pane. Diagnose the value in
the pane where the agent process starts.

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

No manifest matched the process in that pane, so Cyclops has no rules for
reading it and cannot safely write a mailbox notification. Three causes, in
order of likelihood:

1. **The pane is running a shell**, not an agent. Expected. Start the agent.
2. **The process name is not what the manifest expects.** A wrapper script,
   a `sh -c`, or a versioned install whose binary reports a version number
   instead of a name. Pin it: `cyclops name %0 reviewer --manifest claude`.
   The pin wins over detection and sticks with the name.
3. **No manifests loaded at all.** Then every pane reads unknown. Check the
   daemon's log for `manifests loaded dir=... count=N`. Count 0, or no line
   at all, means the directory was empty, missing, or one file failed to
   parse and took the rest with it. See [MANIFESTS.md](../reference/MANIFESTS.md).

## A send is accepted but the pane receives no notification

Standard send returns after the mailbox write:

```
$ cyclops send reviewer --subject "hello"
accepted m-<full-uuid-suffix>
✓ accepted · wake queued
```

The second line is a snapshot of the one-line wake, not proof that the body was
claimed. Run `cyclops messages` to inspect the current mailbox and notification
state. If the target pane is unknown, start its agent or pin the correct
manifest. The body remains in the mailbox and is never pasted into the pane.

If `cyclops status` shows runtime idle but also prints `composer Cyclops
notification staged`, read the whole subrow. It names write readiness, the
notification and mailbox states, the next action, and the exact attempt id.
Cyclops must either submit that exact owned notification once, reconcile an
uncertain submit, expose a recoverable attention item, or allow a proven
pre-write withdrawal. It must not treat the staged notification as a generic
human draft or leave it unreported.

If the notification reaches `needs attention`, run `cyclops alarm preview
--older-than 0s` and inspect the exact attempt with `cyclops attention show
<attempt-id> --diff`. Do not resend or requeue blindly.

If status reports `wake blocked before write`, inspect the cause. A
`binding_unprovable` block needs process or route repair. A
`composer_semantic_missing` block means the matched manifest rule does not say
whether the composer is clean. A `worker_failed` block means the supervised
delivery worker exhausted its pre-write restart budget. The other exact causes
name an unavailable session, manifest, or payload, changed write readiness, or
a failed paste-buffer spool. None writes to the pane. Claim the message through
the socket, or have the workspace administrator withdraw the exact notification
shown by status so the next FIFO item can proceed. Inspect `cyclops health`
before requeueing a worker failure.

## A receipt says `1 ahead`

```
$ cyclops send implementer --subject "hello"
accepted m-<full-uuid-suffix>
✓ accepted · 1 ahead · wake queued
```

The position is the recipient mailbox's FIFO order. One older pending message
must be claimed before this one becomes oldest. Cyclops never overtakes it.
Use `cyclops messages` for current state and `cyclops inbox list` from the
recipient pane for pending metadata.

## Correlate a message with the workspace journal

The receipt id is the join key. Discover the workspace id from the authenticated,
body-free snapshot:

```bash
workspace_id=$(cyclops --json messages | jq -r .workspace_id)
cyclops_home="${CYCLOPS_HOME:-$HOME/.cyclops}"
jq -c 'select(.id == "m-<full-uuid-suffix>")' \
  "$cyclops_home/workspaces/$workspace_id/messages.ndjson"
```

That append-only journal owns immutable messages, mailbox mutations,
notification transitions, and recovery facts. Session ledgers under
`$CYCLOPS_HOME/ledger/` own pane state and legacy direct delivery. Use the CLI
instead of raw journal bytes when caller-scoped body visibility matters.

## "no manifest \"cluade\"; loaded: agy, claude, codex, cursor"

A typo, or a manifest the daemon has not read. The list is what it has.
Adding a file needs a `cyclopsd` restart: manifests are read once at boot.

## `cyclops status` says something is waiting on you

```
$ cyclops status
◑ 1 cyclops · watching main · tmux 3.7b · up 31s · 1 needs attention

  implementer  ○ idle  bash
  reviewer     ○ idle  cat · hooks unverified

  waiting on you
  ghost  ⚠ needs attention · no pane with that name · m-0e9a54 · just now
```

The id and age in this compatibility example change on every run. This status
surface owns blocked panes, legacy direct-delivery attention, the unread admin
count, and a bounded body-free sample of notification wakes blocked before any
write. It prints the full blocked-wake count when the sample is shorter than the
total. Use `cyclops messages` and `cyclops alarm preview --older-than <age>` for
the complete durable view and operator actions.

A Claude prompt-start hook can report `runtime working: provisional` before the
first visible output. `confirmed` means a current visual or exact keyed
lifecycle observation agreed. Both states refuse terminal writing while the
turn is active.

## A mailbox notification says `needs attention`

The send itself was already accepted. `cyclops messages` names the current
content-free notification state and attempt id. Inspect that exact attempt:

```bash
cyclops attention show <attempt-id> --diff
```

`show` is read-only. If the evidence still matches, `attention complete` or
`attention discard` performs one guarded action. An uncertain outcome must be
inspected and must not be repeated. `cyclops messages` names which recovery
boundary was proven, and reconciliation never sends a second terminal key.
`cyclops alarm clear` acknowledges the alarm but does not abandon an unfinished
terminal action. Retrieve the durable message with `cyclops inbox claim
<message-id>`. Only use `cyclops requeue <message-id>` after resolving the cause
and confirming that the notification is eligible. The exact transition and
reconciliation rules are in the
[protocol reference](../reference/PROTOCOL.md#mailbox-and-notification-control).

## A legacy hook self-test lands unverified

```
✓ delivered · unverified (screen)
```

This is the legacy direct-delivery self-test result. The injected test payload
reached screen evidence, but the recipient's acknowledgement hook did not fire.
It is not a standard mailbox receipt.

```
cyclops hooks verify reviewer    # which edges have ever arrived
cyclops hooks selftest reviewer  # one no-op delivery, proves the ack fires
```

The most common cause is Codex CLI in an untrusted directory: it silently
loads zero hooks and `--dangerously-bypass-hook-trust` does not fix it.
Wiring per CLI, including that one: [hooks.md](../reference/hooks.md). Antigravity has no
payload-matchable acknowledgement, so its self-test is screen-tier by design.

## `hooks unverified` on a pane that was fine a minute ago

Liveness belongs to the pane's current occupant, not the pane. Restarting
the CLI in a pane resets it until the new process fires an edge; a
predecessor's hooks never vouch for its replacement.

## A wait times out

```
reviewer didn't reach turn ended within 60 seconds. Last state: working. Give it more time with --timeout, or look in with cyclops status.
```

Exit code 2. Usually the turn is simply longer than the budget.

One real limit: on an agent detected only by pane title or screen rules,
tmux re-evaluates what cyclops subscribed to once per second, so a turn that
starts and ends inside the same second is invisible to `--until turn-ended`.
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
crashing, and a running `cyclops watch` keeps the colors it already has.
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

Run them with `--no-fail-fast`. Nextest must keep scheduling after a failure
so one run reports the full set instead of hiding later failures:

```bash
cargo nextest run --workspace -E 'not package(cyclopsd)' --no-fail-fast
cargo test -p cyclopsd --all-targets --no-fail-fast
```

If the failure is a permission error under `/private/tmp`, relocate the
scratch root: `CYCLOPS_TEST_TMP=/some/writable/dir`. [install.md](install.md).

## Still stuck

`cyclops read <agent> --source detection` shows every sensor reading behind
a state verdict and names the rule that decided it; add `--raw` and the
same answer carries the pane capture those sensors read, so the evidence
and the verdict are one moment. `cyclops watch` streams what the daemon is
seeing, live. `CYCLOPS_LOG=debug cyclopsd` says the rest.

The workspace journal can be read without the daemon. Discover its path instead
of guessing it:

```bash
workspace_id=$(cyclops --json messages | jq -r .workspace_id)
jq -c 'select(.id == "m-914b34")' \
  "${CYCLOPS_HOME:-$HOME/.cyclops}/workspaces/$workspace_id/messages.ndjson"
```
