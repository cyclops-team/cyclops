# Send

`cyclops send` records one immutable message in each recipient's durable
mailbox and returns after acceptance. Terminal delivery is asynchronous: one
doorbell line, the summary beside the exact claim command, goes through the
gate and into the recipient's pane. The full body stays in the authenticated
mailbox and reaches a pane only through `--raw`.

## Send

```text
cyclops send <agent>[,<agent>...] --subject <s> [--summary <one line>] \
  [--body <b> | --body-file <path|->] [--raw] [--fyi] [--reply-to <id>] \
  [--client-key <k>]
```

```console
$ cyclops send reviewer --subject "Review the rate limiter" \
    --summary "The rate limiter is ready for review." \
    --body "gateway.rs:120"
accepted m-3f9c2a
✓ accepted · wake queued
```

Message ids are abbreviated in prose examples. The daemon emits `m-` followed
by the full 32 lowercase hexadecimal UUID digits.

`--summary` is optional. When you give one it must be one non-empty line of
at most 240 characters. When you omit it the daemon derives the preview from
the subject (the first line of the body only when the subject is blank), so a
body never reaches a pane by accident. Use `--body-file -` for stdin,
`--client-key <key>` for an exact retry, and `--fyi` for an announcement that
should not be answered. A successful result proves the message is durable.
It does not prove that the recipient claimed it or completed the work.

If the connection drops before the response arrives, the request may already
be durable. Inspect the mailbox or message history first. Repeat a send or reply
only with the same explicit `--client-key`; an unkeyed retry can create a second
message.

The receipt reports two separate facts:

- `accepted` means the durable message and recipient mailbox entries exist.
- `wake <state>` reports the current doorbell attempt. It may be
  `not started`, `queued`, `checking readiness`, `writing`, `submitted`,
  `submitted (unverified)`, `notified`, `needs attention`, or `superseded`.

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
`submitted` or `notified`. Every other wake state, a missing or unknown state,
`pre_write_cause`, or `wake_block` exits 1.

`--require-wake` asks the daemon's bounded receipt observation to continue past
`writing` for an immediately decidable FIFO head. It returns when the exact
attempt proves `submitted` or `notified`, reaches a terminal refusal, or hits
the existing receipt cap. It does not poll and never waits for agent work or
message completion. A nonzero result does not undo durable acceptance, so
inspect the message before taking action and never use an unkeyed resend as
recovery.

A position such as `2 ahead` is the recipient mailbox's FIFO position. The
daemon never bypasses an older pending message. When the oldest pending
message is not moving (`attention_required` or blocked before write),
`cyclops messages` prints one `held queue` line naming that recipient, the
head message id, its cause, and how many wait behind it, and every follower
cell reads `N ahead · behind <id> (<cause>)`.

The default CLI validates durable acceptance from the response's message id,
positive journal sequence, and deliveries array before interpreting the wake
receipts. A receipt state added by a newer daemon therefore still exits 0
after valid acceptance. Plain output prints the accepted message id and warns
that the wake receipt state is unknown to this client; JSON preserves the raw
response. An incomplete acceptance envelope exits 1 in both modes.

## Receive

The doorbell is one line:

```text
[cyclops from implementer] The rate limiter is ready for review. | cyclops inbox claim m-att_--AAAAAAQACAAAAAAAAAAQ
```

The 22-character token identifies the exact current notification attempt.
The daemon atomically resolves that attempt to its message and claims it for
the authenticated recipient. A narrow pane may visually soft-wrap the line
across terminal rows; the readback joins those rows before comparing the
exact bytes, and Cyclops never drops the summary because of pane width.

The line is written and submitted for a bound, live agent process unless a
human draft is positively observed or a named block is on screen: a modal, a
permission prompt, a quota screen, a dead pane, copy-mode, or a doorbell the
recipient has not consumed. A working agent gets the line during its turn; the
vendor queues it. A composer Cyclops cannot read does not hold the line; the
journal marks that attempt `submitted (unverified)` when the paste could not
be read back exactly. Enter is pressed once and never again. A hook receipt,
screen evidence, or the recipient's claim settles the attempt as `notified`;
no receipt within five seconds settles it as `notified` too, with no
verifier.

The human-draft guard depends on the recipient's manifest recognizing typed
text. The five measured manifests do. The seven unverified manifests carry no
composer rule, so a doorbell to one of those panes never detects a draft and
is effectively a raw write with a receipt.

## The raw transport

```console
$ cyclops send reviewer --subject "Cyclops cannot read your composer" \
    --body "Reply with cyclops reply when you see this." --raw
```

`--raw` pastes the whole rendered message (header, body, reply hint, and the
`[cyclops:end <id>]` marker) into the recipient pane and presses Enter with no
composer check. The gate still requires the pane to be present and alive. Use
it only when Cyclops itself is broken for that pane or the recipient is an
unverified vendor. The journal records the attempt as an unverified raw write
(`transport: raw`, no binding, no verifier), so it never passes for a gated
delivery, and Cyclops never selects it on its own. `cyclops reply --raw` does
the same for a reply.

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
Claiming through this socket command does not cancel the independent doorbell:
a claim before the write leaves it queued and the worker still writes it, and
a claim after Enter is the receipt.

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
Reply: cyclops reply m-3f9c2a --body "..."
[cyclops:end m-3f9c2a]
```

A claim is authenticated to the recipient mailbox. In plain output, repeating
the claim returns the same payload. In JSON, the repeated result has
`disposition: "already_claimed"`. It does not create a second task. A claim
prints immutable acceptance-time `TO`, `FROM`, and `SUBJECT` labels around the
authenticated body, then closes the envelope with the same message id. Older
daemons omit `TO`; current clients retain the legacy header in that case. A
claim proves payload retrieval, not task completion.

Reply using the message id, the `m-att_...` token, or `--last` for the most
recently claimed message, so the daemon derives the recipient, thread, and
subject from the visible parent:

```text
cyclops reply [<message-id> | <m-att_...>] [--last] [--summary <one line>] \
  [--body <b> | --body-file <path|->] [--raw] [--client-key <k>]
```

```console
$ cyclops reply m-3f9c2a \
    --body "Reviewed. One issue in the retry path."
accepted m-a912ef
```

Use a reply or another explicit workflow fact when completion must be durable.
Pane state cannot prove which message a turn handled.

Do not run a polling loop to approximate mailbox delivery. Return to the prompt
for the normal human-visible doorbell. Use bounded `inbox next` only when an
automation genuinely needs to wait for and claim the durable body over the
socket; that claim does not replace or cancel the doorbell.

## The admin inbox

`admin` is a durable mailbox address even though no pane may use that label.
An agent can send to it normally:

```console
$ cyclops send admin --subject "Review needed" \
    --summary "A notification attempt needs a human before requeueing." \
    --body "Attempt n-42 needs attention."
accepted m-c82d11
✓ accepted · wake not started
```

Admin mail receives no doorbell. `cyclops status` shows the pending admin
count. A same-user shell with no agent-vendor ancestor reads it with the same
`cyclops inbox list` and `cyclops inbox claim <id>` commands, including a
shell inside a watched pane. A claim by id may take a later message; when it
does, the answer names the oldest pending message it skipped, which still
holds that mailbox's head, and `inbox next` claims oldest-first. A vendor
process gets an agent identity only through its current watched pane.
Broadcast `*` targets agent panes only; name `admin` explicitly when the
operator needs a durable message.

## Broadcast, reply, and supersession

```bash
cyclops send --to implementer,reviewer --subject "Standup in 5" --fyi
cyclops send --all --subject "Rebase landed" \
  --summary "The shared rebase has landed; refresh before starting new work." --fyi
cyclops send reviewer --subject "Corrected handoff" \
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
durable mailbox attention, held queue heads, and the admin unread count. Its
eye and `waiting on you` rows summarize that combined projection.

An attempt reaches `needs attention` only for a physical write failure after
the paste (`paste_failed`, `submit_failed`, `pane_rebound_after_paste`,
`transport_outcome_unknown`) or because the daemon restarted between the
paste and its receipt (`daemon_restart`). It means the terminal outcome is
unknown, not that the recipient did not get the line. Look at the pane. If
the line is not there and the recipient has not claimed, run:

```bash
cyclops requeue <message-id>
```

Requeue starts a fresh attempt through the ordinary gate. It is an explicit
operator action after the cause is understood; nothing requeues on a timer,
and the daemon refuses it for any state other than `attention_required`.

A wake that stops before writing reports one exact cause: `session_unavailable`,
`manifest_unavailable`, `payload_unavailable`, `write_readiness_changed`,
`spool_failed`, `paste_command_unwritten`, `binding_unprovable`, or
`worker_failed`. Nothing was written and the message remains claimable. A
workspace administrator can release the FIFO without touching the pane:

```bash
cyclops notification withdraw <attempt-id> --recipient <recipient-key>
cyclops clear <agent>
```

`notification withdraw` cancels one exact attempt that is still `queued`,
`gating`, or blocked before write; `clear <agent>` does that for every
unwritten attempt to one recipient. Both leave the messages pending and
claimable. An attempt that has written refuses.

`cyclops alarm clear <attempt-id>...` appends explicit clearances to the
content-free notification alarm register. A clearance acknowledges; it
retires nothing. For an age-selected set, `cyclops alarm clear --older-than
<age>` previews and prints the exact ids, then requires typing `clear` at a
confirmation that names the count and cutoff. The clear request contains only
those frozen ids, so a newer alarm cannot enter the operation. There is no
clear-all form. Scripts use `alarm preview --json` and pass its ids
explicitly. None of these commands changes or deletes the message.

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
