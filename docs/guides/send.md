# Send

`cyclops send` records one immutable message in each recipient's durable
mailbox and returns after acceptance. Terminal delivery is asynchronous. A
recipient with the exact shipped claim skill gets a content-free doorbell. A
recipient without that proof gets the full canonical payload through the same
safe terminal gate.

## Send

```console
$ cyclops send reviewer --subject "Review the rate limiter" --body "gateway.rs:120"
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

A position such as `2 ahead` is the recipient mailbox's FIFO position. The
daemon never bypasses an older pending message. When the oldest pending
message is not moving (`attention_required`, quota held, or blocked before
write), `cyclops messages` prints one `held queue` line naming that recipient,
the head message id, its cause, and how many wait behind it, and every
follower cell reads `N ahead · behind <id> (<cause>)`.

The body and wake use different paths. Doorbell mode never pastes the message
body. A terminal wake stages one `inbox claim` command, while a pull client may
claim the same message through the authenticated socket without changing the
composer. Both are valid. `cyclops messages` is the authoritative combined
view of mailbox and wake state.

## Receive

The preferred path is this content-free notification:

```text
cyclops inbox claim m-att_--AAAAAAQACAAAAAAAAAAQ
```

The daemon selects it only when `cyclops setup check` reports `mailbox
doorbell`. The reserved locator remains valid positional-claim input for older
clients. Its 22-character token losslessly identifies the exact current
notification attempt. The daemon atomically resolves that attempt to its
message and claims it for the authenticated recipient.

If setup reports `mailbox direct payload`, Cyclops writes the full message
envelope instead. This compatibility path exists for an absent, edited,
outdated, unreadable, or changed claim skill. A successful direct delivery is
recorded as `delivered_direct`, not as a claim, and the recipient does not run
`inbox claim` for that message.

Both shapes are written once, only after proving the pane occupant, manifest,
and clean composer. An ambiguous write or submit raises attention. Cyclops does
not write either shape again automatically.

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

To wait for one durable sender, copy its canonical `sender` key from
`cyclops inbox list --json` and pass it to `--from`. Labels such as
`gemini-test` are presentation text and are not accepted as endpoint keys:

```console
$ cyclops inbox next --from 'agent:<workspace-id>/<session-instance-id>/%12' --timeout 30s
```

If the claim request is sent but its answer misses the deadline, JSON reports
`claim_outcome_unknown`. Inspect the named message before retrying because the
claim may already be durable.

Claim exactly the named message to fetch its immutable payload:

```console
$ cyclops inbox claim m-3f9c2a
[cyclops m-3f9c2a] FROM: admin  SUBJECT: Review the rate limiter
gateway.rs:120
Reply: cyclops reply m-3f9c2a --body "..."
```

A claim is authenticated to the recipient mailbox. In plain output, repeating
the claim returns the same payload. In JSON, the repeated result has
`disposition: "already_claimed"`. It does not create a second task. A claim
proves payload retrieval, not task completion or terminal submission. If a
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
$ cyclops reply m-3f9c2a --body "Reviewed. One issue in the retry path."
accepted m-a912ef
```

Use a reply or another explicit workflow fact when completion must be durable.
Pane state cannot prove which message a turn handled.

Do not run a foreground `cyclops watch` or polling loop to wait for a
notification that must be written into the same pane. The wait makes the pane
working, which safely gates the write and creates a circular wait. Return to
the prompt for the normal one-line wake, or use bounded `inbox next` to pull
and claim through the socket.

## The admin inbox

`admin` is a durable mailbox address even though no pane may use that label.
An agent can send to it normally:

```console
$ cyclops send admin --subject "Review needed" --body "Attempt n-42 is blocked."
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
cyclops send --to implementer,reviewer --subject "Standup in 5" --fyi
cyclops send --all --subject "Rebase landed" --fyi
cyclops send reviewer --subject "Corrected handoff" --body-file note.txt \
  --supersedes m-old
```

A broadcast is one message with one mailbox entry and one notification state
per recipient. `--all` targets every adopted agent, not admin. Supersession is
limited to one recipient and only succeeds while the old message is unclaimed
and its notification has not crossed the write boundary. History remains
append-only.

Prefer `cyclops reply <id>` to `cyclops send --subject "ignored" --reply-to <id>`.
Both use the same daemon validation. Neither accepts a recipient because the
daemon derives the exact route and subject from the referenced message.

## Attention and operator recovery

`cyclops messages` is the body-free combined view of mailbox and notification
state. `cyclops alarm preview --older-than <age>` lists unresolved notification
alarms and their exact attempt ids. `cyclops status` reports pane and legacy
delivery attention plus the admin unread count; it is not the mailbox alarm
source.

An ambiguous notification is never an invitation to resend blindly. Use its
exact notification attempt id:

Cyclops automatically handles a current exact-attempt `verify_failed` doorbell only
when the complete durable binding and exact composer bytes still match. It
submits once while the mailbox is pending. If the exact recipient claimed after
the write, it clears that doorbell without submitting it. Durable intent blocks
a second terminal key after an uncertain outcome. Human, trailing, changed, or
unprovable content remains one visible attention item.

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
`write_readiness_changed`, `spool_failed`, `binding_unprovable`,
`composer_semantic_missing`, or `worker_failed`. The message remains claimable.
For format 3, `write_readiness_changed` with an observed width below its
recorded required width is shown as `pane too narrow`. The width pair is
content-free, no pane write occurred, and a later size edge may reopen that
same attempt once.
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
attempt's state and cause, the message and recipient it still holds, and the
two actions that release it: a recipient claim, or `attention show --diff`
followed by `complete` or `discard` (or `requeue` after the cause is fixed).
At daemon start, every open attempt that no automatic reconciliation can
retire is listed once to the operator with the same two actions. For an age-selected
set, `cyclops alarm clear --older-than <age>` previews and prints the exact ids,
then requires typing `clear` at a confirmation that names the count and cutoff.
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

- `0`: the message was accepted, or the idempotency key named an existing
  accepted message
- `1`: no success response was received. The daemon may have refused the
  request, or the response may have been lost after durable acceptance. Inspect
  current state and reuse the same explicit client key for any exact retry
- `2`: local command usage was invalid

`cyclops send` does not wait for task completion. `cyclops wait <target>
--until idle|done|blocked` observes a pane, not a message. See
[wait.md](wait.md).
