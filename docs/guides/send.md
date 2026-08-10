# Send

Deliver a message to any watched agent pane. The daemon gates on the
recipient's state, pastes, verifies the composer, submits, and returns a
receipt. Nothing is typed into the wrong pane and nothing is lost silently.

## Basics

```
cyclops send reviewer --subject "Review the rate limiter" --body "gateway.rs:120"
printf 'longer body' | cyclops send reviewer --subject "Handoff" --body-file -
```

The recipient's model reads:

```
[cyclops m-3f9c2a] FROM: admin  SUBJECT: Review the rate limiter
gateway.rs:120
```

A message from another agent carries one more line, `Reply: cyclops send
<name> --subject "..."`. One from `admin` does not. `admin` is the
operator, the name is reserved so no pane can hold it, and `cyclops send
admin` answers `no_such_target`: an agent that obeyed the hint would file
a failed delivery and raise attention for it.

The daemon builds the header from the sender's real identity (socket peer,
resolved to a pane). Nothing in the body can forge it. Replying to a
specific message? `--reply-to m-3f9c2a` links the two in the record.
Everything sent is queryable later: see [history.md](history.md).

## Receipts

One badge per recipient. The send blocks while an answer is coming and
stops blocking when it is not: an idle target holds until the delivery
resolves, capped at 2.5 s, and a target nothing detects holds only as long
as the refusal takes (milliseconds). A busy target answers immediately; the
head names its hold reason and followers carry their FIFO position.

| Badge | Meaning |
|---|---|
| `✔ delivered · verified` | The recipient's own hook confirmed this exact message arrived. |
| `✓ delivered · unverified (screen)` | Screen evidence only: the marker left the composer and a turn started. A late hook report upgrades it to verified. |
| `● queued · 2 ahead` | Another message is ahead of this one. The per-recipient FIFO remains in order. |
| `● held · recipient working` | This message is the in-flight head, and the recipient is still in a turn. |
| `● held · composer has input` | The composer contains input, so Cyclops waits rather than concatenating it. |
| `● held · pane in copy mode` | The pane is in copy mode; leave it there or exit the mode. |
| `● held · session detached` | The watched session is detached; Cyclops resumes on reattach. |
| `● held · waiting for a decision` | A modal or permission prompt needs a person. |
| `● held · target state unknown` | Sensors cannot safely decide the target state yet. |
| `● submitted` | Rare. The payload is in the pane and the confirmation had not landed when the receipt was printed. The badge lands on `cyclops history` when it does. |
| `⊘ parked · quota, resets in 135h` | Recipient is out of vendor quota. See below. |
| `⚠ needs attention · no pane for "reviewer"` | A human must look. The qualifier names why: missing pane, dead pane, nothing detecting the pane, or a failure whose terminal outcome is unknown. |
| `⚠ needs attention · nothing detects %4` | The recipient has a name and no manifest matches what runs in its pane, so nothing can be typed into it. See below. |

Verified means proven end to end: the recipient CLI's hook reported the
injected text and it contained this message's id. Unverified means the
screen showed the paste left the composer and the recipient started a turn,
but no hook vouched for it. Both are delivered; only the evidence differs,
and the check carries it: heavy `✔` is hook-verified, light `✓` is
screen-tier.
A recipient that always lands unverified probably has hooks that never
load: `cyclops hooks verify <target>` shows the evidence, [hooks.md](../reference/hooks.md)
the fix.

Until you wire hooks, every delivery is screen-tier, so `✓ delivered ·
unverified (screen)` is the normal receipt on a fresh install. It is a
delivered message, not a degraded one.

Add `--json` for the raw receipt. Anything the badge shows, scripts can read.

## A recipient nothing detects

```
$ cyclops send ghostpane --subject "hello"
⚠ needs attention · nothing detects %4
ghostpane did not get this message; it is on the record and needs attention.
Teach cyclops what runs in %4: cyclops name %4 ghostpane --manifest <id>.
cyclops status names the manifests that are loaded, and docs/reference/MANIFESTS.md is
how to write one.
```

Naming a pane makes it addressable, not readable. Cyclops types into a pane
only when a manifest tells it what is running there and how to tell a busy
composer from an idle one, so a pane no manifest binds can receive nothing.

The send stops there rather than queueing: nothing is pasted, the message is
kept on the record as needs attention, and the exit code is `1`. That code is
the contract a script depends on. Exit `0` means cyclops has the message and
will deliver it; a recipient nothing detects is not going to be delivered to
by waiting, so it must never share an exit code with one that is.

The pin command comes pasteable: the pane as the target, the name the pane
already answers to as the label. `cyclops status` lists the manifest ids to
choose from, and [MANIFESTS.md](../reference/MANIFESTS.md) is how to write one for a CLI
cyclops has not met.

`cyclops history` shows the same delivery as `⚠ needs attention · nothing
detects its pane`. Same words, minus the pane id: a folded record line
carries the recipient, not the pane the delivery went to.

## A delivery outcome is unknown

An attention receipt after `pasting`, `staged`, or `submitted` does not prove
that the recipient missed the message. A paste reply can be lost after tmux
has applied it, verification can be inconclusive, the pane can change after
staging, Enter can be accepted before its reply is lost, or the recipient's
ACK can time out after the turn starts. Cyclops records the exact cause and
never automatically pastes that logical message again. Inspect the named
recipient pane and its composer before resending.

Only failures proven before the pane write consume `delivery_retry_max`:
detach or missing manifest before paste, a pre-paste occupant rebind, and a
spool/load-buffer failure. A retry re-enters the full gate.

## Broadcast

```
cyclops send reviewer --to implementer,researcher --subject "Standup in 5"
cyclops send --all --subject "Rebase landed" --fyi
```

`--all` targets every labeled pane. A broadcast is one message in the
record with one delivery per recipient, each advancing on its own; the
receipt is a grid of recipient badges.

## --fyi

An announcement expecting no reply. The reply hint line is dropped from
what the recipient reads, and the record marks the message as fyi.

## --wait

`--wait idle|done|blocked` turns the send into send-and-wait: after the
delivery resolves, the receipt also reports when the recipient reached
that state. `--wait done` is the handoff idiom: deliver the task, return
when the turn it started has ended. Budget with `--timeout` (default 60s).
Details, outcomes, and exit semantics: [wait.md](wait.md).

## Exit codes

- `0` cyclops has the message: delivered, queued, or in flight
- `1` parked or needs attention (also: daemon unreachable)
- `2` usage error (no recipient, unreadable body file)

The line between `0` and `1` is whether waiting helps. Exit `0` means the
message is cyclops's problem now. Exit `1` means it is yours.

## Quota parking

When a recipient's vendor quota runs out, its deliveries park with the
reset hint read from its screen, and everything queued behind them parks
too. Parked messages are never retried automatically, and new sends to
that recipient park immediately. The message is kept in the record: wait
out the reset (the badge carries the hint), then send again, or send to a
different agent now.
