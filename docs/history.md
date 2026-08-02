# History

Read the record. Every message ever sent is a line in an append-only
ledger; `history` and `thread` query it. Reading is free: any agent may
query the whole record, and reading never writes.

## Basics

```
cyclops history                      # the last 50 messages, newest last
cyclops history --with reviewer      # everything from or to reviewer
cyclops history --from codex --to reviewer
cyclops history --to me              # addressed to you
cyclops history --limit 10
```

`--with` reconstructs a conversation: both directions, plus broadcasts
that included the agent. `--from`/`--to` filter one direction each.
`me` resolves to whoever is asking: a pane's label inside a watched pane,
`admin` from any other shell. Pick one shape per query: `--with`, or
`--from`/`--to`.

Recipients are recorded under their canonical name, the pane's label
(pane id when unlabeled), however the sender addressed them: a send to
`%3` of the pane labeled `reviewer` is recorded to `reviewer`, so
`--with reviewer` finds it.

Each line shows when, who to whom, the subject, and the delivery's current
badge (the same voice as send receipts, see [send.md](send.md)). Under 24
hours the gutter is relative ("42s", "3h 12m"); older lines show the date.
Announcements carry a distinct `fyi` mark. A broadcast is one line with one
badge per recipient:

```
  2m  admin → 2 agents  fyi  Standup in 5
      reviewer     ✔ delivered · verified
      implementer  ● queued
```

The badges are live reads of the record: a message that parked or needed
attention shows that, not the state it had when sent.

## Threads

```
cyclops thread m-3f9c2a
```

One message with its body, every reply that chains to it (replies to
replies included), and each delivery's current badge, oldest first. The
full delivery chain (every state and gate line) rides along in `--json`.

## Scripts

`--json` returns the raw folded ledger lines plus `next_cursor`. Pass it
back as `--cursor` to page forward without gaps:

```
cyclops history --limit 100 --json          # newest 100, note next_cursor
cyclops history --cursor 4711 --json        # only messages recorded after
```

Without a cursor you get the newest `--limit` messages; with one you get
the oldest messages recorded after it, so a loop that feeds `next_cursor`
back walks the whole record exactly once.

`next_cursor` is only issued while ONE session is watched: it is a
per-session ledger seq, and with several watched sessions it would skip
whichever file's lines hide behind the other's numbering. There the
daemon refuses `cursor` with an error and pages on `cursor2` instead: an
opaque composite cursor issued as `next_cursor2` in every msg.history
answer. Pass it back verbatim as the `cursor2` param (the empty string
starts from the beginning); the walk covers every session's messages
exactly once, in order, cross-session broadcasts included. `cursor2` is
a socket-API param; the raw files below are always available too.

The ledger itself is plain NDJSON at `~/.cyclops/ledger/<session>.ndjson`,
one line per fact, always `jq`-able. `history` folds each message's
delivery chain into its `deliveries` array at read time; the file is never
rewritten.

## Exit codes

- `0` answered, including an empty record
- `1` daemon unreachable, or `thread` asked for an unknown id
- `2` usage error (`--with` combined with `--from`/`--to`)
