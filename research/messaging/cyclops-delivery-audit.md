# Cyclops messaging and delivery audit

Audit date: 2026-08-07. Source reviewed at `c1d3d88`. The installed daemon
binary embeds build `e610afc`, which is not an object in this checkout, so the
live incident cannot be tied byte-for-byte to the reviewed source. Its ledger
sequence matches the delivery path that is still present.

## Verdict

There is a confirmed double-paste defect for multiline messages sent to Codex.
The defect is before Enter, in composer verification and retry. Three local
deliveries each entered `pasting` twice after the first paste returned success
but the message id could not be read back. The retry gate then misclassified
the non-empty Codex composer as empty or ghost text and admitted a second
paste.

Missing hooks are also confirmed on this machine, but they did not cause these
three failures. Hooks run after Enter and cannot repair pre-submit composer
verification. They would reduce ambiguity after submission and make successful
receipts stronger.

"Agents are blocked from sending" currently has at least two distinct shapes:

1. A delivery can sit at the head of a per-pane FIFO while the target is
   `idle_with_input`, `working`, `unknown`, detached, or blocked on a human.
   Every message behind it waits. This is intentional ordering and safety
   behavior, but it is an availability cost and currently looks like a generic
   queue.
2. Failed or interrupted deliveries remain visible as unresolved attention
   items. They do not stop a worker from accepting later messages, but the live
   status currently shows seven old items under "waiting on you," which can
   look like the messaging system is still blocked.

No daemon-side `denied` evidence was found for `msg.send`. A vendor agent's own
sandbox could still prevent the CLI from opening `$CYCLOPS_HOME/sock` before a
request reaches the daemon; the current evidence neither proves nor disproves
that case.

## Confirmed defect: Codex multiline paste is retried into a non-empty composer

The clearest local chain is `m-b523ec` in
`~/.cyclops/ledger/main.ndjson`, sequences 313-321:

| Seq | Time delta | Fact |
|---:|---:|---|
| 313 | 0 ms | Message from `planner` to `reviewer` recorded |
| 315 | +12 ms | Gate proceeds on `composer_ghost_suggestion` |
| 316 | +17 ms | First `pasting` attempt |
| 317 | +511 ms | First attempt becomes `retry_queued: verify_failed` |
| 319 | +520 ms | Retry gate proceeds on `composer_empty_or_ghost` |
| 320 | +524 ms | Second `pasting` attempt |
| 321 | +1,015 ms | Second attempt becomes `attention_required: verify_failed` |

`m-8a777f` repeats the same shape at sequences 325-333. `m-b4c46c`
reaches `verify_failed`, waits in `unknown` for about 75 seconds, then proceeds
on `composer_ghost_suggestion` and pastes a second time at sequences 370-382.
The message bodies were not printed or copied into this report; metadata shows
lengths of 854, 854, and 416 bytes with 23, 23, and 16 newlines. These are
exactly the messages likely to be collapsed by a TUI.

A `verify_failed` after `pasting` is stronger evidence than a generic attempt
counter. `inject` first awaits `paste`, then performs four captures, and only
returns `verify_failed` after `paste` returned `Ok` and none of the captures
verified staging (`src/cyclopsd/src/delivery.rs:2003-2027`). Each retry enters
that function again and calls `paste` again. The ledger does not prove that the
Codex application consumed both writes, but it proves that Cyclops issued two
successful tmux paste commands; the reported visual double-paste supplies the
last observation.

### Why verification fails

The interaction between the verifier and the Codex manifest has a specific
gap:

- Verification captures at offsets 0, 120, 240, and 480 ms and searches the
  bottom 15 non-empty lines (`delivery.rs:52-56`, `2008-2027`).
- An id-bearing pattern can match anywhere in that region. A generic pattern
  such as `Pasted` only counts when it appears on a composer line identified
  by an `idle_with_input` rule (`delivery.rs:2030-2048`).
- `marker_in_composer` checks only `line_regex` clauses, not
  `line_regex_esc` (`delivery.rs:2472-2507`). It also receives a plain capture.
- The Codex `composer_typed_input` rule has only `line_regex_esc`
  (`resources/manifests/codex.toml:64-73`). Its plain fallback explicitly
  classifies any `› ...` line as idle (`codex.toml:85-99`).
- Codex's verify patterns are `<message_id>` and `Pasted`
  (`codex.toml:153-159`). When a multiline paste collapses and hides the id,
  the generic pattern has no usable Codex `idle_with_input` line and is
  effectively dead.

After the first failure, `process` immediately returns through the gate and
allows the configured retry (`delivery.rs:1258-1316`). The gate correctly
forces a fresh screen evaluation, but the same manifest gap calls the staged
composer `composer_empty_or_ghost`, as the incident gate records show. The
second paste follows.

The existing regression coverage proves that bare typed Codex text is
distinguished from dim ghost text (`src/cyclopsd/tests/m1_blockers.rs:91-161`)
and locks two captured single-line fixtures
(`src/cyclops-manifest/tests/shipped_rules.rs:74-107`). It does not include a
collapsed multiline paste fixture, generic-pattern composer verification, or
an assertion that an ambiguous first paste is never repeated.

### One reported ping was not retried

If "the Codex ping" means message `m-8f9e3e`, its ledger tells a different
story. Sequences 172-179 contain exactly one `pasting` transition, then
`staged`, `submitted`, and `delivered_unverified(screen)`. The daemon did not
retry that message. A duplicate rendering of that exact id therefore needs a
pane recording or tmux command trace; it cannot currently be attributed to the
delivery retry loop. It could be a display artifact, a lower-level repeated
write, or a different observation. Two separate `msg.send` calls would
normally have different message ids.

## Retry policy is broader than the evidence permits

The default is one retry after the first failure
(`src/cyclopsd/src/config.rs:25-32`, `63-66`). `fail_attempt` applies that same
budget to failures in `pasting`, `staged`, and `submitted`
(`delivery.rs:1501-1536`). These failures do not have equal certainty:

- A spool-file failure before `paste-buffer` is definitely safe to retry.
- A pre-paste occupant rebound is definitely safe to re-gate.
- A `paste-buffer` timeout or disconnect may have applied before the reply was
  lost.
- `verify_failed` means tmux accepted the paste command but Cyclops could not
  prove what the TUI did.
- `submit_failed` can be ambiguous if Enter applied before the control reply
  was lost.
- `ack_timeout` occurs after Enter and means the turn may already have begun.

The last four are unknown outcomes, not proven non-deliveries. Automatically
repeating an unknown outcome trades duplicate work for liveness. The current
bounded retry prevents an infinite loop, but does not provide at-most-once
behavior.

The detach-aware ACK clock is a good, tested mitigation for one post-submit
case: it freezes deadlines while the pane is unobservable and performs an
evidence pass before retry after reconnect (`delivery.rs:2249-2406`). It does
not cover the confirmed pre-submit collapsed-composer case.

## Why messages appear blocked

Each pane has one worker and one FIFO (`delivery.rs:1202-1235`). The worker
pops one job and awaits its entire `process` call before it can pop the next
one (`delivery.rs:1230-1254`). The gate deliberately waits for an event while
the target is working, has composer input, is unknown, is in copy mode, is
detached, or needs a human (`delivery.rs:1620-1803`). A notification after the
default 120 seconds makes the hold visible but does not end it.

This produces head-of-line blocking by design: preserving per-recipient order
means later messages cannot overtake the held message. Local evidence includes:

- `m-1aa446`, held on `idle_with_input` for more than 120 seconds before the
  pane disappeared;
- `m-308221`, held on `idle_with_input` until a daemon restart closed it;
- `m-3ebfac`, a hook self-test held on `blocked:approval_prompt` until the
  prompt cleared, after which it delivered.

These are target-side gate holds, not send-API authorization failures. A sender
usually receives `queued` immediately because `msg.send` only waits on a gate
that can answer now (`delivery.rs:930-958`, `1057-1091`). The word `queued`
currently covers behind-another-message, gating, and retry
(`delivery.rs:1113-1129`), which hides the distinction a user needs when
debugging "blocked."

Attention records are another source of confusion. A completed
`attention_required` chain does not keep the worker busy, so later sends can
proceed. It remains in the append-only record and current status displays it
until an explicit resolution exists. The product has no requeue verb and no
operator acknowledgment/clearance for these delivery failures. That is a UX
and record-lifecycle gap, not a queue lock.

The live daemon was also reconnecting to most configured sessions during this
audit, and its log repeatedly reported `can't find window: Desktop`. A detached
session intentionally holds its delivery gate. That watcher problem is being
handled separately and should not be conflated with composer verification, but
it can produce the same user-visible symptom.

## Hooks: present feature, absent wiring

Cyclops already ships all of the hook-side pieces:

- templates in `resources/hooks/`;
- `cyclops hooks install`, which renders a label-specific file under
  `$CYCLOPS_HOME/hooks/<label>/` but intentionally refuses to edit vendor
  directories (`src/cyclops/src/hookset.rs:1-10`, `164-233`);
- `hooks verify` and a real delivery `hooks selftest`;
- payload matching on the exact message id (`src/cyclopsd/src/ack.rs:197-218`);
- hook-origin verification from Unix-socket process ancestry.

They are not wired on this machine. All eight sampled successful local
deliveries ended `delivered_unverified(screen)`; none ended
`delivered_verified`. The local Codex hook file contains another product's
hook but no Cyclops hook, and the Claude settings contain existing hooks but
no Cyclops hook. No Cyclops `hookseq` files or hook error log exist. The local
`m-3ebfac` self-test recorded `hook_ack: false` and the daemon issued its
"hooks configured but never seen" notification.

Hook installation should be easier, but automating it in the download
installer is not yet safe:

1. The templates bake an agent label into every command. The binary installer
   runs before panes are named and cannot know that label.
2. One user-level Codex config applies to every Codex process. A command fixed
   to `--agent reviewer` cannot correctly report from a second Codex pane with
   another label; origin verification will reject the mismatch.
3. Vendor config files already contain unrelated hooks on this machine. A
   copy or overwrite would destroy them. Installation needs an idempotent,
   schema-aware merge with an atomic backup.
4. Codex can load both user and project hooks, so the merge must recognize an
   existing Cyclops command and avoid installing it twice. Event dedupe limits
   duplicate hook reports, but duplicate configuration is still unnecessary.
5. A written file is not proof that a running CLI loaded it. Installation must
   finish with `hooks selftest`, and report unverified until it passes.

The cleanest prerequisite is to make the hook's `agent` parameter optional and
derive the reporting pane and current label from the socket peer ancestry. The
daemon already performs that walk to verify the claimed label
(`src/cyclopsd/src/server.rs:660-697`); using the derived identity as the value
would allow one global, label-free hook command per vendor and would survive
pane renames. After that, `cyclops hooks install --user --merge <vendor>` can
merge the Cyclops entries into vendor config. This belongs in an explicit
Cyclops setup command, not a non-interactive curl installer.

Hooks would make post-submit outcomes stronger and should disable most
`ack_timeout` ambiguity. They do not solve a paste that cannot be verified
before Enter.

## Priority actions

### P0: stop automatic repetition of ambiguous writes

As an immediate operational mitigation, set `delivery_retry_max = 0` and
restart the daemon. A failed multiline paste will require attention after one
attempt instead of issuing a second paste.

In code, replace the one shared retry budget with outcome-aware certainty.
Only failures proven to occur before an irreversible pane write should retry
automatically. `verify_failed`, ambiguous `paste_failed`, `submit_failed`, and
`ack_timeout` should terminate as `attention_required` with an
`outcome_unknown` cause unless a vendor hook or protocol supplies idempotency.
Preserve retry for proven pre-write failures such as spool creation and a
pre-paste rebound.

### P0: measure and support Codex's collapsed composer

Capture plain and escaped screens for a real multiline paste on the currently
supported Codex version, scrub the content, and add
`codex_collapsed_paste_{plain,esc}` fixtures. The manifest needs a high-priority
`idle_with_input` rule for that exact collapsed placeholder so a retry gate can
never call it ghost text.

Verification also needs the same escaped-composer vocabulary as fusion.
`marker_in_composer` currently ignores `line_regex_esc`; either pass both
captures into verification or centralize "which composer line matched" in the
manifest evaluator. Add a regression that simulates a collapsed paste whose id
is hidden and asserts that exactly one `paste-buffer` command is issued.

### P1: separate held, queued, and outcome-unknown

Expose the last gate cause and age in the send receipt and status surface:

- `queued`: another message is ahead;
- `held`: target state or human action prevents delivery;
- `outcome_unknown`: Cyclops wrote to the terminal but lacks proof of the
  result.

Add an append-only operator acknowledgment/cancel transition for obsolete
attention items. It should clear the attention projection without rewriting
the original failure. Do not let later messages bypass a held head item unless
the operator explicitly cancels it; silent reordering would violate FIFO.

### P1: make hooks globally installable and prove them

Derive hook identity from socket peer ancestry, then add a vendor-aware,
idempotent merge command that preserves existing hooks and creates an atomic
backup. Run `hooks selftest` after the target CLI restarts. Keep hook setup out
of the binary download installer until it is label-free and merge-safe.

### P2: distinguish client/socket failures from delivery holds

For the next report of "agent cannot send," capture the CLI exit code and JSON
response from inside that agent pane, then look for the returned message id in
the ledger:

- no message line: client, sandbox, socket, or request validation problem;
- message line ending in `gating`: target-side hold;
- `pasting -> retry_queued`: injection/readback problem;
- `submitted -> retry_queued`: ACK/evidence ambiguity.

This one correlation will prevent socket authorization, gate safety, and TUI
delivery bugs from being diagnosed as the same issue.

## Verification performed

Read-only local probes inspected delivery metadata, gate rules, current hook
configuration, hook liveness artifacts, daemon status, and logs. No message
bodies or raw pane captures were recorded in this report.

Focused tests all passed:

- `cargo test -p cyclopsd --test m1_blockers`: 5 passed;
- `cargo test -p cyclopsd --test m1_fixes`: 9 passed;
- `cargo test -p cyclopsd --test m2_hooks`: 5 passed;
- `cargo test -p cyclops-manifest --test shipped_rules`: 9 passed.

The 28 passing tests validate existing safety invariants. They do not
contradict the incident: no test covers a collapsed Codex multiline composer
followed by `verify_failed` retry.

## Evidence classification

Confirmed:

- Cyclops issued two paste attempts for three local multiline Codex messages.
- The retry gate misclassified the post-paste composer as a ghost/empty
  composer.
- Generic Codex verification cannot use its escaped-only
  `idle_with_input` rule.
- Cyclops hooks are not firing on this machine; successful sampled deliveries
  are screen-only.
- Head-of-line gate holds and unresolved attention records occurred locally.

Plausible but not yet measured:

- The visual duplicate for `m-8f9e3e` came from a TUI/display or lower-level
  write issue; its delivery ledger has only one attempt.
- An agent sandbox prevented opening the Unix socket. No matching daemon denial
  or failed request was found.
- Extending the verification delays would fix the multiline failure. One
  incident waited roughly 75 seconds before its second attempt and still
  failed, so delay alone is unlikely to be sufficient.
