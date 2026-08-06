# Delivery pipeline design

Fixed by ADR-001, the validation amendments, and GOALS.md. This is the
current delivery spec.

## Message rendering

The injected payload is what the recipient's model reads. Shape (kept close
to v1 so existing agent habits transfer):

```
[cyclops m-3f9c2a] FROM: codex  SUBJECT: Review the rate limiter
<body>
Reply: cyclops send codex --subject "..."
```

- `m-3f9c2a` is the message id: short lowercase hex, unique per ledger.
  It is the marker for composer verification and hook ACK matching.
- The daemon generates the header. Client-supplied FROM/SUBJECT text inside
  the body is not stripped but cannot forge the envelope: the header the
  recipient sees is daemon-built from socket-peer identity (v1 keeper made
  structural).
- The reply hint line is included unless the message is `--fyi`.

## Sender identity (fail-closed)

- Socket peer credentials give (uid, pid). Non-matching uid: denied.
- The client pid's ancestry is walked to a `pane_pid` of a watched pane.
  Resolved pane with a label: sender is that label. Resolved pane without a
  label: sender is the pane id. Unresolvable (a shell outside any watched
  pane, same uid): sender is `admin`.
- Nothing in the request body can override the resolved sender.

## Per-recipient pipeline

One worker per target pane; deliveries to one recipient are strictly FIFO
(GOALS: ordering holds per recipient). Broadcast writes ONE msg ledger line
with N delivery records, each advancing independently.

States and transitions are `cyclops_proto::DeliveryState` and its
`can_transition_to` table. Every transition appends a ledger line
(kind=state, id = message id, data = {to, from, to_state, cause}).

### Gate (amendments b, f, g; GOALS invariants)

Runs immediately when the fused state changes to a decidable one; never on a
timer. In order:

1. Resolve target label to pane id. Missing pane: attention_required
   (cause: no_such_pane).
2. `pane_dead`: attention_required. `pane_in_mode` (human scrolling in
   copy-mode): hold in gating; %pane-mode-changed re-triggers.
3. Fused state:
   - `blocked_quota`: park ALL queued deliveries for this recipient as
     parked_blocked_quota, admin.notify urgent with the reset hint parsed
     from screen. Never auto-retried (amendment f). After the reset, the
     operator sends a fresh message; there is no requeue verb.
   - `blocked_modal`: if the matched manifest rule has `auto_dismiss` and
     `decline_keys`, send the decline keys in order with ~250ms spacing,
     ledger a gate line naming the rule, re-evaluate. Multi-key declines
     re-capture the screen before the FINAL confirming key and require the
     same rule to still match; a dialog that changed under the sequence
     aborts back to the gate loop (gate line: decline_aborted, cause
     modal_changed). Rules without auto_dismiss (trust, permission
     prompts) hold in gating and admin.notify action_required.
   - `working`: hold in gating; the turn-end state change re-triggers
     (GOALS: queued lands within 1s of turn end).
   - `idle_with_input` (human typing, human always wins): hold in gating,
     re-check on the pane's next state change.
   - `idle`: proceed.
4. Just before pasting, re-read title and capture once more (the gate
   snapshot must be fresher than any human keystroke round-trip).

A delivery held in gating longer than `gate_hold_notify_ms` (config,
default 120000) pings the admin once (action_required) so a wedged hold is
visible; the hold itself keeps waiting on events, never on a timer.

### Inject (amendments b, e)

1. `load-buffer` from a temp file into buffer `cyc-<bootpid>-<seq>` (unique
   per delivery, amendment e), `paste-buffer -p -d`.
2. `bracket_paste_flag` is unavailable through tmux 3.6a (amendment b), so
   bracketed-paste degradation is not gateable. The gate is post-paste
   composer verification: capture the bottom region and require the
   manifest's `verify_pattern` with `<message_id>` substituted. agy's first
   paste after TUI start does not stage (manifest first_paste_caveat);
   verification failure goes to retry_queued, not silent loss.
3. Verified: state staged. Send the manifest's submit key (Enter):
   state submitted.

### ACK tiers (amendment: per-agent capability tiers)

- Tier 1 (claude, codex, cursor): the manifest `hooks.ack` event arrives via
  `agent.state.report` within the ACK window (default 1500ms; measured p95
  is under 40ms) and its `ack_payload_field` contains the message id:
  delivered_verified (verified_by: hook). Codex duplicate hook events are
  deduped on (session_id, turn_id, event) before matching (amendment d).
- Tier 2 (agy, or tier 1 timeout): screen evidence: the marker left the
  composer and turn-start evidence appeared (working state or output
  activity): delivered_unverified (verified_by: screen). A late matching
  hook ACK upgrades delivered_unverified to delivered_verified (legal
  transition, keeps receipts honest).
- Neither within 5s: retry_queued.

### Retry (bounded)

One redelivery attempt (validation soak needed zero). Second failure:
attention_required + admin.notify action_required. Never loop.

## msg.send semantics (push state, pull context)

The receipt returns the target's disposition, never auto-attached history:

- Target idle at send: block until a terminal-or-parked disposition, capped
  at 2.5s (GOALS: pasted under 1s, receipt under 2s on this path), return
  delivered_verified / delivered_unverified.
- Target busy: return immediately: queued with position (N ahead).
- Target parked: return immediately: parked_blocked_quota with the reset
  hint in `note`.

`wait` (send-and-wait) composes agent.wait onto the same call: after the
delivery resolves, block until the recipient reaches `until` (idle | done |
blocked) or `timeout_ms`. The wait starts only once the delivery reaches a
resolved state, and `done` counts only working phases observed at or after
this delivery's submit; a turn that predates the delivery never satisfies
it. A delivery that resolves anywhere but delivered has no turn to watch:
its wait entry reports the delivery state (`delivery` field) instead of a
fabricated wait result.

## Daemon restart

At boot the daemon replays each session ledger and closes every delivery
whose latest state is still in flight: a state line to attention_required
(cause: daemon_restart) per dangling chain, plus ONE aggregated
admin.notify listing them. Limbo is a bug (GOALS); a restart never leaves
a chain open. msg lines carry a `hosted` list naming the recipients whose
delivery chains live in that file, so a cross-session broadcast closes
only where it is hosted.

## Ledger and privacy

- kind=msg/fyi lines carry subject and body (the record IS the product).
- kind=state and kind=gate lines carry rule ids, states, and causes. Raw
  screen captures never enter the ledger (secrets rule): the gate line
  names the matched rule, not the pixels.

## Timing budgets (GOALS)

- send to paste on an idle target: < 1s (the gate is one state read plus
  one capture; no waits on the happy path).
- receipt: < 2s idle path.
- queued delivery starts within 1s of the turn-end state change (the worker
  wakes on the event, nothing polls).

## Later surfaces and current limit

- `cyclops hooks install`, hook verification, history, and thread queries
  shipped after the original delivery milestone.
- There is still no operator requeue verb for parked or attention-required
  deliveries. Send a new message after resolving the cause.

## v1.1 amendments (M1 gate)

Clarifications from the M1 gate review. They bind the implementation; the
sections above are unchanged.

1. Tier-2 evidence is conjunctive, as already worded: delivered_unverified
   requires BOTH the marker leaving the composer AND turn-start evidence
   (working state or output activity). Marker absence alone is not delivery
   evidence: a dialog or a redraw can clear the composer without the
   message ever becoming a turn. A changed composer window counts as turn
   evidence only when the post-paste verification demonstrably staged the
   message id; without a staged id, working or output evidence is
   required. Similarly, verification itself only accepts a generic
   pattern ("Pasted text") on a manifest composer line; residue from an
   earlier message elsewhere on screen verifies nothing. The substituted
   id pattern counts anywhere in the verify region: it is unique to the
   delivery.
2. ACK deadlines are detach-aware. While the tmux control connection is
   down the daemon cannot observe the pane, so ACK and retry deadlines
   freeze for the duration of the outage; time lost to a detach never
   counts against the ACK window. After reattach the pipeline re-verifies
   the composer and turn evidence before deciding anything, and in
   particular before any retry, so a delivery that landed during the
   outage is never pasted twice. Implemented as a per-delivery ACK clock
   (every remaining deadline shifts by the outage duration on reattach), a
   reattach evidence pass that runs before any deadline can expire, and
   hook reports resolved against the session's last-known pane table while
   detached: a report does not need the tmux connection, so tier-1 ACKs
   stay visible through the outage.
3. The inject contract includes a pane-rebind re-check. The gate's
   admitting snapshot is re-validated against the live pane table
   immediately before the paste and again immediately before the submit
   key: the pane must still exist, be alive, keep the pane_pid it was
   admitted with, and bind to the manifest the gate admitted. Any mismatch
   moves the delivery to retry_queued (cause: pane_rebound) with a gate
   ledger line, and the submit key is never sent; the retry re-enters
   gating and re-evaluates from scratch. Without this, a pane whose
   occupant changed between admit and injection (agent exited to a shell,
   another CLI took over) would receive the payload and an Enter, and a
   shell occupant would execute the message text.
