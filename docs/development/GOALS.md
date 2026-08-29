# Cyclops Goals

Admin-set quality bar, 2026-08-02. Every milestone is reviewed against this
document plus ADR-001 and the validation amendments. Where the two conflict,
this document wins on experience, ADR-001 wins on architecture.

North star: a human and any terminal agents, one team. Coordination that feels
invisible, on a record that never lies.

## Messaging layer

- Every message ends in a named state (delivered_verified, delivered_unverified,
  queued, parked, attention_required). Limbo is a bug. Receipts never conflate
  hook- vs screen-verified, and carry target state.
- Idle-target timing: send to pasted under 1s, receipt under 2s. Queued
  deliveries land within 1s of turn end. Ordering holds per recipient.
  Broadcast is one ledger fact with N tracked deliveries.
- Ledger is truth: append-only, jq-able, replayable, zero loss across crashes.
  Push state, pull context. No command force-feeds tokens.

## Human layer

- Admin stream stays calm: only messages aimed at the human plus states that
  need them. Firehose is one keypress away. Ping on blocked/done/parked,
  silent otherwise. Signal to pane in a keystroke. Everything the daemon
  knows, the human can read plainly.

## Frontend design: the terminal is a canvas, not a log

- Signature element, the one deliberate risk: **the eye**. Cyclops's mark is
  the attention indicator. Closed when calm, opening when something needs you.
  Lives in the stream header, pane badges, and `status`. One memorable device;
  everything else stays quiet.
- Typography here is spacing, weight-via-color, and alignment. Strict grid:
  aligned timestamp gutter, hanging indents, one column rhythm across `list`,
  `status`, and the stream. Whitespace is a feature. Density modes
  comfortable/compact, both readable at firehose speed.
- Exactly two encodings carry meaning: role color and state glyph. Identical
  across CLI, borders, stream, docs. Never color alone: compact surfaces may
  use the fixed glyph by itself; roomy and diagnostic surfaces pair it with
  the word. Badges keep one voice: "✓ delivered · verified"; hollow check
  means unverified.
- Themes are semantic token files (role.*, state.*, surface, accent, badge.*),
  never raw colors in code. Ship at least 3 (dark, light, high-contrast) on
  site identity. Truecolor with 256-color fallback. Honor NO_COLOR and a
  --plain screen-reader mode.
- Motion with restraint: stream lines arrive without reflow jumps; glyphs
  tick, never blink; nothing spins for attention. Keypress feedback under
  50ms; reduced-motion respected.
- Structure is information: markers, dividers, numbering only where order or
  grouping is real. Empty states invite the next action. First run is a guided
  three-step moment, not a blank pane.
- Copy is design material: plain verbs, sentence case, user-side words (no
  "NDJSON", no "pane_id" facing humans). Actions keep one name through a
  flow. Errors: what happened, why, next step. No apologies.
- Layout presets are designed, not arranged: `ops` docks the stream at
  deliberate ratios; resize-stable; borders read `role • state` at a glance.
  Default output should look like a product screenshot.

## Usability

- Valuable at n=1. The ladder is law: one pane, name panes, any terminal
  agent, persist layouts, structured messages, pipe output. Roles optional;
  nothing forced. 60 seconds from install to first delivered message, no docs.
  `?` shows a cheatsheet.

## Smoothness and performance

- Zero polling. Event-driven; reconcile on doubt; idle CPU near zero. UI
  never blocks on the daemon. Stream stays fluid past 10k entries. Startup
  feels under 300ms. Crashes lose nothing; restart reconciles silently.

## Reliability invariants

- Never type into the wrong pane: resolve, gate, verify, submit. Always.
- Never clear a modal generically: per-CLI vocabulary with explicit declines,
  or park + alert.
- blocked_quota never auto-retries.
- Cyclops writes terminal notifications only after fresh positive composer and
  occupant evidence. This minimizes concurrent-input risk; it cannot eliminate
  the final observation-to-write interval without cooperative input ownership.
- Secrets never enter the ledger.
- Vendor quirks are data, not code.

## Extensibility and anti-goals

- New agent CLI = one TOML manifest, no code. Delivery behind an adapter.
  Versioned, tolerant protocol. Anything the UI does, scripts can do.
- No PTY hosting, no GUI app, no cloud, no telemetry, no forced hierarchy, no
  idle CPU or token spend without intent.

## Documentation and repo

Docs are part of "done." Every milestone updates them in the same commit as
the code.

- Style: direct, succinct, comprehensible. Short sentences, active voice,
  task-first headings, examples over explanation. No filler, no marketing
  voice, no walls of text. Any page that cannot be skimmed in 60 seconds
  gets split or cut.
- Structure: README is a 60-second quickstart that follows the progressive
  ladder. docs/ is one page per question (install, send, history, ui,
  themes, manifests, troubleshooting). CHANGELOG.md and STATUS.md always
  current.
- Truth rule: a doc describing behavior that no longer exists is a bug.
  Fix or delete it in the same commit that changes the behavior. Never
  document aspirationally; only what is built and tested.

## Done-well test

A stranger runs `cyclops start`, wires two agents, passes reviewed work
between them, and can audit it months later.
