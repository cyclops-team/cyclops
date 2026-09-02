# Send

`cyclops send` records one immutable message in each recipient's durable
mailbox and returns after acceptance. Terminal delivery is asynchronous. A
CLI send or reply queues a two-sentence preview and exact claim command through
the safe terminal gate. The full technical body remains in the authenticated
mailbox.

## Send

```console
$ cyclops send reviewer --subject "Review the rate limiter" \
    --summary "The rate limiter is ready for review. Check the burst path for regressions." \
    --body "gateway.rs:120"
accepted m-3f9c2a
✓ accepted · wake queued
```

Message ids are abbreviated in prose examples. The daemon emits `m-` followed
by the full 32 lowercase hexadecimal UUID digits.

Use `--body-file -` for stdin, `--client-key <key>` for an exact retry, and
`--fyi` for an announcement that should not be answered. A successful result
proves the message is durable. It does not prove that the recipient claimed it
or completed the work.

If the connection drops before the response arrives, the request may already
be durable. Inspect the mailbox or message history first. Repeat a send or reply
only with the same explicit `--client-key`; an unkeyed retry can create a second
message.

The receipt reports two separate facts:

- `accepted` means the durable message and recipient mailbox entries exist.
- `wake <state>` reports the current terminal notification attempt. It may be
  `not started`, `queued`, `checking readiness`, `writing`, `staged`,
  `submitted`, `notified`, `withdrawn`, `needs attention`, or `superseded`.

When the recipient FIFO head has no live notification worker, the receipt also
reports `wake blocked (<reason>)`. The message remains accepted and claimable.
This field explains why the wake has no current owner; it does not prove that
the composer was written or that the message was claimed. The protocol
reference owns the closed [`wake_block` vocabulary](../reference/PROTOCOL.md#msgsend).

A durable failure before terminal bytes reports `wake blocked before write
(<reason>)`. Its `pre_write_cause` is separate from `wake_block`: the first
names the failed write-boundary proof, while the second names why no scheduler
worker owns the wake. The plain and JSON commands still exit 0 because the
message remains durably accepted and claimable. Add `--require-wake` when a
script needs every recipient's current receipt to prove that the wake reached
`submitted` or `notified`. A legacy direct-delivery receipt without
`notification_state` may instead prove `submitted`, `delivered_verified`, or
`delivered_unverified`. Every other wake state, a missing or unknown state,
`pre_write_cause`, `wake_block`, or a delivery that needs human action exits 1.

`--require-wake` asks the daemon's bounded receipt observation to continue past
`writing`, `staged`, and `submitting` for an immediately decidable FIFO head.
It returns when the exact attempt proves `submitted` or `notified`, reaches a
terminal refusal, or hits the existing receipt cap. It does not poll and never
waits for agent work or message completion. A nonzero result does not undo
durable acceptance, so inspect the message before taking action and never use
an unkeyed resend as recovery. Reuse the same explicit client key only for an
intended exact retry.
Inspect a named block before requeueing; unchanged evidence cannot clear a
terminal pre-write block.

A position such as `2 ahead` is the recipient mailbox's FIFO position. The
daemon never bypasses an older pending message. When the oldest pending
message is not moving (`attention_required`, quota held, or blocked before
write), `cyclops messages` prints one `held queue` line naming that recipient,
the head message id, its cause, and how many wait behind it, and every
follower cell reads `N ahead · behind <id> (<cause>)`. The held-queue line
names next actions for the exact cause. It does not treat alarm clearance or
payload retrieval alone as proof that a post-write composer barrier retired.

The default CLI validates durable acceptance from the response's message id,
positive journal sequence, and deliveries array before interpreting the wake
receipts. A receipt state added by a newer daemon therefore still exits 0
after valid acceptance. Plain output prints the accepted message id and warns
that the wake receipt state is unknown to this client; JSON preserves the raw
response. An incomplete acceptance envelope exits 1 in both modes.

The body and wake use different paths. The terminal wake stages the required
two-sentence summary and one exact `inbox claim` command, never the full body.
Working state does not discard the wake. Human input or an ambiguous composer
makes it wait until the composer is proven available. A pull client may claim
the same message through the authenticated socket without changing the
composer or canceling the independently queued terminal wake. `cyclops
messages` is the authoritative combined view of mailbox and wake state.

## Receive

CLI sends and replies use this summary-bearing notification:

```text
[cyclops from implementer] The rate limiter is ready for review. Check the burst path for regressions. | cyclops inbox claim m-att_--AAAAAAQACAAAAAAAAAAQ
```

The reserved locator remains valid positional-claim input. Its 22-character
token losslessly identifies the exact current notification attempt. The daemon
atomically resolves that attempt to its message and claims it for the
authenticated recipient. A narrow pane may visually soft-wrap the notification
across terminal rows. Verification joins those rows before comparing the exact
bytes, and Cyclops never drops the summary because of pane width. Summaryless
legacy wire clients retain their versioned Format 3 and direct-payload
compatibility paths.

Both shapes are written once after proving the pane occupant and manifest. A
positive human-input reading protects that draft. For an authenticated idle or
working agent whose composer cannot be read conclusively, Cyclops prioritizes
the durable conversation: it writes the one notification and submits it. An
unknown manifest, stale process binding, copy mode, or modal prompt still
stops terminal input.

The workspace Settings card exposes `Force staged submit` for the narrower
failure where Cyclops already pasted the exact notification but normal
verification could not confirm Enter. It is on with no delay by default. The
daemon rechecks the exact recipient,
pane process generation, manifest, and tmux mode, then reserves one key with
`inbox.claim` before pressing the manifest's submit key without re-pasting the
notification. A claim, withdrawal, replacement attempt, or settled barrier
that wins before the reservation makes the timer a no-op. A later claim still
retrieves the message and may count as consumption only after the key is
accepted. A successful disable ordered before the reservation withholds the
key; a later setting change does not retract one already reserved. The durable
reservation prevents duplicate timers or a restart from
pressing a second key.

This setting intentionally bypasses composer-content proof. At 0 seconds it can
submit human input that appeared after the notification was pasted. Use it only
when immediate liveness matters more than preserving unsubmitted composer text.

## Claim a doorbell message

List pending metadata without exposing bodies:

```console
$ cyclops inbox list
m-3f9c2a admin · Review the rate limiter
```

Automation can wait for and claim the oldest pending message in one bounded,
event-driven command:

```console
$ cyclops inbox next --timeout 30s
```

Cyclops subscribes to mailbox changes before the first list, then uses the
same durable list and claim operations shown below. It claims at most one
message, does not poll, and never writes to the terminal composer. Exit `2`
means no pending message arrived before the deadline. With `--json`, that
outcome is a `timeout` object with `data.pending: false`. Every failure in JSON
mode is one object on stdout with stable `code`, `message`, and `data` fields.
Claiming through this socket command does not cancel the independent pane
notification. The daemon still stages the sender's two-sentence summary and
exact claim command when the recipient composer becomes safe.

To wait for one durable sender, copy its canonical `sender` key from
`cyclops inbox list --json` and pass it to `--from`. Labels such as
`gemini-test` are presentation text and are not accepted as endpoint keys:

```console
$ cyclops inbox next --from 'agent:<workspace-id>/<session-instance-id>/%12' --timeout 30s
```

If the claim request is sent but no usable answer arrives, JSON reports
`claim_outcome_unknown`. This includes a deadline, connection loss, or an
unreadable bounded answer. Inspect the named message before retrying because
the claim may already be durable.

Claim exactly the named message to fetch its immutable payload:

```console
$ cyclops inbox claim m-3f9c2a
[cyclops m-3f9c2a] TO: reviewer  FROM: admin  SUBJECT: Review the rate limiter
gateway.rs:120
Reply: cyclops reply m-3f9c2a --summary "First sentence. Second sentence." --body "..."
[cyclops:end m-3f9c2a]
```

A claim is authenticated to the recipient mailbox. In plain output, repeating
the claim returns the same payload. In JSON, the repeated result has
`disposition: "already_claimed"`. It does not create a second task. A claim
prints immutable acceptance-time `TO`, `FROM`, and `SUBJECT` labels around the
authenticated body, then closes the envelope with the same message id. Older
daemons omit `TO`; current clients retain the legacy header in that case. A
claim proves payload retrieval, not task completion or terminal submission. If a
claim races a doorbell already staged in the composer, Cyclops must either
submit a previously reserved terminal key or re-prove and clear that exact
doorbell. The claim alone never settles staged bytes.

A current format 3 doorbell can reach `ack_timeout` after the terminal key was
sent but before Claude paints output. A later exact recipient claim starts
reconciliation. Cyclops clears the exact staged doorbell, or proves the same
bound composer is clean, before moving the attempt to `notified` and clearing
its alarm. The claim alone changes neither the alarm nor the FIFO barrier.
Current exact `verify_failed` doorbells use the guarded automatic policy below.
Other attention causes remain operator work.

Reply using the message id so the daemon derives the recipient, thread, and
subject from the visible parent:

```console
$ cyclops reply m-3f9c2a \
    --summary "The review found one retry issue. Update the backoff path before merging." \
    --body "Reviewed. One issue in the retry path."
accepted m-a912ef
```

Use a reply or another explicit workflow fact when completion must be durable.
Pane state cannot prove which message a turn handled.

Do not run a polling loop to approximate mailbox delivery. Return to the prompt
for the normal human-visible wake. Use bounded `inbox next` only when an
automation genuinely needs to wait for and claim the durable body over the
socket; that claim does not replace or cancel the pane notification.

## The admin inbox

`admin` is a durable mailbox address even though no pane may use that label.
An agent can send to it normally:

```console
$ cyclops send admin --subject "Review needed" \
    --summary "A notification attempt is blocked. Inspect attempt n-42 before requeueing." \
    --body "Attempt n-42 is blocked."
accepted m-c82d11
✓ accepted · wake not started
```

Admin mail receives no pane notification. `cyclops status` shows the pending
admin count. A same-user shell with no agent-vendor ancestor reads it with the
same `cyclops inbox list` and `cyclops inbox claim <id>` commands, including a
shell inside a watched pane. A claim by id may take a later message; when it
does, the answer names the oldest pending message it skipped, which still
holds that mailbox's head, and `inbox next` claims oldest-first. A vendor process gets an agent identity only
through its current watched pane. Broadcast `*` targets agent panes only; name
`admin` explicitly when the operator needs a durable message.

## Broadcast, reply, and supersession

```bash
cyclops send --to implementer,reviewer --subject "Standup in 5" \
  --summary "Standup begins in five minutes. Join when your current step is safe." --fyi
cyclops send --all --subject "Rebase landed" \
  --summary "The shared rebase has landed. Refresh before starting new work." --fyi
cyclops send reviewer --subject "Corrected handoff" \
  --summary "The handoff has been corrected. Use the attached note instead of the prior message." \
  --body-file note.txt \
  --supersedes m-old
```

A broadcast is one message with one mailbox entry and one notification state
per recipient. `--all` targets every adopted agent, not admin. Supersession is
limited to one recipient and only succeeds while the old message is unclaimed
and its notification has not crossed the write boundary. History remains
append-only.

Prefer `cyclops reply <id>` to `cyclops send --subject "ignored" --reply-to <id>`.
Both use the same daemon validation. Neither accepts a recipient because the
daemon derives the exact route and subject from the referenced message. The
default reply command exits 0 after durable acceptance, like a default send.

## Attention and operator recovery

`cyclops messages` is the body-free combined view of mailbox and notification
state. `cyclops alarm preview --older-than <age>` lists unresolved notification
alarms and their exact attempt ids. `cyclops status` reports blocked panes,
legacy delivery alarms, durable mailbox attention, held queue heads, and the
admin unread count. Its eye and `waiting on you` rows summarize that combined
projection; use `alarm preview` when an operator needs the exact unresolved
notification attempts.

An ambiguous notification is never an invitation to resend blindly. Use its
exact notification attempt id:

On the ordinary automatic recovery path, Cyclops handles a current
exact-attempt `verify_failed` doorbell only when the complete durable binding
and exact composer bytes still match. It submits once while the mailbox is
pending. If the exact recipient claimed after the write, it clears that
doorbell without submitting it. Durable intent blocks a second terminal key
after an uncertain outcome. Human, trailing, changed, or unprovable content
remains one visible attention item.

An administrator can turn off the automatic force-submit control, stored as
`force_notification_submit` and exposed through
`notification.force_submit.set`; see [install.md](install.md) for the persisted
configuration. It never pastes or replaces composer bytes, but it may send one
submit key for one exact `verify_failed` attempt without composer-content proof
and may therefore submit trailing human input. It is a deliberate liveness
tradeoff, not an ordinary recovery path.

```bash
cyclops attention show <attempt-id> --diff
cyclops attention complete <attempt-id>
cyclops attention discard <attempt-id>
```

`show` is read-only and available to the workspace administrator or the exact
durable recipient of that attempt. `complete` and `discard` remain
administrator-only and recheck the exact attempt,
terminal layout, process generations, manifest, and current terminal safety.
An uncertain result must be inspected and must not be repeated as a fresh
terminal action. Reconciliation never sends a second key. `cyclops messages`
shows whether intent, terminal acceptance, and notification consumption were
proven. Alarm clearance acknowledges the alarm but does not abandon an
unfinished terminal action. The message remains claimable through `inbox
claim`. The exact transition and reconciliation rules are owned by the
[protocol reference](../reference/PROTOCOL.md#mailbox-and-notification-control).

A wake that stops before writing reports one exact cause, including
`session_unavailable`, `manifest_unavailable`, `payload_unavailable`,
`write_readiness_changed`, `spool_failed`, `paste_command_unwritten`,
`binding_unprovable`, `composer_semantic_missing`, or `worker_failed`. The
message remains claimable.
Current summary-bearing Format 4 notifications may visually soft-wrap in a
narrow pane. Their summary is never removed because of pane width.
A workspace administrator can release the FIFO without touching the pane:

```bash
cyclops notification withdraw <attempt-id> --recipient <recipient-key>
```

`show --diff` returns the exact selected transport bytes to the authenticated
workspace administrator or that attempt's exact durable recipient. A direct
fallback diff therefore contains the message payload. Other recipients receive
the same denial for missing and unauthorized ids. The daemon does not write
those diff inputs to its journal or log.

`cyclops requeue <message-id>` is an explicit operator action for a notification
that is still eligible. `cyclops alarm clear <attempt-id>...` appends explicit
clearances to the content-free notification alarm register. A clearance
acknowledges; it retires nothing. Under each cleared id the command prints the
attempt's state and cause at clearance time, the message and recipient the
clearance did not change, and the available next actions. The recipient
retrieves the durable payload with `inbox claim`, or the administrator inspects
the exact attempt with `attention show --diff` and uses `complete` or `discard`
only when its checks authorize the action. Neither clearance nor payload
retrieval alone proves that a post-write composer barrier retired.

For an age-selected set, `cyclops alarm clear --older-than <age>` previews and
prints the exact ids, then requires typing `clear` at a confirmation that names
the count and cutoff.

The clear request contains only those frozen ids, so a newer alarm cannot enter
the operation. There is no clear-all form. Scripts use `alarm preview --json`
and pass its ids explicitly rather than using the interactive age form. None of
these commands changes or deletes the message.

## Direct delivery records

Current compatibility fallback and older direct records can show a full payload
ending in `[cyclops:end <id>]`. Current mailbox fallback settles as
`delivered_direct` and carries no claimant. Older session-ledger delivery may
show `delivered_verified` or `delivered_unverified`; those receipt tiers are not
the `msg.send` mailbox contract.

## Exit and waiting semantics

- `0`: the send or reply was accepted, or the idempotency key named an existing
  accepted message; a default command does not require terminal wake proof
- `1`: no success response was received; with `--require-wake`, it can also
  mean the message was accepted but at least one current receipt did not prove
  a submitted or notified wake. This is a bounded evaluation, not another
  wait. Inspect current state and do not issue an unkeyed resend. Reuse the same
  explicit client key only for an intended exact retry because a response can
  be lost after durable acceptance
- `2`: local command usage was invalid

`cyclops send` does not wait for task completion. `cyclops wait <target>
--until idle|turn-ended|blocked` observes a pane, not a message. See
[wait.md](wait.md).
