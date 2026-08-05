# Cyclops recommendation implementation instructions

You are the lead senior engineer for Cyclops, with deep expertise in Rust,
Ratatui, Crossterm, terminal emulation, tmux, performance-sensitive event
loops, and maintainable systems architecture.

Implement the complete recommendation plan at:

`.agents/planning/2026-08-03-cyclops-workspace-tui/recommendation.md`

You are the primary intelligence, architect, and integration owner. You must
make the architectural decisions, manage dependencies, review every change,
resolve conflicts, and verify the final result. Delegate bounded
implementation tasks and most code changes to fast, lower-cost subagents
whenever file ownership allows it. Do not delegate overall reasoning,
architectural judgment, integration, or final verification.

## Establish the G0 baseline

Use a fresh context after the current `cyclops-workspace` bug-fix run is
finished. Before beginning plan work, confirm that the previous agent:

1. committed only its bug fixes and regression tests, excluding
   recommendation-plan work and unrelated existing changes;
2. reported the commit hash and tests it ran;
3. stopped any subagents from the bug-fix run; and
4. left no unexplained worktree changes.

Treat that commit as the recommendation plan's G0 baseline. Do not begin plan
work while another agent is still modifying `cyclops-workspace` or while the
baseline remains unclear.

## Prepare

Before making changes:

1. Read `AGENTS.md` and every document it requires for the affected area,
   especially `docs/HANDOFF.md`, `docs/INVARIANTS.md`, and `docs/STYLE.md`.
2. Inspect the worktree and preserve all pre-existing user changes.
3. Read the entire recommendation, including its dependency graph, execution
   waves, UI decisions, performance requirements, and structural migration
   plan.

Follow the recommendation's task IDs and dependencies. Parallelize only tasks
marked as safe to run concurrently. Assign subagents narrow tasks with
explicit prerequisites, file boundaries, acceptance criteria, and targeted
tests.

## Coordinate implementation

- Give `app.rs`, `render.rs`, and `Cargo.lock` only one active owner each.
- Keep `cyclops-tmux`, `cyclops-ui`, pane-runtime, documentation, and skill
  work isolated where possible.
- Do not create a roaming cleanup agent that modifies files owned by other
  tasks.
- Prefer deleting obsolete code, dependencies, wrappers, branches, imports,
  and allowances over adding abstractions.
- Review every subagent diff before integrating it.
- Integrate work in dependency order, even when it was developed
  concurrently.
- Commit after every logical major change rather than accumulating an entire
  execution wave. Keep each commit focused, include only that change, and run
  its relevant targeted tests before moving to the next major change.
- Personally handle small integration fixes when delegation would be slower
  or riskier.

## Preserve the product decisions

- Compact workspace surfaces use the stable status glyphs without redundant
  words.
- Pane split controls remain always visible.
- The workspace event panel remains available and renders the same stream
  model as `cyclops watch`.
- Dragging a workspace row shows a live horizontal insertion rule at the
  prospective drop position.
- UX remains responsive, visually stable, theme-semantic, and understandable
  without relying on color.

Measure before optimizing. Add the required fidelity fixtures and performance
baselines before changing the pane runtime or latency-sensitive paths. Do not
claim an improvement without evidence.

Keep behavioral refactoring separate from the repository layout migration.
Run Q1 before beginning M1, freeze behavior changes during M1, and let one
agent own all moves and path updates. Moving `frontend/` to `website/` is
authorized by this plan, but do not otherwise redesign or modify website
content.

Run targeted tests for each task and all required repository gates at Q1 and
Q2. Update documentation and findings from real measured output, following
the repository's parity and path-verification rules.

Continue until the complete recommendation is implemented and verified.
Pause only for a genuine blocker, destructive ambiguity, or a decision that
would materially depart from the recommendation.

Provide concise progress updates while continuing automatically. Do not pause
for acknowledgment after routine updates.
