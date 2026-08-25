# History

Read the authenticated message view. `history` and `thread` merge the
authoritative workspace journal with pre-upgrade session records without
rewriting either source.

The workspace administrator can inspect all message metadata. An agent sees
only messages it sent or received. A body is visible only to its sender or the
recipient that claimed that exact message. Pre-upgrade session records have no
durable participant identity, so only the administrator retains that
compatibility view.

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
`me` resolves from the authenticated caller. A same-user shell with no
agent-vendor ancestor is `admin`, including inside a watched pane. A vendor
process gets a durable agent identity only through its current watched pane;
unprovable ancestry is denied. Pick one filter shape: `--with`, or `--from`
and `--to`.

Recipients are recorded under their canonical name, the pane's label
(pane id when unlabeled), however the sender addressed them: a send to
`%3` of the pane labeled `reviewer` is recorded to `reviewer`, so
`--with reviewer` finds it.

Each line shows when, who sent to whom, and the subject. Under 24 hours the
gutter is relative ("42s", "3h 12m"); older lines show the date.
Announcements carry a distinct `fyi` mark. A broadcast is one line:

```
  2m  admin → 2 agents  fyi  Standup in 5
```

Standard mailbox and notification state lives in `cyclops messages`. Old
direct-delivery records may still carry legacy delivery badges; those fields
remain readable for compatibility and are not the standard send contract.

## Threads

```
cyclops thread m-3f9c2a
```

One message and every reply that chains to it, oldest first. Bodies follow the
same sender-or-claimant visibility rule as history. Legacy state and gate lines
remain available in `--json` when the underlying compatibility record contains
them.

## Scripts

`--json` returns the folded authenticated view. A numeric cursor works only
when the daemon has one journal source. With the normal workspace journal plus
session compatibility records, the socket API uses the opaque `cursor2`
returned as `next_cursor2`.

```
cyclops history --limit 100 --json
```

Without a cursor, history returns the newest messages up to the limit. For a
complete multi-source walk, call `msg.history` through the socket API and feed
each `next_cursor2` back as `cursor2`.

The canonical mailbox journal is plain NDJSON. Discover its durable workspace
id rather than guessing a directory name:

```bash
workspace_id=$(cyclops --json messages | jq -r .workspace_id)
cyclops_home="${CYCLOPS_HOME:-$HOME/.cyclops}"
jq -c 'select(.kind == "msg" or .kind == "fyi")' \
  "$cyclops_home/workspaces/$workspace_id/messages.ndjson"
```

Treat raw journal bytes as sensitive owner-only state. Session records under
`$CYCLOPS_HOME/ledger/` contain pane state and legacy direct delivery. New
mailbox messages are never copied there.

## Exit codes

- `0` answered, including an empty record
- `1` daemon unreachable, or `thread` asked for an unknown id
- `2` usage error (`--with` combined with `--from`/`--to`)
