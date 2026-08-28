# Next architecture work

Cyclops is stabilized around a durable mailbox, guarded terminal notification,
stable identity, and honest receipts. The next priority is to make that working
delivery system easier to understand and change without altering its behavior.

## High priority: delivery core extraction

`cyclopsd` currently combines pure delivery decisions with journal access,
process evidence, tmux IO, timers, supervision, and RPC coordination. The main
delivery and mailbox modules are large enough that a local change can require a
repository-wide forensic pass before its ownership is clear.

Extract a sans-IO `cyclops-delivery-core` whose central operation is:

```text
(state, input) -> (state, effects)
```

The core should own semantic transitions and effect requests. It must not read
tmux, spawn a process, open a socket, sleep, or append a journal. Adapters remain
responsible for executing effects and returning new evidence.

Keep these boundaries explicit:

- mailbox persistence and replay remain the authority for durable facts;
- fusion remains the authority for pane, screen, lifecycle, and process
  evidence;
- the daemon remains responsible for async workers, locks, timers, RPC, and
  effect execution;
- tmux interaction remains inside `cyclops-tmux`;
- protocol and journal schemas remain in `cyclops-proto`.

The extraction is behavior-preserving. It is not permission to redesign the
wire protocol, change receipts, weaken a gate, or replace the journal.

### Safe sequence

1. Extract closed input, state, decision, and effect types while the existing
   path remains authoritative.
2. Move pure composer-hold transitions behind equivalence tests.
3. Move pre-write gate ordering behind table and mutation tests.
4. Move notification attempt and FIFO decisions behind replay tests.
5. Move post-write verification and recovery decisions behind crash-boundary
   tests.
6. Route one narrow production path through the core at a time, retaining a
   comparison seam until its evidence is green.
7. Remove duplicated decisions only after the extracted path passes the full
   workspace gate and the opt-in frozen evidence components.

Every slice must preserve the invariants in [INVARIANTS.md](INVARIANTS.md), add
no unbounded retry, hold no lock across IO, and keep ambiguous evidence
non-successful. Small PRs are a design requirement, not process ceremony.

## Medium priority

- Add an exact-attempt administrator `release_hold` action only if real use
  shows a hold that cannot recover from visible input becoming settled and
  exactly empty. The current automatic backspace-to-empty path covers the
  normal operator workflow, so this is not a release blocker.
- Continue reducing test wall time through measured removal of duplicated work
  and event-driven synchronization. Do not narrow platform or relocated-root
  coverage without mutation evidence for the defect classes it protects.
- Review tracked planning summaries and historical benchmark artifacts once
  their lasting documentation value can be judged independently of disk
  cleanup.

## Deferred

These are explicitly outside the current stabilization target:

- large-fleet supervisory UI and thread-folding features;
- live Cursor evidence beyond the shipped conservative offline manifest;
- automatic or polished raw-tmux fallback behavior;
- distributed transport or mesh work;
- general cancellation, supersession, and artifact retention systems.

Deferred work may not weaken mailbox durability, composer safety, stable
identity, FIFO ownership, crash recovery, or receipt honesty.
