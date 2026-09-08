# Connect a native coding app

Cyclops now exposes five local MCP tools: send, reply, inbox, claim, and
thread. Run the app's MCP server as `cyclops mcp`. Start the Cyclops daemon
first. The adapter never starts it, changes app settings, or grants an agent
administrator access.

This is tool access, not a promise that every native app wakes itself.

| Surface | Built here | What is not established |
|---|---|---|
| A host that launches a session-owned stdio MCP process | Authenticated messaging tools, a distinct process-bound participant, scoped claims and replies | Live vendor acceptance still needs a manual app test |
| A host that shares one MCP process across chats | One shared participant | Separate chat identities; do not label the same server as several agents |
| Claude Code terminal CLI with Channels enabled | Opt-in event-driven channel notices, plus the same tools | Desktop or VS Code push; immediate interruption while busy |
| Antigravity native-app sidecar | Dry-run and explicitly executed `agentapi` transport probe | End-to-end authenticated conversation routing and live receipt evidence |
| Codex native app | MCP tool adapter | Attaching an independently launched app-server to an already running desktop task |

## MCP setup

Configure a stdio MCP server using the app's own settings. A typical server
entry has this shape; the outer configuration file differs by vendor:

```json
{
  "command": "cyclops",
  "args": ["mcp"]
}
```

The default label is `mcp-<process id>`. Use `--label reviewer` only when
that server belongs to one known conversation; two active processes cannot
take the same label. `--manifest` is an optional pin verified against the
launching vendor, not a way to impersonate one. The daemon must recognize
the actual process ancestry. An unsupported launcher is refused instead of
running its tools as the operator.

Each session-owned bridge registers its own PID and process birth, while
the daemon separately verifies the vendor that launched it. Sibling
bridges do not inherit each other's mailbox. If the vendor exits, a
surviving bridge is denied rather than promoted to administrator. If the
bridge exits, its route retires; old messages remain in the journal and
are not handed to a replacement process. A shared app-wide server still
represents one participant, not an invented per-conversation identity.

The tools are `cyclops_send`, `cyclops_reply`, `cyclops_inbox`,
`cyclops_claim`, and `cyclops_thread`. Claim an exact locator before acting
on its body. Reply with the canonical `message_id` returned by that claim.
No tool accepts a sender identity, arbitrary daemon method, raw terminal
write, deletion, requeue, or administrative read. Unknown tool arguments
are refused. A transport timeout stays uncertain and is never retried by
the adapter.

Stdio is JSON-RPC only, one object per line, with a 1,048,576-byte object
limit excluding the newline. Diagnostics go to stderr. The adapter uses
the existing Cyclops daemon transport and domain operations rather than a
second message store. A host that stops reading stdout closes protocol
output after a two-second frame deadline; partial output is never retried.

## Claude Code Channels: opt in, terminal only

Use `cyclops mcp --claude-channel` as the server command arguments, then
enable the named server using Claude Code's documented Channels setup.
Channels is a research-preview feature with allowlist and organization
policy requirements; custom local development channels require the vendor's
explicit development-channel opt-in. Installing an ordinary MCP server
does not enable it. See the [official Channels reference](https://code.claude.com/docs/en/channels-reference)
and [platform support table](https://code.claude.com/docs/en/platforms).

After MCP initialization, Cyclops subscribes to message changes and sends
body-free "notifications/claude/channel" notices containing the exact
message ID. It lists pending messages only on startup or a change event,
never on a polling timer, and never claims a message on the host's behalf.
Closing stdio cancels and joins the event reader. A broken event stream
stops the channel with a diagnostic; mailbox contents are retained.

Claude controls scheduling: while it is busy, channel events wait for its
next turn. A successful notification write does not prove Claude processed
it, and a disabled channel can ignore it. The authenticated claim remains
the receipt. This does not promise channel support in Claude Desktop or
the VS Code extension. Protocol and teardown tests are isolated; a live
vendor test has not been performed for this release candidate.

## Antigravity sidecar probe

The [official Antigravity sidecar API](https://antigravity.google/docs/sidecars/)
documents `agentapi send-message` with an explicit conversation ID. The
probe in [antigravity_sidecar_probe.py](../../scripts/native/antigravity_sidecar_probe.py)
prints the exact command by default:

```bash
python3 scripts/native/antigravity_sidecar_probe.py --conversation conversation-id --message-id cyc-01234567-89abcdef
```

Only `--execute`, inside an explicitly enabled sidecar environment, invokes
the API. The probe sends a locator, not a message body; it never retries.
An API success is not reported as a claim or processing acknowledgement.
The [mock tests](../../scripts/native/test_native_adapters.py) cover command
arguments and uncertainty without opening a real app.

This is a transport experiment, not an installed connector. Before shipping
automatic routing, prove that native conversation IDs map to distinct
authenticated participants, test idle and busy conversations separately,
and test sidecar restart without routing a message to another conversation.
The app-wide MCP process alone cannot establish that binding.

## Codex: tools now, owned-runtime experiment later

Codex supports [MCP tools](https://learn.chatgpt.com/docs/extend/mcp).
Its [app-server](https://learn.chatgpt.com/docs/app-server) documents thread
and turn operations for integrations that own that runtime. Those methods
do not by themselves establish that a second process can attach to an
existing desktop-owned conversation. No private GUI control or synthetic
keystrokes are used here. Validate that ownership boundary before claiming
automatic native Codex wake support.
