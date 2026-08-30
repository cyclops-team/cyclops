# Current beta execution queue

**Status:** Whole-product beta implementation authorized

**Integration branch:** **beta/messaging-rework**

The [Cyclops Beta Charter](CYCLOPS_BETA_CHARTER.md) controls remaining scope,
dependencies, stop conditions, and release gates. The
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md) continues to
control accepted Track A behavior.

## Current implementation set

The primary tracer is **beta/fix/tmux-focus-context**. The independent
**beta/fix/version-identity** slice may run in parallel. Work in flight is
reported by its pull request and checks rather than copied into this page.

After those slices merge, **beta/test/first-run-journeys** assigns the
representative journeys to their cheapest honest evidence lanes before Track C.

After direct user correctness, continue through Tracks C, D, F, B, E, G, H,
and I, then run the final whole-beta responsibility and user-journey audit. The
charter records their responsibilities and dependencies.

## Working rule

Use one focused branch, worktree, and pull request per coherent slice. Parallel
work is welcome when owners, files, and unsettled interfaces do not overlap.
Serialize shared invariants and merge each slice only after focused regression
evidence, independent review, and required checks are green. Update this queue
at merge boundaries, not during routine implementation.

## Release boundary

Do not merge **beta/messaging-rework** into `main`, create or move a tag, choose
the final public beta version, or publish a release without explicit operator
approval. Preserve readable journals, honest uncertainty, no-silent-loss,
`Daemon::deliver_payload` compatibility, all three messaging visibility modes,
and the prohibition on automatic raw-tmux fallback.
