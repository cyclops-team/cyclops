# Current execution queue

**Status:** Current execution queue

**Implementation authority:**
[Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md)

**Integration branch:** **beta/messaging-rework**

Cyclops is executing the Messaging Beta Rework as independently verifiable
milestones. This page states what runs next. The charter owns rationale,
preserved behavior, stop conditions, and rollback requirements.

## Current milestone

The documentation authority repair runs on
**docs/messaging-refactor-authority**. It approves and repairs the charter,
establishes [HANDOFF.md](HANDOFF.md) as the front door, restores the architecture
review method, repairs documentation status and links, makes the shipped
Cyclops skill the emergency-doctrine source of truth, and synchronizes the
installed copy. Its only Rust source change is the current shipped-skill hash
required by the existing seeding contract.

Exit evidence:

- the documentation checker reports zero broken references;
- the documentation checker reports zero unindexed retained pages;
- the diff contains no messaging runtime, command, journal, UI, workflow,
  installer, or website behavior change; and
- the pull request targets **beta/messaging-rework** and is not merged without
  operator approval.

## Milestone queue

1. **fix/beta-frame-contract**: implement only the end-to-end bounded official
   daemon frame contract. The shared limit is 1,048,576 JSON-object bytes,
   excluding the newline. This is a reliability prerequisite, not the
   `WorkspaceMessaging` extraction.
2. **refactor/beta-daemon-client**: consolidate official framing, correlation,
   timeout, uncertainty, gap, reconnect, and recovery semantics only after
   Milestone 1 is accepted.
3. **refactor/beta-workspace-messaging**: introduce one narrow internal
   `WorkspaceMessaging` operation family and prove that callers lose messaging
   knowledge.

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
- Do not merge a pull request or publish a release without operator approval.
- After the approved beta scope, run fresh architecture, regression,
  performance, migration, and user-journey audits before the final pull request
  from **beta/messaging-rework** into `main`.

## Milestone 1 session boundary

Start a fresh implementation session after the documentation pull request is
accepted. Use this scope:

> Implement only Milestone 1 from the approved Messaging Refactor Charter: the
> end-to-end bounded official daemon frame contract. Do not begin Daemon Client
> consolidation, WorkspaceMessaging extraction, crate extraction, UI redesign,
> CI restructuring, legacy deletion, MCP work, or later milestones. Preserve
> historical replay and honest uncertainty. Stop if any charter stop condition
> is encountered.

## Release naming gate

Verified on 2026-08-29: the newest remote tag is `v0.2.0-beta`, created
2026-08-27; GitHub has no Release objects; and the repository-root `Cargo.toml`
declares `0.1.0`. Reconcile the version, tag, and release authorities before
assigning or publishing the final beta version number.

## Not authorized by this queue

No current milestone authorizes UI redesign, CI restructuring, legacy
deletion, runner or host-adapter production work, MCP work, broad rewriting,
automatic raw-tmux fallback, a complete data-lifecycle policy, or release
publication. Preserve every currently readable journal format throughout this
refactor. Until a complete lifecycle policy is approved, allow no silent
deletion, truncation, or rewriting; a breaking migration requires an explicit
export or migration path.
