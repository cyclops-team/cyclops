# Cyclops Terminal Workspace UI — Implementation Plan

Companion to `../design/detailed-design.md`. Each step is a working,
demoable increment; tests are written with (or before) the code they cover,
and every step ends wired into the whole — no orphaned code. All work runs
under the repo's standing gates: `cargo fmt`, `clippy -D warnings`,
`cargo test --workspace --no-fail-fast`, `scripts/check-doc-paths.py`,
`demos/parity-check.sh`.

**Demo recordings.** For user-visible interactive steps, record a short
demo video after automated tests pass. Videos are review artifacts, not
substitutes for unit, frame, integration, or testrig-backed tests. Cloud
agents record this the direct way: open a terminal on the agent's desktop,
run the demo against a throwaway tmux server, and let the screen recording
attach with the step's deliverables. A scripted `vhs` tape or `asciinema`
cast in a PTY is an acceptable alternative and has the advantage of being
re-renderable after later changes.

## Checklist

- [x] Step 1: VT engine evaluation — `PaneVt` trait + fidelity fixture corpus
- [ ] Step 2: Streaming control client in `cyclops-tmux`
- [ ] Step 3: Hydration bundles, client sizing, and flow control
- [ ] Step 4: Minimal workspace — one live pane, keyboard pass-through, bare `cyclops`
- [ ] Step 5: Tabs and layout — split-tree rendering and the prefix keyboard router
- [ ] Step 6: Structural intents and reconciliation
- [ ] Step 7: Sidebar and workspaces
- [ ] Step 8: Resilience — reconnect, flow-control recovery, server-gone
- [ ] Step 9: Mouse foundation — click, wheel, menus, split controls
- [ ] Step 10: Selection and clipboard
- [ ] Step 11: Drag — dividers, tabs, panes
- [ ] Step 12: Agent decoration — states, attention, labels, event panel
- [ ] Step 13: Persistence — preferences and last-active state
- [ ] Step 14: Command surface and docs — `watch` rename, parity, front doors

---

## Step 1: VT engine evaluation — `PaneVt` trait + fidelity fixture corpus

**Objective.** Create the `crates/cyclops-workspace` crate (library +
entry function, per the repo's library-first pattern) containing only the
`PaneVt` trait, implementations for both candidate engines
(`alacritty_terminal`, `libghostty-vt`), and the fixture corpus that
decides between them.

**Guidance.** Fixtures are recorded byte streams with expected cell grids:
plain output, SGR/256/truecolor, attributes, cursor motion, wrapping, wide
characters, alternate screen entry/exit, bracketed paste, and captures from
real agent CLIs (Claude Code, Codex). No tmux, no rendering — pure trait
tests. Keep the crate out of the CLI for now; it compiles and tests in the
workspace but nothing dispatches into it yet.

**Tests.** The corpus itself is the test suite: every fixture asserts the
grid produced through `PaneVt` for each engine. A comparison summary makes
the engine decision reviewable.

**Integration.** New crate joins the Cargo workspace and CI gates. The
engine decision and every measured behavior gap land in `findings.md` with
their probes (F-numbers). If one engine wins with no prospect of a second,
delete the losing implementor and (per the design) collapse the trait.

**Demo.** `cargo test -p cyclops-workspace` runs the corpus; the summary
shows both engines' scores and the recorded decision.

**Acceptance criteria.**

- `crates/cyclops-workspace` joins the Cargo workspace and passes fmt,
  clippy `-D warnings`, and the workspace test run.
- The corpus covers every listed category (plain, SGR/256/truecolor,
  attributes, cursor motion, wrapping, wide characters, alternate screen,
  bracketed paste) plus at least one recorded capture each from Claude
  Code and Codex.
- Every fixture asserts its expected cell grid through `PaneVt` against
  both engines; the suite is pure (no tmux, no testrig).
- The run produces a per-engine comparison summary that makes the choice
  reviewable, and the decision plus each measured behavior gap lands in
  `findings.md` with its probe.
- Nothing in the CLI dispatches into the crate yet.
- If one engine wins with no prospect of a second, the losing implementor
  is deleted and the trait collapsed in the same change.

## Step 2: Streaming control client in `cyclops-tmux`

**Objective.** Grow the adapter's control-mode support from single
commands to a long-lived streaming client: attach, decode
`%output`/`%extended-output` (octal-escaped bytes) with per-pane
subscription, fan out structural notifications (`%window-add`,
`%unlinked-window-close`, `%layout-change`, `%session-changed`, ...), and
shut down cleanly.

**Guidance.** All tmux invocation stays in `cyclops-tmux` (the existing
guard test already enforces this). The client exposes a typed event stream;
no polling — the reader blocks on the control-mode pipe.

**Tests.** `TmuxServer`-backed: attach to a rig server (`socket()` supplies
the `-L` name), write into a pane via `cmd()`, assert decoded bytes and
notification events arrive in order; kill a window from a second client and
assert the structural notification. Escaping round-trip tests are pure.

**Integration.** Extends the existing `ControlClient` module; the
`SessionWatcher` and command paths are untouched. Nothing consumes the
stream yet except tests.

**Demo.** A rig-backed test (or `cargo test -- --nocapture` run) that
attaches, echoes into a pane, and prints the decoded event stream live.

**Acceptance criteria.**

- The streaming client attaches to a `TmuxServer` rig via `socket()`,
  gated by `tmux_available()`, and detaches cleanly (rig `Drop` teardown
  leaves nothing behind).
- Bytes written into a pane via `cmd()` arrive as decoded
  `%output`/`%extended-output` events for the subscribed pane; octal
  escaping round-trips under pure tests.
- Structural notifications (`%window-add`, `%unlinked-window-close`,
  `%layout-change`, `%session-changed`) surface as typed events in the
  order tmux emitted them; a kill from a second `cmd()` client produces
  the expected notification.
- The reader blocks on the control-mode pipe — no timer or poll anywhere
  in the client.
- The existing `cyclops-tmux` suite and the tmux-invocation guard still
  pass unchanged.

## Step 3: Hydration bundles, client sizing, and flow control

**Objective.** Complete the adapter surface the pane pipeline needs:
hydration bundles (escaped visible + alternate-screen capture plus
cursor/mode metadata in one call), client sizing (`refresh-client -C WxH`,
`window-size latest` setup), and flow control (`pause-after`,
`refresh-client -A`, `%pause`/`%continue` handling).

**Guidance.** This is where the design's honesty about hydration is
enforced: a capture initializes the grid and metadata initializes cursor
and alternate-screen state, but the bundle never claims parser-exact state.
Wire the bundle into `PaneRuntime::hydrate` (Step 1's trait) so the two
halves meet.

**Tests.** Hydration correctness on a `TmuxServer`: run a deterministic
ANSI fixture in a pane (`run_ok` + `wait_screen`), hydrate mid-stream, then
assert the runtime grid matches `capture()` — including after resize,
pause/continue, and pane respawn. Flow-control tests flood a pane and
assert `%pause` triggers rehydration rather than trusting byte continuity.

**Integration.** Joins Step 1's runtimes to Step 2's stream: bytes decoded
by the control client now feed hydrated `PaneRuntime`s in tests.

**Demo.** A rig-backed test that hydrates a pane mid-output and proves the
emulator's grid is byte-for-byte what `capture-pane` reports.

**Acceptance criteria.**

- One adapter call returns the full hydration bundle: escaped visible
  capture, alternate-screen capture, and cursor/mode metadata.
- `PaneRuntime::hydrate` consumes the bundle, and a runtime hydrated
  mid-stream (fixture via `run_ok` + `wait_screen`) matches the rig's
  `capture()` exactly.
- The grid-matches-capture assertion also holds after resize, after
  `%pause`/`%continue`, and after pane respawn.
- Flooding a pane triggers `%pause` under the configured `pause-after`,
  and recovery rehydrates instead of trusting resumed byte continuity.
- The adapter applies `refresh-client -C WxH` and `window-size latest`,
  verified against the rig server.
- No code path claims parser-exact state from a capture; rehydration is
  the codified recovery on pause and reconnect.

## Step 4: Minimal workspace — one live pane, keyboard pass-through, bare `cyclops`

**Objective.** First runnable UI: `cyclops` (TTY-gated) opens a full-screen
Ratatui/Crossterm app that attaches to the tmux server, renders the active
session's active pane live, and forwards typing via `send-keys`. Prefix +
`d` detaches.

**Guidance.** Build the skeleton that everything else hangs off: the
terminal guard (raw mode, alternate screen, mouse off for now; `Drop` and
panic hook restore unconditionally), the single event loop merging
crossterm input and adapter events, the event-armed render debounce
(`RECONCILE_DEBOUNCE` shape — no frame timer), the `theme` adapter over
`cyclops-theme` tokens, and the `copy` module for user-facing strings.
Bare `cyclops` with non-TTY stdout prints help and exits 0. If no tmux
server is running, print the offer-to-start message (full flow in Step 8).

**Tests.** Semantic frame test through Ratatui's `TestBackend` (pane cells
painted from a runtime grid); pass-through encoder tests (arrow keys,
modifiers → `send-keys` arguments); TTY-gate test (non-TTY stdout →
help); terminal-guard restore-on-panic test; the new guard test lands here:
no interval timers in `cyclops-workspace` (zero-polling rule), alongside
extending the raw-color and tmux-invocation guards to the new crate.

**Integration.** The CLI's bare invocation dispatches into
`cyclops-workspace` the way `watch` dispatches into the stream UI. Steps
1–3's runtime and control client become the live data path.

**Demo.** Run `cyclops` in a terminal with an existing tmux session: the
active pane appears live, typing reaches the shell, vim/htop render
correctly in the alternate screen, prefix-`d` detaches and the terminal is
restored.

**Acceptance criteria.**

- Bare `cyclops` on a TTY renders the active pane live and forwards
  typing via `send-keys`; prefix-`d` detaches without killing tmux or the
  daemon.
- Bare `cyclops` with non-TTY stdout prints help and exits 0 (tested).
- The terminal guard restores raw mode, alternate screen, and mouse state
  on every exit path, including a forced panic in a test.
- A Ratatui `TestBackend` frame test shows pane cells painted from a
  runtime grid; pass-through encoder tests cover arrows and modifier
  combinations.
- Rendering is armed only by events through the single debounce — the new
  guard test forbidding interval timers in `cyclops-workspace` passes,
  and the raw-color and tmux-invocation guards now cover the crate.
- All user-facing strings live in the `copy` module; all styles come from
  `cyclops-theme` tokens through the `theme` adapter.

## Step 5: Tabs and layout — split-tree rendering and the prefix keyboard router

**Objective.** Render whole windows, not one pane: parse tmux's layout
string into `LayoutNode`, render every pane of the active window with
borders and the focused-pane highlight, add the tab bar, and complete the
prefix keyboard router (next/previous tab, tab 1–9, new tab, pane focus
movement, rebindable bindings from config).

**Guidance.** Only visible-tab panes hold live emulators; a tab switch
hydrates its panes fresh (the accepted v1 trade-off). The keyboard router
is a small state machine: prefix arms, next key resolves, everything
unreserved passes through. Bindings load from `config.toml` with
prefix-first defaults.

**Tests.** Layout-string parsing against real tmux layout strings
(including nested splits); `TestBackend` frames for multi-pane borders,
focused highlight, and tab bar; router state-machine tests (prefix arming,
resolution, pass-through of unreserved keys, rebinding to direct chords);
rig-backed test that a tab switch rehydrates panes to match `capture()`.

**Integration.** `%layout-change` and window notifications from Step 2 now
drive the model; the renderer composes multiple Step 3 runtimes.

**Demo.** A session with named windows and nested splits renders fully;
prefix-number jumps tabs, prefix-arrows move pane focus, both panes of a
split are live simultaneously.

**Acceptance criteria.**

- Real tmux layout strings, including nested splits, parse to the correct
  `LayoutNode` tree (pure tests).
- `TestBackend` frames show multi-pane borders, the focused-pane
  highlight, and the tab bar with the active tab marked.
- Router tests cover prefix arming and resolution (next/previous tab, tab
  1–9, new tab, pane focus movement), pass-through of every unreserved
  key, and rebinding a default to a direct chord via `config.toml`.
- Only panes of the visible tab hold live emulators; a rig-backed tab
  switch rehydrates and the grids match `capture()`.
- `%layout-change` and window notifications drive all model updates — no
  refresh command issued on a timer.

## Step 6: Structural intents and reconciliation

**Objective.** Mutating operations, keyboard-driven first: split right/
down, close pane (with live-agent confirmation), zoom, new/rename/close
tab, with the reconciliation rule enforced — the model updates only from
tmux replies and notifications, never from an optimistic preview.

**Guidance.** Implement the `intent` module against the design's UI-action
→ tmux-operation map. Splits inherit `pane_current_path`. Confirmations
route through the daemon's state answer when available (full decoration in
Step 12); without the daemon, closing is a plain terminal close.

**Tests.** Rig-backed per action: issue the intent, assert the mapped tmux
operation happened and the model converged to tmux's actual geometry.
Concurrency: a second `cmd()` client mutates the same window mid-flight and
the model still converges — this same shape exercises the `window-size
latest` policy (R5). Confirmation-dialog frames via `TestBackend`.

**Integration.** Intents flow through Step 2's client; reconciliation
consumes the notification fan-out; the router from Step 5 gains the new
bindings.

**Demo.** Build a working multi-pane layout entirely from inside the
workspace — split, zoom, close, new tab, rename — then `tmux attach` from
another terminal and see the identical structure.

**Acceptance criteria.**

- Each action (split right/down, close pane, zoom, new/rename/close tab)
  has a rig-backed test asserting the mapped tmux operation ran and the
  model converged to tmux's actual geometry.
- The model mutates only from tmux replies and notifications; no
  optimistic preview is ever committed.
- Splits open in the source pane's `pane_current_path` (asserted via
  tmux formats on the rig).
- Concurrent mutations from a second `cmd()` client converge, exercising
  the `window-size latest` policy (R5).
- Closing a pane with a live agent shows the confirmation dialog
  (`TestBackend` frame); without the daemon it closes plainly.
- After a scripted mutation sequence, structure seen via a plain second
  client matches the workspace's model.

## Step 7: Sidebar and workspaces

**Objective.** The project sidebar: every session on the server listed,
workspace switching, create-workspace-from-folder (session named after the
folder, default directory set there), rename, and close with confirmation.

**Guidance.** Workspace switch retargets the control client
(`switch-client`) and hydrates the new active window. The folder picker is
a simple path prompt with completion — not a file-manager. Sidebar
collapse/expand is a render state (persisted in Step 13). Attention badges
and agent rows arrive in Step 12; for now rows show name and tab count.

**Tests.** Rig-backed: sessions created outside Cyclops appear in the
model; create-workspace produces a session with the right name and
default directory (assert via tmux formats); switching converges. Frame
tests for sidebar rows, selection, and the picker dialog. Session
add/remove notifications drive list updates (no polling).

**Integration.** `%session-changed` and session notifications from Step 2;
create/rename/kill go through Step 6's intent path.

**Demo.** Two project folders → two workspaces via the picker; switch
between them with sidebar and prefix bindings; a session created by hand
with `tmux new -s` appears in the sidebar immediately.

**Acceptance criteria.**

- Every session on the rig server appears in the sidebar model, including
  one created outside Cyclops via `cmd()`.
- Create-workspace produces a session named after the chosen folder with
  its default directory set there (asserted via tmux formats), and new
  tabs in it open in the project directory.
- Workspace switch retargets the control client and hydrates the new
  active window; the rig test converges.
- Rename and close go through the Step 6 intent path; close shows the
  confirmation dialog.
- Session add/remove list updates are driven by notifications only — no
  session-list polling.
- `TestBackend` frames cover sidebar rows, the selected row, and the
  folder-picker dialog.

## Step 8: Resilience — reconnect, flow-control recovery, server-gone

**Objective.** The failure behaviors from the design's Error Handling
section: control-mode reconnect as a bounded one-shot timer chain (each
attempt arms the next, capped — never a free-running interval), re-list and
rehydrate on reconnect, last-grid-dimmed "reconnecting" pane rendering, and
the tmux-server-gone flow that offers to start a fresh server/session.

**Guidance.** The reconnect timers are named and listed in one module
header, the way `crates/cyclopsd/src/delivery.rs` lists its own. Wire
`%pause`/`%continue` handling from Step 3 into the app: a paused pane shows
a quiet border note and rehydrates on continue.

**Tests.** Rig-backed: kill the control client's tmux process (not the
rig's server) and assert reconnect + rehydration converge; kill the rig
server and assert the offer flow. Frame tests for dimmed/reconnecting and
paused panes. Timer-shape assertions: the chain stops at its cap.

**Integration.** Hardens Steps 2–7 against the disconnect cases; no new
surface area.

**Demo.** Kill the tmux client connection under a running workspace: panes
dim with a reconnecting note, then snap back live, with layout and content
intact.

**Acceptance criteria.**

- Reconnection is a chain of bounded one-shot timers with a cap — each
  attempt arms the next, and a test proves the chain stops at the cap.
- All resilience timers are named and listed in one module header, the
  `delivery.rs` shape.
- Killing the control client's tmux process (not the rig server) leads to
  reconnect, structure re-list, and rehydration; the rig test converges
  to `capture()`.
- During disconnect, panes render their last grid dimmed with the
  reconnecting border note (`TestBackend` frame) — never blank.
- `%pause` shows a quiet border note and `%continue` rehydrates the pane.
- With the rig server killed, the workspace presents the offer-to-start
  flow instead of crashing.

## Step 9: Mouse foundation — click, wheel, menus, split controls

**Objective.** SGR mouse enablement, render-time hit regions, and the
non-drag mouse story: click-to-focus (panes, tabs, sidebar rows), wheel
scrollback in the hovered pane, right-click pane context menu, the
application menu (bottom-left), and the two split-control buttons in the
hovered/selected pane's corner.

**Guidance.** Hit regions are recorded during render, hit-tested on event —
one mouse router with explicit target variants. Application menu and
context menu are mutually exclusive in app state. Every mouse action
already has its keyboard equivalent from Steps 5–6; menus invoke the same
intents. Scrollback scrolls the VT runtime's history (`scroll_offset`,
0 = live tail).

**Tests.** Mouse-router tests over recorded hit regions (click resolution,
menu open/dismiss rules, mutual exclusivity); frame tests for menus and
split controls; runtime scrollback tests (wheel deltas move the viewport,
new output while scrolled doesn't yank to tail unless at tail); rig-backed
click-to-focus convergence.

**Integration.** The terminal guard from Step 4 now also owns mouse
reporting on/off; menus dispatch through Step 6 intents.

**Demo.** Operate the whole workspace mouse-only: click panes and tabs,
wheel through history, right-click → split from the context menu, open the
application menu, split via the corner buttons.

**Acceptance criteria.**

- Mouse reporting is enabled and disabled by the terminal guard; restore
  covers it on all exit paths.
- Hit regions are recorded at render time and the router resolves clicks
  against them (unit tests over recorded regions).
- Click-to-focus on panes, tabs, and sidebar rows converges in rig-backed
  tests.
- Wheel scrolls the hovered pane's history; new output does not yank a
  scrolled viewport unless it is at the live tail (runtime tests).
- Context menu and application menu are mutually exclusive with tested
  open/dismiss rules; both dispatch through the Step 6 intents.
- Split-control buttons render only on the hovered/selected pane and
  trigger the same intents as their keyboard equivalents.
- Every mouse action in this step has a working keyboard equivalent.

## Step 10: Selection and clipboard

**Objective.** Click-drag text selection inside a pane with visual
highlight, copy to the system clipboard on release, and word/line selection
on double/triple click.

**Guidance.** Selection lives in the pane runtime (`PaneVt::select`
extracts text, including scrollback); rendering overlays the highlight.
Clipboard through OSC 52 with a native-clipboard fallback (arboard or
equivalent) — measure which terminals honor OSC 52 and record it in
`findings.md`. Selection is memory-only and is never logged or persisted
(Invariant 7); the clipboard write is the one deliberate export.

**Tests.** Selection-extraction tests through the trait (ranges across
wrapped lines, wide characters, scrollback); frame tests for the highlight;
router tests distinguishing selection drag from the Step 11 drags (drag
starting inside a pane body selects; borders and chrome do not).

**Integration.** Extends the Step 9 mouse router with the pane-body drag
target; completes R9.

**Demo.** Drag across output in a pane, watch the highlight, paste the
copied text into another application.

**Acceptance criteria.**

- Click-drag inside a pane body produces a visible selection highlight
  (`TestBackend` frame); double and triple click select word and line.
- `PaneVt::select` extraction is correct across wrapped lines, wide
  characters, and scrollback (trait-level tests, no tmux needed).
- Releasing the drag copies the selection to the system clipboard via
  OSC 52 with the native fallback; per-terminal OSC 52 support is
  measured and recorded in `findings.md`.
- The router distinguishes a pane-body selection drag from chrome drags:
  borders and chrome never start a selection.
- Selection text is never logged or persisted; the clipboard write is the
  only export (Invariant 7 holds).

## Step 11: Drag — dividers, tabs, panes

**Objective.** The remaining full-mouse story: drag dividers to resize
splits, drag tabs to reorder (and onto a sidebar workspace row to move
them), drag panes between split positions with drop-zone previews — every
drop translated to the mapped tmux operations and then reconciled.

**Guidance.** Drag state machine per `DragTarget` variant: down → threshold
→ move (preview) → up (commit) or Escape (cancel). Previews are chrome-only
hints; tmux's resolution of the drop is the truth (a preview may legally
resolve to slightly different geometry — the render follows tmux). Divider
drags coalesce into `resize-pane` steps; pane drops become
`swap-pane`/`join-pane`/`move-pane` sequences per the action map.

**Tests.** Drag state-machine unit tests (full life cycle per target,
Escape cancellation, threshold behavior); rig-backed drop tests: perform
each drop kind, assert the resulting tmux layout, assert the model
converged even when tmux resolves differently than the preview; divider
resize convergence under `window-size latest` with a second attached
client.

**Integration.** Completes the Step 9 mouse router; reuses Step 6
reconciliation unchanged.

**Demo.** Rearrange a workspace entirely by mouse: reorder tabs, drag a
pane from one split slot to another, resize with the divider, drag a tab
onto another workspace in the sidebar.

**Acceptance criteria.**

- Every `DragTarget` variant has unit-tested life-cycle coverage:
  down → threshold → move → up commit, plus Escape cancellation; no drag
  starts below the threshold.
- Divider drags emit coalesced `resize-pane` steps and converge under
  `window-size latest` with a second `cmd()` client attached (rig test).
- Tab reorder, tab-to-workspace move, and each pane-drop kind produce the
  mapped tmux operations and the model reconciles to the resulting layout
  (rig-backed per drop kind).
- When tmux resolves a drop differently from the preview, the model
  follows tmux — verified with a case where the geometries differ.
- Previews are chrome-only hints; nothing from a preview is ever written
  to the model.

## Step 12: Agent decoration — states, attention, labels, event panel

**Objective.** The Cyclops layer: subscribe to the daemon for agent states,
attention, and label changes; render state badges (glyph AND word, color
third) on sidebar rows, tabs, and pane borders with pane → tab → workspace
rollup; clicking an indicator jumps to the pane; pane rename goes to daemon
`pane.label`; the slide-out event panel embeds the daemon's event stream;
daemon-offline degrades quietly.

**Guidance.** Attention is consumed, never recomputed — the rollup is
presence-aggregation of the daemon's per-pane answer, not a reimplementation
of the attention rule. Sidebar naming priority: user label → detected agent
name → neutral `agent`; unnamed plain shells stay out of the expanded list.
New theme tokens land in `cyclops-theme` and its painted-by-a-renderer
test. The event panel reuses the same daemon subscription `cyclops watch`
uses. The pane title is never written; decoration is border-only.

**Tests.** Boot `cyclopsd` in-process against the rig server (as existing
suites do): detected agent → badge appears; label set → naming priority
shifts; attention set → eye at all three levels, click jumps to the pane;
daemon stopped → offline state, decoration returns on reconnect. Frame
tests for badges, rollup, the panel, and offline states. Rollup
presence-aggregation unit tests.

**Integration.** First daemon consumption in the workspace; Step 6's
close-confirmation now uses real agent state; menus gain rename-identity.

**Demo.** Run a real agent in a pane: the badge walks through states, the
eye appears when input is needed, clicking it lands in the pane, and the
event panel shows the record — while `cyclops watch` in another terminal
shows the same stream.

**Acceptance criteria.**

- With `cyclopsd` booted in-process against the rig server: a detected
  agent produces a badge, a set label shifts the naming priority, and an
  attention fact shows the eye at pane, tab, and workspace levels.
- Every state cell carries glyph AND word with color as the redundant
  third encoding; new tokens live in `cyclops-theme` and the
  painted-by-a-renderer test covers them.
- The rollup is presence aggregation of the daemon's per-pane answer —
  no attention logic exists in the workspace crate.
- Clicking an indicator focuses the flagged pane (rig-backed).
- Sidebar naming follows label → detected name → `agent`; unnamed plain
  shells stay out of the expanded list; rename goes to daemon
  `pane.label` and no code path writes a tmux pane title.
- The event panel renders the same daemon subscription `cyclops watch`
  uses; toggling it never leaves the workspace.
- Stopping the daemon degrades to the quiet offline state and reconnect
  restores decoration (rig test), with the workspace still fully usable
  as a terminal throughout.

## Step 13: Persistence — preferences and last-active state

**Objective.** Durable UI state in the two places the design assigns: user
intent (`sidebar_visible`, `sidebar_width`, `workspace_order`) as
recognized `config.toml` keys, and volatile last-active workspace/tab
through new additive daemon wire methods. Implement the reopen fallback:
last-active → configured `default_workspace` → first workspace → offer to
create.

**Guidance.** Unknown fields ignored in both directions; an older daemon
returns nothing and the UI falls back cleanly (warn-never-reject). No
ledger writes — this state is deliberately not a coordination fact.
Workspace reorder (Step 7's sidebar) now persists via `workspace_order`.

**Tests.** Wire round-trip tests against in-process `cyclopsd`; version-skew
test (method absent → fallback chain); config read/write tests under
`CYCLOPS_HOME` at scratch; fallback-order unit tests; rig-backed reopen
test: close the workspace, reopen, land on the same workspace and tab.

**Integration.** Touches `cyclopsd` (new methods) and the workspace's
`persist` module; Step 7's sidebar state becomes durable.

**Demo.** Arrange the sidebar, focus a particular tab, quit, run `cyclops`
again: everything is where it was — then restart the daemon and see the
graceful fallback cost exactly one click.

**Acceptance criteria.**

- `sidebar_visible`, `sidebar_width`, and `workspace_order` round-trip
  through `config.toml` with `CYCLOPS_HOME` at scratch; unknown keys in
  the file never break the daemon or the UI.
- Last-active workspace/tab round-trips through the new additive wire
  methods against in-process `cyclopsd`.
- A version-skew test (method absent, as an older daemon would answer)
  falls through the chain cleanly — warn, never reject.
- The reopen fallback order (last-active → `default_workspace` → first
  workspace → offer to create) is unit-tested at each link.
- A rig-backed close-and-reopen lands on the same workspace and tab.
- No UI state of any kind is written to the ledger.

## Step 14: Command surface and docs — `watch` rename, parity, front doors

**Objective.** Finish the product surface: `cyclops watch` becomes the
stream TUI (today's `cyclops ui`), `cyclops watch --json` keeps the
existing machine-readable stream byte-for-byte, `--plain` remains,
`cyclops ui` aliases to `watch` with a deprecation note. Ship the docs: a
workspace page reachable from `README.md`/`docs/HANDOFF.md`, updated
QUICKSTART/ui docs, and the `AGENTS.md`/`docs/HANDOFF.md` amendment of the
"one rendering vocabulary" rule (grid stays the CLI/stream vocabulary; the
workspace renders through Ratatui; tokens remain the shared layer).

**Guidance.** Every command shape a doc quotes re-runs in
`demos/parity-check.sh`; quoted output is copied from the parity
transcript, never hand-written. New demo script(s) go through
`demos/lib.sh`. Behavior and docs ship in the same commits throughout —
this step carries the surface rename's docs and the accumulated
cross-cutting pages.

**Tests.** CLI surface tests: `cyclops watch --json` byte-identical to the
pre-rename stream (parity gate); `watch` TTY behavior; `ui` deprecation
path; bare `cyclops` non-TTY help (guarded since Step 4, re-verified
through parity). `scripts/check-doc-paths.py` and
`demos/parity-check.sh` pass with the new pages and shapes.

**Integration.** Final wiring of the CLI surface; closes R4 and the
documentation obligations from every prior step.

**Demo.** The complete product: `cyclops` opens the workspace,
`cyclops watch` opens the stream TUI, `cyclops watch --json` feeds a
script unchanged, and `demos/parity-check.sh` proves the docs tell the
truth.

**Acceptance criteria.**

- `cyclops watch` opens the stream TUI; `cyclops watch --json` output is
  byte-identical to the pre-rename stream, enforced as a parity gate;
  `--plain` behaves as before.
- `cyclops ui` aliases to `watch` and prints the deprecation note.
- Bare `cyclops` non-TTY help (from Step 4) is re-verified through the
  parity run.
- New doc pages are reachable from `README.md` or `docs/HANDOFF.md` and
  every quoted path exists — `scripts/check-doc-paths.py` passes.
- Every command shape quoted in updated docs re-runs in
  `demos/parity-check.sh`, with quoted output copied from the parity
  transcript; new demo scripts source `demos/lib.sh`.
- The "one rendering vocabulary" amendment lands in `AGENTS.md` and
  `docs/HANDOFF.md` in the same commit as the surface change.
