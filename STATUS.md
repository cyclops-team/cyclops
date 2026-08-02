# Status

Updated 2026-08-02. M2 complete: gate passed, both review BLOCKs closed and
re-verified. M3 (stream UI) next. One item waits on admin: the v1 cutover.

## Done

- M0 shadow daemon (ffa4b5b): control-mode attach, zero-polling pane table,
  fusion, socket API, branded CLI. Audited; startup 107ms.
- M1 delivery pipeline (f1b0811): msg.send end to end, ACK tiers with
  detach-frozen deadlines, quota parking, modal declines, occupant re-check
  before paste and submit, restart limbo closure. Gate: 221-delivery soak,
  zero loss, zero duplicates; double adversarial review.
- M2 messaging complete (this commit):
  - msg.history / msg.thread with read-time delivery folding, alias-proof
    filters (recipients canonicalized to labels at send), gapless
    multi-session paging on an opaque composite cursor, `cyclops history` /
    `cyclops thread` on the strict grid. Measured: no ledger index needed
    (10k-line scan 7.3ms). F23 measured: tmux subscriptions tick at 1Hz,
    so title-tier turns shorter than a second are invisible.
  - agent.wait + send-and-wait with occupant pinning at delivery submit;
    `cyclops wait` (exit 0/2/3); wait answers carry outcome; done is
    strictly the working-to-idle edge.
  - hooks install/verify/selftest with occupant-pid-keyed liveness, the F1
    downgrade ping per occupant, and templates that never write vendor
    dot-dirs. Amendment (c) done.
  - agent.state.report is peer-pinned: only a process inside the pane it
    speaks for can report; forged reports are denied and ingest nothing
    (fail-before-proven test). The record cannot be made to lie.
  - commPact v1 shim + guarded installer + docs/CUTOVER.md runbook,
    PREPARED ONLY; ~/.commPact untouched; shim suite wired into CI.
  - Badges: verified is the heavy check, unverified the light check
    (GOALS hollow-check rule; see Deviations).
  - Docs, same commits: history.md, wait.md, hooks.md, send.md updates.
- 277 workspace tests green; clippy -D warnings clean; final verification
  PASS on all nine gate items.

## ADMIN_ACTION_REQUIRED (not blocking the build)

The M2 cutover is ready and waiting on you: docs/CUTOVER.md is the runbook
(preconditions, guarded install of scripts/commpact-shim, parallel window,
verification checklist, rollback). Nothing proceeds there without you;
M3-M6 do not depend on it.

## Next

- M3 stream UI: admin stream + firehose, theme engine seeded from
  frontend/ palette, the eye.

## Backlog (non-blocking)

- codex tier-2 marker evidence still plain-capture-blind (record
  truthfulness nuance, M1 note).
- Accepted hook reports are covered by unit-level ancestry tests plus
  construction; a socket-level in-pane acceptance integration test would
  close the loop.
- Narrow pid-reuse window while a session is detached (last-known table
  trusted for report origin during outages).
- hooks.selftest callable by any same-uid process (costs one trivial turn;
  same trust level as admin msg.send).
- agy uninterrupted 100-leg soak deferred on vendor quota flakiness.

## Risks

- CI runs on push to v2; watch the first runs.

## Open questions

- License file before anything publishes (admin decision).

## Deviations from the brief

- GOALS says "hollow check = unverified"; no portable hollow check glyph
  exists in terminal fonts, so it ships as heavy check (verified) vs light
  check (unverified), words unchanged. Flagged for admin; GOALS.md itself
  untouched.
