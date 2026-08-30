# Current execution queue

**Status:** Current execution queue

**Implementation authority:**
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md)

**Integration branch:** **beta/messaging-rework**

The charter owns rationale, preserved behavior, stop conditions, and rollback
requirements. This page names only the active work and what follows it.

## Active milestone

Milestone 7 runs on **beta/feat/collapsed-messages-cue**.

This milestone completes the full-workspace hidden messaging journey without
changing the other two approved visibility choices. The collapsed one-column
Messages rail projects authenticated, body-free snapshot state:

- `✉` plus `1` through `9`, or `+` for ten or more, reports Work messages;
- `!` reports open attention; and
- `?` reports that no authenticated snapshot exists or the retained one is
  stale.

The cue refreshes after body-free `messages.changed` invalidation and reconnect
without opening the pane. Opening the pane still refreshes the detailed
projection before actions become available. It does not create a second unread
queue, broadcast content, resize the rail, or turn ordinary pane decoration
updates into message reads.

Milestone 7 exits when:

- hidden invalidations fetch an authorized body-free snapshot without forcing
  either panel open;
- current, attention, stale, and unknown rail states have focused rendering
  evidence;
- the existing adopted-tmux body-free count is unchanged;
- native tmux remains intentionally chrome-free with manual inbox inspection;
  and
- full workspace and repository gates pass.

## Remaining product queue

1. Run fresh beta architecture, regression, performance, migration, and
   user-journey audits.
2. Report beta readiness and stop for operator approval before any pull request
   from **beta/messaging-rework** into **main** or any release publication.

Milestones 1 through 6 are integrated. The Milestones 3 and 4 continuation
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
