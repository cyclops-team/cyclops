# Current beta execution queue

**Status:** Technical beta-acceptance decision complete for the audited
candidate; operator release decision required

**Integration branch:** **beta/messaging-rework**

The [Cyclops Beta Charter](CYCLOPS_BETA_CHARTER.md) controls beta scope,
stop conditions, and release gates. The
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md) continues to
control accepted Track A behavior.

The approved implementation tracks are integrated into
**beta/messaging-rework**. The [final beta acceptance audit](CYCLOPS_BETA_FINAL_AUDIT.md)
records the completed technical decision. Pull requests and their checks remain
the authority for work in flight.

## Current work

1. Reconcile release identity before naming or publishing a beta: the Cargo
   workspace version, remote tag history, and GitHub Release state must agree.
2. If a candidate-relevant merge lands, run the release-evidence lane against
   that exact resulting candidate before treating it as release-ready.

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
