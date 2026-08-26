# Delivery and notification pipeline

This file owns the delivery decision and terminal safety contract. Protocol
shapes belong to `src/cyclops-proto` and `docs/reference/PROTOCOL.md`.

## Mailbox transport decision

`msg.send` always records the message and one mailbox entry per recipient
before terminal delivery begins. Each recipient then gets exactly one of two
transport shapes:

- **Doorbell:** when the admitted agent's manifest declares a mailbox
  capability file and that opened regular file exactly matches the claim skill
  compiled into this release, Cyclops writes the exact content-free claim
  command. The recipient runs it to read the durable payload.
- **Direct payload fallback:** when that exact capability proof is absent,
  unreadable, outdated, edited, or changes before the write, Cyclops rebuilds
  the canonical full payload from the workspace journal and writes it through
  the same gated terminal pipeline. Successful delivery appends
  `message_delivered_direct`. It never forges `message_claimed`.

Transport is selected per attempt, not per vendor. A target can move between
the two paths as its installed skill changes. Capability evidence is rechecked
with the exact recipient, process generation, manifest, and file digest at the
terminal write boundary. Losing proof before the paste returns to gating and
selects again. It never downgrades after any pane write.

Transport is write metadata on the notification record. It is not part of the
mailbox state and not part of the terminal occupant identity binding. Current
doorbell writes keep `transport: "doorbell"` and add `doorbell_format: 3`.
Format 3 carries a lossless 128-bit attempt token under the reserved `m-att_`
message-shaped namespace. This keeps the row runnable by older positional
claim clients while only the new daemon interprets the locator.
Format 2 replays its older message id plus trailing attempt-token comment byte for byte.
Format 1 remains readable as the older message-only compact row but cannot
provide hook evidence for a new Complete action. Missing format metadata
selects the original verbose row. Unknown numeric formats replay but cannot
authorize an attention recovery action. Missing
transport metadata on an old journal transition also means the original
doorbell format.

Both paths are one-shot. An ambiguous paste, verification, submit, or receipt
opens one attention entry and never writes the payload a second time. Attention
recovery rebuilds the expected bytes from the durable transport: the fixed row
for a doorbell, or the canonical message payload for direct fallback.

Current terminal-action settlements append `notification_resolved` with
`proof_version: 1` and replay only after the exact intent, action, and required
consumption facts. Missing proof versions are limited to historical format 1
or older doorbells and legacy direct payloads with incomplete process bindings.
They settle the old attempt during replay but cannot authorize a new action.
The same replay-only contract accepts the historical direct `Staged` to
`Submitted` edge. Current writes must pass through `Submitting`.

## Direct payload rendering

The fallback and legacy direct paths use this exact shape:

```
[cyclops m-3f9c2a] FROM: codex  SUBJECT: Review the rate limiter
<body>
Reply: cyclops send codex --subject "..."
[cyclops:end m-3f9c2a]
```

- `m-3f9c2a` is an abbreviated legacy message id in this example.
  It is the marker for composer verification and hook ACK matching.
- `[cyclops:end <id>]` is the terminal sentinel: transport machinery,
  not something the recipient acts on, and deliberately not the reply
  hint, since transport evidence must not depend on human-facing copy
  that changes. The leading id cannot prove the payload tail arrived.
- The daemon generates the header. Client-supplied FROM/SUBJECT text inside
  the body is not stripped but cannot forge the envelope: the header the
  recipient sees is daemon-built from socket-peer identity (v1 keeper made
  structural).
- The reply hint line is included unless the message is `--fyi`, or the
  sender is `admin`. `admin` is a durable mailbox address, not a pane label.
  A recipient replying to an admin message uses the message id so the daemon
  derives the admin inbox route.

## Sender identity (fail-closed)

- Socket peer credentials give (uid, pid). Non-matching uid: denied.
- The client pid's ancestry is walked to a `pane_pid` of a watched pane.
  Resolved pane with a label: sender is that label. Resolved pane without a
  label: sender is the pane id. A same-user caller proven outside every
  watched pane resolves as `admin`.
- Nothing in the request body can override the resolved sender.

## Per-recipient pipeline

One worker per durable recipient; notifications to one mailbox are strictly
FIFO. Broadcast writes one message row with one mailbox entry and one
notification record per recipient. A worker retires after its queue drains.
Enqueue and retirement share one registry lock, so a concurrent send either
joins the existing FIFO or creates its replacement without losing the handle.
Each worker job runs under a supervisor. One failure before the pane write
restarts the exact durable attempt. A repeated worker exit becomes a visible
`worker_failed` block. Other exhausted pre-write failures retain their closed
cause, including `write_readiness_changed` for a repeated binding or capability
race. A failure after the pane may have changed becomes attention instead of
retrying. If the journal cannot record that classification, the worker faults
while retaining the exact FIFO head and status exposes the fault for operator
recovery.

Mailbox notifications use `cyclops_proto::NotificationState`. Each transition
is a content-free workspace journal fact. Legacy direct deliveries still use
`cyclops_proto::DeliveryState` in session ledgers.

An authenticated mailbox claim orders against the terminal write boundary.
`queued`, `gating`, `quota_held`, and `quota_reset_observed` become `withdrawn`
inside the `message_claimed` fact and cancel that exact pending attempt. A claim
at `staged` leaves the attempt and composer barrier intact until Cyclops
re-proves and clears the exact doorbell. One
`notification_claimed_staged_cleared` fact then changes the attempt to
`withdrawn_after_staging` and retires its barrier together. A claim at
`submitting` retrieves the message once but does not cancel the reserved
terminal key. A claim at `submitted` advances the doorbell to `notified`.
An `ack_timeout` alarm for an exact-attempt doorbell with a complete binding
can advance to `notified` after an exact recipient claim and composer
reconciliation. The daemon must first clear the exact staged doorbell, or prove
the same bound composer is clean. The claim alone leaves the alarm and FIFO
barrier in place. Other post-write attention, `writing`, direct-payload states,
and existing `notified` records stay unchanged. `superseded` is reserved for an
actual message replacement. A claim proves retrieval only. It never proves task
completion or resolves any other post-write alarm.

### Gate (amendments b, f, g; GOALS invariants)

Runs immediately when the fused state changes to a decidable one; never on a
timer. In order:

1. Resolve target label to pane id. Missing pane: attention_required
   (cause: no_such_pane).
2. `pane_dead`: attention_required. `pane_in_mode` (human scrolling in
   copy-mode): hold in gating; %pane-mode-changed re-triggers.
3. Fused state:
   - On the legacy direct-delivery path, `blocked_quota` parks ALL queued
     deliveries for this recipient as `parked_blocked_quota` and sends one
     urgent admin notification with the reset hint parsed from screen. It never
     auto-retries (amendment f). After the reset, the operator sends a fresh
     message; this legacy state has no requeue verb. Standard mailbox
     notifications use the explicit guarded requeue command described below.
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
   - `idle`: proceed only on a positive write-readiness stamp. A refusal
     holds in gating and the gate line names it
     (`not_write_ready:<reason>`). `composer_hold` is the one that
     outlives the frame it was raised on: a pane seen holding text stays
     refused until a turn end with a clean screen reading proves the text
     left (INVARIANTS rule 12).
4. Just before pasting, re-read title and capture once more (the gate
   snapshot must be fresher than any human keystroke round-trip). The
   admitted pid is the agent's, resolved fresh; a process table that
   cannot be read is `occupant_unprovable`, not a fallback to the pane's
   shell.
5. Format 3 requires a pane width of at least 60 columns on that final row and
   at the immediate pre-write bookend. A narrower pane becomes
   `blocked_pre_write` with its content-free observed width and no paste. A
   later qualifying width observation from route or size evidence may reopen
   that attempt once.
A delivery held in gating longer than `gate_hold_notify_ms` (config,
default 120000) pings the admin once (action_required) so a wedged hold is
visible; the hold itself keeps waiting on events, never on a timer.

### Inject (amendments b, e)

1. `load-buffer` from a temp file into buffer `cyc-<bootpid>-<seq>` (unique
   per delivery, amendment e), `paste-buffer -p -d`.
2. Capture the joined, escaped composer region. Doorbells require their exact
   fixed row. Direct payloads require byte-for-byte reconstruction through the
   terminal sentinel. In both cases the extracted composer bytes must equal
   the payload selected at the durable write boundary.

   A measured collapsed-paste chip is representation evidence only. The hidden
   bytes cannot be compared, so a chip never proves Cyclops ownership and never
   authorizes Enter. It fails closed into one post-write attention result.

   `bracket_paste_flag` is unavailable through tmux 3.6a (amendment b), so
   bracketed-paste degradation is not gateable. The capture flavor follows the
   manifest: SGR-escaped (`capture-pane -e`) when any rule carries
   `line_regex_esc` clauses, plain otherwise. Composer pinning runs escaped
   clauses against raw rows and compares normalized visible bytes separately.
   A visible leading id is not evidence because it cannot prove completeness.

   The sentinel is terminal only if it matches a whole row, at least one
   row follows it, and the rows that follow are an ordered subsequence of
   the vendor's measured trailer layout, never more rows than the layout
   has. That layout is `injection.composer_trailer_regex` and
   `composer_trailer_regex_esc`, entry i describing row i of what the
   vendor paints below the composer; both forms must match. Order and
   cardinality are what bind the sentinel to the ACTIVE composer. A
   vendor with no measured layout has no sentinel path and refuses
   there. A split or truncated sentinel matches nothing and fails
   closed.

   Representation is decided per capture, never per vendor. A manifest with no
   measured composer and trailer layout has no exact staging proof and refuses.
   A manifest declaring `first_paste_caveat` does not stage its first paste
   after TUI start. Failed exact verification is an ambiguous post-paste
   outcome: it goes straight to `attention_required` and is never re-pasted.
3. Verified: state staged. Under the workspace journal lock, reserve the
   terminal key by appending `submitting`. This is the linearization point
   against an authenticated claim, not proof that a key was sent. Release the
   lock, re-prove the exact process binding and composer bytes, then send the
   manifest's submit key. Only successful terminal IO advances the attempt to
   `submitted`.

   Automatic notification submit runs the full proof immediately before the
   reservation and again after it. Both checks require the same pane-root,
   terminal leader, agent generation, and manifest; no pane mode; a current
   manifest state of `idle` or `idle_with_input`; and the exact attempt-owned
   staged barrier with no live lifecycle or blocked-state conflict. A refusal
   withholds Enter and settles once as `verify_failed`. It is never retried.

### Receipt tiers

- Tier 1 (claude, codex, cursor): the manifest `hooks.ack` event arrives via
  `agent.state.report` within the ACK window (default 1500ms; measured p95
  is under 40ms) and its `ack_payload_field` contains the exact format 3
  attempt locator. Legacy formats correlate through their recorded message
  and attempt markers. Codex duplicate hook events are
  deduped on (session_id, turn_id, event) before matching (amendment d).
- Tier 2 (agy, or tier 1 timeout): screen evidence shows the marker left the
  composer and turn-start evidence appeared (working state or output
  activity). A late matching hook may strengthen the internal receipt without
  changing mailbox ownership.
- Either valid tier advances the notification to `notified`. A direct fallback
  then appends `message_delivered_direct` and retires that mailbox entry. A
  doorbell leaves the entry pending until an authenticated claim.
- Neither within 5s: `attention_required` with cause `ack_timeout`. Enter may
  already have been accepted, so the payload is never re-pasted. A later exact
  recipient claim starts composer reconciliation for a current exact-attempt
  doorbell. Only exact clear or same-binding clean evidence moves it to
  `notified`.

### Retry (bounded, pre-write only)

The configured retry budget applies only when the daemon proves that no
payload bytes reached the pane: a detach or missing manifest before paste, a
pre-paste occupant rebind, or a spool/load-buffer failure. Those failures
enter `retry_queued` and re-enter the full gate. A `paste_failed`,
`verify_failed`, post-paste occupant rebind, `submit_failed`, or `ack_timeout`
is after the irreversible boundary and goes directly to
`attention_required` with that exact cause. The ledger therefore preserves
the attempt boundary and never invites a duplicate paste. `attention_required`
can mean the terminal outcome is unknown, not that the recipient definitely
did not receive the message.

A paste command pipe that accepted zero command bytes is also proven pre-write.
A legacy direct delivery may use its bounded retry because it has no durable
workspace notification boundary to correct.

A workspace notification writes its durable `writing` barrier immediately
before the paste command is attempted. If the transport then proves that the
first command write accepted zero bytes, Cyclops appends the narrowly scoped
`writing` to `blocked_pre_write` correction with cause
`paste_command_unwritten`. Only after that append succeeds does it release the
runtime composer hold. This exact attempt is claimable and withdrawable, but is
not replayed automatically. A partial command write or flush failure remains
`paste_failed` after the write boundary because tmux may have received it.

The `pane_too_narrow` width detail is also pre-write, but it does not consume
the retry budget. Its durable cause remains `write_readiness_changed` for old
reader compatibility, with observed and required widths recorded separately.
It stays withdrawable until a qualifying width edge or operator action.

### Static pre-write blocks

A mailbox notification stops as `blocked_pre_write` after a known pre-write
failure exhausts its bounded retry budget, a worker exhausts its restart budget,
or a write-boundary proof cannot safely continue. Binding and capability races
may receive one re-proof after new causal evidence without consuming transport
retry budget. A held composer barrier blocks immediately. The closed causes name
an unavailable session, manifest, payload, changed write readiness, paste-buffer
spool failure, an exact paste command that accepted zero bytes, unprovable
binding, missing composer semantics, or exhausted worker restart budget. None
writes pane bytes or retries on a timer. The message stays claimable, and a
workspace administrator may withdraw that exact unwritten
notification to release the recipient FIFO.
The transition also records the exact closed scheduler outcome when no live
worker owns the wake. Replay, send receipts, status, and message detail read
that same `wake_block`; they do not reconstruct a generic reason from the
notification state. Legacy rows without the additive field remain readable
and report `scheduler_state_unavailable`.
An exact positive route and composer-readiness observation may reopen the same
attempt once. The cached verdict must carry the same pane-root, foreground
leader, agent process generation, and manifest as the fresh route proof.

A workspace administrator may also withdraw the exact current attempt while it
is `queued` or `gating`. Those states and `blocked_pre_write` are all durably
before `writing`, so the operation writes one withdrawal fact, cancels that
attempt, leaves the message pending and claimable, and admits the next FIFO
item. `writing` and every later state refuse. Withdrawal availability alone
does not make ordinary queued or gating work require human attention.

## `msg.send` semantics

The response proves durable acceptance and reports the current asynchronous
notification state and FIFO position for each recipient. It does not block for
delivery, claim, or task completion. The protocol v1 `wait` field is rejected.
`agent.wait` observes a pane and cannot prove which message a turn handled.

`delivered_direct` and `claimed` are separate mailbox outcomes. Both authorize
that exact recipient to read the body. Direct delivery never records a claimant.

Legacy session delivery receipts retain their older behavior: idle targets wait
for a terminal or parked disposition, busy targets return their queue position,
and quota-blocked targets return the reset hint. The mailbox `msg.send` endpoint
does not use those receipt states.

## Daemon restart

The workspace journal replays mailbox, notification, transport, and attention
facts before scheduling. A `notified` direct attempt whose mailbox entry is
still pending is repaired to `delivered_direct` before the next FIFO item is
scheduled. A doorbell never receives that repair. Writing, staged, or submitted
attempts without a terminal receipt become attention and are not retried.

The following session-ledger rules apply to legacy direct deliveries:

### Legacy direct delivery

At boot the daemon replays each session ledger and resolves every
delivery whose latest state is still in flight. The same pre-write boundary
used by the running pipeline decides how:

- **Before the paste** (queued, gating, retry_queued): nothing has
  touched the pane, so the chain is REQUEUED: payload rebuilt from the
  msg line's from/subject/body, handle re-enqueued against the adopted
  pane (or the raw pane id), and the delivery re-enters the gate as if
  the restart were a long hold. A gating chain steps back through
  retry_queued (cause: daemon_restart) first; queued and retry_queued
  re-enter as recorded. ONE aggregated FYI names everything requeued.
- **Past the paste** (pasting, staged, submitted): the outcome is
  unknowable from here, so the chain closes with a state line to
  attention_required (cause: daemon_restart), plus ONE aggregated
  action-required admin.notify listing them.
- A pre-paste chain whose recipient maps to no pane this boot (label not
  adopted, session not watched) closes the same way: there is nothing to
  requeue into.

Limbo is a bug (GOALS); a restart never leaves a chain open. msg lines
carry a `hosted` list naming the recipients whose delivery chains live
in that file, so a cross-session broadcast resolves only where it is
hosted.

### Mailbox notification composer barriers

Mailbox notifications recover from the workspace message journal separately
from the legacy session-ledger chain above. Every `writing` transition with a
binding arms a durable barrier before the paste, including older bindings that
do not record a foreground leader. The barrier remains active through
`staged`, `submitting`, `submitted`, `notified`, and `attention_required`. A later bound write
replaces an older barrier only for the same exact durable recipient.

At startup, a claimed `staged` doorbell resumes exact-byte reconciliation,
and a claimed `submitted` doorbell advances to `notified`. Exact staged bytes
are cleared once and then settled. A crash may occur after that clear but
before the settlement append. Recovery may settle without another terminal
action only when the current manifest wins a `composer_semantic = "clean"`
rule, exact composer extraction returns visible empty bytes, and the complete process
binding still matches. Unsupported extraction, an unprovable composer, a hidden
chip, transcript ambiguity, or nonempty input refuses and enters one post-write
attention state.
Other unresolved `writing`, `staged`, `submitting`, and `submitted` attempts
close to `attention_required` with `daemon_restart`, but their composer
barriers remain active. The claimed staged settlement fact changes the state
and retires the barrier together. If its first append fails after a proven
clear, Cyclops repeats only that idempotent content-free append once. It never
clears or submits again. A second failure leaves the durable attempt `staged`,
keeps the exact worker and FIFO barrier active, and reports
`notification_settlement_storage_failed`. The operator runs `cyclops health`,
repairs state storage, then restarts the daemon so the same attempt reconciles.
One upgrade-only exception covers a stable `attention_required` or `notified`
format 1 or original doorbell whose binding lacks pane-root generation. An
exact recipient claim ordered after that attempt's `writing` fact, the same
current manifest, and fresh semantic-clean, exact visible-empty composer proof
may append a content-free `recipient_claimed_composer_clear` barrier
retirement. It sends no terminal key, clears no bytes, and leaves the historical
notification outcome and claimed mailbox state intact. Legacy direct payloads
do not qualify.
Recovery compares the recorded and current agent process generations and
manifests. A foreground leader change is normal agent continuity. A different
agent generation or manifest is authoritative replacement and is journaled
before the old barrier is released.

Only a receipt-bearing `notified` attempt may retire from a fresh clean screen
for the same occupant. Every other post-write state restores the composer hold
even if the first frame is clean. It retires automatically only after an exact
post-recovery turn start, its matching end, and a later fresh clean screen.
That retirement is appended before the runtime hold clears and before the end
is consumed. Hook idle alone never releases recovery state. Unknown or failed
journal writes keep the hold and require conservative retry or reopen.

A pane disappearing from one watched session may have moved to another.
Recovery follows the server-wide pane id across that route change, independent
of watcher attach order, while durable compaction stays scoped to the original
`RecipientKey`. Pane-loss retirement requires server-wide absence or a
different root-process generation.

## Quiesce (`daemon.quiesce`)

The pre-restart hold is what makes `cyclops daemon restart` and the restart
performed by `cyclops update` safe unattended. The daemon holds the workers
still. Each finishes its current delivery and starts no new one. The gate
rechecks the hold before proceeding so nothing crosses the paste boundary. A
delivery caught at that edge parks back in `retry_queued` with cause
`quiesce`. The daemon then waits out every delivery already past the paste,
with a default bound of 5s and a ceiling of 30s. Quiet means nothing is
between the paste and a resolved state anywhere. The pipeline stays held for
the stop that should follow and releases itself after 30s if none does. If it
is not quiet, the daemon releases the hold immediately and names what is still
moving; the caller refuses to restart. Pre-paste deliveries never block quiet.
The boot requeue above carries them across.

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

## Guarded composer recovery

`attention show` is read-only. `attention complete` and `attention discard`
require the exact unresolved attempt, the original process and manifest
binding, exact expected composer bytes, anchored trailer layout, and a current
safe terminal state. Diff inputs can contain a direct fallback payload. They
are returned only to the authenticated workspace administrator or the exact
durable recipient of that attempt and never enter the journal or daemon log.
Complete and discard remain administrator-only. Requeue and alarm clearance
remain explicit operator actions and never create an automatic retry loop.

An exact-attempt `verify_failed` doorbell uses the same proof and settlement
path automatically. Pending work selects one submit. An exact recipient claim
ordered after the write selects one measured clear. The mailbox choice and
durable intent share one lock. Human, trailing, changed, or unprovable content
never reaches a terminal key.

Before a terminal-key action, the daemon records one content-free resolution
intent. If the terminal accepts the key, it records a separate content-free
action-accepted fact. Neither fact is settlement. A fresh Complete also needs a
matching authenticated exact-attempt hook receipt or an exact recipient
claim ordered after this action, recorded as a content-free consumption fact.
Legacy and v1 doorbell hooks remain replay-compatible but cannot authorize new
Complete settlement. A
generic Working edge is only a re-evaluation cue. Complete and keyed Discard then require a fresh
manifest-owned clean-composer capture before `notification_resolved` may retire
the barrier. A no-key Discard requires two current positive empty-composer
observations and appends one atomic no-key resolution fact with no prior
intent. Missing evidence leaves the original attention item and barrier
open. Complete enters reconcile-only mode only with matching intent,
action-accepted, and consumption facts. Keyed Discard needs matching intent and
action-accepted facts. Intent-only Discard may use the same atomic no-key path
after two fresh exact-empty observations. Intent-only and accepted-but-unconsumed
Complete cannot retry or reconcile from later screen state. Reconciliation sends no key and
still requires exact binding and positive visible-empty composer proof. Exact
staged content, changed or hidden content, an unprovable composer, and a
different requested resolution remain unresolved.

The legacy session-delivery path has no operator requeue verb. Standard mailbox
notifications use the guarded `cyclops requeue <message-id>` recovery command.
Requeue resolves every selected pending recipient before writing. A current
exact-attempt `verify_failed` composer barrier must be resolved before requeue. Any
post-write barrier with an absent binding or missing pane-root or
foreground-leader generation also makes the whole requeue conflict before any
append.

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
   A mismatch before the payload write moves the delivery to retry_queued
   (cause: pane_rebound) with a gate ledger line, and the submit key is never
   sent; the retry re-enters gating and re-evaluates from scratch. A mismatch
   after the paste instead ends in attention_required with cause
   `pane_rebound_after_paste`; the original occupant may already hold the
   payload, so Cyclops never pastes it again. Without this, a pane whose
   occupant changed between admit and injection (agent exited to a shell,
   another CLI took over) would receive the payload and an Enter, and a
   shell occupant would execute the message text.
