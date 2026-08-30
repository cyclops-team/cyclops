# Current execution queue

**Status:** Current execution queue

**Implementation authority:**
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md)

**Integration branch:** **beta/messaging-rework**

Cyclops is executing the Messaging Beta Rework as independently verifiable
milestones. This page states what runs next. The charter owns rationale,
preserved behavior, stop conditions, and rollback requirements.

## Current milestone

Milestone 5 runs on **beta/refactor/legacy-compatibility**. It quarantines only
the proven compatibility-sensitive legacy writers and readers after completing
their caller census and focused replay evidence. Milestone 4 moved positive
quota-reset consequences out of fusion: pane observation now returns immutable
evidence, while `WorkspaceMessaging` commits the durable reset facts and decides
the explicit administrator notices that the daemon composition root commits in
PR #109. Milestone 3 put send and reply
acceptance behind the internal `WorkspaceMessaging` Module. Milestone 2
consolidated official transport semantics in PR #104, and Milestone 1 merged in
PR #103 after the documentation authority repair in PR #101.

Exit evidence:

- the caller census identifies every repository use of compatibility-sensitive
  legacy paths, including `Daemon::deliver_payload`;
- proven legacy writers and readers sit behind one explicit compatibility
  boundary without claiming unverified public support;
- retained replay fixtures and restart traces define the readable history
  boundary; and
- focused boundary tests and all required repository gates pass.

## Milestone queue

1. **beta/fix/frame-contract**: implement only the end-to-end bounded official
   daemon frame contract. The shared limit is 1,048,576 JSON-object bytes,
   excluding the newline. This is a reliability prerequisite, not the
   `WorkspaceMessaging` extraction.
2. **beta/refactor/daemon-client**: consolidate official framing, correlation,
   timeout, uncertainty, gap, reconnect, and recovery semantics only after
   Milestone 1 is accepted.
3. **beta/refactor/workspace-messaging**: introduce one narrow internal
   `WorkspaceMessaging` operation family and prove that callers lose messaging
   knowledge.
4. **beta/refactor/observation-messaging**: separate observation and messaging
   responsibilities without changing durable behavior.
5. **beta/refactor/legacy-compatibility**: quarantine compatibility-sensitive
   legacy paths after the caller census and focused regression evidence.
6. **beta/refactor/presentation-seams**: make snapshot, follow, event, and
   presentation seams honest and explicit.
7. **beta/feat/collapsed-messages-cue**: add the missing collapsed-workspace
   messaging cue while preserving all three approved visibility choices.

`cyclops-delivery-core` was an earlier name for the same modularity goal now
called `WorkspaceMessaging`. It does not authorize a crate, a second messaging
system, or an extraction debate. Do not create a crate unless the internal
Module later proves that a crate deletes additional caller knowledge or
provides measurable isolation, and the operator separately approves it.

## Branch and review rules

- Do not develop the beta rework directly on `main`.
- Do not make routine implementation commits directly on
  **beta/messaging-rework**.
- Give each milestone its own branch, pull request into the integration branch,
  regression evidence, review, and rollback point.
- Do not begin a later milestone inside an earlier pull request.
- Keep the integration branch synchronized with `main` through reviewed merges.
- Merge milestone pull requests when their required evidence, review, and CI
  are green. Do not merge the beta integration branch into `main` or publish a
  release without operator approval.
- After the approved beta scope, run fresh architecture, regression,
  performance, migration, and user-journey audits before the final pull request
  from **beta/messaging-rework** into `main`.

## Milestone 5 session boundary

Use this scope:

> Implement only Milestone 5 from the approved Messaging Refactor Charter:
> quarantine proven legacy writers and readers behind an explicit compatibility
> path. Complete the caller census first, preserve every currently readable
> journal format, and use replay fixtures plus restart traces to define the
> supported history boundary. Do not substantially change or remove
> Daemon::deliver_payload until its census is complete. Do not begin
> presentation work, UI redesign, broad CI restructuring, MCP work, or later
> milestones. Preserve honest uncertainty and stop if any charter stop
> condition is encountered.

## Release naming gate

Verified on 2026-08-29: the newest remote tag is `v0.2.0-beta`, created
2026-08-27; GitHub has no Release objects; and the repository-root `Cargo.toml`
declares `0.1.0`. Reconcile the version, tag, and release authorities before
assigning or publishing the final beta version number.

## Not authorized by this queue

No current messaging milestone authorizes UI redesign, broad CI restructuring, legacy
deletion, runner or host-adapter production work, MCP work, broad rewriting,
automatic raw-tmux fallback, a complete data-lifecycle policy, or release
publication. The separately authorized CI workstream remains isolated on its
own branches and pull requests. Preserve every currently readable journal
format throughout this refactor. Until a complete lifecycle policy is approved,
allow no silent deletion, truncation, or rewriting; a breaking migration
requires an explicit export or migration path.
