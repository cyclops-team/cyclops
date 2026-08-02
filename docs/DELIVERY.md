# Delivery pipeline design (M1)

Fixed by ADR-001, the validation amendments, and GOALS.md. This is the spec
the M1 implementation is reviewed against.

## Message rendering

The injected payload is what the recipient's model reads. Shape (kept close
to v1 so existing agent habits transfer):

```
[cyclops m-3f9c2a] FROM: codex  SUBJECT: Review the rate limiter
<body>
Reply with: cyclops send codex --subject "..." [--body ... | --body-file -]
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
     from screen. Never auto-retried (amendment f). Re-queue is an explicit
     operator action.
   - `blocked_modal`: if the matched manifest rule has `auto_dismiss` and
     `decline_keys`, send the decline keys in order with ~250ms spacing,
     ledger a gate line naming the rule, re-evaluate. Rules without
     auto_dismiss (trust, permission prompts) hold in gating and
     admin.notify action_required.
   - `working`: hold in gating; the turn-end state change re-triggers
     (GOALS: queued lands within 1s of turn end).
   - `idle_with_input` (human typing, human always wins): hold in gating,
     re-check on the pane's next state change.
   - `idle`: proceed.
4. Just before pasting, re-read title and capture once more (the gate
   snapshot must be fresher than any human keystroke round-trip).

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

- Tier 1 (claude, codex): the manifest `hooks.ack` event arrives via
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
blocked) or `timeout_ms`.

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

## Not in M1

- `cyclops hooks install` and the startup hook self-test (amendment c): M2.
  M1 tests configure hooks the way the validation harness did (--settings /
  CODEX_HOME) and post agent.state.report through the socket.
- msg.history / msg.thread query methods: M2 (the ledger already records
  everything they will read).
- Operator re-queue verb for parked/attention deliveries: M2 surface.
