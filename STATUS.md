# Status

Updated 2026-08-02. M3 is complete and committed: the stream UI, the eye,
the theme engine with state color, 442 tests green. CI is green on Linux
and macOS. v2 has been promoted to main. Next up is M4, pane UX. One thing
still waits on admin: the v1 cutover.

## Done

- M0 shadow daemon (ffa4b5b): control-mode attach, zero-polling pane table,
  fusion, socket API, branded CLI. Audited; startup 107ms.
- M1 delivery pipeline (f1b0811): msg.send end to end, ACK tiers with
  detach-frozen deadlines, quota parking, modal declines, occupant re-check
  before paste and submit, restart limbo closure. Gate: 221-delivery soak,
  zero loss, zero duplicates; double adversarial review.
- M2 messaging complete (8ec10da):
  - msg.history / msg.thread with read-time delivery folding, alias-proof
    filters (recipients canonicalized to labels at send), gapless
    multi-session paging on an opaque composite cursor, `cyclops history` /
    `cyclops thread` on the strict grid. Measured: no ledger index needed
    (10k-line scan 7.3ms). F23 measured: tmux subscriptions tick at 1Hz,
    so title-tier turns shorter than a second are invisible.
  - agent.wait + send-and-wait with occupant pinning at delivery submit;
    `cyclops wait` (exit 0/2/3); done is strictly the working-to-idle edge.
  - hooks install/verify/selftest with occupant-pid-keyed liveness, the F1
    downgrade ping per occupant, and templates that never write vendor
    dot-dirs. Amendment (c) done.
  - agent.state.report is peer-pinned: only a process inside the pane it
    speaks for can report; forged reports are denied and ingest nothing
    (fail-before-proven test). The record cannot be made to lie.
  - commPact v1 shim + guarded installer + docs/CUTOVER.md runbook,
    PREPARED ONLY; ~/.commPact untouched; shim suite wired into CI.
  - Badges: verified is the heavy check, unverified the light check
    (GOALS hollow-check rule; see Deviations).
- CI portability (62d5f9f): F24. Test scratch paths went through
  cyclops_proto::scratch::scratch_root instead of a hardcoded macOS-only
  /private/tmp, and CI gained --no-fail-fast plus fail-fast: false so one
  run reports every failure on both platforms. ubuntu and macos green.
- Repository shape: .claude/ untracked (11c2b9d); v1 history grafted so
  main is an ancestor of v2, tree unchanged (3f45a37); v1 preserved as
  branch `v1` and tag `v1-final`; LICENSE, NOTICE and .gitattributes
  carried from main (17fc43d).
- M3 stream UI and theme engine (this commit). It took six gate rounds and
  the reason is written up under Lessons, because it is the most useful
  thing this milestone produced. What shipped:
  - `cyclops ui`: calm admin view by default, firehose on tab, w/f/t
    filters mirroring the history flags, enter jumps tmux focus to the
    entry's pane through a new adapter-only cyclops-tmux focus helper.
    Routine gate holds stay in the firehose; only holds naming a blocked
    pane reach the calm view.
  - THE EYE, on the stream header, the --plain eye line and `cyclops
    status`. All three read one owner, crates/cyclops-proto/src/attention.rs,
    and none recomputes the rule. Verified by probe, not by reading: the
    count is independent of --backfill on both halves; an item raised
    before a pane is adopted clears after it; a pane that disappears stops
    counting through an additive event with no polling; status and the
    stream agree from one daemon answer. An alarm that reaches the calm
    view now gets a clearance line under it, so a closed eye can never sit
    over a stale warning. A 472,320-frame sweep across both views, both
    densities, four filters and four heights finds zero contradictions;
    the same sweep flagged 7.44% of frames before the fix.
  - Fluidity measured on a 10,000-entry ring: full-ring frame build 0.33
    to 0.35ms median in a debug build, admin view 0.12ms, ingest 3.2ms,
    against a 16ms budget.
  - --plain carries the same content as the sighted comfortable view,
    message bodies included: it is the screen-reader path, not a reduced
    view.
  - crates/cyclops-theme: semantic tokens with data-only theme TOML,
    derived-or-explicit 256-color fallbacks, selection by config `theme`
    key plus CYCLOPS_THEME, hot reload as a stat-on-event with no polling.
    themes/dark|light|high-contrast.toml on the usecyclops.dev palette.
    The vocabulary is role.1-8, surface.fg/dim/accent, eye.calm/eye.alert,
    and the six state.* plus three badge.* tokens restored after the
    misreading described under Corrections. Note: role labels now hash into
    8 slots (was 6), so existing labels may change color once.
  - Docs in the same commit: docs/ui.md and docs/themes.md created,
    ARCHITECTURE and CHANGELOG updated, README given a public face.

## ADMIN_ACTION_REQUIRED (not blocking the build)

### The v2-becomes-main flip: approved, unblocked, runs when M3 lands

Admin approved promoting v2 to main. The prerequisites are done and
pushed, so nothing here is waiting on a person:

- The graft merge (3f45a37) makes origin/main an ancestor of v2, with the
  tree byte-identical before and after, so promoting v2 keeps every v1
  commit reachable.
- v1 is preserved twice over: branch `v1` and tag `v1-final`, both at
  7465307.
- The public installer is pinned (31dcb8e). It served
  https://www.usecyclops.dev/install.sh, defaulted its ref to the main
  branch, and required bin/commPact-install in the downloaded tree, so
  promoting the Rust rewrite would have broken the advertised one-line
  install for every visitor. It now defaults to the v1-final tag, which is
  the same commit main points at today, so nothing changed then and
  nothing breaks after. Verified: the v1-final archive returns 200 and
  carries bin/commPact-install, and extraction uses --strip-components=1
  so the tag-derived directory name does not matter.
- The site itself is safe: frontend/ is byte-identical to main's copy and
  builds through SvelteKit adapter-auto with no committed host config, so
  the deploy is configured host-side and keeps working.

Deliberately NOT carried from main, because both would have clobbered v2
files at the same paths: .github (main's only workflow is ci.yml, the same
path as the Rust CI) and themes/ (main's are v1 tmux .conf files).

Still true and worth a decision before this becomes the public tip: NOTICE
describes v1's staged build, down to SHA-256 sums of bin/commPact files
absent from this tree.

### The M2 cutover

Ready and waiting on you: docs/CUTOVER.md is the runbook (preconditions,
guarded install of scripts/commpact-shim, parallel window, verification
checklist, rollback). Nothing proceeds there without you; M3-M6 do not
depend on it.

## Lessons from M3

M3 took six gate rounds. Three of them were worth it and three were not,
and the split is worth writing down.

Worth it: rounds 1 to 3 found the eye reporting "All calm" over a delivery
parked on quota, and the eye sticking open forever when a pane blocked
before it was labelled. Both would have shipped. An attention indicator
that lies is worse than none, because it is the one thing a person glances
at instead of reading.

Not worth it, and all three causes were mine:

1. An unachievable acceptance criterion. Rounds 4 and 5 were spent
   demanding a static test that catches ANY second implementation of the
   rule. That is semantic equivalence detection. A verifier eventually
   defeated it with a duplicate that named no state at all. The guard is
   now scoped to a best-effort tripwire that says so in its own file, and
   review is named as the real defence.
2. Treating every BLOCK as binding without triaging it. Rounds were spent
   on stale CHANGELOG lines and a STATUS contradiction, which are
   ten-minute fixes owned by the orchestrator, not gate failures.
3. Three verifiers per round, each able to block on anything. Right while
   the product was wrong; wasteful once it was right, because three
   adversarial readers will always find something in a 60-file change.

For M4 onward: one implement round, one gate, and the orchestrator triages
the findings. A finding blocks only if it changes what a user experiences.
Everything else is fixed inline or documented.

The one process change that demonstrably worked: verifiers who write
probes instead of reading code. The single reviewer who read code signed
off on the eye twice. The three who ran probes against a real ledger found
it lying in minutes.

## Next

- M4 pane UX: live titles and borders, layout presets, workspace
  save/restore, `cyclops start`. The dead-pane edge below belongs here,
  as does the status eye once StatusResult can carry open deliveries.

## Backlog (non-blocking)

- F25, and it is the sharpest one: cyclops has no reliable dead-pane edge
  on any tmux version. Death has no notification, per-pane subscriptions
  stop being re-evaluated once the pane's process exits (measured on both
  3.6a and next-3.8), and the watcher only catches a death when an
  unrelated event happens to force a resync afterwards. 3.6a wins that
  race by 23ms and tmux next-3.8 loses it by 13ms, which is the whole of
  the red tmux-HEAD job. Scope is `remain-on-exit on` only; the default
  closes the pane and emits a real %layout-change. Fix belongs with the
  pane work: give death its own edge.
- codex tier-2 marker evidence still plain-capture-blind (record
  truthfulness nuance, M1 note).
- Accepted hook reports are covered by unit-level ancestry tests plus
  construction; a socket-level in-pane acceptance integration test would
  close the loop.
- Narrow pid-reuse window while a session is detached (last-known table
  trusted for report origin during outages).
- hooks.selftest callable by any same-uid process (costs one trivial turn;
  same trust level as admin msg.send).
- agy uninterrupted 100-leg soak deferred on vendor quota flakiness.
- Backfill-versus-live dedupe is exact for one watched session and
  best-effort for several: events carry a ledger seq but not a session
  name. A line landing in the exact startup window can render twice; the
  record itself never duplicates. A server-side subscribe cursor would fix
  it properly.

## Risks

- The tmux-HEAD CI job is red on the F25 dead-pane test. It stays
  continue-on-error: it is early warning and it did its job.
- NOTICE still describes the v1 staged build, down to SHA-256 sums of
  bin/commPact files absent from this tree. It needs rewriting for the
  Rust implementation before this tree becomes the public tip.

## Open questions

- License: MIT carried from v1 (LICENSE), copyright shawn pana. Confirm
  that is the intended license and holder for the rewrite before release.

## Corrections

- **State color was removed on a misreading, and has been restored.** GOALS
  says "Exactly two encodings carry meaning: role color and state glyph.
  Never color alone: states pair glyph + word." That requires color to be
  REDUNDANT with the glyph and the word, not absent. M3's theme work read
  it as "states are never colored", deleted all six state.* and five
  badge.* tokens from the vocabulary, and this file signed that off as
  correct. Admin corrected it on 2026-08-02, referencing herdr.dev, which
  pairs a filled or empty circle with the state word and a color.
  The tokens are back in the vocabulary and the CLI paints all nine, in
  four semantic groups rather than per-state hues: healthy (working, delivered), needs-you
  (blocked_modal, blocked_permission, attention_required), terminal
  (blocked_quota, parked, the states that never retry themselves), and
  quiet (idle, queued, unknown). Role hues stay on the agent name alone so
  the two encodings never share a cell. The test is unchanged in spirit:
  with color off, nothing is lost, and that is measured rather than
  assumed. The CLI and the stream paint from the same tokens through the
  same code, so the two surfaces cannot drift. The wide no-entry emoji was
  retired at the same time: it was the only two-column glyph in the
  vocabulary and the only one that defaulted to color-emoji rendering, so
  it broke the column rhythm and could not be painted at all.

- **Anything in this file claiming state and badge tokens were deleted
  because nothing can paint them is wrong.** That claim was written here
  during M3 and the tree now ships all nine tokens with the CLI painting
  every one. The sentence above is the current rule.

## Deviations from the brief

- GOALS says "hollow check = unverified"; no portable hollow check glyph
  exists in terminal fonts, so it ships as heavy check (verified) vs light
  check (unverified), words unchanged. Flagged for admin; GOALS.md itself
  untouched.
- GOALS puts the eye in "the stream header, pane badges, and `status`".
  Pane-badge eyes are M4 pane work and are not built. The stream header
  and `cyclops status` both have a living eye; making the two run one
  shared rule rather than two implementations is the open work in the
  round-3 consolidation (see In flight). An earlier version of this entry
  claimed status could not have an eye because StatusResult carried no
  delivery data; that stopped being true when the round-2 fix added the
  open-delivery fold to StatusResult, and the entry was wrong until this
  correction.
- docs/GOALS.md:42 lists theme tokens including state.* and badge.*, and
  the tree now ships and paints them, so that line is accurate again after
  the correction below. It still names stream.* and surface.bg, which stay
  dropped because nothing paints a ground and the stream's gutter resolves
  surface.dim like every other detail column. GOALS.md is admin text and
  stays untouched; only those two names need an admin edit.
- ratatui and crossterm were unavailable offline, so the terminal layer is
  a hand-rolled termios and ANSI backend behind a pure frame builder.
  Swapping in a TUI crate later touches only that backend.
- The brief assigns `f` to both the firehose toggle and the from-filter.
  Tab is the sole view toggle; `f` opens the from input, because the
  filter set mirrors the history flags and is the more load-bearing
  contract.
