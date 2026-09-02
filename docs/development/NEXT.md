# Current beta execution queue

**Status:** Versioned beta candidate in review; exact-SHA release evidence and
an open beta-to-main pull request are required before the operator can decide
whether to promote it

**Integration branch:** **beta/messaging-rework**

**Selected package and release tag:** `0.1.2-beta` / `v0.1.2-beta`

**Historical tag preserved:** `v0.2.0-beta` remains attached to its existing
historical commit. It is not this beta candidate's release tag.

The [Cyclops Beta Charter](CYCLOPS_BETA_CHARTER.md) controls beta scope,
stop conditions, and release gates. The
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md) continues to
control accepted Track A behavior.

The approved implementation tracks are integrated into
**beta/messaging-rework**. The [final beta acceptance audit](CYCLOPS_BETA_FINAL_AUDIT.md)
records the completed technical decision. Pull requests and their checks remain
the authority for work in flight.

## Current work

1. Merge the focused release-identity change.
2. Run the release-evidence lane against its exact resulting beta commit.
3. If that evidence passes, open the final **beta/messaging-rework** to `main`
   pull request. Do not merge it, create the `v0.1.2-beta` tag, create a
   GitHub Release, or publish anything without a subsequent explicit operator
   authorization.

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

The operator selected `0.1.2-beta` / `v0.1.2-beta` and authorized preparation
of the beta-to-main pull request. Do not merge `main`, tag, create a GitHub
Release, or publish until the exact candidate passes the complete
release-evidence lane and the operator gives subsequent explicit approval.
