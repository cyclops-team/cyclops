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
Reply with: cyclops send admin --subject "..." [--body ... | --body-file -]
```

The daemon builds the header from the sender's real identity (socket peer,
resolved to a pane). Nothing in the body can forge it. Replying to a
specific message? `--reply-to m-3f9c2a` links the two in the record.
Everything sent is queryable later: see [history.md](history.md).

## Receipts

One badge per recipient. An idle target blocks until the delivery resolves,
capped at 2.5 s. A busy target answers immediately with its queue position.

| Badge | Meaning |
|---|---|
| `✔ delivered · verified` | The recipient's own hook confirmed this exact message arrived. |
| `✓ delivered · unverified (screen)` | Screen evidence only: the marker left the composer and a turn started. A late hook report upgrades it to verified. |
| `● queued · 2 ahead` | Recipient cannot take input yet: mid-turn, a human typing, or a dialog waiting on a human decision (you get alerted). Delivers in order once the pane is ready. |
| `⛔ parked · quota, resets in 135h` | Recipient is out of vendor quota. See below. |
| `⚠ needs attention · no pane for "reviewer"` | A human must look. The qualifier names why: missing pane, dead pane, no matching manifest, or two failed delivery attempts. |

Verified means proven end to end: the recipient CLI's hook reported the
injected text and it contained this message's id. Unverified means the
screen showed the paste left the composer and the recipient started a turn,
but no hook vouched for it. Both are delivered; only the evidence differs,
and the check carries it: heavy `✔` is hook-verified, light `✓` is
screen-tier.
A recipient that always lands unverified probably has hooks that never
load: `cyclops hooks verify <target>` shows the evidence, [hooks.md](hooks.md)
the fix.

Add `--json` for the raw receipt. Anything the badge shows, scripts can read.

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

- `0` delivered or queued
- `1` parked or needs attention (also: daemon unreachable)
- `2` usage error (no recipient, unreadable body file)

## Quota parking

When a recipient's vendor quota runs out, its deliveries park with the
reset hint read from its screen, and everything queued behind them parks
too. Parked messages are never retried automatically, and new sends to
that recipient park immediately. The message is kept in the record: wait
out the reset (the badge carries the hint), then send again, or send to a
different agent now.
