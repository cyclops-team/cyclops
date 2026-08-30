# Current state and operator gates

**Status:** Awaiting operator gates

**Implementation authority:**
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md)

**Integration branch:** **beta/messaging-rework**

The charter owns rationale, preserved behavior, stop conditions, and rollback
requirements. This page records the completed state, remaining operator gates,
and preserved boundaries.

## Current state

All seven messaging milestones and the three authorized CI tasks are
integrated. The post-milestone architecture, regression, performance,
migration, reliability, and user-journey results are recorded in the
[Messaging Beta audit](MESSAGING_BETA_AUDIT.md).

There is no remaining authorized implementation milestone. The queue is now:

1. reconcile the workspace version, remote tag, and GitHub Release authority;
2. obtain operator review of the audit and retained release evidence;
3. obtain explicit operator approval before opening a final pull request from
   **beta/messaging-rework** into **main**, then obtain approval again before
   merging it; and
4. obtain separate operator approval before assigning a version, creating a
   tag, or publishing a release.

## Branch and review rules

- Do not develop directly on **main** or make routine commits directly on
  **beta/messaging-rework**.
- Any post-audit correction gets a focused pull request into the integration
  branch, named regression evidence, review, and a rollback point.
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
