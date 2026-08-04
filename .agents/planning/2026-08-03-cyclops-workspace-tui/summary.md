# Cyclops Terminal Workspace UI — Project Summary

PDD run completed 2026-08-03. This project turns the rough idea — a
polished, Herdr-feeling terminal workspace for Cyclops — into a reviewed
design and a 14-step implementation plan.

## Artifacts

- `rough-idea.md` — the initial concept
- `idea-honing.md` — nine questions and answers (Q&A requirements
  clarification)
- `research/` — ratatui/crossterm, Herdr's UI internals, tmux control
  mode, terminal compatibility, and the UI-action → tmux-operation map
- `design/detailed-design.md` — the standalone design, audited against
  `docs/INVARIANTS.md` and `docs/STYLE.md` (rule alignment table in
  Appendix D)
- `implementation/plan.md` — 14 incremental steps with a progress
  checklist
- `summary.md` — this document

## Design in one paragraph

Bare `cyclops` opens a full-screen composite workspace: Cyclops draws
everything (sidebar, tabs, menus, and live pane content) via a per-pane VT
emulator fed by tmux control mode. tmux stays the multiplexer and source
of layout truth (workspace = session, tab = window), `cyclopsd` stays the
brain (states, attention, labels, delivery), and the UI is a pure client
with no private state. v1 ships full mouse manipulation (drag, resize,
context menus, selection-to-clipboard) with a deliberate fidelity floor
(common ANSI/VT; image protocols and pane mouse forwarding deferred).
`cyclops watch` absorbs the stream TUI; `cyclops watch --json` keeps the
machine stream byte-identical.

## Implementation shape

The plan front-loads the two riskiest pieces — VT engine fidelity (Step 1
decides `alacritty_terminal` vs `libghostty-vt` on a fixture corpus) and
the streaming control-mode pipeline (Steps 2–3) — so a live single-pane
workspace exists by Step 4. From there each step is demoable: layout and
tabs (5), structural mutations with reconciliation (6), the sidebar and
project workspaces (7), failure resilience (8), the mouse story (9–11),
agent decoration and the event panel (12), persistence (13), and the
command-surface rename with CI-verified docs (14). Tests ride along in
every step — the fixture corpus, `cyclops_testrig::TmuxServer`-backed
integration, Ratatui `TestBackend` frames, and extended guard tests
(including a new no-interval-timers guard).

## Next steps

1. Review `implementation/plan.md`; reorder or split steps if review
   disagrees with the sequencing.
2. Begin Step 1 (the VT fixture corpus) — it is pure Rust, needs no tmux,
   and its outcome (the engine decision) shapes everything downstream.
3. Keep the plan's checklist current as steps land.

## Areas that may need refinement during implementation

- **Engine choice fallout.** If `alacritty_terminal` misses agent-TUI
  behaviors the corpus can't paper over, the `libghostty-vt` fallback
  brings a Zig build dependency into CI — worth a deliberate decision, not
  a drift.
- **Hydration edge cases.** The capture-then-stream boundary is honest by
  design but will surface quirks (saved cursors, partial escapes) that
  belong in `findings.md` as they are measured.
- **Drag-drop geometry mapping** (Step 11) has the widest gap between
  preview and tmux's resolution; expect iteration on drop-zone semantics.
- **Clipboard transport** (OSC 52 vs native) varies by terminal; Step 10
  measures rather than assumes.
