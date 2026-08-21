# Wait

Block until an agent reaches a state. The daemon watches the fused state
stream server-side; nothing polls, and the wait is pinned to the pane
occupant so a dead or replaced pane can never read as success.

## Basics

```
cyclops wait reviewer --until idle
cyclops wait codex --until done --timeout 5m
cyclops send codex --subject "Run the tests" --wait done
```

`--until` takes one of three targets:

| Until | Resolves when |
|---|---|
| `idle` | No turn is running. Not the same as being writable: whether a message may be pasted is the daemon-stamped write-readiness answer. |
| `done` | The current or next turn ends (the working to idle edge). An agent that is already idle must start and finish a turn first. |
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
`timeout`, `occupant_changed`) alongside `state` and `waited_ms`, the same
shape as send-and-wait entries.

## send --wait

`cyclops send <target> --wait done` delivers first, then waits. The wait
starts only after the delivery resolves, so `done` can never be satisfied
by a turn that predates your message, and it is pinned to the occupant
the message was SUBMITTED to: if the pane's process is replaced between
the delivery and the answer, the wait reports `occupant_changed` instead
of describing whoever lives there now. The receipt gains one wait line
per recipient; with `--json` each entry carries `{outcome, state,
waited_ms, delivery}`, where outcome is `reached`, `timeout`,
`occupant_changed`, or `not_delivered` (the delivery ended undelivered,
so there was no turn to watch). The exit code still follows the delivery
receipt, not the wait; scripts that branch on the wait read `--json`.

## Limits

State edges come from sensor fusion. On agents detected only by title or
screen rules, tmux reports changes once per second, so a turn that starts
and ends inside the same second can be missed by `done`. Hook-wired agents
report their edges directly and are not subject to that tick.
