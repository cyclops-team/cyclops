# Current execution queue

**Status:** Current execution queue

**Implementation authority:**
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md)

**Integration branch:** **beta/messaging-rework**

The charter owns rationale, preserved behavior, stop conditions, and rollback
requirements. This page names only the active work and what follows it.

## Active milestone

Milestone 6 runs on **beta/refactor/presentation-seams**.

This milestone must leave three honest, distinct recovery paths:

- `events.subscribe` is ephemeral push and invalidation only;
- authoritative snapshots rebuild current projections; and
- `messages.follow` pages durable mailbox progress.

The current slice adds daemon-owned, body-free `events.backfill` for each
stream connection epoch, moves the shared blocking and async transport into
`cyclops-client`, and supplies pane focus as a launcher-owned terminal effect.
Reusable presentation no longer opens journal paths, constructs Unix sockets,
or invokes tmux. Existing backfill ordering, visible gap reporting, message
snapshot/follow recovery, and both UI journeys remain protected by focused
tests.

Milestone 6 exits when:

- both UIs rebuild authorized state through daemon projections and durable
  follow pages;
- slow or disconnected subscribers recover without treating ephemeral events
  as truth;
- reusable presentation owns no socket, journal, tmux, or messaging mechanism;
- legacy subscribe cursor input remains wire-tolerant without promising replay;
  and
- full, compact, reconnect, gap, and startup-race regression evidence passes.

## Remaining product queue

1. **beta/feat/collapsed-messages-cue**: add the missing stateful cue to the
   collapsed full-workspace rail while preserving the existing body-free tmux
   border count and intentionally chrome-free manual-inbox journey.
2. Run fresh beta architecture, regression, performance, migration, and
   user-journey audits.
3. Report beta readiness and stop for operator approval before any pull request
   from **beta/messaging-rework** into **main** or any release publication.

Milestones 1 through 5 are integrated. The Milestones 3 and 4 continuation
passes completed the charter's responsibility audit: `WorkspaceMessaging` owns
the assigned durable decisions, observation returns immutable evidence, and
ordinary callers no longer coordinate messaging projections, locks, workers,
or post-commit scheduling. The three authorized CI branches are also
integrated: **beta/ci/foundation**, **beta/ci/deterministic-tests**, and
**beta/ci/evidence-lanes**.

## Branch and review rules

- Do not develop directly on **main** or make routine commits directly on
  **beta/messaging-rework**.
- Each remaining milestone gets a focused pull request into the integration
  branch, regression evidence, review, and rollback point.
- Merge a milestone pull request when required evidence, review, and CI are
  green, then continue directly.
- Do not merge the beta integration branch into **main** or publish a release
  without operator approval.

## Preserved boundaries

`cyclops-delivery-core` is conceptual shorthand for the modular messaging core,
not authorization for a crate. Preserve every currently readable journal
format, `Daemon::deliver_payload` as compatibility-sensitive with support
status unverified, honest uncertainty, and the interim no-silent-deletion rule.
No milestone authorizes automatic raw-tmux fallback, a generic broker or
workflow engine, MCP production work, or a broad rewrite.

Release identity remains unresolved: the newest remote tag, GitHub Release
objects, and the workspace version must be reconciled before naming or
publishing the beta.
