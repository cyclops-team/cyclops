# Status

Updated 2026-08-02. M1 complete: gate passed, reviews cleared. M2 next.

## Done

- M0 shadow daemon (ffa4b5b): control-mode attach, zero-polling pane table,
  fusion, socket API, branded CLI. Audited; startup 107ms.
- M1 delivery pipeline (416056d + this commit): msg.send end to end with
  per-recipient FIFO workers, the full gate order, unique-buffer injection
  from 0600 spool files, composer verification (id-anchored, stale-text
  proof), ACK tiers with detach-frozen deadlines and a post-reattach
  evidence pass before any retry, late-ACK upgrade, codex hook dedupe with
  reset-safe seq handling, single bounded retry, quota parking, manifest
  modal declines with TOCTOU re-checks, occupant re-check before paste AND
  submit (pane_rebound), restart limbo closure at boot, broadcast, spec
  receipts, fail-closed peer-cred identity, argv-fallback manifest binding
  for native installs, escaped-capture ghost/typed discrimination for
  codex, `cyclops send` + `cyclops hook`. 196 tests green workspace-wide.

## M1 gate evidence

- Soak (tests/raw/m1-soak-2): 221 deliveries, zero unrecovered loss, zero
  retries, zero duplicates, zero control drops in 252s. claude 100/100
  delivered_verified (ack p50 12ms) through the real argv binding; codex
  100/100 (66 hook-verified, 34 screen-tier); agy 20 delivered then a
  genuine vendor quota park, the F11 chain end to end.
- Reviews: correctness PASS (all seven blockers verified fixed); invariants
  BLOCK twice, both HIGHs closed in the finishing pass and re-verified PASS
  (196 tests, clippy clean, regression hunt empty).
- Root causes worth remembering: F22 (tmux 3.6a emits invalid UTF-8 on
  control-mode notification lines when multi-byte glyphs split across pty
  reads; UTF-8 line decoding read a live connection as dead; Claude-only
  because only Claude streams braille), F19-F21 in findings.md.

## Next

- Push this commit to origin/v2, launch m2-messaging: history/thread,
  agent.wait, hooks install + startup self-test (amendment c), v1 shim
  PREPARED with the cutover held for admin.

## Backlog (from final verification, non-blocking)

- codex marker_in_composer consults only plain rules, so tier-2's
  "marker left the composer" conjunct is vacuous for codex: a narrow race
  can label delivered_unverified while text sits in the composer (record
  truthfulness, not injection safety). Teach the ACK evidence path the
  escaped capture, or add a plain composer marker regex for codex.
- occupant_unchanged reads the subscription-fed table, so a swap the
  watcher has not seen yet passes the re-check. Accepted residual window,
  consistent with the zero-polling design; documented in DELIVERY.md.
- agy full uninterrupted 100-leg deferred on vendor quota flakiness
  (parked legs count complete per the frozen gate rules).

## Risks

- CI runs remotely for the first time on this push (v2 branch trigger).

## Open questions

- License file before anything publishes (admin decision).

## Deviations from the brief

- None. M1 closure was held twice on its own gate evidence before passing.
