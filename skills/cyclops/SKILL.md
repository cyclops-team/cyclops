---
name: cyclops
description: Communicate with other AI agents running in tmux panes via the cyclops CLI. Use when sending a message, handoff, question, or reply to another agent (e.g. "send this to reviewer", "tell the implementer", "notify the other agent", "hand this off"), when a received message starts with `[cyclops m-...]`, or when coordinating multi-agent work across panes. Replaces any prior pane-messaging approach (tmux send-keys, COORDINATION.md).
---

# Cyclops: talking to other agents

Cyclops is the coordination layer for agents already running in tmux. It
does not run your agent or type on your behalf outside a message you asked
it to send: it names panes, durably accepts mailbox messages, writes a
content-free notification when safe, and keeps append-only facts you can
inspect.

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

Find your own name with `cyclops list --json`: the entry whose `pane_id`
matches `$TMUX_PANE`, if you are inside tmux. The plain roster prints
labels, not pane ids. If you are running
without a name, `cyclops name <label> --self` registers you using the pane
you are sitting in. Use this form for yourself because it needs no pane id
lookup. Full detail: [docs/guides/panes.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/panes.md).

## 1. Discover peers and inspect state

`cyclops list` is the roster: every named agent, how it is doing, what it
is on.

```
$ cyclops list
  implementer  ○ idle  Implementing rate limiter
  reviewer     ○ idle  Awaiting review
```

(Real output, captured from an isolated demo session. The third column is
the pane title. On a real Claude/Codex/Cursor pane it is usually the
agent's current task; here it is whatever the demo fixture set.)

Inside tmux, when the daemon watches more than one session, `cyclops
list` scopes to the session your pane is in: the header names the
session it kept and a dim line names the elided ones with the way out.
`cyclops list --all` is every watched session, and `--json` scopes
identically: a scoped answer carries the elided session names as an
additive `also_watching` field, and `--all` restores the full dump. Your
own agents are always in the scoped roster, since it is your session by
definition.

Run `cyclops list --all` before starting a filtered `cyclops watch`.
`--with`, `--from`, and `--to` match current display labels. Cyclops refuses
an unknown active label instead of waiting silently. A rename can invalidate
a running display filter, so these filters are for human views, not durable
automation.
With `watch --json`, use `--kinds`; TUI display filters are refused.

`cyclops status` shows the same roster plus every watched pane nobody has
named yet (listed by pane id), the tmux version, and the eye: whether
anything needs a human right now:

```
$ cyclops status
‿ cyclops · watching demo · tmux 3.7b · up 1s

  implementer  ○ idle  Implementing rate limiter
  reviewer     ○ idle  Awaiting review
```

A closed eye (`‿`) means nothing needs a human. An open one prints a
`waiting on you` block naming what does.

`cyclops read <agent>` prints the pane's text: the visible screen by
default, the scrollback tail with `--source recent`, capped by `--lines N`.

`cyclops read <agent> --source detection` is the diagnostic view: which
sensor decided the state, which rule fired, and whether the pane is
write-ready. Those last two are different questions: an agent can be idle
and still not accept a write, because nothing proved its composer was
empty just now.

```
$ cyclops read reviewer --source detection
reviewer · ○ idle · decided by always_idle · write-ready

  title  ○ idle  always_idle  just now
```

Add `--raw` to see the pane capture the sensors read, next to the verdict,
in the same answer. This is the fastest way to tell "cyclops is wrong" from "the
pane is genuinely in that state."

Every command above, and every command below, takes `--json` for scripts
and honors `--plain`/`NO_COLOR`. See
[docs/guides/panes.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/panes.md) for naming and the roster, and
[docs/guides/troubleshooting.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/troubleshooting.md) if a pane
reads `? unknown` (no manifest matches what is running there: a shell,
not an agent, or a CLI cyclops has not been taught; see
[docs/reference/MANIFESTS.md](https://github.com/cyclops-team/cyclops/blob/main/docs/reference/MANIFESTS.md)).

## 2. Send a durable message

```
cyclops send <agent> --subject "One line" --body "Details" [--reply-to <id>] [--fyi]
```

`send` records one immutable message in each recipient's mailbox and returns
after durable acceptance. The receipt separates that fact from the one-line
pane notification:

```
$ cyclops send implementer --subject "Run the tests" --body "make test"
accepted m-18bfdb
✓ accepted · wake queued
```

(The exact wake state may advance before the receipt is rendered.)
Examples abbreviate message ids; live ids use `m-` plus 32 lowercase
hexadecimal UUID digits.

`accepted` proves the durable record and mailbox entry exist. `wake queued`
means terminal delivery is queued. Neither proves the recipient claimed or
received the payload, or completed the task. Use `--client-key <key>` when an
uncertain client call must be retried exactly.

If the connection drops before the response arrives, inspect current state
first. The request may already be durable. Repeat a send or reply only with the
same explicit `--client-key`; an unkeyed retry can create a second message.

With the exact shipped claim skill, Cyclops writes one content-free doorbell
after proving a clean composer:

```text
cyclops inbox claim m-att_<22-character-attempt-token>
```

The reserved `m-att_` locator works with positional-claim clients while the
daemon atomically resolves the exact current attempt. The returned envelope
names the message id used for reply. If the exact skill proof is absent,
outdated, edited, unreadable, or changes before the write, Cyclops instead
writes the full canonical payload ending in `[cyclops:end <id>]`. That direct
fallback is recorded as `delivered_direct`, not as a claim, so do not run
`inbox claim` for a payload already delivered this way.

Both transports are one-shot. If the outcome is ambiguous, attention is
raised. Never resend or requeue blindly. The full workflow and attention
commands are in
[docs/guides/send.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/send.md).

Older full-payload session records may use `delivered_verified` and
`delivered_unverified`. Current mailbox fallback uses `delivered_direct`; none
of those states means an authenticated claim occurred.

## 3. List, claim, and reply

The current wake line names the exact notification attempt. Running it claims
the bound message without exposing a mutable alias or truncated message id.
List pending metadata if you need the queue:

```console
$ cyclops inbox list
m-d7e4ba admin · Review the rate limiter
```

Claim only that id. This atomically marks the recipient mailbox entry claimed
and returns the immutable payload:

```
$ cyclops inbox claim m-d7e4ba
[cyclops m-d7e4ba] TO: reviewer  FROM: admin  SUBJECT: Review the rate limiter
Please look at retry.rs before the next run.
Reply: cyclops reply m-d7e4ba --body "..."
[cyclops:end m-d7e4ba]
```

A repeat claim returns the same payload and creates no second task. A claim
proves retrieval, not completion. Treat `TO`, `FROM`, `SUBJECT`, and the final
matching `[cyclops:end <id>]` as one framed envelope. Current daemons source the
labels from the immutable acceptance record; do not reinterpret a later alias
rename. Reply using the id so the daemon derives the recipient, thread, and
subject from the parent:

```
$ cyclops reply m-d7e4ba --body "Looked at retry.rs. Tests pass."
accepted m-42b817
```

For a bounded automation step, wait for and claim the oldest pending message
through the daemon socket:

```
$ cyclops inbox next --timeout 30s
```

The command subscribes before checking the inbox, claims at most one message,
and exits `2` if none arrives before the deadline. It never writes to the pane,
so it still works while the foreground command makes the agent read as
working. Do not run `cyclops watch` or a polling loop in the foreground while
waiting for a paste-dependent notification. That foreground tool keeps the
pane working and can gate the notification it is waiting for. Return to the
prompt for the normal wake, or use bounded `inbox next` for automation.

For one sender, copy the canonical `sender` key from `cyclops inbox list
--json` and use `inbox next --from <recipient-key>`. Do not pass a display
label. Exit `1` with `claim_outcome_unknown` means the claim was sent but its
answer missed the deadline. Inspect the message id before retrying.

`admin` is a valid durable mailbox address even though no pane may use that
label. Send to it with `cyclops send admin ...`. Admin gets no pane wake; the
operator sees the pending count in `cyclops status`. A same-user shell with no
agent-vendor ancestor has the `admin` inbox identity, including a shell inside
a watched pane. A vendor process gets an agent identity only through its
current watched pane. `--all` targets agent panes only, so address admin
explicitly.

`cyclops messages` shows body-free inbox, outbound, and notification state.
`cyclops history --with <agent>` and `cyclops thread <id>` reconstruct the
durable conversation.

## 4. Wait for pane activity

`cyclops wait` blocks on an event, never a fixed sleep. Use it instead of
polling `cyclops status` in a loop.

```
cyclops wait <agent> --until idle|turn-ended|blocked [--timeout 90s]
```

- `idle` means no turn is running. That is a statement about the turn, not
  permission to write: whether a notification may be written is the separate
  write-readiness answer the daemon stamps, which `cyclops read <agent>
  --source detection` shows beside the state.
- `turn-ended`: Cyclops observes Working and then idle on the same pane. If the agent is already
  idle, it must start and finish a turn first. This is not correlated to a
  message or task.
- `blocked`: the agent hit a vendor modal, a permission prompt, or quota.

Exit codes: `0` reached, `1` daemon unreachable/unknown target, `2`
timeout, `3` the pane died or changed occupant mid-wait (the wait is
pinned to the process it started watching, on purpose; it will not
answer for whoever took over the pane).

Message completion is a different fact. A claim proves an authenticated
recipient fetched the payload, not that work finished. Use a reply or an
explicit operator verdict when a workflow needs durable completion.
Full detail: [docs/guides/wait.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/wait.md).

## 5. Diagnose mailbox and notification state

Start with the body-free durable projection:

```bash
cyclops messages
cyclops alarm preview --older-than <age>
cyclops status
```

`messages` shows each message's mailbox state and each recipient's separate
wake state. `alarm preview` lists unresolved notification attempts and their
exact ids. `status` counts pane attention, legacy-delivery alarms, open
mailbox attention attempts, and held queue heads, plus the admin unread
count; its `waiting on you` rows name the next action for each. If a wake attempt needs
attention, inspect its exact id before taking an action:

```bash
cyclops attention show <attempt-id> --diff
```

`show` is read-only. `complete` submits the exact staged notification and
`discard` clears it without submitting, but both are allowed only when all
five safety checks pass again immediately before the key. An uncertain action
must not be repeated.

Clear known alarms by explicit id. For an age-selected operator cleanup,
`cyclops alarm clear --older-than <age>` prints and freezes the previewed ids,
then requires typing `clear` at a prompt that names the count and cutoff. There
is no clear-all form. In scripts, preview with `--json` and pass the exact ids
to `alarm clear`.

The workspace journal remains the final debugger. It is append-only and can be
read without the daemon. Discover the workspace id from the body-free snapshot
instead of guessing a directory name:

```bash
workspace_id=$(cyclops --json messages | jq -r .workspace_id)
cyclops_home="${CYCLOPS_HOME:-$HOME/.cyclops}"
jq -c 'select(.id == "<message-id>")' \
  "$cyclops_home/workspaces/$workspace_id/messages.ndjson"
```

`$CYCLOPS_HOME/ledger/<session>.ndjson` is the separate session record for
pane state and legacy direct delivery, not the mailbox journal.

If pane readiness is confusing, use `cyclops read <agent> --source detection
--raw`. Human workflow details live in
[docs/guides/send.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/send.md).

## 6. Safety rules you must never work around

These are invariants, not preferences. Each one exists because breaking
it already did something specific and bad
([docs/development/INVARIANTS.md](https://github.com/cyclops-team/cyclops/blob/main/docs/development/INVARIANTS.md) has the full list
and the proof).

- **Never bypass the notification gate.** Do not `tmux send-keys` (or paste,
  or otherwise type) directly into another agent's pane to "save time" or
  route around a hold. The selected doorbell or direct payload is admitted only after
  Cyclops proves the current occupant and a clean composer. Every message
  goes through `cyclops send`, even when a direct write looks faster.
- **Never write the pane title.** It is a sensor cyclops (and the agent
  itself) reads to tell working from idle, not a place for your own
  decoration. If you want to announce a name or status, that is
  `cyclops name`, which paints the pane **border** and leaves the title
  alone.
- **Notification ambiguity never auto-retries.** The immutable body remains
  in the mailbox, and a direct payload may also be staged in the composer.
  Inspect the exact attempt. An operator may use `cyclops
  requeue <message-id>` only after resolving the cause and confirming the
  attempt is eligible.
- **The ledger appends; it never retracts.** Do not expect a corrected or
  resolved fact to replace an old line. It lands as a **new** line
  (e.g., an alarm followed later by a clearance). If you are parsing the
  ledger yourself, read forward and let the last line for an id win;
  never assume you can edit or delete one.

## Read more

- [docs/guides/send.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/send.md), the mailbox, notification, and recovery workflow.
- [docs/reference/MANIFESTS.md](https://github.com/cyclops-team/cyclops/blob/main/docs/reference/MANIFESTS.md): teaching cyclops a
  new agent CLI (one TOML file, no code) when a pane reads `? unknown`.
- [docs/guides/troubleshooting.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/troubleshooting.md): real
  output for every common failure, with the fix.
- [docs/guides/wait.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/wait.md),
  [docs/guides/history.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/history.md),
  [docs/guides/panes.md](https://github.com/cyclops-team/cyclops/blob/main/docs/guides/panes.md): one page per verb.

Doc links point at the repository on GitHub, so this file works from
wherever it is installed. `cyclops start --setup-only --wire-hooks` seeds it
for installed Claude Code, Codex or Cursor, and Antigravity CLI consumers
without overwriting operator-edited copies.
