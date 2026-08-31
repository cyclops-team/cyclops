# Current beta execution queue

**Status:** Final acceptance corrections and evidence

**Integration branch:** **beta/messaging-rework**

The [Cyclops Beta Charter](CYCLOPS_BETA_CHARTER.md) controls beta scope,
stop conditions, and release gates. The
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md) continues to
control accepted Track A behavior.

The approved implementation tracks are integrated into
**beta/messaging-rework**. This page names only the next acceptance work;
pull requests and their checks remain the authority for work in flight.

## Current acceptance work

1. Close any focused final-audit correction with its own regression evidence.
2. Run the release-evidence lane against the exact resulting candidate.
3. Reconcile release identity before naming or publishing a beta: the Cargo
   workspace version, remote tag history, and GitHub Release state must agree.

The final audit is not a reason to weaken a contract. Preserve readable
journals, honest uncertainty, no-silent-loss, `Daemon::deliver_payload`
compatibility, all three messaging visibility modes, and the prohibition on
automatic raw-tmux fallback.

## Working rule

Use one focused branch, worktree, and pull request per coherent slice. Parallel
work is welcome when owners, files, and unsettled interfaces do not overlap.
Serialize shared invariants and merge each slice only after focused regression
evidence, independent review, and required checks are green. Update this queue
at merge boundaries, not during routine implementation.

## Release boundary

Do not merge **beta/messaging-rework** into `main`, create or move a tag, choose
the final public beta version, or publish a release without explicit operator
approval.
