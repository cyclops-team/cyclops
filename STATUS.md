# Status

Updated 2026-08-03. M6 is complete and committed: the first run works now,
and the system is documented for someone who is not its author. A reviewer
working only from the docs could build it, explain how a message becomes a
verified receipt, write a manifest for a CLI that does not exist and watch a
pane bind to it, debug a stuck delivery, and list the invariants. 580 tests
across 63 targets, parity 93/93, CI green, v2 is main. Next is the installer,
then M7 flow features.
One thing still waits on admin: the v1 cutover.

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
- M4 naming, the roster, and pane border chrome (this change). What
  shipped:
  - `cyclops name <target> <label> [--manifest <id>] [--clear]` and
    `cyclops list`. Naming records a `pane_labeled` system line, paints
    the pane's tmux border, and recomputes the pane so an explicit
    `--manifest` binds at that moment rather than at the next unrelated
    event. `list` is the roster on the strict grid and it asks `status`
    for it, so there is one source of the roster and not two.
  - The adoption registry became a file, `$CYCLOPS_HOME/registry.json`.
    M1 kept it in memory, so every daemon restart silently unnamed every
    agent. It is restored per session against the live pane table, and an
    entry survives only when the pane exists AND its root pid matches the
    one recorded at adoption: a tmux server restart re-issues pane ids
    from %0, and without that check an old name lands on whatever
    inherited the id.
  - Border chrome, painted from cyclops-theme's tokens on the fused-state
    edge and no other. Scoped writes only: three per-pane options plus
    one per-window one, snapshotted at adoption and restored on `--clear`,
    on pane close, and at shutdown. Never the server-global scope, proven
    by asking tmux rather than the daemon (tests/m4_name.rs). `chrome =
    "off"` writes nothing at all.
  - MEASURED and recorded as F26 and F27: `pane-border-format` is a real
    pane option on 3.6a and `pane-border-status` is not; a format expands
    once, so an option's value can never become a directive, which is why
    the label rides `@cyclops_role` and the styling stays in the format;
    and the pane TITLE cannot be written by cyclops at all, because every
    shipped manifest reads it as a sensor.
  - Docs in the same change: docs/panes.md written from a real run,
    README, install, themes, ARCHITECTURE, CHANGELOG updated.
- M4 workspaces, layout presets and `cyclops start` (this change):
  - `cyclops start` opens the default workspace and is safe to run twice:
    it restores the saved workspace under that name, builds one from a
    preset when there is nothing saved, and leaves an existing session
    exactly as it is. Under the ready line it prints only the steps still
    undone, so a first run is the guided three-step moment GOALS asks for
    and a second run is one line. It writes `config.toml` when there is
    none and never edits one the user wrote; when the config does not
    name the session it prints the line to add.
  - The number in the ready line has one rule and it was worth finding by
    running the thing: a `--preset` build used to leave no workspace file,
    so the NEXT bare `start` fell back to `solo` and reported one agent
    over a two-agent session. Building from a preset now writes the
    workspace file, and the count comes from cyclopsd when it is watching
    (the roster `cyclops list` shows), from the workspace when there is no
    daemon and it still describes the session pane for pane, and otherwise
    is not claimed: a session split since gets no names touched and a line
    saying so. Regression tests cover both halves.
  - `cyclops workspace save|restore`: a session's shape as one TOML file
    under `$CYCLOPS_HOME/workspaces/`. Structure, ratios, working
    directories, the names from the registry, and a launch hint per pane.
    Restore always builds a NEW session, so it can never rearrange one
    somebody is working in, and it restores structure and not processes:
    panes are empty unless `--launch` is given, and even then tmux runs
    the command as the pane's own. Nothing on this path sends keys to a
    pane or sets a tmux option.
  - Four presets in `layouts/`, compiled into the binary so a fresh
    install has them before it has a config file. They are a ladder: each
    is the one before it plus a pane, and the names carry over. The `ops`
    dock's full width and 30% height are argued from the stream's own
    grid, in the preset file and in docs/workspaces.md.
  - `cyclops_tmux::layout` holds the tree, `capture` and `apply`. The
    one-shot tmux runner `focus_pane` had is now shared in `cmd.rs`. The
    model refuses what it cannot describe honestly, a nested split or a
    zoomed pane, rather than saving an approximation that would restore
    into a different arrangement.
  - F28 came out of this: tmux spreads a window resize EVENLY, not in
    proportion, so a workspace built at tmux's detached 80x24 default and
    then attached from a 200x50 terminal turns a 30% dock into 41%.
    `start` and `restore` build at the size of the terminal they were run
    from. One built in a script and attached elsewhere still drifts, and
    docs/workspaces.md says so rather than hiding it.
  - Three defects came out of writing demos/m4-workspace.sh rather than
    out of reading the code, which is the M3 lesson holding: a preset
    build left no workspace file (so the next run reported the wrong agent
    count); the grid was measured against the WINDOW, so the pane chrome's
    `pane-border-status top` (F27, one line per pane) made
    `cyclops workspace save` refuse a session `cyclops start` had just
    built; and naming a session the daemon had not attached to yet failed
    per pane with a confusing error and then a second, wrong sentence over
    the top of it. All three are fixed with regression tests: the file is
    written at build, the arithmetic measures panes, and the four daemon
    states each get their own line plus the exact `cyclops start` that
    finishes the job.
  - demos/m4-workspace.sh is the whole loop end to end: build, name, save,
    kill the session, restore, name again. Isolated tmux server and a
    throwaway home like every other demo.
  - Docs in the same change: docs/workspaces.md written from a real run,
    README, install, ARCHITECTURE, CHANGELOG updated, F28 in findings.md.

- M4 pane UX (this commit): `cyclops name` / `cyclops list` with a durable
  adoption registry, pane border chrome carrying role and state, four
  declarative layout presets, `cyclops workspace save|restore`, and
  `cyclops start`. Its review found three defects that all shipped as
  working code and all of which would have hurt, so they are recorded
  rather than smoothed over:
  - `cyclops start` matched a saved workspace to a live session by pane
    COUNT, so a rearranged session got its agents renamed onto the wrong
    panes. Proven: a tiled ops layout renamed the tests pane to reviewer
    and the stream dock to tests, under a line reading "workspace ready ·
    3 agents". A name is what every later send resolves through, so that
    is the GOALS cardinal rule broken silently. Now matched on grid
    topology position for position PLUS the names the daemon already
    holds; any mismatch renames nothing at all and names the difference.
    Deliberately not compared: pane sizes (resizing moves no agent) and
    window names (tmux automatic-rename would refuse constantly).
  - `cyclops name --clear` deleted the registry entry holding the user's
    own pane-border-format before restoring it, so a failed restore lost
    it permanently while printing success, and a later rename re-recorded
    cyclops's own border as if it were the user's. Now: peek, restore,
    and only commit the removal once the restore succeeded.
  - `cyclops workspace save` overwrote every label in an existing file
    when it could not read the roster, printing the opposite. Both causes
    are fixed: no daemon, and a daemon with an empty roster.
  - The chrome on/off switch was tested in eight places in five spellings
    and in none of chrome.rs, with two of the eight already disagreeing.
    It now lives in chrome.rs alone.

- M5 themes (this change). The three-theme set, the verb that switches
  between them, and the reload rule that keeps a half-written file off the
  screen. The rest of M5 (landing-page command parity, the README
  quickstart pass) is not in this change.
  - themes/dark, light and high-contrast each set the whole 22-token
    vocabulary with explicit 256-color fallbacks, and each header now
    states its contrast as numbers: the ground it assumes, the floor every
    token clears, and how it was measured. high-contrast was retuned to
    clear 7:1 (WCAG AAA for body text) on every token; `state.dead` moved
    from #949494 (6.9:1) to #9e9e9e (7.8:1) to get there. The floor is the
    red at 7.05:1, and the file says why a red on black is the color that
    sets it. `shipped_themes_meet_their_stated_contrast` fails the build
    if a retune drops below what a header claims.
  - `cyclops theme` lists what is there with a one-line swatch per theme,
    painted in that theme, `▸` on the active one. `cyclops theme <name>`
    edits the one config line and leaves the rest of the file, comments
    included, exactly as written. It refuses a name that will not load
    rather than writing a key that renders built-in colors and says so
    only at the next command.
  - Hot reload got two changes, and the second is the one that matters.
    `ThemeWatch` now watches the SELECTION, config key plus theme file, so
    a switch moves a running `cyclops ui` and the daemon's pane borders,
    not just an edit to the file they were already on. And a reload now
    applies whole or not at all: the file has to load AND still set every
    token it set before, or the colors on screen stay and one line says
    why. That rule exists for how editors save. Truncate, then rewrite: a
    stat landing mid-save reads a SHORTER file that is still valid TOML,
    and loading it paints every lost token out of the compiled default
    table, whose lightness has nothing to do with the theme on screen. A
    misspelled token name does the same thing and stays. Switching is
    deliberately exempt: a palette the user just asked for applies with a
    fresh start's tolerance.
  - `theme.reload` on the daemon (no params, it reads the config itself)
    repaints every adopted pane's border and emits `theme` so subscribers
    wake. The event carries the name and no colors: every surface resolves
    its own, and one that took a palette off the wire could show a theme
    no file on the machine holds. Proven against tmux, not against the
    daemon's own belief: crates/cyclopsd/tests/m5_theme.rs reads the
    border format back off the server.
  - Warnings are drained rather than read (`take_warnings`), which is what
    makes "one warning line" true: the daemon used to log the same theme
    warning on every repaint after a bad edit.
  - One defect came out of writing demos/m5-theme.sh and not out of
    reading the code, which is the M3 lesson holding again. The reload
    exempted a config change from the token rule, and `cyclops theme
    <name>` rewrites the config key every time, so running it while a file
    was mid-save turned the exemption on and put the compiled default
    table onto a real pane border. The question the check asks is now
    whether the selected FILE changed, not whether the config did.
    Regression test in select.rs, and the demo is the thing that would
    catch it again.
  - F32 in findings.md is the measurement behind all of it: rewriting a
    theme file the way every editor does, 27.3% of concurrent reads saw
    valid TOML defining ZERO of the 22 tokens, and 0% saw a syntax error.
    A syntax error was the only thing the loader used to treat as a
    failure, so the failure it guarded against was the one that never
    happens.

- M6 handoff (this commit). Two halves, one goal: a person who is not the
  author can install this, use it, and then maintain it.
  - The first run was broken and the admin hit it. `cyclops start` reported
    success, cyclopsd logged "no manifest directory found" where nobody
    sees it, `status` said "? unknown" without saying why, and the message
    died 24s later as "needs attention · no manifest". The manifests only
    existed in a clone, so an installed binary had none. They are now
    compiled into the CLI and seeded into the cyclops home on every start,
    never clobbering a file already there, and the daemon falls back to
    that directory. Nothing reports success while the thing it set up
    cannot work: start, status and name each say so, and the daemon's
    warning reaches the record instead of only a log.
  - A real defect underneath it, found by measurement: at shipped defaults
    no unhooked agent could ever land a badge on a receipt. ack_timeout_ms
    (1500ms) was the only armed timer during the tier-1 window, so nothing
    looked at the pane at all, and the ladder resumed at submit+3000ms
    while receipt_block_ms closed the window at 2500ms. Tier 2 now opens
    with an immediate evidence pass. Note for the record: the orchestrator
    measured this case with a fixture manifest declaring no ack hook, got
    a healthy 0.43s delivery, and wrongly told an agent the finding was a
    misdiagnosis. Both shipped CLI manifests declare an ack, so the
    defective path was the default one.
  - A send to a recipient nothing detects no longer reports a success
    shape. It says what happened, names the pin command, and exits 1, so a
    script branching on exit 0 cannot read it as delivered.
  - docs/HANDOFF.md, docs/INVARIANTS.md and docs/CONTRIBUTING.md are new
    and reachable from README; findings.md gained an index. ARCHITECTURE
    opened by pointing newcomers at two files that do not exist in this
    repo, which is fixed, and the checker that caught it is adopted as a
    task rather than left in a scratch directory.
  - The parity gate gained a shipped-defaults leg with no hooks and no
    tuning, plus a guard asserting that leg's config stays untuned. Every
    defect above lived behind a gate that only ever tested the
    configured-perfectly path. 93/93.

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

- M5 polish parity. Running. The theme set, `cyclops theme` and hardened
  hot reload are in (see Done). Still open: landing-page command parity,
  docs examples elsewhere in docs/, README quickstart on the progressive
  ladder.
- M6 handoff, and this is now the milestone that matters most. The bar is
  a person, not a checklist: a competent engineer who has never seen this
  repo should be able to build and run it, explain how a message becomes a
  verified receipt, add a new agent CLI, debug a stuck delivery, and know
  which invariants they must not break and why, without asking anyone.
  docs/STYLE.md is the standard, so this means fewer and better placed
  words, not more, with diagrams where prose is worse.
- M7 flow features: `cyclops pipe`, attention routing, `--wait`
  composition ergonomics. Moved behind the handoff milestone so its docs
  are written as it ships rather than retrofitted.

Dropped by admin decision on 2026-08-03:

- The narrator (a cheap agent posting periodic digests into the admin
  stream). Beyond being unwanted, it fought the calm-stream rule: a digest
  on a timer needs nobody, so opening the eye for it is the exact failure
  that cost M3 six rounds.
- The dogfood experiment (running a Cyclops workspace to build Cyclops).

## Backlog (non-blocking)

- M4 floor, documented rather than fixed: if cyclopsd is watching a
  session but holds NO names for it, a reorder is undetectable and
  `cyclops start` maps the workspace's names onto panes by position. Grid
  topology alone cannot tell two same-shaped arrangements apart. This is
  the model's floor, not a regression; naming one pane closes it.
- `cyclops workspace save` prints an agent count taken from the file
  rather than the live roster; only the second line discloses that.

- F25 is FIXED (see findings.md). A per-pane subscription can never
  report a pane's death, because the closed pty fd that sets pane_dead is
  the same gate that makes tmux skip that pane's subscription. Cyclops now
  arms an all-panes subscription, which has no such gate. The tmux-HEAD CI
  job is green again on both 3.6a and next-3.8.
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

- The tmux-HEAD CI job is green again now that F25 is fixed. It stays
  continue-on-error so a break in tmux master cannot block this repo, but
  that also means it can rot unnoticed: check it when it goes red rather
  than assuming it is the usual failure. It earned its keep once already.
- A dead pane's pane_pid differs by tmux version (stale on 3.6a, -1 on
  next-3.8) and is deliberately not normalized. On 3.6a a stale pid can be
  recycled, and sender identity walks socket-peer ancestry to a pid.
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

- **`cyclops start` prints `✔ workspace ready · 3 agents`, and the brief
  and the landing page both write it `✓ workspace ready — 3 agents`.** Two
  characters, and both are the product's existing vocabulary rather than
  taste. The light check already means one specific thing here, "delivered
  but unverified", and the site's other check line
  (`✓ delivered · verified`, HowItWorks.svelte:32) is already translated
  to the heavy check in the shipped CLI: this line gets the same
  translation. The separator is the middle dot every badge, header and
  receipt in the tree uses, and the tree carries no em dash anywhere. The
  site is the branding reference and stays untouched; if admin wants the
  site's characters exactly, it is one line in
  `crates/cyclops/src/workspace.rs` (`ready_line`) and its test.
- The brief's `cyclops start` "ensure the daemon watches it" is done
  through the config file, not by making the daemon watch a session at
  runtime. `Inner.sessions` is fixed at boot and indexed everywhere by
  position, with one ledger per slot; adding sessions live is a daemon
  change that touches M1 and M2 invariants and is not part of a workspace
  verb. `start` writes the config on a first run, prints the exact line to
  add when the file is the user's own, and reports whether cyclopsd is
  down, running elsewhere, or watching.
- The workspace tree is a grid of rows, not tmux's binary split tree. It
  says every preset and everything `select-layout` produces, it reads as
  data a human can edit, and what it cannot say it refuses by name. A
  window nested deeper than a grid can be saved only after
  `select-layout`; the error says exactly that.
- Saved launch commands carry no arguments. `#{pane_current_command}` is a
  process name, so a pane running `claude --resume` records `claude`. The
  field is documented as a hint for that reason. Recording full argv means
  reading `/proc` or `ps` for the pane's foreground child, which is a
  second identity mechanism and belongs with the argv work F21 already
  started, not here.
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
- The M4 brief asks the daemon to set the pane TITLE as well as the
  border format. Only the border is written. The title is the title tier
  of fusion on every shipped manifest (claude's spinner rules sit at
  priority 1100, and a matching title rule means the screen sensor never
  runs), a write from outside pushes a subscription change like any other
  (F13), and an agent that publishes its own title overwrites cyclops
  inside tmux's 1Hz tick (F23). The border already displays the title, so
  replacing the border format gives the same reading surface without
  replacing the evidence underneath. Measured and written up as F26.
- `pane-border-status` is written at WINDOW scope, which is wider than
  the per-pane rule the brief states. tmux offers no pane scope for it:
  `set -p` on that option writes the window option anyway (F27,
  MEASURED). It is snapshotted per window, turned on by the first
  adoption in a window and put back by the last un-adoption, and the
  server-global scope is never touched.
- GOALS puts the eye on pane badges. The border carries `role • state`
  and not the eye; the eye stays on the stream header and `status`. The
  M3 deviation entry above is still accurate.
- ratatui and crossterm were unavailable offline, so the terminal layer is
  a hand-rolled termios and ANSI backend behind a pure frame builder.
  Swapping in a TUI crate later touches only that backend.
- The brief assigns `f` to both the firehose toggle and the from-filter.
  Tab is the sole view toggle; `f` opens the from input, because the
  filter set mirrors the history flags and is the more load-bearing
  contract.
