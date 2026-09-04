# Current execution queue

**Status:** `1.0.2` shipped to `main`.

**Primary branch:** `main`

**Selected package:** `1.0.2`

The [Cyclops Beta Charter](archive/CYCLOPS_BETA_CHARTER.md) and [Messaging Refactor Charter](archive/MESSAGING_REFACTOR_CHARTER.md) record foundational architecture, invariants, and historical beta milestones.

## Current priorities

1. Documentation parity: maintain accurate delivery safety specifications reflecting doorbell best-effort wake semantics and strict direct delivery gating.
2. Production stability: monitor daemon reliability, mailbox FIFO order, and agent lifecycle hooks across Claude Code, Codex, Cursor, and Antigravity.
3. Multi-agent UX: continue refining CLI ergonomics, reply locators, and session management.

## Working rule

Use one focused branch, worktree, and pull request per coherent slice. Parallel work is welcome when owners, files, and unsettled interfaces do not overlap. Serialize shared invariants and merge each slice only after focused regression evidence, independent review, and required checks are green. Update this queue at merge boundaries, not during routine implementation.
