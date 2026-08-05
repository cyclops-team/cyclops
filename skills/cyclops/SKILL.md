---
name: cyclops
description: Communicate with other AI agents running in tmux panes via the cyclops CLI. Use when sending a message, handoff, question, or reply to another agent (e.g. "send this to reviewer", "tell the implementer", "notify the other agent", "hand this off"), when a received message starts with `[cyclops m-...]`, or when coordinating multi-agent work across panes. Replaces any prior pane-messaging approach (commPact, tmux send-keys, COORDINATION.md).
---

# Cyclops: talking to other agents

Cyclops is the coordination layer for agents already running in tmux. It
does not run your agent or type on your behalf outside a message you asked
it to send: it names panes, delivers messages between them with a receipt
you can trust, and keeps every fact in a ledger you can `jq`.

Everything here is `cyclops <subcommand>`. Confirm the exact flags on your
machine before relying on a command in this page: `cyclops --help` and
`cyclops <subcommand> --help` are the source of truth, and they can drift
from any doc, including this one.

## Before you do anything

Check the daemon is reachable and see the roster:

```
$ cyclops status
```

If you get `cyclops isn't running. Start it with: cyclops start`, someone
needs to run `cyclops start` in that tmux session before any of this
works. That is an operator action, not something you should route around
(see Safety rules below).

Find your own name (the row whose pane matches `$TMUX_PANE`, if you are
inside tmux) with `cyclops status` or `cyclops list`. If you are running
without a name, `cyclops name <label> --self` registers you using the pane
you are sitting in — the form to use for yourself, since it needs no pane
id lookup. Full detail: [../../docs/panes.md](../../docs/panes.md).

## 1. Discover peers and inspect state

`cyclops list` is the roster: every named agent, how it is doing, what it
is on.

```
$ cyclops list
  implementer  ○ idle  Implementing rate limiter
  reviewer     ○ idle  Awaiting review
```

(Real output, captured from an isolated demo session. The third column is
the pane title — on a real Claude/Codex/Cursor pane it is usually the
agent's current task; here it is whatever the demo fixture set.)

`cyclops status` shows the same roster plus every watched pane nobody has
named yet (listed by pane id), the tmux version, and the eye — whether
anything needs a human right now:

```
$ cyclops status
‿ cyclops · watching demo · tmux 3.7b · up 1s

  implementer  ○ idle  Implementing rate limiter
  reviewer     ○ idle  Awaiting review
```

A closed eye (`‿`) means nothing needs a human. An open one prints a
`waiting on you` block naming what does.

`cyclops read <agent> --source detection` is the diagnostic view: which
sensor decided the state, and which rule fired.

```
$ cyclops read reviewer --source detection
reviewer · ○ idle · decided by always_idle

  title  ○ idle  always_idle  just now
```

Add `--raw` to see the pane capture the sensors read, next to the verdict,
in the same answer — the fastest way to tell "cyclops is wrong" from "the
pane is genuinely in that state."

Every command above, and every command below, takes `--json` for scripts
and honors `--plain`/`NO_COLOR`. See
[../../docs/panes.md](../../docs/panes.md) for naming and the roster, and
[../../docs/troubleshooting.md](../../docs/troubleshooting.md) if a pane
reads `? unknown` (no manifest matches what is running there — a shell,
not an agent, or a CLI cyclops has not been taught; see
[../../docs/MANIFESTS.md](../../docs/MANIFESTS.md)).

## 2. Send a message and verify delivery

```
cyclops send <agent> --subject "One line" --body "Details" [--reply-to <id>] [--fyi]
```

The receipt is the whole point: it tells you what actually happened, not
what you hoped happened.

```
$ cyclops send implementer --subject "Run the tests" --body "make test"
✓ delivered · unverified (screen)
```

(Real output, isolated demo session — `--json` on the same call:)

```
$ cyclops send implementer --subject "Run the tests" --body "make test" --json
{"deliveries":[{"note":"screen_evidence","pane":"%0","state":"delivered_unverified","to":"implementer"}],"msg_id":"m-18bfdb","seq":5}
```

**Never blur the two evidence tiers.** They are different claims about the
same word "delivered":

| Badge | `state` | Meaning |
|---|---|---|
| `✔ delivered · verified` | `delivered_verified` | The recipient's own hook fired and reported this exact message id. The agent itself confirmed receipt. |
| `✓ delivered · unverified (screen)` | `delivered_unverified` | No hook, or it was late: cyclops saw the paste leave the composer and the turn start. That is strong evidence, not a confirmation from the agent. |

A late hook can upgrade `delivered_unverified` to `delivered_verified` —
the only legal transition backwards into more confidence, never less. On a
fresh install, before hooks are wired, every delivery is screen-tier —
that is normal, not degraded. Full spec:
[../../docs/DELIVERY.md](../../docs/DELIVERY.md);
[../../docs/send.md](../../docs/send.md) for the rest of the badge
vocabulary (`queued`, `parked`, `needs attention`) and quota parking.

<!-- F2: capture from a real run — a delivered_verified receipt needs a
     hook-wired agent CLI (claude/codex) actually running and firing its
     ack hook on this message id. Replace with real --json output once
     the fleet has one, e.g.:
     $ cyclops send reviewer --subject "..." --json
     {"deliveries":[{"note":"hook","state":"delivered_verified", ...}]}
-->

Exit codes for `send`: `0` cyclops has the message (delivered, or queued
behind another one), `1` parked or needs attention (also: daemon
unreachable), `2` usage error. The line between `0` and `1` is whether
waiting helps.

## 3. Receive and reply

A message that lands in your pane looks like this (real capture):

```
[cyclops m-d7e4ba] FROM: admin  SUBJECT: Review the rate limiter
Please look at retry.rs before the next run.
Both lines paste as one message.
Reply with: cyclops send admin --subject "..." [--body ... | --body-file -]
```

`m-d7e4ba` is the message id. `FROM` is daemon-resolved from who actually
sent it (the pane, not anything the sender's request claimed), so you can
trust it — and it is exactly who your reply should go to. The hint line
omits `--reply-to`; add it yourself so your reply chains into the same
thread instead of starting a new, unlinked message. If `FROM` is another
agent's name, send straight back to it:

```
$ cyclops send implementer --reply-to m-d7e4ba --subject "Re: Review the rate limiter" --body "Looked at retry.rs. Tests pass."
✓ delivered · unverified (screen)
```

One real gotcha worth knowing before you hit it: when `FROM` is `admin`
(the message came from a human's shell, not another agent's pane), the
hint's literal command does not work — `admin` is an identity, not an
addressable pane, and `cyclops send admin ...` fails with
`⚠ needs attention · no pane for "admin"` (confirmed by running it). There
is no pane to reply to in that case; do the work and let the record speak
(`cyclops history`, `cyclops thread <id>`), or use whatever channel you
already have with that human.

Reading the message back later, as a thread, oldest first (real capture,
same exchange):

```
$ cyclops thread m-d7e4ba
  2s  admin → implementer  Review the rate limiter      ✓ delivered · unverified (screen)
      Please look at retry.rs before the next run.
      Both lines paste as one message.

  1s  admin → implementer  Re: Review the rate limiter  ✓ delivered · unverified (screen)
      Looked at retry.rs. Tests pass.
```

`--fyi` messages (announcements) drop the reply-hint line — treat their
absence of a hint as intentional, not as something to invent a reply to.
`cyclops history --with <agent>` reconstructs a whole conversation, both
directions: [../../docs/history.md](../../docs/history.md).

## 4. Wait for work

`cyclops wait` blocks on an event, never a fixed sleep — use it instead of
polling `cyclops status` in a loop.

```
cyclops wait <agent> --until idle|done|blocked [--timeout 90s]
```

- `idle` — the composer is ready and no turn is running.
- `done` — the current or next turn ends (working → idle). If the agent is
  already idle, it must start and finish a turn first.
- `blocked` — the agent hit a vendor modal, a permission prompt, or quota.

Exit codes: `0` reached, `1` daemon unreachable/unknown target, `2`
timeout, `3` the pane died or changed occupant mid-wait (the wait is
pinned to the process it started watching, on purpose — it will not
answer for whoever took over the pane).

The handoff idiom composes send and wait in one call: `--wait` on `send`
waits only after the delivery itself resolves, so `done` can never be
satisfied by a turn that predates your message.

```
$ cyclops send implementer --subject "Run the tests" --wait done --timeout 3s
✓ delivered · unverified (screen)
wait: ⚠ wait timed out · still idle
```

(Real output — a `cat` demo fixture never starts a turn, so `done` always
times out here; the point to notice is the exit code.) That command
exited `0`. **The exit code follows the delivery, not the wait** — a
message that was delivered and then simply outran its wait budget is
still success from `send`'s point of view. A script that wants to gate on
the wait outcome must check it explicitly:

```bash
cyclops send reviewer --subject "..." --wait done --timeout 10m --json > receipt.json
jq -e '.wait[0].outcome == "reached"' receipt.json > /dev/null   # this is the line that actually gates
```

`outcome` is one of `reached`, `timeout`, `occupant_changed`, or
`not_delivered` (delivery never got far enough to start a turn — nothing
to wait for). Full detail: [../../docs/wait.md](../../docs/wait.md).

<!-- F2: capture from a real run — a `--wait done` that actually reaches
     `reached` needs a real agent CLI to run a turn (working -> idle
     edge); a `cat` fixture is always idle and can never produce one.
     Replace with real output once the fleet has a live agent, e.g.:
     $ cyclops wait reviewer --until done --timeout 5m
     ○ idle · waited <Ns>
-->

## 5. Diagnose a stuck delivery through the ledger

The ledger is the debugger. Every gate decision is a line, and every line
carries a cause — read it with `jq`, no daemon required:

```bash
jq -c 'select(.id == "<message-id>")' ~/.cyclops/ledger/<session>.ndjson
```

Real capture, a message that delivered cleanly (`m-d8510b`, default
session `demo`) — this is the shape to recognize, `kind=msg` once, then
one `kind=state` line per transition, plus a `kind=gate` line naming the
rule that admitted it:

```
{"seq":7,"id":"m-d8510b","kind":"msg","from":"admin","to":["implementer"],"subject":"Run the tests", ...}
{"seq":8,"id":"m-d8510b","kind":"state","data":{"from":"queued","to":"implementer","to_state":"gating","cause":null}}
{"seq":9,"id":"m-d8510b","kind":"gate","data":{"action":"proceed","cause":null,"rule":"always_idle","to":"implementer"}}
{"seq":10,"id":"m-d8510b","kind":"state","data":{"from":"gating","to":"implementer","to_state":"pasting","cause":null}}
{"seq":11,"id":"m-d8510b","kind":"state","data":{"from":"pasting","to":"implementer","to_state":"staged","cause":null}}
{"seq":12,"id":"m-d8510b","kind":"state","data":{"from":"staged","to":"implementer","to_state":"submitted","cause":null}}
{"seq":13,"id":"m-d8510b","kind":"state","data":{"from":"submitted","to":"implementer","to_state":"delivered_unverified","cause":"screen_evidence"}}
```

Read the last state line first, then the gate lines above it. A stuck
delivery is the same shape with the last line further back — held in
`gating`, or moved to `retry_queued` — and the cause on that line tells
you where to look next (cause table from
[../../docs/HANDOFF.md](../../docs/HANDOFF.md)):

| Cause | It means | Look at |
|---|---|---|
| `no_such_pane`, `pane_dead`, `session_detached` | The target is not there | The pane table: `cyclops status` |
| `pane_in_mode`, `working`, `idle_with_input`, `blocked:<rule id>`, `blocked_quota` | The gate is holding on purpose | Fusion: is the state right? `cyclops read <agent>` |
| `no_manifest` | Nothing bound to the pane | The manifest's `process_names` versus what the pane is actually running |
| `verify_failed` | The paste did not stage | The manifest's `verify_pattern`, and whether the composer is where you think |
| `pane_rebound` | The occupant changed between admit and inject | Something restarted in that pane. Working as intended |

The rule underneath all of it: **a hold waits on an event, never a
clock.** "It is stuck" always means "which event never arrived" — and the
answer is upstream of delivery, in fusion or the pane watcher, not in
delivery itself. A delivery held past `gate_hold_notify_ms` (default
120s) pings the admin once so a wedged hold is at least visible.

<!-- F2: capture from a real run — a genuinely stuck chain (held in
     `gating` on a real `blocked_modal` or ending in `retry_queued` with
     cause `verify_failed`) needs a live vendor CLI that actually opens a
     modal or fails to stage; a `cat` fixture never blocks. Replace with
     a real `jq` transcript once such a case is reproduced, e.g.:
     {"kind":"gate","data":{"action":"hold","cause":"blocked_modal", ...}}
-->

If the daemon itself is confusing rather than the ledger, `cyclops read
<agent> --source detection --raw` and `CYCLOPS_LOG=debug cyclopsd`
(operator-run) are the next layer down:
[../../docs/troubleshooting.md](../../docs/troubleshooting.md).

## 6. Safety rules you must never work around

These are invariants, not preferences — each one exists because breaking
it already did something specific and bad
([../../docs/INVARIANTS.md](../../docs/INVARIANTS.md) has the full list
and the proof).

- **Never bypass the delivery gate.** Do not `tmux send-keys` (or paste,
  or otherwise type) directly into another agent's pane to "save time" or
  route around a hold. The gate exists because a payload can land in a
  pane whose occupant changed — a shell, not the agent you meant — and a
  shell **executes** what a composer would have only read. Every message
  goes through `cyclops send`, with no exceptions, even when it is
  slower.
- **Never write the pane title.** It is a sensor cyclops (and the agent
  itself) reads to tell working from idle, not a place for your own
  decoration. If you want to announce a name or status, that is
  `cyclops name`, which paints the pane **border** and leaves the title
  alone.
- **A quota park is terminal.** `blocked_quota` never auto-retries, by
  design — a retry loop against an exhausted quota burns the reset and
  can cost money. If a recipient is parked, wait out the reset or send to
  a different agent; there is no re-queue verb. An operator resends after
  the quota resets.
- **The ledger appends; it never retracts.** Do not expect a corrected or
  resolved fact to replace an old line — it lands as a **new** line
  (e.g., an alarm followed later by a clearance). If you are parsing the
  ledger yourself, read forward and let the last line for an id win;
  never assume you can edit or delete one.

## Read more

- [../../docs/DELIVERY.md](../../docs/DELIVERY.md) — the delivery spec:
  states, evidence tiers, ordering.
- [../../docs/MANIFESTS.md](../../docs/MANIFESTS.md) — teaching cyclops a
  new agent CLI (one TOML file, no code) when a pane reads `? unknown`.
- [../../docs/troubleshooting.md](../../docs/troubleshooting.md) — real
  output for every common failure, with the fix.
- [../../docs/send.md](../../docs/send.md), [../../docs/wait.md](../../docs/wait.md),
  [../../docs/history.md](../../docs/history.md),
  [../../docs/panes.md](../../docs/panes.md) — one page per verb.

These doc paths are current as of this repo's present layout; a later
repository migration (M1 in the workspace recommendation plan) moves
`docs/` under a reorganized tree, and task F2 updates these links and
fills in the deferred output blocks above once that lands.
