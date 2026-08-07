# Messaging research audit

Date: 2026-08-07

This directory contains three parallel audits of Cyclops delivery, hook
installation, and comparable agent-control systems:

- [Cyclops messaging and delivery audit](cyclops-delivery-audit.md)
- [Hook installation audit](hook-installation-audit.md)
- [Competitive messaging audit](competitive-messaging-audit.md)

## Executive answer

The reported messaging trouble includes one confirmed Cyclops defect and two
separate sources of confusion.

**Confirmed defect:** Cyclops issued two successful tmux paste commands for
each of three local multiline Codex messages. Codex collapsed the pasted text,
which hid the exact message ID. Cyclops's generic `Pasted` verifier could not
use Codex's escaped-only `idle_with_input` rule, returned `verify_failed`, and
used the default retry. The retry gate then misclassified the occupied
composer as empty or ghost text and pasted again.

**Separate observation:** the likely named Codex ping, `m-8f9e3e`, has one
`pasting` attempt in the ledger followed by `staged`, `submitted`, and
`delivered_unverified`. Its apparent duplicate was not created by the daemon's
retry loop. A pane recording or lower-level command trace would be needed to
explain that exact display.

**Blocked-looking sends:** Cyclops preserves one FIFO per target pane. A head
message held by target input, an unknown state, a modal, a detach, or work in
progress holds every later message. Old `attention_required` records also stay
visible even though they no longer occupy the worker. Both can look like the
sender is blocked; neither is evidence that the socket rejected `msg.send`.

## Decisions

### Stop blind retries after terminal writes

The current retry policy groups together failures with very different
certainty. A pre-write spool failure is safe to retry. `verify_failed`, a
tmux timeout after a paste command was written, `submit_failed`, and
`ack_timeout` are ambiguous: the terminal may already have acted.

Immediate operational mitigation: set `delivery_retry_max = 0` until retry
outcomes are classified. This trades an automatic second attempt for an
honest `attention_required` result.

The product fix should preserve automatic retry only for proven pre-write
failures. Once a paste may have happened, Cyclops should reconcile evidence or
return `outcome_unknown`; it should not paste the payload again.

### Fix Codex collapsed-composer detection

Record scrubbed plain and escaped captures from a multiline paste on the
current supported Codex version. Add collapsed-paste fixtures and a
high-priority occupied-composer rule. Composer verification and fusion must
share the same plain/escaped rule vocabulary so the retry gate cannot call
staged input a ghost suggestion.

The regression must assert the irreversible boundary directly: one logical
delivery issues at most one paste command when the first paste succeeded but
readback is inconclusive.

### Automate hooks after identity is label-free

Hooks should be easier to install, but `scripts/install.sh` should not silently
edit vendor configuration in the current design.

Every generated hook command hard-codes `--agent <label>`. A global Codex hook
for one label is wrong in another Codex pane, and the installer runs before
live panes have labels. Vendor hook files on this machine also already contain
unrelated configuration; the current copy instructions could overwrite it.

First make the hook's agent field optional and let the daemon derive the pane
and current label from Unix-socket peer credentials and process ancestry. Then
add an explicit post-start setup manager that previews a semantic merge,
backs up and atomically writes, records exactly what Cyclops owns, supports
surgical uninstall, handles vendor trust/restart states, and finishes with a
real hook self-test.

Hooks improve post-submit evidence. They do not fix the confirmed pre-submit
double paste, because the ACK hook runs only after Enter.

### Add socket-level idempotency

The CLI does not automatically retry `msg.send`, but an agent or script may
retry after losing the response. Today a second request mints a second message
ID. Add a caller-generated idempotency key, persist its mapping before
delivery, return the original result for an equivalent replay, and reject the
same key with a different payload.

This prevents a lost socket response from becoming two logical messages. It
is complementary to—not a replacement for—safe paste/submit state handling.

## What the comparison teaches

No audited competitor combines Cyclops's intended properties in one system.

| Product | Useful lesson | Limit to keep clear |
| --- | --- | --- |
| Herdr | Verify the expected agent still owns the pane; pin waits to that target | A prompt request ID and lifecycle wait are not durable message verification or retry idempotency |
| cmux | Return partial success when paste succeeded but submit failed, specifically to prevent double paste | Terminal `sent` or `queued` is not recipient acknowledgement |
| CAO | A durable, idle-gated FIFO is valuable | Writing `DELIVERED` before terminal input avoids one race but creates false-delivered crash ambiguity |
| Gas Town | Persist mail before best-effort wake-up and make the recipient-side ACK idempotent | Its ACK proves hook handoff of a reminder, not full-body model consumption |

The defensible Cyclops claim is progressive, named evidence: durable
acceptance, safe staging, exact-ID readback, submission, and an
origin-validated hook acknowledgement. It should not claim that a hook proves
the model understood or completed the request.

Herdr was audited from pinned public source only. This process was not inside a
Herdr-managed pane, so no live Herdr session was queried or controlled.

## Recommended work order

1. Set the default retry policy to avoid repeating ambiguous writes and add
   the collapsed-Codex regression.
2. Align escaped composer detection between verification and fusion; re-probe
   current Claude and Codex versions.
3. Persist separate attempt facts for paste accepted, exact ID staged, submit
   accepted, and hook acknowledged.
4. Add socket request idempotency.
5. Derive hook identity from the verified socket peer, then build reversible
   `setup`, `status`, `doctor`, and `remove` flows for Claude and Codex first.
6. Distinguish `queued`, `held`, and `outcome_unknown` in receipts and status;
   add an append-only operator acknowledgement for obsolete attention items.

## Validation

The audit changed research Markdown only. The local incident review inspected
message metadata and state transitions without copying message bodies or pane
captures into the reports.

Thirty-seven unique focused tests across delivery blockers, retry/detach
fixes, hooks, the core M1 path, and shipped manifest rules passed. Their success
is consistent with the defect: none currently covers a collapsed multiline
Codex paste followed by verification failure.
