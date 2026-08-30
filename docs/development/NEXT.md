# Current beta execution queue

**Status:** Whole-product beta implementation authorized

**Integration branch:** **beta/messaging-rework**

The approved [Messaging Refactor Charter](MESSAGING_REFACTOR_CHARTER.md)
continues to control Track A behavior and stop conditions. The operator has
authorized the rest of the whole-product beta program. Track 0 will record that
authority in `CYCLOPS_BETA_CHARTER.md`; until then, this queue records the
authorized sequence without reopening already approved Track A decisions.

## Active work

Track A's seven implementation milestones are integrated. Acceptance remains
open for the focused corrections found by the post-completion review:

1. repair source-boundary lints that stopped at early test-only items;
2. make current status, CI, client responsibility, and evidence language agree;
3. protect the body-free collapsed cue across detach and a fresh workspace
   attachment;
4. route current history and thread knowledge through `WorkspaceMessaging`,
   keeping cross-journal history behind an explicit compatibility adapter; and
5. obtain independent review of the corrected claims.

The active branch is **beta/fix/messaging-beta-audit-integrity**. The read-side
production seam follows on **beta/refactor/messaging-history-seam** so audit
repair and durable history behavior remain independent rollback points.

## Authorized sequence after Track A

1. **Track 0:** establish `CYCLOPS_BETA_CHARTER.md`, reconcile documentation
   authority, and convert the whole-system review into an executable queue.
2. **Direct user correctness:** non-default tmux focus context, product release
   identity, and representative user journeys.
3. **Track C:** workspace interaction and tmux continuity.
4. **Track D:** presentation and user experience.
5. **Track F:** CLI, headless use, installation entry path, and product front
   door.
6. **Track B:** runtime observation, identity, and attention.
7. **Track E:** agent integration.
8. **Track G:** installation, update, health, cleanup, and managed assets.
9. **Track H:** configuration, durable state, and data lifecycle.
10. **Track I:** CI, tests, performance, compatibility, and release evidence.
11. Run the final whole-beta responsibility and user-journey audit.

Each slice starts from the latest integration branch, uses one focused beta
branch and pull request, adds the smallest test that proves its contract,
receives independent review, and merges only when required checks are green. A
later track may overlap only when ownership and files do not conflict.

## Release boundary

Do not merge **beta/messaging-rework** into `main`, create or move a release
tag, assign the final beta version, or publish a release without explicit
operator approval. Release identity remains unresolved until the workspace
version, remote tags, installer identity, daemon greeting, and GitHub Release
authority agree.

Preserve every currently readable journal format, honest uncertainty, the
interim no-silent-deletion rule, `Daemon::deliver_payload` as
compatibility-sensitive with public support status unverified, and all three
messaging visibility choices. No track authorizes automatic raw-tmux fallback,
a distributed broker, a generic workflow engine, or an unrelated rewrite.
