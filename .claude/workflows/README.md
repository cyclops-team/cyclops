# Milestone workflows

The build queue for Cyclops v2, one named workflow per milestone. Launch by
name from a Claude session in this repo:

| Order | Workflow | Ships | Gate before it runs |
|---|---|---|---|
| 1 | `m1-delivery` | ledger + msg.send + state machine + ACK tiers + quota parking + 100-msg mini-soak per CLI | M0 committed, workspace green |
| 2 | `m2-messaging` | history/thread/broadcast/fyi, agent.wait, hooks install + self-test, v1 shim PREPARED | M1 committed + soak artifacts |
| 3 | `m3-stream-ui` | cyclops ui (admin stream + firehose), theme engine, the eye | M2 committed |
| 4 | `m4-pane-ux` | name/list, live role•state chrome, layout presets, workspace save/restore, start | M3 committed |
| 5 | `m5-polish` | three themes + hot reload, docs, README ladder quickstart, parity check | M4 committed |
| 6 | `m6-flow` | pipe, attention routing, --wait ergonomics | M5 committed |

Every workflow starts with a read-only preflight that throws unless
`cargo test --workspace` is green and `git status` is clean, so launching one
early fails fast instead of building on a broken base.

Structure shared by all: Preflight, parallel Implement agents with strict
file-ownership boundaries, an Integrate agent that makes the workspace green
and runs the demo, adversarial Review agents with file:line evidence.
The orchestrating session (not any agent) writes STATUS.md and makes the
conventional commit after reviewing results; agents never run git.

Hard stops that stay with the admin regardless of workflow results:
- Installing the commPact v1 shim or touching anything under ~/.commPact
  (m2 only PREPARES scripts/commpact-shim + docs/CUTOVER.md).
- Wiring real hook configs into ~/.claude, ~/.codex, ~/.gemini, .agents.
- Publishing, tags, releases, remotes, pushes.
- M7 (narrator) and M8 (dogfood) are deliberately not queued: M7 is
  flag-gated design-only until the admin asks, M8 needs a stable M2 and an
  explicit go.
