# How we improve messaging without redesigning it

Status: proposed implementation plan. This page describes work that is not
built yet.

Baseline: `main` at `f84e708` (2026-08-07). The research audited delivery
source at `c1d3d88`; the relevant retry, verification, and hook-preparation
paths are unchanged on this baseline.

Audience: the implementation agent taking the next messaging reliability
work.

## Outcome

Ship a small reliability release that does four things, in this order:

1. Never paste a logical message a second time after a terminal write may
   already have happened.
2. Recognize Codex's collapsed multiline composer from measured captures, so
   Cyclops never treats staged input as an empty composer.
3. Tell the sender whether a message is behind other work, held on the target,
   or has an uncertain terminal outcome, with a safe next action.
4. Remove the two sharp edges in today's manual hook preparation: vendor
   artifacts overwriting one another and instructions that can overwrite a
   user's shared hook file.

This is not a delivery rewrite. Keep the per-pane FIFO, the current ten-state
delivery machine, tmux injection, progressive evidence tiers, append-only
ledger, and event-driven gate.

## Evidence synthesized from the audits

The three research reports support one immediate product priority:

- Cyclops issued two successful paste commands for three multiline Codex
  messages. The first paste returned success, composer readback was
  inconclusive, and the generic retry pasted again.
- Codex's collapsed composer hid the message id. Verification could see the
  generic `Pasted` marker but could not use Codex's escaped-only
  `idle_with_input` rule. The next gate pass then called the occupied composer
  empty or ghost text.
- Hooks were not active on the audited machine, but hooks run after Enter.
  They can strengthen post-submit evidence; they cannot prevent this
  pre-submit double paste.
- A per-target FIFO deliberately makes later messages wait behind a held head
  message. The bug is not FIFO. The quality problem is that the receipt calls
  several different conditions `queued`, including `queued · 0 ahead`.
- cmux's useful lesson is to preserve partial terminal success rather than
  return an error that invites a second paste. Herdr's request ids, CAO's
  durable inbox, and Gas Town's hook ACKs do not make an ambiguous terminal
  write safe to repeat.
- Socket idempotency and automatic hook setup are valuable, but neither fixes
  the confirmed duplicate-paste path. They require broader wire, ledger, and
  configuration ownership work and do not belong in this increment.

Sources:

- [Messaging research summary](README.md)
- [Cyclops delivery audit](cyclops-delivery-audit.md)
- [Hook installation audit](hook-installation-audit.md)
- [Competitive messaging audit](competitive-messaging-audit.md)

## Revalidation against `main`

The implementation agent should not spend a discovery pass proving these
again. They are present on the baseline:

| Finding | Mainline evidence |
| --- | --- |
| Ambiguous failures share one retry budget | `src/cyclopsd/src/delivery.rs`, `fail_attempt`, retries failures from `pasting`, `staged`, `submitted`, and `retry_queued` using only the attempt count. |
| Retry is enabled by default | `src/cyclopsd/src/config.rs` sets `delivery_retry_max = 1`. |
| The regression currently blesses the defect | `src/cyclopsd/tests/m1.rs`, `verification_failure_retries_once_then_needs_attention`, asserts two `pasting` attempts. |
| Codex verification lacks escaped composer matching | `marker_in_composer` reads a plain capture and checks only `line_regex`; Codex's `composer_typed_input` rule uses `line_regex_esc`. |
| Fusion already knows how to do this correctly | `src/cyclopsd/src/fusion.rs` takes paired plain/escaped captures and calls `Manifest::evaluate_esc`. |
| Hook artifacts collide | Codex, agy, and Cursor all render `hooks.json` under `$CYCLOPS_HOME/hooks/<label>/`. |
| Wiring advice can overwrite user config | `src/cyclops/src/hookset.rs` prints raw `cp` commands into shared Codex, agy, and Cursor files. |

## Scope rules

Apply these constraints to every work package:

- Do not add a new delivery state for this release. An ambiguous write ends in
  the existing `attention_required` state with a precise cause and honest
  copy.
- Do not remove or bypass the per-recipient FIFO. A later message never
  overtakes a held head message.
- Do not add a timer or reconciliation loop. Holds still wake only on pane,
  session, or fused-state events.
- Keep vendor recognition in `resources/manifests/`, backed by measured
  fixtures and a finding. Do not add Codex-specific detection to Rust.
- Do not put raw captures in the ledger, logs, notifications, or this plan.
  Scrub fixture content before committing it.
- Keep every tmux command in `cyclops-tmux`; delivery may call the adapter but
  may not spawn tmux itself.
- Keep wire changes additive and optional. Older clients must ignore them;
  newer clients must tolerate their absence.
- Preserve the `Injector` seam. It is the deliberate escape path for a future
  non-terminal backend.

## Work package 1 — stop ambiguous re-pastes (P0)

This package lands first and is independently releasable.

### 1. Start with the failing regression

Change the existing `BAD_VERIFY_MANIFEST` end-to-end case in
`src/cyclopsd/tests/m1.rs` before changing product code:

- Rename the test to state the invariant, for example
  `verification_failure_after_a_successful_paste_never_repastes`.
- Leave `delivery_retry_max` at `1` in the rig. The test must prove that a
  configured retry budget cannot repeat an ambiguous write.
- Assert one `pasting` transition, one visible message header in the isolated
  pane, final `attention_required`, cause `verify_failed`, and `attempts = 1`.
- Keep the existing notification and ledger-legality assertions.

Add a small table-driven unit test for every failure boundary. It should fail
if a future edit makes an after-write failure retryable.

### 2. Classify failure certainty inside delivery

Replace `AttemptOutcome::Failed(String)` with a small internal type carrying
the cause and whether the failure is proven to be before the pane write. Keep
this private to `delivery.rs`; it is pipeline control, not a new public
protocol.

The required classification is:

| Failure | Automatic retry? | Reason |
| --- | --- | --- |
| Session detached before paste | Yes | No pane write was attempted. Re-enter the event-driven gate. |
| Manifest disappeared before paste | Yes | No pane write was attempted. |
| Pre-paste occupant rebound | Yes | The guard ran before the payload write. |
| Spool/load-buffer failure | Yes | No bytes reached the pane. |
| `paste_failed` | No | tmux may have applied the command before its reply was lost. |
| `verify_failed` | No | `paste` returned success; readback is inconclusive. |
| Occupant rebound after verified paste | No | The original occupant received staged input; do not send it to a replacement. |
| `submit_failed` | No | Enter may have applied before the reply was lost. |
| `ack_timeout` | No | Enter was accepted and the turn may already have started. |

`delivery_retry_max` remains useful, but only for the proven pre-write rows.
Keep its default at one after this classification is enforced. If the
classification cannot land atomically, use zero as a temporary default; do
not ship another build where `verify_failed` can consume a retry.

Remove the special case that moves `submit_failed` to `retry_queued` inside
`attempt_delivery`. The caller should make the one certainty-aware decision.
Use distinct pre-paste and post-paste rebound outcomes even if their
user-facing wording shares a stem.

### 3. Preserve precise evidence in the record and receipt

Do not add a parallel attempt journal. The existing transitions already name
the useful boundaries: `pasting`, `staged`, and `submitted`. Make the terminal
`attention_required` line carry the exact machine cause and one attempt.

Do not replace that cause in the receipt with the generic note
`delivery failed after N attempts`. The exact cause is needed to choose safe
copy and remains safe for the ledger because it contains no observed screen
text.

### 4. Make ambiguous failure copy honest

Update `cyclops-ui::grid::cause_words` and the follow-up copy in
`src/cyclops/src/copy.rs` so after-write failures never say the recipient
"did not get this message." The minimum distinction is:

- Proven pre-write failure: not delivered; fix the named cause and retry.
- Ambiguous after-write failure: outcome unknown; inspect the named pane or
  recipient composer before resending.

Keep the exact cause visible in `--json`. Plain and colored output must carry
the same meaning.

### 5. Update the authoritative design pages in the same change

The current diagrams explicitly route `verify_failed` and `ack_timeout` to
retry. Update these pages with the code:

- `docs/development/DELIVERY.md`
- `docs/development/ARCHITECTURE.md`
- `docs/development/INVARIANTS.md` (especially rule 1's diagram)
- `docs/reference/PROTOCOL.md`
- `docs/guides/send.md`
- `docs/guides/troubleshooting.md`
- `docs/guides/install.md` and `docs/public/reference/configuration.mdx` for
  the narrowed meaning of `delivery_retry_max`

The documentation must say that `attention_required` can mean an unknown
terminal outcome, not a proven non-delivery.

### Acceptance criteria

- A successful paste followed by inconclusive verification issues exactly
  one paste, even with `delivery_retry_max = 1`.
- `paste_failed`, post-paste rebound, `submit_failed`, and `ack_timeout` cannot
  cause another payload paste.
- A forced, proven pre-write failure still retries at most the configured
  bound and re-enters the full gate.
- The final ledger line names the original cause and `attempts = 1` for an
  ambiguous first attempt.
- Human output says `outcome unknown` and tells the operator to inspect before
  resending; it never claims non-delivery.

## Work package 2 — support the measured Codex collapsed composer (P0)

This package removes the confirmed trigger. It must follow package 1 so a
future Codex display change degrades to one uncertain write, never two.

### 1. Measure before writing a rule

On the currently supported Codex CLI, paste a multiline payload large enough
to collapse. Capture both normal and `capture-pane -e` views before Enter.
Record the Codex version, tmux version, exact probe, and conclusion as a new
finding in `findings.md`.

Commit scrubbed fixtures under
`src/cyclops-manifest/tests/fixtures/`, named along these lines:

- `codex_collapsed_paste_plain.txt`
- `codex_collapsed_paste_esc.txt`

The placeholder, composer glyph, and SGR boundaries must remain real. Replace
the message body and any local paths or identifiers.

If the current Codex build no longer reproduces the audited shape, stop this
package and update the finding. Do not invent a regex for a stale screenshot.

### 2. Add a manifest rule, not a Rust special case

In `resources/manifests/codex.toml`, add a high-priority
`idle_with_input` rule for the measured collapsed placeholder. It must outrank
`composer_ghost_suggestion` and `composer_empty_or_ghost` and must not match an
empty composer or ordinary transcript text.

Update `version_tested` only to the build actually probed.

Extend `src/cyclops-manifest/tests/shipped_rules.rs` to prove:

- the collapsed fixture is `idle_with_input` with paired captures;
- empty/ghost remains `idle`;
- ordinary typed input remains `idle_with_input`;
- the collapsed rule wins over the plain idle fallback.

### 3. Give verification the same escaped composer vocabulary as fusion

Do not duplicate Codex's SGR rules in `delivery.rs`. Extend the injector's
capture surface so verification can request an escaped capture when
`Manifest::has_escaped_rules()` is true, using the adapter's existing
`capture_pane_escaped` method.

Factor one manifest-owned helper that answers whether a specific paired
plain/escaped line is an `idle_with_input` composer line. Use the compiled
`line_regex` and `line_regex_esc` clauses already used by normal manifest
evaluation. Then make `marker_in_composer` use that helper.

The generic `Pasted` verifier still counts only when it appears on the
matched composer line. The same word in transcript history must not verify a
new delivery.

### 4. Add the irreversible-boundary regressions

Cover all three layers:

- Manifest fixture test: collapsed input is `idle_with_input`.
- Delivery unit test: generic `Pasted` on the escaped-matched composer line
  verifies staging; stale transcript text does not.
- Isolated-tmux delivery test: an inconclusive collapsed paste produces one
  paste and attention, never a second paste. A fresh message presented with
  the collapsed composer already occupied holds at the gate.

Do not lengthen verification delays as the fix. One audited retry still
failed after roughly 75 seconds; this is classification, not timing.

### Acceptance criteria

- Fusion and post-paste verification agree on the same collapsed composer
  fixture.
- An occupied collapsed composer is never classified as idle.
- A stale `Pasted` marker outside the composer never verifies staging.
- No raw capture or matched text enters the ledger or notifications.

## Work package 3 — distinguish waiting from uncertain outcomes (P1)

Do this after the duplicate-paste and Codex fixes. It changes presentation,
not queue scheduling.

### 1. Add an optional held reason to receipts

Add an optional, additive field such as `held_by` to `DeliveryReceipt`.
Populate it only for the in-flight head delivery while the gate is holding.
Store the current hold cause on `DeliveryHandle`, update it when the gate's
hold changes, and clear it on proceed or resolution.

Keep the existing wire `state = queued` for compatibility. Render it as held
when `held_by` is present:

| Condition | Human receipt |
| --- | --- |
| Another message is ahead | `● queued · N ahead` |
| Recipient is working | `● held · recipient working` |
| Composer contains input | `● held · composer has input` |
| Pane is in copy mode | `● held · pane in copy mode` |
| Session is detached | `● held · session detached` |
| Modal/permission rule needs a person | `● held · waiting for a decision` |
| State cannot be decided | `● held · target state unknown` |

Do not expose a raw manifest rule id in the ordinary receipt. It remains in
the ledger and diagnostic surfaces.

### 2. Make old attention rows diagnosable, not clearable

`StatusResult::open_deliveries` already carries message id, timestamp, and
cause. Use those fields when rendering the `waiting on you` rows so an old
failed delivery is visibly a particular message with a particular cause.
Continue using `cyclops_proto::Attention` as the only rule deciding whether
the row exists and whether the eye opens.

Do not add acknowledge, cancel, requeue, or delete behavior in this release.
Those need explicit append-only transitions and separate product decisions.

### 3. Add one troubleshooting correlation

In `docs/guides/troubleshooting.md`, add the audit's quickest diagnostic:

- no message id in the ledger: client, sandbox, socket, or request validation;
- latest state `gating`: target-side hold;
- `pasting` followed by `attention_required`: paste/readback outcome unknown;
- `submitted` followed by `attention_required`: post-submit evidence unknown.

Use actual CLI and `jq` output captured after implementation, not invented
transcripts.

### Acceptance criteria

- `queued · 0 ahead` is no longer shown for a held head delivery.
- A message behind the head remains `queued · N ahead`; nothing overtakes it.
- JSON clients can distinguish a held head using an optional field and still
  parse responses from an older daemon.
- Status names each unresolved delivery's id and readable cause without
  changing the eye count.

## Work package 4 — make manual hook preparation non-destructive (P1, separate PR)

This is the only hook work in scope. Do not turn it into automatic vendor
configuration.

### 1. Isolate prepared artifacts by vendor

Change the default prepared path to:

```text
$CYCLOPS_HOME/hooks/<vendor>/<label>/<vendor-file>
```

This prevents Codex, agy, and Cursor from replacing the same per-label
`hooks.json`. Preserve explicit `--dest` behavior and keep refusing vendor
configuration directories.

Write the neutral artifact through a same-directory temporary file and atomic
rename so an interrupted prepare does not leave partial JSON.

### 2. Remove overwrite-shaped instructions

Replace raw `cp .../hooks.json <shared hooks.json>` advice with merge-aware
instructions:

- If the destination does not exist, copying a new file is safe.
- If it exists, merge the Cyclops event entries and preserve every unrelated
  key and handler.
- Always finish with `hooks verify` or `hooks selftest`; configuration alone
  is not verification.
- Include the current Codex trust/reload step, but never bypass trust on the
  user's behalf.

Do not write into `~/.codex`, `~/.claude`, `.cursor`, or `.agents`. Do not
change `scripts/install.sh` or the website installer.

### 3. Update tests and docs

Extend `src/cyclops/tests/hooks_cli.rs` to prove that preparing all four
vendors for one label leaves four valid, distinct artifacts; a second run is
stable; dry-run writes nothing; and vendor paths remain refused.

Update `docs/reference/hooks.md` with the real prepared paths and
non-destructive merge instructions. If parity captures hook output, refresh it
from the command transcript in the same change.

### Acceptance criteria

- Preparing one vendor cannot overwrite another vendor's artifact.
- No printed happy-path command overwrites an existing shared vendor file.
- `hooks install` remains prepare-only and `hooks selftest` remains the proof.
- Installer files and `website/` are untouched.

## Recommended PR sequence

| PR | Contents | May ship alone? |
| --- | --- | --- |
| 1 | Work package 1: certainty-aware retry and honest unknown-outcome copy | Yes. This is the safety fix. |
| 2 | Work package 2: measured Codex fixtures, manifest rule, paired verification capture | Yes, after PR 1. |
| 3 | Work package 3: held receipts, attention-row details, troubleshooting correlation | Yes. No scheduling change. |
| 4 | Work package 4: vendor-isolated hook artifacts and safe instructions | Yes. Keep separate from delivery. |

Do not combine these into a delivery-module cleanup. Small reviewable changes
make the irreversible-write boundary easier to audit.

## Explicitly deferred

These ideas are supported by the research but are outside this improvement:

- Caller-generated socket idempotency keys and persisted request/result
  replay. Useful for a lost socket response, but not implicated in the
  confirmed duplicate-paste incident.
- A new public `outcome_unknown` delivery state or a replacement state
  machine. Existing `attention_required` plus an exact cause is sufficient
  for the small fix.
- Automatic vendor hook merges, dynamic label-free hook identity, setup
  receipts, doctor/remove flows, or plugin distribution. Treat the hook audit
  as the design input for a later project.
- Operator acknowledgement, cancel, or requeue of old attention items. Any
  such action must append a legal transition; it cannot hide or rewrite the
  original line.
- Parallel delivery workers per target or bypassing a held FIFO head.
- Retry-by-delay, longer verification sleeps, or a polling reconciler.
- A headless vendor protocol backend. Preserve the injector seam but do not
  use this bug as a reason to replace tmux delivery.

## Verification gates

Run focused tests while developing:

```bash
cargo test -p cyclopsd --test m1
cargo test -p cyclopsd --test m1_blockers
cargo test -p cyclops-manifest --test shipped_rules
cargo test -p cyclops --test hooks_cli
cargo test -p cyclops-ui
```

Then run the repository gates in order from a plain shell outside tmux:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
python3 scripts/check-doc-paths.py
./tests/e2e/parity-check.sh
```

Use `cyclops_testrig::TmuxServer` for every automated tmux test and
`cyclops_proto::scratch::scratch_dir` for scratch state. The two documented
timing-sensitive theme tests on this VM are not messaging failures and must
not be changed as part of this work.

## Definition of done

The improvement is complete when:

- the confirmed ambiguous-write path cannot issue a second paste;
- current Codex collapsed multiline input is backed by a probe, finding, and
  scrubbed paired fixtures;
- receipts distinguish queued, held, and unknown terminal outcomes without
  changing FIFO behavior;
- manual hook preparation cannot overwrite another vendor's artifact and no
  instruction encourages replacing shared config;
- authoritative docs and real output transcripts agree with the binaries;
- all five repository gates pass; and
- the generated `.agents/summary/` knowledge base is refreshed after the
  milestone, without hand-editing its generated pages.
