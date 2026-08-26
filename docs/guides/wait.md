# Wait

Block until an agent reaches a state. The daemon watches the fused state
stream server-side; nothing polls, and the wait is pinned to the pane
occupant so a dead or replaced pane can never read as success.

## Basics

```
cyclops wait reviewer --until idle
cyclops wait codex --until done --timeout 5m
```

`--until` takes one of three targets:

| Until | Resolves when |
|---|---|
| `idle` | No turn is running. Not the same as being writable: whether a message may be pasted is the daemon-stamped write-readiness answer. |
| `done` | Working is observed, then the same pane occupant reaches `idle` or `idle_with_input`. This does not identify a turn, message, or task, and it does not prove write readiness. |
| `blocked` | The agent hits any blocked state: vendor modal, permission prompt, or quota. |

`--timeout` reads human durations: `90s`, `2m`, `1m30s`, `500ms`. Default
60s, capped at 10m.

## Exit codes

- `0` reached; stdout shows the state and how long it took
- `1` daemon unreachable or unknown target
- `2` the timeout expired; stderr names the state the agent was last in
- `3` the pane died or changed occupant mid-wait

Code 3 is the pinning rule: the wait records the pane and its process at
start, and if either changes the wait refuses to answer for whoever lives
there now. `2` is also the usage-error code; a valid command line never
exits 2 for usage.

With `--json`, every answer carries an `outcome` field (`reached`,
`timeout`, `occupant_changed`) alongside `state` and `waited_ms`.

## Message completion

`cyclops wait` observes a pane, not a message. It cannot prove that an
agent completed a specific task. A claimed message proves that an
authenticated recipient fetched its payload, but it does not prove work
started or finished. Cyclops therefore keeps `send` and pane waiting as
separate commands and does not advertise a message-completion wait.

## Limits

State edges come from sensor fusion. On agents detected only by title or
screen rules, tmux reports changes once per second, so a turn that starts
and ends inside the same second can be missed by `done`. Hook-wired agents
report their edges directly and are not subject to that tick.
