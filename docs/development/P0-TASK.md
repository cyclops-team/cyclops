# P0 implementation task: messaging safety

Frozen authorization context. Do not edit the frozen fields.

## Frozen

- Plan: `PLAN.md` revision 17, sha256
  `0ff98a86ed107bc5b29e3cfa513388f8853eec3e099939780575d64ad0607abe`,
  in `~/projects/tasks/2026-08-18-cyclops-messaging-redesign/`.
  Three hash-bound ACKs: claude `m-1705d3`, gemini `m-db9912`,
  codex `m-8262d4`.
- Directive: `ADMIN-DIRECTIVE-P0.md`, same directory, 2026-08-20.
- Base commit: `dfe963a`, the revision all rev 17 evidence was
  gathered against.
- Branch: `p0-messaging-safety`.

Any change to the plan voids the ACKs and requires a new revision.
This task implements P0 only; section 7 of the directive lists what is
not authorized.

## Roles

Claudex implementation and writer of record; Codey adversarial review,
read-only during the initial pass; Gemini evidence, harness and
findings.

## Order of work (directive section 5)

1. Freeze the hash (this file).
2. Failing regressions first, including the
   hook-idle-with-staged-input case.
3. Production logic only after 2.

## P0-A, the atomic safety unit

Ships as one gated unit; no part becomes a production-enabled path on
its own, because better verification widens the set of payload shapes
that submit, which is unsafe while the pre-paste clean-composer
verdict can still be poisoned.

- Issues 16, 18, 19.
- Current-version detection fixtures per vendor.
- Terminal sentinel (below).
- Runtime state and terminal write-readiness as separate answers.
- Server-derived hook identity, not enabled ahead of write-readiness.
- Owner-only state permissions with migration repair.
- Adversarial composer matrix and synthetic sentinel soak, passing.

## Terminal sentinel design

`render_payload` appends a final line of fixed shape:

```
[cyclops:end <msg_id>]
```

Separate from the human-facing reply hint on purpose: transport
verification must not depend on CLI copy that can change.

Verification proves all of: the exact sentinel for this message is
present; it is complete, not wrapped or truncated; it is the final
logical payload token in normalized composer content; nothing
unexpected follows it inside the composer; collapsed-chip handling
still works as the vendor alternate; absence, ambiguity, truncation,
or unsupported decoration fails closed.

Coverage is by REPRESENTATION, not vendor (codey `m-46d277`):
raw-wrapped composer text benefits from the sentinel on any vendor,
a collapsed chip hides it on any vendor and falls back to chip
evidence, and one vendor can produce either.

### Why an additive manifest key is required

Codey's ruling: sentinel syntax and terminality are cross-vendor
transport invariants in generic code; composer normalization and chip
alternates stay manifest data; use an additive schema key only if
existing rule regions cannot drive normalized composer extraction.

They cannot. The `idle_with_input` rules pin the composer's PROMPT
line (`line_regex` such as `^\s*❯\s+\S`) inside a
`bottom_non_empty_lines(n)` region. A wrapped payload puts the
sentinel on a continuation line, not the prompt line, and the rules
say nothing about where the composer ENDS. Terminality therefore
cannot be decided without knowing which trailing lines are vendor
chrome.

Additive key, tolerated by unknown-key parsing (AGENTS.md rule 5):

```toml
[injection]
# Lines that may render BELOW the composer and are never payload.
# Sentinel terminality only: anything after the sentinel that does not
# match one of these fails verification closed.
composer_trailer_regex = ['...']
```

No vendor branches in Rust; the daemon owns "is this the final
normalized payload token", the manifest owns "what may appear below".

## Evidence (directive section 5)

Deterministic adversarial matrix, exhaustive, every cell passing: only
a genuinely empty composer admits a write. Synthetic sentinel soak,
100 trials per vendor minimum and 300 preferred, stage-and-clear
against the real verifier, zero failures required before proceeding.

## Stop conditions

Report and stop rather than working around: the sentinel cannot be
proven terminal on a supported vendor (never widen the scan region as
a workaround); a composer decorates pasted text such that the sentinel
is unrecoverable (record as a vendor limitation, that lane keeps
current behavior); any matrix cell admits a write outside the empty
state; any soak sentinel failure.

## Local-environment boundary

Tests use scratch homes and the testrig tmux server only. The built
daemon is not run against the operator's real `~/.cyclops`, and hooks
are not activated on live panes, without a separate explicit
authorization. Directive D2 forbids the interim hook rewire outright.
