# Competitive messaging audit

Research date: 2026-08-07

This audit compares the messaging and terminal-control paths in Herdr, cmux,
CLI Agent Orchestrator (CAO), and Gas Town. It focuses on the questions that
matter to Cyclops: what is durable, what is merely typed, what is verified,
what can be retried safely, and what a hook actually proves.

## Source and confidence policy

The audit is source-based rather than a live interoperability test. Each
project was inspected at one exact upstream revision:

| Product | Version | Audited revision |
| --- | --- | --- |
| Herdr | 0.8.0 | [`7d77e927`](https://github.com/herdrdev/herdr/tree/7d77e927eba265ea864485bf99411abcf62d977d) |
| cmux | 0.64.22, nightly branch | [`6089fa04`](https://github.com/manaflow-ai/cmux/tree/6089fa04d3effd27e43c5c6104a4eada62fe859f) |
| CLI Agent Orchestrator | 2.4.1 | [`e592b21e`](https://github.com/awslabs/cli-agent-orchestrator/tree/e592b21e170d005b26b4cb7bed702fbf3bf7037f) |
| Gas Town | 1.2.1 | [`649b832b`](https://github.com/gastownhall/gastown/tree/649b832b7672bc7a2dbef26f5983aba6198b819b) |

“Evidence” below means the behavior is explicit in source or official
documentation. “Inference” means it follows from the ordering or missing
protocol fields, but was not demonstrated by a fault-injection test. Negative
claims are deliberately narrow: “not found in the inspected send path” is not
the same as proving that a feature exists nowhere in a large codebase.

## Executive conclusion

None of the four systems provides the complete combination Cyclops is aiming
for: a durable message identity, safe delivery gating, exact-ID terminal
readback, an origin-checked recipient hook acknowledgement, and an append-only
record of every delivery decision.

The products divide into two groups:

- Herdr and cmux expose excellent local terminal-control APIs. Success means
  that terminal input was accepted or queued, not that an agent received a
  particular logical message.
- CAO and Gas Town add durable logical messages. CAO has a SQLite FIFO inbox
  and status gating, while Gas Town has Beads-backed mail and a two-phase
  recipient-side acknowledgement. Neither verifies that the exact full
  payload entered the intended agent's context.

The most important competitive lessons are:

1. cmux explicitly models “paste succeeded, submit failed” as partial success
   because returning a generic error would make a client retry and paste the
   block twice.
2. Gas Town separates the durable mail record from its best-effort wake-up
   signal, and makes acknowledgement writes idempotent.
3. cmux and Gas Town both treat hook installation as a product feature rather
   than a manual documentation exercise.
4. CAO shows the value of a durable, idle-gated inbox, but also shows why a
   state named `DELIVERED` must not be written before the observable delivery
   boundary.

## Capability comparison

| Product | Logical unit | Safe-state behavior | Strongest delivery evidence | Retry and duplicate posture | Sender identity | Durable record |
| --- | --- | --- | --- | --- | --- | --- |
| Herdr | Prompt text sent to a live agent terminal | May prompt an already-working agent | Expected agent still owns the pane; bytes accepted by terminal runtime | Interrupted clients are told to reconnect and retry; prompt has no idempotency key | No message sender field in prompt request | Session/runtime metadata, not a message ledger |
| cmux | Raw text or keys sent to a terminal surface | Terminal input may be queued; this is not agent-idle gating | `sent` or terminal-input `queued`; no payload readback | One composer path reports partial success specifically to prevent double paste | Socket access and routing context, not per-message provenance | Bounded event JSONL and app/session state, not authoritative message history |
| CAO | SQLite inbox message with numeric ID | FIFO delivery on `IDLE`/`COMPLETED`; optional eager provider path | `DELIVERED` is written before tmux input; no recipient ACK | Pre-marking prevents re-entrant duplicate, but creates crash and partial-send ambiguity | Usually inherited `CAO_TERMINAL_ID`; raw API accepts caller-supplied sender | Mutable SQLite inbox, retained by cleanup policy |
| Gas Town | Beads mail record with ID, sender, recipient, thread, and body | Durable write first; idle nudge or cooperative nudge queue | Recipient `mail check --inject` prints a reminder, then writes an idempotent ACK | Durable mail and wake-up are separate; ACK retries converge | Auto-detected from workspace, but `--from` can override | Beads/Dolt issue state; default mail is an ephemeral wisp |

## Herdr: a guarded terminal prompt, not durable messaging

### What happens on send

**Evidence.** `agent.prompt` resolves a live agent, rejects a pending launch,
checks that the expected agent is still the foreground process, writes the
encoded text, schedules Enter, and then returns `AgentPrompted`. There is no
message ID, idempotency key, composer readback, or vendor acknowledgement in
this path ([source](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/src/app/api/agents.rs#L62-L110)).

This occupant check is valuable: it prevents an agent-targeted prompt from
being typed into a replacement shell. It does not establish that the terminal
TUI displayed, submitted, or processed the exact text.

Herdr's own automation documentation is precise about the boundary:
`agent.prompt` submits text plus Enter, can prompt an agent that is already
working, and rejects the operation if that agent no longer controls the pane
([source](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/docs/next/website/src/content/docs/agent-automation.mdx#L59-L76)).

### What `--wait` means

**Evidence.** A waited prompt snapshots and pins target identity. If the agent
was not already working, Herdr requires a lifecycle-state change within five
seconds, then waits for a requested settled state
([source](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/src/api/wait.rs#L177-L305)).
The official documentation warns that this does not track an individual turn:
if the agent was already working, completion of the pre-existing turn can
satisfy the wait
([source](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/docs/next/website/src/content/docs/agent-automation.mdx#L72-L80)).

Therefore a successful wait is lifecycle evidence, not an ACK for the prompt
and not proof that a specific response corresponds to it.

### Socket and duplicate risk

**Evidence.** Herdr uses one newline-delimited JSON request per line over a
local Unix-domain socket, echoes the request ID in the response, and keeps
subscription connections open
([source](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/docs/next/website/src/content/docs/socket-api.mdx#L602-L639)).
The socket is restricted to owner read/write permissions
([source](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/src/api/server.rs#L24-L28),
[application](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/src/api/server.rs#L74-L85)).
That is local access control, not logical sender attribution.

**Evidence.** During an experimental live server handoff, in-flight requests,
waits, subscriptions, sockets, and pane-to-pane messages may be interrupted;
clients are instructed to reconnect and retry
([source](https://github.com/herdrdev/herdr/blob/7d77e927eba265ea864485bf99411abcf62d977d/docs/next/website/src/content/docs/session-state.mdx#L92-L99)).

**Inference.** If text was accepted before the connection was interrupted but
the caller did not receive the response, retrying `agent.prompt` can submit the
same prompt twice. The socket request `id` correlates a response on that call;
the inspected prompt path does not persist it as an idempotency key.

### Cyclops lesson

Keep Herdr's occupant-identity guard and event-driven, target-pinned waits, but
do not describe either as message verification. A request ID must be persisted
and deduplicated if callers are expected to retry after an ambiguous outcome.

## cmux: strong terminal control and an explicit partial-send result

### What `surface.send_text` proves

**Evidence.** cmux's v2 socket protocol sends and receives one JSON object per
line and echoes the request ID
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/docs/v2-api-migration.md#L35-L57)).
The current CLI describes the default Unix-socket path as
`~/.local/state/cmux/cmux.sock`
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/CLI/cmux.swift#L36415-L36421)).

`surface.send_text` resolves a terminal surface and injects literal text. Its
reply is explicitly “load-bearing” because `queued`, `input_queue_full`, and
`process_exited` drive caller retry
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/Packages/macOS/CmuxControlSocket/Sources/CmuxControlSocket/Coordinator/Surface/ControlCommandCoordinator%2BSurface2.swift#L197-L242)).
The success result returns `queued: true|false`
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/Packages/macOS/CmuxControlSocket/Sources/CmuxControlSocket/Coordinator/Surface/ControlCommandCoordinator%2BSurface2.swift#L294-L355)).

**Evidence.** The implementation accepts any resolved terminal panel and maps
its terminal-input result to sent, queued, queue-full, unavailable, or exited
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/Sources/TerminalController%2BControlSurfaceContext3.swift#L246-L330)).
It does not require that an agent be present or idle. Here, `queued` means that
cmux buffered terminal input; it does not mean a logical message is durably
waiting for an agent-safe turn boundary.

### The double-paste lesson

cmux contains the clearest directly relevant treatment of the reported
double-paste class of bug.

**Evidence.** Its mobile composed-prompt path performs two separate effects:
bracketed paste, then a submit key. Once paste has been accepted, a submit-key
failure intentionally does not return a generic RPC failure. The source says a
client would otherwise assume nothing was sent, retain the draft, retry, and
paste the entire block a second time. It instead returns partial success with
`submitted: false` and a `submit_error`
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/Sources/TerminalController.swift#L14891-L14989)).

This is the exact ambiguity Cyclops needs to eliminate: “paste failed” and
“paste succeeded but Enter failed” cannot share one retryable error.

### Hooks, events, and system of record

**Evidence.** `cmux hooks setup` discovers installed agents, installs all or
one integration, skips missing binaries, reports a summary, and supports
uninstall. Hook records associate native agent session IDs, workspace/surface,
cwd, PID, lifecycle state, and a sanitized launch command
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/docs/agent-hooks.md#L1-L50)).

cmux also appends lifecycle and control events to
`~/.cmuxterm/events.jsonl`. This is a bounded, rotated audit stream, and disk
backpressure may drop old pending disk-only lines; its documentation says
socket gap detection plus snapshots are the catch-up source of truth
([source](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/docs/events.md#L1-L26),
[limits](https://github.com/manaflow-ai/cmux/blob/6089fa04d3effd27e43c5c6104a4eada62fe859f/docs/events.md#L161-L190)).
It is useful observability, but not an immutable messaging system of record.

### Cyclops lesson

Model and persist paste and submit as separate phases. Return an unambiguous
partial result when only paste succeeded; never make a caller repeat the paste
to recover a missing Enter. Copy the hook installation UX, including discovery,
per-agent setup, summaries, status, and uninstall, while preserving user-owned
configuration.

## CAO: a durable idle-gated inbox with a premature `DELIVERED` state

### Queue and gate

**Evidence.** CAO stores an inbox row with a numeric ID, sender, receiver,
message, status, and creation time
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/clients/database.py#L49-L59)).
New messages start `PENDING`, and reads are FIFO by creation time
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/clients/database.py#L1085-L1137)).

The inbox service normally delivers one pending message on an `IDLE` or
`COMPLETED` event. An opt-in eager path can also deliver while processing or
waiting for user input, but only for providers that declare support
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/services/inbox_service.py#L34-L94)).
The terminal service refuses to type into a provider in `ERROR` state, avoiding
accidental shell execution
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/services/terminal_service.py#L1003-L1057)).

CAO's final transport is still tmux: load a buffer, paste it, delay, and send a
provider-specific number of Enter keys
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/clients/tmux.py#L401-L447)).
The inspected path has no exact composer readback and no recipient hook ACK.

### Delivery-state semantics and duplicate risk

**Evidence.** CAO has only `PENDING`, `DELIVERED`, and `FAILED`
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/models/inbox.py#L17-L32)).
It writes `DELIVERED` before calling `send_input`. The stated reason is to
prevent output/status events caused by the send from re-entering the inbox
service and sending the still-pending message twice. A missing terminal resets
to `PENDING`; other exceptions set `FAILED`
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/services/inbox_service.py#L96-L139)).

**Inference.** This closes one duplicate window but changes the meaning of
`DELIVERED` to “claimed before attempting terminal input.” A process crash
between the database update and paste can leave a false delivered record. An
exception after paste but before all Enter keys complete can become `FAILED`;
a manual resend can then duplicate the already-pasted content.

**Evidence.** A 30-second periodic reconciliation sweep adopts old messages
that remain pending after the immediate and event-driven paths miss them
([source](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/api/main.py#L182-L197)).
That is a pragmatic safety net, but it is polling and is incompatible with
Cyclops' event-driven invariant as a primary design.

### Sender and record

**Evidence.** CAO's MCP helper derives the sender from `CAO_TERMINAL_ID`, but
the underlying HTTP endpoint accepts `sender_id` from the request and returns a
message ID
([helper](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/mcp_server/server.py#L653-L680),
[endpoint](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/api/main.py#L3844-L3880)).
The environment convention is useful routing context, not peer-authenticated
sender provenance at the API boundary.

The SQLite database defaults to
`~/.aws/cli-agent-orchestrator/db/cli-agent-orchestrator.db`, relocatable with
`CAO_HOME_DIR`, and its general cleanup policy is 14 days
([path](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/constants.py#L92-L122),
[retention](https://github.com/awslabs/cli-agent-orchestrator/blob/e592b21e170d005b26b4cb7bed702fbf3bf7037f/src/cli_agent_orchestrator/constants.py#L286-L290)).
This is a mutable current-state inbox, not an append-only event ledger.

### Cyclops lesson

Borrow the durable FIFO and explicit safe-state queue, but represent a claimed
attempt separately from delivered evidence. A persisted attempt lease can stop
re-entrant workers without naming the message delivered before paste begins.
Recovery should be driven by durable replay on daemon startup and real state
events rather than a forever interval.

## Gas Town: durable mail plus recipient-side hook acknowledgement

### Durable message and best-effort wake-up

**Evidence.** Gas Town mail records include an ID, sender, recipient, subject,
full body, read flag, priority, type, queue/interrupt delivery mode, thread and
reply IDs, and delivery-ack fields
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/mail/types.go#L49-L139)).
Direct mail is written as a Beads issue first. Only after the durable write
succeeds does Gas Town asynchronously notify an active recipient
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/mail/router.go#L1112-L1209)).

The notification path waits for a stable idle observation, sends a direct
nudge when idle, and otherwise enqueues a cooperative nudge for the next turn
boundary
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/mail/router.go#L1592-L1605),
[gate](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/mail/router.go#L1655-L1694)).
The wake-up signal is therefore not the message record and does not define
whether the mail exists.

### What the ACK actually proves

**Evidence.** Phase one writes `delivery:pending`. Phase two writes recipient
and timestamp labels, then `delivery:acked`; retries filter already-present
labels and reuse the timestamp when safe. `pending` is removed only after
`acked` is durable
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/mail/delivery.go#L12-L115)).

The recipient-side `gt mail check --inject` path prints a system reminder and
only then acknowledges the messages
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/cmd/mail_check.go#L71-L100)).
The injected reminder contains message ID, sender, and subject and instructs
the agent to read the message; it does not contain the full body
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/cmd/mail_check.go#L126-L189)).

The accurate interpretation is: ACK proves that the recipient's mail-check
process wrote a reminder containing the logical message ID to standard output.
When invoked as a vendor hook, that is hook-handoff evidence. It does not prove
that the full body was read into model context, that the TUI accepted the
hook's output, or that the agent completed the requested work.

### Hooks, sender, and record

**Evidence.** Gas Town documents raw tmux integration as timing-sensitive and
without delivery confirmation
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/docs/agent-provider-integration.md#L42-L61)).
For hook-capable agents, it automatically installs lifecycle commands such as
`gt prime --hook && gt mail check --inject` at session start and
`gt mail check --inject` at prompt submission
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/docs/agent-provider-integration.md#L268-L341)).

Sender identity is normally inferred from workspace context, but the CLI has a
`--from` override for relay/bridge use
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/cmd/mail_send.go#L68-L93)).
It is useful application identity, not unforgeable audit provenance.

Mail lives in the town's Beads/Dolt state. The default `--wisp` mode is
ephemeral and not synced to git; `--permanent` opts into remote-synced mail
([source](https://github.com/gastownhall/gastown/blob/649b832b7672bc7a2dbef26f5983aba6198b819b/internal/cmd/mail.go#L460-L479)).
This is durable issue state, with explicit read and ACK labels, but it is not an
immutable append-only delivery ledger.

### Cyclops lesson

Keep the same architectural separation: persist the logical message before
attempting a wake-up, and make wake-up retry independent of message creation.
Gas Town's idempotent phase-two ACK is a useful pattern. Cyclops should retain a
stronger claim name: a hook event is a hook handoff ACK unless it includes and
origin-verifies the exact message ID expected by the delivery attempt.

## Recommendations for Cyclops

### 1. Make retries idempotent at the socket boundary

Add a caller-generated `client_request_id` or idempotency key distinct from the
server-generated message ID. Persist the mapping before initiating delivery.
A repeated request with the same key and equivalent payload should return the
original message/result; a different payload under the same key should fail.

This closes the Herdr-style ambiguity where the server may have acted before
the client connection disappears.

### 2. Record paste and submit as separate phases

Use explicit states or events such as:

```text
attempt_started -> paste_accepted -> staged_id_observed -> submit_accepted
                -> recipient_hook_acknowledged
```

If paste succeeds and Enter fails, return partial success and retry only the
submit phase after confirming that the exact staged message ID is still
present once. Never re-paste merely because the submit result is unknown.

This is the highest-value response to the observed Claude-to-Codex
double-paste symptom.

### 3. Do not overload `delivered`

Use names tied to evidence:

- `queued`: accepted into Cyclops' durable queue.
- `paste_accepted`: terminal transport accepted the payload.
- `staged`: exact message ID read back from the intended composer.
- `submitted`: submit input was accepted.
- `acknowledged`: an origin-validated vendor hook returned the same message ID.

CAO's pre-send `DELIVERED` state demonstrates why a state name must not claim a
stronger boundary than its evidence.

### 4. Automate hook installation, but keep it inspectable and reversible

The installer can offer hook setup after installing the binaries, while a
standalone command remains the repair and audit surface:

```text
cyclops hooks install [--all | <agent>]
cyclops hooks status
cyclops hooks doctor
cyclops hooks uninstall <agent>
```

The implementation should:

- discover installed agent CLIs and report installed, skipped, outdated, and
  failed integrations;
- merge only marker-owned entries into existing user configuration;
- back up changed files and make repeated installation idempotent;
- pin the generated hook protocol version and resolve the installed Cyclops
  binary/socket safely;
- validate permissions, hook origin, target pane identity, message ID, and
  attempt ID before promoting a receipt;
- preserve an explicit opt-out and avoid silently replacing third-party
  configuration.

cmux supplies the strongest setup/status/uninstall UX model. Gas Town supplies
the strongest example of using hooks at session and prompt boundaries to drain
durable mail.

### 5. Preserve Cyclops' differentiators

Do not weaken these properties while adopting competitor ideas:

- terminal input is not itself a delivery receipt;
- a request ID is not a logical message ID;
- socket ownership is not sender attribution;
- lifecycle completion is not per-message completion;
- hook execution is not model consumption;
- mutable inbox state is not an append-only audit history.

The defensible product claim is not “Cyclops always knows the model understood
the message.” The technically supportable claim is that Cyclops can accumulate
and name progressively stronger, independently recorded evidence: durable
acceptance, safe staging, exact-ID readback, submission, and an
origin-validated hook acknowledgement.

## Negative-source-search caveat

At each pinned revision, the inspected public send paths and official docs were
searched for message IDs, idempotency, acknowledgement, readback, verification,
composer state, delivery state, retries, sender identity, sockets, and hooks.
No live fault injection was performed. In particular, “no exact-payload ACK
found” means none was found in the cited public path; a private integration,
unrelated feature, or newer commit could change that conclusion.
