---
name: cyclops
description: Send, claim, and reply to durable messages between coding agents using Cyclops. Use for cross-agent coordination, an incoming Cyclops pane notification, or Cyclops MCP tools. Use the host's own delegation tools for subagents created within the current task.
---

# Cyclops

Cyclops stores the full message durably. For a recipient in a watched tmux
pane, it separately writes a short notification and presses Enter. A headless
recipient has a mailbox without a pane; a native application needs an explicitly
configured connector for push notifications.

## Receive and reply

When a notification includes `cyclops inbox claim m-att_...`, run that exact
command once. Read the returned envelope and body, then reply to its canonical
message ID:

```bash
cyclops inbox claim m-att_... --json
cyclops reply <message-id> --body "Your answer"
```

You can also reply using the exact attempt locator, or `reply --last` when the
last claimed message is unambiguous. Prefer the explicit ID when working on
several conversations. Use `--body-file <file>` or `--body-file -` for multiline
text. A summary is optional, not a required two-sentence ritual.

The pane preview is a notification, not the authoritative task body. Do not
claim another agent's locator just because you can see it. A repeated claim of
your own message returns the same payload; do not repeat the work.

## Discover and send

Use `cyclops list` to find peers. Inside tmux it is session-scoped;
`cyclops list --all` includes other watched sessions. Use `cyclops status`
when you need daemon and delivery diagnostics, not before every command.

Name yourself with `cyclops name <label> --self`. Inside tmux this names your
pane; outside tmux it registers your proven agent process as headless. Do not
invent a pane ID or another agent's identity.

```bash
cyclops send reviewer --subject "Review the change" --body "Please check the patch."
cyclops send reviewer,implementer --subject "Decision" --body "Keep the existing API."
cyclops reply <message-id> --summary "Tests passed; one issue remains." --body-file notes.md
```

Keep an optional `--summary` short and useful. If omitted, Cyclops derives it
from the subject, never from the message body. Reply derives its recipient,
thread, and subject from the parent message.

Message IDs currently use `cyc-<thread>-<message>`; older `m-...` IDs remain
valid. Treat either as an opaque identifier. A label is for addressing peers,
not a durable identity key.

## What a receipt means

- **Accepted:** the full message is durably stored.
- **Notification queued, submitted, or held:** the separate wake attempt's state.
- **Claimed:** the authenticated recipient retrieved the full message.
- **Reply:** evidence of a response, not proof that every requested task succeeded.

Do not describe acceptance, Enter, or a visible preview as task completion.
Use the recipient's actual reply or an explicitly agreed completion signal.

A lost response can mean the request committed. Inspect `cyclops messages`
before retrying. If retrying an uncertain send or reply, reuse the same explicit
`--client-key`. An unkeyed retry can create a duplicate.

## Pane delivery is independent of reading the mailbox

For a bound, live agent, working versus idle and focused versus unfocused do
not block normal notification delivery. An inconclusive composer reading is
not proof of a human draft. Cyclops records uncertainty and still attempts the
doorbell and Enter.

Positive evidence of unrelated composer text is a real hold. So are concrete
wrong-target or terminal blocks, such as a dead/replaced pane, copy mode, a
permission prompt, or a quota screen. Do not erase a user's draft or bypass
those blocks from your own shell.

Reading a new message through the daemon socket does not cancel, delay, or
replace its owed pane notification. A reply gets the same notification path as
a send, even while the original sender is working. A later pane notification
for an already claimed message is a receipt cue, not another task.

For bounded automation, or when the user asks you to wait for a reply:

```bash
cyclops inbox next --timeout 30s
```

This subscribes to inbox events, claims at most one message, and exits 2 on
timeout. It is not polling and is not a pane-wake command. Do not repeat it
indefinitely or use it as a workaround for a broken wake. Interactive agents
can normally finish their current turn and let the direct notification arrive.

Use `inbox next --from <recipient-key>` to filter by the canonical sender key
from structured inbox metadata, not a display name. Headless automation may
use `inbox next --wait`; it ends if the daemon connection closes. An uncertain
claim result is not proof that the message remains unclaimed.

## Native applications and MCP

An explicitly configured `cyclops mcp` process exposes send, reply, inbox,
claim, and thread tools through the same daemon. Use those tools when your host
provides them; their identity is established by Cyclops, not by a supplied
sender name.

One bridge process is one participant. A host sharing one MCP server across
multiple chats also shares that participant. Do not claim per-chat isolation
unless the host starts a separate bridge for each conversation.

MCP tools alone do not wake a native application's chat. Only a configured push
connector can do that, subject to that application's delivery semantics.
Never claim a native notification was delivered based solely on mailbox
acceptance. Do not modify app configuration, create sessions, or enable a
connector without the user's request.

## Diagnose and recover

Start with `cyclops messages`: it separates mailbox claims from notification
state. Use `cyclops read <agent> --source detection --raw` for the actual
sensor evidence, or `cyclops thread <message-id>` for the conversation.

A held draft is different from a failed physical write. If a paste or Enter
has an uncertain outcome, inspect that exact attempt before retrying. Do not
turn a diagnostic session into repeated sends, requeues, or raw injection.
Operator-only commands remain operator-only; a denied inspection is not
permission to impersonate the operator.

`cyclops wait <agent> --until idle|turn-ended|blocked --timeout 90s` observes
pane activity. It is not a completion receipt for a particular message.
Prefer event waits to loops of status checks or guessed sleeps.

When the user asks to quiet old queued notifications, `cyclops clear <agent>`
withdraws unsubmitted wakes while keeping the durable messages.
`cyclops stop` stops Cyclops and its daemon without killing unrelated tmux
panes. Neither command erases history. Full removal requires an explicit
uninstall or the removal command's own confirmed scope.

## Boundaries

- Use Cyclops, not direct `tmux send-keys` or paste commands, for peer messages.
- `--raw` pastes the full body and bypasses composer checks. It is not an
  automatic fallback for a delay, hold, or uncertain attempt. Use it only when
  the operator explicitly authorizes that bypass and understands its effect.
- Never write pane titles; they are sensors. Use `cyclops name` for labels.
- Journals are append-only. Corrections and withdrawals append facts; they do
  not rewrite prior messages.
- If a command or flag is unfamiliar, check its specific `--help`. Do not
  repeatedly reload all command documentation before ordinary messaging.
